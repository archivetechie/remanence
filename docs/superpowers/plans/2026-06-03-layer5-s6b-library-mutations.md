# Layer 5 S6b — LibraryService mutations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire `RefreshInventory` + `MoveMedium`/`LoadDrive`/`UnloadDrive` onto the Layer 5 daemon as fire-and-track operations that dispatch real changer work to the drive-session owner, and make the library inventory snapshot mutable so post-op state reaches `GetLibrary`.

**Architecture:** `ApiState`'s S6a snapshot becomes a shared mutable cell (`Arc<RwLock<Arc<LibrarySnapshot>>>`). Four new gRPC handlers mirror `Catalog.ReconcileTape` (reserve the single owner, register an S3a operation, return `OperationRef` immediately). The owner runs one `SessionCommand::Robotics` handler with an `open → refresh() → act → publish` sequence, calling the existing `LibraryHandle::{refresh,move_medium,load,unload}`.

**Tech Stack:** Rust, tonic/prost, `remanence-library` (`LibraryHandle`), the S3a `OperationRegistry`, `std::sync::RwLock`.

---

## Context the implementer needs

- **Design doc:** `docs/layer5-s6b-library-mutations-design-v0.1.md` (read first — the open→refresh→act→publish model, single-session/cancel rules, and the deferred Import/Export + BUSY scope live there).
- **`LibraryHandle` is owned (no lifetime parameter).** Methods (all `pub`):
  - `refresh(&mut self) -> Result<(), remanence_scsi::ScsiError>`
  - `move_medium(&mut self, src: u16, dst: u16, policy: &dyn AccessPolicy) -> Result<(), MoveError>`
  - `load(&mut self, slot: u16, bay: u16, policy: &dyn AccessPolicy) -> Result<(), LoadError>`
  - `unload(&mut self, bay: u16, destination: Option<u16>, policy: &dyn AccessPolicy) -> Result<(), UnloadError>`
  - `library(&self) -> &Library`
  Each error type impls `std::error::Error`/`Display` with a descriptive message.
- **Owner template:** `crates/remanence-api/src/write_owner.rs` — `SessionCommand` enum, `session_loop` (dispatches commands via `blocking_recv`), `handle_reconcile` (the fire-and-track template), `SessionBusyGuard::from_reserved`, `publish_running`, `fail_operation`/`cancel_operation`/`record_reconcile_event`, and `cfg.report.library(serial)` + `lib.open(&cfg.policy)`.
- **gRPC template:** `crates/remanence-api/src/lib.rs` `CatalogService::reconcile_tape` (≈ lines 740-789) — reserve→register→record→`try_send`→`OperationRef`, with `TrySendError::{Full,Closed}` handling. Reuse `ApiState::reserve_session_owner()`, `ApiState::record_operation_failed()`, `self.state.operations.register(id, kind)`, `OperationHandle::{clone, publish_failed}`.
- **S6a code:** `crates/remanence-api/src/library.rs` — `LibraryServiceApi`, the `library_uuid(serial) -> [u8;16]` helper, the four `unimplemented` mutation stubs to replace. `crate::LibrarySnapshot { report, captured_at }` is in `lib.rs` (`pub(crate)`).
- **Audit primitive:** `crate::append_operation_audit(index, audit_dir, audit_fsync, lock, OperationAuditInput{ operation_id, operation_kind, event, subject_kind, subject_id, idempotency_key, detail })` (`pub(crate)`); `AuditEvent::{RequestReceived, OperationStarted, OperationFailed, CancelledBeforeDispatch}`.
- **Helpers reachable from the child `library` module:** `crate::decode_uuid_bytes`, `ApiState::{reserve_session_owner, record_operation_failed, operations, session_tx, default_library_serial, index_path}` (private items, visible to descendants).

## File Structure

- **Modify** `crates/remanence-api/src/lib.rs` — `library_snapshot` field type → `Option<Arc<RwLock<Arc<LibrarySnapshot>>>>`; `ApiState::current_library_snapshot()`; `ApiState::record_library_request_received()`; `with_session_owner` builds the cell + passes a clone to the owner; `use std::sync::RwLock`.
- **Modify** `crates/remanence-api/src/library.rs` — S6a read handlers use `current_library_snapshot()`; replace the four mutation stubs with real handlers + `dispatch_robotics`/`resolve_library_serial`/`narrow_element` helpers; update the S6a test helper.
- **Modify** `crates/remanence-api/src/write_owner.rs` — `RoboticsAction` enum; `SessionCommand::Robotics`; `WriteOwnerConfig.library_snapshot`; `handle_robotics` + `record_library_event`/`fail_library_operation`/`cancel_library_operation`/`publish_library_snapshot`; route in `session_loop`.

---

## Task 1: Mutable snapshot cell + S6a read-path switch

**Files:**
- Modify: `crates/remanence-api/src/lib.rs`
- Modify: `crates/remanence-api/src/library.rs`

- [ ] **Step 1: Import `RwLock` and change the field type**

In `lib.rs`, change the `Arc` import line to also bring `RwLock`:
```rust
use std::sync::{Arc, RwLock};
```
(The file currently has `use std::sync::Arc;` near the other `std::sync` imports — replace it. Keep the existing `AtomicBool`/`Ordering` import line as-is.)

Change the `ApiState` field:
```rust
    library_snapshot: Option<Arc<RwLock<Arc<LibrarySnapshot>>>>,
```

- [ ] **Step 2: Add the read helper**

In `impl ApiState`, next to `index()`, add:
```rust
    /// Current inventory snapshot (S6a read path; S6b republishes into the cell).
    /// Recovers a poisoned lock rather than panicking — the owner is the sole
    /// writer, so a poisoned lock only means a prior writer panicked.
    pub(crate) fn current_library_snapshot(&self) -> Option<Arc<LibrarySnapshot>> {
        self.library_snapshot
            .as_ref()
            .map(|cell| cell.read().unwrap_or_else(|e| e.into_inner()).clone())
    }
```

- [ ] **Step 3: Build the cell in `with_session_owner`**

In `with_session_owner`, replace the snapshot construction:
```rust
        let library_snapshot = Arc::new(LibrarySnapshot {
            report: report.clone(),
            captured_at: OffsetDateTime::now_utc(),
        });
```
with a cell, and pass a clone to the owner config:
```rust
        let library_snapshot = Arc::new(RwLock::new(Arc::new(LibrarySnapshot {
            report: report.clone(),
            captured_at: OffsetDateTime::now_utc(),
        })));
```
In the `WriteOwnerConfig { … }` literal passed to `spawn_session_owner`, add the field (Task 2 adds it to the struct):
```rust
                library_snapshot: library_snapshot.clone(),
```
The existing `state.library_snapshot = Some(library_snapshot);` line now stores the cell — leave it.

- [ ] **Step 4: Switch the S6a read handlers to the helper**

In `library.rs`, `list_libraries`:
```rust
        let libraries = match self.state.current_library_snapshot() {
            Some(snapshot) => snapshot
                .report
                .libraries
                .iter()
                .map(project_library)
                .collect(),
            None => Vec::new(),
        };
```
In `get_library`, replace the `self.state.library_snapshot.as_ref().ok_or_else(...)` line with:
```rust
        let snapshot = self
            .state
            .current_library_snapshot()
            .ok_or_else(|| Status::not_found("library not found"))?;
```
(The rest of `get_library` is unchanged — it already binds `snapshot` and reads `snapshot.report` / `snapshot.captured_at`.)

- [ ] **Step 5: Update the S6a test helper to build the cell**

In `library.rs` `mod tests`, change `state_with_snapshot`:
```rust
    fn state_with_snapshot() -> ApiState {
        let mut state = ApiState::new(test_index());
        state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
            crate::LibrarySnapshot {
                report: DiscoveryReport {
                    libraries: vec![mk_library()],
                    warnings: Vec::new(),
                },
                captured_at: OffsetDateTime::UNIX_EPOCH,
            },
        ))));
        state
    }
```

- [ ] **Step 6: Build + run the S6a tests through the lock**

Run: `cargo test -p remanence-api library::tests`
Expected: PASS (the existing 12 S6a tests, now reading through the `RwLock`). This proves the cell change didn't regress S6a.

- [ ] **Step 7: Commit**

```bash
git add crates/remanence-api/src/lib.rs crates/remanence-api/src/library.rs
git commit -m "S6b: make the library inventory snapshot a mutable RwLock cell"
```
(Compiles only if Task 2's `WriteOwnerConfig.library_snapshot` field lands together — implement Task 2 before building the daemon. If building piecewise, expect an `unknown field` error on `with_session_owner` until Task 2 Step 1; that is the only gap.)

---

## Task 2: Owner robotics command + handler

**Files:**
- Modify: `crates/remanence-api/src/write_owner.rs`

- [ ] **Step 1: Add the config field, the action enum, and the command variant**

Add `use std::sync::RwLock;` to the `std::sync` imports. Add the field to `WriteOwnerConfig`:
```rust
    pub session_busy: Arc<AtomicBool>,
    pub default_library_serial: Option<String>,
    pub library_snapshot: Arc<RwLock<Arc<crate::LibrarySnapshot>>>,
```

Add the action enum (above `SessionCommand`):
```rust
/// What a robotics operation should do once the owner has the library open.
pub(crate) enum RoboticsAction {
    Refresh,
    Move { src: u16, dst: u16 },
    Load { slot: u16, bay: u16 },
    Unload { bay: u16, destination: Option<u16> },
}
```

Add the command variant to `SessionCommand`:
```rust
    Robotics {
        library_serial: String,
        action: RoboticsAction,
        handle: crate::operations::OperationHandle,
    },
```

- [ ] **Step 2: Route it in `session_loop`**

In `session_loop`'s match, add an arm beside `SessionCommand::Reconcile`:
```rust
            SessionCommand::Robotics {
                library_serial,
                action,
                handle,
            } => {
                handle_robotics(&mut index, &cfg, library_serial, action, handle);
            }
```
The `session_loop` returns to the idle `while let` after each command (the owner is free again), exactly as `Reconcile` does. Add the same arm (returning a "busy" reply path is not needed — robotics has no synchronous reply) to the two in-session loops (`handle_open`'s and `handle_open_read`'s match), mirroring how they reject `Reconcile` mid-session:
```rust
            SessionCommand::Robotics { handle, .. } => {
                handle.publish_failed(
                    "drive-session owner is busy",
                    &[("phase", "dispatch")],
                );
            }
```

- [ ] **Step 3: Add the publish + audit helpers**

Add to `write_owner.rs`:
```rust
/// Replace this library's entry in the live snapshot and bump the capture time.
fn publish_library_snapshot(
    cell: &RwLock<Arc<crate::LibrarySnapshot>>,
    updated: remanence_library::Library,
) {
    let mut report = cell
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .report
        .clone();
    match report
        .libraries
        .iter_mut()
        .find(|lib| lib.serial == updated.serial)
    {
        Some(slot) => *slot = updated,
        None => report.libraries.push(updated),
    }
    let snapshot = Arc::new(crate::LibrarySnapshot {
        report,
        captured_at: OffsetDateTime::now_utc(),
    });
    *cell.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
}

fn record_library_event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    event: AuditEvent,
    mut detail: BTreeMap<String, CborValue>,
) -> Result<(), Status> {
    detail.insert(
        "library_serial".to_string(),
        CborValue::Text(library_serial.to_string()),
    );
    crate::append_operation_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::OperationAuditInput {
            operation_id: handle.op_id_uuid(),
            operation_kind: handle.operation_kind(),
            event,
            subject_kind: "library",
            subject_id: Some(library_serial.to_string()),
            idempotency_key: None,
            detail,
        },
    )
}

fn fail_library_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    error_summary: &str,
    progress: &[(&str, &str)],
) {
    let mut detail = BTreeMap::new();
    detail.insert(
        "error_summary".to_string(),
        CborValue::Text(error_summary.to_string()),
    );
    if let Err(err) = record_library_event(
        index,
        cfg,
        handle,
        library_serial,
        AuditEvent::OperationFailed,
        detail,
    ) {
        handle.publish_failed(
            format!("{error_summary}; audit record failed: {err}").as_str(),
            progress,
        );
    } else {
        handle.publish_failed(error_summary, progress);
    }
}

fn cancel_library_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    detail_message: &str,
) {
    let mut detail = BTreeMap::new();
    detail.insert(
        "cancel_detail".to_string(),
        CborValue::Text(detail_message.to_string()),
    );
    if let Err(err) = record_library_event(
        index,
        cfg,
        handle,
        library_serial,
        AuditEvent::CancelledBeforeDispatch,
        detail,
    ) {
        handle.publish_failed(
            format!("{detail_message}; audit record failed: {err}").as_str(),
            &[("phase", "audit")],
        );
    } else {
        handle.publish_state(
            pb::OperationState::Cancelled,
            &[("phase", "cancelled"), ("detail", detail_message)],
        );
    }
}

fn robotics_detail(action: &RoboticsAction) -> BTreeMap<String, CborValue> {
    let mut detail = BTreeMap::new();
    let put = |detail: &mut BTreeMap<String, CborValue>, key: &str, value: u16| {
        detail.insert(key.to_string(), CborValue::Integer(u64::from(value).into()));
    };
    match action {
        RoboticsAction::Refresh => {}
        RoboticsAction::Move { src, dst } => {
            put(&mut detail, "src", *src);
            put(&mut detail, "dst", *dst);
        }
        RoboticsAction::Load { slot, bay } => {
            put(&mut detail, "slot", *slot);
            put(&mut detail, "bay", *bay);
        }
        RoboticsAction::Unload { bay, destination } => {
            put(&mut detail, "bay", *bay);
            if let Some(dst) = destination {
                put(&mut detail, "destination", *dst);
            }
        }
    }
    detail
}
```

- [ ] **Step 4: Add `handle_robotics`**

```rust
fn handle_robotics(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library_serial: String,
    action: RoboticsAction,
    handle: crate::operations::OperationHandle,
) {
    let _busy_guard = SessionBusyGuard::from_reserved(cfg.session_busy.clone());

    if handle.is_cancelled() {
        cancel_library_operation(index, cfg, &handle, &library_serial, "cancelled before dispatch");
        return;
    }
    publish_running(&handle, &[("phase", "open")]);
    if let Err(err) = record_library_event(
        index,
        cfg,
        &handle,
        &library_serial,
        AuditEvent::OperationStarted,
        robotics_detail(&action),
    ) {
        fail_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            &format!("record operation start audit: {err}"),
            &[("phase", "audit")],
        );
        return;
    }

    let lib = match cfg.report.library(&library_serial) {
        Some(lib) => lib,
        None => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &format!("library {library_serial} not found in discovery report"),
                &[("phase", "open")],
            );
            return;
        }
    };
    let mut library = match lib.open(&cfg.policy) {
        Ok(handle) => handle,
        Err(err) => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &format!("open library: {err}"),
                &[("phase", "open")],
            );
            return;
        }
    };

    // Sync to current hardware state before acting and before publishing.
    if let Err(err) = library.refresh() {
        fail_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            &format!("refresh inventory: {err}"),
            &[("phase", "refresh")],
        );
        return;
    }

    let action_result: Result<(), String> = match &action {
        RoboticsAction::Refresh => Ok(()),
        RoboticsAction::Move { src, dst } => {
            library.move_medium(*src, *dst, &cfg.policy).map_err(|e| e.to_string())
        }
        RoboticsAction::Load { slot, bay } => {
            library.load(*slot, *bay, &cfg.policy).map_err(|e| e.to_string())
        }
        RoboticsAction::Unload { bay, destination } => {
            library.unload(*bay, *destination, &cfg.policy).map_err(|e| e.to_string())
        }
    };

    // Publish whatever the handle now reflects (refreshed, plus any applied
    // patch) so GetLibrary sees the latest read even on a failed move.
    publish_library_snapshot(&cfg.library_snapshot, library.library().clone());

    match action_result {
        Ok(()) => {
            if let Err(err) = record_library_event(
                index,
                cfg,
                &handle,
                &library_serial,
                AuditEvent::OperationFinished,
                BTreeMap::new(),
            ) {
                fail_library_operation(
                    index,
                    cfg,
                    &handle,
                    &library_serial,
                    &format!("record operation success audit: {err}"),
                    &[("phase", "audit")],
                );
                return;
            }
            handle.publish_state(pb::OperationState::Succeeded, &[("phase", "done")]);
        }
        Err(message) => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &message,
                &[("phase", "execute")],
            );
        }
    }
}
```
Note: the terminal-success audit event is `AuditEvent::OperationFinished` (confirmed — the reconcile path uses it at `write_owner.rs` ~line 782, then `publish_state(Succeeded)`; there is no `OperationSucceeded` variant).

- [ ] **Step 5: Build**

Run: `cargo check -p remanence-api`
Expected: clean. (A `variant Robotics is never constructed` / `RoboticsAction` dead-code warning is expected here and resolved in Task 3, where the gRPC handlers construct it. Do NOT add `#[allow]`; do not run the `-D warnings` gate until Task 4.)

- [ ] **Step 6: Confirm the command bounds hold**

The existing `channel_and_command_bounds_hold` test (`write_owner.rs` `mod tests`) asserts `SessionCommand: Send`. Run it:
Run: `cargo test -p remanence-api write_owner`
Expected: PASS (the new `Robotics` variant is Send — `String`/`RoboticsAction`/`OperationHandle` are all Send).

- [ ] **Step 7: Commit**

```bash
git add crates/remanence-api/src/write_owner.rs
git commit -m "S6b: owner Robotics command (open->refresh->act->publish)"
```

---

## Task 3: gRPC mutation handlers

**Files:**
- Modify: `crates/remanence-api/src/lib.rs` (add `record_library_request_received`)
- Modify: `crates/remanence-api/src/library.rs` (handlers + helpers + tests)

- [ ] **Step 1: Add the request-received audit helper to `ApiState`**

In `lib.rs` `impl ApiState`, beside `record_request_received`, add:
```rust
    fn record_library_request_received(
        &self,
        operation_id: Uuid,
        operation_kind: &str,
        library_serial: &str,
        mut detail: BTreeMap<String, CborValue>,
    ) -> Result<(), Status> {
        let mut index = CatalogIndex::open(self.index_path.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?;
        detail.insert(
            "library_serial".to_string(),
            CborValue::Text(library_serial.to_string()),
        );
        append_operation_audit(
            &mut index,
            self.audit_dir.as_ref(),
            self.audit_fsync,
            &self.audit_append_lock,
            OperationAuditInput {
                operation_id,
                operation_kind,
                event: AuditEvent::RequestReceived,
                subject_kind: "library",
                subject_id: Some(library_serial.to_string()),
                idempotency_key: None,
                detail,
            },
        )
    }
```

- [ ] **Step 2: Add imports + helpers to `library.rs`**

Add near the top of `library.rs`:
```rust
use std::collections::BTreeMap;

use ciborium::value::Value as CborValue;
use uuid::Uuid;
```
(`HashMap` is already imported; keep it. `Uuid` may already be imported — if so, skip the duplicate.)

Add the helpers in `impl LibraryServiceApi` (the struct already exists from S6a):
```rust
impl LibraryServiceApi {
    /// Resolve a request library_uuid to a configured library serial.
    fn resolve_library_serial(&self, library_uuid: &[u8]) -> Result<String, Status> {
        if library_uuid.is_empty() {
            return self
                .state
                .default_library_serial
                .as_ref()
                .map(|serial| serial.as_str().to_string())
                .ok_or_else(|| {
                    Status::invalid_argument(
                        "library_uuid is required when config does not name exactly one library",
                    )
                });
        }
        let requested = crate::decode_uuid_bytes(library_uuid, "library_uuid")?;
        let snapshot = self
            .state
            .current_library_snapshot()
            .ok_or_else(|| Status::not_found("library not found"))?;
        snapshot
            .report
            .libraries
            .iter()
            .find(|lib| library_uuid(&lib.serial) == requested)
            .map(|lib| lib.serial.clone())
            .ok_or_else(|| Status::not_found("library not found"))
    }

    /// Reserve the owner, register an operation, dispatch the robotics command,
    /// and return its OperationRef immediately (fire-and-track, like ReconcileTape).
    async fn dispatch_robotics(
        &self,
        library_uuid: &[u8],
        operation_kind: &'static str,
        action: crate::write_owner::RoboticsAction,
        detail: BTreeMap<String, CborValue>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let library_serial = self.resolve_library_serial(library_uuid)?;
        let tx = self
            .state
            .session_tx
            .as_ref()
            .ok_or_else(|| Status::unavailable("daemon has no drive-session owner"))?;
        let reservation = self.state.reserve_session_owner()?;
        let operation_id = Uuid::new_v4();
        self.state
            .record_library_request_received(operation_id, operation_kind, &library_serial, detail)?;
        let handle = self.state.operations.register(operation_id, operation_kind);
        match tx.try_send(crate::write_owner::SessionCommand::Robotics {
            library_serial,
            action,
            handle: handle.clone(),
        }) {
            Ok(()) => {
                reservation.commit();
                Ok(Response::new(pb::OperationRef {
                    operation_id: operation_id.as_bytes().to_vec(),
                }))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let error = "drive-session owner is busy";
                self.state
                    .record_operation_failed(operation_id, operation_kind, error)?;
                handle.publish_failed(error, &[("phase", "dispatch")]);
                Err(Status::failed_precondition(error))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                let error = "drive-session owner is stopped";
                self.state
                    .record_operation_failed(operation_id, operation_kind, error)?;
                handle.publish_failed(error, &[("phase", "dispatch")]);
                Err(Status::unavailable(error))
            }
        }
    }
}

/// Narrow a wire u32 element address to the u16 the changer layer uses.
fn narrow_element(address: u32, field: &str) -> Result<u16, Status> {
    u16::try_from(address)
        .map_err(|_| Status::invalid_argument(format!("{field} exceeds the 16-bit element range")))
}
```
Note: the inherent method `resolve_library_serial` calls the module-level `library_uuid(&lib.serial)` fn — both names coexist (one is a free fn, one is a method); inside the closure `library_uuid(&lib.serial)` resolves to the free fn. If the compiler flags ambiguity, qualify the free fn as `crate::library::library_uuid` or rename the local binding.

- [ ] **Step 3: Replace the four `unimplemented` stubs**

In the `impl LibraryService for LibraryServiceApi` block, replace the bodies of `refresh_inventory`, `move_medium`, `load_drive`, `unload_drive` (keep `import_element`/`export_element`/`stream_library_events` as their S6b/S6c stubs):
```rust
    async fn refresh_inventory(
        &self,
        request: Request<pb::RefreshInventoryRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let request = request.into_inner();
        self.dispatch_robotics(
            &request.library_uuid,
            "refresh_inventory",
            crate::write_owner::RoboticsAction::Refresh,
            BTreeMap::new(),
        )
        .await
    }

    async fn move_medium(
        &self,
        request: Request<pb::MoveMediumRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let request = request.into_inner();
        let src = narrow_element(request.source_element_address, "source_element_address")?;
        let dst = narrow_element(request.destination_element_address, "destination_element_address")?;
        let mut detail = BTreeMap::new();
        detail.insert("src".to_string(), CborValue::Integer(u64::from(src).into()));
        detail.insert("dst".to_string(), CborValue::Integer(u64::from(dst).into()));
        self.dispatch_robotics(
            &request.library_uuid,
            "move_medium",
            crate::write_owner::RoboticsAction::Move { src, dst },
            detail,
        )
        .await
    }

    async fn load_drive(
        &self,
        request: Request<pb::LoadDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let request = request.into_inner();
        let slot = narrow_element(request.slot_element_address, "slot_element_address")?;
        let bay = narrow_element(request.drive_element_address, "drive_element_address")?;
        let mut detail = BTreeMap::new();
        detail.insert("slot".to_string(), CborValue::Integer(u64::from(slot).into()));
        detail.insert("bay".to_string(), CborValue::Integer(u64::from(bay).into()));
        self.dispatch_robotics(
            &request.library_uuid,
            "load_drive",
            crate::write_owner::RoboticsAction::Load { slot, bay },
            detail,
        )
        .await
    }

    async fn unload_drive(
        &self,
        request: Request<pb::UnloadDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let request = request.into_inner();
        let bay = narrow_element(request.drive_element_address, "drive_element_address")?;
        let destination = if request.destination_slot_address == 0 {
            None
        } else {
            Some(narrow_element(request.destination_slot_address, "destination_slot_address")?)
        };
        let mut detail = BTreeMap::new();
        detail.insert("bay".to_string(), CborValue::Integer(u64::from(bay).into()));
        if let Some(dst) = destination {
            detail.insert("destination".to_string(), CborValue::Integer(u64::from(dst).into()));
        }
        self.dispatch_robotics(
            &request.library_uuid,
            "unload_drive",
            crate::write_owner::RoboticsAction::Unload { bay, destination },
            detail,
        )
        .await
    }
```

- [ ] **Step 4: Add unit + integration tests**

In `library.rs` `mod tests`, add:
```rust
    #[test]
    fn narrow_element_accepts_u16_and_rejects_overflow() {
        assert_eq!(narrow_element(0x0400, "x").unwrap(), 0x0400);
        assert_eq!(narrow_element(0, "x").unwrap(), 0);
        assert_eq!(
            narrow_element(0x1_0000, "x").unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[tokio::test]
    async fn refresh_inventory_without_owner_is_unavailable() {
        // snapshot present (so resolution succeeds) but no session_tx/owner.
        let err = state_with_snapshot()
            .library_service()
            .refresh_inventory(Request::new(pb::RefreshInventoryRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
                idempotency_key: None,
            }))
            .await
            .expect_err("no owner");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn move_medium_rejects_overflow_element_before_dispatch() {
        let err = state_with_snapshot()
            .library_service()
            .move_medium(Request::new(pb::MoveMediumRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
                source_element_address: 0x1_0000,
                destination_element_address: 0x0100,
                idempotency_key: None,
            }))
            .await
            .expect_err("overflow");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn move_medium_unknown_library_is_not_found() {
        let err = state_with_snapshot()
            .library_service()
            .move_medium(Request::new(pb::MoveMediumRequest {
                library_uuid: vec![0u8; 16],
                source_element_address: 0x0400,
                destination_element_address: 0x0100,
                idempotency_key: None,
            }))
            .await
            .expect_err("unknown library");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn unload_drive_zero_destination_resolves_without_owner_error_path() {
        // destination 0 -> None must not be rejected as an overflow/invalid arg;
        // it fails later at the missing owner.
        let err = state_with_snapshot()
            .library_service()
            .unload_drive(Request::new(pb::UnloadDriveRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
                drive_element_address: 0x0001,
                destination_slot_address: 0,
                idempotency_key: None,
            }))
            .await
            .expect_err("no owner");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p remanence-api library::tests`
Expected: PASS (S6a's 12 + the 5 new = 17).

- [ ] **Step 6: Commit**

```bash
git add crates/remanence-api/src/lib.rs crates/remanence-api/src/library.rs
git commit -m "S6b: wire RefreshInventory/MoveMedium/LoadDrive/UnloadDrive gRPC handlers"
```

---

## Task 4: Workspace gates

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt --all`; review `git diff --stat`.

- [ ] **Step 2: Clippy (workspace, `-D warnings`)**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean (the Task 2 transient dead-code warning is now resolved — the handlers construct `Robotics`).

- [ ] **Step 3: Test (workspace)**

Run: `cargo test --workspace`
Expected: PASS (S1/S4a/S5a/S3a/S6a regression + the new S6b tests + the daemon `serve_catalog` e2e).

- [ ] **Step 4: Commit any formatting**

```bash
git add -A
git commit -m "S6b: fmt" || echo "nothing to format"
```

---

## Self-Review (completed during planning)

**Spec coverage:** mutable cell + `current_library_snapshot` (Task 1) ✓; S6a read-path switch (Task 1) ✓; owner `Robotics` open→refresh→act→publish (Task 2) ✓; RefreshInventory/MoveMedium/LoadDrive/UnloadDrive fire-and-track (Task 3) ✓; library_uuid→serial resolution incl. empty→default (Task 3) ✓; u32→u16 narrowing + dest 0→None (Task 3) ✓; single-session reservation + before-dispatch cancel (Task 2 `is_cancelled` + reservation) ✓; operation-level audit with src/dst detail (Tasks 2-3) ✓. Owner-side execution errors surface as terminal `FAILED` with the library error's message (Task 2) — the spec's MoveError/LoadError/UnloadError taxonomy is the conceptual cause; async ops carry it in the summary, while dispatch-time validation (uuid/element/owner-busy) returns gRPC codes synchronously (Task 3). AC3 (harness e2e) is human-run.

**Placeholder scan:** none — every step has concrete code/commands. The terminal-success `AuditEvent` variant was verified as `OperationFinished` (Task 2 Step 4).

**Type consistency:** the cell is `Arc<RwLock<Arc<LibrarySnapshot>>>` everywhere (ApiState field, `with_session_owner`, `WriteOwnerConfig`, the test helper, `publish_library_snapshot`); `current_library_snapshot()` returns `Option<Arc<LibrarySnapshot>>`; element addresses narrow `u32 -> u16` once, in `narrow_element`; the free `library_uuid` fn and the `resolve_library_serial` method coexist (ambiguity note included); `RoboticsAction`/`SessionCommand::Robotics` field names match between owner and handlers.
