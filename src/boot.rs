// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Boot loader — loads firmware, kernel, and DTB into X280 DRAM via luwen.
//!
//! This module replicates boot.py functionality using the luwen Rust crate directly.
//! Since luwen may not be available as a crate dependency, this module provides
//! the boot sequence logic that can be connected when luwen becomes available.

use std::fs;
use std::path::Path;

use crate::clock::{self, PllAccess};
use crate::fdt_ffi::Fdt;
use crate::l2cpu::L2CPU_TILES;

/// Read a binary file and pad to 4-byte alignment.
pub fn read_bin_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut data = fs::read(path)?;
    let padding = data.len() % 4;
    if padding != 0 {
        data.extend(std::iter::repeat_n(0u8, 4 - padding));
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

/// Check if the given L2CPU is currently released from reset (i.e. running).
///
/// In `L2CPU_RESET` at `0x80030014`, bit `idx + 4` is the release bit: 0 means
/// held in reset, 1 means running. The register sits in AXI tile `(8, 0)` and
/// is readable regardless of L2CPU state.
pub fn l2cpu_is_running(chip: &dyn AxiAccess, l2cpu_idx: usize) -> bool {
    let reset_reg: u64 = 0x80030014;
    let val = chip.axi_read32(reset_reg);
    let bit_idx = l2cpu_idx + 4;
    let running = (val >> bit_idx) & 1 == 1;
    eprintln!(
        "[l2cpu_is_running] L2CPU_RESET@0x{:x}={:#010x}, bit {}={}, running={}",
        reset_reg,
        val,
        bit_idx,
        (val >> bit_idx) & 1,
        running,
    );
    running
}

/// Reset the X280 CPUs via the reset unit.
///
/// In `L2CPU_RESET` at `reset_unit_base + 0x14`, bit `idx + 4` releases L2CPU
/// `idx` from reset when set. This mirrors boot.py exactly: a preceding PCIe
/// link reset is assumed to have zeroed the register, so a pure OR-in is an
/// effective 0→1 edge. Calling this on a running L2CPU *in place* (without a
/// prior link reset) is not supported — it will leave PCIe/NOC traffic in
/// flight and has been observed to hard-crash the host.
pub fn reset_x280(chip: &dyn AxiAccess, l2cpu_indices: &[usize]) {
    let reset_unit_base: u64 = 0x80030000;
    let reset_reg = reset_unit_base + 0x14;

    eprintln!("[reset_x280] stepping PLL down to 200 MHz");
    let access = AxiPllAccess { chip };
    clock::set_frequency(&access, 200);

    let reset_val_before = chip.axi_read32(reset_reg);
    let mut reset_val = reset_val_before;
    let mut mask: u32 = 0;
    for &idx in l2cpu_indices {
        mask |= 1 << (idx + 4);
        reset_val |= 1 << (idx + 4);
    }
    eprintln!(
        "[reset_x280] L2CPU_RESET@0x{:x}: {:#010x} | {:#010x} -> {:#010x} (releasing L2CPU {:?})",
        reset_reg, reset_val_before, mask, reset_val, l2cpu_indices
    );
    chip.axi_write32(reset_reg, reset_val);
    let reset_val_after = chip.axi_read32(reset_reg);
    eprintln!("[reset_x280] L2CPU_RESET readback: {:#010x}", reset_val_after);

    eprintln!("[reset_x280] stepping PLL up to 1750 MHz");
    clock::set_frequency(&access, 1750);
    eprintln!("[reset_x280] done");
}

/// Boot sequence for a single L2CPU.
#[allow(clippy::too_many_arguments)]
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
    eprintln!(
        "[boot_l2cpu] L2CPU {} -> tile ({}, {}), l2cpu_base=0x{:x}",
        l2cpu_idx, tile.x, tile.y, l2cpu_base
    );

    let l3_reg_base: u64 = 0x02010000;
    eprintln!("[boot_l2cpu] enabling L3 cache at 0x{:x}+8", l3_reg_base);
    chip.noc_write32(0, tile.x, tile.y, l3_reg_base + 8, 0x0f);
    let l3_readback = chip.noc_read32(0, tile.x, tile.y, l3_reg_base + 8);
    eprintln!("[boot_l2cpu]   L3 readback: {:#x}", l3_readback);

    let opensbi_bytes = read_bin_file(opensbi_path)?;
    eprintln!(
        "[boot_l2cpu] Writing OpenSBI ({} bytes from {}) to 0x{:x}",
        opensbi_bytes.len(),
        opensbi_path.display(),
        opensbi_addr
    );
    chip.noc_write(0, tile.x, tile.y, opensbi_addr, &opensbi_bytes);

    if let Some(kpath) = kernel_path {
        let kernel_bytes = read_bin_file(kpath)?;
        eprintln!(
            "[boot_l2cpu] Writing Kernel ({} bytes from {}) to 0x{:x}",
            kernel_bytes.len(),
            kpath.display(),
            kernel_addr
        );
        chip.noc_write(0, tile.x, tile.y, kernel_addr, &kernel_bytes);
    }

    let mut dtb_padded = dtb_bytes.to_vec();
    let padding = dtb_padded.len() % 4;
    if padding != 0 {
        dtb_padded.extend(std::iter::repeat_n(0u8, 4 - padding));
    }
    eprintln!(
        "[boot_l2cpu] Writing DTB ({} bytes, padded to {}) to 0x{:x}",
        dtb_bytes.len(),
        dtb_padded.len(),
        dtb_addr
    );
    chip.noc_write(0, tile.x, tile.y, dtb_addr, &dtb_padded);

    if let Some(rpath) = rootfs_path {
        let rootfs_bytes = read_bin_file(rpath)?;
        eprintln!(
            "[boot_l2cpu] Writing rootfs ({} bytes from {}) to 0x{:x}",
            rootfs_bytes.len(),
            rpath.display(),
            rootfs_addr
        );
        chip.noc_write(0, tile.x, tile.y, rootfs_addr, &rootfs_bytes);
    }

    let reset_vector_0 = (opensbi_addr & 0xffffffff) as u32;
    let reset_vector_1 = (opensbi_addr >> 32) as u32;
    eprintln!(
        "[boot_l2cpu] Setting reset vectors for 4 cores: lo={:#x}, hi={:#x}",
        reset_vector_0, reset_vector_1
    );
    for core in 0..4u64 {
        chip.noc_write32(0, tile.x, tile.y, l2cpu_base + core * 8, reset_vector_0);
        chip.noc_write32(0, tile.x, tile.y, l2cpu_base + core * 8 + 4, reset_vector_1);
    }
    eprintln!("[boot_l2cpu] L2CPU {} image + vectors loaded", l2cpu_idx);

    Ok(())
}

/// Configure L2 prefetchers for a booted L2CPU.
pub fn configure_prefetchers(chip: &dyn AxiAccess, l2cpu_idx: usize) {
    let tile = L2CPU_TILES[l2cpu_idx];
    let l2_prefetch_base: u64 = 0x02030000;
    eprintln!(
        "[configure_prefetchers] L2CPU {} tile ({}, {}) base=0x{:x}",
        l2cpu_idx, tile.x, tile.y, l2_prefetch_base
    );
    for offset in &[0x0000u64, 0x2000, 0x4000, 0x6000] {
        chip.noc_write32(0, tile.x, tile.y, l2_prefetch_base + offset, 0x15811);
        chip.noc_write32(0, tile.x, tile.y, l2_prefetch_base + offset + 4, 0x38c84e);
    }
    eprintln!("[configure_prefetchers] done");
}

/// Boot-device selection for the guest kernel. Controls the `bootargs` value
/// and whether an initramfs is referenced.
#[derive(Debug, Clone)]
pub enum BootDevice {
    /// `root=/dev/vda` or similar — a virtio-block backed rootfs.
    Vda(String),
    /// `initrd=<addr>,<len>` — no persistent disk, use the in-memory image.
    Initramfs { addr: u64, len: u64 },
}

/// Patch a DTB to match the layout boot.py produces.
///
/// Adds `/chosen/bootargs`, a `reserved-memory` entry for the virtio MMIO
/// region, and four virtio MMIO nodes under `/soc`. `mem_end` is computed by
/// the caller from the target L2CPU's `starting_address + memory_size` so we
/// don't depend on being able to parse every vendor's memory-node naming.
pub fn modify_dtb(
    dtb_bytes: &[u8],
    boot_device: &BootDevice,
    mem_start: u64,
    mem_size: u64,
) -> Result<Vec<u8>, String> {
    let mem_end = mem_start + mem_size;
    eprintln!(
        "[modify_dtb] input DTB {} bytes, mem=[0x{:x}..0x{:x}) ({} MB), boot_device={:?}",
        dtb_bytes.len(),
        mem_start,
        mem_end,
        mem_size / (1024 * 1024),
        boot_device
    );
    let mut fdt = Fdt::open_into(dtb_bytes, 2000)?;

    // Patch /memory@400030000000 so the guest kernel sees only the memory
    // this L2CPU actually owns. The input DTB is baked for L2CPU 0 (4 GiB
    // starting at 0x4000_3000_0000); without this patch, booting L2CPU 2 or
    // 3 (2 GiB each, with L2CPU 3 starting at 0x4000_b000_0000) leaves the
    // kernel thinking it has 4 GiB and allocating virtio buffers past the
    // end of its DRAM window, which our server then rejects as out-of-range.
    // boot.py has the same bug — it reads but never writes /memory.
    let memory_node = fdt
        .path_offset("/memory@400030000000")
        .ok_or_else(|| "memory@400030000000 node not found in DT".to_string())?;
    let mut reg = Vec::with_capacity(16);
    reg.extend_from_slice(&mem_start.to_be_bytes());
    reg.extend_from_slice(&mem_size.to_be_bytes());
    fdt.setprop(memory_node, "reg", &reg)?;
    eprintln!(
        "[modify_dtb]   /memory reg patched -> start=0x{:x} size=0x{:x}",
        mem_start, mem_size
    );

    let chosen = match fdt.path_offset("/chosen") {
        Some(o) => o,
        None => fdt.add_subnode(0, "chosen")?,
    };
    let bootargs = match boot_device {
        BootDevice::Vda(dev) => format!("rw console=hvc0 earlycon=sbi root=/dev/{}", dev),
        BootDevice::Initramfs { addr, len } => {
            format!("rw console=hvc0 earlycon=sbi initrd=0x{:x},{}", addr, len)
        }
    };
    eprintln!("[modify_dtb]   bootargs = {:?}", bootargs);
    let mut bootargs_bytes = bootargs.into_bytes();
    bootargs_bytes.push(0);
    fdt.setprop(chosen, "bootargs", &bootargs_bytes)?;

    // /reserved-memory (create if missing, mirroring boot.py)
    let reserved = match fdt.path_offset("/reserved-memory") {
        Some(o) => o,
        None => {
            let r = fdt.add_subnode(0, "reserved-memory")?;
            fdt.setprop_u32(r, "#address-cells", 2)?;
            fdt.setprop_u32(r, "#size-cells", 2)?;
            fdt.setprop(r, "ranges", &[])?;
            r
        }
    };
    let virtio_reserved = fdt.add_subnode(reserved, "memory@4000afa00000")?;
    let reserved_reg = {
        let mut buf = Vec::with_capacity(16);
        buf.extend_from_slice(&(mem_end - 0x600000).to_be_bytes());
        buf.extend_from_slice(&0x600000u64.to_be_bytes());
        buf
    };
    fdt.setprop(virtio_reserved, "reg", &reserved_reg)?;
    fdt.setprop(virtio_reserved, "no-map", &[])?;

    // /soc and PLIC phandle
    let soc = fdt
        .path_offset("/soc")
        .ok_or_else(|| "soc node not found in DT".to_string())?;
    let plic = fdt
        .path_offset("/soc/interrupt-controller@c000000")
        .ok_or_else(|| "plic node not found in DT".to_string())?;
    let mut plic_phandle = fdt.get_phandle(plic);
    if plic_phandle == 0 {
        plic_phandle = fdt.find_max_phandle()? + 1;
        eprintln!("[modify_dtb]   PLIC had no phandle, allocating {}", plic_phandle);
        fdt.setprop_u32(plic, "phandle", plic_phandle)?;
    } else {
        eprintln!("[modify_dtb]   PLIC phandle = {}", plic_phandle);
    }

    for i in (0..4u64).rev() {
        let virtio_addr = mem_end - 0x200000 * (i + 1);
        let virtio_irq = 33 - i as u32;
        let name = format!("virtio@{:x}", virtio_addr);
        eprintln!(
            "[modify_dtb]   adding {} irq={} parent={}",
            name, virtio_irq, plic_phandle
        );
        let node = fdt.add_subnode(soc, &name)?;
        fdt.setprop_string(node, "compatible", "virtio,mmio")?;
        let mut reg = Vec::with_capacity(16);
        reg.extend_from_slice(&virtio_addr.to_be_bytes());
        reg.extend_from_slice(&0x200000u64.to_be_bytes());
        fdt.setprop(node, "reg", &reg)?;
        fdt.setprop_u32(node, "interrupts", virtio_irq)?;
        fdt.setprop_u32(node, "interrupt-parent", plic_phandle)?;
    }

    let packed = fdt.pack()?;
    eprintln!("[modify_dtb] packed DTB {} bytes", packed.len());
    Ok(packed)
}
