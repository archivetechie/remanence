//! Pure media-consistency rules for neutral catalog projection.
use remanence_parity::authority::{FilePublication, FilePurpose, SidecarProvenance};

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct CommitShapeError(String);

#[derive(Debug)]
pub(crate) struct ValidatedCommitLayout<'a> {
    // Retained for the full-record legacy fixture projector; production only
    // needs successful layout validation. Remove with the B4 codec fixtures.
    #[cfg_attr(not(any(test, feature = "fixtures")), allow(dead_code))]
    pub(crate) last_tape_file: &'a remanence_parity::TapeFileEntry,
}

fn barrier_bundle_shape_error(detail: impl Into<String>) -> CommitShapeError {
    CommitShapeError(detail.into())
}

/// Validate the same media geometry and W/T rules for a neutral commit.
/// The host encoder does not participate in catalog projection.
pub(crate) fn validate_parity_barrier_bundles(
    record: &super::AuthorityCommit,
) -> Result<ValidatedCommitLayout<'_>, CommitShapeError> {
    if record.scheme.is_none() {
        return Err(barrier_bundle_shape_error(
            "parity bundle validation requires a parity scheme",
        ));
    }
    if record.object_files.len() != record.payloads.len() {
        return Err(barrier_bundle_shape_error(format!(
            "parity checkpoint has {} object bundles for {} object projections",
            record.object_files.len(),
            record.payloads.len()
        )));
    }
    let mut first_tape_file = None;
    let mut prior_last_tape_file = None;
    let mut highest_protected_ordinal = None;
    let mut total_committed_ordinals = None;

    for (projection, bundle) in record.payloads.iter().zip(&record.object_files) {
        bundle.validate().map_err(|err| {
            barrier_bundle_shape_error(format!("object {}: {err}", projection.object.object_id))
        })?;
        if bundle.purpose != FilePurpose::Object {
            return Err(barrier_bundle_shape_error(format!(
                "object {} uses {:?} bundle kind",
                projection.object.object_id, bundle.purpose
            )));
        }
        let object_entry = bundle.files.first().ok_or_else(|| {
            barrier_bundle_shape_error(format!(
                "object {} bundle is empty",
                projection.object.object_id
            ))
        })?;
        validate_next_bundle_file(
            prior_last_tape_file,
            object_entry,
            "parity checkpoint object bundle",
        )?;
        first_tape_file.get_or_insert(object_entry);

        if object_entry.object_id.as_deref() != Some(projection.object.object_id.as_str())
            || object_entry.tape_file_number != projection.copy.tape_file_number
            || object_entry.block_count != projection.block_count
        {
            return Err(barrier_bundle_shape_error(format!(
                "object {} bundle entry does not match projection geometry",
                projection.object.object_id
            )));
        }
        if object_entry.block_count == 0 {
            return Err(barrier_bundle_shape_error(format!(
                "object {} has zero stored blocks",
                projection.object.object_id
            )));
        }
        let object_first_ordinal = object_entry.first_parity_data_ordinal.ok_or_else(|| {
            barrier_bundle_shape_error(format!(
                "object {} has no first parity data ordinal",
                projection.object.object_id
            ))
        })?;
        let running_total = match total_committed_ordinals {
            Some(total) => total,
            None => {
                let start = bundle_start_watermark(bundle);
                if start > object_first_ordinal {
                    return Err(barrier_bundle_shape_error(
                        "checkpoint protection starts beyond its prior data",
                    ));
                }
                highest_protected_ordinal = Some(start);
                object_first_ordinal
            }
        };
        if object_first_ordinal != running_total {
            return Err(barrier_bundle_shape_error(format!(
                "object {} starts at parity ordinal {}, expected {}",
                projection.object.object_id, object_first_ordinal, running_total
            )));
        }
        let next_total = running_total
            .checked_add(object_entry.block_count)
            .ok_or_else(|| barrier_bundle_shape_error("checkpoint object ordinals overflow u64"))?;
        if bundle.total != next_total || projection.total_committed_ordinals != next_total {
            return Err(barrier_bundle_shape_error(format!(
                "object {} ends at ordinal {next_total}, but bundle/projection report {}/{}",
                projection.object.object_id, bundle.total, projection.total_committed_ordinals
            )));
        }
        let next_highest = validate_sidecar_watermark_transition(
            highest_protected_ordinal.expect("set with first object total"),
            next_total,
            bundle,
        )?;
        if bundle.protected != next_highest {
            return Err(barrier_bundle_shape_error(format!(
                "object {} bundle reports W={}, expected {next_highest} from its sidecars",
                projection.object.object_id, bundle.protected
            )));
        }
        let scheme = record.scheme.as_ref().expect("parity scheme checked");
        let epoch_blocks =
            u64::from(scheme.data_blocks_per_stripe) * u64::from(scheme.stripes_per_neighborhood);
        if next_total
            .checked_sub(next_highest)
            .is_none_or(|tail| tail >= epoch_blocks)
        {
            return Err(barrier_bundle_shape_error(format!(
                "object {} exceeds the open epoch bound: W={next_highest}, T={next_total}",
                projection.object.object_id
            )));
        }
        highest_protected_ordinal = Some(next_highest);
        total_committed_ordinals = Some(next_total);
        prior_last_tape_file = bundle.files.last();
    }

    if first_tape_file.is_none() {
        let bundle = record
            .barrier
            .as_ref()
            .ok_or_else(|| barrier_bundle_shape_error("empty checkpoint has no sidecar bundle"))?;
        first_tape_file = bundle.files.first();
        total_committed_ordinals = Some(bundle.total);
        let start = bundle_start_watermark(bundle);
        highest_protected_ordinal = Some(start);
    }
    let _first_tape_file = first_tape_file
        .ok_or_else(|| barrier_bundle_shape_error("checkpoint contains no tape files"))?;
    let total_committed_ordinals = total_committed_ordinals.expect("checkpoint has data watermark");
    let mut final_highest = highest_protected_ordinal.expect("checkpoint has protection watermark");
    if let Some(barrier_bundle) = record.barrier.as_ref() {
        barrier_bundle
            .validate()
            .map_err(|err| barrier_bundle_shape_error(format!("checkpoint barrier: {err}")))?;
        if barrier_bundle.purpose != FilePurpose::Sidecars(SidecarProvenance::Fresh) {
            return Err(barrier_bundle_shape_error(format!(
                "checkpoint barrier uses {:?} bundle kind",
                barrier_bundle.purpose
            )));
        }
        let barrier_first = barrier_bundle.files.first().ok_or_else(|| {
            barrier_bundle_shape_error(
                "an exact-boundary parity checkpoint must omit its empty barrier bundle",
            )
        })?;
        validate_next_bundle_file(
            prior_last_tape_file,
            barrier_first,
            "parity checkpoint barrier bundle",
        )?;
        if barrier_bundle.total != total_committed_ordinals {
            return Err(barrier_bundle_shape_error(format!(
                "checkpoint barrier reports T={}, expected {total_committed_ordinals}",
                barrier_bundle.total
            )));
        }
        final_highest = validate_sidecar_watermark_transition(
            final_highest,
            total_committed_ordinals,
            barrier_bundle,
        )?;
        if barrier_bundle.protected != final_highest {
            return Err(barrier_bundle_shape_error(format!(
                "checkpoint barrier reports W={}, expected {final_highest} from its sidecars",
                barrier_bundle.protected
            )));
        }
        prior_last_tape_file = barrier_bundle.files.last();
    }
    let scheme = record.scheme.as_ref().expect("parity scheme checked");
    let epoch_blocks =
        u64::from(scheme.data_blocks_per_stripe) * u64::from(scheme.stripes_per_neighborhood);
    if total_committed_ordinals
        .checked_sub(final_highest)
        .is_none_or(|tail| tail >= epoch_blocks)
    {
        return Err(barrier_bundle_shape_error(format!(
            "checkpoint exceeds the open epoch bound: W={final_highest}, T={total_committed_ordinals}"
        )));
    }
    let last_tape_file = prior_last_tape_file.expect("validated checkpoint has an Object row");
    let expected_next_tape_file = last_tape_file
        .tape_file_number
        .checked_add(1)
        .ok_or_else(|| barrier_bundle_shape_error("checkpoint next tape-file number overflows"))?;
    if record.next_file != expected_next_tape_file {
        return Err(barrier_bundle_shape_error(format!(
            "checkpoint next tape-file number is {}, expected {expected_next_tape_file}",
            record.next_file
        )));
    }

    Ok(ValidatedCommitLayout { last_tape_file })
}

fn validate_next_bundle_file(
    prior_last: Option<&remanence_parity::TapeFileEntry>,
    next_first: &remanence_parity::TapeFileEntry,
    context: &str,
) -> Result<(), CommitShapeError> {
    let Some(prior_last) = prior_last else {
        return Ok(());
    };
    let expected = prior_last
        .tape_file_number
        .checked_add(1)
        .ok_or_else(|| barrier_bundle_shape_error("checkpoint tape-file number overflows u64"))?;
    if next_first.tape_file_number != expected {
        return Err(barrier_bundle_shape_error(format!(
            "{context} starts at tape file {}, expected {expected}",
            next_first.tape_file_number
        )));
    }
    Ok(())
}

/// Recover the protection watermark before the first sidecar in a bundle.
fn bundle_start_watermark(bundle: &FilePublication) -> u64 {
    bundle
        .files
        .iter()
        .find(|entry| entry.kind == remanence_parity::TapeFileKind::ParitySidecar)
        .and_then(|entry| entry.protected_ordinal_start)
        .unwrap_or(bundle.protected)
}

pub(crate) fn validate_sidecar_watermark_transition(
    mut highest_protected_ordinal: u64,
    total_committed_ordinals: u64,
    bundle: &FilePublication,
) -> Result<u64, CommitShapeError> {
    for sidecar in bundle
        .files
        .iter()
        .filter(|entry| entry.kind == remanence_parity::TapeFileKind::ParitySidecar)
    {
        let start = sidecar.protected_ordinal_start.ok_or_else(|| {
            barrier_bundle_shape_error(format!(
                "ParitySidecar at tape file {} has no protected range start",
                sidecar.tape_file_number
            ))
        })?;
        let end = sidecar.protected_ordinal_end_exclusive.ok_or_else(|| {
            barrier_bundle_shape_error(format!(
                "ParitySidecar at tape file {} has no protected range end",
                sidecar.tape_file_number
            ))
        })?;
        if start != highest_protected_ordinal || end <= start || end > total_committed_ordinals {
            return Err(barrier_bundle_shape_error(format!(
                "ParitySidecar at tape file {} protects [{start}, {end}), expected a non-empty range starting at {highest_protected_ordinal} and ending no later than {total_committed_ordinals}",
                sidecar.tape_file_number
            )));
        }
        highest_protected_ordinal = end;
    }
    Ok(highest_protected_ordinal)
}
