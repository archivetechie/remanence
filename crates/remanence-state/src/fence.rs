//! Durable tape-I/O release evidence and restart completion. Callers serialize
//! audit appends with the daemon audit mutex or the exclusive offline state lock.

use crate::{
    AuditActor, AuditEvent, AuditEventRecord, AuditRecord, AuditSubject, CatalogIndex,
    FileAuditLog, SourceLayer, StateError, TapeIoFenceRecord,
};
use ciborium::value::Value;
use std::{collections::BTreeMap, ops::ControlFlow, path::Path};

/// Finish releases whose evidence survives a crash before catalog projection.
pub fn reconcile(index: &mut CatalogIndex, audit_dir: &Path) -> Result<(), StateError> {
    let mut releases = Vec::new();
    FileAuditLog::replay_incremental(audit_dir, |record| {
        if record.event == AuditEvent::TapeIoFenceReleased {
            releases.push(record);
        }
        ControlFlow::Continue(())
    })?;
    if !releases.is_empty() {
        FileAuditLog::open(audit_dir, true)?.sync()?;
    }
    for record in releases {
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
        .ok_or_else(|| StateError::IndexCorrupt("fence release has no quarantine id".into()))?;
    let ack = text("release_ack")
        .ok_or_else(|| StateError::IndexCorrupt("fence release has no acknowledgment".into()))?;
    index.complete_tape_io_fence_release(id, ack, &record.timestamp_utc)?;
    Ok(())
}

/// Persist and sync a release once, then clear its active catalog row.
pub fn release(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    id: &str,
    ack: &str,
    actor: AuditActor,
) -> Result<Option<TapeIoFenceRecord>, StateError> {
    release_with_fault(index, audit_dir, id, ack, actor, || Ok(()))
}

fn release_with_fault(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    id: &str,
    ack: &str,
    actor: AuditActor,
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
}
