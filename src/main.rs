// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! tt-bh-linux — unified Rust binary for booting and managing Linux on
//! Tenstorrent Blackhole L2CPU (SiFive X280) RISC-V cores.

// Many items across the tree are scaffolding for partially-implemented
// subcommands (boot, kmd ioctls, etc.) — keep them compiled without noise.
#![allow(dead_code)]

mod boot;
mod chip;
mod clock;
mod console;
mod daemon;
mod fdt_ffi;
mod image;
mod kernel;
mod kmd;
mod l2cpu;
mod ramdisk;
#[cfg(feature = "slirp")]
mod slirp_ffi;
mod tlb;
mod virtio;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use clap::{Parser, Subcommand};

use virtio::block;
use virtio::interrupt::InterruptController;
#[cfg(feature = "slirp")]
use virtio::network;

#[derive(Parser)]
#[command(name = "tt-bh-linux")]
#[command(about = "Boot and manage Linux on Tenstorrent Blackhole L2CPU")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Tenstorrent device index
    #[arg(short = 't', long = "ttdevice", default_value_t = 0, global = true)]
    ttdevice: u32,

    /// L2CPU index (0-3)
    #[arg(short = 'l', long = "l2cpu", default_value_t = 0, global = true)]
    l2cpu: usize,

    /// Path to disk image (defaults to rootfs.ext4 if present)
    #[arg(short = 'd', long = "disk", global = true)]
    disk: Option<String>,

    /// Enable virtio-net (requires the slirp feature)
    #[arg(short = 'n', long = "network", global = true)]
    network: bool,

    /// Attach the interactive console (default)
    #[arg(long = "console", global = true, overrides_with = "no_console")]
    console: bool,

    /// Skip attaching the interactive console
    #[arg(long = "no-console", global = true, overrides_with = "console")]
    no_console: bool,

    /// Path to cloud-init image (optional)
    #[arg(short = 'c', long = "cloud-init", global = true)]
    cloud_init: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Boot an L2CPU via the daemon (starts the chip + guest; use `connect`
    /// afterwards to attach a terminal).
    Boot {
        /// Path to OpenSBI binary
        #[arg(long, default_value = "fw_jump.bin")]
        opensbi: String,
        /// Path to kernel Image
        #[arg(long, default_value = "Image")]
        kernel: String,
        /// Path to device tree blob
        #[arg(long, default_value = "blackhole-card.dtb")]
        dtb: String,
        /// Boot with an initramfs image instead of a virtio-block rootfs
        #[arg(long)]
        initramfs: Option<String>,
        /// Root device name passed to the kernel (ignored when --initramfs is set)
        #[arg(long, default_value = "vda")]
        root_device: String,
        /// Always do a full board-level PCIe link reset before booting, even
        /// if the target L2CPU is already in reset. Disrupts other L2CPUs on
        /// the same card (they see a PCIe blip), so by default we probe
        /// `L2CPU_RESET` first and only reset when necessary.
        #[arg(long)]
        force_reset_pcie: bool,
        /// If the daemon already has a live slot for this L2CPU, tear it
        /// down (stop workers, drop mmaps) before re-imaging. Without this,
        /// a duplicate `boot` returns an error and leaves the prior slot
        /// untouched. Use when you know you want to re-image a running
        /// core — e.g. switching rootfs without an explicit `stop` first.
        #[arg(long)]
        force: bool,
    },
    /// Attach a terminal to a booted L2CPU's console via the daemon.
    Connect {
        /// Console attach mode: ro | rw | takeover.
        #[arg(long, default_value = "rw")]
        mode: String,
    },
    /// Per-card daemon lifecycle.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Stop an L2CPU's device workers (chip stays up; warm-reattach works).
    Stop,
    /// Show daemon + per-L2CPU status.
    Status,
    /// Attach a disk image to a running L2CPU.
    AddDisk {
        /// Path to the disk image (.ext4 / .img).
        path: String,
    },
    /// Detach the disk from a running L2CPU. Joins the worker thread and
    /// releases the image file (Phase A: one disk per L2CPU, no selector).
    RemoveDisk,
    /// Attach virtio-net (slirp) to a running L2CPU.
    AddNet {
        /// SSH port to forward (for informational use; currently fixed in the daemon).
        #[arg(long)]
        ssh_port: Option<u16>,
    },
    /// Detach virtio-net from a running L2CPU. Drops libvdeslirp state
    /// (active TCP/NAT sessions on the guest will reset).
    RemoveNet,
    /// Manage disk images
    Image {
        #[command(subcommand)]
        action: ImageAction,
    },
    /// Manage kernel/firmware (fw_jump.bin + Image + DTB)
    Kernel {
        #[command(subcommand)]
        action: KernelAction,
    },
    /// Low-level diagnostic probes that bypass the daemon.
    Debug {
        #[command(subcommand)]
        action: DebugAction,
    },
    /// Manage ramdisk/initramfs images
    Ramdisk {
        #[command(subcommand)]
        action: RamdiskAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon (double-forks unless --foreground).
    Start {
        #[arg(long)]
        foreground: bool,
        /// Override the log file path (absolute or relative to cwd). Default
        /// is `$XDG_RUNTIME_DIR/tt-bh-linux/<card>/log` which lives on tmpfs
        /// and is lost on host crash — set this to a file in the project
        /// directory when you need post-mortem logs.
        #[arg(long)]
        log_file: Option<String>,
    },
    /// Stop the daemon: SIGTERM, 5s grace, SIGKILL; idempotent.
    Stop,
    /// Stop then start the daemon.
    Restart {
        #[arg(long)]
        foreground: bool,
        /// See `daemon start --log-file`.
        #[arg(long)]
        log_file: Option<String>,
    },
    /// Show daemon + per-L2CPU status.
    Status,
    /// Tail the daemon log.
    Logs {
        #[arg(long, default_value_t = 200)]
        lines: usize,
        #[arg(long)]
        no_follow: bool,
    },
}

#[derive(Subcommand)]
enum ImageAction {
    /// List available images for download
    #[command(alias = "list-available")]
    List,
    /// Show details about a specific image
    Info {
        /// Image name or alias
        name: String,
    },
    /// Download and prepare a disk image
    Pull {
        /// Image name or alias (e.g., "debian-13", "ubuntu", "fedora")
        name: String,
        /// Output path (default: images/<name>.ext4)
        #[arg(short, long)]
        output: Option<String>,
    },
}

#[derive(Subcommand)]
enum KernelAction {
    /// List available kernel/firmware versions
    List,
    /// Download kernel/firmware bundle (fw_jump.bin + Image + DTB)
    Pull {
        /// Kernel version (e.g., "0.10", "v0.9"); defaults to latest
        #[arg(short, long)]
        version: Option<String>,
        /// Output directory (default: current directory)
        #[arg(short, long)]
        output: Option<String>,
    },
}

#[derive(Subcommand)]
enum DebugAction {
    /// Read the L2CPU_RESET register (0x80030014) and print it.
    ReadResetReg,
    /// Call `boot::reset_x280` on the given L2CPU (OR-in bit idx+4, bracketed
    /// by PLL step 1750→200→1750). Safe only when the daemon is not running
    /// against this card. Use `--l2cpu N` / `-l N`.
    ResetX280,
    /// Clear bit idx+4 of L2CPU_RESET — puts the L2CPU *into* reset in-place
    /// (halts it). Sibling cores keep running. No PLL manipulation.
    AssertReset,
    /// Set bit idx+4 of L2CPU_RESET — releases the L2CPU from reset. Pure
    /// register write, no PLL manipulation. Useful to re-start a core that
    /// was held by `assert-reset`.
    DeassertReset,
}

#[derive(Subcommand)]
enum RamdiskAction {
    /// List available ramdisk/initramfs images
    List,
    /// Download a ramdisk/initramfs
    Pull {
        /// Ramdisk name or alias (e.g., "debian-13-netboot")
        name: String,
        /// Output path
        #[arg(short, long)]
        output: Option<String>,
    },
}

const DEFAULT_DISK_PATH: &str = "rootfs.ext4";

/// Resolve the disk path to serve to the guest.
///
/// If the user passed `--disk`, honor it as-is (failures surface to the user).
/// Otherwise fall back to [`DEFAULT_DISK_PATH`] only when it actually exists,
/// so `connect` stays quiet when no disk image is around.
fn resolve_disk_path(
    explicit: Option<String>,
    default_path: &str,
    default_exists: bool,
) -> Option<String> {
    match explicit {
        Some(p) => Some(p),
        None if default_exists => Some(default_path.to_string()),
        None => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_boot(
    ttdevice: u32,
    l2cpu_idx: usize,
    opensbi_path: &str,
    kernel_path: &str,
    dtb_path: &str,
    opensbi_offset: u64,
    kernel_offset: u64,
    dtb_offset: u64,
    initramfs_offset: u64,
    initramfs_path: Option<&str>,
    root_device: &str,
    force_reset_pcie: bool,
) -> std::io::Result<()> {
    if l2cpu_idx > 3 {
        return Err(std::io::Error::other("l2cpu must be one of 0,1,2,3"));
    }

    let starting_address = l2cpu::L2CPU_STARTING_ADDRESS[l2cpu_idx];
    let memory_size = l2cpu::L2CPU_MEMORY_SIZE[l2cpu_idx];
    let mem_end = starting_address + memory_size;

    let opensbi_addr = starting_address + opensbi_offset;
    let kernel_addr = starting_address + kernel_offset;
    let dtb_addr = starting_address + dtb_offset;
    let rootfs_addr = starting_address + initramfs_offset;

    eprintln!("[boot] tt device: /dev/tenstorrent/{}", ttdevice);
    eprintln!("[boot] L2CPU index: {}", l2cpu_idx);
    eprintln!(
        "[boot] L2CPU memory: start=0x{:x}, size=0x{:x}, end=0x{:x}",
        starting_address, memory_size, mem_end
    );
    eprintln!("[boot] load addresses:");
    eprintln!("[boot]   opensbi    @ 0x{:x} ({})", opensbi_addr, opensbi_path);
    eprintln!("[boot]   kernel     @ 0x{:x} ({})", kernel_addr, kernel_path);
    eprintln!("[boot]   dtb        @ 0x{:x} ({})", dtb_addr, dtb_path);
    if let Some(p) = initramfs_path {
        eprintln!("[boot]   initramfs  @ 0x{:x} ({})", rootfs_addr, p);
    }
    eprintln!(
        "[boot] root device: {} (ignored if initramfs is set)",
        root_device
    );
    eprintln!("[boot] force_reset_pcie: {}", force_reset_pcie);

    eprintln!("[boot] opening /dev/tenstorrent/{} to probe L2CPU state", ttdevice);
    let chip = chip::BootChip::new(ttdevice)
        .map_err(|e| std::io::Error::other(format!("open /dev/tenstorrent/{}: {}", ttdevice, e)))?;

    let running = boot::l2cpu_is_running(&chip, l2cpu_idx);
    let need_reset = force_reset_pcie || running;
    eprintln!(
        "[boot] L2CPU {} running={}, force_reset_pcie={} -> need_reset={}",
        l2cpu_idx, running, force_reset_pcie, need_reset
    );

    let chip = if need_reset {
        if running {
            eprintln!(
                "[boot] target L2CPU is running; full board reset is required \
                 (other L2CPUs on this card will see a PCIe blip)"
            );
        } else {
            eprintln!("[boot] --force-reset-pcie set; doing full board reset anyway");
        }
        // fd must be closed before the reset — PCI re-enumeration invalidates it.
        drop(chip);
        chip::reset_board(ttdevice)?;
        eprintln!("[boot] board reset complete; sleeping 1s for chip to re-initialize");
        std::thread::sleep(std::time::Duration::from_secs(1));
        eprintln!("[boot] reopening /dev/tenstorrent/{} post-reset", ttdevice);
        chip::BootChip::new(ttdevice)
            .map_err(|e| std::io::Error::other(format!("open /dev/tenstorrent/{}: {}", ttdevice, e)))?
    } else {
        eprintln!(
            "[boot] target L2CPU is held in reset; skipping board reset \
             (other L2CPUs on this card are untouched)"
        );
        chip
    };

    eprintln!("[boot] reading DTB from {}", dtb_path);
    let dtb_raw = boot::read_bin_file(std::path::Path::new(dtb_path))?;
    eprintln!("[boot] DTB read, {} bytes", dtb_raw.len());

    let boot_device = match initramfs_path {
        Some(path) => {
            let bytes = boot::read_bin_file(std::path::Path::new(path))?;
            let len = bytes.len() as u64;
            boot::BootDevice::Initramfs {
                addr: rootfs_addr,
                len,
            }
        }
        None => boot::BootDevice::Vda(root_device.to_string()),
    };
    let dtb_patched = boot::modify_dtb(&dtb_raw, &boot_device, starting_address, memory_size)
        .map_err(std::io::Error::other)?;

    let initramfs_path_buf = initramfs_path.map(std::path::PathBuf::from);
    eprintln!("[boot] loading L2CPU {} image via NOC tile writes", l2cpu_idx);
    boot::boot_l2cpu(
        &chip,
        l2cpu_idx,
        std::path::Path::new(opensbi_path),
        opensbi_addr,
        Some(std::path::Path::new(kernel_path)),
        kernel_addr,
        &dtb_patched,
        dtb_addr,
        initramfs_path_buf.as_deref(),
        rootfs_addr,
    )?;

    eprintln!("[boot] releasing L2CPU {} from reset", l2cpu_idx);
    boot::reset_x280(&chip, &[l2cpu_idx]);
    eprintln!("[boot] configuring L2 prefetchers for L2CPU {}", l2cpu_idx);
    boot::configure_prefetchers(&chip, l2cpu_idx);
    eprintln!("[boot] complete");
    Ok(())
}

fn run_connect(
    ttdevice: u32,
    l2cpu: usize,
    disk: Option<String>,
    network: bool,
    console_enabled: bool,
    cloud_init: Option<String>,
) {
    if l2cpu > 3 {
        eprintln!("l2cpu must be one of 0,1,2,3");
        std::process::exit(1);
    }

    let disk_path = resolve_disk_path(
        disk,
        DEFAULT_DISK_PATH,
        std::path::Path::new(DEFAULT_DISK_PATH).exists(),
    );

    let exit_flag = Arc::new(AtomicBool::new(false));

    // Set up SIGINT/SIGTERM handler
    ctrlc_setup(&exit_flag);

    // One L2Cpu per L2CPU, shared across all worker threads via Arc. This
    // holds the two persistent 4 GB TLB windows once instead of per worker,
    // so a single `connect` costs 2 × 4 GB TLB slots total (vs. 6 × 4 GB
    // when console/disk/net each had their own L2Cpu).
    let l2cpu_arc = Arc::new(
        l2cpu::L2Cpu::new(l2cpu, ttdevice)
            .expect("failed to create L2CPU"),
    );

    // PLIC register at 0x2FF10000 + 0x404 — carved from the shared L2Cpu.
    let interrupt_ctl = {
        let window = l2cpu_arc
            .get_persistent_2m_window(0x2FF10000 + 0x404)
            .expect("failed to create interrupt window");
        Arc::new(InterruptController::new(window))
    };

    let mut threads = Vec::new();

    // Console thread (default; suppress with --no-console)
    if console_enabled {
        let exit_flag = exit_flag.clone();
        let l2cpu_arc = l2cpu_arc.clone();
        threads.push(thread::spawn(move || {
            console::console_main(l2cpu_arc, exit_flag);
        }));
    }

    // Disk thread (only if we have an image to serve)
    if let Some(disk_path) = disk_path {
        let exit_flag = exit_flag.clone();
        let interrupt_ctl = interrupt_ctl.clone();
        let l2cpu_arc = l2cpu_arc.clone();
        threads.push(thread::spawn(move || {
            block::disk_main(
                l2cpu_arc,
                interrupt_ctl,
                33,
                2 * 1024 * 1024,
                disk_path,
                exit_flag,
            );
        }));
    }

    // Network thread (requires --network and slirp feature)
    #[cfg(feature = "slirp")]
    if network {
        let exit_flag = exit_flag.clone();
        let interrupt_ctl = interrupt_ctl.clone();
        let l2cpu_arc = l2cpu_arc.clone();
        threads.push(thread::spawn(move || {
            network::network_main(
                ttdevice,
                l2cpu_arc,
                interrupt_ctl,
                32,
                4 * 1024 * 1024,
                exit_flag,
            );
        }));
    }
    #[cfg(not(feature = "slirp"))]
    if network {
        eprintln!("--network requested but binary was built without the slirp feature");
    }

    // Cloud-init disk thread (optional)
    if let Some(ci_path) = cloud_init {
        let exit_flag = exit_flag.clone();
        let interrupt_ctl = interrupt_ctl.clone();
        let l2cpu_arc = l2cpu_arc.clone();
        threads.push(thread::spawn(move || {
            block::disk_main(
                l2cpu_arc,
                interrupt_ctl,
                31,
                6 * 1024 * 1024,
                ci_path,
                exit_flag,
            );
        }));
    }

    for t in threads {
        let _ = t.join();
    }
}

/// Global exit flag set by the signal handler. The Arc<AtomicBool> passed to
/// threads points to this same static, avoiding Arc::into_raw leaks.
static GLOBAL_EXIT: AtomicBool = AtomicBool::new(false);

fn ctrlc_setup(exit_flag: &Arc<AtomicBool>) {
    // Wire the Arc to point at the same static (they share the same AtomicBool
    // value via the signal handler writing GLOBAL_EXIT, and threads checking
    // their Arc clone). We store the Arc's pointer so the handler can set it.
    EXIT_FLAG_PTR.store(Arc::as_ptr(exit_flag) as usize, Ordering::SeqCst);

    // Use sigaction instead of signal — signal() has undefined behavior in
    // multithreaded programs on some platforms and may reset to SIG_DFL.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

static EXIT_FLAG_PTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

extern "C" fn signal_handler(_sig: libc::c_int) {
    // Set the global flag
    GLOBAL_EXIT.store(true, Ordering::SeqCst);
    // Also set the Arc's AtomicBool so threads see it
    let ptr = EXIT_FLAG_PTR.load(Ordering::SeqCst);
    if ptr != 0 {
        let flag = unsafe { &*(ptr as *const AtomicBool) };
        flag.store(true, Ordering::SeqCst);
    }
}

fn main() -> std::process::ExitCode {
    // Ignore SIGPIPE globally — affects all subcommands (network slirp,
    // wget subprocesses, piped stdout, etc.)
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let cli = Cli::parse();

    let res: std::io::Result<()> = (|| match cli.command {
        Some(Commands::Boot {
            opensbi,
            kernel,
            dtb,
            initramfs,
            root_device,
            force_reset_pcie,
            force,
        }) => {
            let disk = resolve_disk_path(
                cli.disk,
                DEFAULT_DISK_PATH,
                std::path::Path::new(DEFAULT_DISK_PATH).exists(),
            );
            run_boot_client(
                cli.ttdevice,
                cli.l2cpu as u8,
                opensbi,
                kernel,
                dtb,
                initramfs,
                root_device,
                force_reset_pcie,
                disk,
                cli.network,
                force,
            )
        }
        Some(Commands::Connect { mode }) => {
            let pmode = parse_console_mode(&mode)?;
            run_connect_client(cli.ttdevice, cli.l2cpu as u8, pmode)
        }
        Some(Commands::Stop) => {
            let mut sock = daemon::client::connect(cli.ttdevice)?;
            daemon::client::stop_l2cpu(&mut sock, cli.l2cpu as u8)
        }
        Some(Commands::Status) => daemon::runner::status(cli.ttdevice),
        Some(Commands::AddDisk { path }) => {
            // Canonicalize client-side — daemon runs with cwd=/ after
            // double-fork, so relative paths from the user's shell would
            // resolve against the wrong base otherwise.
            let path = absolutize(&path)?;
            let mut sock = daemon::client::connect(cli.ttdevice)?;
            daemon::client::add_disk(&mut sock, cli.l2cpu as u8, path)
        }
        Some(Commands::RemoveDisk) => {
            let mut sock = daemon::client::connect(cli.ttdevice)?;
            daemon::client::remove_disk(&mut sock, cli.l2cpu as u8)
        }
        Some(Commands::AddNet { ssh_port }) => {
            let mut sock = daemon::client::connect(cli.ttdevice)?;
            daemon::client::add_net(&mut sock, cli.l2cpu as u8, ssh_port)
        }
        Some(Commands::RemoveNet) => {
            let mut sock = daemon::client::connect(cli.ttdevice)?;
            daemon::client::remove_net(&mut sock, cli.l2cpu as u8)
        }
        Some(Commands::Daemon { action }) => run_daemon_cmd(cli.ttdevice, action),
        Some(Commands::Image { action }) => {
            match action {
                ImageAction::List => image::cmd_list_available(),
                ImageAction::Info { name } => image::cmd_image_info(&name),
                ImageAction::Pull { name, output } => {
                    image::cmd_pull(&name, output.as_deref());
                }
            }
            Ok(())
        }
        Some(Commands::Kernel { action }) => {
            match action {
                KernelAction::List => kernel::cmd_list(),
                KernelAction::Pull { version, output } => {
                    kernel::cmd_pull(version.as_deref(), output.as_deref());
                }
            }
            Ok(())
        }
        Some(Commands::Ramdisk { action }) => {
            match action {
                RamdiskAction::List => ramdisk::cmd_list(),
                RamdiskAction::Pull { name, output } => {
                    ramdisk::cmd_pull(&name, output.as_deref());
                }
            }
            Ok(())
        }
        Some(Commands::Debug { action }) => run_debug_cmd(cli.ttdevice, cli.l2cpu, action),
        None => {
            // Bare invocation → attach console in rw mode, same as `connect`.
            run_connect_client(
                cli.ttdevice,
                cli.l2cpu as u8,
                daemon::protocol::ConsoleMode::Rw,
            )
        }
    })();

    match res {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", e);
            std::process::ExitCode::FAILURE
        }
    }
}

fn parse_console_mode(s: &str) -> std::io::Result<daemon::protocol::ConsoleMode> {
    match s {
        "ro" => Ok(daemon::protocol::ConsoleMode::Ro),
        "rw" => Ok(daemon::protocol::ConsoleMode::Rw),
        "takeover" => Ok(daemon::protocol::ConsoleMode::Takeover),
        other => Err(std::io::Error::other(format!(
            "invalid --mode {}; expected ro|rw|takeover",
            other
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_boot_client(
    card: u32,
    l2cpu: u8,
    opensbi: String,
    kernel: String,
    dtb: String,
    initramfs: Option<String>,
    root_device: String,
    force_reset_pcie: bool,
    disk: Option<String>,
    network: bool,
    force: bool,
) -> std::io::Result<()> {
    // Bundle disk + network into the Boot RPC so the virtio workers come up
    // together with the L2CPU reset release. The guest kernel hits its VFS
    // rootfs mount at ~0.137s and doesn't retry — issuing add-disk as a
    // separate RPC loses that race.
    //
    // Paths are canonicalized here (client side) because the daemon runs
    // from cwd=/, so relative paths from the user's shell wouldn't resolve.
    let opensbi = absolutize(&opensbi)?;
    let kernel = absolutize(&kernel)?;
    let dtb = absolutize(&dtb)?;
    let initramfs = initramfs.map(|p| absolutize(&p)).transpose()?;
    let disk = disk.map(|p| absolutize(&p)).transpose()?;
    let mut sock = daemon::client::connect(card)?;
    daemon::client::boot(
        &mut sock,
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
    )
}

fn absolutize(path: &str) -> std::io::Result<String> {
    let p = std::path::Path::new(path);
    let abs = std::fs::canonicalize(p)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {}", path, e)))?;
    abs.into_os_string()
        .into_string()
        .map_err(|_| std::io::Error::other(format!("non-UTF-8 path: {}", path)))
}

fn run_connect_client(
    card: u32,
    l2cpu: u8,
    mode: daemon::protocol::ConsoleMode,
) -> std::io::Result<()> {
    let mut sock = daemon::client::connect(card)?;
    let (scrollback_bytes, fd) = daemon::client::attach_console(&mut sock, l2cpu, mode)?;
    eprintln!(
        "[connect] attached l2cpu {} ({} bytes scrollback)",
        l2cpu, scrollback_bytes
    );
    let exit = Arc::new(AtomicBool::new(false));
    daemon::terminal::pump(fd, exit)?;
    Ok(())
}

fn run_debug_cmd(card: u32, l2cpu: usize, action: DebugAction) -> std::io::Result<()> {
    // Debug ops bypass the daemon and write the chip directly, so if the
    // daemon is up for this card it has no visibility into what we're
    // doing. That's exactly the silent-state-divergence footgun that
    // crashed the host on 2026-04-24: a CLI `assert-reset` (or similar)
    // put L2CPU 0 back into reset while the daemon still had a live slot
    // pointing at mmaps of that core. Refuse write ops when the daemon
    // is up; read ops warn and proceed.
    let daemon_up = daemon::lifetime::is_running(card);
    let writes_chip = !matches!(action, DebugAction::ReadResetReg);
    if daemon_up && writes_chip {
        return Err(std::io::Error::other(format!(
            "daemon is running for card {} — refusing to write chip state from outside the daemon \
             (stop the daemon first with `tt-bh-linux daemon stop`, then retry)",
            card
        )));
    }
    if daemon_up {
        eprintln!(
            "[debug] warning: daemon is running for card {} — read is racy with daemon's own ops",
            card
        );
    }

    let chip = chip::BootChip::new(card)
        .map_err(|e| std::io::Error::other(format!("open /dev/tenstorrent/{}: {}", card, e)))?;
    match action {
        DebugAction::ReadResetReg => {
            use boot::AxiAccess;
            let reg = 0x80030014u64;
            let val = chip.axi_read32(reg);
            println!("L2CPU_RESET@0x{:x} = {:#010x}", reg, val);
            for i in 0..4 {
                let bit = (val >> (i + 4)) & 1;
                println!("  bit {} (L2CPU {} release): {}", i + 4, i, bit);
            }
            Ok(())
        }
        DebugAction::ResetX280 => {
            if l2cpu > 3 {
                return Err(std::io::Error::other("l2cpu must be 0..3"));
            }
            eprintln!(
                "[debug] invoking boot::reset_x280 on L2CPU {} (PLL step + OR-in bit {})",
                l2cpu,
                l2cpu + 4
            );
            boot::reset_x280(&chip, &[l2cpu]);
            eprintln!("[debug] reset_x280 returned without panic");
            Ok(())
        }
        DebugAction::AssertReset => toggle_reset_bit(&chip, l2cpu, false),
        DebugAction::DeassertReset => toggle_reset_bit(&chip, l2cpu, true),
    }
}

fn toggle_reset_bit(chip: &chip::BootChip, l2cpu: usize, release: bool) -> std::io::Result<()> {
    use boot::AxiAccess;
    if l2cpu > 3 {
        return Err(std::io::Error::other("l2cpu must be 0..3"));
    }
    let reg: u64 = 0x80030014;
    let bit = 1u32 << (l2cpu + 4);
    let before = chip.axi_read32(reg);
    let after = if release { before | bit } else { before & !bit };
    eprintln!(
        "[debug] L2CPU_RESET@0x{:x}: {:#010x} -> {:#010x} ({} bit {} for L2CPU {})",
        reg,
        before,
        after,
        if release { "setting" } else { "clearing" },
        l2cpu + 4,
        l2cpu
    );
    chip.axi_write32(reg, after);
    let readback = chip.axi_read32(reg);
    eprintln!("[debug] readback: {:#010x}", readback);
    Ok(())
}

fn run_daemon_cmd(card: u32, action: DaemonAction) -> std::io::Result<()> {
    match action {
        DaemonAction::Start {
            foreground,
            log_file,
        } => daemon::runner::start(daemon::runner::StartOpts {
            card,
            foreground,
            log_file: log_file.map(std::path::PathBuf::from),
        }),
        DaemonAction::Stop => daemon::runner::stop(card),
        DaemonAction::Restart {
            foreground,
            log_file,
        } => daemon::runner::restart(card, foreground, log_file.map(std::path::PathBuf::from)),
        DaemonAction::Status => daemon::runner::status(card),
        DaemonAction::Logs { lines, no_follow } => daemon::runner::logs(daemon::runner::LogsOpts {
            card,
            follow: !no_follow,
            lines,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("clap failed to parse")
    }

    // --- resolve_disk_path ---------------------------------------------------

    #[test]
    fn disk_explicit_path_is_honored_even_if_default_absent() {
        let got = resolve_disk_path(Some("/tmp/custom.ext4".into()), "rootfs.ext4", false);
        assert_eq!(got.as_deref(), Some("/tmp/custom.ext4"));
    }

    #[test]
    fn disk_explicit_path_is_honored_even_if_default_present() {
        let got = resolve_disk_path(Some("/tmp/custom.ext4".into()), "rootfs.ext4", true);
        assert_eq!(got.as_deref(), Some("/tmp/custom.ext4"));
    }

    #[test]
    fn disk_default_used_when_present_and_unspecified() {
        let got = resolve_disk_path(None, "rootfs.ext4", true);
        assert_eq!(got.as_deref(), Some("rootfs.ext4"));
    }

    #[test]
    fn disk_skipped_when_default_missing_and_unspecified() {
        let got = resolve_disk_path(None, "rootfs.ext4", false);
        assert_eq!(got, None);
    }

    // --- CLI parsing: defaults -----------------------------------------------

    #[test]
    fn cli_defaults_leave_disk_network_console_off_off_on() {
        let cli = parse(&["tt-bh-linux", "connect"]);
        assert_eq!(cli.disk, None);
        assert!(!cli.network);
        // `console_enabled` is computed as `!no_console`; defaults to true.
        assert!(!cli.no_console);
    }

    // --- --disk / -d ---------------------------------------------------------

    #[test]
    fn cli_disk_long_form_captures_path() {
        let cli = parse(&["tt-bh-linux", "connect", "--disk", "/path/to/img.ext4"]);
        assert_eq!(cli.disk.as_deref(), Some("/path/to/img.ext4"));
    }

    #[test]
    fn cli_disk_short_form_captures_path() {
        let cli = parse(&["tt-bh-linux", "connect", "-d", "img.ext4"]);
        assert_eq!(cli.disk.as_deref(), Some("img.ext4"));
    }

    // --- --network -----------------------------------------------------------

    #[test]
    fn cli_network_flag_opts_in() {
        let cli = parse(&["tt-bh-linux", "connect", "--network"]);
        assert!(cli.network);
    }

    #[test]
    fn cli_network_short_form_opts_in() {
        let cli = parse(&["tt-bh-linux", "connect", "-n"]);
        assert!(cli.network);
    }

    // --- --console / --no-console --------------------------------------------

    #[test]
    fn cli_no_console_disables_console() {
        let cli = parse(&["tt-bh-linux", "connect", "--no-console"]);
        assert!(cli.no_console);
    }

    #[test]
    fn cli_explicit_console_flag_keeps_console_on() {
        let cli = parse(&["tt-bh-linux", "connect", "--console"]);
        assert!(!cli.no_console);
    }

    #[test]
    fn cli_console_then_no_console_last_wins_off() {
        let cli = parse(&["tt-bh-linux", "connect", "--console", "--no-console"]);
        assert!(cli.no_console);
    }

    #[test]
    fn cli_no_console_then_console_last_wins_on() {
        let cli = parse(&["tt-bh-linux", "connect", "--no-console", "--console"]);
        assert!(!cli.no_console);
    }

    // --- global flags work on other subcommands & bare invocation ------------

    #[test]
    fn cli_bare_invocation_parses_like_connect() {
        // When no subcommand is given, `main` falls through to run_connect;
        // the global flags must still apply.
        let cli = parse(&["tt-bh-linux", "--no-console", "-n", "-d", "x.ext4"]);
        assert!(cli.command.is_none());
        assert!(cli.no_console);
        assert!(cli.network);
        assert_eq!(cli.disk.as_deref(), Some("x.ext4"));
    }
}
