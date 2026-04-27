// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! VirtIO MMIO device framework — base implementation for device emulation.

pub mod block;
pub mod console;
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

/// Discriminator for the per-device interrupt counters in
/// `crate::daemon::metrics`. The `run_device` accept loop is generic
/// over device kind, but each interrupt belongs to exactly one of
/// these two metric families — the caller picks at spawn time.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InterruptKind {
    Block,
    Net,
    Console,
}

/// Bump the per-kind interrupt counter at index `idx`. Pulled out of
/// `run_device` so the kind-mapping logic is unit-testable without
/// having to drive the full chip-memory loop.
pub(crate) fn bump_interrupt_metric(kind: InterruptKind, idx: u8) {
    match kind {
        InterruptKind::Block => crate::daemon::metrics::BLK_INTERRUPTS_TOTAL.at(idx).inc(),
        InterruptKind::Net => crate::daemon::metrics::NET_INTERRUPTS_TOTAL.at(idx).inc(),
        InterruptKind::Console => crate::daemon::metrics::CONSOLE_INTERRUPTS_TOTAL.at(idx).inc(),
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

// SAFETY: `MmioRegs` is a bag of `*mut u32` pointers into the persistent
// 2 MiB MMIO TLB window owned by the calling worker's `Arc<L2Cpu>`. The
// pointers are valid for the lifetime of `run_device` because:
//   1. `run_device` keeps the originating window alive via the `L2Cpu`
//      it borrows from the worker thread.
//   2. Each worker thread owns its own `MmioRegs` (constructed inside
//      `run_device`); we only need `Send` so the struct can move into
//      the spawned thread, not `Sync` for cross-thread sharing.
// We do NOT mark `Sync` — concurrent access from multiple threads
// would race on the volatile MMIO without our handshake's
// `sel_generation` discipline; keeping `Send`-only enforces that
// assumption at the type level.
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

pub(crate) const QUEUE_SIZE: u16 = 16384;

// Warm-restart stash: we persist the per-queue descriptor/avail/used ring
// addresses in the high half of the MMIO region so a fresh server can resume
// a guest that's already past virtio init (e.g., after Ctrl-C + reconnect).
// The standard virtio registers live in [0x000, 0x100); device-specific config
// in [0x100, ~0x120); we use [0x200, 0x200 + 24*num_queues) for the stash.
const STASH_OFFSET: usize = 0x200;
const STASH_PER_QUEUE: usize = 24; // desc_addr + avail_addr + used_addr, each u64

// ---------------------------------------------------------------------------
// Pure-decode handshake helpers
//
// These are the decisions `run_device` makes after each MMIO read. Pulling
// them out lets unit tests pin the behavior (notably the `wrapping_add(1)`
// fix for sel_generation overflow at u32::MAX — a regression that hardware
// observation alone wouldn't catch quickly) without poking real registers.
// ---------------------------------------------------------------------------

/// `sel_generation` echo helper. Patched guests (Linux + U-Boot) write
/// `prev + 1` to the daemon-private `sel_generation` register at offset
/// `0x01c` and spin until the daemon writes back something different.
/// Stock guests don't touch this register at all.
///
/// Returns `Some(next)` if the guest has written a new value the daemon
/// hasn't echoed yet; the caller writes `next` back into the register
/// to release the patched-guest spin. Returns `None` when there's no
/// new bump to ack, which is the steady state for stock guests.
///
/// `wrapping_add(1)` is mandatory: the register is a `u32` and a plain
/// `+ 1` panics in debug builds when the guest reaches `u32::MAX`
/// legitimately, *or* when garbage from a concurrent-write race lands
/// in the read.
fn echo_sel_generation(curr_gen: u32, last_echoed: u32) -> Option<u32> {
    if curr_gen == last_echoed {
        None
    } else {
        Some(curr_gen.wrapping_add(1))
    }
}

/// Has the guest written a new `device_features_sel`? Returns `Some(idx)`
/// where `idx` is the array index (`& 1`) into [`VirtioDeviceImpl::device_features`]
/// the daemon should now expose at `device_features`. Returns `None` when
/// the value matches what we last published, so callers don't issue an
/// unnecessary MMIO write.
fn next_features_sel(curr_sel: u32, last_published_sel: u32) -> Option<usize> {
    if curr_sel == last_published_sel {
        None
    } else {
        Some((curr_sel as usize) & 1)
    }
}

/// A fresh server is willing to skip the cold-start handshake when the
/// MMIO region looks like a previous successful Phase-3 left it: matching
/// magic + dev id, plus a stash of in-range queue addresses. Without
/// `stash_all_valid` we fall back to cold-start because partial state is
/// worse than starting from scratch.
fn is_warm_restart_candidate(
    existing_magic: u32,
    existing_dev_id: u32,
    expected_magic: u32,
    expected_dev_id: u32,
    stash_all_valid: bool,
) -> bool {
    existing_magic == expected_magic && existing_dev_id == expected_dev_id && stash_all_valid
}

/// Run a VirtIO device: setup MMIO, negotiate features, process descriptors.
pub fn run_device(
    device: &mut dyn VirtioDeviceImpl,
    l2cpu: &L2Cpu,
    interrupt_ctl: &InterruptController,
    interrupt_number: u32,
    mmio_region_offset: u64,
    exit_flag: &AtomicBool,
    interrupt_kind: InterruptKind,
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
    // Range check used by both warm-restart probing (below) and the
    // queue-pointer validation later. Takes a `size` so a stash address
    // near `mem_end` doesn't pass for a ring whose extent runs off the
    // end of mapped DRAM.
    let in_range_size = |addr: u64, size: u64| -> bool {
        if addr < starting_address {
            return false;
        }
        match addr.checked_add(size) {
            Some(end) => end <= mem_end,
            None => false,
        }
    };

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
        let existing_magic =
            ptr::read_volatile(mmio_base.add(VIRTIO_MMIO_MAGIC_VALUE) as *const u32);
        let existing_dev_id =
            ptr::read_volatile(mmio_base.add(VIRTIO_MMIO_DEVICE_ID) as *const u32);
        let existing_status = ptr::read_volatile(mmio_base.add(VIRTIO_MMIO_STATUS) as *const u32);

        let mut desc = vec![0u64; num_queues as usize];
        let mut avail = vec![0u64; num_queues as usize];
        let mut used = vec![0u64; num_queues as usize];
        let mut stash_all_valid = true;
        let mut stash_all_zero = true;
        // Match the sizes in the queue-pointer validation below.
        let queue_size = QUEUE_SIZE as u64;
        let desc_bytes = queue_size * std::mem::size_of::<VringDesc>() as u64;
        let avail_bytes = 4 + queue_size * std::mem::size_of::<u16>() as u64;
        let used_bytes = 4 + queue_size * std::mem::size_of::<VringUsedElem>() as u64;
        for i in 0..num_queues as usize {
            let base = mmio_base.add(STASH_OFFSET + i * STASH_PER_QUEUE);
            desc[i] = ptr::read_volatile(base as *const u64);
            avail[i] = ptr::read_volatile(base.add(8) as *const u64);
            used[i] = ptr::read_volatile(base.add(16) as *const u64);
            if desc[i] != 0 || avail[i] != 0 || used[i] != 0 {
                stash_all_zero = false;
            }
            if !in_range_size(desc[i], desc_bytes)
                || !in_range_size(avail[i], avail_bytes)
                || !in_range_size(used[i], used_bytes)
            {
                stash_all_valid = false;
            }
        }

        let magic_matches = existing_magic == VIRTIO_MAGIC && existing_dev_id == device.device_id();

        if is_warm_restart_candidate(
            existing_magic,
            existing_dev_id,
            VIRTIO_MAGIC,
            device.device_id(),
            stash_all_valid,
        ) {
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
        (
            descriptor_table_address,
            available_ring_address,
            used_ring_address,
        )
    } else {
        // Cold start: zero the standard register window (preserving stash at
        // 0x200+) and drive the guest through the init handshake.
        unsafe {
            ptr::write_bytes(mmio_base, 0, 0x200);
        }

        let features = device.device_features();
        unsafe {
            ptr::write_volatile(regs.magic_value, VIRTIO_MAGIC);
            ptr::write_volatile(mmio_base.add(VIRTIO_MMIO_VERSION) as *mut u32, 2);
            ptr::write_volatile(
                mmio_base.add(VIRTIO_MMIO_DEVICE_ID) as *mut u32,
                device.device_id(),
            );
            ptr::write_volatile(regs.queue_num_max, QUEUE_SIZE as u32);
            ptr::write_volatile(mmio_base.add(0x018) as *mut u32, 1); // sw_impl
            ptr::write_volatile(regs.sel_generation, 0);
            // Initialize `device_features_sel` to 1 to match the
            // pre-populated `device_features = features[1]` below.
            // Phase 2's poll loop tracks `last_published_sel = 1`;
            // if MMIO `_sel` is 0 (the post-zeroing default), the
            // first poll iteration spuriously fires a 1→0
            // "transition" and overwrites our coherent
            // pre-populated value with `features[0]` *before* the
            // guest has ever touched `_sel`. Keeping MMIO and
            // bookkeeping in sync prevents the spurious update.
            ptr::write_volatile(regs.device_features_sel, 1);
            // Pre-populate `device_features` with the **high** half
            // (`features[1]`) BEFORE we wait for the guest's DRIVER
            // bit. Stock guests read `_features` within microseconds
            // of writing `_sel = 1`; if the daemon waits to seed
            // `_features` until Phase 2, the kernel reads 0 (post-
            // zeroing default) on its first feature access and
            // rejects the device for missing `VIRTIO_F_VERSION_1`.
            //
            // Linux's `vm_get_features` reads `_sel = 1` first
            // (`features = readl(_features); features <<= 32`), so
            // `features[1]` is the value we want exposed at the
            // initial cold-start moment.
            //
            // For all three of our devices today (blk, net, console)
            // `features[0]` is `0`, so a stale read of
            // `features[1]` for the second `_sel = 0` access just
            // leaks bit 0 of `features[1]` into the low half — which
            // maps to harmless / no-op feature bits per device:
            // `VIRTIO_BLK_F_BARRIER` (deprecated), `VIRTIO_NET_F_CSUM`
            // (the one to revisit if stock virtio-net ever needs to
            // work), `VIRTIO_CONSOLE_F_SIZE` (config reports 0×0,
            // tolerated). If `features[0]` ever goes non-zero the
            // race becomes harder to paper over and we'd need a
            // real synchronization mechanism.
            ptr::write_volatile(regs.device_features, features[1]);
        }

        // Populate device-specific config region now that the zero above
        // has cleared it. The guest will read this during probe.
        device.init_config(unsafe { mmio_base.add(VIRTIO_MMIO_CONFIG) });

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
            unsafe {
                libc::usleep(1000);
            }
        }

        // Phase 2: feature negotiation.
        //
        // Two flavors of guest share this loop:
        // - **Stock** virtio-mmio drivers (upstream Linux without our
        //   patch, U-Boot DM virtio without #49's patch) write
        //   `device_features_sel` then immediately read
        //   `device_features` — no spin-gate. We pre-populate
        //   `device_features` with `features[0]` so the first read at
        //   the default `_sel = 0` is correct, then poll `_sel` and
        //   update `device_features` whenever it changes. The race
        //   window between the guest's `_sel` write and `_features`
        //   read is shorter than the daemon's busy-poll cadence on
        //   real hardware (#50); on Blackhole, uncached MMIO reads
        //   from the L2CPU side are slower than our PCIe round-trip.
        // - **Patched** drivers (our kernel + U-Boot tree) additionally
        //   bump `sel_generation = prev + 1` after each register write
        //   and spin until the daemon echoes a different value. We
        //   keep echoing those bumps so patched drivers terminate
        //   their spin — but we no longer *require* a bump to do
        //   feature work, so a stock driver is no longer fatally
        //   stuck here.
        //
        // No `usleep` in this loop: tight polling is the only way the
        // daemon catches the stock-driver `_sel → _features` window
        // before the guest's read returns. The loop terminates when
        // the guest sets `FEATURES_OK`, which it does within a
        // microsecond after the second feature read on every guest
        // we've measured.
        // `device_features` is left at the pre-populated `features[1]`
        // from cold-start for the duration of Phase 2. The stale read
        // for `_sel = 0` is benign (see cold-start comment); the
        // alternative — eagerly republishing on `_sel` polls — loses
        // the race against the guest's back-to-back `writel(_sel) +
        // readl(_features)` and ends up exposing zero (post-zeroing
        // default) on the first feature read.
        //
        // Patched guests still bump `sel_generation` between writes
        // and spin on the echo; we keep responding so they don't get
        // stuck.
        let _ = features; // silence unused-variable lint after removing the per-_sel update
        let mut last_echoed_gen: u32 = 0;
        while !exit_flag.load(Ordering::Relaxed) {
            if unsafe { ptr::read_volatile(regs.status) } & VIRTIO_CONFIG_S_FEATURES_OK != 0 {
                break;
            }
            let curr_gen = unsafe { ptr::read_volatile(regs.sel_generation) };
            if let Some(next) = echo_sel_generation(curr_gen, last_echoed_gen) {
                unsafe {
                    ptr::write_volatile(regs.sel_generation, next);
                }
                last_echoed_gen = next;
            }
        }

        // Phase 3: capture per-queue config as the guest writes it,
        // then wait for DRIVER_OK.
        //
        // The virtio-mmio spec multiplexes `QUEUE_DESC` /
        // `QUEUE_AVAIL` / `QUEUE_USED` (and `QUEUE_READY`) through
        // `QUEUE_SEL`: each per-queue register access is implicitly
        // scoped to the currently-selected queue. Real hardware
        // demultiplexes per access; we have flat DRAM-backed
        // registers, so each queue's writes overwrite the previous
        // queue's values in the same DRAM cell.
        //
        // To recover per-queue values without the old `sel_generation`
        // gate, we poll `QUEUE_SEL`: when the guest moves to the
        // next queue, the previous queue's writes are complete and
        // sitting in MMIO — we snapshot them into per-queue daemon
        // state before the next queue's writes overwrite them.
        // Tens of microseconds typically pass between
        // `writel(SEL=next)` and the next queue's first
        // `writel(DESC_LOW)` (kernel `vring_create_virtqueue` does
        // memory allocation in between), comfortably wider than the
        // daemon's PCIe poll cadence.
        //
        // We also clear `QUEUE_READY` on every `SEL` transition so
        // the next queue's "READY must be 0 at start of setup"
        // check (vm_setup_vq returns -ENOENT otherwise) sees a fresh
        // register. Patched guests get their `sel_generation` bumps
        // echoed too so their spin-waits terminate.
        let mut q_state: Vec<[u64; 3]> = vec![[0u64; 3]; num_queues as usize];
        let mut last_sel: u32 = 0;
        let snapshot_current_queue = |q_state: &mut Vec<[u64; 3]>, sel: u32| {
            let qi = sel as usize;
            if qi >= q_state.len() {
                return;
            }
            unsafe {
                q_state[qi][0] = ((ptr::read_volatile(regs.queue_desc_high) as u64) << 32)
                    | (ptr::read_volatile(regs.queue_desc_low) as u64);
                q_state[qi][1] = ((ptr::read_volatile(regs.queue_avail_high) as u64) << 32)
                    | (ptr::read_volatile(regs.queue_avail_low) as u64);
                q_state[qi][2] = ((ptr::read_volatile(regs.queue_used_high) as u64) << 32)
                    | (ptr::read_volatile(regs.queue_used_low) as u64);
            }
        };
        while !exit_flag.load(Ordering::Relaxed) {
            if unsafe { ptr::read_volatile(regs.status) } & VIRTIO_CONFIG_S_DRIVER_OK != 0 {
                break;
            }
            // Eager `QUEUE_READY` clear: the guest just wrote 1 to
            // mark the current queue ready, but its first action on
            // the next queue is `readl(QUEUE_READY)` expecting 0
            // (vm_setup_vq returns -ENOENT otherwise). Real hardware
            // demultiplexes through `QUEUE_SEL`; we clear the single
            // DRAM cell as soon as we see it set so the next queue's
            // start-of-setup read sees fresh 0.
            //
            // Race window between the guest's `writel(QUEUE_READY=1)`
            // and the next queue's `readl(QUEUE_READY)`: the
            // kernel/U-Boot returns from `setup_vq`, runs the per-queue
            // loop body, calls `setup_vq` again, then issues two
            // MMIO ops (`QUEUE_SEL` write + `QUEUE_READY` read) — on
            // the order of microseconds, comfortably wider than this
            // poll loop's PCIe iteration cost. The clear isn't gated
            // on `QUEUE_SEL` change because the guest may set
            // `QUEUE_READY=1` and proceed to the next queue's
            // `QUEUE_SEL` write so quickly that a sel-gated clear
            // would lose the race.
            if unsafe { ptr::read_volatile(regs.queue_ready) } != 0 {
                unsafe {
                    ptr::write_volatile(regs.queue_ready, 0);
                }
            }
            let curr_sel = unsafe { ptr::read_volatile(regs.queue_select) };
            if curr_sel != last_sel {
                // The guest moved to a new queue. The previous
                // queue's writes (DESC / AVAIL / USED) are still in
                // MMIO; capture them into our per-queue state before
                // the next queue's writes overwrite them.
                snapshot_current_queue(&mut q_state, last_sel);
                last_sel = curr_sel;
            }
            let curr_gen = unsafe { ptr::read_volatile(regs.sel_generation) };
            if let Some(next) = echo_sel_generation(curr_gen, last_echoed_gen) {
                unsafe {
                    ptr::write_volatile(regs.sel_generation, next);
                }
                last_echoed_gen = next;
            }
        }

        // The guest's last queue setup never had a following SEL
        // change to trigger the snapshot above — DRIVER_OK was
        // written instead. Capture it now from whatever the MMIO
        // still holds for that queue.
        snapshot_current_queue(&mut q_state, last_sel);

        let mut desc = vec![0u64; num_queues as usize];
        let mut avail = vec![0u64; num_queues as usize];
        let mut used = vec![0u64; num_queues as usize];
        for i in 0..num_queues as usize {
            desc[i] = q_state[i][0];
            avail[i] = q_state[i][1];
            used[i] = q_state[i][2];
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
    // Both the start address AND the full ring extent must lie inside the
    // mapped DRAM window. Without the size check, a guest could place a
    // ring at `mem_end - 8` and the daemon would walk descriptor entries
    // past the end of valid memory (security finding from #17).
    let validate_addr = |addr: u64, size: u64, label: &str, qi: usize| -> usize {
        if addr < starting_address {
            panic!(
                "virtqueue {} address {:#x} for queue {} below L2CPU memory start {:#x}",
                label, addr, qi, starting_address
            );
        }
        let end = addr.saturating_add(size);
        if end > mem_end {
            panic!(
                "virtqueue {} for queue {} extends past L2CPU memory: addr={:#x} size={:#x} mem_end={:#x}",
                label, qi, addr, size, mem_end
            );
        }
        (addr - starting_address) as usize
    };

    // Per the virtio spec (no EVENT_IDX negotiated):
    //   desc table: QUEUE_SIZE × 16-byte VringDesc
    //   avail ring: 2-byte flags + 2-byte idx + QUEUE_SIZE × 2-byte ring entry
    //   used ring : 2-byte flags + 2-byte idx + QUEUE_SIZE × 8-byte VringUsedElem
    let queue_size = QUEUE_SIZE as u64;
    let desc_bytes = queue_size * std::mem::size_of::<VringDesc>() as u64;
    let avail_bytes = 4 + queue_size * std::mem::size_of::<u16>() as u64;
    let used_bytes = 4 + queue_size * std::mem::size_of::<VringUsedElem>() as u64;

    for i in 0..num_queues as usize {
        desc_ptrs.push(unsafe {
            memory.add(validate_addr(
                descriptor_table_address[i],
                desc_bytes,
                "desc",
                i,
            )) as *mut VringDesc
        });
        avail_ptrs.push(unsafe {
            memory.add(validate_addr(
                available_ring_address[i],
                avail_bytes,
                "avail",
                i,
            )) as *mut VringAvail
        });
        used_ptrs.push(unsafe {
            memory.add(validate_addr(used_ring_address[i], used_bytes, "used", i)) as *mut VringUsed
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

    // Three-tier adaptive sleep with hysteresis:
    //   - FAST  (1 µs)   while guest is actively pushing descriptors
    //   - SLOW  (1 ms)   when no activity for FAST_WINDOW (200 ms)
    //   - IDLE  (10 ms)  when no activity for IDLE_WINDOW (2 s)
    // Hysteresis avoids flapping: a single empty pass mid-burst stays
    // FAST; a sustained quiet stretch falls all the way to IDLE.
    //
    // Tier-3 (IDLE) is the difference between ~6% idle CPU (worker
    // polling at 1 ms = 1000 Hz) and well under 1% (10 ms = 100 Hz).
    // The cost is at most one IDLE_SLEEP of latency on the first
    // descriptor after a long idle stretch — fine for guest workloads
    // whose timeouts are at the seconds level. See `chip_console.rs`
    // for the matching shape and #27 for the measurement that drove
    // the tier.
    const FAST_SLEEP_US: libc::c_uint = 1;
    const SLOW_SLEEP_US: libc::c_uint = 1000;
    const IDLE_SLEEP_US: libc::c_uint = 10_000;
    const FAST_WINDOW: std::time::Duration = std::time::Duration::from_millis(200);
    const IDLE_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
    let mut last_active = std::time::Instant::now();

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
                        eprintln!(
                            "virtio: descriptor chain exceeded {} steps, breaking",
                            QUEUE_SIZE
                        );
                        chain_valid = false;
                        break;
                    }
                    steps += 1;

                    let d =
                        unsafe { ptr::read_volatile(desc_q.add((desc_idx % QUEUE_SIZE) as usize)) };

                    // Validate descriptor address is within L2CPU memory.
                    // Use checked arithmetic to prevent overflow bypassing the check.
                    let addr_end = (d.addr).checked_add(d.len as u64);
                    if d.addr < starting_address
                        || d.addr >= mem_end
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
                        // The last descriptor's actual-bytes-written
                        // can be less than its buffer capacity (e.g.
                        // virtio-console RX with partial input). The
                        // device returns the real count; block/net
                        // pass `d.len` through unchanged.
                        let actual =
                            device.process_queue_complete(queue_idx, addr, d.len as u64);
                        num_bytes_written += actual;
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
                bump_interrupt_metric(interrupt_kind, l2cpu.idx() as u8);
                did_work = true;
            }
        }

        // Adaptive sleep — see the FAST/SLOW/IDLE constants above for
        // the tiers and rationale.
        if did_work {
            last_active = std::time::Instant::now();
        }
        let elapsed = last_active.elapsed();
        let tier = crate::daemon::metrics::classify_tier(elapsed, FAST_WINDOW, IDLE_WINDOW);
        let sleep_us = match tier {
            crate::daemon::metrics::Tier::Fast => FAST_SLEEP_US,
            crate::daemon::metrics::Tier::Slow => SLOW_SLEEP_US,
            crate::daemon::metrics::Tier::Idle => IDLE_SLEEP_US,
        };
        let worker = match interrupt_kind {
            InterruptKind::Block => crate::daemon::metrics::WorkerKind::VirtioBlk,
            InterruptKind::Net => crate::daemon::metrics::WorkerKind::VirtioNet,
            InterruptKind::Console => crate::daemon::metrics::WorkerKind::VirtioConsole,
        };
        let idx_u8 = l2cpu.idx() as u8;
        crate::daemon::metrics::WORKER_POLL_ITERATIONS_TOTAL
            .at(worker, idx_u8, tier)
            .inc();
        crate::daemon::metrics::WORKER_TIER_NANOS_TOTAL
            .at(worker, idx_u8, tier)
            .add(sleep_us as u64 * 1_000);
        unsafe {
            libc::usleep(sleep_us);
        }
    }
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

    #[test]
    fn echo_sel_generation_skips_when_unchanged() {
        // Steady state: guest has no new bump for the daemon to ack.
        // Stock guests sit here permanently (they never write the
        // register); patched guests sit here between handshake steps.
        assert_eq!(echo_sel_generation(0, 0), None);
        assert_eq!(echo_sel_generation(7, 7), None);
    }

    #[test]
    fn echo_sel_generation_bumps_normally() {
        // Patched guest wrote prev+1 (=6); daemon last echoed 5;
        // daemon now writes 7 to release the spin.
        assert_eq!(echo_sel_generation(6, 5), Some(7));
    }

    #[test]
    fn echo_sel_generation_wraps_at_u32_max() {
        // Regression guard: the register is a u32; plain `+ 1` panics
        // in debug builds when the guest reaches u32::MAX legitimately
        // *or* when garbage from a concurrent-write race lands in the
        // read. The wrapping_add is the contract.
        assert_eq!(echo_sel_generation(u32::MAX, 0), Some(0));
    }

    #[test]
    fn next_features_sel_skips_when_sel_unchanged() {
        // Guest hasn't moved the selector; no MMIO write needed and
        // we don't want to thrash device_features just because the
        // poll loop ran another iteration.
        assert_eq!(next_features_sel(0, 0), None);
        assert_eq!(next_features_sel(1, 1), None);
    }

    #[test]
    fn next_features_sel_returns_low_index_for_zero() {
        // Default case: guest selects the low half (bits 0..32).
        assert_eq!(next_features_sel(0, 1), Some(0));
    }

    #[test]
    fn next_features_sel_returns_high_index_for_one() {
        // Guest selects high half (bits 32..64) — VIRTIO_F_VERSION_1
        // lives in there for our modern-only devices.
        assert_eq!(next_features_sel(1, 0), Some(1));
    }

    #[test]
    fn next_features_sel_clamps_unexpected_values_to_one_bit() {
        // Spec only defines _sel ∈ {0, 1}, but a buggy or hostile
        // guest could write any u32. We mask to one bit so the
        // index stays in range of `device_features: [u32; 2]`
        // and we don't read out-of-bounds.
        assert_eq!(next_features_sel(2, 1), Some(0));
        assert_eq!(next_features_sel(3, 0), Some(1));
        assert_eq!(next_features_sel(0xffff_ffff, 0), Some(1));
    }

    #[test]
    fn warm_restart_requires_magic_match() {
        // Wrong magic → not a candidate, regardless of stash.
        assert!(!is_warm_restart_candidate(
            0xdead_beef,
            1,
            VIRTIO_MAGIC,
            1,
            true
        ));
    }

    #[test]
    fn warm_restart_requires_dev_id_match() {
        // Right magic but wrong device id → not a candidate. Catches the
        // case where two devices share an MMIO region after a layout bug.
        assert!(!is_warm_restart_candidate(
            VIRTIO_MAGIC,
            7,
            VIRTIO_MAGIC,
            1,
            true
        ));
    }

    #[test]
    fn warm_restart_requires_stash_valid() {
        // Magic + dev id match but stash is partial (out-of-range
        // address) — fall back to cold-start. Partial state is worse
        // than starting fresh.
        assert!(!is_warm_restart_candidate(
            VIRTIO_MAGIC,
            1,
            VIRTIO_MAGIC,
            1,
            false
        ));
    }

    #[test]
    fn warm_restart_accepted_when_all_three_align() {
        assert!(is_warm_restart_candidate(
            VIRTIO_MAGIC,
            1,
            VIRTIO_MAGIC,
            1,
            true
        ));
    }
}
