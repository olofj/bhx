// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! VirtIO MMIO device framework — base implementation for device emulation.

pub mod block;
pub mod interrupt;
#[cfg(feature = "slirp")]
pub mod network;

use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::l2cpu::L2Cpu;
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
    /// Populate device-specific config at MMIO offset 0x100. Called once
    /// during cold-start, after the framework has zeroed the standard
    /// register window (0x00..0x200). Must happen *after* the zero; writing
    /// config before `run_device` would be wiped out.
    fn init_config(&self, _config: *mut u8) {}
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

// Warm-restart stash: we persist the per-queue descriptor/avail/used ring
// addresses in the high half of the MMIO region so a fresh server can resume
// a guest that's already past virtio init (e.g., after Ctrl-C + reconnect).
// The standard virtio registers live in [0x000, 0x100); device-specific config
// in [0x100, ~0x120); we use [0x200, 0x200 + 24*num_queues) for the stash.
const STASH_OFFSET: usize = 0x200;
const STASH_PER_QUEUE: usize = 24; // desc_addr + avail_addr + used_addr, each u64

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

    let num_queues = device.num_queues();
    let mem_end = starting_address + l2cpu.memory_size();
    let in_range = |addr: u64| addr >= starting_address && addr < mem_end;

    // Warm-restart detection: if the MMIO region already has our magic and a
    // full set of stashed queue addresses from a prior successful handshake,
    // the guest driver is already past init and won't re-run it. Skip to the
    // main loop using the stashed addresses.
    //
    // We deliberately don't key off the DRIVER_OK status bit: a previous
    // server's cold-start may have zeroed the standard register window after
    // the guest set DRIVER_OK, leaving status=0 even though the guest is
    // still fully initialized. Valid stash + matching magic is a stronger
    // signal — stash is only written at the end of a successful Phase 3, so
    // an all-in-range stash means a prior server got clean queue addresses.
    let (descriptor_table_address, available_ring_address, used_ring_address, warm_restarted) = unsafe {
        let existing_magic = ptr::read_volatile(mmio_base.add(VIRTIO_MMIO_MAGIC_VALUE) as *const u32);
        let existing_dev_id = ptr::read_volatile(mmio_base.add(VIRTIO_MMIO_DEVICE_ID) as *const u32);
        let existing_status = ptr::read_volatile(mmio_base.add(VIRTIO_MMIO_STATUS) as *const u32);

        let mut desc = vec![0u64; num_queues as usize];
        let mut avail = vec![0u64; num_queues as usize];
        let mut used = vec![0u64; num_queues as usize];
        let mut stash_all_valid = true;
        let mut stash_all_zero = true;
        for i in 0..num_queues as usize {
            let base = mmio_base.add(STASH_OFFSET + i * STASH_PER_QUEUE);
            desc[i] = ptr::read_volatile(base as *const u64);
            avail[i] = ptr::read_volatile(base.add(8) as *const u64);
            used[i] = ptr::read_volatile(base.add(16) as *const u64);
            if desc[i] != 0 || avail[i] != 0 || used[i] != 0 {
                stash_all_zero = false;
            }
            if !in_range(desc[i]) || !in_range(avail[i]) || !in_range(used[i]) {
                stash_all_valid = false;
            }
        }

        let magic_matches = existing_magic == VIRTIO_MAGIC
            && existing_dev_id == device.device_id();

        if magic_matches && stash_all_valid {
            eprintln!(
                "virtio: device {} warm restart — resuming from stashed queue state (status={:#x})",
                existing_dev_id, existing_status
            );
            (desc, avail, used, true)
        } else {
            if magic_matches && !stash_all_zero {
                // We've been here before but the stash is incomplete — a
                // previous cold-start handshake was interrupted partway
                // through Phase 3. Zeroing and trying again is the best we
                // can do; it'll only succeed if the guest is still mid-init.
                eprintln!(
                    "virtio: device {} has partial stashed state (magic set but stash invalid). \
                     Retrying cold-start handshake — if the guest already finished init this will hang; \
                     reboot the guest (`sudo reboot` on the guest console) to recover.",
                    existing_dev_id
                );
            } else if magic_matches {
                eprintln!(
                    "virtio: device {} has no stashed state from a prior run (probably first use of a \
                     server version with warm-restart support). If the guest is already past virtio init \
                     the cold-start handshake will hang; reboot the guest to recover.",
                    existing_dev_id
                );
            }
            (
                vec![0u64; num_queues as usize],
                vec![0u64; num_queues as usize],
                vec![0u64; num_queues as usize],
                false,
            )
        }
    };

    let regs = MmioRegs::new(mmio_base);

    let (descriptor_table_address, available_ring_address, used_ring_address) = if warm_restarted {
        (descriptor_table_address, available_ring_address, used_ring_address)
    } else {
        // Cold start: zero the standard register window (preserving stash at
        // 0x200+) and drive the guest through the init handshake.
        unsafe {
            ptr::write_bytes(mmio_base, 0, 0x200);
        }

        unsafe {
            ptr::write_volatile(regs.magic_value, VIRTIO_MAGIC);
            ptr::write_volatile(mmio_base.add(VIRTIO_MMIO_VERSION) as *mut u32, 2);
            ptr::write_volatile(mmio_base.add(VIRTIO_MMIO_DEVICE_ID) as *mut u32, device.device_id());
            ptr::write_volatile(regs.queue_num_max, QUEUE_SIZE as u32);
            ptr::write_volatile(mmio_base.add(0x018) as *mut u32, 1); // sw_impl
            ptr::write_volatile(regs.sel_generation, 0);
        }

        // Populate device-specific config region now that the zero above
        // has cleared it. The guest will read this during probe.
        device.init_config(unsafe { mmio_base.add(VIRTIO_MMIO_CONFIG) });

        let features = device.device_features();

        // Phase 1: Wait for DRIVER status.
        //
        // If the guest was already past virtio init when the server started,
        // it won't re-assert DRIVER and this loop waits forever. Nudge the
        // user every few seconds so it's clear what's happening.
        let phase1_start = std::time::Instant::now();
        let mut next_hint = phase1_start + std::time::Duration::from_secs(5);
        while !exit_flag.load(Ordering::Relaxed) {
            if unsafe { ptr::read_volatile(regs.status) } & VIRTIO_CONFIG_S_DRIVER != 0 {
                break;
            }
            if std::time::Instant::now() >= next_hint {
                eprintln!(
                    "virtio: device {} still waiting for the guest to start virtio init (DRIVER bit). \
                     If the guest is already up, reboot it (`sudo reboot` on the guest console) to re-run init.",
                    device.device_id()
                );
                next_hint += std::time::Duration::from_secs(15);
            }
            unsafe { libc::usleep(1000); }
        }

        // Phase 2: Feature negotiation via sel_generation.
        //
        // sel_generation is shared with Phase 3, so we must be careful not to
        // consume a queue-setup bump as if it were a feature event. The guest
        // sets FEATURES_OK between the last feature bump and the first queue
        // bump; if FEATURES_OK is observed, any outstanding bump belongs to
        // Phase 3 and we leave it for that loop to handle.
        let mut prev_gen: u32 = 0;
        while !exit_flag.load(Ordering::Relaxed) {
            if unsafe { ptr::read_volatile(regs.status) } & VIRTIO_CONFIG_S_FEATURES_OK != 0 {
                break;
            }
            let curr_gen = unsafe { ptr::read_volatile(regs.sel_generation) };
            if curr_gen != prev_gen {
                // Re-check status *after* reading sel_generation to close the
                // window where the guest flipped FEATURES_OK and bumped the
                // generation for queue setup between our two reads.
                if unsafe { ptr::read_volatile(regs.status) } & VIRTIO_CONFIG_S_FEATURES_OK != 0 {
                    break;
                }
                let sel = unsafe { ptr::read_volatile(regs.device_features_sel) };
                unsafe {
                    ptr::write_volatile(regs.device_features, features[sel as usize & 1]);
                }
                // wrapping_add: sel_generation is a u32 MMIO counter whose
                // native semantics are unsigned wrap. Plain `+ 1` panics in
                // debug builds once the guest's counter reaches u32::MAX.
                let next_gen = curr_gen.wrapping_add(1);
                unsafe {
                    ptr::write_volatile(regs.sel_generation, next_gen);
                }
                prev_gen = next_gen;
            }
        }

        let mut desc = vec![0u64; num_queues as usize];
        let mut avail = vec![0u64; num_queues as usize];
        let mut used = vec![0u64; num_queues as usize];

        // Phase 3: Queue address exchange. Track which queues have been
        // configured; only break when every queue has been seen. The earlier
        // version broke on the last queue index, which loses addresses if a
        // stray sel_generation bump during the Phase 2/3 transition gets
        // consumed by Phase 2.
        let mut queues_seen = vec![false; num_queues as usize];
        while !exit_flag.load(Ordering::Relaxed) {
            let curr_gen = unsafe { ptr::read_volatile(regs.sel_generation) };
            unsafe { ptr::write_volatile(regs.queue_ready, 0); }
            if curr_gen != prev_gen {
                let q = unsafe { ptr::read_volatile(regs.queue_select) } as usize;
                if q >= num_queues as usize {
                    // Invalid queue index from guest — skip this generation
                    let next_gen = curr_gen.wrapping_add(1);
                    unsafe { ptr::write_volatile(regs.sel_generation, next_gen); }
                    prev_gen = next_gen;
                    unsafe { libc::usleep(1); }
                    continue;
                }

                desc[q] = unsafe {
                    ((ptr::read_volatile(regs.queue_desc_high) as u64) << 32)
                        | (ptr::read_volatile(regs.queue_desc_low) as u64)
                };
                avail[q] = unsafe {
                    ((ptr::read_volatile(regs.queue_avail_high) as u64) << 32)
                        | (ptr::read_volatile(regs.queue_avail_low) as u64)
                };
                used[q] = unsafe {
                    ((ptr::read_volatile(regs.queue_used_high) as u64) << 32)
                        | (ptr::read_volatile(regs.queue_used_low) as u64)
                };
                queues_seen[q] = true;

                let next_gen = curr_gen.wrapping_add(1);
                unsafe { ptr::write_volatile(regs.sel_generation, next_gen); }
                prev_gen = next_gen;

                if queues_seen.iter().all(|&b| b) {
                    break;
                }
            }
            unsafe { libc::usleep(1); }
        }

        // Persist the queue addresses so a future server instance can resume.
        unsafe {
            for i in 0..num_queues as usize {
                let base = mmio_base.add(STASH_OFFSET + i * STASH_PER_QUEUE);
                ptr::write_volatile(base as *mut u64, desc[i]);
                ptr::write_volatile(base.add(8) as *mut u64, avail[i]);
                ptr::write_volatile(base.add(16) as *mut u64, used[i]);
            }
        }

        // Wait for DRIVER_OK
        while !exit_flag.load(Ordering::Relaxed) {
            if unsafe { ptr::read_volatile(regs.status) } & VIRTIO_CONFIG_S_DRIVER_OK != 0 {
                break;
            }
        }

        (desc, avail, used)
    };

    // If the user interrupted the handshake, bail out cleanly rather than
    // falling through to validate_addr with zeroed addresses.
    if exit_flag.load(Ordering::Relaxed) {
        return;
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

    // Main device loop. On warm restart, resume processed[qi] from the used
    // ring's idx — everything before that was completed by the previous server,
    // so we pick up exactly where it left off.
    let mut processed = vec![0u16; num_queues as usize];
    if warm_restarted {
        for qi in 0..num_queues as usize {
            processed[qi] = unsafe { ptr::read_volatile(&(*used_ptrs[qi]).idx) };
        }
    }
    let queue_header_size = device.queue_header_size();

    while !exit_flag.load(Ordering::Relaxed) {
        // Check magic still valid
        if unsafe { ptr::read_volatile(regs.magic_value) } != VIRTIO_MAGIC {
            return;
        }

        interrupt_ctl.ack_interrupt(regs.interrupt_ack);

        // Track whether any queue actually had work to do this pass, so we
        // can stretch the sleep at the bottom when the guest is idle. See
        // the sleep site below.
        let mut did_work = false;

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
                did_work = true;
            }
        }

        // usleep(1) is effectively a scheduler round-trip — fine when the
        // guest is actively pushing descriptors (we want to come back
        // quickly for low latency) but wasteful when idle. Stretch the
        // sleep to 1 ms when no queue had work: still polls fast enough
        // that a burst of descriptors is processed within one driver
        // timeout, and drops idle-worker CPU by ~3 orders of magnitude.
        // Coarse stopgap for the more properly tuned adaptive backoff
        // tracked at <https://github.com/olofj/tt-bh-rust/issues/2>.
        let sleep_us = if did_work { 1 } else { 1000 };
        unsafe { libc::usleep(sleep_us); }
    }
}
