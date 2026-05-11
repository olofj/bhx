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

Source: the bhx downstream OpenSBI fork at
[`github.com/olofj/opensbi`](https://github.com/olofj/opensbi) on the
`bhx` branch — pinned to a specific commit SHA by `Makefile`. The
fork is upstream `riscv-software-src/opensbi v1.8.1` plus one commit
per bhx feature:

1. `tenstorrent: debug_descriptor + virtual-UART driver` —
   Tenstorrent's debug_descriptor block and virtual UART, vendored
   from their `tt-blackhole` branch.
2. `bhx: SRST-type-aware purgatory soft-reboot hook` — bhx-purgatory
   (#166) + SRST-type-aware status block (#177).
3. `lib: sbi: trap-based ISA extension emulator (Freisen v3 2/3)` —
   Benedikt Freisen's scalar ISA-emu series. See #163.
4. `lib: sbi: ISA-emu PMU hook + bhx PMU device + host-readable
   publish` — generic emulator counter hook + bhx PMU device + a
   statically-placed publish struct the host reads over PCIe via a
   pointer at fw_jump.bin + 0x88. See #199.
5. `lib: sbi: Zvbb ISA extension emulation (Freisen v3 3/3)` —
   Freisen's Vector Basic Bit-manipulation emulator, the gating
   extension for stock RVA23U64 userspaces (Ubuntu 26.04). See
   #163.
6. `bhx: enable EMU_ZVBB + wire it into the ISA-emu PMU` — defconfig
   knobs + PMU event ID for Zvbb hits.

Local development against the fork: edit + commit in
`~/bh/opensbi-bhx/` (a working clone of `olofj/opensbi`), then point
the Makefile at it temporarily and rebuild:

```bash
make -C third_party/opensbi OPENSBI_REPO=$HOME/bh/opensbi-bhx \
                            OPENSBI_SHA=$(git -C $HOME/bh/opensbi-bhx rev-parse HEAD)
```

When the change is ready, push the branch to `olofj/opensbi:bhx` and
bump `OPENSBI_SHA` in `Makefile` to point at the new tip.

## Bumping OpenSBI (upstream → bhx fork → bhx)

1. In the fork clone (`~/bh/opensbi-bhx`), rebase the `bhx` branch
   onto a newer upstream tag:
   ```bash
   cd ~/bh/opensbi-bhx
   git fetch upstream
   git rebase v1.x.y bhx
   ```
2. Fix any conflicts in the bhx feature commits (most likely
   touching `sbi_illegal_insn.c` or `fw_base.S`).
3. Push: `git push --force-with-lease origin bhx`.
4. In bhx, bump `OPENSBI_VERSION` and `OPENSBI_SHA` in
   `third_party/opensbi/Makefile` together (version is for the
   human-readable banner, SHA is what's checked out).
5. `make clean && make` — refreshes the clone and builds.
6. Verify `fw_jump.bin` boots end-to-end on hardware (banner visible
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
