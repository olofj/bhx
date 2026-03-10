// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! tt-bh-linux — unified Rust binary for booting and managing Linux on
//! Tenstorrent Blackhole L2CPU (SiFive X280) RISC-V cores.

#[allow(dead_code)]
mod boot;
mod clock;
mod console;
mod image;
mod kernel;
mod kmd;
mod l2cpu;
mod ramdisk;
mod slirp_ffi;
mod tlb;
mod virtio;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use clap::{Parser, Subcommand};

use virtio::block;
use virtio::interrupt::InterruptController;
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

    /// Path to disk image
    #[arg(short = 'd', long = "disk", default_value = "rootfs.ext4", global = true)]
    disk: String,

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

fn run_connect(ttdevice: u32, l2cpu: usize, disk: String, cloud_init: Option<String>) {
    if l2cpu > 3 {
        eprintln!("l2cpu must be one of 0,1,2,3");
        std::process::exit(1);
    }

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

    // Console thread
    {
        let exit_flag = exit_flag.clone();
        threads.push(thread::spawn(move || {
            console::console_main(ttdevice, l2cpu, exit_flag);
        }));
    }

    // Disk thread
    {
        let exit_flag = exit_flag.clone();
        let interrupt_ctl = interrupt_ctl.clone();
        let disk_path = disk.clone();
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

    // Network thread
    {
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
    unsafe {
        EXIT_FLAG_PTR.store(
            Arc::as_ptr(exit_flag) as usize,
            Ordering::SeqCst,
        );
    }

    // Use sigaction instead of signal — signal() has undefined behavior in
    // multithreaded programs on some platforms and may reset to SIG_DFL.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = signal_handler as usize;
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

fn main() {
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
            std::process::exit(1);
        }
        Some(Commands::Connect) => {
            run_connect(cli.ttdevice, cli.l2cpu, cli.disk, cli.cloud_init);
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
            run_connect(cli.ttdevice, cli.l2cpu, cli.disk, cli.cloud_init);
        }
    }
}
