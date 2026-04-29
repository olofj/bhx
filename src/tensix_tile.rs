// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Tensix tile selection — pick the tile we'll reserve for the M3+
//! virtio-mmio engine (issue #68).
//!
//! The picker reads ARC firmware's harvest mask (`crate::telemetry`),
//! decodes which Tensix coordinates are alive, and returns a single
//! deterministic `(x, y)` per chip. The deterministic part matters:
//! a daemon restart on the same chip must pick the same tile so
//! operator scripts and soaks don't get surprised.
//!
//! Why we don't just hardcode a corner: chips ship with various
//! patterns of harvested rows + soft-harvested columns. The luwen
//! algorithm (in `tests/read_write_test.rs:520-565` — see #75 for
//! provenance) encodes the only correct decoder we found.

use crate::telemetry::Telemetry;

/// NOC0-logical Tensix column numbers, in firmware-table order. The
/// chip's enable bitmask (`enabled_tensix_col`) indexes into this in
/// the non-translated case, and the count-of-ones determines the
/// prefix in the translated case.
pub const TENSIX_COLS_NOC0: [u16; 14] = [1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 13, 14, 15, 16];

/// NOC0-logical Tensix row numbers (always the same on Blackhole; the
/// `harvesting_state` bitmask removes some of them).
pub const TENSIX_ROWS_NOC0: [u16; 10] = [2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

/// Decode `enabled_tensix_col` + `noc_translation_enabled` into the
/// list of working NOC0-logical column numbers.
///
/// **Algorithm divergence from luwen, validated on real hardware.**
///
/// In the translated case, the firmware exposes the first
/// `N = enabled_mask.count_ones()` *translated positions* of the 14
/// NOC0 logical Tensix columns. NOC0 col `x` maps to translated
/// position `idx(x) = x - 1` for `x ≤ 7` and `idx(x) = x - 3` for
/// `x ≥ 10` (the router gap between cols 7 and 10 collapses to a
/// single skip in the translated numbering). A column is alive iff
/// `idx(x) < N`.
///
/// `luwen/tests/read_write_test.rs:531-555` writes the second
/// branch as `(x - 2) < working`, which is off by one — it would
/// drop NOC0 col 14 on a chip with 12 cols enabled, but hardware
/// observation on a p100a (board id `0x0000043231911060`,
/// EnabledTensixCol=0x0FFF, NocTranslation=1) shows NOC0 col 14 IS
/// reachable. We use the corrected formula (`x - 3 < working`,
/// equivalent to `x ≤ working + 2`).
///
/// In the non-translated case, bit `i` of the mask names column
/// `TENSIX_COLS_NOC0[i]` directly.
pub fn working_tensix_cols(enabled_mask: u32, noc_translation_enabled: bool) -> Vec<u16> {
    if noc_translation_enabled {
        let working = enabled_mask.count_ones();
        TENSIX_COLS_NOC0
            .iter()
            .copied()
            .filter(|&x| {
                let idx = if x <= 7 { x as u32 - 1 } else { x as u32 - 3 };
                idx < working
            })
            .collect()
    } else {
        TENSIX_COLS_NOC0
            .iter()
            .enumerate()
            .filter_map(|(i, &x)| {
                if enabled_mask & (1u32 << i) != 0 {
                    Some(x)
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Decode `harvesting_state` into the list of working NOC0-logical
/// row numbers. Bit `i` set ⟺ row `TENSIX_ROWS_NOC0[i]` harvested.
/// (Convention from tt-metal docs; we have not seen a chip with row
/// harvest yet, so this is a best-effort encoding.)
pub fn working_tensix_rows(harvesting_state: u32) -> Vec<u16> {
    TENSIX_ROWS_NOC0
        .iter()
        .enumerate()
        .filter_map(|(i, &y)| {
            if harvesting_state & (1u32 << i) == 0 {
                Some(y)
            } else {
                None
            }
        })
        .collect()
}

/// Rows that ARE harvested — we prefer to put our virtio engine here
/// so we don't compete with tt-metal compute (which avoids harvested
/// rows because of the defective coprocessor). The BabyRISCs and L1
/// in harvested rows are still functional per the Blackhole ISA docs.
pub fn harvested_tensix_rows(harvesting_state: u32) -> Vec<u16> {
    TENSIX_ROWS_NOC0
        .iter()
        .enumerate()
        .filter_map(|(i, &y)| {
            if harvesting_state & (1u32 << i) != 0 {
                Some(y)
            } else {
                None
            }
        })
        .collect()
}

/// Translate a NOC0-logical Tensix coord into the "translated" coord
/// flavor that the L2CPU's small TLB hardware expects. Mirrors the
/// `BlackholeCoordinateManager::fill_tensix_noc0_translated_mapping`
/// algorithm from `tt-metal/third_party/umd`:
///
///   * Walk `TENSIX_COLS_NOC0` and assign each non-harvested NOC0 col
///     a sequential `logical_x` = 0, 1, 2, … (index among non-harvested
///     cols).
///   * The translated coord of NOC0 col `c` (at array-position `p`,
///     non-harvested) on row `r` is `(TENSIX_COLS_NOC0[logical_x_of_p],
///     r)` — i.e., same row, with the x reshuffled into the
///     contiguous-non-harvested numbering.
///
/// Returns `None` if the NOC0 coord is harvested or out of grid.
///
/// On a chip with no col harvest before `noc0_x`, this is identity —
/// our p100a's lab card matches that case (cols 15+16 harvested,
/// nothing earlier), so for the picker's `(14, 11)` output the
/// translated coord is also `(14, 11)`. On a chip with harvest at,
/// say, NOC0 col 1, the same `(14, 11)` would translate to
/// `(13, 11)`.
///
/// This function exists for M4 (#70): the L2CPU's small TLB takes
/// translated coords (the existing `PCIE_TILE_X = 19, PCIE_TILE_Y = 24`
/// constants in `x280_tlb.rs` are the translated form of the NOC0
/// PCIE tile `(2, 0)`), so any code that programs an L2CPU TLB at a
/// Tensix tile must translate via this helper first.
pub fn noc0_to_translated_tensix(
    noc0_x: u16,
    noc0_y: u16,
    enabled_tensix_col_mask: u32,
    noc_translation_enabled: bool,
) -> Option<(u16, u16)> {
    if !noc_translation_enabled {
        // Without NoC translation, the chip routes raw NOC0 coords;
        // the L2CPU TLB also passes them through. Identity.
        return Some((noc0_x, noc0_y));
    }
    // Find the array-position of `noc0_x` in `TENSIX_COLS_NOC0`.
    let pos = TENSIX_COLS_NOC0.iter().position(|&v| v == noc0_x)?;
    // Per the M2 decoder's translated-mode rule (validated on
    // hardware in #68): a col `pos` is "alive" iff `pos <
    // enabled_count`. If the picker handed us a coord, it's alive
    // — but we still treat the harvest in the same way the C++ ref
    // does: each non-harvested col gets logical_x = its rank among
    // the non-harvested. The rank is just `pos` itself when the
    // first `enabled_count` cols are non-harvested (the canonical
    // Blackhole layout per luwen's algorithm). For chips that
    // depart from that layout — non-prefix harvest — we'd need to
    // walk the mask explicitly; we do that for safety here.
    let enabled_count = enabled_tensix_col_mask.count_ones() as usize;
    let mut logical_x: usize = 0;
    for (i, _) in TENSIX_COLS_NOC0.iter().enumerate() {
        if i == pos {
            if logical_x >= enabled_count {
                // The position is past the enabled range, i.e.
                // harvested.
                return None;
            }
            return Some((TENSIX_COLS_NOC0[logical_x], noc0_y));
        }
        // Pre-position cols are considered non-harvested if they
        // fall within the first `enabled_count` array positions
        // (matches the M2 decoder's "first N positions are alive"
        // rule for translated mode — the Blackhole convention).
        if i < enabled_count {
            logical_x += 1;
        }
    }
    None
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PickError {
    #[error(
        "no working Tensix columns (enabled_tensix_col={mask:#010x}, noc_translation={trans})"
    )]
    NoWorkingCols { mask: u32, trans: bool },
    #[error("no working Tensix rows (harvesting_state={state:#010x})")]
    NoWorkingRows { state: u32 },
}

/// Picked tile coordinate plus the rationale (which decoder branch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickedTile {
    pub x: u16,
    pub y: u16,
    pub reason: PickReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickReason {
    /// Chip has at least one harvested row; we picked the rightmost
    /// working column at a harvested-row Y. tt-metal compute will
    /// avoid this row anyway, so we minimize collision.
    HarvestedRowCorner,
    /// No row harvest — we picked the (rightmost-working-col,
    /// last-row) corner. tt-metal *can* place compute here, so M8
    /// documents the operator-side reservation contract.
    BottomRightCorner,
}

/// Deterministic per-chip tile picker. Pure function over telemetry
/// data — testable without hardware.
///
/// Algorithm:
///   1. Compute the working col + row sets via the luwen decoder.
///   2. If any rows are harvested, pick `(max_working_col, first
///      harvested row)`. The harvested-row tile's BabyRISC + L1 are
///      still functional (the Tensix coprocessor is what's defective)
///      and tt-metal won't try to schedule compute there.
///   3. Otherwise pick `(max_working_col, max_working_row)` —
///      bottom-right corner of the working grid. tt-metal *might*
///      place compute here on a healthy chip; M8 covers the
///      operator-facing reservation.
///
/// Determinism: the working sets are sorted by NOC0 column/row order
/// (that's how `TENSIX_*_NOC0` is laid out), so the same chip always
/// produces the same answer.
pub fn pick_virtio_engine_tile(telem: &Telemetry) -> Result<PickedTile, PickError> {
    let cols = working_tensix_cols(telem.enabled_tensix_col, telem.noc_translation_enabled);
    if cols.is_empty() {
        return Err(PickError::NoWorkingCols {
            mask: telem.enabled_tensix_col,
            trans: telem.noc_translation_enabled,
        });
    }
    let rows = working_tensix_rows(telem.harvesting_state);
    if rows.is_empty() {
        return Err(PickError::NoWorkingRows {
            state: telem.harvesting_state,
        });
    }
    let x = *cols.last().unwrap();
    let harvested = harvested_tensix_rows(telem.harvesting_state);
    if let Some(&y) = harvested.first() {
        Ok(PickedTile {
            x,
            y,
            reason: PickReason::HarvestedRowCorner,
        })
    } else {
        let y = *rows.last().unwrap();
        Ok(PickedTile {
            x,
            y,
            reason: PickReason::BottomRightCorner,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{Telemetry, TelemetryEntry};

    fn telem(harvest: u32, col_mask: u32, translation: bool) -> Telemetry {
        Telemetry {
            version: 0x10000,
            entry_count: 0,
            harvesting_state: harvest,
            enabled_tensix_col: col_mask,
            noc_translation_enabled: translation,
            entries: Vec::<TelemetryEntry>::new(),
            ..Telemetry::default()
        }
    }

    #[test]
    fn translated_14_bits_set_keeps_full_grid() {
        // 14 bits set + translation: every NOC0 col is at translated
        // idx < 14, so all 14 cols are reachable.
        let cols = working_tensix_cols(0x3FFF, true);
        assert_eq!(cols, TENSIX_COLS_NOC0.to_vec());
    }

    #[test]
    fn translated_12_cols_drops_last_two() {
        // 0xFFF = 12 cols enabled, translated.
        // NOC0 cols 1..7 → translated idx 0..6; NOC0 cols 10..16 →
        // translated idx 7..13. With working=12, idx<12 keeps NOC0
        // cols up to translated idx 11 = NOC0 col 14. So 15, 16 drop.
        // Validated on p100a: x=14 reachable, x=15/16 not.
        let cols = working_tensix_cols(0xFFF, true);
        assert_eq!(cols, vec![1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 13, 14]);
    }

    #[test]
    fn untranslated_mask_indexes_directly() {
        // 0xFFF = bits 0..11 set. Indexes into TENSIX_COLS_NOC0 give
        // [1,2,3,4,5,6,7,10,11,12,13,14] — i.e. drop the LAST two
        // (15, 16), which is different from the translated case.
        let cols = working_tensix_cols(0xFFF, false);
        assert_eq!(cols, vec![1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 13, 14]);
    }

    #[test]
    fn untranslated_sparse_mask() {
        // Bits 0, 7, 13 set ⟹ cols [1, 10, 16].
        let mask = (1 << 0) | (1 << 7) | (1 << 13);
        let cols = working_tensix_cols(mask, false);
        assert_eq!(cols, vec![1, 10, 16]);
    }

    #[test]
    fn harvested_rows_are_complement_of_working_rows() {
        // Bits 0 and 9 of harvesting_state ⟹ rows 2 and 11 are
        // harvested.
        let mask = (1 << 0) | (1 << 9);
        let working = working_tensix_rows(mask);
        let harvested = harvested_tensix_rows(mask);
        assert_eq!(harvested, vec![2, 11]);
        assert_eq!(working, vec![3, 4, 5, 6, 7, 8, 9, 10]);
    }

    #[test]
    fn pick_pristine_translated_chip_returns_bottom_right() {
        // 14-bit-set + translation: full grid working, last col = 16.
        let t = telem(0, 0x3FFF, true);
        let picked = pick_virtio_engine_tile(&t).unwrap();
        assert_eq!(picked.x, 16);
        assert_eq!(picked.y, 11);
        assert_eq!(picked.reason, PickReason::BottomRightCorner);
    }

    #[test]
    fn pick_pristine_untranslated_chip_returns_actual_corner() {
        // Without translation, 14 bits set means cols 1..7 + 10..16.
        // Last col is NOC0 16 — actual chip corner.
        let t = telem(0, 0x3FFF, false);
        let picked = pick_virtio_engine_tile(&t).unwrap();
        assert_eq!(picked.x, 16);
        assert_eq!(picked.y, 11);
    }

    #[test]
    fn pick_p100a_translated_returns_col_14() {
        // The actual p100a in our lab: 0xFFF + translation → max
        // working col is 14 (validated on hardware against
        // tensix-hello). Picker should land on (14, 11).
        let t = telem(0, 0xFFF, true);
        let picked = pick_virtio_engine_tile(&t).unwrap();
        assert_eq!(picked.x, 14);
        assert_eq!(picked.y, 11);
        assert_eq!(picked.reason, PickReason::BottomRightCorner);
    }

    #[test]
    fn pick_untranslated_12cols_returns_col_14() {
        // 0xFFF + no translation: bit indices 0..11 set →
        // [1,2,3,4,5,6,7,10,11,12,13,14] — last col = 14, same as
        // translated case (different code path, same answer).
        let t = telem(0, 0xFFF, false);
        let picked = pick_virtio_engine_tile(&t).unwrap();
        assert_eq!(picked.x, 14);
        assert_eq!(picked.y, 11);
    }

    #[test]
    fn pick_with_one_harvested_row_returns_harvested_corner() {
        // Bit 5 → row 7 harvested. Untranslated case so working_cols
        // includes NOC0 16 and we land on the actual chip corner of
        // the harvested row. Picker should pick (16, 7).
        let t = telem(1 << 5, 0x3FFF, false);
        let picked = pick_virtio_engine_tile(&t).unwrap();
        assert_eq!(picked.y, 7);
        assert_eq!(picked.x, 16);
        assert_eq!(picked.reason, PickReason::HarvestedRowCorner);
    }

    #[test]
    fn pick_errors_when_no_working_cols() {
        let t = telem(0, 0, true);
        assert!(matches!(
            pick_virtio_engine_tile(&t),
            Err(PickError::NoWorkingCols { .. })
        ));
    }

    #[test]
    fn pick_errors_when_all_rows_harvested() {
        // 10 bits set in harvesting_state — every row defective.
        let t = telem(0x3FF, 0x3FFF, true);
        assert!(matches!(
            pick_virtio_engine_tile(&t),
            Err(PickError::NoWorkingRows { .. })
        ));
    }

    #[test]
    fn pick_is_deterministic() {
        // Repeated calls on the same telemetry produce the same answer.
        let t = telem(1 << 3, 0xFFF, true);
        let a = pick_virtio_engine_tile(&t).unwrap();
        let b = pick_virtio_engine_tile(&t).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn pick_with_only_one_working_col() {
        // Untranslated, only bit 7 set ⟹ NOC0 col 10 only. Picker
        // takes the bottom-right corner of that single column.
        let t = telem(0, 1u32 << 7, false);
        let picked = pick_virtio_engine_tile(&t).unwrap();
        assert_eq!(picked.x, 10);
        assert_eq!(picked.y, 11);
    }

    #[test]
    fn pick_picks_first_harvested_row_not_last() {
        // Two harvested rows: bits 0 and 9 → rows 2 and 11. The
        // picker prefers the FIRST harvested row (lowest Y) for
        // determinism. tt-metal compute is steered away from
        // harvested rows regardless of which one we pick, so the
        // tie-break is just to be reproducible.
        let t = telem((1 << 0) | (1 << 9), 0x3FFF, false);
        let picked = pick_virtio_engine_tile(&t).unwrap();
        assert_eq!(picked.y, 2);
        assert_eq!(picked.x, 16);
        assert_eq!(picked.reason, PickReason::HarvestedRowCorner);
    }

    #[test]
    fn pick_output_is_always_in_decoded_working_cols() {
        // Property test: across a sweep of plausible (harvest,
        // col_mask, translation) tuples, the picker's chosen X is
        // a member of the working-cols set. (And similarly for Y in
        // the no-row-harvest case.)
        for col_mask in [0x0001u32, 0x0fff, 0x1fff, 0x3fff, 0xa55a, 0x1234] {
            for trans in [true, false] {
                for harvest in [0u32, 1u32 << 0, 1u32 << 5, 0x55, 0x12] {
                    let t = telem(harvest, col_mask, trans);
                    let cols = working_tensix_cols(col_mask, trans);
                    let rows = working_tensix_rows(harvest);
                    if cols.is_empty() || rows.is_empty() {
                        // Picker errors; nothing to check.
                        assert!(pick_virtio_engine_tile(&t).is_err());
                        continue;
                    }
                    let picked = pick_virtio_engine_tile(&t).unwrap();
                    assert!(
                        cols.contains(&picked.x),
                        "picker x={} not in working cols {:?} for col_mask={:#x} trans={}",
                        picked.x,
                        cols,
                        col_mask,
                        trans
                    );
                    let harvested = harvested_tensix_rows(harvest);
                    if !harvested.is_empty() {
                        assert!(
                            harvested.contains(&picked.y),
                            "picker y={} not in harvested rows {:?} for harvest={:#x}",
                            picked.y,
                            harvested,
                            harvest
                        );
                    } else {
                        assert!(
                            rows.contains(&picked.y),
                            "picker y={} not in working rows for harvest={:#x}",
                            picked.y,
                            harvest
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn translate_p100a_picker_output_is_identity() {
        // Our p100a: enabled_tensix_col=0xFFF, translation enabled.
        // The picker chose (14, 11). With nothing harvested before
        // col 14, the translation is identity.
        let t = noc0_to_translated_tensix(14, 11, 0xFFF, true);
        assert_eq!(t, Some((14, 11)));
    }

    #[test]
    fn translate_pristine_chip_is_always_identity() {
        // Full grid, translation enabled — every NOC0 coord maps to
        // itself.
        for &x in &TENSIX_COLS_NOC0 {
            for &y in &TENSIX_ROWS_NOC0 {
                let t = noc0_to_translated_tensix(x, y, 0x3FFF, true);
                assert_eq!(t, Some((x, y)), "({}, {})", x, y);
            }
        }
    }

    #[test]
    fn translate_disabled_col_returns_none() {
        // p100a: cols 15, 16 are at array positions 12, 13. With
        // enabled_count=12, those positions are past the range,
        // so the translation returns None.
        let t = noc0_to_translated_tensix(15, 11, 0xFFF, true);
        assert_eq!(t, None);
        let t = noc0_to_translated_tensix(16, 11, 0xFFF, true);
        assert_eq!(t, None);
    }

    #[test]
    fn translate_with_translation_disabled_is_identity() {
        // When the chip's NoC translation is off, the L2CPU TLB
        // takes raw NOC0 coords; identity.
        let t = noc0_to_translated_tensix(14, 11, 0x3FFF, false);
        assert_eq!(t, Some((14, 11)));
        let t = noc0_to_translated_tensix(15, 11, 0x3FFF, false);
        assert_eq!(t, Some((15, 11)));
    }

    #[test]
    fn translate_unknown_x_returns_none() {
        // 8 and 9 are router columns, not in TENSIX_COLS_NOC0.
        let t = noc0_to_translated_tensix(8, 5, 0x3FFF, true);
        assert_eq!(t, None);
        let t = noc0_to_translated_tensix(9, 5, 0x3FFF, true);
        assert_eq!(t, None);
    }

    #[test]
    fn working_cols_match_count_ones_in_translated_mode() {
        // Property test: for every plausible translated count, the
        // working-cols list has exactly `min(count, 14)` entries.
        for count in 0..=14u32 {
            let mask = if count == 32 {
                u32::MAX
            } else {
                (1u32 << count).wrapping_sub(1)
            };
            let cols = working_tensix_cols(mask, true);
            assert_eq!(
                cols.len() as u32,
                count,
                "count {} mask {:#x} produced {} cols, expected {}",
                count,
                mask,
                cols.len(),
                count
            );
        }
    }
}
