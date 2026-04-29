# Daemon syscall + path inventory for the sandbox (#20)

Captured via `strace -f -tt` over a representative workload:
- `daemon start --foreground` (no-fork; whole process under strace)
- `boot l2cpu 0 -d <buildroot rootfs> -n` (cold-boot, file reads, NOC TLB ioctls)
- `connect -l 0` running `dmesg -n 1; echo hello-from-strace`
- `remove-disk` + `add-disk` cycle
- `remove-net` + `add-net` cycle
- `daemon ports` + `daemon status` (port-conflict probe)
- SIGTERM clean shutdown

8.5 MiB raw trace; this file is the analysis. Re-run by:

```bash
strace -f -tt -o /tmp/strace-daemon.log \
  ./target/debug/bhx daemon start --foreground -t 0 \
    --log-file ./daemon-card0.log
# in a separate shell, drive the workload above; then SIGTERM the daemon.
```

## Distinct syscalls — 53 total

Grouped by purpose, with seccomp policy intent.

### Allow always (steady-state hot path)

| Syscall | Why |
|---------|-----|
| `read`, `write`, `pread64`, `pwrite64` | virtio worker MMIO + control-socket IO |
| `close`, `fcntl` | fd lifecycle (CLOEXEC, NONBLOCK) |
| `lseek`, `fstat`, `statx` | disk image / log file ops |
| `flock`, `ftruncate` | pidfile (`acquire_pidfile`), log rotate |
| `pipe2`, `socketpair` | per-client console fd transfer (`SCM_RIGHTS`) |
| `mmap`, `munmap`, `mprotect`, `madvise` | TLB windows, allocator |
| `brk` | allocator heap growth |
| `clone3` | thread spawn (chip\_console / virtio worker / dispatch handler). Always uses `CLONE_THREAD` per the trace, never a full process fork |
| `futex`, `set_robust_list`, `rseq` | std::sync primitives + glibc TLS |
| `prctl(PR_SET_NAME, …)` | thread name for /proc/$pid/task/$tid/comm. Filter via arg-0 to allow only `PR_SET_NAME` (15) |
| `getpid`, `gettid` | dlog! prefix |
| `tgkill`, `exit_group`, `exit` | thread teardown / panic handling |
| `clock_gettime`, `clock_nanosleep`, `nanosleep` | adaptive sleep tiers + log timestamps |
| `pselect6`, `poll`, `ppoll` | listener accept loop + slirp internal |
| `accept`, `accept4`, `getsockname`, `getsockopt`, `setsockopt` | unix socket + slirp listener. **Both** `accept` and `accept4` are needed: Rust code uses `accept4(SOCK_CLOEXEC)`, but libvdeslirp's worker thread calls the legacy `accept(2)` on the SSH-forward listener — without it slirp busy-loops on EPERM, see #32. |
| `recvfrom`, `sendto`, `sendmsg` | wire-format JSON + SCM\_RIGHTS console fd |
| `socket(AF_UNIX,…)`, `socket(AF_INET,SOCK_STREAM,…)` | control sock + slirp's TCP forward listener |
| `bind`, `listen` | same — slirp listens on `127.0.0.1:<ssh_port>` |
| `connect` | libslirp's TCP NAT path opens a host-side socket per guest-initiated outbound flow. Without this, guest TCP fails with ENETUNREACH (libslirp catches the EPERM in `tcp_fconnect` and synthesizes ICMP unreachable). The original audit only traced inbound SSH-forward, which uses `accept` not `connect`, so this got missed — see #65. |
| `getrandom` | random bytes (slirp's TCP ISN, log timestamps) |
| `ioctl` with magic `0xfa` | tt-kmd: ALLOCATE_TLB / FREE_TLB / CONFIGURE_TLB / RESET_DEVICE / GET_DEVICE_INFO. See `src/kmd.rs` |
| `ioctl(_, FIONBIO, …)` | nonblocking unix socket setup |

### Allow at startup; block after sandbox install

These show up only during binary load + `daemon start` setup. Sandbox is installed once `serve()` enters its accept loop, so blocking these post-startup is safe.

| Syscall | Use |
|---------|-----|
| `execve` | binary load (once) |
| `arch_prctl`, `set_tid_address`, `sigaltstack` | glibc startup, signal stack |
| `prlimit64`, `sched_getaffinity` | resource probe |
| `getcwd` | absolutize log path |
| `mkdir`, `chmod`, `unlink` | runtime-dir + sock-file setup |
| `access` | dlopen path search |
| `rt_sigaction`, `rt_sigprocmask` | ctrlc crate's signal handler install |

### Explicitly block (never observed; close the door)

| Syscall | Why |
|---------|-----|
| `fork`, `vfork` | We're past the fork point. clone3 with CLONE_THREAD is what we use |
| `chroot`, `pivot_root`, `mount`, `unshare`, `setns` | Container-escape primitives |
| `ptrace` | Debug-injection primitive |
| `setuid`, `setgid`, `setresuid`, `setresgid` | Privilege change. Daemon runs as the operator's UID; staying there is the contract |
| `bpf`, `userfaultfd`, `kexec_load` | Privileged kernel surfaces |

## Distinct openat paths

```
/dev/tenstorrent/0
/dev/urandom
/etc/ld.so.cache
/etc/localtime
/etc/resolv.conf
<repo>/blackhole-card.dtb
<repo>/fw_jump.bin
<repo>/Image
<repo>/daemon-card0.log
<repo>/tests/rootfs/buildroot-2026.02.1/output/images/rootfs.ext2
/lib/x86_64-linux-gnu/{libatomic,libc,libfdt,libgcc_s,libglib-2.0,libm,libpcre2-8,libslirp,libvdeslirp}.so.{0,1,6}
/proc/self/maps
/run/user/1000/bhx/0/logpath
/run/user/1000/bhx/0/pid
```

### Landlock policy

Tier into "always" / "boot-time" / "log only":

- `/dev/tenstorrent/<card>` — read+write, always.
- `/dev/urandom` — read, always.
- `/run/user/<uid>/bhx/<card>/` — read+write, always (pidfile, sock, log, sidecars).
- The `--log-file` path (if outside the runtime dir) — read+write+truncate.
- The operator's working dir at `daemon start` time — **read** only, always. Captures rootfs.ext4 reads, blackhole-card.dtb / Image / fw_jump.bin reads, and any path the operator might `add-disk` later. Wider than ideal; see #20's "narrowing" note for the per-RPC scope alternative.
- `/etc/{resolv.conf,localtime}` — read.
- `/lib*/x86_64-linux-gnu/*` — read (already-resolved at startup; dlopen during plugin load not used today, but defensive).
- `/proc/self/maps` — read.

The `/lib*` and dlopen-related paths only matter if we install landlock *before* dlopen happens. The plan is to install landlock + seccomp inside `serve()` after dlopen has finished, so we can drop those from the policy.

## Distinct ioctl request codes

| Hex | Decoded | Site |
|-----|---------|------|
| `_IOC(_IOC_NONE, 0xfa, 0x0, 0)` | `IOCTL_GET_DEVICE_INFO` | l2cpu init |
| `_IOC(_IOC_NONE, 0xfa, 0x6, 0)` | `IOCTL_RESET_DEVICE` | not in this trace; `--force-reset-pcie` |
| `_IOC(_IOC_NONE, 0xfa, 0xb, 0)` | `IOCTL_ALLOCATE_TLB` | TLB window setup |
| `_IOC(_IOC_NONE, 0xfa, 0xc, 0)` | `IOCTL_FREE_TLB` | TLB window teardown |
| `_IOC(_IOC_NONE, 0xfa, 0xd, 0)` | `IOCTL_CONFIGURE_TLB` | TLB window aim |
| `FIONBIO` (`0x5421`) | non-blocking fd | unix socket + listener |

The seccomp filter can either:
1. Allow `ioctl` unconditionally (simplest — seccomp-bpf can match on the request arg but it's awkward across 64-bit args).
2. Match on arg-1's high half == `0xfa00` for kmd ioctls, plus an explicit `FIONBIO` allow.

Option 1 is good enough — `ioctl` itself isn't a privilege-escalation vector when the daemon already holds an operator-trusted fd. Keep simple.

## What's missing from the issue's whitelist

The issue (filed before strace data) listed `epoll_create1`, `epoll_ctl`, `epoll_wait` as expected polling primitives. The trace shows we use `pselect6` / `poll` / `ppoll` instead — **no epoll**. Trim the whitelist accordingly.

The issue listed `dup3` for fd ops; trace shows `fcntl` (`F_DUPFD_CLOEXEC` style) but no `dup3`. Drop `dup3` unless a future change needs it.

The issue's `pipe2` is right — we use it for ctrlc's signal-handling pipe.

`socket` / `bind` / `listen` / `connect` weren't on the issue's whitelist but are required for slirp's TCP forwarder. The first three for inbound NAT (host listens, accepts a host-side connect, forwards into the guest); `connect` for outbound NAT (libslirp opens a host-side TCP socket per guest-initiated flow). The original triage only noticed the inbound surface, which is why `connect` was wrongly flagged as "zero in the trace" — see #65.

## Implementation

The inventory above is translated into a `seccompiler` filter + a `landlock` ruleset in `src/daemon/sandbox.rs`. Both install after `probe_initial_chip_state` + `warm_resume_released` have finished (those open `/dev/tenstorrent/<card>` and need full path access) but before the accept loop starts.

The sandbox is on by default. Operators pass `daemon start --no-sandbox` to opt out (debugging the filter itself, e.g. tracking down a missing syscall). Failure to install the sandbox is fatal — the daemon refuses to start rather than silently run unsandboxed.
