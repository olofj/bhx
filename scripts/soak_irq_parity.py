#!/usr/bin/env python3
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT

"""
Detect missed PLIC interrupt edges in `InterruptController::set_interrupt`
(#195) by comparing daemon-side IRQ fire counts against guest-side IRQ
receive counts under sustained concurrent block load.

Boots a guest with rng + blk + net + console (max PLIC contention) and
drives a 16-job direct=1 random-write fio workload. Two parallel
samplers track the two ends of the PLIC pipe:

- **Host side**: scrapes `bhx_blk_interrupts_total{idx=L} +
  bhx_net_interrupts_total{idx=L} + bhx_console_interrupts_total{idx=L} +
  bhx_rng_interrupts_total{idx=L}` from the daemon's `/metrics`
  endpoint every `--sample-interval-sec`. This is the count of
  `set_interrupt` calls the daemon made — IRQs FIRED.
- **Guest side**: a tight in-guest loop prints
  `IRQ_SAMPLE epoch=<ts> virtio_total=<sum>` to /dev/console at the
  same cadence. `<sum>` is the column-sum across all `virtio*` rows
  in /proc/interrupts. This is the count of IRQs the kernel handler
  actually ran — IRQs RECEIVED.

If `set_interrupt`'s assert/de-assert window is too tight for the
X280 PLIC to latch (#195's hypothesis), the host-side counter grows
while the guest-side counter lags or stalls. Stalls are visible as
sustained zero-delta windows on the guest side under non-zero
host-side dispatch activity, and the cumulative gap monotonically
widens.

CSV columns:
    ts_iso, elapsed_s,
    host_irqs_total, guest_irqs_total, gap,
    host_delta, guest_delta, dispatch_passes_total

Assertion:
    max(gap over the last `--stall-window-sec` seconds) /
    max(host_delta over the same window) must stay below
    `--stall-tolerance` (default 0.05 = 5%).

A current-`main` run is expected to FAIL this assertion (no fix for
#195 yet); the script serves as both the empirical evidence for the
bug and the regression gate for whatever fix lands.

Env / args:
    --duration-sec N          DURATION_SEC env (default 300)
    --sample-interval-sec F   SAMPLE_INTERVAL_SEC env (default 1.0)
    --stall-window-sec N      STALL_WINDOW_SEC env (default 10)
    --stall-tolerance F       STALL_TOLERANCE env (default 0.05)
    --l2cpu N                 L2CPU env (default 0)
    --ttdevice N              CARD env (default 0)
    --metrics-port N          METRICS_PORT env (default 9090)
    --csv PATH                CSV env (default ./soak_irq_parity-<ts>.csv)
    --skip-boot               reuse a daemon+guest already running
    BINARY                    bhx binary (default ./target/debug/bhx)
    LOG_FILE                  daemon log path (default ./daemon-card0.log)
    ROOTFS                    rootfs path (auto-detected from buildroot)
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


# --- args & env -----------------------------------------------------------
parser = argparse.ArgumentParser(
    description=__doc__.split("\n")[0],
    formatter_class=argparse.RawDescriptionHelpFormatter,
)
parser.add_argument(
    "--duration-sec", type=int, default=int(os.environ.get("DURATION_SEC", "300"))
)
parser.add_argument(
    "--sample-interval-sec",
    type=float,
    default=float(os.environ.get("SAMPLE_INTERVAL_SEC", "1.0")),
)
parser.add_argument(
    "--stall-window-sec",
    type=int,
    default=int(os.environ.get("STALL_WINDOW_SEC", "10")),
)
parser.add_argument(
    "--stall-tolerance",
    type=float,
    default=float(os.environ.get("STALL_TOLERANCE", "0.05")),
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
    if os.path.exists("buildroot-stripped.ext4"):
        ROOTFS = "buildroot-stripped.ext4"
    elif os.path.exists("third_party/buildroot/rootfs.ext4"):
        ROOTFS = "third_party/buildroot/rootfs.ext4"
TTDEVICE = str(args.ttdevice)
L2CPU = str(args.l2cpu)
L2CPU_INT = args.l2cpu
TAG = f"[irq-parity l2cpu={L2CPU}]"

# Strip the cycle-test init script if running against the stock
# buildroot rootfs (which auto-poweroffs).
BURST_ROOTFS = os.environ.get("BURST_ROOTFS", "/tmp/burst-rootfs.ext4")

if not args.csv:
    args.csv = os.path.abspath(
        f"./soak_irq_parity-{datetime.datetime.now():%Y%m%d-%H%M%S}.csv"
    )


def fail(msg: str) -> None:
    print(f"FAIL: {msg}", flush=True, file=sys.stderr)
    sys.exit(1)


def note(msg: str) -> None:
    print(f"{TAG} {msg}", flush=True)


# --- sanity --------------------------------------------------------------
if not os.access(BINARY, os.X_OK):
    fail(f"{BINARY} not executable (run cargo build first)")
if not args.skip_boot:
    if not ROOTFS or not os.path.exists(ROOTFS):
        fail("no rootfs available; build third_party/buildroot or set ROOTFS=<path>")
    for f in ("fw_jump.bin", "Image", "blackhole-card.dtb"):
        if not os.path.exists(f):
            fail(f"{f} missing in cwd")


# --- private rootfs without the cycle-test init script -------------------
def prep_burst_rootfs() -> str:
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
    subprocess.run(
        ["/sbin/e2fsck", "-fy", BURST_ROOTFS],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    subprocess.run(
        [
            "/sbin/debugfs",
            "-w",
            "-R",
            "rm /etc/init.d/S99-virtio-cycle-test",
            BURST_ROOTFS,
        ],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return BURST_ROOTFS


# --- boot -----------------------------------------------------------------
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
    note(
        f"daemon start (metrics on :{args.metrics_port}, "
        f"sandbox={'off' if sandbox_off else 'on'})"
    )
    subprocess.run(daemon_args, check=True)
    time.sleep(0.3)
    boot_rootfs = prep_burst_rootfs()
    note(f"cold boot L2CPU {L2CPU} with rng+blk+net+console (rootfs={boot_rootfs})")
    # rng + blk + net + console all on; rng is on-by-default in `boot`, so
    # just passing -n covers blk(via --disk) + net + console + rng.
    subprocess.run(
        [BINARY, "boot", "-t", TTDEVICE, "-l", L2CPU, "-d", boot_rootfs, "-n"],
        check=True,
        timeout=90,
    )


if not args.skip_boot:
    boot_guest()


# --- console session ------------------------------------------------------
note("opening connect for guest-side workload + sampler")
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

# Quiet kernel printk so it doesn't drown our sampler markers.
send(b"dmesg -n 1\r")
with buf_lock:
    cursor = len(buf)
wait_for(b"# ", 5, from_idx=cursor)


# --- start guest-side IRQ sampler ----------------------------------------
# Background loop in guest: every sample_interval, sums per-CPU per-IRQ
# columns from /proc/interrupts rows containing "virtio" and prints
# IRQ_SAMPLE epoch=<unix_s> virtio_total=<n> to /dev/console.
#
# The awk strips the leading IRQ-number column and the trailing
# controller/edge/description columns, sums what's left, then we sum
# across virtio* rows. /proc/interrupts always exposes per-CPU
# counters even when CONFIG_SMP doesn't matter for the rows we care
# about.
def start_irq_sampler(interval_s: float) -> None:
    interval_ms_str = f"{interval_s:.3f}"
    # Use a while-true bash loop with sleep granular to interval_s
    # (busybox sleep accepts fractional seconds when compiled with
    # FANCY_SLEEP — buildroot's default does, so this works).
    cmd = (
        b"(while true; do "
        b"total=$(awk '/virtio/ { sum=0; for(i=2;i<=NF;i++) "
        b"if($i ~ /^[0-9]+$/) sum+=$i; total+=sum } END {print total+0}' "
        b"/proc/interrupts); "
        b"echo IRQ_SAMPLE epoch=$(date +%s) virtio_total=${total:-0}; "
        b"sleep " + interval_ms_str.encode() + b"; "
        b"done > /dev/console) & echo $! > /tmp/sampler.pid; "
        b"echo SAMPLER_STARTED:$(cat /tmp/sampler.pid)\r"
    )
    with buf_lock:
        cursor = len(buf)
    send(cmd)
    wait_for(b"SAMPLER_STARTED:", 10, from_idx=cursor)


# --- start fio workload --------------------------------------------------
def start_fio() -> None:
    runtime = args.duration_sec + 5
    cmd = (
        b"fio --name=parity --rw=randwrite --bs=4k --numjobs=16 "
        b"--filename=/root/fio.tmp --size=32M "
        b"--runtime=" + str(runtime).encode() + b" "
        b"--time_based --group_reporting "
        b"--direct=1 --output=/tmp/fio.log "
        b">/dev/null 2>&1 & echo $! > /tmp/fio.pid; "
        b"echo FIO_STARTED:$(cat /tmp/fio.pid)\r"
    )
    with buf_lock:
        cursor = len(buf)
    send(cmd)
    wait_for(b"FIO_STARTED:", 30, from_idx=cursor)


def stop_workload() -> None:
    cmd = (
        b"kill -9 $(cat /tmp/fio.pid 2>/dev/null) 2>/dev/null; "
        b"kill -9 $(cat /tmp/sampler.pid 2>/dev/null) 2>/dev/null; "
        b"sleep 0.5; "
        b"rm -f /tmp/fio.pid /tmp/sampler.pid /root/fio.* /tmp/fio.log; "
        b"echo PARITY_STOPPED\r"
    )
    with buf_lock:
        cursor = len(buf)
    send(cmd)
    try:
        wait_for(b"PARITY_STOPPED", 15, from_idx=cursor)
    except SystemExit:
        pass


# --- daemon-side metrics sampler -----------------------------------------
# Per-L2CPU labeled counters: sum across all 4 device kinds for our L2CPU.
HOST_IRQ_METRIC_RE = re.compile(
    rb"^bhx_(blk|net|console|rng)_interrupts_total\{([^}]*)\}\s+([0-9.]+)\s*$",
    re.M,
)
DISPATCH_RE = re.compile(rb"^bhx_dispatch_passes_total\s+([0-9.]+)\s*$", re.M)
LABEL_IDX_RE = re.compile(rb'idx="(\d+)"')


def fetch_host_metrics() -> tuple[int, int]:
    """Return (host_irqs_total_for_our_l2cpu, dispatch_passes_total)."""
    try:
        with urllib.request.urlopen(
            f"http://127.0.0.1:{args.metrics_port}/metrics", timeout=2
        ) as resp:
            body = resp.read()
    except (urllib.error.URLError, TimeoutError) as e:
        note(f"metrics fetch failed: {e}")
        return (-1, -1)
    irqs = 0
    for m in HOST_IRQ_METRIC_RE.finditer(body):
        labels = m.group(2)
        idx_m = LABEL_IDX_RE.search(labels)
        if idx_m and int(idx_m.group(1)) == L2CPU_INT:
            try:
                irqs += int(float(m.group(3)))
            except ValueError:
                pass
    dispatch = 0
    dm = DISPATCH_RE.search(body)
    if dm:
        try:
            dispatch = int(float(dm.group(1)))
        except ValueError:
            pass
    return irqs, dispatch


# --- guest-side IRQ sample extraction ------------------------------------
GUEST_IRQ_RE = re.compile(rb"IRQ_SAMPLE epoch=(\d+) virtio_total=(\d+)")


def fetch_latest_guest_irqs(min_epoch: int) -> tuple[int, int] | None:
    """Scan the console buffer for the most recent IRQ_SAMPLE with
    epoch >= min_epoch. Returns (epoch, total) or None if no fresh
    sample has landed since the last call.
    """
    with buf_lock:
        snap = bytes(buf)
    best: tuple[int, int] | None = None
    for m in GUEST_IRQ_RE.finditer(snap):
        ep = int(m.group(1))
        if ep >= min_epoch and (best is None or ep > best[0]):
            best = (ep, int(m.group(2)))
    return best


# --- run -----------------------------------------------------------------
start_irq_sampler(args.sample_interval_sec)
start_fio()
note(
    f"workload running; sampling every {args.sample_interval_sec}s for "
    f"{args.duration_sec}s"
)

t_start = time.time()
csv_file = open(args.csv, "w", newline="")
writer = csv.writer(csv_file)
writer.writerow(
    [
        "ts_iso",
        "elapsed_s",
        "host_irqs_total",
        "guest_irqs_total",
        "gap",
        "host_delta",
        "guest_delta",
        "dispatch_passes_total",
    ]
)

samples: list[tuple[float, int, int, int]] = []  # (elapsed, host_irqs, guest_irqs, dispatch)
last_host = 0
last_guest = 0
last_guest_epoch = 0
deadline = t_start + args.duration_sec
next_tick = t_start + args.sample_interval_sec
while time.time() < deadline:
    while time.time() < next_tick:
        time.sleep(0.02)
    next_tick += args.sample_interval_sec
    host_irqs, dispatch = fetch_host_metrics()
    if host_irqs < 0:
        continue
    guest = fetch_latest_guest_irqs(last_guest_epoch)
    if guest is not None:
        last_guest_epoch, last_guest = guest
    host_delta = host_irqs - last_host
    guest_delta_unused = last_guest - (samples[-1][2] if samples else 0)
    elapsed = time.time() - t_start
    gap = max(0, host_irqs - last_guest)
    writer.writerow(
        [
            datetime.datetime.now().isoformat(timespec="seconds"),
            f"{elapsed:.2f}",
            host_irqs,
            last_guest,
            gap,
            host_delta,
            guest_delta_unused,
            dispatch,
        ]
    )
    csv_file.flush()
    samples.append((elapsed, host_irqs, last_guest, dispatch))
    last_host = host_irqs

    if int(elapsed) % 60 == 0 and host_delta > 0:
        note(
            f"[{int(elapsed):3d}/{args.duration_sec}s] "
            f"host_irqs={host_irqs} guest_irqs={last_guest} gap={gap} "
            f"(host_delta={host_delta} guest_delta={guest_delta_unused})"
        )

note("workload window complete; stopping fio + sampler")
stop_workload()
csv_file.close()


# --- analysis ------------------------------------------------------------
# Aggregate end-state: cumulative gap.
final_elapsed, final_host, final_guest, final_dispatch = samples[-1]
final_gap = max(0, final_host - final_guest)

# Stall detection: over any sliding window of stall_window_sec, what
# fraction of host_deltas went undelivered to the guest?
window_len = max(1, int(args.stall_window_sec / args.sample_interval_sec))
worst_stall_ratio = 0.0
worst_stall_at = 0.0
for i in range(window_len, len(samples)):
    host_in_window = samples[i][1] - samples[i - window_len][1]
    guest_in_window = samples[i][2] - samples[i - window_len][2]
    if host_in_window <= 0:
        continue
    missed = max(0, host_in_window - guest_in_window)
    ratio = missed / host_in_window
    if ratio > worst_stall_ratio:
        worst_stall_ratio = ratio
        worst_stall_at = samples[i][0]

print("")
print("=== summary ===")
print(f"  duration                       : {args.duration_sec}s")
print(f"  samples                        : {len(samples)}")
print(f"  dispatch_passes_total          : {final_dispatch}")
print(f"  host_irqs_total (sum 4 kinds)  : {final_host}")
print(f"  guest_irqs_total (virtio rows) : {final_guest}")
print(f"  cumulative gap                 : {final_gap}")
print(
    f"  worst stall ratio (over {args.stall_window_sec}s) : "
    f"{worst_stall_ratio:.3f} at t={worst_stall_at:.1f}s"
)
print(f"  csv: {args.csv}")

if final_host == 0:
    fail(
        "host_irqs_total = 0 across the entire run — workload didn't "
        "exercise the IRQ path; check fio output / network setup."
    )
if last_guest == 0:
    fail(
        "guest_irqs_total = 0 across the entire run — guest-side sampler "
        "produced no IRQ_SAMPLE markers, or /proc/interrupts has no "
        "virtio rows. Check the console output / awk command."
    )

if worst_stall_ratio > args.stall_tolerance:
    fail(
        f"missed-IRQ stall ratio {worst_stall_ratio:.3f} > tolerance "
        f"{args.stall_tolerance:.3f} (worst at t={worst_stall_at:.1f}s). "
        f"This is #195 — set_interrupt's de-assert window is too tight "
        f"for the X280 PLIC to latch under load."
    )

print(
    f"PASS: clean — worst stall ratio {worst_stall_ratio:.3f} <= "
    f"{args.stall_tolerance:.3f}, cumulative gap {final_gap} on "
    f"{final_host} fired IRQs."
)
