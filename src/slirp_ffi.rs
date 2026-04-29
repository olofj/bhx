// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Manual FFI bindings for libvdeslirp/libslirp.

use std::os::unix::io::RawFd;

/// Opaque SlirpConfig — sized to hold the libslirp SlirpConfig struct.
/// Must be zeroed before calling vdeslirp_init(). The actual struct is 192
/// bytes as of libslirp 4.8; we over-allocate to tolerate future growth.
#[repr(C, align(8))]
pub struct SlirpConfig {
    _data: [u8; 512],
}

pub const VDE_INIT_DEFAULT: libc::c_int = 1;

#[repr(C)]
pub struct VdeSlirp {
    _opaque: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InAddr {
    pub s_addr: u32,
}

extern "C" {
    pub fn vdeslirp_init(cfg: *mut SlirpConfig, flags: libc::c_int);
    pub fn vdeslirp_open(cfg: *mut SlirpConfig) -> *mut VdeSlirp;
    pub fn vdeslirp_add_fwd(
        slirp: *mut VdeSlirp,
        is_udp: libc::c_int,
        host_addr: InAddr,
        host_port: libc::c_int,
        guest_addr: InAddr,
        guest_port: libc::c_int,
    ) -> libc::c_int;
    pub fn vdeslirp_fd(slirp: *mut VdeSlirp) -> RawFd;
    pub fn vdeslirp_close(slirp: *mut VdeSlirp) -> libc::c_int;
    pub fn vdeslirp_recv(slirp: *mut VdeSlirp, buf: *mut u8, len: libc::size_t) -> libc::ssize_t;
    pub fn vdeslirp_send(slirp: *mut VdeSlirp, buf: *const u8, len: libc::size_t) -> libc::ssize_t;

    pub fn inet_aton(cp: *const libc::c_char, inp: *mut InAddr) -> libc::c_int;

    /// Set the DHCP hostname libslirp advertises to the guest (option
    /// 12). Implementation in `src/slirp_set_hostname.c`. The
    /// `vhostname` pointer must outlive `cfg`'s next `vdeslirp_open`
    /// call — the caller (currently `VirtioNet::new`) holds a
    /// `CString` field for that. See #60.
    pub fn tt_slirp_set_vhostname(cfg: *mut SlirpConfig, vhostname: *const libc::c_char);
}

impl InAddr {
    pub fn from_str(s: &str) -> Self {
        let c_str = std::ffi::CString::new(s).unwrap();
        let mut addr = InAddr { s_addr: 0 };
        let ret = unsafe { inet_aton(c_str.as_ptr(), &mut addr) };
        assert!(ret != 0, "inet_aton failed to parse address: {}", s);
        addr
    }
}
