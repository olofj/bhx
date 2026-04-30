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
  boots Debian generic, Ubuntu 24.04 LTS, Fedora Server / Cloud Base,
  AlmaLinux Kitten 10, and similar GPT-partitioned cloud images
  straight from their published `.raw` / `.qcow2` artifacts.
  Pre-extracted single-FS rootfs images boot via the patched
  direct-kernel path.
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
  - For downloading pre-built rootfs images: `xz-utils`,
    `unzip`, `qemu-utils`, `fdisk`, `e2fsprogs`. (HTTP downloads
    themselves are native via the `ureq` crate.)

Build and install the `bhx` binary into `~/.cargo/bin/` (make sure
that's on your `PATH`):

```bash
cargo install --path .
```

Then a one-time U-Boot build (needed for any whole-disk distro image —
see [Images](#images) below for which images need it):

```bash
make -C third_party/uboot check-deps
make -C third_party/uboot           # ~5 min cold; idempotent
```

Run `bhx` commands from the project root — default firmware paths
(`fw_jump.bin`, `blackhole-card.dtb`, `third_party/uboot/u-boot.bin`,
`Image`) are resolved relative to your CWD. The `images/` directory
that `bhx image pull` writes into is the same way.

## Quick start

Three commands from a freshly built `bhx` to a Debian login prompt on
L2CPU 0:

```bash
bhx image pull debian-13
bhx daemon start
bhx boot -l 0 -d images/debian-13.img -n -a
```

What each step does:

1. `image pull debian-13` downloads the official Debian 13 (Trixie)
   riscv64 cloud image (~700 MB compressed → ~10 GB resized) and
   lands it at `images/debian-13.img`. Idempotent — re-running with
   the artifact present is a no-op (pass `--refetch` to force).
2. `daemon start` forks the per-card daemon. Owns the chip;
   everything else is RPC.
3. `boot -l 0 -d images/debian-13.img -n -a`:
   - `-l 0` selects L2CPU 0.
   - `-d` points at the disk; the daemon notices it's a whole-disk
     image and auto-selects the U-Boot + EFI boot path
     (`third_party/uboot/u-boot.bin`).
   - `-n` enables slirp networking — TCP 2222 on the host forwards to
     port 22 in the guest, so `ssh -p 2222 debian@localhost` works.
   - `-a` (`--attach`) drops you straight into the console after boot
     so you see the OpenSBI banner, U-Boot, GRUB, and kernel printk
     live. `Ctrl-A x` to detach.

First boot of a cloud image runs cloud-init through to a login
prompt in ~30 s. Default credentials live in `bhx image info <name>`
(for `debian-13`: user `debian`, password `debian`).

When you're done:

```bash
bhx daemon stop
```

## Images

The image registry is queryable at runtime:

```bash
bhx image list                  # available images
bhx image info <name>           # URL, layout, default user/pass
bhx image pull <name>           # download + prepare
```

Currently shipped:

| Name                  | Layout                  | Boot path                   | Notes                                  |
| --------------------- | ----------------------- | --------------------------- | -------------------------------------- |
| `tt-debian`           | single-FS `.ext4`       | direct kernel (host `Image`)| Tenstorrent pre-built; needs a kernel  |
| `debian-13`           | whole partitioned disk  | U-Boot + EFI                | Default; alias `debian`/`trixie`       |
| `ubuntu-24.04`        | whole partitioned disk  | U-Boot + EFI                | Alias `ubuntu`/`noble`                 |
| `fedora-42`           | whole partitioned disk  | U-Boot + EFI                | Server Host Generic                    |
| `fedora-42-cloud`     | whole partitioned disk  | U-Boot + EFI                | Cloud Base; needs cloud-init           |
| `almalinux-10-kitten` | whole partitioned disk  | U-Boot + EFI                | Alias `alma`/`kitten`                  |

**Single-FS** images are bare ext4 filesystems. The host loads `Image`
(a raw Linux kernel) + initrd + DTB, OpenSBI jumps straight at the
kernel, and the kernel mounts `/dev/vda` as root. Convenient when
you've built your own kernel; you have to provide `Image` yourself
(`--kernel <path>`, defaults to `./Image`).

**Whole-disk** images carry a GPT partition table with an EFI System
Partition and a kernel installed in `/boot`. The daemon loads U-Boot
at the kernel offset, U-Boot reads the GPT, runs the EFI shim, shim
chainloads GRUB, GRUB loads the in-disk kernel + initrd. End-to-end
UEFI on RISC-V; nothing kernel-specific on the host side.

The daemon resolves `u-boot.bin`, `fw_jump.bin`, and
`blackhole-card.dtb` from the in-tree `third_party/{uboot,opensbi,dtb}/`
build trees automatically — see their per-directory READMEs for
`make`-based bumps and reproducibility notes. The U-Boot build is
documented in [`third_party/uboot/README.md`](third_party/uboot/README.md):
pinned config, the three downstream patches we apply, reproducibility
workflow.

## Common operations

Once an L2CPU is booted, you can reconfigure its devices without
rebooting the guest:

```bash
# Swap the disk image (the guest sees a short unmount/remount):
bhx remove-disk -l 0
bhx add-disk    -l 0 some-other-image.img

# Attach/detach networking:
bhx remove-net  -l 0
bhx add-net     -l 0

# Re-image a running core in place (tears down workers first):
bhx boot -l 0 -d images/debian-13.img -n --force
```

Run all four L2CPUs at once — each wants its own disk to avoid
filesystem corruption from concurrent writers:

```bash
for i in 0 1 2 3; do
    cp --reflink=auto images/debian-13.img images/debian-13-l$i.img
    bhx boot -l $i -d images/debian-13-l$i.img -n
done
```

Check what's running:

```bash
bhx daemon status
# daemon: running (card 0, pid ..., uptime Ns, sock /run/user/.../sock)
#   l2cpu 0: Running disk=/.../debian-13.img net=y clients=0
#   l2cpu 1: Stopped disk=- net=- clients=0
#   l2cpu 2: Stopped disk=- net=- clients=0
#   l2cpu 3: Stopped disk=- net=- clients=0
```

Reattach a console to a running core:

```bash
bhx connect -l 0       # Ctrl-A x to detach
```

Multiple `connect` clients fan out through the daemon's 64 KiB
scrollback hub (default `Rw`; `Ro` / `Takeover` modes available via
`--mode`).

## Scripting tips

Wrap `connect` with `timeout` when running non-interactively — it runs
forever and only exits on Ctrl-A x:

```bash
timeout 5 bhx connect -l 0 </dev/null 2>/tmp/stderr.log
```

For non-interactive log scraping, the daemon's log file (default
`./bhx-daemon-card<idx>.log`, override with `--log-file`) is `O_DSYNC`
and contains boot-path events. The `daemon logs` subcommand tails it
for you.

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
bhx debug read-reset-reg
bhx debug reset-x280      -l 0
bhx debug assert-reset    -l 0
bhx debug deassert-reset  -l 0
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
