// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Daemon sandboxing: seccomp syscall whitelist + landlock filesystem
//! restrictions. Defense-in-depth so a virtio bug or JSON-deserializer
//! exploit can't pivot to read arbitrary host files / spawn processes
//! / open outbound network connections.
//!
//! Linux-only. Compiled out everywhere else; callers on non-Linux
//! get a no-op `apply()`.
//!
//! Filter shape derived from a representative strace; see
//! `docs/sandbox-syscalls.md` for the full inventory and rationale.
//!
//! Install point: just before the accept loop in `server::serve`.
//! The warm-resume probe runs *before* `apply()` because it issues
//! kmd ioctls and may spawn chip-console workers; both are
//! whitelisted, so re-applying via TSYNC catches those workers
//! too (`apply_filter_all_threads`).

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use landlock::{
        Access, AccessFs, BitFlags, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, ABI,
    };
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, SeccompRule, TargetArch};

    /// Apply both filters in order: landlock first (file paths), then
    /// seccomp (syscalls). Order matters — once seccomp installs, even
    /// `prctl(PR_SET_NO_NEW_PRIVS)` is blocked, but landlock requires
    /// `NO_NEW_PRIVS` to be set first if we don't have CAP_SYS_ADMIN
    /// (we don't), so landlock has to go first.
    pub fn apply(card: u32, log_path: &Path) -> Result<(), String> {
        apply_landlock(card, log_path)?;
        apply_seccomp()?;
        crate::dlog!("[sandbox] seccomp + landlock filters installed");
        Ok(())
    }

    fn apply_landlock(card: u32, log_path: &Path) -> Result<(), String> {
        // ABI v3 = Linux 6.2+. Older kernels: the crate transparently
        // negotiates down. Best-effort if the kernel is too old —
        // RulesetStatus::FullyEnforced is the sweet spot.
        let abi = ABI::V3;
        let mut ruleset = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(|e| format!("landlock handle_access: {}", e))?
            .create()
            .map_err(|e| format!("landlock create: {}", e))?;

        // Always-needed paths.
        let mut allow = Vec::<(PathBuf, BitFlags<AccessFs>)>::new();

        // Chip device — read+write, plus open for ioctl.
        allow.push((
            PathBuf::from(format!("/dev/tenstorrent/{}", card)),
            AccessFs::ReadFile | AccessFs::WriteFile,
        ));
        // Randomness for slirp's TCP ISN, getrandom under the hood
        // doesn't go through openat but `xz`/etc. seed paths might.
        allow.push((PathBuf::from("/dev/urandom"), AccessFs::ReadFile.into()));
        // Per-card runtime dir (sock, pidfile, log default, sidecars).
        let runtime_dir = crate::daemon::lifetime::runtime_dir(card);
        allow.push((
            runtime_dir,
            AccessFs::ReadFile
                | AccessFs::WriteFile
                | AccessFs::ReadDir
                | AccessFs::RemoveFile
                | AccessFs::MakeReg
                | AccessFs::MakeDir,
        ));
        // Operator-supplied log file (if outside the runtime dir).
        if let Some(parent) = log_path.parent() {
            // Walk up until we find an existing dir (the file itself
            // may not exist yet).
            let mut p = parent.to_path_buf();
            while !p.exists() {
                match p.parent() {
                    Some(up) if !up.as_os_str().is_empty() => p = up.to_path_buf(),
                    _ => break,
                }
            }
            if p.exists() {
                allow.push((
                    p,
                    AccessFs::ReadFile
                        | AccessFs::WriteFile
                        | AccessFs::MakeReg
                        | AccessFs::ReadDir,
                ));
            }
        }
        // Operator's cwd at daemon start time — read-only, captures
        // rootfs.ext4 + Image + fw_jump.bin + blackhole-card.dtb +
        // any path the operator might pass to add-disk.
        if let Ok(cwd) = std::env::current_dir() {
            allow.push((cwd, AccessFs::ReadFile | AccessFs::ReadDir));
        }
        // The boot artifacts often live under ../tt-bh-linux/ (sibling
        // to this checkout). Also allow the parent of cwd, read-only,
        // so symlink traversal there resolves.
        if let Ok(cwd) = std::env::current_dir() {
            if let Some(parent) = cwd.parent() {
                allow.push((parent.to_path_buf(), AccessFs::ReadFile | AccessFs::ReadDir));
            }
        }
        // System read-only paths slirp + libc + dynamic-linker need.
        for (p, access) in [
            ("/etc", AccessFs::ReadFile | AccessFs::ReadDir),
            ("/usr", AccessFs::ReadFile | AccessFs::ReadDir),
            ("/lib", AccessFs::ReadFile | AccessFs::ReadDir),
            ("/lib64", AccessFs::ReadFile | AccessFs::ReadDir),
            ("/proc/self", AccessFs::ReadFile | AccessFs::ReadDir),
            // /tmp because slirp / libc may stage things there
            // (resolvconf, locale archives) and the daemon never has
            // a reason to NOT read tmp.
            ("/tmp", AccessFs::ReadFile | AccessFs::ReadDir),
        ] {
            if Path::new(p).exists() {
                allow.push((PathBuf::from(p), access));
            }
        }

        // Convert (path, access) tuples into landlock rules. Skip
        // paths that don't open (e.g. a missing /lib64 on a glibc-
        // only system).
        for (path, access) in allow {
            match PathFd::new(&path) {
                Ok(fd) => {
                    let rule = PathBeneath::new(fd, access);
                    ruleset = ruleset
                        .add_rule(rule)
                        .map_err(|e| format!("landlock add_rule({}): {}", path.display(), e))?;
                }
                Err(_) => {
                    // Path doesn't exist yet (e.g. log file's parent
                    // before first write). Not fatal.
                }
            }
        }

        let status = ruleset
            .restrict_self()
            .map_err(|e| format!("landlock restrict_self: {}", e))?;
        match status.ruleset {
            RulesetStatus::FullyEnforced => {
                crate::dlog!("[sandbox] landlock: fully enforced");
                Ok(())
            }
            RulesetStatus::PartiallyEnforced => {
                crate::dlog!(
                    "[sandbox] landlock: partially enforced (kernel ABI < V3); \
                     some access types not restricted"
                );
                Ok(())
            }
            RulesetStatus::NotEnforced => {
                Err("landlock not enforced (no kernel support)".to_string())
            }
        }
    }

    fn apply_seccomp() -> Result<(), String> {
        // Steady-state syscall whitelist. Source: docs/sandbox-syscalls.md.
        // Anything NOT in this list returns EPERM, which surfaces as a
        // fail-the-RPC error rather than a SIGSYS abort, so a missed
        // syscall in the whitelist is recoverable (operator-visible).
        //
        // Filter is per-arch via libc::SYS_*; built once for the
        // target architecture (TargetArch::x86_64 is the only host
        // we ship for today).
        let allowed = [
            // --- file / fd lifecycle ---
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_pread64,
            libc::SYS_pwrite64,
            libc::SYS_close,
            libc::SYS_fcntl,
            libc::SYS_lseek,
            libc::SYS_fstat,
            libc::SYS_statx,
            libc::SYS_flock,
            libc::SYS_ftruncate,
            libc::SYS_pipe2,
            libc::SYS_socketpair,
            libc::SYS_openat,
            libc::SYS_getcwd,
            libc::SYS_access,
            libc::SYS_faccessat,
            libc::SYS_readlink,
            libc::SYS_readlinkat,
            libc::SYS_getdents64,
            // --- memory ---
            libc::SYS_mmap,
            libc::SYS_munmap,
            libc::SYS_mprotect,
            libc::SYS_mremap,
            libc::SYS_madvise,
            libc::SYS_brk,
            // --- threading / sync (clone3 with CLONE_THREAD only;
            //     no fork/vfork/clone(2) — those return EPERM) ---
            libc::SYS_clone3,
            libc::SYS_futex,
            libc::SYS_set_robust_list,
            libc::SYS_rseq,
            libc::SYS_getpid,
            libc::SYS_gettid,
            libc::SYS_tgkill,
            libc::SYS_exit,
            libc::SYS_exit_group,
            libc::SYS_sched_getaffinity,
            libc::SYS_sched_yield,
            libc::SYS_getrandom,
            // --- time / poll ---
            libc::SYS_clock_gettime,
            libc::SYS_clock_nanosleep,
            libc::SYS_nanosleep,
            libc::SYS_pselect6,
            libc::SYS_poll,
            libc::SYS_ppoll,
            libc::SYS_epoll_pwait,
            libc::SYS_epoll_wait,
            libc::SYS_epoll_create1,
            libc::SYS_epoll_ctl,
            // --- sockets (AF_UNIX + AF_INET inbound only) ---
            libc::SYS_socket,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_accept4,
            libc::SYS_getsockname,
            libc::SYS_getsockopt,
            libc::SYS_setsockopt,
            libc::SYS_recvfrom,
            libc::SYS_recvmsg,
            libc::SYS_sendto,
            libc::SYS_sendmsg,
            libc::SYS_shutdown,
            // Note: SYS_connect deliberately excluded.
            // --- ioctl (allowed unconditionally — the dangerous
            //     surface is `connect`/`bpf`/etc., not ioctl on the
            //     fds we already hold) ---
            libc::SYS_ioctl,
            // --- signal handling (SIGPIPE to NOSIGPIPE etc.; allowed
            //     unconditionally because the install is post-startup
            //     so attackers can't repurpose installation) ---
            libc::SYS_rt_sigaction,
            libc::SYS_rt_sigprocmask,
            libc::SYS_rt_sigreturn,
            libc::SYS_sigaltstack,
            // --- process introspection (cheap, low-risk) ---
            libc::SYS_prctl,
        ];
        let rules: BTreeMap<i64, Vec<SeccompRule>> = allowed.iter().map(|&n| (n, vec![])).collect();

        let filter = SeccompFilter::new(
            rules,
            // Mismatch action: EPERM, not SIGSYS. Daemon stays alive
            // and surfaces the failure as an error reply; operator
            // sees the bad syscall in the dlog.
            SeccompAction::Errno(libc::EPERM as u32),
            SeccompAction::Allow,
            TargetArch::x86_64,
        )
        .map_err(|e| format!("seccomp filter build: {:?}", e))?;
        let bpf: BpfProgram = filter
            .try_into()
            .map_err(|e| format!("seccomp filter compile: {:?}", e))?;
        // Apply to all threads (TSYNC) so chip-console workers spawned
        // by warm-resume before this point inherit too.
        seccompiler::apply_filter_all_threads(&bpf)
            .map_err(|e| format!("seccomp install: {:?}", e))?;
        crate::dlog!(
            "[sandbox] seccomp filter installed (whitelist of {} syscalls)",
            allowed.len()
        );
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use imp::apply;

#[cfg(not(target_os = "linux"))]
pub fn apply(_card: u32, _log_path: &std::path::Path) -> Result<(), String> {
    // No-op on non-Linux; the daemon target is Linux but the build
    // is portable enough to compile elsewhere for tooling reasons.
    Ok(())
}
