#!/usr/bin/env python3
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT

"""
Disk benchmark — drive `fio` inside the guest with three job profiles
and capture bandwidth + IOPS + p99 latency from fio's terse output.

Job profiles (from #28):
  - seq_write_4M_qd1   : --rw=write     --bs=4M --iodepth=1 (30s)
  - rand_write_4k_qd16 : --rw=randwrite --bs=4k --iodepth=16 (30s)
  - seq_read_4M_qd1    : --rw=read      --bs=4M --iodepth=1 (30s)

Each profile contributes 3 metrics (bandwidth_mbps, iops,
latency_p99_us). 9 BenchResult lines total emitted to the CSV
specified by --csv (default: scripts/bench/results/disk-<ts>.csv).

Pre-reqs: a buildroot rootfs with `fio` (the `tests/rootfs/`
buildroot config has it).

Driving the guest is via the same single-`connect` pattern as
`scripts/soak_fio_remove_disk.py`. The runtime is roughly
fio_seconds * 3 + boot ~30s.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

# Make the lib.py next door importable when invoked as
# `scripts/bench/disk.py` from the repo root.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib import (  # noqa: E402
    BINARY,
    BenchResult,
    GuestSession,
    boot,
    daemon_running,
    daemon_start,
    daemon_stop,
    fail,
    note,
    resolve_rootfs,
    setup_bench_disk,
    wait_for_running,
    write_csv,
)

# Use JSON output rather than the positional `terse` format —
# field offsets differ across fio versions and are a maintenance
# trap. The buildroot rootfs ships fio 3.41 (sufficient for JSON
# minus); if a future buildroot bump downgrades fio, we'll see a
# parse error rather than silently-wrong numbers.

JOBS = [
    {
        "name": "seq_write_4M_qd1",
        "rw": "write",
        "bs": "4M",
        "iodepth": 1,
        "duration": 30,
        "side": "write",
    },
    {
        "name": "rand_write_4k_qd16",
        "rw": "randwrite",
        "bs": "4k",
        "iodepth": 16,
        "duration": 30,
        "side": "write",
    },
    {
        "name": "seq_read_4M_qd1",
        "rw": "read",
        "bs": "4M",
        "iodepth": 1,
        "duration": 30,
        "side": "read",
    },
]


def parse_fio_json(text: str, side: str) -> tuple[float, float, float]:
    """Pull (bandwidth_mbps, iops, clat_p99_us) from fio's JSON
    output. `side` selects "read" or "write" depending on which half
    the job exercised.

    fio's JSON has `jobs[0].read.{bw,iops,clat_ns.percentile['99.000000']}`
    (and same under `.write`). bw is in KiB/s, latency in ns.
    """
    j = json.loads(text)
    block = j["jobs"][0][side]
    bw_kib_s = float(block["bw"])
    iops = float(block["iops"])
    # p99 from clat percentiles; key is a stringified float.
    clat_ns = block.get("clat_ns") or block.get("clat", {})
    pct = clat_ns.get("percentile", {})
    p99_ns = pct.get("99.000000")
    if p99_ns is None:
        # Older fio without clat_ns: fall back to clat_max (μs).
        p99_us = float(block.get("clat", {}).get("max", 0)) or float(
            clat_ns.get("max", 0)
        ) / 1000.0
    else:
        p99_us = float(p99_ns) / 1000.0
    return (bw_kib_s / 1024.0, iops, p99_us)


def run_one_job(g: GuestSession, job: dict) -> tuple[float, float, float]:
    """Run one fio job, parse JSON output, return (mbps, iops, lat_us)."""
    name = job["name"]
    note(f"running fio: {name} ({job['rw']} bs={job['bs']} qd={job['iodepth']})")

    # 64 MiB working set, time-bounded so wall time is predictable.
    # Wrap fio's stdout in markers so we can extract the JSON cleanly
    # — busybox's shell can interleave the cmd output with other
    # noise (kernel printk, ssh banners) that would break json.loads.
    start_marker = "_FIO_JSON_START_"
    end_marker = "_FIO_JSON_END_"
    cmd = (
        f"echo {start_marker} ; "
        f"fio --name={name} --filename=/root/{name}.tmp --size=64M "
        f"--rw={job['rw']} --bs={job['bs']} --iodepth={job['iodepth']} "
        f"--time_based --runtime={job['duration']} "
        f"--ioengine=psync --direct=0 "
        f"--output-format=json ; "
        f"echo {end_marker}"
    )
    out = g.run_cmd(cmd, timeout_s=job["duration"] + 30)

    s = out.find(start_marker)
    e = out.find(end_marker)
    if s < 0 or e < 0 or e < s:
        fail(f"fio job {name}: markers missing in output:\n{out}")
    json_text = out[s + len(start_marker) : e].strip()
    # fio also prints a header line at the top of JSON output;
    # json.loads is happy with leading whitespace but not with the
    # human-readable progress prefix some versions emit. Find the
    # first '{' to be safe.
    brace = json_text.find("{")
    if brace < 0:
        fail(f"fio job {name}: no JSON object in output:\n{json_text[:400]}")
    json_text = json_text[brace:]

    try:
        mbps, iops, lat_us = parse_fio_json(json_text, job["side"])
    except (KeyError, ValueError, json.JSONDecodeError) as e:
        fail(f"fio job {name}: JSON parse failed ({e}); first 400 chars:\n{json_text[:400]}")

    note(
        f"  {name}: {mbps:.1f} MB/s, {iops:.0f} IOPS, p99 clat = {lat_us:.0f} µs"
    )
    # Cleanup the test file so the next job has space.
    g.run_cmd(f"rm -f /root/{name}.tmp", timeout_s=5)
    return mbps, iops, lat_us


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--card", type=int, default=int(os.environ.get("CARD", "0")))
    ap.add_argument("--l2cpu", type=int, default=int(os.environ.get("L2CPU", "0")))
    ap.add_argument("--csv", type=Path, default=None)
    ap.add_argument(
        "--skip-boot",
        action="store_true",
        help="Assume daemon is already running with l2cpu booted",
    )
    args = ap.parse_args()

    if args.csv is None:
        ts = time.strftime("%Y%m%d-%H%M%S")
        args.csv = Path(__file__).resolve().parent / "results" / f"disk-{ts}.csv"

    rootfs = resolve_rootfs()
    # The buildroot rootfs is sized to fit; fio needs writable
    # headroom. Copy + grow once per bench run.
    bench_disk = setup_bench_disk(
        rootfs,
        Path(__file__).resolve().parent / "results" / "rootfs-bench.ext4",
        target_mib=1024,
    )

    if not args.skip_boot:
        # Ensure clean state. tt-smi reset handles a wedged chip but
        # we don't run it from python — leave that to the operator
        # if needed. Just stop+start+boot.
        if daemon_running(args.card):
            daemon_stop(args.card)
            time.sleep(1)
        note(f"daemon start (log: scripts/bench)")
        daemon_start(args.card)
        time.sleep(1)
        note(f"cold boot l2cpu {args.l2cpu} with bench disk={bench_disk}")
        boot(args.card, args.l2cpu, bench_disk, network=False)
        wait_for_running(args.card, args.l2cpu, timeout_s=60)
    else:
        note("--skip-boot: assuming daemon is already running")

    results: list[BenchResult] = []
    with GuestSession(args.card, args.l2cpu) as g:
        for job in JOBS:
            mbps, iops, lat_us = run_one_job(g, job)
            results.append(BenchResult("disk", f"{job['name']}.bandwidth_mbps", mbps, "MB/s"))
            results.append(BenchResult("disk", f"{job['name']}.iops", iops, "IOPS"))
            results.append(
                BenchResult("disk", f"{job['name']}.latency_p99_us", lat_us, "us")
            )

    write_csv(args.csv, results)
    note(f"wrote {len(results)} metrics to {args.csv}")

    if not args.skip_boot:
        note("daemon stop")
        daemon_stop(args.card)

    return 0


if __name__ == "__main__":
    sys.exit(main())
