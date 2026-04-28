// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

// Shared BRISC entry stub for both M1 (hello-world, #67) and M3
// (virtio-mmio engine, #69) firmware variants.
//
// BRISC's reset PC is 0x0 in tile L1 (per
// `BlackholeA0/TensixTile/SoftReset.md`), so the linker places this
// section at the very start of L1. We jump past a small zero-padded
// region into `main_entry`, which sets up a stack in BRISC's local
// data RAM (8 KiB at 0xFFB0_0000) and calls `main()`.
//
// The 0x80-byte zero pad after the entry jump exists so the M1
// hello-world's two magic+counter slots at L1[0x40, 0x44] (well past
// ARC firmware's L1[0x10..0x20] scratch region) land in pre-zeroed
// space. The M3 virtio firmware doesn't touch this range — its
// register files live at L1[0x10000+] — so the pad is harmless.

    .section .text._start, "ax"
    .global  _start
_start:
    j        main_entry         // PC=0: jump past the zero-padded area.

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
