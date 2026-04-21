// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! TLB window abstraction — RAII wrappers around TLB allocations with volatile access.

use std::os::unix::io::RawFd;
use std::ptr;

use crate::kmd::{
    self, AllocateTlb, ConfigureTlb, FreeTlb, FreeTlbIn, NocTlbConfig,
};

// This code requires 64-bit pointers for 4GB TLB windows and NOC addresses.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("tt-bh-linux requires a 64-bit target");

pub const TWO_MEG: usize = 1 << 21;
pub const FOUR_GIG: usize = 1usize << 32;

/// Raw TLB allocation handle. Drops in order: munmap, then FREE_TLB ioctl.
///
/// # Safety
/// The caller must ensure that the file descriptor `fd` remains open for the
/// entire lifetime of this `TlbHandle`. Closing the fd before dropping the
/// handle will cause the FREE_TLB ioctl to fail (leaking kernel TLB resources)
/// or, worse, to operate on a recycled fd.
pub struct TlbHandle {
    fd: RawFd,
    tlb_id: u32,
    tlb_base: *mut u8,
    tlb_size: usize,
}

// TlbHandle contains raw pointers to device memory that are not shared across threads.
// The pointers are only used within single-threaded contexts (volatile reads/writes),
// so Send is safe. Sync is NOT implemented — concurrent access requires external sync.
unsafe impl Send for TlbHandle {}

impl TlbHandle {
    pub fn new(
        fd: RawFd,
        size: usize,
        config: &NocTlbConfig,
        base: *mut u8,
        use_wc: bool,
    ) -> std::io::Result<Self> {
        // 1. Allocate TLB
        let mut allocate = AllocateTlb::default();
        allocate.input.size = size as u64;
        unsafe {
            kmd::ioctl_allocate_tlb(fd, &mut allocate)
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        }
        let tlb_id = allocate.output.id;

        // 2. Configure TLB
        let mut configure = ConfigureTlb::default();
        configure.input.id = tlb_id;
        configure.input.config = *config;
        if let Err(e) = unsafe { kmd::ioctl_configure_tlb(fd, &mut configure) } {
            // Cleanup on failure
            let mut free = FreeTlb { input: FreeTlbIn { id: tlb_id }, output: Default::default() };
            unsafe { let _ = kmd::ioctl_free_tlb(fd, &mut free); }
            return Err(std::io::Error::from_raw_os_error(e as i32));
        }

        // 3. mmap
        let offset = if use_wc {
            allocate.output.mmap_offset_wc
        } else {
            allocate.output.mmap_offset_uc
        };

        let flags = if base.is_null() {
            libc::MAP_SHARED
        } else {
            libc::MAP_SHARED | libc::MAP_FIXED
        };

        let mmap_addr = if base.is_null() {
            ptr::null_mut()
        } else {
            base as *mut libc::c_void
        };

        let mem = unsafe {
            libc::mmap(
                mmap_addr,
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                fd,
                offset as libc::off_t,
            )
        };

        if mem == libc::MAP_FAILED {
            let mut free = FreeTlb { input: FreeTlbIn { id: tlb_id }, output: Default::default() };
            unsafe { let _ = kmd::ioctl_free_tlb(fd, &mut free); }
            return Err(std::io::Error::last_os_error());
        }

        Ok(TlbHandle {
            fd,
            tlb_id,
            tlb_base: mem as *mut u8,
            tlb_size: size,
        })
    }

    pub fn data(&self) -> *mut u8 {
        self.tlb_base
    }

    pub fn size(&self) -> usize {
        self.tlb_size
    }
}

impl Drop for TlbHandle {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.tlb_base as *mut libc::c_void, self.tlb_size);
        }
        let mut free = FreeTlb {
            input: FreeTlbIn { id: self.tlb_id },
            output: Default::default(),
        };
        unsafe {
            let _ = kmd::ioctl_free_tlb(self.fd, &mut free);
        }
    }
}

/// A TLB window of a given SIZE (must be power of 2) providing volatile read/write access.
pub struct TlbWindow {
    offset: usize,
    window: TlbHandle,
    window_size: usize,
}

impl TlbWindow {
    pub fn new(
        fd: RawFd,
        x: u16,
        y: u16,
        addr: u64,
        window_size: usize,
        base: *mut u8,
        use_wc: bool,
    ) -> std::io::Result<Self> {
        assert!(
            window_size.is_power_of_two(),
            "window_size must be a power of 2"
        );
        let mask = window_size - 1;
        let offset = (addr as usize) & mask;

        let config = NocTlbConfig {
            addr: addr & !(mask as u64),
            x_end: x,
            y_end: y,
            ..Default::default()
        };

        let window = TlbHandle::new(fd, window_size, &config, base, use_wc)?;

        Ok(TlbWindow {
            offset,
            window,
            window_size,
        })
    }

    /// Create a 2MB TLB window.
    pub fn new_2m(fd: RawFd, x: u16, y: u16, addr: u64) -> std::io::Result<Self> {
        Self::new(fd, x, y, addr, TWO_MEG, ptr::null_mut(), false)
    }

    /// Create a 4GB TLB window with MAP_FIXED at the given base address.
    pub fn new_4g(
        fd: RawFd,
        x: u16,
        y: u16,
        addr: u64,
        base: *mut u8,
        use_wc: bool,
    ) -> std::io::Result<Self> {
        Self::new(fd, x, y, addr, FOUR_GIG, base, use_wc)
    }

    /// Write a 32-bit value at the given offset within the window. Uses volatile write.
    pub fn write32(&self, addr: u64, value: u32) {
        let off = self.offset.checked_add(addr as usize)
            .expect("TLB window offset overflow");
        assert!(off + 4 <= self.window_size, "TLB write32 out of bounds");
        assert!(off.is_multiple_of(4), "TLB write32 unaligned");
        unsafe {
            ptr::write_volatile(self.window.data().add(off) as *mut u32, value);
        }
    }

    /// Read a 32-bit value at the given offset within the window. Uses volatile read.
    pub fn read32(&self, addr: u64) -> u32 {
        let off = self.offset.checked_add(addr as usize)
            .expect("TLB window offset overflow");
        assert!(off + 4 <= self.window_size, "TLB read32 out of bounds");
        assert!(off.is_multiple_of(4), "TLB read32 unaligned");
        unsafe { ptr::read_volatile(self.window.data().add(off) as *const u32) }
    }

    /// Get a raw pointer to the start of the data within the window (at offset).
    pub fn get_window(&self) -> *mut u8 {
        unsafe { self.window.data().add(self.offset) }
    }

    pub fn data(&self) -> *mut u8 {
        self.window.data()
    }
}
