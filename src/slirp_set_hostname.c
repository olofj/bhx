// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT
//
// Small C shim — pokes the `vhostname` field on libslirp's SlirpConfig
// after `vdeslirp_init` has populated defaults. Required because
// libslirp's SlirpConfig is opaque from our Rust side (treated as a
// 512-byte buffer in `slirp_ffi.rs`); writing one named field would
// otherwise mean mirroring the entire struct layout in Rust and
// chasing layout drift across libslirp versions. This C compiler-side
// `#include` is the version-stable answer.
//
// See #60 for the per-L2CPU DHCP hostname motivation.

#include <slirp/libslirp.h>

// Stash a hostname pointer into the config. The string must outlive
// the libslirp instance; libslirp keeps the pointer verbatim. Caller
// (`VirtioNet::new`) holds a `CString` field for that.
void tt_slirp_set_vhostname(SlirpConfig *cfg, const char *vhostname) {
    cfg->vhostname = vhostname;
}
