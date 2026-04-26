#!/usr/bin/env python3
"""
Network benchmark — drive `iperf3` between host and guest, capture
TCP throughput in both directions.

Two metrics (from #28; UDP egress is optional, skipped today):
  - tcp_egress_30s  : guest → host TCP, single stream, 30 s. MB/s.
  - tcp_ingress_30s : host → guest TCP via the SSH-forward port
                      (slirp's hostfwd hands :2222 → guest:22 by
                      default; we use a separate forward to a
                      iperf3-listening guest port).

If `iperf3` isn't installed on the host, the bench skips that
surface with a SKIP line in the CSV rather than failing.

The guest also needs `iperf3` available (the buildroot rootfs from
#16 includes it).
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

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
    wait_for_running,
    write_csv,
)


def have_iperf3_host() -> bool:
    return shutil.which("iperf3") is not None


def parse_iperf3_summary_mbps(text: str) -> float | None:
    """iperf3's plain-text summary line:
        [SUM] 0.00-30.00 sec  X.XX GBytes  Y.YY Gbits/sec ...
    Pull the receiver-side average (the line that ends with `receiver`)
    or the sender-side if no receiver line is present (the egress
    case where iperf3 only prints sender). Returns MB/s.
    """
    receiver_re = re.compile(
        r"\s*\[\s*\d+\]\s+[\d.]+-[\d.]+\s+sec\s+\S+\s+\S+\s+([\d.]+)\s+(\w+)/sec.*receiver"
    )
    sender_re = re.compile(
        r"\s*\[\s*\d+\]\s+[\d.]+-[\d.]+\s+sec\s+\S+\s+\S+\s+([\d.]+)\s+(\w+)/sec.*sender"
    )
    for re_ in (receiver_re, sender_re):
        for line in text.splitlines():
            m = re_.match(line)
            if m:
                value = float(m.group(1))
                unit = m.group(2)
                # Convert to MB/s.
                if unit == "bits":
                    return value / 8.0 / 1e6
                if unit == "Kbits":
                    return value / 8.0 / 1e3
                if unit == "Mbits":
                    return value / 8.0
                if unit == "Gbits":
                    return value * 1000.0 / 8.0
    return None


def measure_tcp_egress(g: GuestSession, host_port: int, duration: int) -> float:
    """Guest connects to host:host_port. iperf3 reports throughput."""
    note(f"tcp_egress: starting host iperf3 -s on :{host_port}")
    server = subprocess.Popen(
        ["iperf3", "-s", "-1", "-p", str(host_port), "-B", "127.0.0.1"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        time.sleep(0.5)
        # 10.0.2.2 = slirp's host-side gateway address.
        cmd = (
            f"iperf3 -c 10.0.2.2 -p {host_port} -t {duration} -i 0 -f m"
        )
        out = g.run_cmd(cmd, timeout_s=duration + 30)
        mbps = parse_iperf3_summary_mbps(out)
        if mbps is None:
            fail(f"tcp_egress: couldn't parse iperf3 summary:\n{out}")
        note(f"tcp_egress: {mbps:.1f} MB/s")
        return mbps
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()


def measure_tcp_ingress(
    g: GuestSession, card: int, l2cpu: int, port: int, duration: int
) -> float:
    """Host connects to 127.0.0.1:port → slirp NAT → guest's
    iperf3 -s on `port`. Uses `add-net --fwd HOST:GUEST` (#37) for
    the slirp forward; restarts net to install the new fwd cleanly.
    """
    note(f"tcp_ingress: re-attaching net with --fwd {port}:{port}")
    subprocess.run(
        [BINARY, "remove-net", "-t", str(card), "-l", str(l2cpu)],
        capture_output=True,
    )
    r = subprocess.run(
        [
            BINARY,
            "add-net",
            "-t",
            str(card),
            "-l",
            str(l2cpu),
            "--fwd",
            f"{port}:{port}",
        ],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        fail(f"add-net --fwd failed: {r.stderr}")

    # Start guest-side iperf3 -s in the background; -1 makes it serve
    # exactly one connection then exit cleanly.
    g.send(f"iperf3 -s -1 -p {port} >/dev/null 2>&1 &\n".encode())
    time.sleep(2)

    note(f"tcp_ingress: running iperf3 -c 127.0.0.1:{port} for {duration}s")
    proc = subprocess.run(
        [
            "iperf3",
            "-c",
            "127.0.0.1",
            "-p",
            str(port),
            "-t",
            str(duration),
            "-i",
            "0",
            "-f",
            "m",
        ],
        capture_output=True,
        text=True,
        timeout=duration + 30,
    )
    out = proc.stdout
    mbps = parse_iperf3_summary_mbps(out)

    # Reap the guest-side server so it doesn't linger past this
    # benchmark run.
    g.run_cmd("kill %1 2>/dev/null || true", timeout_s=5)

    if mbps is None:
        fail(
            f"tcp_ingress: couldn't parse iperf3 summary:\n"
            f"stdout:\n{out}\nstderr:\n{proc.stderr}"
        )
    note(f"tcp_ingress: {mbps:.1f} MB/s")
    return mbps


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--card", type=int, default=int(os.environ.get("CARD", "0")))
    ap.add_argument("--l2cpu", type=int, default=int(os.environ.get("L2CPU", "0")))
    ap.add_argument("--csv", type=Path, default=None)
    ap.add_argument("--skip-boot", action="store_true")
    ap.add_argument(
        "--duration",
        type=int,
        default=30,
        help="Per-direction iperf3 duration in seconds (default 30)",
    )
    ap.add_argument(
        "--host-port",
        type=int,
        default=5201,
        help="Host-side iperf3 listen port for egress (default 5201)",
    )
    ap.add_argument(
        "--ingress-port",
        type=int,
        default=5202,
        help="Host+guest port for the ingress test (must be free; default 5202).",
    )
    args = ap.parse_args()

    if args.csv is None:
        ts = time.strftime("%Y%m%d-%H%M%S")
        args.csv = Path(__file__).resolve().parent / "results" / f"net-{ts}.csv"

    if not have_iperf3_host():
        note("iperf3 not on host PATH — emitting SKIP rows")
        results = [
            BenchResult("net", "tcp_egress_30s.bandwidth_mbps", 0.0, "SKIP"),
            BenchResult("net", "tcp_ingress_30s.bandwidth_mbps", 0.0, "SKIP"),
        ]
        write_csv(args.csv, results)
        note(f"wrote {len(results)} SKIP rows to {args.csv}")
        return 0

    rootfs = resolve_rootfs()

    if not args.skip_boot:
        if daemon_running(args.card):
            daemon_stop(args.card)
            time.sleep(1)
        daemon_start(args.card)
        time.sleep(1)
        note(f"cold boot l2cpu {args.l2cpu} with rootfs={rootfs}")
        boot(args.card, args.l2cpu, rootfs, network=True)
        wait_for_running(args.card, args.l2cpu, timeout_s=60)
        # Wait for guest userspace + DHCP to come up before slirp
        # routing makes sense.
        time.sleep(20)

    results: list[BenchResult] = []
    with GuestSession(args.card, args.l2cpu) as g:
        mbps = measure_tcp_egress(g, args.host_port, args.duration)
        results.append(
            BenchResult("net", "tcp_egress_30s.bandwidth_mbps", mbps, "MB/s")
        )

        mbps = measure_tcp_ingress(
            g, args.card, args.l2cpu, args.ingress_port, args.duration
        )
        results.append(
            BenchResult("net", "tcp_ingress_30s.bandwidth_mbps", mbps, "MB/s")
        )

    write_csv(args.csv, results)
    note(f"wrote {len(results)} metrics to {args.csv}")

    if not args.skip_boot:
        daemon_stop(args.card)

    return 0


if __name__ == "__main__":
    sys.exit(main())
