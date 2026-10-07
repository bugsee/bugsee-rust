# `bugsee-ffi` C API audit (issue #7)

Status: **design input, not a contract.** The ABI is not frozen and nothing here
promises stability yet. This document records what the C surface is today, what
host bindings (PHP, Ruby, Dart, .NET, Python, Node) need from it, and the order
in which to close the gap. The one thing that ships with it is a CI gate that
makes any accidental ABI change visible (see [Header gate](#header-gate)).

## 1. What exists

15 exported functions over a process-global singleton (`crates/bugsee-ffi/src/lib.rs`):

| Area | Functions |
|---|---|
| Lifecycle | `bugsee_launch(token)`, `bugsee_launch_with_endpoint(token, endpoint)`, `bugsee_stop`, `bugsee_pause`, `bugsee_resume`, `bugsee_is_active` |
| Timeline | `bugsee_log(level, msg)`, `bugsee_event(name)`, `bugsee_event_with_params(name, json)`, `bugsee_trace(name, json)` |
| Identity | `bugsee_set_email`, `bugsee_set_attribute(key, json)` |
| Reporting | `bugsee_capture_exception(name, reason)`, `bugsee_upload`, `bugsee_flush(timeout_ms)` |

Good already: every call is inside a panic boundary (`guarded`) and maps a panic
to `PANIC`; arguments are copied out of the caller's C strings before use, so no
borrow outlives the call; invalid/NULL arguments return `INVALID_ARGUMENT`
instead of crashing; fork-safety (#5) is applied inside the facade, so it holds
for FFI callers without any FFI-level code.

## 2. Gap analysis against the audit principles

Priority: **P0** blocks any binding from being written sanely, **P1** blocks a
production-quality binding, **P2** polish / later.

### 2.1 Opaque handles and configuration — P0

- There is **no configuration surface**. Launch takes a token and (in a second
  function) an endpoint. A host cannot set `data_dir`, app version/build/package
  id, `sample_rate`, `native_crash_capture`, `system_telemetry`, window/size
  caps, or `report_panics_from_hook` — all of which exist in `LaunchOptions` and
  most of which a managed runtime *must* set (a JVM/V8/CLR host owns native
  crash handling and needs `native_crash_capture = false`, or the coexistence
  work in #6 has to carry the whole load).
- Adding `bugsee_launch_with_X` per option does not scale and is exactly the
  shape that breaks ABI each time.
- **Direction:** an opaque options handle (`bugsee_options_t*`) with
  `bugsee_options_new(abi_version)`, typed setters, `bugsee_options_free`, and
  `bugsee_launch(options)`. Setters take the value, not a struct, so adding an
  option never changes an existing signature (no size/version field is then
  needed on a struct — there is no public struct). Keep the **client** a global
  singleton for now: the core (`RECORDER`) is one per process, so a client
  handle would be a lie. Revisit if the core ever supports several.
- The deprecated `bugsee_launch*` entry points can stay as thin wrappers until
  the freeze.

### 2.2 Versioning — P0

- No ABI version, no runtime version query, nothing a binding can check at load
  time. A binding built against header N loaded against library N+1 fails at a
  random call instead of at load.
- **Direction:** `uint32_t bugsee_abi_version(void)` (monotonic integer, bumped
  on any incompatible change, **independent of the crate version**), a
  `BUGSEE_ABI_VERSION` macro in the header, `const char *bugsee_version(void)`
  (static string, never freed). The options constructor takes the ABI version the
  caller was built against and fails with a distinct status on mismatch.
  Document that, until the freeze, any release may bump it.
- Status enum: document that callers must treat **unknown values as an error**,
  so adding codes is non-breaking.

### 2.3 Strings and ownership — P1

- Today **every string is caller-owned and borrowed for the call only**, and the
  SDK returns no pointers at all. That is the right default and means nothing
  needs a `bugsee_free` yet — but it is nowhere written down. Write it in the
  header (it is the single most-asked FFI question).
- Encoding is UTF-8; invalid UTF-8 → `INVALID_ARGUMENT` (fine, but undocumented;
  Windows hosts hold UTF-16 and must convert).
- NULL handling is **inconsistent**: `bugsee_capture_exception` silently accepts
  NULL (defaults to `"Exception"` / `""`), every other function rejects it.
  `bugsee_set_email(NULL)` is an error, so a host **cannot clear** an email, and
  there is no `clear_attribute` at all (the Rust API has both). Pick one rule:
  NULL is `INVALID_ARGUMENT` unless the parameter is documented nullable.
- NUL-terminated strings cap the data a host can pass (no embedded NUL, a copy
  for hosts whose strings are length-prefixed, e.g. PHP, Ruby, Dart). Add
  `(ptr, len)` variants for the payload-bearing calls (JSON bodies, messages).
- `bugsee_log` maps any unknown level (0, -3, 99) to *Verbose* instead of
  rejecting it. Make out-of-range an `INVALID_ARGUMENT`.

### 2.4 Thread-safety and lifecycle contract — P1

Behaviour (verified in `crates/bugsee/src/api.rs`) vs. what the header says
(nothing):

| Situation | Today | Gap |
|---|---|---|
| Any function, any thread | safe (global mutex, blocking calls run off-lock) | document |
| Capture call **before launch** / **after stop** | silently does nothing and returns `OK` | `bugsee_flush` returns `NOT_LAUNCHED`, the rest do not — inconsistent. Decide: either all return `NOT_LAUNCHED`, or none do and it is documented as "no-op when not launched" |
| `launch` while already launched | replaces the recorder (joins the old one) silently | should be an explicit status or documented replace-semantics |
| After `fork()` in the child | rebuilt lazily on the next call (#5) | document; `bugsee_stop` in a forked child is not covered by the fork-safety tests today — verify before promising it |
| Calling the SDK from a host callback (future) | undefined | must be specified when callbacks land (§2.7) |
| `bugsee_stop` | joins worker threads, can block | document; interacts with #8 (shutdown/flush) |
| Host built with `panic = "abort"` | n/a (the cdylib sets its own profile) | document that the library is built `panic = "unwind"` |

### 2.5 Error reporting — P0

- A status code is all a caller gets. `INTERNAL_ERROR` is a flattened
  `io::Error` (data dir not creatable? disk full? no transport?) and `PANIC`
  discards the panic message. A binding cannot give its user a usable error.
- **Direction:** per-thread last-error: `const char *bugsee_last_error(void)`
  (valid until the next SDK call on the same thread, never freed by the caller,
  NULL if none) plus `bugsee_status_string(status)`. Add the codes the audit
  above implies (`ALREADY_LAUNCHED`, `ABI_MISMATCH`, `IO_ERROR`) rather than
  overloading `INTERNAL_ERROR`.
- Panic safety itself is sound (`guarded`; `bugsee_is_active` has its own guard).
  Add a test that a panic inside the *callback* path (§2.7) is also contained.

### 2.6 Report externally captured errors — P0

- `bugsee_capture_exception(name, reason)` is the only entry point and it is
  **lossy by design**: it formats `"name: reason"` into a *message* issue and
  attaches the **Rust** backtrace of the FFI call site — i.e. the stack of the
  binding's glue, not of the exception the host caught. The host's managed stack
  (the thing the user needs) is dropped.
- **Direction:** `bugsee_report_error(json, len)` taking a structured document:
  exception type/message, `frames[]` (function, module/class, file, line,
  column, address, `in_app`), cause chain, `threads[]`, `handled`, host runtime
  (`{"name":"php","version":"8.3"}`), tags/metadata. Core already has the
  landing point: `errors::build_handled_error(name, reason, causes, frames, ts)`
  takes frames, which today only `frames::capture()` fills. Add a JSON → `Frame`
  mapping and converge on that function so host errors go through the same
  bundle writer, signatures and dedupe as native ones.
- **Unhandled** host exceptions (the runtime is about to die) need a
  synchronous, durable path — same marker/recovery mechanism as the panic path —
  so `handled = false` must not go through the asynchronous queue. Needs its own
  design note.
- JSON first (language-neutral, one entry point); a typed builder API can follow
  if profiling shows the parse matters.

### 2.7 Pluggable transport — P1 (design-heavy)

- The core already abstracts delivery (`bugsee_core::transport::Transport`,
  injectable with `LaunchOptions::with_transport`), but **the FFI exposes
  nothing**, and the trait is the wrong level for hosts: its three methods are
  the Bugsee 3-step server protocol (register session → create issue → PUT
  bundle), including the server's dedupe/blacklist semantics. A host should not
  have to reimplement that.
- **Direction:** split the shipped `HttpTransport` into (a) the protocol layer
  (session, issue, presigned PUT, `12003`/`12004`/`401`/`429` handling — stays
  in the core) and (b) a one-method HTTP primitive: `send(method, url, headers,
  body) -> {status, headers, body}`. The FFI exposes only (b) as a callback
  with a `user_data` pointer; the core keeps ownership of **queueing, ordering,
  backoff and retry** (already in `queue.rs`). That also lets hosts reuse their
  own proxy/TLS/cert stores, which is the usual reason to want this.
- Constraints to specify before writing it: the callback runs on an **SDK
  thread** (the uploader), so bindings must be able to attach a foreign thread
  (JVM `AttachCurrentThread`, Python GIL, .NET reverse-P/Invoke); it may block;
  it must not call back into the SDK; its buffers are valid only for the call;
  and what a callback failure means (retryable vs. permanent) must be explicit
  in its return value.

### 2.8 Missing coverage vs. the Rust API — P1/P2

The Rust facade has these; the C API does not: `clear_attribute` /
`clear_all_attributes`, `upload_with(meta)` and deferred reports with
attachments (`create_report`), APM transactions/spans (needs span handles),
`capture_network` / `capture_log` / `capture_breadcrumb` (a PHP/Ruby HTTP-client
integration wants to push network entries), `before_send` / `before_breadcrumb` /
`on_report_dropped` callbacks. Callbacks share the thread/reentrancy questions
of §2.7, so they should be designed together. Data-carrying calls should take
JSON, consistent with `bugsee_event_with_params`.

### 2.9 Symbol visibility and linking — P1

- The header is plain declarations with no export macro: nothing for
  `__declspec(dllimport)` on Windows or `visibility("default")` elsewhere.
  Add a `BUGSEE_API` macro (`BUGSEE_STATIC` opt-out).
- The `cdylib` exports its `#[no_mangle]` set; the `staticlib` additionally
  carries Rust `std` and every dependency's symbols. Linking it next to another
  Rust static library in the same process is a known source of duplicate
  symbols. Add a linker version script (ELF) / exported-symbols list (Mach-O) /
  `.def` (PE) restricting exports to `bugsee_*`, and verify with `nm`.
- Document the native libraries a static link needs (Linux, as reported by `--print native-static-libs`: `-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc`;
  Windows: the `ws2_32`/`userenv`/`ntdll` set from
  `cargo rustc -- --print native-static-libs`) and ship a `bugsee.pc`.
- Both static and dynamic builds already exist (`crate-type`); neither is
  currently exercised from C in CI (see §3).

### 2.10 Tests — P1

The existing tests (`tests/lifecycle.rs`) call the ABI *from Rust*. Nothing
compiles a C program against the header and links the real artifacts. A C smoke
test for both the static and the shared library (launch → capture → flush →
stop, plus every error path) would catch export, calling-convention and
header mistakes that Rust-to-Rust tests cannot.

## 3. Header gate (shipped with this audit)

- `crates/bugsee-ffi/include/bugsee.h` is now **generated by cbindgen** from
  `src/lib.rs` (`cbindgen.toml`); do not edit it by hand.
- `scripts/ffi-header.sh gen` regenerates it; `scripts/ffi-header.sh check` (run
  in CI on Linux) fails if
  1. the committed header differs from what the source generates (an ABI change
     now shows up as a header diff in the PR that made it),
  2. the library's exported `bugsee_*` symbols differ from the header's
     declarations, or
  3. the header does not compile as C99 or C++11 with warnings as errors.
- **One deliberate pre-1.0 break rode along:** the status constants were
  `BUGSEE_OK`, `BUGSEE_PANIC`, … and are now `BUGSEE_STATUS_OK`,
  `BUGSEE_STATUS_PANIC`, … (cbindgen's qualified naming, which keeps the
  enum from colliding with other libraries' `OK`/`PANIC`). The numeric values
  are unchanged, and no code in this repository used the C names. Nothing was
  promised stable.
- The log-level constants (`BUGSEE_LEVEL_*`) are now defined once, in Rust, and
  emitted into the header.
- Not covered yet: a diff against the *previous release's* header with a
  compatibility verdict (added = fine, changed/removed = break). Worth adding at
  the freeze; until then "the header changed" is the signal.

## 4. Prioritised change list

| # | Change | Pri | Depends on |
|---|---|---|---|
| 1 | ABI version + `bugsee_version`; document unknown-status rule | P0 | — |
| 2 | Opaque options handle + setters; `bugsee_launch(options)` | P0 | 1 |
| 3 | Last-error + `bugsee_status_string`; new status codes | P0 | 1 |
| 4 | `bugsee_report_error` (structured external errors) + unhandled-exception path | P0 | 3 |
| 5 | Header documentation pass: ownership, threads, lifecycle table (§2.3–2.4); consistent NULL / level / not-launched rules; `(ptr,len)` variants; `clear_*` | P1 | — |
| 6 | Split `HttpTransport` into protocol + HTTP primitive; callback transport in FFI | P1 | 2, 3 |
| 7 | Export macro, linker export lists (ELF/Mach-O/PE), `bugsee.pc`, native-libs doc | P1 | — |
| 8 | C smoke tests against the static and shared artifacts in CI | P1 | 7 |
| 9 | Remaining API coverage: network/log/breadcrumb push, `before_send` & co. callbacks, deferred reports, APM handles | P2 | 2, 6 |
| 10 | Release-over-release header compatibility check; ABI freeze | P2 | all |

Items 1–3 and 5 are small and should land together; 4 and 6 are the ones that
need design discussion first (4: unhandled path durability; 6: foreign-thread
callback contract).
