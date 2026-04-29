# Running tt-bh-linux alongside tt-metal on the same card

`tt-bh-linux daemon` reserves exactly one Tensix tile on each card it
manages — the tile hosts BRISC firmware that emulates the four
virtio-mmio devices each L2CPU sees. tt-metal also schedules compute
onto Tensix tiles. **Without coordination, tt-metal can pick the same
tile, overwrite our firmware mid-flight, and corrupt the L2CPU's
virtio I/O without any warning.**

This document describes the convention we ask operators to follow.
Enforcement is voluntary today; a future tt-kmd reservation interface
would make it mandatory. Until then: read the reservation, exclude
the tile, expect silent corruption if you don't.

## Reading the daemon's reservation

After the daemon brings up its first L2CPU, the reservation surfaces
in two places:

### 1. `daemon status`

```
$ tt-bh-linux daemon status -t 0
daemon: running (card 0, pid …, uptime …)
  virtio-engine tile (NOC0): (16, 11)
  l2cpu 0: Running …
  …
```

The "virtio-engine tile (NOC0)" line shows the NOC0-logical
coordinate the daemon picked. Absent until at least one L2CPU has
been booted (engine bring-up is lazy). The same line appears for
every `daemon status` invocation — value is stable until daemon stop.

### 2. `$XDG_RUNTIME_DIR/tt-bh-linux/<card>/reserved-tile`

A single line, machine-parseable: `<x> <y>\n`. Same NOC0-logical
coordinate. Useful for shell automation:

```sh
read -r ENGINE_X ENGINE_Y < "$XDG_RUNTIME_DIR/tt-bh-linux/0/reserved-tile"
echo "Daemon's engine tile is ($ENGINE_X, $ENGINE_Y)"
```

The file is unlinked on `daemon stop`. If it's absent, either
(a) the daemon hasn't booted an L2CPU yet, or (b) no daemon is
running for the card — fall back to launching tt-metal without
the exclusion.

## Excluding the tile from tt-metal compute

tt-metal's `DispatchCoreConfig` controls which tiles tt-metal will
schedule kernels onto. The exact API moves between releases — see
[`tt_metal/api/tt-metalium/dispatch_core_common.hpp`](https://github.com/tenstorrent/tt-metal/blob/main/tt_metal/api/tt-metalium/dispatch_core_common.hpp)
for the current shape (last verified against tt-metal v0.50,
2026-04-29).

Conceptually:

```cpp
auto cfg = tt::tt_metal::DispatchCoreConfig{};
cfg.exclude_core(CoreCoord{ENGINE_X, ENGINE_Y}); // pseudo-API
auto device = CreateDevice(0, cfg);
```

A typical wrapper script (Python, calling out to a small Rust /
C++ tt-metal harness) would read the file and pass the coordinate
through:

```python
import os, subprocess

card = 0
runtime = os.environ.get("XDG_RUNTIME_DIR", f"/tmp/tt-bh-linux-{os.getuid()}")
reserved = os.path.join(runtime, "tt-bh-linux", str(card), "reserved-tile")

extra_env = {}
try:
    with open(reserved) as f:
        x, y = map(int, f.read().split())
        extra_env["TT_METAL_EXCLUDE_TILE"] = f"{x},{y}"
except FileNotFoundError:
    pass  # no daemon — fall through

subprocess.run(["./your-tt-metal-app"], env={**os.environ, **extra_env})
```

The `TT_METAL_EXCLUDE_TILE` shape is illustrative — your harness
should parse it into whatever shape the current `DispatchCoreConfig`
API expects.

## What goes wrong if you don't

If tt-metal schedules a kernel onto the daemon's reserved tile while
the daemon is running, you can expect any of:

- **Silent virtio corruption.** The kernel's TX/RX registers are
  overwritten by tt-metal's instruction stream; the daemon-side poll
  loop sees garbage and may dispatch to a stale descriptor. Symptoms
  on the L2CPU side: virtio-blk retry storms, virtio-net packet
  loss, virtio-console garbage characters.
- **Daemon SIGBUS.** Less common — tt-metal's kernel may write to
  Tensix L1 ranges the daemon's TLB also maps. The daemon traps and
  exits; the chip stays in whatever state tt-metal left it in.
- **Subsequent boots fail.** A corrupted firmware can't be cleanly
  re-adopted. Operators see "tt-bh-linux daemon start" succeed, but
  every L2CPU boot fails with a virtio handshake timeout. Recovery:
  `daemon stop`, `tt-smi -r`, `daemon start`, `boot --force`.

None of these are bugs in tt-bh-linux or tt-metal individually —
they're the expected outcome of two processes writing to the same
Tensix tile's L1 + reset registers.

## What the daemon does to detect collisions

At engine bring-up the daemon reads a few magic bytes from the
candidate tile's L1 to look for tt-metal firmware signatures (a
common pattern is the first few bytes of `dispatch.cpp`'s code in
TCM). If found, the daemon logs:

```
[tensix-engine] WARNING: tile (16, 11) appears to be running tt-metal
firmware (signature 0x… at L1+0x…). tt-bh-linux is taking the tile
over and may corrupt the running workload. Stop tt-metal first or
configure DispatchCoreConfig to exclude this tile.
```

The warning is **not fatal** — bring-up still proceeds. We can't
distinguish "tt-metal firmware that finished and left bytes around"
from "tt-metal firmware running right now," so a hard fail would
trigger on every fresh chip after a tt-metal session even when
nothing's actively running. The warning is your hint to investigate.

## Future work

The convention above is voluntary. A future tt-kmd reservation
interface would let the daemon claim the tile process-wide and make
tt-metal's tile picker observe the claim. That requires a tt-kmd
patch and a tt-metal patch; not in scope for this document.

## See also

- #66 — Tensix-as-virtio-engine architecture umbrella.
- #68 — M2 picker (deterministic, harvested-row preference).
- #75 — How the daemon reads harvest mask + active grid (tt-kmd
  doesn't expose it directly today).
