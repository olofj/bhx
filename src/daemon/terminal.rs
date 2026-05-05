// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Client-side terminal pump: bridges the user's tty and the console fd we
//! got back from the daemon via SCM_RIGHTS.
//!
//! Exit sequence is the same Ctrl-A x that the in-process `console_main`
//! used. Exiting here just drops the fd, which causes the daemon's reader
//! thread to see EOF and detach cleanly; the daemon keeps running.

use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use crate::console::TerminalRawMode;

/// Drive the bidirectional stdin ↔ fd pump until the user hits Ctrl-A x
/// (which flips `exit`) or the fd closes.
///
/// CPR-response gating uses two pieces of state. (1) `in_replay` flips
/// to `false` once the reader thread drains `scrollback_bytes` of
/// chip output — answers to queries that lived in the scrollback
/// (and thus had nothing to do with the live guest) get dropped.
/// (2) `pending_cpr_queries` counts `\x1b[6n` queries the LIVE chip
/// stream emitted; the writer forwards a CPR response only when the
/// counter is non-zero. Together they cover three failure modes:
///
///   - Scrollback replay containing stale `\x1b[6n`: counter doesn't
///     bump (we only count live bytes), responses dropped.
///   - Raw-mode entry causing the operator's terminal to emit a
///     spontaneous CPR with no chip-side query: counter is 0,
///     response dropped.
///   - Live `resize` / `vim` query: counter bumped by reader,
///     response forwarded by writer.
///
/// `scrollback_bytes` comes back from `attach_console`'s response.
pub fn pump(fd: OwnedFd, exit: Arc<AtomicBool>, scrollback_bytes: u32) -> io::Result<()> {
    let _raw = TerminalRawMode::new()?;

    // Wrap the OwnedFd in a UnixStream so we get split read/write easily.
    // from_raw_fd() would take ownership, which is what we want — OwnedFd
    // won't double-close because we leak it into the stream.
    let stream = unsafe {
        use std::os::fd::{FromRawFd, IntoRawFd};
        UnixStream::from_raw_fd(fd.into_raw_fd())
    };

    // True until the reader thread has drained `scrollback_bytes` from
    // the chip stream. Writer drops CPR responses while this is true
    // and the counter stays at zero — see the comment on `pump`.
    let in_replay = Arc::new(AtomicBool::new(scrollback_bytes > 0));
    let pending_cpr = Arc::new(AtomicUsize::new(0));

    // Reader thread: stream → stdout. Bumps `pending_cpr` for live
    // `\x1b[6n` queries so the writer knows to forward the next CPR
    // response.
    let reader_exit = exit.clone();
    let reader_stream = stream.try_clone()?;
    let reader_in_replay = in_replay.clone();
    let reader_pending = pending_cpr.clone();
    let reader = thread::spawn(move || {
        reader_loop(
            reader_stream,
            reader_exit,
            scrollback_bytes,
            reader_in_replay,
            reader_pending,
        )
    });

    // Main thread: stdin → stream, with Ctrl-A x detection.
    let writer_result = writer_loop(&stream, &exit, &in_replay, &pending_cpr);

    // Shut down the socket so both the daemon side (which then detaches us
    // from the hub and closes its end) and our reader clone see EOF. Just
    // dropping `stream` isn't enough: the reader thread holds a `try_clone`
    // of the same socket, keeping the client-side endpoint open.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(stream);
    let _ = reader.join();
    writer_result
}

fn reader_loop(
    stream: UnixStream,
    exit: Arc<AtomicBool>,
    scrollback_bytes: u32,
    in_replay: Arc<AtomicBool>,
    pending_cpr: Arc<AtomicUsize>,
) {
    let mut stream = stream;
    let mut buf = [0u8; 4096];
    let mut drained: u64 = 0;
    let target = scrollback_bytes as u64;
    let mut det = CprQueryDetector::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => {
                exit.store(true, Ordering::Relaxed);
                return;
            }
            Ok(n) => {
                let _ = io::stdout().write_all(&buf[..n]);
                let _ = io::stdout().flush();
                // Decide where the live boundary falls inside this
                // chunk. Bytes at offsets `[live_start..n)` are live;
                // bytes before that belong to scrollback. Detector
                // only sees live bytes — counting queries in
                // scrollback would credit responses to a dead
                // incarnation.
                let was_replay = in_replay.load(Ordering::Relaxed);
                let live_start = if was_replay {
                    let remaining = target.saturating_sub(drained);
                    drained = drained.saturating_add(n as u64);
                    if drained >= target {
                        in_replay.store(false, Ordering::Relaxed);
                    }
                    remaining.min(n as u64) as usize
                } else {
                    0
                };
                if live_start < n {
                    let queries = det.consume(&buf[live_start..n]);
                    if queries > 0 {
                        pending_cpr.fetch_add(queries, Ordering::Relaxed);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                exit.store(true, Ordering::Relaxed);
                return;
            }
        }
    }
}

/// Byte-level state machine that counts `ESC [ 6 n` (DSR-CPR query)
/// occurrences in the chip-output stream. Used by the reader thread
/// to gate the writer thread's CPR-response passthrough — without a
/// matching outbound query, a CPR-shaped sequence on the operator's
/// stdin is the spurious / scrollback case and gets dropped.
#[derive(Debug, PartialEq)]
enum CprQueryState {
    Idle,
    Esc,
    /// Saw `ESC [` ; collecting the parameter bytes.
    Csi(Vec<u8>),
}

struct CprQueryDetector {
    state: CprQueryState,
}

impl CprQueryDetector {
    fn new() -> Self {
        CprQueryDetector {
            state: CprQueryState::Idle,
        }
    }

    /// Count DSR queries (`ESC [ <digits/;>* n`) in `bytes`. Strict
    /// shape: final byte `n`, intermediate bytes only digits and `;`.
    /// `\x1b[?6n` (private-mode prefix) does NOT match — `?` isn't a
    /// digit; intentional, the chip doesn't fire that variant.
    fn consume(&mut self, bytes: &[u8]) -> usize {
        let mut hits = 0;
        for &b in bytes {
            match &mut self.state {
                CprQueryState::Idle => {
                    if b == 0x1b {
                        self.state = CprQueryState::Esc;
                    }
                }
                CprQueryState::Esc => {
                    self.state = if b == b'[' {
                        CprQueryState::Csi(Vec::with_capacity(4))
                    } else {
                        CprQueryState::Idle
                    };
                }
                CprQueryState::Csi(buf) => {
                    let is_final = (0x40..=0x7e).contains(&b);
                    if !is_final {
                        buf.push(b);
                        continue;
                    }
                    let intermediate_ok = buf.iter().all(|&c| c.is_ascii_digit() || c == b';');
                    if b == b'n' && intermediate_ok {
                        hits += 1;
                    }
                    self.state = CprQueryState::Idle;
                }
            }
        }
        hits
    }
}

fn writer_loop(
    stream: &UnixStream,
    exit: &AtomicBool,
    in_replay: &AtomicBool,
    pending_cpr: &AtomicUsize,
) -> io::Result<()> {
    let mut stream = stream;
    let mut ctrl_a = false;
    // CPR-response gating: forward only when the LIVE chip stream
    // recently emitted a `\x1b[6n` query that hasn't been answered
    // yet (`pending_cpr > 0`). Scrollback-replay queries are
    // ignored by the reader, raw-mode-entry CPRs from the operator's
    // terminal arrive with the counter at zero and get dropped, and
    // legitimate `resize` / `vim` responses pass through.
    let mut esc = EscState::Idle;
    while !exit.load(Ordering::Relaxed) {
        // Poll stdin with 20 ms timeout so we notice `exit` without spinning.
        let mut rfds = unsafe { std::mem::zeroed::<libc::fd_set>() };
        unsafe { libc::FD_SET(libc::STDIN_FILENO, &mut rfds) };
        let mut tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 20_000,
        };
        let rc = unsafe {
            libc::select(
                libc::STDIN_FILENO + 1,
                &mut rfds,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut tv,
            )
        };
        if rc <= 0 {
            continue;
        }
        let mut b = [0u8; 1];
        let n = unsafe { libc::read(libc::STDIN_FILENO, b.as_mut_ptr() as *mut libc::c_void, 1) };
        if n <= 0 {
            break;
        }
        let byte = b[0];
        let mut to_emit = Vec::new();
        match advance_esc(&mut esc, byte, &mut to_emit) {
            EscDecision::Drop => continue,
            EscDecision::Emit => {}
            EscDecision::CprResponse => {
                if in_replay.load(Ordering::Relaxed) {
                    continue;
                }
                // Live mode: forward only if the chip recently asked
                // for a CPR. Otherwise this is a spontaneous response
                // from raw-mode entry (or any other host-terminal-side
                // emission) that the chip never solicited.
                let popped = pending_cpr
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                    .is_ok();
                if !popped {
                    continue;
                }
            }
        }
        for emitted in to_emit {
            if ctrl_a {
                ctrl_a = false;
                if emitted == b'x' {
                    let _ = io::stdout().write_all(b"\n\n");
                    exit.store(true, Ordering::Relaxed);
                    return Ok(());
                }
                stream.write_all(&[1])?;
                stream.write_all(&[emitted])?;
            } else if emitted == 1 {
                ctrl_a = true;
            } else {
                stream.write_all(&[emitted])?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
enum EscState {
    Idle,
    Esc,
    /// Buffered CSI prefix (everything seen since `ESC [`, including
    /// digits + `;`, NOT including the final byte that closes the
    /// sequence). A short Vec — CPRs are typically <16 bytes.
    Csi(Vec<u8>),
}

#[derive(Debug, PartialEq)]
enum EscDecision {
    /// Forward every byte in `out` verbatim.
    Emit,
    /// Suppress; `out` is empty. Either we're mid-sequence and still
    /// buffering, or we just consumed a non-final ESC byte.
    Drop,
    /// `out` contains a complete CPR response (`ESC [ <digits>;<digits> R`).
    /// The caller decides whether to forward — `pump` does iff there's a
    /// pending DSR-CPR query the guest asked for, otherwise it suppresses
    /// to preserve the #121 getty fix.
    CprResponse,
}

/// Drive the byte-level state machine. Pushes any bytes that should
/// reach the chip-side UART into `out` (or, for `CprResponse`, any
/// bytes the caller may forward). Returns `Drop` while buffering an
/// in-progress sequence with no bytes to deliver yet.
fn advance_esc(state: &mut EscState, byte: u8, out: &mut Vec<u8>) -> EscDecision {
    match state {
        EscState::Idle => {
            if byte == 0x1b {
                *state = EscState::Esc;
                EscDecision::Drop
            } else {
                out.push(byte);
                EscDecision::Emit
            }
        }
        EscState::Esc => {
            if byte == b'[' {
                *state = EscState::Csi(Vec::with_capacity(8));
                EscDecision::Drop
            } else {
                // ESC followed by something other than `[` — not a CSI.
                // Forward both bytes verbatim and reset.
                out.push(0x1b);
                out.push(byte);
                *state = EscState::Idle;
                EscDecision::Emit
            }
        }
        EscState::Csi(buf) => {
            // Final byte = 0x40-0x7E per ECMA-48 §5.4.
            let is_final = (0x40..=0x7e).contains(&byte);
            if !is_final {
                buf.push(byte);
                return EscDecision::Drop;
            }
            // Strict CPR: final 'R' AND intermediate bytes are only
            // digits and `;`. Surface as CprResponse with the bytes
            // populated; caller decides whether to forward. Anything
            // else → forward the whole sequence verbatim.
            let cpr_intermediate = buf.iter().all(|&c| c.is_ascii_digit() || c == b';');
            if byte == b'R' && cpr_intermediate {
                out.push(0x1b);
                out.push(b'[');
                out.extend_from_slice(buf);
                out.push(byte);
                *state = EscState::Idle;
                EscDecision::CprResponse
            } else {
                out.push(0x1b);
                out.push(b'[');
                out.extend_from_slice(buf);
                out.push(byte);
                *state = EscState::Idle;
                EscDecision::Emit
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{advance_esc, CprQueryDetector, EscDecision, EscState};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Replays the writer-side policy as a single helper. `in_replay`
    /// reflects the scrollback-drain flag; `pending` is the live-CPR-
    /// query counter the reader maintains. The two together decide
    /// whether to forward a CPR response.
    fn run(input: &[u8], in_replay: bool, pending: &AtomicUsize) -> Vec<u8> {
        let mut state = EscState::Idle;
        let mut out = Vec::new();
        for &b in input {
            let mut step = Vec::new();
            match advance_esc(&mut state, b, &mut step) {
                EscDecision::Drop => {}
                EscDecision::Emit => out.extend(step),
                EscDecision::CprResponse => {
                    if in_replay {
                        continue;
                    }
                    let popped = pending
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                        .is_ok();
                    if popped {
                        out.extend(step);
                    }
                }
            }
        }
        out
    }

    fn run_replay(input: &[u8]) -> Vec<u8> {
        run(input, true, &AtomicUsize::new(0))
    }

    fn run_live_no_pending(input: &[u8]) -> Vec<u8> {
        run(input, false, &AtomicUsize::new(0))
    }

    #[test]
    fn passthrough_plain_bytes_in_every_mode() {
        assert_eq!(run_replay(b"hello"), b"hello");
        assert_eq!(run_live_no_pending(b"hello"), b"hello");
        assert_eq!(run(b"hello", false, &AtomicUsize::new(2)), b"hello");
    }

    #[test]
    fn drops_cpr_response_during_scrollback_replay() {
        // The classic #121 case: stale CPR from a query buried in the
        // replayed scrollback. Must not reach the chip side.
        assert_eq!(run_replay(b"\x1b[97;428R"), b"");
        assert_eq!(run_replay(b"\x1b[5;1R\x1b[97;428R"), b"");
    }

    #[test]
    fn drops_cpr_response_in_live_mode_with_no_pending_query() {
        // Raw-mode entry on `bhx connect` causes some terminals to
        // emit a spontaneous CPR. Counter is zero — drop.
        assert_eq!(run_live_no_pending(b"\x1b[97;1R"), b"");
        // Multiple spurious CPRs in a row: still all dropped.
        assert_eq!(run_live_no_pending(b"\x1b[1;1R\x1b[2;2R\x1b[3;3R"), b"");
    }

    #[test]
    fn forwards_one_cpr_per_pending_query() {
        // The resize case: chip-side `\x1b[6n` was counted by the
        // reader; the writer forwards exactly that many responses
        // and drops the rest.
        let pending = AtomicUsize::new(2);
        let out = run(b"\x1b[24;80R\x1b[100;200R\x1b[3;3R", false, &pending);
        assert_eq!(out, b"\x1b[24;80R\x1b[100;200R");
        assert_eq!(pending.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn forwards_other_csi_unchanged_in_every_mode() {
        // Arrow keys, cursor home, etc. — must pass through. The
        // gate is strictly on CSI-with-final-`R`-and-digit-only
        // intermediates.
        let pending = AtomicUsize::new(0);
        for &replay in &[true, false] {
            assert_eq!(run(b"\x1b[A", replay, &pending), b"\x1b[A");
            assert_eq!(run(b"\x1b[D", replay, &pending), b"\x1b[D");
            assert_eq!(run(b"\x1b[H", replay, &pending), b"\x1b[H");
            assert_eq!(run(b"\x1b[10;20H", replay, &pending), b"\x1b[10;20H");
        }
    }

    #[test]
    fn forwards_lone_escape_in_every_mode() {
        let pending = AtomicUsize::new(0);
        assert_eq!(run(b"\x1ba", true, &pending), b"\x1ba");
        assert_eq!(run(b"\x1ba", false, &pending), b"\x1ba");
    }

    // ---- CprQueryDetector ----

    #[test]
    fn cpr_query_detector_counts_dsr_query() {
        let mut det = CprQueryDetector::new();
        assert_eq!(det.consume(b"\x1b[6n"), 1);
    }

    #[test]
    fn cpr_query_detector_counts_across_chunk_boundary() {
        let mut det = CprQueryDetector::new();
        assert_eq!(det.consume(b"\x1b["), 0);
        assert_eq!(det.consume(b"6"), 0);
        assert_eq!(det.consume(b"n"), 1);
    }

    #[test]
    fn cpr_query_detector_ignores_non_query_csi() {
        let mut det = CprQueryDetector::new();
        // CUP, arrows, SGR, CPR responses — none of these are queries.
        assert_eq!(det.consume(b"\x1b[10;20H\x1b[A\x1b[1;31m\x1b[24;80R"), 0);
    }

    #[test]
    fn cpr_query_detector_rejects_private_mode_prefix() {
        // `\x1b[?6n` (DECRQM-style) is not a DSR-CPR query — the `?`
        // isn't a digit, so our intermediate-bytes check fails.
        let mut det = CprQueryDetector::new();
        assert_eq!(det.consume(b"\x1b[?6n"), 0);
    }

    #[test]
    fn cpr_response_decision_is_cpr_response_with_full_bytes_emitted() {
        // Real reads come one byte at a time; the state machine must
        // reassemble across calls. The non-final bytes return Drop
        // while buffering; the final `R` flips to CprResponse with
        // the full sequence populated in `out`. The caller picks
        // whether to forward based on replay/live state.
        let mut state = EscState::Idle;
        let mut emitted = Vec::new();
        let bytes = b"\x1b[97;428R";
        for (i, &b) in bytes.iter().enumerate() {
            let mut step = Vec::new();
            let dec = advance_esc(&mut state, b, &mut step);
            if i + 1 < bytes.len() {
                assert_eq!(dec, EscDecision::Drop);
                assert!(step.is_empty());
            } else {
                assert_eq!(dec, EscDecision::CprResponse);
                emitted.extend(step);
            }
        }
        assert_eq!(emitted, b"\x1b[97;428R");
        assert_eq!(state, EscState::Idle);
    }
}
