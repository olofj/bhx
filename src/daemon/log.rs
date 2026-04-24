// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Tiny timestamped logging helper for daemon-side messages.
//!
//! [`dlog!`] is a drop-in for `eprintln!` that prepends a compact
//! `[YYYY-MM-DDTHH:MM:SS.fffZ pid=… tid=…]` tag. When the daemon's log file
//! is opened with `O_DSYNC` (see `runner::start`), each such line is flushed
//! to disk before the macro returns — so messages survive host machine-check
//! crashes that would otherwise lose kernel-buffered stderr.
//!
//! The macro formats the whole line via one `eprintln!` call so concurrent
//! threads don't interleave their output mid-line.

/// Build a `[timestamp pid=… tid=…]` prefix string. Uses `CLOCK_REALTIME` +
/// `gmtime_r` directly to avoid pulling in a chrono/time dependency just for
/// log formatting.
pub fn ts_prefix() -> String {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts);
    }
    let mut tm = libc::tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    unsafe {
        libc::gmtime_r(&ts.tv_sec, &mut tm);
    }
    let ms = ts.tv_nsec / 1_000_000;
    let pid = std::process::id();
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    format!(
        "[{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z pid={} tid={}]",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        ms,
        pid,
        tid
    )
}

/// Log a timestamped line to stderr. Drop-in for `eprintln!`.
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {{
        let body = format!($($arg)*);
        eprintln!("{} {}", $crate::daemon::log::ts_prefix(), body);
    }};
}
