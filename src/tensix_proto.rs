// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Host-side mirror of the daemon ↔ BRISC wire protocol (M5, #71).
//!
//! The C firmware-side header lives at
//! `brisc-firmware/include/tensix_proto.h`. Both sides MUST keep
//! `PROTOCOL_VERSION` and the byte layouts in lockstep — a daemon
//! talking to a firmware with a different protocol version refuses
//! to boot, with a clear error message naming both versions.
//!
//! Module-wide `#![allow(dead_code)]` — kick / completion ring offset
//! constants are kept named for future use even when the current host
//! path doesn't read all of them.
#![allow(dead_code)]
//!
//! The protocol has three phases:
//!
//!   1. **Handshake.** Daemon writes [`HelloMsg`] to L1, BRISC
//!      writes [`HelloAckMsg`] back. Daemon validates and latches
//!      the firmware version + completion-ring address.
//!   2. **Kick FIFO** (BRISC → daemon). On a guest `QUEUE_NOTIFY`,
//!      BRISC appends a [`KickEntry`] to a host-RAM SPSC ring
//!      (allocated via `HostDmaBuf`) and bumps a producer counter.
//!      Daemon polls the counter and drains entries.
//!   3. **Completion ring** (daemon → BRISC). After the daemon
//!      finishes a descriptor chain, it appends a
//!      [`CompletionEntry`] to an SPSC ring in BRISC L1 and bumps
//!      the producer. BRISC's poll loop sweeps the ring and pokes
//!      the relevant L2CPU's PLIC IRQ line.
//!
//! Why both sides are SPSC-and-not-MPSC: the daemon serializes its
//! data-plane work through the existing virtio worker model; BRISC
//! is single-threaded by construction. The control-plane registers
//! that motivated #66 are deterministic on-chip and don't need ring
//! buffers; this module is only the data plane.

/// Protocol version. Bump on any wire-incompatible change.
///
/// v1 = M5 (#71) virtio kick ring + completion ring.
/// v2 = M6 (#78) extended the kick-ring slot encoding so slots 16..19
/// carried per-L2CPU UART TX bytes (one byte per 16-byte kick entry —
/// later found to overflow the 64-entry ring on boot bursts, see #79).
/// v3 = M6.1 (#79) splits UART traffic off the kick ring entirely.
/// TRISC0 produces bytes into per-L2CPU feed rings in BRISC L1, and
/// the daemon polls those rings directly through the chip-side TLB
/// (4 bytes per slot, 1024 slots per ring, one ring per L2CPU). The
/// kick ring is virtio-only at v3, original 64-entry layout.
/// v4 (#81) extends DEVS_PER_L2CPU from 4 to 8 (6 populated + 2
/// padding for power-of-two modulo) and shifts SHADOW_BASE from
/// 0x20000 to 0x40000 to clear the larger reg-file region. A v3
/// daemon talking to v4 firmware (or vice versa) reads/writes shadow
/// state at the wrong address — the kick poller silently drops every
/// kick. Warm-resume on a `firmware_version` mismatch must refuse to
/// adopt and force a fresh firmware load.
pub const PROTOCOL_VERSION: u32 = 4;

// ----- L1 control-plane region (BRISC L1) -----
//
// First-cut M5: kick ring lives in BRISC L1, daemon polls via the
// existing chip-side TLB. A future optimization could move it to a
// host-RAM `HostDmaBuf` so the daemon polls native memory (requires
// BRISC NoC-write capability via the NIU register interface).

pub const CTRL_BASE: u32 = 0x0000_5000;
pub const CTRL_SIZE: u32 = 0x0000_1000;

pub const CTRL_OFF_HELLO: u32 = 0x0000;
pub const CTRL_OFF_HELLO_ACK: u32 = 0x0040;
pub const CTRL_OFF_KICK_RING_HDR: u32 = 0x0080;
pub const CTRL_OFF_ACTIVE_SLOTS: u32 = 0x00C0;
pub const CTRL_OFF_KICK_RING: u32 = 0x0100;
pub const CTRL_OFF_COMPL_RING_HDR: u32 = 0x0500;
pub const CTRL_OFF_COMPL_RING: u32 = 0x0600;

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

// ----- Kick ring header offsets (within CTRL_OFF_KICK_RING_HDR) -----

pub const KICK_HDR_OFF_PRODUCER_SEQ: u32 = 0x00;
pub const KICK_HDR_OFF_CONSUMER_SEQ: u32 = 0x04;
pub const KICK_HDR_OFF_RING_ENTRIES: u32 = 0x08;

// ----- Completion ring header offsets (within CTRL_OFF_COMPL_RING_HDR) -----

pub const COMPL_HDR_OFF_PRODUCER_SEQ: u32 = 0x00;
pub const COMPL_HDR_OFF_CONSUMER_SEQ: u32 = 0x04;
pub const COMPL_HDR_OFF_RING_ENTRIES: u32 = 0x08;

// ----- KickEntry layout (BRISC → daemon, in host RAM ring) -----

pub const KICK_ENTRY_SIZE: u32 = 16;
pub const KICK_ENTRY_OFF_SLOT: u32 = 0x00;
pub const KICK_ENTRY_OFF_QUEUE_IDX: u32 = 0x02;
pub const KICK_ENTRY_OFF_SEQ: u32 = 0x04;
pub const KICK_ENTRY_OFF_EPOCH: u32 = 0x08;

// ----- CompletionEntry layout (daemon → BRISC, in BRISC L1 ring) -----

pub const COMPL_ENTRY_SIZE: u32 = 16;
pub const COMPL_ENTRY_OFF_SLOT: u32 = 0x00;
pub const COMPL_ENTRY_OFF_QUEUE_IDX: u32 = 0x02;
pub const COMPL_ENTRY_OFF_USED_IDX: u32 = 0x04;

// ----- Ring sizing -----

pub const KICK_RING_ENTRIES: u32 = 64;
pub const COMPL_RING_ENTRIES: u32 = 64;

// Compile-time invariants.
const _PROTO_INVARIANTS: () = {
    // Kick + completion rings + headers fit inside the control-plane
    // region.
    assert!(CTRL_OFF_KICK_RING + KICK_RING_ENTRIES * KICK_ENTRY_SIZE <= CTRL_OFF_COMPL_RING_HDR);
    assert!(CTRL_OFF_COMPL_RING + COMPL_RING_ENTRIES * COMPL_ENTRY_SIZE <= CTRL_SIZE);
    // Power-of-two ring sizes — important if we add a wrap-mask
    // optimization later.
    assert!(KICK_RING_ENTRIES.is_power_of_two());
    assert!(COMPL_RING_ENTRIES.is_power_of_two());
};

/// Strongly-typed kick entry, mirroring `KickEntry` in the C header.
/// Used for unit-testable serialization/deserialization tests; the
/// hot path on both sides reads/writes the offset-keyed u16/u32
/// fields directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct KickEntry {
    pub slot: u16,
    pub queue_idx: u16,
    pub seq: u32,
    pub epoch: u32,
    pub reserved: u32,
}

impl KickEntry {
    pub fn to_le_bytes(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..2].copy_from_slice(&self.slot.to_le_bytes());
        out[2..4].copy_from_slice(&self.queue_idx.to_le_bytes());
        out[4..8].copy_from_slice(&self.seq.to_le_bytes());
        out[8..12].copy_from_slice(&self.epoch.to_le_bytes());
        out[12..16].copy_from_slice(&self.reserved.to_le_bytes());
        out
    }

    pub fn from_le_bytes(b: &[u8; 16]) -> Self {
        KickEntry {
            slot: u16::from_le_bytes([b[0], b[1]]),
            queue_idx: u16::from_le_bytes([b[2], b[3]]),
            seq: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            epoch: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            reserved: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct CompletionEntry {
    pub slot: u16,
    pub queue_idx: u16,
    pub used_idx: u32,
    pub reserved0: u32,
    pub reserved1: u32,
}

impl CompletionEntry {
    pub fn to_le_bytes(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..2].copy_from_slice(&self.slot.to_le_bytes());
        out[2..4].copy_from_slice(&self.queue_idx.to_le_bytes());
        out[4..8].copy_from_slice(&self.used_idx.to_le_bytes());
        out[8..12].copy_from_slice(&self.reserved0.to_le_bytes());
        out[12..16].copy_from_slice(&self.reserved1.to_le_bytes());
        out
    }

    pub fn from_le_bytes(b: &[u8; 16]) -> Self {
        CompletionEntry {
            slot: u16::from_le_bytes([b[0], b[1]]),
            queue_idx: u16::from_le_bytes([b[2], b[3]]),
            used_idx: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            reserved0: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            reserved1: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        }
    }
}

// Lock the wire-format protocol version against the firmware. A bump to
// PROTOCOL_VERSION must be matched on both sides simultaneously.
const _PROTOCOL_VERSION_PINNED: () = assert!(PROTOCOL_VERSION == 4);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_words_decode_to_ascii() {
        assert_eq!(HELLO_MAGIC.to_le_bytes(), *b"HELO");
        assert_eq!(HELLO_ACK_MAGIC.to_le_bytes(), *b"ACK!");
    }

    #[test]
    fn kick_entry_round_trips() {
        let e = KickEntry {
            slot: 5,
            queue_idx: 2,
            seq: 0xDEAD_BEEF,
            epoch: 7,
            reserved: 0,
        };
        let b = e.to_le_bytes();
        assert_eq!(KickEntry::from_le_bytes(&b), e);
    }

    #[test]
    fn kick_entry_field_offsets_match_header() {
        // The host-side reader parses entries by raw offset; verify
        // the offsets the C header uses match the Rust struct layout.
        let e = KickEntry {
            slot: 0xAAAA,
            queue_idx: 0xBBBB,
            seq: 0xCCCC_CCCC,
            epoch: 0xDDDD_DDDD,
            reserved: 0xEEEE_EEEE,
        };
        let b = e.to_le_bytes();
        assert_eq!(
            u16::from_le_bytes([b[0], b[1]]),
            e.slot,
            "slot at offset {}",
            KICK_ENTRY_OFF_SLOT
        );
        assert_eq!(u16::from_le_bytes([b[2], b[3]]), e.queue_idx);
        assert_eq!(KICK_ENTRY_OFF_QUEUE_IDX, 2);
        assert_eq!(KICK_ENTRY_OFF_SEQ, 4);
        assert_eq!(KICK_ENTRY_OFF_EPOCH, 8);
    }

    #[test]
    fn completion_entry_round_trips() {
        let e = CompletionEntry {
            slot: 9,
            queue_idx: 0,
            used_idx: 42,
            reserved0: 0,
            reserved1: 0,
        };
        assert_eq!(CompletionEntry::from_le_bytes(&e.to_le_bytes()), e);
    }

    // Layout invariants — all inputs are `const`, so this is a
    // compile-time check. `assert!` would trigger
    // `clippy::assertions_on_constants`; `const { assert!(...) }`
    // fails the build instead, which is what we want.
    const _CTRL_LAYOUT_INVARIANTS: () = {
        assert!(CTRL_OFF_HELLO < CTRL_OFF_HELLO_ACK);
        assert!(CTRL_OFF_HELLO_ACK < CTRL_OFF_KICK_RING_HDR);
        assert!(CTRL_OFF_KICK_RING_HDR < CTRL_OFF_KICK_RING);
        assert!(
            CTRL_OFF_KICK_RING + KICK_RING_ENTRIES * KICK_ENTRY_SIZE <= CTRL_OFF_COMPL_RING_HDR
        );
        assert!(CTRL_OFF_COMPL_RING_HDR < CTRL_OFF_COMPL_RING);
        assert!(CTRL_OFF_COMPL_RING + COMPL_RING_ENTRIES * COMPL_ENTRY_SIZE <= CTRL_SIZE);
    };
}
