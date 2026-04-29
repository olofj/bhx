# bhx and tt-metal on the same card

**bhx cannot currently run alongside tt-metal on the same card.**
Don't try. Stop one before starting the other. This document is the
reference for *why*, and a sketch of what would need to land for
coexistence to become safe.

## Why coexistence isn't supported today

Even if you carefully exclude bhx's reserved Tensix tile from
tt-metal's `DispatchCoreConfig`, several other shared resources are
**not negotiated**:

- **L2CPU DRAM.** Each L2CPU bhx boots claims a fixed range of
  on-chip DRAM. The base addresses + sizes are hard-coded in
  `src/l2cpu.rs::L2CPU_STARTING_ADDRESS` /
  `L2CPU_MEMORY_SIZE`: 4 GiB at `0x4000_3000_0000` for L2CPU 0/1
  each, 2 GiB at `0x4000_3000_0000` / `0x4000_b000_0000` for L2CPU
  2/3 — totalling 12 GiB across the 4 L2CPUs. tt-metal's allocator
  has no idea those ranges are taken; it'll happily place tensors
  there and either corrupt the running guests' memory or get
  silently corrupted by L2CPU writes.
- **PCIe link reset.** `bhx daemon stop`'s teardown path and
  `--force-reset-pcie` both go through `chip::reset_board`, which
  resets the entire PCIe link to the card. Anything tt-metal had
  open against `/dev/tenstorrent/<idx>` becomes invalid —
  in-flight kernels lose their device fd.
- **Tensix tile.** The reserved tile (BRISC virtio engine) is the
  one piece bhx *does* surface — see "Reservation surface" below
  — but it's only meaningful in the future world where DRAM and
  PCIe-reset are also negotiated.

Until those three are negotiated, the safe rule is **one tool at a
time per card**.

## What happens if you ignore the rule

- **Silent guest corruption.** L2CPU memory pages stomped by
  tt-metal compute show up as virtio-blk retry storms,
  virtio-net packet loss, or random kernel oopses on the guest.
  The daemon is still running and the chip looks healthy — there
  is no error path that catches this.
- **tt-metal kernel corruption.** Same in reverse: an L2CPU
  writing to its own DRAM stomps tensors tt-metal placed there.
- **Daemon SIGBUS / chip wedge.** Possible but not the common
  failure mode. Recovery: `daemon stop`, `tt-smi -r`, restart.

None of these are bugs in either tool — they're the expected
outcome of two unrelated processes writing to the same physical
memory on the same chip.

## Reservation surface (informational, for future tooling)

bhx already exports the one piece of state that a coexistence
implementation will need: **the Tensix tile coordinate hosting the
BRISC virtio engine.** It surfaces in two places:

- `bhx daemon status -t <card>` prints
  `virtio-engine tile (NOC0): (x, y)`. Absent until at least one
  L2CPU has been booted (engine bring-up is lazy).
- `$XDG_RUNTIME_DIR/bhx/<card>/reserved-tile` is a single line
  `<x> <y>\n` in NOC0-logical coordinates. Removed on `daemon
  stop`. Documented as a stable file format so future tooling can
  rely on it.

That's it. There is no DRAM-range surface yet, no PCIe-reset
coordination, no claim on `/dev/tenstorrent/<idx>` beyond the
kernel-level fd open. **Reading the reserved-tile file alone is
not enough to coexist** — see the unmet preconditions in the next
section.

## Tt-metal-firmware sniffer

At engine bring-up, the daemon reads a few bytes from the candidate
tile's L1 looking for tt-metal firmware signatures (`dispatch.cpp`
TCM residue is the typical pattern). On a hit it logs:

```
[tensix-engine] WARNING: tile (16, 11) appears to be running
tt-metal firmware (signature 0x… at L1+0x…). bhx is taking the
tile over and may corrupt the running workload. Stop tt-metal
first or configure DispatchCoreConfig to exclude this tile.
```

The warning is **not fatal** — bring-up still proceeds. We can't
distinguish "tt-metal firmware that finished and left bytes
around" from "tt-metal firmware running right now." The warning
is your hint to investigate, not a load-bearing safety check.

## What would need to land for real coexistence

Listed roughly in order of how blocking each piece is. None of
this is in scope for bhx today; this section is here so a future
implementer doesn't have to re-derive the gap list.

1. **L2CPU DRAM reservation.** Either bhx publishes its claimed
   DRAM ranges to a host-side registry that tt-metal's allocator
   consults, *or* a tt-kmd ioctl gates DRAM allocations on a
   reservation table. The hard-coded constants in `src/l2cpu.rs`
   become the input.
2. **PCIe-reset coordination.** `chip::reset_board` needs a
   gate: refuse to fire while another process holds an open fd
   against the same card, or coordinate via a tt-kmd-mediated
   reset-token. The existing flock on
   `$XDG_RUNTIME_DIR/bhx/<card>/daemon.pid` only protects against
   two bhx daemons; it doesn't see tt-metal.
3. **Tile-reservation as a kernel claim.** The reserved-tile file
   is voluntary and human-driven. A real claim is a tt-kmd
   reservation interface that tt-metal's tile picker observes.
4. **`DispatchCoreConfig` plumbing on the tt-metal side.** Even
   with (1)–(3) landed, tt-metal needs an API to consume the
   reservation. Today its `DispatchCoreConfig` exclude-list is
   the closest hook but isn't wired to anything outside the
   tt-metal process.

Each of these is a real piece of cross-project work. None are
imminent.

## See also

- #66 — Tensix-as-virtio-engine architecture umbrella.
- #68 — M2 picker (deterministic, harvested-row preference).
- `src/l2cpu.rs::L2CPU_STARTING_ADDRESS` — the DRAM ranges that
  would have to be surfaced to a future allocator.
- `src/tensix_engine.rs::write_reserved_tile_file` — the existing
  reservation-file writer.
