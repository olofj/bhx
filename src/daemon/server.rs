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
use std::time::Duration;

use crate::boot;
use crate::daemon::chip_console;
use crate::dlog;
use crate::daemon::console_hub::ConsoleHub;
use crate::daemon::lifetime;
use crate::daemon::protocol::{
    read_frame, send_fd, write_frame, ConsoleMode, L2CpuState, L2CpuStatus, Request, Response,
    StatusPayload,
};
use crate::daemon::{DaemonState, DiskWorker, L2CpuSlot, WorkerHandle};
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
pub fn serve(card: u32, listener: UnixListener) -> io::Result<()> {
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
fn probe_initial_chip_state(
    shared: &crate::shared_chip::SharedChip,
    card: u32,
) -> Vec<u8> {
    let val = shared.read_l2cpu_reset();
    dlog!(
        "[probe] L2CPU_RESET={:#010x} (card {})",
        val, card
    );
    let mut released = Vec::new();
    for idx in 0..4u8 {
        let bit = (val >> (idx + 4)) & 1;
        let state = if bit == 1 {
            "released (running or wedged — warm-resume candidate)"
        } else {
            "held in reset (cold-bootable)"
        };
        dlog!("[probe]   L2CPU {} bit {} = {} -> {}", idx, idx + 4, bit, state);
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

    match req {
        Request::Status => dispatch_status(&sock, &state),
        Request::Boot {
            l2cpu,
            opensbi,
            kernel,
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
            &kernel,
            &dtb,
            initramfs.as_deref(),
            &root_device,
            force_reset_pcie,
            disk,
            network,
            force,
        ),
        Request::AttachConsole { l2cpu, mode } => {
            dispatch_attach_console(sock, &state, l2cpu, mode)
        }
        Request::AddDisk { l2cpu, path } => dispatch_add_disk(&sock, &state, l2cpu, path),
        Request::RemoveDisk { l2cpu } => dispatch_remove_disk(&sock, &state, l2cpu),
        Request::AddNet { l2cpu, ssh_port } => dispatch_add_net(&sock, &state, l2cpu, ssh_port),
        Request::RemoveNet { l2cpu } => dispatch_remove_net(&sock, &state, l2cpu),
        Request::Stop { l2cpu } => dispatch_stop(&sock, &state, l2cpu),
        Request::Shutdown => dispatch_shutdown(&sock, &state),
    }
}

fn reply_ok(mut sock: &UnixStream) {
    let _ = write_frame(&mut sock, &Response::Ok);
}

fn reply_err(mut sock: &UnixStream, error: impl Into<String>) {
    let _ = write_frame(
        &mut sock,
        &Response::Error {
            error: error.into(),
        },
    );
}

/// Bounds-check the wire-format `l2cpu` index. Every dispatch handler
/// starts with the same `if idx >= 4 { reply_err … return }` dance —
/// route it through one place so a future change to the per-card core
/// count needs to land in exactly one match arm.
fn validate_l2cpu(idx: u8) -> Result<usize, &'static str> {
    if idx < 4 {
        Ok(idx as usize)
    } else {
        Err("l2cpu must be 0..3")
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

fn dispatch_status(mut sock: &UnixStream, state: &Arc<DaemonState>) {
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
    kernel: &str,
    dtb: &str,
    initramfs: Option<&str>,
    root_device: &str,
    force_reset_pcie: bool,
    disk: Option<String>,
    network: bool,
    force: bool,
) {
    dlog!(
        "[boot l2cpu {}] dispatch_boot entry: opensbi={} kernel={} dtb={} initramfs={:?} root={} force_reset_pcie={} disk={:?} network={} force={}",
        l2cpu_idx, opensbi, kernel, dtb, initramfs, root_device, force_reset_pcie, disk, network, force
    );
    if let Err(msg) = validate_l2cpu(l2cpu_idx) {
        reply_err(sock, msg);
        return;
    }
    if let Err(e) = handle_existing_slot(state, l2cpu_idx, force) {
        reply_err(sock, e);
        return;
    }

    // No daemon-wide serialization of the chip-touching phase here —
    // tile-(8,0) access is mediated by `SharedChip::seq_lock` inside each
    // of its typed methods, and per-L2CPU NOC traffic goes through each
    // L2CPU's own fd + TLB windows. Concurrent boots on different cores
    // proceed in parallel; the same-core case is still serialized by the
    // per-slot `Mutex<Option<L2CpuSlot>>` taken in `handle_existing_slot`.
    dlog!("[boot l2cpu {}] starting boot sequence", l2cpu_idx);
    let l2cpu = match run_boot_sequence(
        state,
        l2cpu_idx,
        opensbi,
        kernel,
        dtb,
        initramfs,
        root_device,
        force_reset_pcie,
    ) {
        Ok(l) => l,
        Err(e) => {
            dlog!("[boot l2cpu {}] boot sequence failed: {}", l2cpu_idx, e);
            reply_err(sock, format!("boot failed: {}", e));
            return;
        }
    };
    dlog!(
        "[boot l2cpu {}] boot sequence returned ok; initializing runtime slot",
        l2cpu_idx
    );

    let mut slot = match make_slot_from_l2cpu(l2cpu, l2cpu_idx) {
        Ok(s) => s,
        Err(e) => {
            dlog!("[boot l2cpu {}] make_slot_from_l2cpu failed: {}", l2cpu_idx, e);
            reply_err(sock, format!("post-boot L2Cpu init failed: {}", e));
            return;
        }
    };
    dlog!(
        "[boot l2cpu {}] slot ready (console worker spawned)",
        l2cpu_idx
    );

    if let Err(e) = start_initial_workers(&mut slot, state.card, l2cpu_idx, disk, network) {
        reply_err(sock, e);
        return;
    }
    install_slot_and_reply_ok(state, l2cpu_idx, slot, sock);
}

/// Reject the boot if a slot is already populated and `--force` wasn't
/// given. With `--force`, take the prior slot out and shut it down so
/// the boot can proceed without races between the new NOC writes and
/// the prior workers' TLB mmaps. Holds the per-slot mutex for the
/// is_some() check + the take(); other cores stay unblocked.
fn handle_existing_slot(
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    force: bool,
) -> Result<(), String> {
    let prior = {
        let mut guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
        if guard.is_some() && !force {
            return Err(format!(
                "l2cpu {} is already booted; stop it first, or re-run with --force",
                l2cpu_idx
            ));
        }
        if force { guard.take() } else { None }
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
        dlog!("[boot l2cpu {}] spawning disk worker for {}", l2cpu_idx, path);
        start_disk_worker(slot, &path).map_err(|e| {
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
    dlog!(
        "[boot l2cpu {}] dispatch_boot complete — replying ok",
        l2cpu_idx
    );
    reply_ok(sock);
}

fn start_disk_worker(slot: &mut L2CpuSlot, path: &str) -> io::Result<()> {
    let exit = Arc::new(AtomicBool::new(false));
    let l2cpu = slot.l2cpu.clone();
    let interrupt = slot.interrupt.clone();
    let exit_thread = exit.clone();
    let path_thread = path.to_string();
    let t = thread::spawn(move || {
        block::disk_main(l2cpu, interrupt, DISK_INT, DISK_MMIO, path_thread, exit_thread);
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
    let t = thread::spawn(move || {
        network::network_main(ssh_port, l2cpu, interrupt, NET_INT, NET_MMIO, exit_thread);
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
    kernel: &str,
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
    let boot_device = match initramfs {
        Some(p) => {
            let bytes = boot::read_bin_file(Path::new(p))?;
            boot::BootDevice::Initramfs {
                addr: rootfs_addr,
                len: bytes.len() as u64,
            }
        }
        None => boot::BootDevice::Vda(root_device.to_string()),
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
        Some(Path::new(kernel)),
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
    let hub = Arc::new(ConsoleHub::new());

    let (input_tx, input_rx) = mpsc::channel::<u8>();
    let exit = Arc::new(AtomicBool::new(false));

    dlog!("[make_slot l2cpu {}] spawning chip_console thread", l2cpu_idx);
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
    })
}

// ---------------------------------------------------------------------------
// AttachConsole
// ---------------------------------------------------------------------------

fn dispatch_attach_console(
    sock: UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    mode: ConsoleMode,
) {
    if let Err(msg) = validate_l2cpu(l2cpu_idx) {
        reply_err(&sock, msg);
        return;
    }

    let (daemon_end, client_end) = match UnixStream::pair() {
        Ok(p) => p,
        Err(e) => {
            reply_err(&sock, format!("socketpair: {}", e));
            return;
        }
    };

    // Hub writes via MSG_DONTWAIT so the socket can stay in blocking mode
    // for the reader thread. Clone the fd for the reader; the hub owns
    // `daemon_end` for fan-out.
    let daemon_read = match daemon_end.try_clone() {
        Ok(c) => c,
        Err(e) => {
            reply_err(&sock, format!("try_clone: {}", e));
            return;
        }
    };

    // Grab everything we need from the slot under the mutex, then release it
    // before doing any IO so other client handlers don't stall.
    let (hub, input_tx) = {
        let slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
        let slot = match slot_guard.as_ref() {
            Some(s) => s,
            None => {
                reply_err(&sock, format!("l2cpu {} is not booted", l2cpu_idx));
                return;
            }
        };
        (slot.console_hub.clone(), slot.console_input_tx.clone())
    };

    let (res, scrollback) = hub.attach(daemon_end, mode);
    if !res.demoted.is_empty() {
        dlog!(
            "[daemon] l2cpu {} console takeover demoted {:?}",
            l2cpu_idx, res.demoted
        );
    }

    // Reply with attached + scrollback size, then send the console fd via
    // SCM_RIGHTS. Order matters: the client reads the Attached response,
    // then the fd, then starts pumping bytes on the fd.
    if let Err(e) = write_frame(
        &sock,
        &Response::Attached {
            scrollback_bytes: res.scrollback_bytes,
        },
    ) {
        dlog!("[daemon] attach write response: {}", e);
        hub.detach(res.id);
        return;
    }
    if let Err(e) = send_fd(&sock, client_end.as_raw_fd()) {
        dlog!("[daemon] send_fd: {}", e);
        hub.detach(res.id);
        return;
    }
    drop(client_end);

    // Replay scrollback over `daemon_read` (blocking writes — 64 KiB fits
    // under SO_SNDBUF so this returns quickly without stalling the chip).
    if let Err(e) = write_scrollback(&daemon_read, &scrollback) {
        dlog!("[daemon] scrollback replay failed: {}", e);
        hub.detach(res.id);
        return;
    }

    thread::spawn(move || client_reader_main(daemon_read, res.id, hub, input_tx));
}

/// Blocking write of scrollback bytes. Loops only on EINTR; returns any
/// other error to the caller (who will detach the client).
fn write_scrollback(mut sock: &UnixStream, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    sock.write_all(bytes)
}

/// Per-client reader: blocks on `sock`, forwards bytes to `input_tx` whenever
/// this client is the writer. Terminates on EOF or hub-driven drop.
fn client_reader_main(
    sock: UnixStream,
    id: u64,
    hub: Arc<ConsoleHub>,
    input_tx: mpsc::Sender<u8>,
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

fn dispatch_add_disk(sock: &UnixStream, state: &Arc<DaemonState>, l2cpu_idx: u8, path: String) {
    if let Err(msg) = validate_l2cpu(l2cpu_idx) {
        reply_err(sock, msg);
        return;
    }
    let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
    let slot = match slot_guard.as_mut() {
        Some(s) => s,
        None => {
            reply_err(sock, format!("l2cpu {} is not booted", l2cpu_idx));
            return;
        }
    };
    if !slot.disks.is_empty() {
        // Phase A: one disk per L2CPU. Phase B+: multi-disk with indexed MMIO.
        reply_err(sock, "a disk is already attached");
        return;
    }

    // Pre-check the image is openable before spawning the worker. Without
    // this, a bad path (e.g. relative path against daemon's cwd=/) spawns
    // a worker that immediately exits, but `slot.disks` is already populated
    // with the dead handle — and subsequent `add-disk` calls then hit
    // "a disk is already attached". Failing fast here keeps the slot clean
    // and returns the real error (ENOENT etc.) to the client.
    if let Err(e) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    {
        reply_err(sock, format!("cannot open disk image {}: {}", path, e));
        return;
    }

    let exit = Arc::new(AtomicBool::new(false));
    let l2cpu = slot.l2cpu.clone();
    let interrupt = slot.interrupt.clone();
    let exit_thread = exit.clone();
    let path_thread = path.clone();
    let t = thread::spawn(move || {
        block::disk_main(l2cpu, interrupt, DISK_INT, DISK_MMIO, path_thread, exit_thread);
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
}

fn dispatch_remove_disk(sock: &UnixStream, state: &Arc<DaemonState>, l2cpu_idx: u8) {
    dlog!("[remove_disk l2cpu {}] dispatch entry", l2cpu_idx);
    if let Err(msg) = validate_l2cpu(l2cpu_idx) {
        reply_err(sock, msg);
        return;
    }
    // Take the disks out under the lock, then release and join outside.
    // stop_and_join blocks until the worker's poll loop notices the exit
    // flag (~100 ms worst case); holding the state mutex for that long
    // would block every other RPC on other L2CPUs.
    let disks = {
        let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
        let slot = match slot_guard.as_mut() {
            Some(s) => s,
            None => {
                reply_err(sock, format!("l2cpu {} is not booted", l2cpu_idx));
                return;
            }
        };
        if slot.disks.is_empty() {
            reply_err(sock, "no disk attached");
            return;
        }
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
}

// ---------------------------------------------------------------------------
// AddNet
// ---------------------------------------------------------------------------

#[cfg(feature = "slirp")]
fn dispatch_add_net(
    sock: &UnixStream,
    state: &Arc<DaemonState>,
    l2cpu_idx: u8,
    _ssh_port: Option<u16>,
) {
    dlog!("[add_net l2cpu {}] dispatch entry", l2cpu_idx);
    if let Err(msg) = validate_l2cpu(l2cpu_idx) {
        reply_err(sock, msg);
        return;
    }
    let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
    let slot = match slot_guard.as_mut() {
        Some(s) => s,
        None => {
            reply_err(sock, format!("l2cpu {} is not booted", l2cpu_idx));
            return;
        }
    };
    if slot.net.is_some() {
        reply_err(sock, "network already attached");
        return;
    }

    let exit = Arc::new(AtomicBool::new(false));
    let l2cpu = slot.l2cpu.clone();
    let interrupt = slot.interrupt.clone();
    let exit_thread = exit.clone();
    let ssh_port = crate::regs::slirp::ssh_port(state.card, l2cpu_idx);
    dlog!(
        "[add_net l2cpu {}] spawning network worker thread (ssh_port={})",
        l2cpu_idx,
        ssh_port
    );
    let t = thread::spawn(move || {
        network::network_main(ssh_port, l2cpu, interrupt, NET_INT, NET_MMIO, exit_thread);
    });
    slot.net = Some(WorkerHandle {
        exit,
        thread: Some(t),
        description: format!("net l2cpu {}", l2cpu_idx),
    });
    dlog!("[add_net l2cpu {}] dispatch complete — replying ok", l2cpu_idx);
    reply_ok(sock);
}

#[cfg(not(feature = "slirp"))]
fn dispatch_add_net(
    sock: &UnixStream,
    _state: &Arc<DaemonState>,
    _l2cpu_idx: u8,
    _ssh_port: Option<u16>,
) {
    reply_err(sock, "daemon built without the slirp feature");
}

fn dispatch_remove_net(sock: &UnixStream, state: &Arc<DaemonState>, l2cpu_idx: u8) {
    dlog!("[remove_net l2cpu {}] dispatch entry", l2cpu_idx);
    if let Err(msg) = validate_l2cpu(l2cpu_idx) {
        reply_err(sock, msg);
        return;
    }
    // Take the net handle under the lock, join outside (same reasoning as
    // dispatch_remove_disk).
    let net = {
        let mut slot_guard = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
        let slot = match slot_guard.as_mut() {
            Some(s) => s,
            None => {
                reply_err(sock, format!("l2cpu {} is not booted", l2cpu_idx));
                return;
            }
        };
        match slot.net.take() {
            Some(n) => n,
            None => {
                reply_err(sock, "no net attached");
                return;
            }
        }
    };
    dlog!("[remove_net l2cpu {}] joining worker", l2cpu_idx);
    net.stop_and_join();
    dlog!("[remove_net l2cpu {}] done — replying ok", l2cpu_idx);
    reply_ok(sock);
}

// ---------------------------------------------------------------------------
// Stop / Shutdown
// ---------------------------------------------------------------------------

fn dispatch_stop(sock: &UnixStream, state: &Arc<DaemonState>, l2cpu_idx: u8) {
    dlog!("[stop l2cpu {}] dispatch_stop entry", l2cpu_idx);
    if let Err(msg) = validate_l2cpu(l2cpu_idx) {
        reply_err(sock, msg);
        return;
    }
    let taken = state.l2cpus[l2cpu_idx as usize].lock().unwrap().take();
    match taken {
        Some(slot) => {
            dlog!(
                "[stop l2cpu {}] slot taken; joining workers",
                l2cpu_idx
            );
            slot.shutdown();
            dlog!("[stop l2cpu {}] workers joined — replying ok", l2cpu_idx);
            reply_ok(sock);
        }
        None => {
            dlog!("[stop l2cpu {}] no slot present — replying err", l2cpu_idx);
            reply_err(sock, format!("l2cpu {} is not booted", l2cpu_idx));
        }
    }
}

fn dispatch_shutdown(sock: &UnixStream, state: &Arc<DaemonState>) {
    dlog!("[shutdown] dispatch_shutdown entry — setting shutdown flag");
    state.shutdown.store(true, Ordering::SeqCst);
    // We don't reach the `serve()` teardown until the accept loop notices the
    // flag, but the accept loop's sleep is 50 ms — client gets Ok promptly.
    reply_ok(sock);
}

