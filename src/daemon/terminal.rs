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
pub fn pump(fd: OwnedFd, exit: Arc<AtomicBool>) -> io::Result<()> {
    let _raw = TerminalRawMode::new()?;

    // Wrap the OwnedFd in a UnixStream so we get split read/write easily.
    // from_raw_fd() would take ownership, which is what we want — OwnedFd
    // won't double-close because we leak it into the stream.
    let stream = unsafe {
        use std::os::fd::{FromRawFd, IntoRawFd};
        UnixStream::from_raw_fd(fd.into_raw_fd())
    };

    // Reader thread: stream → stdout.
    let reader_exit = exit.clone();
    let reader_stream = stream.try_clone()?;
    let reader = thread::spawn(move || reader_loop(reader_stream, reader_exit));

    // Main thread: stdin → stream, with Ctrl-A x detection.
    let writer_result = writer_loop(&stream, &exit);

    // Shut down the socket so both the daemon side (which then detaches us
    // from the hub and closes its end) and our reader clone see EOF. Just
    // dropping `stream` isn't enough: the reader thread holds a `try_clone`
    // of the same socket, keeping the client-side endpoint open.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(stream);
    let _ = reader.join();
    writer_result
}

fn reader_loop(stream: UnixStream, exit: Arc<AtomicBool>) {
    let mut stream = stream;
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => {
                exit.store(true, Ordering::Relaxed);
                return;
            }
            Ok(n) => {
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

fn writer_loop(stream: &UnixStream, exit: &AtomicBool) -> io::Result<()> {
    let mut stream = stream;
    let mut ctrl_a = false;
    // Filter cursor-position-report (CPR) replies coming from the host
    // terminal: when we flip into raw mode (or anything else that the
    // terminal interprets as needing a status report) the terminal can
    // emit `ESC [ <row> ; <col> R` on stdin. With no filter, those bytes
    // forward straight to the chip-side UART and end up at the guest's
    // getty (#121). Drop CSI sequences whose final byte is `R` and
    // whose intermediate bytes are only digits/`;` — that's the strict
    // CPR shape per ECMA-48 §8.3.14, and not a sequence a user would
    // ever type intentionally.
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
    Emit,
    Drop,
}

/// Drive the byte-level state machine. Pushes any bytes that should
/// reach the chip-side UART into `out`. Returns `Drop` only for the
/// final byte of a CPR sequence we just consumed (no bytes pushed);
/// otherwise returns `Emit` and the caller forwards everything in `out`.
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
            // digits and `;`. Drop. Anything else → forward the whole
            // sequence verbatim (including the leading `ESC [`).
            let cpr_intermediate = buf.iter().all(|&c| c.is_ascii_digit() || c == b';');
            if byte == b'R' && cpr_intermediate {
                *state = EscState::Idle;
                EscDecision::Drop
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

    fn run(input: &[u8]) -> Vec<u8> {
        let mut state = EscState::Idle;
        let mut out = Vec::new();
        for &b in input {
            let mut step = Vec::new();
            let _ = advance_esc(&mut state, b, &mut step);
            out.extend(step);
        }
        out
    }

    #[test]
    fn passthrough_plain_bytes() {
        assert_eq!(run(b"hello"), b"hello");
    }

    #[test]
    fn drops_cursor_position_report() {
        // The exact shape that landed in the bug report.
        assert_eq!(run(b"\x1b[97;428R"), b"");
        assert_eq!(run(b"\x1b[5;1R"), b"");
        // Multiple back-to-back CPRs as the bug repro showed.
        assert_eq!(run(b"\x1b[97;428R\x1b[5;1R\x1b[97;428R"), b"");
    }

    #[test]
    fn forwards_other_csi_unchanged() {
        // Arrow keys: ESC [ A / B / C / D — must pass through.
        assert_eq!(run(b"\x1b[A"), b"\x1b[A");
        assert_eq!(run(b"\x1b[D"), b"\x1b[D");
        // Cursor home (HVP), CSI H — must pass through (it's an output
        // sequence, but if a user types it we forward verbatim).
        assert_eq!(run(b"\x1b[H"), b"\x1b[H");
        // CSI with parameters and a non-R terminator (e.g. CUP) — pass through.
        assert_eq!(run(b"\x1b[10;20H"), b"\x1b[10;20H");
    }

    #[test]
    fn forwards_lone_escape() {
        // Bare ESC keypress (some users send this from terminals as the
        // alt-key-equivalent prefix). ESC followed by non-`[` forwards both.
        assert_eq!(run(b"\x1ba"), b"\x1ba");
    }

    #[test]
    fn cpr_sequence_split_across_bytes_still_drops() {
        // Real reads come one byte at a time; the state machine must
        // reassemble across calls before deciding.
        let mut state = EscState::Idle;
        let mut emitted = Vec::new();
        for &b in b"\x1b[97;428R" {
            let mut step = Vec::new();
            let dec = advance_esc(&mut state, b, &mut step);
            // The final byte produces Drop; preceding bytes also drop
            // because they're buffered inside the state.
            assert_eq!(dec, EscDecision::Drop);
            emitted.extend(step);
        }
        assert!(emitted.is_empty());
        assert_eq!(state, EscState::Idle);
    }
}
