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
use windows_sys::Win32::System::Memory::{VirtualQuery, MEMORY_BASIC_INFORMATION};

use crate::ranges::RangeSpec;

const MEM_COMMIT: u32 = 0x1000;
const PAGE_NOACCESS: u32 = 0x01;

/// True when every byte of `[addr, addr+len)` is committed, readable process memory.
///
/// Used to validate engine-owned buffers before the snapshot copies them: a heap
/// allocation the game freed (path pool at game end, tile/creep arrays at map unload)
/// can leave a stale pointer in a `.data` slot, and a memcpy from it faults the
/// process. Module images never unmapped and are not routed through here.
pub fn range_committed(addr: usize, len: usize) -> bool {
    if addr == 0 || len == 0 {
        return false;
    }
    let Some(end) = addr.checked_add(len) else {
        return false;
    };
    let mut cursor = addr;
    unsafe {
        while cursor < end {
            let mut mbi: MEMORY_BASIC_INFORMATION = zeroed();
            if VirtualQuery(
                cursor as *const core::ffi::c_void,
                &mut mbi,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            ) == 0
            {
                return false;
            }
            if mbi.State != MEM_COMMIT || mbi.Protect == PAGE_NOACCESS {
                return false;
            }
            let base = mbi.BaseAddress as usize;
            let next = base.saturating_add(mbi.RegionSize);
            if next <= cursor {
                return false;
            }
            cursor = next;
        }
    }
    true
}

const IMAGE_SCN_MEM_WRITE: u32 = 0x8000_0000;
const IMAGE_SCN_MEM_SHARED: u32 = 0x1000_0000;

/// Writable, non-shared sections of every loaded `.qdp` plugin (ours excluded), up to
/// `max_len` bytes each. The reproducibility probe passes `usize::MAX` so it can *read*
/// gptp's ~19 MB `.data` even though that is never snapshotted.
pub fn writable_sections(max_len: usize) -> Vec<RangeSpec> {
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
                let name = cstr(std::slice::from_raw_parts(
                    me.szModule.as_ptr() as *const u8,
                    me.szModule.len(),
                ));
                let lname = name.to_ascii_lowercase();
                // Skip our own module: its `.data` holds the rollback engine itself
                // (ring, mutex); restoring it mid-step would corrupt us.
                if lname.ends_with(".qdp") && !lname.contains("cosmo-rollback") {
                    add_writable_sections(me.modBaseAddr as usize, &name, max_len, &mut out);
                }
                if Module32Next(snap, &mut me) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
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
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut me: MODULEENTRY32 = zeroed();
        me.dwSize = std::mem::size_of::<MODULEENTRY32>() as u32;
        let mut found = None;
        if Module32First(snap, &mut me) != 0 {
            loop {
                let base = me.modBaseAddr as usize;
                let size = me.modBaseSize as usize;
                if addr >= base && end <= base.saturating_add(size) {
                    found = Some(cstr(std::slice::from_raw_parts(
                        me.szModule.as_ptr() as *const u8,
                        me.szModule.len(),
                    )));
                    break;
                }
                if Module32Next(snap, &mut me) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        found
    }
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
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut me: MODULEENTRY32 = zeroed();
        me.dwSize = std::mem::size_of::<MODULEENTRY32>() as u32;
        let mut found = None;
        if Module32First(snap, &mut me) != 0 {
            loop {
                let modname = cstr(std::slice::from_raw_parts(
                    me.szModule.as_ptr() as *const u8,
                    me.szModule.len(),
                ));
                if let Some(b) = section_base_in(me.modBaseAddr as usize, &modname, name) {
                    found = Some(b);
                    break;
                }
                if Module32Next(snap, &mut me) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        found
    }
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
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, 0);
        if snap == INVALID_HANDLE_VALUE {
            return;
        }
        let mut me: MODULEENTRY32 = zeroed();
        me.dwSize = std::mem::size_of::<MODULEENTRY32>() as u32;
        if Module32First(snap, &mut me) != 0 {
            loop {
                let name = cstr(std::slice::from_raw_parts(
                    me.szModule.as_ptr() as *const u8,
                    me.szModule.len(),
                ));
                let lower = name.to_ascii_lowercase();
                if needles.is_empty() || needles.iter().any(|n| lower.contains(n)) {
                    crate::log::log(&format!(
                        "[cosmo-rollback]   module {:<24} base 0x{:08x} size 0x{:x}",
                        name, me.modBaseAddr as usize, me.modBaseSize
                    ));
                }
                if Module32Next(snap, &mut me) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
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
