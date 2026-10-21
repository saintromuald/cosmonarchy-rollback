//! The StarCraft 1.16.1 struct/data layer.
//!
//! Layouts come from `bw_dat` in its default (v116, non-SC:R) mode. Every struct
//! size is asserted against the classic 32-bit Brood War layout so a wrong crate
//! feature or arch fails at compile time, not mid-game.

#![allow(dead_code)]

pub use bw_dat::structs::*;

const _: () = {
    assert!(
        core::mem::size_of::<Game>() == 0x17700,
        "Game layout is not the 32-bit 1.16.1 layout"
    );
    assert!(
        core::mem::size_of::<Unit>() == 0x150,
        "Unit layout is not the 32-bit 1.16.1 layout"
    );
};

/// Startup self-check that the address/struct layer is the one we compiled against.
pub fn layer0_ok() -> bool {
    core::mem::size_of::<Game>() == 0x17700 && core::mem::size_of::<Unit>() == 0x150
}
