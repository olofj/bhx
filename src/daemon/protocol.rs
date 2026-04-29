// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Control-socket protocol.
//!
//! Wire format: a 4-byte little-endian length prefix followed by JSON.
//! `Request` goes client → daemon, `Response` goes daemon → client. When
//! the server needs to hand the client a console fd it sends it alongside
//! the response using SCM_RIGHTS (`send_fd` / `recv_fd`).

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use serde::{Deserialize, Serialize};

/// Opaque to the server: what a client wants to do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Get daemon + per-L2CPU state.
    Status,
    /// Boot an L2CPU. If `disk` and/or `network` are provided, the matching
    /// virtio workers start up in the same RPC so the guest kernel sees
    /// them before VFS mount (otherwise the guest panics with "Can't open
    /// blockdev" — add-disk's ~100 ms latency is already too late).
    Boot {
        l2cpu: u8,
        opensbi: String,
        /// What sits at `KERNEL_OFFSET` (start+0x20_0000) in L2CPU DRAM.
        /// `Kernel` is the historical raw `Image` payload — OpenSBI's
        /// fw_jump lands directly on it. `Uboot` is the S-mode
        /// bootloader that loads the actual kernel + initrd from disk
        /// at runtime. In `Uboot` mode the daemon does not preload an
        /// initramfs and `modify_dtb` leaves bootargs to U-Boot. See
        /// #44 for the umbrella.
        payload: BootPayload,
        dtb: String,
        /// Initramfs to preload into DRAM. Ignored in `Uboot` mode
        /// (U-Boot reads the initrd from disk).
        #[serde(default)]
        initramfs: Option<String>,
        #[serde(default = "default_root_device")]
        root_device: String,
        #[serde(default)]
        force_reset_pcie: bool,
        #[serde(default)]
        disk: Option<String>,
        #[serde(default)]
        network: bool,
        /// Extra TCP forwards installed in the slirp config at boot
        /// time, on top of the implicit SSH forward. Same shape as
        /// `AddNet::extra_fwd` — `(host_port, guest_port)` pairs.
        /// Needed because hot-add via `add-net` recreates the slirp
        /// instance, which the buildroot guest kernel can't rebind to
        /// (virtio_net is built-in, no module reload). The bench's
        /// ingress measurement requires the forward to be present
        /// from cold-boot, so the guest's binding never gets torn
        /// down. Older clients that omit the field deserialize to an
        /// empty Vec via `#[serde(default)]`.
        #[serde(default)]
        extra_fwd: Vec<(u16, u16)>,
        /// Attach a virtio-console device alongside the boot. Stock
        /// distro kernels with `CONFIG_VIRTIO_CONSOLE` register this
        /// as `/dev/hvc0` and direct their console to it. Older
        /// clients without this field hit the `serde(default)` =
        /// false path. See #51.
        #[serde(default)]
        console: bool,
        /// Attach a virtio-rng device alongside the boot. U-Boot's
        /// EFI loader needs this to install `EFI_RNG_PROTOCOL`,
        /// which the AlmaLinux EFI shim requires to chainload GRUB.
        /// Older clients without this field hit `serde(default)` =
        /// false. See #62.
        #[serde(default)]
        rng: bool,
        /// If true and a slot already exists for this L2CPU, the daemon
        /// tears it down (stop workers, drop L2Cpu) before re-imaging.
        /// Default false → duplicate boots are rejected with an error.
        #[serde(default)]
        force: bool,
    },
    /// Attach a console fd. Daemon replies with `ok` and sends the fd via
    /// SCM_RIGHTS; client pumps bytes between its tty and the passed fd.
    AttachConsole {
        l2cpu: u8,
        #[serde(default)]
        mode: ConsoleMode,
    },
    /// Add a virtio-block device to a running L2CPU.
    AddDisk { l2cpu: u8, path: String },
    /// Remove the virtio-block device from a running L2CPU (Phase A: only
    /// one disk per L2CPU, so no selector). Joins the worker thread and
    /// drops the disk handle. The image file is unlocked and available
    /// to the host again.
    RemoveDisk { l2cpu: u8 },
    /// Add a virtio-net device to a running L2CPU.
    ///
    /// `ssh_port` is the host TCP port for the implicit SSH forward
    /// (host:ssh_port → guest:22). Today's CLI default uses
    /// `regs::slirp::ssh_port(card, l2cpu_idx)` so multiple cores
    /// don't collide.
    ///
    /// `extra_fwd` is a list of additional host→guest TCP forwards as
    /// `(host_port, guest_port)` pairs. Each gets its own slirp
    /// `tcp_listen_add`. `#[serde(default)]` so older clients that
    /// don't know the field stay wire-compatible (deserialize as an
    /// empty Vec).
    AddNet {
        l2cpu: u8,
        ssh_port: Option<u16>,
        #[serde(default)]
        extra_fwd: Vec<(u16, u16)>,
    },
    /// Remove the virtio-net device from a running L2CPU. Joins the
    /// worker thread; libvdeslirp state (TCP/NAT) is dropped.
    RemoveNet { l2cpu: u8 },
    /// Attach a virtio-console device to a running L2CPU. Stock
    /// distro kernels with `CONFIG_VIRTIO_CONSOLE` will pick it up as
    /// `/dev/hvc0`. See #51.
    AddConsole { l2cpu: u8 },
    /// Remove the virtio-console device from a running L2CPU. Joins
    /// the worker thread; any in-flight RX descriptors are dropped.
    RemoveConsole { l2cpu: u8 },
    /// Stop a single L2CPU's device threads.
    Stop { l2cpu: u8 },
    /// Ask the daemon to exit.
    Shutdown,
}

fn default_root_device() -> String {
    "vda".to_string()
}

/// What sits at L2CPU DRAM `KERNEL_OFFSET` (start+0x20_0000) — either a
/// Linux `Image` (OpenSBI fw_jump lands directly on it, classic boot) or
/// the U-Boot S-mode payload (which then loads the actual kernel +
/// initrd from disk via virtio-blk). See #44.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
pub enum BootPayload {
    /// Raw Linux `Image`. OpenSBI's `fw_jump` jumps to this address;
    /// the kernel takes the L2CPU directly.
    Kernel(String),
    /// U-Boot binary built for S-mode (e.g. `uboot/u-boot.bin`).
    /// OpenSBI hands control to U-Boot; U-Boot then reads kernel +
    /// initrd from a virtio-blk disk and `booti`s into them.
    Uboot(String),
}

impl BootPayload {
    /// Path to the binary the daemon will load at `KERNEL_OFFSET`.
    pub fn path(&self) -> &str {
        match self {
            BootPayload::Kernel(p) | BootPayload::Uboot(p) => p,
        }
    }

    /// True iff this is a U-Boot payload (`modify_dtb` and the
    /// initramfs-preload path use this to gate U-Boot-specific
    /// behavior).
    pub fn is_uboot(&self) -> bool {
        matches!(self, BootPayload::Uboot(_))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConsoleMode {
    /// Read-only attach — writer is whoever got there first.
    #[default]
    Ro,
    /// Become the writer (only allowed on the first attach).
    Rw,
    /// Demote the current writer and take over.
    Takeover,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    /// Generic success, no payload.
    Ok,
    /// Success with a typed payload.
    Status(StatusPayload),
    /// Console attach succeeded; a fd follows via SCM_RIGHTS.
    Attached { scrollback_bytes: u32 },
    /// Request was understood but failed. `error` is a human string.
    Error { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusPayload {
    pub pid: u32,
    pub uptime_secs: u64,
    pub l2cpus: Vec<L2CpuStatus>,
    /// NOC0-logical (x, y) of the Tensix tile this daemon reserved
    /// for its virtio engine, or `None` if bring-up hasn't run yet
    /// (no L2CPU booted since daemon start). Operators running
    /// tt-metal alongside the daemon must exclude this tile from
    /// their `DispatchCoreConfig`. See #74. `#[serde(default)]`
    /// keeps pre-#74 clients wire-compatible.
    #[serde(default)]
    pub engine_tile: Option<(u16, u16)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2CpuStatus {
    pub idx: u8,
    pub state: L2CpuState,
    pub disk: Option<String>,
    pub net: bool,
    /// Whether a virtio-console device is attached to this slot
    /// (#54). `#[serde(default)]` keeps this wire-compatible with
    /// pre-#54 clients that don't send the field; they get `false`.
    #[serde(default)]
    pub virtio_console: bool,
    pub clients: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum L2CpuState {
    /// Held in reset; cold-bootable.
    Stopped,
    /// Released from reset and OpenSBI magic is valid.
    Running,
    /// Released but OpenSBI magic is missing — needs explicit recovery.
    Wedged,
}

/// Maximum frame size the decoder will accept before giving up. JSON control
/// frames are tiny; 64 KiB is way more than we'll ever need and protects us
/// against a broken / malicious client making us allocate gigabytes.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024;

/// Write a length-prefixed JSON frame.
pub fn write_frame<W: Write, T: Serialize>(mut w: W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(|e| io::Error::from(crate::Error::Protocol(e)))?;
    if body.len() > MAX_FRAME_BYTES as usize {
        return Err(crate::Error::internal(format!(
            "frame too large: {} > {}",
            body.len(),
            MAX_FRAME_BYTES
        ))
        .into());
    }
    let len = (body.len() as u32).to_le_bytes();
    w.write_all(&len)?;
    w.write_all(&body)?;
    Ok(())
}

/// Read a length-prefixed JSON frame.
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(mut r: R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(crate::Error::internal(format!(
            "frame too large: {} > {}",
            len, MAX_FRAME_BYTES
        ))
        .into());
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::from(crate::Error::Protocol(e)))
}

/// Send a file descriptor alongside a single data byte. Unix requires that
/// an SCM_RIGHTS message carry at least one byte of regular data.
pub fn send_fd(sock: &UnixStream, fd: RawFd) -> io::Result<()> {
    use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
    use std::io::IoSlice;

    let payload = [0u8; 1];
    let iov = [IoSlice::new(&payload)];
    let fds = [fd];
    let cmsgs = [ControlMessage::ScmRights(&fds)];
    sendmsg::<()>(sock.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    Ok(())
}

/// Receive a file descriptor. Consumes one byte of regular data (matching
/// `send_fd`). Returns the OwnedFd so the caller is responsible for closing it.
pub fn recv_fd(sock: &UnixStream) -> io::Result<OwnedFd> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
    use std::io::IoSliceMut;
    use std::os::fd::FromRawFd;

    let mut payload = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut payload)];
    let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);
    let msg = recvmsg::<()>(
        sock.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg_buf),
        MsgFlags::empty(),
    )
    .map_err(|e| io::Error::from_raw_os_error(e as i32))?;

    for cmsg in msg
        .cmsgs()
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?
    {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            if let Some(&fd) = fds.first() {
                return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }
    Err(crate::Error::internal("no SCM_RIGHTS fd in message").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::net::UnixStream;

    #[test]
    fn request_roundtrip_boot() {
        let req = Request::Boot {
            l2cpu: 2,
            opensbi: "fw_jump.bin".into(),
            payload: BootPayload::Kernel("Image".into()),
            dtb: "blackhole-card.dtb".into(),
            initramfs: None,
            root_device: "vda".into(),
            force_reset_pcie: false,
            disk: None,
            network: false,
            extra_fwd: vec![],
            console: false,
            rng: false,
            force: false,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let decoded: Request = read_frame(Cursor::new(&buf)).unwrap();
        assert!(matches!(
            decoded,
            Request::Boot {
                l2cpu: 2,
                payload: BootPayload::Kernel(_),
                ..
            }
        ));
    }

    #[test]
    fn request_roundtrip_boot_uboot() {
        let req = Request::Boot {
            l2cpu: 0,
            opensbi: "fw_jump.bin".into(),
            payload: BootPayload::Uboot("u-boot.bin".into()),
            dtb: "blackhole-card.dtb".into(),
            initramfs: None,
            root_device: "vda".into(),
            force_reset_pcie: false,
            disk: Some("debian.raw".into()),
            network: false,
            extra_fwd: vec![],
            console: false,
            rng: false,
            force: false,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let decoded: Request = read_frame(Cursor::new(&buf)).unwrap();
        match decoded {
            Request::Boot {
                payload: BootPayload::Uboot(p),
                ..
            } => assert_eq!(p, "u-boot.bin"),
            other => panic!("expected uboot variant, got {:?}", other),
        }
    }

    #[test]
    fn response_roundtrip_status() {
        let resp = Response::Status(StatusPayload {
            pid: 1234,
            uptime_secs: 42,
            l2cpus: vec![L2CpuStatus {
                idx: 0,
                state: L2CpuState::Running,
                disk: Some("rootfs-0.ext4".into()),
                net: true,
                virtio_console: true,
                clients: 1,
            }],
            engine_tile: Some((16, 11)),
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &resp).unwrap();
        let decoded: Response = read_frame(Cursor::new(&buf)).unwrap();
        if let Response::Status(s) = decoded {
            assert_eq!(s.pid, 1234);
            assert_eq!(s.l2cpus.len(), 1);
            assert_eq!(s.l2cpus[0].state, L2CpuState::Running);
            assert!(s.l2cpus[0].virtio_console);
            assert_eq!(s.engine_tile, Some((16, 11)));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn status_payload_decodes_pre_54_clients_with_default_virtio_console() {
        // Wire compatibility: a pre-#54 client sends a StatusPayload
        // without `virtio_console`. `#[serde(default)]` must default
        // it to false rather than failing to parse.
        let json = r#"{"pid":1,"uptime_secs":0,"l2cpus":[
            {"idx":0,"state":"stopped","disk":null,"net":false,"clients":0}
        ]}"#;
        let s: StatusPayload = serde_json::from_str(json).expect("legacy frame must decode");
        assert!(!s.l2cpus[0].virtio_console);
    }

    #[test]
    fn console_mode_default_is_ro() {
        let req: Request = serde_json::from_str(r#"{"op":"attach_console","l2cpu":0}"#).unwrap();
        if let Request::AttachConsole { mode, .. } = req {
            assert_eq!(mode, ConsoleMode::Ro);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn oversize_frame_rejected() {
        let huge = vec![0u8; (MAX_FRAME_BYTES + 1) as usize];
        let req = Request::AddDisk {
            l2cpu: 0,
            path: String::from_utf8(huge).unwrap(),
        };
        let mut buf = Vec::new();
        assert!(write_frame(&mut buf, &req).is_err());
    }

    #[test]
    fn truncated_frame_errors() {
        let mut buf = vec![0xff, 0xff, 0xff, 0xff]; // length prefix exceeds MAX_FRAME_BYTES
        buf.extend_from_slice(b"{}");
        let res: io::Result<Request> = read_frame(Cursor::new(&buf));
        assert!(res.is_err());
    }

    #[test]
    fn scm_rights_roundtrips_an_fd() {
        // Send one end of a pipe across a socketpair and verify the receiving
        // side sees the same pipe semantics.
        use nix::unistd;
        use std::io::{Read as _, Write as _};
        use std::os::fd::{FromRawFd, IntoRawFd};

        let (a, b) = UnixStream::pair().unwrap();
        let (pipe_r, pipe_w) = unistd::pipe().unwrap();
        send_fd(&a, pipe_w.as_raw_fd()).unwrap();
        // Drop the original sender side so the only remaining write end is
        // the one that travelled through SCM_RIGHTS.
        drop(pipe_w);
        let received = recv_fd(&b).unwrap();

        // Write to the received fd, read from the original read end.
        let mut w = unsafe { std::fs::File::from_raw_fd(received.into_raw_fd()) };
        w.write_all(b"hello").unwrap();
        drop(w);
        let mut r = unsafe { std::fs::File::from_raw_fd(pipe_r.into_raw_fd()) };
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn request_roundtrip_remove_disk() {
        let req = Request::RemoveDisk { l2cpu: 3 };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let decoded: Request = read_frame(Cursor::new(&buf)).unwrap();
        assert!(matches!(decoded, Request::RemoveDisk { l2cpu: 3 }));
    }

    #[test]
    fn request_roundtrip_remove_net() {
        let req = Request::RemoveNet { l2cpu: 0 };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let decoded: Request = read_frame(Cursor::new(&buf)).unwrap();
        assert!(matches!(decoded, Request::RemoveNet { l2cpu: 0 }));
    }

    #[test]
    fn boot_force_roundtrips_both_values() {
        for force in [false, true] {
            let req = Request::Boot {
                l2cpu: 1,
                opensbi: "a".into(),
                payload: BootPayload::Kernel("b".into()),
                dtb: "c".into(),
                initramfs: None,
                root_device: "vda".into(),
                force_reset_pcie: false,
                disk: None,
                network: false,
                extra_fwd: vec![],
                console: false,
                rng: false,
                force,
            };
            let mut buf = Vec::new();
            write_frame(&mut buf, &req).unwrap();
            let decoded: Request = read_frame(Cursor::new(&buf)).unwrap();
            match decoded {
                Request::Boot { force: got, .. } => assert_eq!(got, force),
                _ => panic!("wrong variant"),
            }
        }
    }

    #[test]
    fn add_net_roundtrips_extra_fwd() {
        // Forwards survive serde encoding + decoding intact.
        let req = Request::AddNet {
            l2cpu: 2,
            ssh_port: Some(2222),
            extra_fwd: vec![(5201, 5201), (8080, 80)],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        let decoded: Request = read_frame(Cursor::new(&buf)).unwrap();
        match decoded {
            Request::AddNet {
                l2cpu,
                ssh_port,
                extra_fwd,
            } => {
                assert_eq!(l2cpu, 2);
                assert_eq!(ssh_port, Some(2222));
                assert_eq!(extra_fwd, vec![(5201, 5201), (8080, 80)]);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn add_net_defaults_extra_fwd_empty_on_old_payload() {
        // Pre-#37 clients send `{"op":"add-net","l2cpu":N,"ssh_port":...}`
        // without `extra_fwd`. serde(default) makes the daemon accept
        // them with an empty Vec — wire-compat preserved.
        let json = r#"{"op":"add_net","l2cpu":1,"ssh_port":null}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::AddNet {
                l2cpu,
                ssh_port,
                extra_fwd,
            } => {
                assert_eq!(l2cpu, 1);
                assert!(ssh_port.is_none());
                assert!(extra_fwd.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn boot_force_defaults_false_on_minimal_payload() {
        // A request without the optional `force` field should still
        // deserialize; `force` must default to false so the
        // duplicate-boot rejection stays the safe default. Note: the
        // `payload` field IS mandatory after #44/#45 (BootPayload
        // replaced the historical `kernel: String`); a daemon receiving
        // an older client's `{"kernel": "..."}` request will reject it
        // at deserialize time. Within this monorepo daemon and client
        // ship together, so this is OK.
        let json = r#"{"op":"boot","l2cpu":0,"opensbi":"a","payload":{"kind":"kernel","path":"b"},"dtb":"c","root_device":"vda"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Boot { force, .. } => assert!(!force),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn l2cpu_state_wedged_serializes_as_wedged() {
        // dispatch_status emits this variant when a released core fails the
        // warm-resume probe. Make sure the wire value matches the client's
        // printf ("Wedged" via Debug/Display).
        let s = serde_json::to_string(&L2CpuState::Wedged).unwrap();
        assert_eq!(s, r#""wedged""#);
        let parsed: L2CpuState = serde_json::from_str(r#""wedged""#).unwrap();
        assert_eq!(parsed, L2CpuState::Wedged);
    }
}
