#!/usr/bin/env bash
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT

#
# Stress-test the daemon's warm-resume path.
#
# Plan:
#   1. tt-smi -r (clean chip state).
#   2. Start daemon, cold-boot L2CPU 0 with disk + net, verify Running.
#   3. Loop N iterations:
#        a. daemon stop  -> chip keeps running
#        b. daemon start -> probe should detect released core, warm-resume
#                           should adopt it, status should report Running
#        c. assert status line for L2CPU 0 shows "Running"
#        d. grep the daemon log for the expected warm-resume log lines
#   4. daemon stop, final status check.
#
# Each iteration that fails an assertion exits the script with status 1;
# successful completion exits 0 and prints a summary.
#
# Env:
#   ITERATIONS  how many stop/start cycles (default 5)
#   BINARY      path to bhx (default ./target/debug/bhx)
#   LOG_FILE    daemon log path (default ./daemon-card0.log)
#   CARD        tenstorrent device index (default 0)
#   L2CPU       core index to exercise (default 0)

set -euo pipefail

ITERATIONS=${ITERATIONS:-5}
BINARY=${BINARY:-./target/debug/bhx}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
CARD=${CARD:-0}
L2CPU=${L2CPU:-0}

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "[soak] $*"; }

# Resolve the rootfs path. Priority: ROOTFS env > buildroot test rootfs >
# ./rootfs.ext4 (the legacy `image pull debian` location).
if [ -z "${ROOTFS:-}" ]; then
    if [ -e tests/rootfs/rootfs.ext4 ]; then
        ROOTFS=tests/rootfs/rootfs.ext4
    elif [ -e rootfs.ext4 ]; then
        ROOTFS=rootfs.ext4
    fi
fi

cleanup() {
    "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Sanity checks -------------------------------------------------------------
[ -x "$BINARY" ] || fail "binary $BINARY not executable (run cargo build first)"
[ -n "${ROOTFS:-}" ] && [ -e "$ROOTFS" ] \
    || fail "no rootfs available; build tests/rootfs or set ROOTFS=<path>"
[ -e fw_jump.bin ] || fail "fw_jump.bin missing"
[ -e Image ] || fail "Image missing"
[ -e blackhole-card.dtb ] || fail "blackhole-card.dtb missing"

# Step 1: reset chip ---------------------------------------------------------
note "tt-smi -r (cold chip)"
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1

# Step 2: cold boot ----------------------------------------------------------
rm -f "$LOG_FILE"
note "daemon start"
"$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
sleep 0.3

note "cold boot L2CPU $L2CPU with disk+net (rootfs=$ROOTFS)"
timeout 60 "$BINARY" boot -t "$CARD" -l "$L2CPU" -d "$ROOTFS" -n >/dev/null

# The CLI absolutizes via canonicalize (follows symlinks) before
# sending to the daemon, so the basename in `daemon status` is the
# basename of the resolved file, not of $ROOTFS. Compute that here so
# soaks work with both regular files (./rootfs.ext4) and the buildroot
# symlink (tests/rootfs/rootfs.ext4 -> ...rootfs.ext2).
rootfs_basename=$(basename "$(readlink -f "$ROOTFS")")
status=$("$BINARY" daemon status -t "$CARD")
echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=.*$rootfs_basename net=y" \
    || fail "post-boot status mismatch:\n$status"
note "post-boot status OK"

# Step 3: soak loop ----------------------------------------------------------
note "starting $ITERATIONS stop/start cycles"
for i in $(seq 1 "$ITERATIONS"); do
    echo "---- iter $i/$ITERATIONS ----"

    "$BINARY" daemon stop -t "$CARD" >/dev/null
    # Small wait to let the ex-daemon's pidfile lock release cleanly.
    sleep 0.2

    rm -f "$LOG_FILE"
    "$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
    # warm-resume does L2Cpu::new + probe + make_slot_from_l2cpu; that's
    # a handful of ioctls, typically ~200-400 ms. Give it a second.
    sleep 1

    status=$("$BINARY" daemon status -t "$CARD")
    echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=- net=-" \
        || fail "iter $i: status did not show warm-resumed Running:\n$status"

    # Disk/net are intentionally NOT carried across the restart, so verify
    # the log also confirms the warm-resume path ran.
    grep -q "\[warm-resume l2cpu $L2CPU\] probe passed" "$LOG_FILE" \
        || fail "iter $i: daemon log missing 'probe passed' line"
    grep -q "\[warm-resume l2cpu $L2CPU\] slot adopted" "$LOG_FILE" \
        || fail "iter $i: daemon log missing 'slot adopted' line"

    note "iter $i: warm-resume OK"
done

# Final teardown ------------------------------------------------------------
note "final daemon stop"
"$BINARY" daemon stop -t "$CARD" >/dev/null
trap - EXIT

echo
echo "PASS: $ITERATIONS warm-resume cycles on card $CARD L2CPU $L2CPU"
