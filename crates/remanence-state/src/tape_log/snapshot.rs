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
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::FileExt,
    sync::Arc,
};

pub(super) type PlanBase = Option<(u64, Arc<TapeSummary>)>;
type ValidatedPrefix = (TapeSummary, Tail, ReplayMetrics, PlanBase);

/// Unvalidated capture, deliberately not an authority token.
pub(super) struct Captured {
    file: Option<Arc<File>>,
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
        let end = file
            .as_ref()
            .map_or(Ok(0), |f| f.metadata().map(|m| m.len()))?;
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
            Ok((summary, tail, metrics, plan_base))
        } else {
            Ok((fold.summary().clone(), Tail::Complete, metrics, None))
        }
    }
    pub(super) fn validate_through(&self, through: u64) -> Result<(TapeSummary, u64), StoreError> {
        if through == 0 {
            return Err(StoreError::Refused(
                "repair must retain a closed initialization",
            ));
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
        let summary = loop {
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
                    if fold.push(&frame).is_err() {
                        break fold.summary().clone();
                    }
                }
                Ok(None) => {
                    let (summary, tail) = fold
                        .finish(decoder.tail())
                        .map_err(CodecError::EncodeSchema)?;
                    if tail == Tail::Complete {
                        return Err(StoreError::Refused(
                            "log has no damaged or provisional suffix",
                        ));
                    }
                    break summary;
                }
                Err(CodecError::Recovery(_)) => break fold.summary().clone(),
                Err(error) => return Err(error.into()),
            }
        };
        if summary.last_closed_sequence != through {
            return Err(StoreError::Refused(
                "repair must retain the last validated closed sequence",
            ));
        }
        let end = summary.closed_end;
        Ok((summary, end))
    }
    /// The exclusive owner has already fully validated and incrementally folded
    /// these bytes. Reuse that proof without a catalog scan under its lock.
    pub(super) fn trusted(self, summary: TapeSummary, tail: Tail, plan_base: PlanBase) -> Snapshot {
        Snapshot {
            file: self.file,
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
    pub(super) fn validate(self) -> Result<Snapshot, StoreError> {
        let (summary, tail, metrics, plan_base) = self.fold()?;
        if self.file.is_some() && summary.is_fresh() && tail == Tail::Complete {
            return Err(StoreError::Refused(
                "active log has no closed initialization",
            ));
        }
        Ok(Snapshot {
            file: self.file,
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
/// Fully trust-validated immutable prefix. Provisional rows never escape visitors.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub(super) plan_base: PlanBase,
    first_sequence: u64,
    file: Option<Arc<File>>,
    start: u64,
    captured_end: u64,
    pub(super) summary: TapeSummary,
    pub(super) tail: Tail,
    pub(super) header: Header,
    metrics: ReplayMetrics,
    capture_time: std::time::Duration,
}
impl Snapshot {
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
    /// Classification of captured bytes after the last closed record.
    pub fn tail(&self) -> Tail {
        self.tail
    }
    /// Byte end pinned under the lock, before validation outside it.
    pub fn captured_end(&self) -> u64 {
        self.captured_end
    }
    /// Run a structural pass over the already validated closed prefix, skipping
    /// catalog bytes. Directory/prefix planning rows are not durable media rows.
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
                    if !matches!(
                        close.record,
                        Record::Initialized(_) | Record::PayloadContinuation(_)
                    ) {
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
    pub(super) fn before(&self, sequence: u64) -> Result<Self, StoreError> {
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
    pub(super) fn group_snapshot(&self, sequence: u64, start: u64, end: u64) -> Self {
        let mut result = self.clone();
        result.start = start;
        result.first_sequence = sequence;
        result.summary.closed_end = end;
        result
    }
    pub(super) fn decoder(&self) -> Option<Decoder<PrefixReader>> {
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
pub(super) struct PrefixReader {
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
                visitor(&CatalogPayload::Object(Box::new(
                    payload::CommittedObjectProjection::get(&mut stream)?,
                )))?;
                loop {
                    let mut first = [0; 1];
                    if stream.read(&mut first)? == 0 {
                        break;
                    }
                    let mut member = io::Cursor::new(first).chain(&mut stream);
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
