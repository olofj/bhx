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
//! This module shells out to `xz`, `unzip`, `qemu-img`, `sfdisk`,
//! `dd`, `e2fsck`, `resize2fs` by basename — `Command::new("xz")`
//! resolves via `$PATH`. A malicious `$PATH` (or a shell function
//! shadowing one of these names) could substitute a different binary.
//! That's accepted: these helpers run as the operator's own user from
//! the CLI, never inside the daemon. An attacker who can already
//! corrupt the operator's `$PATH` already has equivalent access to do
//! anything else the operator can. Resolving via `which` once at
//! startup wouldn't change the threat model — it'd be the same
//! `which` lookup against the same `$PATH`, just done earlier.
//!
//! HTTP downloads themselves go through `crate::fetch::download_to_*`
//! (native ureq), so the `wget` binary is no longer a runtime dep.

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
    /// Whether to extract the largest partition as the final
    /// artifact (true → land an `.ext4` single-FS image) or land
    /// the whole partitioned disk image (false → `.img`). Pairs
    /// with `needs_bootloader` below: an image whose disk has a
    /// GPT and an ESP wants both fields false→true (whole disk,
    /// boot via U-Boot/EFI); a single-FS rootfs image wants
    /// true→false (extract, boot kernel directly).
    pub extract_partition: bool,
    /// Whether the on-disk image expects U-Boot to read the
    /// partition table and chainload /boot/EFI/* (true), versus
    /// the host loading kernel + initrd directly and pointing
    /// `root=/dev/vda` at a single-FS partition image (false).
    /// The boot subcommand picks `BootDevice::Uboot` vs
    /// `BootDevice::Vda` accordingly.
    pub needs_bootloader: bool,
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
        aliases: &["tt"],
        format: ImageFormat::Ext4, // After unzip, it's a raw ext4 image
        compression: Compression::Zip,
        default_size: "10G",
        default_user: "debian",
        default_password: "debian",
        cloud_init: true,
        // Already a single ext4 — no partition table to extract from.
        extract_partition: false,
        // The legacy direct-boot path: kernel + initrd loaded by the
        // host, root=/dev/vda points straight at this single-FS image.
        needs_bootloader: false,
    },
    // ========================================================================
    // Debian 13 — generic cloud image (cloud-init required for first-boot setup).
    // Note: the upstream `nocloud` variant exists too, but its first-boot service
    // set wedges on Tenstorrent Blackhole partway through systemd init (issue
    // observed: kernel + virtio plumbing fine, sshd never reaches listen, no
    // RCU stalls). The cloud-init variant boots clean to login on the same
    // hardware, so we expose only that.
    // ========================================================================
    KnownImage {
        name: "debian-13",
        url: "https://cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-riscv64.raw",
        description: "Debian 13 (Trixie) generic — official cloud image, needs cloud-init",
        aliases: &["default", "debian", "trixie"],
        format: ImageFormat::RawDisk,
        compression: Compression::None,
        default_size: "10G",
        default_user: "",
        default_password: "",
        cloud_init: true,
        // Whole disk — boot via U-Boot + EFI which reads GPT and
        // chainloads /boot/EFI from the ESP partition.
        extract_partition: false,
        needs_bootloader: true,
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
        extract_partition: false,
        needs_bootloader: true,
    },
    // ========================================================================
    // Fedora 42 (riscv64). Two flavors mirror the Debian pattern:
    //   * fedora-42         — Server-Host-Generic, .raw.xz (whole disk).
    //                         Same RawDisk+Xz pipeline as Ubuntu. Has the
    //                         full server package set; first-boot setup
    //                         goes through cloud-init when seeded with a
    //                         NoCloud drive (otherwise no usable login).
    //   * fedora-42-cloud   — Cloud-Base-Generic, .qcow2. Smaller image
    //                         aimed at cloud-init-driven provisioning;
    //                         requires a NoCloud seed for first login.
    // Both boot via U-Boot+EFI off the GPT/ESP on the disk.
    // ========================================================================
    KnownImage {
        name: "fedora-42",
        url: "https://dl.fedoraproject.org/pub/alt/risc-v/release/42/Server/riscv64/images/Fedora-Server-Host-Generic-42.20250911-2251ba41cdd3.riscv64.raw.xz",
        description: "Fedora 42 Server Host Generic — full server image, riscv64",
        aliases: &["fedora", "fedora-server"],
        format: ImageFormat::RawDisk,
        compression: Compression::Xz,
        default_size: "10G",
        default_user: "",
        default_password: "",
        cloud_init: true,
        extract_partition: false,
        needs_bootloader: true,
    },
    KnownImage {
        name: "fedora-42-cloud",
        url: "https://dl.fedoraproject.org/pub/alt/risc-v/release/42/Cloud/riscv64/images/Fedora-Cloud-Base-Generic-42.20250911-2251ba41cdd3.riscv64.qcow2",
        description: "Fedora 42 Cloud Base — small cloud image, riscv64 (needs cloud-init)",
        aliases: &["fedora-cloud"],
        format: ImageFormat::Qcow2,
        compression: Compression::None,
        default_size: "10G",
        default_user: "",
        default_password: "",
        cloud_init: true,
        extract_partition: false,
        needs_bootloader: true,
    },
    // AlmaLinux Kitten 10: the community RHEL10 development branch
    // (downstream of CentOS Stream 10, upstream of AlmaLinux 10 stable).
    // The mainline AlmaLinux 10 release-train doesn't ship riscv64 yet —
    // only Kitten does. URL is the moving "-latest" pointer; the
    // HTTP-conditional cache in fetch.rs (#26) re-pulls only when
    // upstream's ETag/Last-Modified changes.
    KnownImage {
        name: "almalinux-10-kitten",
        url: "https://repo.almalinux.org/almalinux-kitten/10-kitten/cloud/riscv64/images/AlmaLinux-Kitten-GenericCloud-10-latest.riscv64.qcow2",
        description: "AlmaLinux Kitten 10 GenericCloud — community RHEL10 dev branch riscv64",
        aliases: &["almalinux", "alma", "kitten"],
        format: ImageFormat::Qcow2,
        compression: Compression::None,
        default_size: "10G",
        default_user: "",
        default_password: "",
        cloud_init: true,
        extract_partition: false,
        needs_bootloader: true,
    },
    // openSUSE Tumbleweed JeOS — "Just Enough Operating System": SUSE's
    // minimal-footprint image, the openSUSE counterpart to Fedora's
    // Cloud-Base-Generic and AlmaLinux's GenericCloud. Adds a non-
    // RHEL/non-Debian-family distro to the catalog. Boot path matches
    // fedora-42: GPT + ESP, U-Boot reads the partition table and
    // chainloads /boot/EFI. URL is a stable symlink to the latest
    // snapshot — fetch.rs's HTTP-conditional cache re-pulls only when
    // upstream's ETag/Last-Modified changes.
    //
    // First-boot UX is different from the other generic-cloud images
    // here: no cloud-init in the image, and no preset root password.
    // Instead, the `jeos-firstboot` systemd service runs on the
    // console on first boot and walks the operator through license,
    // locale, keyboard, timezone, root password, and network. Walk
    // it via `bhx connect` immediately after `bhx boot`.
    KnownImage {
        name: "opensuse-tumbleweed",
        url: "https://download.opensuse.org/ports/riscv/tumbleweed/appliances/openSUSE-Tumbleweed-RISC-V-JeOS-efi.riscv64.raw.xz",
        description: "openSUSE Tumbleweed JeOS — minimal rolling-release image (interactive firstboot via console)",
        aliases: &["opensuse", "tumbleweed", "suse", "jeos"],
        format: ImageFormat::RawDisk,
        compression: Compression::Xz,
        default_size: "10G",
        default_user: "",
        default_password: "",
        cloud_init: false,
        extract_partition: false,
        needs_bootloader: true,
    },
];

/// Look up a known image by name or alias.
pub fn get_known_image(name: &str) -> Option<&'static KnownImage> {
    if let Some(img) = KNOWN_IMAGES.iter().find(|img| img.name == name) {
        return Some(img);
    }
    KNOWN_IMAGES.iter().find(|img| img.aliases.contains(&name))
}

/// Map a disk path back to its [`KnownImage`] entry, if the basename
/// (minus `.ext4` / `.img` extension) matches a known image's `name`.
///
/// `pull_image` lands artifacts at `images/<name>.{ext4,img}`, so a
/// boot client passing `--disk images/almalinux-10-kitten.img` (or a
/// symlink with the same basename) can recover the image's metadata
/// — including `needs_bootloader` — without the user having to repeat
/// it on the command line.
pub fn known_image_for_disk(path: &Path) -> Option<&'static KnownImage> {
    let stem = path.file_stem()?.to_str()?;
    KNOWN_IMAGES.iter().find(|img| img.name == stem)
}

/// Whether the on-disk artifact for this image is a single-FS file
/// (true → `.ext4`) or a whole partitioned disk (false → `.img`).
///
/// True iff we explicitly extracted the partition or the source was
/// already raw `Ext4` (no partition table to extract from). False
/// otherwise — i.e. RawDisk/Qcow2 sources we kept whole.
pub fn is_single_fs_artifact(image: &KnownImage) -> bool {
    image.extract_partition || matches!(image.format, ImageFormat::Ext4)
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
/// Output suffix tracks the artifact's shape:
///   * `.ext4` — single-FS image. Lands when `extract_partition=true`
///     (we extracted the largest partition out of a partitioned source)
///     or when the source format is already raw `Ext4`.
///   * `.img` — whole partitioned disk image, suitable for the
///     U-Boot + EFI boot path that reads GPT and chainloads
///     /boot/EFI from inside the disk.
pub fn pull_image(name: &str, output: Option<&Path>, force_refetch: bool) -> Result<PathBuf> {
    let image = get_known_image(name).ok_or_else(|| {
        Error::bad_request(format!(
            "Unknown image '{}'. Use 'image list' to see available images.",
            name
        ))
    })?;

    let dir = image_dir();
    let suffix = if is_single_fs_artifact(image) {
        "ext4"
    } else {
        "img"
    };
    let final_path = output
        .map(PathBuf::from)
        .unwrap_or_else(|| dir.join(format!("{}.{}", image.name, suffix)));

    if final_path.exists() && !force_refetch {
        eprintln!("Image already exists at {}", final_path.display());
        eprintln!("Delete it first or pass --refetch if you want to re-download.");
        // Still ensure the cidata sidecar exists for cloud-init
        // images (#115): an upgrade from a pre-#115 install where
        // the image was pulled previously without a seed should
        // get the default seed on first re-pull.
        ensure_cidata_seed(image, &final_path)?;
        return Ok(final_path);
    }

    eprintln!("Pulling {} ...", image.name);
    eprintln!("  {}", image.description);
    eprintln!("  URL: {}", image.url);

    // Step 1: Download. Anchor the sidecar at `final_path` (the
    // surviving artifact) so the cache check works on a re-pull when
    // the download intermediate is gone. See #26.
    let download_path = download_file(
        image.url,
        &dir,
        image.compression,
        &final_path,
        force_refetch,
    )?;

    // Step 2: Convert / optionally partition-extract.
    let final_path = convert_to_disk_image(
        &download_path,
        image.format,
        image.extract_partition,
        &final_path,
    )?;

    // Step 3: Resize the file. For `.ext4` (single-FS) we also grow
    // the filesystem in place via e2fsck + resize2fs. For `.img`
    // (whole disk) we only grow the file; cloud-init's growpart +
    // systemd-growfs (or the equivalent on first boot) extend the
    // partition + filesystem from inside the guest.
    if !image.default_size.is_empty() {
        resize_image(
            &final_path,
            image.default_size,
            is_single_fs_artifact(image),
        )?;
    }

    if download_path != final_path && download_path.exists() {
        let _ = fs::remove_file(&download_path);
    }

    eprintln!("Image ready: {}", final_path.display());
    if !image.default_user.is_empty() {
        eprintln!("  Default user: {}", image.default_user);
        if !image.default_password.is_empty() {
            eprintln!("  Default password: {}", image.default_password);
        }
    }

    ensure_cidata_seed(image, &final_path)?;

    Ok(final_path)
}

/// Ensure a default NoCloud seed sits next to the disk image (#115).
///
/// For images flagged `cloud_init=true` in the registry, write
/// `<basename>.cidata.img` if it doesn't already exist. The boot
/// path looks for this sibling and auto-attaches it as the
/// `--cloud-init` seed unless the operator passes `--no-cidata` or
/// an explicit `--cloud-init <other-path>`.
///
/// Idempotent: an existing seed is left alone (an operator may have
/// edited it). No-op for images without `cloud_init=true` — those
/// already ship with usable default credentials, so we don't need a
/// seed at all.
fn ensure_cidata_seed(image: &KnownImage, disk: &Path) -> Result<()> {
    if !image.cloud_init {
        return Ok(());
    }
    let seed_path = cidata_seed_path_for(disk);
    if seed_path.exists() {
        eprintln!(
            "  Cloud-init seed: {} (existing, not overwritten)",
            seed_path.display()
        );
        return Ok(());
    }
    crate::cloud_init::SeedSpec::default()
        .write_iso(&seed_path)
        .map_err(|e| Error::internal(format!("write default seed ISO: {}", e)))?;
    eprintln!(
        "  Cloud-init seed: {} (default user '{}' / password '{}')",
        seed_path.display(),
        crate::cloud_init::DEFAULT_USER,
        crate::cloud_init::DEFAULT_PASSWORD,
    );
    Ok(())
}

/// Default sibling path for the auto-generated NoCloud seed. The
/// boot path looks here when `--cloud-init` isn't explicitly given
/// and `--no-cidata` isn't set. See #115.
///
/// `images/debian-13.img` → `images/debian-13.cidata.img`. Strips
/// any final extension and appends `.cidata.img`.
pub fn cidata_seed_path_for(disk: &Path) -> PathBuf {
    let stem = disk
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".to_string());
    let mut p = disk.to_path_buf();
    p.set_file_name(format!("{}.cidata.img", stem));
    p
}

/// Download a file via fetch::download_to and run any requested decompression.
///
/// Layout: download lands at `<dir>/<filename>` (the URL's basename),
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

/// Convert a downloaded image to its final on-disk shape:
///
///   * `extract_partition = true` — pluck the largest partition from
///     the GPT/MBR disk and land it as the output. Used for the
///     direct-boot path that mounts `root=/dev/vda` against a single
///     ext4 filesystem.
///   * `extract_partition = false` — keep the whole partitioned disk
///     image. Qcow2 still gets a `qemu-img convert` to raw bytes;
///     RawDisk and Ext4 sources just move/rename. The output is
///     suitable for the U-Boot + EFI boot flow that reads GPT and
///     chainloads /boot/EFI from inside the disk.
fn convert_to_disk_image(
    input: &Path,
    format: ImageFormat,
    extract_partition: bool,
    output: &Path,
) -> Result<PathBuf> {
    match format {
        ImageFormat::Ext4 => {
            // Already ext4, just rename/move. (extract_partition is
            // moot here — there's no partition table.)
            if input != output {
                fs::rename(input, output)
                    .or_else(|_| fs::copy(input, output).map(|_| ()))
                    .map_err(Error::io_ctx("Failed to move image"))?;
                let _ = fs::remove_file(input);
            }
            Ok(output.to_path_buf())
        }
        ImageFormat::Qcow2 => {
            eprintln!("  Converting qcow2 to raw...");
            // For extract_partition we need a temporary raw disk to
            // run sfdisk against; for whole-disk we can convert
            // straight into the output path.
            let raw_dest = if extract_partition {
                input.with_extension("raw")
            } else {
                output.to_path_buf()
            };
            let status = Command::new("qemu-img")
                .args(["convert", "-f", "qcow2", "-O", "raw"])
                .arg(input)
                .arg(&raw_dest)
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
            if extract_partition {
                extract_root_partition(&raw_dest, output)?;
                let _ = fs::remove_file(&raw_dest);
            }
            Ok(output.to_path_buf())
        }
        ImageFormat::RawDisk => {
            if extract_partition {
                extract_root_partition(input, output)?;
                let _ = fs::remove_file(input);
            } else if input != output {
                fs::rename(input, output)
                    .or_else(|_| fs::copy(input, output).map(|_| ()))
                    .map_err(Error::io_ctx("Failed to move image"))?;
                let _ = fs::remove_file(input);
            }
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

    // No fsck here — `pull_image` detects the filesystem from magic
    // bytes and runs the right tool (e2fsck for ext4, nothing for
    // xfs/btrfs which need a mounted FS for offline check anyway).
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

/// Grow the image file to the given size, and (when `is_single_fs`)
/// grow the ext4 filesystem inside it to match. For whole-disk
/// images (`!is_single_fs`) we only grow the file; the guest's
/// first-boot cloud-init growpart + systemd-growfs extend the
/// partition + filesystem from inside the running guest.
fn resize_image(path: &Path, size: &str, is_single_fs: bool) -> Result<()> {
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

    if !is_single_fs {
        eprintln!(
            "  Whole-disk image: file grown to {} but partitions + FS stay at \
             their original extent. Guest's cloud-init growpart / systemd-growfs \
             will expand on first boot.",
            size
        );
        return Ok(());
    }

    // Single-FS image — assumed ext4 by the caller. Grow the FS.
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
    println!("{:<20} {:<75} {:<10}", "NAME", "DESCRIPTION", "ALIASES");
    println!("{}", "-".repeat(107));
    for img in KNOWN_IMAGES {
        let aliases = img.aliases.join(", ");
        println!("{:<20} {:<75} {:<10}", img.name, img.description, aliases);
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
            println!(
                "Layout:        {}",
                if is_single_fs_artifact(img) {
                    "single-FS .ext4"
                } else {
                    "whole partitioned disk (.img)"
                }
            );
            println!(
                "Boot path:     {}",
                if img.needs_bootloader {
                    "U-Boot + EFI (chainload /boot/EFI from disk)"
                } else {
                    "direct kernel (host loads Image + initrd)"
                }
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

    #[test]
    fn get_known_image_finds_almalinux_aliases() {
        for alias in ["almalinux-10-kitten", "almalinux", "alma", "kitten"] {
            let img = get_known_image(alias).unwrap_or_else(|| {
                panic!("alias `{}` should resolve to almalinux-10-kitten", alias)
            });
            assert_eq!(
                img.name, "almalinux-10-kitten",
                "alias `{}` resolved wrong",
                alias
            );
        }
    }

    #[test]
    fn single_fs_artifact_classifies_known_images() {
        // tt-debian: source format Ext4, no extraction needed → single-FS.
        let tt = get_known_image("tt-debian").unwrap();
        assert!(is_single_fs_artifact(tt));

        // AlmaLinux Kitten: Qcow2 source kept as a whole disk (GPT + ESP)
        // for U-Boot/EFI boot → not single-FS.
        let alma = get_known_image("almalinux").unwrap();
        assert!(!is_single_fs_artifact(alma));

        // Ubuntu/Debian cloud: RawDisk, extract_partition=false → whole disk.
        let ubu = get_known_image("ubuntu").unwrap();
        assert!(!is_single_fs_artifact(ubu));
        let deb = get_known_image("debian-13").unwrap();
        assert!(!is_single_fs_artifact(deb));
    }

    #[test]
    fn known_image_for_disk_matches_basename_stem() {
        // The pull pipeline lands AlmaLinux as `images/almalinux-10-kitten.img`
        // — the stem matches, so the boot subcommand can recover the
        // image's `needs_bootloader` from the disk path alone.
        let p = Path::new("images/almalinux-10-kitten.img");
        let img = known_image_for_disk(p).expect("alma should resolve");
        assert_eq!(img.name, "almalinux-10-kitten");
        assert!(img.needs_bootloader);

        // Single-FS artifact: tt-debian.ext4 → tt-debian entry.
        let p = Path::new("images/tt-debian.ext4");
        let img = known_image_for_disk(p).expect("tt-debian should resolve");
        assert_eq!(img.name, "tt-debian");
        assert!(!img.needs_bootloader);

        // Path with no recognisable basename → None.
        assert!(known_image_for_disk(Path::new("/tmp/random.img")).is_none());
        // No extension at all is fine — file_stem is the whole basename.
        assert!(known_image_for_disk(Path::new("almalinux-10-kitten")).is_some());
    }
}
