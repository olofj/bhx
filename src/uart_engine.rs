// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Host-side mirror of the M6 (#78) 16550 UART layout in
//! `brisc-firmware/include/uart_layout.h`.
//!
//! One UART per L2CPU. Reg file lives in BRISC L1 starting at
//! `BASE = 0x40000` with a 16 KiB stride; the existing engine TLB
//! window covers it at offset `OFFSET_FROM_ENGINE_BASE = 0x30000`
//! from each L2CPU's window base, so we don't program a second TLB
//! slot. The active-slots bitmap is shared with virtio: each L2CPU's
//! UART takes the in-window slot at offset
//! [`UART_SLOT_OFFSET_IN_L2CPU`] (= 6 of 8), so the kick-ring slots
//! land at 6, 14, 22, 30 — disjoint from every L2CPU's virtio
//! dev_idx range (0..6). See #175 for the bitmap collision this
//! layout fixed.
//!
//! Module-wide `#![allow(dead_code)]` — feed-ring offset constants are
//! kept named to mirror the firmware header even where the host path
//! doesn't currently read them.
#![allow(dead_code)]
//!
//! TX-only on this side too — the daemon drains each L2CPU's TRISC0
//! feed ring directly via the chip TLB and pushes bytes into
//! `console_hub::push_chip_output`. RX is intentionally a future
//! commit (see `uart_layout.h`).

use crate::virtio_engine::DEVS_PER_L2CPU;

/// Per-L2CPU stride between UART reg files in BRISC L1.
pub const UART_PER_L2CPU_STRIDE: u32 = 0x0000_4000;
/// Reg-file size visible to the guest (4 KiB; only the low ~32 bytes
/// hold real registers, the rest is zeroed).
pub const UART_REG_FILE_SIZE: u32 = 0x0000_1000;
/// Offset from each L2CPU's engine-TLB window base to its UART reg
/// file. Daemon adds this to the engine `x280_base` to get the L2CPU
/// PA for the DTB `reg` property.
pub const UART_OFFSET_FROM_ENGINE_BASE: u32 = 0x0003_0000;

/// Within a per-L2CPU 8-slot region, the index that carries the UART
/// kick. Slots 0..5 are virtio dev_idx (BLK / NET / CONSOLE / RNG /
/// BLK1 / BLK2); slot 6 is UART; slot 7 is reserved padding.
///
/// Mirrored as `BRISC_UART_SLOT_OFFSET_IN_L2CPU` in
/// `brisc-firmware/include/uart_layout.h`. Both sides must move in
/// lockstep.
pub const UART_SLOT_OFFSET_IN_L2CPU: u32 = 6;

/// Convenience: the kick-ring slot for L2CPU `idx`'s UART.
#[inline]
pub fn slot_for_l2cpu(l2cpu_idx: u8) -> u16 {
    (l2cpu_idx as u32 * DEVS_PER_L2CPU + UART_SLOT_OFFSET_IN_L2CPU) as u16
}

/// Inverse: returns the L2CPU index if `slot` is a UART slot, else
/// `None`. UART slots are exactly the slots whose in-L2CPU offset is
/// [`UART_SLOT_OFFSET_IN_L2CPU`].
#[inline]
pub fn l2cpu_for_slot(slot: u16) -> Option<u8> {
    let s = slot as u32;
    if s % DEVS_PER_L2CPU == UART_SLOT_OFFSET_IN_L2CPU
        && s / DEVS_PER_L2CPU < crate::virtio_engine::NUM_L2CPUS
    {
        Some((s / DEVS_PER_L2CPU) as u8)
    } else {
        None
    }
}

/// Compute the L2CPU PA of the UART reg file given the engine
/// window's `x280_base` for that same L2CPU. Used by `modify_dtb`
/// to emit the `ns16550a` node's `reg` property.
#[inline]
pub fn uart_pa_from_engine_base(engine_base: u64) -> u64 {
    engine_base + UART_OFFSET_FROM_ENGINE_BASE as u64
}

// ----- TRISC0 globals (M6.1, #79) — mirror of uart_layout.h -----
//
// Single per-tile block past the per-L2CPU UART private region. Used
// for state that doesn't belong to any one L2CPU: the TRISC0 heartbeat
// today, more diagnostics later as Phase B/C land.

pub const TRISC0_GLOBAL_BASE: u32 = 0x0005_8000;
pub const TRISC0_GLOBAL_OFF_HEARTBEAT: u32 = 0x00;

/// L1 address of the TRISC0 heartbeat slot (`bumped each iteration of
/// `trisc0_main`'s loop). The host reads this through the engine's
/// `read_l1_u32` to verify TRISC0 is alive.
#[inline]
pub fn trisc0_heartbeat_addr() -> u32 {
    TRISC0_GLOBAL_BASE + TRISC0_GLOBAL_OFF_HEARTBEAT
}

// ----- Per-L2CPU UART private region (M6.1, #79) -----
//
// Mirror of the layout in `brisc-firmware/include/uart_layout.h`.
// Each L2CPU gets a 256-byte slot with a tiny SPSC ring TRISC0 fills
// and BRISC drains.

pub const UART_PRIVATE_BASE: u32 = 0x0005_0000;
pub const UART_PRIVATE_PER_L2CPU: u32 = 0x0000_2000;
pub const UART_PRIV_OFF_HOLD: u32 = 0x00;
pub const UART_PRIV_OFF_FEED_PRODUCER_SEQ: u32 = 0x04;
pub const UART_PRIV_OFF_FEED_CONSUMER_SEQ: u32 = 0x08;
pub const UART_PRIV_OFF_FEED_DROP_COUNT: u32 = 0x0C;
pub const UART_PRIV_OFF_FEED_RING: u32 = 0x40;
pub const UART_FEED_RING_ENTRIES: u32 = 1024;

#[inline]
pub fn uart_private_base(l2cpu_idx: u8) -> u32 {
    UART_PRIVATE_BASE + (l2cpu_idx as u32) * UART_PRIVATE_PER_L2CPU
}

#[inline]
pub fn feed_drop_count_addr(l2cpu_idx: u8) -> u32 {
    uart_private_base(l2cpu_idx) + UART_PRIV_OFF_FEED_DROP_COUNT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_assignment_matches_firmware() {
        // Per-L2CPU offset 6 — the UART slot for L2CPU N is N*8+6.
        assert_eq!(slot_for_l2cpu(0), 6);
        assert_eq!(slot_for_l2cpu(1), 14);
        assert_eq!(slot_for_l2cpu(2), 22);
        assert_eq!(slot_for_l2cpu(3), 30);
        assert_eq!(l2cpu_for_slot(6), Some(0));
        assert_eq!(l2cpu_for_slot(14), Some(1));
        assert_eq!(l2cpu_for_slot(22), Some(2));
        assert_eq!(l2cpu_for_slot(30), Some(3));
        // Virtio slots in any L2CPU's range must NOT decode as UART
        // — these are the exact bits that #175 was about.
        for cpu in 0..4u32 {
            for dev_idx in 0..6u32 {
                assert_eq!(l2cpu_for_slot((cpu * 8 + dev_idx) as u16), None);
            }
            // dev_idx 7 is reserved padding; not UART.
            assert_eq!(l2cpu_for_slot((cpu * 8 + 7) as u16), None);
        }
    }

    // The engine TLB window is 2 MiB (small TLB default). UART at
    // 0x30000 from window base is well inside, with room past it
    // for future per-L2CPU additions. Compile-time check.
    const _UART_LIVES_WITHIN_ENGINE_WINDOW: () = {
        assert!(UART_OFFSET_FROM_ENGINE_BASE > 0x4000); // past virtio reg files
        assert!(UART_OFFSET_FROM_ENGINE_BASE + UART_REG_FILE_SIZE < 0x20_0000);
    };

    // The byte-feed ring + its headers must fit inside the per-L2CPU
    // 0x100-byte region. The ring alone is `entries × 4` (each cell
    // holds the byte in the low 8 bits of a u32) and starts at
    // `UART_PRIV_OFF_FEED_RING`. The header offsets that follow are
    // pinned to their wire-format values so a refactor here can't
    // drift from the firmware's matching constants.
    const _FEED_RING_FITS: () = {
        let ring_size = UART_FEED_RING_ENTRIES * 4;
        assert!(UART_PRIV_OFF_FEED_RING + ring_size <= UART_PRIVATE_PER_L2CPU);
        assert!(UART_FEED_RING_ENTRIES.is_power_of_two());
        assert!(UART_PRIV_OFF_HOLD == 0x00);
        assert!(UART_PRIV_OFF_FEED_PRODUCER_SEQ == 0x04);
        assert!(UART_PRIV_OFF_FEED_CONSUMER_SEQ == 0x08);
        assert!(UART_PRIV_OFF_FEED_DROP_COUNT == 0x0C);
        assert!(UART_PRIV_OFF_FEED_RING == 0x40);
        assert!(UART_FEED_RING_ENTRIES == 1024);
    };

    // TRISC0 globals must live past the last per-L2CPU slot (4 ×
    // 0x100 = 0x400) and not collide with anything else. Compile-time
    // check.
    const _TRISC0_GLOBALS_AFTER_PER_L2CPU: () = {
        assert!(
            TRISC0_GLOBAL_BASE
                >= UART_PRIVATE_BASE + crate::virtio_engine::NUM_L2CPUS * UART_PRIVATE_PER_L2CPU
        );
    };

    #[test]
    fn uart_private_base_strides_correctly() {
        assert_eq!(uart_private_base(0), 0x50000);
        assert_eq!(uart_private_base(3), 0x56000);
    }

    #[test]
    fn uart_pa_helper_offsets_correctly() {
        let engine_base = 0x4_3001_0000u64;
        assert_eq!(uart_pa_from_engine_base(engine_base), engine_base + 0x30000);
    }
}
