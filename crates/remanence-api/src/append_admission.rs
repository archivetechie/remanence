//! Physical append admission shared by object writers, terminal writers and
//! journal import. Local state supplies a boundary; only tape evidence can
//! authorize superseding an incomplete suffix at that boundary.
use crate::catalogless_restore::{scan_append_suffix, AppendSuffix};
use remanence_library::{AppendSession, DriveHandle, DriveHandleSource, TapeIoError, TapePosition};
use remanence_parity::TerminalTripleWritePlan;
use remanence_state::authority::TapeAuthorityView;

/// Physical append proof owned by one write session. Only the funnel below can
/// construct it; dropping it revokes the handle's adapters.
#[derive(Debug)]
#[must_use = "hold the admission for the entire write session"]
pub struct AppendAdmission {
    session: AppendSession,
}
impl AppendAdmission {
    /// The identity, partition, position and load generation proved by the funnel.
    pub fn boundary(&self) -> ([u8; 16], u32, u64, u64) {
        self.session.boundary()
    }
}

/// Durable authority whose physical boundary must be admitted.
pub(crate) enum AppendAuthority<'a> {
    Fresh,
    Checkpoint { eod: TapePosition },
    Terminal { plan: &'a TerminalTripleWritePlan },
}

/// Identity, geometry and terminal positioning supplied by the session owner.
pub(crate) struct AppendContext {
    pub tape_uuid: [u8; 16],
    pub block_size: u32,
    pub boundary: TapePosition,
    pub write_position: TapePosition,
    pub confirmed_discard: Option<Vec<String>>,
    pub allow_missing_bootstrap_filemark: bool,
    pub inspect_all: bool,
    pub terminal_prefix: Option<remanence_parity::TerminalPrefixPlan>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AdmissionError {
    #[error("TailBeyondKnownState: tape {tape_uuid}, expected EOD LBA {expected_eod_lba}, found {found:?}. Rescue the data with `rem tape restore --scan`, or restore a newer copy of the state directory. To deliberately discard this tail, use `rem tape supersede-tail --tape-uuid <uuid> --ack-tape-uuid <uuid>` (library), or `rem tape supersede-tail --acknowledge-tape <label|uuid>` (standalone home mode; for an explicit device add `--device <path> --allow-device <serial>`).")]
    TailBeyondKnownState {
        tape_uuid: uuid::Uuid,
        expected_eod_lba: u64,
        found: Vec<String>,
        object_ids: Vec<String>,
    },
    #[error("bootstrap is not followed by a filemark")]
    MissingBootstrapFilemark,
    #[error("physical append admission refused: {0}")]
    Unproved(String),
    #[error(transparent)]
    Io(#[from] TapeIoError),
    #[error("append suffix at LBA {start_lba}: {source}")]
    SuffixIo { start_lba: u64, source: TapeIoError },
}

impl From<String> for AdmissionError {
    fn from(detail: String) -> Self {
        Self::Unproved(detail)
    }
}
impl From<&str> for AdmissionError {
    fn from(detail: &str) -> Self {
        Self::Unproved(detail.into())
    }
}

/// The sole physical-tail admission funnel. Always restores and re-proves the
/// write position, including after a refused probe. Identity invalidation is
/// fatal even if subsequent positioning succeeds.
pub(crate) fn admit_append_boundary(
    drive: &mut DriveHandle,
    authority: AppendAuthority<'_>,
    ctx: AppendContext,
) -> Result<AppendAdmission, AdmissionError> {
    drive.clear_append_admission();
    if ctx.block_size == 0 || ctx.block_size > remanence_scsi::read_write::MAX_TRANSFER_LEN {
        return Err(AdmissionError::Unproved("invalid append block size".into()));
    }

    let generation = drive.identity_generation();
    let probe = (|| {
        let boundary = match &authority {
            AppendAuthority::Fresh => {
                drive.rewind()?;
                crate::verify_tape_identity(&mut DriveHandleSource(drive), &ctx.tape_uuid)
                    .map_err(|e| match e {
                        crate::pool_write::TapeIdentityError::Io(e) => AdmissionError::Io(e),
                        e => AdmissionError::Unproved(e.to_string()),
                    })?;
                let mut block = vec![0; ctx.block_size as usize];
                match drive.read_block(&mut block) {
                    Err(TapeIoError::FilemarkEncountered) => {}
                    Err(e) if e.is_physical_eod() => {
                        if ctx.allow_missing_bootstrap_filemark {
                            return Ok(AppendSuffix::default());
                        }
                        return Err(AdmissionError::MissingBootstrapFilemark);
                    }
                    Err(e) => return Err(AdmissionError::Io(e)),
                    Ok(_) => {
                        return Err(AdmissionError::Unproved(
                            "data where bootstrap filemark was expected".into(),
                        ))
                    }
                }
                let position = drive.position()?;
                if position.partition != 0 {
                    return Err(AdmissionError::Unproved(
                        "bootstrap filemark position mismatch".into(),
                    ));
                }
                position
            }
            AppendAuthority::Checkpoint { eod } => {
                prove_position(drive, *eod)?;
                *eod
            }
            AppendAuthority::Terminal { .. } => {
                prove_position(drive, ctx.boundary)?;
                ctx.boundary
            }
        };
        let mut block = vec![0; ctx.block_size as usize];
        match drive.read_block(&mut block) {
            Err(e) if e.is_physical_eod() => return Ok(AppendSuffix::default()),
            Ok(_) | Err(TapeIoError::FilemarkEncountered) => {}
            Err(source) => {
                return Err(AdmissionError::SuffixIo {
                    start_lba: boundary.lba,
                    source,
                })
            }
        };
        prove_position(drive, boundary)?;
        let plan = match &authority {
            AppendAuthority::Terminal { plan } => Some(*plan),
            _ => None,
        };
        let suffix = scan_append_suffix(
            drive,
            &ctx.tape_uuid,
            ctx.block_size,
            boundary.lba,
            plan,
            ctx.terminal_prefix.as_ref(),
            ctx.inspect_all,
        )
        .map_err(|error| match error {
            AdmissionError::Io(source) => AdmissionError::SuffixIo {
                start_lba: boundary.lba,
                source,
            },
            other => other,
        })?;
        let mut found = suffix.objects.clone();
        found.extend(suffix.foreign_components.clone());
        if ctx
            .confirmed_discard
            .as_ref()
            .is_some_and(|confirmed| confirmed != &found)
            || (!found.is_empty() && ctx.confirmed_discard.is_none())
        {
            return Err(AdmissionError::TailBeyondKnownState {
                tape_uuid: uuid::Uuid::from_bytes(ctx.tape_uuid),
                expected_eod_lba: boundary.lba,
                found,
                object_ids: suffix.objects.clone(),
            });
        }
        Ok(suffix)
    })();
    let restored = prove_position(drive, ctx.write_position);
    // A suffix read failure refuses admission even when it also invalidates the
    // handle. Preserve its location after attempting cursor restoration.
    if let Err(error @ AdmissionError::SuffixIo { .. }) = probe {
        return Err(error);
    }
    if drive.identity_generation() != generation {
        return Err(AdmissionError::Io(TapeIoError::IdentityInvalidated {
            proved: generation,
            current: drive.identity_generation(),
        }));
    }
    restored?;
    probe?;
    // SAFETY: this private, non-cloneable type can only be constructed here,
    // after successful classification, cursor restoration and generation proof.
    // Installation immediately consumes it on the same exclusively held drive.
    struct ProvedBoundary([u8; 16], u32, u64, u64);
    unsafe impl remanence_library::AppendBoundaryProof for ProvedBoundary {
        fn into_boundary(self) -> ([u8; 16], u32, u64, u64) {
            (self.0, self.1, self.2, self.3)
        }
    }
    drive
        .install_append_admission(ProvedBoundary(
            ctx.tape_uuid,
            ctx.write_position.partition,
            ctx.write_position.lba,
            generation,
        ))
        .map(|session| AppendAdmission { session })
        .map_err(AdmissionError::Io)
}

fn prove_position(drive: &mut DriveHandle, expected: TapePosition) -> Result<(), AdmissionError> {
    let actual = drive.locate(expected.lba)?;
    if actual.partition != expected.partition || actual.lba != expected.lba {
        return Err(AdmissionError::Unproved(format!(
            "position mismatch: expected {expected:?}, got {actual:?}"
        )));
    }
    Ok(())
}

pub(crate) fn context(
    tape_uuid: [u8; 16],
    block_size: u32,
    partition: u32,
    lba: u64,
) -> AppendContext {
    let boundary = TapePosition {
        partition,
        lba,
        beginning_of_partition: lba == 0,
        end_of_partition: false,
        block_position_end_of_warning: false,
    };
    AppendContext {
        tape_uuid,
        block_size,
        boundary,
        write_position: boundary,
        confirmed_discard: None,
        allow_missing_bootstrap_filemark: false,
        inspect_all: false,
        terminal_prefix: None,
    }
}

/// Admit the first object session after an explicit bootstrap initialization.
/// The physical bootstrap, its filemark and exact EOD must still be present.
pub fn admit_initialized_tape(
    drive: &mut DriveHandle,
    tape_uuid: [u8; 16],
    block_size: u32,
) -> Result<AppendAdmission, String> {
    admit_append_boundary(
        drive,
        AppendAuthority::Fresh,
        context(tape_uuid, block_size, 0, 0),
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;

/// Admit a durable checkpoint boundary for low-level recovery tools.
pub fn admit_checkpointed_tape(
    drive: &mut DriveHandle,
    tape_uuid: [u8; 16],
    block_size: u32,
    partition: u32,
    lba: u64,
) -> Result<AppendAdmission, String> {
    let ctx = context(tape_uuid, block_size, partition, lba);
    admit_append_boundary(
        drive,
        AppendAuthority::Checkpoint { eod: ctx.boundary },
        ctx,
    )
    .map_err(|e| e.to_string())
}

/// Abandon only this live session's suffix while its exclusive checkpoint lease
/// and admission are still held. The adapter rechecks the original generation.
pub(crate) fn abandon_pending_append(
    drive: &mut DriveHandle,
    lease: &mut remanence_state::TapeAuthorityLease,
) -> Result<(), String> {
    lease
        .with_structural(|lease, rows| abandon_pending_append_with_rows(drive, lease, rows))
        .map_err(|error| error.to_string())?
}

fn abandon_pending_append_with_rows(
    drive: &mut DriveHandle,
    lease: &remanence_state::TapeAuthorityLease,
    journal: Option<&mut dyn remanence_parity::authority::StructuralAuthority>,
) -> Result<(), String> {
    if !drive
        .append_session_has_dispatched_writes()
        .map_err(|e| e.to_string())?
    {
        return Ok(());
    }
    let tape_uuid = drive.admitted_tape_uuid().map_err(|e| e.to_string())?;
    let lease_uuid = lease.tape_uuid();
    if tape_uuid != lease_uuid {
        return Err("abandonment lease belongs to a different tape".into());
    }
    if lease
        .pending_finalization()
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err("terminal intent must be recovered, not abandoned".into());
    }
    let last = lease.latest_commit().map_err(|e| e.to_string())?;
    // A fresh session has already emitted bootstrap + its delimiter. Preserve
    // both; the marker replaces only its first uncheckpointed object.
    let lba = last.as_ref().map_or(2, |r| r.cursor.lba);
    let partition = last.as_ref().map_or(0, |r| r.cursor.partition);
    let actual = drive.locate(lba).map_err(|e| e.to_string())?;
    if actual.partition != partition || actual.lba != lba {
        return Err("abandonment checkpoint position mismatch".into());
    }
    remanence_library::BlockSink::write_filemarks(
        &mut remanence_library::DriveHandleSink(drive),
        1,
    )
    .map_err(|e| e.to_string())?;
    if let Some(journal) = journal {
        discard_reconciled_parity_suffix(journal, lease)?;
    }
    Ok(())
}

/// The caller has physically discarded the suffix under its checkpoint lease.
/// Keep exactly that authority, including when the parity barrier was published
/// just before a failed append to the shared checkpoint journal.
pub(crate) fn discard_reconciled_parity_suffix(
    journal: &mut dyn remanence_parity::authority::StructuralAuthority,
    lease: &remanence_state::TapeAuthorityLease,
) -> Result<(), String> {
    let last = lease.latest_commit().map_err(|e| e.to_string())?;
    journal
        .discard_suffix(last.as_ref().map_or(1, |r| r.next_file))
        .map_err(|e| e.to_string())
}

/// A stale physical tail disables this tape alone. Probe failures without writes
/// remain retryable and do not create durable quarantine or recovery state.
pub(crate) fn daemon_admission_error(
    actor: remanence_state::AuditActor,
    index: &mut remanence_state::CatalogIndex,
    cfg: &crate::write_owner::TerminalWriteConfig,
    tape_uuid: [u8; 16],
    error: AdmissionError,
) -> tonic::Status {
    let detail = error.to_string();
    if matches!(
        error,
        AdmissionError::Io(_) | AdmissionError::SuffixIo { .. }
    ) {
        return tonic::Status::unavailable(detail);
    }
    if let Err(e) = index.set_append_recovery_required(tape_uuid, true) {
        return crate::status_from_state_error(e);
    }
    let audit = crate::audit_projection::append_and_project_audit(
        index,
        &cfg.audit_dir,
        true,
        &cfg.audit_append_lock,
        crate::audit_projection::ProjectedAuditInput {
            actor,
            source_layer: remanence_state::SourceLayer::Layer5,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event: remanence_state::AuditEvent::RecoveryEvent,
            subject_kind: "tape",
            subject_id: Some(uuid::Uuid::from_bytes(tape_uuid).to_string()),
            detail: std::collections::BTreeMap::from([
                (
                    "action".into(),
                    ciborium::Value::Text("append_recovery_required".into()),
                ),
                ("error".into(), ciborium::Value::Text(detail.clone())),
            ]),
        },
    );
    match audit {
        Ok(_) => tonic::Status::failed_precondition(detail),
        Err(e) => e,
    }
}
