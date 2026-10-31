//! Durable tape-I/O fence evidence and restart completion. Callers serialize
//! audit appends with the daemon audit mutex or the exclusive offline state lock.

use crate::{
    AuditActor, AuditEvent, AuditEventRecord, AuditRecord, AuditSubject, CatalogIndex, DriveKey,
    FileAuditLog, SourceLayer, StateError, TapeIoFenceInput, TapeIoFenceRecord,
};
use ciborium::value::Value;
use std::{collections::BTreeMap, ops::ControlFlow, path::Path};

/// Finish raises and releases whose evidence survives a crash before projection.
pub fn reconcile(index: &mut CatalogIndex, audit_dir: &Path) -> Result<(), StateError> {
    let mut records = Vec::new();
    FileAuditLog::replay_incremental(audit_dir, |record| {
        if matches!(
            record.event,
            AuditEvent::TapeIoFenceRaised | AuditEvent::TapeIoFenceReleased
        ) {
            records.push(record);
        }
        ControlFlow::Continue(())
    })?;
    if !records.is_empty() {
        FileAuditLog::open(audit_dir, true)?.sync()?;
    }
    for record in records {
        project(index, &record)?;
    }
    Ok(())
}

fn project(index: &mut CatalogIndex, record: &AuditRecord) -> Result<(), StateError> {
    let text = |key: &str| match record.detail.get(key) {
        Some(Value::Text(value)) => Some(value.as_str()),
        _ => None,
    };
    let id = text("quarantine_id")
        .or(record.subject.id.as_deref())
        .ok_or_else(|| StateError::IndexCorrupt("fence evidence has no quarantine id".into()))?;
    if record.event == AuditEvent::TapeIoFenceRaised {
        if index.tape_io_fence_by_quarantine_id(id)?.is_none() {
            index.project_audit_record(record)?;
            if index.tape_io_fence_by_quarantine_id(id)?.is_none() {
                return Err(StateError::IndexCorrupt(
                    "fence raise has incomplete evidence".into(),
                ));
            }
        }
        return Ok(());
    }
    let ack = text("release_ack")
        .ok_or_else(|| StateError::IndexCorrupt("fence release has no acknowledgment".into()))?;
    index.complete_tape_io_fence_release(id, ack, &record.timestamp_utc)?;
    Ok(())
}

/// Append and sync all fence evidence before projecting its catalog row.
/// Callers hold the shared audit append mutex or the exclusive offline state lock.
pub fn raise(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    input: TapeIoFenceInput,
    endpoint: Option<&DriveKey>,
    actor: AuditActor,
) -> Result<TapeIoFenceRecord, StateError> {
    raise_with_fault(index, audit_dir, input, endpoint, actor, || Ok(()))
}

fn raise_with_fault(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    input: TapeIoFenceInput,
    endpoint: Option<&DriveKey>,
    actor: AuditActor,
    after_sync: impl FnOnce() -> Result<(), StateError>,
) -> Result<TapeIoFenceRecord, StateError> {
    let id = format!(
        "tioq-{}-{}",
        uuid::Uuid::from_bytes(input.tape_uuid),
        uuid::Uuid::new_v4()
    );
    let mut detail = BTreeMap::from([
        ("quarantine_id".into(), Value::Text(id.clone())),
        ("tape_uuid".into(), Value::Bytes(input.tape_uuid.to_vec())),
        ("reason".into(), Value::Text(input.reason)),
    ]);
    if let Some(barcode) = input.barcode {
        detail.insert("barcode".into(), Value::Text(barcode));
    }
    if let Some(evidence) = input.evidence_json {
        detail.insert("evidence_json".into(), Value::Text(evidence));
    }
    if let Some(endpoint) = endpoint {
        detail.insert("endpoint_key".into(), Value::Text(endpoint.encode()));
    }
    let mut audit = FileAuditLog::open(audit_dir, true)?;
    let (_, evidence) = audit.append_and_return_record(AuditEventRecord {
        actor,
        source_layer: SourceLayer::Layer4,
        operation_id: None,
        session_id: None,
        idempotency_key: None,
        event: AuditEvent::TapeIoFenceRaised,
        subject: AuditSubject {
            kind: "tape_io_fence".into(),
            id: Some(id.clone()),
        },
        detail,
    })?;
    audit.sync()?;
    after_sync()?;
    project(index, &evidence)?;
    index
        .tape_io_fence_by_quarantine_id(&id)?
        .ok_or_else(|| StateError::IndexCorrupt("tape-I/O fence vanished".into()))
}

/// Startup fence for a tape whose log is absent or ends below its projection mark.
/// It is the only reason that is released without an operator.
pub const TAPE_LOG_BELOW_PROJECTION: &str = "tape_log_below_projection";
/// Startup fence for a tape whose catalog projection mark points inside its
/// validated log but does not name a closing frame there. Unlike
/// [`TAPE_LOG_BELOW_PROJECTION`], only an operator can release it.
pub const TAPE_LOG_PROJECTION_MARK_INVALID: &str = "tape_log_projection_mark_invalid";
/// Acknowledgment recorded when startup proves that condition has ended.
pub const RESTORED_PROJECTION_RELEASE_ACK: &str =
    "automatic: tape log validates at or above its projection mark";

/// Persist and sync a release once, then clear its active catalog row.
pub fn release(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    id: &str,
    ack: &str,
    actor: AuditActor,
) -> Result<Option<TapeIoFenceRecord>, StateError> {
    release_with_fault(index, audit_dir, id, ack, actor, BTreeMap::new(), || Ok(()))
}

/// Release a [`TAPE_LOG_BELOW_PROJECTION`] fence after the caller validated the
/// tape's log at or above its projection mark. Every other reason is refused.
pub fn release_restored_projection(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    id: &str,
    evidence_json: String,
) -> Result<Option<TapeIoFenceRecord>, StateError> {
    let reason = index
        .tape_io_fence_by_quarantine_id(id)?
        .map(|fence| fence.reason);
    if reason.as_deref() != Some(TAPE_LOG_BELOW_PROJECTION) {
        return Err(StateError::ConfigInvalid(format!(
            "fence {id} is not a {TAPE_LOG_BELOW_PROJECTION} fence; only an operator can release it"
        )));
    }
    let extra = BTreeMap::from([
        ("automatic_release".into(), Value::Bool(true)),
        (
            "reason".into(),
            Value::Text(TAPE_LOG_BELOW_PROJECTION.into()),
        ),
        ("evidence_json".into(), Value::Text(evidence_json)),
    ]);
    release_with_fault(
        index,
        audit_dir,
        id,
        RESTORED_PROJECTION_RELEASE_ACK,
        AuditActor::System,
        extra,
        || Ok(()),
    )
}

fn release_with_fault(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    id: &str,
    ack: &str,
    actor: AuditActor,
    extra: BTreeMap<String, Value>,
    after_sync: impl FnOnce() -> Result<(), StateError>,
) -> Result<Option<TapeIoFenceRecord>, StateError> {
    if ack.trim().is_empty() {
        return Err(StateError::ConfigInvalid(
            "acknowledgment must not be empty".into(),
        ));
    }
    reconcile(index, audit_dir)?;
    let Some(record) = index.tape_io_fence_by_quarantine_id(id)? else {
        return Ok(None);
    };
    if record.state == "released" {
        return Ok(Some(record));
    }
    let mut detail = BTreeMap::from([
        ("quarantine_id".into(), Value::Text(id.into())),
        ("tape_uuid".into(), Value::Bytes(record.tape_uuid)),
        ("release_ack".into(), Value::Text(ack.into())),
    ]);
    if let Some(barcode) = record.barcode {
        detail.insert("barcode".into(), Value::Text(barcode));
    }
    if let Some(endpoint) = record.endpoint {
        detail.insert("endpoint_key".into(), Value::Text(endpoint.encode()));
    }
    detail.extend(extra);
    let mut audit = FileAuditLog::open(audit_dir, true)?;
    let (_, evidence) = audit.append_and_return_record(AuditEventRecord {
        actor,
        source_layer: SourceLayer::Layer4,
        operation_id: None,
        session_id: None,
        idempotency_key: None,
        event: AuditEvent::TapeIoFenceReleased,
        subject: AuditSubject {
            kind: "tape_io_fence".into(),
            id: Some(id.into()),
        },
        detail,
    })?;
    audit.sync()?;
    after_sync()?;
    project(index, &evidence)?;
    index.tape_io_fence_by_quarantine_id(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Startup projects a synced raise after a crash, with no duplicate on retry.
    #[test]
    fn raise_crash_before_projection_recovers_at_startup_once() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
read_only = false
[journal]
dir = "{0}/journals"
require_trusted_volume = false
[audit]
dir = "{0}/custom-audit"
fsync = false
[index]
sqlite_path = "{0}/index.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
"#,
                dir.path().display()
            ),
        )
        .unwrap();
        let mut state = crate::StateHandle::open_from_config_file(&config_path).unwrap();
        let audit = state.paths().audit_dir.clone();
        let endpoint = DriveKey::Standalone {
            drive_serial: "CRASH-DRIVE".into(),
        };
        assert!(raise_with_fault(
            state.catalog_index(),
            &audit,
            TapeIoFenceInput {
                tape_uuid: [2; 16],
                barcode: Some("CRASH001".into()),
                reason: "transfer_error".into(),
                evidence_json: Some(r#"{"error":"uncertain transfer"}"#.into()),
            },
            Some(&endpoint),
            AuditActor::System,
            || Err(StateError::IndexCorrupt("injected after audit sync".into()))
        )
        .is_err());
        assert!(state
            .catalog_index()
            .list_active_tape_io_fences()
            .unwrap()
            .is_empty());
        let records = FileAuditLog::replay(&audit).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].event, AuditEvent::TapeIoFenceRaised);
        drop(state);
        for _ in 0..2 {
            let mut state = crate::StateHandle::open_from_config_file(&config_path).unwrap();
            let fences = state.catalog_index().list_active_tape_io_fences().unwrap();
            assert_eq!(fences.len(), 1);
            assert_eq!(fences[0].endpoint, Some(endpoint.clone()));
            assert_eq!(fences[0].tape_uuid, [2; 16]);
            assert_eq!(fences[0].barcode.as_deref(), Some("CRASH001"));
            assert_eq!(fences[0].created_at_utc, records[0].timestamp_utc);
            reconcile(state.catalog_index(), &audit).unwrap();
            assert_eq!(
                state.catalog_index().list_active_tape_io_fences().unwrap(),
                fences
            );
            assert_eq!(FileAuditLog::replay(&audit).unwrap(), records);
        }
    }

    /// Audit failure leaves no catalog-only fence that a rebuild can lose.
    #[test]
    fn raise_audit_failure_leaves_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = CatalogIndex::open(dir.path().join("index.sqlite")).unwrap();
        let audit = dir.path().join("blocked");
        std::fs::write(&audit, b"not a directory").unwrap();
        assert!(raise(
            &mut index,
            &audit,
            TapeIoFenceInput {
                tape_uuid: [3; 16],
                barcode: None,
                reason: "transfer_error".into(),
                evidence_json: None,
            },
            None,
            AuditActor::System
        )
        .is_err());
        assert!(index.list_active_tape_io_fences().unwrap().is_empty());
    }

    /// Reconciliation preserves a released row and replays ordered evidence into an empty catalog.
    #[test]
    fn reconcile_raise_and_release_preserves_release_and_rebuilds_empty_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit");
        let mut index = CatalogIndex::open(dir.path().join("index.sqlite")).unwrap();
        let raised = raise(
            &mut index,
            &audit,
            TapeIoFenceInput {
                tape_uuid: [4; 16],
                barcode: None,
                reason: "transfer_error".into(),
                evidence_json: None,
            },
            Some(&DriveKey::new("LIB", 0)),
            AuditActor::System,
        )
        .unwrap();
        let released = release(
            &mut index,
            &audit,
            &raised.quarantine_id,
            "checked",
            AuditActor::System,
        )
        .unwrap()
        .unwrap();
        let mut empty = CatalogIndex::open(dir.path().join("empty.sqlite")).unwrap();
        for catalog in [&mut index, &mut empty] {
            for _ in 0..2 {
                reconcile(catalog, &audit).unwrap();
                assert_eq!(
                    catalog
                        .tape_io_fence_by_quarantine_id(&raised.quarantine_id)
                        .unwrap()
                        .unwrap(),
                    released
                );
                assert!(catalog.list_active_tape_io_fences().unwrap().is_empty());
            }
        }
        assert_eq!(FileAuditLog::replay(&audit).unwrap().len(), 2);
    }

    /// A fault after fsync leaves an active fence; retry and restart preserve one release.
    #[test]
    fn release_crash_retries_and_restart_complete_once() {
        for restart in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("index.sqlite");
            let audit = dir.path().join("audit");
            let mut index = CatalogIndex::open(&path).unwrap();
            let endpoint = crate::DriveKey::new("LIB", 1);
            let fence = index
                .record_tape_io_fence_at_endpoint(
                    crate::TapeIoFenceInput {
                        tape_uuid: [0; 16],
                        barcode: None,
                        reason: "transfer_error".into(),
                        evidence_json: None,
                    },
                    Some(&endpoint),
                )
                .unwrap();
            let result = release_with_fault(
                &mut index,
                &audit,
                &fence.quarantine_id,
                "checked drive",
                AuditActor::System,
                BTreeMap::new(),
                || Err(StateError::IndexCorrupt("injected after audit sync".into())),
            );
            assert!(result.is_err());
            assert_eq!(index.list_active_tape_io_fences().unwrap().len(), 1);
            let records = FileAuditLog::replay(&audit).unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].event, AuditEvent::TapeIoFenceReleased);
            drop(index);
            let mut index = CatalogIndex::open(&path).unwrap();
            if restart {
                reconcile(&mut index, &audit).unwrap();
            }
            let released = release(
                &mut index,
                &audit,
                &fence.quarantine_id,
                "different acknowledgment",
                AuditActor::System,
            )
            .unwrap()
            .unwrap();
            assert_eq!(released.release_ack.as_deref(), Some("checked drive"));
            assert_eq!(released.updated_at_utc, records[0].timestamp_utc);
            assert_eq!(released.endpoint, Some(endpoint));
            assert!(index.list_active_tape_io_fences().unwrap().is_empty());
            assert_eq!(FileAuditLog::replay(&audit).unwrap().len(), 1);
        }
    }

    /// Failed evidence append never clears a fence.
    #[test]
    fn release_audit_failure_keeps_fence_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = CatalogIndex::open(dir.path().join("index.sqlite")).unwrap();
        let fence = index
            .record_tape_io_fence(crate::TapeIoFenceInput {
                tape_uuid: [1; 16],
                barcode: None,
                reason: "transfer_error".into(),
                evidence_json: None,
            })
            .unwrap();
        let audit = dir.path().join("not-a-directory");
        std::fs::write(&audit, b"blocked").unwrap();
        assert!(release(
            &mut index,
            &audit,
            &fence.quarantine_id,
            "checked",
            AuditActor::System
        )
        .is_err());
        assert_eq!(index.list_active_tape_io_fences().unwrap().len(), 1);
    }

    /// Only a below-projection fence is released automatically, with evidence.
    #[test]
    fn restored_projection_release_refuses_every_other_reason() {
        let dir = tempfile::tempdir().unwrap();
        let audit = dir.path().join("audit");
        let mut index = CatalogIndex::open(dir.path().join("index.sqlite")).unwrap();
        let raise_reason = |index: &mut CatalogIndex, reason: &str| {
            raise(
                index,
                &audit,
                TapeIoFenceInput {
                    tape_uuid: [5; 16],
                    barcode: None,
                    reason: reason.into(),
                    evidence_json: None,
                },
                None,
                AuditActor::System,
            )
            .unwrap()
        };
        let other = raise_reason(&mut index, "checkpoint_barrier_failed");
        let below = raise_reason(&mut index, TAPE_LOG_BELOW_PROJECTION);
        assert!(
            release_restored_projection(&mut index, &audit, &other.quarantine_id, "{}".into())
                .unwrap_err()
                .to_string()
                .contains("only an operator can release it")
        );
        let released =
            release_restored_projection(&mut index, &audit, &below.quarantine_id, "{}".into())
                .unwrap()
                .unwrap();
        assert_eq!(released.state, "released");
        assert_eq!(
            released.release_ack.as_deref(),
            Some(RESTORED_PROJECTION_RELEASE_ACK)
        );
        let active = index.list_active_tape_io_fences().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].quarantine_id, other.quarantine_id);
        let record = FileAuditLog::replay(&audit).unwrap().pop().unwrap();
        assert_eq!(record.event, AuditEvent::TapeIoFenceReleased);
        assert_eq!(
            record.detail.get("automatic_release"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            record.detail.get("reason"),
            Some(&Value::Text(TAPE_LOG_BELOW_PROJECTION.into()))
        );
    }
}
