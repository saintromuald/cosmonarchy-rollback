//! First-chance exception logging for live matches.
//!
//! The engine's own crash report cannot symbolise a fault through a poisoned indirect call
//! (`Fault address FFFFFFFF`), and it runs after every other handler. A vectored handler runs
//! first, so it can log the faulting instruction, the access address and the stack *before*
//! the engine writes its report — which is what localises a crash inside a hook.

use crate::log::log;
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS,
};

/// Windows status code for an access violation.
const ACCESS_VIOLATION: i32 = 0xC000_0005u32 as i32;

unsafe extern "system" fn on_exception(info: *mut EXCEPTION_POINTERS) -> i32 {
    // EXCEPTION_CONTINUE_SEARCH: never swallow the fault, just describe it.
    const CONTINUE_SEARCH: i32 = 0;
    if info.is_null() {
        return CONTINUE_SEARCH;
    }
    let record = (*info).ExceptionRecord;
    let context = (*info).ContextRecord;
    if record.is_null() || context.is_null() {
        return CONTINUE_SEARCH;
    }
    let code: i32 = (*record).ExceptionCode;
    if code != ACCESS_VIOLATION {
        return CONTINUE_SEARCH;
    }

    let ctx = &*context;
    // For an access violation, ExceptionInformation[0] is 0 (read) / 1 (write) / 8 (execute)
    // and [1] is the address the instruction touched.
    let kind = *(*record).ExceptionInformation.as_ptr();
    let touched = *(*record).ExceptionInformation.as_ptr().add(1);
    let what = match kind {
        0 => "read",
        1 => "write",
        8 => "execute",
        _ => "?",
    };

    // The faulting frame's return addresses: the top of the stack is the cheapest backtrace
    // available from inside a handler without a symbol walker.
    let mut frames = String::new();
    let sp = ctx.Esp as *const u32;
    for i in 0..12 {
        let value = *sp.add(i);
        frames.push_str(&format!(" {value:08x}"));
    }

    log(&format!(
        "[cosmo-rollback] UNHANDLED {code:#010x}: {what} at {:#010x}, eip {:#010x}, esp {:#010x}, ebp {:#010x}, stack:{frames}",
        (*record).ExceptionAddress as usize, ctx.Eip, ctx.Esp, ctx.Ebp
    ));

    // Crash forensics: dump the regions that localise a poisoned dialog callback.
    //  - ESI: the BinDlg element whose fxn_interact slot (0x2A) the engine called through
    //  - exe 0x004190ED: the ui_button_click hook site (patched with an E9 jump by gptp)
    //  - exe 0x005014AC: the engine's 15-entry default dialog interaction function table
    //  - the fault address itself (the fetched page, usually NX data)
    //
    // Every read is guarded: the handler itself runs on the faulting thread, and a second
    // fault inside the handler kills the dump (the gptp dialog crash has ESI == 0, which
    // made the unguarded dump fault before it logged anything).
    fn hexdump(name: &str, ptr: *const u8, len: usize) -> String {
        let mut s = String::new();
        s.push_str(&format!("[{name}]"));
        if (ptr as usize) < 0x10000 || len == 0 {
            s.push_str(" (addr too low to read)");
            return s;
        }
        // IsBadReadPtr: TRUE when any byte in [ptr, ptr+len) is unreadable.
        let bad = unsafe {
            windows_sys::Win32::System::Memory::IsBadReadPtr(
                ptr as *const core::ffi::c_void,
                len as _,
            )
        };
        if bad != 0 {
            s.push_str(" (unreadable range)");
            return s;
        }
        let mut off = 0usize;
        while off < len {
            let chunk = len.saturating_sub(off).min(16);
            s.push_str(&format!("\n  +{off:04x}:"));
            unsafe {
                for i in 0..chunk {
                    s.push_str(&format!(" {:02x}", *ptr.add(off + i)));
                }
            }
            off += chunk;
        }
        s
    }
    log(&hexdump("bin_dlg(esi)", ctx.Esi as *const u8, 0x40));
    log(&hexdump("eip_fault", (*record).ExceptionAddress as *const u8, 0x20));
    log(&hexdump("exe_ui_btn_click_hook", 0x0041_90E0 as *const u8, 0x24));
    log(&hexdump("exe_dlg_interaction_table", 0x0050_14A0 as *const u8, 0x48));
    log(&hexdump("exe_490110", 0x0049_0110 as *const u8, 0x60));
    for (name, base, size) in crate::modules::module_list() {
        if name.to_ascii_lowercase().contains("gptp") {
            log(&format!("[cosmo-rollback] crash: gptp base {base:#010x} size {size:#x}"));
        }
    }

    CONTINUE_SEARCH
}

/// Install the handler. Called once when the live client is armed.
pub fn install() {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    unsafe {
        AddVectoredExceptionHandler(1, Some(on_exception));
    }
    log("[cosmo-rollback] live: vectored exception handler installed");
}
