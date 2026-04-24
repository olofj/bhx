#!/usr/bin/env bash
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
#   BINARY      path to tt-bh-linux (default ./target/debug/tt-bh-linux)
#   LOG_FILE    daemon log path (default ./daemon-card0.log)
#   CARD        tenstorrent device index (default 0)
#   L2CPU       core index to exercise (default 0)

set -euo pipefail

ITERATIONS=${ITERATIONS:-5}
BINARY=${BINARY:-./target/debug/tt-bh-linux}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
CARD=${CARD:-0}
L2CPU=${L2CPU:-0}

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "[soak] $*"; }

cleanup() {
    "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Sanity checks -------------------------------------------------------------
[ -x "$BINARY" ] || fail "binary $BINARY not executable (run cargo build first)"
[ -e rootfs.ext4 ] || fail "rootfs.ext4 not present in cwd (kernel pull + image pull first)"
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

note "cold boot L2CPU $L2CPU with disk+net"
timeout 60 "$BINARY" boot -t "$CARD" -l "$L2CPU" --no-console -n >/dev/null

status=$("$BINARY" daemon status -t "$CARD")
echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=.*rootfs.ext4 net=y" \
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
