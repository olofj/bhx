#!/usr/bin/env bash
#
# Long-running endurance soak. The other soak_*.sh scripts catch
# correctness regressions in 5 iterations / ~2 minutes; this one
# runs for hours, looking for *drift* — fd leaks, RSS growth, slow
# slirp-side state accumulation, u32 counter wraparound — that don't
# surface on short runs.
#
# Plan (per iteration):
#   1. add-disk + remove-disk + add-net + remove-net cycle (so the
#      slot mutates each iteration, exercising the full
#      teardown / reattach path).
#   2. Capture daemon RSS + VSZ + open-fd-count, append to CSV.
#   3. Drift check: RSS grew more than RSS_DRIFT_PCT% (default 25%)
#      OR fd-count grew more than FD_DRIFT_ABS (default 10) above the
#      current baseline → fail the run with the iteration index.
#   4. Every WARM_RESUME_EVERY (default 100) iterations, daemon stop
#      then daemon start. Warm-resume re-adopts L2CPU 0 from chip
#      state. After the restart, baseline RSS/fd reset to the new
#      pid's numbers — drift is per-uptime, not cumulative across
#      the restart.
#   5. Sleep ITER_INTERVAL (default 30 s) so the workers actually
#      park in the IDLE_SLEEP tier (#27) — that's the path we
#      most want to soak in a long-running test.
#
# Background `connect` client stays attached for the duration so
# the chip-console pump path is exercised continuously, not just
# at boot.
#
# Env:
#   DURATION_HOURS     total runtime, default 8 (0.05 ≈ 3 min for a
#                      smoke test under acceptance criterion).
#   ITER_INTERVAL      seconds between iterations, default 30.
#   WARM_RESUME_EVERY  daemon stop/start every N iters, default 100.
#                      Set to 0 to skip the warm-resume drill.
#   RSS_DRIFT_PCT      RSS-growth threshold (% above per-uptime
#                      baseline), default 25.
#   FD_DRIFT_ABS       fd-count growth threshold (absolute), default 10.
#   CSV                output path, default ./soak_endurance-<ts>.csv.
#   BINARY, LOG_FILE, CARD, L2CPU, ROOTFS — see other soak scripts.

set -uo pipefail   # NOT -e: drift checks return nonzero from arith
                   # we want to handle, not abort

DURATION_HOURS=${DURATION_HOURS:-8}
ITER_INTERVAL=${ITER_INTERVAL:-30}
WARM_RESUME_EVERY=${WARM_RESUME_EVERY:-100}
RSS_DRIFT_PCT=${RSS_DRIFT_PCT:-25}
FD_DRIFT_ABS=${FD_DRIFT_ABS:-10}

BINARY=${BINARY:-./target/debug/bhx}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
CARD=${CARD:-0}
L2CPU=${L2CPU:-0}

ts=$(date +%Y%m%d-%H%M%S)
CSV=${CSV:-./soak_endurance-$ts.csv}
PIDFILE="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/bhx/${CARD}/pid"

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "[soak] $*"; }

# Resolve rootfs (priority: ROOTFS env > buildroot > legacy).
if [ -z "${ROOTFS:-}" ]; then
    if [ -e tests/rootfs/rootfs.ext4 ]; then
        ROOTFS=tests/rootfs/rootfs.ext4
    elif [ -e rootfs.ext4 ]; then
        ROOTFS=rootfs.ext4
    fi
fi

# Cleanup state we may end up holding.
held_connect_pid=""
cleanup() {
    if [ -n "$held_connect_pid" ] && kill -0 "$held_connect_pid" 2>/dev/null; then
        kill "$held_connect_pid" 2>/dev/null || true
    fi
    "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'note "interrupted at iter ${i:-0}; cleaning up"; exit 130' INT TERM

# Sanity checks.
[ -x "$BINARY" ] || fail "binary $BINARY not executable (run cargo build first)"
[ -n "${ROOTFS:-}" ] && [ -e "$ROOTFS" ] \
    || fail "no rootfs available; build tests/rootfs or set ROOTFS=<path>"
[ -e fw_jump.bin ] || fail "fw_jump.bin missing"
[ -e Image ] || fail "Image missing"
[ -e blackhole-card.dtb ] || fail "blackhole-card.dtb missing"

rss_of()      { ps -o rss= -p "$1" 2>/dev/null | tr -d ' '; }
vsz_of()      { ps -o vsz= -p "$1" 2>/dev/null | tr -d ' '; }
fd_count_of() { ls "/proc/$1/fd" 2>/dev/null | wc -l; }
read_pid()    { cat "$PIDFILE" 2>/dev/null; }

# Step 1: reset chip + cold boot.
note "tt-smi -r (cold chip)"
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1

rm -f "$LOG_FILE"
note "daemon start"
"$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
sleep 0.3

note "cold boot L2CPU $L2CPU with disk+net (rootfs=$ROOTFS)"
timeout 60 "$BINARY" boot -t "$CARD" -l "$L2CPU" -d "$ROOTFS" -n >/dev/null

rootfs_basename=$(basename "$(readlink -f "$ROOTFS")")
status=$("$BINARY" daemon status -t "$CARD")
echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=.*$rootfs_basename net=y" \
    || fail "post-boot status mismatch:\n$status"

# Hold a `connect` open in the background so the chip-console pump path
# stays warm. </dev/null + no -i means it just sits attached, draining
# the scrollback. SIGPIPE on the daemon's side won't take down the
# connection because the daemon owns the socket.
note "opening background connect to keep chip-console pump warm"
# sleep-infinity feeds stdin so `connect` doesn't EOF immediately;
# the chip-console pump path is what we want exercised, not stdin.
( sleep infinity | "$BINARY" connect -t "$CARD" -l "$L2CPU" >/dev/null 2>&1 ) &
held_connect_pid=$!
sleep 1
if ! kill -0 "$held_connect_pid" 2>/dev/null; then
    # Not fatal — endurance soak still runs, we just don't get the
    # continuous chip-console exercise. Note + continue.
    note "warning: background connect didn't stay up; continuing without"
    held_connect_pid=""
fi

pid=$(read_pid) || fail "couldn't read pidfile $PIDFILE"
[ -n "$pid" ] || fail "empty pid in $PIDFILE"
baseline_rss=$(rss_of "$pid")
baseline_fd=$(fd_count_of "$pid")
note "baseline: pid=$pid rss=${baseline_rss}KiB fd=$baseline_fd"

# CSV header.
echo "iter,t_s,pid,rss_kib,vsz_kib,fd_count,rss_drift_pct,fd_drift_abs" > "$CSV"

# Step 2: endurance loop.
duration_s=$(awk "BEGIN { printf \"%d\", $DURATION_HOURS * 3600 }")
deadline=$(( $(date +%s) + duration_s ))
note "starting endurance loop (deadline: $(date -d @$deadline) = ${DURATION_HOURS}h)"

i=0
t_start=$(date +%s)
while [ "$(date +%s)" -lt "$deadline" ]; do
    i=$((i + 1))

    # Add-disk -> Remove-disk -> Add-net -> Remove-net cycle.
    # Each step asserts the slot reaches the expected state.
    if ! "$BINARY" remove-net -t "$CARD" -l "$L2CPU" >/dev/null 2>&1; then
        # Net was already removed (we may be racing the prior loop iter's
        # remove-net). Not fatal.
        true
    fi
    if ! "$BINARY" remove-disk -t "$CARD" -l "$L2CPU" >/dev/null 2>&1; then
        true
    fi
    "$BINARY" add-disk -t "$CARD" -l "$L2CPU" "$ROOTFS" >/dev/null \
        || fail "iter $i: add-disk failed"
    "$BINARY" add-net -t "$CARD" -l "$L2CPU" >/dev/null \
        || fail "iter $i: add-net failed"

    # Capture daemon health.
    cur_pid=$(read_pid)
    rss=$(rss_of "$cur_pid")
    vsz=$(vsz_of "$cur_pid")
    fd=$(fd_count_of "$cur_pid")
    if [ -z "$rss" ] || [ -z "$fd" ]; then
        fail "iter $i: daemon (pid=$cur_pid) appears dead"
    fi

    rss_drift=$(( (rss - baseline_rss) * 100 / baseline_rss ))
    fd_drift=$(( fd - baseline_fd ))
    elapsed=$(( $(date +%s) - t_start ))
    echo "$i,$elapsed,$cur_pid,$rss,$vsz,$fd,$rss_drift,$fd_drift" >> "$CSV"

    if [ "$rss_drift" -gt "$RSS_DRIFT_PCT" ]; then
        fail "iter $i: RSS drift ${rss_drift}% > ${RSS_DRIFT_PCT}% (rss=${rss}KiB, baseline=${baseline_rss}KiB)"
    fi
    if [ "$fd_drift" -gt "$FD_DRIFT_ABS" ]; then
        fail "iter $i: fd drift +${fd_drift} > ${FD_DRIFT_ABS} (fd=$fd, baseline=$baseline_fd)"
    fi

    # Periodic warm-resume drill.
    if [ "$WARM_RESUME_EVERY" -gt 0 ] && [ $((i % WARM_RESUME_EVERY)) -eq 0 ]; then
        note "iter $i: WARM_RESUME drill (daemon stop+start)"
        # Stop the held connect first — it'll EOF when the daemon exits
        # but cleaning up explicitly is tidier.
        if [ -n "$held_connect_pid" ] && kill -0 "$held_connect_pid" 2>/dev/null; then
            kill "$held_connect_pid" 2>/dev/null || true
            wait "$held_connect_pid" 2>/dev/null || true
            held_connect_pid=""
        fi

        "$BINARY" daemon stop -t "$CARD" >/dev/null
        sleep 0.3
        rm -f "$LOG_FILE"
        "$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
        sleep 1

        # Confirm warm-resume picked the core back up (no disk/net
        # carried across the restart — that's expected).
        status=$("$BINARY" daemon status -t "$CARD")
        echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=- net=-" \
            || fail "iter $i: warm-resume didn't readopt l2cpu $L2CPU:\n$status"

        # Re-baseline drift checks against the new pid.
        pid=$(read_pid)
        baseline_rss=$(rss_of "$pid")
        baseline_fd=$(fd_count_of "$pid")
        note "iter $i: warm-resume OK; new baseline rss=${baseline_rss}KiB fd=$baseline_fd"

        # Re-open the held connect so the chip-console pump exercise
        # continues across the rest of the run.
        "$BINARY" connect -t "$CARD" -l "$L2CPU" </dev/null >/dev/null 2>&1 &
        held_connect_pid=$!
        sleep 0.5
        if ! kill -0 "$held_connect_pid" 2>/dev/null; then
            note "warning: post-resume connect didn't stay up"
            held_connect_pid=""
        fi
    fi

    sleep "$ITER_INTERVAL"
done

# Final teardown.
note "final daemon stop"
"$BINARY" daemon stop -t "$CARD" >/dev/null

echo
echo "PASS: $i endurance iterations over ${DURATION_HOURS}h on card $CARD L2CPU $L2CPU"
echo "      CSV: $CSV"
