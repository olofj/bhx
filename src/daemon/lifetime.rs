// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Daemon lifetime management.
//!
//! Per-card runtime layout:
//! ```text
//! $XDG_RUNTIME_DIR/tt-bh-linux/<card>/
//!     sock    — unix control socket (created by daemon, unlinked on stop)
//!     pid     — pidfile + exclusivity flock (LOCK_EX|LOCK_NB)
//!     log     — stdout/stderr after daemonization
//! ```
//! Falls back to `/tmp/tt-bh-linux-$UID/<card>/` when `$XDG_RUNTIME_DIR` is
//! not set (common on sshd+pam setups that don't provision one).
//!
//! The daemon acquires an exclusive flock on `pid` as its single-instance
//! check. If the flock is held we bail out with a "daemon already running"
//! error; if the file exists but the flock is free (previous daemon crashed
//! without cleanup), we take the lock, unlink any leftover `sock`, and
//! proceed as a clean restart.
//!
//! `stop` / `status` helpers in this module are designed to be callable from
//! a short-lived client process without bringing up the full daemon runtime.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Resolve the per-card runtime directory. Does not create it.
pub fn runtime_dir(card: u32) -> PathBuf {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => {
            // Fall back to /tmp/tt-bh-linux-$UID (per-user to avoid collisions
            // on shared hosts). Creating a world-writable dir would be a
            // security hole; we set mode 0700 in `ensure_runtime_dir`.
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/tt-bh-linux-{}", uid))
        }
    };
    let mut path = base;
    if !path.ends_with("tt-bh-linux") {
        path.push("tt-bh-linux");
    }
    path.push(card.to_string());
    path
}

pub fn socket_path(card: u32) -> PathBuf {
    let mut p = runtime_dir(card);
    p.push("sock");
    p
}

pub fn pidfile_path(card: u32) -> PathBuf {
    let mut p = runtime_dir(card);
    p.push("pid");
    p
}

pub fn log_path(card: u32) -> PathBuf {
    let mut p = runtime_dir(card);
    p.push("log");
    p
}

/// Path to the file that records the Tensix tile this daemon has
/// reserved for its virtio engine. Operators or wrapper scripts that
/// run tt-metal alongside the daemon read this to exclude the tile
/// from `DispatchCoreConfig`. Format: a single line `<x> <y>\n` in
/// NOC0-logical coords; absent if bring-up hasn't picked yet. See #74.
pub fn reserved_tile_path(card: u32) -> PathBuf {
    let mut p = runtime_dir(card);
    p.push("reserved-tile");
    p
}

/// Create the runtime directory if missing, with mode 0700. Idempotent.
pub fn ensure_runtime_dir(card: u32) -> io::Result<PathBuf> {
    let dir = runtime_dir(card);
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

/// Owns an exclusive flock on the pidfile. Dropping it releases the lock;
/// the file itself is *not* unlinked (so stale-pid recovery can distinguish
/// "left over from a crash" from "never started").
pub struct PidfileGuard {
    _file: File,
    path: PathBuf,
}

impl PidfileGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PidfileGuard {
    fn drop(&mut self) {
        // Leaving the file in place is deliberate — see module docs for the
        // stale-pidfile recovery path. The flock is released automatically
        // when `_file` is closed.
    }
}

/// Attempt to acquire the per-card pidfile.
///
/// On contention, returns `io::Error::new(ErrorKind::AlreadyExists, …)` —
/// the kind carries semantic meaning here: `runner::start` matches on
/// `AlreadyExists` to attach the running pid to its user-facing error
/// message. This is the *only* place in the crate that uses a non-`Other`
/// `ErrorKind` deliberately; everywhere else routes through
/// `crate::Error` (and the `From<crate::Error> for io::Error` bridge).
pub fn acquire_pidfile(card: u32) -> io::Result<PidfileGuard> {
    ensure_runtime_dir(card)?;
    let path = pidfile_path(card);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "daemon already running (pidfile locked)",
            ))
        } else {
            Err(e)
        };
    }

    // Replace the file contents with our pid now that we hold the lock.
    let pid = std::process::id();
    let mut f = &file;
    f.set_len(0)?;
    use std::io::Seek;
    f.rewind()?;
    writeln!(f, "{}", pid)?;
    f.flush()?;

    Ok(PidfileGuard { _file: file, path })
}

/// Read the pid from the pidfile (whether or not the flock is held).
pub fn read_pid(card: u32) -> io::Result<Option<u32>> {
    let path = pidfile_path(card);
    let mut f = match File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    match s.trim().parse::<u32>() {
        Ok(p) => Ok(Some(p)),
        Err(_) => Ok(None), // Treat garbage as "no pid".
    }
}

/// Is a daemon currently running for this card? Checks by trying to take
/// the flock non-blockingly; if it fails with EWOULDBLOCK, someone else
/// holds it.
pub fn is_running(card: u32) -> bool {
    let path = pidfile_path(card);
    let file = match File::open(&path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // We got the lock — nobody was holding it. Release immediately.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        false
    } else {
        true
    }
}

/// Stop the daemon: SIGTERM, wait up to 5 s for graceful exit, SIGKILL,
/// unlink leftovers. Returns Ok(()) if the daemon is stopped afterwards
/// (even if it wasn't running to start with — idempotent).
pub fn stop(card: u32) -> io::Result<()> {
    if !is_running(card) {
        // Clean up stale files if any exist.
        let _ = fs::remove_file(socket_path(card));
        let _ = fs::remove_file(pidfile_path(card));
        let _ = fs::remove_file(reserved_tile_path(card));
        return Ok(());
    }
    let pid = match read_pid(card)? {
        Some(p) => p,
        None => {
            return Err(crate::Error::internal("daemon is running but pidfile is empty").into())
        }
    };
    let pid_i = pid as i32;
    unsafe { libc::kill(pid_i, libc::SIGTERM) };

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !is_running(card) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if is_running(card) {
        unsafe { libc::kill(pid_i, libc::SIGKILL) };
        // Give it a moment to die before we clean up.
        std::thread::sleep(Duration::from_millis(200));
    }

    let _ = fs::remove_file(socket_path(card));
    let _ = fs::remove_file(pidfile_path(card));
    let _ = fs::remove_file(reserved_tile_path(card));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    // Tests here mutate XDG_RUNTIME_DIR, which is process-global. Serialize
    // them with a test-only mutex so parallel cargo-test threads don't race.
    // (std::env::set_var is unsafe-by-convention in multi-threaded code.)
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    struct EnvGuard {
        prev: Option<std::ffi::OsString>,
        _lock: MutexGuard<'static, ()>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }
    fn set_xdg(path: &Path) -> EnvGuard {
        let lock = env_lock();
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", path);
        EnvGuard { prev, _lock: lock }
    }
    fn unset_xdg() -> EnvGuard {
        let lock = env_lock();
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::remove_var("XDG_RUNTIME_DIR");
        EnvGuard { prev, _lock: lock }
    }

    #[test]
    fn runtime_dir_uses_xdg() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = set_xdg(tmp.path());
        let p = runtime_dir(0);
        assert!(p.starts_with(tmp.path()));
        assert!(p.ends_with(Path::new("tt-bh-linux/0")));
    }

    #[test]
    fn runtime_dir_falls_back_without_xdg() {
        let _g = unset_xdg();
        let p = runtime_dir(2);
        let uid = unsafe { libc::getuid() };
        let expected = format!("/tmp/tt-bh-linux-{}/tt-bh-linux/2", uid);
        assert_eq!(p, Path::new(&expected));
    }

    #[test]
    fn acquire_pidfile_writes_pid_and_is_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = set_xdg(tmp.path());
        let card = 0;

        let guard = acquire_pidfile(card).unwrap();
        assert!(guard.path().exists());

        // Second attempt from same process fails: flock is exclusive per-fd
        // but Linux honors it per-process too when a second *open* tries to
        // lock it (LOCK_EX|LOCK_NB returns EWOULDBLOCK).
        let err = match acquire_pidfile(card) {
            Err(e) => e,
            Ok(_) => panic!("expected pidfile acquisition to fail"),
        };
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

        let pid = read_pid(card).unwrap();
        assert_eq!(pid, Some(std::process::id()));

        drop(guard);
        // After dropping, the lock should be released.
        let _ = acquire_pidfile(card).unwrap();
    }

    #[test]
    fn is_running_reflects_lock_state() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = set_xdg(tmp.path());
        let card = 1;

        assert!(!is_running(card), "no pidfile yet");
        let guard = acquire_pidfile(card).unwrap();
        assert!(is_running(card), "lock held");
        drop(guard);
        // Pidfile still exists but flock is free.
        assert!(!is_running(card));
    }

    #[test]
    fn stop_cleans_up_stale_files() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = set_xdg(tmp.path());
        let card = 3;
        ensure_runtime_dir(card).unwrap();
        // Write fake stale files by hand; no daemon actually running.
        fs::write(pidfile_path(card), "99999\n").unwrap();
        fs::write(socket_path(card), "").unwrap();

        stop(card).unwrap();
        assert!(!pidfile_path(card).exists());
        assert!(!socket_path(card).exists());
    }

    #[test]
    fn stop_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = set_xdg(tmp.path());
        let card = 0;
        // No daemon, no files.
        stop(card).unwrap();
        stop(card).unwrap();
    }
}
