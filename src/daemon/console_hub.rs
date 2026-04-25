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

/// Fixed-size per-L2CPU scrollback buffer. Sized to comfortably hold a full
/// kernel+systemd boot log (~20–40 KiB worth of ANSI-decorated output).
pub const SCROLLBACK_BYTES: usize = 64 * 1024;

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
            return Err(io::Error::other("send returned 0"));
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
}
