// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Daemon-side data-plane for the Tensix virtio engine.
//!
//! Polls the V2 dirty bitmap (#187) BRISC writes on every guest
//! `QUEUE_NOTIFY`: one byte per (slot, queue) at `CTRL_OFF_DIRTY`.
//! Each pass clears every set bit and dispatches the corresponding
//! virtqueue. The post-dispatch `used.idx` is published into the
//! V2 processed-cursor table at `CTRL_OFF_PROCESSED` so warm-resume
//! reads cursors directly without re-probing guest DRAM.
//!
//! Lifecycle: spawned by `TensixEngine::bring_up`. Lives as long as
//! the engine `Arc<TensixEngine>` does — when the daemon shuts down
//! and the last reference goes away, the poller's exit flag is set
//! and the thread joins.
//!
//! V1 (the kick ring + completion ring + throttle state machine) is
//! gone as of #189: a level-sensitive bitmap can't overflow under
//! any guest burst, so the rescue/throttle paths that grew out of
//! #184 have no V2 analogue.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::daemon::console_hub::ConsoleHub;
use crate::l2cpu::L2Cpu;
use crate::tensix_engine::TensixEngine;
use crate::uart_engine as uart;
use crate::virtio::interrupt::InterruptController;
use crate::virtio::{
    process_one_chain_for_queue, InterruptKind, VirtioDeviceImpl, VringAvail, VringDesc, VringUsed,
};
use crate::virtio_engine as ve;

/// Wraparound recovery for an SPSC ring: if the producer has run
/// `> ring_entries` ahead of the consumer, the daemon's view of
/// "old" entries has been overwritten in their ring slots already.
/// Fast-forward the consumer to the start of the still-readable
/// window so we drop the unreachable entries instead of replaying
/// stale ring contents. See #101.
fn clamp_consumer_to_ring(producer: u32, consumer: u32, ring_entries: u32) -> u32 {
    if producer.wrapping_sub(consumer) > ring_entries {
        producer.wrapping_sub(ring_entries)
    } else {
        consumer
    }
}

/// Snapshot the ratchet-style counter pattern the dispatcher uses
/// for chip-side stats (sel/ready races, queue setup counts,
/// notify events, etc): "if the value changed, log/account the
/// wrapping delta and remember the new value." Returns `Some(delta)`
/// on change, `None` otherwise. Critically uses `wrapping_sub` —
/// the counters are u32 monotonics on the chip side that can wrap
/// across long-running sessions, and a saturating subtract here
/// would silently lose deltas.
pub(crate) fn take_delta(current: u32, last: &mut u32) -> Option<u32> {
    if current == *last {
        return None;
    }
    let delta = current.wrapping_sub(*last);
    *last = current;
    Some(delta)
}

/// One registered (slot, device) pair the dispatcher routes to
/// when a guest QUEUE_NOTIFY sets the slot's dirty byte. The
/// fields mirror what the legacy `virtio::run_device` worker held
/// per device: an `Arc<L2Cpu>` for guest DRAM access, the device
/// implementation for descriptor processing, and the per-L2CPU
/// interrupt controller + IRQ number for the completion-side PLIC
/// poke.
///
/// `processed` tracks the queue's avail-ring head we've consumed up
/// to, mirroring `run_device`'s `processed[qi]` vector. All-zeros
/// at registration time and persists across NOTIFY events until the
/// slot is unregistered.
pub struct RegEntry {
    pub slot: u32,
    pub l2cpu: Arc<L2Cpu>,
    pub device: Box<dyn VirtioDeviceImpl + Send>,
    pub interrupt_ctl: Arc<InterruptController>,
    pub interrupt_number: u32,
    pub interrupt_kind: InterruptKind,
    /// Per-queue avail-ring head we've consumed.
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

    /// Diagnostic helper used in dlog output during debugging. Used
    /// by the dispatcher's per-slot probe-status logging (#123) so
    /// operators can grep daemon logs for "[probe-status] slot N
    /// (virtio_net) reached STATUS_DRIVER_OK".
    pub fn interrupt_kind_name(&self) -> &'static str {
        match self.interrupt_kind {
            InterruptKind::Block => "block",
            InterruptKind::Net => "net",
            InterruptKind::Console => "console",
            InterruptKind::Rng => "rng",
        }
    }
}

/// Stats the dispatcher updates so `daemon status` (or a future
/// metrics endpoint) can surface progress without a separate
/// channel back from the worker.
#[derive(Default)]
pub struct PollerStats {
    /// Cumulative number of (slot, queue) dispatches the bitmap
    /// drain has processed. One increment per non-empty
    /// `dispatch_chain` invocation.
    pub dispatches_total: AtomicU64,
    /// Cumulative number of full poll iterations. Heartbeat for
    /// detecting a stalled dispatcher.
    pub poll_iterations: AtomicU64,
    /// Last (slot, queue_idx) pair we processed, packed as
    /// `(slot << 16) | queue_idx` — same shape as the firmware's
    /// `STATS_OFF_LAST_NOTIFY`.
    pub last_dispatch_slot_queue: AtomicU64,
}

/// Slot → registration map. Wrapped in `Arc<Mutex<...>>` so the
/// dispatcher thread can look up registrations on each NOTIFY while
/// `register_slot` / `unregister_slot` mutate from the boot path.
/// Per-slot keys are the firmware's `slot = l2cpu_idx*8 +
/// device_idx` packing (0..32).
//
// FIXME(perf): `drain_dirty_bitmap` holds this lock for the entire
// drain pass — including every `dispatch_chain` it kicks off, which
// can walk an arbitrarily-deep avail ring. While the lock is held,
// `register_slot` / `unregister_slot` (= add-disk / remove-disk
// RPCs) block. V1 had the same shape so this isn't a regression,
// and at current workloads the latency is fine (~215 ms remove-disk
// observed under 100-iter fio soak). If add/remove SLAs need
// tightening, replace with a per-slot RwLock or a snapshotted
// view that the drain can iterate lock-free. Not blocking V2.2.
pub type Registry = Arc<Mutex<HashMap<u32, RegEntry>>>;

/// Per-L2CPU UART (#78) registry. Maps `l2cpu_idx` → the slot's
/// `console_hub`. The dispatcher's UART feed-ring drain pushes the
/// TRISC0-produced bytes through `push_chip_output` on the
/// appropriate hub. Separate from the virtio `Registry` so
/// register/unregister is independent — `register_uart` flips the
/// L2CPU's UART bit in the active-slots bitmap, telling BRISC to
/// release TRISC0 from soft reset so it starts sweeping that
/// L2CPU's UART reg file.
pub type UartRegistry = Arc<Mutex<HashMap<u8, Arc<ConsoleHub>>>>;

/// Daemon-side V2 dispatcher. Owns a thread that polls the
/// per-(slot, queue) dirty bitmap in BRISC L1 and dispatches each
/// set bit through the registered `RegEntry::device`. Also drains
/// the async RX paths (net, console) and the TRISC0 UART feed
/// rings on the same loop.
pub struct Dispatcher {
    pub stats: Arc<PollerStats>,
    pub registry: Registry,
    pub uart_registry: UartRegistry,
    /// Cloned for register/unregister to push the active-slots
    /// bitmap into BRISC L1 — BRISC uses it to skip non-active
    /// slots in its sweep. Without this, BRISC polls all 32 slots
    /// and the per-slot revisit period stretches enough to lose the
    /// SEL→READY race against stock kernels.
    engine: Arc<TensixEngine>,
    exit: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl Dispatcher {
    /// Spawn the poll thread. Returns immediately; the thread runs
    /// until [`Dispatcher::shutdown`] is called or the
    /// `Dispatcher` is dropped.
    pub fn spawn(engine: Arc<TensixEngine>) -> Self {
        let stats = Arc::new(PollerStats::default());
        let registry: Registry = Arc::new(Mutex::new(HashMap::new()));
        let uart_registry: UartRegistry = Arc::new(Mutex::new(HashMap::new()));
        let exit = Arc::new(AtomicBool::new(false));
        let stats_thread = Arc::clone(&stats);
        let registry_thread = Arc::clone(&registry);
        let uart_registry_thread = Arc::clone(&uart_registry);
        let exit_thread = Arc::clone(&exit);
        let engine_thread = Arc::clone(&engine);
        let join = thread::Builder::new()
            .name("tensix-dispatcher".to_string())
            .spawn(move || {
                run_poll_loop(
                    engine_thread,
                    stats_thread,
                    registry_thread,
                    uart_registry_thread,
                    exit_thread,
                )
            })
            .expect("spawn tensix-dispatcher");
        Dispatcher {
            stats,
            registry,
            uart_registry,
            engine,
            exit,
            join: Some(join),
        }
    }

    /// Recompute the active-slots bitmap from the virtio + UART
    /// registries and write it into BRISC L1 at CTRL_OFF_ACTIVE_SLOTS.
    /// BRISC reads this on every sweep iteration; bit `i` set means
    /// "poll slot `i`." Both virtio dev_idx 0..5 and UART (dev_idx 6)
    /// share each L2CPU's 8-slot region — see #175 and
    /// `uart::UART_SLOT_OFFSET_IN_L2CPU` for the layout.
    fn publish_active_mask(&self) {
        let mut virtio_mask: u32 = 0;
        for &slot in self.registry.lock().unwrap().keys() {
            if slot < 32 {
                virtio_mask |= 1u32 << slot;
            }
        }
        let mut mask: u32 = virtio_mask;
        for &l2cpu_idx in self.uart_registry.lock().unwrap().keys() {
            let slot = uart::slot_for_l2cpu(l2cpu_idx) as u32;
            if slot < 32 {
                mask |= 1u32 << slot;
            }
        }
        self.engine.write_l1_u32(
            crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_ACTIVE_SLOTS,
            mask,
        );
        // Virtio-only mask: TRISC1's race-watch loop reads this so it
        // doesn't skip L2CPU 2/3's actual virtio devices (which live
        // at slot indices that overlap the UART/shutdown range).
        self.engine.write_l1_u32(
            crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_ACTIVE_VIRTIO_SLOTS,
            virtio_mask,
        );
    }

    /// Register a (slot, l2cpu, device, interrupt) tuple. Future
    /// guest QUEUE_NOTIFYs for `slot` will dispatch to
    /// `entry.device`'s `VirtioDeviceImpl` methods. `dispatch_boot`
    /// calls this once per enabled device.
    pub fn register_slot(&self, entry: RegEntry) {
        let slot = entry.slot;
        self.registry.lock().unwrap().insert(slot, entry);
        self.publish_active_mask();
    }

    /// Unregister a slot — called when an L2CPU is being torn down
    /// (slot.shutdown via daemon stop or boot --force). Future
    /// dirty bits for `slot` are cleared without dispatch (the slot
    /// isn't in the registry, so `drain_dirty_bitmap` skips it
    /// before checking its byte).
    pub fn unregister_slot(&self, slot: u32) {
        self.registry.lock().unwrap().remove(&slot);
        self.publish_active_mask();
    }

    /// Register an L2CPU's 16550 UART. The dispatcher's per-iter
    /// UART drain reads bytes from TRISC0's feed ring at
    /// `uart::uart_private_base(idx)` and pushes them through the
    /// registered `console_hub` via `push_chip_output`. Sets the
    /// corresponding bit in the active-slots bitmap so BRISC
    /// releases TRISC0 from soft reset.
    pub fn register_uart(&self, l2cpu_idx: u8, hub: Arc<ConsoleHub>) {
        self.uart_registry.lock().unwrap().insert(l2cpu_idx, hub);
        self.publish_active_mask();
    }

    /// Unregister an L2CPU's UART. Clears the L2CPU's UART bit in
    /// the active-slots bitmap so BRISC re-asserts TRISC0's soft
    /// reset (last UART out turns the lights off).
    pub fn unregister_uart(&self, l2cpu_idx: u8) {
        self.uart_registry.lock().unwrap().remove(&l2cpu_idx);
        self.publish_active_mask();
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

impl Drop for Dispatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_poll_loop(
    engine: Arc<TensixEngine>,
    stats: Arc<PollerStats>,
    registry: Registry,
    uart_registry: UartRegistry,
    exit: Arc<AtomicBool>,
) {
    // Three-tier adaptive sleep mirroring `virtio::run_device`'s
    // pattern: tight FAST while traffic is flowing, SLOW when idle
    // briefly, IDLE during long quiet stretches. Per-poll cost in
    // V2 is a handful of L1 reads (one byte per active (slot,
    // queue) pair for the dirty bitmap, plus the RX/UART/stat
    // sweeps), so the FAST tier can stay tight.
    const FAST_SLEEP: Duration = Duration::from_micros(10);
    const SLOW_SLEEP: Duration = Duration::from_millis(1);
    const IDLE_SLEEP: Duration = Duration::from_millis(10);
    const FAST_WINDOW: Duration = Duration::from_millis(200);
    const IDLE_WINDOW: Duration = Duration::from_secs(2);

    let mut last_active = std::time::Instant::now();

    // Per-L2CPU UART feed-ring consumer state. The daemon polls
    // these rings directly via the chip TLB. Each ring slot is
    // 4 bytes (one byte in low 8 bits) and there are 1024 slots,
    // so a stock-distro boot's ~10 KB of TX fits comfortably
    // without any rate limiting.
    let mut uart_consumer: [u32; crate::virtio_engine::NUM_L2CPUS as usize] =
        [0; crate::virtio_engine::NUM_L2CPUS as usize];

    // Ratchet-style snapshots of BRISC-side counters. Each delta is
    // surfaced via dlog + the matching Prometheus counter so an
    // operator polling `/metrics` sees the actual count rather than
    // having to wait for restart-then-zero rollover.
    let mut last_sel_ready_races: u32 = 0;
    let mut last_ready_capture_sel_races: u32 = 0;
    let mut last_queue_setups: u32 = 0;
    let mut last_queue_teardowns: u32 = 0;
    let mut last_brisc_old_sel_rescue: u32 = 0;
    let mut last_max_sweep_cycles: u32 = 0;
    let mut last_max_steady_sweep_cycles: u32 = 0;
    let mut last_max_sel_path_cycles: u32 = 0;
    let mut last_uart_drops: [u32; crate::virtio_engine::NUM_L2CPUS as usize] =
        [0; crate::virtio_engine::NUM_L2CPUS as usize];
    let mut last_notify_events: u32 = 0;
    // Track per-slot STATUS transitions. Bench harnesses (and
    // operators debugging probe failures) need a definitive
    // "kernel finished probing this device" signal.
    //
    // We can't just log the first DRIVER_OK per slot: U-Boot's own
    // virtio_blk driver probes and reaches DRIVER_OK before the
    // kernel even loads, then the kernel writes STATUS=0 (reset)
    // and runs its own probe. If that re-probe fails, the snap
    // would still be at U-Boot's DRIVER_OK and a "first DRIVER_OK"
    // log would mask the kernel-side failure.
    //
    // STATUS=0 transitions also reset the slot's `processed[]`
    // cursors so a re-probe reads avail.ring[0] for the fresh
    // session instead of avail.ring[stale_idx]. (V1 carried the
    // per-slot epoch in every kick entry to do the same; V2 reads
    // STATUS directly.)
    let mut last_status: HashMap<u32, u32> = HashMap::new();

    while !exit.load(Ordering::Relaxed) {
        // V2 dispatch (#189): drain the dirty bitmap. One single-byte
        // read+clear per (slot, queue), then dispatch. Cannot
        // overflow under any guest burst; replaces V1's kick ring,
        // throttle, and rescue paths.
        let dispatched = drain_dirty_bitmap(&engine, &registry, &stats);
        if dispatched > 0 {
            crate::daemon::metrics::DISPATCH_PASSES_TOTAL.add(1);
            crate::daemon::metrics::DISPATCH_QUEUES_DRAINED.add(dispatched);
            last_active = std::time::Instant::now();
        }

        // Async RX path: net (libslirp delivers packets) and console
        // (operator keystrokes via input_buf) don't get a guest
        // QUEUE_NOTIFY when their backing source has data. Poll
        // their RX queues here — `queue_has_data` (per-device,
        // called inside `process_one_chain_for_queue`) returns true
        // only when the backing source has bytes, so the dispatch
        // is a no-op on idle queues.
        let mut rx_drained = false;
        {
            let mut map = registry.lock().unwrap();
            for reg in map.values_mut() {
                if !matches!(
                    reg.interrupt_kind,
                    InterruptKind::Net | InterruptKind::Console
                ) {
                    continue;
                }
                if dispatch_chain(&engine, reg, 0).is_some() {
                    rx_drained = true;
                }
            }
        }
        if rx_drained {
            last_active = std::time::Instant::now();
        }

        // M6.1 #79 v3 (Phase B revised): drain each registered UART's
        // feed ring directly. TRISC0 produces; we consume. One ring
        // per L2CPU, 1024 slots, 4 bytes per slot (byte in low 8 bits).
        // We hold the registry lock for the whole drain to keep the
        // ConsoleHub Arc alive without bumping refcounts every byte.
        {
            let map = uart_registry.lock().unwrap();
            for (&l2cpu_idx, hub) in map.iter() {
                if (l2cpu_idx as u32) >= crate::virtio_engine::NUM_L2CPUS {
                    continue;
                }
                let priv_base = uart::uart_private_base(l2cpu_idx);
                let producer =
                    engine.read_l1_u32(priv_base + uart::UART_PRIV_OFF_FEED_PRODUCER_SEQ);
                let mut local_consumer = uart_consumer[l2cpu_idx as usize];
                if producer == local_consumer {
                    continue;
                }
                // Wraparound recovery (#101): if TRISC0's producer ran
                // away faster than we drained, the oldest unread bytes
                // have already been overwritten in their ring slots.
                // Fast-forward the consumer to the start of the still-
                // readable window so we don't replay garbage.
                let clamped =
                    clamp_consumer_to_ring(producer, local_consumer, uart::UART_FEED_RING_ENTRIES);
                if clamped != local_consumer {
                    crate::dlog!(
                        "[dispatcher] uart l2cpu {} producer {} consumer {} > ring ({}); fast-forwarding consumer to {}",
                        l2cpu_idx,
                        producer,
                        local_consumer,
                        uart::UART_FEED_RING_ENTRIES,
                        clamped
                    );
                    local_consumer = clamped;
                }
                // Read all available bytes into a stack buffer, then
                // push to the hub once per drain pass — keeps the per-
                // byte overhead (lock + memcpy + send) amortized.
                let mask = uart::UART_FEED_RING_ENTRIES - 1;
                let mut buf = [0u8; 256];
                while local_consumer != producer {
                    let take =
                        std::cmp::min(producer.wrapping_sub(local_consumer) as usize, buf.len());
                    for (i, slot) in buf.iter_mut().take(take).enumerate() {
                        let idx = local_consumer.wrapping_add(i as u32) & mask;
                        let cell =
                            engine.read_l1_u32(priv_base + uart::UART_PRIV_OFF_FEED_RING + idx * 4);
                        *slot = (cell & 0xFF) as u8;
                    }
                    hub.push_chip_output(&buf[..take]);
                    local_consumer = local_consumer.wrapping_add(take as u32);
                }
                engine.write_l1_u32(
                    priv_base + uart::UART_PRIV_OFF_FEED_CONSUMER_SEQ,
                    local_consumer,
                );
                uart_consumer[l2cpu_idx as usize] = local_consumer;
                last_active = std::time::Instant::now();
            }
        }

        stats.poll_iterations.fetch_add(1, Ordering::Relaxed);

        // Surface BRISC's NOTIFY counter as a Prometheus counter.
        // Useful for the burst regression test (#186) which asserts
        // `bhx_notify_events_total > 0` to confirm the workload
        // actually hit the dispatch path.
        let notify_events = engine.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_NOTIFY_EVENTS);
        if let Some(delta) = take_delta(notify_events, &mut last_notify_events) {
            crate::daemon::metrics::NOTIFY_EVENTS_TOTAL.add(delta as u64);
        }

        let sel_ready_races = engine.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_SEL_READY_RACES);
        if let Some(delta) = take_delta(sel_ready_races, &mut last_sel_ready_races) {
            // SEL→READY race observation. Per-iteration dlog used to
            // fire here on every counter advance; under multi-guest
            // stress that buried unrelated lines, exactly the failure
            // mode `feedback_overflow_counters_loud.md` warns about
            // (#172). Surface only via the Prometheus counter; an
            // operator who needs per-event detail can `daemon logs`
            // the historical context plus the metric delta.
            crate::daemon::metrics::SEL_READY_RACES_TOTAL.add(delta as u64);
        }
        // #124 timing probe. Log on ratchet-up only (each new max
        // since last log). 1.35 GHz BRISC ≈ 0.74 ns/cycle, so cycle
        // counts ÷ 1.35 ≈ ns. Surface both in the log line for
        // operator readability.
        let max_sweep = engine.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_MAX_SWEEP_CYCLES);
        if max_sweep > last_max_sweep_cycles {
            crate::dlog!(
                "[brisc-timing] new max main-loop sweep (incl init_device outliers): {} cycles (~{} ns)",
                max_sweep,
                u64::from(max_sweep) * 1000 / 1350
            );
            last_max_sweep_cycles = max_sweep;
        }
        let max_steady_sweep =
            engine.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_MAX_STEADY_SWEEP_CYCLES);
        if max_steady_sweep > last_max_steady_sweep_cycles {
            crate::dlog!(
                "[brisc-timing] new max STEADY sweep (race-relevant): {} cycles (~{} ns)",
                max_steady_sweep,
                u64::from(max_steady_sweep) * 1000 / 1350
            );
            last_max_steady_sweep_cycles = max_steady_sweep;
        }
        let max_sel_path = engine.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_MAX_SEL_PATH_CYCLES);
        if max_sel_path > last_max_sel_path_cycles {
            crate::dlog!(
                "[brisc-timing] new max SEL→READY critical-path: {} cycles \
                 (~{} ns @ 1.35 GHz)",
                max_sel_path,
                u64::from(max_sel_path) * 1000 / 1350
            );
            last_max_sel_path_cycles = max_sel_path;
        }
        // #120 capture-on-READY=1 stats. SETUPS counts queue activations
        // BRISC has snapshotted; TEARDOWNS counts disable events.
        // SEL_RACES counts mid-capture SEL changes that forced an abort
        // (kernel raced past us into the next queue) — those leave the
        // shadow at its prior value, which usually means the next
        // dispatch will see stale or zero address halves.
        let setups = engine.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_QUEUE_SETUPS);
        if setups != last_queue_setups {
            let delta = setups.wrapping_sub(last_queue_setups);
            crate::dlog!(
                "[capture] {} new queue setup(s) snapshotted (cumulative {})",
                delta,
                setups
            );
            last_queue_setups = setups;
        }
        let teardowns = engine.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_QUEUE_TEARDOWNS);
        if teardowns != last_queue_teardowns {
            let delta = teardowns.wrapping_sub(last_queue_teardowns);
            crate::dlog!(
                "[capture] {} new queue teardown(s) (cumulative {})",
                delta,
                teardowns
            );
            last_queue_teardowns = teardowns;
        }
        let brisc_old_sel_rescue =
            engine.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_BRISC_OLD_SEL_RESCUE);
        if let Some(delta) = take_delta(brisc_old_sel_rescue, &mut last_brisc_old_sel_rescue) {
            // Surface the rescue counter on /metrics (#172). Per-event
            // dlog kept below — the rescue is rare and the dlog gives
            // the human-readable cumulative.
            crate::daemon::metrics::BRISC_OLD_SEL_RESCUE_TOTAL.add(delta as u64);
            crate::dlog!(
                "[capture] BRISC rescued {} OLD-sel queue setup(s) at SEL change \
                 (cumulative {})",
                delta,
                brisc_old_sel_rescue
            );
        }
        let ready_capture_sel_races =
            engine.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_READY_CAPTURE_SEL_RACES);
        if ready_capture_sel_races != last_ready_capture_sel_races {
            let delta = ready_capture_sel_races.wrapping_sub(last_ready_capture_sel_races);
            crate::dlog!(
                "[capture] BRISC aborted {} ready-capture(s) — SEL changed mid-snapshot \
                 (cumulative {}); shadow may be stale for those queues",
                delta,
                ready_capture_sel_races
            );
            last_ready_capture_sel_races = ready_capture_sel_races;
        }
        // Per-slot STATUS transitions. Snapshot registry under lock,
        // then do chip-side L1 reads outside the lock.
        //
        // TOCTOU: the snapshot captures (slot, kind) tuples; we
        // release the registry lock before the chip reads. Between
        // the snapshot and the per-slot lock re-acquire (in the
        // STATUS=0 → wipe `processed[]` branch), another thread
        // can `unregister_slot` the entry — that's why the wipe
        // uses `if let Some(reg) = ...get_mut(...)` and silently
        // skips a missing entry. Don't change this to `unwrap()`
        // even after a refactor: the gap window is real.
        //
        // We read the visible-as-MMIO `MMIO_STATUS` register, NOT
        // BRISC's private `SNAP_OFF_STATUS` snap. The snap is BRISC's
        // own diffing buffer (`brisc-firmware/virtio.c::poll_one_device`,
        // comment: "Snap is BRISC-private; no fence needed. The
        // next sweep's diff against snap is local to this hart so
        // ordering is automatic; the daemon never reads snap_addr.").
        // Pre-#159 the logger here read snap, so under sustained
        // multi-L2CPU load BRISC's store-coalescing queue could leave
        // snap stale from the daemon's L1 view — the symptom was zero
        // logged transitions for L2CPU 1+ slots while the kernel-side
        // probe successfully cycled STATUS thousands of times. Reading
        // the same MMIO register the kernel writes lands the diff on
        // a write path that has guest-side fencing, not BRISC's
        // delayed coalescer.
        let snapshot: Vec<(u32, &'static str)> = {
            let map = registry.lock().unwrap();
            map.iter()
                .map(|(&s, e)| (s, e.interrupt_kind_name()))
                .collect()
        };
        for (slot, kind) in snapshot {
            let status = engine.read_l1_u32(ve::slot_regs_base(slot) + ve::MMIO_STATUS);
            let prev = last_status.get(&slot).copied().unwrap_or(0);
            if status == prev {
                continue;
            }
            // Reset: status went to 0 from non-zero. Indicates a probe
            // restart — the kernel's reset before its own probe attempt,
            // or any later device-needs-reset cycle. Also wipe the
            // per-queue `processed[]` cursors so the fresh session
            // reads avail.ring[0]: V1 used the kick-entry epoch field
            // for this; V2 has no per-event side-channel, so we
            // detect STATUS=0 directly.
            if status == 0 && prev != 0 {
                if let Some(reg) = registry.lock().unwrap().get_mut(&slot) {
                    for p in reg.processed.iter_mut() {
                        *p = 0;
                    }
                }
                crate::dlog!(
                    "[probe-status] slot {} ({}) STATUS reset to 0 \
                     (was 0x{:02x}) — probe restart, processed[] wiped",
                    slot,
                    kind,
                    prev
                );
            }
            // DRIVER_OK transition (bit going 0 → 1). One log line per
            // probe completion, including U-Boot's pre-kernel
            // virtio_blk probe. Bench harness counts these per slot to
            // distinguish kernel-side success from U-Boot-only success.
            if status & ve::STATUS_DRIVER_OK != 0 && prev & ve::STATUS_DRIVER_OK == 0 {
                crate::dlog!(
                    "[probe-status] slot {} ({}) reached STATUS_DRIVER_OK \
                     (status=0x{:02x})",
                    slot,
                    kind,
                    status
                );
            }
            // FAILED bit set: probe gave up. Log on every transition
            // into FAILED (rare; kernel only sets this on hard probe
            // errors, not on the SEL→READY -ENOENT case which silently
            // unwinds without setting STATUS_FAILED).
            if status & ve::STATUS_FAILED != 0 && prev & ve::STATUS_FAILED == 0 {
                crate::dlog!(
                    "[probe-status] slot {} ({}) STATUS_FAILED set \
                     (status=0x{:02x}) — kernel-side probe gave up",
                    slot,
                    kind,
                    status
                );
            }
            last_status.insert(slot, status);
        }
        {
            let map = uart_registry.lock().unwrap();
            for &l2cpu_idx in map.keys() {
                if (l2cpu_idx as u32) >= crate::virtio_engine::NUM_L2CPUS {
                    continue;
                }
                let priv_base = uart::uart_private_base(l2cpu_idx);
                let drops = engine.read_l1_u32(priv_base + uart::UART_PRIV_OFF_FEED_DROP_COUNT);
                let prev = last_uart_drops[l2cpu_idx as usize];
                if drops != prev {
                    let delta = drops.wrapping_sub(prev);
                    crate::dlog!(
                        "[dispatcher] uart l2cpu {} dropped {} byte(s) (cumulative {})",
                        l2cpu_idx,
                        delta,
                        drops
                    );
                    crate::daemon::metrics::UART_FEED_DROPS_TOTAL
                        .at(l2cpu_idx)
                        .add(delta as u64);
                    last_uart_drops[l2cpu_idx as usize] = drops;
                }
            }
        }

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

/// L1 byte access surface that `collect_dirty_pairs` needs.
/// `TensixEngine` implements this directly; tests substitute a
/// fake that backs into a `Vec<u8>`. Kept narrow on purpose — the
/// processed-cursor publish happens via `engine.write_l1_u16`
/// directly (the engine is in scope at the call site, no trait
/// indirection needed).
pub(crate) trait CtrlL1Access {
    fn read_u8(&self, addr: u32) -> u8;
    fn write_u8(&self, addr: u32, value: u8);
}

impl CtrlL1Access for TensixEngine {
    fn read_u8(&self, addr: u32) -> u8 {
        TensixEngine::read_l1_u8(self, addr)
    }
    fn write_u8(&self, addr: u32, value: u8) {
        TensixEngine::write_l1_u8(self, addr, value);
    }
}

/// First-pass scan of the dirty bitmap: for each registered
/// (slot, queue) pair, read the L1 byte and clear it if set.
/// Returns the list of pairs that were dirty, in iteration order.
///
/// Clearing happens BEFORE the caller dispatches — if a guest
/// NOTIFY arrives between our clear and the dispatcher's avail-ring
/// read, BRISC sets the byte again and the next pass picks it up.
/// No work gets dropped.
///
/// `slots_with_queue_count` yields `(slot, num_queues)` for each
/// registered slot; the caller typically derives it from the
/// dispatcher's registry under lock.
fn collect_dirty_pairs<L: CtrlL1Access, I: IntoIterator<Item = (u32, u32)>>(
    l1: &L,
    slots_with_queue_count: I,
) -> Vec<(u32, u16)> {
    let max_q = crate::tensix_proto::MAX_QUEUES_PER_SLOT;
    let mut hits = Vec::new();
    for (slot, n_queues) in slots_with_queue_count {
        let n_queues = n_queues.min(max_q);
        for q in 0..n_queues {
            let addr = crate::tensix_proto::dirty_byte_addr(slot, q);
            if l1.read_u8(addr) != 0 {
                l1.write_u8(addr, 0);
                hits.push((slot, q as u16));
            }
        }
    }
    hits
}

/// V2 dispatch (#189): walk every registered (slot, queue) pair,
/// read the L1 dirty byte, clear it, and dispatch if it was set.
/// Returns the count of dispatched (slot, queue) pairs.
///
/// Each successful dispatch publishes the post-dispatch
/// `processed[qi]` cursor into `CTRL_OFF_PROCESSED` so a subsequent
/// daemon restart can re-adopt the live slot's progress without
/// re-probing guest DRAM.
///
/// The bitmap is level-sensitive: a guest QUEUE_NOTIFY storm that
/// arrives between two daemon polls coalesces into a single dirty
/// byte, so this loop cannot fall behind the way the V1 kick ring
/// could (#184).
fn drain_dirty_bitmap(
    engine: &Arc<TensixEngine>,
    registry: &Registry,
    stats: &Arc<PollerStats>,
) -> u64 {
    let mut dispatched = 0u64;
    let mut map = registry.lock().unwrap();
    let slots: Vec<(u32, u32)> = map
        .iter()
        .map(|(&slot, reg)| (slot, reg.processed.len() as u32))
        .collect();
    let hits = collect_dirty_pairs(engine.as_ref(), slots);
    for (slot, q) in hits {
        let Some(reg) = map.get_mut(&slot) else {
            continue;
        };
        if dispatch_chain(engine, reg, q).is_some() {
            dispatched += 1;
            let cur = reg.processed[q as usize];
            engine.write_l1_u16(
                crate::tensix_proto::processed_cursor_addr(slot, q as u32),
                cur,
            );
            stats
                .last_dispatch_slot_queue
                .store(((slot as u64) << 16) | q as u64, Ordering::Relaxed);
            stats.dispatches_total.fetch_add(1, Ordering::Relaxed);
        }
    }
    dispatched
}

/// Drain pending chains for one (slot, queue). Returns `Some(used_idx)`
/// if at least one chain was processed, where `used_idx` is the
/// kernel-visible `VringUsed::idx` after our final commit (read back
/// from guest DRAM via volatile load). `None` if nothing was drained.
/// On non-empty drain, fires the PLIC IRQ before returning.
fn dispatch_chain(engine: &Arc<TensixEngine>, reg: &mut RegEntry, queue_idx: u16) -> Option<u32> {
    // Skip if the NOTIFY references a queue index past what the
    // device announced at registration. Out-of-range shouldn't
    // happen for a well-behaved guest, but a misbehaving guest
    // shouldn't crash the daemon.
    if (queue_idx as usize) >= reg.processed.len() {
        crate::dlog!(
            "[dispatcher]   slot {} queue {} out of range (have {}), dropping",
            reg.slot,
            queue_idx,
            reg.processed.len()
        );
        return None;
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
        // shadow pointers yet. Bail silently; otherwise the RX-side
        // polling loop spams this for every idle iteration while
        // net's queue 0 is unconfigured.
        return None;
    }
    if queue_num == 0 || queue_num > u16::MAX as u32 {
        // Kernel hasn't published QUEUE_NUM yet, or it published
        // something larger than u16. Either way we can't index the
        // ring; bail.
        crate::dlog!(
            "[dispatcher]   slot {} queue {} bad queue_num={}, dropping",
            reg.slot,
            queue_idx,
            queue_num
        );
        return None;
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
            "[dispatcher]   slot {} queue {} pointers out of L2CPU memory range \
             (desc={:#x} avail={:#x} used={:#x}, range=[{:#x},{:#x})), dropping",
            reg.slot,
            queue_idx,
            desc_addr,
            avail_addr,
            used_addr,
            starting,
            mem_end,
        );
        return None;
    }
    let desc_q = unsafe { memory.add((desc_addr - starting) as usize) as *mut VringDesc };
    let avail_q = unsafe { memory.add((avail_addr - starting) as usize) as *mut VringAvail };
    let used_q = unsafe { memory.add((used_addr - starting) as usize) as *mut VringUsed };

    let queue_header_size = reg.device.queue_header_size();
    // Drain the avail ring fully — the kernel batches multiple
    // chains behind a single QUEUE_NOTIFY (each NOTIFY signals
    // everything the driver has queued so far). Processing one
    // chain per dispatch would leak `avail.idx - processed` chains
    // and stall on the next batch. One IRQ per drain at the end
    // is enough; the kernel's virtblk_done loops over completions
    // anyway.
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

    if !posted {
        return None;
    }
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

    // Read the kernel-visible used.idx after our final commit. The
    // dispatcher already advanced `*used_q.idx` for each chain it
    // posted; this is the value the kernel will observe. Volatile
    // because it lives in guest DRAM and the kernel may race a read.
    // u16 → u32 widen for the wire field; ring head fits trivially.
    let used_idx = unsafe { std::ptr::read_volatile(std::ptr::addr_of!((*used_q).idx)) };
    Some(used_idx as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poller_stats_default_is_zero() {
        let s = PollerStats::default();
        assert_eq!(s.dispatches_total.load(Ordering::Relaxed), 0);
        assert_eq!(s.poll_iterations.load(Ordering::Relaxed), 0);
        assert_eq!(s.last_dispatch_slot_queue.load(Ordering::Relaxed), 0);
    }

    // ---- clamp_consumer_to_ring (#101) ----

    #[test]
    fn clamp_consumer_returns_input_when_outstanding_fits_in_ring() {
        // Common-case: producer ahead of consumer by less than the ring.
        assert_eq!(clamp_consumer_to_ring(10, 5, 64), 5);
        assert_eq!(clamp_consumer_to_ring(64, 0, 64), 0);
        assert_eq!(clamp_consumer_to_ring(63, 0, 64), 0);
    }

    #[test]
    fn clamp_consumer_fast_forwards_on_overflow() {
        // producer - consumer > ring -> fast-forward.
        assert_eq!(clamp_consumer_to_ring(100, 0, 64), 100 - 64);
        assert_eq!(clamp_consumer_to_ring(2000, 100, 1024), 2000 - 1024);
    }

    #[test]
    fn clamp_consumer_handles_u32_wraparound() {
        // Consumer near the u32::MAX edge; producer has wrapped past
        // 0. wrapping_sub handles the modular gap correctly.
        let consumer: u32 = u32::MAX;
        let producer: u32 = 99; // 100 steps after consumer (mod 2^32)
        assert_eq!(producer.wrapping_sub(consumer), 100);
        // 100 > 64 -> fast-forward to producer - 64.
        assert_eq!(
            clamp_consumer_to_ring(producer, consumer, 64),
            producer.wrapping_sub(64)
        );
    }

    #[test]
    fn clamp_consumer_at_exact_ring_boundary_does_not_clamp() {
        // gap == ring is "ring is exactly full" — still readable.
        // gap > ring is the trigger.
        assert_eq!(clamp_consumer_to_ring(64, 0, 64), 0);
        assert_eq!(clamp_consumer_to_ring(65, 0, 64), 65 - 64);
    }

    // ---- take_delta ----

    #[test]
    fn take_delta_returns_none_when_value_is_unchanged() {
        let mut last = 7u32;
        assert_eq!(take_delta(7, &mut last), None);
        assert_eq!(last, 7);
    }

    #[test]
    fn take_delta_returns_simple_difference_and_advances_last() {
        let mut last = 7u32;
        assert_eq!(take_delta(10, &mut last), Some(3));
        assert_eq!(last, 10);
        assert_eq!(take_delta(15, &mut last), Some(5));
        assert_eq!(last, 15);
    }

    #[test]
    fn take_delta_uses_wrapping_sub_across_u32_max() {
        // Long-running counters wrap; saturating_sub would silently
        // lose the delta and the metric would understate drops.
        let mut last = u32::MAX - 2;
        assert_eq!(take_delta(3, &mut last), Some(6));
        assert_eq!(last, 3);
    }

    // ---- collect_dirty_pairs ----
    //
    // These cover the bitmap-iteration logic in isolation from the
    // dispatcher's lock + dispatch_chain machinery. The descriptor
    // walk that follows the dirty observation is covered separately
    // by `virtio` integration tests + the burst soak.

    /// In-memory `CtrlL1Access` impl: the addressable bytes the V2
    /// layout uses (CTRL_OFF_DIRTY..CTRL_OFF_END) backed by a `Vec<u8>`,
    /// plus a record of every read so tests can assert visit order.
    struct FakeCtrlL1 {
        bytes: std::cell::RefCell<Vec<u8>>,
        reads: std::cell::RefCell<Vec<u32>>,
    }

    impl FakeCtrlL1 {
        fn new() -> Self {
            // Cover from CTRL_BASE through CTRL_OFF_END (V2 footprint).
            let size =
                (crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_END) as usize;
            FakeCtrlL1 {
                bytes: std::cell::RefCell::new(vec![0u8; size]),
                reads: std::cell::RefCell::new(Vec::new()),
            }
        }
        fn set_dirty(&self, slot: u32, q: u32, value: u8) {
            let addr = crate::tensix_proto::dirty_byte_addr(slot, q) as usize;
            self.bytes.borrow_mut()[addr] = value;
        }
        fn dirty_at(&self, slot: u32, q: u32) -> u8 {
            let addr = crate::tensix_proto::dirty_byte_addr(slot, q) as usize;
            self.bytes.borrow()[addr]
        }
    }

    impl CtrlL1Access for FakeCtrlL1 {
        fn read_u8(&self, addr: u32) -> u8 {
            self.reads.borrow_mut().push(addr);
            self.bytes.borrow()[addr as usize]
        }
        fn write_u8(&self, addr: u32, value: u8) {
            self.bytes.borrow_mut()[addr as usize] = value;
        }
    }

    #[test]
    fn collect_dirty_pairs_empty_bitmap_returns_no_hits() {
        let l1 = FakeCtrlL1::new();
        // Two registered slots, two queues each — all bytes zero.
        let hits = collect_dirty_pairs(&l1, vec![(0u32, 2u32), (5, 2)]);
        assert!(hits.is_empty());
        // Each (slot, queue) was visited exactly once.
        assert_eq!(l1.reads.borrow().len(), 4);
    }

    #[test]
    fn collect_dirty_pairs_returns_set_bytes_and_clears_them() {
        let l1 = FakeCtrlL1::new();
        l1.set_dirty(3, 1, 1);
        l1.set_dirty(7, 0, 1);
        let hits = collect_dirty_pairs(&l1, vec![(3u32, 4u32), (7, 2)]);
        // Visit order is slot-major, queue-minor. Slot 3's queues
        // come before slot 7's.
        assert_eq!(hits, vec![(3, 1), (7, 0)]);
        // Both dirty bytes cleared after observation.
        assert_eq!(l1.dirty_at(3, 1), 0);
        assert_eq!(l1.dirty_at(7, 0), 0);
    }

    #[test]
    fn collect_dirty_pairs_clears_before_dispatch_so_concurrent_notifies_are_preserved() {
        // Simulate the race: at iter N we read+clear the byte.
        // BRISC re-sets it (here: we just write it again post-clear).
        // Iter N+1 must observe the new SET.
        let l1 = FakeCtrlL1::new();
        l1.set_dirty(0, 0, 1);

        let hits1 = collect_dirty_pairs(&l1, vec![(0u32, 1u32)]);
        assert_eq!(hits1, vec![(0, 0)]);
        assert_eq!(l1.dirty_at(0, 0), 0); // cleared

        // BRISC's concurrent NOTIFY between our read + the next pass.
        l1.set_dirty(0, 0, 1);

        let hits2 = collect_dirty_pairs(&l1, vec![(0u32, 1u32)]);
        assert_eq!(hits2, vec![(0, 0)]); // observed again
    }

    #[test]
    fn collect_dirty_pairs_idempotent_after_clear() {
        // Once cleared, repeat passes return empty until the byte
        // gets re-set externally. Catches a regression where a
        // partial clear (e.g. misaligned write) would leave a bit
        // sticky.
        let l1 = FakeCtrlL1::new();
        l1.set_dirty(2, 0, 1);
        let _ = collect_dirty_pairs(&l1, vec![(2u32, 1u32)]);
        let hits2 = collect_dirty_pairs(&l1, vec![(2u32, 1u32)]);
        let hits3 = collect_dirty_pairs(&l1, vec![(2u32, 1u32)]);
        assert!(hits2.is_empty());
        assert!(hits3.is_empty());
    }

    #[test]
    fn collect_dirty_pairs_caps_n_queues_at_max_queues_per_slot() {
        // RegEntry::processed.len() can technically exceed
        // MAX_QUEUES_PER_SLOT if a device reports more queues than
        // the protocol layout allocates. Caller passes the raw
        // length; collect_dirty_pairs must clamp so we don't read
        // past the dirty array into PROCESSED.
        let l1 = FakeCtrlL1::new();
        // Plant a dirty byte one PAST the cap — would be inside
        // the PROCESSED array if we didn't clamp. For slot 0, the
        // first OOB queue index is exactly MAX_QUEUES_PER_SLOT, so
        // the byte sits MAX_QUEUES_PER_SLOT bytes past the start
        // of the DIRTY region.
        let max_q = crate::tensix_proto::MAX_QUEUES_PER_SLOT;
        let oob_addr = crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_DIRTY + max_q;
        l1.bytes.borrow_mut()[oob_addr as usize] = 1;

        let hits = collect_dirty_pairs(&l1, vec![(0u32, max_q + 4)]);
        assert!(hits.is_empty());
        // The OOB byte must still be 1 — we should never have
        // touched it.
        assert_eq!(l1.bytes.borrow()[oob_addr as usize], 1);
    }

    #[test]
    fn collect_dirty_pairs_visits_every_registered_slot_and_queue_once() {
        // Three slots × multiple queues; every (slot, queue) the
        // caller said is registered must be checked exactly once.
        let l1 = FakeCtrlL1::new();
        // Mark different queues across the three slots.
        l1.set_dirty(0, 1, 1);
        l1.set_dirty(1, 0, 1);
        l1.set_dirty(1, 2, 1);
        l1.set_dirty(2, 0, 1);

        let hits = collect_dirty_pairs(&l1, vec![(0u32, 2u32), (1, 3), (2, 1)]);
        assert_eq!(hits, vec![(0, 1), (1, 0), (1, 2), (2, 0)]);
        // Total reads = 2 + 3 + 1 = 6.
        assert_eq!(l1.reads.borrow().len(), 6);
    }

    #[test]
    fn dirty_byte_addr_matches_layout() {
        // Pin the formula. Off-by-one or stride mismatches here
        // would silently land dirty stores in PROCESSED.
        let max_q = crate::tensix_proto::MAX_QUEUES_PER_SLOT;
        assert_eq!(
            crate::tensix_proto::dirty_byte_addr(0, 0),
            crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_DIRTY
        );
        assert_eq!(
            crate::tensix_proto::dirty_byte_addr(1, 0),
            crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_DIRTY + max_q
        );
        assert_eq!(
            crate::tensix_proto::dirty_byte_addr(0, 7),
            crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_DIRTY + 7
        );
        // Last legal slot/queue still falls inside the DIRTY range.
        let last = crate::tensix_proto::dirty_byte_addr(31, max_q - 1);
        assert!(last < crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_PROCESSED);
    }

    #[test]
    fn processed_cursor_addr_matches_layout() {
        let max_q = crate::tensix_proto::MAX_QUEUES_PER_SLOT;
        assert_eq!(
            crate::tensix_proto::processed_cursor_addr(0, 0),
            crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_PROCESSED
        );
        // Stride is 2 bytes per cursor; 2 * MAX_QUEUES_PER_SLOT bytes per slot.
        assert_eq!(
            crate::tensix_proto::processed_cursor_addr(0, 1),
            crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_PROCESSED + 2
        );
        assert_eq!(
            crate::tensix_proto::processed_cursor_addr(1, 0),
            crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_PROCESSED + max_q * 2
        );
        // Last legal cursor still inside the PROCESSED range.
        let last_addr = crate::tensix_proto::processed_cursor_addr(31, max_q - 1);
        assert!(
            last_addr + 2 <= crate::tensix_proto::CTRL_BASE + crate::tensix_proto::CTRL_OFF_END
        );
    }
}
