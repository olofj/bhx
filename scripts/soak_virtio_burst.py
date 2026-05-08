#!/usr/bin/env python3
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT

"""
Sustained multi-queue virtio burst regression test for V2 dispatch.

Drives concurrent block (fio randwrite at high iodepth) and console
(tight printf loop to /dev/console) workload inside the guest for
DURATION_SEC seconds. While the workload runs, the host samples the
daemon's `/metrics` endpoint every 1 s and writes a CSV.

After the workload window, the script asserts:

  - No daemon-log line matches `kick.*drop|rescue|throttle.*ENGAGE`
    (regex catches V1 path activity that should not exist post-V2;
    `kick` is also the natural English word for guest QUEUE_NOTIFY,
    so a future stray dlog using "kick" in V2 vocab would also
    flag — false positive on a synonym is preferable to silently
    missing a real V1-path regression).
  - `bhx_dispatch_passes_total > 0` (workload reached the V2
    dispatch path; not just bench-ran but didn't actually dispatch
    anything).

Targets the buildroot rootfs in `third_party/buildroot/`. Auto-login
on `# `, fio in target/bin.

Originally (#186) this script also had a `--mode baseline` for
measuring V1 ring-fill against pre-V2 main. After V2.1 (#187 / #188
/ #189 / #190) merged, the V1 metrics it relied on were deleted from
the daemon, so baseline mode would forever read 0 and FAIL its
high-water gate. Removed in the V2.1 cleanup pass — anyone
benchmarking V1 against pre-V2 builds can `git checkout 0ff063f^`.

Env / args:
  --duration-sec N        DURATION_SEC env (default 300)
  --l2cpu N               L2CPU env (default 0)
  --ttdevice N            CARD env (default 0)
  --metrics-port N        METRICS_PORT env (default 9090)
  --csv PATH              CSV env (default ./soak_virtio_burst-<ts>.csv)
  --skip-boot             reuse a daemon+guest already running
  BINARY                  bhx binary (default ./target/debug/bhx)
  LOG_FILE                daemon log path (default ./daemon-card0.log)
  ROOTFS                  rootfs path (auto-detected from buildroot)

CSV columns:
  ts_iso, elapsed_s, dispatch_passes_total, dispatch_queues_drained,
  notify_events_total

Output: writes CSV; prints summary; PASS/FAIL line on exit.
"""

from __future__ import annotations

import argparse
import csv
import datetime
import os
import re
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request


# --- args & env ------------------------------------------------------------
parser = argparse.ArgumentParser(
    description=__doc__.split("\n")[0], formatter_class=argparse.RawDescriptionHelpFormatter
)
parser.add_argument(
    "--duration-sec", type=int, default=int(os.environ.get("DURATION_SEC", "300"))
)
parser.add_argument("--l2cpu", type=int, default=int(os.environ.get("L2CPU", "0")))
parser.add_argument("--ttdevice", type=int, default=int(os.environ.get("CARD", "0")))
parser.add_argument(
    "--metrics-port", type=int, default=int(os.environ.get("METRICS_PORT", "9090"))
)
parser.add_argument("--csv", default=os.environ.get("CSV"))
parser.add_argument(
    "--skip-boot",
    action="store_true",
    help="don't reset/start daemon/boot guest; reuse existing setup",
)
args = parser.parse_args()

BINARY = os.environ.get("BINARY", os.path.abspath("./target/debug/bhx"))
LOG_FILE = os.environ.get("LOG_FILE", os.path.abspath("./daemon-card0.log"))
ROOTFS = os.environ.get("ROOTFS")
if not ROOTFS:
    if os.path.exists("third_party/buildroot/rootfs.ext4"):
        ROOTFS = "third_party/buildroot/rootfs.ext4"
    elif os.path.exists("rootfs.ext4"):
        ROOTFS = "rootfs.ext4"
TTDEVICE = str(args.ttdevice)
L2CPU = str(args.l2cpu)
TAG = f"[burst l2cpu={L2CPU}]"

# The project's buildroot rootfs ships with /etc/init.d/S99-virtio-cycle-test
# (#156's net unbind/bind cycle test), which takes over the console and
# powers off — incompatible with our "boot to a shell, drive workload"
# pattern. Strip it into a private copy.
BURST_ROOTFS = os.environ.get("BURST_ROOTFS", "/tmp/burst-rootfs.ext4")

if not args.csv:
    args.csv = os.path.abspath(
        f"./soak_virtio_burst-{datetime.datetime.now():%Y%m%d-%H%M%S}.csv"
    )


def fail(msg: str) -> None:
    print(f"FAIL: {msg}", flush=True, file=sys.stderr)
    sys.exit(1)


def note(msg: str) -> None:
    print(f"{TAG} {msg}", flush=True)


# --- sanity checks ---------------------------------------------------------
if not os.access(BINARY, os.X_OK):
    fail(f"{BINARY} not executable (run cargo build first)")
if not args.skip_boot:
    if not ROOTFS or not os.path.exists(ROOTFS):
        fail(
            "no rootfs available; build third_party/buildroot or set ROOTFS=<path>"
        )
    for f in ("fw_jump.bin", "Image", "blackhole-card.dtb"):
        if not os.path.exists(f):
            fail(f"{f} missing in cwd")


# --- prep a private rootfs without the cycle-test init script -------------
def prep_burst_rootfs() -> str:
    """Make /tmp/burst-rootfs.ext4 if missing: copy of the buildroot ext4
    image with /etc/init.d/S99-virtio-cycle-test removed and any latent
    journal corruption from prior poweroff -f cycles repaired.
    """
    src = os.path.realpath(ROOTFS)
    if (
        os.path.exists(BURST_ROOTFS)
        and os.path.getmtime(BURST_ROOTFS) >= os.path.getmtime(src)
    ):
        note(f"reusing {BURST_ROOTFS} (newer than source)")
        return BURST_ROOTFS
    note(f"prepping {BURST_ROOTFS} from {src}")
    import shutil

    shutil.copyfile(src, BURST_ROOTFS)
    # Repair any latent corruption from prior poweroff -f cycles.
    subprocess.run(
        ["/sbin/e2fsck", "-fy", BURST_ROOTFS],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    # Remove the cycle-test init script.
    subprocess.run(
        ["/sbin/debugfs", "-w", "-R", "rm /etc/init.d/S99-virtio-cycle-test", BURST_ROOTFS],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return BURST_ROOTFS


# --- boot ------------------------------------------------------------------
def boot_guest() -> None:
    note("tt-smi -r (cold chip)")
    subprocess.run(
        [
            "bash",
            "-c",
            "(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1",
        ],
        check=False,
    )
    if os.path.exists(LOG_FILE):
        os.remove(LOG_FILE)
    # The sandbox blocks paths outside the project tree; common dev
    # setups symlink Image / fw_jump.bin from a sibling repo (e.g.
    # ../tt-bh-linux/Image), so default to --no-sandbox for the soak.
    # Override with BHX_SANDBOX=1 to leave it on (your firmware paths
    # need to resolve inside ./ or under the bhx XDG dirs).
    sandbox_off = os.environ.get("BHX_SANDBOX", "0") != "1"
    daemon_args = [
        BINARY,
        "daemon",
        "start",
        "-t",
        TTDEVICE,
        "--log-file",
        LOG_FILE,
        "--metrics-port",
        str(args.metrics_port),
    ]
    if sandbox_off:
        daemon_args.append("--no-sandbox")
    note(f"daemon start (metrics on :{args.metrics_port}, sandbox={'off' if sandbox_off else 'on'})")
    subprocess.run(daemon_args, check=True)
    time.sleep(0.3)
    boot_rootfs = prep_burst_rootfs()
    note(f"cold boot L2CPU {L2CPU} (rootfs={boot_rootfs})")
    subprocess.run(
        [BINARY, "boot", "-t", TTDEVICE, "-l", L2CPU, "-d", boot_rootfs, "-n"],
        check=True,
        timeout=90,
    )


if not args.skip_boot:
    boot_guest()


# --- connect / shell -------------------------------------------------------
note("opening connect for guest-side workload")
proc = subprocess.Popen(
    [BINARY, "connect", "-t", TTDEVICE, "-l", L2CPU, "--mode", "rw"],
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
    note(f"TIMEOUT waiting for {needle!r}; last 400 bytes: {tail!r}")
    proc.kill()
    sys.exit(10)


def send(data: bytes) -> None:
    proc.stdin.write(data)
    proc.stdin.flush()


# Wait for buildroot's `# ` auto-login prompt. The bhx buildroot
# overlay has a `BHX_CYCLE_SETTLE waiting 30s` pause before the shell
# fires, plus the kernel + userspace boot itself takes 30-40 s, so
# 120 s is the operating bound (real boots land ~60-70 s).
PROMPT_TIMEOUT = 120
note(f"waiting for shell prompt (up to {PROMPT_TIMEOUT}s)")
deadline = time.time() + PROMPT_TIMEOUT
prompt_seen = False
while time.time() < deadline:
    with buf_lock:
        snap = bytes(buf)
    if snap.find(b"# ") >= 0:
        prompt_seen = True
        note("auto-login detected (buildroot)")
        break
    time.sleep(0.5)
if not prompt_seen:
    with buf_lock:
        tail = bytes(buf)[-400:]
    fail(
        f"never reached buildroot `# ` prompt within {PROMPT_TIMEOUT}s; "
        f"last 400 bytes of console: {tail!r}"
    )

# Quiet kernel printk so it doesn't flood our parsing window.
send(b"dmesg -n 1\r")
with buf_lock:
    cursor = len(buf)
wait_for(b"# ", 5, from_idx=cursor)


# --- workload --------------------------------------------------------------
# fio: high-iodepth random write with O_DIRECT so every submit becomes
# a real virtio-blk descriptor (no page-cache batching diluting the
# burst). 8 jobs × 128 iodepth = up to 1024 in-flight, well into the
# range where the V1 2048-entry kick ring sees pressure.
#
# Size 32M fits comfortably inside the buildroot rootfs's ~50M free
# space. With time_based + runtime spanning the full sampling window,
# fio loops over the same file the whole time.
def start_fio() -> None:
    runtime = args.duration_sec + 5  # slack so fio doesn't end before we tear down
    # Default ioengine is sync, so each job has effective iodepth=1.
    # Total in-flight = numjobs. 16 jobs all writing to the same 32 MiB
    # file with direct=1 and randwrite generates ~16 concurrent virtio-blk
    # descriptors per dispatch pass — enough to saturate the V1 ring on
    # this hardware.
    cmd = (
        b"fio --name=burst --rw=randwrite --bs=4k --numjobs=16 "
        b"--filename=/root/fio.tmp --size=32M "
        b"--runtime=" + str(runtime).encode() + b" "
        b"--time_based --group_reporting "
        b"--direct=1 --output=/tmp/fio.log "
        b">/dev/null 2>&1 & echo $! > /tmp/fio.pid; "
        b"echo BURST_FIO_STARTED:$(cat /tmp/fio.pid)\r"
    )
    with buf_lock:
        cursor = len(buf)
    send(cmd)
    wait_for(b"BURST_FIO_STARTED:", 30, from_idx=cursor)


# Console burst: tight loop printing to /dev/console. The console TX queue
# (slot 2 q1 in the post-#175 layout) is exactly the queue that hung at
# `lag=1` after a throttle/release cycle in the live #184 reproducer.
# Writing here exercises that queue concurrently with fio's block traffic.
def start_console_burst() -> None:
    cmd = (
        b"(while true; do printf '.%05d' $((RANDOM)); done >/dev/console) "
        b"& echo $! > /tmp/console.pid; "
        b"echo BURST_CONSOLE_STARTED:$(cat /tmp/console.pid)\r"
    )
    with buf_lock:
        cursor = len(buf)
    send(cmd)
    wait_for(b"BURST_CONSOLE_STARTED:", 10, from_idx=cursor)


def stop_workload() -> None:
    # Echo fio log tail (post-truncation) before teardown so we can tell
    # "fio failed to start" from "fio ran but didn't generate enough
    # pressure" in the script output.
    cmd = (
        b"kill -9 $(cat /tmp/fio.pid 2>/dev/null) 2>/dev/null; "
        b"kill -9 $(cat /tmp/console.pid 2>/dev/null) 2>/dev/null; "
        b"echo '--- fio.log tail ---'; tail -5 /tmp/fio.log 2>/dev/null; "
        b"echo '--- end fio.log ---'; "
        b"rm -f /tmp/fio.pid /tmp/console.pid /root/fio.* /tmp/fio.log; "
        b"echo BURST_STOPPED\r"
    )
    with buf_lock:
        cursor = len(buf)
    send(cmd)
    try:
        wait_for(b"BURST_STOPPED", 15, from_idx=cursor)
    except SystemExit:
        # Console burst may still be flooding /dev/console — best effort.
        pass
    # Surface the fio.log tail in the script output for debugging.
    with buf_lock:
        snap = bytes(buf)
    start = snap.rfind(b"--- fio.log tail ---")
    end = snap.rfind(b"--- end fio.log ---")
    if start >= 0 and end > start:
        log_lines = snap[start:end].decode("ascii", errors="replace").splitlines()
        for line in log_lines[1:]:
            note(f"fio: {line}")


# --- metrics sampler -------------------------------------------------------
METRIC_KEYS = [
    "bhx_dispatch_passes_total",
    "bhx_dispatch_queues_drained",
    "bhx_notify_events_total",
]
METRIC_RE = re.compile(rb"^([a-z_]+)(?:\{[^}]*\})?\s+([0-9.eE+\-]+)\s*$", re.M)


def fetch_metrics() -> dict[str, float]:
    """Pull the daemon's /metrics endpoint and reduce to METRIC_KEYS.

    Per-label series collapse to a sum across labels (e.g. per-l2cpu
    counters become a single number). Today's V2 dispatch metrics
    are unlabeled, but the parser tolerates labels for forward
    compatibility.
    """
    out: dict[str, float] = {k: 0.0 for k in METRIC_KEYS}
    try:
        with urllib.request.urlopen(
            f"http://127.0.0.1:{args.metrics_port}/metrics", timeout=2
        ) as resp:
            body = resp.read()
    except (urllib.error.URLError, TimeoutError) as e:
        note(f"metrics fetch failed: {e}")
        return out
    for m in METRIC_RE.finditer(body):
        name = m.group(1).decode()
        if name in out:
            try:
                out[name] += float(m.group(2))
            except ValueError:
                pass
    return out


sampler_stop = threading.Event()
samples: list[dict[str, object]] = []


def sampler() -> None:
    t0 = time.monotonic()
    while not sampler_stop.is_set():
        sample = {"ts_iso": datetime.datetime.now().isoformat(timespec="seconds")}
        sample["elapsed_s"] = round(time.monotonic() - t0, 2)
        sample.update(fetch_metrics())
        samples.append(sample)
        sampler_stop.wait(1.0)


# --- workload + sampling --------------------------------------------------
note("starting in-guest workload (fio + console burst)")
start_fio()
start_console_burst()
note(f"workload running; sampling /metrics for {args.duration_sec} s")
sampler_thread = threading.Thread(target=sampler, daemon=True)
sampler_thread.start()

# Sleep with periodic note() so a long run isn't silent.
slept = 0
while slept < args.duration_sec:
    chunk = min(60, args.duration_sec - slept)
    time.sleep(chunk)
    slept += chunk
    if slept < args.duration_sec:
        cur = samples[-1] if samples else {}
        passes = cur.get("bhx_dispatch_passes_total", 0)
        queues = cur.get("bhx_dispatch_queues_drained", 0)
        notifies = cur.get("bhx_notify_events_total", 0)
        note(
            f"[{slept}/{args.duration_sec}s] dispatch_passes={passes} "
            f"queues_drained={queues} notifies={notifies}"
        )

note("workload window complete; stopping sampler + in-guest workload")
sampler_stop.set()
sampler_thread.join(timeout=5)
stop_workload()


# --- write CSV -------------------------------------------------------------
fieldnames = ["ts_iso", "elapsed_s", *METRIC_KEYS]
with open(args.csv, "w", newline="") as f:
    w = csv.DictWriter(f, fieldnames=fieldnames)
    w.writeheader()
    for s in samples:
        w.writerow({k: s.get(k, 0) for k in fieldnames})
note(f"wrote {len(samples)} samples to {args.csv}")


# --- summary --------------------------------------------------------------
def col_max(name: str) -> float:
    return max((float(s.get(name, 0)) for s in samples), default=0.0)


def col_last(name: str) -> float:
    return float(samples[-1].get(name, 0)) if samples else 0.0


total_dispatch_passes = col_last("bhx_dispatch_passes_total")
total_queues_drained = col_last("bhx_dispatch_queues_drained")
total_notifies = col_last("bhx_notify_events_total")

print()
print("=== summary ===")
print(f"  duration                       : {args.duration_sec}s")
print(f"  samples                        : {len(samples)}")
print(f"  bhx_dispatch_passes_total      : {total_dispatch_passes:.0f}")
print(f"  bhx_dispatch_queues_drained    : {total_queues_drained:.0f}")
print(f"  bhx_notify_events_total        : {total_notifies:.0f}")
print()


# --- daemon-log scan: V1-path activity should not appear -------------------
DAEMON_LOG_RE = re.compile(rb"kick.*drop|rescue|throttle.*ENGAGE", re.IGNORECASE)


def scan_daemon_log() -> list[bytes]:
    if not os.path.exists(LOG_FILE):
        return []
    hits = []
    with open(LOG_FILE, "rb") as f:
        for line in f:
            if DAEMON_LOG_RE.search(line):
                hits.append(line.rstrip())
    return hits


# --- assertions ------------------------------------------------------------
exit_code = 0
log_hits = scan_daemon_log()
issues = []
if log_hits:
    issues.append(
        f"daemon log has {len(log_hits)} matches for kick.*drop|rescue|throttle.*ENGAGE; "
        f"first: {log_hits[0]!r}"
    )
if total_dispatch_passes <= 0:
    issues.append(
        "bhx_dispatch_passes_total stayed at 0 — workload didn't reach the V2 dispatch path "
        "(or the metric isn't exported)."
    )
if issues:
    for i in issues:
        print(f"FAIL: {i}")
    exit_code = 3
else:
    print(
        f"PASS: clean — {total_dispatch_passes:.0f} dispatch passes, "
        f"{total_notifies:.0f} guest NOTIFYs, no drop/rescue/throttle log lines."
    )

# Always print the CSV path so a CI run captures it.
print(f"  csv: {args.csv}")

# Tear down before exiting so a failed assertion doesn't leave the daemon up.
try:
    proc.kill()
except Exception:
    pass
if not args.skip_boot:
    subprocess.run(
        [BINARY, "daemon", "stop", "-t", TTDEVICE], check=False, timeout=10
    )

sys.exit(exit_code)
