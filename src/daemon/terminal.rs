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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use crate::console::TerminalRawMode;

/// Drive the bidirectional stdin ↔ fd pump until the user hits Ctrl-A x
/// (which flips `exit`) or the fd closes.
///
/// `scrollback_bytes` is the number of bytes the daemon will replay
/// before transitioning to live chip output — it comes back from
/// `attach_console`'s response. The writer suppresses CPR responses
/// on the operator's stdin until that many bytes have been drained
/// (the spurious-getty case from #121 — terminal answers a stale
/// query in the replayed history, or emits one on raw-mode entry,
/// and we don't want that response reaching the chip-side getty).
/// After the boundary, everything passes through, including
/// legitimate responses to live queries from `resize` / `vim` / etc.
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
    // the chip stream. Writer drops CPR responses while this is true.
    let in_replay = Arc::new(AtomicBool::new(scrollback_bytes > 0));

    // Reader thread: stream → stdout.
    let reader_exit = exit.clone();
    let reader_stream = stream.try_clone()?;
    let reader_in_replay = in_replay.clone();
    let reader = thread::spawn(move || {
        reader_loop(
            reader_stream,
            reader_exit,
            scrollback_bytes,
            reader_in_replay,
        )
    });

    // Main thread: stdin → stream, with Ctrl-A x detection.
    let writer_result = writer_loop(&stream, &exit, &in_replay);

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
) {
    let mut stream = stream;
    let mut buf = [0u8; 4096];
    let mut drained: u64 = 0;
    let target = scrollback_bytes as u64;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => {
                exit.store(true, Ordering::Relaxed);
                return;
            }
            Ok(n) => {
                let _ = io::stdout().write_all(&buf[..n]);
                let _ = io::stdout().flush();
                if in_replay.load(Ordering::Relaxed) {
                    drained = drained.saturating_add(n as u64);
                    if drained >= target {
                        in_replay.store(false, Ordering::Relaxed);
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

fn writer_loop(stream: &UnixStream, exit: &AtomicBool, in_replay: &AtomicBool) -> io::Result<()> {
    let mut stream = stream;
    let mut ctrl_a = false;
    // Filter cursor-position-report (CPR) replies on the operator's
    // stdin while the chip-output stream is still replaying scrollback.
    // The replay can contain stale `ESC [ 6 n` queries that the host
    // terminal answers with `ESC [ <row> ; <col> R`; entering raw mode
    // can also provoke an unsolicited CPR. Either way, those responses
    // were never asked for by the live guest — forwarding them lands
    // at the chip-side getty (#121).
    //
    // After the reader thread drains `scrollback_bytes`, `in_replay`
    // flips to false and we pass everything through, including the
    // legitimate responses `resize` / `vim` / etc. ask for.
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
                // Live mode: forward as-is.
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
    use super::{advance_esc, EscDecision, EscState};

    /// Replays the writer-side replay-mode policy: CPR responses are
    /// suppressed (the #121 case — answer to a stale query in the
    /// scrollback or to a raw-mode-entry probe).
    fn run_replay_mode(input: &[u8]) -> Vec<u8> {
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

    /// Replays the writer-side live-mode policy: CPR responses pass
    /// through (the `resize` case — guest asked, terminal answered).
    fn run_live_mode(input: &[u8]) -> Vec<u8> {
        let mut state = EscState::Idle;
        let mut out = Vec::new();
        for &b in input {
            let mut step = Vec::new();
            match advance_esc(&mut state, b, &mut step) {
                EscDecision::Drop => {}
                EscDecision::Emit | EscDecision::CprResponse => out.extend(step),
            }
        }
        out
    }

    #[test]
    fn passthrough_plain_bytes() {
        assert_eq!(run_replay_mode(b"hello"), b"hello");
        assert_eq!(run_live_mode(b"hello"), b"hello");
    }

    #[test]
    fn drops_cpr_response_in_replay_mode() {
        // The #121 case: stale CPR from scrollback replay or raw-mode
        // entry — must not reach the chip side.
        assert_eq!(run_replay_mode(b"\x1b[97;428R"), b"");
        assert_eq!(run_replay_mode(b"\x1b[5;1R"), b"");
        // Multiple back-to-back CPRs as the bug repro showed.
        assert_eq!(run_replay_mode(b"\x1b[97;428R\x1b[5;1R\x1b[97;428R"), b"");
    }

    #[test]
    fn forwards_cpr_response_in_live_mode() {
        // The resize case: chip-side guest asked via \x1b[6n, host
        // terminal answered. After scrollback drained, response goes
        // through.
        assert_eq!(run_live_mode(b"\x1b[24;80R"), b"\x1b[24;80R");
        // Multiple back-to-back responses (e.g. resize + a follow-up
        // query): all forwarded.
        assert_eq!(
            run_live_mode(b"\x1b[24;80R\x1b[100;200R"),
            b"\x1b[24;80R\x1b[100;200R"
        );
    }

    #[test]
    fn forwards_other_csi_unchanged_in_both_modes() {
        // Arrow keys, cursor home, etc. — must pass through regardless
        // of the replay/live state. The CPR gating is specifically
        // limited to CSI-with-final-`R`-and-digit-only-intermediates.
        for runner in [run_replay_mode, run_live_mode] {
            assert_eq!(runner(b"\x1b[A"), b"\x1b[A");
            assert_eq!(runner(b"\x1b[D"), b"\x1b[D");
            assert_eq!(runner(b"\x1b[H"), b"\x1b[H");
            assert_eq!(runner(b"\x1b[10;20H"), b"\x1b[10;20H");
        }
    }

    #[test]
    fn forwards_lone_escape_in_both_modes() {
        // Bare ESC + non-`[` byte: forwards both.
        assert_eq!(run_replay_mode(b"\x1ba"), b"\x1ba");
        assert_eq!(run_live_mode(b"\x1ba"), b"\x1ba");
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
