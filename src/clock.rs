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
        PllCntl5 {
            postdiv: val.to_le_bytes(),
        }
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
    let ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 1,
    };
    unsafe {
        libc::nanosleep(&ts, std::ptr::null_mut());
    }
}

/// Step PLL to the target frequency using the given register accessor.
//
// `clippy::needless_range_loop` would prefer `.iter_mut().enumerate()` for
// the postdiv loops, but the body has to call `current_postdivs.to_u32()`
// after each per-element mutation to write the *whole* struct back to
// CNTL5. Holding a `&mut` to one array element while also reborrowing the
// surrounding struct doesn't compile, so the indexed form is structurally
// necessary.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn frequency_solution_returns_known_values_for_200mhz() {
        assert_eq!(frequency_solution(200), (128, [15, 15, 15, 15]));
    }

    #[test]
    fn frequency_solution_returns_known_values_for_1750mhz() {
        assert_eq!(frequency_solution(1750), (140, [1, 1, 1, 1]));
    }

    #[test]
    fn frequency_solution_returns_known_values_for_15mhz() {
        assert_eq!(frequency_solution(15), (120, [99, 99, 99, 99]));
    }

    #[test]
    #[should_panic(expected = "unsupported frequency")]
    fn frequency_solution_panics_on_unsupported_freq() {
        let _ = frequency_solution(600);
    }

    #[test]
    fn pll_cntl1_roundtrip_preserves_fields() {
        for v in [0u32, 0xffff_ffff, 0xdead_beef, 0x1234_5678, 0x0001_0203] {
            assert_eq!(PllCntl1::from_u32(v).to_u32(), v);
        }
        // Constructed-from-fields shape: high u16 is fbdiv, byte0/1 are
        // refdiv/postdiv. Pin the bit layout so a future "let me reorder
        // these" refactor fails this test instead of silently miswriting
        // the PLL register.
        let c = PllCntl1 {
            refdiv: 0x12,
            postdiv: 0x34,
            fbdiv: 0xabcd,
        };
        assert_eq!(c.to_u32(), 0xabcd_3412);
        let back = PllCntl1::from_u32(0xabcd_3412);
        assert_eq!(back.refdiv, 0x12);
        assert_eq!(back.postdiv, 0x34);
        assert_eq!(back.fbdiv, 0xabcd);
    }

    #[test]
    fn pll_cntl5_roundtrip_preserves_postdivs() {
        for v in [0u32, 0xffff_ffff, 0x0102_0304] {
            assert_eq!(PllCntl5::from_u32(v).to_u32(), v);
        }
        let c = PllCntl5 {
            postdiv: [1, 2, 3, 4],
        };
        assert_eq!(c.to_u32(), 0x0403_0201);
    }

    /// Captures every register access set_frequency makes so tests can
    /// pin the sequence without poking real hardware.
    struct MockPll {
        cntl1: RefCell<u32>,
        cntl5: RefCell<u32>,
        log: RefCell<Vec<(u64, u32)>>,
    }

    impl MockPll {
        fn new(initial_cntl1: u32, initial_cntl5: u32) -> Self {
            MockPll {
                cntl1: RefCell::new(initial_cntl1),
                cntl5: RefCell::new(initial_cntl5),
                log: RefCell::new(Vec::new()),
            }
        }
    }

    impl PllAccess for MockPll {
        fn pll_read32(&self, addr: u64) -> u32 {
            match addr {
                a if a == PLL4_BASE + PLL_CNTL_1 => *self.cntl1.borrow(),
                a if a == PLL4_BASE + PLL_CNTL_5 => *self.cntl5.borrow(),
                _ => panic!("unexpected read at {:#x}", addr),
            }
        }
        fn pll_write32(&self, addr: u64, value: u32) {
            self.log.borrow_mut().push((addr, value));
            match addr {
                a if a == PLL4_BASE + PLL_CNTL_1 => *self.cntl1.borrow_mut() = value,
                a if a == PLL4_BASE + PLL_CNTL_5 => *self.cntl5.borrow_mut() = value,
                _ => panic!("unexpected write at {:#x}", addr),
            }
        }
    }

    #[test]
    fn set_frequency_drives_pll_to_target_register_state() {
        // Start from the 1750 MHz solution (fbdiv=140, postdiv=[1,1,1,1])
        // and step down to 200 MHz (fbdiv=128, postdiv=[15,15,15,15]).
        // The exact ordering of writes is verified separately below; this
        // test pins only the final state because that's the contract
        // callers care about.
        let initial_cntl1 = PllCntl1 {
            refdiv: 0,
            postdiv: 0,
            fbdiv: 140,
        }
        .to_u32();
        let initial_cntl5 = PllCntl5 {
            postdiv: [1, 1, 1, 1],
        }
        .to_u32();
        let mock = MockPll::new(initial_cntl1, initial_cntl5);

        set_frequency(&mock, 200);

        let final_cntl1 = PllCntl1::from_u32(*mock.cntl1.borrow());
        assert_eq!(final_cntl1.fbdiv, 128, "fbdiv should land on target");
        let final_cntl5 = PllCntl5::from_u32(*mock.cntl5.borrow());
        assert_eq!(final_cntl5.postdiv, [15, 15, 15, 15]);
    }

    #[test]
    fn set_frequency_steps_postdiv_up_then_fbdiv_then_postdiv_down() {
        // From 1750 → 200: postdiv must go from 1 to 15 (UP), then fbdiv
        // 140 → 128 (DOWN), then postdiv stays at 15 (no further change).
        // Verify the sequencing: every CNTL5 write happens *before* any
        // CNTL1 write, because dropping fbdiv at low postdiv would briefly
        // glitch the PLL above its safe range.
        let initial_cntl1 = PllCntl1 {
            refdiv: 0,
            postdiv: 0,
            fbdiv: 140,
        }
        .to_u32();
        let initial_cntl5 = PllCntl5 {
            postdiv: [1, 1, 1, 1],
        }
        .to_u32();
        let mock = MockPll::new(initial_cntl1, initial_cntl5);

        set_frequency(&mock, 200);

        let log = mock.log.borrow();
        let last_cntl5_up = log.iter().rposition(|&(a, _)| a == PLL4_BASE + PLL_CNTL_5);
        let first_cntl1 = log.iter().position(|&(a, _)| a == PLL4_BASE + PLL_CNTL_1);
        assert!(last_cntl5_up.is_some() && first_cntl1.is_some());
        // For a pure step-down (target postdiv >= initial postdiv), all
        // CNTL5 writes are the postdiv-up phase and precede every CNTL1
        // write.
        assert!(
            last_cntl5_up.unwrap() < first_cntl1.unwrap(),
            "all CNTL5 writes must complete before fbdiv stepping starts"
        );
    }

    #[test]
    fn set_frequency_to_already_current_target_is_idempotent() {
        // When the PLL is already at 200 MHz and we ask for 200 MHz
        // again (e.g. SharedChip::idle_pll called against an idle chip
        // where reset_x280 already stepped down), no register writes
        // should be issued, and the final state must match the
        // expected solution. Pin both: the call is harmless, and the
        // state is what the chip would expect on the next step-up.
        let (target_fbdiv, target_postdiv) = frequency_solution(200);
        let initial_cntl1 = PllCntl1 {
            refdiv: 0,
            postdiv: 0,
            fbdiv: target_fbdiv,
        }
        .to_u32();
        let initial_cntl5 = PllCntl5 {
            postdiv: target_postdiv,
        }
        .to_u32();
        let mock = MockPll::new(initial_cntl1, initial_cntl5);

        set_frequency(&mock, 200);

        assert!(
            mock.log.borrow().is_empty(),
            "expected zero PLL writes at already-target state, got {:?}",
            mock.log.borrow()
        );
        let final_cntl1 = PllCntl1::from_u32(*mock.cntl1.borrow());
        assert_eq!(final_cntl1.fbdiv, target_fbdiv);
        let final_cntl5 = PllCntl5::from_u32(*mock.cntl5.borrow());
        assert_eq!(final_cntl5.postdiv, target_postdiv);
    }
}
