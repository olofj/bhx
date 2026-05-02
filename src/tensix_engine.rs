// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Daemon-owned Tensix tile that hosts the M3 (#69) virtio-mmio
//! engine firmware and serves all L2CPUs on the chip.
//!
//! Lifecycle (when the `virtio-engine` feature is enabled):
//!
//!   1. At daemon startup (or on first boot, depending on integration)
//!      pick a tile via `tensix_tile::pick_virtio_engine_tile`,
//!      assert all baby-RISC soft resets, load the M3 firmware
//!      (`virtio_engine::VIRTIO_FIRMWARE`), and release BRISC.
//!   2. For each L2CPU boot, program one small TLB on the L2CPU
//!      pointing at this tile's L1 sub-window for that L2CPU
//!      (`virtio_engine::l2cpu_window_base(idx)`). Hand the resulting
//!      x280 PA to the DTB builder so the guest's virtio-mmio reg
//!      ranges target our reg files.
//!
//! One Tensix tile serves all four L2CPUs on the chip (per the #66
//! design discussion). BRISC's poll loop sweeps all 16 reg files
//! (4 L2CPUs × 4 devices) in well under 1 µs — orders of magnitude
//! faster than the L2CPU's NoC RTT, which closes the QUEUE_READY
//! race that motivated this whole architecture.

use std::io;

use crate::shared_chip::SharedChip;
use crate::telemetry;
use crate::tensix::TensixTile;
use crate::tensix_proto as proto;
use crate::tensix_tile;
use crate::virtio_engine as ve;

/// Reservation of one Tensix tile for the virtio-mmio engine.
///
/// Holds the open `TensixTile` (so its TLBs stay alive for the
/// daemon's lifetime), the picker output coords, the cached
/// translated coords needed for L2CPU TLB programming, and the
/// firmware version returned by the M5 handshake.
pub struct TensixEngine {
    tile: TensixTile,
    /// NOC0-logical (x, y) — what the M2 picker returned. Used in
    /// `daemon status` and for diagnostics.
    pub noc0_x: u16,
    pub noc0_y: u16,
    /// Translated (x, y) — what the L2CPU's small TLB hardware
    /// expects (per `tensix_tile::noc0_to_translated_tensix`).
    pub translated_x: u16,
    pub translated_y: u16,
    /// Firmware version reported in the M5 handshake's hello-ack.
    pub firmware_version: u32,
    /// Protocol version reported in the same hello-ack. Always
    /// matches `proto::PROTOCOL_VERSION` post-bring-up — bring-up
    /// fails fast on mismatch.
    pub protocol_version: u32,
}

// Safety: same story as `SharedChip`. The contained `TensixTile`
// owns TLB windows backed by PCI BAR MMIO; volatile aligned u32
// reads/writes through those mappings have hardware-level atomicity.
// We promote to `Send + Sync` so the engine can live in an
// `Arc<TensixEngine>` shared across the daemon's worker threads
// (boot path, status, future M5 daemon-side bridge). All write paths
// touch the chip from a single thread at a time (held under the
// daemon's `tensix_engine` mutex during bring-up; read-only after);
// concurrent readers see hardware-coherent values via the BAR.
unsafe impl Send for TensixEngine {}
unsafe impl Sync for TensixEngine {}

impl TensixEngine {
    /// Bring up a Tensix tile to host the M3 virtio firmware. Picks
    /// a tile via M2, asserts all soft resets, loads the firmware,
    /// pre-clears the reg-file region, and releases BRISC. After
    /// this returns, the on-tile firmware is running and is ready
    /// to serve guest MMIO.
    pub fn bring_up(card: u32, chip: &SharedChip) -> io::Result<Self> {
        let telem = telemetry::read_telemetry(chip).map_err(|e| {
            io::Error::from(crate::Error::internal(format!("read telemetry: {}", e)))
        })?;
        let picked = tensix_tile::pick_virtio_engine_tile(&telem)
            .map_err(|e| io::Error::from(crate::Error::internal(format!("pick tile: {}", e))))?;

        // Translate picker output to the L2CPU TLB's coord flavor.
        // For the canonical Blackhole layout (harvest at the tail of
        // the col list), this is identity; the helper handles other
        // chips correctly. Failing to translate at this point means
        // the picker handed us a coord the L2CPU TLB can't reach,
        // which is a hard error — fail bring-up rather than ship a
        // silently-broken engine.
        let (translated_x, translated_y) = tensix_tile::noc0_to_translated_tensix(
            picked.x,
            picked.y,
            telem.enabled_tensix_col,
            telem.noc_translation_enabled,
        )
        .ok_or_else(|| {
            io::Error::from(crate::Error::internal(format!(
                "tile ({}, {}) has no translated form for L2CPU TLB \
                 (enabled_tensix_col={:#x}, noc_translation={})",
                picked.x, picked.y, telem.enabled_tensix_col, telem.noc_translation_enabled
            )))
        })?;

        let tile = TensixTile::new(card, picked.x, picked.y).map_err(|e| {
            io::Error::from(crate::Error::Io {
                ctx: format!(
                    "open tensix tile ({}, {}) on card {}",
                    picked.x, picked.y, card
                ),
                source: e,
            })
        })?;

        // Pre-bring-up sniff: if the picked tile's TCM is already
        // non-zero, *something* loaded firmware here previously.
        // tt-metal's dispatch firmware lives in TCM at offset 0
        // (instruction stream); a fresh chip after `tt-smi -r` zeros
        // L1 and TCM, so any non-zero pattern that isn't ours is a
        // tip-off. We can't always tell tt-metal residue from a
        // benign leftover, so we warn rather than fail. See #74 +
        // docs/tt-metal-coexistence.md.
        warn_if_tile_appears_busy(&tile, picked.x, picked.y);

        // Halt all baby RISCs, pre-clear the reg-file region so the
        // firmware's writes are unambiguous, load firmware, release
        // BRISC. Same pattern as `debug tensix-virtio` (M3 smoke).
        tile.assert_all_resets();
        for slot in 0..ve::NUM_SLOTS {
            let base = ve::slot_regs_base(slot);
            for off in (0..ve::REGS_PER_DEV).step_by(4) {
                tile.write_l1_u32(base + off, 0);
            }
        }
        tile.load_brisc_firmware(ve::VIRTIO_FIRMWARE);

        // M6.1 (#79): set up TRISC0's reset PC override before BRISC
        // runs. BRISC drives TRISC0's soft-reset bit from the active-
        // slots bitmap (UART portion), so all the host has to do is
        // make sure that whenever BRISC clears bit 12, TRISC0 enters
        // `trisc0_reset_entry`. The firmware's `start.S` plants the
        // linker-resolved address at L1[0x4] for us to read here.
        // TRISC0 stays in soft reset (bit 12 of SOFT_RESET_ALL)
        // until BRISC releases it; this is intentional — bring-up
        // doesn't release TRISC0 directly, the firmware does on the
        // first UART register.
        let trisc0_pc = tile.read_trisc0_reset_entry();
        if trisc0_pc != 0 {
            tile.set_trisc0_reset_pc(trisc0_pc);
            tile.enable_trisc0_reset_pc_override();
        } else {
            // Pre-M6.1 firmware doesn't plant the TRISC0 entry word,
            // so we just skip the override. BRISC's lifecycle code
            // also doesn't exist in that case, so TRISC0 stays in
            // soft-reset and nothing tries to release it.
            eprintln!(
                "[tensix-engine] firmware {:#010x} on tile ({}, {}) \
                 doesn't expose a TRISC0 entry; skipping reset-PC override",
                tile.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_VERSION),
                picked.x,
                picked.y
            );
        }

        // Same ritual for TRISC1 (#125 dedicated QUEUE_SEL→READY watch).
        // Skipped silently for pre-#125 firmware (entry word is 0).
        let trisc1_pc = tile.read_trisc1_reset_entry();
        if trisc1_pc != 0 {
            tile.set_trisc1_reset_pc(trisc1_pc);
            tile.enable_trisc1_reset_pc_override();
        }

        // TRISC2 (#158 dedicated DEVICE_FEATURES_SEL watch). Same
        // ritual; pre-#158 firmware leaves the entry word at 0 and
        // the host skips the override. Lifecycle: BRISC drives TRISC2
        // out of soft reset off the virtio-only mask, so no explicit
        // release here.
        let trisc2_pc = tile.read_trisc2_reset_entry();
        if trisc2_pc != 0 {
            tile.set_trisc2_reset_pc(trisc2_pc);
            tile.enable_trisc2_reset_pc_override();
        }

        tile.release_brisc_only();

        // Wait for the firmware to publish its stats-page magic.
        // BRISC runs init_stats + init_proto + init_device × 16
        // before this is set; at ~64 KiB of stores at ~1 GHz this
        // is microseconds, but we poll up to 200 ms to keep things
        // robust against slow first sweeps.
        let stats_magic_addr = ve::STATS_BASE + ve::STATS_OFF_MAGIC;
        let started = std::time::Instant::now();
        loop {
            let m = tile.read_l1_u32(stats_magic_addr);
            if m == ve::STATS_MAGIC_LOADED {
                break;
            }
            if started.elapsed() > std::time::Duration::from_millis(200) {
                return Err(crate::Error::internal(format!(
                    "BRISC firmware on tile ({}, {}) did not initialize \
                     stats magic within 200 ms (got {:#010x}, expected {:#010x})",
                    picked.x,
                    picked.y,
                    m,
                    ve::STATS_MAGIC_LOADED
                ))
                .into());
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        // M5 (#71) handshake. BRISC blocks in `wait_for_hello_and_ack`
        // until we send hello, so this also gates the firmware's
        // entry into the steady-state poll loop.
        let (firmware_version, protocol_version) = run_handshake(&tile, picked.x, picked.y)?;

        eprintln!(
            "[tensix-engine] up on card {} tile NOC0 ({}, {}), translated ({}, {}); \
             firmware version {:#010x} (build_id {:#08x}, protocol v{})",
            card,
            picked.x,
            picked.y,
            translated_x,
            translated_y,
            firmware_version,
            (firmware_version >> 8) & 0x00ff_ffff,
            protocol_version,
        );

        // Surface the reservation to operators / wrapper scripts that
        // run tt-metal alongside the daemon. See #74 +
        // docs/tt-metal-coexistence.md for the contract.
        write_reserved_tile_file(card, picked.x, picked.y);

        Ok(TensixEngine {
            tile,
            noc0_x: picked.x,
            noc0_y: picked.y,
            translated_x,
            translated_y,
            firmware_version,
            protocol_version,
        })
    }

    /// Adopt an engine that the previous daemon left running on the
    /// chip. The Tensix has the same lifetime as the L2CPUs; if some
    /// L2CPUs are still alive across a daemon restart, the firmware
    /// on BRISC is also still running, and re-running `bring_up`
    /// (which halts BRISC and reloads firmware) would tear out the
    /// running guests' MMIO backend mid-flight.
    ///
    /// What this skips relative to `bring_up`:
    ///   - assert_all_resets / write_l1 zero / load_brisc_firmware
    ///   - release_brisc_only
    ///   - run_handshake (firmware is past `wait_for_hello_and_ack`,
    ///     it's in the steady-state poll loop and won't ack again)
    ///
    /// What it still does: pick the same tile (deterministic from
    /// telemetry), open the TLB-backed `TensixTile`, sanity-check
    /// the stats magic to confirm the firmware is alive, populate
    /// the cached versions from the stats page so downstream
    /// callers can render them in `daemon status`.
    ///
    /// Fails if the firmware doesn't have the stats magic — at that
    /// point the chip has lost firmware and the caller should fall
    /// back to a cold `bring_up` (which assumes no L2CPU traffic).
    pub fn adopt_running(card: u32, chip: &SharedChip) -> io::Result<Self> {
        let telem = telemetry::read_telemetry(chip).map_err(|e| {
            io::Error::from(crate::Error::internal(format!("read telemetry: {}", e)))
        })?;
        let picked = tensix_tile::pick_virtio_engine_tile(&telem)
            .map_err(|e| io::Error::from(crate::Error::internal(format!("pick tile: {}", e))))?;
        let (translated_x, translated_y) = tensix_tile::noc0_to_translated_tensix(
            picked.x,
            picked.y,
            telem.enabled_tensix_col,
            telem.noc_translation_enabled,
        )
        .ok_or_else(|| {
            io::Error::from(crate::Error::internal(format!(
                "tile ({}, {}) has no translated form for L2CPU TLB",
                picked.x, picked.y
            )))
        })?;
        let tile = TensixTile::new(card, picked.x, picked.y).map_err(|e| {
            io::Error::from(crate::Error::Io {
                ctx: format!(
                    "open tensix tile ({}, {}) on card {}",
                    picked.x, picked.y, card
                ),
                source: e,
            })
        })?;
        let stats_magic = tile.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_MAGIC);
        if stats_magic != ve::STATS_MAGIC_LOADED {
            return Err(crate::Error::internal(format!(
                "BRISC firmware not running on tile ({}, {}) (stats magic {:#010x}, expected {:#010x}); \
                 chip lost firmware — caller must cold-`bring_up` instead",
                picked.x, picked.y, stats_magic, ve::STATS_MAGIC_LOADED
            ))
            .into());
        }
        let firmware_version = tile.read_l1_u32(ve::STATS_BASE + ve::STATS_OFF_VERSION);
        // Firmware encodes BRISC_VIRTIO_FW_VERSION as
        // `<build_id 24-bit><protocol 8-bit>`. The low byte tracks
        // `TENSIX_PROTOCOL_VERSION` (the wire-format protocol), the
        // upper 24 bits are the build_id (git short hash / sha256
        // prefix of firmware sources). The daemon refuses to adopt
        // unless BOTH match — protocol mismatch is a wire-format
        // break, build_id mismatch means the chip's firmware is from
        // a different daemon build than ours and may have different
        // behavior even at the same protocol version.
        let firmware_protocol = firmware_version & 0xff;
        let firmware_build_id = (firmware_version >> 8) & 0x00ff_ffff;
        let expected_build_id = ve::FW_BUILD_ID & 0x00ff_ffff;
        // ALWAYS log what we found on the chip so the operator has a
        // breadcrumb even when adoption succeeds, fails, or we end up
        // having to reject mid-run with active L2CPUs (in which case
        // the next cold-boot RPC's `bring_up` does the actual reload).
        eprintln!(
            "[tensix-engine] chip-side firmware on card {} tile NOC0 ({}, {}): \
             version {:#010x} (build_id {:#08x}, protocol v{}); \
             daemon embeds build_id {:#08x}, protocol v{}",
            card,
            picked.x,
            picked.y,
            firmware_version,
            firmware_build_id,
            firmware_protocol,
            expected_build_id,
            proto::PROTOCOL_VERSION,
        );
        if firmware_protocol != proto::PROTOCOL_VERSION {
            return Err(crate::Error::internal(format!(
                "BRISC firmware on tile ({}, {}) is protocol v{} but daemon expects v{} \
                 (firmware_version={:#010x}); chip needs `tt-smi -r` to reload firmware",
                picked.x,
                picked.y,
                firmware_protocol,
                proto::PROTOCOL_VERSION,
                firmware_version
            ))
            .into());
        }
        if firmware_build_id != expected_build_id {
            return Err(crate::Error::internal(format!(
                "BRISC firmware on tile ({}, {}) build_id {:#08x} != daemon's embedded build_id {:#08x} \
                 (chip is running a stale firmware from a prior daemon build); \
                 chip needs `tt-smi -r` to reload firmware",
                picked.x, picked.y, firmware_build_id, expected_build_id
            ))
            .into());
        }
        let protocol_version = firmware_protocol;
        eprintln!(
            "[tensix-engine] adopted running firmware on card {} tile NOC0 ({}, {}); \
             build_id {:#08x}, protocol v{}",
            card, picked.x, picked.y, firmware_build_id, protocol_version,
        );
        // Same as `bring_up`: republish the reservation. The previous
        // daemon may have left a stale file behind (or none, if it
        // pre-dated #74) so a fresh write keeps operator tooling
        // accurate without depending on the prior daemon's state.
        write_reserved_tile_file(card, picked.x, picked.y);

        Ok(TensixEngine {
            tile,
            noc0_x: picked.x,
            noc0_y: picked.y,
            translated_x,
            translated_y,
            firmware_version,
            protocol_version,
        })
    }

    /// Snapshot of the kick ring header. Diagnostic; the actual
    /// data-plane consumer in M5+ will read producer_seq in a tight
    /// loop and consume entries.
    pub fn kick_ring_header(&self) -> (u32, u32, u32) {
        let producer = self.tile.read_l1_u32(
            proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_PRODUCER_SEQ,
        );
        let consumer = self.tile.read_l1_u32(
            proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_CONSUMER_SEQ,
        );
        let entries = self.tile.read_l1_u32(
            proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_RING_ENTRIES,
        );
        (producer, consumer, entries)
    }

    /// Read one kick entry by ring index (`producer_seq` modulo the
    /// ring-entries count). Returns the raw 4 u32s; the M5 consumer
    /// in the daemon parses them via `tensix_proto::KickEntry`.
    pub fn read_kick_entry(&self, idx: u32) -> [u32; 4] {
        let off = proto::CTRL_BASE
            + proto::CTRL_OFF_KICK_RING
            + (idx % proto::KICK_RING_ENTRIES) * proto::KICK_ENTRY_SIZE;
        [
            self.tile.read_l1_u32(off),
            self.tile.read_l1_u32(off + 4),
            self.tile.read_l1_u32(off + 8),
            self.tile.read_l1_u32(off + 12),
        ]
    }

    /// Read the kick ring's producer sequence — what the kick poller
    /// thread tight-loops on to detect new entries.
    pub fn kick_producer_seq(&self) -> u32 {
        self.tile.read_l1_u32(
            proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_PRODUCER_SEQ,
        )
    }

    /// Update the kick ring's consumer sequence. The kick poller
    /// thread bumps this after consuming each batch of entries; BRISC
    /// could (in a future flow-control extension) read it to know
    /// when to stop producing if the ring is full. Today BRISC
    /// doesn't read it, but we still update it for accurate
    /// diagnostics in `daemon status`.
    pub fn set_kick_consumer_seq(&self, seq: u32) {
        self.tile.write_l1_u32(
            proto::CTRL_BASE + proto::CTRL_OFF_KICK_RING_HDR + proto::KICK_HDR_OFF_CONSUMER_SEQ,
            seq,
        );
    }

    /// Read a u32 anywhere in the engine tile's L1 — used by the
    /// data-plane worker to fetch per-queue desc/avail/used pointers
    /// from the firmware's shadow region. Generic so we don't have
    /// to add a per-field accessor for every shadow slot.
    pub fn read_l1_u32(&self, addr: u32) -> u32 {
        self.tile.read_l1_u32(addr)
    }

    /// Write a u32 anywhere in the engine tile's L1. Counterpart to
    /// `read_l1_u32`; useful for debug commands that simulate a
    /// guest MMIO write directly into the reg file (skipping the
    /// L2CPU's own TLB), and for the M5.5b daemon-driven init path
    /// when we set up per-queue state.
    pub fn write_l1_u32(&self, addr: u32, value: u32) {
        self.tile.write_l1_u32(addr, value);
    }

    /// Host VA pointing at L1 byte `addr`. Used by paths that need a
    /// raw `*mut u32` into the reg file — notably
    /// `InterruptController::set_interrupt`, which RMWs
    /// `MMIO_INTERRUPT_STATUS` before kicking the PLIC. The pointer
    /// is valid as long as the engine (and its `TensixTile`) is
    /// alive.
    pub fn l1_ptr(&self, addr: u32) -> *mut u8 {
        self.tile.l1_ptr(addr)
    }

    /// Append a CompletionEntry to the L1 completion ring and bump
    /// producer_seq. Called from the data-plane worker after writing
    /// a used-ring entry, so BRISC's poll loop wakes up and (in a
    /// future PLIC IRQ extension) signals the L2CPU directly. Today
    /// the daemon-side worker fires the PLIC IRQ itself; the
    /// completion ring entry is for diagnostics + future use.
    pub fn push_completion(&self, slot: u16, queue_idx: u16, used_idx: u32) {
        let producer_addr =
            proto::CTRL_BASE + proto::CTRL_OFF_COMPL_RING_HDR + proto::COMPL_HDR_OFF_PRODUCER_SEQ;
        let producer = self.tile.read_l1_u32(producer_addr);
        let idx = producer % proto::COMPL_RING_ENTRIES;
        let entry_off =
            proto::CTRL_BASE + proto::CTRL_OFF_COMPL_RING + idx * proto::COMPL_ENTRY_SIZE;
        // Pack slot + queue_idx into the first u32 the same way the
        // firmware reads it.
        self.tile
            .write_l1_u32(entry_off, (slot as u32) | ((queue_idx as u32) << 16));
        self.tile.write_l1_u32(entry_off + 4, used_idx);
        self.tile
            .write_l1_u32(producer_addr, producer.wrapping_add(1));
    }

    /// Read TRISC0's heartbeat (M6.1, #79). Bumped each iteration of
    /// `trisc0_main`'s loop. Stays at zero while TRISC0 is in soft
    /// reset; advances rapidly after BRISC clears the reset bit.
    pub fn trisc0_heartbeat(&self) -> u32 {
        self.tile
            .read_l1_u32(crate::uart_engine::trisc0_heartbeat_addr())
    }

    /// Write the active-slots bitmap directly. Diagnostic — production
    /// code uses [`crate::tensix_data_plane::KickPoller::register_uart`]
    /// and friends, which compute the mask from the registries.
    pub fn write_active_slots(&self, mask: u32) {
        self.tile
            .write_l1_u32(proto::CTRL_BASE + proto::CTRL_OFF_ACTIVE_SLOTS, mask);
    }

    /// Program one small TLB on the given L2CPU pointing at this
    /// tile's reg-file slice for that L2CPU. Returns the L2CPU PA
    /// of the start of the per-device sub-window — identical shape
    /// to the host-buffer path's `x280_base`, only the destination
    /// differs.
    ///
    /// The TLB always uses the **uncached aperture** so the L2CPU's
    /// L3 doesn't shadow guest writes (per #66 fragility item 4).
    /// `program_small_tlb_unicast` already uses the UC aperture.
    pub fn program_l2cpu_tlb(
        &self,
        l2cpu: &crate::l2cpu::L2Cpu,
        l2cpu_idx: u32,
    ) -> std::io::Result<u64> {
        let noc_addr = ve::l2cpu_window_base(l2cpu_idx) as u64;
        crate::x280_tlb::program_small_tlb_unicast(
            l2cpu,
            crate::x280_tlb::SHARED_TLB_SLOT,
            self.translated_x as u32,
            self.translated_y as u32,
            noc_addr,
        )
    }
}

/// M5 (#71) handshake. After firmware is loaded and BRISC released,
/// BRISC blocks in `wait_for_hello_and_ack` polling for the hello
/// magic. We write protocol version + magic-last; BRISC sees magic,
/// reads version, writes hello-ack with its protocol+firmware
/// versions + ack-magic. We poll for ack-magic with a timeout.
///
/// Returns `(firmware_version, protocol_version)` from the ack.
/// Errors on protocol mismatch or timeout.
fn run_handshake(tile: &TensixTile, picked_x: u16, picked_y: u16) -> io::Result<(u32, u32)> {
    use std::time::{Duration, Instant};

    // Write hello: protocol version, then magic last so a partial
    // write never tricks BRISC into responding.
    tile.write_l1_u32(
        proto::CTRL_BASE + proto::CTRL_OFF_HELLO + proto::HELLO_OFF_PROTOCOL_VERSION,
        proto::PROTOCOL_VERSION,
    );
    tile.write_l1_u32(
        proto::CTRL_BASE + proto::CTRL_OFF_HELLO + proto::HELLO_OFF_MAGIC,
        proto::HELLO_MAGIC,
    );

    // Poll for hello-ack magic. Generous timeout — bring-up is a
    // one-shot pass; we'd rather wait a few hundred ms than fail
    // spuriously on a slow-firing first iteration of BRISC's poll
    // loop.
    let timeout = Duration::from_millis(500);
    let started = Instant::now();
    loop {
        let magic = tile
            .read_l1_u32(proto::CTRL_BASE + proto::CTRL_OFF_HELLO_ACK + proto::HELLO_ACK_OFF_MAGIC);
        if magic == proto::HELLO_ACK_MAGIC {
            break;
        }
        if started.elapsed() > timeout {
            return Err(crate::Error::internal(format!(
                "M5 handshake: BRISC on tile ({}, {}) did not respond to hello \
                 within {:?} (last ack magic: {:#010x}, expected {:#010x})",
                picked_x,
                picked_y,
                timeout,
                magic,
                proto::HELLO_ACK_MAGIC
            ))
            .into());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let protocol_version = tile.read_l1_u32(
        proto::CTRL_BASE + proto::CTRL_OFF_HELLO_ACK + proto::HELLO_ACK_OFF_PROTOCOL_VERSION,
    );
    let firmware_version = tile.read_l1_u32(
        proto::CTRL_BASE + proto::CTRL_OFF_HELLO_ACK + proto::HELLO_ACK_OFF_FIRMWARE_VERSION,
    );
    if protocol_version != proto::PROTOCOL_VERSION {
        return Err(crate::Error::internal(format!(
            "M5 handshake: protocol version mismatch — daemon expected {}, \
             firmware reported {}. Rebuild brisc-firmware to match.",
            proto::PROTOCOL_VERSION,
            protocol_version
        ))
        .into());
    }
    Ok((firmware_version, protocol_version))
}

/// Sniff the picked tile's L1 for prior-firmware bytes. tt-metal's
/// dispatch firmware sits at the start of BRISC TCM (offset 0); a
/// fresh chip after `tt-smi -r` has zeros there. If the first 16
/// bytes are non-zero AND don't look like our own start.S header
/// (which we'd see on warm-resume), log a loud warning that the
/// daemon is taking over a tile someone else may still be using.
/// Heuristic — false positives possible (a previous bhx
/// session that crashed mid-flight), so warn rather than fail. See
/// #74 + docs/tt-metal-coexistence.md.
fn warn_if_tile_appears_busy(tile: &TensixTile, x: u16, y: u16) {
    let mut buf = [0u32; 4];
    for (i, slot) in buf.iter_mut().enumerate() {
        *slot = tile.read_l1_u32((i * 4) as u32);
    }
    // All-zero TCM is the freshly-reset baseline. Don't warn there.
    if buf.iter().all(|w| *w == 0) {
        return;
    }
    // Our own start.S plants a fixed dispatch sequence at TCM offset
    // 0; if we recognize the leading word as ours, this is a warm
    // resume of a previous bhx session and we're about to
    // adopt cleanly anyway. Don't spam warnings on the normal path.
    // The expected first bytes are `6f 00 00 08` (LE u32 = 0x0800006f
    // = `j 0x80`, the relative jump that vectors hartid 0 to brisc_main
    // and others to the per-core dispatch). If start.S changes that
    // leading instruction this needs updating.
    const TT_BH_BRISC_FIRMWARE_FIRST_WORD: u32 = 0x0800006f;
    if buf[0] == TT_BH_BRISC_FIRMWARE_FIRST_WORD {
        return;
    }
    eprintln!(
        "[tensix-engine] WARNING: tile NOC0 ({}, {}) TCM is non-zero \
         before bring-up (first 16 bytes: {:08x} {:08x} {:08x} {:08x}). \
         Another process — most likely tt-metal — may have loaded \
         firmware here. bhx is taking the tile over and may \
         corrupt the running workload. See \
         docs/tt-metal-coexistence.md.",
        x, y, buf[0], buf[1], buf[2], buf[3]
    );
}

/// Publish the daemon's reserved Tensix tile to
/// `$XDG_RUNTIME_DIR/bhx/<card>/reserved-tile` for tt-metal
/// coexistence. Format: a single line `<x> <y>\n` in NOC0-logical
/// coords. Best-effort — failure to write (no runtime dir, EROFS,
/// race against `daemon stop` cleanup) downgrades to a `dlog!` and
/// doesn't fail bring-up. See #74 + docs/tt-metal-coexistence.md.
fn write_reserved_tile_file(card: u32, x: u16, y: u16) {
    let path = crate::daemon::lifetime::reserved_tile_path(card);
    let body = format!("{} {}\n", x, y);
    if let Err(e) = std::fs::write(&path, body.as_bytes()) {
        crate::dlog!(
            "[tensix-engine] failed to write reserved-tile file {}: {}",
            path.display(),
            e
        );
    }
}

// Hardware-touching code lives here; the non-trivial logic (translation,
// layout) is tested in `tensix_tile` and `virtio_engine`. This module is
// mostly glue, exercised end-to-end by the soak harness rather than by
// unit tests. CI's `--no-default-features` build is the gate that
// catches feature-gated compile breakage.
