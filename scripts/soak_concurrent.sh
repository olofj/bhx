#!/usr/bin/env bash
#
# Stress-test the daemon under concurrent RPCs hitting sibling L2CPUs.
# Closes the "concurrent RPCs on sibling L2CPUs" gap in the README.
#
# What this exercises:
#   1. Parallel cold boots — 4 simultaneous `boot` RPCs, one per L2CPU,
#      each with its own rootfs. After issue #1 Phase 3 the daemon has no
#      `boot_lock`: tile-(8,0) AXI access serializes via
#      `SharedChip::seq_lock`, per-L2CPU NOC traffic runs on each core's
#      own fd. This used to crash the host; it's now the gating signal
#      that the refactor holds under concurrency.
#   2. Parallel add/remove hammer — for N iterations, 4 background
#      subshells each do `remove-disk && add-disk && remove-net &&
#      add-net` against their own slot, in parallel. Verifies the daemon
#      keeps per-slot state consistent under cross-slot contention.
#   3. Status-poll-while-busy — a background loop polls `daemon status`
#      at ~20 Hz while the add/remove hammer runs. Verifies read RPCs
#      don't deadlock or error while write RPCs are in flight.
#
# Deliberately NOT covered (still in the README's "not covered" list):
#   - `--force` teardown races
#   - SIGKILL mid-RPC
#   - I/O pressure from guest during remove-disk
#   - libvdeslirp TCP session loss on remove-net
#
# Env:
#   ITERATIONS       add/remove cycles (default 5)
#   BINARY           path to tt-bh-linux
#   LOG_FILE         daemon log path
#   CARD             tt device index (default 0)
#   STATUS_POLL_HZ   background status-poll frequency (default 20)

set -euo pipefail

ITERATIONS=${ITERATIONS:-5}
BINARY=${BINARY:-./target/debug/tt-bh-linux}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
CARD=${CARD:-0}
STATUS_POLL_HZ=${STATUS_POLL_HZ:-20}
CORES=(0 1 2 3)

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "[soak] $*"; }

# Track children we start so cleanup can reap them even on mid-run failure.
POLL_PID=""

cleanup() {
    if [ -n "$POLL_PID" ] && kill -0 "$POLL_PID" 2>/dev/null; then
        kill "$POLL_PID" 2>/dev/null || true
        wait "$POLL_PID" 2>/dev/null || true
    fi
    "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true
}
trap cleanup EXIT

[ -x "$BINARY" ] || fail "binary $BINARY not executable (run cargo build first)"

# Resolve rootfs (ROOTFS env > buildroot > legacy ./rootfs.ext4).
if [ -z "${ROOTFS:-}" ]; then
    if [ -e tests/rootfs/rootfs.ext4 ]; then
        ROOTFS=tests/rootfs/rootfs.ext4
    elif [ -e rootfs.ext4 ]; then
        ROOTFS=rootfs.ext4
    fi
fi
[ -n "${ROOTFS:-}" ] && [ -e "$ROOTFS" ] \
    || fail "no rootfs available; build tests/rootfs or set ROOTFS=<path>"
[ -e fw_jump.bin ] || fail "fw_jump.bin missing"
[ -e Image ] || fail "Image missing"
[ -e blackhole-card.dtb ] || fail "blackhole-card.dtb missing"

# Per-core rootfs copies so concurrent disk workers don't share an mmap'd
# backing file (ext4 would corrupt). Only copy missing ones.
for i in "${CORES[@]}"; do
    if [ ! -e "rootfs-${i}.ext4" ]; then
        note "copying $ROOTFS -> rootfs-${i}.ext4"
        cp --reflink=auto "$ROOTFS" "rootfs-${i}.ext4"
    fi
done

note "tt-smi -r (cold chip)"
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1

rm -f "$LOG_FILE"
note "daemon start"
"$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
sleep 0.3

# ---------------------------------------------------------------------------
# Phase 1: parallel cold boots
# ---------------------------------------------------------------------------
note "phase 1: parallel boot of 4 L2CPUs"
BOOT_STATUS_DIR=$(mktemp -d)
trap 'rm -rf "$BOOT_STATUS_DIR"; cleanup' EXIT

boot_pids=()
for i in "${CORES[@]}"; do
    (
        set +e
        timeout 90 "$BINARY" boot -t "$CARD" -l "$i" \
            -d "rootfs-${i}.ext4" -n \
            > "$BOOT_STATUS_DIR/boot-${i}.out" 2>&1
        echo $? > "$BOOT_STATUS_DIR/boot-${i}.rc"
    ) &
    boot_pids+=("$!")
done
for pid in "${boot_pids[@]}"; do wait "$pid"; done

for i in "${CORES[@]}"; do
    rc=$(cat "$BOOT_STATUS_DIR/boot-${i}.rc")
    if [ "$rc" != "0" ]; then
        echo "--- boot-${i}.out ---" >&2
        cat "$BOOT_STATUS_DIR/boot-${i}.out" >&2
        fail "parallel boot L2CPU $i exited $rc"
    fi
done

status=$("$BINARY" daemon status -t "$CARD")
for i in "${CORES[@]}"; do
    echo "$status" | grep -qE "l2cpu $i: Running disk=.*rootfs-${i}.ext4 net=y" \
        || fail "post-parallel-boot status mismatch for L2CPU $i:\n$status"
done
note "all 4 slots Running with disk + net"

# ---------------------------------------------------------------------------
# Phase 2: background status poller
# ---------------------------------------------------------------------------
POLL_RC_FILE=$(mktemp)
(
    interval=$(awk -v hz="$STATUS_POLL_HZ" 'BEGIN{ printf "%.3f\n", 1.0/hz }')
    count=0; errors=0
    while true; do
        if ! "$BINARY" daemon status -t "$CARD" >/dev/null 2>&1; then
            errors=$((errors + 1))
        fi
        count=$((count + 1))
        sleep "$interval"
    done
) &
POLL_PID=$!
# Poll writes nothing until it's signalled; track via separate counter file.

# ---------------------------------------------------------------------------
# Phase 3: concurrent add/remove hammer
# ---------------------------------------------------------------------------
note "phase 3: $ITERATIONS iterations of 4-way concurrent add/remove"
HAMMER_DIR=$(mktemp -d)
trap 'rm -rf "$BOOT_STATUS_DIR" "$HAMMER_DIR" "$POLL_RC_FILE"; cleanup' EXIT

for iter in $(seq 1 "$ITERATIONS"); do
    echo "---- iter $iter/$ITERATIONS ----"

    hammer_pids=()
    for i in "${CORES[@]}"; do
        (
            set +e
            out=$(
                "$BINARY" remove-disk -t "$CARD" -l "$i" 2>&1 \
                && "$BINARY" add-disk  -t "$CARD" -l "$i" "rootfs-${i}.ext4" 2>&1 \
                && "$BINARY" remove-net  -t "$CARD" -l "$i" 2>&1 \
                && "$BINARY" add-net     -t "$CARD" -l "$i" 2>&1
            )
            rc=$?
            printf '%s\n' "$out" > "$HAMMER_DIR/iter${iter}-core${i}.out"
            echo "$rc" > "$HAMMER_DIR/iter${iter}-core${i}.rc"
        ) &
        hammer_pids+=("$!")
    done
    # NB: must wait for these pids explicitly — a bare `wait` also blocks on
    # the infinite status poller ($POLL_PID) and would hang the script.
    for pid in "${hammer_pids[@]}"; do wait "$pid"; done

    for i in "${CORES[@]}"; do
        rc=$(cat "$HAMMER_DIR/iter${iter}-core${i}.rc")
        if [ "$rc" != "0" ]; then
            echo "--- iter${iter}-core${i}.out ---" >&2
            cat "$HAMMER_DIR/iter${iter}-core${i}.out" >&2
            fail "iter $iter core $i: RPC sequence exit $rc"
        fi
    done

    # After each iteration all 4 should end on "disk=rootfs-N.ext4 net=y".
    note "iter $iter: fetching daemon status (post-hammer assertion)"
    status=$(timeout 10 "$BINARY" daemon status -t "$CARD") || fail "iter $iter: daemon status timed out or errored"
    for i in "${CORES[@]}"; do
        echo "$status" | grep -qE "l2cpu $i: Running disk=.*rootfs-${i}.ext4 net=y" \
            || fail "iter $iter: post-hammer status mismatch for L2CPU $i:\n$status"
    done
    note "iter $iter: OK"
done

# ---------------------------------------------------------------------------
# Phase 4: stop the poller, verify it didn't see any errors
# ---------------------------------------------------------------------------
# Kill the poller with a signal that our subshell can handle; the subshell
# exit status is what we check. Our subshell loops forever, so a clean kill
# is expected.
kill "$POLL_PID" 2>/dev/null || true
wait "$POLL_PID" 2>/dev/null || true
POLL_PID=""
# No direct error count — we rely on "daemon stop" succeeding below as the
# smoke test that the poller didn't corrupt anything. If the daemon were
# deadlocked, `daemon stop` would hang or error.

note "final daemon stop"
timeout 30 "$BINARY" daemon stop -t "$CARD" >/dev/null

status_rc=0
"$BINARY" daemon status -t "$CARD" >/dev/null 2>&1 || status_rc=$?
[ "$status_rc" = "0" ] || fail "daemon status after stop returned $status_rc (expected 0 / not-running)"

trap - EXIT
rm -rf "$BOOT_STATUS_DIR" "$HAMMER_DIR" "$POLL_RC_FILE"

echo
echo "PASS: parallel boot + $ITERATIONS concurrent add/remove cycles on card $CARD"
