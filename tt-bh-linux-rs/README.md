# tt-bh-linux-rs

Rust tool for booting and running Linux on the SiFive X280 RISC-V cores
(L2CPUs) embedded on a Tenstorrent Blackhole card. Replaces the
Python + C++ pipeline in this repo's parent directory — the `boot.py`
driver and the C++ `tt-bh-linux` console tool are no longer needed.

This crate does the whole flow end-to-end: reset the chip, load
OpenSBI + Linux kernel + DTB into the L2CPU's DRAM, release it from
reset, and then emulate virtio-block / virtio-net devices and the
OpenSBI virtual UART console.

The tool runs as a **per-card daemon** that owns the chip's resources
and serves boot / console / disk / net operations to short-lived CLI
clients (`boot`, `connect`, `add-disk`, ...).

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
cd tt-bh-linux-rs
cargo build --release       # or plain `cargo build` for a dev build
```

## Fetching the firmware + a rootfs

You need three firmware files plus a disk image, all in the current
directory:

- `fw_jump.bin` — OpenSBI
- `Image` — Linux kernel
- `blackhole-card.dtb` — device tree blob
- `rootfs.ext4` — root filesystem

The tool can download them for you:

```bash
# Firmware (fw_jump.bin + Image + blackhole-card.dtb):
cargo run -- kernel pull

# Rootfs (Debian 13 is the default pick):
cargo run -- image pull debian
ln -sf images/debian-13.ext4 rootfs.ext4     # or use --disk in boot below
```

Run `cargo run -- image list` / `cargo run -- kernel list` to see what
else is available.

## Quick start

Boot one L2CPU with disk and network, then connect a terminal to it:

```bash
# Start the per-card daemon (once per boot of the host). The log file
# is pinned to this directory with O_DSYNC so every line is on disk
# before write() returns — handy if the host ever crashes.
./target/debug/tt-bh-linux daemon start -t 0 --log-file ./daemon-card0.log

# Boot L2CPU 0 with the rootfs in this directory and slirp networking.
./target/debug/tt-bh-linux boot -l 0 -d rootfs.ext4 -n

# Attach a terminal. Ctrl-A x to detach.
./target/debug/tt-bh-linux connect -l 0
```

Log in as `debian` (no password). The `-n` flag enables slirp
networking with TCP port 2222 on the host forwarded to port 22 inside
the guest, so you can also `ssh -p 2222 debian@localhost`.

Check what's running:

```bash
./target/debug/tt-bh-linux daemon status -t 0
# daemon: running (card 0, pid ..., uptime Ns, sock /run/user/.../sock)
#   l2cpu 0: Running disk=/.../rootfs.ext4 net=y clients=0
#   l2cpu 1: Stopped disk=- net=- clients=0
#   l2cpu 2: Stopped disk=- net=- clients=0
#   l2cpu 3: Stopped disk=- net=- clients=0
```

When you're done:

```bash
./target/debug/tt-bh-linux daemon stop -t 0
```

## Common operations

Once an L2CPU is booted, you can reconfigure its devices without
rebooting the guest:

```bash
# Swap the disk image (the guest sees a short unmount/remount):
./target/debug/tt-bh-linux remove-disk -l 0
./target/debug/tt-bh-linux add-disk    -l 0 some-other-rootfs.ext4

# Attach/detach networking:
./target/debug/tt-bh-linux remove-net  -l 0
./target/debug/tt-bh-linux add-net     -l 0

# Re-image a running core in place (tears down workers first):
./target/debug/tt-bh-linux boot -l 0 -d rootfs.ext4 -n --force
```

Run all four L2CPUs at once — each wants its own rootfs to avoid
ext4 corruption from concurrent writers:

```bash
for i in 0 1 2 3; do
    cp --reflink=auto rootfs.ext4 rootfs-$i.ext4
    ./target/debug/tt-bh-linux boot -l $i -d rootfs-$i.ext4 -n
done
```

## Scripting tips

Wrap `connect` with `timeout` when running non-interactively — it runs
forever and only exits on Ctrl-A x:

```bash
timeout 5 ./target/debug/tt-bh-linux connect -l 0 </dev/null 2>/tmp/stderr.log
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
  `rm /run/user/$UID/tt-bh-linux/0/pid` and try again.

- **`vdeslirp_open returned NULL`** on `-n`: check `pkg-config
  --modversion vdeslirp libslirp`. Expected: vdeslirp 0.1.x + libslirp
  4.x. The error from `network.rs` also lists likely causes (fd
  limits, seccomp, ABI mismatch).

## Diagnostics bypassing the daemon

For poking the chip directly (requires the daemon stopped for this
card):

```bash
./target/debug/tt-bh-linux debug read-reset-reg
./target/debug/tt-bh-linux debug reset-x280      -l 0
./target/debug/tt-bh-linux debug assert-reset    -l 0
./target/debug/tt-bh-linux debug deassert-reset  -l 0
```

## Going deeper

- **Architecture + per-module map**: see `CLAUDE.md` in this directory
  (written for AI assistants but very readable as a design doc).
- **Hardware soak scripts**: see `scripts/README.md`. Includes a 4-way
  concurrent console I/O roundtrip test.
- **Open design issues**: the GitHub issue tracker at
  <https://github.com/olofj/tt-bh-rust/issues>.

## Relationship to the parent directory

The `../README.md` at the top of this repo describes the original
Python + C++ pipeline (`../boot.py`, `../console/tt-bh-linux`). That
stack still works but you don't need it for anything this crate does —
this Rust tool replaces both halves. The `../Makefile` pre-dates the
Rust port and still drives the Python/C++ path, not this one.
