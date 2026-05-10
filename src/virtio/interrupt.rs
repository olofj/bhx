// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! PLIC interrupt handling for VirtIO devices.

use std::ptr;
use std::sync::atomic::{self, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::tlb::TlbWindow;

/// Default latch-window busy-wait in [`InterruptController::set_interrupt`]
/// between the assert and de-assert of the PLIC pending bit (#195 fix).
///
/// At 0–4 µs the X280 PLIC reliably misses ~1 % of edges under load
/// (the original openSUSE-install stall pattern). At 9 µs the gap
/// closes — every fire reaches the kernel — with a 1.26× mild surplus
/// (PLIC re-samples the held source once more on average before our
/// de-assert lands). At 10 µs the surplus jumps to 2.17×; 15 µs to
/// 7×; 100 µs to 100×. The phase transition between "loss" and
/// "storm" is unusually sharp at exactly 9 µs.
///
/// 10 µs is one safety µs above the transition. Trade-off: kernel
/// IRQ handler runs ~2× per actual completion (cheap), in exchange
/// for zero missed completions.
///
/// Override via `BHX_PLIC_LATCH_US` (see [`init_latch_window_from_env`])
/// for diagnostics. The full latch-window sweep that picked this
/// value is captured in #195's discussion thread.
const DEFAULT_LATCH_US: u64 = 10;

/// Latch-window for the PLIC assert→de-assert busy-wait, in
/// microseconds. Initialized from `BHX_PLIC_LATCH_US` (or
/// [`DEFAULT_LATCH_US`] if unset) at process startup and then served
/// from this atomic on the hot path — no per-IRQ env lookup.
static LATCH_BUSY_WAIT_US: AtomicU64 = AtomicU64::new(DEFAULT_LATCH_US);

/// Initialize the latch-window from `BHX_PLIC_LATCH_US`, falling back
/// to [`DEFAULT_LATCH_US`] if unset. Called once during daemon
/// startup. Safe to call from any number of threads — last writer
/// wins.
///
/// The env var is primarily a diagnostic override; production should
/// use the default. Setting it to `0` reproduces the pre-#195
/// missed-edge bug (useful only for regression-soak baselines).
pub fn init_latch_window_from_env() {
    if let Some(us) = std::env::var("BHX_PLIC_LATCH_US")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        LATCH_BUSY_WAIT_US.store(us, Ordering::Relaxed);
        if us != DEFAULT_LATCH_US {
            eprintln!(
                "[plic] BHX_PLIC_LATCH_US={} — set_interrupt will busy-wait \
                 {} us between assert and de-assert (default is {})",
                us, us, DEFAULT_LATCH_US
            );
        }
    }
    // Default already wired into LATCH_BUSY_WAIT_US's static
    // initializer; no store needed in the unset-env-var path.
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

    /// Set interrupt: write bit to PLIC source register, fence,
    /// busy-wait [`DEFAULT_LATCH_US`] (#195) for the X280 PLIC to
    /// sample the assert and route to the hart, then de-assert.
    ///
    /// Writing only our bit (not OR-ing with the current register
    /// contents) is intentional and carried over from the original
    /// C++ port — the held source register doesn't track other
    /// devices' assert state in a way that's safe to read-modify-write
    /// against. The C++ author tried OR-in once and rolled it back
    /// (see #195's discussion of the "FIXME: multiple interrupts on
    /// the plic seems to be buggy" comment).
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
            ptr::write_volatile(reg, 1u32 << (interrupt_number - 5));
            atomic::fence(Ordering::SeqCst);
            // (#195) Hold the source register asserted long enough
            // for the X280 PLIC to sample the rising edge and route
            // to the hart. Below ~9 µs the PLIC misses ~1 % of edges
            // under load — the openSUSE-install-stall pattern.
            // Above ~10 µs the PLIC starts re-sampling our held
            // source and the kernel storms (2× at 10 µs, 100× at
            // 100 µs). 10 µs threads the needle.
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
