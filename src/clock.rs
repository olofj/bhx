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

/// PLLCNTL1 register layout: {refdiv: u8, postdiv: u8, fbdiv: u16} — little-endian.
/// Represented as raw bytes to avoid unsafe transmute.
#[derive(Clone, Copy)]
struct PllCntl1 {
    refdiv: u8,
    postdiv: u8,
    fbdiv: u16,
}

impl PllCntl1 {
    fn from_u32(val: u32) -> Self {
        let b = val.to_le_bytes();
        PllCntl1 {
            refdiv: b[0],
            postdiv: b[1],
            fbdiv: u16::from_le_bytes([b[2], b[3]]),
        }
    }
    fn to_u32(self) -> u32 {
        let fb = self.fbdiv.to_le_bytes();
        u32::from_le_bytes([self.refdiv, self.postdiv, fb[0], fb[1]])
    }
}

/// PLLCNTL5 register: 4 postdiv bytes — little-endian.
#[derive(Clone, Copy)]
struct PllCntl5 {
    postdiv: [u8; 4],
}

impl PllCntl5 {
    fn from_u32(val: u32) -> Self {
        PllCntl5 { postdiv: val.to_le_bytes() }
    }
    fn to_u32(self) -> u32 {
        u32::from_le_bytes(self.postdiv)
    }
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
#[allow(clippy::needless_range_loop)]
pub fn set_frequency(access: &dyn PllAccess, mhz: u32) {
    let (target_fbdiv, target_postdiv) = frequency_solution(mhz);

    // Read current values
    let mut current_postdivs = PllCntl5::from_u32(access.pll_read32(PLL4_BASE + PLL_CNTL_5));
    let mut current_fbdiv = PllCntl1::from_u32(access.pll_read32(PLL4_BASE + PLL_CNTL_1));

    // Step 1: Increase postdivs that need to go up
    for i in 0..4 {
        while current_postdivs.postdiv[i] < target_postdiv[i] {
            current_postdivs.postdiv[i] += 1;
            access.pll_write32(PLL4_BASE + PLL_CNTL_5, current_postdivs.to_u32());
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
        access.pll_write32(PLL4_BASE + PLL_CNTL_1, current_fbdiv.to_u32());
        sleep_1ns();
    }

    // Step 3: Decrease postdivs that need to go down
    for i in 0..4 {
        while current_postdivs.postdiv[i] > target_postdiv[i] {
            current_postdivs.postdiv[i] -= 1;
            access.pll_write32(PLL4_BASE + PLL_CNTL_5, current_postdivs.to_u32());
            sleep_1ns();
        }
    }
}

