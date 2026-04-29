#!/usr/bin/env bash
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# 100-boot probe sweep — M7 (#73) reliability gate for the Tensix
# virtio engine (#66).
#
# Each iteration:
#   1. tt-smi -r (cold chip)
#   2. daemon start
#   3. boot via U-Boot/EFI/GRUB chain through the Tensix engine
#      (stock distro image, --virtio-console)
#   4. Wait up to PER_ITER_TIMEOUT_S for `login:` in console scrollback
#   5. daemon stop
#   6. Record PASS/FAIL + time-to-login + write_bytes
#
# Pass bar: 100/100. Compare against the host-buffer baseline (~70%)
# in the sweep report. The sweep records every iteration to CSV so a
# partial failure can be diagnosed from the artifact alone.
#
# Usage:
#   bash scripts/validation/probe_sweep.sh
#
# Env:
#   ITERATIONS          loops (default 100)
#   PER_ITER_TIMEOUT_S  per-iter login deadline in seconds (default 300)
#   DISK                disk image path (default images/debian-13.img)
#   BINARY              bhx binary (default ./target/debug/bhx)
#   CARD                tt device index (default 0)
#   L2CPU               core to exercise (default 0)
#   LOG_FILE            daemon log (default ./daemon-card0.log)
#   RESULTS_CSV         CSV output (default ./probe_sweep-<UTC>.csv)
#   TT_VENV             tt-smi venv activate (default ~/.tenstorrent-venv/bin/activate)

set -uo pipefail

ITERATIONS="${ITERATIONS:-100}"
PER_ITER_TIMEOUT_S="${PER_ITER_TIMEOUT_S:-300}"
DISK="${DISK:-images/debian-13.img}"
BINARY="${BINARY:-./target/debug/bhx}"
CARD="${CARD:-0}"
L2CPU="${L2CPU:-0}"
LOG_FILE="${LOG_FILE:-./daemon-card0.log}"
RESULTS_CSV="${RESULTS_CSV:-./probe_sweep-$(date -u +%Y%m%dT%H%M%SZ).csv}"
TT_VENV="${TT_VENV:-$HOME/.tenstorrent-venv/bin/activate}"

note() { echo "[probe-sweep] $*"; }
fail() { echo "[probe-sweep] FAIL: $*" >&2; exit 1; }

# Sanity ------------------------------------------------------------------
[ -x "$BINARY" ]    || fail "binary $BINARY not executable (run cargo build)"
[ -e "$DISK" ]      || fail "disk image $DISK missing (run: $BINARY image pull <distro>)"
[ -f "$TT_VENV" ]   || fail "tt-smi venv not at $TT_VENV"
[ -e "/dev/tenstorrent/$CARD" ] || fail "/dev/tenstorrent/$CARD not present"

cleanup() {
    "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

reset_card() {
    # shellcheck disable=SC1090
    ( . "$TT_VENV" && tt-smi -r >/dev/null 2>&1 )
}

# CSV header + run banner
echo "iter,status,time_to_login_s,write_bytes,fail_stage" > "$RESULTS_CSV"
note "start: $ITERATIONS iterations against $DISK on card $CARD L2CPU $L2CPU"
note "results CSV: $RESULTS_CSV"
sweep_start=$(date +%s)

passed=0
failed=0
fail_iters=()
pass_times=()

for i in $(seq 1 "$ITERATIONS"); do
    iter_start=$(date +%s)
    note "---- iter $i/$ITERATIONS ----"

    if ! reset_card; then
        echo "$i,FAIL,,,reset" >> "$RESULTS_CSV"
        failed=$((failed+1)); fail_iters+=("$i:reset")
        note "  reset failed"
        continue
    fi

    rm -f "$LOG_FILE"
    if ! "$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null 2>&1; then
        echo "$i,FAIL,,,daemon-start" >> "$RESULTS_CSV"
        failed=$((failed+1)); fail_iters+=("$i:daemon-start")
        continue
    fi
    sleep 1

    if ! "$BINARY" boot -t "$CARD" -l "$L2CPU" -d "$DISK" --virtio-console --force >/dev/null 2>&1; then
        "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true
        echo "$i,FAIL,,,boot" >> "$RESULTS_CSV"
        failed=$((failed+1)); fail_iters+=("$i:boot")
        continue
    fi

    boot_t=$(date +%s)
    deadline=$((boot_t + PER_ITER_TIMEOUT_S))

    # Poll for `login:` via short-lived connects. Each `connect` runs
    # for SNAPSHOT_S seconds, prints the full scrollback (the hub
    # replays it on every attach), then exits — exit flushes Rust's
    # BufWriter so the file is fully written. We can't keep a single
    # connect in the background because the Rust client uses its own
    # stdout buffering that `stdbuf` can't reach.
    SNAPSHOT_S=5
    connect_log=$(mktemp -t probe-sweep-XXXXXX.log)
    status="TIMEOUT"
    time_to_login=""
    while [ "$(date +%s)" -lt "$deadline" ]; do
        timeout "$SNAPSHOT_S" "$BINARY" connect -t "$CARD" -l "$L2CPU" \
            </dev/null >"$connect_log" 2>/dev/null || true
        if grep -q "login:" "$connect_log" 2>/dev/null; then
            status="PASS"
            time_to_login=$(( $(date +%s) - boot_t ))
            break
        fi
        # No sleep between snapshots — the SNAPSHOT_S timeout already
        # paces the loop. A brief gap so the daemon hub sees the
        # detach cleanly before the next attach.
        sleep 1
    done

    # Capture write_bytes BEFORE daemon stop (after stop /proc/<pid> is gone)
    daemon_pid=$(pgrep -f "bhx daemon start" | head -1 || true)
    write_bytes=""
    if [ -n "$daemon_pid" ] && [ -e "/proc/$daemon_pid/io" ]; then
        write_bytes=$(awk '/^write_bytes:/ {print $2}' "/proc/$daemon_pid/io" 2>/dev/null || true)
    fi

    rm -f "$connect_log"

    "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true

    if [ "$status" = "PASS" ]; then
        passed=$((passed+1))
        pass_times+=("$time_to_login")
        echo "$i,PASS,$time_to_login,$write_bytes," >> "$RESULTS_CSV"
        note "  PASS in ${time_to_login}s, ${write_bytes:-?}B writes"
    else
        failed=$((failed+1))
        fail_iters+=("$i:$status")
        echo "$i,$status,,$write_bytes,login-wait" >> "$RESULTS_CSV"
        note "  $status (no login: in ${PER_ITER_TIMEOUT_S}s, write_bytes=${write_bytes:-?})"
    fi

    iter_elapsed=$(($(date +%s) - iter_start))
    note "  iter $i took ${iter_elapsed}s ($(date +%H:%M:%S))"
done

# Summary -----------------------------------------------------------------
sweep_elapsed=$(($(date +%s) - sweep_start))
echo
echo "=========================================="
echo "Probe sweep summary"
echo "=========================================="
echo "Image:       $DISK"
echo "Iterations:  $ITERATIONS"
echo "Pass:        $passed"
echo "Fail:        $failed"
pct=$(awk "BEGIN {printf \"%.1f\", $passed/$ITERATIONS*100}")
echo "Pass rate:   ${pct}%"
echo "Wall time:   ${sweep_elapsed}s"

if [ "${#pass_times[@]}" -gt 0 ]; then
    # Stats on time-to-login (sort + awk percentiles).
    sorted=$(printf '%s\n' "${pass_times[@]}" | sort -n)
    n=${#pass_times[@]}
    min=$(echo "$sorted" | head -1)
    max=$(echo "$sorted" | tail -1)
    p50_idx=$(( (n + 1) / 2 ))
    p99_idx=$(( (n * 99 + 99) / 100 ))
    [ "$p99_idx" -lt 1 ] && p99_idx=1
    [ "$p99_idx" -gt "$n" ] && p99_idx="$n"
    p50=$(echo "$sorted" | sed -n "${p50_idx}p")
    p99=$(echo "$sorted" | sed -n "${p99_idx}p")
    mean=$(awk "BEGIN {s=0} {s+=\$1} END {printf \"%.1f\", s/NR}" <<<"$sorted")
    echo "Time-to-login: min=${min}s p50=${p50}s mean=${mean}s p99=${p99}s max=${max}s"
fi

if [ "$failed" -gt 0 ]; then
    echo "Failures:    ${fail_iters[*]}"
fi
echo "CSV:         $RESULTS_CSV"

if [ "$failed" -eq 0 ] && [ "$passed" -eq "$ITERATIONS" ]; then
    echo "PASS: 100.0% ($passed/$ITERATIONS)"
    exit 0
else
    echo "FAIL: $failed/$ITERATIONS iterations failed"
    exit 1
fi
