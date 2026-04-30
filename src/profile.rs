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
    /// Attach a virtio-console alongside the SBI console.
    #[serde(default)]
    pub virtio: bool,
    /// Attach virtio-rng. Default true — the AlmaLinux EFI shim
    /// requires `EFI_RNG_PROTOCOL` (see #62).
    #[serde(default = "default_true")]
    pub rng: bool,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        ConsoleConfig {
            virtio: false,
            rng: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Resolve `~/.config/bhx/profiles.yaml`. Honors `$XDG_CONFIG_HOME`,
/// falls back to `$HOME/.config`. Pure path construction; doesn't
/// touch the filesystem.
pub fn profiles_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                Error::internal("neither XDG_CONFIG_HOME nor HOME set; can't locate profiles.yaml")
            })?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("bhx").join("profiles.yaml"))
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
    Ok((num * mult as f64) as u64)
}

// ============================================================================
// Per-instance disks (#93)
// ============================================================================

/// Resolve `~/.local/share/bhx/instances`. Honors `$XDG_DATA_HOME`,
/// falls back to `$HOME/.local/share`. Pure path construction.
pub fn instances_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                Error::internal(
                    "neither XDG_DATA_HOME nor HOME set; can't locate instance disk dir",
                )
            })?;
            PathBuf::from(home).join(".local").join("share")
        }
    };
    Ok(base.join("bhx").join("instances"))
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
pub fn edit_with_retry<E: EditorRunner>(
    editor: &mut E,
    path: &Path,
    max_attempts: usize,
) -> Result<ProfilesFile> {
    for attempt in 1..=max_attempts {
        editor.edit(path)?;
        match load_profiles_from(path) {
            Ok(profiles) => match validate_all(&profiles) {
                Ok(()) => return Ok(profiles),
                Err(e) => {
                    eprintln!("validation failed: {}", e);
                    if attempt == max_attempts {
                        return Err(e);
                    }
                    eprintln!(
                        "re-opening editor (attempt {} of {})",
                        attempt + 1,
                        max_attempts
                    );
                }
            },
            Err(e) => {
                eprintln!("parse failed: {}", e);
                if attempt == max_attempts {
                    return Err(e);
                }
                eprintln!(
                    "re-opening editor (attempt {} of {})",
                    attempt + 1,
                    max_attempts
                );
            }
        }
    }
    unreachable!("loop body returns or errors at attempt == max_attempts")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // ---- ProfilesFile round-trip ----

    /// Process-wide mutex serialising every test that mutates env
    /// vars. `cargo test` runs tests in parallel by default and
    /// `std::env::set_var` is globally visible — without this lock,
    /// two tests setting `XDG_DATA_HOME` race and clobber each
    /// other's tmpdir.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Override `$XDG_DATA_HOME` for the duration of a test so
    /// `instances_dir()` lands inside our tempdir instead of the
    /// operator's real `~/.local/share`. Holds `ENV_LOCK` for the
    /// guard's lifetime.
    struct DataHomeGuard {
        prev: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl DataHomeGuard {
        fn set(value: &Path) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    // ---- pick_editor ----

    #[test]
    fn pick_editor_prefers_visual_then_editor_then_vi() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    #[test]
    fn edit_with_retry_succeeds_on_first_clean_save() {
        let dir = tmp_dir();
        let path = dir.path().join("profiles.yaml");
        let mut editor = StubEditor {
            scripts: vec!["profiles:\n  alma:\n    image: debian-13\n"],
            idx: 0,
        };
        let pf = edit_with_retry(&mut editor, &path, 3).unwrap();
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
        let pf = edit_with_retry(&mut editor, &path, 3).unwrap();
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
        let pf = edit_with_retry(&mut editor, &path, 3).unwrap();
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
        let err = edit_with_retry(&mut editor, &path, 3).unwrap_err();
        assert!(matches!(err, Error::BadRequest(_)));
        assert_eq!(editor.idx, 3);
    }
}
