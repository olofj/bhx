// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! VirtIO network device implementation using Slirp.

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::l2cpu::L2Cpu;
use crate::slirp_ffi::*;
use crate::virtio::interrupt::InterruptController;
use crate::virtio::{self, VirtioDeviceImpl};

const PACKET_SIZE: usize = 1514;
const VIRTIO_ID_NET: u32 = 1;
// VirtIO spec 5.1.3: feature bits of virtio-net. We advertise only
// VIRTIO_F_VERSION_1 (bit 32 — bit 0 of features[1]) and nothing else.
// In particular we deliberately do NOT advertise VIRTIO_NET_F_CSUM
// (bit 0 of features[0]). That bit means "device handles packets with
// partial checksum" — if we claim it, the guest's networking stack
// flags outbound UDP/TCP with NETIF_F_HW_CSUM and virtio-net sets
// VIRTIO_NET_HDR_F_NEEDS_CSUM in the vnet header, expecting us to
// compute the L4 checksum before forwarding. We don't; we pass the
// frame straight to slirp, which then drops the packet for failing
// checksum validation. ICMP survived the misconfiguration only because
// the kernel computes ICMP checksums itself regardless of device
// offload. By advertising no csum offload, the guest stack does the
// checksum in software and slirp accepts the packets.
const VIRTIO_F_VERSION_1_BIT: u32 = 1 << 0; // bit 0 of features[1]

/// VirtIO net header (virtio_net_hdr_mrg_rxbuf).
#[repr(C)]
#[derive(Default)]
struct VirtioNetHdrMrgRxbuf {
    flags: u8,
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    num_buffers: u16,
}

pub struct VirtioNet {
    slirp: *mut VdeSlirp,
    slirp_fd: i32,
    buffer: [u8; PACKET_SIZE],
    header_processed: bool,
    queue_header_size: u64,
}

unsafe impl Send for VirtioNet {}

impl Drop for VirtioNet {
    fn drop(&mut self) {
        if !self.slirp.is_null() {
            unsafe { vdeslirp_close(self.slirp); }
        }
    }
}

impl VirtioNet {
    pub fn new(ttdevice: u32, l2cpu_idx: usize) -> std::io::Result<Self> {
        let mut cfg: SlirpConfig = unsafe { std::mem::zeroed() };
        unsafe { vdeslirp_init(&mut cfg, VDE_INIT_DEFAULT) };
        let slirp = unsafe { vdeslirp_open(&mut cfg) };
        if slirp.is_null() {
            let err = std::io::Error::last_os_error();
            return Err(std::io::Error::other(format!(
                "vdeslirp_open returned NULL (errno: {}). \
                 Likely causes: (1) file descriptor limit reached — check `ulimit -n`; \
                 (2) thread or socketpair creation blocked by a seccomp/container policy; \
                 (3) libvdeslirp/libslirp ABI mismatch — this build expects libvdeslirp \
                 0.1.x linked against libslirp 4.x (check `pkg-config --modversion vdeslirp libslirp`)",
                err
            )));
        }

        let host = InAddr::from_str("127.0.0.1");
        let guest = InAddr::from_str("10.0.2.15");
        let port = 2222 + l2cpu_idx as i32 + 4 * ttdevice as i32;
        unsafe {
            vdeslirp_add_fwd(slirp, 0, host, port, guest, 22);
        }

        let slirp_fd = unsafe { vdeslirp_fd(slirp) };

        Ok(VirtioNet {
            slirp,
            slirp_fd,
            buffer: [0u8; PACKET_SIZE],
            header_processed: false,
            queue_header_size: std::mem::size_of::<VirtioNetHdrMrgRxbuf>() as u64,
        })
    }
}

impl VirtioDeviceImpl for VirtioNet {
    fn num_queues(&self) -> u32 { 2 }
    fn queue_header_size(&self) -> u64 { self.queue_header_size }
    fn device_id(&self) -> u32 { VIRTIO_ID_NET }
    fn device_features(&self) -> [u32; 2] {
        [0, VIRTIO_F_VERSION_1_BIT]
    }

    fn process_queue_start(&mut self, queue_idx: u32, addr: *mut u8, _len: u64) {
        self.header_processed = true;
        if queue_idx == 0 {
            // RX: fill in net header
            let hdr = addr as *mut VirtioNetHdrMrgRxbuf;
            unsafe {
                ptr::write_volatile(&mut (*hdr).flags, 0);
                ptr::write_volatile(&mut (*hdr).num_buffers, 1);
                ptr::write_volatile(&mut (*hdr).gso_type, 0);
                ptr::write_volatile(&mut (*hdr).gso_size, 0);
            }
        }
    }

    fn process_queue_data(&mut self, _queue_idx: u32, _addr: *mut u8, _len: u64) {
        // Nothing to do here
    }

    fn process_queue_complete(&mut self, queue_idx: u32, addr: *mut u8, len: u64) {
        // Handle single-descriptor edge case: if header wasn't processed via
        // a separate descriptor, process it from the start of this one and
        // advance past it.
        let mut data_addr = addr;
        let mut data_len = len;
        if !self.header_processed {
            self.process_queue_start(queue_idx, addr, len);
            data_addr = unsafe { addr.add(self.queue_header_size as usize) };
            data_len = len.saturating_sub(self.queue_header_size);
        }

        if queue_idx == 0 {
            // RX: receive packet from slirp
            let max_copy = (data_len as usize).min(PACKET_SIZE);
            let pktlen = unsafe {
                vdeslirp_recv(self.slirp, self.buffer.as_mut_ptr(), max_copy)
            };
            if pktlen > 0 {
                let copy_len = (pktlen as usize).min(max_copy);
                unsafe {
                    ptr::copy_nonoverlapping(
                        self.buffer.as_ptr(),
                        data_addr,
                        copy_len,
                    );
                }
            }
        } else if queue_idx == 1 {
            // TX: send packet to slirp
            let copy_len = (data_len as usize).min(PACKET_SIZE);
            unsafe {
                ptr::copy_nonoverlapping(data_addr, self.buffer.as_mut_ptr(), copy_len);
                let ret = vdeslirp_send(self.slirp, self.buffer.as_ptr(), copy_len);
                if ret < 0 {
                    eprintln!("vdeslirp_send failed: {}", ret);
                }
            }
        }
        self.header_processed = false;
    }

    fn queue_has_data(&self, queue_idx: u32) -> bool {
        if queue_idx == 0 {
            // RX: check if slirp has data via select with zero timeout
            let mut rfds = unsafe { std::mem::zeroed::<libc::fd_set>() };
            unsafe { libc::FD_SET(self.slirp_fd, &mut rfds); }
            let mut tv = libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            };
            let ret = unsafe {
                libc::select(
                    self.slirp_fd + 1,
                    &mut rfds,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut tv,
                )
            };
            ret > 0
        } else {
            true
        }
    }
}

/// Network device thread main function.
pub fn network_main(
    ttdevice: u32,
    l2cpu: Arc<L2Cpu>,
    interrupt_ctl: Arc<InterruptController>,
    interrupt_number: u32,
    mmio_region_offset: u64,
    exit_flag: Arc<AtomicBool>,
) {
    let l2cpu_idx = l2cpu.idx();
    crate::dlog!(
        "[net l2cpu {}] worker thread entered (mmio_offset=0x{:x}, irq={})",
        l2cpu_idx,
        mmio_region_offset,
        interrupt_number
    );
    while !exit_flag.load(Ordering::Relaxed) {
        let mut net = match VirtioNet::new(ttdevice, l2cpu_idx) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("network: failed to initialize slirp user-mode networking: {}", e);
                return;
            }
        };

        virtio::run_device(
            &mut net,
            &l2cpu,
            &interrupt_ctl,
            interrupt_number,
            mmio_region_offset,
            &exit_flag,
        );

        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
