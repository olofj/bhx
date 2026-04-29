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
mod error;
mod fdt_ffi;
mod fetch;
mod image;
mod kernel;
mod kmd;
mod l2cpu;
mod ramdisk;
mod regs;
mod shared_chip;
#[cfg(feature = "slirp")]
mod slirp_ffi;
mod telemetry;
mod tensix;
mod tensix_data_plane;
mod tensix_engine;
mod tensix_proto;
mod tensix_tile;
mod tlb;
mod virtio;
mod virtio_engine;
mod x280_tlb;

// Re-export the structured error type at the crate root so call sites
// can write `crate::Result<T>` rather than `crate::error::Result<T>`.
pub use error::{Error, Result};

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use clap::{Parser, Subcommand};

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
        /// Path to a raw Linux Image (default; mutually exclusive with --uboot)
        #[arg(long, conflicts_with = "uboot")]
        kernel: Option<String>,
        /// Path to a U-Boot binary (S-mode payload). Mutually exclusive
        /// with --kernel. In this mode the daemon loads U-Boot at the
        /// kernel offset and skips initramfs preload — U-Boot reads
        /// the kernel + initrd from the attached --disk at runtime.
        /// See #44.
        #[arg(long, conflicts_with_all = ["kernel", "initramfs"])]
        uboot: Option<String>,
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
        /// Attach a virtio-console device alongside the boot. Stock distro
        /// kernels with `CONFIG_VIRTIO_CONSOLE` register this as `/dev/hvc0`
        /// and direct their console output through it instead of the
        /// OpenSBI debug UART (which often requires `CONFIG_HVC_RISCV_SBI`,
        /// not enabled in upstream-portable distro kernels). See #51.
        #[arg(long = "virtio-console")]
        virtio_console: bool,
        /// Skip attaching virtio-rng. By default the daemon brings up
        /// virtio-rng alongside the boot — U-Boot's EFI loader needs it
        /// to install `EFI_RNG_PROTOCOL`, which the AlmaLinux EFI shim
        /// queries during signature verification (without it the shim
        /// stalls before chainloading GRUB). It's also harmless on
        /// direct-kernel boots (extra thread, satisfies guest
        /// /dev/random). Pass this if you want to bisect a virtio-rng
        /// regression. See #62.
        #[arg(long = "no-virtio-rng")]
        no_virtio_rng: bool,
        /// Additional TCP port forwards as `HOST:GUEST` pairs, installed
        /// at boot time on top of the implicit SSH forward. Repeatable:
        /// `--fwd 5201:5201 --fwd 8080:80`. Same as `add-net --fwd`,
        /// but applied at cold-boot so the guest's virtio_net binding
        /// never has to migrate to a hot-added device — needed for the
        /// net bench's ingress measurement against buildroot kernels
        /// that don't auto-rebind built-in virtio_net after teardown.
        #[arg(long = "fwd", value_parser = parse_fwd_pair)]
        fwd: Vec<(u16, u16)>,
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
        /// Override the host-side port forwarded to the guest's :22.
        /// Default is the formula-derived per-(card, l2cpu_idx) port —
        /// see `daemon ports` for the mapping.
        #[arg(long)]
        ssh_port: Option<u16>,
        /// Additional TCP port forwards as `HOST:GUEST` pairs.
        /// Repeatable: `--fwd 5201:5201 --fwd 8080:80`. Each adds a
        /// slirp `tcp_listen_add` on `127.0.0.1:HOST` forwarding to
        /// `10.0.2.15:GUEST`. The implicit SSH forward (above) stays
        /// in place; this is for everything else (iperf3 server,
        /// HTTP diagnostics, debugger over slirp, …).
        #[arg(long = "fwd", value_parser = parse_fwd_pair)]
        fwd: Vec<(u16, u16)>,
    },
    /// Detach virtio-net from a running L2CPU. Drops libvdeslirp state
    /// (active TCP/NAT sessions on the guest will reset).
    RemoveNet,
    /// Attach a virtio-console device to a running L2CPU (#51). Stock
    /// distro kernels with `CONFIG_VIRTIO_CONSOLE` register this as
    /// `/dev/hvc0`.
    AddConsole,
    /// Detach the virtio-console device from a running L2CPU. Joins the
    /// worker thread; any in-flight RX descriptors are dropped.
    RemoveConsole,
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
        /// Disable the seccomp + landlock sandbox. The sandbox is on
        /// by default — defense-in-depth so a daemon-side bug can't
        /// pivot to read arbitrary host files or open outbound
        /// connections. Pass this only when debugging the filter
        /// itself (e.g. tracking down which syscall is missing from
        /// the whitelist). Linux only; the flag is accepted but a
        /// no-op everywhere else. See docs/sandbox-syscalls.md.
        #[arg(long)]
        no_sandbox: bool,
        /// Bind a Prometheus-style HTTP exporter on
        /// `127.0.0.1:<port>` and serve `GET /metrics`. Loopback only.
        /// Off by default; pass an explicit port to enable. See
        /// `daemon::metrics`.
        #[arg(long)]
        metrics_port: Option<u16>,
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
        /// See `daemon start --no-sandbox`.
        #[arg(long)]
        no_sandbox: bool,
        /// See `daemon start --metrics-port`.
        #[arg(long)]
        metrics_port: Option<u16>,
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
    /// Print the per-L2CPU SSH-forward host ports for the given card.
    /// Probes each port to report whether it's currently bindable.
    /// Useful when `add-net` fails with "ssh-forward port N
    /// unavailable" — this command shows which ports are clear.
    Ports,
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
        /// Bypass the HTTP-conditional cache and always re-download.
        /// (The "image already exists" short-circuit on the converted
        /// .ext4 still applies — delete that file too if you want a
        /// truly clean fetch.)
        #[arg(long)]
        refetch: bool,
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
        /// Bypass the HTTP-conditional cache and always re-download.
        #[arg(long)]
        refetch: bool,
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
    /// Load the M1 hello-world BRISC firmware onto Tensix tile (x, y),
    /// release BRISC from soft-reset, poll L1 for the magic value
    /// and incrementing counter. PASS when the counter advances
    /// across `--duration` seconds. Issue #67.
    TensixHello {
        /// Tensix tile X coordinate (NoC0 logical). When omitted, the
        /// M2 picker chooses one based on the chip's harvest mask.
        /// Functional workers on Blackhole live in x=1..7 and 10..16.
        #[arg(long)]
        x: Option<u16>,
        /// Tensix tile Y coordinate. Same defaulting behavior as `--x`.
        /// Functional workers on Blackhole live in y=2..11.
        #[arg(long)]
        y: Option<u16>,
        /// Number of seconds to poll the counter for. The host samples
        /// once per second.
        #[arg(long, default_value_t = 5)]
        duration: u32,
    },
    /// Dump the ARC firmware telemetry table — prints the three M2
    /// picker inputs (HarvestingState, EnabledTensixCol,
    /// NocTranslation) and the decoded set of working Tensix tile
    /// coordinates. Useful for confirming the picker on a new chip
    /// or diagnosing harvest-related anomalies. Issues #68, #75.
    TelemetryDump {
        /// Print every telemetry tag entry (~60 rows) instead of just
        /// the picker-relevant subset.
        #[arg(long)]
        all_tags: bool,
    },
    /// Print the tile coordinate the picker would reserve for the
    /// virtio-mmio engine on this chip. Pure decode — does not touch
    /// the tile. Issue #68.
    PickTile,
    /// Load the M3 virtio-mmio engine firmware onto a Tensix tile,
    /// release BRISC, and smoke-test the register file: verify the
    /// static MAGIC / VERSION / DEVICE_ID across all 16 slots, drive
    /// a STATUS write to confirm the state machine, drive a
    /// QUEUE_SEL change to confirm the multiplexer, and read the
    /// stats page. Bypasses the daemon. Issue #69.
    TensixVirtio {
        /// Tensix tile X coordinate (NoC0 logical). Defaults to the
        /// M2 picker output.
        #[arg(long)]
        x: Option<u16>,
        /// Tensix tile Y coordinate (NoC0 logical). Same defaulting
        /// as `--x`.
        #[arg(long)]
        y: Option<u16>,
    },
    /// Bring up the M5 (#71) Tensix virtio engine via the
    /// `TensixEngine` module: pick tile, load M3 firmware, release
    /// BRISC, run the M5 handshake. PASS = handshake completes with
    /// matching protocol version. This is the same code path the
    /// daemon will use when the `virtio-engine` feature is enabled
    /// (M4.3 dispatch_boot integration); running it standalone
    /// gives an integration check without booting any L2CPU.
    /// Bypasses the daemon. Issue #71.
    TensixEngine,
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
        /// Bypass the HTTP-conditional cache and always re-download.
        #[arg(long)]
        refetch: bool,
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
            uboot,
            dtb,
            initramfs,
            root_device,
            force_reset_pcie,
            force,
            virtio_console,
            no_virtio_rng,
            fwd,
        }) => {
            let disk = resolve_disk_path(
                cli.disk,
                DEFAULT_DISK_PATH,
                std::path::Path::new(DEFAULT_DISK_PATH).exists(),
            );
            // clap's `conflicts_with` already enforces mutual exclusion;
            // here we just pick the variant. Default to kernel `Image`
            // if neither flag was given (backwards-compat with the
            // pre-#44 default).
            let payload = match (kernel, uboot) {
                (Some(_), Some(_)) => unreachable!("clap conflicts_with"),
                (_, Some(p)) => daemon::protocol::BootPayload::Uboot(p),
                (Some(p), None) => daemon::protocol::BootPayload::Kernel(p),
                (None, None) => daemon::protocol::BootPayload::Kernel("Image".to_string()),
            };
            run_boot_client(
                cli.ttdevice,
                cli.l2cpu as u8,
                opensbi,
                payload,
                dtb,
                initramfs,
                root_device,
                force_reset_pcie,
                disk,
                cli.network,
                fwd,
                virtio_console,
                !no_virtio_rng,
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
        Some(Commands::AddNet { ssh_port, fwd }) => {
            let mut sock = daemon::client::connect(cli.ttdevice)?;
            daemon::client::add_net(&mut sock, cli.l2cpu as u8, ssh_port, fwd)
        }
        Some(Commands::RemoveNet) => {
            let mut sock = daemon::client::connect(cli.ttdevice)?;
            daemon::client::remove_net(&mut sock, cli.l2cpu as u8)
        }
        Some(Commands::AddConsole) => {
            let mut sock = daemon::client::connect(cli.ttdevice)?;
            daemon::client::add_console(&mut sock, cli.l2cpu as u8)
        }
        Some(Commands::RemoveConsole) => {
            let mut sock = daemon::client::connect(cli.ttdevice)?;
            daemon::client::remove_console(&mut sock, cli.l2cpu as u8)
        }
        Some(Commands::Daemon { action }) => run_daemon_cmd(cli.ttdevice, action),
        Some(Commands::Image { action }) => {
            match action {
                ImageAction::List => image::cmd_list_available(),
                ImageAction::Info { name } => image::cmd_image_info(&name),
                ImageAction::Pull {
                    name,
                    output,
                    refetch,
                } => {
                    image::cmd_pull(&name, output.as_deref(), refetch);
                }
            }
            Ok(())
        }
        Some(Commands::Kernel { action }) => {
            match action {
                KernelAction::List => kernel::cmd_list(),
                KernelAction::Pull {
                    version,
                    output,
                    refetch,
                } => {
                    kernel::cmd_pull(version.as_deref(), output.as_deref(), refetch);
                }
            }
            Ok(())
        }
        Some(Commands::Ramdisk { action }) => {
            match action {
                RamdiskAction::List => ramdisk::cmd_list(),
                RamdiskAction::Pull {
                    name,
                    output,
                    refetch,
                } => {
                    ramdisk::cmd_pull(&name, output.as_deref(), refetch);
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

/// Parse `HOST:GUEST` for `add-net --fwd`. Both sides must be in the
/// 1..=65535 range; bare numbers, leading whitespace, and missing
/// halves all error out cleanly.
fn parse_fwd_pair(s: &str) -> std::io::Result<(u16, u16)> {
    let (h, g) = s.split_once(':').ok_or_else(|| {
        std::io::Error::other(format!("invalid --fwd {:?}; expected HOST:GUEST", s))
    })?;
    let host: u16 = h
        .parse()
        .map_err(|_| std::io::Error::other(format!("invalid --fwd HOST {:?}", h)))?;
    let guest: u16 = g
        .parse()
        .map_err(|_| std::io::Error::other(format!("invalid --fwd GUEST {:?}", g)))?;
    if host == 0 || guest == 0 {
        return Err(std::io::Error::other(format!(
            "invalid --fwd {:?}; ports must be 1..=65535",
            s
        )));
    }
    Ok((host, guest))
}

#[allow(clippy::too_many_arguments)]
fn run_boot_client(
    card: u32,
    l2cpu: u8,
    opensbi: String,
    payload: daemon::protocol::BootPayload,
    dtb: String,
    initramfs: Option<String>,
    root_device: String,
    force_reset_pcie: bool,
    disk: Option<String>,
    network: bool,
    extra_fwd: Vec<(u16, u16)>,
    console: bool,
    rng: bool,
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
    let payload = match payload {
        daemon::protocol::BootPayload::Kernel(p) => {
            daemon::protocol::BootPayload::Kernel(absolutize(&p)?)
        }
        daemon::protocol::BootPayload::Uboot(p) => {
            daemon::protocol::BootPayload::Uboot(absolutize(&p)?)
        }
    };
    let dtb = absolutize(&dtb)?;
    let initramfs = initramfs.map(|p| absolutize(&p)).transpose()?;
    let disk = disk.map(|p| absolutize(&p)).transpose()?;
    let mut sock = daemon::client::connect(card)?;
    daemon::client::boot(
        &mut sock,
        l2cpu,
        opensbi,
        payload,
        dtb,
        initramfs,
        root_device,
        force_reset_pcie,
        disk,
        network,
        extra_fwd,
        console,
        rng,
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
    let writes_chip = !matches!(
        action,
        DebugAction::ReadResetReg | DebugAction::TelemetryDump { .. } | DebugAction::PickTile,
    );
    // TensixEngine bring-up writes to the chip (loads firmware,
    // drives reset). It belongs in the writes-chip set below.
    let _ = DebugAction::TensixEngine;
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

    let chip = shared_chip::SharedChip::new(card)
        .map_err(|e| std::io::Error::other(format!("open /dev/tenstorrent/{}: {}", card, e)))?;
    match action {
        DebugAction::ReadResetReg => {
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
                "[debug] invoking SharedChip::reset_x280 on L2CPU {} (PLL step + OR-in bit {})",
                l2cpu,
                l2cpu + 4
            );
            chip.reset_x280(&[l2cpu]);
            eprintln!("[debug] reset_x280 returned without panic");
            Ok(())
        }
        DebugAction::AssertReset => toggle_reset_bit(&chip, l2cpu, false),
        DebugAction::DeassertReset => toggle_reset_bit(&chip, l2cpu, true),
        DebugAction::TensixHello { x, y, duration } => {
            // Resolve any unspecified coord via the M2 picker before
            // dropping the SharedChip — the picker reads telemetry
            // through it.
            let (x, y) = resolve_tensix_coords(&chip, x, y)?;
            drop(chip);
            run_tensix_hello(card, x, y, duration)
        }
        DebugAction::TelemetryDump { all_tags } => run_telemetry_dump(&chip, all_tags),
        DebugAction::PickTile => run_pick_tile(&chip),
        DebugAction::TensixVirtio { x, y } => {
            let (x, y) = resolve_tensix_coords(&chip, x, y)?;
            drop(chip);
            run_tensix_virtio(card, x, y)
        }
        DebugAction::TensixEngine => run_tensix_engine(card, &chip),
    }
}

/// Bring up the Tensix virtio engine via `TensixEngine::bring_up` —
/// the same code path the daemon will use under the
/// `virtio-engine` feature. Verifies handshake + protocol version
/// match, then spawns a `KickPoller` and drives a synthetic kick
/// to prove the daemon-side data plane consumes events end-to-end.
fn run_tensix_engine(card: u32, chip: &shared_chip::SharedChip) -> std::io::Result<()> {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;
    eprintln!("[tensix-engine] bringing up via TensixEngine::bring_up");
    let engine = Arc::new(tensix_engine::TensixEngine::bring_up(card, chip)?);
    eprintln!(
        "[tensix-engine] PASS: tile NOC0 ({}, {}), translated ({}, {}), \
         firmware_version={:#010x}, protocol_version={}",
        engine.noc0_x,
        engine.noc0_y,
        engine.translated_x,
        engine.translated_y,
        engine.firmware_version,
        engine.protocol_version,
    );
    let (producer, consumer, entries) = engine.kick_ring_header();
    eprintln!(
        "[tensix-engine]   kick ring: producer={}, consumer={}, entries={}",
        producer, consumer, entries
    );

    // Spawn the daemon-side kick poller (M5.5a) and verify it
    // consumes a kick we drive directly. This proves the full path:
    // host write → BRISC pickup → kick ring push → poller consume.
    eprintln!("[tensix-engine] spawning kick poller and driving a synthetic kick");
    let mut poller = tensix_data_plane::KickPoller::spawn(Arc::clone(&engine));
    let stats = Arc::clone(&poller.stats);

    // Synthetic kick: write QUEUE_NOTIFY=2 on slot 7 (L2CPU 1's
    // rng device, in the firmware's slot ordering). BRISC sees the
    // write next sweep, appends a KickEntry, advances producer_seq;
    // the poller picks it up.
    let slot7_notify = virtio_engine::slot_regs_base(7) + virtio_engine::MMIO_QUEUE_NOTIFY;
    let before = stats.kicks_consumed.load(Ordering::Relaxed);
    engine.write_l1_u32(slot7_notify, 2);
    // Poller wakes up within a few ms (50 µs FAST sleep + BRISC's
    // poll latency); 100 ms is generous.
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_millis(100) {
        if stats.kicks_consumed.load(Ordering::Relaxed) > before {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let after = stats.kicks_consumed.load(Ordering::Relaxed);
    let last = stats.last_kick_slot_queue.load(Ordering::Relaxed);
    if after > before {
        eprintln!(
            "[tensix-engine]   poller consumed {} kick(s) ({} → {}); \
             last (slot, queue) = ({}, {}) — KICK POLLER PASS",
            after - before,
            before,
            after,
            (last >> 16) & 0xFFFF,
            last & 0xFFFF
        );
    } else {
        poller.shutdown();
        return Err(std::io::Error::other(format!(
            "kick poller did not consume our synthetic kick within 100 ms \
             (kicks_consumed stayed at {})",
            before
        )));
    }
    poller.shutdown();
    Ok(())
}

/// Resolve `(x, y)` from the CLI: pass-through if both are present,
/// fall back to the M2 picker otherwise. If only one is given we
/// still call the picker so the user gets a clear "explain why" path
/// — half-overrides are too easy to misuse otherwise.
fn resolve_tensix_coords(
    chip: &shared_chip::SharedChip,
    x: Option<u16>,
    y: Option<u16>,
) -> std::io::Result<(u16, u16)> {
    if let (Some(x), Some(y)) = (x, y) {
        return Ok((x, y));
    }
    let telem = telemetry::read_telemetry(chip)
        .map_err(|e| std::io::Error::other(format!("read telemetry: {}", e)))?;
    let picked = tensix_tile::pick_virtio_engine_tile(&telem)
        .map_err(|e| std::io::Error::other(format!("pick tile: {}", e)))?;
    eprintln!(
        "[tensix-hello] picker chose ({}, {}) [{:?}]",
        picked.x, picked.y, picked.reason
    );
    Ok((picked.x, picked.y))
}

fn run_telemetry_dump(chip: &shared_chip::SharedChip, all_tags: bool) -> std::io::Result<()> {
    let table_addr = chip.axi_read32(telemetry::ARC_TELEMETRY_PTR_ADDR);
    println!(
        "SCRATCH_RAM[13] @ {:#010x} = {:#010x} (telemetry table base)",
        telemetry::ARC_TELEMETRY_PTR_ADDR,
        table_addr
    );
    let telem = telemetry::read_telemetry(chip)
        .map_err(|e| std::io::Error::other(format!("read telemetry: {}", e)))?;
    println!(
        "telemetry: version={:#010x} entry_count={}",
        telem.version, telem.entry_count
    );
    println!("  HarvestingState     = {:#010x}", telem.harvesting_state);
    println!(
        "  EnabledTensixCol    = {:#010x} ({} cols set)",
        telem.enabled_tensix_col,
        telem.enabled_tensix_col.count_ones()
    );
    println!(
        "  NocTranslation      = {} ({})",
        if telem.noc_translation_enabled { 1 } else { 0 },
        if telem.noc_translation_enabled {
            "translated"
        } else {
            "untranslated"
        }
    );
    println!("  BoardId             = {:#018x}", telem.board_id);
    println!("  AsicId              = {:#010x}", telem.asic_id);
    println!("  AsicLocation        = {}", telem.asic_location);
    println!("  EnabledEth          = {:#010x}", telem.enabled_eth);
    println!("  EnabledGddr         = {:#010x}", telem.enabled_gddr);
    println!("  EnabledL2Cpu        = {:#010x}", telem.enabled_l2cpu);

    let cols =
        tensix_tile::working_tensix_cols(telem.enabled_tensix_col, telem.noc_translation_enabled);
    let rows = tensix_tile::working_tensix_rows(telem.harvesting_state);
    println!(
        "decoded working set ({} cols × {} rows):",
        cols.len(),
        rows.len()
    );
    println!("  cols: {:?}", cols);
    println!("  rows: {:?}", rows);
    let harvested_rows = tensix_tile::harvested_tensix_rows(telem.harvesting_state);
    if !harvested_rows.is_empty() {
        println!("  harvested rows: {:?}", harvested_rows);
    }
    match tensix_tile::pick_virtio_engine_tile(&telem) {
        Ok(p) => println!("  picker would choose: ({}, {}) [{:?}]", p.x, p.y, p.reason),
        Err(e) => println!("  picker would error: {}", e),
    }

    if all_tags {
        println!("--- all telemetry entries ---");
        for e in &telem.entries {
            println!(
                "  tag={:>3}  offset={:>3}  data={:#010x}",
                e.tag, e.offset, e.data
            );
        }
    }
    Ok(())
}

fn run_pick_tile(chip: &shared_chip::SharedChip) -> std::io::Result<()> {
    let telem = telemetry::read_telemetry(chip)
        .map_err(|e| std::io::Error::other(format!("read telemetry: {}", e)))?;
    let picked = tensix_tile::pick_virtio_engine_tile(&telem)
        .map_err(|e| std::io::Error::other(format!("pick tile: {}", e)))?;
    println!("{} {} ({:?})", picked.x, picked.y, picked.reason);
    Ok(())
}

fn run_tensix_hello(card: u32, x: u16, y: u16, duration: u32) -> std::io::Result<()> {
    use std::time::{Duration, Instant};
    use tensix::{
        TensixTile, HELLO_COUNTER_OFFSET, HELLO_FIRMWARE, HELLO_MAGIC_OFFSET, HELLO_MAGIC_VALUE,
    };

    eprintln!(
        "[tensix-hello] tile ({}, {}) on card {}: firmware {} bytes",
        x,
        y,
        card,
        HELLO_FIRMWARE.len()
    );

    let tile = TensixTile::new(card, x, y)
        .map_err(|e| std::io::Error::other(format!("open tile ({}, {}): {}", x, y, e)))?;

    let prior_reset = tile.read_soft_reset();
    eprintln!(
        "[tensix-hello] prior soft-reset register: {:#010x}",
        prior_reset
    );

    eprintln!("[tensix-hello] asserting all baby-RISC soft resets");
    tile.assert_all_resets();
    let after_assert = tile.read_soft_reset();
    eprintln!(
        "[tensix-hello] soft-reset after assert: {:#010x}",
        after_assert
    );

    eprintln!("[tensix-hello] loading firmware to L1[0]");
    tile.load_brisc_firmware(HELLO_FIRMWARE);

    // Pre-clear the magic + counter slots so we observe the BRISC
    // firmware writing them — without this, a stale L1 value would
    // be indistinguishable from "BRISC ran".
    tile.write_l1_u32(HELLO_MAGIC_OFFSET, 0);
    tile.write_l1_u32(HELLO_COUNTER_OFFSET, 0);

    eprintln!("[tensix-hello] releasing BRISC from soft-reset");
    tile.release_brisc_only();
    let after_release = tile.read_soft_reset();
    eprintln!(
        "[tensix-hello] soft-reset after release: {:#010x}",
        after_release
    );

    // Poll once per second for `duration` seconds. The first sample
    // happens ~1s in, after BRISC has had time to start. We
    // pre-cleared the counter to zero, so any nonzero value on the
    // very first sample is also evidence of advancement (otherwise
    // `--duration 1` could never PASS for a logic-only reason — we'd
    // have nothing to compare against).
    let start = Instant::now();
    let mut last_counter: Option<u32> = None;
    let mut counter_advanced = false;
    let mut magic_observed = false;
    for sec in 1..=duration {
        std::thread::sleep(Duration::from_secs(1));
        let magic = tile.read_l1_u32(HELLO_MAGIC_OFFSET);
        let counter = tile.read_l1_u32(HELLO_COUNTER_OFFSET);
        let advanced = match last_counter {
            Some(prev) => counter != prev,
            None => counter != 0,
        };
        eprintln!(
            "[tensix-hello] t+{}s: magic={:#010x} counter={:#010x}{}",
            sec,
            magic,
            counter,
            if advanced { " (advanced)" } else { "" }
        );
        if magic == HELLO_MAGIC_VALUE {
            magic_observed = true;
        }
        if advanced {
            counter_advanced = true;
        }
        last_counter = Some(counter);
    }
    let elapsed = start.elapsed();

    if magic_observed && counter_advanced {
        eprintln!(
            "[tensix-hello] PASS: magic + counter both observed after {:.1?}",
            elapsed
        );
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "FAIL: magic_observed={}, counter_advanced={} after {:.1?}",
            magic_observed, counter_advanced, elapsed
        )))
    }
}

/// Smoke-test the M3 (#69) virtio engine: load the firmware on the
/// chosen Tensix tile, release BRISC, and check the static reg files,
/// the STATUS state machine, and the QUEUE_SEL multiplexer end-to-
/// end. Bypasses the daemon (debug path).
fn run_tensix_virtio(card: u32, x: u16, y: u16) -> std::io::Result<()> {
    use std::thread::sleep;
    use std::time::Duration;
    use tensix::TensixTile;
    use virtio_engine as ve;

    eprintln!(
        "[tensix-virtio] tile ({}, {}) on card {}: firmware {} bytes",
        x,
        y,
        card,
        ve::VIRTIO_FIRMWARE.len()
    );

    let tile = TensixTile::new(card, x, y)
        .map_err(|e| std::io::Error::other(format!("open tile ({}, {}): {}", x, y, e)))?;

    eprintln!("[tensix-virtio] asserting all baby-RISC soft resets");
    tile.assert_all_resets();

    // Wipe the regs region so we don't see leftover state from a
    // prior firmware load. The firmware will overwrite these slots
    // on entry; pre-clearing makes "did the firmware run" vs "did
    // the firmware run AND I'm reading the right addresses"
    // unambiguous on the smoke checks below.
    eprintln!("[tensix-virtio] zeroing reg-file region (16 KiB × 16 slots = 64 KiB)");
    for slot in 0..ve::NUM_SLOTS {
        let base = ve::slot_regs_base(slot);
        for off in (0..ve::REGS_PER_DEV).step_by(4) {
            tile.write_l1_u32(base + off, 0);
        }
    }

    eprintln!("[tensix-virtio] loading firmware to L1[0]");
    tile.load_brisc_firmware(ve::VIRTIO_FIRMWARE);

    eprintln!("[tensix-virtio] releasing BRISC from soft-reset");
    tile.release_brisc_only();

    // Wait briefly for BRISC to finish the static-init pass over all
    // 16 slots. At ~64 KiB of stores at ~1 GHz that's microseconds;
    // 10 ms is hugely generous and avoids any flakiness from a slow
    // first sweep.
    sleep(Duration::from_millis(10));

    // M5 (#71) handshake. Firmware blocks for hello before entering
    // the steady-state poll loop, so without this every subsequent
    // check below would hang.
    eprintln!("[tensix-virtio] M5 handshake: writing hello, polling hello-ack");
    {
        use tensix_proto as proto;
        tile.write_l1_u32(
            proto::CTRL_BASE + proto::CTRL_OFF_HELLO + proto::HELLO_OFF_PROTOCOL_VERSION,
            proto::PROTOCOL_VERSION,
        );
        tile.write_l1_u32(
            proto::CTRL_BASE + proto::CTRL_OFF_HELLO + proto::HELLO_OFF_MAGIC,
            proto::HELLO_MAGIC,
        );
        let timeout = Duration::from_millis(500);
        let started = std::time::Instant::now();
        loop {
            let m = tile.read_l1_u32(
                proto::CTRL_BASE + proto::CTRL_OFF_HELLO_ACK + proto::HELLO_ACK_OFF_MAGIC,
            );
            if m == proto::HELLO_ACK_MAGIC {
                break;
            }
            if started.elapsed() > timeout {
                return Err(std::io::Error::other(format!(
                    "M5 hello-ack timeout after {:?} (got {:#010x})",
                    timeout, m
                )));
            }
            sleep(Duration::from_millis(1));
        }
        let proto_v = tile.read_l1_u32(
            proto::CTRL_BASE + proto::CTRL_OFF_HELLO_ACK + proto::HELLO_ACK_OFF_PROTOCOL_VERSION,
        );
        let fw_v = tile.read_l1_u32(
            proto::CTRL_BASE + proto::CTRL_OFF_HELLO_ACK + proto::HELLO_ACK_OFF_FIRMWARE_VERSION,
        );
        eprintln!(
            "[tensix-virtio]   hello-ack: protocol_version={}, firmware_version={:#010x}",
            proto_v, fw_v
        );
        if proto_v != proto::PROTOCOL_VERSION {
            return Err(std::io::Error::other(format!(
                "M5 protocol version mismatch: daemon expects {}, firmware reported {}",
                proto::PROTOCOL_VERSION,
                proto_v
            )));
        }
    }

    let mut errors = 0usize;
    let device_ids = [
        ve::VIRTIO_ID_BLOCK,
        ve::VIRTIO_ID_NET,
        ve::VIRTIO_ID_CONSOLE,
        ve::VIRTIO_ID_ENTROPY,
    ];
    eprintln!("[tensix-virtio] verifying static reg files across all slots");
    for slot in 0..ve::NUM_SLOTS {
        let base = ve::slot_regs_base(slot);
        let dev_idx = (slot % ve::DEVS_PER_L2CPU) as usize;
        let l2cpu = slot / ve::DEVS_PER_L2CPU;
        let expected_dev_id = device_ids[dev_idx];

        let magic = tile.read_l1_u32(base + ve::MMIO_MAGIC_VALUE);
        let version = tile.read_l1_u32(base + ve::MMIO_VERSION);
        let dev_id = tile.read_l1_u32(base + ve::MMIO_DEVICE_ID);
        let vendor = tile.read_l1_u32(base + ve::MMIO_VENDOR_ID);
        let nmax = tile.read_l1_u32(base + ve::MMIO_QUEUE_NUM_MAX);

        let ok = magic == ve::MAGIC
            && version == ve::VERSION
            && dev_id == expected_dev_id
            && vendor == ve::VENDOR_ID
            && nmax == ve::QUEUE_NUM_MAX;
        if !ok {
            errors += 1;
            eprintln!(
                "  slot {:2} (L2CPU {} dev {}): MAGIC={:#010x} VERSION={} DEVICE_ID={} \
                 VENDOR={:#010x} QUEUE_NUM_MAX={} (expected dev_id {}) — FAIL",
                slot, l2cpu, dev_idx, magic, version, dev_id, vendor, nmax, expected_dev_id
            );
        }
    }
    if errors == 0 {
        eprintln!("[tensix-virtio]   16/16 slots show correct static reg state");
    }

    // Stats page should show the firmware version + magic-loaded.
    let fw_version = tile.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_VERSION);
    let stats_magic = tile.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_MAGIC);
    eprintln!(
        "[tensix-virtio] stats: fw_version={:#010x} magic={:#010x}",
        fw_version, stats_magic
    );
    if stats_magic != ve::STATS_MAGIC_LOADED {
        eprintln!("  stats magic mismatch — BRISC didn't initialize");
        errors += 1;
    }

    // STATUS state machine: write ACK, poll for status_changes counter
    // to bump.
    eprintln!("[tensix-virtio] driving STATUS write on slot 0");
    let slot0_status = ve::slot_regs_base(0) + ve::MMIO_STATUS;
    let status_changes_addr = ve::STATS_BASE + ve::STATS_OFF_STATUS_CHANGES;
    let before_status = tile.read_l1_u32(status_changes_addr);
    tile.write_l1_u32(slot0_status, ve::STATUS_ACKNOWLEDGE);
    sleep(Duration::from_millis(5));
    let after_status = tile.read_l1_u32(status_changes_addr);
    if after_status > before_status {
        eprintln!(
            "  status_changes counter advanced ({} → {}) — STATUS state machine PASS",
            before_status, after_status
        );
    } else {
        eprintln!(
            "  status_changes counter did NOT advance ({} → {}) — STATUS state machine FAIL",
            before_status, after_status
        );
        errors += 1;
    }

    // QUEUE_SEL multiplexer: write SEL=1 on slot 1 (net device, has 2
    // queues). The firmware should swap the visible regs and bump
    // sel_changes.
    eprintln!("[tensix-virtio] driving QUEUE_SEL=1 on slot 1 (net)");
    let slot1_sel = ve::slot_regs_base(1) + ve::MMIO_QUEUE_SEL;
    let sel_changes_addr = ve::STATS_BASE + ve::STATS_OFF_SEL_CHANGES;
    let before_sel = tile.read_l1_u32(sel_changes_addr);
    tile.write_l1_u32(slot1_sel, 1);
    sleep(Duration::from_millis(5));
    let after_sel = tile.read_l1_u32(sel_changes_addr);
    let nmax_after_swap = tile.read_l1_u32(ve::slot_regs_base(1) + ve::MMIO_QUEUE_NUM_MAX);
    if after_sel > before_sel && nmax_after_swap == ve::QUEUE_NUM_MAX {
        eprintln!(
            "  sel_changes counter advanced ({} → {}), QUEUE_NUM_MAX after swap = {} \
             — QUEUE_SEL multiplexer PASS",
            before_sel, after_sel, nmax_after_swap
        );
    } else {
        eprintln!(
            "  QUEUE_SEL multiplexer FAIL: sel_changes {} → {}, QUEUE_NUM_MAX = {}",
            before_sel, after_sel, nmax_after_swap
        );
        errors += 1;
    }

    // M5 (#71) kick-ring path: drive QUEUE_NOTIFY=2 on slot 5,
    // expect the kick ring's producer_seq to advance and the entry
    // at the new ring index to record (slot=5, queue_idx=2).
    use tensix_proto as proto;
    {
        eprintln!("[tensix-virtio] driving QUEUE_NOTIFY=2 on slot 5 (L2CPU 1 net)");
        let slot5_notify = ve::slot_regs_base(5) + ve::MMIO_QUEUE_NOTIFY;
        let producer_addr =
            proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_PRODUCER_SEQ;
        let before_seq = tile.read_l1_u32(producer_addr);
        tile.write_l1_u32(slot5_notify, 2);
        sleep(Duration::from_millis(5));
        let after_seq = tile.read_l1_u32(producer_addr);
        if after_seq != before_seq.wrapping_add(1) {
            eprintln!(
                "  kick ring did NOT advance: producer_seq {} → {} (expected +1)",
                before_seq, after_seq
            );
            errors += 1;
        } else {
            // Read the entry at index `before_seq % KICK_RING_ENTRIES`
            // and verify it records the (slot, queue_idx) we wrote.
            let idx = before_seq % proto::KICK_RING_ENTRIES;
            let entry_off =
                proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING + idx * proto::KICK_ENTRY_SIZE;
            let slot_field = tile.read_l1_u32(entry_off);
            let entry_seq = tile.read_l1_u32(entry_off + 4);
            let recorded_slot = (slot_field & 0xFFFF) as u16;
            let recorded_queue = (slot_field >> 16) as u16;
            if recorded_slot == 5 && recorded_queue == 2 && entry_seq == before_seq {
                eprintln!(
                    "  kick ring advanced to seq {}, entry[{}] = (slot={}, queue={}) — \
                     KICK PATH PASS",
                    after_seq, idx, recorded_slot, recorded_queue
                );
            } else {
                eprintln!(
                    "  kick ring entry mismatch: idx={} recorded (slot={}, queue={}, seq={}), \
                     expected (slot=5, queue=2, seq={})",
                    idx, recorded_slot, recorded_queue, entry_seq, before_seq
                );
                errors += 1;
            }
        }
    }

    // M5.5c firmware extension: feature negotiation. Drive
    // DEVICE_FEATURES_SEL=1 on slot 0 and read DEVICE_FEATURES
    // back; expect bit 0 of the high half (VIRTIO_F_VERSION_1).
    // Without this advertisement the guest's virtio drivers fail
    // FEATURES_OK negotiation before ever writing QUEUE_NOTIFY.
    {
        eprintln!("[tensix-virtio] driving DEVICE_FEATURES_SEL=1 on slot 0");
        let slot0_dev_feat_sel = ve::slot_regs_base(0) + ve::MMIO_DEVICE_FEATURES_SEL;
        let slot0_dev_feat = ve::slot_regs_base(0) + ve::MMIO_DEVICE_FEATURES;
        tile.write_l1_u32(slot0_dev_feat_sel, 1);
        sleep(Duration::from_millis(5));
        let advertised = tile.read_l1_u32(slot0_dev_feat);
        if advertised == 1 {
            eprintln!(
                "  DEVICE_FEATURES (high half) = {:#010x} — VIRTIO_F_VERSION_1 advertised \
                 — FEATURES PASS",
                advertised
            );
        } else {
            eprintln!(
                "  FEATURES FAIL: DEVICE_FEATURES (high half) = {:#010x}, expected 1 \
                 (VIRTIO_F_VERSION_1)",
                advertised
            );
            errors += 1;
        }
        // Switch back to low half — DEVICE_FEATURES should now
        // read 0 (no low-half features advertised).
        tile.write_l1_u32(slot0_dev_feat_sel, 0);
        sleep(Duration::from_millis(5));
        let low_half = tile.read_l1_u32(slot0_dev_feat);
        if low_half == 0 {
            eprintln!(
                "  DEVICE_FEATURES (low half) = {:#010x} — correct",
                low_half
            );
        } else {
            eprintln!(
                "  FEATURES FAIL: DEVICE_FEATURES (low half) = {:#010x}, expected 0",
                low_half
            );
            errors += 1;
        }
    }

    // M5.5b firmware extension: when the guest writes
    // QUEUE_DESC_LOW for the current SEL, BRISC should snapshot it
    // into shadow[slot][sel].SHADOW_Q_OFF_DESC_LO. Drive a write
    // on slot 0 (currently SEL=0 by default) and verify the shadow
    // captures it.
    {
        eprintln!("[tensix-virtio] driving QUEUE_DESC_LOW=0xCAFE0001 on slot 0 (SEL=0)");
        let slot0_desc_lo = ve::slot_regs_base(0) + ve::MMIO_QUEUE_DESC_LOW;
        let test_val = 0xCAFE_0001u32;
        tile.write_l1_u32(slot0_desc_lo, test_val);
        sleep(Duration::from_millis(5));
        let shadow_addr = ve::shadow_queue_addr(0, 0, ve::SHADOW_Q_OFF_DESC_LO);
        let captured = tile.read_l1_u32(shadow_addr);
        if captured == test_val {
            eprintln!(
                "  shadow[slot=0, queue=0, DESC_LO] = {:#010x} — QUEUE SHADOW PASS",
                captured
            );
        } else {
            eprintln!(
                "  QUEUE SHADOW FAIL: shadow[slot=0, queue=0, DESC_LO] = {:#010x}, \
                 expected {:#010x}",
                captured, test_val
            );
            errors += 1;
        }
    }

    // M5 (#71) completion-ring path: write a CompletionEntry into
    // L1 at the next producer index, bump producer_seq, then poll
    // the firmware's `compl_events` stat to confirm BRISC consumed
    // the entry. This exercises the daemon→BRISC half of the
    // bridge.
    {
        eprintln!("[tensix-virtio] driving completion entry on slot 7 (L2CPU 1 rng)");
        let producer_addr =
            proto::CTRL_BASE + proto::CTRL_OFF_COMPL_RING_HDR + proto::COMPL_HDR_OFF_PRODUCER_SEQ;
        let compl_events_addr = ve::STATS_BASE + ve::STATS_OFF_COMPL_EVENTS;
        let last_compl_addr = ve::STATS_BASE + ve::STATS_OFF_LAST_COMPL;
        let before_compl = tile.read_l1_u32(compl_events_addr);
        let producer_before = tile.read_l1_u32(producer_addr);
        let idx = producer_before % proto::COMPL_RING_ENTRIES;
        let entry_off =
            proto::CTRL_BASE + proto::CTRL_OFF_COMPL_RING + idx * proto::COMPL_ENTRY_SIZE;
        // CompletionEntry: slot=7 (low 16), queue_idx=0 (high 16)
        // packed into a single u32 — same packed format the
        // firmware reads for kicks. Queue 0 happens to be the
        // numeric value, but we write it explicitly via shifts so
        // future tweaks (queue=1, queue=N) don't have to remember
        // the layout.
        let expected = 7u32; // (slot=7, queue=0) packs to just 7.
        tile.write_l1_u32(entry_off, expected);
        tile.write_l1_u32(entry_off + 4, 42); // used_idx
        tile.write_l1_u32(producer_addr, producer_before.wrapping_add(1));
        sleep(Duration::from_millis(5));
        let after_compl = tile.read_l1_u32(compl_events_addr);
        let last_compl = tile.read_l1_u32(last_compl_addr);
        if after_compl > before_compl && last_compl == expected {
            eprintln!(
                "  compl_events advanced ({} → {}), last_compl=({}, {}) — \
                 COMPLETION PATH PASS",
                before_compl,
                after_compl,
                (last_compl & 0xFFFF) as u16,
                (last_compl >> 16) as u16
            );
        } else {
            eprintln!(
                "  COMPLETION PATH FAIL: compl_events {} → {}, last_compl={:#010x}",
                before_compl, after_compl, last_compl
            );
            errors += 1;
        }
    }

    if errors == 0 {
        eprintln!("[tensix-virtio] PASS");
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "FAIL: {} subtests failed",
            errors
        )))
    }
}

fn toggle_reset_bit(
    chip: &shared_chip::SharedChip,
    l2cpu: usize,
    release: bool,
) -> std::io::Result<()> {
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
            no_sandbox,
            metrics_port,
        } => daemon::runner::start(daemon::runner::StartOpts {
            card,
            foreground,
            log_file: log_file.map(std::path::PathBuf::from),
            sandbox: !no_sandbox,
            metrics_port,
        }),
        DaemonAction::Stop => daemon::runner::stop(card),
        DaemonAction::Restart {
            foreground,
            log_file,
            no_sandbox,
            metrics_port,
        } => daemon::runner::restart(
            card,
            foreground,
            log_file.map(std::path::PathBuf::from),
            !no_sandbox,
            metrics_port,
        ),
        DaemonAction::Status => daemon::runner::status(card),
        DaemonAction::Logs { lines, no_follow } => daemon::runner::logs(daemon::runner::LogsOpts {
            card,
            follow: !no_follow,
            lines,
        }),
        DaemonAction::Ports => print_ssh_ports(card),
    }
}

/// Print the SSH-forward host port for each L2CPU on `card`, with a
/// quick `bind()` probe to flag which ones are already in use. Pure
/// CLI-side — doesn't talk to the daemon, so it's useful even before
/// `daemon start`.
fn print_ssh_ports(card: u32) -> std::io::Result<()> {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
    println!("card {}: per-L2CPU SSH-forward ports", card);
    for idx in 0..4u8 {
        let port = regs::slirp::ssh_port(card, idx);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let status = match TcpListener::bind(addr) {
            Ok(listener) => {
                drop(listener);
                "available".to_string()
            }
            Err(e) => format!("in use ({})", e),
        };
        println!("  l2cpu {}: 127.0.0.1:{} — {}", idx, port, status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("clap failed to parse")
    }

    // --- parse_fwd_pair (#37) ----------------------------------------------

    #[test]
    fn parse_fwd_pair_accepts_valid() {
        assert_eq!(parse_fwd_pair("5201:5201").unwrap(), (5201, 5201));
        assert_eq!(parse_fwd_pair("8080:80").unwrap(), (8080, 80));
        assert_eq!(parse_fwd_pair("65535:1").unwrap(), (65535, 1));
    }

    #[test]
    fn parse_fwd_pair_rejects_malformed() {
        // Missing separator.
        assert!(parse_fwd_pair("5201").is_err());
        assert!(parse_fwd_pair("5201/5201").is_err());
        // Empty halves.
        assert!(parse_fwd_pair(":80").is_err());
        assert!(parse_fwd_pair("80:").is_err());
        // Non-numeric.
        assert!(parse_fwd_pair("abc:80").is_err());
        assert!(parse_fwd_pair("80:xyz").is_err());
        // Out of u16 range.
        assert!(parse_fwd_pair("65536:80").is_err());
        // Port 0 — kernel-pick wildcard, not what the operator wants.
        assert!(parse_fwd_pair("0:80").is_err());
        assert!(parse_fwd_pair("80:0").is_err());
    }

    #[test]
    fn parse_fwd_pair_error_messages_name_the_input() {
        // Operator-facing diagnostic shouldn't be cryptic.
        let err = parse_fwd_pair("nope").unwrap_err();
        assert!(format!("{}", err).contains("nope"));
        let err = parse_fwd_pair("5201:notaport").unwrap_err();
        assert!(format!("{}", err).contains("notaport"));
    }

    // --- absolutize ---------------------------------------------------------

    #[test]
    fn absolutize_errors_on_missing_file() {
        // The canonicalize() under absolutize requires the path to exist; a
        // typo'd rootfs needs to fail client-side before we send the bogus
        // path over the daemon wire.
        let res = absolutize("/definitely/not/a/real/path/xyzzy.ext4");
        assert!(res.is_err());
        // And the error message must name the offending path, so a cold-
        // booted user can see which argument went wrong.
        let msg = format!("{}", res.err().unwrap());
        assert!(
            msg.contains("xyzzy.ext4"),
            "error should name path: {}",
            msg
        );
    }

    #[test]
    fn absolutize_returns_absolute_for_existing_relative_path() {
        // /etc/hosts is nearly always present on Linux and is absolute, so
        // start from a relative form and round-trip through absolutize.
        let abs = absolutize("/etc/hosts").expect("canonicalize /etc/hosts");
        assert!(std::path::Path::new(&abs).is_absolute());
        // Also exercise a relative-to-cwd case: cwd itself via ".".
        let dot = absolutize(".").expect("canonicalize .");
        assert!(std::path::Path::new(&dot).is_absolute());
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
        // When no subcommand is given, `main` falls through to
        // run_connect_client (same as the explicit `connect` subcommand);
        // the global flags must still apply.
        let cli = parse(&["tt-bh-linux", "--no-console", "-n", "-d", "x.ext4"]);
        assert!(cli.command.is_none());
        assert!(cli.no_console);
        assert!(cli.network);
        assert_eq!(cli.disk.as_deref(), Some("x.ext4"));
    }
}
