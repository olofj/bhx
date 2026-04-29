"""
Shared helpers for the bench/* scripts.

Pattern lifted from scripts/soak_fio_remove_disk.py: open a single
`connect` session at the top of a benchmark, hold it across all
iterations to skip the slirp+ssh dance per-iter, and drive
guest-side commands through it.

A `GuestSession` wraps the `connect` subprocess + a background
reader thread + a buffer + a quiet helper for "send a command, wait
for the prompt back, return everything in between". CSV emission
is in here too because every benchmark uses the same shape.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


# ---- Process / env helpers -------------------------------------------------

# Resolve defaults against the project root (the dir holding
# Cargo.toml), not the cwd — `run_all.sh` cd's to `scripts/bench/`,
# and the operator may run any individual script from anywhere.
PROJECT_ROOT = Path(__file__).resolve().parents[2]
BINARY = os.environ.get(
    "BINARY", str(PROJECT_ROOT / "target/debug/bhx")
)
LOG_FILE = os.environ.get(
    "LOG_FILE", str(PROJECT_ROOT / "daemon-card0.log")
)
ROOTFS_DEFAULT_PATHS = [
    PROJECT_ROOT / "tests/rootfs/rootfs.ext4",
    PROJECT_ROOT / "rootfs.ext4",
]


def resolve_rootfs() -> Path:
    """Same priority as soak scripts: $ROOTFS env > buildroot > legacy.
    Default paths anchor at the project root so the bench works
    regardless of cwd."""
    if env := os.environ.get("ROOTFS"):
        return Path(env)
    for cand in ROOTFS_DEFAULT_PATHS:
        if cand.exists():
            return cand
    raise FileNotFoundError(
        "no rootfs available; build tests/rootfs or set ROOTFS=<path>"
    )


def note(msg: str) -> None:
    print(f"[bench] {msg}", flush=True)


def fail(msg: str) -> "None":  # type: ignore[override]
    print(f"FAIL: {msg}", file=sys.stderr, flush=True)
    sys.exit(1)


# ---- Daemon lifecycle ------------------------------------------------------


def daemon_running(card: int) -> bool:
    r = subprocess.run(
        [BINARY, "daemon", "status", "-t", str(card)],
        capture_output=True,
        text=True,
    )
    return "running" in r.stdout


def daemon_stop(card: int) -> None:
    subprocess.run(
        [BINARY, "daemon", "stop", "-t", str(card)],
        capture_output=True,
    )


def daemon_start(card: int, log_file: str = LOG_FILE) -> None:
    subprocess.run(
        [BINARY, "daemon", "start", "-t", str(card), "--log-file", log_file],
        check=True,
    )


def _find_tool(name: str) -> str:
    """Locate a command, falling back to the standard /sbin paths
    when it's not on $PATH (e2fsprogs ships in /sbin which non-root
    shells often don't have)."""
    if found := shutil.which(name):
        return found
    for p in ("/sbin", "/usr/sbin"):
        candidate = Path(p) / name
        if candidate.is_file():
            return str(candidate)
    raise FileNotFoundError(
        f"{name!r} not on PATH or in /sbin or /usr/sbin — "
        f"install with: apt install e2fsprogs"
    )


def setup_bench_disk(src: Path, dest: Path, target_mib: int = 1024) -> Path:
    """Produce a working disk image at `dest` with at least
    `target_mib` MiB of free space.

    The buildroot rootfs from #16 is sized to fit its contents — zero
    free bytes after boot. fio needs writable headroom to do anything
    useful. Solution: copy the rootfs once to a per-bench file,
    extend with truncate, run resize2fs to make ext4 see the new
    capacity. Idempotent — re-runs see the file already at target
    size and skip the work.

    Requires `e2fsck` + `resize2fs` on the host (e2fsprogs).
    """
    target_bytes = target_mib * 1024 * 1024
    if dest.exists() and dest.stat().st_size >= target_bytes:
        note(f"bench disk already provisioned: {dest} ({target_mib} MiB)")
        return dest
    note(f"provisioning bench disk: {src} -> {dest} ({target_mib} MiB)")
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy(src, dest)
    # Extend the file with sparse zeros (truncate is sparse on ext4).
    with dest.open("r+b") as f:
        f.truncate(target_bytes)
    # ext4 needs to be told the backing file got bigger.
    subprocess.run([_find_tool("e2fsck"), "-fy", str(dest)], check=False)
    subprocess.run([_find_tool("resize2fs"), str(dest)], check=True)
    return dest


def boot(
    card: int,
    l2cpu: int,
    rootfs: Path,
    network: bool = True,
    fwd: list[tuple[int, int]] | None = None,
) -> None:
    """Cold-boot l2cpu N with the given rootfs.

    Passes opensbi/kernel/dtb explicitly because `boot`'s defaults
    (`fw_jump.bin` / `Image` / `blackhole-card.dtb`) are cwd-relative,
    and `run_all.sh` cd's into `scripts/bench/` where those symlinks
    don't exist. The artifacts live at the project root (symlinks
    pointing at `../bhx/`).

    `fwd` is `[(host_port, guest_port), ...]` extra TCP forwards on
    top of the implicit SSH forward. Wired in at cold-boot rather
    than via post-boot `add-net --fwd` because the buildroot kernel
    has virtio_net built in and can't rebind to a hot-replaced
    device — the ingress benchmark needs the forward present from
    the start.
    """
    args = [
        BINARY,
        "boot",
        "-t",
        str(card),
        "-l",
        str(l2cpu),
        "-d",
        str(rootfs),
        "--opensbi",
        str(PROJECT_ROOT / "fw_jump.bin"),
        "--kernel",
        str(PROJECT_ROOT / "Image"),
        "--dtb",
        str(PROJECT_ROOT / "blackhole-card.dtb"),
        # --force so a back-to-back bench (disk.py stops the daemon
        # then console.py starts a fresh one + re-boots) doesn't
        # error out on "already booted" from the warm-resume probe
        # picking up the previous run's L2CPU.
        "--force",
    ]
    if network:
        args.append("-n")
    if fwd:
        for host, guest in fwd:
            args.extend(["--fwd", f"{host}:{guest}"])
    subprocess.run(args, check=True, timeout=120)


def wait_for_running(card: int, l2cpu: int, timeout_s: float = 60) -> None:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        r = subprocess.run(
            [BINARY, "daemon", "status", "-t", str(card)],
            capture_output=True,
            text=True,
        )
        if r.returncode == 0 and f"l2cpu {l2cpu}: Running" in r.stdout:
            return
        time.sleep(1)
    fail(f"l2cpu {l2cpu} never reached Running within {timeout_s}s")


# ---- Console session -------------------------------------------------------


class GuestSession:
    """One `connect` process + reader thread + buffer.

    Use as a context manager so the connect is killed on early exit.
    The send/expect cadence assumes the buildroot rootfs (auto-login,
    `# ` prompt). Debian `login:` flow lives in soak_fio_remove_disk.py
    if anyone needs to port it back.
    """

    def __init__(self, card: int, l2cpu: int) -> None:
        self.card = card
        self.l2cpu = l2cpu
        self.proc: subprocess.Popen | None = None
        self._buf = bytearray()
        self._lock = threading.Lock()
        self.prompt = b"~ # "  # buildroot busybox

    def __enter__(self) -> "GuestSession":
        self.proc = subprocess.Popen(
            [BINARY, "connect", "-t", str(self.card), "-l", str(self.l2cpu)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            bufsize=0,
        )
        threading.Thread(target=self._reader, daemon=True).start()
        # Wait for the auto-login prompt; nudge with a newline if the
        # banner has already settled.
        time.sleep(0.2)
        self.send(b"\n")
        self.wait_for(self.prompt, timeout_s=60.0)
        # Disable echo + widen the terminal. Without this, long fio
        # command lines wrap at column 80 in the guest tty, splitting
        # our completion-marker bytes across `\r\n` and breaking
        # `buffer.find(marker)`. `stty -echo` is the bigger hammer:
        # input characters typed at the shell stop appearing in the
        # output stream, so `run_cmd`'s marker shows up exactly once
        # (in the actual `echo MARKER` output, not in the cmd echo).
        self.send(b"stty -echo cols 4096 rows 1024\n")
        time.sleep(0.5)  # let stty take effect before the next cmd
        # Quiet kernel printk so it doesn't pollute our output captures.
        self.run_cmd("dmesg -n 1", timeout_s=5)
        return self

    def __exit__(self, *_: object) -> None:
        if self.proc is not None:
            self.proc.kill()
            self.proc.wait(timeout=2)

    def _reader(self) -> None:
        assert self.proc is not None
        while True:
            chunk = self.proc.stdout.read(8192)  # type: ignore[union-attr]
            if not chunk:
                return
            with self._lock:
                self._buf.extend(chunk)

    def send(self, data: bytes) -> None:
        assert self.proc is not None
        self.proc.stdin.write(data)  # type: ignore[union-attr]
        self.proc.stdin.flush()  # type: ignore[union-attr]

    def buffer_len(self) -> int:
        with self._lock:
            return len(self._buf)

    def wait_for(self, needle: bytes, timeout_s: float, from_idx: int = 0) -> int:
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            with self._lock:
                idx = bytes(self._buf).find(needle, from_idx)
                if idx >= 0:
                    return idx
            time.sleep(0.02)
        with self._lock:
            tail = bytes(self._buf)[-400:]
        fail(f"TIMEOUT waiting for {needle!r} — last 400 bytes: {tail!r}")
        return -1  # unreachable; fail() exits

    def run_cmd(self, cmd: str, timeout_s: float = 30) -> str:
        """Send `<cmd>\\n`, wait for a unique trailing marker, return
        the cmd's stdout (between the prompt and the marker).

        Assumes `stty -echo` was already sent during `__enter__`, so
        the typed cmd doesn't appear in the output buffer — the
        marker we wait for is unambiguously from `echo MARKER`, not
        from a cmd-echo wrap that bisected the marker bytes.
        """
        marker = f"___BENCHMARK_DONE_{int(time.time() * 1e6) & 0xffff}___"
        from_idx = self.buffer_len()
        self.send(f"{cmd} ; echo {marker}\n".encode())
        end = self.wait_for(marker.encode(), timeout_s=timeout_s, from_idx=from_idx)
        with self._lock:
            window = bytes(self._buf[from_idx:end])
        text = window.decode("utf-8", errors="replace")
        return text.rstrip("\n").rstrip("\r")


# ---- CSV emission ----------------------------------------------------------


@dataclass
class BenchResult:
    benchmark: str
    metric: str
    value: float
    unit: str

    def csv_line(self) -> str:
        return f"{self.benchmark},{self.metric},{self.value:.6g},{self.unit}"


def write_csv(path: Path, results: Iterable[BenchResult]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w") as f:
        f.write("benchmark,metric,value,unit\n")
        for r in results:
            f.write(r.csv_line() + "\n")


# ---- Baseline diff ---------------------------------------------------------


def load_csv(path: Path) -> dict[tuple[str, str], BenchResult]:
    """{(benchmark, metric): result} for easy diffing."""
    out: dict[tuple[str, str], BenchResult] = {}
    with path.open() as f:
        next(f)  # header
        for raw in f:
            line = raw.strip()
            if not line:
                continue
            parts = line.split(",")
            if len(parts) != 4:
                continue
            benchmark, metric, value_s, unit = parts
            try:
                value = float(value_s)
            except ValueError:
                continue
            out[(benchmark, metric)] = BenchResult(benchmark, metric, value, unit)
    return out


def compare_to_baseline(
    current: list[BenchResult],
    baseline: dict[tuple[str, str], BenchResult],
    threshold_pct: float = 10.0,
) -> list[str]:
    """Return list of regression descriptions (empty list = no regression).

    "Regression" means *worse* — for throughput metrics (MB/s, IOPS,
    bytes_per_sec) lower is worse; for latency metrics (anything with
    "latency" in the name) higher is worse. For unknown metrics,
    treat any change > threshold as a regression and let the
    operator decide.
    """
    higher_is_worse = ("latency",)
    regressions: list[str] = []
    for cur in current:
        key = (cur.benchmark, cur.metric)
        base = baseline.get(key)
        if base is None:
            continue
        if base.value == 0:
            continue
        delta_pct = (cur.value - base.value) / base.value * 100.0
        worse = (
            (delta_pct > threshold_pct)
            if any(s in cur.metric for s in higher_is_worse)
            else (delta_pct < -threshold_pct)
        )
        if worse:
            regressions.append(
                f"{cur.benchmark}.{cur.metric}: "
                f"{base.value:.4g} {base.unit} -> {cur.value:.4g} {cur.unit} "
                f"({delta_pct:+.1f}%)"
            )
    return regressions
