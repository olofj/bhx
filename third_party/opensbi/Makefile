# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# Build OpenSBI for the Blackhole L2CPU. Source is the bhx downstream
# fork at github.com/olofj/opensbi on the `bhx` branch — pinned to a
# specific commit SHA. Cloned into opensbi-src/ (gitignored) by this
# Makefile; build artifacts (opensbi-src/, fw_jump.bin symlink) are
# all gitignored.
#
# The fork is a real branch with one commit per bhx feature on top of
# upstream v1.8.1: tenstorrent debug_descriptor + virtual UART, bhx
# purgatory, Freisen v3 2/3 (scalar ISA emu), bhx PMU hook + publish
# struct, Freisen v3 3/3 (Zvbb emu), defconfig + PMU wiring for Zvbb.
# Bumping the upstream base: rebase the bhx branch onto a newer
# upstream tag, then bump OPENSBI_SHA / OPENSBI_VERSION here.
#
# Boot chain: OpenSBI fw_jump (M-mode) -> kernel `Image` directly
# (legacy mode), or -> U-Boot (S-mode) -> kernel + initramfs (#44).
# OpenSBI lands at L2CPU DRAM start; the kernel/U-Boot payload is
# placed at FW_JUMP_OFFSET (+0x20_0000) and the DTB at
# FW_JUMP_FDT_OFFSET (+0x10_0000). Those offsets match the addresses
# in `boot::OPENSBI_OFFSET` + `boot_image::*_OFFSET` on the Rust side.
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

# Pinned bhx-fork commit. The upstream base version is informational;
# OPENSBI_SHA is what's actually checked out.
OPENSBI_VERSION := 1.8.1+bhx
OPENSBI_SHA := 9c5c27f5
OPENSBI_REPO := https://github.com/olofj/opensbi.git
OPENSBI_BRANCH := bhx

# Allow a local override: a developer working on the fork itself can
# point at their working tree without push-roundtripping. Default to
# the public URL above. Example:
#   make OPENSBI_REPO=$HOME/bh/opensbi-bhx
OPENSBI_REPO := $(OPENSBI_REPO)

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
	@echo "  all (default)    Build fw_jump.bin. First run clones the bhx fork."
	@echo "  rebuild          Re-run the build without re-cloning."
	@echo "  clean            Drop the source tree."
	@echo "  distclean        Alias of clean."
	@echo "  check-deps       Verify host tools needed by the OpenSBI build are present."
	@echo
	@echo "Pinned OpenSBI:   $(OPENSBI_VERSION) ($(OPENSBI_SHA))"
	@echo "Source repo:      $(OPENSBI_REPO) [$(OPENSBI_BRANCH)]"
	@echo "Cross-compiler:   $(CROSS_COMPILE)gcc"

# Clone + checkout. The marker file is the upstream Makefile in the
# source tree — its presence + mtime determines whether we need to
# re-fetch.
$(SB_SRCDIR)/Makefile:
	rm -rf $(SB_SRCDIR)
	@echo "==> Cloning $(OPENSBI_REPO) ($(OPENSBI_BRANCH) @ $(OPENSBI_SHA))"
	git clone --quiet --branch $(OPENSBI_BRANCH) $(OPENSBI_REPO) $(SB_SRCDIR)
	@echo "==> Checking out $(OPENSBI_SHA)"
	git -C $(SB_SRCDIR) checkout --quiet $(OPENSBI_SHA)
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
