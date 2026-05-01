// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT
//
// M3 (#69) BRISC firmware — virtio-mmio register-file engine.
//
// On entry, this firmware:
//   1. Initializes a stats page at L1[0x4000] with the firmware version
//      and zeroed counters. The host can read it any time.
//   2. Initializes four virtio-mmio register files at L1[0x10000+] —
//      one per device slot. Each gets its MAGIC / VERSION / DEVICE_ID
//      / VENDOR_ID / DEVICE_FEATURES / QUEUE_NUM_MAX / STATUS planted
//      so a guest probe finds a sensible device.
//   3. Enters the main poll loop. Each iteration:
//      - For each device, snapshots the trigger registers
//        (STATUS, QUEUE_SEL, QUEUE_NOTIFY, QUEUE_READY) and compares
//        against the last-seen value. On any change, dispatch to a
//        handler that updates the per-queue shadow state, then bumps
//        the corresponding stats counter.
//      - `fence w, w` after every L1 write that the host or the
//        guest L2CPU might observe (BRISC's store-coalescing queue
//        otherwise hides the writes — see #67's hello-world finding).
//
// Why polling instead of PIC SW_INT for guest writes: the guest
// L2CPU writes to a virtio MMIO address, which lands in L1 — it
// has no way to know to additionally write a SW_INT register. Per
// the BabyRISCV docs Tensix has no L1-write-watch hardware, so
// BRISC has to poll. At 1.35 GHz with ~20 watched slots, a full
// sweep is well under 1 µs — orders of magnitude faster than
// the L2CPU's NoC RTT, which closes the QUEUE_READY race that
// motivated #66 in the first place.
//
// SW_INT is reserved for daemon → BRISC signaling (M5).

#include <stdint.h>

#include "tensix_proto.h"
#include "uart_layout.h"
#include "virtio_layout.h"
#include "shutdown_layout.h"

// Firmware version, inspected via the stats page. Format:
// `<build_id 24-bit><protocol 8-bit>`. Both sides verify a match
// before adopting a running engine across a daemon restart.
//
// `BRISC_VIRTIO_FW_BUILD_ID` is computed at compile time from
// `git log` short hash of the firmware sources (clean tree) or a
// sha256 prefix of the source bytes (dirty tree / no git). The
// Makefile computes it; the daemon's `build.rs` recomputes the same
// value and embeds it as a Rust const so adoption can compare. Any
// change to firmware sources changes the build_id, which causes
// `adopt_running` to refuse a stale chip-side firmware and force
// `tt-smi -r` reload.
//
// `TENSIX_PROTOCOL_VERSION` is the explicit wire-format protocol
// version, bumped only when the daemon↔BRISC byte layout changes.
//
// History (build_id era starts at TENSIX_PROTOCOL_VERSION = 4):
//   * Pre-build_id versions used a hand-edited 0x000601XX layout.
//     The protocol-version low byte was the only meaningful part;
//     the upper bytes were arbitrary.
#ifndef BRISC_VIRTIO_FW_BUILD_ID
#define BRISC_VIRTIO_FW_BUILD_ID 0x00000000u
#endif
#define BRISC_VIRTIO_FW_VERSION  \
    (((BRISC_VIRTIO_FW_BUILD_ID) << 8) | (TENSIX_PROTOCOL_VERSION & 0xFFu))

#define FENCE_W() __asm__ volatile("fence w, w" ::: "memory")

// ----- Stats page layout (L1 + 0x4000) -----
//
// All u32. Order is part of the wire contract (the daemon reads at
// offsets, not via a struct shared with the firmware).
#define STATS_OFF_VERSION         0x000  // BRISC_VIRTIO_FW_VERSION
#define STATS_OFF_MAGIC           0x004  // 0xB155 — "BRISC virtio loaded"
#define STATS_OFF_HEARTBEAT       0x008  // BRISC main-loop iteration count
#define STATS_OFF_STATUS_CHANGES  0x010  // count of STATUS write events
#define STATS_OFF_SEL_CHANGES     0x014  // count of QUEUE_SEL write events
#define STATS_OFF_NOTIFY_EVENTS   0x018  // count of QUEUE_NOTIFY write events
#define STATS_OFF_READY_EVENTS    0x01c  // count of QUEUE_READY write events
#define STATS_OFF_LAST_NOTIFY     0x020  // last (slot << 16 | queue_idx)
#define STATS_OFF_COMPL_EVENTS    0x024  // count of completion entries consumed
#define STATS_OFF_LAST_COMPL      0x028  // last (slot << 16 | queue_idx) consumed
#define STATS_OFF_KICK_DROPS      0x02c  // count of kick_ring_push drops (#101)
// SEL→READY race-window detector: number of times BRISC processed a
// QUEUE_SEL change while the previous SEL's QUEUE_READY was still 1.
// During that window, a guest doing `writel(SEL=N+1); readl(QUEUE_
// READY)` back-to-back can read the stale 1 and bail with -ENOENT
// from vp_modern's queue setup. Closing the window means BRISC's
// sweep beats the guest's writel→readl gap; this counter lets the
// daemon surface a non-zero count as "race window observed, sweep is
// borderline." Mirrored as `STATS_OFF_SEL_READY_RACES` on the Rust
// side. Goes with TENSIX_PROTOCOL_VERSION (no bump — the field is
// purely additive read-only stats).
#define STATS_OFF_SEL_READY_RACES 0x030
// #124 timing probe: BRISC samples `mcycle` to put real numbers on
// loop period and SEL→READY critical-path duration. Stored as the
// MAX seen since stats reset. mcycle low-32 wraps every ~3.2 s at
// 1.35 GHz; per-iteration deltas are in the hundreds-to-low-thousands
// of cycles so wrap doesn't bother subtraction (uint wraps cleanly).
//
// MAX_SWEEP_CYCLES: top-of-loop to top-of-loop. Worst case sweep
// period — the relevant number for racing the kernel's writel→readl
// gap (kernel's writel can land just after BRISC starts a slow sweep).
//
// MAX_SEL_PATH_CYCLES: handle_queue_sel_change entry to right after
// the QUEUE_READY=0 + FENCE_W. The actual time BRISC holds the
// kernel waiting on its readl response after a SEL write.
//
// LAST_SWEEP_CYCLES: most recent sweep. Helpful when MAX is suspect
// (e.g., a single early outlier from cold-cache effects).
#define STATS_OFF_MAX_SWEEP_CYCLES    0x034
#define STATS_OFF_MAX_SEL_PATH_CYCLES 0x038
#define STATS_OFF_LAST_SWEEP_CYCLES   0x03c
// Per-slot poll_one_device sub-section maxes (#124 follow-up: fence
// cuts didn't move sweep_max so we need to find what does). All in
// BRISC cycles. Each is the max observed across all (slot, iter)
// pairs since stats reset, NOT per-iter — so the three don't sum to
// max_sweep (different slots/iters can dominate each).
//   PRECAP:   poll_one_device entry → just before BLIND CAPTURE
//   BLINDCAP: BLIND CAPTURE block (7 read+writes per active queue)
//   POSTCAP:  after BLIND CAPTURE → end of poll_one_device
//
// All three are deprecated post-#120 — BLIND CAPTURE was removed in
// favour of atomic capture-on-READY=1 in `handle_queue_ready_change`,
// so PRECAP/POSTCAP no longer bracket meaningful work. Reads will be 0.
#define STATS_OFF_MAX_PRECAP_CYCLES   0x040
#define STATS_OFF_MAX_BLINDCAP_CYCLES 0x044
#define STATS_OFF_MAX_POSTCAP_CYCLES  0x048
// #120 atomic capture stats. handle_queue_ready_change snapshots the
// 7 setup fields (NUM, DESC_LO/HI, DRIVER_LO/HI, DEVICE_LO/HI) on the
// READY=0→1 transition. After the snapshot, BRISC re-reads SEL; if
// it changed mid-capture the kernel raced past us and the snapshot
// is mixed across queues — bail and bump SEL_RACES.
//
// SETUPS counts successful captures (queue activations). TEARDOWNS
// counts READY=0 events (queue disable, kernel will rewrite addrs on
// next vm_setup_vq). Together they replace the lumped READY_EVENTS
// counter for "how many virtio commands has BRISC actually processed."
#define STATS_OFF_READY_CAPTURE_SEL_RACES 0x04c
#define STATS_OFF_QUEUE_SETUPS            0x050
#define STATS_OFF_QUEUE_TEARDOWNS         0x054
// #132 TRISC1 DEVICE_FEATURES_SEL watch. Counts every observed change
// of DEVICE_FEATURES_SEL (across all slots). Used to verify the
// SEL-watch is firing during distro probes — non-zero on any cold
// boot is the expected, healthy state.
#define STATS_OFF_DEV_FEAT_SEL_CHANGES    0x058
// #156 TRISC1-side QUEUE_SEL race-window observations. Bumped each
// time TRISC1's SEL-watch loop sees a SEL change AND the visible
// QUEUE_READY for the prior SEL is still 1 (i.e. TRISC1's zero of
// READY is the cleanup the kernel needs before its post-SEL readl
// returns 0). Compare to the BRISC-side STATS_OFF_SEL_READY_RACES
// counter:
//   * BRISC counter > 0  → BOTH BRISC and TRISC1 were too slow; the
//     kernel almost certainly read READY=1 and `vm_setup_vq` returned
//     -ENOENT (counted race).
//   * BRISC counter = 0 AND TRISC1 counter > 0 → TRISC1 cleaned up
//     before BRISC saw, so BRISC's check came back to a clean slate.
//     The kernel may have raced TRISC1 silently — gap is invisible
//     from the firmware side but lives in the (TRISC1_RACES - BRISC_RACES)
//     differential.
#define STATS_OFF_TRISC1_SEL_RACES        0x05c
// Sweep-cycle histogram (#124 follow-up). MAX is misleading because
// `init_device` on STATUS=0 burns ~1200 cycles in a 320-store wipe
// loop, dominating the max even though it runs before the kernel's
// SEL→READY write sequence. The buckets give us the distribution
// shape: how many fast iters, how many medium, how many slow. Each
// bucket is a u32 counter incremented per sweep that falls in its
// range; the bench harness reads them and computes typical / p99.
//   B0: <  256 cycles  (~< 190 ns)  — idle / 0-active-slots iters
//   B1:   256-511     (~190-380 ns) — 1-2 slots, no kernel writes
//   B2:   512-1023    (~380-760 ns)
//   B3:  1024-2047    (~760-1520 ns) — busy probe, multiple handlers
//   B4: >= 2048       (~>= 1520 ns)  — the init_device outlier band
// Steady-state sweep max — like STATS_OFF_MAX_SWEEP_CYCLES but
// EXCLUDES iters where init_device fired (the kernel writing
// STATUS=0 → handle_status_change wipes 320 words = ~1240 cycles
// in PRECAP, which dominates max but is a one-shot that doesn't
// overlap the SEL→READY race window). This is the race-relevant
// number; the original max stays for context.
#define STATS_OFF_MAX_STEADY_SWEEP_CYCLES 0x068

#define STATS_MAGIC_LOADED        0x0000B155u

// VIRTIO_F_VERSION_1 lives at bit 32 of the 64-bit feature space —
// bit 0 of the high half. Stock Linux virtio drivers require this
// bit; M5.5c bumps the firmware to advertise it on a
// DEVICE_FEATURES_SEL=1 read.
#define VIRTIO_F_VERSION_1_HIGH_BIT 0x00000001u

// ----- Per-queue shadow state (BRISC-private, L1 + 0x14000) -----
//
// One block per device. Each block holds queue-indexed real storage
// for the registers that get multiplexed through QUEUE_SEL — the
// whole point of the architecture (per #66) is that we have actual
// per-queue storage rather than a single field that depends on the
// current QUEUE_SEL. This eliminates the SEL-multiplexing race that
// motivated #58, #61, #63, #65.
// Sits immediately after the reg-file region (which ends at 0x30000
// with DEVS_PER_L2CPU = 8) so shadow + reg writes stay in the same
// Tensix L1 bank — Tensix L1 is banked, and `fence w, w` is a
// hart-local store fence that does not enforce global ordering of
// stores across banks. When SHADOW_BASE was at 0x40000 (a bank apart
// from the reg files and the kick ring at 0x5000), a shadow write
// followed by a kick-ring producer-seq bump appeared out-of-order to
// the daemon: it saw the kick first and read a half-formed avail
// address, dropping the kick.
//
// Originally 0x20000 (with NUM_SLOTS=16 → reg files ended at
// 0x20000); new bump puts shadow at 0x30000 (NUM_SLOTS=32 → reg
// files end at 0x30000). Keep contiguous.
//
// Mirrored on the Rust side as `SHADOW_BASE` in `src/virtio_engine.rs`.
#define SHADOW_BASE               0x00030000u
#define SHADOW_PER_DEVICE         0x00000400u  // 1 KiB per slot

// Within a per-device shadow block, queue `q` lives at offset
// `q * SHADOW_PER_QUEUE`. Each queue holds the registers that the
// guest sets via QUEUE_DESC_LOW etc., plus our READY/NUM bookkeeping.
#define SHADOW_PER_QUEUE          0x00000040  // 64 bytes (16 u32s)
#define SHADOW_Q_OFF_NUM          0x00
#define SHADOW_Q_OFF_READY        0x04
#define SHADOW_Q_OFF_DESC_LO      0x08
#define SHADOW_Q_OFF_DESC_HI      0x0c
#define SHADOW_Q_OFF_DRIVER_LO    0x10
#define SHADOW_Q_OFF_DRIVER_HI    0x14
#define SHADOW_Q_OFF_DEVICE_LO    0x18
#define SHADOW_Q_OFF_DEVICE_HI    0x1c

// ----- Last-seen-snapshots (BRISC-private, in shadow region) -----
//
// To detect "the host wrote a new value", we keep a private snapshot
// of the trigger registers from the previous poll iteration. The
// snapshots live in the shadow block past the per-queue slots so a
// host reader looking only at the visible reg file never sees them.
#define SNAP_BASE_OFF             0x00000200  // within per-device shadow
#define SNAP_OFF_STATUS           0x00
#define SNAP_OFF_QUEUE_SEL        0x04
#define SNAP_OFF_QUEUE_NOTIFY     0x08
#define SNAP_OFF_QUEUE_READY      0x0c
#define SNAP_OFF_QUEUE_NUM        0x10
#define SNAP_OFF_DESC_LO          0x14
#define SNAP_OFF_DESC_HI          0x18
#define SNAP_OFF_DRIVER_LO        0x1c
#define SNAP_OFF_DRIVER_HI        0x20
#define SNAP_OFF_DEVICE_LO        0x24
#define SNAP_OFF_DEVICE_HI        0x28
#define SNAP_OFF_DEV_FEAT_SEL     0x2c
#define SNAP_OFF_DRV_FEAT_SEL     0x30
#define SNAP_OFF_DRV_FEAT         0x34
#define SNAP_OFF_SEL_GEN_ECHO     0x38

static inline volatile uint32_t *l1_u32(uintptr_t addr) {
    return (volatile uint32_t *)addr;
}

// Read the low 32 bits of mcycle. Tensix BRISC implements the
// standard RV32 cycle counter; csrr is a single-cycle local op.
// Wraps every ~3.2 s at 1.35 GHz — fine for our use because we only
// take per-iteration deltas (uint32 subtraction wraps cleanly), never
// absolute timestamps. See #124.
static inline uint32_t mcycle_low(void) {
    uint32_t v;
    __asm__ volatile("csrr %0, mcycle" : "=r"(v));
    return v;
}

// Bump a u32 stat to `v` if v > current. Used by the #124 timing
// probe. Plain L1 read + cmp + conditional store; no fence (we read
// the same word back, and reads on BRISC are locally consistent).
static inline void update_max_u32(uintptr_t addr, uint32_t v) {
    uint32_t cur = *l1_u32(addr);
    if (v > cur) {
        *l1_u32(addr) = v;
    }
}

static inline uint32_t read_u32(uintptr_t addr) {
    return *l1_u32(addr);
}

// Plain L1 store, NO fence. Use for BRISC-private state (snap, stats,
// ring entry payloads followed by a fenced producer-seq bump, etc.).
// All targets the kernel or daemon reads asynchronously must use
// `write_u32` (below) which includes a `fence w, w` so the store hits
// L1 before any subsequent write completes — that's the
// kernel-readability guarantee.
static inline void store_u32(uintptr_t addr, uint32_t v) {
    *l1_u32(addr) = v;
}

// Fenced store. Use ONLY when the next thing to happen is an
// externally-observed write (kick ring producer_seq bump after a
// kick entry; QUEUE_READY=0 clear on the SEL→READY race-critical
// path; reset-vector handoff at boot). Each `fence w, w` costs
// ~10-30 cycles on BRISC; chained fences (one per write) were
// behind the worst-case sweep duration before #123's diagnosis.
static inline void write_u32(uintptr_t addr, uint32_t v) {
    *l1_u32(addr) = v;
    FENCE_W();
}

// VIRTIO_NET_F_MAC: bit 5 of the low half (features[0]). Set on the
// net slot only so the kernel reads `mac` from device-config space at
// offset 0..6 instead of generating a random MAC. See #77 + the
// `extra_features_low` comment in `init_device` below.
#define VIRTIO_NET_F_MAC_BIT (1u << 5)

// Static device descriptors used to populate the per-slot reg file
// at boot. Same set for every L2CPU — each L2CPU's guest sees the
// same four virtio devices.
//
// Per-device feature bits split across the 64-bit feature space:
//   * `dev_feat_low`  → bits 0..31  (read with DEVICE_FEATURES_SEL=0)
//   * `dev_feat_high` → bits 32..63 (read with DEVICE_FEATURES_SEL=1)
//
// Pre-#132 these were collapsed into one cell, which leaked
// VIRTIO_F_VERSION_1's high-half bit into the SEL=0 read as bit 0 (=
// VIRTIO_NET_F_CSUM on the net slot). Stock kernels then negotiated
// CSUM and expected matching RX behavior (DATA_VALID handling) we
// don't actually implement. The split below + TRISC1 SEL-watch on
// DEVICE_FEATURES_SEL eliminates the leak — the visible cell holds
// the right half within ~µs of any kernel SEL write.
struct device_init {
    uint32_t device_id;
    uint32_t num_queues;       // shadow only; not visible-as-MMIO
    uint32_t dev_feat_low;
    uint32_t dev_feat_high;
};

// Indexed by `slot % BRISC_VIRTIO_DEVS_PER_L2CPU` (see `device_for_slot`).
// BLK1 / BLK2 mirror BLK exactly — they're additional blk slots used
// for cloud-init seeds (#82) and persistent data volumes (#81).
// Indices 6 and 7 are padding (DEVS_PER_L2CPU is 8 = power of two so
// the modulo stays a bitmask AND); they're unused, leaving the entries
// zeroed which keeps `init_device` planting `device_id = 0` so a
// guest probing those slots sees no device. Static-initialized
// uninitialized members of an indexed designator are zero per C99.
static const struct device_init DEVICE_TEMPLATE[BRISC_VIRTIO_DEVS_PER_L2CPU] = {
    [BRISC_VIRTIO_DEV_BLK]     = { VIRTIO_ID_BLOCK,   BRISC_VIRTIO_QUEUES_BLK,     0,                    VIRTIO_F_VERSION_1_HIGH_BIT },
    [BRISC_VIRTIO_DEV_NET]     = { VIRTIO_ID_NET,     BRISC_VIRTIO_QUEUES_NET,     VIRTIO_NET_F_MAC_BIT, VIRTIO_F_VERSION_1_HIGH_BIT },
    [BRISC_VIRTIO_DEV_CONSOLE] = { VIRTIO_ID_CONSOLE, BRISC_VIRTIO_QUEUES_CONSOLE, 0,                    VIRTIO_F_VERSION_1_HIGH_BIT },
    [BRISC_VIRTIO_DEV_RNG]     = { VIRTIO_ID_ENTROPY, BRISC_VIRTIO_QUEUES_RNG,     0,                    VIRTIO_F_VERSION_1_HIGH_BIT },
#if BRISC_VIRTIO_DEVS_PER_L2CPU >= 8
    [BRISC_VIRTIO_DEV_BLK1]    = { VIRTIO_ID_BLOCK,   BRISC_VIRTIO_QUEUES_BLK,     0,                    VIRTIO_F_VERSION_1_HIGH_BIT },
    [BRISC_VIRTIO_DEV_BLK2]    = { VIRTIO_ID_BLOCK,   BRISC_VIRTIO_QUEUES_BLK,     0,                    VIRTIO_F_VERSION_1_HIGH_BIT },
#endif
};

static const struct device_init *device_for_slot(unsigned slot) {
    return &DEVICE_TEMPLATE[slot % BRISC_VIRTIO_DEVS_PER_L2CPU];
}

static uintptr_t reg_addr(unsigned slot, unsigned reg_off) {
    return (uintptr_t)(BRISC_VIRTIO_REGS_BASE + slot * BRISC_VIRTIO_REGS_PER_DEV + reg_off);
}

static uintptr_t shadow_addr(unsigned slot, unsigned off) {
    return (uintptr_t)(SHADOW_BASE + slot * SHADOW_PER_DEVICE + off);
}

static uintptr_t shadow_queue_addr(unsigned slot, unsigned q, unsigned off) {
    return shadow_addr(slot, q * SHADOW_PER_QUEUE + off);
}

static uintptr_t snap_addr(unsigned slot, unsigned off) {
    return shadow_addr(slot, SNAP_BASE_OFF + off);
}

// ----- Initialization -----

static void zero_region(uintptr_t base, uint32_t size_bytes) {
    for (uint32_t off = 0; off < size_bytes; off += 4) {
        *l1_u32(base + off) = 0;
    }
    FENCE_W();
}

static void init_device(unsigned slot) {
    const struct device_init *d = device_for_slot(slot);

    // Wipe the standard register window (offsets 0x000..0x100) and
    // leave the device-specific config region (0x100..) ALONE. The
    // daemon writes config (e.g. virtio-blk capacity) once at
    // register_slot time; if we zero it on every STATUS=0 reset (which
    // U-Boot's virtio cleanup triggers between U-Boot's own probe and
    // the kernel's re-probe) the kernel sees capacity=0 and binds a
    // 0-sector blockdev. AlmaLinux 10 hits this path; buildroot
    // skipped it because its kernel never re-probes after a soft
    // reset. virtio 1.2 §4.2.2.2 lets device-specific config persist
    // across guest-driven STATUS=0; we exploit that.
    zero_region(reg_addr(slot, 0), VIRTIO_MMIO_CONFIG);
    // Wipe the shadow region (per-queue state + snapshots).
    zero_region(shadow_addr(slot, 0), SHADOW_PER_DEVICE);

    // Plant the read-only registers. Guest probes match magic /
    // version / device_id / vendor_id; if any of these are wrong, it
    // doesn't even consider attaching. virtio 1.2 §4.2.2.2.
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_MAGIC_VALUE)) = VIRTIO_MMIO_MAGIC;
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_VERSION))     = VIRTIO_MMIO_VERSION_2;
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_DEVICE_ID))   = d->device_id;
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_VENDOR_ID))   = BRISC_VENDOR_ID;

    // DeviceFeatures: VIRTIO_F_VERSION_1 only for M3 (we'll need to
    // negotiate device-specific features via the daemon-side bridge
    // in M5). The L2CPU drives DEVICE_FEATURES_SEL to switch between
    // the low and high 32-bit halves; for M3 we just expose the high
    // half (bit 32 = VIRTIO_F_VERSION_1) when SEL=1, zero otherwise.
    // The poll loop below handles that via a SEL-change watcher; the
    // initial register reads zero for both, until the guest writes
    // SEL=1 and we update the visible word.

    // QueueNumMax for queue 0. Multiplexed regs default to queue 0
    // until the guest writes QUEUE_SEL.
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_NUM_MAX)) = BRISC_VIRTIO_QUEUE_NUM_MAX;

    // DEVICE_FEATURES — pre-populate with both halves OR'd together.
    // TRISC1 (`trisc1_main`) watches DEVICE_FEATURES_SEL and swaps
    // the visible cell to the right half on every SEL write, but the
    // initial value before TRISC1's first poll iter has to satisfy
    // BOTH a SEL=0 and a SEL=1 read — empirical testing showed
    // pre-populating with just `dev_feat_low` lost the race against
    // U-Boot's writel(SEL=1); readl(FEATURES); on cold-start probe,
    // and the kernel set STATUS_FAILED because VIRTIO_F_VERSION_1
    // wasn't in the high half it could see. With both halves OR'd:
    //   * SEL=0 read pre-TRISC1: `dev_feat_low | VERSION_1_HIGH` —
    //     same bit-0 leak as pre-#132 firmware (CSUM on net), but
    //     TRISC1 strips it on its first swap so the leak window is
    //     transient (microseconds at most). Stock kernels reading
    //     SEL=0 after TRISC1 has swapped see only `dev_feat_low`
    //     and don't negotiate CSUM.
    //   * SEL=1 read pre-TRISC1: same value — bit 0 = VERSION_1, MAC
    //     bit 5 lands at bit 37 which kernels ignore as undefined.
    //   * Post-TRISC1 swap: cell holds the right half cleanly per
    //     SEL — no leak in either direction.
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_DEVICE_FEATURES)) =
        d->dev_feat_low | d->dev_feat_high;

    // QUEUE_NOTIFY: clear to sentinel (-1) so the first poll
    // after init_device doesn't fire a spurious "queue 0" notify
    // on the zeroed reg file. Guest writes any queue index
    // (including 0) → next poll sees value != -1 → kick fires.
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_NOTIFY)) = 0xFFFFFFFFu;

    // SW_IMPL=1 tells the patched kernel "this is a software
    // virtio backend; use the sel_generation handshake at 0x01c
    // before reading SEL-multiplexed regs." Without it stock
    // Linux's vm_setup_vq writes QUEUE_SEL=1 and immediately reads
    // QUEUE_READY — if BRISC's poll hasn't yet swapped the visible
    // reg file to queue 1's shadow (which still says READY=1 from
    // queue 0's setup), the kernel sees "queue already up" and
    // returns -ENOENT on virtio_net's TX queue.
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_SW_IMPL)) = 1u;

    // Pre-populate every queue's QueueNumMax in the shadow so a SEL
    // swap finds it without re-running init. Same value for all
    // queues at this point; per-device customization comes in M5.
    for (uint32_t q = 0; q < d->num_queues; q++) {
        *l1_u32(shadow_queue_addr(slot, q, SHADOW_Q_OFF_NUM)) = 0;  // not yet sized
        *l1_u32(shadow_queue_addr(slot, q, SHADOW_Q_OFF_READY)) = 0;
    }

    FENCE_W();
}

static void init_stats(void) {
    zero_region((uintptr_t)BRISC_VIRTIO_STATS_BASE, BRISC_VIRTIO_STATS_SIZE);
    *l1_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_VERSION) = BRISC_VIRTIO_FW_VERSION;
    *l1_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_MAGIC) = STATS_MAGIC_LOADED;
    FENCE_W();
}

// Bump a stats-page counter. NO fence — the daemon polls these at
// ms timescales, so a microsecond-delayed visibility is fine and not
// worth a per-call `fence w, w`. Pre-#123 the fence here was on the
// hot path and contributed to the sweep variance the race-window
// debugging chased for hours.
static inline void inc_stat(unsigned off) {
    volatile uint32_t *p = l1_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + off);
    *p = *p + 1u;
}

// ----- Trigger handlers -----

// Fold the guest's QUEUE_SEL write: copy queue[sel].* shadow into the
// visible-as-MMIO regs. This is the core of the SEL multiplexer; in
// the host-buffer architecture (#64) this is the moment where the
// race lived because the host's writes weren't atomic vs the guest's
// follow-up reads. Here BRISC's writes land in L1 directly, the
// guest's next NoC read picks them up, and there's no cache or store
// buffer in between.
static void handle_queue_sel_change(unsigned slot, uint32_t sel) {
    // Out-of-range queue index: leave the visible regs as-is. The
    // guest is choosing a queue we don't expose; whatever it reads
    // back is fine — just don't touch shadow with an OOB index.
    if (sel >= BRISC_VIRTIO_MAX_QUEUES) {
        return;
    }

    // #124 timing probe: t0 = entry, t1 = right after the FENCE_W
    // that publishes QUEUE_READY=0. (t1 - t0) is the race-budget
    // duration BRISC holds the kernel waiting after its writel(SEL).
    uint32_t t0 = mcycle_low();

    // Clear visible QUEUE_READY=0 FIRST, before anything else, to
    // minimize the time from "BRISC observed new SEL" to "QUEUE_READY=0
    // is visible to the kernel." Stock Linux's vm_setup_vq for queue
    // N+1 starts with `writel(SEL=N+1); readl(QUEUE_READY)` expecting
    // 0; if we don't beat the kernel's readl with this clear, it sees
    // stale 1 and bails with -ENOENT. NUM_MAX/NUM updates and the
    // race-counter bookkeeping happen after the fence — they're not
    // on the critical path for the kernel's readl response.
    //
    // Sample the prior READY value first so we can count the race
    // window: if it was 1, BRISC is processing the SEL change AFTER
    // the previous queue was set up. Daemon surfaces this counter;
    // non-zero means our sweep is borderline even if no race lost in
    // this run.
    uint32_t prev_ready = *l1_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_READY));
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_READY)) = 0;
    FENCE_W();

    uint32_t t1 = mcycle_low();
    update_max_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_MAX_SEL_PATH_CYCLES,
                   t1 - t0);

    // QueueNumMax is always BRISC_VIRTIO_QUEUE_NUM_MAX for queues we
    // support; it's 0 for queues past `num_queues` to tell the guest
    // "this queue doesn't exist." NUM comes from shadow.
    uint32_t num     = *l1_u32(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_NUM));
    uint32_t num_max = (sel < device_for_slot(slot)->num_queues) ? BRISC_VIRTIO_QUEUE_NUM_MAX : 0;
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_NUM_MAX)) = num_max;
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_NUM))     = num;

    if (prev_ready != 0) {
        inc_stat(STATS_OFF_SEL_READY_RACES);
    }
    inc_stat(STATS_OFF_SEL_CHANGES);
}

// Guest wrote QUEUE_READY for the currently-selected queue. Persist
// to shadow so a later SEL swap re-reads the right value. Also handle
// the standard virtio rule "READY=0 disables the queue" by zeroing
// the per-queue desc/avail/used pointers — the guest will rewrite
// them on the next configure cycle.
//
// On READY=1 transitions this is also the atomic snapshot point for
// the queue's setup fields (NUM, DESC/DRIVER/DEVICE LO+HI). Per
// virtio-mmio the kernel writes all setup regs BEFORE writing
// READY=1, so capturing here gives a consistent post-setup view —
// no torn LO/HI reads (#120). The earlier per-iter BLIND CAPTURE
// could write inconsistent halves into shadow[sel], and if the
// kernel advanced SEL before BLIND CAPTURE re-ran with the same
// sel, the torn state stuck around forever — that's the
// "used=0x33842000 (high half missing)" failure mode.
static void handle_queue_ready_change(unsigned slot, uint32_t sel, uint32_t ready) {
    if (sel >= BRISC_VIRTIO_MAX_QUEUES) {
        return;
    }
    if (ready != 0) {
        // Snapshot all 7 setup fields, then re-read SEL. If SEL
        // changed mid-capture, the kernel raced past us into the
        // next queue's setup window — our reads are mixed across
        // queues, so discard. Empirically the kernel's per-queue
        // setup time through the MMIO bridge is ~50 µs while the
        // capture loop is sub-µs, so this race is rare; the
        // counter exists to surface it if the margin ever shrinks.
        struct queue_field {
            unsigned mmio_off;
            unsigned shadow_off;
        };
        static const struct queue_field FIELDS[] = {
            {VIRTIO_MMIO_QUEUE_NUM,         SHADOW_Q_OFF_NUM},
            {VIRTIO_MMIO_QUEUE_DESC_LOW,    SHADOW_Q_OFF_DESC_LO},
            {VIRTIO_MMIO_QUEUE_DESC_HIGH,   SHADOW_Q_OFF_DESC_HI},
            {VIRTIO_MMIO_QUEUE_DRIVER_LOW,  SHADOW_Q_OFF_DRIVER_LO},
            {VIRTIO_MMIO_QUEUE_DRIVER_HIGH, SHADOW_Q_OFF_DRIVER_HI},
            {VIRTIO_MMIO_QUEUE_DEVICE_LOW,  SHADOW_Q_OFF_DEVICE_LO},
            {VIRTIO_MMIO_QUEUE_DEVICE_HIGH, SHADOW_Q_OFF_DEVICE_HI},
        };
        const unsigned NF = sizeof(FIELDS) / sizeof(FIELDS[0]);
        uint32_t cap[7];
        for (unsigned f = 0; f < NF; f++) {
            cap[f] = read_u32(reg_addr(slot, FIELDS[f].mmio_off));
        }
        uint32_t sel_post = read_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_SEL));
        if (sel_post == sel) {
            for (unsigned f = 0; f < NF; f++) {
                *l1_u32(shadow_queue_addr(slot, sel, FIELDS[f].shadow_off)) = cap[f];
            }
            // Force the shadow writes to commit to their L1 bank
            // before any subsequent kick reaches the daemon. fence w,w
            // (already implicit in `write_u32` for visible regs above)
            // is hart-local; it drains BRISC's store queue but does
            // not order writes across L1 banks. Shadow lives at
            // SHADOW_BASE (bank A); the kick ring + producer_seq live
            // at CTRL_BASE (bank B). Without forcing bank commit, the
            // daemon could see a producer_seq bump (bank B propagated)
            // before the shadow stores (bank A still pending) — and
            // read DESC_LO=0 with DESC_HI=0x4000 in the worst case.
            // The load below blocks until the bank acknowledges, same
            // pattern as `brisc_set_trisc0_reset`'s `(void)*reg`.
            FENCE_W();
            (void)*l1_u32(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_DEVICE_HI));
            inc_stat(STATS_OFF_QUEUE_SETUPS);
        } else {
            inc_stat(STATS_OFF_READY_CAPTURE_SEL_RACES);
        }
    }
    *l1_u32(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_READY)) = ready;
    if (ready == 0) {
        *l1_u32(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_NUM))       = 0;
        *l1_u32(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_DESC_LO))   = 0;
        *l1_u32(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_DESC_HI))   = 0;
        *l1_u32(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_DRIVER_LO)) = 0;
        *l1_u32(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_DRIVER_HI)) = 0;
        *l1_u32(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_DEVICE_LO)) = 0;
        *l1_u32(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_DEVICE_HI)) = 0;
        inc_stat(STATS_OFF_QUEUE_TEARDOWNS);
    }
    // No FENCE_W here: shadow is BRISC-private and the next kick
    // for this queue will fence inside `kick_ring_push` before the
    // producer-seq bump, which transitively orders these stores
    // against any daemon-side shadow read.
    inc_stat(STATS_OFF_READY_EVENTS);
}

// ----- M5 (#71) wire-protocol helpers -----

static inline uintptr_t ctrl_addr(unsigned off) {
    return (uintptr_t)(CTRL_BASE + off);
}

// Per-slot epoch — bumped on STATUS=0 reset so kicks pre-dating the
// reset can be filtered out by the daemon. BRISC-private; lives in
// the shadow region.
static inline uintptr_t epoch_addr(unsigned slot) {
    return shadow_addr(slot, SNAP_BASE_OFF + 0x10);
}

// Append one KickEntry to the L1 kick ring and bump the producer
// counter. The daemon polls `producer_seq` via the chip-side TLB
// and drains entries in `[consumer_seq..producer_seq)`. Same SPSC
// pattern as a virtio split virtqueue, but with the ring in BRISC
// L1 rather than guest DRAM.
//
// Pre-#101 we wrote unconditionally; under daemon backpressure (slow
// `process_one_chain_for_queue` on a disk stall etc.) BRISC would
// overwrite unread entries and the daemon would consume garbage
// re-runs of the ring. Now we check fullness first: if the ring is
// already at `KICK_RING_ENTRIES - 1` outstanding, drop the kick and
// bump `STATS_OFF_KICK_DROPS` so the daemon can surface the
// pressure rather than silently corrupting state. We don't block —
// BRISC is preemption-free and a stalled daemon will only get worse
// if firmware also stalls.
static void kick_ring_push(unsigned slot, uint32_t queue_idx) {
    uint32_t seq = read_u32(ctrl_addr(CTRL_OFF_KICK_RING_HDR + KICK_HDR_OFF_PRODUCER_SEQ));
    uint32_t consumer = read_u32(ctrl_addr(CTRL_OFF_KICK_RING_HDR + KICK_HDR_OFF_CONSUMER_SEQ));
    uint32_t outstanding = seq - consumer;
    if (outstanding >= KICK_RING_ENTRIES) {
        uint32_t drops = read_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_KICK_DROPS);
        // Stat counter — no fence; daemon reads at ms timescale.
        store_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_KICK_DROPS, drops + 1u);
        return;
    }
    uint32_t epoch = read_u32(epoch_addr(slot));
    uint32_t idx = seq & (KICK_RING_ENTRIES - 1u);
    uintptr_t entry = ctrl_addr(CTRL_OFF_KICK_RING + idx * KICK_ENTRY_SIZE);
    *l1_u32(entry + KICK_ENTRY_OFF_SLOT) = ((uint32_t)slot & 0xFFFFu) | ((queue_idx & 0xFFFFu) << 16);
    *l1_u32(entry + KICK_ENTRY_OFF_SEQ) = seq;
    *l1_u32(entry + KICK_ENTRY_OFF_EPOCH) = epoch;
    FENCE_W();
    // Bump the producer AFTER the entry is fully written so a
    // racing daemon read either misses the entry entirely or sees
    // it in a consistent state.
    write_u32(ctrl_addr(CTRL_OFF_KICK_RING_HDR + KICK_HDR_OFF_PRODUCER_SEQ), seq + 1u);
}

// Guest wrote QUEUE_NOTIFY=q. Append a KickEntry to the kick ring so
// the daemon's poll loop wakes up and drains the device's avail
// ring. Also records (slot, q) in the stats page for diagnostics.
static void handle_queue_notify(unsigned slot, uint32_t q) {
    *l1_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_LAST_NOTIFY) =
        ((uint32_t)slot << 16) | (q & 0xFFFFu);
    kick_ring_push(slot, q);
    inc_stat(STATS_OFF_NOTIFY_EVENTS);
}

// Guest wrote STATUS. If 0, the device is being reset — wipe the per-
// device state, reinitialize the read-only regs, and bump the per-
// slot epoch so any kicks recorded for the previous incarnation are
// filterable on the daemon side.
// Set when handle_status_change fires init_device this main-loop
// iter, so the top-of-loop steady-state max doesn't get poisoned by
// the ~1240-cycle wipe. Cleared after the per-iter max update.
static int init_device_fired_this_iter;

static void handle_status_change(unsigned slot, uint32_t status, uint32_t prev) {
    (void)prev;
    if (status == 0) {
        uint32_t e = read_u32(epoch_addr(slot));
        init_device(slot);
        // init_device wipes the shadow region, including epoch — so
        // re-establish it after. Daemon reads epoch on the next
        // kick; the kick path's own fence orders this for it. No
        // local fence.
        store_u32(epoch_addr(slot), e + 1u);
        init_device_fired_this_iter = 1;
    }
    inc_stat(STATS_OFF_STATUS_CHANGES);
}

// Drain the completion ring (daemon → BRISC). Each entry tells us
// "slot S queue Q has a new used_idx; please IRQ the L2CPU." In
// this M5 first cut we only record the event in stats; the actual
// PLIC IRQ to the L2CPU stays daemon-driven (the NIU register
// dance for BRISC-side NoC writes lands in a follow-up).
static void poll_completion_ring(void) {
    uint32_t producer = read_u32(ctrl_addr(CTRL_OFF_COMPL_RING_HDR + COMPL_HDR_OFF_PRODUCER_SEQ));
    uint32_t consumer = read_u32(ctrl_addr(CTRL_OFF_COMPL_RING_HDR + COMPL_HDR_OFF_CONSUMER_SEQ));
    if (consumer == producer) {
        // Steady state: nothing to drain. Skip the publish — writing
        // the same CONSUMER_SEQ back with a FENCE_W on every main-loop
        // iteration is pure overhead and widens the SEL→READY race
        // window we're trying to shrink (#123).
        return;
    }
    do {
        uint32_t idx = consumer & (COMPL_RING_ENTRIES - 1u);
        uintptr_t entry = ctrl_addr(CTRL_OFF_COMPL_RING + idx * COMPL_ENTRY_SIZE);
        uint32_t slot_q = *l1_u32(entry);  // [15:0] slot, [31:16] queue
        // Stash for diagnostics; same packed format as LAST_NOTIFY.
        *l1_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_LAST_COMPL) = slot_q;
        inc_stat(STATS_OFF_COMPL_EVENTS);
        consumer += 1u;
    } while (consumer != producer);
    // No fence: the daemon polls CONSUMER_SEQ for backpressure
    // diagnostics, not for ordering with anything BRISC writes after.
    // Microsecond visibility delay is fine.
    store_u32(ctrl_addr(CTRL_OFF_COMPL_RING_HDR + COMPL_HDR_OFF_CONSUMER_SEQ), consumer);
}

// ----- Main poll loop -----

static void poll_one_device(unsigned slot) {
    // STATUS — RW. Detect by snapshot diff.
    uint32_t status = read_u32(reg_addr(slot, VIRTIO_MMIO_STATUS));
    uint32_t status_prev = read_u32(snap_addr(slot, SNAP_OFF_STATUS));
    if (status != status_prev) {
        handle_status_change(slot, status, status_prev);
        // Snap is BRISC-private; no fence needed. The next sweep's
        // diff against snap is local to this hart so ordering is
        // automatic; the daemon never reads snap_addr.
        store_u32(snap_addr(slot, SNAP_OFF_STATUS), status);
    }

    // QUEUE_SEL — W. We don't reset visible-as-MMIO QUEUE_SEL, so
    // detection is the same diff approach.
    uint32_t sel = read_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_SEL));
    uint32_t sel_prev = read_u32(snap_addr(slot, SNAP_OFF_QUEUE_SEL));
    if (sel != sel_prev) {
        handle_queue_sel_change(slot, sel);
        store_u32(snap_addr(slot, SNAP_OFF_QUEUE_SEL), sel);
    }

    // QUEUE_READY — RW. Must run BEFORE QUEUE_NOTIFY: the kernel
    // virtio-mmio sequence writes READY=1 then NOTIFY in tight
    // succession, and BRISC observes both in a single sweep. The
    // READY handler atomically captures the queue setup fields into
    // shadow (#120); the NOTIFY handler pushes a kick-ring entry
    // with FENCE_W around the producer-seq bump. Running NOTIFY
    // first published the kick to the daemon BEFORE the capture
    // committed shadow, so the daemon read the kick and saw stale
    // setup values — the pointers-out-of-range drop seen in soak.
    // Running READY first means the capture writes precede the
    // NOTIFY-handler's fence, which transitively orders them
    // against the daemon's view of `producer_seq`.
    //
    // Persisting any non-zero write to shadow then clears the
    // visible reg back to 0. The legacy host-buffer path
    // (src/virtio/mod.rs in the SEL/READY handshake) does the exact
    // same eager clear: the kernel's vm_setup_vq for queue N+1 starts
    // with `writel(QUEUE_SEL=N+1); readl(QUEUE_READY)` expecting 0 —
    // but the visible reg still holds queue N's READY=1 from the
    // immediately-prior setup. Without clearing, the kernel sees 1
    // and bails with -ENOENT ("Queue shouldn't already be set up").
    // Snapshot diff alone isn't enough: the kernel writes 1 once,
    // we'd see the change and persist, but the visible reg keeps the
    // 1 forever until something resets it. By zeroing on every poll
    // (regardless of snapshot state), we keep the visible reg clean
    // for the next vm_setup_vq cycle and rely on shadow for the
    // "is this queue ready?" question that dispatch_chain asks.
    uint32_t ready = read_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_READY));
    if (ready != 0) {
        handle_queue_ready_change(slot, sel, ready);
        *l1_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_READY)) = 0;
        FENCE_W();
    }

    // QUEUE_NOTIFY — W. The guest writes the queue index here. We
    // can't use snapshot-diff because the kernel writes the same
    // queue index repeatedly (queue 0 → queue 0 → ...) and a diff
    // watcher would only see the first one. Instead, the firmware
    // CLEARS QUEUE_NOTIFY to a sentinel (-1) after each fire; any
    // value other than -1 in the visible reg means a guest write
    // happened. Initial state is set to -1 in `init_device` so a
    // pristine reg file doesn't fire a spurious zero-queue notify
    // on first poll.
    //
    // Race window: if the guest writes NOTIFY=N, BRISC reads N
    // (fires), then guest writes NOTIFY=N again before BRISC's
    // sentinel write lands, BRISC's next poll sees the second N
    // and fires correctly. If the guest writes NOTIFY=N a third
    // time within the same BRISC poll iteration, the third one
    // races with the sentinel write — we accept this rare miss in
    // exchange for exact-most-of-the-time semantics. Kernel
    // virtio_blk doesn't burst notifies that tightly in practice.
    uint32_t notify = read_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_NOTIFY));
    if (notify != 0xFFFFFFFFu) {
        handle_queue_notify(slot, notify);
        write_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_NOTIFY), 0xFFFFFFFFu);
    }

    // DEVICE_FEATURES is statically set in `init_device` to
    // VIRTIO_F_VERSION_1_HIGH_BIT (0x1) and not touched per-poll.
    //
    // The naive "poll DEVICE_FEATURES_SEL, write the right half"
    // pattern races with stock Linux virtio-mmio drivers, which
    // do `writel(SEL=N); readl(FEATURES);` back-to-back within
    // ~µs — comparable to BRISC's full 16-slot sweep period.
    // The kernel's read can land before BRISC observes the SEL
    // write. With static 0x1 in DEVICE_FEATURES regardless of
    // SEL:
    //   * SEL=1 (high half) read returns 0x1 ⟹ bit 32 of the
    //     combined 64-bit word ⟹ VIRTIO_F_VERSION_1 set ✓
    //   * SEL=0 (low half) read returns 0x1 ⟹ bit 0 set, which
    //     no current device defines as a feature. Drivers ignore
    //     unknown bits when computing DRIVER_FEATURES.
    // Net effect: kernel sees VIRTIO_F_VERSION_1, negotiation
    // succeeds. We keep the SEL snapshot below for diagnostic
    // purposes.

    // DRIVER_FEATURES_SEL — guest writes 0 or 1, then writes the
    // 32-bit half it has accepted via DRIVER_FEATURES. We don't
    // need to take any visible action (the guest's STATUS=FEATURES_OK
    // write later confirms negotiation), but we do snapshot the
    // value so the daemon can read what was negotiated.
    uint32_t dfd_sel = read_u32(reg_addr(slot, VIRTIO_MMIO_DRIVER_FEATURES_SEL));
    uint32_t dfd_sel_prev = read_u32(snap_addr(slot, SNAP_OFF_DRV_FEAT_SEL));
    if (dfd_sel != dfd_sel_prev) {
        store_u32(snap_addr(slot, SNAP_OFF_DRV_FEAT_SEL), dfd_sel);
    }
    uint32_t dfd = read_u32(reg_addr(slot, VIRTIO_MMIO_DRIVER_FEATURES));
    uint32_t dfd_prev = read_u32(snap_addr(slot, SNAP_OFF_DRV_FEAT));
    if (dfd != dfd_prev) {
        store_u32(snap_addr(slot, SNAP_OFF_DRV_FEAT), dfd);
    }

    // Per-queue setup registers (NUM + DESC/DRIVER/DEVICE LO+HI) are
    // captured atomically in `handle_queue_ready_change` on the
    // READY=0→1 transition, not per-iter. Per-iter BLIND CAPTURE used
    // to live here but could write LO-fresh / HI-stale shadow halves
    // and then leave them stuck if SEL advanced before re-capture
    // (#120). Capture-on-READY=1 is the only point where the kernel
    // guarantees the full setup is committed.

    // sel_generation handshake — done last so any SEL/SEL-multiplexed
    // register update above has already taken effect by the time the
    // guest sees the echo. The patched kernel writes (prev+1) and
    // spins until it reads back something different; we ack by writing
    // (curr+1), which is guaranteed to differ. See `echo_sel_generation`
    // in src/virtio/mod.rs for the matching legacy-path helper.
    uint32_t curr_gen = read_u32(reg_addr(slot, VIRTIO_MMIO_SEL_GENERATION));
    uint32_t last_echoed = read_u32(snap_addr(slot, SNAP_OFF_SEL_GEN_ECHO));
    if (curr_gen != last_echoed) {
        uint32_t next = curr_gen + 1u;
        // Visible reg: kernel may read after a SEL write to detect
        // the echo. Keep fenced.
        write_u32(reg_addr(slot, VIRTIO_MMIO_SEL_GENERATION), next);
        // Snap: BRISC-private. No fence.
        store_u32(snap_addr(slot, SNAP_OFF_SEL_GEN_ECHO), next);
    }
}

// ----- M6 (#78) 16550 UART emulation, TX-only -----
//
// One UART per L2CPU. The reg file lives at L1
// `BRISC_UART_BASE + l2cpu_idx*BRISC_UART_PER_L2CPU_STRIDE` and is
// covered by the L2CPU's existing engine TLB window at offset
// `BRISC_UART_OFFSET_FROM_ENGINE_BASE` from its base. The DTB emits
// an `ns16550a` node at that PA; the kernel's 8250 driver binds it
// as `ttyS0` and writes boot output there.
//
// M6.1 (#79) split: TRISC0 polls the UART reg files and feeds bytes
// through a per-L2CPU SPSC ring in BRISC L1; BRISC drains the rings
// and pushes kick-ring entries to the daemon. Same on-the-wire shape
// as M6 (slot=BRISC_KICK_UART_SLOT_BASE+idx, byte in queue_idx); the
// dedicated TRISC0 sweep closes the LSR-write race that drove the
// 30–60% byte-loss observed on BRISC-only polling.
//
// RX is intentionally not implemented. A static MMIO reg file can't
// observe the kernel's RBR reads, so it can't safely advance an RX
// FIFO. LSR.DR stays 0 forever and the kernel never reads RBR. A
// follow-up can add metered-delivery RX without touching this layout.

static inline uintptr_t uart_reg_addr(unsigned l2cpu_idx, unsigned reg_off) {
    return brisc_uart_regs_base(l2cpu_idx) + reg_off;
}

// Plant the static reg state every guest expects to see post-reset:
// THR holding the sentinel, LSR with THRE+TEMT (TX always ready),
// MSR with CTS+DSR (good link), MCR with DTR+RTS+OUT2 (typical post-
// reset), LCR=0x03 (8N1), IIR with the 16550A FIFO indicator + the
// no-pending-interrupt bit, IER/SCR zeroed. Reg-shift=2 means each
// 8-bit register occupies a 4-byte cell; we pre-init the whole 4 KiB
// to zero so any access past the active register set returns zero.
//
// BRISC owns the reg-file initialization (one-shot at boot, plus
// re-init on TRISC0 lifecycle changes if ever needed). TRISC0 only
// polls; it never plants the static regs.
static void uart_init_one(unsigned l2cpu_idx) {
    uintptr_t base = brisc_uart_regs_base(l2cpu_idx);
    for (unsigned off = 0; off < BRISC_UART_REG_FILE_SIZE; off += 4) {
        *l1_u32(base + off) = 0;
    }
    // RBR/THR sentinel — TRISC0's TX poll uses this as the "no fresh
    // byte from guest" mark.
    *l1_u32(base + UART_REG_RBR_THR) = BRISC_UART_THR_SENTINEL;
    *l1_u32(base + UART_REG_LCR)     = UART_LCR_8N1;
    *l1_u32(base + UART_REG_MCR)     = UART_MCR_DTR_RTS_OUT2;
    *l1_u32(base + UART_REG_LSR)     = UART_LSR_THRE | UART_LSR_TEMT;
    *l1_u32(base + UART_REG_MSR)     = UART_MSR_CTS | UART_MSR_DSR;
    *l1_u32(base + UART_REG_IIR_FCR) = UART_IIR_NO_FIFO | UART_IIR_NO_INT;
    FENCE_W();

    // Per-L2CPU UART feed ring + headers — see uart_layout.h for the
    // 0x100-byte block layout. Zero the whole region at boot so we
    // start with producer = consumer = drop_count = 0 and a clean
    // ring. TRISC0 producer / BRISC consumer indices wrap modulo
    // BRISC_UART_FEED_RING_ENTRIES.
    uintptr_t priv = brisc_uart_private_base(l2cpu_idx);
    for (unsigned off = 0; off < BRISC_UART_PRIVATE_PER_L2CPU; off += 4) {
        *l1_u32(priv + off) = 0;
    }
    FENCE_W();
}

static void uart_init_devices(void) {
    for (unsigned i = 0; i < BRISC_KICK_UART_NUM_SLOTS; i++) {
        uart_init_one(i);
    }
}

// ----- TRISC0 UART poll (M6.1 #79 Phase B) -----
//
// trisc0_uart_poll_one: same logic as the old BRISC version, but
// instead of pushing to the kick ring it produces to a per-L2CPU
// SPSC byte-feed ring. BRISC drains the ring and forwards bytes.
//
// State machine per UART:
//   IDLE:  cell == sentinel, LSR.THRE=1; ready for the next guest
//          write. On observing a fresh byte: drop LSR=0 immediately,
//          handle DLAB/divisor, otherwise enqueue into the feed ring,
//          reset the sentinel, arm hold-down.
//   ARMED: byte enqueued, LSR.THRE=0, hold counter > 0; each sweep
//          decrements, and on hitting zero we restore LSR.THRE+TEMT.
//
// The dedicated TRISC0 loop sweeps just the 4 UARTs (no virtio reg
// files), so the per-UART revisit cadence is well under 1 µs even
// after pessimistic instruction count + L1 latency. That's faster
// than the L2CPU's `writel(THR); readl(LSR);` loop iteration time
// (~200 ns per write + ~50 ns NoC RTT for LSR), so the THRE=0 store
// reliably lands before the kernel's next LSR poll → no byte loss.
static void trisc0_uart_feed_push(unsigned l2cpu_idx, uint8_t byte) {
    uintptr_t priv = brisc_uart_private_base(l2cpu_idx);
    uint32_t producer = *l1_u32(priv + UART_PRIV_OFF_FEED_PRODUCER_SEQ);
    uint32_t consumer = *l1_u32(priv + UART_PRIV_OFF_FEED_CONSUMER_SEQ);
    if (producer - consumer >= BRISC_UART_FEED_RING_ENTRIES) {
        // Ring full — drop the byte and bump the per-L2CPU drop
        // counter. With THRE held at 0 the kernel won't write more,
        // but the existing in-flight byte the kernel saw THRE=1 for
        // is what we'd have to drop. The drop counter tells the
        // daemon if the ring needs to grow.
        uintptr_t drop_addr = priv + UART_PRIV_OFF_FEED_DROP_COUNT;
        uint32_t d = *l1_u32(drop_addr);
        *l1_u32(drop_addr) = d + 1u;
        FENCE_W();
        return;
    }
    uint32_t idx = producer & BRISC_UART_FEED_RING_MASK;
    uintptr_t slot_addr = priv + UART_PRIV_OFF_FEED_RING + idx * 4u;
    // Tensix LSU quirk (per `BabyRISCV/MemoryOrdering.md`'s mailbox
    // example): `fence w,w` drains the store queue but doesn't
    // guarantee writes to different L1 banks are *processed* in
    // program order. The canonical fix is store, load-back, **consume
    // the load result** (creating a true data dependency that forces
    // the LSU to wait for the load to complete, which can only happen
    // once the prior store has been processed by L1), then store
    // again.
    //
    // Inline asm because plain C `(void)*l1_u32(...)` doesn't express
    // the consume — GCC's scheduler issues the load but doesn't make
    // the next store wait for it. We want exactly:
    //   sw  byte,         0(slot_addr)
    //   lw  back,         0(slot_addr)
    //   <use of `back` before next store>
    //   sw  producer+1,   0(producer_seq_addr)
    // Consume-the-result trick must be a real instruction, not just an
    // asm constraint — `__asm__("" :: "r"(back))` doesn't emit code, so
    // the LSU has nothing to stall on. `addi x0, t, 0` reads `t` and
    // discards into x0; that real instruction creates the data
    // dependency the LSU needs to wait for the load to retire (and
    // therefore for the prior store to be processed by L1) before
    // moving on to the next store.
    uint32_t back;
    __asm__ volatile(
        "sw   %1, 0(%2)\n\t"
        "lw   %0, 0(%2)\n\t"
        "addi x0, %0, 0\n\t"
        : "=&r"(back)
        : "r"((uint32_t)byte), "r"(slot_addr)
        : "memory");
    *l1_u32(priv + UART_PRIV_OFF_FEED_PRODUCER_SEQ) = producer + 1u;
    FENCE_W();
}

static void trisc0_uart_poll_one(unsigned l2cpu_idx) {
    uintptr_t cell = uart_reg_addr(l2cpu_idx, UART_REG_RBR_THR);
    uintptr_t lsr_addr = uart_reg_addr(l2cpu_idx, UART_REG_LSR);
    uintptr_t hold_addr = brisc_uart_private_base(l2cpu_idx) + UART_PRIV_OFF_HOLD;

    uint32_t hold = *l1_u32(hold_addr);
    if (hold > 0) {
        hold -= 1;
        *l1_u32(hold_addr) = hold;
        if (hold == 0) {
            *l1_u32(lsr_addr) = UART_LSR_THRE | UART_LSR_TEMT;
            FENCE_W();
        }
        return;
    }

    uint32_t v = *l1_u32(cell);
    if (v == BRISC_UART_THR_SENTINEL) {
        return;
    }
    // Drop THRE+TEMT immediately so the kernel's next LSR poll sees
    // a busy transmitter and stalls.
    *l1_u32(lsr_addr) = 0;
    FENCE_W();
    // Guard against the LCR.DLAB=1 divisor-latch dance: when the
    // kernel sets DLAB and writes DLL/DLM, those writes land in this
    // cell. Don't enqueue them as TX bytes. Pattern is `write
    // LCR=0x83; write DLL; write DLM; write LCR=0x03;` — between
    // DLAB-set and DLAB-clear, treat any THR write as a divisor byte
    // (silently consume + reset sentinel).
    uint32_t lcr = *l1_u32(uart_reg_addr(l2cpu_idx, UART_REG_LCR));
    if (lcr & UART_LCR_DLAB) {
        *l1_u32(cell) = BRISC_UART_THR_SENTINEL;
        FENCE_W();
        *l1_u32(lsr_addr) = UART_LSR_THRE | UART_LSR_TEMT;
        FENCE_W();
        return;
    }
    uint8_t byte = (uint8_t)(v & 0xFFu);
    *l1_u32(cell) = BRISC_UART_THR_SENTINEL;
    FENCE_W();
    trisc0_uart_feed_push(l2cpu_idx, byte);
    // Hold THRE=0 for a few more sweeps so the L2CPU's LSR read has
    // a wide-enough wall-clock window to observe it.
    *l1_u32(hold_addr) = TRISC0_UART_THRE_HOLD_SWEEPS;
}

// ----- M6.1 (#79) TRISC0 lifecycle (BRISC-owned) -----
//
// BRISC asserts/de-asserts TRISC0's soft-reset bit based on the UART
// portion of the active-slots bitmap. When any of bits 16..19 is set,
// at least one L2CPU has a UART registered → release TRISC0 → it
// enters `trisc0_main` (Phase A: heartbeat; Phase B: UART poll). When
// all clear, re-assert TRISC0's reset → its instruction stream stops
// mid-instruction (which is fine; no in-flight UART bytes can exist
// when no UART is registered).
//
// The host's `bring_up` programs TRISC0's reset PC override register
// before `release_brisc_only` so that whenever BRISC clears bit 12
// here, TRISC0 enters `trisc0_reset_entry` (in `start.S`).
//
// Why BRISC instead of the host: ownership cleanup. With this, the
// daemon never directly touches TRISC0's reset state — TRISC0 is a
// BRISC-internal implementation detail behind the kick-ring +
// (Phase B) byte-feed-ring abstraction. The lifecycle is exactly
// aligned with the bitmap state, which is how an operator already
// thinks about UART-on-this-L2CPU.

#define TENSIX_SOFT_RESET_ADDR    0xFFB121B0u
#define SOFT_RESET_TRISC0         (1u << 12)
#define SOFT_RESET_TRISC1         (1u << 13)

static int trisc0_running = 0;
static int trisc1_running = 0;

// Read-modify-write the soft-reset register from within the tile.
// The register is per-tile MMIO at 0xFFB121B0, accessible as a regular
// load/store target from any baby RISC. We preserve all other bits —
// in particular BRISC's own bit 11 stays clear (otherwise BRISC would
// halt itself mid-instruction). Read-back flushes the write so the
// per-core controller observes it on the next clock.
static void brisc_set_trisc0_reset(int asserted) {
    volatile uint32_t *reg = (volatile uint32_t *)TENSIX_SOFT_RESET_ADDR;
    uint32_t v = *reg;
    if (asserted) {
        v |= SOFT_RESET_TRISC0;
    } else {
        v &= ~SOFT_RESET_TRISC0;
    }
    *reg = v;
    FENCE_W();
    (void)*reg;
}

// Drive TRISC0's reset state from the bitmap. Called once per BRISC
// poll-loop iteration. State edges only — the RMW above is several
// instructions plus a NoC round-trip, so we don't want to hammer it
// every sweep when no edge fires.
static void brisc_drive_trisc0_lifecycle(uint32_t active_slots) {
    int want_running = (active_slots & BRISC_UART_SLOT_MASK) != 0;
    if (want_running && !trisc0_running) {
        brisc_set_trisc0_reset(0);
        trisc0_running = 1;
    } else if (!want_running && trisc0_running) {
        brisc_set_trisc0_reset(1);
        trisc0_running = 0;
    }
}

static void brisc_set_trisc1_reset(int asserted) {
    volatile uint32_t *reg = (volatile uint32_t *)TENSIX_SOFT_RESET_ADDR;
    uint32_t v = *reg;
    if (asserted) {
        v |= SOFT_RESET_TRISC1;
    } else {
        v &= ~SOFT_RESET_TRISC1;
    }
    *reg = v;
    FENCE_W();
    (void)*reg;
}

// TRISC1 lifecycle (#125): release when ANY virtio slot is active
// (i.e. any non-UART bit in active_slots). UART bits (16..19) don't
// concern TRISC1 — its job is the SEL→READY race-critical clear,
// which is virtio-only.
static void brisc_drive_trisc1_lifecycle(uint32_t active_slots) {
    uint32_t virtio_active = active_slots & ~BRISC_UART_SLOT_MASK;
    int want_running = virtio_active != 0;
    if (want_running && !trisc1_running) {
        brisc_set_trisc1_reset(0);
        trisc1_running = 1;
    } else if (!want_running && trisc1_running) {
        brisc_set_trisc1_reset(1);
        trisc1_running = 0;
    }
}

// Guest-OS shutdown poll (#94). For each registered shutdown slot
// (one per L2CPU, 4 max), check whether the guest has written a
// magic value to the per-L2CPU shutdown command register. On match,
// push a kick-ring entry with a reserved slot id (20..23) and the
// command kind in the queue_idx field, then clear the cell back to
// the sentinel so we don't re-fire on the next sweep.
//
// Currently only POWEROFF (kind=0) is acted on by the daemon.
// REBOOT (kind=1) is recognized at the firmware level so the BRISC
// side won't change shape when the reboot follow-up (#141) lands —
// daemon-side dispatch will route it to a re-run of the boot
// pipeline. Unknown magics are clear-and-ignore; an operator could
// see them via the kick ring's drop counter if they ever fire.
//
// Cost: one L1 read per active L2CPU per sweep. With 4 L2CPUs that's
// ~20 BRISC cycles when all are active, ~5 cycles in the
// no-shutdown-slots-active mask check that gates entry. Sub-µs.
static void poll_shutdown_slots(uint32_t active) {
    if ((active & BRISC_SHUTDOWN_SLOT_MASK) == 0u) {
        return;
    }
    for (unsigned i = 0; i < BRISC_KICK_SHUTDOWN_NUM_SLOTS; i++) {
        unsigned slot = BRISC_KICK_SHUTDOWN_SLOT_BASE + i;
        if (((active >> slot) & 1u) == 0u) {
            continue;
        }
        uintptr_t reg = brisc_shutdown_regs_base(i) + BRISC_SHUTDOWN_OFF_COMMAND;
        uint32_t cmd = read_u32(reg);
        if (cmd == BRISC_SHUTDOWN_SENTINEL) {
            continue;
        }
        uint32_t kind;
        if (cmd == BRISC_SHUTDOWN_MAGIC_POWEROFF) {
            kind = 0u;
        } else if (cmd == BRISC_SHUTDOWN_MAGIC_REBOOT) {
            kind = 1u;
        } else {
            // Unknown magic — clear and ignore. Don't push a kick;
            // we don't want to teardown an L2CPU on a glitch / stray
            // write to this address from a misbehaving guest.
            *l1_u32(reg) = BRISC_SHUTDOWN_SENTINEL;
            FENCE_W();
            continue;
        }
        kick_ring_push(slot, kind);
        *l1_u32(reg) = BRISC_SHUTDOWN_SENTINEL;
        FENCE_W();
    }
}

// ----- TRISC0 entry (M6.1 #79 Phase B) -----
//
// Steady-state UART poll loop. TRISC0 is held in soft reset until
// BRISC observes a non-zero UART_SLOT_MASK in the active-slots
// bitmap; once released, this is the loop it runs.
//
// Each iteration:
//   1. Read CTRL_OFF_ACTIVE_SLOTS (BRISC L1) to know which UARTs are
//      registered.
//   2. For each registered UART, call `trisc0_uart_poll_one`. The
//      poll handles THRE backpressure + DLAB guard + feed-ring push.
//   3. Bump the heartbeat slot every N iterations (gives BRISC a
//      lightweight liveness signal without fenc'ing on every sweep).
//
// Why bump the heartbeat every N (vs every iteration): a fenced L1
// store on every sweep eats the cycles we just bought by moving off
// BRISC. Heartbeat is observed at human timescales (debug
// inspections), 1 in 1024 sweeps is plenty.
void trisc0_main(void) {
    volatile uint32_t *active_p = (volatile uint32_t *)
        (uintptr_t)(CTRL_BASE + CTRL_OFF_ACTIVE_SLOTS);
    volatile uint32_t *hb = (volatile uint32_t *)
        (uintptr_t)(BRISC_TRISC0_GLOBAL_BASE + TRISC0_GLOBAL_OFF_HEARTBEAT);
    uint32_t c = 0;
    for (;;) {
        uint32_t active = *active_p;
        for (unsigned i = 0; i < BRISC_KICK_UART_NUM_SLOTS; i++) {
            unsigned slot = BRISC_KICK_UART_SLOT_BASE + i;
            if (((active >> slot) & 1u) == 0) {
                continue;
            }
            trisc0_uart_poll_one(i);
        }
        c += 1u;
        if ((c & 0x3FFu) == 0) {
            *hb = c;
            FENCE_W();
        }
    }
}

// ----- M5 (#71) handshake -----

// Initialize the control-plane region: zero the hello/hello-ack/
// kick-ring/compl-ring slots, then publish ring sizes. Daemon must
// not write its hello until BRISC has done this — but in practice
// the daemon waits for the M3 stats-magic before issuing hello, and
// stats-magic is published only after `init_proto`, so the
// ordering is enforced by construction.
static void init_proto(void) {
    for (unsigned off = 0; off < CTRL_SIZE; off += 4) {
        *l1_u32(ctrl_addr(off)) = 0;
    }
    *l1_u32(ctrl_addr(CTRL_OFF_KICK_RING_HDR + KICK_HDR_OFF_RING_ENTRIES)) = KICK_RING_ENTRIES;
    *l1_u32(ctrl_addr(CTRL_OFF_COMPL_RING_HDR + COMPL_HDR_OFF_RING_ENTRIES)) = COMPL_RING_ENTRIES;
    FENCE_W();
}

// Wait for the daemon to write a hello with the magic + protocol
// version, then write a hello-ack reflecting our protocol +
// firmware versions. This is one-shot (we don't re-handshake on
// device reset — the daemon does the engine bring-up once per
// daemon lifetime).
static void wait_for_hello_and_ack(void) {
    for (;;) {
        uint32_t magic = read_u32(ctrl_addr(CTRL_OFF_HELLO + HELLO_OFF_MAGIC));
        if (magic == HELLO_MAGIC) {
            break;
        }
    }
    // Read the daemon's protocol version (we don't act on it yet —
    // the daemon checks our version in its own handshake), then
    // publish hello-ack.
    (void)read_u32(ctrl_addr(CTRL_OFF_HELLO + HELLO_OFF_PROTOCOL_VERSION));
    *l1_u32(ctrl_addr(CTRL_OFF_HELLO_ACK + HELLO_ACK_OFF_PROTOCOL_VERSION)) = TENSIX_PROTOCOL_VERSION;
    *l1_u32(ctrl_addr(CTRL_OFF_HELLO_ACK + HELLO_ACK_OFF_FIRMWARE_VERSION)) = BRISC_VIRTIO_FW_VERSION;
    FENCE_W();
    // Magic last — daemon polls for this transitioning non-zero.
    write_u32(ctrl_addr(CTRL_OFF_HELLO_ACK + HELLO_ACK_OFF_MAGIC), HELLO_ACK_MAGIC);
}

void main(void) {
    init_stats();
    init_proto();
    for (unsigned slot = 0; slot < BRISC_VIRTIO_NUM_SLOTS; slot++) {
        init_device(slot);
    }
    uart_init_devices();

    // Block on the daemon's hello before entering the steady-state
    // poll loop. Without this, a guest could (in theory) start
    // probing before the daemon has agreed on the protocol version
    // — the L1 reg files are already initialized by init_device, so
    // the device looks valid, but the kick FIFO would have no
    // consumer.
    wait_for_hello_and_ack();

    uint32_t heartbeat = 0;
    // Tracks the active-slots bitmap from the previous main-loop
    // iteration so we can detect 0→1 transitions: a bit going from 0
    // to 1 means the daemon just (re-)registered that slot, possibly
    // because the L2CPU was rebooted. Re-run init_device for each
    // newly-active slot to wipe whatever state the *previous* L2CPU's
    // kernel left in the reg / shadow / snap regions. Without this,
    // a fresh kernel reads stale snap values on the first sweep after
    // its reset and the `STATUS=0` cleanup hasn't yet been observed
    // by BRISC, so it can read nondeterministic state. Cheap: one
    // AND per loop iteration; the per-slot init only runs on actual
    // transitions (rare during steady state).
    uint32_t prev_active = 0;
    // #124 timing probe: top-of-loop timestamp from previous iter, so
    // each iter computes (now - prev) = sweep period in cycles. First
    // iter is skipped (no valid prev sample).
    uint32_t prev_sweep_t = mcycle_low();
    int sweep_t_valid = 0;
    for (;;) {
        uint32_t now_t = mcycle_low();
        if (sweep_t_valid) {
            uint32_t sweep_cycles = now_t - prev_sweep_t;
            *l1_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_LAST_SWEEP_CYCLES) = sweep_cycles;
            update_max_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_MAX_SWEEP_CYCLES,
                           sweep_cycles);
            // Steady-state max excludes iters where init_device fired.
            // The flag is set inside handle_status_change(STATUS=0)
            // and consumed/cleared here. Note that the flag was set
            // in the PREVIOUS iter's poll work (which produced this
            // iter's sweep_cycles via prev_sweep_t — yes, the iter
            // we're crediting includes the init_device cost). We
            // clear after this update so the NEXT iter is again a
            // candidate for the steady-state max.
            if (!init_device_fired_this_iter) {
                update_max_u32(
                    (uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_MAX_STEADY_SWEEP_CYCLES,
                    sweep_cycles);
            }
            init_device_fired_this_iter = 0;
        }
        prev_sweep_t = now_t;
        sweep_t_valid = 1;

        // Only poll slots the daemon has marked active. With one
        // L2CPU booted, that's 4 of 16 (or 32, post-#81) slots —
        // sweep period drops from ~4µs to ~1µs, narrow enough to
        // reliably win the SEL→READY race against stock kernels
        // (Alma 10, etc.) that don't have the SW_IMPL handshake.
        //
        // We iterate by 8-bit groups so an empty group skips eight
        // bits in one branch instead of eight per-bit checks. With
        // NUM_SLOTS=32 (#81 multi-disk extended the slot space from
        // 16 → 32), a per-bit loop adds ~80 ns of dead per-sweep
        // overhead which is enough to lose the SEL→READY race for
        // multi-queue devices (virtio-net, virtio-console). The
        // grouped iteration keeps the per-sweep cost bounded by the
        // number of *active* slots, not NUM_SLOTS.
        uint32_t active = read_u32(ctrl_addr(CTRL_OFF_ACTIVE_SLOTS));
        uint32_t newly_active = active & ~prev_active;
        if (newly_active != 0u) {
            // Don't re-init bits in the UART range (16..19) — those
            // are TRISC0 lifecycle bits, not virtio slots, and
            // re-running init_device on the UART range's collision
            // partners (L2CPU 2 dev 0..3) would clobber state if
            // L2CPU 2 happened to be booted alongside L2CPU 0's UART.
            // Slot indices in 0..NUM_SLOTS only.
            for (unsigned slot = 0; slot < BRISC_VIRTIO_NUM_SLOTS; slot++) {
                if (((newly_active >> slot) & 1u) == 0u) {
                    continue;
                }
                if (slot >= BRISC_KICK_UART_SLOT_BASE
                    && slot < (BRISC_KICK_UART_SLOT_BASE + BRISC_KICK_UART_NUM_SLOTS)) {
                    // UART activation, not a virtio slot
                    // (re)registration. Skip the virtio re-init.
                    continue;
                }
                if (slot >= BRISC_KICK_SHUTDOWN_SLOT_BASE
                    && slot < (BRISC_KICK_SHUTDOWN_SLOT_BASE + BRISC_KICK_SHUTDOWN_NUM_SLOTS)) {
                    // Shutdown-slot activation (#94). Wipe the per-L2CPU
                    // shutdown command cell to the sentinel so a stale
                    // value from a prior boot doesn't immediately fire
                    // a kick on the new lifecycle.
                    unsigned idx = slot - BRISC_KICK_SHUTDOWN_SLOT_BASE;
                    uintptr_t reg = brisc_shutdown_regs_base(idx) + BRISC_SHUTDOWN_OFF_COMMAND;
                    *l1_u32(reg) = BRISC_SHUTDOWN_SENTINEL;
                    FENCE_W();
                    continue;
                }
                init_device(slot);
                // Same outlier-tag as handle_status_change(STATUS=0):
                // each init_device is a ~1240-cycle wipe and shouldn't
                // pollute the steady-state max.
                init_device_fired_this_iter = 1;
            }
        }
        prev_active = active;
        for (unsigned base = 0; base < BRISC_VIRTIO_NUM_SLOTS; base += 8u) {
            uint32_t group = (active >> base) & 0xFFu;
            if (group == 0u) {
                continue;
            }
            for (unsigned i = 0; i < 8u; i++) {
                if ((group >> i) & 1u) {
                    poll_one_device(base + i);
                }
            }
        }
        // UART slots live at 16..19 in the same bitmap. M6.1 (#79):
        // BRISC drives TRISC0's reset lifecycle from this mask —
        // TRISC0 runs only while at least one UART is registered —
        // and TRISC0 owns the UART poll. The daemon polls each
        // L2CPU's feed ring directly via the chip-side TLB; BRISC is
        // not in the UART data path beyond the lifecycle bit.
        brisc_drive_trisc0_lifecycle(active);
        brisc_drive_trisc1_lifecycle(active);
        poll_shutdown_slots(active);
        poll_completion_ring();
        heartbeat += 1u;
        // Don't fence on every iteration — heartbeat is observed at
        // human timescales (debug status), so once per ~1024 sweeps
        // is plenty and lets the L0 cache + store queue work.
        if ((heartbeat & 0x3FFu) == 0) {
            *l1_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_HEARTBEAT) = heartbeat;
            FENCE_W();
        }
    }
}

// ----- TRISC1 entry (#125) -----
//
// Dedicated SEL-watch core. The hottest-loop part of the
// kernel↔BRISC dance — `writel(QUEUE_SEL); readl(QUEUE_READY)` race
// — is what limits BRISC's loop-period budget against probe
// reliability (#123). Putting this single duty on TRISC1 lets the
// race-critical sweep period drop to a handful of cycles per slot,
// well below any plausible kernel writel→readl gap.
//
// What TRISC1 owns:
//   * Read each active virtio slot's `MMIO_QUEUE_SEL`. On detected
//     change vs `last_sel[slot]`, write `MMIO_QUEUE_READY = 0` plus
//     a `fence w, w` so the kernel's next readl sees 0 instead of
//     the prior queue's stale 1.
//   * Update `last_sel[slot]`.
//
// What stays on BRISC:
//   * The full `poll_one_device` per slot (NOTIFY/kick ring,
//     STATUS, BLINDCAP, drv-feat snap, sel-gen echo, NUM_MAX/NUM
//     mirroring on SEL change). BRISC's existing
//     `handle_queue_sel_change` still also writes
//     QUEUE_READY=0 on its own slower cadence — that's a harmless
//     idempotent backstop (TRISC1 will usually have cleared it
//     already).
//
// Lifecycle: BRISC's `brisc_drive_trisc1_lifecycle` releases TRISC1
// from soft reset when any virtio slot becomes active and re-asserts
// reset when no virtio slots are active.
//
// Diagnostic: TRISC1 keeps its own per-slot `last_sel` array in BSS
// (BRISC zeroed it during boot). No fences on the snap update — it's
// hart-local state and only TRISC1 reads it.
void trisc1_main(void) {
    volatile uint32_t *active_p =
        (volatile uint32_t *)(uintptr_t)(CTRL_BASE + CTRL_OFF_ACTIVE_SLOTS);
    static uint32_t last_qsel[BRISC_VIRTIO_NUM_SLOTS];
    // #132: also watch DEVICE_FEATURES_SEL so the visible
    // DEVICE_FEATURES cell shows the right half (low vs high) of the
    // 64-bit feature space within ~µs of any kernel SEL write. Same
    // race pattern as QUEUE_SEL→QUEUE_READY (kernel issues writel(SEL);
    // readl(FEATURES); back-to-back in <1 µs). Pre-#132 firmware
    // collapsed both halves into one cell, leaking VIRTIO_F_VERSION_1's
    // bit 0 (= VIRTIO_NET_F_CSUM on the net slot) into the SEL=0 read.
    static uint32_t last_dfsel[BRISC_VIRTIO_NUM_SLOTS];
    // Initialize last_dfsel[] to a sentinel that doesn't match any
    // kernel-write value, so the first observed value (typically 0
    // on a fresh boot) triggers the swap path and seeds the visible
    // cell explicitly. UINT32_MAX = -1 isn't a value the kernel writes
    // in normal operation.
    for (unsigned i = 0; i < BRISC_VIRTIO_NUM_SLOTS; i++) {
        last_dfsel[i] = 0xFFFFFFFFu;
    }
    for (;;) {
        uint32_t active = *active_p;
        // Skip the UART range — those are TRISC0's lifecycle bits,
        // not virtio slots. Iterating them here would race-clear
        // QUEUE_READY on slot indices that aren't real virtio
        // devices when only a low-numbered L2CPU is booted.
        uint32_t virtio_active = active & ~BRISC_UART_SLOT_MASK;
        for (unsigned base = 0; base < BRISC_VIRTIO_NUM_SLOTS; base += 8u) {
            uint32_t group = (virtio_active >> base) & 0xFFu;
            if (group == 0u) {
                continue;
            }
            for (unsigned i = 0; i < 8u; i++) {
                if (((group >> i) & 1u) == 0u) {
                    continue;
                }
                unsigned slot = base + i;

                uint32_t qsel = read_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_SEL));
                if (qsel != last_qsel[slot]) {
                    // #156: read READY before zeroing so we can count
                    // the cases where TRISC1's cleanup mattered. If
                    // the visible cell still showed READY=1 here, that
                    // means BRISC's main loop hasn't seen the SEL
                    // change either yet — kernel's writel(SEL); readl(READY);
                    // is racing both harts. Counter is a proxy for
                    // "TRISC1 had to clean up" — useful diff against the
                    // BRISC-side STATS_OFF_SEL_READY_RACES.
                    uint32_t prev_ready = read_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_READY));
                    *l1_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_READY)) = 0;
                    FENCE_W();
                    last_qsel[slot] = qsel;
                    if (prev_ready != 0u) {
                        inc_stat(STATS_OFF_TRISC1_SEL_RACES);
                    }
                }

                uint32_t dfsel = read_u32(reg_addr(slot, VIRTIO_MMIO_DEVICE_FEATURES_SEL));
                if (dfsel != last_dfsel[slot]) {
                    const struct device_init *d = device_for_slot(slot);
                    uint32_t value = (dfsel == 0u) ? d->dev_feat_low
                                     : (dfsel == 1u) ? d->dev_feat_high
                                     : 0u;
                    *l1_u32(reg_addr(slot, VIRTIO_MMIO_DEVICE_FEATURES)) = value;
                    FENCE_W();
                    last_dfsel[slot] = dfsel;
                    inc_stat(STATS_OFF_DEV_FEAT_SEL_CHANGES);
                }
            }
        }
    }
}
