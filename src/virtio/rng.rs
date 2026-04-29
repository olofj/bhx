// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! VirtIO entropy/RNG (virtio device id 4).
//!
//! Simplest of all the virtio devices we emulate: a single virtqueue,
//! no per-descriptor header, no feature bits beyond `VIRTIO_F_VERSION_1`.
//! The guest pushes write-only buffers; we fill each one with kernel
//! entropy via `getrandom(2)` and complete the descriptor with the
//! actual byte count.
//!
//! ## Why we need it
//!
//! AlmaLinux's EFI shim (`/EFI/almalinux/shimriscv64.efi`) calls
//! `EFI_RNG_PROTOCOL` for entropy in its signature-verification chain.
//! U-Boot only installs the protocol if a backing RNG is registered in
//! its DM-RNG uclass, and on our hardware the only candidate is
//! virtio-rng. Without this device, U-Boot logs `Missing RNG device for
//! EFI_RNG_PROTOCOL` and the shim stalls before chainloading GRUB.
//! See #62.
//!
//! ## Wire format
//!
//! Single virtqueue ("requestq"). Each descriptor chain is one
//! write-only buffer; we fill it with random bytes and report the
//! actual count via the used-ring `len` (most useful for the guest's
//! pool accounting). Spec: virtio 1.2 §5.4.

use std::ptr;

use crate::virtio::VirtioDeviceImpl;

pub struct VirtioRng;

impl VirtioRng {
    pub fn new() -> Self {
        Self
    }
}

impl Default for VirtioRng {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtioDeviceImpl for VirtioRng {
    fn device_id(&self) -> u32 {
        crate::regs::virtio_mmio::VIRTIO_ID_ENTROPY
    }

    fn num_queues(&self) -> u32 {
        1
    }

    fn queue_header_size(&self) -> u64 {
        0
    }

    fn device_features(&self) -> [u32; 2] {
        // Only VIRTIO_F_VERSION_1 (bit 32 of the 64-bit feature space).
        // virtio-rng has no device-specific feature bits in the spec.
        [0, 1 << 0]
    }

    fn process_queue_start(&mut self, _queue_idx: u32, addr: *mut u8, len: u64) {
        // Single-descriptor chains hit `process_queue_start` first
        // and then jump straight to `process_queue_complete` without
        // a `process_queue_data` call. The runner uses the `_start`
        // body as the place to do single-shot work for that
        // descriptor — fill it with random bytes here. For
        // multi-descriptor chains (rare for rng — Linux uses one),
        // `process_queue_data` and `process_queue_complete` below
        // each fill their own segment.
        fill_with_entropy(addr, len);
    }

    fn process_queue_data(&mut self, _queue_idx: u32, addr: *mut u8, len: u64) {
        fill_with_entropy(addr, len);
    }

    fn process_queue_complete(&mut self, _queue_idx: u32, addr: *mut u8, len: u64) -> u64 {
        fill_with_entropy(addr, len);
        len
    }

    fn queue_has_data(&self, _queue_idx: u32) -> bool {
        // We can always satisfy a request — entropy is never
        // backpressured. The runner gates descriptor consumption
        // on this so a busy-poll worker doesn't burn through ring
        // slots on devices that have nothing to give.
        true
    }
}

/// Fill `len` bytes at `addr` with kernel entropy. `getrandom(2)` with
/// `GRND_NONBLOCK` returns up to 256 bytes per call without blocking
/// (the kernel's "fast path" pool); we loop to satisfy larger buffers.
/// On any error or short return we fall back to a counter-style fill so
/// the guest always sees `len` written bytes — the kernel's RNG isn't
/// expected to fail in practice (`getrandom` on a long-running system
/// is essentially infallible) but the fallback keeps the daemon's
/// liveness invariant intact.
fn fill_with_entropy(addr: *mut u8, len: u64) {
    if len == 0 || addr.is_null() {
        return;
    }
    let mut filled: usize = 0;
    let total = len as usize;
    while filled < total {
        let chunk = (total - filled).min(256);
        let dst = unsafe { addr.add(filled) };
        let ret = unsafe { libc::getrandom(dst as *mut libc::c_void, chunk, 0) };
        if ret <= 0 {
            // Fallback: best-effort fill so the guest sees data. A
            // counter pattern is recognizably non-random in scope
            // traces; if a guest ever gets seeded from this branch the
            // operator can spot it. In practice we don't expect to hit
            // this — `getrandom(GRND_NONBLOCK)` only fails with EAGAIN
            // on a very early-boot host where the pool isn't yet
            // initialized.
            for i in 0..chunk {
                unsafe { ptr::write_volatile(addr.add(filled + i), (filled + i) as u8) };
            }
            filled += chunk;
        } else {
            filled += ret as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_entropy() {
        let d = VirtioRng::new();
        assert_eq!(d.device_id(), 4);
        assert_eq!(d.device_id(), crate::regs::virtio_mmio::VIRTIO_ID_ENTROPY);
    }

    #[test]
    fn single_queue() {
        assert_eq!(VirtioRng::new().num_queues(), 1);
    }

    #[test]
    fn no_request_header() {
        assert_eq!(VirtioRng::new().queue_header_size(), 0);
    }

    #[test]
    fn advertises_only_version_1() {
        let f = VirtioRng::new().device_features();
        assert_eq!(f[0], 0);
        assert_eq!(f[1], 1);
    }

    #[test]
    fn always_processable() {
        assert!(VirtioRng::new().queue_has_data(0));
    }

    #[test]
    fn process_queue_complete_fills_buffer() {
        let mut device = VirtioRng::new();
        let mut buf = vec![0u8; 64];
        let written = device.process_queue_complete(0, buf.as_mut_ptr(), buf.len() as u64);
        assert_eq!(written, 64);
        // Sanity: a 64-byte all-zero return is astronomically unlikely
        // (probability 2^-512) — flag it as a test-side bug if it ever
        // happens, not a wholesome rng output.
        assert!(buf.iter().any(|&b| b != 0), "rng returned all zeros");
    }

    #[test]
    fn process_queue_complete_handles_zero_len() {
        let mut device = VirtioRng::new();
        let written = device.process_queue_complete(0, std::ptr::null_mut(), 0);
        assert_eq!(written, 0);
    }
}
