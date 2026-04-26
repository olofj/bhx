// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Shared download helpers used by `image`, `kernel`, and `ramdisk`.
//!
//! Two layers:
//!
//! * [`download_to`] is the basic wget-with-temp wrapper. Used directly
//!   by callers that don't want caching (or that already wrap us with
//!   their own cache).
//! * [`download_to_cached`] consults a `<dest>.fetch.json` sidecar and
//!   skips the body download if a `wget --spider` HEAD against the URL
//!   shows the upstream's `ETag` / `Last-Modified` matches what the
//!   sidecar recorded last time.
//!
//! Decompression / unpacking stays in the call-site modules
//! (`image.rs`, `kernel.rs`, `ramdisk.rs`); the semantics genuinely
//! differ per caller (xz keep-input vs not, gunzip in-place, unzip-
//! into-directory).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Download `url` into `dest_path` via `wget`.
///
/// Writes to `<dest_path>.downloading` first so a Ctrl-C or wget
/// failure mid-transfer doesn't leave a half-written file under the
/// real name. Any pre-existing `.downloading` file from a prior
/// crashed run is removed before the new wget starts. On wget
/// success, the temp is renamed to `dest_path` and the path is
/// returned. On failure, the temp is removed.
pub fn download_to(url: &str, dest_path: &Path) -> Result<PathBuf> {
    let temp_path = downloading_path(dest_path);

    // Stale `.downloading` from a previous crashed run: drop it before
    // starting wget so the new transfer doesn't append/race with old
    // bytes. Ignore errors — if the file isn't there, that's fine.
    let _ = fs::remove_file(&temp_path);

    eprintln!("  Downloading...");
    let status = Command::new("wget")
        .arg("-O")
        .arg(&temp_path)
        .arg("--progress=bar:force:noscroll")
        .arg(url)
        .status()
        .map_err(|e| {
            Error::internal(format!(
                "Failed to run wget: {}. Install with: apt install wget",
                e
            ))
        })?;

    if !status.success() {
        let _ = fs::remove_file(&temp_path);
        return Err(Error::internal("Download failed"));
    }

    fs::rename(&temp_path, dest_path).map_err(Error::io_ctx("Failed to rename download"))?;
    Ok(dest_path.to_path_buf())
}

/// Variant of `download_to` that consults a sidecar metadata file to
/// skip the wget when the upstream hasn't changed.
///
/// `sidecar_anchor` is the path of the *final* artifact that survives
/// the caller's pipeline — the sidecar lives at
/// `<sidecar_anchor>.fetch.json`. For pipelines that do nothing post-
/// download (raw initrd), `sidecar_anchor == dest_path`. For
/// pipelines that decompress / unzip / convert (image .ext4, kernel
/// `Image`, ramdisk .gz/.xz), pass the path the surviving artifact
/// will end up at — the cache check on the next call looks at that
/// file's existence + the sidecar, not the long-gone download
/// intermediate. See #26.
///
/// Behavior:
/// - If `force` is true, the cache check is bypassed and we always
///   download.
/// - Else, if the anchor file is missing OR its sidecar is
///   missing/invalid, we download.
/// - Else, run `wget --spider` to fetch the upstream's ETag /
///   Last-Modified. If either field matches the sidecar, skip the
///   download and return the existing `dest_path`.
/// - After any successful download, refresh the sidecar (anchored at
///   `sidecar_anchor`) with the current upstream metadata so the
///   next call has something to compare against.
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

/// Run `wget --spider --server-response <url>` and parse ETag /
/// Last-Modified out of the response headers. Follows redirects (the
/// last block of HTTP/1.1 lines is the one whose ETag we care about).
fn head_metadata(url: &str) -> Result<FetchMetadata> {
    let output = Command::new("wget")
        .arg("--spider")
        .arg("--server-response")
        .arg("--tries=1")
        .arg("--timeout=10")
        .arg(url)
        .output()
        .map_err(|e| Error::internal(format!("Failed to run wget --spider: {}", e)))?;
    // wget --spider prints headers to stderr regardless of HTTP status.
    // We don't care if it returned non-zero (some servers 405 a HEAD);
    // we only want whatever headers it managed to capture.
    let stderr = String::from_utf8_lossy(&output.stderr);
    Ok(parse_wget_headers(&stderr))
}

/// Parse wget --server-response stderr for the ETag and Last-Modified
/// of the *last* HTTP response block. wget prefixes each header line
/// with two spaces; redirect chains print one block per hop.
pub(crate) fn parse_wget_headers(stderr: &str) -> FetchMetadata {
    let mut last_etag: Option<String> = None;
    let mut last_lm: Option<String> = None;
    let mut block_etag: Option<String> = None;
    let mut block_lm: Option<String> = None;
    for raw in stderr.lines() {
        let line = raw.trim_start();
        // A new HTTP response block resets the per-block accumulators
        // — but commit the previous block's findings to "last_*" so
        // a subsequent block without ETag doesn't lose a redirect's
        // ETag entirely.
        if line.starts_with("HTTP/") {
            if block_etag.is_some() {
                last_etag = block_etag.take();
            }
            if block_lm.is_some() {
                last_lm = block_lm.take();
            }
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("ETag: ")
            .or_else(|| line.strip_prefix("etag: "))
        {
            block_etag = Some(rest.trim().to_string());
        } else if let Some(rest) = line
            .strip_prefix("Last-Modified: ")
            .or_else(|| line.strip_prefix("last-modified: "))
        {
            block_lm = Some(rest.trim().to_string());
        }
    }
    // Final block's findings.
    if let Some(e) = block_etag {
        last_etag = Some(e);
    }
    if let Some(lm) = block_lm {
        last_lm = Some(lm);
    }
    FetchMetadata {
        etag: last_etag,
        last_modified: last_lm,
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

    // ---- header parsing ----

    #[test]
    fn parse_wget_headers_extracts_etag_and_last_modified() {
        let stderr = "
--2026-04-25 10:00:00--  https://example.com/foo.bin
Resolving example.com (example.com)... 1.2.3.4
Connecting to example.com|1.2.3.4|:443... connected.
HTTP request sent, awaiting response...
  HTTP/1.1 200 OK
  Last-Modified: Tue, 09 Apr 2024 10:21:54 GMT
  ETag: \"abc123\"
  Content-Length: 12345
";
        let m = parse_wget_headers(stderr);
        assert_eq!(m.etag.as_deref(), Some("\"abc123\""));
        assert_eq!(
            m.last_modified.as_deref(),
            Some("Tue, 09 Apr 2024 10:21:54 GMT")
        );
    }

    #[test]
    fn parse_wget_headers_takes_last_block_after_redirect() {
        // Redirect chain: 301 → 200. The 200 block's ETag is what we
        // want; the 301 block usually has no ETag but might.
        let stderr = "
  HTTP/1.1 301 Moved Permanently
  Location: https://cdn.example.com/foo.bin
  ETag: \"redirect-etag\"
  HTTP/1.1 200 OK
  ETag: \"final-etag\"
  Content-Length: 12345
";
        let m = parse_wget_headers(stderr);
        assert_eq!(m.etag.as_deref(), Some("\"final-etag\""));
    }

    #[test]
    fn parse_wget_headers_handles_lowercased_header_names() {
        // RFC 7230 says HTTP header names are case-insensitive; some
        // CDNs lower-case them.
        let stderr = "
  HTTP/1.1 200 OK
  etag: \"lower\"
  last-modified: Mon, 01 Jan 2024 00:00:00 GMT
";
        let m = parse_wget_headers(stderr);
        assert_eq!(m.etag.as_deref(), Some("\"lower\""));
        assert_eq!(
            m.last_modified.as_deref(),
            Some("Mon, 01 Jan 2024 00:00:00 GMT")
        );
    }

    #[test]
    fn parse_wget_headers_returns_empty_when_no_headers() {
        let m = parse_wget_headers("");
        assert!(m.etag.is_none());
        assert!(m.last_modified.is_none());
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
        // sidecar. (We can't actually run the network HEAD here,
        // but the function short-circuits on the anchor.exists()
        // check before any wget, so this exercises that path.)
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("rootfs.ext4");
        // Sidecar exists but anchor doesn't. (Simulates the bug
        // pre-#26 would have hit if we were anchoring on the
        // intermediate.)
        let meta = FetchMetadata {
            etag: Some("\"abc123\"".to_string()),
            last_modified: None,
        };
        write_sidecar(&anchor, &meta).unwrap();
        // Sanity: sidecar IS present.
        assert!(read_sidecar(&anchor).is_some());
        // But anchor file is missing.
        assert!(!anchor.exists());
        // cache_hit must be false: nothing for the operator to use.
        assert!(!cache_hit("http://nowhere.invalid/x", &anchor));
    }

    #[test]
    fn sidecar_lives_at_anchor_not_at_dest() {
        // When dest_path and sidecar_anchor differ, the sidecar must
        // be written next to the anchor — so a pipeline that consumes
        // dest (gunzip, unzip, xz -d) leaves the sidecar adjacent to
        // the *surviving* artifact. Rebuilds of pre-#26 behavior
        // would write the sidecar next to dest and orphan it.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("rootfs.ext4.xz");
        let anchor = dir.path().join("rootfs.ext4");
        let meta = FetchMetadata {
            etag: Some("\"v1\"".to_string()),
            last_modified: None,
        };
        write_sidecar(&anchor, &meta).unwrap();
        // Sidecar exists at <anchor>.fetch.json, not <dest>.fetch.json.
        assert!(sidecar_path(&anchor).exists());
        assert!(!sidecar_path(&dest).exists());
        // And it round-trips back via the anchor.
        assert_eq!(read_sidecar(&anchor).unwrap(), meta);
        assert!(read_sidecar(&dest).is_none());
    }
}
