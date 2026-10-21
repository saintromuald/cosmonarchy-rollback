//! Hardware-watchpoint attribution for the extra re-sim RNG call (Iteration 24).
//!
//! The Blizzard LCG is inlined (~15 copies), so there is no function to hook
//! (Iteration 22). Instead we install a **data breakpoint** on `rng_seed`
//! (0x0051CA14) for the game thread, via `SetThreadContext` (user mode may set
//! DR0-DR7 through the thread CONTEXT), and log the address of every instruction
//! that writes it. Arming only around a normal frame vs the shadow re-sim yields
//! the call *site* of the extra RNG consumption.
//!
//! Off unless `CM_ROLLBACK_RNG_TRACE` is set, so normal play is untouched.

#![allow(dead_code, non_snake_case)]

use core::ffi::c_void;
use std::sync::Once;

const RNG_SEED: u32 = 0x0051_CA14;

const CONTEXT_I386: u32 = 0x0001_0000;
const CONTEXT_CONTROL: u32 = CONTEXT_I386 | 0x0000_0001;
const CONTEXT_DEBUG_REGISTERS: u32 = CONTEXT_I386 | 0x0000_0010;

const EXCEPTION_SINGLE_STEP: u32 = 0x8000_0004;

const RF_FLAG: u32 = 0x0001_0000;

/// L0 (bit 0) | RW0=01 write (bits 16-17) | LEN0=11 four bytes (bits 18-19).
const DR7_WATCH: u32 = 0x0000_0001 | (0x1 << 16) | (0x3 << 18); // 0x000D0001

#[repr(C)]
#[derive(Clone, Copy)]
struct FloatingSaveArea {
    data: [u8; 112],
}

#[repr(C)]
struct Context {
    ContextFlags: u32,
    Dr0: u32,
    Dr1: u32,
    Dr2: u32,
    Dr3: u32,
    Dr6: u32,
    Dr7: u32,
    FloatSave: FloatingSaveArea,
    SegGs: u32,
    SegFs: u32,
    SegEs: u32,
    SegDs: u32,
    Edi: u32,
    Esi: u32,
    Ebx: u32,
    Edx: u32,
    Ecx: u32,
    Eax: u32,
    Ebp: u32,
    Eip: u32,
    SegCs: u32,
    EFlags: u32,
    Esp: u32,
    SegSs: u32,
    ExtendedRegisters: [u8; 512],
}

#[repr(C)]
struct ExceptionRecord {
    ExceptionCode: u32,
    ExceptionFlags: u32,
    ExceptionRecord: *mut ExceptionRecord,
    ExceptionAddress: *mut c_void,
    NumberParameters: u32,
    ExceptionInformation: [usize; 15],
}

#[repr(C)]
struct ExceptionPointers {
    ExceptionRecord: *mut ExceptionRecord,
    ContextRecord: *mut Context,
}

type VecHandler = unsafe extern "system" fn(*mut ExceptionPointers) -> i32;

extern "system" {
    fn GetCurrentThread() -> *mut c_void;
    fn GetThreadContext(h: *mut c_void, c: *mut Context) -> i32;
    fn SetThreadContext(h: *mut c_void, c: *const Context) -> i32;
    fn AddVectoredExceptionHandler(first: u32, handler: VecHandler) -> *mut c_void;
}

static INIT: Once = Once::new();

unsafe extern "system" fn veh(info: *mut ExceptionPointers) -> i32 {
    let rec = &*(*info).ExceptionRecord;
    if rec.ExceptionCode == EXCEPTION_SINGLE_STEP {
        let ctx = &mut *(*info).ContextRecord;
        // rand() at 0x4dc4a0 has no stack frame at the fault point, so [Esp] is
        // its return address into the caller. Walk the EBP chain for the rest.
        let caller = *(ctx.Esp as *const u32);
        let mut frames = String::new();
        let mut ebp = ctx.Ebp as usize;
        for _ in 0..8 {
            if ebp < 0x0001_0000 || ebp > 0x7f00_0000 || ebp & 0x3 != 0 {
                break;
            }
            let saved = *(ebp as *const u32) as usize;
            let ret = *((ebp + 4) as *const u32);
            frames.push_str(&format!(" 0x{:08x}", ret));
            if saved <= ebp {
                break;
            }
            ebp = saved;
        }
        crate::log::log(&format!(
            "[cosmo-rollback] RNGWRITE fault=0x{:08x} caller=0x{:08x} bt:{}",
            rec.ExceptionAddress as usize, caller, frames
        ));
        ctx.Dr6 = 0;
        ctx.EFlags |= RF_FLAG;
        return -1; // EXCEPTION_CONTINUE_EXECUTION
    }
    0 // EXCEPTION_CONTINUE_SEARCH
}

fn set_dr(dr0: u32, dr7: u32, flags: u32) -> bool {
    unsafe {
        let h = GetCurrentThread();
        let mut c: Context = core::mem::zeroed();
        c.ContextFlags = flags;
        if GetThreadContext(h, &mut c) == 0 {
            return false;
        }
        c.Dr0 = dr0;
        c.Dr6 = 0;
        c.Dr7 = dr7;
        SetThreadContext(h, &c) != 0
    }
}

pub fn enabled() -> bool {
    std::env::var("CM_ROLLBACK_RNG_TRACE").is_ok()
}

pub fn arm() {
    if !enabled() {
        return;
    }
    INIT.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, veh);
    });
    if set_dr(RNG_SEED, DR7_WATCH, CONTEXT_DEBUG_REGISTERS | CONTEXT_CONTROL) {
        crate::log::log("[cosmo-rollback] RNG watchpoint armed");
    } else {
        crate::log::log("[cosmo-rollback] RNG watchpoint arm FAILED");
    }
}

pub fn disarm() {
    if !enabled() {
        return;
    }
    set_dr(0, 0, CONTEXT_DEBUG_REGISTERS);
}
