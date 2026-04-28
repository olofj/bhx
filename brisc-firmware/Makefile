# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# Hello-world BRISC firmware build for issue #67 (M1).
#
# Toolchain: sfpi GCC at /opt/tenstorrent/sfpi/compiler/bin (GCC 15.1
# targeting riscv-tt-elf, ilp32 ABI, RV32I default).
#
# Output: build/brisc-hello.bin (raw flat binary, copied directly into
# tile L1 starting at offset 0). The Rust side embeds it via
# include_bytes! and writes it via the chip-side TLB.
#
# Size budget: 4 KiB. The firmware stub is < 256 bytes; that headroom
# leaves room for M3's register-file emulation without revisiting
# this Makefile.

TOOLCHAIN_BIN ?= /opt/tenstorrent/sfpi/compiler/bin
CC      := $(TOOLCHAIN_BIN)/riscv-tt-elf-gcc
OBJCOPY := $(TOOLCHAIN_BIN)/riscv-tt-elf-objcopy
SIZE    := $(TOOLCHAIN_BIN)/riscv-tt-elf-size

# RV32I + Zicsr is the lowest common denominator across BRISC/NCRISC/
# TRISC. The hello-world firmware uses no extension instructions;
# bumping -march here when M3 wants atomics is a one-line change.
CFLAGS := \
    -march=rv32i_zicsr \
    -mabi=ilp32 \
    -Os \
    -ffreestanding \
    -fno-builtin \
    -fno-stack-protector \
    -fno-pic \
    -fdata-sections \
    -ffunction-sections \
    -mno-relax \
    -Wall -Wextra -Werror \
    -Wno-array-bounds \
    -std=c11

OUT_DIR := build
ELF     := $(OUT_DIR)/brisc-hello.elf
BIN     := $(OUT_DIR)/brisc-hello.bin
OBJS    := $(OUT_DIR)/start.o $(OUT_DIR)/main.o

LDFLAGS := \
    -nostdlib \
    -static \
    -Wl,--gc-sections \
    -Wl,-T,link.ld \
    -Wl,-Map=$(OUT_DIR)/brisc-hello.map \
    -Wl,--no-warn-rwx-segments

# Hard ceiling on the .bin payload. M1 hello-world is far below this;
# the limit is chosen so M3's register-file firmware also fits without
# revisiting the Makefile.
MAX_SIZE := 4096

all: $(BIN)

$(OUT_DIR):
	@mkdir -p $@

$(OUT_DIR)/start.o: start.S | $(OUT_DIR)
	$(CC) $(CFLAGS) -c -o $@ $<

$(OUT_DIR)/main.o: main.c | $(OUT_DIR)
	$(CC) $(CFLAGS) -c -o $@ $<

$(ELF): $(OBJS) link.ld | $(OUT_DIR)
	$(CC) $(LDFLAGS) -o $@ $(OBJS)

$(BIN): $(ELF)
	$(OBJCOPY) -O binary $< $@
	@actual=$$(stat -c %s $@); \
	if [ $$actual -gt $(MAX_SIZE) ]; then \
		echo "ERROR: $@ is $$actual bytes, exceeds $(MAX_SIZE) byte budget" >&2; \
		exit 1; \
	fi; \
	echo "$@: $$actual bytes (budget $(MAX_SIZE))"
	@$(SIZE) $(ELF)

clean:
	rm -rf $(OUT_DIR)

.PHONY: all clean
