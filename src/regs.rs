// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Chip-side register addresses, MMIO offsets, and IRQ numbers.
//!
//! All literal hardware addresses live here so we have one place to
//! audit when bringing up a new SKU or tracking down a typo. Anything
//! that would otherwise be a bare `0x80030014` / `0xfffff7fefff10000`
//! / `2 * 1024 * 1024` in the body of a function should be a named
//! constant in this module instead.
//!
//! Module-wide `#![allow(dead_code)]` — register-layout constants are
//! kept named for future use even when the current code path doesn't
//! reach them. The compile-time invariants below pin the relationships
//! between values; an unused name with a known meaning beats having to
//! re-derive an offset later.
//!
//! ## Conventions
//!
//! - Constants are grouped into submodules by the device they describe
//!   (`l2cpu`, `plic`, `boot_image`, `slirp`, `virtio_mmio`).
#![allow(dead_code)]
//! - ARC-tile (8,0) registers (PLL, reset unit, etc.) and the
//!   `SharedChip` window over them stay in `shared_chip.rs` —
//!   that's where the lock + access methods live, so locality of
//!   reference wins over symmetry. Likewise PLL control offsets stay in
//!   `clock.rs`. This module covers everything else.

/// Per-L2CPU control registers (NOC writes to the L2CPU's own tile).
pub mod l2cpu {
    /// Base address of the L2CPU control register block on the per-core
    /// NOC tile. Houses 4 × 8-byte reset-vector slots (one per X280
    /// hart) followed by status/config words.
    pub const CONTROL_BASE: u64 = 0xfffff7fefff10000;

    /// L3 cache control register block (relative to the L2CPU's tile).
    pub const L3_CTRL_BASE: u64 = 0x0201_0000;
    /// Offset within `L3_CTRL_BASE` of the enable register.
    pub const L3_ENABLE_OFFSET: u64 = 8;
    /// Value to write to enable L3 across all 4 cores.
    pub const L3_ENABLE_VALUE: u32 = 0x0f;

    /// L2 prefetcher configuration block (relative to the L2CPU's tile).
    pub const L2_PREFETCH_BASE: u64 = 0x0203_0000;
    /// Stride between the four prefetch engines' config blocks.
    pub const L2_PREFETCH_STRIDE: u64 = 0x2000;
    /// Number of prefetch engines.
    pub const L2_PREFETCH_NUM: u64 = 4;
    /// Magic config words written into each prefetcher (firmware-internal
    /// values; inherited from the upstream Tenstorrent reference boot
    /// flow). Pair: low word at offset 0, high word at offset 4.
    pub const L2_PREFETCH_CFG_LO: u32 = 0x0001_5811;
    pub const L2_PREFETCH_CFG_HI: u32 = 0x0038_c84e;
}

/// Per-L2CPU PLIC interrupt-pending window. One PLIC per L2CPU tile;
/// the address is into that core's NOC space.
pub mod plic {
    pub const PENDING_BASE: u64 = 0x2FF1_0000;
    /// Offset of the pending-interrupts register inside `PENDING_BASE`.
    pub const PENDING_OFFSET: u64 = 0x404;

    /// Convenience: full address of the pending-interrupts register.
    pub const PENDING_ADDR: u64 = PENDING_BASE + PENDING_OFFSET;
}

/// Boot-image offsets within an L2CPU's DRAM, relative to the core's
/// `L2CPU_STARTING_ADDRESS`. Set by convention shared with the device
/// tree (`blackhole-card.dtb`).
pub mod boot_image {
    pub const OPENSBI_OFFSET: u64 = 0x0;
    pub const DTB_OFFSET: u64 = 0x10_0000;
    pub const KERNEL_OFFSET: u64 = 0x20_0000;
    pub const INITRAMFS_OFFSET: u64 = 0xb500_0000;
}

/// VirtIO MMIO regions inside the L2CPU's DRAM window: each device
/// occupies 2 MiB; the layout reserves room for six devices (one each
/// of disk / net / console / rng plus two extra disk slots used for
/// data volumes and the cloud-init seed — see #81). The reservation
/// is `RESERVED_SIZE` counted backwards from the end of memory.
///
/// In the engine-driven boot path the actual MMIO PAs come from the
/// Tensix engine's L1 reg files via the per-L2CPU TLB, not from the
/// L2CPU's own DRAM-end region. The OFFSET constants below are the
/// historical reservation map preserved so a guest kernel still sees
/// the top-of-DRAM range as `reserved-memory` and doesn't allocate
/// over it; they're not used to compute actual MMIO PAs anymore.
///
/// The values exposed here are the *region offsets* in the form
/// `mem_end - region_offset` — i.e. the number of bytes to subtract
/// from `mem_end` to get the region's base.
pub mod virtio_mmio {
    /// Total reservation at the top of an L2CPU's DRAM for the six
    /// `virtio,mmio` regions (6 × 2 MiB).
    pub const RESERVED_SIZE: u64 = 0xC0_0000;
    /// Per-device MMIO region size (2 MiB).
    pub const MMIO_SLOT_SIZE: u64 = 0x20_0000;

    /// Primary disk device region offset: 2 MiB before `mem_end`.
    pub const DISK_OFFSET: u64 = MMIO_SLOT_SIZE;
    /// Network device region offset: 4 MiB before `mem_end`.
    pub const NET_OFFSET: u64 = 2 * MMIO_SLOT_SIZE;
    /// Console device region offset: 6 MiB before `mem_end`. Paired
    /// with IRQ 31. See #51 for rationale.
    pub const CONSOLE_OFFSET: u64 = 3 * MMIO_SLOT_SIZE;
    /// RNG device region offset: 8 MiB before `mem_end`. Paired with
    /// IRQ 30. Required for the AlmaLinux EFI shim's
    /// `EFI_RNG_PROTOCOL` during the U-Boot+GRUB+shim chained-boot
    /// path. See #62.
    pub const RNG_OFFSET: u64 = 4 * MMIO_SLOT_SIZE;
    /// Second disk slot (10 MiB before `mem_end`). Used for cloud-init
    /// NoCloud seed images (#82) and any 2nd virtio-blk an operator
    /// attaches via `add-disk --name`.
    pub const DISK1_OFFSET: u64 = 5 * MMIO_SLOT_SIZE;
    /// Third disk slot (12 MiB before `mem_end`). Reserved for a 3rd
    /// virtio-blk (e.g. persistent data volume alongside rootfs +
    /// seed).
    pub const DISK2_OFFSET: u64 = 6 * MMIO_SLOT_SIZE;

    /// PLIC interrupt for virtio-disk. DTB ties together as
    /// `virtio@<addr> { interrupts = <DISK_IRQ>; }`.
    pub const DISK_IRQ: u32 = 33;
    /// PLIC interrupt for virtio-net.
    pub const NET_IRQ: u32 = 32;
    /// PLIC interrupt for virtio-console.
    pub const CONSOLE_IRQ: u32 = 31;
    /// PLIC interrupt for virtio-rng.
    pub const RNG_IRQ: u32 = 30;
    /// PLIC interrupt for the second virtio-blk slot (#81).
    pub const DISK1_IRQ: u32 = 29;
    /// PLIC interrupt for the third virtio-blk slot (#81).
    pub const DISK2_IRQ: u32 = 28;

    /// PLIC interrupt for the M6 (#78) 16550 UART. Disjoint from the
    /// virtio range (30..33) so it doesn't share with virtio-console.
    /// TX-only today — no IRQ is actually fired for TX completion;
    /// reserved for the future RX path. Documented here so adding
    /// RX doesn't have to find a free number under pressure.
    pub const UART_IRQ: u32 = 35;

    /// VirtIO `device_id` value for the block device (block = 2).
    pub const VIRTIO_ID_BLOCK: u32 = 2;
    /// VirtIO `device_id` value for the network device (net = 1).
    pub const VIRTIO_ID_NET: u32 = 1;
    /// VirtIO `device_id` value for the console device (console = 3).
    /// virtio 1.2 §5.3.
    pub const VIRTIO_ID_CONSOLE: u32 = 3;
    /// VirtIO `device_id` value for the entropy/RNG device (rng = 4).
    /// virtio 1.2 §5.4.
    pub const VIRTIO_ID_ENTROPY: u32 = 4;
}

/// Guest-OS shutdown signalling (#94). One u32 register per L2CPU in
/// (#166 Phase 1) OpenSBI purgatory status block. Mirrors the
/// `BHX_PURGATORY_STATUS_OFFSET` / `BHX_PURGATORY_STATUS_PARKED`
/// constants in `third_party/opensbi/patches/0002-bhx-purgatory-magic.patch`.
/// When the SBI SRST fall-through reaches `sbi_platform_final_exit`,
/// our patched OpenSBI writes the magic at this offset within the
/// L2CPU's DRAM range; the daemon polls it via `dispatch_status` to
/// confirm the soft-reboot path is functioning.
pub mod purgatory {
    /// Offset from the L2CPU's memory base (= OpenSBI's `fw_start`)
    /// where the purgatory hook writes its status word. Lives in the
    /// firmware-reserved range `[mem_base, mem_base + 0x100000)` (DTB
    /// is at +0x100000, so 0xE0000 leaves a 64 KiB margin) and well
    /// past OpenSBI's own .text/.data/.bss + scratch (typically
    /// <300 KiB). Future phases will expand this region into a full
    /// handshake area (release magic, next-entry-address slot, per-hart
    /// liveness mask, etc.).
    pub const STATUS_OFFSET: u64 = 0x000E_0000;
    /// Phase 2 (#166) — peer convergence bitmask, 8 bytes after
    /// the status word. Low 4 bits = harts 0..3 of this tile that
    /// reached `SBI_HSM_STATE_STOPPED` before the final_exit hook
    /// announced PARKED. The SRST-issuing hart's bit is NOT set
    /// here (it transitions itself in `sbi_hsm_exit` after final_exit
    /// returns). Operator interpretation:
    ///   0xE = full convergence on a 4-hart tile (peers 1..3 stopped,
    ///         self about to)
    ///   0x0 = SRST hart proceeded without seeing any peer reach STOPPED
    ///         (timeout or single-hart tile)
    pub const PEERS_OFFSET: u64 = STATUS_OFFSET + 8;
    /// Phase 4a (#166) — hart 0 release-metadata block. Each field
    /// is a u64 PA the host writes to via the L2CPU's persistent
    /// TLB window. Layout matches the C #defines in
    /// `third_party/opensbi/patches/0002-bhx-purgatory-magic.patch`.
    pub const NEXT_ADDR_PA_OFFSET: u64 = STATUS_OFFSET + 0x10;
    pub const NEXT_MODE_PA_OFFSET: u64 = STATUS_OFFSET + 0x18;
    pub const NEXT_ARG1_PA_OFFSET: u64 = STATUS_OFFSET + 0x20;
    /// PA of hart 0's HSM `state` field. Host writes
    /// `SBI_HSM_STATE_START_PENDING = 2` here to wake the parked hart.
    pub const HSM_STATE_PA_OFFSET: u64 = STATUS_OFFSET + 0x28;
    /// PA of CLINT MSIP[0]. Host writes `1` here to fire the M-mode
    /// software interrupt that wakes hart 0 from `wfi`.
    pub const MSIP_PA_OFFSET: u64 = STATUS_OFFSET + 0x30;
    /// (#166 Phase 5) Force-park IPI metadata. The host writes
    /// `[FORCE_PARK_REQ_VALUE_OFFSET]` (a u64) to the address held at
    /// `[FORCE_PARK_REQ_PA_OFFSET]`, then writes `1` to MSIP_PA, to
    /// deliver an M-mode software interrupt that OpenSBI's IPI
    /// dispatcher routes to the `bhx_force_park` event's `process`
    /// callback (which calls `sbi_system_reset` → same path as a
    /// guest-issued SBI SRST). Both fields are populated at OpenSBI
    /// cold init (so available before any SRST has happened) and are
    /// stable across reboot cycles. `req_pa == 0` means the IPI event
    /// failed to register and the host should fall back to
    /// `--force-reset-pcie`.
    pub const FORCE_PARK_REQ_PA_OFFSET: u64 = STATUS_OFFSET + 0x38;
    pub const FORCE_PARK_REQ_VALUE_OFFSET: u64 = STATUS_OFFSET + 0x40;

    /// "PARKED__" interpreted as little-endian u64. Final_exit hook
    /// writes this exact value to indicate the harts are about to
    /// enter `sbi_hsm_hart_wait`.
    pub const STATUS_PARKED: u64 = 0x5f5f_4445_4b52_4150;
    /// HSM state values (from `SBI_HSM_STATE_*` in OpenSBI's
    /// `sbi_ecall_interface.h`). Used by the release path to validate
    /// that the parked hart reads as STOPPED before the host writes
    /// START_PENDING.
    pub const HSM_STATE_STARTED: u32 = 0;
    pub const HSM_STATE_STOPPED: u32 = 1;
    pub const HSM_STATE_START_PENDING: u32 = 2;
    pub const HSM_STATE_STOP_PENDING: u32 = 3;
    /// `next_mode` value the host writes for an S-mode kernel handoff.
    /// Mirrors `PRV_S` from RISC-V privileged spec.
    pub const NEXT_MODE_S: u64 = 1;
}

/// Slirp host-side port allocation for SSH forwarding to the guest.
pub mod slirp {
    /// Base host port for the SSH forward of L2CPU 0 on card 0.
    pub const SSH_BASE_PORT: u16 = 2222;
    /// Stride between cards: `host_port = BASE + l2cpu_idx + STRIDE * card`.
    /// Set to the number of L2CPUs per card so every (card, l2cpu) pair
    /// gets a unique host port without collisions.
    pub const PORTS_PER_CARD: u16 = 4;

    /// Compute the SSH forward port for a given (card, l2cpu_idx).
    pub fn ssh_port(card: u32, l2cpu_idx: u8) -> u16 {
        SSH_BASE_PORT + l2cpu_idx as u16 + PORTS_PER_CARD * card as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slirp_ssh_port_is_unique_across_first_card() {
        let p0 = slirp::ssh_port(0, 0);
        let p3 = slirp::ssh_port(0, 3);
        assert_eq!(p0, 2222);
        assert_eq!(p3, 2225);
    }

    #[test]
    fn slirp_ssh_port_does_not_collide_across_cards() {
        // Card 1, l2cpu 0 should not collide with card 0, l2cpu 0..=3.
        let card0_max = slirp::ssh_port(0, 3);
        let card1_min = slirp::ssh_port(1, 0);
        assert!(card1_min > card0_max);
    }

    // Compile-time invariants on the virtio MMIO layout. The values are
    // all `const`, so checking them with `assert!` at runtime is what
    // `clippy::assertions_on_constants` flags. Using a `const` block
    // makes them fail-the-build instead of fail-a-test, which is
    // strictly better — a future edit that breaks the layout never
    // reaches a tested binary.
    const _VIRTIO_MMIO_LAYOUT_INVARIANTS: () = {
        // Reservation holds all six slots end-to-end.
        assert!(virtio_mmio::RESERVED_SIZE == 6 * virtio_mmio::MMIO_SLOT_SIZE);
        // Each region is at a unique slot-aligned offset inside the
        // reservation. Indices are i=1..6 (slot 0 == top-of-DRAM is
        // unused so the reservation never touches mem_end itself).
        assert!(virtio_mmio::DISK_OFFSET == virtio_mmio::MMIO_SLOT_SIZE);
        assert!(virtio_mmio::NET_OFFSET == 2 * virtio_mmio::MMIO_SLOT_SIZE);
        assert!(virtio_mmio::CONSOLE_OFFSET == 3 * virtio_mmio::MMIO_SLOT_SIZE);
        assert!(virtio_mmio::RNG_OFFSET == 4 * virtio_mmio::MMIO_SLOT_SIZE);
        assert!(virtio_mmio::DISK1_OFFSET == 5 * virtio_mmio::MMIO_SLOT_SIZE);
        assert!(virtio_mmio::DISK2_OFFSET == 6 * virtio_mmio::MMIO_SLOT_SIZE);
        assert!(virtio_mmio::DISK_OFFSET.is_multiple_of(virtio_mmio::MMIO_SLOT_SIZE));
        assert!(virtio_mmio::NET_OFFSET.is_multiple_of(virtio_mmio::MMIO_SLOT_SIZE));
        assert!(virtio_mmio::CONSOLE_OFFSET.is_multiple_of(virtio_mmio::MMIO_SLOT_SIZE));
        assert!(virtio_mmio::RNG_OFFSET.is_multiple_of(virtio_mmio::MMIO_SLOT_SIZE));
        assert!(virtio_mmio::DISK1_OFFSET.is_multiple_of(virtio_mmio::MMIO_SLOT_SIZE));
        assert!(virtio_mmio::DISK2_OFFSET.is_multiple_of(virtio_mmio::MMIO_SLOT_SIZE));
        assert!(virtio_mmio::DISK_OFFSET <= virtio_mmio::RESERVED_SIZE);
        assert!(virtio_mmio::DISK2_OFFSET <= virtio_mmio::RESERVED_SIZE);
        // IRQs march down 33..28; primary 4 are 33..30 (disk/net/console/rng);
        // extra disk slots use 29 and 28. UART_IRQ=35 stays disjoint above.
        assert!(virtio_mmio::NET_IRQ == virtio_mmio::DISK_IRQ - 1);
        assert!(virtio_mmio::CONSOLE_IRQ == virtio_mmio::DISK_IRQ - 2);
        assert!(virtio_mmio::RNG_IRQ == virtio_mmio::DISK_IRQ - 3);
        assert!(virtio_mmio::DISK1_IRQ == virtio_mmio::DISK_IRQ - 4);
        assert!(virtio_mmio::DISK2_IRQ == virtio_mmio::DISK_IRQ - 5);
        // The blackhole DTB has riscv,ndev = 128, plenty of headroom.
    };

    // Compile-time invariants on the boot-image layout: each must be
    // ordered and 4 KiB-aligned. Putting these in a `const` block makes
    // them assert at compile time so a future edit to the offsets that
    // breaks the invariants fails the build.
    const _BOOT_IMAGE_LAYOUT_INVARIANTS: () = {
        assert!(boot_image::OPENSBI_OFFSET < boot_image::DTB_OFFSET);
        assert!(boot_image::DTB_OFFSET < boot_image::KERNEL_OFFSET);
        assert!(boot_image::KERNEL_OFFSET < boot_image::INITRAMFS_OFFSET);
        assert!(boot_image::OPENSBI_OFFSET.is_multiple_of(0x1000));
        assert!(boot_image::DTB_OFFSET.is_multiple_of(0x1000));
        assert!(boot_image::KERNEL_OFFSET.is_multiple_of(0x1000));
        assert!(boot_image::INITRAMFS_OFFSET.is_multiple_of(0x1000));
    };
}
