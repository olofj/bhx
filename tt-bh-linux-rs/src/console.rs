// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! Terminal raw-mode guard — used by `daemon::terminal` when `connect`
//! attaches a tty to the console hub.
//!
//! Everything else that used to live here (in-process `console_main`,
//! `uart_loop`, `push_char` / `pop_char`, OpenSBI-descriptor + VIRTUART
//! ring-buffer helpers) moved into `daemon::chip_console` during the
//! daemon migration. That's now the only place those helpers run; the
//! duplicates here were dead code.

use std::io;

/// RAII struct that saves/restores terminal settings. When stdin is not a
/// tty (e.g. piped from /dev/null in a test harness or agent loop), this
/// is a no-op so callers don't need to branch.
pub struct TerminalRawMode {
    orig: Option<nix::sys::termios::Termios>,
}

impl TerminalRawMode {
    pub fn new() -> io::Result<Self> {
        use nix::sys::termios::*;
        let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        if !stdin_is_tty {
            eprintln!("[console] stdin is not a tty; skipping raw-mode setup");
            return Ok(TerminalRawMode { orig: None });
        }
        let orig = tcgetattr(std::io::stdin())
            .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
        let mut raw = orig.clone();

        raw.local_flags &= !(LocalFlags::ECHO
            | LocalFlags::ICANON
            | LocalFlags::ISIG
            | LocalFlags::IEXTEN);
        raw.input_flags &= !(InputFlags::BRKINT
            | InputFlags::INPCK
            | InputFlags::ISTRIP
            | InputFlags::IXON
            | InputFlags::ICRNL);
        raw.output_flags &= !OutputFlags::OPOST;
        raw.control_flags |= ControlFlags::CS8;

        tcsetattr(std::io::stdin(), SetArg::TCSAFLUSH, &raw)
            .map_err(|e| io::Error::from_raw_os_error(e as i32))?;

        Ok(TerminalRawMode { orig: Some(orig) })
    }
}

impl Drop for TerminalRawMode {
    fn drop(&mut self) {
        if let Some(orig) = self.orig.as_ref() {
            let _ = nix::sys::termios::tcsetattr(
                std::io::stdin(),
                nix::sys::termios::SetArg::TCSAFLUSH,
                orig,
            );
        }
    }
}
