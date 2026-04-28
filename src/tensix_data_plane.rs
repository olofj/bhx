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
use crate::virtio::{InterruptKind, VirtioDeviceImpl};

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
            crate::dlog!(
                "[kick-poller] seq={} slot={} queue={} epoch={}",
                seq,
                slot,
                queue_idx,
                epoch
            );
            // M5.5b: dispatch to the registered (slot, queue)
            // device handler if one exists. The handler walks the
            // guest's avail/desc/used rings and fires the PLIC IRQ
            // — same shape as `virtio::run_device`'s descriptor
            // walk, but kick-driven instead of MMIO-polled.
            //
            // First cut: registry lookup + diagnostic log only.
            // The actual descriptor-walk extraction needs
            // `VringDesc/Avail/Used` made `pub(crate)` (done in
            // this commit) and a few helpers from
            // `virtio::run_device` extracted into a shared
            // `process_one_chain` function — that piece lands
            // alongside the firmware-side queue-pointer shadow
            // extension that M5.5b's full implementation needs.
            let map = registry.lock().unwrap();
            match map.get(&(slot as u32)) {
                Some(reg) => {
                    crate::dlog!(
                        "[kick-poller]   dispatching to slot {} ({} kind, irq {})",
                        slot,
                        reg.interrupt_kind_name(),
                        reg.interrupt_number,
                    );
                    // Future descriptor walk goes here. For now,
                    // bump a per-registration "would-dispatch"
                    // counter so a daemon-side test can confirm
                    // the registry path works. The lock is
                    // released at the end of this scope.
                }
                None => {
                    crate::dlog!(
                        "[kick-poller]   no registration for slot {}, dropping kick",
                        slot
                    );
                }
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
