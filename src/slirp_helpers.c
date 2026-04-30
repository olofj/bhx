// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT
//
// Small C shim — pokes individual fields on libslirp's SlirpConfig
// after `vdeslirp_init` has populated defaults. Required because
// libslirp's SlirpConfig is opaque from our Rust side (treated as a
// 512-byte buffer in `slirp_ffi.rs`); writing one named field would
// otherwise mean mirroring the entire struct layout in Rust and
// chasing layout drift across libslirp versions. This C compiler-side
// `#include` is the version-stable answer.
//
// See #60 for the per-L2CPU DHCP hostname motivation.

#include <arpa/inet.h>
#include <slirp/libslirp.h>

// Stash a hostname pointer into the config. The string must outlive
// the libslirp instance; libslirp keeps the pointer verbatim. Caller
// (`VirtioNet::new`) holds a `CString` field for that.
void tt_slirp_set_vhostname(SlirpConfig *cfg, const char *vhostname) {
    cfg->vhostname = vhostname;
}

// Override the IPv4 DNS resolver slirp hands out to the guest via
// DHCP. Default is `10.0.2.3` — slirp's built-in DNS proxy that
// forwards to whatever's in the host's `/etc/resolv.conf`. On hosts
// where resolv.conf points at a host-only IP (Tailscale's MagicDNS
// at `100.100.100.100`, systemd-resolved's `127.0.0.53`,
// dnsmasq-style local resolvers), slirp's NAT can't reach that
// target and DNS dies even though everything else routes fine.
//
// Setting `vnameserver` to a public resolver (e.g., `8.8.8.8`)
// makes the guest query it directly via slirp's NAT rather than
// going through the proxy. Trade-off: queries don't honor any
// host-side DNS config (split-horizon zones, /etc/hosts, etc.).
//
// `addr` is in network byte order. Caller can pass
// `htonl(0x08080808)` (i.e. `8.8.8.8`) directly.
void bhx_slirp_set_vnameserver(SlirpConfig *cfg, uint32_t addr) {
    struct in_addr in;
    in.s_addr = addr;
    cfg->vnameserver = in;
}
