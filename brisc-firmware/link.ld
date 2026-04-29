/* SPDX-FileCopyrightText: © 2026 Olof Johansson
 * SPDX-License-Identifier: MIT
 *
 * Linker script for the M1 hello-world BRISC firmware (issue #67).
 *
 * BRISC fetches instructions only from L1 (per
 * `BlackholeA0/TensixTile/BabyRISCV/README.md` — "Instructions can
 * only be fetched and executed from L1; instructions cannot be
 * executed out of any other memory regions"). The reset PC is 0x0,
 * so .text starts at 0.
 *
 * BRISC's local data RAM (8 KiB at 0xFFB0_0000) is for stack and
 * private scratch only — we use it for the stack via `_stack_top`,
 * but not for code or static data.
 */

OUTPUT_ARCH(riscv)
OUTPUT_FORMAT("elf32-littleriscv")
ENTRY(_start)

MEMORY
{
    L1     (rwx) : ORIGIN = 0x00000000, LENGTH = 0x180000  /* 1.5 MiB shared L1 */
    LOCAL  (rw)  : ORIGIN = 0xFFB00000, LENGTH = 0x2000    /* 8 KiB BRISC private */
}

SECTIONS
{
    .text 0x0 : {
        KEEP(*(.text._start))
        *(.text*)
    } > L1

    .rodata : ALIGN(4) { *(.rodata*) } > L1
    .data   : ALIGN(4) { *(.data*) }  > L1
    .bss    : ALIGN(4) {
        _bss_start = .;
        *(.bss*) *(COMMON)
        . = ALIGN(4);
        _bss_end = .;
    } > L1

    _stack_top = ORIGIN(LOCAL) + LENGTH(LOCAL);

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note*)
        *(.debug_*)
        *(.riscv.attributes)
    }
}
