// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! L2CPU-side NoC TLB programming.
//!
//! The x280 cores can't emit raw 64-bit NoC addresses — their effective PA
//! tops out somewhere in the 40-48 bit range, well below the
//! `noc_pcie_offset = 4 << 58` that tt-kmd uses for outbound iATU
//! regions. To bridge, the x280 has 224 small (2 MiB) and 32 large
//! (128 GiB) configurable TLB windows that map L2CPU physical addresses
//! to arbitrary 64-bit NoC `(x, y, addr)` tuples.
//!
//! See `BlackholeA0/L2CPUTile/{MemoryMap,TLBWindows}.md` in
//! `tenstorrent/tt-isa-documentation` for the canonical description.
//! Phase-0 prototype on `prototype/64-host-buffer` end-to-end-verified
//! the path used here (see `tt-bh-linux` issue #64 for results).

use crate::l2cpu::L2Cpu;

/// Base of the small TLB window configuration registers in the L2CPU's
/// physical address space (= NoC offset, since `[0, 2^47)` is passthrough
/// to x280 PA per the L2CPU tile memory map). 224 entries × 16 bytes.
pub const SMALL_TLB_CFG_BASE: u64 = 0x0000_2000_0000;
pub const SMALL_TLB_CFG_STRIDE: u64 = 16;
pub const SMALL_TLB_COUNT: usize = 224;

/// Base of the small TLB window memory regions (uncached). Each window
/// is 2 MiB, 224 contiguous windows; `0x4004_3000_0000` for the cached
/// alias if that's wanted instead.
pub const SMALL_TLB_WINDOW_BASE_UC: u64 = 0x0000_0004_3000_0000;
pub const SMALL_TLB_WINDOW_SHIFT: u32 = 21;
pub const SMALL_TLB_WINDOW_SIZE: u64 = 1u64 << SMALL_TLB_WINDOW_SHIFT;

/// Translated NoC coordinates of the in-use PCIe tile on Blackhole
/// (p100 / p150). Valid for both NoC #0 and NoC #1.
pub const PCIE_TILE_X: u32 = 19;
pub const PCIE_TILE_Y: u32 = 24;

/// Per-L2CPU small-TLB slot allocation for the shared virtio MMIO
/// buffer in #64. One TLB window covers all four virtio devices on
/// the L2CPU (rng/net/disk/console), each device occupying a 4 KiB
/// sub-region. Each L2CPU has its own 224-slot table, so this
/// index is independent across cores.
pub const SHARED_TLB_SLOT: usize = 0;

/// Program one small (2 MiB) x280 TLB window for unicast access to a
/// single NoC tile.
///
/// `idx` is the small-TLB index in `[0, SMALL_TLB_COUNT)`. `noc_addr` is
/// the target NoC address; the low 21 bits become the offset into the
/// 2 MiB window, the high bits (43 of them, max) populate the window's
/// `local_offset` field.
///
/// Returns the L2CPU PA where the kernel can reach the start of the
/// `noc_addr`-pointed location through this TLB.
pub fn program_small_tlb_unicast(
    l2cpu: &L2Cpu,
    idx: usize,
    target_x: u32,
    target_y: u32,
    noc_addr: u64,
) -> u64 {
    assert!(idx < SMALL_TLB_COUNT, "small TLB idx {} out of range", idx);
    let cfg_base = SMALL_TLB_CFG_BASE + (idx as u64) * SMALL_TLB_CFG_STRIDE;

    let window_size_mask = SMALL_TLB_WINDOW_SIZE - 1;
    let aligned_noc = noc_addr & !window_size_mask;
    let in_window_offset = noc_addr - aligned_noc;
    let local_offset = aligned_noc >> SMALL_TLB_WINDOW_SHIFT;

    // noc_properties_lo bit layout (per BlackholeA0/L2CPUTile/TLBWindows.md):
    //   [0..6)   x_end
    //   [6..12)  y_end
    //   [12..18) x_start  (mcast only — leave 0 for unicast)
    //   [18..24) y_start  (mcast only)
    //   [24]     mcast    (0 = unicast: x_end/y_end name the single tile)
    //   [25..27) ordering (0 = default)
    //   [27]     linked   (0 — unsafe with cached semantics)
    //   [28]     static_vc
    //   [29..31) reserved
    //   [31]     noc_sel  (0 = NoC #0, 1 = NoC #1)
    let props_lo: u32 = (target_x & 0x3F) | ((target_y & 0x3F) << 6);
    let props_hi: u32 = 0;

    // Each cfg slot is 16 bytes: u64 local_offset + u32 lo + u32 hi.
    // Write low 32, high 32 of local_offset, then the two property words.
    l2cpu.write32(cfg_base, local_offset as u32);
    l2cpu.write32(cfg_base + 4, (local_offset >> 32) as u32);
    l2cpu.write32(cfg_base + 8, props_lo);
    l2cpu.write32(cfg_base + 12, props_hi);

    SMALL_TLB_WINDOW_BASE_UC + (idx as u64) * SMALL_TLB_WINDOW_SIZE + in_window_offset
}
