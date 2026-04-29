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
#include "virtio_layout.h"

// Firmware version, inspected via the stats page. Bump for any
// wire-protocol change between daemon ↔ BRISC. Format: 0xAABBCCDD
// where AA=major, BB=minor, CC=patch, DD=reserved/build.
#define BRISC_VIRTIO_FW_VERSION 0x00050002u  // M5 (#71), build 0002 — adds SW_IMPL/sel_gen echo

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
#define SHADOW_BASE               0x00020000u
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

static inline uint32_t read_u32(uintptr_t addr) {
    return *l1_u32(addr);
}

static inline void write_u32(uintptr_t addr, uint32_t v) {
    *l1_u32(addr) = v;
    FENCE_W();
}

// Static device descriptors used to populate the per-slot reg file
// at boot. Same set for every L2CPU — each L2CPU's guest sees the
// same four virtio devices.
struct device_init {
    uint32_t device_id;
    uint32_t num_queues;  // shadow only; not visible-as-MMIO
};

static const struct device_init DEVICE_TEMPLATE[BRISC_VIRTIO_DEVS_PER_L2CPU] = {
    [BRISC_VIRTIO_DEV_BLK]     = { VIRTIO_ID_BLOCK,   BRISC_VIRTIO_QUEUES_BLK     },
    [BRISC_VIRTIO_DEV_NET]     = { VIRTIO_ID_NET,     BRISC_VIRTIO_QUEUES_NET     },
    [BRISC_VIRTIO_DEV_CONSOLE] = { VIRTIO_ID_CONSOLE, BRISC_VIRTIO_QUEUES_CONSOLE },
    [BRISC_VIRTIO_DEV_RNG]     = { VIRTIO_ID_ENTROPY, BRISC_VIRTIO_QUEUES_RNG     },
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

    // DEVICE_FEATURES — pre-populate with the VIRTIO_F_VERSION_1
    // bit (high half, bit 32 of the 64-bit feature space). The
    // poll-loop SEL multiplexer below also writes this on every
    // iteration, but pre-setting here closes the race window
    // between L2CPU reset release and BRISC's first poll: stock
    // Linux virtio-mmio drivers race ~µs between
    // `writel(SEL=1)` and `readl(DEVICE_FEATURES)`, comparable
    // to BRISC's full sweep, so without this initialization the
    // kernel's first read can land on a stale 0 value. We
    // accept that the SEL=0 path can race (kernel sees stale
    // 0x1 instead of 0) — that's a spurious low-half feature
    // bit, which device drivers ignore for unknown bits.
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_DEVICE_FEATURES)) = VIRTIO_F_VERSION_1_HIGH_BIT;

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

static inline void inc_stat(unsigned off) {
    volatile uint32_t *p = l1_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + off);
    *p = *p + 1u;
    FENCE_W();
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
    uint32_t num   = *l1_u32(shadow_queue_addr(slot, sel, SHADOW_Q_OFF_NUM));

    // QueueNumMax is always BRISC_VIRTIO_QUEUE_NUM_MAX for queues we
    // support; it's 0 for queues past `num_queues` to tell the guest
    // "this queue doesn't exist."
    uint32_t num_max = (sel < device_for_slot(slot)->num_queues) ? BRISC_VIRTIO_QUEUE_NUM_MAX : 0;

    *l1_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_NUM_MAX)) = num_max;
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_NUM))     = num;
    // Always clear visible QUEUE_READY=0 on SEL change. Stock Linux's
    // vm_setup_vq for queue N+1 starts with `writel(SEL=N+1);
    // readl(QUEUE_READY)` expecting 0 — but if we mirror
    // shadow[N+1].READY (which might legitimately be 1 if a previous
    // setup left the queue ready), the kernel sees stale 1 and bails
    // with -ENOENT. The eager clear elsewhere handles the
    // SAME-SEL-window race; this addresses the SEL-transition window.
    // Setting READY=0 here is safe: the kernel never reads READY
    // after writing 1 (only on next vm_setup_vq cycle), and del_vq's
    // `writel(0); WARN_ON(readl !=0)` sees 0 (= what it just wrote).
    *l1_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_READY))   = 0;
    FENCE_W();

    inc_stat(STATS_OFF_SEL_CHANGES);
}

// Guest wrote QUEUE_READY for the currently-selected queue. Persist
// to shadow so a later SEL swap re-reads the right value. Also handle
// the standard virtio rule "READY=0 disables the queue" by zeroing
// the per-queue desc/avail/used pointers — the guest will rewrite
// them on the next configure cycle.
static void handle_queue_ready_change(unsigned slot, uint32_t sel, uint32_t ready) {
    if (sel >= BRISC_VIRTIO_MAX_QUEUES) {
        return;
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
    }
    FENCE_W();
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
static void kick_ring_push(unsigned slot, uint32_t queue_idx) {
    uint32_t seq = read_u32(ctrl_addr(CTRL_OFF_KICK_RING_HDR + KICK_HDR_OFF_PRODUCER_SEQ));
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
static void handle_status_change(unsigned slot, uint32_t status, uint32_t prev) {
    (void)prev;
    if (status == 0) {
        uint32_t e = read_u32(epoch_addr(slot));
        init_device(slot);
        // init_device wipes the shadow region, including epoch — so
        // re-establish it after.
        write_u32(epoch_addr(slot), e + 1u);
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
    while (consumer != producer) {
        uint32_t idx = consumer & (COMPL_RING_ENTRIES - 1u);
        uintptr_t entry = ctrl_addr(CTRL_OFF_COMPL_RING + idx * COMPL_ENTRY_SIZE);
        uint32_t slot_q = *l1_u32(entry);  // [15:0] slot, [31:16] queue
        // Stash for diagnostics; same packed format as LAST_NOTIFY.
        *l1_u32((uintptr_t)BRISC_VIRTIO_STATS_BASE + STATS_OFF_LAST_COMPL) = slot_q;
        inc_stat(STATS_OFF_COMPL_EVENTS);
        consumer += 1u;
    }
    write_u32(ctrl_addr(CTRL_OFF_COMPL_RING_HDR + COMPL_HDR_OFF_CONSUMER_SEQ), consumer);
}

// ----- Main poll loop -----

static void poll_one_device(unsigned slot) {
    // STATUS — RW. Detect by snapshot diff.
    uint32_t status = read_u32(reg_addr(slot, VIRTIO_MMIO_STATUS));
    uint32_t status_prev = read_u32(snap_addr(slot, SNAP_OFF_STATUS));
    if (status != status_prev) {
        handle_status_change(slot, status, status_prev);
        write_u32(snap_addr(slot, SNAP_OFF_STATUS), status);
    }

    // QUEUE_SEL — W. We don't reset visible-as-MMIO QUEUE_SEL, so
    // detection is the same diff approach.
    uint32_t sel = read_u32(reg_addr(slot, VIRTIO_MMIO_QUEUE_SEL));
    uint32_t sel_prev = read_u32(snap_addr(slot, SNAP_OFF_QUEUE_SEL));
    if (sel != sel_prev) {
        handle_queue_sel_change(slot, sel);
        write_u32(snap_addr(slot, SNAP_OFF_QUEUE_SEL), sel);
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

    // QUEUE_READY — RW. Persist any non-zero write to shadow, then
    // clear the visible reg back to 0. The legacy host-buffer path
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
        write_u32(snap_addr(slot, SNAP_OFF_DRV_FEAT_SEL), dfd_sel);
    }
    uint32_t dfd = read_u32(reg_addr(slot, VIRTIO_MMIO_DRIVER_FEATURES));
    uint32_t dfd_prev = read_u32(snap_addr(slot, SNAP_OFF_DRV_FEAT));
    if (dfd != dfd_prev) {
        write_u32(snap_addr(slot, SNAP_OFF_DRV_FEAT), dfd);
    }

    // Per-queue setup registers — QUEUE_NUM + the three address
    // pairs (DESC, DRIVER/AVAIL, DEVICE/USED). The guest writes these
    // before QUEUE_READY=1 for each queue. We snapshot-diff each
    // and capture into the shadow row indexed by the currently
    // selected queue, so the daemon-side data plane (#71 M5.5b) can
    // read accurate per-queue state without racing the SEL
    // multiplexer.
    if (sel < BRISC_VIRTIO_MAX_QUEUES) {
        // Per-queue setup: blind-capture every iteration into
        // shadow[sel]. The earlier snapshot-diff approach broke for
        // virtio-net's TX setup, where queue 1's DESC_HIGH is the
        // same 0x40 word the kernel wrote for queue 0 — `v != prev`
        // never fired, queue 1's shadow stayed at 0, and dispatch
        // saw a desc address with the high half missing
        // ("pointers out of L2CPU memory range").
        //
        // Blind-capture is safe AFTER the SEL handler above: that
        // write-from-shadow happens before this loop, so on a fresh
        // SEL change we either capture stale shadow values (a
        // no-op; we just wrote them to visible) or freshly-written
        // kernel values (the setup we want). Either way the shadow
        // for the current sel converges to the kernel's intent
        // within one or two poll iterations.
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
        for (unsigned f = 0; f < sizeof(FIELDS) / sizeof(FIELDS[0]); f++) {
            uint32_t v = read_u32(reg_addr(slot, FIELDS[f].mmio_off));
            *l1_u32(shadow_queue_addr(slot, sel, FIELDS[f].shadow_off)) = v;
        }
    }

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
        write_u32(reg_addr(slot, VIRTIO_MMIO_SEL_GENERATION), next);
        write_u32(snap_addr(slot, SNAP_OFF_SEL_GEN_ECHO), next);
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

    // Block on the daemon's hello before entering the steady-state
    // poll loop. Without this, a guest could (in theory) start
    // probing before the daemon has agreed on the protocol version
    // — the L1 reg files are already initialized by init_device, so
    // the device looks valid, but the kick FIFO would have no
    // consumer.
    wait_for_hello_and_ack();

    uint32_t heartbeat = 0;
    for (;;) {
        // Only poll slots the daemon has marked active. With one
        // L2CPU booted, that's 4 of 16 slots — sweep period drops
        // from ~4µs to ~1µs, narrow enough to reliably win the
        // SEL→READY race against stock kernels (Alma 10) that
        // don't have the SW_IMPL handshake.
        uint32_t active = read_u32(ctrl_addr(CTRL_OFF_ACTIVE_SLOTS));
        for (unsigned slot = 0; slot < BRISC_VIRTIO_NUM_SLOTS; slot++) {
            if (((active >> slot) & 1u) == 0) {
                continue;
            }
            poll_one_device(slot);
        }
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
