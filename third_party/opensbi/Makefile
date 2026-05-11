# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# Build OpenSBI for the Blackhole L2CPU. Source is upstream
# riscv-software-src/opensbi at a pinned commit SHA, fetched into
# opensbi-src/ via `git clone`. Build artifacts (opensbi-src/,
# fw_jump.bin symlink) are gitignored.
#
# The boot chain: OpenSBI fw_jump (M-mode) -> kernel `Image` directly
# (legacy mode), or -> U-Boot (S-mode) -> kernel + initramfs (#44).
# OpenSBI lands at L2CPU DRAM start; the kernel/U-Boot payload is
# placed at FW_JUMP_OFFSET (+0x20_0000) and the DTB at
# FW_JUMP_FDT_OFFSET (+0x10_0000). Those offsets match the addresses
# in `boot::OPENSBI_OFFSET` + `boot_image::*_OFFSET` on the Rust side.
#
# Tenstorrent maintains a `tt-blackhole` branch on their opensbi fork.
# Its functional delta against upstream is the debug_descriptor at
# offset 0x80 of fw_jump.bin plus a virtual-UART driver — load-
# bearing for daemon warm-resume (chip_console::probe_warm_resume).
# We pin upstream and apply the Tenstorrent diff plus our own
# downstream patches in patches/. The build applies *.patch
# alphabetically, idempotently.
#
# Why git clone (not tarball): OpenSBI's own Makefile runs
# `git rev-parse --git-dir` from inside its source tree to produce
# the boot banner string. With a tarball extracted under bhx/.git,
# that probe walks up and finds OUR repo, printing the bhx-side
# `git describe` in the banner ("OpenSBI v0.9.0-42-gb9ae923"
# instead of "OpenSBI v1.8.1-..."). Cloning makes opensbi-src/.git
# the closest .git ancestor; the banner reflects the actual pinned
# OpenSBI provenance.
#
# Usage:
#   make             # clone + build fw_jump.bin
#   make rebuild     # re-run the build without re-cloning
#   make clean       # drop the source tree
#   make distclean   # alias of clean (no separate dl/ cache anymore)

# Pinned OpenSBI commit. Update OPENSBI_VERSION (used for display)
# and OPENSBI_SHA together when bumping. The SHA is what's actually
# checked out; the version string is for human-readable build banner
# only and gets propagated to the banner via OpenSBI's own `git
# describe` against this checkout.
OPENSBI_VERSION := 1.8.1
OPENSBI_SHA := 823b88afe71811eb6df556b927fbd6f52d240bf3
OPENSBI_REPO := https://github.com/riscv-software-src/opensbi.git

TOPDIR := $(CURDIR)
SB_SRCDIR := $(TOPDIR)/opensbi-src

# Build flags. PLATFORM=generic is the canonical RISC-V "boot anything
# that supplies a DTB" platform. FW_JUMP=y selects the fw_jump payload
# variant (the firmware jumps to a fixed address rather than embedding
# the next-stage payload). The two offsets are relative to the firmware
# load address: kernel/U-Boot at +2 MiB, DTB at +1 MiB. BUILD_INFO=y
# embeds the OpenSBI commit/version banner that prints during boot.
SBI_FLAGS := PLATFORM=generic FW_JUMP=y \
             FW_JUMP_OFFSET=0x200000 FW_JUMP_FDT_OFFSET=0x100000 \
             BUILD_INFO=y

# Final blob landed by the build.
FW_JUMP := $(SB_SRCDIR)/build/platform/generic/firmware/fw_jump.bin
# Stable convenience path. The Makefile maintains a symlink here so
# `bhx boot --opensbi third_party/opensbi/fw_jump.bin` doesn't need
# to know the OpenSBI version directory name.
FW_JUMP_LINK := $(TOPDIR)/fw_jump.bin

# Cross-compiler. Same toolchain as third_party/uboot/. The Debian
# package gcc-riscv64-linux-gnu provides this as `riscv64-linux-gnu-`.
CROSS_COMPILE ?= riscv64-linux-gnu-

# Reproducibility: same workflow as third_party/uboot/. SOURCE_DATE_EPOCH
# pinned to the most recent commit touching third_party/opensbi/,
# anchored at $(TOPDIR) so unrelated commits don't bump the cache;
# ?= so the env wins for CI-style fixed-epoch builds. See #39.
SOURCE_DATE_EPOCH ?= $(shell git -C $(TOPDIR) log -1 --format=%ct -- $(TOPDIR) 2>/dev/null || echo 0)
export SOURCE_DATE_EPOCH

.PHONY: all rebuild clean distclean help check-deps

all: $(FW_JUMP)
	ln -sfr $(FW_JUMP) $(FW_JUMP_LINK)
	@echo
	@echo "==> Build complete:"
	@ls -lh $(FW_JUMP)
	@echo
	@echo "    Use as: bhx boot -l 0 --opensbi $(FW_JUMP_LINK)"

help:
	@echo "Targets:"
	@echo "  all (default)    Build fw_jump.bin. First run clones the upstream repo."
	@echo "  rebuild          Re-run the build without re-cloning."
	@echo "  clean            Drop the source tree."
	@echo "  distclean        Alias of clean."
	@echo "  check-deps       Verify host tools needed by the OpenSBI build are present."
	@echo
	@echo "Pinned OpenSBI version: $(OPENSBI_VERSION) ($(OPENSBI_SHA))"
	@echo "Cross-compiler:         $(CROSS_COMPILE)gcc"

PATCH_FILES := $(sort $(wildcard $(TOPDIR)/patches/*.patch))

# Clone + checkout + apply patches. The marker file is the upstream
# Makefile in the source tree — its presence + mtime determines
# whether we need to re-fetch.
$(SB_SRCDIR)/Makefile: $(PATCH_FILES)
	@# Reset the source dir before each fetch so patches always apply
	@# against a known-clean upstream tree. A previous patched
	@# checkout would otherwise have `tt-blackhole virtual-UART
	@# driver` files that aren't in upstream, and patch -N would
	@# double-apply on top.
	rm -rf $(SB_SRCDIR)
	@echo "==> Cloning $(OPENSBI_REPO) @ $(OPENSBI_SHA)"
	git clone --quiet $(OPENSBI_REPO) $(SB_SRCDIR)
	@echo "==> Checking out $(OPENSBI_SHA)"
	git -C $(SB_SRCDIR) checkout --quiet $(OPENSBI_SHA)
	@# Verify the resolved tag matches our pinned version. Belt +
	@# suspenders: catches the case where upstream re-points a tag
	@# (rare but technically allowed by git) or where our SHA/version
	@# pair drifted out of sync in the Makefile.
	@resolved=$$(git -C $(SB_SRCDIR) describe --tags --exact-match 2>/dev/null); \
	if [ "$$resolved" != "v$(OPENSBI_VERSION)" ]; then \
	    echo "WARNING: pinned SHA $(OPENSBI_SHA) resolves to '$$resolved', expected 'v$(OPENSBI_VERSION)'"; \
	fi
	@for p in $(PATCH_FILES); do \
		echo "==> Applying $$(basename $$p)"; \
		(cd $(SB_SRCDIR) && patch -p1 --quiet < $$p) || exit 1; \
	done
	@# (bhx#163) Enable the trap-based ISA extension emulator and turn
	@# on the RVB23 subset that covers the gap between X280 (Gen.1,
	@# RVA22-class) and a stock RVA23 distro (Ubuntu 25.10+). RVB23
	@# pulls in Zicond / Zimop / Zawrs / Zfa / Zcb / Zcmop and (via
	@# its RVA22 dependency) Zicbom / Zicboz / Zfhmin / Zba / Zbb /
	@# Zbs. We additionally enable EMU_ZBC + EMU_SUPM because the
	@# decode for both is already in patch 0003 and the SBI PMU
	@# patch in 0004 reserves event-data IDs for them; turning them
	@# on keeps the wire format complete instead of advertising
	@# always-zero counters. Skips EMU_ALL because that selects
	@# EMU_ZVBB which lives in Freisen v3 3/3 (not vendored).
	@# Append-to-defconfig flow: Kconfiglib picks these up alongside
	@# the platform's existing defaults during the OpenSBI build's
	@# olddefconfig pass.
	@echo "==> Enabling ISA-emu configs in generic defconfig"
	@printf 'CONFIG_SBI_ISA_EXT_EMU=y\nCONFIG_EMU_RVB23=y\nCONFIG_EMU_ZBC=y\nCONFIG_EMU_SUPM=y\n' \
		>> $(SB_SRCDIR)/platform/generic/configs/defconfig
	@touch $@

$(FW_JUMP): $(SB_SRCDIR)/Makefile
	$(MAKE) -C $(SB_SRCDIR) CROSS_COMPILE=$(CROSS_COMPILE) $(SBI_FLAGS)

rebuild:
	$(MAKE) -C $(SB_SRCDIR) CROSS_COMPILE=$(CROSS_COMPILE) $(SBI_FLAGS)
	ln -sfr $(FW_JUMP) $(FW_JUMP_LINK)

clean:
	rm -rf $(SB_SRCDIR)
	rm -f $(FW_JUMP_LINK)

distclean: clean

check-deps:
	@missing=; \
	for tool in git make $(CROSS_COMPILE)gcc; do \
		if ! command -v $$tool >/dev/null; then \
			echo "MISSING: $$tool"; \
			missing=1; \
		fi; \
	done; \
	if [ -n "$$missing" ]; then \
		echo; \
		echo "Install missing tools (Debian/Ubuntu):"; \
		echo "  sudo apt-get install build-essential git gcc-riscv64-linux-gnu"; \
		exit 1; \
	fi; \
	echo "OK: all host tools present"
