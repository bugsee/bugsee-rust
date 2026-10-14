//
//  process.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Process liveness probe, used only for **legacy** session markers.
//!
//! Current sessions and claims are arbitrated by OS file locks (see `claim`),
//! which need no pid and are correct across pid namespaces. A session marker
//! written by an older SDK build has no lock beside it, so for those alone the
//! recorded pid is probed here. The probe is inherently single-namespace.

/// Whether a process with `pid` currently exists and has not terminated. A
/// zombie (exited, not yet reaped by its parent) counts as dead.
///
/// Unix: `kill(pid, 0)`; `0` (permitted) or `EPERM` (exists, not permitted) both
/// mean alive, `ESRCH` means gone. Windows: the process can be opened and its
/// handle is not yet signalled.
#[cfg(unix)]
pub(crate) fn is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs only an error/permission check and
    // has no side effects; it is safe to call with any pid value.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    (ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
        && !is_zombie(pid)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `pid (comm) state ...`: comm may contain spaces and parens, so look after
    // the LAST ')'.
    stat.rfind(')')
        .and_then(|i| stat[i + 1..].split_whitespace().next())
        .is_some_and(|state| state == "Z" || state == "X")
}

#[cfg(target_os = "macos")]
fn is_zombie(pid: u32) -> bool {
    // The "short" BSD info, unlike the full one, is still available for a zombie.
    // SAFETY: an all-zero `proc_bsdshortinfo` is valid plain data, and
    // `proc_pidinfo` writes at most `size_of` bytes into it.
    let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdshortinfo).cast(),
            size,
        )
    };
    const SZOMB: u32 = 5;
    // Observed on macOS: for an exited-but-unreaped child `kill(pid, 0)` still
    // succeeds while `proc_pidinfo` returns 0 (no info), whereas any live process,
    // including other users', reports its info. So "no info" means zombie here.
    n == 0 || (n == size && info.pbsi_status == SZOMB)
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_os = "macos"))
))]
fn is_zombie(_pid: u32) -> bool {
    false
}

#[cfg(windows)]
pub(crate) fn is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: plain Win32 calls; the handle is closed on every path.
    unsafe {
        let handle = win::OpenProcess(win::SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            // Access denied means the process exists but belongs to someone else.
            return std::io::Error::last_os_error().raw_os_error()
                == Some(win::ERROR_ACCESS_DENIED);
        }
        // A process handle is signalled once the process has terminated.
        let alive = win::WaitForSingleObject(handle, 0) == win::WAIT_TIMEOUT;
        win::CloseHandle(handle);
        alive
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn is_alive(_pid: u32) -> bool {
    false
}

#[cfg(windows)]
mod win {
    //! The few kernel32 calls needed, declared directly: kernel32 is always
    //! linked, so this avoids a dependency for three functions.
    use std::ffi::c_void;

    pub type Handle = *mut c_void;
    pub const SYNCHRONIZE: u32 = 0x0010_0000;
    pub const WAIT_TIMEOUT: u32 = 258;
    pub const ERROR_ACCESS_DENIED: i32 = 5;

    extern "system" {
        pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        pub fn CloseHandle(handle: Handle) -> i32;
        pub fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive_and_an_impossible_pid_is_not() {
        assert!(is_alive(std::process::id()));
        assert!(!is_alive(0));
        assert!(!is_alive(4_294_967_290));
    }

    #[cfg(unix)]
    #[test]
    fn a_zombie_is_dead() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        // Wait for it to exit without reaping it (a non-consuming probe).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while is_alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!is_alive(pid), "exited but unreaped child counts as dead");
        let _ = child.wait();
    }
}
