# Telemetry

The daemon exposes a Prometheus-style HTTP exporter. Off by default —
enable per-card with `--metrics-port`:

```
bhx daemon start -t 0 --metrics-port 19100
curl -s http://127.0.0.1:19100/metrics
```

The listener binds to `127.0.0.1` only. Anything other than `GET /metrics`
returns 404. There is no auth and no TLS — loopback-only is the security
model. To scrape from a remote host, run an SSH tunnel.

## What's emitted

All metric names are prefixed `bhx_`. Counters end in `_total`; time
metrics end in `_seconds`; gauges have no suffix. Names follow the
Prometheus 0.0.4 text-format spec; the daemon refuses to start if a
bind() fails (mirrors the seccomp-install failure path) so a missing
`/metrics` is never silently degraded.

### Daemon-global

| Metric | Type | Labels | Bumped by |
|---|---|---|---|
| `bhx_daemon_uptime_seconds` | gauge | — | derived from `DaemonState.started` at scrape time |
| `bhx_daemon_clients_total` | counter | — | `handle_client` on accept |
| `bhx_daemon_clients_active` | gauge | — | `handle_client` (RAII guard inc on entry, dec on return) |
| `bhx_daemon_sandbox_status` | gauge | — | `sandbox::apply_landlock`: 0=disabled, 1=partial, 2=fully-enforced |
| `bhx_daemon_rpc_total` | counter | `method` | `handle_client` on every request, classified via `classify_request` |
| `bhx_daemon_rpc_errors_total` | counter | `method` | `handle_client` after dispatch, when `reply_err` set the per-thread `RPC_FAILED` flag |

`method` ∈ {`status`, `boot`, `attach_console`, `add_disk`,
`remove_disk`, `add_net`, `remove_net`, `stop`, `shutdown`}.

### Per-L2CPU

| Metric | Type | Labels | Bumped by |
|---|---|---|---|
| `bhx_l2cpu_uptime_seconds` | gauge | `idx` | `L2CpuSlot.started` snapshot at scrape time. Absent for empty slots. |
| `bhx_l2cpu_boot_total` | counter | `idx`, `kind` | `dispatch_boot` install path (`kind="cold"`); `warm_resume_released` adoption path (`kind="warm"`) |
| `bhx_l2cpu_console_clients` | gauge | `idx` | `ConsoleHub::{attach,detach,push_chip_output}` (the last one decrements when fan-out drops a slow client) |
| `bhx_l2cpu_console_bytes_total` | counter | `idx`, `direction` | `chip_console::uart_pass`: `g2h` on chip TX → hub batch; `h2g` per byte pushed into chip RX |
| `bhx_l2cpu_disks` | gauge | `idx` | derived from `L2CpuSlot.disks.len()` at scrape time |
| `bhx_l2cpu_net` | gauge | `idx` | derived from `L2CpuSlot.net.is_some()` at scrape time |

`idx` ∈ {`0`, `1`, `2`, `3`}; `kind` ∈ {`cold`, `warm`};
`direction` ∈ {`g2h`, `h2g`}.

### Per virtio-block

| Metric | Type | Labels | Bumped by |
|---|---|---|---|
| `bhx_blk_requests_total` | counter | `idx`, `disk_id`, `op` | `block::process_queue_complete` based on `req.type_` |
| `bhx_blk_bytes_total` | counter | `idx`, `disk_id`, `op` | same site, `data_offset` accumulator |
| `bhx_blk_errors_total` | counter | `idx`, `disk_id`, `reason` | same site, `req_status` discriminator |
| `bhx_blk_interrupts_total` | counter | `idx`, `disk_id` | `virtio::run_device` at the `set_interrupt` call site, gated on `InterruptKind::Block` |

`op` ∈ {`read`, `write`}; `reason` ∈ {`ioerr`, `unsupp`}; `disk_id`
is pinned at `"0"` today (one disk per L2CPU; the multi-disk
"Phase B" will widen the dimension without changing the metric name).

### Per virtio-net

| Metric | Type | Labels | Bumped by |
|---|---|---|---|
| `bhx_net_packets_total` | counter | `idx`, `direction` | `network::process_queue_complete`: queue 0 = `rx`, queue 1 = `tx` |
| `bhx_net_bytes_total` | counter | `idx`, `direction` | same site, `copy_len` |
| `bhx_net_interrupts_total` | counter | `idx` | `virtio::run_device` at the `set_interrupt` site, gated on `InterruptKind::Net` |

`direction` ∈ {`rx`, `tx`}.

### Worker poll-loop

The daemon's three long-running poll loops (`virtio::run_device` for
block + net, `chip_console::uart_pass`) all share the three-tier
adaptive sleep design from #27. These metrics expose the runtime
behavior of that design — together they answer "is the daemon
sleeping enough?" and "where is its CPU going?".

| Metric | Type | Labels | Bumped by |
|---|---|---|---|
| `bhx_worker_poll_iterations_total` | counter | `worker`, `idx`, `tier` | each loop iteration, after `classify_tier` picks a bucket |
| `bhx_worker_tier_seconds_total` | counter | `worker`, `idx`, `tier` | same site, by the chosen sleep duration. Stored internally as nanoseconds for cheap atomic adds; rendered as `value / 1e9` |

`worker` ∈ {`virtio_blk`, `virtio_net`, `chip_console`};
`tier` ∈ {`fast`, `slow`, `idle`}. Tier boundaries (`FAST_WINDOW`
and `IDLE_WINDOW`) are defined inside each loop — both are 200 ms
and 2 s today but allowed to diverge.

## Reading the output

A few prom-style queries operators tend to want, expressed as English:

- **Is the daemon idle?** Look at
  `bhx_worker_poll_iterations_total{tier="idle"}`. If it's barely
  moving while everything else moves, the workers are pinning a core.
  See #27.
- **How fast is virtio-blk?** Bytes/s = derivative of
  `bhx_blk_bytes_total{op="read"}` (or `op="write"`).
  Requests/s = same shape on `_requests_total`.
- **Are there I/O errors?** `bhx_blk_errors_total` should stay flat
  during normal operation. Any non-zero value means the guest sent a
  request the daemon couldn't satisfy (`reason="ioerr"` = overflow;
  `reason="unsupp"` = unrecognized type).
- **Is the chip console backed up?** `bhx_l2cpu_console_bytes_total{direction="g2h"}`
  going up while no clients are attached
  (`bhx_l2cpu_console_clients` = 0) means the daemon is pumping
  bytes into the hub's 64 KiB scrollback ring — fine until the ring
  wraps. Operators only see the most-recent 64 KiB.
- **Are RPCs failing?** `bhx_daemon_rpc_errors_total` per method.
  Cross-reference against the dlog file (set with `--log-file` or
  `daemon logs`) for the actual error messages.

## Cardinality

At full deployment (4 L2CPUs, all booted, both block + net attached)
the inventory expands to roughly 130 series. Comfortably small for
Prometheus.

## What's *not* exposed

- Histograms (`_bucket` rows). `_sum` / `_count` pairs cover what
  most operators want without the cardinality cost.
- Per-RPC tracing IDs.
- Per-thread CPU time.
- Anything from `tt-smi` (the chip-side telemetry tool).

These are out of scope for this exporter — they live in
`scripts/profile_daemon.sh` (samply traces) and the hardware tooling
respectively.

## Source

- `src/daemon/metrics.rs` — primitive types, registry statics,
  `render_prometheus`, the HTTP listener.
- `src/daemon/server.rs` — RPC counter + RPC errors wiring,
  L2CpuSlot derived gauges, sandbox-status set.
- `src/daemon/console_hub.rs` — clients gauge.
- `src/daemon/chip_console.rs` — console byte counters + worker tier
  bumps for `chip_console`.
- `src/virtio/mod.rs`, `src/virtio/block.rs`, `src/virtio/network.rs`
  — block + net counters, interrupt counters, worker tier bumps for
  the virtio workers.
