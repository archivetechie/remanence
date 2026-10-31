//! Per-tape disk authority. A stable companion flock covers creation,
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
    /// The catalog projection mark points inside the validated prefix, but
    /// not at a decodable frame with the sequence it names.
    #[error("tape-log projection mark does not name a decodable frame: {0}")]
    ProjectionMarkInvalid(String),
}
/// Refusal for a log that is absent or shorter than its projection mark.
pub const BELOW_PROJECTION_MARK: &str =
    "tape log is below its projected-through sequence; recovery required";
/// Refusal for a projection offset outside the validated closed prefix.
pub const PROJECTION_MARK_MISMATCH: &str =
    "tape-log projection mark does not match its validated boundary";
/// Refusal for a projection mark that names a provisional frame.
pub const PROJECTION_MARK_NOT_CLOSING: &str =
    "tape-log projection sequence is not a closed boundary";
impl StoreError {
    /// The log is absent or ends below the catalog's projection mark.
    pub fn is_below_projection(&self) -> bool {
        matches!(
            self,
            Self::BelowProjection { .. } | Self::Refused(BELOW_PROJECTION_MARK)
        )
    }
    /// The log reaches the catalog's projection mark, but the mark does not
    /// name one of its closing frames. Only an operator can resolve this.
    pub fn is_invalid_projection_mark(&self) -> bool {
        matches!(
            self,
            Self::ProjectionMarkInvalid(_)
                | Self::Refused(PROJECTION_MARK_MISMATCH | PROJECTION_MARK_NOT_CLOSING)
        )
    }
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
    projection_path: Option<PathBuf>,
}
impl Store {
    /// The supplied directory is the already durably provisioned
    /// `journal.dir/tapes` directory. Volume policy is checked here; directory
    /// provisioning belongs to the caller's state-layout initialization.
    pub fn new(
        directory: impl AsRef<Path>,
        header: Header,
        index_path: &Path,
    ) -> Result<Self, StoreError> {
        let store = Self::paths(directory.as_ref(), header)?.bind_projection(index_path);
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
            projection_path: None,
        })
    }
    /// Open by identity without consulting the catalog. Fresh geometry is bound
    /// by the first report while holding the exclusive lease.
    pub fn open(
        directory: impl AsRef<Path>,
        tape_uuid: [u8; 16],
        index_path: &Path,
    ) -> Result<Self, StoreError> {
        let mut store = Self::new(
            directory,
            Header {
                tape_uuid,
                block_size: 262144,
                scheme: None,
            },
            index_path,
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
        // Headers are immutable and first publication is an atomic rename.
        // Admission revalidates identity under its stable lease; merely opening
        // a factory neither takes a second lock nor creates authority.
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
    /// Bind the configured catalog once at factory construction. Every writer
    /// reads its current floor under the tape lease, including terminal recovery.
    pub(crate) fn bind_projection(mut self, path: &Path) -> Self {
        self.projection_path = Some(path.to_path_buf());
        self
    }
    fn projection_floor(&self) -> Result<u64, StoreError> {
        match &self.projection_path {
            Some(path) => crate::CatalogIndex::open_read_only(path)
                .and_then(|index| index.projected_through_sequence(self.header.tape_uuid))
                .map_err(|e| StoreError::Io(io::Error::other(e))),
            None => Ok(0), // explicit low-level fixture/inspection store
        }
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
        ).map_err(|error| {
            tracing::warn!(tape_uuid = %uuid::Uuid::from_bytes(self.header.tape_uuid), operation = if exclusive { "writer" } else { "snapshot" }, %error, "tape-log lock refusal");
            error
        })
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
        self.acquire_inner(fresh, false)
    }
    /// Recovery-only owner: inspection is allowed, but publication stays fenced
    /// until the caller proves the physical tail under this same lease.
    pub(crate) fn acquire_for_tail_reconciliation(&self) -> Result<Lease, StoreError> {
        self.acquire_inner(false, true)
    }
    fn acquire_inner(&self, fresh: bool, recovery: bool) -> Result<Lease, StoreError> {
        let lock = self.lock(true)?;
        self.cleanup_temporary()?;
        self.check_ambiguity()?;
        let repair_pending = self.path.with_extension("repair-required").try_exists()?;
        if repair_pending && !recovery {
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
        let projected_through_sequence = self.projection_floor()?;
        if snapshot.summary.last_closed_sequence < projected_through_sequence {
            return Err(StoreError::BelowProjection {
                requested: snapshot.summary.last_closed_sequence,
                projected: projected_through_sequence,
            });
        }
        let fold = Fold::from_validated(snapshot.summary.clone());
        Ok(Lease {
            store: current,
            _lock: lock,
            fold,
            plan_base: snapshot.plan_base,
            tail: snapshot.tail,
            fenced: false,
            repair_pending,
            projected_through_sequence,
            publication_observer: None,
        })
    }
    pub(super) fn has_ambiguity(&self) -> Result<bool, StoreError> {
        Ok(process_fences()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&self.path)
            || self.path.with_extension("ambiguous").try_exists()?)
    }
    fn check_ambiguity(&self) -> Result<(), StoreError> {
        if self.has_ambiguity()? {
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
        self.clear_ambiguity_above(0)
    }
    /// Restart reconciliation may not discard an acknowledged projected group.
    pub fn clear_ambiguity_above(&self, projected: u64) -> Result<(), StoreError> {
        let _lock = self.lock(true)?;
        self.cleanup_temporary()?;
        let result = (|| -> Result<(), StoreError> {
            let current = self.current_header()?;
            let snapshot = super::snapshot::Captured::open(&current)?.validate()?;
            if snapshot.summary().last_closed_sequence < projected {
                return Err(StoreError::BelowProjection {
                    requested: snapshot.summary().last_closed_sequence,
                    projected,
                });
            }
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
    /// Inspect a frozen boundary and damage without modifying authority.
    /// Read-only: inspection never creates the lease file. Every owner creates
    /// it before publishing, so a log without one (for example, restored from a
    /// copy) has no live owner and is captured without a shared lease.
    pub fn inspect_tail(&self) -> Result<TailInspection, StoreError> {
        let lease = match File::open(self.path.with_extension("lock")) {
            Ok(file) => Some(lock(file, FlockArg::LockSharedNonblock)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let captured = super::snapshot::Captured::open(&self.current_header()?)?;
        drop(lease);
        captured.inspect_tail()
    }
    /// Validate the proposed retained prefix without archiving or replacing any
    /// bytes, so the state owner can protect accepted terminal obligations first.
    pub(crate) fn preview_repair(
        &self,
        through: u64,
    ) -> Result<super::snapshot::Snapshot, StoreError> {
        let _lock = self.lock(false)?;
        super::snapshot::Captured::open(&self.current_header()?)?.repair_prefix(
            through,
            self.path.with_extension("repair-required").try_exists()?,
        )
    }
    /// Explicit corruption repair, retaining every valid closed group. The
    /// caller must rebuild the tape projection when cutting below its mark.
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
            operator: operator_name(),
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
    #[serde(with = "hex_bytes")]
    pub digest: [u8; 32],
    /// Original byte length.
    pub original_len: u64,
    /// Installed exclusive byte end.
    pub retained_end: u64,
    /// Validated last closed sequence.
    pub through_sequence: u64,
}
/// Read-only damaged-tail diagnosis; no missing authority is inferred.
#[derive(Debug, serde::Serialize)]
pub struct TailInspection {
    /// Last fully validated closing sequence.
    pub through_sequence: u64,
    /// Exclusive end of that closed prefix.
    pub valid_bytes: u64,
    /// Captured file length.
    pub original_bytes: u64,
    /// Corruption or unclosed suffix, absent for a complete log.
    pub damage: Option<String>,
}
/// Release fault seam installed by the actor. `after` is true only after log
/// fsync and first-creation directory fsync, before any summary or SQL projection.
#[derive(Clone)]
pub struct PublicationObserver(pub std::sync::Arc<PublicationCallback>);
type PublicationCallback = dyn Fn(&Record, u64, bool) -> io::Result<()> + Send + Sync;
impl std::fmt::Debug for PublicationObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PublicationObserver")
    }
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
    repair_pending: bool,
    projected_through_sequence: u64,
    publication_observer: Option<PublicationObserver>,
}
impl Lease {
    pub(crate) fn repair_pending(&self) -> bool {
        self.repair_pending
    }
    /// The recovery owner has proved the exact retained cursor and completed any
    /// explicitly confirmed physical supersede. Keep the original repair record
    /// and archive; durably record reconciliation before removing its fence.
    pub(crate) fn reconcile_repaired_tail(
        &mut self,
        cursor: CommitCursor,
    ) -> Result<(), StoreError> {
        self.check()?;
        self.check_projection_floor()?;
        if !self.repair_pending {
            return Ok(());
        }
        if cursor != self.summary().cursor {
            return Err(StoreError::Refused(
                "physical repair cursor differs from retained authority",
            ));
        }
        let fence = self.store.path.with_extension("repair-required");
        let record: RepairRecord = serde_json::from_reader(fs::File::open(&fence)?.take(16 * 1024))
            .map_err(|_| StoreError::Refused("invalid repair record"))?;
        if record.tape_uuid != self.header().tape_uuid
            || record.evidence.through_sequence != self.summary().last_closed_sequence
            || record.evidence.retained_end != self.summary().closed_end
            || fs::metadata(self.store.path())?.len() != record.evidence.retained_end
            || file_digest(self.store.path(), record.evidence.retained_end)?
                != record.retained_digest
        {
            return Err(StoreError::Refused(
                "repaired authority changed before physical reconciliation",
            ));
        }
        let result = (|| -> Result<(), StoreError> {
            let mut evidence = Temporary::new(&self.store, "repair-reconciled")?;
            serde_json::to_writer(
                &mut evidence.file,
                &serde_json::json!({
                    "repair": record,
                    "physical_tail_reconciliation_required": false,
                    "observed_partition": cursor.partition,
                    "observed_lba": cursor.lba.to_string(),
                    "tape_log_sequence": self.summary().last_closed_sequence.to_string(),
                    "tape_incarnation": 1,
                    "operator": operator_name(),
                }),
            )
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            evidence.file.sync_all()?;
            evidence.keep = true;
            File::open(&self.store.directory)?.sync_all()?;
            fs::remove_file(&fence)?;
            File::open(&self.store.directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            self.fenced = true;
            self.store.fence_ambiguity();
        } else {
            self.repair_pending = false;
        }
        result
    }
    /// Install the actor's one before/after-fsync pair.
    pub fn set_publication_observer(&mut self, observer: PublicationObserver) {
        self.publication_observer = Some(observer);
    }

    /// Bind the catalog's transactional projection floor before any append or
    /// automatic torn-tail repair. A missing acknowledged group is recovery
    /// required even when its remaining bytes look like an ordinary torn tail.
    pub fn set_projection_floor(&mut self, sequence: u64) -> Result<(), StoreError> {
        if sequence < self.projected_through_sequence {
            return Err(StoreError::Refused("projection floor cannot regress"));
        }
        self.projected_through_sequence = sequence;
        self.check_projection_floor()
    }
    fn check_projection_floor(&self) -> Result<(), StoreError> {
        if self.summary().last_closed_sequence < self.projected_through_sequence {
            return Err(StoreError::BelowProjection {
                requested: self.summary().last_closed_sequence,
                projected: self.projected_through_sequence,
            });
        }
        Ok(())
    }
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
        if self.repair_pending {
            return Err(StoreError::Refused(
                "physical-tail reconciliation is required before publication",
            ));
        }
        GroupWriter::new(
            &self.store,
            self.summary().last_closed_sequence + 1,
            self.summary().closed_end,
        )
    }
    pub(crate) fn check(&self) -> Result<(), StoreError> {
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
    pub fn stage(&mut self, group: GroupWriter) -> Result<Staged<'_>, StoreError> {
        self.stage_inner(group, true)
    }
    fn stage_inner(
        &mut self,
        mut group: GroupWriter,
        before_media: bool,
    ) -> Result<Staged<'_>, StoreError> {
        self.check()?;
        if self.repair_pending {
            return Err(StoreError::Refused(
                "physical-tail reconciliation is required before publication",
            ));
        }
        self.check_projection_floor()?;
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
        if before_media && group.next != group.sequence {
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
        self.stage_inner(
            writer,
            !matches!(
                closer,
                Record::TerminalPrefix { .. } | Record::ComponentWritten { .. }
            ),
        )?
        .complete(&closer)
    }
    /// Stage provisional frames while retaining the exclusive OS lease, without
    /// keeping a Rust mutex guard across media I/O. Only this owner can complete.
    pub fn stage_detached(&mut self, group: GroupWriter) -> Result<StagedGroup, StoreError> {
        let Staged {
            group,
            trial,
            output,
            temporary,
            ..
        } = self.stage(group)?;
        Ok(StagedGroup {
            group,
            trial,
            output,
            temporary,
        })
    }
    /// Cancel a group for which no closing append has been attempted. Only a
    /// successfully synced rollback makes abandonment of the media suffix safe.
    pub(crate) fn cancel_detached(&mut self, staged: StagedGroup) -> Result<(), StoreError> {
        if !self.fenced
            || staged.group.sequence != self.summary().last_closed_sequence + 1
            || self.store.has_ambiguity()?
        {
            return Err(StoreError::Fenced);
        }
        let StagedGroup {
            output, temporary, ..
        } = staged;
        drop(output);
        if let Some(mut temp) = temporary {
            fs::remove_file(&temp.path)?;
            temp.keep = true;
            File::open(&self.store.directory)?.sync_all()?;
        } else {
            replace_prefix(&self.store, self.summary().closed_end)?;
        }
        self.fenced = false;
        Ok(())
    }
    /// Close a detached group after the actor has proved the media barrier.
    pub fn complete_detached(
        &mut self,
        staged: StagedGroup,
        closer: &Record,
    ) -> Result<(), StoreError> {
        if !self.fenced || staged.group.sequence != self.summary().last_closed_sequence + 1 {
            return Err(StoreError::Refused(
                "detached group does not belong to this pending lease",
            ));
        }
        let StagedGroup {
            group,
            trial,
            output,
            temporary,
        } = staged;
        Staged {
            lease: self,
            group,
            trial,
            output,
            temporary,
        }
        .complete(closer)
    }
}
/// Provisional frames whose live lease remains fenced until completion. It owns
/// no lock guard; dropping it cannot make the owner reusable.
pub struct StagedGroup {
    group: GroupWriter,
    trial: Fold,
    output: File,
    temporary: Option<Temporary>,
}
impl std::fmt::Debug for StagedGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedGroup")
            .field("sequence", &self.group.sequence)
            .finish_non_exhaustive()
    }
}
impl StagedGroup {
    /// Complete range binding after any post-barrier sidecar rows are appended.
    pub fn reference(&self, members: u64) -> GroupRef {
        self.group.reference(members)
    }
    /// Add only the bounded physical rows learned at the barrier. Catalog
    /// payloads were staged and data-synced before the media operation.
    pub fn push_file_row(&mut self, row: TapeFileEntry) -> Result<(), StoreError> {
        let record = Record::PayloadContinuation(Continuation {
            group: self.group.sequence(),
            chunk: Chunk::FileRow(row),
        });
        let bytes = self.group.encode(&record, &[])?;
        let mut decoder = Decoder::at(bytes.as_slice(), self.group.next, self.group.end);
        let frame = decoder
            .next_frame()?
            .ok_or(StoreError::Refused("empty staged row"))?;
        self.trial.push(&frame).map_err(CodecError::from)?;
        self.output.write_all(&bytes)?;
        self.group.temp.file.seek(SeekFrom::End(0))?;
        self.group.push(&record, &[])
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
            if let Some(observer) = &self.lease.publication_observer {
                (observer.0)(closer, self.group.next, false)?;
            }
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
        if let Some(observer) = &self.lease.publication_observer {
            (observer.0)(closer, self.group.next, true)
                .map_err(|e| StoreError::AfterDurable(Box::new(e.into())))?;
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
#[cfg(test)]
thread_local! {
    static ON_READER_CONTENTION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(crate) fn on_reader_contention(hook: impl FnOnce() + 'static) {
    ON_READER_CONTENTION.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
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
        #[cfg(test)]
        ON_READER_CONTENTION.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
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
    operator: String,
    #[serde(with = "hex_bytes")]
    tape_uuid: [u8; 16],
    #[serde(with = "hex_bytes")]
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

// Stable operator evidence, independent of Rust's diagnostic formatting.
fn operator_name() -> String {
    match crate::AuditActor::local_user() {
        crate::AuditActor::User(name) | crate::AuditActor::Service(name) => name,
        crate::AuditActor::System => "system".into(),
    }
}
mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer, const N: usize>(
        bytes: &[u8; N],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }
    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
        deserializer: D,
    ) -> Result<[u8; N], D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.len() != N * 2
            || !text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(serde::de::Error::custom(
                "expected lowercase hexadecimal string",
            ));
        }
        let mut bytes = [0; N];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(bytes)
    }
}
