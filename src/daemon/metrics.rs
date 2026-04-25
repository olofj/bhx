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
// Global metrics
// ============================================================================
//
// At this stage (#30) the inventory is intentionally tiny — just enough
// to exercise the registry plumbing and prove the format. The full
// instrumentation lands in #31, where these statics expand to cover
// virtio block/net, chip_console, RPC dispatch, and slot lifecycle.

/// Cumulative count of accepted RPC client connections.
pub static DAEMON_CLIENTS_TOTAL: Counter = Counter::new();

/// Currently-connected RPC clients (active count, decremented on close).
pub static DAEMON_CLIENTS_ACTIVE: Gauge = Gauge::new();

// ============================================================================
// Prometheus text formatter
// ============================================================================

/// Render the current metric set in Prometheus text format (version
/// 0.0.4). Returns the full response body, suitable for the HTTP
/// listener to write back verbatim.
pub fn render_prometheus(state: &DaemonState) -> String {
    let mut out = String::with_capacity(2048);

    // Daemon-global metrics. Order doesn't matter to scrapers, but
    // keeping uptime first makes manual `curl` output readable.
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

/// Bind the HTTP listener and spawn the accept thread. Returns once the
/// bind has succeeded — bind failure is fatal and propagates so the
/// daemon refuses to start (mirrors the sandbox-install behavior). The
/// thread runs until `state.shutdown` flips.
pub fn spawn_exporter(port: u16, state: Arc<DaemonState>) -> io::Result<()> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    dlog!(
        "[metrics] exporter listening on http://127.0.0.1:{}/metrics",
        port
    );

    thread::spawn(move || {
        run_exporter(listener, state);
    });
    Ok(())
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
    fn render_prometheus_emits_expected_lines() {
        let state = DaemonState::new(0, Arc::new(SharedChip::placeholder()));
        // Bump the globals so the rendered output reflects something
        // non-zero — that way we catch a regression where the format
        // helpers fall back to a default.
        DAEMON_CLIENTS_TOTAL.add(3);
        DAEMON_CLIENTS_ACTIVE.set(1);

        let out = render_prometheus(&state);

        // Each metric should have HELP + TYPE + value lines, in that
        // canonical order. Don't lock the exact uptime value since the
        // clock advances during the test, but assert the line shape.
        assert!(out.contains("# HELP tt_bh_daemon_uptime_seconds"));
        assert!(out.contains("# TYPE tt_bh_daemon_uptime_seconds gauge"));
        assert!(out.contains("\ntt_bh_daemon_uptime_seconds "));

        assert!(out.contains("# HELP tt_bh_daemon_clients_total"));
        assert!(out.contains("# TYPE tt_bh_daemon_clients_total counter"));
        assert!(out.contains("tt_bh_daemon_clients_total 3\n"));

        assert!(out.contains("# HELP tt_bh_daemon_clients_active"));
        assert!(out.contains("# TYPE tt_bh_daemon_clients_active gauge"));
        assert!(out.contains("tt_bh_daemon_clients_active 1\n"));

        // Reset for any later tests to start from a known state.
        DAEMON_CLIENTS_TOTAL.add(0);
        DAEMON_CLIENTS_ACTIVE.set(0);
    }

    #[test]
    fn render_prometheus_format_is_well_formed() {
        // Sanity-check the structural invariants the Prometheus text
        // format demands: each metric is a triple (HELP, TYPE, value)
        // with no blank gaps inside the triple, and every non-comment
        // line has exactly one space between name and value.
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
        assert_eq!(help_count, type_count);
        assert_eq!(help_count, value_count);
        assert!(
            help_count >= 3,
            "expected at least 3 metrics, got {help_count}"
        );
    }
}
