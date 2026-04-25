// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! PLIC interrupt handling for VirtIO devices.

use std::ptr;
use std::sync::atomic::{self, Ordering};
use std::sync::Mutex;

use crate::tlb::TlbWindow;

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
            ptr::write_volatile(reg, 0u32);
        }
    }

    /// Ack interrupt — intentional NO-OP to avoid race with kernel handler.
    pub fn ack_interrupt(&self, _interrupt_ack: *mut u32) {
        // Intentionally empty — see C++ comment about timing race
    }
}
