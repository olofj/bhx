// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT
//
// Wire protocol between the host daemon and the BRISC virtio engine
// firmware. Shared between the C firmware and the Rust host crate
// (`src/tensix_proto.rs`); both sides MUST stay in sync, and both
// sides bump `TENSIX_PROTOCOL_VERSION` together on any observable
// change.
//
// Protocol overview (V2, #187 / #188 / #189):
//
//   1. Boot-time handshake:
//      - Daemon writes a `HelloMsg` to L1 at `CTRL_OFF_HELLO`,
//        finishing with a non-zero magic word.
//      - BRISC polls for the magic, reads the rest, then writes a
//        `HelloAckMsg` to L1 at `CTRL_OFF_HELLO_ACK`.
//      - Daemon polls for the hello-ack magic and validates the
//        protocol version. Mismatch refuses to boot the L2CPU; the
//        firmware version moves independently for diagnostics, the
//        protocol version is the load-bearing contract.
//
//   2. Steady state — dirty bitmap (BRISC → daemon):
//      - On every guest QUEUE_NOTIFY MMIO write, BRISC sets the
//        per-(slot, queue) byte at
//        `CTRL_OFF_DIRTY + slot * MAX_QUEUES_PER_SLOT + q` to 1.
//      - Daemon polls the bitmap each pass, clears every set byte,
//        and dispatches via `dispatch_chain` (avail-ring walk +
//        descriptor processor + used-ring commit + PLIC IRQ).
//      - The bitmap is level-sensitive — concurrent NOTIFY storms
//        coalesce into a single set byte, so the dispatch path
//        cannot fall behind.
//
//   3. Steady state — processed cursor (daemon → BRISC):
//      - After each successful dispatch, the daemon publishes the
//        post-commit `used.idx` into
//        `CTRL_OFF_PROCESSED + slot * MAX_QUEUES_PER_SLOT * 2 + q*2`.
//      - On warm-resume, a freshly-spawned daemon reads the cursor
//        directly instead of probing guest DRAM, so it doesn't
//        re-deliver chains the previous daemon already committed.
//
// L1 control-plane region layout:
//
//   `CTRL_BASE = 0x0000_5000`, `CTRL_SIZE = 0x0000_1000` (4 KiB).
//
//     0x5000 .. 0x5040    HelloMsg slot
//     0x5040 .. 0x5080    HelloAckMsg slot
//     0x50C0 .. 0x50C8    Active-slots bitmaps (full + virtio-only)
//     0x5100 .. 0x5200    DIRTY array (NUM_SLOTS × MAX_QUEUES_PER_SLOT bytes)
//     0x5200 .. 0x5400    PROCESSED array (NUM_SLOTS × MAX_QUEUES_PER_SLOT × 2 bytes)
//     0x5400 .. 0x6000    Reserved for future fields (bump
//                          TENSIX_PROTOCOL_VERSION when adding any).

#ifndef BRISC_TENSIX_PROTO_H
#define BRISC_TENSIX_PROTO_H

#include <stdint.h>

#include "virtio_layout.h"  // for BRISC_VIRTIO_NUM_SLOTS / _MAX_QUEUES

// Protocol version. v1 = M5 (#71) virtio kick ring + completion ring.
// v2 = M6 (#78) extended the kick-ring slot encoding so slots 16..19
// carried per-L2CPU 16550 UART TX bytes — overflowed the 64-entry
// ring during stock-distro boot bursts.
// v3 = M6.1 (#79) split UART traffic off the kick ring (TRISC0 feed
// rings in BRISC L1).
// v4 (#81) extended DEVS_PER_L2CPU 4 → 8 and shifted SHADOW_BASE
// 0x20000 → 0x40000 to clear the larger reg-file region.
// v5 (#188) replaces the kick + completion rings with the V2
// per-queue dirty bitmap + processed-cursor table at
// `CTRL_OFF_DIRTY` / `CTRL_OFF_PROCESSED`. Cold-start handshake
// path is unchanged; the version bump is the daemon's signal to
// drain via bitmap instead of kick-ring consumer.
#define TENSIX_PROTOCOL_VERSION 5u

// L1 control-plane region (within the BRISC firmware tile's L1).
// 4 KiB is more than enough for the V2 layout (~1.5 KiB used) with
// room for a future state-log / counters / lifecycle bits.
#define CTRL_BASE                 0x00005000u
#define CTRL_SIZE                 0x00001000u

#define CTRL_OFF_HELLO            0x0000u
#define CTRL_OFF_HELLO_ACK        0x0040u
// u32 bitmap of "active" slots. Daemon sets the bit on
// register_slot (virtio), register_uart (UART), and shutdown
// registry add; clears on unregister. BRISC's main poll loop reads
// this for kick / shutdown / UART lifecycle dispatch.
//
// With NUM_L2CPUS=4 and DEVS_PER_L2CPU=8 the virtio slot space is
// 0..32, which OVERLAPS UART slots (16..20) and shutdown slots
// (20..24). For dispatch this is fine, but TRISC1's race-watch
// loop must distinguish virtio slots from UART/shutdown bits —
// `CTRL_OFF_ACTIVE_VIRTIO_SLOTS` carries virtio bits only.
#define CTRL_OFF_ACTIVE_SLOTS         0x00C0u
#define CTRL_OFF_ACTIVE_VIRTIO_SLOTS  0x00C4u

// V2 dispatch arrays. DIRTY is u8[NUM_SLOTS][MAX_QUEUES_PER_SLOT];
// BRISC writes 1 on every QUEUE_NOTIFY, daemon reads + clears each
// pass. PROCESSED is u16[NUM_SLOTS][MAX_QUEUES_PER_SLOT]; daemon
// writes the post-dispatch `used.idx` so warm-resume reads cursors
// directly. NUM_SLOTS=32, MAX_QUEUES_PER_SLOT=8 → DIRTY=256 B,
// PROCESSED=512 B.
#define MAX_QUEUES_PER_SLOT       BRISC_VIRTIO_MAX_QUEUES
#define CTRL_OFF_DIRTY            0x0100u
#define CTRL_OFF_PROCESSED        0x0200u
// First byte after the V2 layout. 0x0400 .. CTRL_SIZE is reserved
// for future fields; bump TENSIX_PROTOCOL_VERSION on any addition.
#define CTRL_OFF_END              0x0400u

// Magic words. Both sides write the magic *last* in their respective
// slot so a partial write is observable as "not yet ready" by the
// peer.
#define HELLO_MAGIC               0x4F4C4548u  // "HELO" little-endian
#define HELLO_ACK_MAGIC           0x214B4341u  // "ACK!" little-endian

// HelloMsg layout (8 useful bytes, rest of the 64-byte slot zero):
//   u32 protocol_version
//   u32 magic   (HELLO_MAGIC; written last)
#define HELLO_OFF_PROTOCOL_VERSION  0x00u
#define HELLO_OFF_MAGIC             0x04u

// HelloAckMsg layout (12 useful bytes):
//   u32 protocol_version
//   u32 firmware_version
//   u32 magic   (HELLO_ACK_MAGIC; written last)
#define HELLO_ACK_OFF_PROTOCOL_VERSION  0x00u
#define HELLO_ACK_OFF_FIRMWARE_VERSION  0x04u
#define HELLO_ACK_OFF_MAGIC             0x08u

// ----- Compile-time invariants -----
//
// Mirrored against `_LAYOUT_INVARIANTS` in `src/tensix_proto.rs`.
// Cross-module check in `src/virtio_engine.rs` pins NUM_SLOTS to
// 32 so the array sizing here stays in lockstep.
_Static_assert(
    CTRL_OFF_DIRTY + BRISC_VIRTIO_NUM_SLOTS * MAX_QUEUES_PER_SLOT
        <= CTRL_OFF_PROCESSED,
    "DIRTY array overflows into PROCESSED");
_Static_assert(
    CTRL_OFF_PROCESSED + BRISC_VIRTIO_NUM_SLOTS * MAX_QUEUES_PER_SLOT * 2u
        <= CTRL_OFF_END,
    "PROCESSED array overflows the V2 region");
_Static_assert(
    CTRL_OFF_END <= CTRL_SIZE,
    "V2 region overflows CTRL_SIZE");
// CTRL must end before BRISC_VIRTIO_REGS_BASE (0x10000). A bump
// that overflows CTRL_SIZE silently aliases CTRL onto the virtio
// reg files. Hard-coded value matches the constant in
// virtio_layout.h; cross-header references are clumsier than
// restating the literal.
_Static_assert(
    CTRL_BASE + CTRL_SIZE <= 0x00010000u,
    "CTRL region overlaps BRISC_VIRTIO_REGS_BASE (0x10000)");
// Pin handshake offsets — the daemon and firmware both hard-code
// 0x0000 / 0x0040 in the hello path.
_Static_assert(CTRL_OFF_HELLO == 0x0000u, "HELLO offset moved");
_Static_assert(CTRL_OFF_HELLO_ACK == 0x0040u, "HELLO_ACK offset moved");

#endif  // BRISC_TENSIX_PROTO_H
