//! Durable parity-off checkpoint journal and replayable batch projections.
//!
//! The append-only journal is the numbering and recovery-position authority.
//! A versioned, tape-bound header precedes length-framed JSON records whose
//! CRC-64 covers their version, length, and payload. Each record is fsynced
//! before its corresponding SQLite batch projection. Replay stops at a torn
//! final frame and fails closed on corrupt or unsupported bytes.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};

use crate::{
    NativeObjectCopyProjectionInput, NativeObjectFileProjectionInput, NativeObjectProjectionInput,
    StateError,
};

const CHECKPOINT_JOURNAL_SUFFIX: &str = ".remcheckpoint";
const CHECKPOINT_JOURNAL_MAGIC: &[u8; 8] = b"REMCKPT\x01";
const CHECKPOINT_SEALING_INTENT_MAGIC: &[u8; 8] = b"REMSEAL\x01";
const CHECKPOINT_JOURNAL_HEADER_LEN: u64 = 8 + 16 + 8;
const CHECKPOINT_RECORD_VERSION: u16 = 2;
const CHECKPOINT_RECORD_PREFIX_LEN: u64 = 2 + 4;
const MAX_CHECKPOINT_RECORD_LEN: u64 = 64 * 1024 * 1024;

/// Stable journal representation of one REM-OBJECT recovery row carried by an
/// on-tape checkpoint bootstrap.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CheckpointBootstrapObjectRow {
    /// Filemark-delimited tape-file number of the object copy.
    pub tape_file_number: u32,
    /// Number of fixed-size records occupied by the stored copy.
    pub stored_block_count: u64,
    /// Verbatim 1–64-byte REM-OBJECT object identifier.
    pub object_id: Vec<u8>,
    /// Representation-specific recovery anchors.
    pub representation: CheckpointBootstrapObjectRepresentation,
}

/// Stable representation-specific payload for a checkpoint bootstrap row.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CheckpointBootstrapObjectRepresentation {
    /// Plaintext REM-OBJECT manifest anchors.
    Plaintext {
        /// Object-local body LBA of the manifest payload.
        manifest_first_chunk_lba: u64,
        /// Manifest byte length.
        manifest_size_bytes: u64,
        /// Manifest block count.
        manifest_chunk_count: u64,
        /// SHA-256 digest of the manifest CBOR.
        manifest_sha256: [u8; 32],
    },
    /// Encrypted REM-OBJECT envelope anchors.
    Encrypted {
        /// Recipient epoch identifiers in the key frame.
        recipient_epoch_ids: Vec<[u8; 16]>,
        /// Encrypted metadata frame length.
        metadata_frame_len: u64,
        /// Serialized key-frame length.
        key_frame_len: u32,
    },
}

impl CheckpointBootstrapObjectRow {
    /// Convert the stable journal row into the Layer 3c bootstrap row.
    pub fn to_parity_row(&self) -> remanence_parity::BootstrapObjectRow {
        let row = match &self.representation {
            CheckpointBootstrapObjectRepresentation::Plaintext {
                manifest_first_chunk_lba,
                manifest_size_bytes,
                manifest_chunk_count,
                manifest_sha256,
            } => remanence_parity::BootstrapObjectRow::plaintext(
                self.tape_file_number,
                self.stored_block_count,
                *manifest_first_chunk_lba,
                *manifest_size_bytes,
                *manifest_chunk_count,
                *manifest_sha256,
            ),
            CheckpointBootstrapObjectRepresentation::Encrypted {
                recipient_epoch_ids,
                metadata_frame_len,
                key_frame_len,
            } => remanence_parity::BootstrapObjectRow::encrypted(
                self.tape_file_number,
                self.stored_block_count,
                recipient_epoch_ids.clone(),
                *metadata_frame_len,
                *key_frame_len,
            ),
        };
        row.with_object_id(self.object_id.clone())
    }
}

/// Replayable SQLite projection for one parity-off object in a checkpoint.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CheckpointObjectProjection {
    /// Catalog object row.
    pub object: NativeObjectProjectionInput,
    /// Catalog member-file rows.
    pub files: Vec<NativeObjectFileProjectionInput>,
    /// The single committed copy on this tape.
    pub copy: NativeObjectCopyProjectionInput,
    /// Fixed tape block size.
    pub block_size: u32,
    /// Stored object block count before its delimiter.
    pub block_count: u64,
    /// Whether this object's bundle also projects the BOT bootstrap tape file.
    pub fresh_tape: bool,
    /// Cumulative committed object-data ordinals after this object.
    pub total_committed_ordinals: u64,
    /// REM-OBJECT recovery row emitted in every later checkpoint bootstrap.
    pub bootstrap_object_row: CheckpointBootstrapObjectRow,
}

/// One fsynced checkpoint or terminal-seal authority record.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CheckpointJournalRecord {
    /// Monotonic checkpoint ordinal, starting at one.
    pub ordinal: u64,
    /// Cumulative committed object count after this checkpoint.
    pub committed_object_count: u64,
    /// Barrier-proved EOD partition.
    pub eod_partition: u32,
    /// Barrier-proved EOD logical block address.
    pub eod_lba: u64,
    /// Physical tape UUID, independent of library identity.
    pub tape_uuid: [u8; 16],
    /// Session batch identifier associated with this authority transition.
    pub batch_id: [u8; 16],
    /// Tape-file number occupied by this checkpoint's on-tape bootstrap.
    pub checkpoint_tape_file_number: u32,
    /// Fixed tape block size used to encode that bootstrap.
    pub block_size: u32,
    /// Replayable object projections made durable by an ordinary checkpoint.
    /// Terminal-seal records carry no objects.
    pub objects: Vec<CheckpointObjectProjection>,
    /// Parity scheme for parity-protected checkpoint batches. `None` denotes
    /// the historical parity-off record shape.
    #[serde(default)]
    pub scheme: Option<remanence_parity::ParityScheme>,
    /// Per-object Layer 3c bundles, in the same order as `objects`.
    #[serde(default)]
    pub object_tape_file_bundles: Vec<remanence_parity::CommittedBundle>,
    /// Sidecar/bootstrap bundle emitted by an ordinary parity barrier, or the
    /// `Finish` bundle that identifies a terminal-seal record.
    #[serde(default)]
    pub checkpoint_bundle: Option<remanence_parity::CommittedBundle>,
    /// Whether this objectless record proves the tape's terminal boundary.
    ///
    /// The checkpoint journal is the durable Layer 5 authority for replaying
    /// the SQLite `sealed` projection after a crash. A true value requires an
    /// empty `objects` list and a `Finish` bundle naming the terminal
    /// Bootstrap. The record is appended only after terminal media and its
    /// synchronous barrier have succeeded.
    pub sealed_after_write: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct CheckpointJournalFrame {
    records: Vec<CheckpointJournalRecord>,
}

/// Append-only per-tape checkpoint journal.
#[derive(Debug)]
pub struct FileCheckpointJournal {
    path: PathBuf,
    tape_uuid: [u8; 16],
}

/// Exclusive per-tape checkpoint authority retained across replay, media I/O,
/// and the next durable checkpoint append.
#[derive(Debug)]
pub struct FileCheckpointJournalLease {
    path: PathBuf,
    tape_uuid: [u8; 16],
    file: File,
    _lock: Flock<File>,
}

impl FileCheckpointJournal {
    /// Open or create the journal handle for `tape_uuid` beneath `dir`.
    pub fn open(dir: impl AsRef<Path>, tape_uuid: [u8; 16]) -> Result<Self, StateError> {
        let dir = dir.as_ref();
        let created_dir = !dir.exists();
        fs::create_dir_all(dir)
            .map_err(|err| StateError::io_at("create checkpoint journal directory", dir, err))?;
        if created_dir {
            let parent = dir.parent().ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "checkpoint journal directory has no parent".to_string(),
                )
            })?;
            File::open(parent)
                .and_then(|parent| parent.sync_all())
                .map_err(|err| {
                    StateError::io_at("fsync checkpoint journal parent directory", parent, err)
                })?;
        }
        let path = checkpoint_journal_path(dir, tape_uuid);
        remanence_parity::validate_trusted_journal_volume(&path).map_err(|err| match err {
            remanence_parity::JournalError::UntrustedVolume(detail) => {
                StateError::UntrustedStateVolume(detail)
            }
            other => StateError::JournalReplayFailed(format!(
                "checkpoint trusted-volume validation failed: {other}"
            )),
        })?;
        Ok(Self { path, tape_uuid })
    }

    /// Acquire the exclusive lease that write paths retain from authority
    /// replay through media work and checkpoint append.
    pub fn acquire_exclusive(&self) -> Result<FileCheckpointJournalLease, StateError> {
        self.acquire_exclusive_inner(false)
    }

    /// Acquire an exclusive lease for a supervised physical-tail
    /// reconciliation of a pending `Sealing` intent.
    pub fn acquire_exclusive_for_terminal_recovery(
        &self,
    ) -> Result<FileCheckpointJournalLease, StateError> {
        self.acquire_exclusive_inner(true)
    }

    fn acquire_exclusive_inner(
        &self,
        allow_pending_terminal_intent: bool,
    ) -> Result<FileCheckpointJournalLease, StateError> {
        let lock = acquire_checkpoint_lock(
            &self.path,
            FlockArg::LockExclusiveNonblock,
            "lock checkpoint journal for write session",
        )?;
        let file = if self.path.exists() {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.path)
                .map_err(|err| StateError::io_at("open checkpoint journal", &self.path, err))?
        } else {
            initialize_checkpoint_file(&self.path, self.tape_uuid)?
        };
        let mut lease = FileCheckpointJournalLease {
            path: self.path.clone(),
            tape_uuid: self.tape_uuid,
            file,
            _lock: lock,
        };
        if allow_pending_terminal_intent {
            replay_checkpoint_records(&mut lease.file, lease.tape_uuid, &lease.path)?;
            if !terminal_intent_pending(&lease.path, lease.tape_uuid)? {
                return Err(StateError::JournalReplayFailed(
                    "terminal recovery lease requested without a pending Sealing intent"
                        .to_string(),
                ));
            }
        } else {
            lease.replay()?;
        }
        Ok(lease)
    }

    /// Append and fsync one validated checkpoint record under a short-lived
    /// exclusive lease. Production write paths should retain a lease across
    /// their replay and media work instead.
    pub fn append(&self, record: &CheckpointJournalRecord) -> Result<(), StateError> {
        self.acquire_exclusive()?.append(record)
    }

    /// Replay every record, failing closed on a torn final frame.
    pub fn replay(&self) -> Result<Vec<CheckpointJournalRecord>, StateError> {
        let _lock = acquire_checkpoint_lock(
            &self.path,
            FlockArg::LockSharedNonblock,
            "lock checkpoint journal for replay",
        )?;
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let records = Vec::new();
                enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &records, false)?;
                return Ok(records);
            }
            Err(err) => {
                return Err(StateError::io_at(
                    "open checkpoint journal",
                    &self.path,
                    err,
                ));
            }
        };
        let records = replay_checkpoint_records(&mut file, self.tape_uuid, &self.path)?;
        enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &records, false)?;
        Ok(records)
    }

    /// Return the final fsynced checkpoint, if any.
    pub fn last(&self) -> Result<Option<CheckpointJournalRecord>, StateError> {
        Ok(self.replay()?.pop())
    }

    /// Filesystem path used by this journal.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl FileCheckpointJournalLease {
    /// Replay the authority while retaining the exclusive lease.
    pub fn replay(&mut self) -> Result<Vec<CheckpointJournalRecord>, StateError> {
        let records = replay_checkpoint_records(&mut self.file, self.tape_uuid, &self.path)?;
        enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &records, true)?;
        Ok(records)
    }

    /// Durably enter the recovery-visible `Sealing` admission state.
    ///
    /// This intent is not sealed authority. It is written before terminal tape
    /// motion so process loss between the tape barrier and checkpoint fsync
    /// cannot make a finalized physical tail appear appendable.
    pub fn begin_terminal_transition(&mut self) -> Result<(), StateError> {
        let records = self.replay()?;
        if records
            .last()
            .is_some_and(|record| record.sealed_after_write)
        {
            return Err(StateError::JournalReplayFailed(
                "cannot begin a terminal transition after sealed authority".to_string(),
            ));
        }
        write_terminal_intent(&self.path, self.tape_uuid)
    }

    /// Append terminal authority and clear its durable `Sealing` intent only
    /// after the complete journal frame is fsynced.
    pub fn append_terminal_transition(
        &mut self,
        records: &[CheckpointJournalRecord],
    ) -> Result<(), StateError> {
        if !terminal_intent_pending(&self.path, self.tape_uuid)? {
            return Err(StateError::JournalReplayFailed(
                "terminal checkpoint transition has no durable Sealing intent".to_string(),
            ));
        }
        if !records
            .last()
            .is_some_and(|record| record.sealed_after_write)
        {
            return Err(StateError::JournalReplayFailed(
                "terminal checkpoint transition does not end in sealed authority".to_string(),
            ));
        }
        self.append_batch_inner(records, true)?;
        clear_terminal_intent(&self.path)
    }

    /// Clear a pending `Sealing` intent after a supervised physical probe has
    /// proved that no terminal media exists beyond the named durable EOD.
    pub fn clear_terminal_intent_after_absent_tail(
        &mut self,
        expected_checkpoint_ordinal: Option<u64>,
        expected_eod_lba: u64,
    ) -> Result<(), StateError> {
        if !terminal_intent_pending(&self.path, self.tape_uuid)? {
            return Err(StateError::JournalReplayFailed(
                "terminal recovery found no pending Sealing intent".to_string(),
            ));
        }
        let records = replay_checkpoint_records(&mut self.file, self.tape_uuid, &self.path)?;
        if records
            .last()
            .is_some_and(|record| record.sealed_after_write)
        {
            return Err(StateError::JournalReplayFailed(
                "terminal authority is already durable; absent-tail recovery cannot clear it"
                    .to_string(),
            ));
        }
        let actual_ordinal = records.last().map(|record| record.ordinal);
        let actual_eod = records.last().map_or(0, |record| record.eod_lba);
        if actual_ordinal != expected_checkpoint_ordinal || actual_eod != expected_eod_lba {
            return Err(StateError::JournalReplayFailed(format!(
                "terminal recovery authority changed: expected ordinal {expected_checkpoint_ordinal:?} EOD {expected_eod_lba}, found ordinal {actual_ordinal:?} EOD {actual_eod}"
            )));
        }
        clear_terminal_intent(&self.path)
    }

    /// Validate, append, and fsync one checkpoint while retaining the lease.
    pub fn append(&mut self, record: &CheckpointJournalRecord) -> Result<(), StateError> {
        self.append_batch(std::slice::from_ref(record))
    }

    /// Validate and fsync one indivisible ordered checkpoint transition.
    ///
    /// A watermark seal uses this to place the ordinary object checkpoint and
    /// its terminal-only seal authority in one length-and-integrity-protected
    /// frame. Replay therefore observes both records or fails closed on a torn
    /// frame; it cannot publish only the ordinary half.
    pub fn append_batch(&mut self, records: &[CheckpointJournalRecord]) -> Result<(), StateError> {
        self.append_batch_inner(records, false)
    }

    fn append_batch_inner(
        &mut self,
        records: &[CheckpointJournalRecord],
        terminal_transition: bool,
    ) -> Result<(), StateError> {
        if records.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "checkpoint journal frame must contain at least one record".to_string(),
            ));
        }
        if !terminal_transition && records.iter().any(|record| record.sealed_after_write) {
            return Err(StateError::JournalReplayFailed(
                "sealed checkpoint authority requires a durable Sealing intent".to_string(),
            ));
        }
        let prior = if terminal_transition {
            replay_checkpoint_records(&mut self.file, self.tape_uuid, &self.path)?
        } else {
            self.replay()?
        };
        let mut previous = prior.last();
        for record in records {
            if record.tape_uuid != self.tape_uuid {
                return Err(StateError::JournalReplayFailed(
                    "checkpoint record tape_uuid does not match journal".to_string(),
                ));
            }
            validate_next_record(previous, record)?;
            previous = Some(record);
        }
        let payload = serde_json::to_vec(&CheckpointJournalFrame {
            records: records.to_vec(),
        })
        .map_err(|err| {
            StateError::JournalReplayFailed(format!("encode checkpoint journal frame: {err}"))
        })?;
        let payload_len = u32::try_from(payload.len()).map_err(|_| {
            StateError::JournalReplayFailed("checkpoint record length does not fit u32".to_string())
        })?;
        if u64::from(payload_len) > MAX_CHECKPOINT_RECORD_LEN {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint record length {payload_len} exceeds replay limit {MAX_CHECKPOINT_RECORD_LEN}"
            )));
        }
        let mut frame = Vec::with_capacity(
            usize::try_from(CHECKPOINT_RECORD_PREFIX_LEN)
                .expect("checkpoint record prefix length fits usize")
                .checked_add(payload.len())
                .and_then(|len| len.checked_add(8))
                .ok_or_else(|| {
                    StateError::JournalReplayFailed(
                        "checkpoint record frame length overflows usize".to_string(),
                    )
                })?,
        );
        frame.extend_from_slice(&CHECKPOINT_RECORD_VERSION.to_le_bytes());
        frame.extend_from_slice(&payload_len.to_le_bytes());
        frame.extend_from_slice(&payload);
        let crc = remanence_parity::crc64_xz(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());

        let append_start = self
            .file
            .seek(SeekFrom::End(0))
            .map_err(|err| StateError::io_at("seek checkpoint journal", &self.path, err))?;
        if let Err(err) = self
            .file
            .write_all(&frame)
            .and_then(|_| self.file.sync_all())
        {
            let rollback = self
                .file
                .set_len(append_start)
                .and_then(|_| self.file.sync_all());
            if let Err(rollback_err) = rollback {
                return Err(StateError::JournalReplayFailed(format!(
                    "checkpoint append failed ({err}); rollback to offset {append_start} failed ({rollback_err})"
                )));
            }
            return Err(StateError::io_at(
                "append and fsync checkpoint journal frame",
                &self.path,
                err,
            ));
        }
        Ok(())
    }
}

fn checkpoint_companion_path(path: &Path, suffix: &str) -> PathBuf {
    let mut companion = path.as_os_str().to_os_string();
    companion.push(suffix);
    PathBuf::from(companion)
}

fn terminal_intent_path(path: &Path) -> PathBuf {
    checkpoint_companion_path(path, ".sealing")
}

fn write_terminal_intent(path: &Path, tape_uuid: [u8; 16]) -> Result<(), StateError> {
    let intent_path = terminal_intent_path(path);
    match fs::symlink_metadata(&intent_path) {
        Ok(_) => {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint journal {} already has a pending Sealing intent; physical-tail reconciliation is required",
                path.display()
            )));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(StateError::io_at(
                "inspect checkpoint Sealing intent",
                &intent_path,
                err,
            ));
        }
    }
    let temporary_path = checkpoint_companion_path(path, ".sealing.new");
    let mut intent = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary_path)
        .map_err(|err| {
            StateError::io_at(
                "create temporary checkpoint Sealing intent",
                &temporary_path,
                err,
            )
        })?;
    let mut payload = Vec::with_capacity(32);
    payload.extend_from_slice(CHECKPOINT_SEALING_INTENT_MAGIC);
    payload.extend_from_slice(&tape_uuid);
    let crc = remanence_parity::crc64_xz(&payload);
    payload.extend_from_slice(&crc.to_le_bytes());
    intent
        .write_all(&payload)
        .and_then(|_| intent.sync_all())
        .map_err(|err| {
            StateError::io_at(
                "write temporary checkpoint Sealing intent",
                &temporary_path,
                err,
            )
        })?;
    fs::rename(&temporary_path, &intent_path)
        .map_err(|err| StateError::io_at("publish checkpoint Sealing intent", &intent_path, err))?;
    let parent = intent_path.parent().ok_or_else(|| {
        StateError::JournalReplayFailed(
            "checkpoint Sealing intent path has no parent directory".to_string(),
        )
    })?;
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|err| StateError::io_at("fsync checkpoint Sealing intent directory", parent, err))
}

fn terminal_intent_pending(path: &Path, tape_uuid: [u8; 16]) -> Result<bool, StateError> {
    let intent_path = terminal_intent_path(path);
    let mut intent = match File::open(&intent_path) {
        Ok(intent) => intent,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(StateError::io_at(
                "open checkpoint Sealing intent",
                &intent_path,
                err,
            ))
        }
    };
    let len = intent
        .metadata()
        .map_err(|err| StateError::io_at("stat checkpoint Sealing intent", &intent_path, err))?
        .len();
    if len != 32 {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint Sealing intent {} has invalid length {len}; physical-tail reconciliation is required",
            intent_path.display()
        )));
    }
    let mut payload = [0u8; 24];
    let mut crc = [0u8; 8];
    intent
        .read_exact(&mut payload)
        .and_then(|_| intent.read_exact(&mut crc))
        .map_err(|err| StateError::io_at("read checkpoint Sealing intent", &intent_path, err))?;
    if &payload[..8] != CHECKPOINT_SEALING_INTENT_MAGIC
        || payload[8..24] != tape_uuid
        || remanence_parity::crc64_xz(&payload) != u64::from_le_bytes(crc)
    {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint Sealing intent {} failed identity or integrity validation; physical-tail reconciliation is required",
            intent_path.display()
        )));
    }
    Ok(true)
}

fn clear_terminal_intent(path: &Path) -> Result<(), StateError> {
    let intent_path = terminal_intent_path(path);
    fs::remove_file(&intent_path)
        .map_err(|err| StateError::io_at("clear checkpoint Sealing intent", &intent_path, err))?;
    let parent = intent_path.parent().ok_or_else(|| {
        StateError::JournalReplayFailed(
            "checkpoint Sealing intent path has no parent directory".to_string(),
        )
    })?;
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|err| StateError::io_at("fsync cleared Sealing intent directory", parent, err))
}

fn enforce_terminal_intent_for_replay(
    path: &Path,
    tape_uuid: [u8; 16],
    records: &[CheckpointJournalRecord],
    clear_completed: bool,
) -> Result<(), StateError> {
    if !terminal_intent_pending(path, tape_uuid)? {
        return Ok(());
    }
    if records
        .last()
        .is_some_and(|record| record.sealed_after_write)
    {
        if clear_completed {
            clear_terminal_intent(path)?;
        }
        return Ok(());
    }
    Err(StateError::JournalReplayFailed(format!(
        "checkpoint journal {} has a pending Sealing intent without terminal authority; physical-tail reconciliation is required before append",
        path.display()
    )))
}

fn acquire_checkpoint_lock(
    path: &Path,
    operation: FlockArg,
    action: &str,
) -> Result<Flock<File>, StateError> {
    let lock_path = checkpoint_companion_path(path, ".lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|err| StateError::io_at("open checkpoint journal lock", &lock_path, err))?;
    Flock::lock(lock_file, operation).map_err(|(_file, errno)| {
        StateError::io_at(action, &lock_path, std::io::Error::from(errno))
    })
}

fn initialize_checkpoint_file(path: &Path, tape_uuid: [u8; 16]) -> Result<File, StateError> {
    let init_path = checkpoint_companion_path(path, ".init");
    let mut init = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&init_path)
        .map_err(|err| {
            StateError::io_at(
                "create checkpoint journal initialization file",
                &init_path,
                err,
            )
        })?;
    write_checkpoint_header(&mut init, tape_uuid, &init_path)?;
    if path.exists() {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint journal {} appeared during locked initialization",
            path.display()
        )));
    }
    fs::rename(&init_path, path)
        .map_err(|err| StateError::io_at("publish checkpoint journal header", path, err))?;
    let parent = path.parent().ok_or_else(|| {
        StateError::JournalReplayFailed(
            "checkpoint journal path has no parent directory".to_string(),
        )
    })?;
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|err| StateError::io_at("fsync checkpoint journal directory", parent, err))?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|err| StateError::io_at("open initialized checkpoint journal", path, err))
}

fn replay_checkpoint_records(
    file: &mut File,
    tape_uuid: [u8; 16],
    path: &Path,
) -> Result<Vec<CheckpointJournalRecord>, StateError> {
    let scan = scan_checkpoint_records(file, tape_uuid, path)?;
    match scan.tail {
        CheckpointReplayTail::Clean => Ok(scan.records),
        CheckpointReplayTail::Torn => Err(torn_checkpoint_tail_error(path, scan.valid_end)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckpointReplayTail {
    Clean,
    Torn,
}

#[derive(Debug)]
struct CheckpointReplayScan {
    records: Vec<CheckpointJournalRecord>,
    valid_end: u64,
    tail: CheckpointReplayTail,
}

fn torn_checkpoint_tail_error(path: &Path, valid_end: u64) -> StateError {
    StateError::JournalReplayFailed(format!(
        "checkpoint journal {} has a torn trailing frame after offset {valid_end}; explicit recovery is required before append",
        path.display()
    ))
}

fn write_checkpoint_header(
    file: &mut File,
    tape_uuid: [u8; 16],
    path: &Path,
) -> Result<(), StateError> {
    let mut header = Vec::with_capacity(
        usize::try_from(CHECKPOINT_JOURNAL_HEADER_LEN)
            .expect("checkpoint header length fits usize"),
    );
    header.extend_from_slice(CHECKPOINT_JOURNAL_MAGIC);
    header.extend_from_slice(&tape_uuid);
    let crc = remanence_parity::crc64_xz(&header);
    header.extend_from_slice(&crc.to_le_bytes());
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&header))
        .and_then(|_| file.sync_all())
        .map_err(|err| StateError::io_at("write checkpoint journal header", path, err))
}

fn read_checkpoint_header(
    file: &mut File,
    tape_uuid: [u8; 16],
    path: &Path,
) -> Result<(), StateError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|err| StateError::io_at("seek checkpoint journal header", path, err))?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header).map_err(|err| {
        StateError::JournalReplayFailed(format!(
            "checkpoint journal {} has a missing or torn versioned header: {err}",
            path.display()
        ))
    })?;
    let mut crc = [0u8; 8];
    file.read_exact(&mut crc).map_err(|err| {
        StateError::JournalReplayFailed(format!(
            "checkpoint journal {} has a torn header checksum: {err}",
            path.display()
        ))
    })?;
    if &header[..8] != CHECKPOINT_JOURNAL_MAGIC {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint journal {} uses an unsupported legacy or future format",
            path.display()
        )));
    }
    if header[8..24] != tape_uuid {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint journal {} header tape_uuid mismatch",
            path.display()
        )));
    }
    if remanence_parity::crc64_xz(&header) != u64::from_le_bytes(crc) {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint journal {} header checksum mismatch",
            path.display()
        )));
    }
    Ok(())
}

fn scan_checkpoint_records(
    file: &mut File,
    tape_uuid: [u8; 16],
    path: &Path,
) -> Result<CheckpointReplayScan, StateError> {
    read_checkpoint_header(file, tape_uuid, path)?;
    let file_len = file
        .metadata()
        .map_err(|err| StateError::io_at("stat checkpoint journal", path, err))?
        .len();
    let mut records = Vec::new();
    let mut valid_end = CHECKPOINT_JOURNAL_HEADER_LEN;
    loop {
        let record_start = file
            .stream_position()
            .map_err(|err| StateError::io_at("position checkpoint journal", path, err))?;
        if record_start == file_len {
            return Ok(CheckpointReplayScan {
                records,
                valid_end,
                tail: CheckpointReplayTail::Clean,
            });
        }
        let available = file_len.saturating_sub(record_start);
        if available < CHECKPOINT_RECORD_PREFIX_LEN {
            return Ok(CheckpointReplayScan {
                records,
                valid_end,
                tail: CheckpointReplayTail::Torn,
            });
        }
        let mut prefix = [0u8; 6];
        file.read_exact(&mut prefix)
            .map_err(|err| StateError::io_at("read checkpoint record prefix", path, err))?;
        let version = u16::from_le_bytes(prefix[..2].try_into().expect("slice length"));
        if version != CHECKPOINT_RECORD_VERSION {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint record at offset {record_start} in {} has unsupported version {version}",
                path.display()
            )));
        }
        let payload_len = u64::from(u32::from_le_bytes(
            prefix[2..6].try_into().expect("slice length"),
        ));
        if payload_len > MAX_CHECKPOINT_RECORD_LEN {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint record at offset {record_start} in {} declares {payload_len} bytes, limit {MAX_CHECKPOINT_RECORD_LEN}",
                path.display()
            )));
        }
        let frame_tail_len = payload_len.checked_add(8).ok_or_else(|| {
            StateError::JournalReplayFailed("checkpoint record length overflows u64".to_string())
        })?;
        if available.saturating_sub(CHECKPOINT_RECORD_PREFIX_LEN) < frame_tail_len {
            return Ok(CheckpointReplayScan {
                records,
                valid_end,
                tail: CheckpointReplayTail::Torn,
            });
        }
        let payload_len = usize::try_from(payload_len).map_err(|_| {
            StateError::JournalReplayFailed(
                "checkpoint record length does not fit usize".to_string(),
            )
        })?;
        let mut payload = vec![0u8; payload_len];
        file.read_exact(&mut payload)
            .map_err(|err| StateError::io_at("read checkpoint record payload", path, err))?;
        let mut crc = [0u8; 8];
        file.read_exact(&mut crc)
            .map_err(|err| StateError::io_at("read checkpoint record checksum", path, err))?;
        let mut crc_input = Vec::with_capacity(prefix.len() + payload.len());
        crc_input.extend_from_slice(&prefix);
        crc_input.extend_from_slice(&payload);
        if remanence_parity::crc64_xz(&crc_input) != u64::from_le_bytes(crc) {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint record at offset {record_start} in {} has a checksum mismatch",
                path.display()
            )));
        }
        let frame: CheckpointJournalFrame = serde_json::from_slice(&payload).map_err(|err| {
            StateError::JournalReplayFailed(format!(
                "decode checkpoint journal frame at offset {record_start} in {}: {err}",
                path.display()
            ))
        })?;
        if frame.records.is_empty() {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint journal frame at offset {record_start} in {} is empty",
                path.display()
            )));
        }
        for record in frame.records {
            if record.tape_uuid != tape_uuid {
                return Err(StateError::JournalReplayFailed(format!(
                    "checkpoint record at offset {record_start} tape_uuid mismatch in {}",
                    path.display()
                )));
            }
            validate_next_record(records.last(), &record)?;
            records.push(record);
        }
        valid_end = file
            .stream_position()
            .map_err(|err| StateError::io_at("position checkpoint journal", path, err))?;
    }
}

/// Enumerate all per-tape checkpoint journal paths in a configured directory.
pub fn list_checkpoint_journals(dir: impl AsRef<Path>) -> Result<Vec<PathBuf>, StateError> {
    let dir = dir.as_ref();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in
        fs::read_dir(dir).map_err(|err| StateError::io_at("list checkpoint journals", dir, err))?
    {
        let path = entry
            .map_err(|err| StateError::io_at("read checkpoint journal directory entry", dir, err))?
            .path();
        if path.extension().is_some_and(|ext| ext == "remcheckpoint") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Decode the tape UUID embedded in a checkpoint journal filename.
pub fn tape_uuid_from_checkpoint_path(path: &Path) -> Result<[u8; 16], StateError> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            StateError::JournalReplayFailed(format!(
                "checkpoint journal path has no UTF-8 filename: {}",
                path.display()
            ))
        })?;
    let uuid = filename
        .strip_suffix(CHECKPOINT_JOURNAL_SUFFIX)
        .ok_or_else(|| {
            StateError::JournalReplayFailed(format!(
                "checkpoint journal filename has wrong suffix: {filename}"
            ))
        })?;
    uuid::Uuid::parse_str(uuid)
        .map(|uuid| *uuid.as_bytes())
        .map_err(|err| {
            StateError::JournalReplayFailed(format!(
                "checkpoint journal filename has invalid tape UUID {uuid:?}: {err}"
            ))
        })
}

fn checkpoint_journal_path(dir: &Path, tape_uuid: [u8; 16]) -> PathBuf {
    dir.join(format!(
        "{}{}",
        uuid::Uuid::from_bytes(tape_uuid),
        CHECKPOINT_JOURNAL_SUFFIX
    ))
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct CheckpointBundleShapeError(String);

#[derive(Debug)]
pub(crate) struct ValidatedParityCheckpointLayout<'a> {
    pub(crate) first_tape_file: &'a remanence_parity::TapeFileEntry,
    pub(crate) checkpoint_first_tape_file: &'a remanence_parity::TapeFileEntry,
    pub(crate) checkpoint_bootstrap: &'a remanence_parity::TapeFileEntry,
    pub(crate) starting_total_committed_ordinals: u64,
}

fn checkpoint_bundle_shape_error(detail: impl Into<String>) -> CheckpointBundleShapeError {
    CheckpointBundleShapeError(detail.into())
}

/// Validate the complete current-wire parity layout carried by one checkpoint
/// record. Journal append/replay and SQLite projection both call this function
/// so they cannot accept different control-bundle shapes.
pub(crate) fn validate_parity_checkpoint_bundles(
    record: &CheckpointJournalRecord,
) -> Result<ValidatedParityCheckpointLayout<'_>, CheckpointBundleShapeError> {
    if record.scheme.is_none() {
        return Err(checkpoint_bundle_shape_error(
            "parity bundle validation requires a parity scheme",
        ));
    }
    if record.object_tape_file_bundles.len() != record.objects.len() {
        return Err(checkpoint_bundle_shape_error(format!(
            "parity checkpoint has {} object bundles for {} object projections",
            record.object_tape_file_bundles.len(),
            record.objects.len()
        )));
    }
    let checkpoint_bundle = record.checkpoint_bundle.as_ref().ok_or_else(|| {
        checkpoint_bundle_shape_error("parity checkpoint has no barrier Control bundle")
    })?;

    let mut first_tape_file = None;
    let mut prior_last_tape_file = None;
    let mut highest_protected_ordinal = None;
    let mut total_committed_ordinals = None;
    let mut starting_total_committed_ordinals = None;

    for (projection, bundle) in record.objects.iter().zip(&record.object_tape_file_bundles) {
        remanence_parity::validate_committed_bundle_shape(bundle).map_err(|err| {
            checkpoint_bundle_shape_error(format!("object {}: {err}", projection.object.object_id))
        })?;
        if bundle.kind != remanence_parity::CommittedBundleKind::Object {
            return Err(checkpoint_bundle_shape_error(format!(
                "object {} uses {:?} bundle kind",
                projection.object.object_id, bundle.kind
            )));
        }
        let object_entry = bundle.entries.first().ok_or_else(|| {
            checkpoint_bundle_shape_error(format!(
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
            return Err(checkpoint_bundle_shape_error(format!(
                "object {} bundle entry does not match projection geometry",
                projection.object.object_id
            )));
        }
        if object_entry.block_count == 0 {
            return Err(checkpoint_bundle_shape_error(format!(
                "object {} has zero stored blocks",
                projection.object.object_id
            )));
        }
        let object_first_ordinal = object_entry.first_parity_data_ordinal.ok_or_else(|| {
            checkpoint_bundle_shape_error(format!(
                "object {} has no first parity data ordinal",
                projection.object.object_id
            ))
        })?;
        let running_total = match total_committed_ordinals {
            Some(total) => total,
            None => {
                starting_total_committed_ordinals = Some(object_first_ordinal);
                highest_protected_ordinal = Some(object_first_ordinal);
                object_first_ordinal
            }
        };
        if object_first_ordinal != running_total {
            return Err(checkpoint_bundle_shape_error(format!(
                "object {} starts at parity ordinal {}, expected {}",
                projection.object.object_id, object_first_ordinal, running_total
            )));
        }
        let next_total = running_total
            .checked_add(object_entry.block_count)
            .ok_or_else(|| {
                checkpoint_bundle_shape_error("checkpoint object ordinals overflow u64")
            })?;
        if bundle.total_committed_ordinals != next_total
            || projection.total_committed_ordinals != next_total
        {
            return Err(checkpoint_bundle_shape_error(format!(
                "object {} ends at ordinal {next_total}, but bundle/projection report {}/{}",
                projection.object.object_id,
                bundle.total_committed_ordinals,
                projection.total_committed_ordinals
            )));
        }
        let next_highest = validate_sidecar_watermark_transition(
            highest_protected_ordinal.expect("set with first object total"),
            next_total,
            bundle,
        )?;
        if bundle.highest_protected_ordinal != next_highest {
            return Err(checkpoint_bundle_shape_error(format!(
                "object {} bundle reports W={}, expected {next_highest} from its sidecars",
                projection.object.object_id, bundle.highest_protected_ordinal
            )));
        }
        highest_protected_ordinal = Some(next_highest);
        total_committed_ordinals = Some(next_total);
        prior_last_tape_file = bundle.entries.last();
    }

    let first_tape_file = first_tape_file.ok_or_else(|| {
        checkpoint_bundle_shape_error("parity checkpoint must commit at least one object")
    })?;
    let checkpoint_bootstrap = remanence_parity::validate_committed_bundle_shape(checkpoint_bundle)
        .map_err(|err| checkpoint_bundle_shape_error(format!("checkpoint barrier: {err}")))?
        .ok_or_else(|| checkpoint_bundle_shape_error("checkpoint barrier has no Bootstrap"))?;
    if checkpoint_bundle.kind != remanence_parity::CommittedBundleKind::Control {
        return Err(checkpoint_bundle_shape_error(format!(
            "checkpoint barrier uses {:?} bundle kind",
            checkpoint_bundle.kind
        )));
    }
    let checkpoint_first_tape_file = checkpoint_bundle.entries.first().ok_or_else(|| {
        checkpoint_bundle_shape_error("checkpoint barrier Control bundle is empty")
    })?;
    validate_next_bundle_file(
        prior_last_tape_file,
        checkpoint_first_tape_file,
        "parity checkpoint barrier bundle",
    )?;

    let total_committed_ordinals =
        total_committed_ordinals.expect("a validated parity checkpoint has at least one object");
    if checkpoint_bundle.total_committed_ordinals != total_committed_ordinals {
        return Err(checkpoint_bundle_shape_error(format!(
            "checkpoint barrier reports T={}, expected {total_committed_ordinals}",
            checkpoint_bundle.total_committed_ordinals
        )));
    }
    let final_highest = validate_sidecar_watermark_transition(
        highest_protected_ordinal.expect("a validated parity checkpoint has W"),
        total_committed_ordinals,
        checkpoint_bundle,
    )?;
    if checkpoint_bundle.highest_protected_ordinal != final_highest {
        return Err(checkpoint_bundle_shape_error(format!(
            "checkpoint barrier reports W={}, expected {final_highest} from its sidecars",
            checkpoint_bundle.highest_protected_ordinal
        )));
    }
    if final_highest != total_committed_ordinals {
        return Err(checkpoint_bundle_shape_error(format!(
            "checkpoint barrier left ordinals unprotected: W={final_highest}, T={total_committed_ordinals}"
        )));
    }
    if checkpoint_bootstrap.tape_file_number != record.checkpoint_tape_file_number {
        return Err(checkpoint_bundle_shape_error(format!(
            "checkpoint Bootstrap is tape file {}, record names {}",
            checkpoint_bootstrap.tape_file_number, record.checkpoint_tape_file_number
        )));
    }

    Ok(ValidatedParityCheckpointLayout {
        first_tape_file,
        checkpoint_first_tape_file,
        checkpoint_bootstrap,
        starting_total_committed_ordinals: starting_total_committed_ordinals
            .expect("a validated parity checkpoint has a starting ordinal"),
    })
}

/// Require the external checkpoint record and Layer 3c sink journal to name
/// the same durable resume boundary before append positioning or media
/// modification occurs.
///
/// The two journals cannot be appended atomically. A crash may therefore
/// leave either authority one fsync ahead of the other. Resuming from the EOD
/// in one while seeding logical ordinals from the other could overwrite a
/// newer tape prefix, so disagreement is a fail-closed recovery condition.
pub fn validate_parity_resume_authority(
    records: &[CheckpointJournalRecord],
    committed: &remanence_parity::CommittedState,
    tape_uuid: [u8; 16],
    block_size: u32,
    scheme: &remanence_parity::ParityScheme,
) -> Result<(), StateError> {
    let mismatch = |detail: String| {
        StateError::JournalReplayFailed(format!("parity resume authority mismatch: {detail}"))
    };
    if !committed.orphaned_bundles.is_empty() {
        return Err(mismatch(format!(
            "sink journal exposes {} preserved bundle(s) beyond its last checkpoint marker; physical-tail reconciliation is required before append",
            committed.orphaned_bundles.len()
        )));
    }
    let record = match (records.last(), committed.entries.is_empty()) {
        (None, true) => return Ok(()),
        (None, false) => {
            return Err(mismatch(
                "sink journal has a committed prefix but checkpoint journal is empty".to_string(),
            ));
        }
        (Some(_), true) => {
            return Err(mismatch(
                "checkpoint journal is nonempty but sink journal has no committed prefix"
                    .to_string(),
            ));
        }
        (Some(record), false) => record,
    };
    let layout = validate_parity_checkpoint_bundles(record)
        .map_err(|err| mismatch(format!("checkpoint record is invalid: {err}")))?;
    if record.tape_uuid != tape_uuid {
        return Err(mismatch(format!(
            "checkpoint tape {} does not match selected tape {}",
            uuid::Uuid::from_bytes(record.tape_uuid),
            uuid::Uuid::from_bytes(tape_uuid)
        )));
    }
    if record.block_size != block_size {
        return Err(mismatch(format!(
            "checkpoint block size {} does not match sink journal block size {block_size}",
            record.block_size
        )));
    }
    if record.scheme.as_ref() != Some(scheme) {
        return Err(mismatch(
            "checkpoint parity scheme does not match the sink journal".to_string(),
        ));
    }
    let checkpoint_bundle = record
        .checkpoint_bundle
        .as_ref()
        .expect("validated parity record has a checkpoint bundle");
    if committed.highest_protected_ordinal != checkpoint_bundle.highest_protected_ordinal
        || committed.total_committed_ordinals != checkpoint_bundle.total_committed_ordinals
    {
        return Err(mismatch(format!(
            "checkpoint W/T ({}/{}) does not match sink journal W/T ({}/{})",
            checkpoint_bundle.highest_protected_ordinal,
            checkpoint_bundle.total_committed_ordinals,
            committed.highest_protected_ordinal,
            committed.total_committed_ordinals
        )));
    }

    let committed_object_count = committed
        .entries
        .iter()
        .filter(|entry| entry.kind == remanence_parity::TapeFileKind::Object)
        .count();
    let committed_object_count = u64::try_from(committed_object_count)
        .map_err(|_| mismatch("sink journal object count overflows u64".to_string()))?;
    if committed_object_count != record.committed_object_count {
        return Err(mismatch(format!(
            "checkpoint names {} committed objects but sink journal contains {committed_object_count}",
            record.committed_object_count
        )));
    }

    let bot_bootstrap = &committed.entries[0];
    if bot_bootstrap.tape_file_number != 0
        || bot_bootstrap.kind != remanence_parity::TapeFileKind::Bootstrap
        || bot_bootstrap.block_count != 1
    {
        return Err(mismatch(format!(
            "sink journal does not start with the one-block BOT Bootstrap: {bot_bootstrap:?}"
        )));
    }

    let expected_prefix = records
        .iter()
        .flat_map(|record| {
            record
                .object_tape_file_bundles
                .iter()
                .flat_map(|bundle| bundle.entries.iter())
                .chain(
                    record
                        .checkpoint_bundle
                        .iter()
                        .flat_map(|bundle| bundle.entries.iter()),
                )
        })
        .collect::<Vec<_>>();
    let expected_sink_entries = expected_prefix
        .len()
        .checked_add(1)
        .ok_or_else(|| mismatch("checkpoint prefix entry count overflows usize".to_string()))?;
    if committed.entries.len() != expected_sink_entries {
        return Err(mismatch(format!(
            "checkpoint history names {expected_sink_entries} tape-file entries including BOT, but sink journal contains {}",
            committed.entries.len()
        )));
    }
    for (offset, (actual, expected)) in committed.entries[1..]
        .iter()
        .zip(expected_prefix)
        .enumerate()
    {
        if !parity_resume_entries_match(actual, expected) {
            return Err(mismatch(format!(
                "checkpoint and sink prefixes differ at entry {}: checkpoint={expected:?}, sink={actual:?}",
                offset + 1
            )));
        }
    }

    let expected_eod_lba = layout
        .checkpoint_bootstrap
        .physical_start_hint
        .ok_or_else(|| mismatch("checkpoint Bootstrap has no physical start hint".to_string()))?
        .checked_add(layout.checkpoint_bootstrap.block_count)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| mismatch("checkpoint Bootstrap EOD calculation overflows".to_string()))?;
    if record.eod_partition != 0 || record.eod_lba != expected_eod_lba {
        return Err(mismatch(format!(
            "checkpoint barrier position is partition {} lba {}, expected partition 0 lba {expected_eod_lba} from terminal Bootstrap",
            record.eod_partition, record.eod_lba
        )));
    }
    Ok(())
}

fn parity_resume_entries_match(
    sink: &remanence_parity::TapeFileEntry,
    checkpoint: &remanence_parity::TapeFileEntry,
) -> bool {
    if sink.kind != remanence_parity::TapeFileKind::Object
        || checkpoint.kind != remanence_parity::TapeFileKind::Object
    {
        return sink == checkpoint;
    }

    let Some(checkpoint_object_id) = checkpoint.object_id.as_deref() else {
        return false;
    };
    if sink
        .object_id
        .as_deref()
        .is_some_and(|sink_object_id| sink_object_id != checkpoint_object_id)
    {
        return false;
    }
    if sink
        .bootstrap_object_row
        .as_ref()
        .and_then(|row| row.object_id.as_deref())
        != Some(checkpoint_object_id.as_bytes())
    {
        return false;
    }

    let mut sink = sink.clone();
    let mut checkpoint = checkpoint.clone();
    sink.object_id = None;
    checkpoint.object_id = None;
    sink == checkpoint
}

fn validate_next_bundle_file(
    prior_last: Option<&remanence_parity::TapeFileEntry>,
    next_first: &remanence_parity::TapeFileEntry,
    context: &str,
) -> Result<(), CheckpointBundleShapeError> {
    let Some(prior_last) = prior_last else {
        return Ok(());
    };
    let expected = prior_last.tape_file_number.checked_add(1).ok_or_else(|| {
        checkpoint_bundle_shape_error("checkpoint tape-file number overflows u32")
    })?;
    if next_first.tape_file_number != expected {
        return Err(checkpoint_bundle_shape_error(format!(
            "{context} starts at tape file {}, expected {expected}",
            next_first.tape_file_number
        )));
    }
    Ok(())
}

pub(crate) fn validate_sidecar_watermark_transition(
    mut highest_protected_ordinal: u64,
    total_committed_ordinals: u64,
    bundle: &remanence_parity::CommittedBundle,
) -> Result<u64, CheckpointBundleShapeError> {
    for sidecar in bundle
        .entries
        .iter()
        .filter(|entry| entry.kind == remanence_parity::TapeFileKind::ParitySidecar)
    {
        let start = sidecar.protected_ordinal_start.ok_or_else(|| {
            checkpoint_bundle_shape_error(format!(
                "ParitySidecar at tape file {} has no protected range start",
                sidecar.tape_file_number
            ))
        })?;
        let end = sidecar.protected_ordinal_end_exclusive.ok_or_else(|| {
            checkpoint_bundle_shape_error(format!(
                "ParitySidecar at tape file {} has no protected range end",
                sidecar.tape_file_number
            ))
        })?;
        if start != highest_protected_ordinal || end <= start || end > total_committed_ordinals {
            return Err(checkpoint_bundle_shape_error(format!(
                "ParitySidecar at tape file {} protects [{start}, {end}), expected a non-empty range starting at {highest_protected_ordinal} and ending no later than {total_committed_ordinals}",
                sidecar.tape_file_number
            )));
        }
        highest_protected_ordinal = end;
    }
    Ok(highest_protected_ordinal)
}

fn validate_next_record(
    previous: Option<&CheckpointJournalRecord>,
    record: &CheckpointJournalRecord,
) -> Result<(), StateError> {
    if previous.is_some_and(|prior| prior.sealed_after_write) {
        return Err(StateError::JournalReplayFailed(
            "checkpoint record follows a terminal sealed checkpoint".to_string(),
        ));
    }
    let parity_record = record.scheme.is_some();
    if let Some(scheme) = &record.scheme {
        scheme.validate().map_err(|err| {
            StateError::JournalReplayFailed(format!(
                "parity checkpoint record carries an invalid scheme: {err}"
            ))
        })?;
    }
    if previous.is_some_and(|prior| prior.scheme != record.scheme) {
        return Err(StateError::JournalReplayFailed(
            "checkpoint parity scheme changed within one tape journal".to_string(),
        ));
    }
    let expected_ordinal = match previous {
        Some(prior) => prior.ordinal.checked_add(1).ok_or_else(|| {
            StateError::JournalReplayFailed("checkpoint ordinal overflows u64".to_string())
        })?,
        None => 1,
    };
    if record.ordinal != expected_ordinal {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint ordinal {} is not expected next ordinal {expected_ordinal}",
            record.ordinal
        )));
    }
    let prior_count = previous.map_or(0, |prior| prior.committed_object_count);
    let appended = u64::try_from(record.objects.len()).map_err(|_| {
        StateError::JournalReplayFailed("checkpoint object count exceeds u64".to_string())
    })?;
    let expected_count = prior_count.checked_add(appended).ok_or_else(|| {
        StateError::JournalReplayFailed(
            "checkpoint committed object count overflows u64".to_string(),
        )
    })?;
    if record.committed_object_count != expected_count {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint committed count {} does not extend prior count {prior_count} by {appended}",
            record.committed_object_count
        )));
    }
    if record.eod_partition != 0 {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint EOD partition {} is unsupported",
            record.eod_partition
        )));
    }
    if record.block_size == 0 {
        return Err(StateError::JournalReplayFailed(
            "checkpoint block size must be non-zero".to_string(),
        ));
    }
    if record.sealed_after_write {
        return validate_terminal_checkpoint_record(previous, record);
    }
    if record.objects.is_empty() {
        return Err(StateError::JournalReplayFailed(
            "non-terminal checkpoint record must commit at least one object".to_string(),
        ));
    }
    if !parity_record
        && (!record.object_tape_file_bundles.is_empty() || record.checkpoint_bundle.is_some())
    {
        return Err(StateError::JournalReplayFailed(
            "parity-off checkpoint record carries parity bundle fields".to_string(),
        ));
    }
    let expected_first_file = match previous {
        Some(prior) => prior
            .checkpoint_tape_file_number
            .checked_add(1)
            .ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "checkpoint tape-file number overflows u32".to_string(),
                )
            })?,
        None => 1,
    };
    let parity_layout = if parity_record {
        let layout = validate_parity_checkpoint_bundles(record)
            .map_err(|err| StateError::JournalReplayFailed(err.to_string()))?;
        if layout.first_tape_file.tape_file_number != expected_first_file {
            return Err(StateError::JournalReplayFailed(format!(
                "parity checkpoint starts at tape file {}, expected {expected_first_file}",
                layout.first_tape_file.tape_file_number
            )));
        }
        let expected_starting_total = previous
            .and_then(|prior| prior.objects.last())
            .map_or(0, |projection| projection.total_committed_ordinals);
        if layout.starting_total_committed_ordinals != expected_starting_total {
            return Err(StateError::JournalReplayFailed(format!(
                "parity checkpoint starts at ordinal {}, expected {expected_starting_total}",
                layout.starting_total_committed_ordinals
            )));
        }
        Some(layout)
    } else {
        None
    };
    let mut expected_file = expected_first_file;
    for (index, projection) in record.objects.iter().enumerate() {
        if projection.block_size != record.block_size {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint object {} block size {} differs from record block size {}",
                projection.object.object_id, projection.block_size, record.block_size
            )));
        }
        if projection.copy.tape_uuid != record.tape_uuid {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint object {} copy is on a different tape",
                projection.object.object_id
            )));
        }
        let object_file = if parity_record {
            record.object_tape_file_bundles[index].entries[0].tape_file_number
        } else {
            let object_file = expected_file;
            expected_file = expected_file.checked_add(1).ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "checkpoint object tape-file number overflows u32".to_string(),
                )
            })?;
            object_file
        };
        if projection.copy.tape_file_number != object_file {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint object {} uses tape file {}, expected {object_file}",
                projection.object.object_id, projection.copy.tape_file_number,
            )));
        }
        let row = &projection.bootstrap_object_row;
        if row.tape_file_number != projection.copy.tape_file_number
            || row.stored_block_count != projection.block_count
        {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint object {} bootstrap row does not match its copy geometry",
                projection.object.object_id
            )));
        }
        if row.object_id != projection.object.object_id.as_bytes() {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint object {} bootstrap row has a different object_id",
                projection.object.object_id
            )));
        }
    }
    let expected_checkpoint_file = if let Some(layout) = &parity_layout {
        layout.checkpoint_bootstrap.tape_file_number
    } else {
        expected_file
    };
    if record.checkpoint_tape_file_number != expected_checkpoint_file {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint bootstrap uses tape file {}, expected {expected_checkpoint_file}",
            record.checkpoint_tape_file_number
        )));
    }
    if previous.is_some_and(|prior| prior.block_size != record.block_size) {
        return Err(StateError::JournalReplayFailed(
            "checkpoint block size changed within one tape journal".to_string(),
        ));
    }
    if parity_record {
        if previous.is_some_and(|prior| record.eod_lba <= prior.eod_lba) || record.eod_lba == 0 {
            return Err(StateError::JournalReplayFailed(
                "parity checkpoint EOD must advance monotonically".to_string(),
            ));
        }
        return Ok(());
    }
    let prefix_lba = previous.map_or(2, |prior| prior.eod_lba);
    let expected_eod = record
        .objects
        .iter()
        .try_fold(prefix_lba, |lba, projection| {
            lba.checked_add(projection.block_count)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| {
                    StateError::JournalReplayFailed("checkpoint EOD LBA overflows u64".to_string())
                })
        })?;
    let expected_eod = expected_eod.checked_add(2).ok_or_else(|| {
        StateError::JournalReplayFailed("checkpoint bootstrap EOD LBA overflows u64".to_string())
    })?;
    if record.eod_lba != expected_eod {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint EOD LBA {} does not match structural prefix {expected_eod}",
            record.eod_lba
        )));
    }
    Ok(())
}

fn validate_terminal_checkpoint_record(
    previous: Option<&CheckpointJournalRecord>,
    record: &CheckpointJournalRecord,
) -> Result<(), StateError> {
    if !record.objects.is_empty() || !record.object_tape_file_bundles.is_empty() {
        return Err(StateError::JournalReplayFailed(
            "terminal checkpoint record must not commit objects".to_string(),
        ));
    }
    let bundle = record.checkpoint_bundle.as_ref().ok_or_else(|| {
        StateError::JournalReplayFailed(
            "terminal checkpoint record has no Finish bundle".to_string(),
        )
    })?;
    let bootstrap = remanence_parity::validate_committed_bundle_shape(bundle)
        .map_err(|err| {
            StateError::JournalReplayFailed(format!(
                "terminal checkpoint has invalid Finish bundle: {err}"
            ))
        })?
        .ok_or_else(|| {
            StateError::JournalReplayFailed(
                "terminal checkpoint Finish bundle has no Bootstrap".to_string(),
            )
        })?;
    if bundle.kind != remanence_parity::CommittedBundleKind::Finish {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal checkpoint uses {:?} bundle kind instead of Finish",
            bundle.kind
        )));
    }
    let first = bundle.entries.first().ok_or_else(|| {
        StateError::JournalReplayFailed("terminal checkpoint Finish bundle is empty".to_string())
    })?;
    let expected_first_file = match previous {
        Some(prior) => prior
            .checkpoint_tape_file_number
            .checked_add(1)
            .ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "terminal checkpoint tape-file number overflows u32".to_string(),
                )
            })?,
        None => 0,
    };
    if first.tape_file_number != expected_first_file {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal checkpoint starts at tape file {}, expected {expected_first_file}",
            first.tape_file_number
        )));
    }
    if bootstrap.tape_file_number != record.checkpoint_tape_file_number {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal checkpoint Bootstrap is tape file {}, record names {}",
            bootstrap.tape_file_number, record.checkpoint_tape_file_number
        )));
    }
    let (prior_highest, prior_total) = previous.map_or((0, 0), |prior| {
        prior.checkpoint_bundle.as_ref().map_or_else(
            || {
                prior.objects.last().map_or((0, 0), |projection| {
                    (0, projection.total_committed_ordinals)
                })
            },
            |bundle| {
                (
                    bundle.highest_protected_ordinal,
                    bundle.total_committed_ordinals,
                )
            },
        )
    });
    if bundle.highest_protected_ordinal != prior_highest
        || bundle.total_committed_ordinals != prior_total
    {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal checkpoint W/T ({}/{}) does not preserve prior authority ({prior_highest}/{prior_total})",
            bundle.highest_protected_ordinal, bundle.total_committed_ordinals
        )));
    }
    if previous.is_some_and(|prior| prior.block_size != record.block_size) {
        return Err(StateError::JournalReplayFailed(
            "terminal checkpoint block size changed within one tape journal".to_string(),
        ));
    }
    let prior_eod = previous.map_or(0, |prior| prior.eod_lba);
    if record.eod_lba <= prior_eod {
        return Err(StateError::JournalReplayFailed(
            "terminal checkpoint EOD must advance beyond prior authority".to_string(),
        ));
    }
    let mut expected_start = prior_eod;
    for entry in &bundle.entries {
        let physical_start = entry.physical_start_hint.ok_or_else(|| {
            StateError::JournalReplayFailed(format!(
                "terminal checkpoint tape file {} has no physical start hint",
                entry.tape_file_number
            ))
        })?;
        if physical_start != expected_start {
            return Err(StateError::JournalReplayFailed(format!(
                "terminal checkpoint tape file {} starts at {physical_start}, expected prior terminal cursor {expected_start}",
                entry.tape_file_number
            )));
        }
        expected_start = expected_start
            .checked_add(entry.block_count)
            .and_then(|lba| lba.checked_add(1))
            .ok_or_else(|| {
                StateError::JournalReplayFailed(format!(
                    "terminal checkpoint tape file {} extent overflows u64",
                    entry.tape_file_number
                ))
            })?;
    }
    if record.eod_lba != expected_start {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal checkpoint EOD LBA {} does not match Finish bundle end {expected_start}",
            record.eod_lba
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tape_uuid: [u8; 16]) -> CheckpointJournalRecord {
        let object_uuid = uuid::Uuid::from_bytes([0x51; 16]);
        CheckpointJournalRecord {
            ordinal: 1,
            committed_object_count: 1,
            eod_partition: 0,
            eod_lba: 8,
            tape_uuid,
            batch_id: [0x42; 16],
            checkpoint_tape_file_number: 2,
            block_size: 256 * 1024,
            objects: vec![CheckpointObjectProjection {
                object: NativeObjectProjectionInput {
                    object_id: object_uuid.to_string(),
                    caller_object_id: Some("checkpoint-test".to_string()),
                    body_format: "rem-object-v1".to_string(),
                    logical_size_bytes: Some(1),
                    content_hash: Some(vec![0x11; 32]),
                    metadata_hash: Some(vec![0x22; 32]),
                    created_at_utc: Some("2026-07-21T00:00:00Z".to_string()),
                },
                files: Vec::new(),
                copy: NativeObjectCopyProjectionInput {
                    object_id: object_uuid.to_string(),
                    tape_uuid,
                    tape_file_number: 1,
                    first_body_lba: 0,
                    first_parity_data_ordinal: None,
                    protected_until_ordinal: None,
                    status: "committed".to_string(),
                    representation: "plaintext".to_string(),
                    recipient_epoch_ids: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0x33; 32]),
                    stored_digest: Some(vec![0x33; 32]),
                },
                block_size: 256 * 1024,
                block_count: 3,
                fresh_tape: true,
                total_committed_ordinals: 3,
                bootstrap_object_row: CheckpointBootstrapObjectRow {
                    tape_file_number: 1,
                    stored_block_count: 3,
                    object_id: object_uuid.to_string().into_bytes(),
                    representation: CheckpointBootstrapObjectRepresentation::Plaintext {
                        manifest_first_chunk_lba: 1,
                        manifest_size_bytes: 1,
                        manifest_chunk_count: 1,
                        manifest_sha256: [0x44; 32],
                    },
                },
            }],
            scheme: None,
            object_tape_file_bundles: Vec::new(),
            checkpoint_bundle: None,
            sealed_after_write: false,
        }
    }

    fn second_record(tape_uuid: [u8; 16]) -> CheckpointJournalRecord {
        let mut record = record(tape_uuid);
        let object_uuid = uuid::Uuid::from_bytes([0x52; 16]);
        record.ordinal = 2;
        record.committed_object_count = 2;
        record.eod_lba = 14;
        record.batch_id = [0x43; 16];
        record.checkpoint_tape_file_number = 4;
        record.objects[0].object.object_id = object_uuid.to_string();
        record.objects[0].object.caller_object_id = Some("checkpoint-test-2".to_string());
        record.objects[0].copy.object_id = object_uuid.to_string();
        record.objects[0].copy.tape_file_number = 3;
        record.objects[0].fresh_tape = false;
        record.objects[0].total_committed_ordinals = 6;
        record.objects[0].bootstrap_object_row.tape_file_number = 3;
        record.objects[0].bootstrap_object_row.object_id = object_uuid.to_string().into_bytes();
        record
    }

    fn parity_entry(
        tape_file_number: u32,
        kind: remanence_parity::TapeFileKind,
    ) -> remanence_parity::TapeFileEntry {
        remanence_parity::TapeFileEntry {
            tape_file_number,
            kind,
            block_count: 1,
            physical_start_hint: None,
            object_id: None,
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            bootstrap_object_row: None,
        }
    }

    fn partial_epoch_parity_record(tape_uuid: [u8; 16]) -> CheckpointJournalRecord {
        let mut record = record(tape_uuid);
        record.eod_lba = 12;
        record.checkpoint_tape_file_number = 3;
        record.scheme = Some(remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_static("checkpoint-partial-epoch"),
            data_blocks_per_stripe: 8,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 1,
        });
        record.objects[0].fresh_tape = false;
        record.objects[0].copy.first_parity_data_ordinal = Some(0);
        record.objects[0].copy.protected_until_ordinal = Some(0);
        let mut object = parity_entry(1, remanence_parity::TapeFileKind::Object);
        object.block_count = 3;
        object.object_id = Some(record.objects[0].object.object_id.clone());
        object.first_parity_data_ordinal = Some(0);
        object.bootstrap_object_row = Some(record.objects[0].bootstrap_object_row.to_parity_row());
        record.object_tape_file_bundles = vec![remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::Object,
            entries: vec![object],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 3,
        }];
        let mut sidecar = parity_entry(2, remanence_parity::TapeFileKind::ParitySidecar);
        sidecar.epoch_id = Some(0);
        sidecar.protected_ordinal_start = Some(0);
        sidecar.protected_ordinal_end_exclusive = Some(3);
        sidecar.canonical_metadata_hash = Some([0x61; 32]);
        record.checkpoint_bundle = Some(remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::Control,
            entries: vec![
                sidecar,
                parity_entry(3, remanence_parity::TapeFileKind::Bootstrap),
            ],
            highest_protected_ordinal: 3,
            total_committed_ordinals: 3,
        });
        record
            .checkpoint_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .entries[1]
            .physical_start_hint = Some(10);
        record
    }

    fn second_partial_epoch_parity_record(
        prior: &CheckpointJournalRecord,
    ) -> CheckpointJournalRecord {
        let mut record = partial_epoch_parity_record(prior.tape_uuid);
        let object_uuid = uuid::Uuid::from_bytes([0x52; 16]);
        record.ordinal = 2;
        record.committed_object_count = 2;
        record.eod_lba = 22;
        record.batch_id = [0x43; 16];
        record.checkpoint_tape_file_number = 6;
        record.objects[0].object.object_id = object_uuid.to_string();
        record.objects[0].object.caller_object_id = Some("checkpoint-test-2".to_string());
        record.objects[0].copy.object_id = object_uuid.to_string();
        record.objects[0].copy.tape_file_number = 4;
        record.objects[0].copy.first_parity_data_ordinal = Some(3);
        record.objects[0].copy.protected_until_ordinal = Some(3);
        record.objects[0].total_committed_ordinals = 6;
        record.objects[0].bootstrap_object_row.tape_file_number = 4;
        record.objects[0].bootstrap_object_row.object_id = object_uuid.to_string().into_bytes();

        let mut object = parity_entry(4, remanence_parity::TapeFileKind::Object);
        object.block_count = 3;
        object.object_id = Some(object_uuid.to_string());
        object.first_parity_data_ordinal = Some(3);
        object.bootstrap_object_row = Some(record.objects[0].bootstrap_object_row.to_parity_row());
        record.object_tape_file_bundles = vec![remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::Object,
            entries: vec![object],
            highest_protected_ordinal: 3,
            total_committed_ordinals: 6,
        }];

        let mut sidecar = parity_entry(5, remanence_parity::TapeFileKind::ParitySidecar);
        sidecar.epoch_id = Some(1);
        sidecar.protected_ordinal_start = Some(3);
        sidecar.protected_ordinal_end_exclusive = Some(6);
        sidecar.canonical_metadata_hash = Some([0x62; 32]);
        let mut bootstrap = parity_entry(6, remanence_parity::TapeFileKind::Bootstrap);
        bootstrap.physical_start_hint = Some(20);
        record.checkpoint_bundle = Some(remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::Control,
            entries: vec![sidecar, bootstrap],
            highest_protected_ordinal: 6,
            total_committed_ordinals: 6,
        });
        record
    }

    fn committed_state_for_parity_records(
        records: &[CheckpointJournalRecord],
    ) -> remanence_parity::CommittedState {
        let mut entries = vec![parity_entry(0, remanence_parity::TapeFileKind::Bootstrap)];
        for record in records {
            entries.extend(
                record
                    .object_tape_file_bundles
                    .iter()
                    .flat_map(|bundle| bundle.entries.iter().cloned()),
            );
            entries.extend(
                record
                    .checkpoint_bundle
                    .as_ref()
                    .expect("checkpoint bundle")
                    .entries
                    .iter()
                    .cloned(),
            );
        }
        for entry in &mut entries {
            if entry.kind == remanence_parity::TapeFileKind::Object {
                entry.object_id = None;
            }
        }
        let checkpoint_bundle = records
            .last()
            .expect("at least one checkpoint record")
            .checkpoint_bundle
            .as_ref()
            .expect("checkpoint bundle");
        remanence_parity::CommittedState {
            entries,
            highest_protected_ordinal: checkpoint_bundle.highest_protected_ordinal,
            total_committed_ordinals: checkpoint_bundle.total_committed_ordinals,
            orphaned_bundles: Vec::new(),
        }
    }

    #[test]
    fn parity_resume_requires_checkpoint_and_sink_journal_to_name_same_prefix() {
        let tape_uuid = [0x28; 16];
        let record = partial_epoch_parity_record(tape_uuid);
        let scheme = record.scheme.as_ref().expect("parity scheme");
        let committed = committed_state_for_parity_records(std::slice::from_ref(&record));

        validate_parity_resume_authority(
            std::slice::from_ref(&record),
            &committed,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect("matching durable authorities permit resume");

        let mut wrong_identity = committed.clone();
        wrong_identity
            .entries
            .iter_mut()
            .find(|entry| entry.kind == remanence_parity::TapeFileKind::Object)
            .and_then(|entry| entry.bootstrap_object_row.as_mut())
            .expect("sink object row")
            .object_id = Some(b"different-object".to_vec());
        validate_parity_resume_authority(
            std::slice::from_ref(&record),
            &wrong_identity,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect_err("higher-layer enrichment cannot hide a sink object identity mismatch");

        let mut wrong_inline_identity = committed.clone();
        wrong_inline_identity
            .entries
            .iter_mut()
            .find(|entry| entry.kind == remanence_parity::TapeFileKind::Object)
            .expect("sink object entry")
            .object_id = Some("different-object".to_string());
        validate_parity_resume_authority(
            std::slice::from_ref(&record),
            &wrong_inline_identity,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect_err("a conflicting inline sink object identity must fail closed");

        let mut sink_ahead = committed.clone();
        let mut newer_bootstrap = parity_entry(4, remanence_parity::TapeFileKind::Bootstrap);
        newer_bootstrap.physical_start_hint = Some(record.eod_lba);
        sink_ahead.entries.push(newer_bootstrap);
        let error = validate_parity_resume_authority(
            std::slice::from_ref(&record),
            &sink_ahead,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect_err("a sink journal one checkpoint ahead must fail closed");
        assert!(error
            .to_string()
            .contains("parity resume authority mismatch"));

        let mut stale_eod = record.clone();
        stale_eod.eod_lba -= 1;
        let error = validate_parity_resume_authority(
            std::slice::from_ref(&stale_eod),
            &committed,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect_err("a stale physical checkpoint position must fail closed");
        assert!(error.to_string().contains("expected partition 0 lba 12"));

        let empty = remanence_parity::CommittedState {
            entries: Vec::new(),
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
            orphaned_bundles: Vec::new(),
        };
        validate_parity_resume_authority(&[], &empty, tape_uuid, record.block_size, scheme)
            .expect("two empty authorities describe fresh media");
        let error = validate_parity_resume_authority(
            std::slice::from_ref(&record),
            &empty,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect_err("checkpoint-only authority must fail closed");
        assert!(error
            .to_string()
            .contains("sink journal has no committed prefix"));
        let error =
            validate_parity_resume_authority(&[], &committed, tape_uuid, record.block_size, scheme)
                .expect_err("sink-only authority must fail closed");
        assert!(error.to_string().contains("checkpoint journal is empty"));

        let second = second_partial_epoch_parity_record(&record);
        let records = vec![record.clone(), second];
        let committed = committed_state_for_parity_records(&records);
        validate_parity_resume_authority(
            &records,
            &committed,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect("the complete two-checkpoint prefix permits resume");

        let mut changed_old_prefix = committed;
        changed_old_prefix.entries[1].physical_start_hint = Some(99);
        validate_parity_resume_authority(
            &records,
            &changed_old_prefix,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect_err("a mismatch in an older checkpoint prefix must fail closed");
    }

    #[test]
    fn fsynced_partial_epoch_parity_checkpoint_round_trips() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x25; 16];
        let record = partial_epoch_parity_record(tape_uuid);
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");

        journal
            .append(&record)
            .expect("append sidecar-plus-Bootstrap checkpoint");
        assert_eq!(
            journal.replay().expect("replay parity checkpoint"),
            vec![record]
        );
    }

    #[test]
    fn exclusive_checkpoint_lease_serializes_replay_through_append() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x24; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let mut lease = journal
            .acquire_exclusive()
            .expect("acquire write-session lease");
        assert!(lease.replay().expect("replay under lease").is_empty());

        journal
            .replay()
            .expect_err("shared replay must not overlap a retained write lease");
        journal
            .append(&record(tape_uuid))
            .expect_err("a second writer must fail before deriving the same prefix");

        lease
            .append(&record(tape_uuid))
            .expect("lease owner appends checkpoint");
        drop(lease);
        assert_eq!(
            journal.replay().expect("replay after lease release"),
            vec![record(tape_uuid)]
        );
    }

    #[test]
    fn first_use_contention_never_publishes_a_partial_checkpoint_header() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x23; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let creator_lock = acquire_checkpoint_lock(
            journal.path(),
            FlockArg::LockExclusiveNonblock,
            "hold simulated creator lock",
        )
        .expect("hold stable creation lock");

        journal
            .acquire_exclusive()
            .expect_err("a competing first writer must lose before creating authority");
        assert!(
            !journal.path().exists(),
            "first-use contention must not publish an empty final journal"
        );
        drop(creator_lock);

        let init_path = checkpoint_companion_path(journal.path(), ".init");
        std::fs::write(&init_path, b"simulated-crash-partial-header")
            .expect("seed unpublished partial initialization file");
        let mut lease = journal
            .acquire_exclusive()
            .expect("retry atomically publishes a complete header");
        assert!(lease.replay().expect("replay published header").is_empty());
        assert_eq!(
            std::fs::metadata(journal.path())
                .expect("stat published journal")
                .len(),
            CHECKPOINT_JOURNAL_HEADER_LEN
        );
        assert!(
            !init_path.exists(),
            "atomic rename consumes the initialization file"
        );
    }

    #[test]
    fn parity_checkpoint_validator_accepts_all_current_control_suffixes() {
        let tape_uuid = [0x26; 16];
        let sidecar_then_bootstrap = partial_epoch_parity_record(tape_uuid);

        let mut bootstrap_only = sidecar_then_bootstrap.clone();
        let sidecar = bootstrap_only
            .checkpoint_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .entries
            .remove(0);
        bootstrap_only.object_tape_file_bundles[0]
            .entries
            .push(sidecar);
        bootstrap_only.object_tape_file_bundles[0].highest_protected_ordinal = 3;

        let mut parity_map_then_bootstrap = bootstrap_only.clone();
        parity_map_then_bootstrap.checkpoint_tape_file_number = 4;
        parity_map_then_bootstrap
            .checkpoint_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .entries = vec![
            parity_entry(3, remanence_parity::TapeFileKind::ParityMap),
            parity_entry(4, remanence_parity::TapeFileKind::Bootstrap),
        ];

        let mut sidecar_parity_map_bootstrap = sidecar_then_bootstrap.clone();
        sidecar_parity_map_bootstrap.checkpoint_tape_file_number = 4;
        sidecar_parity_map_bootstrap
            .checkpoint_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .entries
            .insert(
                1,
                parity_entry(3, remanence_parity::TapeFileKind::ParityMap),
            );
        sidecar_parity_map_bootstrap
            .checkpoint_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .entries[2]
            .tape_file_number = 4;

        for record in [
            bootstrap_only,
            parity_map_then_bootstrap,
            sidecar_then_bootstrap,
            sidecar_parity_map_bootstrap,
        ] {
            validate_parity_checkpoint_bundles(&record)
                .unwrap_or_else(|err| panic!("valid checkpoint control suffix rejected: {err}"));
        }
    }

    #[test]
    fn parity_checkpoint_validator_rejects_unprotected_or_misnumbered_barriers() {
        let tape_uuid = [0x27; 16];
        let mut unprotected = partial_epoch_parity_record(tape_uuid);
        unprotected
            .checkpoint_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .entries
            .remove(0);
        unprotected
            .checkpoint_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .entries[0]
            .tape_file_number = 2;
        unprotected
            .checkpoint_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .highest_protected_ordinal = 0;
        unprotected.checkpoint_tape_file_number = 2;
        assert!(validate_parity_checkpoint_bundles(&unprotected)
            .expect_err("checkpoint must close the open epoch")
            .to_string()
            .contains("left ordinals unprotected"));

        let mut misnumbered = partial_epoch_parity_record(tape_uuid);
        misnumbered.checkpoint_tape_file_number = 9;
        assert!(validate_parity_checkpoint_bundles(&misnumbered)
            .expect_err("record must identify the terminal Bootstrap")
            .to_string()
            .contains("record names 9"));

        let mut discontinuous_sidecar = partial_epoch_parity_record(tape_uuid);
        discontinuous_sidecar
            .checkpoint_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .entries[0]
            .protected_ordinal_start = Some(1);
        assert!(validate_parity_checkpoint_bundles(&discontinuous_sidecar)
            .expect_err("sidecar range must start at the prior watermark")
            .to_string()
            .contains("starting at 0"));
    }

    #[test]
    fn torn_final_frame_fails_closed_and_is_not_repaired_by_append() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x11; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        journal
            .append(&record(tape_uuid))
            .expect("append checkpoint");
        let mut file = OpenOptions::new()
            .append(true)
            .open(journal.path())
            .expect("open torn tail");
        file.write_all(&CHECKPOINT_RECORD_VERSION.to_le_bytes()[..1])
            .expect("write torn checkpoint tail");
        file.sync_all().expect("sync torn checkpoint tail");
        let torn_len = file.metadata().expect("stat torn journal").len();
        drop(file);

        let replay_err = journal
            .replay()
            .expect_err("torn checkpoint authority must fail closed");
        assert!(
            replay_err.to_string().contains("torn trailing frame"),
            "{replay_err}"
        );
        let append_err = journal
            .append(&second_record(tape_uuid))
            .expect_err("append must not erase a torn checkpoint tail");
        assert!(
            append_err.to_string().contains("explicit recovery"),
            "{append_err}"
        );
        assert_eq!(
            std::fs::metadata(journal.path())
                .expect("stat preserved torn journal")
                .len(),
            torn_len,
            "failed append must preserve torn evidence"
        );
    }

    #[test]
    fn headerless_checkpoint_tail_fails_closed_as_legacy_or_incomplete() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x31; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal.path())
            .expect("open partial checkpoint record");
        file.write_all(b"{\"ordinal\":1")
            .expect("write incomplete checkpoint record");

        let err = journal
            .replay()
            .expect_err("headerless checkpoint bytes must fail closed");
        assert!(err.to_string().contains("versioned header"), "{err}");
    }

    #[test]
    fn checksum_damage_fails_closed_before_any_later_record() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x32; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        journal
            .append(&record(tape_uuid))
            .expect("append first checkpoint");
        journal
            .append(&second_record(tape_uuid))
            .expect("append second checkpoint");

        let damage_offset = CHECKPOINT_JOURNAL_HEADER_LEN + CHECKPOINT_RECORD_PREFIX_LEN + 8;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(journal.path())
            .expect("open checkpoint for damage");
        file.seek(SeekFrom::Start(damage_offset))
            .expect("seek damaged payload byte");
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).expect("read payload byte");
        byte[0] ^= 0x80;
        file.seek(SeekFrom::Start(damage_offset))
            .expect("reseek damaged payload byte");
        file.write_all(&byte).expect("damage payload byte");
        file.sync_all().expect("sync checkpoint damage");

        let err = journal
            .replay()
            .expect_err("checksum damage must fail closed");
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        let append_err = journal
            .append(&second_record(tape_uuid))
            .expect_err("damage must fence later append");
        assert!(
            append_err.to_string().contains("checksum mismatch"),
            "{append_err}"
        );
    }

    #[test]
    fn hostile_declared_record_length_rejects_before_allocation() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x33; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        journal
            .append(&record(tape_uuid))
            .expect("create versioned journal");

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(journal.path())
            .expect("open checkpoint for hostile length");
        file.set_len(CHECKPOINT_JOURNAL_HEADER_LEN)
            .expect("truncate to header");
        file.seek(SeekFrom::End(0)).expect("seek after header");
        file.write_all(&CHECKPOINT_RECORD_VERSION.to_le_bytes())
            .expect("write record version");
        let hostile_len =
            u32::try_from(MAX_CHECKPOINT_RECORD_LEN + 1).expect("configured replay limit fits u32");
        file.write_all(&hostile_len.to_le_bytes())
            .expect("write hostile record length");
        file.sync_all().expect("sync hostile length");

        let err = journal
            .replay()
            .expect_err("hostile record length must reject");
        assert!(err.to_string().contains("declares"), "{err}");
        assert!(err.to_string().contains("limit"), "{err}");
    }

    #[test]
    fn append_rejects_non_monotonic_count() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x22; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        journal
            .append(&record(tape_uuid))
            .expect("append checkpoint");
        let mut invalid = record(tape_uuid);
        invalid.ordinal = 2;
        invalid.committed_object_count = 9;
        let err = journal
            .append(&invalid)
            .expect_err("invalid count must reject");
        assert!(err.to_string().contains("committed count"), "{err}");
    }

    #[test]
    fn append_rejects_a_checkpoint_after_terminal_seal_authority() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x25; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let prior = record(tape_uuid);
        journal
            .append(&prior)
            .expect("append ordinary checkpoint authority");
        let terminal = CheckpointJournalRecord {
            ordinal: 2,
            committed_object_count: 1,
            eod_partition: 0,
            eod_lba: 10,
            tape_uuid,
            batch_id: [0x45; 16],
            checkpoint_tape_file_number: 3,
            block_size: prior.block_size,
            objects: Vec::new(),
            scheme: None,
            object_tape_file_bundles: Vec::new(),
            checkpoint_bundle: Some(remanence_parity::CommittedBundle {
                kind: remanence_parity::CommittedBundleKind::Finish,
                entries: vec![{
                    let mut bootstrap = parity_entry(3, remanence_parity::TapeFileKind::Bootstrap);
                    bootstrap.physical_start_hint = Some(8);
                    bootstrap
                }],
                highest_protected_ordinal: 0,
                total_committed_ordinals: 3,
            }),
            sealed_after_write: true,
        };
        let err = journal
            .append(&terminal)
            .expect_err("terminal authority must require a durable Sealing intent");
        assert!(err.to_string().contains("durable Sealing intent"), "{err}");
        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        lease
            .begin_terminal_transition()
            .expect("persist Sealing intent");
        lease
            .append_terminal_transition(std::slice::from_ref(&terminal))
            .expect("append terminal checkpoint authority");
        drop(lease);

        let err = journal
            .append(&second_record(tape_uuid))
            .expect_err("a terminal checkpoint must permanently close the journal");
        assert!(
            err.to_string().contains("terminal sealed checkpoint"),
            "{err}"
        );
    }

    #[test]
    fn sealing_intent_blocks_replay_until_terminal_authority_is_durable() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x28; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        lease
            .begin_terminal_transition()
            .expect("persist Sealing intent");
        drop(lease);

        let err = journal
            .replay()
            .expect_err("pending Sealing intent must fail replay closed");
        assert!(err.to_string().contains("pending Sealing intent"), "{err}");
        let err = journal
            .acquire_exclusive()
            .expect_err("pending Sealing intent must fence append acquisition");
        assert!(err.to_string().contains("pending Sealing intent"), "{err}");

        let mut recovery = journal
            .acquire_exclusive_for_terminal_recovery()
            .expect("acquire explicit terminal recovery lease");
        let changed = recovery
            .clear_terminal_intent_after_absent_tail(None, 1)
            .expect_err("recovery must compare the durable EOD it physically inspected");
        assert!(
            changed.to_string().contains("authority changed"),
            "{changed}"
        );
        recovery
            .clear_terminal_intent_after_absent_tail(None, 0)
            .expect("clear intent after proving an absent fresh-tape terminal tail");
        drop(recovery);
        journal
            .acquire_exclusive()
            .expect("ordinary append lease resumes after explicit reconciliation");
    }

    #[test]
    fn unpublished_sealing_intent_temporary_is_safe_to_replace() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x2A; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let temporary_path = checkpoint_companion_path(journal.path(), ".sealing.new");
        std::fs::write(&temporary_path, b"torn unpublished intent")
            .expect("simulate crash before intent publication");

        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        lease
            .begin_terminal_transition()
            .expect("atomically replace unpublished temporary intent");
        drop(lease);

        assert!(
            terminal_intent_pending(journal.path(), tape_uuid).expect("published intent validates")
        );
        assert!(!temporary_path.exists());
    }

    #[test]
    fn terminal_transition_clears_intent_after_one_fsynced_frame() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x29; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let prior = record(tape_uuid);
        let terminal = CheckpointJournalRecord {
            ordinal: 2,
            committed_object_count: 1,
            eod_partition: 0,
            eod_lba: 10,
            tape_uuid,
            batch_id: [0x49; 16],
            checkpoint_tape_file_number: 3,
            block_size: prior.block_size,
            objects: Vec::new(),
            scheme: None,
            object_tape_file_bundles: Vec::new(),
            checkpoint_bundle: Some(remanence_parity::CommittedBundle {
                kind: remanence_parity::CommittedBundleKind::Finish,
                entries: vec![{
                    let mut bootstrap = parity_entry(3, remanence_parity::TapeFileKind::Bootstrap);
                    bootstrap.physical_start_hint = Some(8);
                    bootstrap
                }],
                highest_protected_ordinal: 0,
                total_committed_ordinals: 3,
            }),
            sealed_after_write: true,
        };
        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        lease
            .begin_terminal_transition()
            .expect("persist Sealing intent");
        lease
            .append_terminal_transition(&[prior.clone(), terminal.clone()])
            .expect("append terminal authority transition");
        drop(lease);
        assert!(!terminal_intent_path(journal.path()).exists());
        assert_eq!(
            journal.replay().expect("replay terminal transition"),
            vec![prior, terminal]
        );

        write_terminal_intent(journal.path(), tape_uuid)
            .expect("simulate crash after terminal frame fsync before intent cleanup");
        let lease = journal
            .acquire_exclusive()
            .expect("completed authority makes stale intent safely removable");
        drop(lease);
        assert!(!terminal_intent_path(journal.path()).exists());
    }

    #[test]
    fn parity_terminal_checkpoint_requires_exact_bootstrap_eod() {
        let tape_uuid = [0x26; 16];
        let prior = partial_epoch_parity_record(tape_uuid);
        let prior_bundle = prior
            .checkpoint_bundle
            .as_ref()
            .expect("prior parity checkpoint bundle");
        let terminal = |physical_start_hint, block_count, eod_lba| CheckpointJournalRecord {
            ordinal: 2,
            committed_object_count: prior.committed_object_count,
            eod_partition: 0,
            eod_lba,
            tape_uuid,
            batch_id: [0x46; 16],
            checkpoint_tape_file_number: 4,
            block_size: prior.block_size,
            objects: Vec::new(),
            scheme: prior.scheme.clone(),
            object_tape_file_bundles: Vec::new(),
            checkpoint_bundle: Some(remanence_parity::CommittedBundle {
                kind: remanence_parity::CommittedBundleKind::Finish,
                entries: vec![{
                    let mut bootstrap = parity_entry(4, remanence_parity::TapeFileKind::Bootstrap);
                    bootstrap.physical_start_hint = physical_start_hint;
                    bootstrap.block_count = block_count;
                    bootstrap
                }],
                highest_protected_ordinal: prior_bundle.highest_protected_ordinal,
                total_committed_ordinals: prior_bundle.total_committed_ordinals,
            }),
            sealed_after_write: true,
        };

        validate_next_record(Some(&prior), &terminal(Some(12), 1, 14))
            .expect("exact terminal Bootstrap extent");
        let missing = validate_next_record(Some(&prior), &terminal(None, 1, 14))
            .expect_err("missing terminal physical start must reject");
        assert!(
            missing.to_string().contains("physical start hint"),
            "{missing}"
        );
        let overlap = validate_next_record(Some(&prior), &terminal(Some(11), 1, 13))
            .expect_err("terminal extent overlap must reject");
        assert!(
            overlap
                .to_string()
                .contains("expected prior terminal cursor 12"),
            "{overlap}"
        );
        let gap = validate_next_record(Some(&prior), &terminal(Some(13), 1, 15))
            .expect_err("terminal extent gap must reject");
        assert!(
            gap.to_string()
                .contains("expected prior terminal cursor 12"),
            "{gap}"
        );
        let mismatch = validate_next_record(Some(&prior), &terminal(Some(12), 1, 15))
            .expect_err("terminal EOD mismatch must reject");
        assert!(
            mismatch.to_string().contains("Finish bundle end"),
            "{mismatch}"
        );
    }

    #[test]
    fn checkpoint_batch_is_one_integrity_frame_and_torn_batch_fails_closed() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x27; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let first = record(tape_uuid);
        let second = second_record(tape_uuid);
        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        lease
            .append_batch(&[first.clone(), second.clone()])
            .expect("append checkpoint transition");
        drop(lease);
        assert_eq!(
            journal.replay().expect("replay checkpoint transition"),
            vec![first, second]
        );

        let file = OpenOptions::new()
            .write(true)
            .open(journal.path())
            .expect("open journal for crash cut");
        let len = file.metadata().expect("stat journal").len();
        file.set_len(len - 1).expect("tear transition checksum");
        file.sync_all().expect("sync torn transition");
        let err = journal
            .replay()
            .expect_err("a torn multi-record transition must fail closed");
        assert!(err.to_string().contains("torn trailing frame"), "{err}");
    }

    #[test]
    fn validation_rejects_parity_scheme_change_between_checkpoints() {
        let tape_uuid = [0x23; 16];
        let mut previous = record(tape_uuid);
        previous.scheme = Some(remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_static("checkpoint-scheme-a"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 3,
        });
        let mut next = second_record(tape_uuid);
        next.scheme = Some(remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_static("checkpoint-scheme-b"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 3,
        });

        let err = validate_next_record(Some(&previous), &next)
            .expect_err("one tape checkpoint journal cannot change parity schemes");
        assert!(err.to_string().contains("scheme changed"), "{err}");
    }

    #[test]
    fn validation_accepts_non_uuid_object_id() {
        // object_id is opaque UTF-8, 1-64 bytes (REM-OBJECT 4.5.1); the state layer must
        // not require it to parse as a UUID (task #28 — vestigial UUID guards removed).
        let tape_uuid = [0x24; 16];
        let mut rec = record(tape_uuid);
        let opaque = "accession-2026-0007";
        rec.objects[0].object.object_id = opaque.to_string();
        rec.objects[0].copy.object_id = opaque.to_string();
        rec.objects[0].bootstrap_object_row.object_id = opaque.as_bytes().to_vec();

        validate_next_record(None, &rec).expect("a non-UUID opaque object_id must validate");
    }
}
