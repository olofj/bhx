# bhx

*pronounced "bix"*

**Boot Linux on the embedded RISC-V cores inside Tenstorrent Blackhole AI accelerators.**

Each Tenstorrent Blackhole P100/P150 PCIe card carries four SiFive X280
RISC-V cores ("L2CPUs") sitting alongside the AI compute fabric.
They're intended for control-plane workloads, but they're regular
RV64GC cores with their own DRAM and are capable of booting real
Linux distros end-to-end. `bhx` is the host-side tool that does the
full bring-up:

- **Cold boot**: PCIe reset, OpenSBI + kernel + DTB image load, reset-
  vector setup, prefetcher config.
- **virtio-mmio devices**: block, net (libvdeslirp), console, and rng,
  emulated from a per-card daemon backed by BRISC firmware on a
  reserved Tensix tile.
- **Stock distro support**: U-Boot S-mode payload + EFI-loader chain
  boots AlmaLinux Kitten 10, Debian generic, Ubuntu 24.04 LTS, Fedora
  Cloud Base, and similar GPT-partitioned cloud images straight from
  their published .raw / .qcow2 artifacts. Pre-extracted single-FS
  rootfs images boot via the patched direct-kernel path.
- **Operator UX**: a single `bhx` binary that runs as both the
  per-card daemon (`bhx daemon start`) and a thin RPC client
  (`bhx boot`, `bhx connect`, `bhx add-disk`, …). Console attach
  fans out across multiple clients with a 64 KiB scrollback hub.

## Prerequisites

- An x86-64 Linux host with a Tenstorrent Blackhole card (P100 / P150)
  and the [tt-kmd](https://github.com/tenstorrent/tt-kmd) kernel module
  loaded — `ls /dev/tenstorrent/` must show at least `0`.
- [tt-installer](https://github.com/tenstorrent/tt-installer) for
  `tt-smi` (used to reset the card). On a default install it lives at
  `~/.tenstorrent-venv/bin/`.
- Rust stable (any recent version).
- System packages:
  - `libfdt-dev` (always — for DTB patching).
  - `libvdeslirp-dev` / `libslirp-dev` (if you want guest networking —
    on by default via the `slirp` Cargo feature; disable with
    `--no-default-features` if you don't need net).
  - For downloading pre-built rootfs images: `wget`, `xz-utils`,
    `unzip`, `qemu-utils`, `fdisk`, `e2fsprogs`.

Build the tool:

```bash
cd bhx
cargo build --release       # or plain `cargo build` for a dev build
```

CI runs the full build/clippy/test gauntlet on every push that touches
`bhx/`; see `.github/workflows/rust-ci.yml` (default and
`--no-default-features` builds, plus `cargo fmt --check`).

## Fetching the firmware + a rootfs

For the U-Boot/EFI/GRUB path (any modern distro), all you need is the
disk image:

```bash
cargo run -- image pull debian        # or fedora, ubuntu, almalinux, …
```

Run `cargo run -- image list` to see the registry. The daemon resolves
`u-boot.bin`, `fw_jump.bin`, and `blackhole-card.dtb` from the in-tree
`third_party/{uboot,opensbi,dtb}/` build trees automatically — see
their per-directory READMEs for `make`-based bumps and reproducibility
notes.

For the legacy direct-kernel boot path (`boot --kernel <path>`), you
also need to provide `Image` yourself — most often a kernel you've
just built from a checked-out Linux tree. The host symlink convention
is `./Image` in the project root; pass `--kernel <path>` on `boot` to
point elsewhere.

## Quick start

Boot one L2CPU with disk and network, then connect a terminal to it:

```bash
# Start the per-card daemon (once per boot of the host). The log file
# is pinned to this directory with O_DSYNC so every line is on disk
# before write() returns — handy if the host ever crashes.
./target/debug/bhx daemon start -t 0 --log-file ./daemon-card0.log

# Boot L2CPU 0 with the rootfs in this directory and slirp networking.
./target/debug/bhx boot -l 0 -d rootfs.ext4 -n

# Attach a terminal. Ctrl-A x to detach.
./target/debug/bhx connect -l 0
```

Log in as `debian` (no password). The `-n` flag enables slirp
networking with TCP port 2222 on the host forwarded to port 22 inside
the guest, so you can also `ssh -p 2222 debian@localhost`.

Check what's running:

```bash
./target/debug/bhx daemon status -t 0
# daemon: running (card 0, pid ..., uptime Ns, sock /run/user/.../sock)
#   l2cpu 0: Running disk=/.../rootfs.ext4 net=y clients=0
#   l2cpu 1: Stopped disk=- net=- clients=0
#   l2cpu 2: Stopped disk=- net=- clients=0
#   l2cpu 3: Stopped disk=- net=- clients=0
```

When you're done:

```bash
./target/debug/bhx daemon stop -t 0
```

## Booting stock distro images via U-Boot

`boot --kernel <Image>` (the default with no `--uboot` flag) loads the
host-provided `Image` into L2CPU DRAM and jumps OpenSBI straight at
it. That works for the patched buildroot kernel and for
pipeline-converted single-FS rootfs images, but stock distro cloud
images (Debian generic, AlmaLinux Kitten, Ubuntu preinstalled-server,
Fedora Cloud Base) ship as multi-partition disks with an EFI System
Partition + grub-riscv64-efi + a kernel they install themselves —
the host's `Image` is the wrong kernel to jump to.

For those, run U-Boot as the S-mode payload and let it walk the disk:

```bash
# One-time: build U-Boot from source.
cd third_party/uboot && make check-deps && make    # ~5 min cold; idempotent
cd ../..

# Pull a U-Boot-bootable cloud image. The pull pipeline lands a
# whole-disk `.img` (with GPT + ESP intact) when the known image
# entry has `needs_bootloader: true`:
./target/debug/bhx image pull almalinux

# Boot it. With no `--kernel` and no `--uboot`, the boot subcommand
# detects from the disk's basename that this image needs U-Boot and
# auto-defaults to `--uboot third_party/uboot/u-boot.bin`:
./target/debug/bhx boot -l 0 -d images/almalinux-10-kitten.img -n

# Or be explicit:
./target/debug/bhx boot -l 0 \
    --uboot third_party/uboot/u-boot.bin \
    -d images/almalinux-10-kitten.img -n
```

OpenSBI hands control to U-Boot, U-Boot reads the GPT, finds the
ESP, runs `EFI/<distro>/shimriscv64.efi`, shim chainloads grub, grub
loads the kernel + initrd from `/boot`. End-to-end UEFI on RISC-V.

`cargo run -- image list` annotates each known image's boot path —
`whole partitioned disk` images go through U-Boot, `single-FS .ext4`
images go through `--kernel`.

The U-Boot build is documented in `third_party/uboot/README.md`: pinned config,
the three downstream patches we apply (closes #49 plus two RISC-V
DRAM-init fixes), reproducibility workflow.

## Common operations

Once an L2CPU is booted, you can reconfigure its devices without
rebooting the guest:

```bash
# Swap the disk image (the guest sees a short unmount/remount):
./target/debug/bhx remove-disk -l 0
./target/debug/bhx add-disk    -l 0 some-other-rootfs.ext4

# Attach/detach networking:
./target/debug/bhx remove-net  -l 0
./target/debug/bhx add-net     -l 0

# Re-image a running core in place (tears down workers first):
./target/debug/bhx boot -l 0 -d rootfs.ext4 -n --force
```

Run all four L2CPUs at once — each wants its own rootfs to avoid
ext4 corruption from concurrent writers:

```bash
for i in 0 1 2 3; do
    cp --reflink=auto rootfs.ext4 rootfs-$i.ext4
    ./target/debug/bhx boot -l $i -d rootfs-$i.ext4 -n
done
```

## Scripting tips

Wrap `connect` with `timeout` when running non-interactively — it runs
forever and only exits on Ctrl-A x:

```bash
timeout 5 ./target/debug/bhx connect -l 0 </dev/null 2>/tmp/stderr.log
```

For non-interactive log scraping, the daemon's log file (what you
passed to `--log-file`) is `O_DSYNC` and contains boot-path events.
The `daemon logs` subcommand tails it for you.

## Troubleshooting

- **Chip wedged** (console garbled, `magic was 0`, descriptor-chain
  panics): reset the card, then start over.

  ```bash
  (. ~/.tenstorrent-venv/bin/activate && tt-smi -r)
  ```

  After a reset, either `daemon stop && daemon start` (warm-resume
  picks up any core that survived) or re-boot each affected L2CPU with
  `--force`.

- **`daemon status` shows `Wedged`** for a core: startup probe found
  the core released but its OpenSBI debug descriptor is missing. Re-
  boot with `--force`.

- **`daemon start` reports "already running"**: pidfile/flock is held.
  If you're sure no other daemon is running,
  `rm /run/user/$UID/bhx/0/pid` and try again.

- **`vdeslirp_open returned NULL`** on `-n`: check `pkg-config
  --modversion vdeslirp libslirp`. Expected: vdeslirp 0.1.x + libslirp
  4.x. The error from `network.rs` also lists likely causes (fd
  limits, seccomp, ABI mismatch).

## Diagnostics bypassing the daemon

For poking the chip directly (requires the daemon stopped for this
card):

```bash
./target/debug/bhx debug read-reset-reg
./target/debug/bhx debug reset-x280      -l 0
./target/debug/bhx debug assert-reset    -l 0
./target/debug/bhx debug deassert-reset  -l 0
```

## Going deeper

- **Architecture, design notes, reference docs**: [`docs/`](docs/) —
  see [`docs/README.md`](docs/README.md) for the index. Covers the
  Tensix-engine virtio architecture, Blackhole harvest-mask reading,
  tt-metal coexistence, telemetry / metrics, and the sandboxing
  syscall set.
- **Per-module map** (one-line summary of every file in `src/`):
  [`CLAUDE.md`](CLAUDE.md). Originally written for AI assistants but
  it's the most thorough developer-onboarding doc in the tree.
- **Hardware soak scripts**: [`scripts/README.md`](scripts/README.md).
  Includes a 4-way concurrent console I/O roundtrip test.
- **Open design issues + roadmap**: the GitHub issue tracker at
  <https://github.com/olofj/bhx/issues>.

