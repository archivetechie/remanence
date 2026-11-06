//! Typed operation inputs selected by real owners. Fingerprints are computed
//! from explicit request/action fields before persistence, never audit maps.
use remanence_state::library_log as lib;
use tonic::Status;

pub(crate) fn kind(value: &str) -> Result<lib::OperationKind, Status> {
    Ok(match value {
        "ack_alarm" => lib::OperationKind::AckAlarm,
        "drive_stewardship" => lib::OperationKind::DriveStewardship,
        "clean_drive" => lib::OperationKind::CleanDrive,
        "refresh_inventory" => lib::OperationKind::RefreshInventory,
        "move_medium" => lib::OperationKind::MoveMedium,
        "load_drive" => lib::OperationKind::LoadDrive,
        "unload_drive" => lib::OperationKind::UnloadDrive,
        "media_readiness_wait" => lib::OperationKind::MediaReadinessWait,
        "tape_verification" => lib::OperationKind::TapeVerification,
        "finalize_tape" => lib::OperationKind::FinalizeTape,
        "reconcile_tape" => lib::OperationKind::ReconcileTape,
        "tape_closeout" => lib::OperationKind::TapeCloseout,
        "closeout_overdue" => lib::OperationKind::CloseoutOverdue,
        _ => {
            return Err(Status::failed_precondition(format!(
                "operation kind {value:?} has no typed owner mapping"
            )))
        }
    })
}
/// Domain-separated, length-framed digest of the actual normalized action.
pub(crate) fn robotics_fingerprint(
    library: &str,
    action: &crate::write_owner::RoboticsAction,
) -> [u8; 32] {
    use crate::write_owner::RoboticsAction as A;
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"remanence:robotics-request:v1\0");
    h.update((library.len() as u64).to_le_bytes());
    h.update(library.as_bytes());
    match action {
        A::Refresh => h.update([0]),
        A::Move { src, dst } => {
            h.update([1]);
            h.update(src.to_le_bytes());
            h.update(dst.to_le_bytes());
        }
        A::Load {
            slot,
            bay,
            wait_ready,
        } => {
            h.update([2]);
            h.update(slot.to_le_bytes());
            h.update(bay.to_le_bytes());
            h.update([u8::from(*wait_ready)]);
        }
        A::Unload { bay, destination } => {
            h.update([3]);
            h.update(bay.to_le_bytes());
            h.update([u8::from(destination.is_some())]);
            if let Some(dst) = destination {
                h.update(dst.to_le_bytes());
            }
        }
        A::Clean {
            drive_uuid,
            trigger,
        } => {
            h.update([4]);
            h.update((drive_uuid.len() as u64).to_le_bytes());
            h.update(drive_uuid);
            h.update((trigger.len() as u64).to_le_bytes());
            h.update(trigger.as_bytes());
        }
    }
    h.finalize().into()
}
/// Fixture rendering runs in the reverse direction only; production never
/// constructs typed decisions from AuditEvent or its detail map.
#[cfg(test)]
pub(crate) fn fixture_event(event: &lib::OperationEvent) -> remanence_state::AuditEvent {
    use remanence_state::AuditEvent as A;
    match event {
        lib::OperationEvent::Started(_) => A::OperationStarted,
        lib::OperationEvent::Finished(_) => A::OperationFinished,
        lib::OperationEvent::Failed(_) => A::OperationFailed,
        lib::OperationEvent::CancelledBeforeDispatch(_) => A::CancelledBeforeDispatch,
        lib::OperationEvent::CompletedAfterCancel(_) => A::CompletedAfterCancel,
        lib::OperationEvent::CompletionUnknown(_) => A::CompletionUnknown,
        lib::OperationEvent::Accepted(_) => A::RequestReceived,
        lib::OperationEvent::CancelRequested(_) => A::CancelRequested,
        lib::OperationEvent::CancellationRejected(_) => A::CancellationRejected,
    }
}
