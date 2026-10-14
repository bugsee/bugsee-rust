//
//  process.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Process identity and liveness, for deciding whether the owner recorded in a
//! claim or a liveness marker is still the process that wrote it.
//!
//! A bare pid is not an identity: pids are recycled, and in a container the
//! restarted app is routinely pid 1 again. An *owner token* therefore pairs the
//! pid with the process's **start time** (an opaque, stable per-process number),
//! so a recycled pid is told apart from the original owner. Where the platform
//! offers no start time the token carries the pid alone and liveness degrades to
//! "a process with that pid exists".

use std::sync::OnceLock;

/// Whether a process with `pid` currently exists.
///
/// Unix: `kill(pid, 0)`; `0` (permitted) or `EPERM` (exists, not permitted) both
/// mean alive, `ESRCH` means gone. Windows: the process can be opened and has not
/// exited. A pid-reuse false positive is closed by [`owner_is_alive`]'s start
/// time comparison, not here.
#[cfg(unix)]
pub(crate) fn is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs only an error/permission check and
    // has no side effects; it is safe to call with any pid value.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub(crate) fn is_alive(pid: u32) -> bool {
    win::with_process(pid, |handle| {
        let mut code = 0u32;
        // SAFETY: `handle` is a valid open process handle for the closure's duration.
        let ok = unsafe { win::GetExitCodeProcess(handle, &mut code) };
        ok == 0 || code == win::STILL_ACTIVE
    })
    // Access denied means the process exists but belongs to someone else.
    .unwrap_or_else(|denied| denied)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn is_alive(_pid: u32) -> bool {
    false
}

/// An opaque per-process start time for `pid`, or `None` if it cannot be read
/// (platform without support, process gone, or not permitted). Only ever compared
/// for equality against a value read the same way.
#[cfg(target_os = "linux")]
pub(crate) fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ...`: comm may contain spaces and parens, so split after
    // the LAST ')'. `starttime` is field 22; `state` (field 3) is index 0 here.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(target_os = "macos")]
pub(crate) fn start_time(pid: u32) -> Option<u64> {
    // SAFETY: an all-zero `proc_bsdinfo` is a valid plain-data value, and
    // `proc_pidinfo` writes at most `size_of` bytes into it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (n == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(windows)]
pub(crate) fn start_time(pid: u32) -> Option<u64> {
    win::with_process(pid, |handle| {
        let mut ft = [win::FileTime::default(); 4];
        // SAFETY: `handle` is valid; the four out-pointers point into `ft`.
        let [creation, exit, kernel, user] = &mut ft;
        let ok = unsafe { win::GetProcessTimes(handle, creation, exit, kernel, user) };
        (ok != 0).then(|| (u64::from(ft[0].high) << 32) | u64::from(ft[0].low))
    })
    .ok()
    .flatten()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) fn start_time(_pid: u32) -> Option<u64> {
    None
}

/// This process's owner token, `"<pid>"` or `"<pid> <start>"`. Cached, but keyed
/// by pid: a `fork` child inherits the cache and must not reuse the parent's token.
pub(crate) fn self_token() -> String {
    static CACHE: OnceLock<(u32, String)> = OnceLock::new();
    let pid = std::process::id();
    let (cached_pid, token) = CACHE.get_or_init(|| (pid, make_token(pid)));
    if *cached_pid == pid {
        token.clone()
    } else {
        make_token(pid)
    }
}

fn make_token(pid: u32) -> String {
    match start_time(pid) {
        Some(start) => format!("{pid} {start}"),
        None => pid.to_string(),
    }
}

/// Parse an owner token (`"<pid>"` or `"<pid> <start>"`; trailing fields, such as
/// a claim's nonce, are ignored). `None` if no pid can be read.
pub(crate) fn parse_token(text: &str) -> Option<(u32, Option<u64>)> {
    let mut it = text.split_whitespace();
    let pid = it.next()?.parse().ok()?;
    let start = it.next().and_then(|s| s.parse().ok());
    Some((pid, start))
}

/// Whether the process an owner token names is still alive *and is the same
/// process that wrote the token*. With a recorded start time, a live process
/// whose start time differs is a recycled pid, i.e. the owner is gone.
pub(crate) fn owner_is_alive(pid: u32, recorded_start: Option<u64>) -> bool {
    if !is_alive(pid) {
        return false;
    }
    match (recorded_start, start_time(pid)) {
        (Some(recorded), Some(now)) => recorded == now,
        // Cannot compare (old-format token, or start time unreadable): the pid
        // exists, so give the owner the benefit of the doubt.
        _ => true,
    }
}

#[cfg(windows)]
mod win {
    //! The few kernel32 calls needed, declared directly: kernel32 is always
    //! linked, so this avoids a dependency for four functions.
    use std::ffi::c_void;

    pub type Handle = *mut c_void;
    pub const STILL_ACTIVE: u32 = 259;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const ERROR_ACCESS_DENIED: i32 = 5;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct FileTime {
        pub low: u32,
        pub high: u32,
    }

    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn CloseHandle(handle: Handle) -> i32;
        pub fn GetExitCodeProcess(handle: Handle, code: *mut u32) -> i32;
        pub fn GetProcessTimes(
            handle: Handle,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }

    /// Run `f` with a query handle to `pid`. `Err(true)` if the process exists but
    /// access was denied, `Err(false)` if it cannot be opened (gone).
    pub fn with_process<T>(pid: u32, f: impl FnOnce(Handle) -> T) -> Result<T, bool> {
        if pid == 0 {
            return Err(false);
        }
        // SAFETY: plain Win32 calls; the handle is closed on every path.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                let denied =
                    std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED);
                return Err(denied);
            }
            let out = f(handle);
            CloseHandle(handle);
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive_with_a_stable_start_time() {
        assert!(is_alive(std::process::id()));
        let a = start_time(std::process::id());
        assert_eq!(a, start_time(std::process::id()));
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        assert!(a.is_some(), "start time must be readable on this platform");
    }

    #[test]
    fn a_pid_with_the_wrong_start_time_is_a_recycled_pid() {
        let me = std::process::id();
        if let Some(start) = start_time(me) {
            assert!(owner_is_alive(me, Some(start)));
            assert!(!owner_is_alive(me, Some(start.wrapping_add(1))));
        }
        assert!(owner_is_alive(me, None), "no recorded start: pid alone");
        assert!(!owner_is_alive(4_294_967_290, Some(1)));
    }

    #[test]
    fn tokens_round_trip() {
        assert_eq!(parse_token("12 345"), Some((12, Some(345))));
        assert_eq!(parse_token("12"), Some((12, None)));
        assert_eq!(parse_token("12 345 deadbeef"), Some((12, Some(345))));
        assert_eq!(parse_token("junk"), None);
        assert_eq!(parse_token(""), None);
        let (pid, start) = parse_token(&self_token()).unwrap();
        assert_eq!(pid, std::process::id());
        assert_eq!(start, start_time(pid));
    }
}
