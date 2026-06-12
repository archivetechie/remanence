# Layer 5 S6a — LibraryService read inspection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve `LibraryService.ListLibraries` + `GetLibrary` over the Layer 5 daemon by projecting the startup `DiscoveryReport` snapshot into the proto `LibraryState`, joining cartridge voltags to catalog `tape_uuid`s.

**Architecture:** A new `Arc<LibrarySnapshot>` (startup `DiscoveryReport` + capture time) is held in `ApiState`. A new `remanence-api::library` module holds pure projection helpers and the `LibraryServiceApi` gRPC impl. Reads use the snapshot + a read-only catalog connection (`ApiState::index()`) — they never dispatch to the drive-session owner, so inventory reads succeed even while a write/read/reconcile session holds the drive. The remaining seven `LibraryService` methods are stubbed `unimplemented` (S6b/S6c) so the service registers in full now.

**Tech Stack:** Rust, tonic/prost (`remanence.api.v1`), `remanence-library` (`DiscoveryReport`), `remanence-state` (`CatalogIndex`), `uuid` (v5), `time`.

---

## Context the implementer needs

- **Design doc:** `docs/layer5-s6a-library-read-design-v0.1.md` (read it first — the projection table, drive-status mapping, and the `library_uuid` derivation rationale live there).
- **Generated proto idents** (from `proto/layer5.proto`, prost output `remanence.api.v1`):
  - `pb::Library { library_serial: String, vendor: String, product: String, product_revision: String, library_uuid: Vec<u8> }`
  - `pb::LibraryState { library: Option<pb::Library>, drives: Vec<pb::Drive>, slots: Vec<pb::Slot>, import_export_ports: Vec<pb::PortalSlot>, last_inventory_at: Option<prost_types::Timestamp> }`
  - `pb::Drive { element_address: u32, drive_serial: String, host_device_path: String, vendor: String, product: String, loaded_tape_uuid: Vec<u8>, status: i32 }`
  - `pb::Slot { element_address: u32, voltag: String, tape_uuid: Vec<u8> }`
  - `pb::PortalSlot { element_address: u32, voltag: String, tape_uuid: Vec<u8>, last_direction: i32 }`
  - `pb::ListLibrariesResponse { libraries: Vec<pb::Library> }`, `pb::GetLibraryRequest { library_uuid: Vec<u8> }`
  - Enums (stored as `i32` in the structs): `pb::drive::Status::{DriveStatusUnspecified, DriveStatusIdle, DriveStatusLoaded, DriveStatusBusy, DriveStatusUnreachable}`; `pb::portal_slot::Direction::PortalDirectionUnspecified`. Convert with `... as i32`.
  - Service trait: `pb::library_service_server::{LibraryService, LibraryServiceServer}`. `ListLibraries` takes `Request<()>` (proto `google.protobuf.Empty`).
- **Source types** (`remanence-library`, all `pub`, re-exported at crate root): `DiscoveryReport { libraries: Vec<Library>, warnings: Vec<_> }`, `Library { serial: String, changer_inquiry, drive_bays: Vec<DriveBay>, slots: Vec<Slot>, ie_ports: Vec<IePort>, .. }`, `DriveBay { element_address: u16, installed: Option<InstalledDrive>, loaded: bool, loaded_tape: Option<String>, .. }`, `InstalledDrive { serial: String, vendor: Option<String>, product: Option<String>, sg_path: Option<PathBuf>, .. }`, `Slot { element_address: u16, full: bool, cartridge: Option<String> }`, `IePort { element_address: u16, full: bool, cartridge: Option<String>, .. }`. The SCSI `Inquiry` is reachable as `remanence_library::scsi::Inquiry` with `.vendor_str() / .product_str() / .revision_str()` (trimmed `&str`).
- **Existing crate helpers** (in `remanence-api/src/lib.rs`, crate-root private — reachable from the child `library` module as `crate::…`): `decode_uuid_bytes(value: &[u8], field: &str) -> Result<[u8; 16], Status>`, `status_from_state_error(StateError) -> Status`, `ApiState::index() -> Result<CatalogIndex, Status>` (opens a read-only connection), `ApiState::new(CatalogIndex) -> ApiState`.
- **Catalog join source:** `CatalogIndex::list_tapes(None) -> Result<Vec<TapeRecord>, StateError>` where `TapeRecord { tape_uuid: Vec<u8>, voltag: Option<String>, .. }`.
- **`with_session_owner`** is in `lib.rs` (≈ lines 171-217); it currently *moves* `report` into `spawn_session_owner(index, WriteOwnerConfig { report, .. })`. The snapshot must be cloned from `report` *before* that move.
- **No `tempfile` dev-dep.** Tests build a throwaway index with the existing pattern: `CatalogIndex::open(std::env::temp_dir().join(format!("remanence-api-lib-{}", Uuid::new_v4())).join("state.sqlite"))`.

## File Structure

- **Create** `crates/remanence-api/src/library.rs` — `LibraryServiceApi`, its `LibraryService` impl (2 real + 7 stubbed methods), the pure projection helpers (`library_uuid`, `project_library`, `drive_status`, `joined_tape_uuid`, `project_drive`, `project_slot`, `project_portal`, `project_library_state`, `voltag_uuid_map`), and the `#[cfg(test)]` module.
- **Modify** `crates/remanence-api/Cargo.toml` — add the `uuid` `v5` feature.
- **Modify** `crates/remanence-api/src/lib.rs` — `LibrarySnapshot` struct; `ApiState.library_snapshot` field (default `None`, populated in `with_session_owner`); `library_service()` accessor; `mod library;` + `pub use library::LibraryServiceApi;`.
- **Modify** `crates/remanence-daemon/src/lib.rs` — register `LibraryServiceServer` in `serve()`.

---

## Task 1: Enable the uuid v5 feature

**Files:**
- Modify: `crates/remanence-api/Cargo.toml`

- [ ] **Step 1: Add the `v5` feature to the `uuid` dependency**

In `crates/remanence-api/Cargo.toml`, replace the line:
```toml
uuid.workspace = true
```
with:
```toml
uuid = { workspace = true, features = ["v5"] }
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build -p remanence-api`
Expected: builds clean (the new feature pulls in `Uuid::new_v5`).

- [ ] **Step 3: Commit**

```bash
git add crates/remanence-api/Cargo.toml
git commit -m "S6a: enable uuid v5 feature for derived library UUIDs"
```

---

## Task 2: Pure projection module

**Files:**
- Create: `crates/remanence-api/src/library.rs`
- Modify: `crates/remanence-api/src/lib.rs` (add `mod library;`)
- Test: inline `#[cfg(test)]` in `crates/remanence-api/src/library.rs`

- [ ] **Step 1: Create `library.rs` with the projection helpers**

Create `crates/remanence-api/src/library.rs`:

```rust
//! S6a — LibraryService read inspection: ListLibraries + GetLibrary.
//!
//! Projects the daemon's startup `DiscoveryReport` snapshot (held in `ApiState`)
//! into the proto `LibraryState`, joining cartridge voltags to catalog
//! `tape_uuid`s via a read-only catalog connection. These reads never dispatch
//! to the drive-session owner, so they succeed even while a write/read/reconcile
//! session holds the drive. State-changing robotics is S6b; StreamLibraryEvents
//! is S6c. See docs/layer5-s6a-library-read-design-v0.1.md.

use std::collections::HashMap;
use std::pin::Pin;

use remanence_library::{DriveBay, IePort, Library, Slot};
use remanence_state::CatalogIndex;
use time::OffsetDateTime;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::{pb, ApiState};

/// Fixed namespace for deriving a deterministic library UUID (UUIDv5) from the
/// SCSI unit serial. NEVER change this constant — library identities depend on
/// it. The durable, assigned-at-first-discovery identity that survives a serial
/// change on firmware re-flash is a deferred follow-up (see the S6a design doc).
const REMANENCE_LIBRARY_NS: Uuid = Uuid::from_bytes([
    0x52, 0x65, 0x6d, 0x6e, 0x4c, 0x69, 0x62, 0x72, 0x61, 0x72, 0x79, 0x4e, 0x53, 0x76, 0x31, 0x00,
]);

/// Deterministic 16-byte library UUID derived from the SCSI unit serial.
pub(crate) fn library_uuid(serial: &str) -> [u8; 16] {
    *Uuid::new_v5(&REMANENCE_LIBRARY_NS, serial.as_bytes()).as_bytes()
}

/// Project a discovered library's identity (structure only, no per-element detail).
pub(crate) fn project_library(library: &Library) -> pb::Library {
    pb::Library {
        library_serial: library.serial.clone(),
        vendor: library.changer_inquiry.vendor_str().to_string(),
        product: library.changer_inquiry.product_str().to_string(),
        product_revision: library.changer_inquiry.revision_str().to_string(),
        library_uuid: library_uuid(&library.serial).to_vec(),
    }
}

/// Drive status from the static startup snapshot. `BUSY` (live owner
/// attribution) is deferred to S6b.
pub(crate) fn drive_status(bay: &DriveBay) -> pb::drive::Status {
    match bay.installed.as_ref() {
        None => pb::drive::Status::DriveStatusUnreachable,
        Some(installed) if installed.sg_path.is_none() => {
            pb::drive::Status::DriveStatusUnreachable
        }
        Some(_) if bay.loaded => pb::drive::Status::DriveStatusLoaded,
        Some(_) => pb::drive::Status::DriveStatusIdle,
    }
}

/// Look up a cartridge voltag in the catalog join map; empty bytes when the
/// voltag is absent/uncatalogued (a valid "unknown to catalog" signal).
fn joined_tape_uuid(voltag: &Option<String>, voltags: &HashMap<String, Vec<u8>>) -> Vec<u8> {
    voltag
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| voltags.get(v).cloned())
        .unwrap_or_default()
}

pub(crate) fn project_drive(bay: &DriveBay, voltags: &HashMap<String, Vec<u8>>) -> pb::Drive {
    let installed = bay.installed.as_ref();
    pb::Drive {
        element_address: u32::from(bay.element_address),
        drive_serial: installed.map(|d| d.serial.clone()).unwrap_or_default(),
        host_device_path: installed
            .and_then(|d| d.sg_path.as_ref())
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        vendor: installed.and_then(|d| d.vendor.clone()).unwrap_or_default(),
        product: installed.and_then(|d| d.product.clone()).unwrap_or_default(),
        loaded_tape_uuid: joined_tape_uuid(&bay.loaded_tape, voltags),
        status: drive_status(bay) as i32,
    }
}

pub(crate) fn project_slot(slot: &Slot, voltags: &HashMap<String, Vec<u8>>) -> pb::Slot {
    pb::Slot {
        element_address: u32::from(slot.element_address),
        voltag: slot.cartridge.clone().unwrap_or_default(),
        tape_uuid: joined_tape_uuid(&slot.cartridge, voltags),
    }
}

pub(crate) fn project_portal(port: &IePort, voltags: &HashMap<String, Vec<u8>>) -> pb::PortalSlot {
    pb::PortalSlot {
        element_address: u32::from(port.element_address),
        voltag: port.cartridge.clone().unwrap_or_default(),
        tape_uuid: joined_tape_uuid(&port.cartridge, voltags),
        last_direction: pb::portal_slot::Direction::PortalDirectionUnspecified as i32,
    }
}

pub(crate) fn project_library_state(
    library: &Library,
    captured_at: &OffsetDateTime,
    voltags: &HashMap<String, Vec<u8>>,
) -> pb::LibraryState {
    pb::LibraryState {
        library: Some(project_library(library)),
        drives: library
            .drive_bays
            .iter()
            .map(|bay| project_drive(bay, voltags))
            .collect(),
        slots: library
            .slots
            .iter()
            .map(|slot| project_slot(slot, voltags))
            .collect(),
        import_export_ports: library
            .ie_ports
            .iter()
            .map(|port| project_portal(port, voltags))
            .collect(),
        last_inventory_at: Some(prost_types::Timestamp {
            seconds: captured_at.unix_timestamp(),
            nanos: captured_at.nanosecond() as i32,
        }),
    }
}

/// Build the voltag -> tape_uuid join map from the read-only catalog (one query).
pub(crate) fn voltag_uuid_map(index: &CatalogIndex) -> Result<HashMap<String, Vec<u8>>, Status> {
    let mut map = HashMap::new();
    for tape in index.list_tapes(None).map_err(crate::status_from_state_error)? {
        if let Some(voltag) = tape.voltag {
            let voltag = voltag.trim().to_string();
            if !voltag.is_empty() {
                map.insert(voltag, tape.tape_uuid);
            }
        }
    }
    Ok(map)
}
```

Note: `Pin`, `Stream`, `Request`, `Response`, `ApiState` are unused until Task 3; add them now to avoid re-editing the `use` block. If `cargo build` warns about them between Task 2 and Task 3, that is expected and resolved in Task 3 (do not add `#[allow]`).

- [ ] **Step 2: Declare the module**

In `crates/remanence-api/src/lib.rs`, add `mod library;` to the module block (after `mod write_owner;`, keeping alphabetical-ish order is fine — place it after `mod tape_init;`):

```rust
mod library;
mod mount;
mod operations;
```
(Insert `mod library;` immediately before `mod mount;`.)

- [ ] **Step 3: Add the pure-projection tests**

Append to `crates/remanence-api/src/library.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use remanence_library::scsi::{DeviceType, Inquiry};
    use remanence_library::{
        DriveBay, ElementLayout, IePort, IdentitySource, InstalledDrive, Library, Slot,
    };

    use super::*;

    fn mk_inquiry() -> Inquiry {
        Inquiry {
            device_type: DeviceType::MediumChanger,
            peripheral_qualifier: 0,
            removable: true,
            version: 7,
            response_data_format: 2,
            additional_length: 31,
            vendor: *b"HPE     ",
            product: *b"MSL3040         ",
            revision: *b"6.40",
        }
    }

    fn mk_library() -> Library {
        Library {
            serial: "DEC418146K_LL02".to_string(),
            changer_sg: PathBuf::from("/dev/sg7"),
            changer_sysfs: PathBuf::from("/sys/test"),
            changer_inquiry: mk_inquiry(),
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: 1,
                drive_count: 1,
                slot_start: 0x3e8,
                slot_count: 1,
                ie_start: 0x10,
                ie_count: 1,
            },
            drive_bays: vec![DriveBay {
                element_address: 1,
                installed: Some(InstalledDrive {
                    serial: "8031BDC7D1".to_string(),
                    identity_source: IdentitySource::DvcidInline,
                    vendor: Some("HPE".to_string()),
                    product: Some("Ultrium 9-SCSI".to_string()),
                    revision: Some("R1.0".to_string()),
                    sg_path: Some(PathBuf::from("/dev/sg8")),
                    sysfs_path: None,
                }),
                loaded: true,
                loaded_tape: Some("S30002L9".to_string()),
                source_slot: Some(0x040a),
            }],
            slots: vec![Slot {
                element_address: 0x3e9,
                full: true,
                cartridge: Some("CLNU01L9".to_string()),
            }],
            ie_ports: vec![IePort {
                element_address: 0x10,
                full: false,
                cartridge: None,
                import_enabled: true,
                export_enabled: true,
            }],
        }
    }

    #[test]
    fn library_uuid_is_deterministic_and_serial_specific() {
        assert_eq!(library_uuid("DEC418146K_LL02"), library_uuid("DEC418146K_LL02"));
        assert_ne!(library_uuid("AAA"), library_uuid("BBB"));
    }

    #[test]
    fn project_library_maps_inquiry_and_uuid() {
        let p = project_library(&mk_library());
        assert_eq!(p.library_serial, "DEC418146K_LL02");
        assert_eq!(p.vendor, "HPE");
        assert_eq!(p.product, "MSL3040");
        assert_eq!(p.product_revision, "6.40");
        assert_eq!(p.library_uuid, library_uuid("DEC418146K_LL02").to_vec());
    }

    #[test]
    fn drive_status_covers_each_case() {
        let drive = |sg: Option<PathBuf>| InstalledDrive {
            serial: "S".into(),
            identity_source: IdentitySource::DvcidInline,
            vendor: None,
            product: None,
            revision: None,
            sg_path: sg,
            sysfs_path: None,
        };
        let bay = |installed: Option<InstalledDrive>, loaded: bool| DriveBay {
            element_address: 1,
            installed,
            loaded,
            loaded_tape: None,
            source_slot: None,
        };
        assert_eq!(
            drive_status(&bay(None, false)),
            pb::drive::Status::DriveStatusUnreachable
        );
        assert_eq!(
            drive_status(&bay(Some(drive(None)), true)),
            pb::drive::Status::DriveStatusUnreachable
        );
        assert_eq!(
            drive_status(&bay(Some(drive(Some(PathBuf::from("/dev/sg8")))), true)),
            pb::drive::Status::DriveStatusLoaded
        );
        assert_eq!(
            drive_status(&bay(Some(drive(Some(PathBuf::from("/dev/sg8")))), false)),
            pb::drive::Status::DriveStatusIdle
        );
    }

    #[test]
    fn joined_tape_uuid_hits_and_misses() {
        let mut voltags = HashMap::new();
        voltags.insert("S30002L9".to_string(), vec![9u8; 16]);
        assert_eq!(joined_tape_uuid(&Some("S30002L9".to_string()), &voltags), vec![9u8; 16]);
        assert!(joined_tape_uuid(&Some("NOPE".to_string()), &voltags).is_empty());
        assert!(joined_tape_uuid(&None, &voltags).is_empty());
    }

    #[test]
    fn project_library_state_projects_and_joins() {
        let mut voltags = HashMap::new();
        voltags.insert("S30002L9".to_string(), vec![9u8; 16]);
        voltags.insert("CLNU01L9".to_string(), vec![7u8; 16]);
        let st = project_library_state(&mk_library(), &OffsetDateTime::UNIX_EPOCH, &voltags);
        assert_eq!(st.library.unwrap().library_serial, "DEC418146K_LL02");
        assert_eq!(st.drives.len(), 1);
        assert_eq!(st.drives[0].loaded_tape_uuid, vec![9u8; 16]);
        assert_eq!(st.drives[0].status, pb::drive::Status::DriveStatusLoaded as i32);
        assert_eq!(st.slots.len(), 1);
        assert_eq!(st.slots[0].voltag, "CLNU01L9");
        assert_eq!(st.slots[0].tape_uuid, vec![7u8; 16]);
        assert_eq!(st.import_export_ports.len(), 1);
        assert!(st.import_export_ports[0].tape_uuid.is_empty());
        assert_eq!(
            st.import_export_ports[0].last_direction,
            pb::portal_slot::Direction::PortalDirectionUnspecified as i32
        );
        assert_eq!(st.last_inventory_at.unwrap().seconds, 0);
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p remanence-api library::tests`
Expected: PASS (5 tests).

- [ ] **Step 5: Lint**

Run: `cargo clippy -p remanence-api --all-targets -- -D warnings`
Expected: clean. (The `unused import` warnings for `Pin`/`Stream`/`Request`/`Response`/`ApiState` will surface here — that is expected; they are consumed in Task 3. If clippy fails the build on them, proceed to Task 3 and run the lint gate there. Do NOT silence with `#[allow]`.)

- [ ] **Step 6: Commit**

```bash
git add crates/remanence-api/src/library.rs crates/remanence-api/src/lib.rs
git commit -m "S6a: pure DiscoveryReport -> LibraryState projection helpers"
```

---

## Task 3: Snapshot in ApiState + LibraryServiceApi + accessor

**Files:**
- Modify: `crates/remanence-api/src/lib.rs`
- Modify: `crates/remanence-api/src/library.rs`
- Test: inline `#[cfg(test)]` in `crates/remanence-api/src/library.rs`

- [ ] **Step 1: Add the `LibrarySnapshot` type and the `ApiState` field**

In `crates/remanence-api/src/lib.rs`, add the struct just above `pub struct ApiState` (≈ line 86):

```rust
/// Inventory snapshot captured once at daemon startup (S6a). Static until
/// `RefreshInventory` (S6b); `LibraryState.last_inventory_at` surfaces the
/// capture time so a stale view reads as stale. Held behind an `Arc` so
/// `ApiState` stays cheap to `Clone` and remains `Send + Sync`.
pub(crate) struct LibrarySnapshot {
    pub(crate) report: remanence_library::DiscoveryReport,
    pub(crate) captured_at: OffsetDateTime,
}
```

Add the field to `pub struct ApiState` (after `default_library_serial`):

```rust
    default_library_serial: Option<Arc<String>>,
    library_snapshot: Option<Arc<LibrarySnapshot>>,
    daemon_version: String,
```

- [ ] **Step 2: Default the field to `None` in the inner constructor**

In `new_with_pool_configs_inner` (the `Self { .. }` literal, ≈ line 154), add after `default_library_serial: None,`:

```rust
            default_library_serial: None,
            library_snapshot: None,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
```

- [ ] **Step 3: Populate the snapshot in `with_session_owner`**

In `with_session_owner`, **before** the `spawn_session_owner(index, WriteOwnerConfig { report, .. })` call (which moves `report`), capture a clone. Add immediately before `let session_tx = crate::write_owner::spawn_session_owner(`:

```rust
        let library_snapshot = Arc::new(LibrarySnapshot {
            report: report.clone(),
            captured_at: OffsetDateTime::now_utc(),
        });
```

Then, after the existing `state.default_library_serial = default_library_serial;` line and before `state` is returned, add:

```rust
        state.default_library_serial = default_library_serial;
        state.library_snapshot = Some(library_snapshot);
        state
```

- [ ] **Step 4: Add the `library_service()` accessor and re-export**

In `lib.rs`, next to the other service accessors (e.g. after `read_session_service`), add:

```rust
    /// Return the library-inspection service implementation (S6a).
    pub fn library_service(&self) -> LibraryServiceApi {
        LibraryServiceApi {
            state: self.clone(),
        }
    }
```

Add the re-export near the other `pub use` lines (after the `mod` block / `pub use mount::{…};`):

```rust
pub use library::LibraryServiceApi;
```

- [ ] **Step 5: Implement `LibraryServiceApi` in `library.rs`**

Append to `crates/remanence-api/src/library.rs` (before the `#[cfg(test)]` module):

```rust
/// LibraryService read inspection (S6a). The seven state-changing / streaming
/// methods are stubbed `unimplemented` (S6b/S6c) so the service registers in
/// full now.
#[derive(Clone)]
pub struct LibraryServiceApi {
    pub(crate) state: ApiState,
}

#[tonic::async_trait]
impl pb::library_service_server::LibraryService for LibraryServiceApi {
    async fn list_libraries(
        &self,
        _request: Request<()>,
    ) -> Result<Response<pb::ListLibrariesResponse>, Status> {
        let libraries = match self.state.library_snapshot.as_ref() {
            Some(snapshot) => snapshot.report.libraries.iter().map(project_library).collect(),
            None => Vec::new(),
        };
        Ok(Response::new(pb::ListLibrariesResponse { libraries }))
    }

    async fn get_library(
        &self,
        request: Request<pb::GetLibraryRequest>,
    ) -> Result<Response<pb::LibraryState>, Status> {
        let requested =
            crate::decode_uuid_bytes(&request.into_inner().library_uuid, "library_uuid")?;
        let snapshot = self
            .state
            .library_snapshot
            .as_ref()
            .ok_or_else(|| Status::not_found("library not found"))?;
        let library = snapshot
            .report
            .libraries
            .iter()
            .find(|lib| library_uuid(&lib.serial) == requested)
            .ok_or_else(|| Status::not_found("library not found"))?;
        let index = self.state.index()?;
        let voltags = voltag_uuid_map(&index)?;
        Ok(Response::new(project_library_state(
            library,
            &snapshot.captured_at,
            &voltags,
        )))
    }

    async fn refresh_inventory(
        &self,
        _request: Request<pb::RefreshInventoryRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        Err(Status::unimplemented("RefreshInventory is S6b"))
    }

    async fn move_medium(
        &self,
        _request: Request<pb::MoveMediumRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        Err(Status::unimplemented("MoveMedium is S6b"))
    }

    async fn load_drive(
        &self,
        _request: Request<pb::LoadDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        Err(Status::unimplemented("LoadDrive is S6b"))
    }

    async fn unload_drive(
        &self,
        _request: Request<pb::UnloadDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        Err(Status::unimplemented("UnloadDrive is S6b"))
    }

    async fn import_element(
        &self,
        _request: Request<pb::ImportElementRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        Err(Status::unimplemented("ImportElement is S6b"))
    }

    async fn export_element(
        &self,
        _request: Request<pb::ExportElementRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        Err(Status::unimplemented("ExportElement is S6b"))
    }

    type StreamLibraryEventsStream =
        Pin<Box<dyn Stream<Item = Result<pb::LibraryEvent, Status>> + Send + 'static>>;

    async fn stream_library_events(
        &self,
        _request: Request<pb::StreamLibraryEventsRequest>,
    ) -> Result<Response<Self::StreamLibraryEventsStream>, Status> {
        Err(Status::unimplemented("StreamLibraryEvents is S6c"))
    }
}
```

- [ ] **Step 6: Verify it compiles**

Run: `cargo check -p remanence-api`
Expected: clean (the `use` items from Task 2 are now all consumed).

- [ ] **Step 7: Add the integration tests**

Inside the existing `#[cfg(test)] mod tests` in `library.rs`, add these helpers + tests (the `mk_library`/`mk_inquiry` from Task 2 are reused):

```rust
    fn test_index() -> CatalogIndex {
        let dir = std::env::temp_dir().join(format!("remanence-api-lib-{}", Uuid::new_v4()));
        CatalogIndex::open(dir.join("state.sqlite")).expect("open test index")
    }

    fn state_with_snapshot() -> ApiState {
        let mut state = ApiState::new(test_index());
        state.library_snapshot = Some(std::sync::Arc::new(crate::LibrarySnapshot {
            report: remanence_library::DiscoveryReport {
                libraries: vec![mk_library()],
                warnings: Vec::new(),
            },
            captured_at: OffsetDateTime::UNIX_EPOCH,
        }));
        state
    }

    #[tokio::test]
    async fn list_libraries_projects_snapshot() {
        let api = state_with_snapshot().library_service();
        let resp = api.list_libraries(Request::new(())).await.expect("ok").into_inner();
        assert_eq!(resp.libraries.len(), 1);
        assert_eq!(resp.libraries[0].library_serial, "DEC418146K_LL02");
    }

    #[tokio::test]
    async fn list_libraries_empty_without_snapshot() {
        let api = ApiState::new(test_index()).library_service();
        let resp = api.list_libraries(Request::new(())).await.expect("ok").into_inner();
        assert!(resp.libraries.is_empty());
    }

    #[tokio::test]
    async fn get_library_returns_state_for_known_uuid() {
        let api = state_with_snapshot().library_service();
        let library_uuid = library_uuid("DEC418146K_LL02").to_vec();
        let resp = api
            .get_library(Request::new(pb::GetLibraryRequest { library_uuid }))
            .await
            .expect("ok")
            .into_inner();
        assert_eq!(resp.library.unwrap().library_serial, "DEC418146K_LL02");
        assert_eq!(resp.drives.len(), 1);
        assert_eq!(resp.slots.len(), 1);
        // Empty catalog → no voltag join.
        assert!(resp.drives[0].loaded_tape_uuid.is_empty());
        assert_eq!(resp.last_inventory_at.unwrap().seconds, 0);
    }

    #[tokio::test]
    async fn get_library_unknown_uuid_is_not_found() {
        let api = state_with_snapshot().library_service();
        let err = api
            .get_library(Request::new(pb::GetLibraryRequest { library_uuid: vec![0u8; 16] }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn get_library_bad_uuid_is_invalid_argument() {
        let api = state_with_snapshot().library_service();
        let err = api
            .get_library(Request::new(pb::GetLibraryRequest { library_uuid: vec![1u8, 2, 3] }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn get_library_missing_snapshot_is_not_found() {
        let api = ApiState::new(test_index()).library_service();
        let err = api
            .get_library(Request::new(pb::GetLibraryRequest { library_uuid: vec![0u8; 16] }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn get_library_succeeds_while_owner_busy() {
        let mut state = state_with_snapshot();
        state.session_busy =
            Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)));
        let library_uuid = library_uuid("DEC418146K_LL02").to_vec();
        let resp = state
            .library_service()
            .get_library(Request::new(pb::GetLibraryRequest { library_uuid }))
            .await;
        assert!(resp.is_ok(), "inventory reads must succeed while a session is busy");
    }
```

- [ ] **Step 8: Run the tests**

Run: `cargo test -p remanence-api library::tests`
Expected: PASS (12 tests — 5 pure + 7 integration).

- [ ] **Step 9: Commit**

```bash
git add crates/remanence-api/src/lib.rs crates/remanence-api/src/library.rs
git commit -m "S6a: LibraryService ListLibraries + GetLibrary over a startup snapshot"
```

---

## Task 4: Register LibraryService on the daemon

**Files:**
- Modify: `crates/remanence-daemon/src/lib.rs`

- [ ] **Step 1: Add the service registration**

In `crates/remanence-daemon/src/lib.rs`, inside `serve()`'s `Server::builder()` chain, add a `.add_service(...)` for the library service after the read-session service registration:

```rust
        .add_service(
            pb::read_session_service_server::ReadSessionServiceServer::new(
                state.read_session_service(),
            ),
        )
        .add_service(pb::library_service_server::LibraryServiceServer::new(
            state.library_service(),
        ))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), shutdown)
```

- [ ] **Step 2: Verify the daemon builds**

Run: `cargo build -p remanence-daemon`
Expected: builds clean.

- [ ] **Step 3: Commit**

```bash
git add crates/remanence-daemon/src/lib.rs
git commit -m "S6a: register LibraryService on the Layer 5 daemon"
```

---

## Task 5: Workspace gates

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt --all`
Then: `git diff --stat` — if `fmt` changed anything, review and include it in the final commit.

- [ ] **Step 2: Clippy (workspace)**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 3: Test (workspace)**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 4: Commit any formatting**

```bash
git add -A
git commit -m "S6a: fmt" || echo "nothing to format"
```

---

## Self-Review (completed during planning)

**Spec coverage:** ListLibraries (Task 3) ✓; GetLibrary + projection + voltag join (Tasks 2-3) ✓; UUIDv5 identity (Task 2) ✓; drive-status mapping LOADED/IDLE/UNREACHABLE, BUSY deferred (Task 2) ✓; `last_inventory_at` from capture time (Task 2-3) ✓; seven methods stubbed unimplemented (Task 3) ✓; daemon registration (Task 4) ✓; always-available-while-busy property (Task 3 test) ✓; uuid v5 feature (Task 1) ✓; gates (Task 5) ✓. AC3 (harness e2e on akash) is human-run and tracked outside this plan.

**Placeholder scan:** none — every step has concrete code/commands.

**Type consistency:** `library_uuid` returns `[u8; 16]` (compared to the `[u8; 16]` from `decode_uuid_bytes`); proto fields stored as `Vec<u8>` use `.to_vec()`; enum fields stored as `i32` use `as i32`; `element_address` widened `u16 -> u32` via `u32::from`. The `LibrarySnapshot` field is `pub(crate)` and set from in-crate code only.
