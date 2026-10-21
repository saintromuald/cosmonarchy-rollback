//! Resolves and calls plugin rollback-state exports (ADR-0004/0005 Layers B & C).
//!
//! Each plugin (gptp.qdp, aise.qdp) exports `cosmo_<p>_state_size/save/load/rollback_enable`.
//! We resolve them by name from the loaded module (no link-time dependency) and capture
//! one blob per plugin.

#![allow(dead_code)]

use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicU32, Ordering};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

// Plugin exports use the default MSVC x86 convention (__cdecl), so these must be
// `extern "C"`, NOT `extern "system"` (= __stdcall on i686).
type StateSizeFn = unsafe extern "C" fn() -> u32;
type SaveFn = unsafe extern "C" fn(*mut u8, u32) -> u32;
type LoadFn = unsafe extern "C" fn(*const u8, u32) -> u32;
type EnableFn = unsafe extern "C" fn(i32);

#[derive(Clone, Copy)]
pub struct Fns {
    state_size: StateSizeFn,
    save: SaveFn,
    load: LoadFn,
    enable: EnableFn,
}

static PLUGINS: OnceCell<Vec<(&'static str, Fns)>> = OnceCell::new();
static CAPTURE_N: AtomicU32 = AtomicU32::new(0);

fn resolve_module(module: &[u8], prefix: &str) -> Option<Fns> {
    unsafe {
        let h = GetModuleHandleA(module.as_ptr());
        if h.is_null() {
            return None;
        }
        let sym = |suffix: &str| -> usize {
            let mut name = format!("{}{}\0", prefix, suffix).into_bytes();
            let p = GetProcAddress(h, name.as_mut_ptr());
            match p {
                Some(f) => f as usize,
                None => 0,
            }
        };
        let state_size = sym("state_size");
        let save = sym("save");
        let load = sym("load");
        let enable = sym("rollback_enable");
        if state_size == 0 || save == 0 || load == 0 || enable == 0 {
            return None;
        }
        Some(Fns {
            state_size: std::mem::transmute(state_size),
            save: std::mem::transmute(save),
            load: std::mem::transmute(load),
            enable: std::mem::transmute(enable),
        })
    }
}

fn plugins() -> &'static [(&'static str, Fns)] {
    PLUGINS.get_or_init(|| {
        let mut v = Vec::new();
        if let Some(f) = resolve_module(b"gptp.qdp\0", "cosmo_gptp_") {
            v.push(("gptp", f));
        }
        if let Some(f) = resolve_module(b"aise.qdp\0", "cosmo_aise_") {
            v.push(("aise", f));
        }
        v
    })
}

/// Enable every resolved plugin's serializer for this session (idempotent).
pub fn enable_all() {
    for (_, f) in plugins() {
        unsafe { (f.enable)(1) };
    }
}

/// Names of resolved plugins (for logging).
pub fn plugin_names() -> Vec<&'static str> {
    plugins().iter().map(|(n, _)| *n).collect()
}

/// One blob per resolved plugin (empty vec for a plugin with no state).
pub fn capture_blobs() -> Vec<Vec<u8>> {
    // Diagnostic: CM_ROLLBACK_NO_PLUGIN disables plugin-state capture entirely.
    if std::env::var("CM_ROLLBACK_NO_PLUGIN").is_ok() {
        return Vec::new();
    }
    let trace = CAPTURE_N.fetch_add(1, Ordering::Relaxed) < 4;
    plugins()
        .iter()
        .map(|(name, f)| {
            // Per-plugin gate for isolating lossy save/load (CM_ROLLBACK_NO_AISE etc.).
            if std::env::var(format!("CM_ROLLBACK_NO_{}", name.to_uppercase())).is_ok() {
                return Vec::new();
            }
            let sz = unsafe { (f.state_size)() };
            if trace {
                crate::log::log(&format!("[cosmo-rollback] {} state_size -> {}", name, sz));
            }
            if sz == 0 {
                return Vec::new();
            }
            let mut blob = vec![0u8; sz as usize];
            let w = unsafe { (f.save)(blob.as_mut_ptr(), sz) };
            if trace {
                crate::log::log(&format!("[cosmo-rollback] {} save -> {}", name, w));
            }
            blob.truncate(w as usize);
            blob
        })
        .collect()
}

/// Restore plugin blobs (aligned with [`capture_blobs`]).
pub fn restore_blobs(blobs: &[Vec<u8>]) {
    for (i, (_, f)) in plugins().iter().enumerate() {
        if let Some(b) = blobs.get(i) {
            if !b.is_empty() {
                unsafe { (f.load)(b.as_ptr(), b.len() as u32) };
            }
        }
    }
}
