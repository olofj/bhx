// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT
//
// 16550-compatible UART emulation on the Tensix engine (#78).
//
// One per L2CPU. Reg file lives in BRISC L1 at a fixed offset; each
// L2CPU's small TLB window (already programmed for the virtio engine)
// also covers this region, so the guest sees its UART at a fixed
// offset within its existing engine MMIO range — no second TLB slot
// needed. See `BRISC_UART_OFFSET_FROM_ENGINE_BASE` below.
//
// Why TX-only for the first cut: a static-MMIO 8250 emulation can't
// observe RBR reads from the guest, so it can't safely advance an
// internal RX FIFO without risking duplicate-byte reads on the
// kernel's tight `do { read RBR; read LSR; } while (LSR & DR);` loop.
// TX has no such issue — the guest writes a byte, BRISC sees the
// write via a sentinel-clear pattern, and pushes it to the daemon. RX
// is a follow-up; for now LSR.DR stays 0 forever and the kernel
// silently never reads RBR. That's enough to verify a stock distro
// kernel boots through to a login prompt (the immediate motivation
// from #47/#78), and lets a follow-up commit add a "best-effort RX"
// based on metered delivery without breaking the protocol.

#ifndef BRISC_UART_LAYOUT_H
#define BRISC_UART_LAYOUT_H

#include <stdint.h>

// ----- L1 placement -----
//
// 4 per-L2CPU UART reg files at 16 KiB stride, starting at 0x40000.
// Stride matches the engine's `BRISC_VIRTIO_PER_L2CPU_SIZE` (16 KiB)
// so each L2CPU's existing 2 MiB engine TLB window covers its UART
// at a uniform offset of 0x30000 (= 0x40000 - 0x10000) from the
// window base. The DTB node for L2CPU i emits
// `reg = <engine_base[i] + 0x30000, 0x1000>`.
//
//   L2CPU 0 UART  → L1 0x40000  (engine_base[0]=0x10000, PA = +0x30000)
//   L2CPU 1 UART  → L1 0x44000  (engine_base[1]=0x14000)
//   L2CPU 2 UART  → L1 0x48000
//   L2CPU 3 UART  → L1 0x4C000

#define BRISC_UART_BASE                      0x00040000u
#define BRISC_UART_PER_L2CPU_STRIDE          0x00004000u  // 16 KiB
#define BRISC_UART_REG_FILE_SIZE             0x00001000u  // 4 KiB exposed
#define BRISC_UART_OFFSET_FROM_ENGINE_BASE   0x00030000u

static inline uintptr_t brisc_uart_regs_base(unsigned l2cpu_idx) {
    return (uintptr_t)(BRISC_UART_BASE + l2cpu_idx * BRISC_UART_PER_L2CPU_STRIDE);
}

// ----- BRISC ↔ TRISC0 per-L2CPU UART state -----
//
// Per-L2CPU 8 KiB at 0x50000 + idx*0x2000. Layout (M6.1, #79):
//
//   +0x0000   hold_counter   (TRISC0-private; THRE=0 backpressure window
//                              counted in TRISC0 sweeps after a TX byte)
//   +0x0004   producer_seq   (TRISC0 writes, BRISC reads — SPSC head)
//   +0x0008   consumer_seq   (BRISC writes, TRISC0 reads — SPSC tail)
//   +0x000C   drop_count     (TRISC0 writes; bumped when ring is full)
//   +0x0010..0x0040          reserved
//   +0x0040..0x1040          ring[1024 × u32] (byte payload in low 8 bits)
//   +0x1040..0x2000          reserved (room for future RX state, etc.)
//
// Why split the SPSC ring per L2CPU instead of one shared ring:
// TRISC0 polls the four UARTs in a fixed order, so it never produces
// to two L2CPUs in one sweep — but BRISC's drain pass needs to know
// which L2CPU a byte belongs to, and a per-L2CPU ring makes that
// inherent in the address.
//
// Why 1024 entries: 32 was enough on paper for the steady-state kernel
// rate, but stock-distro boots produce bursts that interact poorly
// with the kick-ring depth downstream (only 64 entries, polled at
// ~50 µs cadence). A deep ring at the TRISC0→BRISC seam absorbs the
// burst so BRISC's drain pass paces the kick ring at a rate the
// daemon poll can keep up with. Memory cost is 4 KiB per L2CPU × 4 =
// 16 KiB of L1 — well within the 1.5 MiB tile budget.
//
// Not mapped into any L2CPU TLB window.

#define BRISC_UART_PRIVATE_BASE              0x00050000u
#define BRISC_UART_PRIVATE_PER_L2CPU         0x00002000u

#define UART_PRIV_OFF_HOLD                   0x00u
#define UART_PRIV_OFF_FEED_PRODUCER_SEQ      0x04u
#define UART_PRIV_OFF_FEED_CONSUMER_SEQ      0x08u
#define UART_PRIV_OFF_FEED_DROP_COUNT        0x0Cu
#define UART_PRIV_OFF_FEED_RING              0x40u

#define BRISC_UART_FEED_RING_ENTRIES         1024u
#define BRISC_UART_FEED_RING_MASK            (BRISC_UART_FEED_RING_ENTRIES - 1u)

// THRE backpressure window in TRISC0 sweeps. TRISC0 sweeps fast
// (~40 ns per UART at 1350 MHz AICLK). 2000 sweeps × ~40 ns ≈ 80 µs
// of THRE=0 — wide enough that even a slow LSR-read return on the
// kernel side reliably observes the backpressure before deciding to
// write the next byte.
#define TRISC0_UART_THRE_HOLD_SWEEPS         20u

static inline uintptr_t brisc_uart_private_base(unsigned l2cpu_idx) {
    return (uintptr_t)(BRISC_UART_PRIVATE_BASE + l2cpu_idx * BRISC_UART_PRIVATE_PER_L2CPU);
}

// ----- 16550 register offsets, reg-shift = 2 (4-byte stride) -----
//
// Eight registers, each occupying a 4-byte cell. The kernel's
// `mem32_serial_in/out` (DT-based ns16550 with reg-shift=2 +
// reg-io-width=4) does 32-bit MMIO accesses with the byte payload
// in the low 8 bits.

#define UART_REG_RBR_THR     0x00  // R: RBR (DLAB=0) / W: THR (DLAB=0) / DLL (DLAB=1)
#define UART_REG_IER_DLM     0x04  // RW: IER (DLAB=0) / DLM (DLAB=1)
#define UART_REG_IIR_FCR     0x08  // R: IIR / W: FCR
#define UART_REG_LCR         0x0c  // RW: LCR (bit 7 = DLAB)
#define UART_REG_MCR         0x10  // RW: MCR
#define UART_REG_LSR         0x14  // R:  LSR
#define UART_REG_MSR         0x18  // R:  MSR
#define UART_REG_SCR         0x1c  // RW: scratch

// LSR bits we care about.
#define UART_LSR_DR          0x01u  // RX data ready
#define UART_LSR_THRE        0x20u  // TX holding empty
#define UART_LSR_TEMT        0x40u  // TX shift empty

// MSR — hardwire CTS+DSR asserted. No carrier change.
#define UART_MSR_CTS         0x10u
#define UART_MSR_DSR         0x20u

// IIR — bits 7:6 advertise FIFO size (0xc0 = 16550A 16-byte FIFO,
// 0x00 = no FIFO / 8250 mode), bits 3:1 are the pending interrupt
// code, bit 0 is the "no-interrupt" indicator.
//
// We deliberately advertise 0x00 (no FIFO) instead of 0xc0
// (16550A). With FIFO=16, the kernel writes up to 16 bytes
// back-to-back before re-polling LSR.THRE; our static reg file
// can't absorb a 16-byte burst in a single L1 cell, and bytes 2..16
// of the burst overwrite each other before BRISC's next sweep
// observes them. With FIFO=0 the driver falls back to the
// single-byte path, polls LSR.THRE between writes, and our
// THRE-clear-on-byte backpressure keeps each write safely captured.
#define UART_IIR_NO_INT      0x01u
#define UART_IIR_NO_FIFO     0x00u

// MCR — hardwire DTR+RTS+OUT2 asserted (typical "good link" pattern
// distros expect at boot).
#define UART_MCR_DTR_RTS_OUT2  0x0bu

// LCR — 8N1 (8 data bits, no parity, 1 stop bit), DLAB clear.
#define UART_LCR_8N1         0x03u
#define UART_LCR_DLAB        0x80u

// ----- TX sentinel encoding -----
//
// The cell at UART_REG_RBR_THR is shared between the guest's THR
// writes (TX) and (in a future commit) BRISC's RBR writes (RX).
// Right now we only handle TX. We mark "no pending TX byte" by
// writing the sentinel `BRISC_UART_THR_SENTINEL`; the kernel's
// 32-bit THR write zeroes the upper 24 bits, so any cell value with
// bits 31:8 == 0 (and bit 8 is implicitly clear) is a fresh TX byte
// from the guest. Pre-init and post-consume the firmware writes the
// sentinel back.
//
// Why bit 31 instead of any non-zero upper bit: the kernel's read
// path (when we add RX) will use bit 31 set + payload in low 8 bits
// to deliver an RX byte; keeping the sentinel and the RX-cookie in
// the same upper-bit space simplifies the two-direction sharing.

#define BRISC_UART_THR_SENTINEL  0xFFFFFFFFu

// ----- Kick-ring slot encoding for UART -----
//
// Each L2CPU owns 8 contiguous slots in the active-slots bitmap (the
// virtio engine's per-L2CPU window stride). Within an L2CPU's region
// the layout is:
//   dev_idx 0..5 -> virtio (BLK / NET / CONSOLE / RNG / BLK1 / BLK2)
//   dev_idx 6    -> UART
//   dev_idx 7    -> reserved
//
// So the absolute UART slot for L2CPU `i` is
// `i * BRISC_VIRTIO_DEVS_PER_L2CPU + BRISC_UART_SLOT_OFFSET_IN_L2CPU`,
// landing at 6, 14, 22, 30. Disjoint from every L2CPU's virtio
// dev_idx range — fixes #175 (the prior contiguous slots 16..19
// layout collided with L2CPU 2's virtio range 16..23 once
// DEVS_PER_L2CPU was bumped from 4 to 8).
//
// The active-slots bitmap (CTRL_OFF_ACTIVE_SLOTS) is u32, so the
// highest UART bit (slot 30) fits. The daemon flips the bit on
// register-uart and off on unregister-uart, mirroring the virtio
// register flow. BRISC only polls UARTs whose bit is set.

#include "virtio_layout.h"

#define BRISC_UART_SLOT_OFFSET_IN_L2CPU  6u

// Absolute slot for L2CPU `i`'s UART.
static inline unsigned brisc_uart_slot_for_l2cpu(unsigned l2cpu_idx) {
    return l2cpu_idx * BRISC_VIRTIO_DEVS_PER_L2CPU + BRISC_UART_SLOT_OFFSET_IN_L2CPU;
}

// All UART bits combined — used by BRISC to drive TRISC0's reset
// lifecycle in M6.1 (#79). If any bit in this mask is set, TRISC0
// runs (polls UARTs); if all clear, TRISC0 is held in soft reset.
//
// With offset 6 in each 8-slot region this is bits {6, 14, 22, 30}
// = 0x40404040.
#define BRISC_UART_SLOT_MASK                                                                       \
    ((1u << (0u * BRISC_VIRTIO_DEVS_PER_L2CPU + BRISC_UART_SLOT_OFFSET_IN_L2CPU))                  \
     | (1u << (1u * BRISC_VIRTIO_DEVS_PER_L2CPU + BRISC_UART_SLOT_OFFSET_IN_L2CPU))                \
     | (1u << (2u * BRISC_VIRTIO_DEVS_PER_L2CPU + BRISC_UART_SLOT_OFFSET_IN_L2CPU))                \
     | (1u << (3u * BRISC_VIRTIO_DEVS_PER_L2CPU + BRISC_UART_SLOT_OFFSET_IN_L2CPU)))

// ----- TRISC0 globals (M6.1, #79) -----
//
// TRISC0-private state that doesn't fit the per-L2CPU UART private
// region's 0x2000-byte stride. Single per-tile block right after the
// last per-L2CPU UART private slot (4 × 0x2000 = 0x8000 → globals at
// 0x58000).
//
//   0x58000 + 0x00   trisc0 heartbeat   (TRISC0 bumps each loop iter;
//                                        BRISC + host watch for stalls)
//   0x58000 + 0x04   trisc0 reserved    (room for stats + diagnostics
//                                        as Phase B / C land features)

#define BRISC_TRISC0_GLOBAL_BASE       0x00058000u
#define BRISC_TRISC0_GLOBAL_SIZE       0x00000100u
#define TRISC0_GLOBAL_OFF_HEARTBEAT    0x00u

#endif  // BRISC_UART_LAYOUT_H
