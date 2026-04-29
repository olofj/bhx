// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Host-side mirror of the M6 (#78) 16550 UART layout in
//! `brisc-firmware/include/uart_layout.h`.
//!
//! One UART per L2CPU. Reg file lives in BRISC L1 starting at
//! `BASE = 0x40000` with a 16 KiB stride; the existing engine TLB
//! window covers it at offset `OFFSET_FROM_ENGINE_BASE = 0x30000`
//! from each L2CPU's window base, so we don't program a second TLB
//! slot. The kick ring is shared with virtio: slots 16..19 carry per-
//! L2CPU TX bytes, with the byte payload in the `queue_idx` field of
//! the existing `KickEntry`.
//!
//! TX-only on this side too — the daemon's kick consumer routes
//! `slot >= UART_SLOT_BASE` kicks into `console_hub::push_chip_output`.
//! RX is intentionally a future commit (see `uart_layout.h`).

/// Per-L2CPU stride between UART reg files in BRISC L1.
pub const UART_PER_L2CPU_STRIDE: u32 = 0x0000_4000;
/// Reg-file size visible to the guest (4 KiB; only the low ~32 bytes
/// hold real registers, the rest is zeroed).
pub const UART_REG_FILE_SIZE: u32 = 0x0000_1000;
/// Offset from each L2CPU's engine-TLB window base to its UART reg
/// file. Daemon adds this to the engine `x280_base` to get the L2CPU
/// PA for the DTB `reg` property.
pub const UART_OFFSET_FROM_ENGINE_BASE: u32 = 0x0003_0000;

/// Kick-ring slot enumeration: virtio occupies 0..16, UART claims
/// 16..20, one per L2CPU.
pub const UART_SLOT_BASE: u16 = 16;
pub const UART_NUM_SLOTS: u16 = 4;

/// Convenience: the kick-ring slot for L2CPU `idx`'s UART.
#[inline]
pub fn slot_for_l2cpu(l2cpu_idx: u8) -> u16 {
    UART_SLOT_BASE + l2cpu_idx as u16
}

/// Inverse: returns the L2CPU index if `slot` is a UART slot, else
/// `None`.
#[inline]
pub fn l2cpu_for_slot(slot: u16) -> Option<u8> {
    if (UART_SLOT_BASE..UART_SLOT_BASE + UART_NUM_SLOTS).contains(&slot) {
        Some((slot - UART_SLOT_BASE) as u8)
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
        assert_eq!(slot_for_l2cpu(0), 16);
        assert_eq!(slot_for_l2cpu(3), 19);
        assert_eq!(l2cpu_for_slot(16), Some(0));
        assert_eq!(l2cpu_for_slot(19), Some(3));
        assert_eq!(l2cpu_for_slot(15), None);
        assert_eq!(l2cpu_for_slot(20), None);
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
    // `UART_PRIV_OFF_FEED_RING`.
    const _FEED_RING_FITS: () = {
        let ring_size = UART_FEED_RING_ENTRIES * 4;
        assert!(UART_PRIV_OFF_FEED_RING + ring_size <= UART_PRIVATE_PER_L2CPU);
        assert!(UART_FEED_RING_ENTRIES.is_power_of_two());
    };

    // TRISC0 globals must live past the last per-L2CPU slot (4 ×
    // 0x100 = 0x400) and not collide with anything else. Compile-time
    // check.
    const _TRISC0_GLOBALS_AFTER_PER_L2CPU: () = {
        assert!(
            TRISC0_GLOBAL_BASE
                >= UART_PRIVATE_BASE + (UART_NUM_SLOTS as u32) * UART_PRIVATE_PER_L2CPU
        );
    };

    #[test]
    fn feed_ring_layout_matches_firmware() {
        // Spot-check the offsets the firmware uses so a future move
        // here forces a sync.
        assert_eq!(UART_PRIV_OFF_HOLD, 0x00);
        assert_eq!(UART_PRIV_OFF_FEED_PRODUCER_SEQ, 0x04);
        assert_eq!(UART_PRIV_OFF_FEED_CONSUMER_SEQ, 0x08);
        assert_eq!(UART_PRIV_OFF_FEED_DROP_COUNT, 0x0C);
        assert_eq!(UART_PRIV_OFF_FEED_RING, 0x40);
        assert_eq!(UART_FEED_RING_ENTRIES, 1024);
    }

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
