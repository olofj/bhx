// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! PLIC interrupt handling for VirtIO devices.

use std::ptr;
use std::sync::atomic::{self, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::tlb::TlbWindow;

/// PLIC latch-window tuning knob (#195 investigation). When set, inserts
/// a busy-wait of this many microseconds between the `write_volatile(reg,
/// 1 << bit)` (assert) and `write_volatile(reg, 0)` (de-assert) calls in
/// [`InterruptController::set_interrupt`]. Default `0` reproduces the
/// pre-#195 behavior.
///
/// Read once from the env var `BHX_PLIC_LATCH_US` at process startup and
/// then served from this atomic on the hot path — no per-IRQ syscalls.
///
/// **Not a production-tuning knob.** This exists so we can sweep latch
/// windows during the PLIC-edge characterization that picks #195's fix.
/// Once #195 lands a real fix, this knob can be removed.
static LATCH_BUSY_WAIT_US: AtomicU64 = AtomicU64::new(0);

/// Initialize the latch-window knob from `BHX_PLIC_LATCH_US`. Called once
/// during daemon startup. Safe to call from any number of threads, the
/// last writer wins.
pub fn init_latch_window_from_env() {
    let us = std::env::var("BHX_PLIC_LATCH_US")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    LATCH_BUSY_WAIT_US.store(us, Ordering::Relaxed);
    if us > 0 {
        eprintln!(
            "[plic] BHX_PLIC_LATCH_US={} — set_interrupt will busy-wait \
             {} us between assert and de-assert (#195 investigation knob)",
            us, us
        );
    }
}

/// Shared interrupt controller state for PLIC register access.
pub struct InterruptController {
    pub window: TlbWindow,
    pub lock: Mutex<()>,
}

// InterruptController is Send+Sync because access to the PLIC register
// is protected by the Mutex, and TlbWindow raw pointers are only accessed
// under the lock.
unsafe impl Send for InterruptController {}
unsafe impl Sync for InterruptController {}

impl InterruptController {
    pub fn new(window: TlbWindow) -> Self {
        InterruptController {
            window,
            lock: Mutex::new(()),
        }
    }

    /// Set interrupt: write bit to PLIC, fence, then clear.
    /// BUG PRESERVED: overwrites entire register instead of OR-ing (matches C++).
    pub fn set_interrupt(&self, interrupt_status: *mut u32, interrupt_number: u32) {
        assert!(
            interrupt_number >= 5,
            "interrupt_number ({}) must be >= 5 to avoid underflow in PLIC bit shift",
            interrupt_number
        );

        // Acquire the lock first, protecting both the MMIO interrupt_status
        // read-modify-write AND the PLIC register access.
        let _guard = self.lock.lock().unwrap();

        // Set VIRTIO_MMIO_INT_VRING in interrupt_status
        let status_val = unsafe { ptr::read_volatile(interrupt_status) };
        unsafe {
            ptr::write_volatile(interrupt_status, 1 | status_val);
        }

        let reg = self.window.get_window() as *mut u32;
        unsafe {
            // BUG PRESERVED: sets only our interrupt, doesn't OR with existing
            ptr::write_volatile(reg, 1u32 << (interrupt_number - 5));
            atomic::fence(Ordering::SeqCst);
            // (#195) Optional latch-window busy-wait. Default 0 = the
            // existing tight assert/de-assert pattern that loses
            // edges under load. Non-zero values widen the window so
            // the X280 PLIC has more time to latch the rising edge
            // before we de-assert. Used for the latch-window
            // characterization that picks the real fix.
            let latch_us = LATCH_BUSY_WAIT_US.load(Ordering::Relaxed);
            if latch_us > 0 {
                let deadline = Instant::now() + Duration::from_micros(latch_us);
                while Instant::now() < deadline {
                    std::hint::spin_loop();
                }
            }
            ptr::write_volatile(reg, 0u32);
        }
    }

    /// Ack interrupt — intentional NO-OP to avoid race with kernel handler.
    pub fn ack_interrupt(&self, _interrupt_ack: *mut u32) {
        // Intentionally empty — see C++ comment about timing race
    }
}
