//! MPQDraft plugin interface.
//!
//! The Cosmonarchy QDP hotloader scans `plugins/*.qdp`, `LoadLibraryA`s each one,
//! and requires the export `GetMPQDraftPlugin` (confirmed by disassembling
//! `_qdp-hotloader.qdp`: it logs "Function GetMPQDraftPlugin not found!"). The
//! returned vtable's `InitializePlugin` is where the plugin does its work.
//!
//! This mirrors `aise/src/mpqdraft.rs` (git checkout `.../da4fed6/src/mpqdraft.rs`).

use std::ffi::c_void;
use std::slice;

use windows_sys::Win32::Foundation::HWND;

struct Module;

#[allow(non_snake_case)]
#[repr(C)]
struct Vtable {
    Identify: unsafe extern "stdcall" fn(*mut MpqdraftPlugin, *mut u32) -> u32,
    GetPluginName: unsafe extern "stdcall" fn(*mut MpqdraftPlugin, *mut u8, u32) -> u32,
    CanPatchExecutable: unsafe extern "stdcall" fn(*mut MpqdraftPlugin, *const u8) -> u32,
    Configure: unsafe extern "stdcall" fn(*mut MpqdraftPlugin, HWND) -> u32,
    ReadyForPatch: unsafe extern "stdcall" fn(*mut MpqdraftPlugin) -> u32,
    GetModules: unsafe extern "stdcall" fn(*mut MpqdraftPlugin, *mut Module, *mut u32) -> u32,
    InitializePlugin: unsafe extern "stdcall" fn(*mut MpqdraftPlugin, *mut c_void) -> u32,
    TerminatePlugin: unsafe extern "stdcall" fn(*mut MpqdraftPlugin) -> u32,
}

#[repr(C)]
pub struct MpqdraftPlugin {
    vtable: *const Vtable,
}

static VTABLE: Vtable = Vtable {
    Identify: identify,
    GetPluginName: get_plugin_name,
    CanPatchExecutable: can_patch_executable,
    Configure: configure,
    ReadyForPatch: ready_for_patch,
    GetModules: get_modules,
    InitializePlugin: initialize_plugin,
    TerminatePlugin: terminate_plugin,
};

static mut PLUGIN: MpqdraftPlugin = MpqdraftPlugin {
    vtable: &VTABLE,
};

unsafe extern "stdcall" fn identify(_plugin: *mut MpqdraftPlugin, plugin_id: *mut u32) -> u32 {
    // Unique to this plugin; must not collide with other Cosmonarchy plugins.
    *plugin_id = 0xc050_0001u32;
    1
}

unsafe extern "stdcall" fn get_plugin_name(
    _plugin: *mut MpqdraftPlugin,
    out: *mut u8,
    out_size: u32,
) -> u32 {
    use std::io::Write;
    let mut out = slice::from_raw_parts_mut(out, out_size as usize);
    match write!(
        out,
        "Cosmonarchy rollback plugin {}\0",
        env!("CARGO_PKG_VERSION")
    ) {
        Ok(_) => 1,
        Err(_) => 0,
    }
}

unsafe extern "stdcall" fn can_patch_executable(
    _plugin: *mut MpqdraftPlugin,
    _exe_name: *const u8,
) -> u32 {
    1
}

unsafe extern "stdcall" fn configure(_plugin: *mut MpqdraftPlugin, _hwnd: HWND) -> u32 {
    1
}

unsafe extern "stdcall" fn ready_for_patch(_plugin: *mut MpqdraftPlugin) -> u32 {
    1
}

unsafe extern "stdcall" fn get_modules(
    _plugin: *mut MpqdraftPlugin,
    _modules: *mut Module,
    count: *mut u32,
) -> u32 {
    *count = 0;
    1
}

unsafe extern "stdcall" fn initialize_plugin(
    _plugin: *mut MpqdraftPlugin,
    _mpqdraft: *mut c_void,
) -> u32 {
    crate::plugin::initialize();
    1
}

unsafe extern "stdcall" fn terminate_plugin(_plugin: *mut MpqdraftPlugin) -> u32 {
    1
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "stdcall" fn GetMPQDraftPlugin(out: *mut *mut MpqdraftPlugin) -> u32 {
    unsafe {
        *out = std::ptr::addr_of_mut!(PLUGIN);
    }
    1
}
