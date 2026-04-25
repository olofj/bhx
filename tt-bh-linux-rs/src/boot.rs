// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! Boot-time per-L2CPU work: loading OpenSBI + kernel + DTB into the core's
//! DRAM, patching the DTB, writing reset vectors, enabling L3, configuring
//! prefetchers.
//!
//! All per-core register / NOC access goes through [`crate::l2cpu::L2Cpu`]
//! (persistent per-L2CPU fd + TLB windows). Chip-wide AXI tile (8,0) work
//! (`L2CPU_RESET` R-M-W, PLL stepping, PCIe reset) lives in
//! [`crate::shared_chip::SharedChip`].

use std::fs;
use std::path::Path;

use crate::fdt_ffi::Fdt;
use crate::l2cpu::{L2Cpu, L2CPU_TILES};

/// Read a binary file and pad to 4-byte alignment.
pub fn read_bin_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut data = fs::read(path)?;
    let padding = data.len() % 4;
    if padding != 0 {
        data.extend(std::iter::repeat_n(0u8, 4 - padding));
    }
    Ok(data)
}

/// Bulk-write `data` to the given NOC address on the L2CPU's own tile, using
/// a transient uncacheable 2 MiB TLB window per chunk. Ordering semantics
/// match the old `BootChip::noc_write_bulk` (UC stores are strictly ordered
/// at the device), but the allocation goes through `L2Cpu`'s per-card fd
/// and its `alloc_lock` — so concurrent bulk writes on different L2CPUs
/// don't stomp each other's kmd state and no longer touch the shared AXI
/// tile (8,0).
fn l2cpu_noc_write_bulk(l2cpu: &L2Cpu, addr: u64, data: &[u8]) -> std::io::Result<()> {
    const TWO_MEG: u64 = crate::tlb::TWO_MEG as u64;
    let mut written: u64 = 0;
    let total = data.len() as u64;
    while written < total {
        let cur_addr = addr + written;
        let window_base = cur_addr & !(TWO_MEG - 1);
        let offset_in_window = (cur_addr - window_base) as usize;
        let remaining_in_window = TWO_MEG - offset_in_window as u64;
        let chunk = remaining_in_window.min(total - written) as usize;

        // Transient UC 2 MiB window on this L2CPU's tile via its persistent
        // fd. `get_persistent_2m_window` is a misnomer — it creates a new
        // window that's dropped when we drop our handle (after this chunk);
        // the "persistent" in its name refers to the caller holding it, not
        // to it outliving a single use.
        let window = l2cpu.get_persistent_2m_window(window_base)?;
        let dst = unsafe { window.get_window().add(offset_in_window) };
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr().add(written as usize),
                dst,
                chunk,
            );
        }
        written += chunk as u64;
    }
    Ok(())
}

/// Boot sequence for a single L2CPU.
#[allow(clippy::too_many_arguments)]
pub fn boot_l2cpu(
    l2cpu: &L2Cpu,
    opensbi_path: &Path,
    opensbi_addr: u64,
    kernel_path: Option<&Path>,
    kernel_addr: u64,
    dtb_bytes: &[u8],
    dtb_addr: u64,
    rootfs_path: Option<&Path>,
    rootfs_addr: u64,
) -> std::io::Result<()> {
    use crate::regs::l2cpu as regs_l2cpu;

    let l2cpu_idx = l2cpu.idx();
    let tile = L2CPU_TILES[l2cpu_idx];
    eprintln!(
        "[boot_l2cpu] L2CPU {} -> tile ({}, {}), control_base=0x{:x}",
        l2cpu_idx, tile.x, tile.y, regs_l2cpu::CONTROL_BASE
    );

    eprintln!(
        "[boot_l2cpu] enabling L3 cache at 0x{:x}+{}",
        regs_l2cpu::L3_CTRL_BASE, regs_l2cpu::L3_ENABLE_OFFSET
    );
    let l3_enable_addr = regs_l2cpu::L3_CTRL_BASE + regs_l2cpu::L3_ENABLE_OFFSET;
    l2cpu.write32(l3_enable_addr, regs_l2cpu::L3_ENABLE_VALUE);
    let l3_readback = l2cpu.read32(l3_enable_addr);
    eprintln!("[boot_l2cpu]   L3 readback: {:#x}", l3_readback);

    let opensbi_bytes = read_bin_file(opensbi_path)?;
    eprintln!(
        "[boot_l2cpu] Writing OpenSBI ({} bytes from {}) to 0x{:x}",
        opensbi_bytes.len(),
        opensbi_path.display(),
        opensbi_addr
    );
    l2cpu_noc_write_bulk(l2cpu, opensbi_addr, &opensbi_bytes)?;

    if let Some(kpath) = kernel_path {
        let kernel_bytes = read_bin_file(kpath)?;
        eprintln!(
            "[boot_l2cpu] Writing Kernel ({} bytes from {}) to 0x{:x}",
            kernel_bytes.len(),
            kpath.display(),
            kernel_addr
        );
        l2cpu_noc_write_bulk(l2cpu, kernel_addr, &kernel_bytes)?;
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
    l2cpu_noc_write_bulk(l2cpu, dtb_addr, &dtb_padded)?;

    if let Some(rpath) = rootfs_path {
        let rootfs_bytes = read_bin_file(rpath)?;
        eprintln!(
            "[boot_l2cpu] Writing rootfs ({} bytes from {}) to 0x{:x}",
            rootfs_bytes.len(),
            rpath.display(),
            rootfs_addr
        );
        l2cpu_noc_write_bulk(l2cpu, rootfs_addr, &rootfs_bytes)?;
    }

    let reset_vector_0 = (opensbi_addr & 0xffff_ffff) as u32;
    let reset_vector_1 = (opensbi_addr >> 32) as u32;
    eprintln!(
        "[boot_l2cpu] Setting reset vectors for 4 cores: lo={:#x}, hi={:#x}",
        reset_vector_0, reset_vector_1
    );
    for core in 0..4u64 {
        l2cpu.write32(regs_l2cpu::CONTROL_BASE + core * 8, reset_vector_0);
        l2cpu.write32(regs_l2cpu::CONTROL_BASE + core * 8 + 4, reset_vector_1);
    }
    eprintln!("[boot_l2cpu] L2CPU {} image + vectors loaded", l2cpu_idx);

    Ok(())
}

/// Configure L2 prefetchers for a booted L2CPU.
pub fn configure_prefetchers(l2cpu: &L2Cpu) {
    use crate::regs::l2cpu as regs_l2cpu;

    let l2cpu_idx = l2cpu.idx();
    let tile = L2CPU_TILES[l2cpu_idx];
    eprintln!(
        "[configure_prefetchers] L2CPU {} tile ({}, {}) base=0x{:x}",
        l2cpu_idx, tile.x, tile.y, regs_l2cpu::L2_PREFETCH_BASE
    );
    for i in 0..regs_l2cpu::L2_PREFETCH_NUM {
        let base = regs_l2cpu::L2_PREFETCH_BASE + i * regs_l2cpu::L2_PREFETCH_STRIDE;
        l2cpu.write32(base, regs_l2cpu::L2_PREFETCH_CFG_LO);
        l2cpu.write32(base + 4, regs_l2cpu::L2_PREFETCH_CFG_HI);
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
        use crate::regs::virtio_mmio::RESERVED_SIZE;
        let mut buf = Vec::with_capacity(16);
        buf.extend_from_slice(&(mem_end - RESERVED_SIZE).to_be_bytes());
        buf.extend_from_slice(&RESERVED_SIZE.to_be_bytes());
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

    {
        use crate::regs::virtio_mmio::{DISK_IRQ, MMIO_SLOT_SIZE};
        for i in (0..4u64).rev() {
            let virtio_addr = mem_end - MMIO_SLOT_SIZE * (i + 1);
            // Slot 0 (lowest in the reservation) has the largest IRQ
            // (DISK_IRQ); the four slots descend by IRQ number.
            let virtio_irq = DISK_IRQ - i as u32;
            let name = format!("virtio@{:x}", virtio_addr);
            eprintln!(
                "[modify_dtb]   adding {} irq={} parent={}",
                name, virtio_irq, plic_phandle
            );
            let node = fdt.add_subnode(soc, &name)?;
            fdt.setprop_string(node, "compatible", "virtio,mmio")?;
            let mut reg = Vec::with_capacity(16);
            reg.extend_from_slice(&virtio_addr.to_be_bytes());
            reg.extend_from_slice(&MMIO_SLOT_SIZE.to_be_bytes());
            fdt.setprop(node, "reg", &reg)?;
            fdt.setprop_u32(node, "interrupts", virtio_irq)?;
            fdt.setprop_u32(node, "interrupt-parent", plic_phandle)?;
        }
    }

    let packed = fdt.pack()?;
    eprintln!("[modify_dtb] packed DTB {} bytes", packed.len());
    Ok(packed)
}
