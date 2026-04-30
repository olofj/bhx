# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# BRISC firmware build for the #66 Tensix-as-virtio-engine work.
#
# Two firmware variants share `start.S` and `link.ld`:
#   * `brisc-hello.bin`  — M1 (#67) bring-up smoke test. ~200 bytes.
#   * `brisc-virtio.bin` — M3 (#69) virtio-mmio register-file engine.
#
# Toolchain: sfpi GCC at /opt/tenstorrent/sfpi/compiler/bin (GCC 15.1
# targeting riscv-tt-elf, ilp32 ABI, RV32I default).
#
# Output: build/brisc-{hello,virtio}.bin — raw flat binaries, copied
# directly into tile L1 at offset 0. The Rust side embeds them via
# include_bytes! and writes them through the chip-side TLB.

TOOLCHAIN_BIN ?= /opt/tenstorrent/sfpi/compiler/bin
CC      := $(TOOLCHAIN_BIN)/riscv-tt-elf-gcc
OBJCOPY := $(TOOLCHAIN_BIN)/riscv-tt-elf-objcopy
SIZE    := $(TOOLCHAIN_BIN)/riscv-tt-elf-size

# RV32I + Zicsr is the lowest common denominator across BRISC/NCRISC/
# TRISC. Bump -march here when something past hello-world needs
# atomics (Zaamo) or bit-manip (Zba/Zbb).
CFLAGS := \
    -march=rv32i_zicsr \
    -mabi=ilp32 \
    -O3 \
    -ffreestanding \
    -fno-builtin \
    -fno-stack-protector \
    -fno-pic \
    -fdata-sections \
    -ffunction-sections \
    -mno-relax \
    -Wall -Wextra -Werror \
    -Wno-array-bounds \
    -Iinclude \
    -std=c11

# Firmware build identity (4 bytes embedded in `BRISC_VIRTIO_FW_VERSION`'s
# upper 24 bits — the low byte stays `TENSIX_PROTOCOL_VERSION`). The
# daemon's `adopt_running` path matches this against the chip's running
# firmware to detect drift across daemon-binary updates: if the daemon
# is rebuilt with new firmware sources but the chip's BRISC is still
# running the prior build, adoption refuses and the operator gets a
# clear error pointing at `tt-smi -r`.
#
# Algorithm: if the tree is clean against HEAD for our sources, use
# `git log` short hash of the most-recent commit touching them.
# Otherwise fall back to a sha256 prefix of the source bytes — that
# also catches the no-git-checkout case (tarball builds, etc.) at the
# cost of changing whenever any whitespace shifts. See #122 for the
# tarball-build follow-up.
FW_SOURCES := $(sort hello.c link.ld start.S virtio.c $(wildcard include/*.h))

GIT_TREE_CLEAN := $(shell git diff --quiet HEAD -- $(FW_SOURCES) 2>/dev/null && \
                          git diff --cached --quiet HEAD -- $(FW_SOURCES) 2>/dev/null && \
                          echo yes)

ifeq ($(GIT_TREE_CLEAN),yes)
FW_BUILD_ID := $(shell git log -1 --pretty=format:%h --abbrev=6 -- $(FW_SOURCES) 2>/dev/null)
else
FW_BUILD_ID := $(shell cat $(FW_SOURCES) 2>/dev/null | sha256sum | head -c 6)
endif

ifeq ($(strip $(FW_BUILD_ID)),)
FW_BUILD_ID := 000000
endif

CFLAGS += -DBRISC_VIRTIO_FW_BUILD_ID=0x$(FW_BUILD_ID)u

OUT_DIR := build

LDFLAGS_BASE := \
    -nostdlib \
    -static \
    -Wl,--gc-sections \
    -Wl,-T,link.ld \
    -Wl,--no-warn-rwx-segments

# Per-variant size budgets. Hello is tiny (init stub + counter loop);
# virtio holds the static reg-file initializer + main poll loop +
# dispatchers. Both are well under the 1.5 MiB L1 hard cap.
HELLO_MAX_SIZE  := 4096
VIRTIO_MAX_SIZE := 16384

HELLO_ELF  := $(OUT_DIR)/brisc-hello.elf
HELLO_BIN  := $(OUT_DIR)/brisc-hello.bin
HELLO_OBJS := $(OUT_DIR)/start.o $(OUT_DIR)/hello.o

VIRTIO_ELF  := $(OUT_DIR)/brisc-virtio.elf
VIRTIO_BIN  := $(OUT_DIR)/brisc-virtio.bin
VIRTIO_OBJS := $(OUT_DIR)/start.o $(OUT_DIR)/virtio.o

all: $(HELLO_BIN) $(VIRTIO_BIN)

$(OUT_DIR):
	@mkdir -p $@

# Compile rules. Order doesn't matter; each .o gets its own command.
$(OUT_DIR)/start.o: start.S | $(OUT_DIR)
	$(CC) $(CFLAGS) -c -o $@ $<

$(OUT_DIR)/hello.o: hello.c | $(OUT_DIR)
	$(CC) $(CFLAGS) -c -o $@ $<

$(OUT_DIR)/virtio.o: virtio.c include/virtio_layout.h include/uart_layout.h include/tensix_proto.h | $(OUT_DIR)
	$(CC) $(CFLAGS) -c -o $@ $<

# Link + objcopy + size-check, parametrized by variant.
$(HELLO_ELF): $(HELLO_OBJS) link.ld | $(OUT_DIR)
	$(CC) $(LDFLAGS_BASE) -Wl,-Map=$(OUT_DIR)/brisc-hello.map -o $@ $(HELLO_OBJS)

$(VIRTIO_ELF): $(VIRTIO_OBJS) link.ld | $(OUT_DIR)
	$(CC) $(LDFLAGS_BASE) -Wl,-Map=$(OUT_DIR)/brisc-virtio.map -o $@ $(VIRTIO_OBJS)

$(HELLO_BIN): $(HELLO_ELF)
	$(OBJCOPY) -O binary $< $@
	@actual=$$(stat -c %s $@); \
	if [ $$actual -gt $(HELLO_MAX_SIZE) ]; then \
		echo "ERROR: $@ is $$actual bytes, exceeds $(HELLO_MAX_SIZE) byte budget" >&2; \
		exit 1; \
	fi; \
	echo "$@: $$actual bytes (budget $(HELLO_MAX_SIZE))"
	@$(SIZE) $(HELLO_ELF)

$(VIRTIO_BIN): $(VIRTIO_ELF)
	$(OBJCOPY) -O binary $< $@
	@actual=$$(stat -c %s $@); \
	if [ $$actual -gt $(VIRTIO_MAX_SIZE) ]; then \
		echo "ERROR: $@ is $$actual bytes, exceeds $(VIRTIO_MAX_SIZE) byte budget" >&2; \
		exit 1; \
	fi; \
	echo "$@: $$actual bytes (budget $(VIRTIO_MAX_SIZE))"
	@$(SIZE) $(VIRTIO_ELF)

clean:
	rm -rf $(OUT_DIR)

.PHONY: all clean
