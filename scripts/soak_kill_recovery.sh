#!/usr/bin/env bash
#
# Stress-test daemon recovery from SIGKILL.
#
# A graceful `daemon stop` is well-tested by soak_warm_resume.sh; this one
# specifically targets the dirty-shutdown path: the daemon dies without
# releasing its socket, pidfile flock, or per-slot worker handles. The
# next `daemon start` has to clean up those artifacts and re-attach to
# the still-running L2CPU via warm-resume.
#
# Plan:
#   1. tt-smi -r; daemon start; boot -l 0 -d rootfs.ext4 -n; verify Running.
#   2. Loop N iterations:
#        a. Read daemon pid from pidfile, SIGKILL it.
#        b. Wait briefly for the kernel to deliver and the parent shells
#           to notice. Verify the pidfile pid no longer exists in /proc.
#        c. daemon start (fresh) — cleans up stale socket, re-acquires
#           pidfile flock, runs warm-resume probe.
#        d. Assert the new daemon's status reports L2CPU 0 as Running
#           (warm-resume adopted the still-live core); disk and net are
#           expected to be gone (they're not preserved across the kill).
#        e. Re-attach disk + net and verify status updates.
#        f. Optional: boot --force to verify the cold-start path still
#           works on a previously-warm-resumed slot.
#   3. Final daemon stop, status check.
#
# Env:
#   ITERATIONS  default 5
#   BINARY      default ./target/debug/bhx
#   LOG_FILE    default ./daemon-card0.log
#   CARD        default 0
#   L2CPU       default 0

set -euo pipefail

ITERATIONS=${ITERATIONS:-5}
BINARY=${BINARY:-./target/debug/bhx}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
CARD=${CARD:-0}
L2CPU=${L2CPU:-0}

PIDFILE="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/bhx/${CARD}/pid"

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "[soak] $*"; }

# Resolve rootfs (ROOTFS env > buildroot > legacy ./rootfs.ext4).
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

# Step 1: cold chip + first daemon ------------------------------------------
note "tt-smi -r (cold chip)"
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1

rm -f "$LOG_FILE"
note "daemon start"
"$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
sleep 0.3

note "cold boot L2CPU $L2CPU with disk+net (rootfs=$ROOTFS)"
timeout 60 "$BINARY" boot -t "$CARD" -l "$L2CPU" -d "$ROOTFS" -n >/dev/null

# Daemon stores the canonicalized (symlinks-followed) path; match
# against THAT basename, not $ROOTFS's basename.
rootfs_basename=$(basename "$(readlink -f "$ROOTFS")")
status=$("$BINARY" daemon status -t "$CARD")
echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=.*$rootfs_basename net=y" \
    || fail "post-boot status mismatch:\n$status"
note "post-boot status OK"

# Step 2: SIGKILL / restart loop --------------------------------------------
note "starting $ITERATIONS SIGKILL/recovery cycles"
for i in $(seq 1 "$ITERATIONS"); do
    echo "---- iter $i/$ITERATIONS ----"

    [ -e "$PIDFILE" ] || fail "iter $i: pidfile $PIDFILE missing before SIGKILL"
    pid=$(cat "$PIDFILE")
    [ -n "$pid" ] || fail "iter $i: pidfile is empty"
    note "iter $i: SIGKILL pid $pid"
    kill -9 "$pid"

    # Wait for the kernel to actually reap the process before checking.
    # A loop with a short timeout beats a fixed sleep on slow hosts.
    deadline=$(( $(date +%s) + 5 ))
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            fail "iter $i: pid $pid still alive 5s after SIGKILL"
        fi
        sleep 0.1
    done

    rm -f "$LOG_FILE"
    note "iter $i: daemon start (recovery)"
    "$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
    # warm-resume probe + slot adoption: usually <500 ms; allow 2 s.
    sleep 1

    status=$("$BINARY" daemon status -t "$CARD")
    echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=- net=-" \
        || fail "iter $i: warm-resume did not adopt l2cpu $L2CPU:\n$status"

    grep -q "\[warm-resume l2cpu $L2CPU\] slot adopted" "$LOG_FILE" \
        || fail "iter $i: log missing 'slot adopted' from warm-resume"

    # Re-attach disk + net to confirm the slot is functional after recovery.
    "$BINARY" add-disk -t "$CARD" -l "$L2CPU" "$ROOTFS" >/dev/null
    "$BINARY" add-net -t "$CARD" -l "$L2CPU" >/dev/null
    status=$("$BINARY" daemon status -t "$CARD")
    echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=.*$rootfs_basename net=y" \
        || fail "iter $i: post-reattach status mismatch:\n$status"

    note "iter $i: SIGKILL recovery OK"
done

note "final daemon stop"
"$BINARY" daemon stop -t "$CARD" >/dev/null
trap - EXIT

echo
echo "PASS: $ITERATIONS SIGKILL/recovery cycles on card $CARD L2CPU $L2CPU"
