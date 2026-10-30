//! Standalone BOT initialization; Q2 retains this orchestration until architecture step 5.

use crate::{
    mount_support::{
        raise_standalone_fence, refusal, standalone_fence_policy, MountError, OperatorMount,
    },
    TapeUuid,
};
use remanence_library::DriveHandle;
use remanence_state::{
    AuditActor, AuditEvent, AuditEventRecord, AuditSubject, SourceLayer, StateHandle,
};
use std::collections::BTreeMap;

impl OperatorMount<'_> {
    /// Prepare a BOT initialization decision, then run its write and provisioning
    /// under the same durable calibration/session guards as mounted writes.
    /// Preparation must only inspect/configure media; it returns the planned UUID.
    pub fn initialize<T, R>(
        &mut self,
        reserve: bool,
        prepare: impl FnOnce(&mut DriveHandle, &mut StateHandle) -> Result<(TapeUuid, T), String>,
        write_and_provision: impl FnOnce(&mut DriveHandle, &mut StateHandle, T) -> Result<R, String>,
    ) -> Result<R, MountError> {
        let mut drive = self
            .context
            .drive
            .take()
            .ok_or_else(|| refusal("one cartridge per invocation"))?;
        if drive.endpoint().durable_key().is_none() {
            return Err(refusal("ephemeral endpoint is read-only"));
        }
        let mut reserved = false;
        let result = (|| {
            if reserve {
                drive.reserve().map_err(refusal)?;
                reserved = true;
            }
            let (uuid, plan) = prepare(&mut drive, self.state).map_err(refusal)?;
            self.flush_audit()?;
            standalone_fence_policy(
                self.state.catalog_index(),
                Some(uuid),
                drive.endpoint().durable_key(),
                true,
            )?;
            let control = self.state.calibration_control().clone();
            crate::calibration::install_standalone_calibration_fence(&mut drive, &control, uuid)
                .map_err(refusal)?;
            let id = uuid::Uuid::new_v4();
            self.session(uuid, id, AuditEvent::SessionOpened)?;
            let before = drive.media_write_dispatch_count();
            let result = write_and_provision(&mut drive, self.state, plan).map_err(refusal);
            self.flush_audit()?;
            let uncertain = result.is_err() && drive.media_write_dispatch_count() != before;
            if uncertain {
                self.unusable = true;
                raise_standalone_fence(
                    self.state,
                    uuid,
                    self.context.endpoint.durable_key(),
                    "standalone_init_failed",
                    &result.as_ref().err().expect("failed write").to_string(),
                )?;
            }
            self.session(
                uuid,
                id,
                if uncertain {
                    AuditEvent::SessionOrphaned
                } else {
                    AuditEvent::SessionClosed
                },
            )?;
            result
        })();
        let allow = drive.allow_medium_removal().map_err(refusal);
        let release = if reserved {
            drive.release_reservation().map_err(refusal)
        } else {
            Ok(())
        };
        self.flush_audit()?;
        let value = result?;
        allow?;
        release?;
        Ok(value)
    }

    pub(crate) fn session(
        &mut self,
        tape_uuid: TapeUuid,
        id: uuid::Uuid,
        event: AuditEvent,
    ) -> Result<(), MountError> {
        self.flush_audit()?;
        let key = self
            .context
            .endpoint
            .durable_key()
            .ok_or_else(|| refusal("ephemeral endpoint is read-only"))?;
        let detail = BTreeMap::from([
            ("session_kind".into(), ciborium::Value::Text("write".into())),
            (
                "tape_uuid".into(),
                ciborium::Value::Bytes(tape_uuid.to_vec()),
            ),
            ("endpoint_key".into(), ciborium::Value::Text(key.encode())),
        ]);
        // Session recovery evidence must reach stable storage before any write,
        // even when ordinary audit appends are configured without fsync.
        use remanence_state::AuditSink;
        let mut log = remanence_state::FileAuditLog::open(&self.state.paths().audit_dir, true)
            .map_err(refusal)?;
        log.append(AuditEventRecord {
            event,
            actor: AuditActor::local_user(),
            source_layer: SourceLayer::Layer5,
            session_id: Some(id),
            operation_id: None,
            idempotency_key: None,
            subject: AuditSubject {
                kind: "write".into(),
                id: Some(id.to_string()),
            },
            detail,
        })
        .map_err(refusal)?;
        self.state.refresh_audit_append_cursor().map_err(refusal)?;
        self.state.replay_audit_projection().map_err(refusal)?;
        Ok(())
    }
}
