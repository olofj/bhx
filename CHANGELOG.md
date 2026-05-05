# Changelog

Notable changes per release. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project does not yet promise SemVer compatibility on the RPC
wire format or library API surface (we're not 1.0).

## 0.9.0 — 2026-05-05

First release-candidate-shaped cut. Boots stock distros end-to-end on
all four L2CPUs of a Blackhole P100 / P150 from the published cloud
images, with a per-card daemon, parallel virtio-mmio device
emulation, soft-reboot via OpenSBI purgatory, and a soak harness
demonstrating 300/300 successful guest reboot cycles.

### Added

- **Soft-reboot architecture** (#166). Guest-issued SBI SRST_SHUTDOWN
  parks the L2CPU's harts in OpenSBI's `sbi_hsm_hart_wait` instead of
  going dark. The host re-releases hart 0 from the parked state on
  the next `bhx boot` — no chip-side reset, no PCIe blip, sibling
  slots untouched. 100-cycle 3-guest soak (300 cycles total) at
  100% success.
- **Force-park IPI** for recovering kernel-wedged guests
  (`bhx boot -l N --force` on a `Running` slot). Custom M-mode IPI
  event drops the guest into the same purgatory path SBI SRST takes;
  works against guests running with `sstatus.SIE=0`. RNMI-based
  recovery for OpenSBI-itself-wedged is tracked separately in #167.
- **Opportunistic PCIe reset_board** folded into cold boot when no
  L2CPU is `Running` (#168). Silent end state — operator just sees
  their boot succeed against a cleanly-quiesced chip.
- **`bhx connect` exits on guest poweroff** — chip-console pump
  detects the parked-state transition and disconnects attached
  console clients with a goodbye line.
- **Per-card daemon** with RPC clients (`bhx boot`, `bhx connect`,
  `bhx add-disk`, `bhx remove-disk`, `bhx add-net`, `bhx remove-net`,
  `bhx daemon {start,stop,status,restart,logs}`). 64 KiB scrollback
  hub fans console output to multiple `bhx connect` clients with a
  Ro/Rw/Takeover writer election. Post-mortem replay (#160) of a
  stopped slot's last screenful or two via `bhx connect` after stop.
- **Stock distro support** via U-Boot S-mode payload + EFI loader
  chain. Boots Debian 13, Ubuntu 24.04 LTS, Fedora 42, AlmaLinux
  Kitten 10 from their published cloud images directly.
- **Cloud-init NoCloud seed** auto-attached on first boot; the
  daemon-emulated second virtio-blk slot carries the seed.
  `bhx cloud-init seed` regenerates with custom user-data /
  meta-data / network-config.
- **Image registry** (`bhx image list`, `bhx image info`,
  `bhx image pull`) — downloads + prepares cloud images into
  `$XDG_DATA_HOME/bhx/images/`. Idempotent.
- **virtio-mmio devices**: virtio-blk (up to 3 slots per L2CPU),
  virtio-net (libvdeslirp; behind `slirp` Cargo feature),
  virtio-console (`hvc0`), virtio-rng (entropy for EFI_RNG_PROTOCOL +
  `/dev/random`). All emulated by daemon-side workers driven by a
  Tensix-tile BRISC firmware engine.
- **L2CPU lifecycle design doc** (`docs/l2cpu-lifecycle.md`) covering
  states, transitions, host-side ownership.
- **Hardware-free test suite** — 400 unit tests covering CLI parsing,
  daemon protocol round-trips + SCM_RIGHTS, console-hub fan-out +
  writer election, kick-poller ring consumer, virtio descriptor
  dispatcher, console-input CPR filter, chip-reset poll state
  machine, clock PLL stepping, and more.
- **Prometheus exporter** (`bhx daemon start --metrics-port`) —
  per-L2CPU + chip-side metrics for kick drops, SEL→READY race
  windows, OLD-sel rescue captures, console clients, UART feed
  drops, and worker poll iterations.
- **Daemon sandbox** (`--no-sandbox` to disable) — seccomp + Landlock
  filters installed after warm-resume.

### Removed

- The legacy `BHX_SOFT_REBOOT` env-var gate is gone — soft-reboot is
  now the default. Syscon-poweroff DTB injection, the
  `guest_poweroff_handler` thread, and the `regs::shutdown` module
  retired with it.
- `--force-reset-pcie` is gone. The chip-wide reset is exclusively
  the opportunistic Phase 6 path; `bhx debug reset-x280` /
  `tt-smi -r` remain as the explicit low-level escape hatches.

### Notes

- The OpenSBI patches under `third_party/opensbi/patches/` are
  vendored at this release; the Tenstorrent X280 Smrnmi support
  needed for OpenSBI-internal-wedge recovery is pending upstream
  (#167) and not yet integrated.
- Ubuntu 25.10 / 26.04 boot is blocked on Zcb emulation work
  tracked in #163; defer to the 1.0 release.
- The BRISC firmware C source still has stale comments + dead
  diagnostic counters from race-fix iteration; partial cleanup
  landed for the daemon-side mirror, firmware-side cleanup deferred.
