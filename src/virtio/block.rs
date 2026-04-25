// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! VirtIO block device implementation.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::l2cpu::L2Cpu;
use crate::virtio::interrupt::InterruptController;
use crate::virtio::{self, VirtioDeviceImpl};

// VirtIO block request types
const VIRTIO_BLK_T_IN: u32 = 0; // read from disk
const VIRTIO_BLK_T_OUT: u32 = 1; // write to disk

// VirtIO IDs
const VIRTIO_ID_BLOCK: u32 = 2;
// VIRTIO_F_VERSION_1 is bit 32 in the combined feature space; in features[1]
// (the high 32 bits) that's bit 0.
const VIRTIO_F_VERSION_1_BIT: u32 = 1;

/// VirtIO block request header (from virtio_blk_outhdr).
#[repr(C)]
struct VirtioBlkOuthdr {
    type_: u32,
    ioprio: u32,
    sector: u64,
}

/// VirtIO block config (capacity at offset 0).
#[repr(C)]
struct VirtioBlkConfig {
    capacity: u64,
}

pub struct VirtioBlk {
    sector_size: usize,
    mapped_data: *mut u8,
    file_size: usize,
    /// Owned file handle for the disk image. Dropped after `mapped_data`
    /// is unmapped (Drop runs munmap first explicitly, then this field
    /// drops at the end of `Drop::drop`, which closes the fd). mmap
    /// holds its own kernel-level reference to the inode so the order
    /// doesn't actually matter, but we mirror the historical
    /// munmap-then-close shape for clarity.
    file: Option<File>,
    req: *const VirtioBlkOuthdr,
    /// Accumulated byte offset within the current I/O request (across data descriptors).
    data_offset: u64,
}

unsafe impl Send for VirtioBlk {}

impl Drop for VirtioBlk {
    fn drop(&mut self) {
        if !self.mapped_data.is_null() {
            unsafe {
                libc::munmap(self.mapped_data as *mut libc::c_void, self.file_size);
            }
        }
        // Explicitly drop the File so the close happens here in the
        // Drop body (after munmap), not at some unspecified point.
        self.file.take();
    }
}

impl VirtioBlk {
    /// Open `image_path` and construct a VirtioBlk against it. Used by
    /// CLI / debug paths that don't go through `dispatch_add_disk`.
    /// The daemon's add-disk path uses `from_file` with a pre-vetted
    /// File handle to avoid the path-resolved-twice TOCTOU.
    #[allow(dead_code)]
    pub fn new(image_path: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(image_path)?;
        Self::from_file(file)
    }

    /// Construct a VirtioBlk from an already-opened File. The File is
    /// owned by the resulting VirtioBlk for its full lifetime; the
    /// caller is freed from any close responsibility. `mmap` derives
    /// the file size via `fstat` on the file's fd.
    pub fn from_file(file: File) -> std::io::Result<Self> {
        let stat = nix::sys::stat::fstat(&file)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        let file_size = stat.st_size as usize;

        let mapped_data = unsafe {
            libc::mmap(
                ptr::null_mut(),
                file_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if mapped_data == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }

        Ok(VirtioBlk {
            sector_size: 512,
            mapped_data: mapped_data as *mut u8,
            file_size,
            file: Some(file),
            req: ptr::null(),
            data_offset: 0,
        })
    }

    pub fn num_sectors(&self) -> u64 {
        self.file_size.div_ceil(self.sector_size) as u64
    }
}

impl VirtioDeviceImpl for VirtioBlk {
    fn num_queues(&self) -> u32 {
        1
    }
    fn queue_header_size(&self) -> u64 {
        std::mem::size_of::<VirtioBlkOuthdr>() as u64
    }
    fn device_id(&self) -> u32 {
        VIRTIO_ID_BLOCK
    }
    fn device_features(&self) -> [u32; 2] {
        [0, VIRTIO_F_VERSION_1_BIT]
    }

    fn process_queue_start(&mut self, _queue_idx: u32, addr: *mut u8, _len: u64) {
        self.req = addr as *const VirtioBlkOuthdr;
        self.data_offset = 0;
    }

    fn process_queue_data(&mut self, _queue_idx: u32, addr: *mut u8, len: u64) {
        let req = unsafe { &*self.req };
        let disk_offset = self.sector_size as u64 * req.sector + self.data_offset;
        let end = disk_offset + len;

        if end > self.file_size as u64 {
            eprintln!(
                "block: I/O at offset {:#x} len {} exceeds disk size {:#x}, ignoring",
                disk_offset, len, self.file_size
            );
            self.data_offset += len;
            return;
        }

        match req.type_ {
            VIRTIO_BLK_T_IN => {
                // Read from disk to guest memory
                unsafe {
                    ptr::copy_nonoverlapping(
                        self.mapped_data.add(disk_offset as usize),
                        addr,
                        len as usize,
                    );
                }
            }
            VIRTIO_BLK_T_OUT => {
                // Write from guest memory to disk
                unsafe {
                    ptr::copy_nonoverlapping(
                        addr,
                        self.mapped_data.add(disk_offset as usize),
                        len as usize,
                    );
                }
            }
            t => {
                eprintln!("Unimplemented block request type: {} len: {}", t, len);
            }
        }
        self.data_offset += len;
    }

    fn process_queue_complete(&mut self, _queue_idx: u32, addr: *mut u8, _len: u64) {
        // Write status byte 0 = success
        unsafe {
            ptr::write_volatile(addr, 0u8);
        }
    }

    fn queue_has_data(&self, _queue_idx: u32) -> bool {
        true
    }

    fn init_config(&self, config: *mut u8) {
        let cfg = config as *mut VirtioBlkConfig;
        unsafe {
            ptr::write_volatile(&mut (*cfg).capacity, self.num_sectors());
        }
    }
}

/// Block device thread main function.
///
/// `disk_image` is the operator-vetted File handle (opened during
/// `dispatch_add_disk`). Holding it for the whole worker lifetime
/// closes the path-resolved-twice TOCTOU window: the inner loop
/// re-creates VirtioBlk on each iteration via `try_clone`, so a
/// symlink swap between iterations can't redirect the daemon at a
/// different inode. `disk_image_path` is kept for log lines only.
pub fn disk_main(
    l2cpu: Arc<L2Cpu>,
    interrupt_ctl: Arc<InterruptController>,
    interrupt_number: u32,
    mmio_region_offset: u64,
    disk_image_path: String,
    disk_image: File,
    exit_flag: Arc<AtomicBool>,
) {
    crate::dlog!(
        "[disk l2cpu {}] worker thread entered (image={}, mmio_offset=0x{:x}, irq={})",
        l2cpu.idx(),
        disk_image_path,
        mmio_region_offset,
        interrupt_number
    );
    while !exit_flag.load(Ordering::Relaxed) {
        // Hand a fresh fd-clone to each VirtioBlk so its Drop's munmap
        // + close is independent of the master `disk_image` File. The
        // dup is cheap (no actual open() syscall, no path resolution).
        let cloned = match disk_image.try_clone() {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "disk: failed to dup fd for image {}: {}",
                    disk_image_path, e
                );
                return;
            }
        };
        let mut blk = match VirtioBlk::from_file(cloned) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("disk: failed to open image {}: {}", disk_image_path, e);
                return;
            }
        };

        // Capacity is written by VirtioBlk::init_config inside run_device,
        // after the cold-start memset. Writing it here would be clobbered.

        virtio::run_device(
            &mut blk,
            &l2cpu,
            &interrupt_ctl,
            interrupt_number,
            mmio_region_offset,
            &exit_flag,
        );

        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
