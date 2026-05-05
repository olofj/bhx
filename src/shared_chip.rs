// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Daemon-owned shared chip access.
//!
//! The chip has register blocks that are **genuinely shared across all L2CPUs**
//! — the PLL at `0x80020500+`, the reset unit at `0x80030000+`, and other
//! chip-wide control registers at `0x8000_xxxx` all live on NOC tile `(8, 0)`,
//! the ARC tile + reset unit. If multiple independent fds each configure
//! their own TLB window onto tile (8,0) at those addresses, concurrent writes
//! race at the hardware level: read-modify-write on `L2CPU_RESET` tears, PLL
//! step sequences interleave, and we've observed the host dying as a result
//! (see <https://github.com/olofj/bhx/issues/1>).
//!
//! `SharedChip` fixes the structural part by owning the **one and only** TLB
//! window to tile (8,0) per card for the daemon's lifetime, with an internal
//! mutex that serializes multi-step sequences (PLL step-down/step-up, reset
//! R-M-W). A daemon-wide `Arc<SharedChip>` replaces the per-boot ephemeral
//! tile-(8,0) mappings that used to alias the same registers.
//!
//! ## PCIe reset handling
//!
//! `chip::reset_board()` does a PCIe LDS reset. The re-enumeration invalidates
//! any fd held across the call — our persistent fd would start returning
//! `ENODEV`. `SharedChip::reset_board()` therefore drops the current
//! `fd + window` before issuing the reset and reopens a fresh pair after.
//! Callers never see the rotation; they acquire a read guard on the inner
//! state and find a valid `Inner` on the other side.
//!
//! ## Scope
//!
//! All tile-(8,0) access in the daemon goes through this type: the startup
//! probe, `l2cpu_is_running`, `reset_x280`, `reset_board`, and the debug
//! CLI's register pokes. Per-L2CPU NOC traffic (OpenSBI / kernel / DTB
//! image loads, L3 / L2 prefetch config, reset vectors) goes through the
//! per-L2CPU `L2Cpu` fd instead, so 4-way parallel cold boots no longer
//! share a single bus-access point.

use std::io;
use std::mem::ManuallyDrop;
use std::os::unix::io::RawFd;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::chip;
use crate::clock::{self, PllAccess};
use crate::kmd;
use crate::tlb::TlbWindow;

/// ARC tile + reset unit on Blackhole — the chip-wide tile shared
/// across all L2CPUs. (Earlier comments called this the "AXI tile";
/// that was an artifact of the syseng.git lift-over and is wrong —
/// (8,0) is the ARC tile.)
const ARC_TILE_X: u16 = 8;
const ARC_TILE_Y: u16 = 0;

/// Base of the 2 MiB TLB window that covers the chip-wide control
/// registers — the reset unit (`0x80030000+`), PLL control
/// (`0x80020500+`), and all other `0x8000_xxxx` registers — all on
/// the ARC tile (8,0).
const ARC_RESET_WINDOW_BASE: u64 = 0x8000_0000;
const ARC_RESET_WINDOW_SIZE: u64 = 0x20_0000;

/// `L2CPU_RESET` lives in the reset unit at this address.
const L2CPU_RESET_ADDR: u64 = 0x8003_0014;

/// `arc_ss.reset_unit.SCRATCH_RAM[2]` — ARC firmware writes the
/// `boot_status_0` word here as it progresses through init. Bits 1..2
/// encode init status (0=NotStarted, 1=Started, 2=Done, 3=Error); used
/// by `wait_arc_fw_ready_inner` to gate the post-reset path on ARC FW
/// finishing GDDR PHY power-up + DRAM training. Mirrors luwen
/// `crates/luwen-api/src/chip/blackhole.rs::arc_fw_init_status`.
const ARC_BOOT_STATUS_0_ADDR: u64 = 0x8003_0408;

/// How long to wait for ARC FW to reach `Done` after a PCIe reset
/// before giving up and proceeding anyway. Empirically the chip
/// finishes well under a second — 5 s is generous headroom.
const ARC_FW_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll cadence while waiting for ARC FW ready. 10 ms keeps the
/// overall wait close to actual completion without spinning.
const ARC_FW_READY_POLL: Duration = Duration::from_millis(10);

/// ARC CSM RAM is at `0x1000_0000` on tile (8,0). The ARC firmware
/// telemetry table lives somewhere in here (its base address is read
/// from `SCRATCH_RAM[13]`). `tt-kmd/telemetry.h::ARC_CSM_SIZE` is
/// `1<<19` = 512 KiB, so a single 2 MiB TLB window covers it with
/// margin. Used by `csm_read32` for the M2 (#68) telemetry walk.
const CSM_WINDOW_BASE: u64 = 0x1000_0000;
const CSM_WINDOW_SIZE: u64 = 0x20_0000;

/// fd + persistent TLB windows that may be rebuilt across a PCIe reset.
///
/// Field order matters: both windows must drop before `fd` because their
/// `Drop` impls issue `FREE_TLB` ioctls on `fd`. We use `ManuallyDrop` +
/// an explicit `Drop` impl so the order is "drop windows, then close
/// fd" regardless of Rust's default field-drop order.
struct Inner {
    /// 2 MiB ARC-tile reset/control window over `0x8000_0000+` — PLL,
    /// reset unit, scratch, MSI FIFO, all the chip-wide config registers.
    reset_window: ManuallyDrop<TlbWindow>,
    /// 2 MiB CSM window over `0x1000_0000+` — ARC firmware RAM, where
    /// the telemetry table lives.
    csm_window: ManuallyDrop<TlbWindow>,
    fd: RawFd,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Unregister the chip-fault ranges before the underlying mmaps
        // disappear (#149). Belt-and-suspenders: even if a stale fault
        // arrives later it can't match a stale range now.
        let reset_data = self.reset_window.data();
        let csm_data = self.csm_window.data();
        crate::daemon::sigbus::unregister_chip_range(reset_data);
        crate::daemon::sigbus::unregister_chip_range(csm_data);
        unsafe {
            ManuallyDrop::drop(&mut self.csm_window);
            ManuallyDrop::drop(&mut self.reset_window);
            libc::close(self.fd);
        }
    }
}

pub struct SharedChip {
    /// Holds the `Inner` across normal use; emptied to `None` only during
    /// `reset_board` while we rotate the fd and window. Readers
    /// (`arc_read32` etc.) take the read lock; `reset_board` takes the write
    /// lock.
    inner: RwLock<Option<Inner>>,
    /// Serializes multi-step sequences (PLL step + reset R-M-W) so concurrent
    /// callers can't interleave their writes to tile (8,0) registers.
    seq_lock: Mutex<()>,
}

// Safety: `SharedChip` may be used via `Arc<SharedChip>` from any thread.
// - `RwLock<Option<Inner>>`: `Inner` owns a `TlbWindow` whose `data()` is a
//   raw `*mut u8` into PCI BAR MMIO. MMIO is a hardware synchronization
//   domain; volatile reads/writes through the pointer from multiple threads
//   land in order at the device (the host bus gives single-copy atomicity
//   for aligned u32 MMIO). `TlbWindow` itself is `Send`-only per `tlb.rs`;
//   we promote `SharedChip` to `Sync` by gating window construction/teardown
//   behind the `RwLock` and gating multi-step sequences behind `seq_lock`.
// - The `RawFd` is a plain integer; kmd's ioctl + mmap paths are thread-safe
//   for our access pattern (per tt-kmd audit in issue #1).
unsafe impl Send for SharedChip {}
unsafe impl Sync for SharedChip {}

/// Read/write surface for the L2CPU_RESET register, abstracted so the
/// concurrency-sensitive R-M-W can be exercised under unit tests.
/// Production goes through `SharedChipResetReg` → `arc_read32 /
/// arc_write32`. Tests substitute an in-memory `AtomicU32` mock that
/// instruments the critical-section invariant.
pub(crate) trait ResetRegisterIo {
    fn read(&self) -> crate::Result<u32>;
    fn write(&self, value: u32) -> crate::Result<()>;
}

struct SharedChipResetReg<'a>(&'a SharedChip);

impl ResetRegisterIo for SharedChipResetReg<'_> {
    fn read(&self) -> crate::Result<u32> {
        self.0.arc_read32(L2CPU_RESET_ADDR)
    }
    fn write(&self, value: u32) -> crate::Result<()> {
        self.0.arc_write32(L2CPU_RESET_ADDR, value)
    }
}

/// Read-modify-write the L2CPU_RESET register: OR-in the release bit
/// for each L2CPU index in `indices` (bit `idx+4`). Returns
/// `(before, mask, after)` for the caller's logging. The caller MUST
/// hold `SharedChip::seq_lock` across this call — bit `idx+4` of
/// L2CPU_RESET is shared across all four L2CPUs, so any concurrent
/// R-M-W from a sibling boot path would silently lose updates and
/// leave one of the cores held in reset. See issue #1 for the
/// host-crashing concurrent-cold-boot incident this lock prevents.
pub(crate) fn apply_release_bits(
    io: &dyn ResetRegisterIo,
    indices: &[usize],
) -> crate::Result<(u32, u32, u32)> {
    let before = io.read()?;
    let mask: u32 = indices.iter().fold(0u32, |m, &i| m | (1u32 << (i + 4)));
    let after = before | mask;
    io.write(after)?;
    Ok((before, mask, after))
}

impl SharedChip {
    pub fn new(card: u32) -> io::Result<Self> {
        let inner = Self::open_inner(card)?;
        Ok(SharedChip {
            inner: RwLock::new(Some(inner)),
            seq_lock: Mutex::new(()),
        })
    }

    /// Test-only constructor that builds a `SharedChip` with no backing fd
    /// or window. Any attempt to call `arc_read32`/`arc_write32`/etc. will
    /// panic — use only from tests that exercise surrounding state without
    /// touching the chip.
    #[cfg(test)]
    pub fn placeholder() -> Self {
        SharedChip {
            inner: RwLock::new(None),
            seq_lock: Mutex::new(()),
        }
    }

    fn open_inner(card: u32) -> io::Result<Inner> {
        let fd = kmd::open_device(card)?;

        // Request max power state. The kmd aggregates flags across all
        // open fds; this fd lives for the daemon's lifetime, so the
        // bump persists. Without it the chip runs at low AICLK (legacy
        // default leaves MAX_AI_CLK off), which slows every Tensix
        // baby-RISC by ~1.7× and breaks the timing assumptions in
        // M6/M6.1's UART poll. Best-effort: older kmds without
        // SET_POWER_STATE return ENOTTY — we warn and carry on.
        if let Err(e) = kmd::request_max_power(fd) {
            crate::dlog!(
                "[shared-chip] warning: SET_POWER_STATE failed ({}); \
                 chip will run at low AICLK (kmd < 2.6?)",
                e
            );
        }

        let reset_window =
            match TlbWindow::new_2m(fd, ARC_TILE_X, ARC_TILE_Y, ARC_RESET_WINDOW_BASE) {
                Ok(w) => w,
                Err(e) => {
                    unsafe {
                        libc::close(fd);
                    }
                    return Err(e);
                }
            };
        let csm_window = match TlbWindow::new_2m(fd, ARC_TILE_X, ARC_TILE_Y, CSM_WINDOW_BASE) {
            Ok(w) => w,
            Err(e) => {
                drop(reset_window);
                unsafe {
                    libc::close(fd);
                }
                return Err(e);
            }
        };
        // Register both persistent windows with the chip-fault handler
        // so a SIGBUS from a tt-smi -r mid-access on the ARC tile lands
        // in the "during chip access" message branch (#149). Re-register
        // on every `open_inner` call (called at SharedChip::new and
        // again after each `reset_board` fd rotation) — the previous
        // mmap was freed and replaced with a new one at a different VA.
        let reset_data = reset_window.data();
        let csm_data = csm_window.data();
        crate::daemon::sigbus::register_chip_range(reset_data, ARC_RESET_WINDOW_SIZE as usize);
        crate::daemon::sigbus::register_chip_range(csm_data, CSM_WINDOW_SIZE as usize);

        Ok(Inner {
            reset_window: ManuallyDrop::new(reset_window),
            csm_window: ManuallyDrop::new(csm_window),
            fd,
        })
    }

    fn reset_window_offset(addr: u64) -> u64 {
        let range = ARC_RESET_WINDOW_BASE..ARC_RESET_WINDOW_BASE + ARC_RESET_WINDOW_SIZE;
        assert!(
            range.contains(&addr),
            "SharedChip: addr 0x{:x} outside ARC reset window [0x{:x}..0x{:x})",
            addr,
            range.start,
            range.end,
        );
        addr - ARC_RESET_WINDOW_BASE
    }

    fn csm_window_offset(addr: u64) -> u64 {
        let range = CSM_WINDOW_BASE..CSM_WINDOW_BASE + CSM_WINDOW_SIZE;
        assert!(
            range.contains(&addr),
            "SharedChip: addr 0x{:x} outside CSM window [0x{:x}..0x{:x})",
            addr,
            range.start,
            range.end,
        );
        addr - CSM_WINDOW_BASE
    }

    /// Single-register u32 read. No `seq_lock` — the MMIO bus gives
    /// single-copy atomicity for aligned u32 reads, and a stale value from a
    /// concurrent writer is fine for a pure read.
    ///
    /// Returns `Err(Internal)` if the inner fd has been rotated out by a
    /// concurrent `reset_board` (#102) — callers in dispatch handlers
    /// propagate via `?` so the daemon stays up.
    pub fn arc_read32(&self, addr: u64) -> crate::Result<u32> {
        let off = Self::reset_window_offset(addr);
        let guard = self.inner.read().unwrap();
        let inner = guard
            .as_ref()
            .ok_or_else(|| crate::Error::internal("SharedChip used while rotating fd"))?;
        Ok(inner.reset_window.read32(off))
    }

    /// Single-register u32 write. Same lock story as `arc_read32`.
    pub fn arc_write32(&self, addr: u64, value: u32) -> crate::Result<()> {
        let off = Self::reset_window_offset(addr);
        let guard = self.inner.read().unwrap();
        let inner = guard
            .as_ref()
            .ok_or_else(|| crate::Error::internal("SharedChip used while rotating fd"))?;
        inner.reset_window.write32(off, value);
        Ok(())
    }

    /// Read a u32 from ARC CSM (the firmware's RAM, used for the
    /// telemetry table — see `src/telemetry.rs` and #75 for context).
    /// The address must lie within `[0x1000_0000, 0x1020_0000)`. A pure
    /// read; same lock story as `arc_read32`.
    pub fn csm_read32(&self, addr: u64) -> crate::Result<u32> {
        let off = Self::csm_window_offset(addr);
        let guard = self.inner.read().unwrap();
        let inner = guard
            .as_ref()
            .ok_or_else(|| crate::Error::internal("SharedChip used while rotating fd"))?;
        Ok(inner.csm_window.read32(off))
    }

    /// Probe whether L2CPU `idx`'s release bit is set. Pure read; no
    /// `seq_lock`.
    pub fn l2cpu_is_running(&self, l2cpu_idx: usize) -> crate::Result<bool> {
        let val = self.arc_read32(L2CPU_RESET_ADDR)?;
        let bit_idx = l2cpu_idx + 4;
        Ok((val >> bit_idx) & 1 == 1)
    }

    /// Read `L2CPU_RESET` raw — used by daemon startup probe to report all
    /// four cores' state in one shot.
    pub fn read_l2cpu_reset(&self) -> crate::Result<u32> {
        self.arc_read32(L2CPU_RESET_ADDR)
    }

    /// Return a `dup(2)` of the inner card fd, with `FD_CLOEXEC` set.
    ///
    /// `dup` shares the kernel `struct file` with the original fd, so it
    /// does *not* re-enter `tt_cdev_open` / `tt_cdev_release` and does not
    /// trigger `tenstorrent_set_aggregated_power_state` — which would
    /// otherwise round-trip through ARC firmware on every L2Cpu
    /// construction/destruction and reset PLL4 to ARC's cached default
    /// (800 MHz, `kPLLInitialSettings`). Per-`L2Cpu` ioctls/mmaps go
    /// through this duped fd so cold-boot/SRST events leave the chip-wide
    /// PLL alone.
    pub fn dup_fd(&self) -> crate::Result<RawFd> {
        let guard = self.inner.read().unwrap();
        let inner = guard
            .as_ref()
            .ok_or_else(|| crate::Error::internal("SharedChip::dup_fd while rotating fd"))?;
        // F_DUPFD_CLOEXEC: one syscall, atomic CLOEXEC. Plain libc::dup
        // doesn't preserve CLOEXEC, so we'd race exec() until the follow-up
        // fcntl set it.
        let dup = unsafe { libc::fcntl(inner.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(crate::Error::internal(format!(
                "SharedChip::dup_fd: fcntl(F_DUPFD_CLOEXEC) failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(dup)
    }

    /// Step the chip-wide L2CPU PLL down to 200 MHz. Caller is
    /// responsible for ensuring no L2CPU is currently running — the
    /// daemon checks the slot table before invoking. Holds `seq_lock`
    /// to serialize against `reset_x280`, so a concurrent boot blocks
    /// here and then `reset_x280`'s existing 200→1750 step-up brings
    /// the PLL back when the next L2CPU boots. See #95.
    pub fn idle_pll(&self) {
        let _guard = self.seq_lock.lock().unwrap();
        crate::dlog!("[idle_pll] stepping L2CPU PLL down to 200 MHz (no L2CPU running)");
        clock::set_frequency(self, 200);
        crate::dlog!("[idle_pll] done");
    }

    /// Release the given L2CPUs from reset via the OpenSBI sequence:
    /// PLL step down to 200 MHz → OR-in release bits → PLL step up to 1750
    /// MHz. Holds `seq_lock` for the entire sequence so concurrent callers
    /// serialize rather than stepping the PLL against each other.
    ///
    /// **PLL is shared across all four L2CPUs.** Stepping it down + up
    /// during a cold boot of one slot glitches the clock for any
    /// running siblings, briefly running them at 200 MHz before they
    /// catch back up. This is unavoidable for the documented OpenSBI
    /// reset-release procedure on this silicon — the alternative
    /// (release at 1750 MHz directly) wedges the chip. Callers should
    /// expect a sub-millisecond performance dip on running siblings
    /// when a new L2CPU comes up; functional correctness is preserved.
    pub fn reset_x280(&self, l2cpu_indices: &[usize]) -> crate::Result<()> {
        let _guard = self.seq_lock.lock().unwrap();

        crate::dlog!("[reset_x280] stepping PLL down to 200 MHz");
        clock::set_frequency(self, 200);

        let io = SharedChipResetReg(self);
        let (before, mask, after) = apply_release_bits(&io, l2cpu_indices)?;
        crate::dlog!(
            "[reset_x280] L2CPU_RESET@0x{:x}: {:#010x} | {:#010x} -> {:#010x} (releasing L2CPU {:?})",
            L2CPU_RESET_ADDR, before, mask, after, l2cpu_indices
        );
        let reset_val_after = self.arc_read32(L2CPU_RESET_ADDR)?;
        crate::dlog!(
            "[reset_x280] L2CPU_RESET readback: {:#010x}",
            reset_val_after
        );

        crate::dlog!("[reset_x280] stepping PLL up to 1750 MHz");
        clock::set_frequency(self, 1750);
        crate::dlog!("[reset_x280] done");
        Ok(())
    }

    /// Full PCIe link reset via tt-kmd's `RESET_DEVICE` ioctl. The
    /// re-enumeration invalidates any fd held across the call, so we drop
    /// our persistent window + fd before issuing the reset and reopen fresh
    /// after. Takes the write lock so no concurrent reader sees a torn state.
    ///
    /// After reopen, polls ARC FW init status (boot_status_0) until it
    /// reports Done — that's how UMD waits for ARC FW boot, GDDR PHY
    /// power-up, and DRAM training to all finish before anyone touches
    /// the chip. Replaces the historical fixed 1 s post-reset sleep.
    pub fn reset_board(&self, card: u32) -> io::Result<()> {
        let mut guard = self.inner.write().unwrap();
        // Drop the existing Inner (window's FREE_TLB + close fd) BEFORE the
        // reset, so the kmd doesn't see stale references mid-reset.
        drop(guard.take());
        chip::reset_board(card)?;
        let inner = Self::open_inner(card)?;
        Self::wait_arc_fw_ready_inner(&inner);
        *guard = Some(inner);
        Ok(())
    }

    /// Poll ARC FW init status on a freshly-opened `Inner`. Reads
    /// `boot_status_0` directly off the reset window (we already hold
    /// the `RwLock` write guard around the caller, so we can't go
    /// through `arc_read32`). Bails out on `Done` or `Error`; warns and
    /// returns on timeout so a future ARC FW that changes the protocol
    /// can't permanently wedge the daemon.
    fn wait_arc_fw_ready_inner(inner: &Inner) {
        let off = ARC_BOOT_STATUS_0_ADDR - ARC_RESET_WINDOW_BASE;
        let start = Instant::now();
        let deadline = start + ARC_FW_READY_TIMEOUT;
        loop {
            let bs0 = inner.reset_window.read32(off);
            match (bs0 >> 1) & 0x3 {
                2 => {
                    crate::dlog!(
                        "[shared-chip] ARC FW init Done after {:?} (boot_status_0={:#010x})",
                        start.elapsed(),
                        bs0
                    );
                    return;
                }
                3 => {
                    crate::dlog!(
                        "[shared-chip] warning: ARC FW init Error after {:?} (boot_status_0={:#010x}); proceeding",
                        start.elapsed(),
                        bs0
                    );
                    return;
                }
                _ if Instant::now() >= deadline => {
                    crate::dlog!(
                        "[shared-chip] warning: timed out waiting for ARC FW init Done after {:?} (boot_status_0={:#010x}); proceeding",
                        start.elapsed(),
                        bs0
                    );
                    return;
                }
                _ => std::thread::sleep(ARC_FW_READY_POLL),
            }
        }
    }
}

impl PllAccess for SharedChip {
    fn pll_read32(&self, addr: u64) -> u32 {
        // PllAccess can't fail from `clock::set_frequency`'s POV; if the
        // SharedChip is rotating during a PLL step the daemon is in
        // unrecoverable territory (concurrent reset_board mid-reset_x280,
        // which #102's invariants rule out). Surface as a clear panic
        // message rather than threading Result through the trait.
        self.arc_read32(addr)
            .expect("PllAccess: SharedChip rotating during clock step")
    }
    fn pll_write32(&self, addr: u64, value: u32) {
        self.arc_write32(addr, value)
            .expect("PllAccess: SharedChip rotating during clock step");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Layout invariants — if these ever change, the assertions in
    // `reset_window_offset` / `csm_window_offset` (and any hand-tuned
    // addresses) stop being valid. Compile-time because all inputs are
    // `const`.
    const _: () = {
        // PLL register, reset register live inside the ARC reset window.
        assert!(ARC_RESET_WINDOW_BASE <= 0x80020500);
        assert!(0x80020500 < ARC_RESET_WINDOW_BASE + ARC_RESET_WINDOW_SIZE);
        assert!(ARC_RESET_WINDOW_BASE <= L2CPU_RESET_ADDR);
        assert!(L2CPU_RESET_ADDR < ARC_RESET_WINDOW_BASE + ARC_RESET_WINDOW_SIZE);
        // The CSM window must cover ARC_CSM_BASE..ARC_CSM_BASE+ARC_CSM_SIZE
        // (0x10000000..0x10080000, 512 KiB) — the telemetry table can
        // live anywhere in there.
        assert!(CSM_WINDOW_BASE == 0x10000000);
        assert!(CSM_WINDOW_SIZE >= 0x80000);
    };

    #[test]
    fn reset_window_offset_maps_known_addresses() {
        assert_eq!(SharedChip::reset_window_offset(0x80020500), 0x20500);
        assert_eq!(SharedChip::reset_window_offset(L2CPU_RESET_ADDR), 0x30014);
    }

    #[test]
    #[should_panic(expected = "outside ARC reset window")]
    fn reset_window_offset_rejects_out_of_range_addr() {
        SharedChip::reset_window_offset(ARC_RESET_WINDOW_BASE + ARC_RESET_WINDOW_SIZE);
    }

    #[test]
    #[should_panic(expected = "outside ARC reset window")]
    fn reset_window_offset_rejects_below_base() {
        SharedChip::reset_window_offset(ARC_RESET_WINDOW_BASE - 1);
    }

    #[test]
    fn csm_window_offset_maps_known_addresses() {
        // ARC_CSM_BASE itself maps to offset 0; one past the end of CSM
        // (still within the 2 MiB window) is also valid arithmetic, but
        // the kernel-side CSM check (`is_range_within_csm` in
        // `tt-kmd/telemetry.h`) bounds at 512 KiB.
        assert_eq!(SharedChip::csm_window_offset(CSM_WINDOW_BASE), 0);
        assert_eq!(
            SharedChip::csm_window_offset(CSM_WINDOW_BASE + 0x434),
            0x434
        );
    }

    #[test]
    #[should_panic(expected = "outside CSM window")]
    fn csm_window_offset_rejects_out_of_range_addr() {
        SharedChip::csm_window_offset(CSM_WINDOW_BASE + CSM_WINDOW_SIZE);
    }

    #[test]
    fn arc_accessors_return_internal_error_during_fd_rotation() {
        // SharedChip::placeholder leaves `inner: None` — the same
        // state reset_board uses while it's swapping the fd. Each
        // accessor must surface that as Internal rather than panic
        // (#102).
        let chip = SharedChip::placeholder();
        let read_err = chip.arc_read32(L2CPU_RESET_ADDR).unwrap_err();
        assert!(matches!(read_err, crate::Error::Internal(_)));
        let write_err = chip.arc_write32(L2CPU_RESET_ADDR, 0).unwrap_err();
        assert!(matches!(write_err, crate::Error::Internal(_)));
        let csm_err = chip.csm_read32(CSM_WINDOW_BASE).unwrap_err();
        assert!(matches!(csm_err, crate::Error::Internal(_)));

        // Higher-level helpers propagate the same way.
        let probe_err = chip.l2cpu_is_running(0).unwrap_err();
        assert!(matches!(probe_err, crate::Error::Internal(_)));
        let read_reset_err = chip.read_l2cpu_reset().unwrap_err();
        assert!(matches!(read_reset_err, crate::Error::Internal(_)));
    }

    // ---- apply_release_bits (pure bit-OR semantics) ----
    //
    // The seq_lock invariant in production `reset_x280` (#1) is the
    // critical correctness property here, but it can't be exercised
    // without driving the real `SharedChip` (which needs hardware).
    // Asserting it via a fresh local Mutex would only test
    // `std::sync::Mutex`, not the production gating — see #173. So we
    // test only the pure bit-OR shape here; integration tests against
    // hardware cover the locking.

    use std::cell::Cell;

    /// Minimal in-memory stand-in for the L2CPU_RESET register: a
    /// single-thread Cell that records the most-recently-written
    /// value. Sufficient for the pure bit-OR tests below.
    struct MockResetReg {
        val: Cell<u32>,
    }

    impl MockResetReg {
        fn new(initial: u32) -> Self {
            MockResetReg {
                val: Cell::new(initial),
            }
        }
    }

    impl ResetRegisterIo for MockResetReg {
        fn read(&self) -> crate::Result<u32> {
            Ok(self.val.get())
        }
        fn write(&self, value: u32) -> crate::Result<()> {
            self.val.set(value);
            Ok(())
        }
    }

    #[test]
    fn apply_release_bits_ors_in_each_index_bit() {
        // Pre-seed with a sentinel that should survive the OR.
        let mock = MockResetReg::new(0x0000_0001);

        let (before, mask, after) = apply_release_bits(&mock, &[0, 2]).unwrap();

        assert_eq!(before, 0x0000_0001);
        // bit 0+4 = 0x10, bit 2+4 = 0x40 -> mask = 0x50.
        assert_eq!(mask, 0x0000_0050);
        // After OR: low sentinel bit preserved; release bits set.
        assert_eq!(after, 0x0000_0051);
        assert_eq!(mock.val.get(), 0x0000_0051);
    }

    #[test]
    fn apply_release_bits_with_empty_indices_is_pure_readback() {
        let mock = MockResetReg::new(0xFEED_F00D);

        let (before, mask, after) = apply_release_bits(&mock, &[]).unwrap();

        assert_eq!(before, 0xFEED_F00D);
        assert_eq!(mask, 0);
        assert_eq!(after, 0xFEED_F00D);
    }
}
