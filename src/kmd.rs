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

// --- Ioctl number computation ---
// Linux _IO macro: direction=0 (none), so ioctl nr = (magic << 8) | nr

const fn io(magic: u8, nr: u8) -> u64 {
    ((magic as u64) << 8) | (nr as u64)
}

pub const IOCTL_GET_DEVICE_INFO: u64 = io(TENSTORRENT_IOCTL_MAGIC, 0);
pub const IOCTL_ALLOCATE_TLB: u64 = io(TENSTORRENT_IOCTL_MAGIC, 11);
pub const IOCTL_FREE_TLB: u64 = io(TENSTORRENT_IOCTL_MAGIC, 12);
pub const IOCTL_CONFIGURE_TLB: u64 = io(TENSTORRENT_IOCTL_MAGIC, 13);

// --- Ioctl wrappers using nix ---

nix::ioctl_readwrite_bad!(ioctl_get_device_info, IOCTL_GET_DEVICE_INFO, GetDeviceInfo);
nix::ioctl_readwrite_bad!(ioctl_allocate_tlb, IOCTL_ALLOCATE_TLB, AllocateTlb);
nix::ioctl_readwrite_bad!(ioctl_free_tlb, IOCTL_FREE_TLB, FreeTlb);
nix::ioctl_readwrite_bad!(ioctl_configure_tlb, IOCTL_CONFIGURE_TLB, ConfigureTlb);

/// Open the tenstorrent character device for the given card index.
pub fn open_device(card: u32) -> std::io::Result<RawFd> {
    let path = format!("/dev/tenstorrent/{}", card);
    let fd = nix::fcntl::open(
        path.as_str(),
        nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|e| std::io::Error::from_raw_os_error(e as i32))?;
    Ok(fd)
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
