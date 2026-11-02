//! Physical append admission shared by object writers, terminal writers and
//! journal import. Local state supplies a boundary; only tape evidence can
//! authorize superseding an incomplete suffix at that boundary.
use crate::catalogless_restore::{scan_append_suffix_observed, AppendSuffix};
mod session_suffix;
use remanence_library::{AppendSession, DriveHandle, DriveHandleSource, TapeIoError, TapePosition};
use remanence_parity::TerminalTripleWritePlan;
use remanence_state::authority::TapeAuthorityView;

/// Physical append proof owned by one write session. Only the funnel below can
/// construct it; dropping it revokes the handle's adapters.
#[derive(Debug)]
#[must_use = "hold the admission for the entire write session"]
pub struct AppendAdmission {
    session: AppendSession,
    pub(crate) recovered_tail: Option<crate::write_owner::session_tail::SessionTail>,
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
    Checkpoint {
        eod: TapePosition,
    },
    Terminal {
        plan: &'a TerminalTripleWritePlan,
    },
    Session {
        snapshot: &'a remanence_state::tape_log::authority::AuthoritySnapshot,
        closeout: Option<&'a remanence_parity::EncodedSidecarTapeFile>,
    },
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
    #[error("SessionSuffixContradiction: tape {tape_uuid} suffix at {partition}:{lba} does not match tape-log sequence {sequence}; writes are fenced.")]
    SessionSuffixContradiction {
        tape_uuid: uuid::Uuid,
        partition: u32,
        lba: u64,
        sequence: u64,
        incarnation: u64,
        detail: String,
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
    let recovered_tail = match &authority {
        AppendAuthority::Session { snapshot, .. }
            if snapshot.tape_summary().open_session.is_some() =>
        {
            Some(
                crate::write_owner::session_tail::SessionTail::new(
                    ctx.tape_uuid,
                    ctx.block_size,
                    snapshot.tape_summary().next_file,
                )
                .map_err(|e| AdmissionError::Unproved(e.to_string()))?,
            )
        }
        _ => None,
    };
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
                prove_position(drive, *eod, ctx.block_size, generation)?;
                *eod
            }
            AppendAuthority::Terminal { .. } | AppendAuthority::Session { .. } => {
                prove_position(drive, ctx.boundary, ctx.block_size, generation)?;
                ctx.boundary
            }
        };
        if let AppendAuthority::Session { snapshot, closeout } = &authority {
            let summary = snapshot.tape_summary();
            // Deterministic own-closeout prefix has priority over the old named
            // tail. Every attempted proof restores C before trying another row.
            if let Some(encoded) = closeout {
                match session_suffix::own_closeout(drive, &encoded.blocks, ctx.block_size) {
                    Ok((count, mark, digest)) if count > 0 => {
                        if let Some(tail) = &recovered_tail {
                            use remanence_state::tape_log::{TailFilemark, TailIdentity};
                            let identity = if mark {
                                TailIdentity::Sidecar(encoded.header.canonical_metadata_hash)
                            } else {
                                TailIdentity::TornPrefix(digest)
                            };
                            tail.observe(
                                identity,
                                count,
                                if mark {
                                    TailFilemark::Present
                                } else {
                                    TailFilemark::Absent
                                },
                            )
                            .map_err(|e| AdmissionError::Unproved(e.to_string()))?;
                        }
                        return Ok(AppendSuffix::default());
                    }
                    Ok(_) | Err(AdmissionError::Unproved(_)) => {
                        prove_position(drive, boundary, ctx.block_size, generation)?;
                    }
                    Err(e) => return Err(e),
                }
            }
            if summary.open_session.is_none() {
                let mut failure = None;
                snapshot
                    .visit_session_tail(&mut |file| {
                        session_suffix::named_file(drive, &ctx.tape_uuid, ctx.block_size, file)
                            .map_err(|e| {
                                failure = Some(e);
                                remanence_state::StateError::JournalReplayFailed(
                                    "session suffix proof failed".into(),
                                )
                            })
                    })
                    .map_err(|e| {
                        failure
                            .take()
                            .unwrap_or_else(|| AdmissionError::Unproved(e.to_string()))
                    })?;
                session_suffix::eod(drive, ctx.block_size)?;
                return Ok(AppendSuffix::default());
            }
        }
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
        prove_position(drive, boundary, ctx.block_size, generation)?;
        let plan = match &authority {
            AppendAuthority::Terminal { plan } => Some(*plan),
            _ => None,
        };
        let suffix = scan_append_suffix_observed(
            drive,
            &ctx.tape_uuid,
            ctx.block_size,
            boundary.lba,
            plan,
            ctx.terminal_prefix.as_ref(),
            ctx.inspect_all || recovered_tail.is_some(),
            recovered_tail.as_ref(),
        )
        .map_err(|error| match error {
            AdmissionError::Io(source) => AdmissionError::SuffixIo {
                start_lba: boundary.lba,
                source,
            },
            other => other,
        })?;
        if recovered_tail.is_some() && suffix.unnameable {
            return Err(AdmissionError::Unproved(
                "crash suffix contains a delimited file without a session-tail identity; restore consistent tape state".into(),
            ));
        }
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
    let probe = probe.map_err(|error| match (&authority, error) {
        (AppendAuthority::Session { snapshot, .. }, AdmissionError::Unproved(detail))
            if detail.starts_with("SessionSuffixContradiction:") =>
        {
            AdmissionError::SessionSuffixContradiction {
                tape_uuid: uuid::Uuid::from_bytes(ctx.tape_uuid),
                partition: ctx.boundary.partition,
                lba: ctx.boundary.lba,
                sequence: snapshot.log_reference().0,
                incarnation: snapshot.tape_summary().incarnation,
                detail,
            }
        }
        (_, error) => error,
    });
    let restored = if matches!(authority, AppendAuthority::Fresh) && ctx.write_position.lba == 0 {
        drive
            .locate(0)
            .and_then(|actual| {
                if actual.partition != ctx.write_position.partition || actual.lba != 0 {
                    return Err(TapeIoError::OperationFailed(format!(
                        "position mismatch: expected {:?}, got {actual:?}",
                        ctx.write_position
                    )));
                }
                Ok(None)
            })
            .map_err(AdmissionError::Io)
    } else {
        prove_position(drive, ctx.write_position, ctx.block_size, generation).map(Some)
    };
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
    let cursor_proof = restored?;
    probe?;
    let (position, generation) = cursor_proof.map_or(
        (ctx.write_position, generation),
        remanence_library::cursor::CursorProof::into_position,
    );
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
            position.partition,
            position.lba,
            generation,
        ))
        .map(|session| AppendAdmission {
            session,
            recovered_tail,
        })
        .map_err(AdmissionError::Io)
}

fn prove_position(
    drive: &mut DriveHandle,
    expected: TapePosition,
    block_size: u32,
    generation: u64,
) -> Result<remanence_library::cursor::CursorProof, AdmissionError> {
    drive
        .prove_cursor(expected, block_size, generation)
        .map_err(AdmissionError::Io)
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
    if let AdmissionError::SessionSuffixContradiction {
        detail,
        partition,
        lba,
        sequence,
        incarnation,
        ..
    } = &error
    {
        crate::write_owner::record_closeout_fence(
            index,
            cfg,
            tape_uuid,
            "SessionSuffixContradiction",
            serde_json::json!({"error": error.to_string(), "detail": detail, "partition":partition, "lba":lba, "tape_log_sequence":sequence, "tape_incarnation":incarnation}).to_string(),
        );
        return crate::closeout_obligation::RetryCause::Operator
            .attach(tonic::Status::failed_precondition(error.to_string()));
    }
    let detail = error.to_string();
    if let AdmissionError::Io(cause) | AdmissionError::SuffixIo { source: cause, .. } = &error {
        return crate::closeout_obligation::RetryCause::tape_io(cause)
            .attach(tonic::Status::unavailable(detail));
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
        Ok(_) => crate::closeout_obligation::RetryCause::Operator
            .attach(tonic::Status::failed_precondition(detail)),
        Err(e) => e,
    }
}
