//! Catalog-independent structural recovery from the beginning of tape.
//!
//! This module owns the slow fallback used when no terminal index replica can
//! establish inventory authority. Physical scanning decides which Object tape
//! files are complete. An optional, separately durable authority may then bind
//! exact Object identifiers to those measured files; it never changes the
//! scanner's completeness decision.

use crate::bootstrap::parse_bootstrap_block;
use crate::error::ParityError;
use crate::filemark_map::{TapeFileKind, TapeFileMapEntry};
use crate::raw::{PhysicalPositionHint, RawReadOutcome, RawTapeSource};
use crate::scan::scan_reconstruct_filemark_map_with_report;
use remanence_library::{scsi::decode_sense, TapeIoError};

/// One exact Object identity assertion from separately durable authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotObjectRecoveryAuthorityRow {
    /// Dense tape-file number measured from BOT.
    pub tape_file_number: u64,
    /// Fixed-block count committed for this Object copy.
    pub stored_block_count: u64,
    /// Verbatim 1–64-byte REM-OBJECT identifier.
    pub object_id: Vec<u8>,
}

/// Immutable scope proved by one complete Object-authority replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BotObjectRecoveryAuthorityScope {
    /// Physical tape identity bound by the authority.
    pub tape_uuid: [u8; 16],
    /// Fixed block size bound by the authority.
    pub block_size: u32,
    /// First tape-file number outside the committed structural prefix.
    ///
    /// Zero denotes the explicit absence of any external authority.
    pub covered_prefix_tape_file_count: u64,
    /// Exact number of Object rows below the covered-prefix boundary.
    pub object_row_count: u64,
}

/// Frozen, validated Object identity authority for a BOT recovery pass.
///
/// Implementations must visit rows in strictly increasing tape-file order.
/// Rows are provisional if this method returns an error; callers must discard
/// them unless the complete recovery operation succeeds.
pub trait BotObjectRecoveryAuthority {
    /// Visit every committed Object identity row without retaining the full
    /// authority in memory.
    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(
            &BotObjectRecoveryAuthorityRow,
        ) -> Result<(), BotStructuralRecoveryError>,
    ) -> Result<BotObjectRecoveryAuthorityScope, BotStructuralRecoveryError>;
}

/// BOT recovery classification for one physical Object candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BotRecoveredObjectState {
    /// A structurally complete Object matched exact external recovery authority.
    Recovered,
    /// The Object is structurally complete but has no trustworthy identity row.
    Unknown,
    /// The scanner reached EOD before this Object's trailing filemark.
    Incomplete,
}

/// One Object classification emitted by the explicit BOT recovery pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotRecoveredObject {
    /// Dense tape-file number measured from BOT.
    pub tape_file_number: u64,
    /// Complete fixed-block count, or zero when a torn file could not be measured.
    pub stored_block_count: u64,
    /// Recovered REM-OBJECT identifier when external authority survived.
    pub object_id: Option<Vec<u8>>,
    /// Typed recovery state.
    pub state: BotRecoveredObjectState,
}

/// Summary of a completed BOT structural recovery pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotStructuralRecoverySummary {
    /// Structurally complete tape files reconstructed from BOT.
    pub structural_entry_count: u64,
    /// Structurally complete physical Object candidates.
    pub complete_object_count: u64,
    /// Objects with exact external recovery authority.
    pub recovered_object_count: u64,
    /// Structurally complete Objects without trustworthy identity authority.
    pub unknown_object_count: u64,
    /// Torn Object candidates at EOD.
    pub incomplete_object_count: u64,
    /// Canonical digest of the structurally complete measured map.
    pub canonical_map_digest: [u8; 32],
    /// Number of physical damage regions retained as recovery evidence.
    pub damaged_region_count: u64,
}

/// Failure of the explicit BOT structural recovery path.
#[derive(Debug, thiserror::Error)]
pub enum BotStructuralRecoveryError {
    /// Physical scan failed and no recovery inventory can be claimed.
    #[error("BOT structural scan failed: {message}")]
    Scan {
        /// Scanner/source detail.
        message: String,
    },
    /// A readable BOT Bootstrap belongs to a different expected tape identity.
    #[error("BOT Bootstrap tape UUID does not match the required recovery identity hint")]
    TapeIdentityMismatch,
    /// The separately durable Object authority could not be validated or read.
    #[error("BOT Object recovery authority failed: {message}")]
    ObjectAuthority {
        /// Authority-source detail.
        message: String,
    },
    /// Object recovery authority disagreed with physical BOT measurements.
    #[error("conflicting BOT Object authority for tape file {tape_file_number}: {detail}")]
    ConflictingObjectAuthority {
        /// Dense tape-file number.
        tape_file_number: u64,
        /// Conflict detail.
        detail: String,
    },
    /// Caller rejected an emitted Object classification.
    #[error("BOT recovery Object visitor failed: {message}")]
    Visitor {
        /// Caller detail.
        message: String,
    },
    /// Checked recovery accounting overflowed.
    #[error("BOT structural recovery arithmetic overflow: {context}")]
    ArithmeticOverflow {
        /// Failed counter.
        context: &'static str,
    },
}

struct NoBotObjectRecoveryAuthority {
    tape_uuid: [u8; 16],
    block_size: u32,
}

impl BotObjectRecoveryAuthority for NoBotObjectRecoveryAuthority {
    fn visit_object_rows(
        &mut self,
        _visitor: &mut dyn FnMut(
            &BotObjectRecoveryAuthorityRow,
        ) -> Result<(), BotStructuralRecoveryError>,
    ) -> Result<BotObjectRecoveryAuthorityScope, BotStructuralRecoveryError> {
        Ok(BotObjectRecoveryAuthorityScope {
            tape_uuid: self.tape_uuid,
            block_size: self.block_size,
            covered_prefix_tape_file_count: 0,
            object_row_count: 0,
        })
    }
}

/// Reconstruct Object candidates by walking from BOT without identity authority.
///
/// Every complete Object is therefore reported as [`BotRecoveredObjectState::Unknown`].
/// Use [`recover_terminal_inventory_from_bot_with_authority`] when a separately
/// durable checkpoint source survived.
pub fn recover_terminal_inventory_from_bot<F>(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    visit_object: F,
) -> Result<BotStructuralRecoverySummary, BotStructuralRecoveryError>
where
    F: FnMut(&BotRecoveredObject) -> Result<(), String>,
{
    recover_terminal_inventory_from_bot_with_authority(
        source,
        tape_uuid,
        block_size,
        &mut NoBotObjectRecoveryAuthority {
            tape_uuid: *tape_uuid,
            block_size,
        },
        visit_object,
    )
}

/// Reconstruct Object candidates from BOT and bind exact surviving authority.
///
/// Physical completeness remains scanner-owned. Complete candidates without a
/// matching row are `Unknown`; an exact file/count/id row is `Recovered`; a
/// torn tail is `Incomplete` and is never offered to the authority source.
pub fn recover_terminal_inventory_from_bot_with_authority<A, F>(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    authority: &mut A,
    mut visit_object: F,
) -> Result<BotStructuralRecoverySummary, BotStructuralRecoveryError>
where
    A: BotObjectRecoveryAuthority + ?Sized,
    F: FnMut(&BotRecoveredObject) -> Result<(), String>,
{
    reject_readable_foreign_bot_bootstrap(source, tape_uuid, block_size)?;
    let walked = scan_reconstruct_filemark_map_with_report(source, tape_uuid, block_size).map_err(
        |error| BotStructuralRecoveryError::Scan {
            message: error.to_string(),
        },
    )?;

    // Pass one proves the complete authority/map bijection before an Object
    // classification can escape to the caller.
    let mut first_previous_file = None;
    let mut first_row_count = 0u64;
    let first_scope = authority.visit_object_rows(&mut |row| {
        validate_authority_row(&walked.map, row, &mut first_previous_file)?;
        first_row_count =
            checked_recovery_increment(first_row_count, "first BOT authority Object-row count")?;
        Ok(())
    })?;
    validate_authority_scope(
        &walked.map,
        tape_uuid,
        block_size,
        first_scope,
        first_previous_file,
        first_row_count,
    )?;

    let mut complete_object_count = 0u64;
    let mut recovered_object_count = 0u64;
    let mut unknown_object_count = 0u64;

    // Pass two replays the same frozen authority and emits only exact matches.
    let mut second_previous_file = None;
    let mut second_row_count = 0u64;
    let second_scope = authority.visit_object_rows(&mut |row| {
        let entry = validate_authority_row(&walked.map, row, &mut second_previous_file)?;
        if row.tape_file_number >= first_scope.covered_prefix_tape_file_count {
            return Err(authority_conflict(
                row.tape_file_number,
                "authority row lies outside its committed prefix",
            ));
        }
        second_row_count =
            checked_recovery_increment(second_row_count, "second BOT authority Object-row count")?;
        emit_complete_object(
            entry,
            Some(row.object_id.clone()),
            &mut visit_object,
            &mut complete_object_count,
            &mut recovered_object_count,
            &mut unknown_object_count,
        )
    })?;
    if second_scope != first_scope || second_row_count != first_row_count {
        return Err(BotStructuralRecoveryError::ObjectAuthority {
            message: "BOT Object authority changed between validation and emission passes"
                .to_string(),
        });
    }

    for entry in walked.map.entries().iter().filter(|entry| {
        entry.kind == TapeFileKind::Object
            && entry.tape_file_number >= first_scope.covered_prefix_tape_file_count
    }) {
        emit_complete_object(
            entry,
            None,
            &mut visit_object,
            &mut complete_object_count,
            &mut recovered_object_count,
            &mut unknown_object_count,
        )?;
    }

    let mut incomplete_object_count = 0u64;
    if walked.truncation_candidate_kind == Some(TapeFileKind::Object) {
        let truncation = walked
            .truncation
            .ok_or_else(|| BotStructuralRecoveryError::Scan {
                message: "scanner classified a torn Object without truncation evidence".to_string(),
            })?;
        incomplete_object_count = 1;
        visit_object(&BotRecoveredObject {
            tape_file_number: truncation.tape_file_number,
            stored_block_count: 0,
            object_id: None,
            state: BotRecoveredObjectState::Incomplete,
        })
        .map_err(|message| BotStructuralRecoveryError::Visitor { message })?;
    }

    Ok(BotStructuralRecoverySummary {
        structural_entry_count: walked.map.tape_file_count(),
        complete_object_count,
        recovered_object_count,
        unknown_object_count,
        incomplete_object_count,
        canonical_map_digest: walked.map.canonical_digest().map_err(|error| {
            BotStructuralRecoveryError::Scan {
                message: error.to_string(),
            }
        })?,
        damaged_region_count: u64::try_from(walked.damaged_regions.len()).map_err(|_| {
            BotStructuralRecoveryError::ArithmeticOverflow {
                context: "BOT damaged-region count",
            }
        })?,
    })
}

fn validate_authority_scope(
    map: &crate::filemark_map::FilemarkMap,
    tape_uuid: &[u8; 16],
    block_size: u32,
    scope: BotObjectRecoveryAuthorityScope,
    last_authority_file: Option<u64>,
    observed_row_count: u64,
) -> Result<(), BotStructuralRecoveryError> {
    if scope.tape_uuid != *tape_uuid || scope.block_size != block_size {
        return Err(BotStructuralRecoveryError::ObjectAuthority {
            message: "BOT Object authority tape identity or block size does not match the recovery request"
                .to_string(),
        });
    }
    if scope.covered_prefix_tape_file_count == 0 {
        if scope.object_row_count != 0 || observed_row_count != 0 {
            return Err(BotStructuralRecoveryError::ObjectAuthority {
                message: "absent BOT Object authority emitted rows or a nonzero count".to_string(),
            });
        }
        return Ok(());
    }
    if map.tape_file_count() < scope.covered_prefix_tape_file_count {
        return Err(BotStructuralRecoveryError::ObjectAuthority {
            message: format!(
                "authority covers {} tape files but the complete BOT map contains only {}",
                scope.covered_prefix_tape_file_count,
                map.tape_file_count()
            ),
        });
    }
    if !map
        .entries()
        .first()
        .is_some_and(|entry| entry.tape_file_number == 0 && entry.kind == TapeFileKind::Bootstrap)
    {
        return Err(BotStructuralRecoveryError::ObjectAuthority {
            message: "authority-covered BOT map does not begin with Bootstrap tape file 0"
                .to_string(),
        });
    }
    if last_authority_file.is_some_and(|file| file >= scope.covered_prefix_tape_file_count) {
        return Err(authority_conflict(
            last_authority_file.expect("checked authority file"),
            "authority row lies outside its committed prefix",
        ));
    }
    let measured_object_count = map
        .entries()
        .iter()
        .filter(|entry| {
            entry.kind == TapeFileKind::Object
                && entry.tape_file_number < scope.covered_prefix_tape_file_count
        })
        .try_fold(0u64, |count, _| {
            checked_recovery_increment(count, "measured authority-prefix Object count")
        })?;
    if observed_row_count != scope.object_row_count
        || measured_object_count != scope.object_row_count
    {
        return Err(BotStructuralRecoveryError::ObjectAuthority {
            message: format!(
                "authority prefix declares {} Object rows, replay emitted {observed_row_count}, and BOT measured {measured_object_count}",
                scope.object_row_count
            ),
        });
    }
    Ok(())
}

fn validate_authority_row<'a>(
    map: &'a crate::filemark_map::FilemarkMap,
    row: &BotObjectRecoveryAuthorityRow,
    previous_authority_file: &mut Option<u64>,
) -> Result<&'a TapeFileMapEntry, BotStructuralRecoveryError> {
    if previous_authority_file.is_some_and(|previous| previous >= row.tape_file_number) {
        return Err(authority_conflict(
            row.tape_file_number,
            "authority rows are not in strictly increasing tape-file order",
        ));
    }
    *previous_authority_file = Some(row.tape_file_number);
    if row.object_id.is_empty() || row.object_id.len() > 64 || row.object_id.contains(&0) {
        return Err(authority_conflict(
            row.tape_file_number,
            "Object identifier is not 1..=64 non-NUL bytes",
        ));
    }
    let index = usize::try_from(row.tape_file_number).map_err(|_| {
        authority_conflict(
            row.tape_file_number,
            "authority tape-file number does not fit the physical map index",
        )
    })?;
    let Some(entry) = map.entries().get(index) else {
        return Err(authority_conflict(
            row.tape_file_number,
            "authority names an Object beyond the structurally complete BOT map",
        ));
    };
    if entry.tape_file_number != row.tape_file_number || entry.kind != TapeFileKind::Object {
        return Err(authority_conflict(
            row.tape_file_number,
            "authority row does not name a complete physical Object candidate",
        ));
    }
    if entry.block_count != row.stored_block_count {
        return Err(authority_conflict(
            row.tape_file_number,
            format!(
                "authority block count {} differs from measured {}",
                row.stored_block_count, entry.block_count
            ),
        ));
    }
    Ok(entry)
}

fn emit_complete_object<F>(
    entry: &TapeFileMapEntry,
    object_id: Option<Vec<u8>>,
    visit_object: &mut F,
    complete_object_count: &mut u64,
    recovered_object_count: &mut u64,
    unknown_object_count: &mut u64,
) -> Result<(), BotStructuralRecoveryError>
where
    F: FnMut(&BotRecoveredObject) -> Result<(), String>,
{
    *complete_object_count =
        checked_recovery_increment(*complete_object_count, "complete BOT Object count")?;
    let state = if object_id.is_some() {
        *recovered_object_count =
            checked_recovery_increment(*recovered_object_count, "recovered BOT Object count")?;
        BotRecoveredObjectState::Recovered
    } else {
        *unknown_object_count =
            checked_recovery_increment(*unknown_object_count, "unknown BOT Object count")?;
        BotRecoveredObjectState::Unknown
    };
    visit_object(&BotRecoveredObject {
        tape_file_number: entry.tape_file_number,
        stored_block_count: entry.block_count,
        object_id,
        state,
    })
    .map_err(|message| BotStructuralRecoveryError::Visitor { message })
}

fn authority_conflict(
    tape_file_number: u64,
    detail: impl Into<String>,
) -> BotStructuralRecoveryError {
    BotStructuralRecoveryError::ConflictingObjectAuthority {
        tape_file_number,
        detail: detail.into(),
    }
}

pub(crate) fn reject_readable_foreign_bot_bootstrap(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<(), BotStructuralRecoveryError> {
    source
        .configure_fixed_block_size(block_size)
        .map_err(|error| bot_bootstrap_source_error("configure fixed block size", error))?;
    source
        .locate_physical(PhysicalPositionHint::new(0))
        .map_err(|error| bot_bootstrap_source_error("locate BOT Bootstrap", error))?;
    let block_size = usize::try_from(block_size).map_err(|_| {
        BotStructuralRecoveryError::ArithmeticOverflow {
            context: "BOT Bootstrap probe block size",
        }
    })?;
    let mut block = vec![0; block_size];
    let bytes = match source.read_record(&mut block) {
        Ok(RawReadOutcome::Block { bytes, .. }) => bytes,
        Ok(RawReadOutcome::Filemark { .. } | RawReadOutcome::EndOfData { .. }) => return Ok(()),
        Err(error) if bot_source_error_is_medium_damage(&error) => return Ok(()),
        Err(error) => return Err(bot_bootstrap_source_error("read BOT Bootstrap", error)),
    };
    if bytes != block_size {
        return Ok(());
    }
    if let Ok(payload) = parse_bootstrap_block(&block) {
        if payload.tape_uuid != *tape_uuid {
            return Err(BotStructuralRecoveryError::TapeIdentityMismatch);
        }
    }
    Ok(())
}

fn bot_bootstrap_source_error(
    operation: &'static str,
    error: ParityError,
) -> BotStructuralRecoveryError {
    BotStructuralRecoveryError::Scan {
        message: format!("{operation}: {error}"),
    }
}

fn bot_source_error_is_medium_damage(error: &ParityError) -> bool {
    matches!(
        error,
        ParityError::TapeIo(TapeIoError::CheckCondition(
            remanence_library::scsi::ScsiError::CheckCondition { sense, .. },
        )) if decode_sense(sense).is_some_and(|decoded| decoded.key == 0x03)
    )
}

fn checked_recovery_increment(
    value: u64,
    context: &'static str,
) -> Result<u64, BotStructuralRecoveryError> {
    value
        .checked_add(1)
        .ok_or(BotStructuralRecoveryError::ArithmeticOverflow { context })
}
