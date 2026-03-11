// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! VirtIO MMIO device framework — base implementation for device emulation.

pub mod block;
pub mod interrupt;
#[cfg(feature = "slirp")]
pub mod network;

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::l2cpu::L2Cpu;
use crate::tlb::TlbWindow;
use interrupt::InterruptController;

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
const VRING_DESC_F_NEXT: u16 = 1;

// VirtIO magic value
const VIRTIO_MAGIC: u32 = 0x74726976; // 'v' | 'i'<<8 | 'r'<<16 | 't'<<24

/// VirtIO ring descriptor.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VringDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

/// VirtIO available ring.
#[repr(C)]
struct VringAvail {
    flags: u16,
    idx: u16,
    ring: [u16; 0], // flexible array
}

/// VirtIO used ring element.
#[repr(C)]
#[derive(Default)]
struct VringUsedElem {
    id: u32,
    len: u32,
}

/// VirtIO used ring.
#[repr(C)]
struct VringUsed {
    flags: u16,
    idx: u16,
    ring: [VringUsedElem; 0], // flexible array
}

/// Trait that VirtIO device implementations must provide.
pub trait VirtioDeviceImpl {
    fn num_queues(&self) -> u32;
    fn queue_header_size(&self) -> u64;
    fn device_id(&self) -> u32;
    fn device_features(&self) -> [u32; 2];
    fn process_queue_start(&mut self, queue_idx: u32, addr: *mut u8, len: u64);
    fn process_queue_data(&mut self, queue_idx: u32, addr: *mut u8, len: u64);
    fn process_queue_complete(&mut self, queue_idx: u32, addr: *mut u8, len: u64);
    fn queue_has_data(&self, queue_idx: u32) -> bool;
}

/// MMIO register pointers — all volatile.
struct MmioRegs {
    magic_value: *mut u32,
    status: *mut u32,
    device_features: *mut u32,
    device_features_sel: *mut u32,
    driver_features_sel: *mut u32,
    queue_num_max: *mut u32,
    queue_ready: *mut u32,
    interrupt_status: *mut u32,
    interrupt_ack: *mut u32,
    queue_select: *mut u32,
    queue_desc_low: *mut u32,
    queue_desc_high: *mut u32,
    queue_avail_low: *mut u32,
    queue_avail_high: *mut u32,
    queue_used_low: *mut u32,
    queue_used_high: *mut u32,
    sel_generation: *mut u32,
}

// MmioRegs contains raw pointers to device-mapped memory regions
unsafe impl Send for MmioRegs {}

impl MmioRegs {
    fn new(base: *mut u8) -> Self {
        unsafe {
            MmioRegs {
                magic_value: base.add(VIRTIO_MMIO_MAGIC_VALUE) as *mut u32,
                status: base.add(VIRTIO_MMIO_STATUS) as *mut u32,
                device_features: base.add(VIRTIO_MMIO_DEVICE_FEATURES) as *mut u32,
                device_features_sel: base.add(VIRTIO_MMIO_DEVICE_FEATURES_SEL) as *mut u32,
                driver_features_sel: base.add(VIRTIO_MMIO_DRIVER_FEATURES_SEL) as *mut u32,
                queue_num_max: base.add(VIRTIO_MMIO_QUEUE_NUM_MAX) as *mut u32,
                queue_ready: base.add(VIRTIO_MMIO_QUEUE_READY) as *mut u32,
                interrupt_status: base.add(VIRTIO_MMIO_INTERRUPT_STATUS) as *mut u32,
                interrupt_ack: base.add(VIRTIO_MMIO_INTERRUPT_ACK) as *mut u32,
                queue_select: base.add(VIRTIO_MMIO_QUEUE_SEL) as *mut u32,
                queue_desc_low: base.add(VIRTIO_MMIO_QUEUE_DESC_LOW) as *mut u32,
                queue_desc_high: base.add(VIRTIO_MMIO_QUEUE_DESC_HIGH) as *mut u32,
                queue_avail_low: base.add(VIRTIO_MMIO_QUEUE_AVAIL_LOW) as *mut u32,
                queue_avail_high: base.add(VIRTIO_MMIO_QUEUE_AVAIL_HIGH) as *mut u32,
                queue_used_low: base.add(VIRTIO_MMIO_QUEUE_USED_LOW) as *mut u32,
                queue_used_high: base.add(VIRTIO_MMIO_QUEUE_USED_HIGH) as *mut u32,
                sel_generation: base.add(0x01c) as *mut u32,
            }
        }
    }
}

const QUEUE_SIZE: u16 = 16384;

/// Run a VirtIO device: setup MMIO, negotiate features, process descriptors.
pub fn run_device(
    device: &mut dyn VirtioDeviceImpl,
    l2cpu: &L2Cpu,
    interrupt_ctl: &InterruptController,
    interrupt_number: u32,
    mmio_region_offset: u64,
    exit_flag: &AtomicBool,
) {
    let starting_address = l2cpu.starting_address();
    let memory = l2cpu.get_memory_ptr();

    // Create MMIO window
    let address = starting_address + l2cpu.memory_size() - mmio_region_offset;
    let window = l2cpu
        .get_persistent_2m_window(address)
        .expect("failed to create MMIO window");
    let mmio_base = window.get_window();

    // Zero first 0x200 bytes
    unsafe {
        ptr::write_bytes(mmio_base, 0, 0x200);
    }

    let regs = MmioRegs::new(mmio_base);

    // Initialize MMIO registers
    unsafe {
        ptr::write_volatile(regs.magic_value, VIRTIO_MAGIC);
        ptr::write_volatile(mmio_base.add(VIRTIO_MMIO_VERSION) as *mut u32, 2);
        ptr::write_volatile(mmio_base.add(VIRTIO_MMIO_DEVICE_ID) as *mut u32, device.device_id());
        ptr::write_volatile(regs.queue_num_max, QUEUE_SIZE as u32);
        ptr::write_volatile(mmio_base.add(0x018) as *mut u32, 1); // sw_impl
        ptr::write_volatile(regs.sel_generation, 0);
    }

    // Write device-specific config
    let features = device.device_features();

    // Phase 1: Wait for DRIVER status
    while !exit_flag.load(Ordering::Relaxed) {
        if unsafe { ptr::read_volatile(regs.status) } & VIRTIO_CONFIG_S_DRIVER != 0 {
            break;
        }
    }

    // Phase 2: Feature negotiation via sel_generation
    let mut prev_gen: u32 = 0;
    while !exit_flag.load(Ordering::Relaxed) {
        let curr_gen = unsafe { ptr::read_volatile(regs.sel_generation) };
        if curr_gen != prev_gen {
            let sel = unsafe { ptr::read_volatile(regs.device_features_sel) };
            unsafe {
                ptr::write_volatile(regs.device_features, features[sel as usize & 1]);
            }
            unsafe {
                ptr::write_volatile(regs.sel_generation, curr_gen + 1);
            }
            prev_gen = curr_gen + 1;
        }
        if unsafe { ptr::read_volatile(regs.status) } & VIRTIO_CONFIG_S_FEATURES_OK != 0 {
            break;
        }
    }

    let num_queues = device.num_queues();
    let mut descriptor_table_address = vec![0u64; num_queues as usize];
    let mut available_ring_address = vec![0u64; num_queues as usize];
    let mut used_ring_address = vec![0u64; num_queues as usize];

    // Phase 3: Queue address exchange
    let mem_end = starting_address + l2cpu.memory_size();
    while !exit_flag.load(Ordering::Relaxed) {
        let curr_gen = unsafe { ptr::read_volatile(regs.sel_generation) };
        unsafe { ptr::write_volatile(regs.queue_ready, 0); }
        if curr_gen != prev_gen {
            let q = unsafe { ptr::read_volatile(regs.queue_select) } as usize;
            if q >= num_queues as usize {
                // Invalid queue index from guest — skip this generation
                unsafe { ptr::write_volatile(regs.sel_generation, curr_gen + 1); }
                prev_gen = curr_gen + 1;
                unsafe { libc::usleep(1); }
                continue;
            }

            descriptor_table_address[q] = unsafe {
                ((ptr::read_volatile(regs.queue_desc_high) as u64) << 32)
                    | (ptr::read_volatile(regs.queue_desc_low) as u64)
            };
            available_ring_address[q] = unsafe {
                ((ptr::read_volatile(regs.queue_avail_high) as u64) << 32)
                    | (ptr::read_volatile(regs.queue_avail_low) as u64)
            };
            used_ring_address[q] = unsafe {
                ((ptr::read_volatile(regs.queue_used_high) as u64) << 32)
                    | (ptr::read_volatile(regs.queue_used_low) as u64)
            };

            unsafe { ptr::write_volatile(regs.sel_generation, curr_gen + 1); }
            prev_gen = curr_gen + 1;

            if q == (num_queues as usize - 1) {
                break;
            }
        }
        unsafe { libc::usleep(1); }
    }

    // Compute pointers to virtqueue structures in L2CPU memory
    let mut desc_ptrs: Vec<*mut VringDesc> = Vec::new();
    let mut avail_ptrs: Vec<*mut VringAvail> = Vec::new();
    let mut used_ptrs: Vec<*mut VringUsed> = Vec::new();

    // Validate and compute pointers to virtqueue structures in L2CPU memory.
    // Addresses must fall within the L2CPU's memory region.
    let validate_addr = |addr: u64, label: &str, qi: usize| -> usize {
        if addr < starting_address || addr >= mem_end {
            panic!(
                "virtqueue {} address {:#x} for queue {} is outside L2CPU memory [{:#x}, {:#x})",
                label, addr, qi, starting_address, mem_end
            );
        }
        (addr - starting_address) as usize
    };

    for i in 0..num_queues as usize {
        desc_ptrs.push(unsafe {
            memory.add(validate_addr(descriptor_table_address[i], "desc", i)) as *mut VringDesc
        });
        avail_ptrs.push(unsafe {
            memory.add(validate_addr(available_ring_address[i], "avail", i)) as *mut VringAvail
        });
        used_ptrs.push(unsafe {
            memory.add(validate_addr(used_ring_address[i], "used", i)) as *mut VringUsed
        });
    }

    // Wait for DRIVER_OK
    while !exit_flag.load(Ordering::Relaxed) {
        if unsafe { ptr::read_volatile(regs.status) } & VIRTIO_CONFIG_S_DRIVER_OK != 0 {
            break;
        }
    }

    // Main device loop
    let mut processed = vec![0u16; num_queues as usize];
    let queue_header_size = device.queue_header_size();

    while !exit_flag.load(Ordering::Relaxed) {
        // Check magic still valid
        if unsafe { ptr::read_volatile(regs.magic_value) } != VIRTIO_MAGIC {
            return;
        }

        interrupt_ctl.ack_interrupt(regs.interrupt_ack);

        for queue_idx in 0..num_queues {
            let qi = queue_idx as usize;
            let desc_q = desc_ptrs[qi];
            let avail_q = avail_ptrs[qi];
            let used_q = used_ptrs[qi];

            std::sync::atomic::fence(Ordering::SeqCst);

            let avail_idx = unsafe { ptr::read_volatile(&(*avail_q).idx) };
            let mut should_set_interrupt = false;

            if processed[qi] != avail_idx && device.queue_has_data(queue_idx) {
                let desc_idx_first = unsafe {
                    let ring_ptr = (*avail_q).ring.as_ptr();
                    ptr::read_volatile(ring_ptr.add((processed[qi] % QUEUE_SIZE) as usize))
                };
                let mut desc_idx = desc_idx_first;

                let mut num_bytes_written: u64 = 0;
                let mut chain_valid = true;
                let mut steps: u16 = 0;

                loop {
                    // Cycle detection: a valid chain can visit at most QUEUE_SIZE descriptors
                    if steps >= QUEUE_SIZE {
                        eprintln!("virtio: descriptor chain exceeded {} steps, breaking", QUEUE_SIZE);
                        chain_valid = false;
                        break;
                    }
                    steps += 1;

                    let d = unsafe {
                        ptr::read_volatile(desc_q.add((desc_idx % QUEUE_SIZE) as usize))
                    };

                    // Validate descriptor address is within L2CPU memory.
                    // Use checked arithmetic to prevent overflow bypassing the check.
                    let addr_end = (d.addr).checked_add(d.len as u64);
                    if d.addr < starting_address || d.addr >= mem_end
                        || addr_end.is_none()
                        || addr_end.unwrap() > mem_end
                    {
                        eprintln!(
                            "virtio: descriptor addr {:#x} len {} outside memory [{:#x}, {:#x}), skipping chain",
                            d.addr, d.len, starting_address, mem_end
                        );
                        chain_valid = false;
                        break;
                    }
                    let addr = unsafe {
                        memory.add((d.addr - starting_address) as usize)
                    };

                    if d.flags & VRING_DESC_F_NEXT != 0 {
                        if num_bytes_written < queue_header_size {
                            device.process_queue_start(queue_idx, addr, d.len as u64);
                        } else {
                            device.process_queue_data(queue_idx, addr, d.len as u64);
                        }
                        num_bytes_written += d.len as u64;
                        desc_idx = d.next;
                    } else {
                        device.process_queue_complete(queue_idx, addr, d.len as u64);
                        num_bytes_written += d.len as u64;
                        break;
                    }
                }

                // Only update the used ring if the entire chain was processed
                // successfully. Posting a partial completion confuses the guest driver.
                if chain_valid {
                    should_set_interrupt = true;

                    let used_idx = unsafe { ptr::read_volatile(&(*used_q).idx) };
                    unsafe {
                        let ring_ptr = (*used_q).ring.as_mut_ptr();
                        let elem = ring_ptr.add((used_idx % QUEUE_SIZE) as usize);
                        ptr::write_volatile(&mut (*elem).id, desc_idx_first as u32);
                        ptr::write_volatile(&mut (*elem).len, num_bytes_written as u32);
                    }
                    std::sync::atomic::fence(Ordering::SeqCst);
                    unsafe {
                        ptr::write_volatile(&mut (*used_q).idx, used_idx.wrapping_add(1));
                    }
                }

                processed[qi] = processed[qi].wrapping_add(1);
            }

            if should_set_interrupt {
                interrupt_ctl.set_interrupt(regs.interrupt_status, interrupt_number);
            }
        }

        unsafe { libc::usleep(1); }
    }
}

/// Get a pointer to the device-specific config region (offset 0x100).
pub fn config_ptr(l2cpu: &L2Cpu, mmio_region_offset: u64) -> (*mut u8, TlbWindow) {
    let address = l2cpu.starting_address() + l2cpu.memory_size() - mmio_region_offset;
    let window = l2cpu
        .get_persistent_2m_window(address)
        .expect("failed to create config window");
    let ptr = unsafe { window.get_window().add(VIRTIO_MMIO_CONFIG) };
    (ptr, window)
}
