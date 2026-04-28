// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT
//
// Hello-world BRISC firmware for issue #67 (M1). On entry it stamps
// a magic value at L1[0x40] and increments a 32-bit counter at
// L1[0x44] in a tight loop. The host poller reads these two words
// to confirm the BRISC is alive and executing.
//
// Why L1[0x40] and L1[0x44]?
//   * L1[0x00..0x10] holds tt-metal's BOOT_CODE_BASE / NOC_ATOMIC_RET_VAL
//     / L1_BARRIER conventions; we put the entry stub there, so the
//     stub overwrites those slots anyway.
//   * L1[0x10..0x20] is `MEM_L1_ARC_FW_SCRATCH` per the Blackhole
//     dev_mem_map — ARC firmware writes throttle state into that
//     range on every Tensix tile, so anything we leave there gets
//     stomped.
//   * L1[0x20..0x40] is `MEM_L1_INLINE_BASE` (4 inline-write slots).
//   * L1[0x40] is past every documented-reserved slot and well below
//     `MEM_MAILBOX_BASE` (0x60), so we're clear of every convention
//     the chip + tt-metal stack agree on.

#include <stdint.h>

#define L1_MAGIC_ADDR    0x00000040u
#define L1_COUNTER_ADDR  0x00000044u
#define L1_MAGIC_VALUE   0xA110C0DEu

// BRISC has a store queue in its Load/Store Unit that coalesces
// consecutive writes to the same address. In a tight increment loop,
// every iteration writes to L1[0x44] and the store queue collapses
// them — without a `fence`, the writes never reach L1 SRAM, so a
// host poller reading L1[0x44] over the NoC sees the pre-firmware
// initial value forever. (Documented in
// `BlackholeA0/TensixTile/BabyRISCV/MemoryOrdering.md`.) The single
// magic write at 0x40 escapes coalescing because the loop never
// touches that address again, and the cache line gets evicted.
//
// `FENCE_W` drains the store queue. Putting it inside the loop
// throttles the counter rate by ~10× — fine, the host samples at 1 s
// granularity and any nonzero advance proves BRISC is alive.
#define FENCE_W() __asm__ volatile("fence w, w" ::: "memory")

void main(void) {
    volatile uint32_t *magic   = (volatile uint32_t *)L1_MAGIC_ADDR;
    volatile uint32_t *counter = (volatile uint32_t *)L1_COUNTER_ADDR;

    *magic = L1_MAGIC_VALUE;
    FENCE_W();

    uint32_t c = 0;
    for (;;) {
        c += 1u;
        *counter = c;
        FENCE_W();
    }
}
