// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! VirtIO MMIO device framework — base implementation for device emulation.
//!
//! Module-wide `#![allow(dead_code)]` — virtio-mmio register offsets
//! and feature-bit constants are kept named to mirror the spec even
//! when the current code paths don't reach all of them.
#![allow(dead_code)]

pub mod block;
pub mod console;
pub mod interrupt;
#[cfg(feature = "slirp")]
pub mod network;
pub mod rng;

use std::ptr;

// VirtIO MMIO register offsets
const VIRTIO_MMIO_MAGIC_VALUE: usize = 0x000;
const VIRTIO_MMIO_VERSION: usize = 0x004;
const VIRTIO_MMIO_DEVICE_ID: usize = 0x008;
const VIRTIO_MMIO_DEVICE_FEATURES: usize = 0x010;
const VIRTIO_MMIO_DEVICE_FEATURES_SEL: usize = 0x014;
const VIRTIO_MMIO_DRIVER_FEATURES: usize = 0x020;
const VIRTIO_MMIO_DRIVER_FEATURES_SEL: usize = 0x024;
const VIRTIO_MMIO_QUEUE_SEL: usize = 0x030;
const VIRTIO_MMIO_QUEUE_NUM_MAX: usize = 0x034;
const VIRTIO_MMIO_QUEUE_NUM: usize = 0x038;
const VIRTIO_MMIO_QUEUE_READY: usize = 0x044;
const VIRTIO_MMIO_QUEUE_NOTIFY: usize = 0x050;
const VIRTIO_MMIO_INTERRUPT_STATUS: usize = 0x060;
const VIRTIO_MMIO_INTERRUPT_ACK: usize = 0x064;
const VIRTIO_MMIO_STATUS: usize = 0x070;
const VIRTIO_MMIO_QUEUE_DESC_LOW: usize = 0x080;
const VIRTIO_MMIO_QUEUE_DESC_HIGH: usize = 0x084;
const VIRTIO_MMIO_QUEUE_AVAIL_LOW: usize = 0x090;
const VIRTIO_MMIO_QUEUE_AVAIL_HIGH: usize = 0x094;
const VIRTIO_MMIO_QUEUE_USED_LOW: usize = 0x0a0;
const VIRTIO_MMIO_QUEUE_USED_HIGH: usize = 0x0a4;
const VIRTIO_MMIO_CONFIG: usize = 0x100;

// VirtIO status bits
const VIRTIO_CONFIG_S_DRIVER: u32 = 2;
const VIRTIO_CONFIG_S_FEATURES_OK: u32 = 8;
const VIRTIO_CONFIG_S_DRIVER_OK: u32 = 4;

// VirtIO ring descriptor flags
pub(crate) const VRING_DESC_F_NEXT: u16 = 1;

// VirtIO magic value
const VIRTIO_MAGIC: u32 = 0x74726976; // 'v' | 'i'<<8 | 'r'<<16 | 't'<<24

/// VirtIO ring descriptor.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct VringDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

/// VirtIO available ring.
#[repr(C)]
pub(crate) struct VringAvail {
    pub flags: u16,
    pub idx: u16,
    pub ring: [u16; 0], // flexible array
}

/// VirtIO used ring element.
#[repr(C)]
#[derive(Default)]
pub(crate) struct VringUsedElem {
    pub id: u32,
    pub len: u32,
}

/// VirtIO used ring.
#[repr(C)]
pub(crate) struct VringUsed {
    pub flags: u16,
    pub idx: u16,
    pub ring: [VringUsedElem; 0], // flexible array
}

/// Discriminator for the per-device interrupt counters in
/// `crate::daemon::metrics`. The `run_device` accept loop is generic
/// over device kind, but each interrupt belongs to exactly one of
/// these two metric families — the caller picks at spawn time.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InterruptKind {
    Block,
    Net,
    Console,
    Rng,
}

/// Bump the per-kind interrupt counter at index `idx`. Pulled out of
/// `run_device` so the kind-mapping logic is unit-testable without
/// having to drive the full chip-memory loop.
pub(crate) fn bump_interrupt_metric(kind: InterruptKind, idx: u8) {
    match kind {
        InterruptKind::Block => crate::daemon::metrics::BLK_INTERRUPTS_TOTAL.at(idx).inc(),
        InterruptKind::Net => crate::daemon::metrics::NET_INTERRUPTS_TOTAL.at(idx).inc(),
        InterruptKind::Console => crate::daemon::metrics::CONSOLE_INTERRUPTS_TOTAL
            .at(idx)
            .inc(),
        InterruptKind::Rng => crate::daemon::metrics::RNG_INTERRUPTS_TOTAL.at(idx).inc(),
    }
}

/// Trait that VirtIO device implementations must provide.
pub trait VirtioDeviceImpl {
    fn num_queues(&self) -> u32;
    fn queue_header_size(&self) -> u64;
    fn device_id(&self) -> u32;
    fn device_features(&self) -> [u32; 2];
    fn process_queue_start(&mut self, queue_idx: u32, addr: *mut u8, len: u64);
    fn process_queue_data(&mut self, queue_idx: u32, addr: *mut u8, len: u64);
    /// Process the LAST descriptor of a chain. Returns the bytes the
    /// device wrote into this descriptor's buffer; the runner uses this
    /// for the used-ring `len` field (combined with chain-summed lens
    /// for the earlier descriptors). For block/net the chain-summed
    /// shape is what existing kernels expect, so they return `len`
    /// unchanged. virtio-console RX writes less than the buffer
    /// capacity when input is short, so it returns the real count.
    fn process_queue_complete(&mut self, queue_idx: u32, addr: *mut u8, len: u64) -> u64;
    fn queue_has_data(&self, queue_idx: u32) -> bool;
    /// Populate device-specific config at MMIO offset 0x100. Called once
    /// during cold-start, after the framework has zeroed the standard
    /// register window (0x00..0x200). Must happen *after* the zero; writing
    /// config before `run_device` would be wiped out.
    fn init_config(&self, _config: *mut u8) {}
}

pub(crate) const QUEUE_SIZE: u16 = 16384;

/// Process at most one descriptor chain on `queue_idx`'s avail
/// ring. Mirrors the per-queue body of `run_device`'s main loop —
/// extracted so the M5.5b kick-driven path
/// (`crate::tensix_data_plane`) can share the descriptor-walk
/// logic instead of duplicating ~80 lines.
///
/// Returns `true` if a chain was processed and the caller should
/// fire a PLIC IRQ. Returns `false` if the queue had no new entry,
/// the device declined to handle it (`queue_has_data` returned
/// `false`), or the chain was malformed (descriptor address out of
/// range, cycle detected). On a malformed chain, `processed` is
/// still advanced so we don't get stuck retrying it.
///
/// # Safety
/// Caller must ensure `desc_q`, `avail_q`, `used_q` point to valid
/// VringDesc/Avail/Used arrays of `QUEUE_SIZE` elements that the
/// guest has set up via virtio queue config. `memory` must be the
/// guest's L2CPU memory mmap base; `[starting_address, mem_end)`
/// must enclose every valid descriptor addr.
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_one_chain_for_queue(
    desc_q: *mut VringDesc,
    avail_q: *mut VringAvail,
    used_q: *mut VringUsed,
    processed: &mut u16,
    device: &mut dyn VirtioDeviceImpl,
    queue_idx: u32,
    queue_header_size: u64,
    queue_num: u16,
    starting_address: u64,
    mem_end: u64,
    memory: *mut u8,
) -> bool {
    use std::sync::atomic::Ordering;

    std::sync::atomic::fence(Ordering::SeqCst);
    let avail_idx = unsafe { ptr::read_volatile(&(*avail_q).idx) };

    if *processed == avail_idx || !device.queue_has_data(queue_idx) {
        return false;
    }

    // The kernel allocates `queue_num` entries each in desc / avail.ring /
    // used.ring (a power-of-two ≤ QUEUE_NUM_MAX). All ring indexing must
    // wrap by `queue_num`, not the host-side QUEUE_SIZE constant — when
    // they disagreed (engine path advertises 64 while QUEUE_SIZE=16384),
    // we read avail.ring[80] past the kernel's allocation and surfaced
    // garbage `id` values in used entries, tripping the kernel's
    // "id N is not a head!" guard in virtqueue_get_buf_ctx_split.
    let qn = queue_num as usize;
    let desc_idx_first = unsafe {
        let ring_ptr = (*avail_q).ring.as_ptr();
        ptr::read_volatile(ring_ptr.add((*processed as usize) % qn))
    };
    let mut desc_idx = desc_idx_first;

    let mut num_bytes_written: u64 = 0;
    let mut chain_valid = true;
    let mut steps: u16 = 0;

    loop {
        // Cycle detection: a valid chain can visit at most queue_num
        // descriptors.
        if steps >= queue_num {
            eprintln!(
                "virtio: descriptor chain exceeded {} steps, breaking",
                queue_num
            );
            chain_valid = false;
            break;
        }
        steps += 1;

        let d = unsafe { ptr::read_volatile(desc_q.add((desc_idx as usize) % qn)) };

        // Validate descriptor address is within L2CPU memory.
        // Use checked arithmetic to prevent overflow bypassing the check.
        let addr_end = d.addr.checked_add(d.len as u64);
        if d.addr < starting_address
            || d.addr >= mem_end
            || addr_end.is_none()
            || addr_end.unwrap() > mem_end
        {
            eprintln!(
                "virtio: descriptor addr {:#x} len {} outside memory [{:#x}, {:#x}), \
                 skipping chain",
                d.addr, d.len, starting_address, mem_end
            );
            chain_valid = false;
            break;
        }
        let addr = unsafe { memory.add((d.addr - starting_address) as usize) };

        if d.flags & VRING_DESC_F_NEXT != 0 {
            if num_bytes_written < queue_header_size {
                device.process_queue_start(queue_idx, addr, d.len as u64);
            } else {
                device.process_queue_data(queue_idx, addr, d.len as u64);
            }
            num_bytes_written += d.len as u64;
            desc_idx = d.next;
        } else {
            // Last descriptor's actual-bytes-written can be less
            // than its buffer capacity (e.g. virtio-console RX with
            // partial input). Device returns the real count;
            // block/net pass `d.len` through unchanged.
            let actual = device.process_queue_complete(queue_idx, addr, d.len as u64);
            num_bytes_written += actual;
            break;
        }
    }

    // Only update the used ring if the entire chain processed
    // successfully. Posting a partial completion confuses the guest
    // driver.
    let mut posted = false;
    if chain_valid {
        let used_idx = unsafe { ptr::read_volatile(&(*used_q).idx) };
        unsafe {
            let ring_ptr = (*used_q).ring.as_mut_ptr();
            let elem = ring_ptr.add((used_idx as usize) % qn);
            ptr::write_volatile(&mut (*elem).id, desc_idx_first as u32);
            ptr::write_volatile(&mut (*elem).len, num_bytes_written as u32);
        }
        std::sync::atomic::fence(Ordering::SeqCst);
        unsafe {
            ptr::write_volatile(&mut (*used_q).idx, used_idx.wrapping_add(1));
        }
        posted = true;
    }

    *processed = processed.wrapping_add(1);
    posted
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_kind_routes_to_correct_metric() {
        // Both arms use distinct global statics; pick a fresh idx
        // (use idx=2 — other tests touching these counters use 0/1
        // implicitly via run_device hardware paths) and snapshot
        // before/after.
        use crate::daemon::metrics::{BLK_INTERRUPTS_TOTAL, NET_INTERRUPTS_TOTAL};
        let blk_before = BLK_INTERRUPTS_TOTAL.at(2).get();
        let net_before = NET_INTERRUPTS_TOTAL.at(2).get();

        bump_interrupt_metric(InterruptKind::Block, 2);
        bump_interrupt_metric(InterruptKind::Block, 2);
        bump_interrupt_metric(InterruptKind::Net, 2);

        assert_eq!(BLK_INTERRUPTS_TOTAL.at(2).get(), blk_before + 2);
        assert_eq!(NET_INTERRUPTS_TOTAL.at(2).get(), net_before + 1);

        // A different idx must not be affected.
        let blk_other_before = BLK_INTERRUPTS_TOTAL.at(0).get();
        bump_interrupt_metric(InterruptKind::Block, 2);
        assert_eq!(BLK_INTERRUPTS_TOTAL.at(0).get(), blk_other_before);
    }

    // ---- process_one_chain_for_queue ----
    //
    // Builds a self-consistent set of VringDesc / VringAvail / VringUsed
    // structures in a Vec<u8> backing store, hands the pointers + a
    // simulated guest-VA range to the dispatcher, and asserts on the
    // observable outcomes (used-ring entry, *processed advance, device
    // callbacks). The fixture stays opaque to the dispatcher — the
    // function under test is the real production code, the fixture is
    // only the chip-memory boundary it would normally see backed by an
    // L2CPU mmap.

    /// Layout for the descriptor + avail + used rings in a contiguous
    /// host-side buffer. All offsets are byte offsets into the same
    /// Vec<u8>, picked once at construction so pointer arithmetic in
    /// the test is straightforward.
    struct VirtqFixture {
        backing: Vec<u8>,
        queue_num: u16,
        desc_off: usize,
        avail_off: usize,
        used_off: usize,
        data_off: usize,
        starting_address: u64,
    }

    impl VirtqFixture {
        const STARTING_ADDR: u64 = 0x4000_0000_0000;
        const DATA_BYTES: usize = 4096;

        fn new(queue_num: u16) -> Self {
            // Layout, all 8-byte aligned to keep volatile reads happy:
            //   desc_off (queue_num × 16 B)
            //   avail_off (4 B header + queue_num × 2 B ring + 2 B used_event)
            //   used_off  (4 B header + queue_num × 8 B ring + 2 B avail_event)
            //   data_off  (DATA_BYTES of guest data the descriptors point at)
            let qn = queue_num as usize;
            let desc_off = 0;
            let desc_size = qn * std::mem::size_of::<VringDesc>();
            let avail_off = (desc_off + desc_size).next_multiple_of(8);
            let avail_size = 4 + qn * 2 + 2;
            let used_off = (avail_off + avail_size).next_multiple_of(8);
            let used_size = 4 + qn * std::mem::size_of::<VringUsedElem>() + 2;
            let data_off = (used_off + used_size).next_multiple_of(8);
            let total = data_off + Self::DATA_BYTES;
            VirtqFixture {
                backing: vec![0u8; total],
                queue_num,
                desc_off,
                avail_off,
                used_off,
                data_off,
                starting_address: Self::STARTING_ADDR,
            }
        }

        fn desc_q(&mut self) -> *mut VringDesc {
            unsafe { self.backing.as_mut_ptr().add(self.desc_off) as *mut VringDesc }
        }
        fn avail_q(&mut self) -> *mut VringAvail {
            unsafe { self.backing.as_mut_ptr().add(self.avail_off) as *mut VringAvail }
        }
        fn used_q(&mut self) -> *mut VringUsed {
            unsafe { self.backing.as_mut_ptr().add(self.used_off) as *mut VringUsed }
        }
        fn memory_ptr(&mut self) -> *mut u8 {
            self.backing.as_mut_ptr()
        }
        fn mem_end(&self) -> u64 {
            self.starting_address + self.backing.len() as u64
        }

        /// Plant a descriptor at `idx` (modulo queue_num). `data_off_in_data`
        /// points within the data area; converted to a guest-VA addr.
        fn write_desc(&mut self, idx: u16, data_off_in_data: u64, len: u32, flags: u16, next: u16) {
            assert!(
                (data_off_in_data as usize + len as usize) <= Self::DATA_BYTES,
                "descriptor extends past test data area"
            );
            let addr = self.starting_address + self.data_off as u64 + data_off_in_data;
            unsafe {
                ptr::write_volatile(
                    self.desc_q().add((idx as usize) % self.queue_num as usize),
                    VringDesc {
                        addr,
                        len,
                        flags,
                        next,
                    },
                );
            }
        }

        /// Plant a descriptor with an arbitrary addr — used to drive the
        /// out-of-range failure path.
        fn write_desc_raw_addr(&mut self, idx: u16, addr: u64, len: u32, flags: u16, next: u16) {
            unsafe {
                ptr::write_volatile(
                    self.desc_q().add((idx as usize) % self.queue_num as usize),
                    VringDesc {
                        addr,
                        len,
                        flags,
                        next,
                    },
                );
            }
        }

        /// Push `desc_idx` into avail.ring[avail_slot] and bump avail.idx.
        fn submit_avail(&mut self, avail_slot: u16, desc_idx: u16) {
            let qn = self.queue_num as usize;
            unsafe {
                let avail = self.avail_q();
                let ring = (*avail).ring.as_mut_ptr();
                ptr::write_volatile(ring.add((avail_slot as usize) % qn), desc_idx);
                let new_idx = ptr::read_volatile(&(*avail).idx).wrapping_add(1);
                ptr::write_volatile(&mut (*avail).idx, new_idx);
            }
        }

        fn used_idx(&mut self) -> u16 {
            unsafe { ptr::read_volatile(&(*self.used_q()).idx) }
        }

        fn used_elem(&mut self, slot: u16) -> VringUsedElem {
            let qn = self.queue_num as usize;
            unsafe {
                let used = self.used_q();
                let ring = (*used).ring.as_ptr();
                let elem = ring.add((slot as usize) % qn);
                VringUsedElem {
                    id: ptr::read_volatile(&(*elem).id),
                    len: ptr::read_volatile(&(*elem).len),
                }
            }
        }
    }

    /// `VirtioDeviceImpl` stub that records every callback for later
    /// inspection. Reports `queue_has_data = true` so the dispatcher
    /// proceeds; tests can flip `has_data` to drive the no-data
    /// short-circuit path.
    struct RecordingDevice {
        starts: Vec<(u32, u64)>,
        datas: Vec<(u32, u64)>,
        completes: Vec<(u32, u64)>,
        has_data: bool,
        complete_returns: u64,
    }

    impl RecordingDevice {
        fn new() -> Self {
            RecordingDevice {
                starts: Vec::new(),
                datas: Vec::new(),
                completes: Vec::new(),
                has_data: true,
                complete_returns: u64::MAX, // sentinel: pass-through input len
            }
        }
    }

    impl VirtioDeviceImpl for RecordingDevice {
        fn num_queues(&self) -> u32 {
            1
        }
        fn queue_header_size(&self) -> u64 {
            8
        }
        fn device_id(&self) -> u32 {
            0
        }
        fn device_features(&self) -> [u32; 2] {
            [0, 0]
        }
        fn process_queue_start(&mut self, q: u32, _addr: *mut u8, len: u64) {
            self.starts.push((q, len));
        }
        fn process_queue_data(&mut self, q: u32, _addr: *mut u8, len: u64) {
            self.datas.push((q, len));
        }
        fn process_queue_complete(&mut self, q: u32, _addr: *mut u8, len: u64) -> u64 {
            self.completes.push((q, len));
            if self.complete_returns == u64::MAX {
                len
            } else {
                self.complete_returns
            }
        }
        fn queue_has_data(&self, _q: u32) -> bool {
            self.has_data
        }
    }

    #[test]
    fn dispatch_single_segment_chain_posts_used_entry() {
        let mut fx = VirtqFixture::new(8);
        // One descriptor, no NEXT — process_queue_complete only.
        fx.write_desc(0, 0, 64, 0, 0);
        fx.submit_avail(0, 0);

        let mut dev = RecordingDevice::new();
        let mut processed: u16 = 0;

        let starting = fx.starting_address;
        let mem_end = fx.mem_end();
        let memory = fx.memory_ptr();
        let queue_num = fx.queue_num;
        let posted = process_one_chain_for_queue(
            fx.desc_q(),
            fx.avail_q(),
            fx.used_q(),
            &mut processed,
            &mut dev,
            0,
            8,
            queue_num,
            starting,
            mem_end,
            memory,
        );

        assert!(posted, "single-segment chain should post a used entry");
        assert_eq!(dev.starts.len(), 0);
        assert_eq!(dev.datas.len(), 0);
        assert_eq!(dev.completes, vec![(0u32, 64u64)]);
        assert_eq!(processed, 1);
        assert_eq!(fx.used_idx(), 1);
        let ue = fx.used_elem(0);
        assert_eq!(ue.id, 0);
        assert_eq!(ue.len, 64);
    }

    #[test]
    fn dispatch_two_descriptor_chain_calls_start_and_complete() {
        // First desc has NEXT and len < queue_header_size (8) → start.
        // Second desc has no NEXT → complete. No data segments here.
        let mut fx = VirtqFixture::new(8);
        fx.write_desc(0, 0, 8, VRING_DESC_F_NEXT, 1);
        fx.write_desc(1, 8, 16, 0, 0);
        fx.submit_avail(0, 0);

        let mut dev = RecordingDevice::new();
        let mut processed: u16 = 0;

        let starting = fx.starting_address;
        let mem_end = fx.mem_end();
        let memory = fx.memory_ptr();
        let queue_num = fx.queue_num;
        let posted = process_one_chain_for_queue(
            fx.desc_q(),
            fx.avail_q(),
            fx.used_q(),
            &mut processed,
            &mut dev,
            0,
            8,
            queue_num,
            starting,
            mem_end,
            memory,
        );

        assert!(posted);
        assert_eq!(dev.starts, vec![(0u32, 8u64)]);
        assert_eq!(dev.completes, vec![(0u32, 16u64)]);
        // No data segments because the chain is exactly 2 descriptors.
        assert_eq!(dev.datas.len(), 0);
        // Used-ring entry's len is the sum of all chain segments.
        let ue = fx.used_elem(0);
        assert_eq!(ue.id, 0, "used entry id should be the chain head");
        assert_eq!(ue.len, 24);
    }

    #[test]
    fn dispatch_three_descriptor_chain_calls_data_in_middle() {
        // start -> data -> complete. Header size 8, so first 8-byte
        // descriptor is start; the next desc bumps num_bytes_written
        // past the header so the *third* call would be data — but here
        // we want exactly start/data/complete so make the chain longer.
        // Sequence: desc 0 (8B, NEXT) start, desc 1 (16B, NEXT) data,
        // desc 2 (4B, no NEXT) complete.
        let mut fx = VirtqFixture::new(8);
        fx.write_desc(0, 0, 8, VRING_DESC_F_NEXT, 1);
        fx.write_desc(1, 8, 16, VRING_DESC_F_NEXT, 2);
        fx.write_desc(2, 24, 4, 0, 0);
        fx.submit_avail(0, 0);

        let mut dev = RecordingDevice::new();
        let mut processed: u16 = 0;

        let starting = fx.starting_address;
        let mem_end = fx.mem_end();
        let memory = fx.memory_ptr();
        let queue_num = fx.queue_num;
        process_one_chain_for_queue(
            fx.desc_q(),
            fx.avail_q(),
            fx.used_q(),
            &mut processed,
            &mut dev,
            0,
            8,
            queue_num,
            starting,
            mem_end,
            memory,
        );

        assert_eq!(dev.starts, vec![(0u32, 8u64)]);
        assert_eq!(dev.datas, vec![(0u32, 16u64)]);
        assert_eq!(dev.completes, vec![(0u32, 4u64)]);
        let ue = fx.used_elem(0);
        assert_eq!(ue.len, 28);
    }

    #[test]
    fn dispatch_queue_has_no_data_returns_false_without_advancing() {
        let mut fx = VirtqFixture::new(8);
        fx.write_desc(0, 0, 64, 0, 0);
        fx.submit_avail(0, 0);

        let mut dev = RecordingDevice::new();
        dev.has_data = false; // device declines

        let mut processed: u16 = 0;
        let starting = fx.starting_address;
        let mem_end = fx.mem_end();
        let memory = fx.memory_ptr();
        let queue_num = fx.queue_num;
        let posted = process_one_chain_for_queue(
            fx.desc_q(),
            fx.avail_q(),
            fx.used_q(),
            &mut processed,
            &mut dev,
            0,
            8,
            queue_num,
            starting,
            mem_end,
            memory,
        );

        assert!(!posted);
        assert_eq!(processed, 0, "no advance when device declines");
        assert_eq!(fx.used_idx(), 0, "no used entry");
        assert!(dev.completes.is_empty());
    }

    #[test]
    fn dispatch_out_of_range_descriptor_skips_chain_safely() {
        let mut fx = VirtqFixture::new(8);
        // Plant a descriptor pointing well past mem_end — the bounds
        // check must reject it without invoking any device callback or
        // posting a used entry. The dispatcher still bumps `processed`
        // so subsequent calls don't replay the same broken chain.
        let bogus_addr = fx.mem_end() + 0x1000;
        fx.write_desc_raw_addr(0, bogus_addr, 32, 0, 0);
        fx.submit_avail(0, 0);

        let mut dev = RecordingDevice::new();
        let mut processed: u16 = 0;
        let starting = fx.starting_address;
        let mem_end = fx.mem_end();
        let memory = fx.memory_ptr();
        let queue_num = fx.queue_num;
        let posted = process_one_chain_for_queue(
            fx.desc_q(),
            fx.avail_q(),
            fx.used_q(),
            &mut processed,
            &mut dev,
            0,
            8,
            queue_num,
            starting,
            mem_end,
            memory,
        );

        assert!(!posted, "out-of-range chain must not post");
        assert_eq!(processed, 1, "processed advances so we don't loop");
        assert_eq!(fx.used_idx(), 0, "used ring untouched");
        assert!(
            dev.completes.is_empty(),
            "no device callback for bogus chain"
        );
    }

    #[test]
    fn dispatch_descriptor_cycle_breaks_after_queue_num_steps() {
        let mut fx = VirtqFixture::new(4);
        // Self-cycle: desc 0 → desc 0. The dispatcher's step counter
        // caps walking at queue_num and rejects the chain. No used
        // entry, processed still advances.
        fx.write_desc(0, 0, 8, VRING_DESC_F_NEXT, 0);
        fx.submit_avail(0, 0);

        let mut dev = RecordingDevice::new();
        let mut processed: u16 = 0;
        let starting = fx.starting_address;
        let mem_end = fx.mem_end();
        let memory = fx.memory_ptr();
        let queue_num = fx.queue_num;
        let posted = process_one_chain_for_queue(
            fx.desc_q(),
            fx.avail_q(),
            fx.used_q(),
            &mut processed,
            &mut dev,
            0,
            8,
            queue_num,
            starting,
            mem_end,
            memory,
        );

        assert!(!posted, "cycle must be rejected");
        assert_eq!(processed, 1);
        assert_eq!(fx.used_idx(), 0);
    }
}
