#!/usr/bin/env bash
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# 3-guest async soak for the OpenSBI-purgatory soft-reboot path (#166).
#
# Per-guest startup script in `rootfs-suicide.ext2` does:
#   sleep 10
#   poweroff -f
# so each guest auto-issues SBI SRST about 10 seconds after reaching
# userspace. The harness watches each L2CPU independently:
#
#   for each L2CPU in parallel:
#       loop:
#           wait for purgatory == PARKED
#           bhx boot -l N    (release-from-purgatory)
#           bump counter; record any anomaly
#
# No barriers between L2CPUs — a stuck guest doesn't block the others.
# Stuck guests get logged + counted, the loop moves on to the next iter.
#
# Env:
#   ITERATIONS    cycles per L2CPU before the harness exits (default 100)
#   GUESTS        space-separated L2CPU indices (default "0 1 2")
#   BINARY        bhx binary (default ./target/debug/bhx)
#   LOG_FILE      daemon log path (default ./daemon-card0.log)
#   CARD          tt device index (default 0)
#   ROOTFS        suicide rootfs (default ./rootfs-suicide.ext2)
#   PARK_TIMEOUT  per-iter timeout waiting for PARKED (default 60s)

set -euo pipefail

ITERATIONS=${ITERATIONS:-100}
GUESTS_STR=${GUESTS:-"0 1 2"}
read -ra GUESTS <<<"$GUESTS_STR"
BINARY=${BINARY:-./target/debug/bhx}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
CARD=${CARD:-0}
ROOTFS=${ROOTFS:-./rootfs-suicide.ext2}
PARK_TIMEOUT=${PARK_TIMEOUT:-60}
PARKED_MAGIC="0x5f5f44454b524150"
STATE_DIR="$(mktemp -d /tmp/bhx-soak.XXXXXX)"

note() { echo "[soak] $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

cleanup() {
    # Kill per-guest watchers cleanly so they stop spamming releases
    # while the daemon is on its way out.
    for i in "${GUESTS[@]}"; do
        local pidfile="$STATE_DIR/guest-$i.pid"
        if [ -f "$pidfile" ]; then
            local p
            p=$(cat "$pidfile")
            if [ -n "$p" ] && kill -0 "$p" 2>/dev/null; then
                kill "$p" 2>/dev/null || true
            fi
        fi
    done
    sleep 0.5
    "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true
    note "state dir: $STATE_DIR"
}
trap cleanup EXIT

[ -x "$BINARY" ] || fail "binary $BINARY not executable (cargo build first)"
[ -e "$ROOTFS" ] || fail "rootfs $ROOTFS missing — build with 'cp third_party/buildroot/rootfs-l2cpu1-quiet.ext2 rootfs-suicide.ext2 && e2cp -P 0755 scripts/soak_suicide_init.sh rootfs-suicide.ext2:/etc/init.d/S99-bhx-suicide'"
[ -e fw_jump.bin ] || fail "fw_jump.bin missing"
[ -e Image ] || fail "Image missing"
[ -e blackhole-card.dtb ] || fail "blackhole-card.dtb missing"

# Per-guest rootfs copies so concurrent disk workers don't share an
# mmap'd backing file.
for i in "${GUESTS[@]}"; do
    cp --reflink=auto "$ROOTFS" "$STATE_DIR/rootfs-$i.ext2"
done

note "tt-smi -r (cold chip)"
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1

rm -f "$LOG_FILE"
note "daemon start (ITERATIONS=$ITERATIONS, GUESTS=${GUESTS[*]})"
"$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
sleep 0.3

# Cold boot all selected L2CPUs once.
note "cold boot: ${GUESTS[*]}"
for i in "${GUESTS[@]}"; do
    "$BINARY" boot -t "$CARD" -l "$i" \
        -d "$STATE_DIR/rootfs-$i.ext2" >/dev/null \
        || fail "cold boot l2cpu $i failed"
done

# Read a single L2CPU's purgatory cell (regex-anchored so format
# changes to other purgatory subfields don't break the parse).
read_purgatory() {
    local idx=$1
    "$BINARY" daemon status -t "$CARD" 2>/dev/null \
      | awk -v idx="$idx" '
          $0 ~ "^  l2cpu " idx ":"           { in_block=1; next }
          in_block && /^  l2cpu /             { in_block=0 }
          in_block && /^    purgatory: / {
              if (match($0, /\(0x[0-9a-fA-F]+\)/)) {
                  hex = substr($0, RSTART, RLENGTH)
                  gsub(/[()]/, "", hex)
                  print hex
                  exit
              }
          }'
}

# Per-guest watcher: each runs in the background, manages its own L2CPU
# without touching the others. Output prefixed `[guest N]` so a `tail
# -f` of the soak log shows interleaved per-guest progress in real time.
guest_loop() {
    local idx=$1
    local n=0
    local stuck=0
    local prev_purg=""
    local prev_purg_since=0

    while [ "$n" -lt "$ITERATIONS" ]; do
        local now elapsed v
        now=$(date +%s)
        v=$(read_purgatory "$idx" || true)

        if [ "$v" = "$PARKED_MAGIC" ]; then
            n=$((n + 1))
            echo "[guest $idx] iter $n/$ITERATIONS: PARKED — releasing"
            if "$BINARY" boot -t "$CARD" -l "$idx" \
                  -d "$STATE_DIR/rootfs-$idx.ext2" >/dev/null 2>&1; then
                echo "[guest $idx] iter $n: released"
            else
                stuck=$((stuck + 1))
                echo "[guest $idx] iter $n: release RPC failed (stuck=$stuck)" >&2
                # Don't bail; let the next iter retry.
                sleep 5
            fi
            prev_purg=""
            prev_purg_since=0
        else
            # Detect a stuck guest: same non-PARKED value for too long
            # means the kernel either hasn't reached our suicide script
            # (slow boot) or wedged before getting there.
            if [ -z "$prev_purg" ] || [ "$prev_purg" != "$v" ]; then
                prev_purg="$v"
                prev_purg_since=$now
            elif [ "$prev_purg_since" -gt 0 ]; then
                elapsed=$((now - prev_purg_since))
                if [ "$elapsed" -ge "$PARK_TIMEOUT" ]; then
                    stuck=$((stuck + 1))
                    echo "[guest $idx] iter $((n+1)): stuck for ${elapsed}s on purgatory=$v (stuck=$stuck) — keep watching" >&2
                    prev_purg_since=$now  # reset window so we log every PARK_TIMEOUT
                fi
            fi
            sleep 1
        fi
    done

    echo "[guest $idx] DONE: completed $ITERATIONS iters, stuck count=$stuck"
}

# Spawn one watcher per guest and write its PID to STATE_DIR so cleanup
# can reap it.
note "spawning per-guest watchers"
for i in "${GUESTS[@]}"; do
    guest_loop "$i" &
    echo "$!" > "$STATE_DIR/guest-$i.pid"
done

# Wait for all per-guest watchers to finish their ITERATIONS targets.
# `wait` without args reaps all child processes started by this shell.
wait

note "all watchers complete"
