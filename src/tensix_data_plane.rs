// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Daemon-side data-plane for the Tensix virtio engine (M5.5, #71).
//!
//! Runs a poll thread that consumes the kick ring (BRISC → daemon)
//! produced by `brisc-firmware/virtio.c`. Each kick is a `(slot,
//! queue_idx, seq, epoch)` tuple, signalling that the guest wrote
//! `QUEUE_NOTIFY` for that virtqueue.
//!
//! Lifecycle: spawned by `TensixEngine::bring_up` (#71 M5.5a). Lives
//! as long as the engine `Arc<TensixEngine>` does — when the daemon
//! shuts down and the last reference goes away, the poller's exit
//! flag is set and the thread joins.
//!
//! Per-(slot, queue) handler dispatch is the M5.5b piece — the first
//! cut here just logs every kick to the daemon log and bumps a
//! counter so we can verify the kick path end-to-end through the
//! daemon. Adding real handlers means: for each kick, look up the
//! registered `L2Cpu` + `VirtioDeviceImpl` for the slot, run the
//! existing descriptor walk over the guest's avail/desc/used rings,
//! and push a `CompletionEntry` back to BRISC.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::l2cpu::L2Cpu;
use crate::tensix_engine::TensixEngine;
use crate::virtio::interrupt::InterruptController;
use crate::virtio::{
    process_one_chain_for_queue, InterruptKind, VirtioDeviceImpl, VringAvail, VringDesc, VringUsed,
};
use crate::virtio_engine as ve;

/// One registered (slot, device) pair the kick poller dispatches to
/// when a kick arrives. The fields mirror what `virtio::run_device`
/// holds per device today: an `Arc<L2Cpu>` for guest DRAM access,
/// the device implementation for descriptor processing, and the
/// per-L2CPU interrupt controller + IRQ number for the
/// completion-side PLIC poke.
///
/// `processed` tracks the queue's avail-ring head we've consumed up
/// to, mirroring `run_device`'s `processed[qi]` vector. Indexed by
/// queue_idx; all-zeros at registration time and persists across
/// kicks until the slot is unregistered.
pub struct RegEntry {
    pub slot: u32,
    pub l2cpu: Arc<L2Cpu>,
    pub device: Box<dyn VirtioDeviceImpl + Send>,
    pub interrupt_ctl: Arc<InterruptController>,
    pub interrupt_number: u32,
    pub interrupt_kind: InterruptKind,
    /// Per-queue avail-ring head we've consumed. Lazily resized
    /// when a kick references a queue beyond the current length.
    pub processed: Vec<u16>,
}

impl RegEntry {
    pub fn new(
        slot: u32,
        l2cpu: Arc<L2Cpu>,
        device: Box<dyn VirtioDeviceImpl + Send>,
        interrupt_ctl: Arc<InterruptController>,
        interrupt_number: u32,
        interrupt_kind: InterruptKind,
    ) -> Self {
        let num_queues = device.num_queues() as usize;
        RegEntry {
            slot,
            l2cpu,
            device,
            interrupt_ctl,
            interrupt_number,
            interrupt_kind,
            processed: vec![0u16; num_queues],
        }
    }

    pub fn interrupt_kind_name(&self) -> &'static str {
        match self.interrupt_kind {
            InterruptKind::Block => "block",
            InterruptKind::Net => "net",
            InterruptKind::Console => "console",
            InterruptKind::Rng => "rng",
        }
    }
}

/// Stats the poller updates so `daemon status` (or a future
/// metrics endpoint) can surface progress without a separate
/// channel back from the worker.
#[derive(Default)]
pub struct PollerStats {
    /// Cumulative number of `KickEntry` records consumed.
    pub kicks_consumed: AtomicU64,
    /// Cumulative number of full poll iterations (with or without
    /// kicks). Heartbeat for detecting a stalled poller.
    pub poll_iterations: AtomicU64,
    /// Last (slot, queue_idx) pair we processed, packed as
    /// `(slot << 16) | queue_idx` — same shape as the firmware's
    /// `STATS_OFF_LAST_NOTIFY`.
    pub last_kick_slot_queue: AtomicU64,
}

/// Slot → registration map. Wrapped in `Arc<Mutex<...>>` so the
/// poller thread can look up registrations on each kick while
/// `register_slot` / `unregister_slot` mutate from the boot path.
/// Per-slot keys are the firmware's `slot = l2cpu_idx*4 +
/// device_idx` packing (0..16).
pub type Registry = Arc<Mutex<HashMap<u32, RegEntry>>>;

/// Daemon-side kick consumer. Owns a thread that loops on
/// `engine.kick_producer_seq()` and consumes new entries.
pub struct KickPoller {
    pub stats: Arc<PollerStats>,
    pub registry: Registry,
    exit: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl KickPoller {
    /// Spawn the poll thread. Returns immediately; the thread runs
    /// until [`KickPoller::shutdown`] is called or the
    /// `KickPoller` is dropped.
    pub fn spawn(engine: Arc<TensixEngine>) -> Self {
        let stats = Arc::new(PollerStats::default());
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        let exit = Arc::new(AtomicBool::new(false));
        let stats_thread = Arc::clone(&stats);
        let registry_thread = Arc::clone(&registry);
        let exit_thread = Arc::clone(&exit);
        let join = thread::Builder::new()
            .name("tensix-kick-poller".to_string())
            .spawn(move || run_poll_loop(engine, stats_thread, registry_thread, exit_thread))
            .expect("spawn tensix-kick-poller");
        KickPoller {
            stats,
            registry,
            exit,
            join: Some(join),
        }
    }

    /// Register a (slot, l2cpu, device, interrupt) tuple. Future
    /// kicks for `slot` will dispatch to `entry.device`'s
    /// VirtioDeviceImpl methods. dispatch_boot calls this under the
    /// `virtio-engine` feature flag, once per enabled device.
    pub fn register_slot(&self, entry: RegEntry) {
        let slot = entry.slot;
        let mut map = self.registry.lock().unwrap();
        map.insert(slot, entry);
    }

    /// Unregister a slot — called when an L2CPU is being torn down
    /// (slot.shutdown via daemon stop or boot --force). Future
    /// kicks for `slot` log a "no registration" warning and bump
    /// stats; they don't touch the device or fire IRQs.
    pub fn unregister_slot(&self, slot: u32) {
        let mut map = self.registry.lock().unwrap();
        map.remove(&slot);
    }

    /// Signal the thread to exit and join it. Idempotent.
    pub fn shutdown(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            // Best-effort; if the thread panicked we don't have a
            // useful recovery path here.
            let _ = j.join();
        }
    }
}

impl Drop for KickPoller {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_poll_loop(
    engine: Arc<TensixEngine>,
    stats: Arc<PollerStats>,
    registry: Registry,
    exit: Arc<AtomicBool>,
) {
    // Three-tier adaptive sleep mirroring `virtio::run_device`'s
    // pattern: tight FAST while traffic is flowing, SLOW when idle
    // briefly, IDLE during long quiet stretches. Saves CPU when the
    // guest is doing nothing without sacrificing notify latency
    // when it picks up.
    const FAST_SLEEP: Duration = Duration::from_micros(50);
    const SLOW_SLEEP: Duration = Duration::from_millis(1);
    const IDLE_SLEEP: Duration = Duration::from_millis(10);
    const FAST_WINDOW: Duration = Duration::from_millis(200);
    const IDLE_WINDOW: Duration = Duration::from_secs(2);

    let mut consumer: u32 = engine.kick_ring_header().1;
    let mut last_active = std::time::Instant::now();

    while !exit.load(Ordering::Relaxed) {
        let producer = engine.kick_producer_seq();
        let mut consumed_this_pass = 0u64;
        while consumer != producer {
            let raw = engine.read_kick_entry(consumer);
            // raw[0] = (queue_idx << 16) | slot ; matches the
            // firmware's `kick_ring_push` packing.
            let slot = (raw[0] & 0xFFFF) as u16;
            let queue_idx = (raw[0] >> 16) as u16;
            let seq = raw[1];
            let epoch = raw[2];
            let _ = (seq, epoch); // currently unused; preserved in case we add per-kick metrics
                                  // M5.5b: dispatch to the registered (slot, queue)
                                  // device handler. Reads the per-queue desc/avail/used
                                  // pointers from BRISC L1 shadow (firmware shadows guest
                                  // writes via the per-queue snapshot extension), walks
                                  // the chain over guest DRAM via the L2CPU's memory
                                  // mapping, calls the device's `process_queue_*` hooks,
                                  // writes used-ring entries, fires the PLIC IRQ.
            let mut map = registry.lock().unwrap();
            if let Some(reg) = map.get_mut(&(slot as u32)) {
                let posted = dispatch_chain(&engine, reg, queue_idx);
                if posted {
                    // Push a completion to BRISC for diagnostics +
                    // future BRISC-side IRQ dispatch. The PLIC IRQ
                    // itself is fired daemon-side today.
                    let used_idx = read_used_idx(reg, queue_idx);
                    engine.push_completion(slot, queue_idx, used_idx);
                }
            } else {
                crate::dlog!(
                    "[kick-poller]   no registration for slot {}, dropping kick",
                    slot
                );
            }
            drop(map);
            stats.last_kick_slot_queue.store(
                ((slot as u64) << 16) | (queue_idx as u64),
                Ordering::Relaxed,
            );
            stats.kicks_consumed.fetch_add(1, Ordering::Relaxed);
            consumer = consumer.wrapping_add(1);
            consumed_this_pass += 1;
        }
        if consumed_this_pass > 0 {
            engine.set_kick_consumer_seq(consumer);
            last_active = std::time::Instant::now();
        }

        // Net RX path: nothing on the BRISC kick ring fires when a
        // libslirp-delivered packet arrives, because the guest never
        // notifies us for RX (it just refills the ring). We poll
        // every registered net device's RX queue here — `queue_has_data`
        // returns true only when slirp has bytes to read, so the
        // dispatch is a no-op on idle queues. Same FAST/SLOW/IDLE
        // adaptive cadence as the kick path.
        let mut net_drained = false;
        {
            let mut map = registry.lock().unwrap();
            for (slot, reg) in map.iter_mut() {
                if !matches!(reg.interrupt_kind, InterruptKind::Net) {
                    continue;
                }
                let posted = dispatch_chain(&engine, reg, 0);
                if posted {
                    let used_idx = read_used_idx(reg, 0);
                    engine.push_completion(*slot as u16, 0, used_idx);
                    net_drained = true;
                }
            }
        }
        if net_drained {
            last_active = std::time::Instant::now();
        }

        stats.poll_iterations.fetch_add(1, Ordering::Relaxed);

        let idle = last_active.elapsed();
        let sleep = if idle < FAST_WINDOW {
            FAST_SLEEP
        } else if idle < IDLE_WINDOW {
            SLOW_SLEEP
        } else {
            IDLE_SLEEP
        };
        thread::sleep(sleep);
    }
}

/// Walk one descriptor chain on `(slot, queue_idx)` via the
/// existing virtio descriptor processor. Reads per-queue
/// desc/avail/used pointers from BRISC L1 shadow, maps them into
/// the L2CPU's memory namespace, calls the device's
/// `process_queue_*` hooks, writes the used-ring entry, fires the
/// PLIC IRQ on success. Returns `true` if a chain was posted.
fn dispatch_chain(engine: &Arc<TensixEngine>, reg: &mut RegEntry, queue_idx: u16) -> bool {
    // Lazily extend `processed` if a kick references a queue index
    // beyond what the device announced at registration. Out of
    // bounds shouldn't happen for a well-behaved guest, but a
    // misbehaving guest shouldn't crash the daemon.
    if (queue_idx as usize) >= reg.processed.len() {
        crate::dlog!(
            "[kick-poller]   slot {} queue {} out of range (have {}), dropping",
            reg.slot,
            queue_idx,
            reg.processed.len()
        );
        return false;
    }

    // Read the four per-queue pointers from BRISC L1 shadow. The
    // firmware mirrors guest writes to QUEUE_DESC_LOW/HIGH /
    // QUEUE_DRIVER_LOW/HIGH / QUEUE_DEVICE_LOW/HIGH into here on
    // each poll iteration, indexed by (slot, current_sel). See
    // `brisc-firmware/virtio.c::poll_one_device`'s shadow capture.
    let qi = queue_idx as u32;
    let desc_lo = engine.read_l1_u32(ve::shadow_queue_addr(
        reg.slot,
        qi,
        ve::SHADOW_Q_OFF_DESC_LO,
    ));
    let desc_hi = engine.read_l1_u32(ve::shadow_queue_addr(
        reg.slot,
        qi,
        ve::SHADOW_Q_OFF_DESC_HI,
    ));
    let avail_lo = engine.read_l1_u32(ve::shadow_queue_addr(
        reg.slot,
        qi,
        ve::SHADOW_Q_OFF_DRIVER_LO,
    ));
    let avail_hi = engine.read_l1_u32(ve::shadow_queue_addr(
        reg.slot,
        qi,
        ve::SHADOW_Q_OFF_DRIVER_HI,
    ));
    let used_lo = engine.read_l1_u32(ve::shadow_queue_addr(
        reg.slot,
        qi,
        ve::SHADOW_Q_OFF_DEVICE_LO,
    ));
    let used_hi = engine.read_l1_u32(ve::shadow_queue_addr(
        reg.slot,
        qi,
        ve::SHADOW_Q_OFF_DEVICE_HI,
    ));
    // BRISC firmware mirrors the kernel's QUEUE_NUM write into the
    // per-queue shadow on each poll iteration (see
    // brisc-firmware/virtio.c::poll_one_device's FIELDS table). Use
    // this for ring wrapping in process_one_chain_for_queue — the
    // kernel allocates rings sized exactly `queue_num`, so any other
    // wrap value reads / writes past the kernel's allocation.
    let queue_num = engine.read_l1_u32(ve::shadow_queue_addr(reg.slot, qi, ve::SHADOW_Q_OFF_NUM));

    let desc_addr = ((desc_hi as u64) << 32) | desc_lo as u64;
    let avail_addr = ((avail_hi as u64) << 32) | avail_lo as u64;
    let used_addr = ((used_hi as u64) << 32) | used_lo as u64;

    if desc_addr == 0 || avail_addr == 0 || used_addr == 0 {
        // Queue not yet configured — guest hasn't published the
        // shadow pointers yet. Drop the kick silently; otherwise the
        // RX-side polling loop spams this for every idle iteration
        // while net's queue 0 is unconfigured.
        return false;
    }
    if queue_num == 0 || queue_num > u16::MAX as u32 {
        // Kernel hasn't published QUEUE_NUM yet, or it published
        // something larger than u16. Either way we can't index the
        // ring; drop the kick.
        crate::dlog!(
            "[kick-poller]   slot {} queue {} bad queue_num={}, dropping",
            reg.slot,
            queue_idx,
            queue_num
        );
        return false;
    }
    let queue_num = queue_num as u16;

    // Convert guest physical addresses to host pointers via the
    // L2CPU's memory mmap. Same arithmetic as
    // `virtio::run_device`.
    let starting = reg.l2cpu.starting_address();
    let mem_end = starting + reg.l2cpu.memory_size();
    let memory = reg.l2cpu.get_memory_ptr();
    let in_range =
        |addr: u64, size: u64| -> bool { addr >= starting && addr.saturating_add(size) <= mem_end };
    if !in_range(desc_addr, 16) || !in_range(avail_addr, 4) || !in_range(used_addr, 4) {
        crate::dlog!(
            "[kick-poller]   slot {} queue {} pointers out of L2CPU memory range \
             (desc={:#x} avail={:#x} used={:#x}, range=[{:#x},{:#x})), dropping",
            reg.slot,
            queue_idx,
            desc_addr,
            avail_addr,
            used_addr,
            starting,
            mem_end,
        );
        return false;
    }
    let desc_q = unsafe { memory.add((desc_addr - starting) as usize) as *mut VringDesc };
    let avail_q = unsafe { memory.add((avail_addr - starting) as usize) as *mut VringAvail };
    let used_q = unsafe { memory.add((used_addr - starting) as usize) as *mut VringUsed };

    let queue_header_size = reg.device.queue_header_size();
    // Drain the avail ring fully — the kernel batches multiple chains
    // behind a single QUEUE_NOTIFY (each kick covers everything the
    // driver has queued so far). With one chain processed per kick,
    // we'd leak (avail.idx - processed) chains and stall on the next
    // batch. One IRQ per drain at the end is enough; the kernel's
    // virtblk_done loops over completions anyway.
    let mut posted = false;
    loop {
        let one = process_one_chain_for_queue(
            desc_q,
            avail_q,
            used_q,
            &mut reg.processed[queue_idx as usize],
            reg.device.as_mut(),
            queue_idx as u32,
            queue_header_size,
            queue_num,
            starting,
            mem_end,
            memory,
        );
        if !one {
            break;
        }
        posted = true;
    }

    if posted {
        // Fire the PLIC IRQ via the existing per-L2CPU
        // InterruptController. This is functionally identical to
        // what `virtio::run_device` does; we just trigger it
        // here instead of from a per-device MMIO-poll worker.
        // (Reading `interrupt_status` from the visible reg file
        // matches what run_device does for the legacy path; for
        // the engine path the address is on the Tensix L1 reg
        // file at the slot's MMIO_INTERRUPT_STATUS offset.)
        let interrupt_status_addr_l1 = ve::slot_regs_base(reg.slot) + ve::MMIO_INTERRUPT_STATUS;
        let interrupt_status_ptr = engine.l1_ptr(interrupt_status_addr_l1) as *mut u32;
        reg.interrupt_ctl
            .set_interrupt(interrupt_status_ptr, reg.interrupt_number);
        crate::virtio::bump_interrupt_metric(reg.interrupt_kind, reg.l2cpu.idx() as u8);
    }
    posted
}

/// Read the current `used.idx` for a queue — used so we can record
/// the latest used-ring head in the completion entry pushed back to
/// BRISC.
fn read_used_idx(reg: &RegEntry, queue_idx: u16) -> u32 {
    let qi = queue_idx as u32;
    let used_lo = 0u32; // no-op read; the actual used.idx lives in
                        // guest DRAM and we already advanced it inside
                        // `process_one_chain_for_queue`. The
                        // CompletionEntry's used_idx field is
                        // diagnostic for now, so we just echo the
                        // queue's ring slot count modulo `processed`.
    let _ = qi;
    let _ = used_lo;
    reg.processed[queue_idx as usize] as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poller_stats_default_is_zero() {
        let s = PollerStats::default();
        assert_eq!(s.kicks_consumed.load(Ordering::Relaxed), 0);
        assert_eq!(s.poll_iterations.load(Ordering::Relaxed), 0);
        assert_eq!(s.last_kick_slot_queue.load(Ordering::Relaxed), 0);
    }
}
