// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! Boot loader — loads firmware, kernel, and DTB into X280 DRAM via luwen.
//!
//! This module replicates boot.py functionality using the luwen Rust crate directly.
//! Since luwen may not be available as a crate dependency, this module provides
//! the boot sequence logic that can be connected when luwen becomes available.

use std::fs;
use std::path::Path;

use crate::clock::{self, PllAccess};
use crate::l2cpu::L2CPU_TILES;

/// Read a binary file and pad to 4-byte alignment.
pub fn read_bin_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut data = fs::read(path)?;
    let padding = data.len() % 4;
    if padding != 0 {
        data.extend(std::iter::repeat(0u8).take(4 - padding));
    }
    Ok(data)
}

/// PLL access via AXI (used at boot time through luwen).
/// This is a placeholder — the actual implementation requires the luwen crate.
pub struct AxiPllAccess<'a> {
    pub chip: &'a dyn AxiAccess,
}

/// Trait abstracting AXI register access (provided by luwen's PciChip).
pub trait AxiAccess {
    fn axi_read32(&self, addr: u64) -> u32;
    fn axi_write32(&self, addr: u64, value: u32);
    fn axi_read(&self, addr: u64, buf: &mut [u8]);
    fn axi_write(&self, addr: u64, data: &[u8]);
    fn noc_read32(&self, noc: u8, x: u16, y: u16, addr: u64) -> u32;
    fn noc_write32(&self, noc: u8, x: u16, y: u16, addr: u64, value: u32);
    fn noc_write(&self, noc: u8, x: u16, y: u16, addr: u64, data: &[u8]);
}

impl<'a> PllAccess for AxiPllAccess<'a> {
    fn pll_read32(&self, addr: u64) -> u32 {
        let mut buf = [0u8; 4];
        self.chip.axi_read(addr, &mut buf);
        u32::from_le_bytes(buf)
    }

    fn pll_write32(&self, addr: u64, value: u32) {
        self.chip.axi_write(addr, &value.to_le_bytes());
    }
}

/// Reset the X280 CPUs via the reset unit.
pub fn reset_x280(chip: &dyn AxiAccess, l2cpu_indices: &[usize]) {
    let reset_unit_base: u64 = 0x80030000;

    // Step down to 200MHz
    let access = AxiPllAccess { chip };
    clock::set_frequency(&access, 200);

    // Assert reset for each L2CPU
    let mut reset_val = chip.axi_read32(reset_unit_base + 0x14);
    for &idx in l2cpu_indices {
        reset_val |= 1 << (idx + 4);
    }
    chip.axi_write32(reset_unit_base + 0x14, reset_val);
    // Read-back to ensure write committed
    let _ = chip.axi_read32(reset_unit_base + 0x14);

    // Step up to 1750MHz
    clock::set_frequency(&access, 1750);
}

/// Boot sequence for a single L2CPU.
pub fn boot_l2cpu(
    chip: &dyn AxiAccess,
    l2cpu_idx: usize,
    opensbi_path: &Path,
    opensbi_addr: u64,
    kernel_path: Option<&Path>,
    kernel_addr: u64,
    dtb_bytes: &[u8],
    dtb_addr: u64,
    rootfs_path: Option<&Path>,
    rootfs_addr: u64,
) -> std::io::Result<()> {
    let tile = L2CPU_TILES[l2cpu_idx];
    let l2cpu_base: u64 = 0xfffff7fefff10000;

    // Enable L3 cache
    let l3_reg_base: u64 = 0x02010000;
    chip.noc_write32(0, tile.x, tile.y, l3_reg_base + 8, 0x0f);
    let _ = chip.noc_read32(0, tile.x, tile.y, l3_reg_base + 8);

    // Load OpenSBI
    let opensbi_bytes = read_bin_file(opensbi_path)?;
    eprintln!("Writing OpenSBI to 0x{:x}", opensbi_addr);
    chip.noc_write(0, tile.x, tile.y, opensbi_addr, &opensbi_bytes);

    // Load kernel if provided
    if let Some(kpath) = kernel_path {
        let kernel_bytes = read_bin_file(kpath)?;
        eprintln!("Writing Kernel to 0x{:x}", kernel_addr);
        chip.noc_write(0, tile.x, tile.y, kernel_addr, &kernel_bytes);
    }

    // Load DTB
    eprintln!("Writing DTB to 0x{:x}", dtb_addr);
    let mut dtb_padded = dtb_bytes.to_vec();
    let padding = dtb_padded.len() % 4;
    if padding != 0 {
        dtb_padded.extend(std::iter::repeat(0u8).take(4 - padding));
    }
    chip.noc_write(0, tile.x, tile.y, dtb_addr, &dtb_padded);

    // Load rootfs if provided (initramfs mode)
    if let Some(rpath) = rootfs_path {
        let rootfs_bytes = read_bin_file(rpath)?;
        eprintln!("Writing rootfs to 0x{:x}", rootfs_addr);
        chip.noc_write(0, tile.x, tile.y, rootfs_addr, &rootfs_bytes);
    }

    // Set reset vectors for all 4 cores
    let reset_vector_0 = (opensbi_addr & 0xffffffff) as u32;
    let reset_vector_1 = (opensbi_addr >> 32) as u32;
    for core in 0..4u64 {
        chip.noc_write32(0, tile.x, tile.y, l2cpu_base + core * 8, reset_vector_0);
        chip.noc_write32(0, tile.x, tile.y, l2cpu_base + core * 8 + 4, reset_vector_1);
    }

    Ok(())
}

/// Configure L2 prefetchers for a booted L2CPU.
pub fn configure_prefetchers(chip: &dyn AxiAccess, l2cpu_idx: usize) {
    let tile = L2CPU_TILES[l2cpu_idx];
    let l2_prefetch_base: u64 = 0x02030000;
    for offset in &[0x0000u64, 0x2000, 0x4000, 0x6000] {
        chip.noc_write32(0, tile.x, tile.y, l2_prefetch_base + offset, 0x15811);
        chip.noc_write32(0, tile.x, tile.y, l2_prefetch_base + offset + 4, 0x38c84e);
    }
}

/// Modify a DTB to add bootargs, reserved memory, and virtio devices.
///
/// This function takes raw DTB bytes and returns modified DTB bytes.
/// Full DTB patching requires libfdt bindings or a Rust FDT library that
/// supports modification of existing DTBs.
pub fn modify_dtb(
    dtb_bytes: &[u8],
    _boot_device: &str,
    _mem_end: u64,
) -> Result<Vec<u8>, String> {
    // DTB modification requires parsing and modifying an existing FDT.
    // For a complete implementation, we need Rust bindings to libfdt or a
    // Rust FDT library that supports modification.
    // For now, the boot.py script should be used for DTB modification.

    // In a full implementation, we would:
    // 1. Parse DTB, resize with +2000 bytes
    // 2. Add/set /chosen/bootargs
    // 3. Parse /memory@400030000000/reg -> mem_start, mem_size -> mem_end
    // 4. Add /reserved-memory with reg=(mem_end-0x600000, 0x600000), no-map
    // 5. Get/create PLIC phandle
    // 6. Add 4 virtio,mmio nodes under /soc

    // For now, return the input unchanged
    eprintln!("WARNING: DTB modification not yet implemented in Rust. Use boot.py for DTB patching.");
    Ok(dtb_bytes.to_vec())
}
