// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! ARC firmware telemetry table reader.
//!
//! Blackhole's ARC firmware publishes a tag-keyed table in CSM RAM.
//! The "soft harvest" mask (which Tensix columns the chip is allowed
//! to expose) lives in this table, populated from board flash at ARC
//! boot. tt-kmd has the address cache internally but does not expose
//! harvest data via ioctl/sysfs (see #75 for the full discovery story).
//! `luwen`, `tt-smi` (via pyluwen), and `tt-metal` UMD all read it
//! directly through AXI on tile (8,0); we do the same.
//!
//! Protocol:
//!   1. Read `SCRATCH_RAM[13]` at AXI `0x80030434` → telem table base.
//!   2. Header: `[base+0..3]` = version, `[base+4..7]` = entry count.
//!   3. Tag table at `[base+8 .. base+8+count*4]`. Each entry is one
//!      u32: low 16 bits = tag id, high 16 bits = data offset (in u32
//!      units) into the data block.
//!   4. Data block at `[base+8+count*4 .. base+8+2*count*4]`. Read the
//!      u32 at the offset from step 3.
//!
//! All reads land on tile (8,0) — `SharedChip` already owns that, and
//! its CSM window covers the table addresses.

use std::io;

use crate::shared_chip::SharedChip;

/// AXI address of `SCRATCH_RAM[13]` on tile (8,0). ARC firmware writes
/// the telemetry table base address here when it finishes booting.
/// Source: `tt-kmd/blackhole.c::ARC_TELEMETRY_PTR` and
/// `tt-metal` UMD `blackhole_implementation.hpp::SCRATCH_RAM_13`.
pub const ARC_TELEMETRY_PTR_ADDR: u64 = 0x8003_0434;

/// CSM bounds inside which the telemetry table base must fall.
/// Mirrors `tt-kmd/telemetry.h::ARC_CSM_BASE` / `ARC_CSM_SIZE`.
pub const ARC_CSM_BASE: u64 = 0x1000_0000;
pub const ARC_CSM_SIZE: u64 = 1 << 19; // 512 KiB

/// Telemetry tag IDs we care about. Full enum is in
/// `luwen/crates/luwen-api/src/chip/blackhole/telemetry_tags.rs`; we
/// only decode what M2 needs (#68) plus a few useful diagnostics.
pub mod tag {
    pub const BOARD_ID_HIGH: u16 = 1;
    pub const BOARD_ID_LOW: u16 = 2;
    pub const ASIC_ID: u16 = 3;
    pub const HARVESTING_STATE: u16 = 4;
    pub const TELEM_ENUM_COUNT: u16 = 33;
    pub const ENABLED_TENSIX_COL: u16 = 34;
    pub const ENABLED_ETH: u16 = 35;
    pub const ENABLED_GDDR: u16 = 36;
    pub const ENABLED_L2CPU: u16 = 37;
    pub const NOC_TRANSLATION: u16 = 40;
    pub const ASIC_LOCATION: u16 = 52;
}

/// Decoded telemetry of interest. The raw `entries` map is preserved
/// so `debug telemetry-dump` can show every tag the firmware reported,
/// not just the few fields M2 cares about.
#[derive(Debug, Clone, Default)]
pub struct Telemetry {
    pub version: u32,
    pub entry_count: u32,
    /// Bitmask: bit `i` = 1 ⟺ Tensix row `y = 2 + i` is harvested.
    /// Source of truth lives in ARC; we only read it. On boards with
    /// no row harvest this is `0`.
    pub harvesting_state: u32,
    /// Bitmask: bit `i` set ⟺ position `i` of the column-enable
    /// vector is alive. Decoder logic depends on `noc_translation`
    /// — see `crate::tensix_tile`.
    pub enabled_tensix_col: u32,
    /// Non-zero when the chip's NoC has coordinate translation
    /// enabled. Affects how `enabled_tensix_col` maps to NOC0
    /// logical column numbers.
    pub noc_translation_enabled: bool,
    /// Other useful identification + status bits.
    pub board_id: u64,
    pub asic_id: u32,
    pub asic_location: u32,
    pub enabled_eth: u32,
    pub enabled_gddr: u32,
    pub enabled_l2cpu: u32,
    /// Every tag the firmware advertised, with its raw value. Useful
    /// for diagnostics and for the `debug telemetry-dump` subcommand.
    pub entries: Vec<TelemetryEntry>,
}

#[derive(Debug, Clone, Copy)]
pub struct TelemetryEntry {
    pub tag: u16,
    pub offset: u16,
    pub data: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    #[error("ARC firmware not ready (telemetry pointer is 0)")]
    ArcNotReady,
    #[error("telemetry pointer 0x{0:08x} is outside CSM range [0x{1:08x}..0x{2:08x})")]
    OutOfCsm(u32, u64, u64),
    #[error("telemetry entry count {0} would overflow CSM")]
    EntryCountTooLarge(u32),
    #[error("unsupported telemetry version {0:#010x}")]
    UnsupportedVersion(u32),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Read the ARC telemetry table from the chip via `SharedChip`.
///
/// Errors with `ArcNotReady` if the scratch register is zero (firmware
/// hasn't published yet) or `OutOfCsm` if the address looks corrupt
/// (defensive — matches luwen's check). The table is small (typically
/// 60-ish entries × 8 bytes header + tags + data); the whole walk
/// completes in one pass with no allocations beyond the result `Vec`.
pub fn read_telemetry(chip: &SharedChip) -> Result<Telemetry, TelemetryError> {
    let table_addr = chip.axi_read32(ARC_TELEMETRY_PTR_ADDR);
    if table_addr == 0 {
        return Err(TelemetryError::ArcNotReady);
    }
    let table_addr = table_addr as u64;
    if !(ARC_CSM_BASE..ARC_CSM_BASE + ARC_CSM_SIZE).contains(&table_addr) {
        return Err(TelemetryError::OutOfCsm(
            table_addr as u32,
            ARC_CSM_BASE,
            ARC_CSM_BASE + ARC_CSM_SIZE,
        ));
    }

    let version = chip.csm_read32(table_addr);
    let major = (version >> 16) & 0xFF;
    if major > 1 {
        return Err(TelemetryError::UnsupportedVersion(version));
    }
    let entry_count = chip.csm_read32(table_addr + 4);

    // Bound the entry count so a corrupt header can't make us read
    // past CSM. Each entry is 4 bytes for the tag table + 4 bytes
    // for the data block = 8 bytes/entry, plus the 8-byte header.
    if entry_count == 0
        || (entry_count as u64).saturating_mul(8) + 8
            > (ARC_CSM_BASE + ARC_CSM_SIZE).saturating_sub(table_addr)
    {
        return Err(TelemetryError::EntryCountTooLarge(entry_count));
    }

    let tags_base = table_addr + 8;
    let data_base = table_addr + 8 + (entry_count as u64) * 4;

    let mut entries = Vec::with_capacity(entry_count as usize);
    for i in 0..entry_count {
        let entry = chip.csm_read32(tags_base + (i as u64) * 4);
        let tag = (entry & 0xFFFF) as u16;
        let offset = ((entry >> 16) & 0xFFFF) as u16;
        let data = chip.csm_read32(data_base + (offset as u64) * 4);
        entries.push(TelemetryEntry { tag, offset, data });
    }

    Ok(decode_entries(version, entry_count, &entries))
}

/// Pure-function decoder used by `read_telemetry` and unit tests.
/// Takes the raw header + entry vector and folds the tags we know
/// about into the typed `Telemetry` struct.
pub fn decode_entries(version: u32, entry_count: u32, entries: &[TelemetryEntry]) -> Telemetry {
    let mut t = Telemetry {
        version,
        entry_count,
        entries: entries.to_vec(),
        ..Telemetry::default()
    };
    let mut board_id_high: u32 = 0;
    let mut board_id_low: u32 = 0;
    for e in entries {
        match e.tag {
            tag::BOARD_ID_HIGH => board_id_high = e.data,
            tag::BOARD_ID_LOW => board_id_low = e.data,
            tag::ASIC_ID => t.asic_id = e.data,
            tag::HARVESTING_STATE => t.harvesting_state = e.data,
            tag::ENABLED_TENSIX_COL => t.enabled_tensix_col = e.data,
            tag::ENABLED_ETH => t.enabled_eth = e.data,
            tag::ENABLED_GDDR => t.enabled_gddr = e.data,
            tag::ENABLED_L2CPU => t.enabled_l2cpu = e.data,
            tag::NOC_TRANSLATION => t.noc_translation_enabled = e.data != 0,
            tag::ASIC_LOCATION => t.asic_location = e.data,
            _ => {}
        }
    }
    t.board_id = ((board_id_high as u64) << 32) | board_id_low as u64;
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tag: u16, data: u32) -> TelemetryEntry {
        TelemetryEntry {
            tag,
            offset: 0,
            data,
        }
    }

    #[test]
    fn decode_extracts_known_tags() {
        let entries = vec![
            entry(tag::HARVESTING_STATE, 0x0000_0000),
            entry(tag::ENABLED_TENSIX_COL, 0x0000_0FFF),
            entry(tag::NOC_TRANSLATION, 1),
            entry(tag::BOARD_ID_HIGH, 0xDEAD_BEEF),
            entry(tag::BOARD_ID_LOW, 0x1234_5678),
            entry(tag::ASIC_ID, 42),
        ];
        let t = decode_entries(0x0001_0000, entries.len() as u32, &entries);
        assert_eq!(t.harvesting_state, 0);
        assert_eq!(t.enabled_tensix_col, 0xFFF);
        assert!(t.noc_translation_enabled);
        assert_eq!(t.board_id, 0xDEAD_BEEF_1234_5678);
        assert_eq!(t.asic_id, 42);
        assert_eq!(t.entries.len(), 6);
    }

    #[test]
    fn decode_handles_missing_tags() {
        // Entry list with only HARVESTING_STATE — every other field
        // must end up at default (0 / false).
        let entries = vec![entry(tag::HARVESTING_STATE, 0b101)];
        let t = decode_entries(0x0001_0000, 1, &entries);
        assert_eq!(t.harvesting_state, 0b101);
        assert_eq!(t.enabled_tensix_col, 0);
        assert!(!t.noc_translation_enabled);
        assert_eq!(t.board_id, 0);
    }

    #[test]
    fn decode_ignores_unknown_tags() {
        let entries = vec![
            entry(0xBEEF, 0xDEAD), // unknown
            entry(tag::HARVESTING_STATE, 0x42),
        ];
        let t = decode_entries(0x0001_0000, 2, &entries);
        assert_eq!(t.harvesting_state, 0x42);
        // The raw entries vector preserves the unknown tag for the
        // diagnostic dump path.
        assert_eq!(t.entries.len(), 2);
        assert_eq!(t.entries[0].tag, 0xBEEF);
    }
}
