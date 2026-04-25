#!/usr/bin/env bash
#
# Stress-test `remove-disk` while the guest is mid-I/O.
#
# A graceful `remove-disk` on an idle guest is well-tested; this one
# exercises the case where the worker thread is actively servicing
# virtio descriptors when the host yanks the disk. Concretely:
#
#   1. Boot the guest with a disk and let it run normally — kernel
#      journal, systemd, etc. are constantly writing to the rootfs even
#      at "idle", so the virtio block worker is genuinely active.
#   2. From the host, call remove-disk and assert it returns within 5
#      seconds (the slowest worker-join we observe in healthy runs is
#      ~150 ms; 5 s leaves headroom for a system under heavy load).
#   3. Assert the daemon is still alive after remove-disk and reports
#      disk=- in status. The guest's I/O will fail with EIO from
#      its perspective, which is the expected outcome — we don't
#      attempt to keep the guest happy across the disk yank.
#   4. Re-attach the disk and confirm the slot becomes addressable
#      again. The guest can't safely re-mount, but the daemon-side
#      worker should come up clean.
#   5. Repeat ITERATIONS times to catch races that show up only on
#      every Nth iteration.
#
# Env:
#   ITERATIONS  default 5
#   BINARY      default ./target/debug/tt-bh-linux
#   LOG_FILE    default ./daemon-card0.log
#   CARD        default 0
#   L2CPU       default 0
#   TIMEOUT     remove-disk timeout in seconds (default 5)

set -euo pipefail

ITERATIONS=${ITERATIONS:-5}
BINARY=${BINARY:-./target/debug/tt-bh-linux}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
CARD=${CARD:-0}
L2CPU=${L2CPU:-0}
TIMEOUT=${TIMEOUT:-5}

PIDFILE="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/tt-bh-linux/${CARD}/pid"

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

# Step 1: cold chip + boot --------------------------------------------------
note "tt-smi -r (cold chip)"
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1

rm -f "$LOG_FILE"
note "daemon start"
"$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
sleep 0.3

note "cold boot L2CPU $L2CPU with disk (rootfs=$ROOTFS)"
timeout 60 "$BINARY" boot -t "$CARD" -l "$L2CPU" -d "$ROOTFS" --no-console >/dev/null

# Give the guest a moment to actually mount the rootfs and start working it.
# Without this, "I/O pressure" is just the boot-time loader, which doesn't
# exercise the steady-state descriptor path we're targeting.
note "letting guest reach steady-state I/O (10 s warm-up)"
sleep 10

rootfs_basename=$(basename "$ROOTFS")
status=$("$BINARY" daemon status -t "$CARD")
echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=.*$rootfs_basename" \
    || fail "post-boot status mismatch:\n$status"
note "post-boot status OK; daemon pid $(cat "$PIDFILE")"

# Step 2: remove-disk under load loop ---------------------------------------
note "starting $ITERATIONS remove-disk-under-load cycles"
for i in $(seq 1 "$ITERATIONS"); do
    echo "---- iter $i/$ITERATIONS ----"

    # Snapshot daemon pid so we can detect a crash mid-iteration.
    pid=$(cat "$PIDFILE")

    note "iter $i: remove-disk (timeout ${TIMEOUT}s)"
    start=$(date +%s%N)
    timeout "$TIMEOUT" "$BINARY" remove-disk -t "$CARD" -l "$L2CPU" \
        || fail "iter $i: remove-disk did not return within ${TIMEOUT}s"
    end=$(date +%s%N)
    elapsed_ms=$(( (end - start) / 1000000 ))
    note "iter $i: remove-disk returned in ${elapsed_ms}ms"

    # Daemon survived?
    kill -0 "$pid" 2>/dev/null \
        || fail "iter $i: daemon (pid $pid) died during remove-disk"

    status=$("$BINARY" daemon status -t "$CARD")
    echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=-" \
        || fail "iter $i: post-remove status not 'disk=-':\n$status"

    # Re-attach the disk for the next iteration.
    "$BINARY" add-disk -t "$CARD" -l "$L2CPU" "$ROOTFS" >/dev/null \
        || fail "iter $i: add-disk failed"
    status=$("$BINARY" daemon status -t "$CARD")
    echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=.*$rootfs_basename" \
        || fail "iter $i: post-readd status mismatch:\n$status"

    # A short settle before the next yank. Without this, the guest hasn't
    # noticed the new disk and the next remove-disk is effectively a noop
    # rather than a yank-mid-IO.
    sleep 2

    note "iter $i: remove-disk under load OK"
done

note "final daemon stop"
"$BINARY" daemon stop -t "$CARD" >/dev/null
trap - EXIT

echo
echo "PASS: $ITERATIONS remove-disk-under-load cycles on card $CARD L2CPU $L2CPU"
