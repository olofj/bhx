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
pub mod lifetime;
pub mod log;
pub mod protocol;
pub mod runner;
pub mod server;
pub mod terminal;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use crate::l2cpu::L2Cpu;
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
    /// Set by the shutdown handler to make the accept loop exit.
    pub shutdown: Arc<AtomicBool>,
}

impl DaemonState {
    pub fn new(card: u32) -> Self {
        DaemonState {
            card,
            started: Instant::now(),
            l2cpus: [
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
                Mutex::new(None),
            ],
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }
}
