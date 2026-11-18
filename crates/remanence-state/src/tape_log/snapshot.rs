//! Immutable inode plus frozen byte end. Capture performs only open/stat/header
//! reads under the lock. Validation and every cursor run after releasing it;
//! positional I/O makes nested visitors independent even on a cloned descriptor.
use super::{
    codec::{CodecError, Decoder, Frame, Tail},
    fold::{Fold, TapeSummary},
    store::{Header, Store, StoreError},
    *,
};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::{FileExt, MetadataExt},
    sync::{Arc, Mutex},
};

pub(super) type PlanBase = Option<(u64, Arc<TapeSummary>)>;
type ValidatedPrefix = (
    TapeSummary,
    Tail,
    ReplayMetrics,
    PlanBase,
    BTreeMap<u64, u64>,
);

const POSITION_INDEX_INTERVAL: u64 = 1024 * 1024;

#[cfg(any(test, feature = "fixtures"))]
static POSITION_INDEX_INTERVAL_OVERRIDE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(POSITION_INDEX_INTERVAL);

/// Set the process-wide bookmark interval for an isolated fixture. Call before
/// constructing any snapshots; production builds always use one MiB.
#[cfg(any(test, feature = "fixtures"))]
pub fn set_position_index_interval(bytes: u64) {
    assert!(bytes > 0, "position index interval must be positive");
    POSITION_INDEX_INTERVAL_OVERRIDE.store(bytes, std::sync::atomic::Ordering::Relaxed);
}

/// Read the same interval for bookmark spacing and full-pass accounting.
fn position_index_interval() -> u64 {
    #[cfg(any(test, feature = "fixtures"))]
    {
        POSITION_INDEX_INTERVAL_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(any(test, feature = "fixtures")))]
    {
        POSITION_INDEX_INTERVAL
    }
}

/// Sparse frame boundaries for one pinned file. This is a leaf lock: copy or
/// insert only, never hold it across I/O or acquisition of another lock.
pub(super) type PositionIndex = Arc<Mutex<BTreeMap<u64, u64>>>;

/// Keep the first frame at least one interval beyond the previous bookmark.
/// The log/decode start is the implicit first bookmark, even in an empty map.
pub(super) fn seed_position(
    entries: &mut BTreeMap<u64, u64>,
    previous: &mut u64,
    sequence: u64,
    offset: u64,
) {
    if offset.saturating_sub(*previous) >= position_index_interval() {
        entries.insert(sequence, offset);
        *previous = offset;
    }
}

/// Unvalidated capture, deliberately not an authority token.
#[derive(Clone)]
pub(super) struct Captured {
    file: Option<Arc<File>>,
    identity: Option<(u64, u64)>,
    end: u64,
    start: u64,
    header: Header,
    capture_time: std::time::Duration,
}
impl Captured {
    pub(super) fn open(store: &Store) -> Result<Self, StoreError> {
        let started = std::time::Instant::now();
        let file = match File::open(&store.path) {
            Ok(file) => Some(Arc::new(file)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let metadata = file.as_ref().map(|f| f.metadata()).transpose()?;
        let end = metadata.as_ref().map_or(0, |m| m.len());
        let identity = metadata.as_ref().map(|m| (m.dev(), m.ino()));
        let start = if let Some(file) = &file {
            Header::read(
                &mut PrefixReader {
                    file: file.clone(),
                    position: 0,
                    end,
                },
                &store.header,
            )?
        } else {
            // Use exactly the same encoded header length for the initial spool.
            let mut bytes = Vec::new();
            use super::wire::Wire;
            store.header.scheme.put(&mut bytes)?;
            50 + bytes.len() as u64
        };
        Ok(Self {
            file,
            identity,
            end,
            start,
            header: store.header.clone(),
            capture_time: started.elapsed(),
        })
    }
    fn fold(&self) -> Result<ValidatedPrefix, StoreError> {
        let mut initial = TapeSummary::default();
        initial.closed_end = self.start;
        let mut fold = Fold::from_validated(initial);
        let mut metrics = ReplayMetrics {
            validation_passes: 1,
            ..ReplayMetrics::default()
        };
        let mut plan_base = None;
        let mut positions = BTreeMap::new();
        let mut previous = self.start;
        if let Some(file) = &self.file {
            let mut decoder = Decoder::at(
                PrefixReader {
                    file: file.clone(),
                    position: self.start,
                    end: self.end,
                },
                1,
                self.start,
            );
            while let Some(frame) = decoder.next_frame()? {
                if let Record::Initialized(init) = &frame.record {
                    if init.tape_uuid != self.header.tape_uuid
                        || init.block_size != self.header.block_size
                        || init.scheme != self.header.scheme
                    {
                        return Err(StoreError::Header);
                    }
                }
                seed_position(&mut positions, &mut previous, frame.sequence, frame.offset);
                metrics.frames += 1;
                metrics.structural_bytes += frame.structural.len() as u64;
                metrics.payload_bytes += frame.payload.len() as u64;
                match &frame.record {
                    Record::FinalizationPrepared(plan) | Record::FinalizationDecided(plan) => {
                        // Keep one validated pre-plan boundary, not commit history.
                        // Terminal construction then uses structural passes only.
                        plan_base =
                            Some((plan.group.first_sequence, Arc::new(fold.summary().clone())));
                    }
                    Record::FinalizationPreparationRetired { .. } => plan_base = None,
                    _ => {}
                }
                fold.push(&frame).map_err(CodecError::from)?;
                metrics.peak_extent_capacity = metrics.peak_extent_capacity.max(
                    fold.retained_extent_capacity()
                        + plan_base
                            .as_ref()
                            .map_or(0, |(_, base)| base.uncovered.capacity()),
                );
                metrics.peak_catalog_bytes = metrics
                    .peak_catalog_bytes
                    .max(fold.retained_catalog_bytes());
                metrics.peak_frame_buffer = metrics
                    .peak_frame_buffer
                    .max(decoder.retained_buffer_bytes());
            }
            metrics.peak_frame_buffer = metrics
                .peak_frame_buffer
                .max(decoder.retained_buffer_bytes());
            let (summary, tail) = fold
                .finish(decoder.tail())
                .map_err(CodecError::EncodeSchema)?;
            positions.retain(|_, offset| *offset < summary.closed_end);
            Ok((summary, tail, metrics, plan_base, positions))
        } else {
            Ok((
                fold.summary().clone(),
                Tail::Complete,
                metrics,
                None,
                positions,
            ))
        }
    }
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub(super) fn inspect_tail(&self) -> Result<super::store::TailInspection, StoreError> {
        #[cfg(any(test, feature = "fixtures"))]
        if self.end > self.start {
            crate::full_pass::count(crate::full_pass::PassKind::TapeValidate);
        }
        let file = self
            .file
            .as_ref()
            .ok_or(StoreError::Refused("no log to repair"))?;
        let mut initial = TapeSummary::default();
        initial.closed_end = self.start;
        let mut fold = Fold::from_validated(initial);
        let mut decoder = Decoder::at(
            PrefixReader {
                file: file.clone(),
                position: self.start,
                end: self.end,
            },
            1,
            self.start,
        );
        let (summary, damage) = loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    if let Record::Initialized(init) = &frame.record {
                        if init.tape_uuid != self.header.tape_uuid
                            || init.block_size != self.header.block_size
                            || init.scheme != self.header.scheme
                        {
                            return Err(StoreError::Header);
                        }
                    }
                    if let Err(error) = fold.push(&frame) {
                        break (fold.summary().clone(), Some(error.to_string()));
                    }
                }
                Ok(None) => {
                    let (summary, tail) = fold
                        .finish(decoder.tail())
                        .map_err(CodecError::EncodeSchema)?;
                    break (
                        summary,
                        (tail != Tail::Complete).then(|| format!("{tail:?}")),
                    );
                }
                Err(CodecError::Recovery(error)) => {
                    break (fold.summary().clone(), Some(error.to_string()))
                }
                Err(error) => return Err(error.into()),
            }
        };
        Ok(super::store::TailInspection {
            through_sequence: summary.last_closed_sequence,
            valid_bytes: summary.closed_end,
            original_bytes: self.end,
            damage,
        })
    }
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub(super) fn validate_through(&self, through: u64) -> Result<(TapeSummary, u64), StoreError> {
        if through == 0 {
            return Err(StoreError::Refused(
                "repair must retain a closed initialization",
            ));
        }
        let inspection = self.inspect_tail()?;
        if inspection.damage.is_none() {
            return Err(StoreError::Refused(
                "log has no damaged or provisional suffix",
            ));
        }
        if inspection.through_sequence != through {
            return Err(StoreError::Refused(
                "repair must retain the last validated closed sequence",
            ));
        }
        // Validate only the retained, already checked prefix for its summary.
        let mut prefix = self.clone();
        prefix.end = inspection.valid_bytes;
        let snapshot = prefix.validate()?;
        let summary = snapshot.summary().clone();
        if summary.last_closed_sequence != through {
            return Err(StoreError::Refused(
                "repair must retain the last validated closed sequence",
            ));
        }
        let end = summary.closed_end;
        Ok((summary, end))
    }
    // Only the L2 repair path (ruling 32) calls this in production; test-only until then.
    #[cfg(test)]
    pub(super) fn repair_prefix(
        mut self,
        through: u64,
        resuming: bool,
    ) -> Result<Snapshot, StoreError> {
        if resuming {
            if let Ok(snapshot) = self.clone().validate() {
                if snapshot.summary.last_closed_sequence == through
                    && snapshot.tail == Tail::Complete
                {
                    return Ok(snapshot);
                }
            }
        }
        let (_, end) = self.validate_through(through)?;
        self.end = end;
        self.validate()
    }
    /// The exclusive owner has already fully validated and incrementally folded
    /// these bytes. Reuse that proof without a catalog scan under its lock.
    pub(super) fn trusted(
        self,
        summary: TapeSummary,
        tail: Tail,
        plan_base: PlanBase,
        position_index: PositionIndex,
    ) -> Snapshot {
        Snapshot {
            file: self.file,
            identity: self.identity,
            prefix_hash: Arc::new(Mutex::new(None)),
            position_index,
            first_sequence: 1,
            start: self.start,
            captured_end: self.end,
            summary,
            tail,
            header: self.header,
            capture_time: self.capture_time,
            metrics: ReplayMetrics::default(),
            plan_base,
        }
    }
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub(super) fn validate(self) -> Result<Snapshot, StoreError> {
        #[cfg(any(test, feature = "fixtures"))]
        if self.end > self.start {
            crate::full_pass::count(crate::full_pass::PassKind::TapeValidate);
        }
        let (summary, tail, metrics, plan_base, positions) = self.fold()?;
        let position_index = Arc::new(Mutex::new(positions));
        if self.file.is_some()
            && summary.is_fresh()
            && tail == Tail::Complete
            && self.end != self.start
        {
            return Err(StoreError::Refused(
                "active log has no closed initialization",
            ));
        }
        Ok(Snapshot {
            file: self.file,
            identity: self.identity,
            prefix_hash: Arc::new(Mutex::new(None)),
            position_index,
            first_sequence: 1,
            start: self.start,
            captured_end: self.end,
            summary,
            tail,
            header: self.header,
            capture_time: self.capture_time,
            metrics,
            plan_base,
        })
    }
}
/// Measurements of the one full trust-validation pass, excluding later cursors.
#[derive(Clone, Copy, Debug, Default)]
pub struct ReplayMetrics {
    /// Full trust scans for this capture; zero when an exclusive owner lends its
    /// already validated retained fold, one for an external snapshot.
    pub validation_passes: u64,
    /// Complete frames decoded, including provisional captured frames.
    pub frames: u64,
    /// Structural CBOR bytes, excluding frame headers/trailers.
    pub structural_bytes: u64,
    /// Catalog continuation bytes.
    pub payload_bytes: u64,
    /// Maximum retained encoded section buffer capacity.
    pub peak_frame_buffer: usize,
    /// Maximum committed plus pending uncovered extent capacity.
    pub peak_extent_capacity: usize,
    /// Maximum incremental catalog-validator heap retained at frame boundaries.
    pub peak_catalog_bytes: usize,
}
/// Exact native counterpart for a dependent library decision. Continuations are
/// never counterparts, even when their enclosing group has closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordEvidence {
    /// Native discriminator, checked by the library fold where required.
    pub kind: Tag,
    /// SHA-256 of the full frame's prefix, structural and payload bytes.
    pub digest: [u8; 32],
}

/// SHA-256 continuation for one validated immutable prefix. The cell's lock is
/// a leaf: copy/install only, never held across I/O or another lock acquisition.
#[derive(Clone, Debug)]
struct PrefixHash {
    end: u64,
    state: sha2::Sha256,
    digest: [u8; 32],
}

/// Fully trust-validated immutable prefix. Provisional rows never escape visitors.
#[derive(Clone, Debug)]
pub struct Snapshot {
    prefix_hash: Arc<Mutex<Option<PrefixHash>>>,
    pub(super) plan_base: PlanBase,
    pub(super) position_index: PositionIndex,
    first_sequence: u64,
    file: Option<Arc<File>>,
    identity: Option<(u64, u64)>,
    start: u64,
    captured_end: u64,
    pub(super) summary: TapeSummary,
    pub(super) tail: Tail,
    pub(super) header: Header,
    metrics: ReplayMetrics,
    capture_time: std::time::Duration,
}
impl Snapshot {
    /// Identity of the pinned native source, used to reject an old inode after
    /// an explicit authority repair atomically replaces the file.
    pub(crate) fn file_identity(&self) -> Result<Option<(u64, u64)>, StoreError> {
        Ok(self.identity)
    }
    /// Keep this snapshot's inode alive for a process-local projection marker.
    pub(crate) fn projection_file(&self) -> Option<Arc<File>> {
        self.file.clone()
    }
    /// Measurements of the full validation pass performed outside the lock.
    pub fn replay_metrics(&self) -> ReplayMetrics {
        self.metrics
    }
    /// Time spent opening, checking the header and pinning the end under lock.
    pub fn capture_time(&self) -> std::time::Duration {
        self.capture_time
    }
    /// Header identity validated before any frame is trusted.
    pub fn header(&self) -> &Header {
        &self.header
    }
    /// Bounded fold including check provenance and finalization references.
    pub fn summary(&self) -> &TapeSummary {
        &self.summary
    }
    /// Hash the exact validated immutable header of an existing log. A factory
    /// for an absent log is not evidence of an empty prefix (ruling 19 / D10).
    pub fn header_digest(&self) -> Result<Option<[u8; 32]>, StoreError> {
        use sha2::{Digest, Sha256};
        let Some(file) = &self.file else {
            return Ok(None);
        };
        let mut reader = PrefixReader {
            file: file.clone(),
            position: 0,
            end: self.start,
        };
        let length = Header::read(&mut reader, &self.header)?;
        let mut bytes = vec![0; length as usize];
        file.read_exact_at(&mut bytes, 0)?;
        Ok(Some(Sha256::digest(bytes).into()))
    }
    /// Read one closing record from this frozen prefix, retaining at most one
    /// frame. A missing sequence or a continuation has no authoritative proof.
    /// Full frames are required: row scans may omit payload bytes. I/O and byte
    /// corruption remain errors, never absence. Acquiring a snapshot does not
    /// replace the writer's obligation to finish a successful native sync before
    /// appending a dependent decision; ambiguous leases cannot yield snapshots.
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub fn record_evidence(&self, sequence: u64) -> Result<Option<RecordEvidence>, StoreError> {
        if sequence == 0 || sequence > self.summary.last_closed_sequence {
            return Ok(None);
        }
        let (first, start) = {
            let positions = self
                .position_index
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            positions
                .range(..=sequence)
                .rev()
                .find(|(_, offset)| **offset >= self.start && **offset < self.summary.closed_end)
                .map(|(&sequence, &offset)| (sequence, offset))
                .unwrap_or((self.first_sequence, self.start))
        };
        let mut positions = BTreeMap::new();
        let mut previous = start;
        let mut answer = Err(StoreError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "native evidence disappeared from a validated snapshot",
        )));
        let mut _reached = self.summary.closed_end;
        if let Some(file) = &self.file {
            let mut decoder = Decoder::at(
                PrefixReader {
                    file: file.clone(),
                    position: start,
                    end: self.summary.closed_end,
                },
                first,
                start,
            );
            while let Some(frame) = decoder.next_frame()? {
                seed_position(&mut positions, &mut previous, frame.sequence, frame.offset);
                if frame.sequence == sequence {
                    _reached = frame.offset;
                    answer = Ok(
                        (!matches!(frame.record, Record::PayloadContinuation(_))).then(|| {
                            RecordEvidence {
                                kind: frame.record.tag(),
                                digest: super::codec::record_digest(&frame),
                            }
                        }),
                    );
                    break;
                }
            }
        }
        #[cfg(any(test, feature = "fixtures"))]
        if _reached.saturating_sub(start) >= position_index_interval() {
            crate::full_pass::count(crate::full_pass::PassKind::TapeFirstFrame);
        }
        if answer.is_ok() {
            self.position_index
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend(positions);
        }
        answer
    }
    /// Original first-frame lookup retained as an independent differential oracle.
    #[cfg(test)]
    pub(super) fn record_evidence_oracle(
        &self,
        sequence: u64,
    ) -> Result<Option<RecordEvidence>, StoreError> {
        if sequence == 0 || sequence > self.summary.last_closed_sequence {
            return Ok(None);
        }
        if let Some(mut decoder) = self.decoder() {
            while let Some(frame) = decoder.next_frame()? {
                if frame.sequence == sequence {
                    return Ok(
                        (!matches!(frame.record, Record::PayloadContinuation(_))).then(|| {
                            RecordEvidence {
                                kind: frame.record.tag(),
                                digest: super::codec::record_digest(&frame),
                            }
                        }),
                    );
                }
            }
        }
        // A validated prefix must contain every sequence through its closed end.
        // A later short read must not turn into a missing-record contradiction.
        Err(StoreError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "native evidence disappeared from a validated snapshot",
        )))
    }
    /// Carry a continuation only between captures of the same tape/header and
    /// pinned inode. Copy out before installing, including when cells are shared.
    pub(crate) fn carry_projection_prefix_hash(&self, prior: &Self) {
        if self.header() != prior.header()
            || !matches!((self.file_identity(), prior.file_identity()),
                (Ok(Some(current)), Ok(Some(previous))) if current == previous)
        {
            return;
        }
        let kept = prior
            .prefix_hash
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(kept) = kept {
            self.install_prefix_hash(kept);
        }
    }

    /// Never replace a continuation with one at an earlier or equal end.
    fn install_prefix_hash(&self, candidate: PrefixHash) {
        let mut kept = self.prefix_hash.lock().unwrap_or_else(|p| p.into_inner());
        if kept.as_ref().is_none_or(|old| old.end < candidate.end) {
            *kept = Some(candidate);
        }
    }

    /// Bind a projection checkpoint to frozen bytes, extending a remembered
    /// SHA-256 state when possible. Older ends still hash from byte zero.
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub(crate) fn projection_prefix_digest(&self, end: u64) -> Result<[u8; 32], StoreError> {
        use sha2::{Digest, Sha256};
        if end > self.summary.closed_end {
            return Err(StoreError::Refused(
                "projection source is shorter than its digest",
            ));
        }
        let kept = self
            .prefix_hash
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let (start, mut state) = match kept {
            Some(kept) if kept.end == end => return Ok(kept.digest),
            Some(kept) if kept.end < end => (kept.end, kept.state),
            _ => (0, Sha256::new()),
        };
        if end != 0 {
            let file = self
                .file
                .as_ref()
                .ok_or(StoreError::Refused("projection source file absent"))?;
            #[cfg(any(test, feature = "fixtures"))]
            if start == 0 && end > self.start {
                crate::full_pass::count(crate::full_pass::PassKind::TapePrefixHash);
            }
            let mut reader = PrefixReader {
                file: file.clone(),
                position: start,
                end,
            };
            let mut bytes = [0u8; 65536];
            loop {
                let n = reader.read(&mut bytes)?;
                if n == 0 {
                    break;
                }
                state.update(&bytes[..n]);
            }
            if reader.position != end {
                return Err(StoreError::Refused(
                    "projection source is shorter than its digest",
                ));
            }
        }
        let digest = state.clone().finalize().into();
        self.install_prefix_hash(PrefixHash { end, state, digest });
        Ok(digest)
    }
    /// Original byte-zero hash retained for differential tests.
    #[cfg(test)]
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub(crate) fn projection_prefix_digest_oracle(&self, end: u64) -> Result<[u8; 32], StoreError> {
        use sha2::{Digest, Sha256};
        if end > self.summary.closed_end {
            return Err(StoreError::Refused(
                "projection source is shorter than its digest",
            ));
        }
        let mut digest = Sha256::new();
        if end != 0 {
            let file = self
                .file
                .as_ref()
                .ok_or(StoreError::Refused("projection source file absent"))?;
            #[cfg(any(test, feature = "fixtures"))]
            if end > self.start {
                crate::full_pass::count(crate::full_pass::PassKind::TapePrefixHash);
            }
            let mut reader = PrefixReader {
                file: file.clone(),
                position: 0,
                end,
            };
            let mut bytes = [0u8; 65536];
            loop {
                let n = reader.read(&mut bytes)?;
                if n == 0 {
                    break;
                }
                digest.update(&bytes[..n]);
            }
        }
        Ok(digest.finalize().into())
    }
    /// Classification of captured bytes after the last closed record.
    pub fn tail(&self) -> Tail {
        self.tail
    }
    /// Byte end pinned under the lock, before validation outside it.
    pub fn captured_end(&self) -> u64 {
        self.captured_end
    }
    /// Stream the latest end's named tail from this validated frozen prefix.
    /// The bounded summary holds only its group reference, never these files.
    pub fn visit_last_session_tail(
        &self,
        visitor: impl FnMut(&TailFile) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let Some(end) = &self.summary.last_session_end else {
            return Ok(());
        };
        self.visit_session_tail_group(end, visitor)
    }
    /// Stream an ending's already validated group, including an EOD refinement
    /// that names the earlier ending's exact original tail reference.
    pub(crate) fn visit_session_tail_group(
        &self,
        end: &super::SessionEnded,
        mut visitor: impl FnMut(&TailFile) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        // The frozen-prefix validation proved this offset and count. Seek
        // directly to the named group without rescanning the tape's history.
        let mut decoder = Decoder::at(
            PrefixReader {
                file: file.clone(),
                position: end.tail.start_offset,
                end: self.summary.closed_end,
            },
            end.tail.first_sequence,
            end.tail.start_offset,
        );
        for _ in 0..end.tail.frame_count {
            let frame = decoder
                .next_row_frame()?
                .ok_or(StoreError::Refused("session tail missing frame"))?;
            let Record::PayloadContinuation(Continuation {
                chunk: Chunk::TailFile(file),
                ..
            }) = &frame.record
            else {
                return Err(StoreError::Refused("session tail has non-tail frame"));
            };
            visitor(file)?;
        }
        Ok(())
    }
    /// Run a structural pass over the already validated closed prefix, skipping
    /// catalog bytes. Directory/prefix planning rows are not durable media rows.
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub fn visit_file_rows(
        &self,
        mut visitor: impl FnMut(&TapeFileEntry) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let Some(mut decoder) = self.decoder() else {
            return Ok(());
        };
        // Plan groups contain hypothetical prefix rows: identify their closer
        // with a structural pass before exposing any of their rows.
        let mut boundary = self.decoder().expect("same file");
        let mut plan = false;
        let mut group_end = self.start;
        while let Some(frame) = decoder.next_row_frame()? {
            if frame.offset >= group_end {
                while let Some(close) = boundary.next_row_frame()? {
                    if !matches!(close.record, Record::PayloadContinuation(_)) {
                        plan = matches!(
                            close.record,
                            Record::FinalizationPrepared(_) | Record::FinalizationDecided(_)
                        );
                        group_end = close.end;
                        break;
                    }
                }
            }
            if plan {
                continue;
            }
            match &frame.record {
                Record::Initialized(init) => visitor(&init.bot)?,
                Record::PayloadContinuation(Continuation {
                    chunk: Chunk::FileRow(row),
                    ..
                }) => visitor(row)?,
                _ => {}
            }
        }
        Ok(())
    }
    /// Full validated-frame visitor for payload/metadata adapters. Only closed
    /// groups are included; clients distinguish plan rows from physical rows.
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub fn visit_frames(
        &self,
        mut visitor: impl FnMut(&Frame<'_>) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        if let Some(mut decoder) = self.decoder() {
            while let Some(frame) = decoder.next_frame()? {
                visitor(&frame)?;
            }
        }
        Ok(())
    }
    /// Visit only frames appended after a prefix already validated by the same
    /// process owner. Explicit repair must discard that owner's progress mark.
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub(crate) fn visit_frames_after(
        &self,
        through: u64,
        end: u64,
        mut visitor: impl FnMut(&Frame<'_>) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        if through == 0 {
            return self.visit_frames(visitor);
        }
        if through > self.summary.last_closed_sequence
            || end > self.summary.closed_end
            || end < self.start
        {
            return Err(StoreError::Refused(
                "invalid native acknowledgement progress",
            ));
        }
        if through == self.summary.last_closed_sequence {
            return if end == self.summary.closed_end {
                Ok(())
            } else {
                Err(StoreError::Refused(
                    "native acknowledgement progress mismatch",
                ))
            };
        }
        let file = self
            .file
            .as_ref()
            .ok_or(StoreError::Refused("missing native acknowledgement prefix"))?;
        let mut decoder = Decoder::at(
            PrefixReader {
                file: file.clone(),
                position: end,
                end: self.summary.closed_end,
            },
            through + 1,
            end,
        );
        while let Some(frame) = decoder.next_frame()? {
            visitor(&frame)?;
        }
        Ok(())
    }

    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub(crate) fn before(&self, sequence: u64) -> Result<Self, StoreError> {
        if let Some((first, summary)) = &self.plan_base {
            if *first == sequence {
                let mut result = self.clone();
                result.summary = (**summary).clone();
                result.captured_end = result.summary.closed_end;
                result.tail = Tail::Complete;
                result.plan_base = None;
                return Ok(result);
            }
        }
        let mut summary = TapeSummary::default();
        summary.closed_end = self.start;
        let mut fold = Fold::from_validated(summary);
        if let Some(mut decoder) = self.decoder() {
            while let Some(frame) = decoder.next_frame()? {
                if frame.sequence >= sequence {
                    break;
                }
                fold.push(&frame).map_err(CodecError::from)?;
            }
        }
        let mut result = self.clone();
        result.summary = fold.summary().clone();
        result.captured_end = result.summary.closed_end;
        result.tail = Tail::Complete;
        Ok(result)
    }
    pub(crate) fn group_snapshot(&self, sequence: u64, start: u64, end: u64) -> Self {
        let mut result = self.clone();
        result.start = start;
        result.first_sequence = sequence;
        result.summary.closed_end = end;
        result
    }
    /// Seek directly to the projection's closing frame. Full snapshot validation
    /// already proved the prefix; this checks the mark without replaying it.
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub(crate) fn projection_decoder(
        &self,
        sequence: u64,
        offset: u64,
    ) -> Result<Option<Decoder<PrefixReader>>, StoreError> {
        if sequence == 0 {
            return Ok(self.decoder());
        }
        let file = self
            .file
            .as_ref()
            .ok_or(StoreError::Refused(super::store::BELOW_PROJECTION_MARK))?;
        if sequence > self.summary.last_closed_sequence
            || offset < self.start
            || offset >= self.summary.closed_end
        {
            return Err(StoreError::Refused(super::store::PROJECTION_MARK_MISMATCH));
        }
        let mut decoder = Decoder::at(
            PrefixReader {
                file: file.clone(),
                position: offset,
                end: self.summary.closed_end,
            },
            sequence,
            offset,
        );
        // The prefix is validated, so bytes that fail to decode here mean the
        // mark itself is wrong, not the log. A transport failure is not evidence.
        let frame = decoder
            .next_row_frame()
            .map_err(|error| match error {
                CodecError::Recovery(_) => StoreError::ProjectionMarkInvalid(format!(
                    "sequence {sequence} at offset {offset}: {error}"
                )),
                other => other.into(),
            })?
            .ok_or(StoreError::Refused(
                super::store::PROJECTION_MARK_NOT_CLOSING,
            ))?;
        if matches!(frame.record, Record::PayloadContinuation(_)) {
            return Err(StoreError::Refused(
                super::store::PROJECTION_MARK_NOT_CLOSING,
            ));
        }
        Ok(Some(decoder))
    }
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub(crate) fn decoder(&self) -> Option<Decoder<PrefixReader>> {
        #[cfg(any(test, feature = "fixtures"))]
        if self.file.is_some() && self.first_sequence == 1 && self.start < self.summary.closed_end {
            crate::full_pass::count(crate::full_pass::PassKind::TapeFirstFrame);
        }
        self.file.as_ref().map(|file| {
            Decoder::at(
                PrefixReader {
                    file: file.clone(),
                    position: self.start,
                    end: self.summary.closed_end,
                },
                self.first_sequence,
                self.start,
            )
        })
    }
}
/// Independent positional cursor, bounded before every read and seek.
#[derive(Debug)]
pub(crate) struct PrefixReader {
    file: Arc<File>,
    position: u64,
    end: u64,
}
impl Read for PrefixReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let len = bytes
            .len()
            .min(self.end.saturating_sub(self.position) as usize);
        let n = self.file.read_at(&mut bytes[..len], self.position)?;
        self.position += n as u64;
        Ok(n)
    }
}
impl Seek for PrefixReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let value = match pos {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.position) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.end) + i128::from(n),
        };
        self.position = u64::try_from(value)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid snapshot seek"))?;
        Ok(self.position)
    }
}

impl Snapshot {
    /// Typed catalog events retain one item, never a member list. Each scalar
    /// field is capped at the frame bound before allocation or consumption.
    /// Stream one catalog item at a time across arbitrary frame boundaries.
    /// Member lists are never materialized. A single variable-length field is
    /// owned by its event consumer; the frame/continuation history is not retained.
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub fn visit_catalog(
        &self,
        mut visitor: impl FnMut(&crate::authority::CatalogPayload) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        use super::wire::Wire;
        use crate::authority::CatalogPayload;
        let Some(mut decoder) = self.decoder() else {
            return Ok(());
        };
        while let Some(frame) = decoder.next_frame()? {
            if let Record::PayloadContinuation(Continuation {
                chunk: Chunk::Catalog {
                    ordinal: 0, last, ..
                },
                ..
            }) = frame.record
            {
                let bytes = frame.payload.to_vec();
                let mut stream = CatalogStream {
                    decoder: &mut decoder,
                    bytes: io::Cursor::new(bytes),
                    last,
                };
                #[cfg(any(test, feature = "fixtures"))]
                super::measure::catalog_payload();
                visitor(&CatalogPayload::Object(Box::new(
                    payload::CommittedObjectProjection::get(&mut stream)?,
                )))?;
                loop {
                    let mut first = [0; 1];
                    if stream.read(&mut first)? == 0 {
                        break;
                    }
                    let mut member = io::Cursor::new(first).chain(&mut stream);
                    #[cfg(any(test, feature = "fixtures"))]
                    super::measure::catalog_payload();
                    visitor(&CatalogPayload::Member(
                        crate::NativeObjectFileProjectionInput::get(&mut member)?,
                    ))?;
                }
                visitor(&CatalogPayload::ObjectEnd)?;
            }
        }
        Ok(())
    }
}
pub(super) struct CatalogStream<'a> {
    pub(super) decoder: &'a mut Decoder<PrefixReader>,
    pub(super) bytes: io::Cursor<Vec<u8>>,
    pub(super) last: bool,
}
impl Read for CatalogStream<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.bytes.read(out)?;
            if n != 0 || self.last {
                return Ok(n);
            }
            let frame = self
                .decoder
                .next_frame()
                .map_err(io::Error::other)?
                .ok_or_else(|| io::Error::other("catalog stream ends before last chunk"))?;
            let Record::PayloadContinuation(Continuation {
                chunk: Chunk::Catalog { last, .. },
                ..
            }) = frame.record
            else {
                return Err(io::Error::other("interrupted catalog stream"));
            };
            self.last = last;
            self.bytes.get_mut().clear();
            self.bytes.get_mut().extend_from_slice(frame.payload);
            self.bytes.set_position(0);
        }
    }
}
