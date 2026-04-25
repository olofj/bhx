// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Chip-side console loop for the daemon.
//!
//! Mirrors the queue-ring mechanics of [`crate::console::console_main`] but
//! replaces stdin/stdout I/O with [`ConsoleHub`] fan-out and an input channel
//! fed by attached clients. Bytes out: chip → hub → all clients. Bytes in:
//! channel → chip (the channel is only written to by the client whose id
//! matches `hub.current_writer_id()`).

use std::ptr;
use std::sync::atomic::{self, AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crate::daemon::console_hub::ConsoleHub;
use crate::l2cpu::L2Cpu;

const BUFFER_SIZE: u32 = 0x1000;
const VIRTUAL_UART_MAGIC: u64 = 0x5649525455415254; // "VIRTUART"

const OPENSBI_DEBUG_PTR: u64 = 0x80;
const EYE_CATCHER: &[u8; 8] = b"OSBIdbug";

const OFF_MAGIC: usize = 0;
const OFF_TX_BUF: usize = 8;
const OFF_RX_BUF: usize = 8 + BUFFER_SIZE as usize;
const OFF_TX_HEAD: usize = 8 + 2 * BUFFER_SIZE as usize;
const OFF_TX_TAIL: usize = OFF_TX_HEAD + 4;
const OFF_RX_HEAD: usize = OFF_TX_TAIL + 4;
const OFF_RX_TAIL: usize = OFF_RX_HEAD + 4;

#[repr(C)]
struct DebugDescriptor {
    eye_catcher: [u8; 8],
    version: u32,
    virtuart_base: u64,
}

unsafe fn read_magic(q: *const u8) -> u64 {
    ptr::read_volatile(q.add(OFF_MAGIC) as *const u64)
}
unsafe fn read_tx_head(q: *const u8) -> u32 {
    ptr::read_volatile(q.add(OFF_TX_HEAD) as *const u32)
}
unsafe fn read_tx_tail(q: *const u8) -> u32 {
    ptr::read_volatile(q.add(OFF_TX_TAIL) as *const u32)
}
unsafe fn write_tx_tail(q: *mut u8, val: u32) {
    ptr::write_volatile(q.add(OFF_TX_TAIL) as *mut u32, val);
}
unsafe fn read_rx_head(q: *const u8) -> u32 {
    ptr::read_volatile(q.add(OFF_RX_HEAD) as *const u32)
}
unsafe fn read_rx_tail(q: *const u8) -> u32 {
    ptr::read_volatile(q.add(OFF_RX_TAIL) as *const u32)
}
unsafe fn write_rx_head(q: *mut u8, val: u32) {
    ptr::write_volatile(q.add(OFF_RX_HEAD) as *mut u32, val);
}
unsafe fn can_push(q: *const u8) -> bool {
    atomic::fence(Ordering::Acquire);
    let head = read_rx_head(q) % BUFFER_SIZE;
    let tail = read_rx_tail(q) % BUFFER_SIZE;
    (head + 1) % BUFFER_SIZE != tail
}
unsafe fn can_pop(q: *const u8) -> bool {
    atomic::fence(Ordering::Acquire);
    let head = read_tx_head(q) % BUFFER_SIZE;
    let tail = read_tx_tail(q) % BUFFER_SIZE;
    head != tail
}
unsafe fn push_char(q: *mut u8, c: u8) -> bool {
    if !can_push(q) {
        return false;
    }
    // Single MMIO read of rx_head — reuse for slot index and next-head
    // write. Two reads would mean two PCIe round-trips per byte and a
    // principled race window if the guest advanced its side in between.
    let head = read_rx_head(q);
    let slot = (head % BUFFER_SIZE) as usize;
    ptr::write_volatile(q.add(OFF_RX_BUF + slot), c);
    atomic::fence(Ordering::Release);
    write_rx_head(q, (head + 1) % BUFFER_SIZE);
    true
}
unsafe fn pop_char(q: *mut u8) -> u8 {
    let tail = read_tx_tail(q);
    let slot = (tail % BUFFER_SIZE) as usize;
    let c = ptr::read_volatile(q.add(OFF_TX_BUF + slot));
    atomic::fence(Ordering::Release);
    write_tx_tail(q, (tail + 1) % BUFFER_SIZE);
    c
}

/// One pass: locate the UART ring and drive bytes in both directions until
/// the chip's magic eye-catcher is lost (reset) or exit is requested.
fn uart_pass(
    l2cpu: &L2Cpu,
    hub: &ConsoleHub,
    input_rx: &mpsc::Receiver<u8>,
    exit_flag: &AtomicBool,
) -> std::io::Result<UartExit> {
    let starting_address = l2cpu.starting_address();
    let tile = l2cpu.coordinates();

    let debug_ptr = l2cpu.read32(starting_address + OPENSBI_DEBUG_PTR);
    let uart_base = {
        let desc_window = l2cpu.get_persistent_2m_window(starting_address + debug_ptr as u64)?;
        let desc = desc_window.get_window() as *const DebugDescriptor;
        for (i, &expected) in EYE_CATCHER.iter().enumerate() {
            let byte = unsafe { ptr::read_volatile(&(*desc).eye_catcher[i]) };
            if byte != expected {
                eprintln!(
                    "[console l2cpu {}] debug descriptor eye catcher mismatch",
                    l2cpu.idx()
                );
                return Ok(UartExit::Retry);
            }
        }
        let base = unsafe { ptr::read_volatile(&(*desc).virtuart_base) };
        if base == !0u64 {
            eprintln!(
                "[console l2cpu {}] virtuart_base is ~0; chip not ready",
                l2cpu.idx()
            );
            return Ok(UartExit::Retry);
        }
        base
    };
    eprintln!(
        "[console l2cpu {}] attached virt UART @ 0x{:x} (tile {},{})",
        l2cpu.idx(),
        uart_base,
        tile.x,
        tile.y
    );

    let queue_window = l2cpu.get_persistent_2m_window(uart_base)?;
    let q = queue_window.get_window();

    // Small batch buffer for chip TX → hub. Keeps the per-iteration overhead
    // down; the hub's fan-out is one syscall per client per push.
    let mut out_buf = [0u8; 256];

    // Three-tier adaptive sleep with hysteresis:
    //   - FAST  (100 µs) while console is actively producing/consuming
    //   - SLOW  (1 ms)   after FAST_WINDOW (200 ms) with no activity
    //   - IDLE  (10 ms)  after IDLE_WINDOW (2 s) with no activity
    // The IDLE tier dominates idle-daemon CPU: at SLOW we polled
    // 1000×/s burning ~2% per worker, IDLE drops that to 100×/s. Cap
    // at 10 ms so bursty guest output (kernel printk to the 4 KiB TX
    // ring) can't fill the ring before we drain it — the chip's ring
    // size sets the cap, not the kernel's tolerable latency. See #27.
    const FAST_SLEEP: Duration = Duration::from_micros(100);
    const SLOW_SLEEP: Duration = Duration::from_millis(1);
    const IDLE_SLEEP: Duration = Duration::from_millis(10);
    const FAST_WINDOW: Duration = Duration::from_millis(200);
    const IDLE_WINDOW: Duration = Duration::from_secs(2);
    let mut last_active = std::time::Instant::now();

    loop {
        if exit_flag.load(Ordering::Relaxed) {
            return Ok(UartExit::Done);
        }

        let magic = unsafe { read_magic(q) };
        if u64::from_le(magic) != VIRTUAL_UART_MAGIC {
            return Ok(UartExit::Retry);
        }

        // Drain up to N bytes from chip TX this pass.
        let mut n = 0usize;
        while n < out_buf.len() && unsafe { can_pop(q) } {
            out_buf[n] = unsafe { pop_char(q) };
            n += 1;
        }
        let got_output = n > 0;
        if got_output {
            crate::daemon::metrics::L2CPU_CONSOLE_BYTES_TOTAL
                .g2h(l2cpu.idx() as u8)
                .add(n as u64);
            let _ = hub.push_chip_output(&out_buf[..n]);
        }

        // Drain pending input from attached writer client (non-blocking).
        let mut got_input = false;
        loop {
            match input_rx.try_recv() {
                Ok(b) => {
                    got_input = true;
                    crate::daemon::metrics::L2CPU_CONSOLE_BYTES_TOTAL
                        .h2g(l2cpu.idx() as u8)
                        .inc();
                    // Wait for the guest's SBI layer to drain the 4 KiB RX
                    // ring. Unbounded with a short sleep per iteration —
                    // upstream (mpsc channel + socket buffer) naturally
                    // back-pressures the client if we fall behind, so we
                    // shouldn't lose bytes on the way in. An earlier
                    // version capped this at 10 000 spin_loop iterations
                    // and dropped bytes past that, which caused sha
                    // mismatches in sustained-write workloads (64 KiB+ at
                    // a time would lose random bytes as the guest
                    // couldn't keep up with microsecond-scale spins).
                    while !unsafe { push_char(q, b) } {
                        if exit_flag.load(Ordering::Relaxed) {
                            return Ok(UartExit::Done);
                        }
                        std::thread::sleep(Duration::from_micros(100));
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Only happens at shutdown.
                    return Ok(UartExit::Done);
                }
            }
        }

        // Pick the sleep tier based on how long it's been since we last
        // did real work — see the FAST/SLOW/IDLE constants above.
        if got_output || got_input {
            last_active = std::time::Instant::now();
        }
        let elapsed = last_active.elapsed();
        let tier = crate::daemon::metrics::classify_tier(elapsed, FAST_WINDOW, IDLE_WINDOW);
        let sleep = match tier {
            crate::daemon::metrics::Tier::Fast => FAST_SLEEP,
            crate::daemon::metrics::Tier::Slow => SLOW_SLEEP,
            crate::daemon::metrics::Tier::Idle => IDLE_SLEEP,
        };
        let idx_u8 = l2cpu.idx() as u8;
        crate::daemon::metrics::WORKER_POLL_ITERATIONS_TOTAL
            .at(
                crate::daemon::metrics::WorkerKind::ChipConsole,
                idx_u8,
                tier,
            )
            .inc();
        crate::daemon::metrics::WORKER_TIER_NANOS_TOTAL
            .at(
                crate::daemon::metrics::WorkerKind::ChipConsole,
                idx_u8,
                tier,
            )
            .add(sleep.as_nanos() as u64);
        std::thread::sleep(sleep);
    }
}

enum UartExit {
    /// Exit flag was set, tear down cleanly.
    Done,
    /// Magic mismatch / chip reset — retry the pass after a short wait.
    Retry,
}

/// Probe whether a released L2CPU's chip-side memory has the expected
/// OpenSBI debug descriptor and VIRTUART magic — i.e. whether it's a
/// warm-resume candidate rather than a wedged or half-booted core.
///
/// Called at daemon startup once per released core (bit `idx+4` = 1 in
/// L2CPU_RESET). Opens transient 2 MiB TLB windows at the descriptor
/// pointer and queue base; both windows are released when this function
/// returns. Any failure on the probe path is treated as "not viable"
/// (caller marks the core wedged).
///
/// Allocates one ioctl-backed window each for the descriptor and queue.
/// Calls about ~2× the allocator cost of a single `read32`. Net cost at
/// daemon start for 4 released cores is well under 100 ms on BH.
pub fn probe_warm_resume(l2cpu: &L2Cpu) -> bool {
    let starting_address = l2cpu.starting_address();
    let debug_ptr = l2cpu.read32(starting_address + OPENSBI_DEBUG_PTR);

    let desc_window = match l2cpu.get_persistent_2m_window(starting_address + debug_ptr as u64) {
        Ok(w) => w,
        Err(e) => {
            eprintln!(
                "[probe l2cpu {}] descriptor window failed: {}",
                l2cpu.idx(),
                e
            );
            return false;
        }
    };
    let desc_ptr = desc_window.get_window() as *const DebugDescriptor;

    // Pull the descriptor bytes into a stack buffer with volatile reads so
    // the compiler can't hoist / re-order them against the subsequent magic
    // read. The pure-decode helper below operates on the copy.
    let mut desc_bytes = [0u8; DESCRIPTOR_BYTES];
    for (i, b) in desc_bytes.iter_mut().enumerate() {
        *b = unsafe { ptr::read_volatile((desc_ptr as *const u8).add(i)) };
    }

    // The virtuart_base field lives past any eye-catcher byte check, so we
    // only need the probe_decode helper after we have the magic read too —
    // but we need virtuart_base first to know where that read lands. Split
    // the decode in two: first the descriptor half (decides whether to
    // read magic at all), then combined with the magic bytes.
    let uart_base = match decode_descriptor(&desc_bytes) {
        Ok(b) => b,
        Err(DescriptorError::EyeCatcherMismatch { offset, got, want }) => {
            eprintln!(
                "[probe l2cpu {}] OSBIdbug eye catcher mismatch at byte {} (got 0x{:02x}, want 0x{:02x})",
                l2cpu.idx(),
                offset,
                got,
                want
            );
            return false;
        }
        Err(DescriptorError::VirtuartBaseUninit) => {
            eprintln!(
                "[probe l2cpu {}] virtuart_base is ~0 (chip not fully initialized)",
                l2cpu.idx()
            );
            return false;
        }
    };

    let queue_window = match l2cpu.get_persistent_2m_window(uart_base) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[probe l2cpu {}] queue window failed: {}", l2cpu.idx(), e);
            return false;
        }
    };
    let q = queue_window.get_window();
    let mut magic_bytes = [0u8; 8];
    for (i, b) in magic_bytes.iter_mut().enumerate() {
        *b = unsafe { ptr::read_volatile(q.add(OFF_MAGIC + i)) };
    }
    match decode_magic(&magic_bytes) {
        Ok(()) => {
            eprintln!(
                "[probe l2cpu {}] warm-resume viable (virtuart @ 0x{:x})",
                l2cpu.idx(),
                uart_base
            );
            true
        }
        Err(got) => {
            eprintln!(
                "[probe l2cpu {}] virt UART magic is 0x{:016x} (want 0x{:016x}) — wedged",
                l2cpu.idx(),
                got,
                VIRTUAL_UART_MAGIC
            );
            false
        }
    }
}

/// Number of bytes we read from the OpenSBI debug descriptor. Matches the
/// `#[repr(C)]` layout of [`DebugDescriptor`]: 8 (eye_catcher) + 4 (version)
/// + 4 (pad to u64 alignment) + 8 (virtuart_base) = 24.
const DESCRIPTOR_BYTES: usize = 24;
/// Offset of `virtuart_base` inside the descriptor under `#[repr(C)]`.
const OFF_VIRTUART_BASE_IN_DESC: usize = 16;

#[derive(Debug, PartialEq, Eq)]
enum DescriptorError {
    EyeCatcherMismatch { offset: usize, got: u8, want: u8 },
    VirtuartBaseUninit,
}

/// Pure-decode half of `probe_warm_resume` for the OpenSBI debug descriptor.
/// `desc` must be the 24-byte volatile-read snapshot of the descriptor.
/// Returns the `virtuart_base` value on success.
fn decode_descriptor(desc: &[u8; DESCRIPTOR_BYTES]) -> Result<u64, DescriptorError> {
    for (i, &expected) in EYE_CATCHER.iter().enumerate() {
        if desc[i] != expected {
            return Err(DescriptorError::EyeCatcherMismatch {
                offset: i,
                got: desc[i],
                want: expected,
            });
        }
    }
    let virtuart_base = u64::from_le_bytes(
        desc[OFF_VIRTUART_BASE_IN_DESC..OFF_VIRTUART_BASE_IN_DESC + 8]
            .try_into()
            .unwrap(),
    );
    if virtuart_base == !0u64 {
        return Err(DescriptorError::VirtuartBaseUninit);
    }
    Ok(virtuart_base)
}

/// Pure-decode half of `probe_warm_resume` for the virt UART magic.
/// `magic_bytes` must be the 8-byte volatile-read snapshot of the word at
/// `virtuart_base + OFF_MAGIC`. Returns `Err(got)` with the decoded u64
/// on mismatch so the caller can log it.
fn decode_magic(magic_bytes: &[u8; 8]) -> Result<(), u64> {
    let magic = u64::from_le_bytes(*magic_bytes);
    if magic == VIRTUAL_UART_MAGIC {
        Ok(())
    } else {
        Err(magic)
    }
}

/// Daemon's long-running per-L2CPU console loop. Reattaches on chip reset
/// (magic mismatch) the same way `console::console_main` does.
pub fn chip_console_main(
    l2cpu: Arc<L2Cpu>,
    hub: Arc<ConsoleHub>,
    input_rx: mpsc::Receiver<u8>,
    exit_flag: Arc<AtomicBool>,
) {
    while !exit_flag.load(Ordering::Relaxed) {
        match uart_pass(&l2cpu, &hub, &input_rx, &exit_flag) {
            Ok(UartExit::Done) => return,
            Ok(UartExit::Retry) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("[console l2cpu {}] error: {} — retrying", l2cpu.idx(), e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a valid descriptor snapshot for `decode_descriptor` tests.
    fn valid_descriptor(virtuart_base: u64) -> [u8; DESCRIPTOR_BYTES] {
        let mut buf = [0u8; DESCRIPTOR_BYTES];
        buf[..8].copy_from_slice(EYE_CATCHER);
        // version at 8..12 is ignored by the decoder; leave zero.
        // padding at 12..16 is ignored.
        buf[OFF_VIRTUART_BASE_IN_DESC..OFF_VIRTUART_BASE_IN_DESC + 8]
            .copy_from_slice(&virtuart_base.to_le_bytes());
        buf
    }

    // The `probe_warm_resume` code reads `DebugDescriptor` as a raw struct
    // via `*const DebugDescriptor`, relying on the `#[repr(C)]` layout
    // matching the OpenSBI firmware-side layout. If the struct ever grows
    // a field or the compiler reorders something, the helper's fixed
    // offsets would silently decode wrong bytes. Pin the layout.
    #[test]
    fn debug_descriptor_has_expected_size_and_virtuart_offset() {
        assert_eq!(std::mem::size_of::<DebugDescriptor>(), DESCRIPTOR_BYTES);
        assert_eq!(
            std::mem::offset_of!(DebugDescriptor, virtuart_base),
            OFF_VIRTUART_BASE_IN_DESC
        );
        assert_eq!(std::mem::offset_of!(DebugDescriptor, eye_catcher), 0);
    }

    #[test]
    fn decode_descriptor_accepts_valid_bytes() {
        let buf = valid_descriptor(0x4000_1234_5678_abc0);
        assert_eq!(decode_descriptor(&buf), Ok(0x4000_1234_5678_abc0));
    }

    #[test]
    fn decode_descriptor_rejects_eye_catcher_at_first_byte() {
        let mut buf = valid_descriptor(0x4000_0000_0000_0000);
        buf[0] = b'X';
        assert_eq!(
            decode_descriptor(&buf),
            Err(DescriptorError::EyeCatcherMismatch {
                offset: 0,
                got: b'X',
                want: b'O',
            })
        );
    }

    #[test]
    fn decode_descriptor_rejects_eye_catcher_at_last_byte() {
        // Ensure the loop covers the full EYE_CATCHER slice, not just the
        // first character.
        let mut buf = valid_descriptor(0x4000_0000_0000_0000);
        buf[7] = 0x00;
        assert_eq!(
            decode_descriptor(&buf),
            Err(DescriptorError::EyeCatcherMismatch {
                offset: 7,
                got: 0x00,
                want: b'g',
            })
        );
    }

    #[test]
    fn decode_descriptor_rejects_all_zero_eye_catcher_at_offset_zero() {
        // A chip that's been reset but never ran OpenSBI will leave all
        // zeros here. We need the first byte (not some later byte) to
        // name the failure so the log points at the real problem.
        let buf = [0u8; DESCRIPTOR_BYTES];
        match decode_descriptor(&buf) {
            Err(DescriptorError::EyeCatcherMismatch { offset: 0, .. }) => {}
            other => panic!("expected EyeCatcherMismatch at offset 0, got {:?}", other),
        }
    }

    #[test]
    fn decode_descriptor_rejects_uninitialized_virtuart_base() {
        // !0u64 is what we observe when OpenSBI cleared the descriptor
        // but hasn't filled the UART pointer yet.
        let buf = valid_descriptor(!0u64);
        assert_eq!(
            decode_descriptor(&buf),
            Err(DescriptorError::VirtuartBaseUninit)
        );
    }

    #[test]
    fn decode_descriptor_accepts_zero_virtuart_base() {
        // Zero is a suspicious but not definitively-invalid address for
        // the decoder to reject — that's the hardware layer's call.
        let buf = valid_descriptor(0);
        assert_eq!(decode_descriptor(&buf), Ok(0));
    }

    #[test]
    fn decode_descriptor_ignores_version_and_padding_bytes() {
        // Fill bytes 8..16 (version + pad) with garbage — decoder must
        // not care.
        let mut buf = valid_descriptor(0x4000_dead_beef_0000);
        for b in buf.iter_mut().take(16).skip(8) {
            *b = 0xff;
        }
        assert_eq!(decode_descriptor(&buf), Ok(0x4000_dead_beef_0000));
    }

    #[test]
    fn decode_magic_accepts_virtuart_bytes() {
        // VIRTUAL_UART_MAGIC is "VIRTUART" as a u64 — the bytes laid out
        // little-endian on the wire are "TRAUTRIV".
        let bytes = VIRTUAL_UART_MAGIC.to_le_bytes();
        assert_eq!(decode_magic(&bytes), Ok(()));
    }

    #[test]
    fn decode_magic_rejects_all_zero() {
        assert_eq!(decode_magic(&[0u8; 8]), Err(0));
    }

    #[test]
    fn decode_magic_rejects_nonzero_mismatch() {
        // Exercise the path where the decoder has to decode a non-zero
        // value (i.e. something actually there but wrong).
        let bogus: u64 = 0xdead_beef_cafe_f00d;
        let bytes = bogus.to_le_bytes();
        assert_eq!(decode_magic(&bytes), Err(bogus));
    }

    #[test]
    fn virtual_uart_magic_constant_matches_ascii_virtuart() {
        // The constant's hex digits spell the ASCII codes for "VIRTUART"
        // when read high-to-low — i.e. it's `u64::from_be_bytes(b"VIRTUART")`.
        // That's how firmware chooses the value. The chip writes this u64 to
        // DRAM natively; on a little-endian host+chip that's 8 bytes in the
        // order "TRAUTRIV", which `from_le_bytes` recovers back to the
        // constant. Lock both readings down so a future refactor that
        // switches to raw byte comparison can pick whichever is handier.
        assert_eq!(u64::from_be_bytes(*b"VIRTUART"), VIRTUAL_UART_MAGIC);
        assert_eq!(u64::from_le_bytes(*b"TRAUTRIV"), VIRTUAL_UART_MAGIC);
    }
}
