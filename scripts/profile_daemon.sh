#!/usr/bin/env bash
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT

#
# Capture a CPU profile of the daemon for a fixed duration. Lets us
# answer "what's burning the idle 6%?" or "where is the disk path
# spending its time under fio?" without re-deriving the incantation.
#
# Plan:
#   1. cargo build --profile profiling (opt + debug info; symbols).
#   2. tt-smi -r; daemon start (using the profiling-build binary).
#   3. boot l2cpu N with the buildroot rootfs + net (or whatever
#      ROOTFS resolves to).
#   4. Wait for the requested workload phase (idle / fio / soak).
#   5. samply record --pid $DAEMON --duration $DURATION → flamegraph.
#   6. daemon stop.
#
# Output:
#   profile-<scenario>-<timestamp>.json (samply)
#   View via `samply load <file>` or `samply serve`.
#
# Env / args:
#   --scenario {idle,fio,soak}   workload (default: idle)
#   --duration N                 profiling window in seconds (default: 30)
#   --l2cpu N / --card N         which to boot (defaults: 0/0)
#   --binary PATH                use this binary (skip rebuild). Defaults
#                                to ./target/profiling/bhx.
#
# Prereqs:
#   - samply installed: `cargo install samply`
#   - For `fio` / `soak` scenarios: a buildroot rootfs at
#     third_party/buildroot/rootfs.ext4 (auto-login + fio in target/bin).

set -euo pipefail

SCENARIO=idle
DURATION=30
L2CPU=${L2CPU:-0}
CARD=${CARD:-0}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
BINARY=${BINARY:-}

while [ $# -gt 0 ]; do
    case "$1" in
        --scenario) SCENARIO=$2; shift 2 ;;
        --duration) DURATION=$2; shift 2 ;;
        --l2cpu)    L2CPU=$2; shift 2 ;;
        --card)     CARD=$2; shift 2 ;;
        --binary)   BINARY=$2; shift 2 ;;
        --help|-h)
            sed -n '2,/^$/p' "$0" | sed 's/^# *//'
            exit 0
            ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

case "$SCENARIO" in idle|fio|soak) ;; *)
    echo "FAIL: --scenario must be one of: idle, fio, soak" >&2; exit 2 ;;
esac

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "[profile] $*"; }

command -v samply >/dev/null || fail "samply not installed; \`cargo install samply\`"

# Build profiling binary unless caller supplied one.
if [ -z "$BINARY" ]; then
    note "cargo build --profile profiling"
    cargo build --profile profiling
    BINARY=./target/profiling/bhx
fi
[ -x "$BINARY" ] || fail "binary $BINARY not executable"

# Resolve rootfs (matches the soak scripts' three-tier search).
if [ -z "${ROOTFS:-}" ]; then
    if [ -e third_party/buildroot/rootfs.ext4 ]; then
        ROOTFS=third_party/buildroot/rootfs.ext4
    elif [ -e rootfs.ext4 ]; then
        ROOTFS=rootfs.ext4
    fi
fi
[ -n "${ROOTFS:-}" ] && [ -e "$ROOTFS" ] || fail "no rootfs available"

# Cleanup trap.
DAEMON_PID=""
CONNECT_PID=""
cleanup() {
    [ -n "$CONNECT_PID" ] && kill -9 "$CONNECT_PID" 2>/dev/null || true
    "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true
}
trap cleanup EXIT

note "tt-smi -r"
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1

rm -f "$LOG_FILE"
note "daemon start ($BINARY)"
"$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
sleep 0.3

note "cold boot L2CPU $L2CPU (rootfs=$ROOTFS, net=on)"
timeout 90 "$BINARY" boot -t "$CARD" -l "$L2CPU" -d "$ROOTFS" -n >/dev/null

DAEMON_PID=$(cat "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/bhx/$CARD/pid")
note "daemon pid=$DAEMON_PID"

# Let the guest reach steady state (DHCP, dropbear, fs).
note "guest warm-up (15 s)"
sleep 15

ts=$(date +%s)
out="profile-${SCENARIO}-${ts}.json.gz"

case "$SCENARIO" in
    idle)
        note "scenario=idle (no extra workload); recording ${DURATION}s"
        samply record --pid "$DAEMON_PID" --duration "$DURATION" --save-only -o "$out"
        ;;
    fio)
        # Drive a fio job inside the guest concurrently with the recording.
        # We need to send commands via `connect` — open a pipe, dump
        # the command, leave the connect open until cleanup.
        note "scenario=fio (driving guest fio for ${DURATION}s)"
        FIFO=$(mktemp -u)
        mkfifo "$FIFO"
        (
            # Hold the FIFO open so connect's stdin doesn't EOF.
            sleep "$((DURATION + 5))" > "$FIFO" &
            holder=$!
            # Send fio start over the console.
            cat > "$FIFO" <<EOF
dmesg -n 1
fio --name=prof --rw=randwrite --bs=4k --size=32M --runtime=$DURATION \
    --filename=/root/fio.tmp --direct=0 --output=/tmp/fio.log \
    >/dev/null 2>&1 &
echo FIO_STARTED
EOF
            wait "$holder" 2>/dev/null || true
        ) | "$BINARY" connect -t "$CARD" -l "$L2CPU" >/dev/null 2>&1 &
        CONNECT_PID=$!
        # Give the FIO_STARTED echo a moment to land.
        sleep 3
        samply record --pid "$DAEMON_PID" --duration "$DURATION" --save-only -o "$out"
        ;;
    soak)
        # Run the existing concurrent soak in parallel with profiling.
        # The 4-way add/remove hammer drives the most paths at once.
        note "scenario=soak (running soak_concurrent in parallel)"
        ITERATIONS=$((DURATION / 30 + 1)) bash scripts/soak_concurrent.sh \
            >/tmp/soak_concurrent_during_profile.log 2>&1 &
        SOAK_PID=$!
        sleep 3
        samply record --pid "$DAEMON_PID" --duration "$DURATION" --save-only -o "$out"
        wait "$SOAK_PID" 2>/dev/null || true
        ;;
esac

note "profile saved to $out"
note "view via:  samply load $out"

note "final daemon stop"
"$BINARY" daemon stop -t "$CARD" >/dev/null
trap - EXIT

echo
echo "PASS: profile captured in $out (${DURATION}s, scenario=$SCENARIO)"
