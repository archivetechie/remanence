//! Drive-owned maintenance closeout. Reservation and native authority precede
//! readiness, checked read-back, the whole suffix proof, and the one emitter.
use super::actor_runtime::WriteOwnerConfig;
use remanence_library::DriveHandle;
use remanence_state::authority::TapeAuthorityView;
use remanence_state::CatalogIndex;
use tonic::Status;

/// Host-reserved maintenance work carried intact across the changer move.
pub(super) struct CloseoutWork<'a> {
    pub tape_uuid: [u8; 16],
    pub expected_pool: Option<&'a str>,
    pub progress: &'a crate::closeout_progress::CloseoutProgress,
    pub permit: crate::io_memory::IoMemoryPermit,
}

pub(super) fn handle(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    work: CloseoutWork<'_>,
    ready: impl FnOnce(&mut CatalogIndex, &mut DriveHandle) -> Result<(), Status>,
) -> Result<(), Status> {
    let CloseoutWork {
        tape_uuid,
        expected_pool,
        progress,
        permit,
    } = work;
    let tape = index
        .get_tape(&tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    if expected_pool.is_some_and(|pool| tape.pool_id.as_deref() != Some(pool)) {
        return Err(Status::failed_precondition(
            "CloseoutTape expected_pool_id does not match the tape's assigned pool",
        ));
    }
    progress.phase("proving");
    let authority =
        remanence_state::TapeAuthority::open(&cfg.checkpoint_journal_dir, tape_uuid, index.path())
            .map_err(crate::status_from_state_error)?;
    let mut lease = authority
        .acquire_exclusive()
        .map_err(crate::status_from_state_error)?;
    let snapshot = lease.freeze().map_err(crate::status_from_state_error)?;
    progress.bind(snapshot.tape_summary(), "rebuilt");
    if !snapshot.tape_summary().closeout_due() {
        return Ok(());
    }
    super::read_session::session_open_reject_tape_io_fences(
        index,
        &tape_uuid,
        tape.voltag.as_deref(),
        "closeout tape",
    )?;
    let (block_size, parity_config) = crate::pool_write::selected_tape_geometry(
        &tape,
        tape.pool_id.as_deref().unwrap_or("(unpooled)"),
    )
    .map_err(|e| Status::failed_precondition(e.to_string()))?;
    let selected = crate::SelectedTape {
        standalone_endpoint: cfg.mounted().map(|m| m.endpoint.clone()),
        tape_uuid,
        block_size,
        parity_config,
        pool_id: tape.pool_id.unwrap_or_else(|| "(unpooled)".into()),
    };
    let previous = lease
        .latest_commit()
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::internal("closeout lacks commit"))?;
    let authority = super::checkpoint::validate_parity_actor_authority(&selected, &mut lease)?;
    ready(index, drive)?;
    super::write_session::prepare_drive_for_write(
        drive,
        &tape_uuid,
        block_size,
        uuid::Uuid::new_v4(),
        super::write_session::WriteMediaPolicy::RewritableObject,
    )?;
    progress.phase("rebuilding");
    let session = super::checkpoint::open_parity_actor_session(
        index,
        drive,
        cfg,
        &selected,
        &[],
        authority,
        &mut lease,
        Some(permit),
    )?;
    let prepared = super::checkpoint::prepare_closeout(session, &previous)?;
    let ctx = crate::append_admission::context(
        tape_uuid,
        block_size,
        previous.cursor.partition,
        previous.cursor.lba,
    );
    let mut admission = crate::append_admission::admit_append_boundary(
        drive,
        crate::append_admission::AppendAuthority::Session {
            snapshot: &snapshot,
            closeout: Some(&prepared.encoded),
        },
        ctx,
    )
    .map_err(|error| {
        crate::append_admission::daemon_admission_error(
            remanence_state::AuditActor::System,
            index,
            &super::terminal_types::TerminalConfig::terminal_config(cfg),
            tape_uuid,
            error,
        )
    })?;
    if let Some(open) = &snapshot.tape_summary().open_session {
        lease
            .end_session(
                open.session_id,
                super::restore::now_rfc3339().map_err(|e| Status::internal(e.to_string()))?,
                remanence_state::tape_log::SessionEndReason::CrashRecovered,
                None,
                admission
                    .recovered_tail
                    .take()
                    .ok_or_else(|| Status::internal("missing recovered tail"))?
                    .files()
                    .map_err(|e| Status::internal(e.to_string()))?,
            )
            .map_err(|error| {
                let status = crate::status_from_state_error(error);
                super::checkpoint::fence_ambiguous_authority(
                    index,
                    cfg,
                    &lease,
                    tape_uuid,
                    "recover_session",
                    &status,
                );
                status
            })?;
    }
    super::checkpoint::write_closeout(
        index, drive, cfg, &selected, &mut lease, prepared, progress,
    )?;
    drive.clear_append_admission();
    Ok(())
}
