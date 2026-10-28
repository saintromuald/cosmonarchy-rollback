//! Module enumeration for plugin-state snapshotting (ADR-0004 Layer A).
//!
//! Each `.qdp` plugin is a self-contained cdylib with its own mutable statics.
//! We enumerate loaded modules and add every writable, non-shared section of each
//! plugin DLL as a flat snapshot range. This captures flat plugin globals (their
//! inline RNG, mode flags, container headers); heap contents still need the
//! plugins' own save/load (ADR-0004 Layers B/C).

#![allow(dead_code)]

use std::mem::zeroed;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32First, Module32Next, MODULEENTRY32, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32,
};

use crate::ranges::RangeSpec;

const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;
const IMAGE_SCN_MEM_SHARED: u32 = 0x1000_0000;

/// The loaded module list `(name, base, size)`, captured **once**.
///
/// `CreateToolhelp32Snapshot(TH32CS_SNAPMODULE)` takes the loader lock. Calling it on
/// the game thread while the engine holds that lock (loading a temp MPQ at the first
/// frame) deadlocks the game — the first-frame ranges build froze the game exactly
/// this way: the menu rendered once, then the frame hook never returned. The module
/// list does not change after plugin startup (the pools live in `gptp.qdp` and the
/// exe, both loaded by then), so the snapshot is taken once at load time and every
/// later lookup walks the cache.
static MODULES: once_cell::sync::OnceCell<Vec<(String, usize, usize)>> =
    once_cell::sync::OnceCell::new();

/// The cached module list. Warm it from `audit::on_start` (load time); every other
/// caller uses the cache and never takes the loader lock.
pub(crate) fn module_list() -> &'static [(String, usize, usize)] {
    MODULES.get_or_init(|| {
        let mut out = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, 0);
            if snap == INVALID_HANDLE_VALUE {
                return out;
            }
            let mut me: MODULEENTRY32 = zeroed();
            me.dwSize = std::mem::size_of::<MODULEENTRY32>() as u32;
            if Module32First(snap, &mut me) != 0 {
                loop {
                    out.push((
                        cstr(std::slice::from_raw_parts(
                            me.szModule.as_ptr() as *const u8,
                            me.szModule.len(),
                        )),
                        me.modBaseAddr as usize,
                        me.modBaseSize as usize,
                    ));
                    if Module32Next(snap, &mut me) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
        out
    })
}

/// Writable, non-shared sections of every loaded `.qdp` plugin (ours excluded), up to
/// `max_len` bytes each. The reproducibility probe passes `usize::MAX` so it can *read*
/// gptp's ~19 MB `.data` even though that is never snapshotted.
pub fn writable_sections(max_len: usize) -> Vec<RangeSpec> {
    let mut out = Vec::new();
    for (name, base, _size) in module_list() {
        let lname = name.to_ascii_lowercase();
        // Skip our own module: its `.data` holds the rollback engine itself
        // (ring, mutex); restoring it mid-step would corrupt us.
        if lname.ends_with(".qdp") && !lname.contains("cosmo-rollback") {
            unsafe { add_writable_sections(*base, name, max_len, &mut out) };
        }
    }
    out
}

/// The name of the loaded module whose mapped image contains all of `[addr, addr+len)`.
///
/// Used to validate a runtime-resolved pool base: a base read out of a patched
/// operand is only trustworthy if it actually lands inside one of the loaded
/// images (the exe or a `.qdp` plugin), not in garbage.
pub fn owner_of(addr: usize, len: usize) -> Option<String> {
    if addr == 0 || len == 0 {
        return None;
    }
    let end = addr.checked_add(len)?;
    for (name, base, size) in module_list() {
        if addr >= *base && end <= base.saturating_add(*size) {
            return Some(name.clone());
        }
    }
    None
}

/// Writable, non-shared sections of every loaded `.qdp` plugin, bounded to 1 MB each:
/// the sizes the snapshot may restore. gptp's own `.data` is deliberately excluded from
/// the snapshot (ADR-0004: restoring plugin heap pointers/mutexes corrupts the plugin).
pub fn plugin_ranges() -> Vec<RangeSpec> {
    writable_sections(0x10_0000)
}

/// The base address of the writable section `name` as [`writable_sections`] names it
/// (e.g. `"gptp.qdp..data"`), if that module is loaded.
///
/// Plugin state is identified by section + offset rather than by absolute address,
/// because the `.qdp` modules are relocated per process (the resolved pool bases differ
/// between runs).
pub fn section_base(name: &str) -> Option<usize> {
    for (modname, base, _size) in module_list() {
        if let Some(b) = unsafe { section_base_in(*base, modname, name) } {
            return Some(b);
        }
    }
    None
}

/// Walk one loaded module's section table for the writable section named
/// `<modname>.<section>`.
///
/// # Safety
/// `base` must be the load address of a mapped PE image.
unsafe fn section_base_in(base: usize, modname: &str, want: &str) -> Option<usize> {
    if base == 0 {
        return None;
    }
    let e_lfanew = *((base as *const u8).add(0x3C) as *const u32) as usize;
    let pe = (base + e_lfanew) as *const u8;
    if *(pe as *const u32) != 0x0000_4550 {
        return None;
    }
    let nsec = *(pe.add(6) as *const u16) as usize;
    let optsz = *(pe.add(0x14) as *const u16) as usize;
    let sec = pe.add(0x18 + optsz);
    for i in 0..nsec {
        let s = sec.add(i * 40);
        let nm = cstr(std::slice::from_raw_parts(s, 8));
        let va = *(s.add(12) as *const u32) as usize;
        let chars = *(s.add(36) as *const u32);
        if chars & IMAGE_SCN_MEM_WRITE != 0 && chars & IMAGE_SCN_MEM_SHARED == 0 {
            if format!("{modname}.{nm}") == want {
                return Some(base + va);
            }
        }
    }
    None
}

/// Log every loaded module's name, base and size.
///
/// The plugin-state serializers are resolved by module name
/// (`plugin_state.rs`'s `GetModuleHandleA("gptp.qdp")`), and a mismatch between the name
/// the loader used and the name assumed there silently disables all plugin state. This
/// makes the real names visible.
pub fn log_modules() {
    log_modules_matching(&[])
}

/// Log only the loaded modules whose name contains any of `needles` (case-insensitive).
/// An empty `needles` logs every module.
pub fn log_modules_matching(needles: &[&str]) {
    for (name, base, size) in module_list() {
        let lower = name.to_ascii_lowercase();
        if needles.is_empty() || needles.iter().any(|n| lower.contains(n)) {
            crate::log::log(&format!(
                "[cosmo-rollback]   module {:<24} base 0x{:08x} size 0x{:x}",
                name, base, size
            ));
        }
    }
}

fn cstr(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// Walk a loaded PE module's section table and collect writable sections.
///
/// # Safety
/// `base` must be the load address of a mapped PE image.
unsafe fn add_writable_sections(
    base: usize,
    modname: &str,
    max_len: usize,
    out: &mut Vec<RangeSpec>,
) {
    if base == 0 {
        return;
    }
    let dos = base as *const u8;
    let e_lfanew = *(dos.add(0x3C) as *const u32) as usize;
    let pe = (base + e_lfanew) as *const u8;
    if *(pe as *const u32) != 0x0000_4550 {
        return;
    }
    let nsec = *(pe.add(6) as *const u16) as usize;
    let optsz = *(pe.add(0x14) as *const u16) as usize;
    let sec = pe.add(0x18 + optsz);
    for i in 0..nsec {
        let s = sec.add(i * 40);
        let nm = cstr(std::slice::from_raw_parts(s, 8));
        let vsize = *(s.add(8) as *const u32) as usize;
        let va = *(s.add(12) as *const u32) as usize;
        let chars = *(s.add(36) as *const u32);
        // Skip very large sections: gptp.qdp's `.data` is ~19 MB and restoring it
        // wholesale corrupts the game (its heap/state needs the plugin's own
        // save/load — ADR-0004 Layer C). Keep the flat statics of the others.
        if chars & IMAGE_SCN_MEM_WRITE != 0
            && chars & IMAGE_SCN_MEM_SHARED == 0
            && vsize > 0
            && vsize <= max_len
        {
            let name: &'static str = Box::leak(format!("{}.{}", modname, nm).into_boxed_str());
            out.push(RangeSpec {
                name,
                addr: base + va,
                len: vsize,
            });
        }
    }
}
