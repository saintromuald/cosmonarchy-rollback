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
    log(&format!(
        "[cosmo-rollback] UNHANDLED kind={kind} ({what}) touched={touched:#010x}: eip {:#010x}, esp {:#010x}, ebp {:#010x}",
        ctx.Eip, ctx.Esp, ctx.Ebp
    ));

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

    // Guarded byte dump: the handler runs on the faulting thread and a second fault here
    // would kill the dump (the gptp dialog crash has ESI == 0, which made the unguarded
    // dump fault before it logged anything).
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

    // The stack a few frames deep: the faulting frame's arguments and the caller's
    // frame (the poisoned pointer usually comes from the caller, which sits just above).
    log(&hexdump("stack_window", (ctx.Esp as usize).saturating_sub(0x40) as *const u8, 0x100));

    // The faulting page's protection: distinguishes a poisoned pointer (the page is
    // fine, the access is out of range) from a protection flip (the page itself lost
    // READ/EXECUTE). VirtualQuery is cheap and never faults.
    let mbi = {
        let mut mbi: windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION =
            unsafe { std::mem::zeroed() };
        let ok = unsafe {
            windows_sys::Win32::System::Memory::VirtualQuery(
                (*record).ExceptionAddress as *const core::ffi::c_void,
                &mut mbi,
                std::mem::size_of_val(&mbi),
            )
        };
        if ok == 0 {
            "[cosmo-rollback] crash: VirtualQuery failed".to_string()
        } else {
            format!(
                "[cosmo-rollback] crash: page at {:#010x} base {:#010x} protect {:#x} state {:#x} type {:#x}",
                (*record).ExceptionAddress as usize,
                mbi.BaseAddress as usize,
                mbi.Protect,
                mbi.State,
                mbi.Type,
            )
        }
    };
    log(&mbi);
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
