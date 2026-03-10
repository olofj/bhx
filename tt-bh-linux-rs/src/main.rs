// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

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

    // Ignore SIGPIPE
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let exit_flag = Arc::new(AtomicBool::new(false));

    // Set up SIGINT/SIGTERM handler
    let exit_flag_sig = exit_flag.clone();
    ctrlc_setup(exit_flag_sig);

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

fn ctrlc_setup(exit_flag: Arc<AtomicBool>) {
    // Simple SIGINT handler using a pipe trick
    let _ = std::thread::spawn(move || {
        // We can't easily install signal handlers in pure Rust without a crate,
        // so we rely on the terminal raw mode restoration in Drop and the
        // exit_flag being checked in all loops.
        // The console thread's Ctrl-A x provides the primary exit mechanism.
    });

    // Set up a basic signal handler for cleanup
    unsafe {
        // Store exit_flag pointer for signal handler
        EXIT_FLAG.store(
            Arc::into_raw(exit_flag) as *mut std::sync::atomic::AtomicBool as usize,
            Ordering::SeqCst,
        );
        libc::signal(libc::SIGINT, signal_handler as usize);
        libc::signal(libc::SIGTERM, signal_handler as usize);
    }
}

static EXIT_FLAG: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

extern "C" fn signal_handler(_sig: libc::c_int) {
    let ptr = EXIT_FLAG.load(Ordering::SeqCst);
    if ptr != 0 {
        let flag = unsafe { &*(ptr as *const AtomicBool) };
        flag.store(true, Ordering::SeqCst);
    }
}

fn main() {
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
