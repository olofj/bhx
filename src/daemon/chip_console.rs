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

// The constant's hex digits spell the ASCII codes for "VIRTUART" when
// read high-to-low — i.e. it's `u64::from_be_bytes(b"VIRTUART")`.
// That's how firmware chooses the value. The chip writes this u64 to
// DRAM natively; on a little-endian host+chip that's 8 bytes in the
// order "TRAUTRIV", which `from_le_bytes` recovers back to the
// constant. Lock both readings down so a future refactor that switches
// to raw byte comparison can pick whichever is handier.
const _VIRTUAL_UART_MAGIC_PINNED: () = {
    assert!(VIRTUAL_UART_MAGIC == u64::from_be_bytes(*b"VIRTUART"));
    assert!(VIRTUAL_UART_MAGIC == u64::from_le_bytes(*b"TRAUTRIV"));
};

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
/// Cross-domain write of the consumer index. Only safe when the chip
/// side isn't reading — i.e. when the guest is parked in OpenSBI's
/// `sbi_hsm_hart_wait` and no kernel is running. Used by
/// [`drain_rx_on_park`] to throw away stale bytes that the dying
/// kernel didn't consume so they don't surface as commands at U-Boot
/// on the next release-from-purgatory wake.
unsafe fn write_rx_tail(q: *mut u8, val: u32) {
    ptr::write_volatile(q.add(OFF_RX_TAIL) as *mut u32, val);
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
    state: &std::sync::Arc<crate::daemon::DaemonState>,
    l2cpu_idx: u8,
) -> std::io::Result<UartExit> {
    let starting_address = l2cpu.starting_address();
    let tile = l2cpu.coordinates();

    let debug_ptr = l2cpu.read32(starting_address + OPENSBI_DEBUG_PTR)?;
    let uart_base = {
        let desc_window = l2cpu.get_persistent_2m_window(starting_address + debug_ptr as u64)?;
        let desc = desc_window.get_window() as *const DebugDescriptor;
        for (i, &expected) in EYE_CATCHER.iter().enumerate() {
            let byte = unsafe { ptr::read_volatile(&(*desc).eye_catcher[i]) };
            if byte != expected {
                crate::dlog!(
                    "[console l2cpu {}] debug descriptor eye catcher mismatch",
                    l2cpu.idx()
                );
                return Ok(UartExit::Retry);
            }
        }
        let base = unsafe { ptr::read_volatile(&(*desc).virtuart_base) };
        if base == !0u64 {
            crate::dlog!(
                "[console l2cpu {}] virtuart_base is ~0; chip not ready",
                l2cpu.idx()
            );
            return Ok(UartExit::Retry);
        }
        base
    };
    crate::dlog!(
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
    /// Cadence for re-reading the OpenSBI bhx-purgatory status cell.
    /// 1.5 s is plenty for `bhx connect` to exit a beat after the
    /// guest's `poweroff` finishes printing — operator-visible
    /// latency, not interrupt-handler latency.
    const PARKED_PROBE_INTERVAL: Duration = Duration::from_millis(1500);
    let mut last_active = std::time::Instant::now();
    let purg_pa = starting_address + crate::regs::purgatory::STATUS_OFFSET;
    let mut last_parked = false;
    let mut last_parked_check = std::time::Instant::now()
        .checked_sub(PARKED_PROBE_INTERVAL)
        .unwrap_or_else(std::time::Instant::now);

    loop {
        if exit_flag.load(Ordering::Relaxed) {
            return Ok(UartExit::Done);
        }

        let magic = unsafe { read_magic(q) };
        if u64::from_le(magic) != VIRTUAL_UART_MAGIC {
            return Ok(UartExit::Retry);
        }

        // (#166) Notice when the guest has powered off and the slot
        // has transitioned to Parked. The slot stays alive on the
        // daemon side (so `bhx boot` can release-from-purgatory),
        // but any attached `bhx connect` clients are now stranded —
        // the chip is silent. Send them a goodbye on the false→true
        // transition so they see EOF and exit cleanly.
        if last_parked_check.elapsed() >= PARKED_PROBE_INTERVAL {
            last_parked_check = std::time::Instant::now();
            let parked = read_parked_state(l2cpu, purg_pa);
            if detect_park_transition(last_parked, parked) {
                // (#177) Discriminate the guest's intent via the SRST
                // type the bhx-purgatory hook stashed in the status
                // block. SHUTDOWN (poweroff/init 0) → tear down,
                // disconnect, scrub. REBOOT (reboot/init 6) →
                // preserve attachments + clients + scrollback, brief
                // pause, fire release-from-purgatory automatically
                // so the operator's reboot is bug-for-bug
                // indistinguishable from a real hardware reboot.
                let reset_type = read_parked_reset_type(l2cpu, starting_address);
                if reset_type == crate::regs::purgatory::RESET_TYPE_COLD_REBOOT
                    || reset_type == crate::regs::purgatory::RESET_TYPE_WARM_REBOOT
                {
                    // Operator-visible status line so the reboot
                    // doesn't appear as silent dead-air. Broadcast
                    // via push_chip_output (NOT disconnect_all_with_reason
                    // — we want the clients to stay attached and ride
                    // through the reboot). Scrollback gets the line
                    // too, so post-mortem `bhx connect` sees it.
                    let reason = format!(
                        "\r\n[bhx: l2cpu {} rebooting (auto-release)…]\r\n",
                        l2cpu.idx()
                    );
                    let _ = hub.push_chip_output(reason.as_bytes());

                    // Drain BOTH input channels even though we're not
                    // disconnecting. Two sources of stale bytes:
                    //   - mpsc: terminal CPR responses to the kernel's
                    //     shutdown-time `\x1b[6n` queries (the
                    //     operator's terminal auto-answers; they
                    //     arrive after the kernel has stopped
                    //     reading), plus any pre-reboot keystrokes
                    //     the kernel's TTY didn't fully consume
                    //     before shutdown.
                    //   - chip RX ring: bytes we pushed to the ring
                    //     during shutdown that the kernel never
                    //     read.
                    // Without draining these, U-Boot's "Hit any key
                    // to stop autoboot" reads them as keystrokes,
                    // interrupts autoboot, drops to the `=>` prompt
                    // — exactly the issue reported on the first
                    // auto-reboot test (terminal CPR `\x1b[97;1R`
                    // bytes parsed by U-Boot as commands like
                    // `[97`, `1R`, `428R`).
                    let mpsc_dropped = drain_input_channel(input_rx);
                    let ring_dropped = unsafe { drain_rx_ring(q) };
                    if mpsc_dropped > 0 || ring_dropped > 0 {
                        crate::dlog!(
                            "[console l2cpu {}] reboot: dropped {} byte(s) from input mpsc, {} byte(s) from chip-side RX ring",
                            l2cpu.idx(),
                            mpsc_dropped,
                            ring_dropped
                        );
                    }

                    // (#177) Drop the slot's net worker NOW (under
                    // slot lock, with dispatcher unregister + worker
                    // join) so the slirp NAT / DHCP / port-forward
                    // state from the pre-reboot session doesn't
                    // bleed into the next boot. Mid-reset libslirp
                    // state has caused real fallout: `networkd-
                    // wait-online` hangs on the post-reboot kernel
                    // because slirp answers a fresh DHCPDISCOVER
                    // with state from the dying VM. Disks +
                    // virtio-console + rng are unaffected by host-
                    // side state and stay attached. The net is
                    // re-attached just before the wake IPI fires,
                    // below, so the new boot's virtio-net probe
                    // lands on a registered RegEntry.
                    let cached_extra_fwd = drop_net_for_reboot(state, l2cpu_idx);

                    // Brief pause: gives the operator's terminal a
                    // visible "guest is rebooting" moment instead of
                    // the U-Boot banner appearing instantly mid-line
                    // after `reboot`. The chip is in
                    // `sbi_hsm_hart_wait` throughout, so pausing the
                    // polling thread here has no observable cost on
                    // chip-side progress.
                    std::thread::sleep(Duration::from_secs(1));

                    // Second drain right before release — catches
                    // anything the operator typed during the pause
                    // (intended for the next boot's userspace but
                    // we can't tell yet; absorb to keep U-Boot's
                    // autoboot countdown clean). Same rationale as
                    // the first drain.
                    let late_mpsc = drain_input_channel(input_rx);
                    let late_ring = unsafe { drain_rx_ring(q) };
                    if late_mpsc > 0 || late_ring > 0 {
                        crate::dlog!(
                            "[console l2cpu {}] reboot pause: late drop {} mpsc + {} ring byte(s)",
                            l2cpu.idx(),
                            late_mpsc,
                            late_ring
                        );
                    }

                    // Re-attach net with the cached extra_fwd, BEFORE
                    // firing the wake IPI. The dispatcher needs a
                    // fresh RegEntry for the net slot in place by
                    // the time the new kernel's virtio-net probe
                    // walks DRIVER_OK; firing the wake first would
                    // race the kernel against the daemon's slirp
                    // setup. `cached_extra_fwd` may be empty if no
                    // net was ever attached or if drop_net_for_reboot
                    // observed the slot in an unexpected state.
                    if let Some(extra) = cached_extra_fwd {
                        if let Err(e) = reattach_net_for_reboot(state, l2cpu_idx, &extra) {
                            crate::dlog!(
                                "[console l2cpu {}] reboot: net re-attach failed: {} — new boot will come up without virtio-net",
                                l2cpu.idx(),
                                e
                            );
                        } else {
                            crate::dlog!(
                                "[console l2cpu {}] reboot: net re-attached with fresh slirp state (extra_fwd={:?})",
                                l2cpu.idx(),
                                extra
                            );
                        }
                    }

                    if let Err(e) = auto_release_for_reboot(state, l2cpu_idx) {
                        crate::dlog!(
                            "[console l2cpu {}] auto-reboot failed: {} — operator can still `bhx boot -l {}` manually",
                            l2cpu.idx(),
                            e,
                            l2cpu.idx()
                        );
                        let fallback = format!(
                            "[bhx: auto-reboot failed: {}; run `bhx boot -l {}` to release]\r\n",
                            e,
                            l2cpu.idx()
                        );
                        let _ = hub.push_chip_output(fallback.as_bytes());
                    } else {
                        crate::dlog!(
                            "[console l2cpu {}] auto-reboot fired (reset_type={}); clients + scrollback preserved",
                            l2cpu.idx(),
                            reset_type
                        );
                    }
                } else {
                    // SHUTDOWN (or unknown type — defaults to the
                    // safe shape that releases resources).
                    drop_workers_on_shutdown_park(state, l2cpu_idx);
                    hub.disconnect_all_with_reason(&format!(
                        "l2cpu {} parked (guest powered off); `bhx boot -l {}` to release",
                        l2cpu.idx(),
                        l2cpu.idx()
                    ));
                    // Throw away the hub's scrollback. Bytes from the
                    // dead kernel aren't useful to a future re-attach,
                    // and stale `\x1b[6n` queries embedded in them would
                    // re-prompt the operator's terminal for a CPR
                    // response that the writer pump then forwards to
                    // U-Boot's interactive prompt — the same
                    // autoboot-interrupted-by-stale-input symptom the
                    // chip-side ring drain (below) addresses, but on
                    // the host side via terminal-replied control
                    // sequences. Operators who want a post-mortem of
                    // what the previous kernel printed can use the tail
                    // captured by `internal_stop` (#160) on a Stopped
                    // slot.
                    hub.clear_scrollback();
                    // Drain any input the kernel didn't consume before
                    // it poweroff'd: bytes still in the daemon-side
                    // mpsc channel from the writer client, plus bytes
                    // already pushed into the chip-side virtuart RX
                    // ring. Without this, U-Boot reads them on the
                    // next release-from-purgatory wake and treats them
                    // as commands at the `=>` prompt, interrupting
                    // autoboot. Cross-domain write of `rx_tail` is
                    // safe here because the guest is in OpenSBI's
                    // hsm_hart_wait — no kernel is reading from this
                    // ring.
                    let mpsc_dropped = drain_input_channel(input_rx);
                    let ring_dropped = unsafe { drain_rx_ring(q) };
                    if mpsc_dropped > 0 || ring_dropped > 0 {
                        crate::dlog!(
                            "[console l2cpu {}] parked: dropped {} byte(s) from input mpsc, {} byte(s) from chip-side RX ring",
                            l2cpu.idx(),
                            mpsc_dropped,
                            ring_dropped
                        );
                    }
                }
            }
            last_parked = parked;
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
    let debug_ptr = match l2cpu.read32(starting_address + OPENSBI_DEBUG_PTR) {
        Ok(v) => v,
        Err(e) => {
            crate::dlog!(
                "[probe l2cpu {}] read OPENSBI_DEBUG_PTR failed: {} — wedged",
                l2cpu.idx(),
                e
            );
            return false;
        }
    };

    let desc_window = match l2cpu.get_persistent_2m_window(starting_address + debug_ptr as u64) {
        Ok(w) => w,
        Err(e) => {
            crate::dlog!(
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
            crate::dlog!(
                "[probe l2cpu {}] OSBIdbug eye catcher mismatch at byte {} (got 0x{:02x}, want 0x{:02x})",
                l2cpu.idx(),
                offset,
                got,
                want
            );
            return false;
        }
        Err(DescriptorError::VirtuartBaseUninit) => {
            crate::dlog!(
                "[probe l2cpu {}] virtuart_base is ~0 (chip not fully initialized)",
                l2cpu.idx()
            );
            return false;
        }
    };

    let queue_window = match l2cpu.get_persistent_2m_window(uart_base) {
        Ok(w) => w,
        Err(e) => {
            crate::dlog!("[probe l2cpu {}] queue window failed: {}", l2cpu.idx(), e);
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
            crate::dlog!(
                "[probe l2cpu {}] warm-resume viable (virtuart @ 0x{:x})",
                l2cpu.idx(),
                uart_base
            );
            true
        }
        Err(got) => {
            crate::dlog!(
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

/// Read the OpenSBI bhx-purgatory status cell and return whether it
/// holds the PARKED magic. Read failures are treated as "not parked"
/// — same defensive default as `classify_sibling` in the boot path:
/// if we can't read the cell we don't claim a transition happened.
fn read_parked_state(l2cpu: &L2Cpu, purg_pa: u64) -> bool {
    let lo = match l2cpu.read32(purg_pa) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let hi = match l2cpu.read32(purg_pa + 4) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let combined = ((hi as u64) << 32) | (lo as u64);
    combined == crate::regs::purgatory::STATUS_PARKED
}

/// Compute whether to fire the "guest powered off" disconnect on this
/// purgatory probe. Fires only on the false→true edge so a fully-
/// parked slot whose pump runs many iterations without intervening
/// transitions doesn't spam goodbyes at any client that re-attaches.
pub(crate) fn detect_park_transition(last_parked: bool, current_parked: bool) -> bool {
    !last_parked && current_parked
}

/// (#177) Read the SBI SRST reset_type the purgatory hook stashed at
/// `RESET_TYPE_OFFSET`. Only meaningful when the PARKED magic is set;
/// caller should check that first. Returns `RESET_TYPE_SHUTDOWN` on
/// read failure — the safe default that releases pinned resources
/// rather than the more leaky "keep them around."
fn read_parked_reset_type(l2cpu: &L2Cpu, mem_start: u64) -> u32 {
    let pa = mem_start + crate::regs::purgatory::RESET_TYPE_OFFSET;
    // u64 wire field; low 32 bits carry the enum.
    match l2cpu.read32(pa) {
        Ok(v) => v,
        Err(_) => crate::regs::purgatory::RESET_TYPE_SHUTDOWN,
    }
}

/// (#177) Drop the slot's net worker on a REBOOT-typed park, the
/// same way `dispatch_remove_net` does it for an explicit operator
/// RPC. Returns the slot's cached `net_extra_fwd` so the caller can
/// re-attach with the same port shape — or `None` if there was no
/// net attached / the slot is gone / boot_payload is missing (warm-
/// resumed; can't recreate the original shape).
///
/// Reason for dropping mid-reboot: libslirp's NAT state from the
/// pre-reboot session would otherwise answer the new kernel's
/// DHCPDISCOVER with stale lease info, leaving systemd-networkd-
/// wait-online stuck for minutes on the new boot. The drop +
/// re-attach pattern in [`reattach_net_for_reboot`] gives the new
/// boot a clean slirp instance.
fn drop_net_for_reboot(
    state: &std::sync::Arc<crate::daemon::DaemonState>,
    l2cpu_idx: u8,
) -> Option<Vec<(u16, u16)>> {
    let net = {
        let mut g = state.l2cpus[l2cpu_idx as usize].lock().ok()?;
        let slot = g.as_mut()?;
        // Only operate on slots that have a cached payload — warm-
        // resumed slots don't have the cold-boot RPC context, and
        // re-attach would have nothing meaningful to use.
        slot.boot_payload.as_ref()?;
        let n = slot.net.take()?;
        let extra = slot.net_extra_fwd.clone();
        if let Ok(disp) = state.dispatcher.lock() {
            if let Some(d) = disp.as_ref() {
                let slot_idx = (l2cpu_idx as u32) * crate::virtio_engine::DEVS_PER_L2CPU
                    + crate::virtio_engine::DEV_NET;
                d.unregister_slot(slot_idx);
            }
        }
        (n, extra)
    };
    let (worker, extra_fwd) = net;
    worker.stop_and_join();
    Some(extra_fwd)
}

/// (#177) Re-attach net to the slot after `drop_net_for_reboot`
/// took it down. Mirrors `dispatch_add_net`'s engine path — fresh
/// `VirtioNet` with the supplied forwards, register with the
/// dispatcher, push a stub `WorkerHandle` so `daemon status`
/// shows `net=y` again.
///
/// Uses the throwaway-socketpair trick to call `dispatch_add_net`
/// directly so all the pre-flight checks (port availability, etc.)
/// run consistently with the hot-add path.
fn reattach_net_for_reboot(
    state: &std::sync::Arc<crate::daemon::DaemonState>,
    l2cpu_idx: u8,
    extra_fwd: &[(u16, u16)],
) -> Result<(), String> {
    use std::os::unix::net::UnixStream;
    let (throwaway, _drop) =
        UnixStream::pair().map_err(|e| format!("socketpair for reply: {}", e))?;
    crate::daemon::server::dispatch_add_net(&throwaway, state, l2cpu_idx, None, extra_fwd.to_vec())
        .map_err(|e| format!("dispatch_add_net: {}", e))
}

/// (#177) On a Running → Parked transition with
/// `reset_type = COLD_REBOOT | WARM_REBOOT`, fire
/// `dispatch_release` against the parked hart without an operator
/// RPC. Uses the slot's cached `boot_payload` (kernel/U-Boot path)
/// and `dtb_bytes` to re-image; passes `disk=None, cloud_init=None,
/// network=false` so `dispatch_release`'s swap-on-RPC-args path
/// stays dormant — the existing attachments survive across the
/// reboot. Result: guest's `reboot` lands back in userspace ~1 s
/// later, attached `bhx connect` clients ride through (no
/// disconnect, no scrollback wipe), `daemon status` never reports
/// Stopped/Parked.
///
/// Failures (warm-resumed slot with no cached payload, slot mutex
/// poisoned, chip read failure, etc.) propagate up so the caller
/// can log; the slot falls back to the pre-#177 "operator must
/// `bhx boot -l N` manually" behavior in that case.
fn auto_release_for_reboot(
    state: &std::sync::Arc<crate::daemon::DaemonState>,
    l2cpu_idx: u8,
) -> Result<(), String> {
    use std::os::unix::net::UnixStream;

    // Read parked release metadata directly from the chip — same
    // call dispatch_boot uses on the Parked-slot branch. Returns
    // Ok(None) if the slot isn't actually parked (e.g. operator
    // raced us with their own `bhx boot`), which we treat as
    // "nothing to do."
    let meta = match crate::daemon::server::read_parked_release_meta(state, l2cpu_idx) {
        Ok(Some(m)) => m,
        Ok(None) => return Err("slot not parked when auto-reboot fired".into()),
        Err(e) => return Err(format!("read_parked_release_meta: {}", e)),
    };

    // Snapshot the cached payload + dtb_bytes from the slot. Warm-
    // resumed slots have boot_payload=None; bail with a clear error
    // so the chip_console caller logs the "manual boot needed" hint.
    let (payload, dtb_bytes) = {
        let g = state.l2cpus[l2cpu_idx as usize]
            .lock()
            .map_err(|_| "slot mutex poisoned".to_string())?;
        let slot = g.as_ref().ok_or_else(|| "slot is gone".to_string())?;
        let p = slot.boot_payload.clone().ok_or_else(|| {
            "no cached boot_payload (warm-resumed slot has no cold-boot args)".to_string()
        })?;
        (p, slot.dtb_bytes.clone())
    };

    // Throwaway socketpair end catches `dispatch_release`'s
    // reply_ok — the function writes `Response::Ok` via
    // `let _ = write_frame(...)`, so a write to a dead half is
    // swallowed silently. Same trick `swap_attached_for_release`
    // uses for `dispatch_add_disk` / `dispatch_add_net`.
    let (throwaway, _drop) =
        UnixStream::pair().map_err(|e| format!("socketpair for reply: {}", e))?;

    // Call dispatch_release with no swap args (all None/false) so
    // the existing attachments are preserved. dispatch_release
    // handles the kernel + DTB re-image, then fires the wake IPI.
    // All five swap flags None/false so dispatch_release stays on
    // the fast-resume path. The slot's existing disks/vconsole/rng
    // attachments survive across the reboot — only net was dropped
    // (by drop_net_for_reboot) and re-attached (by
    // reattach_net_for_reboot) above, both outside this call.
    crate::daemon::server::dispatch_release(
        state,
        &throwaway,
        l2cpu_idx,
        &payload,
        meta,
        dtb_bytes,
        None,
        None,
        false,
        &[],
        false,
        false,
    )
    .map_err(|e| format!("dispatch_release: {}", e))?;
    Ok(())
}

/// (#177) On a Running → Parked transition with `reset_type =
/// SHUTDOWN`, drop the slot's per-device workers: disks, net,
/// virtio-console, virtio-rng. The guest is done; holding the .img
/// files and slirp ports open would pin host-side resources, and
/// keeping the virtio-mmio device descriptors registered makes
/// `daemon status` misleadingly report them as live.
///
/// The slot itself stays alive — `bhx connect` still attaches
/// post-park to see the goodbye line via the chip-side virtUART
/// (separate from the virtio-mmio console device dropped here), and
/// a subsequent `bhx boot` re-attaches fresh resources via
/// `dispatch_release`'s RPC-arg-aware path.
///
/// REBOOT-typed parks return early without dropping anything (the
/// guest is about to come right back; the existing fast-resume path
/// expects everything still attached).
fn drop_workers_on_shutdown_park(
    state: &std::sync::Arc<crate::daemon::DaemonState>,
    l2cpu_idx: u8,
) {
    use crate::daemon::{DiskWorker, VirtioConsoleSlot, WorkerHandle};

    // Take all per-device workers out under the slot lock; unregister
    // their dispatcher entries while still holding the lock (matches
    // the shape dispatch_remove_disk / dispatch_remove_net /
    // dispatch_remove_console use). Then release the lock before
    // joining so we don't block other RPCs for the join window.
    let (disks, net, vconsole, rng): (
        Vec<DiskWorker>,
        Option<WorkerHandle>,
        Option<VirtioConsoleSlot>,
        Option<WorkerHandle>,
    ) = {
        let mut g = match state.l2cpus[l2cpu_idx as usize].lock() {
            Ok(g) => g,
            Err(_) => {
                crate::dlog!(
                    "[console l2cpu {}] shutdown-park: slot mutex poisoned; skipping worker drop",
                    l2cpu_idx
                );
                return;
            }
        };
        let slot = match g.as_mut() {
            Some(s) => s,
            None => return,
        };
        let disks = std::mem::take(&mut slot.disks);
        let net = slot.net.take();
        let vconsole = slot.virtio_console.take();
        let rng = slot.virtio_rng.take();
        if let Ok(disp) = state.dispatcher.lock() {
            if let Some(d) = disp.as_ref() {
                let base = (l2cpu_idx as u32) * crate::virtio_engine::DEVS_PER_L2CPU;
                for w in &disks {
                    d.unregister_slot(w.slot_idx);
                }
                if net.is_some() {
                    d.unregister_slot(base + crate::virtio_engine::DEV_NET);
                }
                if vconsole.is_some() {
                    d.unregister_slot(base + crate::virtio_engine::DEV_CONSOLE);
                }
                if rng.is_some() {
                    d.unregister_slot(base + crate::virtio_engine::DEV_RNG);
                }
            }
        }
        (disks, net, vconsole, rng)
    };

    let dropped_disks = disks.len();
    let had_net = net.is_some();
    let had_vconsole = vconsole.is_some();
    let had_rng = rng.is_some();
    for d in disks {
        d.worker.stop_and_join();
    }
    if let Some(n) = net {
        n.stop_and_join();
    }
    if let Some(vc) = vconsole {
        vc.worker.stop_and_join();
    }
    if let Some(r) = rng {
        r.stop_and_join();
    }
    if dropped_disks > 0 || had_net || had_vconsole || had_rng {
        crate::dlog!(
            "[console l2cpu {}] shutdown-park: dropped {} disk(s), net={}, vconsole={}, rng={} \
             (guest issued SBI_SRST_RESET_TYPE_SHUTDOWN — #177)",
            l2cpu_idx,
            dropped_disks,
            had_net,
            had_vconsole,
            had_rng
        );
    }
}

/// Pull every queued byte out of the writer-side mpsc channel and
/// drop them. Returns the count of bytes dropped. Called on the
/// Parked transition so client keystrokes the kernel didn't consume
/// don't end up shoved into the chip-side ring during the parked
/// window — they'd surface as commands at U-Boot's `=>` prompt on
/// the next release-from-purgatory wake.
fn drain_input_channel(input_rx: &mpsc::Receiver<u8>) -> usize {
    let mut n = 0usize;
    while input_rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

/// Cross-domain reset of the chip-side virtuart RX ring: write the
/// guest-owned `rx_tail` to match the daemon-owned `rx_head`,
/// effectively "the guest has consumed everything." Safe only when
/// no guest code is reading the ring — i.e. the guest is parked in
/// OpenSBI's hsm_hart_wait. Returns the byte count we threw away
/// (head - tail, modulo BUFFER_SIZE).
///
/// # Safety
/// Caller must hold a valid `q` pointer to a virtuart whose magic
/// has been validated, and must guarantee no other code path is
/// concurrently writing `rx_head` (the daemon's own pump is the only
/// other writer; this function is called from the same pump thread).
unsafe fn drain_rx_ring(q: *mut u8) -> u32 {
    let head = read_rx_head(q);
    let tail = read_rx_tail(q);
    let pending = head.wrapping_sub(tail) % BUFFER_SIZE;
    write_rx_tail(q, head);
    pending
}

/// Daemon's long-running per-L2CPU console loop. Reattaches on chip reset
/// (magic mismatch) the same way `console::console_main` does.
pub fn chip_console_main(
    l2cpu: Arc<L2Cpu>,
    hub: Arc<ConsoleHub>,
    input_rx: mpsc::Receiver<u8>,
    exit_flag: Arc<AtomicBool>,
    state: Arc<crate::daemon::DaemonState>,
    l2cpu_idx: u8,
) {
    while !exit_flag.load(Ordering::Relaxed) {
        match uart_pass(&l2cpu, &hub, &input_rx, &exit_flag, &state, l2cpu_idx) {
            Ok(UartExit::Done) => return,
            Ok(UartExit::Retry) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                crate::dlog!("[console l2cpu {}] error: {} — retrying", l2cpu.idx(), e);
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

    // ---- detect_park_transition ----

    #[test]
    fn park_transition_fires_on_false_to_true_edge() {
        assert!(detect_park_transition(false, true));
    }

    #[test]
    fn park_transition_does_not_fire_when_still_parked() {
        // Each pump iteration after the initial transition keeps
        // observing PARKED. We must not re-fire — already-attached
        // clients are gone, and a NEW attach (post-park) would get
        // hit on every probe interval.
        assert!(!detect_park_transition(true, true));
    }

    #[test]
    fn park_transition_does_not_fire_when_steady_running() {
        assert!(!detect_park_transition(false, false));
    }

    #[test]
    fn park_transition_does_not_fire_on_parked_to_running_edge() {
        // The Parked → Running transition (host released the slot
        // via `bhx boot`) just resets last_parked for future
        // detections. No goodbye, no disconnect — there's nothing
        // attached to disconnect from anyway.
        assert!(!detect_park_transition(true, false));
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
}
