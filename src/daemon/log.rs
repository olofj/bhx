// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Tiny timestamped logging helper for daemon-side messages.
//!
//! [`dlog!`] is a drop-in for `eprintln!` that prepends a compact
//! `[YYYY-MM-DDTHH:MM:SS.fff±HH:MM pid=… tid=…]` tag (the offset renders as
//! `Z` when the local zone is UTC). When the daemon's log file is opened
//! with `O_DSYNC` (see `runner::start`), each such line is flushed to disk
//! before the macro returns — so messages survive host machine-check crashes
//! that would otherwise lose kernel-buffered stderr.
//!
//! The macro formats the whole line via one `eprintln!` call so concurrent
//! threads don't interleave their output mid-line.

/// Build a `[timestamp pid=… tid=…]` prefix string. Uses `CLOCK_REALTIME` +
/// `localtime_r` directly to avoid pulling in a chrono/time dependency just
/// for log formatting. The offset suffix is `Z` when `tm_gmtoff == 0`,
/// otherwise `±HH:MM`.
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
        libc::localtime_r(&ts.tv_sec, &mut tm);
    }
    let ms = ts.tv_nsec / 1_000_000;
    let pid = std::process::id();
    let tid = unsafe { libc::syscall(libc::SYS_gettid) };
    let offset = format_offset(tm.tm_gmtoff);
    format!(
        "[{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{} pid={} tid={}]",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        ms,
        offset,
        pid,
        tid
    )
}

/// Format a UTC offset (seconds east of UTC, as in `tm::tm_gmtoff`) as `Z`
/// when zero, otherwise `±HH:MM`. Sub-minute offsets are truncated, matching
/// every real-world IANA zone (none has sub-minute resolution since 1972).
fn format_offset(gmtoff_secs: libc::c_long) -> String {
    if gmtoff_secs == 0 {
        return "Z".to_string();
    }
    let (sign, abs) = if gmtoff_secs < 0 {
        ('-', -gmtoff_secs)
    } else {
        ('+', gmtoff_secs)
    };
    let hours = abs / 3600;
    let minutes = (abs % 3600) / 60;
    format!("{}{:02}:{:02}", sign, hours, minutes)
}

/// Log a timestamped line to stderr. Drop-in for `eprintln!`.
#[macro_export]
macro_rules! dlog {
    ($($arg:tt)*) => {{
        let body = format!($($arg)*);
        eprintln!("{} {}", $crate::daemon::log::ts_prefix(), body);
    }};
}

#[cfg(test)]
mod tests {
    use super::format_offset;

    #[test]
    fn utc_renders_as_z() {
        assert_eq!(format_offset(0), "Z");
    }

    #[test]
    fn positive_offsets() {
        assert_eq!(format_offset(2 * 3600), "+02:00");
        assert_eq!(format_offset(5 * 3600 + 30 * 60), "+05:30");
        assert_eq!(format_offset(14 * 3600), "+14:00");
    }

    #[test]
    fn negative_offsets() {
        assert_eq!(format_offset(-8 * 3600), "-08:00");
        assert_eq!(format_offset(-(3 * 3600 + 30 * 60)), "-03:30");
    }
}
