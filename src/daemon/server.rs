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
use crate::daemon::{DaemonState, DiskWorker, L2CpuSlot, LockExt, WorkerHandle};
use crate::dlog;
use crate::l2cpu::L2Cpu;
use crate::virtio::interrupt::InterruptController;

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
    spawn_guest_poweroff_handler(Arc::clone(&state));
    spawn_pll_watcher(Arc::clone(&state));

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
            return Err(crate::Error::Io {
                ctx: format!("metrics exporter bind on 127.0.0.1:{}", port),
                source: e,
            }
            .into());
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
    for (i, slot_mutex) in state.l2cpus.iter().enumerate() {
        if let Some(slot) = slot_mutex.lock_or_internal_error()?.take() {
            slot.console_hub
                .disconnect_all_with_reason("daemon shutting down");
            unregister_engine_slots(&state, i as u8);
            slot.shutdown();
        }
    }
    // All slots drained — step the L2CPU PLL down before the daemon
    // releases its SharedChip. Next daemon's reset_x280 brings it
    // back up unconditionally (#95).
    state.maybe_idle_pll();
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
/// is a single read to ARC tile (8,0), no state change.
fn probe_initial_chip_state(shared: &crate::shared_chip::SharedChip, card: u32) -> Vec<u8> {
    let val = match shared.read_l2cpu_reset() {
        Ok(v) => v,
        Err(e) => {
            dlog!(
                "[probe] L2CPU_RESET read failed: {}; assuming all cores cold",
                e
            );
            return Vec::new();
        }
    };
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
    // Engine path: if any L2CPU is live across the daemon restart,
    // the BRISC firmware on the engine tile is also live (same chip
    // lifetime as the L2CPUs). Adopt it before probing per-L2CPU so
    // a future cold-boot RPC's `get_or_bring_up_tensix_engine` finds
    // the engine already up and skips `bring_up`'s halt+reload (which
    // would tear out the running guests' MMIO backend).
    if !released.is_empty() {
        match state.adopt_running_tensix_engine() {
            Ok(()) => dlog!("[warm-resume] adopted running tensix engine"),
            Err(e) => dlog!(
                "[warm-resume] adopt_running_tensix_engine failed: {} \
                 (chip likely lost firmware; next cold boot will reload)",
                e
            ),
        }
    }
    for &idx in released {
        dlog!("[warm-resume l2cpu {}] probing chip state", idx);
        let l2cpu = match L2Cpu::new(idx as usize, &state.shared_chip) {
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
                // Daemon startup; if a slot mutex is already poisoned
                // here something has gone very wrong — fail loudly via
                // unwrap rather than silently dropping the warm-resume.
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
            disk,
            network,
            extra_fwd,
            console,
            rng,
            force,
            memory_override,
            hostname_override,
            cloud_init,
        } => dispatch_boot(
            &sock,
            &state,
            l2cpu,
            &opensbi,
            &payload,
            &dtb,
            initramfs.as_deref(),
            &root_device,
            disk,
            network,
            extra_fwd,
            console,
            rng,
            force,
            memory_override,
            hostname_override,
            cloud_init,
        ),
        Request::AttachConsole { l2cpu, mode } => {
            dispatch_attach_console(&sock, &state, l2cpu, mode)
        }
        Request::AddDisk { l2cpu, path, name } => {
            dispatch_add_disk(&sock, &state, l2cpu, path, name)
        }
        Request::RemoveDisk { l2cpu, name } => dispatch_remove_disk(&sock, &state, l2cpu, name),
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
/// Drives `bhx_daemon_rpc_total{method}`. Per-method failures live
/// on `bhx_daemon_rpc_errors_total` and are tracked via the
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
/// to bump `bhx_daemon_rpc_errors_total{method}` on the way out.
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
/// Maximum disks per L2CPU. Three blk slots: DEV_BLK (rootfs by
/// convention), DEV_BLK1 (cloud-init seed if --cloud-init was set,
/// else first add-disk), DEV_BLK2 (additional data disk).
pub const MAX_DISKS_PER_L2CPU: usize = 3;

fn validate_add_disk_request(
    current_count: usize,
    name: Option<&str>,
    existing_names: impl IntoIterator<Item = Option<String>>,
    path: &std::path::Path,
) -> crate::Result<std::fs::File> {
    if current_count >= MAX_DISKS_PER_L2CPU {
        return Err(crate::Error::slot_state(format!(
            "L2CPU already has {} disks attached (max {})",
            current_count, MAX_DISKS_PER_L2CPU
        )));
    }
    // Reject duplicate names — guest udev would otherwise see two
    // virtio-blk devices with the same /dev/disk/by-id/virtio-* link
    // and the operator's selector logic in remove-disk would be
    // ambiguous.
    if let Some(n) = name {
        for existing in existing_names {
            if existing.as_deref() == Some(n) {
                return Err(crate::Error::slot_state(format!(
                    "L2CPU already has a disk with name {:?}",
                    n
                )));
            }
        }
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

/// Pick the first free `DEV_BLK*` slot index for a new attach, given
/// the slots already in use. Returns `None` if all three are taken.
/// Tied to `MAX_DISKS_PER_L2CPU`; bumping that requires extending the
/// list here in lockstep.
fn pick_free_blk_slot(used: &[u32]) -> Option<u32> {
    use crate::virtio_engine::{DEV_BLK, DEV_BLK1, DEV_BLK2};
    [DEV_BLK, DEV_BLK1, DEV_BLK2]
        .into_iter()
        .find(|s| !used.contains(s))
}

fn irq_for_blk_dev_idx(dev_idx: u32) -> u32 {
    use crate::regs::virtio_mmio::{DISK1_IRQ, DISK2_IRQ, DISK_IRQ};
    use crate::virtio_engine::{DEV_BLK, DEV_BLK1, DEV_BLK2};
    match dev_idx {
        x if x == DEV_BLK => DISK_IRQ,
        x if x == DEV_BLK1 => DISK1_IRQ,
        x if x == DEV_BLK2 => DISK2_IRQ,
        _ => panic!(
            "irq_for_blk_dev_idx called with non-blk dev_idx {}",
            dev_idx
        ),
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

fn dispatch_status(mut sock: &UnixStream, state: &Arc<DaemonState>) -> crate::Result<()> {
    let mut l2cpus = Vec::new();
    for (idx, slot_mutex) in state.l2cpus.iter().enumerate() {
        let slot = slot_mutex.lock_or_internal_error()?;
        #[allow(clippy::type_complexity)]
        let (
            st,
            disk,
            disks,
            net,
            virtio_console,
            clients,
            purgatory_status,
            purgatory_peers,
            release_meta,
        ) = match slot.as_ref() {
            None => {
                let st = if state.wedged[idx].load(Ordering::Relaxed) {
                    L2CpuState::Wedged
                } else {
                    L2CpuState::Stopped
                };
                (st, None, Vec::new(), false, false, 0, None, None, None)
            }
            Some(s) => {
                // (#166 Phase 1/2/4a) Read the OpenSBI purgatory status
                // block at L2CPU memory base + 0xE0000. Layout in
                // src/regs.rs::purgatory.
                let mem_base = crate::l2cpu::L2CPU_STARTING_ADDRESS[idx];
                let read_u64 = |pa: u64| -> Option<u64> {
                    let lo = s.l2cpu.read32(pa).ok()?;
                    let hi = s.l2cpu.read32(pa + 4).ok()?;
                    Some(((hi as u64) << 32) | (lo as u64))
                };
                let purg = read_u64(mem_base + crate::regs::purgatory::STATUS_OFFSET);
                let peers = read_u64(mem_base + crate::regs::purgatory::PEERS_OFFSET);
                // (#166 Phase 3) When the magic reads PARKED, surface
                // it as a distinct slot state. The L2Cpu is still
                // alive on the daemon side; release-from-purgatory
                // (Phase 4) re-images and IPIs hart 0 to wake parked
                // harts. Also pull the release metadata when PARKED so
                // the operator can sanity-check the published PAs.
                let parked = purg == Some(crate::regs::purgatory::STATUS_PARKED);
                let st = if parked {
                    L2CpuState::Parked
                } else {
                    L2CpuState::Running
                };
                // Phase 4a metadata is populated by final_exit (only
                // valid when PARKED). Phase 5 metadata is populated at
                // cold init (valid for any state). Read both; surface
                // unconditionally so an operator can sanity-check the
                // force-park PAs on a Running guest before issuing
                // `bhx boot --force`.
                let force_park_request_pa =
                    read_u64(mem_base + crate::regs::purgatory::FORCE_PARK_REQ_PA_OFFSET);
                let force_park_request_value =
                    read_u64(mem_base + crate::regs::purgatory::FORCE_PARK_REQ_VALUE_OFFSET);
                let msip_pa = read_u64(mem_base + crate::regs::purgatory::MSIP_PA_OFFSET);
                let release = if parked {
                    let next_addr_pa =
                        read_u64(mem_base + crate::regs::purgatory::NEXT_ADDR_PA_OFFSET);
                    let next_mode_pa =
                        read_u64(mem_base + crate::regs::purgatory::NEXT_MODE_PA_OFFSET);
                    let next_arg1_pa =
                        read_u64(mem_base + crate::regs::purgatory::NEXT_ARG1_PA_OFFSET);
                    let hsm_state_pa =
                        read_u64(mem_base + crate::regs::purgatory::HSM_STATE_PA_OFFSET);
                    match (
                        next_addr_pa,
                        next_mode_pa,
                        next_arg1_pa,
                        hsm_state_pa,
                        msip_pa,
                    ) {
                        (Some(a), Some(b), Some(c), Some(d), Some(e)) => {
                            Some(crate::daemon::protocol::PurgatoryReleaseMeta {
                                next_addr_pa: a,
                                next_mode_pa: b,
                                next_arg1_pa: c,
                                hsm_state_pa: d,
                                msip_pa: e,
                                force_park_request_pa: force_park_request_pa.filter(|&v| v > 0),
                                force_park_request_value: force_park_request_value
                                    .filter(|&v| v > 0),
                            })
                        }
                        _ => None,
                    }
                } else if force_park_request_pa.unwrap_or(0) > 0 {
                    // Running slot — surface just the Phase 5 fields by
                    // setting the Phase 4a fields to 0 (operator
                    // shouldn't act on them while not parked).
                    Some(crate::daemon::protocol::PurgatoryReleaseMeta {
                        next_addr_pa: 0,
                        next_mode_pa: 0,
                        next_arg1_pa: 0,
                        hsm_state_pa: 0,
                        msip_pa: msip_pa.unwrap_or(0),
                        force_park_request_pa,
                        force_park_request_value,
                    })
                } else {
                    None
                };
                (
                    st,
                    s.disks.first().map(|d| d.path.clone()),
                    s.disks
                        .iter()
                        .map(|d| crate::daemon::protocol::DiskAttach {
                            path: d.path.clone(),
                            name: d.name.clone(),
                        })
                        .collect(),
                    s.net.is_some(),
                    s.virtio_console.is_some(),
                    s.console_hub.client_count() as u32,
                    purg,
                    peers,
                    release,
                )
            }
        };
        l2cpus.push(L2CpuStatus {
            idx: idx as u8,
            state: st,
            disk,
            disks,
            net,
            virtio_console,
            clients,
            purgatory_status,
            purgatory_peers,
            purgatory_release_meta: release_meta,
        });
    }

    let engine_tile = state
        .tensix_engine
        .lock_or_internal_error()?
        .as_ref()
        .map(|e| (e.noc0_x, e.noc0_y));
    // Read L2CPU PLL state via SharedChip ARC window. PLL4 lives at
    // 0x80020500; CNTL1 (offset 0x4) packs fbdiv (high 16) + postdiv
    // (byte 1) + refdiv (byte 0). For operator readability the CLI
    // decodes (fbdiv, postdiv0) into a frequency label.
    const PLL4_BASE: u64 = 0x80020500;
    const PLL_CNTL_1_OFF: u64 = 0x4;
    const PLL_CNTL_5_OFF: u64 = 0x14;
    let (pll_fbdiv, pll_postdiv0) = match (
        state.shared_chip.arc_read32(PLL4_BASE + PLL_CNTL_1_OFF),
        state.shared_chip.arc_read32(PLL4_BASE + PLL_CNTL_5_OFF),
    ) {
        (Ok(cntl1), Ok(cntl5)) => {
            let fbdiv = (cntl1 >> 16) as u16;
            let postdiv0 = (cntl5 & 0xFF) as u8;
            (Some(fbdiv), Some(postdiv0))
        }
        _ => (None, None),
    };
    let payload = StatusPayload {
        pid: std::process::id(),
        uptime_secs: state.started.elapsed().as_secs(),
        l2cpus,
        engine_tile,
        pll_fbdiv,
        pll_postdiv0,
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
    disk: Option<String>,
    network: bool,
    extra_fwd: Vec<(u16, u16)>,
    console: bool,
    rng: bool,
    force: bool,
    memory_override: Option<u64>,
    hostname_override: Option<String>,
    cloud_init: Option<String>,
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
        "[boot l2cpu {}] dispatch_boot entry: opensbi={} payload={:?} dtb={} initramfs={:?} root={} disk={:?} network={} console={} rng={} force={} mem_override={:?} hostname_override={:?} cloud_init={:?}",
        l2cpu_idx, opensbi, payload, dtb, initramfs, root_device, disk, network, console, rng, force, memory_override, hostname_override, cloud_init
    );
    validate_l2cpu(l2cpu_idx)?;

    // (#166 Phase 4b) Detect a Parked slot and route to the
    // release-from-purgatory path instead of cold boot. Skips
    // chip-side reset entirely; the parked OpenSBI in M-mode is
    // already running and just needs hart 0 woken from
    // sbi_hsm_hart_wait.
    if let Some(meta) = read_parked_release_meta(state, l2cpu_idx)? {
        dlog!(
            "[boot l2cpu {}] Parked slot detected — routing to release-from-purgatory",
            l2cpu_idx
        );
        return dispatch_release(state, sock, l2cpu_idx, payload, meta);
    }

    // (#166 Phase 5) `bhx boot --force` on a Running slot, when the
    // slot has the force-park IPI metadata available, fires a M-mode
    // IPI to drive the running guest into purgatory — same parked
    // state a guest-issued SBI SRST would produce. Then routes through
    // the standard release-from-purgatory path. Recovers any
    // kernel-level wedge (S-mode `sstatus.SIE=0` cannot mask M-mode
    // IPIs). The fallback for "metadata not published" (i.e., OpenSBI
    // wasn't built with the force-park hook, or the IPI event slot
    // ran out) is the existing handle_existing_slot --force path,
    // which tears down the slot + does a cold-boot.
    if force {
        if let Some(meta) = trigger_force_park_if_available(state, l2cpu_idx)? {
            dlog!(
                "[boot l2cpu {}] --force: fired force-park IPI, awaiting PARKED",
                l2cpu_idx
            );
            return dispatch_release(state, sock, l2cpu_idx, payload, meta);
        }
        dlog!(
            "[boot l2cpu {}] --force: force-park unavailable; falling back to cold-boot teardown path",
            l2cpu_idx
        );
    }

    handle_existing_slot(state, l2cpu_idx, force).map_err(crate::Error::slot_state)?;

    // (#166 Phase 6 / #168) Opportunistic chip-wide quiesce: if no
    // sibling slot is Running, fold a `reset_board` into the cold-boot
    // path. Parked siblings auto-transition to Stopped. Silent — the
    // operator just sees a fresh chip + their target booted. When any
    // sibling IS Running, we skip the reset (it would SIGBUS that
    // sibling's worker) and proceed straight to cold-boot.
    maybe_opportunistic_reset_board(state, l2cpu_idx, state.card)?;

    // Cold-boot: clear any captured shutdown scrollback (#160) so the
    // next slot's tail isn't conflated with the previous one's. Done
    // here rather than at run_boot_sequence's tail because the
    // existing-slot teardown above (when `force` is set) populates the
    // tail via internal_stop, and we want to throw it away — the
    // operator opted into `force` because they want a fresh slot.
    if let Ok(mut g) = state.shutdown_tails[l2cpu_idx as usize].lock() {
        g.clear();
    }

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
        disk.is_some(),
        network,
        console,
        rng,
        cloud_init.is_some(),
        memory_override,
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

    // Register virtio device handlers with the engine's kick poller.
    // When the guest writes QUEUE_NOTIFY for a registered slot, the
    // poller looks up the entry, walks the descriptor chain via
    // `process_one_chain_for_queue`, and fires the PLIC IRQ.
    {
        let engine_for_init = state.tensix_engine.lock_or_internal_error()?.clone();
        if let (Some(poller), Some(engine)) = (
            state.kick_poller.lock_or_internal_error()?.as_ref(),
            engine_for_init,
        ) {
            // Helper closure: register one device + initialize its
            // device-specific config space directly into BRISC L1
            // via the engine's L1 pointer. The existing
            // `VirtioDeviceImpl::init_config` writes (e.g.)
            // virtio-blk's capacity at offset 0 of the CONFIG
            // region; running it once at registration matches what
            // `virtio::run_device` did at Phase 3 in the legacy
            // host-buffer path.
            let dev_slot = |dev_idx: u32| -> u32 {
                (l2cpu_idx as u32) * crate::virtio_engine::DEVS_PER_L2CPU + dev_idx
            };
            let register = |slot_idx: u32,
                            device: Box<dyn crate::virtio::VirtioDeviceImpl + Send>,
                            irq: u32,
                            kind: crate::virtio::InterruptKind,
                            label: &str| {
                let config_addr = crate::virtio_engine::slot_regs_base(slot_idx)
                    + crate::virtio_engine::MMIO_CONFIG;
                let config_ptr = engine.l1_ptr(config_addr);
                device.init_config(config_ptr);
                let entry = crate::tensix_data_plane::RegEntry::new(
                    slot_idx,
                    Arc::clone(&slot.l2cpu),
                    device,
                    Arc::clone(&slot.interrupt),
                    irq,
                    kind,
                );
                poller.register_slot(entry);
                dlog!(
                    "[run_boot l2cpu {}] virtio-engine: registered {} on slot {} \
                         (config @ {:#x})",
                    l2cpu_idx,
                    label,
                    slot_idx,
                    config_addr
                );
            };
            if rng {
                register(
                    dev_slot(crate::virtio_engine::DEV_RNG),
                    Box::<crate::virtio::rng::VirtioRng>::default(),
                    crate::regs::virtio_mmio::RNG_IRQ,
                    crate::virtio::InterruptKind::Rng,
                    "rng",
                );
            }
            // Helper: open the disk image, build a VirtioBlk with the
            // operator-supplied serial, register it with the kick
            // poller at `dev_idx`, and append a stub DiskWorker so
            // `daemon status` reports the attachment. Logs and
            // swallows errors at this site (a failed cloud-init
            // attach shouldn't block the rest of the boot).
            let mut attach_disk = |disk_path: &str,
                                   serial: Option<String>,
                                   dev_idx: u32,
                                   irq: u32,
                                   label: &'static str| {
                let file = match std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(disk_path)
                {
                    Ok(f) => f,
                    Err(e) => {
                        dlog!(
                            "[run_boot l2cpu {}] virtio-engine: open {} for {} failed: {}",
                            l2cpu_idx,
                            disk_path,
                            label,
                            e
                        );
                        return;
                    }
                };
                let blk = match crate::virtio::block::VirtioBlk::from_file_with_serial(
                    file,
                    l2cpu_idx,
                    serial.clone(),
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        dlog!(
                            "[run_boot l2cpu {}] virtio-engine: VirtioBlk::from_file({}) for {} \
                                 failed: {}",
                            l2cpu_idx,
                            disk_path,
                            label,
                            e
                        );
                        return;
                    }
                };
                let slot_idx = dev_slot(dev_idx);
                register(
                    slot_idx,
                    Box::new(blk),
                    irq,
                    crate::virtio::InterruptKind::Block,
                    label,
                );
                slot.disks.push(DiskWorker {
                    path: disk_path.to_string(),
                    slot_idx,
                    name: serial,
                    worker: WorkerHandle {
                        exit: Arc::new(AtomicBool::new(false)),
                        thread: None,
                        description: format!(
                            "{} l2cpu {} @ {} (engine)",
                            label, l2cpu_idx, disk_path
                        ),
                    },
                });
            };
            if let Some(disk_path) = disk.as_ref() {
                attach_disk(
                    disk_path,
                    None,
                    crate::virtio_engine::DEV_BLK,
                    crate::regs::virtio_mmio::DISK_IRQ,
                    "blk",
                );
            }
            if let Some(seed_path) = cloud_init.as_ref() {
                // cloud-init's NoCloud datasource probes for a virtio
                // device with serial="cidata"; pin it to DEV_BLK1 so
                // the second virtio-blk index is stable across boots.
                attach_disk(
                    seed_path,
                    Some("cidata".to_string()),
                    crate::virtio_engine::DEV_BLK1,
                    crate::regs::virtio_mmio::DISK1_IRQ,
                    "cidata",
                );
            }
            dlog!(
                "[run_boot l2cpu {}] virtio-engine: net check (network={}, slirp_compiled={})",
                l2cpu_idx,
                network,
                cfg!(feature = "slirp")
            );
            #[cfg(feature = "slirp")]
            if network {
                let ssh_port = crate::regs::slirp::ssh_port(state.card, l2cpu_idx);
                let mut forwards = vec![(ssh_port, 22u16)];
                forwards.extend(extra_fwd.iter().copied());
                match crate::virtio::network::VirtioNet::new(
                    &forwards,
                    state.card,
                    l2cpu_idx,
                    hostname_override.as_deref(),
                ) {
                    Ok(net) => {
                        register(
                            dev_slot(crate::virtio_engine::DEV_NET),
                            Box::new(net),
                            crate::regs::virtio_mmio::NET_IRQ,
                            crate::virtio::InterruptKind::Net,
                            "net",
                        );
                        // Stub net WorkerHandle so `daemon status` shows
                        // `net=true`. Kick poller owns dispatch (TX) +
                        // its own RX poll; no thread to join.
                        slot.net = Some(WorkerHandle {
                            exit: Arc::new(AtomicBool::new(false)),
                            thread: None,
                            description: format!("net l2cpu {} (engine)", l2cpu_idx),
                        });
                    }
                    Err(e) => dlog!(
                        "[run_boot l2cpu {}] virtio-engine: VirtioNet::new failed: {}",
                        l2cpu_idx,
                        e
                    ),
                }
            }
            #[cfg(not(feature = "slirp"))]
            let _ = extra_fwd;
            if console {
                // virtio-console wants two halves: the device side
                // (registered with the kick poller for TX/RX) and a
                // VirtioConsoleSlot on the per-L2CPU slot so the
                // attach-console RPC + the input-fanout in
                // dispatch_attach_console can find the input_buf.
                // Same shape as `start_virtio_console` for the legacy
                // host-buffer path; the only difference is we skip
                // spawning a worker (the kick poller handles dispatch).
                let input_buf = Arc::new(std::sync::Mutex::new(
                    std::collections::VecDeque::with_capacity(
                        crate::virtio::console::RX_BUFFER_CAP,
                    ),
                ));
                let device = crate::virtio::console::VirtioConsole::new(
                    slot.console_hub.clone(),
                    Arc::clone(&input_buf),
                );
                register(
                    dev_slot(crate::virtio_engine::DEV_CONSOLE),
                    Box::new(device),
                    crate::regs::virtio_mmio::CONSOLE_IRQ,
                    crate::virtio::InterruptKind::Console,
                    "console",
                );
                slot.virtio_console = Some(crate::daemon::VirtioConsoleSlot {
                    // Stub WorkerHandle — the engine path doesn't
                    // spawn a per-device worker; the kick poller
                    // handles dispatch. Keeping the slot field
                    // populated lets the rest of the daemon (attach,
                    // shutdown, status) treat the engine path
                    // identically to the legacy host-buffer path.
                    worker: WorkerHandle {
                        exit: Arc::new(AtomicBool::new(false)),
                        thread: None,
                        description: format!("virtio-console l2cpu {} (engine)", l2cpu_idx),
                    },
                    input_buf,
                });
            }
            // M6 (#78) 16550 UART: register the L2CPU's console_hub so
            // BRISC's TX kicks (slot 16+l2cpu_idx) route the byte
            // through `push_chip_output`. Always-on with the engine —
            // the DTB node is also unconditionally emitted, so distro
            // kernels with `console=ttyS0` find a real backing device.
            poller.register_uart(l2cpu_idx, Arc::clone(&slot.console_hub));
            dlog!(
                "[run_boot l2cpu {}] uart-engine: registered TX path on slot {}",
                l2cpu_idx,
                crate::uart_engine::slot_for_l2cpu(l2cpu_idx),
            );
            // #94: arm the per-L2CPU shutdown slot so BRISC starts
            // polling the syscon-poweroff register. Cleared on slot
            // teardown by `unregister_engine_slots`.
            poller.register_shutdown(l2cpu_idx);
            dlog!(
                "[run_boot l2cpu {}] shutdown slot: registered (slot {})",
                l2cpu_idx,
                crate::regs::shutdown::SLOT_BASE + l2cpu_idx as u32,
            );
        } else {
            dlog!(
                "[run_boot l2cpu {}] virtio-engine: engine + kick poller not up — \
                 skipping device registration",
                l2cpu_idx
            );
        }
    }
    // Workers are now in Phase 1 polling for the DRIVER bit. Release the
    // L2CPU from reset so the kernel's virtio probe runs against a
    // daemon that's already watching MMIO. See `release_l2cpu_from_reset`.
    release_l2cpu_from_reset(state, l2cpu_idx, &slot.l2cpu)?;

    install_slot_and_reply_ok(state, l2cpu_idx, slot, sock)?;
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
        let mut guard = state.l2cpus[l2cpu_idx as usize]
            .lock_or_internal_error()
            .map_err(|e| e.to_string())?;
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
        // Order is load-bearing: console_hub disconnect first (so
        // attached `bhx connect` clients see a goodbye rather than EOF
        // on a dead fd), engine unregister next (so no kick can
        // dispatch into the slot we're about to tear down), then
        // slot.shutdown() joins workers and drops the L2Cpu Arc. A
        // reorder that put shutdown() before the unregister would
        // leave a one-iteration window where the kick poller could
        // dereference a freed Arc<L2Cpu>. See CLAUDE.md's "Boot must
        // not race with live workers during board reset" invariant.
        prior
            .console_hub
            .disconnect_all_with_reason(&format!("l2cpu {} re-imaged via --force", l2cpu_idx));
        unregister_engine_slots(state, l2cpu_idx);
        prior.shutdown();
        dlog!("[boot l2cpu {}] prior slot torn down", l2cpu_idx);
    }
    Ok(())
}

/// Classification of a non-target sibling L2CPU's purgatory cell, used
/// by the Phase 6 opportunistic reset_board path. PARKED magic ⇒ slot
/// is in OpenSBI's `sbi_hsm_hart_wait` and we can drop it cleanly. Any
/// other value (including read failures) is treated as Running — we
/// must NOT reset the board because reset_board would invalidate that
/// slot's worker mmaps and SIGBUS the daemon.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SiblingClassification {
    Parked,
    Running,
}

pub(crate) fn classify_sibling(purg: Option<u64>) -> SiblingClassification {
    if purg == Some(crate::regs::purgatory::STATUS_PARKED) {
        SiblingClassification::Parked
    } else {
        SiblingClassification::Running
    }
}

/// (#166 Phase 6 / #168) Opportunistic `reset_board` folded into the
/// cold-boot path. Silent / automatic. Goes through when:
///
///   - the target slot itself is `None` after `handle_existing_slot`
///     (a true cold boot, not a release-from-purgatory or force-park
///     flow that already returned earlier);
///   - no NON-target slot is `Running` — we need a clean sweep, but
///     a running guest's worker would SIGBUS the daemon when the PCIe
///     LDS reset invalidates its mmap.
///
/// Effect on parked siblings: their `L2Cpu` instances are dropped and
/// the slots transition to `None` (Stopped). The operator sees them
/// as cold-bootable again. The reset is logged loudly so this is
/// visible without `--verbose` flags.
///
/// When skipped (because a sibling is Running), the cold boot
/// proceeds as today — no reset, no kernel-side state guarantees
/// beyond what the previous boot left behind. The operator is in
/// charge of explicitly stopping the running sibling first if they
/// want a fully-clean baseline.
fn maybe_opportunistic_reset_board(
    state: &Arc<DaemonState>,
    target_idx: u8,
    card: u32,
) -> crate::Result<()> {
    // Snapshot non-target slot states without holding all four mutexes
    // simultaneously: identify Running siblings and Parked siblings in
    // separate passes. Running detection: slot is Some AND its
    // purgatory cell does NOT read PARKED. Parked detection: slot is
    // Some AND purgatory cell == PARKED magic.
    let mut running_siblings = Vec::new();
    let mut parked_siblings = Vec::new();
    for i in 0..4u8 {
        if i == target_idx {
            continue;
        }
        let guard = state.l2cpus[i as usize].lock_or_internal_error()?;
        let Some(slot) = guard.as_ref() else {
            continue;
        };
        let mem_base = crate::l2cpu::L2CPU_STARTING_ADDRESS[i as usize];
        let purg_pa = mem_base + crate::regs::purgatory::STATUS_OFFSET;
        let purg = match (slot.l2cpu.read32(purg_pa), slot.l2cpu.read32(purg_pa + 4)) {
            (Ok(lo), Ok(hi)) => Some(((hi as u64) << 32) | (lo as u64)),
            _ => None,
        };
        match classify_sibling(purg) {
            SiblingClassification::Parked => parked_siblings.push(i),
            SiblingClassification::Running => running_siblings.push(i),
        }
    }

    if !running_siblings.is_empty() {
        dlog!(
            "[boot l2cpu {}] opportunistic reset_board skipped — sibling l2cpu {:?} still Running",
            target_idx,
            running_siblings
        );
        return Ok(());
    }

    dlog!(
        "[boot l2cpu {}] opportunistic reset_board: no Running siblings; parked siblings to drop: {:?}",
        target_idx,
        parked_siblings
    );

    // Drop parked-sibling L2Cpu instances + transition slots to None.
    // Console-hub clients (if any) get the goodbye; engine slots
    // unregister so the kick poller stops dispatching to them.
    for &i in &parked_siblings {
        if let Ok(true) = internal_stop(state, i, "opportunistic reset_board") {
            dlog!(
                "[boot l2cpu {}] opportunistic reset: l2cpu {} transitioned Parked → Stopped",
                target_idx,
                i
            );
        }
    }

    // Drop the engine + kick poller before the PCIe LDS reset. Their
    // fds point at the about-to-be-reset chardev; holding them across
    // the reset would leave them pointing at unmapped BARs.
    let _ = state.kick_poller.lock_or_internal_error()?.take();
    let _ = state.tensix_engine.lock_or_internal_error()?.take();

    // Hold the boot_lock across reset_board so a concurrent boot RPC
    // (different thread, different L2CPU) doesn't race with the reset.
    let _boot_guard = state
        .boot_lock
        .lock()
        .map_err(|_| crate::Error::internal("daemon boot_lock poisoned"))?;
    state
        .shared_chip
        .reset_board(card)
        .map_err(|e| crate::Error::Io {
            ctx: "opportunistic reset_board".into(),
            source: e,
        })?;
    dlog!(
        "[boot l2cpu {}] opportunistic reset_board complete",
        target_idx
    );
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
) -> crate::Result<()> {
    *state.l2cpus[l2cpu_idx as usize].lock_or_internal_error()? = Some(slot);
    state.wedged[l2cpu_idx as usize].store(false, Ordering::Relaxed);
    crate::daemon::metrics::L2CPU_BOOT_COLD_TOTAL
        .at(l2cpu_idx)
        .inc();
    dlog!(
        "[boot l2cpu {}] dispatch_boot complete — replying ok",
        l2cpu_idx
    );
    reply_ok(sock);
    Ok(())
}

/// (#166 Phase 4b) Release-from-purgatory metadata, captured under
/// the slot mutex along with the L2Cpu Arc. Returned by
/// `read_parked_release_meta` only when the slot's purgatory cell
/// reads as PARKED with all five published PAs valid (>0).
pub struct ParkedReleaseMeta {
    /// Arc to the slot's L2Cpu so the caller can issue NoC writes
    /// without holding the slot mutex across the long re-image.
    pub l2cpu: Arc<L2Cpu>,
    /// PA of `&hart0_scratch->next_addr`. Host writes the new kernel
    /// entry here.
    pub next_addr_pa: u64,
    /// PA of `&hart0_scratch->next_mode`. Host writes PRV_S = 1.
    pub next_mode_pa: u64,
    /// PA of `&hart0_scratch->next_arg1`. Host writes the DTB PA.
    pub next_arg1_pa: u64,
    /// PA of hart 0's HSM `state` atomic. Host writes
    /// SBI_HSM_STATE_START_PENDING (= 2).
    pub hsm_state_pa: u64,
    /// PA of CLINT MSIP[0]. Host writes 1 to fire the M-mode
    /// software interrupt that wakes hart 0 from `wfi`.
    pub msip_pa: u64,
}

/// (#166 Phase 4b) If the slot is alive AND its OpenSBI purgatory
/// cell reads as PARKED with valid release metadata, return the
/// metadata so the caller can drive a release-from-purgatory.
/// Returns Ok(None) if the slot is None or not parked.
fn read_parked_release_meta(
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
) -> crate::Result<Option<ParkedReleaseMeta>> {
    let guard = state.l2cpus[l2cpu_idx as usize].lock_or_internal_error()?;
    let Some(slot) = guard.as_ref() else {
        return Ok(None);
    };
    let mem_base = crate::l2cpu::L2CPU_STARTING_ADDRESS[l2cpu_idx as usize];
    let read_u64 = |pa: u64| -> Option<u64> {
        let lo = slot.l2cpu.read32(pa).ok()?;
        let hi = slot.l2cpu.read32(pa + 4).ok()?;
        Some(((hi as u64) << 32) | (lo as u64))
    };
    let status = read_u64(mem_base + crate::regs::purgatory::STATUS_OFFSET);
    if status != Some(crate::regs::purgatory::STATUS_PARKED) {
        return Ok(None);
    }
    let next_addr_pa = read_u64(mem_base + crate::regs::purgatory::NEXT_ADDR_PA_OFFSET);
    let next_mode_pa = read_u64(mem_base + crate::regs::purgatory::NEXT_MODE_PA_OFFSET);
    let next_arg1_pa = read_u64(mem_base + crate::regs::purgatory::NEXT_ARG1_PA_OFFSET);
    let hsm_state_pa = read_u64(mem_base + crate::regs::purgatory::HSM_STATE_PA_OFFSET);
    let msip_pa = read_u64(mem_base + crate::regs::purgatory::MSIP_PA_OFFSET);
    let (next_addr_pa, next_mode_pa, next_arg1_pa, hsm_state_pa, msip_pa) = match (
        next_addr_pa,
        next_mode_pa,
        next_arg1_pa,
        hsm_state_pa,
        msip_pa,
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e))
            if a > 0 && b > 0 && c > 0 && d > 0 && e > 0 =>
        {
            (a, b, c, d, e)
        }
        _ => {
            dlog!(
                    "[release l2cpu {}] PARKED magic seen but metadata fields are zero/missing — skipping release",
                    l2cpu_idx
                );
            return Ok(None);
        }
    };
    Ok(Some(ParkedReleaseMeta {
        l2cpu: Arc::clone(&slot.l2cpu),
        next_addr_pa,
        next_mode_pa,
        next_arg1_pa,
        hsm_state_pa,
        msip_pa,
    }))
}

/// (#166 Phase 5) On `bhx boot --force` of a Running slot, fire the
/// M-mode IPI to drive the running guest into purgatory. Returns
/// `Ok(Some(meta))` once the slot reads PARKED so the caller can
/// drive a release-from-purgatory; `Ok(None)` if the metadata isn't
/// published (e.g. OpenSBI built without Phase 5 support, or IPI
/// event slot ran out) so the caller can fall back to the cold-boot
/// teardown path.
///
/// Sequence:
///   1. Read FORCE_PARK_REQ_PA + FORCE_PARK_REQ_VALUE + MSIP_PA from
///      the published metadata block. All three must be non-zero.
///   2. Write the value to the request PA. This sets the
///      `bhx_force_park` event-pending bit in hart 0's
///      `sbi_ipi_data->ipi_type`. No concurrent IPI sender exists
///      (the kernel can't issue M-mode IPIs), so a direct u64 write
///      is equivalent to atomic-OR.
///   3. Write 1 to MSIP_PA (CLINT MSIP[0]).
///   4. Poll the slot's purgatory cell for PARKED magic, up to
///      `force_park_timeout`. The hart traps into OpenSBI's IPI
///      dispatcher → `bhx_force_park_process` → `sbi_system_reset`
///      → final_exit (peer-convergence, scratch metadata) → warmboot.
///   5. Once PARKED, return the same release metadata
///      `read_parked_release_meta` would have returned.
const FORCE_PARK_TIMEOUT_SECS: u64 = 10;
fn trigger_force_park_if_available(
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
) -> crate::Result<Option<ParkedReleaseMeta>> {
    let mem_base = crate::l2cpu::L2CPU_STARTING_ADDRESS[l2cpu_idx as usize];

    // Step 1: read the metadata under the slot lock.
    let l2cpu_arc = {
        let guard = state.l2cpus[l2cpu_idx as usize].lock_or_internal_error()?;
        let Some(slot) = guard.as_ref() else {
            return Ok(None);
        };
        let read_u64 = |pa: u64| -> Option<u64> {
            let lo = slot.l2cpu.read32(pa).ok()?;
            let hi = slot.l2cpu.read32(pa + 4).ok()?;
            Some(((hi as u64) << 32) | (lo as u64))
        };
        let req_pa = read_u64(mem_base + crate::regs::purgatory::FORCE_PARK_REQ_PA_OFFSET);
        let req_val = read_u64(mem_base + crate::regs::purgatory::FORCE_PARK_REQ_VALUE_OFFSET);
        let msip_pa = read_u64(mem_base + crate::regs::purgatory::MSIP_PA_OFFSET);
        match (req_pa, req_val, msip_pa) {
            (Some(rpa), Some(rval), Some(mpa)) if rpa > 0 && rval > 0 && mpa > 0 => {
                // Step 2+3: fire the IPI.
                let l2cpu = &slot.l2cpu;
                l2cpu
                    .write32(rpa, rval as u32)
                    .and_then(|_| l2cpu.write32(rpa + 4, (rval >> 32) as u32))
                    .map_err(|e| crate::Error::Io {
                        ctx: "write force-park request bit".into(),
                        source: e,
                    })?;
                l2cpu.write32(mpa, 1).map_err(|e| crate::Error::Io {
                    ctx: "write CLINT MSIP for force-park".into(),
                    source: e,
                })?;
                dlog!(
                    "[force-park l2cpu {}] fired: req_pa={:#x} req_val={:#x} msip={:#x}",
                    l2cpu_idx,
                    rpa,
                    rval,
                    mpa
                );
                Arc::clone(&slot.l2cpu)
            }
            _ => {
                dlog!(
                    "[force-park l2cpu {}] metadata not published (req_pa={:?} req_val={:?} msip={:?}); falling back",
                    l2cpu_idx,
                    req_pa,
                    req_val,
                    msip_pa
                );
                return Ok(None);
            }
        }
    };

    // Step 4: poll for PARKED magic.
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(FORCE_PARK_TIMEOUT_SECS);
    let purg_pa = mem_base + crate::regs::purgatory::STATUS_OFFSET;
    loop {
        if let (Ok(lo), Ok(hi)) = (l2cpu_arc.read32(purg_pa), l2cpu_arc.read32(purg_pa + 4)) {
            let v = ((hi as u64) << 32) | (lo as u64);
            if v == crate::regs::purgatory::STATUS_PARKED {
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(crate::Error::internal(format!(
                "force-park l2cpu {}: PARKED magic not seen within {}s after firing IPI",
                l2cpu_idx, FORCE_PARK_TIMEOUT_SECS
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Step 5: re-read the now-populated release metadata under the
    // slot lock and return it. (Same code path as a slot that
    // PARKED via a guest-issued SBI SRST.)
    read_parked_release_meta(state, l2cpu_idx)
}

/// (#166 Phase 4b) Drive the host-handshake release of a parked
/// L2CPU. Re-writes the kernel image (DTB and OpenSBI keep their
/// cold-boot bytes), then writes hart 0's scratch+state and fires
/// the wake IPI. Hart 0 exits sbi_hsm_hart_wait, mainline runs
/// init_warmboot_run → sbi_hart_switch_mode → mret to S-mode at
/// next_addr. The new kernel SBI-HSM-starts harts 1..3 itself.
fn dispatch_release(
    state: &Arc<DaemonState>,
    sock: &UnixStream,
    l2cpu_idx: u8,
    payload: &crate::daemon::protocol::BootPayload,
    meta: ParkedReleaseMeta,
) -> crate::Result<()> {
    use crate::regs::boot_image;
    let mem_base = crate::l2cpu::L2CPU_STARTING_ADDRESS[l2cpu_idx as usize];
    let kernel_addr = mem_base + boot_image::KERNEL_OFFSET;
    let dtb_addr = mem_base + boot_image::DTB_OFFSET;
    let rootfs_addr = mem_base + boot_image::INITRAMFS_OFFSET;

    dlog!(
        "[release l2cpu {}] re-writing kernel from {} at {:#x}",
        l2cpu_idx,
        payload.path(),
        kernel_addr
    );
    let kernel_path = std::path::Path::new(payload.path());
    let kernel_bytes = crate::boot::read_bin_file(kernel_path).map_err(|e| crate::Error::Io {
        ctx: format!("read kernel {}", payload.path()),
        source: e,
    })?;
    crate::boot::l2cpu_noc_write_bulk_pub(&meta.l2cpu, kernel_addr, &kernel_bytes).map_err(
        |e| crate::Error::Io {
            ctx: "kernel re-image".into(),
            source: e,
        },
    )?;

    // Clear the PARKED magic before kicking hart 0 so a status read
    // racing the wake doesn't see stale Parked state. The new boot
    // doesn't write to this cell unless the guest issues another SRST.
    let purg_pa = mem_base + crate::regs::purgatory::STATUS_OFFSET;
    meta.l2cpu
        .write32(purg_pa, 0)
        .and_then(|_| meta.l2cpu.write32(purg_pa + 4, 0))
        .map_err(|e| crate::Error::Io {
            ctx: "clear purgatory magic".into(),
            source: e,
        })?;

    // Phase 4b core: write hart 0's release metadata, flip HSM state,
    // fire the IPI. Order matters — next_addr/mode/arg1 must be
    // visible before the parked hart wakes and reads them, so
    // ordering is: meta writes → fence → state write → fence → MSIP.
    let write_u64 = |pa: u64, val: u64| -> crate::Result<()> {
        meta.l2cpu
            .write32(pa, val as u32)
            .and_then(|_| meta.l2cpu.write32(pa + 4, (val >> 32) as u32))
            .map_err(|e| crate::Error::Io {
                ctx: format!("write u64 @ {:#x}", pa),
                source: e,
            })
    };
    write_u64(meta.next_addr_pa, kernel_addr)?;
    write_u64(meta.next_mode_pa, crate::regs::purgatory::NEXT_MODE_S)?;
    write_u64(meta.next_arg1_pa, dtb_addr)?;

    // HSM state is a 32-bit atomic_t (long on RV32 / int on the
    // OpenSBI generic platform after we expanded sbi_atomic.h to use
    // `int counter`). Read it first as a sanity check (must be
    // STOPPED = 1) so we fail loudly on a races-with-cold-boot
    // misconfiguration before firing the IPI.
    let hsm_now = meta
        .l2cpu
        .read32(meta.hsm_state_pa)
        .map_err(|e| crate::Error::Io {
            ctx: "read hsm state".into(),
            source: e,
        })?;
    if hsm_now != crate::regs::purgatory::HSM_STATE_STOPPED {
        return Err(crate::Error::internal(format!(
            "release l2cpu {}: hart 0 HSM state {} (expected STOPPED=1) — refusing to release",
            l2cpu_idx, hsm_now
        )));
    }
    meta.l2cpu
        .write32(
            meta.hsm_state_pa,
            crate::regs::purgatory::HSM_STATE_START_PENDING,
        )
        .map_err(|e| crate::Error::Io {
            ctx: "write hsm state START_PENDING".into(),
            source: e,
        })?;

    // Fire the IPI. CLINT MSIP is a 32-bit register; writing 1 sets
    // the M-mode software interrupt pending bit, waking hart 0 from
    // wfi inside sbi_hsm_hart_wait.
    meta.l2cpu
        .write32(meta.msip_pa, 1)
        .map_err(|e| crate::Error::Io {
            ctx: "write CLINT MSIP".into(),
            source: e,
        })?;

    dlog!(
        "[release l2cpu {}] hart 0 released — next_addr={:#x} dtb={:#x}; awaiting kernel",
        l2cpu_idx,
        kernel_addr,
        dtb_addr
    );

    // Mark the slot fresh: the new kernel won't write to the
    // shutdown_tail buffer, but stale tail content from the
    // previous incarnation would leak into a post-mortem attach if
    // we left it. Clear here so the next stop captures only this
    // boot's output.
    if let Ok(mut g) = state.shutdown_tails[l2cpu_idx as usize].lock() {
        g.clear();
    }

    // No need to re-spawn workers or update slot bookkeeping: the
    // existing console worker keeps draining the virt UART, the
    // virtio handlers stay registered with the kick poller, the
    // slot's L2Cpu Arc lives on. Status will flip from Parked back
    // to Running on the next status query (we cleared the magic).
    let _ = (rootfs_addr,); // unused on this path until DTB rebuild lands
    let _ = state; // state used for shutdown_tails above
    reply_ok(sock);
    Ok(())
}

/// Output of `run_boot_sequence`. The L2Cpu is what every later step in
/// dispatch_boot drives off of.
pub struct BootArtifacts {
    pub l2cpu: Arc<L2Cpu>,
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
    has_disk: bool,
    has_network: bool,
    has_console: bool,
    has_rng: bool,
    has_cidata: bool,
    memory_override: Option<u64>,
) -> io::Result<BootArtifacts> {
    use crate::regs::boot_image;

    let starting_address = crate::l2cpu::L2CPU_STARTING_ADDRESS[l2cpu_idx as usize];
    let physical_size = crate::l2cpu::L2CPU_MEMORY_SIZE[l2cpu_idx as usize];
    // #91: clamp the operator's override to the L2CPU's physical DRAM
    // size so a too-large value can't drive the guest into MMIO space.
    // Round down to a 2 MiB page boundary so the reservation math at
    // mem_end stays aligned.
    let memory_size = match memory_override {
        Some(req) => {
            let aligned = req & !((2 * 1024 * 1024) - 1);
            let clamped = aligned.min(physical_size);
            if clamped != req {
                dlog!(
                    "[run_boot l2cpu {}] memory override {:#x} clamped to {:#x} (physical {:#x})",
                    l2cpu_idx,
                    req,
                    clamped,
                    physical_size
                );
            }
            clamped
        }
        None => physical_size,
    };

    let opensbi_addr = starting_address + boot_image::OPENSBI_OFFSET;
    let kernel_addr = starting_address + boot_image::KERNEL_OFFSET;
    let dtb_addr = starting_address + boot_image::DTB_OFFSET;
    let rootfs_addr = starting_address + boot_image::INITRAMFS_OFFSET;

    // Phase 6 (#168) moved the opportunistic reset_board into
    // `maybe_opportunistic_reset_board` called from dispatch_boot
    // BEFORE this function — the precondition needs daemon-level slot
    // state visibility, and Phases 4b/5 may take alternative paths
    // (release-from-purgatory, force-park) that bypass cold-boot
    // entirely. Once we're here, this is a true cold boot: build the
    // L2Cpu, write the image, release from reset.

    // Construct the runtime L2Cpu handle BEFORE the image load so the load
    // can go through its persistent fd + `get_persistent_2m_window` UC
    // path (no shared tile-(8,0) aliasing). Returned from this function so
    // the caller hands the same Arc to `make_slot_from_l2cpu` — one
    // construction, one PLL step, no double-init.
    dlog!(
        "[run_boot l2cpu {}] constructing L2Cpu (ioctls + 8GB VA + TLB windows)",
        l2cpu_idx
    );
    let l2cpu = Arc::new(L2Cpu::new(l2cpu_idx as usize, &state.shared_chip)?);

    dlog!("[run_boot l2cpu {}] reading DTB from {}", l2cpu_idx, dtb);
    let dtb_raw = boot::read_bin_file(Path::new(dtb))?;
    // U-Boot manages root + initrd at runtime. Skip the daemon's
    // initramfs preload + leave bootargs to U-Boot (no `root=/dev/vda`
    // injection).
    let boot_device = if payload.is_uboot() {
        boot::BootDevice::Uboot
    } else {
        match (initramfs, has_disk) {
            (Some(p), true) => {
                // Distro-style boot: kernel image + dracut initramfs +
                // real disk. The initramfs is loaded at rootfs_addr,
                // dracut runs and needs `root=/dev/<dev>` to know what
                // to pivot_root onto — without it, switch_root drops
                // to emergency shell.
                let bytes = boot::read_bin_file(Path::new(p))?;
                boot::BootDevice::InitramfsAndVda {
                    addr: rootfs_addr,
                    len: bytes.len() as u64,
                    dev: root_device.to_string(),
                }
            }
            (Some(p), false) => {
                let bytes = boot::read_bin_file(Path::new(p))?;
                boot::BootDevice::Initramfs {
                    addr: rootfs_addr,
                    len: bytes.len() as u64,
                }
            }
            (None, _) => boot::BootDevice::Vda(root_device.to_string()),
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
    let mut virtio_nodes: Vec<crate::boot::VirtioMmioNode> = Vec::with_capacity(8);
    let uart_addr_for_dtb: Option<u64> = None;
    // #94 guest-OS shutdown: hoisted out of the engine-bring-up branch
    // because it's needed at modify_dtb call time. Set inside the
    // bring-up block when x280_base is computed; stays None when the
    // boot doesn't bring up the engine at all (rare — initramfs-only,
    // no virtio devices).
    let mut x280_base_for_shutdown: Option<u64> = None;

    let any_host_device = has_rng || has_network || has_disk || has_console || has_cidata;

    // The L2CPU's virtio-mmio reg windows point at the tensix engine
    // tile's L1. The engine is brought up lazily —
    // first feature-on boot triggers the firmware load + handshake,
    // subsequent boots reuse the same Arc<TensixEngine>.
    //
    // The L2CPU's virtio-mmio reg windows point at the tensix engine
    // tile's L1. Engine bring-up is lazy (first-boot triggers
    // firmware load + handshake; subsequent boots reuse the same
    // engine) — same Tensix tile serves all 4 L2CPUs on the chip.
    if any_host_device {
        let engine = state.get_or_bring_up_tensix_engine().map_err(|e| {
            dlog!(
                "[run_boot l2cpu {}] tensix engine bring-up failed: {}",
                l2cpu_idx,
                e
            );
            e
        })?;
        let x280_base = engine.program_l2cpu_tlb(&l2cpu, l2cpu_idx as u32)?;
        x280_base_for_shutdown = Some(x280_base);
        dlog!(
            "[run_boot l2cpu {}] L2CPU TLB → tensix tile NOC0 ({}, {}) translated \
             ({}, {}); per-L2CPU window x280_base={:#x}",
            l2cpu_idx,
            engine.noc0_x,
            engine.noc0_y,
            engine.translated_x,
            engine.translated_y,
            x280_base
        );
        // Per-device DTB nodes: each device's reg file sits at
        // `x280_base + dev_idx * REGS_PER_DEV`, where dev_idx follows
        // the BRISC firmware's BRISC_VIRTIO_DEV_* ordering (blk=0,
        // net=1, console=2, rng=3). MMIO size = REGS_PER_DEV (4 KiB)
        // per virtio 1.2 §4.2.2.
        let regs_per_dev = crate::virtio_engine::REGS_PER_DEV as u64;
        let mut emit = |dev_idx: u32, enabled: bool, irq: u32, label: &str| {
            if !enabled {
                return;
            }
            let pa = x280_base + (dev_idx as u64) * regs_per_dev;
            virtio_nodes.push(crate::boot::VirtioMmioNode {
                addr: pa,
                size: regs_per_dev,
                irq,
            });
            dlog!(
                "[run_boot l2cpu {}]   {}: x280_pa={:#x}",
                l2cpu_idx,
                label,
                pa
            );
        };
        emit(
            crate::virtio_engine::DEV_BLK,
            has_disk,
            crate::regs::virtio_mmio::DISK_IRQ,
            "disk",
        );
        emit(
            crate::virtio_engine::DEV_NET,
            has_network,
            crate::regs::virtio_mmio::NET_IRQ,
            "net",
        );
        emit(
            crate::virtio_engine::DEV_CONSOLE,
            has_console,
            crate::regs::virtio_mmio::CONSOLE_IRQ,
            "console",
        );
        emit(
            crate::virtio_engine::DEV_RNG,
            has_rng,
            crate::regs::virtio_mmio::RNG_IRQ,
            "rng",
        );
        // Cloud-init NoCloud seed (#82). DEV_BLK1 is reserved for it
        // so the seed disk index is stable across boots and operator
        // udev rules can rely on `serial=cidata` finding it at a
        // predictable virtio-mmio address.
        emit(
            crate::virtio_engine::DEV_BLK1,
            has_cidata,
            crate::regs::virtio_mmio::DISK1_IRQ,
            "cidata",
        );
        // M6 (#78) 16550 UART. Lives at a fixed offset within the
        // engine's small TLB window — no second TLB slot needed.
        //
        // INTENTIONALLY NOT EMITTED IN THE DTB by default. The
        // Tenstorrent-built OpenSBI scans `/chosen/stdout-path` AND
        // /soc/serial* and prefers a real ns16550a over its DBCN
        // debug-console fallback. With this node present, M-mode
        // console output goes through the lossy 8250 emulation (#79)
        // instead of the byte-clean chip-DRAM virtuart drained by
        // `chip_console.rs`. Symptom: U-Boot / OpenSBI / earlycon
        // banners arrive at the operator's terminal heavily
        // corrupted.
        //
        // The 8250 *register file* is still set up in BRISC L1 so a
        // patched guest that writes to the fixed UART PA gets its
        // bytes drained by TRISC0 + the kick poller. Leaving it
        // visible to OpenSBI was the regression. Re-add the DTB node
        // (set `uart_addr_for_dtb = Some(uart_pa)`) only when
        // bringing up a distro that *requires* `console=ttyS0` AND
        // accepts the corrupted boot output.
        let _uart_pa = crate::uart_engine::uart_pa_from_engine_base(x280_base);
    }
    dlog!(
        "[run_boot l2cpu {}] patching DTB (memory start=0x{:x} size=0x{:x}, {} virtio nodes, uart={:?})",
        l2cpu_idx,
        starting_address,
        memory_size,
        virtio_nodes.len(),
        uart_addr_for_dtb,
    );
    // #94 guest-OS shutdown: per-L2CPU shutdown command register sits
    // at a fixed offset within the engine TLB window. Compute the PA
    // and pass to modify_dtb so the DT carries `/soc/syscon@<addr>`
    // + `/poweroff` nodes pointing at it.
    //
    // #166 Phase 1 escape hatch: BHX_SOFT_REBOOT=1 skips the syscon
    // injection entirely. With the syscon node absent, OpenSBI's
    // fdt_reset_syscon driver doesn't register itself; SBI SRST falls
    // through to sbi_exit → sbi_platform_final_exit → sbi_hsm_exit,
    // landing in our patched purgatory hook (third_party/opensbi/
    // patches/0002-bhx-purgatory-magic.patch) which writes the
    // "PARKED__" magic at scratch->fw_start + 0xE0000.
    let soft_reboot = std::env::var("BHX_SOFT_REBOOT").ok().as_deref() == Some("1");
    let shutdown_addr = if soft_reboot {
        dlog!(
            "[run_boot l2cpu {}] BHX_SOFT_REBOOT=1: skipping syscon-poweroff DTB injection",
            l2cpu_idx
        );
        None
    } else {
        x280_base_for_shutdown.map(|b| b + crate::regs::shutdown::OFFSET_FROM_ENGINE_BASE)
    };
    let dtb_patched = boot::modify_dtb(
        &dtb_raw,
        &boot_device,
        starting_address,
        memory_size,
        &virtio_nodes,
        uart_addr_for_dtb,
        has_console,
        shutdown_addr,
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
    // All four virtio devices' pre-inits (rng / net / disk / console)
    // happen further up, next to their host-buffer allocations — those
    // writes go to the daemon-local mmap (the chip reads them via PCIe
    // outbound iATU + x280 TLB, not via NoC writes from the daemon).
    // The chip-DRAM virtio nodes that stay in the DTB to keep the
    // post-boot `add-*` RPCs working are deliberately *not* pre-init'd
    // here; the kernel sees them as `Wrong magic value` and ignores.

    dlog!(
        "[run_boot l2cpu {}] image+pre_init done; deferring reset release until workers spawn",
        l2cpu_idx
    );
    Ok(BootArtifacts { l2cpu })
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
fn release_l2cpu_from_reset(
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    l2cpu: &L2Cpu,
) -> crate::Result<()> {
    dlog!("[run_boot l2cpu {}] releasing from reset", l2cpu_idx);
    // (#166) Zero the bhx-purgatory status block before reset release.
    // After tt-smi -r the L2CPU's DRAM holds whatever pattern the
    // training pass left — typically random-looking bytes that read
    // as garbage like `0x0000555574209000` and confuse the
    // `daemon status` purgatory line ("unknown (0x...)"). Pre-zeroing
    // gives the operator a clean "running (no SRST yet)" baseline
    // until the first SBI SRST writes the PARKED magic.
    let mem_base = crate::l2cpu::L2CPU_STARTING_ADDRESS[l2cpu_idx as usize];
    let purg_pa = mem_base + crate::regs::purgatory::STATUS_OFFSET;
    let _ = l2cpu.write32(purg_pa, 0);
    let _ = l2cpu.write32(purg_pa + 4, 0);
    // reset_x280 goes through SharedChip so the PLL step and the
    // L2CPU_RESET R-M-W serialize against any other boot RPC issuing the
    // same sequence.
    state.shared_chip.reset_x280(&[l2cpu_idx as usize])?;
    dlog!("[run_boot l2cpu {}] configuring prefetchers", l2cpu_idx);
    boot::configure_prefetchers(l2cpu).map_err(crate::Error::io_ctx("configure_prefetchers"))?;
    dlog!("[run_boot l2cpu {}] run_boot_sequence done", l2cpu_idx);
    Ok(())
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
        virtio_console: None,
        virtio_rng: None,
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
        let slot_guard = state.l2cpus[l2cpu_idx as usize].lock_or_internal_error()?;
        match slot_guard.as_ref() {
            Some(slot) => (
                slot.console_hub.clone(),
                slot.console_input_tx.clone(),
                slot.virtio_console.as_ref().map(|vc| vc.input_buf.clone()),
            ),
            None => {
                // Slot is gone. Try a post-mortem replay (#160) of the
                // tail captured at internal_stop time. Rw/Takeover are
                // refused — there's no live core to forward keystrokes
                // to and silently degrading would mislead the operator.
                drop(slot_guard);
                return dispatch_attach_console_post_mortem(
                    sock,
                    state,
                    l2cpu_idx,
                    mode,
                    daemon_end,
                    daemon_read,
                    client_end,
                );
            }
        }
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
            live: true,
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
                        // Per-client reader thread; if the vc input
                        // mutex is poisoned the producing worker
                        // already paniced — let this thread die too.
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

/// Post-mortem attach: the slot is gone, but we may have a captured
/// scrollback tail from `internal_stop` (#160). If so, replay it
/// read-only and EOF the daemon side so the client exits cleanly.
/// Rw / Takeover are refused because there's no live core to forward
/// keystrokes to and silently degrading would mislead the operator.
fn dispatch_attach_console_post_mortem(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    mode: ConsoleMode,
    daemon_end: UnixStream,
    daemon_read: UnixStream,
    client_end: UnixStream,
) -> crate::Result<()> {
    if !matches!(mode, ConsoleMode::Ro) {
        // The daemon-side socketpair gets dropped here; the kernel
        // tears them down without the operator's client_end having
        // ever seen them.
        drop(daemon_end);
        drop(daemon_read);
        drop(client_end);
        return Err(crate::Error::slot_state(format!(
            "l2cpu {} is not booted; attach in Ro mode to view the last scrollback",
            l2cpu_idx
        )));
    }
    let tail: Vec<u8> = {
        let g = state.shutdown_tails[l2cpu_idx as usize].lock_or_internal_error()?;
        g.clone()
    };
    if tail.is_empty() {
        drop(daemon_end);
        drop(daemon_read);
        drop(client_end);
        return Err(crate::Error::slot_state(format!(
            "l2cpu {} is not booted",
            l2cpu_idx
        )));
    }
    if let Err(e) = write_frame(
        sock,
        &Response::Attached {
            scrollback_bytes: tail.len() as u32,
            live: false,
        },
    ) {
        dlog!(
            "[daemon] post-mortem attach l2cpu {} write response: {}",
            l2cpu_idx,
            e
        );
        return Ok(());
    }
    if let Err(e) = send_fd(sock, client_end.as_raw_fd()) {
        dlog!(
            "[daemon] post-mortem attach l2cpu {} send_fd: {}",
            l2cpu_idx,
            e
        );
        return Ok(());
    }
    drop(client_end);
    if let Err(e) = write_scrollback(&daemon_read, &tail) {
        dlog!(
            "[daemon] post-mortem attach l2cpu {} write_scrollback: {}",
            l2cpu_idx,
            e
        );
    }
    // EOF the daemon side so the client's reader loop sees end-of-stream
    // and exits cleanly. No worker thread, no further bytes.
    let _ = daemon_read.shutdown(std::net::Shutdown::Both);
    let _ = daemon_end.shutdown(std::net::Shutdown::Both);
    Ok(())
}

// ---------------------------------------------------------------------------
// AddDisk
// ---------------------------------------------------------------------------

fn dispatch_add_disk(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    path: String,
    name: Option<String>,
) -> crate::Result<()> {
    validate_l2cpu(l2cpu_idx)?;
    let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock_or_internal_error()?;
    let slot = slot_guard
        .as_mut()
        .ok_or_else(|| crate::Error::slot_state(format!("l2cpu {} is not booted", l2cpu_idx)))?;
    let disk_image = validate_add_disk_request(
        slot.disks.len(),
        name.as_deref(),
        slot.disks.iter().map(|d| d.name.clone()),
        std::path::Path::new(&path),
    )?;
    // Pick the first free DEV_BLK* slot. Each existing disk's slot_idx
    // is in engine-slot space (l2cpu_idx * DEVS_PER_L2CPU + dev_idx);
    // map back to dev_idx for the picker.
    let l2cpu_base = (l2cpu_idx as u32) * crate::virtio_engine::DEVS_PER_L2CPU;
    let used: Vec<u32> = slot.disks.iter().map(|d| d.slot_idx - l2cpu_base).collect();
    let dev_idx = pick_free_blk_slot(&used).ok_or_else(|| {
        crate::Error::slot_state(format!(
            "no free virtio-blk slots (max {})",
            MAX_DISKS_PER_L2CPU
        ))
    })?;
    let irq = irq_for_blk_dev_idx(dev_idx);

    // Engine path: register a fresh VirtioBlk with the kick poller
    // and push a stub DiskWorker so `daemon status` reflects the
    // attach. The kick poller drops kicks for unregistered slots
    // (#71 M5.5b), so a guest probe before this call simply doesn't
    // see the device — same effect as the legacy worker not having
    // started yet. There is no per-device worker thread to spawn;
    // the kick poller handles dispatch.

    {
        if let (Some(poller), Some(engine)) = (
            state.kick_poller.lock_or_internal_error()?.as_ref(),
            state.tensix_engine.lock_or_internal_error()?.clone(),
        ) {
            match crate::virtio::block::VirtioBlk::from_file_with_serial(
                disk_image,
                l2cpu_idx,
                name.clone(),
            ) {
                Ok(blk) => {
                    let slot_idx = l2cpu_base + dev_idx;
                    let config_addr = crate::virtio_engine::slot_regs_base(slot_idx)
                        + crate::virtio_engine::MMIO_CONFIG;
                    crate::virtio::VirtioDeviceImpl::init_config(&blk, engine.l1_ptr(config_addr));
                    let entry = crate::tensix_data_plane::RegEntry::new(
                        slot_idx,
                        Arc::clone(&slot.l2cpu),
                        Box::new(blk),
                        Arc::clone(&slot.interrupt),
                        irq,
                        crate::virtio::InterruptKind::Block,
                    );
                    poller.register_slot(entry);
                    slot.disks.push(DiskWorker {
                        path: path.clone(),
                        slot_idx,
                        name: name.clone(),
                        worker: WorkerHandle {
                            exit: Arc::new(AtomicBool::new(false)),
                            thread: None,
                            description: format!("disk l2cpu {} @ {} (engine)", l2cpu_idx, path),
                        },
                    });
                    dlog!(
                        "[add_disk l2cpu {}] engine: registered blk on slot {} for {} (name={:?})",
                        l2cpu_idx,
                        slot_idx,
                        path,
                        name
                    );
                    reply_ok(sock);
                    return Ok(());
                }
                Err(e) => {
                    dlog!(
                        "[add_disk l2cpu {}] engine: VirtioBlk::from_file failed: {}",
                        l2cpu_idx,
                        e
                    );
                    return Err(crate::Error::slot_state(format!(
                        "VirtioBlk::from_file({}) failed: {}",
                        path, e
                    )));
                }
            }
        }
    }
    let _ = disk_image; // engine path not brought up
    Err(crate::Error::slot_state(
        "tensix engine not yet brought up; cold-boot the L2CPU first",
    ))
}

fn dispatch_remove_disk(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    name: Option<String>,
) -> crate::Result<()> {
    dlog!(
        "[remove_disk l2cpu {}] dispatch entry (name={:?})",
        l2cpu_idx,
        name
    );
    validate_l2cpu(l2cpu_idx)?;

    // Take the matching disks out under the lock; unregister their
    // kick-poller slots while still holding the slot mutex (so the
    // poller can't race a fresh kick against a half-detached
    // VirtioBlk); release the lock; then join workers outside.
    // `stop_and_join` blocks until the worker's poll loop notices its
    // exit flag (~100 ms worst case) — holding the state mutex for
    // that long would block every other RPC on other L2CPUs.
    let disks_to_remove: Vec<DiskWorker> = {
        let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock_or_internal_error()?;
        let slot = slot_guard.as_mut().ok_or_else(|| {
            crate::Error::slot_state(format!("l2cpu {} is not booted", l2cpu_idx))
        })?;
        validate_remove_disk_request(slot.disks.is_empty()).map_err(crate::Error::slot_state)?;
        let mut taken: Vec<DiskWorker> = Vec::new();
        match name.as_deref() {
            None => {
                // Legacy behavior: remove all disks. Kept for
                // single-disk callers that don't pass a selector.
                taken.append(&mut slot.disks);
            }
            Some(want) => {
                let mut keep: Vec<DiskWorker> = Vec::with_capacity(slot.disks.len());
                for d in std::mem::take(&mut slot.disks) {
                    if d.name.as_deref() == Some(want) {
                        taken.push(d);
                    } else {
                        keep.push(d);
                    }
                }
                slot.disks = keep;
                if taken.is_empty() {
                    return Err(crate::Error::slot_state(format!(
                        "no disk with name {:?} attached to l2cpu {}",
                        want, l2cpu_idx
                    )));
                }
            }
        }
        // Drop the kick-poller registrations for the slots we're
        // taking. Done under the slot mutex so a concurrent boot RPC
        // for this L2CPU can't observe a torn state.
        if let Some(poller) = state.kick_poller.lock_or_internal_error()?.as_ref() {
            for d in &taken {
                poller.unregister_slot(d.slot_idx);
            }
        }
        taken
    };
    for d in disks_to_remove {
        dlog!(
            "[remove_disk l2cpu {}] joining worker for {} (slot {})",
            l2cpu_idx,
            d.path,
            d.slot_idx
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
    let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock_or_internal_error()?;
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

    // Engine path: build the VirtioNet directly + register with the
    // kick poller. No worker thread; the kick poller's RX poll loop
    // (#71 M5.5e) drives slirp's recv side.

    {
        if let Some(poller) = state.kick_poller.lock_or_internal_error()?.as_ref() {
            // add-net is a hot-add post-boot path: no profile-pinned
            // hostname here; `format_dhcp_hostname` is fine.
            match crate::virtio::network::VirtioNet::new(&forwards, state.card, l2cpu_idx, None) {
                Ok(net) => {
                    let slot_idx = (l2cpu_idx as u32) * crate::virtio_engine::DEVS_PER_L2CPU
                        + crate::virtio_engine::DEV_NET;
                    let entry = crate::tensix_data_plane::RegEntry::new(
                        slot_idx,
                        Arc::clone(&slot.l2cpu),
                        Box::new(net),
                        Arc::clone(&slot.interrupt),
                        crate::regs::virtio_mmio::NET_IRQ,
                        crate::virtio::InterruptKind::Net,
                    );
                    poller.register_slot(entry);
                    slot.net = Some(WorkerHandle {
                        exit: Arc::new(AtomicBool::new(false)),
                        thread: None,
                        description: format!("net l2cpu {} (engine)", l2cpu_idx),
                    });
                    dlog!(
                        "[add_net l2cpu {}] engine: registered net on slot {} (forwards={:?})",
                        l2cpu_idx,
                        slot_idx,
                        forwards
                    );
                    reply_ok(sock);
                    return Ok(());
                }
                Err(e) => {
                    dlog!(
                        "[add_net l2cpu {}] engine: VirtioNet::new failed: {}",
                        l2cpu_idx,
                        e
                    );
                    return Err(crate::Error::slot_state(format!(
                        "VirtioNet::new failed: {}",
                        e
                    )));
                }
            }
        }
    }
    let _ = forwards; // engine path not brought up
    Err(crate::Error::slot_state(
        "tensix engine not yet brought up; cold-boot the L2CPU first",
    ))
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
    // Engine path: drop the kick poller's net registration first
    // (frees the VirtioNet's slirp connection too via Box drop).
    // No-op when there's no engine registration for this slot.

    if let Some(poller) = state.kick_poller.lock_or_internal_error()?.as_ref() {
        let slot_idx = (l2cpu_idx as u32) * crate::virtio_engine::DEVS_PER_L2CPU
            + crate::virtio_engine::DEV_NET;
        poller.unregister_slot(slot_idx);
    }
    // Take the net handle under the lock, join outside (same reasoning as
    // dispatch_remove_disk).
    let net = {
        let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock_or_internal_error()?;
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
    let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock_or_internal_error()?;
    let slot = slot_guard
        .as_mut()
        .ok_or_else(|| crate::Error::slot_state(format!("l2cpu {} is not booted", l2cpu_idx)))?;
    if slot.virtio_console.is_some() {
        return Err(crate::Error::slot_state("virtio-console already attached"));
    }

    // Engine path: build a VirtioConsole + register with the kick
    // poller; same shape as the boot-time `console` branch in
    // dispatch_boot. Stub WorkerHandle so the slot teardown path
    // doesn't need to special-case engine-vs-legacy.

    {
        if let Some(poller) = state.kick_poller.lock_or_internal_error()?.as_ref() {
            let input_buf = Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(crate::virtio::console::RX_BUFFER_CAP),
            ));
            let device = crate::virtio::console::VirtioConsole::new(
                slot.console_hub.clone(),
                Arc::clone(&input_buf),
            );
            let slot_idx = (l2cpu_idx as u32) * crate::virtio_engine::DEVS_PER_L2CPU
                + crate::virtio_engine::DEV_CONSOLE;
            let entry = crate::tensix_data_plane::RegEntry::new(
                slot_idx,
                Arc::clone(&slot.l2cpu),
                Box::new(device),
                Arc::clone(&slot.interrupt),
                crate::regs::virtio_mmio::CONSOLE_IRQ,
                crate::virtio::InterruptKind::Console,
            );
            poller.register_slot(entry);
            slot.virtio_console = Some(crate::daemon::VirtioConsoleSlot {
                worker: WorkerHandle {
                    exit: Arc::new(AtomicBool::new(false)),
                    thread: None,
                    description: format!("virtio-console l2cpu {} (engine)", l2cpu_idx),
                },
                input_buf,
            });
            dlog!(
                "[add_console l2cpu {}] engine: registered console on slot {}",
                l2cpu_idx,
                slot_idx
            );
            reply_ok(sock);
            return Ok(());
        }
    }

    Err(crate::Error::slot_state(
        "tensix engine not yet brought up; cold-boot the L2CPU first",
    ))
}

fn dispatch_remove_console(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
) -> crate::Result<()> {
    dlog!("[remove_console l2cpu {}] dispatch entry", l2cpu_idx);
    validate_l2cpu(l2cpu_idx)?;
    // Engine path: drop kick-poller registration first (frees the
    // VirtioConsole's input_buf reference too).

    if let Some(poller) = state.kick_poller.lock_or_internal_error()?.as_ref() {
        let slot_idx = (l2cpu_idx as u32) * crate::virtio_engine::DEVS_PER_L2CPU
            + crate::virtio_engine::DEV_CONSOLE;
        poller.unregister_slot(slot_idx);
    }
    // Take the slot under the lock, join outside.
    let vc = {
        let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock_or_internal_error()?;
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

/// Drop every kick-poller registration for `l2cpu_idx` before the
/// caller tears down the L2CpuSlot. Without this the poller keeps
/// holding `Arc<L2Cpu>` for a stale slot — when the operator
/// re-boots that L2CPU, the now-stale L2Cpu sticks around behind the
/// scenes and any kick the firmware fires for the new boot still
/// dispatches against the old memory mmap.
///
/// No-op without the engine feature; under `virtio-engine` we ask
/// the poller to drop entries for slot indices owned by `l2cpu_idx`
/// (`l2cpu_idx*4 + dev_idx` for `dev_idx` in 0..4 — see
/// `virtio_engine::DEVS_PER_L2CPU`).
fn unregister_engine_slots(state: &Arc<DaemonState>, l2cpu_idx: u8) {
    // Best-effort — called from teardown paths (dispatch_stop,
    // serve()'s shutdown loop) where a poisoned poller mutex means
    // the daemon is on its way out anyway. Silently skip in that
    // case rather than crashing harder.
    let Ok(poller_guard) = state.kick_poller.lock() else {
        return;
    };
    if let Some(poller) = poller_guard.as_ref() {
        let base = (l2cpu_idx as u32) * crate::virtio_engine::DEVS_PER_L2CPU;
        for dev_idx in 0..crate::virtio_engine::DEVS_PER_L2CPU {
            poller.unregister_slot(base + dev_idx);
        }
        poller.unregister_uart(l2cpu_idx);
        poller.unregister_shutdown(l2cpu_idx);
    }
}

fn dispatch_stop(sock: &UnixStream, state: &Arc<DaemonState>, l2cpu_idx: u8) -> crate::Result<()> {
    dlog!("[stop l2cpu {}] dispatch_stop entry", l2cpu_idx);
    match internal_stop(state, l2cpu_idx, "client-requested")? {
        true => {
            reply_ok(sock);
            Ok(())
        }
        false => Err(crate::Error::slot_state(format!(
            "l2cpu {} is not booted",
            l2cpu_idx
        ))),
    }
}

/// Spawn the guest-poweroff handler thread (#94). Takes the receiver
/// off `state.guest_poweroff_rx`, then loops forever consuming
/// l2cpu_idx values pushed by the kick poller and tearing down the
/// matching slot via `internal_stop`. Exits when the channel closes
/// (i.e. all senders are dropped — happens when the daemon is on its
/// way out and the kick poller has been shut down).
/// Polls PLL4 (L2CPU PLL) every second and logs every observed change.
/// Used to track down "PLL silently reverted to ARC init values" reports
/// — without this, dlog only fires on our own set_frequency calls.
fn spawn_pll_watcher(state: Arc<DaemonState>) {
    std::thread::Builder::new()
        .name("pll-watcher".to_string())
        .spawn(move || {
            const PLL4_BASE: u64 = 0x80020500;
            const CNTL1_OFF: u64 = 0x4;
            const CNTL5_OFF: u64 = 0x14;
            let mut last: Option<(u32, u32)> = None;
            while !state.shutdown.load(Ordering::Relaxed) {
                let cntl1 = state.shared_chip.arc_read32(PLL4_BASE + CNTL1_OFF);
                let cntl5 = state.shared_chip.arc_read32(PLL4_BASE + CNTL5_OFF);
                if let (Ok(c1), Ok(c5)) = (cntl1, cntl5) {
                    let observed = (c1, c5);
                    if last != Some(observed) {
                        let fbdiv = (c1 >> 16) as u16;
                        let postdiv0 = (c5 & 0xFF) as u8;
                        let mhz = 25u32 * (fbdiv as u32) / ((postdiv0 as u32) + 1);
                        dlog!(
                            "[pll-watcher] PLL4 fbdiv={} postdiv0={} → {} MHz \
                             (CNTL1={:#010x} CNTL5={:#010x})",
                            fbdiv,
                            postdiv0,
                            mhz,
                            c1,
                            c5
                        );
                        last = Some(observed);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        })
        .expect("spawn pll-watcher");
}

fn spawn_guest_poweroff_handler(state: Arc<DaemonState>) {
    let rx = match state.guest_poweroff_rx.lock_or_internal_error() {
        Ok(mut g) => g.take(),
        Err(e) => {
            dlog!("[guest-poweroff] failed to acquire rx mutex: {}", e);
            return;
        }
    };
    let Some(rx) = rx else {
        dlog!("[guest-poweroff] handler already running; not spawning a second");
        return;
    };
    std::thread::Builder::new()
        .name("guest-poweroff-handler".to_string())
        .spawn(move || {
            for l2cpu_idx in rx {
                dlog!(
                    "[guest-poweroff] received SRST_SHUTDOWN for l2cpu {}",
                    l2cpu_idx
                );
                match internal_stop(&state, l2cpu_idx, "guest SRST_SHUTDOWN") {
                    Ok(true) => {}
                    Ok(false) => {
                        dlog!(
                            "[guest-poweroff] l2cpu {} not booted on event arrival; ignoring",
                            l2cpu_idx
                        );
                    }
                    Err(e) => {
                        dlog!(
                            "[guest-poweroff] internal_stop l2cpu {} failed: {}",
                            l2cpu_idx,
                            e
                        );
                    }
                }
            }
            dlog!("[guest-poweroff] receiver closed; handler exiting");
        })
        .expect("spawn guest-poweroff-handler");
}

/// Tear down an L2CPU slot. Called from both the client-driven
/// `dispatch_stop` and the daemon-internal #94 guest-poweroff handler.
/// Returns `Ok(true)` if a slot was present and torn down, `Ok(false)`
/// if no slot was booted at the time of call. The `reason` string
/// goes into the dlog for triage — distinguishes "user typed
/// `bhx daemon stop`" from "guest issued SBI SRST_SHUTDOWN".
pub(crate) fn internal_stop(
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    reason: &str,
) -> crate::Result<bool> {
    validate_l2cpu(l2cpu_idx)?;
    let taken = state.l2cpus[l2cpu_idx as usize]
        .lock_or_internal_error()?
        .take();
    match taken {
        Some(slot) => {
            dlog!(
                "[stop l2cpu {}] {} — slot taken; joining workers",
                l2cpu_idx,
                reason
            );
            slot.console_hub
                .disconnect_all_with_reason(&format!("l2cpu {} stopped ({})", l2cpu_idx, reason));
            // Snapshot a tail of the scrollback for post-mortem
            // attaches (#160). disconnect_all_with_reason has just
            // appended the goodbye line into the ring, so the
            // captured tail ends with `[bhx: l2cpu N stopped (...)]`.
            // Must run BEFORE `slot.shutdown()` consumes the slot.
            let tail = slot
                .console_hub
                .tail(crate::daemon::console_hub::SHUTDOWN_TAIL_BYTES);
            if let Ok(mut g) = state.shutdown_tails[l2cpu_idx as usize].lock() {
                *g = tail;
            }
            unregister_engine_slots(state, l2cpu_idx);
            slot.shutdown();
            state.maybe_idle_pll();
            dlog!("[stop l2cpu {}] {} — workers joined", l2cpu_idx, reason);
            Ok(true)
        }
        None => {
            dlog!("[stop l2cpu {}] {} — no slot present", l2cpu_idx, reason);
            Ok(false)
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

    /// Tests in this module observe the cumulative
    /// `DAEMON_RPC_TOTAL` / `DAEMON_RPC_ERRORS_TOTAL` counters from
    /// `daemon::metrics`. Cargo runs tests in parallel within a binary
    /// — a "did this dispatch bump the counter?" snapshot+assert in
    /// one test races with a parallel test that bumps the same global.
    /// Serialize all metrics-observing tests on this lock so each can
    /// take its before/after snapshots without interference. Recovers
    /// from poison so a panicking test doesn't wedge the rest of the run.
    fn metrics_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
        M.lock().unwrap_or_else(|p| p.into_inner())
    }

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

        let _guard = metrics_test_lock();
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

    /// A poisoned `state.tensix_engine` mutex must not panic the
    /// Post-mortem attach (#160): when the slot is gone but a tail
    /// has been captured at internal_stop time, an Ro attach replays
    /// the tail with `live: false` and EOFs the daemon side. Rw is
    /// rejected with a clear "attach in Ro mode" message.
    #[test]
    fn dispatch_attach_console_replays_tail_for_stopped_slot() {
        use crate::daemon::protocol::{recv_fd, ConsoleMode, Request};
        use crate::shared_chip::SharedChip;
        use std::io::Read;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let state = Arc::new(DaemonState::new(0, Arc::new(SharedChip::placeholder())));
        let captured = b"early dmesg...\n[bhx: l2cpu 0 stopped (test)]\r\n";
        *state.shutdown_tails[0].lock().unwrap() = captured.to_vec();

        // Ro path: replay tail, live=false, fd EOFs after writing.
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_frame(
            &mut client,
            &Request::AttachConsole {
                l2cpu: 0,
                mode: ConsoleMode::Ro,
            },
        )
        .unwrap();
        handle_client(server, Arc::clone(&state));

        let resp: Response = read_frame(&mut client).expect("read response");
        let bytes = match resp {
            Response::Attached {
                scrollback_bytes,
                live,
            } => {
                assert!(!live, "post-mortem attach should report live=false");
                scrollback_bytes
            }
            other => panic!("expected Attached, got {:?}", other),
        };
        assert_eq!(bytes as usize, captured.len());

        let fd = recv_fd(&client).expect("recv_fd");
        let mut chan: std::fs::File = fd.into();
        let mut got = Vec::new();
        chan.read_to_end(&mut got).expect("drain post-mortem fd");
        assert_eq!(got.as_slice(), captured.as_slice());

        // Rw path: rejected with slot-state error mentioning Ro.
        let (mut client2, server2) = UnixStream::pair().unwrap();
        client2
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_frame(
            &mut client2,
            &Request::AttachConsole {
                l2cpu: 0,
                mode: ConsoleMode::Rw,
            },
        )
        .unwrap();
        handle_client(server2, state);

        let resp_rw: Response = read_frame(&mut client2).expect("read response");
        match resp_rw {
            Response::Error { error } => {
                assert!(
                    error.contains("Ro"),
                    "Rw rejection should mention Ro mode, got {:?}",
                    error
                );
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    /// Empty tail + absent slot keeps today's "not booted" error.
    #[test]
    fn dispatch_attach_console_empty_tail_returns_not_booted() {
        use crate::daemon::protocol::{ConsoleMode, Request};
        use crate::shared_chip::SharedChip;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let state = Arc::new(DaemonState::new(0, Arc::new(SharedChip::placeholder())));
        // shutdown_tails[0] is empty by default.
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_frame(
            &mut client,
            &Request::AttachConsole {
                l2cpu: 0,
                mode: ConsoleMode::Ro,
            },
        )
        .unwrap();
        handle_client(server, state);

        let resp_err: Response = read_frame(&mut client).expect("read response");
        match resp_err {
            Response::Error { error } => {
                assert!(
                    error.contains("not booted"),
                    "empty-tail rejection should preserve historical wording, got {:?}",
                    error
                );
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    /// A poisoned `state.tensix_engine` mutex must not panic the
    /// daemon: dispatch_status used to call `.lock().unwrap()` on it
    /// (sweep #104 missed that one site, fix in #145), which would
    /// kill the daemon on the next status RPC after any unrelated
    /// panic that held the lock. Now wrapped in `lock_or_internal_error`,
    /// so the RPC just gets a framed `Response::Error` and
    /// `rpc_errors_total{Status}` bumps.
    #[test]
    fn dispatch_status_handles_poisoned_tensix_engine_lock() {
        use crate::daemon::metrics::{RpcMethod, DAEMON_RPC_ERRORS_TOTAL};
        use crate::shared_chip::SharedChip;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let _guard = metrics_test_lock();
        let state = Arc::new(DaemonState::new(0, Arc::new(SharedChip::placeholder())));

        // Poison `state.tensix_engine` by panicking inside a held lock.
        let state_for_poison = Arc::clone(&state);
        let _ = std::thread::spawn(move || {
            let _guard = state_for_poison.tensix_engine.lock().unwrap();
            panic!("intentional: poisoning tensix_engine for test");
        })
        .join();
        assert!(state.tensix_engine.is_poisoned());

        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        write_frame(&mut client, &Request::Status).unwrap();

        let err_before = DAEMON_RPC_ERRORS_TOTAL.at(RpcMethod::Status).get();
        handle_client(server, state);

        let resp: Response = read_frame(&mut client).expect("read response");
        match resp {
            Response::Error { error } => {
                assert!(
                    error.to_lowercase().contains("internal"),
                    "expected internal-error message, got {:?}",
                    error
                );
            }
            other => panic!("expected Error response, got {:?}", other),
        }
        assert!(
            DAEMON_RPC_ERRORS_TOTAL.at(RpcMethod::Status).get() > err_before,
            "rpc_errors_total{{Status}} should bump on poisoned-lock failure"
        );
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

        let _guard = metrics_test_lock();

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
                name: None,
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

        let _guard = metrics_test_lock();
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
                name: None,
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

    // ---- classify_sibling (#107 item 4) ----

    #[test]
    fn classify_sibling_treats_parked_magic_as_parked() {
        let purg = Some(crate::regs::purgatory::STATUS_PARKED);
        assert_eq!(classify_sibling(purg), SiblingClassification::Parked);
    }

    #[test]
    fn classify_sibling_treats_zero_as_running() {
        // Cold-booted guest with no SBI SRST yet — purgatory cell
        // sits at zero. Must NOT be misread as parked, or we'd
        // SIGBUS the daemon by reset_board-ing into a live worker.
        assert_eq!(classify_sibling(Some(0)), SiblingClassification::Running);
    }

    #[test]
    fn classify_sibling_treats_unknown_value_as_running() {
        // Defensive: a non-PARKED, non-zero value (kernel printk
        // bytes, a partial write that the kernel scribbled, etc.)
        // must default to Running. The reset_board path then bails
        // out rather than risking the SIGBUS.
        assert_eq!(
            classify_sibling(Some(0xDEAD_BEEF_CAFE_BABE)),
            SiblingClassification::Running
        );
    }

    #[test]
    fn classify_sibling_treats_read_failure_as_running() {
        // Read failures (`Err` from `l2cpu.read32`) become `None` in
        // the caller; this branch must also be Running. Dropping a
        // sibling whose state we couldn't even read would be a
        // correctness disaster.
        assert_eq!(classify_sibling(None), SiblingClassification::Running);
    }

    // ---- validate_add_disk_request ----

    #[test]
    fn add_disk_rejects_when_max_disks_attached() {
        let tf = tempfile::NamedTempFile::new().unwrap();
        // Fill all three blk slots.
        let existing = vec![Some("rootfs".to_string()), Some("cidata".to_string()), None];
        let err =
            validate_add_disk_request(MAX_DISKS_PER_L2CPU, None, existing, tf.path()).unwrap_err();
        assert!(matches!(err, crate::Error::SlotState(_)));
        let msg = err.to_string();
        assert!(msg.contains("max"), "got: {}", msg);
    }

    #[test]
    fn add_disk_rejects_when_image_path_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("nonexistent.ext4");
        let err = validate_add_disk_request(0, None, Vec::new(), &bogus).unwrap_err();
        // Io variant for IO failures — wire shape "cannot open disk image <p>: <io>".
        assert!(matches!(err, crate::Error::Io { .. }));
        let msg = err.to_string();
        assert!(msg.contains("cannot open disk image"), "got: {}", msg);
    }

    #[test]
    fn add_disk_rejects_duplicate_name() {
        let tf = tempfile::NamedTempFile::new().unwrap();
        let existing = vec![Some("cidata".to_string())];
        let err = validate_add_disk_request(1, Some("cidata"), existing, tf.path()).unwrap_err();
        assert!(matches!(err, crate::Error::SlotState(_)));
        let msg = err.to_string();
        assert!(msg.contains("already has a disk with name"), "got: {}", msg);
    }

    #[test]
    fn add_disk_accepts_valid_open_image_on_empty_slot() {
        let mut tf = tempfile::NamedTempFile::new().unwrap();
        // Disk image needs to be writable too (worker opens with read+write).
        tf.write_all(&[0u8; 4096]).unwrap();
        // Returns the open File on success — the caller hands the same
        // fd to the worker so a path-resolved-twice TOCTOU can't redirect
        // the daemon at a different inode after this point.
        let _file = validate_add_disk_request(0, None, Vec::new(), tf.path())
            .expect("valid path on empty slot must accept");
    }

    #[test]
    fn pick_free_blk_slot_walks_dev_blk_first() {
        use crate::virtio_engine::{DEV_BLK, DEV_BLK1, DEV_BLK2};
        // No disks attached → DEV_BLK.
        assert_eq!(pick_free_blk_slot(&[]), Some(DEV_BLK));
        // Rootfs attached → next is DEV_BLK1 (cidata slot).
        assert_eq!(pick_free_blk_slot(&[DEV_BLK]), Some(DEV_BLK1));
        // Rootfs + cidata → DEV_BLK2.
        assert_eq!(pick_free_blk_slot(&[DEV_BLK, DEV_BLK1]), Some(DEV_BLK2));
        // All three taken → None.
        assert_eq!(pick_free_blk_slot(&[DEV_BLK, DEV_BLK1, DEV_BLK2]), None);
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
