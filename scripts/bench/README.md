# scripts/bench/ — daemon I/O baselines + regression detection

Hardware-only benchmarks for the three I/O surfaces the daemon
mediates: virtio-blk (disk), libvdeslirp (network), and the
chip-side virtual UART (console).

The soaks (`scripts/soak_*.sh`) verify pass/fail. These benchmarks
verify *how fast*. A soak still PASSes if the daemon halves its
disk throughput; the disk benchmark catches that within 3 seconds
of regression. Cadence: run before every release tag, and any time
you touch hot-path code in `virtio/`, `chip_console.rs`,
`shared_chip.rs`, or anywhere that can perturb the adaptive sleep
tiers (#27).

## Quick start

```sh
cargo build
bash scripts/bench/run_all.sh
# -> scripts/bench/results/run-<timestamp>.csv (one row per metric)

# regression-fail mode (CI candidate; manual today):
bash scripts/bench/run_all.sh --baseline scripts/bench/results/baseline.csv
```

Each individual bench is also runnable on its own:

```sh
python3 scripts/bench/disk.py
python3 scripts/bench/console.py
python3 scripts/bench/net.py
```

## Prereqs

- A built daemon: `cargo build`.
- A buildroot rootfs with `fio`, `iperf3`, `dropbear`: `make -C third_party/buildroot`
  (one-time, ~30 min cold).
- Host packages:
  - `e2fsprogs` (`e2fsck` + `resize2fs`) — disk.py grows a rootfs
    copy to make room for fio test files.
  - `iperf3` — required for the net benchmark only. Without it,
    net.py emits SKIP rows in the CSV instead of failing.
- A live Blackhole card and `tt-kmd` driver. Same deps as the soaks.

The disk bench writes to `scripts/bench/results/rootfs-bench.ext4`
(~1 GiB, gitignored). It's regenerated only when missing or
under-sized — re-runs are a no-op for the disk-prep step.

## What each benchmark measures

### disk.py (~3 minutes)

Drives `fio` inside the guest, three job profiles:

| job | rw | bs | iodepth | duration |
|---|---|---|---|---|
| `seq_write_4M_qd1` | write | 4M | 1 | 30s |
| `rand_write_4k_qd16` | randwrite | 4k | 16 | 30s |
| `seq_read_4M_qd1` | read | 4M | 1 | 30s |

Three metrics per job — bandwidth (MB/s), IOPS, p99 latency (μs)
— so the CSV is 9 rows. Each job tests a different surface of the
virtio-blk path:

- `seq_write_4M_qd1`: contiguous writes — closest to the rootfs-image
  write workload during normal guest use.
- `rand_write_4k_qd16`: small-block random — exercises the virtio
  descriptor-chain machinery harder.
- `seq_read_4M_qd1`: pure read path. No fsync, cleaner baseline.

### console.py (~1 minute)

Two metrics:

- `bytes_per_sec_g2h`: throughput from guest → host. The chip's
  virtual UART has a 4 KiB ring; the daemon's `chip_console::uart_pass`
  drains it. This number is the rate-limiting step for any
  console-driven workload (and for the `daemon logs --follow`
  experience).
- `roundtrip_latency_p{50,99,mean}_us`: 200 single-byte
  echo round trips, host → guest → host. p99 catches regressions
  in the dlog/scrollback/hub fan-out that throughput alone misses.

### net.py (~1 minute)

`iperf3` between host and guest. Two metrics:

- `tcp_egress_30s.bandwidth_mbps`: guest → host (10.0.2.2), 30s.
- `tcp_ingress_30s.bandwidth_mbps`: host → guest via the slirp
  forward port, 30s.

If `iperf3` isn't on the host, both rows are emitted with
`unit=SKIP` rather than failing the run.

## What "metric got worse" means per surface

- **Disk bandwidth/IOPS down**: virtio descriptor-chain handling
  got slower, the worker poll-tier shifted (less FAST, more SLOW),
  or fio is now hitting a bottleneck the daemon didn't have before.
  Diff `bhx_blk_*` and `bhx_worker_tier_seconds_total{worker="virtio_blk"}`
  in the daemon's `/metrics` to localize.
- **Disk p99 latency up**: even occasional stalls. Look at
  `bhx_blk_errors_total{reason="ioerr"}` — the rootfs-grow can
  expose IOERR if the test file overflows.
- **Console throughput/latency**: regressions usually come from
  `chip_console::uart_pass`'s adaptive-sleep tuning (#27) or
  `console_hub` fan-out. The hub holds an internal Mutex; a code
  change that lengthens the critical section shows up here.
- **Net throughput**: slirp + `virtio::network`. Cross-reference
  against `bhx_net_packets_total` rate during the run; if packet
  count is still high but bytes are low, the descriptor path
  fragmented packets.

## Refreshing the baseline

After a clean run on a tag-able `main`:

```sh
bash scripts/bench/run_all.sh
cp scripts/bench/results/run-<timestamp>.csv scripts/bench/results/baseline.csv
git add scripts/bench/results/baseline.csv
git commit -m "bench: refresh baseline (<release-tag>)"
```

Then future PRs pass `--baseline` to detect regressions. The
threshold is ±10% (latency: higher is worse; throughput: lower is
worse) — change in `lib.compare_to_baseline` if needed.

## Out of scope

- Multi-card / cluster benchmarks. Single card, single host.
- CI integration. CI doesn't run on hardware. The bench is an
  operator-driven gate before tagging.
- Hypervisor comparisons (qemu/kvm). We benchmark *this* daemon.
- Per-virtio-request timing. End-to-end metrics are the contract;
  go to `scripts/profile_daemon.sh` (samply traces) when a
  regression needs deeper localization.
- Power consumption.
