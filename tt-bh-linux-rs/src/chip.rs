// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! Minimal chip-access surface used by the boot sequence.
//!
//! Implements the [`AxiAccess`](crate::boot::AxiAccess) trait that
//! [`boot_l2cpu`](crate::boot::boot_l2cpu) and [`reset_x280`](crate::boot::reset_x280)
//! expect. AXI register reads/writes (`0x8000_xxxx` register block) are routed
//! through a NOC TLB window to tile `(8, 0)` — the same path already used by
//! the PLL-stepping code in [`crate::clock`]. NOC writes go through a TLB
//! window to the requested `(x, y)` tile. Bulk writes iterate 2 MiB TLB
//! windows because that's the window size exposed by the kernel driver.

use std::os::unix::io::RawFd;
use std::ptr;

use crate::boot::AxiAccess;
use crate::kmd;
use crate::tlb::{TlbWindow, TWO_MEG};

/// AXI register tile on Blackhole. Accessing `0x8000_xxxx` via NOC `(8, 0)`
/// hits the same register block that pyluwen's `axi_*` methods hit.
const AXI_TILE_X: u16 = 8;
const AXI_TILE_Y: u16 = 0;

/// Holds an open handle to `/dev/tenstorrent/<card>` for the duration of the
/// boot sequence.
pub struct BootChip {
    fd: RawFd,
}

impl BootChip {
    pub fn new(card: u32) -> std::io::Result<Self> {
        let fd = kmd::open_device(card)?;
        Ok(BootChip { fd })
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    fn axi_window(&self, addr: u64) -> TlbWindow {
        TlbWindow::new_2m(self.fd, AXI_TILE_X, AXI_TILE_Y, addr)
            .expect("failed to create AXI TLB window")
    }

    fn noc_window(&self, x: u16, y: u16, addr: u64) -> TlbWindow {
        TlbWindow::new_2m(self.fd, x, y, addr).expect("failed to create NOC TLB window")
    }

    /// Copy `data` to NOC address `addr` on tile `(x, y)` using a sequence of
    /// 2 MiB TLB windows. Crosses window boundaries cleanly by remapping.
    fn noc_write_bulk(&self, x: u16, y: u16, addr: u64, data: &[u8]) {
        let mut written = 0usize;
        while written < data.len() {
            let cur_addr = addr + written as u64;
            let window_base = cur_addr & !(TWO_MEG as u64 - 1);
            let offset_in_window = (cur_addr - window_base) as usize;
            let remaining_in_window = TWO_MEG - offset_in_window;
            let chunk = remaining_in_window.min(data.len() - written);

            let window = TlbWindow::new_2m(self.fd, x, y, window_base)
                .expect("failed to create NOC TLB window for bulk write");
            unsafe {
                ptr::copy_nonoverlapping(
                    data.as_ptr().add(written),
                    window.data().add(offset_in_window),
                    chunk,
                );
            }
            written += chunk;
        }
    }
}

impl Drop for BootChip {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd); }
    }
}

/// Full board reset mirroring tt-smi's `BHChipReset.full_lds_reset`.
///
/// Run this *before* opening a long-lived [`BootChip`]: the PCI device is
/// re-enumerated across the reset, so any fd held across it returns `ENODEV`
/// on the follow-up `RESTORE_STATE` ioctl. tt-smi sidesteps this by opening a
/// fresh fd for each ioctl step; we do the same.
///
/// Sequence (taken from `tt_tools_common/reset_common/bh_reset.py`):
///   1. Open fd, `CONFIG_WRITE` ioctl (triggers the LDS reset), close fd.
///   2. Poll PCIe config-space byte 4 bit 1 (Memory Space Enable) until it
///      clears to 0, indicating the chip has entered reset.
///   3. Open fd, `RESTORE_STATE` ioctl (restores saved state), close fd.
pub fn reset_board(card: u32) -> std::io::Result<()> {
    let bdf = {
        let fd = kmd::open_device(card)?;
        let info = kmd::get_device_info(fd);
        unsafe { libc::close(fd); }
        let info = info?;
        let domain = info.pci_domain as u32;
        let bus = ((info.bus_dev_fn >> 8) & 0xff) as u32;
        let dev = ((info.bus_dev_fn >> 3) & 0x1f) as u32;
        let func = (info.bus_dev_fn & 0x7) as u32;
        format!("{:04x}:{:02x}:{:02x}.{}", domain, bus, dev, func)
    };
    let config_path = format!("/sys/bus/pci/devices/{}/config", bdf);
    eprintln!(
        "[reset_board] card={} BDF={} config={}",
        card, bdf, config_path
    );

    eprintln!("[reset_board] step 1: open fd + CONFIG_WRITE ioctl (triggers LDS reset)");
    {
        let fd = kmd::open_device(card)?;
        let r = kmd::reset_device(fd, kmd::TENSTORRENT_RESET_DEVICE_CONFIG_WRITE);
        unsafe { libc::close(fd); }
        r?;
    }

    eprintln!("[reset_board] step 2: polling config byte 4 bit 1 for reset completion (max 2s)");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut saw_asserted = false;
    let mut iters = 0u32;
    loop {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&config_path)
            .map_err(|e| std::io::Error::other(format!("open {}: {}", config_path, e)))?;
        f.seek(SeekFrom::Start(4))?;
        let mut byte = [0u8; 1];
        f.read_exact(&mut byte)?;
        let reset_bit = (byte[0] >> 1) & 1;
        iters += 1;
        if reset_bit == 1 {
            saw_asserted = true;
        }
        if reset_bit == 0 && saw_asserted {
            eprintln!(
                "[reset_board]   reset completed after {} polls (command byte={:#04x})",
                iters, byte[0]
            );
            break;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "[reset_board]   timeout after {} polls (last byte={:#04x}, saw_asserted={})",
                iters, byte[0], saw_asserted
            );
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    eprintln!("[reset_board] step 3: open fd + RESTORE_STATE ioctl");
    {
        let fd = kmd::open_device(card)?;
        let r = kmd::reset_device(fd, kmd::TENSTORRENT_RESET_DEVICE_RESTORE_STATE);
        unsafe { libc::close(fd); }
        r?;
    }
    eprintln!("[reset_board] complete");
    Ok(())
}

impl AxiAccess for BootChip {
    fn axi_read32(&self, addr: u64) -> u32 {
        self.axi_window(addr).read32(0)
    }

    fn axi_write32(&self, addr: u64, value: u32) {
        self.axi_window(addr).write32(0, value);
    }

    fn axi_read(&self, addr: u64, buf: &mut [u8]) {
        // Only 32-bit aligned reads are needed by boot; loop them.
        assert!(addr.is_multiple_of(4), "axi_read: addr must be 4-byte aligned");
        assert!(buf.len().is_multiple_of(4), "axi_read: len must be 4-byte aligned");
        let mut off = 0usize;
        while off < buf.len() {
            let v = self.axi_read32(addr + off as u64).to_le_bytes();
            buf[off..off + 4].copy_from_slice(&v);
            off += 4;
        }
    }

    fn axi_write(&self, addr: u64, data: &[u8]) {
        assert!(addr.is_multiple_of(4), "axi_write: addr must be 4-byte aligned");
        assert!(data.len().is_multiple_of(4), "axi_write: len must be 4-byte aligned");
        let mut off = 0usize;
        while off < data.len() {
            let v = u32::from_le_bytes(data[off..off + 4].try_into().unwrap());
            self.axi_write32(addr + off as u64, v);
            off += 4;
        }
    }

    fn noc_read32(&self, _noc: u8, x: u16, y: u16, addr: u64) -> u32 {
        self.noc_window(x, y, addr).read32(0)
    }

    fn noc_write32(&self, _noc: u8, x: u16, y: u16, addr: u64, value: u32) {
        self.noc_window(x, y, addr).write32(0, value);
    }

    fn noc_write(&self, _noc: u8, x: u16, y: u16, addr: u64, data: &[u8]) {
        self.noc_write_bulk(x, y, addr, data);
    }
}
