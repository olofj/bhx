// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Tensix tile bring-up — load BRISC firmware, drive soft reset.
//!
//! Issue #67 (M1) — foundation for the Tensix-as-virtio-engine
//! architecture in #66. Establishes "we can run RISC-V code on a
//! Tensix tile" without doing anything virtio-related yet.
//!
//! Per `BlackholeA0/TensixTile/BabyRISCV/README.md`, BabyRISCs fetch
//! instructions only from L1 — code in BRISC's local 8 KiB at
//! `0xFFB0_0000` cannot be executed. BRISC's reset PC is `0x0` (per
//! `BlackholeA0/TensixTile/SoftReset.md`), so a flat firmware binary
//! starts executing the moment BRISC's soft-reset bit is cleared.
//!
//! Memory map of a Tensix tile, from this module's perspective:
//! ```text
//!   0x0000_0000 .. 0x0018_0000  shared L1 SRAM (1.5 MiB)
//!   0xFFB0_0000 .. 0xFFB0_2000  BRISC private data RAM (8 KiB)
//!   0xFFB1_2000 .. 0xFFB1_3000  per-tile RISC-V debug-regs block
//!     0xFFB1_21B0                soft-reset register (this file)
//! ```
//! Two 2 MiB chip-side TLB windows cover the addresses we touch:
//! one based at L1 offset 0 for firmware load + status polling, one
//! based at `0xFFA0_0000` for the soft-reset register.

use std::io;
use std::mem::ManuallyDrop;
use std::os::unix::io::RawFd;

use crate::kmd;
use crate::tlb::TlbWindow;

/// Soft-reset register address (per-tile, NoC-addressable). Bits 11,
/// 12, 13, 14, 18 control BRISC, TRISC0/1/2, NCRISC respectively.
/// Source: `BlackholeA0/TensixTile/SoftReset.md`.
pub const TENSIX_SOFT_RESET_ADDR: u64 = 0xFFB1_21B0;

pub const SOFT_RESET_BRISC: u32 = 1 << 11;
pub const SOFT_RESET_TRISC0: u32 = 1 << 12;
pub const SOFT_RESET_TRISC1: u32 = 1 << 13;
pub const SOFT_RESET_TRISC2: u32 = 1 << 14;
pub const SOFT_RESET_NCRISC: u32 = 1 << 18;

/// All five baby RISCs in soft reset (idempotent halt). Used as the
/// pre-firmware-load barrier so an unknown prior tile state can't
/// race the host's L1 writes.
pub const SOFT_RESET_ALL: u32 = SOFT_RESET_BRISC
    | SOFT_RESET_TRISC0
    | SOFT_RESET_TRISC1
    | SOFT_RESET_TRISC2
    | SOFT_RESET_NCRISC;

/// All baby RISCs in reset *except* BRISC. Writing this releases
/// BRISC; NCRISC and the TRISCs stay halted so they don't fetch
/// garbage out of L1.
pub const SOFT_RESET_ALL_EXCEPT_BRISC: u32 = SOFT_RESET_ALL & !SOFT_RESET_BRISC;

// ----- Per-core reset PC override (M6.1, #79) -----
//
// By default the baby RISCs come out of reset at fixed PCs (BRISC at
// 0x0, TRISC0 at 0x6000, TRISC1 at 0xA000, TRISC2 at 0xE000, NCRISC at
// 0x12000 — see `BlackholeA0/TensixTile/SoftReset.md`). For the
// shared-binary M6.1 layout we keep BRISC's default (0x0) and override
// TRISC0 to point at `trisc0_reset_entry` in the same firmware image
// — the host reads the linker-resolved address out of L1[0x4] (planted
// by `start.S` as `.word trisc0_reset_entry`) and writes it here.
//
// Bit 0 of `RISCV_DEBUG_REG_TRISC_RESET_PC_OVERRIDE` enables the
// override for TRISC0; bits 1 and 2 cover TRISC1/TRISC2 (we don't use
// those in M6.1).

/// TRISC0 reset PC override value. 32-bit register.
pub const RISCV_DEBUG_REG_TRISC0_RESET_PC: u64 = 0xFFB1_2228;
/// TRISC1 reset PC override value (#125). Per the SoftReset.md doc
/// the per-TRISC override registers are 4-byte spaced after T0.
pub const RISCV_DEBUG_REG_TRISC1_RESET_PC: u64 = 0xFFB1_222C;
/// Enable bits for the TRISC reset PC override. Bit 0 = TRISC0,
/// bit 1 = TRISC1, bit 2 = TRISC2. M6.1 set bit 0; #125 also sets
/// bit 1 for TRISC1's dedicated SEL-watch loop.
pub const RISCV_DEBUG_REG_TRISC_RESET_PC_OVERRIDE: u64 = 0xFFB1_2234;
pub const TRISC_RESET_PC_OVERRIDE_T0: u32 = 1 << 0;
pub const TRISC_RESET_PC_OVERRIDE_T1: u32 = 1 << 1;

/// L1 offset of the u32 word `start.S` plants holding the linker-
/// resolved address of `trisc0_reset_entry`. Mirrored by the firmware
/// header — keep in sync with `start.S` (`.word trisc0_reset_entry`).
pub const TRISC0_RESET_ENTRY_PTR_L1_OFFSET: u32 = 0x4;
/// L1 offset of `trisc1_reset_entry` (#125). Adjacent to TRISC0's
/// slot in the start.S header.
pub const TRISC1_RESET_ENTRY_PTR_L1_OFFSET: u32 = 0x8;

/// Tensix L1 size in bytes (per `dev_mem_map.h::MEM_L1_SIZE`).
pub const TENSIX_L1_SIZE: usize = 1536 * 1024;

/// L1 offsets the M1 hello-world firmware writes. Kept here so
/// the host poller and the firmware agree without the firmware
/// having to compile against this crate.
pub const HELLO_MAGIC_OFFSET: u32 = 0x40;
pub const HELLO_COUNTER_OFFSET: u32 = 0x44;
pub const HELLO_MAGIC_VALUE: u32 = 0xA110_C0DE;

/// Hello-world firmware bytes, embedded at compile time. Built by
/// `build.rs` via `brisc-firmware/Makefile`.
pub const HELLO_FIRMWARE: &[u8] = include_bytes!(env!("BRISC_HELLO_BIN"));

/// 2 MiB TLB window bases on the chosen tile.
///
/// * L1 lives at NoC offset 0 — one 2 MiB window covers the first
///   2 MiB (which is more than the 1.5 MiB it actually has).
/// * The RISC-V debug-regs block at `0xFFB1_2000` is reached via a
///   2 MiB-aligned window based at `0xFFA0_0000` (covers
///   `[0xFFA0_0000, 0xFFC0_0000)`, includes `0xFFB1_21B0`).
const TLB_BASE_L1: u64 = 0x0;
const TLB_BASE_DEBUG_REGS: u64 = 0xFFA0_0000;

/// One Tensix tile, with chip-side TLB windows already wired up to
/// its L1 and per-tile RISC-V debug-regs block.
///
/// Drop order matters: TLB windows free via FREE_TLB ioctl (which
/// needs the fd open), then we close the fd. `ManuallyDrop` enforces
/// the ordering regardless of the default field-drop order.
pub struct TensixTile {
    /// NoC0 logical X / Y of this tile. Stored at construction for
    /// diagnostic logging; not read by the current code paths after
    /// the TLB windows are wired.
    #[allow(dead_code)]
    pub x: u16,
    #[allow(dead_code)]
    pub y: u16,
    fd: RawFd,
    l1_window: ManuallyDrop<TlbWindow>,
    debug_regs_window: ManuallyDrop<TlbWindow>,
}

// Safety: the contained TlbWindow holds a raw pointer to PCI BAR
// MMIO, and that pointer is single-copy-atomic for aligned u32 access
// per the host bus. We're Send-only (matching `TlbWindow`); no Sync
// implementation — multi-threaded access requires external sync, just
// like `SharedChip`.
unsafe impl Send for TensixTile {}

impl TensixTile {
    /// Open card `card`, configure two TLB windows on tile `(x, y)`.
    pub fn new(card: u32, x: u16, y: u16) -> io::Result<Self> {
        let fd = kmd::open_device(card)?;
        let l1_window = match TlbWindow::new_2m(fd, x, y, TLB_BASE_L1) {
            Ok(w) => w,
            Err(e) => {
                unsafe {
                    libc::close(fd);
                }
                return Err(e);
            }
        };
        let debug_regs_window = match TlbWindow::new_2m(fd, x, y, TLB_BASE_DEBUG_REGS) {
            Ok(w) => w,
            Err(e) => {
                drop(l1_window);
                unsafe {
                    libc::close(fd);
                }
                return Err(e);
            }
        };
        Ok(TensixTile {
            x,
            y,
            fd,
            l1_window: ManuallyDrop::new(l1_window),
            debug_regs_window: ManuallyDrop::new(debug_regs_window),
        })
    }

    pub fn read_l1_u32(&self, offset: u32) -> u32 {
        assert!(
            (offset as usize) + 4 <= TENSIX_L1_SIZE,
            "L1 read offset 0x{:x} + 4 > L1 size",
            offset
        );
        self.l1_window.read32(offset as u64)
    }

    pub fn write_l1_u32(&self, offset: u32, value: u32) {
        assert!(
            (offset as usize) + 4 <= TENSIX_L1_SIZE,
            "L1 write offset 0x{:x} + 4 > L1 size",
            offset
        );
        self.l1_window.write32(offset as u64, value);
    }

    /// Host VA pointing at L1 byte `offset`. Used by code paths that
    /// need a raw pointer (e.g. `InterruptController::set_interrupt`,
    /// which takes `*mut u32`). Caller is responsible for keeping
    /// the `TensixTile` alive for the lifetime of the returned
    /// pointer.
    pub fn l1_ptr(&self, offset: u32) -> *mut u8 {
        assert!(
            (offset as usize) < TENSIX_L1_SIZE,
            "L1 ptr offset 0x{:x} > L1 size",
            offset
        );
        unsafe { self.l1_window.get_window().add(offset as usize) }
    }

    pub fn read_soft_reset(&self) -> u32 {
        let off = TENSIX_SOFT_RESET_ADDR - TLB_BASE_DEBUG_REGS;
        self.debug_regs_window.read32(off)
    }

    pub fn write_soft_reset(&self, value: u32) {
        let off = TENSIX_SOFT_RESET_ADDR - TLB_BASE_DEBUG_REGS;
        self.debug_regs_window.write32(off, value);
    }

    /// Put every BabyRISC on this tile into soft reset and read back
    /// the register so the write is flushed before any subsequent L1
    /// access. Idempotent.
    pub fn assert_all_resets(&self) {
        self.write_soft_reset(SOFT_RESET_ALL);
        let _ = self.read_soft_reset();
    }

    /// Release BRISC from soft reset, keeping NCRISC + TRISCs halted
    /// so they don't fetch from L1 (where our BRISC firmware lives).
    /// Reads back to flush the write.
    pub fn release_brisc_only(&self) {
        self.write_soft_reset(SOFT_RESET_ALL_EXCEPT_BRISC);
        let _ = self.read_soft_reset();
    }

    /// Program TRISC0's reset PC override register (M6.1, #79). Once
    /// the override is enabled, releasing TRISC0's soft-reset bit
    /// jumps to `pc` instead of the default 0x6000. Caller must call
    /// [`Self::enable_trisc0_reset_pc_override`] separately to flip
    /// the enable bit.
    pub fn set_trisc0_reset_pc(&self, pc: u32) {
        let off = RISCV_DEBUG_REG_TRISC0_RESET_PC - TLB_BASE_DEBUG_REGS;
        self.debug_regs_window.write32(off, pc);
    }

    /// Enable the TRISC0 reset-PC override (RMW: set bit 0 of the
    /// shared TRISC override register, leaving TRISC1/TRISC2 bits
    /// untouched).
    pub fn enable_trisc0_reset_pc_override(&self) {
        let off = RISCV_DEBUG_REG_TRISC_RESET_PC_OVERRIDE - TLB_BASE_DEBUG_REGS;
        let prev = self.debug_regs_window.read32(off);
        self.debug_regs_window
            .write32(off, prev | TRISC_RESET_PC_OVERRIDE_T0);
    }

    /// Read TRISC0's reset entry-point address out of L1[0x4]. The
    /// firmware's `start.S` plants the linker-resolved address there
    /// as a `.word`. The host calls this after `load_brisc_firmware`
    /// to feed [`Self::set_trisc0_reset_pc`].
    pub fn read_trisc0_reset_entry(&self) -> u32 {
        self.read_l1_u32(TRISC0_RESET_ENTRY_PTR_L1_OFFSET)
    }

    /// TRISC1 (#125) variants of the same setup ritual.
    pub fn set_trisc1_reset_pc(&self, pc: u32) {
        let off = RISCV_DEBUG_REG_TRISC1_RESET_PC - TLB_BASE_DEBUG_REGS;
        self.debug_regs_window.write32(off, pc);
    }

    pub fn enable_trisc1_reset_pc_override(&self) {
        let off = RISCV_DEBUG_REG_TRISC_RESET_PC_OVERRIDE - TLB_BASE_DEBUG_REGS;
        let prev = self.debug_regs_window.read32(off);
        self.debug_regs_window
            .write32(off, prev | TRISC_RESET_PC_OVERRIDE_T1);
    }

    pub fn read_trisc1_reset_entry(&self) -> u32 {
        self.read_l1_u32(TRISC1_RESET_ENTRY_PTR_L1_OFFSET)
    }

    /// Copy `firmware` bytes into L1 starting at offset 0 using
    /// 32-bit MMIO writes. Pads with zeros if `firmware.len()` is
    /// not a multiple of 4.
    pub fn load_brisc_firmware(&self, firmware: &[u8]) {
        assert!(
            firmware.len() <= TENSIX_L1_SIZE,
            "firmware ({} bytes) exceeds L1 size ({} bytes)",
            firmware.len(),
            TENSIX_L1_SIZE
        );
        let chunks = firmware.chunks_exact(4);
        let remainder = chunks.remainder();
        for (i, chunk) in chunks.clone().enumerate() {
            let w = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            self.l1_window.write32((i * 4) as u64, w);
        }
        if !remainder.is_empty() {
            let mut tail = [0u8; 4];
            tail[..remainder.len()].copy_from_slice(remainder);
            let w = u32::from_le_bytes(tail);
            let off = (firmware.len() / 4) * 4;
            self.l1_window.write32(off as u64, w);
        }
    }
}

impl Drop for TensixTile {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.debug_regs_window);
            ManuallyDrop::drop(&mut self.l1_window);
            libc::close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The soft-reset register must land inside the 2 MiB debug-regs
    // window, otherwise the offset arithmetic in
    // {read,write}_soft_reset would underflow or read past the
    // window. The TRISC0 reset-PC override registers (M6.1, #79) live
    // in the same range and need the same bound. Compile-time check —
    // `const { assert!(...) }` fails the build instead of a test if a
    // future edit breaks the invariant. The same block also pins the
    // soft-reset bitmask membership and the TRISC0 PC-override bit
    // layout (matches `BlackholeA0/TensixTile/SoftReset.md`).
    const _DEBUG_REGS_WINDOW_INVARIANTS: () = {
        const TWO_MEG: u64 = 2 * 1024 * 1024;
        assert!(TENSIX_SOFT_RESET_ADDR >= TLB_BASE_DEBUG_REGS);
        assert!(TENSIX_SOFT_RESET_ADDR + 4 <= TLB_BASE_DEBUG_REGS + TWO_MEG);
        assert!(RISCV_DEBUG_REG_TRISC0_RESET_PC >= TLB_BASE_DEBUG_REGS);
        assert!(RISCV_DEBUG_REG_TRISC0_RESET_PC + 4 <= TLB_BASE_DEBUG_REGS + TWO_MEG);
        assert!(RISCV_DEBUG_REG_TRISC_RESET_PC_OVERRIDE >= TLB_BASE_DEBUG_REGS);
        assert!(RISCV_DEBUG_REG_TRISC_RESET_PC_OVERRIDE + 4 <= TLB_BASE_DEBUG_REGS + TWO_MEG);
        assert!(SOFT_RESET_ALL & SOFT_RESET_BRISC == SOFT_RESET_BRISC);
        assert!(SOFT_RESET_ALL & SOFT_RESET_TRISC0 == SOFT_RESET_TRISC0);
        assert!(SOFT_RESET_ALL & SOFT_RESET_TRISC1 == SOFT_RESET_TRISC1);
        assert!(SOFT_RESET_ALL & SOFT_RESET_TRISC2 == SOFT_RESET_TRISC2);
        assert!(SOFT_RESET_ALL & SOFT_RESET_NCRISC == SOFT_RESET_NCRISC);
        assert!(SOFT_RESET_ALL & !SOFT_RESET_ALL_EXCEPT_BRISC == SOFT_RESET_BRISC);
        assert!(TRISC_RESET_PC_OVERRIDE_T0 == 0x1);
    };

    #[test]
    fn hello_firmware_is_nonempty_and_aligned() {
        // The build script must have produced a non-empty .bin and
        // the firmware load path assumes 4-byte stride writes are
        // sufficient (no straddling reads).
        assert!(!HELLO_FIRMWARE.is_empty());
        assert!(HELLO_FIRMWARE.len() <= TENSIX_L1_SIZE);
        // First instruction at offset 0 should be a non-trivial value
        // (the `j main_entry` jump in start.S, encoded as 0x0800006f).
        let first_word = u32::from_le_bytes([
            HELLO_FIRMWARE[0],
            HELLO_FIRMWARE[1],
            HELLO_FIRMWARE[2],
            HELLO_FIRMWARE[3],
        ]);
        assert_ne!(first_word, 0, "firmware appears empty (first word is zero)");
    }
}
