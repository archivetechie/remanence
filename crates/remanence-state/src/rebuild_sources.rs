//! Read-only completeness check before replacing the catalog. Received library
//! records prove source presence even when native evidence excludes their application.
//! Catalog witnesses and tolerated probe omissions must survive a refused rebuild.
use crate::{
    library_log::{self as lib, native_view::DroppedTapeLog},
    projector::apply::{library_source_digest, StoredLibraryRecord},
    CatalogIndex, StateError, StatePaths,
};
use std::collections::BTreeMap;

/// Capture only source digests; decoded log history is not needed by the comparison.
pub(crate) fn observe_received(
    records: &mut BTreeMap<u64, [u8; 32]>,
    record: &lib::Record,
    receipt: &lib::Receipt,
) -> Result<(), lib::store::StoreError> {
    let digest = library_source_digest(receipt.sequence, receipt.record_hash, record)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    records.insert(receipt.sequence, digest);
    Ok(())
}

/// Evaluate every completeness clause before returning one combined refusal.
/// True means the catalog supplied no witness; only the offline caller reports that fact.
pub(crate) fn rebuild_sources_complete(
    index: &mut CatalogIndex,
    received: &BTreeMap<u64, [u8; 32]>,
    last: u64,
    dropped: &[DroppedTapeLog],
    paths: Option<&StatePaths>,
) -> Result<bool, StateError> {
    let catalog = index.library_catalog_evidence()?;
    let no_witness = catalog.rows.is_empty() && catalog.received == 0;
    let end = catalog
        .rows
        .last()
        .map_or(0, |r| r.sequence)
        .max(catalog.received);
    // Count the holes without iterating an arbitrarily large received-position gap.
    // Diagnostic rows count as present here, while remaining exempt from comparisons.
    let missing = catalog.received.saturating_sub(last)
        - catalog
            .rows
            .iter()
            .filter(|r| r.sequence > last && r.sequence <= catalog.received)
            .count() as u64;
    let mut standing = Vec::new();
    let mut other = BTreeMap::<lib::Tag, u64>::new();
    for row in &catalog.rows {
        if row.tag == lib::Tag::Diagnostic as u16 {
            continue;
        }
        let reason = if row.sequence > last {
            "not in the library log".to_owned()
        } else if received.get(&row.sequence) != Some(&row.digest) {
            format!("differs from the library log's record {}", row.sequence)
        } else {
            continue;
        };
        let record = decode(row)?;
        let tape = match record.body_view() {
            lib::BodyView::AssignPool(v) => Some(Some(v.tape.uuid)),
            lib::BodyView::Label(v) => Some(Some(v.tape.uuid)),
            lib::BodyView::Retire(v) => Some(Some(v.tape.uuid)),
            lib::BodyView::Reprovision(v) => Some(Some(v.tape.uuid)),
            lib::BodyView::AcceptUnprotectedFinalization(v) => Some(Some(v.at.tape.uuid)),
            lib::BodyView::ManualFinalizationAccepted(v) => Some(Some(v.at.tape.uuid)),
            lib::BodyView::VerificationVerdict(v) => Some(Some(v.tape.uuid)),
            lib::BodyView::FenceRaised(v) => Some(fence_tape(v)),
            lib::BodyView::FenceReleased(v) => {
                let raise = catalog
                    .rows
                    .binary_search_by_key(&v.fence, |r| r.sequence)
                    .ok()
                    .map(|i| decode(&catalog.rows[i]))
                    .transpose()?;
                Some(raise.as_ref().and_then(|r| match r.body_view() {
                    lib::BodyView::FenceRaised(v) => fence_tape(v),
                    _ => None,
                }))
            }
            _ => None,
        };
        if let Some(tape) = tape {
            let tape = tape.map_or_else(|| "-".into(), |id| uuid::Uuid::from_bytes(id).to_string());
            standing.push(format!(
                "  record {} {:?} tape {tape} at {}: {reason}",
                row.sequence,
                record.tag(),
                record_time(record.envelope.timestamp_unix_ns)?
            ));
        } else {
            *other.entry(record.tag()).or_default() += 1;
        }
    }
    let mut findings = Vec::new();
    if !standing.is_empty() || !other.is_empty() || missing != 0 {
        findings.push(format!("RebuildWouldDropLibraryRecords: the catalog holds library records that the library log does not (library log ends at record {last}, catalog at record {end}):"));
        findings.extend(standing);
        findings.extend(
            other
                .into_iter()
                .map(|(kind, count)| format!("  and {count} {kind:?} records")),
        );
        if missing != 0 {
            findings.push(format!(
                "  and {missing} records received by the catalog with no catalog row"
            ));
        }
        findings.push("Put back the library log this catalog was built from and run again. If that log no longer exists, keep the state directory and the catalog as they are: they hold the only copy of these records, and this release cannot write them back. The declared reset is the only way to run sooner, and it erases them.".into());
    }
    let mut marker_only = false;
    for entry in dropped {
        let tape = uuid::Uuid::from_bytes(entry.tape);
        if let Some(error) = &entry.error {
            findings.push(format!("RebuildWouldDropTapeLog: tape {tape} log {}: {error}; restore that tape log from a copy and run again, or keep the state directory as it is", entry.path.display()));
        } else {
            marker_only = true;
            findings.push(format!(
                "RebuildWouldDropTapeLog: tape {tape} has only an ambiguity marker {}",
                entry.path.display()
            ));
        }
    }
    if marker_only {
        if let Some(hint) = paths.and_then(crate::ambiguity_recovery::startup_hint) {
            findings.push(hint);
        }
    }
    if findings.is_empty() {
        Ok(no_witness)
    } else {
        Err(StateError::ConfigInvalid(findings.join("\n")))
    }
}

fn decode(row: &StoredLibraryRecord) -> Result<lib::Record, StateError> {
    let mut decoder = lib::codec::Decoder::at(row.bytes.as_slice(), row.sequence, 0);
    let frame = decoder
        .next_frame()
        .map_err(|e| StateError::IndexCorrupt(e.to_string()))?
        .ok_or_else(|| StateError::IndexCorrupt("library rebuild witness has no record".into()))?;
    if frame.sequence != row.sequence || frame.record.tag() as u16 != row.tag {
        return Err(StateError::IndexCorrupt(
            "library rebuild witness identity differs".into(),
        ));
    }
    Ok(frame.record.clone())
}
fn fence_tape(raise: &lib::FenceRaised) -> Option<lib::Id> {
    match &raise.scope {
        lib::FenceScope::Tape(tape) => Some(tape.uuid),
        lib::FenceScope::Drive(_) | lib::FenceScope::Model { .. } => None,
    }
}
/// The envelope's own UTC timestamp is shared by refusal lines and the no-witness note.
pub(crate) fn record_time(timestamp: u64) -> Result<String, StateError> {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp))
        .map_err(|e| StateError::IndexCorrupt(e.to_string()))?
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| StateError::IndexCorrupt(e.to_string()))
}
