// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Daemon-owned Tensix tile that hosts the M3 (#69) virtio-mmio
//! engine firmware and serves all L2CPUs on the chip.
//!
//! Lifecycle (when the `virtio-engine` feature is enabled):
//!
//!   1. At daemon startup (or on first boot, depending on integration)
//!      pick a tile via `tensix_tile::pick_virtio_engine_tile`,
//!      assert all baby-RISC soft resets, load the M3 firmware
//!      (`virtio_engine::VIRTIO_FIRMWARE`), and release BRISC.
//!   2. For each L2CPU boot, program one small TLB on the L2CPU
//!      pointing at this tile's L1 sub-window for that L2CPU
//!      (`virtio_engine::l2cpu_window_base(idx)`). Hand the resulting
//!      x280 PA to the DTB builder so the guest's virtio-mmio reg
//!      ranges target our reg files.
//!
//! One Tensix tile serves all four L2CPUs on the chip (per the #66
//! design discussion). BRISC's poll loop sweeps all 16 reg files
//! (4 L2CPUs × 4 devices) in well under 1 µs — orders of magnitude
//! faster than the L2CPU's NoC RTT, which closes the QUEUE_READY
//! race that motivated this whole architecture.

use std::io;

use crate::shared_chip::SharedChip;
use crate::telemetry;
use crate::tensix::TensixTile;
use crate::tensix_tile;
use crate::virtio_engine as ve;

/// Reservation of one Tensix tile for the virtio-mmio engine.
///
/// Holds the open `TensixTile` (so its TLBs stay alive for the
/// daemon's lifetime), the picker output coords, and the cached
/// translated coords needed for L2CPU TLB programming.
pub struct TensixEngine {
    tile: TensixTile,
    /// NOC0-logical (x, y) — what the M2 picker returned. Used in
    /// `daemon status` and for diagnostics.
    pub noc0_x: u16,
    pub noc0_y: u16,
    /// Translated (x, y) — what the L2CPU's small TLB hardware
    /// expects (per `tensix_tile::noc0_to_translated_tensix`).
    pub translated_x: u16,
    pub translated_y: u16,
}

// Safety: same story as `SharedChip`. The contained `TensixTile`
// owns TLB windows backed by PCI BAR MMIO; volatile aligned u32
// reads/writes through those mappings have hardware-level atomicity.
// We promote to `Send + Sync` so the engine can live in an
// `Arc<TensixEngine>` shared across the daemon's worker threads
// (boot path, status, future M5 daemon-side bridge). All write paths
// touch the chip from a single thread at a time (held under the
// daemon's `tensix_engine` mutex during bring-up; read-only after);
// concurrent readers see hardware-coherent values via the BAR.
unsafe impl Send for TensixEngine {}
unsafe impl Sync for TensixEngine {}

impl TensixEngine {
    /// Bring up a Tensix tile to host the M3 virtio firmware. Picks
    /// a tile via M2, asserts all soft resets, loads the firmware,
    /// pre-clears the reg-file region, and releases BRISC. After
    /// this returns, the on-tile firmware is running and is ready
    /// to serve guest MMIO.
    pub fn bring_up(card: u32, chip: &SharedChip) -> io::Result<Self> {
        let telem = telemetry::read_telemetry(chip)
            .map_err(|e| io::Error::other(format!("read telemetry: {}", e)))?;
        let picked = tensix_tile::pick_virtio_engine_tile(&telem)
            .map_err(|e| io::Error::other(format!("pick tile: {}", e)))?;

        // Translate picker output to the L2CPU TLB's coord flavor.
        // For the canonical Blackhole layout (harvest at the tail of
        // the col list), this is identity; the helper handles other
        // chips correctly. Failing to translate at this point means
        // the picker handed us a coord the L2CPU TLB can't reach,
        // which is a hard error — fail bring-up rather than ship a
        // silently-broken engine.
        let (translated_x, translated_y) = tensix_tile::noc0_to_translated_tensix(
            picked.x,
            picked.y,
            telem.enabled_tensix_col,
            telem.noc_translation_enabled,
        )
        .ok_or_else(|| {
            io::Error::other(format!(
                "tile ({}, {}) has no translated form for L2CPU TLB \
                 (enabled_tensix_col={:#x}, noc_translation={})",
                picked.x, picked.y, telem.enabled_tensix_col, telem.noc_translation_enabled
            ))
        })?;

        let tile = TensixTile::new(card, picked.x, picked.y).map_err(|e| {
            io::Error::other(format!(
                "open tensix tile ({}, {}) on card {}: {}",
                picked.x, picked.y, card, e
            ))
        })?;

        // Halt all baby RISCs, pre-clear the reg-file region so the
        // firmware's writes are unambiguous, load firmware, release
        // BRISC. Same pattern as `debug tensix-virtio` (M3 smoke).
        tile.assert_all_resets();
        for slot in 0..ve::NUM_SLOTS {
            let base = ve::slot_regs_base(slot);
            for off in (0..ve::REGS_PER_DEV).step_by(4) {
                tile.write_l1_u32(base + off, 0);
            }
        }
        tile.load_brisc_firmware(ve::VIRTIO_FIRMWARE);
        tile.release_brisc_only();

        // Sanity-poke the firmware: read the magic out of the stats
        // page. If BRISC didn't run (reset stuck, firmware corrupt,
        // etc.) we want a clear error here rather than a confused
        // guest later.
        let stats_magic = tile.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_MAGIC);
        if stats_magic != ve::STATS_MAGIC_LOADED {
            return Err(io::Error::other(format!(
                "BRISC firmware on tile ({}, {}) did not initialize \
                 stats magic (got {:#010x}, expected {:#010x})",
                picked.x,
                picked.y,
                stats_magic,
                ve::STATS_MAGIC_LOADED
            )));
        }

        eprintln!(
            "[tensix-engine] up on card {} tile NOC0 ({}, {}), translated ({}, {}); \
             firmware version {:#010x}",
            card,
            picked.x,
            picked.y,
            translated_x,
            translated_y,
            tile.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_VERSION),
        );

        Ok(TensixEngine {
            tile,
            noc0_x: picked.x,
            noc0_y: picked.y,
            translated_x,
            translated_y,
        })
    }

    /// Read the firmware's reported version word. Useful for the
    /// daemon-side compatibility check in M5 (#71).
    pub fn firmware_version(&self) -> u32 {
        self.tile
            .read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_VERSION)
    }

    /// Read the cumulative QUEUE_NOTIFY event count. Diagnostic only.
    pub fn notify_event_count(&self) -> u32 {
        self.tile
            .read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_NOTIFY_EVENTS)
    }

    /// Program one small TLB on the given L2CPU pointing at this
    /// tile's reg-file slice for that L2CPU. Returns the L2CPU PA
    /// of the start of the per-device sub-window — identical shape
    /// to the host-buffer path's `x280_base`, only the destination
    /// differs.
    ///
    /// The TLB always uses the **uncached aperture** so the L2CPU's
    /// L3 doesn't shadow guest writes (per #66 fragility item 4).
    /// `program_small_tlb_unicast` already uses the UC aperture.
    pub fn program_l2cpu_tlb(&self, l2cpu: &crate::l2cpu::L2Cpu, l2cpu_idx: u32) -> u64 {
        let noc_addr = ve::l2cpu_window_base(l2cpu_idx) as u64;
        crate::x280_tlb::program_small_tlb_unicast(
            l2cpu,
            crate::x280_tlb::SHARED_TLB_SLOT,
            self.translated_x as u32,
            self.translated_y as u32,
            noc_addr,
        )
    }
}

#[cfg(test)]
mod tests {
    // Hardware-touching code lives here; the non-trivial logic
    // (translation, layout) is tested in `tensix_tile` and
    // `virtio_engine` already. This module is mostly glue.

    #[test]
    fn module_compiles_with_virtio_engine_feature_off() {
        // Smoke: the module is referenced from main.rs even when
        // the feature is off, so it has to compile cleanly. Nothing
        // to assert beyond that.
    }
}
