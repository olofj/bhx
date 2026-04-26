#!/usr/bin/env python3
"""
Console benchmark — measure the chip-side virtual UART throughput
and roundtrip latency.

Two metrics (from #28):
  - bytes_per_sec_g2h : guest-to-host throughput. Send N bytes from
                        guest, time how long for the whole stream
                        to land on the host.
  - roundtrip_latency_p99_ms : host sends one byte → guest echoes →
                        host receives. 1000 iterations, report p99.
                        Catches regressions in dlog/scrollback/hub
                        fan-out that throughput alone would miss.

The daemon's `chip_console::uart_pass` is the rate-limiting step;
this benchmark is a direct probe of its FAST/SLOW/IDLE adaptive-
sleep behavior (#27).
"""

from __future__ import annotations

import argparse
import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from lib import (  # noqa: E402
    BenchResult,
    GuestSession,
    boot,
    daemon_running,
    daemon_start,
    daemon_stop,
    fail,
    note,
    resolve_rootfs,
    wait_for_running,
    write_csv,
)


# ---- bytes_per_sec_g2h: send N bytes from guest, time arrival --------------


def measure_g2h_throughput(g: GuestSession, n_bytes: int) -> float:
    """Send `n_bytes` from the guest via base64-of-zeroes, time
    arrival on the host. Returns bytes/sec.

    Wraps the workload between two unique markers — START fires
    after shell parsing is done, END fires after the last payload
    byte has been written to stdout. Timing is END - START, which
    excludes cmd parsing and includes only the actual stream
    transit through the chip-side UART. Cmd echo is off (stty -echo
    in `__enter__`) so the markers appear exactly once each.
    """
    start_marker = "_BENCH_G2H_START_"
    end_marker = "_BENCH_G2H_END_"
    # base64 fills its output with letters/digits — no \n, no
    # control bytes that could confuse the host-side terminal.
    cmd = (
        f"printf '{start_marker}\\n' ; "
        f"dd if=/dev/zero bs=1024 count={n_bytes // 1024} 2>/dev/null "
        f"| base64 -w0 | head -c {n_bytes} ; "
        f"printf '\\n{end_marker}\\n'"
    )
    before = g.buffer_len()
    g.send(f"{cmd}\n".encode())
    # Start marker fires once shell parsing is done.
    s_idx = g.wait_for(start_marker.encode(), timeout_s=15, from_idx=before)
    t0 = time.time()
    g.wait_for(end_marker.encode(), timeout_s=120, from_idx=s_idx + len(start_marker))
    elapsed = time.time() - t0
    if elapsed <= 0:
        fail(f"g2h elapsed <= 0 ({elapsed})")
    return n_bytes / elapsed


# ---- roundtrip_latency_p99: host sends 1B → guest echoes → host reads -----


# Roundtrip-latency was prototyped here as a `head -c1 | printf '%s' "$c"`
# loop but busybox's stdio buffers per-byte writes against /dev/hvc0.
# The bench's main() emits SKIP rows and leaves the implementation
# for a follow-up — likely a tiny C helper in the rootfs that uses
# unbuffered write(2) for byte-by-byte echo.


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--card", type=int, default=int(os.environ.get("CARD", "0")))
    ap.add_argument("--l2cpu", type=int, default=int(os.environ.get("L2CPU", "0")))
    ap.add_argument("--csv", type=Path, default=None)
    ap.add_argument("--skip-boot", action="store_true")
    ap.add_argument(
        "--g2h-bytes",
        type=int,
        default=64 * 1024,
        help="Total bytes to stream guest-to-host (default 64 KiB)",
    )
    ap.add_argument(
        "--latency-iters",
        type=int,
        default=200,
        help="Roundtrip-latency iteration count (currently unused; latency test is SKIP — see source comment)",
    )
    args = ap.parse_args()

    if args.csv is None:
        ts = time.strftime("%Y%m%d-%H%M%S")
        args.csv = Path(__file__).resolve().parent / "results" / f"console-{ts}.csv"

    rootfs = resolve_rootfs()

    if not args.skip_boot:
        if daemon_running(args.card):
            daemon_stop(args.card)
            time.sleep(1)
        daemon_start(args.card)
        time.sleep(1)
        note(f"cold boot l2cpu {args.l2cpu} with rootfs={rootfs}")
        boot(args.card, args.l2cpu, rootfs, network=False)
        wait_for_running(args.card, args.l2cpu, timeout_s=60)

    results: list[BenchResult] = []
    with GuestSession(args.card, args.l2cpu) as g:
        bps = measure_g2h_throughput(g, args.g2h_bytes)
        note(f"g2h throughput: {bps / 1024:.1f} KiB/s ({args.g2h_bytes} bytes)")
        results.append(BenchResult("console", "bytes_per_sec_g2h", bps, "B/s"))

        # Roundtrip-latency: skipped at this scope. busybox's
        # `head -c1 | printf '%s' "$c"` doesn't flush byte-by-byte
        # — printf is line-buffered against /dev/hvc0 — so the
        # host-side wait_for never sees the echoed byte until the
        # guest writes a newline. Fixing this needs either an
        # unbuffered helper in the rootfs (e.g. a tiny C program that
        # uses unistd write with no FILE*) or a different harness
        # design (e.g. measure latency via an inotify-style channel,
        # not the chip console). Emit SKIP so the CSV shape is stable
        # for baseline diffs.
        for metric in (
            "roundtrip_latency_p50_us",
            "roundtrip_latency_p99_us",
            "roundtrip_latency_mean_us",
        ):
            results.append(BenchResult("console", metric, 0.0, "SKIP"))

    write_csv(args.csv, results)
    note(f"wrote {len(results)} metrics to {args.csv}")

    if not args.skip_boot:
        daemon_stop(args.card)

    return 0


if __name__ == "__main__":
    sys.exit(main())
