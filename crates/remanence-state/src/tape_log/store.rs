//! Unwired per-tape disk authority. A stable companion flock covers creation,
//! append and copy-prefix repair. Prepared groups are disk spools; staging writes
//! provisional frames and fdatasyncs them before media work; completion writes
//! only the closer and fsyncs it. Provisional durability never grants authority.
use super::{
    codec::{self, CodecError, Decoder, Tail},
    fold::{Fold, GroupDigest, TapeSummary},
    wire::Wire,
    *,
};
use nix::fcntl::{Flock, FlockArg};
use remanence_library::durable_file::{self, File};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAGIC: &[u8; 8] = b"REMTAPE\x01";
/// Fixed header bytes before the length-delimited canonical geometry and CRC.
const FIXED_HEADER: usize = 38;
/// Typed storage refusals distinguish preflight, uncertain publication and
/// known-durable callback failure. A fenced owner cannot publish again.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// Existing durability policy rejected the host volume.
    #[error(transparent)]
    Volume(#[from] remanence_parity::JournalError),
    /// A successful durable commit was followed by a failing callback.
    #[error("tape-log commit is durable; after-fsync callback failed: {0}")]
    AfterDurable(Box<StoreError>),
    /// Environmental failure before publication starts.
    #[error("tape-log I/O: {0}")]
    Io(#[from] io::Error),
    /// Invalid frame, group or candidate.
    #[error(transparent)]
    Codec(#[from] CodecError),
    /// Header cannot establish tape identity and geometry.
    #[error("tape-log header identity, geometry or CRC mismatch")]
    Header,
    /// Stable lock is held by a conflicting owner.
    #[error("tape-log lease conflict: {0}")]
    Locked(nix::errno::Errno),
    /// A UUID claimed fresh already has an active log.
    #[error("active tape log already exists")]
    AlreadyExists,
    /// Append or namespace durability became uncertain; explicit fence clearing is required.
    #[error("tape-log authority ambiguous: {0}")]
    Ambiguous(io::Error),
    /// No further operation is allowed on an ambiguous lease.
    #[error("tape-log lease is fenced")]
    Fenced,
    /// Repair cannot destroy catalog authority without rebuilding it.
    #[error("repair sequence {requested} is below projection mark {projected}")]
    BelowProjection {
        /// Requested closing sequence.
        requested: u64,
        /// Current durable projection mark.
        projected: u64,
    },
    /// Invalid repair boundary or append protocol.
    #[error("tape-log operation refused: {0}")]
    Refused(&'static str),
}
/// Identity bound by every file, independently of its frames and group digests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Physical identity; reinitialization requires a different UUID.
    pub tape_uuid: [u8; 16],
    /// Immutable block size.
    pub block_size: u32,
    /// Immutable parity geometry, absent for parity-off.
    pub scheme: Option<ParityScheme>,
}
impl Header {
    fn bytes(&self) -> Result<Vec<u8>, StoreError> {
        if self.tape_uuid == [0; 16]
            || !remanence_parity::DEFAULT_BOOTSTRAP_CANDIDATE_BLOCK_SIZES.contains(&self.block_size)
        {
            return Err(StoreError::Header);
        }
        if let Some(s) = &self.scheme {
            s.validate().map_err(|_| StoreError::Header)?;
            if s.id.as_str().len() > u16::MAX as usize {
                return Err(StoreError::Header);
            }
        }
        let mut geometry = Vec::new();
        self.scheme.put(&mut geometry)?;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&self.tape_uuid);
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&(geometry.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&self.block_size.to_le_bytes());
        bytes.extend_from_slice(&geometry);
        bytes.extend_from_slice(&remanence_crc::crc64_xz(&bytes).to_le_bytes());
        Ok(bytes)
    }
    /// Decode the bounded self-describing header when opening by tape identity.
    pub fn decode(r: &mut impl Read, tape_uuid: [u8; 16]) -> Result<Self, StoreError> {
        let mut fixed = [0; 42];
        r.read_exact(&mut fixed).map_err(header_read_error)?;
        if &fixed[..8] != MAGIC
            || fixed[8..10] != VERSION.to_le_bytes()
            || fixed[10..26] != tape_uuid
            || fixed[26..34] != 1u64.to_le_bytes()
        {
            return Err(StoreError::Header);
        }
        let len = u32::from_le_bytes(fixed[34..38].try_into().unwrap()) as usize;
        if len > u16::MAX as usize + 128 {
            return Err(StoreError::Header);
        }
        let mut rest = vec![0; len + 8];
        r.read_exact(&mut rest).map_err(header_read_error)?;
        let mut geometry = &rest[..len];
        let scheme = Option::<ParityScheme>::get(&mut geometry)?;
        if !geometry.is_empty() {
            return Err(StoreError::Header);
        }
        let header = Self {
            tape_uuid,
            block_size: u32::from_le_bytes(fixed[38..42].try_into().unwrap()),
            scheme,
        };
        let mut actual = fixed.to_vec();
        actual.extend_from_slice(&rest);
        if actual != header.bytes()? {
            return Err(StoreError::Header);
        }
        Ok(header)
    }
    pub(super) fn read(r: &mut impl Read, expected: &Self) -> Result<u64, StoreError> {
        let bytes = expected.bytes()?;
        let mut actual = vec![0; bytes.len()];
        r.read_exact(&mut actual).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                StoreError::Header
            } else {
                e.into()
            }
        })?;
        if actual != bytes {
            return Err(StoreError::Header);
        }
        debug_assert!(bytes.len() >= FIXED_HEADER + 4 + 8);
        Ok(bytes.len() as u64)
    }
}
/// Path factory; constructing it does not create a log or grant a lease.
#[derive(Clone, Debug)]
pub struct Store {
    pub(super) directory: PathBuf,
    pub(super) path: PathBuf,
    pub(super) header: Header,
    discover: bool,
}
impl Store {
    /// The supplied directory is the already durably provisioned
    /// `journal.dir/tapes` directory. Volume policy is checked here; directory
    /// provisioning belongs to the caller's state-layout initialization.
    pub fn new(directory: impl AsRef<Path>, header: Header) -> Result<Self, StoreError> {
        let store = Self::paths(directory.as_ref(), header)?;
        remanence_parity::validate_trusted_journal_volume(&store.path)?;
        Ok(store)
    }
    /// Hermetic fixtures alone may bypass local-volume policy.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn new_fixture(directory: impl AsRef<Path>, header: Header) -> Result<Self, StoreError> {
        Self::paths(directory.as_ref(), header)
    }
    fn paths(directory: &Path, header: Header) -> Result<Self, StoreError> {
        header.bytes()?;
        let directory = directory.canonicalize()?;
        let path = directory.join(format!(
            "{}.remtape",
            uuid::Uuid::from_bytes(header.tape_uuid).simple()
        ));
        Ok(Self {
            directory,
            path,
            header,
            discover: false,
        })
    }
    /// Open by identity without consulting the catalog. Fresh geometry is bound
    /// by the first report while holding the exclusive lease.
    pub fn open(directory: impl AsRef<Path>, tape_uuid: [u8; 16]) -> Result<Self, StoreError> {
        let mut store = Self::new(
            directory,
            Header {
                tape_uuid,
                block_size: 262144,
                scheme: None,
            },
        )?;
        store.discover = true;
        store.discover_header()?;
        Ok(store)
    }
    /// Identity-only fixture open, with the same header/lock checks.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn open_fixture(
        directory: impl AsRef<Path>,
        tape_uuid: [u8; 16],
    ) -> Result<Self, StoreError> {
        let mut store = Self::new_fixture(
            directory,
            Header {
                tape_uuid,
                block_size: 262144,
                scheme: None,
            },
        )?;
        store.discover = true;
        store.discover_header()?;
        Ok(store)
    }
    fn discover_header(&mut self) -> Result<(), StoreError> {
        let _lock = self.lock(false)?;
        match fs::File::open(&self.path) {
            Ok(mut file) => self.header = Header::decode(&mut file, self.header.tape_uuid)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    // Called only under the stable lock. Explicit geometry stores keep enforcing
    // their header; identity-only factories discover a later first publication.
    fn current_header(&self) -> Result<Self, StoreError> {
        let mut current = self.clone();
        if self.discover {
            match fs::File::open(&self.path) {
                Ok(mut file) => current.header = Header::decode(&mut file, self.header.tape_uuid)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(current)
    }
    /// Active file path, for diagnostics and offline inspection.
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn lock(&self, exclusive: bool) -> Result<Flock<File>, StoreError> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(self.path.with_extension("lock"))?;
        lock(
            file,
            if exclusive {
                FlockArg::LockExclusiveNonblock
            } else {
                FlockArg::LockSharedNonblock
            },
        )
    }
    /// Capture inode/header/end under a short shared lease, then validate unlocked.
    pub fn snapshot(&self) -> Result<super::snapshot::Snapshot, StoreError> {
        let lease = self.lock(false)?;
        self.check_ambiguity()?;
        let current = self.current_header()?;
        let captured = super::snapshot::Captured::open(&current)?;
        drop(lease);
        captured.validate()
    }
    /// Acquire the only mutation capability. Torn tails are replaced before append.
    /// `fresh` refuses an existing active file even if it contains no closed group.
    pub fn acquire(&self, fresh: bool) -> Result<Lease, StoreError> {
        let lock = self.lock(true)?;
        self.cleanup_temporary()?;
        self.check_ambiguity()?;
        if self.path.with_extension("repair-required").try_exists()? {
            return Err(StoreError::Refused(
                "explicit repair requires physical-tail reconciliation before append",
            ));
        }
        if fresh && self.path.try_exists()? {
            return Err(StoreError::AlreadyExists);
        }
        let current = self.current_header()?;
        let captured = super::snapshot::Captured::open(&current)?;
        let snapshot = captured.validate()?;
        if snapshot.summary.is_fresh() && snapshot.captured_end() != 0 {
            return Err(StoreError::Refused(
                "active log has no closed initialization; fresh creation cannot replace it",
            ));
        }
        let fold = Fold::from_validated(snapshot.summary.clone());
        Ok(Lease {
            store: current,
            _lock: lock,
            fold,
            plan_base: snapshot.plan_base,
            tail: snapshot.tail,
            fenced: false,
        })
    }
    fn check_ambiguity(&self) -> Result<(), StoreError> {
        if process_fences()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&self.path)
            || self.path.with_extension("ambiguous").try_exists()?
        {
            Err(StoreError::Fenced)
        } else {
            Ok(())
        }
    }
    // Remember first, even if ENOSPC/EIO prevents every persistence attempt.
    fn fence_ambiguity(&self) {
        process_fences()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(self.path.clone());
        // The marker's existence is the fence; contents are not required. Still
        // attempt namespace sync when the marker's own fsync reports EIO.
        let file_result = File::options()
            .write(true)
            .create(true)
            .open(self.path.with_extension("ambiguous"))
            .and_then(|file| file.sync_all());
        let directory_result = File::open(&self.directory).and_then(|dir| dir.sync_all());
        if let Err(error) = file_result.and(directory_result) {
            tracing::error!(path = %self.path.display(), %error, "could not durably persist ambiguity fence; process fence remains");
        }
    }
    /// Explicit recovery of uncertain host durability. Revalidate, copy the closed
    /// prefix to a new inode, fsync it and its replacement name, then retire the
    /// ambiguity fence durably. A physical-tail repair fence is never cleared here.
    pub fn clear_ambiguity(&self) -> Result<(), StoreError> {
        let _lock = self.lock(true)?;
        self.cleanup_temporary()?;
        let result = (|| -> Result<(), StoreError> {
            let current = self.current_header()?;
            let snapshot = super::snapshot::Captured::open(&current)?.validate()?;
            if self.path.try_exists()? {
                if snapshot.summary().is_fresh() {
                    return Err(StoreError::Refused(
                        "cannot clear an uninitialized active log",
                    ));
                }
                replace_prefix(&current, snapshot.summary().closed_end)?;
            } else {
                File::open(&self.directory)?.sync_all()?;
            }
            match fs::remove_file(self.path.with_extension("ambiguous")) {
                Ok(()) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
            File::open(&self.directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_ok() {
            process_fences()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&self.path);
        } else {
            self.fence_ambiguity();
        }
        result
    }
    // Only this UUID's disposable temporaries: other tapes can have live spools.
    fn cleanup_temporary(&self) -> Result<(), StoreError> {
        let id = uuid::Uuid::from_bytes(self.header.tape_uuid)
            .simple()
            .to_string();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if ["spool", "initial", "replacement", "repair-record"]
                .iter()
                .any(|purpose| name.starts_with(&format!(".{purpose}-{id}-")))
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
    /// Explicit corruption repair. B3a has no projection rebuild transaction, so
    /// cuts below the supplied mark are always refused. The exact damaged inode
    /// is archived and hashed before atomic installation of the validated prefix.
    pub fn repair_tail(&self, through: u64, projected: u64) -> Result<RepairEvidence, StoreError> {
        if through < projected {
            return Err(StoreError::BelowProjection {
                requested: through,
                projected,
            });
        }
        let _lock = self.lock(true)?;
        self.cleanup_temporary()?;
        if self.path.with_extension("repair-required").try_exists()? {
            return self.resume_repair(through);
        }
        let current = self.current_header()?;
        let captured = super::snapshot::Captured::open(&current)?;
        let (summary, end) = captured.validate_through(through)?;
        if summary.last_closed_sequence != through {
            return Err(StoreError::Refused("not a closed sequence"));
        }
        let source = fs::File::open(&self.path)?;
        let original_len = source.metadata()?.len();
        if end >= original_len {
            return Err(StoreError::Refused("no suffix to repair"));
        }
        let mut archive = Temporary::new(self, "repair-evidence")?;
        let mut digest = Sha256::new();
        let mut reader = source;
        let mut buffer = [0; 65536];
        loop {
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            archive.file.write_all(&buffer[..n])?;
            digest.update(&buffer[..n]);
        }
        archive.file.sync_all()?;
        // Keep evidence even if replacement fails. Its unique name is never reused.
        archive.keep = true;
        let digest: [u8; 32] = digest.finalize().into();
        let fence_path = self.path.with_extension("repair-required");
        let mut fence = Temporary::new(self, "repair-record")?;
        let evidence = RepairEvidence {
            archive: archive.path.clone(),
            digest,
            reconciliation_record: fence_path.clone(),
            original_len,
            retained_end: end,
            through_sequence: through,
        };
        let record = RepairRecord {
            tape_uuid: self.header.tape_uuid,
            retained_digest: file_digest(&self.path, end)?,
            evidence,
            physical_tail_reconciliation_required: true,
        };
        serde_json::to_writer(&mut fence.file, &record)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fence.file.sync_all()?;
        durable_file::rename_noreplace(&fence.path, &fence_path)?;
        File::open(&self.directory)?.sync_all()?;
        replace_prefix(self, end)?;
        Ok(record.evidence)
    }
    fn resume_repair(&self, through: u64) -> Result<RepairEvidence, StoreError> {
        let file = fs::File::open(self.path.with_extension("repair-required"))?;
        let record: RepairRecord = serde_json::from_reader(file.take(16 * 1024))
            .map_err(|_| StoreError::Refused("invalid repair record"))?;
        let evidence = &record.evidence;
        if record.tape_uuid != self.header.tape_uuid
            || evidence.through_sequence != through
            || evidence.reconciliation_record != self.path.with_extension("repair-required")
            || !record.physical_tail_reconciliation_required
        {
            return Err(StoreError::Refused("repair record does not match request"));
        }
        let current = self.current_header()?;
        let len = fs::metadata(&self.path)?.len();
        if len == evidence.original_len && file_digest(&self.path, len)? == evidence.digest {
            let (summary, end) =
                super::snapshot::Captured::open(&current)?.validate_through(through)?;
            if end != evidence.retained_end
                || summary.last_closed_sequence != through
                || file_digest(&self.path, end)? != record.retained_digest
            {
                return Err(StoreError::Refused("repair boundary changed"));
            }
        } else if len == evidence.retained_end
            && file_digest(&self.path, len)? == record.retained_digest
        {
            let snapshot = super::snapshot::Captured::open(&current)?.validate()?;
            if snapshot.summary().last_closed_sequence != through
                || snapshot.summary().closed_end != len
            {
                return Err(StoreError::Refused("repaired boundary changed"));
            }
        } else {
            return Err(StoreError::Refused("repair source digest changed"));
        }
        // Re-copy even an already installed prefix: a prior directory sync may
        // have failed. Only this successful return proves the replacement durable.
        replace_prefix(&current, evidence.retained_end)?;
        Ok(record.evidence)
    }
}
/// Durable exact-byte archive and the explicit repair boundary.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct RepairEvidence {
    /// Durable diagnostic/fence record. B3b must reconcile the physical tail
    /// under the wider recovery owner before retiring this append fence.
    pub reconciliation_record: PathBuf,
    /// Complete original damaged file, including its header.
    pub archive: PathBuf,
    /// SHA-256 over every archived byte.
    pub digest: [u8; 32],
    /// Original byte length.
    pub original_len: u64,
    /// Installed exclusive byte end.
    pub retained_end: u64,
    /// Validated last closed sequence.
    pub through_sequence: u64,
}
/// Exclusive owner; no in-place truncation is ever performed.
#[derive(Debug)]
pub struct Lease {
    store: Store,
    _lock: Flock<File>,
    fold: Fold,
    plan_base: super::snapshot::PlanBase,
    tail: Tail,
    fenced: bool,
}
impl Lease {
    /// Header identity held by this owner.
    pub fn header(&self) -> &Header {
        &self.store.header
    }
    /// Bind geometry before the first active file is published.
    pub fn bind_header(&mut self, header: Header) -> Result<(), StoreError> {
        self.check()?;
        if header.tape_uuid != self.store.header.tape_uuid {
            return Err(StoreError::Header);
        }
        if self.summary().is_fresh() && !self.store.path.try_exists()? {
            let mut summary = TapeSummary::default();
            summary.closed_end = header.bytes()?.len() as u64;
            self.store.header = header;
            self.fold = Fold::from_validated(summary);
            Ok(())
        } else if self.store.header == header {
            Ok(())
        } else {
            Err(StoreError::Header)
        }
    }
    /// Latest validated authority, excluding any provisional suffix.
    pub fn summary(&self) -> &TapeSummary {
        self.fold.summary()
    }
    /// An independent immutable prefix under the retained lease.
    pub fn snapshot(&self) -> Result<super::snapshot::Snapshot, StoreError> {
        self.check()?;
        Ok(super::snapshot::Captured::open(&self.store)?.trusted(
            self.summary().clone(),
            self.tail,
            self.plan_base.clone(),
        ))
    }
    /// Create a bounded disk spool at this lease's next sequence and offset.
    pub fn prepare(&self) -> Result<GroupWriter, StoreError> {
        self.check()?;
        GroupWriter::new(
            &self.store,
            self.summary().last_closed_sequence + 1,
            self.summary().closed_end,
        )
    }
    fn check(&self) -> Result<(), StoreError> {
        if self.fenced {
            Err(StoreError::Fenced)
        } else {
            Ok(())
        }
    }
    /// Validate provisional frames without poisoning the owner, then write each
    /// frame separately and fdatasync it before the media barrier. The retained
    /// trial has no closer. A failed data sync rolls back the provisional prefix
    /// under this lease before returning a known pre-media I/O failure.
    pub fn stage(&mut self, mut group: GroupWriter) -> Result<Staged<'_>, StoreError> {
        self.check()?;
        if group.offset != self.summary().closed_end
            || group.sequence != self.summary().last_closed_sequence + 1
        {
            return Err(StoreError::Refused("stale prepared group"));
        }
        group.temp.file.seek(SeekFrom::Start(0))?;
        let trial = self.fold.preflight_prefix(&mut group.temp.file, false)?;
        group.temp.file.seek(SeekFrom::Start(0))?;
        let mut decoder = Decoder::at(&mut group.temp.file, group.sequence, group.offset);
        if let Some(frame) = decoder.next_frame()? {
            if let Record::Initialized(init) = frame.record {
                if init.tape_uuid != self.store.header.tape_uuid
                    || init.block_size != self.store.header.block_size
                    || init.scheme != self.store.header.scheme
                {
                    return Err(StoreError::Header);
                }
            }
        }
        drop(decoder);
        let fresh = self.summary().is_fresh();
        if !fresh && !self.store.path.try_exists()? {
            return Err(StoreError::Refused(
                "active log disappeared under exclusive lease",
            ));
        }
        if self.tail != Tail::Complete {
            self.fenced = true;
            replace_prefix(&self.store, self.summary().closed_end)?;
            tracing::warn!(path = %self.store.path.display(), tail = ?self.tail, "replaced torn tape-log suffix");
            self.tail = Tail::Complete;
            self.fenced = false;
        }
        let mut temporary = if fresh {
            Some(Temporary::new(&self.store, "initial")?)
        } else {
            None
        };
        let mut output = if let Some(temp) = &mut temporary {
            temp.file.write_all(&self.store.header.bytes()?)?;
            temp.file.try_clone()?
        } else {
            File::options().append(true).open(&self.store.path)?
        };
        self.fenced = true;
        let copied = (|| -> io::Result<()> {
            group.temp.file.seek(SeekFrom::Start(0))?;
            copy_frames(&mut group.temp.file, &mut output, group.end - group.offset)
        })();
        if let Err(error) = copied {
            return Err(self.ambiguous(error));
        }
        if group.next != group.sequence {
            if let Err(error) = output.sync_data() {
                // No media barrier has run and no closer was written. Restore
                // only the already acknowledged prefix, preserving old snapshots.
                drop(output);
                if let Some(temp) = temporary.as_mut() {
                    fs::remove_file(&temp.path)?;
                    temp.keep = true;
                } else {
                    // If rollback itself fails, retain the live owner's guard;
                    // that is a separate cleanup failure, not an ambiguous commit.
                    replace_prefix(&self.store, self.summary().closed_end)?;
                }
                self.fenced = false;
                return Err(StoreError::Io(error));
            }
        }
        Ok(Staged {
            lease: self,
            group,
            trial,
            output,
            temporary,
        })
    }
    fn ambiguous(&mut self, error: io::Error) -> StoreError {
        self.fenced = true;
        self.store.fence_ambiguity();
        StoreError::Ambiguous(error)
    }
    /// Convenience for metadata-only events. Preflight the complete candidate
    /// before staging, so a bad prebuilt closer leaves this owner usable.
    pub fn append(&mut self, group: PreparedGroup) -> Result<(), StoreError> {
        let PreparedGroup { mut writer, closer } = group;
        let bytes = writer.encode(&closer, &[])?;
        writer.temp.file.seek(SeekFrom::Start(0))?;
        self.fold
            .preflight((&mut writer.temp.file).chain(bytes.as_slice()))?;
        self.stage(writer)?.complete(&closer)
    }
}
/// A staged group keeps the exclusive lease borrowed through the media barrier.
/// Dropping it fences the live owner; a fresh temporary log remains unpublished.
pub struct Staged<'a> {
    lease: &'a mut Lease,
    group: GroupWriter,
    trial: Fold,
    output: File,
    temporary: Option<Temporary>,
}
impl Staged<'_> {
    /// Reference for constructing the closing record after the media barrier.
    pub fn reference(&self, members: u64) -> GroupRef {
        self.group.reference(members)
    }
    /// Run a post-durability seam only after first-publication directory sync.
    pub fn complete_with_after(
        self,
        closer: &Record,
        after: impl FnOnce() -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        self.complete(closer)?;
        after().map_err(|error| StoreError::AfterDurable(Box::new(error)))
    }
    /// Validate the newly constructed closer against the retained provisional
    /// trial, then append that one frame and fsync. Closer preflight does not
    /// rescan payloads; one post-sync pass advances the retained authoritative fold.
    pub fn complete(mut self, closer: &Record) -> Result<(), StoreError> {
        if matches!(
            closer,
            Record::Initialized(_) | Record::PayloadContinuation(_)
        ) {
            return Err(StoreError::Refused("closing record required"));
        }
        let bytes = self.group.encode(closer, &[])?;
        let mut decoder = Decoder::at(bytes.as_slice(), self.group.next, self.group.end);
        let frame = decoder
            .next_frame()?
            .ok_or(StoreError::Refused("empty closer"))?;
        self.trial.push(&frame).map_err(CodecError::from)?;
        if self.trial.summary().last_closed_sequence != self.group.next {
            return Err(StoreError::Refused("closer did not close group"));
        }
        let durable = (|| -> io::Result<()> {
            self.output.write_all(&bytes)?;
            self.output.sync_all()?;
            if let Some(temp) = &self.temporary {
                durable_file::rename_noreplace(&temp.path, &self.lease.store.path)?;
                File::open(&self.lease.store.directory)?.sync_all()?;
            }
            Ok(())
        })();
        if let Err(error) = durable {
            return Err(self.lease.ambiguous(error));
        }
        self.group
            .temp
            .file
            .seek(SeekFrom::Start(0))
            .map_err(|e| StoreError::AfterDurable(Box::new(e.into())))?;
        let mut decoder = Decoder::at(
            (&mut self.group.temp.file).chain(bytes.as_slice()),
            self.group.sequence,
            self.group.offset,
        );
        let folded = (|| -> Result<(), CodecError> {
            while let Some(frame) = decoder.next_frame()? {
                match &frame.record {
                    Record::FinalizationPrepared(plan) | Record::FinalizationDecided(plan) => {
                        self.lease.plan_base = Some((
                            plan.group.first_sequence,
                            std::sync::Arc::new(self.lease.summary().clone()),
                        ));
                    }
                    Record::FinalizationPreparationRetired { .. } => self.lease.plan_base = None,
                    _ => {}
                }
                self.lease.fold.push(&frame)?;
            }
            Ok(())
        })();
        folded.map_err(|error| StoreError::AfterDurable(Box::new(StoreError::Codec(error))))?;
        self.lease.fenced = false;
        Ok(())
    }
}
/// Copy bounded, already preflighted frames with one whole-frame write each.
/// A short kernel write may still tear; EOF-short prefixes/bodies handle that.
fn copy_frames(input: &mut impl Read, output: &mut impl Write, mut left: u64) -> io::Result<()> {
    let mut bytes = Vec::new();
    while left > 0 {
        let mut prefix = [0; PREFIX_LEN];
        input.read_exact(&mut prefix)?;
        let body = u64::from(u32::from_le_bytes(prefix[4..8].try_into().unwrap()))
            + u64::from(u32::from_le_bytes(prefix[8..12].try_into().unwrap()));
        let len = FRAME_OVERHEAD as u64 + body;
        if body > MAX_FRAME_BODY as u64 || len > left {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid preflighted frame size",
            ));
        }
        bytes.resize(len as usize, 0);
        bytes[..PREFIX_LEN].copy_from_slice(&prefix);
        input.read_exact(&mut bytes[PREFIX_LEN..])?;
        output.write_all(&bytes)?;
        #[cfg(any(test, feature = "fixtures"))]
        super::measure::copied(len);
        left -= len;
    }
    Ok(())
}
/// Disk-backed candidate builder. It retains only the current encoded frame.
pub struct GroupWriter {
    temp: Temporary,
    sequence: u64,
    offset: u64,
    next: u64,
    end: u64,
    digest: GroupDigest,
}
impl GroupWriter {
    fn new(store: &Store, sequence: u64, offset: u64) -> Result<Self, StoreError> {
        Ok(Self {
            temp: Temporary::new(store, "spool")?,
            sequence,
            offset,
            next: sequence,
            end: offset,
            digest: GroupDigest::new(sequence, offset),
        })
    }
    /// First provisional sequence identifying this group.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Add one Initialized or continuation frame. Payloads should use modest
    /// chunks (normally 1 MiB); logical items may span any number of chunks.
    pub fn push(&mut self, record: &Record, payload: &[u8]) -> Result<(), StoreError> {
        if !matches!(
            record,
            Record::Initialized(_) | Record::PayloadContinuation(_)
        ) {
            return Err(StoreError::Refused("provisional frame required"));
        }
        let bytes = self.encode(record, payload)?;
        let mut decoder = Decoder::at(bytes.as_slice(), self.next, self.end);
        let frame = decoder
            .next_frame()?
            .ok_or(StoreError::Refused("empty frame"))?;
        self.digest.push(&frame).map_err(CodecError::EncodeSchema)?;
        self.end = frame.end;
        self.next += 1;
        self.temp.file.write_all(&bytes)?;
        Ok(())
    }
    fn encode(&self, record: &Record, payload: &[u8]) -> Result<Vec<u8>, StoreError> {
        let mut bytes = Vec::new();
        codec::encode_frame(&mut bytes, self.next, record, payload)?;
        Ok(bytes)
    }
    /// Reference to embed in the closing record. Member count is independently
    /// checked by the non-poisoning fold preflight, never trusted from the writer.
    pub fn reference(&self, members: u64) -> GroupRef {
        self.digest.reference(members)
    }
    /// Pair a provisional spool with a known metadata closer. Media callers use
    /// stage(self), run their barrier, then complete with its newly proved closer.
    pub fn close(self, record: &Record) -> Result<PreparedGroup, StoreError> {
        if matches!(
            record,
            Record::Initialized(_) | Record::PayloadContinuation(_)
        ) {
            return Err(StoreError::Refused("closing record required"));
        }
        Ok(PreparedGroup {
            writer: self,
            closer: record.clone(),
        })
    }
}
/// Prebuilt metadata candidate, fully preflighted by append before mutation.
pub struct PreparedGroup {
    pub(super) writer: GroupWriter,
    pub(super) closer: Record,
}
struct Temporary {
    path: PathBuf,
    file: File,
    keep: bool,
}
impl Temporary {
    fn new(store: &Store, purpose: &str) -> io::Result<Self> {
        let path = store.directory.join(format!(
            ".{purpose}-{}-{}",
            uuid::Uuid::from_bytes(store.header.tape_uuid).simple(),
            uuid::Uuid::new_v4()
        ));
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            file,
            keep: false,
        })
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}
fn replace_prefix(store: &Store, end: u64) -> Result<(), StoreError> {
    let mut temp = Temporary::new(store, "replacement")?;
    let source = fs::File::open(&store.path)?;
    if io::copy(&mut source.take(end), &mut temp.file)? != end {
        return Err(StoreError::Refused("short repair source"));
    }
    temp.file.sync_all()?;
    durable_file::rename(&temp.path, &store.path)?;
    File::open(&store.directory)?.sync_all()?;
    Ok(())
}
fn lock(mut file: File, operation: FlockArg) -> Result<Flock<File>, StoreError> {
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        let error;
        (file, error) = match Flock::lock(file, operation) {
            Ok(lock) => return Ok(lock),
            Err(failure) => failure,
        };
        if operation != FlockArg::LockExclusiveNonblock
            || error != nix::errno::Errno::EWOULDBLOCK
            || Instant::now() >= deadline
        {
            return Err(StoreError::Locked(error));
        }
        let reader = Flock::lock(file, FlockArg::LockSharedNonblock)
            .map_err(|_| StoreError::Locked(error))?;
        file = reader
            .unlock()
            .map_err(|(_, error)| StoreError::Locked(error))?;
        std::thread::sleep(
            Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn header_read_error(error: io::Error) -> StoreError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        StoreError::Header
    } else {
        error.into()
    }
}
fn process_fences() -> &'static Mutex<BTreeSet<PathBuf>> {
    static FENCES: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
    FENCES.get_or_init(Mutex::default)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RepairRecord {
    tape_uuid: [u8; 16],
    retained_digest: [u8; 32],
    evidence: RepairEvidence,
    physical_tail_reconciliation_required: bool,
}
fn file_digest(path: &Path, end: u64) -> io::Result<[u8; 32]> {
    let mut reader = fs::File::open(path)?.take(end);
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    let mut count = 0;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
        count += n as u64;
    }
    if count != end {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(digest.finalize().into())
}
