#!/usr/bin/env bash
#
# Stress-test `remove-net` while a host-side TCP session is alive.
#
# slirp's TCP forwarder spins up host-side accept sockets bound to
# 127.0.0.1:<ssh_port>. When `remove-net` tears down the slirp instance,
# any live forwarded session should drop cleanly (not leave the daemon
# hung, not leak fds, not crash the host).
#
# We don't need a real SSH login to exercise this — opening a TCP
# connection to the forwarded port and holding it is enough. nc with an
# input pipe held open by `sleep infinity` keeps the connection alive
# without sending any application bytes.
#
# Plan:
#   1. tt-smi -r; daemon start; boot -l 0 -d ... -n.
#   2. Loop N iterations:
#        a. Compute forward port = 2222 + L2CPU + 4*CARD.
#        b. Wait until the port accepts TCP connections (i.e. slirp +
#           guest sshd are both up; gating on this is what makes the
#           per-iteration setup deterministic).
#        c. Open a held connection via a python one-liner that connects
#           and sleeps. python here because the `nc` shipping on Debian
#           is the v1.10 Hobbit netcat which exits as soon as the peer
#           half-closes — that flap is what we're trying to detect, so
#           we can't use the tool that hides it.
#        d. Verify the connection is up (the python is alive after the
#           connect succeeded).
#        e. Call remove-net; assert it returns within TIMEOUT seconds.
#        f. The held connection must die — either immediately when
#           slirp shuts down its accept socket, or shortly after as
#           the TCP session is reaped. Still alive 10 s later = bug.
#        g. add-net to set up the next iteration.
#   3. Final daemon stop.
#
# Env:
#   ITERATIONS  default 5
#   BINARY      default ./target/debug/bhx
#   LOG_FILE    default ./daemon-card0.log
#   CARD        default 0
#   L2CPU       default 0
#   TIMEOUT     remove-net timeout in seconds (default 5)
#   PORT_WAIT   max seconds to wait for slirp to bring up the listener
#               (default 60 — guest sshd takes longest on first cold boot)

set -euo pipefail

ITERATIONS=${ITERATIONS:-5}
BINARY=${BINARY:-./target/debug/bhx}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
CARD=${CARD:-0}
L2CPU=${L2CPU:-0}
TIMEOUT=${TIMEOUT:-5}
PORT_WAIT=${PORT_WAIT:-60}

PIDFILE="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/bhx/${CARD}/pid"
SSH_PORT=$(( 2222 + L2CPU + 4 * CARD ))

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "[soak] $*"; }

held_pids=()
cleanup() {
    for p in "${held_pids[@]}"; do
        kill -9 "$p" 2>/dev/null || true
    done
    "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Resolve rootfs (ROOTFS env > buildroot > legacy ./rootfs.ext4).
if [ -z "${ROOTFS:-}" ]; then
    if [ -e tests/rootfs/rootfs.ext4 ]; then
        ROOTFS=tests/rootfs/rootfs.ext4
    elif [ -e rootfs.ext4 ]; then
        ROOTFS=rootfs.ext4
    fi
fi

# Sanity checks -------------------------------------------------------------
[ -x "$BINARY" ] || fail "binary $BINARY not executable (run cargo build first)"
[ -n "${ROOTFS:-}" ] && [ -e "$ROOTFS" ] \
    || fail "no rootfs available; build tests/rootfs or set ROOTFS=<path>"
[ -e fw_jump.bin ] || fail "fw_jump.bin missing"
[ -e Image ] || fail "Image missing"
[ -e blackhole-card.dtb ] || fail "blackhole-card.dtb missing"
command -v nc >/dev/null || fail "nc (netcat) required"
command -v python3 >/dev/null || fail "python3 required (used for the held TCP session)"

# Step 1: cold chip + boot --------------------------------------------------
note "tt-smi -r (cold chip)"
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1

rm -f "$LOG_FILE"
note "daemon start"
"$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
sleep 0.3

note "cold boot L2CPU $L2CPU with disk+net (rootfs=$ROOTFS, ssh fwd port $SSH_PORT)"
timeout 90 "$BINARY" boot -t "$CARD" -l "$L2CPU" -d "$ROOTFS" -n >/dev/null

# Daemon stores the canonicalized path; match its basename.
rootfs_basename=$(basename "$(readlink -f "$ROOTFS")")
status=$("$BINARY" daemon status -t "$CARD")
echo "$status" | grep -qE "l2cpu $L2CPU: Running disk=.*$rootfs_basename net=y" \
    || fail "post-boot status mismatch:\n$status"
note "post-boot status OK; daemon pid $(cat "$PIDFILE")"

# Wait for the guest's sshd to come up so the slirp forwarder actually
# accepts. nc -z probes by attempting a TCP connect with no data. We
# require N successful probes in a row to filter out the brief window
# where the new slirp instance has a listener up but the guest's sshd
# hasn't re-handshaked yet — that race causes a held nc to die mid-iter.
wait_for_port() {
    local deadline=$(( $(date +%s) + PORT_WAIT ))
    local consecutive=0
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if nc -z -w 1 127.0.0.1 "$SSH_PORT" 2>/dev/null; then
            consecutive=$(( consecutive + 1 ))
            if [ "$consecutive" -ge 3 ]; then
                return 0
            fi
        else
            consecutive=0
        fi
        sleep 1
    done
    return 1
}

note "waiting up to ${PORT_WAIT}s for guest sshd via slirp"
wait_for_port || fail "guest sshd never came up on port $SSH_PORT"
note "port $SSH_PORT accepting"

# Step 2: net-teardown loop -------------------------------------------------
note "starting $ITERATIONS remove-net-under-active-session cycles"
for i in $(seq 1 "$ITERATIONS"); do
    echo "---- iter $i/$ITERATIONS ----"

    # Open a held TCP connection via python. We have to actively read
    # the socket so the python notices when slirp drops it — without a
    # read loop the kernel parks the connection in CLOSE_WAIT and the
    # python process blissfully sleeps forever. The Debian v1.10 nc
    # would also close fine here but it lacks a way to *not* close
    # when the server half-closes during the period we're testing
    # against, which is exactly the flap we're trying to verify.
    #
    # Sync via a ready-file: cold python startup + create_connection +
    # the SSH banner exchange take longer than a fixed sleep is
    # comfortable with, so we touch a file once the socket is up and
    # spin on its existence here.
    ready_file=$(mktemp)
    rm -f "$ready_file"  # mktemp creates it; we want existence as a signal
    python3 -c "
import socket, sys
try:
    s = socket.create_connection(('127.0.0.1', $SSH_PORT), timeout=10)
except Exception as e:
    print(f'connect failed: {e}', file=sys.stderr)
    sys.exit(1)
open('$ready_file', 'w').close()
s.settimeout(0.5)
while True:
    try:
        data = s.recv(4096)
        if not data:
            sys.exit(0)
    except socket.timeout:
        continue
    except OSError:
        sys.exit(0)
" >/dev/null 2>&1 &
    held_pid=$!
    held_pids+=("$held_pid")

    deadline=$(( $(date +%s) + 15 ))
    while [ ! -e "$ready_file" ]; do
        if ! kill -0 "$held_pid" 2>/dev/null; then
            fail "iter $i: held python TCP session died before signaling ready"
        fi
        if [ "$(date +%s)" -ge "$deadline" ]; then
            kill -9 "$held_pid" 2>/dev/null || true
            fail "iter $i: held python TCP session didn't reach ready in 15s"
        fi
        sleep 0.1
    done
    rm -f "$ready_file"
    note "iter $i: held TCP session up (python pid $held_pid)"

    pid=$(cat "$PIDFILE")
    note "iter $i: remove-net (timeout ${TIMEOUT}s)"
    start=$(date +%s%N)
    timeout "$TIMEOUT" "$BINARY" remove-net -t "$CARD" -l "$L2CPU" \
        || fail "iter $i: remove-net did not return within ${TIMEOUT}s"
    end=$(date +%s%N)
    elapsed_ms=$(( (end - start) / 1000000 ))
    note "iter $i: remove-net returned in ${elapsed_ms}ms"

    kill -0 "$pid" 2>/dev/null \
        || fail "iter $i: daemon (pid $pid) died during remove-net"

    # Wait for the held TCP session to die. slirp dropping its accept
    # socket causes the kernel to send RST/FIN to the host endpoint
    # within a few hundred ms.
    deadline=$(( $(date +%s) + 10 ))
    while kill -0 "$held_pid" 2>/dev/null; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            kill -9 "$held_pid" 2>/dev/null || true
            fail "iter $i: held TCP session still alive 10s after remove-net"
        fi
        sleep 0.2
    done
    note "iter $i: held TCP session torn down cleanly"

    status=$("$BINARY" daemon status -t "$CARD")
    echo "$status" | grep -qE "l2cpu $L2CPU: Running .* net=-" \
        || fail "iter $i: post-remove status not 'net=-':\n$status"

    # Re-attach for the next iteration.
    "$BINARY" add-net -t "$CARD" -l "$L2CPU" >/dev/null \
        || fail "iter $i: add-net failed"
    note "iter $i: re-attached net; waiting for guest sshd to be reachable again"
    wait_for_port || fail "iter $i: guest sshd unreachable after add-net"

    note "iter $i: remove-net under active session OK"
done

note "final daemon stop"
"$BINARY" daemon stop -t "$CARD" >/dev/null
trap - EXIT

echo
echo "PASS: $ITERATIONS remove-net-under-active-session cycles on card $CARD L2CPU $L2CPU"
