# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# Top-level convenience Makefile.
#
# `make`           - build bhx (release) + the three firmware artifacts in-tree.
# `make install`   - install bhx + firmware under $(PREFIX) (default ~/.local).
#                    Lays out: $(PREFIX)/bin/bhx and $(PREFIX)/share/bhx/firmware/{u-boot.bin,fw_jump.bin,blackhole-card.dtb}.
# `make uninstall` - remove the installed bhx binary and firmware directory.
# `make clean`     - cargo clean + clean each third_party/ build tree.
#
# Cargo and the third_party/ Makefiles are all incremental, so re-running
# `make` or `make install` is cheap.

PREFIX       ?= $(HOME)/.local
BIN_DIR      := $(PREFIX)/bin
DATA_DIR     := $(PREFIX)/share/bhx
FIRMWARE_DIR := $(DATA_DIR)/firmware

UBOOT_BIN    := third_party/uboot/u-boot.bin
FW_JUMP_BIN  := third_party/opensbi/fw_jump.bin
DTB_BIN      := third_party/dtb/blackhole-card.dtb

.PHONY: all build firmware install uninstall clean check-deps

all: build

build: firmware
	cargo build --release

firmware:
	$(MAKE) -C third_party/uboot
	$(MAKE) -C third_party/opensbi
	$(MAKE) -C third_party/dtb

check-deps:
	$(MAKE) -C third_party/uboot   check-deps
	$(MAKE) -C third_party/opensbi check-deps
	$(MAKE) -C third_party/dtb     check-deps

install: build
	cargo install --path . --root $(PREFIX)
	install -d -m 0755 $(FIRMWARE_DIR)
	install -m 0644 $(UBOOT_BIN)   $(FIRMWARE_DIR)/u-boot.bin
	install -m 0644 $(FW_JUMP_BIN) $(FIRMWARE_DIR)/fw_jump.bin
	install -m 0644 $(DTB_BIN)     $(FIRMWARE_DIR)/blackhole-card.dtb
	@echo
	@echo "  bhx installed to     $(BIN_DIR)/bhx"
	@echo "  firmware installed to $(FIRMWARE_DIR)/"
	@echo
	@echo "  ensure $(BIN_DIR) is on your PATH"

uninstall:
	rm -f $(BIN_DIR)/bhx
	rm -rf $(DATA_DIR)

clean:
	cargo clean
	-$(MAKE) -C third_party/uboot   clean
	-$(MAKE) -C third_party/opensbi clean
	-$(MAKE) -C third_party/dtb     clean
