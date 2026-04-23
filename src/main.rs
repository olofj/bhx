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
    /// Boot sequence + console/disk/net threads
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
        /// Offset (relative to the L2CPU starting address) to place OpenSBI
        #[arg(long, default_value_t = 0x0)]
        opensbi_offset: u64,
        /// Offset (relative to the L2CPU starting address) to place the kernel
        #[arg(long, default_value_t = 0x200000)]
        kernel_offset: u64,
        /// Offset (relative to the L2CPU starting address) to place the DTB
        #[arg(long, default_value_t = 0x100000)]
        dtb_offset: u64,
        /// Offset (relative to the L2CPU starting address) to place the initramfs
        #[arg(long, default_value_t = 0xb5000000)]
        initramfs_offset: u64,
        /// Boot with an initramfs image instead of a virtio-block rootfs
        #[arg(long)]
        initramfs: Option<String>,
        /// Root device name passed to the kernel (ignored when --initramfs is set)
        #[arg(long, default_value = "vda")]
        root_device: String,
        /// Always do a full board-level PCIe link reset before booting, even
        /// if the target L2CPU is already in reset. Disrupts other L2CPUs on
        /// the same card (they see a PCIe blip), so by default we probe
        /// `L2CPU_RESET` first and only reset when necessary. Use this as a
        /// recovery escape hatch when a previous boot left the card in a
        /// wedged state the probe can't detect.
        #[arg(long)]
        force_reset_pcie: bool,
        /// After the boot sequence, do not attach the console/disk/net threads
        /// (equivalent to exiting like boot.py does).
        #[arg(long)]
        no_connect: bool,
    },
    /// Console/disk/net threads only (chip already booted)
    Connect,
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
    /// Manage ramdisk/initramfs images
    Ramdisk {
        #[command(subcommand)]
        action: RamdiskAction,
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

    match cli.command {
        Some(Commands::Boot {
            opensbi,
            kernel,
            dtb,
            opensbi_offset,
            kernel_offset,
            dtb_offset,
            initramfs_offset,
            initramfs,
            root_device,
            force_reset_pcie,
            no_connect,
        }) => {
            if let Err(e) = run_boot(
                cli.ttdevice,
                cli.l2cpu,
                &opensbi,
                &kernel,
                &dtb,
                opensbi_offset,
                kernel_offset,
                dtb_offset,
                initramfs_offset,
                initramfs.as_deref(),
                &root_device,
                force_reset_pcie,
            ) {
                eprintln!("boot: {}", e);
                return std::process::ExitCode::FAILURE;
            }
            if !no_connect {
                run_connect(
                    cli.ttdevice,
                    cli.l2cpu,
                    cli.disk,
                    cli.network,
                    !cli.no_console,
                    cli.cloud_init,
                );
            }
        }
        Some(Commands::Connect) => {
            run_connect(
                cli.ttdevice,
                cli.l2cpu,
                cli.disk,
                cli.network,
                !cli.no_console,
                cli.cloud_init,
            );
        }
        Some(Commands::Image { action }) => {
            match action {
                ImageAction::List => image::cmd_list_available(),
                ImageAction::Info { name } => image::cmd_image_info(&name),
                ImageAction::Pull { name, output } => {
                    image::cmd_pull(&name, output.as_deref());
                }
            }
        }
        Some(Commands::Kernel { action }) => {
            match action {
                KernelAction::List => kernel::cmd_list(),
                KernelAction::Pull { version, output } => {
                    kernel::cmd_pull(version.as_deref(), output.as_deref());
                }
            }
        }
        Some(Commands::Ramdisk { action }) => {
            match action {
                RamdiskAction::List => ramdisk::cmd_list(),
                RamdiskAction::Pull { name, output } => {
                    ramdisk::cmd_pull(&name, output.as_deref());
                }
            }
        }
        None => {
            run_connect(
                cli.ttdevice,
                cli.l2cpu,
                cli.disk,
                cli.network,
                !cli.no_console,
                cli.cloud_init,
            );
        }
    }

    std::process::ExitCode::SUCCESS
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
