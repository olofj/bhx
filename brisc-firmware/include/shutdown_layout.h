// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT
//
// Guest-OS shutdown / reset signalling (#94). One register per L2CPU
// in BRISC L1, mapped into the L2CPU's existing engine TLB window so
// the guest sees a single 32-bit MMIO cell at a fixed offset. OpenSBI
// generic's `fdt_reset_syscon` driver writes a magic value to that
// cell on SBI SRST; BRISC observes the write, pushes a kick-ring entry
// with a reserved slot id, and the daemon tears down the slot.
//
// Why a separate register file rather than reusing a virtio slot:
// - The BRISC virtio slots are sel-multiplexed and feature-negotiated
//   on every probe; layering a side-channel on them complicates the
//   protocol invariants. A dedicated register is the simplest
//   contract — the guest writes one well-known address with a magic
//   value, BRISC sees it on next sweep, done.
// - OpenSBI's fdt_reset_syscon driver expects a single (regmap, offset,
//   value) triple. Pointing it at our virtio slot regs would
//   collide with the kernel's virtio probe ordering.
//
// Per-L2CPU placement. The stride MUST match the L2CPU TLB
// `PER_L2CPU_WINDOW_SIZE` (8 devices × 4 KiB regfile = 32 KiB), since
// each L2CPU's view of "engine_base + OFFSET_FROM_ENGINE_BASE" is
// computed by adding a fixed per-L2CPU `engine_base[idx]` (which
// itself uses 32 KiB stride from the TLB programming) to a single
// constant offset. If the BRISC-side stride here doesn't match the
// L2CPU-side stride, the kernel's syscon write for L2CPU N lands at
// L2CPU (N+1)'s register slot in BRISC L1 (or past the range entirely
// for higher idx).
//
//   L2CPU 0 shutdown reg → BRISC L1 0x60000  (engine_base[0] = 0x10000,
//                                              PA = engine_base + 0x50000)
//   L2CPU 1 shutdown reg → BRISC L1 0x68000  (engine_base[1] = 0x18000)
//   L2CPU 2 shutdown reg → BRISC L1 0x70000  (engine_base[2] = 0x20000)
//   L2CPU 3 shutdown reg → BRISC L1 0x78000  (engine_base[3] = 0x28000)

#ifndef BRISC_SHUTDOWN_LAYOUT_H
#define BRISC_SHUTDOWN_LAYOUT_H

#include <stdint.h>

#define BRISC_SHUTDOWN_BASE                      0x00060000u
#define BRISC_SHUTDOWN_PER_L2CPU_STRIDE          0x00008000u  // 32 KiB — must match L2CPU TLB PER_L2CPU_WINDOW_SIZE
#define BRISC_SHUTDOWN_REG_FILE_SIZE             0x00000010u  // 16 bytes (one u32 + slack)
#define BRISC_SHUTDOWN_OFFSET_FROM_ENGINE_BASE   0x00050000u

static inline uintptr_t brisc_shutdown_regs_base(unsigned l2cpu_idx) {
    return (uintptr_t)(BRISC_SHUTDOWN_BASE + l2cpu_idx * BRISC_SHUTDOWN_PER_L2CPU_STRIDE);
}

// The single u32 register in each L2CPU's shutdown reg file. Guest
// writes the magic value to request poweroff; BRISC observes and
// clears the cell back to 0 after firing the kick.
#define BRISC_SHUTDOWN_OFF_COMMAND               0x00u

// Magic values the guest writes to request a teardown. These match
// the `value = <...>;` field on the syscon-poweroff / syscon-reboot
// DT nodes emitted by boot::modify_dtb. Distinct values let BRISC
// distinguish poweroff from reboot when the reboot follow-up (#141)
// lands; today only POWEROFF fires.
#define BRISC_SHUTDOWN_MAGIC_POWEROFF            0x5AFEDEADu
#define BRISC_SHUTDOWN_MAGIC_REBOOT              0xB007BEEFu

// Sentinel meaning "no pending command." Initial state after BRISC
// boot-time wipe; written back after BRISC fires a kick.
#define BRISC_SHUTDOWN_SENTINEL                  0x00000000u

// Reserved slot ids for shutdown kicks. Disjoint from virtio
// (0..15) and UART (16..19); slots 20..23 are one per L2CPU. Daemon
// kick dispatcher decodes `l2cpu_idx = slot - BRISC_KICK_SHUTDOWN_SLOT_BASE`.
#define BRISC_KICK_SHUTDOWN_SLOT_BASE            20u
#define BRISC_KICK_SHUTDOWN_NUM_SLOTS            4u

// All 4 shutdown bits combined — used by BRISC to decide whether to
// poll any shutdown reg this sweep. If all clear, the inner loop skips
// the register reads entirely. Set/cleared by the daemon's
// publish_active_mask via the existing CTRL_OFF_ACTIVE_SLOTS bitmap.
#define BRISC_SHUTDOWN_SLOT_MASK \
    (((1u << BRISC_KICK_SHUTDOWN_NUM_SLOTS) - 1u) << BRISC_KICK_SHUTDOWN_SLOT_BASE)

#endif  // BRISC_SHUTDOWN_LAYOUT_H
