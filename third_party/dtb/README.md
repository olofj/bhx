# bhx Device Tree

Vendored device tree sources for the Blackhole L2CPU. `make` compiles
them with `cpp` + `dtc` into `blackhole-card.dtb` — the binary OpenSBI
reads at L2CPU DRAM start + 1 MiB (`FW_JUMP_FDT_OFFSET=0x100000`) and
that `boot::modify_dtb` patches further at runtime (per-L2CPU DRAM
size, virtio-mmio nodes, reserved memory, `/chosen/bootargs`).

This is the only source of `blackhole-card.dtb` — every operator
builds it locally from this directory. (Earlier versions of bhx had a
`kernel pull` subcommand that downloaded a prebuilt DTB from the
`tt-bh-linux` release bundle; that subcommand has been removed in
favor of in-tree builds, see #84.)

## Build

```bash
cd third_party/dtb       # from the bhx project root
make check-deps          # verifies cpp, dtc
make                     # produces blackhole-card.dtb (~2.7 KB)
```

The build is trivial: `cpp` resolves the `#include "blackhole.dtsi"`,
then `dtc` compiles the merged DTS into a DTB. No cross-toolchain
needed (cpp + dtc both run on the host).

## Provenance

The two source files in this directory — `blackhole-card.dts` and
`blackhole.dtsi` — are verbatim copies from the Tenstorrent Linux
fork:

- **Source repo:** https://github.com/tenstorrent/linux
- **Source branch:** `tt-blackhole`
- **Source commit:** `711227ac36a960712af7687b913e6ff3c00e1769`
  (2026-04-12)
- **Source path in upstream:**
  `arch/riscv/boot/dts/tenstorrent/blackhole-card.dts`
  + `arch/riscv/boot/dts/tenstorrent/blackhole.dtsi`
- **As cross-checked via:** https://github.com/tenstorrent/tt-bh-linux
  @ `a5223b720a811f5d9da5a6070b1e182ec0d0a7ef` (2026-04-13), which
  pins the linux fork as a submodule

A clean build of these sources reproduces the SHA-256
`d447e8c9613a1a33154c51aa31148f49d8ef69581d2f7a5e24b8ca4d1f0a0f73`
— the same DTB tt-bh-linux ships in its release bundle.

## License & copyright

The upstream device tree source files declare:

```
// SPDX-License-Identifier: (GPL-2.0 OR MIT)
// Copyright 2025 Tenstorrent AI ULC
```

The same SPDX header is preserved in the vendored copies in this
directory. We redistribute under MIT (the bhx project's license) per
that dual license. Copyright stays with **Tenstorrent AI ULC**.

## Use

```bash
./target/debug/bhx daemon start -t 0 --log-file ./daemon-card0.log
./target/debug/bhx boot -l 0 --dtb third_party/dtb/blackhole-card.dtb
```

`bhx boot` defaults `--dtb` to `blackhole-card.dtb` in the caller's
cwd, so the simplest workflow is:

```bash
make -C third_party/dtb
ln -sf third_party/dtb/blackhole-card.dtb ./blackhole-card.dtb
./target/debug/bhx boot -l 0    # picks up blackhole-card.dtb from cwd
```

## Bumping

1. Find the upstream commit you want from
   https://github.com/tenstorrent/linux (branch `tt-blackhole`).
2. Replace `blackhole-card.dts` and `blackhole.dtsi` with the new
   versions from `arch/riscv/boot/dts/tenstorrent/`.
3. Update the **Provenance** section above with the new commit SHA.
4. `make clean && make` and verify boot end-to-end on hardware — DTB
   changes can quietly shift `/memory`, `/chosen`, or virtio-mmio reg
   ranges in ways `boot::modify_dtb` may not anticipate.

## Layout

```
third_party/dtb/
├── README.md           (this file)
├── Makefile            cpp + dtc
├── blackhole-card.dts  vendored (Copyright 2025 Tenstorrent AI ULC, GPL-2.0 OR MIT)
├── blackhole.dtsi      vendored (Copyright 2025 Tenstorrent AI ULC, GPL-2.0 OR MIT)
└── blackhole-card.dtb  (gitignored) build output
```
