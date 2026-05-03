// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! XDG Base Directory resolution. Single source of truth so the same
//! `XDG_DATA_HOME` / `XDG_CONFIG_HOME` / `HOME` fallback logic stays
//! consistent across every feature module that needs a config or
//! data path. See <https://github.com/olofj/bhx/issues/154>.
//!
//! Three resolvers map to the three XDG categories the crate uses:
//!
//! - [`data_home`] — `$XDG_DATA_HOME` or `$HOME/.local/share`
//! - [`config_home`] — `$XDG_CONFIG_HOME` or `$HOME/.config`
//! - [`runtime_dir_fallback`] — bhx-specific `/tmp/bhx-<uid>`
//!   replacement when `$XDG_RUNTIME_DIR` is unset
//!
//! Plus a convenience: [`data_subdir`] joins `data_home` with
//! `bhx/<subdir>` and best-effort creates the directory.
//!
//! Pre-#154 each feature module had its own resolver with subtly
//! different fallback shape — `image.rs` returned a CWD-relative
//! `PathBuf::from(".local/share")` if both env vars were unset (a
//! footgun under daemonization where the daemon chdirs to `/`),
//! while `profile.rs` errored out cleanly. Convergence here picks
//! the error path: callers should not invent relative paths.

use crate::error::{Error, Result};
use std::path::PathBuf;

/// `$XDG_DATA_HOME` or `$HOME/.local/share`. Errors if neither is
/// set — the caller should not be inventing CWD-relative paths.
pub fn data_home() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("XDG_DATA_HOME") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| Error::internal("neither XDG_DATA_HOME nor HOME set"))?;
    Ok(PathBuf::from(home).join(".local/share"))
}

/// `$XDG_CONFIG_HOME` or `$HOME/.config`. Errors if neither is set.
pub fn config_home() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("XDG_CONFIG_HOME") {
        if !v.is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| Error::internal("neither XDG_CONFIG_HOME nor HOME set"))?;
    Ok(PathBuf::from(home).join(".config"))
}

/// `/tmp/bhx-<uid>`. The XDG spec doesn't define a fallback for
/// `XDG_RUNTIME_DIR`; this is bhx's pick, lifted from the
/// pre-consolidation behavior of `daemon::lifetime::runtime_dir`.
/// Per-user (no `bhx-shared` collisions across logins on a multi-user
/// host); callers that mkdir it should set mode 0700 to avoid a
/// world-writable hole.
pub fn runtime_dir_fallback() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/bhx-{}", uid))
}

/// `<data_home>/bhx/<subdir>`. Best-effort `mkdir -p`; ignores
/// errors (callers that need to fail on unwriteable parents check
/// the path themselves at write time).
pub fn data_subdir(subdir: &str) -> Result<PathBuf> {
    let dir = data_home()?.join("bhx").join(subdir);
    let _ = std::fs::create_dir_all(&dir);
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::env_lock;

    fn save_and_set(key: &str, value: Option<&str>) -> Option<std::ffi::OsString> {
        let prev = std::env::var_os(key);
        unsafe {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        prev
    }

    fn restore(key: &str, prev: Option<std::ffi::OsString>) {
        unsafe {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn data_home_prefers_xdg_data_home() {
        let _lock = env_lock();
        let p_xdg = save_and_set("XDG_DATA_HOME", Some("/tmp/xdg-test"));
        let p_home = save_and_set("HOME", Some("/tmp/home-test"));
        assert_eq!(data_home().unwrap(), PathBuf::from("/tmp/xdg-test"));
        restore("HOME", p_home);
        restore("XDG_DATA_HOME", p_xdg);
    }

    #[test]
    fn data_home_falls_back_to_home_dot_local_share() {
        let _lock = env_lock();
        let p_xdg = save_and_set("XDG_DATA_HOME", None);
        let p_home = save_and_set("HOME", Some("/tmp/home-test"));
        assert_eq!(
            data_home().unwrap(),
            PathBuf::from("/tmp/home-test/.local/share")
        );
        restore("HOME", p_home);
        restore("XDG_DATA_HOME", p_xdg);
    }

    #[test]
    fn data_home_errors_when_nothing_set() {
        let _lock = env_lock();
        let p_xdg = save_and_set("XDG_DATA_HOME", None);
        let p_home = save_and_set("HOME", None);
        assert!(data_home().is_err());
        restore("HOME", p_home);
        restore("XDG_DATA_HOME", p_xdg);
    }

    #[test]
    fn config_home_prefers_xdg_config_home() {
        let _lock = env_lock();
        let p_xdg = save_and_set("XDG_CONFIG_HOME", Some("/tmp/xdg-cfg"));
        let p_home = save_and_set("HOME", Some("/tmp/home-test"));
        assert_eq!(config_home().unwrap(), PathBuf::from("/tmp/xdg-cfg"));
        restore("HOME", p_home);
        restore("XDG_CONFIG_HOME", p_xdg);
    }

    #[test]
    fn data_subdir_appends_bhx_and_subdir() {
        let _lock = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let p_xdg = save_and_set("XDG_DATA_HOME", Some(&tmp.path().to_string_lossy()));
        let result = data_subdir("images").unwrap();
        assert_eq!(result, tmp.path().join("bhx").join("images"));
        assert!(result.is_dir());
        restore("XDG_DATA_HOME", p_xdg);
    }

    #[test]
    fn runtime_dir_fallback_includes_uid() {
        let path = runtime_dir_fallback();
        let s = path.to_string_lossy();
        let uid = unsafe { libc::getuid() };
        assert!(s.contains(&uid.to_string()), "expected uid in {:?}", s);
    }
}
