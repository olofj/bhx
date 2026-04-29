# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# Compile blackhole-card.dts -> blackhole-card.dtb using cpp + dtc.
# Source files are vendored verbatim from the Tenstorrent Linux fork
# (see README.md for upstream commit SHAs and license). The build is
# trivial — two files, no external headers — but we still go through
# the standard cpp(1) pre-process step because the .dts uses #include
# "blackhole.dtsi".
#
# Usage:
#   make             # produce blackhole-card.dtb in this directory
#   make clean       # remove the build output
#   make check-deps  # verify cpp + dtc are present

DTS := blackhole-card.dts
DTB := blackhole-card.dtb

TOPDIR := $(CURDIR)

# Reproducibility — set but unused; cpp/dtc are deterministic for this
# input regardless. Pinned for parity with third_party/uboot and
# third_party/opensbi. See #39.
SOURCE_DATE_EPOCH ?= $(shell git -C $(TOPDIR) log -1 --format=%ct -- $(TOPDIR) 2>/dev/null || echo 0)
export SOURCE_DATE_EPOCH

.PHONY: all clean check-deps help

all: $(DTB)

help:
	@echo "Targets:"
	@echo "  all (default)    Compile $(DTS) -> $(DTB)."
	@echo "  clean            Remove build output."
	@echo "  check-deps       Verify cpp + dtc are present."

$(DTB): $(DTS) blackhole.dtsi
	@echo "==> cpp $(DTS) | dtc -> $@"
	cpp -nostdinc -I. -undef -x assembler-with-cpp $(DTS) | \
		dtc -I dts -O dtb -o $@
	@echo
	@echo "==> Build complete:"
	@ls -lh $@
	@echo
	@echo "    Use as: bhx boot -l 0 --dtb $(TOPDIR)/$@"

clean:
	rm -f $(DTB)

check-deps:
	@missing=; \
	for tool in cpp dtc; do \
		if ! command -v $$tool >/dev/null; then \
			echo "MISSING: $$tool"; \
			missing=1; \
		fi; \
	done; \
	if [ -n "$$missing" ]; then \
		echo; \
		echo "Install missing tools (Debian/Ubuntu):"; \
		echo "  sudo apt-get install cpp device-tree-compiler"; \
		exit 1; \
	fi; \
	echo "OK: all host tools present"
