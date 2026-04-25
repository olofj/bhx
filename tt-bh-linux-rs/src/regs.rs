// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! Chip-side register addresses, MMIO offsets, and IRQ numbers.
//!
//! All literal hardware addresses live here so we have one place to
//! audit when bringing up a new SKU or tracking down a typo. Anything
//! that would otherwise be a bare `0x80030014` / `0xfffff7fefff10000`
//! / `2 * 1024 * 1024` in the body of a function should be a named
//! constant in this module instead.
//!
//! ## Conventions
//!
//! - Constants are grouped into submodules by the device they describe
//!   (`l2cpu`, `plic`, `boot_image`, `slirp`, `virtio_mmio`).
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
    /// Magic config words written into each prefetcher (firmware-internal,
    /// inherited from boot.py). Pair: low word at offset 0, high word at
    /// offset 4.
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
/// occupies 2 MiB; we currently expose disk + net (and reserve two
/// slots for future expansion). The reservation is `RESERVED_SIZE`
/// counted backwards from the end of memory.
///
/// `boot::modify_dtb` lays out four `virtio,mmio` nodes at addresses
/// `mem_end - (i+1) * MMIO_SLOT_SIZE` for `i = 0..4`, with IRQ
/// `DISK_IRQ - i`. The disk device occupies slot 0 (closest to
/// `mem_end`, IRQ 33); the network device occupies slot 1 (IRQ 32).
/// Slots 2 and 3 are reserved.
///
/// The values exposed here are the *region offsets* in the form
/// `mem_end - region_offset` — i.e. the number of bytes to subtract
/// from `mem_end` to get the region's base. `virtio::run_device`
/// computes the absolute MMIO base as
/// `starting_address + memory_size - region_offset`.
pub mod virtio_mmio {
    /// Total reservation at the top of an L2CPU's DRAM for the four
    /// `virtio,mmio` regions (4 × 2 MiB plus padding).
    pub const RESERVED_SIZE: u64 = 0x60_0000;
    /// Per-device MMIO region size (2 MiB).
    pub const MMIO_SLOT_SIZE: u64 = 0x20_0000;

    /// Disk device region offset: 2 MiB before `mem_end`.
    pub const DISK_OFFSET: u64 = MMIO_SLOT_SIZE;
    /// Network device region offset: 4 MiB before `mem_end`.
    pub const NET_OFFSET: u64 = 2 * MMIO_SLOT_SIZE;

    /// PLIC interrupt for virtio-disk. DTB ties together as
    /// `virtio@<addr> { interrupts = <DISK_IRQ>; }`.
    pub const DISK_IRQ: u32 = 33;
    /// PLIC interrupt for virtio-net.
    pub const NET_IRQ: u32 = 32;
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
        // Disk and net regions are distinct, slot-aligned, and inside
        // the reservation.
        assert!(virtio_mmio::DISK_OFFSET != virtio_mmio::NET_OFFSET);
        assert!(virtio_mmio::DISK_OFFSET.is_multiple_of(virtio_mmio::MMIO_SLOT_SIZE));
        assert!(virtio_mmio::NET_OFFSET.is_multiple_of(virtio_mmio::MMIO_SLOT_SIZE));
        assert!(virtio_mmio::DISK_OFFSET <= virtio_mmio::RESERVED_SIZE);
        assert!(virtio_mmio::NET_OFFSET <= virtio_mmio::RESERVED_SIZE);
        // DTB walks i=0..4 with offset = (i+1)*MMIO_SLOT_SIZE and
        // IRQ = DISK_IRQ - i. Disk = i=0; net = i=1.
        assert!(virtio_mmio::DISK_OFFSET == virtio_mmio::MMIO_SLOT_SIZE);
        assert!(virtio_mmio::NET_OFFSET == 2 * virtio_mmio::MMIO_SLOT_SIZE);
        assert!(virtio_mmio::NET_IRQ == virtio_mmio::DISK_IRQ - 1);
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
