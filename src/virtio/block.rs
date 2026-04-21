// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! VirtIO block device implementation.

use std::os::unix::io::RawFd;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::l2cpu::L2Cpu;
use crate::virtio::interrupt::InterruptController;
use crate::virtio::{self, VirtioDeviceImpl};

// VirtIO block request types
const VIRTIO_BLK_T_IN: u32 = 0;  // read from disk
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
    fd: RawFd,
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
        if self.fd >= 0 {
            unsafe {
                libc::close(self.fd);
            }
        }
    }
}

impl VirtioBlk {
    pub fn new(image_path: &Path) -> std::io::Result<Self> {
        let fd = nix::fcntl::open(
            image_path,
            nix::fcntl::OFlag::O_RDWR,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;

        let stat = nix::sys::stat::fstat(fd)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
        let file_size = stat.st_size as usize;

        let mapped_data = unsafe {
            libc::mmap(
                ptr::null_mut(),
                file_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if mapped_data == libc::MAP_FAILED {
            unsafe { libc::close(fd); }
            return Err(std::io::Error::last_os_error());
        }

        Ok(VirtioBlk {
            sector_size: 512,
            mapped_data: mapped_data as *mut u8,
            file_size,
            fd,
            req: ptr::null(),
            data_offset: 0,
        })
    }

    pub fn num_sectors(&self) -> u64 {
        self.file_size.div_ceil(self.sector_size) as u64
    }
}

impl VirtioDeviceImpl for VirtioBlk {
    fn num_queues(&self) -> u32 { 1 }
    fn queue_header_size(&self) -> u64 { std::mem::size_of::<VirtioBlkOuthdr>() as u64 }
    fn device_id(&self) -> u32 { VIRTIO_ID_BLOCK }
    fn device_features(&self) -> [u32; 2] { [0, VIRTIO_F_VERSION_1_BIT] }

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
        unsafe { ptr::write_volatile(addr, 0u8); }
    }

    fn queue_has_data(&self, _queue_idx: u32) -> bool {
        true
    }
}

/// Block device thread main function.
pub fn disk_main(
    ttdevice: u32,
    l2cpu_idx: usize,
    interrupt_ctl: Arc<InterruptController>,
    interrupt_number: u32,
    mmio_region_offset: u64,
    disk_image_path: String,
    exit_flag: Arc<AtomicBool>,
) {
    while !exit_flag.load(Ordering::Relaxed) {
        let l2cpu = match L2Cpu::new(l2cpu_idx, ttdevice) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("disk: failed to create L2CPU: {}", e);
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        };

        let mut blk = match VirtioBlk::new(Path::new(&disk_image_path)) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("disk: failed to open image {}: {}", disk_image_path, e);
                return;
            }
        };

        // Write capacity to device config
        {
            let address = l2cpu.starting_address() + l2cpu.memory_size() - mmio_region_offset;
            let config_window = l2cpu
                .get_persistent_2m_window(address)
                .expect("failed to create config window");
            let config_ptr =
                unsafe { config_window.get_window().add(0x100) as *mut VirtioBlkConfig };
            unsafe {
                ptr::write_volatile(&mut (*config_ptr).capacity, blk.num_sectors());
            }
        }

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
