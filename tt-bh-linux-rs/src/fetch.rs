// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! Shared download helpers used by `image`, `kernel`, and `ramdisk`.
//!
//! The three downloader modules all do the same dance: run `wget` to a
//! `<filename>.downloading` temp path, on success rename to the final
//! name, on failure clean up the temp. The decompression / unpacking
//! steps that follow differ enough per caller (xz keep-input vs not,
//! gunzip in-place, unzip-into-directory) that they stay in the
//! call-site modules. This module owns just the wget piece plus stale-
//! temp cleanup.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Download `url` into `dest_path` via `wget`.
///
/// Writes to `<dest_path>.downloading` first so a Ctrl-C or wget
/// failure mid-transfer doesn't leave a half-written file under the
/// real name. Any pre-existing `.downloading` file from a prior
/// crashed run is removed before the new wget starts. On wget
/// success, the temp is renamed to `dest_path` and the path is
/// returned. On failure, the temp is removed.
pub fn download_to(url: &str, dest_path: &Path) -> Result<PathBuf, String> {
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
        .map_err(|e| format!("Failed to run wget: {}. Install with: apt install wget", e))?;

    if !status.success() {
        let _ = fs::remove_file(&temp_path);
        return Err("Download failed".to_string());
    }

    fs::rename(&temp_path, dest_path).map_err(|e| format!("Failed to rename download: {}", e))?;
    Ok(dest_path.to_path_buf())
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
}
