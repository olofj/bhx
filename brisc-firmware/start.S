// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

// Hello-world BRISC entry stub for issue #67 (M1).
//
// BRISC's reset PC is 0x0 in tile L1 (per
// `BlackholeA0/TensixTile/SoftReset.md`), so the linker places this
// section at the very start of L1. The first instruction jumps past
// the reserved magic+counter slots into `main_entry`, which sets up
// a tiny stack in BRISC's local data RAM (8 KiB at 0xFFB0_0000) and
// calls `main()` in `main.c`.
//
// Note: ARC firmware uses the L1[0x10..0x20] scratch range on every
// tile for power-throttle state, so our magic+counter must NOT be in
// that range. We put them at L1[0x40..0x48], well past anything ARC
// or tt-metal touches in a freshly-reset tile.

    .section .text._start, "ax"
    .global  _start
_start:
    j        main_entry         // PC=0: jump past the reserved area.

    // Pad up to 0x40 so the magic+counter slots land at fixed offsets.
    // We stash the magic at 0x40 and the counter at 0x44 so the host
    // poller knows where to look. main() writes both; we leave them as
    // zeros in the binary so a fresh load is unambiguously "not yet
    // run."
    .org 0x40
    .global _l1_magic
_l1_magic:
    .word    0
    .global _l1_counter
_l1_counter:
    .word    0

    // Aligned entry. Anything past here is the actual program.
    .org 0x80
main_entry:
    // BRISC private data RAM is 8 KiB at 0xFFB0_0000. Stack grows
    // downward from the top of that range.
    li       sp, 0xFFB02000
    call     main
    // main() never returns, but if it did, spin in WFI to halt
    // cleanly (BRISC supports WFI per the BabyRISCV CSRs doc).
1:  wfi
    j        1b
