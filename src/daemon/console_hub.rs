// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Per-L2CPU console fan-out.
//!
//! Chip-side: one thread reads the OpenSBI virtual-UART TX ring and calls
//! [`ConsoleHub::push_chip_output`]. The hub appends into a 64 KiB scrollback
//! ring and fans out to every attached client socket using `send(..,
//! MSG_DONTWAIT)` so the chip reader never stalls on a slow client. A client
//! whose socket would block is dropped (its fd is closed, the socket returns
//! EOF to the far side, and the client's own pump thread quits cleanly).
//!
//! The socket itself is left in blocking mode so the server's per-client
//! reader thread can `read()` without polling. `MSG_DONTWAIT` on writes is
//! per-call and doesn't flip the file-description flag, which matters
//! because the daemon holds two fds to the same socket (one for reads, one
//! for writes via the hub) and `O_NONBLOCK` is shared across `dup`'d fds.
//!
//! Client-side input (keystrokes → chip) is *not* handled here. The server
//! starts a dedicated reader thread per attach that only forwards if the
//! client currently owns the writer role ([`ConsoleHub::current_writer_id`]).

use std::collections::VecDeque;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;

use crate::daemon::protocol::ConsoleMode;

/// Bytes of console scrollback the daemon snapshots into
/// `DaemonState::shutdown_tails` at slot teardown so an operator who
/// runs `bhx connect` *after* the slot is gone can still see the
/// last screenful or two of console output (#160). 16 KiB covers a
/// full 80×24 terminal plus a few prior scroll lines and ANSI/escape
/// overhead — enough to capture a kernel panic banner or `poweroff`
/// sequence. 4 L2CPUs × 16 KiB = 64 KiB resident across the daemon's
/// lifetime; trivial.
pub const SHUTDOWN_TAIL_BYTES: usize = 16 * 1024;

/// Fixed-size per-L2CPU scrollback buffer. Sized to comfortably hold a full
/// stock-distro boot log — Fedora 42's OpenSBI + U-Boot + grub + kernel +
/// systemd output runs ~200 KiB of ANSI-decorated bytes, so 1 MiB leaves
/// headroom for a verbose initramfs without truncating the early-boot
/// banners by the time an operator runs `bhx connect`.
pub const SCROLLBACK_BYTES: usize = 1024 * 1024;

/// A single attached client. The socket is the daemon-side end of the
/// socketpair; the other end was sent to the client via SCM_RIGHTS.
struct Client {
    id: u64,
    sock: UnixStream,
    is_writer: bool,
}

struct HubState {
    scrollback: VecDeque<u8>,
    clients: Vec<Client>,
    next_id: u64,
}

pub struct ConsoleHub {
    /// L2CPU index. Used as a label on the per-hub metric updates;
    /// not otherwise referenced internally.
    idx: u8,
    state: Mutex<HubState>,
}

/// Outcome of an attach request.
pub struct AttachResult {
    pub id: u64,
    /// Whether this client became the writer (Rw / Takeover); the
    /// daemon side reads it via `current_writer_id` on the hub, but
    /// callers who attach occasionally read this snapshot directly.
    /// Tests assert against it; production currently doesn't.
    #[allow(dead_code)]
    pub is_writer: bool,
    pub scrollback_bytes: u32,
    /// Ids of clients that were demoted from writer as a side effect of this
    /// attach (only populated on `Takeover`). The server can send them a
    /// control frame; we don't do it here to keep I/O off the lock.
    pub demoted: Vec<u64>,
}

impl Default for ConsoleHub {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ConsoleHub {
    pub fn new(idx: u8) -> Self {
        ConsoleHub {
            idx,
            state: Mutex::new(HubState {
                scrollback: VecDeque::with_capacity(SCROLLBACK_BYTES),
                clients: Vec::new(),
                next_id: 1,
            }),
        }
    }

    /// Register an attached client. The socket may be in blocking mode —
    /// the hub writes via `send(.., MSG_DONTWAIT)` regardless. Returns the
    /// full scrollback for the client to replay.
    pub fn attach(&self, sock: UnixStream, mode: ConsoleMode) -> (AttachResult, Vec<u8>) {
        let mut s = self.state.lock().unwrap();
        let id = s.next_id;
        s.next_id += 1;

        let current_writer = s.clients.iter().find(|c| c.is_writer).map(|c| c.id);
        let mut demoted = Vec::new();
        let is_writer = match mode {
            ConsoleMode::Ro => false,
            ConsoleMode::Rw => {
                // First writer wins; later Rw attaches degrade to Ro.
                current_writer.is_none()
            }
            ConsoleMode::Takeover => {
                if let Some(prev) = current_writer {
                    for c in s.clients.iter_mut() {
                        if c.id == prev {
                            c.is_writer = false;
                            demoted.push(prev);
                        }
                    }
                }
                true
            }
        };

        let scrollback: Vec<u8> = s.scrollback.iter().copied().collect();
        s.clients.push(Client {
            id,
            sock,
            is_writer,
        });
        crate::daemon::metrics::L2CPU_CONSOLE_CLIENTS
            .at(self.idx)
            .set(s.clients.len() as i64);
        (
            AttachResult {
                id,
                is_writer,
                scrollback_bytes: scrollback.len() as u32,
                demoted,
            },
            scrollback,
        )
    }

    pub fn detach(&self, id: u64) {
        let mut s = self.state.lock().unwrap();
        s.clients.retain(|c| c.id != id);
        crate::daemon::metrics::L2CPU_CONSOLE_CLIENTS
            .at(self.idx)
            .set(s.clients.len() as i64);
    }

    /// Id of the current writer, if any.
    pub fn current_writer_id(&self) -> Option<u64> {
        self.state
            .lock()
            .unwrap()
            .clients
            .iter()
            .find(|c| c.is_writer)
            .map(|c| c.id)
    }

    /// Push chip-side output: append to scrollback (dropping oldest bytes to
    /// stay under [`SCROLLBACK_BYTES`]) and non-blocking-fan-out to every
    /// attached client via `send(.., MSG_DONTWAIT)`. Clients whose write
    /// would block are dropped.
    ///
    /// Returns the ids of clients that were dropped because their socket
    /// errored or blocked; the caller can use them to emit log messages.
    pub fn push_chip_output(&self, bytes: &[u8]) -> Vec<u64> {
        if bytes.is_empty() {
            return Vec::new();
        }
        let mut s = self.state.lock().unwrap();

        // Append to scrollback, dropping oldest bytes to stay bounded.
        let overflow = (s.scrollback.len() + bytes.len()).saturating_sub(SCROLLBACK_BYTES);
        for _ in 0..overflow {
            s.scrollback.pop_front();
        }
        s.scrollback.extend(bytes.iter().copied());

        // Fan-out. `send` with MSG_DONTWAIT returns EAGAIN if the kernel
        // buffer can't take the full frame — we drop that client entirely
        // rather than tracking partial writes, because scrollback already
        // covers short disconnects and we don't want to pay the bookkeeping.
        let mut dropped = Vec::new();
        s.clients
            .retain_mut(|c| match send_all_dontwait(&c.sock, bytes) {
                Ok(()) => true,
                Err(_) => {
                    dropped.push(c.id);
                    false
                }
            });
        if !dropped.is_empty() {
            crate::daemon::metrics::L2CPU_CONSOLE_CLIENTS
                .at(self.idx)
                .set(s.clients.len() as i64);
        }
        dropped
    }

    /// Current attached-client count (test + status helper).
    pub fn client_count(&self) -> usize {
        self.state.lock().unwrap().clients.len()
    }

    /// Send a final goodbye line to every attached client and shut down
    /// their daemon-side fds so client-side reader threads see EOF and
    /// exit cleanly. Used by stop / force-reboot / shutdown so a
    /// `bhx connect` doesn't hang silently after its slot disappears.
    ///
    /// Also appends the goodbye line into the scrollback ring so a
    /// subsequent [`Self::tail`] capture (taken by `internal_stop` for
    /// the post-mortem readout in #160) ends with `[bhx: <reason>]\r\n`
    /// — the cue an operator needs to know the slot torn down cleanly.
    pub fn disconnect_all_with_reason(&self, reason: &str) {
        let mut s = self.state.lock().unwrap();
        let goodbye = format!("\r\n[bhx: {}]\r\n", reason);

        // Append to scrollback first (#160). Bound by the same overflow
        // logic as `push_chip_output`; the goodbye is short so this is
        // effectively a no-overflow append in practice.
        let overflow = (s.scrollback.len() + goodbye.len()).saturating_sub(SCROLLBACK_BYTES);
        for _ in 0..overflow {
            s.scrollback.pop_front();
        }
        s.scrollback.extend(goodbye.bytes());

        for c in &s.clients {
            // Best-effort write of the goodbye line. Errors here are
            // fine — we're about to shut the socket down anyway.
            let _ = send_all_dontwait(&c.sock, goodbye.as_bytes());
            let _ = c.sock.shutdown(std::net::Shutdown::Both);
        }
        s.clients.clear();
        crate::daemon::metrics::L2CPU_CONSOLE_CLIENTS
            .at(self.idx)
            .set(0);
    }

    /// Snapshot the last `n` bytes of the scrollback (or the whole
    /// scrollback if it's shorter). Pure read — no state change.
    /// Used by `internal_stop` to capture a post-mortem tail before
    /// the slot is dropped (#160).
    pub fn tail(&self, n: usize) -> Vec<u8> {
        let s = self.state.lock().unwrap();
        let len = s.scrollback.len();
        let take = n.min(len);
        let start = len - take;
        s.scrollback.iter().skip(start).copied().collect()
    }

    /// Throw away the scrollback ring. Called on the Running → Parked
    /// transition: the bytes from the dead kernel's lifetime aren't
    /// useful to a future `bhx connect` against a re-released slot,
    /// and replaying them breaks things — operator terminals respond
    /// to `\x1b[6n` queries embedded in the old output, the writer
    /// pump forwards those CPR responses to the chip, and U-Boot on
    /// the next release reads them as keystrokes that interrupt
    /// autoboot. Operators who want a post-mortem can use the tail
    /// captured at `internal_stop` time (#160) once the slot reaches
    /// the Stopped state.
    pub fn clear_scrollback(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.scrollback.clear();
        }
    }
}

/// `send()` with `MSG_DONTWAIT`. Loops only on EINTR; any partial write (or
/// WouldBlock) is reported as an error so the caller drops the client.
fn send_all_dontwait(sock: &UnixStream, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let n = unsafe {
            libc::send(
                sock.as_raw_fd(),
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            // send() returning 0 is unexpected on a streaming socket;
            // surfacing it as Internal lets the caller's logger flag
            // it as a daemon-side anomaly rather than a typical IO
            // error. Bridge to io::Error so the function's
            // io::Result return type stays unchanged for callers.
            return Err(crate::Error::internal("send returned 0").into());
        }
        bytes = &bytes[n as usize..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    fn pair_nonblocking() -> (UnixStream, UnixStream) {
        // Fan-out uses MSG_DONTWAIT so the hub end doesn't need O_NONBLOCK,
        // but some tests drain the client-side non-blockingly to detect
        // disconnection cleanly. Keep both non-blocking for symmetry.
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();
        (a, b)
    }

    /// Wiring test (#33): attach + detach update
    /// `metrics::L2CPU_CONSOLE_CLIENTS{idx=N}`. The hub takes its idx
    /// at construction, so each test can use a distinct slot to
    /// avoid bleeding state across the parallel test run.
    #[test]
    fn attach_and_detach_update_clients_gauge() {
        use crate::daemon::metrics::L2CPU_CONSOLE_CLIENTS;

        // Use idx=3 so we don't collide with the default-0 hub
        // construction other tests use; the gauge is a global so
        // test order matters otherwise.
        let hub = ConsoleHub::new(3);
        let before = L2CPU_CONSOLE_CLIENTS.at(3).get();

        let (d1, _c1) = pair_nonblocking();
        let (d2, _c2) = pair_nonblocking();

        let (r1, _) = hub.attach(d1, ConsoleMode::Rw);
        assert_eq!(L2CPU_CONSOLE_CLIENTS.at(3).get(), before + 1);

        let (r2, _) = hub.attach(d2, ConsoleMode::Ro);
        assert_eq!(L2CPU_CONSOLE_CLIENTS.at(3).get(), before + 2);

        hub.detach(r1.id);
        assert_eq!(L2CPU_CONSOLE_CLIENTS.at(3).get(), before + 1);

        hub.detach(r2.id);
        assert_eq!(L2CPU_CONSOLE_CLIENTS.at(3).get(), before);
    }

    #[test]
    fn push_fans_out_to_all_clients() {
        let hub = ConsoleHub::new(0);
        let (a_daemon, a_client) = pair_nonblocking();
        let (b_daemon, b_client) = pair_nonblocking();

        hub.attach(a_daemon, ConsoleMode::Rw);
        hub.attach(b_daemon, ConsoleMode::Ro);

        let dropped = hub.push_chip_output(b"hello");
        assert!(dropped.is_empty());
        assert_eq!(hub.client_count(), 2);

        // Read with a tight blocking timeout instead of relying on the
        // socket buffer being immediately ready after the push. Either
        // client side blocking forever would mean push_chip_output's
        // fan-out missed it; a tight bound surfaces that as a clean
        // test failure rather than a hang.
        a_client.set_nonblocking(false).unwrap();
        b_client.set_nonblocking(false).unwrap();
        let timeout = Some(std::time::Duration::from_secs(2));
        a_client.set_read_timeout(timeout).unwrap();
        b_client.set_read_timeout(timeout).unwrap();

        let mut buf_a = [0u8; 16];
        let mut buf_b = [0u8; 16];
        let n_a = (&a_client).read(&mut buf_a).unwrap();
        let n_b = (&b_client).read(&mut buf_b).unwrap();
        assert_eq!(&buf_a[..n_a], b"hello");
        assert_eq!(&buf_b[..n_b], b"hello");
    }

    #[test]
    fn scrollback_is_bounded() {
        let hub = ConsoleHub::new(0);
        let chunk = vec![b'x'; SCROLLBACK_BYTES];
        hub.push_chip_output(&chunk);
        hub.push_chip_output(b"tail");
        let state = hub.state.lock().unwrap();
        assert_eq!(state.scrollback.len(), SCROLLBACK_BYTES);
        // Last 4 bytes should be "tail"
        let tail: Vec<u8> = state
            .scrollback
            .iter()
            .skip(SCROLLBACK_BYTES - 4)
            .copied()
            .collect();
        assert_eq!(&tail, b"tail");
    }

    #[test]
    fn first_rw_becomes_writer_later_rw_is_ro() {
        let hub = ConsoleHub::new(0);
        let (d1, _c1) = pair_nonblocking();
        let (d2, _c2) = pair_nonblocking();

        let (r1, _) = hub.attach(d1, ConsoleMode::Rw);
        let (r2, _) = hub.attach(d2, ConsoleMode::Rw);
        assert!(r1.is_writer);
        assert!(!r2.is_writer);
        assert_eq!(hub.current_writer_id(), Some(r1.id));
    }

    #[test]
    fn takeover_demotes_previous_writer() {
        let hub = ConsoleHub::new(0);
        let (d1, _c1) = pair_nonblocking();
        let (d2, _c2) = pair_nonblocking();

        let (r1, _) = hub.attach(d1, ConsoleMode::Rw);
        let (r2, _) = hub.attach(d2, ConsoleMode::Takeover);

        // r1.is_writer is a snapshot from the attach call; don't re-check it.
        // The live state is in the hub — `current_writer_id` and `demoted`.
        assert!(r2.is_writer);
        assert_eq!(r2.demoted, vec![r1.id]);
        assert_eq!(hub.current_writer_id(), Some(r2.id));
    }

    #[test]
    fn detach_removes_client() {
        let hub = ConsoleHub::new(0);
        let (d1, _c1) = pair_nonblocking();
        let (r1, _) = hub.attach(d1, ConsoleMode::Rw);
        assert_eq!(hub.client_count(), 1);
        hub.detach(r1.id);
        assert_eq!(hub.client_count(), 0);
    }

    #[test]
    fn attach_returns_scrollback() {
        let hub = ConsoleHub::new(0);
        hub.push_chip_output(b"already-there");
        let (d1, _c1) = pair_nonblocking();
        let (res, replay) = hub.attach(d1, ConsoleMode::Ro);
        assert_eq!(res.scrollback_bytes as usize, b"already-there".len());
        assert_eq!(&replay, b"already-there");
    }

    #[test]
    fn disconnect_all_with_reason_sends_message_and_clears_clients() {
        let hub = ConsoleHub::new(0);
        let (d1, c1) = pair_nonblocking();
        let (d2, c2) = pair_nonblocking();
        hub.attach(d1, ConsoleMode::Rw);
        hub.attach(d2, ConsoleMode::Ro);

        hub.disconnect_all_with_reason("l2cpu 0 stopped");
        assert_eq!(hub.client_count(), 0);

        // Each client end should see the goodbye message followed by EOF.
        c1.set_nonblocking(false).unwrap();
        c2.set_nonblocking(false).unwrap();
        let timeout = Some(std::time::Duration::from_secs(2));
        c1.set_read_timeout(timeout).unwrap();
        c2.set_read_timeout(timeout).unwrap();

        for client in [&c1, &c2] {
            let mut all = Vec::new();
            let mut buf = [0u8; 256];
            loop {
                match (&*client).read(&mut buf) {
                    Ok(0) => break, // EOF — what we want
                    Ok(n) => all.extend_from_slice(&buf[..n]),
                    Err(e) => panic!("unexpected read error: {}", e),
                }
            }
            let msg = String::from_utf8_lossy(&all);
            assert!(
                msg.contains("[bhx: l2cpu 0 stopped]"),
                "expected goodbye line, got {:?}",
                msg
            );
        }
    }

    #[test]
    fn non_draining_client_is_dropped() {
        // Fill the socket buffer of a non-draining client until write_all
        // returns WouldBlock; hub should report it dropped and remove it.
        let hub = ConsoleHub::new(0);
        let (d1, _c1_never_drains) = pair_nonblocking();
        let (r1, _) = hub.attach(d1, ConsoleMode::Ro);

        let big = vec![b'z'; 64 * 1024];
        let mut dropped = Vec::new();
        // Worst case: default AF_UNIX SO_SNDBUF on Linux is ~200 KB, so a
        // few iterations are enough. Cap at 64 to be paranoid.
        for _ in 0..64 {
            dropped = hub.push_chip_output(&big);
            if !dropped.is_empty() {
                break;
            }
        }
        assert_eq!(dropped, vec![r1.id]);
        assert_eq!(hub.client_count(), 0);
    }

    #[test]
    fn partial_fan_out_drops_only_the_staller_and_keeps_the_drainers() {
        // Three attached clients. Two drain promptly between pushes; the
        // third never reads. Eventually the third's socket buffer fills
        // and push_chip_output drops it on the next push. The drainers
        // must (a) remain attached, (b) have received every byte sent
        // up to and including the last successful push.
        let hub = ConsoleHub::new(2);
        let (d1, c1) = pair_nonblocking();
        let (d2, c2) = pair_nonblocking();
        let (d3, _c3_never_drains) = pair_nonblocking();

        let (r1, _) = hub.attach(d1, ConsoleMode::Ro);
        let (r2, _) = hub.attach(d2, ConsoleMode::Ro);
        let (r3, _) = hub.attach(d3, ConsoleMode::Ro);
        assert_eq!(hub.client_count(), 3);

        let chunk = vec![b'P'; 8 * 1024];
        let mut total_sent: usize = 0;
        let mut dropped = Vec::new();

        // Drain c1/c2 between pushes so their buffers don't backfill;
        // c3 never gets drained, so the kernel-side buffer climbs each
        // push until the next send_all_dontwait() EAGAINs.
        let mut received_c1: Vec<u8> = Vec::new();
        let mut received_c2: Vec<u8> = Vec::new();
        let mut buf = [0u8; 32 * 1024];
        for _ in 0..256 {
            dropped = hub.push_chip_output(&chunk);
            // The push either succeeds for everyone (returns []) or
            // succeeds for c1/c2 and drops c3. In neither case do c1/c2
            // miss bytes — we only count `total_sent` when the drainers
            // were retained.
            if dropped.is_empty() || dropped == vec![r3.id] {
                total_sent += chunk.len();
            }
            // Drain c1 and c2 best-effort (non-blocking).
            while let Ok(n) = (&c1).read(&mut buf) {
                if n == 0 {
                    break;
                }
                received_c1.extend_from_slice(&buf[..n]);
            }
            while let Ok(n) = (&c2).read(&mut buf) {
                if n == 0 {
                    break;
                }
                received_c2.extend_from_slice(&buf[..n]);
            }
            if !dropped.is_empty() {
                break;
            }
        }

        assert_eq!(dropped, vec![r3.id], "only the staller should drop");
        // The drainers stay attached — c3's stall must not cascade.
        assert_eq!(hub.client_count(), 2);
        let writer = hub.current_writer_id();
        assert!(
            writer.is_none() || writer == Some(r1.id) || writer == Some(r2.id),
            "writer state shouldn't promote c3"
        );

        // Drain whatever c1/c2 still have buffered (post-drop pushes go
        // only to them; even pre-drop the drainers may have bytes the
        // tight in-loop drain didn't reach).
        c1.set_nonblocking(false).unwrap();
        c2.set_nonblocking(false).unwrap();
        let timeout = Some(std::time::Duration::from_millis(200));
        c1.set_read_timeout(timeout).unwrap();
        c2.set_read_timeout(timeout).unwrap();
        loop {
            match (&c1).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received_c1.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        loop {
            match (&c2).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => received_c2.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }

        // Every retained-push byte must reach c1 and c2. (>= because of
        // the stop-at-drop loop semantics — the *post*-drop pushes also
        // succeed for the drainers but we don't count those into
        // total_sent.)
        assert!(
            received_c1.len() >= total_sent,
            "c1 received {} of {} sent",
            received_c1.len(),
            total_sent
        );
        assert!(received_c1.iter().all(|&b| b == b'P'));
        assert!(
            received_c2.len() >= total_sent,
            "c2 received {} of {} sent",
            received_c2.len(),
            total_sent
        );
        assert!(received_c2.iter().all(|&b| b == b'P'));

        // r2 keeps a useful side check: it was not promoted to writer
        // simply because r3 dropped (its mode was Ro). Assert role
        // hasn't shifted.
        assert!(!r2.is_writer);
    }

    // ---- shutdown tail (#160) ----

    #[test]
    fn tail_returns_last_n_bytes_or_full_scrollback_if_shorter() {
        let hub = ConsoleHub::new(0);
        let big: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        hub.push_chip_output(&big);
        let t = hub.tail(16 * 1024);
        assert_eq!(t.len(), 16 * 1024);
        // The tail is the last 16 KiB of the source.
        assert_eq!(t.as_slice(), &big[big.len() - 16 * 1024..]);

        // Shorter scrollback than requested → return everything.
        let hub2 = ConsoleHub::new(0);
        hub2.push_chip_output(b"only-100-bytes-here");
        let t2 = hub2.tail(16 * 1024);
        assert_eq!(t2, b"only-100-bytes-here");
    }

    #[test]
    fn disconnect_all_with_reason_appends_goodbye_into_scrollback() {
        let hub = ConsoleHub::new(0);
        hub.push_chip_output(b"some kernel output\n");
        hub.disconnect_all_with_reason("l2cpu 0 stopped (test)");
        let t = hub.tail(SHUTDOWN_TAIL_BYTES);
        let s = String::from_utf8_lossy(&t);
        assert!(
            s.ends_with("[bhx: l2cpu 0 stopped (test)]\r\n"),
            "scrollback tail should end with the goodbye line, got {:?}",
            s
        );
        assert!(
            s.contains("some kernel output"),
            "scrollback tail should retain prior bytes, got {:?}",
            s
        );
    }
}
