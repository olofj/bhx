// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Virtual UART console — circular buffer communication with OpenSBI on X280.

use std::io::{self, Write};
use std::ptr;
use std::sync::atomic::{self, AtomicBool, Ordering};
use std::sync::Arc;

use crate::l2cpu::L2Cpu;

const BUFFER_SIZE: u32 = 0x1000;
const VIRTUAL_UART_MAGIC: u64 = 0x5649525455415254; // "VIRTUART"

const OPENSBI_DEBUG_PTR: u64 = 0x80;
const EYE_CATCHER: &[u8; 8] = b"OSBIdbug";

/// Field offsets within the Queues structure in device memory.
/// Layout: magic(8) + tx_buf(0x1000) + rx_buf(0x1000) + tx_head(4) + tx_tail(4) + rx_head(4) + rx_tail(4)
const OFF_MAGIC: usize = 0;
const OFF_TX_BUF: usize = 8;
const OFF_RX_BUF: usize = 8 + BUFFER_SIZE as usize;
const OFF_TX_HEAD: usize = 8 + 2 * BUFFER_SIZE as usize;
const OFF_TX_TAIL: usize = OFF_TX_HEAD + 4;
const OFF_RX_HEAD: usize = OFF_TX_TAIL + 4;
const OFF_RX_TAIL: usize = OFF_RX_HEAD + 4;


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

unsafe fn push_char(q: *mut u8, c: u8, exit_flag: &AtomicBool) {
    while !can_push(q) {
        if exit_flag.load(Ordering::Relaxed) { return; }
    }
    // One MMIO read of rx_head; reuse for both the slot index and the
    // next-head write. The old code called `read_rx_head(q)` twice, which
    // is a PCIe round-trip per byte written — and racy in principle if the
    // guest ever advanced its side concurrently (we'd wrap past it).
    let head = read_rx_head(q);
    let slot = (head % BUFFER_SIZE) as usize;
    ptr::write_volatile(q.add(OFF_RX_BUF + slot), c);
    atomic::fence(Ordering::Release);
    write_rx_head(q, (head + 1) % BUFFER_SIZE);
}

unsafe fn pop_char(q: *mut u8) -> Option<u8> {
    // Non-blocking: caller checks can_pop() first
    let tail = read_tx_tail(q);
    let slot = (tail % BUFFER_SIZE) as usize;
    let c = ptr::read_volatile(q.add(OFF_TX_BUF + slot));
    atomic::fence(Ordering::Release);
    write_tx_tail(q, (tail + 1) % BUFFER_SIZE);
    Some(c)
}

/// Debug descriptor struct written by OpenSBI at a known location.
#[repr(C)]
struct DebugDescriptor {
    eye_catcher: [u8; 8],
    version: u32,
    virtuart_base: u64,
}

/// RAII struct that saves/restores terminal settings. When stdin is not a
/// tty (e.g. piped from /dev/null in a test harness), this is a no-op.
pub struct TerminalRawMode {
    orig: Option<nix::sys::termios::Termios>,
}

impl TerminalRawMode {
    pub fn new() -> io::Result<Self> {
        use nix::sys::termios::*;
        let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        if !stdin_is_tty {
            eprintln!("[console] stdin is not a tty; skipping raw-mode setup");
            return Ok(TerminalRawMode { orig: None });
        }
        let orig = tcgetattr(std::io::stdin())
            .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
        let mut raw = orig.clone();

        raw.local_flags &= !(LocalFlags::ECHO
            | LocalFlags::ICANON
            | LocalFlags::ISIG
            | LocalFlags::IEXTEN);
        raw.input_flags &= !(InputFlags::BRKINT
            | InputFlags::INPCK
            | InputFlags::ISTRIP
            | InputFlags::IXON
            | InputFlags::ICRNL);
        raw.output_flags &= !OutputFlags::OPOST;
        raw.control_flags |= ControlFlags::CS8;

        tcsetattr(std::io::stdin(), SetArg::TCSAFLUSH, &raw)
            .map_err(|e| io::Error::from_raw_os_error(e as i32))?;

        Ok(TerminalRawMode { orig: Some(orig) })
    }
}

impl Drop for TerminalRawMode {
    fn drop(&mut self) {
        if let Some(orig) = self.orig.as_ref() {
            let _ = nix::sys::termios::tcsetattr(
                std::io::stdin(),
                nix::sys::termios::SetArg::TCSAFLUSH,
                orig,
            );
        }
    }
}

/// Run the UART console loop. Returns Ok(()) on clean exit, Err on failure.
fn uart_loop(l2cpu: &L2Cpu, exit_flag: &AtomicBool) -> io::Result<i32> {
    let starting_address = l2cpu.starting_address();
    let tile = l2cpu.coordinates();

    // 1. Read debug descriptor pointer
    let debug_ptr = l2cpu.read32(starting_address + OPENSBI_DEBUG_PTR);
    eprintln!(
        "L2CPU[{}, {}] debug descriptor: {:x}",
        tile.x, tile.y, debug_ptr
    );

    // 2. Open window to debug descriptor and verify eye catcher.
    //    Read uart_base, then drop the window to free the TLB resource
    //    before opening the UART queue window.
    let uart_base = {
        let desc_window =
            l2cpu.get_persistent_2m_window(starting_address + debug_ptr as u64)?;
        let desc = desc_window.get_window() as *const DebugDescriptor;

        for (i, &expected) in EYE_CATCHER.iter().enumerate() {
            let byte = unsafe { ptr::read_volatile(&(*desc).eye_catcher[i]) };
            if byte != expected {
                eprintln!(
                    "L2CPU[{}, {}] debug descriptor eye catcher mismatch",
                    tile.x, tile.y
                );
                return Ok(1);
            }
        }

        let base = unsafe { ptr::read_volatile(&(*desc).virtuart_base) };
        if base == !0u64 {
            eprintln!(
                "L2CPU[{}, {}] failed to find the virtual UART; exiting",
                tile.x, tile.y
            );
            return Ok(1);
        }
        base
        // desc_window dropped here, freeing the TLB
    };
    eprintln!(
        "L2CPU[{}, {}] found the virtual UART at 0x{:x}",
        tile.x, tile.y, uart_base
    );

    // 3. Open window to UART circular buffer
    let queue_window = l2cpu.get_persistent_2m_window(uart_base)?;
    let q = queue_window.get_window();

    let _raw_mode = TerminalRawMode::new()?;
    let mut ctrl_a_pressed = false;

    while !exit_flag.load(Ordering::Relaxed) {
        // Check magic
        let magic = unsafe { read_magic(q) };
        if u64::from_le(magic) != VIRTUAL_UART_MAGIC {
            return Ok(-libc::EAGAIN);
        }

        // Check for input from terminal using select with 1µs timeout
        let mut rfds = unsafe { std::mem::zeroed::<libc::fd_set>() };
        unsafe { libc::FD_SET(libc::STDIN_FILENO, &mut rfds); }
        let mut tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 1,
        };
        let retval = unsafe {
            libc::select(
                libc::STDIN_FILENO + 1,
                &mut rfds,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut tv,
            )
        };

        if retval > 0 {
            let mut input = [0u8; 1];
            // Use raw libc::read instead of Rust's buffered io::stdin().read()
            // to avoid the internal buffer consuming bytes that select() won't
            // see on subsequent iterations (causing dropped keystrokes).
            let n = unsafe { libc::read(libc::STDIN_FILENO, input.as_mut_ptr() as *mut libc::c_void, 1) };
            if n > 0 {
                if ctrl_a_pressed {
                    ctrl_a_pressed = false;
                    if input[0] == b'x' {
                        let _ = io::stdout().write_all(b"\n\n");
                        return Ok(0);
                    }
                    // Forward both the Ctrl-A and the character to the device
                    unsafe {
                        push_char(q, 1, exit_flag); // Ctrl-A
                        push_char(q, input[0], exit_flag);
                    }
                } else if input[0] == 1 {
                    // Ctrl-A
                    ctrl_a_pressed = true;
                } else {
                    unsafe { push_char(q, input[0], exit_flag); }
                }
            }
        }

        // Check for output from device
        if unsafe { can_pop(q) } {
            if let Some(c) = unsafe { pop_char(q) } {
                // Write raw byte directly — print!("{}", c as char) would corrupt
                // bytes > 127 by expanding them into multi-byte UTF-8 sequences.
                let _ = io::stdout().write_all(&[c]);
                let _ = io::stdout().flush();
            }
        }
    }

    Ok(0)
}

/// Console thread main function. Retries on EAGAIN (chip reset).
///
/// The `L2Cpu` is shared with the disk / network / cloud-init workers so we
/// don't burn a pair of 4 GB TLB windows per worker. If the chip is reset
/// mid-run the shared TLB windows become stale and all workers have to bail
/// out together — the host-level recovery is a fresh `connect` (or, in the
/// daemon model, a fresh `daemon start`).
pub fn console_main(l2cpu: Arc<L2Cpu>, exit_flag: Arc<AtomicBool>) {
    eprintln!("Press Ctrl-A x to exit.\n");
    while !exit_flag.load(Ordering::Relaxed) {
        match uart_loop(&l2cpu, &exit_flag) {
            Ok(r) if r == -libc::EAGAIN => {
                eprintln!("Error (UART vanished) -- was the chip reset?  Retrying...");
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Ok(_) => {
                exit_flag.store(true, Ordering::Relaxed);
                return;
            }
            Err(e) => {
                eprintln!("Error ({}) -- was the chip reset?  Retrying...", e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
}
