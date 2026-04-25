// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

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
pub mod terminal;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::l2cpu::L2Cpu;
use crate::shared_chip::SharedChip;
use crate::virtio::interrupt::InterruptController;

use console_hub::ConsoleHub;

/// One running disk or net worker, plus its stop flag. The thread handle is
/// kept so [`L2CpuSlot::shutdown`] can join it.
pub struct WorkerHandle {
    pub exit: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
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
    pub idx: u8,
    pub l2cpu: Arc<L2Cpu>,
    pub interrupt: Arc<InterruptController>,
    pub console_hub: Arc<ConsoleHub>,
    /// Byte-level input channel: client reader threads push into this, the
    /// chip console loop pops from it and feeds the chip RX ring.
    pub console_input_tx: Sender<u8>,
    pub console_worker: WorkerHandle,
    pub disks: Vec<DiskWorker>,
    pub net: Option<WorkerHandle>,
    /// Wall-clock instant the slot was installed. Drives
    /// `tt_bh_l2cpu_uptime_seconds`. Set once at construction; never
    /// updated.
    pub started: Instant,
}

pub struct DiskWorker {
    pub path: String,
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
    /// Single shared access point for chip-wide AXI registers on NOC tile
    /// (8,0) — PLL, reset unit, `L2CPU_RESET`. Concurrent boots serialize
    /// their PLL steps and reset R-M-W through `SharedChip::seq_lock`
    /// instead of racing through independently-configured TLB windows.
    /// Kept as an `Arc` so worker threads can hold their own references
    /// if they ever need chip-wide register access. See
    /// <https://github.com/olofj/tt-bh-rust/issues/1>.
    pub shared_chip: Arc<SharedChip>,
    /// Set by the shutdown handler to make the accept loop exit.
    pub shutdown: Arc<AtomicBool>,
}

impl DaemonState {
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
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

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
