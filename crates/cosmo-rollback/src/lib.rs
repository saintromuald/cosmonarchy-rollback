//! Cosmonarchy rollback netcode DLL.
//!
//! Port of ShieldBattery's rollback engine to StarCraft 1.16.1 (Cosmonarchy BW).
//! See `docs/decisions/`.

#![allow(dead_code, non_snake_case)]

#[macro_use]
extern crate whack;

mod audit;
mod bw161;
mod crash;
mod creep;
mod frame_hook;
mod image;
mod log;
mod modules;
mod mpqdraft;
mod outer_trace;
mod plugin;
mod plugin_state;
mod pools;
mod ranges;
mod region;
mod replay;
mod session;
mod rng_watch;
mod rollback;
mod snapshot;
mod state_hash;
mod tick;
mod tiles;
mod transport;
mod turn_hook;
mod turns;

/// Bumped on ABI-affecting changes so an injected caller can gate on the build.
#[no_mangle]
pub extern "system" fn cosmo_rollback_abi() -> u32 {
    4
}
