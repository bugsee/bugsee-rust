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

/// Max crashing-thread frames captured for the dedup signature.
// Gated to the platforms whose handler actually captures frames. Widening
// this cfg is part of adding Windows native capture — the helper is needed
// verbatim there, so it is gated rather than `allow(dead_code)`d.
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
const MAX_FRAMES: usize = 64;
/// Module-map file (base→name) written next to `crash.info` at install time and
/// read back by recovery to turn crash-time frame PCs into stable module offsets.
/// MUST match `bugsee_core::recovery::MODULES_NAME`.
const MODULES_NAME: &str = "crash.modules";

/// Keeps the native crash handler installed for its lifetime.
pub struct NativeHandler {
    _handler: CrashHandler,
}

/// Install the native crash handler, writing the marker to `crash_info_path` on
/// a fatal crash. Keep the returned guard alive for the handler to stay active.
pub fn install(crash_info_path: PathBuf) -> std::io::Result<NativeHandler> {
    let path_bytes = path_to_cbytes(&crash_info_path);

    // Snapshot the loaded module map NOW: `dyld`/`dl_iterate_phdr` are not
    // async-signal-safe, so this cannot run at crash time. Recovery joins these
    // bases with the crash-time frame PCs to derive ASLR-invariant offsets.
    if let Some(dir) = crash_info_path.parent() {
        write_modules_file(&dir.join(MODULES_NAME));
    }

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
#[cfg(unix)]
fn path_to_cbytes(path: &std::path::Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    // Use the real OS bytes (paths are not guaranteed UTF-8); a lossy conversion
    // would target the wrong file and silently lose the crash marker.
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    bytes
}

#[cfg(not(unix))]
fn path_to_cbytes(path: &std::path::Path) -> Vec<u8> {
    let mut bytes = path.to_string_lossy().into_owned().into_bytes();
    bytes.push(0);
    bytes
}

// Linux/Android deliver a POSIX signal — `siginfo` carries signo/code/addr.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn on_crash(path_cbytes: &[u8], cc: &CrashContext) {
    let si = &cc.siginfo;
    let signo = si.ssi_signo as i32;
    let code = si.ssi_code;
    let addr = fault_address(si, signo);

    // F29: persist the GUARANTEED marker (signal/code/addr) FIRST, THEN capture
    // frames. Linux frame capture runs `backtrace::trace_unsynchronized` on the
    // crashing thread, which is NOT async-signal-safe (it may lock / re-fault);
    // if it does, the essential crash info has already been written rather than
    // the whole report being lost.
    unsafe {
        write_marker(path_cbytes, signo, code, addr);
    }
    let mut frames = [0usize; MAX_FRAMES];
    let n = capture_frames(cc, &mut frames);
    unsafe {
        append_frames(path_cbytes, &frames[..n]);
    }
}

/// The fault address for a signal that carries one (SIGSEGV/SIGBUS/SIGILL/
/// SIGFPE/SIGTRAP), read through the real `siginfo_t` layout; `0` otherwise.
///
/// `crash-handler` fills `cc.siginfo` by REINTERPRETING the delivered
/// `siginfo_t` as a `signalfd_siginfo` (a raw byte copy), so `ssi_addr` (offset
/// 72) does not line up with `siginfo_t::si_addr` (offset 16) and reads as ~0
/// (F30). The copied bytes ARE a faithful `siginfo_t`, so read the address back
/// through that layout.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn fault_address(si: &libc::signalfd_siginfo, signo: i32) -> usize {
    const SIGILL: i32 = 4;
    const SIGTRAP: i32 = 5;
    const SIGBUS: i32 = 7;
    const SIGFPE: i32 = 8;
    const SIGSEGV: i32 = 11;
    if !matches!(signo, SIGILL | SIGTRAP | SIGBUS | SIGFPE | SIGSEGV) {
        return 0;
    }
    // SAFETY: `si` is a byte-faithful copy of the original `siginfo_t` (both
    // structs are 128 bytes), so reinterpreting back and reading `si_addr()` (the
    // `_sigfault` union member) is valid for a fault signal.
    unsafe {
        let sip = si as *const libc::signalfd_siginfo as *const libc::siginfo_t;
        (*sip).si_addr() as usize
    }
}

// Apple platforms deliver a Mach exception — map it to the closest signal.
#[cfg(target_vendor = "apple")]
fn on_crash(path_cbytes: &[u8], cc: &CrashContext) {
    // Mach exception kinds + codes we special-case (mach/exception_types.h).
    const EXC_BAD_ACCESS: u32 = 1;
    const EXC_SOFTWARE: u32 = 5;
    // EXC_SOFTWARE code[0] marking a delivered Unix signal; the subcode (code[1])
    // then holds the *signal number*, NOT a fault address.
    const EXC_SOFT_SIGNAL: i64 = 0x10003;
    const SIGSEGV: i32 = 11;
    const SIGABRT: i32 = 6;

    let (signo, code, addr) = match &cc.exception {
        Some(e) => {
            if e.kind == EXC_SOFTWARE && e.code as i64 == EXC_SOFT_SIGNAL {
                // A Unix signal delivered as a Mach exception (e.g. SIGABRT from a
                // Swift fatalError / uncaught NSException): the real signal is in
                // the subcode, and there is no fault address.
                (
                    e.subcode.map(|s| s as i32).unwrap_or(SIGABRT),
                    e.code as i32,
                    0,
                )
            } else if e.kind == EXC_BAD_ACCESS {
                // Only EXC_BAD_ACCESS carries a fault address in the subcode.
                (SIGSEGV, e.code as i32, e.subcode.unwrap_or(0) as usize)
            } else {
                // Other exceptions: map kind→signal; the subcode is not a reliable
                // address for these, so don't record it as one.
                (mach_to_signal(e.kind), e.code as i32, 0)
            }
        }
        None => (0, 0, 0),
    };
    unsafe {
        write_marker(path_cbytes, signo, code, addr);
    }
    let mut frames = [0usize; MAX_FRAMES];
    let n = unsafe { capture_frames(cc, &mut frames) };
    unsafe {
        append_frames(path_cbytes, &frames[..n]);
    }
}

/// Map a Mach exception kind to the closest POSIX signal number (BSD values).
/// `EXC_BAD_ACCESS`/`EXC_SOFTWARE` are handled by the caller; an unknown kind
/// maps to `0` (UNKNOWN) rather than masquerading as `SIGABRT` — which would make
/// it spuriously eligible for panic↔SIGABRT correlation on the next launch.
#[cfg(target_vendor = "apple")]
fn mach_to_signal(kind: u32) -> i32 {
    match kind {
        1 => 11, // EXC_BAD_ACCESS      -> SIGSEGV
        2 => 4,  // EXC_BAD_INSTRUCTION -> SIGILL
        3 => 8,  // EXC_ARITHMETIC      -> SIGFPE
        6 => 5,  // EXC_BREAKPOINT      -> SIGTRAP
        _ => 0,  // EXC_CRASH/RESOURCE/GUARD/… -> UNKNOWN (not SIGABRT)
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
// Unix-only until the Windows marker writers exist; they will use this
// buffer unchanged, which is why it is cfg'd rather than allow'd.
#[cfg(unix)]
struct StackBuf {
    buf: [u8; 160],
    len: usize,
}

#[cfg(unix)]
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
    fn dec(&mut self, v: i64) {
        // Work in unsigned magnitude so `i64::MIN` (whose negation overflows) is
        // handled correctly: `(v as u64).wrapping_neg()` is the two's-complement
        // magnitude for any negative `v`, including `i64::MIN`.
        let mut m: u64 = if v < 0 {
            self.byte(b'-');
            (v as u64).wrapping_neg()
        } else {
            v as u64
        };
        if m == 0 {
            self.byte(b'0');
            return;
        }
        let mut tmp = [0u8; 20];
        let mut n = 0;
        while m > 0 {
            tmp[n] = b'0' + (m % 10) as u8;
            m /= 10;
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

/// Write the guaranteed crash-info marker (signal/code/address/time) using only
/// async-signal-safe primitives. Frame lines are appended separately via
/// [`append_frames`] so this essential header is persisted BEFORE any (possibly
/// non-async-signal-safe) frame capture runs — a re-fault there then loses only
/// the frames, not the whole crash report (F29).
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
    // Crash time (epoch ms) via clock_gettime — async-signal-safe, lets
    // next-launch recovery bound panic↔crash correlation by freshness.
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // `tv_sec`/`tv_nsec` are `time_t`/`c_long`, whose widths vary by target
    // (e.g. 32-bit on some platforms), so the casts to `i64` are needed for
    // portability even where clippy sees them as no-ops on this host.
    #[allow(clippy::unnecessary_cast)]
    let time_ms = if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) } == 0 {
        ts.tv_sec as i64 * 1000 + ts.tv_nsec as i64 / 1_000_000
    } else {
        0
    };
    let mut b = StackBuf::new();
    b.s(b"signal=");
    b.dec(signo as i64);
    b.s(b"\ncode=");
    b.dec(code as i64);
    b.s(b"\naddress=0x");
    b.hex(addr);
    b.s(b"\ntime=");
    b.dec(time_ms);
    b.byte(b'\n');
    unsafe {
        let _ = libc::write(fd, b.buf.as_ptr() as *const libc::c_void, b.len);
        let _ = libc::close(fd);
    }
}

/// Append one `frame=0x<pc>` line per captured absolute PC to an already-written
/// marker (opened `O_APPEND`). Kept separate from [`write_marker`] so the header
/// survives even if the frame capture that produced `frames` faulted (F29). Each
/// line is formatted in its own stack buffer and written immediately (no heap),
/// so an arbitrary frame count never overruns a single fixed buffer.
#[cfg(unix)]
unsafe fn append_frames(path_cbytes: &[u8], frames: &[usize]) {
    if frames.is_empty() {
        return;
    }
    let fd = unsafe {
        libc::open(
            path_cbytes.as_ptr() as *const libc::c_char,
            libc::O_WRONLY | libc::O_APPEND,
            0o600,
        )
    };
    if fd < 0 {
        return;
    }
    for &pc in frames {
        let mut fb = StackBuf::new();
        fb.s(b"frame=0x");
        fb.hex(pc);
        fb.byte(b'\n');
        unsafe {
            let _ = libc::write(fd, fb.buf.as_ptr() as *const libc::c_void, fb.len);
        }
    }
    unsafe {
        let _ = libc::close(fd);
    }
}

#[cfg(not(unix))]
unsafe fn write_marker(_path_cbytes: &[u8], _signo: i32, _code: i32, _addr: usize) {
    // Windows marker writing is added with the Windows exception path.
}

// NOTE: there is deliberately no `cfg(not(unix))` `append_frames`. The
// non-unix `on_crash` captures no frames, so a stub would itself be dead code;
// the Windows exception path adds both together.

// ---------------------------------------------------------------------------
// Module map (captured at install) + crash-time frame capture.
// ---------------------------------------------------------------------------

/// Write the loaded-module map, one `<base_hex>\t<size_hex>\t<code_id>\t<name>`
/// line per module, so recovery can:
/// - turn crash-time frame PCs into ASLR-invariant `pc - base` module offsets,
/// - using `size`, reject a PC that falls OUTSIDE the module's loaded range
///   instead of misattributing it to the nearest-below module (F25), and
/// - report each module's **code id** — the Mach-O `LC_UUID` on Apple, the GNU
///   build-id (`.note.gnu.build-id`) on Linux/Android.
///
/// The code id is what the backend's symbol store is keyed on (the worker
/// extracts the same value from uploaded symbols via `symbolic`), so without it
/// a native crash can never be symbolicated no matter what the user uploads.
/// `code_id` is empty when the module carries none (e.g. an ELF linked without
/// `-Wl,--build-id`); recovery then simply omits it.
fn write_modules_file(path: &std::path::Path) {
    let modules = snapshot_modules();
    if modules.is_empty() {
        return;
    }
    let mut body = String::with_capacity(modules.len() * 64);
    for (base, size, code_id, name) in modules {
        body.push_str(&format!("{base:x}\t{size:x}\t{code_id}\t{name}\n"));
    }
    let _ = std::fs::write(path, body);
}

/// Lowercase hex of raw id bytes (the form the symbol service stores).
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// The trailing path component (module file name).
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Enumerate loaded modules as `(load_base, vm_size, code_id, name)`. NOT
/// async-signal-safe — call only from `install` (normal context). `vm_size` is
/// the module's loaded address extent, used by recovery to reject an
/// out-of-module PC (F25); `code_id` is the Mach-O `LC_UUID` (empty if absent).
#[cfg(target_vendor = "apple")]
fn snapshot_modules() -> Vec<(usize, usize, String, String)> {
    use mach2::dyld::{_dyld_get_image_header, _dyld_get_image_name, _dyld_image_count};
    let mut modules = Vec::new();
    let count = unsafe { _dyld_image_count() };
    for i in 0..count {
        let header = unsafe { _dyld_get_image_header(i) } as usize;
        let name_ptr = unsafe { _dyld_get_image_name(i) };
        if header == 0 || name_ptr.is_null() {
            continue;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(name_ptr) }.to_string_lossy();
        let (size, code_id) = image_info(header);
        modules.push((header, size, code_id, basename(&name)));
    }
    modules
}

// Minimal Mach-O structures needed to compute a loaded image's vm extent and read
// its LC_UUID (mach2 only exposes the 32-bit `mach_header`). Layout matches
// `<mach-o/loader.h>`.
#[cfg(target_vendor = "apple")]
const LC_SEGMENT_64: u32 = 0x19;
#[cfg(target_vendor = "apple")]
const LC_UUID: u32 = 0x1b;

#[cfg(target_vendor = "apple")]
#[repr(C)]
struct MachHeader64 {
    magic: u32,
    cputype: i32,
    cpusubtype: i32,
    filetype: u32,
    ncmds: u32,
    sizeofcmds: u32,
    flags: u32,
    reserved: u32,
}

#[cfg(target_vendor = "apple")]
#[repr(C)]
struct LoadCommand {
    cmd: u32,
    cmdsize: u32,
}

#[cfg(target_vendor = "apple")]
#[repr(C)]
struct SegmentCommand64 {
    cmd: u32,
    cmdsize: u32,
    segname: [u8; 16],
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    maxprot: i32,
    initprot: i32,
    nsects: u32,
    flags: u32,
}

#[cfg(target_vendor = "apple")]
#[repr(C)]
struct UuidCommand {
    cmd: u32,
    cmdsize: u32,
    uuid: [u8; 16],
}

/// One walk of the Mach-O load commands at `header`, returning
/// `(vm_extent, code_id)`:
/// - **vm_extent** — the span from the lowest to the highest `LC_SEGMENT_64` vm
///   address. `base + extent` bounds the module, so recovery can tell whether a
///   crash PC actually belongs to it.
/// - **code_id** — the `LC_UUID` as lowercase hex (empty when the image has no
///   `LC_UUID`). This is the identity the symbol store is keyed on; `symbolic`
///   reads the same value out of an uploaded dSYM/Mach-O.
#[cfg(target_vendor = "apple")]
fn image_info(header: usize) -> (usize, String) {
    if header == 0 {
        return (0, String::new());
    }
    // SAFETY: `header` is a valid loaded mach_header from dyld; we only read the
    // header and walk `ncmds` load commands, each bounded by its own `cmdsize`.
    unsafe {
        let mh = &*(header as *const MachHeader64);
        let mut lc = header + core::mem::size_of::<MachHeader64>();
        let mut min_vmaddr = u64::MAX;
        let mut max_vmend: u64 = 0;
        let mut code_id = String::new();
        for _ in 0..mh.ncmds {
            let cmd = &*(lc as *const LoadCommand);
            if cmd.cmdsize == 0 {
                break; // malformed — avoid an infinite loop
            }
            if cmd.cmd == LC_SEGMENT_64 {
                let seg = &*(lc as *const SegmentCommand64);
                min_vmaddr = min_vmaddr.min(seg.vmaddr);
                max_vmend = max_vmend.max(seg.vmaddr.saturating_add(seg.vmsize));
            } else if cmd.cmd == LC_UUID
                && cmd.cmdsize as usize >= core::mem::size_of::<UuidCommand>()
            {
                let uc = &*(lc as *const UuidCommand);
                code_id = hex_lower(&uc.uuid);
            }
            lc += cmd.cmdsize as usize;
        }
        let extent = if max_vmend > min_vmaddr {
            (max_vmend - min_vmaddr) as usize
        } else {
            0
        };
        (extent, code_id)
    }
}

/// ELF note type for the GNU build-id (`NT_GNU_BUILD_ID`), in a `PT_NOTE`
/// segment whose note name is `"GNU\0"`.
#[cfg(any(target_os = "linux", target_os = "android"))]
const NT_GNU_BUILD_ID: u32 = 3;

/// Scan a loaded `PT_NOTE` segment for the GNU build-id, returning it as
/// lowercase hex.
///
/// Note layout (`Elf_Nhdr` + payloads, each 4-byte aligned):
/// `n_namesz | n_descsz | n_type | name[n_namesz] | desc[n_descsz]`.
///
/// # Safety
/// `start` must point at `len` readable bytes of a loaded PT_NOTE segment.
#[cfg(any(target_os = "linux", target_os = "android"))]
unsafe fn gnu_build_id_from_notes(start: *const u8, len: usize) -> Option<String> {
    let align4 = |n: usize| n.div_ceil(4) * 4;
    let mut off = 0usize;
    // Each iteration consumes a full note; the header alone is 12 bytes.
    while off + 12 <= len {
        let hdr = unsafe { start.add(off) } as *const u32;
        let namesz = unsafe { *hdr } as usize;
        let descsz = unsafe { *hdr.add(1) } as usize;
        let ntype = unsafe { *hdr.add(2) };

        let name_off = off + 12;
        let desc_off = name_off + align4(namesz);
        let next = desc_off + align4(descsz);
        if next > len || desc_off > len {
            break; // malformed/truncated — stop rather than read out of bounds
        }

        if ntype == NT_GNU_BUILD_ID && namesz == 4 {
            let name = unsafe { std::slice::from_raw_parts(start.add(name_off), 4) };
            if name == b"GNU\0" && descsz > 0 {
                let desc = unsafe { std::slice::from_raw_parts(start.add(desc_off), descsz) };
                return Some(hex_lower(desc));
            }
        }
        off = next;
    }
    None
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn snapshot_modules() -> Vec<(usize, usize, String, String)> {
    // Widths of `p_vaddr`/`p_memsz` differ between Elf32/Elf64 phdrs, so the
    // `as u64` casts are needed for portability even where they are no-ops.
    #[allow(clippy::unnecessary_cast)]
    extern "C" fn collect(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        data: *mut libc::c_void,
    ) -> libc::c_int {
        unsafe {
            let modules = &mut *(data as *mut Vec<(usize, usize, String, String)>);
            let info = &*info;
            let base = info.dlpi_addr as usize;
            // One phdr walk yields both the module extent (highest
            // `p_vaddr + p_memsz` over PT_LOAD, so recovery can bound offsets)
            // and the GNU build-id from PT_NOTE — the identity the symbol store
            // is keyed on.
            let mut extent: u64 = 0;
            let mut code_id = String::new();
            if !info.dlpi_phdr.is_null() && info.dlpi_phnum > 0 {
                let phdrs = std::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum as usize);
                for ph in phdrs {
                    if ph.p_type == libc::PT_LOAD {
                        let end = (ph.p_vaddr as u64).saturating_add(ph.p_memsz as u64);
                        extent = extent.max(end);
                    } else if ph.p_type == libc::PT_NOTE && code_id.is_empty() {
                        // PT_NOTE p_vaddr is link-time; add the load bias.
                        let addr = base.wrapping_add(ph.p_vaddr as usize);
                        if addr != 0 {
                            if let Some(id) =
                                gnu_build_id_from_notes(addr as *const u8, ph.p_memsz as usize)
                            {
                                code_id = id;
                            }
                        }
                    }
                }
            }
            let name = if info.dlpi_name.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(info.dlpi_name)
                    .to_string_lossy()
                    .into_owned()
            };
            // The main executable reports an empty name.
            let name = if name.is_empty() {
                "main".to_string()
            } else {
                basename(&name)
            };
            modules.push((base, extent as usize, code_id, name));
        }
        0
    }
    let mut modules: Vec<(usize, usize, String, String)> = Vec::new();
    unsafe {
        libc::dl_iterate_phdr(Some(collect), &mut modules as *mut _ as *mut libc::c_void);
    }
    modules
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
fn snapshot_modules() -> Vec<(usize, usize, String, String)> {
    Vec::new()
}

/// Strip ARM pointer-authentication (PAC) bits from a return address read off the
/// stack. On arm64e these are signed; leaving them in breaks `pc - base` module
/// offsets and the dedup signature. `xpaci` exists on all Apple Silicon
/// (armv8.3+) and is a no-op on an unsigned pointer, so it is safe for both plain
/// arm64 and arm64e binaries. Async-signal-safe (a single register op, no memory
/// access, no side effects).
#[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
#[inline]
fn strip_pac(ptr: usize) -> usize {
    let mut p = ptr;
    // SAFETY: `xpaci` only transforms the register value in place.
    unsafe {
        core::arch::asm!("xpaci {p}", p = inout(reg) p, options(nomem, nostack, preserves_flags));
    }
    p
}

/// x86_64 has no pointer authentication — return addresses are already plain.
#[cfg(all(target_vendor = "apple", target_arch = "x86_64"))]
#[inline]
fn strip_pac(ptr: usize) -> usize {
    ptr
}

/// Capture up to [`MAX_FRAMES`] absolute PCs of the crashing thread into `out`,
/// returning the count. Must be async-signal-safe.
///
/// Apple: the handler runs on a *separate* thread, so read the crashed thread's
/// registers via `thread_get_state` and walk its frame-pointer chain with the
/// fault-safe `mach_vm_read_overwrite`.
#[cfg(target_vendor = "apple")]
unsafe fn capture_frames(cc: &CrashContext, out: &mut [usize; MAX_FRAMES]) -> usize {
    use mach2::kern_return::KERN_SUCCESS;
    use mach2::thread_act::thread_get_state;
    use mach2::thread_status::thread_state_t;

    #[cfg(target_arch = "aarch64")]
    let (pc, mut fp) = {
        use mach2::structs::arm_thread_state64_t;
        use mach2::thread_status::ARM_THREAD_STATE64;
        let mut state = arm_thread_state64_t::new();
        let mut count = arm_thread_state64_t::count();
        let kr = unsafe {
            thread_get_state(
                cc.thread,
                ARM_THREAD_STATE64,
                &mut state as *mut _ as thread_state_t,
                &mut count,
            )
        };
        if kr != KERN_SUCCESS {
            return 0;
        }
        (state.__pc as usize, state.__fp as usize)
    };
    #[cfg(target_arch = "x86_64")]
    let (pc, mut fp) = {
        use mach2::structs::x86_thread_state64_t;
        use mach2::thread_status::x86_THREAD_STATE64;
        let mut state = x86_thread_state64_t::new();
        let mut count = x86_thread_state64_t::count();
        let kr = unsafe {
            thread_get_state(
                cc.thread,
                x86_THREAD_STATE64,
                &mut state as *mut _ as thread_state_t,
                &mut count,
            )
        };
        if kr != KERN_SUCCESS {
            return 0;
        }
        (state.__rip as usize, state.__rbp as usize)
    };

    let mut n = 0;
    if pc != 0 {
        out[n] = pc;
        n += 1;
    }
    // Frame-pointer chain: [fp] = caller's fp, [fp + word] = return address
    // (same layout on arm64 and x86_64).
    while n < out.len() && fp >= 0x1000 {
        let mut slot = [0usize; 2];
        if !unsafe { read_task_mem(cc.task, fp, &mut slot) } {
            break;
        }
        // On arm64e a return address on the stack is PAC-signed; strip the
        // authentication bits so `pc - base` module offsets (and the dedup
        // signature) are stable (F31). No-op for plain arm64 / x86_64.
        let (next_fp, ra) = (slot[0], strip_pac(slot[1]));
        if ra == 0 {
            break;
        }
        out[n] = ra;
        n += 1;
        // Stack grows down, so a valid caller frame is at a strictly higher
        // address; anything else means the chain is corrupt — stop.
        if next_fp <= fp {
            break;
        }
        fp = next_fp;
    }
    n
}

/// Fault-safe read of two words at `addr` from `task` (returns false on bad
/// memory instead of faulting — critical on the crash path).
#[cfg(target_vendor = "apple")]
unsafe fn read_task_mem(
    task: mach2::mach_types::task_t,
    addr: usize,
    dst: &mut [usize; 2],
) -> bool {
    use mach2::vm::mach_vm_read_overwrite;
    let want = core::mem::size_of::<[usize; 2]>() as u64;
    let mut outsize: u64 = 0;
    let kr = unsafe {
        mach_vm_read_overwrite(
            task,
            addr as u64,
            want,
            dst.as_mut_ptr() as u64,
            &mut outsize,
        )
    };
    kr == mach2::kern_return::KERN_SUCCESS && outsize == want
}

/// Linux/Android: the handler runs on the crashing thread, so a signal-safe
/// unwind of the current stack captures the fault (handles x86_64 and aarch64).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn capture_frames(_cc: &CrashContext, out: &mut [usize; MAX_FRAMES]) -> usize {
    let mut n = 0;
    unsafe {
        backtrace::trace_unsynchronized(|frame| {
            if n < out.len() {
                out[n] = frame.ip() as usize;
                n += 1;
                true
            } else {
                false
            }
        });
    }
    n
}
