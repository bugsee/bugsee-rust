//
//  win32.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The few kernel32 calls this crate needs, declared ONCE.
//!
//! kernel32 is always linked, so declaring them directly avoids a dependency.
//! They live in one module on purpose: two declarations of the same symbol with
//! different Rust types (a `HANDLE` as `*mut c_void` in one place, `usize` in
//! another) trip `clashing_extern_declarations`, a crate-wide lint. Any new
//! kernel32 call in this crate goes here.

use std::ffi::c_void;

pub type Handle = *mut c_void;

pub const SYNCHRONIZE: u32 = 0x0010_0000;
pub const WAIT_OBJECT_0: u32 = 0;
pub const WAIT_TIMEOUT: u32 = 258;
pub const ERROR_ACCESS_DENIED: i32 = 5;

extern "system" {
    pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    pub fn CloseHandle(handle: Handle) -> i32;
    pub fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
    pub fn CreateEventW(
        attrs: *const c_void,
        manual_reset: i32,
        initial: i32,
        name: *const u16,
    ) -> Handle;
    pub fn SetEvent(event: Handle) -> i32;
    pub fn GetCurrentThreadId() -> u32;
    pub fn QueryPerformanceCounter(value: *mut i64) -> i32;
    pub fn QueryPerformanceFrequency(value: *mut i64) -> i32;
}
