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

/// Subset of `TensixEngine`'s API used by `consume_kick_ring_pass`.
/// Production code uses the engine directly; tests substitute a
/// fake that reads from an in-memory ring without touching the chip.
pub(crate) trait KickRingReader {
    fn kick_producer_seq(&self) -> u32;
    fn read_kick_entry(&self, idx: u32) -> [u32; 4];
    fn set_kick_consumer_seq(&self, seq: u32);
}

impl KickRingReader for TensixEngine {
    fn kick_producer_seq(&self) -> u32 {
        TensixEngine::kick_producer_seq(self)
    }
    fn read_kick_entry(&self, idx: u32) -> [u32; 4] {
        TensixEngine::read_kick_entry(self, idx)
    }
    fn set_kick_consumer_seq(&self, seq: u32) {
        TensixEngine::set_kick_consumer_seq(self, seq);
    }
}

/// One decoded kick-ring entry, as packed by `kick_ring_push` in
/// `brisc-firmware/virtio.c`: word 0 is `(queue_idx << 16) | slot`,
/// word 1 is the producer-side `seq`, word 2 is the per-slot epoch
/// (bumped on STATUS=0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodedKick {
    pub slot: u16,
    pub queue_idx: u16,
    pub seq: u32,
    pub epoch: u32,
}

pub(crate) fn decode_kick_entry(raw: [u32; 4]) -> DecodedKick {
    DecodedKick {
        slot: (raw[0] & 0xFFFF) as u16,
        queue_idx: (raw[0] >> 16) as u16,
        seq: raw[1],
        epoch: raw[2],
    }
}

/// Drain the kick ring from `*consumer` up to the engine's current
/// `kick_producer_seq()`, invoking `on_kick` for each entry in
/// arrival order. Returns the count of entries delivered. Updates
/// the engine-side consumer register only when there's progress to
/// commit. Applies `clamp_consumer_to_ring` first so a runaway
/// producer doesn't make us replay overwritten ring slots.
pub(crate) fn consume_kick_ring_pass(
    engine: &impl KickRingReader,
    consumer: &mut u32,
    mut on_kick: impl FnMut(DecodedKick),
) -> u64 {
    let producer = engine.kick_producer_seq();
    let clamped =
        clamp_consumer_to_ring(producer, *consumer, crate::tensix_proto::KICK_RING_ENTRIES);
    if clamped != *consumer {
        crate::dlog!(
            "[kick-poller] producer {} consumer {} > ring ({}); fast-forwarding consumer to {}",
            producer,
            *consumer,
            crate::tensix_proto::KICK_RING_ENTRIES,
            clamped
        );
        *consumer = clamped;
    }
    let mut consumed = 0u64;
    // Publish the consumer to BRISC every PUBLISH_BATCH kicks during
    // the drain — not just at the end. BRISC's `kick_ring_push`
    // checks `(producer - consumer) >= KICK_RING_ENTRIES` against
    // the LAST consumer value the daemon published. If the daemon
    // is inside a long drain pass (e.g., 1000+ kicks at 10-100 µs
    // dispatch each = many ms total) and only updates the consumer
    // at the end, BRISC sees the stale pre-pass consumer the whole
    // time — and drops new kicks once `producer - stale_consumer`
    // reaches 2048, even though the daemon is actively making
    // progress. Periodic mid-drain updates fix this: BRISC sees
    // free space open up batch by batch, bursts that exceed ring
    // size in raw count are absorbed as long as our drain rate
    // keeps up. Cost is one extra L1 NoC write per 64 kicks
    // (~50 ns each) — negligible vs the 10-100 µs per-kick
    // dispatch. Batch size 64 is a round-number compromise: small
    // enough that BRISC's view stays within ~3-6 ms of reality
    // even at slow per-kick dispatch, large enough to amortize the
    // NoC writes across many kicks.
    const PUBLISH_BATCH: u64 = 64;
    while *consumer != producer {
        let raw = engine.read_kick_entry(*consumer);
        on_kick(decode_kick_entry(raw));
        *consumer = consumer.wrapping_add(1);
        consumed += 1;
        if consumed.is_multiple_of(PUBLISH_BATCH) {
            engine.set_kick_consumer_seq(*consumer);
        }
    }
    if consumed > 0 && !consumed.is_multiple_of(PUBLISH_BATCH) {
        // Final tail update so BRISC sees the post-drain consumer.
        // The mid-drain publish above already covers full batches;
        // this catches the residual.
        engine.set_kick_consumer_seq(*consumer);
    }
    consumed
}

/// Snapshot the ratchet-style counter pattern the kick poller uses
/// for chip-side stats (kick drops, sel/ready races, queue setup
/// counts, etc): "if the value changed, log/account the wrapping
/// delta and remember the new value." Returns `Some(delta)` on
/// change, `None` otherwise. Critically uses `wrapping_sub` — the
/// counters are u32 monotonics on the chip side that can wrap
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
    /// Last STATUS=0 epoch we saw on a kick. BRISC bumps the per-slot
    /// epoch on STATUS=0 (see `handle_status_change` in
    /// `brisc-firmware/virtio.c`). When a new kick arrives with an
    /// epoch we haven't seen, the guest has reinitialized the queue —
    /// reset `processed[]` so we read avail.ring[0] for the new
    /// session instead of avail.ring[stale_index]. Initial value is
    /// `u32::MAX` so the first kick (epoch=0 from a fresh BRISC, or
    /// any non-MAX value from an adopted firmware) always triggers a
    /// reset, which is benign at registration time.
    pub last_epoch: u32,
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
            last_epoch: u32::MAX,
        }
    }

    /// Diagnostic helper used in dlog output during debugging. Used
    /// by the kick-poller's per-slot probe-status logging (#123) so
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

/// Per-L2CPU UART (#78) registry. Maps `l2cpu_idx` → the slot's
/// `console_hub`. The kick poller routes UART TX bytes (kick-ring
/// slots 16..19) through `push_chip_output` on the appropriate
/// hub. Separate from the virtio `Registry` so register/unregister
/// is independent — `register_uart` flips bit `16+idx` in the
/// active-slots bitmap, telling BRISC to start sweeping that L2CPU's
/// UART reg file.
pub type UartRegistry = Arc<Mutex<HashMap<u8, Arc<ConsoleHub>>>>;

/// Daemon-side kick consumer. Owns a thread that loops on
/// `engine.kick_producer_seq()` and consumes new entries.
pub struct KickPoller {
    pub stats: Arc<PollerStats>,
    pub registry: Registry,
    pub uart_registry: UartRegistry,
    /// Cloned for register/unregister to push the active-slots
    /// bitmap into BRISC L1 — BRISC uses it to skip non-active
    /// slots in its sweep. Without this, BRISC polls all 16 slots
    /// and the per-slot revisit period is ~4µs, slow enough to
    /// lose the SEL→READY race against stock kernels.
    engine: Arc<TensixEngine>,
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
        let uart_registry: UartRegistry = Arc::new(Mutex::new(HashMap::new()));
        let exit = Arc::new(AtomicBool::new(false));
        let stats_thread = Arc::clone(&stats);
        let registry_thread = Arc::clone(&registry);
        let uart_registry_thread = Arc::clone(&uart_registry);
        let exit_thread = Arc::clone(&exit);
        let engine_thread = Arc::clone(&engine);
        let join = thread::Builder::new()
            .name("tensix-kick-poller".to_string())
            .spawn(move || {
                run_poll_loop(
                    engine_thread,
                    stats_thread,
                    registry_thread,
                    uart_registry_thread,
                    exit_thread,
                )
            })
            .expect("spawn tensix-kick-poller");
        KickPoller {
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
    /// kicks for `slot` will dispatch to `entry.device`'s
    /// VirtioDeviceImpl methods. dispatch_boot calls this once per
    /// enabled device.
    pub fn register_slot(&self, entry: RegEntry) {
        let slot = entry.slot;
        self.registry.lock().unwrap().insert(slot, entry);
        self.publish_active_mask();
    }

    /// Unregister a slot — called when an L2CPU is being torn down
    /// (slot.shutdown via daemon stop or boot --force). Future
    /// kicks for `slot` log a "no registration" warning and bump
    /// stats; they don't touch the device or fire IRQs.
    pub fn unregister_slot(&self, slot: u32) {
        self.registry.lock().unwrap().remove(&slot);
        self.publish_active_mask();
    }

    /// Register an L2CPU's 16550 UART. Future kicks with slot
    /// `uart::slot_for_l2cpu(idx)` route the byte payload through the
    /// registered `console_hub` via `push_chip_output`. Sets the
    /// corresponding bit in the active-slots bitmap so BRISC starts
    /// sweeping the L2CPU's UART reg file.
    pub fn register_uart(&self, l2cpu_idx: u8, hub: Arc<ConsoleHub>) {
        self.uart_registry.lock().unwrap().insert(l2cpu_idx, hub);
        self.publish_active_mask();
    }

    /// Unregister an L2CPU's UART. Clears bit `16+idx` of the
    /// active-slots bitmap so BRISC stops sweeping that reg file.
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

impl Drop for KickPoller {
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
    // briefly, IDLE during long quiet stretches. Saves CPU when the
    // guest is doing nothing without sacrificing notify latency
    // when it picks up.
    //
    // FAST tier was 50µs originally; bumped to 10µs after the
    // 2048-entry ring (#184) absorbed peak bursts but still let
    // 200-1000 kicks drop on sustained disk-to-disk I/O. The
    // shorter poll lets the drain pass start sooner after each
    // batch lands, narrowing the producer-vs-consumer gap. Per-
    // poll cost is one L1 read for the kick-ring header (~100 ns
    // NoC RTT), so 10µs polling is ~1% of one CPU core in NoC
    // overhead during sustained activity — comfortable.
    const FAST_SLEEP: Duration = Duration::from_micros(10);
    const SLOW_SLEEP: Duration = Duration::from_millis(1);
    const IDLE_SLEEP: Duration = Duration::from_millis(10);
    const FAST_WINDOW: Duration = Duration::from_millis(200);
    const IDLE_WINDOW: Duration = Duration::from_secs(2);

    let mut consumer: u32 = engine.kick_ring_header().1;
    let mut last_active = std::time::Instant::now();
    // Last time we emitted an idle-snapshot log line. Reset to
    // `last_active` after each snapshot so the next active period
    // re-arms it. See `log_used_flags_snapshot` rationale: when a
    // hang lands, we want one diagnostic dump of every queue's
    // used.flags to tell daemon-state issues from guest-cache
    // staleness, but we don't want to spam after that.
    let mut last_snapshot = last_active;

    // Per-L2CPU UART feed-ring consumer state. M6.1 #79 v3 (Phase B
    // revised): the daemon polls these rings directly via the chip
    // TLB instead of going through BRISC's kick ring. Each ring slot
    // is 4 bytes (one byte in low 8 bits) and there are 1024 slots,
    // so a stock-distro boot's ~10 KB of TX fits comfortably without
    // any rate limiting.
    let mut uart_consumer: [u32; crate::virtio_engine::NUM_L2CPUS as usize] =
        [0; crate::virtio_engine::NUM_L2CPUS as usize];

    // Last-seen drop counters (#101). BRISC and TRISC0 expose
    // monotonic u32s; we cache the last value we saw and surface
    // every delta to dlog! + the matching Prometheus counter, so an
    // operator polling `/metrics` sees the actual drop count rather
    // than waiting for a future-restart-then-zero rollover.
    let mut last_kick_drops: u32 = 0;
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
    // Track per-slot STATUS transitions. Bench harnesses (and
    // operators debugging probe failures — see #123) need a
    // definitive "kernel finished probing this device" signal.
    //
    // We can't just log the first DRIVER_OK per slot: U-Boot's own
    // virtio_blk driver probes and reaches DRIVER_OK before the
    // kernel even loads, then the kernel writes STATUS=0 (reset)
    // and runs its own probe. If that re-probe fails, the snap
    // would still be at U-Boot's DRIVER_OK and a "first DRIVER_OK"
    // log would mask the kernel-side failure.
    //
    // Instead we track per-slot last-seen status and log every
    // DRIVER_OK transition (i.e., bit 4 going from 0 to 1) and every
    // reset (status going to 0). On a healthy debian-13 + uboot
    // boot, virtio_blk produces 2 DRIVER_OK lines (U-Boot + kernel)
    // and the others produce 1 (kernel only). A failed kernel-side
    // probe shows as zero DRIVER_OK lines after the reset.
    let mut last_status: HashMap<u32, u32> = HashMap::new();

    // High-water-mark tracking for the kick ring (#184). Diagnostic
    // for "how close are we skirting overflow" without having to
    // wait for an actual drop. `hw_mark` is the all-time max
    // (producer - consumer) gap observed; `hw_logged_threshold`
    // is the highest fill-percentage bucket we've already emitted
    // a log line for, so we don't spam — one log per crossing of
    // 50%, 75%, 90%, 95%.
    let mut hw_mark: u32 = 0;
    let mut hw_logged_threshold: u32 = 0;

    // Backpressure state (#184). When the kick ring crosses
    // THROTTLE_HIGH_PCT fill, the daemon writes
    // VRING_USED_F_NO_NOTIFY into every active queue's used.flags
    // so the guest's `virtqueue_kick_prepare` skips the
    // QUEUE_NOTIFY MMIO write — throttling kick production at the
    // source rather than letting BRISC drop them downstream.
    // Released at THROTTLE_LOW_PCT (hysteresis to avoid bouncing).
    //
    // Why so low (25%/5%): the throttle is reactive, applied only
    // on poll-iteration boundaries. If a single dispatch pass
    // takes unusually long (slow disk write, page-cache flush),
    // BRISC can pile in many kicks before the next poll.
    // Empirically observed bursts of ~2000 kicks added between
    // two polls during install workloads, putting an earlier 75%
    // (1536) threshold miles too late: the throttle engaged at
    // the same millisecond as the overflow drop fired. Engage at
    // 25% (512 of 2048) so even a ~150 ms dispatch outlier at 3K
    // kicks/s only lands at ~1000, safely under the ring size.
    // Tradeoff: more time spent throttled, lower peak throughput.
    // Acceptable when the alternative is install death.
    const THROTTLE_HIGH_PCT: u32 = 25;
    const THROTTLE_LOW_PCT: u32 = 5;
    let throttle_high_gap =
        crate::tensix_proto::KICK_RING_ENTRIES.saturating_mul(THROTTLE_HIGH_PCT) / 100;
    let throttle_low_gap =
        crate::tensix_proto::KICK_RING_ENTRIES.saturating_mul(THROTTLE_LOW_PCT) / 100;
    let mut throttled = false;

    while !exit.load(Ordering::Relaxed) {
        // Snapshot the producer BEFORE the drain so we can compute
        // the pre-drain ring fill. Costs one extra L1 read per
        // iter (~100 ns NoC RTT) — acceptable at 10µs poll cadence.
        let pre_producer = engine.kick_producer_seq();
        let pre_gap = pre_producer.wrapping_sub(consumer);
        crate::daemon::metrics::KICK_RING_CURRENT_GAP.set(pre_gap as i64);

        // Backpressure: hysteretic guest-side throttle via
        // VRING_USED_F_NO_NOTIFY. Engage at HIGH, release at LOW.
        let want_throttle = if throttled {
            pre_gap > throttle_low_gap
        } else {
            pre_gap >= throttle_high_gap
        };
        if want_throttle != throttled {
            let refresh_irqs = set_used_no_notify_for_all_queues(&engine, &registry, want_throttle);
            crate::daemon::metrics::KICK_THROTTLE_ENGAGED.set(if want_throttle { 1 } else { 0 });
            if want_throttle {
                crate::daemon::metrics::KICK_THROTTLE_TRANSITIONS_TOTAL.add(1);
            }
            crate::dlog!(
                "[kick-poller] backpressure {} at gap {}/{} (high={}, low={})",
                if want_throttle { "ENGAGED" } else { "RELEASED" },
                pre_gap,
                crate::tensix_proto::KICK_RING_ENTRIES,
                throttle_high_gap,
                throttle_low_gap,
            );
            if !want_throttle && refresh_irqs > 0 {
                crate::dlog!(
                    "[kick-poller] backpressure release: fired {} cache-refresh IRQs",
                    refresh_irqs,
                );
            }
            // Rescue any descriptors the guest queued during the
            // throttle window whose QUEUE_NOTIFY happened while
            // `used.flags=1` and was silently suppressed. Without
            // this, a single suppressed kick on (say) the console
            // TX queue leaves one `lag=1` chain in avail forever
            // and the guest hangs waiting on its completion.
            // Idempotent — caught-up queues are no-ops.
            if !want_throttle {
                let rescued = rescan_all_queues(&engine, &registry);
                if rescued > 0 {
                    crate::dlog!(
                        "[kick-poller] backpressure release: rescued {} avail-ring entries",
                        rescued,
                    );
                    crate::daemon::metrics::KICK_RESCUED_TOTAL.add(rescued);
                    last_active = std::time::Instant::now();
                }
            }
            throttled = want_throttle;
        }

        if pre_gap > hw_mark {
            hw_mark = pre_gap;
            crate::daemon::metrics::KICK_RING_HIGH_WATER.set(hw_mark as i64);
            // Emit a log line the FIRST time we cross each
            // fill-bucket boundary, so an operator chasing #184
            // sees how close we got without having to scrape
            // metrics. Buckets pinned to engineering interest
            // levels: 50% = "approaching margin", 75% = "tight",
            // 90% = "imminent", 95% = "skid mark".
            let pct = pre_gap.saturating_mul(100) / crate::tensix_proto::KICK_RING_ENTRIES;
            let bucket = if pct >= 95 {
                95
            } else if pct >= 90 {
                90
            } else if pct >= 75 {
                75
            } else if pct >= 50 {
                50
            } else {
                0
            };
            if bucket > hw_logged_threshold {
                let now = std::time::Instant::now();
                let elapsed = now.duration_since(last_active);
                let tier = if elapsed < FAST_WINDOW {
                    "FAST"
                } else if elapsed < IDLE_WINDOW {
                    "SLOW"
                } else {
                    "IDLE"
                };
                crate::dlog!(
                    "[kick-poller] kick ring fill new high-water: {}/{} ({}%, bucket {}%, tier {})",
                    pre_gap,
                    crate::tensix_proto::KICK_RING_ENTRIES,
                    pct,
                    bucket,
                    tier
                );
                hw_logged_threshold = bucket;
            }
        }
        let consumed_this_pass = consume_kick_ring_pass(&*engine, &mut consumer, |kick| {
            // Dispatch every kick through the virtio registry. There
            // used to be a slots-20..23 carve-out here for the legacy
            // syscon-poweroff path, but that path is gone (#166), and
            // those slot numbers ARE virtio slots for l2cpu 2's
            // BLK1/BLK2/padding under the post-bump DEVS_PER_L2CPU=8
            // layout. Routing them anywhere other than the registry
            // would intercept l2cpu 2's cidata kicks and silently kill
            // dispatch — exactly the bug that tore down a sibling
            // L2CPU when the previous "defensive log" eats slot 20.
            let mut map = registry.lock().unwrap();
            if let Some(reg) = map.get_mut(&(kick.slot as u32)) {
                // STATUS=0 epoch tracking: BRISC bumps per-slot epoch
                // on every guest STATUS=0 (see brisc-firmware
                // virtio.c::handle_status_change). When a kick lands
                // with an epoch we haven't seen before, the guest has
                // reinit'd the queue — reset `processed[]` so the
                // first dispatch reads avail.ring[0] for the fresh
                // session instead of avail.ring[stale_idx]. Without
                // this, AlmaLinux's U-Boot→kernel handoff (U-Boot
                // probes virtio_blk, writes STATUS=0 on cleanup,
                // kernel re-probes) had us pulling stale indices from
                // the kernel's freshly-zeroed avail ring and writing
                // id=0 into the used ring, tripping the kernel's
                // "id 0 is not a head" guard.
                if kick.epoch != reg.last_epoch {
                    for p in reg.processed.iter_mut() {
                        *p = 0;
                    }
                    reg.last_epoch = kick.epoch;
                }
                if let Some(used_idx) = dispatch_chain(&engine, reg, kick.queue_idx) {
                    // Push a completion to BRISC for diagnostics +
                    // future BRISC-side IRQ dispatch. The PLIC IRQ
                    // itself is fired daemon-side today.
                    engine.push_completion(kick.slot, kick.queue_idx, used_idx);
                }
            } else {
                crate::dlog!(
                    "[kick-poller]   no registration for slot {}, dropping kick",
                    kick.slot
                );
            }
            stats.last_kick_slot_queue.store(
                ((kick.slot as u64) << 16) | (kick.queue_idx as u64),
                Ordering::Relaxed,
            );
            stats.kicks_consumed.fetch_add(1, Ordering::Relaxed);
        });
        if consumed_this_pass > 0 {
            last_active = std::time::Instant::now();
        }

        // Async RX path: net (libslirp delivers packets) and console
        // (operator keystrokes via input_buf) don't get a guest kick
        // when their backing source has data. We poll their RX queues
        // here — `queue_has_data` (per-device, called inside
        // `process_one_chain_for_queue`) returns true only when the
        // backing source has bytes, so the dispatch is a no-op on
        // idle queues. Same FAST/SLOW/IDLE adaptive cadence as the
        // kick path.
        let mut rx_drained = false;
        {
            let mut map = registry.lock().unwrap();
            for (slot, reg) in map.iter_mut() {
                if !matches!(
                    reg.interrupt_kind,
                    InterruptKind::Net | InterruptKind::Console
                ) {
                    continue;
                }
                if let Some(used_idx) = dispatch_chain(&engine, reg, 0) {
                    engine.push_completion(*slot as u16, 0, used_idx);
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
                        "[kick-poller] uart l2cpu {} producer {} consumer {} > ring ({}); fast-forwarding consumer to {}",
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

        // #101: surface drop counters so a stalled daemon shows up in
        // metrics + the daemon log instead of silently corrupting the
        // ring. Polled once per iteration — the IDLE tier (10 ms) is
        // plenty of headroom; even at the FAST tier (50 µs) the
        // overhead is two extra L1 reads per registered L2CPU.
        let kick_drops = engine.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_KICK_DROPS);
        if let Some(delta) = take_delta(kick_drops, &mut last_kick_drops) {
            crate::dlog!(
                "[kick-poller] BRISC dropped {} kick(s) (cumulative {})",
                delta,
                kick_drops
            );
            crate::daemon::metrics::KICK_DROPS_TOTAL.add(delta as u64);
            // Rescue path (#184): a dropped kick means BRISC discarded
            // the notify, but the descriptor is still sitting in the
            // guest's avail ring untouched — `dispatch_chain` walks
            // from `processed[qi]` to current avail.idx, so calling
            // it on every active (slot, queue) pair processes
            // anything BRISC failed to notify us about. Without this,
            // the guest's kernel virtio-blk waits ~30 s for its I/O
            // timeout, retries, and the install workload usually
            // gives up before the timeout-retry cycle catches up.
            // With it, the dropped kick becomes a temporary latency
            // hit (fixed cost: scan all queues once) instead of a
            // multi-second stall.
            //
            // Cost: one dispatch_chain call per (slot, queue) pair.
            // With ~5 active slots × 1-2 queues each, that's ~10
            // calls. Each is a no-op (early return) if the queue's
            // processed[qi] already matches avail.idx — i.e., we
            // didn't actually miss anything for that queue. Real
            // work happens only on queues that have unprocessed
            // entries. Runs only when drops are detected, so no
            // cost in steady state.
            let rescued = rescan_all_queues(&engine, &registry);
            if rescued > 0 {
                crate::dlog!(
                    "[kick-poller] rescued {} avail-ring entries via post-drop rescan",
                    rescued
                );
                crate::daemon::metrics::KICK_RESCUED_TOTAL.add(rescued);
                last_active = std::time::Instant::now();
            }
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
        // shadow at its prior value, which usually means the kick that
        // follows will see stale or zero address halves.
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
        // We read the visible-as-MMIO `MMIO_STATUS` register, NOT
        // BRISC's private `SNAP_OFF_STATUS` snap. The snap is BRISC's
        // own diffing buffer (`brisc-firmware/virtio.c::poll_one_device`
        // line ~818, comment: "Snap is BRISC-private; no fence needed.
        // The next sweep's diff against snap is local to this hart so
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
            // or any later device-needs-reset cycle.
            if status == 0 && prev != 0 {
                crate::dlog!(
                    "[probe-status] slot {} ({}) STATUS reset to 0 \
                     (was 0x{:02x}) — probe restart",
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
            for (&l2cpu_idx, _) in map.iter() {
                if (l2cpu_idx as u32) >= crate::virtio_engine::NUM_L2CPUS {
                    continue;
                }
                let priv_base = uart::uart_private_base(l2cpu_idx);
                let drops = engine.read_l1_u32(priv_base + uart::UART_PRIV_OFF_FEED_DROP_COUNT);
                let prev = last_uart_drops[l2cpu_idx as usize];
                if drops != prev {
                    let delta = drops.wrapping_sub(prev);
                    crate::dlog!(
                        "[kick-poller] uart l2cpu {} dropped {} byte(s) (cumulative {})",
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
        // Once-per-idle-period diagnostic for #184. After a chunk
        // of activity goes quiet for SNAPSHOT_AFTER seconds, dump
        // every active queue's used.flags. If the daemon's view
        // shows flag=0 across the board but no kicks are
        // arriving, that's evidence of host→guest cache staleness
        // (the workaround for which is the cache-refresh IRQ on
        // throttle release). If anything shows flag=1 unexpectedly,
        // our state machine has a bug.
        const SNAPSHOT_AFTER: Duration = Duration::from_secs(5);
        if idle >= SNAPSHOT_AFTER && last_snapshot < last_active {
            log_used_flags_snapshot(&engine, &registry);
            last_snapshot = std::time::Instant::now();
        }
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

/// Per-queue idle snapshot for #184 hang diagnosis. For each
/// active (slot, queue) pair logs: used.flags (daemon-side read
/// of what we wrote), avail.idx (what the guest has published),
/// processed[qi] (how far we've drained), used.idx (what we've
/// committed back to the guest).
///
/// If `avail.idx > processed[qi]` while idle, the guest queued
/// requests we never saw a kick for — typical of throttle-window
/// suppression that the guest never recovered from. If
/// `used.idx == processed[qi]` and the guest is waiting on a
/// completion, the guest's cache likely holds a stale `used.idx`
/// (host wrote the new value, guest's L1 still has the old one).
fn log_used_flags_snapshot(engine: &Arc<TensixEngine>, registry: &Registry) {
    use crate::virtio_engine::{
        shadow_queue_addr, SHADOW_Q_OFF_DEVICE_HI, SHADOW_Q_OFF_DEVICE_LO, SHADOW_Q_OFF_DRIVER_HI,
        SHADOW_Q_OFF_DRIVER_LO,
    };
    let map = registry.lock().unwrap();
    if map.is_empty() {
        return;
    }
    let mut parts: Vec<String> = Vec::new();
    for reg in map.values() {
        let starting = reg.l2cpu.starting_address();
        let mem_end = starting + reg.l2cpu.memory_size();
        let memory = reg.l2cpu.get_memory_ptr();
        let n_queues = reg.processed.len() as u32;
        for qi in 0..n_queues {
            let used_lo =
                engine.read_l1_u32(shadow_queue_addr(reg.slot, qi, SHADOW_Q_OFF_DEVICE_LO));
            let used_hi =
                engine.read_l1_u32(shadow_queue_addr(reg.slot, qi, SHADOW_Q_OFF_DEVICE_HI));
            let used_addr = ((used_hi as u64) << 32) | (used_lo as u64);
            let avail_lo =
                engine.read_l1_u32(shadow_queue_addr(reg.slot, qi, SHADOW_Q_OFF_DRIVER_LO));
            let avail_hi =
                engine.read_l1_u32(shadow_queue_addr(reg.slot, qi, SHADOW_Q_OFF_DRIVER_HI));
            let avail_addr = ((avail_hi as u64) << 32) | (avail_lo as u64);
            if used_addr == 0
                || used_addr < starting
                || used_addr >= mem_end
                || avail_addr == 0
                || avail_addr < starting
                || avail_addr >= mem_end
            {
                continue;
            }
            let used_flags_ptr =
                unsafe { memory.add((used_addr - starting) as usize) as *const u16 };
            let used_idx_ptr =
                unsafe { memory.add((used_addr - starting) as usize + 2) as *const u16 };
            let avail_idx_ptr =
                unsafe { memory.add((avail_addr - starting) as usize + 2) as *const u16 };
            let used_flags = unsafe { std::ptr::read_volatile(used_flags_ptr) };
            let used_idx = unsafe { std::ptr::read_volatile(used_idx_ptr) };
            let avail_idx = unsafe { std::ptr::read_volatile(avail_idx_ptr) };
            let processed = reg.processed[qi as usize];
            let lag = avail_idx.wrapping_sub(processed);
            parts.push(format!(
                "({},{}) flags={} avail.idx={} proc={} (lag={}) used.idx={}",
                reg.slot, qi, used_flags, avail_idx, processed, lag, used_idx,
            ));
        }
    }
    if parts.is_empty() {
        return;
    }
    crate::dlog!("[kick-poller] idle snapshot: {}", parts.join(" | "));
}

/// Walk one descriptor chain on `(slot, queue_idx)` via the
/// existing virtio descriptor processor. Reads per-queue
/// desc/avail/used pointers from BRISC L1 shadow, maps them into
/// the L2CPU's memory namespace, calls the device's
/// `process_queue_*` hooks, writes the used-ring entry, fires the
/// Walk every registered (slot, queue) and call `dispatch_chain`
/// on it. Idempotent: a queue whose `processed[qi]` already
/// matches `avail.idx` is a no-op. Used by both the
/// kick-drop recovery path (BRISC dropped notifies, descriptors
/// sit in the avail ring) and the backpressure-release path (a
/// guest QUEUE_NOTIFY that lined up with `used.flags=1` got
/// silently suppressed; the descriptor is in avail but no kick
/// will ever come). Returns the number of (slot, queue) pairs
/// that actually had work to do.
fn rescan_all_queues(engine: &Arc<TensixEngine>, registry: &Registry) -> u64 {
    let mut rescued = 0u64;
    let mut map = registry.lock().unwrap();
    for (slot, reg) in map.iter_mut() {
        let n_queues = reg.processed.len() as u16;
        for qi in 0..n_queues {
            if let Some(used_idx) = dispatch_chain(engine, reg, qi) {
                engine.push_completion(*slot as u16, qi, used_idx);
                rescued += 1;
            }
        }
    }
    rescued
}

/// Toggle `VRING_USED_F_NO_NOTIFY` in every active queue's used.flags
/// across every registered slot (#184 backpressure path). When
/// `enable=true`, the bit is set — the guest's
/// `virtqueue_kick_prepare` will check the flag, see it set, and
/// skip the QUEUE_NOTIFY MMIO write. The kick ring stops getting
/// new entries while the daemon drains. When `enable=false`, the
/// bit is cleared and one PLIC IRQ is fired per slot that received
/// a write — see the cache-refresh rationale below. Returns the
/// number of slots that received the cache-refresh IRQ so the
/// caller can log it.
///
/// Per-queue used-ring address comes from BRISC L1 shadow (the
/// `SHADOW_Q_OFF_DEVICE_LO/HI` fields), captured at QUEUE_READY=1
/// commit. Skips queues whose `used_addr` is zero (uninitialized)
/// or out-of-range vs the L2CPU's DRAM window (defensive — guest
/// shouldn't be able to publish a bad address but treat it as
/// suspicious if observed).
///
/// Cache-refresh IRQ on release: the X280 doesn't snoop host-side
/// PCIe stores, so writing flag=0 from the daemon lands in DRAM
/// but the guest's L1 cache may hold the prior flag=1 value
/// indefinitely. `virtqueue_kick_prepare` does a plain READ_ONCE
/// of `used->flags`, hits cached =1, and keeps skipping the
/// QUEUE_NOTIFY MMIO write — I/O hangs after a single
/// throttle/release cycle. Firing a spurious PLIC IRQ forces the
/// guest's virtio IRQ handler to read used.idx; used.idx and
/// used.flags share the same 64-byte cache line, so the load
/// refreshes the whole line and the next `virtqueue_kick_prepare`
/// sees the host's flag=0.
fn set_used_no_notify_for_all_queues(
    engine: &Arc<TensixEngine>,
    registry: &Registry,
    enable: bool,
) -> usize {
    use crate::virtio_engine::{shadow_queue_addr, SHADOW_Q_OFF_DEVICE_HI, SHADOW_Q_OFF_DEVICE_LO};
    let flag_val: u16 = if enable { 1 } else { 0 };
    let map = registry.lock().unwrap();
    let mut irqs_fired = 0usize;
    for reg in map.values() {
        let starting = reg.l2cpu.starting_address();
        let mem_end = starting + reg.l2cpu.memory_size();
        let memory = reg.l2cpu.get_memory_ptr();
        let n_queues = reg.processed.len() as u32;
        let mut wrote_any = false;
        for qi in 0..n_queues {
            let used_lo =
                engine.read_l1_u32(shadow_queue_addr(reg.slot, qi, SHADOW_Q_OFF_DEVICE_LO));
            let used_hi =
                engine.read_l1_u32(shadow_queue_addr(reg.slot, qi, SHADOW_Q_OFF_DEVICE_HI));
            let used_addr = ((used_hi as u64) << 32) | (used_lo as u64);
            if used_addr == 0 || used_addr < starting || used_addr >= mem_end {
                continue;
            }
            // Used ring layout: u16 flags at offset 0. Volatile store
            // so the compiler doesn't fold consecutive writes.
            let flags_ptr = unsafe { memory.add((used_addr - starting) as usize) as *mut u16 };
            unsafe {
                std::ptr::write_volatile(flags_ptr, flag_val);
            }
            wrote_any = true;
        }
        if !enable && wrote_any {
            let interrupt_status_addr_l1 = ve::slot_regs_base(reg.slot) + ve::MMIO_INTERRUPT_STATUS;
            let interrupt_status_ptr = engine.l1_ptr(interrupt_status_addr_l1) as *mut u32;
            reg.interrupt_ctl
                .set_interrupt(interrupt_status_ptr, reg.interrupt_number);
            crate::virtio::bump_interrupt_metric(reg.interrupt_kind, reg.l2cpu.idx() as u8);
            irqs_fired += 1;
        }
    }
    irqs_fired
}

/// PLIC IRQ on success. Returns `true` if a chain was posted.
/// Drain pending chains for one (slot, queue). Returns `Some(used_idx)`
/// if at least one chain was processed, where `used_idx` is the
/// kernel-visible `VringUsed::idx` after our final commit (read back
/// from guest DRAM via volatile load). `None` if nothing was drained.
fn dispatch_chain(engine: &Arc<TensixEngine>, reg: &mut RegEntry, queue_idx: u16) -> Option<u32> {
    // Drop the kick if it references a queue index past what the
    // device announced at registration. Out of bounds shouldn't
    // happen for a well-behaved guest, but a misbehaving guest
    // shouldn't crash the daemon.
    if (queue_idx as usize) >= reg.processed.len() {
        crate::dlog!(
            "[kick-poller]   slot {} queue {} out of range (have {}), dropping",
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
        // shadow pointers yet. Drop the kick silently; otherwise the
        // RX-side polling loop spams this for every idle iteration
        // while net's queue 0 is unconfigured.
        return None;
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
        return None;
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
    use std::cell::{Cell, RefCell};

    use super::*;

    #[test]
    fn poller_stats_default_is_zero() {
        let s = PollerStats::default();
        assert_eq!(s.kicks_consumed.load(Ordering::Relaxed), 0);
        assert_eq!(s.poll_iterations.load(Ordering::Relaxed), 0);
        assert_eq!(s.last_kick_slot_queue.load(Ordering::Relaxed), 0);
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

    // ---- decode_kick_entry ----

    #[test]
    fn decode_kick_entry_unpacks_word0_into_slot_and_queue() {
        // word 0 = (queue_idx << 16) | slot ; matches BRISC firmware's
        // kick_ring_push packing.
        let raw = [(7u32 << 16) | 0x12, 0xdead_beef, 0x42, 0];
        let kick = decode_kick_entry(raw);
        assert_eq!(kick.slot, 0x12);
        assert_eq!(kick.queue_idx, 7);
        assert_eq!(kick.seq, 0xdead_beef);
        assert_eq!(kick.epoch, 0x42);
    }

    #[test]
    fn decode_kick_entry_handles_max_field_widths() {
        // Both halves of word 0 saturated. Slot and queue_idx are u16 so
        // the upper bits of word 0 must not bleed into slot.
        let raw = [0xFFFF_FFFFu32, 0xFFFF_FFFF, 0xFFFF_FFFF, 0];
        let kick = decode_kick_entry(raw);
        assert_eq!(kick.slot, 0xFFFF);
        assert_eq!(kick.queue_idx, 0xFFFF);
        assert_eq!(kick.seq, 0xFFFF_FFFF);
        assert_eq!(kick.epoch, 0xFFFF_FFFF);
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

    // ---- consume_kick_ring_pass + a hand-rolled FakeKickRing ----

    /// Backing store for the ring tests. Mirrors the BRISC-side ring's
    /// shape: a `KICK_RING_ENTRIES`-deep array of 4-word slots, plus
    /// monotonic producer/consumer sequences. Production code goes
    /// through `TensixEngine`; tests drive this directly so we can
    /// pin ordering, wraparound, and clamp behavior without a chip.
    struct FakeKickRing {
        producer: Cell<u32>,
        consumer_writes: RefCell<Vec<u32>>,
        slots: RefCell<Vec<[u32; 4]>>,
    }

    impl FakeKickRing {
        fn new() -> Self {
            FakeKickRing {
                producer: Cell::new(0),
                consumer_writes: RefCell::new(Vec::new()),
                slots: RefCell::new(vec![
                    [0u32; 4];
                    crate::tensix_proto::KICK_RING_ENTRIES as usize
                ]),
            }
        }

        /// Push at the current producer seq and advance. Mirrors the
        /// firmware's `kick_ring_push` (slot index = seq mod ring_entries).
        fn push(&self, slot: u16, queue_idx: u16, seq: u32, epoch: u32) {
            let prod = self.producer.get();
            let idx = (prod % crate::tensix_proto::KICK_RING_ENTRIES) as usize;
            self.slots.borrow_mut()[idx] =
                [(queue_idx as u32) << 16 | (slot as u32), seq, epoch, 0];
            self.producer.set(prod.wrapping_add(1));
        }

        /// Push without bumping `producer` — for tests that need to set
        /// up a stale slot the consumer should skip.
        fn plant(&self, slot_index: u32, raw: [u32; 4]) {
            let idx = (slot_index % crate::tensix_proto::KICK_RING_ENTRIES) as usize;
            self.slots.borrow_mut()[idx] = raw;
        }

        /// Force `producer` to a specific value — for tests that exercise
        /// runaway-producer semantics.
        fn set_producer(&self, seq: u32) {
            self.producer.set(seq);
        }
    }

    impl KickRingReader for FakeKickRing {
        fn kick_producer_seq(&self) -> u32 {
            self.producer.get()
        }
        fn read_kick_entry(&self, idx: u32) -> [u32; 4] {
            let i = (idx % crate::tensix_proto::KICK_RING_ENTRIES) as usize;
            self.slots.borrow()[i]
        }
        fn set_kick_consumer_seq(&self, seq: u32) {
            self.consumer_writes.borrow_mut().push(seq);
        }
    }

    fn drain(ring: &FakeKickRing, consumer: &mut u32) -> Vec<DecodedKick> {
        let collected: RefCell<Vec<DecodedKick>> = RefCell::new(Vec::new());
        let count = consume_kick_ring_pass(ring, consumer, |k| collected.borrow_mut().push(k));
        let v = collected.into_inner();
        assert_eq!(v.len() as u64, count, "callback count must match return");
        v
    }

    #[test]
    fn consume_pass_delivers_entries_in_arrival_order() {
        let ring = FakeKickRing::new();
        ring.push(0, 0, 1, 0);
        ring.push(1, 2, 2, 0);
        ring.push(5, 0, 3, 0);
        let mut consumer = 0u32;

        let got = drain(&ring, &mut consumer);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].slot, 0);
        assert_eq!(got[1].slot, 1);
        assert_eq!(got[1].queue_idx, 2);
        assert_eq!(got[2].slot, 5);
        // Consumer advanced to producer.
        assert_eq!(consumer, 3);
        // The engine-side consumer register was committed exactly once.
        assert_eq!(*ring.consumer_writes.borrow(), vec![3]);
    }

    #[test]
    fn consume_pass_returns_zero_and_skips_commit_when_ring_empty() {
        let ring = FakeKickRing::new();
        let mut consumer = 17u32;
        ring.set_producer(17);

        let got = drain(&ring, &mut consumer);
        assert!(got.is_empty());
        assert_eq!(consumer, 17);
        // No progress -> no consumer-register write. This matters
        // because every L1 write is an MMIO transaction; spamming
        // them on idle ticks would burn PCIe bandwidth.
        assert!(ring.consumer_writes.borrow().is_empty());
    }

    #[test]
    fn consume_pass_tolerates_seq_zero_payload() {
        // The ring entry's seq word can legitimately be zero (the
        // first push after a fresh boot, or just on wrap). The
        // consumer should NOT treat seq=0 as a sentinel; it's a
        // real entry and must be delivered.
        let ring = FakeKickRing::new();
        ring.push(3, 0, 0, 0); // <-- seq=0
        ring.push(4, 0, 1, 0);
        let mut consumer = 0u32;

        let got = drain(&ring, &mut consumer);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].slot, 3);
        assert_eq!(got[0].seq, 0);
        assert_eq!(got[1].slot, 4);
    }

    #[test]
    fn consume_pass_walks_full_ring_around_physical_slots() {
        // Push exactly one ring's worth of entries, all with distinct
        // monotonically-bumped slots (mod 31 to fit u8). Drain. Even
        // though the physical slot index in L1 wraps, the consumer
        // sees them all in arrival order because it tracks the u32
        // monotonic seq, not the modular index.
        let ring = FakeKickRing::new();
        let n = crate::tensix_proto::KICK_RING_ENTRIES;
        for i in 0..n {
            ring.push((i % 31) as u16, 0, i, 0);
        }
        let mut consumer = 0u32;

        let got = drain(&ring, &mut consumer);
        assert_eq!(got.len() as u32, n);
        for (i, kick) in got.iter().enumerate() {
            assert_eq!(kick.slot, (i as u16) % 31);
            assert_eq!(kick.seq, i as u32);
        }
        assert_eq!(consumer, n);
    }

    #[test]
    fn consume_pass_clamps_when_producer_outpaced_consumer_by_more_than_ring() {
        // Producer ran a full ring + a few entries ahead while the
        // consumer was stalled. Those overrun entries are lost — their
        // ring slots have already been overwritten by newer pushes.
        // The clamp drops the unreachable head and resumes from the
        // oldest still-readable position.
        let ring = FakeKickRing::new();
        let n = crate::tensix_proto::KICK_RING_ENTRIES;
        // Plant one "newest" entry at every physical slot; if the
        // consumer ever reaches a stale slot, we'd see a mismatch.
        for s in 0..n {
            ring.plant(s, [0xAA, n + s, 0, 0]);
        }
        // Producer is `n + 5`; consumer is at 0. Gap is n + 5, bigger
        // than the ring — the first 5 entries (seqs 0..4) are gone.
        ring.set_producer(n + 5);
        let mut consumer = 0u32;

        let got = drain(&ring, &mut consumer);
        // Clamp reset the consumer to producer - n = 5; we then
        // delivered n entries (5..n+5).
        assert_eq!(got.len() as u32, n);
        assert_eq!(consumer, n + 5);
    }

    #[test]
    fn consume_pass_advances_consumer_through_u32_wraparound() {
        // Consumer near u32::MAX; producer has wrapped to a small
        // positive value. wrapping_sub keeps the gap correct.
        let ring = FakeKickRing::new();
        let start: u32 = u32::MAX - 1;
        // Plant 3 entries; we only need them at the right physical slots.
        for offset in 0..3u32 {
            let seq = start.wrapping_add(offset);
            ring.plant(seq, [offset, seq, 0, 0]);
        }
        ring.set_producer(start.wrapping_add(3));
        let mut consumer = start;

        let got = drain(&ring, &mut consumer);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].slot, 0);
        assert_eq!(got[1].slot, 1);
        assert_eq!(got[2].slot, 2);
        // Consumer ended at start + 3 (modular).
        assert_eq!(consumer, start.wrapping_add(3));
    }
}
