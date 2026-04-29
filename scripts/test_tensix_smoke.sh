#!/usr/bin/env bash
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT

# Hardware smoke for the M1+M2+M3 Tensix-engine work (#66 sub-issues
# #67 #68 #69). Drives the existing debug subcommands and asserts on
# their output. Resets the card before and after to avoid leaving the
# chip in a wedged state from prior tests.
#
# Usage:
#   bash scripts/test_tensix_smoke.sh
#
# Requires:
#   - /dev/tenstorrent/<CARD> exists
#   - tt-smi installed under ~/.tenstorrent-venv/bin/
#   - cargo build done (or set BINARY to a prebuilt path)
#   - Daemon NOT running for the target card (debug subcommands
#     refuse to operate against a live daemon).
#
# Exits 0 on PASS, non-zero on regression.

set -uo pipefail

CARD="${CARD:-0}"
BINARY="${BINARY:-./target/debug/bhx}"
TT_VENV="${TT_VENV:-$HOME/.tenstorrent-venv/bin/activate}"

note() { echo "[tensix-smoke] $*"; }
fail() { echo "[tensix-smoke] FAIL: $*" >&2; exit 1; }

if [ ! -e "/dev/tenstorrent/${CARD}" ]; then
    fail "/dev/tenstorrent/${CARD} not present — no Blackhole card"
fi
if [ ! -x "$BINARY" ]; then
    fail "binary not found at $BINARY (build with 'cargo build')"
fi
# `daemon status` exits 0 in both cases (running/not-running), so we
# have to read the message. The not-running line is exactly
# "daemon: not running for card N"; anything else means the daemon
# is up.
DAEMON_STATUS=$("$BINARY" daemon status -t "$CARD" 2>&1)
if ! echo "$DAEMON_STATUS" | head -1 | grep -q "^daemon: not running"; then
    fail "daemon appears to be running for card $CARD — stop it first ($DAEMON_STATUS)"
fi

reset_card() {
    note "tt-smi -r"
    if [ -f "$TT_VENV" ]; then
        # shellcheck disable=SC1090
        ( . "$TT_VENV" && tt-smi -r >/dev/null 2>&1 ) || fail "tt-smi -r failed"
    else
        note "tt-smi venv not found at $TT_VENV — skipping reset"
    fi
}

# Make sure we always reset on the way out, even on failure.
trap 'reset_card || true' EXIT

reset_card

# --- M2 picker --------------------------------------------------------
note "M2: pick-tile"
PICK_OUT=$("$BINARY" debug pick-tile -t "$CARD" 2>&1) || fail "pick-tile errored: $PICK_OUT"
if ! echo "$PICK_OUT" | grep -qE '^[0-9]+ [0-9]+ \(.*\)$'; then
    fail "pick-tile output not in '<x> <y> (reason)' form: $PICK_OUT"
fi
note "  picker chose: $PICK_OUT"

note "M2: telemetry-dump"
DUMP_OUT=$("$BINARY" debug telemetry-dump -t "$CARD" 2>&1) || fail "telemetry-dump errored"
echo "$DUMP_OUT" | grep -q "EnabledTensixCol" || fail "telemetry dump missing EnabledTensixCol"
echo "$DUMP_OUT" | grep -q "NocTranslation"    || fail "telemetry dump missing NocTranslation"
echo "$DUMP_OUT" | grep -q "decoded working set" || fail "telemetry dump missing decoded set"
echo "$DUMP_OUT" | grep -q "picker would choose" || fail "telemetry dump missing picker output"
note "  telemetry-dump prints expected sections"

# --- M1 hello-world ---------------------------------------------------
note "M1: tensix-hello (default --duration=2)"
HELLO_OUT=$("$BINARY" debug tensix-hello -t "$CARD" --duration 2 2>&1)
HELLO_RC=$?
if [ $HELLO_RC -ne 0 ] || ! echo "$HELLO_OUT" | grep -q "PASS:"; then
    echo "$HELLO_OUT" >&2
    fail "tensix-hello did not PASS (exit=$HELLO_RC)"
fi
note "  $(echo "$HELLO_OUT" | grep PASS:)"

# Card reset between hello and virtio — hello leaves BRISC running a
# tight counter loop, and we want a clean state for the M3 firmware
# load.
reset_card

# --- M3 virtio engine -------------------------------------------------
note "M3: tensix-virtio"
VIRTIO_OUT=$("$BINARY" debug tensix-virtio -t "$CARD" 2>&1)
VIRTIO_RC=$?
if [ $VIRTIO_RC -ne 0 ] || ! echo "$VIRTIO_OUT" | tail -1 | grep -q "PASS"; then
    echo "$VIRTIO_OUT" >&2
    fail "tensix-virtio did not PASS (exit=$VIRTIO_RC)"
fi
echo "$VIRTIO_OUT" | grep -q "16/16 slots show correct static reg state" \
    || fail "missing 16/16 slot init confirmation"
echo "$VIRTIO_OUT" | grep -q "STATUS state machine PASS" \
    || fail "STATUS state machine did not pass"
echo "$VIRTIO_OUT" | grep -q "QUEUE_SEL multiplexer PASS" \
    || fail "QUEUE_SEL multiplexer did not pass"
note "  STATUS state machine + QUEUE_SEL multiplexer + 16/16 reg files all confirmed"

note "PASS: M1+M2+M3 hardware smoke clean on card $CARD"
