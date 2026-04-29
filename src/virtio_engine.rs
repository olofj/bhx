// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Host-side mirror of the M3 (#69) virtio-mmio register-file engine
//! layout in `brisc-firmware/include/virtio_layout.h`.
//!
//! The BRISC firmware lays out 16 virtio register files (4 L2CPUs ×
//! 4 devices) in the picked Tensix tile's L1 starting at
//! `REGS_BASE = 0x0001_0000`. The host (and eventually the L2CPU's
//! retargeted small TLB, see #70) reads/writes these via the chip
//! TLB on the picker tile. The constants here MUST match the
//! firmware header — the hardware-test path verifies them by
//! reading `MAGIC_VALUE` at the expected offsets.
//!
//! Module-wide `#![allow(dead_code)]` — wire-format constants and
//! VirtioFwSim helpers are intentionally retained even when the
//! current crate path doesn't reach all of them.
#![allow(dead_code)]

/// The embedded M3 firmware bytes, produced by
/// `brisc-firmware/Makefile` and surfaced via `build.rs`.
pub const VIRTIO_FIRMWARE: &[u8] = include_bytes!(env!("BRISC_VIRTIO_BIN"));

// ----- L1 layout -----

pub const CODE_BASE: u32 = 0x0000_0000;
pub const CODE_SIZE: u32 = 0x0000_4000;

pub const STATS_BASE: u32 = 0x0000_4000;
pub const STATS_SIZE: u32 = 0x0000_1000;

pub const REGS_BASE: u32 = 0x0001_0000;
pub const REGS_PER_DEV: u32 = 0x0000_1000;

pub const NUM_L2CPUS: u32 = 4;
pub const DEVS_PER_L2CPU: u32 = 4;
pub const NUM_SLOTS: u32 = NUM_L2CPUS * DEVS_PER_L2CPU;

pub const PER_L2CPU_WINDOW_SIZE: u32 = DEVS_PER_L2CPU * REGS_PER_DEV;

// ----- Device-index assignment within an L2CPU's window -----

pub const DEV_BLK: u32 = 0;
pub const DEV_NET: u32 = 1;
pub const DEV_CONSOLE: u32 = 2;
pub const DEV_RNG: u32 = 3;

#[inline]
pub fn slot(l2cpu_idx: u32, device_idx: u32) -> u32 {
    l2cpu_idx * DEVS_PER_L2CPU + device_idx
}

#[inline]
pub fn slot_regs_base(slot: u32) -> u32 {
    REGS_BASE + slot * REGS_PER_DEV
}

#[inline]
pub fn l2cpu_window_base(l2cpu_idx: u32) -> u32 {
    REGS_BASE + l2cpu_idx * PER_L2CPU_WINDOW_SIZE
}

// ----- Virtio MMIO register offsets (virtio 1.2 §4.2.2) -----

pub const MMIO_MAGIC_VALUE: u32 = 0x000;
pub const MMIO_VERSION: u32 = 0x004;
pub const MMIO_DEVICE_ID: u32 = 0x008;
pub const MMIO_VENDOR_ID: u32 = 0x00c;
pub const MMIO_DEVICE_FEATURES: u32 = 0x010;
pub const MMIO_DEVICE_FEATURES_SEL: u32 = 0x014;
pub const MMIO_SW_IMPL: u32 = 0x018;
pub const MMIO_SEL_GENERATION: u32 = 0x01c;
pub const MMIO_DRIVER_FEATURES: u32 = 0x020;
pub const MMIO_DRIVER_FEATURES_SEL: u32 = 0x024;
pub const MMIO_QUEUE_SEL: u32 = 0x030;
pub const MMIO_QUEUE_NUM_MAX: u32 = 0x034;
pub const MMIO_QUEUE_NUM: u32 = 0x038;
pub const MMIO_QUEUE_READY: u32 = 0x044;
pub const MMIO_QUEUE_NOTIFY: u32 = 0x050;
pub const MMIO_INTERRUPT_STATUS: u32 = 0x060;
pub const MMIO_INTERRUPT_ACK: u32 = 0x064;
pub const MMIO_STATUS: u32 = 0x070;
pub const MMIO_QUEUE_DESC_LOW: u32 = 0x080;
pub const MMIO_QUEUE_DESC_HIGH: u32 = 0x084;
pub const MMIO_QUEUE_DRIVER_LOW: u32 = 0x090;
pub const MMIO_QUEUE_DRIVER_HIGH: u32 = 0x094;
pub const MMIO_QUEUE_DEVICE_LOW: u32 = 0x0a0;
pub const MMIO_QUEUE_DEVICE_HIGH: u32 = 0x0a4;
pub const MMIO_CONFIG_GENERATION: u32 = 0x0fc;
pub const MMIO_CONFIG: u32 = 0x100;

// ----- Constants the firmware writes -----

pub const MAGIC: u32 = 0x7472_6976; // "virt" little-endian
const _MAGIC_IS_VIRT_LE: () = assert!(MAGIC == u32::from_le_bytes([b'v', b'i', b'r', b't']));
pub const VERSION: u32 = 2;
pub const VENDOR_ID: u32 = 0x5554_4254; // "TBTU" — keep in sync with virtio.c

pub const VIRTIO_ID_NET: u32 = 1;
pub const VIRTIO_ID_BLOCK: u32 = 2;
pub const VIRTIO_ID_CONSOLE: u32 = 3;
pub const VIRTIO_ID_ENTROPY: u32 = 4;

/// Must match `BRISC_VIRTIO_QUEUE_NUM_MAX` in
/// `brisc-firmware/include/virtio_layout.h` — the diagnostic in
/// `main.rs::cmd_engine_probe` reads QUEUE_NUM_MAX off the chip and
/// asserts it equals this constant.
pub const QUEUE_NUM_MAX: u32 = 256;

// ----- Status bits (virtio 1.2 §2.1) -----

pub const STATUS_ACKNOWLEDGE: u32 = 1;
pub const STATUS_DRIVER: u32 = 2;
pub const STATUS_DRIVER_OK: u32 = 4;
pub const STATUS_FEATURES_OK: u32 = 8;
pub const STATUS_DEVICE_NEEDS_RESET: u32 = 64;
pub const STATUS_FAILED: u32 = 128;

// ----- Stats page offsets (must match firmware's STATS_OFF_*) -----

pub const STATS_OFF_VERSION: u32 = 0x000;
pub const STATS_OFF_MAGIC: u32 = 0x004;
pub const STATS_OFF_HEARTBEAT: u32 = 0x008;
pub const STATS_OFF_STATUS_CHANGES: u32 = 0x010;
pub const STATS_OFF_SEL_CHANGES: u32 = 0x014;
pub const STATS_OFF_NOTIFY_EVENTS: u32 = 0x018;
pub const STATS_OFF_READY_EVENTS: u32 = 0x01c;
pub const STATS_OFF_LAST_NOTIFY: u32 = 0x020;
pub const STATS_OFF_COMPL_EVENTS: u32 = 0x024;
pub const STATS_OFF_LAST_COMPL: u32 = 0x028;
/// Cumulative count of `kick_ring_push` calls dropped due to a full
/// kick ring (#101). BRISC bumps this when daemon backpressure leaves
/// `(producer - consumer) >= KICK_RING_ENTRIES` at the moment a new
/// kick would be appended; the daemon polls this and surfaces deltas
/// via `bhx_kick_drops_total`.
pub const STATS_OFF_KICK_DROPS: u32 = 0x02c;

// ----- Per-queue shadow region (BRISC L1, M5.5b firmware) -----
//
// The firmware shadows guest writes to the per-queue setup
// registers (QUEUE_NUM + DESC/DRIVER/DEVICE addresses) into this
// region, indexed by `(slot, queue_idx)`. The data plane reads
// from here on each kick to know which avail/desc/used rings to
// walk in guest DRAM. These offsets MUST match the C firmware's
// `SHADOW_Q_OFF_*` constants in `brisc-firmware/virtio.c`.

pub const SHADOW_BASE: u32 = 0x0002_0000;
pub const SHADOW_PER_DEVICE: u32 = 0x400;
pub const SHADOW_PER_QUEUE: u32 = 0x40;
pub const SHADOW_Q_OFF_NUM: u32 = 0x00;
pub const SHADOW_Q_OFF_READY: u32 = 0x04;
pub const SHADOW_Q_OFF_DESC_LO: u32 = 0x08;
pub const SHADOW_Q_OFF_DESC_HI: u32 = 0x0c;
pub const SHADOW_Q_OFF_DRIVER_LO: u32 = 0x10;
pub const SHADOW_Q_OFF_DRIVER_HI: u32 = 0x14;
pub const SHADOW_Q_OFF_DEVICE_LO: u32 = 0x18;
pub const SHADOW_Q_OFF_DEVICE_HI: u32 = 0x1c;

#[inline]
pub fn shadow_queue_addr(slot: u32, queue_idx: u32, off: u32) -> u32 {
    SHADOW_BASE + slot * SHADOW_PER_DEVICE + queue_idx * SHADOW_PER_QUEUE + off
}

pub const STATS_MAGIC_LOADED: u32 = 0x0000_B155;

// ----- Per-device queue counts (must match firmware constants) -----

pub const QUEUES_BLK: u32 = 1;
pub const QUEUES_NET: u32 = 2;
pub const QUEUES_CONSOLE: u32 = 2;
pub const QUEUES_RNG: u32 = 1;

#[inline]
pub fn num_queues_for_device(device_idx: u32) -> u32 {
    match device_idx {
        DEV_BLK => QUEUES_BLK,
        DEV_NET => QUEUES_NET,
        DEV_CONSOLE => QUEUES_CONSOLE,
        DEV_RNG => QUEUES_RNG,
        _ => 0,
    }
}

#[inline]
pub fn device_id_for_index(device_idx: u32) -> u32 {
    match device_idx {
        DEV_BLK => VIRTIO_ID_BLOCK,
        DEV_NET => VIRTIO_ID_NET,
        DEV_CONSOLE => VIRTIO_ID_CONSOLE,
        DEV_RNG => VIRTIO_ID_ENTROPY,
        _ => 0,
    }
}

// ----- Compile-time sanity -----

const _LAYOUT_INVARIANTS: () = {
    // Stats lives between code and the reg files; reg files don't
    // overlap with the shadow region.
    assert!(STATS_BASE >= CODE_BASE + CODE_SIZE);
    assert!(REGS_BASE >= STATS_BASE + STATS_SIZE);
    // 16 reg files fit inside the 64 KiB region [REGS_BASE,
    // REGS_BASE+0x10000).
    assert!(NUM_SLOTS * REGS_PER_DEV == 16 * 0x1000);
    // Each L2CPU's window is exactly 4 contiguous device reg files.
    assert!(PER_L2CPU_WINDOW_SIZE == DEVS_PER_L2CPU * REGS_PER_DEV);
};

/// Pure-Rust simulator of the BRISC virtio firmware (`virtio.c`).
///
/// Mirrors the algorithm — boot init, snapshot-diff polling, the
/// STATUS / QUEUE_SEL / QUEUE_READY / QUEUE_NOTIFY handlers — so we
/// can exercise the state machine without hardware. It's the
/// hardware-free arm of the M3 (#69) test coverage; the C firmware
/// and this simulator must produce identical observable behavior.
///
/// If you change the firmware, port the change here too — divergence
/// will surface when `tests::firmware_logic_*` start failing or the
/// hardware smoke shows different behavior than the simulator.
pub mod sim {
    use std::collections::HashMap;

    use super::*;

    /// Sparse 32-bit memory keyed by absolute L1 byte offset. Reads
    /// of unwritten addresses return 0 (matches a freshly-zeroed
    /// L1).
    #[derive(Default, Clone)]
    pub struct SimL1 {
        cells: HashMap<u32, u32>,
    }

    impl SimL1 {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn read(&self, addr: u32) -> u32 {
            *self.cells.get(&addr).unwrap_or(&0)
        }

        pub fn write(&mut self, addr: u32, value: u32) {
            assert!(addr.is_multiple_of(4), "unaligned sim L1 write");
            self.cells.insert(addr, value);
        }
    }

    // Per-device shadow region layout — matches the C firmware's
    // SHADOW_BASE / SHADOW_PER_DEVICE / SHADOW_PER_QUEUE.
    const SHADOW_BASE: u32 = 0x0002_0000;
    const SHADOW_PER_DEVICE: u32 = 0x400;
    const SHADOW_PER_QUEUE: u32 = 0x40;
    const SHADOW_Q_OFF_NUM: u32 = 0x00;
    const SHADOW_Q_OFF_READY: u32 = 0x04;
    const SHADOW_Q_OFF_DESC_LO: u32 = 0x08;
    const SHADOW_Q_OFF_DESC_HI: u32 = 0x0c;
    const SHADOW_Q_OFF_DRIVER_LO: u32 = 0x10;
    const SHADOW_Q_OFF_DRIVER_HI: u32 = 0x14;
    const SHADOW_Q_OFF_DEVICE_LO: u32 = 0x18;
    const SHADOW_Q_OFF_DEVICE_HI: u32 = 0x1c;
    const SNAP_BASE_OFF: u32 = 0x200;
    const SNAP_OFF_STATUS: u32 = 0x00;
    const SNAP_OFF_QUEUE_SEL: u32 = 0x04;
    const SNAP_OFF_QUEUE_NOTIFY: u32 = 0x08;
    const SNAP_OFF_QUEUE_READY: u32 = 0x0c;
    const SNAP_OFF_SEL_GEN_ECHO: u32 = 0x38;

    fn shadow_addr(slot: u32, off: u32) -> u32 {
        SHADOW_BASE + slot * SHADOW_PER_DEVICE + off
    }
    fn shadow_queue_addr(slot: u32, q: u32, off: u32) -> u32 {
        shadow_addr(slot, q * SHADOW_PER_QUEUE + off)
    }
    fn snap_addr(slot: u32, off: u32) -> u32 {
        shadow_addr(slot, SNAP_BASE_OFF + off)
    }

    /// Simulator state. `boot()` runs the init pass once; each call
    /// to `step()` runs a full poll-all-slots sweep, equivalent to
    /// one iteration of the firmware's outer loop.
    pub struct VirtioFwSim {
        pub l1: SimL1,
    }

    impl Default for VirtioFwSim {
        fn default() -> Self {
            Self::new()
        }
    }

    impl VirtioFwSim {
        pub fn new() -> Self {
            VirtioFwSim { l1: SimL1::new() }
        }

        /// One-time post-reset init. Plants the static reg state for
        /// every device slot and zeroes the stats page magic +
        /// version.
        pub fn boot(&mut self) {
            self.l1.write(STATS_BASE + STATS_OFF_VERSION, 0x0003_0001);
            self.l1
                .write(STATS_BASE + STATS_OFF_MAGIC, STATS_MAGIC_LOADED);
            for slot in 0..NUM_SLOTS {
                self.init_device(slot);
            }
        }

        /// Plant the read-only registers for one slot. Mirrors
        /// `init_device` in `virtio.c`.
        fn init_device(&mut self, slot: u32) {
            // Wipe reg + shadow region.
            for off in (0..REGS_PER_DEV).step_by(4) {
                self.l1.write(slot_regs_base(slot) + off, 0);
            }
            for off in (0..SHADOW_PER_DEVICE).step_by(4) {
                self.l1.write(shadow_addr(slot, off), 0);
            }
            let dev_idx = slot % DEVS_PER_L2CPU;
            let base = slot_regs_base(slot);
            self.l1.write(base + MMIO_MAGIC_VALUE, MAGIC);
            self.l1.write(base + MMIO_VERSION, VERSION);
            self.l1
                .write(base + MMIO_DEVICE_ID, device_id_for_index(dev_idx));
            self.l1.write(base + MMIO_VENDOR_ID, VENDOR_ID);
            self.l1.write(base + MMIO_QUEUE_NUM_MAX, QUEUE_NUM_MAX);
            // SW_IMPL=1: tell the patched kernel to use the
            // sel_generation handshake. See virtio.c::init_device.
            self.l1.write(base + MMIO_SW_IMPL, 1);
        }

        fn poll_one_device(&mut self, slot: u32) {
            let base = slot_regs_base(slot);
            // STATUS
            let status = self.l1.read(base + MMIO_STATUS);
            let status_prev = self.l1.read(snap_addr(slot, SNAP_OFF_STATUS));
            if status != status_prev {
                self.handle_status_change(slot, status);
                self.l1.write(snap_addr(slot, SNAP_OFF_STATUS), status);
            }
            // QUEUE_SEL
            let sel = self.l1.read(base + MMIO_QUEUE_SEL);
            let sel_prev = self.l1.read(snap_addr(slot, SNAP_OFF_QUEUE_SEL));
            if sel != sel_prev {
                self.handle_queue_sel_change(slot, sel);
                self.l1.write(snap_addr(slot, SNAP_OFF_QUEUE_SEL), sel);
            }
            // QUEUE_NOTIFY
            let notify = self.l1.read(base + MMIO_QUEUE_NOTIFY);
            let notify_prev = self.l1.read(snap_addr(slot, SNAP_OFF_QUEUE_NOTIFY));
            if notify != notify_prev {
                self.handle_queue_notify(slot, notify);
                self.l1
                    .write(snap_addr(slot, SNAP_OFF_QUEUE_NOTIFY), notify);
            }
            // QUEUE_READY (uses the *current* SEL after the swap above)
            let sel_after = self.l1.read(base + MMIO_QUEUE_SEL);
            // Eager clear: persist any non-zero write to shadow then
            // zero the visible reg, so the next vm_setup_vq cycle
            // doesn't see stale READY=1 from the previous queue.
            // Mirrors virtio.c's QUEUE_READY handling.
            let ready = self.l1.read(base + MMIO_QUEUE_READY);
            if ready != 0 {
                self.handle_queue_ready_change(slot, sel_after, ready);
                self.l1.write(base + MMIO_QUEUE_READY, 0);
            }

            // Per-queue setup blind capture into shadow[sel] —
            // mirrors virtio.c's FIELDS loop.
            if sel_after < 8 {
                for (mmio_off, shadow_off) in [
                    (MMIO_QUEUE_NUM, SHADOW_Q_OFF_NUM),
                    (MMIO_QUEUE_DESC_LOW, SHADOW_Q_OFF_DESC_LO),
                    (MMIO_QUEUE_DESC_HIGH, SHADOW_Q_OFF_DESC_HI),
                    (MMIO_QUEUE_DRIVER_LOW, SHADOW_Q_OFF_DRIVER_LO),
                    (MMIO_QUEUE_DRIVER_HIGH, SHADOW_Q_OFF_DRIVER_HI),
                    (MMIO_QUEUE_DEVICE_LOW, SHADOW_Q_OFF_DEVICE_LO),
                    (MMIO_QUEUE_DEVICE_HIGH, SHADOW_Q_OFF_DEVICE_HI),
                ] {
                    let v = self.l1.read(base + mmio_off);
                    self.l1
                        .write(shadow_queue_addr(slot, sel_after, shadow_off), v);
                }
            }

            // sel_generation echo — last so all snapshots above are in
            // shadow before the kernel's spin-wait sees the bump.
            // Mirrors virtio.c's curr_gen != last_echoed branch.
            let curr_gen = self.l1.read(base + MMIO_SEL_GENERATION);
            let last_echoed = self.l1.read(snap_addr(slot, SNAP_OFF_SEL_GEN_ECHO));
            if curr_gen != last_echoed {
                let next = curr_gen.wrapping_add(1);
                self.l1.write(base + MMIO_SEL_GENERATION, next);
                self.l1.write(snap_addr(slot, SNAP_OFF_SEL_GEN_ECHO), next);
            }
        }

        fn handle_status_change(&mut self, slot: u32, status: u32) {
            if status == 0 {
                self.init_device(slot);
            }
            self.bump_stat(STATS_OFF_STATUS_CHANGES);
        }

        fn handle_queue_sel_change(&mut self, slot: u32, sel: u32) {
            // Out-of-range queue index: leave visible regs alone.
            if sel >= 8 {
                self.bump_stat(STATS_OFF_SEL_CHANGES);
                return;
            }
            let base = slot_regs_base(slot);
            let dev_idx = slot % DEVS_PER_L2CPU;
            let nq = num_queues_for_device(dev_idx);
            let num_max = if sel < nq { QUEUE_NUM_MAX } else { 0 };
            let num = self.l1.read(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_NUM));
            let ready = self
                .l1
                .read(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_READY));
            self.l1.write(base + MMIO_QUEUE_NUM_MAX, num_max);
            self.l1.write(base + MMIO_QUEUE_NUM, num);
            self.l1.write(base + MMIO_QUEUE_READY, ready);
            self.bump_stat(STATS_OFF_SEL_CHANGES);
        }

        fn handle_queue_ready_change(&mut self, slot: u32, sel: u32, ready: u32) {
            if sel >= 8 {
                self.bump_stat(STATS_OFF_READY_EVENTS);
                return;
            }
            self.l1
                .write(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_READY), ready);
            if ready == 0 {
                for off in [
                    SHADOW_Q_OFF_NUM,
                    SHADOW_Q_OFF_DESC_LO,
                    SHADOW_Q_OFF_DESC_HI,
                    SHADOW_Q_OFF_DRIVER_LO,
                    SHADOW_Q_OFF_DRIVER_HI,
                    SHADOW_Q_OFF_DEVICE_LO,
                    SHADOW_Q_OFF_DEVICE_HI,
                ] {
                    self.l1.write(shadow_queue_addr(slot, sel, off), 0);
                }
            }
            self.bump_stat(STATS_OFF_READY_EVENTS);
        }

        fn handle_queue_notify(&mut self, slot: u32, q: u32) {
            self.l1.write(
                STATS_BASE + STATS_OFF_LAST_NOTIFY,
                (slot << 16) | (q & 0xFFFF),
            );
            self.bump_stat(STATS_OFF_NOTIFY_EVENTS);
            self.kick_ring_push(slot, q);
        }

        /// Mirror of the firmware's `kick_ring_push` for the
        /// fullness-check + drop-counter behavior in #101. Returns
        /// true if the entry was appended; false if the ring was
        /// already at `KICK_RING_ENTRIES - 1` outstanding and we had
        /// to drop.
        pub fn kick_ring_push(&mut self, slot: u32, queue_idx: u32) -> bool {
            use crate::tensix_proto as proto;
            let producer = self.l1.read(
                proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_PRODUCER_SEQ,
            );
            let consumer = self.l1.read(
                proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_CONSUMER_SEQ,
            );
            if producer.wrapping_sub(consumer) >= proto::KICK_RING_ENTRIES {
                self.bump_stat(STATS_OFF_KICK_DROPS);
                return false;
            }
            let idx = producer & (proto::KICK_RING_ENTRIES - 1);
            let entry = proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING + idx * proto::KICK_ENTRY_SIZE;
            // SLOT field packs queue_idx into the high half — matches
            // the firmware's KICK_ENTRY_OFF_SLOT layout.
            self.l1.write(
                entry + proto::KICK_ENTRY_OFF_SLOT,
                (slot & 0xFFFF) | ((queue_idx & 0xFFFF) << 16),
            );
            self.l1.write(entry + proto::KICK_ENTRY_OFF_SEQ, producer);
            self.l1.write(
                proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_PRODUCER_SEQ,
                producer.wrapping_add(1),
            );
            true
        }

        fn bump_stat(&mut self, off: u32) {
            let cur = self.l1.read(STATS_BASE + off);
            self.l1.write(STATS_BASE + off, cur.wrapping_add(1));
        }

        /// Run a full poll sweep over all 16 slots — equivalent to
        /// one iteration of the firmware's outer loop.
        pub fn step(&mut self) {
            for slot in 0..NUM_SLOTS {
                self.poll_one_device(slot);
            }
        }

        // Convenience accessors used by tests.
        pub fn read(&self, addr: u32) -> u32 {
            self.l1.read(addr)
        }
        pub fn write(&mut self, addr: u32, value: u32) {
            self.l1.write(addr, value);
        }
        pub fn stat(&self, off: u32) -> u32 {
            self.l1.read(STATS_BASE + off)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_indices_match_layout() {
        assert_eq!(slot(0, DEV_BLK), 0);
        assert_eq!(slot(0, DEV_RNG), 3);
        assert_eq!(slot(1, DEV_BLK), 4);
        assert_eq!(slot(3, DEV_RNG), 15);
    }

    #[test]
    fn slot_regs_base_matches_layout() {
        assert_eq!(slot_regs_base(0), 0x10000);
        assert_eq!(slot_regs_base(1), 0x11000);
        assert_eq!(slot_regs_base(15), 0x1f000);
    }

    #[test]
    fn l2cpu_windows_are_disjoint_and_contiguous() {
        for cpu in 0..NUM_L2CPUS {
            let base = l2cpu_window_base(cpu);
            assert_eq!(base, REGS_BASE + cpu * PER_L2CPU_WINDOW_SIZE);
            let blk_slot_base = slot_regs_base(slot(cpu, DEV_BLK));
            assert_eq!(blk_slot_base, base);
            let rng_slot_base = slot_regs_base(slot(cpu, DEV_RNG));
            assert_eq!(rng_slot_base, base + 3 * REGS_PER_DEV);
        }
    }

    #[test]
    fn embedded_firmware_is_nonempty_and_aligned() {
        assert!(!VIRTIO_FIRMWARE.is_empty());
        // First 4 bytes must be the entry stub `j main_entry` from
        // start.S, encoded as `0x0800006f`. If this changes the M3
        // firmware probably broke its calling convention.
        let entry = u32::from_le_bytes([
            VIRTIO_FIRMWARE[0],
            VIRTIO_FIRMWARE[1],
            VIRTIO_FIRMWARE[2],
            VIRTIO_FIRMWARE[3],
        ]);
        assert_eq!(entry, 0x0800_006f);
    }

    // ----- Firmware-state-machine simulator tests (M3 #69) -----
    //
    // These exercise the same algorithm the on-chip C firmware
    // implements, in pure Rust. If the C firmware diverges from
    // this Rust mirror, `debug tensix-virtio` on hardware will
    // show different behavior than `cargo test`.

    use super::sim::VirtioFwSim;

    #[test]
    fn firmware_boot_plants_static_regs_for_every_slot() {
        let mut sim = VirtioFwSim::new();
        sim.boot();
        for s in 0..NUM_SLOTS {
            let base = slot_regs_base(s);
            assert_eq!(sim.read(base + MMIO_MAGIC_VALUE), MAGIC, "slot {} magic", s);
            assert_eq!(sim.read(base + MMIO_VERSION), VERSION);
            assert_eq!(sim.read(base + MMIO_VENDOR_ID), VENDOR_ID);
            assert_eq!(sim.read(base + MMIO_QUEUE_NUM_MAX), QUEUE_NUM_MAX);
            let dev_idx = s % DEVS_PER_L2CPU;
            assert_eq!(
                sim.read(base + MMIO_DEVICE_ID),
                device_id_for_index(dev_idx),
                "slot {} device_id",
                s
            );
        }
        assert_eq!(sim.stat(STATS_OFF_MAGIC), STATS_MAGIC_LOADED);
    }

    #[test]
    fn firmware_status_write_bumps_counter_once_per_change() {
        let mut sim = VirtioFwSim::new();
        sim.boot();
        let base = slot_regs_base(0);
        // Idle step — no change, counter stays put.
        sim.step();
        assert_eq!(sim.stat(STATS_OFF_STATUS_CHANGES), 0);
        // Guest writes ACK.
        sim.write(base + MMIO_STATUS, STATUS_ACKNOWLEDGE);
        sim.step();
        assert_eq!(sim.stat(STATS_OFF_STATUS_CHANGES), 1);
        // Idempotent write of the same value — snapshot-diff suppresses.
        sim.write(base + MMIO_STATUS, STATUS_ACKNOWLEDGE);
        sim.step();
        assert_eq!(sim.stat(STATS_OFF_STATUS_CHANGES), 1);
        // Different value — fires again.
        sim.write(base + MMIO_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);
        sim.step();
        assert_eq!(sim.stat(STATS_OFF_STATUS_CHANGES), 2);
    }

    #[test]
    fn firmware_status_zero_resets_device() {
        let mut sim = VirtioFwSim::new();
        sim.boot();
        let base = slot_regs_base(0);
        // Drive the device through the full negotiation — values
        // visible-as-MMIO drift from their post-init state.
        sim.write(base + MMIO_STATUS, STATUS_ACKNOWLEDGE);
        sim.step();
        sim.write(
            base + MMIO_STATUS,
            STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
        );
        sim.step();
        // Mutate a "guest-controllable" reg too so the reset has
        // something to clean up.
        sim.write(base + MMIO_QUEUE_NUM, 32);
        sim.step();
        // Now reset.
        sim.write(base + MMIO_STATUS, 0);
        sim.step();
        // Static regs are re-planted; mutated reg is back to 0.
        assert_eq!(sim.read(base + MMIO_MAGIC_VALUE), MAGIC);
        assert_eq!(sim.read(base + MMIO_DEVICE_ID), VIRTIO_ID_BLOCK);
        assert_eq!(sim.read(base + MMIO_QUEUE_NUM), 0);
    }

    #[test]
    fn firmware_queue_sel_swap_updates_visible_regs() {
        let mut sim = VirtioFwSim::new();
        sim.boot();
        // Use the net slot — 2 queues, so SEL=1 is a valid in-range
        // queue we can swap to.
        let net = slot(0, DEV_NET);
        let base = slot_regs_base(net);
        // Initial SEL=0 → QUEUE_NUM_MAX should be QUEUE_NUM_MAX. We
        // verify the SEL semantics via observable side effects
        // (visible MMIO regs + counter); the per-queue shadow values
        // start at zero post-boot, so the visible regs after a swap
        // reflect "SEL points to a queue that hasn't been configured
        // yet" rather than per-queue uniqueness.
        assert_eq!(sim.read(base + MMIO_QUEUE_NUM_MAX), QUEUE_NUM_MAX);
        // Swap to queue 1 — still in range for net.
        sim.write(base + MMIO_QUEUE_SEL, 1);
        sim.step();
        assert_eq!(sim.read(base + MMIO_QUEUE_NUM_MAX), QUEUE_NUM_MAX);
        assert_eq!(sim.stat(STATS_OFF_SEL_CHANGES), 1);
        // Swap to queue 2 — out of range for net (only 2 queues).
        // Firmware reports QUEUE_NUM_MAX=0 to tell the guest "no
        // such queue."
        sim.write(base + MMIO_QUEUE_SEL, 2);
        sim.step();
        assert_eq!(sim.read(base + MMIO_QUEUE_NUM_MAX), 0);
        assert_eq!(sim.stat(STATS_OFF_SEL_CHANGES), 2);
        // Swap back to queue 0.
        sim.write(base + MMIO_QUEUE_SEL, 0);
        sim.step();
        assert_eq!(sim.read(base + MMIO_QUEUE_NUM_MAX), QUEUE_NUM_MAX);
        assert_eq!(sim.stat(STATS_OFF_SEL_CHANGES), 3);
    }

    #[test]
    fn firmware_queue_notify_records_slot_and_queue() {
        let mut sim = VirtioFwSim::new();
        sim.boot();
        let net = slot(2, DEV_NET); // L2CPU 2's net device
        let base = slot_regs_base(net);
        sim.write(base + MMIO_QUEUE_NOTIFY, 1);
        sim.step();
        let last = sim.stat(STATS_OFF_LAST_NOTIFY);
        assert_eq!(last, (net << 16) | 1);
        assert_eq!(sim.stat(STATS_OFF_NOTIFY_EVENTS), 1);
    }

    #[test]
    fn firmware_kick_ring_push_writes_entry_and_bumps_producer() {
        use crate::tensix_proto as proto;
        let mut sim = VirtioFwSim::new();
        sim.boot();
        // First push lands at producer=0, idx=0.
        assert!(sim.kick_ring_push(7, 1));
        let producer = sim.read(
            proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_PRODUCER_SEQ,
        );
        assert_eq!(producer, 1);
        let entry0 = proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING;
        let raw = sim.read(entry0 + proto::KICK_ENTRY_OFF_SLOT);
        assert_eq!(raw & 0xFFFF, 7);
        assert_eq!(raw >> 16, 1);
        // Drop counter stays at 0 on a healthy push.
        assert_eq!(sim.stat(STATS_OFF_KICK_DROPS), 0);
    }

    #[test]
    fn firmware_kick_ring_push_drops_when_ring_full() {
        // #101: when (producer - consumer) >= KICK_RING_ENTRIES the
        // firmware drops the kick instead of overwriting an unread
        // entry, and bumps STATS_OFF_KICK_DROPS so the daemon can
        // surface the pressure.
        use crate::tensix_proto as proto;
        let mut sim = VirtioFwSim::new();
        sim.boot();
        // Fill the ring without advancing the consumer.
        for i in 0..proto::KICK_RING_ENTRIES {
            assert!(
                sim.kick_ring_push(0, i),
                "iteration {} should fit in the ring",
                i
            );
        }
        assert_eq!(sim.stat(STATS_OFF_KICK_DROPS), 0);

        // The next push must drop.
        assert!(!sim.kick_ring_push(0, 99));
        assert_eq!(sim.stat(STATS_OFF_KICK_DROPS), 1);
        let producer = sim.read(
            proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_PRODUCER_SEQ,
        );
        // Producer doesn't advance on a drop.
        assert_eq!(producer, proto::KICK_RING_ENTRIES);

        // Advancing the consumer makes room again.
        sim.write(
            proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_CONSUMER_SEQ,
            proto::KICK_RING_ENTRIES / 2,
        );
        for i in 0..proto::KICK_RING_ENTRIES / 2 {
            assert!(
                sim.kick_ring_push(0, 100 + i),
                "post-drain push {} should fit",
                i
            );
        }
        // Drop count unchanged — only the original overflow.
        assert_eq!(sim.stat(STATS_OFF_KICK_DROPS), 1);
    }

    #[test]
    fn firmware_queue_ready_eager_clear_keeps_shadow_set() {
        // The eager-clear behavior the firmware adopted (M5.5e, #71)
        // makes READY a write-only signal from the kernel: any non-
        // zero write persists to shadow and the visible reg is
        // immediately zeroed for the next vm_setup_vq cycle. Verify
        // that side effect: the kernel sees READY=0 even after
        // writing 1, but shadow stays at 1 until a STATUS=0 reset.
        let mut sim = VirtioFwSim::new();
        sim.boot();
        let blk = slot(0, DEV_BLK);
        let base = slot_regs_base(blk);
        sim.write(base + MMIO_QUEUE_READY, 1);
        sim.step();
        assert_eq!(sim.stat(STATS_OFF_READY_EVENTS), 1);
        // Visible cleared back to 0 — the next vm_setup_vq guard read
        // sees 0 and proceeds.
        assert_eq!(sim.read(base + MMIO_QUEUE_READY), 0);
        // Shadow still holds 1 — dispatch_chain uses this to know the
        // queue is configured.
        assert_eq!(sim.read(shadow_queue_addr(blk, 0, SHADOW_Q_OFF_READY)), 1);
    }

    #[test]
    fn firmware_init_sets_sw_impl() {
        // Without SW_IMPL=1, the patched kernel skips the
        // sel_generation handshake and hits the SEL→READY race
        // (#58/#61/#63/#65). Verify init_device plants the bit.
        let mut sim = VirtioFwSim::new();
        sim.boot();
        for slot_idx in 0..NUM_SLOTS {
            let base = slot_regs_base(slot_idx);
            assert_eq!(
                sim.read(base + MMIO_SW_IMPL),
                1,
                "slot {} missing SW_IMPL=1",
                slot_idx
            );
        }
    }

    #[test]
    fn firmware_sel_generation_echo_increments_on_kernel_bump() {
        // The kernel writes prev+1 and spins until the daemon writes
        // back a different value. The firmware's "echo" rule: if
        // visible.sel_gen != snap.last_echoed, write curr+1 and
        // update snap. Verify one round-trip.
        let mut sim = VirtioFwSim::new();
        sim.boot();
        let blk = slot(0, DEV_BLK);
        let base = slot_regs_base(blk);
        // Kernel writes prev(0)+1 = 1.
        sim.write(base + MMIO_SEL_GENERATION, 1);
        sim.step();
        // Firmware echoes 2 — kernel sees != 1 and exits its spin.
        assert_eq!(sim.read(base + MMIO_SEL_GENERATION), 2);
        // Idempotent on next step (no kernel write).
        sim.step();
        assert_eq!(sim.read(base + MMIO_SEL_GENERATION), 2);
        // Next round: kernel reads 2 as new prev, writes prev+1=3.
        sim.write(base + MMIO_SEL_GENERATION, 3);
        sim.step();
        assert_eq!(sim.read(base + MMIO_SEL_GENERATION), 4);
    }

    #[test]
    fn firmware_per_queue_blind_capture_handles_repeated_high_half() {
        // Regression for the M5.5e bug: snapshot-diff missed queue 1's
        // DESC_HIGH=0x40 after queue 0 wrote the same 0x40, leaving
        // queue 1's shadow.DESC_HI=0 and dispatch_chain dropping
        // virtio-net's TX. The fix is blind capture every iteration.
        let mut sim = VirtioFwSim::new();
        sim.boot();
        let net = slot(0, DEV_NET);
        let base = slot_regs_base(net);
        // Queue 0 setup: SEL=0 + DESC_HIGH=0x40
        sim.write(base + MMIO_QUEUE_SEL, 0);
        sim.write(base + MMIO_QUEUE_DESC_HIGH, 0x40);
        sim.write(base + MMIO_QUEUE_DESC_LOW, 0x1000);
        sim.step();
        assert_eq!(
            sim.read(shadow_queue_addr(net, 0, SHADOW_Q_OFF_DESC_HI)),
            0x40
        );
        assert_eq!(
            sim.read(shadow_queue_addr(net, 0, SHADOW_Q_OFF_DESC_LO)),
            0x1000
        );
        // Queue 1 setup: SEL=1 + DESC_HIGH=0x40 (SAME value as queue 0)
        // + DESC_LOW=0x2000 (different).
        sim.write(base + MMIO_QUEUE_SEL, 1);
        sim.write(base + MMIO_QUEUE_DESC_HIGH, 0x40);
        sim.write(base + MMIO_QUEUE_DESC_LOW, 0x2000);
        sim.step();
        // Both halves must land in queue 1's shadow even though
        // DESC_HIGH didn't change between the two setups.
        assert_eq!(
            sim.read(shadow_queue_addr(net, 1, SHADOW_Q_OFF_DESC_HI)),
            0x40
        );
        assert_eq!(
            sim.read(shadow_queue_addr(net, 1, SHADOW_Q_OFF_DESC_LO)),
            0x2000
        );
    }

    #[test]
    fn firmware_no_cross_slot_interference() {
        // A write to slot 0 must not bump slot 5's counters.
        let mut sim = VirtioFwSim::new();
        sim.boot();
        let s0 = slot(0, DEV_BLK);
        let s5 = slot(1, DEV_NET);
        sim.write(slot_regs_base(s0) + MMIO_STATUS, STATUS_ACKNOWLEDGE);
        sim.step();
        // s5's regs are still in their post-boot state.
        let s5_base = slot_regs_base(s5);
        assert_eq!(sim.read(s5_base + MMIO_STATUS), 0);
        assert_eq!(sim.read(s5_base + MMIO_MAGIC_VALUE), MAGIC);
        // status_changes is global (one counter for the whole
        // firmware), but slot-attribution only happens via
        // LAST_NOTIFY for QUEUE_NOTIFY events; for STATUS the
        // identity of the changing slot is implicit in the
        // surrounding I/O context.
    }
}
