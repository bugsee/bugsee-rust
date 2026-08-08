# Bugsee Rust — Capture Subsystem (Android-style port)

**Status:** Approved design (brainstorm-validated). Not yet implemented.
**Date:** 2026-08-08
**Parent:** [`DESIGN.md`](./DESIGN.md) · [`DESIGN_PLATFORM.md`](./DESIGN_PLATFORM.md)
**Reference:** Bugsee Android SDK `contracts/capture` + `capture/` (coordinator, providers, aggregator, exporters).

This document ports Android’s capture actor model into the Rust shared engine, with protobuf on-disk parts, C-reachable extension points, and a pluggable snapshot dedup store (FS-as-DB default, optional `redb`).

---

## 1. Understanding Summary

- **What:** Restructure capture around Android’s roles — Coordinator, Provider, DataEntry, EntryFactory, Aggregator, AggregatorSession, PartManager, Exporter, Registry — as Rust traits + private impls, with a C vtable face for embedders.
- **Why:** Flexible, extensible capture is required for the engine-as-basement story (custom providers, including binary/opaque payloads such as future screen capture).
- **On-disk:** Length-delimited **protobuf** per `type_id` stream (not JSON parts). Opaque payloads use protobuf `bytes`. Export maps structured types to Bugsee wire JSON; media needs custom exporters.
- **V1 built-in providers:** events, logs, network, system traces / APM — **no** video / view-hierarchy / input (YAGNI).
- **Snapshot:** hardlink → reflink/CoW → content-addressed dedup. Dedup refs: **FS-as-DB** default; optional **`dedup-redb`**. Content hash: **XXH3-128**.
- **Non-goals:** `no_std` capture core; byte-identical Android `.bgsfile` field format; mmap “file DB” for refs; API stability (unpublished SDK).

## 2. Assumptions

1. Platform `Storage` / `Clock` from `DESIGN_PLATFORM.md` underlie PartManager and dedup blob files.
2. Launch options gate provider **activation**; registrations are **cached** across `relaunch` (same policy as crash backends).
3. C embedders extend capture via vtables + `bugsee_capture_submit(type_id, protobuf_bytes)`; Rust implements traits directly.
4. Switching `dedup-redb` on/off is a clean break (may wipe capture data dir) — no cross-backend migration in v1.
5. Capture worker remains the single writer for aggregate/snapshot/GC.

## 3. Decision Log

| # | Decision | Alternatives | Why |
|---|---|---|---|
| C1 | Structural Android actor port (Approach A) | Mega-monolithic store; binary-format clone of Android serializer | Extensibility without Java reflection |
| C2 | Protobuf on-disk (length-delimited per type) | JSON parts only; hybrid Protobuf\|Opaque codec; port BugseeBinarySerializer | Embedder binary payloads; `bytes` covers opaque; no second codec |
| C3 | DefaultJson vs Custom exporters | Everything → JSON | Media/frames need custom artifacts |
| C4 | Streaming export into **one reused entry** per type | Load whole file / alloc per record | Android parity; bounded RSS |
| C5 | V1 providers = events, logs, network, traces/APM | Minimal events+logs only | Android-analogue minus UI |
| C6 | C API for register provider/type, submit, custom exporter | Rust-only extensions | Engine basement for non-Rust SDKs |
| C7 | Snapshot tiers: hardlink → reflink → dedup | Hardlink-only; always copy | Match Android + DESIGN.md intent |
| C8 | Dedup metadata: FS-as-DB default | SQLite; mmap file; redb-only | Zero dep; portable via `Storage`; ~9k files OK |
| C9 | Optional `dedup-redb` feature for refs | Always redb; custom append log | Escape hatch without abandoning FS default |
| C10 | Blobs always as files under `Storage`; redb holds **refs only** | Store blobs in redb | Large streams / video-sized `bytes` |
| C11 | Content id = **XXH3-128** | SHA-256; XXH64 | Extremely fast; enough for local dedup |
| C12 | Pathological budget: 600s × 1s parts × 15 files ≈ 9k streams | — | Measured ~2s tier-1/tier-3 on APFS; ~4.5×10⁴ inodes tier-3 |

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
     │ C: submit(bytes)    │     │ protobuf streams    │
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

**Placement:** contracts + coordinator/aggregator/part/export/dedup in `bugsee-core`. Built-ins in core or integration crates (`bugsee-reqwest`, …) registering at launch. Crash remains a sibling subsystem (not a capture provider).

---

## 5. On-disk protobuf & export

**Part files:** `<part_dir>/<type_id>.bgs` — stream header (version, type_id, flags) + length-delimited protobuf messages.

**Registry entry:** `type_id`, proto schema, `EntryFactory`, exporter (`DefaultJson { map_to_wire }` | `Custom`), controlling launch-option keys.

**Export decode principle:** never load the whole file. One pooled entry per type; for each record: `reset` → decode into entry → `exporter.process` → repeat. C custom exporters see one entry (or protobuf view) at a time.

**DefaultJson:** explicit mapping to Bugsee wire JSON (do not assume naive proto3 JSON names). **Custom:** required when the bundle artifact is not a JSON event array (e.g. future video).

---

## 6. Lifecycle & options

- Launch: build coordinator; register built-ins; accept embedder providers/types into cache.
- Options off → do not start provider; keep registration. Options on → start. `relaunch` flips without re-register.
- Happy path: provider/C submit → aggregator queue → session append protobuf.
- Part rotation ~1s (+ size caps); eviction by `max_window` / `max_bytes` / count caps.
- Suspend/resume: honor platform lifecycle (pause uploads/providers as appropriate).

---

## 7. Snapshot materialization

Pin time bounds, then materialize part files:

| Priority | Mechanism |
|---|---|
| 1 | **Hard link** into snapshot tree |
| 2 | **Reflink / CoW** (APFS `clonefile`, Linux `FICLONE`, …) |
| 3 | **`DedupStore`** content-addressed share |

Probe once per data volume; cache capability. Export streams from the snapshot tree regardless of tier.

### 7.1 `DedupStore` abstraction

```rust
trait DedupStore: Send + Sync {
    fn ingest_file(&self, src: &StoragePath) -> Result<ContentHash>;
    fn link_into_snapshot(&self, hash: &ContentHash, snap_dst: &StoragePath) -> Result<()>;
    fn pin_refs(&self, snapshot_id: &str, hashes: &[ContentHash]) -> Result<()>;
    fn unpin_snapshot(&self, snapshot_id: &str) -> Result<()>;
    fn holders(&self, hash: &ContentHash) -> Result<Vec<String>>;
}
```

`ContentHash` = **XXH3-128** (streamed over file bytes; non-cryptographic, local dedup only).

| Backend | Feature gate | Layout |
|---|---|---|
| `FsDedupStore` | **default** | `shared_content/<xxh3-hex>`, `content_refs/<hash>/<snap_id>` markers, symlinks in snapshot |
| `RedbDedupStore` | `dedup-redb` | Same blob files; refs in `dedup.redb` multimaps (`hash→snap`, `snap→hash`) |

```toml
# bugsee-core
[features]
default = []
dedup-redb = ["dep:redb"]
```

Wiring is `cfg`-selected at build time (no runtime switch). Single capture-worker writer. Switching features may require wiping the capture data dir.

**Not:** preallocated mmap file-as-DB for FS backend; storing large blobs inside redb.

**Inode note (pathological 600×15 unique, tier 3):** order ~4.5×10⁴ inodes with live+one snapshot — acceptable on desktop; prefer tiers 1–2 when available.

---

## 8. C API extension surface (sketch)

| API | Role |
|---|---|
| `bugsee_capture_register_provider(name, &ProviderVTable, options[])` | Host provider; options gate activation |
| Provider vtable `on_start` / `on_stop` / `on_event` / … | Lifecycle |
| `bugsee_capture_submit(type_id, proto_bytes, len)` | Push entry without Rust types |
| `bugsee_capture_register_type(type_id, &TypeInfo)` | DefaultJson hints or `ExporterVTable` |
| Exporter vtable `init` / `process` / `complete` | Custom artifacts |

FFI adapters implement Rust traits by calling vtables.

---

## 9. Migration & testing

1. Traits + registry + protobuf streams; migrate built-in providers.
2. Coordinator replaces ad-hoc capture entry in runtime.
3. SnapshotMaterializer + `FsDedupStore`; optional `dedup-redb` CI.
4. C register/submit/export smoke.
5. Remove JSON length-prefixed parts when covered.

**Tests:** mock `Storage`; streaming export reuse; force tier-3 + xxh3 identity + GC; optional 9k-file soak; feature-gated redb; option cache relaunch; FFI provider path.

---

## 10. Relationship to other docs

- [`DESIGN_PLATFORM.md`](./DESIGN_PLATFORM.md) — engine/platform/C vtable for FS/HTTP/crash; capture sits *on* `Storage`.
- [`DESIGN.md`](./DESIGN.md) — wire bundle, signatures, product behavior; decision 13 (JSON-on-disk) is **amended** for capture parts → protobuf; export still produces wire JSON.
- Android `CaptureFileStorage` / `CaptureFileStorageRefsDb` — behavioral reference for tier-3 dedup (FS markers replace SQLite refs).

## 11. References

- Android: `library/.../contracts/capture/*`, `capture/BugseeCaptureCoordinator.java`, `CaptureAggregator*.java`, `CaptureExporter.java`, `CaptureFileStorage.java`
- Composer research on Rust embedded DBs: prefer FS-as-DB; redb as optional fallback
- xxHash / XXH3 — content identity for local blobs only
