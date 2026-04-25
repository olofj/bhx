// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Kernel/firmware management — download OpenSBI, kernel, and DTB for Blackhole.
//!
//! The Blackhole L2CPU requires a patched kernel and OpenSBI firmware from the
//! tenstorrent/linux and tenstorrent/opensbi repos. Most upstream distro kernels
//! won't work because they lack the VirtIO polling patches. The firmware bundle
//! (fw_jump.bin + Image + blackhole-card.dtb) is published as a zip in each
//! tt-bh-linux GitHub release.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A known kernel/firmware release.
#[derive(Debug, Clone)]
pub struct KnownKernel {
    /// Version tag (e.g., "0.10").
    pub version: &'static str,
    /// Download URL for the firmware zip.
    pub url: &'static str,
    /// Human-readable description.
    pub description: &'static str,
    /// Whether this is the default/recommended version.
    pub is_default: bool,
}

/// Registry of known kernel releases.
pub const KNOWN_KERNELS: &[KnownKernel] = &[
    KnownKernel {
        version: "0.10",
        url: "https://github.com/tenstorrent/tt-bh-linux/releases/download/v0.10/tt-bh-linux.zip",
        description: "tt-bh-linux v0.10 — latest release (fw_jump.bin + Image + blackhole-card.dtb)",
        is_default: true,
    },
    KnownKernel {
        version: "0.9",
        url: "https://github.com/tenstorrent/tt-bh-linux/releases/download/v0.9/tt-bh-linux.zip",
        description: "tt-bh-linux v0.9 (fw_jump.bin + Image + blackhole-card.dtb)",
        is_default: false,
    },
    KnownKernel {
        version: "0.5",
        url: "https://github.com/tenstorrent/tt-bh-linux/releases/download/v0.5/tt-bh-linux.zip",
        description: "tt-bh-linux v0.5 (fw_jump.bin + Image + blackhole-card.dtb)",
        is_default: false,
    },
];

/// Look up a kernel release by version, or return the default.
///
/// Accepts both `0.10` and `v0.10` / `V0.10` — the `v`/`V` prefix is
/// optional and case-insensitive so "v0.10" from a release tag and
/// "0.10" from a humans-typed CLI argument both work.
pub fn get_known_kernel(version: Option<&str>) -> Option<&'static KnownKernel> {
    match version {
        Some(v) => {
            let v_stripped = v
                .strip_prefix('v')
                .or_else(|| v.strip_prefix('V'))
                .unwrap_or(v);
            KNOWN_KERNELS
                .iter()
                .find(|k| k.version == v_stripped || k.version == v)
        }
        None => KNOWN_KERNELS.iter().find(|k| k.is_default),
    }
}

/// Default directory for firmware files.
fn firmware_dir() -> PathBuf {
    // Firmware goes in the project root (alongside fw_jump.bin, Image, etc.)
    PathBuf::from(".")
}

/// Pull the kernel/firmware bundle.
///
/// Downloads and extracts fw_jump.bin, Image, and blackhole-card.dtb to the
/// output directory.
pub fn pull_kernel(version: Option<&str>, output_dir: Option<&Path>) -> Result<PathBuf, String> {
    let kernel = get_known_kernel(version).ok_or_else(|| {
        let available: Vec<_> = KNOWN_KERNELS.iter().map(|k| k.version).collect();
        format!(
            "Unknown kernel version '{}'. Available: {}",
            version.unwrap_or("?"),
            available.join(", ")
        )
    })?;

    let dir = output_dir
        .map(PathBuf::from)
        .unwrap_or_else(firmware_dir);
    let _ = fs::create_dir_all(&dir);

    // Check if files already exist
    let fw_path = dir.join("fw_jump.bin");
    let image_path = dir.join("Image");
    let dtb_path = dir.join("blackhole-card.dtb");
    if fw_path.exists() && image_path.exists() && dtb_path.exists() {
        eprintln!("Firmware files already exist:");
        eprintln!("  {}", fw_path.display());
        eprintln!("  {}", image_path.display());
        eprintln!("  {}", dtb_path.display());
        eprintln!("Delete them first if you want to re-download.");
        return Ok(dir);
    }

    eprintln!("Pulling kernel v{} ...", kernel.version);
    eprintln!("  {}", kernel.description);
    eprintln!("  URL: {}", kernel.url);

    // Download zip
    let zip_path = dir.join("tt-bh-linux.zip");
    crate::fetch::download_to(kernel.url, &zip_path)?;

    // Extract
    eprintln!("  Extracting...");
    let status = Command::new("unzip")
        .args(["-o", "-j", "-d"])
        .arg(&dir)
        .arg(&zip_path)
        .status()
        .map_err(|e| format!("Failed to run unzip: {}", e))?;

    let _ = fs::remove_file(&zip_path);

    if !status.success() {
        return Err("unzip failed".to_string());
    }

    // Verify expected files exist
    let mut found = Vec::new();
    for name in &["fw_jump.bin", "Image", "blackhole-card.dtb"] {
        let p = dir.join(name);
        if p.exists() {
            found.push(name.to_string());
        }
    }

    if found.is_empty() {
        return Err("No firmware files found in zip".to_string());
    }

    eprintln!("Firmware ready in {}:", dir.display());
    for f in &found {
        let p = dir.join(f);
        let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        eprintln!("  {} ({:.1} MB)", f, size as f64 / (1024.0 * 1024.0));
    }

    Ok(dir)
}

// ============================================================================
// CLI command handlers
// ============================================================================

/// Print available kernel versions.
pub fn cmd_list() {
    println!("{:<12} {:<8} DESCRIPTION", "VERSION", "DEFAULT");
    println!("{}", "-".repeat(80));
    for k in KNOWN_KERNELS {
        let default_marker = if k.is_default { "*" } else { "" };
        println!("{:<12} {:<8} {}", k.version, default_marker, k.description);
    }
}

/// Pull the kernel firmware bundle.
pub fn cmd_pull(version: Option<&str>, output_dir: Option<&str>) {
    match pull_kernel(version, output_dir.map(Path::new)) {
        Ok(dir) => {
            println!("{}", dir.display());
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_known_kernel_returns_default_when_none_passed() {
        let k = get_known_kernel(None).expect("a default kernel must be set");
        assert!(k.is_default);
    }

    #[test]
    fn get_known_kernel_strips_v_prefix() {
        let with_v = get_known_kernel(Some("v0.10")).expect("v0.10 should resolve");
        let bare = get_known_kernel(Some("0.10")).expect("0.10 should resolve");
        assert_eq!(with_v.version, bare.version);
    }

    #[test]
    fn get_known_kernel_uppercase_v_prefix_resolves() {
        let with_v = get_known_kernel(Some("V0.10")).expect("V0.10 should resolve");
        assert_eq!(with_v.version, "0.10");
    }

    #[test]
    fn get_known_kernel_returns_none_for_unknown_version() {
        assert!(get_known_kernel(Some("9999")).is_none());
    }
}
