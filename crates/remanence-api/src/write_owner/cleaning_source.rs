//! Explicit cleaning producer actions use the shared library writer. Legacy SQL
//! mutation exists only for historical unit fixtures without typed capability.
use super::actor_runtime::WriteOwnerConfig;
use crate::status_from_state_error;
use remanence_state::index::CleanRunRecord;
use remanence_state::{library_log as lib, CatalogIndex, TapeRecord};
use tonic::Status;

fn failure(error: impl std::fmt::Display) -> Status {
    Status::failed_precondition(error.to_string())
}
fn id(bytes: &[u8]) -> Result<lib::Id, Status> {
    bytes
        .try_into()
        .map_err(|_| Status::internal("cleaning identity is not 16 bytes"))
}
fn current(index: &CatalogIndex, run: &str) -> Result<CleanRunRecord, Status> {
    index
        .get_clean_run(run)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::internal("cleaning source has no derived run"))
}

pub(super) fn register(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    barcode: &str,
    home: u16,
) -> Result<TapeRecord, Status> {
    if let Some(runtime) = cfg.authority.runtime() {
        let tape = runtime
            .register_cleaning_cartridge(barcode, home)
            .map_err(failure)?;
        return index
            .get_tape(&tape.uuid)
            .map_err(status_from_state_error)?
            .ok_or_else(|| Status::internal("cleaning source has no derived cartridge"));
    }
    #[cfg(test)]
    {
        index
            .ensure_cleaning_cartridge(barcode)
            .map_err(status_from_state_error)
    }
    #[cfg(not(test))]
    Err(Status::failed_precondition(
        "cleaning requires shared authority",
    ))
}

pub(super) fn begin(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &[u8],
    library: &str,
    trigger: &str,
) -> Result<CleanRunRecord, Status> {
    if let Some(runtime) = cfg.authority.runtime() {
        let trigger = match trigger {
            "manual" => lib::CleanTrigger::Manual,
            "periodic" => lib::CleanTrigger::Periodic,
            "now" => lib::CleanTrigger::Now,
            _ => return Err(Status::invalid_argument("unknown cleaning trigger")),
        };
        let run = uuid::Uuid::new_v4().to_string();
        runtime
            .record_cleaning_action(
                &run,
                Some(id(drive)?),
                None,
                lib::CleaningAction::Begin(lib::Begin { trigger }),
                lib::AuditActor::System,
            )
            .map_err(failure)?;
        return current(index, &run);
    }
    #[cfg(test)]
    {
        index
            .begin_clean_run(drive, library, trigger, None)
            .map_err(status_from_state_error)
    }
    #[cfg(not(test))]
    {
        let _ = library;
        Err(Status::failed_precondition(
            "cleaning requires shared authority",
        ))
    }
}

pub(super) fn select(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    run: &str,
    cart: &[u8],
    slot: u16,
) -> Result<CleanRunRecord, Status> {
    if let Some(runtime) = cfg.authority.runtime() {
        let cartridge = runtime.cleaning_cartridge(id(cart)?).map_err(failure)?;
        runtime
            .record_cleaning_action(
                run,
                None,
                Some(cartridge),
                lib::CleaningAction::Advance(lib::Advance {
                    phase: lib::CleanPhase::MovingIn,
                    home_slot: Some(slot),
                }),
                lib::AuditActor::System,
            )
            .map_err(failure)?;
        return current(index, run);
    }
    #[cfg(test)]
    {
        index
            .select_clean_run_cart(
                run,
                cart,
                i64::from(slot),
                Some("{\"phase\":\"selecting\"}"),
            )
            .map_err(status_from_state_error)?;
        current(index, run)
    }
    #[cfg(not(test))]
    Err(Status::failed_precondition(
        "cleaning requires shared authority",
    ))
}

pub(super) fn advance(
    _index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    run: &str,
    phase: lib::CleanPhase,
    detail: Option<&str>,
) -> Result<(), Status> {
    if let Some(runtime) = cfg.authority.runtime() {
        let action = match phase {
            lib::CleanPhase::Done | lib::CleanPhase::Failed | lib::CleanPhase::NeedsOperator => {
                lib::CleaningAction::Finish(lib::Finish {
                    result: match phase {
                        lib::CleanPhase::Done => lib::CleanResult::Done,
                        lib::CleanPhase::Failed => lib::CleanResult::Failed,
                        _ => lib::CleanResult::NeedsOperator,
                    },
                    verified_credit: false,
                })
            }
            lib::CleanPhase::MovingIn => {
                return Err(Status::internal(
                    "MovingIn requires its real selected home slot",
                ))
            }
            _ => lib::CleaningAction::Advance(lib::Advance {
                phase,
                home_slot: None,
            }),
        };
        runtime
            .record_cleaning_action(run, None, None, action, lib::AuditActor::System)
            .map_err(failure)?;
        return Ok(());
    }
    #[cfg(test)]
    {
        let phase = match phase {
            lib::CleanPhase::Fencing => "fencing",
            lib::CleanPhase::Selecting => "selecting",
            lib::CleanPhase::MovingIn => "moving-in",
            lib::CleanPhase::Cleaning => "cleaning",
            lib::CleanPhase::MovingBack => "moving-back",
            lib::CleanPhase::Verifying => "verifying",
            lib::CleanPhase::Done => "done",
            lib::CleanPhase::Failed => "failed",
            lib::CleanPhase::NeedsOperator => "needs-operator",
        };
        _index
            .advance_clean_run(run, phase, detail)
            .map_err(status_from_state_error)?;
        Ok(())
    }
    #[cfg(not(test))]
    {
        let _ = detail;
        Err(Status::failed_precondition(
            "cleaning requires shared authority",
        ))
    }
}

pub(super) fn finish_verified(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    run: &str,
    drive: &[u8],
    cart: &[u8],
    detail: &str,
) -> Result<(), Status> {
    if let Some(runtime) = cfg.authority.runtime() {
        runtime
            .record_cleaning_action(
                run,
                None,
                None,
                lib::CleaningAction::Finish(lib::Finish {
                    result: lib::CleanResult::Done,
                    verified_credit: true,
                }),
                lib::AuditActor::System,
            )
            .map_err(failure)?;
        return Ok(());
    }
    #[cfg(test)]
    {
        index
            .finalize_verified_clean_run(run, drive, Some(cart), Some(detail))
            .map_err(status_from_state_error)?;
        Ok(())
    }
    #[cfg(not(test))]
    {
        let _ = (index, drive, cart, detail);
        Err(Status::failed_precondition(
            "cleaning requires shared authority",
        ))
    }
}

pub(super) fn cartridge_failed(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    run: &str,
    cart: &[u8],
    expired: bool,
    reason: &str,
) -> Result<(), Status> {
    if let Some(runtime) = cfg.authority.runtime() {
        let cartridge = runtime.cleaning_cartridge(id(cart)?).map_err(failure)?;
        let reason = lib::Reason {
            reason: reason.into(),
        };
        let action = if expired {
            lib::CleaningAction::Expire(reason)
        } else {
            lib::CleaningAction::Reject(reason)
        };
        runtime
            .record_cleaning_action(run, None, Some(cartridge), action, lib::AuditActor::System)
            .map_err(failure)?;
        return Ok(());
    }
    #[cfg(test)]
    {
        index
            .set_tape_cleaning_state(cart, if expired { "expired" } else { "rejected" })
            .map_err(status_from_state_error)?;
        Ok(())
    }
    #[cfg(not(test))]
    {
        let _ = index;
        Err(Status::failed_precondition(
            "cleaning requires shared authority",
        ))
    }
}

/// The returned source sequence belongs to this run's fence only. Releasing it
/// never clears another run's or subsystem's independent drive obligation.
pub(super) fn fence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    drive: &[u8],
    library: &str,
    bay: u16,
) -> Result<Option<u64>, Status> {
    if let Some(runtime) = cfg.authority.runtime() {
        let value = lib::FenceRaised {
            detail: lib::Bounded(vec![]),
            kind: lib::FenceKind::DriveFault,
            scope: lib::FenceScope::Drive(lib::DriveKey::Bay(lib::Bay {
                library_serial: library.into(),
                bay,
            })),
            cause: lib::FenceCause::Cleaning,
            evidence: None,
            at: None,
            operation: Some(*handle.op_id_uuid().as_bytes()),
        };
        let receipt = runtime
            .append(|a, v| {
                a.fence_raised(value, lib::AuditActor::System, lib::SourceLayer::Layer4, v)
            })
            .map_err(failure)?;
        return Ok(Some(receipt.sequence));
    }
    #[cfg(test)]
    {
        index
            .set_drive_fenced(drive, true)
            .map_err(status_from_state_error)?;
        Ok(None)
    }
    #[cfg(not(test))]
    {
        let _ = (index, drive);
        Err(Status::failed_precondition(
            "cleaning requires shared authority",
        ))
    }
}

pub(super) fn release(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &[u8],
    fence: Option<u64>,
    reason: &str,
) -> Result<(), Status> {
    if let Some(runtime) = cfg.authority.runtime() {
        let fence = fence
            .ok_or_else(|| Status::internal("cleaning release has no owned fence sequence"))?;
        runtime
            .append(|a, v| {
                a.fence_released(
                    lib::FenceReleased {
                        detail: lib::Bounded(vec![]),
                        fence,
                        evidence: reason.into(),
                        forced: false,
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .map_err(failure)?;
        return Ok(());
    }
    #[cfg(test)]
    {
        index
            .set_drive_fenced(drive, false)
            .map_err(status_from_state_error)?;
        Ok(())
    }
    #[cfg(not(test))]
    {
        let _ = (index, drive);
        Err(Status::failed_precondition(
            "cleaning requires shared authority",
        ))
    }
}
