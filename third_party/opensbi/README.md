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
make check-deps          # verifies git, riscv64-linux-gnu-gcc
make                     # clones upstream, builds fw_jump.bin
```

First build clones `riscv-software-src/opensbi` into `opensbi-src/`,
checks out the pinned SHA, applies patches, and compiles in <1 minute
on a modern host.

Output: `fw_jump.bin` (symlink to
`opensbi-src/build/platform/generic/firmware/fw_jump.bin`).

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

### Why git clone (not tarball)

The OpenSBI build's `Makefile` runs `git rev-parse --git-dir` from
inside its own source tree to derive the boot banner string. With a
tarball extracted under bhx's `.git`, that probe walked up the
filesystem and found OUR repo — the banner printed
`OpenSBI v<bhx-cargo-version>-<n>-g<bhx-sha>` instead of the actual
upstream OpenSBI version. Cloning makes `opensbi-src/.git` the
closest `.git` ancestor; the banner reflects the real provenance:

```
OpenSBI v1.8.1
```

The clone is pinned to a specific commit SHA in the Makefile so
upstream tag re-points can't silently change what gets built.

## Why upstream OpenSBI plus local patches

`tt-bh-linux` builds OpenSBI from `github.com/tenstorrent/opensbi`
branch `tt-blackhole`. Its functional delta against upstream is one
feature: a `debug_descriptor` placed at a fixed offset (0x80) of
`fw_jump.bin` plus a virtual-UART driver that reads/writes through
it. The daemon's chip-side console pump
(`src/daemon/chip_console.rs`) probes that descriptor on warm-resume
to recover a still-running L2CPU's UART base, so this patch is
load-bearing — without it `daemon stop` + `daemon start` against a
live core can't re-adopt the slot and reports `Wedged`.

Rather than vendoring the entire `tt-blackhole` branch, we pin
upstream `riscv-software-src/opensbi` at a known SHA and apply the
Tenstorrent diff plus our own downstream patches as files under
`patches/`. The Makefile picks up any `*.patch` in that directory in
alphabetical order, so adding more later is drop-in:

- `0001-tenstorrent-debug-descriptor-virtual-uart.patch` — Tenstorrent's
  debug_descriptor + virtual-UART driver.
- `0002-bhx-purgatory-magic.patch` — bhx-purgatory soft-reboot hook
  (#166), force-park IPI event (#166), SRST-type-aware status block
  (#177).
- `0003-isa-ext-emu.patch` — Benedikt Freisen's v3 patch 2/3,
  trap-based emulation of RVA22/RVA23 ISA extensions so the X280
  (RVA22-class Gen.1) can run stock RVA23 distros like Ubuntu
  25.10/26.04. The Makefile flips `CONFIG_SBI_ISA_EXT_EMU=y` +
  `CONFIG_EMU_RVB23=y` in the platform defconfig after the patch
  applies. See #163.

## Bumping OpenSBI

1. Find the desired upstream tag's SHA:
   ```bash
   git ls-remote https://github.com/riscv-software-src/opensbi.git refs/tags/vNEW
   ```
2. Update `OPENSBI_VERSION` and `OPENSBI_SHA` in `Makefile` (both must
   move together — version is for the human-readable status line,
   SHA is what's checked out).
3. `make clean && make` — refreshes the clone, applies patches, and
   builds.
4. If any of the `patches/*.patch` files fail to apply, refresh them
   against the new tree (`make` will halt with the hunk that didn't
   apply — fix and re-run).
5. Verify `fw_jump.bin` boots end-to-end on hardware (banner visible
   via `connect`, kernel hands off to `Image` or U-Boot as expected).

## Reproducibility

Same workflow as `third_party/uboot/`: the Makefile pins
`SOURCE_DATE_EPOCH` to the timestamp of the last commit touching
`third_party/opensbi/`. Two clean builds from the same commit produce
a byte-identical `fw_jump.bin`. Verify:

```bash
make clean && make && sha256sum fw_jump.bin
make clean && make && sha256sum fw_jump.bin   # must match
```

## Layout

```
third_party/opensbi/
├── README.md             (this file)
├── Makefile              clone + build
├── patches/              local downstream patches
├── opensbi-src/          (gitignored) cloned upstream tree
└── fw_jump.bin           (gitignored) symlink to the build output
```
