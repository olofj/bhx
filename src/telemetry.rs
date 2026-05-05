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
//! directly through tile (8,0) — the ARC tile + reset unit; we do the
//! same.
//!
//! Protocol:
//!   1. Read `SCRATCH_RAM[13]` at `0x80030434` → telem table base.
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

/// Address of `SCRATCH_RAM[13]` on tile (8,0). ARC firmware writes
/// the telemetry table base address here when it finishes booting.
/// Source: `tt-kmd/blackhole.c::ARC_TELEMETRY_PTR` and
/// `tt-metal` UMD `blackhole_implementation.hpp::SCRATCH_RAM_13`.
pub const ARC_TELEMETRY_PTR_ADDR: u64 = 0x8003_0434;

/// CSM bounds inside which the telemetry table base must fall.
/// Mirrors `tt-kmd/telemetry.h::ARC_CSM_BASE` / `ARC_CSM_SIZE`.
pub const ARC_CSM_BASE: u64 = 0x1000_0000;
pub const ARC_CSM_SIZE: u64 = 1 << 19; // 512 KiB

/// Telemetry tag IDs we care about. Full enum is in
/// `luwen/crates/luwen-api/src/chip/blackhole/telemetry_tags.rs`. We
/// decode the subset that's load-bearing for M2 boot logic (#68) plus
/// the operator-visibility set the Prometheus exporter surfaces.
pub mod tag {
    pub const BOARD_ID_HIGH: u16 = 1;
    pub const BOARD_ID_LOW: u16 = 2;
    pub const ASIC_ID: u16 = 3;
    pub const HARVESTING_STATE: u16 = 4;
    pub const VCORE: u16 = 6;
    pub const TDP: u16 = 7;
    pub const TDC: u16 = 8;
    pub const VDD_LIMITS: u16 = 9;
    pub const ASIC_TEMPERATURE: u16 = 11;
    pub const VREG_TEMPERATURE: u16 = 12;
    pub const BOARD_TEMPERATURE: u16 = 13;
    pub const AICLK: u16 = 14;
    pub const AXICLK: u16 = 15;
    pub const ARCCLK: u16 = 16;
    pub const L2CPUCLK0: u16 = 17;
    pub const L2CPUCLK1: u16 = 18;
    pub const L2CPUCLK2: u16 = 19;
    pub const L2CPUCLK3: u16 = 20;
    pub const ETH_LIVE_STATUS: u16 = 21;
    pub const DDR_STATUS: u16 = 22;
    pub const DDR_SPEED: u16 = 23;
    pub const FAN_SPEED: u16 = 31;
    pub const TIMER_HEARTBEAT: u16 = 32;
    /// Sentinel: total number of distinct telemetry tag IDs the ARC
    /// firmware can emit. Pinned for parity with the firmware header.
    #[allow(dead_code)]
    pub const TELEM_ENUM_COUNT: u16 = 33;
    pub const ENABLED_TENSIX_COL: u16 = 34;
    pub const ENABLED_ETH: u16 = 35;
    pub const ENABLED_GDDR: u16 = 36;
    pub const ENABLED_L2CPU: u16 = 37;
    pub const PCIE_USAGE: u16 = 38;
    pub const NOC_TRANSLATION: u16 = 40;
    pub const FAN_RPM: u16 = 41;
    pub const GDDR01_TEMP: u16 = 42;
    pub const GDDR23_TEMP: u16 = 43;
    pub const GDDR45_TEMP: u16 = 44;
    pub const GDDR67_TEMP: u16 = 45;
    pub const GDDR01_CORR_ERRS: u16 = 46;
    pub const GDDR23_CORR_ERRS: u16 = 47;
    pub const GDDR45_CORR_ERRS: u16 = 48;
    pub const GDDR67_CORR_ERRS: u16 = 49;
    pub const GDDR_UNCORR_ERRS: u16 = 50;
    pub const MAX_GDDR_TEMP: u16 = 51;
    pub const ASIC_LOCATION: u16 = 52;
    pub const BOARD_POWER_LIMIT: u16 = 53;
    pub const INPUT_POWER: u16 = 54;
    pub const TDC_LIMIT_MAX: u16 = 55;
    pub const THM_LIMIT_THROTTLE: u16 = 56;
    pub const THERM_TRIP_COUNT: u16 = 60;
    pub const AICLK_LIMIT_MAX: u16 = 63;
    pub const TDP_LIMIT_MAX: u16 = 64;
}

/// Decode an ARC `ASIC_TEMPERATURE` value into millicelsius.
///
/// The chip publishes the temperature as a 16.16 signed fixed-point
/// number (high half = integer °C, low half = fraction × 65536). Per
/// `tt-kmd/telemetry.c::asic_temp_to_milli_celsius`. Rounds to nearest
/// millicelsius, ties away from zero. Uses `/` (truncates toward
/// zero) rather than `>>` (floors toward -infinity), so an exact
/// integer negative like -10.0 °C lands at -10000 mC and not -10001.
pub fn fixed16_to_millicelsius(raw: u32) -> i32 {
    let signed = raw as i32;
    let scaled = (signed as i64) * 1000;
    let half = 1i64 << 15;
    let adjusted = if scaled >= 0 {
        scaled + half
    } else {
        scaled - half
    };
    (adjusted / (1i64 << 16)) as i32
}

/// Decode the packed `VDD_LIMITS` tag (low u16 = min mV, high u16 = max mV).
pub fn vdd_limits(raw: u32) -> (u16, u16) {
    ((raw & 0xFFFF) as u16, ((raw >> 16) & 0xFFFF) as u16)
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
    // ---- Operator-visible health ----
    /// ASIC temperature in millicelsius (decoded from the chip's
    /// 16.16 fixed-point Celsius word). 0 if the firmware didn't
    /// publish the tag yet.
    pub asic_temperature_mc: i32,
    pub vreg_temperature_mc: i32,
    pub board_temperature_mc: i32,
    pub max_gddr_temperature_c: u32,
    /// Per-channel-pair GDDR temps (°C, integer). Indexes 0..4 map to
    /// channel pairs 01, 23, 45, 67.
    pub gddr_temperature_c: [u32; 4],
    /// Per-channel-pair correctable ECC error counts (monotonic).
    pub gddr_corr_errs: [u32; 4],
    /// Total uncorrectable ECC error count across all channels.
    pub gddr_uncorr_errs: u32,
    /// PLLs in MHz. 0 if the firmware didn't publish the tag.
    pub aiclk_mhz: u32,
    pub axiclk_mhz: u32,
    pub arcclk_mhz: u32,
    pub l2cpuclk_mhz: [u32; 4],
    pub aiclk_limit_max_mhz: u32,
    /// Power / current / voltage. Units in field names.
    pub vcore_mv: u32,
    pub tdp_w: u32,
    pub tdc_a: u32,
    pub input_power_w: u32,
    pub vdd_min_mv: u16,
    pub vdd_max_mv: u16,
    pub board_power_limit_w: u32,
    pub tdp_limit_max_w: u32,
    pub tdc_limit_max_a: u32,
    pub thm_limit_throttle_c: u32,
    /// Fan / cooling.
    pub fan_speed_pct: u32,
    pub fan_rpm: u32,
    /// DRAM controller state. `ddr_status` is a bitfield (per-channel
    /// trained / not-trained); `ddr_speed` is the trained speed grade
    /// in MT/s.
    pub ddr_status: u32,
    pub ddr_speed_mts: u32,
    /// Health counters. Monotonic on the chip; mirror as Prometheus
    /// counters. Reset on tt-smi -r (chip-side reset).
    pub timer_heartbeat: u32,
    pub therm_trip_count: u32,
    /// PCIe tile in use (chip has two; only one is active at a time).
    pub pcie_usage: u32,
    /// Ethernet liveness bitfield (per-tile).
    pub eth_live_status: u32,
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
    #[error(transparent)]
    SharedChip(#[from] crate::Error),
}

/// Read the ARC telemetry table from the chip via `SharedChip`.
///
/// Errors with `ArcNotReady` if the scratch register is zero (firmware
/// hasn't published yet) or `OutOfCsm` if the address looks corrupt
/// (defensive — matches luwen's check). The table is small (typically
/// 60-ish entries × 8 bytes header + tags + data); the whole walk
/// completes in one pass with no allocations beyond the result `Vec`.
pub fn read_telemetry(chip: &SharedChip) -> Result<Telemetry, TelemetryError> {
    let table_addr = chip.arc_read32(ARC_TELEMETRY_PTR_ADDR)?;
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

    let version = chip.csm_read32(table_addr)?;
    let major = (version >> 16) & 0xFF;
    if major > 1 {
        return Err(TelemetryError::UnsupportedVersion(version));
    }
    let entry_count = chip.csm_read32(table_addr + 4)?;

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
        let entry = chip.csm_read32(tags_base + (i as u64) * 4)?;
        let tag = (entry & 0xFFFF) as u16;
        let offset = ((entry >> 16) & 0xFFFF) as u16;
        let data = chip.csm_read32(data_base + (offset as u64) * 4)?;
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
            // Power / thermal / electrical
            tag::VCORE => t.vcore_mv = e.data,
            tag::TDP => t.tdp_w = e.data,
            tag::TDC => t.tdc_a = e.data,
            tag::VDD_LIMITS => {
                let (min, max) = vdd_limits(e.data);
                t.vdd_min_mv = min;
                t.vdd_max_mv = max;
            }
            tag::THM_LIMIT_THROTTLE => t.thm_limit_throttle_c = e.data,
            tag::ASIC_TEMPERATURE => t.asic_temperature_mc = fixed16_to_millicelsius(e.data),
            tag::VREG_TEMPERATURE => t.vreg_temperature_mc = fixed16_to_millicelsius(e.data),
            tag::BOARD_TEMPERATURE => t.board_temperature_mc = fixed16_to_millicelsius(e.data),
            tag::MAX_GDDR_TEMP => t.max_gddr_temperature_c = e.data,
            tag::GDDR01_TEMP => t.gddr_temperature_c[0] = e.data,
            tag::GDDR23_TEMP => t.gddr_temperature_c[1] = e.data,
            tag::GDDR45_TEMP => t.gddr_temperature_c[2] = e.data,
            tag::GDDR67_TEMP => t.gddr_temperature_c[3] = e.data,
            tag::GDDR01_CORR_ERRS => t.gddr_corr_errs[0] = e.data,
            tag::GDDR23_CORR_ERRS => t.gddr_corr_errs[1] = e.data,
            tag::GDDR45_CORR_ERRS => t.gddr_corr_errs[2] = e.data,
            tag::GDDR67_CORR_ERRS => t.gddr_corr_errs[3] = e.data,
            tag::GDDR_UNCORR_ERRS => t.gddr_uncorr_errs = e.data,
            tag::INPUT_POWER => t.input_power_w = e.data,
            tag::BOARD_POWER_LIMIT => t.board_power_limit_w = e.data,
            tag::TDP_LIMIT_MAX => t.tdp_limit_max_w = e.data,
            tag::TDC_LIMIT_MAX => t.tdc_limit_max_a = e.data,
            tag::FAN_SPEED => t.fan_speed_pct = e.data,
            tag::FAN_RPM => t.fan_rpm = e.data,
            // Clock state
            tag::AICLK => t.aiclk_mhz = e.data,
            tag::AXICLK => t.axiclk_mhz = e.data,
            tag::ARCCLK => t.arcclk_mhz = e.data,
            tag::L2CPUCLK0 => t.l2cpuclk_mhz[0] = e.data,
            tag::L2CPUCLK1 => t.l2cpuclk_mhz[1] = e.data,
            tag::L2CPUCLK2 => t.l2cpuclk_mhz[2] = e.data,
            tag::L2CPUCLK3 => t.l2cpuclk_mhz[3] = e.data,
            tag::AICLK_LIMIT_MAX => t.aiclk_limit_max_mhz = e.data,
            // DRAM
            tag::DDR_STATUS => t.ddr_status = e.data,
            tag::DDR_SPEED => t.ddr_speed_mts = e.data,
            // Health
            tag::TIMER_HEARTBEAT => t.timer_heartbeat = e.data,
            tag::THERM_TRIP_COUNT => t.therm_trip_count = e.data,
            // Misc
            tag::PCIE_USAGE => t.pcie_usage = e.data,
            tag::ETH_LIVE_STATUS => t.eth_live_status = e.data,
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
    fn fixed16_decodes_room_temperature() {
        // 25.0 °C in 16.16 fixed-point = 25 << 16 = 0x0019_0000.
        assert_eq!(fixed16_to_millicelsius(0x0019_0000), 25_000);
    }

    #[test]
    fn fixed16_rounds_to_nearest_millicelsius() {
        // 49.99 °C ≈ 49 + 0.99 in 16.16 = (49 << 16) | (0.99 * 65536 ≈ 0xFD70).
        // 0.99 * 65536 = 64880.64 → 0xFD70 (= 64880).
        let raw = (49u32 << 16) | 0xFD70;
        let mc = fixed16_to_millicelsius(raw);
        // Expected: 49 * 1000 + 0.99 * 1000 = 49990 (allow ±1 rounding).
        assert!(
            (49989..=49991).contains(&mc),
            "expected ~49990 mC, got {}",
            mc
        );
    }

    #[test]
    fn fixed16_handles_negative_temperatures() {
        // -10.0 °C: 2's-complement of (10 << 16). At i64-extension it
        // should round symmetrically toward zero.
        let raw = (!(10u32 << 16)).wrapping_add(1);
        let mc = fixed16_to_millicelsius(raw);
        assert_eq!(mc, -10_000);
    }

    #[test]
    fn vdd_limits_unpacks_low_high_halves() {
        let raw = (0x1234u32) | (0x4321u32 << 16);
        let (min, max) = vdd_limits(raw);
        assert_eq!(min, 0x1234);
        assert_eq!(max, 0x4321);
    }

    #[test]
    fn decode_picks_up_chip_health_tags() {
        let entries = vec![
            entry(tag::ASIC_TEMPERATURE, 0x0019_0000), // 25 °C
            entry(tag::AICLK, 1350),
            entry(tag::TDP, 75),
            entry(tag::VCORE, 950),
            entry(tag::FAN_RPM, 4200),
            entry(tag::TIMER_HEARTBEAT, 12345),
            entry(tag::THERM_TRIP_COUNT, 3),
            entry(tag::GDDR_UNCORR_ERRS, 0),
            entry(tag::GDDR01_CORR_ERRS, 7),
        ];
        let t = decode_entries(0x0001_0000, entries.len() as u32, &entries);
        assert_eq!(t.asic_temperature_mc, 25_000);
        assert_eq!(t.aiclk_mhz, 1350);
        assert_eq!(t.tdp_w, 75);
        assert_eq!(t.vcore_mv, 950);
        assert_eq!(t.fan_rpm, 4200);
        assert_eq!(t.timer_heartbeat, 12345);
        assert_eq!(t.therm_trip_count, 3);
        assert_eq!(t.gddr_corr_errs[0], 7);
        assert_eq!(t.gddr_uncorr_errs, 0);
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
