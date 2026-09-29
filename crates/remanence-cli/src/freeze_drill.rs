//! Live and hermetic REM-PARITY freeze criterion 4 freeze-drill orchestration.
//!
//! The drill writes deterministic REM-OBJECT payloads through the production
//! parity sink, derives read-side medium-error LBAs from the committed tape
//! map, and scans without a catalog by validating terminal replicas
//! (REM-PARITY §12.4). It streams every recovered object through digest
//! verification. Live Linux SG and the in-memory SSC model differ only in
//! their `DrillTransportFactory` implementations; [`run_drill`] owns the
//! complete workflow.

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
    stream_rem_tar_object, FormatError, RemTarEntrySink, RemTarFileSpec, RemTarObjectOptions,
    RemTarStreamEntry,
};
use remanence_library::{DriveHandle, DriveHandleSource, SgTransport};
use remanence_parity::{
    default_scheme_for_block_size, index_separation_records, read_terminal_index_inventory,
    scan_reconstruct_filemark_map_with_report, DriveHandleRawSource, FilemarkMap,
    ObjectParitySource, OpenTrust, ParityAuditHook, ParityScheme, RecoveryEvent, RecoveryOutcome,
    ScanDamageKind, ScopedFilemarkMap, TapeFileKind, TapeFileMapEntry, TapeFilePosition,
    TapeIndexReplicaFileKind, TapeIndexReplicaMapEntry, TerminalInventoryOutcome,
    DEFAULT_INDEX_SEPARATION_BYTES,
};
use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const MIB: u64 = 1024 * 1024;
// The hardware drill is intentionally conservative across the supported LTO-7
// and newer validation cartridges. Pool writes use the selected cartridge's
// detected capacity (and optional downward cap) instead.
const DRILL_CAPACITY_BYTES: u64 = 6_000_000_000_000;
const DRILL_UUID_PREFIX: &[u8; 6] = b"RMDL1!";
const DRILL_SEED: &[u8] = b"remanence-freeze-drill-v1";
const DRILL_TIMESTAMP: &str = "2026-01-01T00:00:00Z";
const OBJECT_COUNT: usize = 2;
const DRILL_GAP_RECORDS: u64 = 3;

/// Fixed block sizes admitted by the REM-PARITY freeze criterion 4 steering vehicle.
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
    /// Make terminal replica C unreadable and require B/A fallback.
    TerminalReplicaC,
    /// Combine the maximal supported set of damage classes for the drill.
    Combined,
}

impl DamagePlan {
    #[cfg(test)]
    const ALL: [Self; 5] = [
        Self::BootstrapCopy0,
        Self::SidecarHead,
        Self::ObjectSpan,
        Self::TerminalReplicaC,
        Self::Combined,
    ];
}

#[derive(Clone, Debug)]
pub(crate) struct DrillSettings {
    block_size: u32,
    data_mib: u64,
    default_index_separation: bool,
    gap_records: u64,
    damage_plan: DamagePlan,
    unreadable_bot_ack: bool,
    scheme: ParityScheme,
}

impl DrillSettings {
    fn live(
        block_size: u32,
        data_mib: u64,
        default_index_separation: bool,
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
        let gap_records = if default_index_separation {
            index_separation_records(block_size, DEFAULT_INDEX_SEPARATION_BYTES)
                .map_err(|error| format!("derive default index separation: {error}"))?
        } else {
            DRILL_GAP_RECORDS
        };
        Ok(Self {
            block_size,
            data_mib,
            default_index_separation,
            gap_records,
            damage_plan,
            unreadable_bot_ack,
            scheme: default_scheme_for_block_size(block_size),
        })
    }
}

/// One planned read fault and why that physical record was selected.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct FaultedBlock {
    #[serde(serialize_with = "serialize_decimal_u64")]
    lba: u64,
    role: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct ScanSummary {
    #[serde(serialize_with = "serialize_decimal_u64")]
    bootstrap_generation_used: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    bootstrap_tape_file_number: u64,
    overlay_source: &'static str,
    terminal_replica_fallback_performed: bool,
    damaged_regions: Vec<ScanDamageReport>,
}

#[derive(Clone, Debug, Serialize)]
struct ScanDamageReport {
    #[serde(serialize_with = "serialize_decimal_u64")]
    start_lba: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    block_count: u64,
    kind: &'static str,
}

#[derive(Clone, Debug, Default, Serialize)]
struct ObjectVerificationSummary {
    #[serde(serialize_with = "serialize_decimal_u64")]
    total: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    verified: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    failed: u64,
    failures: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
struct PhaseDurationsMs {
    #[serde(serialize_with = "serialize_decimal_u64")]
    write: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    damage: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    recovery: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    total: u64,
}

/// Stable machine-readable result written by `rem-debug tape freeze-drill`.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct FreezeDrillReport {
    report_version: u32,
    tape_uuid: String,
    block_size_bytes: u32,
    #[serde(serialize_with = "serialize_decimal_u64")]
    data_mib: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    data_bytes: u64,
    terminal_gap_profile: &'static str,
    #[serde(serialize_with = "serialize_decimal_u64")]
    index_separation_bytes: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    index_separation_records: u64,
    compression_verified_off: bool,
    damage_plan: DamagePlan,
    faulted_blocks: Vec<FaultedBlock>,
    #[serde(serialize_with = "serialize_decimal_u64_slice")]
    observed_fault_lbas: Vec<u64>,
    scan: ScanSummary,
    #[serde(serialize_with = "serialize_decimal_u64")]
    blocks_reconstructed: u64,
    objects: ObjectVerificationSummary,
    expectations_met: bool,
    expectation_failures: Vec<String>,
    wall_clock_ms: PhaseDurationsMs,
    success: bool,
}

fn serialize_decimal_u64<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

fn serialize_decimal_u64_slice<S>(values: &[u64], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    values
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .serialize(serializer)
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

#[derive(Clone, Debug)]
struct ExpectedObject {
    object_id: String,
    payload_path: String,
    payload_size: u64,
    payload_sha256: [u8; 32],
    tape_file_number: u64,
    stored_block_count: u64,
}

struct WrittenDrill {
    tape_uuid: [u8; 16],
    expected_objects: Vec<ExpectedObject>,
    map: FilemarkMap,
    terminal_replica_c_tape_file: u64,
    first_sidecar_header_block_count: u64,
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

fn filemark_entry_from_terminal(entry: &TapeIndexReplicaMapEntry) -> TapeFileMapEntry {
    TapeFileMapEntry {
        tape_file_number: entry.tape_file_number,
        kind: match entry.kind {
            TapeIndexReplicaFileKind::Object => TapeFileKind::Object,
            TapeIndexReplicaFileKind::ParitySidecar => TapeFileKind::ParitySidecar,
            TapeIndexReplicaFileKind::Bootstrap => TapeFileKind::Bootstrap,
            TapeIndexReplicaFileKind::ParityMap => TapeFileKind::ParityMap,
            TapeIndexReplicaFileKind::TapeIndexReplica => TapeFileKind::TapeIndexReplica,
            TapeIndexReplicaFileKind::IndexSeparationExtent => TapeFileKind::IndexSeparationExtent,
        },
        block_count: entry.block_count,
        first_parity_data_ordinal: entry.first_parity_data_ordinal,
        protected_ordinal_start: entry.protected_ordinal_start,
        protected_ordinal_end_exclusive: entry.protected_ordinal_end_exclusive,
        epoch_id: entry.epoch_id,
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

    let sizes = payload_sizes(settings.data_mib)?;
    let mut objects = Vec::with_capacity(OBJECT_COUNT);
    for (object_index, payload_size) in sizes.into_iter().enumerate() {
        let mut options = RemTarObjectOptions::new(
            deterministic_uuid(settings.block_size, object_index, b"object"),
            format!("freeze-drill-{object_index}"),
            DRILL_TIMESTAMP,
            deterministic_uuid(settings.block_size, object_index, b"manifest"),
        );
        options.chunk_size = settings.block_size as usize;
        let spec = RemTarFileSpec::new(
            format!("freeze-drill/object-{object_index}.bin"),
            deterministic_uuid(settings.block_size, object_index, b"file"),
            payload_size,
            deterministic_digest(settings.block_size, object_index, payload_size)?,
        );
        let block_size = settings.block_size;
        objects.push(crate::tape_image::TapeImageObject {
            options,
            files: vec![crate::tape_image::TapeImageFile {
                spec,
                open: Arc::new(move || {
                    Box::new(DeterministicPayload::new(
                        block_size,
                        object_index,
                        payload_size,
                    ))
                }),
            }],
        });
    }
    let mut edition_hasher = Sha256::new();
    edition_hasher.update(DRILL_SEED);
    edition_hasher.update(tape_uuid);
    edition_hasher.update(b"terminal-index-edition");
    let mut edition_id = [0u8; 16];
    edition_id.copy_from_slice(&edition_hasher.finalize()[..16]);
    let image = crate::tape_image::write_tape_image(
        drive,
        crate::tape_image::TapeImageInputs {
            tape_uuid,
            scheme: settings.scheme.clone(),
            block_size: settings.block_size,
            objects,
            checkpoint_after_objects: vec![0],
            nominal_extent_bytes: settings
                .gap_records
                .checked_mul(u64::from(settings.block_size))
                .ok_or_else(|| "drill separation bytes overflow".to_string())?,
            writer_identity: remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION")),
            edition_id,
            edition_sequence: 1,
            parity_map_sequence_start: 0,
            directory_flags: remanence_parity::SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD
                | remanence_parity::SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
            diagnostic_keys_present: true,
            capacity_bytes: DRILL_CAPACITY_BYTES,
            stop: crate::tape_image::TapeImageStop::Finalized,
        },
    )?;
    let expected_objects = image
        .inputs
        .objects
        .iter()
        .zip(image.object_rows)
        .map(|(object, recovery_row)| {
            let file = &object.files[0].spec;
            Ok(ExpectedObject {
                object_id: object.options.object_id.clone(),
                payload_path: file.path.clone(),
                payload_size: file.size_bytes,
                payload_sha256: file
                    .file_sha256
                    .ok_or_else(|| "drill file has no payload digest".to_string())?,
                tape_file_number: recovery_row.tape_file_number,
                stored_block_count: recovery_row.stored_block_count,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(WrittenDrill {
        tape_uuid,
        expected_objects,
        map: image.map,
        terminal_replica_c_tape_file: image.terminal_plan.expect("finalized drill image").replicas
            [2]
        .component
        .planned_tape_file_number,
        first_sidecar_header_block_count: image
            .sidecars
            .first()
            .ok_or_else(|| "drill emitted no parity sidecar".to_string())?
            .sidecar_header_block_count,
    })
}

fn physical_lba(
    map: &FilemarkMap,
    tape_file_number: u64,
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
    // Keep the combined plan maximal while leaving the independently tested
    // sole BOT copy intact. The terminal-replica-c leg proves B/A fallback.
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
        for offset in 0..header_blocks {
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
    if includes(DamagePlan::TerminalReplicaC) {
        faults.push(FaultedBlock {
            lba: physical_lba(&written.map, written.terminal_replica_c_tape_file, 0)?,
            role: "terminal_replica_c",
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
    let mut terminal_entries = Vec::new();
    let mut terminal_rows = Vec::new();
    let inventory = read_terminal_index_inventory(
        &mut raw,
        &written.tape_uuid,
        settings.block_size,
        |entry| {
            terminal_entries.push(entry.clone());
            Ok(())
        },
        |row| {
            terminal_rows.push(row.clone());
            Ok(())
        },
    )
    .map_err(|error| format!("read terminal recovery index: {error}"))?;
    let selection = match inventory {
        TerminalInventoryOutcome::Inventory(selection) => selection,
        TerminalInventoryOutcome::BotStructuralRecoveryRequired(required) => {
            return Err(format!(
                "freeze-drill terminal recovery index has no surviving replica: {required:?}"
            ))
        }
    };
    let terminal_map = FilemarkMap::new(
        terminal_entries
            .iter()
            .map(filemark_entry_from_terminal)
            .collect(),
    )
    .map_err(|error| format!("build terminal recovery map: {error}"))?;
    if terminal_rows.len() != written.expected_objects.len() {
        return Err(format!(
            "terminal recovery index has {} Object rows, expected {}",
            terminal_rows.len(),
            written.expected_objects.len()
        ));
    }
    for expected in &written.expected_objects {
        let row = terminal_rows
            .iter()
            .find(|row| row.tape_file_number == expected.tape_file_number)
            .ok_or_else(|| {
                format!(
                    "terminal recovery index omits Object tape file {}",
                    expected.tape_file_number
                )
            })?;
        if row.object_id != expected.object_id.as_bytes()
            || row.stored_block_count != expected.stored_block_count
        {
            return Err(format!(
                "terminal recovery row for tape file {} disagrees with the written Object",
                expected.tape_file_number
            ));
        }
    }
    let terminal_replica_fallback_performed = selection.selected_replica_ordinal != 3;
    let scan = ScanSummary {
        bootstrap_generation_used: 0,
        bootstrap_tape_file_number: 0,
        overlay_source: selected_terminal_replica_name(selection.selected_replica_ordinal),
        terminal_replica_fallback_performed,
        damaged_regions: walk
            .damaged_regions
            .iter()
            .map(|region| ScanDamageReport {
                start_lba: region.start.lba,
                block_count: region.block_count,
                kind: match region.kind {
                    ScanDamageKind::UnreadableTapeFileHead => "unreadable_tape_file_head",
                    ScanDamageKind::ClassificationCountMismatch => "classification_count_mismatch",
                    ScanDamageKind::ClassificationTailMismatch => "classification_tail_mismatch",
                    ScanDamageKind::InvalidTerminalControl => "invalid_terminal_control",
                    ScanDamageKind::WrongLengthTapeFileHead => "wrong_length_tape_file_head",
                },
            })
            .collect(),
    };

    let counter = Arc::new(RecoveryCounter::default());
    let read_scheme = settings.scheme.clone();
    let scoped_map = ScopedFilemarkMap::from_catalog(
        terminal_map,
        selection.edition.descriptor.scope.highest_protected_ordinal,
    );

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
                scoped_map.clone(),
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
    if settings.damage_plan == DamagePlan::TerminalReplicaC && !terminal_replica_fallback_performed
    {
        expectation_failures
            .push("terminal-replica-c plan did not force terminal replica fallback".to_string());
    }
    if settings.damage_plan == DamagePlan::Combined && !terminal_replica_fallback_performed {
        expectation_failures
            .push("combined plan did not force terminal replica fallback".to_string());
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

fn selected_terminal_replica_name(ordinal: u16) -> &'static str {
    match ordinal {
        1 => "terminal_index_replica_a",
        2 => "terminal_index_replica_b",
        3 => "terminal_index_replica_c",
        _ => "terminal_index_replica_unknown",
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
        terminal_gap_profile: if settings.default_index_separation {
            "default_1gib"
        } else {
            "compact"
        },
        index_separation_bytes: settings
            .gap_records
            .checked_mul(u64::from(settings.block_size))
            .ok_or_else(|| "report separation byte count overflows u64".to_string())?,
        index_separation_records: settings.gap_records,
        // The raw writer returns success only after MODE SENSE confirms this
        // exact write precondition.
        compression_verified_off: true,
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
    pub(crate) default_index_separation: bool,
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
            request.default_index_separation,
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
    use serde_json::Value;

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
                        default_index_separation: false,
                        gap_records: DRILL_GAP_RECORDS,
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
                assert_eq!(report.scan.bootstrap_generation_used, 0);
                assert_eq!(report.scan.bootstrap_tape_file_number, 0);
                let expected_overlay = if matches!(
                    damage_plan,
                    DamagePlan::TerminalReplicaC | DamagePlan::Combined
                ) {
                    "terminal_index_replica_b"
                } else {
                    "terminal_index_replica_c"
                };
                assert_eq!(report.scan.overlay_source, expected_overlay);
                assert_eq!(
                    report.scan.terminal_replica_fallback_performed,
                    matches!(
                        damage_plan,
                        DamagePlan::TerminalReplicaC | DamagePlan::Combined
                    )
                );
                if damage_plan == DamagePlan::TerminalReplicaC {
                    assert_eq!(
                        report
                            .faulted_blocks
                            .iter()
                            .map(|fault| fault.role)
                            .collect::<BTreeSet<_>>(),
                        BTreeSet::from(["terminal_replica_c"])
                    );
                }
                if damage_plan == DamagePlan::Combined {
                    let roles = report
                        .faulted_blocks
                        .iter()
                        .map(|fault| fault.role)
                        .collect::<BTreeSet<_>>();
                    assert!(!roles.contains("bootstrap_copy0"));
                    assert!(roles.contains("terminal_replica_c"));
                    assert!(roles.contains("sidecar_head"));
                    assert!(roles.contains("object_span"));
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
    fn drill_write_has_one_bot_and_sidecar_only_checkpoint_before_terminal_tail() {
        let block_size = 262_144;
        let mut factory = ModelTransportFactory::new(block_size);
        let mut drive = factory.open_clean().expect("open clean model drive");
        let written = write_drill_tape(
            &mut drive,
            &DrillSettings {
                block_size,
                data_mib: 1,
                default_index_separation: false,
                gap_records: DRILL_GAP_RECORDS,
                damage_plan: DamagePlan::TerminalReplicaC,
                unreadable_bot_ack: false,
                scheme: test_scheme(block_size),
            },
        )
        .expect("write sole-BOT drill tape");
        let bootstraps = written
            .map
            .entries()
            .iter()
            .filter(|entry| entry.kind == TapeFileKind::Bootstrap)
            .collect::<Vec<_>>();
        assert_eq!(bootstraps.len(), 1);
        assert_eq!(bootstraps[0].tape_file_number, 0);
        assert!(written
            .map
            .entries()
            .iter()
            .any(|entry| entry.kind == TapeFileKind::ParitySidecar));
        assert_eq!(
            written
                .map
                .entries()
                .iter()
                .filter(|entry| entry.kind == TapeFileKind::TapeIndexReplica)
                .count(),
            3
        );
        assert_eq!(
            written
                .map
                .entries()
                .iter()
                .filter(|entry| entry.kind == TapeFileKind::IndexSeparationExtent)
                .count(),
            2
        );
        assert_eq!(
            written.map.entries()[written.terminal_replica_c_tape_file as usize].kind,
            TapeFileKind::TapeIndexReplica
        );
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
            default_index_separation: false,
            gap_records: DRILL_GAP_RECORDS,
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
        assert!(DrillSettings::live(262_144, 0, false, DamagePlan::ObjectSpan, false).is_err());
        assert!(DrillSettings::live(4096, 1, false, DamagePlan::ObjectSpan, false).is_err());
        assert_eq!(
            DrillSettings::live(262_144, 1, true, DamagePlan::ObjectSpan, false)
                .expect("default separation settings")
                .gap_records,
            4096
        );
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
            "--default-index-separation",
            "--damage-plan",
            "terminal-replica-c",
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
        assert!(args.default_index_separation);
        assert_eq!(args.damage_plan, DamagePlan::TerminalReplicaC);
        assert!(args.yes_i_know_scratch);
        assert_eq!(args.report, Path::new("/tmp/freeze-report.json"));
    }

    #[test]
    fn freeze_drill_json_preserves_all_u64_values_as_decimal_strings() {
        let report = FreezeDrillReport {
            report_version: 1,
            tape_uuid: "00000000-0000-4000-8000-000000000000".to_string(),
            block_size_bytes: 262_144,
            data_mib: u64::MAX,
            data_bytes: u64::MAX,
            terminal_gap_profile: "default_1gib",
            index_separation_bytes: u64::MAX,
            index_separation_records: u64::MAX,
            compression_verified_off: true,
            damage_plan: DamagePlan::Combined,
            faulted_blocks: vec![FaultedBlock {
                lba: u64::MAX,
                role: "object_span",
            }],
            observed_fault_lbas: vec![u64::MAX],
            scan: ScanSummary {
                bootstrap_generation_used: u64::MAX,
                bootstrap_tape_file_number: u64::MAX,
                overlay_source: "structural_walk",
                terminal_replica_fallback_performed: true,
                damaged_regions: vec![ScanDamageReport {
                    start_lba: u64::MAX,
                    block_count: u64::MAX,
                    kind: "unreadable_tape_file_head",
                }],
            },
            blocks_reconstructed: u64::MAX,
            objects: ObjectVerificationSummary {
                total: u64::MAX,
                verified: u64::MAX,
                failed: u64::MAX,
                failures: Vec::new(),
            },
            expectations_met: true,
            expectation_failures: Vec::new(),
            wall_clock_ms: PhaseDurationsMs {
                write: u64::MAX,
                damage: u64::MAX,
                recovery: u64::MAX,
                total: u64::MAX,
            },
            success: true,
        };

        let json = serde_json::to_value(report).expect("freeze-drill report serializes");
        let maximum = Value::String(u64::MAX.to_string());
        for pointer in [
            "/data_mib",
            "/data_bytes",
            "/index_separation_bytes",
            "/index_separation_records",
            "/faulted_blocks/0/lba",
            "/observed_fault_lbas/0",
            "/scan/bootstrap_generation_used",
            "/scan/bootstrap_tape_file_number",
            "/scan/damaged_regions/0/start_lba",
            "/scan/damaged_regions/0/block_count",
            "/blocks_reconstructed",
            "/objects/total",
            "/objects/verified",
            "/objects/failed",
            "/wall_clock_ms/write",
            "/wall_clock_ms/damage",
            "/wall_clock_ms/recovery",
            "/wall_clock_ms/total",
        ] {
            assert_eq!(json.pointer(pointer), Some(&maximum), "{pointer}");
        }
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
