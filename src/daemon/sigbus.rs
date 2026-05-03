// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Last-resort signal handler for chip-access faults (#129, #149).
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
//! and `_exit`s with `128 + sig`. The handler walks a snapshot of
//! known chip-mapped VA ranges (registered by `L2Cpu::new`,
//! `SharedChip::new`, `TensixTile::new`) to discriminate "fault inside
//! a chip mapping → likely external invalidation" from "fault outside
//! any chip mapping → likely a daemon-side bug" (#149). The
//! discriminated path includes `si_addr` formatted as hex so the
//! operator can grep for it in disassembly.
//!
//! ## Async-signal-safety contract
//!
//! Allowed inside [`handle_chip_fault`]:
//! - [`libc::write`] against a byte buffer.
//! - [`libc::_exit`].
//! - [`AtomicPtr::load`] on a heap-allocated `Box<[ChipRange]>` that's
//!   never freed (publishers leak old slices on update).
//! - The hex formatter [`write_hex_u64`], which has no allocation,
//!   no locking, and pure stack-only state.
//!
//! Forbidden:
//! - `dlog!`, `eprintln!`, `format!`, `Mutex`, `Box`, `String`,
//!   `Vec::push`, anything that may allocate or block.
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
use std::sync::atomic::{AtomicPtr, Ordering};

/// Pre-formatted message tails. Each path appends a newline at the
/// final write call so the O_DSYNC log fd flushes a complete line.
const MSG_SIGBUS_CHIP: &[u8] =
    b"fatal: SIGBUS during chip access (tt-smi -r? PCIe link lost? Card pulled?) si_addr=0x";
const MSG_SIGSEGV_CHIP: &[u8] = b"fatal: SIGSEGV during chip access si_addr=0x";
const MSG_SIGBUS_NONCHIP: &[u8] =
    b"fatal: SIGBUS in daemon process (likely a bug, not chip): si_addr=0x";
const MSG_SIGSEGV_NONCHIP: &[u8] =
    b"fatal: SIGSEGV in daemon process (likely a bug, not chip): si_addr=0x";
const MSG_UNKNOWN: &[u8] = b"fatal: unexpected signal in chip-fault handler\n";

/// One registered VA range that may legitimately fault on chip
/// invalidation. Kept simple so the handler can iterate without any
/// dependency on heap allocators or atomic field updates.
#[derive(Clone, Copy)]
pub struct ChipRange {
    /// First host VA the range covers (inclusive).
    pub start: usize,
    /// Length in bytes. The range covers `[start, start + len)`.
    pub len: usize,
}

/// Snapshot of registered ranges. Published as a leaked
/// `Box<[ChipRange]>` so the signal handler can read it without
/// blocking. On update we publish a fresh boxed slice and leak the
/// previous one — chip-range registration is rare (per-boot
/// L2Cpu::new etc., maybe a few dozen times in a daemon's lifetime),
/// so unbounded leaks are bounded in practice.
static CHIP_RANGES: AtomicPtr<&'static [ChipRange]> = AtomicPtr::new(std::ptr::null_mut());

/// Add a chip-mapped VA range to the snapshot. Lock-free for the
/// reader; the writer side serializes on a spin/CAS loop, fine for
/// the rare register-side cadence.
///
/// `start` is the host VA of the first byte; `len` is the byte count.
/// Best-effort: registration failure (out-of-memory) is silently
/// dropped — the worst that happens is a chip fault gets attributed
/// to "daemon bug" instead of "chip access".
pub fn register_chip_range(start: *mut u8, len: usize) {
    if len == 0 {
        return;
    }
    let new_entry = ChipRange {
        start: start as usize,
        len,
    };
    loop {
        let cur = CHIP_RANGES.load(Ordering::Acquire);
        let mut next: Vec<ChipRange> = if cur.is_null() {
            Vec::with_capacity(1)
        } else {
            // Safety: `cur` was published by `Box::leak` from a slice;
            // the slice is immutable and outlives the daemon.
            let slice = unsafe { *cur };
            let mut v = Vec::with_capacity(slice.len() + 1);
            v.extend_from_slice(slice);
            v
        };
        next.push(new_entry);
        let leaked: &'static [ChipRange] = Box::leak(next.into_boxed_slice());
        let leaked_outer: &'static &'static [ChipRange] = Box::leak(Box::new(leaked));
        // CAS publish — if we lost a race, drop the new outer-box (the
        // inner slice still leaks, oh well — bounded total) and retry.
        match CHIP_RANGES.compare_exchange(
            cur,
            leaked_outer as *const _ as *mut _,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(_) => continue,
        }
    }
}

/// Remove a previously-registered range by its starting address. No-op
/// if the range isn't present (allows idempotent calls from `Drop`
/// paths that may run after a manual unregister). Same lock-free
/// publication shape as `register_chip_range`.
pub fn unregister_chip_range(start: *mut u8) {
    let target = start as usize;
    loop {
        let cur = CHIP_RANGES.load(Ordering::Acquire);
        if cur.is_null() {
            return;
        }
        let slice = unsafe { *cur };
        let mut v: Vec<ChipRange> = Vec::with_capacity(slice.len());
        let mut found = false;
        for r in slice {
            if r.start == target {
                found = true;
                continue;
            }
            v.push(*r);
        }
        if !found {
            return;
        }
        let leaked: &'static [ChipRange] = Box::leak(v.into_boxed_slice());
        let leaked_outer: &'static &'static [ChipRange] = Box::leak(Box::new(leaked));
        match CHIP_RANGES.compare_exchange(
            cur,
            leaked_outer as *const _ as *mut _,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(_) => continue,
        }
    }
}

/// Async-signal-safe check: is `addr` inside any registered chip
/// range? Pure read of `CHIP_RANGES`; safe to call from a signal
/// handler.
fn addr_is_in_chip_range(addr: usize) -> bool {
    let p = CHIP_RANGES.load(Ordering::Acquire);
    if p.is_null() {
        return false;
    }
    // Safety: see `register_chip_range`; the published pointer
    // outlives all readers.
    let slice = unsafe { *p };
    for r in slice {
        if addr >= r.start && addr < r.start.saturating_add(r.len) {
            return true;
        }
    }
    false
}

/// Format `value` as 16-char zero-padded lowercase hex into `buf`.
/// Returns the number of bytes written (always 16). Pure stack-only,
/// no allocation, async-signal-safe.
fn write_hex_u64(value: u64, buf: &mut [u8; 16]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, slot) in buf.iter_mut().enumerate() {
        let nibble = (value >> ((15 - i) * 4)) & 0xf;
        *slot = HEX[nibble as usize];
    }
    16
}

extern "C" fn handle_chip_fault(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    _ucontext: *mut libc::c_void,
) {
    // Async-signal-safe ALLOWED LIST: see module-level docs.
    // DO NOT add: dlog!, eprintln!, format!, mutex lock, std::panic, malloc, …

    let si_addr: usize = if info.is_null() {
        0
    } else {
        // Safety: kernel guarantees `info` points at a valid siginfo_t
        // for the lifetime of this handler invocation when SA_SIGINFO
        // is set in sa_flags. `si_addr` lives inside the union; libc's
        // accessor returns a `*mut c_void` for it.
        unsafe { (*info).si_addr() as usize }
    };

    let in_chip = addr_is_in_chip_range(si_addr);

    let (prefix, append_addr): (&[u8], bool) = match (sig, in_chip) {
        (libc::SIGBUS, true) => (MSG_SIGBUS_CHIP, true),
        (libc::SIGSEGV, true) => (MSG_SIGSEGV_CHIP, true),
        (libc::SIGBUS, false) => (MSG_SIGBUS_NONCHIP, true),
        (libc::SIGSEGV, false) => (MSG_SIGSEGV_NONCHIP, true),
        _ => (MSG_UNKNOWN, false),
    };

    // STDERR_FILENO is the daemonized process's log fd. Best-effort —
    // if a write fails (storage gone with the chip) we still _exit.
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            prefix.as_ptr() as *const libc::c_void,
            prefix.len(),
        );
        if append_addr {
            let mut hex = [0u8; 16];
            write_hex_u64(si_addr as u64, &mut hex);
            libc::write(
                libc::STDERR_FILENO,
                hex.as_ptr() as *const libc::c_void,
                hex.len(),
            );
            libc::write(
                libc::STDERR_FILENO,
                b"\n".as_ptr() as *const libc::c_void,
                1,
            );
        }
        libc::_exit(128 + sig);
    }
}

/// Install the chip-fault handler for SIGBUS + SIGSEGV. Idempotent —
/// later calls overwrite earlier ones.
pub fn install_chip_fault_handler() -> io::Result<()> {
    let mut act: libc::sigaction = unsafe { std::mem::zeroed() };
    // SA_SIGINFO so the handler receives `siginfo_t` and `ucontext`.
    // Bind the function item to a typed pointer first to keep clippy's
    // function_casts_as_integer happy.
    let handler_ptr: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
        handle_chip_fault;
    act.sa_sigaction = handler_ptr as usize;
    // SA_RESETHAND: a re-fault inside the handler restores the default
    // disposition for that signal, so the second hit core-dumps cleanly
    // instead of looping us through the handler again.
    act.sa_flags = libc::SA_RESETHAND | libc::SA_SIGINFO;
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

    /// Body of the install assertion — same checks the in-process
    /// version had, factored so the child process can run them.
    fn assert_handler_installed_for_bus_and_segv() {
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
            assert!(
                current.sa_flags & libc::SA_SIGINFO != 0,
                "sig={} missing SA_SIGINFO",
                sig
            );
        }
    }

    /// Calling `install_chip_fault_handler` registers SOMETHING for
    /// SIGBUS + SIGSEGV. We don't assert the handler pointer is exactly
    /// ours — that's brittle across build modes — only that we replaced
    /// the default disposition.
    ///
    /// Runs in a subprocess so the installed handler doesn't bleed into
    /// other parallel tests in the same binary (#150). The handler
    /// `_exit(128+sig)`s on first fault — without isolation, any other
    /// test that happens to SIGSEGV during the install window would
    /// silently kill the runner.
    #[test]
    fn install_replaces_default_disposition_for_bus_and_segv() {
        const CHILD_MARKER: &str = "BHX_SIGBUS_TEST_CHILD";

        if std::env::var(CHILD_MARKER).is_ok() {
            assert_handler_installed_for_bus_and_segv();
            std::process::exit(0);
        }

        let exe = std::env::current_exe().expect("current_exe");
        let status = std::process::Command::new(&exe)
            .args([
                "--exact",
                "daemon::sigbus::tests::install_replaces_default_disposition_for_bus_and_segv",
                "--nocapture",
            ])
            .env(CHILD_MARKER, "1")
            .status()
            .expect("spawn child test runner");
        assert!(
            status.success(),
            "child failed (exit code {:?})",
            status.code()
        );
    }

    /// Every prefix that the handler can write must either be
    /// newline-terminated already (UNKNOWN path) or end with `0x` so
    /// the hex+newline tail concatenates into a complete line. The
    /// handler appends `\n` after the hex bytes; a missing `0x` here
    /// would produce a garbled line.
    #[test]
    fn message_prefixes_have_correct_tails() {
        for (name, msg, want_0x) in [
            ("SIGBUS_CHIP", MSG_SIGBUS_CHIP, true),
            ("SIGSEGV_CHIP", MSG_SIGSEGV_CHIP, true),
            ("SIGBUS_NONCHIP", MSG_SIGBUS_NONCHIP, true),
            ("SIGSEGV_NONCHIP", MSG_SIGSEGV_NONCHIP, true),
            ("UNKNOWN", MSG_UNKNOWN, false),
        ] {
            if want_0x {
                assert!(
                    msg.ends_with(b"0x"),
                    "{} should end with '0x' so hex appends cleanly",
                    name
                );
            } else {
                assert_eq!(
                    msg.last(),
                    Some(&b'\n'),
                    "{} (no hex appended) must end with newline",
                    name
                );
            }
        }
    }

    #[test]
    fn write_hex_u64_zero() {
        let mut buf = [0u8; 16];
        let n = write_hex_u64(0, &mut buf);
        assert_eq!(n, 16);
        assert_eq!(&buf, b"0000000000000000");
    }

    #[test]
    fn write_hex_u64_max() {
        let mut buf = [0u8; 16];
        write_hex_u64(u64::MAX, &mut buf);
        assert_eq!(&buf, b"ffffffffffffffff");
    }

    #[test]
    fn write_hex_u64_arbitrary() {
        let mut buf = [0u8; 16];
        write_hex_u64(0xdeadbeefcafe1234, &mut buf);
        assert_eq!(&buf, b"deadbeefcafe1234");
    }

    /// Ranges register and unregister cleanly. Uses synthetic
    /// addresses that won't collide with anything the daemon
    /// genuinely registers in tests. Also exercises the
    /// `addr_is_in_chip_range` reader.
    #[test]
    fn chip_range_registration_round_trip() {
        // Synthetic high addresses unlikely to ever land in a real
        // mmap. `unregister` cleans up after the test so the static
        // doesn't leak entries that affect later tests.
        let base: *mut u8 = 0xf000_0000_0000usize as *mut u8;
        let len = 4096usize;
        register_chip_range(base, len);

        assert!(addr_is_in_chip_range(base as usize));
        assert!(addr_is_in_chip_range(base as usize + 100));
        assert!(addr_is_in_chip_range(base as usize + len - 1));
        assert!(!addr_is_in_chip_range(base as usize + len));
        assert!(!addr_is_in_chip_range(0xdead_dead_dead));

        unregister_chip_range(base);
        assert!(!addr_is_in_chip_range(base as usize));
    }
}
