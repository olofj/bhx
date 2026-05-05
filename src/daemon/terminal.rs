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
pub fn pump(fd: OwnedFd, exit: Arc<AtomicBool>) -> io::Result<()> {
    let _raw = TerminalRawMode::new()?;

    // Wrap the OwnedFd in a UnixStream so we get split read/write easily.
    // from_raw_fd() would take ownership, which is what we want — OwnedFd
    // won't double-close because we leak it into the stream.
    let stream = unsafe {
        use std::os::fd::{FromRawFd, IntoRawFd};
        UnixStream::from_raw_fd(fd.into_raw_fd())
    };

    // Counter for DSR-CPR queries the guest has sent toward the
    // operator's terminal but whose response we haven't forwarded yet.
    // The reader thread bumps it when it spots `ESC [ 6 n` in the
    // chip-output stream; the writer thread drains it when a CPR
    // response on the operator's stdin should be forwarded to the
    // chip side instead of suppressed (#121 / resize).
    let pending_cpr = Arc::new(AtomicUsize::new(0));

    // Reader thread: stream → stdout.
    let reader_exit = exit.clone();
    let reader_stream = stream.try_clone()?;
    let reader_cpr = pending_cpr.clone();
    let reader = thread::spawn(move || reader_loop(reader_stream, reader_exit, reader_cpr));

    // Main thread: stdin → stream, with Ctrl-A x detection.
    let writer_result = writer_loop(&stream, &exit, &pending_cpr);

    // Shut down the socket so both the daemon side (which then detaches us
    // from the hub and closes its end) and our reader clone see EOF. Just
    // dropping `stream` isn't enough: the reader thread holds a `try_clone`
    // of the same socket, keeping the client-side endpoint open.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(stream);
    let _ = reader.join();
    writer_result
}

fn reader_loop(stream: UnixStream, exit: Arc<AtomicBool>, pending_cpr: Arc<AtomicUsize>) {
    let mut stream = stream;
    let mut buf = [0u8; 4096];
    let mut det = CprQueryDetector::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => {
                exit.store(true, Ordering::Relaxed);
                return;
            }
            Ok(n) => {
                let queries = det.consume(&buf[..n]);
                if queries > 0 {
                    pending_cpr.fetch_add(queries, Ordering::Relaxed);
                }
                let _ = io::stdout().write_all(&buf[..n]);
                let _ = io::stdout().flush();
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
/// occurrences in the chip-output stream. Used by the reader thread to
/// gate the writer thread's CPR-response passthrough — without a
/// matching outbound query, a CPR-shaped sequence on the operator's
/// stdin is the spurious-getty case from #121 and gets dropped.
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

    /// Count DSR-CPR queries (`ESC [ 6 n`, with optional digits/`;`
    /// in place of the bare `6` for the unusual case of a tool asking
    /// for an alternative report) in `bytes`. Strict shape: final byte
    /// `n`, intermediate bytes only digits and `;`.
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
    pending_cpr: &AtomicUsize,
) -> io::Result<()> {
    let mut stream = stream;
    let mut ctrl_a = false;
    // Filter cursor-position-report (CPR) replies coming from the host
    // terminal: when we flip into raw mode (or anything else that the
    // terminal interprets as needing a status report) the terminal can
    // emit `ESC [ <row> ; <col> R` on stdin. With no outbound query
    // pending, forwarding those bytes straight to the chip-side UART
    // delivers them to the guest's getty (#121).
    //
    // When the GUEST asked for the report (e.g. `resize`, `top`'s
    // window-size probe), it wrote `ESC [ 6 n` to the chip-side UART;
    // the reader thread sees that go past on the chip-output stream
    // and bumps `pending_cpr`. We let the matching response through
    // by decrementing the counter instead of dropping the bytes.
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
                // Forward only if the chip side just asked for a CPR.
                // Otherwise this is the spurious-getty case from #121.
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

    /// Replays the writer-side suppress-by-default policy: drops CPR
    /// responses outright. Equivalent to running with `pending_cpr`
    /// stuck at zero — the #121 getty case.
    fn run_suppress_cprs(input: &[u8]) -> Vec<u8> {
        let mut state = EscState::Idle;
        let mut out = Vec::new();
        for &b in input {
            let mut step = Vec::new();
            match advance_esc(&mut state, b, &mut step) {
                EscDecision::Drop | EscDecision::CprResponse => {}
                EscDecision::Emit => out.extend(step),
            }
        }
        out
    }

    /// Replays the writer's full policy: forward CPR responses when the
    /// counter is non-zero (the resize case), suppress otherwise.
    fn run_with_pending(input: &[u8], pending: &AtomicUsize) -> Vec<u8> {
        let mut state = EscState::Idle;
        let mut out = Vec::new();
        for &b in input {
            let mut step = Vec::new();
            match advance_esc(&mut state, b, &mut step) {
                EscDecision::Drop => {}
                EscDecision::Emit => out.extend(step),
                EscDecision::CprResponse => {
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

    #[test]
    fn passthrough_plain_bytes() {
        assert_eq!(run_suppress_cprs(b"hello"), b"hello");
    }

    #[test]
    fn drops_cursor_position_report_when_no_query_pending() {
        // The #121 getty case: terminal spuriously emits a CPR with
        // no matching outbound query — must NOT reach the chip side.
        assert_eq!(run_suppress_cprs(b"\x1b[97;428R"), b"");
        assert_eq!(run_suppress_cprs(b"\x1b[5;1R"), b"");
        // Multiple back-to-back CPRs as the bug repro showed.
        assert_eq!(run_suppress_cprs(b"\x1b[97;428R\x1b[5;1R\x1b[97;428R"), b"");
    }

    #[test]
    fn forwards_cpr_response_when_query_was_pending() {
        // The resize case: guest asked, terminal answered, we forward.
        let pending = AtomicUsize::new(1);
        assert_eq!(run_with_pending(b"\x1b[24;80R", &pending), b"\x1b[24;80R");
        // Counter was drained.
        assert_eq!(pending.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn forwards_one_cpr_per_outstanding_query_then_suppresses() {
        // Two queries, three responses arriving back-to-back. First
        // two forwarded; third suppressed.
        let pending = AtomicUsize::new(2);
        assert_eq!(
            run_with_pending(b"\x1b[1;1R\x1b[2;2R\x1b[3;3R", &pending),
            b"\x1b[1;1R\x1b[2;2R"
        );
        assert_eq!(pending.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn forwards_other_csi_unchanged() {
        // Arrow keys: ESC [ A / B / C / D — must pass through.
        assert_eq!(run_suppress_cprs(b"\x1b[A"), b"\x1b[A");
        assert_eq!(run_suppress_cprs(b"\x1b[D"), b"\x1b[D");
        // Cursor home (HVP), CSI H — must pass through (it's an output
        // sequence, but if a user types it we forward verbatim).
        assert_eq!(run_suppress_cprs(b"\x1b[H"), b"\x1b[H");
        // CSI with parameters and a non-R terminator (e.g. CUP) — pass through.
        assert_eq!(run_suppress_cprs(b"\x1b[10;20H"), b"\x1b[10;20H");
    }

    #[test]
    fn forwards_lone_escape() {
        // Bare ESC keypress (some users send this from terminals as the
        // alt-key-equivalent prefix). ESC followed by non-`[` forwards both.
        assert_eq!(run_suppress_cprs(b"\x1ba"), b"\x1ba");
    }

    #[test]
    fn cpr_sequence_split_across_bytes_decision_is_cpr_response() {
        // Real reads come one byte at a time; the state machine must
        // reassemble across calls before deciding. The non-final bytes
        // emit Drop while buffering; the final `R` flips to CprResponse
        // with the full sequence in `out`.
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

    // ---- CprQueryDetector ----

    #[test]
    fn cpr_query_detector_counts_dsr_query_in_chip_output() {
        let mut det = CprQueryDetector::new();
        // The exact bytes `resize` writes when probing.
        assert_eq!(det.consume(b"\x1b[6n"), 1);
    }

    #[test]
    fn cpr_query_detector_counts_multiple_queries_across_chunks() {
        let mut det = CprQueryDetector::new();
        // Pretend the chip output came in three reads. Total: 2 queries.
        assert_eq!(det.consume(b"hello\x1b[6n"), 1);
        assert_eq!(det.consume(b"world"), 0);
        assert_eq!(det.consume(b"\x1b[6n more"), 1);
    }

    #[test]
    fn cpr_query_detector_handles_query_split_across_chunk_boundary() {
        let mut det = CprQueryDetector::new();
        assert_eq!(det.consume(b"\x1b["), 0);
        assert_eq!(det.consume(b"6"), 0);
        assert_eq!(det.consume(b"n"), 1);
    }

    #[test]
    fn cpr_query_detector_ignores_non_query_csi() {
        let mut det = CprQueryDetector::new();
        // CUP, arrows, SGR — not a DSR query.
        assert_eq!(det.consume(b"\x1b[10;20H\x1b[A\x1b[1;31m"), 0);
        // CPR response also doesn't count — wrong direction.
        assert_eq!(det.consume(b"\x1b[24;80R"), 0);
    }

    #[test]
    fn cpr_query_detector_recognizes_dsr_with_param_digits() {
        // DSR variants other than `6n`: e.g. `5n` (status report). The
        // detector treats anything ending in `n` with digit/`;`-only
        // intermediates as a query candidate. False positives here cost
        // little — at worst we forward one spurious CPR response that
        // then lands on getty, no worse than pre-#121.
        let mut det = CprQueryDetector::new();
        assert_eq!(det.consume(b"\x1b[5n"), 1);
        assert_eq!(det.consume(b"\x1b[?6n"), 0); // private-mode prefix breaks it
    }
}
