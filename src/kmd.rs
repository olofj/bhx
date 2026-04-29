// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Kernel module ioctl bindings — exact `#[repr(C)]` translations of console/ioctl.h

use std::os::unix::io::RawFd;

pub const TENSTORRENT_IOCTL_MAGIC: u8 = 0xFA;

// --- Structs ---

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct GetDeviceInfoIn {
    pub output_size_bytes: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct GetDeviceInfoOut {
    pub output_size_bytes: u32,
    pub vendor_id: u16,
    pub device_id: u16,
    pub subsystem_vendor_id: u16,
    pub subsystem_id: u16,
    pub bus_dev_fn: u16,
    pub max_dma_buf_size_log2: u16,
    pub pci_domain: u16,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct GetDeviceInfo {
    pub input: GetDeviceInfoIn,
    pub output: GetDeviceInfoOut,
}

// --- RESET_DEVICE ---
// Reset flags. RESET_PCIE_LINK=1 performs a PCIe link reset (equivalent to
// `tt-smi -r 0`) without reloading the FW.
pub const TENSTORRENT_RESET_DEVICE_RESTORE_STATE: u32 = 0;
pub const TENSTORRENT_RESET_DEVICE_RESET_PCIE_LINK: u32 = 1;
pub const TENSTORRENT_RESET_DEVICE_CONFIG_WRITE: u32 = 2;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ResetDeviceIn {
    pub output_size_bytes: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ResetDeviceOut {
    pub output_size_bytes: u32,
    pub result: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ResetDevice {
    pub input: ResetDeviceIn,
    pub output: ResetDeviceOut,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct NocTlbConfig {
    pub addr: u64,
    pub x_end: u16,
    pub y_end: u16,
    pub x_start: u16,
    pub y_start: u16,
    pub noc: u8,
    pub mcast: u8,
    pub ordering: u8,
    pub linked: u8,
    pub static_vc: u8,
    pub _reserved0: [u8; 3],
    pub _reserved1: [u32; 2],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct AllocateTlbIn {
    pub size: u64,
    pub _reserved: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct AllocateTlbOut {
    pub id: u32,
    pub _reserved0: u32,
    pub mmap_offset_uc: u64,
    pub mmap_offset_wc: u64,
    pub _reserved1: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct AllocateTlb {
    pub input: AllocateTlbIn,
    pub output: AllocateTlbOut,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FreeTlbIn {
    pub id: u32,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FreeTlbOut;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FreeTlb {
    pub input: FreeTlbIn,
    pub output: FreeTlbOut,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ConfigureTlbIn {
    pub id: u32,
    pub config: NocTlbConfig,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ConfigureTlbOut {
    pub _reserved: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ConfigureTlb {
    pub input: ConfigureTlbIn,
    pub output: ConfigureTlbOut,
}

// --- ALLOCATE_DMA_BUF ---
// Allocates a DMA-coherent host buffer. With the NOC_DMA flag, tt-kmd also
// programs an outbound iATU region so the chip-side NoC can reach the buffer
// at `noc_address`. Returned `mapping_offset` can be passed to mmap() on the
// device fd to get a userspace pointer to the same physical memory. See #64.
pub const TENSTORRENT_ALLOCATE_DMA_BUF_NOC_DMA: u8 = 2;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct AllocateDmaBufIn {
    pub requested_size: u32,
    pub buf_index: u8,
    pub flags: u8,
    pub _reserved0: [u8; 2],
    pub _reserved1: [u64; 2],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct AllocateDmaBufOut {
    pub physical_address: u64,
    pub mapping_offset: u64,
    pub size: u32,
    pub _reserved0: u32,
    pub noc_address: u64,
    pub _reserved1: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct AllocateDmaBuf {
    pub input: AllocateDmaBufIn,
    pub output: AllocateDmaBufOut,
}

// --- SET_POWER_STATE (kmd >= 2.6) ---
//
// Per-fd power-flag aggregation. tt-metal's UMD calls this on
// LocalChip construction with all four flags set; without it the chip
// runs at low AICLK (800 MHz on p100a) instead of max (1350 MHz),
// which makes BRISC/TRISC0 ~1.7× slower than designed for. AICLK is
// the only flag the legacy default leaves OFF — so just defaulting
// the open is not enough; we have to call this ioctl explicitly.
//
// All flags drop again when the last fd that requested them closes.

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct PowerState {
    pub argsz: u32,
    pub flags: u32,
    pub reserved0: u8,
    pub validity: u8,
    pub power_flags: u16,
    pub power_settings: [u16; 14],
}

pub const TT_POWER_FLAG_MAX_AI_CLK: u16 = 1 << 0;
pub const TT_POWER_FLAG_MRISC_PHY_WAKEUP: u16 = 1 << 1;
pub const TT_POWER_FLAG_TENSIX_ENABLE: u16 = 1 << 2;
pub const TT_POWER_FLAG_L2CPU_ENABLE: u16 = 1 << 3;

#[inline]
const fn validity(flags_count: u8, settings_count: u8) -> u8 {
    (flags_count & 0xF) | ((settings_count & 0xF) << 4)
}

// --- Ioctl number computation ---
// Linux _IO macro: direction=0 (none), so ioctl nr = (magic << 8) | nr

const fn io(magic: u8, nr: u8) -> u64 {
    ((magic as u64) << 8) | (nr as u64)
}

pub const IOCTL_GET_DEVICE_INFO: u64 = io(TENSTORRENT_IOCTL_MAGIC, 0);
pub const IOCTL_ALLOCATE_DMA_BUF: u64 = io(TENSTORRENT_IOCTL_MAGIC, 3);
pub const IOCTL_RESET_DEVICE: u64 = io(TENSTORRENT_IOCTL_MAGIC, 6);
pub const IOCTL_ALLOCATE_TLB: u64 = io(TENSTORRENT_IOCTL_MAGIC, 11);
pub const IOCTL_FREE_TLB: u64 = io(TENSTORRENT_IOCTL_MAGIC, 12);
pub const IOCTL_CONFIGURE_TLB: u64 = io(TENSTORRENT_IOCTL_MAGIC, 13);
pub const IOCTL_SET_POWER_STATE: u64 = io(TENSTORRENT_IOCTL_MAGIC, 15);

// --- Ioctl wrappers using nix ---

nix::ioctl_readwrite_bad!(ioctl_get_device_info, IOCTL_GET_DEVICE_INFO, GetDeviceInfo);
nix::ioctl_readwrite_bad!(
    ioctl_allocate_dma_buf,
    IOCTL_ALLOCATE_DMA_BUF,
    AllocateDmaBuf
);
nix::ioctl_readwrite_bad!(ioctl_reset_device, IOCTL_RESET_DEVICE, ResetDevice);
nix::ioctl_readwrite_bad!(ioctl_allocate_tlb, IOCTL_ALLOCATE_TLB, AllocateTlb);
nix::ioctl_readwrite_bad!(ioctl_free_tlb, IOCTL_FREE_TLB, FreeTlb);
nix::ioctl_readwrite_bad!(ioctl_configure_tlb, IOCTL_CONFIGURE_TLB, ConfigureTlb);
nix::ioctl_readwrite_bad!(ioctl_set_power_state, IOCTL_SET_POWER_STATE, PowerState);

/// Open the tenstorrent character device for the given card index.
pub fn open_device(card: u32) -> std::io::Result<RawFd> {
    use std::os::fd::IntoRawFd;
    let path = format!("/dev/tenstorrent/{}", card);
    // nix 0.31 returns OwnedFd; convert to RawFd to keep the existing
    // manual-close lifecycle (callers store the RawFd alongside other
    // resources and close in their own Drop ordering).
    let fd = nix::fcntl::open(
        path.as_str(),
        nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?
    .into_raw_fd();
    Ok(fd)
}

/// Request a PCIe link reset on the given open fd.
///
/// `flags` is one of `TENSTORRENT_RESET_DEVICE_*`. The kernel reports a
/// non-zero `result` for chip-side failures; we surface those as EIO.
pub fn reset_device(fd: RawFd, flags: u32) -> std::io::Result<()> {
    let mut req = ResetDevice {
        input: ResetDeviceIn {
            output_size_bytes: std::mem::size_of::<ResetDeviceOut>() as u32,
            flags,
        },
        output: ResetDeviceOut::default(),
    };
    unsafe {
        ioctl_reset_device(fd, &mut req)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    }
    if req.output.result != 0 {
        return Err(crate::Error::internal(format!(
            "RESET_DEVICE ioctl reported result={}",
            req.output.result
        ))
        .into());
    }
    Ok(())
}

/// Request max-power state on `fd`: AICLK at max, Tensix + L2CPU
/// enabled. The kmd aggregates this with any other open fd's request
/// and signals the ARC firmware. The state lasts until `fd` is closed
/// (or the flags are unset via another call).
///
/// Without this the chip runs at the legacy default — everything
/// enabled EXCEPT max AICLK — which on a p100a means 800 MHz instead
/// of 1350 MHz. Tensix baby-RISCs share that clock domain, so the
/// difference shows up as a 1.7× slower poll loop.
///
/// We deliberately don't request `MRISC_PHY_WAKEUP` — MRISC manages
/// GDDR PHY, which `tt-bh-linux` doesn't use (L2CPU runs out of host-
/// allocated DRAM and Tensix uses its own L1).
///
/// Best-effort: if the kmd is older than 2.6 it doesn't know this
/// ioctl and returns ENOTTY/EINVAL. We log and continue rather than
/// fail bring-up — the daemon still works, just at low clock.
pub fn request_max_power(fd: RawFd) -> std::io::Result<()> {
    let mut state = PowerState {
        argsz: std::mem::size_of::<PowerState>() as u32,
        // 4 valid flags so the kmd OR-aggregates ALL of bits 0..3 (we
        // request 0=MAX_AI_CLK, 2=TENSIX_ENABLE, 3=L2CPU_ENABLE; bit 1
        // / MRISC_PHY_WAKEUP intentionally left clear since we don't
        // touch GDDR).
        validity: validity(4, 0),
        power_flags: TT_POWER_FLAG_MAX_AI_CLK
            | TT_POWER_FLAG_TENSIX_ENABLE
            | TT_POWER_FLAG_L2CPU_ENABLE,
        ..Default::default()
    };
    unsafe {
        ioctl_set_power_state(fd, &mut state)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    }
    Ok(())
}

/// Read device info from an open fd.
pub fn get_device_info(fd: RawFd) -> std::io::Result<GetDeviceInfoOut> {
    let mut info = GetDeviceInfo {
        input: GetDeviceInfoIn {
            output_size_bytes: std::mem::size_of::<GetDeviceInfoOut>() as u32,
        },
        output: GetDeviceInfoOut::default(),
    };
    unsafe {
        ioctl_get_device_info(fd, &mut info)
            .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    }
    Ok(info.output)
}
