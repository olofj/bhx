// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! VirtIO network device implementation using Slirp.

use std::ptr;

use crate::slirp_ffi::*;
use crate::virtio::VirtioDeviceImpl;

const PACKET_SIZE: usize = 1514;
const VIRTIO_ID_NET: u32 = 1;
// VirtIO spec 5.1.3: feature bits of virtio-net. We advertise
// VIRTIO_NET_F_MAC + VIRTIO_F_VERSION_1 (bit 32 — bit 0 of
// features[1]) and nothing else.
//
// VIRTIO_NET_F_MAC (bit 5 of features[0]): tells the guest the 6-byte
// `mac` field at config offset 0 is valid. Without it, U-Boot's
// virtio-net probe loops printing "No valid MAC address found." and
// Linux falls back to a random MAC that changes every reboot. See #77.
//
// We deliberately do NOT advertise VIRTIO_NET_F_CSUM (bit 0 of
// features[0]). That bit means "device handles packets with
// partial checksum" — if we claim it, the guest's networking stack
// flags outbound UDP/TCP with NETIF_F_HW_CSUM and virtio-net sets
// VIRTIO_NET_HDR_F_NEEDS_CSUM in the vnet header, expecting us to
// compute the L4 checksum before forwarding. We don't; we pass the
// frame straight to slirp, which then drops the packet for failing
// checksum validation. ICMP survived the misconfiguration only because
// the kernel computes ICMP checksums itself regardless of device
// offload. By advertising no csum offload, the guest stack does the
// checksum in software and slirp accepts the packets.
const VIRTIO_NET_F_MAC_BIT: u32 = 1 << 5; // bit 5 of features[0]
const VIRTIO_F_VERSION_1_BIT: u32 = 1 << 0; // bit 0 of features[1]
const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;

/// Format the per-(card, L2CPU) hostname libslirp advertises in DHCP
/// option 12 (#60). RFC-952-clean (`a-z0-9-`, no `_`, ≤63 chars) so
/// every guest distro accepts it as the system hostname.
pub fn format_dhcp_hostname(card: u32, l2cpu_idx: u8) -> String {
    format!("tt-bh-card{}-l2cpu{}", card, l2cpu_idx)
}

/// Derive a stable MAC for a given (card, l2cpu_idx). Locally
/// administered (bit 1 of byte 0 set) and unicast (bit 0 clear) so
/// it never collides with a real OUI. Card + L2CPU index encoded in
/// the lower bytes — distinct on a multi-card host, stable across
/// daemon restarts on the same chip.
///
/// `02:00:CC:CC:LL:00`
///   byte 0: 0x02 — locally administered, unicast
///   byte 1: 0x00 — reserved for future use
///   byte 2: card index low 8 bits
///   byte 3: card index high 8 bits
///   byte 4: L2CPU index (0..3)
///   byte 5: 0x00 — reserved for future use
pub fn derive_mac(card: u32, l2cpu_idx: u8) -> [u8; 6] {
    [
        0x02,
        0x00,
        (card & 0xff) as u8,
        ((card >> 8) & 0xff) as u8,
        l2cpu_idx,
        0x00,
    ]
}

/// virtio-net config space layout (VirtIO 1.2 §5.1.4). Only `mac` is
/// populated today (we don't negotiate STATUS / MQ / MTU / SPEED).
#[repr(C)]
struct VirtioNetConfig {
    mac: [u8; 6],
    status: u16,
    max_virtqueue_pairs: u16,
    mtu: u16,
    speed: u32,
    duplex: u8,
}

/// Write a MAC into the device config region. Extracted so it's
/// testable without standing up a full `VirtioNet` (which requires
/// libslirp). `config` must point to a buffer at least
/// `size_of::<VirtioNetConfig>()` bytes long.
fn write_mac_into_config(mac: &[u8; 6], config: *mut u8) {
    let cfg = config as *mut VirtioNetConfig;
    for (i, b) in mac.iter().enumerate() {
        unsafe {
            ptr::write_volatile(&mut (*cfg).mac[i], *b);
        }
    }
}

/// Recompute the IPv4 TCP/UDP checksum in place. The guest sets this on
/// outbound packets when it thinks we negotiated `VIRTIO_NET_F_CSUM` —
/// our cold-start register multiplexing leaks the bit even though
/// `device_features` doesn't advertise it (see comment on
/// `tx_needs_csum`). Without this fix, TCP/UDP frames from the guest
/// arrive at libslirp with a partial pseudo-header sum in the L4
/// checksum field and slirp drops them as malformed.
///
/// Approach: zero the field, sum pseudo-header + L4 segment, fold,
/// invert, write. Robust against either of the two pre-fill conventions
/// (negated pseudo-sum vs. raw pseudo-sum) the kernel and spec disagree
/// on. Only handles IPv4. IPv6 + TCP/UDP could be added later; for now
/// IPv6 packets pass through unchanged and may get dropped by slirp.
fn fix_tx_l4_checksum(buf: &mut [u8]) {
    if buf.len() < 14 + 20 {
        return;
    }
    let etype = u16::from_be_bytes([buf[12], buf[13]]);
    if etype != 0x0800 {
        return;
    }
    let ip_hdr_off = 14;
    if (buf[ip_hdr_off] >> 4) != 4 {
        return;
    }
    let ihl = (buf[ip_hdr_off] & 0x0f) as usize * 4;
    if ihl < 20 || buf.len() < ip_hdr_off + ihl {
        return;
    }
    let total_len = u16::from_be_bytes([buf[ip_hdr_off + 2], buf[ip_hdr_off + 3]]) as usize;
    if total_len < ihl || buf.len() < ip_hdr_off + total_len {
        return;
    }
    let proto = buf[ip_hdr_off + 9];
    let csum_field_off = match proto {
        6 => ip_hdr_off + ihl + 16, // TCP checksum at offset 16 in TCP header
        17 => ip_hdr_off + ihl + 6, // UDP checksum at offset 6 in UDP header
        _ => return,
    };
    if csum_field_off + 2 > buf.len() {
        return;
    }
    let l4_off = ip_hdr_off + ihl;
    let l4_len = total_len - ihl;

    buf[csum_field_off] = 0;
    buf[csum_field_off + 1] = 0;

    let mut sum: u32 = 0;
    // Pseudo-header: src(4) + dst(4) + 0,proto(2) + len(2)
    for i in (0..4).step_by(2) {
        sum += u16::from_be_bytes([buf[ip_hdr_off + 12 + i], buf[ip_hdr_off + 13 + i]]) as u32;
        sum += u16::from_be_bytes([buf[ip_hdr_off + 16 + i], buf[ip_hdr_off + 17 + i]]) as u32;
    }
    sum += proto as u32;
    sum += l4_len as u32;
    // L4 segment (header + payload, with checksum field zeroed)
    let l4 = &buf[l4_off..l4_off + l4_len];
    let mut i = 0;
    while i + 1 < l4.len() {
        sum += u16::from_be_bytes([l4[i], l4[i + 1]]) as u32;
        i += 2;
    }
    if i < l4.len() {
        sum += (l4[i] as u32) << 8;
    }
    while sum > 0xFFFF {
        sum = (sum >> 16) + (sum & 0xFFFF);
    }
    let mut checksum: u16 = !(sum as u16);
    // RFC 768: a UDP checksum of 0 means "no checksum"; transmit 0xFFFF instead.
    if proto == 17 && checksum == 0 {
        checksum = 0xFFFF;
    }
    buf[csum_field_off] = (checksum >> 8) as u8;
    buf[csum_field_off + 1] = (checksum & 0xff) as u8;
}

/// VirtIO net header (virtio_net_hdr_mrg_rxbuf).
#[repr(C)]
#[derive(Default)]
struct VirtioNetHdrMrgRxbuf {
    flags: u8,
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    num_buffers: u16,
}

pub struct VirtioNet {
    slirp: *mut VdeSlirp,
    slirp_fd: i32,
    buffer: [u8; PACKET_SIZE],
    header_processed: bool,
    /// True when the most recent TX descriptor's vnet header had
    /// `VIRTIO_NET_HDR_F_NEEDS_CSUM` set, meaning the guest stack
    /// expects us to compute the L4 checksum before forwarding to slirp.
    /// Set in `process_queue_start(queue=1, ...)` and consumed (cleared)
    /// in `process_queue_complete(queue=1, ...)`. The bit is reachable
    /// even though we no longer advertise `VIRTIO_NET_F_CSUM` because
    /// the cold-start `device_features` register multiplexes via a
    /// single MMIO cell — the high half (`VIRTIO_F_VERSION_1`, bit 0
    /// of `features[1]`) leaks into the low-half read, and that bit
    /// happens to be `VIRTIO_NET_F_CSUM`. The race that fixes the leak
    /// is unwinnable on PCIe (see comment block in
    /// `virtio::run_device`), so we just compute the checksum here.
    tx_needs_csum: bool,
    /// Bytes accumulated into `buffer` during a multi-descriptor TX
    /// chain. The runner calls `process_queue_data` for every
    /// descriptor between the header and the last one; each descriptor
    /// holds a slice of the L2 frame the kernel built (Linux can split
    /// an `skb` across multiple page-sized fragments). Dropping the
    /// middle descriptors silently truncated the packet to whatever
    /// the LAST descriptor held — for TCP that surfaced as the guest
    /// emitting only the final fragment of an SSH banner, libslirp
    /// dropping it as malformed, and the host SSH client timing out
    /// at "banner exchange" (#58).
    tx_offset: usize,
    queue_header_size: u64,
    /// L2CPU index this device serves. Stored only for metric labels.
    l2cpu_idx: u8,
    /// Stable per-(card, L2CPU) MAC published in config space. See
    /// [`derive_mac`] for the encoding. Required by `VIRTIO_NET_F_MAC`,
    /// which we advertise so U-Boot stops spamming "No valid MAC
    /// address found" and Linux uses a deterministic address (#77).
    mac: [u8; 6],
    /// Per-(card, L2CPU) hostname plumbed into libslirp's vhostname so
    /// the guest's DHCP lease (option 12) carries
    /// e.g. `tt-bh-card0-l2cpu0` instead of libslirp's compiled-in
    /// "slirp" default. The CString must outlive `slirp` — libslirp
    /// keeps the pointer verbatim, so we hold ownership here. See #60.
    #[allow(dead_code)]
    hostname: std::ffi::CString,
}

unsafe impl Send for VirtioNet {}

impl Drop for VirtioNet {
    fn drop(&mut self) {
        if !self.slirp.is_null() {
            // libvdeslirp returns non-zero on internal teardown errors
            // (e.g., a NAT-table free that hits an unexpected state).
            // The instance is gone either way, but a non-zero return
            // can foreshadow EADDRINUSE on the next add-net for the
            // same forward port — log it so the operator has a paper
            // trail when add-net later fails.
            let rc = unsafe { vdeslirp_close(self.slirp) };
            if rc != 0 {
                eprintln!("vdeslirp_close returned {}", rc);
            }
        }
    }
}

impl VirtioNet {
    /// Construct a virtio-net device backed by slirp. `forwards` is
    /// a list of `(host_port, guest_port)` TCP pairs to register as
    /// slirp NAT entries — each becomes a `tcp_listen_add` on
    /// `127.0.0.1:<host_port>` that forwards to `10.0.2.15:<guest_port>`.
    ///
    /// Today's call sites:
    /// - `dispatch_add_net` builds `[(ssh_port, 22)]` plus any
    ///   `--fwd HOST:GUEST` extras the operator passed.
    /// - `start_net_worker` (the boot-path default) builds
    ///   `[(regs::slirp::ssh_port(card, idx), 22)]`.
    ///
    /// `card` + `l2cpu_idx` are used to derive a stable MAC via
    /// [`derive_mac`] that is published in config space and announced
    /// via `VIRTIO_NET_F_MAC`.
    pub fn new(forwards: &[(u16, u16)], card: u32, l2cpu_idx: u8) -> std::io::Result<Self> {
        let hostname = std::ffi::CString::new(format_dhcp_hostname(card, l2cpu_idx))
            .expect("hostname is plain ASCII, no embedded NULs");
        let mut cfg: SlirpConfig = unsafe { std::mem::zeroed() };
        unsafe {
            vdeslirp_init(&mut cfg, VDE_INIT_DEFAULT);
            // Override libslirp's "slirp" default vhostname with our
            // per-L2CPU name. Order matters: must run after
            // `vdeslirp_init` (which populates defaults) and before
            // `vdeslirp_open` (which copies / consumes the config).
            // See #60.
            tt_slirp_set_vhostname(&mut cfg, hostname.as_ptr());
        }
        let slirp = unsafe { vdeslirp_open(&mut cfg) };
        if slirp.is_null() {
            let err = std::io::Error::last_os_error();
            return Err(crate::Error::Io {
                ctx: "vdeslirp_open returned NULL. \
                 Likely causes: (1) file descriptor limit reached — check `ulimit -n`; \
                 (2) thread or socketpair creation blocked by a seccomp/container policy; \
                 (3) libvdeslirp/libslirp ABI mismatch — this build expects libvdeslirp \
                 0.1.x linked against libslirp 4.x (check `pkg-config --modversion vdeslirp libslirp`)"
                    .into(),
                source: err,
            }
            .into());
        }

        let host = InAddr::from_str("127.0.0.1");
        let guest = InAddr::from_str("10.0.2.15");
        for &(host_port, guest_port) in forwards {
            unsafe {
                vdeslirp_add_fwd(slirp, 0, host, host_port as i32, guest, guest_port as i32);
            }
        }

        let slirp_fd = unsafe { vdeslirp_fd(slirp) };

        Ok(VirtioNet {
            slirp,
            slirp_fd,
            buffer: [0u8; PACKET_SIZE],
            header_processed: false,
            tx_needs_csum: false,
            tx_offset: 0,
            queue_header_size: std::mem::size_of::<VirtioNetHdrMrgRxbuf>() as u64,
            l2cpu_idx,
            mac: derive_mac(card, l2cpu_idx),
            hostname,
        })
    }
}

impl VirtioDeviceImpl for VirtioNet {
    fn num_queues(&self) -> u32 {
        2
    }
    fn queue_header_size(&self) -> u64 {
        self.queue_header_size
    }
    fn device_id(&self) -> u32 {
        VIRTIO_ID_NET
    }
    fn device_features(&self) -> [u32; 2] {
        [VIRTIO_NET_F_MAC_BIT, VIRTIO_F_VERSION_1_BIT]
    }

    fn init_config(&self, config: *mut u8) {
        write_mac_into_config(&self.mac, config);
    }

    fn process_queue_start(&mut self, queue_idx: u32, addr: *mut u8, len: u64) {
        self.header_processed = true;
        if queue_idx == 0 {
            // RX: fill in net header
            let hdr = addr as *mut VirtioNetHdrMrgRxbuf;
            unsafe {
                ptr::write_volatile(&mut (*hdr).flags, 0);
                ptr::write_volatile(&mut (*hdr).num_buffers, 1);
                ptr::write_volatile(&mut (*hdr).gso_type, 0);
                ptr::write_volatile(&mut (*hdr).gso_size, 0);
            }
        } else if queue_idx == 1 {
            // TX: read the guest-supplied vnet header to learn whether
            // we need to fix up the L4 checksum before forwarding.
            let hdr = addr as *const VirtioNetHdrMrgRxbuf;
            let flags = unsafe { ptr::read_volatile(&(*hdr).flags) };
            self.tx_needs_csum = flags & VIRTIO_NET_HDR_F_NEEDS_CSUM != 0;
            self.tx_offset = 0;
            // The descriptor isn't required to be exactly 12 bytes — Linux
            // packs the L2 frame's leading bytes (Ethernet+IP+TCP headers)
            // into the same descriptor as the vnet_hdr when the skb's
            // first fragment is large enough. Anything past the 12-byte
            // vnet_hdr is real packet data that must land in the buffer
            // we hand to libslirp; ignoring it would emit a header-less
            // packet (the SSH-banner truncation that surfaced as #58).
            let hdr_size = self.queue_header_size as usize;
            if len as usize > hdr_size {
                let extra = len as usize - hdr_size;
                let copy_len = extra.min(PACKET_SIZE);
                unsafe {
                    ptr::copy_nonoverlapping(
                        addr.add(hdr_size),
                        self.buffer.as_mut_ptr(),
                        copy_len,
                    );
                }
                self.tx_offset = copy_len;
            }
        }
    }

    fn process_queue_data(&mut self, queue_idx: u32, addr: *mut u8, len: u64) {
        if queue_idx == 1 {
            // TX middle descriptor: append to the in-flight L2 frame.
            // See `tx_offset` for why this isn't a no-op.
            let copy_len = (len as usize).min(PACKET_SIZE - self.tx_offset);
            unsafe {
                ptr::copy_nonoverlapping(
                    addr,
                    self.buffer.as_mut_ptr().add(self.tx_offset),
                    copy_len,
                );
            }
            self.tx_offset += copy_len;
        }
    }

    fn process_queue_complete(&mut self, queue_idx: u32, addr: *mut u8, len: u64) -> u64 {
        if queue_idx == 0 {
            // RX: single-descriptor path runs `start` inline so the
            // vnet_hdr lands in place; multi-descriptor RX has the
            // separate hdr descriptor and we just write the payload
            // here. In both cases libslirp gives us a fresh frame.
            let mut data_addr = addr;
            let mut data_len = len;
            if !self.header_processed {
                self.process_queue_start(queue_idx, addr, len);
                data_addr = unsafe { addr.add(self.queue_header_size as usize) };
                data_len = len.saturating_sub(self.queue_header_size);
            }
            let max_copy = (data_len as usize).min(PACKET_SIZE);
            let pktlen = unsafe { vdeslirp_recv(self.slirp, self.buffer.as_mut_ptr(), max_copy) };
            if pktlen > 0 {
                let copy_len = (pktlen as usize).min(max_copy);
                unsafe {
                    ptr::copy_nonoverlapping(self.buffer.as_ptr(), data_addr, copy_len);
                }
                crate::daemon::metrics::NET_PACKETS_TOTAL
                    .rx(self.l2cpu_idx)
                    .inc();
                crate::daemon::metrics::NET_BYTES_TOTAL
                    .rx(self.l2cpu_idx)
                    .add(copy_len as u64);
            }
        } else if queue_idx == 1 {
            // TX: the L2 frame is reassembled into `self.buffer`
            // across `process_queue_start` (vnet_hdr + leading bytes
            // packed in the same descriptor) and `process_queue_data`
            // (middle descriptors). What lands here is either:
            //   - the trailing-data descriptor of a multi-descriptor
            //     chain (`header_processed == true`), or
            //   - the only descriptor of a single-descriptor chain
            //     (`header_processed == false`).
            // In the second case, `process_queue_start` does the
            // vnet_hdr read AND copies the full L2 frame into the
            // buffer; nothing left for us to append.
            if !self.header_processed {
                self.process_queue_start(queue_idx, addr, len);
            } else {
                let copy_len = (len as usize).min(PACKET_SIZE - self.tx_offset);
                unsafe {
                    ptr::copy_nonoverlapping(
                        addr,
                        self.buffer.as_mut_ptr().add(self.tx_offset),
                        copy_len,
                    );
                }
                self.tx_offset += copy_len;
            }
            let total_len = self.tx_offset;
            self.tx_offset = 0;
            if self.tx_needs_csum {
                fix_tx_l4_checksum(&mut self.buffer[..total_len]);
            }
            self.tx_needs_csum = false;
            unsafe {
                let ret = vdeslirp_send(self.slirp, self.buffer.as_ptr(), total_len);
                if ret < 0 {
                    eprintln!("vdeslirp_send failed: {}", ret);
                }
            }
            crate::daemon::metrics::NET_PACKETS_TOTAL
                .tx(self.l2cpu_idx)
                .inc();
            crate::daemon::metrics::NET_BYTES_TOTAL
                .tx(self.l2cpu_idx)
                .add(total_len as u64);
        }
        self.header_processed = false;
        // Buffer capacity, summed with earlier descriptors. virtio-net
        // RX may write less than the buffer (small packet); existing
        // kernels tolerate the over-report — leaving as-is for parity.
        len
    }

    fn queue_has_data(&self, queue_idx: u32) -> bool {
        if queue_idx == 0 {
            // RX: check if slirp has data via select with zero timeout
            let mut rfds = unsafe { std::mem::zeroed::<libc::fd_set>() };
            unsafe {
                libc::FD_SET(self.slirp_fd, &mut rfds);
            }
            let mut tv = libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            };
            let ret = unsafe {
                libc::select(
                    self.slirp_fd + 1,
                    &mut rfds,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut tv,
                )
            };
            ret > 0
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_dhcp_hostname_uses_card_and_l2cpu() {
        assert_eq!(format_dhcp_hostname(0, 0), "tt-bh-card0-l2cpu0");
        assert_eq!(format_dhcp_hostname(0, 3), "tt-bh-card0-l2cpu3");
        assert_eq!(format_dhcp_hostname(7, 1), "tt-bh-card7-l2cpu1");
    }

    #[test]
    fn format_dhcp_hostname_is_rfc952_clean() {
        // RFC 952: hostnames must be ≤63 chars, drawn from {a-z, 0-9, -}.
        // Underscores are common but not RFC-952; we deliberately avoid
        // them since some older DHCP clients reject the hostname option
        // when the name has them.
        for card in [0u32, 1, 99, 1234, u32::MAX] {
            for l2cpu in 0u8..4 {
                let h = format_dhcp_hostname(card, l2cpu);
                assert!(h.len() <= 63, "len {} > 63: {}", h.len(), h);
                for c in h.chars() {
                    assert!(
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-',
                        "non-RFC-952 char {:?} in {}",
                        c,
                        h
                    );
                }
                // CString round-trip: no embedded NULs.
                std::ffi::CString::new(h.clone()).unwrap_or_else(|_| panic!("CString: {}", h));
            }
        }
    }

    #[test]
    fn derive_mac_encodes_card_and_l2cpu_index() {
        assert_eq!(
            derive_mac(0, 0),
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x00],
            "card=0,l2cpu=0"
        );
        assert_eq!(
            derive_mac(0, 1),
            [0x02, 0x00, 0x00, 0x00, 0x01, 0x00],
            "card=0,l2cpu=1"
        );
        assert_eq!(
            derive_mac(0, 3),
            [0x02, 0x00, 0x00, 0x00, 0x03, 0x00],
            "card=0,l2cpu=3"
        );
        assert_eq!(
            derive_mac(1, 0),
            [0x02, 0x00, 0x01, 0x00, 0x00, 0x00],
            "card=1,l2cpu=0"
        );
        // Card index high byte exercised so a multi-card host with >255
        // cards (theoretical, but keep the encoding faithful) still
        // gets a unique MAC.
        assert_eq!(
            derive_mac(0x1234, 2),
            [0x02, 0x00, 0x34, 0x12, 0x02, 0x00],
            "card=0x1234,l2cpu=2"
        );
    }

    #[test]
    fn derive_mac_is_locally_administered_unicast() {
        // Locally-administered (bit 1 of byte 0 set) so we never collide
        // with a real OUI; unicast (bit 0 clear) so the kernel doesn't
        // treat the device as a broadcast/multicast endpoint.
        for card in [0u32, 1, 0xff, 0xffff] {
            for l2cpu in 0u8..4 {
                let mac = derive_mac(card, l2cpu);
                assert_ne!(
                    mac[0] & 0x02,
                    0,
                    "locally-administered bit must be set: card={card},l2cpu={l2cpu}"
                );
                assert_eq!(
                    mac[0] & 0x01,
                    0,
                    "multicast bit must be clear: card={card},l2cpu={l2cpu}"
                );
            }
        }
    }

    #[test]
    fn derive_mac_distinct_per_l2cpu_on_same_card() {
        let mut seen = std::collections::HashSet::new();
        for l2cpu in 0u8..4 {
            assert!(
                seen.insert(derive_mac(0, l2cpu)),
                "MAC must be unique per L2CPU (l2cpu={l2cpu})"
            );
        }
    }

    #[test]
    fn write_mac_into_config_lands_at_offset_zero() {
        let mut buf = [0xAAu8; 64];
        let mac = [0x02, 0x00, 0x00, 0x00, 0x05, 0x00];
        write_mac_into_config(&mac, buf.as_mut_ptr());
        assert_eq!(&buf[0..6], &mac, "MAC must land at config offset 0..6");
        // Bytes after the MAC must be untouched — we don't populate
        // status / max_virtqueue_pairs / mtu / speed / duplex, so the
        // surrounding sentinel pattern (0xAA) survives.
        for (i, b) in buf.iter().enumerate().skip(6) {
            assert_eq!(*b, 0xAA, "byte {i} must be untouched");
        }
    }

    #[test]
    fn device_features_advertises_mac() {
        // Bit 5 of features[0] is VIRTIO_NET_F_MAC. Without it U-Boot
        // refuses to use the interface and Linux falls back to a random
        // MAC. See #77.
        assert_ne!(
            VIRTIO_NET_F_MAC_BIT & (1 << 5),
            0,
            "VIRTIO_NET_F_MAC_BIT must encode bit 5 of features[0]"
        );
        assert_eq!(VIRTIO_NET_F_MAC_BIT, 1 << 5);
    }

    #[test]
    fn virtio_net_config_layout_matches_spec() {
        // VirtIO 1.2 §5.1.4: mac at offset 0, status at offset 6,
        // max_virtqueue_pairs at offset 8, mtu at offset 10, speed at
        // offset 12, duplex at offset 16. Our struct must agree with
        // the kernel's view; if alignment ever pushes mac off offset 0
        // the guest reads the wrong bytes.
        let cfg: VirtioNetConfig = unsafe { std::mem::zeroed() };
        let base = &cfg as *const _ as usize;
        assert_eq!(&cfg.mac as *const _ as usize - base, 0);
        assert_eq!(&cfg.status as *const _ as usize - base, 6);
        assert_eq!(&cfg.max_virtqueue_pairs as *const _ as usize - base, 8);
        assert_eq!(&cfg.mtu as *const _ as usize - base, 10);
        assert_eq!(&cfg.speed as *const _ as usize - base, 12);
        assert_eq!(&cfg.duplex as *const _ as usize - base, 16);
    }
}
