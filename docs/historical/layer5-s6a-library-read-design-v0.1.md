# Layer 5 S6a — LibraryService read inspection (ListLibraries + GetLibrary) Design v0.1

Status: design decision. S1 (catalog) + S4a (write) + S5a (read) + S3a
(operations backbone) are live. S6a is the **read half** of the LibraryService:
`ListLibraries` and `GetLibrary` — projecting the daemon's discovered hardware
inventory over gRPC so an orchestrator/operator can see libraries, drives, slots,
and import/export ports without SSH+`rem-debug`. The state-changing robotics
(`RefreshInventory` / `MoveMedium` / `Load`/`Unload` / `Import`/`Export`) are
S6b; `StreamLibraryEvents` is S6c. Grounds in `proto/layer5.proto`
(`LibraryService` + `Library`/`LibraryState`/`Drive`/`Slot`/`PortalSlot`),
`remanence-library` (`DiscoveryReport`/`Library`/`DriveBay`/`Slot`/`IePort`,
already pure `Clone` data), `remanence-api` (`ApiState`, the read-only catalog
connection pattern), and `remanence-scsi::Inquiry` (`vendor_str`/`product_str`/
`revision_str`).

## Background — what exists

- **Discovery (exists):** `remanence_library::discover() -> DiscoveryReport` runs
  once at daemon startup on the write-capable path (`main.rs`). The report is a
  pure-data snapshot (`Vec<Library>` + warnings); each `Library` carries
  `changer_inquiry`, `drive_bays`, `slots`, `ie_ports`. It is `Clone + Send +
  Sync` (no FFI pointers, no lifetimes).
- **The report is consumed, not retained:** today `with_session_owner` *moves*
  the report into the drive-session owner thread (`WriteOwnerConfig.report`).
  `ApiState` keeps no copy, so no handler can read inventory.
- **Catalog reads are owner-independent:** every `CatalogService` handler reads
  through `ApiState::index()` → `CatalogIndex::open_read_only(index_path)`, a
  fresh read-only SQLite connection per call. This path never touches the
  session owner and never blocks on / fails when a write/read/reconcile session
  holds the drive.
- **Gaps:** `ApiState` holds no inventory snapshot; `LibraryService` is entirely
  unimplemented and unregistered on the daemon; the proto `library_uuid`
  (durable library identity) has no source in the codebase.

## Architecture — project a startup snapshot; reads never touch the owner

S6a adds a **library inventory snapshot** to `ApiState` and a `LibraryService`
that projects it. Two deliberate properties:

1. **Static-at-startup.** The snapshot is the `DiscoveryReport` captured once at
   daemon startup, plus the capture timestamp. `LibraryState.last_inventory_at`
   surfaces that timestamp so a stale view is *visible as* stale. Refreshing the
   snapshot (re-running discovery / a live `READ ELEMENT STATUS` on the owner) is
   `RefreshInventory` — **S6b**. S6a never mutates the snapshot.
2. **Always-available.** `ListLibraries`/`GetLibrary` read the `ApiState`
   snapshot + a read-only catalog connection only. They never dispatch a
   `SessionCommand` to the owner, so inventory inspection **succeeds even while a
   write/read/reconcile session holds the drive** — it never returns
   `FAILED_PRECONDITION` for "owner busy." This is the right behavior for a
   read surface an operator reaches for *because* something is running.

| | Source | Serves |
|---|---|---|
| Library structure (libraries, drives, slots, ports) | `Arc<LibrarySnapshot>` in `ApiState` (startup `DiscoveryReport` + `captured_at`) | `ListLibraries`, `GetLibrary` |
| Cartridge → tape identity (voltag → `tape_uuid`) | `ApiState::index()` (read-only catalog), `list_tapes(None)` | the join inside `GetLibrary` |

`LibrarySnapshot { report: DiscoveryReport, captured_at: OffsetDateTime }` is
populated in `with_session_owner` (the report is cloned: one clone stays in the
`Arc<LibrarySnapshot>` for reads, the original still moves into the owner). On
the read-only daemon path (`new_with_config`, which never discovers) the snapshot
is `None` → `ListLibraries` returns an empty list and `GetLibrary` returns
`not_found`. The snapshot is `Arc`-wrapped so `ApiState` stays cheap to `Clone`
and remains `Send + Sync`.

## library_uuid — derived bootstrap, durable identity deferred

The proto comments envision `library_uuid` as "assigned at first discovery …
independent of SCSI serial which can change with firmware re-flash." S6a has no
identity-persistence layer, so it uses a **deterministic derivation**:

```
library_uuid = UUIDv5(REMANENCE_LIBRARY_NS, library.serial.as_bytes())
```

where `REMANENCE_LIBRARY_NS` is a fixed, never-to-be-changed namespace UUID
constant defined in `remanence-api`. This needs no new storage and round-trips
`ListLibraries` ↔ `GetLibrary` within a daemon run **and across restarts**, as
long as the SCSI serial is stable (it is, except on the firmware re-flash case).

**Honest limitation:** because it is *derived from* the serial, this UUID is
**not** stable across a re-flash that changes the serial — the exact durability
the proto comment wants. The durable, assigned-at-first-discovery identity (a
`libraries` table mapping `serial → assigned_uuid`, re-associable by an operator
after a serial change) is a deliberate **follow-up slice**, not S6a. With a
single library in the current deployment and inventory used as a live read
surface (not yet a long-term durable reference), the derived bootstrap is the
right scope-minimal call; the spec records the limitation so it isn't a silent
assumption.

## The two RPCs

- **`ListLibraries(Empty) -> ListLibrariesResponse{ repeated Library }`** —
  project each `snapshot.report.libraries[i]` to a `pb::Library` (structure only,
  no per-element detail). `None` snapshot → empty list.
- **`GetLibrary(GetLibraryRequest{ library_uuid }) -> LibraryState`** — decode
  the 16-byte `library_uuid`, find the library whose `UUIDv5(serial)` equals it,
  and project the full `LibraryState` (library + drives + slots +
  import_export_ports + `last_inventory_at`). Unknown uuid / `None` snapshot →
  `not_found`; non-16-byte uuid → `invalid_argument`.

The other seven methods are implemented as `Status::unimplemented` with the
slice marker (`RefreshInventory`/`MoveMedium`/`LoadDrive`/`UnloadDrive`/
`ImportElement`/`ExportElement` → "S6b"; `StreamLibraryEvents` → "S6c"), so the
service registers in full on the daemon now and later slices only swap bodies.

## Projection — `DiscoveryReport` → proto

**`Library` (proto) ← `remanence_library::Library`:**

| proto field | source |
|---|---|
| `library_serial` | `library.serial` |
| `vendor` | `library.changer_inquiry.vendor_str()` (trimmed) |
| `product` | `library.changer_inquiry.product_str()` |
| `product_revision` | `library.changer_inquiry.revision_str()` |
| `library_uuid` | `UUIDv5(REMANENCE_LIBRARY_NS, serial)` (16 bytes) |

**`Drive` (proto) ← `DriveBay`:**

| proto field | source |
|---|---|
| `element_address` | `bay.element_address as u32` |
| `drive_serial` | `bay.installed.as_ref().map(serial)`, else `""` |
| `host_device_path` | `bay.installed.and_then(sg_path)` display, else `""` |
| `vendor` / `product` | `bay.installed.and_then(vendor/product)`, else `""` |
| `loaded_tape_uuid` | catalog join on `bay.loaded_tape` (voltag); empty if idle/unknown |
| `status` | see drive-status mapping below |

**Drive status** (from the static snapshot):

- `installed.is_none()` → `DRIVE_STATUS_UNREACHABLE` (bay present, no drive
  identity resolved).
- `installed.is_some()` but `sg_path.is_none()` → `DRIVE_STATUS_UNREACHABLE`
  (identity known via DVCID, but no host `/dev/sgN` → no I/O possible).
- `installed` + `sg_path` + `bay.loaded` → `DRIVE_STATUS_LOADED`.
- `installed` + `sg_path` + `!bay.loaded` → `DRIVE_STATUS_IDLE`.
- `DRIVE_STATUS_BUSY` is **deferred to S6b**: the static snapshot can't attribute
  the daemon's global `session_busy` flag to a *specific* bay without guessing
  which drive the owner mounted. Per-drive live state tracking (and the
  `session_busy` hook) lands with `RefreshInventory`/live drive state in S6b.

**`Slot` (proto) ← `remanence_library::Slot`:**

| proto field | source |
|---|---|
| `element_address` | `slot.element_address as u32` |
| `voltag` | `slot.cartridge.unwrap_or_default()` |
| `tape_uuid` | catalog join on `slot.cartridge`; empty if absent/uncatalogued |

**`PortalSlot` (proto) ← `IePort`:**

| proto field | source |
|---|---|
| `element_address` | `ie.element_address as u32` |
| `voltag` | `ie.cartridge.unwrap_or_default()` |
| `tape_uuid` | catalog join on `ie.cartridge` |
| `last_direction` | `PORTAL_DIRECTION_UNSPECIFIED` |

`last_direction` is `UNSPECIFIED` in S6a: the RES `import_enabled`/`export_enabled`
flags describe the port's *current capability*, not the *last move direction*,
and we keep no move history in a static snapshot. Direction history is an
operations/audit concern (S6b/S7).

`last_inventory_at` ← `snapshot.captured_at` (proto `Timestamp`).

**Catalog join.** Once per `GetLibrary`, `index.list_tapes(None)` yields
`Vec<TapeRecord{ tape_uuid, voltag, … }>`; build a `HashMap<String, Vec<u8>>`
(voltag → tape_uuid) and look up each cartridge voltag. A missing voltag → empty
`tape_uuid` bytes, which the proto treats as "not known to the catalog" — valid
(un-barcoded or not-yet-ingested cartridges). Voltag uniqueness among catalogued
tapes is assumed; the catalog's own uniqueness constraint governs any collision
(map insertion is last-writer-wins, which only matters for an already-invalid
duplicate-voltag catalog).

## Module placement

- New `crates/remanence-api/src/library.rs` (child of the crate root, sibling to
  `mount.rs`/`operations.rs`): `LibraryServiceApi { state: ApiState }`, its
  `impl pb::library_service_server::LibraryService`, and the pure projection
  helpers (`project_library`, `project_library_state`, `drive_status`,
  `voltag_uuid_map`). Projection helpers are pure functions over borrowed data →
  unit-testable without a daemon.
- `lib.rs`: add `LibrarySnapshot` + the `library_snapshot: Option<Arc<LibrarySnapshot>>`
  field on `ApiState`, populate it in `with_session_owner`, add the
  `library_service()` accessor, add `mod library;`.
- `crates/remanence-daemon/src/lib.rs`: register
  `pb::library_service_server::LibraryServiceServer::new(state.library_service())`
  in `serve()`.
- `crates/remanence-api/Cargo.toml`: `uuid = { workspace = true, features = ["v5"] }`.

## Error taxonomy

`invalid_argument` (library_uuid not 16 bytes), `not_found` (no library matches
the uuid; or snapshot absent on a read-only daemon), `internal` (catalog
read/SQLite failure during the join). `ListLibraries` never errors on an absent
snapshot — it returns an empty list (a read-only catalog daemon legitimately has
no hardware view).

## Pinned contract for the consumer (orchestrator / harness)

- `LibraryService.ListLibraries{}` → `ListLibrariesResponse{ libraries: [Library{
  library_serial, vendor, product, product_revision, library_uuid }] }`.
- `LibraryService.GetLibrary{ library_uuid }` → `LibraryState{ library, drives[],
  slots[], import_export_ports[], last_inventory_at }`. `library_uuid` is the
  16-byte `UUIDv5(serial)` returned by `ListLibraries`.
- Inventory reads are **always available** — they succeed during an active
  write/read/reconcile session (no `FAILED_PRECONDITION`).
- The view is the **startup snapshot**; `last_inventory_at` dates it.
  `RefreshInventory` (S6b) is required to pick up post-startup tape moves.
- `tape_uuid` on a slot/drive/port is empty when the cartridge's voltag is not in
  the catalog (un-barcoded / not yet ingested).

## Scope

**IN (S6a):** the `LibrarySnapshot` field on `ApiState` (startup report +
capture time, `Arc`-shared); `ListLibraries` + `GetLibrary` projecting it;
`UUIDv5(serial)` library identity; the voltag → `tape_uuid` catalog join;
drive-status mapping (LOADED/IDLE/UNREACHABLE); registering `LibraryService` on
the daemon with the remaining seven methods stubbed `unimplemented`.

**OUT:** `RefreshInventory` and all state-changing robotics (`MoveMedium`,
`Load`/`Unload`, `Import`/`Export`) → **S6b**; `StreamLibraryEvents` hot-plug →
**S6c**; durable assigned-at-first-discovery library identity (`libraries`
table, re-association across serial change) → follow-up; `DRIVE_STATUS_BUSY`
attribution + live drive state → S6b; `PortalSlot.last_direction` history →
S6b/S7; multi-library snapshot refresh / read-only-daemon discovery-for-inventory
→ later.

## Acceptance criteria

1. **Unit (pure projection):** hand-built `DiscoveryReport` (as in
   `model.rs` tests) → `project_library` yields the expected `pb::Library`
   including the `UUIDv5(serial)` bytes; `drive_status` returns
   UNREACHABLE/IDLE/LOADED for each input case; `project_library_state` maps
   slots/ports and joins voltag → `tape_uuid` (hit and miss) given a supplied
   voltag→uuid map.
2. **Integration (hardware-free):** an `ApiState` carrying a `LibrarySnapshot`
   (no session owner needed) → `list_libraries` returns the projected library;
   `get_library` by its `UUIDv5` returns a `LibraryState` with the expected
   drives/slots/ports; an unknown `library_uuid` → `not_found`; a non-16-byte
   `library_uuid` → `invalid_argument`; both calls succeed with `session_tx` set
   to a busy owner (always-available property).
3. **Harness e2e (akash fixture):** `ListLibraries` shows the QuadStor library;
   `GetLibrary` shows its drives + slots, with `tape_uuid` populated for
   catalogued voltags and empty otherwise; a `GetLibrary` issued while a write
   session is open still returns `OK`.
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

Verified against `cargo check -p remanence-api` + `cargo clippy -p remanence-api
--all-targets -- -D warnings` (both clean) on 2026-06-03, with a skeleton
(`crates/remanence-api/src/library_skeleton.rs`: `LibraryServiceApi` +
`impl pb::library_service_server::LibraryService` for all nine methods incl. the
`type StreamLibraryEventsStream = Pin<Box<dyn Stream<…> + Send>>` associated type,
plus `Send+Sync` asserts on `Arc<DiscoveryReport>` and a `uuid::Uuid::new_v5`
call requiring the `v5` feature). Removed after (design-only); the plan recreates
it. Checked against current HEAD (post S1/S4a/S5a/S3a).

Five-category result:

1. **Module privacy** — `LibraryServiceApi` + projection helpers live in
   `remanence-api` beside `ApiState`/`pb`; they read `DiscoveryReport`/`Library`/
   `DriveBay`/`Slot`/`IePort` through their `pub` fields (`remanence-library`
   exposes them publicly). `Inquiry::{vendor,product,revision}_str` are `pub`.
   Pass.
2. **`!Send` in threading** — `DiscoveryReport` is pure `Clone` data (no FFI
   pointers/lifetimes) → `Send + Sync`; `Arc<LibrarySnapshot>` keeps `ApiState`
   `Clone + Send + Sync` (verified by a `send_sync::<Arc<DiscoveryReport>>()`
   assert). No value crosses a new thread/`spawn` boundary; the handlers read the
   `Arc` in place. Pass.
3. **Reactor timing** — no new tokio-aware type is constructed; `ListLibraries`/
   `GetLibrary` are plain `async fn` returning ready data (no streaming in S6a;
   the stubbed `StreamLibraryEventsStream` is `unimplemented`). Pass.
4. **Borrowed-handle plumbing** — the snapshot is `Arc`-shared (owned clone, no
   borrow of `ApiState`); the catalog join uses an owned read-only
   `CatalogIndex`; projection helpers take `&` and return owned `pb` values. No
   multi-field borrow of any handle. Pass.
5. **Trait/method visibility traps** — `pb::{Library, LibraryState, Drive, Slot,
   PortalSlot, ListLibrariesResponse, GetLibraryRequest,
   library_service_server::{LibraryService, LibraryServiceServer}}`,
   `uuid::Uuid::new_v5` (with the added `v5` feature), `Inquiry::*_str`,
   `CatalogIndex::list_tapes` are all reachable from `remanence-api`; the trait
   impl compiling proves `LibraryServiceServer::new(state.library_service())`
   wires. Pass.

One dependency change: `remanence-api`'s `uuid` gains the `v5` feature (already a
workspace dependency; `v5` adds the SHA-1 name-based constructor). No new crates.
