// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! Control-socket server: accepts clients, dispatches ops, holds L2CPU state.
//!
//! Each accepted client gets its own thread that reads one request frame,
//! dispatches it against [`DaemonState`], writes a response, and (for
//! `AttachConsole`) stays alive as the client's console reader.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use crate::boot;
use crate::chip;
use crate::daemon::chip_console;
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

/// Virtio MMIO offsets and interrupt numbers (match `run_connect`).
const DISK_INT: u32 = 33;
const DISK_MMIO: u64 = 2 * 1024 * 1024;
const NET_INT: u32 = 32;
const NET_MMIO: u64 = 4 * 1024 * 1024;

/// Run the daemon accept loop foreground-style. Returns on SIGTERM / SIGINT
/// once the shutdown flag has been tripped. Caller is responsible for
/// daemonization (double-fork) before calling this.
pub fn serve(card: u32, listener: UnixListener) -> io::Result<()> {
    let state = Arc::new(DaemonState::new(card));
    install_signal_handlers(state.shutdown.clone());

    listener.set_nonblocking(true)?;

    eprintln!("[daemon] accepting connections on card {}", card);
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
                eprintln!("[daemon] accept error: {}", e);
                thread::sleep(Duration::from_millis(200));
            }
        }
    }

    eprintln!("[daemon] shutdown flag set — tearing down L2CPU slots");
    for slot_mutex in state.l2cpus.iter() {
        if let Some(slot) = slot_mutex.lock().unwrap().take() {
            slot.shutdown();
        }
    }
    // Clean up socket file; pidfile flock is released when our guard drops in
    // the caller (`run_foreground`).
    let _ = std::fs::remove_file(lifetime::socket_path(card));
    eprintln!("[daemon] bye");
    Ok(())
}

/// Install handlers for SIGTERM / SIGINT that flip the daemon's shutdown flag.
fn install_signal_handlers(flag: Arc<AtomicBool>) {
    static FLAG_PTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    FLAG_PTR.store(Arc::as_ptr(&flag) as usize, Ordering::SeqCst);
    // Leak the Arc so the signal handler can reach it — the daemon runs for
    // the lifetime of the process anyway.
    std::mem::forget(flag);

    extern "C" fn handler(_sig: libc::c_int) {
        let ptr = FLAG_PTR.load(Ordering::SeqCst);
        if ptr != 0 {
            let f = unsafe { &*(ptr as *const AtomicBool) };
            f.store(true, Ordering::SeqCst);
        }
    }

    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

fn handle_client(mut sock: UnixStream, state: Arc<DaemonState>) {
    let req: Request = match read_frame(&mut sock) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[daemon] read request failed: {}", e);
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
        ),
        Request::AttachConsole { l2cpu, mode } => {
            dispatch_attach_console(sock, &state, l2cpu, mode)
        }
        Request::AddDisk { l2cpu, path } => dispatch_add_disk(&sock, &state, l2cpu, path),
        Request::AddNet { l2cpu, ssh_port } => dispatch_add_net(&sock, &state, l2cpu, ssh_port),
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

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

fn dispatch_status(mut sock: &UnixStream, state: &Arc<DaemonState>) {
    let mut l2cpus = Vec::new();
    for (idx, slot_mutex) in state.l2cpus.iter().enumerate() {
        let slot = slot_mutex.lock().unwrap();
        let (st, disk, net, clients) = match slot.as_ref() {
            None => (L2CpuState::Stopped, None, false, 0),
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
) {
    if l2cpu_idx >= 4 {
        reply_err(sock, "l2cpu must be 0..3");
        return;
    }
    {
        let slot = state.l2cpus[l2cpu_idx as usize].lock().unwrap();
        if slot.is_some() {
            reply_err(
                sock,
                format!("l2cpu {} is already booted; stop it first", l2cpu_idx),
            );
            return;
        }
    }

    if let Err(e) = run_boot_sequence(
        state.card,
        l2cpu_idx,
        opensbi,
        kernel,
        dtb,
        initramfs,
        root_device,
        force_reset_pcie,
    ) {
        reply_err(sock, format!("boot failed: {}", e));
        return;
    }

    match make_slot(state.card, l2cpu_idx) {
        Ok(slot) => {
            *state.l2cpus[l2cpu_idx as usize].lock().unwrap() = Some(slot);
            reply_ok(sock);
        }
        Err(e) => reply_err(sock, format!("post-boot L2Cpu init failed: {}", e)),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_boot_sequence(
    card: u32,
    l2cpu_idx: u8,
    opensbi: &str,
    kernel: &str,
    dtb: &str,
    initramfs: Option<&str>,
    root_device: &str,
    force_reset_pcie: bool,
) -> io::Result<()> {
    // Same sequence as `main::run_boot` but inlined here so the daemon owns
    // the boot path without going through CLI plumbing.
    let starting_address = crate::l2cpu::L2CPU_STARTING_ADDRESS[l2cpu_idx as usize];
    let memory_size = crate::l2cpu::L2CPU_MEMORY_SIZE[l2cpu_idx as usize];

    let opensbi_offset: u64 = 0x0;
    let kernel_offset: u64 = 0x200000;
    let dtb_offset: u64 = 0x100000;
    let initramfs_offset: u64 = 0xb5000000;

    let opensbi_addr = starting_address + opensbi_offset;
    let kernel_addr = starting_address + kernel_offset;
    let dtb_addr = starting_address + dtb_offset;
    let rootfs_addr = starting_address + initramfs_offset;

    let chip = chip::BootChip::new(card)
        .map_err(|e| io::Error::other(format!("open /dev/tenstorrent/{}: {}", card, e)))?;

    let running = boot::l2cpu_is_running(&chip, l2cpu_idx as usize);
    let need_reset = force_reset_pcie || running;

    let chip = if need_reset {
        drop(chip);
        chip::reset_board(card)?;
        std::thread::sleep(std::time::Duration::from_secs(1));
        chip::BootChip::new(card)
            .map_err(|e| io::Error::other(format!("reopen /dev/tenstorrent/{}: {}", card, e)))?
    } else {
        chip
    };

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
    let dtb_patched = boot::modify_dtb(&dtb_raw, &boot_device, starting_address, memory_size)
        .map_err(io::Error::other)?;

    let initramfs_pb = initramfs.map(std::path::PathBuf::from);
    boot::boot_l2cpu(
        &chip,
        l2cpu_idx as usize,
        Path::new(opensbi),
        opensbi_addr,
        Some(Path::new(kernel)),
        kernel_addr,
        &dtb_patched,
        dtb_addr,
        initramfs_pb.as_deref(),
        rootfs_addr,
    )?;

    boot::reset_x280(&chip, &[l2cpu_idx as usize]);
    boot::configure_prefetchers(&chip, l2cpu_idx as usize);
    Ok(())
}

/// Construct the `L2Cpu` + interrupt controller + console hub + chip console
/// worker for a freshly-booted L2CPU.
fn make_slot(card: u32, l2cpu_idx: u8) -> io::Result<L2CpuSlot> {
    let l2cpu = Arc::new(L2Cpu::new(l2cpu_idx as usize, card)?);
    let interrupt = {
        let window = l2cpu.get_persistent_2m_window(0x2FF10000 + 0x404)?;
        Arc::new(InterruptController::new(window))
    };
    let hub = Arc::new(ConsoleHub::new());

    let (input_tx, input_rx) = mpsc::channel::<u8>();
    let exit = Arc::new(AtomicBool::new(false));

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
    if l2cpu_idx >= 4 {
        reply_err(&sock, "l2cpu must be 0..3");
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
        eprintln!(
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
        eprintln!("[daemon] attach write response: {}", e);
        hub.detach(res.id);
        return;
    }
    if let Err(e) = send_fd(&sock, client_end.as_raw_fd()) {
        eprintln!("[daemon] send_fd: {}", e);
        hub.detach(res.id);
        return;
    }
    drop(client_end);

    // Replay scrollback over `daemon_read` (blocking writes — 64 KiB fits
    // under SO_SNDBUF so this returns quickly without stalling the chip).
    if let Err(e) = write_scrollback(&daemon_read, &scrollback) {
        eprintln!("[daemon] scrollback replay failed: {}", e);
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
    if l2cpu_idx >= 4 {
        reply_err(sock, "l2cpu must be 0..3");
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
    if l2cpu_idx >= 4 {
        reply_err(sock, "l2cpu must be 0..3");
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
    let card = state.card;
    let t = thread::spawn(move || {
        network::network_main(card, l2cpu, interrupt, NET_INT, NET_MMIO, exit_thread);
    });
    slot.net = Some(WorkerHandle {
        exit,
        thread: Some(t),
        description: format!("net l2cpu {}", l2cpu_idx),
    });
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

// ---------------------------------------------------------------------------
// Stop / Shutdown
// ---------------------------------------------------------------------------

fn dispatch_stop(sock: &UnixStream, state: &Arc<DaemonState>, l2cpu_idx: u8) {
    if l2cpu_idx >= 4 {
        reply_err(sock, "l2cpu must be 0..3");
        return;
    }
    let taken = state.l2cpus[l2cpu_idx as usize].lock().unwrap().take();
    match taken {
        Some(slot) => {
            slot.shutdown();
            reply_ok(sock);
        }
        None => reply_err(sock, format!("l2cpu {} is not booted", l2cpu_idx)),
    }
}

fn dispatch_shutdown(sock: &UnixStream, state: &Arc<DaemonState>) {
    state.shutdown.store(true, Ordering::SeqCst);
    // We don't reach the `serve()` teardown until the accept loop notices the
    // flag, but the accept loop's sleep is 50 ms — client gets Ok promptly.
    reply_ok(sock);
}

/// Helper: spawn the per-client console reader. The reader owns one half of
/// a cloned UnixStream (pointing at the same socketpair end the hub writes
/// into for fan-out); bytes read from the socket become chip RX pushes
/// whenever this client is the current writer.
#[allow(dead_code)]
fn spawn_client_reader(
    daemon_end: UnixStream,
    client_id: u64,
    hub: Arc<ConsoleHub>,
    input_tx: mpsc::Sender<u8>,
) {
    // TODO(phase-A.wire): currently unused — attach_console is still being
    // finished. Placeholder for the reader-thread design so the module
    // compiles while we iterate.
    drop(daemon_end);
    drop(hub);
    drop(input_tx);
    let _ = client_id;
}

/// Convenience: write a string line to a client socket (used by the client-
/// side CLI for error rendering). Exported to keep the module testable.
pub fn write_line(sock: &mut UnixStream, s: &str) -> io::Result<()> {
    sock.write_all(s.as_bytes())?;
    sock.write_all(b"\n")
}
