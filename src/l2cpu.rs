// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! L2CPU tile abstraction — manages TLB windows to X280 RISC-V CPU memory.

use std::mem::ManuallyDrop;
use std::os::unix::io::RawFd;
use std::ptr;

use crate::clock::{self, TlbPllAccess};
use crate::kmd;
use crate::tlb::TlbWindow;

#[derive(Debug, Clone, Copy)]
pub struct Xy {
    pub x: u16,
    pub y: u16,
}

/// Static L2CPU tile NOC coordinates.
pub const L2CPU_TILES: [Xy; 4] = [
    Xy { x: 8, y: 3 },
    Xy { x: 8, y: 9 },
    Xy { x: 8, y: 5 },
    Xy { x: 8, y: 7 },
];

/// Starting DRAM address for each L2CPU.
pub const L2CPU_STARTING_ADDRESS: [u64; 4] = [
    0x4000_3000_0000,
    0x4000_3000_0000,
    0x4000_3000_0000,
    0x4000_b000_0000,
];

/// Memory size available to each L2CPU.
pub const L2CPU_MEMORY_SIZE: [u64; 4] = [
    0x1_0000_0000, // 4GB
    0x1_0000_0000, // 4GB
    0x8000_0000,   // 2GB
    0x8000_0000,   // 2GB
];

/// GDDR enable bit mapping for telemetry check.
pub const L2CPU_GDDR_ENABLE_BIT: [u32; 4] = [5, 6, 7, 7];

pub struct L2Cpu {
    fd: RawFd,
    idx: usize,
    starting_address: u64,
    memory_size: u64,
    coordinates: Xy,
    /// Base of the 8GB reserved VA region.
    memory: *mut u8,
    /// First 4GB TLB window (0x4000_0000_0000).
    /// ManuallyDrop so we control drop order in Drop::drop.
    _first: ManuallyDrop<TlbWindow>,
    /// Second 4GB TLB window (0x4001_0000_0000).
    /// ManuallyDrop so we control drop order in Drop::drop.
    _second: ManuallyDrop<TlbWindow>,
}

// L2Cpu is Send because the raw pointers are to memory-mapped device regions
// that are not shared across threads without explicit synchronization.
unsafe impl Send for L2Cpu {}

impl L2Cpu {
    pub fn new(idx: usize, card_idx: u32) -> std::io::Result<Self> {
        assert!(idx < 4, "L2CPU index must be 0..3");

        let fd = kmd::open_device(card_idx)?;

        // Set PLL frequency to 1750MHz via TLB windows to NOC (8, 0)
        {
            let window_cntl5 =
                TlbWindow::new_2m(fd, 8, 0, 0x80020500 + 0x14)?;
            let window_cntl1 =
                TlbWindow::new_2m(fd, 8, 0, 0x80020500 + 0x04)?;
            let access = TlbPllAccess {
                window_cntl1: &window_cntl1,
                window_cntl5: &window_cntl5,
            };
            clock::set_frequency(&access, 1750);
        }

        let coordinates = L2CPU_TILES[idx];
        let starting_address = L2CPU_STARTING_ADDRESS[idx];
        let memory_size = L2CPU_MEMORY_SIZE[idx];

        // Reserve 8GB of virtual address space
        let memory = unsafe {
            libc::mmap(
                ptr::null_mut(),
                2usize << 32, // 8GB
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if memory == libc::MAP_FAILED {
            unsafe { libc::close(fd); }
            return Err(std::io::Error::last_os_error());
        }
        let memory = memory as *mut u8;

        // Map first 4GB window at base of reserved VA
        let first = match TlbWindow::new_4g(
            fd,
            coordinates.x,
            coordinates.y,
            0x4000_0000_0000,
            memory,
            true,
        ) {
            Ok(w) => w,
            Err(e) => {
                unsafe {
                    libc::munmap(memory as *mut libc::c_void, 2usize << 32);
                    libc::close(fd);
                }
                return Err(e);
            }
        };

        // Map second 4GB window at base+4GB
        let second = match TlbWindow::new_4g(
            fd,
            coordinates.x,
            coordinates.y,
            0x4001_0000_0000,
            unsafe { memory.add(1usize << 32) },
            true,
        ) {
            Ok(w) => w,
            Err(e) => {
                drop(first);
                unsafe {
                    libc::munmap(memory as *mut libc::c_void, 2usize << 32);
                    libc::close(fd);
                }
                return Err(e);
            }
        };

        Ok(L2Cpu {
            fd,
            idx,
            starting_address,
            memory_size,
            coordinates,
            memory,
            _first: ManuallyDrop::new(first),
            _second: ManuallyDrop::new(second),
        })
    }

    pub fn idx(&self) -> usize {
        self.idx
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    pub fn starting_address(&self) -> u64 {
        self.starting_address
    }

    pub fn memory_size(&self) -> u64 {
        self.memory_size
    }

    pub fn coordinates(&self) -> Xy {
        self.coordinates
    }

    /// Get pointer to the start of L2CPU's usable memory (at starting_address offset).
    pub fn get_memory_ptr(&self) -> *mut u8 {
        unsafe {
            self.memory
                .add((self.starting_address - 0x4000_0000_0000) as usize)
        }
    }

    /// Create a temporary 2M TLB window and write a 32-bit value.
    pub fn write32(&self, addr: u64, value: u32) {
        let window = TlbWindow::new_2m(self.fd, self.coordinates.x, self.coordinates.y, addr)
            .expect("failed to create TLB window for write32");
        window.write32(0, value);
    }

    /// Create a temporary 2M TLB window and read a 32-bit value.
    pub fn read32(&self, addr: u64) -> u32 {
        let window = TlbWindow::new_2m(self.fd, self.coordinates.x, self.coordinates.y, addr)
            .expect("failed to create TLB window for read32");
        window.read32(0)
    }

    /// Create a persistent 2M TLB window at the given address.
    pub fn get_persistent_2m_window(&self, addr: u64) -> std::io::Result<TlbWindow> {
        TlbWindow::new_2m(self.fd, self.coordinates.x, self.coordinates.y, addr)
    }
}

impl Drop for L2Cpu {
    fn drop(&mut self) {
        // Drop order is critical: TLB windows must be freed (ioctl) before closing
        // the fd, because ioctl_free_tlb uses the fd. We use ManuallyDrop to control
        // this explicitly: drop windows first (second before first, matching C++),
        // then close the fd.
        unsafe {
            ManuallyDrop::drop(&mut self._second);
            ManuallyDrop::drop(&mut self._first);
            libc::close(self.fd);
        }
    }
}
