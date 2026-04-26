// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Control-socket server: accepts clients, dispatches ops, holds L2CPU state.
//!
//! Each accepted client gets its own thread. Most RPCs are short-lived
//! request → reply (boot, status, add-disk, etc.). The exception is
//! `AttachConsole`: the daemon keeps a per-client thread alive that
//! shuttles bytes between the client's socketpair end and the hub's
//! input channel for as long as the client stays attached.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use crate::boot;
use crate::daemon::chip_console;
use crate::daemon::console_hub::ConsoleHub;
use crate::daemon::lifetime;
use crate::daemon::protocol::{
    read_frame, send_fd, write_frame, ConsoleMode, L2CpuState, L2CpuStatus, Request, Response,
    StatusPayload,
};
use crate::daemon::{DaemonState, DiskWorker, L2CpuSlot, WorkerHandle};
use crate::dlog;
use crate::l2cpu::L2Cpu;
use crate::virtio::block;
use crate::virtio::interrupt::InterruptController;
#[cfg(feature = "slirp")]
use crate::virtio::network;

// VirtIO MMIO offsets and interrupt numbers come from `crate::regs::virtio_mmio`;
// re-export under the legacy short names so the dispatch code reads cleanly.
use crate::regs::virtio_mmio::{DISK_IRQ as DISK_INT, DISK_OFFSET as DISK_MMIO};
#[cfg(feature = "slirp")]
use crate::regs::virtio_mmio::{NET_IRQ as NET_INT, NET_OFFSET as NET_MMIO};

/// Run the daemon accept loop foreground-style. Returns on SIGTERM / SIGINT
/// once the shutdown flag has been tripped. Caller is responsible for
/// daemonization (double-fork) before calling this.
// Logging convention inside this module: use `dlog!` for every line we
// emit, so the log file has consistent `[timestamp pid=… tid=…]`
// prefixes that grep/triage tooling can rely on. Failures inside a
// `dispatch_*` handler get *both* a `dlog!` (so the daemon log records
// what happened) *and* a `reply_err` (so the client sees an error).
// Plain `eprintln!` is reserved for `runner.rs` (pre-daemonize messages
// to the user's terminal) and `main.rs` (CLI-side errors).
//
// Lower-level modules (`chip.rs`, `shared_chip.rs`, `virtio/*`,
// `chip_console.rs`) still use `eprintln!` because they're shared with
// `cargo run -- debug …` subcommands where eprintln-to-terminal is the
// right default; daemon callers see those lines via the stderr → log
// redirect, just without the timestamp prefix.
pub fn serve(
    card: u32,
    listener: UnixListener,
    sandbox: bool,
    log_path: &Path,
    metrics_port: Option<u16>,
) -> io::Result<()> {
    // Open the one-and-only persistent TLB window to tile (8,0) before
    // anything else touches chip state, so the daemon has a single
    // serialization point for PLL / reset register access. Fallible because
    // it opens the card fd and issues ALLOCATE_TLB; propagate the error so
    // the daemon exits cleanly if /dev/tenstorrent/<card> is missing or
    // the kmd rejects the allocation.
    let shared_chip = Arc::new(crate::shared_chip::SharedChip::new(card)?);
    let state = Arc::new(DaemonState::new(card, shared_chip));
    install_signal_handlers(state.shutdown.clone());

    listener.set_nonblocking(true)?;

    dlog!("[daemon] accepting connections on card {}", card);
    let released = probe_initial_chip_state(&state.shared_chip, card);
    if !released.is_empty() {
        warm_resume_released(&state, &released);
    }

    // Spawn the metrics exporter BEFORE the sandbox so a `bind()`
    // failure (e.g. port in use) is fatal at start time, mirroring how
    // sandbox-install failures abort the daemon. Once the listener is
    // up, the accept thread runs under whatever sandbox we install
    // next (the seccomp filter already allows TCP listen+accept).
    if let Some(port) = metrics_port {
        if let Err(e) = crate::daemon::metrics::spawn_exporter(port, state.clone()) {
            return Err(io::Error::other(format!(
                "metrics exporter bind on 127.0.0.1:{} failed: {}",
                port, e
            )));
        }
        // The bound port is also the requested port (we don't pass 0
        // through the CLI), so we don't need to log it here — the
        // metrics module already logs the bind on its own.
    }

    // Install seccomp + landlock AFTER chip probe + warm-resume have
    // opened /dev/tenstorrent/<card> and done their initial ioctls,
    // BEFORE the accept loop spawns dispatch threads. The sandbox
    // module uses TSYNC so chip-console workers spawned by warm-
    // resume inherit too. On by default; operators pass
    // `daemon start --no-sandbox` to opt out (debugging only). See
    // `docs/sandbox-syscalls.md` for the policy.
    if sandbox {
        // Failure to install is fatal — refuse to start rather than
        // silently run unsandboxed. The crate::Error → io::Error bridge
        // (see error.rs) preserves the variant's display string.
        crate::daemon::sandbox::apply(card, log_path)?;
    }

    while !state.shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((sock, _addr)) => {
                let state = state.clone();
                thread::spawn(move || handle_client(sock, state));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                dlog!("[daemon] accept error: {}", e);
                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    dlog!("[daemon] shutdown flag set — tearing down L2CPU slots");
    for slot_mutex in state.l2cpus.iter() {
        if let Some(slot) = slot_mutex.lock().unwrap().take() {
            slot.shutdown();
        }
    }
    // Clean up socket file; pidfile flock is released when our guard drops in
    // the caller (`run_foreground`).
    let _ = std::fs::remove_file(lifetime::socket_path(card));
    dlog!("[daemon] bye");
    Ok(())
}

/// Probe the chip's L2CPU_RESET register once at daemon startup and log
/// each L2CPU's state. Returns the list of core indices that are
/// released (bit idx+4 == 1) — warm-resume candidates.
///
/// Safe to call even when the chip is wedged: reading the reset register
/// is a single AXI read to tile (8,0), no state change.
fn probe_initial_chip_state(shared: &crate::shared_chip::SharedChip, card: u32) -> Vec<u8> {
    let val = shared.read_l2cpu_reset();
    dlog!("[probe] L2CPU_RESET={:#010x} (card {})", val, card);
    let mut released = Vec::new();
    for idx in 0..4u8 {
        let bit = (val >> (idx + 4)) & 1;
        let state = if bit == 1 {
            "released (running or wedged — warm-resume candidate)"
        } else {
            "held in reset (cold-bootable)"
        };
        dlog!(
            "[probe]   L2CPU {} bit {} = {} -> {}",
            idx,
            idx + 4,
            bit,
            state
        );
        if bit == 1 {
            released.push(idx);
        }
    }
    released
}

/// For each released core, probe the chip's VIRTUART / OSBIdbug
/// signatures. If valid → construct a runtime slot (warm-resume, console
/// worker starts immediately). If invalid → mark the core `wedged` in
/// `DaemonState` so `dispatch_status` reports it as such; the user
/// recovers via `boot --force-reset-pcie`.
///
/// Only the console worker is started — the daemon has no way to know
/// which disk image or network config was attached before, so the user
/// must re-issue `add-disk` / `add-net` to rewire those. The chip's
/// guest kernel stays up throughout; virtio descriptor chains re-sync
/// on the next queue kick once workers come back.
fn warm_resume_released(state: &Arc<DaemonState>, released: &[u8]) {
    for &idx in released {
        dlog!("[warm-resume l2cpu {}] probing chip state", idx);
        let l2cpu = match L2Cpu::new(idx as usize, state.card) {
            Ok(c) => Arc::new(c),
            Err(e) => {
                dlog!(
                    "[warm-resume l2cpu {}] L2Cpu::new failed: {} — marking wedged",
                    idx,
                    e
                );
                state.wedged[idx as usize].store(true, Ordering::Relaxed);
                continue;
            }
        };
        if !chip_console::probe_warm_resume(&l2cpu) {
            dlog!(
                "[warm-resume l2cpu {}] probe failed — marking wedged, dropping L2Cpu",
                idx
            );
            state.wedged[idx as usize].store(true, Ordering::Relaxed);
            // Arc<L2Cpu> drops here; TLB windows and 8 GB VA released.
            continue;
        }
        dlog!(
            "[warm-resume l2cpu {}] probe passed; adopting (console only — use add-disk/add-net to reattach)",
            idx
        );
        match make_slot_from_l2cpu(l2cpu, idx) {
            Ok(slot) => {
                *state.l2cpus[idx as usize].lock().unwrap() = Some(slot);
                state.wedged[idx as usize].store(false, Ordering::Relaxed);
                crate::daemon::metrics::L2CPU_BOOT_WARM_TOTAL.at(idx).inc();
                dlog!("[warm-resume l2cpu {}] slot adopted", idx);
            }
            Err(e) => {
                dlog!(
                    "[warm-resume l2cpu {}] make_slot_from_l2cpu failed: {} — marking wedged",
                    idx,
                    e
                );
                state.wedged[idx as usize].store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Install handlers for SIGTERM / SIGINT that flip the daemon's shutdown flag.
fn install_signal_handlers(flag: Arc<AtomicBool>) {
    // ctrlc handles both SIGINT and SIGTERM via set_handler (it spawns a
    // dedicated thread that converts signals into handler invocations, so
    // we don't have to think about async-signal-safety in the closure).
    //
    // Relaxed is sufficient: the flag is the only thing we read on the
    // accept-loop side, and there's nothing else that needs to be
    // happens-before-ordered against the flag write. Both sides of the
    // flag now use Relaxed; if you ever need to publish other state along
    // with the shutdown signal, upgrade both sides together.
    ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))
        .expect("failed to install SIGINT/SIGTERM handler");
}

fn handle_client(mut sock: UnixStream, state: Arc<DaemonState>) {
    crate::daemon::metrics::DAEMON_CLIENTS_TOTAL.inc();
    crate::daemon::metrics::DAEMON_CLIENTS_ACTIVE.inc();
    // RAII so the active gauge decrements on every return path
    // (read failure, dispatch panic, etc.) without sprinkling decs.
    struct ActiveGuard;
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            crate::daemon::metrics::DAEMON_CLIENTS_ACTIVE.dec();
        }
    }
    let _active_guard = ActiveGuard;

    let req: Request = match read_frame(&mut sock) {
        Ok(r) => r,
        Err(e) => {
            dlog!("[daemon] read request failed: {}", e);
            let _ = write_frame(
                &mut sock,
                &Response::Error {
                    error: format!("bad request: {}", e),
                },
            );
            return;
        }
    };

    let method = classify_request(&req);
    crate::daemon::metrics::DAEMON_RPC_TOTAL.at(method).inc();
    // Reset the per-thread "did the dispatch fail?" flag so each
    // RPC starts clean. `reply_err` flips it on the way out (we
    // call reply_err from the Err arm below); we read it after
    // dispatch to bump the per-method error counter.
    RPC_FAILED.with(|f| f.set(false));

    let result: crate::Result<()> = match req {
        Request::Status => dispatch_status(&sock, &state),
        Request::Boot {
            l2cpu,
            opensbi,
            payload,
            dtb,
            initramfs,
            root_device,
            force_reset_pcie,
            disk,
            network,
            force,
        } => dispatch_boot(
            &sock,
            &state,
            l2cpu,
            &opensbi,
            &payload,
            &dtb,
            initramfs.as_deref(),
            &root_device,
            force_reset_pcie,
            disk,
            network,
            force,
        ),
        Request::AttachConsole { l2cpu, mode } => {
            dispatch_attach_console(&sock, &state, l2cpu, mode)
        }
        Request::AddDisk { l2cpu, path } => dispatch_add_disk(&sock, &state, l2cpu, path),
        Request::RemoveDisk { l2cpu } => dispatch_remove_disk(&sock, &state, l2cpu),
        Request::AddNet {
            l2cpu,
            ssh_port,
            extra_fwd,
        } => dispatch_add_net(&sock, &state, l2cpu, ssh_port, extra_fwd),
        Request::RemoveNet { l2cpu } => dispatch_remove_net(&sock, &state, l2cpu),
        Request::Stop { l2cpu } => dispatch_stop(&sock, &state, l2cpu),
        Request::Shutdown => dispatch_shutdown(&sock, &state),
    };

    // Route Err results to reply_err, which writes a wire frame
    // and handles the Internal → "internal daemon error" + dlog
    // translation. Ok(()) means the dispatch already wrote its
    // response (Status payload, Attached, or Response::Ok).
    if let Err(e) = result {
        reply_err(&sock, e);
    }

    if RPC_FAILED.with(|f| f.get()) {
        crate::daemon::metrics::DAEMON_RPC_ERRORS_TOTAL
            .at(method)
            .inc();
    }
}

std::thread_local! {
    /// Set by `reply_err` whenever a dispatch handler reports an
    /// error to the client. Read+reset by `handle_client` so the
    /// per-method failure counter ticks without each dispatch
    /// having to grow a return value.
    ///
    /// Per-thread is the right scope: each accepted client gets its
    /// own thread (via `thread::spawn(move || handle_client(...))`),
    /// so the flag never leaks across requests. handle_client clears
    /// the flag on entry to be defensive against thread reuse from
    /// some future thread-pool refactor.
    static RPC_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Map a wire-format `Request` to its metrics-friendly `RpcMethod` tag.
/// Drives `tt_bh_daemon_rpc_total{method}`. Per-method failures live
/// on `tt_bh_daemon_rpc_errors_total` and are tracked via the
/// `RPC_FAILED` thread-local — `reply_err` flips it, `handle_client`
/// reads it after the dispatch returns.
fn classify_request(req: &Request) -> crate::daemon::metrics::RpcMethod {
    use crate::daemon::metrics::RpcMethod;
    match req {
        Request::Status => RpcMethod::Status,
        Request::Boot { .. } => RpcMethod::Boot,
        Request::AttachConsole { .. } => RpcMethod::AttachConsole,
        Request::AddDisk { .. } => RpcMethod::AddDisk,
        Request::RemoveDisk { .. } => RpcMethod::RemoveDisk,
        Request::AddNet { .. } => RpcMethod::AddNet,
        Request::RemoveNet { .. } => RpcMethod::RemoveNet,
        Request::Stop { .. } => RpcMethod::Stop,
        Request::Shutdown => RpcMethod::Shutdown,
    }
}

fn reply_ok(mut sock: &UnixStream) {
    let _ = write_frame(&mut sock, &Response::Ok);
}

/// Translate a `crate::Error` to a wire `Response::Error` and write it.
/// `Internal` is special-cased: the operator-visible message is the
/// generic "internal daemon error" while the full context goes to the
/// daemon log via `dlog!`. All other variants pass their `Display`
/// through as-is — wire format is identical to the pre-#21 shape.
///
/// Also flips the `RPC_FAILED` thread-local so `handle_client` knows
/// to bump `tt_bh_daemon_rpc_errors_total{method}` on the way out.
fn reply_err(mut sock: &UnixStream, e: crate::Error) {
    RPC_FAILED.with(|f| f.set(true));
    let wire_msg = match &e {
        crate::Error::Internal(msg) => {
            dlog!("[dispatch] internal error: {}", msg);
            "internal daemon error".to_string()
        }
        _ => e.to_string(),
    };
    let _ = write_frame(&mut sock, &Response::Error { error: wire_msg });
}

/// Bounds-check the wire-format `l2cpu` index. Every dispatch handler
/// starts with the same `if idx >= 4 { return Err(BadRequest) }`
/// dance — route it through one place so a future change to the
/// per-card core count needs to land in exactly one match arm.
fn validate_l2cpu(idx: u8) -> crate::Result<usize> {
    if idx < 4 {
        Ok(idx as usize)
    } else {
        Err(crate::Error::bad_request("l2cpu must be 0..3"))
    }
}

/// Probe whether a TCP port on 127.0.0.1 is available to bind. Used by
/// `dispatch_add_net` before handing the port to slirp so an
/// already-occupied port produces a useful error instead of a slirp
/// `vdeslirp_open` returning NULL with no context.
///
/// There is a small TOCTOU window between the probe-bind dropping and
/// slirp's subsequent bind. The intent is to catch operator-error
/// cases (port held by another process for a long time), not to
/// prevent concurrent-bind races.
#[cfg(feature = "slirp")]
fn probe_port_available(port: u16) -> std::io::Result<()> {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr)?;
    drop(listener); // release immediately so slirp can rebind
    Ok(())
}

/// What `dispatch_boot` should do given the current per-slot state and
/// the client's `force` flag. Pulled out as a pure function so the
/// reject-vs-teardown decision is unit-testable without a real slot
/// (which carries hardware-bound `Arc<L2Cpu>` and `Arc<InterruptController>`).
#[derive(Debug, PartialEq, Eq)]
enum BootSlotDecision {
    /// Slot is occupied and `force` wasn't set — return the message
    /// verbatim to the client.
    Reject(String),
    /// Slot is empty (or already torn down on a prior `force`); just
    /// proceed to the boot sequence.
    Proceed,
    /// Slot is occupied and `force` was set — caller must take the
    /// existing slot out of `DaemonState`, drop the lock, and call
    /// `slot.shutdown()` *before* the new NOC writes start.
    TearDownAndProceed,
}

fn decide_boot_slot(slot_present: bool, force: bool, l2cpu_idx: u8) -> BootSlotDecision {
    match (slot_present, force) {
        (false, _) => BootSlotDecision::Proceed,
        (true, true) => BootSlotDecision::TearDownAndProceed,
        (true, false) => BootSlotDecision::Reject(format!(
            "l2cpu {} is already booted; stop it first, or re-run with --force",
            l2cpu_idx
        )),
    }
}

/// Pre-flight check for `dispatch_add_disk`. Returns the opened File on
/// success so the caller can hand the *same* fd to the worker — a bare
/// path-then-reopen would leave a TOCTOU window where a symlink swap
/// between dispatch and the worker's first VirtioBlk::new could redirect
/// the daemon at a different inode (security finding from #17).
///
/// Catching the bad path here also avoids a stuck-slot state where
/// `slot.disks` is non-empty with a dead worker handle and subsequent
/// `add-disk` calls fail with "a disk is already attached". The
/// `disks_empty` argument is the `slot.disks.is_empty()` reading taken
/// under the slot mutex.
fn validate_add_disk_request(
    disks_empty: bool,
    path: &std::path::Path,
) -> crate::Result<std::fs::File> {
    if !disks_empty {
        // Phase A: one disk per L2CPU. Phase B+: multi-disk with indexed MMIO.
        return Err(crate::Error::slot_state("a disk is already attached"));
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(crate::Error::io_ctx(format!(
            "cannot open disk image {}",
            path.display()
        )))
}

/// Pre-flight check for `dispatch_remove_disk`. The slot-not-booted
/// rejection is handled by the dispatch I/O wrapper because it needs
/// to inspect `Option<L2CpuSlot>`; this helper is just for the
/// "no disk attached" case so we have something testable.
fn validate_remove_disk_request(disks_empty: bool) -> Result<(), &'static str> {
    if disks_empty {
        Err("no disk attached")
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

fn dispatch_status(mut sock: &UnixStream, state: &Arc<DaemonState>) -> crate::Result<()> {
    let mut l2cpus = Vec::new();
    for (idx, slot_mutex) in state.l2cpus.iter().enumerate() {
        let slot = slot_mutex.lock().unwrap();
        let (st, disk, net, clients) = match slot.as_ref() {
            None => {
                let st = if state.wedged[idx].load(Ordering::Relaxed) {
                    L2CpuState::Wedged
                } else {
                    L2CpuState::Stopped
                };
                (st, None, false, 0)
            }
            Some(s) => (
                L2CpuState::Running,
                s.disks.first().map(|d| d.path.clone()),
                s.net.is_some(),
                s.console_hub.client_count() as u32,
            ),
        };
        l2cpus.push(L2CpuStatus {
            idx: idx as u8,
            state: st,
            disk,
            net,
            clients,
        });
    }

    let payload = StatusPayload {
        pid: std::process::id(),
        uptime_secs: state.started.elapsed().as_secs(),
        l2cpus,
    };
    let _ = write_frame(&mut sock, &Response::Status(payload));
    Ok(())
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn dispatch_boot(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    opensbi: &str,
    payload: &crate::daemon::protocol::BootPayload,
    dtb: &str,
    initramfs: Option<&str>,
    root_device: &str,
    force_reset_pcie: bool,
    disk: Option<String>,
    network: bool,
    force: bool,
) -> crate::Result<()> {
    // U-Boot mode reads kernel + initrd from disk at runtime, so the
    // daemon's preloaded initramfs would be unreachable. Reject up
    // front so the operator gets a clear error rather than a silent
    // drop.
    let initramfs = if payload.is_uboot() && initramfs.is_some() {
        return Err(crate::Error::bad_request(
            "--initramfs is not supported in --uboot mode (U-Boot loads initrd from disk)",
        ));
    } else {
        initramfs
    };
    dlog!(
        "[boot l2cpu {}] dispatch_boot entry: opensbi={} payload={:?} dtb={} initramfs={:?} root={} force_reset_pcie={} disk={:?} network={} force={}",
        l2cpu_idx, opensbi, payload, dtb, initramfs, root_device, force_reset_pcie, disk, network, force
    );
    validate_l2cpu(l2cpu_idx)?;
    handle_existing_slot(state, l2cpu_idx, force).map_err(crate::Error::slot_state)?;

    // No daemon-wide serialization of the chip-touching phase here —
    // tile-(8,0) access is mediated by `SharedChip::seq_lock` inside each
    // of its typed methods, and per-L2CPU NOC traffic goes through each
    // L2CPU's own fd + TLB windows. Concurrent boots on different cores
    // proceed in parallel; the same-core case is still serialized by the
    // per-slot `Mutex<Option<L2CpuSlot>>` taken in `handle_existing_slot`.
    dlog!("[boot l2cpu {}] starting boot sequence", l2cpu_idx);
    let l2cpu = run_boot_sequence(
        state,
        l2cpu_idx,
        opensbi,
        payload,
        dtb,
        initramfs,
        root_device,
        force_reset_pcie,
    )
    .map_err(|e| {
        dlog!("[boot l2cpu {}] boot sequence failed: {}", l2cpu_idx, e);
        // Boot failures wrap an io::Error from chip access — preserve
        // the kind via Io { ctx, source }. The wire shape stays
        // `"boot failed: <io_error_display>"`.
        crate::Error::Io {
            ctx: "boot failed".into(),
            source: e,
        }
    })?;
    dlog!(
        "[boot l2cpu {}] boot sequence returned ok; initializing runtime slot",
        l2cpu_idx
    );

    let mut slot = make_slot_from_l2cpu(l2cpu, l2cpu_idx).map_err(|e| {
        dlog!(
            "[boot l2cpu {}] make_slot_from_l2cpu failed: {}",
            l2cpu_idx,
            e
        );
        crate::Error::Io {
            ctx: "post-boot L2Cpu init failed".into(),
            source: e,
        }
    })?;
    dlog!(
        "[boot l2cpu {}] slot ready (console worker spawned)",
        l2cpu_idx
    );

    start_initial_workers(&mut slot, state.card, l2cpu_idx, disk, network)
        .map_err(crate::Error::slot_state)?;
    install_slot_and_reply_ok(state, l2cpu_idx, slot, sock);
    Ok(())
}

/// Reject the boot if a slot is already populated and `--force` wasn't
/// given. With `--force`, take the prior slot out and shut it down so
/// the boot can proceed without races between the new NOC writes and
/// the prior workers' TLB mmaps. The reject-vs-teardown policy lives
/// in `decide_boot_slot`; this function is the I/O wrapper that grabs
/// the lock, invokes the policy, and threads the slot through.
fn handle_existing_slot(
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    force: bool,
) -> Result<(), String> {
    let prior = {
        let mut guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
        match decide_boot_slot(guard.is_some(), force, l2cpu_idx) {
            BootSlotDecision::Reject(msg) => return Err(msg),
            BootSlotDecision::Proceed => None,
            BootSlotDecision::TearDownAndProceed => guard.take(),
        }
    };
    if let Some(prior) = prior {
        dlog!(
            "[boot l2cpu {}] --force: tearing down existing slot before re-imaging",
            l2cpu_idx
        );
        prior.shutdown();
        dlog!("[boot l2cpu {}] prior slot torn down", l2cpu_idx);
    }
    Ok(())
}

/// Spawn the requested virtio workers *before* replying Ok — kernel hits
/// VFS mount at ~0.137s and has no retry. Three sequential RPCs
/// (boot + add-disk + add-net) lose that race; bundling them keeps the
/// worker threads up within a few ms of L2CPU reset release.
fn start_initial_workers(
    slot: &mut L2CpuSlot,
    card: u32,
    l2cpu_idx: u8,
    disk: Option<String>,
    network: bool,
) -> Result<(), String> {
    if let Some(path) = disk {
        dlog!(
            "[boot l2cpu {}] spawning disk worker for {}",
            l2cpu_idx,
            path
        );
        // Open the disk image at the trust boundary so the worker
        // operates on the exact inode we vetted, immune to symlink
        // swaps between dispatch and the worker's mmap call.
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| {
                dlog!(
                    "[boot l2cpu {}] open disk image {} failed: {}",
                    l2cpu_idx,
                    path,
                    e
                );
                format!("cannot open disk image {}: {}", path, e)
            })?;
        start_disk_worker(slot, &path, file).map_err(|e| {
            dlog!("[boot l2cpu {}] start_disk_worker failed: {}", l2cpu_idx, e);
            format!("start disk worker failed: {}", e)
        })?;
    }
    if network {
        dlog!("[boot l2cpu {}] spawning net worker", l2cpu_idx);
        start_net_worker(card, slot).map_err(|e| {
            dlog!("[boot l2cpu {}] start_net_worker failed: {}", l2cpu_idx, e);
            format!("start net worker failed: {}", e)
        })?;
    }
    Ok(())
}

/// Park the slot in `DaemonState`, clear any stale `wedged` mark left
/// over from a prior startup probe (since the cold boot just succeeded
/// and the core is running with valid magic), and reply Ok to the
/// client.
fn install_slot_and_reply_ok(
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    slot: L2CpuSlot,
    sock: &UnixStream,
) {
    *state.l2cpus[l2cpu_idx as usize].lock().unwrap() = Some(slot);
    state.wedged[l2cpu_idx as usize].store(false, Ordering::Relaxed);
    crate::daemon::metrics::L2CPU_BOOT_COLD_TOTAL
        .at(l2cpu_idx)
        .inc();
    dlog!(
        "[boot l2cpu {}] dispatch_boot complete — replying ok",
        l2cpu_idx
    );
    reply_ok(sock);
}

/// Spawn the disk worker. `disk_image` is an already-open File for the
/// image — caller (typically `validate_add_disk_request`) opened it
/// once at the trust boundary and we hand the same handle to the
/// worker, defending against a path-resolved-twice TOCTOU.
fn start_disk_worker(
    slot: &mut L2CpuSlot,
    path: &str,
    disk_image: std::fs::File,
) -> io::Result<()> {
    let exit = Arc::new(AtomicBool::new(false));
    let l2cpu = slot.l2cpu.clone();
    let interrupt = slot.interrupt.clone();
    let exit_thread = exit.clone();
    let path_thread = path.to_string();
    let t = thread::spawn(move || {
        block::disk_main(
            l2cpu,
            interrupt,
            DISK_INT,
            DISK_MMIO,
            path_thread,
            disk_image,
            exit_thread,
        );
    });
    slot.disks.push(DiskWorker {
        path: path.to_string(),
        worker: WorkerHandle {
            exit,
            thread: Some(t),
            description: format!("disk l2cpu {} @ {}", slot.idx, path),
        },
    });
    Ok(())
}

#[cfg(feature = "slirp")]
fn start_net_worker(card: u32, slot: &mut L2CpuSlot) -> io::Result<()> {
    let exit = Arc::new(AtomicBool::new(false));
    let l2cpu = slot.l2cpu.clone();
    let interrupt = slot.interrupt.clone();
    let exit_thread = exit.clone();
    let idx = slot.idx;
    let ssh_port = crate::regs::slirp::ssh_port(card, idx);
    // Boot-path default: just the SSH forward. `add-net --fwd
    // HOST:GUEST` is the path for arbitrary forwards post-boot.
    let forwards = vec![(ssh_port, 22)];
    let t = thread::spawn(move || {
        network::network_main(forwards, l2cpu, interrupt, NET_INT, NET_MMIO, exit_thread);
    });
    slot.net = Some(WorkerHandle {
        exit,
        thread: Some(t),
        description: format!("net l2cpu {}", idx),
    });
    Ok(())
}

#[cfg(not(feature = "slirp"))]
fn start_net_worker(_card: u32, _slot: &mut L2CpuSlot) -> io::Result<()> {
    Err(io::Error::other("daemon built without the slirp feature"))
}

#[allow(clippy::too_many_arguments)]
fn run_boot_sequence(
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    opensbi: &str,
    payload: &crate::daemon::protocol::BootPayload,
    dtb: &str,
    initramfs: Option<&str>,
    root_device: &str,
    force_reset_pcie: bool,
) -> io::Result<Arc<L2Cpu>> {
    use crate::regs::boot_image;

    let card = state.card;
    let starting_address = crate::l2cpu::L2CPU_STARTING_ADDRESS[l2cpu_idx as usize];
    let memory_size = crate::l2cpu::L2CPU_MEMORY_SIZE[l2cpu_idx as usize];

    let opensbi_addr = starting_address + boot_image::OPENSBI_OFFSET;
    let kernel_addr = starting_address + boot_image::KERNEL_OFFSET;
    let dtb_addr = starting_address + boot_image::DTB_OFFSET;
    let rootfs_addr = starting_address + boot_image::INITRAMFS_OFFSET;

    // Pre-reset state check goes through the daemon's SharedChip — one
    // persistent TLB to tile (8,0) for all L2CPUs, so this read can't race
    // with other boots' reads/writes of the same register.
    let running = state.shared_chip.l2cpu_is_running(l2cpu_idx as usize);
    let need_reset = force_reset_pcie || running;
    dlog!(
        "[run_boot l2cpu {}] running={} force_reset_pcie={} need_reset={}",
        l2cpu_idx,
        running,
        force_reset_pcie,
        need_reset
    );

    if need_reset {
        dlog!(
            "[run_boot l2cpu {}] issuing full board reset (other L2CPUs on this card will see a PCIe blip)",
            l2cpu_idx
        );
        // SharedChip::reset_board internally drops its fd+window, issues the
        // PCIe LDS reset, and reopens fresh — callers never see stale fd
        // errors across the reset.
        state.shared_chip.reset_board(card)?;
        dlog!(
            "[run_boot l2cpu {}] board reset complete; sleeping 1s",
            l2cpu_idx
        );
        std::thread::sleep(std::time::Duration::from_secs(1));
    } else {
        dlog!(
            "[run_boot l2cpu {}] target held in reset; skipping board reset (siblings untouched)",
            l2cpu_idx
        );
    }

    // Construct the runtime L2Cpu handle BEFORE the image load so the load
    // can go through its persistent fd + `get_persistent_2m_window` UC
    // path (no shared tile-(8,0) aliasing). Returned from this function so
    // the caller hands the same Arc to `make_slot_from_l2cpu` — one
    // construction, one PLL step, no double-init.
    dlog!(
        "[run_boot l2cpu {}] constructing L2Cpu (ioctls + 8GB VA + TLB windows)",
        l2cpu_idx
    );
    let l2cpu = Arc::new(L2Cpu::new(l2cpu_idx as usize, card)?);

    dlog!("[run_boot l2cpu {}] reading DTB from {}", l2cpu_idx, dtb);
    let dtb_raw = boot::read_bin_file(Path::new(dtb))?;
    // U-Boot manages root + initrd at runtime. Skip the daemon's
    // initramfs preload + leave bootargs to U-Boot (no `root=/dev/vda`
    // injection).
    let boot_device = if payload.is_uboot() {
        boot::BootDevice::Uboot
    } else {
        match initramfs {
            Some(p) => {
                let bytes = boot::read_bin_file(Path::new(p))?;
                boot::BootDevice::Initramfs {
                    addr: rootfs_addr,
                    len: bytes.len() as u64,
                }
            }
            None => boot::BootDevice::Vda(root_device.to_string()),
        }
    };
    dlog!(
        "[run_boot l2cpu {}] patching DTB (memory start=0x{:x} size=0x{:x})",
        l2cpu_idx,
        starting_address,
        memory_size
    );
    let dtb_patched = boot::modify_dtb(&dtb_raw, &boot_device, starting_address, memory_size)?;

    let initramfs_pb = initramfs.map(std::path::PathBuf::from);
    dlog!(
        "[run_boot l2cpu {}] loading image via NOC tile writes",
        l2cpu_idx
    );
    boot::boot_l2cpu(
        &l2cpu,
        Path::new(opensbi),
        opensbi_addr,
        Some(Path::new(payload.path())),
        kernel_addr,
        &dtb_patched,
        dtb_addr,
        initramfs_pb.as_deref(),
        rootfs_addr,
    )?;

    dlog!("[run_boot l2cpu {}] releasing from reset", l2cpu_idx);
    // reset_x280 goes through SharedChip so the PLL step and the
    // L2CPU_RESET R-M-W serialize against any other boot RPC issuing the
    // same sequence.
    state.shared_chip.reset_x280(&[l2cpu_idx as usize]);
    dlog!("[run_boot l2cpu {}] configuring prefetchers", l2cpu_idx);
    boot::configure_prefetchers(&l2cpu);
    dlog!("[run_boot l2cpu {}] run_boot_sequence done", l2cpu_idx);
    Ok(l2cpu)
}

/// Build the runtime slot on top of an already-constructed `L2Cpu`. All
/// callers (dispatch_boot, warm-resume) construct the `L2Cpu` themselves
/// so the chip-touching phase runs exactly once per boot / adoption.
fn make_slot_from_l2cpu(l2cpu: Arc<L2Cpu>, l2cpu_idx: u8) -> io::Result<L2CpuSlot> {
    dlog!(
        "[make_slot l2cpu {}] L2Cpu ready; mapping PLIC interrupt window",
        l2cpu_idx
    );
    let interrupt = {
        let window = l2cpu.get_persistent_2m_window(crate::regs::plic::PENDING_ADDR)?;
        Arc::new(InterruptController::new(window))
    };
    let hub = Arc::new(ConsoleHub::new(l2cpu_idx));

    let (input_tx, input_rx) = mpsc::channel::<u8>();
    let exit = Arc::new(AtomicBool::new(false));

    dlog!(
        "[make_slot l2cpu {}] spawning chip_console thread",
        l2cpu_idx
    );
    let t = thread::spawn({
        let l2cpu = l2cpu.clone();
        let hub = hub.clone();
        let exit = exit.clone();
        move || chip_console::chip_console_main(l2cpu, hub, input_rx, exit)
    });

    Ok(L2CpuSlot {
        idx: l2cpu_idx,
        l2cpu,
        interrupt,
        console_hub: hub,
        console_input_tx: input_tx,
        console_worker: WorkerHandle {
            exit,
            thread: Some(t),
            description: format!("chip_console l2cpu {}", l2cpu_idx),
        },
        disks: Vec::new(),
        net: None,
        started: Instant::now(),
    })
}

// ---------------------------------------------------------------------------
// AttachConsole
// ---------------------------------------------------------------------------

fn dispatch_attach_console(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    mode: ConsoleMode,
) -> crate::Result<()> {
    validate_l2cpu(l2cpu_idx)?;

    let (daemon_end, client_end) =
        UnixStream::pair().map_err(crate::Error::io_ctx("socketpair"))?;

    // Hub writes via MSG_DONTWAIT so the socket can stay in blocking mode
    // for the reader thread. Clone the fd for the reader; the hub owns
    // `daemon_end` for fan-out.
    let daemon_read = daemon_end
        .try_clone()
        .map_err(crate::Error::io_ctx("try_clone"))?;

    // Grab everything we need from the slot under the mutex, then release it
    // before doing any IO so other client handlers don't stall.
    let (hub, input_tx) = {
        let slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
        let slot = slot_guard.as_ref().ok_or_else(|| {
            crate::Error::slot_state(format!("l2cpu {} is not booted", l2cpu_idx))
        })?;
        (slot.console_hub.clone(), slot.console_input_tx.clone())
    };

    let (res, scrollback) = hub.attach(daemon_end, mode);
    if !res.demoted.is_empty() {
        dlog!(
            "[daemon] l2cpu {} console takeover demoted {:?}",
            l2cpu_idx,
            res.demoted
        );
    }

    // Reply with attached + scrollback size, then send the console fd via
    // SCM_RIGHTS. Order matters: the client reads the Attached response,
    // then the fd, then starts pumping bytes on the fd.
    //
    // Failures here are post-attach: the hub already accepted the client.
    // We log + detach + return Ok(()) without surfacing an error to the
    // wire — the client either disconnected (so a Response::Error would
    // race with their own EOF) or has a half-written response on the
    // socket. Keep the wire-state semantics matching the pre-#21 code.
    if let Err(e) = write_frame(
        sock,
        &Response::Attached {
            scrollback_bytes: res.scrollback_bytes,
        },
    ) {
        dlog!("[daemon] attach write response: {}", e);
        hub.detach(res.id);
        return Ok(());
    }
    if let Err(e) = send_fd(sock, client_end.as_raw_fd()) {
        dlog!("[daemon] send_fd: {}", e);
        hub.detach(res.id);
        return Ok(());
    }
    drop(client_end);

    // Replay scrollback over `daemon_read` (blocking writes — 64 KiB fits
    // under SO_SNDBUF so this returns quickly without stalling the chip).
    if let Err(e) = write_scrollback(&daemon_read, &scrollback) {
        dlog!("[daemon] scrollback replay failed: {}", e);
        hub.detach(res.id);
        return Ok(());
    }

    thread::spawn(move || client_reader_main(daemon_read, res.id, hub, input_tx));
    Ok(())
}

/// Blocking write of scrollback bytes. Loops only on EINTR; returns any
/// other error to the caller (who will detach the client).
fn write_scrollback(mut sock: &UnixStream, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    sock.write_all(bytes)
}

/// Per-client reader: blocks on `sock`, forwards bytes to `input_tx` whenever
/// this client is the writer. Terminates on EOF or hub-driven drop.
fn client_reader_main(sock: UnixStream, id: u64, hub: Arc<ConsoleHub>, input_tx: mpsc::Sender<u8>) {
    use std::io::Read;
    let mut buf = [0u8; 128];
    loop {
        match (&sock).read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if hub.current_writer_id() == Some(id) {
                    for &b in &buf[..n] {
                        if input_tx.send(b).is_err() {
                            return;
                        }
                    }
                }
                // Non-writer bytes are silently dropped.
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    hub.detach(id);
}

// ---------------------------------------------------------------------------
// AddDisk
// ---------------------------------------------------------------------------

fn dispatch_add_disk(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    path: String,
) -> crate::Result<()> {
    validate_l2cpu(l2cpu_idx)?;
    let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
    let slot = slot_guard
        .as_mut()
        .ok_or_else(|| crate::Error::slot_state(format!("l2cpu {} is not booted", l2cpu_idx)))?;
    let disk_image = validate_add_disk_request(slot.disks.is_empty(), std::path::Path::new(&path))?;

    let exit = Arc::new(AtomicBool::new(false));
    let l2cpu = slot.l2cpu.clone();
    let interrupt = slot.interrupt.clone();
    let exit_thread = exit.clone();
    let path_thread = path.clone();
    let t = thread::spawn(move || {
        block::disk_main(
            l2cpu,
            interrupt,
            DISK_INT,
            DISK_MMIO,
            path_thread,
            disk_image,
            exit_thread,
        );
    });
    slot.disks.push(DiskWorker {
        path: path.clone(),
        worker: WorkerHandle {
            exit,
            thread: Some(t),
            description: format!("disk l2cpu {} @ {}", l2cpu_idx, path),
        },
    });
    reply_ok(sock);
    Ok(())
}

fn dispatch_remove_disk(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
) -> crate::Result<()> {
    dlog!("[remove_disk l2cpu {}] dispatch entry", l2cpu_idx);
    validate_l2cpu(l2cpu_idx)?;
    // Take the disks out under the lock, then release and join outside.
    // stop_and_join blocks until the worker's poll loop notices the exit
    // flag (~100 ms worst case); holding the state mutex for that long
    // would block every other RPC on other L2CPUs.
    let disks = {
        let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
        let slot = slot_guard.as_mut().ok_or_else(|| {
            crate::Error::slot_state(format!("l2cpu {} is not booted", l2cpu_idx))
        })?;
        validate_remove_disk_request(slot.disks.is_empty()).map_err(crate::Error::slot_state)?;
        std::mem::take(&mut slot.disks)
    };
    for d in disks {
        dlog!(
            "[remove_disk l2cpu {}] joining worker for {}",
            l2cpu_idx,
            d.path
        );
        d.worker.stop_and_join();
    }
    dlog!("[remove_disk l2cpu {}] done — replying ok", l2cpu_idx);
    reply_ok(sock);
    Ok(())
}

// ---------------------------------------------------------------------------
// AddNet
// ---------------------------------------------------------------------------

#[cfg(feature = "slirp")]
fn dispatch_add_net(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    ssh_port_override: Option<u16>,
    extra_fwd: Vec<(u16, u16)>,
) -> crate::Result<()> {
    dlog!(
        "[add_net l2cpu {}] dispatch entry (ssh_port_override={:?}, extra_fwd={:?})",
        l2cpu_idx,
        ssh_port_override,
        extra_fwd
    );
    validate_l2cpu(l2cpu_idx)?;
    let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
    let slot = slot_guard
        .as_mut()
        .ok_or_else(|| crate::Error::slot_state(format!("l2cpu {} is not booted", l2cpu_idx)))?;
    if slot.net.is_some() {
        return Err(crate::Error::slot_state("network already attached"));
    }

    // Resolve the host port. CLI override wins over the formula.
    let ssh_port =
        ssh_port_override.unwrap_or_else(|| crate::regs::slirp::ssh_port(state.card, l2cpu_idx));

    // Build the full forward list: implicit SSH first, then any
    // operator-supplied extras in order.
    let mut forwards: Vec<(u16, u16)> = vec![(ssh_port, 22)];
    forwards.extend(extra_fwd.iter().copied());

    // Pre-flight every host port. Reject the whole add-net if any
    // one is unavailable — slirp's tcp_listen_add doesn't roll back
    // on partial failure, so we'd be left in a half-installed state.
    for &(host_port, _guest_port) in &forwards {
        probe_port_available(host_port).map_err(crate::Error::io_ctx(format!(
            "host port {} unavailable. Pass a different host port (--fwd HOST:GUEST or --ssh-port), or stop whatever's using it",
            host_port
        )))?;
    }

    let exit = Arc::new(AtomicBool::new(false));
    let l2cpu = slot.l2cpu.clone();
    let interrupt = slot.interrupt.clone();
    let exit_thread = exit.clone();
    dlog!(
        "[add_net l2cpu {}] spawning network worker thread (forwards={:?})",
        l2cpu_idx,
        forwards
    );
    let forwards_thread = forwards.clone();
    let t = thread::spawn(move || {
        network::network_main(
            forwards_thread,
            l2cpu,
            interrupt,
            NET_INT,
            NET_MMIO,
            exit_thread,
        );
    });
    slot.net = Some(WorkerHandle {
        exit,
        thread: Some(t),
        description: format!("net l2cpu {}", l2cpu_idx),
    });
    dlog!(
        "[add_net l2cpu {}] dispatch complete — replying ok",
        l2cpu_idx
    );
    reply_ok(sock);
    Ok(())
}

#[cfg(not(feature = "slirp"))]
fn dispatch_add_net(
    _sock: &UnixStream,
    _state: &Arc<DaemonState>,
    _l2cpu_idx: u8,
    _ssh_port: Option<u16>,
    _extra_fwd: Vec<(u16, u16)>,
) -> crate::Result<()> {
    Err(crate::Error::bad_request(
        "daemon built without the slirp feature",
    ))
}

fn dispatch_remove_net(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
) -> crate::Result<()> {
    dlog!("[remove_net l2cpu {}] dispatch entry", l2cpu_idx);
    validate_l2cpu(l2cpu_idx)?;
    // Take the net handle under the lock, join outside (same reasoning as
    // dispatch_remove_disk).
    let net = {
        let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
        let slot = slot_guard.as_mut().ok_or_else(|| {
            crate::Error::slot_state(format!("l2cpu {} is not booted", l2cpu_idx))
        })?;
        slot.net
            .take()
            .ok_or_else(|| crate::Error::slot_state("no net attached"))?
    };
    dlog!("[remove_net l2cpu {}] joining worker", l2cpu_idx);
    net.stop_and_join();
    dlog!("[remove_net l2cpu {}] done — replying ok", l2cpu_idx);
    reply_ok(sock);
    Ok(())
}

// ---------------------------------------------------------------------------
// Stop / Shutdown
// ---------------------------------------------------------------------------

fn dispatch_stop(sock: &UnixStream, state: &Arc<DaemonState>, l2cpu_idx: u8) -> crate::Result<()> {
    dlog!("[stop l2cpu {}] dispatch_stop entry", l2cpu_idx);
    validate_l2cpu(l2cpu_idx)?;
    let taken = state.l2cpus[l2cpu_idx as usize].lock().unwrap().take();
    match taken {
        Some(slot) => {
            dlog!("[stop l2cpu {}] slot taken; joining workers", l2cpu_idx);
            slot.shutdown();
            dlog!("[stop l2cpu {}] workers joined — replying ok", l2cpu_idx);
            reply_ok(sock);
            Ok(())
        }
        None => {
            dlog!("[stop l2cpu {}] no slot present — replying err", l2cpu_idx);
            Err(crate::Error::slot_state(format!(
                "l2cpu {} is not booted",
                l2cpu_idx
            )))
        }
    }
}

fn dispatch_shutdown(sock: &UnixStream, state: &Arc<DaemonState>) -> crate::Result<()> {
    dlog!("[shutdown] dispatch_shutdown entry — setting shutdown flag");
    state.shutdown.store(true, Ordering::SeqCst);
    // We don't reach the `serve()` teardown until the accept loop notices the
    // flag, but the accept loop's sleep is 50 ms — client gets Ok promptly.
    reply_ok(sock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn validate_l2cpu_accepts_in_range() {
        for i in 0..4u8 {
            assert_eq!(validate_l2cpu(i).unwrap(), i as usize);
        }
    }

    #[test]
    fn validate_l2cpu_rejects_out_of_range() {
        for i in [4u8, 5, 99, u8::MAX] {
            assert!(validate_l2cpu(i).is_err());
        }
    }

    // ---- decide_boot_slot ----

    /// Wiring test for #31's RPC counter: drive a real `Request::Status`
    /// through `handle_client` over a unix socketpair and assert
    /// `DAEMON_RPC_TOTAL{method=status}` ticked. Catches a regression
    /// where someone accidentally removes the
    /// `metrics::DAEMON_RPC_TOTAL.at(...).inc()` line — pre-#33 the
    /// only thing keeping that line in place was code review. This
    /// test exercises classify_request, the bump, and dispatch_status
    /// in one shot.
    ///
    /// Note: globals are shared across tests in the same process. We
    /// snapshot `before` rather than asserting an absolute count.
    #[test]
    fn handle_client_bumps_rpc_total_for_status() {
        use crate::daemon::metrics::{RpcMethod, DAEMON_RPC_TOTAL};
        use crate::shared_chip::SharedChip;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_frame(&mut client, &Request::Status).unwrap();

        let before = DAEMON_RPC_TOTAL.at(RpcMethod::Status).get();
        let state = Arc::new(DaemonState::new(0, Arc::new(SharedChip::placeholder())));
        // handle_client returns when the dispatch finishes; for Status
        // that's near-instant since no chip access happens.
        handle_client(server, state);

        let after = DAEMON_RPC_TOTAL.at(RpcMethod::Status).get();
        assert!(
            after > before,
            "expected at least one bump (got before={} after={})",
            before,
            after
        );

        // Sanity: client should see a framed response (we don't care
        // which variant — Status payload, error, etc. — only that the
        // dispatch returned a valid frame).
        let resp: Response = read_frame(&mut client).expect("read response");
        match resp {
            Response::Status { .. } => {}
            other => panic!("expected Status response, got {:?}", other),
        }
    }

    /// Wiring test for the rpc_errors_total bump (sub of #29). An
    /// AddDisk against an empty slot fails with a "not running"
    /// reply_err. Both `rpc_total{method=add_disk}` and
    /// `rpc_errors_total{method=add_disk}` should bump. A successful
    /// dispatch should bump only `rpc_total`, not `rpc_errors_total`
    /// — the second half of this test asserts that on Status.
    #[test]
    fn handle_client_bumps_rpc_errors_on_failure() {
        use crate::daemon::metrics::{RpcMethod, DAEMON_RPC_ERRORS_TOTAL, DAEMON_RPC_TOTAL};
        use crate::shared_chip::SharedChip;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        // --- Failure path: AddDisk against empty slot ---
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_frame(
            &mut client,
            &Request::AddDisk {
                l2cpu: 0,
                path: "/tmp/nonexistent.img".into(),
            },
        )
        .unwrap();

        let total_before = DAEMON_RPC_TOTAL.at(RpcMethod::AddDisk).get();
        let err_before = DAEMON_RPC_ERRORS_TOTAL.at(RpcMethod::AddDisk).get();
        let state = Arc::new(DaemonState::new(0, Arc::new(SharedChip::placeholder())));
        handle_client(server, state);

        assert!(
            DAEMON_RPC_TOTAL.at(RpcMethod::AddDisk).get() > total_before,
            "rpc_total should still bump even on failure"
        );
        assert!(
            DAEMON_RPC_ERRORS_TOTAL.at(RpcMethod::AddDisk).get() > err_before,
            "rpc_errors_total should bump on reply_err"
        );

        let resp: Response = read_frame(&mut client).expect("read response");
        match resp {
            Response::Error { .. } => {}
            other => panic!("expected Error response, got {:?}", other),
        }

        // --- Success path: Status (always succeeds for an empty state). ---
        // rpc_errors_total{Status} must NOT move when the dispatch
        // returns OK. This is the half of the wiring that catches
        // a stuck-failed RPC_FAILED flag bleeding across requests.
        let (mut client2, server2) = UnixStream::pair().unwrap();
        client2
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_frame(&mut client2, &Request::Status).unwrap();

        let err_status_before = DAEMON_RPC_ERRORS_TOTAL.at(RpcMethod::Status).get();
        let state2 = Arc::new(DaemonState::new(0, Arc::new(SharedChip::placeholder())));
        handle_client(server2, state2);

        assert_eq!(
            DAEMON_RPC_ERRORS_TOTAL.at(RpcMethod::Status).get(),
            err_status_before,
            "rpc_errors_total{{Status}} must not bump on a successful dispatch"
        );

        let _: Response = read_frame(&mut client2).expect("read status response");
    }

    /// Same shape as the Status test, but for AddDisk — proves
    /// classify_request maps each Request variant to the right
    /// RpcMethod, not just Status.
    #[test]
    fn handle_client_bumps_rpc_total_for_add_disk() {
        use crate::daemon::metrics::{RpcMethod, DAEMON_RPC_TOTAL};
        use crate::shared_chip::SharedChip;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        // AddDisk against an empty slot — dispatch will reply with
        // an error ("l2cpu N is not running"), but the metrics bump
        // happens before that, on the way into the dispatch.
        write_frame(
            &mut client,
            &Request::AddDisk {
                l2cpu: 0,
                path: "/tmp/nonexistent.img".into(),
            },
        )
        .unwrap();

        let before = DAEMON_RPC_TOTAL.at(RpcMethod::AddDisk).get();
        let state = Arc::new(DaemonState::new(0, Arc::new(SharedChip::placeholder())));
        handle_client(server, state);

        let after = DAEMON_RPC_TOTAL.at(RpcMethod::AddDisk).get();
        assert!(after > before, "AddDisk counter didn't bump");

        // Drain the response so the test doesn't leak the socket.
        let _: Response = read_frame(&mut client).expect("read response");
    }

    #[test]
    fn boot_decides_proceed_when_slot_empty_no_force() {
        assert_eq!(decide_boot_slot(false, false, 0), BootSlotDecision::Proceed);
    }

    #[test]
    fn boot_decides_proceed_when_slot_empty_with_force() {
        // --force on an empty slot is a noop, not an error.
        assert_eq!(decide_boot_slot(false, true, 0), BootSlotDecision::Proceed);
    }

    #[test]
    fn boot_decides_reject_when_slot_full_no_force() {
        match decide_boot_slot(true, false, 2) {
            BootSlotDecision::Reject(msg) => {
                assert!(msg.contains("l2cpu 2"), "got: {}", msg);
                assert!(msg.contains("--force"), "got: {}", msg);
            }
            other => panic!("expected Reject, got {:?}", other),
        }
    }

    #[test]
    fn boot_decides_teardown_when_slot_full_with_force() {
        assert_eq!(
            decide_boot_slot(true, true, 1),
            BootSlotDecision::TearDownAndProceed
        );
    }

    // ---- validate_add_disk_request ----

    #[test]
    fn add_disk_rejects_when_disk_already_attached() {
        let tf = tempfile::NamedTempFile::new().unwrap();
        let err = validate_add_disk_request(false, tf.path()).unwrap_err();
        // SlotState variant for "already attached" — wire shape is bare.
        assert!(matches!(err, crate::Error::SlotState(_)));
        let msg = err.to_string();
        assert!(msg.contains("already attached"), "got: {}", msg);
    }

    #[test]
    fn add_disk_rejects_when_image_path_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("nonexistent.ext4");
        let err = validate_add_disk_request(true, &bogus).unwrap_err();
        // Io variant for IO failures — wire shape "cannot open disk image <p>: <io>".
        assert!(matches!(err, crate::Error::Io { .. }));
        let msg = err.to_string();
        assert!(msg.contains("cannot open disk image"), "got: {}", msg);
    }

    #[test]
    fn add_disk_accepts_valid_open_image_on_empty_slot() {
        let mut tf = tempfile::NamedTempFile::new().unwrap();
        // Disk image needs to be writable too (worker opens with read+write).
        tf.write_all(&[0u8; 4096]).unwrap();
        // Returns the open File on success — the caller hands the same
        // fd to the worker so a path-resolved-twice TOCTOU can't redirect
        // the daemon at a different inode after this point.
        let _file = validate_add_disk_request(true, tf.path())
            .expect("valid path on empty slot must accept");
    }

    // ---- validate_remove_disk_request ----

    #[test]
    fn remove_disk_rejects_when_no_disk_attached() {
        assert_eq!(validate_remove_disk_request(true), Err("no disk attached"));
    }

    #[test]
    fn remove_disk_accepts_when_disk_attached() {
        assert_eq!(validate_remove_disk_request(false), Ok(()));
    }

    // ---- probe_port_available ----

    #[cfg(feature = "slirp")]
    #[test]
    fn probe_port_available_succeeds_for_clear_port() {
        // Bind a transient listener to grab a free port, then drop it
        // so the port is provably bindable. Probe should succeed.
        use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
        let probe_listener =
            TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).unwrap();
        let port = probe_listener.local_addr().unwrap().port();
        drop(probe_listener);
        // Best-effort — a flaky kernel could re-assign the port between
        // drop and probe, but in practice this is reliable.
        probe_port_available(port).expect("just-released port should be bindable");
    }

    #[cfg(feature = "slirp")]
    #[test]
    fn probe_port_available_errors_when_port_in_use() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
        let occupant =
            TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).unwrap();
        let port = occupant.local_addr().unwrap().port();
        let err = probe_port_available(port).unwrap_err();
        // Linux returns EADDRINUSE; portable check: we just want some Err.
        assert!(
            err.kind() == std::io::ErrorKind::AddrInUse
                || err.raw_os_error() == Some(libc::EADDRINUSE)
        );
        drop(occupant);
    }
}
