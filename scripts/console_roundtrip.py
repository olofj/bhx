#!/usr/bin/env python3
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT

"""
End-to-end virtual-UART console I/O stress test via the daemon-mediated
`connect` RPC. Drives everything through the console — no SSH required.

Prereqs (caller responsibility):
  - Daemon running on the selected card, the selected L2CPU booted with
    a guest disk reachable.

Auto-detects two rootfs flavors:
  - Debian-style: `login:` prompt → send `debian\\r` → `$ ` prompt.
  - Buildroot-style: auto-logged-in `# ` prompt directly (no login
    needed, e.g. third_party/buildroot/output/.../rootfs.ext4 from #16).

Flow:
  1. Spawn `connect -l N` (default `--mode rw`). Reader thread
     accumulates stdout bytes.
  2. Race-wait for either a `login:` prompt or a bare `# ` prompt
     (whichever comes first within LOGIN_WAIT seconds).
  3. If `login:` won, send `debian\\r` and wait for `$ ` shell prompt.
     If `# ` won, we're already at the shell.
  4. Send `stty -echo` (so the next command's input isn't echoed back).
  5. Send one big compound command that flips stty raw, runs the
     guest-side test program (printf markers + cat file + sha256 read-
     back), then restores stty sane.
  6. Parse markers out of the stream. Compare SHAs.

Markers use leading/trailing `__` which cannot appear in base64 output,
so we can carve payloads out of the stream unambiguously.

Usage:
    console_roundtrip.py [--l2cpu N] [--ttdevice N] [--size BYTES]
"""

import argparse
import hashlib
import os
import secrets
import string
import subprocess
import sys
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument("--l2cpu", type=int, default=0)
parser.add_argument("--ttdevice", type=int, default=0)
parser.add_argument("--size", type=int, default=64 * 1024)
args = parser.parse_args()

BINARY = os.path.abspath("./target/debug/bhx")
FILE_SIZE = args.size
L2CPU = str(args.l2cpu)
TTDEVICE = str(args.ttdevice)
TAG = f"[console-test l2cpu={L2CPU}]"

# Markers start/end with '__' which never appears in base64 output, so
# they're unambiguous inside the data stream.
MARK_GH_START = b"__MGH::START__"
MARK_GH_END = b"__MGH::END__"
MARK_HG_START = b"__MHG::START__"
MARK_HG_END = b"__MHG::END__"

# Deadlines (seconds)
LOGIN_WAIT = 30
SHELL_WAIT = 20
RUN_WAIT = 60


def say(msg: str) -> None:
    print(f"{TAG} {msg}", flush=True)


# ---------------------------------------------------------------------------
# Host-side payload: 64 KiB of base64 chars, no newlines (so we don't have
# to worry about terminal CR/LF surprises — though the tty is in raw/opost-
# off mode during the transfer anyway).
# ---------------------------------------------------------------------------
alphabet = string.ascii_letters + string.digits + "+/"
host_payload = "".join(secrets.choice(alphabet) for _ in range(FILE_SIZE)).encode("ascii")
assert len(host_payload) == FILE_SIZE
host_sha = hashlib.sha256(host_payload).hexdigest()
say(f"host-side payload built, sha256 = {host_sha}")


# ---------------------------------------------------------------------------
# Launch connect (default --mode rw) with stdin + stdout piped.
# ---------------------------------------------------------------------------
say("spawning `connect`")
proc = subprocess.Popen(
    [BINARY, "connect", "-t", TTDEVICE, "-l", L2CPU],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL,
    bufsize=0,
)

buf = bytearray()
buf_lock = threading.Lock()


def reader():
    while True:
        chunk = proc.stdout.read(8192)
        if not chunk:
            return
        with buf_lock:
            buf.extend(chunk)


t = threading.Thread(target=reader, daemon=True)
t.start()


def wait_for(needle: bytes, deadline: float, from_idx: int = 0) -> int:
    """Return index of first occurrence of `needle` in the shared buf at or
    after `from_idx`. Polls until `deadline` (wall-clock seconds since epoch)."""
    while time.time() < deadline:
        with buf_lock:
            idx = bytes(buf).find(needle, from_idx)
            if idx >= 0:
                return idx
        time.sleep(0.05)
    with buf_lock:
        tail = bytes(buf)[-400:]
    say(f"TIMEOUT waiting for {needle!r} — last 400 bytes of stream: {tail!r}")
    proc.kill()
    raise SystemExit(10)


def send(data: bytes) -> None:
    proc.stdin.write(data)
    proc.stdin.flush()


# ---------------------------------------------------------------------------
# Step 1: get to a shell prompt. Race between `login:` (Debian-style;
# we type `debian\r` to log in) and a bare `# ` prompt (buildroot-style
# auto-login; we're already root). Whichever the rootfs hands us first
# within LOGIN_WAIT seconds wins.
# ---------------------------------------------------------------------------
say("waiting for either 'login:' or '# ' prompt")
deadline = time.time() + LOGIN_WAIT
prompt_idx = -1
auto_login = False
while time.time() < deadline:
    with buf_lock:
        snapshot = bytes(buf)
    li = snapshot.find(b"login:")
    hi = snapshot.find(b"# ")
    if li >= 0 and (hi < 0 or li < hi):
        # Debian-style — type the username and wait for `$ `.
        time.sleep(0.3)  # let getty finish rendering before typing
        send(b"debian\r")
        say("sent 'debian\\r' — waiting for shell prompt")
        # Debian default prompt is `debian@tt-blackhole:~$ ` — match `$ `.
        prompt_idx = wait_for(b"$ ", time.time() + SHELL_WAIT, from_idx=li)
        say("shell prompt found (Debian)")
        break
    if hi >= 0:
        auto_login = True
        prompt_idx = hi + len(b"# ")
        say("auto-login detected (buildroot) — already at root shell")
        break
    time.sleep(0.05)

if prompt_idx < 0:
    with buf_lock:
        tail = bytes(buf)[-400:]
    say(f"TIMEOUT waiting for login or shell prompt — last 400 bytes: {tail!r}")
    proc.kill()
    raise SystemExit(10)


# ---------------------------------------------------------------------------
# Step 2a: turn off input echo so subsequent commands we send don't appear
# in the output stream. Without this, bash echoes every character we type
# in cooked mode — including the marker strings inside the command body —
# and the parser can't tell the echoed-input marker from the actual
# printf-output marker.
# ---------------------------------------------------------------------------
# Pick the prompt suffix matching the shell we landed on. busybox's
# default PS1 ends in `# `; bash's user PS1 ends in `$ `.
prompt_marker = b"# " if auto_login else b"$ "

# Silence kernel printk to the console. Without this, lazy-init lines
# like `random: crng init done` arrive mid-payload and pollute the
# byte stream the host is parsing. Level 1 = emergency-only on
# console; the guest's dmesg buffer is unaffected.
send(b"dmesg -n 1\r")
with buf_lock:
    cursor = len(buf)
wait_for(prompt_marker, time.time() + SHELL_WAIT, from_idx=cursor)

send(b"stty -echo\r")
# Wait for a new prompt to appear after the one we already saw (the
# stty command's execution reprints PS1).
with buf_lock:
    echo_cursor = len(buf)
# We can't just wait_for(prompt_marker) because the previous prompt is
# still in the buffer before echo_cursor — use from_idx to skip it.
wait_for(prompt_marker, time.time() + SHELL_WAIT, from_idx=echo_cursor)


# ---------------------------------------------------------------------------
# Step 2b: ship the compound test command. Echo is off now, so nothing
# about the command text appears in the output — only the printf markers
# and tool output.
# ---------------------------------------------------------------------------
with buf_lock:
    cursor = len(buf)

guest_cmd = (
    "stty raw -opost; "
    # Generate 64 KiB of guest-side base64 random, *stripped of newlines*,
    # into /tmp/src.bin. `base64 -w 0` does the same but some coreutils
    # versions differ; `tr -d '\\n'` is universal. We take 65536 bytes of
    # the flattened stream. No newlines means the kernel tty's lingering
    # ONLCR (which I can't seem to turn off with stty raw -opost) has
    # nothing to translate — bytes pass through exactly.
    "printf %s " + MARK_GH_START.decode() + "; "
    "base64 /dev/urandom | tr -d '\\n' | head -c " + str(FILE_SIZE) + " | tee /tmp/src.bin; "
    "printf %s " + MARK_GH_END.decode() + "; "
    "sha256sum /tmp/src.bin | awk '{print $1}'; "
    # Ready to receive: host will now write FILE_SIZE bytes to stdin. `head
    # -c N` consumes exactly that, sha256sum emits the hex, we close out
    # with the HG markers.
    "printf %s " + MARK_HG_START.decode() + "; "
    "head -c " + str(FILE_SIZE) + " > /tmp/dst.bin; "
    "sha256sum /tmp/dst.bin | awk '{print $1}'; "
    "printf %s " + MARK_HG_END.decode() + "; "
    "stty sane\r"
)
send(guest_cmd.encode("ascii"))
say(f"guest command issued ({len(guest_cmd)} bytes)")


# ---------------------------------------------------------------------------
# Step 3: G -> H — wait for both markers, slice payload, verify sha.
# ---------------------------------------------------------------------------
start_idx = wait_for(MARK_GH_START, time.time() + RUN_WAIT, from_idx=cursor)
# Payload starts just after the start marker.
data_start = start_idx + len(MARK_GH_START)
end_idx = wait_for(MARK_GH_END, time.time() + RUN_WAIT, from_idx=data_start)
with buf_lock:
    gh_payload = bytes(buf[data_start:end_idx])

say(f"G->H payload: {len(gh_payload)} bytes captured (expected {FILE_SIZE})")

# The guest-side sha follows MARK_GH_END, then later MARK_HG_START. Wait
# for the *next* marker as the upper bound so we never race the guest
# still emitting the sha256 hex.
hg_start_preview_idx = wait_for(MARK_HG_START, time.time() + RUN_WAIT,
                                from_idx=end_idx + len(MARK_GH_END))
with buf_lock:
    sha_region = bytes(buf[end_idx + len(MARK_GH_END):hg_start_preview_idx])
hex_chars = "".join(c for c in sha_region.decode("ascii", errors="replace") if c in string.hexdigits)
guest_src_sha = hex_chars[:64]
say(f"guest-reported src sha256 = {guest_src_sha}")

host_computed_src_sha = hashlib.sha256(gh_payload).hexdigest()
say(f"host sha256 of captured payload = {host_computed_src_sha}")

if len(gh_payload) != FILE_SIZE:
    say(f"G->H FAIL: payload length {len(gh_payload)} != {FILE_SIZE}")
    # Dump byte-category counts to diagnose.
    counts = {}
    for b in gh_payload:
        counts[b] = counts.get(b, 0) + 1
    non_b64 = {b: c for b, c in counts.items() if b not in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=\n"}
    say(f"  non-base64, non-LF bytes: {non_b64}")
    # Show first few lines.
    say(f"  first 200 bytes: {gh_payload[:200]!r}")
    proc.kill()
    raise SystemExit(11)
if host_computed_src_sha != guest_src_sha:
    say(f"G->H FAIL: sha mismatch (guest {guest_src_sha}, host {host_computed_src_sha})")
    proc.kill()
    raise SystemExit(12)
say("G->H PASS")


# ---------------------------------------------------------------------------
# Step 4: H -> G — HG start marker already found above while waiting for
# the sha. Send payload, wait for HG end marker, parse guest-reported sha,
# verify against host_sha.
# ---------------------------------------------------------------------------
hg_start_idx = hg_start_preview_idx
say("HG start marker seen — streaming payload to guest")

# Write in a single shot — the guest-side `head -c N` reads whatever
# arrives, so the kernel's tty input buffer handles piecemeal. One write
# is cleanest.
send(host_payload)
say(f"sent {FILE_SIZE} bytes to guest")

hg_end_idx = wait_for(MARK_HG_END, time.time() + RUN_WAIT,
                      from_idx=hg_start_idx + len(MARK_HG_START))
with buf_lock:
    between = bytes(buf[hg_start_idx + len(MARK_HG_START):hg_end_idx])
# After MARK_HG_START and before MARK_HG_END, the guest emits the sha of
# what it received (plus any tty leftovers). Extract the 64 hex chars.
hex_chars = "".join(c for c in between.decode("ascii", errors="replace") if c in string.hexdigits)
guest_dst_sha = hex_chars[-64:] if len(hex_chars) >= 64 else hex_chars
say(f"guest-reported dst sha256 = {guest_dst_sha}")

if guest_dst_sha != host_sha:
    say(f"H->G FAIL: sha mismatch (guest {guest_dst_sha}, host {host_sha})")
    say(f"  between markers, first 200 bytes: {between[:200]!r}")
    say(f"  between markers, last 200 bytes:  {between[-200:]!r}")
    proc.kill()
    raise SystemExit(13)
say("H->G PASS")


# ---------------------------------------------------------------------------
# Cleanup: send Ctrl-A x to detach, or just kill the process.
# ---------------------------------------------------------------------------
proc.kill()
proc.wait(timeout=2)
say("ALL PASS")
