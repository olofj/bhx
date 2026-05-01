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
//! - AXI tile (8,0) registers (PLL, reset unit, etc.) and the
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
/// BRISC L1, exposed to the guest via the existing engine TLB window
/// at a fixed offset. OpenSBI's `fdt_reset_syscon` driver writes a
/// magic value here on SBI SRST; BRISC observes the write and pushes
/// a kick-ring entry with a reserved slot id.
///
/// Mirrors `brisc-firmware/include/shutdown_layout.h`.
pub mod shutdown {
    /// Engine-base-relative offset at which each L2CPU sees its own
    /// shutdown command register. Matches
    /// `BRISC_SHUTDOWN_OFFSET_FROM_ENGINE_BASE` in the firmware.
    pub const OFFSET_FROM_ENGINE_BASE: u64 = 0x0005_0000;
    /// Region size to expose in the DT `syscon` node. Generous so a
    /// future `reboot` cell at offset 0x4 doesn't need a DT change.
    pub const REG_FILE_SIZE: u64 = 0x0000_0010;
    /// Offset within the per-L2CPU shutdown reg file for the command
    /// cell. Today the only cell.
    pub const OFF_COMMAND: u64 = 0x00;

    /// Magic value the guest writes to request poweroff.
    pub const MAGIC_POWEROFF: u32 = 0x5AFE_DEAD;
    /// Magic value the guest writes to request reboot. Recognized by
    /// BRISC firmware today (kicks with kind=1) but the daemon-side
    /// dispatch lands in the reboot follow-up.
    pub const MAGIC_REBOOT: u32 = 0xB007_BEEF;
    /// Sentinel "no pending command." BRISC writes this back after
    /// firing a kick so the next sweep doesn't re-fire on the same
    /// guest write.
    pub const SENTINEL: u32 = 0;

    /// Reserved kick-ring slot ids: one per L2CPU at slots 20..23.
    /// Disjoint from virtio (0..15) and UART (16..19).
    pub const SLOT_BASE: u32 = 20;
    pub const NUM_SLOTS: u32 = 4;

    /// Convert a kick-ring slot id back to its L2CPU index. Returns
    /// `None` for slots outside the shutdown range.
    pub fn l2cpu_for_slot(slot: u32) -> Option<u8> {
        if (SLOT_BASE..SLOT_BASE + NUM_SLOTS).contains(&slot) {
            Some((slot - SLOT_BASE) as u8)
        } else {
            None
        }
    }

    /// Kick `queue_idx` value the BRISC firmware sets when reporting
    /// a poweroff vs reboot magic write. Must match the firmware's
    /// `kind` decoding in `poll_shutdown_slots`.
    pub const KIND_POWEROFF: u16 = 0;
    pub const KIND_REBOOT: u16 = 1;
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
