# Bugsee Rust — Capture Subsystem (Android-style port)

**Status:** Approved design (brainstorm-validated; adversarial-review amended 2026-08-08). Not yet implemented.
**Date:** 2026-08-08
**Parent:** [`DESIGN.md`](./DESIGN.md) · [`DESIGN_PLATFORM.md`](./DESIGN_PLATFORM.md)
**Reference:** Bugsee Android SDK `contracts/capture` + `capture/` (coordinator, providers, aggregator, exporters).

This document ports Android’s capture actor model into the Rust shared engine, with protobuf on-disk parts, C-reachable extension points, and a pluggable snapshot dedup store (FS-as-DB default, optional `redb`).

**Migration:** follow the **canonical phased plan in [`DESIGN_PLATFORM.md` §10](./DESIGN_PLATFORM.md)** — do not run a parallel capture-only rewrite ahead of Storage / vtable / relaunch.

---

## 1. Understanding Summary

- **What:** Restructure capture around Android’s roles — Coordinator, Provider, DataEntry, EntryFactory, Aggregator, AggregatorSession, PartManager, Exporter, Registry — as Rust traits + private impls, with a C vtable face for embedders.
- **Why:** Flexible, extensible capture is required for the engine-as-basement story (custom providers, including binary/opaque payloads such as future screen capture).
- **On-disk:** Length-delimited **protobuf** per `type_id` stream (not JSON parts). Opaque payloads use protobuf `bytes`. Export maps structured types to Bugsee wire JSON; media needs custom exporters.
- **V1 built-in providers:** events, logs, network, system traces / APM — **no** video / view-hierarchy / input (YAGNI).
- **Snapshot:** hardlink → reflink/CoW → content-addressed dedup. Dedup refs: **FS-as-DB** default; optional **`dedup-redb`**. Content hash: **XXH3-128**. Tier-3 snapshot leaves use **hardlink into `shared_content`** (symlink optional only).
- **Non-goals:** `no_std` capture core; byte-identical Android `.bgsfile` field format; mmap “file DB” for refs; API stability (unpublished SDK).

## 2. Assumptions

1. Platform `Storage` / `Clock` from `DESIGN_PLATFORM.md` underlie PartManager and dedup blob files.
2. Launch options gate provider **activation**; registrations live in process-wide `BackendCache` and survive `relaunch` (same policy as crash backends).
3. C embedders extend capture via vtables + `bugsee_capture_submit(type_id, protobuf_bytes)`; Rust implements traits directly.
4. Switching `dedup-redb` on/off is a clean break (may wipe capture data dir) — no cross-backend migration in v1.
5. Capture worker remains the single writer for aggregate/snapshot/GC; all submits enqueue onto its bounded channel.

## 3. Decision Log

| # | Decision | Alternatives | Why |
|---|---|---|---|
| C1 | Structural Android actor port (Approach A) | Mega-monolithic store; binary-format clone of Android serializer | Extensibility without Java reflection |
| C2 | Protobuf on-disk (length-delimited per type) in versioned `proto/` schemas | JSON parts only; hybrid codec; port BugseeBinarySerializer | Embedder binary payloads; schema evolution |
| C3 | DefaultJson vs Custom exporters | Everything → JSON | Media/frames need custom artifacts |
| C4 | Streaming export into **one reused entry** per type | Load whole file / alloc per record | Android parity; bounded RSS |
| C5 | V1 providers = events, logs, network, traces/APM | Minimal events+logs only | Android-analogue minus UI |
| C6 | C API for register provider/type, submit, custom exporter | Rust-only extensions | Engine basement for non-Rust SDKs |
| C7 | Snapshot tiers: hardlink → reflink → dedup | Hardlink-only; always copy | Match Android + amended DESIGN.md §15 |
| C8 | Dedup metadata: FS-as-DB default | SQLite; mmap file; redb-only | Zero dep; portable via `Storage` |
| C9 | Optional `dedup-redb` feature for refs | Always redb; custom append log | Escape hatch |
| C10 | Blobs always as files under `Storage`; redb holds **refs only** | Store blobs in redb | Large streams |
| C11 | Content id = **XXH3-128** | SHA-256; XXH64 | Extremely fast; local dedup only |
| C12 | Pathological budget: 600s × 1s parts × 15 files ≈ 9k streams | — | ~2s tier-1/3 on APFS; smoke not capacity proof |
| C13 | Tier-3 leaf = **hardlink to `shared_content/<hash>`**; symlink best-effort only | Symlink-required (Android) | Windows without Developer Mode / elevation |
| C14 | Outer record framing may carry timestamp for filter-without-full-decode | Pure protobuf-only frames | Preserve export early-stop; meet perf budget |
| C15 | C/`DataProvider` submit → **bounded MPSC** only | Direct session write from host threads | Preserve single-writer + drop-newest backpressure |

---

## 4. Actor map

```text
┌─────────────────────────────────────────────────────────────┐
│  CaptureCoordinator     single entry (Rust + mirrored in C) │
│  start/stop · register · options · snapshot · export        │
└───────────────┬───────────────────────────┬─────────────────┘
                │                           │
     ┌──────────▼──────────┐     ┌──────────▼──────────┐
     │ DataProvider(s)     │     │ PartManager         │
     │ + C ProviderVTable  │     │ Storage-backed      │
     └──────────┬──────────┘     └──────────┬──────────┘
                │                           │
     ┌──────────▼──────────┐     ┌──────────▼──────────┐
     │ EntryFactory + pool │     │ Aggregator → Session│
     │ C: submit → MPSC    │     │ protobuf streams    │
     └─────────────────────┘     └──────────┬──────────┘
                                            │
                                 ┌──────────▼──────────┐
                                 │ ExportOrchestrator  │
                                 │ stream → 1 entry    │
                                 │ DefaultJson|Custom  │
                                 └─────────────────────┘
```

| Android | Rust |
|---|---|
| `BugseeCaptureCoordinator` | `CaptureCoordinator` |
| `BugseeCaptureDataProvider` | `trait DataProvider` (+ C `ProviderVTable`) |
| `BugseeCaptureDataEntry` | protobuf `Message` (+ thin wrapper) |
| `BugseeCaptureDataEntryProvider` | `trait EntryFactory` |
| `BugseeCaptureAggregator` | `Aggregator` |
| `CaptureAggregatorSession` | `AggregatorSession` (private) |
| `BugseeCaptureExporter` | `trait Exporter` |
| `@BugseeCaptureExporterClass` + registry | `EntryRegistry::register(...)` |
| `BugseeCapturePartManager` | `PartManager` |

**Placement:** contracts + coordinator/aggregator/part/export/dedup in `bugsee-core`. Built-ins in core or integration crates registering at launch (into `BackendCache`). Crash remains a sibling subsystem.

---

## 5. On-disk protobuf, schemas, and export

### 5.1 Stream layout

**Part files:** `<part_dir>/<type_id>.bgs`

```text
stream_header { version: u32, type_id, flags }
repeated record {
  // C14: optional outer framing for timestamp filter without full protobuf decode
  timestamp_ms: u64 LE
  proto_len: u32 LE
  protobuf_bytes  // length-delimited message body
}
```

### 5.2 Schema registry (`proto/` crate)

- Versioned `.proto` (or prost-equivalent) **per built-in `type_id`**.
- `EntryRegistry` binds: `type_id` → message type + on-disk stream version + `EntryFactory` + exporter + controlling options.
- Forward compat: unknown fields retained per proto3 rules; unknown `type_id` streams skipped at export with a log (hard fail in tests).
- CI: encode/decode round-trips; golden vectors per type.

### 5.3 Export

**Decode principle:** never load the whole file. One pooled entry per type; for each record: read timestamp → if outside snapshot window skip/advance → else `reset` entry → decode protobuf into entry → `exporter.process` → repeat.

**DefaultJson:** explicit mapper to Bugsee wire JSON (not naive proto3 JSON names).

**Custom:** required when the artifact is not a JSON event array.

**Perf budget (acceptance):** on a fixture of **60s window / ~100k structured events**, protobuf export wall time ≤ **JSON-concat baseline × 1.5**, peak RSS growth ≤ **2× one max entry + stream buffer**. Gate in CI as a perf unit/bench (not flaky wall-clock on shared runners — compare to same-machine JSON baseline).

---

## 6. Lifecycle, options, and submit path

- Launch: build coordinator; register built-ins into `BackendCache`; accept embedder providers/types.
- Options off → do not start provider; keep registration. Options on → start. `relaunch` flips without re-register (`DESIGN_PLATFORM.md` §7).
- Happy path: provider or C submit **enqueues** onto the existing depth-capped MPSC (drop-newest under pressure) → worker → `AggregatorSession` append.
- **Host threads:** FFI/provider callbacks must not write sessions directly; they only enqueue (or post to the worker). `on_start`/`on_stop` happen-before rules: no submit after `on_stop` completes; submits before `on_start` may use the pre-start buffer (Android parity) or drop — pick one per provider, document in registry.
- Part rotation ~1s (+ size caps); eviction by window/bytes/counts — **never** deletes `shared_content/` blobs without `DedupStore` refcheck.

---

## 7. Snapshot materialization

Pin time bounds, then materialize part files (capability probe cached per volume):

| Priority | Mechanism |
|---|---|
| 1 | **Hard link** live part file → snapshot tree |
| 2 | **Reflink / CoW** when hardlink unavailable |
| 3 | **`DedupStore`**: ingest → `shared_content/<xxh3>` → **hardlink** (preferred) or copy into snapshot tree; symlink only if probe says yes **and** hardlink failed |

Plain copy remains last-resort inside tier 3 when link ops fail for a given file.

### 7.1 `DedupStore`

```rust
trait DedupStore: Send + Sync {
    fn ingest_file(&self, src: &StoragePath) -> Result<ContentHash>;
    fn link_into_snapshot(&self, hash: &ContentHash, snap_dst: &StoragePath) -> Result<()>;
    fn pin_refs(&self, snapshot_id: &str, hashes: &[ContentHash]) -> Result<()>;
    fn unpin_snapshot(&self, snapshot_id: &str) -> Result<()>;
    fn holders(&self, hash: &ContentHash) -> Result<Vec<String>>;
}
```

`ContentHash` = **XXH3-128** hex (streamed; non-cryptographic). Optional paranoid mode: on hash hit, byte-compare before sharing (off by default).

| Backend | Feature | Layout |
|---|---|---|
| `FsDedupStore` | **default** | `shared_content/<hash>` blobs; `content_refs/<hash>/<snap_id>` markers; snapshot leaves = hardlinks to blobs |
| `RedbDedupStore` | `dedup-redb` | Same blobs on `Storage`; refs in `dedup.redb` multimaps |

```toml
[features]
default = []
dedup-redb = ["dep:redb"]
```

### 7.2 GC / eviction invariants (testable)

1. **Snapshot return is synchronous:** materialization + `pin_refs` complete on the capture worker **before** `create_snapshot` / report handle is returned to the caller.
2. **Eviction** may `unlink` live `parts/` files only when not needed for the live window; it **must not** delete `shared_content/<hash>` except via `unpin_snapshot` / GC when `holders` is empty.
3. **Queued reports** hold snapshot dirs (and thus refs) until upload succeeds or the report is abandoned — unpin only then.
4. Mid-pin crash: incomplete snapshot dir is deleted on next launch recovery; orphan blobs without refs are GC’d by a startup sweep.

---

## 8. C API extension surface

| API | Role |
|---|---|
| `bugsee_capture_register_provider(name, &ProviderVTable, options[])` | Host provider → `BackendCache` |
| Provider vtable `on_start` / `on_stop` / `on_event` / … | Lifecycle on worker/policy thread as documented |
| `bugsee_capture_submit(type_id, proto_bytes, len)` | Enqueue only (C15); returns drop/full errors |
| `bugsee_capture_register_type(type_id, &TypeInfo)` | Schema id + DefaultJson hints or `ExporterVTable` |
| Exporter vtable | Custom artifacts; process one entry at a time |

---

## 9. Migration & testing

**Phases 3–6 only** after `DESIGN_PLATFORM.md` §10 phases 1a–1c are green.

Capture-specific exit criteria:

- Actors + registry with JSON parts still green (phase 3).
- `proto/` + protobuf parts + export perf budget (phase 4).
- Snapshot tier matrix including **Windows without symlink privilege** (phase 5).
- C register/submit smoke; optional `dedup-redb` job (phase 6).

**Tests:** mock `Storage`; streaming single-entry reuse; force tier-3 hardlink-to-blob + xxh3 + GC; pin_refs-before-return; eviction does not delete pinned blobs; optional 9k soak; relaunch provider cache; FFI submit from non-worker thread still safe (enqueue).

---

## 10. Adversarial review resolutions (2026-08-08)

| Finding | Resolution |
|---|---|
| F3 dual migration | Defer to platform §10 |
| F4 Windows symlink | C13 hardlink-to-blob |
| F7 parse-free export lost | C14 outer timestamp framing + perf budget §5.3 |
| F8 protobuf unspecified | §5.2 `proto/` registry |
| F11 C submit races | C15 + §6 enqueue-only |
| F12 pin vs eviction | §7.2 invariants |

## 11. Relationship to other docs

- [`DESIGN_PLATFORM.md`](./DESIGN_PLATFORM.md) — engine/platform/C vtable; canonical migration.
- [`DESIGN.md`](./DESIGN.md) — wire bundle; decisions 1/13/15 amended.
- Android `CaptureFileStorage` — behavioral reference for tier-3 (FS markers replace SQLite; hardlink preferred over symlink for portability).

## 12. References

- Android: `contracts/capture/*`, `BugseeCaptureCoordinator.java`, `CaptureAggregator*.java`, `CaptureExporter.java`, `CaptureFileStorage.java`
- Embedded DB research: FS-as-DB default; redb optional
- xxHash / XXH3 — local content identity only
