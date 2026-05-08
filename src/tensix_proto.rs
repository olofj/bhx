// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Host-side mirror of the daemon ↔ BRISC wire protocol.
//!
//! The C firmware-side header lives at
//! `brisc-firmware/include/tensix_proto.h`. Both sides MUST keep
//! `PROTOCOL_VERSION` and the byte layouts in lockstep — a daemon
//! talking to a firmware with a different protocol version refuses
//! to boot, with a clear error message naming both versions.
//!
//! The protocol has three phases (V2, #187 / #188 / #189):
//!
//!   1. **Handshake.** Daemon writes [`HelloMsg`] to L1, BRISC writes
//!      [`HelloAckMsg`] back. Daemon validates the protocol version
//!      and latches the firmware version.
//!   2. **Dirty bitmap** (BRISC → daemon). On every guest
//!      `QUEUE_NOTIFY`, BRISC sets the per-(slot, queue) byte at
//!      [`CTRL_OFF_DIRTY`] to 1. The daemon poll loop reads + clears
//!      and dispatches via `dispatch_chain`. Level-sensitive — burst
//!      NOTIFYs coalesce into a single set byte and cannot overflow.
//!   3. **Processed cursor** (daemon → BRISC). After each successful
//!      dispatch the daemon publishes the post-commit `used.idx`
//!      into [`CTRL_OFF_PROCESSED`]. A freshly-spawned daemon reads
//!      cursors directly on warm-resume instead of probing guest
//!      DRAM, so it doesn't re-deliver chains the previous daemon
//!      already committed.

/// Protocol version. Bump on any wire-incompatible change.
///
/// v1 = M5 (#71) virtio kick ring + completion ring.
/// v2 = M6 (#78) extended the kick-ring slot encoding so slots 16..19
/// carried per-L2CPU UART TX bytes — overflowed the 64-entry ring on
/// boot bursts (#79).
/// v3 = M6.1 (#79) split UART traffic off the kick ring (TRISC0 feed
/// rings in BRISC L1).
/// v4 (#81) extended DEVS_PER_L2CPU 4 → 8 and shifted SHADOW_BASE
/// 0x20000 → 0x40000 to clear the larger reg-file region.
/// v5 (#188) replaces the V1 kick + completion rings with the V2
/// per-queue dirty bitmap ([`CTRL_OFF_DIRTY`]) and processed-cursor
/// table ([`CTRL_OFF_PROCESSED`]). The cold-start handshake stays at
/// HELLO/HELLO_ACK; the version bump is the daemon's signal to drain
/// via bitmap instead of kick-ring consumer.
pub const PROTOCOL_VERSION: u32 = 5;

// ----- L1 control-plane region (BRISC L1) -----

pub const CTRL_BASE: u32 = 0x0000_5000;
/// 4 KiB CTRL region. V2's full footprint is ~1.5 KiB; the rest is
/// reserved for future state-log / counters / lifecycle bits. CTRL
/// must end before `BRISC_VIRTIO_REGS_BASE = 0x10000`.
pub const CTRL_SIZE: u32 = 0x0000_1000;

pub const CTRL_OFF_HELLO: u32 = 0x0000;
pub const CTRL_OFF_HELLO_ACK: u32 = 0x0040;
pub const CTRL_OFF_ACTIVE_SLOTS: u32 = 0x00C0;
pub const CTRL_OFF_ACTIVE_VIRTIO_SLOTS: u32 = 0x00C4;

/// Mirrors `BRISC_VIRTIO_MAX_QUEUES` from `virtio_layout.h`. The
/// per-(slot, queue) DIRTY / PROCESSED arrays size against this.
pub const MAX_QUEUES_PER_SLOT: u32 = 8;
/// `u8[NUM_SLOTS][MAX_QUEUES_PER_SLOT]`. BRISC writes 1 on every
/// guest QUEUE_NOTIFY; daemon reads + clears each pass.
pub const CTRL_OFF_DIRTY: u32 = 0x0100;
/// `u16[NUM_SLOTS][MAX_QUEUES_PER_SLOT]`. Daemon writes the post-
/// dispatch `used.idx` so warm-resume reads cursors directly
/// instead of probing guest DRAM.
pub const CTRL_OFF_PROCESSED: u32 = 0x0200;
/// First byte after the V2 layout. Anything between
/// `CTRL_OFF_END` and `CTRL_SIZE` is reserved for future fields;
/// adding one bumps `PROTOCOL_VERSION` so a mismatched daemon ↔
/// firmware pair refuses to attach loudly.
pub const CTRL_OFF_END: u32 = 0x0400;

// ----- Magic words (written last in each side's slot) -----

pub const HELLO_MAGIC: u32 = 0x4F4C_4548; // "HELO" little-endian
pub const HELLO_ACK_MAGIC: u32 = 0x214B_4341; // "ACK!" little-endian

// ----- HelloMsg slot offsets (within CTRL_OFF_HELLO) -----

pub const HELLO_OFF_PROTOCOL_VERSION: u32 = 0x00;
pub const HELLO_OFF_MAGIC: u32 = 0x04;

// ----- HelloAckMsg slot offsets (within CTRL_OFF_HELLO_ACK) -----

pub const HELLO_ACK_OFF_PROTOCOL_VERSION: u32 = 0x00;
pub const HELLO_ACK_OFF_FIRMWARE_VERSION: u32 = 0x04;
pub const HELLO_ACK_OFF_MAGIC: u32 = 0x08;

// Layout invariants. `NUM_SLOTS` is hard-coded as the literal 32 to
// keep this module self-contained (the firmware-side mirror in
// `tensix_proto.h` uses `BRISC_VIRTIO_NUM_SLOTS` from
// `virtio_layout.h`); a separate cross-module assert in
// `crate::virtio_engine` pins both values to match
// `virtio_engine::NUM_SLOTS`.
const _NUM_SLOTS: u32 = 32;
const _LAYOUT_INVARIANTS: () = {
    // DIRTY: 1 byte per (slot, queue), placed at 0x0100.
    assert!(CTRL_OFF_DIRTY + _NUM_SLOTS * MAX_QUEUES_PER_SLOT <= CTRL_OFF_PROCESSED);
    // PROCESSED: 2 bytes per (slot, queue), placed at 0x0200.
    assert!(CTRL_OFF_PROCESSED + _NUM_SLOTS * MAX_QUEUES_PER_SLOT * 2 <= CTRL_OFF_END);
    // The V2 region (HELLO/HELLO_ACK + ACTIVE bitmaps + DIRTY +
    // PROCESSED) fits within CTRL_SIZE with the rest reserved for
    // future fields.
    assert!(CTRL_OFF_END <= CTRL_SIZE);
    // Pin handshake offsets — both sides hard-code 0x0000 / 0x0040
    // in the hello path.
    assert!(CTRL_OFF_HELLO == 0x0000);
    assert!(CTRL_OFF_HELLO_ACK == 0x0040);
};

// Lock the wire-format protocol version against the firmware. A bump
// to PROTOCOL_VERSION must be matched on both sides simultaneously.
const _PROTOCOL_VERSION_PINNED: () = assert!(PROTOCOL_VERSION == 5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_words_decode_to_ascii() {
        assert_eq!(HELLO_MAGIC.to_le_bytes(), *b"HELO");
        assert_eq!(HELLO_ACK_MAGIC.to_le_bytes(), *b"ACK!");
    }

    /// Pinned literals catch accidental edits to any layout constant;
    /// a future revision that legitimately moves an offset updates
    /// this test along with the const definition.
    #[test]
    fn layout_constants_pinned_to_expected_values() {
        assert_eq!(MAX_QUEUES_PER_SLOT, 8);
        assert_eq!(CTRL_OFF_DIRTY, 0x0100);
        assert_eq!(CTRL_OFF_PROCESSED, 0x0200);
        assert_eq!(CTRL_OFF_END, 0x0400);
    }

    #[test]
    fn handshake_offsets_pinned() {
        assert_eq!(CTRL_OFF_HELLO, 0x0000);
        assert_eq!(CTRL_OFF_HELLO_ACK, 0x0040);
    }
}
