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
// v2 = M6 (#78) extends the kick-ring slot encoding to 32 slots so
// slots 16..19 carry per-L2CPU 16550 UART TX bytes (with the byte in
// the queue_idx field). Wire layout of KickEntry is unchanged; only
// the slot enumeration grew. A daemon talking to a v1 firmware
// refuses to boot, so old-daemon-vs-new-firmware can't end up
// silently dropping UART kicks.
#define TENSIX_PROTOCOL_VERSION 2u

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
#define CTRL_SIZE                 0x00001000u

#define CTRL_OFF_HELLO            0x0000u
#define CTRL_OFF_HELLO_ACK        0x0040u
#define CTRL_OFF_KICK_RING_HDR    0x0080u
// 16-bit bitmap (low 16 bits used) of active virtio slots. Daemon
// sets the bit when register_slot is called for that slot, clears
// when unregister_slot. BRISC's main poll loop skips slots whose
// bit is 0 — cuts the per-slot sweep cost on a single-L2CPU boot
// (4 active vs 16 total) so the sweep period drops far enough
// to reliably win the SEL→READY race against stock kernels that
// don't have the SW_IMPL handshake.
#define CTRL_OFF_ACTIVE_SLOTS     0x00C0u
#define CTRL_OFF_KICK_RING        0x0100u
#define CTRL_OFF_COMPL_RING_HDR   0x0500u
#define CTRL_OFF_COMPL_RING       0x0600u

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
#define KICK_RING_ENTRIES        64u   // 64 × 16 = 1 KiB at CTRL_OFF_KICK_RING
#define COMPL_RING_ENTRIES       64u   // 64 × 16 = 1 KiB at CTRL_OFF_COMPL_RING

#endif  // BRISC_TENSIX_PROTO_H
