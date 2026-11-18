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
    sync::Arc,
};
/// Path shared by lost-fence recording and runtime reconciliation.
pub(crate) fn fence_reconcile_path(library_root: &Path) -> PathBuf {
    library_root.join("library-log.fence-reconcile")
}

/// Make the scanned library tail and its directory durable before projection.
pub(crate) fn sync_published_tail(dir: &Path, replay: &Replay) -> io::Result<()> {
    if let Some(last) = replay.segments.last() {
        File::open(dir.join(&last.name))?.sync_all()?;
    }
    File::open(dir)?.sync_all()
}

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
/// Durable append acknowledgment for every library record.
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
    /// Every 64th validated frame: sequence, byte offset, and preceding chain hash.
    history_index: Vec<(u64, u64, Hash)>,
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
#[cfg(test)]
impl Replay {
    /// Allocated sparse-index bytes only; fold and segment metadata are excluded.
    pub(super) fn history_index_bytes(&self) -> usize {
        self.segments
            .iter()
            .map(|segment| {
                segment.history_index.capacity() * std::mem::size_of::<(u64, u64, Hash)>()
            })
            .sum()
    }
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
    /// Assignment writer found a protected native finalization after pending retry.
    #[error("finalization protects {0:?}")]
    FinalizationInProgress(TapeRef),
    /// Writer-only occupancy refusal; never a wire or frozen-fold rule.
    #[error("tape {tape_uuid} holds {instance_count} committed instances; a tape holding data cannot change pool; it can only be retired")]
    TapeHoldsCommittedInstances {
        /// Occupied tape identity.
        tape_uuid: uuid::Uuid,
        /// Number of committed instances at the decision.
        instance_count: u64,
    },
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
#[cfg_attr(any(test, feature = "fixtures"), track_caller)]
pub fn replay(dir: &Path, log_uuid: Id, view: &dyn TapeView) -> Result<Replay, StoreError> {
    let _directory = directory_lock(dir, false)?;
    let _lock = if dir.join("library-log.lock").try_exists()? {
        Some(lock(dir, false)?)
    } else {
        None
    };
    scan(dir, log_uuid, view)
}
#[cfg_attr(any(test, feature = "fixtures"), track_caller)]
fn scan(dir: &Path, log_uuid: Id, view: &dyn TapeView) -> Result<Replay, StoreError> {
    scan_observed(dir, log_uuid, view, &mut |_| Ok(()))
}
/// Stream validated applications without retaining a decoded history.
#[cfg_attr(any(test, feature = "fixtures"), track_caller)]
pub fn replay_observed(
    dir: &Path,
    log_uuid: Id,
    view: &dyn TapeView,
    applied: &mut dyn FnMut(super::authority::AppliedLibraryRecord) -> Result<(), StoreError>,
) -> Result<Replay, StoreError> {
    replay_observed_with_received(dir, log_uuid, view, applied, &mut |_, _| Ok(()))
}
/// Observe every validated received record, including excluded and pending records,
/// under exactly the same directory and library-log leases as ordinary replay.
#[cfg_attr(any(test, feature = "fixtures"), track_caller)]
pub fn replay_observed_with_received(
    dir: &Path,
    log_uuid: Id,
    view: &dyn TapeView,
    applied: &mut dyn FnMut(super::authority::AppliedLibraryRecord) -> Result<(), StoreError>,
    received: &mut dyn FnMut(&Record, &Receipt) -> Result<(), StoreError>,
) -> Result<Replay, StoreError> {
    let _directory = directory_lock(dir, false)?;
    let _lock = if dir.join("library-log.lock").try_exists()? {
        Some(lock(dir, false)?)
    } else {
        None
    };
    scan_observed_with_received(dir, log_uuid, view, applied, received)
}
#[cfg_attr(any(test, feature = "fixtures"), track_caller)]
fn scan_observed(
    dir: &Path,
    log_uuid: Id,
    view: &dyn TapeView,
    applied: &mut dyn FnMut(super::authority::AppliedLibraryRecord) -> Result<(), StoreError>,
) -> Result<Replay, StoreError> {
    scan_observed_with_received(dir, log_uuid, view, applied, &mut |_, _| Ok(()))
}
#[cfg_attr(any(test, feature = "fixtures"), track_caller)]
fn scan_observed_with_received(
    dir: &Path,
    log_uuid: Id,
    view: &dyn TapeView,
    applied: &mut dyn FnMut(super::authority::AppliedLibraryRecord) -> Result<(), StoreError>,
    received: &mut dyn FnMut(&Record, &Receipt) -> Result<(), StoreError>,
) -> Result<Replay, StoreError> {
    let mut files: Vec<_> = fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    files.retain(|p| {
        p.extension()
            .is_some_and(|e| e == std::ffi::OsStr::new("remlib"))
    });
    files.sort();
    let files = files
        .into_iter()
        .map(|path| (path, None))
        .collect::<Vec<_>>();
    scan_prefixes_observed(dir, log_uuid, view, applied, received, &files)
}
#[cfg_attr(any(test, feature = "fixtures"), track_caller)]
fn scan_prefixes(
    dir: &Path,
    log_uuid: Id,
    view: &dyn TapeView,
    applied: &mut dyn FnMut(super::authority::AppliedLibraryRecord) -> Result<(), StoreError>,
    files: &[(PathBuf, Option<u64>)],
) -> Result<Replay, StoreError> {
    scan_prefixes_observed(dir, log_uuid, view, applied, &mut |_, _| Ok(()), files)
}

/// Received observations follow byte-chain and fold validation, including
/// pending/excluded dispositions; they never grant projector application proof.
#[cfg_attr(any(test, feature = "fixtures"), track_caller)]
fn scan_prefixes_observed(
    dir: &Path,
    log_uuid: Id,
    view: &dyn TapeView,
    applied: &mut dyn FnMut(super::authority::AppliedLibraryRecord) -> Result<(), StoreError>,
    received: &mut dyn FnMut(&Record, &Receipt) -> Result<(), StoreError>,
    files: &[(PathBuf, Option<u64>)],
) -> Result<Replay, StoreError> {
    #[cfg(any(test, feature = "fixtures"))]
    crate::full_pass::count(crate::full_pass::PassKind::LibraryReplay);
    let mut out = Replay::default();
    for (index, (path, limit)) in files.iter().enumerate() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let mut f = fs::File::open(path)?.take(limit.unwrap_or(u64::MAX));
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
                    history_index: Vec::new(),
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
            history_index: Vec::new(),
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
                    let observed_record = frame.record.clone();
                    let mut applied_sequences = Vec::new();
                    if let Err(e) = out.fold.receive_observed(
                        frame.sequence,
                        frame.record,
                        receipt.clone(),
                        view,
                        &SegmentHistory {
                            dir,
                            segments: &out.segments,
                            before: frame.sequence,
                        },
                        &mut |sequence| applied_sequences.push(sequence),
                    ) {
                        if matches!(e, FoldError::TapeUnavailable(_)) {
                            // Environmental evidence failure is not a damaged
                            // library suffix and must never authorize repair.
                            return Err(e.into());
                        }
                        out.refusal = Some(Refusal {
                            segment: name.clone(),
                            offset: frame.offset,
                            sequence: Some(frame.sequence),
                            fault: RefusalKind::Fold(e),
                        });
                        break;
                    }
                    received(&observed_record, &receipt)?;
                    for sequence in applied_sequences {
                        applied(super::authority::AppliedLibraryRecord {
                            log_uuid,
                            record_hash: hash,
                            sequence,
                            record: observed_record.clone(),
                            progress: super::authority::FoldProgress::capture(&out.fold),
                        })?;
                    }
                    let segment = out.segments.last_mut().unwrap();
                    if (frame.sequence - header.first_sequence).is_multiple_of(64) {
                        segment.history_index.push((
                            frame.sequence,
                            frame.offset,
                            out.terminal_hash,
                        ));
                    }
                    out.last_sequence = frame.sequence;
                    out.terminal_hash = hash;
                    segment.through = frame.end;
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
#[cfg(test)]
thread_local! {
    /// Successful frame decodes in history lookups on this test's thread only.
    static HISTORY_FRAMES_DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
/// Borrow the validated segment list, binary-search its header sequence boundaries,
/// then stream at most 64 frames from a sparse chain anchor. No decoded cache is kept.
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
        Ok(self.read_application(sequence)?.map(|(record, _)| record))
    }
    fn read_application(&self, sequence: u64) -> Result<Option<(Record, Hash)>, FoldError> {
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
        let anchor = segment
            .history_index
            .partition_point(|entry| entry.0 <= sequence);
        let &(first_sequence, offset, mut previous) = anchor
            .checked_sub(1)
            .map(|index| &segment.history_index[index])
            .ok_or(FoldError::HistoryUnavailable)?;
        let mut file = fs::File::open(self.dir.join(&segment.name))
            .map_err(|_| FoldError::HistoryUnavailable)?;
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset))
            .map_err(|_| FoldError::HistoryUnavailable)?;
        let mut decoder =
            codec::Decoder::at(file.take(segment.through - offset), first_sequence, offset);
        while let Some(frame) = decoder
            .next_frame()
            .map_err(|_| FoldError::HistoryUnavailable)?
        {
            #[cfg(test)]
            HISTORY_FRAMES_DECODED.with(|count| count.set(count.get() + 1));
            previous =
                codec::verify(&frame, previous).map_err(|_| FoldError::HistoryUnavailable)?;
            if frame.sequence == sequence {
                return Ok(Some((frame.record, previous)));
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
/// Sole append owner for the segmented library log.
pub struct LibraryLog {
    dir: PathBuf,
    identity: PathBuf,
    log_uuid: Id,
    origin: Origin,
    clock: Box<dyn Clock>,
    uuids: Box<dyn UuidSource>,
    _lock: Arc<Flock<fs::File>>,
    _directory_lock: Arc<Flock<fs::File>>,
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
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub fn open(
        dir: &Path,
        log_uuid: Id,
        origin: Origin,
        clock: Box<dyn Clock>,
        uuids: Box<dyn UuidSource>,
        _diagnostic_fsync: bool,
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
            _lock: Arc::new(lease),
            _directory_lock: Arc::new(directory_lease),
            state,
        };
        if owner.state.refusal.is_none() && !publication::has_ambiguity(&owner.identity)? {
            owner.remove_absent_suffix()?;
        }
        if let Err(error) = sync_published_tail(&owner.dir, &owner.state) {
            publication::fence_ambiguity(&owner.dir, &owner.identity);
            return Err(StoreError::Ambiguous(error));
        }
        Ok(owner)
    }
    pub(super) fn log_uuid(&self) -> Id {
        self.log_uuid
    }
    /// Last validated replay. Its refusal may also be an in-memory re-fold
    /// latch, not a scan boundary or permission to repair the durable stream.
    pub fn state(&self) -> &Replay {
        &self.state
    }
    /// Read-only inspection using the already held exclusive lease.
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
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
    /// Republish even a complete final segment, then verify its retained end before clearing the fence.
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub fn clear_ambiguity(&mut self, view: &dyn TapeView) -> Result<(), StoreError> {
        let state = scan(&self.dir, self.log_uuid, view)?;
        if let Some(e) = state.refusal {
            return Err(StoreError::RecoveryRequired(e));
        }
        let result = (|| -> io::Result<()> {
            if let Some(last) = state.segments.last() {
                publication::replace_prefix(
                    &self.dir,
                    &self.dir.join(&last.name),
                    "library-log",
                    last.through,
                )
                .map_err(io::Error::other)?;
            }
            let rescanned = scan(&self.dir, self.log_uuid, view).map_err(io::Error::other)?;
            let same_end = rescanned.last_sequence == state.last_sequence
                && rescanned.terminal_hash == state.terminal_hash
                && rescanned.segments.last().map(|s| (&s.name, s.through))
                    == state.segments.last().map(|s| (&s.name, s.through));
            let valid_empty = !state.segments.is_empty()
                || (rescanned.segments.is_empty()
                    && rescanned.last_sequence == 0
                    && rescanned.terminal_hash == [0; 32]);
            if rescanned.refusal.is_some() || !same_end || !valid_empty {
                return Err(io::Error::other(
                    "library end changed during ambiguity recovery",
                ));
            }
            self.state = rescanned;
            if self.state.segments.is_empty() {
                File::open(&self.dir)?.sync_all()?;
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
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
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
    /// Durably name a tape whose safety fence could not be published.
    fn record_lost_fence(&self, tape: TapeRef) {
        let result = (|| -> io::Result<()> {
            let mut file = File::options()
                .create(true)
                .append(true)
                .open(fence_reconcile_path(&self.dir))?;
            file.write_all(&tape.uuid)?;
            file.sync_all()?;
            File::open(&self.dir)?.sync_all()
        })();
        if let Err(error) = result {
            tracing::error!(tape = %uuid::Uuid::from_bytes(tape.uuid), %error, "could not record lost library fence");
        }
    }
    /// Read the lost-tape trigger; malformed entries must keep runtime admission closed.
    pub(super) fn fence_reconciliation_tapes(&self) -> Result<Vec<Id>, StoreError> {
        let bytes = match fs::read(fence_reconcile_path(&self.dir)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        if bytes.len() % 16 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "malformed fence-reconcile entries",
            )
            .into());
        }
        Ok(bytes
            .chunks_exact(16)
            .map(|entry| entry.try_into().unwrap())
            .collect())
    }
    /// Remove the trigger only after all owed fences have been durably reconciled.
    pub(super) fn finish_fence_reconciliation(&self) -> Result<(), StoreError> {
        match fs::remove_file(fence_reconcile_path(&self.dir)) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
    /// Preflight under the append lease, publish, then advance in-memory authority.
    #[allow(dead_code)] // Fixture-facing wrapper; production uses append_checked.
    pub(super) fn append(
        &mut self,
        body: Body,
        actor: AuditActor,
        source_layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<Receipt, StoreError> {
        self.append_observed(body, actor, source_layer, view, &mut |_| {})
    }
    /// Append while reporting the fold's application order, including pending retries.
    pub(super) fn append_observed(
        &mut self,
        body: Body,
        actor: AuditActor,
        source_layer: SourceLayer,
        view: &dyn TapeView,
        applied: &mut dyn FnMut(u64),
    ) -> Result<Receipt, StoreError> {
        self.append_checked(
            body,
            actor,
            source_layer,
            view,
            &mut |sequence, _, _| applied(sequence),
            &|_, _| Ok(()),
        )
    }
    /// Writer-specific checks run after pending retry, under the same append ownership.
    pub(super) fn append_checked(
        &mut self,
        body: Body,
        actor: AuditActor,
        source_layer: SourceLayer,
        view: &dyn TapeView,
        applied: &mut dyn FnMut(u64, Record, Hash),
        check: &dyn Fn(&LibraryFold, &Body) -> Result<(), StoreError>,
    ) -> Result<Receipt, StoreError> {
        let lost = match &body {
            Body::FenceRaised(FenceRaised {
                scope: FenceScope::Tape(tape),
                ..
            }) => Some(*tape),
            _ => None,
        };
        match publication::has_ambiguity(&self.identity) {
            Ok(false) => (),
            refusal => {
                if let Some(tape) = lost {
                    self.record_lost_fence(tape);
                }
                return Err(match refusal {
                    Ok(true) => StoreError::Fenced,
                    Err(error) => StoreError::Io(error),
                    Ok(false) => unreachable!(),
                });
            }
        }
        if let Some(e) = &self.state.refusal {
            return Err(StoreError::RecoveryRequired(e.clone()));
        }
        let history = SegmentHistory {
            dir: &self.dir,
            segments: &self.state.segments,
            before: self.state.last_sequence.saturating_add(1),
        };
        // Retain only the bounded pending set before resolution consumes it.
        // A decode error belongs to resolve's own refusal unless that record
        // actually applies; preserve applications emitted before any refusal.
        {
            let mut pending: std::collections::BTreeMap<_, _> = self
                .state
                .fold
                .pending
                .iter()
                .map(|pending| {
                    let (bytes, hash) = pending.application_bytes();
                    (
                        pending.sequence,
                        wire::decode(pending.tag as u16, bytes)
                            .map(|record| (record, hash))
                            .map_err(|_| FoldError::InvalidSchema),
                    )
                })
                .collect();
            let mut sequences = Vec::new();
            let resolution = self
                .state
                .fold
                .resolve_observed(view, &history, &mut |sequence| sequences.push(sequence));
            for sequence in sequences {
                let (record, hash) = pending
                    .remove(&sequence)
                    .ok_or(FoldError::HistoryUnavailable)??;
                applied(sequence, record, hash);
            }
            resolution?;
        }
        check(&self.state.fold, &body)?;
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
                    Body::SupersedeTail(v) => v.session_id,
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
        // The projector receives the codec's interpretation, checked entirely
        // in memory before a file is opened for publication.
        let decoded = {
            let mut decoder = codec::Decoder::at(bytes.as_slice(), sequence, 0);
            let frame = decoder
                .next_frame()
                .map_err(io::Error::other)?
                .ok_or_else(|| io::Error::other("encoded library frame is absent"))?;
            let decoded_hash =
                codec::verify(&frame, self.state.terminal_hash).map_err(io::Error::other)?;
            if decoded_hash != hash {
                return Err(io::Error::other("encoded library frame hash differs").into());
            }
            frame.record
        };
        let date = date(now)?;
        let rotate = self
            .state
            .segments
            .last()
            .is_none_or(|s| date > s.header.as_ref().unwrap().date);
        let synced = true;
        let mut new_segment = None;
        let result = (|| -> io::Result<()> {
            if rotate {
                // Preserve the predecessor barrier before publishing its successor's hash chain.
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
                    history_index: Vec::new(),
                });
            } else {
                let target = self.dir.join(&self.state.segments.last().unwrap().name);
                let mut f = File::options().append(true).open(target)?;
                // Publication helpers fence before the lost tape can be recorded.
                f.write_all(&bytes)?;
                f.sync_all()?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            if let Some(tape) = lost {
                self.record_lost_fence(tape);
            }
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
        if let Err(error) = self.state.fold.apply_validated_observed(
            sequence,
            record,
            receipt.clone(),
            view,
            &mut |sequence| applied(sequence, decoded.clone(), hash),
        ) {
            // Publication already happened: fail closed if a future scope lacks apply semantics.
            if let Some(tape) = lost {
                self.record_lost_fence(tape);
            }
            publication::fence_ambiguity(&self.dir, &self.identity);
            return Err(StoreError::Ambiguous(io::Error::other(error)));
        }
        if let Some(mut segment) = new_segment {
            segment
                .history_index
                .push((sequence, HEADER_LEN, self.state.terminal_hash));
            self.state.segments.push(segment);
        } else {
            let segment = self.state.segments.last_mut().unwrap();
            if (sequence - segment.header.as_ref().unwrap().first_sequence).is_multiple_of(64) {
                segment
                    .history_index
                    .push((sequence, segment.through, self.state.terminal_hash));
            }
            segment.through += bytes.len() as u64;
        }
        self.state.last_sequence = sequence;
        self.state.terminal_hash = hash;
        Ok(receipt)
    }
    /// Fixture-only producer; no production authority escape.
    #[cfg(feature = "fixtures")]
    #[allow(dead_code)] // fixture producers are module-private after B4b-1
    pub(super) fn fixture_append(
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

/// Immutable validated byte prefixes retaining the exclusive owner leases.
/// The active owner may append or rotate; it never modifies these prefixes.
#[derive(Clone)]
pub(super) struct ReadSnapshot {
    dir: PathBuf,
    uuid: Id,
    files: Vec<(PathBuf, Option<u64>)>,
    _lock: Arc<Flock<fs::File>>,
    _directory_lock: Arc<Flock<fs::File>>,
}
impl ReadSnapshot {
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub(super) fn inspect(
        &self,
        view: &dyn TapeView,
        applied: &mut dyn FnMut(super::authority::AppliedLibraryRecord) -> Result<(), StoreError>,
        received: &mut dyn FnMut(&Record, &Receipt) -> Result<(), StoreError>,
    ) -> Result<Replay, StoreError> {
        scan_prefixes_observed(&self.dir, self.uuid, view, applied, received, &self.files)
    }
    #[cfg_attr(any(test, feature = "fixtures"), track_caller)]
    pub(super) fn replay(
        &self,
        view: &dyn TapeView,
        applied: &mut dyn FnMut(super::authority::AppliedLibraryRecord) -> Result<(), StoreError>,
    ) -> Result<Replay, StoreError> {
        scan_prefixes(&self.dir, self.uuid, view, applied, &self.files)
    }
    /// Walk only the captured prefixes, checking bytes without folding records.
    pub(super) fn frames(
        &self,
        visit: &mut dyn FnMut(&Record, u64, Hash) -> std::ops::ControlFlow<()>,
    ) -> Result<super::history::FramesEnd, (String, u64, String)> {
        let mut previous = [0; 32];
        let mut sequence = 0;
        for (index, (path, limit)) in self.files.iter().enumerate() {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let damage = |offset, fault: String| (name.clone(), offset, fault);
            let limit = limit.expect("history snapshots carry byte limits");
            let mut file = fs::File::open(path)
                .map_err(|error| damage(0, error.to_string()))?
                .take(limit);
            let mut bytes = [0; 84];
            match file.read_exact(&mut bytes) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    return Err(damage(0, Fault::Header.to_string()));
                }
                Err(error) => return Err(damage(0, error.to_string())),
            }
            let header = Header::decode(&mut bytes.as_slice(), self.uuid)
                .map_err(|fault| damage(0, fault.to_string()))?;
            let fault = if header.name() != name {
                Some(Fault::SegmentName)
            } else if header.first_sequence != sequence + 1 {
                Some(Fault::SegmentSequence)
            } else if header.previous_segment_hash != previous {
                Some(Fault::PreviousSegmentHash)
            } else {
                None
            };
            if let Some(fault) = fault {
                return Err(damage(0, fault.to_string()));
            }
            let mut through = HEADER_LEN;
            let mut decoder = codec::Decoder::at(file, header.first_sequence, HEADER_LEN);
            loop {
                match decoder.next_frame() {
                    Ok(Some(frame)) => {
                        let hash = codec::verify(&frame, previous)
                            .map_err(|fault| damage(frame.offset, fault.to_string()))?;
                        previous = hash;
                        sequence = frame.sequence;
                        through = frame.end;
                        if visit(&frame.record, frame.sequence, hash).is_break() {
                            return Ok(super::history::FramesEnd::Stopped);
                        }
                    }
                    Ok(None) => break,
                    Err(codec::Error::Io(error)) => {
                        return Err(damage(through, error.to_string()));
                    }
                    Err(codec::Error::Recovery { offset, fault, .. }) => {
                        return Err(damage(offset, fault.to_string()));
                    }
                }
            }
            let fault = if through == HEADER_LEN {
                Some(Fault::EmptySegment)
            } else if index + 1 < self.files.len() && decoder.tail() != log_frame::Tail::Complete {
                Some(Fault::InteriorTorn)
            } else {
                None
            };
            if let Some(fault) = fault {
                return Err(damage(through, fault.to_string()));
            }
            if through != limit || decoder.tail() != log_frame::Tail::Complete {
                return Err(damage(
                    through,
                    "segment ends before its recorded limit".into(),
                ));
            }
        }
        Ok(super::history::FramesEnd::Complete)
    }
}
impl LibraryLog {
    pub(super) fn lookup_application(
        &self,
        sequence: u64,
    ) -> Result<Option<(Record, Hash)>, FoldError> {
        SegmentHistory {
            dir: &self.dir,
            segments: &self.state.segments,
            before: self.state.last_sequence.saturating_add(1),
        }
        .read_application(sequence)
    }
    /// Snapshot's storage gates plus resolved dependencies, without copying history.
    pub(super) fn check_fold_read(&self) -> Result<(), StoreError> {
        if publication::has_ambiguity(&self.identity)? {
            return Err(StoreError::Fenced);
        }
        if let Some(refusal) = &self.state.refusal {
            return Err(StoreError::RecoveryRequired(refusal.clone()));
        }
        if !self.state.fold.pending.is_empty() {
            return Err(StoreError::Fold(FoldError::HistoryUnavailable));
        }
        Ok(())
    }
    pub(super) fn snapshot(&self) -> Result<ReadSnapshot, StoreError> {
        if publication::has_ambiguity(&self.identity)? {
            return Err(StoreError::Fenced);
        }
        if let Some(refusal) = &self.state.refusal {
            return Err(StoreError::RecoveryRequired(refusal.clone()));
        }
        Ok(self.bounded_source())
    }

    /// Copy validated byte limits under append ownership. A replacement re-fold
    /// deliberately bypasses ambiguity, but never reads a frame past `through`.
    pub(super) fn bounded_source(&self) -> ReadSnapshot {
        ReadSnapshot {
            dir: self.dir.clone(),
            uuid: self.log_uuid,
            files: self
                .state
                .segments
                .iter()
                .map(|segment| (self.dir.join(&segment.name), Some(segment.through)))
                .collect(),
            _lock: self._lock.clone(),
            _directory_lock: self._directory_lock.clone(),
        }
    }

    /// Install only an exact clean re-fold. A failure latches only refusal in
    /// memory: this is not necessarily a scan boundary, and writes no marker.
    /// Existing refusals belong to this process and are never cleared or replaced.
    pub(super) fn install_refold(
        &mut self,
        scanned: Result<Replay, StoreError>,
    ) -> Option<Refusal> {
        if self.state.refusal.is_some() {
            return None;
        }
        let live_sequence = self.state.last_sequence;
        let live_hash = self.state.terminal_hash;
        let (live_segment, live_offset) = self
            .state
            .segments
            .last()
            .map(|segment| (segment.name.clone(), segment.through))
            .unwrap_or_default();
        let at_live_end = |fault| Refusal {
            segment: live_segment.clone(),
            offset: live_offset,
            sequence: Some(live_sequence.saturating_add(1)),
            fault: RefusalKind::Fold(fault),
        };
        let refusal = match scanned {
            Ok(scan) => {
                if let Some(refusal) = scan.refusal {
                    tracing::error!(?refusal, live_sequence, ?live_hash,
                        scan_sequence = scan.last_sequence, scan_hash = ?scan.terminal_hash,
                        "replacement re-fold found a refusal; retaining the live fold");
                    refusal
                } else if scan.last_sequence == live_sequence && scan.terminal_hash == live_hash {
                    self.state.fold = scan.fold;
                    return None;
                } else {
                    let mut refusal = at_live_end(FoldError::HistoryUnavailable);
                    if scan.last_sequence < live_sequence {
                        if let Some(segment) = scan.segments.last() {
                            refusal.segment = segment.name.clone();
                            refusal.offset = segment.through;
                        }
                        refusal.sequence = Some(scan.last_sequence.saturating_add(1));
                    }
                    tracing::error!(?refusal, live_sequence, ?live_hash,
                        scan_sequence = scan.last_sequence, scan_hash = ?scan.terminal_hash,
                        "replacement re-fold ends differ; retaining the live fold");
                    refusal
                }
            }
            Err(error) => {
                tracing::error!(%error, live_sequence, ?live_hash,
                    segment = %live_segment, offset = live_offset,
                    "replacement re-fold failed; retaining the live fold");
                at_live_end(match error {
                    StoreError::Fold(error) => error,
                    _ => FoldError::HistoryUnavailable,
                })
            }
        };
        self.state.refusal = Some(refusal.clone());
        Some(refusal)
    }
    /// Capture readable limits even when authority writes or projections refuse.
    pub(super) fn history_source(
        &self,
    ) -> Result<(ReadSnapshot, super::history::HistoryStanding), StoreError> {
        use super::history::HistoryStanding;
        let standing = if publication::has_ambiguity(&self.identity)? {
            HistoryStanding::Fenced
        } else if let Some(refusal) = &self.state.refusal {
            HistoryStanding::Refused {
                refusal: format!("{refusal:?}"),
            }
        } else {
            HistoryStanding::Clean
        };
        Ok((self.bounded_source(), standing))
    }
}

#[cfg(test)]
mod sparse_history_tests {
    use super::*;
    use crate::library_log::tests as fixture;
    use std::io::{Seek, SeekFrom};

    /// Independent oracle: decode each complete segment from its header anchor.
    fn full_scan(log: &LibraryLog) -> Vec<(Record, Hash)> {
        let mut records = Vec::new();
        for segment in &log.state.segments {
            let header = segment.header.as_ref().unwrap();
            let mut file = fs::File::open(log.dir.join(&segment.name)).unwrap();
            file.seek(SeekFrom::Start(HEADER_LEN)).unwrap();
            let mut decoder = codec::Decoder::at(
                file.take(segment.through - HEADER_LEN),
                header.first_sequence,
                HEADER_LEN,
            );
            let mut previous = header.previous_segment_hash;
            while let Some(frame) = decoder.next_frame().unwrap() {
                previous = codec::verify(&frame, previous).unwrap();
                records.push((frame.record, previous));
            }
        }
        records
    }

    /// Append and replay build identical anchors; every lookup, including a
    /// rotated segment's first frame, matches a full scan within 64 decodes.
    #[test]
    fn sparse_history_matches_full_scan_and_reopen() {
        let root = tempfile::tempdir().unwrap();
        let (mut log, clock) = fixture::open(root.path(), &fixture::Present);
        for _ in 0..257 {
            fixture::put(&mut log, fixture::diagnostic());
        }
        clock.0.store(
            fixture::NOW + 86_400_000_000_000,
            std::sync::atomic::Ordering::SeqCst,
        );
        for _ in 0..65 {
            fixture::put(&mut log, fixture::diagnostic());
        }
        assert_eq!(log.state.segments.len(), 2);
        let expected = full_scan(&log);
        assert_eq!(expected.len(), 322);
        let appended: Vec<_> = log
            .state
            .segments
            .iter()
            .map(|s| s.history_index.clone())
            .collect();
        assert_eq!(appended[0].len(), 5);
        assert_eq!(appended[1].len(), 2);
        let check = |log: &LibraryLog| {
            for sequence in 1..=322 {
                HISTORY_FRAMES_DECODED.with(|count| count.set(0));
                let actual = log
                    .lookup_application(sequence)
                    .expect("indexed lookup must verify from the preceding frame hash")
                    .unwrap();
                let count = HISTORY_FRAMES_DECODED.with(|count| count.get());
                assert!(count <= 64, "sequence {sequence} decoded {count} frames");
                assert_eq!(
                    actual,
                    expected[sequence as usize - 1],
                    "sequence {sequence}"
                );
            }
            for segment in &log.state.segments {
                for &(sequence, offset, _) in &segment.history_index {
                    assert!(sequence <= log.state.last_sequence);
                    assert!(offset < segment.through);
                }
            }
            assert_eq!(log.lookup_application(0).unwrap(), None);
            assert_eq!(log.lookup_application(323).unwrap(), None);
        };
        check(&log);
        let snapshot = log.snapshot().unwrap();
        let replay = snapshot.replay(&fixture::Present, &mut |_| Ok(())).unwrap();
        assert_eq!(
            replay
                .segments
                .iter()
                .map(|s| s.history_index.clone())
                .collect::<Vec<_>>(),
            appended
        );
        drop(snapshot);
        drop(log);
        let (log, _) = fixture::open(root.path(), &fixture::Present);
        assert_eq!(
            log.state
                .segments
                .iter()
                .map(|s| s.history_index.clone())
                .collect::<Vec<_>>(),
            appended
        );
        check(&log);
    }
}
