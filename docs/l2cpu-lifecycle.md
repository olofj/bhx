# L2CPU lifecycle and state transitions

bhx exposes an L2CPU as a daemon-managed slot with a small set of
states. The states differ on three independent axes: whether the
daemon holds an `L2Cpu` Arc, whether OpenSBI is actively running on
the chip, and whether a guest kernel has been launched. This
document is the canonical reference for what each state means, how
transitions happen, and what's safe to do in each.

For end-to-end design rationale (the OpenSBI-purgatory architecture,
the L2CPU "reset bit can only flip 0→1 once" hardware bug, the
upstream Smrnmi work) see [#166](https://github.com/olofj/bhx/issues/166)
and the tt-isa-documentation references therein.

## States

The four enum variants on the wire (`L2CpuState` in
`src/daemon/protocol.rs`):

| State | Daemon `L2Cpu` Arc | Workers | OpenSBI on chip | Kernel | What `purgatory:` reads |
|---|---|---|---|---|---|
| `Stopped` | dropped | none | held in reset (`L2CPU_RESET` bit `idx+4` = 0) | no | n/a (slot is None — daemon doesn't read) |
| `Running` | live | running | live in M-mode | live in S-mode (or U-Boot, or SBI debug) | `0` ("running, no SRST yet") |
| `Parked` | live | running but idle | live, all 4 harts in `sbi_hsm_hart_wait` | gone (kernel issued SBI SRST) | `0x5f5f44454b524150` ("`PARKED__`") |
| `Wedged` | dropped | none | unknown — reset bit is set, but OpenSBI debug-descriptor magic missing | unknown | n/a |

`Wedged` is rare — it's the daemon's startup-probe verdict for a
slot whose chip-side reset bit was already set when the daemon
attached but whose OpenSBI debug-descriptor doesn't read as the
expected magic. Treated like `Stopped` for cold-boot purposes; the
operator sees it in `daemon status` as a heads-up that the slot
came up un-cleanly.

## What lives where

Some state lives on the daemon side, some on the chip side. The
distinction matters because each state's `Drop` follows a different
path:

| Object | Lifetime |
|---|---|
| `state.l2cpus[i]: Mutex<Option<L2CpuSlot>>` | always present in `DaemonState`; `Some` only while the slot has been booted (or warm-resumed) |
| `L2CpuSlot.l2cpu: Arc<L2Cpu>` | constructed at cold boot, dropped at slot teardown. Holds the per-L2CPU `dup_fd` + 8 GiB VA + two persistent 4 GiB TLB windows. |
| `L2CpuSlot.console_worker / disks / net / virtio_console / virtio_rng` | spawned at cold boot, `stop_and_join`ed on slot teardown |
| `state.tensix_engine` and `state.kick_poller` | shared across all 4 L2CPUs on the card; brought up on first cold boot, dropped on chip-wide reset (`tt-smi -r` or `reset_board`) |
| OpenSBI `_fw_start..._fw_reserved_space_end` in L2CPU DRAM | written once at cold boot, preserved across SBI SRST → warmboot loop, **wiped by `reset_board`** (DRAM training overwrites it) |
| OpenSBI `sbi_scratch[]`, `sbi_ipi_data[]`, `sbi_hsm_data[]` | persistent across the warmboot loop; published PAs in the bhx-purgatory status block at cold init |
| Bhx-purgatory status block (DRAM at `mem_base + 0xE0000`, 4 KiB) | reserved-memory carve-out in DTB; written by OpenSBI at cold init (Phase 5 metadata) and `final_exit` (Phase 1/2/4a status); read by the daemon over NoC |

## Transitions

### Cold boot

```
Stopped/Wedged → (Phase 6 opportunistic reset_board if safe) → Running
```

- Trigger: `bhx boot -l N`.
- Pre-flight: if no other slot is `Running`, daemon drops parked
  siblings + kick poller + tensix engine and calls
  `SharedChip::reset_board`. Parked siblings transition Parked → Stopped.
- `run_boot_sequence` constructs `L2Cpu::new`, modifies DTB, calls
  `boot_l2cpu` (writes OpenSBI / kernel / DTB / initramfs into DRAM),
  configures prefetchers, calls `reset_x280` (PLL step + release
  bit OR-in). Per-hart reset vectors point at OpenSBI's `_start`.
- Post-boot: `make_slot_from_l2cpu` spawns `chip_console`, registers
  virtio devices with the kick poller, attaches disks/net.
- Slot transitions to `Running` once the install completes.

The pre-flight `reset_board` is the only chip-wide reset in normal
operation. It's silent — the operator just sees their boot succeed.

### In-guest shutdown

```
Running → Parked
```

- Trigger: guest kernel issues `SBI SRST_SHUTDOWN` (e.g., `poweroff
  -f`).
- OpenSBI's stub `bhx-purgatory` reset device returns immediately,
  letting `sbi_system_reset` fall through to `sbi_exit` →
  `sbi_platform_final_exit` (our hook) → `sbi_hsm_exit` →
  `jump_warmboot`. All 4 harts converge into `sbi_hsm_hart_wait`.
- `final_exit` polls peer HSM state until everyone is `STOPPED`,
  writes the PARKED magic + metadata to the status block.
- Daemon's `dispatch_status` reads the status block on each query;
  when it sees PARKED, returns the slot as `Parked`.
- Daemon-side workers stay alive — chip-console keeps polling the
  virtuart, kick poller stays registered. They're idle (the chip
  side isn't generating new traffic) but immediately ready to serve
  when hart 0 wakes back up.

### Operator-driven force-park

```
Running → Parked
```

- Trigger: `bhx boot -l N --force` on a `Running` slot.
- Daemon reads the force-park metadata from the slot's status
  block (PA of hart 0's `ipi_type` + the bit value for the
  `bhx_force_park` IPI event + CLINT MSIP[0] PA).
- Writes the value to the request PA (sets the IPI-event-pending
  bit), writes `1` to MSIP[0] (delivers M-mode software interrupt).
- Hart 0 traps; OpenSBI's existing IPI dispatcher invokes the
  `bhx_force_park` event's `process` callback, which calls
  `sbi_system_reset(SHUTDOWN, 0)` — same path as a guest-issued
  SBI SRST.
- The slot reaches `Parked` typically within ~50 ms. Daemon then
  routes through the standard release-from-purgatory path.

This recovers any kernel-level wedge — Linux running with
`sstatus.SIE=0` cannot mask M-mode interrupts. The fallback for
"OpenSBI itself wedged with `mstatus.MIE=0`" is RNMI, tracked
separately in [#167](https://github.com/olofj/bhx/issues/167).

### Release from purgatory (re-boot)

```
Parked → Running
```

- Trigger: `bhx boot -l N` on a `Parked` slot.
- Daemon re-writes the kernel image into DRAM (DTB + OpenSBI keep
  their cold-boot bytes — overwriting the running OpenSBI's `.text`
  would fault the parked harts mid-poll).
- Clears the PARKED magic in the status block.
- Reads hart 0's release metadata: writes `next_addr` (kernel
  entry), `next_mode` (PRV_S = 1), `next_arg1` (DTB PA) into hart
  0's `sbi_scratch`. Reads HSM state to confirm STOPPED. Writes
  `START_PENDING` to flip the state. Writes `1` to CLINT MSIP[0]
  to fire the wake IPI.
- Hart 0 exits `sbi_hsm_hart_wait`; mainline runs
  `init_warmboot_run` → `sbi_hart_switch_mode` → `mret` to S-mode
  at the new kernel entry. The new kernel SBI-HSM-starts harts
  1..3 itself.
- Slot transitions back to `Running`.

No chip-side reset, no PCIe blip, sibling slots untouched.

**Constraints on what re-boots cleanly.** Release-from-purgatory
re-uses the cold-boot OpenSBI + DTB + initramfs bytes. The current
`dispatch_release` only re-writes the kernel image; the DTB, OpenSBI
firmware, and initramfs in DRAM are preserved as-is. So a re-boot is
clean only when the new kernel can run against the cold-boot
configuration. In practice, a release that changes any of the
following relative to the cold boot is unsupported today and may
behave subtly wrong:

- DTB content (memory layout, virtio nodes, `/chosen/bootargs`,
  console fragment, root device)
- Initramfs (or its absence) — if the cold boot used one, the new
  kernel still finds it at `rootfs_addr`, but bytes match the cold
  boot, not whatever the operator might have passed on the
  release-time `bhx boot` command line
- Memory size / `--memory` override

For workloads that need to rev these, do a cold boot (`bhx boot`
on a `Stopped` slot, or `bhx daemon stop` first) instead of a
release. A future change ([#170](https://github.com/olofj/bhx/issues/170))
either rewrites all four regions on release or rejects
configuration-changing releases up front.

### Daemon stop / shutdown

```
Running → Stopped (or Parked → Stopped)
```

- Trigger: `bhx daemon stop` (RPC) or SIGTERM to the daemon.
- For each booted slot, `internal_stop` runs:
  console-hub disconnect, capture scrollback tail (#160),
  `unregister_engine_slots`, `slot.shutdown()` (joins all per-slot
  workers, drops the L2Cpu Arc).
- The chip-side L2CPU is **not** put back in reset — its release
  bit stays set, OpenSBI keeps running (in `sbi_hsm_hart_wait` if
  the slot was Parked, or live if it was Running).
- After daemon restart, the startup probe sees the release bit set
  and reads OpenSBI's debug-descriptor magic. If valid, the slot
  is reported `Running` (or `Parked` when the daemon also reads
  the bhx-purgatory status block) — preserving the running guest
  across a daemon-only restart.
- If OpenSBI's magic is missing, the slot is reported `Wedged`.

### Wedge recovery

```
Wedged → Stopped (via cold boot)
```

- Operator runs `bhx boot -l N`. Phase 6 opportunistic reset takes
  the chip back to a clean baseline (since no other slot is
  `Running` — `Wedged` doesn't have a live chip-side state worth
  preserving), then cold-boots normally.

### Chip-side reset (manual)

```
any → Stopped (all)
```

- Trigger: `tt-smi -r` (operator), or PCIe link drop / hot-unplug
  (chip-fault SIGBUS handler exits the daemon, no graceful path).
- Chip-side state wiped wholesale. All slots become Stopped on
  next daemon start. Anything that was `Parked` or `Running` is
  gone — DRAM contents wiped by GDDR re-training.

## State-transition diagram

```
                         ┌───────────────────────────────┐
                         │             tt-smi -r         │
                         │     (any state → Stopped)     │
                         └────────────┬──────────────────┘
                                      │
                                      ▼
        ┌────────────┐  bhx boot   ┌──────────┐   guest poweroff   ┌──────────┐
        │  Stopped   │────────────►│ Running  │───────────────────►│  Parked  │
        │  /Wedged   │ (Phase 6    │          │   (SBI SRST)       │          │
        │            │  optional   │          │                    │          │
        │            │  reset)     │          │     bhx boot -l N   │          │
        │            │             │          │     --force         │          │
        │            │             │          │     (M-mode IPI)    │          │
        │            │             │          │ ◄──────────────────┤          │
        │            │             │          │                    │          │
        │            │             │          │                    │          │
        │            │             │          │  bhx boot -l N     │          │
        │            │             │          │  (release-         │          │
        │            │             │          │   from-purgatory)  │          │
        │            │             │          │ ◄──────────────────┤          │
        │            │             │          │                    │          │
        │            │             │          │   daemon stop      │          │
        │            │ ◄───────────┤          ├───────────────────►│          │
        │            │ daemon stop │          │                    │          │
        └────────────┘             └──────────┘                    └──────────┘
                ▲                         ▲                              │
                │                         │                              │
                │  Phase 6 opportunistic  │                              │
                │  reset_board (siblings  │ daemon restart sees          │
                │  only, when target is   │ release bit + OSBIdbg magic  │
                │  Stopped)               │ + (optional) PARKED magic    │
                └─────────────────────────┴──────────────────────────────┘
```

## Things that are NOT states

- **`force_reset_pcie`** is gone. The chip-wide reset is now
  exclusively the opportunistic Phase 6 path (silent, gated on
  no-Running-siblings) plus the explicit
  `bhx debug reset-x280 -l N` / `tt-smi -r` low-level escape hatches.
- **In-guest reboot** (SBI SRST_COLD_REBOOT vs SHUTDOWN) is
  currently treated identically — both land in our `final_exit`
  hook and park the harts. The daemon doesn't distinguish; the
  operator decides what to do next by issuing `bhx boot -l N`.
- **The previous `force` flag** (`bhx boot --force`) used to do
  "tear down the slot and cold-boot it" via reset_board. As of
  Phase 5, on a `Running` slot it does the M-mode-IPI force-park
  (preserving siblings); on a `Parked` slot it's identical to
  no-flag (release-from-purgatory). The flag's documented intent
  has shifted from "destructive force" to "force the transition
  through purgatory."

## Invariants worth knowing

- **The release bit (bit `idx+4` of `L2CPU_RESET`) flips 0→1 exactly
  once.** Per the documented hardware bug, asserting reset on a
  running hart is undefined and in practice takes the chip off the
  bus. Bhx never writes that bit to 0 outside of `tt-smi -r`'s
  effect.
- **`reset_board` invalidates every host-side mmap on the card.**
  Any worker thread mid-poll on a chip-side address SIGBUSes the
  daemon. Phase 6's precondition (no Running siblings) is what
  keeps this safe; `dispatch_release` and `force-park` keep workers
  alive but never call `reset_board`.
- **Workers stay alive across a Parked transition.** They idle
  (no traffic from the chip) but stand ready to serve when hart 0
  wakes. This is why `internal_stop` on a guest-poweroff doesn't
  drop the slot's `L2Cpu` — the release-from-purgatory path
  reuses the existing `L2Cpu`'s NoC mappings.
- **OpenSBI's `mmode_resv*` PMP regions stay live across the
  warmboot loop.** Mainline 1.7 doesn't set the PMP lock bit on
  the generic platform, so a future re-init could re-program them
  if needed (the bhx Phase 4b release path does NOT — it just
  drives hart 0 from STOPPED to START_PENDING via the standard
  HSM path, which doesn't touch PMP).

## Files

- States, transitions, and slot bookkeeping: `src/daemon/server.rs`
  (`dispatch_boot`, `dispatch_release`, `internal_stop`,
  `maybe_opportunistic_reset_board`, `trigger_force_park_if_available`,
  `read_parked_release_meta`).
- Per-slot resources: `src/daemon/mod.rs` (`L2CpuSlot`,
  `DaemonState`).
- Chip-side helpers: `src/shared_chip.rs` (`reset_board`,
  `reset_x280`, `halt_x280`, `idle_pll`).
- Per-L2CPU access: `src/l2cpu.rs` (`L2Cpu::new`, `read32`,
  `write32`).
- Status block layout: `src/regs.rs` (`mod purgatory`).
- OpenSBI side: `third_party/opensbi/patches/0002-bhx-purgatory-magic.patch`
  (`bhx_purgatory_final_exit`, `bhx_force_park_process`,
  `bhx_purgatory_publish_force_park_metadata`,
  `bhx_purgatory_register_reset_device`).
