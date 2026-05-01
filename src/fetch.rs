// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Shared download helpers used by `image` and `ramdisk`.
//!
//! Two layers:
//!
//! * [`download_to`] streams a URL to a temp file (via `ureq`), then
//!   atomically renames into place. Used directly by callers that don't
//!   want caching.
//! * [`download_to_cached`] consults a `<dest>.fetch.json` sidecar and
//!   skips the body download if a HEAD against the URL shows the
//!   upstream's `ETag` / `Last-Modified` matches what the sidecar
//!   recorded last time.
//!
//! Decompression / unpacking stays in the call-site modules
//! (`image.rs`, `ramdisk.rs`); the semantics genuinely differ per
//! caller (xz keep-input vs not, gunzip in-place, unzip-into-directory).

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Cached HTTP-conditional metadata for a downloaded file. Persisted
/// next to the destination as `<dest>.fetch.json`. Either field may be
/// absent if the upstream doesn't emit it.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

/// Download `url` into `dest_path` via a streaming HTTP GET.
///
/// Writes to `<dest_path>.downloading` first so a Ctrl-C or transfer
/// failure mid-flight doesn't leave a half-written file under the real
/// name. Any pre-existing `.downloading` file from a prior crashed run
/// is removed before the new GET starts. On success, the temp is
/// renamed to `dest_path` and the path is returned. On failure, the
/// temp is removed.
///
/// Live progress (bytes received / total / current rate) is written to
/// stderr; the post-download summary line lands on stderr too so log-
/// scraping by stdout-only consumers stays unaffected.
pub fn download_to(url: &str, dest_path: &Path) -> Result<PathBuf> {
    let temp_path = downloading_path(dest_path);

    // Stale `.downloading` from a previous crashed run: drop it before
    // starting the new GET so we don't append/race with old bytes.
    let _ = fs::remove_file(&temp_path);

    eprintln!("  Downloading {}", url);
    let started = Instant::now();
    let mut response = ureq::get(url)
        .call()
        .map_err(|e| Error::internal(format!("GET {}: {}", url, e)))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(Error::internal(format!(
            "GET {} returned status {}",
            url, status
        )));
    }
    let total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let mut file = fs::File::create(&temp_path).map_err(Error::io_ctx("create temp"))?;
    let mut reader = response.body_mut().as_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut got: u64 = 0;
    let mut last_print = Instant::now();
    let res = (|| -> Result<()> {
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => return Err(Error::internal(format!("read body: {}", e))),
            };
            file.write_all(&buf[..n])
                .map_err(Error::io_ctx("write temp"))?;
            got += n as u64;
            if last_print.elapsed() >= Duration::from_millis(200) {
                print_progress(got, total, started.elapsed());
                last_print = Instant::now();
            }
        }
        Ok(())
    })();
    drop(reader);
    if let Err(e) = res {
        let _ = fs::remove_file(&temp_path);
        return Err(e);
    }
    file.flush().map_err(Error::io_ctx("flush temp"))?;
    drop(file);

    let elapsed = started.elapsed();
    fs::rename(&temp_path, dest_path).map_err(Error::io_ctx("rename download"))?;

    // Force one final tick so the rendered progress reads "100.0%" instead
    // of whatever the last 200-ms tick happened to catch (typically 99.9%).
    print_progress(got, total, elapsed);
    eprintln!();
    eprintln!(
        "  Downloaded {} in {} (avg {}/s)",
        format_bytes(got),
        format_duration(elapsed),
        format_bytes(rate_bps(got, elapsed)),
    );
    Ok(dest_path.to_path_buf())
}

/// Variant of `download_to` that consults a sidecar metadata file to
/// skip the GET when the upstream hasn't changed.
///
/// `sidecar_anchor` is the path of the *final* artifact that survives
/// the caller's pipeline — the sidecar lives at
/// `<sidecar_anchor>.fetch.json`. For pipelines that do nothing post-
/// download (raw initrd), `sidecar_anchor == dest_path`. For pipelines
/// that decompress / unzip / convert (image .ext4, ramdisk .gz/.xz),
/// pass the path the surviving artifact will end up at — the cache
/// check on the next call looks at that file's existence + the
/// sidecar, not the long-gone download intermediate. See #26.
pub fn download_to_cached(
    url: &str,
    dest_path: &Path,
    sidecar_anchor: &Path,
    force: bool,
) -> Result<PathBuf> {
    if !force && cache_hit(url, sidecar_anchor) {
        eprintln!("  Skipping download — upstream unchanged ({})", url);
        return Ok(dest_path.to_path_buf());
    }
    download_to(url, dest_path)?;
    // Best-effort sidecar refresh; HEAD failure shouldn't fail the
    // download since we already have the file. Worst case: next call
    // sees a stale sidecar and re-downloads.
    if let Ok(meta) = head_metadata(url) {
        let _ = write_sidecar(sidecar_anchor, &meta);
    } else {
        // If HEAD failed but the body succeeded, drop any pre-existing
        // sidecar so we don't keep a stale match around.
        let _ = fs::remove_file(sidecar_path(sidecar_anchor));
    }
    Ok(dest_path.to_path_buf())
}

/// True iff the anchor file exists, the sidecar at
/// `<anchor>.fetch.json` exists and parses, and a HEAD against `url`
/// shows a matching ETag or Last-Modified.
fn cache_hit(url: &str, anchor: &Path) -> bool {
    if !anchor.exists() {
        return false;
    }
    let sidecar = match read_sidecar(anchor) {
        Some(m) => m,
        None => return false,
    };
    let upstream = match head_metadata(url) {
        Ok(m) => m,
        Err(_) => return false,
    };
    upstream_matches(&sidecar, &upstream)
}

/// Two metadata records match if at least one of (etag, last_modified)
/// agrees and is non-empty on both sides. If both fields are absent
/// from upstream, we conservatively treat that as "no match" so we
/// re-download on the next pull.
pub(crate) fn upstream_matches(cached: &FetchMetadata, upstream: &FetchMetadata) -> bool {
    let etag_match = match (&cached.etag, &upstream.etag) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => a == b,
        _ => false,
    };
    if etag_match {
        return true;
    }
    match (&cached.last_modified, &upstream.last_modified) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => a == b,
        _ => false,
    }
}

/// HTTP HEAD against `url` to read ETag / Last-Modified for the
/// conditional-cache check. ureq follows redirects automatically, so
/// the headers we read come from the final response.
fn head_metadata(url: &str) -> Result<FetchMetadata> {
    let response = ureq::head(url)
        .call()
        .map_err(|e| Error::internal(format!("HEAD {}: {}", url, e)))?;
    let headers = response.headers();
    Ok(FetchMetadata {
        etag: headers
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
        last_modified: headers
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
    })
}

/// In-place stderr progress line. Overwrites itself with `\r` so the
/// terminal doesn't scroll a hundred lines per download. With no
/// content-length, omits the percentage and ETA.
fn print_progress(got: u64, total: Option<u64>, elapsed: Duration) {
    let rate = rate_bps(got, elapsed);
    match total {
        Some(t) if t > 0 => {
            let pct = (got as f64 / t as f64 * 100.0).min(100.0);
            eprint!(
                "\r  {} / {} ({:.1}%) — {}/s          ",
                format_bytes(got),
                format_bytes(t),
                pct,
                format_bytes(rate),
            );
        }
        _ => {
            eprint!(
                "\r  {} — {}/s          ",
                format_bytes(got),
                format_bytes(rate),
            );
        }
    }
    let _ = std::io::stderr().flush();
}

/// Bytes-per-second over `elapsed`; 0 if no time has passed yet.
fn rate_bps(bytes: u64, elapsed: Duration) -> u64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        0
    } else {
        (bytes as f64 / secs) as u64
    }
}

/// Format bytes as IEC binary (KiB / MiB / GiB). Ranges chosen to keep
/// the printable string tight: 3-4 significant figures.
fn format_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.1} KiB", n as f64 / KIB as f64)
    } else {
        format!("{} B", n)
    }
}

/// Format a duration as `Xs`, `XmYs`, or `XhYm`. Tighter than the
/// stdlib's `{:?}` print and more operator-friendly.
fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{}h{}m", h, m)
    } else if m > 0 {
        format!("{}m{}s", m, s)
    } else {
        // Sub-second resolution for short downloads, since most
        // assets in our `image pull` registry land in 5-15 s.
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// Sidecar path: `<dest>.fetch.json` next to the file.
fn sidecar_path(dest_path: &Path) -> PathBuf {
    let mut s = dest_path.as_os_str().to_owned();
    s.push(".fetch.json");
    PathBuf::from(s)
}

/// Read and parse the sidecar. Returns None on any error (missing
/// file, malformed JSON, missing fields) so the caller falls through
/// to a re-download — partial cache state is worse than no cache.
pub(crate) fn read_sidecar(dest_path: &Path) -> Option<FetchMetadata> {
    let sc = sidecar_path(dest_path);
    let bytes = fs::read(&sc).ok()?;
    serde_json::from_slice::<FetchMetadata>(&bytes).ok()
}

/// Write the sidecar atomically (write to temp, rename). On any error,
/// returns Err but the caller treats this as best-effort.
fn write_sidecar(dest_path: &Path, meta: &FetchMetadata) -> Result<()> {
    let sc = sidecar_path(dest_path);
    let mut tmp = sc.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let json = serde_json::to_vec_pretty(meta)?;
    fs::write(&tmp, json).map_err(Error::io_ctx("write sidecar tmp"))?;
    fs::rename(&tmp, &sc).map_err(Error::io_ctx("rename sidecar"))?;
    Ok(())
}

fn downloading_path(dest_path: &Path) -> PathBuf {
    let mut s = dest_path.as_os_str().to_owned();
    s.push(".downloading");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloading_path_appends_suffix() {
        let p = Path::new("/tmp/foo.bin");
        assert_eq!(
            downloading_path(p),
            PathBuf::from("/tmp/foo.bin.downloading")
        );
    }

    #[test]
    fn downloading_path_appends_to_extensionless_path() {
        let p = Path::new("/tmp/Image");
        assert_eq!(downloading_path(p), PathBuf::from("/tmp/Image.downloading"));
    }

    #[test]
    fn sidecar_path_appends_fetch_json() {
        assert_eq!(
            sidecar_path(Path::new("/tmp/foo.bin")),
            PathBuf::from("/tmp/foo.bin.fetch.json")
        );
    }

    // ---- format_bytes ----

    #[test]
    fn format_bytes_handles_each_range() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(50 * 1024 * 1024), "50.0 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GiB");
        assert_eq!(format_bytes(2_500_000_000), "2.33 GiB");
    }

    // ---- format_duration ----

    #[test]
    fn format_duration_subsecond_returns_one_decimal_seconds() {
        assert_eq!(format_duration(Duration::from_millis(450)), "0.5s");
        assert_eq!(format_duration(Duration::from_millis(8500)), "8.5s");
    }

    #[test]
    fn format_duration_minutes_and_hours() {
        assert_eq!(format_duration(Duration::from_secs(75)), "1m15s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h2m");
    }

    // ---- rate_bps ----

    #[test]
    fn rate_bps_zero_elapsed_yields_zero() {
        assert_eq!(rate_bps(1024, Duration::from_secs(0)), 0);
    }

    #[test]
    fn rate_bps_computes_average() {
        // 1 MiB over 2 s = 524288 B/s.
        let half_meg = rate_bps(1024 * 1024, Duration::from_secs(2));
        assert_eq!(half_meg, 524288);
    }

    // ---- upstream_matches ----

    #[test]
    fn upstream_matches_on_etag_alone() {
        let cached = FetchMetadata {
            etag: Some("\"x\"".to_string()),
            last_modified: None,
        };
        let upstream = FetchMetadata {
            etag: Some("\"x\"".to_string()),
            last_modified: Some("ignored".to_string()),
        };
        assert!(upstream_matches(&cached, &upstream));
    }

    #[test]
    fn upstream_matches_on_last_modified_when_etag_missing() {
        let cached = FetchMetadata {
            etag: None,
            last_modified: Some("Mon, 01 Jan 2024 00:00:00 GMT".to_string()),
        };
        let upstream = FetchMetadata {
            etag: None,
            last_modified: Some("Mon, 01 Jan 2024 00:00:00 GMT".to_string()),
        };
        assert!(upstream_matches(&cached, &upstream));
    }

    #[test]
    fn upstream_does_not_match_when_etag_changes() {
        let cached = FetchMetadata {
            etag: Some("\"old\"".to_string()),
            last_modified: Some("same".to_string()),
        };
        let upstream = FetchMetadata {
            etag: Some("\"new\"".to_string()),
            last_modified: None,
        };
        // ETag mismatch + no last-modified comparison possible → miss.
        assert!(!upstream_matches(&cached, &upstream));
    }

    #[test]
    fn upstream_does_not_match_when_both_missing() {
        let cached = FetchMetadata::default();
        let upstream = FetchMetadata::default();
        // No fields to compare → conservatively a miss; we'd rather
        // re-download than skip on suspect "match".
        assert!(!upstream_matches(&cached, &upstream));
    }

    #[test]
    fn upstream_does_not_match_empty_strings() {
        let cached = FetchMetadata {
            etag: Some(String::new()),
            last_modified: None,
        };
        let upstream = FetchMetadata {
            etag: Some(String::new()),
            last_modified: None,
        };
        assert!(!upstream_matches(&cached, &upstream));
    }

    // ---- read_sidecar ----

    #[test]
    fn read_sidecar_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("foo.bin");
        assert!(read_sidecar(&dest).is_none());
    }

    #[test]
    fn read_sidecar_returns_none_for_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("foo.bin");
        let sc = sidecar_path(&dest);
        fs::write(&sc, b"not valid json {{{").unwrap();
        assert!(read_sidecar(&dest).is_none());
    }

    #[test]
    fn read_sidecar_round_trips_full_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("foo.bin");
        let original = FetchMetadata {
            etag: Some("\"abc123\"".to_string()),
            last_modified: Some("Mon, 01 Jan 2024 00:00:00 GMT".to_string()),
        };
        write_sidecar(&dest, &original).unwrap();
        let parsed = read_sidecar(&dest).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn read_sidecar_round_trips_etag_only() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("foo.bin");
        let original = FetchMetadata {
            etag: Some("\"abc123\"".to_string()),
            last_modified: None,
        };
        write_sidecar(&dest, &original).unwrap();
        let parsed = read_sidecar(&dest).unwrap();
        assert_eq!(parsed, original);
    }

    // ---- cache_hit (anchor semantics, #26) ----

    #[test]
    fn cache_hit_false_when_anchor_does_not_exist() {
        // The download intermediate's existence shouldn't matter —
        // cache_hit checks the anchor file. With no anchor file
        // present, cache is a miss regardless of what's in the
        // sidecar.
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("rootfs.ext4");
        let meta = FetchMetadata {
            etag: Some("\"abc123\"".to_string()),
            last_modified: None,
        };
        write_sidecar(&anchor, &meta).unwrap();
        assert!(read_sidecar(&anchor).is_some());
        assert!(!anchor.exists());
        // Short-circuit on anchor.exists() before any HEAD.
        assert!(!cache_hit("http://nowhere.invalid/x", &anchor));
    }

    #[test]
    fn sidecar_lives_at_anchor_not_at_dest() {
        // When dest_path and sidecar_anchor differ, the sidecar must
        // be written next to the anchor — so a pipeline that consumes
        // dest (gunzip, unzip, xz -d) leaves the sidecar adjacent to
        // the *surviving* artifact.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("rootfs.ext4.xz");
        let anchor = dir.path().join("rootfs.ext4");
        let meta = FetchMetadata {
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
        };
        write_sidecar(&anchor, &meta).unwrap();
        assert!(sidecar_path(&anchor).exists());
        assert!(!sidecar_path(&dest).exists());
        assert_eq!(read_sidecar(&anchor).unwrap(), meta);
        assert!(read_sidecar(&dest).is_none());
    }
}
