// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Host-side DMA-coherent buffer for the chip to access via PCIe outbound iATU.
//!
//! Wraps `tt-kmd`'s `IOCTL_ALLOCATE_DMA_BUF` with the `NOC_DMA` flag set, which
//! both `dma_alloc_coherent`s a buffer on the host and programs an outbound
//! iATU region so the chip can reach it at the returned NoC address. This is
//! the host-side anchor for the virtio-mmio control-plane relocation in #64;
//! the chip-side bridge is in [`crate::x280_tlb`].
//!
//! Lifetime: `tt-kmd` does not currently support `IOCTL_FREE_DMA_BUF`
//! (returns `EINVAL` — see `memory.c` in the driver). The buffer is released
//! only when the underlying fd closes. So a `HostDmaBuf` keeps a reference to
//! that fd, but the actual cleanup happens when the L2CPU fd is dropped. We
//! still `munmap` here on drop to release our user-side mapping.

use std::os::fd::RawFd;
use std::ptr;

use crate::kmd;

/// A DMA-coherent host buffer mapped into the chip's NoC address space.
///
/// Daemon side: `as_ptr()` returns the host VA; reads/writes are native
/// memory accesses (no PCIe round-trip).
///
/// Chip side: the buffer lives at `noc_address` on the PCIe outbound tile
/// (the in-use one — `(19, 24)` on Blackhole p150, per the translated NoC
/// coordinates). Reaching it from an L2CPU also requires programming an
/// x280 TLB window (see `crate::x280_tlb`); reaching it from a Tensix /
/// ERISC core or from another iATU consumer just requires the NoC address.
pub struct HostDmaBuf {
    fd: RawFd,
    ptr: *mut u8,
    size: u32,
    pub bus_address: u64,
    pub noc_address: u64,
    pub mapping_offset: u64,
    pub buf_index: u8,
}

// Send so callers can hand a buffer (or a stable pointer to its contents)
// across thread boundaries — needed by virtio workers running on their own
// threads. The kernel-mode buffer itself is mapped MAP_SHARED, accessed
// through volatile reads/writes, and synchronised externally; passing the
// owning struct between threads is just moving an fd + an mmap pointer.
unsafe impl Send for HostDmaBuf {}

/// Per-L2CPU virtio MMIO control plane: one 16 KiB DMA-coherent buffer
/// reachable through one outbound iATU region + one x280 small TLB
/// window, divided into four 4 KiB sub-regions (one per device).
///
/// Sharing the iATU + TLB across all four devices on the L2CPU keeps
/// the iATU outbound budget at 4 regions for the whole chip (one per
/// L2CPU), instead of 16 — Blackhole only has 16 outbound iATU slots
/// total, so the per-device allocation was sitting right at the limit
/// and would have failed the moment any other tt-kmd consumer (the
/// userspace Tenstorrent stack, peer-bar mappings, …) needed an
/// outbound region.
pub struct VirtioHostMmio {
    /// Full 16 KiB backing. Drop order: this buf is held in
    /// `L2CpuSlot.virtio_host_mmio`; slot shutdown joins workers
    /// before drop.
    pub buf: HostDmaBuf,
    /// x280 PA at which the kernel reaches sub-region `n` (n=0..4).
    /// `device_x280_pa[RNG_INDEX] = …` etc.
    pub device_x280_pa: [u64; 4],
}

/// Indices into `VirtioHostMmio.device_x280_pa`. Stable across the
/// codebase so `slot.virtio_host_mmio.device_x280_pa[crate::host_buf::RNG_INDEX]`
/// always means "the rng device's x280 PA".
pub const RNG_INDEX: usize = 0;
pub const NET_INDEX: usize = 1;
pub const DISK_INDEX: usize = 2;
pub const CONSOLE_INDEX: usize = 3;

/// Per-device sub-region size within `VirtioHostMmio.buf`. virtio-mmio
/// uses [0..0x200) for standard registers; we round up to a full 4 KiB
/// page so each sub-region starts page-aligned (lets a device-side
/// ioremap of the sub-region work cleanly without surprises).
pub const SUB_REGION_SIZE: u32 = 4096;
pub const SUB_REGION_COUNT: u32 = 4;

impl VirtioHostMmio {
    /// Daemon-side VA for sub-region `index`. Stable for the lifetime
    /// of this struct.
    pub fn device_va(&self, index: usize) -> *mut u8 {
        debug_assert!(index < SUB_REGION_COUNT as usize);
        unsafe { self.buf.as_ptr().add(index * SUB_REGION_SIZE as usize) }
    }
}

impl HostDmaBuf {
    /// Allocate a DMA-coherent buffer of `size` bytes (rounded up to
    /// `PAGE_SIZE`), program an outbound iATU region for it, and mmap it
    /// into the daemon's address space.
    ///
    /// `buf_index` is the kmd-side buffer index for this fd; must be unique
    /// per fd in `[0, 256)`. Conflict yields `EINVAL`.
    pub fn allocate(fd: RawFd, size: u32, buf_index: u8) -> std::io::Result<Self> {
        let page = 4096u32;
        let size = size.div_ceil(page) * page;

        let mut req = kmd::AllocateDmaBuf {
            input: kmd::AllocateDmaBufIn {
                requested_size: size,
                buf_index,
                flags: kmd::TENSTORRENT_ALLOCATE_DMA_BUF_NOC_DMA,
                _reserved0: [0; 2],
                _reserved1: [0; 2],
            },
            output: kmd::AllocateDmaBufOut::default(),
        };
        unsafe {
            kmd::ioctl_allocate_dma_buf(fd, &mut req)
                .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        }
        let out = req.output;

        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                out.mapping_offset as i64,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }

        Ok(HostDmaBuf {
            fd,
            ptr: ptr as *mut u8,
            size: out.size,
            bus_address: out.physical_address,
            noc_address: out.noc_address,
            mapping_offset: out.mapping_offset,
            buf_index,
        })
    }

    /// Host-side pointer for native CPU access. Stable for the lifetime of
    /// this `HostDmaBuf`.
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Buffer size in bytes (rounded up to PAGE_SIZE).
    pub fn size(&self) -> u32 {
        self.size
    }
}

impl Drop for HostDmaBuf {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.size as usize);
        }
        // `IOCTL_FREE_DMA_BUF` is not implemented by tt-kmd. The buffer +
        // iATU region are released when the underlying fd closes, which the
        // L2Cpu owner handles in its own Drop.
        let _ = self.fd; // silence unused warning if logging removed
    }
}
