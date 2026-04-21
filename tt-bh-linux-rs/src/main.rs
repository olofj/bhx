// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! tt-bh-linux — unified Rust binary for booting and managing Linux on
//! Tenstorrent Blackhole L2CPU (SiFive X280) RISC-V cores.

// Many items across the tree are scaffolding for partially-implemented
// subcommands (boot, kmd ioctls, etc.) — keep them compiled without noise.
#![allow(dead_code)]

mod boot;
mod clock;
mod console;
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

    // Create shared interrupt controller
    // PLIC register at 0x2FF10000 + 0x404
    let interrupt_ctl = {
        let temp_l2cpu = l2cpu::L2Cpu::new(l2cpu, ttdevice)
            .expect("failed to create L2CPU for interrupt controller");
        let window = temp_l2cpu
            .get_persistent_2m_window(0x2FF10000 + 0x404)
            .expect("failed to create interrupt window");
        Arc::new(InterruptController::new(window))
    };

    let mut threads = Vec::new();

    // Console thread (default; suppress with --no-console)
    if console_enabled {
        let exit_flag = exit_flag.clone();
        threads.push(thread::spawn(move || {
            console::console_main(ttdevice, l2cpu, exit_flag);
        }));
    }

    // Disk thread (only if we have an image to serve)
    if let Some(disk_path) = disk_path {
        let exit_flag = exit_flag.clone();
        let interrupt_ctl = interrupt_ctl.clone();
        threads.push(thread::spawn(move || {
            block::disk_main(
                ttdevice,
                l2cpu,
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
        threads.push(thread::spawn(move || {
            network::network_main(
                ttdevice,
                l2cpu,
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
        threads.push(thread::spawn(move || {
            block::disk_main(
                ttdevice,
                l2cpu,
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
        Some(Commands::Boot { opensbi, kernel, dtb }) => {
            eprintln!("Boot command requires luwen crate integration.");
            eprintln!("Use boot.py for now, then run: tt-bh-linux connect");
            eprintln!("  opensbi: {}, kernel: {}, dtb: {}", opensbi, kernel, dtb);
            return std::process::ExitCode::FAILURE;
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
