// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! In-process metrics + Prometheus text exporter.
//!
//! Off by default; enabled with `daemon start --metrics-port N`. Binds an
//! HTTP listener on 127.0.0.1:N (loopback only) and serves a single
//! endpoint, `GET /metrics`, in the standard Prometheus 0.0.4 text
//! format. No external dependencies — hand-rolled HTTP/1.1 parser is a
//! few dozen lines because the protocol surface we care about is "read
//! the request line, dispatch one of two responses".
//!
//! Primitive types (`Counter`, `Gauge`, `CounterVec<N>`, `GaugeVec<N>`)
//! are designed to be cheap on the hot path: `inc()` / `add()` are a
//! single relaxed atomic. They live in `static` slots so call sites can
//! reach them without going through a registry lookup. The
//! `render_prometheus` formatter has the metric inventory hard-coded —
//! it's a small static set, and explicit > clever.
//!
//! Threading model: a single dedicated thread runs the accept loop. One
//! request at a time is fine because Prometheus scrapes are sub-second
//! and infrequent. The thread polls the listener non-blocking with a
//! short sleep so it can notice the daemon's shutdown flag without
//! needing a wakeup pipe.
//!
//! Sandbox compatibility: the seccomp filter (#20) already allows
//! `socket`, `bind`, `listen`, `accept4`, `recvfrom`, `sendto`, and
//! `shutdown`. landlock doesn't restrict AF_INET sockets. No additional
//! policy work needed.

use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::daemon::DaemonState;
use crate::dlog;

// ============================================================================
// Primitives
// ============================================================================

/// Monotonically increasing u64. Cheapest hot-path primitive — `inc()` is a
/// single relaxed `fetch_add(1)`. Use for "events occurred N times" semantics.
pub struct Counter {
    value: AtomicU64,
}

impl Counter {
    pub const fn new() -> Self {
        Self {
            value: AtomicU64::new(0),
        }
    }
    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }
    pub fn add(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

impl Default for Counter {
    fn default() -> Self {
        Self::new()
    }
}

/// Signed gauge (i64). `set` / `inc` / `dec` for "current value" semantics.
/// Use when the metric can go down (active connections, queue depth, etc.).
pub struct Gauge {
    value: AtomicI64,
}

impl Gauge {
    pub const fn new() -> Self {
        Self {
            value: AtomicI64::new(0),
        }
    }
    pub fn set(&self, v: i64) {
        self.value.store(v, Ordering::Relaxed);
    }
    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec(&self) {
        self.value.fetch_sub(1, Ordering::Relaxed);
    }
    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }
}

impl Default for Gauge {
    fn default() -> Self {
        Self::new()
    }
}

/// Fixed-size array of counters keyed by a small finite index (most often
/// L2CPU index 0..3, sometimes virtio-block disk_id, etc.). Avoids the
/// runtime cost and allocation of a `HashMap<String, Counter>` for the
/// label tuples we actually have. Kept available even when the current
/// metric set doesn't instantiate a generic vec — specialized variants
/// (`PerL2cpuCounter`, `PerL2cpuDirCounter`, …) are the more common
/// shape today.
#[allow(dead_code)]
pub struct CounterVec<const N: usize> {
    values: [Counter; N],
}

#[allow(dead_code)]
impl<const N: usize> CounterVec<N> {
    pub const fn new() -> Self {
        // Workaround for `[Counter::new(); N]` not being permitted on
        // non-Copy types: build the array element-wise with a const fn.
        // This is one of the few places where a small unsafe-free
        // const-init helper is genuinely cleaner than the alternatives.
        Self {
            values: [const { Counter::new() }; N],
        }
    }
    /// Panics on out-of-range index — by design. Indexes come from
    /// finite enums (L2CPU 0..3, ops 0..1) and out-of-range means a
    /// caller bug, not a runtime data error.
    pub fn at(&self, idx: usize) -> &Counter {
        &self.values[idx]
    }
}

impl<const N: usize> Default for CounterVec<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Same shape as `CounterVec` but for `Gauge`s.
#[allow(dead_code)]
pub struct GaugeVec<const N: usize> {
    values: [Gauge; N],
}

#[allow(dead_code)]
impl<const N: usize> GaugeVec<N> {
    pub const fn new() -> Self {
        Self {
            values: [const { Gauge::new() }; N],
        }
    }
    pub fn at(&self, idx: usize) -> &Gauge {
        &self.values[idx]
    }
}

impl<const N: usize> Default for GaugeVec<N> {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Labeled metric shapes
// ============================================================================
//
// Each shape is its own type so call sites and the renderer share a
// schema. Verbose at definition; explicit at use site (`CONSOLE_BYTES.g2h(idx).add(n)`).

/// Per-L2CPU counter array (idx 0..3).
pub struct PerL2cpuCounter {
    values: [Counter; 4],
}
impl PerL2cpuCounter {
    pub const fn new() -> Self {
        Self {
            values: [const { Counter::new() }; 4],
        }
    }
    pub fn at(&self, idx: u8) -> &Counter {
        &self.values[idx as usize]
    }
}
impl Default for PerL2cpuCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-L2CPU gauge array (idx 0..3).
pub struct PerL2cpuGauge {
    values: [Gauge; 4],
}
impl PerL2cpuGauge {
    pub const fn new() -> Self {
        Self {
            values: [const { Gauge::new() }; 4],
        }
    }
    pub fn at(&self, idx: u8) -> &Gauge {
        &self.values[idx as usize]
    }
}
impl Default for PerL2cpuGauge {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-L2CPU + per-direction counter (chip-console bytes). `g2h` =
/// guest → host (chip TX ring drained into the hub); `h2g` = host →
/// guest (input pushed into the chip RX ring).
pub struct PerL2cpuDirCounter {
    g2h: [Counter; 4],
    h2g: [Counter; 4],
}
impl PerL2cpuDirCounter {
    pub const fn new() -> Self {
        Self {
            g2h: [const { Counter::new() }; 4],
            h2g: [const { Counter::new() }; 4],
        }
    }
    pub fn g2h(&self, idx: u8) -> &Counter {
        &self.g2h[idx as usize]
    }
    pub fn h2g(&self, idx: u8) -> &Counter {
        &self.h2g[idx as usize]
    }
}
impl Default for PerL2cpuDirCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-L2CPU virtio-block counter, partitioned by op. `read` =
/// `VIRTIO_BLK_T_IN`, `write` = `VIRTIO_BLK_T_OUT`. UNSUPP request
/// types don't bump either — they go through `PerL2cpuBlkErrors`.
pub struct PerL2cpuBlkOpCounter {
    read: [Counter; 4],
    write: [Counter; 4],
}
impl PerL2cpuBlkOpCounter {
    pub const fn new() -> Self {
        Self {
            read: [const { Counter::new() }; 4],
            write: [const { Counter::new() }; 4],
        }
    }
    pub fn read(&self, idx: u8) -> &Counter {
        &self.read[idx as usize]
    }
    pub fn write(&self, idx: u8) -> &Counter {
        &self.write[idx as usize]
    }
}
impl Default for PerL2cpuBlkOpCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-L2CPU virtio-block error counter. `ioerr` = request overflowed
/// the disk image's size (`VIRTIO_BLK_S_IOERR`); `unsupp` = guest sent
/// an unrecognized request type (`VIRTIO_BLK_S_UNSUPP`).
pub struct PerL2cpuBlkErrors {
    ioerr: [Counter; 4],
    unsupp: [Counter; 4],
}
impl PerL2cpuBlkErrors {
    pub const fn new() -> Self {
        Self {
            ioerr: [const { Counter::new() }; 4],
            unsupp: [const { Counter::new() }; 4],
        }
    }
    pub fn ioerr(&self, idx: u8) -> &Counter {
        &self.ioerr[idx as usize]
    }
    pub fn unsupp(&self, idx: u8) -> &Counter {
        &self.unsupp[idx as usize]
    }
}
impl Default for PerL2cpuBlkErrors {
    fn default() -> Self {
        Self::new()
    }
}

/// Worker poll-tier discriminator. Each adaptive-sleep iteration in
/// `virtio::run_device` and `chip_console::uart_pass` falls into one
/// of these tiers based on how long the loop has been quiet —
/// Fast (microseconds) when there's recent activity, Slow (~ms)
/// after the FAST_WINDOW expires, Idle (~10 ms) after IDLE_WINDOW
/// expires. See #27 for the rationale.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tier {
    Fast,
    Slow,
    Idle,
}
impl Tier {
    pub const fn name(self) -> &'static str {
        match self {
            Tier::Fast => "fast",
            Tier::Slow => "slow",
            Tier::Idle => "idle",
        }
    }
    pub const fn all() -> &'static [Tier] {
        &[Tier::Fast, Tier::Slow, Tier::Idle]
    }
}

/// Worker discriminator. One variant per long-running poll loop in
/// the daemon.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WorkerKind {
    VirtioBlk,
    VirtioNet,
    VirtioConsole,
    VirtioRng,
    ChipConsole,
}
impl WorkerKind {
    pub const fn name(self) -> &'static str {
        match self {
            WorkerKind::VirtioBlk => "virtio_blk",
            WorkerKind::VirtioNet => "virtio_net",
            WorkerKind::VirtioConsole => "virtio_console",
            WorkerKind::VirtioRng => "virtio_rng",
            WorkerKind::ChipConsole => "chip_console",
        }
    }
    pub const fn all() -> &'static [WorkerKind] {
        &[
            WorkerKind::VirtioBlk,
            WorkerKind::VirtioNet,
            WorkerKind::VirtioConsole,
            WorkerKind::VirtioRng,
            WorkerKind::ChipConsole,
        ]
    }
    fn idx(self) -> usize {
        match self {
            WorkerKind::VirtioBlk => 0,
            WorkerKind::VirtioNet => 1,
            WorkerKind::VirtioConsole => 2,
            WorkerKind::VirtioRng => 3,
            WorkerKind::ChipConsole => 4,
        }
    }
}

/// Classify an "elapsed since last activity" duration into one of the
/// three poll tiers. Pure function — extracted from `run_device` /
/// `uart_pass` so it's unit-testable. Each loop has its own
/// `fast_window` / `idle_window` (see #27).
pub fn classify_tier(elapsed: Duration, fast_window: Duration, idle_window: Duration) -> Tier {
    if elapsed < fast_window {
        Tier::Fast
    } else if elapsed < idle_window {
        Tier::Slow
    } else {
        Tier::Idle
    }
}

/// 3D counter: `[worker][idx][tier]`. 5 × 4 × 3 = 60 cells per metric.
/// Indexed by `at(WorkerKind, idx: u8, Tier)`. Both keying enums have
/// stable `name()` strings the renderer uses verbatim.
pub struct WorkerTierCounter {
    values: [[[Counter; 3]; 4]; 5],
}
impl WorkerTierCounter {
    pub const fn new() -> Self {
        Self {
            values: [const { [const { [const { Counter::new() }; 3] }; 4] }; 5],
        }
    }
    pub fn at(&self, worker: WorkerKind, idx: u8, tier: Tier) -> &Counter {
        &self.values[worker.idx()][idx as usize][tier as usize]
    }
}
impl Default for WorkerTierCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-L2CPU virtio-net counter, partitioned by direction. `rx` =
/// inbound (slirp → guest), `tx` = outbound (guest → slirp).
pub struct PerL2cpuNetDirCounter {
    rx: [Counter; 4],
    tx: [Counter; 4],
}
impl PerL2cpuNetDirCounter {
    pub const fn new() -> Self {
        Self {
            rx: [const { Counter::new() }; 4],
            tx: [const { Counter::new() }; 4],
        }
    }
    pub fn rx(&self, idx: u8) -> &Counter {
        &self.rx[idx as usize]
    }
    pub fn tx(&self, idx: u8) -> &Counter {
        &self.tx[idx as usize]
    }
}
impl Default for PerL2cpuNetDirCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// One counter per RPC method. Drives `bhx_daemon_rpc_total{method}`
/// and `_errors_total{method}`. Adding a new method = one field +
/// matching arm in `RpcMethod::name`.
pub struct RpcMethodCounter {
    pub status: Counter,
    pub boot: Counter,
    pub attach_console: Counter,
    pub add_disk: Counter,
    pub remove_disk: Counter,
    pub add_net: Counter,
    pub remove_net: Counter,
    pub add_console: Counter,
    pub remove_console: Counter,
    pub stop: Counter,
    pub shutdown: Counter,
}
impl RpcMethodCounter {
    pub const fn new() -> Self {
        Self {
            status: Counter::new(),
            boot: Counter::new(),
            attach_console: Counter::new(),
            add_disk: Counter::new(),
            remove_disk: Counter::new(),
            add_net: Counter::new(),
            remove_net: Counter::new(),
            add_console: Counter::new(),
            remove_console: Counter::new(),
            stop: Counter::new(),
            shutdown: Counter::new(),
        }
    }
    pub fn at(&self, m: RpcMethod) -> &Counter {
        match m {
            RpcMethod::Status => &self.status,
            RpcMethod::Boot => &self.boot,
            RpcMethod::AttachConsole => &self.attach_console,
            RpcMethod::AddDisk => &self.add_disk,
            RpcMethod::RemoveDisk => &self.remove_disk,
            RpcMethod::AddNet => &self.add_net,
            RpcMethod::RemoveNet => &self.remove_net,
            RpcMethod::AddConsole => &self.add_console,
            RpcMethod::RemoveConsole => &self.remove_console,
            RpcMethod::Stop => &self.stop,
            RpcMethod::Shutdown => &self.shutdown,
        }
    }
}
impl Default for RpcMethodCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Discriminator for the RPC method label. One variant per
/// `protocol::Request` arm we want to bucket separately.
#[derive(Copy, Clone, Debug)]
pub enum RpcMethod {
    Status,
    Boot,
    AttachConsole,
    AddDisk,
    RemoveDisk,
    AddNet,
    RemoveNet,
    AddConsole,
    RemoveConsole,
    Stop,
    Shutdown,
}
impl RpcMethod {
    pub const fn name(self) -> &'static str {
        match self {
            RpcMethod::Status => "status",
            RpcMethod::Boot => "boot",
            RpcMethod::AttachConsole => "attach_console",
            RpcMethod::AddDisk => "add_disk",
            RpcMethod::RemoveDisk => "remove_disk",
            RpcMethod::AddNet => "add_net",
            RpcMethod::RemoveNet => "remove_net",
            RpcMethod::AddConsole => "add_console",
            RpcMethod::RemoveConsole => "remove_console",
            RpcMethod::Stop => "stop",
            RpcMethod::Shutdown => "shutdown",
        }
    }
    pub const fn all() -> &'static [RpcMethod] {
        &[
            RpcMethod::Status,
            RpcMethod::Boot,
            RpcMethod::AttachConsole,
            RpcMethod::AddDisk,
            RpcMethod::RemoveDisk,
            RpcMethod::AddNet,
            RpcMethod::RemoveNet,
            RpcMethod::AddConsole,
            RpcMethod::RemoveConsole,
            RpcMethod::Stop,
            RpcMethod::Shutdown,
        ]
    }
}

// ============================================================================
// Global metrics
// ============================================================================

// --- Daemon-global ---

/// Cumulative count of accepted RPC client connections.
pub static DAEMON_CLIENTS_TOTAL: Counter = Counter::new();

/// Currently-connected RPC clients (active count, decremented on close).
pub static DAEMON_CLIENTS_ACTIVE: Gauge = Gauge::new();

/// Sandbox enforcement state. 0 = disabled, 1 = partially enforced,
/// 2 = fully enforced. Set by `sandbox::apply` (no-op on non-Linux).
pub static DAEMON_SANDBOX_STATUS: Gauge = Gauge::new();

/// Per-method RPC counters.
pub static DAEMON_RPC_TOTAL: RpcMethodCounter = RpcMethodCounter::new();

/// Per-method RPC failure counters (response was an `Error` variant
/// or framed response failed to write).
pub static DAEMON_RPC_ERRORS_TOTAL: RpcMethodCounter = RpcMethodCounter::new();

// --- Per-L2CPU ---

/// Cold-boot count per L2CPU (`dispatch_boot` install path).
pub static L2CPU_BOOT_COLD_TOTAL: PerL2cpuCounter = PerL2cpuCounter::new();

/// Warm-resume count per L2CPU (`warm_resume_released` adoption path).
pub static L2CPU_BOOT_WARM_TOTAL: PerL2cpuCounter = PerL2cpuCounter::new();

/// Currently-attached console clients per L2CPU (`ConsoleHub` writer registry).
pub static L2CPU_CONSOLE_CLIENTS: PerL2cpuGauge = PerL2cpuGauge::new();

/// Chip-console bytes per L2CPU per direction.
pub static L2CPU_CONSOLE_BYTES_TOTAL: PerL2cpuDirCounter = PerL2cpuDirCounter::new();

// --- Per virtio-block worker ---
//
// Today there's exactly one disk per L2CPU (Phase A in dispatch_add_disk),
// so the rendered metric pins `disk_id="0"`. When Phase B (multi-disk)
// lands, the `disk_id` dimension expands without changing the metric
// name — dashboards keyed off `disk_id` keep working.

/// Block requests completed per L2CPU per op (read|write).
pub static BLK_REQUESTS_TOTAL: PerL2cpuBlkOpCounter = PerL2cpuBlkOpCounter::new();

/// Block bytes transferred per L2CPU per op. Sum of the per-request
/// `data_offset` (total bytes attempted, including overflow chunks
/// counted by IOERR).
pub static BLK_BYTES_TOTAL: PerL2cpuBlkOpCounter = PerL2cpuBlkOpCounter::new();

/// Block error counter per L2CPU. `ioerr` = request crossed the disk
/// image's size; `unsupp` = guest sent an unknown request type.
pub static BLK_ERRORS_TOTAL: PerL2cpuBlkErrors = PerL2cpuBlkErrors::new();

// --- Per virtio-net worker ---

/// Net packets per L2CPU per direction.
pub static NET_PACKETS_TOTAL: PerL2cpuNetDirCounter = PerL2cpuNetDirCounter::new();

/// Net bytes per L2CPU per direction. Counts the actual bytes copied
/// to/from the slirp buffer (after `min(data_len, PACKET_SIZE)`).
pub static NET_BYTES_TOTAL: PerL2cpuNetDirCounter = PerL2cpuNetDirCounter::new();

/// Block-device interrupt count per L2CPU. Bumped on every
/// `set_interrupt` call from the block worker's `run_device` loop. In
/// the absence of interrupt coalescing, this tracks
/// `BLK_REQUESTS_TOTAL{read} + {write}` 1:1 — a divergence is the
/// observable signal that something dropped or coalesced.
pub static BLK_INTERRUPTS_TOTAL: PerL2cpuCounter = PerL2cpuCounter::new();

/// Net-device interrupt count per L2CPU. Same shape as
/// `BLK_INTERRUPTS_TOTAL`.
pub static NET_INTERRUPTS_TOTAL: PerL2cpuCounter = PerL2cpuCounter::new();

/// virtio-console interrupt count per L2CPU. Bumped from the console
/// worker's `run_device` loop. See #51.
pub static CONSOLE_INTERRUPTS_TOTAL: PerL2cpuCounter = PerL2cpuCounter::new();

/// virtio-rng interrupt count per L2CPU. Bumped from the rng worker's
/// `run_device` loop. See #62.
pub static RNG_INTERRUPTS_TOTAL: PerL2cpuCounter = PerL2cpuCounter::new();

// --- Worker poll loop ---

/// Iterations per worker per L2CPU per tier. A high count in
/// `tier="fast"` means the worker is busy; a high count in
/// `tier="idle"` means it's quiet. Together these tell the operator
/// where the daemon is spending its CPU. See #27.
pub static WORKER_POLL_ITERATIONS_TOTAL: WorkerTierCounter = WorkerTierCounter::new();

/// Cumulative wall time slept per tier, in nanoseconds. Internal
/// representation is u64 nanos so the increments are integer
/// `fetch_add` — the renderer divides by 1e9 to emit
/// `bhx_worker_tier_seconds_total` as floating-point seconds (the
/// canonical Prometheus shape for time).
pub static WORKER_TIER_NANOS_TOTAL: WorkerTierCounter = WorkerTierCounter::new();

// ============================================================================
// Prometheus text formatter
// ============================================================================

/// Render the current metric set in Prometheus text format (version
/// 0.0.4). Returns the full response body, suitable for the HTTP
/// listener to write back verbatim.
///
/// Walks the per-L2CPU slot mutexes for derived gauges (uptime, disks,
/// net, slot-state). Holds each slot lock briefly — same contention
/// model `dispatch_status` uses, which is well-tested under the
/// concurrent-soak.
pub fn render_prometheus(state: &DaemonState) -> String {
    let mut out = String::with_capacity(4096);

    // ----- Daemon-global -----

    write_gauge(
        &mut out,
        "bhx_daemon_uptime_seconds",
        "Daemon uptime in seconds.",
        state.started.elapsed().as_secs() as i64,
    );
    write_counter(
        &mut out,
        "bhx_daemon_clients_total",
        "Cumulative count of accepted RPC client connections.",
        DAEMON_CLIENTS_TOTAL.get(),
    );
    write_gauge(
        &mut out,
        "bhx_daemon_clients_active",
        "Currently-connected RPC clients.",
        DAEMON_CLIENTS_ACTIVE.get(),
    );
    write_gauge(
        &mut out,
        "bhx_daemon_sandbox_status",
        "Sandbox enforcement: 0=disabled, 1=partial, 2=fully-enforced.",
        DAEMON_SANDBOX_STATUS.get(),
    );

    // Per-method RPC totals. One emit pass per metric so HELP/TYPE
    // appear once at the top per Prometheus convention.
    let _ = writeln!(
        &mut out,
        "# HELP bhx_daemon_rpc_total Cumulative RPC count per method."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_daemon_rpc_total counter");
    for &m in RpcMethod::all() {
        let _ = writeln!(
            &mut out,
            "bhx_daemon_rpc_total{{method=\"{}\"}} {}",
            m.name(),
            DAEMON_RPC_TOTAL.at(m).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP bhx_daemon_rpc_errors_total RPC failures per method."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_daemon_rpc_errors_total counter");
    for &m in RpcMethod::all() {
        let _ = writeln!(
            &mut out,
            "bhx_daemon_rpc_errors_total{{method=\"{}\"}} {}",
            m.name(),
            DAEMON_RPC_ERRORS_TOTAL.at(m).get()
        );
    }

    // ----- Per-L2CPU -----

    let _ = writeln!(
        &mut out,
        "# HELP bhx_l2cpu_boot_total Boot count per L2CPU, by kind (cold|warm)."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_l2cpu_boot_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "bhx_l2cpu_boot_total{{idx=\"{}\",kind=\"cold\"}} {}",
            idx,
            L2CPU_BOOT_COLD_TOTAL.at(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "bhx_l2cpu_boot_total{{idx=\"{}\",kind=\"warm\"}} {}",
            idx,
            L2CPU_BOOT_WARM_TOTAL.at(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP bhx_l2cpu_console_clients Currently-attached console clients per L2CPU."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_l2cpu_console_clients gauge");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "bhx_l2cpu_console_clients{{idx=\"{}\"}} {}",
            idx,
            L2CPU_CONSOLE_CLIENTS.at(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP bhx_l2cpu_console_bytes_total Chip-console byte transfers per L2CPU \
         per direction (g2h = guest-to-host, h2g = host-to-guest)."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_l2cpu_console_bytes_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "bhx_l2cpu_console_bytes_total{{idx=\"{}\",direction=\"g2h\"}} {}",
            idx,
            L2CPU_CONSOLE_BYTES_TOTAL.g2h(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "bhx_l2cpu_console_bytes_total{{idx=\"{}\",direction=\"h2g\"}} {}",
            idx,
            L2CPU_CONSOLE_BYTES_TOTAL.h2g(idx).get()
        );
    }

    // ----- Per virtio-block -----
    //
    // disk_id="0" pinned today (one disk per L2CPU). Phase B will
    // expand the dimension without changing the metric name.

    let _ = writeln!(
        &mut out,
        "# HELP bhx_blk_requests_total Block requests completed per L2CPU per op."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_blk_requests_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "bhx_blk_requests_total{{idx=\"{}\",disk_id=\"0\",op=\"read\"}} {}",
            idx,
            BLK_REQUESTS_TOTAL.read(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "bhx_blk_requests_total{{idx=\"{}\",disk_id=\"0\",op=\"write\"}} {}",
            idx,
            BLK_REQUESTS_TOTAL.write(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP bhx_blk_bytes_total Block bytes transferred per L2CPU per op."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_blk_bytes_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "bhx_blk_bytes_total{{idx=\"{}\",disk_id=\"0\",op=\"read\"}} {}",
            idx,
            BLK_BYTES_TOTAL.read(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "bhx_blk_bytes_total{{idx=\"{}\",disk_id=\"0\",op=\"write\"}} {}",
            idx,
            BLK_BYTES_TOTAL.write(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP bhx_blk_errors_total Block-request errors per L2CPU per reason \
         (ioerr=overflowed image size, unsupp=unknown request type)."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_blk_errors_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "bhx_blk_errors_total{{idx=\"{}\",disk_id=\"0\",reason=\"ioerr\"}} {}",
            idx,
            BLK_ERRORS_TOTAL.ioerr(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "bhx_blk_errors_total{{idx=\"{}\",disk_id=\"0\",reason=\"unsupp\"}} {}",
            idx,
            BLK_ERRORS_TOTAL.unsupp(idx).get()
        );
    }

    // ----- Per virtio-net -----

    let _ = writeln!(
        &mut out,
        "# HELP bhx_net_packets_total Net packets per L2CPU per direction \
         (rx=slirp-to-guest, tx=guest-to-slirp)."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_net_packets_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "bhx_net_packets_total{{idx=\"{}\",direction=\"rx\"}} {}",
            idx,
            NET_PACKETS_TOTAL.rx(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "bhx_net_packets_total{{idx=\"{}\",direction=\"tx\"}} {}",
            idx,
            NET_PACKETS_TOTAL.tx(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP bhx_net_bytes_total Net bytes per L2CPU per direction."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_net_bytes_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "bhx_net_bytes_total{{idx=\"{}\",direction=\"rx\"}} {}",
            idx,
            NET_BYTES_TOTAL.rx(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "bhx_net_bytes_total{{idx=\"{}\",direction=\"tx\"}} {}",
            idx,
            NET_BYTES_TOTAL.tx(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP bhx_blk_interrupts_total Block-device PLIC interrupts per L2CPU."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_blk_interrupts_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "bhx_blk_interrupts_total{{idx=\"{}\",disk_id=\"0\"}} {}",
            idx,
            BLK_INTERRUPTS_TOTAL.at(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP bhx_net_interrupts_total Net-device PLIC interrupts per L2CPU."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_net_interrupts_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "bhx_net_interrupts_total{{idx=\"{}\"}} {}",
            idx,
            NET_INTERRUPTS_TOTAL.at(idx).get()
        );
    }

    // ----- Worker poll-loop tiers -----

    let _ = writeln!(
        &mut out,
        "# HELP bhx_worker_poll_iterations_total Adaptive-sleep loop iterations \
         per worker per L2CPU per tier (fast=µs, slow=ms, idle=10ms — see #27)."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_worker_poll_iterations_total counter");
    for &w in WorkerKind::all() {
        for idx in 0..4u8 {
            for &t in Tier::all() {
                let _ = writeln!(
                    &mut out,
                    "bhx_worker_poll_iterations_total{{worker=\"{}\",idx=\"{}\",tier=\"{}\"}} {}",
                    w.name(),
                    idx,
                    t.name(),
                    WORKER_POLL_ITERATIONS_TOTAL.at(w, idx, t).get()
                );
            }
        }
    }

    let _ = writeln!(
        &mut out,
        "# HELP bhx_worker_tier_seconds_total Cumulative seconds spent sleeping \
         in each tier per worker per L2CPU."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_worker_tier_seconds_total counter");
    for &w in WorkerKind::all() {
        for idx in 0..4u8 {
            for &t in Tier::all() {
                // Internal counter holds nanoseconds for cheap atomic
                // adds; convert to seconds for the rendered metric.
                let nanos = WORKER_TIER_NANOS_TOTAL.at(w, idx, t).get();
                let secs = nanos as f64 / 1_000_000_000.0;
                let _ = writeln!(
                    &mut out,
                    "bhx_worker_tier_seconds_total{{worker=\"{}\",idx=\"{}\",tier=\"{}\"}} {}",
                    w.name(),
                    idx,
                    t.name(),
                    secs
                );
            }
        }
    }

    // Slot-derived gauges: uptime, disks, net, state. Walk the
    // mutexes once to read every slot's snapshot.
    let _ = writeln!(
        &mut out,
        "# HELP bhx_l2cpu_uptime_seconds Seconds since slot installation. \
         Absent for L2CPUs without an installed slot."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_l2cpu_uptime_seconds gauge");
    let _ = writeln!(
        &mut out,
        "# HELP bhx_l2cpu_disks Attached disk-worker count per L2CPU."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_l2cpu_disks gauge");
    let _ = writeln!(
        &mut out,
        "# HELP bhx_l2cpu_net Net-worker presence per L2CPU (0 or 1)."
    );
    let _ = writeln!(&mut out, "# TYPE bhx_l2cpu_net gauge");
    for idx in 0..4u8 {
        let g = state.l2cpus[idx as usize].lock().unwrap();
        if let Some(slot) = g.as_ref() {
            let uptime = slot.started.elapsed().as_secs() as i64;
            let _ = writeln!(
                &mut out,
                "bhx_l2cpu_uptime_seconds{{idx=\"{}\"}} {}",
                idx, uptime
            );
            let _ = writeln!(
                &mut out,
                "bhx_l2cpu_disks{{idx=\"{}\"}} {}",
                idx,
                slot.disks.len()
            );
            let _ = writeln!(
                &mut out,
                "bhx_l2cpu_net{{idx=\"{}\"}} {}",
                idx,
                slot.net.is_some() as u8
            );
        } else {
            // Emit explicit zero for disks/net so absence is visible
            // without an "is the slot installed?" lookup. Skip uptime
            // — emitting 0 would alias to "just-installed" which is
            // misleading in a tail of recently-stopped slots.
            let _ = writeln!(&mut out, "bhx_l2cpu_disks{{idx=\"{}\"}} 0", idx);
            let _ = writeln!(&mut out, "bhx_l2cpu_net{{idx=\"{}\"}} 0", idx);
        }
    }

    out
}

fn write_counter(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {} {}", name, help);
    let _ = writeln!(out, "# TYPE {} counter", name);
    let _ = writeln!(out, "{} {}", name, value);
}

fn write_gauge(out: &mut String, name: &str, help: &str, value: i64) {
    let _ = writeln!(out, "# HELP {} {}", name, help);
    let _ = writeln!(out, "# TYPE {} gauge", name);
    let _ = writeln!(out, "{} {}", name, value);
}

// ============================================================================
// HTTP exporter
// ============================================================================

/// Bind the HTTP listener and spawn the accept thread. Returns the
/// actually-bound port — `port=0` lets the kernel pick a free one,
/// which the integration test relies on. Bind failure is fatal and
/// propagates so the daemon refuses to start (mirrors the
/// sandbox-install behavior). The thread runs until `state.shutdown`
/// flips.
pub fn spawn_exporter(port: u16, state: Arc<DaemonState>) -> io::Result<u16> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    let bound_port = listener.local_addr()?.port();
    dlog!(
        "[metrics] exporter listening on http://127.0.0.1:{}/metrics",
        bound_port
    );

    thread::spawn(move || {
        run_exporter(listener, state);
    });
    Ok(bound_port)
}

fn run_exporter(listener: TcpListener, state: Arc<DaemonState>) {
    while !state.shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _addr)) => {
                handle_request(stream, &state);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Idle; sleep a bit so we're not pinning a core. 100 ms
                // is fine — Prometheus scrapes are seconds apart.
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                dlog!("[metrics] accept error: {}", e);
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
    dlog!("[metrics] exporter thread exiting");
}

fn handle_request(mut stream: TcpStream, state: &Arc<DaemonState>) {
    // Bound the per-request work: short timeouts so a slow / hung
    // client can't block the accept loop.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    // Read the request line + headers in one go. 1 KiB is plenty —
    // we only care about the first line, but headers may follow.
    let mut buf = [0u8; 1024];
    let n = match stream.read(&mut buf) {
        Ok(0) => return,
        Ok(n) => n,
        Err(_) => return,
    };
    let request_line = std::str::from_utf8(&buf[..n])
        .ok()
        .and_then(|s| s.lines().next())
        .unwrap_or("");

    let response = if request_line.starts_with("GET /metrics") {
        let body = render_prometheus(state);
        format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        )
    } else {
        let body = "404 Not Found\n";
        format!(
            "HTTP/1.1 404 Not Found\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        )
    };

    let _ = stream.write_all(response.as_bytes());
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::shared_chip::SharedChip;

    /// Process-global mutex acquired by tests that touch the shared
    /// metric statics (DAEMON_RPC_TOTAL etc.). Holding it serializes
    /// those tests against each other under cargo's default-parallel
    /// runner — without it, two tests racing on the same counter
    /// have to use snapshot-before/after assertions instead of
    /// absolute values, which is fragile to expand.
    ///
    /// Re-exposed as a guard returned from `metrics_test_lock()` so
    /// callers don't have to spell the static. A poisoned lock from
    /// a panicking earlier test is recovered into a healthy guard
    /// (the metric statics aren't structurally corruptable; they're
    /// just integers).
    fn metrics_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
        M.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Validate `text` against the Prometheus 0.0.4 text-format spec
    /// — the actual rules, not just our internal HELP/TYPE/value
    /// shape. Returns `Ok(())` if every metric name, label name,
    /// and label value is well-formed; `Err(reason)` otherwise.
    /// Pulled out as a helper so the same validator runs against
    /// `render_prometheus` output and (eventually) against any new
    /// metric we wire up.
    fn validate_prometheus_text(text: &str) -> Result<(), String> {
        // Metric names: [a-zA-Z_:][a-zA-Z0-9_:]*. Label names:
        // [a-zA-Z_][a-zA-Z0-9_]*. Label values: any UTF-8 with `\`,
        // `"`, and newline escaped.
        let valid_metric_name = |s: &str| -> bool {
            let mut chars = s.chars();
            match chars.next() {
                Some(c) if c.is_ascii_alphabetic() || c == '_' || c == ':' => {}
                _ => return false,
            }
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
        };
        let valid_label_name = |s: &str| -> bool {
            let mut chars = s.chars();
            match chars.next() {
                Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
                _ => return false,
            }
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        };
        let valid_label_value = |s: &str| -> bool {
            // Bare newlines and unescaped backslashes/quotes are
            // illegal. Our renderer doesn't emit them, but a future
            // change that interpolates user input could.
            let mut iter = s.chars().peekable();
            while let Some(c) = iter.next() {
                if c == '\n' {
                    return false;
                }
                if c == '\\' {
                    match iter.next() {
                        Some('\\') | Some('"') | Some('n') => {}
                        _ => return false,
                    }
                } else if c == '"' {
                    return false; // unescaped quote inside the value
                }
            }
            true
        };

        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // value line: <name>[{<labels>}] <value>
            let space = line.find(' ').ok_or_else(|| {
                format!("line {}: missing value separator: {:?}", lineno + 1, line)
            })?;
            let (name_with_labels, _value) = line.split_at(space);

            let (name, labels) = if let Some(brace) = name_with_labels.find('{') {
                if !name_with_labels.ends_with('}') {
                    return Err(format!("line {}: unclosed label brace", lineno + 1));
                }
                (
                    &name_with_labels[..brace],
                    Some(&name_with_labels[brace + 1..name_with_labels.len() - 1]),
                )
            } else {
                (name_with_labels, None)
            };

            if !valid_metric_name(name) {
                return Err(format!(
                    "line {}: invalid metric name {:?}",
                    lineno + 1,
                    name
                ));
            }

            if let Some(labels) = labels {
                // Each label: <name>="<value>", comma-separated. We
                // can't naively split on `,` because a label value
                // could contain a `,` (escaped or not). The renderer
                // we ship doesn't put commas in values, so split-on-
                // comma is safe here — but assert no unescaped comma
                // in the value half of each pair as we go.
                for pair in labels.split(',') {
                    let eq = pair
                        .find('=')
                        .ok_or_else(|| format!("line {}: label missing '='", lineno + 1))?;
                    let (lname, rest) = pair.split_at(eq);
                    let value_part = &rest[1..]; // skip the '='
                    if !valid_label_name(lname) {
                        return Err(format!(
                            "line {}: invalid label name {:?}",
                            lineno + 1,
                            lname
                        ));
                    }
                    if !value_part.starts_with('"') || !value_part.ends_with('"') {
                        return Err(format!(
                            "line {}: label value not quoted: {:?}",
                            lineno + 1,
                            value_part
                        ));
                    }
                    let inner = &value_part[1..value_part.len() - 1];
                    if !valid_label_value(inner) {
                        return Err(format!(
                            "line {}: invalid label value escape: {:?}",
                            lineno + 1,
                            inner
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn render_prometheus_emits_expected_metric_names() {
        // Don't assert exact counter values against shared statics —
        // other tests in this module touch the same globals and run in
        // parallel. Just confirm the labelled inventory is present.
        let state = DaemonState::new(0, Arc::new(SharedChip::placeholder()));
        let out = render_prometheus(&state);

        // Daemon-global.
        for needle in [
            "# HELP bhx_daemon_uptime_seconds",
            "# TYPE bhx_daemon_uptime_seconds gauge",
            "\nbhx_daemon_uptime_seconds ",
            "# HELP bhx_daemon_clients_total",
            "# TYPE bhx_daemon_clients_total counter",
            "# HELP bhx_daemon_clients_active",
            "# TYPE bhx_daemon_clients_active gauge",
            "# HELP bhx_daemon_sandbox_status",
            "# TYPE bhx_daemon_sandbox_status gauge",
            "# HELP bhx_daemon_rpc_total",
            "# TYPE bhx_daemon_rpc_total counter",
            "bhx_daemon_rpc_total{method=\"boot\"} ",
            "bhx_daemon_rpc_total{method=\"add_disk\"} ",
            "bhx_daemon_rpc_errors_total{method=\"boot\"} ",
            // Per-L2CPU (every idx 0..3 should appear).
            "bhx_l2cpu_boot_total{idx=\"0\",kind=\"cold\"} ",
            "bhx_l2cpu_boot_total{idx=\"3\",kind=\"warm\"} ",
            "bhx_l2cpu_console_clients{idx=\"2\"} ",
            "bhx_l2cpu_console_bytes_total{idx=\"0\",direction=\"g2h\"} ",
            "bhx_l2cpu_console_bytes_total{idx=\"3\",direction=\"h2g\"} ",
            "bhx_l2cpu_disks{idx=\"0\"} ",
            "bhx_l2cpu_net{idx=\"3\"} ",
            // Per virtio-block (disk_id pinned at 0 in Phase A).
            "bhx_blk_requests_total{idx=\"0\",disk_id=\"0\",op=\"read\"} ",
            "bhx_blk_requests_total{idx=\"2\",disk_id=\"0\",op=\"write\"} ",
            "bhx_blk_bytes_total{idx=\"3\",disk_id=\"0\",op=\"read\"} ",
            "bhx_blk_errors_total{idx=\"0\",disk_id=\"0\",reason=\"ioerr\"} ",
            "bhx_blk_errors_total{idx=\"1\",disk_id=\"0\",reason=\"unsupp\"} ",
            // Per virtio-net.
            "bhx_net_packets_total{idx=\"0\",direction=\"rx\"} ",
            "bhx_net_packets_total{idx=\"3\",direction=\"tx\"} ",
            "bhx_net_bytes_total{idx=\"2\",direction=\"rx\"} ",
            "bhx_blk_interrupts_total{idx=\"0\",disk_id=\"0\"} ",
            "bhx_net_interrupts_total{idx=\"3\"} ",
            // Worker poll-tier (every (worker, idx, tier) combination
            // gets a line; spot-check a representative subset).
            "bhx_worker_poll_iterations_total{worker=\"virtio_blk\",idx=\"0\",tier=\"fast\"} ",
            "bhx_worker_poll_iterations_total{worker=\"chip_console\",idx=\"3\",tier=\"idle\"} ",
            "bhx_worker_tier_seconds_total{worker=\"virtio_net\",idx=\"2\",tier=\"slow\"} ",
        ] {
            assert!(
                out.contains(needle),
                "rendered output missing {:?}; first 200 chars:\n{}",
                needle,
                &out.chars().take(200).collect::<String>()
            );
        }
    }

    #[test]
    fn render_prometheus_format_is_well_formed() {
        // Sanity-check the structural invariants Prometheus demands:
        // every value line has `name [labels] value` (2 whitespace
        // tokens), and HELP appears at most once per metric name. With
        // labelled metrics, multiple value lines share one HELP/TYPE
        // pair, so the simple equality count we used pre-#31 no
        // longer holds — instead require value_count >= help_count
        // and HELP/TYPE balance.
        let state = DaemonState::new(0, Arc::new(SharedChip::placeholder()));
        let out = render_prometheus(&state);

        let mut help_count = 0;
        let mut type_count = 0;
        let mut value_count = 0;
        for line in out.lines() {
            if line.starts_with("# HELP ") {
                help_count += 1;
            } else if line.starts_with("# TYPE ") {
                type_count += 1;
            } else if !line.is_empty() {
                value_count += 1;
                assert_eq!(
                    line.split_whitespace().count(),
                    2,
                    "metric line should be `name value`: {:?}",
                    line
                );
            }
        }
        assert_eq!(help_count, type_count, "every HELP needs a matching TYPE");
        assert!(
            value_count >= help_count,
            "expected ≥ help_count value lines (got {} value, {} help)",
            value_count,
            help_count
        );
        assert!(
            help_count >= 8,
            "expected at least 8 metric names, got {help_count}"
        );
    }

    #[test]
    fn per_l2cpu_dir_counter_separates_directions() {
        let m = PerL2cpuDirCounter::new();
        m.g2h(0).add(100);
        m.h2g(0).add(200);
        m.g2h(1).add(50);
        assert_eq!(m.g2h(0).get(), 100);
        assert_eq!(m.h2g(0).get(), 200);
        assert_eq!(m.g2h(1).get(), 50);
        assert_eq!(m.h2g(1).get(), 0);
        assert_eq!(m.g2h(2).get(), 0);
    }

    #[test]
    fn rpc_method_counter_dispatches_per_method() {
        let m = RpcMethodCounter::new();
        m.at(RpcMethod::Boot).add(3);
        m.at(RpcMethod::AddDisk).inc();
        m.at(RpcMethod::AddDisk).inc();
        assert_eq!(m.at(RpcMethod::Boot).get(), 3);
        assert_eq!(m.at(RpcMethod::AddDisk).get(), 2);
        assert_eq!(m.at(RpcMethod::Status).get(), 0);
    }

    #[test]
    fn classify_tier_picks_correct_bucket() {
        let fast = Duration::from_millis(200);
        let idle = Duration::from_secs(2);

        // Below fast_window → Fast.
        assert_eq!(
            classify_tier(Duration::from_millis(0), fast, idle),
            Tier::Fast
        );
        assert_eq!(
            classify_tier(Duration::from_millis(199), fast, idle),
            Tier::Fast
        );
        // At/above fast_window but below idle_window → Slow.
        assert_eq!(
            classify_tier(Duration::from_millis(200), fast, idle),
            Tier::Slow
        );
        assert_eq!(
            classify_tier(Duration::from_millis(1999), fast, idle),
            Tier::Slow
        );
        // At/above idle_window → Idle.
        assert_eq!(
            classify_tier(Duration::from_secs(2), fast, idle),
            Tier::Idle
        );
        assert_eq!(
            classify_tier(Duration::from_secs(60), fast, idle),
            Tier::Idle
        );
    }

    #[test]
    fn worker_tier_counter_indexes_independently() {
        let m = WorkerTierCounter::new();
        m.at(WorkerKind::VirtioBlk, 0, Tier::Fast).add(10);
        m.at(WorkerKind::VirtioNet, 1, Tier::Slow).add(5);
        m.at(WorkerKind::ChipConsole, 3, Tier::Idle).inc();

        assert_eq!(m.at(WorkerKind::VirtioBlk, 0, Tier::Fast).get(), 10);
        assert_eq!(m.at(WorkerKind::VirtioNet, 1, Tier::Slow).get(), 5);
        assert_eq!(m.at(WorkerKind::ChipConsole, 3, Tier::Idle).get(), 1);
        // No bleed across (worker, idx, tier) axes.
        assert_eq!(m.at(WorkerKind::VirtioBlk, 0, Tier::Slow).get(), 0);
        assert_eq!(m.at(WorkerKind::VirtioNet, 0, Tier::Slow).get(), 0);
        assert_eq!(m.at(WorkerKind::ChipConsole, 3, Tier::Fast).get(), 0);
    }

    #[test]
    fn worker_kind_and_tier_names_are_stable() {
        // Wire labels are consumed by external dashboards; a typo
        // here would break alerts at deploy time.
        for &(w, expected) in &[
            (WorkerKind::VirtioBlk, "virtio_blk"),
            (WorkerKind::VirtioNet, "virtio_net"),
            (WorkerKind::ChipConsole, "chip_console"),
        ] {
            assert_eq!(w.name(), expected);
        }
        for &(t, expected) in &[
            (Tier::Fast, "fast"),
            (Tier::Slow, "slow"),
            (Tier::Idle, "idle"),
        ] {
            assert_eq!(t.name(), expected);
        }
    }

    #[test]
    fn per_l2cpu_blk_op_separates_read_and_write() {
        let m = PerL2cpuBlkOpCounter::new();
        m.read(0).add(100);
        m.write(0).add(200);
        m.read(3).add(50);
        assert_eq!(m.read(0).get(), 100);
        assert_eq!(m.write(0).get(), 200);
        assert_eq!(m.read(3).get(), 50);
        assert_eq!(m.write(3).get(), 0);
    }

    #[test]
    fn per_l2cpu_blk_errors_separates_ioerr_and_unsupp() {
        let m = PerL2cpuBlkErrors::new();
        m.ioerr(1).inc();
        m.ioerr(1).inc();
        m.unsupp(2).inc();
        assert_eq!(m.ioerr(0).get(), 0);
        assert_eq!(m.ioerr(1).get(), 2);
        assert_eq!(m.unsupp(2).get(), 1);
        assert_eq!(m.unsupp(1).get(), 0);
    }

    #[test]
    fn per_l2cpu_net_dir_separates_rx_and_tx() {
        let m = PerL2cpuNetDirCounter::new();
        m.rx(0).add(1500);
        m.tx(0).add(64);
        m.rx(2).add(1500);
        assert_eq!(m.rx(0).get(), 1500);
        assert_eq!(m.tx(0).get(), 64);
        assert_eq!(m.rx(2).get(), 1500);
        assert_eq!(m.tx(2).get(), 0);
    }

    /// Drive an actual TCP request through `spawn_exporter` so the
    /// HTTP layer is covered (request parsing, response shape,
    /// content-length matching, connection-close semantics). Without
    /// this, the only thing exercising `handle_request` is the
    /// hardware soak — which doesn't run in CI.
    #[test]
    fn http_exporter_serves_metrics_and_404s() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let state = Arc::new(DaemonState::new(0, Arc::new(SharedChip::placeholder())));
        // Port 0 = kernel picks free; spawn_exporter returns the
        // actually-bound port so we know where to connect.
        let port = spawn_exporter(0, state.clone()).expect("bind on port 0");

        // Helper: open a fresh connection (the exporter writes
        // Connection: close so each request is its own TCP) and read
        // the full response.
        let request = |path: &str| -> (u16, String) {
            let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
                .unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).unwrap();
            let text = String::from_utf8_lossy(&buf).into_owned();
            // Parse the status line: `HTTP/1.1 <code> <reason>`.
            let status: u16 = text
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            (status, text)
        };

        // /metrics → 200 + valid prom text.
        let (code, body) = request("/metrics");
        assert_eq!(code, 200, "got status {code}; body:\n{body}");
        assert!(
            body.contains("\r\n\r\nbhx_daemon_uptime_seconds")
                || body.contains("\r\n\r\n# HELP bhx_daemon_uptime_seconds"),
            "metrics body missing daemon uptime line; body:\n{body}"
        );
        // Content-Type matches the Prometheus text-format spec.
        assert!(
            body.contains("Content-Type: text/plain"),
            "missing or wrong Content-Type; body:\n{body}"
        );

        // /healthz → 404 (we only serve /metrics).
        let (code, _) = request("/healthz");
        assert_eq!(code, 404);

        // /metrics with extra query string still hits the route —
        // we use prefix-match `starts_with("GET /metrics")`.
        let (code, _) = request("/metrics?foo=bar");
        assert_eq!(code, 200);

        // Tell the exporter thread to stop. There's no join handle
        // (the spawn is fire-and-forget so callers don't have to
        // hold one), but flipping shutdown leaves the thread to
        // exit on its own next poll cycle (≤100 ms).
        state.shutdown.store(true, Ordering::Relaxed);
    }

    /// Adversarial input handling for the HTTP exporter (#33). The
    /// listener uses a fixed 1024-byte read buffer and only inspects
    /// the first line of the request — which is fine, but easy to
    /// break with a refactor. Test the three plausible regressions:
    /// oversize requests, malformed first lines, and pipelined
    /// follow-on requests on the same connection.
    #[test]
    fn http_exporter_handles_adversarial_inputs() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::time::Duration;

        let state = Arc::new(DaemonState::new(0, Arc::new(SharedChip::placeholder())));
        let port = spawn_exporter(0, state.clone()).expect("bind on port 0");

        let do_request = |raw_request: &[u8]| -> Vec<u8> {
            let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            s.write_all(raw_request).unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).unwrap();
            buf
        };

        // Oversize request: 5 KiB of `X` headers BEFORE the request
        // line gets the request line truncated, so it doesn't start
        // with "GET /metrics" — should still produce a clean 404
        // (or 400-class). Today's impl returns 404 because the
        // first line of garbage doesn't match. The point is no
        // panic, no hang, no leak.
        let big_garbage: Vec<u8> = std::iter::repeat_n(b'X', 5000).collect();
        let resp = do_request(&big_garbage);
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.starts_with("HTTP/1.1 404"),
            "expected 404 on oversize garbage, got:\n{}",
            text
        );

        // Non-UTF-8 bytes in the request line. `from_utf8_lossy`
        // replaces invalid sequences with U+FFFD, so the literal
        // doesn't match `GET /metrics` — should be 404.
        let non_utf8 = b"\xff\xfe\xc3\x28GET /metrics HTTP/1.1\r\n\r\n";
        let resp = do_request(non_utf8);
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.starts_with("HTTP/1.1 404"),
            "expected 404 on non-UTF8 prefix, got:\n{}",
            text
        );

        // Pipelined request: client sends two GETs in one write.
        // Our impl uses Connection: close so the second is ignored —
        // first should still get a clean 200 + the connection
        // closes. The exporter must NOT hang waiting for the second
        // body or panic on the leftover bytes.
        let pipelined =
            b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\nGET /metrics HTTP/1.1\r\nHost: x\r\n\r\n";
        let resp = do_request(pipelined);
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.starts_with("HTTP/1.1 200"),
            "pipelined first-request should still 200, got:\n{}",
            text
        );

        // Malformed request: just a CRLF, no method/URL.
        let resp = do_request(b"\r\n\r\n");
        let text = String::from_utf8_lossy(&resp);
        assert!(
            text.starts_with("HTTP/1.1 404"),
            "expected 404 on empty request line, got:\n{}",
            text
        );

        state.shutdown.store(true, Ordering::Relaxed);
    }

    /// Spec-compliance check (#33). Run our renderer's output past
    /// a hand-rolled validator covering the actual Prometheus
    /// 0.0.4 grammar — metric-name charset, label-name charset,
    /// label-value escape rules. Catches a regression where someone
    /// emits a `:` in a label name or an unescaped `"` in a value.
    /// Faster + more portable than wiring `promtool check metrics`
    /// into CI (which would require the prometheus binary on the
    /// runner).
    #[test]
    fn render_prometheus_passes_spec_validator() {
        let _g = metrics_test_lock();
        let state = DaemonState::new(0, Arc::new(SharedChip::placeholder()));
        let out = render_prometheus(&state);
        if let Err(reason) = validate_prometheus_text(&out) {
            panic!(
                "render_prometheus output failed spec validation: {}\nfull output:\n{}",
                reason, out
            );
        }
    }

    /// Negative tests for the validator — confirms it rejects the
    /// classes of malformed input it's meant to catch. Without these
    /// the spec test above could pass against a no-op validator and
    /// nobody would notice.
    #[test]
    fn validator_rejects_malformed_inputs() {
        // Invalid metric name (digit prefix).
        assert!(validate_prometheus_text("1bad_name 5\n").is_err());
        // Invalid label name (digit prefix).
        assert!(validate_prometheus_text("foo{1bad=\"x\"} 5\n").is_err());
        // Unescaped quote inside label value.
        assert!(validate_prometheus_text("foo{x=\"a\"b\"} 5\n").is_err());
        // Bare newline inside label value.
        assert!(validate_prometheus_text("foo{x=\"a\nb\"} 5\n").is_err());
        // Unclosed brace.
        assert!(validate_prometheus_text("foo{x=\"y\" 5\n").is_err());
        // No value (missing space).
        assert!(validate_prometheus_text("foo\n").is_err());

        // And confirm the validator accepts the canonical happy paths.
        assert!(validate_prometheus_text("foo 42\n").is_ok());
        assert!(validate_prometheus_text("foo_bar:baz 42\n").is_ok());
        assert!(validate_prometheus_text("foo{a=\"1\",b=\"two\"} 42\n").is_ok());
        assert!(validate_prometheus_text("# HELP foo blah\n").is_ok());
        assert!(validate_prometheus_text("# TYPE foo counter\n").is_ok());
        // Escaped quote / backslash / newline are valid in values.
        assert!(validate_prometheus_text("foo{x=\"a\\\"b\"} 42\n").is_ok());
        assert!(validate_prometheus_text("foo{x=\"a\\\\b\"} 42\n").is_ok());
        assert!(validate_prometheus_text("foo{x=\"a\\nb\"} 42\n").is_ok());
    }

    #[test]
    fn rpc_method_names_cover_every_variant() {
        // Names are stable wire labels — operators build dashboards
        // off them. A typo here would silently break a dashboard at
        // some point in the future.
        let names: Vec<_> = RpcMethod::all().iter().map(|m| m.name()).collect();
        for expected in [
            "status",
            "boot",
            "attach_console",
            "add_disk",
            "remove_disk",
            "add_net",
            "remove_net",
            "stop",
            "shutdown",
        ] {
            assert!(
                names.contains(&expected),
                "expected method name {:?} in {:?}",
                expected,
                names
            );
        }
    }
}
