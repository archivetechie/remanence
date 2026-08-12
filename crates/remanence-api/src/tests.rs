use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::Read as _;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use ciborium::value::Value as CborValue;
use remanence_aead::{RecipientPrivateKey, RecipientPublicKey};
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record as VirtualRecord, VirtualTape, VirtualWorld,
};
use remanence_format::{read_encrypted_rem_object, read_rem_tar_object};
use remanence_format_driver::{
    ArchiveEventSink, ArchiveReader, FileDataSink, FileId, FileStreamReport, ForeignFormatAdapter,
    FormatCapabilities, FormatDescriptor, ScanReport, StreamReport,
};
use remanence_library::scsi::{DeviceType, Inquiry};
use remanence_library::{
    BlockSink, DiscoveryReport, DriveBay, ElementLayout, IdentitySource, IePort, InstalledDrive,
    Library, Slot, TapeIoError, TapePosition, VecBlockSink, VecBlockSource, WriteBatchOutcome,
    WriteFilemarksOutcome, WriteOutcome,
};
use remanence_parity::bootstrap::{parse_bootstrap_block, write_bootstrap_block, BootstrapPayload};
use remanence_parity::{
    CommittedBundle, CommittedBundleKind, CommittedState, ParityConfig, ParityScheme, SchemeId,
    TapeFileEntry, TapeFileKind,
};
use remanence_state::{
    watermark_floor_bytes, AuditActor, AuditEvent, AuditRecord, AuditSubject,
    ForeignArchiveProjectionInput, NativeObjectCopyProjectionInput,
    NativeObjectFileProjectionInput, NativeObjectProjectionInput, PoolSelectionPolicyName,
    ProvisionTapeInput, RetireTapeInput, SourceLayer, TapeJournalIndexInput,
    TapePoolProjectionInput, OBJECT_COPY_REPRESENTATION_ENCRYPTED,
    OBJECT_COPY_REPRESENTATION_PLAINTEXT,
};
use remanence_stream::{restore_object_to_directory, FilesystemRestoreOptions};
use sha2::{Digest, Sha256};
use tokio_stream::StreamExt;
use tracing::dispatcher::Dispatch;
use tracing::field::{Field, Visit};
use tracing::metadata::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};

use super::*;
use crate::append_request::*;
use crate::audit_projection::*;
use crate::audit_query_service::*;
use crate::catalog_conversion::*;
use crate::drive_collection::*;
use crate::live_status::*;
use crate::read_session_service::*;
use crate::startup_checkpoint::*;
use crate::startup_guard::*;
use crate::startup_media_readiness::*;
use crate::write_session_ingress::*;

const OBJECT_ID_TEXT: &str = "11111111-1111-1111-1111-111111111111";
const OPERATION_ID_TEXT: &str = "22222222-2222-2222-2222-222222222222";
const TAPE_UUID: [u8; 16] = [3u8; 16];
const POOL_WRITE_TAPE_UUID: [u8; 16] = [4u8; 16];

#[test]
fn finalization_progress_uses_wire_tag_11_not_legacy_health_tag_5() {
    use prost::Message as _;

    let encoded = pb::TapeFinalization {
        replica_progress: vec![pb::TapeIndexReplicaProgress {
            replica_ordinal: 1,
            state: pb::tape_index_replica_progress::State::TapeIndexReplicaProgressStatePending
                as i32,
            detail: String::new(),
        }],
        ..Default::default()
    }
    .encode_to_vec();
    assert_eq!(encoded.first(), Some(&0x5A), "field 11 wire key");
    assert!(!encoded.contains(&0x2A), "legacy field 5 wire key");
}

#[test]
fn recovery_progress_marks_only_the_replica_currently_in_flight_unknown() {
    use pb::tape_index_replica_progress::State as ReplicaState;
    use remanence_state::{
        TerminalFinalizationOutcome, TerminalFinalizationProgress, TerminalFinalizationProjection,
        TerminalFinalizationTrigger,
    };

    let cases = [
        (
            TerminalFinalizationProgress::BeforeReplicaA,
            [
                ReplicaState::TapeIndexReplicaProgressStateCompletionUnknown,
                ReplicaState::TapeIndexReplicaProgressStatePending,
                ReplicaState::TapeIndexReplicaProgressStatePending,
            ],
        ),
        (
            TerminalFinalizationProgress::AfterReplicaA,
            [
                ReplicaState::TapeIndexReplicaProgressStateBarrierProved,
                ReplicaState::TapeIndexReplicaProgressStatePending,
                ReplicaState::TapeIndexReplicaProgressStatePending,
            ],
        ),
        (
            TerminalFinalizationProgress::AfterSeparationAb,
            [
                ReplicaState::TapeIndexReplicaProgressStateBarrierProved,
                ReplicaState::TapeIndexReplicaProgressStateCompletionUnknown,
                ReplicaState::TapeIndexReplicaProgressStatePending,
            ],
        ),
        (
            TerminalFinalizationProgress::AfterReplicaB,
            [
                ReplicaState::TapeIndexReplicaProgressStateBarrierProved,
                ReplicaState::TapeIndexReplicaProgressStateBarrierProved,
                ReplicaState::TapeIndexReplicaProgressStatePending,
            ],
        ),
        (
            TerminalFinalizationProgress::AfterSeparationBc,
            [
                ReplicaState::TapeIndexReplicaProgressStateBarrierProved,
                ReplicaState::TapeIndexReplicaProgressStateBarrierProved,
                ReplicaState::TapeIndexReplicaProgressStateCompletionUnknown,
            ],
        ),
        (
            TerminalFinalizationProgress::AfterReplicaC,
            [ReplicaState::TapeIndexReplicaProgressStateBarrierProved; 3],
        ),
    ];
    for (progress, expected) in cases {
        let status = tape_finalization_to_proto(
            TAPE_UUID,
            None,
            TerminalFinalizationProjection {
                trigger: TerminalFinalizationTrigger::ReachedLowWatermark,
                operation_id: None,
                progress,
                edition_digest: [0xA1; 32],
                layout_digest: [0xA2; 32],
                completed_replicas: progress.completed_replicas(),
                outcome: TerminalFinalizationOutcome::RecoveryRequired,
            },
        );
        assert_eq!(
            status
                .replica_progress
                .iter()
                .map(|row| ReplicaState::try_from(row.state).expect("known replica progress"))
                .collect::<Vec<_>>(),
            expected,
            "{progress:?}",
        );
    }
}

#[test]
fn native_file_proto_preserves_u64_chunk_count() {
    let chunk_count = u64::from(u32::MAX) + 1;
    let record = NativeObjectFileRecord {
        object_id: OBJECT_ID_TEXT.to_string(),
        file_id: "large-file".to_string(),
        path: "large.bin".to_string(),
        size_bytes: 0,
        file_sha256: vec![0x5a; 32],
        file_digest_algorithm: "sha256".to_string(),
        first_chunk_lba: Some(0),
        chunk_count,
        mtime: None,
        executable: None,
    };

    let proto = native_object_file_to_proto(record).expect("valid file record");

    assert_eq!(proto.chunk_count, chunk_count);
}

#[test]
fn rolling_drive_rate_uses_only_the_last_five_seconds() {
    let started_at = Instant::now();
    let mut window = RollingByteWindow::new(started_at);
    window.record_at(5_000, started_at + Duration::from_secs(1));
    window.record_at(5_000, started_at + Duration::from_secs(4));

    assert_eq!(
        window.bytes_per_second_at(started_at + Duration::from_secs(5)),
        2_000
    );
    assert_eq!(
        window.bytes_per_second_at(started_at + Duration::from_millis(6_100)),
        1_000
    );
    assert_eq!(
        window.bytes_per_second_at(started_at + Duration::from_millis(9_100)),
        0
    );
}

#[test]
fn mount_age_resets_on_barcode_change_or_empty_bay() {
    let state = LiveStatusState::new(Duration::from_secs(1));
    let started_at = Instant::now();
    let mut drive = pb::Drive {
        element_address: Some(0x0100),
        loaded_tape_barcode: Some("A00001L9".to_string()),
        ..pb::Drive::default()
    };

    state.observe_mount_at("mainlib", &mut drive, started_at);
    state.observe_mount_at("mainlib", &mut drive, started_at + Duration::from_secs(83));
    assert_eq!(drive.mount_age_seconds, Some(83));

    // A different barcode is a NEW mount, seated just now: age zero, and
    // zero here is a real measurement of a real cartridge.
    drive.loaded_tape_barcode = Some("A00002L9".to_string());
    state.observe_mount_at("mainlib", &mut drive, started_at + Duration::from_secs(84));
    assert_eq!(drive.mount_age_seconds, Some(0));

    // An empty bay has no mount, so it has no age. This used to assert 0,
    // which made an empty bay indistinguishable from a cartridge that had
    // just been seated -- the two lines above and below reported the same
    // number for opposite situations.
    drive.loaded_tape_barcode = None;
    state.observe_mount_at("mainlib", &mut drive, started_at + Duration::from_secs(85));
    assert_eq!(drive.mount_age_seconds, None);
}
const SECOND_POOL_WRITE_TAPE_UUID: [u8; 16] = [5u8; 16];
const API_SESSION_BLOCK_SIZE: u32 = 4096;

fn media_readiness_record(
    operation_id: Uuid,
    library_serial: &str,
    drive_element: i64,
    drive_serial: &str,
    barcode: &str,
) -> MediaReadinessOperationRecord {
    MediaReadinessOperationRecord {
        operation_id: operation_id.to_string(),
        run_id: None,
        library_serial: library_serial.to_string(),
        changer_sg: Some("/dev/sg8".to_string()),
        drive_element,
        drive_sg: Some("/dev/sg11".to_string()),
        drive_serial: Some(drive_serial.to_string()),
        barcode: Some(barcode.to_string()),
        source_slot: Some(0x03ed),
        media_generation: Some(9),
        phase: "readiness_poll".to_string(),
        state: "media_initializing".to_string(),
        dirty_scope: Some("drive+tape".to_string()),
        started_at_utc: "2026-07-06T00:00:00Z".to_string(),
        updated_at_utc: "2026-07-06T00:01:00Z".to_string(),
        deadline_at_utc: None,
        last_cdb_opcode: Some(0),
        last_sense_raw: None,
        last_sense_key: Some(2),
        last_asc: Some(4),
        last_ascq: Some(1),
        last_host_status: None,
        last_driver_status: None,
        target_status: None,
        transport_class: None,
        cancel_source: None,
        signal: None,
        evidence_path: None,
        last_error_json: None,
        quarantine_id: None,
    }
}

#[test]
fn media_readiness_admission_helper_blocks_active_fence() {
    let temp = tempfile::Builder::new()
        .prefix("rem-api-readiness-admission")
        .tempdir()
        .expect("temp dir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
    let operation_id = Uuid::from_u128(0xabc);
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id,
            run_id: None,
            library_serial: "LIB-A".to_string(),
            changer_sg: None,
            drive_element: 0x0002,
            drive_sg: None,
            drive_serial: Some("DRV2".to_string()),
            barcode: Some("AOX032L9".to_string()),
            source_slot: Some(0x03ed),
            media_generation: Some(9),
            phase: "readiness_poll".to_string(),
            state: "media_initializing".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .expect("record readiness operation");
    index
        .record_media_readiness_transition(remanence_state::MediaReadinessTransitionInput {
            operation_id,
            phase: Some("readiness_poll".to_string()),
            state: "media_initializing".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            last_cdb_opcode: Some(0x00),
            last_sense_raw: None,
            last_sense_key: Some(0x02),
            last_asc: Some(0x04),
            last_ascq: Some(0x01),
            last_host_status: None,
            last_driver_status: None,
            target_status: None,
            transport_class: None,
            cancel_source: None,
            signal: None,
            evidence_path: None,
            last_error_json: None,
            quarantine_id: Some("mrq-test".to_string()),
        })
        .expect("record readiness transition");

    let err = ensure_media_readiness_admitted(
        &index,
        "open session",
        "LIB-A",
        Some(0x0002),
        Some("AOX032L9"),
        false,
    )
    .expect_err("active same-drive/barcode fence must block admission");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("mrq-test"), "{err}");
    assert!(err.message().contains("AOX032L9"), "{err}");
    assert!(err.message().contains("media_initializing"), "{err}");

    ensure_media_readiness_admitted(
        &index,
        "open session",
        "LIB-B",
        Some(0x0002),
        Some("AOX032L9"),
        true,
    )
    .expect("different selected library is not blocked");
}

#[test]
fn startup_readiness_plan_requires_verified_drive_and_barcode() {
    let mut library = test_library("LIB-A");
    library.drive_bays[0].loaded = true;
    library.drive_bays[0].loaded_tape = Some("AOX032L9".to_string());
    let operation_id = Uuid::from_u128(0xabc);
    let record = media_readiness_record(operation_id, "LIB-A", 1, "8031BDC7D1", "AOX032L9");

    match startup_media_readiness_probe_plan(&record, &library, operation_id) {
        StartupReadinessPlan::Probe {
            drive_element,
            family,
            ..
        } => {
            assert_eq!(drive_element, 1);
            assert_eq!(family, remanence_library::MediaFamily::Lto9OrLater);
        }
        StartupReadinessPlan::KeepFenced { transition } => {
            panic!("expected probe plan, got fenced transition {transition:?}");
        }
    }

    library.drive_bays[0].loaded_tape = Some("OTHERL9".to_string());
    match startup_media_readiness_probe_plan(&record, &library, operation_id) {
        StartupReadinessPlan::KeepFenced { transition } => {
            assert_eq!(transition.state, "aborted_unknown");
            assert_eq!(
                transition.dirty_scope.as_deref(),
                Some("selected-library-snapshot")
            );
            assert!(
                transition
                    .last_error_json
                    .as_deref()
                    .unwrap_or_default()
                    .contains("expected barcode AOX032L9"),
                "{transition:?}"
            );
        }
        StartupReadinessPlan::Probe { .. } => panic!("barcode mismatch must not probe TUR"),
    }

    let mut missing_barcode =
        media_readiness_record(operation_id, "LIB-A", 1, "8031BDC7D1", "AOX032L9");
    missing_barcode.barcode = None;
    library.drive_bays[0].loaded_tape = Some("AOX032L9".to_string());
    match startup_media_readiness_probe_plan(&missing_barcode, &library, operation_id) {
        StartupReadinessPlan::KeepFenced { transition } => {
            assert_eq!(transition.state, "aborted_unknown");
            assert!(
                transition
                    .last_error_json
                    .as_deref()
                    .unwrap_or_default()
                    .contains("no barcode"),
                "{transition:?}"
            );
        }
        StartupReadinessPlan::Probe { .. } => {
            panic!("missing barcode must remain fenced without TUR")
        }
    }
}

#[test]
fn startup_readiness_plan_preserves_release_required_prior_state() {
    let mut library = test_library("LIB-A");
    library.drive_bays[0].loaded = true;
    library.drive_bays[0].loaded_tape = Some("AOX032L9".to_string());
    let operation_id = Uuid::from_u128(0xabc);
    let mut record = media_readiness_record(operation_id, "LIB-A", 1, "8031BDC7D1", "AOX032L9");
    record.state = "transport_unknown".to_string();
    record.dirty_scope = Some("selected-library-snapshot".to_string());
    record.last_error_json = Some("{\"detail\":\"DID_TIME_OUT\"}".to_string());
    record.quarantine_id = Some("mrq-existing".to_string());

    match startup_media_readiness_probe_plan(&record, &library, operation_id) {
        StartupReadinessPlan::KeepFenced { transition } => {
            assert_eq!(
                transition.phase.as_deref(),
                Some("startup_reconcile_requires_release")
            );
            assert_eq!(transition.state, "transport_unknown");
            assert_eq!(
                transition.dirty_scope.as_deref(),
                Some("selected-library-snapshot")
            );
            assert_eq!(transition.quarantine_id.as_deref(), Some("mrq-existing"));
            assert!(
                transition
                    .last_error_json
                    .as_deref()
                    .unwrap_or_default()
                    .contains("requires operator release"),
                "{transition:?}"
            );
        }
        StartupReadinessPlan::Probe { .. } => {
            panic!("release-required prior state must not be cleared by startup TUR")
        }
    }
}

#[test]
fn startup_readiness_probe_transition_clears_ready_and_keeps_initializing_fenced() {
    let operation_id = Uuid::from_u128(0xabc);
    let ready = startup_media_readiness_probe_transition(
        operation_id,
        &remanence_library::MediaReadiness::Ready,
    );
    assert_eq!(ready.phase.as_deref(), Some("startup_reconcile_tur"));
    assert_eq!(ready.state, "ready");
    assert_eq!(ready.dirty_scope.as_deref(), Some("none"));
    assert_eq!(ready.quarantine_id, None);

    let initializing = startup_media_readiness_probe_transition(
        operation_id,
        &remanence_library::MediaReadiness::BecomingReady {
            ascq: 0x01,
            media_initializing: true,
        },
    );
    assert_eq!(initializing.state, "media_initializing");
    assert_eq!(initializing.dirty_scope.as_deref(), Some("drive+tape"));
    assert_eq!(initializing.last_sense_key, Some(0x02));
    assert_eq!(initializing.last_asc, Some(0x04));
    assert_eq!(initializing.last_ascq, Some(0x01));
    assert_eq!(initializing.quarantine_id, None);

    let transport = startup_media_readiness_probe_transition(
        operation_id,
        &remanence_library::MediaReadiness::TransportUnknown {
            detail: "DID_TIME_OUT".to_string(),
        },
    );
    assert_eq!(transport.state, "transport_unknown");
    assert_eq!(
        transport.quarantine_id.as_deref(),
        Some("mrq-00000000-0000-0000-0000-000000000abc")
    );
    assert_eq!(transport.transport_class.as_deref(), Some("unknown"));
}

struct WarningCaptureSubscriber {
    messages: Arc<Mutex<Vec<String>>>,
    next_span_id: AtomicU64,
}

impl WarningCaptureSubscriber {
    fn new(messages: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            messages,
            next_span_id: AtomicU64::new(1),
        }
    }
}

struct WarningMessageVisitor {
    message: Option<String>,
}

impl WarningMessageVisitor {
    fn new() -> Self {
        Self { message: None }
    }
}

impl Visit for WarningMessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }
}

impl Subscriber for WarningCaptureSubscriber {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::WARN
    }

    fn new_span(&self, _attrs: &Attributes<'_>) -> Id {
        Id::from_u64(self.next_span_id.fetch_add(1, AtomicOrdering::Relaxed))
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }
        let mut visitor = WarningMessageVisitor::new();
        event.record(&mut visitor);
        if let Some(message) = visitor.message {
            self.messages
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .push(message);
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}

    fn register_callsite(
        &self,
        _metadata: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        // This subscriber is installed only as a thread-local default.  A
        // process-wide `never` cache can otherwise suppress INFO callsites in
        // concurrently running tests that install their own local subscriber.
        tracing::subscriber::Interest::sometimes()
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        None
    }
}

fn capture_warnings<F>(f: F) -> Vec<String>
where
    F: FnOnce(),
{
    let messages = Arc::new(Mutex::new(Vec::new()));
    let subscriber = WarningCaptureSubscriber::new(Arc::clone(&messages));
    tracing::dispatcher::with_default(&Dispatch::new(subscriber), f);
    Arc::try_unwrap(messages)
        .expect("warning capture has one owner")
        .into_inner()
        .expect("warning capture mutex not poisoned")
}

fn test_index() -> CatalogIndex {
    let dir = std::env::temp_dir().join(format!("remanence-api-{}", Uuid::new_v4()));
    CatalogIndex::open(dir.join("state.sqlite")).expect("open test index")
}

fn temp_dir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn recipient_pair(first_epoch: u8) -> (RecipientPrivateKey, Vec<RecipientPublicKey>) {
    let primary = RecipientPrivateKey::new(
        [first_epoch; 16],
        format!("archive-{first_epoch:02x}"),
        [first_epoch.wrapping_add(1); 32],
    )
    .expect("primary recipient key");
    let recovery_epoch = first_epoch.wrapping_add(1);
    let recovery = RecipientPrivateKey::new(
        [recovery_epoch; 16],
        format!("recovery-{recovery_epoch:02x}"),
        [recovery_epoch.wrapping_add(1); 32],
    )
    .expect("recovery recipient key");
    let recipients = vec![
        primary.public_key(0).expect("primary public key"),
        recovery.public_key(1).expect("recovery public key"),
    ];
    (primary, recipients)
}

fn state_with_spool(spool_dir: PathBuf, budget_bytes: u64) -> ApiState {
    let mut state = ApiState::new(test_index());
    state.spool_dir = Some(Arc::new(spool_dir));
    state.spool_budget_bytes = Some(budget_bytes);
    state.io_memory =
        crate::io_memory::IoMemoryReservation::new(budget_bytes).expect("test I/O memory ceiling");
    state
}

fn append_start_message(session_id: Uuid, declared_size_bytes: u64) -> pb::AppendObjectMessage {
    pb::AppendObjectMessage {
        payload: Some(pb::append_object_message::Payload::Start(
            pb::AppendObjectStart {
                session_id: session_id.as_bytes().to_vec(),
                caller_object_id: "caller-object".to_string(),
                caller_metadata: HashMap::new(),
                declared_size_bytes: Some(declared_size_bytes),
                body_format_manifest: None,
                expected_content_sha256: None,
                source_replay_capability: pb::SourceReplayCapability::Unspecified as i32,
                expected_content_digest: None,
            },
        )),
    }
}

fn append_chunk_message(session_id: Uuid, data: Vec<u8>) -> pb::AppendObjectMessage {
    pb::AppendObjectMessage {
        payload: Some(pb::append_object_message::Payload::Chunk(
            pb::AppendObjectChunk {
                session_id: session_id.as_bytes().to_vec(),
                data,
            },
        )),
    }
}

fn append_finish_message(session_id: Uuid, digest: [u8; 32]) -> pb::AppendObjectMessage {
    pb::AppendObjectMessage {
        payload: Some(pb::append_object_message::Payload::Finish(
            pb::AppendObjectFinish {
                session_id: session_id.as_bytes().to_vec(),
                expected_content_sha256: Some(digest.to_vec()),
                expected_content_digest: None,
            },
        )),
    }
}

#[test]
fn object_record_to_proto_carries_append_commit_info() {
    let record = NativeObjectRecord {
        object_id: OBJECT_ID_TEXT.to_string(),
        caller_object_id: Some("caller-object".to_string()),
        body_format: "rem-object-v1".to_string(),
        logical_size_bytes: Some(456),
        content_hash: Some(vec![0x33; 32]),
        content_hash_algorithm: Some(remanence_state::DIGEST_ALGORITHM_SHA256.to_string()),
        metadata_hash: None,
        metadata_hash_algorithm: None,
        created_at_utc: "2026-07-05T00:00:00Z".to_string(),
        copies: vec![NativeObjectCopyRecord {
            object_id: OBJECT_ID_TEXT.to_string(),
            tape_uuid: TAPE_UUID.to_vec(),
            tape_file_number: 2,
            first_body_lba: 7,
            first_parity_data_ordinal: None,
            protected_until_ordinal: None,
            status: "committed".to_string(),
            pool_id: Some("camera.copy-a".to_string()),
            representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
            recipient_epoch_ids: None,
            metadata_frame_len: None,
            plaintext_digest: Some(vec![0x33; 32]),
            plaintext_digest_algorithm: Some(remanence_state::DIGEST_ALGORITHM_SHA256.to_string()),
            stored_digest: Some(vec![0x33; 32]),
            stored_digest_algorithm: Some(remanence_state::DIGEST_ALGORITHM_SHA256.to_string()),
            global_start_block: None,
            global_end_block: None,
        }],
    };

    let proto = object_record_to_proto(record).expect("object record to proto");
    let info = proto
        .append_commit_info
        .expect("append commit info from first copy");
    assert_eq!(info.append_mode, pb::AppendMode::Append as i32);
    assert_eq!(info.tape_uuid, TAPE_UUID.to_vec());
    assert_eq!(info.tape_file_number, Some(2));
    assert_eq!(info.first_body_lba, 7);
    assert_eq!(info.position_before_lba, None);
    assert_eq!(info.position_after_lba, None);
    assert_eq!(info.journal_record_ordinal, None);
    let content_digest = proto.content_digest.expect("content digest pair");
    assert_eq!(content_digest.algorithm, "sha256");
    assert_eq!(content_digest.value, vec![0x33; 32]);
    assert!(proto.metadata_digest.is_none());
    let copy = proto.copies.first().expect("copy");
    assert_eq!(
        copy.plaintext_digest
            .as_ref()
            .map(|digest| digest.algorithm.as_str()),
        Some("sha256")
    );
    assert_eq!(
        copy.stored_digest
            .as_ref()
            .map(|digest| digest.value.as_slice()),
        Some(&[0x33; 32][..])
    );
}

#[test]
fn wire_span_and_extent_keep_absent_distinguishable_from_zero() {
    // Proto3 optional: absent = unknown, never guessed, never defaulted.
    // Block 0 is a valid position, so Some(0) must survive the wire as
    // present-zero — distinguishable from absent.
    let mut copy_record = NativeObjectCopyRecord {
        object_id: OBJECT_ID_TEXT.to_string(),
        tape_uuid: TAPE_UUID.to_vec(),
        tape_file_number: 1,
        first_body_lba: 0,
        first_parity_data_ordinal: None,
        protected_until_ordinal: None,
        status: "committed".to_string(),
        pool_id: None,
        representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
        recipient_epoch_ids: None,
        metadata_frame_len: None,
        plaintext_digest: None,
        plaintext_digest_algorithm: None,
        stored_digest: None,
        stored_digest_algorithm: None,
        global_start_block: None,
        global_end_block: None,
    };
    let absent = object_copy_to_proto(&copy_record);
    assert_eq!(absent.global_start_block, None);
    assert_eq!(absent.global_end_block, None);

    copy_record.global_start_block = Some(0);
    copy_record.global_end_block = Some(3);
    let present = object_copy_to_proto(&copy_record);
    assert_eq!(
        present.global_start_block,
        Some(0),
        "a span starting at block 0 is present-zero, not absent"
    );
    assert_eq!(present.global_end_block, Some(3));
    assert_ne!(present.global_start_block, absent.global_start_block);

    let mut tape_record = writable_tape_record();
    assert_eq!(tape_record.written_extent_lba, None);
    let absent_tape = tape_to_proto(tape_record.clone());
    assert_eq!(absent_tape.written_extent_lba, None);
    tape_record.written_extent_lba = Some(11);
    let present_tape = tape_to_proto(tape_record);
    assert_eq!(present_tape.written_extent_lba, Some(11));
}

#[test]
fn tape_target_shape_requires_the_pool_guard() {
    let base = pb::TapeTarget {
        tape_uuid: Uuid::from_bytes([9; 16]).as_bytes().to_vec(),
        mount_if_needed: true,
        required_pool_id: "camera.copy-a".to_string(),
        allow_unpooled: false,
    };

    let (tape_uuid, pool) = validate_tape_target_shape(&base).expect("valid shape");
    assert_eq!(tape_uuid, [9; 16]);
    assert_eq!(pool, "camera.copy-a");

    // No guard at all: the caller must state an intent.
    let mut no_guard = base.clone();
    no_guard.required_pool_id = String::new();
    let status = validate_tape_target_shape(&no_guard).unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("required_pool_id"), "{status}");

    // allow_unpooled alongside a guard is contradictory.
    let mut both = base.clone();
    both.allow_unpooled = true;
    let status = validate_tape_target_shape(&both).unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("mutually exclusive"), "{status}");

    // allow_unpooled alone is honest but not wired yet.
    let mut unpooled = base.clone();
    unpooled.required_pool_id = String::new();
    unpooled.allow_unpooled = true;
    let status = validate_tape_target_shape(&unpooled).unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unimplemented);

    // mount_if_needed=false is not wired in this slice.
    let mut no_mount = base.clone();
    no_mount.mount_if_needed = false;
    let status = validate_tape_target_shape(&no_mount).unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("mount_if_needed"), "{status}");
}

#[test]
fn append_mode_for_tape_file_number_handles_unset_and_append_files() {
    assert_eq!(
        append_mode_for_tape_file_number(0),
        pb::AppendMode::Unspecified
    );
    assert_eq!(append_mode_for_tape_file_number(1), pb::AppendMode::Fresh);
    assert_eq!(append_mode_for_tape_file_number(2), pb::AppendMode::Append);
}

#[test]
fn object_record_without_append_info_decodes_as_absent() {
    use prost::Message as _;

    let record = pb::ObjectRecord {
        object_id: Uuid::nil().as_bytes().to_vec(),
        caller_object_id: Some(String::new()),
        content_sha256: Some(vec![0x11; 32]),
        logical_size_bytes: Some(10),
        body_format: Some("rem-object-v1".to_string()),
        caller_metadata: Default::default(),
        created_at: None,
        copies: Vec::new(),
        append_commit_info: None,
        content_digest: None,
        metadata_digest: None,
    };
    let mut encoded = Vec::new();
    record.encode(&mut encoded).expect("encode object record");

    let decoded = pb::ObjectRecord::decode(encoded.as_slice()).expect("decode object record");
    assert!(decoded.append_commit_info.is_none());
}

#[tokio::test]
async fn connect_unix_to_missing_socket_fails() {
    let missing = temp_dir("remanence-api-missing-socket").join("nope.sock");
    let result = crate::connect_unix(missing).await;
    assert!(result.is_err(), "connecting to a missing socket must error");
}

#[test]
fn append_spool_cap_clamps_client_declared_size() {
    assert_eq!(
        append_spool_cap(None),
        crate::write_owner::SPOOL_MAX_BYTES,
        "an undeclared size buys the full spool"
    );
    assert_eq!(
        append_spool_cap(Some(0)),
        0,
        "a declared empty object is held to its own declaration, not \
             silently given the largest spool we allow"
    );
    assert_eq!(append_spool_cap(Some(1024)), 1024);
    assert_eq!(
        append_spool_cap(Some(u64::MAX)),
        crate::write_owner::SPOOL_MAX_BYTES
    );
}

#[test]
fn overlap_admission_compatibility_table_routes_deterministically() {
    let digest = [0x44; 32];
    let eligible = pb::AppendObjectStart {
        session_id: Uuid::new_v4().as_bytes().to_vec(),
        caller_object_id: "caller-object".into(),
        caller_metadata: HashMap::new(),
        declared_size_bytes: Some(123),
        body_format_manifest: None,
        expected_content_sha256: Some(digest.to_vec()),
        source_replay_capability: pb::SourceReplayCapability::ReplayFromStart as i32,
        expected_content_digest: None,
    };
    assert!(overlap_append_eligible(
        remanence_state::AppendStagingMode::Overlap,
        &eligible,
        Some(&digest)
    ));
    assert!(!overlap_append_eligible(
        remanence_state::AppendStagingMode::Serial,
        &eligible,
        Some(&digest)
    ));

    let mut missing_id = eligible.clone();
    missing_id.caller_object_id.clear();
    let mut unknown_size = eligible.clone();
    unknown_size.declared_size_bytes = None;
    // Distinct from the above: the caller knows the size and it is zero.
    // There is nothing to overlap, but it is not an unknown.
    let mut declared_empty = eligible.clone();
    declared_empty.declared_size_bytes = Some(0);
    let mut supplied_manifest = eligible.clone();
    supplied_manifest.body_format_manifest = Some(Vec::new());
    let mut no_replay = eligible.clone();
    no_replay.source_replay_capability = pb::SourceReplayCapability::Unspecified as i32;
    for fallback in [
        &missing_id,
        &unknown_size,
        &declared_empty,
        &supplied_manifest,
        &no_replay,
    ] {
        assert!(!overlap_append_eligible(
            remanence_state::AppendStagingMode::Overlap,
            fallback,
            Some(&digest)
        ));
    }
    assert!(!overlap_append_eligible(
        remanence_state::AppendStagingMode::Overlap,
        &eligible,
        None
    ));
}

#[test]
fn algorithm_aware_expected_digest_validates_compatibility_mirror() {
    let digest = [0x45; 32];
    let paired = pb::Digest {
        algorithm: "sha256".to_string(),
        value: digest.to_vec(),
    };
    assert_eq!(
        expected_content_digest(None, Some(&paired)).expect("paired digest"),
        Some(digest)
    );
    assert_eq!(
        expected_content_digest(Some(&digest), Some(&paired)).expect("matching mirrors"),
        Some(digest)
    );

    let mismatch = pb::Digest {
        algorithm: "sha256".to_string(),
        value: vec![0x46; 32],
    };
    assert_eq!(
        expected_content_digest(Some(&digest), Some(&mismatch))
            .expect_err("mismatched mirrors")
            .code(),
        tonic::Code::InvalidArgument
    );
    let unsupported = pb::Digest {
        algorithm: "sha512".to_string(),
        value: vec![0x45; 64],
    };
    assert_eq!(
        expected_content_digest(None, Some(&unsupported))
            .expect_err("unsupported digest algorithm")
            .code(),
        tonic::Code::InvalidArgument
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlap_receiver_preserves_bytes_and_accepts_only_the_binding_digest() {
    let session_id = Uuid::new_v4();
    let payload = b"receiver and REM-OBJECT writer observe the same plaintext".to_vec();
    let digest: [u8; 32] = Sha256::digest(&payload).into();
    let capacity = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
    let (producer, mut consumer, control) =
        crate::append_ring::create_append_ring(&manager, capacity, 90, 25, payload.len() as u64)
            .expect("ring");
    let stream = tokio_stream::iter([
        Ok(append_chunk_message(session_id, payload.clone())),
        Ok(append_finish_message(session_id, digest)),
    ]);
    let receive = tokio::spawn(receive_overlap_messages(
        stream,
        producer,
        session_id,
        payload.len() as u64,
        digest,
        control,
    ));
    let consumed = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        consumer.read_to_end(&mut bytes).expect("consume ring");
        bytes
    });

    let report = receive.await.expect("receive task").expect("valid Finish");
    assert_eq!(report.bytes, payload.len() as u64);
    assert_eq!(report.chunks, 1);
    assert_eq!(consumed.await.expect("consumer task"), payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlap_receiver_rejects_finish_or_observed_digest_disagreement() {
    for mismatch in ["finish", "observed"] {
        let session_id = Uuid::new_v4();
        let payload = format!("payload for {mismatch}").into_bytes();
        let actual_digest: [u8; 32] = Sha256::digest(&payload).into();
        let binding_digest = if mismatch == "observed" {
            [0x7b; 32]
        } else {
            actual_digest
        };
        let finish_digest = if mismatch == "finish" {
            [0x8c; 32]
        } else {
            binding_digest
        };
        let capacity = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
        let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
        let (producer, mut consumer, control) = crate::append_ring::create_append_ring(
            &manager,
            capacity,
            90,
            25,
            payload.len() as u64,
        )
        .expect("ring");
        let stream = tokio_stream::iter([
            Ok(append_chunk_message(session_id, payload.clone())),
            Ok(append_finish_message(session_id, finish_digest)),
        ]);
        let receive = tokio::spawn(receive_overlap_messages(
            stream,
            producer,
            session_id,
            payload.len() as u64,
            binding_digest,
            control,
        ));
        let consumer_task = tokio::task::spawn_blocking(move || {
            let mut bytes = Vec::new();
            consumer
                .read_to_end(&mut bytes)
                .expect_err("invalid Finish reaches consumer")
        });

        let status = receive
            .await
            .expect("receive task")
            .expect_err("digest disagreement must reject");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        let error = consumer_task.await.expect("consumer task");
        let expected = if mismatch == "finish" {
            "disagree"
        } else {
            "payload SHA-256"
        };
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn append_spool_write_error_mapping_keeps_io_errors_distinct() {
    let cap = status_from_append_spool_write_error(io::Error::new(
        io::ErrorKind::InvalidInput,
        "spool size cap exceeded",
    ));
    assert_eq!(cap.code(), tonic::Code::ResourceExhausted);
    assert!(cap.message().contains("cap"));

    let io_error = status_from_append_spool_write_error(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "permission denied",
    ));
    assert_eq!(io_error.code(), tonic::Code::Internal);
    assert!(io_error.message().contains("write append spool"));
}

#[test]
fn append_object_spool_create_failure_returns_cause_and_logs_path() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let temp = tempfile::Builder::new()
        .prefix("remanence-api-spool-create-failure")
        .tempdir()
        .expect("tempdir");
    let spool_dir = temp.path().join("missing").join("spool");
    let service =
        state_with_spool(spool_dir.clone(), APPEND_SPOOL_MAX_BYTES).write_session_service();
    let session_id = Uuid::new_v4();
    let stream = tokio_stream::iter([Ok(append_start_message(session_id, 1))]);

    let mut status = None;
    let warnings = capture_warnings(|| {
        status = Some(
            runtime
                .block_on(service.append_object_stream_logged(stream))
                .expect_err("missing spool dir must fail"),
        );
    });
    let status = status.expect("status captured");
    assert_eq!(status.code(), tonic::Code::Internal);
    assert!(
        status.message().contains("create append spool in"),
        "{status}"
    );
    assert!(
        status.message().contains(&spool_dir.display().to_string()),
        "{status}"
    );
    assert!(
        !status.message().contains("stream closed"),
        "spool-create failure must not collapse to a bare stream close: {status}"
    );
    assert!(
        warnings.iter().any(|message| {
            message.contains("append_object failed")
                && message.contains(&spool_dir.display().to_string())
                && message.contains("create append spool")
        }),
        "append failure log must include spool path and cause, got {warnings:?}"
    );
}

#[test]
fn append_object_refuses_request_beyond_effective_tmpfs_budget() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let temp = tempfile::Builder::new()
        .prefix("remanence-api-spool-budget")
        .tempdir()
        .expect("tempdir");
    let spool_dir = temp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).expect("spool dir");
    let budget = 1024 * 1024;
    let service = state_with_spool(spool_dir.clone(), budget).write_session_service();
    let session_id = Uuid::new_v4();
    let stream = tokio_stream::iter([Ok(append_start_message(session_id, budget + 1))]);

    let status = runtime
        .block_on(service.append_object_stream_logged(stream))
        .expect_err("over-budget append must fail");
    assert_eq!(status.code(), tonic::Code::ResourceExhausted);
    assert!(
        status.message().contains("daemon.spool_tmpfs_ram_budget"),
        "{status}"
    );
    assert!(
        status
            .message()
            .contains("overflow-to-disk is not implemented"),
        "{status}"
    );
    assert!(
        status.message().contains(&spool_dir.display().to_string()),
        "{status}"
    );
}

#[test]
fn actor_from_request_uses_metadata_fallback_when_no_peer_cert() {
    let mut request = Request::new(());
    request
        .metadata_mut()
        .insert("x-remanence-actor", "operator-a".parse().unwrap());

    assert_eq!(
        actor_from_request(&request),
        AuditActor::Service("operator-a".to_string())
    );
}

#[test]
fn actor_from_request_defaults_to_system_without_identity() {
    assert_eq!(actor_from_request(&Request::new(())), AuditActor::System);
}

#[test]
fn manual_finalize_fingerprint_binds_presence_actor_and_exact_reason() {
    let tape_uuid = [0x11; 16];
    let baseline = manual_finalize_request_fingerprint(
        tape_uuid,
        Some("slow-pool"),
        "service:operator-a",
        b"ship copy offsite",
    );
    assert_ne!(
        baseline,
        manual_finalize_request_fingerprint(
            tape_uuid,
            None,
            "service:operator-a",
            b"ship copy offsite",
        )
    );
    assert_ne!(
        baseline,
        manual_finalize_request_fingerprint(
            [0x12; 16],
            Some("slow-pool"),
            "service:operator-a",
            b"ship copy offsite",
        )
    );
    assert_ne!(
        baseline,
        manual_finalize_request_fingerprint(
            tape_uuid,
            Some("other-slow-pool"),
            "service:operator-a",
            b"ship copy offsite",
        )
    );
    assert_ne!(
        baseline,
        manual_finalize_request_fingerprint(
            tape_uuid,
            Some("slow-pool"),
            "service:operator-b",
            b"ship copy offsite",
        )
    );
    assert_ne!(
        baseline,
        manual_finalize_request_fingerprint(
            tape_uuid,
            Some("slow-pool"),
            "service:operator-a",
            b" ship copy offsite ",
        )
    );
    assert_eq!(
        baseline,
        manual_finalize_request_fingerprint(
            tape_uuid,
            Some("slow-pool"),
            "service:operator-a",
            b"ship copy offsite",
        )
    );
}

#[test]
fn manual_finalize_changed_fields_conflict_after_catalog_restart() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-manual-finalize-idempotency-restart")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let actor_fingerprint = "service:operator-a";
    let idempotency_key = Uuid::from_u128(0xf101);
    let operation_id = Uuid::from_u128(0xf102);
    let tape_uuid = [0x11; 16];
    let baseline = manual_finalize_request_fingerprint(
        tape_uuid,
        Some("slow-pool"),
        actor_fingerprint,
        b"ship copy offsite",
    );
    CatalogIndex::open(&index_path)
        .expect("open catalog")
        .register_idempotency_request(
            actor_fingerprint,
            FINALIZE_TAPE_OPERATION_KIND,
            idempotency_key,
            baseline,
            operation_id,
            Some("2026-08-12T00:00:00Z"),
        )
        .expect("register baseline request");

    let changed_fingerprints = [
        manual_finalize_request_fingerprint(
            [0x12; 16],
            Some("slow-pool"),
            actor_fingerprint,
            b"ship copy offsite",
        ),
        manual_finalize_request_fingerprint(
            tape_uuid,
            Some("other-slow-pool"),
            actor_fingerprint,
            b"ship copy offsite",
        ),
        manual_finalize_request_fingerprint(
            tape_uuid,
            Some("slow-pool"),
            actor_fingerprint,
            b"ship the copy offsite",
        ),
    ];
    for changed_fingerprint in changed_fingerprints {
        let conflict = CatalogIndex::open(&index_path)
            .expect("restart catalog")
            .register_idempotency_request(
                actor_fingerprint,
                FINALIZE_TAPE_OPERATION_KIND,
                idempotency_key,
                changed_fingerprint,
                Uuid::new_v4(),
                None,
            )
            .expect_err("changed request must conflict after restart");
        assert!(matches!(conflict, StateError::IdempotencyConflict(_)));
    }
}

#[test]
fn auth_role_parser_accepts_spec_roles_and_subject_prefixes() {
    assert_eq!(parse_client_role("readonly"), Some(ClientRole::Readonly));
    assert_eq!(
        parse_client_role("role:orchestrator"),
        Some(ClientRole::Orchestrator)
    );
    assert_eq!(
        parse_client_role("remanence-role=operator"),
        Some(ClientRole::Operator)
    );
    assert_eq!(
        parse_client_role("Role:Operator"),
        Some(ClientRole::Operator)
    );
    assert_eq!(parse_client_role("admin"), Some(ClientRole::Admin));
    assert_eq!(parse_client_role("system"), Some(ClientRole::System));
    assert_eq!(parse_client_role("writer"), None);
}

#[test]
fn certificate_role_requires_remanence_prefix() {
    assert_eq!(
        parse_certificate_role_attribute("remanence-role=operator"),
        Some(ClientRole::Operator)
    );
    assert_eq!(
        parse_certificate_role_attribute("Remanence-Role:Admin"),
        Some(ClientRole::Admin)
    );
    // A human-chosen subject value must never grant a role from a
    // certificate: bare words and generic prefixes are rejected.
    assert_eq!(parse_certificate_role_attribute("operator"), None);
    assert_eq!(parse_certificate_role_attribute("admin"), None);
    assert_eq!(parse_certificate_role_attribute("role=admin"), None);
    assert_eq!(parse_certificate_role_attribute("role:operator"), None);
}

#[test]
fn auth_role_parser_reads_mtls_certificate_subject() {
    // CN = "remanence-role=orchestrator" — the only certificate
    // form that grants a role.
    const CERT: &[u8] = b"-----BEGIN CERTIFICATE-----
MIICKDCCAZGgAwIBAgIUSC6Pz9m7L+r7OACC/z3EyzxjlukwDQYJKoZIhvcNAQEL
BQAwJjEkMCIGA1UEAwwbcmVtYW5lbmNlLXJvbGU9b3JjaGVzdHJhdG9yMB4XDTI2
MDYxMDA5MzYxM1oXDTI3MDYxMDA5MzYxM1owJjEkMCIGA1UEAwwbcmVtYW5lbmNl
LXJvbGU9b3JjaGVzdHJhdG9yMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDT
Oj3oJ5Mj+bwA9KUTNWM6Sn7085JZJFyWXFYnnTCXGeQKcFB/4hWtNT4RzNOPOuHE
yenUAdnjERB0Q88+ZGiCFW0a7mqVgGvIQ0ALe5hUtDbr1C/L5PVnTPdJL6qx05tW
AFKiFiSgTZCf5jXmUL8ijJk6PwaWsziX78aowc8ahQIDAQABo1MwUTAdBgNVHQ4E
FgQUC6w9intd3BWy5ndUax7FvPuFys0wHwYDVR0jBBgwFoAUC6w9intd3BWy5ndU
ax7FvPuFys0wDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOBgQBHhl8C
ut8itrK85Q5dfBXf9PF+VO2mBDwygxFHq2zGc7h+adH22nDP5O9ruYp0f6CO/YE+
UCR1Of7847/e0wZzH2MZWiSxwbcPPO9IbLLfJcL9+WOZDuLlbJOlSW3fsQjuCK/3
0BJvX603jdLLX35ExjbI9rZf+ljSS7BGLFDHBA==
-----END CERTIFICATE-----
";
    let (_, pem) = x509_parser::pem::parse_x509_pem(CERT).expect("parse pem");

    assert_eq!(
        role_from_certificate_subject(&pem.contents),
        Some(ClientRole::Orchestrator)
    );
}

#[test]
fn auth_role_certificate_ignores_unprefixed_subject_values() {
    // CN = "role:orchestrator" — the generic prefix (and any bare
    // role word) must NOT grant a role from a certificate subject;
    // such a client falls back to the Readonly default.
    const CERT: &[u8] = b"-----BEGIN CERTIFICATE-----
MIICFDCCAX2gAwIBAgIUWUo200SX/lizn4w3+toMUqWGebAwDQYJKoZIhvcNAQEL
BQAwHDEaMBgGA1UEAwwRcm9sZTpvcmNoZXN0cmF0b3IwHhcNMjYwNjEwMDYwOTQ1
WhcNMjYwNjExMDYwOTQ1WjAcMRowGAYDVQQDDBFyb2xlOm9yY2hlc3RyYXRvcjCB
nzANBgkqhkiG9w0BAQEFAAOBjQAwgYkCgYEAvlexVTSFywY/KmuOrb/JcWHZRe+k
+4xTSSoli2GPVtLLbtG20P8M2f3ztgmspofWYEHizDTAazEDUVpuNVMArHxtCYkl
F870VaNGqNLQbuO7RTuxZBdBsPx53r4r9+y98EoMXaIDY9fr+KLHCVbRM95fdVoE
SbZhirGgZDzedZUCAwEAAaNTMFEwHQYDVR0OBBYEFErl+mpvQQw8/j/Wtwleg0Hj
SuTbMB8GA1UdIwQYMBaAFErl+mpvQQw8/j/Wtwleg0HjSuTbMA8GA1UdEwEB/wQF
MAMBAf8wDQYJKoZIhvcNAQELBQADgYEAp1gfrStgB/mqWv9CEp5RN4zzHRK4M52m
Hr4Eecw8Zz+C5rD4eTUvlTEVUuOHsHkXm3/KYkp5Emw3ncNvtjnrc5eKRalaj59Z
hZqLlGKuZLlibfY5VIYyxzQ1tuZlG7PFCKFjOmT8xoY7/nysfaITmwD7JazQPELZ
BCw3Wyv2UWY=
-----END CERTIFICATE-----
";
    let (_, pem) = x509_parser::pem::parse_x509_pem(CERT).expect("parse pem");

    assert_eq!(role_from_certificate_subject(&pem.contents), None);
}

#[test]
fn authorization_allows_readonly_reads_but_denies_writes() {
    let mut request = Request::new(());
    request
        .metadata_mut()
        .insert("x-remanence-role", "readonly".parse().unwrap());

    assert!(authorize_request(&request, AuthPermission::Read).is_ok());
    assert!(authorize_request(&request, AuthPermission::ReadTape).is_ok());
    assert_eq!(
        authorize_request(&request, AuthPermission::Write)
            .expect_err("readonly must not write")
            .code(),
        tonic::Code::PermissionDenied
    );
}

#[test]
fn authorization_denies_operator_write_but_allows_robotics() {
    let mut request = Request::new(());
    request
        .metadata_mut()
        .insert("x-remanence-role", "operator".parse().unwrap());

    assert!(authorize_request(&request, AuthPermission::Robotics).is_ok());
    assert_eq!(
        authorize_request(&request, AuthPermission::Write)
            .expect_err("operator must not write")
            .code(),
        tonic::Code::PermissionDenied
    );
}

#[test]
fn authorization_matrix_covers_drive_stewardship_mutations() {
    let cases = [
        (ClientRole::System, true, true, true),
        (ClientRole::Admin, true, true, false),
        (ClientRole::Orchestrator, true, true, false),
        (ClientRole::Operator, false, true, false),
        (ClientRole::Readonly, false, false, false),
    ];
    for (role, annotate, robotics, lifecycle) in cases {
        assert_eq!(
            role.allows(AuthPermission::Write),
            annotate,
            "{role:?} AnnotateDrive/Write"
        );
        assert_eq!(
            role.allows(AuthPermission::Robotics),
            robotics,
            "{role:?} PollDrive/CleanDrive/AckAlarm/Robotics"
        );
        assert_eq!(
            role.allows(AuthPermission::Lifecycle),
            lifecycle,
            "{role:?} RetireDrive/Lifecycle"
        );
    }
}

#[tokio::test]
async fn write_session_rejects_readonly_role_before_validation() {
    let service = ApiState::new(test_index()).write_session_service();
    let mut request = Request::new(pb::OpenWriteSessionRequest {
        target: None,
        body_format: None,
        idempotency_key: None,
        recover_session_id: None,
    });
    request
        .metadata_mut()
        .insert("x-remanence-role", "readonly".parse().unwrap());

    let err = pb::write_session_service_server::WriteSessionService::open_write_session(
        &service, request,
    )
    .await
    .expect_err("readonly write must be rejected before request validation");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
}

#[test]
fn unimplemented_idempotency_rejects_only_non_empty_keys() {
    reject_unimplemented_idempotency(None, "TestRpc").expect("absent key");
    reject_unimplemented_idempotency(Some(&pb::IdempotencyKey { value: Vec::new() }), "TestRpc")
        .expect("empty key");

    let err =
        reject_unimplemented_idempotency(Some(&pb::IdempotencyKey { value: vec![1] }), "TestRpc")
            .expect_err("malformed key rejected before feature gate");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    let err = reject_unimplemented_idempotency(
        Some(&pb::IdempotencyKey {
            value: Uuid::new_v4().as_bytes().to_vec(),
        }),
        "TestRpc",
    )
    .expect_err("non-empty valid key is not silently accepted");
    assert_eq!(err.code(), tonic::Code::Unimplemented);
}

#[tokio::test]
async fn write_session_rejects_idempotency_key_before_validation() {
    let service = ApiState::new(test_index()).write_session_service();
    let err = pb::write_session_service_server::WriteSessionService::open_write_session(
        &service,
        Request::new(pb::OpenWriteSessionRequest {
            target: None,
            body_format: None,
            idempotency_key: Some(pb::IdempotencyKey {
                value: Uuid::new_v4().as_bytes().to_vec(),
            }),
            recover_session_id: None,
        }),
    )
    .await
    .expect_err("non-enforced idempotency key must fail before dispatch");
    assert_eq!(err.code(), tonic::Code::Unimplemented);
}

/// An absent body_format asks for the daemon default; a present but blank
/// one names no format and is malformed. Before the field carried presence
/// these were the same request on the wire.
#[tokio::test]
async fn write_session_separates_absent_body_format_from_a_blank_one() {
    let service = ApiState::new(test_index()).write_session_service();
    let open = |body_format| {
        pb::write_session_service_server::WriteSessionService::open_write_session(
            &service,
            Request::new(pb::OpenWriteSessionRequest {
                target: None,
                body_format,
                idempotency_key: None,
                recover_session_id: None,
            }),
        )
    };

    let absent = open(None)
        .await
        .expect_err("no target, so the open fails either way");
    assert_eq!(
        absent.code(),
        tonic::Code::InvalidArgument,
        "absent body_format must take the default and fall through to the target check"
    );
    assert!(
        absent.message().contains("target"),
        "absent body_format must not itself be the complaint: {absent}"
    );

    let blank = open(Some("   ".to_string()))
        .await
        .expect_err("a blank body_format is malformed");
    assert_eq!(blank.code(), tonic::Code::InvalidArgument);
    assert!(
        blank.message().contains("body_format"),
        "a supplied-but-blank body_format must be refused by name: {blank}"
    );
}

#[test]
fn cancel_audit_records_request_actor() {
    let state = ApiState::new(test_index());
    let operation_id = Uuid::new_v4();
    let actor = AuditActor::Service("operator-a".to_string());

    state
        .record_cancel_requested(actor.clone(), operation_id, None, false)
        .expect("record cancel");

    let records = FileAuditLog::replay(state.audit_dir.as_ref()).expect("replay audit");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].actor, actor);
    assert_eq!(records[0].event, AuditEvent::CancelRequested);
}

#[test]
fn library_request_audit_records_request_actor() {
    let state = ApiState::new(test_index());
    let operation_id = Uuid::new_v4();
    let actor = AuditActor::Service("operator-a".to_string());

    state
        .record_library_request_received(
            actor.clone(),
            operation_id,
            "refresh_inventory",
            "LIB001",
            BTreeMap::new(),
        )
        .expect("record library request");

    let records = FileAuditLog::replay(state.audit_dir.as_ref()).expect("replay audit");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].actor, actor);
    assert_eq!(records[0].event, AuditEvent::RequestReceived);
    assert_eq!(records[0].subject.kind, "library");
    assert_eq!(records[0].subject.id.as_deref(), Some("LIB001"));
}

fn test_scheme() -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("rs-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 3,
    }
}

fn project_pool(index: &mut CatalogIndex, pool_id: &str) {
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: Some(pool_id.to_string()),
            copy_class: None,
            content_class: None,
            created_at_utc: Some("2026-05-28T09:00:00Z".to_string()),
        })
        .expect("project pool");
}

fn pool_config(pool_id: &str) -> TapePoolConfig {
    TapePoolConfig {
        id: pool_id.trim().to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(API_SESSION_BLOCK_SIZE),
        min_object_size_bytes: 0,
    }
}

fn pool_config_with_block_size(pool_id: &str, block_size: u32) -> TapePoolConfig {
    let mut cfg = pool_config(pool_id);
    cfg.block_size_bytes = u64::from(block_size);
    cfg
}

fn pool_config_with_watermarks(
    pool_id: &str,
    watermark_low: f64,
    watermark_high: f64,
    min_object_size_bytes: u64,
) -> TapePoolConfig {
    TapePoolConfig {
        id: pool_id.to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: PoolSelectionPolicyName::CompleteOrFill,
        watermark_low,
        watermark_high,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(API_SESSION_BLOCK_SIZE),
        min_object_size_bytes,
    }
}

fn project_eligible_tape(index: &mut CatalogIndex, pool_id: &str, tape_uuid: [u8; 16]) {
    project_eligible_tape_with_voltag(
        index,
        pool_id,
        tape_uuid,
        format!("RMN{:03}L9", tape_uuid[0]).as_str(),
    );
}

fn project_eligible_tape_with_block_size(
    index: &mut CatalogIndex,
    pool_id: &str,
    tape_uuid: [u8; 16],
    block_size: u32,
) {
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: format!("RMN{:03}L9", tape_uuid[0]),
            block_size,
            parity: ParityConfig::Scheme(remanence_parity::default_scheme_for_block_size(
                block_size,
            )),
            force: false,
        })
        .expect("provision parity tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("project tape pool membership");
}

fn project_eligible_tape_with_voltag(
    index: &mut CatalogIndex,
    pool_id: &str,
    tape_uuid: [u8; 16],
    voltag: &str,
) {
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: voltag.to_string(),
            block_size: API_SESSION_BLOCK_SIZE,
            parity: ParityConfig::Scheme(test_scheme()),
            force: false,
        })
        .expect("provision parity tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign tape to pool");
}

fn project_no_parity_tape_usage(
    index: &mut CatalogIndex,
    tape_uuid: [u8; 16],
    total_committed_ordinals: u64,
) {
    index
        .project_committed_tape_file_bundle(
            TapeJournalIndexInput {
                tape_uuid,
                block_size: API_SESSION_BLOCK_SIZE,
                scheme: None,
                journal_offset_bytes: 0,
            },
            &CommittedBundle {
                kind: CommittedBundleKind::Object,
                entries: vec![TapeFileEntry {
                    tape_file_number: 1,
                    kind: TapeFileKind::Object,
                    block_count: total_committed_ordinals,
                    physical_start_hint: Some(0),
                    object_id: None,
                    first_parity_data_ordinal: None,
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: None,
                    object_recovery_row: None,
                }],
                highest_protected_ordinal: 0,
                total_committed_ordinals,
            },
        )
        .expect("project no-parity tape usage");
}

#[test]
fn sealed_manual_completion_audit_repair_is_exactly_once() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    let temp = tempfile::Builder::new()
        .prefix("remanence-terminal-finish-audit-repair")
        .tempdir()
        .expect("tempdir");
    let audit_dir = temp.path().join("audit");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0xa1; 16];
    let operation_id = Uuid::from_u128(0xa101);
    let idempotency_key = Uuid::from_u128(0xa102);
    let request_fingerprint = [0xa3; 32];
    let actor_fingerprint = "sha256:terminal-audit-repair";
    index
        .register_idempotency_request(
            actor_fingerprint,
            FINALIZE_TAPE_OPERATION_KIND,
            idempotency_key,
            request_fingerprint,
            operation_id,
            Some("2026-08-09T00:00:00Z"),
        )
        .expect("project exact request binding");
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        BLOCK_SIZE,
        2,
        10,
        3,
        remanence_parity::index_separation_records(
            BLOCK_SIZE,
            remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        )
        .expect("separation records"),
    )
    .expect("terminal layout");
    let completion = remanence_state::TerminalFinalizationIntent {
        tape_uuid,
        trigger: remanence_state::TerminalFinalizationTrigger::OperatorCloseOut,
        manual: Some(remanence_state::ManualTerminalFinalizationIdentity {
            operation_id: *operation_id.as_bytes(),
            operation_kind: FINALIZE_TAPE_OPERATION_KIND.to_string(),
            actor_fingerprint: actor_fingerprint.to_string(),
            idempotency_key: *idempotency_key.as_bytes(),
            request_fingerprint,
            assigned_pool_id: Some("audit-repair".to_string()),
            expected_pool_id: Some("audit-repair".to_string()),
            assignment_generation: 1,
            reason: "repair final audit after host-only crash".to_string(),
        }),
        progress: remanence_state::TerminalFinalizationProgress::AfterReplicaC,
        recovery_required: false,
        edition_id: [0xa4; 16],
        edition_sequence: 1,
        edition_digest: [0xa5; 32],
        writer_version: "audit-repair-test".to_string(),
        write_timestamp: "2026-08-09T00:00:00Z".to_string(),
        terminal_prefix: None,
        layout: remanence_state::TerminalFinalizationLayout::try_from(layout)
            .expect("persisted layout"),
    };
    let lock = Arc::new(std::sync::Mutex::new(()));

    for _ in 0..2 {
        ensure_tape_sealed_audit(&mut index, &audit_dir, &lock, tape_uuid)
            .expect("ensure exact tape-sealed evidence");
        ensure_manual_finalize_finished_audit(
            &mut index,
            &audit_dir,
            &lock,
            tape_uuid,
            &completion,
        )
        .expect("ensure exact operation-finished evidence");
    }

    let records = FileAuditLog::replay(&audit_dir).expect("replay repaired audit");
    assert_eq!(
        records
            .iter()
            .filter(|record| {
                record.event == AuditEvent::TapeSealed
                    && audit_subject_matches_tape(record, tape_uuid)
            })
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| {
                record.event == AuditEvent::OperationFinished
                    && record.operation_id == Some(operation_id)
            })
            .count(),
        1
    );
    assert_eq!(
        index
            .idempotency_scope_record(
                actor_fingerprint,
                FINALIZE_TAPE_OPERATION_KIND,
                idempotency_key,
            )
            .expect("read repaired scope")
            .expect("scope exists")
            .terminal_state
            .as_deref(),
        Some("finished")
    );

    append_and_project_audit(
        &mut index,
        &audit_dir,
        false,
        &lock,
        ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event: AuditEvent::TapeSealed,
            subject_kind: "tape",
            subject_id: Some(crate::bytes_to_hex(tape_uuid.as_slice())),
            detail: BTreeMap::new(),
        },
    )
    .expect("append hostile duplicate tape-sealed evidence");
    let duplicate_seal = ensure_tape_sealed_audit(&mut index, &audit_dir, &lock, tape_uuid)
        .expect_err("duplicate tape-sealed evidence must fail closed");
    assert_eq!(duplicate_seal.code(), tonic::Code::FailedPrecondition);
    assert!(duplicate_seal.message().contains("2 durable TapeSealed"));

    append_and_project_audit(
        &mut index,
        &audit_dir,
        false,
        &lock,
        ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            operation_id: Some(operation_id),
            session_id: None,
            idempotency_key: Some(idempotency_key),
            event: AuditEvent::OperationFinished,
            subject_kind: "tape",
            subject_id: Some(Uuid::from_bytes(tape_uuid).to_string()),
            detail: BTreeMap::from([
                (
                    "tape_uuid".to_string(),
                    CborValue::Bytes(tape_uuid.to_vec()),
                ),
                (
                    "actor_fingerprint".to_string(),
                    CborValue::Text(actor_fingerprint.to_string()),
                ),
                (
                    "finalization_progress".to_string(),
                    CborValue::Text("after_replica_c".to_string()),
                ),
                (
                    "operation_kind".to_string(),
                    CborValue::Text(FINALIZE_TAPE_OPERATION_KIND.to_string()),
                ),
            ]),
        },
    )
    .expect("append hostile duplicate operation-finished evidence");
    let duplicate_finish = ensure_manual_finalize_finished_audit(
        &mut index,
        &audit_dir,
        &lock,
        tape_uuid,
        &completion,
    )
    .expect_err("duplicate operation-finished evidence must fail closed");
    assert_eq!(duplicate_finish.code(), tonic::Code::FailedPrecondition);
    assert!(duplicate_finish
        .message()
        .contains("2 durable OperationFinished"));

    append_and_project_audit(
        &mut index,
        &audit_dir,
        false,
        &lock,
        ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            operation_id: Some(operation_id),
            session_id: None,
            idempotency_key: Some(idempotency_key),
            event: AuditEvent::OperationFailed,
            subject_kind: "tape",
            subject_id: Some(Uuid::from_bytes(tape_uuid).to_string()),
            detail: BTreeMap::from([
                (
                    "actor_fingerprint".to_string(),
                    CborValue::Text(actor_fingerprint.to_string()),
                ),
                (
                    "operation_kind".to_string(),
                    CborValue::Text(FINALIZE_TAPE_OPERATION_KIND.to_string()),
                ),
            ]),
        },
    )
    .expect("append hostile incompatible terminal evidence");
    let incompatible_terminal = ensure_manual_finalize_finished_audit(
        &mut index,
        &audit_dir,
        &lock,
        tape_uuid,
        &completion,
    )
    .expect_err("incompatible terminal evidence must fail closed");
    assert_eq!(
        incompatible_terminal.code(),
        tonic::Code::FailedPrecondition
    );
    assert!(incompatible_terminal
        .message()
        .contains("incompatible durable terminal audit"));
}

fn project_no_parity_tape(index: &mut CatalogIndex, pool_id: &str, tape_uuid: [u8; 16]) {
    project_no_parity_tape_with_block_size(index, pool_id, tape_uuid, API_SESSION_BLOCK_SIZE);
}

fn append_checkpoint_record(checkpoint_dir: &Path, tape_uuid: [u8; 16]) {
    const PAYLOAD: &[u8] = b"checkpoint selection test payload";
    let object_uuid = Uuid::from_bytes([0x51; 16]);
    let content_sha256 = Sha256::digest(PAYLOAD).to_vec();
    let journal = remanence_state::FileCheckpointJournal::open(checkpoint_dir, tape_uuid)
        .expect("open checkpoint journal");
    journal
        .append(&remanence_state::CheckpointJournalRecord {
            ordinal: 1,
            committed_object_count: 1,
            eod_partition: 0,
            eod_lba: 6,
            tape_uuid,
            batch_id: [0x42; 16],
            next_tape_file_number: 2,
            block_size: API_SESSION_BLOCK_SIZE,
            objects: vec![remanence_state::CheckpointObjectProjection {
                object: NativeObjectProjectionInput {
                    object_id: object_uuid.to_string(),
                    caller_object_id: Some("checkpoint-selection-test".to_string()),
                    body_format: "rem-object-v1".to_string(),
                    logical_size_bytes: Some(PAYLOAD.len() as u64),
                    content_hash: Some(content_sha256.clone()),
                    metadata_hash: Some(vec![0x22; 32]),
                    created_at_utc: Some("2026-07-21T00:00:00Z".to_string()),
                },
                files: vec![NativeObjectFileProjectionInput {
                    object_id: object_uuid.to_string(),
                    file_id: "checkpoint-selection-file".to_string(),
                    path: "payload.bin".to_string(),
                    size_bytes: PAYLOAD.len() as u64,
                    file_sha256: content_sha256,
                    first_chunk_lba: Some(1),
                    chunk_count: 1,
                    mtime: Some("0".to_string()),
                    executable: Some(false),
                }],
                copy: NativeObjectCopyProjectionInput {
                    object_id: object_uuid.to_string(),
                    tape_uuid,
                    tape_file_number: 1,
                    first_body_lba: 0,
                    first_parity_data_ordinal: None,
                    protected_until_ordinal: None,
                    status: "committed".to_string(),
                    representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                    recipient_epoch_ids: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0x33; 32]),
                    stored_digest: Some(vec![0x33; 32]),
                },
                block_size: API_SESSION_BLOCK_SIZE,
                block_count: 3,
                fresh_tape: true,
                total_committed_ordinals: 3,
                object_recovery_row: remanence_state::CheckpointObjectRecoveryRow {
                    tape_file_number: 1,
                    stored_block_count: 3,
                    object_id: object_uuid.to_string().into_bytes(),
                    representation:
                        remanence_state::CheckpointObjectRecoveryRepresentation::Plaintext {
                            manifest_first_chunk_lba: 1,
                            manifest_size_bytes: 1,
                            manifest_chunk_count: 1,
                            manifest_sha256: [0x44; 32],
                        },
                },
            }],
            scheme: None,
            object_tape_file_bundles: Vec::new(),
            barrier_bundle: None,
            terminal_finalization: None,
            sealed_after_write: false,
        })
        .expect("append checkpoint record");
}

#[test]
fn direct_reconciliation_projects_cross_tape_identity_before_retry_selection() {
    const PAYLOAD: &[u8] = b"checkpoint selection test payload";
    let temp = tempfile::Builder::new()
        .prefix("remanence-direct-global-checkpoint-replay-")
        .tempdir()
        .expect("tempdir");
    let config_text = format!(
        r#"
[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
read_only = false

[[tape_pools]]
id = "direct-replay"

[[tape_pool_rules]]
prefix = "DRP"
pool_id = "direct-replay"

[journal]
dir = "{0}/journals"
require_trusted_volume = false

[audit]
dir = "{0}/audit"
fsync = true

[index]
sqlite_path = "{0}/index/rem-state.sqlite"

[cache]
tape_catalog_dir = "{0}/cache/tapes"
"#,
        temp.path().display()
    );
    let config = remanence_state::parse_config_toml(&config_text).expect("parse config");
    let paths = remanence_state::StatePaths::from_config(temp.path().join("config.toml"), &config);
    let checkpoint_dir = paths.journal_dir.join("checkpoints");
    let mut state = StateHandle::open_with_config(paths, config).expect("open locked state");
    let tape_uuid = [0x93; 16];
    state
        .catalog_index()
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: "DRP001L9".to_string(),
            block_size: API_SESSION_BLOCK_SIZE,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision first tape");
    state
        .catalog_index()
        .project_tape_pool_membership(tape_uuid, "direct-replay")
        .expect("assign first tape");
    append_checkpoint_record(&checkpoint_dir, tape_uuid);
    assert!(
        state
            .catalog_index()
            .get_native_object_by_pool_and_caller_object_id(
                "direct-replay",
                "checkpoint-selection-test",
            )
            .expect("query pre-reconcile identity")
            .is_none(),
        "the test must begin at the journal-fsync before-SQLite crash cut"
    );

    let source_path = temp.path().join("payload.bin");
    std::fs::write(&source_path, PAYLOAD).expect("write exact replay source");
    let request = WriteObjectToPoolRequest {
        pool_id: "direct-replay".to_string(),
        source: WriteObjectSource::Path(source_path.clone()),
        archive_path: "./payload.bin".into(),
        caller_object_id: "checkpoint-selection-test".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let wrong_path = replay_committed_pool_write_from_state(
        &mut state,
        &WriteObjectToPoolRequest {
            pool_id: "direct-replay".to_string(),
            source: WriteObjectSource::Path(source_path.clone()),
            archive_path: "renamed.bin".into(),
            caller_object_id: "checkpoint-selection-test".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect_err("journal replay must reject a changed logical member path");
    assert!(matches!(
        wrong_path,
        PoolWriteError::CallerObjectIdArchivePathConflict { .. }
    ));

    let (_, recipients) = recipient_pair(0x72);
    let wrong_representation = replay_committed_pool_write_from_state(
        &mut state,
        &WriteObjectToPoolRequest {
            pool_id: "direct-replay".to_string(),
            source: WriteObjectSource::Path(source_path),
            archive_path: "./payload.bin".into(),
            caller_object_id: "checkpoint-selection-test".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Encrypted { recipients },
        },
    )
    .expect_err("journal replay must reject a changed stored representation");
    assert!(matches!(
        wrong_representation,
        PoolWriteError::CallerObjectIdRepresentationConflict { .. }
    ));

    let replay = replay_committed_pool_write_from_state(&mut state, &request)
        .expect("global host-only direct-write replay")
        .expect("durable checkpoint identity must replay before selection");
    assert!(replay.is_replay());

    assert!(
        state
            .catalog_index()
            .get_native_object_by_pool_and_caller_object_id(
                "direct-replay",
                "checkpoint-selection-test",
            )
            .expect("query reconciled identity")
            .is_some(),
        "a retry must see tape A's identity before it can select tape B"
    );
}

fn restart_cycle_checkpoint_record(
    tape_uuid: [u8; 16],
    ordinal: u64,
    object_byte: u8,
    object_tape_file: u64,
    block_count: u64,
    total_committed_ordinals: u64,
) -> remanence_state::CheckpointJournalRecord {
    let object_uuid = Uuid::from_bytes([object_byte; 16]);
    remanence_state::CheckpointJournalRecord {
        ordinal,
        committed_object_count: ordinal,
        eod_partition: 0,
        eod_lba: total_committed_ordinals + ordinal + 2,
        tape_uuid,
        batch_id: [0x60 + ordinal as u8; 16],
        next_tape_file_number: object_tape_file + 1,
        block_size: API_SESSION_BLOCK_SIZE,
        objects: vec![remanence_state::CheckpointObjectProjection {
            object: NativeObjectProjectionInput {
                object_id: object_uuid.to_string(),
                caller_object_id: Some(format!("restart-object-{ordinal}")),
                body_format: "rem-object-v1".to_string(),
                logical_size_bytes: Some(block_count * u64::from(API_SESSION_BLOCK_SIZE)),
                content_hash: Some(vec![object_byte; 32]),
                metadata_hash: Some(vec![object_byte.wrapping_add(1); 32]),
                created_at_utc: Some(format!("2026-07-21T00:00:0{ordinal}Z")),
            },
            files: Vec::new(),
            copy: NativeObjectCopyProjectionInput {
                object_id: object_uuid.to_string(),
                tape_uuid,
                tape_file_number: object_tape_file,
                first_body_lba: 1,
                first_parity_data_ordinal: None,
                protected_until_ordinal: None,
                status: "committed".to_string(),
                representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                recipient_epoch_ids: None,
                metadata_frame_len: None,
                plaintext_digest: Some(vec![object_byte.wrapping_add(2); 32]),
                stored_digest: Some(vec![object_byte.wrapping_add(2); 32]),
            },
            block_size: API_SESSION_BLOCK_SIZE,
            block_count,
            fresh_tape: ordinal == 1,
            total_committed_ordinals,
            object_recovery_row: remanence_state::CheckpointObjectRecoveryRow {
                tape_file_number: object_tape_file,
                stored_block_count: block_count,
                object_id: object_uuid.to_string().into_bytes(),
                representation:
                    remanence_state::CheckpointObjectRecoveryRepresentation::Plaintext {
                        manifest_first_chunk_lba: 1,
                        manifest_size_bytes: 1,
                        manifest_chunk_count: 1,
                        manifest_sha256: [object_byte.wrapping_add(1); 32],
                    },
            },
        }],
        scheme: None,
        object_tape_file_bundles: Vec::new(),
        barrier_bundle: None,
        terminal_finalization: None,
        sealed_after_write: false,
    }
}

#[test]
fn startup_replay_repairs_live_restart_cycle_partial_projection() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-checkpoint-live-restart")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let checkpoint_dir = temp.path().join("checkpoints");
    let tape_uuid = [0x91; 16];
    let mut index = CatalogIndex::open(&index_path).expect("open index");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: "RST001L9".to_string(),
            block_size: API_SESSION_BLOCK_SIZE,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision restart-cycle tape");
    let journal = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
        .expect("open checkpoint journal");

    // Append, checkpoint, kill, and restart through the daemon's startup replay helper.
    let first = restart_cycle_checkpoint_record(tape_uuid, 1, 0x71, 1, 3, 3);
    journal.append(&first).expect("fsync first checkpoint");
    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("project first checkpoint");
    drop(index);
    let mut index = CatalogIndex::open(&index_path).expect("restart after first checkpoint");
    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("startup replay after first kill");

    // Append again, then preserve the same independently compatible views found in the
    // field evidence before killing mid-projection: object metadata is visible,
    // while the copy, object tape-file, and catalog unit are absent.
    let second = restart_cycle_checkpoint_record(tape_uuid, 2, 0x72, 2, 2, 5);
    journal.append(&second).expect("fsync second checkpoint");
    index
        .upsert_native_object_projection(second.objects[0].object.clone(), &[])
        .expect("project independently visible object metadata");
    drop(index);

    let mut index = CatalogIndex::open(&index_path).expect("restart after projection kill");
    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("journal-authoritative startup repair");
    let second_object_id = second.objects[0].object.object_id.as_str();
    let copies = index
        .find_native_object_copies(second_object_id)
        .expect("query repaired copy");
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].tape_uuid, tape_uuid);
    assert_eq!(copies[0].tape_file_number, 2);
    let tape_files = index
        .list_tape_files(&tape_uuid)
        .expect("query repaired tape files");
    assert!(tape_files.iter().any(|entry| {
        entry.tape_file_number == 2
            && entry.kind == "object"
            && entry.object_id.as_deref() == Some(second_object_id)
    }));

    index
        .retire_tape(RetireTapeInput {
            tape_uuid,
            reason: "restart-regression-retire".to_string(),
        })
        .expect("retire checkpoint tape");
    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("retired copy status is compatible with checkpoint history");
    assert_eq!(
        index
            .find_native_object_copies(second_object_id)
            .expect("query retired checkpoint copy")[0]
            .status,
        "missing"
    );

    // A genuine contradiction still refuses startup and names the record and both views.
    let mut divergent_object = second.objects[0].object.clone();
    divergent_object.content_hash = Some(vec![0xff; 32]);
    index
        .upsert_native_object_projection(divergent_object, &[])
        .expect("seed contradictory SQLite object view");
    let error = replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect_err("contradictory projection must refuse startup");
    let message = error.message();
    assert!(
        message.contains("tape=91919191919191919191919191919191"),
        "{message}"
    );
    assert!(message.contains("ordinal=2"), "{message}");
    assert!(
        message.contains("batch=62626262626262626262626262626262"),
        "{message}"
    );
    assert!(message.contains(second_object_id), "{message}");
    assert!(message.contains("journal="), "{message}");
    assert!(message.contains("sqlite="), "{message}");
}

#[test]
fn startup_replay_retires_only_companion_only_manual_acceptance() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    let temp = tempfile::Builder::new()
        .prefix("remanence-manual-acceptance-restart")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let checkpoint_dir = temp.path().join("checkpoints");
    let tape_uuid = [0x93; 16];
    let operation_id = Uuid::from_u128(0x9301);
    let idempotency_key = Uuid::from_u128(0x9302);
    let request_fingerprint = [0x93; 32];
    let mut index = CatalogIndex::open(&index_path).expect("open index");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: "RST003L9".to_string(),
            block_size: BLOCK_SIZE,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision restart-cycle tape");
    let journal = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
        .expect("open checkpoint journal");
    let mut checkpoint = restart_cycle_checkpoint_record(tape_uuid, 1, 0x74, 1, 3, 3);
    checkpoint.block_size = BLOCK_SIZE;
    checkpoint.objects[0].block_size = BLOCK_SIZE;
    checkpoint.objects[0].object.logical_size_bytes = Some(3 * u64::from(BLOCK_SIZE));
    journal.append(&checkpoint).expect("fsync checkpoint");
    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("project ordinary checkpoint");
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        BLOCK_SIZE,
        checkpoint.next_tape_file_number,
        checkpoint.eod_lba,
        3,
        remanence_parity::index_separation_records(
            BLOCK_SIZE,
            remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        )
        .expect("gap records"),
    )
    .expect("terminal layout");
    let intent = remanence_state::TerminalFinalizationIntent {
        tape_uuid,
        trigger: remanence_state::TerminalFinalizationTrigger::OperatorCloseOut,
        manual: Some(remanence_state::ManualTerminalFinalizationIdentity {
            operation_id: *operation_id.as_bytes(),
            operation_kind: FINALIZE_TAPE_OPERATION_KIND.to_string(),
            actor_fingerprint: "user:restart-operator".to_string(),
            idempotency_key: *idempotency_key.as_bytes(),
            request_fingerprint,
            assigned_pool_id: None,
            expected_pool_id: None,
            assignment_generation: 0,
            reason: "restart provisional acceptance".to_string(),
        }),
        progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
        recovery_required: false,
        edition_id: [0x94; 16],
        edition_sequence: 2,
        edition_digest: [0x95; 32],
        writer_version: "restart-manual-test".to_string(),
        write_timestamp: "2026-08-10T00:00:00Z".to_string(),
        terminal_prefix: None,
        layout: remanence_state::TerminalFinalizationLayout::try_from(layout)
            .expect("persist layout"),
    };
    let mut lease = journal
        .acquire_exclusive()
        .expect("acquire checkpoint owner");
    lease
        .begin_terminal_finalization(&intent)
        .expect("publish provisional companion");
    drop(lease);

    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("startup retires companion-only acceptance");
    assert!(journal
        .terminal_finalization_intent()
        .expect("read retired provisional companion")
        .is_none());
    assert!(index
        .terminal_finalization(&tape_uuid)
        .expect("read absent finalization projection")
        .is_none());
    assert!(index
        .idempotency_scope_record(
            "user:restart-operator",
            FINALIZE_TAPE_OPERATION_KIND,
            idempotency_key,
        )
        .expect("read absent idempotency binding")
        .is_none());

    let mut lease = journal
        .acquire_exclusive()
        .expect("reacquire checkpoint owner");
    lease
        .begin_terminal_finalization(&intent)
        .expect("republish provisional companion");
    drop(lease);
    index
        .register_idempotency_request(
            "user:restart-operator",
            FINALIZE_TAPE_OPERATION_KIND,
            idempotency_key,
            request_fingerprint,
            operation_id,
            None,
        )
        .expect("seed one database half");
    let half_error = replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect_err("startup refuses one-half acceptance");
    assert!(
        half_error.message().contains("not both present"),
        "{half_error}"
    );
    assert!(journal
        .terminal_finalization_intent()
        .expect("read retained one-half companion")
        .is_some());

    index
        .project_terminal_finalization(remanence_state::TerminalFinalizationProjectionInput {
            tape_uuid,
            trigger: intent.trigger,
            operation_id: Some(operation_id),
            progress: intent.progress,
            edition_digest: intent.edition_digest,
            layout_digest: intent.layout.layout_digest,
            outcome: remanence_state::TerminalFinalizationOutcome::InProgress,
            updated_at_utc: None,
        })
        .expect("seed finalization projection");
    rusqlite::Connection::open(&index_path)
        .expect("open raw index connection")
        .execute(
            "delete from idempotency_keys
                 where actor_fingerprint = ?1
                   and operation_kind = ?2
                   and idempotency_key = ?3",
            rusqlite::params![
                "user:restart-operator",
                FINALIZE_TAPE_OPERATION_KIND,
                idempotency_key.to_string()
            ],
        )
        .expect("remove idempotency half");
    let projection_half_error = replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect_err("startup refuses projection-only acceptance");
    assert!(
        projection_half_error.message().contains("not both present"),
        "{projection_half_error}"
    );
    assert!(journal
        .terminal_finalization_intent()
        .expect("read retained projection-only companion")
        .is_some());

    index
        .register_idempotency_request(
            "user:restart-operator",
            FINALIZE_TAPE_OPERATION_KIND,
            idempotency_key,
            request_fingerprint,
            operation_id,
            None,
        )
        .expect("restore exact second database half");
    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("startup resumes exact two-half acceptance");
    assert!(journal
        .terminal_finalization_intent()
        .expect("read resumable accepted companion")
        .is_some());
}

#[test]
fn startup_replay_restores_pending_automatic_finalization_through_replica_c() {
    const TERMINAL_BLOCK_SIZE: u32 = 256 * 1024;
    let temp = tempfile::Builder::new()
        .prefix("remanence-checkpoint-terminal-restart")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let checkpoint_dir = temp.path().join("checkpoints");
    let tape_uuid = [0x92; 16];
    let mut index = CatalogIndex::open(&index_path).expect("open index");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: "RST002L9".to_string(),
            block_size: TERMINAL_BLOCK_SIZE,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision restart-cycle tape");
    let journal = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
        .expect("open checkpoint journal");
    let mut checkpoint = restart_cycle_checkpoint_record(tape_uuid, 1, 0x73, 1, 3, 3);
    checkpoint.block_size = TERMINAL_BLOCK_SIZE;
    checkpoint.objects[0].block_size = TERMINAL_BLOCK_SIZE;
    checkpoint.objects[0].object.logical_size_bytes = Some(3 * u64::from(TERMINAL_BLOCK_SIZE));
    journal.append(&checkpoint).expect("fsync checkpoint");
    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("project ordinary checkpoint");

    let parity_layout = remanence_parity::TerminalTailLayout::new(
        0,
        TERMINAL_BLOCK_SIZE,
        checkpoint.next_tape_file_number,
        checkpoint.eod_lba,
        3,
        remanence_parity::index_separation_records(
            TERMINAL_BLOCK_SIZE,
            remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        )
        .expect("gap records"),
    )
    .expect("terminal layout");
    let intent = remanence_state::TerminalFinalizationIntent {
        tape_uuid,
        trigger: remanence_state::TerminalFinalizationTrigger::ReachedLowWatermark,
        manual: None,
        progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
        recovery_required: false,
        edition_id: [0x24; 16],
        edition_sequence: 2,
        edition_digest: [0x25; 32],
        writer_version: "restart-test".to_string(),
        write_timestamp: "2026-08-09T00:00:00Z".to_string(),
        terminal_prefix: None,
        layout: remanence_state::TerminalFinalizationLayout::try_from(parity_layout)
            .expect("persisted layout"),
    };
    let mut lease = journal
        .acquire_exclusive()
        .expect("exclusive checkpoint owner");
    lease
        .begin_terminal_finalization(&intent)
        .expect("persist terminal intent");
    for (expected, next) in [
        (
            remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
            remanence_state::TerminalFinalizationProgress::AfterReplicaA,
        ),
        (
            remanence_state::TerminalFinalizationProgress::AfterReplicaA,
            remanence_state::TerminalFinalizationProgress::AfterSeparationAb,
        ),
        (
            remanence_state::TerminalFinalizationProgress::AfterSeparationAb,
            remanence_state::TerminalFinalizationProgress::AfterReplicaB,
        ),
        (
            remanence_state::TerminalFinalizationProgress::AfterReplicaB,
            remanence_state::TerminalFinalizationProgress::AfterSeparationBc,
        ),
        (
            remanence_state::TerminalFinalizationProgress::AfterSeparationBc,
            remanence_state::TerminalFinalizationProgress::AfterReplicaC,
        ),
    ] {
        lease
            .advance_terminal_finalization(expected, next)
            .expect("advance durable terminal progress");
    }
    let intent = lease
        .mark_terminal_recovery_required()
        .expect("persist recovery-required state before restart");
    drop(lease);
    drop(index);

    let mut index = CatalogIndex::open(&index_path).expect("restart index");
    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("restore pending finalization");
    let tape = index
        .get_tape(&tape_uuid)
        .expect("read tape")
        .expect("known tape");
    assert_eq!(tape.state, "recovery_required");
    let projection = tape.terminal_finalization.expect("terminal projection");
    assert_eq!(projection.progress, intent.progress);
    assert_eq!(projection.completed_replicas, 3);
    assert_eq!(projection.edition_digest, intent.edition_digest);
    assert_eq!(projection.layout_digest, intent.layout.layout_digest);
    assert_eq!(
        projection.outcome,
        remanence_state::TerminalFinalizationOutcome::RecoveryRequired
    );

    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("pending finalization replay is idempotent");

    let replica_c = intent.layout.components[4];
    let mut completed_intent = intent.clone();
    completed_intent.recovery_required = false;
    let terminal = remanence_state::CheckpointJournalRecord {
        ordinal: checkpoint.ordinal + 1,
        committed_object_count: checkpoint.committed_object_count,
        eod_partition: intent.layout.partition,
        eod_lba: intent.layout.expected_eod_lba,
        tape_uuid,
        batch_id: intent.edition_id,
        next_tape_file_number: replica_c
            .tape_file_number
            .checked_add(1)
            .expect("replica C next file"),
        block_size: intent.layout.block_size,
        objects: Vec::new(),
        scheme: checkpoint.scheme.clone(),
        object_tape_file_bundles: Vec::new(),
        barrier_bundle: Some(remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::TerminalComponent,
            entries: vec![remanence_parity::TapeFileEntry {
                tape_file_number: replica_c.tape_file_number,
                kind: remanence_parity::TapeFileKind::TapeIndexReplica,
                block_count: replica_c.record_count,
                physical_start_hint: Some(replica_c.start_lba),
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: Some(intent.edition_digest),
                object_recovery_row: None,
            }],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 3,
        }),
        terminal_finalization: Some(completed_intent),
        sealed_after_write: true,
    };
    let mut lease = journal
        .acquire_exclusive_for_terminal_recovery()
        .expect("acquire final host recovery");
    let interrupted = lease
        .append_terminal_finalization_with_after_fsync(std::slice::from_ref(&terminal), || {
            Err(remanence_state::StateError::JournalReplayFailed(
                "simulated sealed-checkpoint cleanup interruption".to_string(),
            ))
        })
        .expect_err("leave exact intent beside sealed checkpoint");
    assert!(interrupted.to_string().contains("cleanup interruption"));
    drop(lease);
    drop(index);

    let failed_index_path = temp.path().join("terminal-projection-failure.sqlite");
    let mut failed_index =
        CatalogIndex::open(&failed_index_path).expect("open terminal projection-failure catalog");
    failed_index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: "RST902L9".to_string(),
            block_size: TERMINAL_BLOCK_SIZE,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision projection-failure tape");
    failed_index
        .project_checkpoint_record(&checkpoint)
        .expect("project every ordinary record before terminal failure");
    let completed = terminal
        .terminal_finalization
        .as_ref()
        .expect("terminal completion authority");
    let mut conflicting_edition_digest = completed.edition_digest;
    conflicting_edition_digest[0] ^= 0xff;
    failed_index
        .project_terminal_finalization(remanence_state::TerminalFinalizationProjectionInput {
            tape_uuid,
            trigger: completed.trigger,
            operation_id: None,
            progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
            edition_digest: conflicting_edition_digest,
            layout_digest: completed.layout.layout_digest,
            outcome: remanence_state::TerminalFinalizationOutcome::InProgress,
            updated_at_utc: None,
        })
        .expect("seed a contradiction reached only by the sealed projection");
    let projection_error =
        replay_checkpoint_journal_projections(&mut failed_index, &checkpoint_dir)
            .expect_err("terminal projection failure retains terminal retry authority");
    assert!(
        projection_error
            .message()
            .contains("terminal finalization identity changed"),
        "{projection_error}"
    );
    assert!(
        journal
            .terminal_finalization_intent()
            .expect("read companion after startup projection failure")
            .is_some(),
        "startup must not retire the companion before every checkpoint projection succeeds"
    );

    let mut index = CatalogIndex::open(&index_path).expect("restart after sealed checkpoint");
    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("sealed checkpoint wins over its stale matching intent");
    let tape = index
        .get_tape(&tape_uuid)
        .expect("read sealed tape")
        .expect("known sealed tape");
    assert_eq!(tape.state, "sealed");
    assert_eq!(
        tape.terminal_finalization
            .expect("sealed finalization projection")
            .outcome,
        remanence_state::TerminalFinalizationOutcome::Finalized
    );
    assert_eq!(
        journal
            .terminal_finalization_intent()
            .expect("read cleaned companion intent"),
        None
    );
    replay_checkpoint_journal_projections(&mut index, &checkpoint_dir)
        .expect("sealed startup audit replay is idempotent");
    let audit =
        FileAuditLog::replay(temp.path().join("audit")).expect("replay startup sealing evidence");
    assert_eq!(
        audit
            .iter()
            .filter(|record| {
                record.event == AuditEvent::TapeSealed
                    && audit_subject_matches_tape(record, tape_uuid)
            })
            .count(),
        1,
        "startup recovery must repair one durable TapeSealed event without duplication"
    );
}

fn project_no_parity_tape_with_block_size(
    index: &mut CatalogIndex,
    pool_id: &str,
    tape_uuid: [u8; 16],
    block_size: u32,
) {
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: format!("RMN{:03}L9", tape_uuid[0]),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision no-parity tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign no-parity tape to pool");
}

fn no_parity_bootstrap_block(tape_uuid: [u8; 16]) -> Vec<u8> {
    let payload = BootstrapPayload {
        scheme: None,
        no_parity_flag: true,
        filemark_map_digest: None,
        tape_uuid,
        written_by_version: "test".to_string(),
        written_at: "2026-05-29T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: API_SESSION_BLOCK_SIZE,
        drive_compression: false,
    };
    let mut block = vec![0u8; API_SESSION_BLOCK_SIZE as usize];
    write_bootstrap_block(&payload, &mut block).expect("write no-parity bootstrap");
    block
}

fn writable_tape_record() -> TapeRecord {
    TapeRecord {
        tape_uuid: POOL_WRITE_TAPE_UUID.to_vec(),
        voltag: Some("RMN001L9".to_string()),
        kind: "data".to_string(),
        pool_id: Some("camera.copy-a".to_string()),
        assignment_generation: 0,
        body_format: None,
        block_size: Some(API_SESSION_BLOCK_SIZE as u64),
        scheme_id: None,
        data_blocks_per_stripe: None,
        parity_blocks_per_stripe: None,
        stripes_per_neighborhood: None,
        last_committed_tape_file: None,
        total_committed_ordinals: 0,
        written_extent_lba: None,
        terminal_finalization: None,
        state: "ready".to_string(),
        updated_at_utc: "2026-05-29T00:00:00Z".to_string(),
    }
}

#[test]
fn tape_proto_preserves_adoption_state_and_assignment_generation() {
    let mut record = writable_tape_record();
    record.assignment_generation = 7;
    record.state = "recovery_required".to_string();
    let projected = tape_to_proto(record.clone());
    assert_eq!(projected.assignment_generation, 7);
    assert_eq!(
        projected.state,
        pb::tape::State::TapeStateRecoveryRequired as i32
    );
    record.state = "retired".to_string();
    assert_eq!(
        tape_to_proto(record).state,
        pb::tape::State::TapeStateRetired as i32
    );
}

#[test]
fn bootstrap_build_write_parse_round_trips_no_parity_and_parity() {
    let no_parity = build_tape_bootstrap(
        POOL_WRITE_TAPE_UUID,
        API_SESSION_BLOCK_SIZE,
        ParityConfig::None,
        "2026-05-29T00:00:00Z",
        "test-version",
    );
    let mut no_parity_sink = VecBlockSink::new();
    write_tape_bootstrap(&mut no_parity_sink, &no_parity).expect("write no-parity bootstrap");
    assert_eq!(no_parity_sink.filemarks, vec![1]);
    let parsed =
        parse_bootstrap_block(&no_parity_sink.blocks[0]).expect("parse no-parity bootstrap");
    assert_eq!(parsed.tape_uuid, POOL_WRITE_TAPE_UUID);
    assert_eq!(parsed.block_size_bytes, API_SESSION_BLOCK_SIZE);
    assert!(parsed.no_parity_flag);
    assert!(parsed.scheme.is_none());
    assert!(parsed.filemark_map_digest.is_none());

    let parity = build_tape_bootstrap(
        SECOND_POOL_WRITE_TAPE_UUID,
        API_SESSION_BLOCK_SIZE,
        ParityConfig::Scheme(test_scheme()),
        "2026-05-29T00:00:00Z",
        "test-version",
    );
    let mut parity_sink = VecBlockSink::new();
    write_tape_bootstrap(&mut parity_sink, &parity).expect("write parity bootstrap");
    assert_eq!(parity_sink.filemarks, vec![1]);
    let parsed = parse_bootstrap_block(&parity_sink.blocks[0]).expect("parse parity bootstrap");
    assert_eq!(parsed.tape_uuid, SECOND_POOL_WRITE_TAPE_UUID);
    assert_eq!(parsed.block_size_bytes, API_SESSION_BLOCK_SIZE);
    assert!(!parsed.no_parity_flag);
    assert!(parsed.scheme.is_some());
    assert_eq!(
        parsed.filemark_map_digest,
        Some(remanence_parity::sole_bot_filemark_map_digest().expect("sole BOT digest"))
    );
}

#[test]
fn lto_capacity_table_parses_suffixes_and_reports_raw_capacity() {
    assert_eq!(lto_generation_from_voltag("RMN001L9"), Some(LtoGen::Lto9));
    assert_eq!(lto_generation_from_voltag("rmn002l8"), Some(LtoGen::Lto8));
    assert_eq!(lto_generation_from_voltag("RMN003L7"), Some(LtoGen::Lto7));
    assert_eq!(lto_generation_from_voltag("RMN004"), None);
    assert_eq!(raw_capacity_bytes(LtoGen::Lto7), 6_000_000_000_000);
    assert_eq!(raw_capacity_bytes(LtoGen::Lto8), 12_000_000_000_000);
    assert_eq!(raw_capacity_bytes(LtoGen::Lto9), 18_000_000_000_000);
}

#[test]
fn writability_preconditions_accept_ready_tape_and_report_each_reject() {
    let tape = writable_tape_record();
    check_writability_preconditions(&tape, 1024).expect("ready tape is writable");

    let mut not_ready = tape.clone();
    not_ready.state = "ingested".to_string();
    let err = check_writability_preconditions(&not_ready, 1024).expect_err("not ready rejects");
    assert!(
        matches!(err, WritabilityError::NotReady { ref state } if state == "ingested"),
        "{err}"
    );

    let mut missing_geometry = tape.clone();
    missing_geometry.scheme_id = Some("rs-test".to_string());
    let err = check_writability_preconditions(&missing_geometry, 1024)
        .expect_err("partial parity geometry rejects");
    assert!(
        matches!(err, WritabilityError::MissingGeometry { .. }),
        "{err}"
    );

    let mut exhausted = tape;
    exhausted.voltag = Some("RMN001L1".to_string());
    exhausted.block_size = Some(100);
    let scheme = test_scheme();
    exhausted.scheme_id = Some(scheme.id.as_str().to_string());
    exhausted.data_blocks_per_stripe = Some(u32::from(scheme.data_blocks_per_stripe));
    exhausted.parity_blocks_per_stripe = Some(u32::from(scheme.parity_blocks_per_stripe));
    exhausted.stripes_per_neighborhood = Some(scheme.stripes_per_neighborhood);
    let err = check_writability_preconditions(
        &exhausted,
        raw_capacity_bytes(LtoGen::Lto1).saturating_add(1),
    )
    .expect_err("capacity rejects");
    assert!(
        matches!(err, WritabilityError::InsufficientCapacity { .. }),
        "{err}"
    );

    let mut written_no_parity = writable_tape_record();
    written_no_parity.total_committed_ordinals = 7;
    check_writability_preconditions(&written_no_parity, 1)
        .expect("written no-parity tape is appendable");

    let mut written_parity = writable_tape_record();
    let scheme = test_scheme();
    written_parity.scheme_id = Some(scheme.id.as_str().to_string());
    written_parity.data_blocks_per_stripe = Some(u32::from(scheme.data_blocks_per_stripe));
    written_parity.parity_blocks_per_stripe = Some(u32::from(scheme.parity_blocks_per_stripe));
    written_parity.stripes_per_neighborhood = Some(scheme.stripes_per_neighborhood);
    written_parity.total_committed_ordinals = 11;
    let err = check_writability_preconditions(&written_parity, 1)
        .expect_err("written parity tape must not be reopened at BOT");
    assert!(
        matches!(
            err,
            WritabilityError::ParityAppendUnsupported {
                total_committed_ordinals: 11
            }
        ),
        "{err}"
    );
}

#[test]
fn retired_tape_is_rejected_as_not_ready_for_writes() {
    let mut retired = writable_tape_record();
    retired.state = "retired".to_string();

    let err = check_writability_preconditions(&retired, 1024)
        .expect_err("retired tape must reject writes");

    assert!(
        matches!(err, WritabilityError::NotReady { ref state } if state == "retired"),
        "{err}"
    );
}

fn object_uuid() -> Uuid {
    Uuid::parse_str(OBJECT_ID_TEXT).expect("valid object uuid")
}

fn operation_uuid() -> Uuid {
    Uuid::parse_str(OPERATION_ID_TEXT).expect("valid operation uuid")
}

fn populated_state() -> ApiState {
    let mut index = test_index();
    let scheme = ParityScheme {
        id: SchemeId::new_static("rs-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 3,
    };
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: "camera.copy-a".to_string(),
            display_name: Some("Camera copy A".to_string()),
            copy_class: Some("copy-a".to_string()),
            content_class: Some("camera".to_string()),
            created_at_utc: Some("2026-05-28T09:00:00Z".to_string()),
        })
        .expect("project tape pool");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: TAPE_UUID,
            voltag: "ACM003L9".to_string(),
            block_size: 4096,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision tape before assigning pool");
    index
        .project_tape_pool_membership(TAPE_UUID, "camera.copy-a")
        .expect("assign tape to pool");
    index
        .index_committed_tape_journal(
            TapeJournalIndexInput {
                tape_uuid: TAPE_UUID,
                block_size: 4096,
                scheme: Some(scheme),
                journal_offset_bytes: 99,
            },
            &CommittedState {
                entries: vec![
                    TapeFileEntry {
                        tape_file_number: 4,
                        kind: TapeFileKind::Object,
                        block_count: 5,
                        physical_start_hint: Some(0),
                        object_id: Some(OBJECT_ID_TEXT.to_string()),
                        first_parity_data_ordinal: Some(0),
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        object_recovery_row: None,
                    },
                    TapeFileEntry {
                        tape_file_number: 5,
                        kind: TapeFileKind::ParitySidecar,
                        block_count: 2,
                        physical_start_hint: Some(5),
                        object_id: None,
                        first_parity_data_ordinal: None,
                        epoch_id: Some(0),
                        protected_ordinal_start: Some(0),
                        protected_ordinal_end_exclusive: Some(5),
                        canonical_metadata_hash: Some([9u8; 32]),
                        object_recovery_row: None,
                    },
                    TapeFileEntry {
                        tape_file_number: 6,
                        kind: TapeFileKind::ParityMap,
                        block_count: 1,
                        physical_start_hint: Some(7),
                        object_id: None,
                        first_parity_data_ordinal: None,
                        epoch_id: Some(0),
                        protected_ordinal_start: Some(0),
                        protected_ordinal_end_exclusive: Some(5),
                        canonical_metadata_hash: Some([8u8; 32]),
                        object_recovery_row: None,
                    },
                    TapeFileEntry {
                        tape_file_number: 7,
                        kind: TapeFileKind::Bootstrap,
                        block_count: 1,
                        physical_start_hint: Some(8),
                        object_id: None,
                        first_parity_data_ordinal: None,
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: Some([7u8; 32]),
                        object_recovery_row: None,
                    },
                ],
                highest_protected_ordinal: 5,
                total_committed_ordinals: 5,
                orphaned_bundles: Vec::new(),
            },
        )
        .expect("index tape journal");
    index
        .upsert_native_object_projection(
            NativeObjectProjectionInput {
                object_id: OBJECT_ID_TEXT.to_string(),
                caller_object_id: Some("caller-1".to_string()),
                body_format: "rem-object-v1".to_string(),
                logical_size_bytes: Some(17),
                content_hash: Some(vec![7u8; 32]),
                metadata_hash: None,
                created_at_utc: Some("2026-05-28T12:00:00Z".to_string()),
            },
            &[NativeObjectCopyProjectionInput {
                object_id: OBJECT_ID_TEXT.to_string(),
                tape_uuid: TAPE_UUID,
                tape_file_number: 4,
                first_body_lba: 0,
                first_parity_data_ordinal: Some(0),
                protected_until_ordinal: Some(8),
                status: "committed".to_string(),
                representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                recipient_epoch_ids: None,
                metadata_frame_len: None,
                plaintext_digest: Some(vec![0x51; 32]),
                stored_digest: Some(vec![0x51; 32]),
            }],
        )
        .expect("populate object");
    ApiState::new(index)
}

fn populated_state_with_file_catalog() -> ApiState {
    let state = populated_state();
    let scheme = ParityScheme {
        id: SchemeId::new_static("rs-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 3,
    };
    let mut index = CatalogIndex::open(state.index_path.as_ref()).expect("open test index");
    index
        .project_native_object_and_committed_tape_file_bundle(
            NativeObjectProjectionInput {
                object_id: OBJECT_ID_TEXT.to_string(),
                caller_object_id: Some("caller-1".to_string()),
                body_format: "rem-object-v1".to_string(),
                logical_size_bytes: Some(17),
                content_hash: Some(vec![7u8; 32]),
                metadata_hash: None,
                created_at_utc: Some("2026-05-28T12:00:00Z".to_string()),
            },
            &[NativeObjectFileProjectionInput {
                object_id: OBJECT_ID_TEXT.to_string(),
                file_id: "file-camera".to_string(),
                path: "payload.bin".to_string(),
                size_bytes: 17,
                file_sha256: vec![7u8; 32],
                first_chunk_lba: Some(2),
                chunk_count: 1,
                mtime: Some("0".to_string()),
                executable: Some(false),
            }],
            &[NativeObjectCopyProjectionInput {
                object_id: OBJECT_ID_TEXT.to_string(),
                tape_uuid: TAPE_UUID,
                tape_file_number: 4,
                first_body_lba: 0,
                first_parity_data_ordinal: Some(0),
                protected_until_ordinal: Some(8),
                status: "committed".to_string(),
                representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                recipient_epoch_ids: None,
                metadata_frame_len: None,
                plaintext_digest: Some(vec![0x51; 32]),
                stored_digest: Some(vec![0x51; 32]),
            }],
            TapeJournalIndexInput {
                tape_uuid: TAPE_UUID,
                block_size: 4096,
                scheme: Some(scheme),
                journal_offset_bytes: 99,
            },
            &CommittedBundle {
                kind: CommittedBundleKind::Object,
                entries: vec![TapeFileEntry {
                    tape_file_number: 4,
                    kind: TapeFileKind::Object,
                    block_count: 5,
                    physical_start_hint: Some(0),
                    object_id: Some(OBJECT_ID_TEXT.to_string()),
                    first_parity_data_ordinal: Some(0),
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: None,
                    object_recovery_row: None,
                }],
                highest_protected_ordinal: 5,
                total_committed_ordinals: 5,
            },
        )
        .expect("populate object file rows");
    state
}

fn empty_pool_state() -> ApiState {
    pool_state_with_tapes(&[TAPE_UUID])
}

fn state_with_library_snapshot(serial: &str) -> ApiState {
    let mut state = empty_pool_state();
    state.default_library_serial = Some(Arc::new(serial.to_string()));
    state.library_snapshot = Some(Arc::new(RwLock::new(Arc::new(LibrarySnapshot {
        report: DiscoveryReport {
            libraries: vec![test_library(serial)],
            warnings: Vec::new(),
        },
        captured_at: OffsetDateTime::UNIX_EPOCH,
    }))));
    state
}

fn test_library(serial: &str) -> Library {
    Library {
        serial: serial.to_string(),
        changer_sg: PathBuf::from("/dev/sg7"),
        changer_sysfs: PathBuf::from("/sys/test"),
        changer_inquiry: Inquiry {
            device_type: DeviceType::MediumChanger,
            peripheral_qualifier: 0,
            removable: true,
            version: 7,
            response_data_format: 2,
            additional_length: 31,
            vendor: *b"HPE     ",
            product: *b"MSL3040         ",
            revision: *b"6.40",
        },
        chassis_designator: None,
        layout: ElementLayout {
            robot_address: 0,
            drive_start: 1,
            drive_count: 1,
            slot_start: 0x03e8,
            slot_count: 1,
            ie_start: 0x10,
            ie_count: 1,
        },
        drive_bays: vec![DriveBay {
            element_address: 1,
            accessible: true,
            exception: None,
            installed: Some(InstalledDrive {
                serial: "8031BDC7D1".to_string(),
                identity_source: IdentitySource::DvcidInline,
                vendor: Some("HPE".to_string()),
                product: Some("Ultrium 9-SCSI".to_string()),
                revision: Some("R1.0".to_string()),
                sg_path: Some(PathBuf::from("/dev/sg8")),
                sysfs_path: None,
            }),
            loaded: false,
            loaded_tape: None,
            source_slot: None,
        }],
        slots: vec![Slot {
            element_address: 0x03e9,
            accessible: true,
            exception: None,
            full: true,
            cartridge: Some("S30002L9".to_string()),
        }],
        ie_ports: vec![IePort {
            element_address: 0x10,
            accessible: true,
            exception: None,
            full: false,
            cartridge: None,
            import_enabled: true,
            export_enabled: true,
        }],
    }
}

fn foreign_drive_library(serial: &str, bays: &[(u16, &str, Option<&str>)]) -> Library {
    let mut library = test_library(serial);
    library.layout.drive_start = bays
        .iter()
        .map(|(element_address, _, _)| *element_address)
        .min()
        .unwrap_or(0);
    library.layout.drive_count = u16::try_from(bays.len()).expect("test bay count fits u16");
    library.drive_bays = bays
        .iter()
        .map(|(element_address, drive_serial, sg_path)| DriveBay {
            element_address: *element_address,
            accessible: true,
            exception: None,
            installed: Some(InstalledDrive {
                serial: (*drive_serial).to_string(),
                identity_source: IdentitySource::DvcidAndInquiry,
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                revision: Some("A1".to_string()),
                sg_path: sg_path.map(PathBuf::from),
                sysfs_path: None,
            }),
            loaded: false,
            loaded_tape: None,
            source_slot: None,
        })
        .collect();
    library
}

fn foreign_counter_snapshot(tape_alert_flags: Option<&str>) -> ForeignDriveSnapshot {
    ForeignDriveSnapshot {
        tape_alert_flags: tape_alert_flags.map(str::to_string),
        write_errors_corrected: Some(11),
        write_errors_uncorrected: Some(1),
        read_errors_corrected: Some(7),
        read_errors_uncorrected: Some(0),
    }
}

#[test]
fn foreign_poll_skips_same_serial_collision_rows_without_attribution() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-api-foreign-collision")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let report = DiscoveryReport {
        libraries: vec![foreign_drive_library(
            "d2lib",
            &[
                (0x0100, "DUPSER", Some("/dev/sg10")),
                (0x0101, "DUPSER", Some("/dev/sg11")),
            ],
        )],
        warnings: Vec::new(),
    };
    let mut index = CatalogIndex::open(&index_path).expect("open catalog");
    observe_drive_catalog_from_libraries(&mut index, report.libraries.iter(), &HashSet::new())
        .expect("observe duplicate serial foreign rows");
    let collision_rows = index.list_drives(true, false).expect("list drives");
    assert_eq!(collision_rows.len(), 2);
    assert!(
        collision_rows.iter().all(|drive| !drive.actionable),
        "duplicate serial rows must be non-actionable: {collision_rows:?}"
    );
    drop(index);

    let drives_cfg = remanence_state::DrivesConfig {
        foreign_tapealert: true,
        ..remanence_state::DrivesConfig::default()
    };
    let audit = AuditAppendContext {
        dir: temp.path().join("audit"),
        fsync: false,
        lock: Arc::new(std::sync::Mutex::new(())),
    };
    let mut reads = Vec::new();
    poll_foreign_drive_counters_once_with_reader(
        &index_path,
        &report,
        &drives_cfg,
        &HashSet::new(),
        &audit,
        |path, _foreign_tapealert| {
            reads.push(path.to_path_buf());
            Ok(foreign_counter_snapshot(Some("[20]")))
        },
    )
    .expect("poll foreign counters");

    assert!(
        reads.is_empty(),
        "ambiguous duplicate serial bays must not be polled or attributed: {reads:?}"
    );
    let index = CatalogIndex::open(&index_path).expect("reopen catalog");
    for drive in collision_rows {
        assert!(
            index
                .list_drive_health_snapshots(&drive.drive_uuid)
                .expect("list snapshots")
                .is_empty(),
            "collision row received a snapshot: {drive:?}"
        );
    }
    let active_alarms = index.list_alarms(false).expect("list active alarms");
    assert!(
        active_alarms
            .iter()
            .all(|alarm| alarm.kind != "foreign-drive-wants-cleaning"),
        "collision rows must not receive foreign cleaning advisories: {active_alarms:?}"
    );
}

#[test]
fn foreign_poll_attributes_unambiguous_row_by_library_and_element_address() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-api-foreign-unambiguous")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let report = DiscoveryReport {
        libraries: vec![foreign_drive_library(
            "d2lib",
            &[
                (0x0100, "FOREIGN_A", None),
                (0x0101, "FOREIGN_B", Some("/dev/sg-target")),
            ],
        )],
        warnings: Vec::new(),
    };
    let mut index = CatalogIndex::open(&index_path).expect("open catalog");
    observe_drive_catalog_from_libraries(&mut index, report.libraries.iter(), &HashSet::new())
        .expect("observe foreign rows");
    let other_drive = index
        .get_actionable_drive_at("d2lib", 0x0100)
        .expect("lookup other bay")
        .expect("other bay drive");
    let target_drive = index
        .get_actionable_drive_at("d2lib", 0x0101)
        .expect("lookup target bay")
        .expect("target bay drive");
    assert_eq!(other_drive.serial.as_deref(), Some("FOREIGN_A"));
    assert_eq!(target_drive.serial.as_deref(), Some("FOREIGN_B"));
    drop(index);

    let drives_cfg = remanence_state::DrivesConfig {
        foreign_tapealert: true,
        ..remanence_state::DrivesConfig::default()
    };
    let audit = AuditAppendContext {
        dir: temp.path().join("audit"),
        fsync: false,
        lock: Arc::new(std::sync::Mutex::new(())),
    };
    let mut reads = Vec::new();
    poll_foreign_drive_counters_once_with_reader(
        &index_path,
        &report,
        &drives_cfg,
        &HashSet::new(),
        &audit,
        |path, foreign_tapealert| {
            assert!(foreign_tapealert, "test config must request TapeAlert");
            reads.push(path.to_path_buf());
            Ok(foreign_counter_snapshot(Some("[20]")))
        },
    )
    .expect("poll foreign counters");

    assert_eq!(reads, vec![PathBuf::from("/dev/sg-target")]);
    let index = CatalogIndex::open(&index_path).expect("reopen catalog");
    assert!(
        index
            .list_drive_health_snapshots(&other_drive.drive_uuid)
            .expect("other snapshots")
            .is_empty(),
        "non-polled bay must not receive target snapshot"
    );
    let target_snapshots = index
        .list_drive_health_snapshots(&target_drive.drive_uuid)
        .expect("target snapshots");
    assert_eq!(target_snapshots.len(), 1);
    assert_eq!(target_snapshots[0].trigger, "foreign-counter");
    assert_eq!(target_snapshots[0].write_errors_corrected, Some(11));
    let audit_records = FileAuditLog::replay(audit.dir.as_path()).expect("replay poll audit");
    assert!(audit_records
        .iter()
        .any(|record| record.event == AuditEvent::DriveHealthObserved));
    assert!(audit_records
        .iter()
        .any(|record| record.event == AuditEvent::AlarmRaised));
    let advisory = index
        .list_alarms(false)
        .expect("list active alarms")
        .into_iter()
        .find(|alarm| alarm.kind == "foreign-drive-wants-cleaning")
        .expect("foreign advisory alarm");
    assert!(advisory
        .detail
        .as_deref()
        .is_some_and(|detail| { detail.contains("d2lib") && detail.contains("FOREIGN_B") }));
}

fn pool_state_with_tapes(tape_uuids: &[[u8; 16]]) -> ApiState {
    let mut index = test_index();
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: "camera.copy-a".to_string(),
            display_name: Some("Camera copy A".to_string()),
            copy_class: Some("copy-a".to_string()),
            content_class: Some("camera".to_string()),
            created_at_utc: Some("2026-05-28T09:00:00Z".to_string()),
        })
        .expect("project tape pool");
    for tape_uuid in tape_uuids {
        project_eligible_tape(&mut index, "camera.copy-a", *tape_uuid);
    }
    ApiState::new_with_pool_configs(index, vec![pool_config("camera.copy-a")])
}

fn state_with_operation() -> ApiState {
    let mut index = test_index();
    let operation_id = operation_uuid();
    let session_id = Uuid::from_u128(0x33);
    for record in [
        audit_record(
            1,
            AuditEvent::OperationStarted,
            operation_id,
            Some(session_id),
            detail(&[(
                "operation_kind",
                CborValue::Text("write_object".to_string()),
            )]),
        ),
        audit_record(
            2,
            AuditEvent::OperationFinished,
            operation_id,
            Some(session_id),
            detail(&[("response_fingerprint", CborValue::Bytes(vec![1, 2, 3, 4]))]),
        ),
    ] {
        index
            .project_audit_record(&record)
            .expect("project operation audit record");
    }
    ApiState::new(index)
}

fn state_with_queued_operation() -> ApiState {
    let mut index = test_index();
    let operation_id = operation_uuid();
    let record = audit_record(
        1,
        AuditEvent::RequestReceived,
        operation_id,
        None,
        detail(&[
            (
                "operation_kind",
                CborValue::Text("write_object".to_string()),
            ),
            ("request_fingerprint", CborValue::Bytes(vec![1, 2, 3])),
        ]),
    );
    index
        .project_audit_record(&record)
        .expect("project queued operation");
    ApiState::new(index)
}

fn state_with_failed_operation() -> ApiState {
    let mut index = test_index();
    let operation_id = operation_uuid();
    let record = audit_record(
        1,
        AuditEvent::OperationFailed,
        operation_id,
        None,
        detail(&[
            ("operation_kind", CborValue::Text("clean_drive".to_string())),
            (
                "error_summary",
                CborValue::Text("no eligible cleaning cartridge: CLNU01L9 is expired".to_string()),
            ),
        ]),
    );
    index
        .project_audit_record(&record)
        .expect("project failed operation");
    ApiState::new(index)
}

#[derive(Debug)]
struct TestForeignAdapter;

impl FormatDescriptor for TestForeignAdapter {
    fn id(&self) -> &'static str {
        "test-foreign-v1"
    }

    fn version(&self) -> &'static str {
        "test"
    }

    fn source_requirement(&self) -> SourceRequirement {
        SourceRequirement::ByteStreamDump
    }

    fn capabilities(&self) -> FormatCapabilities {
        FormatCapabilities {
            catalog_scan: true,
            ..FormatCapabilities::default()
        }
    }
}

impl ForeignFormatAdapter for TestForeignAdapter {
    fn aliases(&self) -> &'static [&'static str] {
        &["test-foreign"]
    }

    fn supported_sources(&self) -> &'static [SourceRequirement] {
        &[SourceRequirement::ByteStreamDump]
    }

    fn open_dump_reader<'a>(
        &self,
        _source: Box<dyn remanence_format_driver::ReadSeek + 'a>,
        adapter_state: &[u8],
    ) -> Result<Box<dyn ArchiveReader + 'a>, FormatError> {
        if adapter_state != b"test-resume-state" {
            return Err(FormatError::invalid("missing persisted adapter state"));
        }
        Ok(Box::new(TestForeignReader))
    }
}

struct TestForeignReader;

impl ArchiveReader for TestForeignReader {
    fn scan(&mut self, sink: &mut dyn EntryCatalogSink) -> Result<ScanReport, FormatError> {
        sink.entry(&NormalizedEntry {
            file_id: FileId::from("test:1"),
            path: "camera/a.txt".to_string(),
            kind: EntryKind::RegularFile,
            link_target: None,
            size_bytes: Some(3),
            adapter_state: Vec::new(),
        })?;
        sink.archive_gap(&ArchiveGapRange {
            source_start: 8,
            source_end: 16,
            cause: ArchiveGapCause::UnrecognizedData,
            adapter_state: Vec::new(),
        })?;
        Ok(ScanReport {
            entries: 1,
            damage_events: 0,
            archive_gaps: 1,
            integrity_basis: ScanIntegrityBasis::Unknown,
        })
    }

    fn stream_all(
        &mut self,
        _sink: &mut dyn ArchiveEventSink,
    ) -> Result<StreamReport, FormatError> {
        Err(FormatError::unsupported("test adapter is scan-only"))
    }

    fn stream_file(
        &mut self,
        _file_id: &FileId,
        _sink: &mut dyn FileDataSink,
    ) -> Result<FileStreamReport, FormatError> {
        Err(FormatError::unsupported("test adapter is scan-only"))
    }
}

fn foreign_test_state() -> (tempfile::TempDir, ApiState, String) {
    let temp = tempfile::tempdir().expect("create foreign fixture directory");
    let source = temp.path().join("fixture.bin");
    std::fs::write(&source, b"test").expect("write foreign fixture");
    let mut index = test_index();
    let unit_id = index
        .upsert_foreign_archive_projection(ForeignArchiveProjectionInput {
            tape_uuid: Vec::new(),
            format_id: "test-foreign-v1".to_string(),
            scan_id: "scan-test-1".to_string(),
            source_kind: "byte_stream_dump".to_string(),
            source_id: source.to_string_lossy().into_owned(),
            confidence: "high".to_string(),
            entry_count: 1,
            damage_event_count: 1,
            adapter_state: b"test-resume-state".to_vec(),
            last_scan_at_utc: Some("2026-05-28T13:15:00Z".to_string()),
            created_at_utc: Some("2026-05-28T13:15:01Z".to_string()),
        })
        .expect("project foreign unit");
    let mut registry = ForeignFormatRegistry::new();
    registry.register(Arc::new(TestForeignAdapter)).unwrap();
    (
        temp,
        ApiState::new(index).with_foreign_formats(registry),
        unit_id,
    )
}

fn audit_record(
    sequence: u64,
    event: AuditEvent,
    operation_id: Uuid,
    session_id: Option<Uuid>,
    detail: BTreeMap<String, CborValue>,
) -> AuditRecord {
    AuditRecord {
        schema_version: 1,
        record_uuid: Uuid::from_u128(sequence as u128),
        sequence,
        timestamp_utc: format!("2026-05-28T13:15:0{sequence}Z"),
        host_id: "host".to_string(),
        process_id: 123,
        software_build: Some("test-build".to_string()),
        actor: AuditActor::System,
        source_layer: SourceLayer::Layer5,
        operation_id: Some(operation_id),
        session_id,
        idempotency_key: None,
        event,
        subject: AuditSubject {
            kind: "object".to_string(),
            id: Some("subject-1".to_string()),
        },
        detail,
    }
}

fn detail(entries: &[(&str, CborValue)]) -> BTreeMap<String, CborValue> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect()
}

fn sha256_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

/// Test sink that injects a deterministic write failure after a fixed
/// number of successful tape blocks while preserving all captured writes.
#[derive(Debug)]
struct FailAfterBlocksSink {
    inner: VecBlockSink,
    max_successful_blocks: usize,
}

impl FailAfterBlocksSink {
    fn new(max_successful_blocks: usize) -> Self {
        Self {
            inner: VecBlockSink::new(),
            max_successful_blocks,
        }
    }
}

impl BlockSink for FailAfterBlocksSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        if self.inner.blocks.len() >= self.max_successful_blocks {
            return Err(TapeIoError::OperationFailed(
                "injected write_block failure".to_string(),
            ));
        }
        self.inner.write_block(buf)
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

#[derive(Debug)]
struct BatchedVecSink {
    inner: VecBlockSink,
    batch_blocks: u32,
    batch_calls: u64,
}

impl BatchedVecSink {
    fn new(batch_blocks: u32) -> Self {
        assert!(batch_blocks > 1, "test batch size must exercise batching");
        Self {
            inner: VecBlockSink::new(),
            batch_blocks,
            batch_calls: 0,
        }
    }
}

impl BlockSink for BatchedVecSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.inner.write_block(buf)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        self.batch_calls = self.batch_calls.saturating_add(1);
        self.inner.write_block_batch(buf, block_size_bytes)
    }

    fn write_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
        self.batch_blocks
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.batch_blocks
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

/// Test sink that reports one partial fixed batch after the bootstrap has
/// been written, modeling an object body that cannot be committed.
#[derive(Debug)]
struct PartialBatchSink {
    inner: VecBlockSink,
    batch_blocks: u32,
    partial_records: u32,
    injected: bool,
}

impl PartialBatchSink {
    fn new(batch_blocks: u32, partial_records: u32) -> Self {
        assert!(batch_blocks > 1, "test batch size must exercise batching");
        assert!(
            partial_records > 0 && partial_records < batch_blocks,
            "partial batch must write a nonzero prefix"
        );
        Self {
            inner: VecBlockSink::new(),
            batch_blocks,
            partial_records,
            injected: false,
        }
    }
}

impl BlockSink for PartialBatchSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.inner.write_block(buf)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let block_size = usize::try_from(block_size_bytes).expect("test block size fits usize");
        assert_eq!(
            buf.len() % block_size,
            0,
            "test batch buffer must contain whole records"
        );
        let records = u32::try_from(buf.len() / block_size).expect("test record count fits");
        if !self.injected && records > self.partial_records {
            self.injected = true;
            let partial_len =
                usize::try_from(self.partial_records).expect("partial count fits") * block_size;
            return self
                .inner
                .write_block_batch(&buf[..partial_len], block_size_bytes);
        }
        self.inner.write_block_batch(buf, block_size_bytes)
    }

    fn write_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
        self.batch_blocks
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.batch_blocks
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

/// Test sink that reports a full record count but a short byte count for
/// one object-body batch. This models inconsistent completion accounting
/// at the physical submission boundary.
#[derive(Debug)]
struct ShortByteBatchSink {
    inner: VecBlockSink,
    batch_blocks: u32,
    bootstrap_delimited: bool,
    injected: bool,
}

impl ShortByteBatchSink {
    fn new(batch_blocks: u32) -> Self {
        assert!(batch_blocks > 1, "test batch size must exercise batching");
        Self {
            inner: VecBlockSink::new(),
            batch_blocks,
            bootstrap_delimited: false,
            injected: false,
        }
    }
}

impl BlockSink for ShortByteBatchSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.inner.write_block(buf)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let outcome = self.inner.write_block_batch(buf, block_size_bytes)?;
        if self.bootstrap_delimited && !self.injected {
            self.injected = true;
            return Ok(WriteBatchOutcome::from_computed_position(
                outcome.records_written,
                outcome
                    .bytes_written
                    .checked_sub(1)
                    .expect("nonempty test batch"),
                outcome.early_warning,
                outcome.end_of_medium,
                outcome.position_after,
            ));
        }
        Ok(outcome)
    }

    fn write_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
        self.batch_blocks
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.batch_blocks
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        let outcome = self.inner.write_filemarks(count)?;
        self.bootstrap_delimited = true;
        Ok(outcome)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

#[derive(Debug)]
struct PositionDriftBatchSink {
    inner: VecBlockSink,
    batch_blocks: u32,
    injected: bool,
}

impl PositionDriftBatchSink {
    fn new(batch_blocks: u32) -> Self {
        assert!(batch_blocks > 1, "test batch size must exercise batching");
        Self {
            inner: VecBlockSink::new(),
            batch_blocks,
            injected: false,
        }
    }
}

impl BlockSink for PositionDriftBatchSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.inner.write_block(buf)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let block_size = usize::try_from(block_size_bytes).expect("test block size fits usize");
        let records = u32::try_from(buf.len() / block_size).expect("test record count fits");
        if !self.injected && records > 1 {
            self.injected = true;
            return Err(TapeIoError::OperationFailed(
                    "position drift: expected_partition=0 expected_lba=11 device_partition=0 device_lba=12"
                        .to_string(),
                ));
        }
        self.inner.write_block_batch(buf, block_size_bytes)
    }

    fn write_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
        self.batch_blocks
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.batch_blocks
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

fn assert_no_pool_write_catalog_reference(
    index: &CatalogIndex,
    caller_object_id: &str,
    tape_uuid: [u8; 16],
) {
    assert!(
        index
            .get_native_object_by_caller_object_id(caller_object_id)
            .expect("query caller object id")
            .is_none(),
        "failed write must not leave an object row"
    );
    assert!(
        index
            .list_native_objects()
            .expect("list native objects")
            .is_empty(),
        "failed write must not leave any native object rows"
    );
    assert!(
        index
            .list_tape_files(&tape_uuid)
            .expect("list tape files")
            .is_empty(),
        "failed write must not leave committed tape-file rows"
    );
}

#[test]
fn select_tape_in_pool_returns_unique_eligible_tape() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_eligible_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);

    let cfg = pool_config("camera.copy-a");
    let selected = select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("select tape");

    assert_eq!(selected.pool_id, "camera.copy-a");
    assert_eq!(selected.tape_uuid, POOL_WRITE_TAPE_UUID);
    assert_eq!(selected.block_size, API_SESSION_BLOCK_SIZE);
    match selected.parity_config {
        ParityConfig::Scheme(ref scheme) => assert_eq!(scheme, &test_scheme()),
        ParityConfig::None => panic!("expected parity scheme"),
    }
}

#[test]
fn select_tape_in_pool_accepts_no_parity_tape_geometry() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_no_parity_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);

    let cfg = pool_config("camera.copy-a");
    let selected = select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("select tape");

    assert_eq!(selected.pool_id, "camera.copy-a");
    assert_eq!(selected.tape_uuid, POOL_WRITE_TAPE_UUID);
    assert_eq!(selected.block_size, API_SESSION_BLOCK_SIZE);
    assert!(matches!(selected.parity_config, ParityConfig::None));
}

#[test]
fn tape_io_fence_refuses_pool_selection_until_operator_release() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_no_parity_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
    let fence = index
        .record_tape_io_fence(remanence_state::TapeIoFenceInput {
            tape_uuid: POOL_WRITE_TAPE_UUID,
            barcode: Some("RMN004L9".to_string()),
            reason: "partial_batch".to_string(),
            evidence_json: Some("{\"written_records\":2}".to_string()),
        })
        .expect("record active tape-I/O fence");

    let cfg = pool_config("camera.copy-a");
    let err = select_tape_in_pool(&index, &cfg, 123, &HashSet::new())
        .expect_err("active tape-I/O fence must block selection");
    match err {
        SelectTapeError::NoWritableTapes { pool_id, reasons } => {
            assert_eq!(pool_id, "camera.copy-a");
            assert!(
                reasons.iter().any(|reason| matches!(
                    reason,
                    WritabilityError::TapeIoFence {
                        quarantine_id,
                        reason,
                    } if quarantine_id == &fence.quarantine_id && reason == "partial_batch"
                )),
                "{reasons:?}"
            );
        }
        other => panic!("unexpected selection error: {other}"),
    }

    index
        .release_tape_io_fence(&fence.quarantine_id, "operator released")
        .expect("release active tape-I/O fence")
        .expect("released fence");
    let selected =
        select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("selection after release");
    assert_eq!(selected.tape_uuid, POOL_WRITE_TAPE_UUID);
}

#[test]
fn startup_refuses_active_tape_io_fence_until_operator_release() {
    let mut index = test_index();
    let fence = index
        .record_tape_io_fence(remanence_state::TapeIoFenceInput {
            tape_uuid: POOL_WRITE_TAPE_UUID,
            barcode: Some("RMN004L9".to_string()),
            reason: "position_drift".to_string(),
            evidence_json: Some("{\"expected_lba\":9,\"device_lba\":8}".to_string()),
        })
        .expect("record startup fence");

    let status = reject_active_tape_io_fences_on_startup(&index)
        .expect_err("active tape-I/O fence must block startup");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(status.message().contains(&fence.quarantine_id));
    assert!(status.message().contains("position_drift"));

    index
        .release_tape_io_fence(&fence.quarantine_id, "operator released")
        .expect("release startup fence")
        .expect("released fence");
    reject_active_tape_io_fences_on_startup(&index).expect("startup after release");
}

#[test]
fn select_tape_in_pool_prefers_appendable_written_no_parity_tape() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_no_parity_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
    project_no_parity_tape(&mut index, "camera.copy-a", SECOND_POOL_WRITE_TAPE_UUID);
    project_no_parity_tape_usage(&mut index, POOL_WRITE_TAPE_UUID, 7);

    let cfg = pool_config("camera.copy-a");
    let selected =
        select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("select append tape");

    assert_eq!(selected.pool_id, "camera.copy-a");
    assert_eq!(selected.tape_uuid, POOL_WRITE_TAPE_UUID);
    assert_eq!(selected.block_size, API_SESSION_BLOCK_SIZE);
    assert!(matches!(selected.parity_config, ParityConfig::None));
}

#[test]
fn checkpoint_selection_skips_journal_less_legacy_tape() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_no_parity_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
    project_no_parity_tape(&mut index, "camera.copy-a", SECOND_POOL_WRITE_TAPE_UUID);
    project_no_parity_tape_usage(&mut index, POOL_WRITE_TAPE_UUID, 7);
    let checkpoint_dir = temp_dir("remanence-batched-selection-mixed");
    let cfg = pool_config("camera.copy-a");

    let batched = crate::pool_write::select_tape_in_pool_for_write_session(
        &index,
        &cfg,
        123,
        &HashSet::new(),
        checkpoint_dir.as_path(),
    )
    .expect("batched selection should use the fresh tape");
    assert_eq!(batched.tape_uuid, SECOND_POOL_WRITE_TAPE_UUID);
}

#[test]
fn batched_selection_accepts_non_fresh_tape_with_checkpoint_journal() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_no_parity_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
    project_no_parity_tape_usage(&mut index, POOL_WRITE_TAPE_UUID, 3);
    let checkpoint_dir = temp_dir("remanence-batched-selection-journaled");
    append_checkpoint_record(checkpoint_dir.as_path(), POOL_WRITE_TAPE_UUID);

    let selected = crate::pool_write::select_tape_in_pool_for_write_session(
        &index,
        &pool_config("camera.copy-a"),
        123,
        &HashSet::new(),
        checkpoint_dir.as_path(),
    )
    .expect("checkpoint journal should make a non-fresh tape eligible");

    assert_eq!(selected.tape_uuid, POOL_WRITE_TAPE_UUID);
}

#[test]
fn checkpoint_selection_reports_every_accept_sealed_legacy_candidate() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_no_parity_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
    project_no_parity_tape(&mut index, "camera.copy-a", SECOND_POOL_WRITE_TAPE_UUID);
    project_no_parity_tape_usage(&mut index, POOL_WRITE_TAPE_UUID, 3);
    project_no_parity_tape_usage(&mut index, SECOND_POOL_WRITE_TAPE_UUID, 5);
    let checkpoint_dir = temp_dir("remanence-batched-selection-ineligible");

    let error = crate::pool_write::select_tape_in_pool_for_write_session(
        &index,
        &pool_config("camera.copy-a"),
        123,
        &HashSet::new(),
        checkpoint_dir.as_path(),
    )
    .expect_err("journal-less non-fresh pool must fail before ranking");
    let message = error.to_string();

    let SelectTapeError::NoBatchedEligibleTapes {
        pool_id,
        ineligible_candidates,
    } = error
    else {
        panic!("expected batched eligibility error, got {message}");
    };
    assert_eq!(pool_id, "camera.copy-a");
    assert_eq!(ineligible_candidates.len(), 2);
    assert!(message.contains("RMN004L9"), "{message}");
    assert!(message.contains("RMN005L9"), "{message}");
    assert!(message.contains("accept-sealed"), "{message}");
    assert!(message.contains("re-initialized before reuse"), "{message}");
}

#[test]
fn select_tape_in_pool_reports_unknown_pool() {
    let index = test_index();

    let cfg = pool_config("missing.pool");
    let err = select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect_err("unknown pool");

    assert!(matches!(
        err,
        SelectTapeError::UnknownPool { ref pool_id } if pool_id == "missing.pool"
    ));
}

#[test]
fn select_tape_in_pool_reports_empty_pool() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");

    let cfg = pool_config("camera.copy-a");
    let err = select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect_err("empty pool");

    assert!(matches!(
        err,
        SelectTapeError::EmptyPool { ref pool_id } if pool_id == "camera.copy-a"
    ));
}

#[test]
fn select_tape_in_pool_uses_policy_for_multiple_eligible_tapes() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_eligible_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
    project_eligible_tape(&mut index, "camera.copy-a", SECOND_POOL_WRITE_TAPE_UUID);

    let cfg = pool_config("camera.copy-a");
    let selected = select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("select tape");

    assert_eq!(selected.pool_id, "camera.copy-a");
    assert_eq!(selected.tape_uuid, POOL_WRITE_TAPE_UUID);
}

#[test]
fn write_session_selection_respects_the_physical_library_scope() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_eligible_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
    project_eligible_tape(&mut index, "camera.copy-a", SECOND_POOL_WRITE_TAPE_UUID);
    let cfg = pool_config("camera.copy-a");
    let allowed = HashSet::from([SECOND_POOL_WRITE_TAPE_UUID]);
    let checkpoint_dir = tempfile::tempdir().expect("checkpoint dir");

    let selected = crate::pool_write::select_tape_in_pool_for_write_session_scoped(
        &index,
        &cfg,
        123,
        &HashSet::new(),
        checkpoint_dir.path(),
        &allowed,
    )
    .expect("select tape from physical library scope");

    assert_eq!(selected.tape_uuid, SECOND_POOL_WRITE_TAPE_UUID);
}

#[test]
fn select_tape_in_pool_excludes_reserved_preferred_tape() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_eligible_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
    project_eligible_tape(&mut index, "camera.copy-a", SECOND_POOL_WRITE_TAPE_UUID);
    let reserved = [POOL_WRITE_TAPE_UUID].into_iter().collect();

    let cfg = pool_config("camera.copy-a");
    let selected =
        select_tape_in_pool(&index, &cfg, 123, &reserved).expect("select unreserved tape");

    assert_eq!(selected.pool_id, "camera.copy-a");
    assert_eq!(selected.tape_uuid, SECOND_POOL_WRITE_TAPE_UUID);
}

#[test]
fn select_tape_in_pool_errors_when_only_eligible_tape_is_reserved() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_eligible_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
    let reserved = [POOL_WRITE_TAPE_UUID].into_iter().collect();

    let cfg = pool_config("camera.copy-a");
    let err = select_tape_in_pool(&index, &cfg, 123, &reserved)
        .expect_err("reserved-only pool must fail");

    assert!(matches!(
        err,
        SelectTapeError::NoUnreservedWritableTapes {
            ref pool_id,
            reserved_tape_count: 1,
        } if pool_id == "camera.copy-a"
    ));
}

#[test]
fn select_tape_in_pool_uses_partially_written_no_parity_tape_to_complete() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_no_parity_tape_with_block_size(
        &mut index,
        "camera.copy-a",
        POOL_WRITE_TAPE_UUID,
        API_SESSION_BLOCK_SIZE,
    );
    project_no_parity_tape_with_block_size(
        &mut index,
        "camera.copy-a",
        SECOND_POOL_WRITE_TAPE_UUID,
        API_SESSION_BLOCK_SIZE,
    );
    let cfg = pool_config_with_watermarks("camera.copy-a", 0.0001, 0.0002, 0);
    let low_bytes = watermark_floor_bytes(raw_capacity_bytes(LtoGen::Lto9), cfg.watermark_low)
        .expect("low watermark");
    let object_size = u64::from(API_SESSION_BLOCK_SIZE) * 2;
    let ordinals_before_low = low_bytes / u64::from(API_SESSION_BLOCK_SIZE);
    project_no_parity_tape_usage(
        &mut index,
        SECOND_POOL_WRITE_TAPE_UUID,
        ordinals_before_low - 1,
    );

    let selected = select_tape_in_pool(&index, &cfg, object_size, &HashSet::new())
        .expect("select completing append tape");

    assert_eq!(selected.tape_uuid, SECOND_POOL_WRITE_TAPE_UUID);
}

#[test]
fn select_tape_in_pool_reuses_appendable_no_parity_tape_before_fresh_empty_tape() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_no_parity_tape_with_block_size(
        &mut index,
        "camera.copy-a",
        POOL_WRITE_TAPE_UUID,
        API_SESSION_BLOCK_SIZE,
    );
    project_no_parity_tape_with_block_size(
        &mut index,
        "camera.copy-a",
        SECOND_POOL_WRITE_TAPE_UUID,
        API_SESSION_BLOCK_SIZE,
    );
    let cfg = pool_config_with_watermarks("camera.copy-a", 0.0001, 0.0002, 0);
    let low_bytes = watermark_floor_bytes(raw_capacity_bytes(LtoGen::Lto9), cfg.watermark_low)
        .expect("low watermark");
    let object_size = u64::from(API_SESSION_BLOCK_SIZE) * 2;
    let ordinals_before_low = low_bytes / u64::from(API_SESSION_BLOCK_SIZE);
    project_no_parity_tape_usage(&mut index, POOL_WRITE_TAPE_UUID, ordinals_before_low - 3);

    let selected = select_tape_in_pool(&index, &cfg, object_size, &HashSet::new())
        .expect("select appendable tape");

    assert_eq!(selected.tape_uuid, POOL_WRITE_TAPE_UUID);
}

#[test]
fn select_tape_in_pool_enforces_capacity_invariant_against_lto_capacity() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_eligible_tape_with_voltag(
        &mut index,
        "camera.copy-a",
        POOL_WRITE_TAPE_UUID,
        "RMN004L1",
    );
    let cfg = pool_config_with_watermarks("camera.copy-a", 0.10, 0.11, 1_000_000_001);

    let err = select_tape_in_pool(&index, &cfg, 1, &HashSet::new())
        .expect_err("too-narrow watermark band must reject");

    assert!(
        matches!(
            err,
            SelectTapeError::State(StateError::ConfigInvalid(ref message))
                if message.contains("watermark band")
        ),
        "{err}"
    );
}

#[test]
fn seal_decision_uses_actual_position_inclusive_boundary() {
    assert_eq!(
        seal_decision_after_write(
            TapePositionAfterWrite {
                used_bytes: 100,
                early_warning: false,
            },
            100,
            None,
        ),
        Some(TapeSealReason::ReachedLowWatermark)
    );
}

#[test]
fn seal_decision_keeps_below_low_tape_active_without_force_or_early_warning() {
    assert_eq!(
        seal_decision_after_write(
            TapePositionAfterWrite {
                used_bytes: 99,
                early_warning: false,
            },
            100,
            None,
        ),
        None
    );
}

#[test]
fn seal_decision_hardware_early_warning_wins_below_low() {
    assert_eq!(
        seal_decision_after_write(
            TapePositionAfterWrite {
                used_bytes: 10,
                early_warning: true,
            },
            100,
            None,
        ),
        Some(TapeSealReason::HardwareEarlyWarning)
    );
}

#[test]
fn seal_decision_honors_force_seal_valve_below_low() {
    assert_eq!(
        seal_decision_after_write(
            TapePositionAfterWrite {
                used_bytes: 10,
                early_warning: false,
            },
            100,
            Some(TapeSealReason::NoPendingObjectFits),
        ),
        Some(TapeSealReason::NoPendingObjectFits)
    );
}

#[test]
fn write_object_to_pool_returns_locator_commits_catalog_and_round_trips_payload() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_eligible_tape_with_block_size(
        &mut index,
        "camera.copy-a",
        POOL_WRITE_TAPE_UUID,
        BLOCK_SIZE,
    );
    let source_dir = temp_dir("remanence-api-pool-write-src");
    let restore_dir = temp_dir("remanence-api-pool-write-restore");
    let source_path = source_dir.join("payload.bin");
    let payload = b"pool targeted write payload".to_vec();
    std::fs::write(&source_path, &payload).expect("write source payload");
    let expected_hash = sha256_bytes(&payload);
    let mut tape_sink = VecBlockSink::new();
    let cfg = pool_config_with_block_size("camera.copy-a", BLOCK_SIZE);

    let result = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: " camera.copy-a ".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-pool-core".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("write object to pool");

    assert_eq!(result.object.caller_object_id, "caller-pool-core");
    assert_eq!(result.object.content_sha256.to_vec(), expected_hash);
    assert_eq!(result.object.logical_size_bytes, payload.len() as u64);
    assert_eq!(result.object.body_format, "rem-object-v1");
    assert_eq!(result.object.copies.len(), 1);
    let copy = &result.object.copies[0];
    assert_eq!(copy.tape_uuid, POOL_WRITE_TAPE_UUID);
    assert_eq!(copy.pool_id, "camera.copy-a");
    assert_eq!(
        copy.tape_file_number,
        result.expect_write_report().object_close.tape_file_number
    );
    assert_eq!(
        copy.first_body_lba,
        result.expect_write_report().catalog.files[0]
            .first_chunk_lba
            .expect("payload lba")
            .0
    );

    let committed = index
        .get_native_object(result.object.object_id_text().as_str())
        .expect("query committed object")
        .expect("committed object exists");
    assert_eq!(
        committed.caller_object_id.as_deref(),
        Some("caller-pool-core")
    );
    assert_eq!(
        committed.content_hash.as_deref(),
        Some(expected_hash.as_slice())
    );
    assert_eq!(committed.copies.len(), 1);
    assert_eq!(
        committed.copies[0].pool_id.as_deref(),
        Some(copy.pool_id.as_str())
    );
    assert_eq!(committed.copies[0].first_body_lba, copy.first_body_lba);
    let projected_file = &result.expect_write_report().catalog.files[0];
    let committed_file = index
        .get_native_object_file(
            result.object.object_id_text().as_str(),
            projected_file.file_id.as_str(),
        )
        .expect("query committed object file")
        .expect("committed object file exists");
    assert_eq!(committed_file.path, "payload.bin");
    assert_eq!(
        committed_file.size_bytes,
        u64::try_from(payload.len()).expect("payload length fits u64")
    );
    assert_eq!(committed_file.file_sha256, expected_hash);
    assert_eq!(
        committed_file.first_chunk_lba,
        projected_file.first_chunk_lba.map(|lba| lba.0)
    );
    assert_eq!(committed_file.chunk_count, projected_file.chunk_count);

    let object_block_start = 1usize;
    let object_block_count =
        usize::try_from(result.expect_write_report().object_close.data_block_count)
            .expect("object block count fits usize");
    let object_blocks =
        tape_sink.blocks[object_block_start..object_block_start + object_block_count].to_vec();
    let mut object_source = VecBlockSource::new(object_blocks);
    let restore = restore_object_to_directory(
        &mut object_source,
        BLOCK_SIZE as usize,
        result.expect_write_report().layout.projected_size_blocks,
        &restore_dir,
        FilesystemRestoreOptions::default(),
    )
    .expect("restore object");

    assert_eq!(restore.files_written, 1);
    assert_eq!(
        std::fs::read(restore_dir.join("payload.bin")).unwrap(),
        payload
    );
}

#[test]
fn no_parity_write_round_trips_payload_and_commits_without_parity_geometry() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-no-parity-src");
    let source_path = source_dir.join("payload.bin");
    let payload = b"scenario-a no parity payload".to_vec();
    std::fs::write(&source_path, &payload).expect("write source payload");
    let mut tape_sink = VecBlockSink::new();
    let cfg = pool_config("scenario-a");

    let result = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-no-parity".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("write no-parity object");

    assert_eq!(tape_sink.filemarks, vec![1, 1]);
    assert_eq!(
        result
            .expect_write_report()
            .catalog
            .object_copy
            .first_parity_data_ordinal,
        None
    );
    assert_eq!(
        result
            .expect_write_report()
            .catalog
            .object_copy
            .protected_until_ordinal,
        None
    );
    let bootstrap = parse_bootstrap_block(&tape_sink.blocks[0]).expect("parse no-parity bootstrap");
    assert!(bootstrap.no_parity_flag);
    assert!(bootstrap.scheme.is_none());
    assert_eq!(bootstrap.tape_uuid, POOL_WRITE_TAPE_UUID);

    let mut source = VecBlockSource::new(tape_sink.blocks.clone());
    verify_tape_identity(&mut source, &POOL_WRITE_TAPE_UUID)
        .expect("verify matching no-parity bootstrap");
    let read = read_rem_tar_object(
        &mut source,
        API_SESSION_BLOCK_SIZE as usize,
        result.expect_write_report().layout.projected_size_blocks,
    )
    .expect("read no-parity REM-OBJECT object");
    assert_eq!(
        read.entry("payload.bin").expect("payload entry").data,
        payload
    );

    let tape = index
        .get_tape(&POOL_WRITE_TAPE_UUID)
        .expect("query tape")
        .expect("tape row");
    assert_eq!(tape.scheme_id, None);
    assert_eq!(tape.data_blocks_per_stripe, None);
    assert_eq!(tape.parity_blocks_per_stripe, None);
    assert_eq!(tape.stripes_per_neighborhood, None);

    let committed = index
        .get_native_object(result.object.object_id_text().as_str())
        .expect("query committed no-parity object")
        .expect("committed no-parity object exists");
    assert_eq!(
        committed.metadata_hash.as_deref(),
        Some(&result.expect_write_report().catalog.object.manifest_sha256[..])
    );
    assert_eq!(committed.copies.len(), 1);
    assert_eq!(committed.copies[0].first_parity_data_ordinal, None);
    assert_eq!(committed.copies[0].protected_until_ordinal, None);
}

#[test]
fn fresh_tape_first_object_span_excludes_bootstrap_prefix_and_trailing_filemark() {
    // The panel's named fixture, no-parity flavor: the fresh-tape
    // bootstrap prefix (bootstrap block at LBA 0 + its filemark at 1)
    // precedes the first object's start and is excluded at capture, so
    // the object's span is [2, 2 + block_count) — exclusive — with the
    // trailing filemark at 2 + block_count outside the span.
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-fresh-span-src");
    let source_path = source_dir.join("payload.bin");
    std::fs::write(&source_path, b"fresh tape span payload").expect("write source payload");
    let mut tape_sink = VecBlockSink::new();
    let cfg = pool_config("scenario-a");

    let result = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-fresh-span".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("write no-parity object");

    let report = result.expect_write_report();
    let entries = &report.catalog.tape_file_bundle.entries;
    assert_eq!(entries.len(), 2, "fresh tape: bootstrap prefix + object");
    let bootstrap = &entries[0];
    assert_eq!(bootstrap.kind, remanence_parity::TapeFileKind::Bootstrap);
    assert_eq!(
        bootstrap.physical_start_hint,
        Some(0),
        "the fresh-tape bootstrap prefix starts at BOT — Some(0), a valid position, not absent"
    );
    let object = &entries[1];
    assert_eq!(object.kind, remanence_parity::TapeFileKind::Object);
    assert_eq!(
        object.physical_start_hint,
        Some(2),
        "the first object's start sits after the bootstrap prefix, never at 0"
    );
    assert_eq!(object.block_count, report.layout.projected_size_blocks);
    // Exclusive span: the trailing filemark at start + block_count is
    // outside it, so the position after that filemark is one more.
    let span_end_exclusive = object.physical_start_hint.expect("captured") + object.block_count;
    assert_eq!(
        report.object_close.filemark_outcome.position_after.lba,
        span_end_exclusive + 1,
        "start + block_count addresses the trailing filemark, outside the span"
    );
    assert_eq!(report.object_close.physical_start_lba, Some(2));

    // The catalog read path serves the same span through the
    // copy→tape-file join — the fact the wire fields project.
    let committed = index
        .get_native_object(result.object.object_id_text().as_str())
        .expect("query committed object")
        .expect("committed object exists");
    assert_eq!(committed.copies.len(), 1);
    assert_eq!(committed.copies[0].global_start_block, Some(2));
    assert_eq!(
        committed.copies[0].global_end_block,
        Some(span_end_exclusive)
    );
}

#[test]
fn no_parity_stored_images_cross_read_between_serial_and_batched_paths() {
    let source_dir = temp_dir("remanence-api-cross-version-src");
    let source_path = source_dir.join("payload.bin");
    let payload = (0..(API_SESSION_BLOCK_SIZE as usize * 6 + 123))
        .map(|value| u8::try_from(value % 251).unwrap())
        .collect::<Vec<_>>();
    std::fs::write(&source_path, &payload).expect("write source payload");
    let cfg = pool_config("scenario-a");

    let mut batched_index = test_index();
    project_pool(&mut batched_index, "scenario-a");
    project_no_parity_tape(&mut batched_index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let mut batched_sink = BatchedVecSink::new(4);
    let batched = write_object_to_pool(
        &mut batched_index,
        &mut batched_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path.clone()),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-batched-image".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("write batched image");
    assert!(
        batched_sink.batch_calls > 0,
        "test must exercise write_block_batch"
    );
    let batched_block_count =
        usize::try_from(batched.expect_write_report().object_close.data_block_count)
            .expect("batched block count fits usize");
    let mut old_reader =
        VecBlockSource::new(batched_sink.inner.blocks[1..1 + batched_block_count].to_vec());
    let old_read = read_rem_tar_object(
        &mut old_reader,
        API_SESSION_BLOCK_SIZE as usize,
        batched.expect_write_report().layout.projected_size_blocks,
    )
    .expect("old single-block reader reads batched image");
    assert_eq!(
        old_read
            .entry("payload.bin")
            .expect("payload entry")
            .data
            .as_slice(),
        payload.as_slice()
    );

    let mut serial_index = test_index();
    project_pool(&mut serial_index, "scenario-a");
    project_no_parity_tape(&mut serial_index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let mut serial_sink = VecBlockSink::new();
    let serial = write_object_to_pool(
        &mut serial_index,
        &mut serial_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-serial-image".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("write serial image");
    let serial_block_count =
        usize::try_from(serial.expect_write_report().object_close.data_block_count)
            .expect("serial block count fits usize");
    let mut batched_reader =
        VecBlockSource::new(serial_sink.blocks[1..1 + serial_block_count].to_vec())
            .with_read_batch_blocks_for_test(4);
    let mut restored = Vec::new();
    let mut capture = crate::read_core::CapturePayloadSink::new(&mut restored);
    crate::read_core::read_object_payload(
        &mut batched_reader,
        API_SESSION_BLOCK_SIZE as usize,
        serial.expect_write_report().layout.projected_size_blocks,
        0,
        None,
        &mut capture,
    )
    .expect("new batched reader reads serial image");
    let (bytes_written, _digest) = capture.finish().expect("finish capture");
    assert_eq!(bytes_written, payload.len() as u64);
    assert_eq!(restored, payload);
    assert!(
        batched_reader.calls.iter().any(|call| matches!(
            call,
            remanence_library::VecBlockSourceCall::ReadBlockBatch {
                requested_records,
                ..
            } if *requested_records > 1
        )),
        "test must exercise read_block_batch: {:?}",
        batched_reader.calls
    );
}

#[test]
fn partial_batched_no_parity_write_is_uncommittable_and_fences_tape() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-partial-batch-src");
    let source_path = source_dir.join("payload.bin");
    let payload = vec![0xA5; API_SESSION_BLOCK_SIZE as usize * 8];
    std::fs::write(&source_path, &payload).expect("write source payload");
    let mut tape_sink = PartialBatchSink::new(4, 2);
    let cfg = pool_config("scenario-a");

    let err = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-partial-batch".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect_err("partial batch must fail the write");

    assert!(
        err.to_string()
            .contains("partial fixed batch uncommittable"),
        "{err}"
    );
    assert_eq!(
        tape_sink.inner.filemarks,
        vec![1],
        "bootstrap filemark is allowed; object-closing filemark must not be written"
    );
    assert_no_pool_write_catalog_reference(&index, "caller-partial-batch", POOL_WRITE_TAPE_UUID);
    let fences = index
        .tape_io_admission_conflicts(&POOL_WRITE_TAPE_UUID, Some("RMN004L9"))
        .expect("active partial-batch fence");
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].reason, "partial_batch");
    assert!(
        fences[0]
            .evidence_json
            .as_deref()
            .is_some_and(|evidence| evidence.contains("partial fixed batch uncommittable")),
        "{fences:?}"
    );
}

#[test]
fn encrypted_no_parity_short_byte_batch_is_uncommittable_and_fences_tape() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-short-byte-encrypted-src");
    let source_path = source_dir.join("payload.bin");
    let payload = vec![0x5A; API_SESSION_BLOCK_SIZE as usize * 8];
    std::fs::write(&source_path, &payload).expect("write source payload");
    let mut tape_sink = ShortByteBatchSink::new(4);
    let cfg = pool_config("scenario-a");
    let (_private_key, recipients) = recipient_pair(0x52);

    let err = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-short-byte-encrypted".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Encrypted { recipients },
        },
    )
    .expect_err("short byte accounting must fail the write");

    let message = err.to_string();
    assert!(
        message.contains("partial fixed batch uncommittable"),
        "{err}"
    );
    assert!(message.contains("requested_bytes="), "{err}");
    assert!(message.contains("written_bytes="), "{err}");
    assert_eq!(
        tape_sink.inner.filemarks,
        vec![1],
        "bootstrap filemark is allowed; object-closing filemark must not be written"
    );
    assert_no_pool_write_catalog_reference(
        &index,
        "caller-short-byte-encrypted",
        POOL_WRITE_TAPE_UUID,
    );
    let fences = index
        .tape_io_admission_conflicts(&POOL_WRITE_TAPE_UUID, Some("RMN004L9"))
        .expect("active short-byte fence");
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].reason, "partial_batch");
    assert!(
        fences[0]
            .evidence_json
            .as_deref()
            .is_some_and(|evidence| evidence.contains("written_bytes=")),
        "{fences:?}"
    );
}

#[test]
fn position_drift_during_batched_write_fences_with_position_evidence() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-position-drift-src");
    let source_path = source_dir.join("payload.bin");
    let payload = vec![0x5A; API_SESSION_BLOCK_SIZE as usize * 4];
    std::fs::write(&source_path, &payload).expect("write source payload");
    let mut tape_sink = PositionDriftBatchSink::new(4);
    let cfg = pool_config("scenario-a");

    let err = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-position-drift".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect_err("position drift must fail the write");

    assert!(err.to_string().contains("position drift"), "{err}");
    assert_eq!(
        tape_sink.inner.filemarks,
        vec![1],
        "object-closing filemark must not be written after drift"
    );
    assert_no_pool_write_catalog_reference(&index, "caller-position-drift", POOL_WRITE_TAPE_UUID);
    let fences = index
        .tape_io_admission_conflicts(&POOL_WRITE_TAPE_UUID, Some("RMN004L9"))
        .expect("active drift fence");
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].reason, "position_drift");
    let evidence = fences[0].evidence_json.as_deref().expect("drift evidence");
    assert!(evidence.contains("expected_lba=11"), "{evidence}");
    assert!(evidence.contains("device_lba=12"), "{evidence}");
}

#[test]
fn encrypted_no_parity_write_round_trips_payload_and_commits_envelope_fields() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-encrypted-no-parity-src");
    let source_path = source_dir.join("payload.bin");
    let payload = b"encrypted no parity payload".to_vec();
    std::fs::write(&source_path, &payload).expect("write source payload");
    let mut tape_sink = VecBlockSink::new();
    let cfg = pool_config("scenario-a");
    let (primary_key, recipients) = recipient_pair(0x42);
    let recipient_epoch_ids = vec![
        "42424242424242424242424242424242".to_string(),
        "43434343434343434343434343434343".to_string(),
    ];

    let result = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path.clone()),
            archive_path: "./payload.bin".into(),
            caller_object_id: "caller-encrypted-no-parity".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Encrypted {
                recipients: recipients.clone(),
            },
        },
    )
    .expect("write encrypted no-parity object");

    assert_eq!(tape_sink.filemarks, vec![1, 1]);
    assert_eq!(
        result.object.copies[0].representation,
        OBJECT_COPY_REPRESENTATION_ENCRYPTED
    );
    assert_eq!(
        result.object.copies[0].recipient_epoch_ids,
        Some(recipient_epoch_ids.clone())
    );
    let metadata_frame_len = result.object.copies[0]
        .metadata_frame_len
        .expect("metadata frame length");
    let committed = index
        .get_native_object(result.object.object_id_text().as_str())
        .expect("query encrypted object")
        .expect("encrypted object exists");
    assert_eq!(committed.metadata_hash, None);
    assert_eq!(committed.copies.len(), 1);
    assert_eq!(
        committed.copies[0].representation,
        OBJECT_COPY_REPRESENTATION_ENCRYPTED
    );
    assert_eq!(
        committed.copies[0].recipient_epoch_ids.as_deref(),
        Some(recipient_epoch_ids.as_slice())
    );
    assert_eq!(
        committed.copies[0].metadata_frame_len,
        Some(metadata_frame_len)
    );

    let blocks_after_commit = tape_sink.blocks.len();
    let filemarks_after_commit = tape_sink.filemarks.clone();
    let exact_replay = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path.clone()),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-encrypted-no-parity".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Encrypted { recipients },
        },
    )
    .expect("exact encrypted recipient replay succeeds");
    assert!(exact_replay.is_replay());
    assert_eq!(exact_replay.object.copies.len(), 1);
    assert_eq!(
        exact_replay.object.copies[0].recipient_epoch_ids.as_deref(),
        Some(recipient_epoch_ids.as_slice())
    );

    let (_, changed_recipients) = recipient_pair(0x52);
    let changed_recipient_error = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path.clone()),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-encrypted-no-parity".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Encrypted {
                recipients: changed_recipients,
            },
        },
    )
    .expect_err("encrypted replay must preserve ordered recipient epochs");
    assert!(matches!(
        changed_recipient_error,
        PoolWriteError::CallerObjectIdRepresentationConflict { .. }
    ));

    let plaintext_error = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "./payload.bin".into(),
            caller_object_id: "caller-encrypted-no-parity".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect_err("encrypted replay must not return a plaintext request as committed");
    assert!(matches!(
        plaintext_error,
        PoolWriteError::CallerObjectIdRepresentationConflict { .. }
    ));
    assert_eq!(tape_sink.blocks.len(), blocks_after_commit);
    assert_eq!(tape_sink.filemarks, filemarks_after_commit);

    let stored_block_count =
        usize::try_from(result.expect_write_report().object_close.data_block_count)
            .expect("stored block count fits usize");
    let mut source = VecBlockSource::new(tape_sink.blocks[1..1 + stored_block_count].to_vec());
    let opened = read_encrypted_rem_object(
        &mut source,
        API_SESSION_BLOCK_SIZE as usize,
        result.expect_write_report().object_close.data_block_count,
        &primary_key,
    )
    .expect("decrypt encrypted REM-OBJECT object");
    let restored = opened.object.entry("payload.bin").expect("payload entry");
    assert_eq!(restored.data, payload);
    assert_eq!(opened.envelope.header.format_version, 2);
    assert_eq!(
        opened.envelope.header.metadata_frame_len,
        metadata_frame_len
    );
}

#[test]
fn pool_write_rejects_pool_tape_block_size_mismatch_before_tape_io() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-block-mismatch-src");
    let source_path = source_dir.join("payload.bin");
    std::fs::write(&source_path, b"block mismatch must not reach tape")
        .expect("write source payload");
    let cfg = pool_config_with_block_size("scenario-a", API_SESSION_BLOCK_SIZE * 2);
    let mut tape_sink = VecBlockSink::new();

    let err = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-block-mismatch".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect_err("pool/tape block-size mismatch must reject");

    match err {
        PoolWriteError::Select(SelectTapeError::NoWritableTapes { pool_id, reasons }) => {
            assert_eq!(pool_id, "scenario-a");
            assert!(
                reasons.iter().any(|reason| matches!(
                    reason,
                    WritabilityError::BlockSizeMismatch {
                        tape_block_size,
                        pool_block_size,
                    } if *tape_block_size == u64::from(API_SESSION_BLOCK_SIZE)
                        && *pool_block_size == u64::from(API_SESSION_BLOCK_SIZE * 2)
                )),
                "{reasons:?}"
            );
        }
        other => panic!("unexpected pool write error: {other}"),
    }
    assert!(tape_sink.blocks.is_empty());
    assert!(tape_sink.filemarks.is_empty());

    let selected_path = source_dir.join("selected-payload.bin");
    std::fs::write(&selected_path, b"selected mismatch must not reach tape")
        .expect("write selected source payload");
    let selected = SelectedTape {
        pool_id: "scenario-a".to_string(),
        tape_uuid: POOL_WRITE_TAPE_UUID,
        block_size: API_SESSION_BLOCK_SIZE,
        parity_config: ParityConfig::None,
    };
    let mut selected_sink = VecBlockSink::new();

    let err = write_to_selected_tape(
        &mut index,
        &mut selected_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(selected_path),
            archive_path: "selected-payload.bin".into(),
            caller_object_id: "caller-selected-block-mismatch".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected,
    )
    .expect_err("selected pool/tape block-size mismatch must reject");

    assert!(
        matches!(
            err,
            PoolWriteError::InvalidInput(ref message)
                if message.contains("does not match pool configured block size")
        ),
        "{err}"
    );
    assert!(selected_sink.blocks.is_empty());
    assert!(selected_sink.filemarks.is_empty());
}

#[test]
fn pool_write_uses_selected_tape_block_size_as_rem_object_chunk_size() {
    const CUSTOM_BLOCK_SIZE: u32 = 8192;

    let mut plaintext_index = test_index();
    project_pool(&mut plaintext_index, "custom-plain");
    project_no_parity_tape_with_block_size(
        &mut plaintext_index,
        "custom-plain",
        POOL_WRITE_TAPE_UUID,
        CUSTOM_BLOCK_SIZE,
    );
    let source_dir = temp_dir("remanence-api-custom-block-plain-src");
    let plaintext_path = source_dir.join("plain.bin");
    let plaintext_payload = b"plaintext custom block size payload".to_vec();
    std::fs::write(&plaintext_path, &plaintext_payload).expect("write plaintext source");
    let mut plaintext_sink = VecBlockSink::new();
    let plaintext_cfg = pool_config_with_block_size("custom-plain", CUSTOM_BLOCK_SIZE);

    let plaintext = write_object_to_pool(
        &mut plaintext_index,
        &mut plaintext_sink,
        &plaintext_cfg,
        WriteObjectToPoolRequest {
            pool_id: "custom-plain".to_string(),
            source: crate::WriteObjectSource::Path(plaintext_path),
            archive_path: "plain.bin".into(),
            caller_object_id: "caller-custom-block-plain".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("write plaintext custom-block object");

    assert_eq!(
        plaintext.expect_write_report().layout.chunk_size,
        CUSTOM_BLOCK_SIZE as usize
    );
    assert_eq!(
        parse_bootstrap_block(&plaintext_sink.blocks[0])
            .expect("parse plaintext bootstrap")
            .block_size_bytes,
        CUSTOM_BLOCK_SIZE
    );
    assert_eq!(
        plaintext_index
            .get_tape(&POOL_WRITE_TAPE_UUID)
            .expect("query plaintext custom-block tape")
            .expect("plaintext custom-block tape exists")
            .block_size,
        Some(u64::from(CUSTOM_BLOCK_SIZE))
    );
    let plaintext_block_count = usize::try_from(
        plaintext
            .expect_write_report()
            .object_close
            .data_block_count,
    )
    .expect("plaintext object block count fits usize");
    let mut plaintext_source =
        VecBlockSource::new(plaintext_sink.blocks[1..1 + plaintext_block_count].to_vec());
    let plaintext_read = read_rem_tar_object(
        &mut plaintext_source,
        CUSTOM_BLOCK_SIZE as usize,
        plaintext
            .expect_write_report()
            .object_close
            .data_block_count,
    )
    .expect("read plaintext custom-block REM-OBJECT object");
    assert_eq!(
        plaintext_read
            .global_pax
            .get("REMANENCE.chunk_size")
            .map(String::as_str),
        Some("8192")
    );
    assert_eq!(
        plaintext_read.entry("plain.bin").expect("plain entry").data,
        plaintext_payload
    );

    let mut encrypted_index = test_index();
    project_pool(&mut encrypted_index, "custom-encrypted");
    project_no_parity_tape_with_block_size(
        &mut encrypted_index,
        "custom-encrypted",
        SECOND_POOL_WRITE_TAPE_UUID,
        CUSTOM_BLOCK_SIZE,
    );
    let source_dir = temp_dir("remanence-api-custom-block-encrypted-src");
    let encrypted_path = source_dir.join("secret.bin");
    let encrypted_payload = b"encrypted custom block size payload".to_vec();
    std::fs::write(&encrypted_path, &encrypted_payload).expect("write encrypted source");
    let mut encrypted_sink = VecBlockSink::new();
    let encrypted_cfg = pool_config_with_block_size("custom-encrypted", CUSTOM_BLOCK_SIZE);
    let (primary_key, recipients) = recipient_pair(0x62);

    let encrypted = write_object_to_pool(
        &mut encrypted_index,
        &mut encrypted_sink,
        &encrypted_cfg,
        WriteObjectToPoolRequest {
            pool_id: "custom-encrypted".to_string(),
            source: crate::WriteObjectSource::Path(encrypted_path),
            archive_path: "secret.bin".into(),
            caller_object_id: "caller-custom-block-encrypted".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Encrypted { recipients },
        },
    )
    .expect("write encrypted custom-block object");

    assert_eq!(
        encrypted.expect_write_report().layout.chunk_size,
        CUSTOM_BLOCK_SIZE as usize
    );
    assert_eq!(
        parse_bootstrap_block(&encrypted_sink.blocks[0])
            .expect("parse encrypted bootstrap")
            .block_size_bytes,
        CUSTOM_BLOCK_SIZE
    );
    assert_eq!(
        encrypted_index
            .get_tape(&SECOND_POOL_WRITE_TAPE_UUID)
            .expect("query encrypted custom-block tape")
            .expect("encrypted custom-block tape exists")
            .block_size,
        Some(u64::from(CUSTOM_BLOCK_SIZE))
    );
    let encrypted_block_count = usize::try_from(
        encrypted
            .expect_write_report()
            .object_close
            .data_block_count,
    )
    .expect("encrypted object block count fits usize");
    let mut encrypted_source =
        VecBlockSource::new(encrypted_sink.blocks[1..1 + encrypted_block_count].to_vec());
    let opened = read_encrypted_rem_object(
        &mut encrypted_source,
        CUSTOM_BLOCK_SIZE as usize,
        encrypted
            .expect_write_report()
            .object_close
            .data_block_count,
        &primary_key,
    )
    .expect("decrypt encrypted custom-block REM-OBJECT object");
    assert_eq!(opened.envelope.header.chunk_size, CUSTOM_BLOCK_SIZE);
    assert_eq!(
        opened
            .object
            .global_pax
            .get("REMANENCE.chunk_size")
            .map(String::as_str),
        Some("8192")
    );
    assert_eq!(
        opened
            .object
            .entry("secret.bin")
            .expect("secret entry")
            .data,
        encrypted_payload
    );
}

#[test]
fn encrypted_write_transfer_failure_leaves_no_durable_catalog_reference() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-encrypted-transfer-fail-src");
    let source_path = source_dir.join("payload.bin");
    let payload = b"encrypted transfer failure payload".to_vec();
    std::fs::write(&source_path, &payload).expect("write source payload");
    let cfg = pool_config("scenario-a");
    let selected = select_tape_in_pool(&index, &cfg, payload.len() as u64, &HashSet::new())
        .expect("select no-parity tape");
    let (_primary_key, recipients) = recipient_pair(0x42);
    let mut tape_sink = FailAfterBlocksSink::new(1);

    let err = write_to_selected_tape(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-encrypted-transfer-fail".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Encrypted { recipients },
        },
        selected,
    )
    .expect_err("injected transfer error must fail the write");

    assert!(
        matches!(
            err,
            PoolWriteError::TapeIo(TapeIoError::OperationFailed(ref message))
                if message.contains("injected write_block failure")
        ),
        "{err}"
    );
    assert_eq!(
        tape_sink.inner.blocks.len(),
        1,
        "only the tape bootstrap should be written before the injected failure"
    );
    assert_eq!(
        tape_sink.inner.filemarks,
        vec![1],
        "failed transfer must not write an object-closing filemark"
    );
    assert_no_pool_write_catalog_reference(
        &index,
        "caller-encrypted-transfer-fail",
        POOL_WRITE_TAPE_UUID,
    );
}

#[test]
fn plaintext_write_transfer_failure_leaves_no_durable_catalog_reference() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-plaintext-transfer-fail-src");
    let source_path = source_dir.join("payload.bin");
    let payload = b"plaintext transfer failure payload".to_vec();
    std::fs::write(&source_path, &payload).expect("write source payload");
    let cfg = pool_config("scenario-a");
    let selected = select_tape_in_pool(&index, &cfg, payload.len() as u64, &HashSet::new())
        .expect("select no-parity tape");
    let mut tape_sink = FailAfterBlocksSink::new(1);

    let err = write_to_selected_tape(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-plaintext-transfer-fail".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected,
    )
    .expect_err("injected transfer error must fail the write");

    assert!(
        err.to_string().contains("injected write_block failure"),
        "{err}"
    );
    assert_eq!(
        tape_sink.inner.blocks.len(),
        1,
        "only the tape bootstrap should be written before the injected failure"
    );
    assert_eq!(
        tape_sink.inner.filemarks,
        vec![1],
        "failed transfer must not write an object-closing filemark"
    );
    assert_no_pool_write_catalog_reference(
        &index,
        "caller-plaintext-transfer-fail",
        POOL_WRITE_TAPE_UUID,
    );
}

/// §10.6 integration: the recycle-concern repro with **no** catalog
/// reset anywhere. Init → write+commit → retire → re-init the same
/// barcode under a fresh identity → write+commit; the first identity's
/// copies read back `missing` while the second's read back `committed`.
#[test]
fn retire_then_reinit_same_barcode_round_trips_without_catalog_reset() {
    const FIRST_UUID: [u8; 16] = [0xA1; 16];
    const SECOND_UUID: [u8; 16] = [0xB2; 16];
    const VOLTAG: &str = "RMN161L9";
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", FIRST_UUID);
    let source_dir = temp_dir("remanence-api-retire-cycle-src");
    let first_path = source_dir.join("first.bin");
    let second_path = source_dir.join("second.bin");
    std::fs::write(&first_path, b"first lifecycle payload").expect("write first payload");
    std::fs::write(&second_path, b"second lifecycle payload").expect("write second payload");
    let cfg = pool_config("scenario-a");

    // Write + commit an object to the first identity (in-memory tape).
    let mut first_sink = VecBlockSink::new();
    let first = write_object_to_pool(
        &mut index,
        &mut first_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(first_path),
            archive_path: "first.bin".into(),
            caller_object_id: "caller-retire-first".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("first write succeeds");
    assert_eq!(first.object.copies[0].tape_uuid, FIRST_UUID);

    // Retire the identity — catalog + audit only, no hardware.
    let outcome = index
        .retire_tape(remanence_state::RetireTapeInput {
            tape_uuid: FIRST_UUID,
            reason: "recycled".to_string(),
        })
        .expect("retire first identity");
    assert!(outcome.newly_retired);
    assert_eq!(outcome.released_voltag.as_deref(), Some(VOLTAG));
    assert_eq!(outcome.copies_marked_missing, 1);

    // Re-init the same physical medium: BOT still carries the retired
    // identity's bootstrap with object data past it.
    let mut bot_source = VecBlockSource::new(first_sink.blocks.clone());
    let projection = classify_bot_from_source(&mut bot_source);
    assert!(
        projection.physical_data_past_bootstrap,
        "fixture must reproduce the concern doc's data-past-bootstrap state"
    );
    let catalog_inputs =
        project_tape_init_catalog_inputs(&index, VOLTAG, &projection.classification, "scenario-a")
            .expect("project init inputs");
    assert_eq!(
        catalog_inputs.barcode_state,
        BarcodeLifecycleState::Available
    );
    assert_eq!(
        catalog_inputs
            .catalog_row
            .as_ref()
            .map(|row| row.disposition),
        Some(CatalogRowDisposition::Retired)
    );
    let decision = decide_tape_init(
        &projection.classification,
        catalog_inputs.catalog_row.as_ref(),
        &catalog_inputs.barcode_state,
        "scenario-a",
        projection.physical_data_past_bootstrap,
        &catalog_inputs.committed_copies,
    );
    assert_eq!(
        decision,
        InitDecision::FreshInit,
        "retired identity must re-init without CLOBBER or force"
    );

    // Fresh bootstrap + fresh catalog row for the same barcode.
    let mut reinit_sink = VecBlockSink::new();
    let action = maybe_write_tape_init_bootstrap(
        &mut reinit_sink,
        &decision,
        TapeInitWriteOptions::default(),
        SECOND_UUID,
        API_SESSION_BLOCK_SIZE,
        ParityConfig::None,
        "test",
    )
    .expect("write fresh bootstrap");
    assert_eq!(action, TapeInitWriteAction::WroteBootstrap);
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: SECOND_UUID,
            voltag: VOLTAG.to_string(),
            block_size: API_SESSION_BLOCK_SIZE,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision fresh identity for the released barcode");
    index
        .project_tape_pool_membership(SECOND_UUID, "scenario-a")
        .expect("assign fresh identity to pool");

    // Write + commit a second object: selection must pick the fresh
    // identity (the retired one is not `ready`).
    let mut second_sink = VecBlockSink::new();
    let second = write_object_to_pool(
        &mut index,
        &mut second_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(second_path),
            archive_path: "second.bin".into(),
            caller_object_id: "caller-retire-second".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("second write succeeds without any catalog reset");
    assert_eq!(second.object.copies[0].tape_uuid, SECOND_UUID);

    let first_record = index
        .get_native_object(first.object.object_id_text().as_str())
        .expect("query first object")
        .expect("first object exists");
    assert_eq!(first_record.copies.len(), 1);
    assert_eq!(first_record.copies[0].status, "missing");
    let second_record = index
        .get_native_object(second.object.object_id_text().as_str())
        .expect("query second object")
        .expect("second object exists");
    assert_eq!(second_record.copies.len(), 1);
    assert_eq!(second_record.copies[0].status, "committed");
    assert_eq!(
        index
            .list_objects_with_no_committed_copies()
            .expect("degraded objects"),
        vec![first.object.object_id_text()]
    );
}

#[test]
fn no_parity_pool_write_appends_second_object_to_same_tape() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-no-parity-reuse-src");
    let first_path = source_dir.join("first.bin");
    let second_path = source_dir.join("second.bin");
    let first_payload = b"first no parity payload".to_vec();
    let second_payload = b"second no parity payload".to_vec();
    std::fs::write(&first_path, &first_payload).expect("write first payload");
    std::fs::write(&second_path, &second_payload).expect("write second payload");
    let cfg = pool_config("scenario-a");
    let mut tape_sink = VecBlockSink::new();

    let first = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(first_path),
            archive_path: "first.bin".into(),
            caller_object_id: "caller-no-parity-first".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("first no-parity write succeeds");
    assert_eq!(first.object.copies[0].tape_uuid, POOL_WRITE_TAPE_UUID);
    assert_eq!(first.object.copies[0].tape_file_number, 1);
    let eod_after_first = tape_sink.eod_lba();
    tape_sink.set_next_lba_for_test(0);

    let second = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(second_path),
            archive_path: "second.bin".into(),
            caller_object_id: "caller-no-parity-second".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("second no-parity append succeeds");

    assert_eq!(second.object.copies[0].tape_uuid, POOL_WRITE_TAPE_UUID);
    assert_eq!(second.object.copies[0].tape_file_number, 2);
    assert_eq!(
        second.expect_write_report().object_close.tape_file_number,
        2,
        "append report must carry the real tape file number"
    );
    assert_eq!(
        tape_sink.filemarks,
        vec![1, 1, 1],
        "append must not write a second file-0 bootstrap"
    );
    assert_eq!(
        tape_sink.space_to_eod_calls, 1,
        "append from a BOT-positioned session must space to EOD before writing"
    );

    let tape_files = index
        .list_tape_files(&POOL_WRITE_TAPE_UUID)
        .expect("list committed tape files");
    assert_eq!(tape_files.len(), 3);
    assert_eq!(tape_files[0].tape_file_number, 0);
    assert_eq!(tape_files[0].kind, "bootstrap");
    assert_eq!(tape_files[1].tape_file_number, 1);
    assert_eq!(tape_files[1].kind, "object");
    assert_eq!(tape_files[2].tape_file_number, 2);
    assert_eq!(tape_files[2].kind, "object");

    let tape = index
        .get_tape(&POOL_WRITE_TAPE_UUID)
        .expect("query tape")
        .expect("tape row");
    assert_eq!(tape.last_committed_tape_file, Some(2));
    assert_eq!(
        tape.total_committed_ordinals,
        first.expect_write_report().object_close.data_block_count
            + second.expect_write_report().object_close.data_block_count
    );

    let first_blocks = usize::try_from(first.expect_write_report().object_close.data_block_count)
        .expect("first block count fits usize");
    let second_blocks = usize::try_from(second.expect_write_report().object_close.data_block_count)
        .expect("second block count fits usize");
    let second_start = 1 + first_blocks;
    assert_eq!(
        tape_sink.block_lbas[second_start], eod_after_first,
        "second object must be written at the captured EOD, not over BOT"
    );
    let mut second_source =
        VecBlockSource::new(tape_sink.blocks[second_start..second_start + second_blocks].to_vec());
    let read = read_rem_tar_object(
        &mut second_source,
        API_SESSION_BLOCK_SIZE as usize,
        second.expect_write_report().layout.projected_size_blocks,
    )
    .expect("read appended no-parity REM-OBJECT object");
    assert_eq!(
        read.entry("second.bin").expect("second entry").data,
        second_payload
    );
}

#[test]
fn pool_write_replays_same_pool_caller_object_id_without_tape_io() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-caller-replay-src");
    let source_path = source_dir.join("payload.bin");
    let payload = b"same caller id replay payload".to_vec();
    std::fs::write(&source_path, &payload).expect("write source payload");
    let cfg = pool_config("scenario-a");
    let mut tape_sink = VecBlockSink::new();

    let first = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path.clone()),
            archive_path: "./payload.bin".into(),
            caller_object_id: "caller-replay".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("first write succeeds");
    assert!(!first.is_replay());
    let blocks_after_first = tape_sink.blocks.len();
    let filemarks_after_first = tape_sink.filemarks.clone();

    let replay = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path.clone()),
            archive_path: "./payload.bin".into(),
            caller_object_id: "caller-replay".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("same caller/content replay succeeds");

    assert!(replay.is_replay());
    assert!(replay.write_report().is_none());
    assert_eq!(replay.object, first.object);
    assert_eq!(tape_sink.blocks.len(), blocks_after_first);
    assert_eq!(tape_sink.filemarks, filemarks_after_first);
    assert_eq!(
        index
            .list_native_objects()
            .expect("list native objects")
            .len(),
        1
    );
    assert_eq!(
        index
            .list_tape_files(&POOL_WRITE_TAPE_UUID)
            .expect("list tape files")
            .len(),
        2,
        "replay must not append another object tape file"
    );

    let selected = select_tape_in_pool(&index, &cfg, payload.len() as u64, &HashSet::new())
        .expect("select tape before drive-bound replay recheck");
    let mut drive_bound_sink = VecBlockSink::new();
    let drive_bound_replay = write_to_selected_tape(
        &mut index,
        &mut drive_bound_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path.clone()),
            archive_path: "./payload.bin".into(),
            caller_object_id: "caller-replay".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected.clone(),
    )
    .expect("drive-bound exact replay normalizes the accepted member path");
    assert!(drive_bound_replay.is_replay());

    let wrong_path = write_to_selected_tape(
        &mut index,
        &mut drive_bound_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path.clone()),
            archive_path: "renamed.bin".into(),
            caller_object_id: "caller-replay".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected.clone(),
    )
    .expect_err("drive-bound replay must reject a changed member path");
    assert!(matches!(
        wrong_path,
        PoolWriteError::CallerObjectIdArchivePathConflict { .. }
    ));

    let (_, recipients) = recipient_pair(0x62);
    let wrong_representation = write_to_selected_tape(
        &mut index,
        &mut drive_bound_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "./payload.bin".into(),
            caller_object_id: "caller-replay".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Encrypted { recipients },
        },
        selected,
    )
    .expect_err("drive-bound replay must reject a changed representation");
    assert!(matches!(
        wrong_representation,
        PoolWriteError::CallerObjectIdRepresentationConflict { .. }
    ));
    assert!(drive_bound_sink.blocks.is_empty());
    assert!(drive_bound_sink.filemarks.is_empty());
}

#[test]
fn canonical_uuid_collision_is_rejected_before_tape_motion() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-canonical-uuid-collision");
    let source_path = source_dir.join("payload.bin");
    std::fs::write(&source_path, b"already committed logical source")
        .expect("write source payload");
    let cfg = pool_config("scenario-a");
    let mut tape_sink = VecBlockSink::new();
    let first = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path.clone()),
            archive_path: "payload.bin".into(),
            caller_object_id: "original-caller".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("seed existing object UUID");
    let blocks_before = tape_sink.blocks.len();
    let filemarks_before = tape_sink.filemarks.clone();

    let collision = WriteObjectToPoolRequest {
        pool_id: "scenario-a".to_string(),
        source: crate::WriteObjectSource::Path(source_path),
        archive_path: PathBuf::new(),
        caller_object_id: "different-caller".to_string(),
        expected_content_sha256: None,
        expected_object_id: Some(first.object.object_id),
        input_kind: crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let error = crate::pool_write::maybe_replay_pool_write(&index, &cfg, &collision)
        .expect_err("global object UUID collision must fail before append");
    assert!(
        error.to_string().contains("already exists outside"),
        "{error}"
    );
    assert_eq!(tape_sink.blocks.len(), blocks_before);
    assert_eq!(tape_sink.filemarks, filemarks_before);
}

#[test]
fn committed_replay_rejects_input_kind_change_before_tape_motion() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-input-kind-replay");
    let source_path = source_dir.join("payload.bin");
    std::fs::write(&source_path, b"same bytes, different ingestion semantics")
        .expect("write source payload");
    let cfg = pool_config("scenario-a");
    let mut tape_sink = VecBlockSink::new();
    let first = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path.clone()),
            archive_path: "payload.bin".into(),
            caller_object_id: "input-kind-replay".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("seed logical-file object");
    let blocks_before = tape_sink.blocks.len();
    let filemarks_before = tape_sink.filemarks.clone();

    let error = crate::pool_write::maybe_replay_pool_write(
        &index,
        &cfg,
        &WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: PathBuf::new(),
            caller_object_id: "input-kind-replay".to_string(),
            expected_content_sha256: None,
            expected_object_id: Some(first.object.object_id),
            input_kind: crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect_err("committed replay must preserve the original input kind");
    assert!(matches!(
        error,
        PoolWriteError::CallerObjectIdInputKindConflict { .. }
    ));
    assert_eq!(tape_sink.blocks.len(), blocks_before);
    assert_eq!(tape_sink.filemarks, filemarks_before);
}

#[test]
fn pool_write_conflicts_same_pool_caller_object_id_with_different_content() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-caller-conflict-src");
    let first_path = source_dir.join("first.bin");
    let second_path = source_dir.join("second.bin");
    let first_payload = b"first caller id payload".to_vec();
    let second_payload = b"second caller id payload".to_vec();
    std::fs::write(&first_path, &first_payload).expect("write first payload");
    std::fs::write(&second_path, &second_payload).expect("write second payload");
    let cfg = pool_config("scenario-a");
    let mut tape_sink = VecBlockSink::new();

    let first = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(first_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-conflict".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("first write succeeds");
    let blocks_after_first = tape_sink.blocks.len();
    let filemarks_after_first = tape_sink.filemarks.clone();

    let err = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(second_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-conflict".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect_err("same caller with different content must conflict");

    match err {
        PoolWriteError::CallerObjectIdConflict {
            pool_id,
            caller_object_id,
            existing_content_sha256,
            requested_content_sha256,
        } => {
            assert_eq!(pool_id, "scenario-a");
            assert_eq!(caller_object_id, "caller-conflict");
            assert_eq!(
                existing_content_sha256,
                bytes_to_hex(&sha256_bytes(&first_payload))
            );
            assert_eq!(
                requested_content_sha256,
                bytes_to_hex(&sha256_bytes(&second_payload))
            );
        }
        other => panic!("unexpected pool write error: {other}"),
    }
    assert_eq!(first.object.caller_object_id, "caller-conflict");
    assert_eq!(tape_sink.blocks.len(), blocks_after_first);
    assert_eq!(tape_sink.filemarks, filemarks_after_first);
    assert_eq!(
        index
            .list_native_objects()
            .expect("list native objects")
            .len(),
        1
    );
}

#[test]
fn streamed_caller_id_digest_conflict_rejects_before_any_tape_motion() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-streamed-caller-conflict");
    let committed_path = source_dir.join("committed.bin");
    let committed_payload = b"committed overlap identity".to_vec();
    std::fs::write(&committed_path, &committed_payload).expect("write committed payload");
    let cfg = pool_config("scenario-a");
    let mut tape_sink = VecBlockSink::new();
    write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(committed_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "streamed-conflict".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("seed committed object");
    let blocks_before_conflict = tape_sink.blocks.len();
    let conflicting_digest: [u8; 32] = sha256_bytes(b"different streamed bytes")
        .try_into()
        .expect("SHA-256 length");
    let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("manager");
    let (_producer, consumer, control) =
        crate::append_ring::create_append_ring(&manager, ring_bytes, 90, 25, ring_bytes)
            .expect("ring");
    let request = WriteObjectToPoolRequest {
        pool_id: "scenario-a".to_string(),
        source: crate::WriteObjectSource::Streamed(crate::StreamedWriteSource::new(
            consumer,
            ring_bytes,
            conflicting_digest,
            control,
        )),
        archive_path: "payload.bin".into(),
        caller_object_id: "streamed-conflict".to_string(),
        expected_content_sha256: Some(conflicting_digest),
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let error = crate::pool_write::maybe_replay_pool_write(&index, &cfg, &request)
        .expect_err("different streamed digest must conflict");
    assert!(matches!(
        error,
        PoolWriteError::CallerObjectIdConflict { .. }
    ));
    assert_eq!(
        tape_sink.blocks.len(),
        blocks_before_conflict,
        "identity conflict must not issue another tape write"
    );
}

#[test]
fn pool_write_rejects_empty_caller_object_id_without_tape_io() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-empty-caller-src");
    let source_path = source_dir.join("payload.bin");
    std::fs::write(&source_path, b"empty caller may duplicate").expect("write payload");
    let cfg = pool_config("scenario-a");
    let mut tape_sink = VecBlockSink::new();

    let error = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path.clone()),
            archive_path: "payload.bin".into(),
            caller_object_id: String::new(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect_err("empty caller_object_id must be rejected");

    assert_eq!(
        error.to_string(),
        "invalid REM-OBJECT input: caller_object_id must not be empty"
    );
    assert!(tape_sink.blocks.is_empty());
    assert!(tape_sink.filemarks.is_empty());
    assert!(index
        .list_native_objects()
        .expect("list native objects")
        .is_empty());
}

#[test]
fn write_to_selected_tape_rejects_second_parity_object_before_tape_io() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_eligible_tape_with_block_size(
        &mut index,
        "scenario-a",
        POOL_WRITE_TAPE_UUID,
        BLOCK_SIZE,
    );
    let source_dir = temp_dir("remanence-api-parity-reuse-src");
    let first_path = source_dir.join("first.bin");
    let second_path = source_dir.join("second.bin");
    std::fs::write(&first_path, b"first parity payload").expect("write first payload");
    std::fs::write(&second_path, b"second parity payload").expect("write second payload");
    let cfg = pool_config_with_block_size("scenario-a", BLOCK_SIZE);
    let selected =
        select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("select parity tape");
    let mut first_sink = VecBlockSink::new();

    let first = write_to_selected_tape(
        &mut index,
        &mut first_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(first_path),
            archive_path: "first.bin".into(),
            caller_object_id: "caller-parity-first".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected.clone(),
    )
    .expect("first parity write succeeds");
    let mut second_sink = VecBlockSink::new();

    let err = write_to_selected_tape(
        &mut index,
        &mut second_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(second_path),
            archive_path: "second.bin".into(),
            caller_object_id: "caller-parity-second".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected,
    )
    .expect_err("second parity write must reject before opening at BOT");

    assert!(
        matches!(
            err,
            PoolWriteError::ParityAppendUnsupported {
                ref tape_uuid,
                total_committed_ordinals
            } if tape_uuid == &Uuid::from_bytes(POOL_WRITE_TAPE_UUID).to_string()
                && total_committed_ordinals
                    == first.expect_write_report().catalog.tape_file_bundle.total_committed_ordinals
        ),
        "{err}"
    );
    assert!(second_sink.blocks.is_empty());
    assert!(second_sink.filemarks.is_empty());
}

#[test]
fn write_rejects_expected_content_sha256_mismatch_before_writing() {
    let mut index = test_index();
    project_pool(&mut index, "scenario-a");
    project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
    let source_dir = temp_dir("remanence-api-hash-mismatch-src");
    let source_path = source_dir.join("payload.bin");
    let payload = b"hash mismatch must stop before tape I/O".to_vec();
    std::fs::write(&source_path, &payload).expect("write source payload");
    let cfg = pool_config("scenario-a");
    let selected = select_tape_in_pool(&index, &cfg, payload.len() as u64, &HashSet::new())
        .expect("select tape");
    let mut tape_sink = VecBlockSink::new();
    let mut wrong_hash = [0u8; 32];
    wrong_hash[0] = 1;

    let err = write_to_selected_tape(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "scenario-a".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-hash-mismatch".to_string(),
            expected_content_sha256: Some(wrong_hash),
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected,
    )
    .expect_err("hash mismatch must reject before tape write");

    assert!(
        matches!(err, PoolWriteError::ContentHashMismatch { .. }),
        "{err}"
    );
    assert!(tape_sink.blocks.is_empty());
    assert!(tape_sink.filemarks.is_empty());
}

#[test]
fn write_object_to_pool_seals_after_crossing_low_and_excludes_tape() {
    let mut index = test_index();
    project_pool(&mut index, "seal.pool");
    project_no_parity_tape(&mut index, "seal.pool", POOL_WRITE_TAPE_UUID);
    project_no_parity_tape(&mut index, "seal.pool", SECOND_POOL_WRITE_TAPE_UUID);
    let cfg = pool_config_with_watermarks("seal.pool", 0.00000000001, 0.000000001, 0);
    let source_dir = temp_dir("remanence-api-seal-src");
    let source_path = source_dir.join("payload.bin");
    std::fs::write(&source_path, b"seal after actual position crosses low")
        .expect("write source payload");
    let mut tape_sink = VecBlockSink::new();

    let result = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "seal.pool".to_string(),
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-seal".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect("write and seal first tape");

    assert_eq!(result.object.copies[0].tape_uuid, POOL_WRITE_TAPE_UUID);
    let sealed = index
        .get_tape(&POOL_WRITE_TAPE_UUID)
        .expect("query sealed tape")
        .expect("sealed tape exists");
    assert_eq!(sealed.state, "sealed");

    let selected = select_tape_in_pool(&index, &cfg, 1, &HashSet::new())
        .expect("select unsealed tape after seal");
    assert_eq!(selected.tape_uuid, SECOND_POOL_WRITE_TAPE_UUID);
}

#[test]
fn verify_tape_identity_accepts_match_and_rejects_mismatch_or_absent_bootstrap() {
    let mut matching = VecBlockSource::new(vec![no_parity_bootstrap_block(POOL_WRITE_TAPE_UUID)]);
    verify_tape_identity(&mut matching, &POOL_WRITE_TAPE_UUID).expect("matching identity");

    let mut mismatched = VecBlockSource::new(vec![no_parity_bootstrap_block(POOL_WRITE_TAPE_UUID)]);
    let err = verify_tape_identity(&mut mismatched, &SECOND_POOL_WRITE_TAPE_UUID)
        .expect_err("mismatched identity");
    assert!(matches!(err, TapeIdentityError::Mismatch { .. }), "{err}");

    let mut absent = VecBlockSource::new(vec![vec![0u8; API_SESSION_BLOCK_SIZE as usize]]);
    let err =
        verify_tape_identity(&mut absent, &POOL_WRITE_TAPE_UUID).expect_err("absent bootstrap");
    assert!(
        matches!(err, TapeIdentityError::AbsentBootstrap(_)),
        "{err}"
    );
}

#[test]
fn write_object_to_pool_rejects_non_regular_source_as_invalid_argument() {
    let mut index = test_index();
    project_pool(&mut index, "camera.copy-a");
    project_no_parity_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
    let mut tape_sink = VecBlockSink::new();
    let source_dir = temp_dir("remanence-api-pool-write-dir-src");
    let cfg = pool_config("camera.copy-a");

    let err = write_object_to_pool(
        &mut index,
        &mut tape_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: "camera.copy-a".to_string(),
            source: crate::WriteObjectSource::Path(source_dir),
            archive_path: "payload.bin".into(),
            caller_object_id: "caller-non-regular".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
    )
    .expect_err("directory source must be caller-fault input");

    assert!(
        matches!(&err, PoolWriteError::InvalidInput(message) if message.contains("not a regular file")),
        "{err}"
    );
    assert!(tape_sink.blocks.is_empty());
    assert!(tape_sink.filemarks.is_empty());
    assert_no_pool_write_catalog_reference(&index, "caller-non-regular", POOL_WRITE_TAPE_UUID);
}

#[tokio::test]
async fn daemon_health_and_version_are_wired() {
    let service = populated_state().daemon_service();

    let health = pb::daemon_server::Daemon::health(&service, Request::new(()))
        .await
        .expect("health")
        .into_inner();
    assert_eq!(health.status, pb::health_response::Status::Healthy as i32);
    assert_eq!(
        health.components.get("sqlite_index").map(String::as_str),
        Some("ok")
    );
    assert_eq!(health.component_health.len(), 1);
    assert_eq!(health.component_health[0].component, "sqlite_index");
    assert_eq!(
        health.component_health[0].status,
        pb::component_health::Status::Healthy as i32
    );
    assert!(health.component_health[0].other_status.is_empty());

    let version = pb::daemon_server::Daemon::version(&service, Request::new(()))
        .await
        .expect("version")
        .into_inner();
    assert_eq!(version.api_version, "v1-draft");
    assert!(!version.daemon_version.is_empty());
    assert!(!version.rust_target.is_empty());
}

fn drive_target_test_state(
    loaded: bool,
    loaded_tape: Option<&str>,
    busy: bool,
) -> (
    ApiState,
    tokio::sync::mpsc::Receiver<crate::write_owner::DriveCommand>,
) {
    let mut state = populated_state();
    let serial = "LIB-DRIVE-TARGET";
    let mut library = test_library(serial);
    library.drive_bays[0].loaded = loaded;
    library.drive_bays[0].loaded_tape = loaded_tape.map(str::to_string);
    library.drive_bays[0].source_slot = loaded.then_some(0x03e9);
    state.default_library_serial = Some(Arc::new(serial.to_string()));
    state.library_snapshot = Some(Arc::new(RwLock::new(Arc::new(LibrarySnapshot {
        report: DiscoveryReport {
            libraries: vec![library],
            warnings: Vec::new(),
        },
        captured_at: OffsetDateTime::UNIX_EPOCH,
    }))));
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let (drive_tx, drive_rx) = tokio::sync::mpsc::channel(1);
    state.drive_pool = Some(crate::write_owner::DrivePool::new_for_library(
        serial,
        changer_tx,
        HashMap::from([(1, drive_tx)]),
        Arc::new(HashMap::from([(1, AtomicBool::new(busy))])),
    ));
    (state, drive_rx)
}

fn drive_target_request(required_pool_id: &str) -> pb::OpenReadSessionRequest {
    pb::OpenReadSessionRequest {
        target: Some(pb::open_read_session_request::Target::DriveTarget(
            pb::DriveTarget {
                library_uuid: crate::library::library_uuid("LIB-DRIVE-TARGET").to_vec(),
                drive_element_address: 1,
                required_pool_id: required_pool_id.to_string(),
            },
        )),
        idempotency_key: None,
        resume_target: None,
    }
}

#[tokio::test]
async fn drive_target_read_open_uses_loaded_drive_and_reports_it() {
    let (state, mut drive_rx) = drive_target_test_state(true, Some("ACM003L9"), false);
    let session_id = Uuid::new_v4();
    let actor = tokio::spawn(async move {
        match drive_rx.recv().await.expect("read-open command") {
            crate::write_owner::DriveCommand::OpenRead {
                tape_uuid,
                needs_drive_load,
                source_slot,
                reply,
                ..
            } => {
                assert_eq!(tape_uuid, TAPE_UUID);
                assert!(!needs_drive_load);
                assert_eq!(source_slot, None);
                reply
                    .send(Ok(pb::ReadSession {
                        session_id: session_id.as_bytes().to_vec(),
                        tape_uuid: Some(TAPE_UUID.to_vec()),
                        drive_element_address: Some(1),
                        state: pb::read_session::State::ReadSessionStateOpen as i32,
                        opened_at: Some(prost_types::Timestamp {
                            seconds: 0,
                            nanos: 0,
                        }),
                        position_proof: None,
                        daemon_epoch: 1,
                    }))
                    .expect("open reply receiver");
            }
            _ => panic!("drive-target open must use the common OpenRead actor command"),
        }
    });

    let opened = pb::read_session_service_server::ReadSessionService::open_read_session(
        &state.read_session_service(),
        Request::new(drive_target_request("camera.copy-a")),
    )
    .await
    .expect("open read session on loaded drive")
    .into_inner();

    assert_eq!(opened.tape_uuid.as_deref(), Some(TAPE_UUID.as_slice()));
    assert_eq!(opened.drive_element_address, Some(1));
    actor.await.expect("mock drive actor joins");
}

#[tokio::test]
async fn drive_target_read_open_uses_virtual_world_actor_and_proves_bot_identity() {
    const LIBRARY_SERIAL: &str = "LIB-DRIVE-TARGET";
    const DRIVE_SERIAL: &str = "DRV-DRIVE-TARGET";
    const BAY: u16 = 1;
    const SLOT: u16 = 0x03e9;

    let temp = tempfile::Builder::new()
        .prefix("remanence-api-drive-target-actor")
        .tempdir()
        .expect("temp dir");
    let mut index =
        CatalogIndex::open(temp.path().join("state.sqlite")).expect("open test catalog");
    project_pool(&mut index, "camera.copy-a");
    project_no_parity_tape(&mut index, "camera.copy-a", TAPE_UUID);

    let barcode = format!("RMN{:03}L9", TAPE_UUID[0]);
    let bootstrap = no_parity_bootstrap_block(TAPE_UUID);
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, API_SESSION_BLOCK_SIZE);
    tape.written_bytes = bootstrap.len() as u64;
    tape.records.push(VirtualRecord::Block(bootstrap));
    let mut virtual_world = VirtualWorld::single_drive(LIBRARY_SERIAL, BAY, DRIVE_SERIAL, SLOT, 1);
    virtual_world.put_tape_in_drive(BAY, barcode, None, tape);
    let world = Arc::new(Mutex::new(virtual_world));

    let discovered_library = world.lock().expect("virtual world lock").library_snapshot();
    let report = DiscoveryReport {
        libraries: vec![discovered_library.clone()],
        warnings: Vec::new(),
    };
    let policy = remanence_library::StaticAllowlist::new([LIBRARY_SERIAL]);
    let actor_world = Arc::clone(&world);
    let mut library = discovered_library
        .open_with(&policy, move |path| {
            let role = actor_world
                .lock()
                .expect("virtual world lock")
                .role_for_path(path)
                .expect("known virtual device path");
            Ok::<_, remanence_library::IoErrorKind>(Box::new(ModelTransport::new(
                Arc::clone(&actor_world),
                role,
            ))
                as Box<dyn remanence_library::SgTransport>)
        })
        .expect("open virtual library");
    let drive = library
        .open_drive(BAY, &policy)
        .expect("open virtual drive");
    let library_snapshot = Arc::new(RwLock::new(Arc::new(LibrarySnapshot {
        report: report.clone(),
        captured_at: OffsetDateTime::UNIX_EPOCH,
    })));
    let reservations = Arc::new(HashMap::from([(
        crate::drive_pool::DriveKey::new(LIBRARY_SERIAL, BAY),
        AtomicBool::new(false),
    )]));

    let mut state = ApiState::new(index);
    let owner_config = crate::write_owner::WriteOwnerConfig {
        index_path: state.index_path.as_ref().clone(),
        report,
        policy,
        audit_dir: state.audit_dir.as_ref().clone(),
        audit_fsync: false,
        audit_append_lock: Arc::clone(&state.audit_append_lock),
        reservations: Arc::clone(&reservations),
        actor_library_serial: LIBRARY_SERIAL.to_string(),
        library_snapshot: Arc::clone(&library_snapshot),
        snapshot_miss_alarm: 1,
        managed_library_serials: Arc::new(HashSet::from([LIBRARY_SERIAL.to_string()])),
        cleaning: remanence_state::CleaningConfig::default(),
        tape_io: remanence_state::TapeIoConfig::default(),
        io_memory: crate::io_memory::IoMemoryReservation::new(
            remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES,
        )
        .expect("test I/O memory manager"),
        write_admissions: crate::write_owner::WriteAdmissionCoordinator::default(),
        checkpoint_journal_dir: temp.path().join("checkpoints"),
        checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
        checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
        checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
        session_idle_seconds: 1800,
        lifecycle: None,
        calibration_store: state.calibration_store().clone(),
    };
    let drive_tx = crate::write_owner::spawn_drive_actor(BAY, drive, owner_config.clone());
    let changer_tx = crate::write_owner::spawn_changer_actor(library.into_changer(), owner_config);
    state.drive_pool = Some(crate::write_owner::DrivePool::new_with_lifecycle(
        HashMap::from([(LIBRARY_SERIAL.to_string(), changer_tx)]),
        HashMap::from([(
            crate::drive_pool::DriveKey::new(LIBRARY_SERIAL, BAY),
            drive_tx,
        )]),
        reservations,
        crate::drive_pool::DrivePoolLifecycle::default(),
    ));
    state.default_library_serial = Some(Arc::new(LIBRARY_SERIAL.to_string()));
    state.library_snapshot = Some(library_snapshot);

    let opened = pb::read_session_service_server::ReadSessionService::open_read_session(
        &state.read_session_service(),
        Request::new(drive_target_request("camera.copy-a")),
    )
    .await
    .expect("open read session through real drive actor")
    .into_inner();

    assert_eq!(opened.tape_uuid.as_deref(), Some(TAPE_UUID.as_slice()));
    assert_eq!(opened.drive_element_address, Some(u32::from(BAY)));
    let drive_opcodes = world
        .lock()
        .expect("virtual world lock")
        .command_log
        .iter()
        .filter_map(|command| {
            matches!(command.role, DeviceRole::Drive { bay: BAY }).then_some(command.opcode)
        })
        .collect::<Vec<_>>();
    assert!(
        drive_opcodes.contains(&0x00),
        "actor must prove media readiness with TEST UNIT READY: {drive_opcodes:?}"
    );
    assert!(
        drive_opcodes.contains(&0x01) && drive_opcodes.contains(&0x08),
        "actor must rewind and read the BOT bootstrap identity: {drive_opcodes:?}"
    );

    let closed = pb::read_session_service_server::ReadSessionService::close_read_session(
        &state.read_session_service(),
        Request::new(pb::CloseReadSessionRequest {
            session_id: opened.session_id,
            idempotency_key: None,
        }),
    )
    .await
    .expect("close real-actor read session")
    .into_inner();
    assert_eq!(
        closed.state,
        pb::read_session::State::ReadSessionStateClosed as i32
    );
}

#[test]
fn drive_target_read_errors_are_precise_and_pool_guard_is_enforced() {
    let (empty, _rx) = drive_target_test_state(false, None, false);
    let error = select_read_target(&empty, drive_target_request("").target)
        .expect_err("empty drive must fail");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(error.message(), "drive bay 0x0001 is empty");

    let (busy, _rx) = drive_target_test_state(true, Some("ACM003L9"), true);
    let error = select_read_target(&busy, drive_target_request("").target)
        .expect_err("busy drive must fail");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(error.message(), "drive bay 0x0001 is busy");

    let (unproven, _rx) = drive_target_test_state(true, None, false);
    let error = select_read_target(&unproven, drive_target_request("").target)
        .expect_err("unproven identity must fail");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert!(error.message().contains("identity cannot be proven"));

    let (wrong_pool, _rx) = drive_target_test_state(true, Some("ACM003L9"), false);
    let error = select_read_target(&wrong_pool, drive_target_request("camera.copy-b").target)
        .expect_err("pool guard mismatch must fail");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(error.message(), "tape is not assigned to the required pool");

    let (mut foreign, _rx) = drive_target_test_state(true, Some("ACM003L9"), false);
    let mut second_library = test_library("LIB-D2");
    second_library.drive_bays[0].loaded = true;
    second_library.drive_bays[0].loaded_tape = Some("ACM003L9".to_string());
    let current = foreign
        .current_library_snapshot()
        .expect("library snapshot");
    foreign.library_snapshot = Some(Arc::new(RwLock::new(Arc::new(LibrarySnapshot {
        report: DiscoveryReport {
            libraries: vec![current.report.libraries[0].clone(), second_library],
            warnings: Vec::new(),
        },
        captured_at: current.captured_at,
    }))));
    let mut request = drive_target_request("");
    let Some(pb::open_read_session_request::Target::DriveTarget(target)) = request.target.as_mut()
    else {
        panic!("drive target request")
    };
    target.library_uuid = crate::library::library_uuid("LIB-D2").to_vec();
    let error = select_read_target(&foreign, request.target)
        .expect_err("a foreign discovered library must not alias the operated drive pool");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error.message(),
        "library LIB-D2 is discovered but is not operated by this daemon"
    );
}

fn audit_test_record(
    sequence: u64,
    timestamp_utc: &str,
    session_id: Option<Uuid>,
    operation_id: Option<Uuid>,
    event: AuditEvent,
) -> AuditRecord {
    AuditRecord {
        schema_version: 1,
        record_uuid: Uuid::new_v4(),
        sequence,
        timestamp_utc: timestamp_utc.to_string(),
        host_id: "test-host".to_string(),
        process_id: 1,
        software_build: Some("test-build".to_string()),
        actor: AuditActor::System,
        source_layer: SourceLayer::Layer5,
        operation_id,
        session_id,
        idempotency_key: None,
        event,
        subject: AuditSubject {
            kind: "session".to_string(),
            id: session_id.map(|value| value.to_string()),
        },
        detail: BTreeMap::new(),
    }
}

#[test]
fn audit_query_window_and_filters_select_exact_entries() {
    let session_id = Uuid::new_v4();
    let operation_id = Uuid::new_v4();
    let query = AuditQuery::try_from(pb::QueryAuditRequest {
        since: timestamp_from_rfc3339("2026-07-18T10:00:00Z"),
        until: timestamp_from_rfc3339("2026-07-18T11:00:00Z"),
        filter: HashMap::from([
            ("session_id".to_string(), session_id.to_string()),
            ("operation_id".to_string(), operation_id.to_string()),
            ("event_kind".to_string(), "OperationFailed".to_string()),
        ]),
    })
    .expect("valid audit query");
    let records = [
        audit_test_record(
            1,
            "2026-07-18T09:59:59Z",
            Some(session_id),
            Some(operation_id),
            AuditEvent::OperationFailed,
        ),
        audit_test_record(
            2,
            "2026-07-18T10:30:00Z",
            Some(session_id),
            Some(operation_id),
            AuditEvent::OperationFailed,
        ),
        audit_test_record(
            3,
            "2026-07-18T11:00:00Z",
            Some(session_id),
            Some(operation_id),
            AuditEvent::OperationFailed,
        ),
        audit_test_record(
            4,
            "2026-07-18T10:45:00Z",
            Some(Uuid::new_v4()),
            Some(operation_id),
            AuditEvent::OperationFailed,
        ),
    ];
    let matched = records
        .iter()
        .filter(|record| audit_record_matches(record, &query).expect("match audit record"))
        .map(|record| record.sequence)
        .collect::<Vec<_>>();
    assert_eq!(matched, vec![2]);
}

#[test]
fn audit_query_uuid_filters_canonicalize_uppercase_and_unhyphenated_inputs() {
    let session_id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("session UUID");
    let operation_id =
        Uuid::parse_str("a987fbc9-4bed-4078-8f07-9141ba07c9f3").expect("operation UUID");
    let query = AuditQuery::try_from(pb::QueryAuditRequest {
        since: None,
        until: None,
        filter: HashMap::from([
            (
                "session_id".to_string(),
                session_id.to_string().to_ascii_uppercase(),
            ),
            (
                "operation_id".to_string(),
                operation_id.simple().to_string(),
            ),
            (
                "subject_id".to_string(),
                session_id.simple().to_string().to_ascii_uppercase(),
            ),
        ]),
    })
    .expect("non-canonical UUID spellings are valid filters");
    let record = audit_test_record(
        1,
        "2026-07-18T10:30:00Z",
        Some(session_id),
        Some(operation_id),
        AuditEvent::OperationFailed,
    );

    assert!(audit_record_matches(&record, &query).expect("match canonical UUIDs"));
}

#[tokio::test]
async fn audit_service_streams_filtered_records() {
    let state = ApiState::new(test_index());
    let session_id = Uuid::new_v4();
    let mut audit = FileAuditLog::open(state.audit_dir.as_ref(), false).expect("open audit");
    for (current_session, event) in [
        (session_id, AuditEvent::OperationFailed),
        (Uuid::new_v4(), AuditEvent::OperationFinished),
    ] {
        audit
            .append_and_return_record(AuditEventRecord {
                actor: AuditActor::System,
                source_layer: SourceLayer::Layer5,
                operation_id: None,
                session_id: Some(current_session),
                idempotency_key: None,
                event,
                subject: AuditSubject {
                    kind: "session".to_string(),
                    id: Some(current_session.to_string()),
                },
                detail: BTreeMap::new(),
            })
            .expect("append audit record");
    }
    drop(audit);
    let mut stream = pb::audit_server::Audit::query_audit(
        &state.audit_service(),
        Request::new(pb::QueryAuditRequest {
            since: timestamp_from_rfc3339("2020-01-01T00:00:00Z"),
            until: timestamp_from_rfc3339("2100-01-01T00:00:00Z"),
            filter: HashMap::from([("session_id".to_string(), session_id.to_string())]),
        }),
    )
    .await
    .expect("query audit")
    .into_inner();
    let entry = stream.next().await.expect("one entry").expect("audit item");
    assert_eq!(entry.session_id, session_id.as_bytes());
    assert_eq!(entry.event_kind, "OperationFailed");
    assert_eq!(
        entry.software_build.as_deref(),
        Some(remanence_state::audit::software_build())
    );
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn daemon_operations_are_projected() {
    let service = state_with_operation().daemon_service();
    let operation_id = operation_uuid();

    let status = pb::daemon_server::Daemon::get_operation(
        &service,
        Request::new(pb::GetOperationRequest {
            operation_id: operation_id.as_bytes().to_vec(),
        }),
    )
    .await
    .expect("get operation")
    .into_inner();
    assert_eq!(status.operation_id, operation_id.as_bytes().to_vec());
    assert_eq!(status.operation_kind, "write_object");
    assert_eq!(status.state, pb::OperationState::Succeeded as i32);
    assert!(status.created_at.is_some());
    assert!(status.updated_at.is_some());
    assert!(status.progress.is_empty());
    assert!(status.error_summary.is_empty());

    let listed = pb::daemon_server::Daemon::list_operations(
        &service,
        Request::new(pb::ListOperationsRequest {
            filter: Default::default(),
            page_token: None,
            page_size: 0,
        }),
    )
    .await
    .expect("list operations")
    .into_inner();
    assert_eq!(listed.operations, vec![status.clone()]);
    assert!(listed.next_page_token.is_none());

    let filtered = pb::daemon_server::Daemon::list_operations(
        &service,
        Request::new(pb::ListOperationsRequest {
            filter: [("state".to_string(), "succeeded".to_string())]
                .into_iter()
                .collect(),
            page_token: None,
            page_size: 0,
        }),
    )
    .await
    .expect("list filtered operations")
    .into_inner();
    assert_eq!(filtered.operations, vec![status]);
}

#[tokio::test]
async fn daemon_reports_queued_operation() {
    let service = state_with_queued_operation().daemon_service();
    let operation_id = operation_uuid();

    let status = pb::daemon_server::Daemon::get_operation(
        &service,
        Request::new(pb::GetOperationRequest {
            operation_id: operation_id.as_bytes().to_vec(),
        }),
    )
    .await
    .expect("get queued operation")
    .into_inner();

    assert_eq!(status.operation_id, operation_id.as_bytes().to_vec());
    assert_eq!(status.operation_kind, "write_object");
    assert_eq!(status.state, pb::OperationState::Queued as i32);
    assert!(status.created_at.is_some());
    assert_eq!(status.created_at, status.updated_at);
    assert!(status.progress.is_empty());
    assert!(status.error_summary.is_empty());
}

#[tokio::test]
async fn daemon_reports_durable_operation_failure_detail() {
    let service = state_with_failed_operation().daemon_service();

    let status = pb::daemon_server::Daemon::get_operation(
        &service,
        Request::new(pb::GetOperationRequest {
            operation_id: operation_uuid().as_bytes().to_vec(),
        }),
    )
    .await
    .expect("get failed operation")
    .into_inner();

    assert_eq!(status.operation_kind, "clean_drive");
    assert_eq!(status.state, pb::OperationState::Failed as i32);
    assert_eq!(
        status.error_summary,
        "no eligible cleaning cartridge: CLNU01L9 is expired"
    );
}

#[tokio::test]
async fn watch_streams_until_terminal_and_cancel_flips_token() {
    let state = ApiState::new(test_index());
    let operation_id = Uuid::from_u128(7);
    let handle = state.operations.register(operation_id, "reconcile_tape");
    let daemon = state.daemon_service();

    pb::daemon_server::Daemon::cancel_operation(
        &daemon,
        Request::new(pb::CancelOperationRequest {
            operation_id: operation_id.as_bytes().to_vec(),
            idempotency_key: None,
            force: false,
        }),
    )
    .await
    .expect("cancel operation");
    assert!(handle.is_cancelled());

    handle.publish(crate::operations::status(
        operation_id,
        "reconcile_tape",
        pb::OperationState::Cancelled,
        &[],
    ));
    let mut stream = pb::daemon_server::Daemon::watch_operation(
        &daemon,
        Request::new(pb::GetOperationRequest {
            operation_id: operation_id.as_bytes().to_vec(),
        }),
    )
    .await
    .expect("watch operation")
    .into_inner();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.state, pb::OperationState::Cancelled as i32);
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn cancel_terminal_live_operation_does_not_regress_durable_state() {
    let state = state_with_operation();
    let operation_id = operation_uuid();
    let handle = state.operations.register(operation_id, "write_object");
    handle.publish(crate::operations::status(
        operation_id,
        "write_object",
        pb::OperationState::Succeeded,
        &[],
    ));
    let daemon = state.daemon_service();

    let cancel = pb::daemon_server::Daemon::cancel_operation(
        &daemon,
        Request::new(pb::CancelOperationRequest {
            operation_id: operation_id.as_bytes().to_vec(),
            idempotency_key: None,
            force: false,
        }),
    )
    .await
    .expect("cancel terminal operation")
    .into_inner();
    assert_eq!(cancel.resulting_state, pb::OperationState::Succeeded as i32);

    let durable = pb::daemon_server::Daemon::get_operation(
        &daemon,
        Request::new(pb::GetOperationRequest {
            operation_id: operation_id.as_bytes().to_vec(),
        }),
    )
    .await
    .expect("get durable operation")
    .into_inner();
    assert_eq!(durable.state, pb::OperationState::Succeeded as i32);
}

#[test]
fn drive_pool_reserves_bays_independently() {
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let key1 = crate::drive_pool::DriveKey::new("LIB001", 0x0101);
    let key2 = crate::drive_pool::DriveKey::new("LIB001", 0x0102);
    let reservations = Arc::new(HashMap::from([
        (key1.clone(), AtomicBool::new(false)),
        (key2, AtomicBool::new(false)),
    ]));
    let pool = crate::write_owner::DrivePool::new_with_lifecycle(
        HashMap::from([("LIB001".to_string(), changer_tx)]),
        HashMap::new(),
        reservations.clone(),
        crate::drive_pool::DrivePoolLifecycle::default(),
    );

    assert_eq!(
        pool.reserve_free_drive("LIB001").expect("first bay").bay,
        0x0101
    );
    assert_eq!(
        pool.reserve_free_drive("LIB001").expect("second bay").bay,
        0x0102
    );
    assert_eq!(
        pool.reserve_free_drive("LIB001")
            .expect_err("pool full")
            .code(),
        tonic::Code::FailedPrecondition
    );
    pool.release(&key1);
    assert_eq!(
        pool.reserve_free_drive("LIB001").expect("released bay").bay,
        0x0101
    );
    assert!(reservations
        .get(&key1)
        .expect("reservation")
        .load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn drive_pool_exclusive_reservation_rolls_back_on_busy_bay() {
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let key1 = crate::drive_pool::DriveKey::new("LIB001", 0x0101);
    let key2 = crate::drive_pool::DriveKey::new("LIB001", 0x0102);
    let key3 = crate::drive_pool::DriveKey::new("LIB001", 0x0103);
    let reservations = Arc::new(HashMap::from([
        (key1.clone(), AtomicBool::new(false)),
        (key2.clone(), AtomicBool::new(true)),
        (key3.clone(), AtomicBool::new(false)),
    ]));
    let pool = crate::write_owner::DrivePool::new_with_lifecycle(
        HashMap::from([("LIB001".to_string(), changer_tx)]),
        HashMap::new(),
        reservations.clone(),
        crate::drive_pool::DrivePoolLifecycle::default(),
    );

    assert_eq!(
        pool.reserve_all_exclusive()
            .expect_err("busy bay blocks exclusive reservation")
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert!(!reservations
        .get(&key1)
        .expect("rolled back")
        .load(std::sync::atomic::Ordering::SeqCst));
    assert!(reservations
        .get(&key2)
        .expect("busy remains busy")
        .load(std::sync::atomic::Ordering::SeqCst));
    assert!(!reservations
        .get(&key3)
        .expect("unvisited remains free")
        .load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn drive_pool_shutdown_closes_normal_admission_but_keeps_cleanup_reservation() {
    let bay = 0x0101;
    let key = crate::drive_pool::DriveKey::new("LIB001", bay);
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let reservations = Arc::new(HashMap::from([(key.clone(), AtomicBool::new(false))]));
    let pool = crate::write_owner::DrivePool::new_with_lifecycle(
        HashMap::from([("LIB001".to_string(), changer_tx)]),
        HashMap::new(),
        reservations.clone(),
        crate::drive_pool::DrivePoolLifecycle::default(),
    );

    pool.begin_shutdown();

    let err = pool
        .reserve_drive(&key)
        .expect_err("normal admission must close during shutdown");
    assert_eq!(err.code(), tonic::Code::Unavailable);
    let cleanup = pool
        .reserve_drive_for_shutdown(&key)
        .expect("shutdown cleanup keeps a reservation path");
    assert!(reservations
        .get(&key)
        .expect("reservation")
        .load(std::sync::atomic::Ordering::SeqCst));
    drop(cleanup);
}

#[test]
fn drive_pool_exclusive_guard_drop_releases_all_bays() {
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let key1 = crate::drive_pool::DriveKey::new("LIB001", 0x0101);
    let key2 = crate::drive_pool::DriveKey::new("LIB001", 0x0102);
    let reservations = Arc::new(HashMap::from([
        (key1, AtomicBool::new(false)),
        (key2, AtomicBool::new(false)),
    ]));
    let pool = crate::write_owner::DrivePool::new_with_lifecycle(
        HashMap::from([("LIB001".to_string(), changer_tx)]),
        HashMap::new(),
        reservations.clone(),
        crate::drive_pool::DrivePoolLifecycle::default(),
    );

    pool.reserve_all_exclusive().expect("reserve all");
    assert_eq!(
        pool.reserve_free_drive("LIB001")
            .expect_err("exclusive reservation holds all bays")
            .code(),
        tonic::Code::FailedPrecondition
    );
    drop(crate::write_owner::ExclusiveGuard::from_reserved(
        reservations.clone(),
    ));
    assert_eq!(
        pool.reserve_free_drive("LIB001").expect("released bay").bay,
        0x0101
    );
}

#[test]
fn drive_pool_tracks_mounted_tapes_for_selection_exclusion() {
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(false))]));
    let pool = crate::write_owner::DrivePool::new(changer_tx, HashMap::new(), reservations);
    let session_id = Uuid::new_v4();

    pool.record_session(
        session_id,
        crate::write_owner::MountedSession {
            bay: 0x0101,
            library_serial: "LIB-A".to_string(),
            barcode: Some("AOX001L9".to_string()),
            home_slot: Some(0x1001),
            tape_uuid: TAPE_UUID,
            drive_uuid: Some(Uuid::new_v4().as_bytes().to_vec()),
        },
    );

    assert!(pool.mounted_tape_uuids().contains(&TAPE_UUID));
    pool.forget_session(session_id);
    assert!(!pool.mounted_tape_uuids().contains(&TAPE_UUID));
}

#[test]
fn drive_pool_close_parks_cartridge_and_follow_on_session_claims_it() {
    let bay = 0x0101;
    let key = crate::drive_pool::DriveKey::new("LIB-A", bay);
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let reservations = Arc::new(HashMap::from([(key.clone(), AtomicBool::new(true))]));
    let pool = crate::write_owner::DrivePool::new_with_lifecycle(
        HashMap::from([("LIB-A".to_string(), changer_tx)]),
        HashMap::new(),
        reservations.clone(),
        crate::drive_pool::DrivePoolLifecycle::default(),
    );
    let first_session = Uuid::new_v4();
    let mounted = crate::write_owner::MountedSession {
        bay,
        library_serial: "LIB-A".to_string(),
        barcode: Some("AOX001L9".to_string()),
        home_slot: Some(0x1001),
        tape_uuid: TAPE_UUID,
        drive_uuid: Some(Uuid::new_v4().as_bytes().to_vec()),
    };
    pool.record_session(first_session, mounted.clone());

    let parked = pool
        .finish_session(first_session, mounted.clone())
        .expect("library tape remains parked");
    assert!(pool.parked_is_current(&parked));
    assert_eq!(parked.seated.prior_session_id, Some(first_session));
    assert!(!reservations
        .get(&key)
        .expect("reservation")
        .load(std::sync::atomic::Ordering::SeqCst));
    assert!(!pool.mounted_tape_uuids().contains(&TAPE_UUID));

    let follow_on = Uuid::new_v4();
    pool.record_session(follow_on, mounted);
    assert!(pool
        .parked_at(&crate::drive_pool::DriveKey::new("LIB-A", bay))
        .is_none());
    assert!(pool.mounted_tape_uuids().contains(&TAPE_UUID));
}

#[tokio::test]
async fn drive_byte_accounting_uses_session_drive_uuid_for_shared_bay() {
    let state = ApiState::new(test_index());
    let shared_bay = 0x0101;
    let session_a = crate::write_owner::MountedSession {
        bay: shared_bay,
        library_serial: "LIB-A".to_string(),
        barcode: Some("AOX001L9".to_string()),
        home_slot: None,
        tape_uuid: TAPE_UUID,
        drive_uuid: Some(Uuid::from_u128(0x1111).as_bytes().to_vec()),
    };
    let session_b = crate::write_owner::MountedSession {
        bay: shared_bay,
        library_serial: "LIB-A".to_string(),
        barcode: Some("AOX002L9".to_string()),
        home_slot: None,
        tape_uuid: TAPE_UUID,
        drive_uuid: Some(Uuid::from_u128(0x2222).as_bytes().to_vec()),
    };
    let mut read_a = CountingBytesStream {
        inner: Box::pin(tokio_stream::iter(vec![Ok(pb::BytesChunk {
            data: b"abc".to_vec(),
            is_last: true,
        })])),
        state: state.clone(),
        drive_uuid: session_a.drive_uuid.clone(),
    };
    let mut read_b = CountingBytesStream {
        inner: Box::pin(tokio_stream::iter(vec![Ok(pb::BytesChunk {
            data: b"defgh".to_vec(),
            is_last: true,
        })])),
        state: state.clone(),
        drive_uuid: session_b.drive_uuid.clone(),
    };

    assert_eq!(session_a.bay, session_b.bay);
    assert!(read_a.next().await.is_some());
    assert!(read_b.next().await.is_some());
    state.record_drive_write_bytes(session_b.drive_uuid.as_deref(), 7);

    let counters = state
        .live_status
        .drive_counters
        .read()
        .unwrap_or_else(|err| err.into_inner());
    assert_eq!(counters.len(), 2);
    let a = counters
        .get(
            session_a
                .drive_uuid
                .as_deref()
                .expect("session A drive uuid"),
        )
        .expect("session A counter");
    let b = counters
        .get(
            session_b
                .drive_uuid
                .as_deref()
                .expect("session B drive uuid"),
        )
        .expect("session B counter");
    assert_eq!(a.read_bytes.load(AtomicOrdering::Relaxed), 3);
    assert_eq!(a.write_bytes.load(AtomicOrdering::Relaxed), 0);
    assert_eq!(b.read_bytes.load(AtomicOrdering::Relaxed), 5);
    assert_eq!(b.write_bytes.load(AtomicOrdering::Relaxed), 7);
}

#[tokio::test]
async fn append_finish_does_not_double_count() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-append-finish-live-counter")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let index = CatalogIndex::open(&index_path).expect("open catalog");
    let mut state = ApiState::new(index);
    let session_id = Uuid::new_v4();
    let bay = 0x0101;
    let drive_uuid = Uuid::new_v4().as_bytes().to_vec();
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
    let reservations = Arc::new(std::collections::HashMap::from([(
        bay,
        std::sync::atomic::AtomicBool::new(false),
    )]));
    let pool = crate::write_owner::DrivePool::new_for_library(
        "LIB-A",
        changer_tx,
        std::collections::HashMap::from([(bay, drive_tx)]),
        reservations,
    );
    pool.record_session(
        session_id,
        crate::write_owner::MountedSession {
            bay,
            library_serial: "LIB-A".to_string(),
            barcode: Some("AOX001L9".to_string()),
            home_slot: Some(0x0400),
            tape_uuid: [0xAB; 16],
            drive_uuid: Some(drive_uuid.clone()),
        },
    );
    state.drive_pool = Some(pool);

    let actor = tokio::spawn(async move {
        while let Some(cmd) = drive_rx.recv().await {
            match cmd {
                crate::write_owner::DriveCommand::AppendFinish {
                    source,
                    live_write_counter,
                    reply,
                    ..
                } => {
                    let spool_path = match source {
                        crate::WriteObjectSource::Path(path) => path,
                        crate::WriteObjectSource::Streamed(_) => {
                            panic!("test expected path-backed append source")
                        }
                    };
                    let counter = live_write_counter.expect("live write counter");
                    counter.record_write_bytes(3);
                    counter.record_write_bytes(5);
                    let _ = std::fs::remove_file(spool_path);
                    let _ = reply.send(Ok(crate::write_owner::AppendFinishOutcome {
                        record: pb::ObjectRecord {
                            object_id: Uuid::nil().to_string().into_bytes(),
                            caller_object_id: Some("caller-object".to_string()),
                            content_sha256: Some(vec![0x11; 32]),
                            logical_size_bytes: Some(8),
                            body_format: Some("rem-object-v1".to_string()),
                            caller_metadata: Default::default(),
                            created_at: None,
                            copies: Vec::new(),
                            append_commit_info: None,
                            content_digest: None,
                            metadata_digest: None,
                        },
                        replay: false,
                    }));
                }
                _ => panic!("unexpected drive command"),
            }
        }
    });

    let spool_path = temp.path().join("spool.bin");
    std::fs::write(&spool_path, b"spool").expect("write spool");
    let archive_path = temp.path().join("archive.rem-object");

    let record = crate::mount::append_finish(
        &state,
        session_id,
        crate::mount::AppendFinishRequest {
            spool_path,
            archive_path,
            caller_object_id: "caller-object".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
        },
    )
    .await
    .expect("append finish");

    assert_eq!(record.logical_size_bytes, Some(8));
    let counter = state.drive_counters(&drive_uuid);
    assert_eq!(counter.write_bytes(), 8);

    actor.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlap_idempotent_replay_cancels_the_live_receive_and_returns_the_record() {
    let mut state = ApiState::new(test_index());
    let session_id = Uuid::new_v4();
    let bay = 0x0102;
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
    let reservations = Arc::new(std::collections::HashMap::from([(
        bay,
        std::sync::atomic::AtomicBool::new(false),
    )]));
    let pool = crate::write_owner::DrivePool::new_for_library(
        "LIB-A",
        changer_tx,
        std::collections::HashMap::from([(bay, drive_tx)]),
        reservations,
    );
    pool.record_session(
        session_id,
        crate::write_owner::MountedSession {
            bay,
            library_serial: "LIB-A".to_string(),
            barcode: Some("AOX002L9".to_string()),
            home_slot: Some(0x0401),
            tape_uuid: [0xAC; 16],
            drive_uuid: None,
        },
    );
    state.drive_pool = Some(pool);
    state.append_staging_mode = remanence_state::AppendStagingMode::Overlap;
    let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    state.append_ring_bytes = ring_bytes;
    state.append_ring_high_pct = 50;
    state.append_ring_low_pct = 25;
    state.io_memory = crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("manager");

    let replayed_id = Uuid::new_v4();
    let actor = tokio::spawn(async move {
        let command = drive_rx.recv().await.expect("append command");
        let crate::write_owner::DriveCommand::AppendFinish { source, reply, .. } = command else {
            panic!("expected append command");
        };
        assert!(matches!(source, crate::WriteObjectSource::Streamed(_)));
        let _ = reply.send(Ok(crate::write_owner::AppendFinishOutcome {
            record: pb::ObjectRecord {
                object_id: replayed_id.as_bytes().to_vec(),
                caller_object_id: Some("caller-object".to_string()),
                content_sha256: Some(vec![0x11; 32]),
                logical_size_bytes: Some(2 * ring_bytes),
                body_format: Some("rem-object-v1".to_string()),
                caller_metadata: Default::default(),
                created_at: None,
                copies: Vec::new(),
                append_commit_info: None,
                content_digest: None,
                metadata_digest: None,
            },
            replay: true,
        }));
    });

    let payload = vec![0x5a; 2 * crate::append_ring::APPEND_RING_SLAB_BYTES];
    let digest: [u8; 32] = Sha256::digest(&payload).into();
    let mut start_message = append_start_message(session_id, payload.len() as u64);
    let Some(pb::append_object_message::Payload::Start(start_fields)) =
        start_message.payload.as_mut()
    else {
        panic!("start helper must emit Start");
    };
    start_fields.expected_content_sha256 = Some(digest.to_vec());
    start_fields.source_replay_capability = pb::SourceReplayCapability::ReplayFromStart as i32;
    let messages = vec![
        Ok(start_message),
        Ok(append_chunk_message(
            session_id,
            payload[..crate::append_ring::APPEND_RING_SLAB_BYTES].to_vec(),
        )),
        Ok(append_chunk_message(
            session_id,
            payload[crate::append_ring::APPEND_RING_SLAB_BYTES..].to_vec(),
        )),
        Ok(append_finish_message(session_id, digest)),
    ];
    let api = WriteSessionApi { state };
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        api.append_object_stream(tokio_stream::iter(messages)),
    )
    .await
    .expect("replay must not deadlock on a full receive ring")
    .expect("committed replay returns success")
    .into_inner();
    assert_eq!(response.object_id, replayed_id.as_bytes());
    actor.await.expect("actor task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlap_success_response_waits_for_checkpointed_receipt() {
    let mut state = ApiState::new(test_index());
    let session_id = Uuid::new_v4();
    let bay = 0x0103;
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(2);
    let reservations = Arc::new(std::collections::HashMap::from([(
        bay,
        std::sync::atomic::AtomicBool::new(false),
    )]));
    let pool = crate::write_owner::DrivePool::new_for_library(
        "LIB-A",
        changer_tx,
        std::collections::HashMap::from([(bay, drive_tx)]),
        reservations,
    );
    pool.record_session(
        session_id,
        crate::write_owner::MountedSession {
            bay,
            library_serial: "LIB-A".to_string(),
            barcode: Some("AOX003L9".to_string()),
            home_slot: Some(0x0402),
            tape_uuid: [0xAD; 16],
            drive_uuid: None,
        },
    );
    state.drive_pool = Some(pool);
    state.append_staging_mode = remanence_state::AppendStagingMode::Overlap;
    let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    state.append_ring_bytes = ring_bytes;
    state.append_ring_high_pct = 50;
    state.append_ring_low_pct = 25;
    state.io_memory = crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("manager");

    let object_id = Uuid::new_v4();
    let (checkpoint_seen_tx, checkpoint_seen_rx) = tokio::sync::oneshot::channel();
    let (release_checkpoint_tx, release_checkpoint_rx) = tokio::sync::oneshot::channel();
    let actor = tokio::spawn(async move {
        let command = drive_rx.recv().await.expect("append command");
        let crate::write_owner::DriveCommand::AppendFinish { source, reply, .. } = command else {
            panic!("expected append command");
        };
        let streamed = match source {
            crate::WriteObjectSource::Streamed(streamed) => streamed,
            crate::WriteObjectSource::Path(_) => panic!("expected overlap stream"),
        };
        let received = tokio::task::spawn_blocking(move || streamed.read_all_for_test())
            .await
            .expect("overlap consumer task")
            .expect("actor consumes overlap stream");
        assert!(!received.is_empty());
        let written = pb::ObjectRecord {
            object_id: object_id.as_bytes().to_vec(),
            append_commit_info: Some(pb::AppendCommitInfo {
                durability: pb::AppendDurability::Written as i32,
                ..Default::default()
            }),
            ..Default::default()
        };
        reply
            .send(Ok(crate::write_owner::AppendFinishOutcome {
                record: written,
                replay: false,
            }))
            .expect("send WRITTEN");

        let command = drive_rx.recv().await.expect("checkpoint command");
        let crate::write_owner::DriveCommand::Checkpoint { reply, .. } = command else {
            panic!("overlap must funnel through checkpoint before returning");
        };
        checkpoint_seen_tx.send(()).expect("signal checkpoint call");
        release_checkpoint_rx.await.expect("release checkpoint");
        let mut checkpointed = pb::ObjectRecord {
            object_id: object_id.as_bytes().to_vec(),
            ..Default::default()
        };
        checkpointed.append_commit_info = Some(pb::AppendCommitInfo {
            durability: pb::AppendDurability::Checkpointed as i32,
            ..Default::default()
        });
        reply
            .expect("explicit checkpoint carries a reply")
            .send(Ok(crate::write_owner::CheckpointActorReply {
                session: pb::WriteSession::default(),
                committed_objects: vec![checkpointed],
            }))
            .expect("send CHECKPOINTED");
    });

    let payload = vec![0x6b; crate::append_ring::APPEND_RING_SLAB_BYTES];
    let digest: [u8; 32] = Sha256::digest(&payload).into();
    let mut start_message = append_start_message(session_id, payload.len() as u64);
    let Some(pb::append_object_message::Payload::Start(start_fields)) =
        start_message.payload.as_mut()
    else {
        panic!("start helper must emit Start");
    };
    start_fields.expected_content_sha256 = Some(digest.to_vec());
    start_fields.source_replay_capability = pb::SourceReplayCapability::ReplayFromStart as i32;
    let messages = vec![
        Ok(start_message),
        Ok(append_chunk_message(session_id, payload)),
        Ok(append_finish_message(session_id, digest)),
    ];
    let api = WriteSessionApi { state };
    let append =
        tokio::spawn(async move { api.append_object_stream(tokio_stream::iter(messages)).await });
    checkpoint_seen_rx
        .await
        .expect("overlap issued explicit checkpoint");
    assert!(
        !append.is_finished(),
        "caller response must remain held while durability is only WRITTEN"
    );
    release_checkpoint_tx
        .send(())
        .expect("release checkpoint reply");
    let response = append
        .await
        .expect("append task")
        .expect("checkpointed overlap succeeds")
        .into_inner();
    assert_eq!(response.object_id, object_id.as_bytes());
    assert_eq!(
        response
            .append_commit_info
            .expect("checkpoint receipt")
            .durability,
        pb::AppendDurability::Checkpointed as i32
    );
    actor.await.expect("actor task");
}

#[test]
fn drive_byte_accounting_skips_unresolvable_drive_and_warns() {
    let state = ApiState::new(test_index());
    let warnings = capture_warnings(|| {
        state.record_drive_read_bytes(None, 512);
        state.record_drive_write_bytes(Some(&[]), 1024);
    });

    let counters = state
        .live_status
        .drive_counters
        .read()
        .unwrap_or_else(|err| err.into_inner());
    assert!(counters.is_empty());
    assert_eq!(warnings.len(), 2);
    assert!(warnings
        .iter()
        .all(|message| message.contains("skipping byte accounting for unresolved drive")));
}

#[test]
fn drive_pool_tracks_pending_tape_reservations_for_selection_exclusion() {
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(false))]));
    let pool = crate::write_owner::DrivePool::new(changer_tx, HashMap::new(), reservations);

    let reservation = pool.reserve_tape(TAPE_UUID).expect("reserve tape");

    assert!(pool.mounted_tape_uuids().contains(&TAPE_UUID));
    assert_eq!(
        pool.reserve_tape(TAPE_UUID)
            .expect_err("duplicate tape reservation")
            .code(),
        tonic::Code::FailedPrecondition
    );
    drop(reservation);
    assert!(!pool.mounted_tape_uuids().contains(&TAPE_UUID));
}

#[tokio::test]
async fn catalog_lists_tapes_and_tape_files() {
    let service = populated_state().catalog_service();

    let tapes = pb::catalog_server::Catalog::list_tapes(
        &service,
        Request::new(pb::ListTapesRequest {
            library_uuid: Vec::new(),
            page_token: None,
            page_size: 0,
            pool_id: String::new(),
            kind: "data".to_string(),
        }),
    )
    .await
    .expect("list tapes")
    .into_inner();
    assert_eq!(tapes.tapes.len(), 1);
    assert_eq!(tapes.tapes[0].tape_uuid, TAPE_UUID.to_vec());
    assert_eq!(tapes.tapes[0].body_format.as_deref(), Some("rem-object-v1"));
    assert_eq!(tapes.tapes[0].block_size_bytes, Some(4096));
    assert_eq!(tapes.tapes[0].last_committed_tape_file, Some(7));
    assert_eq!(tapes.tapes[0].state, pb::tape::State::TapeStateReady as i32);
    assert_eq!(tapes.tapes[0].pool_id.as_deref(), Some("camera.copy-a"));

    let filtered_tapes = pb::catalog_server::Catalog::list_tapes(
        &service,
        Request::new(pb::ListTapesRequest {
            library_uuid: Vec::new(),
            page_token: None,
            page_size: 0,
            pool_id: "camera.copy-a".to_string(),
            kind: "data".to_string(),
        }),
    )
    .await
    .expect("list tapes by pool")
    .into_inner();
    assert_eq!(filtered_tapes.tapes, tapes.tapes);

    let pools = pb::catalog_server::Catalog::list_tape_pools(
        &service,
        Request::new(pb::ListTapePoolsRequest {
            page_token: None,
            page_size: 0,
        }),
    )
    .await
    .expect("list tape pools")
    .into_inner();
    assert_eq!(pools.pools.len(), 1);
    assert_eq!(pools.pools[0].pool_id, "camera.copy-a");
    assert_eq!(pools.pools[0].display_name, "Camera copy A");
    assert_eq!(pools.pools[0].copy_class, "copy-a");
    assert_eq!(pools.pools[0].content_class, "camera");

    let pool = pb::catalog_server::Catalog::get_tape_pool(
        &service,
        Request::new(pb::GetTapePoolRequest {
            pool_id: "camera.copy-a".to_string(),
        }),
    )
    .await
    .expect("get tape pool")
    .into_inner();
    assert_eq!(pool, pools.pools[0]);

    let invalid_pool = pb::catalog_server::Catalog::list_tapes(
        &service,
        Request::new(pb::ListTapesRequest {
            library_uuid: Vec::new(),
            page_token: None,
            page_size: 0,
            pool_id: "camera copy a".to_string(),
            kind: "data".to_string(),
        }),
    )
    .await
    .expect_err("invalid pool id must fail");
    assert_eq!(invalid_pool.code(), tonic::Code::InvalidArgument);

    let tape = pb::catalog_server::Catalog::get_tape(
        &service,
        Request::new(pb::GetTapeRequest {
            tape_uuid: TAPE_UUID.to_vec(),
        }),
    )
    .await
    .expect("get tape")
    .into_inner();
    assert_eq!(tape.tape_uuid, TAPE_UUID.to_vec());

    let files = pb::catalog_server::Catalog::list_tape_files(
        &service,
        Request::new(pb::ListTapeFilesRequest {
            tape_uuid: TAPE_UUID.to_vec(),
            page_token: None,
            page_size: 0,
        }),
    )
    .await
    .expect("list tape files")
    .into_inner();
    assert_eq!(files.tape_files.len(), 4);
    assert_eq!(files.tape_files[0].kind, "object");
    assert_eq!(
        files.tape_files[0].object_id,
        object_uuid().as_bytes().to_vec()
    );
    assert_eq!(files.tape_files[1].kind, "parity_sidecar");
    assert_eq!(files.tape_files[2].kind, "parity_map");
    assert_eq!(files.tape_files[3].kind, "bootstrap");
}

#[tokio::test]
async fn terminal_inventory_rpcs_require_an_exact_tape_uuid_before_mounting() {
    let service = populated_state().catalog_service();
    let inventory = pb::catalog_server::Catalog::get_tape_inventory(
        &service,
        Request::new(pb::TapeInventoryRequest {
            tape_uuid: vec![0x11; 15],
        }),
    )
    .await
    .err()
    .expect("short tape UUID must fail before drive ownership");
    assert_eq!(inventory.code(), tonic::Code::InvalidArgument);

    let verify = pb::catalog_server::Catalog::verify_tape_index(
        &service,
        Request::new(pb::VerifyTapeIndexRequest {
            tape_uuid: vec![0x22; 17],
        }),
    )
    .await
    .expect_err("long tape UUID must fail before drive ownership");
    assert_eq!(verify.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn catalog_lists_and_fetches_files_in_native_object() {
    let service = populated_state_with_file_catalog().catalog_service();

    let files = pb::catalog_server::Catalog::list_files_in_object(
        &service,
        Request::new(pb::ListFilesInObjectRequest {
            object_id: object_uuid().as_bytes().to_vec(),
            page_token: None,
            page_size: 0,
        }),
    )
    .await
    .expect("list object files")
    .into_inner();
    assert_eq!(files.files.len(), 1);
    let file = &files.files[0];
    assert_eq!(file.object_id, object_uuid().as_bytes().to_vec());
    assert_eq!(file.file_id, b"file-camera");
    assert_eq!(file.path, "payload.bin");
    assert_eq!(file.size_bytes, 17);
    assert_eq!(file.file_sha256, vec![7u8; 32]);
    assert_eq!(
        file.file_digest
            .as_ref()
            .map(|digest| digest.algorithm.as_str()),
        Some("sha256")
    );
    assert_eq!(
        file.file_digest
            .as_ref()
            .map(|digest| digest.value.as_slice()),
        Some(&[7u8; 32][..])
    );
    assert_eq!(file.first_chunk_body_lba, Some(2));
    assert_eq!(file.chunk_count, 1);

    let by_path = pb::catalog_server::Catalog::get_file(
        &service,
        Request::new(pb::GetFileRequest {
            object_id: object_uuid().as_bytes().to_vec(),
            key: Some(pb::get_file_request::Key::Path("payload.bin".to_string())),
        }),
    )
    .await
    .expect("get file by path")
    .into_inner();
    assert_eq!(by_path, *file);

    let by_id = pb::catalog_server::Catalog::get_file(
        &service,
        Request::new(pb::GetFileRequest {
            object_id: object_uuid().as_bytes().to_vec(),
            key: Some(pb::get_file_request::Key::FileId(b"file-camera".to_vec())),
        }),
    )
    .await
    .expect("get file by id")
    .into_inner();
    assert_eq!(by_id, *file);
}

#[tokio::test]
async fn catalog_enumerates_and_fetches_native_objects() {
    let service = populated_state().catalog_service();
    let mut stream = pb::catalog_server::Catalog::enumerate_objects(
        &service,
        Request::new(pb::EnumerateObjectsRequest {
            scope: Some(pb::enumerate_objects_request::Scope::All(())),
            reconcile_from_tape: false,
        }),
    )
    .await
    .expect("enumerate objects")
    .into_inner();

    let first = stream
        .next()
        .await
        .expect("one object")
        .expect("object record");
    assert_eq!(first.object_id, object_uuid().as_bytes().to_vec());
    assert_eq!(first.caller_object_id.as_deref(), Some("caller-1"));
    assert_eq!(first.body_format.as_deref(), Some("rem-object-v1"));
    assert_eq!(first.logical_size_bytes, Some(17));
    assert_eq!(first.content_sha256, Some(vec![7u8; 32]));
    assert_eq!(first.copies.len(), 1);
    assert_eq!(first.copies[0].pool_id, "camera.copy-a");
    assert!(stream.next().await.is_none());

    let fetched = pb::catalog_server::Catalog::get_object(
        &service,
        Request::new(pb::GetObjectRequest {
            key: Some(pb::get_object_request::Key::CallerObjectId(
                "caller-1".to_string(),
            )),
        }),
    )
    .await
    .expect("get object")
    .into_inner();
    assert_eq!(fetched.object_id, object_uuid().as_bytes().to_vec());

    let fetched_by_id = pb::catalog_server::Catalog::get_object(
        &service,
        Request::new(pb::GetObjectRequest {
            key: Some(pb::get_object_request::Key::ObjectId(
                object_uuid().as_bytes().to_vec(),
            )),
        }),
    )
    .await
    .expect("get object by uuid")
    .into_inner();
    assert_eq!(fetched_by_id.object_id, object_uuid().as_bytes().to_vec());

    let fetched_by_digest = pb::catalog_server::Catalog::get_object(
        &service,
        Request::new(pb::GetObjectRequest {
            key: Some(pb::get_object_request::Key::ContentDigest(pb::Digest {
                algorithm: "sha256".to_string(),
                value: vec![7u8; 32],
            })),
        }),
    )
    .await
    .expect("get object by algorithm-aware digest")
    .into_inner();
    assert_eq!(
        fetched_by_digest.object_id,
        object_uuid().as_bytes().to_vec()
    );

    let copies = pb::catalog_server::Catalog::find_object_copies(
        &service,
        Request::new(pb::FindObjectCopiesRequest {
            key: Some(pb::find_object_copies_request::Key::ContentSha256(vec![
                7u8;
                32
            ])),
        }),
    )
    .await
    .expect("find copies")
    .into_inner();
    assert_eq!(copies.copies.len(), 1);
    assert_eq!(copies.copies[0].tape_uuid, vec![3u8; 16]);
    assert_eq!(copies.copies[0].tape_file_number, 4);
    assert_eq!(copies.copies[0].pool_id, "camera.copy-a");
}

#[test]
fn content_digest_lookup_rejects_ambiguous_or_malformed_keys() {
    let state = populated_state();
    let second_object_id = Uuid::from_u128(0x2222).to_string();
    let mut index = CatalogIndex::open(state.index_path.as_ref()).expect("open test index");
    index
        .upsert_native_object_projection(
            NativeObjectProjectionInput {
                object_id: second_object_id,
                caller_object_id: Some("caller-2".to_string()),
                body_format: "rem-object-v1".to_string(),
                logical_size_bytes: Some(17),
                content_hash: Some(vec![7u8; 32]),
                metadata_hash: None,
                created_at_utc: Some("2026-05-28T13:00:00Z".to_string()),
            },
            &[],
        )
        .expect("project colliding logical object");
    drop(index);

    let ambiguous = find_object_for_key(
        &state,
        Some(pb::get_object_request::Key::ContentSha256(vec![7u8; 32])),
    )
    .expect_err("ambiguous content identity must fail");
    assert_eq!(ambiguous.code(), tonic::Code::FailedPrecondition);
    assert!(ambiguous.message().contains("multiple logical objects"));

    let malformed = find_object_for_key(
        &state,
        Some(pb::get_object_request::Key::ContentSha256(vec![7u8; 31])),
    )
    .expect_err("malformed SHA-256 key must fail");
    assert_eq!(malformed.code(), tonic::Code::InvalidArgument);

    let unsupported = find_object_for_key(
        &state,
        Some(pb::get_object_request::Key::ContentDigest(pb::Digest {
            algorithm: "sha512".to_string(),
            value: vec![7u8; 64],
        })),
    )
    .expect_err("unsupported digest algorithm must fail");
    assert_eq!(unsupported.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn write_session_tape_target_is_wired_and_shape_validated() {
    // The tape-target arm is wired (no longer Unimplemented); its shape
    // validation runs before any catalog or hardware access, so a
    // mount_if_needed=false request fails with InvalidArgument even on a
    // state with no drive pool.
    let service = empty_pool_state().write_session_service();
    let err = pb::write_session_service_server::WriteSessionService::open_write_session(
        &service,
        Request::new(pb::OpenWriteSessionRequest {
            target: Some(pb::open_write_session_request::Target::TapeTarget(
                pb::TapeTarget {
                    tape_uuid: TAPE_UUID.to_vec(),
                    mount_if_needed: false,
                    required_pool_id: "camera.copy-b".to_string(),
                    allow_unpooled: false,
                },
            )),
            body_format: Some("rem-object-v1".to_string()),
            idempotency_key: None,
            recover_session_id: None,
        }),
    )
    .await
    .expect_err("shape validation refuses mount_if_needed=false");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("mount_if_needed"), "{err}");
}

#[tokio::test]
async fn write_session_drive_target_stays_unimplemented() {
    let service = empty_pool_state().write_session_service();
    let err = pb::write_session_service_server::WriteSessionService::open_write_session(
        &service,
        Request::new(pb::OpenWriteSessionRequest {
            target: Some(pb::open_write_session_request::Target::DriveTarget(
                pb::DriveTarget {
                    library_uuid: Vec::new(),
                    drive_element_address: 0x0100,
                    required_pool_id: String::new(),
                },
            )),
            body_format: Some("rem-object-v1".to_string()),
            idempotency_key: None,
            recover_session_id: None,
        }),
    )
    .await
    .expect_err("drive target is out of scope for this slice");
    assert_eq!(err.code(), tonic::Code::Unimplemented);
}

#[test]
fn pool_target_library_uuid_resolves_snapshot_uuid() {
    let service = state_with_library_snapshot("LIB001").write_session_service();
    let target = pb::TapePoolTarget {
        pool_id: "camera.copy-a".to_string(),
        library_uuid: crate::library::library_uuid("LIB001").to_vec(),
        mount_if_needed: true,
    };

    let serial = service
        .library_serial_for_pool_target(&target)
        .expect("library UUID resolves to serial");

    assert_eq!(serial.as_deref(), Some("LIB001"));
}

#[test]
fn pool_target_library_uuid_rejects_legacy_raw_serial_bytes() {
    let service = state_with_library_snapshot("LIB001").write_session_service();
    let target = pb::TapePoolTarget {
        pool_id: "camera.copy-a".to_string(),
        library_uuid: b"LIB001".to_vec(),
        mount_if_needed: true,
    };

    let err = service
        .library_serial_for_pool_target(&target)
        .expect_err("library_uuid is a 16-byte UUID, not a serial string");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn write_session_pool_target_rejects_legacy_raw_library_serial() {
    let service = empty_pool_state().write_session_service();
    let err = pb::write_session_service_server::WriteSessionService::open_write_session(
        &service,
        Request::new(pb::OpenWriteSessionRequest {
            target: Some(pb::open_write_session_request::Target::PoolTarget(
                pb::TapePoolTarget {
                    pool_id: "camera.copy-a".to_string(),
                    library_uuid: b"LIB001".to_vec(),
                    mount_if_needed: true,
                },
            )),
            body_format: Some("rem-object-v1".to_string()),
            idempotency_key: None,
            recover_session_id: None,
        }),
    )
    .await
    .expect_err("legacy raw serial is not a library_uuid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn read_session_read_only_state_returns_unavailable_for_tape_open() {
    let service = populated_state().read_session_service();
    let err = pb::read_session_service_server::ReadSessionService::open_read_session(
        &service,
        Request::new(pb::OpenReadSessionRequest {
            target: Some(pb::open_read_session_request::Target::TapeTarget(
                pb::TapeTarget {
                    tape_uuid: TAPE_UUID.to_vec(),
                    mount_if_needed: true,
                    required_pool_id: "camera.copy-a".to_string(),
                    allow_unpooled: false,
                },
            )),
            idempotency_key: None,
            resume_target: None,
        }),
    )
    .await
    .expect_err("read-only ApiState has no session owner");
    assert_eq!(err.code(), tonic::Code::Unavailable);
}

#[tokio::test]
async fn read_object_range_dispatches_empty_file_id_range_to_drive_actor() {
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
    let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(true))]));
    let pool = crate::write_owner::DrivePool::new_for_library(
        "LIB-A",
        changer_tx,
        HashMap::from([(0x0101, drive_tx)]),
        reservations,
    );
    let session_id = Uuid::new_v4();
    pool.record_session(
        session_id,
        crate::write_owner::MountedSession {
            bay: 0x0101,
            library_serial: "LIB-A".to_string(),
            barcode: Some("AOX001L9".to_string()),
            home_slot: None,
            tape_uuid: TAPE_UUID,
            drive_uuid: Some(Uuid::new_v4().as_bytes().to_vec()),
        },
    );
    let mut state = populated_state();
    state.drive_pool = Some(pool);
    let service = state.read_session_service();

    let drive_task = tokio::spawn(async move {
        let command = drive_rx.recv().await.expect("drive command");
        let crate::write_owner::DriveCommand::ReadObjectRange {
            session_id: got_session_id,
            object_id,
            file_id,
            start_byte,
            end_byte,
            stream_chunk_bytes,
            chunk_tx,
        } = command
        else {
            panic!("expected ReadObjectRange command");
        };
        assert_eq!(got_session_id, session_id);
        assert_eq!(object_id, OBJECT_ID_TEXT);
        assert_eq!(file_id, "");
        assert_eq!(start_byte, 1);
        assert_eq!(end_byte, 5);
        assert_eq!(stream_chunk_bytes, 4);
        chunk_tx
            .send(Ok(pb::BytesChunk {
                data: b"ANGE".to_vec(),
                is_last: false,
            }))
            .await
            .expect("send data chunk");
        chunk_tx
            .send(Ok(pb::BytesChunk {
                data: Vec::new(),
                is_last: true,
            }))
            .await
            .expect("send last chunk");
    });

    let mut stream = pb::read_session_service_server::ReadSessionService::read_object_range(
        &service,
        Request::new(pb::ReadObjectRangeRequest {
            session_id: session_id.as_bytes().to_vec(),
            object_id: object_uuid().as_bytes().to_vec(),
            file_id: Vec::new(),
            start_byte: 1,
            end_byte: 5,
            stream_chunk_bytes: 4,
        }),
    )
    .await
    .expect("range stream")
    .into_inner();

    let mut got = Vec::new();
    let mut saw_last = false;
    while let Some(item) = stream.next().await {
        let chunk = item.expect("chunk");
        got.extend_from_slice(&chunk.data);
        saw_last |= chunk.is_last;
    }
    drive_task.await.expect("drive task");
    assert_eq!(got, b"ANGE");
    assert!(saw_last);
}

#[tokio::test]
async fn read_object_range_empty_file_id_zero_zero_uses_whole_payload_path() {
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
    let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(true))]));
    let pool = crate::write_owner::DrivePool::new_for_library(
        "LIB-A",
        changer_tx,
        HashMap::from([(0x0101, drive_tx)]),
        reservations,
    );
    let session_id = Uuid::new_v4();
    pool.record_session(
        session_id,
        crate::write_owner::MountedSession {
            bay: 0x0101,
            library_serial: "LIB-A".to_string(),
            barcode: Some("AOX001L9".to_string()),
            home_slot: None,
            tape_uuid: TAPE_UUID,
            drive_uuid: Some(Uuid::new_v4().as_bytes().to_vec()),
        },
    );
    let mut state = populated_state();
    state.drive_pool = Some(pool);
    let service = state.read_session_service();

    let drive_task = tokio::spawn(async move {
        let command = drive_rx.recv().await.expect("drive command");
        let crate::write_owner::DriveCommand::ReadFile {
            session_id: got_session_id,
            object_id,
            file_id,
            stream_chunk_bytes,
            chunk_tx,
        } = command
        else {
            panic!("expected ReadFile command");
        };
        assert_eq!(got_session_id, session_id);
        assert_eq!(object_id, OBJECT_ID_TEXT);
        assert!(file_id.is_empty());
        assert_eq!(stream_chunk_bytes, 6);
        chunk_tx
            .send(Ok(pb::BytesChunk {
                data: b"whole payload".to_vec(),
                is_last: false,
            }))
            .await
            .expect("send data chunk");
        chunk_tx
            .send(Ok(pb::BytesChunk {
                data: Vec::new(),
                is_last: true,
            }))
            .await
            .expect("send last chunk");
    });

    let mut stream = pb::read_session_service_server::ReadSessionService::read_object_range(
        &service,
        Request::new(pb::ReadObjectRangeRequest {
            session_id: session_id.as_bytes().to_vec(),
            object_id: object_uuid().as_bytes().to_vec(),
            file_id: Vec::new(),
            start_byte: 0,
            end_byte: 0,
            stream_chunk_bytes: 6,
        }),
    )
    .await
    .expect("whole payload stream")
    .into_inner();

    let mut got = Vec::new();
    let mut saw_last = false;
    while let Some(item) = stream.next().await {
        let chunk = item.expect("chunk");
        got.extend_from_slice(&chunk.data);
        saw_last |= chunk.is_last;
    }
    drive_task.await.expect("drive task");
    assert_eq!(got, b"whole payload");
    assert!(saw_last);
}

#[tokio::test]
async fn read_object_range_dispatches_file_scoped_range_to_drive_actor() {
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
    let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(true))]));
    let pool = crate::write_owner::DrivePool::new_for_library(
        "LIB-A",
        changer_tx,
        HashMap::from([(0x0101, drive_tx)]),
        reservations,
    );
    let session_id = Uuid::new_v4();
    pool.record_session(
        session_id,
        crate::write_owner::MountedSession {
            bay: 0x0101,
            library_serial: "LIB-A".to_string(),
            barcode: Some("AOX001L9".to_string()),
            home_slot: None,
            tape_uuid: TAPE_UUID,
            drive_uuid: Some(Uuid::new_v4().as_bytes().to_vec()),
        },
    );
    let mut state = populated_state();
    state.drive_pool = Some(pool);
    let service = state.read_session_service();

    let drive_task = tokio::spawn(async move {
        let command = drive_rx.recv().await.expect("drive command");
        let crate::write_owner::DriveCommand::ReadObjectRange {
            session_id: got_session_id,
            object_id,
            file_id,
            start_byte,
            end_byte,
            stream_chunk_bytes,
            chunk_tx,
        } = command
        else {
            panic!("expected ReadObjectRange command");
        };
        assert_eq!(got_session_id, session_id);
        assert_eq!(object_id, OBJECT_ID_TEXT);
        assert_eq!(file_id, "file-camera");
        assert_eq!(start_byte, 5);
        assert_eq!(end_byte, 9);
        assert_eq!(stream_chunk_bytes, 3);
        chunk_tx
            .send(Ok(pb::BytesChunk {
                data: b"nge".to_vec(),
                is_last: false,
            }))
            .await
            .expect("send data chunk");
        chunk_tx
            .send(Ok(pb::BytesChunk {
                data: Vec::new(),
                is_last: true,
            }))
            .await
            .expect("send last chunk");
    });

    let mut stream = pb::read_session_service_server::ReadSessionService::read_object_range(
        &service,
        Request::new(pb::ReadObjectRangeRequest {
            session_id: session_id.as_bytes().to_vec(),
            object_id: object_uuid().as_bytes().to_vec(),
            file_id: b"file-camera".to_vec(),
            start_byte: 5,
            end_byte: 9,
            stream_chunk_bytes: 3,
        }),
    )
    .await
    .expect("range stream")
    .into_inner();

    let mut got = Vec::new();
    let mut saw_last = false;
    while let Some(item) = stream.next().await {
        let chunk = item.expect("chunk");
        got.extend_from_slice(&chunk.data);
        saw_last |= chunk.is_last;
    }
    drive_task.await.expect("drive task");
    assert_eq!(got, b"nge");
    assert!(saw_last);
}

#[tokio::test]
async fn read_file_dispatches_file_id_as_whole_file_range() {
    let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
    let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
    let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(true))]));
    let pool = crate::write_owner::DrivePool::new_for_library(
        "LIB-A",
        changer_tx,
        HashMap::from([(0x0101, drive_tx)]),
        reservations,
    );
    let session_id = Uuid::new_v4();
    pool.record_session(
        session_id,
        crate::write_owner::MountedSession {
            bay: 0x0101,
            library_serial: "LIB-A".to_string(),
            barcode: Some("AOX001L9".to_string()),
            home_slot: None,
            tape_uuid: TAPE_UUID,
            drive_uuid: Some(Uuid::new_v4().as_bytes().to_vec()),
        },
    );
    let mut state = populated_state();
    state.drive_pool = Some(pool);
    let service = state.read_session_service();

    let drive_task = tokio::spawn(async move {
        let command = drive_rx.recv().await.expect("drive command");
        let crate::write_owner::DriveCommand::ReadObjectRange {
            file_id,
            start_byte,
            end_byte,
            chunk_tx,
            ..
        } = command
        else {
            panic!("expected ReadObjectRange command");
        };
        assert_eq!(file_id, "file-camera");
        assert_eq!(start_byte, 0);
        assert_eq!(end_byte, 0);
        chunk_tx
            .send(Ok(pb::BytesChunk {
                data: Vec::new(),
                is_last: true,
            }))
            .await
            .expect("send last chunk");
    });

    let mut stream = pb::read_session_service_server::ReadSessionService::read_file(
        &service,
        Request::new(pb::ReadFileRequest {
            session_id: session_id.as_bytes().to_vec(),
            object_id: object_uuid().as_bytes().to_vec(),
            file_id: b"file-camera".to_vec(),
            stream_chunk_bytes: 0,
        }),
    )
    .await
    .expect("read file stream")
    .into_inner();

    assert!(stream.next().await.expect("last").expect("chunk").is_last);
    assert!(stream.next().await.is_none());
    drive_task.await.expect("drive task");
}

#[tokio::test]
async fn catalog_units_are_exposed_as_parallel_surface() {
    let service = populated_state().catalog_service();
    let mut stream = pb::catalog_server::Catalog::enumerate_units(
        &service,
        Request::new(pb::EnumerateUnitsRequest {
            scope: Some(pb::enumerate_units_request::Scope::All(())),
            origin_filter: pb::CatalogUnitOriginFilter::NativeObjects as i32,
            refresh_from_source: false,
        }),
    )
    .await
    .expect("enumerate units")
    .into_inner();

    let unit = stream
        .next()
        .await
        .expect("one unit")
        .expect("catalog unit");
    assert_eq!(
        unit.origin_kind,
        pb::CatalogUnitOriginKind::NativeObject as i32
    );
    assert_eq!(unit.format_id, "rem-object-v1");
    assert!(matches!(
        unit.origin,
        Some(pb::catalog_unit::Origin::Native(
            pb::NativeUnitSummary { .. }
        ))
    ));

    let fetched = pb::catalog_server::Catalog::get_catalog_unit(
        &service,
        Request::new(pb::GetCatalogUnitRequest {
            unit_id: unit.unit_id.clone(),
        }),
    )
    .await
    .expect("get catalog unit")
    .into_inner();
    assert_eq!(fetched.unit_id, unit.unit_id);

    let err = pb::catalog_server::Catalog::list_entries_in_unit(
        &service,
        Request::new(pb::ListEntriesInUnitRequest {
            unit_id: fetched.unit_id,
            page_token: None,
            page_size: 0,
            refresh_from_source: false,
        }),
    )
    .await
    .expect_err("entry listing is deliberately not wired yet");
    assert_eq!(err.code(), tonic::Code::Unimplemented);
}

#[tokio::test]
async fn foreign_unit_uses_registered_adapter_for_normalized_entries() {
    let (_temp, state, unit_id) = foreign_test_state();
    let service = state.catalog_service();

    let entries = pb::catalog_server::Catalog::list_entries_in_unit(
        &service,
        Request::new(pb::ListEntriesInUnitRequest {
            unit_id: unit_id.as_bytes().to_vec(),
            page_token: None,
            page_size: 0,
            refresh_from_source: false,
        }),
    )
    .await
    .expect("list foreign unit entries")
    .into_inner();

    assert_eq!(entries.entries.len(), 1);
    assert_eq!(entries.entries[0].path, "camera/a.txt");
    assert_eq!(
        entries.entries[0].kind,
        pb::CatalogEntryKind::RegularFile as i32
    );
    assert_eq!(entries.entries[0].size_bytes, Some(3));
    assert_eq!(
        entries.entries[0].integrity_basis,
        pb::IntegrityBasis::Unknown as i32
    );
    assert_eq!(entries.archive_gaps.len(), 1);
    assert_eq!(entries.archive_gaps[0].source_start, 8);
    assert_eq!(entries.archive_gaps[0].source_end, 16);
}

#[tokio::test]
async fn foreign_unit_reports_unregistered_format_in_core_distribution() {
    let mut index = test_index();
    let unit_id = index
        .upsert_foreign_archive_projection(ForeignArchiveProjectionInput {
            tape_uuid: Vec::new(),
            format_id: "legacy-example-v1".to_string(),
            scan_id: "scan-example-1".to_string(),
            source_kind: "byte_stream_dump".to_string(),
            source_id: "/nonexistent/fixture.archive".to_string(),
            confidence: "high".to_string(),
            entry_count: 0,
            damage_event_count: 0,
            adapter_state: vec![],
            last_scan_at_utc: Some("2026-05-28T13:15:00Z".to_string()),
            created_at_utc: Some("2026-05-28T13:15:01Z".to_string()),
        })
        .expect("project foreign unit");
    let service = ApiState::new(index).catalog_service();

    let error = pb::catalog_server::Catalog::list_entries_in_unit(
        &service,
        Request::new(pb::ListEntriesInUnitRequest {
            unit_id: unit_id.as_bytes().to_vec(),
            page_token: None,
            page_size: 0,
            refresh_from_source: false,
        }),
    )
    .await
    .expect_err("core distribution has an empty foreign-format registry");

    assert_eq!(error.code(), tonic::Code::Unimplemented);
    assert!(error
        .message()
        .contains("foreign format legacy-example-v1 is not registered in this distribution"));
}
use crate::auth::{authorize_request, AuthPermission};
