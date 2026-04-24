// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! PCIe link-reset helper.
//!
//! Minimal chip-access surface: what used to be `BootChip` (ephemeral per-op
//! TLB windows to AXI tile (8,0) + per-L2CPU NOC tiles) has been retired.
//! AXI tile (8,0) access now lives in [`crate::shared_chip::SharedChip`] as
//! a single persistent mapping with an internal mutex; per-L2CPU NOC writes
//! use [`crate::l2cpu::L2Cpu`]'s persistent fd. What remains here is the raw
//! `RESET_DEVICE` ioctl sequence that the kmd requires a fresh fd for.

use crate::kmd;

/// Full board reset mirroring tt-smi's `BHChipReset.full_lds_reset`.
///
/// The PCI device is re-enumerated across the reset, so any fd held across
/// it returns `ENODEV` on the follow-up `RESTORE_STATE` ioctl. tt-smi
/// sidesteps this by opening a fresh fd for each ioctl step; we do the same.
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
