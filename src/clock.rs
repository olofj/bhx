// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! PLL clock configuration for L2CPU.

const PLL4_BASE: u64 = 0x80020500;
const PLL_CNTL_1: u64 = 0x4;
const PLL_CNTL_5: u64 = 0x14;

/// PLL frequency solutions: (fbdiv, [postdiv0, postdiv1, postdiv2, postdiv3])
pub fn frequency_solution(mhz: u32) -> (u16, [u8; 4]) {
    match mhz {
        200 => (128, [15, 15, 15, 15]),
        1750 => (140, [1, 1, 1, 1]),
        15 => (120, [99, 99, 99, 99]),
        _ => panic!("unsupported frequency: {} MHz", mhz),
    }
}

/// PLLCNTL1 register: {refdiv: u8, postdiv: u8, fbdiv: u16} — little-endian packed
#[repr(C)]
#[derive(Clone, Copy)]
struct PllCntl1 {
    refdiv: u8,
    postdiv: u8,
    fbdiv: u16,
}

/// PLLCNTL5 register: {postdiv: [u8; 4]} — little-endian packed
#[repr(C)]
#[derive(Clone, Copy)]
struct PllCntl5 {
    postdiv: [u8; 4],
}

/// Trait for accessing PLL registers, abstracting over TLB windows vs AXI.
pub trait PllAccess {
    fn pll_read32(&self, addr: u64) -> u32;
    fn pll_write32(&self, addr: u64, value: u32);
}

fn sleep_1ns() {
    let ts = libc::timespec { tv_sec: 0, tv_nsec: 1 };
    unsafe { libc::nanosleep(&ts, std::ptr::null_mut()); }
}

/// Step PLL to the target frequency using the given register accessor.
pub fn set_frequency(access: &dyn PllAccess, mhz: u32) {
    let (target_fbdiv, target_postdiv) = frequency_solution(mhz);

    // Read current values
    let raw5 = access.pll_read32(PLL4_BASE + PLL_CNTL_5);
    let mut current_postdivs: PllCntl5 = unsafe { std::mem::transmute(raw5) };

    let raw1 = access.pll_read32(PLL4_BASE + PLL_CNTL_1);
    let mut current_fbdiv: PllCntl1 = unsafe { std::mem::transmute(raw1) };

    // Step 1: Increase postdivs that need to go up
    for i in 0..4 {
        while current_postdivs.postdiv[i] < target_postdiv[i] {
            current_postdivs.postdiv[i] += 1;
            let raw: u32 = unsafe { std::mem::transmute(current_postdivs) };
            access.pll_write32(PLL4_BASE + PLL_CNTL_5, raw);
            sleep_1ns();
        }
    }

    // Step 2: Adjust fbdiv toward target
    while current_fbdiv.fbdiv != target_fbdiv {
        if target_fbdiv > current_fbdiv.fbdiv {
            current_fbdiv.fbdiv += 1;
        } else {
            current_fbdiv.fbdiv -= 1;
        }
        let raw: u32 = unsafe { std::mem::transmute(current_fbdiv) };
        access.pll_write32(PLL4_BASE + PLL_CNTL_1, raw);
        sleep_1ns();
    }

    // Step 3: Decrease postdivs that need to go down
    for i in 0..4 {
        while current_postdivs.postdiv[i] > target_postdiv[i] {
            current_postdivs.postdiv[i] -= 1;
            let raw: u32 = unsafe { std::mem::transmute(current_postdivs) };
            access.pll_write32(PLL4_BASE + PLL_CNTL_5, raw);
            sleep_1ns();
        }
    }
}

/// PLL access via TLB windows (used at runtime by the host tool).
pub struct TlbPllAccess<'a> {
    pub window_cntl1: &'a crate::tlb::TlbWindow,
    pub window_cntl5: &'a crate::tlb::TlbWindow,
}

impl<'a> PllAccess for TlbPllAccess<'a> {
    fn pll_read32(&self, addr: u64) -> u32 {
        if addr == PLL4_BASE + PLL_CNTL_1 {
            self.window_cntl1.read32(0)
        } else if addr == PLL4_BASE + PLL_CNTL_5 {
            self.window_cntl5.read32(0)
        } else {
            panic!("unexpected PLL register address: 0x{:x}", addr);
        }
    }

    fn pll_write32(&self, addr: u64, value: u32) {
        if addr == PLL4_BASE + PLL_CNTL_1 {
            self.window_cntl1.write32(0, value);
        } else if addr == PLL4_BASE + PLL_CNTL_5 {
            self.window_cntl5.write32(0, value);
        } else {
            panic!("unexpected PLL register address: 0x{:x}", addr);
        }
    }
}
