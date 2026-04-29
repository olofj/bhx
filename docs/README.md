# `docs/` — design notes and reference material

Stable references for things that are awkward to discover from the
source alone — chip-specific magic, operator-facing contracts, and the
parts of the design that needed a paragraph more than a code comment
could carry. Keep one document per topic; the source tree is the
authoritative reference for behavior, this directory is the *why*.

## Index

| Document | What's in it |
|---|---|
| [`blackhole-harvest-mask.md`](blackhole-harvest-mask.md) | How to read the Blackhole chip's row/column harvest mask. tt-kmd's `GET_HARVESTING` ioctl is a stub — the mask actually lives in the ARC firmware telemetry table at `SCRATCH_RAM[13]`. Includes the column-decode algorithm (the only place it appears as code outside `luwen`). Feeds the `src/tensix_tile.rs` picker. |
| [`sandbox-syscalls.md`](sandbox-syscalls.md) | The exact syscall + path set the daemon needs after seccomp + landlock are installed. Maintained alongside `src/daemon/sandbox.rs`; expand the inventory before adding any new daemon code path that hits a syscall outside the list. |
| [`telemetry.md`](telemetry.md) | The daemon's Prometheus-style metrics exporter — enable flag, listener security model, and the full per-metric inventory (daemon-global + per-L2CPU). Cross-reference for anyone scraping the daemon. |
| [`tt-metal-coexistence.md`](tt-metal-coexistence.md) | **bhx and tt-metal cannot run together on the same card today** — the L2CPU DRAM ranges and PCIe link reset aren't negotiated. This doc explains what's missing and what would need to land for coexistence to be safe. Documents the existing reserved-tile surface (`daemon status`, `$XDG_RUNTIME_DIR/bhx/<card>/reserved-tile`) as a hook for future tooling, not as a coexistence contract. |

## Where else to look

- [`../CLAUDE.md`](../CLAUDE.md) — per-module map of `src/`, working-style
  guidance, gotchas. Originally written for AI agents but the most
  thorough onboarding doc in the tree.
- [`../scripts/README.md`](../scripts/README.md) — operator-driven hardware
  soak scripts (warm-resume, concurrent boots, kill-recovery, fio /
  iperf3 sustained I/O, console roundtrip).
- [`../scripts/bench/README.md`](../scripts/bench/README.md) — per-surface
  performance baselines (disk, console, net) with a regression-fail mode.
- [`../third_party/uboot/README.md`](../third_party/uboot/README.md) — the pinned U-Boot build
  used for stock distro images (config fragment, downstream patches,
  reproducibility).
- [`../third_party/opensbi/README.md`](../third_party/opensbi/README.md) — the pinned OpenSBI
  build that produces `fw_jump.bin` (the M-mode payload that hands
  control to the kernel or U-Boot).
- [`../tests/rootfs/README.md`](../tests/rootfs/README.md) — the
  buildroot test rootfs construction.
