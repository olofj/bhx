// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT
//
// Shared layout header for the M3 (#69) Tensix-as-virtio-engine work.
//
// One Tensix tile serves *all* L2CPUs on the chip — each L2CPU points
// its own small TLB at a different slice of this same L1, so BRISC
// sees a unified 16-reg-file picture (4 L2CPUs × 4 devices) while
// each L2CPU sees only its 4 devices. See #66 for why one-tile-per-
// card was always the intent.
//
// L1 layout:
//   0x0000_0000 .. 0x0000_4000   firmware code (16 KiB max, see Makefile)
//   0x0000_4000 .. 0x0000_5000   stats page (M3.6, 4 KiB)
//   0x0001_0000 .. 0x0002_0000   virtio register files
//                                  16 slots × 4 KiB = 64 KiB
//                                  slot = l2cpu_idx*4 + device_idx
//                                    L2CPU 0 blk      → slot  0  → 0x10000
//                                    L2CPU 0 net      → slot  1  → 0x11000
//                                    L2CPU 0 console  → slot  2  → 0x12000
//                                    L2CPU 0 rng      → slot  3  → 0x13000
//                                    L2CPU 1 blk      → slot  4  → 0x14000
//                                    ...
//                                    L2CPU 3 rng      → slot 15  → 0x1F000
//   0x0002_0000 .. 0x0002_4000   per-queue shadow state (BRISC-private,
//                                  not visible-as-MMIO; M3.5)
//                                  16 × 1 KiB
//
// The 4 KiB per device matches virtio 1.2 §4.2.2's MMIO spec (last
// reg ConfigGeneration is at 0x0fc, config space starts at 0x100 and
// extends as needed for the device — 4 KiB is plenty).
//
// Why per-L2CPU reg files (instead of sharing one set of 4 across all
// L2CPUs)? Each L2CPU runs its own Linux guest with its own rootfs
// disk, network namespace, console — a guest's reads must not see
// another guest's queue state. The L2CPU's small TLB picks a 4 KiB
// (or 16 KiB, for all four devices) window into this region; the
// guest is unaware that other L2CPUs' devices are nearby in physical
// L1 space.
//
// The 64 KiB gap between code and reg files is intentional: M4 (#70)
// will retarget each L2CPU's small TLB at the right `BRISC_VIRTIO_REGS_BASE +
// l2cpu_idx*0x4000`, and we want that retargeting to land far from
// the firmware code so the guest can't accidentally clobber BRISC's
// instructions through a misaligned access.
//
// All offsets here are also mirrored as Rust constants in
// `src/virtio_engine.rs`. Keep the two in sync — there's a static
// assert on the Rust side that picks up the firmware-side magic via
// include_bytes! and verifies it's compiled with the same numbers.

#ifndef BRISC_VIRTIO_LAYOUT_H
#define BRISC_VIRTIO_LAYOUT_H

#include <stdint.h>

// ----- L1 layout -----

#define BRISC_VIRTIO_CODE_BASE      0x00000000u
#define BRISC_VIRTIO_CODE_SIZE      0x00004000u  // 16 KiB

#define BRISC_VIRTIO_STATS_BASE     0x00004000u
#define BRISC_VIRTIO_STATS_SIZE     0x00001000u  // 4 KiB

#define BRISC_VIRTIO_REGS_BASE      0x00010000u
#define BRISC_VIRTIO_REGS_PER_DEV   0x00001000u  // 4 KiB per device

#define BRISC_VIRTIO_NUM_L2CPUS     4u
#define BRISC_VIRTIO_DEVS_PER_L2CPU 4u
#define BRISC_VIRTIO_NUM_SLOTS      (BRISC_VIRTIO_NUM_L2CPUS * BRISC_VIRTIO_DEVS_PER_L2CPU)

// Each L2CPU's view of the reg files is a contiguous 16 KiB window
// (4 devices × 4 KiB). This is the size the L2CPU's small TLB
// retargets in M4.
#define BRISC_VIRTIO_PER_L2CPU_SIZE \
    (BRISC_VIRTIO_DEVS_PER_L2CPU * BRISC_VIRTIO_REGS_PER_DEV)

#define BRISC_VIRTIO_MAX_QUEUES     8u  // per device; enough for net (rx+tx)
                                        // + console (multiple ports) +
                                        // future expansion. Keeps the
                                        // shadow array small.

// ----- Device-index-within-L2CPU assignment -----
// Stable indices that match the existing `regs::virtio_mmio`
// reservation order in `src/regs.rs`. The full slot index for the
// reg file is `l2cpu_idx * BRISC_VIRTIO_DEVS_PER_L2CPU + device_idx`.
// Keeping this mapping fixed means the guest's DTB doesn't have to
// change across M3 → M4 → M5; only the physical destination of the
// L2CPU's MMIO TLB does.

#define BRISC_VIRTIO_DEV_BLK       0
#define BRISC_VIRTIO_DEV_NET       1
#define BRISC_VIRTIO_DEV_CONSOLE   2
#define BRISC_VIRTIO_DEV_RNG       3

static inline unsigned brisc_virtio_slot(unsigned l2cpu_idx, unsigned device_idx) {
    return l2cpu_idx * BRISC_VIRTIO_DEVS_PER_L2CPU + device_idx;
}

// ----- Virtio device IDs (virtio 1.2 §5) -----

#define VIRTIO_ID_NET           1u
#define VIRTIO_ID_BLOCK         2u
#define VIRTIO_ID_CONSOLE       3u
#define VIRTIO_ID_ENTROPY       4u

// ----- Virtio MMIO register offsets (virtio 1.2 §4.2.2) -----

#define VIRTIO_MMIO_MAGIC_VALUE         0x000  // R: "virt" (0x74726976)
#define VIRTIO_MMIO_VERSION             0x004  // R: 2 (modern)
#define VIRTIO_MMIO_DEVICE_ID           0x008  // R: per-device
#define VIRTIO_MMIO_VENDOR_ID           0x00c  // R: any
#define VIRTIO_MMIO_DEVICE_FEATURES     0x010  // R
#define VIRTIO_MMIO_DEVICE_FEATURES_SEL 0x014  // W
// 0x018: VIRTIO_MMIO_SW_IMPL — daemon-private, set to 1 to tell the
// patched kernel "this is a software virtio implementation; use the
// sel_generation handshake at 0x01c to wait for SEL writes to land
// before reading multiplexed regs." Without it the kernel hits the
// SEL race that motivated #58/#61/#63/#65 and surfaces probe -ENOENT
// on virtio_net's queue 1 setup.
#define VIRTIO_MMIO_SW_IMPL             0x018  // RW (daemon-private)
#define VIRTIO_MMIO_SEL_GENERATION      0x01c  // RW (handshake counter)
#define VIRTIO_MMIO_DRIVER_FEATURES     0x020  // W
#define VIRTIO_MMIO_DRIVER_FEATURES_SEL 0x024  // W
#define VIRTIO_MMIO_QUEUE_SEL           0x030  // W
#define VIRTIO_MMIO_QUEUE_NUM_MAX       0x034  // R
#define VIRTIO_MMIO_QUEUE_NUM           0x038  // W
#define VIRTIO_MMIO_QUEUE_READY         0x044  // RW
#define VIRTIO_MMIO_QUEUE_NOTIFY        0x050  // W
#define VIRTIO_MMIO_INTERRUPT_STATUS    0x060  // R
#define VIRTIO_MMIO_INTERRUPT_ACK       0x064  // W
#define VIRTIO_MMIO_STATUS              0x070  // RW
#define VIRTIO_MMIO_QUEUE_DESC_LOW      0x080  // W
#define VIRTIO_MMIO_QUEUE_DESC_HIGH     0x084  // W
#define VIRTIO_MMIO_QUEUE_DRIVER_LOW    0x090  // W (a.k.a. AVAIL)
#define VIRTIO_MMIO_QUEUE_DRIVER_HIGH   0x094  // W
#define VIRTIO_MMIO_QUEUE_DEVICE_LOW    0x0a0  // W (a.k.a. USED)
#define VIRTIO_MMIO_QUEUE_DEVICE_HIGH   0x0a4  // W
#define VIRTIO_MMIO_CONFIG_GENERATION   0x0fc  // R
#define VIRTIO_MMIO_CONFIG              0x100  // RW (device-specific)

// ----- Magic / version / vendor constants -----

#define VIRTIO_MMIO_MAGIC               0x74726976u  // "virt" little-endian
#define VIRTIO_MMIO_VERSION_2           2u
#define BRISC_VENDOR_ID                 0x55544254u  // "TBTU" little-endian

// Per-device queue counts. Conventional virtio:
//   blk      — 1 queue
//   net      — 2 queues (rx, tx) for the simple non-MQ case
//   console  — 2 queues (port 0: rx, tx)
//   rng      — 1 queue
// Bumped to 2 across the board for a uniform shadow-state shape; the
// daemon/guest is free to use only the queues it needs.
#define BRISC_VIRTIO_QUEUES_BLK         1u
#define BRISC_VIRTIO_QUEUES_NET         2u
#define BRISC_VIRTIO_QUEUES_CONSOLE     2u
#define BRISC_VIRTIO_QUEUES_RNG         1u

// Per-queue maximum descriptor count we advertise. Round number that
// any reasonable driver accepts; the actual value the driver picks
// is written back via QUEUE_NUM.
#define BRISC_VIRTIO_QUEUE_NUM_MAX      64u

// ----- Status bits (virtio 1.2 §2.1) -----

#define VIRTIO_STATUS_ACKNOWLEDGE       1u
#define VIRTIO_STATUS_DRIVER            2u
#define VIRTIO_STATUS_DRIVER_OK         4u
#define VIRTIO_STATUS_FEATURES_OK       8u
#define VIRTIO_STATUS_DEVICE_NEEDS_RESET 64u
#define VIRTIO_STATUS_FAILED            128u

// ----- Helpers used by the firmware -----

static inline uintptr_t brisc_virtio_regs_base(unsigned slot) {
    return (uintptr_t)(BRISC_VIRTIO_REGS_BASE + slot * BRISC_VIRTIO_REGS_PER_DEV);
}

static inline uintptr_t brisc_virtio_l2cpu_window_base(unsigned l2cpu_idx) {
    return (uintptr_t)(BRISC_VIRTIO_REGS_BASE
                       + l2cpu_idx * BRISC_VIRTIO_PER_L2CPU_SIZE);
}

#endif // BRISC_VIRTIO_LAYOUT_H
