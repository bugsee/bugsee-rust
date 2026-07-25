//
//  lib.rs
//  bugsee-native
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Native fatal-crash capture.
//!
//! Installs a process crash handler (via the `crash-handler` crate) that, on a
//! fatal signal/exception, writes a tiny **async-signal-safe** crash-info marker
//! next to the current session's capture parts, then lets default handling
//! terminate the process. The marker is picked up on the next launch by
//! `bugsee-core`'s recovery path, which builds the thin native `crash.json`.
//!
//! The crash-time path does no allocation, locking, or formatting through the
//! standard library — only stack buffers and raw `open`/`write`/`close`.
//!
//! See `DESIGN.md` §5, §15.

use std::path::PathBuf;

use crash_handler::{CrashContext, CrashEventResult, CrashHandler};

/// Keeps the native crash handler installed for its lifetime.
pub struct NativeHandler {
    _handler: CrashHandler,
}

/// Install the native crash handler, writing the marker to `crash_info_path` on
/// a fatal crash. Keep the returned guard alive for the handler to stay active.
pub fn install(crash_info_path: PathBuf) -> std::io::Result<NativeHandler> {
    let path_bytes = path_to_cbytes(&crash_info_path);

    let handler = CrashHandler::attach(unsafe {
        crash_handler::make_crash_event(move |cc: &CrashContext| {
            on_crash(&path_bytes, cc);
            // Continue to the previous/default handler so the process terminates
            // with the original signal (and any host reporter also sees it).
            CrashEventResult::Handled(false)
        })
    })
    .map_err(|e| std::io::Error::other(format!("crash handler attach failed: {e}")))?;

    Ok(NativeHandler { _handler: handler })
}

/// NUL-terminated path bytes, prepared at install time so the crash-time path
/// performs no allocation.
fn path_to_cbytes(path: &std::path::Path) -> Vec<u8> {
    let mut bytes = path.to_string_lossy().into_owned().into_bytes();
    bytes.push(0);
    bytes
}

// Linux/Android deliver a POSIX signal — `siginfo` carries signo/code/addr.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn on_crash(path_cbytes: &[u8], cc: &CrashContext) {
    let si = cc.siginfo;
    unsafe {
        write_marker(path_cbytes, si.ssi_signo as i32, si.ssi_code, si.ssi_addr as usize);
    }
}

// Apple platforms deliver a Mach exception — map it to the closest signal.
#[cfg(target_vendor = "apple")]
fn on_crash(path_cbytes: &[u8], cc: &CrashContext) {
    let (signo, code, addr) = match &cc.exception {
        Some(e) => (mach_to_signal(e.kind), e.code as i32, e.subcode.unwrap_or(0) as usize),
        None => (0, 0, 0),
    };
    unsafe {
        write_marker(path_cbytes, signo, code, addr);
    }
}

/// Map a Mach exception kind to the closest POSIX signal number (BSD values).
#[cfg(target_vendor = "apple")]
fn mach_to_signal(kind: u32) -> i32 {
    match kind {
        1 => 11, // EXC_BAD_ACCESS      -> SIGSEGV
        2 => 4,  // EXC_BAD_INSTRUCTION -> SIGILL
        3 => 8,  // EXC_ARITHMETIC      -> SIGFPE
        6 => 5,  // EXC_BREAKPOINT      -> SIGTRAP
        _ => 6,  // default             -> SIGABRT
    }
}

// Other Unixes / Windows: record that a crash occurred; details vary per OS.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn on_crash(path_cbytes: &[u8], _cc: &CrashContext) {
    unsafe {
        write_marker(path_cbytes, 0, 0, 0);
    }
}

/// A small stack-only formatter — no heap, no locks (async-signal-safe).
struct StackBuf {
    buf: [u8; 160],
    len: usize,
}

impl StackBuf {
    fn new() -> Self {
        StackBuf {
            buf: [0; 160],
            len: 0,
        }
    }
    fn byte(&mut self, b: u8) {
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }
    fn s(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.byte(b);
        }
    }
    fn dec(&mut self, mut v: i64) {
        if v < 0 {
            self.byte(b'-');
            v = -v;
        }
        if v == 0 {
            self.byte(b'0');
            return;
        }
        let mut tmp = [0u8; 20];
        let mut n = 0;
        while v > 0 {
            tmp[n] = b'0' + (v % 10) as u8;
            v /= 10;
            n += 1;
        }
        while n > 0 {
            n -= 1;
            self.byte(tmp[n]);
        }
    }
    fn hex(&mut self, mut v: usize) {
        if v == 0 {
            self.byte(b'0');
            return;
        }
        let mut tmp = [0u8; 16];
        let mut n = 0;
        while v > 0 {
            let d = (v & 0xf) as u8;
            tmp[n] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
            v >>= 4;
            n += 1;
        }
        while n > 0 {
            n -= 1;
            self.byte(tmp[n]);
        }
    }
}

/// Write the crash-info marker using only async-signal-safe primitives.
#[cfg(unix)]
unsafe fn write_marker(path_cbytes: &[u8], signo: i32, code: i32, addr: usize) {
    let fd = unsafe {
        libc::open(
            path_cbytes.as_ptr() as *const libc::c_char,
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o600,
        )
    };
    if fd < 0 {
        return;
    }
    let mut b = StackBuf::new();
    b.s(b"signal=");
    b.dec(signo as i64);
    b.s(b"\ncode=");
    b.dec(code as i64);
    b.s(b"\naddress=0x");
    b.hex(addr);
    b.byte(b'\n');
    unsafe {
        let _ = libc::write(fd, b.buf.as_ptr() as *const libc::c_void, b.len);
        let _ = libc::close(fd);
    }
}

#[cfg(not(unix))]
unsafe fn write_marker(_path_cbytes: &[u8], _signo: i32, _code: i32, _addr: usize) {
    // Windows marker writing is added with the Windows exception path.
}
