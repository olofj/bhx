# bhx Device Tree Blob

A vendored copy of `blackhole-card.dtb` — the RISC-V Device Tree
describing the Blackhole L2CPU's hart, DRAM range, and SBI debug-
console node. OpenSBI reads it at L2CPU DRAM start + 1 MiB
(`FW_JUMP_FDT_OFFSET=0x100000`) and patches it further at runtime via
`boot::modify_dtb` (per-L2CPU DRAM size, virtio-mmio nodes, reserved
memory, `/chosen/bootargs`).

This is the second source of `blackhole-card.dtb`. The default operator
path remains `bhx kernel pull`, which downloads the DTB as part of a
prebuilt bundle from `tt-bh-linux` releases. Use this in-tree copy when
you want to avoid the dependency on someone else's release artifacts.

## Provenance

The binary in this directory is a verbatim copy of `blackhole-card.dtb`
as built by the upstream Tenstorrent tt-bh-linux project:

- **Source repo:** https://github.com/tenstorrent/tt-bh-linux
- **Source repo commit:**
  `a5223b720a811f5d9da5a6070b1e182ec0d0a7ef` (2026-04-13)
- **Built from DTS files:** `arch/riscv/boot/dts/tenstorrent/blackhole-card.dts`
  + `blackhole.dtsi`
- **DTS source repo:** https://github.com/tenstorrent/linux
- **DTS source branch / commit:** `tt-blackhole` @
  `711227ac36a960712af7687b913e6ff3c00e1769` (2026-04-12)
- **SHA-256 of the imported DTB:**
  `d447e8c9613a1a33154c51aa31148f49d8ef69581d2f7a5e24b8ca4d1f0a0f73`

The kernel tree is wired into `tt-bh-linux` as a git submodule at
`linux/`; the DTB is produced by the kernel's standard
`make dtbs` flow against that submodule.

## License & copyright

The upstream device tree source files declare:

```
// SPDX-License-Identifier: (GPL-2.0 OR MIT)
// Copyright 2025 Tenstorrent AI ULC
```

That license and copyright apply to this vendored binary too. The DTB
is a compiled artifact of those `.dts` / `.dtsi` files, so it inherits
their SPDX terms — we redistribute it under MIT (the bhx project's
license) per the dual license. The copyright stays with Tenstorrent.

## Use

```bash
./target/debug/bhx daemon start -t 0 --log-file ./daemon-card0.log
./target/debug/bhx boot -l 0 --dtb third_party/dtb/blackhole-card.dtb
```

`bhx boot` defaults `--dtb` to `blackhole-card.dtb` in the caller's
cwd (the `kernel pull` convention), so the simplest workflow is:

```bash
ln -sf third_party/dtb/blackhole-card.dtb ./blackhole-card.dtb
./target/debug/bhx boot -l 0    # picks up blackhole-card.dtb from cwd
```

## Refreshing

This is a checked-in binary, not a build harness — there is no
`Makefile` here. To refresh:

1. Update the upstream `tt-bh-linux` checkout, build the DTB
   (`make build_dtb` in tt-bh-linux), or pull a release bundle.
2. `cp <fresh>/blackhole-card.dtb third_party/dtb/blackhole-card.dtb`
3. Update the **Provenance** section above with the new commit SHAs
   and the new SHA-256.
4. Verify boot end-to-end on hardware before committing — DTB changes
   can quietly shift `/memory`, `/chosen`, or virtio-mmio reg ranges
   in ways `boot::modify_dtb` may not anticipate.

## Why not build from source in-tree

Unlike `third_party/uboot/` and `third_party/opensbi/`, we do **not**
ship a Makefile that builds the DTB from a kernel source tree. The
DTS depends on a specific Linux kernel (the Tenstorrent fork's
RISC-V tree), and pulling the full kernel just to compile a 2.7 KB
binary is disproportionate. If we ever need to patch the DTS itself,
the right move is to vendor the two `.dts` / `.dtsi` files alongside
this directory and build them with `dtc` directly — file an issue
when that need lands.

## Layout

```
third_party/dtb/
├── README.md           (this file)
└── blackhole-card.dtb  (checked in; ~2.7 KB)
```
