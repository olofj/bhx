// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Last-resort signal handler for chip-access faults (#129).
//!
//! When `tt-smi -r` resets the card under a running daemon, our mmap'd
//! TLB windows + per-L2CPU VA regions become inaccessible mid-access.
//! The kernel raises SIGBUS (sometimes SIGSEGV depending on PTE state)
//! and the default disposition is `core_dump_and_die`, which leaves no
//! trace in the daemon log. Operators see the daemon "just disappear"
//! and can't tell whether it was tt-smi, an OOM kill, or a real bug.
//!
//! Install a small handler that writes a single line to stderr (which
//! the daemonized process has already pointed at the O_DSYNC log fd)
//! and `_exit`s with `128 + sig`. The handler is async-signal-safe by
//! construction: only `write(2)` against a pre-built static buffer
//! and `_exit(2)`. No allocation, no locking, no `format!`.
//!
//! `SA_RESETHAND` means a re-fault inside the handler hits the default
//! disposition (core dump) — no infinite-loop hazard. The empty signal
//! mask means a re-fault on the *log fd* (storage gone with the chip)
//! will still cleanly die rather than wedge.
//!
//! Install gating:
//! - Daemonized path only. Foreground `bhx daemon start --foreground`
//!   keeps the default disposition so the operator sees the panic /
//!   SIGBUS in their terminal directly (installing the handler would
//!   `_exit` before stdio buffers flush).
//! - Client process (`bhx boot`, `bhx connect`, …) does not touch the
//!   chip, so installing there would only mask client bugs.
//! - `#[cfg(test)]`-only paths skip install for the same reason.

use std::io;

/// Pre-formatted bytes for each handled signal. Looked up by index in
/// the handler — no `match` arms involving non-async-signal-safe code.
const MSG_SIGBUS: &[u8] =
    b"fatal: SIGBUS during chip access (tt-smi -r? PCIe link lost? Card pulled?)\n";
const MSG_SIGSEGV: &[u8] = b"fatal: SIGSEGV during chip access\n";
const MSG_UNKNOWN: &[u8] = b"fatal: unexpected signal in chip-fault handler\n";

extern "C" fn handle_chip_fault(sig: libc::c_int) {
    // Async-signal-safe ALLOWED LIST:
    //   write(2), _exit(2). That's it.
    //
    // DO NOT add: dlog!, eprintln!, format!, mutex lock, std::panic, malloc, …
    let msg: &[u8] = match sig {
        libc::SIGBUS => MSG_SIGBUS,
        libc::SIGSEGV => MSG_SIGSEGV,
        _ => MSG_UNKNOWN,
    };
    // STDERR_FILENO is the daemonized process's log fd. Best-effort —
    // if the write fails (storage gone with the chip) we still _exit.
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            msg.as_ptr() as *const libc::c_void,
            msg.len(),
        );
    }
    unsafe {
        libc::_exit(128 + sig);
    }
}

/// Install the chip-fault handler for SIGBUS + SIGSEGV. Idempotent —
/// later calls overwrite earlier ones.
pub fn install_chip_fault_handler() -> io::Result<()> {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    // Without SA_SIGINFO the kernel calls the handler with just `int sig`,
    // matching our extern "C" fn signature. Bind the function item to a
    // typed pointer first to keep clippy's function_casts_as_integer
    // happy (direct fn-item-to-usize is the lint trigger).
    let handler_ptr: extern "C" fn(libc::c_int) = handle_chip_fault;
    act.sa_sigaction = handler_ptr as usize;
    // SA_RESETHAND: a re-fault inside the handler restores the default
    // disposition for that signal, so the second hit core-dumps cleanly
    // instead of looping us through the handler again.
    act.sa_flags = libc::SA_RESETHAND;
    if unsafe { libc::sigemptyset(&mut act.sa_mask) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for sig in [libc::SIGBUS, libc::SIGSEGV] {
        if unsafe { libc::sigaction(sig, &act, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calling `install_chip_fault_handler` registers SOMETHING for
    /// SIGBUS + SIGSEGV. We don't assert the handler pointer is exactly
    /// ours — that's brittle across build modes — only that we replaced
    /// the default disposition.
    #[test]
    fn install_replaces_default_disposition_for_bus_and_segv() {
        // Save the current handler so we can restore it at the end.
        // Tests inside the same binary share the process's signal
        // table; we don't want to leave SA_RESETHAND'd handlers behind
        // for subsequent tests in the same run.
        let mut prev_bus: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut prev_segv: libc::sigaction = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigaction(libc::SIGBUS, std::ptr::null(), &mut prev_bus);
            libc::sigaction(libc::SIGSEGV, std::ptr::null(), &mut prev_segv);
        }

        install_chip_fault_handler().expect("install should succeed");

        for sig in [libc::SIGBUS, libc::SIGSEGV] {
            let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::sigaction(sig, std::ptr::null(), &mut current) };
            assert_eq!(rc, 0, "sigaction query should succeed for sig={}", sig);
            // SIG_DFL is 0 and SIG_IGN is 1 on every Linux libc.
            assert!(
                current.sa_sigaction != libc::SIG_DFL && current.sa_sigaction != libc::SIG_IGN,
                "sig={} still has default/ignore disposition after install",
                sig
            );
            assert!(
                current.sa_flags & libc::SA_RESETHAND != 0,
                "sig={} missing SA_RESETHAND",
                sig
            );
        }

        // Restore so other tests in the same process don't inherit
        // the chip-fault handler.
        unsafe {
            libc::sigaction(libc::SIGBUS, &prev_bus, std::ptr::null_mut());
            libc::sigaction(libc::SIGSEGV, &prev_segv, std::ptr::null_mut());
        }
    }

    /// The pre-formatted message buffers must end with a newline so the
    /// O_DSYNC log fd flushes a complete line. Easy to break if the
    /// constants are ever rewritten.
    #[test]
    fn message_buffers_are_newline_terminated() {
        for (name, msg) in [
            ("SIGBUS", MSG_SIGBUS),
            ("SIGSEGV", MSG_SIGSEGV),
            ("UNKNOWN", MSG_UNKNOWN),
        ] {
            assert_eq!(
                msg.last(),
                Some(&b'\n'),
                "{} message must end with newline",
                name
            );
        }
    }
}
