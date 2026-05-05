// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Per-card daemon: owns `L2Cpu` handles, runs the virtio workers, and serves
//! client requests over a unix control socket.
//!
//! Layout of this module:
//! - [`protocol`]: wire format (JSON + SCM_RIGHTS) and request/response enums.
//! - [`lifetime`]: runtime dir + pidfile + `stop`/`status` helpers a client
//!   can call without spinning up the full daemon runtime.
//! - [`console_hub`]: per-L2CPU scrollback ring and fan-out to attached
//!   client sockets.
//! - [`chip_console`]: the long-running chip-side console thread (daemon
//!   counterpart to `crate::console::console_main`).
//! - [`server`]: unix socket accept loop + request dispatch.

pub mod chip_console;
pub mod client;
pub mod console_hub;
pub mod fork;
pub mod lifetime;
pub mod log;
pub mod metrics;
pub mod protocol;
pub mod runner;
pub mod sandbox;
pub mod server;
pub mod sigbus;
pub mod terminal;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Instant;

/// Lock a `Mutex` while converting poisoning into a daemon-recoverable
/// `Internal` error rather than an `unwrap()` panic. Use inside RPC
/// dispatch handlers so a single panicked worker doesn't bring down
/// the whole daemon (#104).
///
/// Drop impls and signal handlers can't return Result, so they keep
/// using plain `lock().unwrap()` — when those panic, the daemon is
/// already on its way out anyway.
pub trait LockExt<T> {
    fn lock_or_internal_error(&self) -> crate::Result<MutexGuard<'_, T>>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_or_internal_error(&self) -> crate::Result<MutexGuard<'_, T>> {
        self.lock()
            .map_err(|_| crate::Error::internal("daemon mutex poisoned (worker thread paniced)"))
    }
}

use crate::l2cpu::L2Cpu;
use crate::shared_chip::SharedChip;
use crate::virtio::interrupt::InterruptController;

use console_hub::ConsoleHub;

/// One running disk or net worker, plus its stop flag. The thread handle is
/// kept so [`L2CpuSlot::shutdown`] can join it.
pub struct WorkerHandle {
    pub exit: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
    /// Human-readable identifier for the worker (e.g. "disk l2cpu 2 @
    /// rootfs.ext4 (engine)"). Set at construction; not currently read
    /// at runtime, but populated everywhere so a future log line can
    /// pick it up without a separate refactor.
    #[allow(dead_code)]
    pub description: String,
}

impl WorkerHandle {
    pub fn stop_and_join(mut self) {
        self.exit.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// State of a per-L2CPU slot inside the daemon.
pub struct L2CpuSlot {
    /// Slot index (0..3). Self-documenting; kept on the slot for
    /// future call sites to avoid threading the index through
    /// separately.
    #[allow(dead_code)]
    pub idx: u8,
    pub l2cpu: Arc<L2Cpu>,
    pub interrupt: Arc<InterruptController>,
    pub console_hub: Arc<ConsoleHub>,
    /// Byte-level input channel: client reader threads push into this, the
    /// chip console loop pops from it and feeds the chip RX ring.
    pub console_input_tx: Sender<u8>,
    pub console_worker: WorkerHandle,
    pub disks: Vec<DiskWorker>,
    /// Set of `DEV_BLK*` device indices (inside `[0, DEVS_PER_L2CPU)`)
    /// for which a `virtio,mmio` node was emitted at cold-boot time
    /// (#117). Linux's virtio probe walks the DT once at boot and
    /// doesn't rescan, so only these slots can accept a post-boot
    /// `add-disk`. Slots outside this set are silently invisible to
    /// the kernel and `dispatch_add_disk` rejects them with a clear
    /// error rather than letting the operator wonder why their disk
    /// never shows up.
    pub blk_dtb_dev_idx: Vec<u32>,
    pub net: Option<WorkerHandle>,
    /// virtio-console worker (#51). When `Some`, kernel sees a
    /// virtio-mmio device with id=3 in the third virtio slot. Operator
    /// keystrokes are fanned out to both this and `console_input_tx`
    /// in `client_reader_main`; whichever HVC driver the kernel
    /// activates as its console absorbs them.
    pub virtio_console: Option<VirtioConsoleSlot>,
    /// virtio-rng worker (#62). Provides entropy to the guest. Required
    /// to satisfy `EFI_RNG_PROTOCOL` on the U-Boot+GRUB+shim chained-boot
    /// path; useful as plain `/dev/random` backing on direct-kernel paths.
    pub virtio_rng: Option<WorkerHandle>,
    /// Wall-clock instant the slot was installed. Drives
    /// `bhx_l2cpu_uptime_seconds`. Set once at construction; never
    /// updated.
    pub started: Instant,
}

/// Per-L2CPU virtio-console state — the worker handle plus the
/// keystroke queue the worker drains. Held inside the slot so client
/// reader threads can fan input into it.
pub struct VirtioConsoleSlot {
    pub worker: WorkerHandle,
    /// Operator → guest keystroke buffer, drained by the worker on
    /// each RX descriptor. Bounded at `RX_BUFFER_CAP` bytes; overflow
    /// drops oldest in `client_reader_main`.
    pub input_buf: Arc<std::sync::Mutex<std::collections::VecDeque<u8>>>,
}

pub struct DiskWorker {
    pub path: String,
    /// Engine slot (`l2cpu_idx * DEVS_PER_L2CPU + dev_idx`) the disk
    /// occupies — needed so `RemoveDisk` can unregister exactly the
    /// kick-poller entry that backs this worker. Stored explicitly
    /// rather than recomputed because multi-disk support means a
    /// given (l2cpu, disk) tuple can land at any of `DEV_BLK`,
    /// `DEV_BLK1`, or `DEV_BLK2`.
    pub slot_idx: u32,
    /// Operator-supplied serial — `cidata` for cloud-init seeds,
    /// arbitrary string for data disks, `None` for the legacy
    /// rootfs-only single-disk shape (which derives its serial from
    /// the L2CPU index).
    pub name: Option<String>,
    pub worker: WorkerHandle,
}

impl L2CpuSlot {
    pub fn shutdown(self) {
        for d in self.disks {
            d.worker.stop_and_join();
        }
        if let Some(n) = self.net {
            n.stop_and_join();
        }
        if let Some(vc) = self.virtio_console {
            vc.worker.stop_and_join();
        }
        if let Some(rng) = self.virtio_rng {
            rng.stop_and_join();
        }
        self.console_worker.stop_and_join();
        // Arc<L2Cpu> and Arc<InterruptController> drop here when the last
        // reference (held by the workers we just joined) is released.
    }
}

/// Daemon-wide shared state. `l2cpus[i]` is `Some` only while that L2CPU
/// has been booted (or warm-resumed) by this daemon. Mutex-protected because
/// client handlers run on independent threads.
pub struct DaemonState {
    pub card: u32,
    pub started: Instant,
    pub l2cpus: [Mutex<Option<L2CpuSlot>>; 4],
    /// Set by the startup warm-resume probe when a core's reset bit is 1
    /// but its OSBIdbug / VIRTUART magic is missing. Cleared on successful
    /// cold `boot`. Read by `dispatch_status` to report `Wedged`.
    pub wedged: [AtomicBool; 4],
    /// Single shared access point for chip-wide control registers on NOC
    /// tile (8,0) — the ARC tile + reset unit (PLL, reset unit,
    /// `L2CPU_RESET`). Concurrent boots serialize
    /// their PLL steps and reset R-M-W through `SharedChip::seq_lock`
    /// instead of racing through independently-configured TLB windows.
    /// Kept as an `Arc` so worker threads can hold their own references
    /// if they ever need chip-wide register access. See
    /// <https://github.com/olofj/bhx/issues/1>.
    pub shared_chip: Arc<SharedChip>,
    /// Tensix tile reserved for the M3+ virtio-mmio engine (#69). One
    /// tile serves all four L2CPUs on the card. Brought up lazily on
    /// the first boot under the `virtio-engine` feature flag —
    /// `None` until then, and unconditionally `None` when the flag is
    /// off. `Mutex` so concurrent boots serialize the bring-up
    /// (only the first one runs the firmware load + reset release).
    pub tensix_engine: Mutex<Option<Arc<crate::tensix_engine::TensixEngine>>>,
    /// Daemon-side kick poller (#71 M5.5a). Spawned alongside the
    /// engine bring-up; consumes the kick ring (BRISC → daemon)
    /// and (in M5.5b+) dispatches each kick to the relevant
    /// per-(slot, queue) device handler. Lifetime tied to
    /// `DaemonState`: dropped on daemon shutdown.
    pub kick_poller: Mutex<Option<crate::tensix_data_plane::KickPoller>>,
    /// Card-wide gate around the `dispatch_boot` decide-and-do-reset
    /// step (#162). Without it, two concurrent boots in the same iter
    /// both observe `running=true` from the previous iter, both call
    /// `reset_board()`, and the second blip lands while the first
    /// boot is mid-`L2Cpu::new`, killing it. With the lock the first
    /// boot's `reset_board` clears `running`; the second boot then
    /// sees `running=false` and skips. After release, both boots
    /// proceed in parallel through the per-L2CPU image-load path,
    /// which has been concurrent-safe under load.
    pub boot_lock: Mutex<()>,
    /// Per-L2CPU shutdown scrollback tails (#160). Captured at
    /// `internal_stop` time as the last
    /// [`crate::daemon::console_hub::SHUTDOWN_TAIL_BYTES`] of the live
    /// hub's ring (which by then has the goodbye line appended). Read
    /// by `dispatch_attach_console` when the slot is `None`, so an
    /// operator who runs `bhx connect` after a slot has torn down can
    /// still see the last screenful or two.
    /// Cleared on cold-boot of the same L2CPU so the next slot's
    /// scrollback isn't conflated with the previous one's tail.
    pub shutdown_tails: [Mutex<Vec<u8>>; 4],
    /// Set by the shutdown handler to make the accept loop exit.
    pub shutdown: Arc<AtomicBool>,
}

impl DaemonState {
    /// Step the chip-wide L2CPU PLL down to 200 MHz when no slot is
    /// currently booted. Call after a teardown that drains the slot
    /// table — `dispatch_stop`, `boot --force` teardown, daemon
    /// shutdown loop. The PLL comes back up to 1750 MHz automatically
    /// on the next boot via `reset_x280`'s existing step-up. See #95.
    pub fn maybe_idle_pll(&self) {
        // Best-effort: if a slot mutex is poisoned the daemon is already
        // unwinding from a panic, so just skip the PLL step rather than
        // propagating. Either path leaves the chip in a recoverable state
        // because the next reset_x280 unconditionally steps the PLL up.
        //
        // Both predicates have to hold:
        //   1. No daemon-side slot is alive (no workers polling chip
        //      state at 1750 MHz).
        //   2. No L2CPU has its release bit set on `L2CPU_RESET`.
        //      Stepping the PLL while a core is mid-execution corrupts
        //      its OpenSBI state — symptomatic on `daemon stop` (which
        //      tears down workers but leaves the core released) where
        //      the next warm-resume's OSBIdbug eye-catcher probe came
        //      back as 0xbd instead of 0x4f. See #95 + soak hardware
        //      runs after #93.
        let any_booted = self
            .l2cpus
            .iter()
            .any(|m| m.lock().map(|g| g.is_some()).unwrap_or(true));
        if any_booted {
            return;
        }
        for idx in 0..4usize {
            match self.shared_chip.l2cpu_is_running(idx) {
                Ok(true) => return,
                Ok(false) => {}
                Err(_) => return, // chip access failed — be conservative.
            }
        }
        self.shared_chip.idle_pll();
    }

    /// Build daemon state with a ready-made `SharedChip`. The server
    /// constructs the `SharedChip` at daemon startup (`SharedChip::new(card)`)
    /// and passes an `Arc` in here; tests pass a placeholder (see
    /// `SharedChip::placeholder`) so `DaemonState::new` stays hardware-free.
    pub fn new(card: u32, shared_chip: Arc<SharedChip>) -> Self {
        DaemonState {
            card,
            started: Instant::now(),
            l2cpus: [
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
            ],
            wedged: [
                AtomicBool::new(false),
                AtomicBool::new(false),
                AtomicBool::new(false),
                AtomicBool::new(false),
            ],
            shared_chip,
            tensix_engine: Mutex::new(None),
            kick_poller: Mutex::new(None),
            boot_lock: Mutex::new(()),
            shutdown_tails: [
                Mutex::new(Vec::new()),
                Mutex::new(Vec::new()),
                Mutex::new(Vec::new()),
                Mutex::new(Vec::new()),
            ],
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Lazy getter for the Tensix virtio engine. First call brings
    /// up the tile (picks via M2, loads M3 firmware, releases BRISC),
    /// then spawns the daemon-side kick poller; subsequent calls
    /// return the cached `Arc`.
    pub fn get_or_bring_up_tensix_engine(
        &self,
    ) -> std::io::Result<Arc<crate::tensix_engine::TensixEngine>> {
        let mut guard = self.tensix_engine.lock().unwrap();
        if let Some(eng) = guard.as_ref() {
            return Ok(Arc::clone(eng));
        }
        let eng = crate::tensix_engine::TensixEngine::bring_up(self.card, &self.shared_chip)?;
        let arc = Arc::new(eng);
        // Spawn the kick poller against the same Arc so it consumes
        // events the BRISC firmware produces. The poller's thread
        // holds its own clone of the Arc; the engine outlives the
        // poller because the poller's drop joins its thread before
        // releasing the reference.
        let poller = crate::tensix_data_plane::KickPoller::spawn(Arc::clone(&arc));
        *self.kick_poller.lock().unwrap() = Some(poller);
        *guard = Some(Arc::clone(&arc));
        Ok(arc)
    }

    /// Daemon warm-resume: adopt the engine that the previous daemon
    /// instance left running on the chip, without halting BRISC or
    /// reloading firmware. Must be called before any cold boot RPC
    /// (which would otherwise lazily call `bring_up` and clobber the
    /// running firmware). Idempotent — second call on an already-
    /// adopted engine is a no-op.
    ///
    /// Failure is non-fatal: if the chip has lost firmware (stats
    /// magic mismatch), this returns Err and the next cold-boot RPC
    /// will go through `bring_up` as if no warm engine ever existed.
    pub fn adopt_running_tensix_engine(&self) -> std::io::Result<()> {
        let mut guard = self.tensix_engine.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }
        let eng = crate::tensix_engine::TensixEngine::adopt_running(self.card, &self.shared_chip)?;
        let arc = Arc::new(eng);
        let poller = crate::tensix_data_plane::KickPoller::spawn(Arc::clone(&arc));
        *self.kick_poller.lock().unwrap() = Some(poller);
        *guard = Some(arc);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn lock_or_internal_error_yields_internal_when_poisoned() {
        // Poison a Mutex by panicking inside a `lock()` scope on a
        // background thread, then verify lock_or_internal_error
        // surfaces it as crate::Error::Internal rather than panicking
        // on its own.
        let m = Arc::new(Mutex::new(0u32));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _guard = m2.lock().unwrap();
            panic!("intentional");
        })
        .join();
        assert!(m.is_poisoned());
        let err = m.lock_or_internal_error().unwrap_err();
        assert!(matches!(err, crate::Error::Internal(_)));
    }

    #[test]
    fn lock_or_internal_error_yields_guard_on_healthy_mutex() {
        let m = Mutex::new(42u32);
        let g = m.lock_or_internal_error().unwrap();
        assert_eq!(*g, 42);
    }

    #[test]
    fn fresh_daemon_state_has_no_slots_and_no_wedged_cores() {
        let s = DaemonState::new(0, Arc::new(SharedChip::placeholder()));
        for idx in 0..4 {
            assert!(
                s.l2cpus[idx].lock().unwrap().is_none(),
                "slot {} should start empty",
                idx
            );
            assert!(
                !s.wedged[idx].load(Ordering::Relaxed),
                "wedged[{}] should start false",
                idx
            );
        }
        assert!(!s.shutdown.load(Ordering::Relaxed));
        assert_eq!(s.card, 0);
    }

    #[test]
    fn wedged_flag_set_and_clear_per_core() {
        // Exercises the read/write semantics dispatch_status relies on.
        let s = DaemonState::new(1, Arc::new(SharedChip::placeholder()));
        s.wedged[2].store(true, Ordering::Relaxed);
        assert!(s.wedged[2].load(Ordering::Relaxed));
        assert!(!s.wedged[0].load(Ordering::Relaxed));
        assert!(!s.wedged[1].load(Ordering::Relaxed));
        assert!(!s.wedged[3].load(Ordering::Relaxed));
        s.wedged[2].store(false, Ordering::Relaxed);
        assert!(!s.wedged[2].load(Ordering::Relaxed));
    }
}
