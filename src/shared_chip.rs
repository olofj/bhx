// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Daemon-owned shared chip access.
//!
//! The chip has register blocks that are **genuinely shared across all L2CPUs**
//! — the PLL at `0x80020500+`, the reset unit at `0x80030000+`, and other AXI
//! registers at `0x8000_xxxx` all live on NOC tile `(8, 0)`. If multiple
//! independent fds each configure their own TLB window onto tile (8,0) at
//! those addresses, concurrent writes race at the hardware level: read-modify-
//! write on `L2CPU_RESET` tears, PLL step sequences interleave, and we've
//! observed the host dying as a result (see
//! <https://github.com/olofj/tt-bh-rust/issues/1>).
//!
//! `SharedChip` fixes the structural part by owning the **one and only** TLB
//! window to tile (8,0) per card for the daemon's lifetime, with an internal
//! mutex that serializes multi-step sequences (PLL step-down/step-up, reset
//! R-M-W). A daemon-wide `Arc<SharedChip>` replaces the per-boot ephemeral
//! tile-(8,0) mappings that used to alias the same registers.
//!
//! ## PCIe reset handling
//!
//! `chip::reset_board()` does a PCIe LDS reset. The re-enumeration invalidates
//! any fd held across the call — our persistent fd would start returning
//! `ENODEV`. `SharedChip::reset_board()` therefore drops the current
//! `fd + window` before issuing the reset and reopens a fresh pair after.
//! Callers never see the rotation; they acquire a read guard on the inner
//! state and find a valid `Inner` on the other side.
//!
//! ## Scope
//!
//! All tile-(8,0) access in the daemon goes through this type: the startup
//! probe, `l2cpu_is_running`, `reset_x280`, `reset_board`, and the debug
//! CLI's register pokes. Per-L2CPU NOC traffic (OpenSBI / kernel / DTB
//! image loads, L3 / L2 prefetch config, reset vectors) goes through the
//! per-L2CPU `L2Cpu` fd instead, so 4-way parallel cold boots no longer
//! share a single bus-access point.

use std::io;
use std::mem::ManuallyDrop;
use std::os::unix::io::RawFd;
use std::sync::{Mutex, RwLock};

use crate::chip;
use crate::clock::{self, PllAccess};
use crate::kmd;
use crate::tlb::TlbWindow;

/// AXI tile on Blackhole — the one shared across all L2CPUs.
const AXI_TILE_X: u16 = 8;
const AXI_TILE_Y: u16 = 0;

/// Base of the 2 MiB TLB window that covers our AXI register accesses.
/// The reset unit (`0x80030000+`), PLL control (`0x80020500+`), and all
/// currently-used `0x8000_xxxx` registers live within this 2 MiB slot.
const AXI_WINDOW_BASE: u64 = 0x8000_0000;
const AXI_WINDOW_SIZE: u64 = 0x20_0000;

/// `L2CPU_RESET` lives in the reset unit at this AXI address.
const L2CPU_RESET_ADDR: u64 = 0x8003_0014;

/// fd + persistent TLB window that may be rebuilt across a PCIe reset.
/// Field order matters: `window` must drop before `fd` because the window's
/// `Drop` issues `FREE_TLB` via the ioctl on `fd`. We use `ManuallyDrop<
/// TlbWindow>` + an explicit `Drop` impl so the order is "drop window, then
/// close fd" regardless of what Rust's default field drop order would do.
struct Inner {
    window: ManuallyDrop<TlbWindow>,
    fd: RawFd,
}

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.window);
            libc::close(self.fd);
        }
    }
}

pub struct SharedChip {
    /// Holds the `Inner` across normal use; emptied to `None` only during
    /// `reset_board` while we rotate the fd and window. Readers
    /// (`axi_read32` etc.) take the read lock; `reset_board` takes the write
    /// lock.
    inner: RwLock<Option<Inner>>,
    /// Serializes multi-step sequences (PLL step + reset R-M-W) so concurrent
    /// callers can't interleave their writes to tile (8,0) registers.
    seq_lock: Mutex<()>,
}

// Safety: `SharedChip` may be used via `Arc<SharedChip>` from any thread.
// - `RwLock<Option<Inner>>`: `Inner` owns a `TlbWindow` whose `data()` is a
//   raw `*mut u8` into PCI BAR MMIO. MMIO is a hardware synchronization
//   domain; volatile reads/writes through the pointer from multiple threads
//   land in order at the device (the host bus gives single-copy atomicity
//   for aligned u32 MMIO). `TlbWindow` itself is `Send`-only per `tlb.rs`;
//   we promote `SharedChip` to `Sync` by gating window construction/teardown
//   behind the `RwLock` and gating multi-step sequences behind `seq_lock`.
// - The `RawFd` is a plain integer; kmd's ioctl + mmap paths are thread-safe
//   for our access pattern (per tt-kmd audit in issue #1).
unsafe impl Send for SharedChip {}
unsafe impl Sync for SharedChip {}

impl SharedChip {
    pub fn new(card: u32) -> io::Result<Self> {
        let inner = Self::open_inner(card)?;
        Ok(SharedChip {
            inner: RwLock::new(Some(inner)),
            seq_lock: Mutex::new(()),
        })
    }

    /// Test-only constructor that builds a `SharedChip` with no backing fd
    /// or window. Any attempt to call `axi_read32`/`axi_write32`/etc. will
    /// panic — use only from tests that exercise surrounding state without
    /// touching the chip.
    #[cfg(test)]
    pub fn placeholder() -> Self {
        SharedChip {
            inner: RwLock::new(None),
            seq_lock: Mutex::new(()),
        }
    }

    fn open_inner(card: u32) -> io::Result<Inner> {
        let fd = kmd::open_device(card)?;
        let window = match TlbWindow::new_2m(fd, AXI_TILE_X, AXI_TILE_Y, AXI_WINDOW_BASE) {
            Ok(w) => w,
            Err(e) => {
                unsafe { libc::close(fd); }
                return Err(e);
            }
        };
        Ok(Inner {
            window: ManuallyDrop::new(window),
            fd,
        })
    }

    fn window_offset(addr: u64) -> u64 {
        let range = AXI_WINDOW_BASE..AXI_WINDOW_BASE + AXI_WINDOW_SIZE;
        assert!(
            range.contains(&addr),
            "SharedChip: addr 0x{:x} outside AXI window [0x{:x}..0x{:x})",
            addr,
            range.start,
            range.end,
        );
        addr - AXI_WINDOW_BASE
    }

    /// Single-register u32 read. No `seq_lock` — the MMIO bus gives
    /// single-copy atomicity for aligned u32 reads, and a stale value from a
    /// concurrent writer is fine for a pure read.
    pub fn axi_read32(&self, addr: u64) -> u32 {
        let off = Self::window_offset(addr);
        let guard = self.inner.read().unwrap();
        let inner = guard.as_ref().expect("SharedChip used while rotating fd");
        inner.window.read32(off)
    }

    /// Single-register u32 write. Same lock story as `axi_read32`.
    pub fn axi_write32(&self, addr: u64, value: u32) {
        let off = Self::window_offset(addr);
        let guard = self.inner.read().unwrap();
        let inner = guard.as_ref().expect("SharedChip used while rotating fd");
        inner.window.write32(off, value);
    }

    /// Probe whether L2CPU `idx`'s release bit is set. Pure read; no
    /// `seq_lock`. Mirrors the logging style of the old `boot::l2cpu_is_running`.
    pub fn l2cpu_is_running(&self, l2cpu_idx: usize) -> bool {
        let val = self.axi_read32(L2CPU_RESET_ADDR);
        let bit_idx = l2cpu_idx + 4;
        let running = (val >> bit_idx) & 1 == 1;
        eprintln!(
            "[l2cpu_is_running] L2CPU_RESET@0x{:x}={:#010x}, bit {}={}, running={}",
            L2CPU_RESET_ADDR,
            val,
            bit_idx,
            (val >> bit_idx) & 1,
            running,
        );
        running
    }

    /// Read `L2CPU_RESET` raw — used by daemon startup probe to report all
    /// four cores' state in one shot.
    pub fn read_l2cpu_reset(&self) -> u32 {
        self.axi_read32(L2CPU_RESET_ADDR)
    }

    /// Release the given L2CPUs from reset via the OpenSBI sequence:
    /// PLL step down to 200 MHz → OR-in release bits → PLL step up to 1750
    /// MHz. Holds `seq_lock` for the entire sequence so concurrent callers
    /// serialize rather than stepping the PLL against each other.
    pub fn reset_x280(&self, l2cpu_indices: &[usize]) {
        let _guard = self.seq_lock.lock().unwrap();

        eprintln!("[reset_x280] stepping PLL down to 200 MHz");
        clock::set_frequency(self, 200);

        let reset_val_before = self.axi_read32(L2CPU_RESET_ADDR);
        let mut reset_val = reset_val_before;
        let mut mask: u32 = 0;
        for &idx in l2cpu_indices {
            mask |= 1 << (idx + 4);
            reset_val |= 1 << (idx + 4);
        }
        eprintln!(
            "[reset_x280] L2CPU_RESET@0x{:x}: {:#010x} | {:#010x} -> {:#010x} (releasing L2CPU {:?})",
            L2CPU_RESET_ADDR, reset_val_before, mask, reset_val, l2cpu_indices
        );
        self.axi_write32(L2CPU_RESET_ADDR, reset_val);
        let reset_val_after = self.axi_read32(L2CPU_RESET_ADDR);
        eprintln!("[reset_x280] L2CPU_RESET readback: {:#010x}", reset_val_after);

        eprintln!("[reset_x280] stepping PLL up to 1750 MHz");
        clock::set_frequency(self, 1750);
        eprintln!("[reset_x280] done");
    }

    /// Full PCIe link reset via tt-kmd's `RESET_DEVICE` ioctl. The
    /// re-enumeration invalidates any fd held across the call, so we drop
    /// our persistent window + fd before issuing the reset and reopen fresh
    /// after. Takes the write lock so no concurrent reader sees a torn state.
    ///
    /// Caller is responsible for the usual post-reset sleep (~1 s) for the
    /// chip to re-initialize. Not baked in so callers can pipeline other
    /// setup against it.
    pub fn reset_board(&self, card: u32) -> io::Result<()> {
        let mut guard = self.inner.write().unwrap();
        // Drop the existing Inner (window's FREE_TLB + close fd) BEFORE the
        // reset, so the kmd doesn't see stale references mid-reset.
        drop(guard.take());
        chip::reset_board(card)?;
        *guard = Some(Self::open_inner(card)?);
        Ok(())
    }
}

impl PllAccess for SharedChip {
    fn pll_read32(&self, addr: u64) -> u32 {
        self.axi_read32(addr)
    }
    fn pll_write32(&self, addr: u64, value: u32) {
        self.axi_write32(addr, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Layout invariants — if these ever change, the assertions in
    // `window_offset` (and any hand-tuned addresses) stop being valid.
    // Compile-time because all inputs are `const`.
    const _: () = {
        assert!(AXI_WINDOW_BASE <= 0x80020500);
        assert!(0x80020500 < AXI_WINDOW_BASE + AXI_WINDOW_SIZE);
        assert!(AXI_WINDOW_BASE <= L2CPU_RESET_ADDR);
        assert!(L2CPU_RESET_ADDR < AXI_WINDOW_BASE + AXI_WINDOW_SIZE);
    };

    #[test]
    fn window_offset_maps_known_addresses() {
        assert_eq!(SharedChip::window_offset(0x80020500), 0x20500);
        assert_eq!(SharedChip::window_offset(L2CPU_RESET_ADDR), 0x30014);
    }

    #[test]
    #[should_panic(expected = "outside AXI window")]
    fn window_offset_rejects_out_of_range_addr() {
        SharedChip::window_offset(AXI_WINDOW_BASE + AXI_WINDOW_SIZE);
    }

    #[test]
    #[should_panic(expected = "outside AXI window")]
    fn window_offset_rejects_below_base() {
        SharedChip::window_offset(AXI_WINDOW_BASE - 1);
    }
}
