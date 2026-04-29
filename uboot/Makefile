# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# Build U-Boot for the Blackhole L2CPU guest. Source materials (config
# fragment, README) are checked in; build artifacts (dl/, the extracted
# u-boot-<ver>/ source tree, output blob) are gitignored.
#
# The boot chain after this lands: OpenSBI fw_jump -> U-Boot (S-mode) ->
# kernel + initramfs. U-Boot replaces the raw `Image` payload at
# KERNEL_OFFSET (0x20_0000 from L2CPU DRAM start). See #44 + #45 for the
# overall plan.
#
# Usage:
#   make             # download + verify + build u-boot.bin
#   make rebuild     # re-run the build without re-extracting
#   make clean       # drop the extracted source tree; keep dl/
#   make distclean   # drop everything including dl/

UBOOT_VERSION := 2026.04
UBOOT_TARBALL := u-boot-$(UBOOT_VERSION).tar.bz2
UBOOT_URL := https://ftp.denx.de/pub/u-boot/$(UBOOT_TARBALL)

TOPDIR := $(CURDIR)
DL_DIR := $(TOPDIR)/dl
UB_SRCDIR := $(TOPDIR)/u-boot-$(UBOOT_VERSION)

# Our defconfig is `qemu-riscv64_smode_defconfig` (upstream) plus the
# fragment in tt-bh.config that turns on SBI console + virtio + EFI for
# the daemon-emulated platform. The fragment is intentionally small
# enough that bumping U-Boot only re-runs the qemu defconfig + merge,
# no per-version patches.
BASE_DEFCONFIG := qemu-riscv64_smode_defconfig
FRAGMENT := $(TOPDIR)/tt-bh.config

# Final blob landed by the build.
UBOOT_BIN := $(UB_SRCDIR)/u-boot.bin
# Stable convenience path. The Makefile maintains a symlink here so
# `tt-bh-linux boot --uboot uboot/u-boot.bin` doesn't need to
# know the U-Boot version directory name.
UBOOT_LINK := $(TOPDIR)/u-boot.bin

# Cross-compiler. The Debian package gcc-riscv64-linux-gnu provides this
# as `riscv64-linux-gnu-`. The buildroot tree's own `riscv64-buildroot-
# linux-gnu-` works too if you'd rather not install a host package, but
# requires running `make -C tests/rootfs` first to bring up the
# toolchain.
CROSS_COMPILE ?= riscv64-linux-gnu-

# Reproducibility: same workflow as tests/rootfs/. SOURCE_DATE_EPOCH
# pinned to the most recent commit touching uboot/, anchored at
# $(TOPDIR) so unrelated commits don't bump the cache; ?= so the env
# wins for CI-style fixed-epoch builds. See #39.
SOURCE_DATE_EPOCH ?= $(shell git -C $(TOPDIR) log -1 --format=%ct -- $(TOPDIR) 2>/dev/null || echo 0)
export SOURCE_DATE_EPOCH

.PHONY: all rebuild clean distclean help check-deps

all: $(UBOOT_BIN)
	ln -sfr $(UBOOT_BIN) $(UBOOT_LINK)
	@echo
	@echo "==> Build complete:"
	@ls -lh $(UBOOT_BIN)
	@echo
	@echo "    Use as: tt-bh-linux boot -l 0 --uboot $(UBOOT_LINK)"

help:
	@echo "Targets:"
	@echo "  all (default)    Build u-boot.bin. First run downloads + extracts upstream tree."
	@echo "  rebuild          Re-run the build without re-extracting."
	@echo "  clean            Drop the extracted source tree. Keeps dl/."
	@echo "  distclean        Drop everything including dl/."
	@echo "  check-deps       Verify host tools needed by the U-Boot build are present."
	@echo
	@echo "Pinned U-Boot version: $(UBOOT_VERSION)"
	@echo "Cross-compiler:        $(CROSS_COMPILE)gcc"

PATCH_FILES := $(sort $(wildcard $(TOPDIR)/patches/*.patch))

# Download + verify + extract the U-Boot tarball.
$(UB_SRCDIR)/Makefile: $(DL_DIR)/$(UBOOT_TARBALL) $(TOPDIR)/patches/serial_sbi_dm.c $(PATCH_FILES)
	@echo "==> Verifying $(UBOOT_TARBALL)"
	@cd $(DL_DIR) && sha256sum -c $(TOPDIR)/sha256sums --ignore-missing
	@echo "==> Extracting"
	tar -xf $(DL_DIR)/$(UBOOT_TARBALL) -C $(TOPDIR)
	@echo "==> Installing tt-bh DM SBI serial driver"
	cp $(TOPDIR)/patches/serial_sbi_dm.c $(UB_SRCDIR)/drivers/serial/
	@# Append Kconfig entry + Makefile rule. Idempotent: only add if
	@# not already present (so re-running this step on an existing
	@# tree doesn't duplicate.)
	@grep -q SERIAL_SBI_DM $(UB_SRCDIR)/drivers/serial/Kconfig || \
		printf '\nconfig SERIAL_SBI_DM\n\tbool "DM serial driver wrapping SBI DBCN"\n\tdepends on DM_SERIAL && RISCV_SMODE\n\thelp\n\t  tt-bh downstream patch — see uboot/patches/serial_sbi_dm.c.\n' >> $(UB_SRCDIR)/drivers/serial/Kconfig
	@grep -q SERIAL_SBI_DM $(UB_SRCDIR)/drivers/serial/Makefile || \
		echo 'obj-$$(CONFIG_SERIAL_SBI_DM) += serial_sbi_dm.o' >> $(UB_SRCDIR)/drivers/serial/Makefile
	@# Apply any *.patch files (alphabetical order). The patch tool's
	@# -N flag skips already-applied hunks so a re-extract that left
	@# stale .config behind doesn't double-patch.
	@for p in $(PATCH_FILES); do \
		echo "==> Applying $$(basename $$p)"; \
		(cd $(UB_SRCDIR) && patch -p1 -N --quiet < $$p) || true; \
	done
	@touch $@

$(DL_DIR)/$(UBOOT_TARBALL):
	@mkdir -p $(DL_DIR)
	@echo "==> Downloading $(UBOOT_URL)"
	wget -q -O $@.tmp $(UBOOT_URL)
	@mv $@.tmp $@

# Configure step: start from upstream's qemu-riscv64_smode_defconfig,
# overlay our fragment, finalize with olddefconfig so any new symbols
# get sane defaults.
$(UB_SRCDIR)/.config: $(UB_SRCDIR)/Makefile $(FRAGMENT)
	@echo "==> Generating .config from $(BASE_DEFCONFIG) + $(FRAGMENT)"
	$(MAKE) -C $(UB_SRCDIR) $(BASE_DEFCONFIG)
	$(UB_SRCDIR)/scripts/kconfig/merge_config.sh -m -O $(UB_SRCDIR) \
		$(UB_SRCDIR)/.config $(FRAGMENT)
	$(MAKE) -C $(UB_SRCDIR) olddefconfig

$(UBOOT_BIN): $(UB_SRCDIR)/.config
	$(MAKE) -C $(UB_SRCDIR) CROSS_COMPILE=$(CROSS_COMPILE)

rebuild:
	$(MAKE) -C $(UB_SRCDIR) CROSS_COMPILE=$(CROSS_COMPILE)
	ln -sfr $(UBOOT_BIN) $(UBOOT_LINK)

clean:
	rm -rf $(UB_SRCDIR)
	rm -f $(UBOOT_LINK)

distclean: clean
	rm -rf $(DL_DIR)

check-deps:
	@missing=; \
	for tool in wget tar make $(CROSS_COMPILE)gcc bison flex; do \
		if ! command -v $$tool >/dev/null; then \
			echo "MISSING: $$tool"; \
			missing=1; \
		fi; \
	done; \
	if [ -n "$$missing" ]; then \
		echo; \
		echo "Install missing tools (Debian/Ubuntu):"; \
		echo "  sudo apt-get install build-essential wget gcc-riscv64-linux-gnu bison flex"; \
		exit 1; \
	fi; \
	echo "OK: all host tools present"
