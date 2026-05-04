# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# Build OpenSBI for the Blackhole L2CPU. Source is upstream
# riscv-software-src/opensbi at a pinned tag; build artifacts (dl/,
# extracted opensbi-<ver>/, fw_jump.bin symlink) are gitignored.
#
# The boot chain: OpenSBI fw_jump (M-mode) -> kernel `Image` directly
# (legacy mode), or -> U-Boot (S-mode) -> kernel + initramfs (#44).
# OpenSBI lands at L2CPU DRAM start; the kernel/U-Boot payload is
# placed at FW_JUMP_OFFSET (+0x20_0000) and the DTB at
# FW_JUMP_FDT_OFFSET (+0x10_0000). Those offsets match the addresses
# in `boot::OPENSBI_OFFSET` + `boot_image::*_OFFSET` on the Rust side.
#
# Tenstorrent maintains a `tt-blackhole` branch on their opensbi fork.
# Its functional delta against upstream (v1.7) is the debug_descriptor
# at offset 0x80 of fw_jump.bin plus a virtual-UART driver — load-
# bearing for daemon warm-resume (chip_console::probe_warm_resume).
# We pin upstream and apply the Tenstorrent diff as a patch in
# patches/. The build applies *.patch alphabetically, idempotently.
#
# Usage:
#   make             # download + verify + build fw_jump.bin
#   make rebuild     # re-run the build without re-extracting
#   make clean       # drop the extracted source tree; keep dl/
#   make distclean   # drop everything including dl/

OPENSBI_VERSION := 1.7
OPENSBI_TARBALL := opensbi-$(OPENSBI_VERSION).tar.gz
OPENSBI_URL := https://github.com/riscv-software-src/opensbi/archive/refs/tags/v$(OPENSBI_VERSION).tar.gz

TOPDIR := $(CURDIR)
DL_DIR := $(TOPDIR)/dl
SB_SRCDIR := $(TOPDIR)/opensbi-$(OPENSBI_VERSION)

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
	@echo "  all (default)    Build fw_jump.bin. First run downloads + extracts upstream tree."
	@echo "  rebuild          Re-run the build without re-extracting."
	@echo "  clean            Drop the extracted source tree. Keeps dl/."
	@echo "  distclean        Drop everything including dl/."
	@echo "  check-deps       Verify host tools needed by the OpenSBI build are present."
	@echo
	@echo "Pinned OpenSBI version: $(OPENSBI_VERSION)"
	@echo "Cross-compiler:         $(CROSS_COMPILE)gcc"

PATCH_FILES := $(sort $(wildcard $(TOPDIR)/patches/*.patch))

# Download + verify + extract the OpenSBI tarball.
$(SB_SRCDIR)/Makefile: $(DL_DIR)/$(OPENSBI_TARBALL) $(PATCH_FILES)
	@echo "==> Verifying $(OPENSBI_TARBALL)"
	@cd $(DL_DIR) && sha256sum -c $(TOPDIR)/sha256sums --ignore-missing
	@echo "==> Extracting"
	@# Reset the source dir before extracting so patches always apply
	@# against a known-clean upstream tree. Idempotency via -N is
	@# unreliable for create-new-file hunks: tar -xf doesn't overwrite
	@# files that aren't in the tarball, so a patch-created file like
	@# lib/utils/serial/virtual-uart.c survives a re-extract; patch -N
	@# then double-applies and the build fails with "redefinition" errors.
	rm -rf $(SB_SRCDIR)
	tar -xf $(DL_DIR)/$(OPENSBI_TARBALL) -C $(TOPDIR)
	@for p in $(PATCH_FILES); do \
		echo "==> Applying $$(basename $$p)"; \
		(cd $(SB_SRCDIR) && patch -p1 --quiet < $$p) || exit 1; \
	done
	@touch $@

$(DL_DIR)/$(OPENSBI_TARBALL):
	@mkdir -p $(DL_DIR)
	@echo "==> Downloading $(OPENSBI_URL)"
	wget -q -O $@.tmp $(OPENSBI_URL)
	@mv $@.tmp $@

$(FW_JUMP): $(SB_SRCDIR)/Makefile
	$(MAKE) -C $(SB_SRCDIR) CROSS_COMPILE=$(CROSS_COMPILE) $(SBI_FLAGS)

rebuild:
	$(MAKE) -C $(SB_SRCDIR) CROSS_COMPILE=$(CROSS_COMPILE) $(SBI_FLAGS)
	ln -sfr $(FW_JUMP) $(FW_JUMP_LINK)

clean:
	rm -rf $(SB_SRCDIR)
	rm -f $(FW_JUMP_LINK)

distclean: clean
	rm -rf $(DL_DIR)

check-deps:
	@missing=; \
	for tool in wget tar make $(CROSS_COMPILE)gcc; do \
		if ! command -v $$tool >/dev/null; then \
			echo "MISSING: $$tool"; \
			missing=1; \
		fi; \
	done; \
	if [ -n "$$missing" ]; then \
		echo; \
		echo "Install missing tools (Debian/Ubuntu):"; \
		echo "  sudo apt-get install build-essential wget gcc-riscv64-linux-gnu"; \
		exit 1; \
	fi; \
	echo "OK: all host tools present"
