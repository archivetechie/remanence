//! Durable tape-file journal for Layer 3c committed bundles.
//!
//! Layer 3c v0.7.2 decouples restart persistence from any database. The
//! write path records each filemark-durable commit unit as a
//! [`CommittedBundle`] through a [`TapeFileJournal`]. The default
//! [`FileTapeFileJournal`] is a local append-only file: a fixed header followed
//! by length-prefixed canonical CBOR bundle records with CRC-64/XZ checksums.
//! Replay stops at the first torn trailing record and then filters valid
//! records through the last `CheckpointedThrough` marker. Later valid bundles
//! are surfaced as orphans because their tape writes were not included in a
//! daemon checkpoint projection.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use ciborium::value::Value as CborValue;
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};

use crate::bootstrap::{
    decode_bootstrap_object_row_cbor, encode_bootstrap_object_row_cbor,
    validate_bootstrap_object_row, BootstrapObjectRow,
};
use crate::error::ParityError;
use crate::filemark_map::{FilemarkMap, TapeFileKind, TapeFileMapEntry};
use crate::mapping::data_shards_per_epoch;
use crate::model::ParityScheme;
use crate::sidecar::crc64_xz;

const JOURNAL_MAGIC: &[u8; 8] = b"REMJRNL\x01";
const JOURNAL_VERSION: u16 = 3;
const FIXED_HEADER_LEN_WITHOUT_SCHEME: usize = 8 + 2 + 16 + 4 + 1 + 2 + 2 + 4 + 2;
const MAX_RECORD_LEN: u64 = 64 * 1024 * 1024;

/// Durable-append journal failures from Layer 3c v0.7.2 §10.2/§10.6.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// Underlying filesystem I/O failed.
    #[error("journal I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Existing journal header does not match the tape UUID, block size,
    /// compression-off precondition, or parity scheme requested by this
    /// session.
    #[error("journal header mismatch (tape_uuid / scheme / block_size / drive_compression)")]
    HeaderMismatch,
    /// Journal record encoding or decoding failed.
    #[error("journal encode/decode: {0}")]
    Codec(String),
    /// Journal path is on a filesystem class that cannot be trusted as a
    /// crash-recovery commit point.
    #[error(
        "journal volume rejected: {0} (must be a trusted local volume with honored fsync; §10.6)"
    )]
    UntrustedVolume(String),
}

impl JournalError {
    /// True when a non-blocking journal lock could not be acquired because
    /// another append or replay handle already owns the per-tape journal.
    pub fn is_lock_contended(&self) -> bool {
        matches!(self, JournalError::Io(err) if err.kind() == std::io::ErrorKind::WouldBlock)
    }
}

/// One journaled tape-file row inside a committed bundle.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TapeFileEntry {
    /// Dense filemark-delimited tape-file number.
    pub tape_file_number: u32,
    /// Structural file kind.
    pub kind: TapeFileKind,
    /// Fixed-block count before the trailing filemark.
    pub block_count: u64,
    /// Advisory physical LOCATE hint. Map validation never trusts this alone.
    pub physical_start_hint: Option<u64>,
    /// Optional higher-layer object identifier. Layer 3c does not interpret it.
    pub object_id: Option<String>,
    /// First parity data ordinal for object tape files.
    pub first_parity_data_ordinal: Option<u64>,
    /// Epoch ID for parity-sidecar rows.
    pub epoch_id: Option<u64>,
    /// First protected ordinal for parity-sidecar rows.
    pub protected_ordinal_start: Option<u64>,
    /// End-exclusive protected ordinal for parity-sidecar rows.
    pub protected_ordinal_end_exclusive: Option<u64>,
    /// Canonical metadata hash for sidecar or parity_map control files.
    pub canonical_metadata_hash: Option<[u8; 32]>,
    /// Optional higher-layer bootstrap object row for object tape files.
    pub bootstrap_object_row: Option<BootstrapObjectRow>,
}

impl TapeFileEntry {
    /// Convert the structural part of a filemark-map row into a journal row.
    pub fn from_map_entry(entry: TapeFileMapEntry) -> Self {
        Self {
            tape_file_number: entry.tape_file_number,
            kind: entry.kind,
            block_count: entry.block_count,
            physical_start_hint: None,
            object_id: None,
            first_parity_data_ordinal: entry.first_parity_data_ordinal,
            epoch_id: entry.epoch_id,
            protected_ordinal_start: entry.protected_ordinal_start,
            protected_ordinal_end_exclusive: entry.protected_ordinal_end_exclusive,
            canonical_metadata_hash: None,
            bootstrap_object_row: None,
        }
    }

    /// Return the structural filemark-map row represented by this journal row.
    pub fn to_map_entry(&self) -> TapeFileMapEntry {
        TapeFileMapEntry {
            tape_file_number: self.tape_file_number,
            kind: self.kind,
            block_count: self.block_count,
            first_parity_data_ordinal: self.first_parity_data_ordinal,
            protected_ordinal_start: self.protected_ordinal_start,
            protected_ordinal_end_exclusive: self.protected_ordinal_end_exclusive,
            epoch_id: self.epoch_id,
        }
    }
}

impl From<TapeFileMapEntry> for TapeFileEntry {
    fn from(entry: TapeFileMapEntry) -> Self {
        Self::from_map_entry(entry)
    }
}

/// Operational kind for one atomic committed bundle.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum CommittedBundleKind {
    /// Object tape file plus the sidecars it completed.
    Object,
    /// Control tape files such as checkpoint bootstraps and parity maps.
    Control,
    /// Sidecars generated during restart append recovery.
    ResumeSidecars,
    /// Final sidecar/bootstrap bundle at tape close.
    Finish,
    /// Watermark proving that all preceding bundles were projected by the
    /// shared checkpoint barrier.
    CheckpointedThrough,
}

/// One atomic journal commit unit.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CommittedBundle {
    /// Operational bundle kind.
    pub kind: CommittedBundleKind,
    /// Tape-file rows made durable by this commit, in ascending tape-file
    /// order.
    pub entries: Vec<TapeFileEntry>,
    /// Protection watermark after this bundle.
    pub highest_protected_ordinal: u64,
    /// Total committed object-data ordinals after this bundle.
    pub total_committed_ordinals: u64,
}

/// Replay result for the committed prefix stored in a journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedState {
    /// Committed tape-file rows in ascending tape-file order.
    pub entries: Vec<TapeFileEntry>,
    /// Highest protected ordinal `W`.
    pub highest_protected_ordinal: u64,
    /// Total committed object-data ordinals `T`.
    pub total_committed_ordinals: u64,
    /// Valid bundles after the last checkpoint watermark. These records are
    /// excluded from `entries` and the committed watermarks.
    pub orphaned_bundles: Vec<CommittedBundle>,
}

impl CommittedState {
    /// Build the structural filemark map for this committed prefix.
    pub fn filemark_map(&self) -> Result<FilemarkMap, ParityError> {
        FilemarkMap::new(
            self.entries
                .iter()
                .map(TapeFileEntry::to_map_entry)
                .collect(),
        )
    }

    /// Validate the v0.7.2 restart invariant for a committed prefix.
    ///
    /// A full epoch auto-closes, so restart only has to rebuild the one open
    /// explicit range after `W`. This bound does not imply that `W` is aligned
    /// to `S*k`: a prior barrier may have closed a short epoch at any ordinal.
    /// If `T - W` reaches a full epoch, the auto-close invariant was violated
    /// and the journal must not be resumed in production mode.
    pub fn validate_v1_restart_bound(&self, scheme: &ParityScheme) -> Result<(), ParityError> {
        if self.highest_protected_ordinal > self.total_committed_ordinals {
            return Err(ParityError::ResumeAppend(format!(
                "journal committed state is incoherent: W={} exceeds T={}",
                self.highest_protected_ordinal, self.total_committed_ordinals
            )));
        }
        let live_ordinals = self
            .total_committed_ordinals
            .checked_sub(self.highest_protected_ordinal)
            .ok_or(ParityError::Invariant(
                "committed-state W/T arithmetic underflows",
            ))?;
        let epoch_data_shards = data_shards_per_epoch(scheme)?;
        if live_ordinals >= epoch_data_shards {
            return Err(ParityError::ResumeAppend(format!(
                "journal committed prefix has {live_ordinals} unprotected ordinals, \
                 exceeding the v1 restart bound of one partial epoch ({epoch_data_shards})"
            )));
        }
        Ok(())
    }
}

/// Durable append surface for Layer 3c committed tape-file bundles.
pub trait TapeFileJournal {
    /// Tape UUID this journal belongs to.
    fn tape_uuid(&self) -> [u8; 16];

    /// Atomically and durably append one committed bundle. Returns only after
    /// the append is fsynced.
    fn commit_bundle(&mut self, bundle: &CommittedBundle) -> Result<(), JournalError>;

    /// Replay committed entries.
    ///
    /// File-backed append journals may repair crash state while replaying:
    /// [`FileTapeFileJournal::load_committed`] truncates a torn trailing record
    /// even though this method takes `&self`. Callers should treat replay as
    /// recovery I/O, not as a purely read-only query.
    fn load_committed(&self) -> Result<CommittedState, JournalError>;
}

/// Append-only file-backed implementation of [`TapeFileJournal`].
#[derive(Debug)]
pub struct FileTapeFileJournal {
    file: Flock<File>,
    path: PathBuf,
    tape_uuid: [u8; 16],
    block_size: u32,
    drive_compression: bool,
    scheme: ParityScheme,
    first_create: bool,
    last_highest_protected_ordinal: u64,
    last_total_committed_ordinals: u64,
    orphaned_bundles_truncated_on_open: usize,
}

/// Read-only, shared-lock replay handle for a file-backed tape journal.
///
/// Layer 4 projections use this handle to replay the 3c journal through the
/// 3c-owned framing and validation code without acquiring the exclusive append
/// lock or reparsing `.remjournal` bytes directly.
#[derive(Debug)]
pub struct FileTapeFileJournalReader {
    file: Flock<File>,
    path: PathBuf,
    tape_uuid: [u8; 16],
    block_size: u32,
    drive_compression: bool,
    scheme: ParityScheme,
}

impl FileTapeFileJournal {
    /// Open or create a local journal file, rejecting untrusted filesystem
    /// classes before any header or record is written.
    pub fn open(
        path: impl AsRef<Path>,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        validate_trusted_volume(&path)?;
        Self::open_inner(path, tape_uuid, block_size, scheme)
    }

    /// Open an existing local journal for read-only replay under a shared,
    /// non-blocking lock.
    ///
    /// This is the Layer 4 ingestion surface: it validates the header and then
    /// exposes only [`FileTapeFileJournalReader::load_committed`]. It conflicts
    /// with the exclusive append-session lock, so callers that see
    /// `ErrorKind::WouldBlock` should retry after the active session releases
    /// the tape.
    pub fn open_shared_for_replay(
        path: impl AsRef<Path>,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<FileTapeFileJournalReader, JournalError> {
        let path = path.as_ref().to_path_buf();
        validate_trusted_volume(&path)?;
        FileTapeFileJournalReader::open_inner(path, tape_uuid, block_size, scheme)
    }

    /// Open an existing local journal for read-only replay, using the journal
    /// header itself as the tape UUID, block-size, and parity-scheme source.
    ///
    /// Layer 4 uses this during catalog rebuilds, where it starts with a
    /// directory of `.remjournal` files and must not trust SQLite to tell it
    /// which tape/scheme each journal belongs to.
    pub fn open_shared_existing_for_replay(
        path: impl AsRef<Path>,
    ) -> Result<FileTapeFileJournalReader, JournalError> {
        let path = path.as_ref().to_path_buf();
        validate_trusted_volume(&path)?;
        FileTapeFileJournalReader::open_existing_inner(path)
    }

    fn open_inner(
        path: PathBuf,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<Self, JournalError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let existed = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let mut file = Flock::lock(file, FlockArg::LockExclusiveNonblock)
            .map_err(|(_file, errno)| JournalError::Io(std::io::Error::from(errno)))?;
        let len = file.metadata()?.len();
        let mut last_highest_protected_ordinal = 0;
        let mut last_total_committed_ordinals = 0;
        let mut orphaned_bundles_truncated_on_open = 0;
        if len == 0 {
            write_header(&mut file, tape_uuid, block_size, &scheme)?;
            file.sync_all()?;
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
        } else {
            let header = read_header(&mut file)?;
            if header.tape_uuid != tape_uuid
                || header.block_size != block_size
                || header.drive_compression
                || header.scheme != scheme
            {
                return Err(JournalError::HeaderMismatch);
            }
            let committed =
                load_committed_from_reader(&mut file, len, ReplayTruncation::TornAndOrphans)?;
            last_highest_protected_ordinal = committed.highest_protected_ordinal;
            last_total_committed_ordinals = committed.total_committed_ordinals;
            orphaned_bundles_truncated_on_open = committed.orphaned_bundles.len();
        }
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            path,
            tape_uuid,
            block_size,
            drive_compression: false,
            scheme,
            first_create: !existed,
            last_highest_protected_ordinal,
            last_total_committed_ordinals,
            orphaned_bundles_truncated_on_open,
        })
    }

    /// Number of valid post-watermark bundles removed by this exclusive open.
    pub fn orphaned_bundles_truncated_on_open(&self) -> usize {
        self.orphaned_bundles_truncated_on_open
    }

    /// Path backing this journal.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Fixed block size copied into the journal header.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Effective drive hardware compression mode copied into the journal
    /// header. Parity-protected v1 journals always record `false`.
    pub fn drive_compression(&self) -> bool {
        self.drive_compression
    }

    /// Parity scheme consistency copy from the journal header.
    pub fn scheme(&self) -> &ParityScheme {
        &self.scheme
    }

    #[cfg(test)]
    fn open_without_volume_check_for_tests(
        path: impl AsRef<Path>,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<Self, JournalError> {
        Self::open_inner(path.as_ref().to_path_buf(), tape_uuid, block_size, scheme)
    }
}

impl FileTapeFileJournalReader {
    fn open_existing_inner(path: PathBuf) -> Result<Self, JournalError> {
        let file = OpenOptions::new().read(true).open(&path)?;
        let mut file = Flock::lock(file, FlockArg::LockSharedNonblock)
            .map_err(|(_file, errno)| JournalError::Io(std::io::Error::from(errno)))?;
        let header = read_header(&mut file)?;
        Ok(Self {
            file,
            path,
            tape_uuid: header.tape_uuid,
            block_size: header.block_size,
            drive_compression: header.drive_compression,
            scheme: header.scheme,
        })
    }

    fn open_inner(
        path: PathBuf,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<Self, JournalError> {
        let file = OpenOptions::new().read(true).open(&path)?;
        let mut file = Flock::lock(file, FlockArg::LockSharedNonblock)
            .map_err(|(_file, errno)| JournalError::Io(std::io::Error::from(errno)))?;
        let header = read_header(&mut file)?;
        if header.tape_uuid != tape_uuid
            || header.block_size != block_size
            || header.drive_compression
            || header.scheme != scheme
        {
            return Err(JournalError::HeaderMismatch);
        }
        Ok(Self {
            file,
            path,
            tape_uuid,
            block_size,
            drive_compression: false,
            scheme,
        })
    }

    /// Tape UUID this replay handle belongs to.
    pub fn tape_uuid(&self) -> [u8; 16] {
        self.tape_uuid
    }

    /// Path backing this replay handle.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Fixed block size copied into the journal header.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Effective drive hardware compression mode copied into the journal
    /// header. Parity-protected v1 journals always record `false`.
    pub fn drive_compression(&self) -> bool {
        self.drive_compression
    }

    /// Parity scheme consistency copy from the journal header.
    pub fn scheme(&self) -> &ParityScheme {
        &self.scheme
    }

    /// Replay committed entries without mutating the journal file.
    ///
    /// Consumes the shared replay handle so Layer 4 projections cannot hold the
    /// shared flock across downstream SQLite work after the committed prefix has
    /// been read.
    pub fn load_committed(self) -> Result<CommittedState, JournalError> {
        let mut file = self.file.try_clone()?;
        let file_len = file.metadata()?.len();
        let header = read_header(&mut file)?;
        if header.tape_uuid != self.tape_uuid
            || header.block_size != self.block_size
            || header.drive_compression != self.drive_compression
            || header.scheme != self.scheme
        {
            return Err(JournalError::HeaderMismatch);
        }
        load_committed_from_reader(&mut file, file_len, ReplayTruncation::No)
    }

    #[cfg(test)]
    fn open_without_volume_check_for_tests(
        path: impl AsRef<Path>,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<Self, JournalError> {
        Self::open_inner(path.as_ref().to_path_buf(), tape_uuid, block_size, scheme)
    }
}

impl TapeFileJournal for FileTapeFileJournal {
    fn tape_uuid(&self) -> [u8; 16] {
        self.tape_uuid
    }

    fn commit_bundle(&mut self, bundle: &CommittedBundle) -> Result<(), JournalError> {
        validate_commit_watermarks(
            bundle,
            self.last_highest_protected_ordinal,
            self.last_total_committed_ordinals,
        )?;
        let payload = encode_bundle(bundle)?;
        append_journal_record_with_rollback(&mut *self.file, &payload)?;
        self.last_highest_protected_ordinal = bundle.highest_protected_ordinal;
        self.last_total_committed_ordinals = bundle.total_committed_ordinals;
        if self.first_create {
            if let Some(parent) = self.path.parent() {
                sync_directory(parent)?;
            }
            self.first_create = false;
        }
        Ok(())
    }

    fn load_committed(&self) -> Result<CommittedState, JournalError> {
        // This intentionally truncates a torn trailing record through a cloned
        // file descriptor. The append lock still belongs to `self`, so replay
        // can repair the journal without requiring a separate mutable handle.
        let mut file = self.file.try_clone()?;
        let file_len = file.metadata()?.len();
        let header_end = read_header(&mut file)?;
        if header_end.tape_uuid != self.tape_uuid
            || header_end.block_size != self.block_size
            || header_end.drive_compression != self.drive_compression
            || header_end.scheme != self.scheme
        {
            return Err(JournalError::HeaderMismatch);
        }
        load_committed_from_reader(&mut file, file_len, ReplayTruncation::TornOnly)
    }
}

trait JournalAppendTarget: Write + Seek {
    fn journal_set_len(&mut self, len: u64) -> std::io::Result<()>;
    fn journal_sync_all(&mut self) -> std::io::Result<()>;
}

impl JournalAppendTarget for File {
    fn journal_set_len(&mut self, len: u64) -> std::io::Result<()> {
        self.set_len(len)
    }

    fn journal_sync_all(&mut self) -> std::io::Result<()> {
        self.sync_all()
    }
}

fn append_journal_record_with_rollback(
    file: &mut impl JournalAppendTarget,
    payload: &[u8],
) -> Result<(), JournalError> {
    let record_len = validate_record_len(payload.len())?;
    let crc = crc64_xz(payload);
    let start = file.seek(SeekFrom::End(0))?;
    let append_result = (|| {
        file.write_all(&record_len.to_le_bytes())?;
        file.write_all(payload)?;
        file.write_all(&crc.to_le_bytes())?;
        file.journal_sync_all()?;
        Ok(())
    })();
    if let Err(err) = append_result {
        if let Err(rollback_err) = rollback_failed_append(file, start) {
            return Err(JournalError::Codec(format!(
                "journal append failed ({err}); rollback to offset {start} failed ({rollback_err})"
            )));
        }
        return Err(err);
    }
    Ok(())
}

fn validate_record_len(payload_len: usize) -> Result<u32, JournalError> {
    if payload_len as u64 > MAX_RECORD_LEN {
        return Err(JournalError::Codec(format!(
            "committed bundle record length {payload_len} exceeds replay limit {MAX_RECORD_LEN}"
        )));
    }
    u32::try_from(payload_len)
        .map_err(|_| JournalError::Codec("committed bundle record exceeds u32 length".into()))
}

fn validate_commit_watermarks(
    bundle: &CommittedBundle,
    last_highest_protected_ordinal: u64,
    last_total_committed_ordinals: u64,
) -> Result<(), JournalError> {
    if bundle.kind == CommittedBundleKind::CheckpointedThrough {
        if !bundle.entries.is_empty() {
            return Err(JournalError::Codec(
                "checkpointed-through bundle must not contain tape-file entries".into(),
            ));
        }
        if bundle.highest_protected_ordinal != last_highest_protected_ordinal
            || bundle.total_committed_ordinals != last_total_committed_ordinals
        {
            return Err(JournalError::Codec(format!(
                "checkpointed-through bundle W/T ({}/{}) must equal preceding journal state ({}/{})",
                bundle.highest_protected_ordinal,
                bundle.total_committed_ordinals,
                last_highest_protected_ordinal,
                last_total_committed_ordinals
            )));
        }
    }
    if bundle.highest_protected_ordinal > bundle.total_committed_ordinals {
        return Err(JournalError::Codec(format!(
            "committed bundle W={} exceeds T={}",
            bundle.highest_protected_ordinal, bundle.total_committed_ordinals
        )));
    }
    if bundle.highest_protected_ordinal < last_highest_protected_ordinal {
        return Err(JournalError::Codec(format!(
            "journal bundle regressed highest_protected_ordinal from {} to {}",
            last_highest_protected_ordinal, bundle.highest_protected_ordinal
        )));
    }
    if bundle.total_committed_ordinals < last_total_committed_ordinals {
        return Err(JournalError::Codec(format!(
            "journal bundle regressed total_committed_ordinals from {} to {}",
            last_total_committed_ordinals, bundle.total_committed_ordinals
        )));
    }
    Ok(())
}

fn load_committed_from_reader(
    file: &mut File,
    file_len: u64,
    truncation: ReplayTruncation,
) -> Result<CommittedState, JournalError> {
    let records_start = file.stream_position()?;
    let mut replay_highest_protected_ordinal = 0;
    let mut replay_total_committed_ordinals = 0;
    let mut valid_end = records_start;
    let mut records = Vec::new();
    loop {
        let record_start = file.stream_position()?;
        let mut len_buf = [0u8; 4];
        match file.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(JournalError::Io(err)),
        }
        let record_len = u64::from(u32::from_le_bytes(len_buf));
        let available = file_len.saturating_sub(record_start).saturating_sub(4);
        if record_len > MAX_RECORD_LEN || record_len.saturating_add(8) > available {
            break;
        }
        let record_len = usize::try_from(record_len)
            .map_err(|_| JournalError::Codec("journal record length does not fit usize".into()))?;
        let mut payload = vec![0u8; record_len];
        if let Err(err) = file.read_exact(&mut payload) {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                break;
            }
            return Err(JournalError::Io(err));
        }
        let mut crc_buf = [0u8; 8];
        if let Err(err) = file.read_exact(&mut crc_buf) {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                break;
            }
            return Err(JournalError::Io(err));
        }
        let expected_crc = u64::from_le_bytes(crc_buf);
        if crc64_xz(&payload) != expected_crc {
            break;
        }
        let bundle = decode_bundle(&payload)?;
        if bundle.highest_protected_ordinal < replay_highest_protected_ordinal {
            return Err(JournalError::Codec(format!(
                "journal bundle regressed highest_protected_ordinal from {} to {}",
                replay_highest_protected_ordinal, bundle.highest_protected_ordinal
            )));
        }
        if bundle.total_committed_ordinals < replay_total_committed_ordinals {
            return Err(JournalError::Codec(format!(
                "journal bundle regressed total_committed_ordinals from {} to {}",
                replay_total_committed_ordinals, bundle.total_committed_ordinals
            )));
        }
        validate_commit_watermarks(
            &bundle,
            replay_highest_protected_ordinal,
            replay_total_committed_ordinals,
        )?;
        replay_highest_protected_ordinal = bundle.highest_protected_ordinal;
        replay_total_committed_ordinals = bundle.total_committed_ordinals;
        valid_end = file.stream_position()?;
        records.push((bundle, valid_end));
    }

    let last_checkpoint_index = records
        .iter()
        .rposition(|(bundle, _)| bundle.kind == CommittedBundleKind::CheckpointedThrough);
    let retained_record_count = last_checkpoint_index.map_or(0, |index| index + 1);
    let retained_end = last_checkpoint_index
        .map(|index| records[index].1)
        .unwrap_or(records_start);
    let orphaned_records = records.split_off(retained_record_count);
    let orphaned_bundles = orphaned_records
        .into_iter()
        .map(|(bundle, _)| bundle)
        .collect();
    let mut entries = Vec::new();
    let mut highest_protected_ordinal = 0;
    let mut total_committed_ordinals = 0;
    for (bundle, _) in records {
        entries.extend(bundle.entries.iter().cloned());
        highest_protected_ordinal = bundle.highest_protected_ordinal;
        total_committed_ordinals = bundle.total_committed_ordinals;
    }

    let truncate_end = match truncation {
        ReplayTruncation::No => None,
        ReplayTruncation::TornOnly => (valid_end < file_len).then_some(valid_end),
        ReplayTruncation::TornAndOrphans => (retained_end < file_len).then_some(retained_end),
    };
    if let Some(truncate_end) = truncate_end {
        file.set_len(truncate_end)?;
        file.seek(SeekFrom::Start(truncate_end))?;
        file.sync_all()?;
    }
    Ok(CommittedState {
        entries,
        highest_protected_ordinal,
        total_committed_ordinals,
        orphaned_bundles,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplayTruncation {
    No,
    TornOnly,
    TornAndOrphans,
}

fn rollback_failed_append(
    file: &mut impl JournalAppendTarget,
    start: u64,
) -> Result<(), JournalError> {
    file.journal_set_len(start)?;
    file.seek(SeekFrom::Start(start))?;
    file.journal_sync_all()?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct JournalHeader {
    tape_uuid: [u8; 16],
    block_size: u32,
    drive_compression: bool,
    scheme: ParityScheme,
}

fn write_header(
    file: &mut File,
    tape_uuid: [u8; 16],
    block_size: u32,
    scheme: &ParityScheme,
) -> Result<(), JournalError> {
    let mut header = Vec::new();
    header.extend_from_slice(JOURNAL_MAGIC);
    header.extend_from_slice(&JOURNAL_VERSION.to_le_bytes());
    header.extend_from_slice(&tape_uuid);
    header.extend_from_slice(&block_size.to_le_bytes());
    header.push(0);
    header.extend_from_slice(&scheme.data_blocks_per_stripe.to_le_bytes());
    header.extend_from_slice(&scheme.parity_blocks_per_stripe.to_le_bytes());
    header.extend_from_slice(&scheme.stripes_per_neighborhood.to_le_bytes());
    let scheme_id = scheme.id.as_str().as_bytes();
    let scheme_id_len = u16::try_from(scheme_id.len())
        .map_err(|_| JournalError::Codec("scheme id exceeds u16 length".into()))?;
    header.extend_from_slice(&scheme_id_len.to_le_bytes());
    header.extend_from_slice(scheme_id);
    let crc = crc64_xz(&header);
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header)?;
    file.write_all(&crc.to_le_bytes())?;
    Ok(())
}

fn read_header(file: &mut File) -> Result<JournalHeader, JournalError> {
    file.seek(SeekFrom::Start(0))?;
    let mut fixed = [0u8; FIXED_HEADER_LEN_WITHOUT_SCHEME];
    file.read_exact(&mut fixed)?;
    if &fixed[..8] != JOURNAL_MAGIC {
        return Err(JournalError::HeaderMismatch);
    }
    let version = u16::from_le_bytes([fixed[8], fixed[9]]);
    if version != JOURNAL_VERSION {
        return Err(JournalError::HeaderMismatch);
    }
    let mut tape_uuid = [0u8; 16];
    tape_uuid.copy_from_slice(&fixed[10..26]);
    let block_size = u32::from_le_bytes(fixed[26..30].try_into().expect("slice length"));
    let drive_compression = match fixed[30] {
        0 => false,
        1 => true,
        _ => return Err(JournalError::HeaderMismatch),
    };
    if drive_compression {
        return Err(JournalError::HeaderMismatch);
    }
    let k = u16::from_le_bytes(fixed[31..33].try_into().expect("slice length"));
    let m = u16::from_le_bytes(fixed[33..35].try_into().expect("slice length"));
    let stripes = u32::from_le_bytes(fixed[35..39].try_into().expect("slice length"));
    let scheme_id_len = u16::from_le_bytes(fixed[39..41].try_into().expect("slice length"));
    let mut scheme_id = vec![0u8; usize::from(scheme_id_len)];
    file.read_exact(&mut scheme_id)?;
    let mut crc_buf = [0u8; 8];
    file.read_exact(&mut crc_buf)?;

    let mut crc_input = fixed.to_vec();
    crc_input.extend_from_slice(&scheme_id);
    if crc64_xz(&crc_input) != u64::from_le_bytes(crc_buf) {
        return Err(JournalError::HeaderMismatch);
    }
    let scheme_id = String::from_utf8(scheme_id)
        .map_err(|err| JournalError::Codec(format!("scheme id is not UTF-8: {err}")))?;
    let scheme = ParityScheme {
        id: crate::model::SchemeId::new_owned(scheme_id),
        data_blocks_per_stripe: k,
        parity_blocks_per_stripe: m,
        stripes_per_neighborhood: stripes,
    };
    Ok(JournalHeader {
        tape_uuid,
        block_size,
        drive_compression,
        scheme,
    })
}

fn sync_directory(path: &Path) -> Result<(), JournalError> {
    let dir = File::open(path)?;
    dir.sync_all()?;
    Ok(())
}

fn validate_trusted_volume(path: &Path) -> Result<(), JournalError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    let fstype = filesystem_type_for_path(&canonical_parent);
    validate_journal_fstype(&canonical_parent, fstype.as_deref())?;
    validate_journal_write_cache(&canonical_parent)
}

fn validate_journal_fstype(
    canonical_parent: &Path,
    fstype: Option<&str>,
) -> Result<(), JournalError> {
    let fstype = fstype.ok_or_else(|| {
        JournalError::UntrustedVolume(format!(
            "cannot determine filesystem type for {}",
            canonical_parent.display()
        ))
    })?;
    if is_untrusted_journal_fstype(fstype) {
        return Err(JournalError::UntrustedVolume(format!(
            "{} is on {fstype}",
            canonical_parent.display()
        )));
    }
    Ok(())
}

fn is_untrusted_journal_fstype(fstype: &str) -> bool {
    fstype == "fuse"
        || fstype == "fuseblk"
        || fstype.starts_with("fuse.")
        || matches!(
            fstype,
            "tmpfs" | "ramfs" | "nfs" | "nfs4" | "cifs" | "smbfs" | "9p" | "overlay" | "overlayfs"
        )
}

#[cfg(target_os = "linux")]
fn validate_journal_write_cache(canonical_parent: &Path) -> Result<(), JournalError> {
    let metadata = fs::metadata(canonical_parent)?;
    let dev = metadata.dev();
    let major = nix::sys::stat::major(dev);
    let minor = nix::sys::stat::minor(dev);
    validate_journal_write_cache_for_dev(
        canonical_parent,
        major,
        minor,
        Path::new("/sys/dev/block"),
    )
}

#[cfg(not(target_os = "linux"))]
fn validate_journal_write_cache(canonical_parent: &Path) -> Result<(), JournalError> {
    Err(JournalError::UntrustedVolume(format!(
        "cannot verify write-cache flush behavior for {} on this platform",
        canonical_parent.display()
    )))
}

#[cfg(target_os = "linux")]
fn validate_journal_write_cache_for_dev(
    canonical_parent: &Path,
    major: u64,
    minor: u64,
    sys_dev_block: &Path,
) -> Result<(), JournalError> {
    let queue = sys_dev_block.join(format!("{major}:{minor}")).join("queue");
    let write_cache_path = queue.join("write_cache");
    let write_cache = fs::read_to_string(&write_cache_path)
        .map_err(|err| {
            JournalError::UntrustedVolume(format!(
                "cannot read {} for {} (device {major}:{minor}; virtual or stacked filesystems such as btrfs may expose anonymous devices without queue/write_cache, so place journals on a trusted local block-backed volume or add explicit operator support): {err}",
                write_cache_path.display(),
                canonical_parent.display()
            ))
        })?
        .trim()
        .to_string();
    match write_cache.as_str() {
        "write through" => Ok(()),
        "write back" => {
            // Conservative v0.7.2 policy: require FUA for write-back journal
            // volumes even though some devices also honor fsync through flush.
            // Operators who hit this rejection need an explicit allowlist before
            // this commit point can be relaxed.
            let fua_path = queue.join("fua");
            let fua = fs::read_to_string(&fua_path)
                .map_err(|err| {
                    JournalError::UntrustedVolume(format!(
                        "{} reports write back cache but {} is unavailable: {err}",
                        canonical_parent.display(),
                        fua_path.display()
                    ))
                })?
                .trim()
                .to_string();
            if fua == "1" {
                Ok(())
            } else {
                Err(JournalError::UntrustedVolume(format!(
                    "{} reports write back cache without FUA support ({}={fua})",
                    canonical_parent.display(),
                    fua_path.display()
                )))
            }
        }
        other => Err(JournalError::UntrustedVolume(format!(
            "{} has unsupported write-cache mode {other:?}",
            canonical_parent.display()
        ))),
    }
}

fn filesystem_type_for_path(path: &Path) -> Option<String> {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        let mut left_fields = left.split_whitespace();
        let Some(mount_point) = left_fields.nth(4) else {
            continue;
        };
        let Some(fstype) = right.split_whitespace().next().map(str::to_string) else {
            continue;
        };
        let mount_path = PathBuf::from(unescape_mountinfo_path(mount_point));
        if path.starts_with(&mount_path) {
            let len = mount_path.as_os_str().len();
            if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
                best = Some((len, fstype));
            }
        }
    }
    best.map(|(_, fstype)| fstype)
}

fn unescape_mountinfo_path(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn encode_bundle(bundle: &CommittedBundle) -> Result<Vec<u8>, JournalError> {
    let entries = bundle
        .entries
        .iter()
        .map(encode_entry)
        .collect::<Result<Vec<_>, _>>()?;
    let value = CborValue::Map(vec![
        (
            CborValue::Integer(1.into()),
            CborValue::Integer(kind_code(bundle.kind).into()),
        ),
        (CborValue::Integer(2.into()), CborValue::Array(entries)),
        (
            CborValue::Integer(3.into()),
            CborValue::Integer(bundle.highest_protected_ordinal.into()),
        ),
        (
            CborValue::Integer(4.into()),
            CborValue::Integer(bundle.total_committed_ordinals.into()),
        ),
    ]);
    let mut bytes = Vec::new();
    ciborium::into_writer(&value, &mut bytes)
        .map_err(|err| JournalError::Codec(format!("bundle CBOR encode failed: {err}")))?;
    Ok(bytes)
}

fn decode_bundle(bytes: &[u8]) -> Result<CommittedBundle, JournalError> {
    let value: CborValue = ciborium::from_reader(bytes)
        .map_err(|err| JournalError::Codec(format!("bundle CBOR decode failed: {err}")))?;
    let CborValue::Map(map) = value else {
        return Err(JournalError::Codec("bundle is not a CBOR map".into()));
    };
    let mut kind = None;
    let mut entries = None;
    let mut highest_protected_ordinal = None;
    let mut total_committed_ordinals = None;
    let mut seen_keys = Vec::new();
    for (key, value) in map {
        let key = decode_map_key(key, &mut seen_keys, "bundle")?;
        match (key, value) {
            (1, CborValue::Integer(value)) => {
                kind = Some(kind_from_code(cbor_u64(value, "kind")?)?)
            }
            (2, CborValue::Array(items)) => {
                entries = Some(
                    items
                        .into_iter()
                        .map(decode_entry)
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
            (3, CborValue::Integer(value)) => {
                highest_protected_ordinal = Some(cbor_u64(value, "highest_protected_ordinal")?)
            }
            (4, CborValue::Integer(value)) => {
                total_committed_ordinals = Some(cbor_u64(value, "total_committed_ordinals")?)
            }
            _ => {}
        }
    }
    Ok(CommittedBundle {
        kind: kind.ok_or_else(|| JournalError::Codec("bundle missing kind".into()))?,
        entries: entries.ok_or_else(|| JournalError::Codec("bundle missing entries".into()))?,
        highest_protected_ordinal: highest_protected_ordinal.ok_or_else(|| {
            JournalError::Codec("bundle missing highest_protected_ordinal".into())
        })?,
        total_committed_ordinals: total_committed_ordinals
            .ok_or_else(|| JournalError::Codec("bundle missing total_committed_ordinals".into()))?,
    })
}

fn encode_entry(entry: &TapeFileEntry) -> Result<CborValue, JournalError> {
    let bootstrap_object_row = match entry.bootstrap_object_row.as_ref() {
        Some(row) => Some(
            encode_bootstrap_object_row_cbor(row)
                .map_err(|err| JournalError::Codec(err.to_string()))?,
        ),
        None => None,
    };
    Ok(CborValue::Map(vec![
        (
            CborValue::Integer(1.into()),
            CborValue::Integer(entry.tape_file_number.into()),
        ),
        (
            CborValue::Integer(2.into()),
            CborValue::Integer(tape_file_kind_code(entry.kind).into()),
        ),
        (
            CborValue::Integer(3.into()),
            CborValue::Integer(entry.block_count.into()),
        ),
        (
            CborValue::Integer(4.into()),
            optional_u64(entry.physical_start_hint),
        ),
        (
            CborValue::Integer(5.into()),
            entry
                .object_id
                .as_ref()
                .map_or(CborValue::Null, |value| CborValue::Text(value.clone())),
        ),
        (
            CborValue::Integer(6.into()),
            optional_u64(entry.first_parity_data_ordinal),
        ),
        (CborValue::Integer(7.into()), optional_u64(entry.epoch_id)),
        (
            CborValue::Integer(8.into()),
            optional_u64(entry.protected_ordinal_start),
        ),
        (
            CborValue::Integer(9.into()),
            optional_u64(entry.protected_ordinal_end_exclusive),
        ),
        (
            CborValue::Integer(10.into()),
            entry
                .canonical_metadata_hash
                .map_or(CborValue::Null, |hash| CborValue::Bytes(hash.to_vec())),
        ),
        (
            CborValue::Integer(11.into()),
            bootstrap_object_row.unwrap_or(CborValue::Null),
        ),
    ]))
}

fn decode_entry(value: CborValue) -> Result<TapeFileEntry, JournalError> {
    let CborValue::Map(map) = value else {
        return Err(JournalError::Codec(
            "journal entry is not a CBOR map".into(),
        ));
    };
    let mut tape_file_number = None;
    let mut kind = None;
    let mut block_count = None;
    let mut physical_start_hint = None;
    let mut object_id = None;
    let mut first_parity_data_ordinal = None;
    let mut epoch_id = None;
    let mut protected_ordinal_start = None;
    let mut protected_ordinal_end_exclusive = None;
    let mut canonical_metadata_hash = None;
    let mut bootstrap_object_row = None;
    let mut seen_keys = Vec::new();
    for (key, value) in map {
        let key = decode_map_key(key, &mut seen_keys, "journal entry")?;
        match (key, value) {
            (1, CborValue::Integer(value)) => {
                tape_file_number = Some(cbor_u32(value, "tape_file_number")?)
            }
            (2, CborValue::Integer(value)) => {
                kind = Some(tape_file_kind_from_code(cbor_u64(value, "kind")?)?)
            }
            (3, CborValue::Integer(value)) => block_count = Some(cbor_u64(value, "block_count")?),
            (4, value) => physical_start_hint = optional_cbor_u64(value, "physical_start_hint")?,
            (5, CborValue::Text(value)) => object_id = Some(value),
            (5, CborValue::Null) => {}
            (6, value) => {
                first_parity_data_ordinal = optional_cbor_u64(value, "first_parity_data_ordinal")?
            }
            (7, value) => epoch_id = optional_cbor_u64(value, "epoch_id")?,
            (8, value) => {
                protected_ordinal_start = optional_cbor_u64(value, "protected_ordinal_start")?
            }
            (9, value) => {
                protected_ordinal_end_exclusive =
                    optional_cbor_u64(value, "protected_ordinal_end_exclusive")?
            }
            (10, CborValue::Bytes(bytes)) => {
                canonical_metadata_hash = Some(bytes.try_into().map_err(|bytes: Vec<u8>| {
                    JournalError::Codec(format!(
                        "canonical metadata hash has length {}, expected 32",
                        bytes.len()
                    ))
                })?)
            }
            (10, CborValue::Null) => {}
            (11, CborValue::Null) => {}
            (11, value) => {
                let row = decode_bootstrap_object_row_cbor(value, None, 2)
                    .map_err(|err| JournalError::Codec(err.to_string()))?;
                validate_bootstrap_object_row(&row, None)
                    .map_err(|err| JournalError::Codec(err.to_string()))?;
                bootstrap_object_row = Some(row);
            }
            _ => {}
        }
    }
    Ok(TapeFileEntry {
        tape_file_number: tape_file_number
            .ok_or_else(|| JournalError::Codec("entry missing tape_file_number".into()))?,
        kind: kind.ok_or_else(|| JournalError::Codec("entry missing kind".into()))?,
        block_count: block_count
            .ok_or_else(|| JournalError::Codec("entry missing block_count".into()))?,
        physical_start_hint,
        object_id,
        first_parity_data_ordinal,
        epoch_id,
        protected_ordinal_start,
        protected_ordinal_end_exclusive,
        canonical_metadata_hash,
        bootstrap_object_row,
    })
}

fn decode_map_key(
    key: CborValue,
    seen_keys: &mut Vec<i128>,
    context: &str,
) -> Result<i128, JournalError> {
    let CborValue::Integer(key) = key else {
        return Err(JournalError::Codec(format!(
            "{context} contains non-integer CBOR map key"
        )));
    };
    let key: i128 = key.into();
    if key <= 0 {
        return Err(JournalError::Codec(format!(
            "{context} contains non-positive CBOR map key {key}"
        )));
    }
    if seen_keys.contains(&key) {
        return Err(JournalError::Codec(format!(
            "{context} contains duplicate CBOR map key {key}"
        )));
    }
    if let Some(previous) = seen_keys.last() {
        if key <= *previous {
            return Err(JournalError::Codec(format!(
                "{context} CBOR map keys are not in canonical order: {key} after {previous}"
            )));
        }
    }
    seen_keys.push(key);
    Ok(key)
}

fn optional_u64(value: Option<u64>) -> CborValue {
    value.map_or(CborValue::Null, |value| CborValue::Integer(value.into()))
}

fn optional_cbor_u64(value: CborValue, field: &str) -> Result<Option<u64>, JournalError> {
    match value {
        CborValue::Null => Ok(None),
        CborValue::Integer(value) => Ok(Some(cbor_u64(value, field)?)),
        _ => Err(JournalError::Codec(format!(
            "{field} is not an optional uint"
        ))),
    }
}

fn kind_code(kind: CommittedBundleKind) -> u64 {
    match kind {
        CommittedBundleKind::Object => 0,
        CommittedBundleKind::Control => 1,
        CommittedBundleKind::ResumeSidecars => 2,
        CommittedBundleKind::Finish => 3,
        CommittedBundleKind::CheckpointedThrough => 4,
    }
}

fn kind_from_code(value: u64) -> Result<CommittedBundleKind, JournalError> {
    match value {
        0 => Ok(CommittedBundleKind::Object),
        1 => Ok(CommittedBundleKind::Control),
        2 => Ok(CommittedBundleKind::ResumeSidecars),
        3 => Ok(CommittedBundleKind::Finish),
        4 => Ok(CommittedBundleKind::CheckpointedThrough),
        _ => Err(JournalError::Codec(format!(
            "unknown bundle kind code {value}"
        ))),
    }
}

fn tape_file_kind_code(kind: TapeFileKind) -> u64 {
    match kind {
        TapeFileKind::Object => 0,
        TapeFileKind::ParitySidecar => 1,
        TapeFileKind::Bootstrap => 2,
        TapeFileKind::ParityMap => 3,
    }
}

fn tape_file_kind_from_code(value: u64) -> Result<TapeFileKind, JournalError> {
    match value {
        0 => Ok(TapeFileKind::Object),
        1 => Ok(TapeFileKind::ParitySidecar),
        2 => Ok(TapeFileKind::Bootstrap),
        3 => Ok(TapeFileKind::ParityMap),
        _ => Err(JournalError::Codec(format!(
            "unknown tape-file kind code {value}"
        ))),
    }
}

fn cbor_u32(value: ciborium::value::Integer, field: &str) -> Result<u32, JournalError> {
    let value: i128 = value.into();
    u32::try_from(value)
        .map_err(|_| JournalError::Codec(format!("{field}: value {value} out of u32 range")))
}

fn cbor_u64(value: ciborium::value::Integer, field: &str) -> Result<u64, JournalError> {
    let value: i128 = value.into();
    u64::try_from(value)
        .map_err(|_| JournalError::Codec(format!("{field}: value {value} out of u64 range")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::default_scheme;
    use crate::model::SchemeId;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_journal_path(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("remanence-{name}-{stamp}.remjournal"))
    }

    fn sample_bundle() -> CommittedBundle {
        CommittedBundle {
            kind: CommittedBundleKind::Object,
            entries: vec![
                {
                    let mut entry =
                        TapeFileEntry::from_map_entry(TapeFileMapEntry::object(0, 3, 0));
                    entry.bootstrap_object_row = Some(
                        BootstrapObjectRow::encrypted(0, 3, vec![[0x24; 16], [0x25; 16]], 66, 2377)
                            .with_object_id([0x44; 16]),
                    );
                    entry
                },
                TapeFileEntry {
                    canonical_metadata_hash: Some([0x5A; 32]),
                    ..TapeFileEntry::from_map_entry(TapeFileMapEntry::parity_sidecar(1, 9, 0, 0, 3))
                },
            ],
            highest_protected_ordinal: 3,
            total_committed_ordinals: 3,
        }
    }

    fn sample_checkpoint() -> CommittedBundle {
        CommittedBundle {
            kind: CommittedBundleKind::CheckpointedThrough,
            entries: Vec::new(),
            highest_protected_ordinal: 3,
            total_committed_ordinals: 3,
        }
    }

    fn commit_sample_checkpoint(journal: &mut FileTapeFileJournal) {
        journal
            .commit_bundle(&sample_bundle())
            .expect("commit sample bundle");
        journal
            .commit_bundle(&sample_checkpoint())
            .expect("commit checkpoint watermark");
    }

    fn small_scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("journal-test"),
            data_blocks_per_stripe: 3,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 2,
        }
    }

    struct FaultyAppendTarget {
        bytes: Vec<u8>,
        cursor: u64,
        fail_after_total_bytes: u64,
        sync_count: usize,
    }

    impl FaultyAppendTarget {
        fn new(prefix: &[u8], fail_after_total_bytes: u64) -> Self {
            Self {
                bytes: prefix.to_vec(),
                cursor: prefix.len() as u64,
                fail_after_total_bytes,
                sync_count: 0,
            }
        }
    }

    impl Write for FaultyAppendTarget {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.cursor >= self.fail_after_total_bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "injected journal write failure",
                ));
            }
            let remaining =
                usize::try_from(self.fail_after_total_bytes - self.cursor).unwrap_or(usize::MAX);
            let to_write = remaining.min(buf.len());
            let cursor = usize::try_from(self.cursor)
                .map_err(|_| std::io::Error::other("cursor does not fit usize"))?;
            let end = cursor
                .checked_add(to_write)
                .ok_or_else(|| std::io::Error::other("write end overflows"))?;
            if self.bytes.len() < end {
                self.bytes.resize(end, 0);
            }
            self.bytes[cursor..end].copy_from_slice(&buf[..to_write]);
            self.cursor = end as u64;
            Ok(to_write)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for FaultyAppendTarget {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let next = match pos {
                SeekFrom::Start(offset) => i128::from(offset),
                SeekFrom::End(offset) => self.bytes.len() as i128 + i128::from(offset),
                SeekFrom::Current(offset) => i128::from(self.cursor) + i128::from(offset),
            };
            if next < 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "negative seek",
                ));
            }
            self.cursor = u64::try_from(next)
                .map_err(|_| std::io::Error::other("seek target does not fit u64"))?;
            Ok(self.cursor)
        }
    }

    impl JournalAppendTarget for FaultyAppendTarget {
        fn journal_set_len(&mut self, len: u64) -> std::io::Result<()> {
            let len = usize::try_from(len)
                .map_err(|_| std::io::Error::other("length does not fit usize"))?;
            self.bytes.truncate(len);
            self.cursor = self.cursor.min(len as u64);
            Ok(())
        }

        fn journal_sync_all(&mut self) -> std::io::Result<()> {
            self.sync_count += 1;
            Ok(())
        }
    }

    #[test]
    fn file_journal_round_trips_committed_bundle() {
        let path = temp_journal_path("roundtrip");
        let tape_uuid = [0x42; 16];
        let scheme = default_scheme();
        {
            let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("open journal");
            assert!(
                !journal.drive_compression(),
                "parity journal header must record compression disabled"
            );
            commit_sample_checkpoint(&mut journal);
        }

        let reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("reopen journal");
        assert!(
            !reopened.drive_compression(),
            "reopened journal must preserve compression-disabled header"
        );
        let state = reopened.load_committed().expect("load committed");

        assert_eq!(state.highest_protected_ordinal, 3);
        assert_eq!(state.total_committed_ordinals, 3);
        assert_eq!(state.entries, sample_bundle().entries);
        let map = state.filemark_map().expect("journal map validates");
        assert_eq!(map.entries().len(), 2);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_journal_rejects_compression_enabled_header() {
        let path = temp_journal_path("compression-enabled-header");
        let tape_uuid = [0x43; 16];
        let scheme = default_scheme();
        {
            let mut file = File::create(&path).expect("create journal");
            write_header(&mut file, tape_uuid, 256 * 1024, &scheme).expect("write header");
            file.sync_all().expect("sync header");
        }

        let mut bytes = fs::read(&path).expect("read journal header");
        bytes[30] = 1;
        let crc_start = bytes
            .len()
            .checked_sub(8)
            .expect("journal header includes CRC");
        let crc = crc64_xz(&bytes[..crc_start]);
        bytes[crc_start..].copy_from_slice(&crc.to_le_bytes());
        fs::write(&path, bytes).expect("rewrite mutated header");

        let err = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect_err("compression-enabled journal header must be rejected");

        assert!(matches!(err, JournalError::HeaderMismatch));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_journal_drops_torn_trailing_record() {
        let path = temp_journal_path("torn");
        let tape_uuid = [0x43; 16];
        let scheme = default_scheme();
        {
            let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("open journal");
            commit_sample_checkpoint(&mut journal);
            journal.file.write_all(&99u32.to_le_bytes()).unwrap();
            journal.file.write_all(&[0xAA, 0xBB]).unwrap();
            journal.file.sync_all().unwrap();
        }

        let reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("reopen journal");
        let state = reopened.load_committed().expect("load committed");

        assert_eq!(state.entries, sample_bundle().entries);
        assert_eq!(state.total_committed_ordinals, 3);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_journal_bounds_and_truncates_corrupt_trailing_length() {
        let path = temp_journal_path("oversized-len");
        let tape_uuid = [0x46; 16];
        let scheme = default_scheme();
        let valid_len;
        {
            let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("open journal");
            commit_sample_checkpoint(&mut journal);
            valid_len = journal.file.stream_position().expect("journal offset");
            journal.file.write_all(&u32::MAX.to_le_bytes()).unwrap();
            journal.file.sync_all().unwrap();
        }
        assert!(
            fs::metadata(&path).unwrap().len() > valid_len,
            "fixture should leave a corrupt trailing length prefix"
        );

        let reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("reopen journal");
        let state = reopened.load_committed().expect("load committed");

        assert_eq!(state.entries, sample_bundle().entries);
        assert_eq!(fs::metadata(&path).unwrap().len(), valid_len);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn journal_append_rollback_truncates_partial_record_after_write_error() {
        let prefix = b"valid journal prefix";
        let payload = encode_bundle(&sample_bundle()).expect("bundle encodes");
        let fail_after = prefix.len() as u64 + 4 + 7;
        let mut target = FaultyAppendTarget::new(prefix, fail_after);

        let err = append_journal_record_with_rollback(&mut target, &payload)
            .expect_err("injected write failure should roll back the partial record");

        assert!(matches!(err, JournalError::Io(_)));
        assert_eq!(target.bytes, prefix);
        assert_eq!(target.cursor, prefix.len() as u64);
        assert_eq!(target.sync_count, 1, "rollback must be fsynced");
    }

    #[test]
    fn file_journal_rejects_header_mismatch() {
        let path = temp_journal_path("mismatch");
        let tape_uuid = [0x44; 16];
        let scheme = default_scheme();
        {
            let _journal = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("create journal");
        }

        let err = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            [0x45; 16],
            256 * 1024,
            scheme,
        )
        .expect_err("uuid mismatch should reject");

        assert!(matches!(err, JournalError::HeaderMismatch));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_journal_rejects_second_writer() {
        let path = temp_journal_path("lock");
        let tape_uuid = [0x47; 16];
        let scheme = default_scheme();
        let _first = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme.clone(),
        )
        .expect("first writer opens");

        let err = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect_err("second writer should fail to lock the journal");

        assert!(err.is_lock_contended());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_journal_shared_reader_loads_without_truncating_torn_tail() {
        let path = temp_journal_path("shared-reader");
        let tape_uuid = [0x49; 16];
        let scheme = default_scheme();
        let valid_len;
        {
            let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("open journal");
            commit_sample_checkpoint(&mut journal);
            valid_len = journal.file.stream_position().expect("journal offset");
            journal.file.write_all(&u32::MAX.to_le_bytes()).unwrap();
            journal.file.sync_all().unwrap();
        }
        let len_with_torn_tail = fs::metadata(&path).unwrap().len();
        assert!(len_with_torn_tail > valid_len);

        let reader = FileTapeFileJournalReader::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("open shared reader");
        let state = reader.load_committed().expect("load committed");

        assert_eq!(state.entries, sample_bundle().entries);
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            len_with_torn_tail,
            "read-only replay must not truncate the journal"
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn replay_filters_watermark_orphans_and_exclusive_reopen_truncates_them() {
        let path = temp_journal_path("watermark-orphans");
        let tape_uuid = [0x4B; 16];
        let scheme = default_scheme();
        let checkpoint_len;
        let orphan = CommittedBundle {
            kind: CommittedBundleKind::Object,
            entries: vec![TapeFileEntry::from_map_entry(TapeFileMapEntry::object(
                2, 2, 3,
            ))],
            highest_protected_ordinal: 3,
            total_committed_ordinals: 5,
        };
        {
            let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("open journal");
            commit_sample_checkpoint(&mut journal);
            checkpoint_len = journal.file.stream_position().expect("checkpoint offset");
            journal
                .commit_bundle(&orphan)
                .expect("commit orphan fixture");
        }
        let orphaned_len = fs::metadata(&path).expect("stat orphaned journal").len();
        assert!(orphaned_len > checkpoint_len);

        let reader = FileTapeFileJournalReader::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme.clone(),
        )
        .expect("open shared reader");
        let state = reader.load_committed().expect("shared replay");
        assert_eq!(state.entries, sample_bundle().entries);
        assert_eq!(state.total_committed_ordinals, 3);
        assert_eq!(state.orphaned_bundles, vec![orphan]);
        assert_eq!(fs::metadata(&path).unwrap().len(), orphaned_len);

        let reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("exclusive reopen truncates orphans");
        assert_eq!(reopened.orphaned_bundles_truncated_on_open(), 1);
        assert_eq!(fs::metadata(&path).unwrap().len(), checkpoint_len);
        let repaired = reopened.load_committed().expect("replay repaired journal");
        assert_eq!(repaired.entries, sample_bundle().entries);
        assert!(repaired.orphaned_bundles.is_empty());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn replay_treats_all_bundles_as_orphans_before_the_first_watermark() {
        let path = temp_journal_path("pre-watermark-orphans");
        let tape_uuid = [0x5B; 16];
        let scheme = default_scheme();
        {
            let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("open journal");
            journal
                .commit_bundle(&sample_bundle())
                .expect("commit pre-watermark crash fixture");
        }

        let reader = FileTapeFileJournalReader::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme.clone(),
        )
        .expect("open shared reader");
        let state = reader.load_committed().expect("shared replay");
        assert!(state.entries.is_empty());
        assert_eq!(state.orphaned_bundles, vec![sample_bundle()]);

        let reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("exclusive reopen truncates pre-watermark bundles");
        assert_eq!(reopened.orphaned_bundles_truncated_on_open(), 1);
        let repaired = reopened.load_committed().expect("replay repaired journal");
        assert!(repaired.entries.is_empty());
        assert!(repaired.orphaned_bundles.is_empty());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_journal_shared_reader_serializes_with_append_writer() {
        let path = temp_journal_path("shared-lock");
        let tape_uuid = [0x4A; 16];
        let scheme = default_scheme();
        let writer = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme.clone(),
        )
        .expect("writer opens");

        let err = FileTapeFileJournalReader::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme.clone(),
        )
        .expect_err("shared replay must not run while an append writer owns the journal");
        assert!(err.is_lock_contended());
        drop(writer);

        let reader = FileTapeFileJournalReader::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme.clone(),
        )
        .expect("shared reader opens after writer releases");
        let second_reader = FileTapeFileJournalReader::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme.clone(),
        )
        .expect("multiple shared readers can coexist");

        let err = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect_err("append writer must wait for shared replay handles to close");
        assert!(err.is_lock_contended());

        drop(second_reader);
        drop(reader);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_journal_rejects_regressed_watermarks_at_commit() {
        let path = temp_journal_path("regressed-watermark");
        let tape_uuid = [0x48; 16];
        let scheme = default_scheme();
        {
            let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("open journal");
            journal
                .commit_bundle(&sample_bundle())
                .expect("commit first bundle");
            journal
                .commit_bundle(&sample_checkpoint())
                .expect("commit checkpoint watermark");
            let mut regressed = sample_bundle();
            regressed.highest_protected_ordinal = 2;
            regressed.total_committed_ordinals = 2;
            let err = journal
                .commit_bundle(&regressed)
                .expect_err("regressed W/T watermarks should be rejected before append");
            assert!(matches!(err, JournalError::Codec(_)));
        }

        let reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("reopen journal");
        let state = reopened.load_committed().expect("load committed prefix");

        assert_eq!(
            state.total_committed_ordinals,
            sample_bundle().total_committed_ordinals
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn journal_record_length_is_checked_before_append() {
        let err = validate_record_len((MAX_RECORD_LEN + 1) as usize)
            .expect_err("record above replay limit must be rejected");

        assert!(matches!(err, JournalError::Codec(_)));
        assert_eq!(
            validate_record_len(MAX_RECORD_LEN as usize).expect("max record length"),
            MAX_RECORD_LEN as u32
        );
    }

    #[test]
    fn journal_cbor_rejects_duplicate_and_non_integer_keys() {
        let duplicate_key_bundle = CborValue::Map(vec![
            (CborValue::Integer(1.into()), CborValue::Integer(0.into())),
            (CborValue::Integer(1.into()), CborValue::Integer(0.into())),
        ]);
        let mut bytes = Vec::new();
        ciborium::into_writer(&duplicate_key_bundle, &mut bytes).unwrap();
        let err = decode_bundle(&bytes).expect_err("duplicate keys reject");
        assert!(matches!(err, JournalError::Codec(_)));

        let non_integer_key_bundle = CborValue::Map(vec![(
            CborValue::Text("kind".to_string()),
            CborValue::Integer(0.into()),
        )]);
        let mut bytes = Vec::new();
        ciborium::into_writer(&non_integer_key_bundle, &mut bytes).unwrap();
        let err = decode_bundle(&bytes).expect_err("non-integer keys reject");
        assert!(matches!(err, JournalError::Codec(_)));

        let duplicate_key_entry = CborValue::Map(vec![
            (CborValue::Integer(1.into()), CborValue::Integer(0.into())),
            (CborValue::Integer(1.into()), CborValue::Integer(0.into())),
        ]);
        let err = decode_entry(duplicate_key_entry).expect_err("duplicate entry keys reject");
        assert!(matches!(err, JournalError::Codec(_)));
    }

    #[test]
    fn trusted_volume_policy_fails_closed_and_rejects_virtual_filesystems() {
        let path = Path::new("/journal-dir");
        let err = validate_journal_fstype(path, None)
            .expect_err("unknown filesystem type should fail closed");
        assert!(matches!(err, JournalError::UntrustedVolume(_)));

        for fstype in [
            "tmpfs",
            "ramfs",
            "nfs",
            "nfs4",
            "cifs",
            "smbfs",
            "9p",
            "overlay",
            "overlayfs",
            "fuse",
            "fuseblk",
            "fuse.s3fs",
        ] {
            let err = validate_journal_fstype(path, Some(fstype))
                .expect_err("untrusted filesystem should reject");
            assert!(matches!(err, JournalError::UntrustedVolume(_)));
        }

        validate_journal_fstype(path, Some("ext4")).expect("ordinary local fs passes");
        validate_journal_fstype(path, Some("xfs")).expect("ordinary local fs passes");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_volume_policy_checks_write_cache_and_fua() {
        let root = temp_journal_path("mock-sysfs");
        let sys_dev_block = root.with_extension("sys-dev-block");
        let queue = sys_dev_block.join("8:1").join("queue");
        fs::create_dir_all(&queue).expect("mock queue dir");

        let err =
            validate_journal_write_cache_for_dev(Path::new("/journal-dir"), 259, 0, &sys_dev_block)
                .expect_err("missing sysfs queue is rejected with a useful hint");
        match err {
            JournalError::UntrustedVolume(message) => {
                assert!(message.contains("device 259:0"), "{message}");
                assert!(message.contains("btrfs"), "{message}");
                assert!(message.contains("trusted local block-backed"), "{message}");
            }
            other => panic!("expected UntrustedVolume, got {other:?}"),
        }

        fs::write(queue.join("write_cache"), "write through\n").expect("write mock write_cache");
        validate_journal_write_cache_for_dev(Path::new("/journal-dir"), 8, 1, &sys_dev_block)
            .expect("write-through cache is trusted");

        fs::write(queue.join("write_cache"), "write back\n").expect("write mock write_cache");
        let err =
            validate_journal_write_cache_for_dev(Path::new("/journal-dir"), 8, 1, &sys_dev_block)
                .expect_err("write-back cache without FUA is rejected");
        assert!(matches!(err, JournalError::UntrustedVolume(_)));

        fs::write(queue.join("fua"), "0\n").expect("write mock fua");
        let err =
            validate_journal_write_cache_for_dev(Path::new("/journal-dir"), 8, 1, &sys_dev_block)
                .expect_err("write-back cache with FUA=0 is rejected");
        assert!(matches!(err, JournalError::UntrustedVolume(_)));

        fs::write(queue.join("fua"), "1\n").expect("write mock fua");
        validate_journal_write_cache_for_dev(Path::new("/journal-dir"), 8, 1, &sys_dev_block)
            .expect("write-back cache with FUA=1 is trusted");

        fs::write(queue.join("write_cache"), "mystery\n").expect("write mock write_cache");
        let err =
            validate_journal_write_cache_for_dev(Path::new("/journal-dir"), 8, 1, &sys_dev_block)
                .expect_err("unknown write-cache modes fail closed");
        assert!(matches!(err, JournalError::UntrustedVolume(_)));

        let _ = fs::remove_dir_all(sys_dev_block);
    }

    #[test]
    fn committed_state_validates_restart_bound() {
        let scheme = small_scheme();
        let ok = CommittedState {
            entries: Vec::new(),
            highest_protected_ordinal: 6,
            total_committed_ordinals: 11,
            orphaned_bundles: Vec::new(),
        };
        ok.validate_v1_restart_bound(&scheme)
            .expect("less than one open epoch is resumable");

        let full_epoch = CommittedState {
            entries: Vec::new(),
            highest_protected_ordinal: 6,
            total_committed_ordinals: 12,
            orphaned_bundles: Vec::new(),
        };
        let err = full_epoch
            .validate_v1_restart_bound(&scheme)
            .expect_err("one full open epoch is a corrupt or legacy journal");
        assert!(matches!(err, ParityError::ResumeAppend(_)));

        let incoherent = CommittedState {
            entries: Vec::new(),
            highest_protected_ordinal: 12,
            total_committed_ordinals: 11,
            orphaned_bundles: Vec::new(),
        };
        let err = incoherent
            .validate_v1_restart_bound(&scheme)
            .expect_err("W>T is incoherent");
        assert!(matches!(err, ParityError::ResumeAppend(_)));
    }
}
