#!/usr/bin/env bash
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# High-iteration cold-boot soak. Targets the historical Ubuntu-3% class
# of probe-time regressions where feature-bit / queue-config writes were
# missed during virtio-mmio bring-up, and the kernel's retry loop either
# converged after extra cycles (visible as `[probe-status] STATUS reset
# to 0`) or gave up (visible as `[probe-status] STATUS_FAILED set`).
#
# Each iteration is a true cold boot: tt-smi -r resets the card, daemon
# starts fresh, BRISC firmware re-initializes, the guest kernel walks
# the full virtio-mmio probe sequence for all four devices.
#
# Per-iteration assertions on the daemon log:
#   - exactly 4 `[probe-status] slot N (...) reached STATUS_DRIVER_OK`
#     lines (rng, blk, net, console)
#   - 0 `[probe-status] ... STATUS reset to 0` lines (any = kernel
#     retry-cycle, regression candidate even if probe eventually
#     converges)
#   - 0 `[probe-status] ... STATUS_FAILED set` lines (kernel gave up)
#   - 0 `panic|fatal|kick.*drop|rescue|throttle.*ENGAGE` lines (V1-path
#     activity that should never exist post-V2.1)
#
# Each failed iteration is logged + counted; the harness keeps going so
# we get an N-of-M failure rate, not just first-failure-aborts. The
# script exits 0 only if all iterations passed; non-zero with the
# failure list otherwise.
#
# Env:
#   ITERATIONS    cold-boot cycles (default 50; bump to 200+ for a real
#                 regression hunt)
#   BINARY        bhx binary (default ./target/release/bhx — cold-boot
#                 timing is what we want to measure, so release build)
#   LOG_FILE      daemon log path (default ./daemon-card0.log)
#   CARD          tt device index (default 0)
#   L2CPU         core to exercise (default 0)
#   ROOTFS        disk image (default: buildroot-stripped.ext4 →
#                 third_party/buildroot/rootfs.ext4 → ./rootfs.ext4)
#   BOOT_TIMEOUT  max seconds for all 4 DRIVER_OK lines to appear
#                 (default 60)
#   FAIL_LOG_DIR  per-iter log archive for failed iters (default
#                 ./soak-cold-boot-failures-<timestamp>)

set -euo pipefail

ITERATIONS=${ITERATIONS:-50}
BINARY=${BINARY:-./target/release/bhx}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
CARD=${CARD:-0}
L2CPU=${L2CPU:-0}
BOOT_TIMEOUT=${BOOT_TIMEOUT:-60}
TS=$(date +%Y%m%d-%H%M%S)
FAIL_LOG_DIR=${FAIL_LOG_DIR:-./soak-cold-boot-failures-$TS}
RESULT_CSV=${RESULT_CSV:-./soak-cold-boot-$TS.csv}

if [ -z "${ROOTFS:-}" ]; then
    if [ -e buildroot-stripped.ext4 ]; then
        ROOTFS=buildroot-stripped.ext4
    elif [ -e third_party/buildroot/rootfs.ext4 ]; then
        ROOTFS=third_party/buildroot/rootfs.ext4
    elif [ -e rootfs.ext4 ]; then
        ROOTFS=rootfs.ext4
    fi
fi

if [ ! -e "$ROOTFS" ]; then
    echo "FAIL: rootfs not found: $ROOTFS" >&2
    exit 1
fi
if [ ! -x "$BINARY" ]; then
    echo "FAIL: bhx binary not executable: $BINARY" >&2
    exit 1
fi

note() { echo "[soak] $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

cleanup() {
    "$BINARY" daemon stop -t "$CARD" 2>/dev/null || true
}
trap cleanup EXIT

reset_card() {
    # tt-smi -r is the canonical full-board reset. Quiet its progress
    # spinner so the iteration log stays readable.
    (
        # shellcheck disable=SC1091
        . ~/.tenstorrent-venv/bin/activate
        tt-smi -r 2>&1 | tail -1
    )
}

note "ITERATIONS=$ITERATIONS BINARY=$BINARY ROOTFS=$ROOTFS L2CPU=$L2CPU"
note "fail-log dir: $FAIL_LOG_DIR  result CSV: $RESULT_CSV"
mkdir -p "$FAIL_LOG_DIR"

# CSV header
echo "iter,result,reset_s,daemon_s,boot_s,driver_ok_count,status_resets,status_failed,errors" > "$RESULT_CSV"

PASS=0
FAIL=0
declare -a FAIL_ITERS=()

for i in $(seq 1 "$ITERATIONS"); do
    iter_start=$(date +%s.%N)
    errors=()

    # 1. Stop any previous daemon (idempotent — first iteration may have
    # nothing to stop).
    "$BINARY" daemon stop -t "$CARD" 2>/dev/null || true

    # 2. Reset card (true cold).
    t0=$(date +%s.%N)
    reset_card >/dev/null
    reset_s=$(awk "BEGIN{printf \"%.2f\", $(date +%s.%N) - $t0}")

    # 3. Truncate daemon log so per-iter greps see only this iteration.
    : > "$LOG_FILE"

    # 4. Start daemon.
    t0=$(date +%s.%N)
    if ! "$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null 2>&1; then
        errors+=("daemon-start-failed")
    fi
    daemon_s=$(awk "BEGIN{printf \"%.2f\", $(date +%s.%N) - $t0}")

    # 5. Boot + wait for all 4 DRIVER_OK lines, with hard timeout.
    t0=$(date +%s.%N)
    if ! "$BINARY" boot -l "$L2CPU" -d "$ROOTFS" -n >/dev/null 2>&1; then
        errors+=("boot-rpc-failed")
    fi

    # Wait for the 4 expected DRIVER_OK lines (rng, blk, net, console).
    # The daemon log is the source of truth; busy-wait with a deadline.
    # `grep -c` always prints an integer (0 on no match) but exits 1 if
    # there were zero matches, so we suppress the exit code with `|| :`
    # rather than an `|| echo 0` that would duplicate the count.
    deadline=$(awk "BEGIN{printf \"%.2f\", $(date +%s.%N) + $BOOT_TIMEOUT}")
    while :; do
        ok_count=$(grep -c "reached STATUS_DRIVER_OK" "$LOG_FILE" 2>/dev/null || :)
        ok_count=${ok_count:-0}
        if [ "$ok_count" -ge 4 ]; then
            break
        fi
        now=$(date +%s.%N)
        if awk "BEGIN{exit !($now > $deadline)}"; then
            errors+=("boot-timeout($ok_count/4-driver-ok)")
            break
        fi
        sleep 0.5
    done
    boot_s=$(awk "BEGIN{printf \"%.2f\", $(date +%s.%N) - $t0}")

    # 6. Per-iter assertions on daemon log content.
    driver_ok=$(grep -c "reached STATUS_DRIVER_OK" "$LOG_FILE" 2>/dev/null || :)
    driver_ok=${driver_ok:-0}
    status_resets=$(grep -c "STATUS reset to 0" "$LOG_FILE" 2>/dev/null || :)
    status_resets=${status_resets:-0}
    status_failed=$(grep -c "STATUS_FAILED set" "$LOG_FILE" 2>/dev/null || :)
    status_failed=${status_failed:-0}
    if [ "$driver_ok" -lt 4 ]; then
        errors+=("driver-ok-count=$driver_ok")
    fi
    if [ "$status_resets" -gt 0 ]; then
        errors+=("status-resets=$status_resets")
    fi
    if [ "$status_failed" -gt 0 ]; then
        errors+=("status-failed=$status_failed")
    fi
    # V1-path regression sentinels.
    if grep -qE "kick.*drop|rescue|throttle.*ENGAGE" "$LOG_FILE" 2>/dev/null; then
        errors+=("v1-path-log-line")
    fi
    # Daemon panic / fatal — daemon dying mid-boot would be ugly.
    if grep -qE "^\[.*panic|^\[.*fatal" "$LOG_FILE" 2>/dev/null; then
        errors+=("daemon-panic-or-fatal")
    fi

    # 7. Tear down for next iter.
    "$BINARY" daemon stop -t "$CARD" 2>/dev/null || true

    iter_total=$(awk "BEGIN{printf \"%.2f\", $(date +%s.%N) - $iter_start}")

    if [ "${#errors[@]}" -eq 0 ]; then
        PASS=$((PASS + 1))
        echo "$i,PASS,$reset_s,$daemon_s,$boot_s,$driver_ok,$status_resets,$status_failed," >> "$RESULT_CSV"
        note "iter $i/$ITERATIONS PASS  (reset=${reset_s}s daemon=${daemon_s}s boot=${boot_s}s total=${iter_total}s, ${driver_ok}×DRIVER_OK)"
    else
        FAIL=$((FAIL + 1))
        FAIL_ITERS+=("$i")
        err_joined=$(IFS=,; echo "${errors[*]}")
        echo "$i,FAIL,$reset_s,$daemon_s,$boot_s,$driver_ok,$status_resets,$status_failed,\"$err_joined\"" >> "$RESULT_CSV"
        # Archive the daemon log of this failed iteration for inspection.
        cp "$LOG_FILE" "$FAIL_LOG_DIR/iter-$i.log" 2>/dev/null || true
        note "iter $i/$ITERATIONS FAIL ($err_joined)  (saved log to $FAIL_LOG_DIR/iter-$i.log)"
    fi
done

echo ""
note "===== summary ====="
note "iterations: $ITERATIONS  pass: $PASS  fail: $FAIL"
note "results CSV: $RESULT_CSV"
if [ "$FAIL" -gt 0 ]; then
    note "failed iters: ${FAIL_ITERS[*]}"
    note "failed iter logs in: $FAIL_LOG_DIR"
    rate=$(awk "BEGIN{printf \"%.1f\", 100.0 * $FAIL / $ITERATIONS}")
    echo "FAIL: $FAIL/$ITERATIONS cold-boot iterations failed (${rate}%)" >&2
    exit 1
fi
echo "PASS: $ITERATIONS/$ITERATIONS cold-boot iterations clean (driver_ok=4 each, no resets, no failures)"
