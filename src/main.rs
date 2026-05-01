// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! bhx — unified Rust binary for booting and managing Linux on
//! Tenstorrent Blackhole L2CPU (SiFive X280) RISC-V cores.

// Crate-wide allow(dead_code) was retired in favor of targeted
// module-level allows (kmd, regs, virtio_engine, virtio/mod, uart_engine,
// tensix_proto) plus a handful of per-item allows on retained API
// surface — see #100. New unused items should now surface as build
// warnings; either delete them, justify with an inline allow, or fold
// into the appropriate module-level allow if it's another wire-format
// constant.

mod boot;
mod chip;
mod clock;
mod cloud_init;
mod console;
mod daemon;
mod error;
mod fdt_ffi;
mod fetch;
mod image;
mod kmd;
mod l2cpu;
mod profile;
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
mod uart_engine;
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
#[command(name = "bhx")]
#[command(about = "Boot and manage Linux on Tenstorrent Blackhole L2CPU")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Tenstorrent device index.
    ///
    /// Defaults to 0. May be overridden by an explicit card prefix in
    /// `-l <C>:<N>`; passing both with conflicting cards is an error.
    #[arg(short = 't', long = "ttdevice", global = true)]
    ttdevice: Option<u32>,

    /// L2CPU index (0-3), optionally prefixed with `<card>:`.
    ///
    /// Plain `-l 2` targets the L2CPU on the card selected by `-t`
    /// (default 0). `-l 1:2` targets card 1, L2CPU 2 — overrides
    /// `-t` and surfaces a conflict if `-t` was passed with a
    /// different card.
    #[arg(short = 'l', long = "l2cpu", default_value = "0", global = true)]
    l2cpu: String,

    /// Path to disk image (defaults to rootfs.ext4 if present).
    /// Mutually exclusive with `--image`.
    #[arg(short = 'd', long = "disk", global = true)]
    disk: Option<String>,

    /// Boot a registry image by name (e.g. `debian-13`,
    /// `fedora-42`). Resolves to the canonical pulled location
    /// (`$XDG_DATA_HOME/bhx/images/<name>.<ext>`) so an operator
    /// doesn't have to remember or type the full path. The image
    /// must have been pulled first via `bhx image pull <name>`.
    /// Mutually exclusive with `--disk`.
    #[arg(short = 'i', long = "image", global = true, conflicts_with = "disk")]
    image: Option<String>,

    /// Enable virtio-net (requires the slirp feature)
    #[arg(short = 'n', long = "network", global = true)]
    network: bool,
}

// clap-derived subcommand enum. Variants vary in size (Boot has ~16
// fields), but boxing them just to satisfy `large_enum_variant`
// would add an allocation per parsed CLI invocation for no real win.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Commands {
    /// Boot an L2CPU via the daemon.
    ///
    /// Starts the chip + guest; use `connect` afterwards to attach a
    /// terminal.
    Boot {
        /// Path to OpenSBI binary.
        #[arg(long, default_value = "fw_jump.bin")]
        opensbi: String,
        /// Path to a raw Linux Image.
        ///
        /// Default boot path; mutually exclusive with --uboot.
        #[arg(long, conflicts_with = "uboot")]
        kernel: Option<String>,
        /// Path to a U-Boot binary (S-mode payload).
        ///
        /// Mutually exclusive with --kernel and --initramfs. In this mode
        /// the daemon loads U-Boot at the kernel offset and skips
        /// initramfs preload — U-Boot reads the kernel + initrd from the
        /// attached --disk at runtime.
        #[arg(long, conflicts_with_all = ["kernel", "initramfs"])]
        uboot: Option<String>,
        /// Path to device tree blob.
        #[arg(long, default_value = "blackhole-card.dtb")]
        dtb: String,
        /// Boot with an initramfs image instead of a virtio-block rootfs.
        #[arg(long)]
        initramfs: Option<String>,
        /// Root device name passed to the kernel.
        ///
        /// Ignored when --initramfs is set.
        #[arg(long, default_value = "vda")]
        root_device: String,
        /// Force a full PCIe link reset before booting.
        ///
        /// Disrupts other L2CPUs on the same card (they see a PCIe
        /// blip), so by default we probe `L2CPU_RESET` first and only
        /// reset when necessary.
        #[arg(long)]
        force_reset_pcie: bool,
        /// Tear down any existing slot for this L2CPU before re-imaging.
        ///
        /// Without this, a duplicate `boot` returns an error and leaves
        /// the prior slot untouched. Use when you know you want to
        /// re-image a running core — e.g. switching rootfs without an
        /// explicit `stop` first.
        #[arg(long)]
        force: bool,
        /// Skip attaching virtio-console.
        ///
        /// By default the daemon attaches a virtio-console device
        /// alongside the boot — the DTB-baked bootargs direct the
        /// kernel's console to `/dev/hvc0`, and stock distro kernels
        /// (which usually lack `CONFIG_HVC_RISCV_SBI`) have nowhere
        /// else to send output. Pass this only to bisect a virtio-
        /// console regression or to test SBI debug-console paths.
        #[arg(long = "no-virtio-console")]
        no_virtio_console: bool,
        /// Skip attaching virtio-rng.
        ///
        /// By default the daemon brings up virtio-rng alongside the
        /// boot — U-Boot's EFI loader needs it to install
        /// `EFI_RNG_PROTOCOL`, which the AlmaLinux EFI shim queries
        /// during signature verification (without it the shim stalls
        /// before chainloading GRUB). Harmless on direct-kernel boots
        /// (extra thread, satisfies guest /dev/random). Pass this only
        /// to bisect a virtio-rng regression.
        #[arg(long = "no-virtio-rng")]
        no_virtio_rng: bool,
        /// TCP port forwards as `HOST:GUEST` pairs (repeatable).
        ///
        /// Installed at boot time on top of the implicit SSH forward.
        /// Repeatable: `--fwd 5201:5201 --fwd 8080:80`. Same as
        /// `add-net --fwd`, but applied at cold-boot so the guest's
        /// virtio_net binding doesn't have to migrate to a hot-added
        /// device — needed for the net bench's ingress measurement
        /// against buildroot kernels that don't auto-rebind built-in
        /// virtio_net after teardown.
        #[arg(long = "fwd", value_parser = parse_fwd_pair)]
        fwd: Vec<(u16, u16)>,
        /// Attach a terminal (Rw mode) immediately after boot.
        ///
        /// Equivalent to running `bhx connect -l N` straight after a
        /// successful `boot`, but with no round-trip gap so the
        /// OpenSBI banner + early kernel printk arrive live rather
        /// than as scrollback. Ctrl-A x to detach.
        #[arg(short = 'a', long)]
        attach: bool,
        /// Override the L2CPU's advertised DRAM size.
        ///
        /// Accepts SI (`MB` / `GB`) and IEC binary (`MiB` / `GiB`)
        /// suffixes. Clamped to the L2CPU's physical size and rounded
        /// down to a 2 MiB boundary daemon-side. Empty means "use the
        /// physical size."
        #[arg(long = "memory", value_parser = parse_memory)]
        memory: Option<u64>,
        /// Override the slirp DHCP hostname.
        ///
        /// RFC-952-clean (`a-z0-9-`, no underscore, ≤63 chars).
        /// Replaces the per-(card, l2cpu) `bhx-cardN-l2cpuM` default
        /// — useful when a profile wants a stable name across
        /// re-images so SSH known_hosts caches.
        #[arg(long = "hostname", value_parser = parse_hostname)]
        hostname: Option<String>,
        /// Boot a saved profile.
        ///
        /// Resolves `<name>` against `~/.config/bhx/profiles.yaml`,
        /// clones the profile's image template into a per-(profile,
        /// l2cpu) writable disk under `~/.local/share/bhx/instances/`
        /// on first boot, and translates the profile's other fields
        /// into the equivalent inline flags. Mutually exclusive with
        /// `--kernel`, `--uboot`, `--memory`, and `--hostname`. When
        /// `-c` is set, the global `-d/--disk` and `-n/--network`
        /// flags are ignored — the profile owns those.
        #[arg(
            short = 'c',
            long = "profile",
            conflicts_with_all = ["kernel", "uboot", "memory", "hostname"],
        )]
        profile: Option<String>,
        /// Path to a cloud-init NoCloud seed image.
        ///
        /// Attached as a 2nd virtio-blk with `serial="cidata"` so
        /// cloud-init's NoCloud datasource finds it during the
        /// `local` boot stage and seeds users / SSH keys / hostname
        /// before sshd starts. Generate one with `bhx cloud-init
        /// seed`. Bundling at boot (rather than `add-disk` after) is
        /// required — cloud-init's local stage runs before the
        /// kernel finishes block probe; an add-disk arriving later
        /// loses that race and the seed never gets read.
        #[arg(long = "cloud-init")]
        cloud_init: Option<String>,
        /// Suppress auto-attach of a sibling `<disk>.cidata.img`
        /// seed. By default `bhx image pull` writes a default
        /// NoCloud seed next to each cloud-init image, and this boot
        /// path picks it up unless the operator passes `--cloud-init
        /// <other-path>` (explicit override) or this flag (skip
        /// auto-attach entirely).
        #[arg(long = "no-cidata")]
        no_cidata: bool,
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
        /// Operator-supplied serial returned by the guest's
        /// `VIRTIO_BLK_T_GET_ID`. Used by the guest's udev rules to
        /// assemble `/dev/disk/by-id/virtio-<name>` so the device has
        /// a stable identity across reboots and between rootfs and
        /// data disks. `cidata` is reserved for cloud-init seed
        /// disks. Omit to keep the legacy auto-derived
        /// `bhx-l2cpu-XX` serial.
        #[arg(long)]
        name: Option<String>,
    },
    /// Detach disks from a running L2CPU.
    ///
    /// Without `--name`, removes every disk attached to the slot
    /// (matches the legacy single-disk shape). With `--name X`,
    /// detaches only the disk whose serial matches `X`.
    RemoveDisk {
        /// Disk name (serial) to remove. Omit to remove all disks.
        #[arg(long)]
        name: Option<String>,
    },
    /// Attach virtio-net (slirp) to a running L2CPU.
    AddNet {
        /// Override the host-side port forwarded to the guest's :22.
        ///
        /// Default is the formula-derived per-(card, l2cpu_idx) port —
        /// see `daemon ports` for the mapping.
        #[arg(long)]
        ssh_port: Option<u16>,
        /// TCP port forwards as `HOST:GUEST` pairs (repeatable).
        ///
        /// Repeatable: `--fwd 5201:5201 --fwd 8080:80`. Each adds a
        /// slirp `tcp_listen_add` on `127.0.0.1:HOST` forwarding to
        /// `10.0.2.15:GUEST`. The implicit SSH forward (above) stays
        /// in place; this is for everything else (iperf3 server,
        /// HTTP diagnostics, debugger over slirp, …).
        #[arg(long = "fwd", value_parser = parse_fwd_pair)]
        fwd: Vec<(u16, u16)>,
    },
    /// Detach virtio-net from a running L2CPU.
    ///
    /// Drops libvdeslirp state (active TCP/NAT sessions on the guest
    /// will reset).
    RemoveNet,
    /// Attach a virtio-console device to a running L2CPU.
    ///
    /// Stock distro kernels with `CONFIG_VIRTIO_CONSOLE` register this
    /// as `/dev/hvc0`.
    AddConsole,
    /// Detach the virtio-console device from a running L2CPU.
    ///
    /// Joins the worker thread; any in-flight RX descriptors are
    /// dropped.
    RemoveConsole,
    /// Manage disk images
    Image {
        #[command(subcommand)]
        action: ImageAction,
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
    /// Generate cloud-init NoCloud seed images for stock cloud distros.
    ///
    /// Produces a `cidata`-labeled ISO containing user-data +
    /// meta-data that cloud-init's NoCloud datasource consumes on
    /// first boot. Pair with `bhx boot --cloud-init <path>` to seed
    /// users / SSH keys / hostname into a Debian / Fedora / Ubuntu /
    /// AlmaLinux cloud image.
    CloudInit {
        #[command(subcommand)]
        action: CloudInitAction,
    },
    /// Manage named boot profiles.
    ///
    /// Profiles let an operator save a long `bhx boot` flag bundle as
    /// a named YAML stanza in `~/.config/bhx/profiles.yaml` and recall
    /// it later.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon (double-forks unless --foreground).
    Start {
        #[arg(long)]
        foreground: bool,
        /// Override the log file path.
        ///
        /// Default is `$XDG_RUNTIME_DIR/bhx/<card>/log` which lives
        /// on tmpfs and is lost on host crash — set this to a file
        /// in the project directory when you need post-mortem logs.
        #[arg(long)]
        log_file: Option<String>,
        /// Disable the seccomp + landlock sandbox.
        ///
        /// The sandbox is on by default — defense-in-depth so a
        /// daemon-side bug can't pivot to read arbitrary host files
        /// or open outbound connections. Pass this only when
        /// debugging the filter itself (e.g. tracking down which
        /// syscall is missing from the whitelist). Linux only; the
        /// flag is accepted but a no-op everywhere else. See
        /// docs/sandbox-syscalls.md.
        #[arg(long)]
        no_sandbox: bool,
        /// Bind a Prometheus exporter on 127.0.0.1:<port>.
        ///
        /// Serves `GET /metrics`. Loopback only. Off by default; pass
        /// an explicit port to enable. See `daemon::metrics`.
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
    ///
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
    /// Verify pulled images against their sha256 sidecars.
    ///
    /// With NAME, recompute the sha256 of that image's pulled artifact
    /// and compare to the sidecar written at pull time. Without NAME,
    /// walks the image directory and verifies everything pulled.
    /// Reports MATCH / MODIFIED / MISSING / NO SIDECAR; modification
    /// is informational (cloud-init mutates the disk on first boot),
    /// not an error.
    Verify {
        /// Image name or alias (e.g., "debian-13"). Omit to verify all
        /// pulled images in the image directory.
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum DebugAction {
    /// Read the L2CPU_RESET register (0x80030014) and print it.
    ReadResetReg,
    /// Reset L2CPU N via the OpenSBI bit-toggle + PLL-bracket sequence.
    ///
    /// Calls `boot::reset_x280` on the given L2CPU (OR-in bit idx+4,
    /// bracketed by PLL step 1750→200→1750). Safe only when the daemon
    /// is not running against this card. Use `--l2cpu N` / `-l N`.
    ResetX280,
    /// Halt L2CPU N in place by clearing its release bit.
    ///
    /// Clears bit idx+4 of L2CPU_RESET — puts the L2CPU into reset in
    /// place. Sibling cores keep running. No PLL manipulation.
    AssertReset,
    /// Release L2CPU N from reset by setting its release bit.
    ///
    /// Sets bit idx+4 of L2CPU_RESET — releases the L2CPU from reset.
    /// Pure register write, no PLL manipulation. Useful to re-start a
    /// core that was held by `assert-reset`.
    DeassertReset,
    /// M1 hello-world: load BRISC firmware on Tensix (x, y) and poll L1.
    ///
    /// Loads the hello-world BRISC firmware onto Tensix tile (x, y),
    /// releases BRISC from soft-reset, and polls L1 for the magic
    /// value and incrementing counter. PASS when the counter advances
    /// across `--duration` seconds.
    TensixHello {
        /// Tensix tile X coordinate (NoC0 logical).
        ///
        /// When omitted, the M2 picker chooses one based on the chip's
        /// harvest mask. Functional workers on Blackhole live in
        /// x=1..7 and 10..16.
        #[arg(long)]
        x: Option<u16>,
        /// Tensix tile Y coordinate.
        ///
        /// Same defaulting behavior as `--x`. Functional workers on
        /// Blackhole live in y=2..11.
        #[arg(long)]
        y: Option<u16>,
        /// Number of seconds to poll the counter for.
        ///
        /// The host samples once per second.
        #[arg(long, default_value_t = 5)]
        duration: u32,
    },
    /// Dump the ARC firmware telemetry table.
    ///
    /// Prints the three M2 picker inputs (HarvestingState,
    /// EnabledTensixCol, NocTranslation) and the decoded set of
    /// working Tensix tile coordinates. Useful for confirming the
    /// picker on a new chip or diagnosing harvest-related anomalies.
    TelemetryDump {
        /// Print every telemetry tag entry (~60 rows) instead of just
        /// the picker-relevant subset.
        #[arg(long)]
        all_tags: bool,
    },
    /// Print the tile the picker would reserve for the virtio engine.
    ///
    /// Pure decode — does not touch the tile.
    PickTile,
    /// Load the M3 virtio-mmio engine firmware and smoke-test it.
    ///
    /// Loads the firmware onto a Tensix tile, releases BRISC, and
    /// smoke-tests the register file: verify the static MAGIC /
    /// VERSION / DEVICE_ID across all 16 slots, drive a STATUS write
    /// to confirm the state machine, drive a QUEUE_SEL change to
    /// confirm the multiplexer, and read the stats page. Bypasses
    /// the daemon.
    TensixVirtio {
        /// Tensix tile X coordinate (NoC0 logical).
        ///
        /// Defaults to the M2 picker output.
        #[arg(long)]
        x: Option<u16>,
        /// Tensix tile Y coordinate (NoC0 logical).
        ///
        /// Same defaulting as `--x`.
        #[arg(long)]
        y: Option<u16>,
    },
    /// Bring up the Tensix virtio engine end-to-end (M5 handshake).
    ///
    /// Picks a tile, loads M3 firmware, releases BRISC, and runs the
    /// M5 handshake. PASS = handshake completes with matching protocol
    /// version. Same code path the daemon uses when the
    /// `virtio-engine` feature is enabled; running it standalone gives
    /// an integration check without booting any L2CPU. Bypasses the
    /// daemon.
    TensixEngine,
    /// Drive a known byte pattern through the TRISC0 UART → feed ring.
    ///
    /// Writes from the host into TRISC0's UART input cell (mimicking
    /// the L2CPU kernel's `writel(THR, byte) ; poll(LSR.THRE)` loop),
    /// then reads TRISC0's per-L2CPU feed ring directly to count how
    /// many bytes survived. Bypasses the L2CPU entirely so we can
    /// localize whether residual byte loss is on the kernel→THR path
    /// or the TRISC0→feed-ring path. Daemon must be stopped first.
    UartLoopback {
        /// Number of bytes to send.
        #[arg(long, default_value_t = 1024)]
        count: usize,
        /// Microseconds to sleep between writes.
        ///
        /// In addition to the THRE-poll wait. 0 = back-to-back as fast
        /// as host MMIO will allow. Higher values give TRISC0 more
        /// time per byte.
        #[arg(long, default_value_t = 0)]
        gap_us: u64,
        /// Skip the LSR.THRE poll between writes (stress race window).
        #[arg(long)]
        no_lsr_poll: bool,
    },
}

#[derive(Subcommand)]
enum CloudInitAction {
    /// Build a NoCloud seed ISO suitable for `bhx boot --cloud-init`.
    ///
    /// All fields default to "sensible for a dev box" — running
    /// `bhx cloud-init seed -o seed.iso` with no other flags creates
    /// a `bhx` user with password `bhx`, hostname `bhx-guest`, and a
    /// random instance-id. Override fields explicitly for
    /// production use (operator's SSH key, fixed hostname, etc.).
    Seed {
        /// Output path for the generated ISO.
        #[arg(short = 'o', long)]
        output: String,
        /// Login name to create. Default `bhx`.
        #[arg(long)]
        user: Option<String>,
        /// Plain-text password for the user. cloud-init hashes it
        /// before writing /etc/shadow. Default `bhx`. Pass `--no-
        /// password` to keep the user key-only (requires `--ssh-
        /// key`).
        #[arg(long, conflicts_with = "no_password")]
        password: Option<String>,
        /// Don't set a password — user is key-only. Requires at
        /// least one `--ssh-key`.
        #[arg(long)]
        no_password: bool,
        /// SSH public key file to install (repeatable). Each file's
        /// contents become an entry in the user's
        /// `authorized_keys`.
        #[arg(long = "ssh-key", value_name = "PATH")]
        ssh_keys: Vec<String>,
        /// Guest hostname. Default `bhx-guest`.
        #[arg(long)]
        hostname: Option<String>,
        /// cloud-init instance-id. cloud-init re-runs config modules
        /// when this changes; pin a stable value to avoid that.
        /// Default: random.
        #[arg(long = "instance-id")]
        instance_id: Option<String>,
        /// Path to a YAML file whose contents are appended verbatim
        /// to the generated user-data. Use for arbitrary
        /// cloud-config knobs (`packages:`, `runcmd:`, etc.) without
        /// extending bhx's CLI for every field.
        #[arg(long = "user-data", value_name = "PATH")]
        user_data: Option<String>,
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
        /// Bypass the HTTP-conditional cache and always re-download.
        #[arg(long)]
        refetch: bool,
    },
}

#[derive(Subcommand)]
enum ProfileAction {
    /// Add a new profile.
    ///
    /// Appends a templated stanza for `<name>` to the catalog, then
    /// opens `$VISUAL`/`$EDITOR`/vi on the file so the operator can
    /// fill in the image / network / etc. Validates on save with a
    /// visudo-style retry — broken YAML or a schema violation
    /// re-opens the editor preserving the operator's text.
    Add {
        /// Profile name. Must match `[a-zA-Z][a-zA-Z0-9_-]*`, ≤32
        /// chars. Reserved: must not collide with an existing profile.
        name: String,
    },
    /// Edit the profile catalog.
    ///
    /// Opens `~/.config/bhx/profiles.yaml` in `$EDITOR`. Same
    /// visudo-style retry as `add`.
    Edit,
    /// List all profiles in tabular form.
    List,
    /// Pretty-print one profile's YAML stanza.
    Show {
        /// Profile name to look up.
        name: String,
    },
    /// Remove a profile from the catalog.
    ///
    /// Removes the YAML stanza. Per-instance disks under
    /// `~/.local/share/bhx/instances/<name>-l*/` are left in place —
    /// run `bhx profile reset <name>` first if you want them gone.
    Rm {
        /// Profile name to remove.
        name: String,
    },
    /// Delete instance disks for a profile (next boot re-clones).
    ///
    /// Without `-l`, sweeps every L2CPU's instance directory for the
    /// profile. With `-l <idx>`, only that one. The profile's YAML
    /// stanza is left in place; only the writable copy(s) get
    /// removed. Useful when the template has been re-pulled or the
    /// guest's filesystem has drifted in a way you want to undo.
    Reset {
        /// Profile name to reset.
        name: String,
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

/// Default boot payload when neither `--kernel` nor `--uboot` was
/// passed: peek at the disk basename to see if it matches a known
/// image whose `needs_bootloader` is true, and pick the U-Boot+EFI
/// path in that case. Falls back to the pre-#44 direct-kernel default
/// of `Image` otherwise.
///
/// U-Boot binary lookup prefers `u-boot.bin` symlinked into the
/// caller's cwd (the same convention `rootfs.ext4` follows), and
/// falls back to `third_party/uboot/u-boot.bin` (the in-tree build
/// path that `make -C third_party/uboot` produces). If neither
/// exists, uses `third_party/uboot/u-boot.bin` so the daemon-side
/// error names a concrete path.
/// Resolve a `--image <name>` reference to the canonical on-disk
/// path. Looks up `name` in the registry (so aliases like
/// `debian` → `debian-13` work), derives the canonical artifact
/// filename via the registry's format flags, and confirms the
/// file actually exists. Errors if the image is unknown or hasn't
/// been pulled yet, so the operator gets an actionable message
/// instead of a downstream "no such file" from the boot path.
fn resolve_image_name(name: &str) -> std::io::Result<String> {
    let img = image::get_known_image(name).ok_or_else(|| {
        crate::Error::bad_request(format!(
            "unknown image '{}' — see `bhx image list` for available names",
            name
        ))
    })?;
    let ext = if image::is_single_fs_artifact(img) {
        "ext4"
    } else {
        "img"
    };
    let path = image::image_dir().join(format!("{}.{}", img.name, ext));
    if !path.exists() {
        return Err(crate::Error::bad_request(format!(
            "image '{}' not pulled yet — run `bhx image pull {}` first \
             (expected at {})",
            name,
            img.name,
            path.display()
        ))
        .into());
    }
    Ok(path.to_string_lossy().into_owned())
}

fn default_boot_payload(disk: Option<&str>) -> daemon::protocol::BootPayload {
    if let Some(d) = disk {
        if let Some(img) = image::known_image_for_disk(std::path::Path::new(d)) {
            if img.needs_bootloader {
                return daemon::protocol::BootPayload::Uboot(default_uboot_path());
            }
        }
    }
    daemon::protocol::BootPayload::Kernel("Image".to_string())
}

/// Pick a sensible default U-Boot binary path for the no-`--uboot`
/// flow. Goes through the same firmware-file search the daemon-side
/// resolver uses for `fw_jump.bin` and `blackhole-card.dtb`.
fn default_uboot_path() -> String {
    resolve_firmware_path("u-boot.bin", "third_party/uboot")
}

/// Resolve a firmware filename ("u-boot.bin", "fw_jump.bin",
/// "blackhole-card.dtb") to an actual on-disk path by searching
/// the conventional locations in order:
///
///   1. The bare filename in the caller's cwd (operator override —
///      a symlink in the project root takes precedence over an
///      installed copy).
///   2. `$XDG_DATA_HOME/bhx/firmware/<filename>` (defaults to
///      `~/.local/share/bhx/firmware/`) — where `make install`
///      lands artifacts so a system-wide `bhx` works from any cwd.
///   3. The in-tree `<in_tree_subdir>/<filename>` build output —
///      what `make -C third_party/...` produces, for dev workflows
///      that haven't run `make install`.
///
/// If none of the candidates exist, returns the in-tree path so
/// the daemon-side error names the most-actionable location.
/// Absolute inputs short-circuit and pass through unchanged.
fn resolve_firmware_path(filename: &str, in_tree_subdir: &str) -> String {
    let p = std::path::Path::new(filename);
    if p.is_absolute() {
        return filename.to_string();
    }
    if p.exists() {
        return filename.to_string();
    }
    if let Some(xdg) = xdg_firmware_dir() {
        let candidate = xdg.join(filename);
        if candidate.exists() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    let intree = std::path::PathBuf::from(in_tree_subdir).join(filename);
    if intree.exists() {
        return intree.to_string_lossy().into_owned();
    }
    intree.to_string_lossy().into_owned()
}

/// `$XDG_DATA_HOME/bhx/firmware/`, falling back to
/// `~/.local/share/bhx/firmware/` per the XDG Base Directory spec.
fn xdg_firmware_dir() -> Option<std::path::PathBuf> {
    let base: std::path::PathBuf = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => std::path::PathBuf::from(v),
        _ => std::path::PathBuf::from(std::env::var_os("HOME")?).join(".local/share"),
    };
    Some(base.join("bhx/firmware"))
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
            no_virtio_console,
            no_virtio_rng,
            fwd,
            attach,
            memory,
            hostname,
            profile,
            cloud_init,
            no_cidata,
        }) => {
            let (card, l2cpu) = resolve_target(&cli.l2cpu, cli.ttdevice)?;
            // `--image <name>` is a registry-name shortcut: resolve
            // it to the canonical pulled path and feed the rest of
            // the boot flow as if the operator had typed
            // `--disk <full-path>`. clap's `conflicts_with = "disk"`
            // already enforces mutual exclusion.
            //
            // Also remember which arrow we took so the eprintln below
            // can mirror the cloud-init seed line and tell the operator
            // exactly which file is being attached as the rootfs.
            let (disk_arg, disk_source): (Option<String>, &'static str) =
                match (cli.image.as_deref(), cli.disk) {
                    (Some(_), Some(_)) => unreachable!("clap conflicts_with"),
                    (Some(name), None) => (Some(resolve_image_name(name)?), "image"),
                    (None, Some(p)) => (Some(p), "--disk"),
                    (None, None) => (None, "default in cwd"),
                };
            let image_name_for_print = cli.image.clone();
            if let Some(profile_name) = profile {
                run_boot_via_profile(
                    card,
                    l2cpu,
                    &profile_name,
                    opensbi,
                    dtb,
                    initramfs,
                    root_device,
                    force_reset_pcie,
                    force,
                    disk_arg.as_deref(),
                    cli.network,
                )?;
                if attach {
                    run_connect_client(card, l2cpu, daemon::protocol::ConsoleMode::Rw)?;
                }
                return Ok(());
            }
            let disk = resolve_disk_path(
                disk_arg,
                DEFAULT_DISK_PATH,
                std::path::Path::new(DEFAULT_DISK_PATH).exists(),
            );
            // clap's `conflicts_with` already enforces mutual exclusion;
            // here we just pick the variant. With neither flag given,
            // peek at the disk path: if it maps to a known image whose
            // entry has `needs_bootloader=true` (e.g. AlmaLinux's GPT
            // cloud image), default to the U-Boot+EFI path with
            // `u-boot.bin` in cwd. Otherwise fall back to the pre-#44
            // direct-kernel default of `Image`.
            let payload = match (kernel, uboot) {
                (Some(_), Some(_)) => unreachable!("clap conflicts_with"),
                (_, Some(p)) => daemon::protocol::BootPayload::Uboot(p),
                (Some(p), None) => daemon::protocol::BootPayload::Kernel(p),
                (None, None) => default_boot_payload(disk.as_deref()),
            };
            // #115 auto-cidata: if no explicit `--cloud-init` was
            // given and `--no-cidata` isn't set, look for the sibling
            // seed `<disk>.cidata.img` written by `bhx image pull`.
            // Picks up the default `bhx`/`bhx` user automatically.
            let (cloud_init, cloud_init_source) = match (cloud_init, no_cidata) {
                (Some(p), _) => (Some(p), "explicit --cloud-init"),
                (None, true) => (None, "suppressed by --no-cidata"),
                (None, false) => match disk.as_deref().and_then(|d| {
                    let candidate = image::cidata_seed_path_for(std::path::Path::new(d));
                    if candidate.exists() {
                        Some(candidate.to_string_lossy().into_owned())
                    } else {
                        None
                    }
                }) {
                    Some(p) => (Some(p), "auto-attached sibling"),
                    None => (None, "no seed found"),
                },
            };
            // Print what we're attaching (or not) so the operator
            // isn't surprised by an invisible 2nd virtio-blk slot,
            // or by a missing one if they expected the auto-attach.
            if let Some(p) = disk.as_deref() {
                let source = match (disk_source, image_name_for_print.as_deref()) {
                    ("image", Some(name)) => format!("image: {}", name),
                    (s, _) => s.to_string(),
                };
                eprintln!("Disk: {} ({})", p, source);
            }
            match (&cloud_init, cloud_init_source) {
                (Some(p), src) => eprintln!("Cloud-init seed: {} ({})", p, src),
                (None, "no seed found") if disk.is_some() => {
                    // Only mention the absence when there's actually
                    // a disk in the picture — initramfs-only boots
                    // don't care.
                    eprintln!(
                        "Cloud-init seed: none (no `<disk>.cidata.img` sibling; \
                         pass --cloud-init <path> if needed)"
                    );
                }
                (None, _) => {}
            }
            run_boot_client(
                card,
                l2cpu,
                opensbi,
                payload,
                dtb,
                initramfs,
                root_device,
                force_reset_pcie,
                disk,
                cli.network,
                fwd,
                !no_virtio_console,
                !no_virtio_rng,
                force,
                memory,
                hostname,
                cloud_init,
            )?;
            if attach {
                run_connect_client(card, l2cpu, daemon::protocol::ConsoleMode::Rw)?;
            }
            Ok(())
        }
        Some(Commands::Connect { mode }) => {
            let pmode = parse_console_mode(&mode)?;
            let (card, l2cpu) = resolve_target(&cli.l2cpu, cli.ttdevice)?;
            run_connect_client(card, l2cpu, pmode)
        }
        Some(Commands::Stop) => {
            let (card, l2cpu) = resolve_target(&cli.l2cpu, cli.ttdevice)?;
            let mut sock = daemon::client::connect(card)?;
            daemon::client::stop_l2cpu(&mut sock, l2cpu)
        }
        Some(Commands::Status) => daemon::runner::status(resolve_card(&cli.l2cpu, cli.ttdevice)?),
        Some(Commands::AddDisk { path, name }) => {
            // Canonicalize client-side — daemon runs with cwd=/ after
            // double-fork, so relative paths from the user's shell would
            // resolve against the wrong base otherwise.
            let path = absolutize(&path)?;
            let (card, l2cpu) = resolve_target(&cli.l2cpu, cli.ttdevice)?;
            let mut sock = daemon::client::connect(card)?;
            daemon::client::add_disk(&mut sock, l2cpu, path, name)
        }
        Some(Commands::RemoveDisk { name }) => {
            let (card, l2cpu) = resolve_target(&cli.l2cpu, cli.ttdevice)?;
            let mut sock = daemon::client::connect(card)?;
            daemon::client::remove_disk(&mut sock, l2cpu, name)
        }
        Some(Commands::AddNet { ssh_port, fwd }) => {
            let (card, l2cpu) = resolve_target(&cli.l2cpu, cli.ttdevice)?;
            let mut sock = daemon::client::connect(card)?;
            daemon::client::add_net(&mut sock, l2cpu, ssh_port, fwd)
        }
        Some(Commands::RemoveNet) => {
            let (card, l2cpu) = resolve_target(&cli.l2cpu, cli.ttdevice)?;
            let mut sock = daemon::client::connect(card)?;
            daemon::client::remove_net(&mut sock, l2cpu)
        }
        Some(Commands::AddConsole) => {
            let (card, l2cpu) = resolve_target(&cli.l2cpu, cli.ttdevice)?;
            let mut sock = daemon::client::connect(card)?;
            daemon::client::add_console(&mut sock, l2cpu)
        }
        Some(Commands::RemoveConsole) => {
            let (card, l2cpu) = resolve_target(&cli.l2cpu, cli.ttdevice)?;
            let mut sock = daemon::client::connect(card)?;
            daemon::client::remove_console(&mut sock, l2cpu)
        }
        Some(Commands::Daemon { action }) => {
            run_daemon_cmd(resolve_card(&cli.l2cpu, cli.ttdevice)?, action)
        }
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
                ImageAction::Verify { name } => {
                    image::cmd_verify(name.as_deref());
                }
            }
            Ok(())
        }
        Some(Commands::CloudInit { action }) => match action {
            CloudInitAction::Seed {
                output,
                user,
                password,
                no_password,
                ssh_keys,
                hostname,
                instance_id,
                user_data,
            } => cmd_cloud_init_seed(
                &output,
                user,
                password,
                no_password,
                ssh_keys,
                hostname,
                instance_id,
                user_data.as_deref(),
            ),
        },
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
        Some(Commands::Profile { action }) => match action {
            ProfileAction::Add { name } => cmd_profile_add(&name),
            ProfileAction::Edit => cmd_profile_edit(),
            ProfileAction::List => cmd_profile_list(),
            ProfileAction::Show { name } => cmd_profile_show(&name),
            ProfileAction::Rm { name } => cmd_profile_rm(&name),
            ProfileAction::Reset { name } => {
                // `-l` is the global L2CPU locator. With `-l <plain
                // N>` it scopes to one l2cpu; with the unset/default
                // locator we still got `Some((card, 0))` back, so
                // distinguish "user didn't set -l" from "user said
                // -l 0" by checking the raw locator string.
                let l2cpu_filter =
                    if cli.l2cpu == "0" && std::env::args().all(|a| a != "-l" && a != "--l2cpu") {
                        None
                    } else {
                        Some(resolve_target(&cli.l2cpu, cli.ttdevice)?.1)
                    };
                cmd_profile_reset(&name, l2cpu_filter)
            }
        },
        Some(Commands::Debug { action }) => {
            let (card, l2cpu) = resolve_target(&cli.l2cpu, cli.ttdevice)?;
            run_debug_cmd(card, l2cpu as usize, action)
        }
        None => {
            // Bare invocation → attach console in rw mode, same as `connect`.
            let (card, l2cpu) = resolve_target(&cli.l2cpu, cli.ttdevice)?;
            run_connect_client(card, l2cpu, daemon::protocol::ConsoleMode::Rw)
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

/// Parse the `-l` value into `(Option<card>, l2cpu_idx)`. Plain `<N>`
/// returns no card override; `<C>:<N>` returns one. Multi-card hosts use
/// the prefix form; single-card scripts can keep using plain numbers.
fn parse_l2cpu_locator(s: &str) -> std::result::Result<(Option<u32>, u8), String> {
    let (card_str, l2cpu_str) = match s.split_once(':') {
        Some((c, n)) => {
            if c.is_empty() {
                return Err(format!("locator missing card before ':' in {:?}", s));
            }
            if n.is_empty() {
                return Err(format!("locator missing l2cpu after ':' in {:?}", s));
            }
            (Some(c), n)
        }
        None => (None, s),
    };
    let card = match card_str {
        Some(c) => Some(
            c.parse::<u32>()
                .map_err(|_| format!("locator card not a number: {:?}", c))?,
        ),
        None => None,
    };
    let l2cpu = l2cpu_str
        .parse::<u8>()
        .map_err(|_| format!("l2cpu not a number: {:?}", l2cpu_str))?;
    if l2cpu > 3 {
        return Err(format!("l2cpu index {} out of range (0-3)", l2cpu));
    }
    Ok((card, l2cpu))
}

/// Resolve `(card, l2cpu)` from the global flags. Errors if the locator
/// carries a card prefix that disagrees with an explicit `-t`. Falls
/// back to card 0 when neither is given.
fn resolve_target(loc: &str, ttdevice: Option<u32>) -> std::io::Result<(u32, u8)> {
    let (loc_card, l2cpu) =
        parse_l2cpu_locator(loc).map_err(|e| std::io::Error::from(crate::Error::bad_request(e)))?;
    let card = match (loc_card, ttdevice) {
        (Some(c), Some(t)) if c != t => {
            return Err(std::io::Error::from(crate::Error::bad_request(format!(
                "conflicting card: -l {}:{} vs -t {} (drop one)",
                c, l2cpu, t
            ))));
        }
        (Some(c), _) => c,
        (None, Some(t)) => t,
        (None, None) => 0,
    };
    Ok((card, l2cpu))
}

/// Resolve just the card for subcommands that don't take an L2CPU
/// (daemon lifecycle, status). Honors a `<C>:<N>` locator prefix even
/// though the L2CPU index is unused, and surfaces a card conflict the
/// same way `resolve_target` does.
fn resolve_card(loc: &str, ttdevice: Option<u32>) -> std::io::Result<u32> {
    let (loc_card, _) =
        parse_l2cpu_locator(loc).map_err(|e| std::io::Error::from(crate::Error::bad_request(e)))?;
    let card = match (loc_card, ttdevice) {
        (Some(c), Some(t)) if c != t => {
            return Err(std::io::Error::from(crate::Error::bad_request(format!(
                "conflicting card: -l {}:* vs -t {} (drop one)",
                c, t
            ))));
        }
        (Some(c), _) => c,
        (None, Some(t)) => t,
        (None, None) => 0,
    };
    Ok(card)
}

fn parse_console_mode(s: &str) -> std::io::Result<daemon::protocol::ConsoleMode> {
    match s {
        "ro" => Ok(daemon::protocol::ConsoleMode::Ro),
        "rw" => Ok(daemon::protocol::ConsoleMode::Rw),
        "takeover" => Ok(daemon::protocol::ConsoleMode::Takeover),
        other => Err(crate::Error::bad_request(format!(
            "invalid --mode {}; expected ro|rw|takeover",
            other
        ))
        .into()),
    }
}

/// Parse `HOST:GUEST` for `add-net --fwd`. Both sides must be in the
/// 1..=65535 range; bare numbers, leading whitespace, and missing
/// halves all error out cleanly.
/// Parse an operator-friendly memory size string into a byte count.
/// Accepts plain integers (interpreted as bytes) and suffixed forms
/// in either SI (`KB`/`MB`/`GB`) or IEC binary (`KiB`/`MiB`/`GiB`)
/// notation. The number portion can carry a decimal point; the
/// daemon clamps to the L2CPU's physical size and 2 MiB-aligns.
///
/// Examples:
///   - "2GB"   -> 2_000_000_000
///   - "2GiB"  -> 2_147_483_648
///   - "2048MB" -> 2_048_000_000
///   - "1.5GiB" -> 1_610_612_736
fn parse_memory(s: &str) -> std::io::Result<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(crate::Error::bad_request("empty --memory value").into());
    }
    let (num_part, mult) = if let Some(rest) = trimmed.strip_suffix("GiB") {
        (rest, 1u64 << 30)
    } else if let Some(rest) = trimmed.strip_suffix("MiB") {
        (rest, 1u64 << 20)
    } else if let Some(rest) = trimmed.strip_suffix("KiB") {
        (rest, 1u64 << 10)
    } else if let Some(rest) = trimmed.strip_suffix("GB") {
        (rest, 1_000_000_000u64)
    } else if let Some(rest) = trimmed.strip_suffix("MB") {
        (rest, 1_000_000u64)
    } else if let Some(rest) = trimmed.strip_suffix("KB") {
        (rest, 1_000u64)
    } else if let Some(rest) = trimmed.strip_suffix('B') {
        (rest, 1u64)
    } else {
        (trimmed, 1u64)
    };
    let num: f64 = num_part.trim().parse().map_err(|_| {
        std::io::Error::from(crate::Error::bad_request(format!(
            "invalid --memory {:?}; expected e.g. 2GB or 2GiB",
            s
        )))
    })?;
    if !num.is_finite() || num <= 0.0 {
        return Err(
            crate::Error::bad_request(format!("--memory must be positive: {:?}", s)).into(),
        );
    }
    Ok((num * mult as f64) as u64)
}

/// RFC-952 hostname check: 1..=63 chars from `a-z0-9-`, lowercase,
/// no leading/trailing `-`. Strict so a malformed override doesn't
/// trip the slirp DHCP server's parser silently. Per RFC-1123 we
/// also allow leading digits.
fn parse_hostname(s: &str) -> std::io::Result<String> {
    let bad = |reason: &str| -> std::io::Error {
        std::io::Error::from(crate::Error::bad_request(format!(
            "invalid --hostname {:?}: {}",
            s, reason
        )))
    };
    if s.is_empty() {
        return Err(bad("empty"));
    }
    if s.len() > 63 {
        return Err(bad("longer than 63 chars (RFC 952)"));
    }
    if s.starts_with('-') || s.ends_with('-') {
        return Err(bad("must not start or end with '-'"));
    }
    for c in s.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(bad("only lowercase a-z, 0-9, '-' allowed"));
        }
    }
    Ok(s.to_string())
}

fn parse_fwd_pair(s: &str) -> std::io::Result<(u16, u16)> {
    let (h, g) = s.split_once(':').ok_or_else(|| {
        std::io::Error::from(crate::Error::bad_request(format!(
            "invalid --fwd {:?}; expected HOST:GUEST",
            s
        )))
    })?;
    let host: u16 = h.parse().map_err(|_| {
        std::io::Error::from(crate::Error::bad_request(format!(
            "invalid --fwd HOST {:?}",
            h
        )))
    })?;
    let guest: u16 = g.parse().map_err(|_| {
        std::io::Error::from(crate::Error::bad_request(format!(
            "invalid --fwd GUEST {:?}",
            g
        )))
    })?;
    if host == 0 || guest == 0 {
        return Err(crate::Error::bad_request(format!(
            "invalid --fwd {:?}; ports must be 1..=65535",
            s
        ))
        .into());
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
    memory_override: Option<u64>,
    hostname_override: Option<String>,
    cloud_init: Option<String>,
) -> std::io::Result<()> {
    // Bundle disk + network into the Boot RPC so the virtio workers come up
    // together with the L2CPU reset release. The guest kernel hits its VFS
    // rootfs mount at ~0.137s and doesn't retry — issuing add-disk as a
    // separate RPC loses that race.
    //
    // Paths are canonicalized here (client side) because the daemon runs
    // from cwd=/, so relative paths from the user's shell wouldn't resolve.
    // Firmware artifacts (opensbi/dtb/uboot) get a search-path lookup
    // first so a `make install`-d bhx run from any cwd still finds them
    // under `$XDG_DATA_HOME/bhx/firmware/`. User-supplied kernel/initramfs
    // paths are taken verbatim — those are operator content, not firmware.
    let opensbi = absolutize(&resolve_firmware_path(&opensbi, "third_party/opensbi"))?;
    let payload = match payload {
        daemon::protocol::BootPayload::Kernel(p) => {
            daemon::protocol::BootPayload::Kernel(absolutize(&p)?)
        }
        daemon::protocol::BootPayload::Uboot(p) => daemon::protocol::BootPayload::Uboot(
            absolutize(&resolve_firmware_path(&p, "third_party/uboot"))?,
        ),
    };
    let dtb = absolutize(&resolve_firmware_path(&dtb, "third_party/dtb"))?;
    let initramfs = initramfs.map(|p| absolutize(&p)).transpose()?;
    let disk = disk.map(|p| absolutize(&p)).transpose()?;
    let cloud_init = cloud_init.map(|p| absolutize(&p)).transpose()?;
    ensure_daemon_running(card)?;
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
        memory_override,
        hostname_override,
        cloud_init,
    )
}

/// Auto-start the per-card daemon if it isn't already running. Limited
/// to `bhx boot` (and the profile-driven boot path it shares); other
/// subcommands are intentionally NOT covered — `connect`/`status`/
/// `add-disk`/`stop` against a stopped daemon usually means
/// "the daemon I expected just died" or "wrong shell," and silently
/// starting one would mask that. `boot` is the natural session-start
/// verb where auto-start is unsurprising. (#134)
fn ensure_daemon_running(card: u32) -> std::io::Result<()> {
    if daemon::lifetime::is_running(card) {
        return Ok(());
    }
    eprintln!("[daemon] not running for card {} — auto-starting", card);
    daemon::runner::start(daemon::runner::StartOpts {
        card,
        foreground: false,
        log_file: None,
        sandbox: true,
        metrics_port: None,
    })
}

fn absolutize(path: &str) -> std::io::Result<String> {
    let p = std::path::Path::new(path);
    let abs = std::fs::canonicalize(p)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {}", path, e)))?;
    abs.into_os_string().into_string().map_err(|_| {
        std::io::Error::from(crate::Error::bad_request(format!(
            "non-UTF-8 path: {}",
            path
        )))
    })
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
        return Err(crate::Error::slot_state(format!(
            "daemon is running for card {} — refusing to write chip state from outside the daemon \
             (stop the daemon first with `bhx daemon stop`, then retry)",
            card
        ))
        .into());
    }
    if daemon_up {
        eprintln!(
            "[debug] warning: daemon is running for card {} — read is racy with daemon's own ops",
            card
        );
    }

    let chip = shared_chip::SharedChip::new(card).map_err(|e| {
        std::io::Error::from(crate::Error::Io {
            ctx: format!("open /dev/tenstorrent/{}", card),
            source: e,
        })
    })?;
    match action {
        DebugAction::ReadResetReg => {
            let reg = 0x80030014u64;
            let val = chip.arc_read32(reg)?;
            println!("L2CPU_RESET@0x{:x} = {:#010x}", reg, val);
            for i in 0..4 {
                let bit = (val >> (i + 4)) & 1;
                println!("  bit {} (L2CPU {} release): {}", i + 4, i, bit);
            }
            Ok(())
        }
        DebugAction::ResetX280 => {
            if l2cpu > 3 {
                return Err(crate::Error::bad_request("l2cpu must be 0..3").into());
            }
            eprintln!(
                "[debug] invoking SharedChip::reset_x280 on L2CPU {} (PLL step + OR-in bit {})",
                l2cpu,
                l2cpu + 4
            );
            chip.reset_x280(&[l2cpu])?;
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
        DebugAction::UartLoopback {
            count,
            gap_us,
            no_lsr_poll,
        } => run_uart_loopback(card, &chip, l2cpu as u8, count, gap_us, no_lsr_poll),
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
    // Diagnostic path doesn't drive guest-poweroff dispatch — discard
    // the receiver so any spurious shutdown event from a real boot in
    // progress doesn't accumulate.
    let (gp_tx, _gp_rx) = std::sync::mpsc::channel::<u8>();
    let mut poller = tensix_data_plane::KickPoller::spawn(Arc::clone(&engine), gp_tx);
    let stats = Arc::clone(&poller.stats);

    // BRISC's main loop only polls slots whose bit is set in the
    // active-slots bitmap (M7 optimization to keep the per-slot revisit
    // period tight when only a few slots are in use). Set bit 7 first
    // so the synthetic notify is observed.
    engine.write_active_slots(1u32 << 7);

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
        return Err(crate::Error::internal(format!(
            "kick poller did not consume our synthetic kick within 100 ms \
             (kicks_consumed stayed at {})",
            before
        ))
        .into());
    }

    // M6.1 (#79) Phase A/B verification: the active-slots bitmap drives
    // TRISC0's reset lifecycle (BRISC owns the soft-reset bit). Without
    // any UART bit set, TRISC0 is held in reset and its heartbeat must
    // not advance. Setting any of bits 16..19 releases TRISC0; clearing
    // them re-asserts.
    eprintln!("[tensix-engine] M6.1: verifying TRISC0 lifecycle");
    engine.write_active_slots(1u32 << 7); // UART bits clear
    std::thread::sleep(Duration::from_millis(20));
    let hb_quiet = engine.trisc0_heartbeat();
    std::thread::sleep(Duration::from_millis(20));
    let hb_quiet2 = engine.trisc0_heartbeat();
    if hb_quiet != hb_quiet2 {
        poller.shutdown();
        return Err(crate::Error::internal(format!(
            "TRISC0 heartbeat advanced ({} → {}) while UART bits clear; \
             BRISC isn't holding TRISC0 in reset",
            hb_quiet, hb_quiet2
        ))
        .into());
    }
    eprintln!(
        "[tensix-engine]   TRISC0 heartbeat held at {} (in reset)",
        hb_quiet
    );

    // Set bit 16 (UART for L2CPU 0) — BRISC must release TRISC0.
    engine.write_active_slots((1u32 << 7) | (1u32 << 16));
    std::thread::sleep(Duration::from_millis(20));
    let hb_running = engine.trisc0_heartbeat();
    std::thread::sleep(Duration::from_millis(20));
    let hb_running2 = engine.trisc0_heartbeat();
    if hb_running2 <= hb_running {
        poller.shutdown();
        return Err(crate::Error::internal(format!(
            "TRISC0 heartbeat did not advance after setting UART bit 16 \
             ({} → {}); release path not working",
            hb_running, hb_running2
        ))
        .into());
    }
    eprintln!(
        "[tensix-engine]   TRISC0 heartbeat advanced {} → {} after UART register",
        hb_running, hb_running2
    );

    // Clear bit 16 — BRISC must re-assert TRISC0.
    engine.write_active_slots(1u32 << 7);
    std::thread::sleep(Duration::from_millis(20));
    let hb_after_unreg = engine.trisc0_heartbeat();
    std::thread::sleep(Duration::from_millis(20));
    let hb_after_unreg2 = engine.trisc0_heartbeat();
    if hb_after_unreg != hb_after_unreg2 {
        poller.shutdown();
        return Err(crate::Error::internal(format!(
            "TRISC0 heartbeat still advancing after clearing UART bit 16 \
             ({} → {}); re-assert path not working",
            hb_after_unreg, hb_after_unreg2
        ))
        .into());
    }
    eprintln!(
        "[tensix-engine]   TRISC0 heartbeat froze at {} after UART unregister — TRISC0 LIFECYCLE PASS",
        hb_after_unreg
    );

    poller.shutdown();
    Ok(())
}

/// Host-driven byte-pattern test for the M6.1 (#79) UART path.
///
/// Brings up the engine, releases TRISC0 by setting bit 16 of the
/// active-slots bitmap, then writes a known sequence of bytes into the
/// THR cell from the host (mimicking what the L2CPU kernel's
/// `serial8250_console_putchar` would do). Reads TRISC0's per-L2CPU
/// feed ring in BRISC L1 directly to count exactly how many bytes
/// landed and in what order — bypasses the daemon, the kernel, and
/// the L2CPU NoC entirely so we can localize residual byte loss.
///
/// Three numbers come out of this:
///
///   * **sent**: how many bytes the host wrote to THR.
///   * **producer-seq**: how many bytes TRISC0 pushed to the feed
///     ring. `sent - producer_seq` = bytes the kernel→THR path lost
///     (kernel saw stale THRE=1 and overwrote a byte before TRISC0
///     could read it, OR TRISC0 missed a byte for some other reason).
///   * **errors**: how many of the bytes TRISC0 captured don't match
///     the expected pattern at the position the kick poller delivered
///     them. Tells us if there's a *content* corruption bug separate
///     from byte loss.
fn run_uart_loopback(
    card: u32,
    chip: &shared_chip::SharedChip,
    l2cpu_idx: u8,
    count: usize,
    gap_us: u64,
    no_lsr_poll: bool,
) -> std::io::Result<()> {
    use std::sync::Arc;
    use std::time::Duration;

    if l2cpu_idx >= 4 {
        return Err(crate::Error::bad_request(format!(
            "uart-loopback: l2cpu must be 0..3 (got {})",
            l2cpu_idx
        ))
        .into());
    }

    eprintln!("[uart-loopback] bringing up engine…");
    let engine = Arc::new(tensix_engine::TensixEngine::bring_up(card, chip)?);
    eprintln!(
        "[uart-loopback] engine up: tile NOC0 ({}, {}), firmware {:#010x}, protocol v{}",
        engine.noc0_x, engine.noc0_y, engine.firmware_version, engine.protocol_version
    );

    // Set the UART slot's bit so BRISC releases TRISC0. We don't run
    // a kick poller here — we read the feed ring directly below.
    let uart_bit = 1u32 << (uart_engine::UART_SLOT_BASE + l2cpu_idx as u16);
    engine.write_active_slots(uart_bit);
    // Give BRISC's main loop a beat to observe the bitmap and
    // release TRISC0, plus TRISC0 a beat to enter its poll loop.
    std::thread::sleep(Duration::from_millis(50));

    // Verify TRISC0 is alive: heartbeat should advance.
    let hb0 = engine.trisc0_heartbeat();
    std::thread::sleep(Duration::from_millis(10));
    let hb1 = engine.trisc0_heartbeat();
    if hb1 <= hb0 {
        return Err(crate::Error::internal(format!(
            "TRISC0 heartbeat not advancing ({} → {}); refusing to send",
            hb0, hb1
        ))
        .into());
    }
    eprintln!("[uart-loopback] TRISC0 alive (heartbeat {} → {})", hb0, hb1);

    // L1 addresses for this UART. reg-shift=2 (4-byte stride per
    // register) matches the firmware layout in `uart_layout.h`.
    let uart_base = 0x40000u32 + (l2cpu_idx as u32) * 0x4000;
    let thr_addr = uart_base; // UART_REG_RBR_THR = 0x00
    let lsr_addr = uart_base + 0x14; // UART_REG_LSR

    let priv_base = uart_engine::uart_private_base(l2cpu_idx);
    let producer_addr = priv_base + uart_engine::UART_PRIV_OFF_FEED_PRODUCER_SEQ;
    let consumer_addr = priv_base + uart_engine::UART_PRIV_OFF_FEED_CONSUMER_SEQ;
    let drops_addr = priv_base + uart_engine::UART_PRIV_OFF_FEED_DROP_COUNT;
    let ring_base = priv_base + uart_engine::UART_PRIV_OFF_FEED_RING;

    // Snapshot starting producer/drops so we can compute deltas
    // (TRISC0 may have produced "noise" bytes during bring-up).
    let p_start = engine.read_l1_u32(producer_addr);
    let d_start = engine.read_l1_u32(drops_addr);
    // Sync the consumer to the producer so the ring entries we read
    // back later belong to *our* bytes only.
    engine.write_l1_u32(consumer_addr, p_start);

    // Pattern: cycling 'A'..'P' (16 distinct printable bytes). The
    // distinct values let us catch reordering or duplicate-byte loss
    // separately from sheer drop count.
    let pattern: Vec<u8> = (0..count).map(|i| b'A' + ((i % 16) as u8)).collect();

    eprintln!(
        "[uart-loopback] sending {} bytes (gap={} µs, lsr_poll={})…",
        count, gap_us, !no_lsr_poll
    );
    let started = std::time::Instant::now();
    let mut lsr_timeouts = 0u64;
    for &b in &pattern {
        if !no_lsr_poll {
            // Poll LSR.THRE = 1 (bit 5) before writing, exactly like
            // the kernel's `wait_for_xmitr` loop.
            let mut tmout = 10_000u32;
            loop {
                let lsr = engine.read_l1_u32(lsr_addr);
                if lsr & 0x20 != 0 {
                    break;
                }
                tmout -= 1;
                if tmout == 0 {
                    lsr_timeouts += 1;
                    break;
                }
            }
        }
        engine.write_l1_u32(thr_addr, b as u32);
        if gap_us > 0 {
            std::thread::sleep(Duration::from_micros(gap_us));
        }
    }
    let elapsed = started.elapsed();
    eprintln!(
        "[uart-loopback] sent {} bytes in {:.3} ms ({:.0} B/s); lsr-timeouts: {}",
        count,
        elapsed.as_secs_f64() * 1000.0,
        count as f64 / elapsed.as_secs_f64(),
        lsr_timeouts,
    );

    // Give TRISC0 a beat to drain any in-flight reads.
    std::thread::sleep(Duration::from_millis(50));

    let p_end = engine.read_l1_u32(producer_addr);
    let d_end = engine.read_l1_u32(drops_addr);
    let captured = p_end.wrapping_sub(p_start);
    let drops = d_end.wrapping_sub(d_start);
    eprintln!(
        "[uart-loopback] feed ring: producer {} → {} (Δ {}), drops {} → {} (Δ {})",
        p_start, p_end, captured, d_start, d_end, drops
    );

    // Read the captured bytes back from the ring and compare to the
    // expected pattern. We compare position by position — we know
    // TRISC0 doesn't reorder, so any mismatch means a byte was lost
    // (kernel→THR race) and the position-N expected byte got eaten.
    let mask = uart_engine::UART_FEED_RING_ENTRIES - 1;
    let take = std::cmp::min(captured as usize, count);
    let mut received = Vec::with_capacity(take);
    for i in 0..(take as u32) {
        let idx = (p_start.wrapping_add(i)) & mask;
        let cell = engine.read_l1_u32(ring_base + idx * 4);
        received.push((cell & 0xFF) as u8);
    }

    // Slot-by-slot match: scan received against the pattern,
    // advancing the pattern index whenever we get a match. Any
    // skipped pattern slot is a "lost byte." The number of received
    // bytes that don't fit the pattern even with skips = corruption.
    let mut pat_i = 0usize;
    let mut lost_in_stream = 0usize;
    let mut corrupted = 0usize;
    for &b in &received {
        // Find the next pattern position matching this byte.
        let mut found = false;
        while pat_i < pattern.len() {
            if pattern[pat_i] == b {
                pat_i += 1;
                found = true;
                break;
            }
            pat_i += 1;
            lost_in_stream += 1;
        }
        if !found {
            corrupted += 1;
        }
    }
    let lost_at_tail = pattern.len().saturating_sub(pat_i);

    let pct = |n: usize| (n as f64 / count as f64) * 100.0;
    eprintln!("[uart-loopback] sent     : {}", count);
    eprintln!(
        "[uart-loopback] captured : {} ({:.1}% of sent)",
        captured,
        pct(captured as usize)
    );
    eprintln!("[uart-loopback] dropped  : {}  (feed-ring full)", drops);
    eprintln!(
        "[uart-loopback] lost in stream: {}  ({:.1}%)",
        lost_in_stream,
        pct(lost_in_stream)
    );
    eprintln!("[uart-loopback] lost at tail : {}", lost_at_tail);
    eprintln!(
        "[uart-loopback] corrupted (byte didn't match pattern): {}",
        corrupted
    );

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
    let telem = telemetry::read_telemetry(chip).map_err(|e| {
        std::io::Error::from(crate::Error::internal(format!("read telemetry: {}", e)))
    })?;
    let picked = tensix_tile::pick_virtio_engine_tile(&telem)
        .map_err(|e| std::io::Error::from(crate::Error::internal(format!("pick tile: {}", e))))?;
    eprintln!(
        "[tensix-hello] picker chose ({}, {}) [{:?}]",
        picked.x, picked.y, picked.reason
    );
    Ok((picked.x, picked.y))
}

fn run_telemetry_dump(chip: &shared_chip::SharedChip, all_tags: bool) -> std::io::Result<()> {
    let table_addr = chip.arc_read32(telemetry::ARC_TELEMETRY_PTR_ADDR)?;
    println!(
        "SCRATCH_RAM[13] @ {:#010x} = {:#010x} (telemetry table base)",
        telemetry::ARC_TELEMETRY_PTR_ADDR,
        table_addr
    );
    let telem = telemetry::read_telemetry(chip).map_err(|e| {
        std::io::Error::from(crate::Error::internal(format!("read telemetry: {}", e)))
    })?;
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
    let telem = telemetry::read_telemetry(chip).map_err(|e| {
        std::io::Error::from(crate::Error::internal(format!("read telemetry: {}", e)))
    })?;
    let picked = tensix_tile::pick_virtio_engine_tile(&telem)
        .map_err(|e| std::io::Error::from(crate::Error::internal(format!("pick tile: {}", e))))?;
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

    let tile = TensixTile::new(card, x, y).map_err(|e| {
        std::io::Error::from(crate::Error::Io {
            ctx: format!("open tile ({}, {})", x, y),
            source: e,
        })
    })?;

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
        Err(crate::Error::internal(format!(
            "FAIL: magic_observed={}, counter_advanced={} after {:.1?}",
            magic_observed, counter_advanced, elapsed
        ))
        .into())
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

    let tile = TensixTile::new(card, x, y).map_err(|e| {
        std::io::Error::from(crate::Error::Io {
            ctx: format!("open tile ({}, {})", x, y),
            source: e,
        })
    })?;

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
                return Err(crate::Error::internal(format!(
                    "M5 hello-ack timeout after {:?} (got {:#010x})",
                    timeout, m
                ))
                .into());
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
            return Err(crate::Error::internal(format!(
                "M5 protocol version mismatch: daemon expects {}, firmware reported {}",
                proto::PROTOCOL_VERSION,
                proto_v
            ))
            .into());
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

    // BRISC's poll loop is gated on `CTRL_OFF_ACTIVE_SLOTS` (#71
    // M5.5b) — slots whose bit isn't set are skipped on every sweep.
    // The smoke test below drives writes on slots 0, 1, 5, 7 and
    // expects BRISC to observe each one, so set every virtio bit
    // (UART bits 16..19 stay clear since we're not exercising
    // TRISC0 here).
    {
        use tensix_proto as proto;
        tile.write_l1_u32(proto::CTRL_BASE + proto::CTRL_OFF_ACTIVE_SLOTS, 0x0000FFFF);
        sleep(Duration::from_millis(2));
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
        // Switch back to SEL=0 (low half). The firmware intentionally
        // keeps DEVICE_FEATURES static at 0x1 regardless of SEL to
        // dodge the readl-after-writel race with stock Linux virtio
        // drivers (see virtio.c around DEVICE_FEATURES handling) —
        // bit 0 in the low half is undefined and stock drivers
        // ignore unknown bits, so VIRTIO_F_VERSION_1 still
        // negotiates correctly. The test mirrors that behavior.
        tile.write_l1_u32(slot0_dev_feat_sel, 0);
        sleep(Duration::from_millis(5));
        let low_half = tile.read_l1_u32(slot0_dev_feat);
        if low_half == 1 {
            eprintln!(
                "  DEVICE_FEATURES (low half) = {:#010x} — static-by-design — PASS",
                low_half
            );
        } else {
            eprintln!(
                "  FEATURES FAIL: DEVICE_FEATURES (low half) = {:#010x}, expected 1 \
                 (firmware keeps DEVICE_FEATURES static across SEL switches)",
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
        Err(crate::Error::internal(format!("FAIL: {} subtests failed", errors)).into())
    }
}

fn toggle_reset_bit(
    chip: &shared_chip::SharedChip,
    l2cpu: usize,
    release: bool,
) -> std::io::Result<()> {
    if l2cpu > 3 {
        return Err(crate::Error::bad_request("l2cpu must be 0..3").into());
    }
    let reg: u64 = 0x80030014;
    let bit = 1u32 << (l2cpu + 4);
    let before = chip.arc_read32(reg)?;
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
    chip.arc_write32(reg, after)?;
    let readback = chip.arc_read32(reg)?;
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

// ============================================================================
// Cloud-init seed CLI (#82)
// ============================================================================

#[allow(clippy::too_many_arguments)]
fn cmd_cloud_init_seed(
    output: &str,
    user: Option<String>,
    password: Option<String>,
    no_password: bool,
    ssh_key_files: Vec<String>,
    hostname: Option<String>,
    instance_id: Option<String>,
    user_data: Option<&str>,
) -> std::io::Result<()> {
    if no_password && ssh_key_files.is_empty() {
        return Err(
            crate::Error::bad_request("--no-password requires at least one --ssh-key").into(),
        );
    }

    // Read each --ssh-key file into a flat list of pubkey lines.
    // Empty lines and #-comments are filtered (a typical
    // authorized_keys file has neither, but accepting them lets
    // operators point at /etc/ssh/sshd-trusted-keys-style files).
    let mut ssh_keys: Vec<String> = Vec::new();
    for f in &ssh_key_files {
        let content = std::fs::read_to_string(f)
            .map_err(crate::Error::io_ctx(format!("read --ssh-key file {}", f)))?;
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                ssh_keys.push(trimmed.to_string());
            }
        }
    }

    let extra = match user_data {
        Some(p) => Some(
            std::fs::read_to_string(p)
                .map_err(crate::Error::io_ctx(format!("read --user-data file {}", p)))?,
        ),
        None => None,
    };

    let spec = cloud_init::SeedSpec {
        user,
        password: if no_password { None } else { password },
        ssh_keys,
        hostname,
        instance_id,
        nameservers: Vec::new(),
        extra_user_data: extra,
    };

    spec.write_iso(std::path::Path::new(output))?;
    eprintln!("seed ISO written to {}", output);
    Ok(())
}

// ============================================================================
// Profile CRUD CLI (#92)
// ============================================================================

/// Append a templated stanza for `<name>` to the catalog, then drop
/// into the operator's editor with visudo-style retry.
fn cmd_profile_add(name: &str) -> std::io::Result<()> {
    profile::validate_profile_name(name)?;
    let path = profile::profiles_path()?;
    let mut profiles = profile::load_profiles_from(&path)?;
    if profiles.profiles.contains_key(name) {
        return Err(crate::Error::bad_request(format!(
            "profile {:?} already exists; edit with `bhx profile edit` or remove with `bhx profile rm`",
            name
        ))
        .into());
    }
    profiles.profiles.insert(
        name.to_string(),
        profile::Profile {
            // Templated: empty image so the operator must fill it in.
            // Validation will reject the save until they do.
            image: String::new(),
            ..Default::default()
        },
    );
    let yaml = serde_yaml_ng::to_string(&profiles)
        .map_err(|e| crate::Error::internal(format!("serialize profiles: {}", e)))?;
    let edited = edit_via_temp_file(yaml.as_bytes())?;
    profile::save_profiles_to(&edited, &path)?;
    eprintln!("profile {:?} added", name);
    Ok(())
}

/// Drop into the editor on the catalog file with visudo-style retry.
/// The catalog at `~/.config/bhx/profiles.yaml` isn't touched until
/// the operator saves a clean edit — a Ctrl-C at the editor or the
/// retry prompt leaves the original catalog intact (#112).
fn cmd_profile_edit() -> std::io::Result<()> {
    let path = profile::profiles_path()?;
    // First-run UX: seed with commented example stanzas so the
    // operator has something to crib from. Comments don't survive a
    // save_profiles_to round-trip, so once the operator defines a
    // real profile the templates naturally fall away (#111).
    let initial: Vec<u8> = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            profile::FIRST_RUN_TEMPLATE.as_bytes().to_vec()
        }
        Err(e) => return Err(e),
    };
    let edited = edit_via_temp_file(&initial)?;
    profile::save_profiles_to(&edited, &path)?;
    Ok(())
}

/// Spawn `$EDITOR` against a private scratch copy of the catalog so
/// a Ctrl-C at the visudo-style retry prompt leaves the canonical
/// `~/.config/bhx/profiles.yaml` untouched. On a clean save, returns
/// the parsed catalog for the caller to persist.
///
/// The temp file is cleaned up by `NamedTempFile::Drop` on the
/// success path. SIGINT from the operator's Ctrl-C terminates the
/// process before Drop runs, leaking the temp file — that's an
/// acceptable cost; the canonical catalog is what we care about.
fn edit_via_temp_file(initial: &[u8]) -> std::io::Result<profile::ProfilesFile> {
    use std::io::Write;
    let mut tmp = tempfile::Builder::new()
        .prefix("bhx-profile-edit-")
        .suffix(".yaml")
        .tempfile()?;
    tmp.write_all(initial)?;
    tmp.flush()?;
    let tmp_path = tmp.path().to_path_buf();

    let mut runner = profile::ProcessEditor;
    let edited = profile::edit_with_retry(&mut runner, &tmp_path, 5, profile::stdin_retry_prompt)?;
    Ok(edited)
}

/// Print every known profile, one row per profile.
fn cmd_profile_list() -> std::io::Result<()> {
    let profiles = profile::load_profiles()?;
    if profiles.profiles.is_empty() {
        println!("(no profiles defined; use `bhx profile add <name>` to create one)");
        return Ok(());
    }
    println!(
        "{:<24} {:<24} {:<8} {:<7} {:<7}",
        "NAME", "IMAGE", "MEMORY", "NETWORK", "VCONSOLE"
    );
    for (name, p) in &profiles.profiles {
        let mem = p.memory.as_deref().unwrap_or("(default)");
        let net = if p.network.enabled { "yes" } else { "no" };
        let vc = if p.console.virtio { "yes" } else { "no" };
        println!(
            "{:<24} {:<24} {:<8} {:<7} {:<7}",
            name, p.image, mem, net, vc
        );
    }
    Ok(())
}

/// Pretty-print one profile's YAML stanza.
fn cmd_profile_show(name: &str) -> std::io::Result<()> {
    let profiles = profile::load_profiles()?;
    let p = profiles
        .profiles
        .get(name)
        .ok_or_else(|| crate::Error::bad_request(format!("no such profile {:?}", name)))?;
    let yaml = serde_yaml_ng::to_string(p)
        .map_err(|e| crate::Error::internal(format!("serialize profile: {}", e)))?;
    println!("# {}", name);
    print!("{}", yaml);
    Ok(())
}

/// Remove a profile from the catalog.
fn cmd_profile_rm(name: &str) -> std::io::Result<()> {
    let path = profile::profiles_path()?;
    let mut profiles = profile::load_profiles_from(&path)?;
    if profiles.profiles.remove(name).is_none() {
        return Err(crate::Error::bad_request(format!("no such profile {:?}", name)).into());
    }
    profile::save_profiles_to(&profiles, &path)?;
    eprintln!("profile {:?} removed", name);
    // Surface any instance disks that survived. The operator can
    // `bhx profile reset <name>` to clear them — they're now
    // disconnected from any catalog entry.
    if let Ok(dir) = profile::instances_dir() {
        let mut leftover: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Ok(n) = entry.file_name().into_string() {
                    if let Some((p, suffix)) = n.rsplit_once("-l") {
                        if p == name && suffix.parse::<u8>().is_ok() {
                            leftover.push(n);
                        }
                    }
                }
            }
        }
        if !leftover.is_empty() {
            eprintln!(
                "note: instance disks left in {}: {}",
                dir.display(),
                leftover.join(", ")
            );
            eprintln!("  remove them with: bhx profile reset {}", name);
        }
    }
    Ok(())
}

/// Delete the instance disk(s) for a profile.
fn cmd_profile_reset(name: &str, l2cpu_filter: Option<u8>) -> std::io::Result<()> {
    profile::validate_profile_name(name)?;
    let removed = profile::reset_instances(name, l2cpu_filter)?;
    if removed.is_empty() {
        match l2cpu_filter {
            Some(idx) => eprintln!("no instance disk for {:?} on l2cpu {}", name, idx),
            None => eprintln!("no instance disks for {:?}", name),
        }
    } else {
        for p in &removed {
            eprintln!("removed {}", p.display());
        }
    }
    Ok(())
}

/// Profile-driven boot. Compiles the named profile into the same
/// arguments `run_boot_client` takes, then delegates. Honors the
/// global `-d/--disk` (override the cloned instance disk) and
/// `-n/--network` (force-on even if the profile has it disabled,
/// since `--network` is presence-only) flags.
#[allow(clippy::too_many_arguments)]
fn run_boot_via_profile(
    card: u32,
    l2cpu: u8,
    name: &str,
    opensbi: String,
    dtb: String,
    initramfs: Option<String>,
    root_device: String,
    force_reset_pcie: bool,
    force: bool,
    cli_disk: Option<&str>,
    cli_network: bool,
) -> std::io::Result<()> {
    let profiles = profile::load_profiles()?;
    let p = profiles.profiles.get(name).ok_or_else(|| {
        crate::Error::bad_request(format!(
            "no such profile {:?}; run `bhx profile list` for available",
            name
        ))
    })?;
    profile::validate_profile(name, p)?;
    let img = image::get_known_image(&p.image).ok_or_else(|| {
        crate::Error::internal(format!(
            "profile {:?}: image {:?} validates but didn't resolve",
            name, p.image
        ))
    })?;

    // Locate the template the operator pulled. `bhx image pull` lands
    // artifacts in the canonical XDG image dir — re-derive the
    // filename here through the same helper so we stay in lock-step
    // with the pull side.
    let ext = if img.needs_bootloader { "img" } else { "ext4" };
    let template = image::image_dir().join(format!("{}.{}", img.name, ext));

    // Clone-or-reuse the instance disk. cli_disk overrides the
    // clone (operator wants to point at a different writable file).
    let (disk_path, was_cloned) = match cli_disk {
        Some(d) => (std::path::PathBuf::from(d), false),
        None => profile::clone_template_if_missing(&template, name, l2cpu)?,
    };
    if was_cloned {
        eprintln!(
            "profile {}: cloned {} -> {}",
            name,
            template.display(),
            disk_path.display()
        );
    }

    // Pick the boot payload. Profile bootloader override wins; else
    // image's needs_bootloader; else direct-kernel.
    let payload = match p.bootloader.as_deref() {
        Some("uboot") => daemon::protocol::BootPayload::Uboot(default_uboot_path()),
        Some("kernel") => daemon::protocol::BootPayload::Kernel("Image".to_string()),
        Some(other) => {
            return Err(crate::Error::bad_request(format!(
                "profile {:?}: invalid bootloader {:?}",
                name, other
            ))
            .into());
        }
        None => {
            if img.needs_bootloader {
                daemon::protocol::BootPayload::Uboot(default_uboot_path())
            } else {
                daemon::protocol::BootPayload::Kernel("Image".to_string())
            }
        }
    };

    let memory_override = match &p.memory {
        Some(s) => Some(profile::parse_memory_str(s)?),
        None => None,
    };
    let hostname_override = p.network.hostname.clone();
    let mut fwd: Vec<(u16, u16)> = Vec::new();
    for raw in &p.network.forwards {
        // The schema is operator-readable strings; profile
        // validation already accepted them, so unwrap_or here is
        // belt-and-braces.
        let (h_str, g_str) = raw.split_once(':').ok_or_else(|| {
            crate::Error::internal(format!("profile {}: post-validate parse: {:?}", name, raw))
        })?;
        let h: u16 = h_str.parse().map_err(|_| {
            crate::Error::internal(format!("profile {}: post-validate parse h", name))
        })?;
        let g: u16 = g_str.parse().map_err(|_| {
            crate::Error::internal(format!("profile {}: post-validate parse g", name))
        })?;
        fwd.push((h, g));
    }

    // Profile network setting + cli_network: union (positive). The
    // CLI flag is presence-only so a passed `-n` plus a profile
    // with network disabled still enables network. Warn when they
    // disagree.
    let network = p.network.enabled || cli_network;
    if cli_network && !p.network.enabled {
        eprintln!(
            "note: profile {} has network disabled, but -n was passed; enabling for this boot",
            name
        );
    }

    // #127 profile-driven cloud-init seed: materialize the SeedSpec
    // into the per-instance dir as `cidata.img` and pass that path
    // through. Re-rendered every boot — the seed is small (~10 KiB)
    // and a fresh write avoids an explicit cache-invalidation rule for
    // edits to `profile.cloud_init`. Per-(profile, l2cpu) location so
    // concurrent boots of the same profile on different L2CPUs don't
    // race on a single seed file.
    let cloud_init_path: Option<String> = match &p.cloud_init {
        Some(ci) => {
            let dir = profile::instance_dir(name, l2cpu)?;
            std::fs::create_dir_all(&dir).map_err(crate::Error::io_ctx(format!(
                "create instance dir {}",
                dir.display()
            )))?;
            let seed_path = dir.join("cidata.img");
            ci.to_seed_spec()
                .write_iso(&seed_path)
                .map_err(|e| crate::Error::internal(format!("render profile seed: {}", e)))?;
            eprintln!(
                "profile {}: cloud-init seed -> {}",
                name,
                seed_path.display()
            );
            Some(seed_path.display().to_string())
        }
        None => None,
    };

    run_boot_client(
        card,
        l2cpu,
        opensbi,
        payload,
        dtb,
        initramfs.or_else(|| p.initramfs.clone()),
        root_device,
        force_reset_pcie,
        Some(disk_path.display().to_string()),
        network,
        fwd,
        p.console.virtio,
        p.console.rng,
        force,
        memory_override,
        hostname_override,
        cloud_init_path,
    )
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

    // --- parse_memory + parse_hostname (#91) -------------------------------

    #[test]
    fn parse_memory_accepts_si_and_iec_suffixes() {
        assert_eq!(parse_memory("2GB").unwrap(), 2_000_000_000);
        assert_eq!(parse_memory("2GiB").unwrap(), 2_147_483_648);
        assert_eq!(parse_memory("2048MB").unwrap(), 2_048_000_000);
        assert_eq!(parse_memory("512MiB").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory("1024KB").unwrap(), 1_024_000);
        assert_eq!(parse_memory("1024KiB").unwrap(), 1_024 * 1024);
        // Bare integer = bytes.
        assert_eq!(parse_memory("4096").unwrap(), 4096);
        assert_eq!(parse_memory("4096B").unwrap(), 4096);
    }

    #[test]
    fn parse_memory_accepts_decimal_with_iec_suffix() {
        // 1.5 GiB = 1.5 * 2^30 = 1_610_612_736
        assert_eq!(parse_memory("1.5GiB").unwrap(), 1_610_612_736);
    }

    #[test]
    fn parse_memory_rejects_malformed() {
        assert!(parse_memory("").is_err());
        assert!(parse_memory("abc").is_err());
        assert!(parse_memory("0").is_err());
        assert!(parse_memory("-1GB").is_err());
        assert!(parse_memory("2 GB ").is_ok()); // trim
        assert!(parse_memory("GB").is_err());
    }

    #[test]
    fn parse_hostname_accepts_rfc952_clean() {
        assert_eq!(parse_hostname("debian-bench").unwrap(), "debian-bench");
        assert_eq!(parse_hostname("alma01").unwrap(), "alma01");
        assert_eq!(parse_hostname("a").unwrap(), "a");
    }

    #[test]
    fn parse_hostname_rejects_invalid() {
        // empty
        assert!(parse_hostname("").is_err());
        // too long (>63)
        assert!(parse_hostname(&"a".repeat(64)).is_err());
        // leading / trailing dash
        assert!(parse_hostname("-foo").is_err());
        assert!(parse_hostname("foo-").is_err());
        // uppercase rejected (RFC 952 strict)
        assert!(parse_hostname("Foo").is_err());
        // underscores rejected
        assert!(parse_hostname("foo_bar").is_err());
        // dots rejected (this is the per-label part, not FQDN)
        assert!(parse_hostname("foo.bar").is_err());
    }

    // --- parse_l2cpu_locator + resolve_target (#98) ------------------------

    #[test]
    fn locator_plain_n_has_no_card_override() {
        assert_eq!(parse_l2cpu_locator("0").unwrap(), (None, 0));
        assert_eq!(parse_l2cpu_locator("3").unwrap(), (None, 3));
    }

    #[test]
    fn locator_c_colon_n_returns_card_override() {
        assert_eq!(parse_l2cpu_locator("0:0").unwrap(), (Some(0), 0));
        assert_eq!(parse_l2cpu_locator("1:2").unwrap(), (Some(1), 2));
        assert_eq!(parse_l2cpu_locator("17:3").unwrap(), (Some(17), 3));
    }

    #[test]
    fn locator_rejects_malformed() {
        assert!(parse_l2cpu_locator(":0").is_err()); // empty card
        assert!(parse_l2cpu_locator("5:").is_err()); // empty l2cpu
        assert!(parse_l2cpu_locator("abc").is_err()); // non-numeric
        assert!(parse_l2cpu_locator("0:abc").is_err()); // non-numeric l2cpu
        assert!(parse_l2cpu_locator("a:0").is_err()); // non-numeric card
        assert!(parse_l2cpu_locator("4").is_err()); // out-of-range l2cpu
        assert!(parse_l2cpu_locator("0:9").is_err()); // out-of-range l2cpu
    }

    #[test]
    fn resolve_target_uses_ttdevice_when_no_locator_card() {
        assert_eq!(resolve_target("2", Some(1)).unwrap(), (1, 2));
        assert_eq!(resolve_target("0", None).unwrap(), (0, 0));
    }

    #[test]
    fn resolve_target_locator_card_wins_when_ttdevice_absent_or_matches() {
        assert_eq!(resolve_target("3:1", None).unwrap(), (3, 1));
        assert_eq!(resolve_target("3:1", Some(3)).unwrap(), (3, 1));
    }

    #[test]
    fn resolve_target_errors_on_card_conflict() {
        // Locator card 1 and -t 2 disagree -> error
        let err = resolve_target("1:0", Some(2)).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("conflicting card"),
            "expected conflict diagnostic, got {:?}",
            msg
        );
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

    // --- default_boot_payload (#42 / needs_bootloader) -----------------------

    #[test]
    fn default_payload_with_no_disk_falls_back_to_kernel_image() {
        match default_boot_payload(None) {
            daemon::protocol::BootPayload::Kernel(p) => assert_eq!(p, "Image"),
            other => panic!("expected Kernel(Image), got {:?}", other),
        }
    }

    #[test]
    fn default_payload_for_unknown_disk_falls_back_to_kernel_image() {
        // A disk path whose basename doesn't match any known image —
        // we must preserve the pre-#44 direct-kernel default.
        match default_boot_payload(Some("/some/random/rootfs.ext4")) {
            daemon::protocol::BootPayload::Kernel(p) => assert_eq!(p, "Image"),
            other => panic!("expected Kernel(Image), got {:?}", other),
        }
    }

    #[test]
    fn default_payload_for_known_extract_image_picks_kernel() {
        // tt-debian is a single-FS ext4 with needs_bootloader=false:
        // the boot subcommand should default to direct-kernel mode.
        match default_boot_payload(Some("images/tt-debian.ext4")) {
            daemon::protocol::BootPayload::Kernel(p) => assert_eq!(p, "Image"),
            other => panic!("expected Kernel(Image), got {:?}", other),
        }
    }

    #[test]
    fn default_payload_for_known_uboot_image_picks_uboot() {
        // almalinux-10-kitten lands as a whole-disk `.img` with
        // needs_bootloader=true: default must flip to U-Boot mode.
        // The path can be cwd-relative (`u-boot.bin` / `third_party/...`)
        // or XDG-resolved (`~/.local/share/bhx/firmware/u-boot.bin`)
        // depending on which of the search paths first matches in the
        // dev's environment, so assert on the filename rather than the
        // full path.
        match default_boot_payload(Some("images/almalinux-10-kitten.img")) {
            daemon::protocol::BootPayload::Uboot(p) => {
                let filename = std::path::Path::new(&p)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                assert_eq!(filename, "u-boot.bin", "got {:?}", p);
            }
            other => panic!("expected Uboot(...), got {:?}", other),
        }
    }

    // --- CLI parsing: defaults -----------------------------------------------

    #[test]
    fn cli_defaults_leave_disk_network_off() {
        let cli = parse(&["bhx", "connect"]);
        assert_eq!(cli.disk, None);
        assert!(!cli.network);
    }

    // --- --disk / -d ---------------------------------------------------------

    #[test]
    fn cli_disk_long_form_captures_path() {
        let cli = parse(&["bhx", "connect", "--disk", "/path/to/img.ext4"]);
        assert_eq!(cli.disk.as_deref(), Some("/path/to/img.ext4"));
    }

    #[test]
    fn cli_disk_short_form_captures_path() {
        let cli = parse(&["bhx", "connect", "-d", "img.ext4"]);
        assert_eq!(cli.disk.as_deref(), Some("img.ext4"));
    }

    // --- --network -----------------------------------------------------------

    #[test]
    fn cli_network_flag_opts_in() {
        let cli = parse(&["bhx", "connect", "--network"]);
        assert!(cli.network);
    }

    #[test]
    fn cli_network_short_form_opts_in() {
        let cli = parse(&["bhx", "connect", "-n"]);
        assert!(cli.network);
    }

    // --- global flags work on other subcommands & bare invocation ------------

    #[test]
    fn cli_bare_invocation_parses_like_connect() {
        // When no subcommand is given, `main` falls through to
        // run_connect_client (same as the explicit `connect` subcommand);
        // the global flags must still apply.
        let cli = parse(&["bhx", "-n", "-d", "x.ext4"]);
        assert!(cli.command.is_none());
        assert!(cli.network);
        assert_eq!(cli.disk.as_deref(), Some("x.ext4"));
    }
}
