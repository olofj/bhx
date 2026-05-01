// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT
//
//! Cloud-init NoCloud seed-disk generator (#82).
//!
//! Stock cloud images for riscv64 (Debian 13 generic, Fedora 42
//! Cloud Base, AlmaLinux Kitten 10, Ubuntu 24.04 preinstalled-server)
//! ship with no usable default login. They expect to be provisioned
//! by [cloud-init][1] on first boot via the NoCloud datasource: a
//! virtio-blk device whose filesystem label is `cidata`, containing
//! `user-data` (cloud-config YAML — sets users, SSH keys, packages,
//! commands) and `meta-data` (instance-id + local-hostname).
//!
//! This module renders a [`SeedSpec`] to YAML and packs the resulting
//! files into an ISO9660 image labeled `cidata` using the host's
//! `genisoimage` (or `mkisofs` / `xorrisofs`) tool. The seed image is
//! attached to a guest as a 2nd virtio-blk via `bhx boot --cloud-init`
//! (see `daemon::server::dispatch_boot`).
//!
//! [1]: https://cloudinit.readthedocs.io/en/latest/reference/datasources/nocloud.html

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Result;

/// Default user created when [`SeedSpec::user`] is `None`. Picked so
/// that operator scripts can rely on a stable account name across
/// distros without having to know which image is in play.
pub const DEFAULT_USER: &str = "bhx";

/// Default password for the auto-created user when no SSH keys are
/// supplied. Purely a development convenience — operators who care
/// about security should supply an `ssh_keys` list and leave
/// `password` as `None` to keep the account key-only.
pub const DEFAULT_PASSWORD: &str = "bhx";

/// Default DNS resolver baked into the seed when [`SeedSpec::nameservers`]
/// is empty. Slirp's built-in DNS proxy at `10.0.2.3` forwards to
/// the host's `/etc/resolv.conf`; on hosts where resolv.conf points
/// at a host-only IP (Tailscale MagicDNS at `100.100.100.100`,
/// dnsmasq, etc.) slirp's NAT can't reach the target and DNS dies.
/// Pointing the guest's resolv.conf at `8.8.8.8` directly sidesteps
/// the proxy: queries go through slirp's normal UDP NAT to the
/// public internet. Operators who actually want host-DNS forwarding
/// (split-horizon zones, internal corp resolvers) supply their own
/// seed via `--cloud-init <path>`.
pub const DEFAULT_NAMESERVER: &str = "8.8.8.8";

/// User-supplied parameters for a NoCloud seed image. Fields default
/// to "sensible for a dev box" so a bare
/// `SeedSpec::default().write_iso(out)` produces a working image.
#[derive(Debug, Clone, Default)]
pub struct SeedSpec {
    /// Login name to create. Defaults to [`DEFAULT_USER`] when `None`.
    pub user: Option<String>,
    /// Plain-text password to set on the user. cloud-init hashes it
    /// before writing to /etc/shadow. `None` + empty `ssh_keys`
    /// falls back to [`DEFAULT_PASSWORD`] so first-boot login works
    /// without operator setup; `None` + non-empty `ssh_keys` keeps
    /// the user key-only.
    pub password: Option<String>,
    /// SSH public keys to install into the user's authorized_keys.
    /// Each entry is a single line in OpenSSH format (e.g.
    /// `ssh-ed25519 AAAA… user@host`).
    pub ssh_keys: Vec<String>,
    /// Guest hostname. `None` defaults to `bhx-guest`.
    pub hostname: Option<String>,
    /// cloud-init instance-id. Used for first-boot detection — if
    /// the same id is seen on a subsequent boot, cloud-init skips
    /// re-running its config modules. `None` generates a random id
    /// at write time so re-imaging the rootfs without rebuilding
    /// the seed re-runs everything.
    pub instance_id: Option<String>,
    /// DNS resolvers the guest should use. Empty = bake the
    /// [`DEFAULT_NAMESERVER`] (`8.8.8.8`). Emitted as a `bootcmd`
    /// that replaces `/etc/resolv.conf` (which on Debian/Ubuntu/
    /// Fedora is a symlink to `/run/systemd/resolve/stub-resolv.conf`
    /// pointing at systemd-resolved's `127.0.0.53` stub) with a
    /// plain file containing the configured nameservers. This
    /// detaches glibc's resolver from systemd-resolved's stub —
    /// stub forwarding fails when slirp's DNS proxy at `10.0.2.3`
    /// can't reach the host's resolv.conf target (Tailscale,
    /// dnsmasq), but a direct nameserver entry takes the normal
    /// slirp UDP NAT path which works.
    ///
    /// `bootcmd` runs on every boot before networkd brings up
    /// interfaces, so it survives DHCP renewals that would
    /// otherwise let the symlink reappear. systemd-resolved keeps
    /// running for any service that talks to it explicitly; we just
    /// stop glibc's default lookup path from going through it.
    pub nameservers: Vec<String>,
    /// Extra cloud-config YAML to merge with the generated user-data.
    /// Concatenated verbatim after the auto-generated stanzas; the
    /// caller is responsible for keeping it valid YAML. Useful for
    /// adding `packages:`, `runcmd:`, `write_files:` etc. without
    /// extending [`SeedSpec`] for every cloud-config feature.
    pub extra_user_data: Option<String>,
}

impl SeedSpec {
    /// Render [`SeedSpec`] to a NoCloud seed ISO at `output`. The ISO
    /// has filesystem label `cidata`, contains `user-data` and
    /// `meta-data` at the root, and is ~10 KiB on disk. Overwrites
    /// `output` if it exists.
    pub fn write_iso(&self, output: &Path) -> Result<()> {
        let user_data = self.render_user_data()?;
        let meta_data = self.render_meta_data();
        write_iso(&user_data, &meta_data, output)
    }

    fn render_user_data(&self) -> Result<String> {
        let user = self.user.as_deref().unwrap_or(DEFAULT_USER);
        // If no SSH keys are set, fall back to a default password so
        // first-boot login works without preconfiguration. Operators
        // who set ssh_keys explicitly want key-only auth.
        let password = if !self.ssh_keys.is_empty() {
            self.password.as_deref()
        } else {
            Some(self.password.as_deref().unwrap_or(DEFAULT_PASSWORD))
        };

        // Hash the password with sha512crypt at seed-build time, so
        // the rendered user-data sets `users[].passwd: $6$...$...`
        // directly at user-creation time. We deliberately avoid both
        // `users[].plain_text_passwd` (deprecated in 22.2; cloud-init
        // 24.x silently routes it through chpasswd.list and warns)
        // and the top-level `chpasswd.users[]` block (cloud-init 24.x
        // appears to normalize it back to chpasswd.list internally,
        // re-firing the same deprecation). `users[].passwd` is the
        // user-creation-time field, has worked since 0.7, and bypasses
        // the chpasswd module entirely.
        let password_hash: Option<String> = match password {
            Some(p) => Some(hash_password_sha512(p)?),
            None => None,
        };

        let mut s = String::new();
        s.push_str("#cloud-config\n");
        // ssh_pwauth controls the global PasswordAuthentication knob
        // in sshd_config. Default-off in many cloud images; turn it
        // on whenever a password is set so the first-boot login is
        // reachable from the slirp forwarder.
        if password_hash.is_some() {
            s.push_str("ssh_pwauth: true\n");
        }
        s.push_str("users:\n");
        s.push_str(&format!("  - name: {}\n", user));
        s.push_str("    sudo: ALL=(ALL) NOPASSWD:ALL\n");
        s.push_str("    shell: /bin/bash\n");
        if let Some(h) = &password_hash {
            s.push_str("    lock_passwd: false\n");
            // The hash format is `$6$<salt>$<hash>` — `$` characters
            // need single-quoting in YAML (double-quoted would let
            // the shell-style `${...}` expansion through some YAML
            // dialects). Single-quote escapes embedded `'` by
            // doubling, but our hash never contains `'`.
            s.push_str(&format!("    passwd: '{}'\n", h));
        } else {
            s.push_str("    lock_passwd: true\n");
        }
        if !self.ssh_keys.is_empty() {
            s.push_str("    ssh_authorized_keys:\n");
            for key in &self.ssh_keys {
                s.push_str(&format!("      - \"{}\"\n", key.trim()));
            }
        }

        // Detach /etc/resolv.conf from systemd-resolved's stub so
        // glibc lookups bypass the broken slirp DNS proxy at
        // 10.0.2.3 (which fails on Tailscale hosts) and go through
        // slirp's normal UDP NAT to the configured nameservers.
        // `bootcmd` runs every boot before networkd brings up
        // interfaces, so a DHCP renewal can't re-establish the
        // systemd-resolved symlink on subsequent boots.
        let nameservers: Vec<&str> = if self.nameservers.is_empty() {
            vec![DEFAULT_NAMESERVER]
        } else {
            self.nameservers.iter().map(String::as_str).collect()
        };
        s.push_str("bootcmd:\n");
        s.push_str("  - [ sh, -c, 'rm -f /etc/resolv.conf && (\n");
        for ns in &nameservers {
            s.push_str(&format!("      echo nameserver {}\n", ns));
        }
        s.push_str("    ) > /etc/resolv.conf' ]\n");

        if let Some(extra) = &self.extra_user_data {
            // Caller's responsibility to keep extras valid YAML.
            // Concatenated raw with a leading newline so the operator
            // can start with a top-level key.
            if !extra.starts_with('\n') {
                s.push('\n');
            }
            s.push_str(extra);
            if !extra.ends_with('\n') {
                s.push('\n');
            }
        }
        Ok(s)
    }

    fn render_meta_data(&self) -> String {
        let hostname = self.hostname.as_deref().unwrap_or("bhx-guest");
        let instance_id = self
            .instance_id
            .clone()
            .unwrap_or_else(generate_instance_id);
        format!(
            "instance-id: {}\nlocal-hostname: {}\n",
            instance_id, hostname
        )
    }
}

/// Hash a plaintext password to the `$6$<salt>$<hash>` shadow format
/// (SHA-512crypt) so it can be embedded directly in
/// `users[].passwd:`. Shells out to `openssl passwd -6` since openssl
/// is in every base distro install we target; falls back to
/// `mkpasswd -m sha512crypt` (Debian/Ubuntu's whois package). Both
/// produce shadow-compatible output. The salt is generated by the
/// underlying tool, not us — we don't want to be in the random-bytes
/// business at seed-render time.
///
/// We deliberately don't import a sha512crypt crate. Cloud-init seed
/// rendering happens at most a few times per host setup; shelling
/// out adds ~15 ms of CLI startup that isn't on any hot path. A
/// crate dependency would be a maintenance footprint for a one-shot.
fn hash_password_sha512(plaintext: &str) -> Result<String> {
    use std::io::Write;
    // Pass the password on stdin so it never appears in argv (visible
    // to other users via `ps`, kept in shell history if anyone
    // strace's). `-stdin` is supported by openssl 1.1+.
    if let Ok(mut child) = Command::new("openssl")
        .args(["passwd", "-6", "-stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        if let Some(mut sin) = child.stdin.take() {
            let _ = sin.write_all(plaintext.as_bytes());
            let _ = sin.write_all(b"\n");
        }
        if let Ok(out) = child.wait_with_output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if s.starts_with("$6$") {
                    return Ok(s);
                }
            }
        }
    }
    // Fallback: mkpasswd from the whois package. Different stdin
    // story — takes the password as a positional arg unless `-S` is
    // used for an explicit salt. We don't pass a salt; mkpasswd
    // generates one. Pass the password via env to avoid argv
    // exposure when feasible.
    let out = Command::new("mkpasswd")
        .args(["-m", "sha512crypt", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            if let Some(mut sin) = c.stdin.take() {
                let _ = sin.write_all(plaintext.as_bytes());
                let _ = sin.write_all(b"\n");
            }
            c.wait_with_output()
        });
    if let Ok(out) = out {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.starts_with("$6$") {
                return Ok(s);
            }
        }
    }
    Err(crate::Error::bad_request(
        "could not hash password: install one of `openssl` (>= 1.1) or \
         `mkpasswd` (Debian/Ubuntu: apt install whois)",
    ))
}

/// Best-effort random-ish instance id. Doesn't need to be UUID-grade
/// uniqueness — cloud-init only checks for byte-level equality
/// against the previously-applied id stored in
/// `/var/lib/cloud/data/instance-id`. Mixing PID + nanos gives
/// enough entropy to make collision-with-self vanishingly unlikely.
fn generate_instance_id() -> String {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("iid-bhx-{:x}-{:x}", pid, nanos)
}

/// Stage `user-data` + `meta-data` in a tempdir and pack them into a
/// `cidata`-labeled ISO at `output`. Prefers `xorrisofs` (modern,
/// active upstream) and falls back through `genisoimage` /
/// `mkisofs`, since one-or-other is in every distro's repos.
fn write_iso(user_data: &str, meta_data: &str, output: &Path) -> Result<()> {
    let tool = pick_iso_tool().ok_or_else(|| {
        crate::Error::bad_request(
            "no ISO tool found — install one of: xorrisofs, genisoimage, mkisofs",
        )
    })?;

    let staging = tempfile::tempdir().map_err(crate::Error::io_ctx("create iso staging tmpdir"))?;
    let stage = staging.path();
    std::fs::write(stage.join("user-data"), user_data)
        .map_err(crate::Error::io_ctx("write user-data"))?;
    std::fs::write(stage.join("meta-data"), meta_data)
        .map_err(crate::Error::io_ctx("write meta-data"))?;

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(crate::Error::io_ctx("create output parent dir"))?;
        }
    }

    // -V: filesystem label (must be exactly "cidata"; cloud-init's
    //     NoCloud datasource accepts both "cidata" and "CIDATA",
    //     but we pin lowercase for stability).
    // -J: Joliet extensions — Windows-side compat, also makes
    //     long filenames work everywhere.
    // -r: rationalized Rock Ridge — proper POSIX file metadata
    //     (the seed is read-only post-boot but the kernel's
    //     ext4-or-iso9660 driver is happier when permissions
    //     are sane).
    let status = Command::new(tool)
        .args(["-V", "cidata", "-J", "-r", "-o"])
        .arg(output)
        .arg(stage)
        .status()
        .map_err(crate::Error::io_ctx(format!("spawn {}", tool)))?;
    if !status.success() {
        return Err(crate::Error::bad_request(format!(
            "{} exited with status {}",
            tool, status
        )));
    }
    Ok(())
}

fn pick_iso_tool() -> Option<&'static str> {
    ["xorrisofs", "genisoimage", "mkisofs"]
        .into_iter()
        .find(|t| which(t).is_some())
}

fn which(name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-512crypt output is `$6$<salt>$<hash>` and ~106 bytes long
    /// total. Helper to assert "this looks like a sha512crypt hash"
    /// without pinning a specific salt or hash.
    fn assert_looks_like_sha512crypt(passwd_line: &str) {
        // Strip the YAML key prefix and surrounding single quotes.
        let h = passwd_line
            .trim_start()
            .strip_prefix("passwd: '")
            .and_then(|s| s.strip_suffix('\''))
            .unwrap_or_else(|| panic!("not a `passwd: '...'` line: {:?}", passwd_line));
        assert!(h.starts_with("$6$"), "not sha512crypt: {:?}", h);
        // $6$<salt up to 16>$<hash 86>: at least 90 bytes after the prefix.
        assert!(h.len() >= 90, "hash too short: {:?}", h);
    }

    fn passwd_line(ud: &str) -> &str {
        ud.lines()
            .find(|l| l.trim_start().starts_with("passwd: "))
            .expect("user-data should have a passwd line")
    }

    #[test]
    fn user_data_with_default_password_when_no_keys() {
        let spec = SeedSpec::default();
        let ud = spec.render_user_data().unwrap();
        assert!(ud.starts_with("#cloud-config\n"));
        assert!(ud.contains("name: bhx"));
        assert_looks_like_sha512crypt(passwd_line(&ud));
        assert!(ud.contains("ssh_pwauth: true"));
        // No ssh_authorized_keys block when none supplied.
        assert!(!ud.contains("ssh_authorized_keys"));
        // None of the rejected schemas — neither the deprecated
        // `plain_text_passwd` nor the chpasswd module variants must
        // appear, since both have been observed to round-trip through
        // chpasswd.list internally on cloud-init 24.x.
        assert!(!ud.contains("plain_text_passwd"));
        assert!(!ud.contains("chpasswd"));
    }

    #[test]
    fn user_data_locks_password_when_keys_supplied_without_password() {
        let spec = SeedSpec {
            ssh_keys: vec!["ssh-ed25519 AAAA test@host".into()],
            ..SeedSpec::default()
        };
        let ud = spec.render_user_data().unwrap();
        assert!(ud.contains("ssh_authorized_keys:"));
        assert!(ud.contains("ssh-ed25519 AAAA test@host"));
        assert!(ud.contains("lock_passwd: true"));
        // No password set → no passwd line, no chpasswd block.
        assert!(!ud.contains("passwd: '"));
        assert!(!ud.contains("chpasswd"));
        assert!(!ud.contains("plain_text_passwd"));
        // Without a password, ssh_pwauth stays at the cloud-init
        // default (off) — operators who supply keys want key-only.
        assert!(!ud.contains("ssh_pwauth: true"));
    }

    #[test]
    fn user_data_uses_explicit_password_with_keys() {
        let spec = SeedSpec {
            password: Some("hunter2".into()),
            ssh_keys: vec!["ssh-ed25519 AAAA".into()],
            ..SeedSpec::default()
        };
        let ud = spec.render_user_data().unwrap();
        assert_looks_like_sha512crypt(passwd_line(&ud));
        // Plaintext password must NOT appear in the rendered output.
        // The whole point of hashing at seed-build time is that the
        // plaintext doesn't land on the seed ISO.
        assert!(!ud.contains("hunter2"));
        assert!(ud.contains("ssh-ed25519 AAAA"));
    }

    #[test]
    fn user_data_uses_custom_user() {
        let spec = SeedSpec {
            user: Some("operator".into()),
            ..SeedSpec::default()
        };
        let ud = spec.render_user_data().unwrap();
        assert!(ud.contains("name: operator"));
        assert!(!ud.contains("name: bhx"));
        // Password line is on the user, so there's exactly one
        // occurrence per user. No chpasswd block means no second
        // "name: <user>" elsewhere.
        assert_eq!(ud.matches("name: operator").count(), 1);
    }

    #[test]
    fn extra_user_data_is_concatenated_verbatim() {
        let spec = SeedSpec {
            extra_user_data: Some("packages:\n  - tmux\n".into()),
            ..SeedSpec::default()
        };
        let ud = spec.render_user_data().unwrap();
        assert!(ud.contains("packages:"));
        assert!(ud.contains("- tmux"));
    }

    #[test]
    fn meta_data_uses_default_hostname_and_random_instance_id() {
        let spec = SeedSpec::default();
        let md = spec.render_meta_data();
        assert!(md.contains("local-hostname: bhx-guest"));
        assert!(md.contains("instance-id: iid-bhx-"));
    }

    #[test]
    fn meta_data_uses_explicit_instance_id() {
        let spec = SeedSpec {
            instance_id: Some("v42".into()),
            hostname: Some("worker-3".into()),
            ..SeedSpec::default()
        };
        let md = spec.render_meta_data();
        assert!(md.contains("instance-id: v42"));
        assert!(md.contains("local-hostname: worker-3"));
    }

    /// Strip the leading `#cloud-config` comment line so the rest can
    /// be fed to a YAML parser. cloud-init treats the first line as a
    /// MIME-style format hint, not part of the YAML document.
    fn strip_cloud_config_header(ud: &str) -> &str {
        ud.strip_prefix("#cloud-config\n").unwrap_or(ud)
    }

    /// Acceptance test for #118: operator-supplied password containing
    /// shell-hostile characters (`"`, `\`, `'`, newline) must produce
    /// user-data that round-trips through a YAML parser. The fix landed
    /// when we switched to sha512crypt-at-seed-build-time — the hash
    /// has only `$`, `/`, `.`, and alphanumerics, all YAML-safe inside
    /// single quotes — so no plaintext ever reaches the YAML emitter.
    #[test]
    fn user_data_with_hostile_password_is_valid_yaml() {
        for hostile in ["hunter\"two", "back\\slash", "quo'te", "with\nnewline"] {
            let spec = SeedSpec {
                password: Some(hostile.into()),
                ..SeedSpec::default()
            };
            let ud = spec.render_user_data().unwrap();
            assert!(!ud.contains(hostile), "plaintext leaked: {:?}", hostile);
            // Parse the document — if anything in it is malformed the
            // YAML parser raises an error, which fails the assert.
            let parsed: std::result::Result<serde_yaml_ng::Value, _> =
                serde_yaml_ng::from_str(strip_cloud_config_header(&ud));
            parsed
                .unwrap_or_else(|e| panic!("user-data for {:?} failed YAML parse: {}", hostile, e));
        }
    }

    /// Same protection for the operator-supplied user name. The
    /// rendering quotes nothing today (`name: <user>` bare scalar);
    /// if this ever breaks for a chosen username, switch to
    /// double-quoted scalars or a YAML emitter (per #118).
    #[test]
    fn user_data_with_hostile_user_is_valid_yaml() {
        // Pick names a normal CLI user might plausibly type. Anything
        // wilder (newlines, NUL) belongs in input validation, not the
        // YAML emitter.
        for hostile in ["op-1", "op_one", "op.dot"] {
            let spec = SeedSpec {
                user: Some(hostile.into()),
                ..SeedSpec::default()
            };
            let ud = spec.render_user_data().unwrap();
            let parsed: std::result::Result<serde_yaml_ng::Value, _> =
                serde_yaml_ng::from_str(strip_cloud_config_header(&ud));
            parsed.unwrap_or_else(|e| {
                panic!("user-data for user={:?} failed YAML parse: {}", hostile, e)
            });
        }
    }

    #[test]
    #[ignore] // requires xorrisofs / genisoimage; gated to keep CI hardware-free
    fn write_iso_round_trip_via_tool() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("seed.iso");
        let spec = SeedSpec {
            user: Some("test".into()),
            ssh_keys: vec!["ssh-ed25519 AAAA test".into()],
            ..SeedSpec::default()
        };
        spec.write_iso(&out).expect("ISO must build");
        let bytes = std::fs::read(&out).unwrap();
        // ISO9660 has a "CD001" magic at offset 0x8001.
        assert!(bytes.len() > 0x8006);
        assert_eq!(&bytes[0x8001..0x8006], b"CD001");
        // Volume identifier is at 0x8028, padded with spaces. Pinned
        // at "cidata" via the -V flag we pass.
        let vol_id = &bytes[0x8028..0x8030];
        assert_eq!(&vol_id[..6], b"cidata");
    }
}
