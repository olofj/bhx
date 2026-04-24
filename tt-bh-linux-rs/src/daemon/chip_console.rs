// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

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
    let head = read_rx_head(q) % BUFFER_SIZE;
    ptr::write_volatile(q.add(OFF_RX_BUF + head as usize), c);
    atomic::fence(Ordering::Release);
    write_rx_head(q, (read_rx_head(q) + 1) % BUFFER_SIZE);
    true
}
unsafe fn pop_char(q: *mut u8) -> u8 {
    let tail = read_tx_tail(q) % BUFFER_SIZE;
    let c = ptr::read_volatile(q.add(OFF_TX_BUF + tail as usize));
    atomic::fence(Ordering::Release);
    write_tx_tail(q, (read_tx_tail(q) + 1) % BUFFER_SIZE);
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
        let desc_window =
            l2cpu.get_persistent_2m_window(starting_address + debug_ptr as u64)?;
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
            let _ = hub.push_chip_output(&out_buf[..n]);
        }

        // Drain pending input from attached writer client (non-blocking).
        let mut got_input = false;
        loop {
            match input_rx.try_recv() {
                Ok(b) => {
                    got_input = true;
                    // Spin briefly if ring is full — guest SBI will drain it
                    // within a few iterations.
                    let mut spins = 0;
                    while !unsafe { push_char(q, b) } {
                        if exit_flag.load(Ordering::Relaxed) {
                            return Ok(UartExit::Done);
                        }
                        spins += 1;
                        if spins > 10_000 {
                            // Drop a byte rather than block forever.
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Only happens at shutdown.
                    return Ok(UartExit::Done);
                }
            }
        }

        if !got_output && !got_input {
            std::thread::sleep(Duration::from_millis(1));
        }
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

    let desc_window =
        match l2cpu.get_persistent_2m_window(starting_address + debug_ptr as u64) {
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
    let desc = desc_window.get_window() as *const DebugDescriptor;
    for (i, &expected) in EYE_CATCHER.iter().enumerate() {
        let byte = unsafe { ptr::read_volatile(&(*desc).eye_catcher[i]) };
        if byte != expected {
            eprintln!(
                "[probe l2cpu {}] OSBIdbug eye catcher mismatch at byte {} (got 0x{:02x}, want 0x{:02x})",
                l2cpu.idx(),
                i,
                byte,
                expected
            );
            return false;
        }
    }

    let uart_base = unsafe { ptr::read_volatile(&(*desc).virtuart_base) };
    if uart_base == !0u64 {
        eprintln!(
            "[probe l2cpu {}] virtuart_base is ~0 (chip not fully initialized)",
            l2cpu.idx()
        );
        return false;
    }

    let queue_window = match l2cpu.get_persistent_2m_window(uart_base) {
        Ok(w) => w,
        Err(e) => {
            eprintln!(
                "[probe l2cpu {}] queue window failed: {}",
                l2cpu.idx(),
                e
            );
            return false;
        }
    };
    let q = queue_window.get_window();
    let magic = unsafe { read_magic(q) };
    if u64::from_le(magic) != VIRTUAL_UART_MAGIC {
        eprintln!(
            "[probe l2cpu {}] virt UART magic is 0x{:016x} (want 0x{:016x}) — wedged",
            l2cpu.idx(),
            u64::from_le(magic),
            VIRTUAL_UART_MAGIC
        );
        return false;
    }

    eprintln!(
        "[probe l2cpu {}] warm-resume viable (virtuart @ 0x{:x})",
        l2cpu.idx(),
        uart_base
    );
    true
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
                eprintln!(
                    "[console l2cpu {}] error: {} — retrying",
                    l2cpu.idx(),
                    e
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}
