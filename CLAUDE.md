# bhx — notes for Claude

## Working style

You are a highly ambitious principal engineer with a keen sense for clean,
maintainable code and no tolerance for needless abstractions. You are
high-agency and driven: when a task is done, look for the next real piece
of work — a loose end, a TODO, a test gap, a rough edge — and keep going.
Don't propose to "finish for the day," "wrap up here," or "stop and review
later" as a default. Push through to a genuinely good stopping point:
everything you touched compiles cleanly, `cargo fmt --check` is clean,
`cargo clippy --all-targets -- -D warnings` is clean, tests pass, and
no half-finished work is left behind.

**Before every commit**, run all three:

```bash
cargo fmt                              # apply formatting (or --check to verify)
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs all three as separate gates. `cargo fmt --check` failures will
block the push and require an immediate follow-up commit. Skipping
`cargo fmt` locally has been the most common cause of red CI on this
project — don't skip it.

As of M6.9 (#71) the legacy host-buffer #64 path is gone — there is
exactly one virtio control plane: BRISC firmware on a Tensix tile
serving all four L2CPUs through `process_one_chain_for_queue` in
`src/virtio/mod.rs`, dispatched from the kick poller in
`src/tensix_data_plane.rs`. The only remaining feature flag is
`slirp` (libvdeslirp/libslirp link for virtio-net).

Ambition does not mean scope creep. Stay ruthless about simplicity:
- Don't invent abstractions for hypothetical future needs.
- Don't add code the task doesn't require.
- Prefer deleting code over adding it; prefer editing an existing file
  over creating a new one.
- Fix root causes, not symptoms.

When you spot unrelated rough edges mid-task (dead code, a stale
comment, a missing test, a refactor that would be nice), do **not**
silently fold them into the current change. Instead, finish the
requested task and then surface the observation to the user as a short
list of "things I noticed, worth a separate issue?" Let the user decide
whether to act on any of them now, file an issue, or ignore. If the
user says yes, do it as its own piece of work.

Only stop when the requested work is actually done, the user tells you
to stop, or you genuinely need input to proceed. Within the scope of
the requested task, make trade-off calls yourself; only escalate
decisions that change a public API, require a migration, or affect
cross-L2CPU runtime invariants. When you do need input, ask a specific
question rather than waiting passively.

Commit each logical unit of work as you finish it — this overrides the
default "ask before committing" behavior on this project. Don't batch a
multi-item request into one giant commit and don't pause to summarize
between items; the commits are the record.

Rust host tool that runs Linux on a Tenstorrent Blackhole card's
on-chip SiFive X280 RISC-V cores (the "L2CPUs"). This crate does the
full stack end-to-end: boots the
L2CPU (reset, OpenSBI + kernel + DTB image load, DTB patching, reset
vectors, prefetchers), then emulates VirtIO block/network devices and
serves the OpenSBI virtual-UART console.

The tool runs as a **per-card daemon** (`bhx daemon start`) that
owns the card's resources; `boot`, `connect`, `add-disk` etc. are thin
RPC clients. The old in-process `connect` path has been removed.

## Layout

```
src/
├── main.rs           # clap CLI; dispatches to daemon/runner.rs (lifetime) + daemon/client.rs (RPCs)
├── l2cpu.rs          # L2Cpu: per-L2CPU fd + 8GB VA + two persistent 4GB TLB windows + alloc_lock
├── tlb.rs            # TlbHandle (ioctl/mmap RAII) and TlbWindow (volatile r/w)
├── kmd.rs            # Manual FFI to tt-kmd ioctls (ALLOCATE/FREE/CONFIGURE_TLB, RESET_DEVICE, ...)
├── chip.rs           # Standalone PCIe LDS reset (reset_board); called by SharedChip during reset_board
├── shared_chip.rs    # Daemon-owned AXI tile (8,0) access: persistent TLB + seq_lock for PLL/reset R-M-W
├── fdt_ffi.rs        # Manual FFI to libfdt for DTB patching
├── clock.rs          # PLL stepping (200/1750 MHz) via SharedChip's PllAccess impl
├── boot.rs           # boot_l2cpu, modify_dtb, configure_prefetchers (all via Arc<L2Cpu>)
├── console.rs        # TerminalRawMode RAII (tcgetattr/tcsetattr guard) used by daemon/terminal
├── virtio/
│   ├── mod.rs        # run_device(): cold-start handshake (Phase 1-3) + warm-restart stash + desc loop
│   ├── block.rs      # VirtIO block device (mmaps .ext4)
│   ├── network.rs    # VirtIO net device (libvdeslirp)
│   └── interrupt.rs  # PLIC interrupt poke (mutex-protected; intentionally overwrites, see gotchas)
├── daemon/
│   ├── mod.rs        # DaemonState, L2CpuSlot, WorkerHandle; holds Arc<SharedChip>
│   ├── server.rs     # Accept loop + dispatch_{boot,status,attach_console,add/remove_disk/net,stop,shutdown}
│   ├── client.rs     # Thin RPC helpers used by main.rs
│   ├── runner.rs     # daemon start/stop/restart/status/logs — drives daemon::fork
│   ├── fork.rs       # POSIX double-fork + setsid + stdio redirect (replaces daemonize crate)
│   ├── lifetime.rs   # pidfile + flock + runtime dir ($XDG_RUNTIME_DIR/bhx/<card>)
│   ├── protocol.rs   # Request/Response, length-prefixed JSON framing, SCM_RIGHTS fd passing
│   ├── console_hub.rs# 64 KiB scrollback + writer election (Ro/Rw/Takeover)
│   ├── chip_console.rs # Daemon's chip-side UART pump; probe_warm_resume + pure decode helpers
│   ├── log.rs        # dlog! macro with O_DSYNC log file (survives host crashes)
│   └── terminal.rs   # Client-side tty pump for `connect` (Ctrl-A x exit detection)
├── slirp_ffi.rs      # FFI to libvdeslirp 0.1.x (only with slirp feature)
├── image.rs          # download / convert rootfs images (Debian/Ubuntu/Fedora) — hardware-free
└── ramdisk.rs        # download initramfs images — hardware-free

scripts/  (see scripts/README.md)

brisc-firmware/        # BRISC + TRISC0 firmware for the Tensix virtio engine
├── start.S            # multi-core entry; hart-ID dispatch via reset-PC override
├── virtio.c           # register-file emulation + UART poll + kick ring + handshake
├── hello.c            # minimal heartbeat-only firmware (#67 M1 smoke)
├── include/           # virtio_layout.h, uart_layout.h, tensix_proto.h shared with Rust
├── prebuilt/          # checked-in *.bin fallback when sfpi toolchain absent
└── Makefile           # toolchain at /opt/tenstorrent/sfpi/compiler/bin

third_party/           # vendored, builds-from-source dependencies
├── uboot/             # U-Boot S-mode payload for booting stock distro images (#44 + sub-issues)
│   ├── README.md      # build / pinned config / bumping / reproducibility
│   ├── Makefile       # download + extract + patch + merge_config.sh + build
│   ├── bhx.config     # defconfig fragment merged on top of qemu-riscv64_smode_defconfig
│   ├── patches/       # 3 downstream patches (sel_generation handshake + 2 RISC-V DRAM fixes)
│   ├── sha256sums     # pinned tarball checksum
│   └── u-boot.bin     # (gitignored) symlink the build maintains
├── opensbi/           # OpenSBI M-mode payload — produces fw_jump.bin
│   ├── README.md      # build / pinned version / bumping / reproducibility
│   ├── Makefile       # download + verify + build (PLATFORM=generic, FW_JUMP=y)
│   ├── sha256sums     # pinned tarball checksum
│   └── fw_jump.bin    # (gitignored) symlink the build maintains
├── dtb/               # Blackhole device tree, vendored from tenstorrent/linux
│   ├── README.md      # provenance (upstream commit SHA, license, bumping)
│   ├── Makefile       # cpp + dtc -> blackhole-card.dtb
│   ├── blackhole-card.dts   # vendored DTS source (Copyright 2025 Tenstorrent AI ULC)
│   ├── blackhole.dtsi       # vendored DTSI source (Copyright 2025 Tenstorrent AI ULC)
│   └── blackhole-card.dtb   # (gitignored) build output
└── buildroot/         # Buildroot test rootfs (auto-login, fio/iperf3, used by soaks)
    ├── README.md      # build / pinned version / overlay / reproducibility
    ├── Makefile       # download + extract + build
    ├── buildroot.config     # defconfig fragment
    ├── overlay/             # /etc/inittab and other fixed-up files
    ├── post-build.sh        # final rootfs touch-ups
    ├── echo-byte.c          # tiny test helper baked into target/bin
    ├── sha256sums           # pinned tarball checksum
    └── rootfs.ext4    # (gitignored) symlink to output/images/rootfs.ext4
```

The `slirp` feature is on by default and links `libvdeslirp`+`libslirp`;
disable with `--no-default-features` if you just need console+disk.

## Dependencies (runtime)

- `/dev/tenstorrent/<idx>` — provided by `tt-kmd` (Tenstorrent kernel
  module). `ls /dev/tenstorrent/` must show at least `0`.
- `libvdeslirp` / `libslirp` at link time (only with the `slirp`
  feature). See `build.rs` — a `cargo build` compiles a tiny C probe to
  assert `sizeof(SlirpConfig) <= 512`; if that ever fails, bump
  `_data: [u8; 512]` in `slirp_ffi.rs`.
- `libfdt` at link time (always; see `build.rs`). `modify_dtb` uses it
  to patch `/memory`, `/reserved-memory`, and the four `virtio,mmio`
  nodes at boot time.
- `tt-smi` (Python, installed by tt-installer into
  `~/.tenstorrent-venv/bin/`). Used to reset the card — see below.

## Typical dev loop

```bash
cargo build

# Start the daemon once per card. Log pinned to project dir with O_DSYNC
# so every line hits disk before the write() returns.
./target/debug/bhx daemon start -t 0 --log-file ./daemon-card0.log

# Boot one L2CPU with its rootfs + net. Defaults: rootfs.ext4 in cwd,
# fw_jump.bin / Image / blackhole-card.dtb in cwd.
./target/debug/bhx boot -l 0 -d rootfs.ext4 -n

# Attach an interactive console (Ctrl-A x to detach).
./target/debug/bhx connect -l 0

# Swap the disk or net without rebooting the guest:
./target/debug/bhx remove-disk -l 0
./target/debug/bhx add-disk -l 0 other-rootfs.ext4

# Check state:
./target/debug/bhx daemon status -t 0

# Shut down everything on this card:
./target/debug/bhx daemon stop -t 0
```

`connect` is a thin RPC client: the daemon owns the chip-side UART pump
and a 64 KiB scrollback hub, and the client receives a socketpair fd via
`SCM_RIGHTS`. Multiple `connect`s fan out through the hub — default is
`Ro`; `Rw` / `Takeover` available via `daemon/protocol.rs::ConsoleMode`
(not yet exposed on the CLI).

**Scripting**: if the agent can't send Ctrl-A x, always wrap `connect`
with `timeout`:

```bash
timeout 5 ./target/debug/bhx connect -l 0 </dev/null 2>/tmp/stderr.log
```

Hardware-free: `cargo run -- image|ramdisk` subcommands (no daemon, no
card needed). `image pull <distro>` downloads + converts disk images;
`fw_jump.bin` / `blackhole-card.dtb` / `u-boot.bin` come from the
in-tree `third_party/{opensbi,dtb,uboot}/` build trees.

Low-level diagnostics that bypass the daemon (require the daemon
stopped — enforced at the client):

```bash
cargo run -- debug read-reset-reg
cargo run -- debug reset-x280 -l N     # PLL step + OR-in bit idx+4 (safe on live cores)
cargo run -- debug assert-reset -l N   # clear bit idx+4 (safe on live cores per empirical test)
cargo run -- debug deassert-reset -l N
```

## Resetting the card

If the chip wedges (console garbled, `magic was 0` errors,
descriptor-chain panics spinning, ioctl failures), reset it:

```bash
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r)
```

The daemon's slots are stale after `tt-smi -r` — either stop+start
(startup probe picks up what survives via warm-resume) or re-image with
`--force`. Full reflow: `daemon stop` → `tt-smi -r` → `daemon start` →
`boot --force`.

If `tt-smi -r` doesn't recover the card, power-cycle the host.

## Diagnostic signals

- **"eye catcher mismatch" / "Magic was 0"**: chip-side state lost. Tear
  down the slot (`daemon stop <...>` or re-boot with `--force`) and
  re-image.
- **`daemon status` shows `Wedged`**: startup probe found the core
  released (bit idx+4=1) but its OpenSBI debug descriptor magic is
  wrong. Re-boot with `--force`.
- **`vdeslirp_open returned NULL`**: `network.rs` lists the likely
  causes (fd limit, seccomp, ABI mismatch). Verify
  `pkg-config --modversion vdeslirp libslirp` shows 0.1.x + 4.x.
- **Descriptor-chain panics in `virtio::mod::run_device`**: bogus
  descriptor from a guest that got torn mid-run. Reset and reboot.
- **Daemon died / host crashed under load**: check
  `./daemon-card0.log` — it's `O_DSYNC`, so the last line reflects the
  last thing that actually hit disk. The historical 4-way concurrent
  cold boot hazard is fixed (see issue #1); if a fresh crash lands on
  the boot path, check `SharedChip::seq_lock` holds + `L2Cpu`'s own
  `alloc_lock` holds before assuming it's a concurrency regression.
- **Daemon log ends with `fatal: SIGBUS …` or `fatal: SIGSEGV …`**: the
  per-card chip-fault handler caught an external invalidation
  (`tt-smi -r`, PCIe link drop, hot-unplug, etc.) — not a daemon bug.
  Daemon exits 134/139 (`128 + signum`) so any supervisor sees a
  distinct kill code. Restart with `daemon start` once the card is
  back. Foreground (`--foreground`) mode does not install the handler
  on purpose, so a foreground-run daemon prints the panic / fault
  context to the operator's terminal.

## Building & testing

```bash
cargo build                          # default features (slirp)
cargo build --no-default-features    # no slirp link

cargo fmt --check                    # CI gate; run `cargo fmt` to fix
cargo clippy --all-targets -- -D warnings   # must stay clean
cargo test                           # hardware-free unit tests
```

CI runs the same three as separate steps in this exact order. Run all
three locally before any commit; a `cargo fmt` slip is the easiest way
to red-light the pipeline.

Unit tests cover: CLI parsing + `absolutize`, daemon `protocol`
round-trips + SCM_RIGHTS, `console_hub` fan-out + writer election,
`lifetime` pidfile / XDG runtime dir, `DaemonState` initial + wedged
flag lifecycle, `chip_console::probe_warm_resume` byte decode +
`DebugDescriptor` layout invariants. All hardware-free.

Hardware-gated soak scripts under `scripts/` (see `scripts/README.md`).
Remaining coverage gaps: `dispatch_boot --force` state machine,
`dispatch_add_disk` stuck-slot path, clock/tlb/virtio core handshake,
image/kernel/ramdisk downloaders.

## Conventions / gotchas

- Never put GitHub issue numbers (`#NNN`) in user-visible strings —
  CLI help text (clap `///` doc comments on enum variants and struct
  fields), `eprintln!` / `println!` output, error messages, log lines
  the operator reads. Issue numbers are project-management metadata;
  they're noise to a user reading `--help`. Internal Rust comments
  (regular `//` source comments, `///` docs on non-CLI items) keep
  the references for future-debug context. Quick check before
  committing CLI changes:
  ```bash
  ./target/debug/bhx --help | grep -E '#[0-9]+'   # should be empty
  ./target/debug/bhx <subcommand> --help | grep -E '#[0-9]+'
  ```
- When filing GitHub issues with `gh issue create --body "$(cat <<'EOF' ... EOF)"`,
  do NOT pre-escape backticks, `"`, or `$`. The single-quoted heredoc
  passes everything through verbatim — backslashes survive into the
  body and GitHub renders them literally, breaking inline code spans
  (`` \` `` shows up as `\` followed by a backtick instead of opening a
  code span). Write markdown as-is: `` `bhx boot` ``, `"foo"`, `$VAR`.
  Reflexive shell-style escaping is wrong here; the heredoc is its
  own quoting. Same applies to `gh issue comment` and `gh pr create`
  bodies.
- Worker poll loops (`virtio::run_device`, `chip_console::uart_pass`)
  use a three-tier adaptive sleep: FAST (1 µs / 100 µs) while there's
  observable activity; SLOW (1 ms) after `FAST_WINDOW=200ms` quiet;
  IDLE (10 ms) after `IDLE_WINDOW=2s` quiet. Tier-3 dropped idle
  daemon CPU from ~6% to <2%. Don't shrink the IDLE_SLEEP without
  re-measuring the chip TX ring fill rate (4 KiB; bursty kernel
  printk fills it in <50 ms — 10 ms IDLE poll keeps a comfortable
  margin). See `scripts/profile_daemon.sh` for the harness that
  produced the original numbers.
- `L2CPU_STARTING_ADDRESS` / `L2CPU_MEMORY_SIZE` in `l2cpu.rs` encode
  that L2CPUs 0/1 have 4 GB each and 2/3 share 4 GB — don't assume
  uniform memory sizes. `boot::modify_dtb` patches
  `/memory@400030000000` per-L2CPU with the actual size so guest
  kernels on L2CPU 2/3 don't over-allocate.
- `L2Cpu::drop` order is critical: TLB windows free via ioctl (needs
  fd), then munmap the 8 GB VA, then close fd. Enforced by
  `ManuallyDrop`.
- `L2Cpu` implements `Sync` via an internal `alloc_lock: Mutex<()>`
  guarding the ioctl path. Shared across daemon workers as
  `Arc<L2Cpu>`. Persistent windows are set up at `new()` and never
  remapped — `write32` / `read32` / `get_persistent_2m_window` lock the
  mutex only for allocation, not the subsequent volatile ops.
- **Chip-wide AXI access goes through exactly one place**: the daemon's
  `SharedChip` (`src/shared_chip.rs`), holding a single persistent 2 MiB
  TLB window to NOC tile (8,0). `SharedChip::seq_lock` serializes any
  multi-step register sequence (PLL step + `L2CPU_RESET` R-M-W in
  `reset_x280`, fd-drop + PCIe reset + fd-reopen in `reset_board`).
  Do NOT create another mapping to tile (8,0) — concurrent accessors
  aliasing the PLL or reset registers caused host crashes on 4-way cold
  boots before the `SharedChip` refactor (see issue #1). Per-L2CPU NOC
  traffic (DRAM image load, L3 / L2 prefetch, reset vectors) goes
  through each core's own `L2Cpu` fd + TLB windows, which is fine
  because those regions are disjoint across cores.
- `InterruptController::set_interrupt` **intentionally overwrites** the
  PLIC pending register instead of OR-ing — preserves a quirky but
  working behavior from the C++ implementation. Don't "fix" without
  understanding the timing interaction.
- `process_queue_start` / `_data` / `_complete` in `VirtioDeviceImpl`
  carry implicit state across calls (e.g. `VirtioBlk::req` is a raw
  pointer set in `_start` and dereferenced in `_data`). Don't rearrange
  the call sites in `run_device` without reviewing those invariants.
- All `sel_generation` bumps in `virtio/mod.rs` use `wrapping_add(1)`.
  The MMIO counter wraps natively on u32; plain `+ 1` panics in debug
  builds when the guest reaches `u32::MAX` legitimately *or* when garbage
  from a concurrent-write race lands in the read.
- Daemon chdir's to `/` in the grand-child of `daemon::fork::double_fork`, so relative paths in client
  RPCs won't resolve on the daemon side. The CLI `absolutize`s paths
  before sending — see `main.rs::absolutize` and its tests. `add-disk`
  also has a server-side pre-open check so a bad path fails the RPC
  without leaving a dead worker in the slot.
- The `boot` subcommand's default disk logic: if `--disk` is not given
  and `./rootfs.ext4` doesn't exist in the *client's* cwd, no disk is
  attached. Guest will VFS-mount-panic with `root=/dev/vda`. Pass
  `--initramfs` or an explicit `--disk` to avoid.
- **Two boot modes** (boot.rs `BootDevice` + protocol.rs `BootPayload`):
  `Kernel(<path>)` jumps OpenSBI straight at a raw `Image` (the legacy
  default; the daemon also preloads initramfs and patches
  `/chosen/bootargs` with `root=/dev/<device>`). `Uboot(<path>)` loads
  `u-boot.bin` at the kernel offset instead and skips both initramfs
  preload and bootargs injection — U-Boot reads the disk at runtime,
  finds the ESP, runs the EFI shim+grub chain, loads the actual kernel
  itself. Each known image entry's `needs_bootloader` field decides
  which mode the no-`--uboot` boot path defaults to (see
  `default_boot_payload` in main.rs). `default_uboot_path` prefers
  `./u-boot.bin` (operator symlink) over `./third_party/uboot/u-boot.bin`
  (in-tree build); see `third_party/uboot/README.md` for the build.
