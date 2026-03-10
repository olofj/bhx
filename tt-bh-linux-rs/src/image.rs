// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! Image management — download, convert, and manage riscv64 rootfs images
//! for booting Linux on the Blackhole L2CPU.
//!
//! Inspired by the image management in ~/exe, adapted for riscv64 and the
//! Blackhole's requirement for raw ext4 filesystem images.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
/// Returns the path to the ready-to-use ext4 image.
pub fn pull_image(name: &str, output: Option<&Path>) -> Result<PathBuf, String> {
    let image = get_known_image(name)
        .ok_or_else(|| format!("Unknown image '{}'. Use 'image list' to see available images.", name))?;

    let dir = image_dir();
    let final_path = output
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join(format!("{}.ext4", image.name)));

    if final_path.exists() {
        eprintln!("Image already exists at {}", final_path.display());
        eprintln!("Delete it first if you want to re-download.");
        return Ok(final_path);
    }

    eprintln!("Pulling {} ...", image.name);
    eprintln!("  {}", image.description);
    eprintln!("  URL: {}", image.url);

    // Step 1: Download
    let download_path = download_file(image.url, &dir, image.compression)?;

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

/// Download a file using wget.
fn download_file(url: &str, dir: &Path, compression: Compression) -> Result<PathBuf, String> {
    let filename = url.rsplit('/').next().unwrap_or("download");
    let download_path = dir.join(filename);
    let temp_path = dir.join(format!("{}.downloading", filename));

    eprintln!("  Downloading...");

    let status = Command::new("wget")
        .args(["-O", temp_path.to_str().unwrap()])
        .arg("--progress=bar:force:noscroll")
        .arg(url)
        .status()
        .map_err(|e| format!("Failed to run wget: {}. Install with: apt install wget", e))?;

    if !status.success() {
        let _ = fs::remove_file(&temp_path);
        return Err("Download failed".to_string());
    }

    // Decompress if needed
    match compression {
        Compression::None => {
            fs::rename(&temp_path, &download_path)
                .map_err(|e| format!("Failed to rename download: {}", e))?;
            Ok(download_path)
        }
        Compression::Xz => {
            eprintln!("  Decompressing (xz)...");
            let decompressed = dir.join(filename.trim_end_matches(".xz"));
            let status = Command::new("xz")
                .args(["-d", "-k", "-f"])
                .arg(&temp_path)
                .status()
                .map_err(|e| format!("Failed to run xz: {}. Install with: apt install xz-utils", e))?;
            let _ = fs::remove_file(&temp_path);
            if !status.success() {
                return Err("xz decompression failed".to_string());
            }
            // xz -d removes the .xz suffix from the file
            let decompressed_from_xz = temp_path.with_extension("");
            if decompressed_from_xz.exists() && decompressed_from_xz != decompressed {
                fs::rename(&decompressed_from_xz, &decompressed)
                    .map_err(|e| format!("Failed to rename decompressed file: {}", e))?;
            }
            Ok(decompressed)
        }
        Compression::Zip => {
            eprintln!("  Extracting (zip)...");
            let status = Command::new("unzip")
                .args(["-o", "-j", "-d"])
                .arg(dir)
                .arg(&temp_path)
                .status()
                .map_err(|e| format!("Failed to run unzip: {}. Install with: apt install unzip", e))?;
            let _ = fs::remove_file(&temp_path);
            if !status.success() {
                return Err("unzip failed".to_string());
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
            Err("Could not find extracted image file".to_string())
        }
    }
}

/// Convert a downloaded image to a raw ext4 filesystem.
fn convert_to_ext4(
    input: &Path,
    format: ImageFormat,
    output: &Path,
) -> Result<PathBuf, String> {
    match format {
        ImageFormat::Ext4 => {
            // Already ext4, just rename/move
            if input != output {
                fs::rename(input, output)
                    .or_else(|_| fs::copy(input, output).map(|_| ()))
                    .map_err(|e| format!("Failed to move image: {}", e))?;
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
                .map_err(|e| format!("Failed to run qemu-img: {}. Install with: apt install qemu-utils", e))?;
            if !status.success() {
                return Err("qemu-img convert failed".to_string());
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
fn extract_root_partition(disk: &Path, output: &Path) -> Result<(), String> {
    eprintln!("  Extracting root partition...");

    // Parse partition table with sfdisk
    let sfdisk_output = Command::new("sfdisk")
        .args(["--json"])
        .arg(disk)
        .output()
        .map_err(|e| format!("Failed to run sfdisk: {}. Install with: apt install fdisk", e))?;

    if !sfdisk_output.status.success() {
        return Err(format!(
            "sfdisk failed: {}",
            String::from_utf8_lossy(&sfdisk_output.stderr)
        ));
    }

    let json_str = String::from_utf8_lossy(&sfdisk_output.stdout);

    // Simple JSON parsing for partition info — find the largest partition
    // sfdisk JSON format: {"partitiontable":{"partitions":[{"start":N,"size":N,...}]}}
    let (start_sectors, size_sectors) = parse_largest_partition(&json_str)?;

    // Extract with dd (sector size = 512)
    let status = Command::new("dd")
        .args([
            &format!("if={}", disk.to_str().unwrap()),
            &format!("of={}", output.to_str().unwrap()),
            "bs=512",
            &format!("skip={}", start_sectors),
            &format!("count={}", size_sectors),
            "status=progress",
        ])
        .status()
        .map_err(|e| format!("Failed to run dd: {}", e))?;

    if !status.success() {
        return Err("dd failed to extract partition".to_string());
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
fn parse_largest_partition(json: &str) -> Result<(u64, u64), String> {
    // Simple parser: find all "start":N and "size":N pairs
    let mut best_start: u64 = 0;
    let mut best_size: u64 = 0;

    // Split by partition entries — look for "start" and "size" fields
    let mut i = 0;
    let bytes = json.as_bytes();
    while i < bytes.len() {
        if let Some(pos) = json[i..].find("\"start\"") {
            let abs_pos = i + pos;
            let start = parse_json_number(&json[abs_pos..], "start");
            let size = parse_json_number(&json[abs_pos..], "size");

            if let (Some(s), Some(sz)) = (start, size) {
                if sz > best_size {
                    best_start = s;
                    best_size = sz;
                }
            }
            i = abs_pos + 7;
        } else {
            break;
        }
    }

    if best_size == 0 {
        return Err("No partitions found in disk image".to_string());
    }

    eprintln!(
        "  Found root partition: start={}, size={} sectors ({} MB)",
        best_start,
        best_size,
        best_size * 512 / (1024 * 1024)
    );

    Ok((best_start, best_size))
}

/// Parse a JSON number field like `"fieldname": 12345` from a substring.
fn parse_json_number(s: &str, field: &str) -> Option<u64> {
    let pattern = format!("\"{}\"", field);
    let pos = s.find(&pattern)?;
    let after_key = &s[pos + pattern.len()..];
    // Skip whitespace and colon
    let after_colon = after_key.find(':')? + 1;
    let num_start = &after_key[after_colon..];
    let num_str: String = num_start
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    num_str.parse().ok()
}

/// Resize an ext4 image to the given size.
fn resize_image(path: &Path, size: &str) -> Result<(), String> {
    eprintln!("  Resizing to {}...", size);

    // First resize the file
    let status = Command::new("qemu-img")
        .args(["resize", "-f", "raw"])
        .arg(path)
        .arg(size)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(_) => {
            // Fallback: use truncate
            let size_bytes = parse_size(size)?;
            let file = fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|e| format!("Failed to open image for resize: {}", e))?;
            file.set_len(size_bytes)
                .map_err(|e| format!("Failed to resize image: {}", e))?;
        }
        Err(_) => {
            // Fallback: use truncate
            let size_bytes = parse_size(size)?;
            let file = fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|e| format!("Failed to open image for resize: {}", e))?;
            file.set_len(size_bytes)
                .map_err(|e| format!("Failed to resize image: {}", e))?;
        }
    }

    // Then resize the filesystem
    let status = Command::new("e2fsck")
        .args(["-f", "-y"])
        .arg(path)
        .status();
    if let Ok(s) = status {
        if !s.success() && s.code() != Some(1) {
            eprintln!("  Warning: e2fsck returned {:?}", s.code());
        }
    }

    let status = Command::new("resize2fs")
        .arg(path)
        .status()
        .map_err(|e| format!("Failed to run resize2fs: {}. Install with: apt install e2fsprogs", e))?;

    if !status.success() {
        eprintln!("  Warning: resize2fs failed. Image may not use full disk size.");
    }

    Ok(())
}

/// Parse a size string like "10G" or "2T" to bytes.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size string".to_string());
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
        .map_err(|e| format!("Invalid size '{}': {}", s, e))?;
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
            println!("Resize to:     {}", if img.default_size.is_empty() { "none" } else { img.default_size });
            println!("Aliases:       {}", img.aliases.join(", "));
            if !img.default_user.is_empty() {
                println!("Default user:  {}", img.default_user);
            }
            if !img.default_password.is_empty() {
                println!("Default pass:  {}", img.default_password);
            }
            println!("Cloud-init:    {}", if img.cloud_init { "yes" } else { "no" });
        }
        None => {
            eprintln!("Unknown image '{}'. Use 'image list' to see available images.", name);
            std::process::exit(1);
        }
    }
}

/// Pull an image by name.
pub fn cmd_pull(name: &str, output: Option<&str>) {
    match pull_image(name, output.map(Path::new)) {
        Ok(path) => {
            println!("{}", path.display());
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}
