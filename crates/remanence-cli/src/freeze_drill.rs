//! Live and hermetic §18.4 freeze-drill orchestration.
//!
//! The drill writes deterministic REM-OBJECT payloads through the production
//! parity sink, derives read-side medium-error LBAs from the committed tape
//! map, scans without a catalog through the §12.4 overlay funnel, and streams
//! every recovered object through digest verification. Live Linux SG and the
//! in-memory SSC model differ only in their `DrillTransportFactory`
//! implementations; [`run_drill`] owns the complete workflow.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use remanence_format::{
    plan_rem_tar_object, stream_rem_tar_object, write_rem_tar_object_from_readers, FormatError,
    RemTarEntrySink, RemTarFileSpec, RemTarFileStream, RemTarObjectOptions, RemTarStreamEntry,
};
use remanence_library::{DriveHandle, DriveHandleSource, SgTransport};
use remanence_parity::{
    default_scheme_for_block_size, scan_reconstruct_filemark_map_with_report,
    validate_scan_reconstruction_with_report, BootstrapObjectRow, BootstrapObjectRowAdmission,
    CapacityReserveInput, CommittedBundle, CommittedBundleKind, CommittedState, DriveHandleRawSink,
    DriveHandleRawSource, FilemarkMap, JournalError, ObjectParitySource, OpenTrust,
    ParityAuditHook, ParityScheme, ParitySink, RecoveryEvent, RecoveryOutcome, ScanDamageKind,
    ScanOverlaySource, TapeFileJournal, TapeFileKind, TapeFilePosition,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MIB: u64 = 1024 * 1024;
const DRILL_UUID_PREFIX: &[u8; 6] = b"RMDL1!";
const DRILL_SEED: &[u8] = b"remanence-freeze-drill-v1";
const DRILL_TIMESTAMP: &str = "2026-01-01T00:00:00Z";
const OBJECT_COUNT: usize = 2;

/// Fixed block sizes admitted by the §18.4 steering vehicle.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum FreezeDrillBlockSize {
    /// 256 KiB fixed records.
    #[value(name = "262144")]
    Bytes262144,
    /// 512 KiB fixed records.
    #[value(name = "524288")]
    Bytes524288,
    /// 1 MiB fixed records.
    #[value(name = "1048576")]
    Bytes1048576,
}

impl FreezeDrillBlockSize {
    pub(crate) fn bytes(self) -> u32 {
        match self {
            Self::Bytes262144 => 262_144,
            Self::Bytes524288 => 524_288,
            Self::Bytes1048576 => 1_048_576,
        }
    }
}

/// Read-side damage arrangements supported by the freeze drill.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DamagePlan {
    /// Make the BOT bootstrap copy unreadable.
    BootstrapCopy0,
    /// Make one sidecar's primary header/index copy unreadable.
    SidecarHead,
    /// Make a contiguous, parity-tolerable object-data span unreadable.
    ObjectSpan,
    /// Make one intermediate checkpoint bootstrap unreadable.
    CheckpointBootstrap,
    /// Combine the maximal §12.4-conforming set of damage classes.
    Combined,
}

impl DamagePlan {
    #[cfg(test)]
    const ALL: [Self; 5] = [
        Self::BootstrapCopy0,
        Self::SidecarHead,
        Self::ObjectSpan,
        Self::CheckpointBootstrap,
        Self::Combined,
    ];
}

#[derive(Clone, Debug)]
pub(crate) struct DrillSettings {
    block_size: u32,
    data_mib: u64,
    damage_plan: DamagePlan,
    unreadable_bot_ack: bool,
    scheme: ParityScheme,
}

impl DrillSettings {
    fn live(
        block_size: u32,
        data_mib: u64,
        damage_plan: DamagePlan,
        unreadable_bot_ack: bool,
    ) -> Result<Self, String> {
        validate_block_size(block_size)?;
        if data_mib == 0 {
            return Err("--data-mib must be greater than zero".to_string());
        }
        data_mib
            .checked_mul(MIB)
            .ok_or_else(|| "--data-mib byte count overflows u64".to_string())?;
        Ok(Self {
            block_size,
            data_mib,
            damage_plan,
            unreadable_bot_ack,
            scheme: default_scheme_for_block_size(block_size),
        })
    }
}

/// One planned read fault and why that physical record was selected.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct FaultedBlock {
    lba: u64,
    role: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct ScanSummary {
    bootstrap_generation_used: u32,
    bootstrap_tape_file_number: u32,
    overlay_source: &'static str,
    retyping_performed: bool,
    damaged_regions: Vec<ScanDamageReport>,
}

#[derive(Clone, Debug, Serialize)]
struct ScanDamageReport {
    start_lba: u64,
    block_count: u64,
    kind: &'static str,
}

#[derive(Clone, Debug, Default, Serialize)]
struct ObjectVerificationSummary {
    total: u64,
    verified: u64,
    failed: u64,
    failures: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct PhaseDurationsMs {
    write: u64,
    damage: u64,
    recovery: u64,
    total: u64,
}

/// Stable machine-readable result written by `rem-debug tape freeze-drill`.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct FreezeDrillReport {
    report_version: u32,
    tape_uuid: String,
    block_size_bytes: u32,
    data_mib: u64,
    data_bytes: u64,
    damage_plan: DamagePlan,
    faulted_blocks: Vec<FaultedBlock>,
    observed_fault_lbas: Vec<u64>,
    scan: ScanSummary,
    blocks_reconstructed: u64,
    objects: ObjectVerificationSummary,
    expectations_met: bool,
    expectation_failures: Vec<String>,
    wall_clock_ms: PhaseDurationsMs,
    success: bool,
}

impl FreezeDrillReport {
    fn exit_code(&self) -> ExitCode {
        if self.success {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(2)
        }
    }
}

struct DamagedDrive {
    drive: DriveHandle,
    engine: remanence_chaos::FaultEngine,
}

trait DrillTransportFactory {
    fn open_clean(&mut self) -> Result<DriveHandle, String>;

    fn open_damaged(
        &mut self,
        fault_lbas: &[u64],
        tape_uuid: [u8; 16],
    ) -> Result<DamagedDrive, String>;
}

struct DrillJournal {
    tape_uuid: [u8; 16],
    bundles: Vec<CommittedBundle>,
}

impl DrillJournal {
    fn new(tape_uuid: [u8; 16]) -> Self {
        Self {
            tape_uuid,
            bundles: Vec::new(),
        }
    }
}

impl TapeFileJournal for DrillJournal {
    fn tape_uuid(&self) -> [u8; 16] {
        self.tape_uuid
    }

    fn commit_bundle(&mut self, bundle: &CommittedBundle) -> Result<(), JournalError> {
        self.bundles.push(bundle.clone());
        Ok(())
    }

    fn load_committed(&self) -> Result<CommittedState, JournalError> {
        let retained_end = self
            .bundles
            .iter()
            .rposition(|bundle| bundle.kind == CommittedBundleKind::CheckpointedThrough)
            .map_or(0, |index| index + 1);
        let retained = &self.bundles[..retained_end];
        let last = retained
            .iter()
            .rev()
            .find(|bundle| bundle.kind != CommittedBundleKind::CheckpointedThrough);
        Ok(CommittedState {
            entries: retained
                .iter()
                .filter(|bundle| bundle.kind != CommittedBundleKind::CheckpointedThrough)
                .flat_map(|bundle| bundle.entries.iter().cloned())
                .collect(),
            highest_protected_ordinal: last.map_or(0, |bundle| bundle.highest_protected_ordinal),
            total_committed_ordinals: last.map_or(0, |bundle| bundle.total_committed_ordinals),
            orphaned_bundles: self.bundles[retained_end..].to_vec(),
        })
    }
}

#[derive(Clone, Debug)]
struct ExpectedObject {
    object_id: String,
    payload_path: String,
    payload_size: u64,
    payload_sha256: [u8; 32],
    tape_file_number: u32,
    stored_block_count: u64,
}

struct WrittenDrill {
    tape_uuid: [u8; 16],
    expected_objects: Vec<ExpectedObject>,
    map: FilemarkMap,
    checkpoint_bootstrap_tape_file: u32,
    first_sidecar_header_block_count: u32,
}

#[derive(Debug)]
struct DeterministicPayload {
    remaining: u64,
    state: u64,
    word: [u8; 8],
    word_offset: usize,
}

impl DeterministicPayload {
    fn new(block_size: u32, object_index: usize, size: u64) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DRILL_SEED);
        hasher.update(block_size.to_le_bytes());
        hasher.update((object_index as u64).to_le_bytes());
        let seed = hasher.finalize();
        let mut state_bytes = [0u8; 8];
        state_bytes.copy_from_slice(&seed[..8]);
        let mut state = u64::from_le_bytes(state_bytes);
        if state == 0 {
            state = 0x9e37_79b9_7f4a_7c15;
        }
        Self {
            remaining: size,
            state,
            word: [0; 8],
            word_offset: 8,
        }
    }

    fn refill_word(&mut self) {
        let mut value = self.state;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.state = value;
        self.word = value.to_le_bytes();
        self.word_offset = 0;
    }
}

impl Read for DeterministicPayload {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let wanted = usize::try_from(self.remaining.min(buf.len() as u64))
            .map_err(|_| std::io::Error::other("deterministic read length exceeds usize"))?;
        let mut written = 0usize;
        while written < wanted {
            if self.word_offset == self.word.len() {
                self.refill_word();
            }
            let available = self.word.len() - self.word_offset;
            let count = available.min(wanted - written);
            buf[written..written + count]
                .copy_from_slice(&self.word[self.word_offset..self.word_offset + count]);
            self.word_offset += count;
            written += count;
        }
        self.remaining = self.remaining.saturating_sub(written as u64);
        Ok(written)
    }
}

fn deterministic_digest(
    block_size: u32,
    object_index: usize,
    size: u64,
) -> Result<[u8; 32], String> {
    let mut reader = DeterministicPayload::new(block_size, object_index, size);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let read = reader
            .read(&mut buf)
            .map_err(|error| format!("generate deterministic payload digest: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hasher.finalize().into())
}

fn deterministic_uuid(block_size: u32, object_index: usize, label: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DRILL_SEED);
    hasher.update(block_size.to_le_bytes());
    hasher.update((object_index as u64).to_le_bytes());
    hasher.update(label);
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

fn payload_sizes(data_mib: u64) -> Result<[u64; OBJECT_COUNT], String> {
    let total = data_mib
        .checked_mul(MIB)
        .ok_or_else(|| "deterministic payload byte count overflows u64".to_string())?;
    let first = total / 2;
    let second = total
        .checked_sub(first)
        .ok_or_else(|| "deterministic payload split underflows".to_string())?;
    if first == 0 || second == 0 {
        return Err("deterministic payload split produced an empty object".to_string());
    }
    Ok([first, second])
}

fn capacity_input(
    scheme: &ParityScheme,
    block_size: u32,
    projected_object_blocks: u64,
    current_epoch_fill_blocks: u64,
) -> CapacityReserveInput {
    CapacityReserveInput {
        projected_object_blocks,
        block_size_bytes: u64::from(block_size),
        current_epoch_fill_blocks,
        data_shards_per_epoch: u64::from(scheme.data_blocks_per_stripe)
            * u64::from(scheme.stripes_per_neighborhood),
        parity_shards_per_epoch: u64::from(scheme.parity_blocks_per_stripe)
            * u64::from(scheme.stripes_per_neighborhood),
        sidecar_index_block_count: 1,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        bootstrap_filemark_blocks: 1,
        pending_completed_sidecars: 0,
        remaining_bootstrap_count: 2,
        safety_margin_blocks: 4,
        remaining_tape_blocks: u64::MAX / 4,
        empty_tape_usable_blocks: u64::MAX / 4,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: u64::MAX / 4,
    }
}

fn authorize_bot(
    classification: &remanence_api::BotClassification,
    unreadable_bot_ack: bool,
) -> Result<[u8; 16], String> {
    match classification {
        remanence_api::BotClassification::BlankCheckEod => Ok(new_drill_tape_uuid()),
        remanence_api::BotClassification::OursBootstrap { uuid, .. }
            if is_drill_tape_uuid(uuid) =>
        {
            Ok(*uuid)
        }
        remanence_api::BotClassification::ReadError if unreadable_bot_ack => {
            Ok(new_drill_tape_uuid())
        }
        remanence_api::BotClassification::ReadError => {
            Err("BOT is unreadable; refusing without --yes-i-know-scratch".to_string())
        }
        remanence_api::BotClassification::OursBootstrap { uuid, .. } => Err(format!(
            "refusing to overwrite foreign Remanence tape identity {}; it has no freeze-drill identity marker",
            Uuid::from_bytes(*uuid)
        )),
        remanence_api::BotClassification::ForeignFormat { name } => Err(format!(
            "refusing to overwrite tape with foreign BOT format {name:?}"
        )),
        remanence_api::BotClassification::UnrecognizedData => {
            Err("refusing to overwrite unrecognized readable BOT data".to_string())
        }
    }
}

fn new_drill_tape_uuid() -> [u8; 16] {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[..DRILL_UUID_PREFIX.len()].copy_from_slice(DRILL_UUID_PREFIX);
    bytes
}

fn is_drill_tape_uuid(uuid: &[u8; 16]) -> bool {
    uuid.starts_with(DRILL_UUID_PREFIX)
}

fn write_drill_tape(
    drive: &mut DriveHandle,
    settings: &DrillSettings,
) -> Result<WrittenDrill, String> {
    let bot = {
        let mut source = DriveHandleSource(drive);
        remanence_api::classify_bot_from_source(&mut source)
    };
    let tape_uuid = authorize_bot(&bot.classification, settings.unreadable_bot_ack)?;
    drive
        .rewind()
        .map_err(|error| format!("rewind scratch tape before write: {error}"))?;

    let mut raw = DriveHandleRawSink::new(drive);
    raw.configure_parity_write_session(settings.block_size)
        .map_err(|error| format!("configure parity write session: {error}"))?;
    let mut journal = DrillJournal::new(tape_uuid);
    let mut parity = ParitySink::new_with_journal(
        &mut raw,
        &mut journal,
        settings.scheme.clone(),
        tape_uuid,
        settings.block_size,
    )
    .map_err(|error| format!("open parity write sink: {error}"))?;
    parity
        .write_bootstrap()
        .map_err(|error| format!("write BOT bootstrap: {error}"))?;

    let sizes = payload_sizes(settings.data_mib)?;
    let mut expected_objects = Vec::with_capacity(OBJECT_COUNT);
    let mut checkpoint_bootstrap_tape_file = None;
    let mut first_sidecar_header_block_count = None;
    for (object_index, payload_size) in sizes.into_iter().enumerate() {
        let object_id = deterministic_uuid(settings.block_size, object_index, b"object");
        let manifest_file_id = deterministic_uuid(settings.block_size, object_index, b"manifest");
        let payload_path = format!("freeze-drill/object-{object_index}.bin");
        let payload_sha256 = deterministic_digest(settings.block_size, object_index, payload_size)?;
        let mut options = RemTarObjectOptions::new(
            object_id.clone(),
            format!("freeze-drill-{object_index}"),
            DRILL_TIMESTAMP,
            manifest_file_id,
        );
        options.chunk_size = settings.block_size as usize;
        let spec = RemTarFileSpec::new(
            payload_path.clone(),
            deterministic_uuid(settings.block_size, object_index, b"file"),
            payload_size,
            payload_sha256,
        );
        let layout = plan_rem_tar_object(&options, std::slice::from_ref(&spec))
            .map_err(|error| format!("plan deterministic object {object_index}: {error}"))?;
        let reserve = capacity_input(
            &settings.scheme,
            settings.block_size,
            layout.projected_size_blocks,
            parity.data_blocks_in_neighborhood(),
        );
        let opened = parity
            .begin_object_with_capacity_reserve_and_bootstrap_object_row(
                reserve,
                BootstrapObjectRowAdmission::PlaintextRemObject,
            )
            .map_err(|error| format!("admit deterministic object {object_index}: {error}"))?;
        let mut payload =
            DeterministicPayload::new(settings.block_size, object_index, payload_size);
        let mut streams = [RemTarFileStream::new(spec, &mut payload)];
        let written_layout = write_rem_tar_object_from_readers(&mut parity, &options, &mut streams)
            .map_err(|error| format!("write deterministic object {object_index}: {error}"))?;
        parity
            .record_bootstrap_object_row(
                BootstrapObjectRow::plaintext(
                    opened.0,
                    written_layout.projected_size_blocks,
                    written_layout
                        .manifest
                        .first_chunk_lba
                        .ok_or_else(|| {
                            format!("object {object_index} manifest has no first chunk LBA")
                        })?
                        .0,
                    written_layout.manifest.size_bytes,
                    written_layout.manifest.chunk_count,
                    written_layout.manifest_sha256,
                )
                .with_object_id(object_id.as_bytes().to_vec()),
            )
            .map_err(|error| {
                format!("record deterministic object {object_index} bootstrap row: {error}")
            })?;
        let closed = parity
            .finish_object()
            .map_err(|error| format!("close deterministic object {object_index}: {error}"))?;
        expected_objects.push(ExpectedObject {
            object_id,
            payload_path,
            payload_size,
            payload_sha256,
            tape_file_number: closed.tape_file_number,
            stored_block_count: closed.data_block_count,
        });
        if object_index == 0 {
            let checkpoint = parity
                .checkpoint()
                .map_err(|error| format!("write intermediate checkpoint: {error}"))?;
            checkpoint_bootstrap_tape_file = Some(checkpoint.bootstrap_tape_file_number);
            first_sidecar_header_block_count = checkpoint
                .sidecars_emitted
                .first()
                .map(|sidecar| sidecar.sidecar_header_block_count);
        }
    }
    parity
        .finish()
        .map_err(|error| format!("write final bootstrap: {error}"))?;
    let committed = journal
        .load_committed()
        .map_err(|error| format!("replay in-memory drill journal: {error}"))?;
    if !committed.orphaned_bundles.is_empty() {
        return Err("drill journal retained orphaned bundles after final checkpoint".to_string());
    }
    let map = committed
        .filemark_map()
        .map_err(|error| format!("build committed drill filemark map: {error}"))?;
    Ok(WrittenDrill {
        tape_uuid,
        expected_objects,
        map,
        checkpoint_bootstrap_tape_file: checkpoint_bootstrap_tape_file
            .ok_or_else(|| "drill did not emit an intermediate checkpoint".to_string())?,
        first_sidecar_header_block_count: first_sidecar_header_block_count
            .ok_or_else(|| "drill checkpoint emitted no parity sidecar".to_string())?,
    })
}

fn physical_lba(
    map: &FilemarkMap,
    tape_file_number: u32,
    block_within_file: u64,
) -> Result<u64, String> {
    map.physical_position(TapeFilePosition {
        tape_file_number,
        block_within_file,
    })
    .map(|position| position.lba)
    .map_err(|error| {
        format!(
            "map tape file {tape_file_number} block {block_within_file} to physical LBA: {error}"
        )
    })
}

fn build_fault_plan(
    settings: &DrillSettings,
    written: &WrittenDrill,
) -> Result<Vec<FaultedBlock>, String> {
    let mut faults = Vec::new();
    // §12.4 forbids hypotheses that re-type multiple files at once. Keep the
    // combined plan maximal by selecting the checkpoint bootstrap (which
    // proves re-typing) and leaving the independently tested BOT copy intact.
    let includes = |candidate| {
        settings.damage_plan == candidate
            || (settings.damage_plan == DamagePlan::Combined
                && candidate != DamagePlan::BootstrapCopy0)
    };
    if includes(DamagePlan::BootstrapCopy0) {
        faults.push(FaultedBlock {
            lba: physical_lba(&written.map, 0, 0)?,
            role: "bootstrap_copy0",
        });
    }
    if includes(DamagePlan::SidecarHead) {
        let sidecar = written
            .map
            .entries()
            .iter()
            .find(|entry| entry.kind == TapeFileKind::ParitySidecar)
            .ok_or_else(|| "drill write emitted no parity sidecar".to_string())?;
        let sidecar_start = physical_lba(&written.map, sidecar.tape_file_number, 0)?;
        let header_blocks = written.first_sidecar_header_block_count;
        for offset in 0..u64::from(header_blocks) {
            faults.push(FaultedBlock {
                lba: sidecar_start
                    .checked_add(offset)
                    .ok_or_else(|| "sidecar header fault LBA overflows".to_string())?,
                role: "sidecar_head",
            });
        }
    }
    if includes(DamagePlan::ObjectSpan) {
        let object = written
            .expected_objects
            .last()
            .ok_or_else(|| "drill write emitted no objects".to_string())?;
        let available_after_head = object.stored_block_count.saturating_sub(1);
        let (start, count): (u64, u64) = if available_after_head == 0 {
            (0, 1)
        } else {
            (1, available_after_head.min(4))
        };
        for offset in 0..count {
            faults.push(FaultedBlock {
                lba: physical_lba(
                    &written.map,
                    object.tape_file_number,
                    start
                        .checked_add(offset)
                        .ok_or_else(|| "object damage offset overflows".to_string())?,
                )?,
                role: "object_span",
            });
        }
    }
    if includes(DamagePlan::CheckpointBootstrap) {
        faults.push(FaultedBlock {
            lba: physical_lba(&written.map, written.checkpoint_bootstrap_tape_file, 0)?,
            role: "checkpoint_bootstrap",
        });
    }
    faults.sort_by_key(|fault| (fault.lba, fault.role));
    faults.dedup();
    if faults.is_empty() {
        return Err("damage plan selected no physical blocks".to_string());
    }
    Ok(faults)
}

#[derive(Default)]
struct RecoveryCounter {
    reconstructed: AtomicU64,
}

impl ParityAuditHook for RecoveryCounter {
    fn on_recovery(&self, event: &RecoveryEvent) {
        if matches!(&event.outcome, RecoveryOutcome::Recovered) {
            self.reconstructed.fetch_add(
                u64::try_from(event.lost_blocks.len()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
    }
}

struct PayloadVerifier {
    expected_path: String,
    expected_size: u64,
    expected_sha256: [u8; 32],
    active_payload: bool,
    saw_payload: bool,
    bytes: u64,
    hasher: Sha256,
}

impl PayloadVerifier {
    fn new(expected: &ExpectedObject) -> Self {
        Self {
            expected_path: expected.payload_path.clone(),
            expected_size: expected.payload_size,
            expected_sha256: expected.payload_sha256,
            active_payload: false,
            saw_payload: false,
            bytes: 0,
            hasher: Sha256::new(),
        }
    }

    fn finish(self) -> Result<(), String> {
        if !self.saw_payload {
            return Err(format!("payload {} was not present", self.expected_path));
        }
        if self.bytes != self.expected_size {
            return Err(format!(
                "payload {} read {} bytes, expected {}",
                self.expected_path, self.bytes, self.expected_size
            ));
        }
        let actual: [u8; 32] = self.hasher.finalize().into();
        if actual != self.expected_sha256 {
            return Err(format!(
                "payload {} SHA-256 mismatch: expected {}, got {}",
                self.expected_path,
                hex(&self.expected_sha256),
                hex(&actual)
            ));
        }
        Ok(())
    }
}

impl RemTarEntrySink for PayloadVerifier {
    fn begin_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        self.active_payload = entry.path == self.expected_path;
        if self.active_payload {
            if self.saw_payload {
                return Err(FormatError::invalid(format!(
                    "duplicate expected payload {}",
                    self.expected_path
                )));
            }
            self.saw_payload = true;
        }
        Ok(())
    }

    fn write_file_data(&mut self, bytes: &[u8]) -> Result<(), FormatError> {
        if self.active_payload {
            self.bytes = self
                .bytes
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| FormatError::invalid("verified payload byte count overflows"))?;
            self.hasher.update(bytes);
        }
        Ok(())
    }

    fn end_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        if self.active_payload && entry.path != self.expected_path {
            return Err(FormatError::invalid(
                "payload verifier active entry changed unexpectedly",
            ));
        }
        self.active_payload = false;
        Ok(())
    }
}

struct RecoveryRun {
    scan: ScanSummary,
    blocks_reconstructed: u64,
    objects: ObjectVerificationSummary,
    observed_fault_lbas: Vec<u64>,
    expectation_failures: Vec<String>,
}

fn recover_and_verify(
    drive: &mut DriveHandle,
    settings: &DrillSettings,
    written: &WrittenDrill,
    engine: &remanence_chaos::FaultEngine,
    faulted_blocks: &[FaultedBlock],
) -> Result<RecoveryRun, String> {
    let mut raw = DriveHandleRawSource::new(drive);
    let walk = scan_reconstruct_filemark_map_with_report(
        &mut raw,
        &written.tape_uuid,
        settings.block_size,
    )
    .map_err(|error| format!("catalog-less structural scan: {error}"))?;
    let authoritative = walk
        .authoritative_bootstrap()
        .cloned()
        .ok_or_else(|| "catalog-less scan found no surviving bootstrap".to_string())?;
    let walked_map = walk.map.clone();
    let validated =
        validate_scan_reconstruction_with_report(&mut raw, &authoritative.payload, walk)
            .map_err(|error| format!("validate §12.4 scan reconstruction: {error}"))?;
    let retyping_performed = bootstrap_retyping_performed(&walked_map, &validated.scoped_map.map);
    let scan = ScanSummary {
        bootstrap_generation_used: validated.authoritative_bootstrap_sequence,
        bootstrap_tape_file_number: authoritative.tape_file_number,
        overlay_source: overlay_source_name(validated.overlay_source),
        retyping_performed,
        damaged_regions: validated
            .damaged_regions
            .iter()
            .map(|region| ScanDamageReport {
                start_lba: region.start.lba,
                block_count: region.block_count,
                kind: match region.kind {
                    ScanDamageKind::UnreadableTapeFileHead => "unreadable_tape_file_head",
                },
            })
            .collect(),
    };

    let counter = Arc::new(RecoveryCounter::default());
    let scheme_record = authoritative
        .payload
        .scheme
        .as_ref()
        .ok_or_else(|| "authoritative drill bootstrap has no parity scheme".to_string())?;
    let read_scheme = ParityScheme {
        id: remanence_parity::SchemeId::new_owned(scheme_record.id.clone()),
        data_blocks_per_stripe: scheme_record.data_blocks_per_stripe,
        parity_blocks_per_stripe: scheme_record.parity_blocks_per_stripe,
        stripes_per_neighborhood: scheme_record.stripes_per_neighborhood,
    };
    if read_scheme != settings.scheme {
        return Err("authoritative bootstrap parity scheme differs from write scheme".to_string());
    }

    let mut objects = ObjectVerificationSummary {
        total: written.expected_objects.len() as u64,
        ..ObjectVerificationSummary::default()
    };
    for expected in &written.expected_objects {
        let result = (|| -> Result<(), String> {
            let mut source = ObjectParitySource::open(
                &mut raw,
                read_scheme.clone(),
                written.tape_uuid,
                validated.scoped_map.clone(),
                settings.block_size,
                expected.tape_file_number,
                OpenTrust::RequireValidated,
            )
            .map_err(|error| {
                format!(
                    "open recovered object tape file {}: {error}",
                    expected.tape_file_number
                )
            })?;
            source.set_audit_hook(Some(counter.clone()));
            let mut verifier = PayloadVerifier::new(expected);
            let report = stream_rem_tar_object(
                &mut source,
                settings.block_size as usize,
                expected.stored_block_count,
                &mut verifier,
            )
            .map_err(|error| {
                format!(
                    "stream recovered object tape file {}: {error}",
                    expected.tape_file_number
                )
            })?;
            let actual_object_id =
                report
                    .global_pax
                    .get("REMANENCE.object_id")
                    .ok_or_else(|| {
                        format!(
                            "object tape file {} has no REMANENCE.object_id",
                            expected.tape_file_number
                        )
                    })?;
            if actual_object_id != &expected.object_id {
                return Err(format!(
                    "object tape file {} id mismatch: expected {}, got {}",
                    expected.tape_file_number, expected.object_id, actual_object_id
                ));
            }
            verifier.finish()
        })();
        match result {
            Ok(()) => objects.verified += 1,
            Err(error) => {
                objects.failed += 1;
                objects.failures.push(error);
            }
        }
    }

    let observed = engine.observed_medium_error_lbas();
    let expected_fault_lbas = faulted_blocks
        .iter()
        .map(|fault| fault.lba)
        .collect::<BTreeSet<_>>();
    let mut expectation_failures = Vec::new();
    for missed in expected_fault_lbas.difference(&observed) {
        expectation_failures.push(format!("planned medium error at LBA {missed} did not fire"));
    }
    if settings.damage_plan == DamagePlan::CheckpointBootstrap && !retyping_performed {
        expectation_failures
            .push("checkpoint-bootstrap plan did not trigger §12.4 re-typing".to_string());
    }
    if settings.damage_plan == DamagePlan::Combined && !retyping_performed {
        expectation_failures.push("combined plan did not trigger §12.4 re-typing".to_string());
    }
    if matches!(
        settings.damage_plan,
        DamagePlan::ObjectSpan | DamagePlan::Combined
    ) && counter.reconstructed.load(Ordering::Relaxed) == 0
    {
        expectation_failures
            .push("object damage was not reconstructed through the parity source".to_string());
    }
    Ok(RecoveryRun {
        scan,
        blocks_reconstructed: counter.reconstructed.load(Ordering::Relaxed),
        objects,
        observed_fault_lbas: observed.into_iter().collect(),
        expectation_failures,
    })
}

fn bootstrap_retyping_performed(walked: &FilemarkMap, validated: &FilemarkMap) -> bool {
    walked
        .entries()
        .iter()
        .zip(validated.entries())
        .any(|(before, after)| {
            before.tape_file_number == after.tape_file_number
                && before.kind == TapeFileKind::Object
                && after.kind == TapeFileKind::Bootstrap
        })
}

fn overlay_source_name(source: ScanOverlaySource) -> &'static str {
    match source {
        ScanOverlaySource::StructuralWalk => "structural_walk",
        ScanOverlaySource::Catalog => "catalog",
        ScanOverlaySource::BootstrapInlineDirectory => "bootstrap_inline_directory",
        ScanOverlaySource::ReferencedParityMap => "referenced_parity_map",
        ScanOverlaySource::StructurallySelectedParityMap => "structurally_selected_parity_map",
        ScanOverlaySource::ParityMapReferenceProjection => "parity_map_reference_projection",
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Execute one complete write/damage/scan/recovery/verify drill.
fn run_drill(
    factory: &mut dyn DrillTransportFactory,
    settings: DrillSettings,
) -> Result<FreezeDrillReport, String> {
    let total_started = Instant::now();
    let write_started = Instant::now();
    let written = {
        let mut drive = factory.open_clean()?;
        write_drill_tape(&mut drive, &settings)?
    };
    let write_elapsed = write_started.elapsed();

    let damage_started = Instant::now();
    let faulted_blocks = build_fault_plan(&settings, &written)?;
    let fault_lbas = faulted_blocks
        .iter()
        .map(|fault| fault.lba)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut damaged = factory.open_damaged(&fault_lbas, written.tape_uuid)?;
    let damage_elapsed = damage_started.elapsed();

    let recovery_started = Instant::now();
    let recovery = recover_and_verify(
        &mut damaged.drive,
        &settings,
        &written,
        &damaged.engine,
        &faulted_blocks,
    )?;
    let recovery_elapsed = recovery_started.elapsed();
    let expectations_met = recovery.expectation_failures.is_empty();
    let success = recovery.objects.failed == 0
        && recovery.objects.verified == recovery.objects.total
        && expectations_met;
    Ok(FreezeDrillReport {
        report_version: 1,
        tape_uuid: Uuid::from_bytes(written.tape_uuid).to_string(),
        block_size_bytes: settings.block_size,
        data_mib: settings.data_mib,
        data_bytes: settings
            .data_mib
            .checked_mul(MIB)
            .ok_or_else(|| "report payload byte count overflows u64".to_string())?,
        damage_plan: settings.damage_plan,
        faulted_blocks,
        observed_fault_lbas: recovery.observed_fault_lbas,
        scan: recovery.scan,
        blocks_reconstructed: recovery.blocks_reconstructed,
        objects: recovery.objects,
        expectations_met,
        expectation_failures: recovery.expectation_failures,
        wall_clock_ms: PhaseDurationsMs {
            write: duration_ms(write_elapsed),
            damage: duration_ms(damage_elapsed),
            recovery: duration_ms(recovery_elapsed),
            total: duration_ms(total_started.elapsed()),
        },
        success,
    })
}

fn validate_block_size(block_size: u32) -> Result<(), String> {
    if matches!(block_size, 262_144 | 524_288 | 1_048_576) {
        Ok(())
    } else {
        Err(format!(
            "unsupported freeze-drill block size {block_size}; expected 262144, 524288, or 1048576"
        ))
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

fn write_report(path: &Path, report: &FreezeDrillReport) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        if !parent.is_dir() {
            return Err(format!(
                "freeze-drill report parent {} is not a directory",
                parent.display()
            ));
        }
    }
    let mut file =
        File::create(path).map_err(|error| format!("create report {}: {error}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, report)
        .map_err(|error| format!("serialize report {}: {error}", path.display()))?;
    file.write_all(b"\n")
        .map_err(|error| format!("finish report {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync report {}: {error}", path.display()))
}

#[cfg(target_os = "linux")]
struct LiveTransportFactory {
    device: PathBuf,
}

#[cfg(target_os = "linux")]
impl LiveTransportFactory {
    fn new(device: PathBuf) -> Result<Self, String> {
        if !remanence_chaos::chaos_real_enabled_from_env() {
            return Err(format!(
                "live freeze-drill requires {}=1 and {}=1",
                remanence_chaos::ENV_CHAOS_ENABLED,
                remanence_chaos::ENV_CHAOS_ALLOW_REAL
            ));
        }
        Ok(Self { device })
    }

    fn open_linux_transport(&self) -> Result<remanence_library::LinuxSgTransport, String> {
        remanence_library::LinuxSgTransport::open_rw(&self.device)
            .map_err(|error| format!("open {}: {error}", self.device.display()))
    }

    fn open_handle(&self, transport: Box<dyn SgTransport>) -> Result<DriveHandle, String> {
        DriveHandle::open_standalone_with_transport(&self.device, transport)
            .map_err(|error| format!("open standalone drive {}: {error}", self.device.display()))
    }
}

#[cfg(target_os = "linux")]
impl DrillTransportFactory for LiveTransportFactory {
    fn open_clean(&mut self) -> Result<DriveHandle, String> {
        let transport = self.open_linux_transport()?;
        self.open_handle(Box::new(transport))
    }

    fn open_damaged(
        &mut self,
        fault_lbas: &[u64],
        tape_uuid: [u8; 16],
    ) -> Result<DamagedDrive, String> {
        let engine =
            remanence_chaos::FaultEngine::for_read_medium_errors(fault_lbas.iter().copied())
                .map_err(|error| format!("build freeze-drill chaos plan: {error}"))?;
        let label = self
            .device
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| self.device.display().to_string());
        let ctx = remanence_chaos::DeviceCtx::new()
            .with_backend("linux")
            .with_drive_id(label)
            .with_tape_id(Uuid::from_bytes(tape_uuid).to_string());
        let transport =
            remanence_chaos::ChaosTransport::new(self.open_linux_transport()?, engine.clone(), ctx);
        Ok(DamagedDrive {
            drive: self.open_handle(Box::new(transport))?,
            engine,
        })
    }
}

/// Borrowed inputs for one live freeze-drill invocation.
pub(crate) struct LiveDrillRequest<'a> {
    pub(crate) device: &'a Path,
    pub(crate) block_size: u32,
    pub(crate) data_mib: u64,
    pub(crate) damage_plan: DamagePlan,
    pub(crate) unreadable_bot_ack: bool,
    pub(crate) report_path: &'a Path,
}

/// Run the live CLI vehicle and persist its JSON report.
pub(crate) fn run_live(
    request: LiveDrillRequest<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    #[cfg(target_os = "linux")]
    let result = (|| {
        let settings = DrillSettings::live(
            request.block_size,
            request.data_mib,
            request.damage_plan,
            request.unreadable_bot_ack,
        )?;
        let mut factory = LiveTransportFactory::new(request.device.to_path_buf())?;
        run_drill(&mut factory, settings)
    })();
    #[cfg(not(target_os = "linux"))]
    let result: Result<FreezeDrillReport, String> = Err(format!(
        "freeze-drill device {} requires Linux SG_IO access",
        request.device.display()
    ));

    let report = match result {
        Ok(report) => report,
        Err(error) => {
            let _ = writeln!(err, "error: freeze-drill: {error}");
            return ExitCode::from(1);
        }
    };
    if let Err(error) = write_report(request.report_path, &report) {
        let _ = writeln!(err, "error: {error}");
        return ExitCode::from(1);
    }
    let _ = writeln!(
        out,
        "freeze-drill {}: {} objects verified, {} blocks reconstructed; report {}",
        if report.success { "passed" } else { "failed" },
        report.objects.verified,
        report.blocks_reconstructed,
        request.report_path.display()
    );
    report.exit_code()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use remanence_chaos::model::{
        DeviceRole, ModelTransport, SharedVirtualWorld, VirtualTape, VirtualWorld,
    };

    use super::*;

    const MODEL_BAY: u16 = 0x0100;

    struct ModelTransportFactory {
        world: SharedVirtualWorld,
    }

    impl ModelTransportFactory {
        fn new(block_size: u32) -> Self {
            let mut world =
                VirtualWorld::single_drive("FREEZE-LIB", MODEL_BAY, "FREEZE-DRV", 0x0400, 1);
            world.put_tape_in_drive(
                MODEL_BAY,
                "FREEZE001",
                None,
                VirtualTape::empty(256 * MIB, block_size),
            );
            Self {
                world: Arc::new(Mutex::new(world)),
            }
        }

        fn model(&self) -> ModelTransport {
            ModelTransport::new(
                Arc::clone(&self.world),
                DeviceRole::Drive { bay: MODEL_BAY },
            )
        }

        fn handle(&self, transport: Box<dyn SgTransport>) -> Result<DriveHandle, String> {
            DriveHandle::open_standalone_with_transport(
                Path::new("/dev/sg-freeze-model"),
                transport,
            )
            .map_err(|error| format!("open model drive: {error}"))
        }
    }

    impl DrillTransportFactory for ModelTransportFactory {
        fn open_clean(&mut self) -> Result<DriveHandle, String> {
            self.handle(Box::new(self.model()))
        }

        fn open_damaged(
            &mut self,
            fault_lbas: &[u64],
            tape_uuid: [u8; 16],
        ) -> Result<DamagedDrive, String> {
            let engine =
                remanence_chaos::FaultEngine::for_read_medium_errors(fault_lbas.iter().copied())
                    .map_err(|error| format!("build model fault engine: {error}"))?;
            let ctx = remanence_chaos::DeviceCtx::new()
                .with_backend("model")
                .with_drive_id("freeze-drive")
                .with_tape_id(Uuid::from_bytes(tape_uuid).to_string());
            let transport = remanence_chaos::ChaosTransport::new(self.model(), engine.clone(), ctx);
            Ok(DamagedDrive {
                drive: self.handle(Box::new(transport))?,
                engine,
            })
        }
    }

    fn test_scheme(block_size: u32) -> ParityScheme {
        let mut scheme = default_scheme_for_block_size(block_size);
        // Preserve the production RS(128,4) codec and reduce only S so the
        // 15-case CI matrix does not emit a 512 MiB sidecar per checkpoint.
        scheme.stripes_per_neighborhood = 4;
        scheme
    }

    #[test]
    fn hermetic_freeze_drill_matrix_passes() {
        for block_size in [262_144, 524_288, 1_048_576] {
            for damage_plan in DamagePlan::ALL {
                let started = Instant::now();
                let mut factory = ModelTransportFactory::new(block_size);
                let report = run_drill(
                    &mut factory,
                    DrillSettings {
                        block_size,
                        data_mib: 1,
                        damage_plan,
                        unreadable_bot_ack: false,
                        scheme: test_scheme(block_size),
                    },
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "hermetic freeze drill failed for block_size={block_size} plan={damage_plan:?}: {error}"
                    )
                });
                assert!(
                    report.success,
                    "block_size={block_size} plan={damage_plan:?}: {:?}",
                    report.expectation_failures
                );
                assert_eq!(report.objects.verified, OBJECT_COUNT as u64);
                assert_eq!(report.objects.failed, 0);
                if damage_plan == DamagePlan::Combined {
                    let roles = report
                        .faulted_blocks
                        .iter()
                        .map(|fault| fault.role)
                        .collect::<BTreeSet<_>>();
                    assert!(!roles.contains("bootstrap_copy0"));
                    assert!(roles.contains("checkpoint_bootstrap"));
                    assert!(roles.contains("sidecar_head"));
                    assert!(roles.contains("object_span"));
                    assert!(report.scan.retyping_performed);
                }
                assert_eq!(
                    report.observed_fault_lbas,
                    report
                        .faulted_blocks
                        .iter()
                        .map(|fault| fault.lba)
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                );
                eprintln!(
                    "freeze-drill hermetic block_size={block_size} plan={damage_plan:?} elapsed_ms={}",
                    duration_ms(started.elapsed())
                );
            }
        }
    }

    #[test]
    fn bot_safety_refuses_foreign_identity_and_requires_unreadable_ack() {
        let geometry = remanence_api::TapeInitGeometry {
            block_size_bytes: 262_144,
            parity: remanence_parity::ParityConfig::Scheme(default_scheme_for_block_size(262_144)),
        };
        let foreign = remanence_api::BotClassification::OursBootstrap {
            uuid: [0xAA; 16],
            geometry: geometry.clone(),
        };
        assert!(authorize_bot(&foreign, true).is_err());
        assert!(authorize_bot(&remanence_api::BotClassification::ReadError, false).is_err());
        let acknowledged =
            authorize_bot(&remanence_api::BotClassification::ReadError, true).unwrap();
        assert!(is_drill_tape_uuid(&acknowledged));
        let blank = authorize_bot(&remanence_api::BotClassification::BlankCheckEod, false).unwrap();
        assert!(is_drill_tape_uuid(&blank));
        let own = remanence_api::BotClassification::OursBootstrap {
            uuid: blank,
            geometry,
        };
        assert_eq!(authorize_bot(&own, false).unwrap(), blank);
    }

    #[test]
    fn hermetic_drill_reuses_only_its_marked_tape_identity() {
        let block_size = 262_144;
        let mut factory = ModelTransportFactory::new(block_size);
        let settings = |damage_plan| DrillSettings {
            block_size,
            data_mib: 1,
            damage_plan,
            unreadable_bot_ack: false,
            scheme: test_scheme(block_size),
        };
        let first = run_drill(&mut factory, settings(DamagePlan::BootstrapCopy0))
            .expect("first blank-tape drill");
        let second = run_drill(&mut factory, settings(DamagePlan::SidecarHead))
            .expect("second drill recognizes its earlier tape identity");
        assert!(first.success);
        assert!(second.success);
        assert_eq!(first.tape_uuid, second.tape_uuid);
    }

    #[test]
    fn deterministic_payload_is_independent_of_read_chunking() {
        let mut one = DeterministicPayload::new(262_144, 0, 19_001);
        let mut one_bytes = Vec::new();
        one.read_to_end(&mut one_bytes).unwrap();

        let mut split = DeterministicPayload::new(262_144, 0, 19_001);
        let mut split_bytes = Vec::new();
        let mut buf = [0u8; 37];
        loop {
            let read = split.read(&mut buf).unwrap();
            if read == 0 {
                break;
            }
            split_bytes.extend_from_slice(&buf[..read]);
        }
        assert_eq!(one_bytes, split_bytes);
        assert_eq!(
            deterministic_digest(262_144, 0, 19_001).unwrap(),
            Sha256::digest(&one_bytes).as_slice()
        );
    }

    #[test]
    fn live_settings_reject_zero_volume_and_unknown_block_size() {
        assert!(DrillSettings::live(262_144, 0, DamagePlan::ObjectSpan, false).is_err());
        assert!(DrillSettings::live(4096, 1, DamagePlan::ObjectSpan, false).is_err());
    }

    #[test]
    fn rem_debug_parses_the_documented_freeze_drill_cli() {
        let cli = <crate::DebugCli as clap::Parser>::try_parse_from([
            "rem-debug",
            "tape",
            "freeze-drill",
            "--device",
            "/dev/sg7",
            "--block-size",
            "524288",
            "--data-mib",
            "32",
            "--damage-plan",
            "checkpoint-bootstrap",
            "--yes-i-know-scratch",
            "--report",
            "/tmp/freeze-report.json",
        ])
        .expect("documented freeze-drill CLI parses");
        let crate::Command::Tape {
            command: crate::TapeCommand::FreezeDrill(args),
        } = cli.command
        else {
            panic!("expected tape freeze-drill command");
        };
        assert_eq!(args.device, Path::new("/dev/sg7"));
        assert_eq!(args.block_size.bytes(), 524_288);
        assert_eq!(args.data_mib, 32);
        assert_eq!(args.damage_plan, DamagePlan::CheckpointBootstrap);
        assert!(args.yes_i_know_scratch);
        assert_eq!(args.report, Path::new("/tmp/freeze-report.json"));
    }

    #[test]
    fn payload_entries_never_treat_manifest_as_expected_data() {
        let expected = ExpectedObject {
            object_id: "id".to_string(),
            payload_path: "payload.bin".to_string(),
            payload_size: 0,
            payload_sha256: Sha256::digest([]).into(),
            tape_file_number: 1,
            stored_block_count: 1,
        };
        let mut verifier = PayloadVerifier::new(&expected);
        let manifest = RemTarStreamEntry {
            entry_type: remanence_format::RemTarEntryType::Regular,
            path: remanence_format::MANIFEST_PATH.to_string(),
            size_bytes: 4,
            first_chunk_lba: None,
            chunk_count: 0,
            data_offset: 0,
            pax_records: Default::default(),
            link_target: None,
            xattrs: Default::default(),
            extensions: Default::default(),
        };
        verifier.begin_file(&manifest).unwrap();
        verifier.write_file_data(b"cbor").unwrap();
        verifier.end_file(&manifest).unwrap();
        assert!(!verifier.saw_payload);
        assert_eq!(verifier.bytes, 0);
    }
}
