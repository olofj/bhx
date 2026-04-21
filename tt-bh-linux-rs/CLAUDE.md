# tt-bh-linux-rs — notes for Claude

Rust rewrite of the C++ host tool (`../console/tt-bh-linux`) that runs
Linux on a Tenstorrent Blackhole card's on-chip SiFive X280 RISC-V
cores. This crate emulates VirtIO block/network devices and provides
the OpenSBI virtual-UART console for an L2CPU that has already been
booted. It does **not** yet do the boot sequence itself — that still
goes through `../boot.py` (luwen integration is stubbed).

## Layout

```
src/
├── main.rs           # clap CLI, signal handling, thread orchestration
├── l2cpu.rs          # L2Cpu: owns fd + 8GB VA + two 4GB TLB windows
├── tlb.rs            # TlbHandle (ioctl/mmap RAII) and TlbWindow (volatile r/w)
├── kmd.rs            # Manual FFI to tt-kmd ioctls (ALLOCATE/FREE/CONFIGURE_TLB)
├── clock.rs          # PLL stepping for L2CPU frequency changes (200/1750 MHz)
├── console.rs        # Virtual-UART circular-buffer loop + raw terminal mode
├── virtio/
│   ├── mod.rs        # run_device(): 4-phase MMIO handshake + descriptor loop
│   ├── block.rs      # VirtIO block device (mmaps a .ext4 file)
│   ├── network.rs    # VirtIO net device (backed by libvdeslirp)
│   └── interrupt.rs  # PLIC interrupt poke (mutex-protected)
├── slirp_ffi.rs      # FFI to libvdeslirp 0.1.x (only with slirp feature)
├── image.rs          # download / convert rootfs images (Debian/Ubuntu/Fedora)
├── kernel.rs         # download kernel+OpenSBI+DTB bundles
├── ramdisk.rs        # download initramfs images
└── boot.rs           # SCAFFOLDING — unfinished; needs luwen crate
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
- `tt-smi` (Python, installed by tt-installer into
  `~/.tenstorrent-venv/bin/`). Used to reset the card — see below.
- For `image pull` workflows: `wget`, `xz-utils`, `unzip`, `qemu-utils`,
  `fdisk` (`sfdisk`), `e2fsprogs` (`e2fsck`, `resize2fs`).

## Typical dev loop

This assumes the chip has already been booted via `../boot.py` (or
`make boot` in the parent repo) in a **separate terminal**. Then from
this directory:

```bash
cargo run -- connect            # console only (quiet, no-op if no rootfs.ext4)
cargo run -- connect -d rootfs.ext4     # console + virtio-block
cargo run -- connect -n                 # console + virtio-net (slirp, port 2222→22)
cargo run -- connect -d rootfs.ext4 -n  # console + disk + network
cargo run -- connect --no-console -d rootfs.ext4 -n   # headless
```

**Exit**: type `Ctrl-A x`. Hitting `Ctrl-C` also works (goes through
the SIGINT handler in `main.rs`).

**When scripting or testing**: wrap with `timeout`:

```bash
timeout 5 cargo run -- connect 2>/tmp/stderr.log </dev/null
```

Running `cargo run -- connect` without a tty that can send `Ctrl-A x`
will hang — always use `timeout` in agent loops.

Other subcommands (read-only, no hardware needed):

```bash
cargo run -- image list         # list downloadable rootfs images
cargo run -- image info debian-13
cargo run -- image pull debian  # downloads, converts, resizes to images/debian-13.ext4
cargo run -- kernel list        # list firmware bundles
cargo run -- kernel pull        # downloads fw_jump.bin, Image, blackhole-card.dtb
cargo run -- ramdisk list
```

The `boot` subcommand **does not actually boot** yet — it prints
"requires luwen crate integration" and exits. To boot from scratch use
`../boot.py` or `make boot` in the parent repo.

## Resetting the card

If the chip gets wedged (console garbled, `magic was 0` errors,
descriptor-chain warnings spinning, ioctl failures), reset it. `tt-smi`
is installed in the tt-installer venv, so run it in a subshell that
activates the venv (doesn't pollute the parent shell):

```bash
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r)
```

If `tt-smi -r` doesn't recover the card, power-cycle the host.

After resetting, you must re-run the boot sequence before reconnecting:

```bash
cd ..
make boot               # resets chip, loads OpenSBI+kernel+DTB, then runs tt-bh-linux
# or, for a manual split:
./boot.py               # just boot; exits when chip is running Linux
cd tt-bh-linux-rs
cargo run -- connect -d ../rootfs.ext4 -n
```

## Diagnostic signals

- **Console works but dies after 100ms retry spam + "eye catcher
  mismatch"**: chip was reset or never booted. Re-run `make boot`.
- **"Magic was 0, not ..."**: OpenSBI virtual UART magic gone — chip
  state lost; reset and reboot.
- **`vdeslirp_open returned NULL`** on `cargo run -- connect -n`: the
  error message from `network.rs` lists likely causes (fd limit,
  seccomp, ABI mismatch). Check `pkg-config --modversion vdeslirp
  libslirp` — we expect vdeslirp 0.1.x + libslirp 4.x.
- **descriptor-chain address-range panics in `virtio::mod::run_device`**:
  guest driver wrote a bogus descriptor, usually after a chip reset
  mid-run. Stop the tool, reset, reboot.

## Building & testing

```bash
cargo build                     # default features (includes slirp)
cargo build --no-default-features   # console+disk only, no slirp link

cargo clippy --all-targets -- -D warnings   # must stay clean
cargo test                      # 14 unit tests, all in src/main.rs (CLI parsing)
```

Tests are hardware-free — they only exercise clap and `resolve_disk_path`.
There is currently no coverage for the clock/tlb/virtio/image modules;
expanding that is a known gap (see `~/.claude/plans/logical-splashing-gosling.md`
for the planned cleanup).

## Useful parent-repo artifacts

- `../rootfs.ext4` — default disk image the tool auto-picks up.
- `../fw_jump.bin`, `../Image`, `../blackhole-card.dtb` — firmware the
  boot step loads into X280 DRAM.
- `../boot.py` — the Python boot driver (uses pyluwen); run before
  `cargo run -- connect`.
- `../Makefile` — `make boot`, `make ssh` (uses port 2222 forward),
  `make boot_cloud_init`, etc.

## Conventions / gotchas

- `L2CPU_STARTING_ADDRESS` / `L2CPU_MEMORY_SIZE` in `l2cpu.rs` encode
  that L2CPUs 0/1 have 4 GB each and 2/3 share 4 GB — don't assume
  uniform memory sizes.
- `L2Cpu::drop` order is critical: TLB windows free via ioctl (needs
  fd), then munmap the 8 GB VA, then close fd. This is enforced by
  `ManuallyDrop`.
- The `InterruptController::set_interrupt` **intentionally overwrites**
  the PLIC pending register instead of OR-ing — this preserves a
  (quirky but working) behavior from the C++ implementation. Don't
  "fix" it without understanding the timing interaction.
- `process_queue_start`/`_data`/`_complete` in `VirtioDeviceImpl` carry
  implicit state across calls (e.g. `VirtioBlk::req` is a raw pointer
  set in `_start` and dereferenced in `_data`). Don't rearrange the
  call sites in `run_device` without reviewing those invariants.
- The `connect` path's default disk logic: if `--disk` is not given
  and `./rootfs.ext4` doesn't exist, no disk thread is spawned (this
  is what makes `cargo run -- connect` quiet in a dev checkout).
