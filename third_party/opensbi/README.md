# bhx OpenSBI

A pinned OpenSBI build that produces `fw_jump.bin` — the M-mode firmware
the L2CPU lands in after release-from-reset. OpenSBI sets up M-mode
state, reads the DTB at `+0x10_0000`, and jumps to the next stage at
`+0x20_0000` (either a raw Linux `Image` or U-Boot in S-mode).

This is the only source of `fw_jump.bin` — every operator builds it
locally from this directory. (Earlier versions of bhx had a
`kernel pull` subcommand that downloaded a prebuilt OpenSBI from the
`tt-bh-linux` release bundle; that subcommand has been removed in
favor of in-tree builds, see #84.)

## Build

```bash
cd third_party/opensbi   # from the bhx project root
make check-deps          # verifies wget, tar, riscv64-linux-gnu-gcc
make                     # downloads opensbi-1.7.tar.gz, builds fw_jump.bin
```

First build downloads ~480 KB and compiles in <1 minute on a modern
host. Subsequent builds reuse the extracted source tree under
`opensbi-1.7/`.

Output: `fw_jump.bin` (symlink to `opensbi-1.7/build/platform/generic/firmware/fw_jump.bin`).

## Use

```bash
cd ../..    # back to bhx/
./target/debug/bhx daemon start -t 0 --log-file ./daemon-card0.log
./target/debug/bhx boot -l 0 --opensbi third_party/opensbi/fw_jump.bin
./target/debug/bhx connect -l 0
```

`bhx boot` defaults `--opensbi` to `fw_jump.bin` in the caller's cwd,
so the simplest workflow is to keep the symlink there:

```bash
ln -sf third_party/opensbi/fw_jump.bin ./fw_jump.bin
./target/debug/bhx boot -l 0    # picks up fw_jump.bin from cwd
```

## How the build is wired

OpenSBI's `generic` platform reads everything it needs (memory map,
HART list, console) from the DTB at runtime. The bhx DTB
(`blackhole-card.dtb`) describes the L2CPU's hart, DRAM range, and
SBI debug-console node, so no platform-specific OpenSBI port is
needed.

Build flags pinned in `Makefile`:

- `PLATFORM=generic` — the DTB-driven catch-all platform.
- `FW_JUMP=y` — produce `fw_jump.bin`, the variant that jumps to a
  fixed address rather than embedding the next-stage payload. The
  daemon loads kernel/U-Boot at that fixed address itself.
- `FW_JUMP_OFFSET=0x200000` — kernel/U-Boot is at L2CPU DRAM start
  + 2 MiB. Matches `boot_image::KERNEL_OFFSET` on the Rust side.
- `FW_JUMP_FDT_OFFSET=0x100000` — DTB is at L2CPU DRAM start + 1 MiB.
  Matches `boot_image::DTB_OFFSET` on the Rust side.
- `BUILD_INFO=y` — prints the OpenSBI banner with version + commit on
  boot. Useful when triaging "is the firmware actually rebuilt"
  questions over the chip console.

## Why upstream OpenSBI plus a Tenstorrent patch

`tt-bh-linux` builds OpenSBI from `github.com/tenstorrent/opensbi`
branch `tt-blackhole`. As of v1.7 the functional delta against
upstream is one feature: a `debug_descriptor` placed at a fixed
offset (0x80) of `fw_jump.bin` plus a virtual-UART driver that
reads/writes through it. The daemon's chip-side console pump
(`src/daemon/chip_console.rs`) probes that descriptor on warm-resume
to recover a still-running L2CPU's UART base, so this patch is
load-bearing — without it `daemon stop` + `daemon start` against a
live core can't re-adopt the slot and reports `Wedged`.

Rather than vendoring the entire `tt-blackhole` branch, we pin
upstream `riscv-software-src/opensbi` v1.7 and apply the
Tenstorrent diff as `patches/0001-tenstorrent-debug-descriptor-
virtual-uart.patch`. The Makefile picks up any `*.patch` in that
directory in alphabetical order, idempotently (`patch -N`), so
adding more later is drop-in.

## Bumping OpenSBI

1. `wget -O - https://github.com/riscv-software-src/opensbi/archive/refs/tags/vNEW.tar.gz | sha256sum`
2. Update `OPENSBI_VERSION` in `Makefile`.
3. Update `sha256sums` with the new tarball's hash.
4. `make distclean && make` and verify `fw_jump.bin` boots end-to-end
   on hardware (banner visible via `connect`, kernel hands off to
   `Image` or U-Boot as expected).

## Reproducibility

Same workflow as `third_party/uboot/`: the Makefile pins
`SOURCE_DATE_EPOCH` to the timestamp of the last commit touching
`third_party/opensbi/`. Two clean builds from the same commit produce
a byte-identical `fw_jump.bin`. Verify:

```bash
make distclean && make && sha256sum fw_jump.bin
make distclean && make && sha256sum fw_jump.bin   # must match
```

## Layout

```
third_party/opensbi/
├── README.md           (this file)
├── Makefile            download + build
├── sha256sums          pinned tarball checksum
├── dl/                 (gitignored) download cache
├── opensbi-1.7/        (gitignored) extracted source tree
└── fw_jump.bin         (gitignored) symlink to the build output
```
