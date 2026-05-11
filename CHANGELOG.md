# Changelog

Notable changes per release. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project does not yet promise SemVer compatibility on the RPC
wire format or library API surface (we're not 1.0).

## Unreleased

V2 virtio-dispatch redesign. The kick ring + completion ring + host-
side throttle that grew up around #184 are gone; in their place is a
per-(slot, queue) dirty bitmap in BRISC L1. The bitmap is level-
sensitive — guest QUEUE_NOTIFY storms coalesce into a single set
byte, so the dispatch path can't fall behind under any burst. Wire
incompatible with 0.9.0; `TENSIX_PROTOCOL_VERSION` bumped 4 → 5.

### Added

- **V2 dirty-bitmap dispatch** (`#187` / `#188` / `#189`). BRISC
  writes 1 to `CTRL_OFF_DIRTY[slot][queue]` on every guest
  QUEUE_NOTIFY; the daemon's `Dispatcher` clears the byte and
  dispatches each pass. Replaces V1's 2048-entry kick ring +
  daemon-side `consume_kick_ring_pass` consumer.
- **V2 processed-cursor table** at `CTRL_OFF_PROCESSED`. Daemon
  publishes `used.idx` after each successful dispatch so
  warm-resume reads cursors directly without re-probing guest
  DRAM.
- **`bhx_notify_events_total`, `bhx_dispatch_passes_total`,
  `bhx_dispatch_queues_drained`** Prometheus counters surface the
  new dispatch path. The burst regression test (`scripts/
  soak_virtio_burst.py`) asserts `dispatch_passes_total > 0` to
  confirm the workload reached the new path.
- **`scripts/soak_virtio_burst.py`** — multi-queue burst regression
  test. Sustains 16-job direct=1 fio randwrite + a tight
  `printf` loop to `/dev/console`, samples `/metrics` every 1 s,
  and verifies the daemon log contains zero
  `kick.*drop|rescue|throttle.*ENGAGE` matches.
- **`DaemonState.chip_reset_this_session`** flag — gates
  `maybe_opportunistic_reset_board` so 4-way parallel cold boots
  reset the chip exactly once, not once per L2CPU. Without this
  the second-and-later resets blip the chip while earlier-booted
  L2CPUs hold mmap pages, SIGBUSing their workers.
- **`Dispatcher` (was `KickPoller`)** with documented testability
  seam (`CtrlL1Access` trait); `drain_dirty_bitmap` is unit-tested
  against an in-memory L1 fake covering all five visit/clear
  semantics cases plus the address-formula pins.

### Changed

- **`KickPoller` → `Dispatcher`**, plus `kick_poller` → `dispatcher`
  field on `DaemonState`, `tensix-kick-poller` → `tensix-dispatcher`
  thread name, `[kick-poller]` → `[dispatcher]` log tag,
  `kicks_consumed` → `dispatches_total`,
  `last_kick_slot_queue` → `last_dispatch_slot_queue`. Pure
  rename; no behavior change. V1 vocabulary scrubbed throughout
  the codebase (firmware, daemon, scripts, docs).
- **`CTRL_SIZE` shrinks 36 KiB → 4 KiB**. V2 footprint is ~1.5 KiB;
  the rest is reserved for future fields.
- **Stats-page offsets repacked** — V1 `STATS_OFF_KICK_DROPS`,
  `STATS_OFF_COMPL_EVENTS`, `STATS_OFF_LAST_COMPL` retired with
  V1 (#190); deprecated PRECAP / BLINDCAP / POSTCAP slots dropped
  in this cleanup pass. `proto::PROTOCOL_VERSION` is the gate on
  the layout shift.
- **`TensixEngine`** gains typed L1 helpers (`read_l1_u8`,
  `write_l1_u8`, `write_l1_u16`) so the V2 dispatcher's volatile
  byte / halfword accesses go through one centralized
  `unsafe`-block per primitive instead of inline ad-hoc casts.
- **`scripts/soak_fio_remove_disk.py`** — three pre-existing harness
  bugs fixed: regex matching against execution-output (instead of
  false-matching the typed-back command echo), staged fio binary
  in tmpfs to dodge vda-page-cache eviction across many remove/add
  cycles, dropped `rm /root/fio.tmp` from kill_fio (the unlink
  queued behind D-state fio I/O on the just-yanked disk and
  wedged the shell). 100/100 iter run now passes cleanly.

### Removed

- **`KickEntry` / `CompletionEntry` types**, their offsets, and the
  `STATS_OFF_KICK_DROPS` / `STATS_OFF_COMPL_*` stats. The V1
  Rust-side `consume_kick_ring_pass`, the throttle state machine
  (`THROTTLE_HIGH_PCT` / `THROTTLE_LOW_PCT` / `set_used_no_notify
  _for_all_queues` / `rescan_all_queues`), the
  `bhx_kick_drops_total` / `bhx_kick_ring_high_water` /
  `bhx_kick_ring_current_gap` / `bhx_kick_rescued_total` /
  `bhx_kick_throttle_engaged` /
  `bhx_kick_throttle_transitions_total` metrics, and the
  `[kick-poller] BRISC dropped N kicks` log line are all gone.
- **BRISC firmware: `kick_ring_push`, `poll_completion_ring`,
  per-slot `epoch_addr`** RMW (V1's STATUS=0 detector — V2 reads
  MMIO_STATUS directly), kick-ring + completion-ring init in
  `init_proto`. Firmware text shrinks 5012 → 4844 bytes.

### Notes

- `CTRL_OFF_STATE_LOG` was reserved in early V2 patches but never
  written; dropped during cleanup. 0x0400..CTRL_SIZE in the V2
  layout is reserved for future fields with the convention "bump
  `proto::PROTOCOL_VERSION` on any addition."
- The `Registry` mutex is still held across the entire
  `drain_dirty_bitmap` pass — same shape as V1, not a regression.
  See FIXME comment on the type alias for the per-slot RwLock /
  snapshotted-view refactor when add/remove SLAs need tightening.

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
