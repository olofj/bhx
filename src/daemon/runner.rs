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
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use crate::daemon::lifetime;
use crate::daemon::server;

pub struct StartOpts {
    pub card: u32,
    pub foreground: bool,
}

pub fn start(opts: StartOpts) -> io::Result<()> {
    let card = opts.card;
    lifetime::ensure_runtime_dir(card)?;
    let sock_path = lifetime::socket_path(card);
    let pid_path = lifetime::pidfile_path(card);
    let log_path = lifetime::log_path(card);

    // If a stale socket file exists (daemon crashed without cleanup), remove
    // it so the bind succeeds.
    if !lifetime::is_running(card) {
        let _ = std::fs::remove_file(&sock_path);
    }

    // Acquiring the flock here also catches the "already running" case early.
    let pid_guard = lifetime::acquire_pidfile(card).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            let existing = lifetime::read_pid(card).ok().flatten();
            io::Error::other(format!(
                "daemon already running for card {}{} (sock: {})",
                card,
                existing.map(|p| format!(" (pid {})", p)).unwrap_or_default(),
                sock_path.display()
            ))
        } else {
            e
        }
    })?;

    let listener = UnixListener::bind(&sock_path)?;
    eprintln!(
        "[daemon] card {} listening on {} (log: {})",
        card,
        sock_path.display(),
        log_path.display()
    );

    if opts.foreground {
        server::serve(card, listener)?;
        drop(pid_guard);
        let _ = std::fs::remove_file(&pid_path);
        return Ok(());
    }

    // Background mode: daemonize, then serve. We close our side of the
    // listener inherit path by dropping `pid_guard` and `listener` after the
    // fork — the child inherits both fds through the double-fork.
    // The `daemonize` crate does fork + setsid + fork + chdir + umask + stdio
    // redirect. We skip its pid_file machinery so our own pidfile semantics
    // (flock + stale-recovery) stay authoritative.
    let log_out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
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

    // We are now the grand-child; parent has returned from this function.
    // Re-acquire pidfile and run the server loop.
    let _pid_guard = lifetime::acquire_pidfile(card)?;
    if let Err(e) = server::serve(card, listener) {
        eprintln!("[daemon] fatal: {}", e);
    }
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&pid_path);
    std::process::exit(0);
}

pub fn stop(card: u32) -> io::Result<()> {
    lifetime::stop(card)
}

pub fn restart(card: u32, foreground: bool) -> io::Result<()> {
    stop(card)?;
    start(StartOpts { card, foreground })
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

pub fn logs(opts: LogsOpts) -> io::Result<()> {
    let path: PathBuf = lifetime::log_path(opts.card);
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
