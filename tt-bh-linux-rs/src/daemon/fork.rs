// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! POSIX double-fork + stdio redirect, tailored for our daemon.
//!
//! Replaces the `daemonize` crate. The contract:
//!
//! - Returns [`Outcome::Parent`] in the original process. Caller exits 0.
//! - Returns [`Outcome::Child`] only in the *grand*-child after both
//!   forks have completed, the session has been detached, cwd is set,
//!   stdout/stderr point at the log fd, and stdin is `/dev/null`.
//!   Caller in the grand-child re-acquires the pidfile and runs the
//!   server loop.
//!
//! The shape mirrors the slice of `daemonize::Daemonize` we used (set
//! working dir, umask, stdout, stderr; call `execute()`; match on
//! `Parent`/`Child`). We deliberately don't re-implement the rest
//! (user/group switching, root chroot, pidfile management) — we never
//! used those.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

pub enum Outcome {
    /// Original process. Returned to the caller of `double_fork`. The
    /// intermediate-parent (between the two forks) and the daemonize
    /// glue exit silently inside `double_fork` — they never bubble
    /// back here.
    Parent,
    /// Grand-child after both forks. Caller continues into the server
    /// loop and exits via `std::process::exit` once the loop returns.
    Child,
}

/// Daemonize the calling process via the canonical double-fork pattern.
///
/// Steps performed in the grand-child before returning `Outcome::Child`:
///
/// 1. `setsid` — detach from controlling tty / process group.
/// 2. Second `fork` — the intermediate parent exits via `_exit(0)` so
///    `init` (PID 1) reaps the grand-child rather than us.
/// 3. `chdir(working_directory)` — release any held mount under cwd
///    so the operator can unmount things later.
/// 4. `umask(umask_octal)` — set the file-creation mask for log /
///    pidfile writes.
/// 5. `dup2(stdout_fd, 1)` and `dup2(stderr_fd, 2)` — redirect stdio
///    to the supplied log files.
/// 6. Open `/dev/null` and `dup2` over fd 0 so reads from stdin
///    immediately see EOF rather than blocking on the inherited tty.
///
/// `stdout` and `stderr` are taken by value (consumed) because the
/// kernel duplicates the underlying open-file-description; closing
/// our handle in the grand-child is the right thing.
///
/// On failure: returns the underlying `io::Error`. If a syscall fails
/// inside the grand-child (e.g., `dup2` on stdout), the grand-child
/// exits with status 1 — the parent already returned `Ok(Parent)` so
/// it doesn't see the failure directly; the operator notices via the
/// missing pidfile.
pub fn double_fork(
    working_directory: &Path,
    umask_octal: u32,
    stdout: File,
    stderr: File,
) -> io::Result<Outcome> {
    // First fork: the parent returns Outcome::Parent immediately.
    // SAFETY: fork() is async-signal-safe and we're in single-threaded
    // pre-daemonize startup; no Rust invariants are crossed.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid > 0 {
        // Original process — daemonize crate's "Parent" outcome.
        return Ok(Outcome::Parent);
    }

    // Intermediate child: detach, fork again, then exit so the
    // grand-child gets re-parented to PID 1. Any failure here is
    // fatal; the intermediate process exits non-zero so the grand-
    // child never starts.
    if unsafe { libc::setsid() } < 0 {
        unsafe { libc::_exit(1) };
    }
    let pid2 = unsafe { libc::fork() };
    if pid2 < 0 {
        unsafe { libc::_exit(1) };
    }
    if pid2 > 0 {
        // Intermediate parent. _exit (not exit) to skip atexit
        // handlers; we own no state worth flushing.
        unsafe { libc::_exit(0) };
    }

    // Grand-child. Past this point any error path uses _exit(1)
    // because the operator's ack came from the original process and
    // we can't surface a Result back through fork().
    if unsafe { libc::chdir(path_to_cstr(working_directory)?.as_ptr()) } < 0 {
        unsafe { libc::_exit(1) };
    }
    unsafe { libc::umask(umask_octal as libc::mode_t) };

    // Redirect stdout/stderr. dup2 atomically replaces the destination
    // fd; if either dup fails the daemon exits non-zero (no log to
    // write to, so the operator notices via the missing pidfile).
    if unsafe { libc::dup2(stdout.as_raw_fd(), libc::STDOUT_FILENO) } < 0 {
        unsafe { libc::_exit(1) };
    }
    if unsafe { libc::dup2(stderr.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        unsafe { libc::_exit(1) };
    }
    // Drop the source fds; the dup'd copies on 1/2 are what the
    // daemon writes through.
    drop(stdout);
    drop(stderr);

    // Replace stdin with /dev/null so any inherited tty fd is gone
    // and reads from fd 0 immediately see EOF.
    let devnull_path = b"/dev/null\0";
    let devnull = unsafe {
        libc::open(
            devnull_path.as_ptr() as *const libc::c_char,
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if devnull < 0 {
        unsafe { libc::_exit(1) };
    }
    if unsafe { libc::dup2(devnull, libc::STDIN_FILENO) } < 0 {
        unsafe { libc::_exit(1) };
    }
    if devnull != libc::STDIN_FILENO {
        unsafe { libc::close(devnull) };
    }

    Ok(Outcome::Child)
}

/// Convert a Path to a C string for libc syscalls. Fails if the path
/// contains an embedded NUL.
fn path_to_cstr(p: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains NUL: {}", e),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_to_cstr_round_trips_normal_paths() {
        let cs = path_to_cstr(Path::new("/var/log/foo")).unwrap();
        assert_eq!(cs.to_bytes(), b"/var/log/foo");
    }

    #[test]
    fn path_to_cstr_rejects_embedded_nul() {
        // Construct an OsString with an embedded NUL via raw bytes.
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let bad: OsString = OsString::from_vec(b"/etc/\0bad".to_vec());
        let path: &Path = bad.as_ref();
        let err = path_to_cstr(path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    // Note: we don't unit-test double_fork itself because it forks the
    // test runner, which (a) confuses cargo's harness and (b) leaves
    // half-detached processes around. The hardware soaks
    // (soak_warm_resume, soak_kill_recovery) exercise the full path
    // and were the basis for verifying this rewrite.
}
