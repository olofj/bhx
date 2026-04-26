# tt-bh-linux test rootfs (buildroot)

A small, reproducible riscv64 ext4 image built with buildroot, intended
**only** for hardware soaks. **Never** deploy this image anywhere
networked or shared — it auto-logs in as root on the console with no
authentication.

## What's inside

| Component | Why |
|-----------|-----|
| busybox | init + most coreutils + getty + ifupdown |
| bash | Shell idioms used in our soaks (busybox ash isn't quite enough) |
| dropbear | Minimal SSH daemon for SSH-based stress tests |
| iperf3 | TCP/UDP throughput |
| fio | Disk-IO patterns finer than `dd` |
| stress-ng | CPU/memory/disk stress |
| tcpdump | Network-test debugging |
| python3 | Inline helpers some soaks shell out to |

## Build

First build is slow (~20-30 minutes; downloads + builds the toolchain
from source). Subsequent builds are incremental.

```bash
cd tt-bh-linux-rs/tests/rootfs
make check-deps          # verify host tools (build-essential, wget, cpio, ...)
make                     # produces output/images/rootfs.ext4
```

The build pins buildroot **v2026.02.1** (SHA256 in `sha256sums`).
Bumping the pin needs a separate PR that updates both files.

Output: `output/images/rootfs.ext4` (~96 MiB by default — the size cap
in `buildroot.config`'s `BR2_TARGET_ROOTFS_EXT2_SIZE`. Drop it if you
strip packages; raise it if you add more).

## Use

```bash
cd ../..    # back to tt-bh-linux-rs/
./target/debug/tt-bh-linux daemon start -t 0 --log-file ./daemon-card0.log
./target/debug/tt-bh-linux boot -l 0 -d tests/rootfs/output/images/rootfs.ext4 -n
./target/debug/tt-bh-linux connect -l 0
```

Within ~15s the console drops to `#` (no `login:` prompt). DHCP comes
up automatically against slirp; check `ip addr` to confirm.

## How it differs from the regular `image pull debian` flow

- **No package manager.** Need a tool that's not in the image? Edit
  `buildroot.config`, add a `BR2_PACKAGE_<NAME>=y`, rebuild. There is
  no `apt-get install` — that's a feature, not a bug.
- **No persistent users / groups beyond root.** dropbear inherits
  buildroot's empty root password by default; the tests that exercise
  SSH must arrange for either an authorized_keys overlay or
  `PermitEmptyPasswords=yes` (dropbear's `-B` flag). See `Out of box
  SSH` below.
- **Auto-login on hvc0.** `overlay/etc/inittab` runs busybox getty
  with `-n -l /bin/sh` so soak scripts can drive a shell immediately
  after warm-resume.

## Out-of-box SSH

Dropbear's default in buildroot is "blank-password login disallowed";
this is the safe default. To run the issue-#16 acceptance test
`ssh -p 2222 root@localhost echo ok`, drop a public key into
`overlay/root/.ssh/authorized_keys` before building:

```bash
mkdir -p overlay/root/.ssh
chmod 700 overlay/root/.ssh
cp ~/.ssh/id_ed25519.pub overlay/root/.ssh/authorized_keys
chmod 600 overlay/root/.ssh/authorized_keys
make rebuild
```

(buildroot copies the overlay into the rootfs as-is and applies sane
permissions.)

## Layout

```
tests/rootfs/
├── README.md                 (this file)
├── Makefile                  download buildroot, run defconfig + make
├── buildroot.config          our defconfig (collapsed; only non-default lines)
├── sha256sums                pinned tarball checksum
├── overlay/                  copied verbatim into the rootfs
│   └── etc/
│       └── inittab           overrides busybox default; auto-login on hvc0
├── dl/                       (gitignored) buildroot's download cache
├── buildroot-2026.02.1/      (gitignored) extracted source tree
└── output/                   (gitignored) build output; rootfs.ext4 lives here
```

## Reproducibility

- The buildroot tarball is verified against `sha256sums` before
  extraction.
- `BR2_DOWNLOAD_FORCE_CHECK_HASHES=y` (inherited from the parent
  qemu_riscv64_virt config — set explicitly in `buildroot.config` if
  you ever drop that inheritance) forces buildroot to verify every
  downloaded source tarball.
- `BR2_REPRODUCIBLE=y` is set in `buildroot.config`, and the Makefile
  pins `SOURCE_DATE_EPOCH` to the timestamp of the last commit
  touching `tests/rootfs/`. Two builds from the same commit produce a
  byte-identical `rootfs.ext4`; a different commit (or
  `SOURCE_DATE_EPOCH=N make` from the shell) produces a different but
  equally reproducible image. UUID and ext4 hash_seed are pinned via
  `BR2_TARGET_ROOTFS_EXT2_MKFS_OPTIONS` so `mkfs.ext4`'s libuuid call
  doesn't reintroduce randomness in the superblock.

  Verify reproducibility:
  ```bash
  make rebuild && sha256sum rootfs.ext4
  make rebuild && sha256sum rootfs.ext4   # must match
  ```

## Bumping buildroot

1. `wget -O - https://buildroot.org/downloads/buildroot-NEW.tar.gz | sha256sum`
2. Update `BUILDROOT_VERSION` in `Makefile`.
3. Update `sha256sums` with the new tarball's hash.
4. `make distclean && make` and verify the resulting `rootfs.ext4`
   still boots end-to-end.
