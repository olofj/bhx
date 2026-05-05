// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! PCIe link-reset helper.
//!
//! Minimal chip-access surface: what used to be `BootChip` (ephemeral per-op
//! TLB windows to the ARC tile (8,0) + per-L2CPU NOC tiles) has been retired.
//! ARC-tile (8,0) access now lives in [`crate::shared_chip::SharedChip`] as
//! a single persistent mapping with an internal mutex; per-L2CPU NOC writes
//! use [`crate::l2cpu::L2Cpu`]'s persistent fd. What remains here is the raw
//! `RESET_DEVICE` ioctl sequence that the kmd requires a fresh fd for.

use crate::kmd;

/// Format a sysfs PCI BDF string (`0000:01:00.0`) from kmd's
/// `pci_domain` field plus the packed `bus_dev_fn` u16. Pulled out
/// for unit testing — the rest of `reset_board` requires hardware.
pub(crate) fn format_bdf(domain: u32, bus_dev_fn: u32) -> String {
    let bus = (bus_dev_fn >> 8) & 0xff;
    let dev = (bus_dev_fn >> 3) & 0x1f;
    let func = bus_dev_fn & 0x7;
    format!("{:04x}:{:02x}:{:02x}.{}", domain, bus, dev, func)
}

/// One-step state transition of the LDS-reset poll loop. We're
/// watching PCI config-space byte 4's bit 1 (Memory Space Enable) and
/// waiting for it to go from "asserted" to "released" — the chip has
/// to ENTER reset (bit=1) before we can declare it has LEFT reset
/// (bit=0). Returns `(completed, new_saw_asserted)`. The loop exits
/// successfully only on `completed=true`.
pub(crate) fn lds_reset_poll_step(byte: u8, saw_asserted: bool) -> (bool, bool) {
    let reset_bit = (byte >> 1) & 1;
    let new_saw = saw_asserted || reset_bit == 1;
    let completed = reset_bit == 0 && new_saw;
    (completed, new_saw)
}

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
        unsafe {
            libc::close(fd);
        }
        let info = info?;
        format_bdf(info.pci_domain as u32, info.bus_dev_fn as u32)
    };
    let config_path = format!("/sys/bus/pci/devices/{}/config", bdf);
    crate::dlog!(
        "[reset_board] card={} BDF={} config={}",
        card,
        bdf,
        config_path
    );

    crate::dlog!("[reset_board] step 1: open fd + CONFIG_WRITE ioctl (triggers LDS reset)");
    {
        let fd = kmd::open_device(card)?;
        let r = kmd::reset_device(fd, kmd::TENSTORRENT_RESET_DEVICE_CONFIG_WRITE);
        unsafe {
            libc::close(fd);
        }
        r?;
    }

    crate::dlog!("[reset_board] step 2: polling config byte 4 bit 1 for reset completion (max 2s)");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut saw_asserted = false;
    let mut iters = 0u32;
    loop {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&config_path).map_err(|e| {
            std::io::Error::from(crate::Error::Io {
                ctx: format!("open {}", config_path),
                source: e,
            })
        })?;
        f.seek(SeekFrom::Start(4))?;
        let mut byte = [0u8; 1];
        f.read_exact(&mut byte)?;
        iters += 1;
        let (completed, new_saw) = lds_reset_poll_step(byte[0], saw_asserted);
        saw_asserted = new_saw;
        if completed {
            crate::dlog!(
                "[reset_board]   reset completed after {} polls (command byte={:#04x})",
                iters,
                byte[0]
            );
            break;
        }
        if std::time::Instant::now() >= deadline {
            crate::dlog!(
                "[reset_board]   timeout after {} polls (last byte={:#04x}, saw_asserted={})",
                iters,
                byte[0],
                saw_asserted
            );
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    crate::dlog!("[reset_board] step 3: open fd + RESTORE_STATE ioctl");
    {
        let fd = kmd::open_device(card)?;
        let r = kmd::reset_device(fd, kmd::TENSTORRENT_RESET_DEVICE_RESTORE_STATE);
        unsafe {
            libc::close(fd);
        }
        r?;
    }
    crate::dlog!("[reset_board] complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bdf_canonical_layout() {
        // tt-kmd's bus_dev_fn packing: bits 15..8 = bus, 7..3 = dev,
        // 2..0 = func. For 01:00.0 in domain 0000:
        // bus_dev_fn = (1 << 8) | (0 << 3) | 0 = 0x0100.
        assert_eq!(format_bdf(0, 0x0100), "0000:01:00.0");
    }

    #[test]
    fn format_bdf_widens_dev_and_func() {
        // dev=0x1f (max 5-bit value), func=0x7 (max 3-bit value),
        // bus=0xff (max byte): bus_dev_fn = 0xfffd | 0x07 = 0xffff.
        // Domain comfortably wider than 16-bit so the {:04x} format
        // still pads the standard sysfs case.
        assert_eq!(format_bdf(0xabcd, 0xffff), "abcd:ff:1f.7");
    }

    #[test]
    fn format_bdf_masks_out_extra_bits_in_bus_dev_fn() {
        // The kmd field is u16 in practice, but `format_bdf` accepts a
        // u32 to dodge the cast at the callsite. Make sure stray high
        // bits don't bleed through.
        assert_eq!(format_bdf(0, 0xdead_0100), "0000:01:00.0");
    }

    // ---- lds_reset_poll_step ----
    //
    // The poll loop needs to observe the reset bit go through the
    // 0 -> 1 -> 0 transition before returning success. These tests
    // pin the small state machine so a reordering that lets it exit
    // without ever seeing the assertion would fail loudly.

    #[test]
    fn poll_step_initial_zero_does_not_complete() {
        // Bit hasn't been asserted yet; we keep waiting.
        let (done, saw) = lds_reset_poll_step(0b0000_0000, false);
        assert!(!done);
        assert!(!saw);
    }

    #[test]
    fn poll_step_observes_assertion_and_keeps_waiting() {
        // Bit went up: record that we saw it, keep waiting for release.
        let (done, saw) = lds_reset_poll_step(0b0000_0010, false);
        assert!(!done);
        assert!(saw);
    }

    #[test]
    fn poll_step_completes_on_release_after_seeing_assertion() {
        // Standard happy path: previous iter saw bit=1, this iter
        // sees bit=0. Reset completed.
        let (done, saw) = lds_reset_poll_step(0b0000_0000, true);
        assert!(done);
        assert!(saw);
    }

    #[test]
    fn poll_step_does_not_complete_on_zero_without_prior_assertion() {
        // Bit is 0 but we never saw it asserted. This is the broken
        // path: returning early here would mean the chip never
        // entered reset. Guard against an "if reset_bit == 0 break"
        // reorder regression.
        let (done, saw) = lds_reset_poll_step(0b0000_0000, false);
        assert!(!done);
        assert!(!saw);
    }

    #[test]
    fn poll_step_keeps_saw_asserted_sticky_under_assertion() {
        // While bit is still 1, we haven't completed yet but
        // saw_asserted remains true.
        let (done, saw) = lds_reset_poll_step(0b0000_0010, true);
        assert!(!done);
        assert!(saw);
    }

    #[test]
    fn poll_step_ignores_unrelated_bits_in_command_byte() {
        // Bit 0 (IO Space Enable) and bits 2..7 must not perturb the
        // poll's decision — only bit 1 matters.
        let (done_a, _) = lds_reset_poll_step(0b1111_1101, true);
        assert!(done_a, "bit 1 = 0 plus saw_asserted -> done");
        let (done_b, _) = lds_reset_poll_step(0b1111_1111, false);
        assert!(!done_b, "all other bits set, bit 1 also set -> waiting");
    }
}
