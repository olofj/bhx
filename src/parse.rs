// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Operator-input parsers shared across the CLI and the profile schema.
//!
//! Pre-#143 each parser had two near-identical copies (one in `main.rs`
//! returning `io::Result`, one in `profile.rs` returning `crate::Result`)
//! plus a third disk-only `parse_size` in `image.rs`. Consolidating
//! here keeps the canonical implementation in one place and lets
//! `io::Result`-needing callers wrap with `.map_err(io::Error::from)`
//! at the boundary.

use crate::error::{Error, Result};

/// Parse an operator-friendly memory size string into a byte count.
///
/// Accepts plain integers (interpreted as bytes) and suffixed forms in
/// either SI (`KB`/`MB`/`GB`) or IEC binary (`KiB`/`MiB`/`GiB`)
/// notation. The number portion can carry a decimal point.
///
/// Examples:
///   - `"2GB"`   → 2_000_000_000
///   - `"2GiB"`  → 2_147_483_648
///   - `"1.5GiB"` → 1_610_612_736
///
/// Errors on empty input, NaN/Inf, non-positive, malformed, or
/// overflow-past-`u64::MAX` after the suffix multiply (#152).
pub fn parse_memory(s: &str) -> Result<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(Error::bad_request("empty memory value"));
    }
    let (num_part, mult) = if let Some(rest) = trimmed.strip_suffix("GiB") {
        (rest, 1u64 << 30)
    } else if let Some(rest) = trimmed.strip_suffix("MiB") {
        (rest, 1u64 << 20)
    } else if let Some(rest) = trimmed.strip_suffix("KiB") {
        (rest, 1u64 << 10)
    } else if let Some(rest) = trimmed.strip_suffix("GB") {
        (rest, 1_000_000_000u64)
    } else if let Some(rest) = trimmed.strip_suffix("MB") {
        (rest, 1_000_000u64)
    } else if let Some(rest) = trimmed.strip_suffix("KB") {
        (rest, 1_000u64)
    } else if let Some(rest) = trimmed.strip_suffix('B') {
        (rest, 1u64)
    } else {
        (trimmed, 1u64)
    };
    let num: f64 = num_part
        .trim()
        .parse()
        .map_err(|_| Error::bad_request(format!("expected e.g. 2GB or 2GiB, got {:?}", s)))?;
    if !num.is_finite() || num <= 0.0 {
        return Err(Error::bad_request(format!(
            "memory must be positive: {:?}",
            s
        )));
    }
    let bytes_f = num * mult as f64;
    if !bytes_f.is_finite() || bytes_f < 0.0 || bytes_f > u64::MAX as f64 {
        return Err(Error::bad_request(format!("memory {:?}: too large", s)));
    }
    Ok(bytes_f as u64)
}

/// Parse a disk-image size string. Integer-only, IEC-binary, single
/// suffix from `M`/`G`/`T` (case-insensitive). Used by `bhx image
/// pull` for `KnownImage::default_size`-style values like `"10G"` or
/// `"24G"`.
///
/// Distinct from [`parse_memory`] in that fractional input is
/// rejected — disk image sizes are always whole counts of MiB/GiB
/// — and the suffix grammar is single-char rather than the more
/// verbose `KiB`/`MiB`/etc. form.
///
/// Pre-#143 this lived in `image.rs::parse_size` with
/// `&s[..s.len() - 1]`, which would panic on a multi-byte UTF-8 last
/// char. The replacement uses `strip_suffix` which is byte-safe.
pub fn parse_size_disk(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::bad_request("empty size string"));
    }
    let (num_str, mult) = if let Some(rest) = s.strip_suffix('G').or_else(|| s.strip_suffix('g')) {
        (rest, 1024u64 * 1024 * 1024)
    } else if let Some(rest) = s.strip_suffix('T').or_else(|| s.strip_suffix('t')) {
        (rest, 1024u64 * 1024 * 1024 * 1024)
    } else if let Some(rest) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        (rest, 1024u64 * 1024)
    } else {
        (s, 1u64)
    };

    let num: u64 = num_str
        .parse()
        .map_err(|e| Error::bad_request(format!("invalid disk size {:?}: {}", s, e)))?;
    num.checked_mul(mult)
        .ok_or_else(|| Error::bad_request(format!("disk size {:?}: too large", s)))
}

/// RFC-952 / RFC-1123 hostname check. Returns the input unchanged
/// on success so the caller can use this in a `let host = parse_hostname(s)?;`
/// pipeline.
///
/// Constraints: 1..=63 chars, lowercase `a-z`, `0-9`, `-`, no leading
/// or trailing dash. Strict so a malformed override doesn't trip the
/// slirp DHCP server's parser silently.
pub fn parse_hostname(s: &str) -> Result<String> {
    if s.is_empty() {
        return Err(Error::bad_request("hostname: empty"));
    }
    if s.len() > 63 {
        return Err(Error::bad_request(
            "hostname longer than 63 chars (RFC 952)",
        ));
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err(Error::bad_request(
            "hostname must not start or end with '-'",
        ));
    }
    for c in s.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(Error::bad_request(format!(
                "hostname: only lowercase a-z, 0-9, '-' allowed (got {:?})",
                c
            )));
        }
    }
    Ok(s.to_string())
}

/// Parse a `HOST:GUEST` port forward into a `(host, guest)` u16 pair.
/// Both ports must be in `1..=65535`. Used by `--fwd` on the CLI and
/// by the profile schema's `forwards` array.
pub fn parse_fwd_pair(s: &str) -> Result<(u16, u16)> {
    let (h, g) = s
        .split_once(':')
        .ok_or_else(|| Error::bad_request(format!("expected HOST:GUEST in {:?}", s)))?;
    let host: u16 = h
        .parse()
        .map_err(|_| Error::bad_request(format!("invalid HOST {:?}", h)))?;
    let guest: u16 = g
        .parse()
        .map_err(|_| Error::bad_request(format!("invalid GUEST {:?}", g)))?;
    if host == 0 || guest == 0 {
        return Err(Error::bad_request(format!(
            "ports must be 1..=65535 in {:?}",
            s
        )));
    }
    Ok((host, guest))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_memory ----

    #[test]
    fn parse_memory_accepts_si_and_iec() {
        assert_eq!(parse_memory("2GB").unwrap(), 2_000_000_000);
        assert_eq!(parse_memory("2GiB").unwrap(), 2_147_483_648);
        assert_eq!(parse_memory("2048MB").unwrap(), 2_048_000_000);
        assert_eq!(parse_memory("1KB").unwrap(), 1000);
        assert_eq!(parse_memory("1KiB").unwrap(), 1024);
    }

    #[test]
    fn parse_memory_accepts_decimal_with_iec_suffix() {
        assert_eq!(parse_memory("1.5GiB").unwrap(), 1_610_612_736);
    }

    #[test]
    fn parse_memory_rejects_malformed() {
        assert!(parse_memory("").is_err());
        assert!(parse_memory("abc").is_err());
        assert!(parse_memory("0").is_err());
        assert!(parse_memory("-1GB").is_err());
        assert!(parse_memory(" 2GB ").is_ok());
        assert!(parse_memory("GB").is_err());
    }

    #[test]
    fn parse_memory_overflow_returns_error() {
        assert!(parse_memory("1e30GB").is_err());
        assert!(parse_memory("99999999999GB").is_err());
        assert!(parse_memory("inf").is_err());
        assert!(parse_memory("NaN").is_err());
    }

    #[test]
    fn parse_memory_at_or_below_u64_max_succeeds() {
        assert_eq!(parse_memory(&format!("{}", u64::MAX)).unwrap(), u64::MAX);
        assert_eq!(parse_memory(&format!("{}B", u64::MAX)).unwrap(), u64::MAX);
        assert!(parse_memory("1EiB").is_err()); // EiB suffix not supported
    }

    // ---- parse_size_disk ----

    #[test]
    fn parse_size_disk_accepts_iec_suffixes() {
        assert_eq!(parse_size_disk("10G").unwrap(), 10u64 * 1024 * 1024 * 1024);
        assert_eq!(parse_size_disk("10g").unwrap(), 10u64 * 1024 * 1024 * 1024);
        assert_eq!(
            parse_size_disk("2T").unwrap(),
            2u64 * 1024 * 1024 * 1024 * 1024
        );
        assert_eq!(parse_size_disk("512M").unwrap(), 512u64 * 1024 * 1024);
        assert_eq!(parse_size_disk("1024").unwrap(), 1024);
    }

    #[test]
    fn parse_size_disk_no_fractional() {
        assert!(parse_size_disk("1.5G").is_err());
    }

    #[test]
    fn parse_size_disk_rejects_multibyte_last_byte_safely() {
        // The pre-#143 `&s[..s.len() - 1]` would panic on a UTF-8
        // multi-byte char as the trailing byte. The replacement uses
        // `strip_suffix`, which only matches whole chars. Still an
        // error (no valid number-with-suffix), but a clean one.
        assert!(parse_size_disk("10€").is_err());
    }

    // ---- parse_hostname ----

    #[test]
    fn parse_hostname_accepts_clean() {
        assert_eq!(parse_hostname("alma01").unwrap(), "alma01");
        assert_eq!(parse_hostname("debian-bench").unwrap(), "debian-bench");
        assert_eq!(parse_hostname("a").unwrap(), "a");
    }

    #[test]
    fn parse_hostname_rejects_invalid() {
        assert!(parse_hostname("").is_err());
        assert!(parse_hostname(&"a".repeat(64)).is_err());
        assert!(parse_hostname("-foo").is_err());
        assert!(parse_hostname("foo-").is_err());
        assert!(parse_hostname("UPPER").is_err());
        assert!(parse_hostname("foo_bar").is_err());
        assert!(parse_hostname("foo.bar").is_err());
    }

    // ---- parse_fwd_pair ----

    #[test]
    fn parse_fwd_pair_accepts_valid() {
        assert_eq!(parse_fwd_pair("2222:22").unwrap(), (2222, 22));
        assert_eq!(parse_fwd_pair("65535:65535").unwrap(), (65535, 65535));
    }

    #[test]
    fn parse_fwd_pair_rejects_malformed() {
        assert!(parse_fwd_pair("").is_err());
        assert!(parse_fwd_pair("2222").is_err());
        assert!(parse_fwd_pair(":22").is_err());
        assert!(parse_fwd_pair("2222:").is_err());
        assert!(parse_fwd_pair("0:22").is_err());
        assert!(parse_fwd_pair("2222:0").is_err());
        assert!(parse_fwd_pair("65536:22").is_err());
        assert!(parse_fwd_pair("abc:22").is_err());
    }

    #[test]
    fn parse_fwd_pair_error_messages_name_the_input() {
        let e = parse_fwd_pair("abc:def").unwrap_err();
        assert!(format!("{:?}", e).contains("abc"));
    }
}
