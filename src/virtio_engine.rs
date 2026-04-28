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
pub const VERSION: u32 = 2;
pub const VENDOR_ID: u32 = 0x5554_4254; // "TBTU" — keep in sync with virtio.c

pub const VIRTIO_ID_NET: u32 = 1;
pub const VIRTIO_ID_BLOCK: u32 = 2;
pub const VIRTIO_ID_CONSOLE: u32 = 3;
pub const VIRTIO_ID_ENTROPY: u32 = 4;

pub const QUEUE_NUM_MAX: u32 = 64;

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

pub const STATS_MAGIC_LOADED: u32 = 0x0000_B155;

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

    #[test]
    fn magic_constant_is_virt_little_endian() {
        // 'v' | 'i'<<8 | 'r'<<16 | 't'<<24 = 0x74726976
        assert_eq!(MAGIC, u32::from_le_bytes([b'v', b'i', b'r', b't']));
    }
}
