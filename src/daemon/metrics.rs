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
/// label tuples we actually have.
pub struct CounterVec<const N: usize> {
    values: [Counter; N],
}

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
pub struct GaugeVec<const N: usize> {
    values: [Gauge; N],
}

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

/// One counter per RPC method. Drives `tt_bh_daemon_rpc_total{method}`
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
        "tt_bh_daemon_uptime_seconds",
        "Daemon uptime in seconds.",
        state.started.elapsed().as_secs() as i64,
    );
    write_counter(
        &mut out,
        "tt_bh_daemon_clients_total",
        "Cumulative count of accepted RPC client connections.",
        DAEMON_CLIENTS_TOTAL.get(),
    );
    write_gauge(
        &mut out,
        "tt_bh_daemon_clients_active",
        "Currently-connected RPC clients.",
        DAEMON_CLIENTS_ACTIVE.get(),
    );
    write_gauge(
        &mut out,
        "tt_bh_daemon_sandbox_status",
        "Sandbox enforcement: 0=disabled, 1=partial, 2=fully-enforced.",
        DAEMON_SANDBOX_STATUS.get(),
    );

    // Per-method RPC totals. One emit pass per metric so HELP/TYPE
    // appear once at the top per Prometheus convention.
    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_daemon_rpc_total Cumulative RPC count per method."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_daemon_rpc_total counter");
    for &m in RpcMethod::all() {
        let _ = writeln!(
            &mut out,
            "tt_bh_daemon_rpc_total{{method=\"{}\"}} {}",
            m.name(),
            DAEMON_RPC_TOTAL.at(m).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_daemon_rpc_errors_total RPC failures per method."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_daemon_rpc_errors_total counter");
    for &m in RpcMethod::all() {
        let _ = writeln!(
            &mut out,
            "tt_bh_daemon_rpc_errors_total{{method=\"{}\"}} {}",
            m.name(),
            DAEMON_RPC_ERRORS_TOTAL.at(m).get()
        );
    }

    // ----- Per-L2CPU -----

    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_l2cpu_boot_total Boot count per L2CPU, by kind (cold|warm)."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_l2cpu_boot_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "tt_bh_l2cpu_boot_total{{idx=\"{}\",kind=\"cold\"}} {}",
            idx,
            L2CPU_BOOT_COLD_TOTAL.at(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "tt_bh_l2cpu_boot_total{{idx=\"{}\",kind=\"warm\"}} {}",
            idx,
            L2CPU_BOOT_WARM_TOTAL.at(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_l2cpu_console_clients Currently-attached console clients per L2CPU."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_l2cpu_console_clients gauge");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "tt_bh_l2cpu_console_clients{{idx=\"{}\"}} {}",
            idx,
            L2CPU_CONSOLE_CLIENTS.at(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_l2cpu_console_bytes_total Chip-console byte transfers per L2CPU \
         per direction (g2h = guest-to-host, h2g = host-to-guest)."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_l2cpu_console_bytes_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "tt_bh_l2cpu_console_bytes_total{{idx=\"{}\",direction=\"g2h\"}} {}",
            idx,
            L2CPU_CONSOLE_BYTES_TOTAL.g2h(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "tt_bh_l2cpu_console_bytes_total{{idx=\"{}\",direction=\"h2g\"}} {}",
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
        "# HELP tt_bh_blk_requests_total Block requests completed per L2CPU per op."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_blk_requests_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "tt_bh_blk_requests_total{{idx=\"{}\",disk_id=\"0\",op=\"read\"}} {}",
            idx,
            BLK_REQUESTS_TOTAL.read(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "tt_bh_blk_requests_total{{idx=\"{}\",disk_id=\"0\",op=\"write\"}} {}",
            idx,
            BLK_REQUESTS_TOTAL.write(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_blk_bytes_total Block bytes transferred per L2CPU per op."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_blk_bytes_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "tt_bh_blk_bytes_total{{idx=\"{}\",disk_id=\"0\",op=\"read\"}} {}",
            idx,
            BLK_BYTES_TOTAL.read(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "tt_bh_blk_bytes_total{{idx=\"{}\",disk_id=\"0\",op=\"write\"}} {}",
            idx,
            BLK_BYTES_TOTAL.write(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_blk_errors_total Block-request errors per L2CPU per reason \
         (ioerr=overflowed image size, unsupp=unknown request type)."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_blk_errors_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "tt_bh_blk_errors_total{{idx=\"{}\",disk_id=\"0\",reason=\"ioerr\"}} {}",
            idx,
            BLK_ERRORS_TOTAL.ioerr(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "tt_bh_blk_errors_total{{idx=\"{}\",disk_id=\"0\",reason=\"unsupp\"}} {}",
            idx,
            BLK_ERRORS_TOTAL.unsupp(idx).get()
        );
    }

    // ----- Per virtio-net -----

    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_net_packets_total Net packets per L2CPU per direction \
         (rx=slirp-to-guest, tx=guest-to-slirp)."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_net_packets_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "tt_bh_net_packets_total{{idx=\"{}\",direction=\"rx\"}} {}",
            idx,
            NET_PACKETS_TOTAL.rx(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "tt_bh_net_packets_total{{idx=\"{}\",direction=\"tx\"}} {}",
            idx,
            NET_PACKETS_TOTAL.tx(idx).get()
        );
    }

    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_net_bytes_total Net bytes per L2CPU per direction."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_net_bytes_total counter");
    for idx in 0..4u8 {
        let _ = writeln!(
            &mut out,
            "tt_bh_net_bytes_total{{idx=\"{}\",direction=\"rx\"}} {}",
            idx,
            NET_BYTES_TOTAL.rx(idx).get()
        );
        let _ = writeln!(
            &mut out,
            "tt_bh_net_bytes_total{{idx=\"{}\",direction=\"tx\"}} {}",
            idx,
            NET_BYTES_TOTAL.tx(idx).get()
        );
    }

    // Slot-derived gauges: uptime, disks, net, state. Walk the
    // mutexes once to read every slot's snapshot.
    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_l2cpu_uptime_seconds Seconds since slot installation. \
         Absent for L2CPUs without an installed slot."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_l2cpu_uptime_seconds gauge");
    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_l2cpu_disks Attached disk-worker count per L2CPU."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_l2cpu_disks gauge");
    let _ = writeln!(
        &mut out,
        "# HELP tt_bh_l2cpu_net Net-worker presence per L2CPU (0 or 1)."
    );
    let _ = writeln!(&mut out, "# TYPE tt_bh_l2cpu_net gauge");
    for idx in 0..4u8 {
        let g = state.l2cpus[idx as usize].lock().unwrap();
        if let Some(slot) = g.as_ref() {
            let uptime = slot.started.elapsed().as_secs() as i64;
            let _ = writeln!(
                &mut out,
                "tt_bh_l2cpu_uptime_seconds{{idx=\"{}\"}} {}",
                idx, uptime
            );
            let _ = writeln!(
                &mut out,
                "tt_bh_l2cpu_disks{{idx=\"{}\"}} {}",
                idx,
                slot.disks.len()
            );
            let _ = writeln!(
                &mut out,
                "tt_bh_l2cpu_net{{idx=\"{}\"}} {}",
                idx,
                slot.net.is_some() as u8
            );
        } else {
            // Emit explicit zero for disks/net so absence is visible
            // without an "is the slot installed?" lookup. Skip uptime
            // — emitting 0 would alias to "just-installed" which is
            // misleading in a tail of recently-stopped slots.
            let _ = writeln!(&mut out, "tt_bh_l2cpu_disks{{idx=\"{}\"}} 0", idx);
            let _ = writeln!(&mut out, "tt_bh_l2cpu_net{{idx=\"{}\"}} 0", idx);
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

    #[test]
    fn counter_inc_and_add() {
        let c = Counter::new();
        assert_eq!(c.get(), 0);
        c.inc();
        c.inc();
        c.add(5);
        assert_eq!(c.get(), 7);
    }

    #[test]
    fn gauge_set_inc_dec() {
        let g = Gauge::new();
        assert_eq!(g.get(), 0);
        g.inc();
        g.inc();
        g.inc();
        g.dec();
        assert_eq!(g.get(), 2);
        g.set(-10);
        assert_eq!(g.get(), -10);
    }

    #[test]
    fn counter_vec_indexed_access() {
        let cv: CounterVec<4> = CounterVec::new();
        cv.at(0).inc();
        cv.at(2).add(7);
        cv.at(2).inc();
        assert_eq!(cv.at(0).get(), 1);
        assert_eq!(cv.at(1).get(), 0);
        assert_eq!(cv.at(2).get(), 8);
        assert_eq!(cv.at(3).get(), 0);
    }

    #[test]
    fn gauge_vec_indexed_access() {
        let gv: GaugeVec<4> = GaugeVec::new();
        gv.at(1).set(42);
        gv.at(3).inc();
        assert_eq!(gv.at(0).get(), 0);
        assert_eq!(gv.at(1).get(), 42);
        assert_eq!(gv.at(3).get(), 1);
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
            "# HELP tt_bh_daemon_uptime_seconds",
            "# TYPE tt_bh_daemon_uptime_seconds gauge",
            "\ntt_bh_daemon_uptime_seconds ",
            "# HELP tt_bh_daemon_clients_total",
            "# TYPE tt_bh_daemon_clients_total counter",
            "# HELP tt_bh_daemon_clients_active",
            "# TYPE tt_bh_daemon_clients_active gauge",
            "# HELP tt_bh_daemon_sandbox_status",
            "# TYPE tt_bh_daemon_sandbox_status gauge",
            "# HELP tt_bh_daemon_rpc_total",
            "# TYPE tt_bh_daemon_rpc_total counter",
            "tt_bh_daemon_rpc_total{method=\"boot\"} ",
            "tt_bh_daemon_rpc_total{method=\"add_disk\"} ",
            "tt_bh_daemon_rpc_errors_total{method=\"boot\"} ",
            // Per-L2CPU (every idx 0..3 should appear).
            "tt_bh_l2cpu_boot_total{idx=\"0\",kind=\"cold\"} ",
            "tt_bh_l2cpu_boot_total{idx=\"3\",kind=\"warm\"} ",
            "tt_bh_l2cpu_console_clients{idx=\"2\"} ",
            "tt_bh_l2cpu_console_bytes_total{idx=\"0\",direction=\"g2h\"} ",
            "tt_bh_l2cpu_console_bytes_total{idx=\"3\",direction=\"h2g\"} ",
            "tt_bh_l2cpu_disks{idx=\"0\"} ",
            "tt_bh_l2cpu_net{idx=\"3\"} ",
            // Per virtio-block (disk_id pinned at 0 in Phase A).
            "tt_bh_blk_requests_total{idx=\"0\",disk_id=\"0\",op=\"read\"} ",
            "tt_bh_blk_requests_total{idx=\"2\",disk_id=\"0\",op=\"write\"} ",
            "tt_bh_blk_bytes_total{idx=\"3\",disk_id=\"0\",op=\"read\"} ",
            "tt_bh_blk_errors_total{idx=\"0\",disk_id=\"0\",reason=\"ioerr\"} ",
            "tt_bh_blk_errors_total{idx=\"1\",disk_id=\"0\",reason=\"unsupp\"} ",
            // Per virtio-net.
            "tt_bh_net_packets_total{idx=\"0\",direction=\"rx\"} ",
            "tt_bh_net_packets_total{idx=\"3\",direction=\"tx\"} ",
            "tt_bh_net_bytes_total{idx=\"2\",direction=\"rx\"} ",
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
    fn per_l2cpu_counter_indexed() {
        let m = PerL2cpuCounter::new();
        m.at(0).add(10);
        m.at(2).add(7);
        assert_eq!(m.at(0).get(), 10);
        assert_eq!(m.at(1).get(), 0);
        assert_eq!(m.at(2).get(), 7);
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
            body.contains("\r\n\r\ntt_bh_daemon_uptime_seconds")
                || body.contains("\r\n\r\n# HELP tt_bh_daemon_uptime_seconds"),
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
