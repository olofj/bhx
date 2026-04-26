// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Image management — download, convert, and manage riscv64 rootfs images
//! for booting Linux on the Blackhole L2CPU.
//!
//! Inspired by the image management in ~/exe, adapted for riscv64 and the
//! Blackhole's requirement for raw ext4 filesystem images.
//!
//! # Threat model for `Command::new` invocations
//!
//! This module shells out to `wget`, `xz`, `unzip`, `qemu-img`, `sfdisk`,
//! `dd`, `e2fsck`, `resize2fs` by basename — `Command::new("wget")`
//! resolves via `$PATH`. A malicious `$PATH` (or a shell function
//! shadowing one of these names) could substitute a different binary.
//! That's accepted: these helpers run as the operator's own user from
//! the CLI, never inside the daemon. An attacker who can already
//! corrupt the operator's `$PATH` already has equivalent access to do
//! anything else the operator can. Resolving via `which` once at
//! startup wouldn't change the threat model — it'd be the same
//! `which` lookup against the same `$PATH`, just done earlier.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

// ============================================================================
// Image format and source definitions
// ============================================================================

/// Format of the downloaded image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// Raw ext4 filesystem — ready to use directly.
    Ext4,
    /// Raw disk image with GPT partition table — needs partition extraction.
    RawDisk,
    /// QEMU qcow2 — needs conversion to raw, then partition extraction.
    Qcow2,
}

/// Compression format of the download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Xz,
    Zip,
}

/// A known riscv64 image available for download.
#[derive(Debug, Clone)]
pub struct KnownImage {
    /// Unique name (e.g., "debian-13").
    pub name: &'static str,
    /// Download URL.
    pub url: &'static str,
    /// Human-readable description.
    pub description: &'static str,
    /// Aliases for this image (e.g., ["debian", "trixie"]).
    pub aliases: &'static [&'static str],
    /// Format of the downloaded file.
    pub format: ImageFormat,
    /// Compression of the download.
    pub compression: Compression,
    /// Default size to resize to (e.g., "10G"). Empty string = no resize.
    pub default_size: &'static str,
    /// Default username for login.
    pub default_user: &'static str,
    /// Default password (if any).
    pub default_password: &'static str,
    /// Whether the image has cloud-init support.
    pub cloud_init: bool,
}

/// Registry of known riscv64 images available for download.
pub const KNOWN_IMAGES: &[KnownImage] = &[
    // ========================================================================
    // Tenstorrent pre-built image (easiest, recommended)
    // ========================================================================
    KnownImage {
        name: "tt-debian",
        url: "https://github.com/tenstorrent/tt-bh-linux/releases/download/v0.10/tt-bh-disk-image.zip",
        description: "Tenstorrent pre-built Debian riscv64 (recommended, ready to use)",
        aliases: &["default", "tt"],
        format: ImageFormat::Ext4, // After unzip, it's a raw ext4 image
        compression: Compression::Zip,
        default_size: "10G",
        default_user: "debian",
        default_password: "debian",
        cloud_init: true,
    },
    // ========================================================================
    // Debian Cloud Images (from cloud.debian.org)
    // The "nocloud" variant works without cloud-init (has default root login).
    // The "generic" variant requires cloud-init for initial setup.
    // ========================================================================
    KnownImage {
        name: "debian-13",
        url: "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-nocloud-riscv64.raw",
        description: "Debian 13 (Trixie) nocloud — official cloud image, no cloud-init needed",
        aliases: &["debian", "trixie"],
        format: ImageFormat::RawDisk,
        compression: Compression::None,
        default_size: "10G",
        default_user: "root",
        default_password: "",
        cloud_init: false,
    },
    KnownImage {
        name: "debian-13-cloud",
        url: "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-riscv64.raw",
        description: "Debian 13 (Trixie) generic — official cloud image, needs cloud-init",
        aliases: &["debian-cloud", "trixie-cloud"],
        format: ImageFormat::RawDisk,
        compression: Compression::None,
        default_size: "10G",
        default_user: "",
        default_password: "",
        cloud_init: true,
    },
    // ========================================================================
    // Ubuntu (from cdimage.ubuntu.com)
    // Preinstalled server images — GPT partitioned, xz compressed.
    // ========================================================================
    KnownImage {
        name: "ubuntu-24.04",
        url: "https://cdimage.ubuntu.com/releases/noble/release/ubuntu-24.04.4-preinstalled-server-riscv64.img.xz",
        description: "Ubuntu 24.04 LTS (Noble Numbat) — preinstalled server for riscv64",
        aliases: &["ubuntu", "noble"],
        format: ImageFormat::RawDisk,
        compression: Compression::Xz,
        default_size: "10G",
        default_user: "ubuntu",
        default_password: "ubuntu",
        cloud_init: true,
    },
    // ========================================================================
    // Fedora (from dl.fedoraproject.org)
    // Cloud base images — qcow2 format.
    // ========================================================================
    KnownImage {
        name: "fedora-42",
        url: "https://dl.fedoraproject.org/pub/alt/risc-v/release/42/Cloud/riscv64/images/Fedora-Cloud-Base-Generic-42.20250911-2251ba41cdd3.riscv64.qcow2",
        description: "Fedora 42 Cloud Base — generic riscv64 cloud image",
        aliases: &["fedora"],
        format: ImageFormat::Qcow2,
        compression: Compression::None,
        default_size: "10G",
        default_user: "",
        default_password: "",
        cloud_init: true,
    },
];

/// Look up a known image by name or alias.
pub fn get_known_image(name: &str) -> Option<&'static KnownImage> {
    if let Some(img) = KNOWN_IMAGES.iter().find(|img| img.name == name) {
        return Some(img);
    }
    KNOWN_IMAGES.iter().find(|img| img.aliases.contains(&name))
}

/// List all known image names.
pub fn list_known_images() -> &'static [KnownImage] {
    KNOWN_IMAGES
}

// ============================================================================
// Download and conversion pipeline
// ============================================================================

/// Default directory for storing downloaded images.
pub fn image_dir() -> PathBuf {
    let dir = PathBuf::from("images");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Pull (download and convert) an image by name.
///
/// Returns the path to the ready-to-use ext4 image. With `force_refetch`,
/// the HTTP-conditional cache is bypassed and the body is re-downloaded
/// even if the upstream's ETag/Last-Modified hasn't changed; the
/// already-converted `.ext4` short-circuit at the top still applies
/// because that's a separate "I already have the final artifact"
/// signal and re-converting is far slower than the conditional GET.
pub fn pull_image(name: &str, output: Option<&Path>, force_refetch: bool) -> Result<PathBuf> {
    let image = get_known_image(name).ok_or_else(|| {
        Error::bad_request(format!(
            "Unknown image '{}'. Use 'image list' to see available images.",
            name
        ))
    })?;

    let dir = image_dir();
    let final_path = output
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join(format!("{}.ext4", image.name)));

    if final_path.exists() && !force_refetch {
        eprintln!("Image already exists at {}", final_path.display());
        eprintln!("Delete it first or pass --refetch if you want to re-download.");
        return Ok(final_path);
    }

    eprintln!("Pulling {} ...", image.name);
    eprintln!("  {}", image.description);
    eprintln!("  URL: {}", image.url);

    // Step 1: Download. Anchor the sidecar at `final_path` (the
    // .ext4 that survives the convert step) so the cache check
    // works on a re-pull when the download intermediate is gone.
    // See #26.
    let download_path = download_file(
        image.url,
        &dir,
        image.compression,
        &final_path,
        force_refetch,
    )?;

    // Step 2: Convert to ext4 if needed
    let ext4_path = convert_to_ext4(&download_path, image.format, &final_path)?;

    // Step 3: Resize if configured
    if !image.default_size.is_empty() {
        resize_image(&ext4_path, image.default_size)?;
    }

    // Clean up intermediate files
    if download_path != ext4_path && download_path.exists() {
        let _ = fs::remove_file(&download_path);
    }

    eprintln!("Image ready: {}", ext4_path.display());
    if !image.default_user.is_empty() {
        eprintln!("  Default user: {}", image.default_user);
        if !image.default_password.is_empty() {
            eprintln!("  Default password: {}", image.default_password);
        }
    }
    if image.cloud_init {
        eprintln!("  Cloud-init: supported (use --cloud-init for custom setup)");
    }

    Ok(ext4_path)
}

/// Download a file via wget and run any requested decompression.
///
/// Layout: wget downloads to `<dir>/<filename>` (the URL's basename),
/// using `fetch::download_to` for temp+cleanup. For Xz, we then run
/// `xz -d` on the downloaded file, which consumes it and leaves
/// `<filename without .xz>`. For Zip, we unzip into `dir` and locate
/// the extracted image. For None, the downloaded file is the result.
fn download_file(
    url: &str,
    dir: &Path,
    compression: Compression,
    sidecar_anchor: &Path,
    force_refetch: bool,
) -> Result<PathBuf> {
    let filename = url.rsplit('/').next().unwrap_or("download");
    let download_path = dir.join(filename);

    crate::fetch::download_to_cached(url, &download_path, sidecar_anchor, force_refetch)?;

    match compression {
        Compression::None => Ok(download_path),
        Compression::Xz => {
            eprintln!("  Decompressing (xz)...");
            let status = Command::new("xz")
                .args(["-d", "-f"])
                .arg(&download_path)
                .status()
                .map_err(|e| {
                    Error::internal(format!(
                        "Failed to run xz: {}. Install with: apt install xz-utils",
                        e
                    ))
                })?;
            if !status.success() {
                return Err(Error::internal("xz decompression failed"));
            }
            // `xz -d` strips `.xz` from the input filename.
            Ok(dir.join(filename.trim_end_matches(".xz")))
        }
        Compression::Zip => {
            eprintln!("  Extracting (zip)...");
            let status = Command::new("unzip")
                .args(["-o", "-j", "-d"])
                .arg(dir)
                .arg(&download_path)
                .status()
                .map_err(|e| {
                    Error::internal(format!(
                        "Failed to run unzip: {}. Install with: apt install unzip",
                        e
                    ))
                })?;
            let _ = fs::remove_file(&download_path);
            if !status.success() {
                return Err(Error::internal("unzip failed"));
            }
            // For the tt-bh-linux zip, the extracted file is debian-riscv64.img
            let extracted = dir.join("debian-riscv64.img");
            if extracted.exists() {
                return Ok(extracted);
            }
            // Try to find any .img or .ext4 file that was extracted
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(ext) = path.extension() {
                        if ext == "img" || ext == "ext4" {
                            return Ok(path);
                        }
                    }
                }
            }
            Err(Error::internal("Could not find extracted image file"))
        }
    }
}

/// Convert a downloaded image to a raw ext4 filesystem.
fn convert_to_ext4(input: &Path, format: ImageFormat, output: &Path) -> Result<PathBuf> {
    match format {
        ImageFormat::Ext4 => {
            // Already ext4, just rename/move
            if input != output {
                fs::rename(input, output)
                    .or_else(|_| fs::copy(input, output).map(|_| ()))
                    .map_err(Error::io_ctx("Failed to move image"))?;
                let _ = fs::remove_file(input);
            }
            Ok(output.to_path_buf())
        }
        ImageFormat::Qcow2 => {
            // Convert qcow2 to raw disk first
            eprintln!("  Converting qcow2 to raw...");
            let raw_path = input.with_extension("raw");
            let status = Command::new("qemu-img")
                .args(["convert", "-f", "qcow2", "-O", "raw"])
                .arg(input)
                .arg(&raw_path)
                .status()
                .map_err(|e| {
                    Error::internal(format!(
                        "Failed to run qemu-img: {}. Install with: apt install qemu-utils",
                        e
                    ))
                })?;
            if !status.success() {
                return Err(Error::internal("qemu-img convert failed"));
            }
            let _ = fs::remove_file(input);
            // Now extract partition from raw disk
            extract_root_partition(&raw_path, output)?;
            let _ = fs::remove_file(&raw_path);
            Ok(output.to_path_buf())
        }
        ImageFormat::RawDisk => {
            // Extract the root partition from GPT disk
            extract_root_partition(input, output)?;
            let _ = fs::remove_file(input);
            Ok(output.to_path_buf())
        }
    }
}

/// Extract the root (largest) partition from a GPT/MBR disk image.
///
/// Uses `sfdisk --json` to find partition offsets, then `dd` to extract.
fn extract_root_partition(disk: &Path, output: &Path) -> Result<()> {
    eprintln!("  Extracting root partition...");

    // Parse partition table with sfdisk
    let sfdisk_output = Command::new("sfdisk")
        .args(["--json"])
        .arg(disk)
        .output()
        .map_err(|e| {
            Error::internal(format!(
                "Failed to run sfdisk: {}. Install with: apt install fdisk",
                e
            ))
        })?;

    if !sfdisk_output.status.success() {
        return Err(Error::internal(format!(
            "sfdisk failed: {}",
            String::from_utf8_lossy(&sfdisk_output.stderr)
        )));
    }

    let json_str = String::from_utf8_lossy(&sfdisk_output.stdout);

    // Simple JSON parsing for partition info — find the largest partition
    // sfdisk JSON format: {"partitiontable":{"partitions":[{"start":N,"size":N,...}]}}
    let (start_sectors, size_sectors) = parse_largest_partition(&json_str)?;

    // Extract with dd (sector size = 512)
    let status = Command::new("dd")
        .arg(format!("if={}", disk.display()))
        .arg(format!("of={}", output.display()))
        .arg("bs=512")
        .arg(format!("skip={}", start_sectors))
        .arg(format!("count={}", size_sectors))
        .arg("status=progress")
        .status()
        .map_err(|e| Error::internal(format!("Failed to run dd: {}", e)))?;

    if !status.success() {
        return Err(Error::internal("dd failed to extract partition"));
    }

    // Verify it's a valid ext4 filesystem
    let fsck_status = Command::new("e2fsck")
        .args(["-f", "-y"])
        .arg(output)
        .status();

    match fsck_status {
        Ok(s) if s.success() || s.code() == Some(1) => {
            // Exit code 0 = clean, 1 = errors corrected — both OK
        }
        Ok(s) => {
            eprintln!(
                "  Warning: e2fsck returned exit code {:?}. The partition may not be ext4.",
                s.code()
            );
        }
        Err(e) => {
            eprintln!(
                "  Warning: could not run e2fsck: {}. Install with: apt install e2fsprogs",
                e
            );
        }
    }

    Ok(())
}

/// Parse sfdisk JSON output to find the largest partition (by sector count).
/// Returns (start_sector, size_sectors).
///
/// sfdisk emits `{"partitiontable":{"partitions":[{"start":N,"size":N,...},...]}}`.
/// We pick the partition with the largest `size`; that's reliably the rootfs
/// for the cloud images we convert (much larger than the `/boot` / EFI
/// partitions that sit alongside it).
fn parse_largest_partition(json: &str) -> Result<(u64, u64)> {
    let root: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| Error::internal(format!("sfdisk emitted non-JSON: {}", e)))?;
    let partitions = root
        .pointer("/partitiontable/partitions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::internal("sfdisk JSON missing /partitiontable/partitions array"))?;

    let (best_start, best_size) = partitions
        .iter()
        .filter_map(|p| {
            let start = p.get("start")?.as_u64()?;
            let size = p.get("size")?.as_u64()?;
            Some((start, size))
        })
        .max_by_key(|(_, size)| *size)
        .ok_or_else(|| Error::internal("No partitions found in disk image"))?;

    eprintln!(
        "  Found root partition: start={}, size={} sectors ({} MB)",
        best_start,
        best_size,
        best_size * 512 / (1024 * 1024)
    );

    Ok((best_start, best_size))
}

/// Resize an ext4 image to the given size.
fn resize_image(path: &Path, size: &str) -> Result<()> {
    eprintln!("  Resizing to {}...", size);

    // First resize the file
    let status = Command::new("qemu-img")
        .args(["resize", "-f", "raw"])
        .arg(path)
        .arg(size)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(_) | Err(_) => {
            // Fallback: use truncate
            let size_bytes = parse_size(size)?;
            let file = fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(Error::io_ctx("Failed to open image for resize"))?;
            file.set_len(size_bytes)
                .map_err(Error::io_ctx("Failed to resize image"))?;
        }
    }

    // Then resize the filesystem
    let status = Command::new("e2fsck").args(["-f", "-y"]).arg(path).status();
    if let Ok(s) = status {
        if !s.success() && s.code() != Some(1) {
            eprintln!("  Warning: e2fsck returned {:?}", s.code());
        }
    }

    let status = Command::new("resize2fs").arg(path).status().map_err(|e| {
        Error::internal(format!(
            "Failed to run resize2fs: {}. Install with: apt install e2fsprogs",
            e
        ))
    })?;

    if !status.success() {
        eprintln!("  Warning: resize2fs failed. Image may not use full disk size.");
    }

    Ok(())
}

/// Parse a size string like "10G" or "2T" to bytes.
fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        return Err(Error::bad_request("empty size string"));
    }

    let (num_str, suffix) = if s.ends_with('G') || s.ends_with('g') {
        (&s[..s.len() - 1], 1024u64 * 1024 * 1024)
    } else if s.ends_with('T') || s.ends_with('t') {
        (&s[..s.len() - 1], 1024u64 * 1024 * 1024 * 1024)
    } else if s.ends_with('M') || s.ends_with('m') {
        (&s[..s.len() - 1], 1024u64 * 1024)
    } else {
        (s, 1u64)
    };

    let num: u64 = num_str
        .parse()
        .map_err(|e| Error::bad_request(format!("Invalid size '{}': {}", s, e)))?;
    Ok(num * suffix)
}

// ============================================================================
// CLI command handlers
// ============================================================================

/// Print a table of available images.
pub fn cmd_list_available() {
    println!("{:<20} {:<65} {:<10}", "NAME", "DESCRIPTION", "ALIASES");
    println!("{}", "-".repeat(95));
    for img in KNOWN_IMAGES {
        let aliases = img.aliases.join(", ");
        println!("{:<20} {:<65} {:<10}", img.name, img.description, aliases);
    }
}

/// Print details about a specific image.
pub fn cmd_image_info(name: &str) {
    match get_known_image(name) {
        Some(img) => {
            println!("Name:          {}", img.name);
            println!("Description:   {}", img.description);
            println!("URL:           {}", img.url);
            println!("Format:        {:?}", img.format);
            println!("Compression:   {:?}", img.compression);
            println!(
                "Resize to:     {}",
                if img.default_size.is_empty() {
                    "none"
                } else {
                    img.default_size
                }
            );
            println!("Aliases:       {}", img.aliases.join(", "));
            if !img.default_user.is_empty() {
                println!("Default user:  {}", img.default_user);
            }
            if !img.default_password.is_empty() {
                println!("Default pass:  {}", img.default_password);
            }
            println!(
                "Cloud-init:    {}",
                if img.cloud_init { "yes" } else { "no" }
            );
        }
        None => {
            eprintln!(
                "Unknown image '{}'. Use 'image list' to see available images.",
                name
            );
            std::process::exit(1);
        }
    }
}

/// Pull an image by name.
pub fn cmd_pull(name: &str, output: Option<&str>, force_refetch: bool) {
    match pull_image(name, output.map(Path::new), force_refetch) {
        Ok(path) => {
            println!("{}", path.display());
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

    /// Representative sfdisk JSON for a two-partition cloud image (a tiny
    /// EFI/boot partition + a large root partition). The root is the one
    /// with the larger `size`, which is what `parse_largest_partition`
    /// must select regardless of partition order in the array.
    const SFDISK_SAMPLE: &str = r#"{
        "partitiontable": {
            "label": "gpt",
            "id": "A1B2C3D4-0000-0000-0000-000000000000",
            "device": "/tmp/img.raw",
            "unit": "sectors",
            "firstlba": 34,
            "lastlba": 4194270,
            "partitions": [
                {"node": "/tmp/img.raw1", "start": 2048,   "size": 204800,  "type": "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"},
                {"node": "/tmp/img.raw2", "start": 206848, "size": 3985407, "type": "0FC63DAF-8483-4772-8E79-3D69D8477DE4"}
            ]
        }
    }"#;

    #[test]
    fn parse_largest_partition_picks_biggest_by_size() {
        let (start, size) = parse_largest_partition(SFDISK_SAMPLE).unwrap();
        assert_eq!(start, 206848);
        assert_eq!(size, 3985407);
    }

    #[test]
    fn parse_largest_partition_rejects_non_json() {
        let err = parse_largest_partition("<html>nope</html>")
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-JSON"), "got: {}", err);
    }

    #[test]
    fn parse_largest_partition_rejects_missing_partitions_array() {
        let err = parse_largest_partition(r#"{"partitiontable": {}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("partitions array"), "got: {}", err);
    }

    #[test]
    fn parse_largest_partition_rejects_empty_partitions_array() {
        let err = parse_largest_partition(r#"{"partitiontable": {"partitions": []}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("No partitions"), "got: {}", err);
    }

    #[test]
    fn parse_largest_partition_survives_partitions_without_start_or_size() {
        // A buggy sfdisk emitting a partition record with missing fields
        // shouldn't crash us — we skip it and pick from the well-formed ones.
        let json = r#"{"partitiontable": {"partitions": [
            {"node": "/tmp/x1"},
            {"node": "/tmp/x2", "start": 2048, "size": 42}
        ]}}"#;
        let (start, size) = parse_largest_partition(json).unwrap();
        assert_eq!((start, size), (2048, 42));
    }

    #[test]
    fn get_known_image_finds_exact_name() {
        let img = get_known_image("debian-13").expect("debian-13 should be known");
        assert_eq!(img.name, "debian-13");
    }

    #[test]
    fn get_known_image_finds_alias() {
        let img = get_known_image("debian").expect("`debian` alias should resolve");
        assert_eq!(img.name, "debian-13");
    }

    #[test]
    fn get_known_image_returns_none_for_unknown() {
        assert!(get_known_image("fedora-40").is_none());
    }

    #[test]
    fn get_known_image_treats_empty_string_as_unknown() {
        assert!(get_known_image("").is_none());
    }
}
