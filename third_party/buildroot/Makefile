# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# Build a minimal riscv64 rootfs.ext4 for hardware soaks, using a pinned
# buildroot release. Source materials (defconfig, overlay) are checked in;
# build artifacts (output/, dl/, build/) are gitignored.
#
# Usage:
#   make             # build the rootfs (first run downloads + builds buildroot ~30 min)
#   make rebuild     # clean buildroot's images dir and rebuild
#   make clean       # remove build artifacts; keep dl/ cache
#   make distclean   # remove everything including dl/

BUILDROOT_VERSION := 2026.02.1
BUILDROOT_TARBALL := buildroot-$(BUILDROOT_VERSION).tar.gz
BUILDROOT_URL := https://buildroot.org/downloads/$(BUILDROOT_TARBALL)
BUILDROOT_SHA256 := \
$(shell cat sha256sums | grep $(BUILDROOT_TARBALL) | awk '{print $$1}')

# Where buildroot source lives, where it builds, where artifacts land.
# Buildroot insists on its in-tree `output/` dir; using `O=<dir>` only
# half-works (config goes there but the build still uses in-tree
# paths and races itself). Lean into the default.
TOPDIR := $(CURDIR)
DL_DIR := $(TOPDIR)/dl
BR_SRCDIR := $(TOPDIR)/buildroot-$(BUILDROOT_VERSION)
OUTPUT_DIR := $(BR_SRCDIR)/output
DEFCONFIG := $(TOPDIR)/buildroot.config
OVERLAY := $(TOPDIR)/overlay

ROOTFS_EXT4 := $(OUTPUT_DIR)/images/rootfs.ext4
# Stable convenience path. The Makefile maintains a symlink here so soak
# scripts and ad-hoc invocations don't need to know the buildroot version
# directory name.
ROOTFS_LINK := $(TOPDIR)/rootfs.ext4

.PHONY: all rebuild clean distclean help check-deps

all: $(ROOTFS_EXT4)
	@echo
	@echo "==> Build complete:"
	@ls -lh $(ROOTFS_EXT4)
	@echo
	@echo "    Use as: bhx boot -l 0 -d $(ROOTFS_EXT4)"

help:
	@echo "Targets:"
	@echo "  all (default)    Build rootfs.ext4. First run downloads + builds buildroot."
	@echo "  rebuild          Clear images/, rebuild from existing build dir."
	@echo "  clean            Remove output/ + buildroot source tree. Keeps dl/."
	@echo "  distclean        Remove everything including dl/."
	@echo "  check-deps       Verify host tools needed by buildroot are present."
	@echo
	@echo "Pinned buildroot version: $(BUILDROOT_VERSION)"

# Download + verify + extract the buildroot tarball.
$(BR_SRCDIR)/Makefile: $(DL_DIR)/$(BUILDROOT_TARBALL)
	@echo "==> Verifying $(BUILDROOT_TARBALL)"
	@cd $(DL_DIR) && sha256sum -c $(TOPDIR)/sha256sums --ignore-missing
	@echo "==> Extracting"
	tar -xf $(DL_DIR)/$(BUILDROOT_TARBALL) -C $(TOPDIR)
	@touch $@

$(DL_DIR)/$(BUILDROOT_TARBALL):
	@mkdir -p $(DL_DIR)
	@echo "==> Downloading $(BUILDROOT_URL)"
	wget -q -O $@.tmp $(BUILDROOT_URL)
	@mv $@.tmp $@

# Reproducibility: pin SOURCE_DATE_EPOCH to the timestamp of the last
# commit that touched third_party/buildroot/. Combined with BR2_REPRODUCIBLE=y
# in buildroot.config this makes rootfs.ext4 byte-identical across
# rebuilds from the same commit (mkfs.ext4 derives the UUID and inode
# timestamps from this; gcc honors it for __DATE__/__TIME__). Anchored
# at $(TOPDIR) rather than HEAD so unrelated repo commits don't bump
# the cache; falls back to 0 if not in a git repo (release tarballs,
# CI builds outside a checkout). See #39.
SOURCE_DATE_EPOCH ?= $(shell git -C $(TOPDIR) log -1 --format=%ct -- $(TOPDIR) 2>/dev/null || echo 0)

# Configure + build.  Buildroot's `make` is invoked from its own
# directory; we set BR2_DEFCONFIG, BR2_EXTERNAL_*, and dl/output dirs
# via env so the source tree itself doesn't get polluted.
BR_MAKE_ENV := \
	BR2_DEFCONFIG=$(DEFCONFIG) \
	BR2_DL_DIR=$(DL_DIR) \
	SOURCE_DATE_EPOCH=$(SOURCE_DATE_EPOCH)

# Configure step: generates buildroot's in-tree .config from our
# defconfig. The defconfig encodes BR2_ROOTFS_OVERLAY as a relative
# path (`../overlay`) which buildroot resolves against its own working
# directory (BR_SRCDIR). That avoids hard-coding any contributor-
# specific absolute path here.
BR_DOTCONFIG := $(BR_SRCDIR)/.config

$(BR_DOTCONFIG): $(DEFCONFIG) $(BR_SRCDIR)/Makefile
	$(BR_MAKE_ENV) $(MAKE) -C $(BR_SRCDIR) defconfig

$(ROOTFS_EXT4): $(BR_DOTCONFIG) $(shell find $(OVERLAY) -type f 2>/dev/null)
	$(BR_MAKE_ENV) $(MAKE) -C $(BR_SRCDIR)
	# Stable convenience symlink — soak scripts look for this path
	# without needing to know the buildroot version directory name.
	ln -sfr $(ROOTFS_EXT4) $(ROOTFS_LINK)

rebuild:
	rm -rf $(OUTPUT_DIR)/images
	$(BR_MAKE_ENV) $(MAKE) -C $(BR_SRCDIR)
	ln -sfr $(ROOTFS_EXT4) $(ROOTFS_LINK)

clean:
	rm -rf $(BR_SRCDIR)

distclean: clean
	rm -rf $(DL_DIR)

check-deps:
	@for tool in wget tar make gcc g++ patch cpio bc rsync sed which file unzip; do \
		if ! command -v $$tool >/dev/null; then \
			echo "MISSING: $$tool"; \
			missing=1; \
		fi; \
	done; \
	if [ -n "$$missing" ]; then \
		echo; \
		echo "Install missing tools (Debian/Ubuntu):"; \
		echo "  sudo apt-get install build-essential wget cpio rsync bc unzip file"; \
		exit 1; \
	fi
	@echo "OK: all host tools present"
