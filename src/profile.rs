// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Profile catalog: named boot configurations stored in
//! `~/.config/bhx/profiles.yaml`.
//!
//! This module is the schema + persistence layer (#92). Boot
//! integration (compiling a profile into a `Request::Boot`) lives
//! in #93.
//!
//! Lifecycle:
//! - [`load_profiles`] reads the catalog (returns an empty
//!   [`ProfilesFile`] if absent).
//! - [`save_profiles`] writes atomically via a temp file + rename so a
//!   crash mid-write doesn't trash the operator's catalog.
//! - [`validate_profile`] runs schema-level checks: profile name regex,
//!   image resolves to a `KnownImage`, memory parses, hostname is
//!   RFC-952-clean, forwards parse, bootloader value is in the allowed set.
//!
//! Wire format mirrors the umbrella (#90). Top-level shape:
//!
//! ```yaml
//! profiles:
//!   alma-dev:
//!     image: almalinux-10-kitten
//!     memory: 2GB
//!     network: { enabled: true, hostname: alma-dev, forwards: ["5201:5201"] }
//!     console: { virtio: true }
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Top-level schema. The YAML file looks like
/// `{ profiles: { <name>: <Profile>, ... } }` so a future addition
/// (default settings, defaults: block, etc.) can sit alongside the
/// `profiles:` key without breaking existing consumers.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfilesFile {
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// One profile stanza. Most fields default to None / sensible values
/// so a minimal stanza (`image: <name>`) is a valid profile.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    /// `KnownImage::name` or any of its aliases.
    pub image: String,

    /// Operator-friendly memory size string (`2GB`, `1.5GiB`, `2048MB`).
    /// Parsed at apply-time via the same helper that backs the
    /// `--memory` CLI flag (#91). Defaults to "use the L2CPU's
    /// physical DRAM size" when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,

    /// `kernel` or `uboot`. Override for the default the image's
    /// `needs_bootloader` flag picks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootloader: Option<String>,

    /// Initramfs path. Ignored in `bootloader: uboot` mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initramfs: Option<String>,

    /// Root device (defaults to `vda` daemon-side).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_device: Option<String>,

    #[serde(default)]
    pub network: NetworkConfig,

    #[serde(default)]
    pub console: ConsoleConfig,

    /// Cloud-init NoCloud seed config. When `Some`, `bhx boot -c <name>`
    /// renders a seed ISO into the per-instance dir and attaches it as
    /// the second virtio-blk, same shape as `--cloud-init <path>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_init: Option<CloudInitConfig>,
}

/// Profile-side cloud-init knobs. Mirrors the on-disk shape of
/// [`crate::cloud_init::SeedSpec`] so the materialize step is a
/// straight field copy. Only the fields an operator typically wants to
/// set; advanced corners (`extra_user_data`) are exposed verbatim.
///
/// Validation: hostname is RFC-952-clean if set; other fields are
/// pass-through (passwords are sha512crypt'd at seed-build time, ssh
/// keys go straight into authorized_keys).
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct CloudInitConfig {
    /// Login name. None ⇒ `cloud_init::DEFAULT_USER` ("bhx").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Plain-text password. Hashed at seed build time. None +
    /// non-empty `ssh_keys` ⇒ key-only auth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,

    /// SSH public keys (each entry is a single OpenSSH-format line).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_keys: Vec<String>,

    /// Guest hostname. Must be RFC-952-clean if set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,

    /// cloud-init instance-id. None ⇒ random at seed-build time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,

    /// DNS resolvers baked into /etc/resolv.conf via bootcmd.
    /// Empty ⇒ `cloud_init::DEFAULT_NAMESERVER` (8.8.8.8).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nameservers: Vec<String>,

    /// Extra YAML appended verbatim to user-data (operator's escape
    /// hatch for `packages:`, `runcmd:`, `write_files:`, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_user_data: Option<String>,
}

impl CloudInitConfig {
    /// Materialize the profile config into a [`crate::cloud_init::SeedSpec`].
    pub fn to_seed_spec(&self) -> crate::cloud_init::SeedSpec {
        crate::cloud_init::SeedSpec {
            user: self.user.clone(),
            password: self.password.clone(),
            ssh_keys: self.ssh_keys.clone(),
            hostname: self.hostname.clone(),
            instance_id: self.instance_id.clone(),
            nameservers: self.nameservers.clone(),
            extra_user_data: self.extra_user_data.clone(),
        }
    }
}

/// Network sub-block.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetworkConfig {
    /// Default false — operator opts in.
    #[serde(default)]
    pub enabled: bool,
    /// RFC-952-clean override for the per-(card, l2cpu) DHCP hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Extra port forwards as `HOST:GUEST` strings (parsed at apply
    /// time so the profile is operator-readable).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forwards: Vec<String>,
}

/// Console sub-block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConsoleConfig {
    /// Attach a virtio-console (`/dev/hvc0`). Default true — the DTB
    /// bootargs direct the kernel console to hvc0, and stock distro
    /// kernels usually can't fall back to the SBI debug console
    /// (CONFIG_HVC_RISCV_SBI is not in upstream-portable builds), so
    /// without virtio-console the boot is silent. Set to false only
    /// to bisect virtio-console issues.
    #[serde(default = "default_true")]
    pub virtio: bool,
    /// Attach virtio-rng. Default true — the AlmaLinux EFI shim
    /// requires `EFI_RNG_PROTOCOL` (see #62).
    #[serde(default = "default_true")]
    pub rng: bool,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        ConsoleConfig {
            virtio: true,
            rng: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// First-run seed for `~/.config/bhx/profiles.yaml`. Written by
/// `bhx profile edit` when no catalog exists yet (#111). Operators
/// see real example stanzas to crib from instead of an empty
/// `profiles: {}` they have to build from memory.
///
/// Every example is commented so the seeded file parses to an empty
/// `ProfilesFile` (the active line is `profiles: {}`). Comments
/// don't survive a `save_profiles_to` round-trip — once the
/// operator defines a real profile, the templates fall away
/// naturally. That's fine; their job is done after the first edit.
pub const FIRST_RUN_TEMPLATE: &str = "\
# bhx profile catalog (~/.config/bhx/profiles.yaml).
#
# Each stanza under `profiles:` is a named override set for `bhx boot`,
# applied with `bhx boot -c <name>`. See:
#   - `bhx image list`   for valid `image:` values
#   - `bhx profile add`  for the imperative path that skips this file
#
# The map below is empty by default. Uncomment one of the templates,
# replace `profiles: {}` with the new stanza(s), and save.

profiles: {}

# ---------------------------------------------------------------------
# Templates — copy, uncomment, edit, remove the `profiles: {}` above.
# ---------------------------------------------------------------------
#
# profiles:
#
#   # Minimal: tt-debian smoke (no network, console-only).
#   tt-smoke:
#     image: tt-debian
#
#   # Debian + slirp networking + iperf3 forward.
#   debian-net:
#     image: debian-13
#     memory: 4GB
#     network:
#       enabled: true
#       hostname: deb-l0
#       forwards:
#         - \"5201:5201\"
#
#   # Fedora boot via U-Boot + EFI shim, 8 GiB.
#   fedora-uboot:
#     image: fedora-42
#     memory: 8GB
#     bootloader: uboot
";

/// Resolve `~/.config/bhx/profiles.yaml` via [`crate::xdg::config_home`].
/// Pure path construction; doesn't touch the filesystem.
pub fn profiles_path() -> Result<PathBuf> {
    Ok(crate::xdg::config_home()?.join("bhx").join("profiles.yaml"))
}

/// Load the profile catalog. Returns an empty [`ProfilesFile`] if the
/// file is absent — that's the steady-state for a brand-new operator.
/// I/O errors and YAML parse errors propagate.
pub fn load_profiles() -> Result<ProfilesFile> {
    let path = profiles_path()?;
    load_profiles_from(&path)
}

/// Test-friendly variant: load from an explicit path.
pub fn load_profiles_from(path: &Path) -> Result<ProfilesFile> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ProfilesFile::default()),
        Err(e) => {
            return Err(Error::Io {
                ctx: format!("read {}", path.display()),
                source: e,
            })
        }
    };
    if bytes.is_empty() {
        return Ok(ProfilesFile::default());
    }
    serde_yaml_ng::from_slice(&bytes)
        .map_err(|e| Error::bad_request(format!("parse {}: {}", path.display(), e)))
}

/// Atomic write: serialize to YAML, write to a sibling `.tmp` file,
/// rename into place. Partial writes never leave a half-baked
/// catalog visible to the next `load_profiles_from`.
pub fn save_profiles_to(profiles: &ProfilesFile, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(Error::io_ctx(format!("create dir {}", parent.display())))?;
    }
    let yaml = serde_yaml_ng::to_string(profiles)
        .map_err(|e| Error::internal(format!("serialize profiles: {}", e)))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, yaml.as_bytes()).map_err(Error::io_ctx(format!("write {}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(Error::io_ctx(format!(
        "rename {} -> {}",
        tmp.display(),
        path.display()
    )))?;
    Ok(())
}

/// Schema-level validation. Used by both `bhx profile add` (on the
/// freshly-edited single stanza) and `bhx profile edit` (sweeps every
/// profile in the catalog). Returns the first error encountered
/// rather than collecting; the editor retry loop re-displays whichever
/// stanza is broken first.
pub fn validate_profile(name: &str, p: &Profile) -> Result<()> {
    validate_profile_name(name)?;

    // Image must resolve to a known entry.
    if crate::image::get_known_image(&p.image).is_none() {
        return Err(Error::bad_request(format!(
            "profile {:?}: unknown image {:?} (run `bhx image list` for available)",
            name, p.image
        )));
    }

    if let Some(mem) = &p.memory {
        let bytes = parse_memory_str(mem).map_err(|e| {
            Error::bad_request(format!("profile {:?}: invalid memory: {}", name, e))
        })?;
        if bytes == 0 {
            return Err(Error::bad_request(format!(
                "profile {:?}: memory must be > 0",
                name
            )));
        }
    }

    if let Some(bl) = &p.bootloader {
        if bl != "kernel" && bl != "uboot" {
            return Err(Error::bad_request(format!(
                "profile {:?}: bootloader must be 'kernel' or 'uboot' (got {:?})",
                name, bl
            )));
        }
    }

    if let Some(host) = &p.network.hostname {
        validate_hostname(host).map_err(|e| {
            Error::bad_request(format!("profile {:?}: invalid hostname: {}", name, e))
        })?;
    }

    for fwd in &p.network.forwards {
        parse_forward(fwd).map_err(|e| {
            Error::bad_request(format!(
                "profile {:?}: invalid forward {:?}: {}",
                name, fwd, e
            ))
        })?;
    }

    if let Some(ci) = &p.cloud_init {
        if let Some(host) = &ci.hostname {
            validate_hostname(host).map_err(|e| {
                Error::bad_request(format!(
                    "profile {:?}: cloud_init.hostname invalid: {}",
                    name, e
                ))
            })?;
        }
    }

    Ok(())
}

/// Validate every stanza in a `ProfilesFile`. The CRUD CLI calls this
/// before `save_profiles` so the editor retry can re-prompt on any
/// validation failure.
pub fn validate_all(profiles: &ProfilesFile) -> Result<()> {
    for (name, p) in &profiles.profiles {
        validate_profile(name, p)?;
    }
    Ok(())
}

/// Profile name regex: `[a-zA-Z][a-zA-Z0-9_-]*`, ≤32 chars.
pub fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::bad_request("profile name is empty"));
    }
    if name.len() > 32 {
        return Err(Error::bad_request(format!(
            "profile name {:?} > 32 chars",
            name
        )));
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => {
            return Err(Error::bad_request(format!(
                "profile name {:?} must start with a letter",
                name
            )));
        }
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(Error::bad_request(format!(
                "profile name {:?} contains invalid char {:?} (allowed: a-zA-Z0-9_-)",
                name, c
            )));
        }
    }
    Ok(())
}

/// Strict RFC-952 / RFC-1123 hostname check (lowercase a-z0-9-, ≤63
/// chars, no leading/trailing dash).
fn validate_hostname(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(Error::bad_request("empty"));
    }
    if s.len() > 63 {
        return Err(Error::bad_request("longer than 63 chars (RFC 952)"));
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err(Error::bad_request("must not start or end with '-'"));
    }
    for c in s.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(Error::bad_request(format!(
                "only lowercase a-z, 0-9, '-' allowed (got {:?})",
                c
            )));
        }
    }
    Ok(())
}

/// `HOST:GUEST` → `(u16, u16)`. Both must be 1..=65535.
fn parse_forward(s: &str) -> Result<(u16, u16)> {
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

/// Parse a memory size string. Same accepted format as the `--memory`
/// CLI flag — kept in sync with `main::parse_memory` (the
/// CLI-side parser uses io::Result, this one uses crate::Result).
pub fn parse_memory_str(s: &str) -> Result<u64> {
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
    // `as u64` saturates rather than erroring on overflow, so a
    // malformed profile like `memory: 1e30GB` would otherwise yield
    // u64::MAX silently.
    if !bytes_f.is_finite() || bytes_f < 0.0 || bytes_f > u64::MAX as f64 {
        return Err(Error::bad_request(format!("memory {:?}: too large", s)));
    }
    Ok(bytes_f as u64)
}

// ============================================================================
// Per-instance disks (#93)
// ============================================================================

/// Resolve `~/.local/share/bhx/instances` via [`crate::xdg::data_subdir`].
pub fn instances_dir() -> Result<PathBuf> {
    crate::xdg::data_subdir("instances")
}

/// Per-(profile, l2cpu) instance directory. Each pair gets its own
/// writable disk so two L2CPUs running the same profile in parallel
/// don't trample each other's filesystem.
pub fn instance_dir(profile_name: &str, l2cpu_idx: u8) -> Result<PathBuf> {
    Ok(instances_dir()?.join(format!("{}-l{}", profile_name, l2cpu_idx)))
}

/// `disk.img` inside the instance directory. Test-time helper today;
/// production code reaches the disk via `clone_template_if_missing`'s
/// return value, so building without `--tests` sees this as unused.
#[allow(dead_code)]
pub fn instance_disk_path(profile_name: &str, l2cpu_idx: u8) -> Result<PathBuf> {
    Ok(instance_dir(profile_name, l2cpu_idx)?.join("disk.img"))
}

/// `meta.json` inside the instance directory. Records the template
/// the disk was cloned from so a future `profile reset` or staleness
/// detector has the provenance to compare against.
#[allow(dead_code)]
pub fn instance_meta_path(profile_name: &str, l2cpu_idx: u8) -> Result<PathBuf> {
    Ok(instance_dir(profile_name, l2cpu_idx)?.join("meta.json"))
}

/// Provenance for a cloned instance disk. Saved as `meta.json`
/// alongside `disk.img`. The sha is recorded at clone time so a
/// future operator running `bhx image pull <same>` to refresh the
/// template can be warned that their instance disks are stale.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstanceMeta {
    pub template_path: String,
    pub template_sha256_at_clone: Option<String>,
    pub cloned_at_unix_secs: u64,
}

/// Read the instance's `meta.json`. Returns Ok(None) if the file is
/// absent (no instance disk exists). Currently only the test exercise
/// it; the field set is shaped for a future stale-template warning.
#[allow(dead_code)]
pub fn read_instance_meta(profile_name: &str, l2cpu_idx: u8) -> Result<Option<InstanceMeta>> {
    let path = instance_meta_path(profile_name, l2cpu_idx)?;
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Error::Io {
                ctx: format!("read {}", path.display()),
                source: e,
            })
        }
    };
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn write_instance_meta_to(path: &Path, meta: &InstanceMeta) -> Result<()> {
    let json = serde_json::to_vec_pretty(meta)?;
    fs::write(path, json).map_err(Error::io_ctx(format!("write {}", path.display())))?;
    Ok(())
}

/// Clone `template` to `<instance_dir>/disk.img` if not already
/// present. Records `meta.json` alongside the disk. Returns
/// `(disk_path, was_cloned)` — `was_cloned == true` on the first
/// invocation, false on subsequent ones (idempotent).
///
/// Phase A uses a plain `fs::copy`; #83 will replace it with a
/// qcow2 backing-file overlay.
pub fn clone_template_if_missing(
    template_path: &Path,
    profile_name: &str,
    l2cpu_idx: u8,
) -> Result<(PathBuf, bool)> {
    let dir = instance_dir(profile_name, l2cpu_idx)?;
    let disk = dir.join("disk.img");
    let meta_path = dir.join("meta.json");
    if disk.exists() {
        return Ok((disk, false));
    }
    fs::create_dir_all(&dir).map_err(Error::io_ctx(format!("create dir {}", dir.display())))?;
    if !template_path.exists() {
        return Err(Error::bad_request(format!(
            "template image {} not found; run `bhx image pull` first",
            template_path.display()
        )));
    }
    fs::copy(template_path, &disk).map_err(Error::io_ctx(format!(
        "copy {} -> {}",
        template_path.display(),
        disk.display()
    )))?;
    let meta = InstanceMeta {
        template_path: template_path.display().to_string(),
        template_sha256_at_clone: None, // Hashing a 16 GiB file at every clone
        // is wasteful; defer to a follow-up if staleness detection
        // matters in practice (#83's qcow2 work makes the clone
        // near-zero-cost and the hash cheap).
        cloned_at_unix_secs: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    write_instance_meta_to(&meta_path, &meta)?;
    Ok((disk, true))
}

/// Remove the instance directory(ies) for a profile. Returns the
/// list of directories actually removed so the CLI can print a
/// diagnostic. With `l2cpu_filter == None`, sweeps every
/// `<profile>-l*/` directory in `instances_dir()`. With
/// `Some(idx)`, only removes that one.
pub fn reset_instances(profile_name: &str, l2cpu_filter: Option<u8>) -> Result<Vec<PathBuf>> {
    let dir = instances_dir()?;
    let mut removed = Vec::new();
    if !dir.exists() {
        return Ok(removed);
    }
    if let Some(idx) = l2cpu_filter {
        let target = instance_dir(profile_name, idx)?;
        if target.exists() {
            fs::remove_dir_all(&target)
                .map_err(Error::io_ctx(format!("remove {}", target.display())))?;
            removed.push(target);
        }
        return Ok(removed);
    }
    // Sweep every <profile>-l<n> subdir.
    for entry in fs::read_dir(&dir).map_err(Error::io_ctx(format!("read dir {}", dir.display())))? {
        let entry = entry.map_err(Error::io_ctx("readdir entry"))?;
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        if let Some((p_name, suffix)) = name.rsplit_once("-l") {
            if p_name == profile_name && suffix.parse::<u8>().is_ok() {
                let path = entry.path();
                fs::remove_dir_all(&path)
                    .map_err(Error::io_ctx(format!("remove {}", path.display())))?;
                removed.push(path);
            }
        }
    }
    Ok(removed)
}

/// Pick the operator's preferred editor: `$VISUAL` → `$EDITOR` → `vi`.
/// Plain string return so tests can inject without spawning anything.
pub fn pick_editor() -> String {
    std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "vi".to_string())
}

/// Trait covering the editor invocation so the visudo-style retry loop
/// can be exercised hardware-free. The production impl spawns `$EDITOR
/// <path>` and waits; tests inject a stub that mutates the file.
pub trait EditorRunner {
    /// Run the editor against `path`. Returns `Ok(())` on a clean
    /// exit (exit 0). Non-zero exit codes propagate as
    /// `Error::Internal` so the retry loop can surface them.
    fn edit(&mut self, path: &Path) -> Result<()>;
}

/// Default impl: spawn `$EDITOR <path>`, wait for exit.
pub struct ProcessEditor;

impl EditorRunner for ProcessEditor {
    fn edit(&mut self, path: &Path) -> Result<()> {
        use std::process::Command;
        let editor = pick_editor();
        let status = Command::new(&editor)
            .arg(path)
            .status()
            .map_err(|e| Error::internal(format!("spawn editor {:?}: {}", editor, e)))?;
        if !status.success() {
            return Err(Error::internal(format!(
                "editor {:?} exited {:?}",
                editor,
                status.code()
            )));
        }
        Ok(())
    }
}

/// Visudo-style retry: opens the editor on `path`, parses + validates
/// after each save, re-opens with the broken text on failure. Loop
/// caps at `max_attempts` so a stuck editor doesn't block the CLI
/// indefinitely.
///
/// On a successful edit, returns the parsed [`ProfilesFile`]. The
/// caller persists it (atomically) via [`save_profiles_to`] —
/// keeping the persist step out of the loop means tests can stub
/// the editor without also stubbing the file system.
///
/// `confirm` runs between a failed save and the next editor reopen.
/// Production passes [`stdin_retry_prompt`], which prints the
/// failure and blocks on a stdin read so the operator can either
/// press Enter to retry or Ctrl-C to abort. Tests pass a closure
/// that returns `Ok(())` to auto-retry. Returning `Err(_)` from
/// the closure aborts the loop with that error (so e.g. an
/// abort-on-EOF check could surface as a clean shutdown).
pub fn edit_with_retry<E, F>(
    editor: &mut E,
    path: &Path,
    max_attempts: usize,
    mut confirm: F,
) -> Result<ProfilesFile>
where
    E: EditorRunner,
    F: FnMut(usize, usize, &Error) -> Result<()>,
{
    for attempt in 1..=max_attempts {
        editor.edit(path)?;
        let outcome = match load_profiles_from(path) {
            Ok(profiles) => validate_all(&profiles).map(|()| profiles),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(profiles) => return Ok(profiles),
            Err(e) => {
                if attempt == max_attempts {
                    return Err(e);
                }
                confirm(attempt, max_attempts, &e)?;
            }
        }
    }
    unreachable!("loop body returns or errors at attempt == max_attempts")
}

/// Production retry prompt for `edit_with_retry`: print the
/// rejected-save error, then block on a stdin read so the operator
/// can review before re-entering the editor. Ctrl-C at the prompt
/// raises SIGINT and tears the process down with the canonical
/// catalog still untouched (we only edit a temp copy — see
/// `cmd_profile_edit`).
pub fn stdin_retry_prompt(attempt: usize, max_attempts: usize, error: &Error) -> Result<()> {
    eprintln!();
    eprintln!("save rejected: {}", error);
    eprintln!();
    eprintln!(
        "press Enter to re-open the editor (attempt {} of {}), or Ctrl-C to abort.",
        attempt + 1,
        max_attempts
    );
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .map_err(|e| Error::internal(format!("read stdin: {}", e)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // ---- ProfilesFile round-trip ----

    use crate::test_util::env_lock;

    /// Override `$XDG_DATA_HOME` for the duration of a test so
    /// `instances_dir()` lands inside our tempdir instead of the
    /// operator's real `~/.local/share`. Holds the process-wide
    /// env-var lock from `crate::test_util` for the guard's
    /// lifetime, so cross-module tests that mutate other env vars
    /// (lifetime.rs's `XDG_RUNTIME_DIR`) serialize against this one.
    struct DataHomeGuard {
        prev: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl DataHomeGuard {
        fn set(value: &Path) -> Self {
            let lock = env_lock();
            let prev = std::env::var_os("XDG_DATA_HOME");
            unsafe {
                std::env::set_var("XDG_DATA_HOME", value);
            }
            DataHomeGuard { prev, _lock: lock }
        }
    }

    impl Drop for DataHomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
            }
        }
    }

    // ---- Per-instance disks (#93) ----

    #[test]
    fn instance_path_layout_includes_profile_and_l2cpu() {
        let dir = tmp_dir();
        let _g = DataHomeGuard::set(dir.path());
        let path = instance_disk_path("alma-dev", 2).unwrap();
        let expected = dir
            .path()
            .join("bhx")
            .join("instances")
            .join("alma-dev-l2")
            .join("disk.img");
        assert_eq!(path, expected);

        let meta = instance_meta_path("alma-dev", 2).unwrap();
        assert_eq!(meta, expected.parent().unwrap().join("meta.json"));
    }

    #[test]
    fn clone_template_creates_disk_and_meta_first_time() {
        let dir = tmp_dir();
        let _g = DataHomeGuard::set(dir.path());
        let template = dir.path().join("template.img");
        fs::write(&template, b"template-bytes").unwrap();

        let (disk, was_cloned) = clone_template_if_missing(&template, "alma", 0).unwrap();
        assert!(was_cloned);
        assert!(disk.exists());
        assert_eq!(fs::read(&disk).unwrap(), b"template-bytes");

        let meta = read_instance_meta("alma", 0).unwrap().unwrap();
        assert_eq!(meta.template_path, template.display().to_string());
        assert!(meta.cloned_at_unix_secs > 0);
    }

    #[test]
    fn clone_template_is_idempotent_on_existing_disk() {
        let dir = tmp_dir();
        let _g = DataHomeGuard::set(dir.path());
        let template = dir.path().join("template.img");
        fs::write(&template, b"v1").unwrap();
        let (disk, first) = clone_template_if_missing(&template, "alma", 0).unwrap();
        assert!(first);
        // Mutate the template — second call must NOT re-copy.
        fs::write(&template, b"v2-different-bytes").unwrap();
        let (_, second) = clone_template_if_missing(&template, "alma", 0).unwrap();
        assert!(!second);
        assert_eq!(fs::read(&disk).unwrap(), b"v1");
    }

    #[test]
    fn clone_template_errors_when_template_missing() {
        let dir = tmp_dir();
        let _g = DataHomeGuard::set(dir.path());
        let template = dir.path().join("does-not-exist.img");
        let err = clone_template_if_missing(&template, "alma", 0).unwrap_err();
        assert!(matches!(err, Error::BadRequest(_)));
    }

    #[test]
    fn reset_instances_removes_only_matching_subdirs() {
        let dir = tmp_dir();
        let _g = DataHomeGuard::set(dir.path());
        let template = dir.path().join("template.img");
        fs::write(&template, b"x").unwrap();
        clone_template_if_missing(&template, "alma", 0).unwrap();
        clone_template_if_missing(&template, "alma", 1).unwrap();
        clone_template_if_missing(&template, "debian", 0).unwrap();

        let removed = reset_instances("alma", None).unwrap();
        assert_eq!(removed.len(), 2);
        // alma instances gone, debian survives.
        assert!(!instance_dir("alma", 0).unwrap().exists());
        assert!(!instance_dir("alma", 1).unwrap().exists());
        assert!(instance_dir("debian", 0).unwrap().exists());
    }

    #[test]
    fn reset_instances_with_l2cpu_filter_only_removes_one() {
        let dir = tmp_dir();
        let _g = DataHomeGuard::set(dir.path());
        let template = dir.path().join("template.img");
        fs::write(&template, b"x").unwrap();
        clone_template_if_missing(&template, "alma", 0).unwrap();
        clone_template_if_missing(&template, "alma", 1).unwrap();

        let removed = reset_instances("alma", Some(0)).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(!instance_dir("alma", 0).unwrap().exists());
        assert!(instance_dir("alma", 1).unwrap().exists());
    }

    #[test]
    fn reset_instances_on_missing_dir_is_noop() {
        let dir = tmp_dir();
        let _g = DataHomeGuard::set(dir.path());
        // No instances dir exists yet.
        let removed = reset_instances("alma", None).unwrap();
        assert!(removed.is_empty());
    }

    #[test]
    fn empty_profiles_file_round_trips() {
        let pf = ProfilesFile::default();
        let dir = tmp_dir();
        let path = dir.path().join("profiles.yaml");
        save_profiles_to(&pf, &path).unwrap();
        let loaded = load_profiles_from(&path).unwrap();
        assert_eq!(loaded, pf);
    }

    #[test]
    fn populated_profiles_file_round_trips() {
        let mut pf = ProfilesFile::default();
        pf.profiles.insert(
            "alma-dev".to_string(),
            Profile {
                image: "almalinux-10-kitten".to_string(),
                memory: Some("2GB".to_string()),
                bootloader: None,
                initramfs: None,
                root_device: None,
                network: NetworkConfig {
                    enabled: true,
                    hostname: Some("alma-dev".to_string()),
                    forwards: vec!["5201:5201".to_string()],
                },
                console: ConsoleConfig {
                    virtio: true,
                    rng: true,
                },
                cloud_init: None,
            },
        );
        let dir = tmp_dir();
        let path = dir.path().join("profiles.yaml");
        save_profiles_to(&pf, &path).unwrap();
        let loaded = load_profiles_from(&path).unwrap();
        assert_eq!(loaded, pf);
    }

    #[test]
    fn missing_profiles_file_loads_as_default() {
        let dir = tmp_dir();
        let path = dir.path().join("does-not-exist.yaml");
        let pf = load_profiles_from(&path).unwrap();
        assert_eq!(pf, ProfilesFile::default());
    }

    #[test]
    fn empty_file_loads_as_default() {
        let dir = tmp_dir();
        let path = dir.path().join("empty.yaml");
        fs::write(&path, b"").unwrap();
        let pf = load_profiles_from(&path).unwrap();
        assert_eq!(pf, ProfilesFile::default());
    }

    #[test]
    fn malformed_yaml_returns_bad_request() {
        let dir = tmp_dir();
        let path = dir.path().join("broken.yaml");
        fs::write(&path, b"profiles: { not properly closed").unwrap();
        let err = load_profiles_from(&path).unwrap_err();
        assert!(matches!(err, Error::BadRequest(_)));
    }

    // ---- validate_profile_name ----

    #[test]
    fn profile_name_accepts_valid() {
        assert!(validate_profile_name("alma-dev").is_ok());
        assert!(validate_profile_name("debian_min").is_ok());
        assert!(validate_profile_name("a").is_ok());
        assert!(validate_profile_name("aB1-2_3").is_ok());
    }

    #[test]
    fn profile_name_rejects_invalid() {
        assert!(validate_profile_name("").is_err());
        assert!(validate_profile_name("1abc").is_err()); // can't start with digit
        assert!(validate_profile_name("-foo").is_err()); // can't start with dash
        assert!(validate_profile_name("foo bar").is_err()); // space
        assert!(validate_profile_name("foo.bar").is_err()); // dot
        assert!(validate_profile_name(&"a".repeat(33)).is_err()); // too long
    }

    // ---- validate_profile (combined rules) ----

    fn good_profile() -> Profile {
        Profile {
            image: "debian-13".to_string(),
            memory: Some("2GB".to_string()),
            bootloader: None,
            initramfs: None,
            root_device: None,
            network: NetworkConfig {
                enabled: true,
                hostname: Some("debian-bench".to_string()),
                forwards: vec!["5201:5201".to_string(), "8080:80".to_string()],
            },
            console: ConsoleConfig::default(),
            cloud_init: None,
        }
    }

    #[test]
    fn validate_accepts_well_formed_profile() {
        validate_profile("alma-dev", &good_profile()).unwrap();
    }

    #[test]
    fn validate_rejects_unknown_image() {
        let mut p = good_profile();
        p.image = "no-such-distro".to_string();
        assert!(validate_profile("alma-dev", &p).is_err());
    }

    #[test]
    fn validate_rejects_invalid_memory() {
        let mut p = good_profile();
        p.memory = Some("notabyte".to_string());
        assert!(validate_profile("alma-dev", &p).is_err());
    }

    #[test]
    fn validate_rejects_unknown_bootloader() {
        let mut p = good_profile();
        p.bootloader = Some("syslinux".to_string());
        assert!(validate_profile("alma-dev", &p).is_err());
    }

    #[test]
    fn validate_accepts_known_bootloaders() {
        let mut p = good_profile();
        p.bootloader = Some("kernel".to_string());
        validate_profile("alma-dev", &p).unwrap();
        p.bootloader = Some("uboot".to_string());
        validate_profile("alma-dev", &p).unwrap();
    }

    #[test]
    fn validate_rejects_invalid_hostname() {
        let mut p = good_profile();
        p.network.hostname = Some("BadHost_Name".to_string());
        assert!(validate_profile("alma-dev", &p).is_err());
    }

    #[test]
    fn validate_rejects_invalid_cloud_init_hostname() {
        let mut p = good_profile();
        p.cloud_init = Some(CloudInitConfig {
            hostname: Some("BadHost_Name".to_string()),
            ..CloudInitConfig::default()
        });
        let err = validate_profile("alma-dev", &p).unwrap_err().to_string();
        assert!(
            err.contains("cloud_init.hostname"),
            "error didn't mention cloud_init.hostname: {}",
            err
        );
    }

    #[test]
    fn validate_accepts_minimal_cloud_init() {
        let mut p = good_profile();
        p.cloud_init = Some(CloudInitConfig {
            user: Some("operator".to_string()),
            password: Some("hunter2".to_string()),
            hostname: Some("dev-l0".to_string()),
            ..CloudInitConfig::default()
        });
        validate_profile("alma-dev", &p).unwrap();
    }

    #[test]
    fn cloud_init_config_to_seed_spec_is_a_field_copy() {
        let ci = CloudInitConfig {
            user: Some("alice".to_string()),
            password: Some("p".to_string()),
            ssh_keys: vec!["ssh-ed25519 AAAA test".to_string()],
            hostname: Some("dev-l1".to_string()),
            instance_id: Some("v42".to_string()),
            nameservers: vec!["1.1.1.1".to_string()],
            extra_user_data: Some("packages: [tmux]\n".to_string()),
        };
        let spec = ci.to_seed_spec();
        assert_eq!(spec.user.as_deref(), Some("alice"));
        assert_eq!(spec.password.as_deref(), Some("p"));
        assert_eq!(spec.ssh_keys, ci.ssh_keys);
        assert_eq!(spec.hostname.as_deref(), Some("dev-l1"));
        assert_eq!(spec.instance_id.as_deref(), Some("v42"));
        assert_eq!(spec.nameservers, ci.nameservers);
        assert_eq!(spec.extra_user_data.as_deref(), Some("packages: [tmux]\n"));
    }

    #[test]
    fn profile_yaml_round_trip_with_cloud_init() {
        // Smoke: serialize → deserialize must preserve the cloud_init
        // sub-block. Field skipping (`Option::is_none` etc.) is enabled,
        // so absent sub-blocks don't pollute the output.
        let p = Profile {
            image: "debian-13".to_string(),
            cloud_init: Some(CloudInitConfig {
                user: Some("ops".to_string()),
                hostname: Some("box".to_string()),
                ..CloudInitConfig::default()
            }),
            ..Profile::default()
        };
        let yaml = serde_yaml_ng::to_string(&p).unwrap();
        assert!(yaml.contains("cloud_init:"));
        assert!(yaml.contains("ops"));
        let back: Profile = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn validate_rejects_invalid_forward() {
        let mut p = good_profile();
        p.network.forwards = vec!["not-a-pair".to_string()];
        assert!(validate_profile("alma-dev", &p).is_err());
        p.network.forwards = vec!["0:80".to_string()];
        assert!(validate_profile("alma-dev", &p).is_err());
    }

    #[test]
    fn validate_rejects_invalid_profile_name() {
        let p = good_profile();
        assert!(validate_profile("1bad", &p).is_err());
        assert!(validate_profile("", &p).is_err());
    }

    // ---- parse_memory_str ----

    #[test]
    fn parse_memory_str_accepts_iec_and_si() {
        assert_eq!(parse_memory_str("2GB").unwrap(), 2_000_000_000);
        assert_eq!(parse_memory_str("2GiB").unwrap(), 2_147_483_648);
        assert_eq!(parse_memory_str("1.5GiB").unwrap(), 1_610_612_736);
    }

    #[test]
    fn parse_memory_str_overflow_returns_error() {
        // Each of these saturated to u64::MAX before #152.
        assert!(parse_memory_str("1e30GB").is_err());
        assert!(parse_memory_str("99999999999GB").is_err());
        assert!(parse_memory_str("inf").is_err());
        assert!(parse_memory_str("NaN").is_err());
    }

    // ---- pick_editor ----

    #[test]
    fn pick_editor_prefers_visual_then_editor_then_vi() {
        let _lock = env_lock();
        // Save + restore env so test order doesn't leak state.
        let prev_visual = std::env::var_os("VISUAL");
        let prev_editor = std::env::var_os("EDITOR");
        unsafe {
            std::env::set_var("VISUAL", "myvisual");
            std::env::set_var("EDITOR", "myeditor");
        }
        assert_eq!(pick_editor(), "myvisual");
        unsafe {
            std::env::remove_var("VISUAL");
        }
        assert_eq!(pick_editor(), "myeditor");
        unsafe {
            std::env::remove_var("EDITOR");
        }
        assert_eq!(pick_editor(), "vi");
        // Restore.
        unsafe {
            if let Some(v) = prev_visual {
                std::env::set_var("VISUAL", v);
            }
            if let Some(v) = prev_editor {
                std::env::set_var("EDITOR", v);
            }
        }
    }

    // ---- edit_with_retry ----

    /// Stub editor that runs a series of canned mutations against the
    /// catalog file, one per call, so a test can simulate
    /// broken-then-fixed YAML.
    struct StubEditor<'a> {
        scripts: Vec<&'a str>,
        idx: usize,
    }

    impl<'a> EditorRunner for StubEditor<'a> {
        fn edit(&mut self, path: &Path) -> Result<()> {
            let body = self
                .scripts
                .get(self.idx)
                .copied()
                .unwrap_or("profiles: {}\n");
            self.idx += 1;
            fs::write(path, body)
                .map_err(|e| Error::internal(format!("stub editor write: {}", e)))?;
            Ok(())
        }
    }

    /// Test confirm callback that records every error it sees and
    /// always returns Ok (auto-retry). Production replaces this with
    /// `stdin_retry_prompt`.
    fn auto_retry_confirm() -> impl FnMut(usize, usize, &Error) -> Result<()> {
        |_attempt, _max, _err| Ok(())
    }

    #[test]
    fn edit_with_retry_succeeds_on_first_clean_save() {
        let dir = tmp_dir();
        let path = dir.path().join("profiles.yaml");
        let mut editor = StubEditor {
            scripts: vec!["profiles:\n  alma:\n    image: debian-13\n"],
            idx: 0,
        };
        let pf = edit_with_retry(&mut editor, &path, 3, auto_retry_confirm()).unwrap();
        assert!(pf.profiles.contains_key("alma"));
        assert_eq!(pf.profiles["alma"].image, "debian-13");
    }

    #[test]
    fn edit_with_retry_re_prompts_on_validation_failure_then_accepts_fix() {
        let dir = tmp_dir();
        let path = dir.path().join("profiles.yaml");
        let mut editor = StubEditor {
            scripts: vec![
                // Attempt 1: unknown image.
                "profiles:\n  alma:\n    image: no-such-image\n",
                // Attempt 2: clean.
                "profiles:\n  alma:\n    image: debian-13\n",
            ],
            idx: 0,
        };
        let pf = edit_with_retry(&mut editor, &path, 3, auto_retry_confirm()).unwrap();
        assert_eq!(pf.profiles["alma"].image, "debian-13");
        assert_eq!(editor.idx, 2);
    }

    #[test]
    fn edit_with_retry_re_prompts_on_parse_failure_then_accepts_fix() {
        let dir = tmp_dir();
        let path = dir.path().join("profiles.yaml");
        let mut editor = StubEditor {
            scripts: vec![
                // Attempt 1: garbage YAML.
                "profiles: { broken",
                // Attempt 2: clean.
                "profiles:\n  debian:\n    image: debian-13\n",
            ],
            idx: 0,
        };
        let pf = edit_with_retry(&mut editor, &path, 3, auto_retry_confirm()).unwrap();
        assert_eq!(pf.profiles["debian"].image, "debian-13");
    }

    #[test]
    fn edit_with_retry_gives_up_after_max_attempts() {
        let dir = tmp_dir();
        let path = dir.path().join("profiles.yaml");
        let mut editor = StubEditor {
            scripts: vec!["broken", "still broken", "yet broken"],
            idx: 0,
        };
        let err = edit_with_retry(&mut editor, &path, 3, auto_retry_confirm()).unwrap_err();
        assert!(matches!(err, Error::BadRequest(_)));
        assert_eq!(editor.idx, 3);
    }

    #[test]
    fn edit_with_retry_calls_confirm_with_each_attempt_error_until_clean_save() {
        // The confirm callback is the operator-facing prompt — it
        // sees one (attempt, max, error) tuple per failed save.
        // Capture them so we can assert the loop hands out the right
        // attempt counter and the right error each time.
        let dir = tmp_dir();
        let path = dir.path().join("profiles.yaml");
        let mut editor = StubEditor {
            scripts: vec![
                "profiles: { broken",
                "profiles:\n  alma:\n    image: no-such-image\n",
                "profiles:\n  alma:\n    image: debian-13\n",
            ],
            idx: 0,
        };
        let mut seen: Vec<(usize, usize, String)> = Vec::new();
        let confirm = |attempt: usize, max: usize, err: &Error| -> Result<()> {
            seen.push((attempt, max, format!("{}", err)));
            Ok(())
        };
        let pf = edit_with_retry(&mut editor, &path, 5, confirm).unwrap();
        assert_eq!(pf.profiles["alma"].image, "debian-13");
        assert_eq!(seen.len(), 2, "confirm runs once per failed attempt");
        assert_eq!((seen[0].0, seen[0].1), (1, 5));
        assert!(seen[0].2.contains("parse"));
        assert_eq!((seen[1].0, seen[1].1), (2, 5));
        assert!(
            seen[1].2.contains("no-such-image"),
            "expected validation message, got: {}",
            seen[1].2
        );
    }

    #[test]
    fn edit_with_retry_aborts_when_confirm_returns_err() {
        // An operator who hits Ctrl-C at the prompt is modelled here
        // as a confirm callback that returns Err. The loop must
        // surface that error verbatim instead of re-running the
        // editor.
        let dir = tmp_dir();
        let path = dir.path().join("profiles.yaml");
        let mut editor = StubEditor {
            scripts: vec!["broken", "broken-again-but-we-never-see-this"],
            idx: 0,
        };
        let confirm = |_, _, _: &Error| -> Result<()> { Err(Error::internal("operator aborted")) };
        let err = edit_with_retry(&mut editor, &path, 5, confirm).unwrap_err();
        assert!(matches!(err, Error::Internal(ref m) if m.contains("operator aborted")));
        assert_eq!(editor.idx, 1, "editor must run only once before the abort");
    }

    // ---- FIRST_RUN_TEMPLATE (#111) ----

    #[test]
    fn first_run_template_parses_to_empty_profiles() {
        // Saving the seeded file as-is must round-trip cleanly: the
        // active line is `profiles: {}` and the rest is comments.
        let dir = tmp_dir();
        let path = dir.path().join("profiles.yaml");
        fs::write(&path, FIRST_RUN_TEMPLATE).unwrap();
        let pf = load_profiles_from(&path).unwrap();
        assert!(
            pf.profiles.is_empty(),
            "seeded template should parse to no profiles, got: {:?}",
            pf.profiles.keys().collect::<Vec<_>>(),
        );
        validate_all(&pf).unwrap();
    }

    /// Strip a single leading "# " (or "#" for `#`-only lines) from
    /// each line of the template's example block. Used to verify the
    /// commented examples don't drift away from the live `Profile`
    /// schema.
    fn uncomment_examples(template: &str) -> String {
        let marker = "# Templates";
        let body = template
            .split_once(marker)
            .map(|(_, rest)| rest)
            .expect("template missing `# Templates` marker");
        // Skip the rest of the marker line (everything up to \n).
        let body = body.split_once('\n').map(|(_, r)| r).unwrap_or(body);
        // And the trailing `# ----` divider line that follows the marker.
        let body = body.split_once('\n').map(|(_, r)| r).unwrap_or(body);
        body.lines()
            .map(|l| {
                l.strip_prefix("# ")
                    .or_else(|| l.strip_prefix("#"))
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn first_run_template_examples_parse_and_validate_when_uncommented() {
        // The template is operator-facing documentation as much as it
        // is YAML. If a future Profile field rename leaves the
        // commented examples pointing at a key that no longer exists,
        // the operator follows broken instructions. Detect that drift
        // by un-commenting the examples and feeding them through the
        // real load + validate path.
        let raw = uncomment_examples(FIRST_RUN_TEMPLATE);
        let dir = tmp_dir();
        let path = dir.path().join("profiles.yaml");
        fs::write(&path, &raw).unwrap();
        let pf = load_profiles_from(&path).unwrap_or_else(|e| {
            panic!("uncommented template failed to parse:\n{}\nerr: {}", raw, e)
        });
        // We expect every templated stanza to come through.
        for expected in ["tt-smoke", "debian-net", "fedora-uboot"] {
            assert!(
                pf.profiles.contains_key(expected),
                "uncommented template missing expected profile {:?}; got {:?}",
                expected,
                pf.profiles.keys().collect::<Vec<_>>(),
            );
        }
        validate_all(&pf)
            .unwrap_or_else(|e| panic!("uncommented template fails validation: {}", e));
    }
}
