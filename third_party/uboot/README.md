# bhx U-Boot

A pinned U-Boot build that runs as the S-mode payload after OpenSBI on a
booted Blackhole L2CPU. Replaces the raw Linux `Image` payload in the
`bhx boot` chain so stock distro images (multi-partition disks
with EFI shim + grub + kernel + initramfs) can boot end-to-end.

The umbrella for the U-Boot integration is **#44**; this build feeds
**M1 (#45)** through **M3 (#47)** without per-milestone rebuilds — once
`u-boot.bin` exists, the milestones differ only in U-Boot env / boot
target, not the binary.

## Build

```bash
cd third_party/uboot     # from the bhx project root
make check-deps          # verifies wget, tar, riscv64-linux-gnu-gcc, bison, flex
make                     # downloads u-boot-2026.04.tar.bz2, builds u-boot.bin
```

First build downloads ~34 MB and compiles ~5 minutes on a modern host.
Subsequent builds reuse the extracted source tree under `u-boot-2026.04/`.

Output: `u-boot.bin` (symlink to the actual file in the source tree).

## Use

```bash
cd ../..    # back to bhx/
./target/debug/bhx daemon start -t 0 --log-file ./daemon-card0.log
./target/debug/bhx boot -l 0 --uboot third_party/uboot/u-boot.bin
./target/debug/bhx connect -l 0
```

When the boot subcommand is called without `--uboot` against a disk
that maps to a known image with `needs_bootloader: true` (e.g.
`images/almalinux-10-kitten.img`), it auto-defaults to
`--uboot u-boot.bin` — i.e. operators can keep the symlink in cwd
and the disk-detection wiring picks it up.

A successful M1 brings up a `=>` U-Boot prompt within ~10 s of release-
from-reset. M2 / M3 add disk-attached autoboot.

## How the build is wired

The defconfig is a two-step layer:

1. **Upstream's `qemu-riscv64_smode_defconfig`** as the base. This is
   the canonical S-mode-after-OpenSBI config: virtio-mmio + virtio-blk
   for disk, EFI loader for bootefi, bootstd for autoboot discovery.
2. **`bhx.config`** (in this directory) as a delta that turns on the
   SBI debug-console driver (so U-Boot console output reaches our
   `chip_console::uart_pass` worker via OpenSBI ecalls) and pins a few
   bootflow knobs.

`scripts/kconfig/merge_config.sh` does the merge; `make olddefconfig`
shakes out new symbols. The fragment is intentionally short — bumping
U-Boot's pinned version means re-running the merge, not re-resolving a
fork.

## Bumping U-Boot

1. `wget -O - https://ftp.denx.de/pub/u-boot/u-boot-NEW.tar.bz2 | sha256sum`
2. Update `UBOOT_VERSION` in `Makefile`.
3. Update `sha256sums` with the new tarball's hash.
4. `make distclean && make` and verify `u-boot.bin` boots end-to-end on
   hardware (banner visible via `connect`, M2's `virtio scan` works,
   M3's `bootefi bootmgr` finds an ESP).

## Reproducibility

Same workflow as `third_party/buildroot/` (#39): the Makefile pins
`SOURCE_DATE_EPOCH` to the timestamp of the last commit touching
`third_party/uboot/`. Two clean builds from the same commit produce a
byte-identical `u-boot.bin`. Verify:

```bash
make distclean && make && sha256sum u-boot.bin
make distclean && make && sha256sum u-boot.bin   # must match
```

## Layout

```
third_party/uboot/
├── README.md           (this file)
├── Makefile            download + build
├── bhx.config          defconfig fragment merged on top of qemu-riscv64_smode_defconfig
├── sha256sums          pinned tarball checksum
├── dl/                 (gitignored) download cache
├── u-boot-2026.04/     (gitignored) extracted source tree
└── u-boot.bin          (gitignored) symlink to the build output
```
