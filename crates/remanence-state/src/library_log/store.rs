//! Exclusive segmented append authority with immutable-prefix recovery and sticky ambiguity.
use super::codec::{Fault, Header, HEADER_LEN};
use super::*;
use crate::log_frame::{
    self,
    publication::{self, Temporary},
};
use nix::fcntl::{Flock, FlockArg};
use remanence_library::durable_file::File;
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
/// Origin attribution injected by the eventual authority owner.
#[derive(Clone, Debug)]
pub struct Origin {
    /// Host identifier.
    pub host: Text,
    /// Process identifier.
    pub pid: u32,
    /// Build identifier.
    pub build: Text,
}
/// Injected UTC source; regression changes attribution but never segment ordering.
pub trait Clock: Send + Sync {
    /// Unix nanoseconds.
    fn now(&self) -> u64;
}
/// Injected record UUID source.
pub trait UuidSource: Send + Sync {
    /// UUID for the prospective sequence.
    fn next(&self, sequence: u64) -> Id;
}
/// Durable or unsynced append acknowledgment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// Source sequence.
    pub sequence: u64,
    /// Record identity.
    pub record_uuid: Id,
    /// Chained frame hash.
    pub record_hash: Hash,
    /// Publication included fsync.
    pub synced: bool,
}
/// Exact refusal at the first unauthoritative boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Segment basename.
    pub segment: String,
    /// Offset of the rejected frame/header.
    pub offset: u64,
    /// Frame sequence, when known.
    pub sequence: Option<u64>,
    /// Exact byte or semantic refusal.
    pub fault: RefusalKind,
}
/// Byte contradictions stop the stream; semantic contradictions stop library replay.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RefusalKind {
    /// Byte fault.
    #[error("{0}")]
    Bytes(Fault),
    /// Writer-impossible history.
    #[error("{0}")]
    Fold(FoldError),
}
/// One segment's validated prefix and tail classification.
#[derive(Clone, Debug)]
pub struct SegmentTail {
    /// Basename.
    pub name: String,
    /// Validated header.
    pub header: Option<Header>,
    /// Authoritative byte end.
    pub through: u64,
    /// Shared byte tail class.
    pub tail: log_frame::Tail,
}
/// Streaming replay result; errors preserve all previously accepted authority.
#[derive(Clone, Debug, Default)]
pub struct Replay {
    /// Resource state and pending dependencies.
    pub fold: LibraryFold,
    /// Last complete frame sequence.
    pub last_sequence: u64,
    /// Terminal chain hash.
    pub terminal_hash: Hash,
    /// Segment prefix boundaries.
    pub segments: Vec<SegmentTail>,
    /// First stopping refusal.
    pub refusal: Option<Refusal>,
    /// Peak encoded decoder buffer, not a frame history.
    pub decoder_peak_bytes: usize,
}
/// Storage refusal, distinguished from uncertain durability.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Pre-publication environment failure.
    #[error("library I/O: {0}")]
    Io(#[from] io::Error),
    /// Competing owner.
    #[error("Locked({0})")]
    Locked(nix::errno::Errno),
    /// Semantic preflight refusal; writes nothing.
    #[error("{0}")]
    Fold(#[from] FoldError),
    /// Recovery is required before appending.
    #[error("RecoveryRequired({0:?})")]
    RecoveryRequired(Refusal),
    /// Unknown write/sync/rename outcome.
    #[error("Ambiguous: {0}")]
    Ambiguous(io::Error),
    /// Sticky fence remains until explicit recovery.
    #[error("Fenced")]
    Fenced,
    /// Only the final segment may be repaired.
    #[error("InteriorDamage")]
    InteriorDamage,
    /// Repair must retain exactly the authoritative boundary.
    #[error("RepairBoundary")]
    RepairBoundary,
}
fn lock(dir: &Path, exclusive: bool) -> Result<Flock<fs::File>, StoreError> {
    let path = dir.join("library-log.lock");
    let f = if exclusive {
        fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?
    } else {
        fs::File::open(path)?
    };
    Flock::lock(
        f,
        if exclusive {
            FlockArg::LockExclusiveNonblock
        } else {
            FlockArg::LockSharedNonblock
        },
    )
    .map_err(|(_, e)| StoreError::Locked(e))
}
fn directory_lock(dir: &Path, exclusive: bool) -> Result<Flock<fs::File>, StoreError> {
    Flock::lock(
        fs::File::open(dir)?,
        if exclusive {
            FlockArg::LockExclusiveNonblock
        } else {
            FlockArg::LockSharedNonblock
        },
    )
    .map_err(|(_, e)| StoreError::Locked(e))
}
/// Replay under shared leases without creating any file, including the lock.
pub fn replay(dir: &Path, log_uuid: Id, view: &dyn TapeView) -> Result<Replay, StoreError> {
    let _directory = directory_lock(dir, false)?;
    let _lock = if dir.join("library-log.lock").try_exists()? {
        Some(lock(dir, false)?)
    } else {
        None
    };
    scan(dir, log_uuid, view)
}
fn scan(dir: &Path, log_uuid: Id, view: &dyn TapeView) -> Result<Replay, StoreError> {
    let mut files: Vec<_> = fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    files.retain(|p| {
        p.extension()
            .is_some_and(|e| e == std::ffi::OsStr::new("remlib"))
    });
    files.sort();
    let mut out = Replay::default();
    for (index, path) in files.iter().enumerate() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let mut f = fs::File::open(path)?;
        let mut header_bytes = [0; 84];
        let header_result = match f.read_exact(&mut header_bytes) {
            Ok(()) => Header::decode(&mut header_bytes.as_slice(), log_uuid),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(Fault::Header),
            Err(e) => return Err(e.into()),
        };
        let header = match header_result {
            Ok(h) => h,
            Err(fault) => {
                out.refusal = Some(Refusal {
                    segment: name.clone(),
                    offset: 0,
                    sequence: None,
                    fault: RefusalKind::Bytes(fault),
                });
                out.segments.push(SegmentTail {
                    name,
                    header: None,
                    through: 0,
                    tail: log_frame::Tail::Complete,
                });
                break;
            }
        };
        let fault = if header.name() != name {
            Some(Fault::SegmentName)
        } else if header.first_sequence != out.last_sequence + 1 {
            Some(Fault::SegmentSequence)
        } else if header.previous_segment_hash != out.terminal_hash {
            Some(Fault::PreviousSegmentHash)
        } else {
            None
        };
        out.segments.push(SegmentTail {
            name: name.clone(),
            header: Some(header.clone()),
            through: HEADER_LEN,
            tail: log_frame::Tail::Complete,
        });
        if let Some(fault) = fault {
            out.refusal = Some(Refusal {
                segment: name,
                offset: 0,
                sequence: Some(header.first_sequence),
                fault: RefusalKind::Bytes(fault),
            });
            break;
        }
        let mut decoder = codec::Decoder::at(f, header.first_sequence, HEADER_LEN);
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => {
                    let hash = match codec::verify(&frame, out.terminal_hash) {
                        Ok(h) => h,
                        Err(e) => {
                            out.refusal = Some(Refusal {
                                segment: name.clone(),
                                offset: frame.offset,
                                sequence: Some(frame.sequence),
                                fault: RefusalKind::Bytes(e),
                            });
                            break;
                        }
                    };
                    let receipt = Receipt {
                        sequence: frame.sequence,
                        record_uuid: frame.record.envelope.record_uuid,
                        record_hash: hash,
                        synced: true,
                    };
                    if let Err(e) = out.fold.receive(
                        frame.sequence,
                        frame.record,
                        receipt,
                        view,
                        &SegmentHistory {
                            dir,
                            segments: &out.segments,
                            before: frame.sequence,
                        },
                    ) {
                        out.refusal = Some(Refusal {
                            segment: name.clone(),
                            offset: frame.offset,
                            sequence: Some(frame.sequence),
                            fault: RefusalKind::Fold(e),
                        });
                        break;
                    }
                    out.last_sequence = frame.sequence;
                    out.terminal_hash = hash;
                    out.segments.last_mut().unwrap().through = frame.end;
                }
                Ok(None) => break,
                Err(codec::Error::Io(e)) => return Err(e.into()),
                Err(codec::Error::Recovery {
                    offset,
                    sequence,
                    fault,
                }) => {
                    out.refusal = Some(Refusal {
                        segment: name.clone(),
                        offset,
                        sequence,
                        fault: RefusalKind::Bytes(fault),
                    });
                    break;
                }
            }
        }
        out.decoder_peak_bytes = out.decoder_peak_bytes.max(decoder.retained_buffer_bytes());
        let segment = out.segments.last_mut().unwrap();
        segment.tail = decoder.tail();
        if out.refusal.is_none() {
            let fault = if segment.through == HEADER_LEN {
                Some(Fault::EmptySegment)
            } else if index + 1 < files.len() && segment.tail != log_frame::Tail::Complete {
                Some(Fault::InteriorTorn)
            } else {
                None
            };
            if let Some(fault) = fault {
                out.refusal = Some(Refusal {
                    segment: name,
                    offset: segment.through,
                    sequence: Some(out.last_sequence + 1),
                    fault: RefusalKind::Bytes(fault),
                });
            }
        }
        if out.refusal.is_some() {
            break;
        }
    }
    Ok(out)
}
/// Borrow the validated segment list, binary-search its header sequence boundaries,
/// then stream only that segment. No historical index or decoded cache is created.
struct SegmentHistory<'a> {
    dir: &'a Path,
    segments: &'a [SegmentTail],
    before: u64,
}
impl LibraryHistory for SegmentHistory<'_> {
    fn lookup(&self, reference: DecisionRef) -> Result<Option<Record>, FoldError> {
        Ok(self
            .read_sequence(reference.sequence)?
            .filter(|r| r.envelope.record_uuid == reference.record_uuid))
    }
    fn lookup_fence(&self, sequence: u64) -> Result<Option<Record>, FoldError> {
        self.read_sequence(sequence)
    }
}
impl SegmentHistory<'_> {
    /// Stream one validated segment for either reference form without caching history.
    fn read_sequence(&self, sequence: u64) -> Result<Option<Record>, FoldError> {
        if sequence == 0 || sequence >= self.before {
            return Ok(None);
        }
        // Replay records the current header before folding its first frame;
        // inspection may also include a refused header. Neither contributes
        // searchable authority, and a refused first_sequence may be unsorted.
        let end = self.segments.len()
            - usize::from(
                self.segments
                    .last()
                    .is_some_and(|s| s.through <= HEADER_LEN),
            );
        let segments = &self.segments[..end];
        let index = segments.partition_point(|s| {
            s.header
                .as_ref()
                .is_some_and(|h| h.first_sequence <= sequence)
        });
        let Some(segment) = index.checked_sub(1).map(|i| &segments[i]) else {
            return Ok(None);
        };
        let header = segment
            .header
            .as_ref()
            .ok_or(FoldError::HistoryUnavailable)?;
        let mut file = fs::File::open(self.dir.join(&segment.name))
            .map_err(|_| FoldError::HistoryUnavailable)?;
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(HEADER_LEN))
            .map_err(|_| FoldError::HistoryUnavailable)?;
        let mut decoder = codec::Decoder::at(
            file.take(segment.through - HEADER_LEN),
            header.first_sequence,
            HEADER_LEN,
        );
        let mut previous = header.previous_segment_hash;
        while let Some(frame) = decoder
            .next_frame()
            .map_err(|_| FoldError::HistoryUnavailable)?
        {
            previous =
                codec::verify(&frame, previous).map_err(|_| FoldError::HistoryUnavailable)?;
            if frame.sequence == sequence {
                return Ok(Some(frame.record));
            }
            if frame.sequence > sequence {
                break;
            }
        }
        // The segment's validated prefix includes this sequence. If it has
        // disappeared, report a read fault instead of inventing an absent cause.
        Err(FoldError::HistoryUnavailable)
    }
}
/// Sole append owner. Public producers are intentionally absent in B2.
pub struct LibraryLog {
    dir: PathBuf,
    identity: PathBuf,
    log_uuid: Id,
    origin: Origin,
    clock: Box<dyn Clock>,
    uuids: Box<dyn UuidSource>,
    diagnostic_fsync: bool,
    _lock: Flock<fs::File>,
    _directory_lock: Flock<fs::File>,
    state: Replay,
}
impl LibraryHistory for LibraryLog {
    fn lookup_fence(&self, sequence: u64) -> Result<Option<Record>, FoldError> {
        SegmentHistory {
            dir: &self.dir,
            segments: &self.state.segments,
            before: self.state.last_sequence.saturating_add(1),
        }
        .lookup_fence(sequence)
    }
    fn lookup(&self, reference: DecisionRef) -> Result<Option<Record>, FoldError> {
        SegmentHistory {
            dir: &self.dir,
            segments: &self.state.segments,
            before: self.state.last_sequence.saturating_add(1),
        }
        .lookup(reference)
    }
}
impl LibraryLog {
    /// Acquire ownership, validate every segment and replace an absent final suffix.
    pub fn open(
        dir: &Path,
        log_uuid: Id,
        origin: Origin,
        clock: Box<dyn Clock>,
        uuids: Box<dyn UuidSource>,
        diagnostic_fsync: bool,
        view: &dyn TapeView,
    ) -> Result<Self, StoreError> {
        fs::create_dir_all(dir)?;
        let directory_lease = directory_lock(dir, true)?;
        let lease = lock(dir, true)?;
        let identity = dir.join("library-log");
        // Only this family's unpublished temporaries belong to this owner.
        for e in fs::read_dir(dir)? {
            let e = e?;
            let n = e.file_name();
            let b = n.as_encoded_bytes();
            if [
                b".segment-library-log-".as_slice(),
                b".replacement-library-log-",
                b".evidence-library-log-",
            ]
            .iter()
            .any(|p| b.starts_with(p))
            {
                fs::remove_file(e.path())?;
            }
        }
        let state = scan(dir, log_uuid, view)?;
        let mut owner = Self {
            dir: dir.to_path_buf(),
            identity,
            log_uuid,
            origin,
            clock,
            uuids,
            diagnostic_fsync,
            _lock: lease,
            _directory_lock: directory_lease,
            state,
        };
        if owner.state.refusal.is_none() && !publication::has_ambiguity(&owner.identity)? {
            owner.remove_absent_suffix()?;
        }
        Ok(owner)
    }
    /// Last validated replay, including any recovery-required boundary.
    pub fn state(&self) -> &Replay {
        &self.state
    }
    /// Read-only inspection using the already held exclusive lease.
    pub fn inspect_tail(&self, view: &dyn TapeView) -> Result<Replay, StoreError> {
        scan(&self.dir, self.log_uuid, view)
    }
    fn remove_absent_suffix(&mut self) -> Result<(), StoreError> {
        if let Some(last) = self.state.segments.last() {
            if last.tail != log_frame::Tail::Complete {
                if let Err(e) = publication::replace_prefix(
                    &self.dir,
                    &self.dir.join(&last.name),
                    "library-log",
                    last.through,
                ) {
                    publication::fence_ambiguity(&self.dir, &self.identity);
                    return Err(StoreError::Ambiguous(io::Error::other(e)));
                }
                self.state.segments.last_mut().unwrap().tail = log_frame::Tail::Complete;
            }
        }
        Ok(())
    }
    /// Explicit recovery retains every complete valid frame, including a landed failed append.
    pub fn clear_ambiguity(&mut self, view: &dyn TapeView) -> Result<(), StoreError> {
        let state = scan(&self.dir, self.log_uuid, view)?;
        if let Some(e) = state.refusal {
            return Err(StoreError::RecoveryRequired(e));
        }
        self.state = state;
        self.remove_absent_suffix()?;
        let result = (|| {
            for s in &self.state.segments {
                File::open(self.dir.join(&s.name))?.sync_all()?;
            }
            let p = self.identity.with_extension("ambiguous");
            match fs::remove_file(p) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            File::open(&self.dir)?.sync_all()
        })();
        if let Err(e) = result {
            publication::fence_ambiguity(&self.dir, &self.identity);
            return Err(StoreError::Ambiguous(e));
        }
        publication::clear_process_fence(&self.identity);
        Ok(())
    }
    /// Archive damaged final bytes, then atomically retain exactly the authoritative prefix.
    pub fn repair_tail(&mut self, through: u64, view: &dyn TapeView) -> Result<(), StoreError> {
        let state = scan(&self.dir, self.log_uuid, view)?;
        let refusal = state.refusal.as_ref().ok_or(StoreError::RepairBoundary)?;
        let mut segments = Vec::new();
        for e in fs::read_dir(&self.dir)? {
            let p = e?.path();
            if p.extension()
                .is_some_and(|e| e == std::ffi::OsStr::new("remlib"))
            {
                segments.push(p);
            }
        }
        segments.sort();
        let last = state.segments.last().ok_or(StoreError::RepairBoundary)?;
        if segments.last() != Some(&self.dir.join(&refusal.segment)) {
            return Err(StoreError::InteriorDamage);
        }
        if through != last.through || through <= HEADER_LEN {
            return Err(StoreError::RepairBoundary);
        }
        let source = self.dir.join(&last.name);
        let evidence = self
            .dir
            .join(format!("{}.repair-{through}.evidence", last.name));
        let result = (|| {
            if evidence.try_exists()? {
                // A prior uncertain publication may have landed. Never overwrite evidence.
                if !same_bytes(&source, &evidence)? {
                    return Err(io::Error::other(
                        "repair evidence differs from damaged segment",
                    ));
                }
                File::open(&evidence)?.sync_all()?;
                File::open(&self.dir)?.sync_all()?;
            } else {
                let mut tmp = Temporary::new(&self.dir, "library-log", "evidence")?;
                io::copy(&mut fs::File::open(&source)?, &mut tmp.file)?;
                publication::publish_new(&tmp.file, &tmp.path, &evidence, &self.dir)?;
            }
            publication::replace_prefix(&self.dir, &source, "library-log", through)
                .map_err(io::Error::other)
        })();
        if let Err(e) = result {
            publication::fence_ambiguity(&self.dir, &self.identity);
            return Err(StoreError::Ambiguous(e));
        }
        self.clear_ambiguity(view)
    }
    /// Preflight under the append lease, publish, then advance in-memory authority.
    #[allow(dead_code)] // No production caller until B4b.
    pub(crate) fn append(
        &mut self,
        body: Body,
        actor: AuditActor,
        source_layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<Receipt, StoreError> {
        if publication::has_ambiguity(&self.identity)? {
            return Err(StoreError::Fenced);
        }
        if let Some(e) = &self.state.refusal {
            return Err(StoreError::RecoveryRequired(e.clone()));
        }
        let history = SegmentHistory {
            dir: &self.dir,
            segments: &self.state.segments,
            before: self.state.last_sequence.saturating_add(1),
        };
        self.state.fold.resolve(view, &history)?;
        let sequence = self
            .state
            .last_sequence
            .checked_add(1)
            .ok_or(FoldError::Sequence)?;
        let now = self.clock.now();
        let record = Record {
            envelope: Envelope {
                record_uuid: self.uuids.next(sequence),
                timestamp_unix_ns: now,
                host_id: self.origin.host.clone(),
                process_id: self.origin.pid,
                software_build: self.origin.build.clone(),
                actor,
                source_layer,
                operation_id: body.operation(),
                session_id: match &body {
                    Body::SessionObserved(v) => Some(v.session_id),
                    Body::SupersedeTail(v) => Some(v.session_id),
                    Body::DriveHealthObserved(v) => v.session_id,
                    _ => None,
                },
                idempotency_key: match &body {
                    Body::ManualFinalizationAccepted(v) => Some(v.idempotency_key),
                    Body::Reprovision(v) => v.adoption.as_ref().map(|a| a.idempotency_key),
                    Body::OperationRecorded(v) => {
                        if let OperationEvent::Accepted(a) = &v.event {
                            a.idempotency_key
                        } else {
                            None
                        }
                    }
                    _ => None,
                },
            },
            body,
        };
        if let fold::Validated::Duplicate(r) = self.state.fold.validate(&record, view, &history)? {
            return Ok(r);
        }
        let (bytes, hash) = codec::encode(sequence, self.state.terminal_hash, &record)?;
        let date = date(now)?;
        let rotate = self
            .state
            .segments
            .last()
            .is_none_or(|s| date > s.header.as_ref().unwrap().date);
        let synced = rotate || self.diagnostic_fsync || !matches!(record.body, Body::Diagnostic(_));
        let mut new_segment = None;
        let result = (|| -> io::Result<()> {
            if rotate {
                // The new segment's hash chain covers every byte in its predecessor,
                // including diagnostics whose own receipts did not promise durability.
                if let Some(active) = self.state.segments.last() {
                    File::open(self.dir.join(&active.name))?.sync_all()?;
                }
                let header = Header {
                    log_uuid: self.log_uuid,
                    first_sequence: sequence,
                    previous_segment_hash: self.state.terminal_hash,
                    date,
                };
                let name = header.name();
                let mut tmp = Temporary::new(&self.dir, "library-log", "segment")?;
                tmp.file
                    .write_all(&header.encode().map_err(io::Error::other)?)?;
                tmp.file.write_all(&bytes)?;
                publication::publish_new(&tmp.file, &tmp.path, &self.dir.join(&name), &self.dir)?;
                new_segment = Some(SegmentTail {
                    name,
                    header: Some(header),
                    through: HEADER_LEN + bytes.len() as u64,
                    tail: log_frame::Tail::Complete,
                });
            } else {
                let target = self.dir.join(&self.state.segments.last().unwrap().name);
                let mut f = File::options().append(true).open(target)?;
                if synced {
                    publication::append_and_publish(
                        &mut f,
                        &bytes,
                        None,
                        &self.identity,
                        &self.dir,
                        || Ok(()),
                    )?;
                } else {
                    publication::append_without_sync(&mut f, &bytes, &self.identity, &self.dir)?;
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            publication::fence_ambiguity(&self.dir, &self.identity);
            return Err(StoreError::Ambiguous(e));
        }
        let receipt = Receipt {
            sequence,
            record_uuid: record.envelope.record_uuid,
            record_hash: hash,
            synced,
        };
        // The same frozen view is expected throughout this synchronous append.
        if let Err(error) = self
            .state
            .fold
            .apply_validated(sequence, record, receipt.clone(), view)
        {
            // Publication already happened: fail closed if a future scope lacks apply semantics.
            publication::fence_ambiguity(&self.dir, &self.identity);
            return Err(StoreError::Ambiguous(io::Error::other(error)));
        }
        self.state.last_sequence = sequence;
        self.state.terminal_hash = hash;
        if let Some(s) = new_segment {
            self.state.segments.push(s);
        } else {
            self.state.segments.last_mut().unwrap().through += bytes.len() as u64;
        }
        Ok(receipt)
    }
    /// Fixture-only producer; no production authority escape.
    #[cfg(feature = "fixtures")]
    pub fn fixture_append(
        &mut self,
        body: Body,
        actor: AuditActor,
        view: &dyn TapeView,
    ) -> Result<Receipt, StoreError> {
        self.append(body, actor, SourceLayer::Layer4, view)
    }
}
fn date(ns: u64) -> io::Result<[u8; 10]> {
    let t =
        time::OffsetDateTime::from_unix_timestamp_nanos(ns as i128).map_err(io::Error::other)?;
    let s = format!("{:04}-{:02}-{:02}", t.year(), u8::from(t.month()), t.day());
    s.as_bytes().try_into().map_err(io::Error::other)
}

// Bounded comparison makes retry safe even for a large evidence segment.
fn same_bytes(a: &Path, b: &Path) -> io::Result<bool> {
    let mut a = fs::File::open(a)?;
    let mut b = fs::File::open(b)?;
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let mut left = [0; 8192];
    let mut right = [0; 8192];
    loop {
        let n = a.read(&mut left)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut right[..n])?;
        if left[..n] != right[..n] {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use crate::library_log::tests::{decision, diagnostic, open, put, Present};

    /// Replay history cannot see the current frame or any later on-disk frame.
    #[test]
    fn segment_history_is_strictly_before_current_record() {
        let d = tempfile::tempdir().unwrap();
        let (mut log, _) = open(d.path(), &Present);
        for _ in 0..3 {
            put(&mut log, diagnostic());
        }
        let history = SegmentHistory {
            dir: d.path(),
            segments: &log.state.segments,
            before: 2,
        };
        assert!(history.lookup(decision(1)).unwrap().is_some());
        assert!(history.lookup_fence(1).unwrap().is_some());
        assert!(history.lookup_fence(2).unwrap().is_none());
        assert!(history.lookup_fence(3).unwrap().is_none());
        assert!(history.lookup(decision(2)).unwrap().is_none());
        assert!(history.lookup(decision(3)).unwrap().is_none());
        let mut wrong = decision(1);
        wrong.record_uuid = [99; 16];
        assert!(history.lookup(wrong).unwrap().is_none());
        // A refused later header is not part of the binary-search boundaries.
        let mut segments = log.state.segments.clone();
        let mut refused = segments[0].clone();
        refused.header.as_mut().unwrap().first_sequence = 0;
        refused.through = HEADER_LEN;
        segments.push(refused);
        assert!(SegmentHistory {
            dir: d.path(),
            segments: &segments,
            before: 4
        }
        .lookup(decision(1))
        .unwrap()
        .is_some());
        // Corruption after validation is a history fault, never a missing record.
        fs::write(d.path().join(&log.state.segments[0].name), []).unwrap();
        assert_eq!(
            history.lookup_fence(1).unwrap_err(),
            FoldError::HistoryUnavailable
        );
        assert_eq!(
            history.lookup(decision(1)).unwrap_err(),
            FoldError::HistoryUnavailable
        );
    }
}
