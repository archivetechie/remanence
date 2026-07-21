//! Durable parity-off checkpoint journal and replayable batch projections.
//!
//! The append-only journal is the numbering and recovery-position authority.
//! Each newline-framed JSON record is fsynced before its corresponding SQLite
//! batch projection. A torn final line is ignored; corruption before the final
//! record boundary is fatal.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    NativeObjectCopyProjectionInput, NativeObjectFileProjectionInput, NativeObjectProjectionInput,
    StateError,
};

const CHECKPOINT_JOURNAL_SUFFIX: &str = ".remcheckpoint";

/// Stable journal representation of one RAO recovery row carried by an
/// on-tape checkpoint bootstrap.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CheckpointBootstrapObjectRow {
    /// Filemark-delimited tape-file number of the object copy.
    pub tape_file_number: u32,
    /// Number of fixed-size records occupied by the stored copy.
    pub stored_block_count: u64,
    /// RAO object UUID.
    pub object_id: [u8; 16],
    /// Representation-specific recovery anchors.
    pub representation: CheckpointBootstrapObjectRepresentation,
}

/// Stable representation-specific payload for a checkpoint bootstrap row.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CheckpointBootstrapObjectRepresentation {
    /// Plaintext RAO manifest anchors.
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
    /// Encrypted RAO envelope anchors.
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
        row.with_object_id(self.object_id)
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
    /// RAO recovery row emitted in every later checkpoint bootstrap.
    pub bootstrap_object_row: CheckpointBootstrapObjectRow,
}

/// One fsynced barrier record in the checkpoint journal.
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
    /// Session batch identifier whose objects this barrier committed.
    pub batch_id: [u8; 16],
    /// Tape-file number occupied by this checkpoint's on-tape bootstrap.
    pub checkpoint_tape_file_number: u32,
    /// Fixed tape block size used to encode that bootstrap.
    pub block_size: u32,
    /// Replayable projections made durable by this record.
    pub objects: Vec<CheckpointObjectProjection>,
    /// Parity scheme for parity-protected checkpoint batches. `None` denotes
    /// the historical parity-off record shape.
    #[serde(default)]
    pub scheme: Option<remanence_parity::ParityScheme>,
    /// Per-object Layer 3c bundles, in the same order as `objects`.
    #[serde(default)]
    pub object_tape_file_bundles: Vec<remanence_parity::CommittedBundle>,
    /// Sidecar/bootstrap bundle emitted by the barrier on parity tapes.
    #[serde(default)]
    pub checkpoint_bundle: Option<remanence_parity::CommittedBundle>,
}

/// Append-only per-tape checkpoint journal.
#[derive(Debug)]
pub struct FileCheckpointJournal {
    path: PathBuf,
    tape_uuid: [u8; 16],
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
        Ok(Self { path, tape_uuid })
    }

    /// Append and fsync one validated checkpoint record.
    pub fn append(&self, record: &CheckpointJournalRecord) -> Result<(), StateError> {
        if record.tape_uuid != self.tape_uuid {
            return Err(StateError::JournalReplayFailed(
                "checkpoint record tape_uuid does not match journal".to_string(),
            ));
        }
        let prior = self.replay()?;
        validate_next_record(prior.last(), record)?;
        self.truncate_torn_tail()?;
        let mut encoded = serde_json::to_vec(record).map_err(|err| {
            StateError::JournalReplayFailed(format!("encode checkpoint record: {err}"))
        })?;
        encoded.push(b'\n');
        let created = !self.path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|err| StateError::io_at("open checkpoint journal", &self.path, err))?;
        file.write_all(&encoded)
            .map_err(|err| StateError::io_at("append checkpoint record", &self.path, err))?;
        file.flush()
            .map_err(|err| StateError::io_at("flush checkpoint record", &self.path, err))?;
        file.sync_all()
            .map_err(|err| StateError::io_at("fsync checkpoint record", &self.path, err))?;
        if created {
            let parent = self.path.parent().ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "checkpoint journal path has no parent directory".to_string(),
                )
            })?;
            File::open(parent)
                .and_then(|dir| dir.sync_all())
                .map_err(|err| {
                    StateError::io_at("fsync checkpoint journal directory", parent, err)
                })?;
        }
        Ok(())
    }

    /// Replay every complete record, ignoring only a torn final line.
    pub fn replay(&self) -> Result<Vec<CheckpointJournalRecord>, StateError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let mut bytes = Vec::new();
        File::open(&self.path)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|err| StateError::io_at("read checkpoint journal", &self.path, err))?;
        let complete_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let mut records = Vec::new();
        for (line_index, line) in bytes[..complete_len]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .enumerate()
        {
            let record: CheckpointJournalRecord = serde_json::from_slice(line).map_err(|err| {
                StateError::JournalReplayFailed(format!(
                    "decode checkpoint record {} in {}: {err}",
                    line_index + 1,
                    self.path.display()
                ))
            })?;
            if record.tape_uuid != self.tape_uuid {
                return Err(StateError::JournalReplayFailed(format!(
                    "checkpoint record {} tape_uuid mismatch in {}",
                    line_index + 1,
                    self.path.display()
                )));
            }
            validate_next_record(records.last(), &record)?;
            records.push(record);
        }
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

    fn truncate_torn_tail(&self) -> Result<(), StateError> {
        if !self.path.exists() {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|err| {
                StateError::io_at("open checkpoint journal for repair", &self.path, err)
            })?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|err| {
            StateError::io_at("read checkpoint journal for repair", &self.path, err)
        })?;
        let complete_len = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        if complete_len == bytes.len() {
            return Ok(());
        }
        file.set_len(complete_len as u64)
            .map_err(|err| StateError::io_at("truncate torn checkpoint tail", &self.path, err))?;
        file.sync_all()
            .map_err(|err| StateError::io_at("fsync checkpoint tail repair", &self.path, err))
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

fn validate_next_record(
    previous: Option<&CheckpointJournalRecord>,
    record: &CheckpointJournalRecord,
) -> Result<(), StateError> {
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
    if parity_record
        && (record.object_tape_file_bundles.len() != record.objects.len()
            || record.checkpoint_bundle.is_none())
    {
        return Err(StateError::JournalReplayFailed(
            "parity checkpoint record must carry one Layer 3c bundle per object and a barrier bundle"
                .to_string(),
        ));
    }
    if !parity_record
        && (!record.object_tape_file_bundles.is_empty() || record.checkpoint_bundle.is_some())
    {
        return Err(StateError::JournalReplayFailed(
            "parity-off checkpoint record carries parity bundle fields".to_string(),
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
    if record.objects.is_empty() {
        return Err(StateError::JournalReplayFailed(
            "checkpoint record must commit at least one object".to_string(),
        ));
    }
    if record.block_size == 0 {
        return Err(StateError::JournalReplayFailed(
            "checkpoint block size must be non-zero".to_string(),
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
            let bundle = &record.object_tape_file_bundles[index];
            let first = bundle.entries.first().ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "parity checkpoint object bundle is empty".to_string(),
                )
            })?;
            if first.kind != remanence_parity::TapeFileKind::Object {
                return Err(StateError::JournalReplayFailed(
                    "parity checkpoint object bundle does not start with its object".to_string(),
                ));
            }
            for entry in &bundle.entries {
                if entry.tape_file_number != expected_file {
                    return Err(StateError::JournalReplayFailed(format!(
                        "parity checkpoint bundle uses tape file {}, expected {expected_file}",
                        entry.tape_file_number
                    )));
                }
                expected_file = expected_file.checked_add(1).ok_or_else(|| {
                    StateError::JournalReplayFailed(
                        "checkpoint tape-file number overflows u32".to_string(),
                    )
                })?;
            }
            first.tape_file_number
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
        let parsed_object_id =
            uuid::Uuid::parse_str(&projection.object.object_id).map_err(|err| {
                StateError::JournalReplayFailed(format!(
                    "checkpoint object id {} is not a UUID: {err}",
                    projection.object.object_id
                ))
            })?;
        if row.object_id != *parsed_object_id.as_bytes() {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint object {} bootstrap row has a different object UUID",
                projection.object.object_id
            )));
        }
    }
    let expected_checkpoint_file = if let Some(bundle) = &record.checkpoint_bundle {
        let mut bootstrap_file = None;
        for entry in &bundle.entries {
            if entry.tape_file_number != expected_file {
                return Err(StateError::JournalReplayFailed(format!(
                    "parity checkpoint barrier uses tape file {}, expected {expected_file}",
                    entry.tape_file_number
                )));
            }
            expected_file = expected_file.checked_add(1).ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "checkpoint barrier tape-file number overflows u32".to_string(),
                )
            })?;
            if entry.kind == remanence_parity::TapeFileKind::Bootstrap {
                bootstrap_file = Some(entry.tape_file_number);
            }
        }
        bootstrap_file.ok_or_else(|| {
            StateError::JournalReplayFailed(
                "parity checkpoint barrier bundle has no bootstrap".to_string(),
            )
        })?
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
                    body_format: "rao-v1".to_string(),
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
                    object_id: *object_uuid.as_bytes(),
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
        record.objects[0].bootstrap_object_row.object_id = *object_uuid.as_bytes();
        record
    }

    #[test]
    fn fsynced_round_trip_ignores_only_torn_final_record() {
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
        file.write_all(b"{\"ordinal\":2")
            .expect("write torn checkpoint tail");
        file.sync_all().expect("sync torn checkpoint tail");

        assert_eq!(
            journal.replay().expect("replay journal"),
            vec![record(tape_uuid)]
        );

        journal
            .append(&second_record(tape_uuid))
            .expect("truncate torn tail and append next checkpoint");
        assert_eq!(
            journal.replay().expect("replay repaired journal"),
            vec![record(tape_uuid), second_record(tape_uuid)]
        );
    }

    #[test]
    fn post_barrier_pre_fsync_cut_has_no_durable_checkpoint() {
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

        assert!(
            journal.replay().expect("replay partial journal").is_empty(),
            "a record without its durable newline boundary is not a checkpoint"
        );
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
}
