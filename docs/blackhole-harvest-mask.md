# Reading the Blackhole harvest mask

(Originally filed as issue #75 — moved into `docs/` since the value is
the text itself, not the issue lifecycle.)

**Reference / discovery doc.** Captures findings from looking at
`tt-kmd`, `luwen`, and `tt-metal` UMD source while planning M2
(#68 — Tensix tile selection). Posterity issue so the next person
to need this doesn't re-walk the same trail.

Discovery context: while bringing up M1 (#67) on a p100a I observed
that NOC0-logical Tensix coords `x ∈ [11, 16]` were unreachable
(reads return `0xFFFFFFFF`) even though `tt-smi` reported
`HARVESTING_STATE = 0x0` and `ENABLED_TENSIX_COL = 0xFFF`. The
observed unreachable count was bigger than what either telemetry
field alone would predict, so before designing M2 I traced where
those telemetry values actually live and how upstream tools read
them.

## TL;DR

- `tt-kmd` does **not** expose the harvest mask. Both the
  `TENSTORRENT_IOCTL_GET_HARVESTING` ioctl and the sysfs/hwmon
  telemetry surface are stubs / partial (see below).
- The harvest mask is in the **ARC firmware telemetry table** in
  CSM. ARC firmware populates it from board flash + chip-class
  defaults at boot. `tt-smi` (via `pyluwen`) and the UMD all
  read it the same way: AXI-read the table and decode tags by id.
- Three tags matter for our use case:
  - `HarvestingState = 4` (row harvest)
  - `EnabledTensixCol = 34` (column enable bitmask)
  - `NocTranslation = 40` (which coordinate decode rule applies)
- The reads all land inside the **existing
  `SharedChip`** AXI tile-(8,0) window (base `0x80000000`,
  size 2 MiB) — no new ioctls or kmd changes required.

## What `tt-kmd` does NOT give us

The `TENSTORRENT_IOCTL_GET_HARVESTING` ioctl number is defined
(`ioctl.h`, value `_IO(0xFA, 1)`) but the dispatch table in
`chardev.c` is literally:

```c
case TENSTORRENT_IOCTL_GET_HARVESTING:
    break;
```

— it returns 0 and does nothing. Don't call it; you'll get an
empty response with no useful data.

`tt-kmd`'s sysfs/hwmon does expose *some* telemetry tags
(`telemetry.h:15` enum: `BoardId`, `Vcore`, `AICLK`,
`AsicTemp`, …). But the enum **does not include**
`HarvestingState`, `EnabledTensixCol`, or `NocTranslation`.
There is no sysfs node for harvest data. The kernel-side cache
(`tt_dev->telemetry_tag_cache`) holds addresses for *every*
discovered tag, but only the listed ones are surfaced upward.

## How `luwen`/`tt-smi`/`tt-metal` actually read it

All three go through the same path in `luwen-api`'s
`get_telemetry()` (file
`crates/luwen-api/src/chip/blackhole.rs:707+`). The protocol:

1. **Find the table.** Read `SCRATCH_RAM[13]` at AXI
   `0x80030434` (tile (8,0)).
   - The address is computed as
     `arc_ss.reset_unit.SCRATCH_RAM[13]` resolved via
     `luwen/axi-data/blackhole-axi-pci.bin`.
   - Cross-checked in `tt-kmd/blackhole.c:59`:
     `#define RESET_SCRATCH(N) (0x80030400 + ((N) * 4))`.
   - Cross-checked in `tt-metal` UMD
     (`blackhole_implementation.hpp:253`): `SCRATCH_RAM_13 =
     ARC_RESET_UNIT_OFFSET + 0x434`.
2. **Sanity-check.** The returned address must lie in the ARC
   CSM range `0x10000000..0x1007FFFF` (`tt-kmd/telemetry.h`
   defines `ARC_CSM_BASE = 0x10000000`,
   `ARC_CSM_SIZE = 1<<19`); zero or out-of-range means ARC
   firmware hasn't finished booting.
3. **Read the header.**
   - `[base + 0..3]` = version (currently 1; ignore for now).
   - `[base + 4..7]` = entry count (`u32`).
4. **Read the tag table** at `[base + 8 .. base + 8 + count*4]`.
   Each entry is a single `u32`:
   - low 16 bits = tag id
   - high 16 bits = data offset (in `u32` units)
5. **Read the data block** at
   `[base + 8 + count*4 .. base + 8 + count*4 + count*4]`.
   Per tag, the value is the `u32` at offset
   `base + 8 + count*4 + tag_offset*4`.
6. **Decode by tag id** using the enum from
   `luwen-api/src/chip/blackhole/telemetry_tags.rs`. Tags we
   care about for M2 / harvest:

   | tag id | name                  | meaning                              |
   |--------|-----------------------|--------------------------------------|
   | 4      | `HarvestingState`   | row harvest bitmask                  |
   | 34     | `EnabledTensixCol`  | column enable bitmask (soft harvest) |
   | 40     | `NocTranslation`    | non-zero ⟺ coord translation active  |

   (The full enum is reproduced in luwen's source — 50+ tags. We
   only need these three.)

The "soft harvest" you might suspect from a chip having more
unreachable cols than telemetry alone implies is real: ARC firmware
fuses board-flash + chip-class defaults into the telemetry table
during boot. The mask we see is whatever ARC published; nothing
downstream of ARC can change it.

## Coordinate decoding (column harvest)

This is the algorithm `luwen`'s
`tests/read_write_test.rs:520-565` uses to convert
`(EnabledTensixCol, NocTranslation)` into a list of valid
NOC0-logical Tensix coords. It is the **only** place the full
algorithm appears as code; it's not in any docstring or RFC I
could find.

```rust
// Pseudocode mirroring luwen's logic.
let working_count = enabled_tensix_col.count_ones();
let tensix_cols_noc0 = [1,2,3,4,5,6,7,10,11,12,13,14,15,16];

let valid_cols: Vec<u32> = if noc_translation_enabled {
    // First `working_count` logical positions are valid.
    // The mapping skips the col-8/col-9 router gap.
    (0..14u32).filter(|&i| {
        let x = tensix_cols_noc0[i as usize];
        (x <=  7 &&  x      < working_count)
        || (x >= 10 && (x-2) < working_count)
    }).map(|i| tensix_cols_noc0[i as usize]).collect()
} else {
    // Bit `i` of mask ⟺ NOC0 col `tensix_cols_noc0[i]`.
    (0..14u32).filter(|i| enabled_tensix_col & (1 << i) != 0)
              .map(|i| tensix_cols_noc0[i as usize]).collect()
};
```

Two follow-on observations:

- The `(x <= 7 && x < working_count)` form looks weird, but it
  is correct. The 14 logical column positions are
  `[1..7, 10..16]`; the router gap between cols 7 and 10
  collapses to one slot's worth of skip in the translated
  numbering, so subtracting 2 from cols ≥ 10 yields the
  contiguous index 0..13.
- The `HarvestingState` tag (rows) decodes the same way:
  bit `i` set ⟺ row `i` (in `y = 2..11`) is harvested. We
  haven't seen a chip with row harvest yet, so this is informed
  by tt-metal docs more than direct evidence.

## What this means for tt-bh-linux M2

- All required reads land inside the existing 2 MiB
  `SharedChip` window at `0x80000000`. Reuse it; don't add a
  second AXI accessor.
- Implementation is pure user-space code over three `axi_read32`
  values + the lookup table above. No tt-kmd patches, no ARC
  message round-trips, no new fds.
- The decoder is unit-testable: feed synthetic
  `(HarvestingState, EnabledTensixCol, NocTranslation)`
  triples and assert the produced valid-tile list. Cover at
  least: pristine, 1 row harvested, 2 cols harvested in
  translated mode, 2 cols harvested in non-translated mode, and
  a "no candidate qualifies" edge case.
- A `debug telemetry-dump` subcommand that prints all three
  tags plus the decoded valid set is worth adding now — useful
  for diagnostics and for cross-checking M2's picker against
  ground truth on any new card.
- One thing the algorithm does NOT cover: per-row +
  per-column simultaneous harvest with arbitrary patterns. The
  decoders treat them as independent. If we ever see a chip
  with both, M2's picker should AND the two masks to get
  reachable tiles.

## References

- `tt-kmd` (this discovery's source repo for the stub
  observation): `chardev.c` GET_HARVESTING dispatch, `telemetry.h`
  enum, `telemetry.c::tt_telemetry_read32`,
  `blackhole.c::telemetry_probe`.
- `luwen-api`:
  - `crates/luwen-api/src/chip/blackhole.rs:707-834` — table walk
  - `crates/luwen-api/src/chip/blackhole/telemetry_tags.rs` —
    full tag enum
  - `tests/read_write_test.rs:520-565` — the column decode
    algorithm (only place it appears as code)
- `tt-metal` UMD:
  `tt_metal/third_party/umd/device/api/umd/device/arch/blackhole_implementation.hpp`
  — confirms `SCRATCH_RAM_13 = ARC_RESET_UNIT_OFFSET + 0x434`.

## Related

- #66 — Tensix-as-virtio-engine architecture (umbrella).
- #68 — M2: Tensix tile selection. This document feeds the
  implementation directly.
- #67 — M1 (this is where the discovery happened — see
  M1 verification comment for the empirical observations).

