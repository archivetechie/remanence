//! Cross-source validation of native crash-recovered endings. The native fold
//! validates its continuation group; this join proves that the cited library
//! decision accepted exactly that tape/session and the same streamed preview.
use super::{
    authority::AppliedLibraryRecord,
    native_view::NativeTapeView,
    store::{Replay, StoreError},
    TapeRef,
};
use crate::{
    tape_log::{Record, TailFile, TailFilemark, TailIdentity},
    StateError,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The existing recovery-preview encoding, shared by the spool producer and
/// replay validator. Synchronization annotations are established separately;
/// the preview commits file identity, extent bounds and filemark uncertainty.
pub fn write_preview_file(w: &mut impl std::io::Write, f: &TailFile) -> std::io::Result<()> {
    for n in [f.tape_file_number, f.known_blocks, f.unknown_blocks] {
        w.write_all(&n.to_le_bytes())?;
    }
    w.write_all(&[match f.filemark {
        TailFilemark::Present => 0,
        TailFilemark::Absent => 1,
        TailFilemark::Unknown => 2,
    }])?;
    match &f.identity {
        TailIdentity::Object(id) => {
            w.write_all(&[0])?;
            w.write_all(&(id.len() as u32).to_le_bytes())?;
            w.write_all(id.as_bytes())
        }
        TailIdentity::Sidecar(hash) => {
            w.write_all(&[1])?;
            w.write_all(hash)
        }
        TailIdentity::TornPrefix(hash) => {
            w.write_all(&[2])?;
            w.write_all(hash)
        }
    }
}
struct HashWriter(Sha256);
impl std::io::Write for HashWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
struct Expected {
    tape: TapeRef,
    session: [u8; 16],
    before: u64,
    count: u64,
    digest: [u8; 32],
}
fn invalid(message: impl Into<String>) -> StateError {
    StateError::JournalReplayFailed(format!("crash acknowledgement: {}", message.into()))
}

/// Validate before projection or physical admission. Only compact outstanding
/// references are retained; native tail payloads are hashed in streaming order.
pub(crate) fn validate(
    view: &NativeTapeView,
    replay: impl FnOnce(
        &mut dyn FnMut(AppliedLibraryRecord) -> Result<(), StoreError>,
    ) -> Result<Replay, StoreError>,
) -> Result<(), StateError> {
    validate_prefixes(view.snapshots().map(|s| (s, None)), replay).map(|_| ())
}

/// Incremental validation of changed native prefixes. Each progress mark is
/// in-memory source validation, never a derived catalog watermark.
pub(crate) fn validate_new(
    prefixes: impl IntoIterator<Item = (crate::tape_log::snapshot::Snapshot, Option<(u64, u64)>)>,
    replay: impl FnOnce(
        &mut dyn FnMut(AppliedLibraryRecord) -> Result<(), StoreError>,
    ) -> Result<Replay, StoreError>,
) -> Result<usize, StateError> {
    validate_prefixes(prefixes.into_iter(), replay)
}
fn validate_prefixes(
    prefixes: impl Iterator<Item = (crate::tape_log::snapshot::Snapshot, Option<(u64, u64)>)>,
    replay: impl FnOnce(
        &mut dyn FnMut(AppliedLibraryRecord) -> Result<(), StoreError>,
    ) -> Result<Replay, StoreError>,
) -> Result<usize, StateError> {
    let mut checked = 0usize;
    let mut expected = BTreeMap::<u64, Vec<Expected>>::new();
    for (native, prior) in prefixes {
        let (through, end) = prior.unwrap_or((0, 0));
        native
            .visit_frames_after(through, end, |frame| {
                checked += 1;
                if let Record::SessionEnded(end) = &frame.record {
                    if let Some(sequence) = end.crash_ack_audit_sequence {
                        let mut hash = HashWriter(Sha256::new());
                        let mut count = 0u64;
                        native.visit_session_tail_group(end, |tail| {
                            write_preview_file(&mut hash, tail)?;
                            count += 1;
                            Ok(())
                        })?;
                        if count != end.tail_file_count {
                            return Err(crate::tape_log::store::StoreError::Refused(
                                "crash acknowledgement tail count differs",
                            ));
                        }
                        expected.entry(sequence).or_default().push(Expected {
                            tape: TapeRef {
                                uuid: native.header().tape_uuid,
                                incarnation: end.incarnation,
                            },
                            session: end.session_id,
                            before: end.tail.first_sequence,
                            count,
                            digest: hash.0.finalize().into(),
                        });
                    }
                }
                Ok(())
            })
            .map_err(|e| invalid(e.to_string()))?;
    }
    if expected.is_empty() {
        return Ok(checked);
    }
    let mut mismatch = None;
    let result = replay(&mut |applied| {
        let Some(claims) = expected.remove(&applied.sequence()) else {
            return Ok(());
        };
        let matches = claims.iter().all(|want| match &applied.record().body {
            super::Body::SupersedeTail(got) => {
                got.at.tape == want.tape
                    && got.at.sequence < want.before
                    && got.session_id == Some(want.session)
                    && got.tail_file_count == want.count
                    && got.preview_digest == Some(want.digest)
            }
            _ => false,
        });
        if !matches {
            mismatch = Some(applied.sequence());
        }
        Ok(())
    })
    .map_err(|e| invalid(e.to_string()))?;
    if result.refusal.is_some() {
        return Err(invalid("library replay is unresolved"));
    }
    if let Some(sequence) = mismatch {
        return Err(invalid(format!(
            "library sequence {sequence} has wrong type, tape, session or preview"
        )));
    }
    if let Some(sequence) = expected.keys().next() {
        return Err(invalid(format!(
            "library sequence {sequence} is absent or excluded"
        )));
    }
    Ok(checked)
}
