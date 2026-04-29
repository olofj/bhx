#!/usr/bin/env python3
"""
Stress-test `remove-disk` against a guest running real disk I/O via
`fio`. Complements `soak_disk_io_pressure.sh`, which relies on
incidental kernel-journal writes — fio gives us controllable,
reproducible high write pressure on the rootfs at the moment we yank
the disk.

Targets a buildroot-flavored rootfs (auto-login on hvc0, fio in
target/bin). For Debian, fall back to soak_disk_io_pressure.sh.

Plan (per iteration):
  1. (First iteration only) cold-boot l2cpu N with the buildroot
     rootfs + net.
  2. Start a fio job inside the guest writing 64 MiB to /root/fio.tmp
     in the background (`fio --output-format=terse > /tmp/fio.log &`).
     Wait briefly so it's actually issuing virtio-blk descriptors.
  3. From the host: `remove-disk` and assert it returns within
     TIMEOUT seconds.
  4. Confirm the daemon's pid is still alive.
  5. `add-disk` to re-attach so the next iteration has something to
     yank.
  6. Sleep briefly so the new disk is mounted before the next fio.

Usage:
    soak_fio_remove_disk.py [--l2cpu N] [--ttdevice N] [--iterations N]

Env overrides (CARD, BINARY, ROOTFS, LOG_FILE, ITERATIONS, TIMEOUT)
are read for parity with the bash soaks.
"""

import argparse
import os
import subprocess
import sys
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument("--l2cpu", type=int, default=int(os.environ.get("L2CPU", "0")))
parser.add_argument("--ttdevice", type=int, default=int(os.environ.get("CARD", "0")))
parser.add_argument(
    "--iterations", type=int, default=int(os.environ.get("ITERATIONS", "5"))
)
parser.add_argument(
    "--timeout-s", type=float, default=float(os.environ.get("TIMEOUT", "5"))
)
parser.add_argument(
    "--fio-runtime", type=int, default=int(os.environ.get("FIO_RUNTIME", "60"))
)
args = parser.parse_args()

BINARY = os.environ.get("BINARY", os.path.abspath("./target/debug/bhx"))
LOG_FILE = os.environ.get("LOG_FILE", os.path.abspath("./daemon-card0.log"))
ROOTFS_DEFAULT_BUILDROOT = "tests/rootfs/rootfs.ext4"
ROOTFS_DEFAULT_LEGACY = "rootfs.ext4"
ROOTFS = os.environ.get("ROOTFS")
if not ROOTFS:
    if os.path.exists(ROOTFS_DEFAULT_BUILDROOT):
        ROOTFS = ROOTFS_DEFAULT_BUILDROOT
    elif os.path.exists(ROOTFS_DEFAULT_LEGACY):
        ROOTFS = ROOTFS_DEFAULT_LEGACY
TTDEVICE = str(args.ttdevice)
L2CPU = str(args.l2cpu)
_runtime_dir = os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
PIDFILE = f"{_runtime_dir}/bhx/{TTDEVICE}/pid"
TAG = f"[fio-soak l2cpu={L2CPU}]"


def fail(msg: str) -> None:
    print(f"FAIL: {msg}", flush=True, file=sys.stderr)
    sys.exit(1)


def note(msg: str) -> None:
    print(f"{TAG} {msg}", flush=True)


# ---- Sanity checks --------------------------------------------------------
if not os.access(BINARY, os.X_OK):
    fail(f"{BINARY} not executable (run cargo build first)")
if not ROOTFS or not os.path.exists(ROOTFS):
    fail("no rootfs available; build tests/rootfs or set ROOTFS=<path>")
for f in ("fw_jump.bin", "Image", "blackhole-card.dtb"):
    if not os.path.exists(f):
        fail(f"{f} missing in cwd")


def run(*argv: str, check: bool = True, timeout: float | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        [BINARY, *argv], capture_output=True, text=True, check=check, timeout=timeout
    )


# ---- Boot ----------------------------------------------------------------
note(f"tt-smi -r (cold chip)")
subprocess.run(
    ["bash", "-c", "(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1"],
    check=False,
)

if os.path.exists(LOG_FILE):
    os.remove(LOG_FILE)

note("daemon start")
subprocess.run(
    [BINARY, "daemon", "start", "-t", TTDEVICE, "--log-file", LOG_FILE],
    check=True,
)
time.sleep(0.3)

note(f"cold boot L2CPU {L2CPU} with rootfs={ROOTFS}")
subprocess.run(
    [BINARY, "boot", "-t", TTDEVICE, "-l", L2CPU, "-d", ROOTFS],
    check=True,
    timeout=90,
)

# ---- Console session: open once, drive across all iterations -------------
# Re-using a single connect avoids the slirp+ssh + login dance per iter.
# auto-login (buildroot) means we land at `# ` immediately.
note("opening connect for guest-side fio control")
proc = subprocess.Popen(
    [BINARY, "connect", "-t", TTDEVICE, "-l", L2CPU],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL,
    bufsize=0,
)


buf = bytearray()
buf_lock = threading.Lock()


def reader() -> None:
    while True:
        chunk = proc.stdout.read(8192)
        if not chunk:
            return
        with buf_lock:
            buf.extend(chunk)


threading.Thread(target=reader, daemon=True).start()


def wait_for(needle: bytes, timeout_s: float, from_idx: int = 0) -> int:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        with buf_lock:
            idx = bytes(buf).find(needle, from_idx)
            if idx >= 0:
                return idx
        time.sleep(0.05)
    with buf_lock:
        tail = bytes(buf)[-400:]
    note(f"TIMEOUT waiting for {needle!r} — last 400 bytes: {tail!r}")
    proc.kill()
    sys.exit(10)


def send(data: bytes) -> None:
    proc.stdin.write(data)
    proc.stdin.flush()


# Wait for the buildroot auto-login `# ` prompt (or Debian's `$ `).
note("waiting for shell prompt")
deadline = time.time() + 60
prompt = None
while time.time() < deadline:
    with buf_lock:
        snap = bytes(buf)
    if snap.find(b"# ") >= 0:
        prompt = b"# "
        note("auto-login detected (buildroot)")
        break
    if snap.find(b"login:") >= 0:
        send(b"debian\r")
        note("sent 'debian\\r' (Debian-style login)")
        time.sleep(0.5)
        prompt = b"$ "
        break
    time.sleep(0.1)
if prompt is None:
    fail("never reached a shell prompt within 60 s")

# Quiet kernel printk so it doesn't interleave with our prompt-grepping.
send(b"dmesg -n 1\r")
with buf_lock:
    cursor = len(buf)
wait_for(prompt, 5, from_idx=cursor)


# ---- Helper: drive fio in the guest --------------------------------------
def start_fio_in_guest() -> None:
    """Background-fork fio in the guest. Writes to /root/fio.tmp on the
    rootfs (vda) so the daemon's virtio-blk worker actually sees the
    descriptors. The PID file lets us kill it if anything goes wrong."""
    cmd = (
        b"fio --name=stress --rw=randwrite --bs=4k "
        b"--size=64M --runtime=" + str(args.fio_runtime).encode() + b" "
        b"--filename=/root/fio.tmp --direct=0 "
        b"--output=/tmp/fio.log "
        b">/dev/null 2>&1 & echo $! > /tmp/fio.pid; "
        b"echo FIO_STARTED:$(cat /tmp/fio.pid)\r"
    )
    with buf_lock:
        cursor = len(buf)
    send(cmd)
    idx = wait_for(b"FIO_STARTED:", 30, from_idx=cursor)
    # Sleep so fio actually starts issuing descriptors (the runtime
    # parameter is when it stops; we want it actively writing when we
    # call remove-disk).
    time.sleep(2)
    return idx


def kill_fio_in_guest() -> None:
    cmd = b"kill -9 $(cat /tmp/fio.pid 2>/dev/null) 2>/dev/null; rm -f /tmp/fio.pid /root/fio.tmp; echo FIO_KILLED\r"
    with buf_lock:
        cursor = len(buf)
    send(cmd)
    # Best-effort — if connect dropped because remove-disk killed the
    # virtio-blk worker, the next add-disk's re-mount will reset
    # everything anyway.
    try:
        wait_for(b"FIO_KILLED", 5, from_idx=cursor)
    except SystemExit:
        pass


# ---- Soak loop -----------------------------------------------------------
note(f"starting {args.iterations} fio + remove-disk cycles")
with open(PIDFILE) as f:
    initial_pid = f.read().strip()

for i in range(1, args.iterations + 1):
    print(f"---- iter {i}/{args.iterations} ----", flush=True)

    fio_start_idx = start_fio_in_guest()
    note(f"iter {i}: fio active in guest")

    note(f"iter {i}: remove-disk (timeout {args.timeout_s}s)")
    t0 = time.time()
    try:
        subprocess.run(
            [BINARY, "remove-disk", "-t", TTDEVICE, "-l", L2CPU],
            check=True,
            timeout=args.timeout_s,
        )
    except subprocess.TimeoutExpired:
        fail(f"iter {i}: remove-disk did not return within {args.timeout_s}s")
    elapsed_ms = (time.time() - t0) * 1000
    note(f"iter {i}: remove-disk returned in {elapsed_ms:.0f}ms")

    # Daemon survived?
    try:
        os.kill(int(initial_pid), 0)
    except ProcessLookupError:
        fail(f"iter {i}: daemon (pid {initial_pid}) died during remove-disk")

    status = subprocess.run(
        [BINARY, "daemon", "status", "-t", TTDEVICE], capture_output=True, text=True
    ).stdout
    # Match the specific l2cpu's line so we don't pick up `disk=-` from
    # sibling Stopped slots.
    target_line = next(
        (line for line in status.splitlines() if line.lstrip().startswith(f"l2cpu {L2CPU}:")),
        "",
    )
    if "Running disk=-" not in target_line:
        fail(f"iter {i}: post-remove status not 'disk=-' for l2cpu {L2CPU}:\n{status}")

    # Re-attach. From the guest's perspective, the underlying file
    # changed mid-write; vda is now a fresh device. The kernel's
    # in-flight fio writes failed with EIO; the orphan fio process is
    # still hanging around but its PID file is stale. Best-effort
    # kill from the console (which may itself hang if the rootfs is
    # the same image — but we re-add fast, so the kernel's vfs sees
    # a working /vda again quickly).
    subprocess.run(
        [BINARY, "add-disk", "-t", TTDEVICE, "-l", L2CPU, ROOTFS],
        check=True,
    )
    status = subprocess.run(
        [BINARY, "daemon", "status", "-t", TTDEVICE], capture_output=True, text=True
    ).stdout
    rootfs_basename = os.path.basename(os.path.realpath(ROOTFS))
    target_line = next(
        (line for line in status.splitlines() if line.lstrip().startswith(f"l2cpu {L2CPU}:")),
        "",
    )
    if rootfs_basename not in target_line or "Running disk=-" in target_line:
        fail(f"iter {i}: post-readd status mismatch:\n{status}")

    # Settle so the guest's vfs notices the new vda before the next fio.
    time.sleep(3)
    kill_fio_in_guest()
    time.sleep(1)
    note(f"iter {i}: cycle OK")


# ---- Cleanup -------------------------------------------------------------
note("final daemon stop")
proc.kill()
proc.wait(timeout=2)
subprocess.run([BINARY, "daemon", "stop", "-t", TTDEVICE], check=False)

print()
print(
    f"PASS: {args.iterations} fio+remove-disk cycles on card {TTDEVICE} L2CPU {L2CPU}"
)
