//! Physical append admission shared by object writers, terminal writers and
//! journal import. Local state supplies a boundary; only tape evidence can
//! authorize superseding an incomplete suffix at that boundary.
use crate::catalogless_restore::{scan_append_suffix_observed, AppendSuffix};
use crate::closeout_obligation::RetryCauseExt as _;
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
    /// Proved ended-tail EOD; publish through the existing native ending before reuse.
    pub(crate) ended_tail_eod: Option<u64>,
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

/// Admission failures retain the failed probe even after restoration issues other commands.
pub(crate) struct AdmissionError {
    pub(crate) kind: AdmissionErrorKind,
    outcome: Option<Box<remanence_library::outcome::ClassifiedOutcome>>,
}
impl std::fmt::Debug for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.kind, f)
    }
}
impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind.fmt(f)
    }
}
impl std::error::Error for AdmissionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        std::error::Error::source(&self.kind)
    }
}
impl AdmissionError {
    /// Keep a private intermediate suffix fact pending until admission supplies its authority scope.
    pub(super) fn pending(kind: AdmissionErrorKind) -> Self {
        Self {
            kind,
            outcome: None,
        }
    }
    /// Finish the final public admission error, never an intermediate diagnostic.
    fn finish(&mut self) {
        if self.outcome.is_none() {
            self.outcome = Some(Box::new(finish_admission_refusal(&self.kind)));
        }
    }
    pub(crate) fn new(kind: AdmissionErrorKind) -> Self {
        let outcome = match &kind {
            AdmissionErrorKind::Io(error) | AdmissionErrorKind::SuffixIo { source: error, .. } => {
                error.outcome().clone()
            }
            _ => finish_admission_refusal(&kind),
        };
        Self {
            kind,
            outcome: Some(Box::new(outcome)),
        }
    }
}
impl From<TapeIoError> for AdmissionError {
    fn from(error: TapeIoError) -> Self {
        Self::new(AdmissionErrorKind::Io(error))
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AdmissionErrorKind {
    #[error("TailBeyondKnownState: tape {tape_uuid}, expected EOD LBA {expected_eod_lba}, found {}. The tape holds data this state does not record, often after restoring an older copy of the state directory. It stays readable and closed for writing. To keep the data, start from a newer copy of the state directory. Unless this state shows the tape's last session closed, `rem tape supersede-tail --tape-uuid <uuid> --ack-tape-uuid <uuid>` (library) or `rem tape supersede-tail --acknowledge-tape <label|uuid>` (standalone home mode; for an explicit device add `--device <path> --allow-device <serial>`) discards it; a drive outside a library can first copy its files out with `rem tape restore --scan`.", capped_found(.found))]
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

/// Bound the diagnostic carried in RPC headers while retaining all scan results in the error.
fn capped_found(found: &[String]) -> String {
    let shown = found.len().min(8);
    let mut text = format!("{:?}", &found[..shown]);
    if found.len() > shown {
        text.push_str(&format!(" and {} more", found.len() - shown));
    }
    text
}

impl From<String> for AdmissionError {
    fn from(detail: String) -> Self {
        crate::append_admission::AdmissionError::new(
            crate::append_admission::AdmissionErrorKind::Unproved(detail),
        )
    }
}
impl From<&str> for AdmissionError {
    fn from(detail: &str) -> Self {
        crate::append_admission::AdmissionError::new(
            crate::append_admission::AdmissionErrorKind::Unproved(detail.into()),
        )
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
    #[cfg(test)]
    {
        if let Some(error) = b3b_caller_injection::take() {
            return Err(error);
        }
    }
    let result = (|| {
        let _context = drive.scoped_outcome_context(remanence_library::outcome::OperationContext {
            phase: remanence_library::outcome::Phase::Admission,
            region: remanence_library::outcome::Region::Unknown,
            ..Default::default()
        });
        drive.clear_append_admission();
        if ctx.block_size == 0 || ctx.block_size > remanence_scsi::read_write::MAX_TRANSFER_LEN {
            return Err(crate::append_admission::AdmissionError::new(
                crate::append_admission::AdmissionErrorKind::Unproved(
                    "invalid append block size".into(),
                ),
            ));
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
                    .map_err(|e| {
                        crate::append_admission::AdmissionError::new(
                            crate::append_admission::AdmissionErrorKind::Unproved(e.to_string()),
                        )
                    })?,
                )
            }
            _ => None,
        };
        let mut ended_tail_eod = None;
        let probe = (|| {
            let boundary = match &authority {
                AppendAuthority::Fresh => {
                    drive.rewind()?;
                    crate::verify_tape_identity(&mut DriveHandleSource(drive), &ctx.tape_uuid)
                        .map_err(|e| match e {
                            crate::pool_write::TapeIdentityError::Io(e) => {
                                crate::append_admission::AdmissionError::new(
                                    crate::append_admission::AdmissionErrorKind::Io(e),
                                )
                            }
                            e => crate::append_admission::AdmissionError::new(
                                crate::append_admission::AdmissionErrorKind::Unproved(
                                    e.to_string(),
                                ),
                            ),
                        })?;
                    let mut block = vec![0; ctx.block_size as usize];
                    match drive.read_block(&mut block) {
                        Err(remanence_library::TapeIoError {
                            kind: remanence_library::TapeIoErrorKind::FilemarkEncountered,
                            ..
                        }) => {}
                        Err(e) if e.is_physical_eod() => {
                            if ctx.allow_missing_bootstrap_filemark {
                                return Ok(AppendSuffix::default());
                            }
                            return Err(crate::append_admission::AdmissionError::new(
                            crate::append_admission::AdmissionErrorKind::MissingBootstrapFilemark,
                        ));
                        }
                        Err(e) => {
                            return Err(crate::append_admission::AdmissionError::new(
                                crate::append_admission::AdmissionErrorKind::Io(e),
                            ))
                        }
                        Ok(_) => {
                            return Err(crate::append_admission::AdmissionError::new(
                                crate::append_admission::AdmissionErrorKind::Unproved(
                                    "data where bootstrap filemark was expected".into(),
                                ),
                            ))
                        }
                    }
                    let position = drive.position()?;
                    if position.partition != 0 {
                        return Err(crate::append_admission::AdmissionError::new(
                            crate::append_admission::AdmissionErrorKind::Unproved(
                                "bootstrap filemark position mismatch".into(),
                            ),
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
                                .map_err(|e| {
                                    crate::append_admission::AdmissionError::new(
                                        crate::append_admission::AdmissionErrorKind::Unproved(
                                            e.to_string(),
                                        ),
                                    )
                                })?;
                            }
                            return Ok(AppendSuffix::default());
                        }
                        Ok(_)
                        | Err(crate::append_admission::AdmissionError {
                            kind: crate::append_admission::AdmissionErrorKind::Unproved(_),
                            ..
                        }) => {
                            prove_position(drive, boundary, ctx.block_size, generation)?;
                        }
                        Err(e) => return Err(e),
                    }
                }
                if summary.open_session.is_none() {
                    let mut failure = None;
                    let mut reached_eod = false;
                    let mut named_end = boundary.lba;
                    snapshot
                        .visit_session_tail(&mut |file| {
                            if reached_eod {
                                return Ok(());
                            }
                            session_suffix::named_file(drive, &ctx.tape_uuid, ctx.block_size, file)
                                .map(|(mark, lba)| {
                                    reached_eod = !mark;
                                    named_end = lba;
                                })
                                .map_err(|e| {
                                    failure = Some(e);
                                    remanence_state::StateError::JournalReplayFailed(
                                        "session suffix proof failed".into(),
                                    )
                                })
                        })
                        .map_err(|e| {
                            failure.take().unwrap_or_else(|| {
                                crate::append_admission::AdmissionError::new(
                                    crate::append_admission::AdmissionErrorKind::Unproved(
                                        e.to_string(),
                                    ),
                                )
                            })
                        })?;
                    if !reached_eod {
                        match session_suffix::eod(drive, ctx.block_size) {
                            Err(AdmissionError {
                                kind: AdmissionErrorKind::Unproved(_),
                                ..
                            }) if summary
                                .last_session_end
                                .as_ref()
                                .and_then(|end| end.proved_eod)
                                .is_none_or(|old| old == named_end) =>
                            {
                                // Only bytes beyond the fully proved named suffix are a restore tail.
                                prove_position(
                                    drive,
                                    TapePosition {
                                        lba: named_end,
                                        ..boundary
                                    },
                                    ctx.block_size,
                                    generation,
                                )?;
                                let suffix = scan_append_suffix_observed(
                                    drive,
                                    &ctx.tape_uuid,
                                    ctx.block_size,
                                    named_end,
                                    None,
                                    ctx.terminal_prefix.as_ref(),
                                    true,
                                    None,
                                )
                                .map_err(|error| match error {
                                    AdmissionError {
                                        kind: AdmissionErrorKind::Io(source),
                                        ..
                                    } => AdmissionError::new(AdmissionErrorKind::SuffixIo {
                                        start_lba: named_end,
                                        source,
                                    }),
                                    other => other,
                                })?;
                                let mut found = suffix.objects.clone();
                                found.extend(suffix.foreign_components);
                                // A confirmed list or an empty torn fragment cannot authorize this shape.
                                return Err(AdmissionError::new(
                                    AdmissionErrorKind::TailBeyondKnownState {
                                        tape_uuid: uuid::Uuid::from_bytes(ctx.tape_uuid),
                                        expected_eod_lba: named_end,
                                        found,
                                        object_ids: suffix.objects,
                                    },
                                ));
                            }
                            other => other?,
                        }
                    }
                    let eod = drive.position()?.lba;
                    if let Some(end) = &summary.last_session_end {
                        if eod < end.cursor.lba.saturating_add(end.synchronized_tail_blocks)
                            || eod > end.cursor.lba.saturating_add(end.possible_tail_blocks)
                            || end.proved_eod.is_some_and(|old| old != eod)
                        {
                            return Err(crate::append_admission::AdmissionError::pending(
                                crate::append_admission::AdmissionErrorKind::Unproved(
                                    "SessionSuffixContradiction: EOD outside recorded tail bounds"
                                        .into(),
                                ),
                            ));
                        }
                        if end.possible_tail_blocks > end.synchronized_tail_blocks {
                            ended_tail_eod = Some(eod);
                        }
                    }
                    return Ok(AppendSuffix::default());
                }
            }
            let mut block = vec![0; ctx.block_size as usize];
            match drive.read_block(&mut block) {
                Err(e) if e.is_physical_eod() => return Ok(AppendSuffix::default()),
                Ok(_)
                | Err(remanence_library::TapeIoError {
                    kind: remanence_library::TapeIoErrorKind::FilemarkEncountered,
                    ..
                }) => {}
                Err(source) => {
                    return Err(crate::append_admission::AdmissionError::new(
                        crate::append_admission::AdmissionErrorKind::SuffixIo {
                            start_lba: boundary.lba,
                            source,
                        },
                    ))
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
                crate::append_admission::AdmissionError {
                    kind: crate::append_admission::AdmissionErrorKind::Io(source),
                    ..
                } => crate::append_admission::AdmissionError::new(
                    crate::append_admission::AdmissionErrorKind::SuffixIo {
                        start_lba: boundary.lba,
                        source,
                    },
                ),
                other => other,
            })?;
            if recovered_tail.is_some() && suffix.unnameable {
                return Err(crate::append_admission::AdmissionError::new(crate::append_admission::AdmissionErrorKind::Unproved(
                "crash suffix contains a delimited file without a session-tail identity; restore consistent tape state".into(),
            )));
            }
            let mut found = suffix.objects.clone();
            found.extend(suffix.foreign_components.clone());
            if ctx
                .confirmed_discard
                .as_ref()
                .is_some_and(|confirmed| confirmed != &found)
                || (!found.is_empty() && ctx.confirmed_discard.is_none())
            {
                return Err(crate::append_admission::AdmissionError::new(
                    crate::append_admission::AdmissionErrorKind::TailBeyondKnownState {
                        tape_uuid: uuid::Uuid::from_bytes(ctx.tape_uuid),
                        expected_eod_lba: boundary.lba,
                        found,
                        object_ids: suffix.objects.clone(),
                    },
                ));
            }
            Ok(suffix)
        })();
        let probe = probe.map_err(|error| match (&authority, error) {
            (
                AppendAuthority::Session { snapshot, .. },
                crate::append_admission::AdmissionError {
                    kind: crate::append_admission::AdmissionErrorKind::Unproved(detail),
                    ..
                },
            ) if detail.starts_with("SessionSuffixContradiction:") => {
                crate::append_admission::AdmissionError::new(
                    crate::append_admission::AdmissionErrorKind::SessionSuffixContradiction {
                        tape_uuid: uuid::Uuid::from_bytes(ctx.tape_uuid),
                        partition: ctx.boundary.partition,
                        lba: ctx.boundary.lba,
                        sequence: snapshot.log_reference().0,
                        incarnation: snapshot.tape_summary().incarnation,
                        detail,
                    },
                )
            }
            (_, error) => error,
        });
        let restored = if matches!(authority, AppendAuthority::Fresh) && ctx.write_position.lba == 0
        {
            drive
                .locate(0)
                .and_then(|actual| {
                    if actual.partition != ctx.write_position.partition || actual.lba != 0 {
                        return Err(remanence_library::TapeIoError::new(
                            remanence_library::TapeIoErrorKind::OperationFailed(format!(
                                "position mismatch: expected {:?}, got {actual:?}",
                                ctx.write_position
                            )),
                        ));
                    }
                    Ok(None)
                })
                .map_err(|value| {
                    crate::append_admission::AdmissionError::new(
                        crate::append_admission::AdmissionErrorKind::Io(value),
                    )
                })
        } else {
            prove_position(drive, ctx.write_position, ctx.block_size, generation).map(Some)
        };
        // A suffix read failure refuses admission even when it also invalidates the
        // handle. Preserve its location after attempting cursor restoration.
        if let Err(
            error @ crate::append_admission::AdmissionError {
                kind: crate::append_admission::AdmissionErrorKind::SuffixIo { .. },
                ..
            },
        ) = probe
        {
            return Err(error);
        }
        if drive.identity_generation() != generation {
            return Err(crate::append_admission::AdmissionError::new(
                crate::append_admission::AdmissionErrorKind::Io(
                    remanence_library::TapeIoError::new(
                        remanence_library::TapeIoErrorKind::IdentityInvalidated {
                            proved: generation,
                            current: drive.identity_generation(),
                        },
                    ),
                ),
            ));
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
                ended_tail_eod,
            })
            .map_err(|value| {
                crate::append_admission::AdmissionError::new(
                    crate::append_admission::AdmissionErrorKind::Io(value),
                )
            })
    })();
    result.map_err(|mut error: AdmissionError| {
        error.finish();
        error
    })
}

fn prove_position(
    drive: &mut DriveHandle,
    expected: TapePosition,
    block_size: u32,
    generation: u64,
) -> Result<remanence_library::cursor::CursorProof, AdmissionError> {
    drive
        .prove_cursor(expected, block_size, generation)
        .map_err(|value| {
            crate::append_admission::AdmissionError::new(
                crate::append_admission::AdmissionErrorKind::Io(value),
            )
        })
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
pub(crate) mod tests;

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

/// Read the outcome retained before any restoration or status rendering.
pub(crate) fn admission_error_outcome(
    error: &AdmissionError,
) -> remanence_library::outcome::ClassifiedOutcome {
    error
        .outcome
        .as_deref()
        .expect("unfinished admission error")
        .clone()
}

/// Finish a new structural admission fact; device failures already carry their outcome.
fn finish_admission_refusal(
    error: &AdmissionErrorKind,
) -> remanence_library::outcome::ClassifiedOutcome {
    use remanence_library::outcome::*;
    let mut evidence = CompletionEvidence::success();
    evidence.failed = true;
    evidence.fact = if matches!(error, AdmissionErrorKind::SessionSuffixContradiction { .. }) {
        CompletionFact::SuffixContradiction
    } else {
        CompletionFact::AdmissionRecovery
    };
    classify(OutcomeInput {
        class: CommandClass::Read,
        context: OperationContext {
            phase: Phase::Admission,
            region: Region::Unknown,
            ..Default::default()
        },
        evidence,
    })
}

/// Preserve the tape fence producer's label and evidence within the library log's
/// Text bound, so live projection and replay copy the same diagnostic values.
pub(crate) fn tape_fence_detail(
    barcode: Option<&str>,
    evidence_json: Option<&str>,
) -> remanence_state::library_log::Bounded<remanence_state::library_log::DiagnosticEntry, 256> {
    use remanence_state::library_log as lib;
    let mut detail = std::collections::BTreeMap::<lib::Text, lib::DiagnosticValue>::new();
    for (key, value) in [
        ("barcode", barcode.map(str::trim)),
        ("evidence_json", evidence_json),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            let mut end = value.len().min(4096);
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            detail.insert(key.into(), lib::DiagnosticValue::Text(value[..end].into()));
        }
    }
    lib::Bounded(
        detail
            .into_iter()
            .map(|(key, value)| lib::DiagnosticEntry(key, value))
            .collect(),
    )
}

/// A stale physical tail disables this tape alone. Probe failures without writes
/// remain retryable and do not create durable quarantine or recovery state.
pub(crate) fn daemon_admission_error(
    actor: remanence_state::AuditActor,
    index: &mut remanence_state::CatalogIndex,
    cfg: &crate::write_owner::TerminalWriteConfig,
    tape_uuid: [u8; 16],
    error: AdmissionError,
    outcome: &remanence_library::outcome::ClassifiedOutcome,
) -> tonic::Status {
    if matches!(
        error.kind,
        AdmissionErrorKind::Io(_) | AdmissionErrorKind::SuffixIo { .. }
    ) {
        return outcome
            .retry
            .attach(tonic::Status::unavailable(error.to_string()));
    }
    if let remanence_library::outcome::Effect::WriteFence(cause) = outcome.effect {
        let evidence = match &error {
            crate::append_admission::AdmissionError { kind: crate::append_admission::AdmissionErrorKind::SessionSuffixContradiction { detail, partition, lba, sequence, incarnation, .. }, .. } =>
                serde_json::json!({"error": error.to_string(), "detail": detail, "partition":partition, "lba":lba, "tape_log_sequence":sequence, "tape_incarnation":incarnation}).to_string(),
            _ => serde_json::json!({"error": error.to_string()}).to_string(),
        };
        crate::write_owner::record_closeout_fence(index, cfg, tape_uuid, cause.reason(), evidence);
        return outcome
            .retry
            .attach(tonic::Status::failed_precondition(error.to_string()));
    }
    let detail = error.to_string();
    if !outcome.recovery_required {
        return outcome.retry.attach(tonic::Status::unavailable(detail));
    }
    use remanence_state::library_log as lib;
    let Some(library) = cfg.authority.runtime() else {
        return tonic::Status::failed_precondition(
            "append refusal cannot be durably fenced: shared library authority is missing",
        );
    };
    let Some(tape) = library.tape_state(tape_uuid) else {
        return tonic::Status::failed_precondition(
            "append refusal cannot be durably fenced: tape has no Reprovision authority",
        );
    };
    let barcode = match index.get_tape(&tape_uuid) {
        Ok(Some(tape)) => tape.voltag,
        other => {
            tracing::error!(
                ?other,
                "cannot resolve barcode for admission fence; retain UUID identity"
            );
            None
        }
    };
    let evidence = serde_json::json!({"error": detail}).to_string();
    match library.append(|authority, view| {
        authority.fence_raised(
            lib::FenceRaised {
                detail: tape_fence_detail(barcode.as_deref(), Some(&evidence)),
                kind: lib::FenceKind::Write,
                scope: lib::FenceScope::Tape(tape.tape),
                cause: lib::FenceCause::AppendAdmissionRefused,
                evidence: None,
                at: None,
                operation: None,
            },
            actor,
            remanence_state::SourceLayer::Layer5,
            view,
        )
    }) {
        Ok(_) => outcome
            .retry
            .attach(tonic::Status::failed_precondition(detail)),
        Err(error) => tonic::Status::failed_precondition(format!(
            "{detail}; durable library fence failed: {error}"
        )),
    }
}

/// Thread-scoped admission-boundary injection for production-caller persistence regressions.
#[cfg(test)]
pub(crate) mod b3b_caller_injection {
    use super::AdmissionError;
    thread_local! {
        static FAILURE: std::cell::RefCell<Option<Box<dyn FnOnce() -> AdmissionError>>> = Default::default();
    }
    pub(crate) fn set(make: impl FnOnce() -> AdmissionError + 'static) {
        FAILURE.with(|slot| assert!(slot.borrow_mut().replace(Box::new(make)).is_none()));
    }
    pub(super) fn take() -> Option<AdmissionError> {
        FAILURE
            .with(|slot| slot.borrow_mut().take())
            .map(|make| make())
    }
    pub(crate) fn pending() -> bool {
        FAILURE.with(|slot| slot.borrow().is_some())
    }
}
