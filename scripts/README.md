# Hardware soak scripts

Scripts that exercise the daemon's new code paths on real hardware,
with per-step assertions. A non-zero exit means regression; zero exit
with a `PASS:` line means it held up.

These aren't part of `cargo test` — they need a live Blackhole card and
the tt-kmd driver, plus a `rootfs.ext4` and the firmware bundle
(`fw_jump.bin` / `Image` / `blackhole-card.dtb`) in the project root.
Run them from `bhx/` after `cargo build`.

## Scripts

| Script | What it exercises |
|--------|-------------------|
| `soak_warm_resume.sh` | N cycles of `daemon stop` + `daemon start`; asserts each restart re-adopts L2CPU 0 via warm-resume (probe passes, slot adopted). |
| `soak_cold_boot.sh` | High-iteration cold-boot regression hunt for the historical Ubuntu-3% probe-time class. Each iteration: tt-smi reset → daemon start → cold boot L2CPU with rng+blk+net+console → wait for the 4 expected `[probe-status] ... reached STATUS_DRIVER_OK` log lines (60 s timeout) → assert zero `STATUS reset to 0` / `STATUS_FAILED set` / V1-path log lines → daemon stop. Records per-iter timings + verdict to a CSV; archives the daemon log of any failed iter. Default 50 iters (~7 min); `ITERATIONS=200` is the regression-hunt run (~28 min). |
| `soak_add_remove.sh`  | N cycles of `add-disk` / `remove-disk` / `add-net` / `remove-net`; asserts `daemon status` after each step. Also verifies double-remove errors cleanly without mutating the slot. |
| `soak_concurrent.sh`  | Boots all 4 L2CPUs in parallel (each with its own `rootfs-N.ext4`), then runs N iterations of **4-way concurrent** `remove-disk`/`add-disk`/`remove-net`/`add-net` hammering sibling slots in parallel, with a background `daemon status` poller alongside. Chip-wide AXI ops go through `SharedChip::seq_lock`; per-L2CPU NOC traffic goes through each core's own fd. See [issue #1](https://github.com/olofj/bhx/issues/1). |
| `soak_kill_recovery.sh` | N SIGKILL-the-daemon cycles. After each kill, asserts the next `daemon start` cleans up stale runtime files, the warm-resume probe adopts the still-live L2CPU, and add-disk + add-net re-attach successfully. Targets the dirty-shutdown path that graceful `daemon stop` doesn't exercise. |
| `soak_disk_io_pressure.sh` | N `remove-disk` calls while the guest is in steady-state I/O (kernel journal + systemd housekeeping). Asserts each remove-disk returns within `TIMEOUT` seconds (default 5; healthy runs are ~300 ms), the daemon survives, and the slot becomes addressable for re-add. Light pressure — the buildroot-only [`soak_fio_remove_disk.py`](#soak_fio_remove_diskpy) drives real fio writes for a stronger version of the same test. |
| `soak_fio_remove_disk.py` | Like `soak_disk_io_pressure.sh` but drives a real `fio` job inside the guest writing 64 MiB to the rootfs at the moment we yank the disk. Drives the virtio-blk descriptor path much harder than the kernel-journal-only version. Requires a buildroot rootfs **with the cycle-test init script removed** (the standard `third_party/buildroot/rootfs.ext4` ships `/etc/init.d/S99-virtio-cycle-test` for the #156 cycle test, which races getty for the console). Strip it once with `cp third_party/buildroot/rootfs.ext4 buildroot-stripped.ext4 && /sbin/e2fsck -fy buildroot-stripped.ext4 && /sbin/debugfs -w -R 'rm /etc/init.d/S99-virtio-cycle-test' buildroot-stripped.ext4`, then run with `ROOTFS=./buildroot-stripped.ext4`. Tested clean to 100 iterations under V2.1. |
| `soak_irq_parity.py` | Detects missed PLIC interrupt edges in `InterruptController::set_interrupt` (#195) by comparing daemon-side `bhx_{blk,net,console,rng}_interrupts_total` against guest-side `/proc/interrupts` virtio row sums during a 16-job direct=1 fio randwrite workload. Boots rng + blk + net + console for max PLIC contention. Asserts no sliding window of `STALL_WINDOW_SEC` seconds drops more than `STALL_TOLERANCE` (default 5 %) of fired IRQs. Default 300 s (~5 min); 60 s smoke run on current `main` reliably shows a step-function loss episode (single-second window with 640 fired / 0 received). |
| `soak_virtio_burst.py` | Sustained multi-queue virtio burst regression gate for the V2 dispatch path. Drives 16-job direct=1 fio randwrite + a tight `printf` loop to `/dev/console` for `DURATION_SEC` seconds, samples the daemon's `/metrics` every 1 s into a CSV, then asserts `bhx_dispatch_passes_total > 0` and zero daemon-log matches for `kick.*drop\|rescue\|throttle.*ENGAGE` (the regex catches V1-path activity that should never exist post-V2). Auto-strips `/etc/init.d/S99-virtio-cycle-test` from a private `/tmp/burst-rootfs.ext4` copy so getty actually fires. |
| `soak_net_teardown.sh` | N `remove-net` calls while a host-side TCP session is held open against the slirp-forwarded SSH port. Asserts the held connection drops cleanly (no host hang), the daemon survives, and add-net brings the listener back up. Doesn't depend on guest SSH credentials — just exercises the TCP-listener teardown. |
| `console_roundtrip.py` | End-to-end console I/O stress — logs into the guest via `connect`, puts the tty in raw mode, then roundtrips 64 KiB of base64 text in each direction (guest→host and host→guest) and compares sha256. Validates `chip_console`'s `push_char` / `pop_char` + `ConsoleHub` fan-out under sustained transfers. Auto-detects buildroot (auto-login on `# `) vs Debian (`login:` → `debian\r` → `$ `); silences kernel printk to the console before the test so async kernel messages don't pollute the captured stream. See "Concurrent console roundtrip" below for the 4-way stress form. |
| `soak_endurance.sh` | Long-running drift soak (default 8 h). Add-disk / remove-disk / add-net / remove-net cycle every `ITER_INTERVAL` seconds, with daemon `RSS` / `VSZ` / open-fd-count captured per-iteration into a CSV. Fails if RSS grows >`RSS_DRIFT_PCT`% (default 25) or fd-count grows >`FD_DRIFT_ABS` (default 10) above the per-uptime baseline. Periodically (`WARM_RESUME_EVERY` iters, default 100) does a daemon stop/start drill so warm-resume gets exercised across thousands of slot mutations. Background `connect` client stays attached the whole run so the chip-console pump path is continuously warm. Catches fd leaks, slow memory growth, slirp state accumulation, u32 counter wraparound — drift that the short soaks miss. |
| `profile_daemon.sh` | Capture a `samply` CPU profile of the daemon for a fixed duration. Builds via the `profiling` cargo profile (release + debug info). Three scenarios: `--scenario idle` (no workload — surfaces the poll-loop hot path), `--scenario fio` (drives guest fio in parallel — surfaces the disk worker), `--scenario soak` (runs `soak_concurrent.sh` alongside — surfaces the dispatch path). Output goes to `profile-<scenario>-<timestamp>.json.gz`; view via `samply load <file>`. Used to quantify and verify the three-tier adaptive sleep (#27). |
| `test_tensix_smoke.sh` | End-to-end hardware smoke for the M1+M2+M3 Tensix-engine work (#67/#68/#69). Resets the card, runs `debug pick-tile` + `debug telemetry-dump` (M2), `debug tensix-hello` (M1 BRISC bring-up), resets, then `debug tensix-virtio` (M3 register-file engine + STATUS state machine + QUEUE_SEL multiplexer). Asserts on the output of each step; resets the card on the way out via `trap`. ~10 s end-to-end. Refuses to run when the daemon is up. |

## Env overrides

All scripts honour:

- `ITERATIONS`     — soak count (defaults: 5 for warm-resume, 10 for add/remove, 5 for concurrent).
- `BINARY`         — path to `bhx` (default `./target/debug/bhx`).
- `LOG_FILE`       — daemon log path (default `./daemon-card0.log`).
- `CARD`           — tt device index (default 0).
- `L2CPU`          — core index to exercise (default 0, ignored by `soak_concurrent.sh` which always uses all 4).
- `STATUS_POLL_HZ` — background status poll frequency in `soak_concurrent.sh` (default 20).
- `TIMEOUT`        — per-step timeout for `soak_disk_io_pressure.sh` and `soak_net_teardown.sh` (default 5 s).
- `PORT_WAIT`      — max wait for guest sshd to come up in `soak_net_teardown.sh` (default 60 s; bump for slow boots).
- `ROOTFS`         — disk image to attach. Auto-detected: `third_party/buildroot/rootfs.ext4` (the buildroot test image — preferred) → `./rootfs.ext4` (legacy `image pull debian` location). Set explicitly to override.

## Typical use

```bash
cargo build
bash scripts/soak_warm_resume.sh         # 5 cycles, ~1 min
bash scripts/soak_add_remove.sh          # 10 cycles, ~20 s
bash scripts/soak_concurrent.sh          # 4-core cold boot + 5 concurrent cycles, ~2 min

ITERATIONS=20 bash scripts/soak_warm_resume.sh   # longer soak

# Overnight drift soak — 8 hours, RSS/fd-count tracked per iteration.
DURATION_HOURS=8 bash scripts/soak_endurance.sh

# Quick smoke of the endurance script (~3 min, ~6 cycles).
DURATION_HOURS=0.05 ITER_INTERVAL=10 bash scripts/soak_endurance.sh
```

The endurance soak's CSV (`./soak_endurance-<timestamp>.csv`) is the
primary artifact for an overnight run — drop it in the PR description
or attach it to a release tag so trends are reviewable historically.

Each `soak_*.sh` cleans up on exit (daemon stopped via trap), so Ctrl-C
or an assertion failure leaves the host in a recoverable state. If you
do wedge the chip,
`(. ~/.tenstorrent-venv/bin/activate && tt-smi -r)` resets it.

### Concurrent console roundtrip

`console_roundtrip.py` targets one L2CPU per invocation (`--l2cpu N`);
the caller wires up however many parallel instances they want. A
typical 4-way stress run:

```bash
# Fresh chip, boot all 4 L2CPUs serially, wait for each login prompt.
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null
./target/debug/bhx daemon start -t 0 --log-file ./daemon-card0.log
for i in 0 1 2 3; do
    cp --reflink=auto rootfs.ext4 rootfs-$i.ext4
    ./target/debug/bhx boot -l $i -d rootfs-$i.ext4
    until timeout 5 ./target/debug/bhx connect -l $i </dev/null 2>/dev/null \
            | grep -q "login:"; do sleep 3; done
done

# Fire 4 roundtrips in parallel.
for i in 0 1 2 3; do
    (python3 scripts/console_roundtrip.py --l2cpu $i \
         > /tmp/rt_$i.log 2>&1; echo "l2cpu $i exit=$?") &
done
wait

# Each /tmp/rt_$i.log should end with "ALL PASS".
```

Two rootfs flavors are supported, auto-detected from the prompt that
appears first:
- **Buildroot** (`third_party/buildroot/rootfs.ext4`, recommended for soaks):
  drops to `# ` immediately, no login. See [third_party/buildroot/](../third_party/buildroot/).
- **Debian** (legacy `image pull debian` flow): needs a `debian` user
  with a passwordless console login.

## What isn't covered here

These hammer the most common stressors but a few things still aren't:

- multi-card concurrency (we only have one Blackhole on this host)
- long-running endurance soaks (>1 h cycles) — the existing scripts
  are short-cycle stress, not stability runs
- application-level guest I/O patterns (real workloads, not just
  kernel journal) during disk teardown
