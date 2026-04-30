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
