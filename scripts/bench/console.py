#!/usr/bin/env python3
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT

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


def measure_roundtrip_latency_us(
    g: GuestSession, iterations: int
) -> tuple[float, float, float]:
    """Echo-loop latency. Guest runs the unbuffered `echo-byte` helper
    (#36 — see third_party/buildroot/echo-byte.c); host times each round trip.

    Pattern: send a READY marker before invoking echo-byte so we know
    when the guest is actually sitting in read(2). Then per-byte
    timing. After `iterations` bytes we close the helper's stdin (Ctrl-D
    via a shell exit), and a DONE marker confirms cleanup.

    Returns (p50_us, p99_us, mean_us).
    """
    note(f"starting guest-side echo-byte loop ({iterations} iterations)")
    ready_marker = "_BENCH_LAT_READY_"
    done_marker = "_BENCH_LAT_DONE_"
    # echo-byte takes an optional byte count; pass `iterations` so it
    # exits cleanly without the bench needing to forge an EOF on the
    # guest tty. We flip -icanon so read(0,&c,1) returns per byte instead
    # of waiting for a newline (the GuestSession already runs with -echo
    # off so we don't touch echo here).
    cmd = (
        f"if [ ! -x /usr/local/bin/echo-byte ]; then "
        f"  printf 'BENCH_LAT_NO_HELPER\\n'; exit 0; "
        f"fi; "
        f"stty -icanon; "
        f"printf '{ready_marker}\\n'; "
        f"/usr/local/bin/echo-byte {iterations}; "
        f"stty icanon; "
        f"printf '\\n{done_marker}\\n'"
    )
    before = g.buffer_len()
    g.send(f"{cmd}\n".encode())

    # If the rootfs doesn't have echo-byte (operator hasn't rebuilt
    # post-#36), the cmd printf's BENCH_LAT_NO_HELPER and exits.
    # Detect that path before timing anything so the bench surfaces
    # a clear SKIP rather than a wait_for timeout.
    deadline = time.time() + 15
    while time.time() < deadline:
        with g._lock:  # type: ignore[attr-defined]
            snap = bytes(g._buf)  # type: ignore[attr-defined]
        if b"BENCH_LAT_NO_HELPER" in snap[before:]:
            raise RuntimeError(
                "rootfs missing /usr/local/bin/echo-byte — rebuild third_party/buildroot (#36)"
            )
        if ready_marker.encode() in snap[before:]:
            break
        time.sleep(0.05)
    else:
        fail("timeout waiting for echo-byte READY marker")

    # Skip past the READY marker + its newline so the per-byte loop
    # below searches forward from the right position.
    ready_at = g.wait_for(ready_marker.encode(), timeout_s=2, from_idx=before)
    next_idx = g.wait_for(b"\n", timeout_s=2, from_idx=ready_at + len(ready_marker)) + 1

    samples_us: list[float] = []
    payload = bytes((ord("a") + (i % 26)) for i in range(iterations))
    for i in range(iterations):
        b = bytes([payload[i]])
        t0 = time.time()
        g.send(b)
        idx = g.wait_for(b, timeout_s=2.0, from_idx=next_idx)
        elapsed_us = (time.time() - t0) * 1e6
        samples_us.append(elapsed_us)
        next_idx = idx + 1

    # echo-byte exits after `iterations` bytes (count arg above);
    # the shell prints DONE next.
    g.wait_for(done_marker.encode(), timeout_s=10, from_idx=next_idx)

    samples_us.sort()
    n = len(samples_us)
    p50 = samples_us[n // 2]
    p99 = samples_us[min(n - 1, int(n * 0.99))]
    mean = sum(samples_us) / n
    return p50, p99, mean


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
        help="Roundtrip-latency iteration count (default 200)",
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

        # Roundtrip-latency uses the unbuffered `echo-byte` helper
        # (#36) baked into third_party/buildroot by the post-build script.
        # If the rootfs is older than #36 it doesn't have the helper
        # — we surface that as SKIP rather than failing the whole
        # bench so existing CI / baseline runs still complete.
        try:
            p50, p99, mean = measure_roundtrip_latency_us(g, args.latency_iters)
            note(
                f"roundtrip latency: p50={p50:.0f} µs, p99={p99:.0f} µs, mean={mean:.0f} µs "
                f"({args.latency_iters} samples)"
            )
            results.append(BenchResult("console", "roundtrip_latency_p50_us", p50, "us"))
            results.append(BenchResult("console", "roundtrip_latency_p99_us", p99, "us"))
            results.append(BenchResult("console", "roundtrip_latency_mean_us", mean, "us"))
        except RuntimeError as e:
            note(f"roundtrip latency: SKIP ({e})")
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
