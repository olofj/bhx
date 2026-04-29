#!/usr/bin/env bash
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT

#
# Buildroot post-build hook. Runs after the main package install but
# before image generation, with $TARGET_DIR set to the staging tree
# that becomes the rootfs and $HOST_DIR set to buildroot's host
# tooling output (cross-compiler under host/bin/).
#
# Today's only job: cross-compile the byte-echo helper from #36 so
# the console roundtrip-latency benchmark in scripts/bench/console.py
# can use unbuffered write(2) instead of busybox printf's per-byte
# stdio buffering.
#
# Adding more on-target tooling later? Keep them small and atomic.
# Anything that pulls a non-trivial dependency tree should go through
# a buildroot package, not this hook.

set -euo pipefail

TARGET_DIR="${TARGET_DIR:?TARGET_DIR is required}"
HOST_DIR="${HOST_DIR:?HOST_DIR is required}"

# Buildroot's cross-toolchain naming is fixed by the BR2_TOOLCHAIN_*
# config; the riscv64 build we use here always lands these symlinks.
CC="${HOST_DIR}/bin/riscv64-buildroot-linux-gnu-gcc"

# `dirname` of this script — buildroot invokes us with cwd inside the
# buildroot source dir, so source-relative paths need to anchor
# explicitly.
SRC_DIR="$(cd "$(dirname "$0")" && pwd)"

mkdir -p "$TARGET_DIR/usr/local/bin"
"$CC" -O2 -Wall -Wextra -Werror -static \
    -o "$TARGET_DIR/usr/local/bin/echo-byte" \
    "$SRC_DIR/echo-byte.c"

# Strip — small wins matter on a 96 MiB image.
"${HOST_DIR}/bin/riscv64-buildroot-linux-gnu-strip" \
    "$TARGET_DIR/usr/local/bin/echo-byte"
