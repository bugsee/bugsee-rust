# Bugsee Rust — Platform & Engine Layout

**Status:** Approved design (brainstorm-validated). Not yet implemented.
**Date:** 2026-08-08
**Parent:** [`DESIGN.md`](./DESIGN.md) (product behavior, wire format, capture semantics)
**Scope:** Restructure the workspace into a **shared embeddable engine** with pluggable platform capabilities, a stable **C host vtable**, default backends for desktop/mobile, and **private NDA sibling repos** for Xbox / PlayStation.

This document **amends** parts of `DESIGN.md` that assumed a standalone-Rust-first product and I/O living inside `bugsee-core`. Wire format, signatures, and backend ingestion contracts are unchanged.

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
7. Launch options gate **activation** of subsystems; backends are cached independently (see §5).

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
| P5 | **C callback vtable** is the stable host contract from day one | Rust traits only now; dual forever | Foreign embedders won’t use Rust traits |
| P6 | Standalone Rust calls backends **directly** | Always through C ABI | Avoid useless FFI cost |
| P7 | Console backends in **private sibling repos** | Host-only public API; submodules in this repo | Same pattern as industry console middleware |
| P8 | Approach = **capability-split** crates (not mega native/console) | Two mega platform crates; all host-injected | Mobile ≠ desktop; NDA ≠ public “console” crate |
| P9 | Launch options **gate activation**; provided backends are **cached** for `relaunch` | Drop backend when option off; refuse unused backends | Supports option flips without re-passing vtable |
| P10 | Crash built-in required path: desktop + Android + iOS; console may lack backend | Partial platform everywhere | Only consoles lack a sensible built-in |
| P11 | APIs may break freely (unpublished SDK) | Stable facade during migration | Avoid compatibility shims |
| P12 | Prefer clean extraction/renames over shims | Big compatibility layer | Internal-only consumers |

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
| `bugsee` | Thin Rust facade; wires defaults |
| `bugsee-ffi` | C API + `BugseePlatformVTable`; panic-safe boundaries |
| `bugsee-panic` / `bugsee-tracing` / `bugsee-log` / `bugsee-reqwest` | Optional integrations (not platform backends) |

**Private (not in this workspace):** `bugsee-rust-xbox`, `bugsee-rust-ps` — implement the same traits / vtable under NDA.

**Rules:** core never depends on desktop/console crates. Dependencies point down only. Console code is never `cfg`’d into the public tree.

## 6. Platform capabilities & C vtable

One capability set, two faces: Rust traits and `BugseePlatformVTable`.

| Capability | Purpose | Typical owner |
|---|---|---|
| `Storage` | Durable create/append/read/remove/list under data root | Engine on desktop; host or NDA on console |
| `HttpTransport` | POST/PUT → status + body (3-step API) | Engine on desktop; GDK HTTP / host on console |
| `Clock` | Unix ms + monotonic | Engine (`std`) almost everywhere |
| `Entropy` | Fill random buffer | Engine on desktop; host if restricted |
| `CrashIngest` | Accept marker / frames / modules / optional minidump from a backend | Wired from crash backend or host |

**Launch wiring**
- Engine receives a platform bundle (Rust) or vtable (C).
- Hosts may override **individual** capabilities.
- Optional caps may be absent; required caps for launch depend on options (HTTP+storage still required for a functioning reporter).

**Crash backend resolution** (when `native_crash_capture` / equivalent option is **on**):

1. Embedder-supplied crash backend (cached), else
2. Engine built-in for this OS (desktop / Android / iOS), else
3. Consoles: leave native crash inactive; other platforms: should not occur once built-ins exist

**When the launch option is off:** do not install/init the crash backend; **keep** any provided backend cached for a later `relaunch` that enables it. Turning the option off uninstalls the active handler but does not drop the cache unless the embedder clears/replaces it.

The same **accept/cache vs activate** pattern applies to other option-gated subsystems with swappable backends.

## 7. Lifecycle & data flow

- **Rust launch:** facade builds default desktop platform (+ crash) or caller overrides → core recorder.
- **C launch:** `bugsee_launch(..., &PlatformVTable)`; null entries fall back to linked built-ins where applicable.
- **Happy path:** enqueue → persist via `Storage` → export → `HttpTransport` 3-step upload.
- **Desktop fatal:** crash backend writes marker via crash-safe storage subset → next-launch recovery → upload.
- **Host/Sony fatal:** host calls `bugsee_report_native_crash` (or equivalent) / supplies artifact → core recovery path.
- **Suspend/resume:** `bugsee_notify_lifecycle`; pause uploads; transport reinit after resume (Xbox PLM).
- **Shutdown:** flush/stop semantics unchanged (`BUGSEE_TIMEOUT`, etc.).

## 8. Edge cases & error handling

- **Storage pressure / Xbox write budget:** prefer dropping newest non-crash data over losing a sealed crash artifact.
- **HTTP errors:** typed `NotReady` / `Suspended` / `Transient` / `Permanent` — do not burn permanent-failure budget on not-ready/suspend.
- **Crash-time path:** no alloc/lock in engine fault path; desktop backend uses preallocated buffers + narrow crash-safe storage ops.
- **Shared data dir multi-process:** still deferred; server dedup remains backstop.
- **FFI:** all C exports behind `catch_unwind`; host callbacks isolated (error back, never unwind across FFI).

## 9. Migration & testing

**Migration (breaking OK, keep CI green on desktop)**

1. Add `bugsee-platform` + desktop impl mirroring current behavior.
2. Thread `Platform` through `bugsee-core`, removing direct `std::fs` / ad-hoc time.
3. Rename/extract `bugsee-native` → `bugsee-crash-desktop`; core only sees ingest + option/cache policy.
4. Extend `bugsee-ffi` with vtable + lifecycle.
5. Document private sibling repos; add Android/iOS crash built-ins as follow-ons.

Prefer clean cuts/renames over compatibility shims.

**Testing**

- In-memory mock `Storage` / `HttpTransport` for core unit tests.
- Existing subprocess + `native_recovery_e2e` stay green on desktop.
- Relaunch matrix: cache backend + option off → no handler; option on → activate cache; option off → uninstall, cache retained.
- FFI vtable smoke + panic-across-callback.
- Public CI: ubuntu / macOS / windows only (no console CI in public repo).

## 10. Console research notes (public)

| | Xbox (GDK/GDKX) | PlayStation (Prospero) |
|---|---|---|
| Toolchain | MSVC/Clang, Windows-like; Rust = custom target under NDA | Proprietary C/C++; Rust not official |
| FS | PLS/temp + GameSave; **≤1 GiB writes / 5 min** cert limit | Sandboxed save APIs; TRC atomic saves |
| Net | Wait for network init; WinHTTP / xCurl; suspend/resume | Sony net APIs; host-mediated |
| Crash | Exception/minidump-like paths possible in NDA middleware | Retail path largely Sony CRS; SDK enriches |
| Distribution | Private middleware (GDK portal) | Private middleware (Partners); often SaaS-only pieces |

## 11. References

- [`DESIGN.md`](./DESIGN.md) — product/architecture baseline
- [`DESIGN_CAPTURE.md`](./DESIGN_CAPTURE.md) — capture actors, protobuf parts, snapshot dedup (`DedupStore`)
- [`PROGRESS.md`](./PROGRESS.md) — implementation status
- `report-bundle-structure` — wire contract
- Microsoft GDK docs — networking init, local storage, WinHTTP/xCurl, PLM
- Industry pattern — Sentry console middleware (private ports + engine wrappers)
