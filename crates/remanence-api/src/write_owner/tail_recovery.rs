//! Explicit tail recovery shared by the mounted and library drive actors.
use remanence_library::{DriveHandle, DriveHandleSource};
use remanence_state::authority::TapeAuthorityView;
use remanence_state::{AuditActor, AuditEvent, SourceLayer};
use std::collections::BTreeMap;

/// Explicit recovery shared by standalone ownership and the library drive actor.
/// Confirmation, rescan, audit, and mutation all retain the same exclusive lease.
#[allow(clippy::too_many_arguments)]
pub(crate) fn supersede_mounted_tail(
    index: &mut remanence_state::CatalogIndex,
    cfg: &crate::write_owner::TerminalWriteConfig,
    drive: &mut DriveHandle,
    tape_uuid: [u8; 16],
    identity_generation: u64,
    acknowledge: &str,
    actor: AuditActor,
    confirm: impl FnOnce(&str, &[String]) -> Result<(), String>,
) -> Result<Vec<String>, tonic::Status> {
    use crate::append_admission::{
        admit_append_boundary, context, AdmissionError, AppendAuthority,
    };
    use tonic::Status;
    let failure = |e: String| Status::failed_precondition(e);
    let tape = index
        .get_tape(&tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| failure("adopt the tape before superseding its tail".into()))?;
    if tape.cartridge_generation.is_none() {
        return Err(failure("tape generation is required".into()));
    }
    if tape.terminal_finalization.is_some() {
        return Err(failure("terminal finalization prevents tail discard; retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent".into()));
    }
    if tape.kind != "data"
        || !matches!(tape.state.as_str(), "ready" | "recovery_required")
        || tape.terminal_finalization.is_some()
    {
        return Err(failure(
            "tape is sealed, retired, or requires terminal recovery".into(),
        ));
    }
    if !index
        .tape_io_admission_conflicts(&tape_uuid, tape.voltag.as_deref())
        .map_err(crate::status_from_state_error)?
        .is_empty()
    {
        return Err(failure(
            "release the tape I/O fence before superseding".into(),
        ));
    }
    let uuid = uuid::Uuid::from_bytes(tape_uuid).to_string();
    let label = tape.voltag.as_deref().unwrap_or(&uuid);
    if acknowledge != uuid && acknowledge != label {
        return Err(failure(
            "acknowledgment does not identify the loaded tape".into(),
        ));
    }
    let (size, parity) = crate::pool_write::selected_tape_geometry(
        &tape,
        tape.pool_id.as_deref().unwrap_or("(unpooled)"),
    )
    .map_err(|e| failure(e.to_string()))?;
    let journal =
        remanence_state::TapeAuthority::open(&cfg.checkpoint_journal_dir, tape_uuid, index.path())
            .map_err(crate::status_from_state_error)?;
    let terminal_guidance = "retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent";
    if journal
        .pending_finalization()
        .map_err(|e| {
            failure(format!(
                "cannot establish terminal authority: {e}; {terminal_guidance}"
            ))
        })?
        .is_some()
    {
        return Err(failure(format!(
            "terminal finalization intent prevents tail discard; {terminal_guidance}"
        )));
    }
    // The log's own seal refuses too: SQLite may not have projected it yet,
    // and pending_finalization reports nothing for a sealed log.
    let sealed = "tape log is sealed; supersede-tail cannot make a sealed tape writable";
    if journal
        .authority_summary()
        .map_err(crate::status_from_state_error)?
        .sealed
    {
        return Err(failure(sealed.into()));
    }
    let mut lease = journal
        .acquire_for_tail_reconciliation()
        .map_err(crate::status_from_state_error)?;
    lease
        .set_projection_floor(
            index
                .projected_through_sequence(tape_uuid)
                .map_err(crate::status_from_state_error)?,
        )
        .map_err(crate::status_from_state_error)?;
    let repaired_tail = lease
        .repair_pending()
        .map_err(crate::status_from_state_error)?;
    if lease
        .pending_finalization()
        .map_err(crate::status_from_state_error)?
        .is_some()
    {
        return Err(failure(
            "terminal finalization intent prevents tail discard; retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent".into(),
        ));
    }
    if lease
        .authority_summary()
        .map_err(crate::status_from_state_error)?
        .sealed
    {
        return Err(failure(sealed.into()));
    }
    let last = lease
        .latest_commit()
        .map_err(crate::status_from_state_error)?;
    let make_context = || {
        let mut ctx = context(
            tape_uuid,
            size,
            last.as_ref().map_or(0, |r| r.cursor.partition),
            last.as_ref().map_or(0, |r| r.cursor.lba),
        );
        ctx.inspect_all = true;
        ctx
    };
    let authority = || {
        last.as_ref()
            .map_or(AppendAuthority::Fresh, |_| AppendAuthority::Checkpoint {
                eod: make_context().boundary,
            })
    };
    drive
        .bind_identity_generation(identity_generation)
        .map_err(|e| failure(e.to_string()))?;
    drive.rewind().map_err(|e| failure(e.to_string()))?;
    crate::verify_tape_identity(&mut DriveHandleSource(drive), &tape_uuid)
        .map_err(|e| failure(e.to_string()))?;
    let config = drive.read_config().map_err(|e| failure(e.to_string()))?;
    crate::pool_write::require_rewritable_object_media(config)
        .map_err(|e| failure(e.to_string()))?;
    crate::drive_mode::configure_fixed_uncompressed_write(drive, config, size)
        .map_err(|e| failure(e.to_string()))?;
    let (found, object_ids, missing_filemark) =
        match admit_append_boundary(drive, authority(), make_context()) {
            Err(AdmissionError::TailBeyondKnownState {
                found, object_ids, ..
            }) => (found, object_ids, false),
            Err(AdmissionError::MissingBootstrapFilemark) => (Vec::new(), Vec::new(), true),
            Err(e) => return Err(failure(e.to_string())),
            Ok(_) if repaired_tail => (Vec::new(), Vec::new(), false),
            Ok(_) => return Err(failure("no foreign tail requires superseding".into())),
        };
    confirm(&format!("SUPERSEDE {label}"), &found).map_err(failure)?;
    let mut ctx = make_context();
    ctx.confirmed_discard = Some(found.clone());
    ctx.allow_missing_bootstrap_filemark = missing_filemark;
    let _admission =
        admit_append_boundary(drive, authority(), ctx).map_err(|e| failure(e.to_string()))?;
    drive
        .bind_identity_generation(identity_generation)
        .map_err(|e| failure(e.to_string()))?;
    crate::audit_projection::append_and_project_audit(
        index,
        &cfg.audit_dir,
        true,
        &cfg.audit_append_lock,
        crate::audit_projection::ProjectedAuditInput {
            actor: actor.clone(),
            source_layer: SourceLayer::Layer5,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event: AuditEvent::RecoveryEvent,
            subject_kind: "tape",
            subject_id: Some(uuid.clone()),
            detail: BTreeMap::from([
                (
                    "action".into(),
                    ciborium::Value::Text("supersede_tail".into()),
                ),
                (
                    "discarded_object_ids".into(),
                    ciborium::Value::Array(
                        object_ids.into_iter().map(ciborium::Value::Text).collect(),
                    ),
                ),
                (
                    "found".into(),
                    ciborium::Value::Array(
                        found.iter().cloned().map(ciborium::Value::Text).collect(),
                    ),
                ),
                (
                    "expected_eod_lba".into(),
                    ciborium::Value::Integer(make_context().boundary.lba.into()),
                ),
            ]),
        },
    )?;
    let result = (|| {
        // READ POSITION proves the retained boundary before the synchronous
        // recovery filemark advances it. Do not LOCATE merely to compare its
        // requested argument with itself.
        if repaired_tail {
            let expected = last
                .as_ref()
                .ok_or_else(|| "repaired tail has no retained commit".to_string())?
                .cursor;
            let observed = drive.position().map_err(|e| e.to_string())?;
            if observed.partition != expected.partition || observed.lba != expected.lba {
                return Err("physical repair cursor differs from retained authority".into());
            }
        }
        let mut sink = remanence_library::DriveHandleSink(drive);
        if last.is_none() {
            let bootstrap = crate::tape_init::build_tape_bootstrap(
                tape_uuid,
                size,
                parity.clone(),
                remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION"))
                    .written_at()
                    .map_err(|e| e.to_string())?,
                env!("CARGO_PKG_VERSION"),
            );
            crate::tape_init::write_tape_bootstrap(&mut sink, &bootstrap)
                .map_err(|e| e.to_string())?;
        } else {
            remanence_library::BlockSink::write_filemarks(&mut sink, 1)
                .map_err(|e| e.to_string())?;
        }
        if let remanence_parity::ParityConfig::Scheme(scheme) = &parity {
            let journal = lease
                .open_structural(size, scheme.clone())
                .map_err(|e| e.to_string())?;
            lease
                .retain_structural_publisher(journal)
                .map_err(|error| error.to_string())?;
            lease
                .with_structural(|lease, rows| {
                    lease.finish_structural_scope();
                    crate::append_admission::discard_reconciled_parity_suffix(
                        rows.ok_or_else(|| "missing retained structural capability".to_string())?,
                        lease,
                    )
                })
                .map_err(|error| error.to_string())??;
        }
        if repaired_tail {
            let expected = last
                .as_ref()
                .ok_or_else(|| "repaired tail has no retained commit".to_string())?
                .cursor;
            lease
                .reconcile_repaired_tail(expected)
                .map_err(|e| e.to_string())?;
        }
        Ok::<_, String>(())
    })();
    drive.clear_append_admission();
    if let Err(error) = result {
        crate::write_owner::raise_tape_io_fence(
            index,
            cfg,
            remanence_state::TapeIoFenceInput {
                tape_uuid,
                barcode: tape.voltag,
                reason: "supersede_tail_failed".into(),
                evidence_json: Some(serde_json::json!({"error":error}).to_string()),
            },
            drive
                .endpoint()
                .durable_key()
                .filter(|key| matches!(key, remanence_library::DriveKey::Standalone { .. })),
        )
        .map_err(crate::status_from_state_error)?;
        return Err(Status::unavailable(error));
    }
    index
        .set_append_recovery_required(tape_uuid, false)
        .map_err(|e| {
            Status::failed_precondition(format!(
                "tail discard completed, but clearing tape recovery state failed: {e}"
            ))
        })?;
    Ok(found)
}
