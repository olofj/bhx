# Reading the Blackhole harvest mask

How `bhx` figures out which Tensix tiles on a Blackhole chip are
actually reachable. Captured here because `tt-kmd`'s
`TENSTORRENT_IOCTL_GET_HARVESTING` is a stub and the actual decode
algorithm only appears as code inside `luwen` — whoever next needs
this on a different chip family or under a Wormhole port shouldn't
have to re-walk the same trail.

Empirical context: on a p100a, NOC0-logical Tensix coords
`x ∈ [11, 16]` read back as `0xFFFFFFFF` even though `tt-smi`
reported `HARVESTING_STATE = 0x0` and `ENABLED_TENSIX_COL = 0xFFF`.
The unreachable-column count was bigger than either telemetry
field alone would predict — that prompted the trace through
`tt-kmd` / `luwen` / `tt-metal` UMD that's the rest of this doc.

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
   `luwen-api/src/chip/blackhole/telemetry_tags.rs`. Tags `bhx`
   uses for the harvest decode:

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

## How `bhx` uses this

The decode shipped as `src/telemetry.rs::read_telemetry` (table
walk + tag indexing) and `src/tensix_tile.rs::working_tensix_cols`
+ `working_tensix_rows` (the per-row / per-column lookups described
above). They feed `pick_virtio_engine_tile`, which the daemon
calls at engine bring-up to choose the Tensix tile that hosts the
virtio firmware.

Implementation choices the doc above motivated:

- All reads go through the existing 2 MiB `SharedChip` window at
  AXI `0x80000000` — no second accessor.
- Pure user-space: three `axi_read32` calls + the lookup table.
  No `tt-kmd` patches, no ARC message round-trips, no new fds.
- The decoder is unit-tested in `src/tensix_tile.rs` against
  synthetic `(HarvestingState, EnabledTensixCol, NocTranslation)`
  triples covering pristine, 1-row-harvested, 2-cols-harvested
  in both translated and non-translated mode, and the
  no-candidate-qualifies edge case.
- A `bhx debug telemetry-dump` subcommand prints all three tags
  plus the decoded valid set, for cross-checking the picker
  against ground truth on any new card.

One known gap: the algorithm treats row + column harvest as
independent. A chip with both simultaneously would need an AND of
the two masks to get reachable tiles. We haven't seen one yet; if
one shows up the picker in `tensix_tile.rs` is the place to grow
the combined check.

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

- [`src/telemetry.rs`](../src/telemetry.rs) — table walk + tag-by-id reader.
- [`src/tensix_tile.rs`](../src/tensix_tile.rs) — `working_tensix_cols` /
  `working_tensix_rows` decode + `pick_virtio_engine_tile` picker.
- [`src/shared_chip.rs`](../src/shared_chip.rs) — the AXI tile-(8,0)
  accessor every read on this path uses.
- GitHub issue [#66](https://github.com/olofj/bhx/issues/66) — the
  Tensix-as-virtio-engine architecture umbrella that motivated all
  of this.

