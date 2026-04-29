#!/usr/bin/env bash
#
# Stress-test the dynamic add/remove-disk and add/remove-net RPCs.
#
# Plan:
#   1. tt-smi -r (clean chip state).
#   2. Start daemon, cold-boot L2CPU 0 WITHOUT disk or network (so the
#      slot starts bare and add/remove is exercised from scratch).
#   3. Loop N iterations:
#        add-disk    -> status shows rootfs.ext4
#        remove-disk -> status shows disk=-
#        add-net     -> status shows net=y
#        remove-net  -> status shows net=-
#      Any assertion failure aborts with the mismatched status.
#   4. Also verify repeat-remove behaves (clean error, slot untouched).
#   5. daemon stop.
#
# Env:
#   ITERATIONS  add/remove cycles (default 10)
#   BINARY      path to tt-bh-linux
#   LOG_FILE    daemon log path
#   CARD, L2CPU

set -euo pipefail

ITERATIONS=${ITERATIONS:-10}
BINARY=${BINARY:-./target/debug/tt-bh-linux}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
CARD=${CARD:-0}
L2CPU=${L2CPU:-0}

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "[soak] $*"; }

# Resolve the disk to attach. DISK_PATH explicit override > ROOTFS env >
# buildroot test rootfs > legacy ./rootfs.ext4.
if [ -z "${DISK_PATH:-}" ]; then
    if [ -n "${ROOTFS:-}" ]; then
        DISK_PATH="$ROOTFS"
    elif [ -e tests/rootfs/rootfs.ext4 ]; then
        DISK_PATH=tests/rootfs/rootfs.ext4
    else
        DISK_PATH=rootfs.ext4
    fi
fi

cleanup() {
    "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true
}
trap cleanup EXIT

[ -x "$BINARY" ] || fail "binary $BINARY not executable (cargo build first)"
[ -e "$DISK_PATH" ] || fail "$DISK_PATH not present"

note "tt-smi -r"
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1

rm -f "$LOG_FILE"
note "daemon start"
"$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
sleep 0.3

# Cold-boot with the defaults — boot picks up rootfs.ext4 automatically.
# We immediately remove the disk so the soak loop starts from a known
# "no disk, no net" state regardless of what `boot` attached.
note "cold boot L2CPU $L2CPU"
timeout 60 "$BINARY" boot -t "$CARD" -l "$L2CPU" >/dev/null
note "clear attached disk (default rootfs was auto-picked up)"
"$BINARY" remove-disk -t "$CARD" -l "$L2CPU" >/dev/null 2>&1 || true

status=$("$BINARY" daemon status -t "$CARD")
echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=- net=-" \
    || fail "pre-soak status mismatch:\n$status"
note "starting state OK: Running, no disk, no net"

# Match the daemon-side basename (CLI canonicalizes via readlink), so
# the regex works for both regular files and the buildroot symlink.
disk_basename=$(basename "$(readlink -f "$DISK_PATH")")

# Soak loop ----------------------------------------------------------------
note "starting $ITERATIONS add/remove cycles"
for i in $(seq 1 "$ITERATIONS"); do
    echo "---- iter $i/$ITERATIONS ----"

    "$BINARY" add-disk -t "$CARD" -l "$L2CPU" "$DISK_PATH" >/dev/null
    status=$("$BINARY" daemon status -t "$CARD")
    echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=.*$disk_basename net=-" \
        || fail "iter $i: after add-disk:\n$status"

    "$BINARY" remove-disk -t "$CARD" -l "$L2CPU" >/dev/null
    status=$("$BINARY" daemon status -t "$CARD")
    echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=- net=-" \
        || fail "iter $i: after remove-disk:\n$status"

    "$BINARY" add-net -t "$CARD" -l "$L2CPU" >/dev/null
    status=$("$BINARY" daemon status -t "$CARD")
    echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=- net=y" \
        || fail "iter $i: after add-net:\n$status"

    "$BINARY" remove-net -t "$CARD" -l "$L2CPU" >/dev/null
    status=$("$BINARY" daemon status -t "$CARD")
    echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=- net=-" \
        || fail "iter $i: after remove-net:\n$status"
done

# Error paths --------------------------------------------------------------
note "double-remove should error cleanly (no disk)"
if "$BINARY" remove-disk -t "$CARD" -l "$L2CPU" >/dev/null 2>&1; then
    fail "remove-disk on empty slot unexpectedly succeeded"
fi
status=$("$BINARY" daemon status -t "$CARD")
echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=- net=-" \
    || fail "status changed after failed remove-disk:\n$status"

note "double-remove should error cleanly (no net)"
if "$BINARY" remove-net -t "$CARD" -l "$L2CPU" >/dev/null 2>&1; then
    fail "remove-net on empty slot unexpectedly succeeded"
fi

note "final daemon stop"
"$BINARY" daemon stop -t "$CARD" >/dev/null
trap - EXIT

echo
echo "PASS: $ITERATIONS add/remove cycles on card $CARD L2CPU $L2CPU"
