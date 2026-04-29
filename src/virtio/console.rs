// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! VirtIO-console (virtio device id 3, single-port, no MULTIPORT).
//!
//! Pairs with `chip_console::uart_pass`: that one services the OpenSBI
//! virtual-UART (used by the bootloader chain and our patched kernel's
//! HVC SBI driver); this one services the standard
//! `CONFIG_VIRTIO_CONSOLE` driver that distro kernels ship — the only
//! console path available on stock distro kernels post-Linux 6.8 since
//! `HVC_RISCV_SBI` got gated behind `NONPORTABLE` (see #51 for context).
//!
//! Both pumps push into the same `ConsoleHub`, so an operator's
//! `bhx connect` view shows kernel output regardless of which
//! console driver the kernel ended up using. Operator keystrokes are
//! fanned out to both pumps in `client_reader_main`; whichever HVC
//! driver the kernel registered as its active console absorbs them,
//! the other's input ring fills harmlessly.
//!
//! ## Wire format
//!
//! Two virtqueues, no header:
//!   * Queue 0 (RX, host → guest): guest adds write-only buffers; we
//!     fill them from the operator-keystroke channel and report the
//!     **actual** bytes written via the used-ring `len` (the runner
//!     special-cases this — most other devices report buffer
//!     capacity).
//!   * Queue 1 (TX, guest → host): guest writes data to read-only
//!     buffers; we drain them into the console hub.
//!
//! ## Features
//!
//! We advertise no features. Specifically NOT:
//! - `VIRTIO_CONSOLE_F_SIZE` — would require us to track host
//!   terminal cols/rows and notify the guest on resize. Default
//!   `cols=rows=0` ("don't know") is fine.
//! - `VIRTIO_CONSOLE_F_MULTIPORT` — single-port keeps the queue layout
//!   simple. Multiport adds 2 control queues + per-port data queues.
//! - `VIRTIO_CONSOLE_F_EMERG_WRITE` — late-stage panic-time write to a
//!   special MMIO config field. Nice-to-have, not on the M3 path.

use std::collections::VecDeque;
use std::ptr;
use std::sync::{Arc, Mutex};

use crate::daemon::console_hub::ConsoleHub;
use crate::virtio::VirtioDeviceImpl;

/// virtio 1.2 §5.3.4 — device-specific config layout.
#[repr(C)]
struct VirtioConsoleConfig {
    cols: u16,
    rows: u16,
    max_nr_ports: u32,
    emerg_wr: u32,
}

const RX_QUEUE: u32 = 0;
const TX_QUEUE: u32 = 1;

/// Bound on the operator → virtio-console keystroke buffer. Bytes
/// queued past this cap are silently dropped by the
/// `client_reader_main` fan-out — that's the back-pressure path for
/// guests that have virtio-console bound but aren't reading from it
/// (e.g. a patched kernel where SBI HVC is the active console and
/// `/dev/hvc1` is just sitting open with nobody draining it).
pub const RX_BUFFER_CAP: usize = 16 * 1024;

pub struct VirtioConsole {
    hub: Arc<ConsoleHub>,
    /// Keystroke ring fed by `client_reader_main`. We drain in the RX
    /// completion path. Use `Mutex<VecDeque>` rather than `mpsc` so
    /// `queue_has_data` (`&self`) can probe non-destructively; mpsc's
    /// only way to ask "is there data?" is to take a byte.
    input_buf: Arc<Mutex<VecDeque<u8>>>,
    /// Accumulator across multi-descriptor TX chains. Single-descriptor
    /// is the common case (Linux's virtio_console driver coalesces
    /// writes), but the spec allows chains, so we accumulate in
    /// `process_queue_data` and flush in `process_queue_complete`.
    tx_buf: Vec<u8>,
}

impl VirtioConsole {
    pub fn new(hub: Arc<ConsoleHub>, input_buf: Arc<Mutex<VecDeque<u8>>>) -> Self {
        Self {
            hub,
            input_buf,
            tx_buf: Vec::with_capacity(256),
        }
    }
}

impl VirtioDeviceImpl for VirtioConsole {
    fn device_id(&self) -> u32 {
        crate::regs::virtio_mmio::VIRTIO_ID_CONSOLE
    }

    fn num_queues(&self) -> u32 {
        2
    }

    fn queue_header_size(&self) -> u64 {
        0
    }

    fn device_features(&self) -> [u32; 2] {
        // VIRTIO_F_VERSION_1 (feature bit 32) is required by the
        // runner's modern-only handshake. The high u32 covers feature
        // bits 32..64; bit 32 of the full 64-bit space is bit 0 of the
        // high u32. Everything else off.
        [0, 1 << 0]
    }

    fn init_config(&self, config: *mut u8) {
        let cfg = config as *mut VirtioConsoleConfig;
        unsafe {
            ptr::write_volatile(&mut (*cfg).cols, 0);
            ptr::write_volatile(&mut (*cfg).rows, 0);
            ptr::write_volatile(&mut (*cfg).max_nr_ports, 0);
            ptr::write_volatile(&mut (*cfg).emerg_wr, 0);
        }
    }

    fn process_queue_start(&mut self, queue_idx: u32, addr: *mut u8, len: u64) {
        if queue_idx == TX_QUEUE {
            self.tx_buf.clear();
            // Fast path: most TX chains are a single descriptor. Stash
            // its bytes here so a one-shot chain doesn't need
            // `process_queue_data`.
            let bytes = unsafe { std::slice::from_raw_parts(addr, len as usize) };
            self.tx_buf.extend_from_slice(bytes);
        }
        // RX (queue 0): the descriptor is write-only. We fill it in
        // `process_queue_complete`.
    }

    fn process_queue_data(&mut self, queue_idx: u32, addr: *mut u8, len: u64) {
        if queue_idx == TX_QUEUE {
            let bytes = unsafe { std::slice::from_raw_parts(addr, len as usize) };
            self.tx_buf.extend_from_slice(bytes);
        }
    }

    fn process_queue_complete(&mut self, queue_idx: u32, addr: *mut u8, len: u64) -> u64 {
        if queue_idx == TX_QUEUE {
            // Flush accumulated TX bytes to the console hub. The hub
            // fans out to all attached `connect` clients.
            if !self.tx_buf.is_empty() {
                let _ = self.hub.push_chip_output(&self.tx_buf);
                self.tx_buf.clear();
            } else if len > 0 {
                let bytes = unsafe { std::slice::from_raw_parts(addr, len as usize) };
                let _ = self.hub.push_chip_output(bytes);
            }
            // Pass capacity through for TX — the kernel doesn't read
            // used-ring `len` for read-only chains, and matching
            // block/net's "sum chain capacities" shape minimizes
            // surprise.
            len
        } else {
            // RX: drain `input_buf` into the guest-supplied buffer.
            // Never block — if no input is ready we complete with 0
            // bytes written and the kernel re-adds the buffer.
            let mut written = 0usize;
            let mut q = self.input_buf.lock().unwrap();
            while written < len as usize {
                match q.pop_front() {
                    Some(b) => {
                        unsafe {
                            *addr.add(written) = b;
                        }
                        written += 1;
                    }
                    None => break,
                }
            }
            written as u64
        }
    }

    fn queue_has_data(&self, queue_idx: u32) -> bool {
        match queue_idx {
            // TX is always processable: the kernel's writes are
            // already in the descriptor's buffer; we just need a
            // chance to drain them.
            TX_QUEUE => true,
            // RX should only consume an avail buffer when there's
            // actual input to put in it. Otherwise the daemon would
            // burn through every buffer the kernel adds with 0-byte
            // completions, which the kernel handles but generates
            // unnecessary virtqueue churn.
            RX_QUEUE => !self.input_buf.lock().unwrap().is_empty(),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Worker entry point
// ---------------------------------------------------------------------------

/// Per-L2CPU virtio-console worker. Hosts a `VirtioConsole` and runs
/// `virtio::run_device` against the dedicated MMIO slot.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
mod tests {
    use super::*;

    fn make() -> (VirtioConsole, Arc<Mutex<VecDeque<u8>>>) {
        let hub = Arc::new(ConsoleHub::new(0));
        let input_buf = Arc::new(Mutex::new(VecDeque::new()));
        (VirtioConsole::new(hub, input_buf.clone()), input_buf)
    }

    #[test]
    fn device_id_is_console() {
        let (d, _) = make();
        assert_eq!(d.device_id(), 3);
        assert_eq!(d.device_id(), crate::regs::virtio_mmio::VIRTIO_ID_CONSOLE);
    }

    #[test]
    fn single_port_two_queues() {
        let (d, _) = make();
        assert_eq!(d.num_queues(), 2);
    }

    #[test]
    fn no_request_header() {
        let (d, _) = make();
        assert_eq!(d.queue_header_size(), 0);
    }

    #[test]
    fn features_advertise_only_version_1() {
        let (d, _) = make();
        // Bit 32 (VERSION_1) is in features[1] bit 0. No other bits
        // set — no MULTIPORT, F_SIZE, EMERG_WRITE.
        assert_eq!(d.device_features(), [0, 1]);
    }

    #[test]
    fn config_layout_matches_spec() {
        // virtio 1.2 §5.3.4: cols (u16), rows (u16), max_nr_ports
        // (u32), emerg_wr (u32) — total 12 bytes, packed.
        assert_eq!(std::mem::size_of::<VirtioConsoleConfig>(), 12);
        assert_eq!(std::mem::offset_of!(VirtioConsoleConfig, cols), 0);
        assert_eq!(std::mem::offset_of!(VirtioConsoleConfig, rows), 2);
        assert_eq!(std::mem::offset_of!(VirtioConsoleConfig, max_nr_ports), 4);
        assert_eq!(std::mem::offset_of!(VirtioConsoleConfig, emerg_wr), 8);
    }

    #[test]
    fn init_config_writes_zeros() {
        let (d, _) = make();
        let mut buf = [0xAAu8; 12];
        d.init_config(buf.as_mut_ptr());
        assert_eq!(buf, [0u8; 12]);
    }

    #[test]
    fn rx_skips_when_input_buf_empty() {
        let (d, _) = make();
        assert!(!d.queue_has_data(RX_QUEUE));
    }

    #[test]
    fn rx_ready_when_input_buf_nonempty() {
        let (d, buf) = make();
        buf.lock().unwrap().push_back(b'x');
        assert!(d.queue_has_data(RX_QUEUE));
    }

    #[test]
    fn tx_always_ready() {
        let (d, _) = make();
        assert!(d.queue_has_data(TX_QUEUE));
    }

    #[test]
    fn rx_complete_drains_input_into_buffer_and_returns_actual_count() {
        let (mut d, buf) = make();
        {
            let mut q = buf.lock().unwrap();
            q.push_back(b'a');
            q.push_back(b'b');
            q.push_back(b'c');
        }
        let mut out = [0u8; 8]; // capacity 8, only 3 bytes available
        let written = d.process_queue_complete(RX_QUEUE, out.as_mut_ptr(), out.len() as u64);
        assert_eq!(written, 3);
        assert_eq!(&out[..3], b"abc");
        // Untouched suffix.
        assert_eq!(&out[3..], &[0, 0, 0, 0, 0]);
        // Input buf drained.
        assert!(buf.lock().unwrap().is_empty());
    }

    #[test]
    fn rx_complete_returns_zero_when_input_buf_empty() {
        let (mut d, _) = make();
        let mut out = [0u8; 4];
        let written = d.process_queue_complete(RX_QUEUE, out.as_mut_ptr(), out.len() as u64);
        assert_eq!(written, 0);
    }

    #[test]
    fn rx_complete_does_not_overrun_short_buffer() {
        let (mut d, buf) = make();
        for b in b"hello world" {
            buf.lock().unwrap().push_back(*b);
        }
        let mut out = [0u8; 5];
        let written = d.process_queue_complete(RX_QUEUE, out.as_mut_ptr(), out.len() as u64);
        assert_eq!(written, 5);
        assert_eq!(&out, b"hello");
        // The remaining 6 bytes stay queued for the next RX descriptor.
        let q = buf.lock().unwrap();
        let remaining: Vec<u8> = q.iter().copied().collect();
        assert_eq!(remaining, b" world");
    }
}
