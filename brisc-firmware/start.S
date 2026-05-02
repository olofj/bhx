// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

// Multi-core BRISC + TRISC0 entry stub for the M1 hello-world (#67),
// the M3 (#69) virtio-mmio engine, and the M6.1 (#79) TRISC0-owned
// UART poll.
//
// Core layout:
//   * BRISC reset PC = 0x0 (per `BlackholeA0/TensixTile/SoftReset.md`).
//     We jump past a small zero-padded region into `brisc_main_entry`,
//     set up a stack at the top of BRISC's 8 KiB private RAM, and call
//     `main()`.
//   * TRISC0 reset PC is set via the host-programmed override register
//     `RISCV_DEBUG_REG_TRISC0_RESET_PC` (0xFFB1_2228), enabled by bit 0
//     of `RISCV_DEBUG_REG_TRISC_RESET_PC_OVERRIDE` (0xFFB1_2234), both
//     written by the host during engine bring-up. The host reads
//     `trisc0_reset_entry`'s linker-resolved address from L1[0x4]
//     (planted as a u32 word below) and writes it to the override.
//
// One binary, two entry points: BRISC and TRISC0 share the same
// `.text*` sections in L1 (per BabyRISCV README, instructions can only
// be fetched from L1, not core-local TCM), and which entry runs is
// determined by which core gets released from soft reset.
//
// Why a header word instead of `mhartid`: `mhartid` is read-only zero
// on every baby RISC (per `BabyRISCV/CSRs.md` — explicitly called out
// as "non-conformant"). Each core has its own configurable reset PC
// and that's the hardware mechanism Tensix exposes for distinguishing
// what code each core runs.
//
// The 0x80-byte zero pad past the entry slot exists so the M1 hello-
// world's magic+counter slots at L1[0x40, 0x44] (well past ARC
// firmware's L1[0x10..0x20] scratch region) land in pre-zeroed space.
// The .word at L1[0x4] doesn't conflict with hello's slots.

    .section .text._start, "ax"
    .global  _start
_start:
    j        brisc_main_entry      // PC=0:   BRISC reset entry
    .word    trisc0_reset_entry    // PC=4:   TRISC0 reset entry address
                                   //         (host reads, writes to PC override)
    .word    trisc1_reset_entry    // PC=8:   TRISC1 reset entry address (#125)
                                   //         dedicated QUEUE_SEL→READY watch.
    .word    trisc2_reset_entry    // PC=C:   TRISC2 reset entry address (#158)
                                   //         dedicated DEVICE_FEATURES_SEL watch
                                   //         (split from TRISC1 to halve per-slot
                                   //         work in the race-critical path).

    // Aligned entry. Anything past here is the actual program.
    .org 0x80
brisc_main_entry:
    // BRISC private data RAM is 8 KiB at 0xFFB0_0000. Stack grows
    // downward from the top.
    li       sp, 0xFFB02000

    // Zero BSS. After a tt-smi reset L1 retains whatever was left
    // by a previous workload, and `objcopy -O binary` doesn't include
    // BSS bytes — so without an explicit clear, statics like
    // `trisc0_running` could start non-zero. Word-at-a-time loop;
    // `_bss_start`/`_bss_end` come from the linker script and are
    // 4-byte aligned (see `link.ld`).
    la       t0, _bss_start
    la       t1, _bss_end
1:  beq      t0, t1, 2f
    sw       zero, 0(t0)
    addi     t0, t0, 4
    j        1b
2:
    call     main
    // main() never returns, but if it did, spin in WFI to halt.
1:  wfi
    j        1b

    .global  trisc0_reset_entry
trisc0_reset_entry:
    // TRISC0 private data RAM is 4 KiB at 0xFFB0_0000 (each core sees
    // its own memory at that base address; the size is per-core — 8
    // KiB on B/NC, 4 KiB on T0/T1/T2). Stack grows downward from the
    // top of the 4 KiB region.
    //
    // BSS is shared with BRISC and was already zeroed when BRISC
    // entered main. (TRISC0 is released only after BRISC observes
    // a non-zero UART_SLOT_MASK in the active-slots bitmap, which
    // can only happen after the daemon's hello — long past BSS init.)
    li       sp, 0xFFB01000
    call     trisc0_main
1:  wfi
    j        1b

    .global  trisc1_reset_entry
trisc1_reset_entry:
    // TRISC1 private data RAM mirrors TRISC0's: each core sees its own
    // 4 KiB at the same 0xFFB0_0000 base. Stack grows downward from
    // the top. Same BSS guarantee — BRISC zeroed it before any TRISC
    // is released.
    //
    // TRISC1 runs the dedicated SEL-watch loop (#125): the tightest
    // possible polling of QUEUE_SEL across active slots, clearing
    // QUEUE_READY=0 immediately on detected SEL change so the kernel's
    // `writel(SEL); readl(QUEUE_READY)` race window collapses below
    // BRISC's full sweep period.
    li       sp, 0xFFB01000
    call     trisc1_main
1:  wfi
    j        1b

    .global  trisc2_reset_entry
trisc2_reset_entry:
    // TRISC2 private data RAM mirrors TRISC0/1: each core sees its own
    // 4 KiB at 0xFFB0_0000. Stack grows downward from the top.
    //
    // TRISC2 runs the dedicated DEVICE_FEATURES_SEL watch (#158): swap
    // the visible DEVICE_FEATURES cell to match the kernel's most
    // recent SEL write, closing the bit-0 leak (#132) without competing
    // with TRISC1's QUEUE_SEL watch for L1 bandwidth in the same hart.
    li       sp, 0xFFB01000
    call     trisc2_main
1:  wfi
    j        1b
