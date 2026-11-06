//! Standalone BOT initialization; Q2 retains this orchestration until architecture step 5.

use crate::{
    mount_support::{
        raise_standalone_fence, refusal, standalone_fence_policy, MountError, OperatorMount,
    },
    TapeUuid,
};
use remanence_library::DriveHandle;
use remanence_state::{AuditEvent, StateHandle};

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
                self.state.catalog(),
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
            let _outcome_context =
                drive.scoped_outcome_context(remanence_library::outcome::OperationContext {
                    phase: remanence_library::outcome::Phase::Initialization,
                    region: remanence_library::outcome::Region::Bot,
                    mounted: true,
                    ..Default::default()
                });
            let result = write_and_provision(&mut drive, self.state, plan).map_err(refusal);
            self.flush_audit()?;
            let uncertain = result.is_err() && drive.media_write_dispatch_count() != before;
            if uncertain {
                self.unusable = true;
                let outcome = drive
                    .media_failure_outcome_after(before)
                    .filter(|outcome| {
                        // D2 preserves issued write results, including R6's definite rejection.
                        // A local adapter refusal cannot account for the preceding media writes.
                        outcome.write_context
                            && outcome.evidence.dispatch
                                == remanence_library::outcome::Dispatch::Issued
                    })
                    .cloned()
                    .unwrap_or_else(|| {
                        remanence_library::outcome::finish_abandoned_write(
                            remanence_library::outcome::OperationContext {
                                phase: remanence_library::outcome::Phase::Initialization,
                                region: remanence_library::outcome::Region::Bot,
                                mounted: true,
                                ..Default::default()
                            },
                        )
                    });
                drive.retain_operation_failure(before, outcome.clone());
                if let remanence_library::outcome::Effect::WriteFence(cause) = outcome.effect {
                    raise_standalone_fence(
                        self.state,
                        uuid,
                        self.context.endpoint.durable_key(),
                        cause.reason(),
                        &result.as_ref().err().expect("failed write").to_string(),
                    )?;
                }
                drive.mark_media_failure_outcome_applied();
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

    /// C8-G5: a bootstrap write needs a durable pre-write session obligation,
    /// but native SessionOpened requires an already committed Bootstrap. Keep
    /// the real write gate closed; legacy audit is no longer that authority.
    pub(crate) fn session(
        &mut self,
        _tape_uuid: TapeUuid,
        _id: uuid::Uuid,
        _event: AuditEvent,
    ) -> Result<(), MountError> {
        self.flush_audit()?;
        Err(refusal("standalone initialization requires durable pre-Bootstrap write-session authority; no Bootstrap bytes were written"))
    }
}
