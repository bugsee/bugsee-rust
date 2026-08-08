# Bugsee Rust — Platform & Engine Layout

**Status:** Approved design (brainstorm-validated; adversarial-review amended 2026-08-08). Not yet implemented.
**Date:** 2026-08-08
**Parent:** [`DESIGN.md`](./DESIGN.md) (product behavior, wire format, capture semantics)
**Scope:** Restructure the workspace into a **shared embeddable engine** with pluggable platform capabilities, a stable **C host vtable**, default backends for desktop/mobile, and **private NDA sibling repos** for Xbox / PlayStation.

This document **amends** parts of `DESIGN.md` that assumed a standalone-Rust-first product and I/O living inside `bugsee-core`. Wire format, signatures, and backend ingestion contracts are unchanged.

**Companion:** [`DESIGN_CAPTURE.md`](./DESIGN_CAPTURE.md). **Canonical migration order** is §10 of *this* document (capture must not invent a parallel plan).

---

## 1. Understanding Summary

- **What:** Re-layout the Bugsee Rust workspace so `bugsee-core` is a platform-agnostic engine (business logic only), with FS / HTTP / clock / entropy / crash backends supplied by platform crates or host injection.
- **Why:** The SDK is the **basement for most Bugsee SDKs** (not primarily a standalone Rust product), and must extend beyond desktop to Android / iOS / Windows / macOS / Linux / Xbox / PlayStation.
- **Who:** Foreign hosts (C++/Swift/Kotlin/Unity/console titles) embed via **C ABI + Platform vtable**; pure Rust apps use a thin Rust facade that wires backends **directly** (no mandatory C hop).
- **Ownership rule:** Engine owns a capability **where it can**; otherwise the embedder supplies it. Same rule for I/O and crash.
- **Core bar:** Keep `std` (threads/sync/collections OK); **no direct** `std::fs` / net / crash APIs in core.
- **Console reality:** Xbox is Windows-like under GDKX (custom storage quotas, WinHTTP/xCurl, PLM). PlayStation fatal crashes are largely Sony-owned; engine enriches + handles non-fatals. NDA backends live in **private sibling repos**.
- **Non-goals:** `no_std` core; UI/video; publishing console code publicly; API stability (crate is unpublished — breaking changes are fine).

## 2. Assumptions

1. Desktop (Linux / macOS / Windows) keeps engine-owned FS, HTTP, and native crash backends in the public repo.
2. Android / iOS will also have **engine built-in** crash backends (host may override); they are not “host-only.”
3. Consoles may omit a crash backend (PlayStation-style). Handled errors / panics / upload still work.
4. Xbox/PS implementations live in private repos (`bugsee-rust-xbox`, `bugsee-rust-ps`) implementing the same traits / C vtable — not as public workspace members.
5. Generic `reqwest` / `tokio` are not the console HTTP story; desktop may keep sync `ureq` (or equivalent).
6. Existing report-bundle wire contract and appserver/worker routing stay; this is a **client layout** redesign.
7. Launch options gate **activation** of subsystems; backends/providers are cached independently (see §7).

## 3. Non-Functional Requirements

Inherited from `DESIGN.md`, plus:

- **Console lifecycle:** honor suspend/resume; do not use network before platform “network ready”; respect storage write budgets (e.g. Xbox PLS 1 GiB / 5 min).
- **Maintenance:** public tree stays free of NDA material; console selection is link-time / launch-time, not public `cfg(playstation)`.
- **Perf:** Rust apps keep in-process trait calls (no forced FFI).

## 4. Decision Log

| # | Decision | Alternatives | Why |
|---|---|---|---|
| P1 | Product shape = **shared engine**; Rust facade is one thin consumer | Standalone-Rust-first; dual-equal forever | Matches “basement for most Bugsee SDKs” |
| P2 | Engine owns capability where possible; else host supplies | Always host-inject; always engine-own | Fits desktop vs console research |
| P3 | Crash follows same ownership rule, with built-in fallbacks on desktop+mobile | Crash always pluggable; crash always engine | Consoles differ; mobile still gets defaults |
| P4 | Core keeps `std`, but **no direct FS/net/crash** | `no_std`/`alloc` core now; soft defer traits only | Xbox doesn’t need `no_std`; PS blocker is NDA APIs |
| P5 | **C callback vtable** is the stable host contract; land **minimal stub early** (phase 1b) | Traits-only; wait until full migration | Foreign embedders otherwise blocked for a whole cycle |
| P6 | Standalone Rust calls backends **directly** | Always through C ABI | Avoid useless FFI cost |
| P7 | Console backends in **private sibling repos** | Host-only public API; submodules in this repo | Same pattern as industry console middleware |
| P8 | Approach = **capability-split** crates (not mega native/console) | Two mega platform crates; all host-injected | Mobile ≠ desktop; NDA ≠ public “console” crate |
| P9 | Launch options **gate activation**; `BackendCache` + **`relaunch`** | Drop backend when option off; stop+launch only | Mobile/console option flips without re-passing vtables |
| P10 | Crash built-in required path: desktop + Android + iOS; console may lack backend | Partial platform everywhere | Only consoles lack a sensible built-in |
| P11 | APIs may break freely (unpublished SDK) | Stable facade during migration | Avoid compatibility shims |
| P12 | Prefer clean extraction/renames over shims | Big compatibility layer | Internal-only consumers |
| P13 | Unified migration order in §10 (platform then capture) | Parallel redesigns | Keep CI/`native_recovery_e2e` green |
| P14 | `HttpTransport` errors include `NotReady` / `Suspended` | Map into Transient only | Console PLM must not burn upload retry budget |

## 5. Architecture — crate map

```text
                    ┌────────────┐     ┌──────────────────────────┐
   Rust apps  ─────▶│   bugsee   │     │      bugsee-ffi (C)      │◀── Swift/Kotlin/C++/Unity
                    │  facade    │     │  engine API + Platform   │
                    └─────┬──────┘     │  vtable registration     │
                          │            └────────────┬─────────────┘
                          │  direct Rust wiring      │ host vtable / defaults
                          ▼                          ▼
                 ┌─────────────────┐      ┌─────────────────────┐
                 │ platform-*      │      │  crash-* backends     │
                 │ desktop (public)│      │  desktop (public)     │
                 │ xbox/ps (private│      │  mobile built-ins     │
                 └────────┬────────┘      │  host / NDA console   │
                          └────────────┬──┴─────────────────────┘
                                       ▼
                          ┌────────────────────────┐
                          │    bugsee-platform     │  capability traits
                          └────────────┬───────────┘
                                       ▼
                          ┌────────────────────────┐
                          │      bugsee-core       │  std OK; no FS/net/crash I/O
                          └────────────────────────┘
```

| Crate | Role |
|---|---|
| `bugsee-core` | Model, signatures, capture-window logic, queue/recovery state machines, bundle export — all I/O via traits |
| `bugsee-platform` | Capability traits only (`Storage`, `HttpTransport`, `Clock`, `Entropy`, `CrashIngest`, …) |
| `bugsee-platform-desktop` | Default `std::fs` + sync HTTP + clock/entropy |
| `bugsee-crash-desktop` | Today’s `bugsee-native` (POSIX / Mach / SEH) → marker/frames/modules |
| `bugsee-crash-android` / `bugsee-crash-ios` | Built-in mobile crash backends (follow-on; required fallbacks) |
| `bugsee` | Thin Rust facade; wires defaults; owns `BackendCache` + `relaunch` |
| `bugsee-ffi` | C API + `BugseePlatformVTable`; panic-safe boundaries |
| `bugsee-panic` / `bugsee-tracing` / `bugsee-log` / `bugsee-reqwest` | Optional integrations (not platform backends) |

**Private (not in this workspace):** `bugsee-rust-xbox`, `bugsee-rust-ps` — implement the same traits / vtable under NDA.

**Rules:** core never depends on desktop/console crates. Dependencies point down only. Console code is never `cfg`’d into the public tree.

## 6. Platform capability contracts

One capability set, two faces: Rust traits and `BugseePlatformVTable`.

### 6.1 `Storage`

Required ops (paths relative to SDK data root unless absolute platform paths are explicitly allowed):

| Op | Guarantee |
|---|---|
| `create_dir_all`, `remove_file`, `remove_dir_all`, `rename` | `rename` is the durability primitive (temp → final); must be atomic on same volume |
| `open_create_append`, `write_all`, `flush` | Buffered append as today |
| `open_read`, `read`, `read_at` (or equivalent) | Streaming reads for export/dedup hash |
| `read_dir` / list | For GC and part discovery |
| `metadata` (size, is_file) | Eviction / quotas |

**Capability probes** (cached per data volume once at launch):

| Probe | Used by |
|---|---|
| `supports_hardlink` | Snapshot tier 1 |
| `supports_reflink` | Snapshot tier 2 |
| `supports_symlink` | Optional convenience only — **not required** for tier 3 |

**Link ops:** `hard_link(src, dst)` required when probe says yes; `reflink_or_clone` when probe says yes; `symlink` optional.

**Crash-safe subset:** a narrow API for the native crash backend (pre-opened append / write-at to a prepared marker path, no alloc-heavy directory walks). Full `Storage` is **not** used on the fault path.

**Errors:** typed I/O failures (NotFound, Full, Permission, Unsupported, Other). Console impls may map quota exhaustion to Full.

### 6.2 `HttpTransport`

POST/PUT with headers/body → status + body for the 3-step API.

**Error taxonomy** (uploader must honor):

| Error | Retry counter | Behavior |
|---|---|---|
| `NotReady` | **do not** advance abandon budget | Backoff / wait for network-ready or host signal |
| `Suspended` | **do not** advance abandon budget | Pause until `Resume` lifecycle |
| `Transient` | advances | Existing exponential backoff |
| `Permanent` | terminal | Drop / blacklist paths as today |
| `SessionExpired` / `DuplicateDropped` / `TooManySimilar` | as today | Existing API semantics |

Desktop `ureq` impl never returns `NotReady`/`Suspended` unless we later add hooks; console/GDK impls must.

### 6.3 Other capabilities

| Capability | Purpose | Typical owner |
|---|---|---|
| `Clock` | Unix ms + monotonic | Engine (`std`) almost everywhere |
| `Entropy` | Fill random buffer | Engine on desktop; host if restricted |
| `CrashIngest` | Accept marker / frames / modules / optional minidump | Crash backend or host |

### 6.4 Minimal C vtable (phase 1b — land early)

`BugseePlatformVTable` stub (nullable fields = desktop defaults when linked):

- `storage_*` (or opaque host storage handle)
- `http_request`
- `now_unix_ms` / `now_mono_ms` / `random_bytes` (optional)
- `bugsee_notify_lifecycle(Suspend\|Resume\|NetworkReady)`
- `bugsee_report_native_crash(...)` (host → engine)

Keep existing flat C facade API; vtable is a **parallel** launch entry. Do **not** wait for the capture actor rewrite.

## 7. BackendCache, options, and `relaunch`

**Problem today:** `native_crash_capture(false)` drops the handler; there is no cache and no `relaunch`. That contradicts P9 for embedders.

**Process-wide `BackendCache`** (owned by the facade / FFI layer):

- Embedder-supplied crash backend (Rust trait object or C vtable ptr + `user_data`)
- Optional HTTP / Storage overrides
- Registered capture providers / entry types (see capture doc)
- Clear/replace APIs for hosts that need to swap

**`Bugsee::relaunch(LaunchOptions)`** (Rust) / `bugsee_relaunch(...)` (C):

1. Stop active subsystems (uninstall crash handler, stop providers, flush per policy).
2. Apply new options.
3. **Activate** from cache + built-ins per resolution rules — do **not** require the host to re-pass vtables.
4. Same accept/cache vs activate rules as launch.

**Crash resolution** when option **on:** cached embedder backend → OS built-in → (consoles) inactive.

**When option off:** uninstall/deinit active backend; **retain** cache unless embedder clears it.

Document C `user_data` lifetime: host owns memory; engine only copies function pointers; host must not free while SDK launched or while cached unless `bugsee_clear_platform_overrides`.

## 8. Lifecycle & data flow

- **Rust launch:** facade builds default desktop platform (+ crash) or caller overrides → core recorder; seed `BackendCache`.
- **C launch:** `bugsee_launch(..., &PlatformVTable)`; null entries fall back to linked built-ins.
- **Happy path:** enqueue → persist via `Storage` → export → `HttpTransport` 3-step upload.
- **Desktop fatal:** crash backend writes marker via crash-safe storage subset → next-launch recovery → upload.
- **Host/Sony fatal:** host calls `bugsee_report_native_crash` → core recovery path.
- **Suspend/resume / network-ready:** lifecycle notify; pause uploads; transport reinit; `NotReady`/`Suspended` do not burn retry budget.
- **Shutdown:** flush/stop semantics unchanged (`BUGSEE_TIMEOUT`, etc.).

## 9. Edge cases & error handling

- **Storage pressure / Xbox write budget:** prefer dropping newest non-crash data over losing a sealed crash artifact.
- **Shared data dir multi-process:** still deferred; server dedup remains backstop.
- **FFI:** all C exports behind `catch_unwind`; host callbacks isolated (error back, never unwind across FFI).

## 10. Unified migration plan (canonical)

Breaking OK; each phase keeps desktop CI green (`native_recovery_e2e`, `runtime_e2e`, clippy/fmt).

| Phase | Work | Exit criteria |
|---|---|---|
| **0** | Amend parent `DESIGN.md` decisions (done with this pass) | Docs consistent |
| **1a** | Add `bugsee-platform` + desktop `Storage`/`HttpTransport`/`Clock`/`Entropy`; thread through core **keeping JSON parts** | No `std::fs` in core; tests green |
| **1b** | Minimal `BugseePlatformVTable` + lifecycle + `relaunch`/`BackendCache` (crash option cache) | FFI smoke; relaunch matrix green |
| **1c** | Extend `TransportError` with `NotReady`/`Suspended`; uploader honors them | Unit tests |
| **2** | Rename `bugsee-native` → `bugsee-crash-desktop`; crash resolution uses cache | Subprocess crash tests green |
| **3** | Capture actors + registry **behind feature / incremental**, still JSON parts | Facade E2E green |
| **4** | Protobuf parts + `proto/` schemas + DefaultJson mappers; remove JSON parts | Wire + export perf budget met |
| **5** | Snapshot tiers: probe hardlink → reflink → `DedupStore` (FS default; hardlink into `shared_content`) | Windows CI without symlink privilege |
| **6** | C capture register/submit; optional `dedup-redb` CI job | FFI provider smoke |
| **7** | Android/iOS crash built-ins; private sibling docs | Follow-on |

**Do not** start phase 3–5 until 1a–1c are green. Capture doc §9 defers to this table.

**Testing (platform):** mock `Storage`/`HttpTransport`; relaunch matrix; vtable smoke; NotReady/Suspended do not abandon queue items; public CI ubuntu/macOS/windows only.

## 11. Console research notes (public)

| | Xbox (GDK/GDKX) | PlayStation (Prospero) |
|---|---|---|
| Toolchain | MSVC/Clang, Windows-like; Rust = custom target under NDA | Proprietary C/C++; Rust not official |
| FS | PLS/temp + GameSave; **≤1 GiB writes / 5 min** cert limit | Sandboxed save APIs; TRC atomic saves |
| Net | Wait for network init; WinHTTP / xCurl; suspend/resume | Sony net APIs; host-mediated |
| Crash | Exception/minidump-like paths possible in NDA middleware | Retail path largely Sony CRS; SDK enriches |
| Distribution | Private middleware (GDK portal) | Private middleware (Partners); often SaaS-only pieces |

## 12. Adversarial review resolutions (2026-08-08)

| Finding | Resolution |
|---|---|
| F1 relaunch/cache not buildable | §7 `BackendCache` + `relaunch`; phase 1b |
| F2 vtable absent | §6.4 minimal stub; phase 1b before capture rewrite |
| F3 dual migration | §10 unified plan |
| F5 HTTP NotReady/Suspended | §6.2 + P14 + phase 1c |
| F6/F9/F10 parent-doc drift | Amended in `DESIGN.md`; Storage contract in §6.1 |
| F4 Windows symlinks | Owned in `DESIGN_CAPTURE.md` (hardlink-to-blob) |

## 13. References

- [`DESIGN.md`](./DESIGN.md) — product/architecture baseline
- [`DESIGN_CAPTURE.md`](./DESIGN_CAPTURE.md) — capture actors, protobuf parts, snapshot dedup
- [`PROGRESS.md`](./PROGRESS.md) — implementation status
- `report-bundle-structure` — wire contract
- Microsoft GDK docs — networking init, local storage, WinHTTP/xCurl, PLM
- Industry pattern — Sentry console middleware (private ports + engine wrappers)
