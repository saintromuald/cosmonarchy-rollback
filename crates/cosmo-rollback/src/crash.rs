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
