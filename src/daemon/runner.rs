// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! `daemon { start, stop, status, restart, logs }` subcommand implementations.
//!
//! `start` opens the control socket *before* forking so the parent can exit
//! with the confidence that a subsequent `connect` will find the daemon
//! listening. Uses the `daemonize` crate for the fork + setsid + stdio
//! redirect; we manage the pidfile ourselves because our stop/status helpers
//! in `lifetime.rs` already speak that format.

use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use crate::daemon::lifetime;
use crate::daemon::server;

pub struct StartOpts {
    pub card: u32,
    pub foreground: bool,
    /// If set, redirect daemon stdout/stderr here instead of the default
    /// tmpfs log path. Written to `<runtime_dir>/logpath` so `daemon logs`
    /// knows where to tail. Opened with `O_DSYNC` so lines survive a host
    /// crash (trading throughput for durability — see `runner::open_log`).
    pub log_file: Option<PathBuf>,
}

/// Resolve the caller-supplied log path against the current working dir and
/// open it with `O_APPEND | O_DSYNC`. Daemonize chdirs to `/`, so any
/// relative path has to be made absolute beforehand. `O_DSYNC` forces each
/// write to hit disk synchronously — necessary because we're specifically
/// trying to capture logs across host crashes.
fn open_log(path: &PathBuf) -> io::Result<(PathBuf, std::fs::File)> {
    let abs = if path.is_absolute() {
        path.clone()
    } else {
        std::env::current_dir()?.join(path)
    };
    // Make sure the parent directory exists — a typo'd subdir would
    // otherwise surface only after daemonize has forked.
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_DSYNC)
        .open(&abs)?;
    Ok((abs, file))
}

pub fn start(opts: StartOpts) -> io::Result<()> {
    let card = opts.card;
    lifetime::ensure_runtime_dir(card)?;
    let sock_path = lifetime::socket_path(card);
    let pid_path = lifetime::pidfile_path(card);

    let log_path = resolve_log_path_with_override(card, opts.log_file.as_deref())?;

    // Stale socket cleanup: if the daemon crashed without removing its
    // socket file, the new bind would fail with EADDRINUSE.
    if !lifetime::is_running(card) {
        let _ = std::fs::remove_file(&sock_path);
    }

    let pid_guard = acquire_pidfile_or_already_running(card, &sock_path)?;
    let listener = UnixListener::bind(&sock_path)?;
    eprintln!(
        "[daemon] card {} listening on {} (log: {})",
        card,
        sock_path.display(),
        log_path.display()
    );

    if opts.foreground {
        run_foreground(card, listener, pid_guard, &pid_path)
    } else {
        run_daemonized(card, listener, &sock_path, &pid_path, &log_path, pid_guard)
    }
}

/// Pick the log path: either the operator's explicit `--log-file`, or
/// the per-card default under the runtime directory. For an explicit
/// path, persist the absolute location to a sidecar so `daemon logs`
/// can locate it without re-parsing the original CLI args.
fn resolve_log_path_with_override(card: u32, override_path: Option<&Path>) -> io::Result<PathBuf> {
    let runtime_dir = lifetime::runtime_dir(card);
    let sidecar = runtime_dir.join("logpath");
    match override_path {
        Some(p) => {
            // Test-open up front so we fail fast on bad paths before
            // anything else touches the runtime dir; the real open for
            // the daemonize handoff happens later via `open_log` again.
            let (abs, _f) = open_log(&p.to_path_buf())?;
            let _ = std::fs::write(&sidecar, format!("{}\n", abs.display()));
            Ok(abs)
        }
        None => {
            // Clear any stale sidecar from a previous override-based start.
            let _ = std::fs::remove_file(&sidecar);
            Ok(lifetime::log_path(card))
        }
    }
}

/// Acquire the pidfile flock, converting the `AlreadyExists` outcome into
/// a user-friendly "daemon already running for card N (pid …)" message.
fn acquire_pidfile_or_already_running(
    card: u32,
    sock_path: &Path,
) -> io::Result<lifetime::PidfileGuard> {
    lifetime::acquire_pidfile(card).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            let existing = lifetime::read_pid(card).ok().flatten();
            io::Error::other(format!(
                "daemon already running for card {}{} (sock: {})",
                card,
                existing
                    .map(|p| format!(" (pid {})", p))
                    .unwrap_or_default(),
                sock_path.display()
            ))
        } else {
            e
        }
    })
}

/// Foreground mode: serve directly in this process. The pidfile guard
/// is held across the serve loop and dropped on the way out so the
/// pidfile + flock are released cleanly even on Ctrl-C.
fn run_foreground(
    card: u32,
    listener: UnixListener,
    pid_guard: lifetime::PidfileGuard,
    pid_path: &Path,
) -> io::Result<()> {
    server::serve(card, listener)?;
    drop(pid_guard);
    let _ = std::fs::remove_file(pid_path);
    Ok(())
}

/// Background mode: double-fork via the `daemonize` crate, then serve
/// in the grand-child. Parent returns after the fork; the grand-child
/// re-acquires the pidfile flock and runs `serve` to completion.
///
/// We use `daemonize` for the fork + setsid + fork + chdir + umask +
/// stdio redirect, but skip its pid_file machinery so our own pidfile
/// semantics (flock + stale-recovery) stay authoritative.
///
/// The log fd is opened with O_DSYNC via `open_log` so each stderr line
/// is durably on disk before the write() returns. Matters because the
/// scenarios we most want logs from are host machine-check crashes
/// where tmpfs and pending page-cache writes are gone.
fn run_daemonized(
    card: u32,
    listener: UnixListener,
    sock_path: &Path,
    pid_path: &Path,
    log_path: &Path,
    pid_guard: lifetime::PidfileGuard,
) -> io::Result<()> {
    let (_abs, log_out) = open_log(&log_path.to_path_buf())?;
    let log_err = log_out.try_clone()?;

    // Drop pid_guard BEFORE daemonize: the daemonize crate forks, and the
    // child re-acquires the pidfile flock below. If we held the guard across
    // the fork, the child would inherit the flock (good), but the parent
    // would also keep it until process exit (the fork-exit is nearly
    // instant, but let's not take the risk).
    drop(pid_guard);

    let daemonize = daemonize::Daemonize::new()
        .working_directory("/")
        .umask(0o027)
        .stdout(log_out)
        .stderr(log_err);

    match daemonize.execute() {
        daemonize::Outcome::Parent(p) => match p {
            Ok(_) => {
                eprintln!(
                    "[daemon] started for card {} (pid will be in {})",
                    card,
                    pid_path.display()
                );
                return Ok(());
            }
            Err(e) => return Err(io::Error::other(format!("daemonize failed: {}", e))),
        },
        daemonize::Outcome::Child(c) => {
            c.map_err(|e| io::Error::other(format!("daemonize child failed: {}", e)))?;
        }
    }

    // Grand-child: re-acquire pidfile and run the server loop.
    let _pid_guard = lifetime::acquire_pidfile(card)?;
    if let Err(e) = server::serve(card, listener) {
        eprintln!("[daemon] fatal: {}", e);
    }
    let _ = std::fs::remove_file(sock_path);
    let _ = std::fs::remove_file(pid_path);
    std::process::exit(0);
}

pub fn stop(card: u32) -> io::Result<()> {
    lifetime::stop(card)
}

pub fn restart(card: u32, foreground: bool, log_file: Option<PathBuf>) -> io::Result<()> {
    stop(card)?;
    start(StartOpts {
        card,
        foreground,
        log_file,
    })
}

pub fn status(card: u32) -> io::Result<()> {
    if !lifetime::is_running(card) {
        println!("daemon: not running for card {}", card);
        return Ok(());
    }
    // RPC to the daemon for a richer status payload.
    match crate::daemon::client::connect(card) {
        Ok(mut sock) => match crate::daemon::client::status(&mut sock) {
            Ok(p) => {
                println!(
                    "daemon: running (card {}, pid {}, uptime {}s, sock {})",
                    card,
                    p.pid,
                    p.uptime_secs,
                    lifetime::socket_path(card).display()
                );
                for l in &p.l2cpus {
                    let disk = l.disk.as_deref().unwrap_or("-");
                    let net = if l.net { "y" } else { "-" };
                    println!(
                        "  l2cpu {}: {:?} disk={} net={} clients={}",
                        l.idx, l.state, disk, net, l.clients
                    );
                }
                Ok(())
            }
            Err(e) => {
                println!("daemon: running but RPC failed ({})", e);
                Ok(())
            }
        },
        Err(e) => {
            println!("daemon: lockfile held but socket unreachable ({})", e);
            Ok(())
        }
    }
}

pub struct LogsOpts {
    pub card: u32,
    pub follow: bool,
    pub lines: usize,
}

/// If the user started the daemon with `--log-file`, we persisted the path
/// in a sidecar inside the runtime dir. Fall back to the default tmpfs path
/// when no sidecar is present.
fn resolve_log_path(card: u32) -> PathBuf {
    let sidecar = lifetime::runtime_dir(card).join("logpath");
    if let Ok(s) = std::fs::read_to_string(&sidecar) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    lifetime::log_path(card)
}

pub fn logs(opts: LogsOpts) -> io::Result<()> {
    let path: PathBuf = resolve_log_path(opts.card);
    if !path.exists() {
        return Err(io::Error::other(format!(
            "no log file at {}",
            path.display()
        )));
    }
    // Print last `lines` lines.
    let mut file = std::fs::File::open(&path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let tail: Vec<&str> = text.lines().rev().take(opts.lines).collect();
    for line in tail.into_iter().rev() {
        let _ = writeln!(io::stdout(), "{}", line);
    }
    if !opts.follow {
        return Ok(());
    }
    // Naive follow: seek to end, poll with sleep.
    use std::io::{Seek, SeekFrom};
    let mut follow_file = std::fs::File::open(&path)?;
    follow_file.seek(SeekFrom::End(0))?;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = follow_file.read(&mut chunk)?;
        if n > 0 {
            io::stdout().write_all(&chunk[..n])?;
            io::stdout().flush()?;
        } else {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
}
