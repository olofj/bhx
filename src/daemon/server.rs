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
use crate::regs::virtio_mmio::{
    CONSOLE_IRQ as CONSOLE_INT, CONSOLE_OFFSET as CONSOLE_MMIO, DISK_IRQ as DISK_INT,
    DISK_OFFSET as DISK_MMIO, RNG_IRQ as RNG_INT,
};
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
            console,
            rng,
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
            console,
            rng,
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
        Request::AddConsole { l2cpu } => dispatch_add_console(&sock, &state, l2cpu),
        Request::RemoveConsole { l2cpu } => dispatch_remove_console(&sock, &state, l2cpu),
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
        Request::AddConsole { .. } => RpcMethod::AddConsole,
        Request::RemoveConsole { .. } => RpcMethod::RemoveConsole,
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
    console: bool,
    rng: bool,
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
        "[boot l2cpu {}] dispatch_boot entry: opensbi={} payload={:?} dtb={} initramfs={:?} root={} force_reset_pcie={} disk={:?} network={} console={} rng={} force={}",
        l2cpu_idx, opensbi, payload, dtb, initramfs, root_device, force_reset_pcie, disk, network, console, rng, force
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
    let arts = run_boot_sequence(
        state,
        l2cpu_idx,
        opensbi,
        payload,
        dtb,
        initramfs,
        root_device,
        force_reset_pcie,
        disk.is_some(),
        network,
        console,
        rng,
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

    let mut slot = make_slot_from_l2cpu(arts.l2cpu, l2cpu_idx).map_err(|e| {
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

    // Resolve each migrated device's MMIO backing and stash its
    // buffer so it outlives the worker. Chip-DRAM is the historical
    // path; host-buffer (#64) is selected when `run_boot_sequence`
    // allocated one.
    let rng_backing = match &arts.rng_buf {
        Some(buf) => crate::virtio::MmioBacking::Host {
            va: buf.as_ptr() as usize,
        },
        None => crate::virtio::MmioBacking::ChipDram {
            region_offset: crate::regs::virtio_mmio::RNG_OFFSET,
        },
    };
    let net_backing = match &arts.net_buf {
        Some(buf) => crate::virtio::MmioBacking::Host {
            va: buf.as_ptr() as usize,
        },
        None => crate::virtio::MmioBacking::ChipDram {
            region_offset: crate::regs::virtio_mmio::NET_OFFSET,
        },
    };
    slot.virtio_rng_buf = arts.rng_buf;
    slot.virtio_net_buf = arts.net_buf;

    start_initial_workers(
        &mut slot,
        state.card,
        l2cpu_idx,
        disk,
        network,
        console,
        rng,
        rng_backing,
        net_backing,
    )
    .map_err(crate::Error::slot_state)?;
    // Workers are now in Phase 1 polling for the DRIVER bit. Release the
    // L2CPU from reset so the kernel's virtio probe runs against a
    // daemon that's already watching MMIO. See `release_l2cpu_from_reset`.
    release_l2cpu_from_reset(state, l2cpu_idx, &slot.l2cpu);
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
#[allow(clippy::too_many_arguments)]
fn start_initial_workers(
    slot: &mut L2CpuSlot,
    card: u32,
    l2cpu_idx: u8,
    disk: Option<String>,
    network: bool,
    console: bool,
    rng: bool,
    rng_backing: crate::virtio::MmioBacking,
    net_backing: crate::virtio::MmioBacking,
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
        dlog!(
            "[boot l2cpu {}] spawning net worker (backing={:?})",
            l2cpu_idx,
            net_backing
        );
        start_net_worker(card, slot, net_backing).map_err(|e| {
            dlog!("[boot l2cpu {}] start_net_worker failed: {}", l2cpu_idx, e);
            format!("start net worker failed: {}", e)
        })?;
    }
    if console {
        dlog!("[boot l2cpu {}] spawning virtio-console worker", l2cpu_idx);
        start_console_worker(slot).map_err(|e| {
            dlog!(
                "[boot l2cpu {}] start_console_worker failed: {}",
                l2cpu_idx,
                e
            );
            format!("start console worker failed: {}", e)
        })?;
    }
    if rng {
        dlog!(
            "[boot l2cpu {}] spawning virtio-rng worker (backing={:?})",
            l2cpu_idx,
            rng_backing
        );
        start_rng_worker(slot, rng_backing).map_err(|e| {
            dlog!("[boot l2cpu {}] start_rng_worker failed: {}", l2cpu_idx, e);
            format!("start rng worker failed: {}", e)
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
            crate::virtio::MmioBacking::ChipDram {
                region_offset: DISK_MMIO,
            },
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
fn start_net_worker(
    card: u32,
    slot: &mut L2CpuSlot,
    mmio_backing: crate::virtio::MmioBacking,
) -> io::Result<()> {
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
        network::network_main(forwards, l2cpu, interrupt, NET_INT, mmio_backing, exit_thread);
    });
    slot.net = Some(WorkerHandle {
        exit,
        thread: Some(t),
        description: format!("net l2cpu {}", idx),
    });
    Ok(())
}

#[cfg(not(feature = "slirp"))]
fn start_net_worker(
    _card: u32,
    _slot: &mut L2CpuSlot,
    _mmio_backing: crate::virtio::MmioBacking,
) -> io::Result<()> {
    Err(io::Error::other("daemon built without the slirp feature"))
}

/// Spawn the virtio-console worker (#51). The worker drains the
/// kernel's TX queue into the console hub and fills RX descriptors
/// from the per-slot `input_buf`. Idempotent guard via the slot's
/// existing `virtio_console: Option<...>`: caller checks that.
fn start_console_worker(slot: &mut L2CpuSlot) -> io::Result<()> {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    let exit = Arc::new(AtomicBool::new(false));
    let l2cpu = slot.l2cpu.clone();
    let interrupt = slot.interrupt.clone();
    let hub = slot.console_hub.clone();
    let input_buf = Arc::new(Mutex::new(VecDeque::with_capacity(
        crate::virtio::console::RX_BUFFER_CAP,
    )));
    let input_buf_thread = input_buf.clone();
    let exit_thread = exit.clone();
    let idx = slot.idx;
    let t = thread::spawn(move || {
        crate::virtio::console::console_main(
            l2cpu,
            interrupt,
            CONSOLE_INT,
            crate::virtio::MmioBacking::ChipDram {
                region_offset: CONSOLE_MMIO,
            },
            hub,
            input_buf_thread,
            exit_thread,
        );
    });
    slot.virtio_console = Some(crate::daemon::VirtioConsoleSlot {
        worker: WorkerHandle {
            exit,
            thread: Some(t),
            description: format!("virtio-console l2cpu {}", idx),
        },
        input_buf,
    });
    Ok(())
}

/// Spawn the virtio-rng worker (#62 / #64). The worker fills any guest
/// write-only descriptor with kernel entropy. Required for the
/// AlmaLinux EFI shim's `EFI_RNG_PROTOCOL` on the U-Boot+GRUB+shim
/// chained-boot path; harmless extra entropy source on other paths.
/// Idempotent guard via the slot's `virtio_rng: Option<...>`: caller
/// checks that.
///
/// `mmio_backing` is the resolved control-plane location: chip DRAM for
/// the historical layout, host buffer for the #64 path. The caller
/// supplies whichever it set up in `run_boot_sequence`. For the host
/// path the underlying `HostDmaBuf` lives in `slot.virtio_rng_buf`,
/// outliving this worker thanks to slot shutdown ordering.
fn start_rng_worker(
    slot: &mut L2CpuSlot,
    mmio_backing: crate::virtio::MmioBacking,
) -> io::Result<()> {
    let exit = Arc::new(AtomicBool::new(false));
    let l2cpu = slot.l2cpu.clone();
    let interrupt = slot.interrupt.clone();
    let exit_thread = exit.clone();
    let idx = slot.idx;
    let t = thread::spawn(move || {
        crate::virtio::rng::rng_main(l2cpu, interrupt, RNG_INT, mmio_backing, exit_thread);
    });
    slot.virtio_rng = Some(WorkerHandle {
        exit,
        thread: Some(t),
        description: format!("virtio-rng l2cpu {}", idx),
    });
    Ok(())
}

/// Output of `run_boot_sequence`. The L2Cpu is what every later step in
/// dispatch_boot drives off of. The optional buffers are `Some` exactly
/// when this boot allocated a host-side MMIO buffer for that device
/// (see #64); each buffer's `noc_address` and `as_ptr()` were already
/// used during this sequence to program an x280 small TLB and patch
/// the DTB, but the daemon needs to keep the buffer alive for the
/// worker's lifetime. dispatch_boot stashes them in the matching
/// `slot.virtio_*_buf` field.
pub struct BootArtifacts {
    pub l2cpu: Arc<L2Cpu>,
    pub rng_buf: Option<crate::host_buf::HostDmaBuf>,
    pub net_buf: Option<crate::host_buf::HostDmaBuf>,
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
    has_disk: bool,
    has_network: bool,
    has_console: bool,
    has_rng: bool,
) -> io::Result<BootArtifacts> {
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
    // Build the list of virtio-mmio nodes we'll inject under /soc and
    // (if has_rng) optionally allocate the host-side RNG buffer +
    // program an x280 small TLB to bridge it. The buffer's `as_ptr()`
    // also gets pre-init'd here with the standard register window so a
    // fast-init guest sees a coherent device the instant the kernel
    // probes virtio after reset release. See #64 for the full chain.
    //
    // Order in this Vec becomes DTB-storage order, which becomes
    // kernel-probe order. We want it to match what the daemon used to
    // emit before #64 — host-RNG (if any) first, then any chip-DRAM
    // slots in ascending address order (RNG-slot < CONSOLE < NET <
    // DISK in chip DRAM, but RNG is now host-side so the chip-DRAM
    // sequence starts at CONSOLE). Probe ordering matters for the #61
    // multi-queue race — virtio_net, the only multi-queue device that
    // matters today, was already known to bind reliably when block
    // probes after net so we keep DISK last.
    let mut virtio_nodes: Vec<crate::boot::VirtioMmioNode> = Vec::with_capacity(4);

    // Allocate one DMA-coherent buffer + program one x280 small TLB
    // for `device_id`, pre-init the standard register window, and add
    // a DTB node pointing at the resulting x280 PA. Returns the
    // owning `HostDmaBuf` so the caller stashes it in the slot.
    let mut alloc_host_mmio = |buf_index: u8,
                           tlb_slot: usize,
                           device_id: u32,
                           irq: u32,
                           label: &str|
     -> io::Result<crate::host_buf::HostDmaBuf> {
        let buf = crate::host_buf::HostDmaBuf::allocate(l2cpu.fd(), 4096, buf_index)
            .map_err(|e| {
                dlog!(
                    "[run_boot l2cpu {}] HostDmaBuf::allocate for {} failed: {}",
                    l2cpu_idx,
                    label,
                    e
                );
                e
            })?;
        let x280_pa = crate::x280_tlb::program_small_tlb_unicast(
            &l2cpu,
            tlb_slot,
            crate::x280_tlb::PCIE_TILE_X,
            crate::x280_tlb::PCIE_TILE_Y,
            buf.noc_address,
        );
        dlog!(
            "[run_boot l2cpu {}] {} host buffer: noc={:#x} x280_pa={:#x} host_va={:p}",
            l2cpu_idx,
            label,
            buf.noc_address,
            x280_pa,
            buf.as_ptr()
        );
        pre_init_virtio_mmio_host(buf.as_ptr(), device_id);
        virtio_nodes.push(crate::boot::VirtioMmioNode {
            addr: x280_pa,
            size: 4096,
            irq,
        });
        Ok(buf)
    };

    let rng_buf: Option<crate::host_buf::HostDmaBuf> = if has_rng {
        Some(alloc_host_mmio(
            0x40,
            crate::x280_tlb::RNG_TLB_SLOT,
            crate::regs::virtio_mmio::VIRTIO_ID_ENTROPY,
            crate::regs::virtio_mmio::RNG_IRQ,
            "rng",
        )?)
    } else {
        None
    };

    let net_buf: Option<crate::host_buf::HostDmaBuf> = if has_network {
        Some(alloc_host_mmio(
            0x41,
            crate::x280_tlb::NET_TLB_SLOT,
            crate::regs::virtio_mmio::VIRTIO_ID_NET,
            crate::regs::virtio_mmio::NET_IRQ,
            "net",
        )?)
    } else {
        None
    };

    // Chip-DRAM virtio slots. Always emit nodes for the historical 4
    // chip-DRAM addresses: even with the host-buffer-backed RNG + NET,
    // the chip-DRAM slots stay in the DTB so `add-disk` / `add-net` /
    // `add-console` (which currently always populate the chip-DRAM
    // slot) keep working post-boot. Order matches the pre-#64 emission
    // (RNG-slot, CONSOLE, NET, DISK in ascending address) so the
    // kernel's probe order — which gates the multi-queue race in #61
    // — stays where it was.
    //
    // Note: with has_network=true we also emit the host-buffer net
    // node above. Linux sees two virtio_net candidates; the chip-DRAM
    // one shows up as `Wrong magic value` (we don't pre-init it) and
    // the host-buffer one is the one that actually binds. The
    // chip-DRAM-net `Wrong magic` warning is the same noise we
    // already had for unconfigured slots (#53). Same logic applies to
    // RNG.
    {
        use crate::regs::virtio_mmio::{
            CONSOLE_IRQ, CONSOLE_OFFSET, DISK_IRQ, DISK_OFFSET, MMIO_SLOT_SIZE, NET_IRQ,
            NET_OFFSET, RNG_IRQ, RNG_OFFSET,
        };
        virtio_nodes.push(crate::boot::VirtioMmioNode {
            addr: starting_address + memory_size - RNG_OFFSET,
            size: MMIO_SLOT_SIZE,
            irq: RNG_IRQ,
        });
        virtio_nodes.push(crate::boot::VirtioMmioNode {
            addr: starting_address + memory_size - CONSOLE_OFFSET,
            size: MMIO_SLOT_SIZE,
            irq: CONSOLE_IRQ,
        });
        virtio_nodes.push(crate::boot::VirtioMmioNode {
            addr: starting_address + memory_size - NET_OFFSET,
            size: MMIO_SLOT_SIZE,
            irq: NET_IRQ,
        });
        virtio_nodes.push(crate::boot::VirtioMmioNode {
            addr: starting_address + memory_size - DISK_OFFSET,
            size: MMIO_SLOT_SIZE,
            irq: DISK_IRQ,
        });
    }

    dlog!(
        "[run_boot l2cpu {}] patching DTB (memory start=0x{:x} size=0x{:x}, {} virtio nodes)",
        l2cpu_idx,
        starting_address,
        memory_size,
        virtio_nodes.len()
    );
    let dtb_patched = boot::modify_dtb(
        &dtb_raw,
        &boot_device,
        starting_address,
        memory_size,
        &virtio_nodes,
    )?;

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

    // Pre-write virtio MMIO magic + version + device-id for slots that
    // will get a worker. Without this, fast-init guests like U-Boot
    // probe MMIO sub-millisecond after reset release and read 0 — the
    // workers spawn ~50-200ms later and would write the right values
    // then, but by that time U-Boot has already silently bound zero
    // virtio_blk children. Linux is tolerant because its virtio probe
    // sits behind init+initramfs latency. See #46.
    //
    // The actual workers re-write these bytes on cold-start (the path
    // when no stash is present) and the writes are idempotent, so
    // there's no double-init bug. Doing the pre-write from the daemon
    // main thread also avoids reordering the worker spawn relative to
    // reset release, which would invasively touch the slot lifecycle.
    if has_disk {
        pre_init_virtio_mmio(
            &l2cpu,
            starting_address + memory_size - crate::regs::virtio_mmio::DISK_OFFSET,
            crate::regs::virtio_mmio::VIRTIO_ID_BLOCK,
        );
    }
    if has_console {
        pre_init_virtio_mmio(
            &l2cpu,
            starting_address + memory_size - crate::regs::virtio_mmio::CONSOLE_OFFSET,
            crate::regs::virtio_mmio::VIRTIO_ID_CONSOLE,
        );
    }
    // Note: virtio-rng and virtio-net pre-inits happen further up,
    // next to their host-buffer allocations — those writes go to the
    // daemon-local mmap (the chip reads them via PCIe outbound iATU +
    // x280 TLB, not via NoC writes from the daemon).

    dlog!(
        "[run_boot l2cpu {}] image+pre_init done; deferring reset release until workers spawn",
        l2cpu_idx
    );
    Ok(BootArtifacts {
        l2cpu,
        rng_buf,
        net_buf,
    })
}

/// Release the L2CPU from reset and configure its prefetchers. Called
/// AFTER virtio worker threads are spawned so the kernel's first virtio
/// probe doesn't race ahead of the daemon's Phase 1/2/3 handshake. The
/// previous arrangement had reset release inside `run_boot_sequence`,
/// which fired before workers were even constructed — kernel reached
/// `vm_setup_vq` for queue 1 of multi-queue devices (net, console)
/// while no daemon thread was clearing `QUEUE_READY` from queue 0's
/// setup, so the kernel saw `READY=1` and bailed with `-ENOENT`. With
/// workers already in Phase 3 polling when this returns, the
/// guest-side `writel(READY=1)` → next-queue `readl(READY)` race is
/// reliably won.
fn release_l2cpu_from_reset(state: &Arc<DaemonState>, l2cpu_idx: u8, l2cpu: &L2Cpu) {
    dlog!("[run_boot l2cpu {}] releasing from reset", l2cpu_idx);
    // reset_x280 goes through SharedChip so the PLL step and the
    // L2CPU_RESET R-M-W serialize against any other boot RPC issuing the
    // same sequence.
    state.shared_chip.reset_x280(&[l2cpu_idx as usize]);
    dlog!("[run_boot l2cpu {}] configuring prefetchers", l2cpu_idx);
    boot::configure_prefetchers(l2cpu);
    dlog!("[run_boot l2cpu {}] run_boot_sequence done", l2cpu_idx);
}

/// Pre-write the standard virtio-mmio header registers (magic, version,
/// device-id, plus the registers a fast-init guest reads right after
/// the device-id check) for one MMIO slot. Called from
/// `run_boot_sequence` after boot_l2cpu but before reset release, gated
/// on whether a worker for that slot will be spawned in
/// `start_initial_workers`.
///
/// Writes everything the guest can read between reset release and the
/// daemon worker's cold-start completion:
/// - **Magic / version / device-id**: the device-presence handshake;
///   pre_init was originally added in #46 to keep U-Boot's sub-µs
///   probe from seeing zeros.
/// - **`device_features` (high half)**: stock virtio-mmio guests
///   write `_sel = 1` then read `_features` immediately, with no
///   spin gate. The daemon worker doesn't run cold-start until
///   ~tens-of-ms after reset release; if the guest gets to feature
///   negotiation in that window (short paths exist via early
///   initramfs) the read returns zero and the kernel rejects the
///   device with "must provide VIRTIO_F_VERSION_1 feature!". We
///   pre-publish the high-half features (which carry
///   `VIRTIO_F_VERSION_1`) so that initial read is always coherent.
///   The worker re-writes this with the real value (still
///   `features[1]` for all our devices today) during cold-start.
/// - **`queue_num_max`**: the kernel's per-queue setup expects a
///   non-zero max; same race as features.
///
/// `device_features_high` is hardcoded to the `VIRTIO_F_VERSION_1` bit
/// (bit 32 of the 64-bit feature space, = bit 0 of the high u32) — the
/// only feature any of our devices currently advertise. If a device
/// ever advertises additional high-half features, plumb them through
/// here too.
fn pre_init_virtio_mmio(l2cpu: &L2Cpu, mmio_addr: u64, device_id: u32) {
    const VIRTIO_MAGIC: u32 = 0x74726976; // 'v'|'i'<<8|'r'<<16|'t'<<24
                                          // Standard virtio-mmio register offsets (see virtio 1.2 § 4.2.2).
    const OFF_MAGIC: u64 = 0x000;
    const OFF_VERSION: u64 = 0x004;
    const OFF_DEVICE_ID: u64 = 0x008;
    const OFF_DEVICE_FEATURES: u64 = 0x010;
    const OFF_QUEUE_NUM_MAX: u64 = 0x034;
    const OFF_SW_IMPL: u64 = 0x018;
    let queue_size_pre: u32 = crate::virtio::QUEUE_SIZE as u32;
    const VIRTIO_F_VERSION_1_HIGH: u32 = 1; // bit 32 of the 64-bit space

    dlog!(
        "[pre_init_virtio l2cpu {}] mmio_addr=0x{:x} device_id={} features_high={} qnum_max={}",
        l2cpu.idx(),
        mmio_addr,
        device_id,
        VIRTIO_F_VERSION_1_HIGH,
        queue_size_pre
    );
    // Zero the standard register window [0x00, 0x200) BEFORE writing
    // the device-presence values. The worker's cold-start used to do
    // this zeroing itself, but it ran AFTER reset release — direct-
    // kernel boots race into virtio probe ~50ms after release while
    // the worker hasn't yet spawned, so the zeroing took 1+ ms of
    // byte-at-a-time PCIe writes during which the kernel saw garbage.
    // Doing it here, before reset release, means by the time the
    // kernel can probe, MMIO is in a coherent state with our pre_init
    // values present. The worker's cold-start path now relies on
    // this and just re-writes idempotent values.
    for off in (0..0x200u64).step_by(4) {
        l2cpu.write32(mmio_addr + off, 0);
    }
    l2cpu.write32(mmio_addr + OFF_MAGIC, VIRTIO_MAGIC);
    l2cpu.write32(mmio_addr + OFF_VERSION, 2); // virtio 1.0+ MMIO
    l2cpu.write32(mmio_addr + OFF_DEVICE_ID, device_id);
    // device_features at offset 0x10 is sel-driven (sel=0 → low,
    // sel=1 → high). After cold-start zeroing _sel = 0; but Linux
    // writes _sel = 1 first. We pre-publish the high half here so
    // the first stock-guest read sees VIRTIO_F_VERSION_1 even if the
    // worker hasn't started its cold-start yet.
    l2cpu.write32(mmio_addr + OFF_DEVICE_FEATURES, VIRTIO_F_VERSION_1_HIGH);
    l2cpu.write32(mmio_addr + OFF_QUEUE_NUM_MAX, queue_size_pre);
    // sw_impl=1 lets our patched virtio-mmio driver enable the
    // sel_generation handshake; a stock kernel just ignores this
    // register. The patched kernel rejects the device with
    // "SW_IMPL value must be 0 or 1" if it reads garbage here, so we
    // must write 1 *before* reset release rather than waiting for
    // the worker's cold-start.
    l2cpu.write32(mmio_addr + OFF_SW_IMPL, 1);
    // Read back features to verify the write took effect.
    let features_back = l2cpu.read32(mmio_addr + OFF_DEVICE_FEATURES);
    let qnum_back = l2cpu.read32(mmio_addr + OFF_QUEUE_NUM_MAX);
    dlog!(
        "[pre_init_virtio l2cpu {}] readback: features=0x{:x} qnum_max=0x{:x}",
        l2cpu.idx(),
        features_back,
        qnum_back
    );
}

/// Host-side counterpart to [`pre_init_virtio_mmio`] for #64. Same
/// register-window initialization, but writes go to a daemon-local
/// mmap'd address (which the chip later reads through PCIe outbound
/// iATU + an x280 TLB) instead of through the daemon's TLB onto the
/// L2CPU tile. Native u32 stores at the mmap'd VA (no PCIe round-trip
/// from the daemon's side); the kernel sees them as uncached MMIO
/// reads.
fn pre_init_virtio_mmio_host(host_va: *mut u8, device_id: u32) {
    use std::ptr;

    const VIRTIO_MAGIC: u32 = 0x74726976; // 'virt'
    const OFF_MAGIC: usize = 0x000;
    const OFF_VERSION: usize = 0x004;
    const OFF_DEVICE_ID: usize = 0x008;
    const OFF_DEVICE_FEATURES: usize = 0x010;
    const OFF_QUEUE_NUM_MAX: usize = 0x034;
    const OFF_SW_IMPL: usize = 0x018;
    const VIRTIO_F_VERSION_1_HIGH: u32 = 1;
    let queue_size_pre: u32 = crate::virtio::QUEUE_SIZE as u32;

    // Zero the standard register window [0x00, 0x200), then write the
    // initial device-presence values. Same sequencing as the chip-DRAM
    // pre_init: the worker's cold-start re-writes these idempotently.
    unsafe {
        for off in (0..0x200usize).step_by(4) {
            ptr::write_volatile(host_va.add(off) as *mut u32, 0);
        }
        ptr::write_volatile(host_va.add(OFF_MAGIC) as *mut u32, VIRTIO_MAGIC);
        ptr::write_volatile(host_va.add(OFF_VERSION) as *mut u32, 2);
        ptr::write_volatile(host_va.add(OFF_DEVICE_ID) as *mut u32, device_id);
        ptr::write_volatile(
            host_va.add(OFF_DEVICE_FEATURES) as *mut u32,
            VIRTIO_F_VERSION_1_HIGH,
        );
        ptr::write_volatile(host_va.add(OFF_QUEUE_NUM_MAX) as *mut u32, queue_size_pre);
        ptr::write_volatile(host_va.add(OFF_SW_IMPL) as *mut u32, 1);
    }
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
        virtio_net_buf: None,
        virtio_console: None,
        virtio_rng: None,
        virtio_rng_buf: None,
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
    // before doing any IO so other client handlers don't stall. The
    // virtio-console input_buf is `Some` only when an operator opted in
    // with `boot --virtio-console` or `add-console`; when it's present
    // we fan keystrokes into both the chip UART (`input_tx`) and the
    // virtio-console (`vc_input`) so whichever HVC the kernel ended up
    // using as its console picks them up.
    let (hub, input_tx, vc_input) = {
        let slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
        let slot = slot_guard.as_ref().ok_or_else(|| {
            crate::Error::slot_state(format!("l2cpu {} is not booted", l2cpu_idx))
        })?;
        (
            slot.console_hub.clone(),
            slot.console_input_tx.clone(),
            slot.virtio_console.as_ref().map(|vc| vc.input_buf.clone()),
        )
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

    thread::spawn(move || client_reader_main(daemon_read, res.id, hub, input_tx, vc_input));
    Ok(())
}

/// Blocking write of scrollback bytes. Loops only on EINTR; returns any
/// other error to the caller (who will detach the client).
fn write_scrollback(mut sock: &UnixStream, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    sock.write_all(bytes)
}

/// Per-client reader: blocks on `sock`, forwards bytes whenever this client
/// is the writer. Bytes go to **both** the chip-side OpenSBI debug UART
/// (`input_tx`) and, if attached, the virtio-console RX queue
/// (`vc_input`). Whichever HVC the kernel chose as its console absorbs
/// them; the other side just sits idle. Terminates on EOF or hub-driven
/// drop.
///
/// `vc_input` is bounded at `RX_BUFFER_CAP` (16 KiB). On overflow we
/// drop the oldest byte in the deque rather than blocking — the chip
/// path is the primary console for diagnostic kernels, and a bursty
/// paste shouldn't wedge the reader.
fn client_reader_main(
    sock: UnixStream,
    id: u64,
    hub: Arc<ConsoleHub>,
    input_tx: mpsc::Sender<u8>,
    vc_input: Option<Arc<std::sync::Mutex<std::collections::VecDeque<u8>>>>,
) {
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
                    if let Some(vc) = vc_input.as_ref() {
                        let mut g = vc.lock().unwrap();
                        for &b in &buf[..n] {
                            if g.len() >= crate::virtio::console::RX_BUFFER_CAP {
                                g.pop_front();
                            }
                            g.push_back(b);
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
            crate::virtio::MmioBacking::ChipDram {
                region_offset: DISK_MMIO,
            },
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
            crate::virtio::MmioBacking::ChipDram {
                region_offset: NET_MMIO,
            },
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
// AddConsole / RemoveConsole (virtio-console, #51)
// ---------------------------------------------------------------------------

fn dispatch_add_console(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
) -> crate::Result<()> {
    dlog!("[add_console l2cpu {}] dispatch entry", l2cpu_idx);
    validate_l2cpu(l2cpu_idx)?;
    let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
    let slot = slot_guard
        .as_mut()
        .ok_or_else(|| crate::Error::slot_state(format!("l2cpu {} is not booted", l2cpu_idx)))?;
    if slot.virtio_console.is_some() {
        return Err(crate::Error::slot_state("virtio-console already attached"));
    }
    start_console_worker(slot).map_err(crate::Error::io_ctx("start virtio-console worker"))?;
    dlog!(
        "[add_console l2cpu {}] dispatch complete — replying ok",
        l2cpu_idx
    );
    reply_ok(sock);
    Ok(())
}

fn dispatch_remove_console(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
) -> crate::Result<()> {
    dlog!("[remove_console l2cpu {}] dispatch entry", l2cpu_idx);
    validate_l2cpu(l2cpu_idx)?;
    // Take the slot under the lock, join outside.
    let vc = {
        let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
        let slot = slot_guard.as_mut().ok_or_else(|| {
            crate::Error::slot_state(format!("l2cpu {} is not booted", l2cpu_idx))
        })?;
        slot.virtio_console
            .take()
            .ok_or_else(|| crate::Error::slot_state("no virtio-console attached"))?
    };
    dlog!("[remove_console l2cpu {}] joining worker", l2cpu_idx);
    vc.worker.stop_and_join();
    dlog!("[remove_console l2cpu {}] done — replying ok", l2cpu_idx);
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
