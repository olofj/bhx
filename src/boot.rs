// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

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
            std::ptr::copy_nonoverlapping(data.as_ptr().add(written as usize), dst, chunk);
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
    // Defensive: every caller today is the daemon, which has already
    // gone through `validate_l2cpu`. If a future debug subcommand
    // routes around the daemon and passes a stale/garbage idx, the
    // raw array index panic message would be cryptic. Catch it here
    // with a useful one.
    assert!(
        l2cpu_idx < L2CPU_TILES.len(),
        "boot_l2cpu: l2cpu_idx {} out of range (have {} tiles)",
        l2cpu_idx,
        L2CPU_TILES.len()
    );
    let tile = L2CPU_TILES[l2cpu_idx];
    eprintln!(
        "[boot_l2cpu] L2CPU {} -> tile ({}, {}), control_base=0x{:x}",
        l2cpu_idx,
        tile.x,
        tile.y,
        regs_l2cpu::CONTROL_BASE
    );

    eprintln!(
        "[boot_l2cpu] enabling L3 cache at 0x{:x}+{}",
        regs_l2cpu::L3_CTRL_BASE,
        regs_l2cpu::L3_ENABLE_OFFSET
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
    assert!(
        l2cpu_idx < L2CPU_TILES.len(),
        "configure_prefetchers: l2cpu_idx {} out of range (have {} tiles)",
        l2cpu_idx,
        L2CPU_TILES.len()
    );
    let tile = L2CPU_TILES[l2cpu_idx];
    eprintln!(
        "[configure_prefetchers] L2CPU {} tile ({}, {}) base=0x{:x}",
        l2cpu_idx,
        tile.x,
        tile.y,
        regs_l2cpu::L2_PREFETCH_BASE
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
    /// `initrd=<addr>,<len> root=/dev/<dev>` — a distro-style boot:
    /// kernel + dracut (or similar) initramfs that pivot_root's onto
    /// the persistent disk. dracut needs `root=` on the cmdline to
    /// know where to mount; without it, switch_root drops to an
    /// emergency shell. Used when the operator passes both
    /// `--initramfs` and `--disk` to `boot`.
    InitramfsAndVda { addr: u64, len: u64, dev: String },
    /// U-Boot is the payload at KERNEL_OFFSET; it discovers root +
    /// initrd at runtime from disk. Daemon-side bootargs is left as a
    /// minimal `console=hvc0` so any kernel U-Boot eventually `booti`s
    /// gets a working console even if U-Boot doesn't override the
    /// cmdline. See #44.
    Uboot,
}

/// One virtio-mmio node to inject under `/soc` in the patched DTB.
/// `addr` and `size` go into the node's `reg` property; `irq` ties the
/// node to the PLIC. Order in the input slice doesn't matter; the
/// nodes are emitted independently. Used to support both chip-DRAM and
/// host-buffer (#64) virtio-mmio placement in the same kernel boot.
#[derive(Debug, Clone, Copy)]
pub struct VirtioMmioNode {
    pub addr: u64,
    pub size: u64,
    pub irq: u32,
}

/// Patch a DTB so the guest kernel sees the per-L2CPU memory range, the
/// virtio-mmio devices the daemon emulates, and the bootargs / SBI console.
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
    virtio_nodes: &[VirtioMmioNode],
    uart_addr: Option<u64>,
) -> crate::Result<Vec<u8>> {
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
    // (See #85: the unit name still says `@400030000000` even on L2CPU 3,
    // which is cosmetic — the kernel reads `reg`, not the unit name.)
    let memory_node = fdt.path_offset("/memory@400030000000")?.ok_or_else(|| {
        crate::Error::fdt("path_offset", "memory@400030000000 node not found in DT")
    })?;
    let mut reg = Vec::with_capacity(16);
    reg.extend_from_slice(&mem_start.to_be_bytes());
    reg.extend_from_slice(&mem_size.to_be_bytes());
    fdt.setprop(memory_node, "reg", &reg)?;
    eprintln!(
        "[modify_dtb]   /memory reg patched -> start=0x{:x} size=0x{:x}",
        mem_start, mem_size
    );

    let chosen = match fdt.path_offset("/chosen")? {
        Some(o) => o,
        None => fdt.add_subnode(0, "chosen")?,
    };
    let bootargs = match boot_device {
        BootDevice::Vda(dev) => format!(
            "rw console=hvc0 earlycon=sbi keep_bootcon root=/dev/{}",
            dev
        ),
        BootDevice::Initramfs { addr, len } => {
            format!(
                "rw console=hvc0 earlycon=sbi keep_bootcon initrd=0x{:x},{}",
                addr, len
            )
        }
        BootDevice::InitramfsAndVda { addr, len, dev } => format!(
            "rw console=hvc0 earlycon=sbi keep_bootcon initrd=0x{:x},{} root=/dev/{}",
            addr, len, dev
        ),
        BootDevice::Uboot => "console=hvc0 earlycon=sbi keep_bootcon".to_string(),
    };
    eprintln!("[modify_dtb]   bootargs = {:?}", bootargs);
    let mut bootargs_bytes = bootargs.into_bytes();
    bootargs_bytes.push(0);
    fdt.setprop(chosen, "bootargs", &bootargs_bytes)?;
    // /chosen/sbi-console — bind point for U-Boot's downstream DM SBI
    // serial driver (third_party/uboot/patches/serial_sbi_dm.c, see #45).
    // The Linux kernel uses its own SBI HVC driver regardless of DT;
    // adding this node is harmless when booting raw kernel mode and
    // required for U-Boot's interactive console.
    let sbi_console = fdt.add_subnode(chosen, "sbi-console")?;
    fdt.setprop(sbi_console, "compatible", b"riscv,sbi-debug-console\0")?;
    fdt.setprop(chosen, "stdout-path", b"/chosen/sbi-console\0")?;

    // /reserved-memory: create if the upstream DTB didn't include one.
    let reserved = match fdt.path_offset("/reserved-memory")? {
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
        .path_offset("/soc")?
        .ok_or_else(|| crate::Error::fdt("path_offset", "soc node not found in DT"))?;
    let plic = fdt
        .path_offset("/soc/interrupt-controller@c000000")?
        .ok_or_else(|| crate::Error::fdt("path_offset", "plic node not found in DT"))?;
    let mut plic_phandle = fdt.get_phandle(plic);
    if plic_phandle == 0 {
        plic_phandle = fdt.find_max_phandle()? + 1;
        eprintln!(
            "[modify_dtb]   PLIC had no phandle, allocating {}",
            plic_phandle
        );
        fdt.setprop_u32(plic, "phandle", plic_phandle)?;
    } else {
        eprintln!("[modify_dtb]   PLIC phandle = {}", plic_phandle);
    }

    for node_spec in virtio_nodes {
        let name = format!("virtio@{:x}", node_spec.addr);
        eprintln!(
            "[modify_dtb]   adding {} size={:#x} irq={} parent={}",
            name, node_spec.size, node_spec.irq, plic_phandle
        );
        let node = fdt.add_subnode(soc, &name)?;
        fdt.setprop_string(node, "compatible", "virtio,mmio")?;
        let mut reg = Vec::with_capacity(16);
        reg.extend_from_slice(&node_spec.addr.to_be_bytes());
        reg.extend_from_slice(&node_spec.size.to_be_bytes());
        fdt.setprop(node, "reg", &reg)?;
        fdt.setprop_u32(node, "interrupts", node_spec.irq)?;
        fdt.setprop_u32(node, "interrupt-parent", plic_phandle)?;
    }

    // /soc/serial@<UART_PA> — M6 (#78) 16550 UART, TX-only first cut.
    // Emitted under /soc so distro kernels with `console=ttyS0` find
    // a real backing device. `reg-shift = 2` matches our 4-byte stride
    // reg file; `clock-frequency` is purely informational (Linux's
    // 8250 driver doesn't reprogram the divisor on a memory-mapped
    // device that already has a `current-speed` set).
    //
    // The caller controls whether this node lands in the DTB (pass
    // `None` to suppress). Adding it makes the Tenstorrent-built
    // OpenSBI pick the ns16550a as its M-mode console — which is
    // the lossy 8250 emulation per #79 — instead of its DBCN
    // debug-console path that's drained byte-cleanly by
    // `chip_console.rs`. Today the daemon defaults to `None` and
    // operators opt in via boot flag when a stock distro insists
    // on `console=ttyS0`.
    if let Some(addr) = uart_addr {
        let name = format!("serial@{:x}", addr);
        eprintln!(
            "[modify_dtb]   adding {} size=0x1000 irq={} (UART, TX-only)",
            name,
            crate::regs::virtio_mmio::UART_IRQ,
        );
        let node = fdt.add_subnode(soc, &name)?;
        fdt.setprop_string(node, "compatible", "ns16550a")?;
        let mut reg = Vec::with_capacity(16);
        reg.extend_from_slice(&addr.to_be_bytes());
        reg.extend_from_slice(&0x1000u64.to_be_bytes());
        fdt.setprop(node, "reg", &reg)?;
        fdt.setprop_u32(node, "reg-shift", 2)?;
        fdt.setprop_u32(node, "reg-io-width", 4)?;
        fdt.setprop_u32(node, "clock-frequency", 1843200)?;
        fdt.setprop_u32(node, "current-speed", 115200)?;
        fdt.setprop_u32(node, "interrupts", crate::regs::virtio_mmio::UART_IRQ)?;
        fdt.setprop_u32(node, "interrupt-parent", plic_phandle)?;
    }

    let packed = fdt.pack()?;
    eprintln!("[modify_dtb] packed DTB {} bytes", packed.len());
    Ok(packed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Pinned copy of `blackhole-card.dtb` for hardware-free tests.
    /// Updating the on-card DTB requires re-copying this fixture.
    const FIXTURE_DTB: &[u8] = include_bytes!("../tests/fixtures/blackhole-card.dtb");

    #[test]
    fn read_bin_file_pads_to_4byte_alignment() {
        let mut tf = tempfile::NamedTempFile::new().unwrap();
        tf.write_all(&[1, 2, 3]).unwrap();
        let out = read_bin_file(tf.path()).unwrap();
        assert_eq!(out, vec![1, 2, 3, 0]);
    }

    #[test]
    fn read_bin_file_leaves_aligned_files_alone() {
        for size in [0usize, 4, 8, 16] {
            let mut tf = tempfile::NamedTempFile::new().unwrap();
            tf.write_all(&vec![0xa5u8; size]).unwrap();
            let out = read_bin_file(tf.path()).unwrap();
            assert_eq!(out.len(), size, "size {} should be unchanged", size);
        }
    }

    /// Locate `/memory@400030000000` and parse its `reg` property as
    /// (start, size) — the layout `boot::modify_dtb` writes is two
    /// big-endian u64s.
    fn read_memory_reg(dtb: &[u8]) -> (u64, u64) {
        let fdt = Fdt::open_into(dtb, 0).unwrap();
        let node = fdt.path_offset("/memory@400030000000").unwrap().unwrap();
        let reg = fdt.getprop(node, "reg").unwrap();
        assert_eq!(reg.len(), 16);
        let start = u64::from_be_bytes(reg[0..8].try_into().unwrap());
        let size = u64::from_be_bytes(reg[8..16].try_into().unwrap());
        (start, size)
    }

    #[test]
    fn modify_dtb_patches_memory_node_for_l2cpu_with_2gib() {
        // L2CPU 2/3 each see 2 GiB. Verify modify_dtb writes (start, 2 GiB)
        // into /memory's reg.
        let mem_start = 0x4000_3000_0000u64;
        let mem_size = 0x8000_0000u64; // 2 GiB
        let dev = BootDevice::Vda("vda".to_string());
        let out = modify_dtb(FIXTURE_DTB, &dev, mem_start, mem_size, &[], None).unwrap();
        assert_eq!(read_memory_reg(&out), (mem_start, mem_size));
    }

    #[test]
    fn modify_dtb_bootargs_for_vda_root() {
        let dev = BootDevice::Vda("vda".to_string());
        let out = modify_dtb(
            FIXTURE_DTB,
            &dev,
            0x4000_3000_0000,
            0x1_0000_0000,
            &[],
            None,
        )
        .unwrap();
        let fdt = Fdt::open_into(&out, 0).unwrap();
        let chosen = fdt.path_offset("/chosen").unwrap().unwrap();
        let args = fdt.getprop(chosen, "bootargs").unwrap();
        // Trailing NUL — the fdt setprop wrote a C string.
        let s = std::str::from_utf8(&args[..args.len() - 1]).unwrap();
        assert!(
            s.contains("root=/dev/vda"),
            "bootargs missing root=/dev/vda: {:?}",
            s
        );
        assert!(s.contains("console=hvc0"), "bootargs missing hvc0: {:?}", s);
    }

    #[test]
    fn modify_dtb_bootargs_for_initramfs() {
        let dev = BootDevice::Initramfs {
            addr: 0x4000_3210_0000,
            len: 4096,
        };
        let out = modify_dtb(
            FIXTURE_DTB,
            &dev,
            0x4000_3000_0000,
            0x1_0000_0000,
            &[],
            None,
        )
        .unwrap();
        let fdt = Fdt::open_into(&out, 0).unwrap();
        let chosen = fdt.path_offset("/chosen").unwrap().unwrap();
        let args = fdt.getprop(chosen, "bootargs").unwrap();
        let s = std::str::from_utf8(&args[..args.len() - 1]).unwrap();
        assert!(
            s.contains("initrd=0x4000321"),
            "bootargs missing initrd addr: {:?}",
            s
        );
        assert!(s.contains(",4096"), "bootargs missing initrd len: {:?}", s);
    }

    #[test]
    fn modify_dtb_creates_reserved_memory_subnode_at_top_of_dram() {
        let mem_start = 0x4000_3000_0000u64;
        let mem_size = 0x1_0000_0000u64; // 4 GiB
        let mem_end = mem_start + mem_size;
        let expected_base = mem_end - crate::regs::virtio_mmio::RESERVED_SIZE;

        let dev = BootDevice::Vda("vda".to_string());
        let out = modify_dtb(FIXTURE_DTB, &dev, mem_start, mem_size, &[], None).unwrap();
        let fdt = Fdt::open_into(&out, 0).unwrap();
        let res = fdt
            .path_offset("/reserved-memory/memory@4000afa00000")
            .expect("path_offset shouldn't fail")
            .expect("modify_dtb must create the virtio reserved-memory node");
        assert!(
            fdt.getprop(res, "no-map").is_some(),
            "reserved region must be no-map"
        );
        let reg = fdt.getprop(res, "reg").unwrap();
        let base = u64::from_be_bytes(reg[0..8].try_into().unwrap());
        let size = u64::from_be_bytes(reg[8..16].try_into().unwrap());
        assert_eq!(base, expected_base);
        assert_eq!(size, crate::regs::virtio_mmio::RESERVED_SIZE);
    }

    #[test]
    fn modify_dtb_emits_each_virtio_mmio_node_passed_in() {
        // The chip-DRAM virtio layout used to be hardcoded inside
        // modify_dtb; #64 generalised it to take a caller-supplied list
        // so host-side and chip-DRAM placements can coexist in the same
        // boot. Verify the historical descending-from-mem_end /
        // descending-IRQ shape still works when the caller passes it.
        use crate::regs::virtio_mmio::{DISK_IRQ, MMIO_SLOT_SIZE};
        let mem_start = 0x4000_3000_0000u64;
        let mem_size = 0x1_0000_0000u64;
        let mem_end = mem_start + mem_size;
        let dev = BootDevice::Vda("vda".to_string());
        let nodes: Vec<VirtioMmioNode> = (0..4u64)
            .map(|i| VirtioMmioNode {
                addr: mem_end - MMIO_SLOT_SIZE * (i + 1),
                size: MMIO_SLOT_SIZE,
                irq: DISK_IRQ - i as u32,
            })
            .collect();
        let out = modify_dtb(FIXTURE_DTB, &dev, mem_start, mem_size, &nodes, None).unwrap();
        let fdt = Fdt::open_into(&out, 0).unwrap();

        for spec in &nodes {
            let path = format!("/soc/virtio@{:x}", spec.addr);
            let node = fdt
                .path_offset(&path)
                .expect("path_offset shouldn't fail")
                .unwrap_or_else(|| panic!("missing {}", path));
            let compat = fdt.getprop(node, "compatible").unwrap();
            assert!(
                compat.starts_with(b"virtio,mmio"),
                "{} compatible={:?}",
                path,
                compat
            );
            let irq = fdt.getprop(node, "interrupts").unwrap();
            assert_eq!(irq.len(), 4);
            let irq_val = u32::from_be_bytes(irq.try_into().unwrap());
            assert_eq!(irq_val, spec.irq, "{} irq", path);
        }
    }

    #[test]
    fn modify_dtb_with_empty_virtio_nodes_emits_none() {
        // With #64's host-buffer path, runs that don't have any
        // chip-DRAM virtio devices configured (e.g. host-RNG-only)
        // should produce a DTB with no virtio,mmio children under /soc.
        let mem_start = 0x4000_3000_0000u64;
        let mem_size = 0x1_0000_0000u64;
        let dev = BootDevice::Vda("vda".to_string());
        let out = modify_dtb(FIXTURE_DTB, &dev, mem_start, mem_size, &[], None).unwrap();
        let fdt = Fdt::open_into(&out, 0).unwrap();
        for i in 0..4u64 {
            let addr = mem_start + mem_size - crate::regs::virtio_mmio::MMIO_SLOT_SIZE * (i + 1);
            let path = format!("/soc/virtio@{:x}", addr);
            let r = fdt.path_offset(&path).unwrap();
            assert!(
                r.is_none(),
                "{} should not exist with empty node list",
                path
            );
        }
    }
}
