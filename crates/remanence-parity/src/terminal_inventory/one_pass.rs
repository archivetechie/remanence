//! Index-first verification accepts only complete agreement with a measured prefix.
//! Any non-cancellation rejection drops the attempt and runs the walk from scratch.

use super::*;

/// Full-verification implementation selected by a daemon operator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationRoute {
    /// Measure the tape with filemark spacing before checking protected content.
    #[default]
    Walk,
    /// Stream the prefix after validating the terminal index.
    OnePass,
}

impl VerificationRoute {
    /// Stable configuration and progress name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Walk => "walk",
            Self::OnePass => "one_pass",
        }
    }
}

/// Route progress stays outside the physical verification verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerificationRouteStatus {
    /// Implementation currently in use.
    pub route: VerificationRoute,
    /// The one-pass attempt rejected its provisional evidence.
    pub fell_back: bool,
    /// First rejection kind, or the reason the walk is selected directly.
    pub fallback_reason: Option<&'static str>,
}

/// Select the daemon route while preserving the walk's recovery and authority path.
pub fn verify_terminal_index_full_with_route(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    mut authority: Option<&mut dyn BotObjectRecoveryAuthority>,
    mode: ScanMode<'_>,
    route: VerificationRoute,
    mut report: impl FnMut(VerificationRouteStatus),
) -> Result<TerminalIndexVerificationOutcome, TerminalIndexVerificationError> {
    report(VerificationRouteStatus {
        route,
        fell_back: false,
        fallback_reason: None,
    });
    if route == VerificationRoute::OnePass {
        let _verification = source.verification_context().verification();
        match verify_one_pass(source, tape_uuid, block_size, mode, 64 * 1024 * 1024) {
            Ok(complete) => return Ok(complete_outcome(complete)),
            Err(AttemptError::Fatal(error)) => return Err(error),
            Err(AttemptError::Rejected(reason)) => report(VerificationRouteStatus {
                route: VerificationRoute::Walk,
                fell_back: reason != "no_parity",
                fallback_reason: Some(reason),
            }),
        }
    }
    verify_terminal_index_full_inner::<Forward>(source, tape_uuid, block_size, &mut authority, mode)
}

/// Only cancellation can end an attempt without running the walk.
enum AttemptError {
    Rejected(&'static str),
    Fatal(TerminalIndexVerificationError),
}

impl From<TerminalIndexVerificationError> for AttemptError {
    fn from(error: TerminalIndexVerificationError) -> Self {
        use TerminalIndexVerificationError::*;
        let reason = match error {
            Cancelled => return Self::Fatal(error),
            Source { .. } => "source",
            BlockSize(_) => "block_size",
            NoCompleteLayout { .. } => "no_complete_layout",
            ConflictingLayouts { .. } => "conflicting_layouts",
            ConflictingReplicaEditions { .. } => "conflicting_editions",
            PrefixWalk { .. } => "prefix_walk",
            RecoveryAuthority { .. } => "recovery_authority",
            PrefixTruncated { .. } => "prefix_truncated",
            PrefixDamaged { .. } => "prefix_damaged",
            TapeFileCountMismatch { .. } => "file_count",
            TerminalComponentMismatch { .. } => "terminal_component",
            CanonicalMapMismatch { .. } => "canonical_map",
            Replica { .. } => "replica",
            CrossReplicaConflict { .. } => "cross_replica",
            Separation { .. } => "separation",
            ArithmeticOverflow { .. } => "arithmetic",
        };
        Self::Rejected(reason)
    }
}

fn prefix_error(error: ParityError) -> AttemptError {
    match error {
        ParityError::TapeIo(remanence_library::TapeIoError::Cancelled) => {
            AttemptError::Fatal(TerminalIndexVerificationError::Cancelled)
        }
        ParityError::TapeIo(_) => AttemptError::Rejected("tape_io"),
        ParityError::FilemarkMapReconstruct(_) => AttemptError::Rejected("prefix_map"),
        _ => AttemptError::Rejected("prefix_check"),
    }
}

fn verify_one_pass(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    mode: ScanMode<'_>,
    budget: usize,
) -> Result<TerminalIndexCompleteEvidence, AttemptError> {
    let _phase = source
        .verification_context()
        .phase(remanence_library::verification_io::VerificationPhase::Walk);
    let TerminalVerificationPlan {
        measured_eod,
        layout,
    } = plan_terminal_verification(source, tape_uuid, block_size)?;
    let TerminalReplicaSetEvidence {
        edition,
        replicas,
        rows,
    } = verify_terminal_replicas(
        source,
        tape_uuid,
        block_size,
        layout,
        ReplicaRowCheck::CollectOnly,
    )?;
    if !rows[0]
        .iter()
        .any(|row| row.kind == TapeIndexReplicaFileKind::ParitySidecar)
    {
        return Err(AttemptError::Rejected("no_parity"));
    }
    let separation_interior_records = verify_terminal_separations(
        source,
        tape_uuid,
        block_size,
        layout,
        edition.descriptor.edition_id,
    )?;
    let scoped = scoped_map_from_terminal_replica(&edition, &rows[0]).map_err(prefix_error)?;
    let mut directory_cache = crate::recovery::DirectoryCache::default();
    directory_cache
        .preload(source, &scoped, tape_uuid, block_size)
        .map_err(prefix_error)?;
    if matches!(directory_cache, crate::recovery::DirectoryCache::Failed) {
        return Err(AttemptError::Rejected("directory_load"));
    }
    let directory = {
        let _phase = source
            .verification_context()
            .phase(remanence_library::verification_io::VerificationPhase::ParityMap);
        crate::parity_map::read_final_parity_map(source, &scoped.map, tape_uuid, block_size)
            .map_err(prefix_error)?
    };
    let mut checks = crate::verify_protected::one_pass::PrefixChecks::new(
        scoped,
        &rows,
        layout.components[0].planned_start_lba,
        *tape_uuid,
        block_size,
        budget,
        directory_cache,
    )
    .map_err(prefix_error)?;
    let _prefix_phase = source
        .verification_context()
        .phase(remanence_library::verification_io::VerificationPhase::OnePassPrefix);
    let mut walked = crate::scan::scan_observed_prefix(
        source,
        tape_uuid,
        block_size,
        mode,
        &mut checks,
        directory,
    )
    .map_err(|error| match prefix_error(error) {
        AttemptError::Fatal(error) => AttemptError::Fatal(attach_prefix_findings(
            error,
            checks.bootstrap_findings.clone(),
        )),
        rejected => rejected,
    })?;
    for (index, rows) in rows.iter().enumerate() {
        check_rows_against_measured(index as u16 + 1, rows, walked.map.entries())?;
    }
    // The measured prefix joins the independently validated terminal components.
    let measured_end = walked
        .map
        .entries()
        .iter()
        .try_fold(0u64, |lba, entry| {
            lba.checked_add(entry.block_count)?.checked_add(1)
        })
        .ok_or(TerminalIndexVerificationError::ArithmeticOverflow {
            context: "observed prefix end",
        })?;
    if measured_end != layout.components[0].planned_start_lba {
        return Err(TerminalIndexVerificationError::CanonicalMapMismatch {
            ordinal: 1,
            tape_file_number: walked.map.tape_file_count(),
            detail: "measured prefix does not join replica A".into(),
        }
        .into());
    }
    let mut entries = walked.map.entries().to_vec();
    for component in layout.components {
        entries.push(match component.kind {
            crate::terminal_tail::TerminalTailComponentKind::TapeIndexReplica => {
                TapeFileMapEntry::tape_index_replica(
                    component.planned_tape_file_number,
                    component.record_count,
                )
            }
            crate::terminal_tail::TerminalTailComponentKind::IndexSeparationExtent => {
                TapeFileMapEntry::index_separation_extent(
                    component.planned_tape_file_number,
                    component.record_count,
                )
            }
        });
    }
    walked.map = crate::filemark_map::FilemarkMap::new(entries).map_err(prefix_error)?;
    let measured = check_measured_prefix(&walked, &layout)?;
    let protected = retain_prefix_findings(
        Ok(checks.result),
        walked
            .bootstrap_candidates
            .iter()
            .filter_map(bootstrap_fill_finding)
            .collect(),
    )?;
    Ok(TerminalIndexCompleteEvidence {
        edition,
        replicas,
        separation_interior_records,
        measured_eod,
        verified_prefix_tape_file_count: measured.prefix.len() as u64,
        verified_prefix_record_count: measured.verified_prefix_record_count,
        measured_tape_file_count: measured.expected_file_count,
        protected,
    })
}

/// Exercise the same provisional pass with a constrained pending budget.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub fn verify_terminal_index_one_pass_budget_for_test(
    source: &mut dyn RawTapeSource,
    uuid: &[u8; 16],
    size: u32,
    budget: usize,
) -> Result<TerminalIndexVerificationOutcome, TerminalIndexVerificationError> {
    let _verification = source.verification_context().verification();
    match verify_one_pass(source, uuid, size, ScanMode::Standard, budget) {
        Ok(complete) => Ok(complete_outcome(complete)),
        Err(AttemptError::Fatal(error)) => Err(error),
        Err(AttemptError::Rejected(_)) => Err(TerminalIndexVerificationError::PrefixWalk {
            message: "provisional pass rejected".into(),
        }),
    }
}
