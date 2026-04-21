// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Ramdisk/initramfs management — download initramfs images for Blackhole L2CPU.
//!
//! Some distributions provide standalone initramfs/initrd images for riscv64.
//! These can be loaded directly into L2CPU DRAM alongside the kernel, useful
//! for network-boot or installer scenarios where no block device is needed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Compression format of the ramdisk download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gz,
    Xz,
}

/// A known ramdisk/initramfs available for download.
#[derive(Debug, Clone)]
pub struct KnownRamdisk {
    /// Unique name (e.g., "debian-13-netboot").
    pub name: &'static str,
    /// Download URL.
    pub url: &'static str,
    /// Human-readable description.
    pub description: &'static str,
    /// Aliases for this ramdisk.
    pub aliases: &'static [&'static str],
    /// Compression of the download.
    pub compression: Compression,
}

/// Registry of known ramdisk/initramfs images.
pub const KNOWN_RAMDISKS: &[KnownRamdisk] = &[
    KnownRamdisk {
        name: "debian-13-netboot",
        url: "https://deb.debian.org/debian/dists/trixie/main/installer-riscv64/current/images/netboot/initrd.gz",
        description: "Debian 13 (Trixie) netboot installer initrd — riscv64",
        aliases: &["debian-netboot", "debian-installer"],
        compression: Compression::Gz,
    },
    KnownRamdisk {
        name: "ubuntu-24.04-netboot",
        url: "https://cdimage.ubuntu.com/ubuntu-server/daily-live/current/noble-netboot-riscv64.initrd",
        description: "Ubuntu 24.04 (Noble) netboot initrd — riscv64",
        aliases: &["ubuntu-netboot"],
        compression: Compression::None,
    },
];

/// Look up a ramdisk by name or alias.
pub fn find_ramdisk(name: &str) -> Option<&'static KnownRamdisk> {
    let name_lower = name.to_lowercase();
    KNOWN_RAMDISKS.iter().find(|r| {
        r.name == name_lower || r.aliases.iter().any(|a| *a == name_lower)
    })
}

/// Default directory for ramdisk files.
fn ramdisk_dir() -> PathBuf {
    PathBuf::from("ramdisks")
}

/// Pull a ramdisk/initramfs.
pub fn pull_ramdisk(name: &str, output: Option<&Path>) -> Result<PathBuf, String> {
    let ramdisk = find_ramdisk(name).ok_or_else(|| {
        let available: Vec<_> = KNOWN_RAMDISKS.iter().map(|r| r.name).collect();
        format!(
            "Unknown ramdisk '{}'. Available: {}",
            name,
            available.join(", ")
        )
    })?;

    let dir = ramdisk_dir();
    let _ = fs::create_dir_all(&dir);

    let output_path = output
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join(format!("{}.initrd", ramdisk.name)));

    if output_path.exists() {
        eprintln!("Ramdisk already exists: {}", output_path.display());
        eprintln!("Delete it first if you want to re-download.");
        return Ok(output_path);
    }

    eprintln!("Pulling ramdisk: {}", ramdisk.name);
    eprintln!("  {}", ramdisk.description);
    eprintln!("  URL: {}", ramdisk.url);

    let download_path = match ramdisk.compression {
        Compression::None => output_path.clone(),
        Compression::Gz => {
            // Append .gz suffix so gunzip produces the correct output filename
            let mut name = output_path.as_os_str().to_owned();
            name.push(".gz");
            PathBuf::from(name)
        }
        Compression::Xz => {
            let mut name = output_path.as_os_str().to_owned();
            name.push(".xz");
            PathBuf::from(name)
        }
    };

    // Download
    eprintln!("  Downloading...");
    let status = Command::new("wget")
        .arg("-O")
        .arg(&download_path)
        .arg("--progress=bar:force:noscroll")
        .arg(ramdisk.url)
        .status()
        .map_err(|e| format!("Failed to run wget: {}", e))?;

    if !status.success() {
        let _ = fs::remove_file(&download_path);
        return Err("Download failed".to_string());
    }

    // Decompress if needed
    match ramdisk.compression {
        Compression::None => {}
        Compression::Gz => {
            eprintln!("  Decompressing (gzip)...");
            let status = Command::new("gunzip")
                .arg("-f")
                .arg(&download_path)
                .status()
                .map_err(|e| format!("Failed to run gunzip: {}", e))?;
            if !status.success() {
                return Err("gunzip failed".to_string());
            }
        }
        Compression::Xz => {
            eprintln!("  Decompressing (xz)...");
            let status = Command::new("xz")
                .args(["-d", "-f"])
                .arg(&download_path)
                .status()
                .map_err(|e| format!("Failed to run xz: {}", e))?;
            if !status.success() {
                return Err("xz decompression failed".to_string());
            }
        }
    }

    let size = fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);
    eprintln!(
        "Ramdisk ready: {} ({:.1} MB)",
        output_path.display(),
        size as f64 / (1024.0 * 1024.0)
    );

    Ok(output_path)
}

// ============================================================================
// CLI command handlers
// ============================================================================

/// Print available ramdisks.
pub fn cmd_list() {
    println!("{:<25} DESCRIPTION", "NAME");
    println!("{}", "-".repeat(80));
    for r in KNOWN_RAMDISKS {
        println!("{:<25} {}", r.name, r.description);
        if !r.aliases.is_empty() {
            println!("{:<25} aliases: {}", "", r.aliases.join(", "));
        }
    }
}

/// Pull a ramdisk.
pub fn cmd_pull(name: &str, output: Option<&str>) {
    match pull_ramdisk(name, output.map(Path::new)) {
        Ok(path) => {
            println!("{}", path.display());
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}
