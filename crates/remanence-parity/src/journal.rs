//! Durable tape-file journal for Layer 3c committed bundles.
//!
//! Layer 3c v0.7.2 decouples restart persistence from any database. The
//! write path records each filemark-durable commit unit as a
//! [`CommittedBundle`] through a [`TapeFileJournal`]. The default
//! [`FileTapeFileJournal`] is a local append-only file: a fixed header followed
//! by length-prefixed canonical CBOR bundle records with CRC-64/XZ checksums.
//! Replay fails closed at the first torn or corrupt record. Complete records
//! are filtered through the last `CheckpointedThrough` marker; later valid
//! bundles are surfaced as orphans because their tape writes were not included
//! in a daemon checkpoint projection.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(target_os = "linux")]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use ciborium::value::Value as CborValue;
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cbor::IntegerMapKeyTracker;
use crate::error::ParityError;
use crate::filemark_map::{FilemarkMap, TapeFileKind, TapeFileMapEntry};
use crate::mapping::data_shards_per_epoch;
use crate::model::ParityScheme;
use crate::object_recovery::{
    decode_object_recovery_row_cbor, encode_object_recovery_row_cbor, validate_object_recovery_row,
    ObjectRecoveryRow,
};
use crate::sidecar::crc64_xz;

const JOURNAL_MAGIC: &[u8; 8] = b"REMJRNL\x01";
const JOURNAL_VERSION: u16 = 4;
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
    /// A bundle does not match the structural grammar for its operational
    /// kind.
    #[error(transparent)]
    InvalidBundleShape(#[from] CommittedBundleShapeError),
    /// Journal path is on a filesystem class that cannot be trusted as a
    /// crash-recovery commit point.
    #[error(
        "journal volume rejected: {0} (must be a trusted local volume with honored fsync; §10.6)"
    )]
    UntrustedVolume(String),
    /// Replay found a torn or corrupt tail that must be reconciled against the
    /// physical tape before any append may proceed.
    #[error("journal recovery required: {0}")]
    RecoveryRequired(String),
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
    pub tape_file_number: u64,
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
    /// Optional higher-layer recovery row for object tape files.
    pub object_recovery_row: Option<ObjectRecoveryRow>,
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
            object_recovery_row: None,
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
    /// The sole tape-file-0 BOT Bootstrap.
    BotBootstrap,
    /// Sidecars emitted while closing a checkpoint epoch.
    CheckpointSidecars,
    /// Sidecars generated during restart append recovery.
    ResumeSidecars,
    /// Final partial sidecars and optional external ParityMap immediately
    /// before terminal replica A. This replacement grammar never includes a
    /// legacy final Bootstrap.
    TerminalPrefix,
    /// Exactly one barrier-proved A/gap-AB/B/gap-BC/C component.
    TerminalComponent,
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

/// Structural grammar violation in one committed tape-file bundle.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind:?} committed bundle: {detail}")]
pub struct CommittedBundleShapeError {
    kind: CommittedBundleKind,
    detail: String,
}

impl CommittedBundleShapeError {
    fn new(kind: CommittedBundleKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

/// Validate the current-wire entry grammar for one committed bundle.
///
/// Object bundles contain their Object first followed only by completed
/// sidecars. The sole BOT Bootstrap, checkpoint sidecars, restart sidecars,
/// and terminal components each use distinct exact grammars.
pub fn validate_committed_bundle_shape(
    bundle: &CommittedBundle,
) -> Result<Option<&TapeFileEntry>, CommittedBundleShapeError> {
    validate_dense_entry_numbers(bundle)?;
    match bundle.kind {
        CommittedBundleKind::Object => {
            let Some((object, tail)) = bundle.entries.split_first() else {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must contain an Object entry",
                ));
            };
            if object.kind != TapeFileKind::Object {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    format!("must start with Object, got {:?}", object.kind),
                ));
            }
            validate_sidecar_prefix(bundle.kind, tail)?;
            Ok(None)
        }
        CommittedBundleKind::BotBootstrap => {
            let [entry] = bundle.entries.as_slice() else {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must contain exactly one Bootstrap",
                ));
            };
            if entry.tape_file_number != 0
                || entry.kind != TapeFileKind::Bootstrap
                || entry.block_count != 1
            {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must contain the one-block tape-file-0 BOT Bootstrap",
                ));
            }
            Ok(Some(entry))
        }
        CommittedBundleKind::TerminalPrefix => {
            validate_terminal_prefix(bundle.kind, &bundle.entries)?;
            Ok(None)
        }
        CommittedBundleKind::TerminalComponent => {
            if !validate_terminal_control_component(bundle.kind, &bundle.entries)? {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must contain exactly one terminal index or separation component",
                ));
            }
            Ok(None)
        }
        CommittedBundleKind::CheckpointSidecars | CommittedBundleKind::ResumeSidecars => {
            if bundle.entries.is_empty() {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must contain at least one ParitySidecar",
                ));
            }
            validate_sidecar_prefix(bundle.kind, &bundle.entries)?;
            Ok(None)
        }
        CommittedBundleKind::CheckpointedThrough => {
            if !bundle.entries.is_empty() {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must not contain tape-file entries",
                ));
            }
            Ok(None)
        }
    }
}

fn validate_terminal_prefix(
    kind: CommittedBundleKind,
    entries: &[TapeFileEntry],
) -> Result<(), CommittedBundleShapeError> {
    let sidecars = match entries.split_last() {
        Some((last, sidecars)) if last.kind == TapeFileKind::ParityMap => sidecars,
        _ => entries,
    };
    validate_sidecar_prefix(kind, sidecars)
}

fn validate_terminal_control_component(
    kind: CommittedBundleKind,
    entries: &[TapeFileEntry],
) -> Result<bool, CommittedBundleShapeError> {
    let [entry] = entries else {
        return Ok(false);
    };
    if !matches!(
        entry.kind,
        TapeFileKind::TapeIndexReplica | TapeFileKind::IndexSeparationExtent
    ) {
        return Ok(false);
    }
    if entry.block_count == 0 {
        return Err(CommittedBundleShapeError::new(
            kind,
            format!(
                "terminal {:?} at tape file {} has zero blocks",
                entry.kind, entry.tape_file_number
            ),
        ));
    }
    if entry.object_id.is_some()
        || entry.first_parity_data_ordinal.is_some()
        || entry.epoch_id.is_some()
        || entry.protected_ordinal_start.is_some()
        || entry.protected_ordinal_end_exclusive.is_some()
        || entry.object_recovery_row.is_some()
    {
        return Err(CommittedBundleShapeError::new(
            kind,
            format!(
                "terminal {:?} at tape file {} has invalid kind-specific fields",
                entry.kind, entry.tape_file_number
            ),
        ));
    }
    Ok(true)
}

fn validate_dense_entry_numbers(bundle: &CommittedBundle) -> Result<(), CommittedBundleShapeError> {
    for pair in bundle.entries.windows(2) {
        let expected = pair[0].tape_file_number.checked_add(1).ok_or_else(|| {
            CommittedBundleShapeError::new(bundle.kind, "tape-file number overflows u64")
        })?;
        if pair[1].tape_file_number != expected {
            return Err(CommittedBundleShapeError::new(
                bundle.kind,
                format!(
                    "entries are not dense: expected tape file {expected}, got {}",
                    pair[1].tape_file_number
                ),
            ));
        }
    }
    Ok(())
}

fn validate_sidecar_prefix(
    kind: CommittedBundleKind,
    entries: &[TapeFileEntry],
) -> Result<(), CommittedBundleShapeError> {
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.kind != TapeFileKind::ParitySidecar)
    {
        return Err(CommittedBundleShapeError::new(
            kind,
            format!(
                "unexpected {:?} entry at tape file {}",
                entry.kind, entry.tape_file_number
            ),
        ));
    }
    Ok(())
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

    /// Idempotently make an exact terminal-prefix bundle and its following
    /// checkpoint watermark durable.
    ///
    /// File-backed journals override this to reconcile a prefix orphan or an
    /// already-checkpointed prefix after restart. Other implementations use
    /// the ordinary two-record append sequence.
    fn commit_terminal_prefix_transition(
        &mut self,
        prefix: &CommittedBundle,
        checkpoint: &CommittedBundle,
    ) -> Result<(), JournalError> {
        self.commit_bundle(prefix)?;
        self.commit_bundle(checkpoint)
    }

    /// Idempotently make one terminal component and its following checkpoint
    /// watermark durable.
    fn commit_terminal_component_transition(
        &mut self,
        component: &CommittedBundle,
        checkpoint: &CommittedBundle,
    ) -> Result<(), JournalError> {
        self.commit_bundle(component)?;
        self.commit_bundle(checkpoint)
    }

    /// Replay committed entries.
    ///
    /// Replay is non-destructive. File-backed journals return
    /// [`JournalError::RecoveryRequired`] for a torn or corrupt tail and never
    /// erase evidence merely because an append or replay handle was opened.
    fn load_committed(&self) -> Result<CommittedState, JournalError>;

    /// Freeze an allocation-bounded committed-prefix snapshot.
    ///
    /// Production append/resume must fail closed rather than fall back to
    /// whole-prefix materialization. File-backed journals override this;
    /// compatibility journals must explicitly implement an equivalent bounded
    /// authority if they enter that production path.
    fn committed_snapshot_bounded_authority(
        &self,
    ) -> Result<FileTapeFileJournalCommittedSnapshot, JournalError> {
        Err(JournalError::Codec(
            "journal does not provide bounded committed-prefix authority".into(),
        ))
    }
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
    orphaned_bundles_preserved_on_open: usize,
    terminal_grammar: TerminalJournalGrammar,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TerminalJournalPhase {
    #[default]
    Open,
    PrefixAwaitingCheckpoint,
    Ready {
        component_count: u8,
    },
    ComponentAwaitingCheckpoint {
        component_count: u8,
    },
    Complete,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TerminalJournalGrammar {
    phase: TerminalJournalPhase,
    last_tape_file_number: Option<u64>,
}

impl TerminalJournalGrammar {
    fn observe(&mut self, bundle: &CommittedBundle) -> Result<(), JournalError> {
        use TerminalJournalPhase::{
            Complete, ComponentAwaitingCheckpoint, Open, PrefixAwaitingCheckpoint, Ready,
        };
        if matches!(
            bundle.kind,
            CommittedBundleKind::TerminalPrefix | CommittedBundleKind::TerminalComponent
        ) {
            if let Some(first) = bundle.entries.first() {
                let expected = self.last_tape_file_number.map_or(Ok(0), |last| {
                    last.checked_add(1).ok_or_else(|| {
                        JournalError::Codec(
                            "terminal journal tape-file number overflows u64".into(),
                        )
                    })
                })?;
                if first.tape_file_number != expected {
                    return Err(JournalError::Codec(format!(
                        "terminal journal starts at tape file {}, expected {expected}",
                        first.tape_file_number
                    )));
                }
            }
        }
        match (self.phase, bundle.kind) {
            (Open, CommittedBundleKind::TerminalPrefix) => {
                self.phase = PrefixAwaitingCheckpoint;
            }
            (Open, CommittedBundleKind::TerminalComponent) => {
                return Err(JournalError::Codec(
                    "terminal component precedes TerminalPrefix authority".into(),
                ));
            }
            (Open, _) => {}
            (PrefixAwaitingCheckpoint, CommittedBundleKind::CheckpointedThrough) => {
                self.phase = Ready { component_count: 0 };
            }
            (Ready { component_count }, CommittedBundleKind::TerminalComponent) => {
                let entry = bundle.entries.first().ok_or_else(|| {
                    JournalError::Codec("terminal component bundle is empty".into())
                })?;
                let expected_kind = terminal_component_kind(component_count)?;
                if entry.kind != expected_kind {
                    return Err(JournalError::Codec(format!(
                        "terminal component {} is {:?}, expected {:?}",
                        component_count + 1,
                        entry.kind,
                        expected_kind
                    )));
                }
                self.phase = ComponentAwaitingCheckpoint {
                    component_count: component_count + 1,
                };
            }
            (
                ComponentAwaitingCheckpoint { component_count },
                CommittedBundleKind::CheckpointedThrough,
            ) => {
                self.phase = if component_count == 5 {
                    Complete
                } else {
                    Ready { component_count }
                };
            }
            (Complete, _) => {
                return Err(JournalError::Codec(
                    "journal record follows complete terminal A/gap/B/gap/C tail".into(),
                ));
            }
            (phase, kind) => {
                return Err(JournalError::Codec(format!(
                    "journal {kind:?} record is illegal in terminal phase {phase:?}"
                )));
            }
        }
        if let Some(last) = bundle.entries.last() {
            self.last_tape_file_number = Some(last.tape_file_number);
        }
        Ok(())
    }
}

fn terminal_component_kind(component_index: u8) -> Result<TapeFileKind, JournalError> {
    match component_index {
        0 | 2 | 4 => Ok(TapeFileKind::TapeIndexReplica),
        1 | 3 => Ok(TapeFileKind::IndexSeparationExtent),
        _ => Err(JournalError::Codec(
            "terminal journal already contains five components".into(),
        )),
    }
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

/// Allocation-bounded replay of the committed tape-file prefix.
///
/// The replay owns an independent read-only cursor over a frozen checkpoint
/// boundary. At most one length-bounded journal bundle is decoded at a time.
#[derive(Debug)]
pub struct FileTapeFileJournalCommittedReplay {
    file: File,
    committed_end: u64,
    current_entries: std::vec::IntoIter<TapeFileEntry>,
    highest_protected_ordinal: u64,
    total_committed_ordinals: u64,
    committed_entry_count: u64,
    metrics: BoundedJournalReplayMetrics,
    row_replay_started: bool,
}

/// Immutable, allocation-bounded view of one validated committed prefix.
///
/// The snapshot owns an independently opened read-only file description and a
/// frozen checkpoint boundary. Later append-only terminal journal transitions
/// do not change the rows replayed from this authority snapshot.
#[derive(Debug)]
pub struct FileTapeFileJournalCommittedSnapshot {
    path: PathBuf,
    _file: File,
    header: JournalHeader,
    committed_end: u64,
    highest_protected_ordinal: u64,
    total_committed_ordinals: u64,
    committed_entry_count: u64,
    metrics: BoundedJournalReplayMetrics,
}

/// Deterministic allocation and pass counters for bounded journal replay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BoundedJournalReplayMetrics {
    /// Complete validation scans performed before row replay.
    pub validation_passes: u64,
    /// Complete/started row-emission scans performed by this replay handle.
    pub row_replay_passes: u64,
    /// Number of framed bundle records seen by validation.
    pub journal_record_count: u64,
    /// Largest validated encoded bundle payload in bytes.
    pub peak_record_payload_bytes: u64,
    /// Largest number of decoded tape-file rows simultaneously retained by
    /// the validation scan. This includes the prior final-candidate bundle.
    pub peak_live_entry_count: u64,
}

impl FileTapeFileJournalCommittedReplay {
    /// Highest protected ordinal at the last durable checkpoint marker.
    pub const fn highest_protected_ordinal(&self) -> u64 {
        self.highest_protected_ordinal
    }

    /// Total committed data ordinals at the last durable checkpoint marker.
    pub const fn total_committed_ordinals(&self) -> u64 {
        self.total_committed_ordinals
    }

    /// Exact number of tape-file rows in the committed prefix.
    pub const fn committed_entry_count(&self) -> u64 {
        self.committed_entry_count
    }

    /// Return deterministic bounded-replay counters gathered so far.
    pub fn metrics(&self) -> BoundedJournalReplayMetrics {
        let mut metrics = self.metrics;
        metrics.row_replay_passes = u64::from(self.row_replay_started);
        metrics
    }

    /// Decode and return the next committed row.
    ///
    /// Empty checkpoint-marker bundles are skipped. The iterator never holds
    /// rows from more than one journal bundle.
    pub fn next_entry(&mut self) -> Result<Option<TapeFileEntry>, JournalError> {
        self.row_replay_started = true;
        loop {
            if let Some(entry) = self.current_entries.next() {
                return Ok(Some(entry));
            }
            if self.file.stream_position()? == self.committed_end {
                return Ok(None);
            }
            let frame = read_validated_journal_bundle(&mut self.file, self.committed_end)?
                .ok_or_else(|| {
                    JournalError::RecoveryRequired(
                        "committed replay ended before its validated checkpoint boundary".into(),
                    )
                })?;
            self.current_entries = frame.bundle.entries.into_iter();
        }
    }
}

impl FileTapeFileJournalCommittedSnapshot {
    /// Validation-pass and peak-allocation counters captured at freeze time.
    pub const fn metrics(&self) -> BoundedJournalReplayMetrics {
        self.metrics
    }

    /// Exact number of structural rows in the frozen committed prefix.
    pub const fn committed_entry_count(&self) -> u64 {
        self.committed_entry_count
    }

    /// Highest protected ordinal at the frozen checkpoint boundary.
    pub const fn highest_protected_ordinal(&self) -> u64 {
        self.highest_protected_ordinal
    }

    /// Total committed Object-data ordinals at the frozen checkpoint boundary.
    pub const fn total_committed_ordinals(&self) -> u64 {
        self.total_committed_ordinals
    }

    /// Tape UUID frozen into this journal authority.
    pub const fn tape_uuid(&self) -> [u8; 16] {
        self.header.tape_uuid
    }

    /// Fixed block size frozen into this journal authority.
    pub const fn block_size(&self) -> u32 {
        self.header.block_size
    }

    /// Parity scheme frozen into this journal authority.
    pub const fn scheme(&self) -> &ParityScheme {
        &self.header.scheme
    }

    /// Open a fresh cursor over the frozen committed prefix.
    pub fn replay(&self) -> Result<FileTapeFileJournalCommittedReplay, JournalError> {
        let mut file = File::open(&self.path)?;
        #[cfg(target_os = "linux")]
        {
            let frozen_metadata = self._file.metadata()?;
            let replay_metadata = file.metadata()?;
            if frozen_metadata.dev() != replay_metadata.dev()
                || frozen_metadata.ino() != replay_metadata.ino()
            {
                return Err(JournalError::RecoveryRequired(
                    "journal path no longer names the frozen authority file".into(),
                ));
            }
        }
        let header = read_header(&mut file)?;
        if header != self.header {
            return Err(JournalError::HeaderMismatch);
        }
        if file.metadata()?.len() < self.committed_end {
            return Err(JournalError::RecoveryRequired(
                "journal was truncated before the frozen committed boundary".into(),
            ));
        }
        Ok(FileTapeFileJournalCommittedReplay {
            file,
            committed_end: self.committed_end,
            current_entries: Vec::new().into_iter(),
            highest_protected_ordinal: self.highest_protected_ordinal,
            total_committed_ordinals: self.total_committed_ordinals,
            committed_entry_count: self.committed_entry_count,
            metrics: self.metrics,
            row_replay_started: false,
        })
    }
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
        validate_trusted_journal_volume(&path)?;
        Self::open_inner(path, tape_uuid, block_size, scheme)
    }

    /// Report whether this exact terminal-prefix bundle and its canonical
    /// checkpoint watermark are already durable.
    ///
    /// This inspects journal records rather than flattened committed rows, so
    /// an empty `TerminalPrefix` remains distinguishable from the open phase.
    /// A different persisted terminal prefix is a conflict, not `false`.
    pub fn terminal_prefix_transition_is_durable(
        &self,
        prefix: &CommittedBundle,
        checkpoint: &CommittedBundle,
    ) -> Result<bool, JournalError> {
        if prefix.kind != CommittedBundleKind::TerminalPrefix
            || checkpoint.kind != CommittedBundleKind::CheckpointedThrough
            || !checkpoint.entries.is_empty()
            || prefix.highest_protected_ordinal != checkpoint.highest_protected_ordinal
            || prefix.total_committed_ordinals != checkpoint.total_committed_ordinals
        {
            return Err(JournalError::Codec(
                "invalid terminal-prefix/checkpoint transition probe".into(),
            ));
        }
        validate_committed_bundle_shape(prefix)?;
        validate_committed_bundle_shape(checkpoint)?;
        let mut replay_file = self.file.try_clone()?;
        let file_len = replay_file.metadata()?.len();
        read_header(&mut replay_file)?;
        let replay = scan_bounded_committed_metadata(&mut replay_file, file_len)?;
        let prefix_sha256 = encoded_bundle_sha256(prefix)?;
        let checkpoint_sha256 = encoded_bundle_sha256(checkpoint)?;
        match replay.terminal_grammar.phase {
            TerminalJournalPhase::Open => Ok(false),
            TerminalJournalPhase::PrefixAwaitingCheckpoint => {
                if replay.terminal_prefix_payload_sha256 != Some(prefix_sha256) {
                    return Err(JournalError::Codec(
                        "persisted terminal prefix conflicts with requested plan".into(),
                    ));
                }
                Ok(false)
            }
            _ => {
                if replay.terminal_prefix_payload_sha256 != Some(prefix_sha256) {
                    return Err(JournalError::Codec(
                        "persisted terminal prefix conflicts with requested plan".into(),
                    ));
                }
                Ok(replay.terminal_prefix_checkpoint_sha256 == Some(checkpoint_sha256))
            }
        }
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
        validate_trusted_journal_volume(&path)?;
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
        validate_trusted_journal_volume(&path)?;
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
        let mut orphaned_bundles_preserved_on_open = 0;
        let mut terminal_grammar = TerminalJournalGrammar::default();
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
            let replay = scan_bounded_committed_metadata(&mut file, len)?;
            last_highest_protected_ordinal = replay.highest_protected_ordinal;
            last_total_committed_ordinals = replay.total_committed_ordinals;
            orphaned_bundles_preserved_on_open = usize::try_from(replay.orphan_bundle_count)
                .map_err(|_| JournalError::Codec("orphan count does not fit usize".into()))?;
            terminal_grammar = replay.terminal_grammar;
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
            orphaned_bundles_preserved_on_open,
            terminal_grammar,
        })
    }

    /// Number of valid post-watermark bundles preserved by this exclusive open.
    ///
    /// A non-zero value fences appends until higher-layer reconciliation has
    /// compared the journal evidence with the physical tape tail and checkpoint
    /// authority. Opening an append handle never erases this evidence.
    pub fn orphaned_bundles_preserved_on_open(&self) -> usize {
        self.orphaned_bundles_preserved_on_open
    }

    /// Tape UUID this append/replay authority belongs to.
    pub fn tape_uuid(&self) -> [u8; 16] {
        self.tape_uuid
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

    /// Open an allocation-bounded replay of the last checkpointed prefix.
    ///
    /// Construction validates every journal frame, its monotonic watermarks,
    /// terminal grammar, and the absence of an uncheckpointed orphan suffix.
    /// The returned replay then decodes at most one 64-MiB-bounded bundle at a
    /// time from its frozen checkpoint boundary without borrowing this handle.
    pub fn replay_committed_entries_bounded(
        &self,
    ) -> Result<FileTapeFileJournalCommittedReplay, JournalError> {
        self.committed_snapshot_bounded()?.replay()
    }

    /// Freeze the last checkpointed prefix into an owned read-only authority.
    ///
    /// Callers construct this while retaining the journal's exclusive append
    /// handle. The returned snapshot has no Rust borrow of that handle, so the
    /// caller may append terminal progress while all edition passes continue
    /// to replay the exact preselected prefix.
    pub fn committed_snapshot_bounded(
        &self,
    ) -> Result<FileTapeFileJournalCommittedSnapshot, JournalError> {
        self.snapshot_bounded_at(false, None)
    }

    /// Freeze the last checkpointed base for an immutable planned
    /// `TerminalPrefix`.
    ///
    /// Besides an open journal, this admits exactly one matching
    /// `TerminalPrefix` orphan left by a crash before its following checkpoint
    /// marker. Any other orphan suffix or terminal phase fails closed. The
    /// returned rows stop before the prefix because callers append that plan
    /// virtually while reconstructing the terminal edition.
    pub fn planned_terminal_prefix_base_snapshot_bounded(
        &self,
        prefix: &CommittedBundle,
    ) -> Result<FileTapeFileJournalCommittedSnapshot, JournalError> {
        if prefix.kind != CommittedBundleKind::TerminalPrefix {
            return Err(JournalError::Codec(
                "planned bounded snapshot requires a TerminalPrefix bundle".into(),
            ));
        }
        validate_committed_bundle_shape(prefix)?;
        self.snapshot_bounded_at(false, Some(encoded_bundle_sha256(prefix)?))
    }

    /// Freeze the committed journal exactly through the TerminalPrefix
    /// checkpoint, excluding any later A/gap/B/gap/C progress records.
    ///
    /// This restart surface validates the complete current journal first, so
    /// damage or orphan evidence in the later tail still fails closed.
    pub fn terminal_prefix_snapshot_bounded(
        &self,
    ) -> Result<FileTapeFileJournalCommittedSnapshot, JournalError> {
        self.snapshot_bounded_at(true, None)
    }

    fn snapshot_bounded_at(
        &self,
        through_terminal_prefix: bool,
        planned_terminal_prefix_sha256: Option<[u8; 32]>,
    ) -> Result<FileTapeFileJournalCommittedSnapshot, JournalError> {
        let mut validation_file = File::open(&self.path)?;
        let file_len = validation_file.metadata()?.len();
        let header = read_header(&mut validation_file)?;
        if header.tape_uuid != self.tape_uuid
            || header.block_size != self.block_size
            || header.drive_compression != self.drive_compression
            || header.scheme != self.scheme
        {
            return Err(JournalError::HeaderMismatch);
        }
        let metadata = scan_bounded_committed_metadata(&mut validation_file, file_len)?;
        if let Some(expected_prefix_sha256) = planned_terminal_prefix_sha256 {
            match metadata.terminal_grammar.phase {
                TerminalJournalPhase::Open if metadata.orphan_bundle_count == 0 => {}
                TerminalJournalPhase::PrefixAwaitingCheckpoint
                    if metadata.orphan_bundle_count == 1
                        && metadata.terminal_prefix_payload_sha256
                            == Some(expected_prefix_sha256) => {}
                _ => {
                    return Err(JournalError::RecoveryRequired(
                        "journal is not an open base or the exact planned TerminalPrefix orphan"
                            .into(),
                    ));
                }
            }
        } else if metadata.orphan_bundle_count != 0 {
            return Err(JournalError::RecoveryRequired(format!(
                "journal exposes {} valid bundle(s) beyond its last checkpoint marker",
                metadata.orphan_bundle_count
            )));
        }

        let boundary = if through_terminal_prefix {
            metadata.terminal_prefix_boundary.ok_or_else(|| {
                JournalError::Codec("journal has no checkpointed TerminalPrefix authority".into())
            })?
        } else {
            BoundedPrefixBoundary {
                committed_end: metadata.committed_end,
                highest_protected_ordinal: metadata.highest_protected_ordinal,
                total_committed_ordinals: metadata.total_committed_ordinals,
                committed_entry_count: metadata.committed_entry_count,
            }
        };

        let mut file = File::open(&self.path)?;
        let replay_header = read_header(&mut file)?;
        if replay_header != header {
            return Err(JournalError::HeaderMismatch);
        }
        Ok(FileTapeFileJournalCommittedSnapshot {
            path: self.path.clone(),
            _file: file,
            header,
            committed_end: boundary.committed_end,
            highest_protected_ordinal: boundary.highest_protected_ordinal,
            total_committed_ordinals: boundary.total_committed_ordinals,
            committed_entry_count: boundary.committed_entry_count,
            metrics: metadata.metrics,
        })
    }

    /// Durably discard only the orphan suffix that a higher recovery layer has
    /// already matched against physical tape and checkpoint authority.
    ///
    /// The exact orphan bundles are a compare-and-truncate guard: if journal
    /// evidence changed after reconciliation, nothing is removed. Callers must
    /// not invoke this merely because orphans exist.
    pub fn truncate_reconciled_orphans(
        &mut self,
        expected_orphans: &[CommittedBundle],
    ) -> Result<CommittedState, JournalError> {
        if expected_orphans.is_empty() {
            return Err(JournalError::Codec(
                "reconciled orphan truncation requires a non-empty expected suffix".into(),
            ));
        }
        let file_len = self.file.metadata()?.len();
        let header = read_header(&mut self.file)?;
        if header.tape_uuid != self.tape_uuid
            || header.block_size != self.block_size
            || header.drive_compression != self.drive_compression
            || header.scheme != self.scheme
        {
            return Err(JournalError::HeaderMismatch);
        }
        let replay = load_committed_replay_from_reader(&mut self.file, file_len)?;
        if replay.state.orphaned_bundles != expected_orphans {
            return Err(JournalError::RecoveryRequired(
                "orphan journal suffix changed after physical reconciliation; refusing truncation"
                    .into(),
            ));
        }
        self.file.set_len(replay.retained_end)?;
        self.file.seek(SeekFrom::Start(replay.retained_end))?;
        self.file.sync_all()?;
        self.last_highest_protected_ordinal = replay.state.highest_protected_ordinal;
        self.last_total_committed_ordinals = replay.state.total_committed_ordinals;
        self.orphaned_bundles_preserved_on_open = 0;
        let mut replay_file = self.file.try_clone()?;
        let header = read_header(&mut replay_file)?;
        if header.tape_uuid != self.tape_uuid {
            return Err(JournalError::HeaderMismatch);
        }
        self.terminal_grammar =
            load_committed_replay_from_reader(&mut replay_file, replay.retained_end)?
                .terminal_grammar;
        let mut state = replay.state;
        state.orphaned_bundles.clear();
        Ok(state)
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
        load_committed_from_reader(&mut file, file_len)
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
        if self.orphaned_bundles_preserved_on_open != 0 {
            return Err(JournalError::Codec(format!(
                "journal contains {} preserved orphan bundle(s); reconcile physical tail and checkpoint authority before append",
                self.orphaned_bundles_preserved_on_open
            )));
        }
        validate_committed_bundle_shape(bundle)?;
        validate_commit_watermarks(
            bundle,
            self.last_highest_protected_ordinal,
            self.last_total_committed_ordinals,
        )?;
        let mut terminal_grammar = self.terminal_grammar;
        terminal_grammar.observe(bundle)?;
        let payload = encode_bundle(bundle)?;
        append_journal_record_with_rollback(&mut *self.file, &payload)?;
        self.last_highest_protected_ordinal = bundle.highest_protected_ordinal;
        self.last_total_committed_ordinals = bundle.total_committed_ordinals;
        self.terminal_grammar = terminal_grammar;
        if self.first_create {
            if let Some(parent) = self.path.parent() {
                sync_directory(parent)?;
            }
            self.first_create = false;
        }
        Ok(())
    }

    fn commit_terminal_prefix_transition(
        &mut self,
        prefix: &CommittedBundle,
        checkpoint: &CommittedBundle,
    ) -> Result<(), JournalError> {
        if prefix.kind != CommittedBundleKind::TerminalPrefix
            || checkpoint.kind != CommittedBundleKind::CheckpointedThrough
            || !checkpoint.entries.is_empty()
            || prefix.highest_protected_ordinal != checkpoint.highest_protected_ordinal
            || prefix.total_committed_ordinals != checkpoint.total_committed_ordinals
        {
            return Err(JournalError::Codec(
                "invalid terminal-prefix/checkpoint transition".into(),
            ));
        }
        let mut replay_file = self.file.try_clone()?;
        let file_len = replay_file.metadata()?.len();
        read_header(&mut replay_file)?;
        let replay = scan_bounded_committed_metadata(&mut replay_file, file_len)?;
        match replay.terminal_grammar.phase {
            TerminalJournalPhase::Open => {
                if self.orphaned_bundles_preserved_on_open != 0 {
                    return Err(JournalError::Codec(
                        "non-terminal orphan evidence precedes terminal prefix".into(),
                    ));
                }
                self.commit_bundle(prefix)?;
                self.commit_bundle(checkpoint)
            }
            TerminalJournalPhase::PrefixAwaitingCheckpoint => {
                if replay.last_bundle.as_ref() != Some(prefix) {
                    return Err(JournalError::Codec(
                        "physical terminal prefix conflicts with journal orphan".into(),
                    ));
                }
                let preserved = self.orphaned_bundles_preserved_on_open;
                self.orphaned_bundles_preserved_on_open = 0;
                let result = self.commit_bundle(checkpoint);
                if result.is_err() {
                    self.orphaned_bundles_preserved_on_open = preserved;
                }
                result
            }
            TerminalJournalPhase::Ready { component_count: 0 } => {
                let exact = replay.penultimate_bundle.as_ref() == Some(prefix)
                    && replay.last_bundle.as_ref() == Some(checkpoint);
                if !exact {
                    return Err(JournalError::Codec(
                        "checkpointed terminal prefix conflicts with requested plan".into(),
                    ));
                }
                self.orphaned_bundles_preserved_on_open = 0;
                Ok(())
            }
            phase => Err(JournalError::Codec(format!(
                "terminal prefix transition is illegal in journal phase {phase:?}"
            ))),
        }
    }

    fn commit_terminal_component_transition(
        &mut self,
        component: &CommittedBundle,
        checkpoint: &CommittedBundle,
    ) -> Result<(), JournalError> {
        if component.kind != CommittedBundleKind::TerminalComponent
            || checkpoint.kind != CommittedBundleKind::CheckpointedThrough
            || !checkpoint.entries.is_empty()
            || component.highest_protected_ordinal != checkpoint.highest_protected_ordinal
            || component.total_committed_ordinals != checkpoint.total_committed_ordinals
        {
            return Err(JournalError::Codec(
                "invalid terminal-component/checkpoint transition".into(),
            ));
        }
        let mut replay_file = self.file.try_clone()?;
        let file_len = replay_file.metadata()?.len();
        read_header(&mut replay_file)?;
        let replay = scan_bounded_committed_metadata(&mut replay_file, file_len)?;
        let exact_checkpointed = replay.penultimate_bundle.as_ref() == Some(component)
            && replay.last_bundle.as_ref() == Some(checkpoint);
        if exact_checkpointed
            && matches!(
                replay.terminal_grammar.phase,
                TerminalJournalPhase::Ready {
                    component_count: 1..=4
                } | TerminalJournalPhase::Complete
            )
        {
            self.orphaned_bundles_preserved_on_open = 0;
            return Ok(());
        }
        match replay.terminal_grammar.phase {
            TerminalJournalPhase::Ready { .. } => {
                if self.orphaned_bundles_preserved_on_open != 0 {
                    return Err(JournalError::Codec(
                        "orphan evidence precedes terminal component".into(),
                    ));
                }
                self.commit_bundle(component)?;
                self.commit_bundle(checkpoint)
            }
            TerminalJournalPhase::ComponentAwaitingCheckpoint { .. } => {
                if replay.last_bundle.as_ref() != Some(component) {
                    return Err(JournalError::Codec(
                        "physical terminal component conflicts with journal orphan".into(),
                    ));
                }
                let preserved = self.orphaned_bundles_preserved_on_open;
                self.orphaned_bundles_preserved_on_open = 0;
                let result = self.commit_bundle(checkpoint);
                if result.is_err() {
                    self.orphaned_bundles_preserved_on_open = preserved;
                }
                result
            }
            phase => Err(JournalError::Codec(format!(
                "terminal component transition is illegal in journal phase {phase:?}"
            ))),
        }
    }

    fn load_committed(&self) -> Result<CommittedState, JournalError> {
        // Replay is non-destructive. Torn or corrupt tails require explicit
        // reconciliation and are never erased merely by opening an append
        // handle.
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
        load_committed_from_reader(&mut file, file_len)
    }

    fn committed_snapshot_bounded_authority(
        &self,
    ) -> Result<FileTapeFileJournalCommittedSnapshot, JournalError> {
        self.committed_snapshot_bounded()
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
) -> Result<CommittedState, JournalError> {
    Ok(load_committed_replay_from_reader(file, file_len)?.state)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BoundedCommittedMetadata {
    committed_end: u64,
    highest_protected_ordinal: u64,
    total_committed_ordinals: u64,
    committed_entry_count: u64,
    orphan_bundle_count: u64,
    terminal_grammar: TerminalJournalGrammar,
    penultimate_bundle: Option<CommittedBundle>,
    last_bundle: Option<CommittedBundle>,
    metrics: BoundedJournalReplayMetrics,
    terminal_prefix_payload_sha256: Option<[u8; 32]>,
    terminal_prefix_checkpoint_sha256: Option<[u8; 32]>,
    terminal_prefix_boundary: Option<BoundedPrefixBoundary>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BoundedPrefixBoundary {
    committed_end: u64,
    highest_protected_ordinal: u64,
    total_committed_ordinals: u64,
    committed_entry_count: u64,
}

fn scan_bounded_committed_metadata(
    file: &mut File,
    file_len: u64,
) -> Result<BoundedCommittedMetadata, JournalError> {
    let header_end = file.stream_position()?;
    let mut replay_highest_protected_ordinal = 0;
    let mut replay_total_committed_ordinals = 0;
    let mut scanned_entry_count = 0u64;
    let mut terminal_grammar = TerminalJournalGrammar::default();
    let mut metadata = BoundedCommittedMetadata {
        committed_end: header_end,
        highest_protected_ordinal: 0,
        total_committed_ordinals: 0,
        committed_entry_count: 0,
        orphan_bundle_count: 0,
        terminal_grammar,
        penultimate_bundle: None,
        last_bundle: None,
        metrics: BoundedJournalReplayMetrics {
            validation_passes: 1,
            ..BoundedJournalReplayMetrics::default()
        },
        terminal_prefix_payload_sha256: None,
        terminal_prefix_checkpoint_sha256: None,
        terminal_prefix_boundary: None,
    };
    while file.stream_position()? < file_len {
        // Once another record exists, the older of the retained final
        // candidates cannot be one of the final two. Drop it before decoding
        // the next length-bounded payload so peak live rows remain bounded by
        // the previous bundle plus the current bundle.
        metadata.penultimate_bundle = None;
        let frame = read_validated_journal_bundle(file, file_len)?.ok_or_else(|| {
            JournalError::RecoveryRequired(
                "journal replay ended before the validated file boundary".into(),
            )
        })?;
        let bundle = frame.bundle;
        metadata.metrics.journal_record_count = metadata
            .metrics
            .journal_record_count
            .checked_add(1)
            .ok_or_else(|| JournalError::Codec("journal record count overflows u64".into()))?;
        metadata.metrics.peak_record_payload_bytes = metadata
            .metrics
            .peak_record_payload_bytes
            .max(frame.payload_len);
        let current_entry_count = u64::try_from(bundle.entries.len())
            .map_err(|_| JournalError::Codec("bundle entry count exceeds u64".into()))?;
        let prior_entry_count = metadata.last_bundle.as_ref().map_or(Ok(0), |prior| {
            u64::try_from(prior.entries.len())
                .map_err(|_| JournalError::Codec("bundle entry count exceeds u64".into()))
        })?;
        metadata.metrics.peak_live_entry_count = metadata.metrics.peak_live_entry_count.max(
            prior_entry_count
                .checked_add(current_entry_count)
                .ok_or_else(|| {
                    JournalError::Codec("peak live journal entry count overflows u64".into())
                })?,
        );
        validate_commit_watermarks(
            &bundle,
            replay_highest_protected_ordinal,
            replay_total_committed_ordinals,
        )?;
        let prior_terminal_phase = terminal_grammar.phase;
        terminal_grammar.observe(&bundle)?;
        if bundle.kind == CommittedBundleKind::TerminalPrefix {
            metadata.terminal_prefix_payload_sha256 = Some(frame.payload_sha256);
        } else if prior_terminal_phase == TerminalJournalPhase::PrefixAwaitingCheckpoint
            && bundle.kind == CommittedBundleKind::CheckpointedThrough
        {
            metadata.terminal_prefix_checkpoint_sha256 = Some(frame.payload_sha256);
            metadata.terminal_prefix_boundary = Some(BoundedPrefixBoundary {
                committed_end: file.stream_position()?,
                highest_protected_ordinal: bundle.highest_protected_ordinal,
                total_committed_ordinals: bundle.total_committed_ordinals,
                committed_entry_count: scanned_entry_count
                    .checked_add(current_entry_count)
                    .ok_or_else(|| {
                        JournalError::Codec(
                            "terminal prefix committed entry count overflows u64".into(),
                        )
                    })?,
            });
        }
        replay_highest_protected_ordinal = bundle.highest_protected_ordinal;
        replay_total_committed_ordinals = bundle.total_committed_ordinals;
        scanned_entry_count = scanned_entry_count
            .checked_add(current_entry_count)
            .ok_or_else(|| JournalError::Codec("committed entry count overflows u64".into()))?;
        metadata.orphan_bundle_count = metadata
            .orphan_bundle_count
            .checked_add(1)
            .ok_or_else(|| JournalError::Codec("orphan bundle count overflows u64".into()))?;
        if bundle.kind == CommittedBundleKind::CheckpointedThrough {
            metadata.committed_end = file.stream_position()?;
            metadata.highest_protected_ordinal = replay_highest_protected_ordinal;
            metadata.total_committed_ordinals = replay_total_committed_ordinals;
            metadata.committed_entry_count = scanned_entry_count;
            metadata.orphan_bundle_count = 0;
        }
        metadata.penultimate_bundle = metadata.last_bundle.take();
        metadata.last_bundle = Some(bundle);
    }
    metadata.terminal_grammar = terminal_grammar;
    Ok(metadata)
}

fn read_validated_journal_bundle(
    file: &mut File,
    replay_end: u64,
) -> Result<Option<ValidatedJournalBundle>, JournalError> {
    let record_start = file.stream_position()?;
    if record_start == replay_end {
        return Ok(None);
    }
    if record_start > replay_end {
        return Err(JournalError::RecoveryRequired(format!(
            "journal replay position {record_start} exceeds validated end {replay_end}"
        )));
    }
    let available = replay_end.checked_sub(record_start).ok_or_else(|| {
        JournalError::RecoveryRequired("journal replay boundary underflowed".into())
    })?;
    if available < 12 {
        return Err(JournalError::RecoveryRequired(format!(
            "torn record-length prefix at offset {record_start}"
        )));
    }
    let mut len_buf = [0u8; 4];
    file.read_exact(&mut len_buf)?;
    let record_len = u64::from(u32::from_le_bytes(len_buf));
    if record_len > MAX_RECORD_LEN {
        return Err(JournalError::RecoveryRequired(format!(
            "record at offset {record_start} declares {record_len} bytes, limit {MAX_RECORD_LEN}"
        )));
    }
    let framed_len = record_len
        .checked_add(12)
        .ok_or_else(|| JournalError::Codec("journal record frame length overflows u64".into()))?;
    if framed_len > available {
        return Err(JournalError::RecoveryRequired(format!(
            "torn record at offset {record_start}: declared frame exceeds the remaining {available} bytes"
        )));
    }
    let payload_len = usize::try_from(record_len)
        .map_err(|_| JournalError::Codec("journal record length does not fit usize".into()))?;
    let mut payload = vec![0u8; payload_len];
    file.read_exact(&mut payload)?;
    let mut crc_buf = [0u8; 8];
    file.read_exact(&mut crc_buf)?;
    let expected_crc = u64::from_le_bytes(crc_buf);
    if crc64_xz(&payload) != expected_crc {
        return Err(JournalError::RecoveryRequired(format!(
            "record checksum mismatch at offset {record_start}"
        )));
    }
    let bundle = decode_bundle(&payload)?;
    validate_committed_bundle_shape(&bundle)?;
    let payload_sha256 = Sha256::digest(&payload).into();
    Ok(Some(ValidatedJournalBundle {
        bundle,
        payload_len: record_len,
        payload_sha256,
    }))
}

#[derive(Debug)]
struct ValidatedJournalBundle {
    bundle: CommittedBundle,
    payload_len: u64,
    payload_sha256: [u8; 32],
}

#[derive(Debug)]
struct CommittedJournalReplay {
    state: CommittedState,
    retained_end: u64,
    terminal_grammar: TerminalJournalGrammar,
}

fn load_committed_replay_from_reader(
    file: &mut File,
    file_len: u64,
) -> Result<CommittedJournalReplay, JournalError> {
    let header_end = file.stream_position()?;
    let mut replay_highest_protected_ordinal = 0;
    let mut replay_total_committed_ordinals = 0;
    let mut records = Vec::new();
    let mut terminal_grammar = TerminalJournalGrammar::default();
    loop {
        let record_start = file.stream_position()?;
        if record_start == file_len {
            break;
        }
        let mut len_buf = [0u8; 4];
        match file.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(JournalError::RecoveryRequired(format!(
                    "torn record-length prefix at offset {record_start}"
                )));
            }
            Err(err) => return Err(JournalError::Io(err)),
        }
        let record_len = u64::from(u32::from_le_bytes(len_buf));
        let available = file_len.saturating_sub(record_start).saturating_sub(4);
        if record_len > MAX_RECORD_LEN {
            return Err(JournalError::RecoveryRequired(format!(
                "record at offset {record_start} declares {record_len} bytes, limit {MAX_RECORD_LEN}"
            )));
        }
        if record_len.saturating_add(8) > available {
            return Err(JournalError::RecoveryRequired(format!(
                "torn record at offset {record_start}: declared payload and checksum exceed the remaining {available} bytes"
            )));
        }
        let record_len = usize::try_from(record_len)
            .map_err(|_| JournalError::Codec("journal record length does not fit usize".into()))?;
        let mut payload = vec![0u8; record_len];
        if let Err(err) = file.read_exact(&mut payload) {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                return Err(JournalError::RecoveryRequired(format!(
                    "torn payload at offset {record_start}"
                )));
            }
            return Err(JournalError::Io(err));
        }
        let mut crc_buf = [0u8; 8];
        if let Err(err) = file.read_exact(&mut crc_buf) {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                return Err(JournalError::RecoveryRequired(format!(
                    "torn checksum at offset {record_start}"
                )));
            }
            return Err(JournalError::Io(err));
        }
        let expected_crc = u64::from_le_bytes(crc_buf);
        if crc64_xz(&payload) != expected_crc {
            return Err(JournalError::RecoveryRequired(format!(
                "record checksum mismatch at offset {record_start}"
            )));
        }
        let bundle = decode_bundle(&payload)?;
        validate_committed_bundle_shape(&bundle)?;
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
        terminal_grammar.observe(&bundle)?;
        replay_highest_protected_ordinal = bundle.highest_protected_ordinal;
        replay_total_committed_ordinals = bundle.total_committed_ordinals;
        records.push((bundle, file.stream_position()?));
    }

    let last_checkpoint_index = records
        .iter()
        .rposition(|(bundle, _)| bundle.kind == CommittedBundleKind::CheckpointedThrough);
    let retained_record_count = last_checkpoint_index.map_or(0, |index| index + 1);
    let retained_end = last_checkpoint_index.map_or(header_end, |index| records[index].1);
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

    Ok(CommittedJournalReplay {
        state: CommittedState {
            entries,
            highest_protected_ordinal,
            total_committed_ordinals,
            orphaned_bundles,
        },
        retained_end,
        terminal_grammar,
    })
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

/// Require a local block-backed volume whose filesystem and write-cache
/// behavior are suitable for an fsync-based recovery authority.
///
/// Layer 4 checkpoint journals call the same policy so the two authorities do
/// not silently have different durability assumptions.
pub fn validate_trusted_journal_volume(path: &Path) -> Result<(), JournalError> {
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

fn encoded_bundle_sha256(bundle: &CommittedBundle) -> Result<[u8; 32], JournalError> {
    Ok(Sha256::digest(encode_bundle(bundle)?).into())
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
    let mut key_order = IntegerMapKeyTracker::default();
    for (key, value) in map {
        let key = key_order.next(key, "bundle").map_err(JournalError::Codec)?;
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
    let object_recovery_row = match entry.object_recovery_row.as_ref() {
        Some(row) => Some(
            encode_object_recovery_row_cbor(row)
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
            object_recovery_row.unwrap_or(CborValue::Null),
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
    let mut object_recovery_row = None;
    let mut key_order = IntegerMapKeyTracker::default();
    for (key, value) in map {
        let key = key_order
            .next(key, "journal entry")
            .map_err(JournalError::Codec)?;
        match (key, value) {
            (1, CborValue::Integer(value)) => {
                tape_file_number = Some(cbor_u64(value, "tape_file_number")?)
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
                let row = decode_object_recovery_row_cbor(value, None)
                    .map_err(|err| JournalError::Codec(err.to_string()))?;
                validate_object_recovery_row(&row, None)
                    .map_err(|err| JournalError::Codec(err.to_string()))?;
                object_recovery_row = Some(row);
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
        object_recovery_row,
    })
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
        CommittedBundleKind::BotBootstrap => 1,
        CommittedBundleKind::ResumeSidecars => 2,
        CommittedBundleKind::CheckpointSidecars => 3,
        CommittedBundleKind::CheckpointedThrough => 4,
        CommittedBundleKind::TerminalPrefix => 5,
        CommittedBundleKind::TerminalComponent => 6,
    }
}

fn kind_from_code(value: u64) -> Result<CommittedBundleKind, JournalError> {
    match value {
        0 => Ok(CommittedBundleKind::Object),
        1 => Ok(CommittedBundleKind::BotBootstrap),
        2 => Ok(CommittedBundleKind::ResumeSidecars),
        3 => Ok(CommittedBundleKind::CheckpointSidecars),
        4 => Ok(CommittedBundleKind::CheckpointedThrough),
        5 => Ok(CommittedBundleKind::TerminalPrefix),
        6 => Ok(CommittedBundleKind::TerminalComponent),
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
        TapeFileKind::TapeIndexReplica => 4,
        TapeFileKind::IndexSeparationExtent => 5,
    }
}

fn tape_file_kind_from_code(value: u64) -> Result<TapeFileKind, JournalError> {
    match value {
        0 => Ok(TapeFileKind::Object),
        1 => Ok(TapeFileKind::ParitySidecar),
        2 => Ok(TapeFileKind::Bootstrap),
        3 => Ok(TapeFileKind::ParityMap),
        4 => Ok(TapeFileKind::TapeIndexReplica),
        5 => Ok(TapeFileKind::IndexSeparationExtent),
        _ => Err(JournalError::Codec(format!(
            "unknown tape-file kind code {value}"
        ))),
    }
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

    struct UnboundedTestJournal;

    impl TapeFileJournal for UnboundedTestJournal {
        fn tape_uuid(&self) -> [u8; 16] {
            [0; 16]
        }

        fn commit_bundle(&mut self, _bundle: &CommittedBundle) -> Result<(), JournalError> {
            Ok(())
        }

        fn load_committed(&self) -> Result<CommittedState, JournalError> {
            Ok(CommittedState {
                entries: Vec::new(),
                highest_protected_ordinal: 0,
                total_committed_ordinals: 0,
                orphaned_bundles: Vec::new(),
            })
        }
    }

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
                    entry.object_recovery_row = Some(
                        ObjectRecoveryRow::encrypted(0, 3, vec![[0x24; 16], [0x25; 16]], 66, 2377)
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

    fn structural_entry(tape_file_number: u64, kind: TapeFileKind) -> TapeFileEntry {
        TapeFileEntry {
            tape_file_number,
            kind,
            block_count: if kind == TapeFileKind::Bootstrap {
                1
            } else {
                2
            },
            physical_start_hint: None,
            object_id: (kind == TapeFileKind::Object).then(|| "object-1".to_string()),
            first_parity_data_ordinal: (kind == TapeFileKind::Object).then_some(0),
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            object_recovery_row: None,
        }
    }

    fn structural_bundle(
        kind: CommittedBundleKind,
        entry_kinds: &[TapeFileKind],
    ) -> CommittedBundle {
        CommittedBundle {
            kind,
            entries: entry_kinds
                .iter()
                .enumerate()
                .map(|(number, kind)| {
                    structural_entry(u64::try_from(number).expect("test index fits u64"), *kind)
                })
                .collect(),
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
        }
    }

    #[test]
    fn shared_bundle_validator_accepts_every_current_wire_shape() {
        let accepted = [
            structural_bundle(CommittedBundleKind::Object, &[TapeFileKind::Object]),
            structural_bundle(
                CommittedBundleKind::Object,
                &[TapeFileKind::Object, TapeFileKind::ParitySidecar],
            ),
            structural_bundle(
                CommittedBundleKind::BotBootstrap,
                &[TapeFileKind::Bootstrap],
            ),
            structural_bundle(
                CommittedBundleKind::CheckpointSidecars,
                &[TapeFileKind::ParitySidecar],
            ),
            structural_bundle(
                CommittedBundleKind::ResumeSidecars,
                &[TapeFileKind::ParitySidecar, TapeFileKind::ParitySidecar],
            ),
            structural_bundle(
                CommittedBundleKind::TerminalPrefix,
                &[TapeFileKind::ParitySidecar, TapeFileKind::ParityMap],
            ),
            structural_bundle(CommittedBundleKind::TerminalPrefix, &[]),
            structural_bundle(
                CommittedBundleKind::TerminalComponent,
                &[TapeFileKind::TapeIndexReplica],
            ),
            structural_bundle(
                CommittedBundleKind::TerminalComponent,
                &[TapeFileKind::IndexSeparationExtent],
            ),
            structural_bundle(CommittedBundleKind::CheckpointedThrough, &[]),
        ];

        for bundle in accepted {
            validate_committed_bundle_shape(&bundle)
                .unwrap_or_else(|err| panic!("valid {:?} shape rejected: {err}", bundle.kind));
        }
    }

    #[test]
    fn shared_bundle_validator_rejects_ambiguous_or_non_dense_shapes() {
        let mut non_dense = structural_bundle(
            CommittedBundleKind::CheckpointSidecars,
            &[TapeFileKind::ParitySidecar, TapeFileKind::ParitySidecar],
        );
        non_dense.entries[1].tape_file_number = 9;
        let mut multi_block_bootstrap = structural_bundle(
            CommittedBundleKind::BotBootstrap,
            &[TapeFileKind::Bootstrap],
        );
        multi_block_bootstrap.entries[0].block_count = 2;
        let mut non_bot_bootstrap = structural_bundle(
            CommittedBundleKind::BotBootstrap,
            &[TapeFileKind::Bootstrap],
        );
        non_bot_bootstrap.entries[0].tape_file_number = 1;
        let mut empty_terminal = structural_bundle(
            CommittedBundleKind::TerminalComponent,
            &[TapeFileKind::TapeIndexReplica],
        );
        empty_terminal.entries[0].block_count = 0;
        let rejected = [
            structural_bundle(CommittedBundleKind::BotBootstrap, &[]),
            structural_bundle(CommittedBundleKind::CheckpointSidecars, &[]),
            structural_bundle(CommittedBundleKind::ResumeSidecars, &[]),
            structural_bundle(
                CommittedBundleKind::Object,
                &[TapeFileKind::Bootstrap, TapeFileKind::Object],
            ),
            structural_bundle(
                CommittedBundleKind::Object,
                &[TapeFileKind::Object, TapeFileKind::ParityMap],
            ),
            structural_bundle(
                CommittedBundleKind::CheckpointSidecars,
                &[TapeFileKind::ParitySidecar, TapeFileKind::Bootstrap],
            ),
            structural_bundle(
                CommittedBundleKind::TerminalComponent,
                &[
                    TapeFileKind::TapeIndexReplica,
                    TapeFileKind::IndexSeparationExtent,
                ],
            ),
            structural_bundle(
                CommittedBundleKind::Object,
                &[TapeFileKind::Object, TapeFileKind::TapeIndexReplica],
            ),
            non_dense,
            multi_block_bootstrap,
            non_bot_bootstrap,
            empty_terminal,
        ];

        for bundle in rejected {
            assert!(
                validate_committed_bundle_shape(&bundle).is_err(),
                "invalid {:?} shape accepted",
                bundle.kind
            );
        }
    }
    fn transpose_first_two_cbor_map_entries(value: CborValue) -> CborValue {
        let CborValue::Map(mut entries) = value else {
            panic!("test value must be a CBOR map");
        };
        assert!(entries.len() >= 2, "test map needs two entries");
        entries.swap(0, 1);
        CborValue::Map(entries)
    }

    fn append_unknown_cbor_map_key(value: CborValue) -> CborValue {
        let CborValue::Map(mut entries) = value else {
            panic!("test value must be a CBOR map");
        };
        entries.push((CborValue::Integer(99.into()), CborValue::Null));
        CborValue::Map(entries)
    }

    fn decode_bundle_value(value: &CborValue) -> Result<CommittedBundle, JournalError> {
        let mut bytes = Vec::new();
        ciborium::into_writer(value, &mut bytes).expect("test CBOR value encodes");
        decode_bundle(&bytes)
    }

    fn assert_journal_key_order_error(error: JournalError, key: i128, previous: i128) {
        let JournalError::Codec(message) = error else {
            panic!("expected journal codec error, got {error:?}");
        };
        assert!(message.contains(&format!("key {key}")), "{message}");
        assert!(
            message.contains(&format!("after key {previous}")),
            "{message}"
        );
    }

    fn commit_sample_checkpoint(journal: &mut FileTapeFileJournal) {
        journal
            .commit_bundle(&sample_bundle())
            .expect("commit sample bundle");
        journal
            .commit_bundle(&sample_checkpoint())
            .expect("commit checkpoint watermark");
    }

    #[test]
    fn journal_bundle_map_enforces_order_and_ignores_ordered_unknown_key() {
        let bundle = sample_bundle();
        let canonical_bytes = encode_bundle(&bundle).expect("bundle encodes");
        let canonical: CborValue =
            ciborium::from_reader(canonical_bytes.as_slice()).expect("bundle value decodes");

        assert_eq!(
            decode_bundle_value(&canonical).expect("canonical bundle map decodes"),
            bundle
        );

        let transposed = transpose_first_two_cbor_map_entries(canonical.clone());
        let error =
            decode_bundle_value(&transposed).expect_err("transposed bundle keys must reject");
        assert_journal_key_order_error(error, 1, 2);

        let extended = append_unknown_cbor_map_key(canonical);
        assert_eq!(
            decode_bundle_value(&extended).expect("ordered unknown bundle key is ignored"),
            bundle
        );
    }

    #[test]
    fn journal_entry_map_enforces_order_and_ignores_ordered_unknown_key() {
        let entry = sample_bundle().entries.remove(0);
        let canonical = encode_entry(&entry).expect("journal entry encodes");

        assert_eq!(
            decode_entry(canonical.clone()).expect("canonical journal entry decodes"),
            entry
        );

        let transposed = transpose_first_two_cbor_map_entries(canonical.clone());
        let error = decode_entry(transposed).expect_err("transposed entry keys must reject");
        assert_journal_key_order_error(error, 1, 2);

        let extended = append_unknown_cbor_map_key(canonical);
        assert_eq!(
            decode_entry(extended).expect("ordered unknown journal-entry key is ignored"),
            entry
        );
    }

    #[test]
    fn journal_entry_round_trips_u64_file_numbers_and_terminal_kinds() {
        assert_eq!(
            tape_file_kind_code(TapeFileKind::TapeIndexReplica),
            4,
            "journal structural kind 4 is stable"
        );
        assert_eq!(
            tape_file_kind_code(TapeFileKind::IndexSeparationExtent),
            5,
            "journal structural kind 5 is stable"
        );
        assert_eq!(
            tape_file_kind_from_code(4).expect("kind 4 decodes"),
            TapeFileKind::TapeIndexReplica
        );
        assert_eq!(
            tape_file_kind_from_code(5).expect("kind 5 decodes"),
            TapeFileKind::IndexSeparationExtent
        );
        let boundaries = [
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 1,
            (1u64 << 53) + 1,
            i64::MAX as u64 + 1,
            u64::MAX,
        ];
        for (index, tape_file_number) in boundaries.into_iter().enumerate() {
            let kind = if index % 2 == 0 {
                TapeFileKind::TapeIndexReplica
            } else {
                TapeFileKind::IndexSeparationExtent
            };
            let entry = structural_entry(tape_file_number, kind);
            let encoded = encode_entry(&entry).expect("u64 journal entry encodes");
            assert_eq!(
                decode_entry(encoded).expect("u64 journal entry decodes"),
                entry
            );
        }
    }

    #[test]
    fn journal_entry_rejects_negative_file_numbers_and_unknown_structural_kinds() {
        let mut negative = encode_entry(&structural_entry(0, TapeFileKind::TapeIndexReplica))
            .expect("fixture entry encodes");
        let CborValue::Map(ref mut fields) = negative else {
            panic!("entry fixture must be a map");
        };
        fields[0].1 = CborValue::Integer((-1i64).into());
        let error = decode_entry(negative).expect_err("negative tape-file number must reject");
        assert!(
            matches!(error, JournalError::Codec(message) if message.contains("out of u64 range"))
        );

        let mut unknown_kind = encode_entry(&structural_entry(0, TapeFileKind::TapeIndexReplica))
            .expect("fixture entry encodes");
        let CborValue::Map(ref mut fields) = unknown_kind else {
            panic!("entry fixture must be a map");
        };
        fields[1].1 = CborValue::Integer(6.into());
        let error = decode_entry(unknown_kind).expect_err("unknown structural kind must reject");
        assert!(
            matches!(error, JournalError::Codec(message) if message.contains("unknown tape-file kind code 6"))
        );

        let overflow = CommittedBundle {
            kind: CommittedBundleKind::Object,
            entries: vec![
                structural_entry(u64::MAX, TapeFileKind::Object),
                structural_entry(0, TapeFileKind::ParitySidecar),
            ],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
        };
        let error = validate_committed_bundle_shape(&overflow)
            .expect_err("dense tape-file increment past u64::MAX must reject");
        assert!(error.to_string().contains("overflows u64"), "{error}");
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
            journal
                .commit_bundle(&structural_bundle(
                    CommittedBundleKind::BotBootstrap,
                    &[TapeFileKind::Bootstrap],
                ))
                .expect("commit BOT Bootstrap");
            let mut body = sample_bundle();
            for entry in &mut body.entries {
                entry.tape_file_number += 1;
                if let Some(row) = &mut entry.object_recovery_row {
                    row.tape_file_number += 1;
                }
            }
            journal.commit_bundle(&body).expect("commit sample body");
            journal
                .commit_bundle(&sample_checkpoint())
                .expect("commit sample checkpoint");
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
        assert_eq!(state.entries[1].kind, TapeFileKind::Object);
        assert_eq!(state.entries[2].kind, TapeFileKind::ParitySidecar);
        let map = state.filemark_map().expect("journal map validates");
        assert_eq!(map.entries().len(), 3);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_journal_appends_and_replays_terminal_index_ladder_components() {
        let path = temp_journal_path("terminal-index-ladder");
        let tape_uuid = [0x43; 16];
        let scheme = default_scheme();
        let kinds = [
            TapeFileKind::TapeIndexReplica,
            TapeFileKind::IndexSeparationExtent,
            TapeFileKind::TapeIndexReplica,
            TapeFileKind::IndexSeparationExtent,
            TapeFileKind::TapeIndexReplica,
        ];
        {
            let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("open journal");
            journal
                .commit_bundle(&structural_bundle(
                    CommittedBundleKind::BotBootstrap,
                    &[TapeFileKind::Bootstrap],
                ))
                .expect("commit BOT Bootstrap");
            journal
                .commit_bundle(&CommittedBundle {
                    kind: CommittedBundleKind::TerminalPrefix,
                    entries: Vec::new(),
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 0,
                })
                .expect("commit terminal prefix");
            journal
                .commit_bundle(&CommittedBundle {
                    kind: CommittedBundleKind::CheckpointedThrough,
                    entries: Vec::new(),
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 0,
                })
                .expect("checkpoint terminal prefix");
            for (number, entry_kind) in kinds.into_iter().enumerate() {
                journal
                    .commit_bundle(&CommittedBundle {
                        kind: CommittedBundleKind::TerminalComponent,
                        entries: vec![structural_entry(
                            u64::try_from(number + 1).expect("test index fits u64"),
                            entry_kind,
                        )],
                        highest_protected_ordinal: 0,
                        total_committed_ordinals: 0,
                    })
                    .expect("commit terminal component");
                journal
                    .commit_bundle(&CommittedBundle {
                        kind: CommittedBundleKind::CheckpointedThrough,
                        entries: Vec::new(),
                        highest_protected_ordinal: 0,
                        total_committed_ordinals: 0,
                    })
                    .expect("commit checkpoint watermark");
            }
        }

        let state = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("reopen journal")
        .load_committed()
        .expect("replay terminal components");
        assert_eq!(state.entries.len(), kinds.len() + 1);
        assert_eq!(state.entries[0].kind, TapeFileKind::Bootstrap);
        for (number, (entry, expected_kind)) in state.entries[1..].iter().zip(kinds).enumerate() {
            assert_eq!(entry.kind, expected_kind);
            assert_eq!(entry.tape_file_number, number as u64 + 1);
        }
        state
            .filemark_map()
            .expect("replayed terminal map validates");

        let _ = fs::remove_file(path);
    }

    #[test]
    fn terminal_prefix_transition_reconciles_orphan_and_checkpointed_restart() {
        let path = temp_journal_path("terminal-prefix-idempotency");
        let tape_uuid = [0x49; 16];
        let scheme = default_scheme();
        let prefix = CommittedBundle {
            kind: CommittedBundleKind::TerminalPrefix,
            entries: Vec::new(),
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
        };
        let checkpoint = CommittedBundle {
            kind: CommittedBundleKind::CheckpointedThrough,
            entries: Vec::new(),
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
        };
        let component = structural_bundle(
            CommittedBundleKind::TerminalComponent,
            &[TapeFileKind::TapeIndexReplica],
        );
        {
            let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("open journal");
            assert!(!journal
                .terminal_prefix_transition_is_durable(&prefix, &checkpoint)
                .expect("open-phase probe"));
            journal
                .commit_bundle(&prefix)
                .expect("prefix orphan fsyncs");
        }
        {
            let mut reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("reopen prefix orphan");
            assert_eq!(reopened.orphaned_bundles_preserved_on_open(), 1);
            assert!(!reopened
                .terminal_prefix_transition_is_durable(&prefix, &checkpoint)
                .expect("orphan-prefix probe"));
            reopened
                .commit_terminal_prefix_transition(&prefix, &checkpoint)
                .expect("exact orphan gains checkpoint");
        }
        {
            let mut reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("reopen checkpointed prefix");
            reopened
                .commit_terminal_prefix_transition(&prefix, &checkpoint)
                .expect("checkpointed prefix is idempotent");
            assert!(reopened
                .terminal_prefix_transition_is_durable(&prefix, &checkpoint)
                .expect("checkpointed-prefix probe"));
            let state = reopened.load_committed().expect("prefix replay");
            assert!(state.orphaned_bundles.is_empty());
            assert!(state.entries.is_empty());
            reopened
                .commit_bundle(&component)
                .expect("component orphan fsyncs");
        }
        {
            let mut reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("reopen component orphan");
            reopened
                .commit_terminal_component_transition(&component, &checkpoint)
                .expect("exact component orphan gains checkpoint");
        }
        {
            let mut reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme,
            )
            .expect("reopen checkpointed component");
            reopened
                .commit_terminal_component_transition(&component, &checkpoint)
                .expect("checkpointed component is idempotent");
            assert!(reopened
                .terminal_prefix_transition_is_durable(&prefix, &checkpoint)
                .expect("prefix remains durable after component"));
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn terminal_journal_grammar_rejects_skips_duplicates_and_post_c_append() {
        let prefix = structural_bundle(CommittedBundleKind::TerminalPrefix, &[]);
        let watermark = structural_bundle(CommittedBundleKind::CheckpointedThrough, &[]);
        let replica = structural_bundle(
            CommittedBundleKind::TerminalComponent,
            &[TapeFileKind::TapeIndexReplica],
        );
        let gap = structural_bundle(
            CommittedBundleKind::TerminalComponent,
            &[TapeFileKind::IndexSeparationExtent],
        );

        let mut missing_prefix = TerminalJournalGrammar::default();
        assert!(missing_prefix.observe(&replica).is_err());

        let mut grammar = TerminalJournalGrammar::default();
        grammar.observe(&prefix).unwrap();
        assert!(grammar.observe(&replica).is_err());
        grammar.observe(&watermark).unwrap();
        for (number, template) in [&replica, &gap, &replica, &gap, &replica]
            .into_iter()
            .enumerate()
        {
            let mut component = template.clone();
            component.entries[0].tape_file_number = number as u64;
            grammar.observe(&component).unwrap();
            assert!(grammar.observe(&component).is_err());
            grammar.observe(&watermark).unwrap();
        }
        assert_eq!(grammar.phase, TerminalJournalPhase::Complete);
        assert!(grammar.observe(&replica).is_err());
        assert!(grammar.observe(&watermark).is_err());
    }

    #[test]
    fn file_journal_round_trips_populated_physical_start_hint() {
        // Block 0 is a valid position: Some(0) must survive as Some(0), and
        // a record journalled without the hint must read back as absent —
        // never zero, never defaulted.
        let path = temp_journal_path("hint-roundtrip");
        let tape_uuid = [0x45; 16];
        let scheme = default_scheme();
        let mut bundle = sample_bundle();
        bundle.entries[0].physical_start_hint = Some(0);
        bundle.entries[1].physical_start_hint = Some(4);
        {
            let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
                &path,
                tape_uuid,
                256 * 1024,
                scheme.clone(),
            )
            .expect("open journal");
            journal
                .commit_bundle(&bundle)
                .expect("commit hinted bundle");
            journal
                .commit_bundle(&sample_checkpoint())
                .expect("commit checkpoint watermark");
        }

        let state = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("reopen journal")
        .load_committed()
        .expect("load committed");
        assert_eq!(
            state.entries[0].physical_start_hint,
            Some(0),
            "block 0 is a valid captured position and must not collapse to absent"
        );
        assert_eq!(state.entries[1].physical_start_hint, Some(4));
        assert_eq!(state.entries, bundle.entries);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn pre_hint_journal_records_read_as_absent_not_zero() {
        // Records journalled before the hint was populated carry CBOR key 4
        // as Null. They must decode to None — absent, not zero.
        let legacy = sample_bundle();
        assert!(legacy
            .entries
            .iter()
            .all(|entry| entry.physical_start_hint.is_none()));
        let decoded = decode_bundle(&encode_bundle(&legacy).expect("legacy bundle encodes"))
            .expect("legacy bundle decodes");
        for entry in &decoded.entries {
            assert_eq!(
                entry.physical_start_hint, None,
                "an absent hint must never materialise as Some(0)"
            );
        }
    }

    #[test]
    fn to_map_entry_drops_hint_and_canonical_digest_is_unchanged() {
        // REM-PARITY §7.1: the span rides the journal record only. The
        // map-entry field set and the seven-element canonical digest array
        // must be byte-identical with the hint populated and absent —
        // otherwise bootstrap digests change, which is an on-tape break.
        let without_hint = sample_bundle();
        let mut with_hint = sample_bundle();
        with_hint.entries[0].physical_start_hint = Some(2);
        with_hint.entries[1].physical_start_hint = Some(9);

        for (hinted, bare) in with_hint.entries.iter().zip(without_hint.entries.iter()) {
            assert_eq!(
                hinted.to_map_entry(),
                bare.to_map_entry(),
                "to_map_entry must keep dropping the hint"
            );
        }

        let map_with_hint = FilemarkMap::new(
            std::iter::once(TapeFileMapEntry::bootstrap(0, 1))
                .chain(with_hint.entries.iter().map(|entry| {
                    let mut entry = entry.to_map_entry();
                    entry.tape_file_number += 1;
                    entry
                }))
                .collect(),
        )
        .expect("hinted entries build a valid map");
        let map_without_hint = FilemarkMap::new(
            std::iter::once(TapeFileMapEntry::bootstrap(0, 1))
                .chain(without_hint.entries.iter().map(|entry| {
                    let mut entry = entry.to_map_entry();
                    entry.tape_file_number += 1;
                    entry
                }))
                .collect(),
        )
        .expect("bare entries build a valid map");
        assert_eq!(
            map_with_hint
                .canonical_projection_bytes()
                .expect("hinted canonical projection encodes"),
            map_without_hint
                .canonical_projection_bytes()
                .expect("bare canonical projection encodes"),
            "the §7.1 seven-element canonical projection must be byte-identical"
        );
        assert_eq!(
            map_with_hint.canonical_digest().expect("hinted digest"),
            map_without_hint.canonical_digest().expect("bare digest"),
            "bootstrap digests must not move when the hint is populated"
        );
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
    fn file_journal_rejects_legacy_narrow_structural_version() {
        let path = temp_journal_path("legacy-u32-version");
        let tape_uuid = [0x63; 16];
        let scheme = default_scheme();
        {
            let mut file = File::create(&path).expect("create journal");
            write_header(&mut file, tape_uuid, 256 * 1024, &scheme).expect("write v4 header");
            file.sync_all().expect("sync header");
        }

        let mut bytes = fs::read(&path).expect("read journal header");
        bytes[8..10].copy_from_slice(&3u16.to_le_bytes());
        let crc_start = bytes
            .len()
            .checked_sub(8)
            .expect("journal header includes CRC");
        let crc = crc64_xz(&bytes[..crc_start]);
        bytes[crc_start..].copy_from_slice(&crc.to_le_bytes());
        fs::write(&path, bytes).expect("rewrite legacy-version header");

        let error = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect_err("v3 u32 journal must fail closed");
        assert!(matches!(error, JournalError::HeaderMismatch));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_journal_preserves_torn_trailing_record_and_fails_closed() {
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

        let len_with_torn_tail = fs::metadata(&path).expect("stat torn journal").len();
        let err = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect_err("torn journal must require reconciliation");
        assert!(matches!(err, JournalError::RecoveryRequired(_)), "{err}");
        assert_eq!(fs::metadata(&path).unwrap().len(), len_with_torn_tail);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_journal_bounds_and_preserves_corrupt_trailing_length() {
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

        let corrupt_len = fs::metadata(&path).unwrap().len();
        let err = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect_err("hostile length must require reconciliation");
        assert!(matches!(err, JournalError::RecoveryRequired(_)), "{err}");
        assert_eq!(fs::metadata(&path).unwrap().len(), corrupt_len);

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
        let err = reader
            .load_committed()
            .expect_err("shared replay must report the torn tail");
        assert!(matches!(err, JournalError::RecoveryRequired(_)), "{err}");
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            len_with_torn_tail,
            "read-only replay must not truncate the journal"
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn bounded_committed_replay_matches_flattened_authority_and_rejects_orphans() {
        let path = temp_journal_path("bounded-committed-replay");
        let tape_uuid = [0x6B; 16];
        let scheme = default_scheme();
        let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("open journal");
        commit_sample_checkpoint(&mut journal);
        let expected = journal.load_committed().expect("flatten committed state");

        let mut replay = journal
            .replay_committed_entries_bounded()
            .expect("open bounded committed replay");
        assert_eq!(
            replay.committed_entry_count(),
            u64::try_from(expected.entries.len()).expect("test entry count fits u64")
        );
        assert_eq!(
            replay.highest_protected_ordinal(),
            expected.highest_protected_ordinal
        );
        assert_eq!(
            replay.total_committed_ordinals(),
            expected.total_committed_ordinals
        );
        let mut rows = Vec::new();
        while let Some(row) = replay.next_entry().expect("replay bounded row") {
            rows.push(row);
        }
        assert_eq!(rows, expected.entries);
        drop(replay);

        let orphan = CommittedBundle {
            kind: CommittedBundleKind::Object,
            entries: vec![TapeFileEntry::from_map_entry(TapeFileMapEntry::object(
                2, 2, 3,
            ))],
            highest_protected_ordinal: 3,
            total_committed_ordinals: 5,
        };
        journal
            .commit_bundle(&orphan)
            .expect("append orphan fixture");
        let error = journal
            .replay_committed_entries_bounded()
            .expect_err("bounded finalization replay must reject orphan authority");
        assert!(
            matches!(error, JournalError::RecoveryRequired(_)),
            "{error}"
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn bounded_authority_trait_fails_closed_by_default_and_file_override_replays() {
        let error = UnboundedTestJournal
            .committed_snapshot_bounded_authority()
            .expect_err("unbounded compatibility journal must not enter production resume");
        assert!(
            error.to_string().contains("does not provide bounded"),
            "{error}"
        );

        let path = temp_journal_path("bounded-authority-trait");
        let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            [0x6D; 16],
            256 * 1024,
            default_scheme(),
        )
        .expect("open file journal");
        commit_sample_checkpoint(&mut journal);
        let snapshot = (&journal as &dyn TapeFileJournal)
            .committed_snapshot_bounded_authority()
            .expect("file journal trait override freezes bounded authority");
        assert_eq!(snapshot.committed_entry_count(), 2);
        assert_eq!(snapshot.highest_protected_ordinal(), 3);
        assert_eq!(snapshot.total_committed_ordinals(), 3);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn high_count_bounded_replay_reports_passes_and_peak_live_rows() {
        const BUNDLE_COUNT: u64 = 64;
        const ROWS_PER_BUNDLE: u64 = 128;
        let path = temp_journal_path("bounded-high-count");
        let tape_uuid = [0x6C; 16];
        let mut journal = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            default_scheme(),
        )
        .expect("open high-count journal");
        journal
            .commit_bundle(&structural_bundle(
                CommittedBundleKind::BotBootstrap,
                &[TapeFileKind::Bootstrap],
            ))
            .expect("append sole BOT Bootstrap bundle");
        for bundle_index in 0..BUNDLE_COUNT {
            let start = 1 + bundle_index * ROWS_PER_BUNDLE;
            let entries = (0..ROWS_PER_BUNDLE)
                .map(|offset| {
                    TapeFileEntry::from_map_entry(TapeFileMapEntry::parity_sidecar(
                        start + offset,
                        2,
                        bundle_index * ROWS_PER_BUNDLE + offset,
                        0,
                        0,
                    ))
                })
                .collect();
            journal
                .commit_bundle(&CommittedBundle {
                    kind: CommittedBundleKind::CheckpointSidecars,
                    entries,
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 0,
                })
                .expect("append bounded checkpoint-sidecar bundle");
        }
        journal
            .commit_bundle(&CommittedBundle {
                kind: CommittedBundleKind::CheckpointedThrough,
                entries: Vec::new(),
                highest_protected_ordinal: 0,
                total_committed_ordinals: 0,
            })
            .expect("checkpoint high-count authority");

        let mut replay = journal
            .replay_committed_entries_bounded()
            .expect("open high-count bounded replay");
        let initial = replay.metrics();
        assert_eq!(initial.validation_passes, 1);
        assert_eq!(initial.row_replay_passes, 0);
        assert_eq!(initial.journal_record_count, BUNDLE_COUNT + 2);
        assert_eq!(
            initial.peak_live_entry_count,
            ROWS_PER_BUNDLE * 2,
            "validation retains only the previous and current bounded bundles"
        );
        assert!(initial.peak_record_payload_bytes <= MAX_RECORD_LEN);
        assert_eq!(
            replay.committed_entry_count(),
            1 + BUNDLE_COUNT * ROWS_PER_BUNDLE
        );
        let mut emitted = 0u64;
        while replay.next_entry().expect("emit high-count row").is_some() {
            emitted += 1;
        }
        assert_eq!(emitted, 1 + BUNDLE_COUNT * ROWS_PER_BUNDLE);
        assert_eq!(replay.metrics().row_replay_passes, 1);
        assert!(replay.metrics().peak_live_entry_count < emitted);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn replay_filters_watermark_orphans_and_exclusive_reopen_preserves_them() {
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
        assert_eq!(state.orphaned_bundles, vec![orphan.clone()]);
        assert_eq!(fs::metadata(&path).unwrap().len(), orphaned_len);

        let mut reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("exclusive reopen preserves orphans");
        assert_eq!(reopened.orphaned_bundles_preserved_on_open(), 1);
        assert_eq!(fs::metadata(&path).unwrap().len(), orphaned_len);
        let preserved = reopened.load_committed().expect("replay preserved journal");
        assert_eq!(preserved.entries, sample_bundle().entries);
        assert_eq!(preserved.orphaned_bundles, vec![orphan.clone()]);

        let wrong = reopened
            .truncate_reconciled_orphans(&[sample_bundle()])
            .expect_err("changed reconciliation evidence must not truncate");
        assert!(
            matches!(wrong, JournalError::RecoveryRequired(_)),
            "{wrong}"
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), orphaned_len);
        let reconciled = reopened
            .truncate_reconciled_orphans(std::slice::from_ref(&orphan))
            .expect("truncate exactly reconciled orphan suffix");
        assert!(reconciled.orphaned_bundles.is_empty());
        assert_eq!(fs::metadata(&path).unwrap().len(), checkpoint_len);
        assert_eq!(reopened.orphaned_bundles_preserved_on_open(), 0);
        drop(reopened);

        let reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            default_scheme(),
        )
        .expect("restart after durable orphan truncation");
        let state = reopened.load_committed().expect("replay reconciled prefix");
        assert!(state.orphaned_bundles.is_empty());
        assert_eq!(state.total_committed_ordinals, 3);

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

        let mut reopened = FileTapeFileJournal::open_without_volume_check_for_tests(
            &path,
            tape_uuid,
            256 * 1024,
            scheme,
        )
        .expect("exclusive reopen preserves pre-watermark bundles");
        assert_eq!(reopened.orphaned_bundles_preserved_on_open(), 1);
        let preserved = reopened.load_committed().expect("replay preserved journal");
        assert!(preserved.entries.is_empty());
        assert_eq!(preserved.orphaned_bundles, vec![sample_bundle()]);
        let err = reopened
            .commit_bundle(&sample_bundle())
            .expect_err("preserved orphan evidence fences append");
        assert!(err.to_string().contains("reconcile physical tail"), "{err}");

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
