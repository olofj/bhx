// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT
//
// Wire protocol between the host daemon and the BRISC virtio engine
// firmware (M5, #71). Shared between the C firmware and the Rust
// host crate (`src/tensix_proto.rs`); both sides MUST stay in sync,
// and both sides bump `TENSIX_PROTOCOL_VERSION` together on any
// observable change.
//
// Protocol overview:
//
//   1. Boot-time handshake:
//      - Daemon writes a `HelloMsg` to L1 at `CTRL_OFF_HELLO`,
//        finishing with a non-zero magic word.
//      - BRISC polls for the magic, reads the rest, then writes a
//        `HelloAckMsg` to L1 at `CTRL_OFF_HELLO_ACK`.
//      - Daemon polls for the hello-ack magic, validates protocol
//        version, latches `firmware_version` and
//        `l1_completion_fifo_addr` for later use.
//      - Mismatch on protocol version → daemon refuses to boot. The
//        firmware version moves independently for diagnostics; the
//        protocol version is the load-bearing contract.
//
//   2. Steady state — kick FIFO (BRISC → daemon):
//      - Whenever BRISC observes a guest QUEUE_NOTIFY write, it
//        appends a `KickEntry` to the host's kick-FIFO ring (at the
//        NoC address `daemon_kick_fifo_noc_addr` from the hello)
//        and bumps the ring's producer counter.
//      - Daemon polls the producer counter; on advance, drains
//        entries from `[consumer..producer)`, processes the
//        descriptor chain via the existing virtio infra
//        (`src/virtio/`), and increments `consumer`.
//
//   3. Steady state — completion ring (daemon → BRISC):
//      - When the daemon finishes a descriptor chain it appends a
//        `CompletionEntry` to a ring at L1 `CTRL_OFF_COMPL_RING`
//        and bumps the producer counter.
//      - BRISC's poll loop sweeps the producer counter; on advance,
//        consumes entries and pokes the relevant L2CPU's PLIC IRQ
//        line so the guest's virtio IRQ handler runs.
//
// L1 control-plane region layout (extends the M3 #69 layout):
//
//   `CTRL_BASE = 0x0000_5000`, 4 KiB total.
//
//     0x5000 .. 0x5040    HelloMsg slot (64 bytes — 16 for fields,
//                          rest reserved/zero)
//     0x5040 .. 0x5080    HelloAckMsg slot
//     0x5080 .. 0x5100    Kick FIFO control (size + producer / consumer
//                          *shadow*; the canonical pointers live in
//                          host RAM — these are diagnostic copies BRISC
//                          maintains)
//     0x5100 .. 0x5200    Completion ring header (producer u32,
//                          consumer u32, plus reserved)
//     0x5200 .. 0x5C00    Completion ring entries
//                          (CTRL_COMPL_RING_ENTRIES × 16 bytes)
//
// The kick FIFO lives in host RAM (allocated via tt-kmd's
// IOCTL_ALLOCATE_DMA_BUF with NOC_DMA), reachable from BRISC via
// the iATU outbound region tt-kmd programs alongside the buffer.
// Layout in host RAM:
//
//     [ producer_seq: u32 ] [ consumer_seq: u32 ] [ ring_size: u32 ] [ rsvd: u32 ]
//     [ entries: KickEntry × KICK_FIFO_ENTRIES ]
//
// `producer_seq` is a monotonically-increasing counter (no wrap
// until u32::MAX, which at one kick per microsecond gives ~71 min
// before wrap — plenty for any single boot but not infinity, see
// the Wire-protocol notes below). `consumer_seq` lags behind.

#ifndef BRISC_TENSIX_PROTO_H
#define BRISC_TENSIX_PROTO_H

#include <stdint.h>

// Protocol version. v1 = M5 (#71) virtio kick ring + completion ring.
// v2 = M6 (#78) extended the kick-ring slot encoding so slots 16..19
// carried per-L2CPU 16550 UART TX bytes — one byte per 16-byte kick
// entry, which overflowed the 64-entry ring during stock-distro boot
// bursts.
// v3 = M6.1 (#79) splits UART traffic off the kick ring entirely:
// TRISC0 produces bytes into a per-L2CPU 1024-entry SPSC feed ring at
// `BRISC_UART_PRIVATE_BASE + idx*0x2000` (see uart_layout.h), and the
// daemon polls those rings directly through the chip-side TLB. BRISC
// is no longer in the UART data path. Kick ring stays virtio-only at
// its original 64-entry size and layout.
// v4 (#81) extends DEVS_PER_L2CPU from 4 to 8 (6 populated + 2
// padding for power-of-two modulo) and shifts SHADOW_BASE from
// 0x20000 to 0x40000 to clear the larger reg-file region. A v3
// daemon talking to v4 firmware (or vice versa) reads/writes shadow
// state at the wrong address — every kick gets silently dropped.
#define TENSIX_PROTOCOL_VERSION 4u

// L1 control-plane region (within the BRISC firmware tile's L1).
//
// First-cut M5: the kick FIFO (BRISC → daemon) lives in BRISC L1
// rather than host RAM. The daemon polls L1 via the existing chip-
// side TLB, same loop pattern as the chip-DRAM virtio path. A
// future optimization will move the kick FIFO to a host-RAM
// `HostDmaBuf` so the daemon polls native memory; that change
// requires BRISC firmware to issue NoC writes via the NIU register
// interface (`0xFFB2_0000+`), which we punt on for now in favor of
// landing the basic mechanism. See #71.
#define CTRL_BASE                 0x00005000u
// 36 KiB control region. Bumped from 16 KiB when KICK_RING_ENTRIES
// grew 512 → 2048 (32 KiB ring); previous 16 KiB region couldn't
// hold the 32 KiB kick ring at all. L1 budget allows up to 44 KiB
// for the CTRL region (0x10000 - 0x5000); we use 36 KiB, leaving
// ~8 KiB headroom for future header/control growth. Going larger
// would require relocating REGS_BASE or moving the kick ring into
// host DRAM via NoC writes.
#define CTRL_SIZE                 0x00009000u

#define CTRL_OFF_HELLO            0x0000u
#define CTRL_OFF_HELLO_ACK        0x0040u
#define CTRL_OFF_KICK_RING_HDR    0x0080u
// u32 bitmap of "active" slots. Daemon sets the bit on
// register_slot (virtio), register_uart (UART), and shutdown
// registry add; clears on unregister. BRISC's main poll loop reads
// this for kick / shutdown / UART lifecycle dispatch.
//
// With NUM_L2CPUS=4 and DEVS_PER_L2CPU=8 the virtio slot space is
// 0..32, which OVERLAPS UART slots (16..20) and shutdown slots
// (20..24). For dispatch this is fine — a kick on slot N decodes
// against the unified registry — but TRISC1's race-watch loop must
// distinguish virtio slots from UART/shutdown bits, otherwise it
// either skips L2CPU 2/3's actual virtio devices (when masked out
// as "UART range") or clobbers UART/shutdown reg files (when
// unmasked). `CTRL_OFF_ACTIVE_VIRTIO_SLOTS` carries virtio bits
// only — TRISC1 reads it.
#define CTRL_OFF_ACTIVE_SLOTS         0x00C0u
#define CTRL_OFF_ACTIVE_VIRTIO_SLOTS  0x00C4u
#define CTRL_OFF_KICK_RING        0x0100u  // ends at 0x8100 (2048 × 16)
#define CTRL_OFF_COMPL_RING_HDR   0x8100u
#define CTRL_OFF_COMPL_RING       0x8200u  // ends at 0x8600 (64 × 16)

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

// Kick ring header (in L1 at CTRL_OFF_KICK_RING_HDR; ring entries
// follow at CTRL_OFF_KICK_RING):
//   u32 producer_seq    (BRISC writes; monotonic, wraps at u32::MAX)
//   u32 consumer_seq    (daemon writes; monotonic)
//   u32 ring_entries    (constant; BRISC publishes on init)
//   u32 reserved
#define KICK_HDR_OFF_PRODUCER_SEQ     0x00u
#define KICK_HDR_OFF_CONSUMER_SEQ     0x04u
#define KICK_HDR_OFF_RING_ENTRIES     0x08u
#define KICK_HDR_OFF_RESERVED         0x0Cu

// Completion ring header (in L1 at CTRL_OFF_COMPL_RING_HDR):
//   u32 producer_seq    (daemon writes; monotonic)
//   u32 consumer_seq    (BRISC writes; monotonic)
//   u32 ring_entries    (constant; BRISC publishes on init)
//   u32 reserved
#define COMPL_HDR_OFF_PRODUCER_SEQ    0x00u
#define COMPL_HDR_OFF_CONSUMER_SEQ    0x04u
#define COMPL_HDR_OFF_RING_ENTRIES    0x08u
#define COMPL_HDR_OFF_RESERVED        0x0Cu

// Per-entry sizes (in bytes; both sides allocate buffers as
// `entries × sizeof(*Entry)`).
//
// KickEntry — BRISC → daemon:
//   u16 slot          (0..15: l2cpu_idx*4 + device_idx)
//   u16 queue_idx     (the QUEUE_NOTIFY value)
//   u32 seq           (monotonic per entry, equals producer_seq at
//                      the moment of write — useful for catching
//                      lost writes via gap detection)
//   u32 epoch         (incremented when BRISC sees STATUS=0 on a
//                      device — daemon uses this to invalidate
//                      stale kicks for a device that just got reset)
//   u32 reserved
#define KICK_ENTRY_SIZE       16u
#define KICK_ENTRY_OFF_SLOT       0x00u
#define KICK_ENTRY_OFF_QUEUE_IDX  0x02u
#define KICK_ENTRY_OFF_SEQ        0x04u
#define KICK_ENTRY_OFF_EPOCH      0x08u
#define KICK_ENTRY_OFF_RESERVED   0x0Cu

// CompletionEntry — daemon → BRISC:
//   u16 slot          (0..15)
//   u16 queue_idx
//   u32 used_idx      (the new used_ring index; BRISC echoes via PLIC IRQ)
//   u32 reserved[2]
#define COMPL_ENTRY_SIZE        16u
#define COMPL_ENTRY_OFF_SLOT      0x00u
#define COMPL_ENTRY_OFF_QUEUE_IDX 0x02u
#define COMPL_ENTRY_OFF_USED_IDX  0x04u

// Ring sizing. Powers of 2 so wrap is a mask.
// 2048 entries = 32 KiB. Bumped from 512 (8 KiB) after disk-to-disk
// install workloads (e.g., openSUSE NET ISO → empty target image)
// overflowed the 512-entry ring with bursts of 600-1300+ dropped
// kicks per overflow window (#184). 2048 is the practical max
// without restructuring the L1 layout: CTRL sits at 0x5000,
// REGS_BASE is at 0x10000, leaving 44 KiB for CTRL — and
// 32 KiB ring + 4 KiB headers/compl + slack just fits.
#define KICK_RING_ENTRIES        2048u  // 2048 × 16 = 32 KiB at CTRL_OFF_KICK_RING
#define COMPL_RING_ENTRIES       64u    // 64 × 16 = 1 KiB at CTRL_OFF_COMPL_RING

// ----- Compile-time invariants (firmware-side) -----
//
// Mirrored against `_PROTO_INVARIANTS` in `src/tensix_proto.rs`. If
// the daemon-side bump diverges from the firmware-side bump, one
// side's static asserts will fire. Every constant referenced here
// is a compile-time integer literal in this same file, so these
// resolve at preprocess time without dragging in other layout
// headers.

_Static_assert(
    CTRL_OFF_KICK_RING + KICK_RING_ENTRIES * KICK_ENTRY_SIZE <= CTRL_OFF_COMPL_RING_HDR,
    "kick ring overflows into completion ring header");
_Static_assert(
    CTRL_OFF_COMPL_RING + COMPL_RING_ENTRIES * COMPL_ENTRY_SIZE <= CTRL_SIZE,
    "completion ring overflows CTRL_SIZE");
// Power-of-two ring sizes — required so wrap is a mask AND, not a
// modulo (the bare-metal toolchain doesn't link __umodsi3).
_Static_assert(
    (KICK_RING_ENTRIES & (KICK_RING_ENTRIES - 1)) == 0,
    "KICK_RING_ENTRIES must be a power of two");
_Static_assert(
    (COMPL_RING_ENTRIES & (COMPL_RING_ENTRIES - 1)) == 0,
    "COMPL_RING_ENTRIES must be a power of two");
// Cross-region: CTRL must end before BRISC_VIRTIO_REGS_BASE
// (0x10000). A bump that overflows CTRL_SIZE silently aliases the
// kick ring onto the virtio reg files — exactly the kind of
// post-resize aliasing bug we want the compiler to catch. Hard-
// coded value matches the constant in virtio_layout.h; cross-
// header references are clumsier than restating the literal.
_Static_assert(
    CTRL_BASE + CTRL_SIZE <= 0x00010000u,
    "CTRL region overlaps BRISC_VIRTIO_REGS_BASE (0x10000)");

#endif  // BRISC_TENSIX_PROTO_H
