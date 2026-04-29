// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! VirtIO block device implementation.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr;

use crate::virtio::VirtioDeviceImpl;

// VirtIO block request types (virtio 1.2 §5.2.6).
const VIRTIO_BLK_T_IN: u32 = 0; // read from disk
const VIRTIO_BLK_T_OUT: u32 = 1; // write to disk
                                 // GET_ID returns a 20-byte device serial. AlmaLinux's kernel issues
                                 // this once at probe time; without a real reply it stalls before
                                 // mounting the rootfs.
const VIRTIO_BLK_T_GET_ID: u32 = 8;
const VIRTIO_BLK_ID_BYTES: usize = 20;

// VirtIO block status bytes (virtio 1.2 §5.2.6). Written into the last
// descriptor of the chain to tell the guest whether its request
// succeeded.
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

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
    /// Pending status byte for the in-flight request. Starts at S_OK
    /// in `process_queue_start`; gets set to S_IOERR on out-of-bounds
    /// or to S_UNSUPP on an unrecognized request type. Written into
    /// the final descriptor by `process_queue_complete`. Without this
    /// field, an overflow request silently returned S_OK and the
    /// guest's blockdev layer never saw EIO — the request just hung
    /// (virtio 1.2 §5.2.6).
    req_status: u8,
    /// L2CPU index this device serves. Stored only for metric labels;
    /// not used in the I/O path itself.
    l2cpu_idx: u8,
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
    pub fn new(image_path: &Path, l2cpu_idx: u8) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(image_path)?;
        Self::from_file(file, l2cpu_idx)
    }

    /// Construct a VirtioBlk from an already-opened File. The File is
    /// owned by the resulting VirtioBlk for its full lifetime; the
    /// caller is freed from any close responsibility. `mmap` derives
    /// the file size via `fstat` on the file's fd.
    pub fn from_file(file: File, l2cpu_idx: u8) -> std::io::Result<Self> {
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
            req_status: VIRTIO_BLK_S_OK,
            l2cpu_idx,
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
        // Reset the per-request status — `process_queue_data` may set
        // it to IOERR/UNSUPP; `process_queue_complete` writes whatever
        // we end with into the final descriptor.
        self.req_status = VIRTIO_BLK_S_OK;
    }

    fn process_queue_data(&mut self, _queue_idx: u32, addr: *mut u8, len: u64) {
        let req = unsafe { &*self.req };
        let disk_offset = self.sector_size as u64 * req.sector + self.data_offset;
        let end = disk_offset.saturating_add(len);

        if end > self.file_size as u64 {
            eprintln!(
                "block: I/O at offset {:#x} len {} exceeds disk size {:#x}, returning IOERR",
                disk_offset, len, self.file_size
            );
            // Signal a device I/O error to the guest. Without this the
            // request looked successful from the guest's POV and its
            // blockdev layer hung waiting for data that never arrived.
            self.req_status = VIRTIO_BLK_S_IOERR;
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
            VIRTIO_BLK_T_GET_ID => {
                // Spec: write a 20-byte device serial into the chain's
                // data descriptor. AlmaLinux issues this at probe; if
                // we return UNSUPP the kernel stalls before mount_root.
                // Buildroot ignores the failure, which is why we got
                // away with leaving it unimplemented.
                let serial = format!("bhx-l2cpu-{:02}", self.l2cpu_idx);
                let bytes = serial.as_bytes();
                let n = (len as usize).min(VIRTIO_BLK_ID_BYTES);
                unsafe {
                    ptr::write_bytes(addr, 0, n);
                    let copy_len = bytes.len().min(n);
                    ptr::copy_nonoverlapping(bytes.as_ptr(), addr, copy_len);
                }
            }
            t => {
                eprintln!("Unimplemented block request type: {} len: {}", t, len);
                // Tell the guest "we don't know what this is" so its
                // block layer surfaces an error instead of hanging.
                self.req_status = VIRTIO_BLK_S_UNSUPP;
            }
        }
        self.data_offset += len;
    }

    fn process_queue_complete(&mut self, _queue_idx: u32, addr: *mut u8, len: u64) -> u64 {
        // Emit the status byte set by `process_queue_data`. S_OK if no
        // overflow / unsupported-type was observed; S_IOERR on
        // overflow; S_UNSUPP on an unknown request type.
        unsafe {
            ptr::write_volatile(addr, self.req_status);
        }

        // One bump per request, regardless of how many data
        // descriptors made it up. `data_offset` was accumulated by
        // `process_queue_data` across each chunk.
        let req = unsafe { &*self.req };
        let idx = self.l2cpu_idx;
        match req.type_ {
            VIRTIO_BLK_T_IN => {
                crate::daemon::metrics::BLK_REQUESTS_TOTAL.read(idx).inc();
                crate::daemon::metrics::BLK_BYTES_TOTAL
                    .read(idx)
                    .add(self.data_offset);
            }
            VIRTIO_BLK_T_OUT => {
                crate::daemon::metrics::BLK_REQUESTS_TOTAL.write(idx).inc();
                crate::daemon::metrics::BLK_BYTES_TOTAL
                    .write(idx)
                    .add(self.data_offset);
            }
            _ => {
                // Unknown type — already flagged in req_status; the
                // error counter below picks it up. Don't pollute
                // read/write totals with it.
            }
        }
        match self.req_status {
            VIRTIO_BLK_S_IOERR => crate::daemon::metrics::BLK_ERRORS_TOTAL.ioerr(idx).inc(),
            VIRTIO_BLK_S_UNSUPP => crate::daemon::metrics::BLK_ERRORS_TOTAL.unsupp(idx).inc(),
            _ => {}
        }

        self.req_status = VIRTIO_BLK_S_OK;
        // Pass the buffer capacity through unchanged — block driver
        // expects the chain-summed length.
        len
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
