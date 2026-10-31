use remanence_parity::authority::StructuralAuthority;
use remanence_state::authority::TapeAuthorityView;
#[path = "durability_tests.rs"]
mod durability_tests;

use super::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration as StdDuration;

use super::checkpoint::*;
use super::cleaning::*;
use super::read_session::*;
use super::readiness::*;
use super::restore::*;
use super::robotics::*;
use super::terminal_inventory::*;
use super::write_session::*;
use crate::{PoolWriteError, FINALIZE_TAPE_OPERATION_KIND};
use ciborium::value::Value as CborValue;
use prost::Message as _;
use remanence_aead::RecipientPrivateKey;
use remanence_chaos::model::{ModelTransport, Record, VirtualTape, VirtualWorld};
use remanence_format::error::FormatError;
use remanence_format::{
    read_encrypted_rem_object_file_range_to_vec, write_encrypted_rem_object, write_rem_tar_object,
    RemTarFile, RemTarObjectLayout, RemTarObjectOptions,
};
use remanence_library::{
    DiscoveryReport, DriveHandle, DriveOpError, LoadError, MediaFamily, MediaReadiness, SpaceKind,
    TapeConfig, TapeIoError,
};
use remanence_library::{
    DriveBay, ElementLayout, FixtureTransport, IdentitySource, InstalledDrive, Library,
    RecordingLog, RecordingTransport, SgTransport, Slot, VecBlockSink, VecBlockSource,
    VecBlockSourceCall, WormMediaState,
};
use remanence_parity::bootstrap::{parse_bootstrap_block, write_bootstrap_block};
use remanence_parity::*;
use remanence_parity::{
    BootstrapPayload, CommittedBundle, CommittedBundleKind, ParityConfig, TapeFileEntry,
    TapeFileKind,
};
use remanence_state::*;
use remanence_state::{
    CatalogIndex, DriveObservationInput, NativeObjectCopyProjectionInput,
    NativeObjectFileProjectionInput, NativeObjectProjectionInput, ProvisionTapeInput,
    TapeFileRecord, TapeJournalIndexInput, TapePoolProjectionInput,
    OBJECT_COPY_REPRESENTATION_PLAINTEXT,
};
use remanence_stream::StreamingError;
use sha2::{Digest, Sha256};
use std::sync::atomic::AtomicU64;
use time::{Duration, OffsetDateTime};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;
use tonic::Status;
use uuid::Uuid;

const RANGE_OBJECT_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
const RANGE_TAPE_UUID: [u8; 16] = [0xAB; 16];

#[test]
fn provisional_replay_enforces_digest_identity_and_input_kind_guards() {
    let object_id = [0x41; 16];
    let digest = [0x42; 32];
    assert!(validate_provisional_replay_guards(
        "canonical-pending",
        crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
        None,
        object_id,
        digest,
        crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
        Path::new("ignored-on-canonical-replay"),
        Some(object_id),
        Some(digest),
        digest,
    )
    .is_ok());

    let wrong_digest = validate_provisional_replay_guards(
        "canonical-pending",
        crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
        None,
        object_id,
        digest,
        crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
        Path::new("ignored-on-canonical-replay"),
        Some(object_id),
        Some([0x43; 32]),
        digest,
    )
    .expect_err("wrong caller digest guard must fail");
    assert_eq!(wrong_digest.code(), tonic::Code::FailedPrecondition);

    let wrong_id = validate_provisional_replay_guards(
        "canonical-pending",
        crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
        None,
        object_id,
        digest,
        crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
        Path::new("ignored-on-canonical-replay"),
        Some([0x44; 16]),
        Some(digest),
        digest,
    )
    .expect_err("wrong object identity guard must fail");
    assert_eq!(wrong_id.code(), tonic::Code::InvalidArgument);

    let wrong_kind = validate_provisional_replay_guards(
        "canonical-pending",
        crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
        None,
        object_id,
        digest,
        crate::WriteObjectInputKind::LogicalFile,
        Path::new("payload.bin"),
        None,
        Some(digest),
        digest,
    )
    .expect_err("logical replay must not conflate a canonical pending object");
    assert_eq!(wrong_kind.code(), tonic::Code::AlreadyExists);

    let wrong_path = validate_provisional_replay_guards(
        "logical-pending",
        crate::WriteObjectInputKind::LogicalFile,
        Some("original.bin"),
        object_id,
        digest,
        crate::WriteObjectInputKind::LogicalFile,
        Path::new("renamed.bin"),
        None,
        Some(digest),
        digest,
    )
    .expect_err("logical replay must preserve its pending member path");
    assert_eq!(wrong_path.code(), tonic::Code::AlreadyExists);
    assert!(wrong_path.message().contains("changed archive path"));

    assert!(validate_provisional_replay_guards(
        "logical-pending",
        crate::WriteObjectInputKind::LogicalFile,
        Some("dir/original.bin"),
        object_id,
        digest,
        crate::WriteObjectInputKind::LogicalFile,
        Path::new("./dir/./original.bin"),
        None,
        Some(digest),
        digest,
    )
    .is_ok());
}

#[test]
fn concurrent_drive_admission_allows_only_one_matching_identity() {
    let coordinator = WriteAdmissionCoordinator::default();
    let start = Arc::new(std::sync::Barrier::new(2));
    let finish = Arc::new(std::sync::Barrier::new(2));
    let object_id = [0x73; 16];
    let mut workers = Vec::new();
    for caller in ["drive-a", "drive-b"] {
        let coordinator = coordinator.clone();
        let start = Arc::clone(&start);
        let finish = Arc::clone(&finish);
        workers.push(std::thread::spawn(move || {
            start.wait();
            let result = coordinator.reserve("pool", caller, Some(object_id));
            let admitted = result.is_ok();
            finish.wait();
            drop(result);
            admitted
        }));
    }
    let outcomes = workers
        .into_iter()
        .map(|worker| worker.join().expect("drive admission worker"))
        .collect::<Vec<_>>();
    assert_eq!(outcomes.iter().filter(|admitted| **admitted).count(), 1);

    let replay = coordinator
        .reserve("pool", "drive-c", Some(object_id))
        .expect("identity becomes available after the checkpoint-held claim drops");
    let same_key = coordinator
        .reserve("pool", "drive-c", Some([0x74; 16]))
        .expect_err("pool/caller replay key is independently exclusive");
    assert_eq!(same_key.code(), tonic::Code::Aborted);
    drop(replay);
}

#[test]
fn journal_durable_projection_failure_quarantines_identity_until_restart() {
    let coordinator = WriteAdmissionCoordinator::default();
    let object_id = [0x75; 16];
    let admission = coordinator
        .reserve("pool", "journal-owner", Some(object_id))
        .expect("first drive owns identity");
    let mut failed_batch = PendingCheckpointBatch::new(StdDuration::from_secs(60));
    failed_batch._write_admissions.push(admission);
    let failure = CheckpointBarrierFailure::after_journal(Status::internal(
        "injected catalog projection failure after journal fsync",
    ));
    assert!(failure.requires_identity_quarantine());
    failed_batch.quarantine_write_admissions_until_restart();
    drop(failed_batch);

    let second_drive = coordinator
        .reserve("pool", "other-caller", Some(object_id))
        .expect_err("durable unprojected UUID must remain quarantined");
    assert_eq!(second_drive.code(), tonic::Code::Aborted);

    let projected_coordinator = WriteAdmissionCoordinator::default();
    let projected_admission = projected_coordinator
        .reserve("pool", "projected-owner", Some([0x76; 16]))
        .expect("projected identity starts reserved");
    let projected_failure = CheckpointBarrierFailure::after_projection(Status::internal(
        "injected post-projection receipt failure",
    ));
    assert!(!projected_failure.requires_identity_quarantine());
    drop(projected_admission);
    projected_coordinator
        .reserve("pool", "new-caller", Some([0x76; 16]))
        .expect("catalog-projected failures release the transient claim");
}

#[test]
fn malformed_canonical_caller_bytes_map_to_invalid_argument() {
    let error = crate::pool_write::canonical_admission_format_error(FormatError::Parse(
        "hostile truncated pax record".to_string(),
    ));
    let status = status_from_pool_write_error(error);
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status
        .message()
        .contains("canonical plaintext REM object is malformed"));
}

#[test]
fn automatic_terminal_preflight_accepts_empty_fresh_authority() {
    const TAPE_UUID: TapeUuid = [0xD3; 16];
    const BLOCK_SIZE: u32 = 256 * 1024;

    let temp = tempfile::Builder::new()
        .prefix("remanence-empty-automatic-preflight")
        .tempdir()
        .expect("create automatic preflight tempdir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite"))
        .expect("open automatic preflight catalog");
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "fresh-open".to_string(),
        tape_uuid: TAPE_UUID,
        block_size: BLOCK_SIZE,
        parity_config: ParityConfig::None,
    };
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(BLOCK_SIZE),
        min_object_size_bytes: 0,
    };
    let checkpoint_dir = temp.path().join("tapes");
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    let audit_append_lock = Arc::new(std::sync::Mutex::new(()));

    assert!(
        !preflight_automatic_terminal_completion(
            &mut index,
            ManualFinalizePreflightConfig {
                mounted_endpoint: None,
                checkpoint_journal_dir: &checkpoint_dir,
                audit_dir: temp.path(),
                audit_fsync: false,
                audit_append_lock: &audit_append_lock,
            },
            &selected,
            &pool_cfg,
        )
        .expect("an empty fresh checkpoint has no terminal work"),
        "empty fresh authority must continue to ordinary Object admission"
    );
    let checkpoint =
        remanence_state::authority::fixtures::AuthorityBuilder::new(checkpoint_dir, TAPE_UUID)
            .build()
            .expect("reopen empty fresh checkpoint");
    assert!(
        checkpoint
            .last()
            .expect("read empty fresh checkpoint")
            .is_none(),
        "preflight must not synthesize checkpoint authority"
    );
    assert!(
        checkpoint
            .terminal_finalization_intent()
            .expect("read empty fresh terminal companion")
            .is_none(),
        "preflight must not synthesize a terminal companion"
    );
}

#[test]
fn tape_reservation_holds_session_guard_through_handoff() {
    const TAPE_UUID: TapeUuid = [0xD4; 16];
    let (changer_tx, _changer_rx) = mpsc::channel(1);
    let pool = DrivePool::new(changer_tx, HashMap::new(), Arc::new(HashMap::new()));
    let sessions = Arc::clone(&pool.sessions);
    let reservation = pool
        .reserve_tape_with_after_insert(TAPE_UUID, |reservations| {
            assert!(reservations.contains(&TAPE_UUID));
            assert!(
                matches!(
                    sessions.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ),
                "session publication lock must remain held after exact-tape insertion"
            );
        })
        .expect("reserve exact tape through guarded handoff");
    assert!(pool
        .sessions
        .lock()
        .expect("session map after handoff")
        .is_empty());
    drop(reservation);
}

#[test]
fn parity_and_checkpoint_terminal_progress_are_exhaustive_bijections() {
    use remanence_state::TerminalFinalizationProgress as State;

    for progress in [
        State::BeforeReplicaA,
        State::AfterReplicaA,
        State::AfterSeparationAb,
        State::AfterReplicaB,
        State::AfterSeparationBc,
        State::AfterReplicaC,
    ] {
        let parity = parity_progress_from_state(progress);
        assert_eq!(state_progress_from_parity(parity), progress);
        assert_eq!(
            usize::from(completed_terminal_component_count(progress)),
            parity.next_component_index().unwrap_or(5),
        );
    }
}

#[test]
fn terminal_inventory_status_distinguishes_media_conflict_transport_and_geometry() {
    let conflict = status_from_terminal_inventory_read_error(
        remanence_parity::TerminalInventoryReadError::TerminalIndexReplicaConflict { count: 2 },
    );
    assert_eq!(conflict.code(), tonic::Code::DataLoss);
    assert!(conflict.message().contains("conflicting replica editions"));

    let selected_replica = status_from_terminal_inventory_read_error(
        remanence_parity::TerminalInventoryReadError::SelectedReplica {
            ordinal: 3,
            source: remanence_parity::TapeIndexReplicaError::DigestMismatch { field: "payload" },
        },
    );
    assert_eq!(selected_replica.code(), tonic::Code::DataLoss);

    let source = status_from_terminal_inventory_read_error(
        remanence_parity::TerminalInventoryReadError::Source {
            operation: "READ",
            message: "transport unavailable".to_string(),
        },
    );
    assert_eq!(source.code(), tonic::Code::Unavailable);

    let geometry = status_from_terminal_inventory_read_error(
        remanence_parity::TerminalInventoryReadError::BlockSize(
            remanence_parity::TerminalTailLayoutError::UnsupportedBlockSize { block_size: 512 },
        ),
    );
    assert_eq!(geometry.code(), tonic::Code::FailedPrecondition);

    let visitor = status_from_terminal_inventory_read_error(
        remanence_parity::TerminalInventoryReadError::StreamVisitor {
            message: "receiver closed".to_string(),
        },
    );
    assert_eq!(visitor.code(), tonic::Code::Cancelled);
}

#[test]
fn terminal_verification_cross_layout_conflict_is_data_loss() {
    let conflict = status_from_terminal_index_verification_error(
        remanence_parity::TerminalIndexVerificationError::ConflictingLayouts { count: 2 },
    );
    assert_eq!(conflict.code(), tonic::Code::DataLoss);

    let editions = status_from_terminal_index_verification_error(
        remanence_parity::TerminalIndexVerificationError::ConflictingReplicaEditions { count: 2 },
    );
    assert_eq!(editions.code(), tonic::Code::DataLoss);

    let source = status_from_terminal_index_verification_error(
        remanence_parity::TerminalIndexVerificationError::Source {
            operation: "READ",
            message: "transport unavailable".to_string(),
        },
    );
    assert_eq!(source.code(), tonic::Code::Unavailable);
}

#[test]
fn proved_worm_tail_with_surviving_replicas_requires_explicit_degraded_acceptance() {
    use remanence_state::TerminalFinalizationProgress as Progress;

    for progress in [
        Progress::AfterReplicaA,
        Progress::AfterSeparationAb,
        Progress::AfterReplicaB,
        Progress::AfterSeparationBc,
    ] {
        assert_eq!(
            terminal_reconciliation_outcome(progress, TerminalComponentReconcileEvidence::TornWorm,),
            TerminalFinalizationOutcome::RecoveryRequired,
            "{progress:?}",
        );
    }
}

#[test]
fn unknown_or_zero_replica_tail_remains_recovery_required() {
    use remanence_state::TerminalFinalizationProgress as Progress;

    assert_eq!(
        terminal_reconciliation_outcome(
            Progress::BeforeReplicaA,
            TerminalComponentReconcileEvidence::TornWorm,
        ),
        TerminalFinalizationOutcome::RecoveryRequired,
    );
    assert_eq!(
        terminal_reconciliation_outcome(
            Progress::AfterReplicaB,
            TerminalComponentReconcileEvidence::Unproved,
        ),
        TerminalFinalizationOutcome::RecoveryRequired,
    );
    assert_eq!(
        terminal_reconciliation_outcome(
            Progress::AfterReplicaC,
            TerminalComponentReconcileEvidence::TornWorm,
        ),
        TerminalFinalizationOutcome::RecoveryRequired,
    );
}

#[test]
fn one_transition_ahead_sink_journal_reconciles_before_media_is_available() {
    terminal_authority_fixture(None);
}

#[test]
fn shadow_component_catalog_authority_reports_fresh_orphan_and_checkpointed_units() {
    for already_present in 0..=2 {
        terminal_authority_fixture(Some(already_present));
    }
}

fn terminal_authority_fixture(receipt_case: Option<usize>) {
    const BLOCK_SIZE: u32 = 256 * 1024;
    const TAPE_UUID: [u8; 16] = [0x8A; 16];

    struct PrefixRows;
    impl remanence_parity::TapeIndexReplicaRecordSource for PrefixRows {
        fn visit_structural_entries(
            &mut self,
            visitor: &mut dyn FnMut(
                &remanence_parity::TapeIndexReplicaMapEntry,
            ) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            visitor(&remanence_parity::TapeIndexReplicaMapEntry {
                tape_file_number: 0,
                kind: remanence_parity::TapeIndexReplicaFileKind::Bootstrap,
                block_count: 1,
                first_parity_data_ordinal: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            })?;
            visitor(&remanence_parity::TapeIndexReplicaMapEntry {
                tape_file_number: 1,
                kind: remanence_parity::TapeIndexReplicaFileKind::Object,
                block_count: 1,
                first_parity_data_ordinal: Some(0),
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            })
        }

        fn visit_object_rows(
            &mut self,
            visitor: &mut dyn FnMut(
                &remanence_parity::TapeIndexReplicaObjectRow,
            ) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            visitor(&remanence_parity::TapeIndexReplicaObjectRow {
                tape_file_number: 1,
                stored_block_count: 1,
                object_id: b"8e8e8e8e-8e8e-8e8e-8e8e-8e8e8e8e8e8e".to_vec(),
                representation: remanence_parity::ObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: 0,
                    manifest_size_bytes: 1,
                    manifest_chunk_count: 1,
                    manifest_sha256: [0x8D; 32],
                },
            })
        }
    }

    let replica_layout = remanence_parity::checked_tape_index_replica_layout(
        BLOCK_SIZE,
        remanence_parity::TapeIndexReplicaCounts {
            structural_entry_count: 2,
            object_row_count: 1,
        },
    )
    .expect("replica layout");
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        BLOCK_SIZE,
        2,
        4,
        replica_layout.replica_record_count,
        remanence_parity::index_separation_records(
            BLOCK_SIZE,
            remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        )
        .expect("default separation records"),
    )
    .expect("terminal layout");
    let mut rows = PrefixRows;
    let edition = remanence_parity::plan_tape_index_edition(
        remanence_parity::TapeIndexEditionDescriptor {
            tape_uuid: TAPE_UUID,
            edition_id: [0x8B; 16],
            edition_sequence: 2,
            scope: remanence_parity::TapeIndexReplicaScope {
                covered_prefix_tape_file_count: 2,
                total_data_ordinals: 1,
                highest_protected_ordinal: 0,
            },
            counts: remanence_parity::TapeIndexReplicaCounts {
                structural_entry_count: 2,
                object_row_count: 1,
            },
            block_size: BLOCK_SIZE,
            compression_enabled: false,
            writer_version: "host-authority-test".to_string(),
            write_timestamp: "2026-08-09T00:00:00Z".to_string(),
            terminal_layout: layout,
        },
        &mut rows,
    )
    .expect("edition plan");
    let plan = TerminalTripleWritePlan::new(edition.clone()).expect("terminal writer plan");
    let intent = remanence_state::TerminalFinalizationIntent {
        tape_uuid: TAPE_UUID,
        trigger: remanence_state::TerminalFinalizationTrigger::ReachedLowWatermark,
        manual: None,
        progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
        recovery_required: false,
        edition_id: edition.descriptor.edition_id,
        edition_sequence: edition.descriptor.edition_sequence,
        edition_digest: edition.edition_digest,
        writer_version: edition.descriptor.writer_version.clone(),
        write_timestamp: edition.descriptor.write_timestamp.clone(),
        terminal_prefix: None,
        layout: remanence_state::TerminalFinalizationLayout::try_from(layout)
            .expect("persist terminal layout"),
    };
    let temp = tempfile::tempdir().expect("tempdir");
    let checkpoint_journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
        temp.path().join("tapes"),
        TAPE_UUID,
    )
    .build()
    .expect("checkpoint journal");
    let object_uuid = Uuid::from_bytes([0x8E; 16]);
    checkpoint_journal
        .append(&remanence_state::CheckpointJournalRecord {
            ordinal: 1,
            committed_object_count: 1,
            eod_partition: 0,
            eod_lba: 4,
            tape_uuid: TAPE_UUID,
            batch_id: [0x8C; 16],
            next_tape_file_number: 2,
            block_size: BLOCK_SIZE,
            objects: vec![remanence_state::CheckpointObjectProjection {
                object: NativeObjectProjectionInput {
                    object_id: object_uuid.to_string(),
                    caller_object_id: Some("host-authority-object".to_string()),
                    body_format: "rem-object-v1".to_string(),
                    logical_size_bytes: Some(1),
                    content_hash: Some(vec![0x8F; 32]),
                    metadata_hash: Some(vec![0x90; 32]),
                    created_at_utc: Some("2026-08-09T00:00:00Z".to_string()),
                },
                files: Vec::new(),
                copy: NativeObjectCopyProjectionInput {
                    object_id: object_uuid.to_string(),
                    tape_uuid: TAPE_UUID,
                    tape_file_number: 1,
                    first_body_lba: 2,
                    first_parity_data_ordinal: None,
                    protected_until_ordinal: None,
                    status: "committed".to_string(),
                    representation: "plaintext".to_string(),
                    recipient_epoch_ids: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0x91; 32]),
                    stored_digest: Some(vec![0x91; 32]),
                },
                block_size: BLOCK_SIZE,
                block_count: 1,
                fresh_tape: true,
                total_committed_ordinals: 1,
                object_recovery_row: remanence_state::CheckpointObjectRecoveryRow {
                    tape_file_number: 1,
                    stored_block_count: 1,
                    object_id: b"8e8e8e8e-8e8e-8e8e-8e8e-8e8e8e8e8e8e".to_vec(),
                    representation:
                        remanence_state::CheckpointObjectRecoveryRepresentation::Plaintext {
                            manifest_first_chunk_lba: 0,
                            manifest_size_bytes: 1,
                            manifest_chunk_count: 1,
                            manifest_sha256: [0x8D; 32],
                        },
                },
            }],
            scheme: None,
            object_tape_file_bundles: Vec::new(),
            barrier_bundle: None,
            terminal_finalization: None,
            sealed_after_write: false,
            seal_due: None,
        })
        .expect("append base checkpoint");
    let mut checkpoint = checkpoint_journal
        .acquire_exclusive()
        .expect("checkpoint lease");
    checkpoint
        .begin_terminal_finalization(&intent)
        .expect("publish terminal intent");

    let mut index = CatalogIndex::open(temp.path().join("state.sqlite")).expect("open catalog");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: TAPE_UUID,
            voltag: "AUTH01L9".to_string(),
            block_size: BLOCK_SIZE,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    project_checkpoint_authority_bounded(&mut index, &checkpoint)
        .expect("project base checkpoint authority");
    index
        .project_terminal_finalization(TerminalFinalizationProjectionInput {
            tape_uuid: TAPE_UUID,
            trigger: intent.trigger,
            operation_id: None,
            progress: intent.progress,
            edition_digest: intent.edition_digest,
            layout_digest: intent.layout.layout_digest,
            outcome: TerminalFinalizationOutcome::InProgress,
            updated_at_utc: None,
        })
        .expect("project initial finalization");

    let scheme = remanence_parity::default_scheme();
    let mut journal = remanence_parity::authority::fixtures::RowAuthorityBuilder::new(
        temp.path(),
        TAPE_UUID,
        BLOCK_SIZE,
        scheme.clone(),
    )
    .build()
    .expect("open sink journal");
    let bot_bundle = CommittedBundle {
        kind: CommittedBundleKind::BotBootstrap,
        entries: vec![TapeFileEntry {
            tape_file_number: 0,
            kind: TapeFileKind::Bootstrap,
            block_count: 1,
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
        total_committed_ordinals: 0,
    };
    let object_bundle = CommittedBundle {
        kind: CommittedBundleKind::Object,
        entries: vec![TapeFileEntry {
            tape_file_number: 1,
            kind: TapeFileKind::Object,
            block_count: 1,
            physical_start_hint: Some(2),
            object_id: Some(object_uuid.to_string()),
            first_parity_data_ordinal: Some(0),
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            object_recovery_row: Some(remanence_parity::ObjectRecoveryRow {
                tape_file_number: 1,
                stored_block_count: 1,
                object_id: Some(b"8e8e8e8e-8e8e-8e8e-8e8e-8e8e8e8e8e8e".to_vec()),
                representation: remanence_parity::ObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: 0,
                    manifest_size_bytes: 1,
                    manifest_chunk_count: 1,
                    manifest_sha256: [0x8D; 32],
                },
            }),
        }],
        highest_protected_ordinal: 0,
        total_committed_ordinals: 1,
    };
    let object_checkpoint = CommittedBundle {
        kind: CommittedBundleKind::CheckpointedThrough,
        entries: Vec::new(),
        highest_protected_ordinal: 0,
        total_committed_ordinals: 1,
    };
    let sink_checkpoint = CommittedBundle {
        kind: CommittedBundleKind::CheckpointedThrough,
        entries: Vec::new(),
        highest_protected_ordinal: 0,
        total_committed_ordinals: 1,
    };
    journal
        .publish_fixture(&remanence_parity::authority::ShadowPublication {
            bundles: vec![(bot_bundle).clone()],
        })
        .expect("journal BOT");
    journal
        .publish_fixture(&remanence_parity::authority::ShadowPublication {
            bundles: vec![(object_bundle).clone()],
        })
        .expect("journal Object");
    journal
        .publish_fixture(&remanence_parity::authority::ShadowPublication {
            bundles: vec![(object_checkpoint).clone()],
        })
        .expect("journal Object checkpoint");
    journal
        .publish_fixture(&remanence_parity::authority::ShadowPublication {
            bundles: vec![
                (CommittedBundle {
                    kind: CommittedBundleKind::TerminalPrefix,
                    entries: Vec::new(),
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 1,
                })
                .clone(),
                (sink_checkpoint).clone(),
            ],
        })
        .expect("journal terminal prefix");
    if let Some(already_present) = receipt_case {
        use remanence_parity::authority::PublicationReceipt;
        use remanence_parity::{
            TerminalComponentCommit, TerminalTailAuthority, TerminalTailProgress,
        };
        let spec = TerminalFinalizeSpec {
            actor: AuditActor::System,
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            pool_config: None,
            trigger: intent.trigger,
            operation_id: None,
            manual: None,
        };
        let mut authority = super::terminal_types::TerminalTailCatalogAuthority {
            checkpoint: &mut checkpoint,
            index: &mut index,
            spec: &spec,
            intent: (&intent).into(),
            tix_fault: None,
            reconciliation: None,
            durable_projection_pending: false,
        };
        let mut progress = TerminalTailProgress::BeforeReplicaA;
        for component in layout.components {
            let bundle = remanence_parity::terminal_component_bundle(&plan, component).unwrap();
            let before = authority.checkpoint.freeze().unwrap().log_reference().0;
            let next = progress.successor().unwrap();
            let mut commit = TerminalComponentCommit {
                previous_progress: progress,
                next_progress: next,
                component,
                observed_position: remanence_parity::PhysicalPositionHint::new(
                    component.planned_start_lba + component.record_count + 1,
                ),
                reconciled_existing_component: already_present != 0,
                rewrote_torn_component: false,
                journal_bundle: bundle,
                checkpoint_bundle: sink_checkpoint.clone(),
                publication: PublicationReceipt::default(),
            };
            authority.commit_after_barrier(&mut commit).unwrap();
            assert_eq!(authority.load_progress().unwrap(), next);
            let after = authority.checkpoint.freeze().unwrap().log_reference().0;
            assert!(after > before, "one native group closes the component");
            assert_eq!(
                authority
                    .index
                    .projected_through_sequence(TAPE_UUID)
                    .unwrap(),
                after
            );
            assert!(
                commit.publication.items.is_empty(),
                "the sink no longer publishes host units"
            );
            progress = next;
        }
        return;
    }
    let replica_a = remanence_parity::terminal_component_bundle(&plan, layout.components[0])
        .expect("replica A bundle");
    journal
        .publish_fixture(&remanence_parity::authority::ShadowPublication {
            bundles: vec![(replica_a).clone()],
        })
        .expect("simulate crash after sink component fsync");

    let spec = TerminalFinalizeSpec {
        actor: AuditActor::System,
        tape_uuid: TAPE_UUID,
        block_size: BLOCK_SIZE,
        pool_config: None,
        trigger: intent.trigger,
        operation_id: None,
        manual: None,
    };
    let reconciled = reconcile_terminal_component_host_authority(
        &mut index,
        &mut checkpoint,
        &spec,
        (&intent).into(),
        &plan,
        &mut journal,
    )
    .expect("host-only one-transition reconciliation");
    assert_eq!(
        reconciled.progress,
        remanence_state::TerminalFinalizationProgress::AfterReplicaA
    );
    let transitions = layout
        .components
        .iter()
        .map(|component| {
            let bundle = remanence_parity::terminal_component_bundle(&plan, *component)
                .expect("component bundle");
            (bundle, sink_checkpoint.clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        journal
            .component_relation(
                1,
                &transitions
                    .iter()
                    .map(
                        |(unit, _)| remanence_parity::authority::FilePublication::try_from(unit)
                            .unwrap()
                    )
                    .collect::<Vec<_>>()
            )
            .expect("host authorities aligned after reconciliation"),
        remanence_parity::TerminalComponentAuthorityRelation::Aligned
    );
    assert_eq!(
        checkpoint
            .terminal_finalization_intent()
            .expect("read durable progress")
            .expect("pending intent")
            .progress,
        remanence_state::TerminalFinalizationProgress::AfterReplicaA
    );
    assert_eq!(
        index
            .terminal_finalization(&TAPE_UUID)
            .expect("read projection")
            .expect("finalization projection")
            .progress,
        remanence_state::TerminalFinalizationProgress::AfterReplicaA
    );

    // Simulate the separate crash window after the next external
    // checkpoint fsync but before its SQLite projection. Both durable
    // journals agree; restart must repair the cache before media motion.
    journal
        .publish_fixture(&remanence_parity::authority::ShadowPublication {
            bundles: vec![(transitions[1].0).clone(), (transitions[1].1).clone()],
        })
        .expect("journal separation AB transition");
    let checkpoint_intent = checkpoint
        .advance_terminal_finalization(
            remanence_state::TerminalFinalizationProgress::AfterReplicaA,
            remanence_state::TerminalFinalizationProgress::AfterSeparationAb,
        )
        .expect("advance external checkpoint without SQLite projection");
    assert_eq!(
        index
            .terminal_finalization(&TAPE_UUID)
            .expect("read deliberately stale projection")
            .expect("stale finalization projection")
            .progress,
        remanence_state::TerminalFinalizationProgress::AfterReplicaA
    );
    let reconciled = reconcile_terminal_component_host_authority(
        &mut index,
        &mut checkpoint,
        &spec,
        (&checkpoint_intent).into(),
        &plan,
        &mut journal,
    )
    .expect("aligned journals repair SQLite before media");
    assert_eq!(
        reconciled.progress,
        remanence_state::TerminalFinalizationProgress::AfterSeparationAb
    );
    assert_eq!(
        index
            .terminal_finalization(&TAPE_UUID)
            .expect("read repaired projection")
            .expect("repaired finalization projection")
            .progress,
        remanence_state::TerminalFinalizationProgress::AfterSeparationAb
    );

    let mut current = reconciled;
    for (component_index, expected_progress) in [
        (
            2,
            remanence_state::TerminalFinalizationProgress::AfterReplicaB,
        ),
        (
            3,
            remanence_state::TerminalFinalizationProgress::AfterSeparationBc,
        ),
    ] {
        journal
            .publish_fixture(&remanence_parity::authority::ShadowPublication {
                bundles: vec![(transitions[component_index].0).clone()],
            })
            .expect("simulate the next sink component one transition ahead");
        current = reconcile_terminal_component_host_authority(
            &mut index,
            &mut checkpoint,
            &spec,
            current,
            &plan,
            &mut journal,
        )
        .expect("promote one exact sink transition before media");
        assert_eq!(current.progress, expected_progress);
        assert_eq!(
            index
                .terminal_finalization(&TAPE_UUID)
                .expect("read promoted projection")
                .expect("promoted finalization projection")
                .progress,
            expected_progress
        );
    }

    // Replica C may be barrier-proved in the sink journal one host fsync
    // before checkpoint progress. A newly lowered cap would reject the
    // stale pre-C view, so recovery must reconcile first and then observe
    // that no tape capacity remains to authorize.
    journal
        .publish_fixture(&remanence_parity::authority::ShadowPublication {
            bundles: vec![(transitions[4].0).clone(), (transitions[4].1).clone()],
        })
        .expect("journal replica C transition");
    let lowered_pool = TapePoolConfig {
        scheme: None,
        id: "host-authority-test".to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: Some(u64::from(BLOCK_SIZE)),
        block_size_bytes: u64::from(BLOCK_SIZE),
        min_object_size_bytes: 0,
    };
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: lowered_pool.id.clone(),
        tape_uuid: TAPE_UUID,
        block_size: BLOCK_SIZE,
        parity_config: ParityConfig::Scheme(scheme),
    };
    let lowered_spec = TerminalFinalizeSpec {
        actor: AuditActor::System,
        tape_uuid: TAPE_UUID,
        block_size: BLOCK_SIZE,
        pool_config: Some(lowered_pool),
        trigger: current.trigger,
        operation_id: None,
        manual: None,
    };
    assert!(
        authorize_terminal_intent_capacity(
            &index,
            &lowered_spec,
            &selected,
            &current,
            plan.edition.descriptor.counts,
        )
        .is_err(),
        "the deliberately stale pre-C checkpoint view must still see the lowered cap"
    );
    assert_eq!(
        index
            .terminal_finalization(&TAPE_UUID)
            .expect("read stale pre-C projection")
            .expect("stale pre-C finalization projection")
            .progress,
        remanence_state::TerminalFinalizationProgress::AfterSeparationBc
    );
    let completed = reconcile_and_authorize_parity_resume(
        &mut index,
        &mut checkpoint,
        &lowered_spec,
        &selected,
        current,
        &plan,
        &mut journal,
    )
    .expect("replica-C sink proof must reconcile before changed-cap authorization");
    assert_eq!(
        completed.progress,
        remanence_state::TerminalFinalizationProgress::AfterReplicaC
    );
    let projected = index
        .terminal_finalization(&TAPE_UUID)
        .expect("read repaired replica C projection")
        .expect("replica C finalization projection");
    assert_eq!(
        projected.progress,
        remanence_state::TerminalFinalizationProgress::AfterReplicaC
    );
    assert_eq!(projected.outcome, TerminalFinalizationOutcome::InProgress);

    let completed = checkpoint
        .mark_terminal_recovery_required()
        .expect("persist post-C recovery classification");
    index
        .project_terminal_finalization(TerminalFinalizationProjectionInput {
            tape_uuid: TAPE_UUID,
            trigger: completed.trigger,
            operation_id: None,
            progress: completed.progress,
            edition_digest: completed.edition_digest,
            layout_digest: completed.layout.layout_digest,
            outcome: TerminalFinalizationOutcome::RecoveryRequired,
            updated_at_utc: None,
        })
        .expect("project post-C recovery classification");
    drop(checkpoint);
    let audit_append_lock = Arc::new(std::sync::Mutex::new(()));
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "host-authority-test".to_string(),
        tape_uuid: TAPE_UUID,
        block_size: BLOCK_SIZE,
        parity_config: ParityConfig::None,
    };
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(BLOCK_SIZE),
        min_object_size_bytes: 0,
    };
    assert!(preflight_automatic_terminal_completion(
        &mut index,
        ManualFinalizePreflightConfig {
            mounted_endpoint: None,
            checkpoint_journal_dir: &temp.path().join("tapes"),
            audit_dir: temp.path(),
            audit_fsync: false,
            audit_append_lock: &audit_append_lock,
        },
        &selected,
        &pool_cfg,
    )
    .expect("automatic entry completes final checkpoint without media capability"));
    let finalized = index
        .terminal_finalization(&TAPE_UUID)
        .expect("read finalized projection")
        .expect("finalized projection");
    assert_eq!(finalized.outcome, TerminalFinalizationOutcome::Finalized);
    let checkpoint_journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
        temp.path().join("tapes"),
        TAPE_UUID,
    )
    .build()
    .expect("reopen finalized checkpoint journal");
    assert!(
        checkpoint_journal
            .last()
            .expect("read final checkpoint")
            .expect("final record")
            .sealed_after_write
    );
}

#[test]
fn all_invalid_terminal_inventory_projects_explicit_bot_recovery() {
    let replicas = std::array::from_fn(|_| {
        remanence_parity::TerminalReplicaEvidence::Invalid(
            remanence_parity::TerminalReplicaFailure {
                kind: remanence_parity::TerminalReplicaFailureKind::Missing,
                detail: "test member missing".to_string(),
            },
        )
    });
    let projected = terminal_inventory_to_proto(
        RANGE_TAPE_UUID,
        remanence_parity::TerminalInventoryOutcome::BotStructuralRecoveryRequired(Box::new(
            remanence_parity::BotStructuralRecoveryRequired {
                reason: remanence_parity::BotStructuralRecoveryReason::AllMembersInvalid,
                replicas,
            },
        )),
    );
    assert_eq!(
        projected.outcome,
        pb::TapeInventoryOutcome::BotStructuralRecoveryRequired as i32
    );
    // No scan has run, so every summary field the outcome lacks is absent,
    // never a zero that reads as an empty tape.
    assert_eq!(projected.selected_replica_ordinal, None);
    assert_eq!(projected.selected_attempt_id, None);
    assert_eq!(projected.structural_entry_count, None);
    assert_eq!(projected.object_row_count, None);
    assert_eq!(projected.edition_digest, None);
    assert_eq!(projected.layout_digest, None);
    assert_eq!(projected.payload_digest, None);
    assert_eq!(projected.canonical_map_digest, None);
    assert_eq!(projected.recovered_object_count, None);
    assert_eq!(projected.unknown_object_count, None);
    assert_eq!(projected.incomplete_object_count, None);
    assert_eq!(projected.damaged_region_count, None);
    assert_eq!(projected.replica_health.len(), 3);
    assert!(projected.detail.contains("structural recovery from BOT"));
}

/// Build the edition facts the converters read. The converters never validate
/// an edition, so only the shape has to be real.
fn converter_edition_plan(counts: TapeIndexReplicaCounts) -> TapeIndexEditionPlan {
    const TERMINAL_BLOCK_SIZE: u32 = 256 * 1024;
    let replica_layout =
        checked_tape_index_replica_layout(TERMINAL_BLOCK_SIZE, counts).expect("replica layout");
    let terminal_layout = TerminalTailLayout::new(
        0,
        TERMINAL_BLOCK_SIZE,
        counts.structural_entry_count,
        4,
        replica_layout.replica_record_count,
        3,
    )
    .expect("tail layout");
    TapeIndexEditionPlan {
        descriptor: TapeIndexEditionDescriptor {
            tape_uuid: RANGE_TAPE_UUID,
            edition_id: [0x81; 16],
            edition_sequence: 1,
            scope: TapeIndexReplicaScope {
                covered_prefix_tape_file_count: counts.structural_entry_count,
                total_data_ordinals: counts.object_row_count,
                highest_protected_ordinal: 0,
            },
            counts,
            block_size: TERMINAL_BLOCK_SIZE,
            compression_enabled: false,
            writer_version: "presence-converter-test".to_string(),
            write_timestamp: "2026-09-25T00:00:00Z".to_string(),
            terminal_layout,
        },
        replica_layout,
        payload_sha256: [0x51; 32],
        canonical_map_sha256: [0x52; 32],
        edition_digest: [0x53; 32],
        layout_digest: [0x54; 32],
    }
}

fn converter_payload_summary(counts: TapeIndexReplicaCounts) -> TapeIndexReplicaPayloadSummary {
    TapeIndexReplicaPayloadSummary {
        structural_entry_count: counts.structural_entry_count,
        object_row_count: counts.object_row_count,
        payload_sha256: [0x51; 32],
        canonical_map_sha256: [0x52; 32],
        covered_prefix_end_lba: 3,
    }
}

#[test]
fn complete_inventory_with_no_objects_reports_a_present_zero_and_no_bot_counts() {
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: 1,
        object_row_count: 0,
    };
    let payload = converter_payload_summary(counts);
    let projected = terminal_inventory_to_proto(
        RANGE_TAPE_UUID,
        TerminalInventoryOutcome::Inventory(Box::new(TerminalInventorySelection {
            selected_attempt_id: 1,
            selected_replica_ordinal: 3,
            edition: converter_edition_plan(counts),
            payload,
            replicas: std::array::from_fn(|_| TerminalReplicaEvidence::Valid { summary: payload }),
        })),
    );

    assert_eq!(projected.outcome, pb::TapeInventoryOutcome::Complete as i32);
    // A tape with no Objects has a real Object count of zero.
    assert_eq!(projected.object_row_count, Some(0));
    assert_eq!(projected.structural_entry_count, Some(1));
    assert_eq!(projected.selected_replica_ordinal, Some(3));
    assert_eq!(projected.selected_attempt_id, Some(1));
    assert_eq!(
        projected.edition_digest.as_deref(),
        Some([0x53; 32].as_slice())
    );
    assert_eq!(
        projected.layout_digest.as_deref(),
        Some([0x54; 32].as_slice())
    );
    assert_eq!(
        projected.payload_digest.as_deref(),
        Some([0x51; 32].as_slice())
    );
    assert_eq!(
        projected.canonical_map_digest.as_deref(),
        Some([0x52; 32].as_slice())
    );
    // The fast path runs no BOT classification, so those counts are absent.
    assert_eq!(projected.recovered_object_count, None);
    assert_eq!(projected.unknown_object_count, None);
    assert_eq!(projected.incomplete_object_count, None);
    assert_eq!(projected.damaged_region_count, None);
}

#[test]
fn bot_recovered_summary_reports_present_zero_counts_and_no_terminal_selection() {
    let projected = bot_structural_recovery_to_proto(
        RANGE_TAPE_UUID,
        BotStructuralRecoverySummary {
            bootstrap_recovery_hints: None,
            structural_entry_count: 3,
            complete_object_count: 1,
            recovered_object_count: 1,
            unknown_object_count: 0,
            incomplete_object_count: 0,
            canonical_map_digest: [0x44; 32],
            damaged_region_count: 0,
        },
    );

    assert_eq!(
        projected.outcome,
        pb::TapeInventoryOutcome::BotStructuralRecovered as i32
    );
    // No torn file: a real zero, present on the wire.
    assert_eq!(projected.incomplete_object_count, Some(0));
    assert_eq!(projected.unknown_object_count, Some(0));
    assert_eq!(projected.damaged_region_count, Some(0));
    assert_eq!(projected.recovered_object_count, Some(1));
    assert_eq!(projected.structural_entry_count, Some(3));
    assert_eq!(projected.object_row_count, Some(1));
    assert_eq!(
        projected.canonical_map_digest.as_deref(),
        Some([0x44; 32].as_slice())
    );
    // No terminal member was selected and there is no terminal edition.
    assert_eq!(projected.selected_replica_ordinal, None);
    assert_eq!(projected.selected_attempt_id, None);
    assert_eq!(projected.edition_digest, None);
    assert_eq!(projected.layout_digest, None);
    assert_eq!(projected.payload_digest, None);
}

#[test]
fn separation_health_keeps_a_two_record_zero_and_omits_an_invalid_count() {
    let rows = terminal_separation_health(&[
        TerminalSeparationEvidence::Valid {
            interior_record_count: 0,
        },
        TerminalSeparationEvidence::Invalid {
            detail: "footer missing".to_string(),
        },
    ]);

    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].state,
        pb::tape_index_separation_health::State::TapeIndexSeparationStateValid as i32
    );
    assert_eq!(rows[0].verified_interior_record_count, Some(0));
    assert_eq!(
        rows[1].state,
        pb::tape_index_separation_health::State::TapeIndexSeparationStateInvalid as i32
    );
    assert_eq!(rows[1].verified_interior_record_count, None);
}

#[test]
fn verification_names_every_protected_content_failure_and_is_never_complete() {
    use remanence_parity::{
        BlockFailureReason, FailedDataBlock, FailedParityShard, ParityMapFinding,
        ProtectedContentVerification, SidecarComponentState, SidecarVerification, TapeFilePosition,
    };
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: 2,
        object_row_count: 1,
    };
    let payload = converter_payload_summary(counts);
    let sidecar = SidecarVerification {
        tape_file_number: 2,
        epoch_id: 0,
        footer: SidecarComponentState::Unreadable,
        primary: SidecarComponentState::Valid,
        tail: SidecarComponentState::Valid,
        copies_diverge: false,
        metadata_health: None,
        checked_against_index: true,
        findings: vec![],
        failed_data_blocks: vec![FailedDataBlock {
            position: TapeFilePosition {
                tape_file_number: 1,
                block_within_file: 2,
            },
            ordinal: 2,
            physical: PhysicalPositionHint::new(4),
            reason: BlockFailureReason::CrcMismatch,
        }],
        failed_parity_shards: vec![FailedParityShard {
            epoch_id: 0,
            stripe_index: 1,
            parity_index: 0,
            sidecar_tape_file_number: 2,
            sidecar_block: 2,
            physical: PhysicalPositionHint::new(9),
            reason: BlockFailureReason::Unreadable,
        }],
    };
    let protected = ProtectedContentVerification {
        unprotected_ordinals: None,
        sidecars: vec![sidecar],
        parity_map_findings: vec![ParityMapFinding {
            tape_file_number: 3,
            detail: "the tail ParityMap copy is not usable".to_string(),
        }],
        not_performed: None,
        prefix_damage: vec![
            "UnreadableTapeFileHead at LBA 2".to_string(),
            "bootstrap at tape_file 0 has nonzero trailing fill (REM-PARITY 8.1)".to_string(),
        ],
    };
    let verified = TerminalIndexVerification {
        edition: converter_edition_plan(counts),
        selected_payload: payload,
        replicas: std::array::from_fn(|_| TerminalReplicaEvidence::Valid { summary: payload }),
        separations: std::array::from_fn(|_| TerminalSeparationEvidence::Valid {
            interior_record_count: 1,
        }),
        measured_eod: PhysicalPositionHint::new(40),
        verified_prefix_tape_file_count: 2,
        verified_prefix_record_count: 3,
        measured_tape_file_count: 7,
        protected: protected.clone(),
    };
    let projected = terminal_verification_to_proto(
        RANGE_TAPE_UUID,
        TerminalIndexVerificationOutcome::VerifiedComplete(Box::new(verified)),
    );
    assert_ne!(
        projected.state,
        pb::TapeIndexVerificationState::VerifiedComplete as i32
    );
    let addresses: Vec<_> = projected
        .protected_content_findings
        .iter()
        .map(|f| (f.kind, f.address.as_deref()))
        .collect();
    use pb::ProtectedContentFindingKind as Kind;
    assert!(addresses.contains(&(Kind::DataBlock as i32, Some("tape_file 1 block 2"))));
    assert!(addresses.contains(&(Kind::ParityShard as i32, Some("epoch 0 stripe 1 parity 0"))));
    // Prefix damage has no address, and every finding states its reason.
    assert!(addresses.contains(&(Kind::PrefixDamage as i32, None)));
    assert!(projected.protected_content_findings.iter().any(|f| f.kind
        == Kind::PrefixDamage as i32
        && f.address.is_none()
        && f.detail.as_deref()
            == Some("bootstrap at tape_file 0 has nonzero trailing fill (REM-PARITY 8.1)")));
    // A ParityMap copy that was not used is a finding at its tape file.
    assert!(addresses.contains(&(Kind::ParityMap as i32, Some("tape_file 3"))));
    assert!(projected
        .protected_content_findings
        .iter()
        .all(|f| f.detail.is_some()));
    assert!(projected
        .protected_content_findings
        .iter()
        .any(|f| f.detail.as_deref() == Some("CRC mismatch")));
    // The recovery-required route carries the same pass.
    let recovery = remanence_parity::TerminalIndexRecoveryRequired {
        measured_eod: PhysicalPositionHint::new(9),
        bot_recovery: remanence_parity::BotStructuralRecoverySummary {
            bootstrap_recovery_hints: None,
            structural_entry_count: 3,
            complete_object_count: 1,
            recovered_object_count: 0,
            unknown_object_count: 1,
            incomplete_object_count: 0,
            canonical_map_digest: [0x44; 32],
            damaged_region_count: 0,
        },
        replicas: std::array::from_fn(|_| {
            TerminalReplicaEvidence::Invalid(TerminalReplicaFailure {
                kind: TerminalReplicaFailureKind::Missing,
                detail: "test".to_string(),
            })
        }),
        detail: "no survivor".to_string(),
        protected,
    };
    let projected = terminal_verification_to_proto(
        RANGE_TAPE_UUID,
        TerminalIndexVerificationOutcome::RecoveryRequired(Box::new(recovery)),
    );
    assert!(projected
        .protected_content_findings
        .iter()
        .any(|f| f.kind == Kind::DataBlock as i32
            && f.address.as_deref() == Some("tape_file 1 block 2")));
}

#[test]
fn verified_index_reports_present_prefix_counts_and_digests() {
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: 2,
        object_row_count: 1,
    };
    let payload = converter_payload_summary(counts);
    let projected = terminal_verification_to_proto(
        RANGE_TAPE_UUID,
        TerminalIndexVerificationOutcome::VerifiedComplete(Box::new(TerminalIndexVerification {
            edition: converter_edition_plan(counts),
            selected_payload: payload,
            replicas: std::array::from_fn(|_| TerminalReplicaEvidence::Valid { summary: payload }),
            separations: std::array::from_fn(|_| TerminalSeparationEvidence::Valid {
                interior_record_count: 1,
            }),
            measured_eod: PhysicalPositionHint::new(40),
            verified_prefix_tape_file_count: 2,
            verified_prefix_record_count: 3,
            measured_tape_file_count: 7,
            protected: remanence_parity::ProtectedContentVerification::none_protected(),
        })),
    );

    assert_eq!(
        projected.state,
        pb::TapeIndexVerificationState::VerifiedComplete as i32
    );
    assert_eq!(projected.verified_prefix_tape_file_count, Some(2));
    assert_eq!(projected.verified_prefix_record_count, Some(3));
    assert_eq!(
        projected.edition_digest.as_deref(),
        Some([0x53; 32].as_slice())
    );
    assert_eq!(
        projected.layout_digest.as_deref(),
        Some([0x54; 32].as_slice())
    );
    assert_eq!(
        projected.payload_digest.as_deref(),
        Some([0x51; 32].as_slice())
    );
    assert_eq!(
        projected.canonical_map_digest.as_deref(),
        Some([0x52; 32].as_slice())
    );
    assert!(projected
        .separation_health
        .iter()
        .all(|row| row.verified_interior_record_count == Some(1)));
}

#[test]
fn bot_object_stream_item_keeps_an_unmeasured_torn_count_absent() {
    let block_count = |object: &BotRecoveredObject| match bot_recovered_object_to_proto(object).item
    {
        Some(pb::tape_inventory_stream_item::Item::BotObject(row)) => row.stored_block_count,
        other => panic!("expected a BOT Object row, got {other:?}"),
    };
    let torn = BotRecoveredObject {
        tape_file_number: 5,
        stored_block_count: None,
        object_id: None,
        state: BotRecoveredObjectState::Incomplete,
    };
    let complete = BotRecoveredObject {
        tape_file_number: 4,
        stored_block_count: Some(9),
        object_id: None,
        state: BotRecoveredObjectState::Unknown,
    };

    assert_eq!(block_count(&torn), None);
    assert_eq!(block_count(&complete), Some(9));
}

#[test]
fn recovery_required_verification_projects_measured_bot_evidence() {
    let replicas = std::array::from_fn(|_| {
        remanence_parity::TerminalReplicaEvidence::Invalid(
            remanence_parity::TerminalReplicaFailure {
                kind: remanence_parity::TerminalReplicaFailureKind::PayloadInvalid,
                detail: "test payload invalid".to_string(),
            },
        )
    });
    let projected = terminal_verification_to_proto(
        RANGE_TAPE_UUID,
        remanence_parity::TerminalIndexVerificationOutcome::RecoveryRequired(Box::new(
            remanence_parity::TerminalIndexRecoveryRequired {
                measured_eod: remanence_parity::PhysicalPositionHint::new(123),
                bot_recovery: remanence_parity::BotStructuralRecoverySummary {
                    bootstrap_recovery_hints: None,
                    structural_entry_count: 7,
                    complete_object_count: 4,
                    recovered_object_count: 2,
                    unknown_object_count: 2,
                    incomplete_object_count: 1,
                    canonical_map_digest: [0x44; 32],
                    damaged_region_count: 1,
                },
                replicas,
                detail: "no canonical survivor".to_string(),
                protected: remanence_parity::ProtectedContentVerification::none_protected(),
            },
        )),
    );

    assert_eq!(
        projected.state,
        pb::TapeIndexVerificationState::RecoveryRequired as i32
    );
    assert_eq!(projected.measured_eod_lba, 123);
    assert_eq!(projected.measured_tape_file_count, 7);
    // No canonical prefix authority: prefix counts and digests are absent.
    assert_eq!(projected.verified_prefix_tape_file_count, None);
    assert_eq!(projected.verified_prefix_record_count, None);
    assert_eq!(projected.edition_digest, None);
    assert_eq!(projected.layout_digest, None);
    assert_eq!(projected.payload_digest, None);
    assert_eq!(projected.canonical_map_digest, None);
    assert_eq!(projected.separation_health.len(), 2);
    assert!(projected.separation_health.iter().all(|row| {
        row.state == pb::tape_index_separation_health::State::TapeIndexSeparationStateUnknown as i32
            && row.verified_interior_record_count.is_none()
    }));
    assert_eq!(
        projected.recovery_inventory.as_ref().map(|row| row.outcome),
        Some(pb::TapeInventoryOutcome::BotStructuralRecovered as i32)
    );
    assert_eq!(
        projected
            .recovery_inventory
            .as_ref()
            .and_then(|row| row.incomplete_object_count),
        Some(1)
    );
}

struct ShortFirstModelWriteTransport {
    inner: ModelTransport,
    write_returned_short: bool,
}

impl ShortFirstModelWriteTransport {
    fn new(inner: ModelTransport) -> Self {
        Self {
            inner,
            write_returned_short: false,
        }
    }
}

impl SgTransport for ShortFirstModelWriteTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_in(cdb, buf)
    }

    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        SgTransport::execute_none(&mut self.inner, cdb)
    }

    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        let mut outcome = SgTransport::execute_out(&mut self.inner, cdb, buf)?;
        if cdb.first() == Some(&0x0A) && !self.write_returned_short {
            self.write_returned_short = true;
            outcome.bytes_transferred = outcome.bytes_transferred.saturating_sub(1);
        }
        Ok(outcome)
    }

    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }

    fn configure_reserved_buffer(
        &mut self,
        requested_bytes: u32,
    ) -> Result<u32, remanence_library::ScsiError> {
        self.inner.configure_reserved_buffer(requested_bytes)
    }
}

struct ArmableShortModelWriteTransport {
    inner: ModelTransport,
    short_next_write: Arc<AtomicBool>,
}

impl ArmableShortModelWriteTransport {
    fn new(inner: ModelTransport, short_next_write: Arc<AtomicBool>) -> Self {
        Self {
            inner,
            short_next_write,
        }
    }
}

impl SgTransport for ArmableShortModelWriteTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_in(cdb, buf)
    }

    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        SgTransport::execute_none(&mut self.inner, cdb)
    }

    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        let mut outcome = SgTransport::execute_out(&mut self.inner, cdb, buf)?;
        if cdb.first() == Some(&0x0A) && self.short_next_write.swap(false, Ordering::SeqCst) {
            outcome.bytes_transferred = outcome.bytes_transferred.saturating_sub(1);
        }
        Ok(outcome)
    }

    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }

    fn configure_reserved_buffer(
        &mut self,
        requested_bytes: u32,
    ) -> Result<u32, remanence_library::ScsiError> {
        self.inner.configure_reserved_buffer(requested_bytes)
    }
}

struct FailNthModelWriteTransport {
    inner: ModelTransport,
    target_write: u64,
    write_count: Arc<AtomicU64>,
}

impl FailNthModelWriteTransport {
    fn new(inner: ModelTransport, target_write: u64, write_count: Arc<AtomicU64>) -> Self {
        Self {
            inner,
            target_write,
            write_count,
        }
    }
}

impl SgTransport for FailNthModelWriteTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_in(cdb, buf)
    }

    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        SgTransport::execute_none(&mut self.inner, cdb)
    }

    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        let outcome = SgTransport::execute_out(&mut self.inner, cdb, buf)?;
        let write_ordinal = if cdb.first() == Some(&0x0A) {
            self.write_count.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            0
        };
        if write_ordinal == self.target_write {
            return Err(remanence_library::ScsiError::InvalidInput(
                "injected terminal write completion failure",
            ));
        }
        Ok(outcome)
    }

    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }

    fn configure_reserved_buffer(
        &mut self,
        requested_bytes: u32,
    ) -> Result<u32, remanence_library::ScsiError> {
        self.inner.configure_reserved_buffer(requested_bytes)
    }
}

#[test]
fn restore_phase_decomposition_sums_to_wall_including_saturation() {
    let wall = StdDuration::from_millis(100);
    let position = StdDuration::from_millis(20);
    let transfer = StdDuration::from_millis(65);
    let relay = exclusive_restore_relay_phase(wall, position, transfer);
    assert_eq!(relay, StdDuration::from_millis(15));
    assert_eq!(position + transfer + relay, wall);

    let saturated = exclusive_restore_relay_phase(
        StdDuration::from_millis(5),
        StdDuration::from_millis(4),
        StdDuration::from_millis(4),
    );
    assert_eq!(saturated, StdDuration::ZERO);
}

#[test]
fn session_open_media_family_classifies_media_observations() {
    assert!(matches!(
        session_open_media_family(crate::MediaObservation::label_hint("AOX030L9")),
        MediaFamily::Lto9OrLater
    ));
    assert!(matches!(
        session_open_media_family(crate::MediaObservation::label_hint("AOX030LZ")),
        MediaFamily::Lto9OrLater
    ));
    assert!(matches!(
        session_open_media_family(crate::MediaObservation::label_hint("AOX030L8")),
        MediaFamily::Unknown
    ));
    assert!(matches!(
        session_open_media_family(None),
        MediaFamily::Unknown
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn load_wait_absorbs_only_retryable_drive_completions() {
    fn load_check_condition(key: u8, asc: u8, ascq: u8) -> LoadError {
        let mut sense = vec![0_u8; 32];
        sense[0] = 0x70;
        sense[2] = key;
        sense[7] = 24;
        sense[12] = asc;
        sense[13] = ascq;
        LoadError::DriveLoad(DriveOpError::ScsiError(
            remanence_library::ScsiError::CheckCondition {
                sense,
                bytes_transferred: 0,
            },
        ))
    }

    let first_mount_attention = load_check_condition(0x06, 0x28, 0x00);
    assert!(matches!(
        retryable_readiness_from_load_error(&first_mount_attention, MediaFamily::Lto9OrLater),
        Some(MediaReadiness::UnitAttention {
            asc: 0x28,
            ascq: 0x00
        })
    ));

    let medium_error = load_check_condition(0x03, 0x11, 0x00);
    assert!(retryable_readiness_from_load_error(&medium_error, MediaFamily::Lto9OrLater).is_none());
}

/// The abort reason is the caller's only account of why a session died,
/// and until now the server read it off the request and dropped it. It now
/// reaches the session audit record -- and an abort with no reason leaves
/// the key out rather than writing an empty string, so a later reader can
/// tell "the caller said nothing" from "the caller said nothing useful".
#[test]
fn abort_reason_reaches_the_session_audit_record_only_when_given() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-abort-reason-audit-")
        .tempdir()
        .expect("tempdir");
    let mut index =
        CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "LIB-ABORT-REASON",
        0x0100,
        "DRV-ABORT-REASON",
        0x0400,
        1,
    )));
    let library = open_model_library(Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let cfg = test_write_owner_config(
        temp.path().join("rem-state.sqlite"),
        audit_dir.clone(),
        &library,
        snapshot,
    );

    let explained = Uuid::new_v4();
    let silent = Uuid::new_v4();
    for (session_id, abort_reason) in [
        (explained, Some("rem put: append failed".to_string())),
        (silent, None),
    ] {
        record_session_event(
            &mut index,
            &cfg,
            SessionAuditInput {
                session_id,
                session_kind: "write",
                event: AuditEvent::SessionClosed,
                tape_uuid: None,
                library_serial: None,
                drive_bay: None,
                drive_uuid: None,
                drive_serial: None,
                abort_reason,
            },
        )
        .expect("record session event");
    }

    let records = FileAuditLog::replay(audit_dir.as_path()).expect("replay session audit");
    let detail_for = |session_id: Uuid| {
        records
            .iter()
            .find(|record| record.session_id == Some(session_id))
            .unwrap_or_else(|| panic!("no audit record for session {session_id}"))
            .detail
            .clone()
    };

    assert_eq!(
        detail_for(explained).get("abort_reason"),
        Some(&CborValue::Text("rem put: append failed".to_string())),
        "a stated reason must survive to the audit record"
    );
    assert!(
        !detail_for(silent).contains_key("abort_reason"),
        "an abort with no reason must leave the key out, not record an empty one"
    );
}

#[test]
fn session_open_readiness_fence_records_operation_and_guidance() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-session-open-readiness-")
        .tempdir()
        .expect("tempdir");
    let mut index =
        CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
    let ctx = SessionOpenReadinessContext {
        action: "open write session",
        bay: 0x0001,
        library_serial: "DEC418146K_LL02",
        barcode: Some("AOX030L9"),
        source_slot: Some(0x03eb),
        drive_serial: Some("8031BDC7D1"),
        needs_drive_load: true,
    };

    let status = record_session_open_readiness_fence(
        &mut index,
        &ctx,
        "session_open_short_probe",
        &MediaReadiness::BecomingReady {
            ascq: 0x01,
            media_initializing: true,
        },
    );

    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(status
        .message()
        .contains("media_readiness_state=media_initializing"));
    assert!(status
        .message()
        .contains("rem tape wait-ready --library DEC418146K_LL02"));
    let active = index
        .list_active_media_readiness_operations(Some("DEC418146K_LL02"))
        .expect("active fences");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].phase, "session_open_short_probe");
    assert_eq!(active[0].state, "media_initializing");
    assert_eq!(active[0].dirty_scope.as_deref(), Some("drive+tape"));
    assert_eq!(active[0].drive_element, Some(1));
    assert_eq!(active[0].drive_serial.as_deref(), Some("8031BDC7D1"));
    assert_eq!(active[0].barcode.as_deref(), Some("AOX030L9"));
    assert_eq!(active[0].source_slot, Some(0x03eb));
    assert_eq!(active[0].media_generation, Some(9));
    assert_eq!(active[0].last_cdb_opcode, Some(0));
    assert_eq!(active[0].last_sense_key, Some(0x02));
    assert_eq!(active[0].last_asc, Some(0x04));
    assert_eq!(active[0].last_ascq, Some(0x01));
    assert!(active[0].quarantine_id.is_none());
}

#[test]
fn session_open_refuses_active_tape_io_fence_until_operator_release() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-session-open-tape-io-fence-")
        .tempdir()
        .expect("tempdir");
    let mut index =
        CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
    let tape_uuid = [0x44; 16];
    let fence = index
        .record_tape_io_fence(remanence_state::TapeIoFenceInput {
            tape_uuid,
            barcode: Some("AOX044L9".to_string()),
            reason: "partial_batch".to_string(),
            evidence_json: Some("{\"records_written\":2}".to_string()),
        })
        .expect("record tape-I/O fence");

    let status = session_open_reject_tape_io_fences(
        &index,
        &tape_uuid,
        Some("AOX044L9"),
        "open write session",
    )
    .expect_err("active tape-I/O fence must block session open");

    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(status.message().contains(&fence.quarantine_id));
    assert!(status.message().contains("partial_batch"));

    index
        .release_tape_io_fence(&fence.quarantine_id, "operator released")
        .expect("release tape-I/O fence")
        .expect("released fence");
    session_open_reject_tape_io_fences(&index, &tape_uuid, Some("AOX044L9"), "open write session")
        .expect("released tape-I/O fence no longer blocks session open");
}

#[test]
fn parity_raw_activity_tracks_write_entry_but_not_position_queries() {
    struct FailingRawSink;

    impl RawTapeSink for FailingRawSink {
        fn write_fixed_block(&mut self, _buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            Err(ParityError::Invariant("injected raw block failure"))
        }

        fn write_filemarks(
            &mut self,
            _count: u32,
            _immed: bool,
        ) -> Result<RawWriteOutcome, ParityError> {
            Err(ParityError::Invariant("injected raw filemark failure"))
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(17))
        }
    }

    let mut inner = FailingRawSink;
    let mut write_attempted = false;
    {
        let mut tracked = ActivityTrackingRawTapeSink::new(&mut inner, &mut write_attempted);
        assert_eq!(
            tracked.position().expect("position succeeds"),
            PhysicalPositionHint::new(17)
        );
    }
    assert!(
        !write_attempted,
        "position queries must not raise a write fence"
    );

    {
        let mut tracked = ActivityTrackingRawTapeSink::new(&mut inner, &mut write_attempted);
        tracked
            .write_fixed_block(&[0xAB; 4])
            .expect_err("injected block failure");
    }
    assert!(
        write_attempted,
        "entering the raw block-write boundary must make later failure fenceable"
    );

    struct PositionFailingRawSink;

    impl RawTapeSink for PositionFailingRawSink {
        fn write_fixed_block(&mut self, _buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            panic!("a failed pre-write position check must prevent the block write")
        }

        fn write_filemarks(
            &mut self,
            _count: u32,
            _immed: bool,
        ) -> Result<RawWriteOutcome, ParityError> {
            panic!("a failed pre-write position check must prevent the filemark write")
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Err(ParityError::Invariant(
                "injected pre-write position failure",
            ))
        }
    }

    let mut inner = PositionFailingRawSink;
    let mut write_attempted = false;
    {
        let mut tracked = ActivityTrackingRawTapeSink::new(&mut inner, &mut write_attempted);
        tracked
            .write_fixed_block(&[0xCD; 4])
            .expect_err("position failure must stop before raw write activity");
    }
    assert!(
        !write_attempted,
        "pre-write position failure must not raise a physical-write fence"
    );
}

#[test]
fn parity_raw_admission_refusal_before_dispatch_is_not_fenceable() {
    const BLOCK_SIZE: u32 = 4096;
    let tape_uuid = [0x48; 16];
    let bootstrap = crate::build_tape_bootstrap(
        tape_uuid,
        BLOCK_SIZE,
        ParityConfig::None,
        "2026-10-31T00:00:00Z",
        "test",
    );
    let mut block = vec![0; BLOCK_SIZE as usize];
    write_bootstrap_block(&bootstrap, &mut block).expect("encode bootstrap");
    let mut tape = VirtualTape::empty(1024 * 1024, BLOCK_SIZE);
    tape.records = vec![Record::Block(block), Record::Filemark];
    let mut world = VirtualWorld::single_drive(
        "LIB-ADMISSION-REFUSAL",
        0x0100,
        "DRV-ADMISSION-REFUSAL",
        0x0400,
        1,
    );
    world.put_tape_in_drive(0x0100, "ADMIT001", Some(0x0400), tape);
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(Arc::clone(&world));
    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let mut drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    drive.locate(2).expect("locate the bootstrap EOD");

    // Without an admission the adapter refuses the barrier before dispatch.
    let before = drive.media_write_dispatch_count();
    let mut write_attempted = false;
    let error = {
        let mut raw = DriveHandleRawSink::new(&mut drive);
        let mut tracked = ActivityTrackingRawTapeSink::new(&mut raw, &mut write_attempted);
        tracked
            .write_filemarks(0, false)
            .expect_err("an unadmitted barrier is refused")
    };
    assert!(
        error.to_string().contains("missing append admission"),
        "{error}"
    );
    assert!(write_attempted, "the tracker saw the write attempt");
    assert!(
        !raw_write_reached_drive(&drive, before, write_attempted),
        "a refusal that dispatched nothing must not fence the tape"
    );

    // An admitted barrier reaches the drive and stays fenceable.
    let _admission =
        crate::append_admission::admit_initialized_tape(&mut drive, tape_uuid, BLOCK_SIZE)
            .expect("admit the initialized tape");
    let before = drive.media_write_dispatch_count();
    let mut write_attempted = false;
    {
        let mut raw = DriveHandleRawSink::new(&mut drive);
        let mut tracked = ActivityTrackingRawTapeSink::new(&mut raw, &mut write_attempted);
        tracked
            .write_filemarks(0, false)
            .expect("an admitted barrier dispatches");
    }
    assert!(raw_write_reached_drive(&drive, before, write_attempted));
}

#[tokio::test]
async fn daemon_rejects_corrupt_checkpoint_authority_before_position_or_config_commands() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    const BARCODE: &str = "PARAUTH1";

    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-authority-before-motion-")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let index = CatalogIndex::open(&index_path).expect("open test index");
    drop(index);

    let tape_uuid = [0x47; 16];
    let scheme = remanence_parity::default_scheme_for_block_size(BLOCK_SIZE);
    let mut world = VirtualWorld::single_drive(
        "LIB-PARITY-AUTHORITY",
        0x0100,
        "DRV-PARITY-AUTHORITY",
        0x0400,
        1,
    );
    world.put_tape_in_drive(
        0x0100,
        BARCODE,
        Some(0x0400),
        VirtualTape::empty(1024 * 1024, BLOCK_SIZE),
    );
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let mut cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot);
    cfg.checkpoint_journal_dir = temp.path().join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();

    let checkpoint = remanence_state::authority::fixtures::AuthorityBuilder::new(
        &cfg.checkpoint_journal_dir,
        tape_uuid,
    )
    .build()
    .expect("open checkpoint handle");
    remanence_state::authority::fixtures::damage_authority(checkpoint)
        .expect("write corrupt checkpoint authority");

    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
    let command_start = world.lock().expect("world lock").command_log.len();
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: "parity.authority".to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(BLOCK_SIZE),
        min_object_size_bytes: 0,
    };
    let (open_tx, open_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenWrite {
            actor: remanence_state::AuditActor::System,
            pool_cfg: pool_cfg.clone(),
            selected: SelectedTape {
                standalone_endpoint: None,
                pool_id: "parity.authority".to_string(),
                tape_uuid,
                block_size: BLOCK_SIZE,
                parity_config: ParityConfig::Scheme(scheme),
            },
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: false,
            library_serial: serial.clone(),
            barcode: Some(BARCODE.to_string()),
            source_slot: None,
            drive_uuid: None,
            drive_serial: Some("DRV-PARITY-AUTHORITY".to_string()),
            reply: open_tx,
        })
        .await
        .expect("send parity write open");
    let status = open_rx
        .await
        .expect("parity open reply")
        .expect_err("corrupt checkpoint authority must reject the session");
    assert!(
        status
            .message()
            .contains("tape-log header identity, geometry or CRC mismatch"),
        "{status}"
    );

    let opcodes = world.lock().expect("world lock").command_log[command_start..]
        .iter()
        .map(|record| record.opcode)
        .collect::<Vec<_>>();
    for forbidden in [0x01, 0x92, 0x1a, 0x15] {
        assert!(
            !opcodes.contains(&forbidden),
            "authority rejection issued forbidden drive opcode 0x{forbidden:02x}: {opcodes:02x?}"
        );
    }
}

#[tokio::test]
async fn bootstrap_projection_never_authorizes_writing_physically_blank_media() {
    const BLOCK_SIZE: u32 = 256 * 1024;

    let scheme = remanence_parity::default_scheme_for_block_size(BLOCK_SIZE);
    for (case, barcode, tape_uuid, parity_config) in [
        (
            "parity",
            "MISPAR01",
            [0x4B; 16],
            ParityConfig::Scheme(scheme.clone()),
        ),
        ("no-parity", "MISNOP01", [0x4C; 16], ParityConfig::None),
    ] {
        let temp = tempfile::Builder::new()
            .prefix(&format!("remanence-missing-{case}-authority-"))
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open test index");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: barcode.to_string(),
                block_size: BLOCK_SIZE,
                parity: parity_config.clone(),
                force: false,
            })
            .expect("provision tape");
        let projected_scheme = match &parity_config {
            ParityConfig::Scheme(scheme) => Some(scheme.clone()),
            ParityConfig::None => None,
        };
        index
            .project_committed_tape_file_bundle(
                TapeJournalIndexInput {
                    tape_uuid,
                    block_size: BLOCK_SIZE,
                    scheme: projected_scheme,
                    journal_offset_bytes: 0,
                },
                &CommittedBundle {
                    kind: CommittedBundleKind::BotBootstrap,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 0,
                        kind: TapeFileKind::Bootstrap,
                        block_count: 1,
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
                    total_committed_ordinals: 0,
                },
            )
            .expect("project known written BOT prefix");
        let pool_id = format!("missing.{case}.authority");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: pool_id.clone(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .project_tape_pool_membership(tape_uuid, &pool_id)
            .expect("assign pool");
        drop(index);

        let mut world = VirtualWorld::single_drive(
            format!("LIB-MISSING-{case}-AUTHORITY"),
            0x0100,
            format!("DRV-MISSING-{case}-AUTHORITY"),
            0x0400,
            1,
        );
        world.put_tape_in_drive(
            0x0100,
            barcode,
            Some(0x0400),
            VirtualTape::empty(1024 * 1024, BLOCK_SIZE),
        );
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let mut cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot);
        cfg.checkpoint_journal_dir = temp.path().join("missing-checkpoints");
        std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();

        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
        let command_start = world.lock().expect("world lock").command_log.len();
        let (open_tx, open_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                actor: remanence_state::AuditActor::System,
                pool_cfg: TapePoolConfig {
                    scheme: None,
                    id: format!("missing.{case}.authority"),
                    display_name: None,
                    copy_class: None,
                    content_class: None,
                    selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
                    watermark_low: 0.9,
                    watermark_high: 0.95,
                    capacity_cap_bytes: None,
                    block_size_bytes: u64::from(BLOCK_SIZE),
                    min_object_size_bytes: 0,
                },
                selected: SelectedTape {
                    standalone_endpoint: None,
                    pool_id: format!("missing.{case}.authority"),
                    tape_uuid,
                    block_size: BLOCK_SIZE,
                    parity_config,
                },
                target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                needs_drive_load: false,
                library_serial: serial,
                barcode: Some(barcode.to_string()),
                source_slot: None,
                drive_uuid: None,
                drive_serial: None,
                reply: open_tx,
            })
            .await
            .expect("send write open");
        let status = open_rx
            .await
            .expect("open reply")
            .expect_err("missing authority for catalog-written tape must reject");
        assert!(
            status.message().contains("tape identity"),
            "{case}: {status}"
        );
        assert!(
            world.lock().expect("world lock").command_log[command_start..]
                .iter()
                .all(|command| !matches!(command.opcode, 0x0a | 0x10)),
            "bootstrap projection must not authorize media writes"
        );
    }
}

#[tokio::test]
async fn daemon_checkpoint_lease_contention_precedes_conditional_load_for_no_parity() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    const BARCODE: &str = "NOAUTH01";

    let temp = tempfile::Builder::new()
        .prefix("remanence-checkpoint-lease-before-load-")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    drop(CatalogIndex::open(&index_path).expect("open test index"));
    let tape_uuid = [0x48; 16];
    let mut world = VirtualWorld::single_drive(
        "LIB-CHECKPOINT-LEASE",
        0x0100,
        "DRV-CHECKPOINT-LEASE",
        0x0400,
        1,
    );
    world.put_tape_in_drive(
        0x0100,
        BARCODE,
        Some(0x0400),
        VirtualTape::empty(1024 * 1024, BLOCK_SIZE),
    );
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let mut cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot);
    cfg.checkpoint_journal_dir = temp.path().join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    let checkpoint = remanence_state::authority::fixtures::AuthorityBuilder::new(
        &cfg.checkpoint_journal_dir,
        tape_uuid,
    )
    .build()
    .expect("open checkpoint handle");
    let _held_lease = checkpoint
        .acquire_exclusive()
        .expect("hold competing checkpoint lease");

    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let log_path = temp.path().join("daemon.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || log.try_clone().unwrap())
        .finish();
    let drive_tx =
        tracing::subscriber::with_default(subscriber, || spawn_drive_actor(0x0100, drive, cfg));
    let command_start = world.lock().expect("world lock").command_log.len();
    let (open_tx, open_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenWrite {
            actor: remanence_state::AuditActor::System,
            pool_cfg: TapePoolConfig {
                scheme: None,
                id: "checkpoint.lease".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
                watermark_low: 0.9,
                watermark_high: 0.95,
                capacity_cap_bytes: None,
                block_size_bytes: u64::from(BLOCK_SIZE),
                min_object_size_bytes: 0,
            },
            selected: SelectedTape {
                standalone_endpoint: None,
                pool_id: "checkpoint.lease".to_string(),
                tape_uuid,
                block_size: BLOCK_SIZE,
                parity_config: ParityConfig::None,
            },
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: true,
            library_serial: serial,
            barcode: Some(BARCODE.to_string()),
            source_slot: Some(0x0400),
            drive_uuid: None,
            drive_serial: Some("DRV-CHECKPOINT-LEASE".to_string()),
            reply: open_tx,
        })
        .await
        .expect("send no-parity write open");
    open_rx
        .await
        .expect("no-parity open reply")
        .expect_err("competing checkpoint lease must reject session open");
    let log = std::fs::read_to_string(log_path).unwrap();
    for expected in [
        "WARN",
        "tape-log lock refusal",
        "session_id=",
        "tape_uuid=",
        "operation=",
    ] {
        assert!(log.contains(expected), "missing {expected}: {log}");
    }
    assert!(
        world.lock().expect("world lock").command_log[command_start..].is_empty(),
        "checkpoint contention must reject before conditional LOAD or any drive command"
    );
}

#[test]
fn fresh_parity_bootstrap_short_completion_fences_without_journal_visibility() {
    const BLOCK_SIZE: u32 = 256 * 1024;

    let temp = tempfile::Builder::new()
        .prefix("remanence-fresh-parity-bootstrap-fence-")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open test index");
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "LIB-FRESH-PARITY-FENCE",
        0x0100,
        "DRV-FRESH-PARITY-FENCE",
        0x0400,
        1,
    )));
    world.lock().expect("world lock").put_tape_in_drive(
        0x0100,
        "PARITY001",
        Some(0x0400),
        VirtualTape::empty(1024 * 1024, BLOCK_SIZE),
    );
    let library_model = world.lock().expect("world lock").library_snapshot();
    let policy = remanence_library::StaticAllowlist::new([library_model.serial.as_str()]);
    let factory_world = Arc::clone(&world);
    let mut library = library_model
        .open_with(&policy, move |path| {
            let role = factory_world
                .lock()
                .expect("world lock")
                .role_for_path(path)
                .expect("known model path");
            let model = ModelTransport::new(Arc::clone(&factory_world), role);
            let transport: Box<dyn SgTransport> =
                if path.to_string_lossy().contains("/sg-chaos-drive-") {
                    Box::new(ShortFirstModelWriteTransport::new(model))
                } else {
                    Box::new(model)
                };
            Ok::<_, remanence_library::IoErrorKind>(transport)
        })
        .expect("open model library");
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let mut cfg = test_write_owner_config(index_path, audit_dir.clone(), &library, snapshot);
    cfg.checkpoint_journal_dir = temp.path().join("journals/tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    std::fs::create_dir_all(cfg.checkpoint_journal_dir.parent().expect("journal parent"))
        .expect("create journal parent");
    let tape_uuid = [0x46; 16];
    let scheme = remanence_parity::default_scheme_for_block_size(BLOCK_SIZE);
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "fresh-parity-fence-test".to_string(),
        tape_uuid,
        block_size: BLOCK_SIZE,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };

    let bootstrap = crate::build_tape_bootstrap(
        tape_uuid,
        BLOCK_SIZE,
        ParityConfig::Scheme(scheme.clone()),
        "2026-10-23T00:00:00Z",
        "test",
    );
    let mut block = vec![0; BLOCK_SIZE as usize];
    write_bootstrap_block(&bootstrap, &mut block).unwrap();
    world
        .lock()
        .unwrap()
        .tapes
        .get_mut("PARITY001")
        .unwrap()
        .records = vec![Record::Block(block), Record::Filemark];
    let mut drive = library.open_drive(0x0100, &policy).expect("open drive");
    let _admission = crate::admit_initialized_tape(&mut drive, tape_uuid, BLOCK_SIZE).unwrap();
    let status = {
        let checkpoint_journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
            &cfg.checkpoint_journal_dir,
            tape_uuid,
        )
        .build()
        .expect("open fresh checkpoint journal");
        let mut checkpoint_lease = checkpoint_journal
            .acquire_exclusive()
            .expect("acquire fresh checkpoint lease");
        let authority =
            validate_parity_actor_authority(&cfg, &selected, &mut checkpoint_lease, &[])
                .expect("validate fresh parity authority");
        match open_parity_actor_session(
            &mut index,
            &mut drive,
            &cfg,
            &selected,
            &[],
            authority,
            &mut checkpoint_lease,
        ) {
            Ok(_) => panic!("short bootstrap completion must fail closed"),
            Err(status) => status,
        }
    };

    assert_eq!(status.code(), tonic::Code::Internal);
    let active = index
        .tape_io_admission_conflicts(&tape_uuid, Some("PARITY001"))
        .expect("active tape-I/O fence");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].reason, "partial_batch");
    assert!(active[0]
        .evidence_json
        .as_deref()
        .expect("fence evidence")
        .contains("\"phase\":\"fresh_bootstrap\""));
    let world_guard = world.lock().expect("world lock");
    let records = &world_guard
        .tapes
        .get("PARITY001")
        .expect("virtual tape")
        .records;
    assert!(
        matches!(records.as_slice(), [Record::Block(block)] if block.len() == BLOCK_SIZE as usize),
        "the modeled drive physically accepted the bootstrap before reporting it short"
    );
    drop(world_guard);

    let journal_path = parity_journal_directory(&cfg, tape_uuid).expect("parity journal path");
    let journal = remanence_parity::authority::fixtures::RowAuthorityBuilder::new(
        journal_path,
        tape_uuid,
        BLOCK_SIZE,
        scheme,
    )
    .build()
    .expect("reopen parity journal");
    assert!(
        journal
            .load_committed()
            .expect("load committed journal prefix")
            .entries
            .is_empty(),
        "a nonexact bootstrap completion must remain invisible to the committed journal"
    );
    let audit = FileAuditLog::replay(audit_dir.as_path()).expect("replay fence audit");
    assert!(audit.iter().any(|record| {
        record.event == AuditEvent::TapeIoFenceRaised
            && record.subject.id.as_deref() == Some(active[0].quarantine_id.as_str())
    }));
}

#[test]
fn first_parity_raw_write_failure_persists_and_audits_partial_fence() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-first-parity-raw-fence-")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open test index");
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "LIB-PARITY-FENCE",
        0x0100,
        "DRV-PARITY-FENCE",
        0x0400,
        1,
    )));
    let library = open_model_library(Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let cfg = test_write_owner_config(index_path, audit_dir.clone(), &library, snapshot);
    let tape_uuid = [0x45; 16];
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "parity-fence-test".to_string(),
        tape_uuid,
        block_size: 4096,
        parity_config: ParityConfig::None,
    };
    let error = TapeIoError::PartialBatchUncommittable {
        requested_records: 1,
        written_records: 0,
        requested_bytes: 4096,
        written_bytes: 4095,
        end_of_medium: false,
        sense: None,
    }
    .to_string();

    let (status, audited) = fence_failed_parity_raw_write(
        &mut index,
        &cfg,
        &selected,
        "append",
        Some("caller-first-after-checkpoint"),
        None,
        error.as_str(),
        Status::internal(error.clone()),
    );

    assert_eq!(status.code(), tonic::Code::Internal);
    assert!(audited, "the exact persisted fence must be audited");
    crate::tests::assert_fences_survive_empty_rebuild(&index, &audit_dir);
    let active = index
        .tape_io_admission_conflicts(&tape_uuid, None)
        .expect("active tape-I/O fence");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].reason, "partial_batch");
    let evidence = active[0].evidence_json.as_deref().expect("fence evidence");
    assert!(evidence.contains("\"phase\":\"append\""), "{evidence}");
    assert!(
        evidence.contains("caller-first-after-checkpoint"),
        "{evidence}"
    );
    session_open_reject_tape_io_fences(
        &index,
        &tape_uuid,
        None,
        "open write session after failed parity append",
    )
    .expect_err("the durable partial fence must block the next session");

    let records = FileAuditLog::replay(audit_dir.as_path()).expect("replay fence audit");
    assert!(records.iter().any(|record| {
        record.event == AuditEvent::TapeIoFenceRaised
            && record.subject.id.as_deref() == Some(active[0].quarantine_id.as_str())
    }));
}

#[test]
fn automatic_terminal_transition_transfers_the_session_parity_journal_lock() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-terminal-journal-transfer-")
        .tempdir()
        .expect("tempdir");
    let tape_uuid = [0x48; 16];
    let scheme = remanence_parity::default_scheme_for_block_size(256 * 1024);
    let path = temp.path().to_path_buf();
    let journal = remanence_parity::authority::fixtures::RowAuthorityBuilder::new(
        &path,
        tape_uuid,
        256 * 1024,
        scheme.clone(),
    )
    .build()
    .expect("open session journal");
    let checkpoint = remanence_state::authority::fixtures::AuthorityBuilder::new(
        temp.path().join("tapes"),
        tape_uuid,
    )
    .build()
    .unwrap();
    let mut writer = checkpoint.acquire_exclusive().unwrap();
    writer.retain_structural_publisher(journal).unwrap();

    let contended = remanence_parity::authority::fixtures::RowAuthorityBuilder::new(
        &path,
        tape_uuid,
        256 * 1024,
        scheme.clone(),
    )
    .build()
    .expect_err("a second open must contend with the session-owned journal");
    assert!(contended.is_lock_contended(), "{contended}");

    writer
        .with_structural(|owner, terminal_journal| {
            assert!(
                terminal_journal.is_some(),
                "lend the retained publisher to finalization"
            );
            assert!(
                owner.structural().is_none(),
                "the capability cannot be borrowed twice"
            );
            owner.finish_structural_scope();
        })
        .unwrap();
    assert!(writer.structural().is_none());
    remanence_parity::authority::fixtures::RowAuthorityBuilder::new(
        &path,
        tape_uuid,
        256 * 1024,
        scheme,
    )
    .build()
    .expect("the journal lock is released after terminal ownership ends");
}

#[tokio::test]
async fn partial_epoch_checkpoint_projects_replays_and_first_post_checkpoint_short_write_fences() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    const POOL_ID: &str = "parity-actor-fence";
    const BARCODE: &str = "PAF001L9";

    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-actor-first-write-fence-")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let tape_uuid = [0x47; 16];
    let scheme = remanence_parity::ParityScheme {
        id: remanence_parity::SchemeId::new_static("actor-short-write-test"),
        data_blocks_per_stripe: 128,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    let mut index = CatalogIndex::open(&index_path).expect("open catalog");
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: POOL_ID.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: BARCODE.to_string(),
            block_size: BLOCK_SIZE,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision parity tape");
    index
        .project_tape_pool_membership(tape_uuid, POOL_ID)
        .expect("assign pool");
    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: "DRV-PARITY-ACTOR-FENCE".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("LIB-PARITY-ACTOR-FENCE".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-08-08T00:00:00Z".to_string()),
        })
        .expect("observe drive")
        .drive_uuid;
    drop(index);

    let bootstrap = BootstrapPayload {
        scheme: Some(remanence_parity::ParitySchemeRecord {
            id: scheme.id.as_str().to_string(),
            data_blocks_per_stripe: scheme.data_blocks_per_stripe,
            parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
            stripes_per_neighborhood: scheme.stripes_per_neighborhood,
            no_parity_flag: false,
        }),
        no_parity_flag: false,
        filemark_map_digest: Some(
            remanence_parity::sole_bot_filemark_map_digest().expect("sole BOT digest"),
        ),
        tape_uuid,
        written_by_version: "test".to_string(),
        written_at: "2026-08-08T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: BLOCK_SIZE,
        drive_compression: false,
    };
    let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
    write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode parity bootstrap");
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK_SIZE);
    tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
    tape.written_bytes = u64::from(BLOCK_SIZE);
    let mut world = VirtualWorld::single_drive(
        "LIB-PARITY-ACTOR-FENCE",
        0x0100,
        "DRV-PARITY-ACTOR-FENCE",
        0x0400,
        1,
    );
    world.put_tape_in_drive(0x0100, BARCODE, Some(0x0400), tape);
    let world = Arc::new(Mutex::new(world));
    let short_next_write = Arc::new(AtomicBool::new(false));
    let library_model = world.lock().expect("world lock").library_snapshot();
    let policy = remanence_library::StaticAllowlist::new([library_model.serial.as_str()]);
    let factory_world = Arc::clone(&world);
    let factory_short = Arc::clone(&short_next_write);
    let mut library = library_model
        .open_with(&policy, move |path| {
            let role = factory_world
                .lock()
                .expect("world lock")
                .role_for_path(path)
                .expect("known model path");
            let model = ModelTransport::new(Arc::clone(&factory_world), role);
            let transport: Box<dyn SgTransport> =
                if path.to_string_lossy().contains("/sg-chaos-drive-") {
                    Box::new(ArmableShortModelWriteTransport::new(
                        model,
                        Arc::clone(&factory_short),
                    ))
                } else {
                    Box::new(model)
                };
            Ok::<_, remanence_library::IoErrorKind>(transport)
        })
        .expect("open model library");
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    let checkpoint_dir = temp.path().join("tapes");
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let mut cfg =
        test_write_owner_config(index_path.clone(), audit_dir.clone(), &library, snapshot);
    cfg.checkpoint_journal_dir = checkpoint_dir.clone();
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    cfg.checkpoint_max_objects = 2;
    cfg.checkpoint_max_age_seconds = 3600;
    let serial = library.library().serial.clone();
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: POOL_ID.to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(BLOCK_SIZE),
        min_object_size_bytes: 0,
    };
    let (open_tx, open_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenWrite {
            actor: remanence_state::AuditActor::System,
            pool_cfg: pool_cfg.clone(),
            selected: SelectedTape {
                standalone_endpoint: None,
                pool_id: POOL_ID.to_string(),
                tape_uuid,
                block_size: BLOCK_SIZE,
                parity_config: ParityConfig::Scheme(scheme.clone()),
            },
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: false,
            library_serial: serial.clone(),
            barcode: Some(BARCODE.to_string()),
            source_slot: None,
            drive_uuid: Some(drive_uuid.clone()),
            drive_serial: Some("DRV-PARITY-ACTOR-FENCE".to_string()),
            reply: open_tx,
        })
        .await
        .expect("send parity write open");
    let session = open_rx
        .await
        .expect("parity open reply")
        .expect("open parity actor session");
    let session_id = Uuid::from_slice(&session.session_id).expect("session UUID");

    let bootstrap_index = CatalogIndex::open(&index_path).expect("open bootstrap projection");
    let bootstrap_files = bootstrap_index
        .list_tape_files(&tape_uuid)
        .expect("list freshly projected BOT bootstrap");
    assert!(bootstrap_files.is_empty());
    drop(bootstrap_index);

    let first = append_actor_test_file(
        &drive_tx,
        session_id,
        temp.path().join("parity-first.bin"),
        "parity-first.bin",
        "parity-actor-first",
        b"first parity checkpoint payload",
    )
    .await;
    assert_eq!(
        first
            .record
            .append_commit_info
            .as_ref()
            .expect("first append info")
            .durability,
        pb::AppendDurability::Written as i32
    );
    let (checkpoint_tx, checkpoint_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::Checkpoint {
            session_id,
            trigger: CheckpointTrigger::Explicit,
            expected_batch_id: None,
            reply: Some(checkpoint_tx),
        })
        .await
        .expect("send parity checkpoint");
    let checkpoint = checkpoint_rx
        .await
        .expect("parity checkpoint reply")
        .expect("parity checkpoint succeeds");
    assert_eq!(checkpoint.committed_objects.len(), 1);

    // A committed parity duplicate conflicts before any additional tape write.
    let command_count = world.lock().unwrap().command_log.len();
    let duplicate = append_actor_test_file_result(
        &drive_tx,
        session_id,
        temp.path().join("parity-conflict.bin"),
        "parity-first.bin",
        "parity-actor-first",
        b"different parity content",
    )
    .await
    .unwrap_err();
    assert_eq!(duplicate.code(), tonic::Code::AlreadyExists);
    assert!(
        world.lock().unwrap().command_log[command_count..]
            .iter()
            .all(|command| !matches!(command.opcode, 0x0a | 0x10)),
        "a conflicting caller id writes no tape records or filemarks"
    );

    let (close_tx, close_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::Close {
            session_id,
            reply: close_tx,
        })
        .await
        .expect("close first parity session");
    close_rx
        .await
        .expect("first parity close reply")
        .expect("close checkpointed parity session");

    let checkpoint_journal =
        remanence_state::authority::fixtures::AuthorityBuilder::new(&checkpoint_dir, tape_uuid)
            .build()
            .expect("open durable parity checkpoint journal");
    let records = checkpoint_journal
        .replay()
        .expect("replay durable partial-epoch checkpoint");
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert!(
        record.object_tape_file_bundles[0].total_committed_ordinals
            < u64::from(scheme.data_blocks_per_stripe),
        "the regression keeps a partial parity epoch"
    );
    assert!(
        record.barrier_bundle.is_none(),
        "ordinary checkpoints keep a short epoch open"
    );
    assert_eq!(record.next_tape_file_number, 2);
    assert_eq!(
        record.object_tape_file_bundles[0].highest_protected_ordinal,
        0
    );
    let mut replay_index = CatalogIndex::open(&index_path).expect("open replay projection");
    replay_index
        .project_checkpoint_record(record)
        .expect("idempotently replay the durable partial-epoch checkpoint into SQLite");
    drop(replay_index);

    let (resume_tx, resume_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenWrite {
            actor: remanence_state::AuditActor::System,
            pool_cfg,
            selected: SelectedTape {
                standalone_endpoint: None,
                pool_id: POOL_ID.to_string(),
                tape_uuid,
                block_size: BLOCK_SIZE,
                parity_config: ParityConfig::Scheme(scheme.clone()),
            },
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: false,
            library_serial: serial,
            barcode: Some(BARCODE.to_string()),
            source_slot: None,
            drive_uuid: Some(drive_uuid),
            drive_serial: Some("DRV-PARITY-ACTOR-FENCE".to_string()),
            reply: resume_tx,
        })
        .await
        .expect("send parity resume open");
    let resumed = resume_rx
        .await
        .expect("parity resume reply")
        .expect("resume checkpointed parity actor session");
    let resumed_session_id =
        Uuid::from_slice(&resumed.session_id).expect("resumed parity session UUID");

    short_next_write.store(true, Ordering::SeqCst);
    let second = append_actor_test_file_result(
        &drive_tx,
        resumed_session_id,
        temp.path().join("parity-second.bin"),
        "parity-second.bin",
        "parity-actor-second",
        b"first payload after the durable parity checkpoint",
    )
    .await
    .expect_err("first post-checkpoint raw write must report the injected short completion");
    assert!(second
        .message()
        .contains("partial fixed batch uncommittable"));
    assert!(!short_next_write.load(Ordering::SeqCst));

    let third = append_actor_test_file_result(
        &drive_tx,
        resumed_session_id,
        temp.path().join("parity-third.bin"),
        "parity-third.bin",
        "parity-actor-third",
        b"poisoned session must refuse this payload",
    )
    .await
    .expect_err("the failed raw append must poison the actor session");
    assert_eq!(third.code(), tonic::Code::FailedPrecondition);

    let read_only = CatalogIndex::open_read_only(&index_path).expect("open catalog projection");
    assert!(read_only
        .get_native_object_by_pool_and_caller_object_id(POOL_ID, "parity-actor-first")
        .expect("query checkpointed object")
        .is_some());
    assert!(
        read_only
            .get_native_object_by_unique_caller_object_id("parity-actor-second")
            .expect("query failed object")
            .is_none(),
        "the short post-checkpoint append must not reach catalog visibility"
    );
    let active = read_only
        .tape_io_admission_conflicts(&tape_uuid, Some(BARCODE))
        .expect("active tape-I/O fence");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].reason, "partial_batch");
    assert!(active[0]
        .evidence_json
        .as_deref()
        .expect("fence evidence")
        .contains("parity-actor-second"));
    drop(read_only);

    let world_guard = world.lock().expect("world lock");
    assert!(matches!(
        world_guard
            .tapes
            .get(BARCODE)
            .expect("virtual parity tape")
            .records
            .last(),
        Some(Record::Block(_))
    ));
    drop(world_guard);
    let records = FileAuditLog::replay(audit_dir.as_path()).expect("replay fence audit");
    assert_eq!(
        records
            .iter()
            .filter(|record| {
                record.event == AuditEvent::TapeIoFenceRaised
                    && record.subject.id.as_deref() == Some(active[0].quarantine_id.as_str())
            })
            .count(),
        1,
        "the actor must audit exactly the fence returned by the failed raw append"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn session_open_refuses_active_media_readiness_fence_before_drive_probe() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-session-open-admission-")
        .tempdir()
        .expect("tempdir");
    let mut index =
        CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
    let active_operation_id = Uuid::from_u128(0x9100);
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id: active_operation_id,
            run_id: None,
            endpoint: remanence_state::DriveKey::new("DEC418146K_LL02".to_string(), 0x0100),
            changer_sg: Some("/dev/sg8".to_string()),
            drive_sg: Some("/dev/sg7".to_string()),
            drive_serial: Some("DRV_MOVE_OBS".to_string()),
            barcode: Some("AOX030L9".to_string()),
            source_slot: Some(0x03eb),
            media_generation: Some(9),
            phase: "readiness_poll".to_string(),
            state: "media_initializing".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .expect("record active readiness operation");
    let (mut drive, log) = open_test_drive_with_tur_script("DEC418146K_LL02", vec![None]);
    let before = log
        .borrow()
        .iter()
        .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
        .count();
    let ctx = SessionOpenReadinessContext {
        action: "open write session",
        bay: 0x0100,
        library_serial: "DEC418146K_LL02",
        barcode: Some("AOX030L9"),
        source_slot: Some(0x03eb),
        drive_serial: Some("DRV_MOVE_OBS"),
        needs_drive_load: true,
    };

    let status = session_open_short_probe_or_load(&mut index, &mut drive, ctx)
        .expect_err("active readiness fence must block session-open admission");

    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(status.message().contains("active media-readiness fence"));
    assert!(status.message().contains(&active_operation_id.to_string()));
    let after = log
        .borrow()
        .iter()
        .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
        .count();
    assert_eq!(
        after, before,
        "session-open admission must refuse before TUR or LOAD"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn session_open_loads_immediate_after_unit_attention_then_load_required() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-session-open-load-after-ua-")
        .tempdir()
        .expect("tempdir");
    let mut index =
        CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
    let (mut drive, log) = open_test_drive_with_tur_script(
        "DEC418146K_LL02",
        vec![
            Some(readiness_fixed_sense(0x06, 0x29, 0x00)),
            Some(readiness_fixed_sense(0x02, 0x04, 0x02)),
            None,
        ],
    );
    let ctx = SessionOpenReadinessContext {
        action: "open write session",
        bay: 0x0100,
        library_serial: "DEC418146K_LL02",
        barcode: Some("AOX030L9"),
        source_slot: Some(0x03eb),
        drive_serial: Some("DRV_MOVE_OBS"),
        needs_drive_load: true,
    };

    session_open_short_probe_or_load(&mut index, &mut drive, ctx)
        .expect("session-open readiness should issue LOAD IMMED then reach ready");

    let control_cdbs = log
        .borrow()
        .iter()
        .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
        .map(|cdb| (cdb[0], cdb[1], cdb[4]))
        .collect::<Vec<_>>();
    assert_eq!(
        control_cdbs,
        vec![
            (0x00, 0x00, 0x00),
            (0x00, 0x00, 0x00),
            (0x1b, 0x01, 0x01),
            (0x00, 0x00, 0x00)
        ]
    );
    assert!(
        index
            .list_active_media_readiness_operations(Some("DEC418146K_LL02"))
            .expect("active fences")
            .is_empty(),
        "ready session-open probe must not leave an active fence"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn session_open_loads_immediate_for_already_loaded_initialization_required() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-session-open-already-loaded-load-required-")
        .tempdir()
        .expect("tempdir");
    let mut index =
        CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
    let (mut drive, log) = open_test_drive_with_tur_script(
        "DEC418146K_LL02",
        vec![Some(readiness_fixed_sense(0x02, 0x04, 0x02)), None],
    );
    let ctx = SessionOpenReadinessContext {
        action: "open read session",
        bay: 0x0100,
        library_serial: "DEC418146K_LL02",
        barcode: Some("AOX030L9"),
        source_slot: None,
        drive_serial: Some("DRV_MOVE_OBS"),
        needs_drive_load: false,
    };

    session_open_short_probe_or_load(&mut index, &mut drive, ctx)
        .expect("already-loaded 04/02 should issue LOAD IMMED then reach ready");

    let control_cdbs = log
        .borrow()
        .iter()
        .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
        .map(|cdb| (cdb[0], cdb[1], cdb[4]))
        .collect::<Vec<_>>();
    assert_eq!(
        control_cdbs,
        vec![(0x00, 0x00, 0x00), (0x1b, 0x01, 0x01), (0x00, 0x00, 0x00)]
    );
    assert!(
        index
            .list_active_media_readiness_operations(Some("DEC418146K_LL02"))
            .expect("active fences")
            .is_empty(),
        "ready session-open probe must not leave an active fence"
    );
}

/// A cartridge left unthreaded in its drive (daemon killed between SSC UNLOAD
/// and the changer move), modelled by the virtual world: TEST UNIT READY
/// answers 02/04/02 until an SSC LOAD arrives.
fn unthreaded_cartridge_world(
    library_serial: &str,
) -> (Arc<Mutex<VirtualWorld>>, remanence_library::LibraryHandle) {
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        library_serial,
        0x0100,
        "DRV-UNTHREADED",
        0x0400,
        2,
    )));
    {
        let mut world = world.lock().expect("world lock");
        world.put_tape_in_drive(0x0100, "UNT001L9", Some(0x0400), VirtualTape::default());
        world.leave_cartridge_unthreaded(0x0100);
    }
    let library = open_model_library(Arc::clone(&world));
    (world, library)
}

fn record_unthreaded_readiness_operation(
    index: &mut CatalogIndex,
    library_serial: &str,
    operation_id: Uuid,
) {
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id,
            run_id: None,
            endpoint: remanence_state::DriveKey::new(library_serial.to_string(), 0x0100),
            changer_sg: None,
            drive_sg: None,
            drive_serial: Some("DRV-UNTHREADED".to_string()),
            barcode: Some("UNT001L9".to_string()),
            source_slot: Some(0x0400),
            media_generation: Some(9),
            phase: "readiness_poll".to_string(),
            state: "media_initializing".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .expect("record readiness operation");
}

fn drive_load_commands(world: &Arc<Mutex<VirtualWorld>>) -> usize {
    world
        .lock()
        .expect("world lock")
        .command_log
        .iter()
        .filter(|command| {
            command.opcode == 0x1b
                && matches!(
                    command.role,
                    remanence_chaos::model::DeviceRole::Drive { bay: 0x0100 }
                )
        })
        .count()
}

/// (phase, state, opcode, sense key, ASC, ASCQ) of every recorded transition.
type ReadinessTransitionRow = (
    Option<String>,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

fn readiness_transition_rows(index_path: &Path, operation_id: Uuid) -> Vec<ReadinessTransitionRow> {
    let conn = rusqlite::Connection::open(index_path).expect("open catalog sqlite");
    let mut stmt = conn
        .prepare(
            "select phase, state, last_cdb_opcode, last_sense_key, last_asc, last_ascq
             from media_readiness_transitions where operation_id = ?1
             order by transition_id",
        )
        .expect("prepare transition query");
    stmt.query_map([operation_id.to_string()], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    })
    .expect("query transitions")
    .collect::<Result<Vec<_>, _>>()
    .expect("read transitions")
}

#[cfg(target_os = "linux")]
#[test]
fn drive_wait_ready_sends_one_load_for_an_unthreaded_cartridge() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-wait-ready-unthreaded-")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open test index");
    let (world, mut library) = unthreaded_cartridge_world("LIB-UNTHREAD");
    let policy = remanence_library::StaticAllowlist::new(["LIB-UNTHREAD"]);
    let mut drive = library.open_drive(0x0100, &policy).expect("drive opens");
    let operation_id = Uuid::new_v4();
    record_unthreaded_readiness_operation(&mut index, "LIB-UNTHREAD", operation_id);
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(operation_id, "media_readiness_wait");

    handle_drive_wait_ready(
        &mut index,
        &mut drive,
        DriveReadinessPoll {
            bay: 0x0100,
            operation_id,
            family: MediaFamily::Lto9OrLater,
            options: remanence_library::MediaReadinessWaitOptions {
                wait: true,
                timeout: StdDuration::from_secs(9_000),
                poll_interval: StdDuration::from_secs(30),
            },
            cartridge_in_drive: true,
            phase: "grpc_wait_ready",
        },
        &handle,
    );

    assert_eq!(drive_load_commands(&world), 1, "exactly one SSC LOAD");
    let record = index
        .media_readiness_operation(operation_id)
        .expect("lookup operation")
        .expect("operation exists");
    assert_eq!(record.state, "ready", "{record:?}");
    assert!(index
        .list_active_media_readiness_operations(Some("LIB-UNTHREAD"))
        .expect("active fences")
        .is_empty());
    let rows = readiness_transition_rows(&index_path, operation_id);
    assert_eq!(
        rows.iter()
            .map(|(phase, state, opcode, ..)| (phase.as_deref(), state.as_str(), *opcode))
            .collect::<Vec<_>>(),
        vec![
            (Some("grpc_wait_ready"), "media_initializing", Some(0x00)),
            (
                Some("initializing_command_load"),
                "pre_ready_loading",
                Some(0x1b)
            ),
            (Some("grpc_wait_ready"), "ready", Some(0x00)),
        ]
    );
    assert_eq!(
        (rows[1].3, rows[1].4, rows[1].5),
        (Some(0x02), Some(0x04), Some(0x02)),
        "the LOAD attempt records the sense that asked for it"
    );
    assert!(
        !world.lock().expect("world lock").drive_states[&0x0100].initializing_command_required,
        "the LOAD threaded the cartridge"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn drive_wait_ready_never_repeats_a_load_or_loads_without_inventory_evidence() {
    let options = remanence_library::MediaReadinessWaitOptions {
        wait: true,
        timeout: StdDuration::from_millis(200),
        poll_interval: StdDuration::from_secs(30),
    };
    for (already_loaded, cartridge_in_drive) in [(true, true), (false, false)] {
        let temp = tempfile::Builder::new()
            .prefix("remanence-wait-ready-load-bound-")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open test index");
        let (world, mut library) = unthreaded_cartridge_world("LIB-LOADBOUND");
        let policy = remanence_library::StaticAllowlist::new(["LIB-LOADBOUND"]);
        let mut drive = library.open_drive(0x0100, &policy).expect("drive opens");
        let operation_id = Uuid::new_v4();
        record_unthreaded_readiness_operation(&mut index, "LIB-LOADBOUND", operation_id);
        if already_loaded {
            // An earlier wait on this operation already sent its LOAD.
            index
                .record_media_readiness_transition(
                    crate::startup_media_readiness::media_readiness_initializing_load_transition(
                        operation_id,
                        &MediaReadiness::BecomingReady {
                            ascq: 0x02,
                            media_initializing: true,
                        },
                    ),
                )
                .expect("record earlier LOAD attempt");
        }
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(operation_id, "media_readiness_wait");

        handle_drive_wait_ready(
            &mut index,
            &mut drive,
            DriveReadinessPoll {
                bay: 0x0100,
                operation_id,
                family: MediaFamily::Lto9OrLater,
                options,
                cartridge_in_drive,
                phase: "grpc_wait_ready",
            },
            &handle,
        );

        assert_eq!(
            drive_load_commands(&world),
            0,
            "already_loaded={already_loaded} cartridge_in_drive={cartridge_in_drive}"
        );
        let record = index
            .media_readiness_operation(operation_id)
            .expect("lookup operation")
            .expect("operation exists");
        assert_eq!(record.state, "timeout_unknown", "{record:?}");
        assert_eq!(
            (record.last_sense_key, record.last_asc, record.last_ascq),
            (Some(0x02), Some(0x04), Some(0x02)),
            "the fence keeps the drive's not-ready reason"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn startup_reconcile_loads_an_unthreaded_cartridge_once_and_clears_the_fence() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-startup-unthreaded-")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open test index");
    let (world, mut library) = unthreaded_cartridge_world("LIB-STARTLOAD");
    let policy = remanence_library::StaticAllowlist::new(["LIB-STARTLOAD"]);
    let snapshot = library.library().clone();
    assert_eq!(
        snapshot.drive_bays[0].loaded_tape.as_deref(),
        Some("UNT001L9"),
        "changer inventory places the cartridge in the drive"
    );
    let mut opened_drives = vec![(
        0x0100,
        library.open_drive(0x0100, &policy).expect("drive opens"),
    )];
    let operation_id = Uuid::new_v4();
    record_unthreaded_readiness_operation(&mut index, "LIB-STARTLOAD", operation_id);

    let reconciled = crate::startup_media_readiness::reconcile_library_media_readiness_on_startup(
        &mut index,
        &snapshot,
        &mut opened_drives,
    )
    .expect("startup reconcile");

    assert_eq!(reconciled, 1);
    assert_eq!(drive_load_commands(&world), 1, "exactly one SSC LOAD");
    assert!(index
        .list_active_media_readiness_operations(Some("LIB-STARTLOAD"))
        .expect("active fences")
        .is_empty());
    let rows = readiness_transition_rows(&index_path, operation_id);
    assert_eq!(
        rows.iter()
            .map(|(phase, state, opcode, ..)| (phase.as_deref(), state.as_str(), *opcode))
            .collect::<Vec<_>>(),
        vec![
            (
                Some("startup_reconcile_tur"),
                "media_initializing",
                Some(0x00)
            ),
            (
                Some("initializing_command_load"),
                "pre_ready_loading",
                Some(0x1b)
            ),
            (Some("startup_reconcile_tur"), "ready", Some(0x00)),
        ]
    );
}

/// Reopening the catalog preserves the write-ahead LOAD budget across startup.
#[cfg(target_os = "linux")]
#[test]
fn startup_reconcile_never_repeats_recorded_load_across_restart() {
    let temp = tempfile::tempdir().expect("tempdir");
    let index_path = temp.path().join("state.sqlite");
    let operation_id = Uuid::new_v4();
    {
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        record_unthreaded_readiness_operation(&mut index, "LIB-RESTART", operation_id);
        index
            .record_media_readiness_transition(
                crate::startup_media_readiness::media_readiness_initializing_load_transition(
                    operation_id,
                    &MediaReadiness::BecomingReady {
                        ascq: 2,
                        media_initializing: true,
                    },
                ),
            )
            .expect("record LOAD attempt");
    }
    let (world, library) = unthreaded_cartridge_world("LIB-RESTART");
    drop(library);
    for _ in 0..2 {
        let mut index = CatalogIndex::open(&index_path).expect("reopen catalog");
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library.library().clone();
        let policy = remanence_library::StaticAllowlist::new(["LIB-RESTART"]);
        let mut drives = vec![(0x0100, library.open_drive(0x0100, &policy).expect("drive"))];
        assert_eq!(
            crate::startup_media_readiness::reconcile_library_media_readiness_on_startup(
                &mut index,
                &snapshot,
                &mut drives,
            )
            .expect("reconcile"),
            1
        );
        assert_eq!(
            drive_load_commands(&world),
            0,
            "recorded attempt consumes the LOAD budget"
        );
        assert_eq!(
            index
                .media_readiness_operation(operation_id)
                .expect("record")
                .unwrap()
                .state,
            "media_initializing"
        );
    }
    let rows = readiness_transition_rows(&index_path, operation_id);
    assert_eq!(rows.iter().filter(|row| row.2 == Some(0x1b)).count(), 1);
    assert_eq!(
        rows.iter()
            .filter(|row| row.0.as_deref() == Some("startup_reconcile_tur"))
            .count(),
        2
    );
}

/// RPC inventory evidence reaches the actor's LOAD gate for an unlabelled fence.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn resume_readiness_rpc_without_inventory_sends_no_load() {
    use crate::pb::library_service_server::LibraryService;
    let temp = tempfile::tempdir().expect("tempdir");
    let mut index = CatalogIndex::open(temp.path().join("state.sqlite")).expect("catalog");
    let operation_id = Uuid::new_v4();
    // An unlabelled fence reaches the loaded flag instead of the barcode guard.
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id,
            run_id: None,
            endpoint: remanence_state::DriveKey::new("LIB001".to_string(), 0x0100),
            changer_sg: None,
            drive_sg: None,
            drive_serial: Some("DRV-UNTHREADED".to_string()),
            barcode: None,
            source_slot: None,
            media_generation: Some(9),
            phase: "readiness_poll".to_string(),
            state: "media_initializing".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .expect("record unlabelled fence");
    let (world, mut library) = unthreaded_cartridge_world("LIB001");
    let mut snapshot = library.library().clone();
    snapshot.drive_bays[0].loaded = false;
    snapshot.drive_bays[0].loaded_tape = None;
    let policy = remanence_library::StaticAllowlist::new(["LIB001"]);
    let mut drive = library.open_drive(0x0100, &policy).expect("drive");
    let mut state = crate::ApiState::new(index);
    state.default_library_serial = Some(Arc::new("LIB001".to_string()));
    state.library_snapshot = Some(Arc::new(RwLock::new(Arc::new(
        crate::live_status::LibrarySnapshot {
            report: DiscoveryReport {
                libraries: vec![snapshot],
                unattached_drives: vec![],
                complete: true,
                incomplete_evidence: vec![],
                warnings: vec![],
            },
            captured_at: OffsetDateTime::UNIX_EPOCH,
        },
    ))));
    let (drive_tx, mut drive_rx) = mpsc::channel(1);
    let (changer_tx, _changer_rx) = mpsc::channel(1);
    state.drive_pool = Some(DrivePool::new(
        changer_tx,
        HashMap::from([(0x0100, drive_tx)]),
        Arc::new(HashMap::from([(0x0100, AtomicBool::new(false))])),
    ));
    let service = crate::LibraryServiceApi {
        state: state.clone(),
    };
    service
        .resume_media_readiness(tonic::Request::new(pb::ResumeMediaReadinessRequest {
            operation_id: operation_id.as_bytes().to_vec(),
            timeout_seconds: 1,
            poll_interval_seconds: 1,
        }))
        .await
        .expect("resume RPC");
    let DriveCommand::WaitReady {
        operation_id,
        family,
        mut options,
        cartridge_in_drive,
        handle,
        reservation,
    } = drive_rx.try_recv().expect("RPC dispatches wait")
    else {
        panic!("readiness command")
    };
    assert!(!cartridge_in_drive, "RPC forwards empty inventory");
    assert_eq!(options.timeout.as_secs(), 9000);
    // Shorten only the actor test clock after checking the RPC admission profile.
    options.timeout = StdDuration::from_millis(20);
    handle_drive_wait_ready(
        &mut state.index_write().expect("actor index"),
        &mut drive,
        DriveReadinessPoll {
            bay: 0x0100,
            operation_id,
            family,
            options,
            cartridge_in_drive,
            phase: "grpc_wait_ready",
        },
        &handle,
    );
    drop(reservation);
    assert_eq!(drive_load_commands(&world), 0);
    assert_eq!(
        state
            .index()
            .unwrap()
            .media_readiness_operation(operation_id)
            .unwrap()
            .unwrap()
            .state,
        "timeout_unknown"
    );
}

/// Not-ready polling publishes the same sense in the daemon log and watch stream.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn readiness_poll_logs_sense_and_publishes_shared_progress_keys() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut index = CatalogIndex::open(temp.path().join("state.sqlite")).expect("catalog");
    let operation_id = Uuid::new_v4();
    record_unthreaded_readiness_operation(&mut index, "LIB-LOG", operation_id);
    let (_world, mut library) = unthreaded_cartridge_world("LIB-LOG");
    let policy = remanence_library::StaticAllowlist::new(["LIB-LOG"]);
    let mut drive = library.open_drive(0x0100, &policy).expect("drive");
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(operation_id, "media_readiness_wait");
    let log_path = temp.path().join("daemon.log");
    let log = std::fs::File::create(&log_path).expect("log file");
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || log.try_clone().expect("clone log writer"))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        handle_drive_wait_ready(
            &mut index,
            &mut drive,
            DriveReadinessPoll {
                bay: 0x0100,
                operation_id,
                family: MediaFamily::Lto9OrLater,
                options: remanence_library::MediaReadinessWaitOptions {
                    wait: true,
                    timeout: StdDuration::ZERO,
                    poll_interval: StdDuration::ZERO,
                },
                cartridge_in_drive: false,
                phase: "grpc_wait_ready",
            },
            &handle,
        );
    });
    let log = std::fs::read_to_string(log_path).expect("read daemon log");
    for text in [
        "media-readiness fence remains active: drive not ready",
        "02/04/02",
        "initializing command required",
        "sense=",
        "sense_name=",
        "LIB-LOG",
        "grpc_wait_ready",
    ] {
        assert!(log.contains(text), "missing {text}: {log}");
    }
    let keys = remanence_library::MEDIA_READINESS_SENSE_PROGRESS_KEYS;
    assert_eq!(keys, ("sense", "sense_name"));
    let mut stream = registry.watch(&operation_id).expect("watch");
    let mut observed = false;
    while let Some(status) = tokio::time::timeout(StdDuration::from_secs(1), stream.next())
        .await
        .expect("watch completes")
    {
        let status = status.expect("status");
        if status
            .progress
            .get("state")
            .is_some_and(|state| state == "timeout_unknown")
            && status.state == pb::OperationState::Running as i32
        {
            assert_eq!(
                status.progress.get(keys.0).map(String::as_str),
                Some("02/04/02")
            );
            assert_eq!(
                status.progress.get(keys.1).map(String::as_str),
                Some("initializing command required")
            );
            observed = true;
        }
    }
    assert!(observed, "watch includes the not-ready observation");
}

fn changer_inquiry_response() -> Vec<u8> {
    include_bytes!("../../../../fixtures/inquiry/changer-msl-g3.bin").to_vec()
}

fn drive_lto9_inquiry_response() -> Vec<u8> {
    include_bytes!("../../../../fixtures/inquiry/drive1-lto9.bin").to_vec()
}

fn vpd80_response(serial: &str) -> Vec<u8> {
    let bytes = serial.as_bytes();
    let mut response = vec![0x08u8, 0x80, 0x00, bytes.len() as u8];
    response.extend_from_slice(bytes);
    response
}

fn test_changer_library(serial: &str) -> Library {
    Library {
        serial: serial.to_string(),
        changer_sg: PathBuf::from("/dev/sg-mock"),
        changer_sysfs: PathBuf::from("/sys/class/scsi_device/mock"),
        changer_inquiry: remanence_library::scsi::Inquiry::parse(include_bytes!(
            "../../../../fixtures/inquiry/changer-msl-g3.bin"
        ))
        .expect("parse changer inquiry fixture"),
        chassis_designator: None,
        layout: ElementLayout {
            robot_address: 0,
            drive_start: 0x0100,
            drive_count: 1,
            slot_start: 0x0400,
            slot_count: 1,
            ie_start: 0,
            ie_count: 0,
        },
        drive_bays: vec![DriveBay {
            element_address: 0x0100,
            accessible: true,
            exception: None,
            installed: Some(InstalledDrive {
                serial: "DRV_MOVE_OBS".to_string(),
                identity_source: IdentitySource::DvcidAndInquiry,
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                revision: Some("A1".to_string()),
                sg_path: Some(PathBuf::from("/dev/sg-drive-mock")),
                sysfs_path: None,
            }),
            loaded: false,
            loaded_tape: None,
            source_slot: None,
        }],
        slots: vec![Slot {
            element_address: 0x0400,
            accessible: true,
            exception: None,
            full: true,
            cartridge: Some("TAPE_MOVE".to_string()),
        }],
        ie_ports: Vec::new(),
    }
}

#[cfg(target_os = "linux")]
fn open_test_drive_with_tur_script(
    library_serial: &str,
    tur_senses: Vec<Option<Vec<u8>>>,
) -> (DriveHandle, RecordingLog) {
    let library = test_changer_library(library_serial);
    let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
    let log = RecordingLog::new();
    let log_for_factory = log.clone();
    let changer_serial = library.serial.clone();
    let mut changer_responses = Some(vec![
        changer_inquiry_response(),
        vpd80_response(&changer_serial),
    ]);
    let mut drive_responses = Some(vec![
        drive_lto9_inquiry_response(),
        vpd80_response("DRV_MOVE_OBS"),
    ]);
    let mut tur_senses = Some(tur_senses);
    let mut handle = library
        .open_with(&policy, move |path| {
            if path == Path::new("/dev/sg-mock") {
                let responses = changer_responses
                    .take()
                    .expect("changer opened once in test");
                Ok::<_, remanence_library::IoErrorKind>(Box::new(RecordingTransport::with_log(
                    FixtureTransport::new().with_responses(responses),
                    log_for_factory.clone(),
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let responses = drive_responses.take().expect("drive opened once in test");
                let inner = FixtureTransport::new().with_responses(responses);
                Ok::<_, remanence_library::IoErrorKind>(Box::new(RecordingTransport::with_log(
                    TurScriptTransport::new(
                        inner,
                        tur_senses.take().expect("TUR script consumed once"),
                    ),
                    log_for_factory.clone(),
                )) as Box<dyn SgTransport>)
            } else {
                Err(remanence_library::IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown test path {path:?}"),
                    raw_os_error: None,
                })
            }
        })
        .expect("library opens");
    (
        handle.open_drive(0x0100, &policy).expect("drive opens"),
        log,
    )
}

#[cfg(target_os = "linux")]
struct TurScriptTransport<T> {
    inner: T,
    tur_senses: std::collections::VecDeque<Option<Vec<u8>>>,
}

#[cfg(target_os = "linux")]
impl<T> TurScriptTransport<T> {
    fn new(inner: T, tur_senses: Vec<Option<Vec<u8>>>) -> Self {
        Self {
            inner,
            tur_senses: tur_senses.into(),
        }
    }
}

#[cfg(target_os = "linux")]
impl<T: SgTransport> SgTransport for TurScriptTransport<T> {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_in(cdb, buf)
    }

    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        self.inner.execute_none(cdb)?;
        if cdb == [0, 0, 0, 0, 0, 0] {
            if let Some(Some(sense)) = self.tur_senses.pop_front() {
                return Err(remanence_library::ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                });
            }
        }
        Ok(())
    }

    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_out(cdb, buf)
    }

    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class)
    }
}

#[cfg(target_os = "linux")]
fn readiness_fixed_sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
    let mut sense = vec![0u8; 32];
    sense[0] = 0x70;
    sense[2] = key & 0x0f;
    sense[7] = 24;
    sense[12] = asc;
    sense[13] = ascq;
    sense
}

fn open_model_library(
    world: std::sync::Arc<std::sync::Mutex<VirtualWorld>>,
) -> remanence_library::LibraryHandle {
    let library = world.lock().expect("world lock").library_snapshot();
    let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
    library
        .open_with(&policy, move |path| {
            let role = world
                .lock()
                .expect("world lock")
                .role_for_path(path)
                .expect("known model path");
            Ok(Box::new(ModelTransport::new(
                std::sync::Arc::clone(&world),
                role,
            )))
        })
        .expect("open model library")
}

fn test_write_owner_config(
    index_path: PathBuf,
    audit_dir: PathBuf,
    library: &remanence_library::LibraryHandle,
    library_snapshot: Arc<RwLock<Arc<crate::LibrarySnapshot>>>,
) -> WriteOwnerConfig {
    let serial = library.library().serial.clone();
    let checkpoint_journal_dir = index_path.parent().unwrap().join("tapes");
    std::fs::create_dir_all(&checkpoint_journal_dir).unwrap();
    WriteOwnerConfig {
        index_path,
        report: DiscoveryReport {
            unattached_drives: Vec::new(),
            complete: true,
            incomplete_evidence: Vec::new(),
            libraries: vec![library.library().clone()],
            warnings: Vec::new(),
        },
        policy: remanence_library::StaticAllowlist::new([serial.as_str()]),
        audit_dir,
        audit_fsync: false,
        audit_append_lock: Arc::new(std::sync::Mutex::new(())),
        reservations: Arc::new(HashMap::new()),
        placement: crate::write_owner::ActorPlacement::Library(
            crate::write_owner::LibraryActorContext {
                serial: serial.clone(),
                snapshot: library_snapshot,
            },
        ),
        snapshot_miss_alarm: 1,
        managed_library_serials: Arc::new(HashSet::from([serial])),
        cleaning: remanence_state::CleaningConfig::default(),
        tape_io: remanence_state::TapeIoConfig::default(),
        tape_pools: Vec::new(),
        verification_route: remanence_parity::VerificationRoute::Walk,
        io_memory: crate::io_memory::IoMemoryReservation::new(
            remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES,
        )
        .expect("test I/O memory manager"),
        write_admissions: WriteAdmissionCoordinator::default(),
        checkpoint_journal_dir,
        checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
        checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
        checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
        session_idle_seconds: 1800,
        lifecycle: None,
        changer_ready_wait: remanence_library::ChangerReadyWait::default(),
        calibration_store: remanence_state::CalibrationControlStore::open(
            std::env::temp_dir().join(format!("rem-calibration-tests-{}", Uuid::new_v4())),
        )
        .expect("open test calibration store"),
    }
}

fn test_io_memory() -> Arc<crate::io_memory::IoMemoryReservation> {
    crate::io_memory::IoMemoryReservation::new(remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES)
        .expect("test I/O memory manager")
}

fn library_snapshot_cell(library: Library) -> Arc<RwLock<Arc<crate::LibrarySnapshot>>> {
    Arc::new(RwLock::new(Arc::new(crate::LibrarySnapshot {
        report: DiscoveryReport {
            unattached_drives: Vec::new(),
            complete: true,
            incomplete_evidence: Vec::new(),
            libraries: vec![library],
            warnings: Vec::new(),
        },
        captured_at: OffsetDateTime::UNIX_EPOCH,
    })))
}

async fn append_actor_test_file(
    drive_tx: &mpsc::Sender<DriveCommand>,
    session_id: Uuid,
    source_path: PathBuf,
    archive_path: &str,
    caller_object_id: &str,
    payload: &[u8],
) -> AppendFinishOutcome {
    append_actor_test_file_result(
        drive_tx,
        session_id,
        source_path,
        archive_path,
        caller_object_id,
        payload,
    )
    .await
    .expect("actor test append succeeds")
}

async fn append_actor_test_file_result(
    drive_tx: &mpsc::Sender<DriveCommand>,
    session_id: Uuid,
    source_path: PathBuf,
    archive_path: &str,
    caller_object_id: &str,
    payload: &[u8],
) -> Result<AppendFinishOutcome, Status> {
    std::fs::write(&source_path, payload).expect("write actor test source");
    let (append_tx, append_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::AppendFinish {
            session_id,
            source: crate::WriteObjectSource::Path(source_path),
            archive_path: PathBuf::from(archive_path),
            caller_object_id: caller_object_id.to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            representation: crate::PoolWriteRepresentation::Plaintext,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            live_write_counter: None,
            reply: append_tx,
        })
        .await
        .expect("send actor test append");
    append_rx.await.expect("actor test append reply")
}

/// Open one parity-off actor session against an already seated test tape.
async fn open_actor_test_write_session(
    drive_tx: &mpsc::Sender<DriveCommand>,
    pool_cfg: &TapePoolConfig,
    tape_uuid: TapeUuid,
    library_serial: &str,
    barcode: &str,
    drive_uuid: &[u8],
    drive_serial: &str,
) -> Uuid {
    let (open_tx, open_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenWrite {
            actor: remanence_state::AuditActor::System,
            pool_cfg: pool_cfg.clone(),
            selected: SelectedTape {
                standalone_endpoint: None,
                pool_id: pool_cfg.id.clone(),
                tape_uuid,
                block_size: u32::try_from(pool_cfg.block_size_bytes)
                    .expect("actor test pool block size fits u32"),
                parity_config: ParityConfig::None,
            },
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: false,
            library_serial: library_serial.to_string(),
            barcode: Some(barcode.to_string()),
            source_slot: None,
            drive_uuid: Some(drive_uuid.to_vec()),
            drive_serial: Some(drive_serial.to_string()),
            reply: open_tx,
        })
        .await
        .expect("send actor test write open");
    let session = open_rx
        .await
        .expect("actor test write open reply")
        .expect("open actor test write session");
    Uuid::from_slice(&session.session_id).expect("actor test write session UUID")
}

#[test]
fn checkpoint_timer_request_queues_behind_existing_drive_actor_work() {
    let (tx, mut rx) = mpsc::channel(4);
    let session_id = Uuid::from_bytes([0x71; 16]);
    let batch_id = Uuid::from_bytes([0x72; 16]);
    let (reply, _reply_rx) = oneshot::channel();
    tx.blocking_send(DriveCommand::Get { session_id, reply })
        .expect("queue in-flight actor command");

    let mut timers = super::timers::SessionTimers::default();
    arm_checkpoint_timer(
        &mut timers,
        tx,
        session_id,
        batch_id,
        StdDuration::from_millis(0),
    )
    .expect("spawn checkpoint timer");

    assert!(matches!(
        rx.blocking_recv().expect("first queued command"),
        DriveCommand::Get { .. }
    ));
    assert!(matches!(
        rx.blocking_recv().expect("timer checkpoint request"),
        DriveCommand::Checkpoint {
            session_id: queued_session,
            trigger: CheckpointTrigger::Timer,
            expected_batch_id: Some(queued_batch),
            reply: None,
        } if queued_session == session_id && queued_batch == batch_id
    ));
}

#[test]
fn canceled_checkpoint_reply_restores_unclaimed_committed_receipts() {
    let mut receipts = vec![pb::ObjectRecord {
        object_id: vec![0x70; 16],
        ..Default::default()
    }];
    let (reply, receiver) = oneshot::channel();
    drop(receiver);

    send_checkpoint_actor_reply(
        reply,
        pb::WriteSession::default(),
        &mut receipts,
        CheckpointTrigger::Explicit,
    );

    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].object_id, vec![0x70; 16]);
}

#[test]
fn timer_close_parks_session_and_releases_drive_bay() {
    let temp = tempfile::tempdir().expect("temp dir");
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "LIB-CHECKPOINT-IDLE",
        0x0100,
        "DRV-CHECKPOINT-IDLE",
        0x0400,
        1,
    )));
    let library = open_model_library(world);
    let snapshot = library_snapshot_cell(library.library().clone());
    let (timer_park_tx, mut timer_park_rx) = mpsc::unbounded_channel();
    let lifecycle = DrivePoolLifecycle::with_timer_park_sender(timer_park_tx);
    let drive_key = DriveKey::new("LIB-CHECKPOINT-IDLE", 0x0100);
    let reservations = Arc::new(HashMap::from([(drive_key.clone(), AtomicBool::new(true))]));
    let session_id = Uuid::from_bytes([0x73; 16]);
    lifecycle
        .sessions
        .lock()
        .expect("session lifecycle")
        .insert(
            session_id,
            MountedSession {
                bay: 0x0100,
                library_serial: "LIB-CHECKPOINT-IDLE".to_string(),
                barcode: Some("CHK001L9".to_string()),
                home_slot: Some(0x0400),
                tape_uuid: [0x74; 16],
                drive_uuid: Some(vec![0x75; 16]),
            },
        );
    let mut cfg = test_write_owner_config(
        temp.path().join("index.sqlite"),
        temp.path().join("audit"),
        &library,
        snapshot,
    );
    cfg.reservations = Arc::clone(&reservations);
    cfg.lifecycle = Some(lifecycle.clone());

    park_timer_closed_session(&cfg, session_id).expect("close and park session");

    assert!(!lifecycle
        .sessions
        .lock()
        .expect("session lifecycle")
        .contains_key(&session_id));
    let parked = lifecycle
        .parked
        .lock()
        .expect("parked lifecycle")
        .by_drive
        .get(&drive_key)
        .cloned()
        .expect("parked cartridge");
    assert_eq!(parked.seated.prior_session_id, Some(session_id));
    assert!(!reservations[&drive_key].load(Ordering::SeqCst));
    assert_eq!(
        timer_park_rx
            .try_recv()
            .expect("timer close arms idle-dismount scheduling"),
        parked
    );
}

#[tokio::test]
async fn checkpoint_actor_deduplicates_in_batch_and_holds_until_checkpoint() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    let temp = tempfile::Builder::new()
        .prefix("remanence-batched-actor")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let tape_uuid = [0x76; 16];
    let mut index = CatalogIndex::open(&index_path).expect("open catalog");
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: "checkpoint-test".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: "CHK002L9".to_string(),
            block_size: BLOCK_SIZE,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_tape_pool_membership(tape_uuid, "checkpoint-test")
        .expect("assign pool");
    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: "DRV-CHECKPOINT".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("LIB-CHECKPOINT".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-07-21T00:00:00Z".to_string()),
        })
        .expect("observe drive")
        .drive_uuid;
    drop(index);

    let bootstrap = BootstrapPayload {
        scheme: None,
        no_parity_flag: true,
        filemark_map_digest: None,
        tape_uuid,
        written_by_version: "test".to_string(),
        written_at: "2026-07-21T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: BLOCK_SIZE,
        drive_compression: false,
    };
    let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
    write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK_SIZE);
    tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
    tape.written_bytes = u64::from(BLOCK_SIZE);
    let mut world =
        VirtualWorld::single_drive("LIB-CHECKPOINT", 0x0100, "DRV-CHECKPOINT", 0x0400, 1);
    world.put_tape_in_drive(0x0100, "CHK002L9", Some(0x0400), tape);
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let mut cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
    cfg.checkpoint_journal_dir = temp.path().join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    cfg.checkpoint_max_objects = 2;
    cfg.checkpoint_max_age_seconds = 3600;
    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);

    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: "checkpoint-test".to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(BLOCK_SIZE),
        min_object_size_bytes: 0,
    };
    let (open_tx, open_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenWrite {
            actor: remanence_state::AuditActor::System,
            pool_cfg,
            selected: SelectedTape {
                standalone_endpoint: None,
                pool_id: "checkpoint-test".to_string(),
                tape_uuid,
                block_size: BLOCK_SIZE,
                parity_config: ParityConfig::None,
            },
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: false,
            library_serial: serial,
            barcode: Some("CHK002L9".to_string()),
            source_slot: None,
            drive_uuid: Some(drive_uuid),
            drive_serial: Some("DRV-CHECKPOINT".to_string()),
            reply: open_tx,
        })
        .await
        .expect("send write open");
    let session = open_rx
        .await
        .expect("open reply")
        .expect("open batched session");
    // The session reports the target kind it was opened with.
    assert_eq!(
        session.target_kind,
        pb::write_session::TargetKind::WriteSessionTargetKindPool as i32
    );
    let session_id = Uuid::from_slice(&session.session_id).expect("session UUID");

    let written = append_actor_test_file(
        &drive_tx,
        session_id,
        temp.path().join("checkpoint-source-1.bin"),
        "payload-1.bin",
        "checkpoint-caller-object-1",
        b"checkpoint payload one",
    )
    .await
    .record;
    let written_info = written
        .append_commit_info
        .as_ref()
        .expect("WRITTEN append info");
    assert_eq!(
        written_info.durability,
        pb::AppendDurability::Written as i32
    );
    assert!(written.copies.is_empty());
    assert_eq!(written_info.tape_file_number, None);
    let replay = append_actor_test_file(
        &drive_tx,
        session_id,
        temp.path().join("checkpoint-source-1-replay.bin"),
        "payload-1.bin",
        "checkpoint-caller-object-1",
        b"checkpoint payload one",
    )
    .await;
    assert!(replay.replay, "same in-batch content must be a replay");
    assert_eq!(replay.record.object_id, written.object_id);
    let conflict = append_actor_test_file_result(
        &drive_tx,
        session_id,
        temp.path().join("checkpoint-source-1-conflict.bin"),
        "payload-1.bin",
        "checkpoint-caller-object-1",
        b"different checkpoint payload",
    )
    .await
    .expect_err("different in-batch content under the same caller id must conflict");
    assert_eq!(conflict.code(), tonic::Code::AlreadyExists);
    let object_id = Uuid::from_slice(&written.object_id)
        .expect("object UUID")
        .to_string();
    let read_only = CatalogIndex::open_read_only(&index_path).expect("open projection");
    assert!(read_only
        .get_native_object(&object_id)
        .expect("query WRITTEN object")
        .is_none());
    drop(read_only);

    let (get_tx, get_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::Get {
            session_id,
            reply: get_tx,
        })
        .await
        .expect("send session get");
    let pending = get_rx
        .await
        .expect("get reply")
        .expect("get batched session");
    assert_eq!(pending.pending_checkpoint_objects, 1);
    assert!(pending.pending_checkpoint_bytes > 0);
    assert!(pending.checkpoint_deadline.is_some());

    let (checkpoint_tx, checkpoint_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::Checkpoint {
            session_id,
            trigger: CheckpointTrigger::Explicit,
            expected_batch_id: None,
            reply: Some(checkpoint_tx),
        })
        .await
        .expect("send explicit checkpoint");
    let checkpoint = checkpoint_rx
        .await
        .expect("checkpoint reply")
        .expect("checkpoint batch");
    assert_eq!(checkpoint.committed_objects.len(), 1);
    let checkpointed_object = &checkpoint.committed_objects[0];
    assert_eq!(
        checkpointed_object
            .append_commit_info
            .as_ref()
            .expect("checkpointed append info")
            .durability,
        pb::AppendDurability::Checkpointed as i32
    );
    assert_eq!(
        checkpointed_object
            .append_commit_info
            .as_ref()
            .expect("checkpointed append info")
            .sealed_after_write,
        Some(false)
    );
    assert_eq!(checkpointed_object.copies.len(), 1);
    assert_eq!(checkpointed_object.copies[0].tape_uuid, tape_uuid);
    assert_eq!(checkpointed_object.copies[0].tape_file_number, 1);
    let read_only = CatalogIndex::open_read_only(&index_path).expect("open projection");
    assert!(read_only
        .get_native_object(&object_id)
        .expect("query checkpointed object")
        .is_some());
    drop(read_only);

    let second = append_actor_test_file(
        &drive_tx,
        session_id,
        temp.path().join("checkpoint-source-2.bin"),
        "payload-2.bin",
        "checkpoint-caller-object-2",
        b"checkpoint payload two",
    )
    .await
    .record;
    assert_eq!(
        second
            .append_commit_info
            .as_ref()
            .expect("second WRITTEN info")
            .durability,
        pb::AppendDurability::Written as i32
    );
    let third = append_actor_test_file(
        &drive_tx,
        session_id,
        temp.path().join("checkpoint-source-3.bin"),
        "payload-3.bin",
        "checkpoint-caller-object-3",
        b"checkpoint payload three",
    )
    .await
    .record;
    assert_eq!(
        third
            .append_commit_info
            .as_ref()
            .expect("threshold CHECKPOINTED info")
            .durability,
        pb::AppendDurability::Checkpointed as i32
    );
    assert_eq!(
        third
            .append_commit_info
            .as_ref()
            .expect("threshold CHECKPOINTED info")
            .sealed_after_write,
        Some(false)
    );
    assert_eq!(third.copies.len(), 1);
    assert_eq!(third.copies[0].tape_uuid, tape_uuid);
    assert_eq!(third.copies[0].tape_file_number, 3);

    let (receipt_tx, receipt_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::Checkpoint {
            session_id,
            trigger: CheckpointTrigger::Explicit,
            expected_batch_id: None,
            reply: Some(receipt_tx),
        })
        .await
        .expect("request automatic-checkpoint receipts");
    let receipts = receipt_rx
        .await
        .expect("receipt reply")
        .expect("retrieve automatic checkpoint receipts");
    assert_eq!(
        receipts.committed_objects.len(),
        2,
        "automatic threshold checkpoints retain their full copy set"
    );
    let threshold_copy_placements = receipts
        .committed_objects
        .iter()
        .map(|object| {
            assert_eq!(object.copies.len(), 1);
            (
                object.copies[0].tape_uuid.clone(),
                object.copies[0].tape_file_number,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        threshold_copy_placements,
        vec![(tape_uuid.to_vec(), 2), (tape_uuid.to_vec(), 3)]
    );
    let fourth = append_actor_test_file(
        &drive_tx,
        session_id,
        temp.path().join("checkpoint-source-4.bin"),
        "payload-4.bin",
        "checkpoint-caller-object-4",
        b"checkpoint payload four",
    )
    .await
    .record;
    assert_eq!(
        fourth
            .append_commit_info
            .as_ref()
            .expect("close-trigger WRITTEN info")
            .durability,
        pb::AppendDurability::Written as i32
    );
    let fourth_object_id = Uuid::from_slice(&fourth.object_id)
        .expect("fourth object UUID")
        .to_string();

    let (close_tx, close_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::Close {
            session_id,
            reply: close_tx,
        })
        .await
        .expect("send close");
    let closed = close_rx
        .await
        .expect("close reply")
        .expect("close checkpointed session");
    assert_eq!(closed.session.checkpointed_objects.len(), 1);
    assert_eq!(
        closed.session.checkpointed_objects[0].object_id,
        fourth.object_id
    );
    assert_eq!(closed.session.committed_copies.len(), 1);
    assert_eq!(closed.session.committed_copies[0].tape_uuid, tape_uuid);
    assert_eq!(closed.session.committed_copies[0].tape_file_number, 4);
    let journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
        temp.path().join("tapes"),
        tape_uuid,
    )
    .build()
    .expect("open checkpoint journal after session lease release");
    let read_only = CatalogIndex::open_read_only(&index_path).expect("open projection");
    assert!(read_only
        .get_native_object(&fourth_object_id)
        .expect("query close-checkpointed object")
        .is_some());
    assert_eq!(
        journal
            .last()
            .expect("replay close checkpoint")
            .expect("close checkpoint record")
            .committed_object_count,
        4
    );
    let records = journal.replay().expect("replay all checkpoints");
    assert_eq!(
        records
            .iter()
            .map(|record| record.next_tape_file_number)
            .collect::<Vec<_>>(),
        vec![2, 4, 5],
        "each checkpoint names the first free dense tape file"
    );
    let read_only = CatalogIndex::open_read_only(&index_path).expect("open final projection");
    let tape_files = read_only
        .list_tape_files(&tape_uuid)
        .expect("list tape files");
    assert_eq!(tape_files.len(), 5);
    assert_eq!(tape_files.last().expect("last tape file").kind, "object");
    drop(read_only);

    let world = world.lock().expect("world lock");
    let tape = world.tapes.get("CHK002L9").expect("checkpoint tape");
    let bootstraps = tape
        .records
        .iter()
        .filter_map(|record| match record {
            Record::Block(block) => parse_bootstrap_block(block).ok(),
            Record::ZeroBlock(_) | Record::Filemark => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(bootstraps.len(), 1, "only the BOT Bootstrap is physical");
    assert_eq!(bootstraps[0].sequence, 0);
    for record in &records {
        assert!(record
            .objects
            .iter()
            .all(|object| !object.object_recovery_row.object_id.is_empty()));
    }
    assert_eq!(
        records.last().expect("last record").eod_lba as usize,
        tape.records.len(),
        "journal EOD names the physical Object boundary"
    );
}

#[tokio::test]
async fn daemon_watermark_terminalization_is_atomic_and_terminal_failure_stays_fenced() {
    const BLOCK_SIZE: u32 = 256 * 1024;

    for reject_terminal in [false, true] {
        let case = if reject_terminal {
            "failure"
        } else {
            "success"
        };
        let temp = tempfile::Builder::new()
            .prefix(&format!("remanence-daemon-terminal-{case}-"))
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let tape_uuid = if reject_terminal {
            [0x7C; 16]
        } else {
            [0x7B; 16]
        };
        let barcode = if reject_terminal {
            "TRMFL1L9"
        } else {
            "TRMOK1L9"
        };
        let pool_id = format!("terminal.{case}");
        let library_serial = format!("LIB-TERMINAL-{case}");
        let drive_serial = format!("DRV-TERMINAL-{case}");
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: pool_id.clone(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: barcode.to_string(),
                block_size: BLOCK_SIZE,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, &pool_id)
            .expect("assign pool");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: drive_serial.clone(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some(library_serial.clone()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-08-08T00:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        drop(index);

        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-08-08T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: BLOCK_SIZE,
            drive_compression: false,
        };
        let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
        let mut tape = VirtualTape::empty(3 * 1024 * 1024 * 1024, BLOCK_SIZE);
        tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
        tape.written_bytes = u64::from(BLOCK_SIZE);
        let mut world = VirtualWorld::single_drive(
            library_serial.clone(),
            0x0100,
            drive_serial.clone(),
            0x0400,
            1,
        );
        world.put_tape_in_drive(0x0100, barcode, Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let library_model = world.lock().expect("world lock").library_snapshot();
        let policy = remanence_library::StaticAllowlist::new([library_serial.as_str()]);
        let transport_world = Arc::clone(&world);
        let write_count = Arc::new(AtomicU64::new(0));
        let transport_write_count = Arc::clone(&write_count);
        let mut library = library_model
            .open_with(&policy, move |path| {
                let role = transport_world
                    .lock()
                    .expect("world lock")
                    .role_for_path(path)
                    .expect("known model path");
                let model = ModelTransport::new(Arc::clone(&transport_world), role);
                let transport: Box<dyn SgTransport> = if reject_terminal {
                    Box::new(FailNthModelWriteTransport::new(
                        model,
                        4,
                        Arc::clone(&transport_write_count),
                    ))
                } else {
                    Box::new(model)
                };
                Ok::<_, remanence_library::IoErrorKind>(transport)
            })
            .expect("open model library");
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let mut cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
        cfg.checkpoint_journal_dir = temp.path().join("tapes");
        std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
        cfg.checkpoint_max_objects = 1;
        cfg.checkpoint_max_age_seconds = 3600;
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
        let pool_cfg = TapePoolConfig {
            scheme: None,
            id: pool_id,
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.000_000_000_001,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        let session_id = open_actor_test_write_session(
            &drive_tx,
            &pool_cfg,
            tape_uuid,
            &library_serial,
            barcode,
            &drive_uuid,
            &drive_serial,
        )
        .await;
        let first = append_actor_test_file_result(
            &drive_tx,
            session_id,
            temp.path().join("terminal-source.bin"),
            "terminal.bin",
            "terminal-caller",
            b"terminal watermark payload",
        )
        .await;

        if reject_terminal {
            let status = match first {
                    Err(status) => status,
                    Ok(outcome) => panic!(
                        "short terminal component must fail append; observed {} WRITE CDBs: {outcome:?}",
                        write_count.load(Ordering::SeqCst)
                    ),
                };
            assert!(
                status.message().contains("terminal tape write failed"),
                "{status}"
            );
        } else {
            let outcome = first.expect("watermark terminalization succeeds");
            let append_info = outcome
                .record
                .append_commit_info
                .expect("checkpointed append info");
            assert_eq!(
                append_info.durability,
                pb::AppendDurability::Checkpointed as i32
            );
            assert_eq!(append_info.sealed_after_write, Some(true));
        }

        let second = append_actor_test_file_result(
            &drive_tx,
            session_id,
            temp.path().join("terminal-second-source.bin"),
            "terminal-second.bin",
            "terminal-second-caller",
            b"must not reach tape",
        )
        .await
        .expect_err("terminal outcome must close the append gate");
        if reject_terminal {
            assert_eq!(second.code(), tonic::Code::FailedPrecondition);
            assert!(second.message().contains("poisoned"), "{second}");
        } else {
            assert_eq!(second.code(), tonic::Code::ResourceExhausted);
            assert!(second.message().contains("sealed"), "{second}");
        }

        let (close_tx, close_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Close {
                session_id,
                reply: close_tx,
            })
            .await
            .expect("send close");
        close_rx
            .await
            .expect("close reply")
            .expect("close terminal session");

        let checkpoint = remanence_state::authority::fixtures::AuthorityBuilder::new(
            temp.path().join("tapes"),
            tape_uuid,
        )
        .build()
        .expect("open checkpoint journal after lease release");
        let read_only = CatalogIndex::open_read_only(&index_path).expect("open catalog");
        let tape = read_only
            .get_tape(&tape_uuid)
            .expect("query tape")
            .expect("tape exists");
        if reject_terminal {
            assert_eq!(checkpoint.replay().unwrap().len(), 1);
            assert!(checkpoint.pending_finalization().unwrap().is_some());
            assert_eq!(tape.state, "recovery_required");
            assert!(read_only
                .get_native_object_by_unique_caller_object_id("terminal-caller")
                .expect("query committed object")
                .is_some());
            let fences = read_only
                .tape_io_admission_conflicts(&tape_uuid, Some(barcode))
                .expect("query terminal failure fence");
            assert_eq!(fences.len(), 1);
            assert_eq!(fences[0].reason, "transfer_error");
        } else {
            let records = checkpoint.replay().expect("replay terminal authority");
            assert_eq!(records.len(), 2);
            assert!(!records[0].sealed_after_write);
            assert!(records[1].sealed_after_write);
            assert!(records[1].objects.is_empty());
            assert_eq!(tape.state, "sealed");
            assert_eq!(tape.written_extent_lba, Some(records[1].eod_lba));
            assert!(read_only
                .get_native_object_by_unique_caller_object_id("terminal-caller")
                .expect("query committed object")
                .is_some());
            assert!(read_only
                .tape_io_admission_conflicts(&tape_uuid, Some(barcode))
                .expect("query success fences")
                .is_empty());
        }
    }
}

#[tokio::test]
async fn manual_finalize_closes_below_low_and_same_key_replay_moves_no_tape() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    let temp = tempfile::Builder::new()
        .prefix("remanence-manual-terminal-below-low-")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let tape_uuid = [0x7D; 16];
    let barcode = "TRMOP1L9";
    let pool_id = "terminal.operator";
    let library_serial = "LIB-TERMINAL-OPERATOR";
    let drive_serial = "DRV-TERMINAL-OPERATOR";
    let mut index = CatalogIndex::open(&index_path).expect("open catalog");
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: barcode.to_string(),
            block_size: BLOCK_SIZE,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign pool");
    let assignment_generation = index
        .get_tape_assignment_snapshot(&tape_uuid)
        .expect("assignment query")
        .expect("assignment exists")
        .assignment_generation;
    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: drive_serial.to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some(library_serial.to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-08-09T00:00:00Z".to_string()),
        })
        .expect("observe drive")
        .drive_uuid;
    drop(index);

    let bootstrap = BootstrapPayload {
        scheme: None,
        no_parity_flag: true,
        filemark_map_digest: None,
        tape_uuid,
        written_by_version: "test".to_string(),
        written_at: "2026-08-09T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: BLOCK_SIZE,
        drive_compression: false,
    };
    let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
    write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
    let mut tape = VirtualTape::empty(3 * 1024 * 1024 * 1024, BLOCK_SIZE);
    tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
    tape.written_bytes = u64::from(BLOCK_SIZE);
    let mut world = VirtualWorld::single_drive(library_serial, 0x0100, drive_serial, 0x0400, 1);
    world.put_tape_in_drive(0x0100, barcode, Some(0x0400), tape);
    let world = Arc::new(Mutex::new(world));
    let library_model = world.lock().expect("world lock").library_snapshot();
    let policy = remanence_library::StaticAllowlist::new([library_serial]);
    let transport_world = Arc::clone(&world);
    let mut library = library_model
        .open_with(&policy, move |path| {
            let role = transport_world
                .lock()
                .expect("world lock")
                .role_for_path(path)
                .expect("known model path");
            Ok::<_, remanence_library::IoErrorKind>(Box::new(ModelTransport::new(
                Arc::clone(&transport_world),
                role,
            )) as Box<dyn SgTransport>)
        })
        .expect("open model library");
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let mut cfg =
        test_write_owner_config(index_path.clone(), audit_dir.clone(), &library, snapshot);
    let checkpoint_dir = temp.path().join("tapes");
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    cfg.checkpoint_journal_dir = checkpoint_dir.clone();
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    cfg.checkpoint_max_objects = 1;
    cfg.checkpoint_max_age_seconds = 3600;
    let audit_append_lock = Arc::clone(&cfg.audit_append_lock);
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: pool_id.to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.97,
        watermark_high: 0.98,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(BLOCK_SIZE),
        min_object_size_bytes: 0,
    };
    let session_id = open_actor_test_write_session(
        &drive_tx,
        &pool_cfg,
        tape_uuid,
        library_serial,
        barcode,
        &drive_uuid,
        drive_serial,
    )
    .await;
    let object_payload = vec![0x7d; 64 * BLOCK_SIZE as usize];
    append_actor_test_file_result(
        &drive_tx,
        session_id,
        temp.path().join("operator-terminal-source.bin"),
        "operator-terminal.bin",
        "operator-terminal-caller",
        &object_payload,
    )
    .await
    .expect("checkpoint one Object below low watermark");
    let (close_tx, close_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::Close {
            session_id,
            reply: close_tx,
        })
        .await
        .expect("send close");
    close_rx
        .await
        .expect("close reply")
        .expect("close below-low write session without finalizing");

    let checkpoint = remanence_state::authority::fixtures::AuthorityBuilder::new(
        temp.path().join("tapes"),
        tape_uuid,
    )
    .build()
    .expect("open checkpoint journal");
    let records = checkpoint.replay().expect("replay pre-final checkpoint");
    assert_eq!(records.len(), 1);
    assert!(!records[0].sealed_after_write);
    let capacity_blocks = crate::pool_write::raw_capacity_bytes(crate::pool_write::LtoGen::Lto9)
        / u64::from(BLOCK_SIZE);
    let low_watermark_blocks =
        remanence_state::watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_low)
            .expect("low watermark");
    assert!(records[0].eod_lba < low_watermark_blocks);

    let operation_id = Uuid::from_u128(0x7d01);
    let request = ManualFinalizeTapeActorRequest {
        candidate_operation_id: operation_id,
        actor: AuditActor::User("operator@example.invalid".to_string()),
        actor_fingerprint: "sha256:operator-test".to_string(),
        idempotency_key: Uuid::from_u128(0x7d02),
        request_fingerprint: [0x7D; 32],
        tape_uuid,
        expected_pool_id: Some(pool_id.to_string()),
        assignment_generation,
        reason: "ship partially filled copy offsite".to_string(),
        block_size: BLOCK_SIZE,
        parity_config: ParityConfig::None,
        pool_config: Some(pool_cfg.clone()),
    };
    let mut impossible = request.clone();
    impossible.candidate_operation_id = Uuid::from_u128(0x7d03);
    impossible.idempotency_key = Uuid::from_u128(0x7d04);
    impossible.request_fingerprint = [0x7E; 32];
    let impossible_policy = impossible.pool_config.as_mut().expect("pooled request");
    impossible_policy.watermark_low = 0.0001;
    impossible_policy.watermark_high = 0.0002;
    impossible_policy.capacity_cap_bytes = Some(8_241 * u64::from(BLOCK_SIZE));
    let commands_before_rejection = world.lock().expect("world lock").command_log.len();
    let mut rejected_index = CatalogIndex::open(&index_path).expect("open writable catalog");
    let rejected = preflight_manual_finalize_tape(
        &mut rejected_index,
        ManualFinalizePreflightConfig {
            mounted_endpoint: None,
            checkpoint_journal_dir: &checkpoint_dir,
            audit_dir: &audit_dir,
            audit_fsync: false,
            audit_append_lock: &audit_append_lock,
        },
        Some(barcode),
        &mut impossible,
    )
    .expect_err("exact close that cannot fit must fail");
    assert_eq!(
        rejected.code(),
        tonic::Code::ResourceExhausted,
        "{rejected:?}"
    );
    assert_eq!(
        world.lock().expect("world lock").command_log.len(),
        commands_before_rejection,
        "fit rejection must precede every drive command"
    );
    assert!(rejected_index
        .idempotency_scope_record(
            impossible.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            impossible.idempotency_key,
        )
        .expect("query rejected idempotency scope")
        .is_none());
    assert!(checkpoint
        .terminal_finalization_intent()
        .expect("query rejected terminal intent")
        .is_none());

    // Failed companion publication must roll back both halves of manual
    // acceptance. No drive command is possible because preflight owns
    // neither a drive nor a changer, and the scoped key remains reusable.
    let blocked_intent = remanence_state::authority::fixtures::block_finalization(&checkpoint)
        .expect("block intent temporary file creation");
    let commands_before_crash_cut = world.lock().expect("world lock").command_log.len();
    let mut crash_index = CatalogIndex::open(&index_path).expect("open crash-cut catalog");
    let crash_error = preflight_manual_finalize_tape(
        &mut crash_index,
        ManualFinalizePreflightConfig {
            mounted_endpoint: None,
            checkpoint_journal_dir: &checkpoint_dir,
            audit_dir: &audit_dir,
            audit_fsync: false,
            audit_append_lock: &audit_append_lock,
        },
        Some(barcode),
        &mut request.clone(),
    )
    .expect_err("intent publication failure rolls back acceptance");
    assert_eq!(crash_error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        world.lock().expect("world lock").command_log.len(),
        commands_before_crash_cut,
        "crash cut before intent must issue zero drive commands"
    );
    assert!(checkpoint
        .terminal_finalization_intent()
        .expect("read crash-cut intent")
        .is_none());
    assert!(crash_index
        .idempotency_scope_record(
            request.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            request.idempotency_key,
        )
        .expect("read crash-cut idempotency binding")
        .is_none());
    assert!(crash_index
        .terminal_finalization(&request.tape_uuid)
        .expect("read crash-cut finalization projection")
        .is_none());
    blocked_intent
        .unblock()
        .expect("unblock intent publication");

    let mut recovered_request = request.clone();
    let recovered_operation_id = Uuid::from_u128(0x7d05);
    recovered_request.candidate_operation_id = recovered_operation_id;
    let mut retry_index = CatalogIndex::open(&index_path).expect("open retry catalog");
    assert!(preflight_manual_finalize_tape(
        &mut retry_index,
        ManualFinalizePreflightConfig {
            mounted_endpoint: None,
            checkpoint_journal_dir: &checkpoint_dir,
            audit_dir: &audit_dir,
            audit_fsync: false,
            audit_append_lock: &audit_append_lock,
        },
        Some(barcode),
        &mut recovered_request,
    )
    .expect("identical crash retry rejoins")
    .is_none());
    assert_eq!(
        recovered_request.candidate_operation_id,
        recovered_operation_id
    );
    let durable = checkpoint
        .terminal_finalization_intent()
        .expect("read retry intent")
        .expect("retry published BeforeReplicaA intent");
    assert_eq!(
        durable.progress,
        remanence_state::TerminalFinalizationProgress::BeforeReplicaA
    );
    assert_eq!(
        durable
            .manual
            .as_ref()
            .expect("manual identity")
            .operation_id,
        *recovered_operation_id.as_bytes()
    );
    assert_eq!(
        durable
            .manual
            .as_ref()
            .expect("manual identity")
            .operation_kind,
        FINALIZE_TAPE_OPERATION_KIND
    );

    // Model process death after companion fsync but before the guarded
    // SQLite commit by rolling back both database halves while retaining
    // the exact BeforeReplicaA companion. Retry must retire only that
    // provisional companion, rebuild acceptance atomically, and move no
    // media.
    let raw = rusqlite::Connection::open(&index_path).expect("open acceptance rollback fixture");
    let rollback = raw
        .unchecked_transaction()
        .expect("begin acceptance rollback fixture");
    rollback
        .execute(
            "update tapes
                 set finalization_progress = null,
                     finalization_trigger = null,
                     finalization_operation_id = null,
                     finalization_edition_digest = null,
                     finalization_layout_digest = null,
                     completed_replicas = null,
                     finalization_outcome = null,
                     state = 'ready'
                 where tape_uuid = ?1",
            rusqlite::params![tape_uuid.to_vec()],
        )
        .expect("roll back finalization projection fixture");
    rollback
        .execute(
            "delete from idempotency_keys
                 where actor_fingerprint = ?1
                   and operation_kind = ?2
                   and idempotency_key = ?3",
            rusqlite::params![
                recovered_request.actor_fingerprint.as_str(),
                FINALIZE_TAPE_OPERATION_KIND,
                recovered_request.idempotency_key.to_string()
            ],
        )
        .expect("roll back idempotency projection fixture");
    rollback
        .commit()
        .expect("commit acceptance rollback fixture");
    drop(raw);
    assert!(checkpoint
        .terminal_finalization_intent()
        .expect("read provisional companion")
        .is_some());
    let commands_before_provisional_retry = world.lock().expect("world lock").command_log.len();
    let mut provisional_retry = recovered_request.clone();
    assert!(preflight_manual_finalize_tape(
        &mut retry_index,
        ManualFinalizePreflightConfig {
            mounted_endpoint: None,
            checkpoint_journal_dir: &checkpoint_dir,
            audit_dir: &audit_dir,
            audit_fsync: false,
            audit_append_lock: &audit_append_lock,
        },
        Some(barcode),
        &mut provisional_retry,
    )
    .expect("retry repairs provisional companion")
    .is_none());
    assert_eq!(
        world.lock().expect("world lock").command_log.len(),
        commands_before_provisional_retry,
        "provisional acceptance retry must issue zero drive commands"
    );
    assert!(retry_index
        .idempotency_scope_record(
            recovered_request.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            recovered_request.idempotency_key,
        )
        .expect("read repaired idempotency binding")
        .is_some());
    assert!(retry_index
        .terminal_finalization(&tape_uuid)
        .expect("read repaired finalization projection")
        .is_some());

    let mut changed_request = recovered_request.clone();
    changed_request.reason = "different exact reason bytes".to_string();
    changed_request.request_fingerprint = [0x7F; 32];
    let changed = preflight_manual_finalize_tape(
        &mut retry_index,
        ManualFinalizePreflightConfig {
            mounted_endpoint: None,
            checkpoint_journal_dir: &checkpoint_dir,
            audit_dir: &audit_dir,
            audit_fsync: false,
            audit_append_lock: &audit_append_lock,
        },
        Some(barcode),
        &mut changed_request,
    )
    .expect_err("changed request conflicts with durable binding");
    assert_eq!(changed.code(), tonic::Code::AlreadyExists);
    assert_eq!(
        world.lock().expect("world lock").command_log.len(),
        commands_before_crash_cut,
        "changed retry conflict must issue zero drive commands"
    );

    retry_index
        .set_append_recovery_required(tape_uuid, true)
        .unwrap();
    let before_open = world.lock().unwrap().command_log.len();
    let (reply, rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenWrite {
            actor: AuditActor::System,
            pool_cfg: pool_cfg.clone(),
            selected: SelectedTape {
                standalone_endpoint: None,
                pool_id: pool_id.into(),
                tape_uuid,
                block_size: BLOCK_SIZE,
                parity_config: ParityConfig::None,
            },
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPinnedTape,
            needs_drive_load: false,
            library_serial: library_serial.into(),
            barcode: Some(barcode.into()),
            source_slot: None,
            drive_uuid: None,
            drive_serial: None,
            reply,
        })
        .await
        .unwrap();
    let refused = rx.await.unwrap().unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    assert!(refused.message().contains("FinalizeTape"), "{refused}");
    assert_eq!(world.lock().unwrap().command_log.len(), before_open);

    let (finalize_tx, finalize_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::FinalizeTape {
            request: recovered_request.clone(),
            needs_drive_load: false,
            library_serial: library_serial.to_string(),
            barcode: Some(barcode.to_string()),
            source_slot: None,
            drive_uuid: Some(drive_uuid.clone()),
            drive_serial: Some(drive_serial.to_string()),
            reply: finalize_tx,
        })
        .await
        .expect("send manual finalize");
    let first = finalize_rx
        .await
        .expect("manual finalize reply")
        .expect("manual finalization below low succeeds");
    assert_eq!(first.operation_id, recovered_operation_id);
    assert_eq!(
        first.projection.outcome,
        TerminalFinalizationOutcome::Finalized
    );
    assert_eq!(
        first.projection.trigger,
        remanence_state::TerminalFinalizationTrigger::OperatorCloseOut
    );
    let records_after_first = world
        .lock()
        .expect("world lock")
        .tapes
        .get(barcode)
        .expect("virtual tape")
        .records
        .len();

    let (replay_tx, replay_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::FinalizeTape {
            request: recovered_request,
            needs_drive_load: false,
            library_serial: library_serial.to_string(),
            barcode: Some(barcode.to_string()),
            source_slot: None,
            drive_uuid: Some(drive_uuid),
            drive_serial: Some(drive_serial.to_string()),
            reply: replay_tx,
        })
        .await
        .expect("send idempotent replay");
    let replay = replay_rx
        .await
        .expect("idempotent replay reply")
        .expect("same-key replay returns completed operation");
    assert_eq!(replay, first);
    assert_eq!(
        world
            .lock()
            .expect("world lock")
            .tapes
            .get(barcode)
            .expect("virtual tape")
            .records
            .len(),
        records_after_first,
        "same-key replay must not append another terminal component"
    );

    let final_records = checkpoint.replay().expect("replay sealed checkpoint");
    assert_eq!(final_records.len(), 2);
    assert!(final_records[1].sealed_after_write);
    assert_eq!(
        final_records[1]
            .terminal_finalization
            .as_ref()
            .expect("durable terminal intent")
            .manual
            .as_ref()
            .expect("manual operation identity")
            .reason,
        "ship partially filled copy offsite"
    );

    // Recreate the live post-sealed-checkpoint/pre-SQLite window without
    // restarting the daemon. The durable sealed record must repair the
    // stale projection before any fence or media-capable path is consulted.
    let commands_before_host_repair = world.lock().expect("world lock").command_log.len();
    let mut fenced_index = CatalogIndex::open(&index_path).expect("open fenced catalog");
    fenced_index
        .record_tape_io_fence(remanence_state::TapeIoFenceInput {
            tape_uuid,
            barcode: Some(barcode.to_string()),
            reason: "post_replica_c_host_projection".to_string(),
            evidence_json: None,
        })
        .expect("record post-C fence");
    drop(fenced_index);
    rusqlite::Connection::open(&index_path)
            .expect("open raw projection fixture")
            .execute(
                "update tapes set state = 'finalizing', finalization_outcome = 'in_progress' where tape_uuid = ?1",
                rusqlite::params![tape_uuid.to_vec()],
            )
            .expect("downgrade only the disposable SQLite projection");
    let mut host_index = CatalogIndex::open(&index_path).expect("reopen stale projection");
    let mut host_retry = request.clone();
    let repaired = preflight_manual_finalize_tape(
        &mut host_index,
        ManualFinalizePreflightConfig {
            mounted_endpoint: None,
            checkpoint_journal_dir: &checkpoint_dir,
            audit_dir: &audit_dir,
            audit_fsync: false,
            audit_append_lock: &audit_append_lock,
        },
        Some(barcode),
        &mut host_retry,
    )
    .expect("sealed checkpoint repairs stale SQLite despite active fence")
    .expect("sealed retry completes in host preflight");
    assert_eq!(
        repaired.projection.outcome,
        TerminalFinalizationOutcome::Finalized
    );
    assert_eq!(
        world.lock().expect("world lock").command_log.len(),
        commands_before_host_repair,
        "sealed host repair must issue zero drive commands"
    );
    let audit = FileAuditLog::replay(&audit_dir).expect("replay manual finalization audit");
    assert_eq!(
        audit
            .iter()
            .filter(|record| {
                record.event == AuditEvent::OperationFinished
                    && record.operation_id == Some(recovered_operation_id)
            })
            .count(),
        1,
        "manual finalization and every sealed retry share one completion event"
    );
    assert_eq!(
        audit
            .iter()
            .filter(|record| {
                record.event == AuditEvent::TapeSealed
                    && record.subject.kind == "tape"
                    && record.subject.id.as_deref()
                        == Some(crate::bytes_to_hex(tape_uuid.as_slice()).as_str())
            })
            .count(),
        1,
        "manual finalization and every sealed retry share one TapeSealed event"
    );
}

/// Mount-dispatched explicit checkpoints must return catalog-projected copies after reopening
/// a session, including a catalog replay whose append acknowledgement is already durable.
#[tokio::test]
async fn sequential_sessions_and_replay_return_catalog_copies_through_mount() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    let temp = tempfile::Builder::new()
        .prefix("remanence-sequential-batch-one")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let tape_uuid = [0x77; 16];
    let mut index = CatalogIndex::open(&index_path).expect("open catalog");
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: "batch-one-test".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: "CHK003L9".to_string(),
            block_size: BLOCK_SIZE,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_tape_pool_membership(tape_uuid, "batch-one-test")
        .expect("assign pool");
    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: "DRV-BATCH-ONE".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("LIB-BATCH-ONE".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-07-21T00:00:00Z".to_string()),
        })
        .expect("observe drive")
        .drive_uuid;
    drop(index);

    let bootstrap = BootstrapPayload {
        scheme: None,
        no_parity_flag: true,
        filemark_map_digest: None,
        tape_uuid,
        written_by_version: "test".to_string(),
        written_at: "2026-07-21T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: BLOCK_SIZE,
        drive_compression: false,
    };
    let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
    write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK_SIZE);
    tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
    tape.written_bytes = u64::from(BLOCK_SIZE);
    let mut world = VirtualWorld::single_drive("LIB-BATCH-ONE", 0x0100, "DRV-BATCH-ONE", 0x0400, 1);
    world.put_tape_in_drive(0x0100, "CHK003L9", Some(0x0400), tape);
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let mut cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
    cfg.checkpoint_journal_dir = temp.path().join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    cfg.checkpoint_max_objects = 2;
    cfg.checkpoint_max_age_seconds = 3600;
    let library_serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([library_serial.as_str()]);
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: "batch-one-test".to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(BLOCK_SIZE),
        min_object_size_bytes: 0,
    };
    let (changer_tx, _changer_rx) = mpsc::channel(1);
    let reservations = Arc::new(HashMap::from([(0x0100, AtomicBool::new(false))]));
    let pool = DrivePool::new_for_library(
        library_serial.as_str(),
        changer_tx,
        HashMap::from([(0x0100, drive_tx.clone())]),
        reservations,
    );
    let state_index = CatalogIndex::open(&index_path).expect("open mount test catalog");
    let mut state = crate::ApiState::new_with_pool_configs(state_index, [pool_cfg.clone()]);
    state.drive_pool = Some(pool.clone());

    let mut previous_session_id = None;
    for (session_ordinal, expected_tape_file_number) in [(1u8, 1u64), (2, 2)] {
        let session_id = open_actor_test_write_session(
            &drive_tx,
            &pool_cfg,
            tape_uuid,
            library_serial.as_str(),
            "CHK003L9",
            &drive_uuid,
            "DRV-BATCH-ONE",
        )
        .await;
        assert_ne!(Some(session_id), previous_session_id);
        previous_session_id = Some(session_id);
        pool.record_session(
            session_id,
            MountedSession {
                bay: 0x0100,
                library_serial: library_serial.clone(),
                barcode: Some("CHK003L9".to_string()),
                home_slot: Some(0x0400),
                tape_uuid,
                drive_uuid: Some(drive_uuid.clone()),
            },
        );
        let source_path = temp
            .path()
            .join(format!("batch-one-source-{session_ordinal}.bin"));
        std::fs::write(&source_path, format!("batch-one payload {session_ordinal}"))
            .expect("write mount append source");
        let append = crate::mount::append_finish(
            &state,
            session_id,
            crate::mount::AppendFinishRequest {
                staging_reason: "configured_serial",
                mtime: None,
                content_sha256: Sha256::digest(std::fs::read(&source_path).unwrap()).into(),
                received_bytes: std::fs::metadata(&source_path).unwrap().len(),
                spool_path: source_path,
                archive_path: PathBuf::from(format!("payload-{session_ordinal}.bin")),
                caller_object_id: format!("batch-one-caller-{session_ordinal}"),
                expected_content_sha256: None,
                expected_object_id: None,
                input_kind: crate::WriteObjectInputKind::LogicalFile,
            },
        )
        .await
        .expect("append through mount dispatcher");
        let written_info = append
            .append_commit_info
            .as_ref()
            .expect("batch-of-one WRITTEN append info");
        assert_eq!(
            written_info.durability,
            pb::AppendDurability::Written as i32
        );
        assert!(append.copies.is_empty());

        let checkpoint =
            crate::mount::checkpoint_write_session(&state, session_id, CheckpointTrigger::Explicit)
                .await
                .expect("explicit checkpoint through mount dispatcher");
        assert_eq!(checkpoint.committed_objects.len(), 1);
        let committed = &checkpoint.committed_objects[0];
        assert_eq!(committed.object_id, append.object_id);
        let committed_info = committed
            .append_commit_info
            .as_ref()
            .expect("batch-of-one CHECKPOINTED append info");
        assert_eq!(
            committed_info.durability,
            pb::AppendDurability::Checkpointed as i32
        );
        assert_eq!(committed.copies.len(), 1);
        assert_eq!(committed.copies[0].tape_uuid, tape_uuid);
        assert_eq!(
            committed.copies[0].tape_file_number,
            expected_tape_file_number
        );

        let (close_tx, close_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Close {
                session_id,
                reply: close_tx,
            })
            .await
            .expect("send actor test write close");
        let closed = close_rx
            .await
            .expect("actor test write close reply")
            .expect("close actor test write session");
        assert!(closed.session.checkpointed_objects.is_empty());
        assert!(closed.session.committed_copies.is_empty());
        pool.forget_session(session_id);
    }

    let replay_session_id = open_actor_test_write_session(
        &drive_tx,
        &pool_cfg,
        tape_uuid,
        library_serial.as_str(),
        "CHK003L9",
        &drive_uuid,
        "DRV-BATCH-ONE",
    )
    .await;
    pool.record_session(
        replay_session_id,
        MountedSession {
            bay: 0x0100,
            library_serial,
            barcode: Some("CHK003L9".to_string()),
            home_slot: Some(0x0400),
            tape_uuid,
            drive_uuid: Some(drive_uuid),
        },
    );
    let replay_source = temp.path().join("batch-one-replay-source.bin");
    std::fs::write(&replay_source, "batch-one payload 1").expect("write replay source");
    let replay = crate::mount::append_finish(
        &state,
        replay_session_id,
        crate::mount::AppendFinishRequest {
            staging_reason: "configured_serial",
            mtime: None,
            content_sha256: Sha256::digest(std::fs::read(&replay_source).unwrap()).into(),
            received_bytes: std::fs::metadata(&replay_source).unwrap().len(),
            spool_path: replay_source,
            archive_path: PathBuf::from("payload-1.bin"),
            caller_object_id: "batch-one-caller-1".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
        },
    )
    .await
    .expect("replay append through mount dispatcher");
    assert_eq!(
        replay
            .append_commit_info
            .as_ref()
            .expect("catalog replay append info")
            .durability,
        pb::AppendDurability::Checkpointed as i32
    );
    assert_eq!(replay.copies.len(), 1);
    assert_eq!(replay.copies[0].tape_file_number, 1);

    let replay_checkpoint = crate::mount::checkpoint_write_session(
        &state,
        replay_session_id,
        CheckpointTrigger::Explicit,
    )
    .await
    .expect("explicit replay checkpoint through mount dispatcher");
    assert_eq!(
        replay_checkpoint.committed_objects.len(),
        1,
        "catalog replay must remain claimable by the explicit checkpoint"
    );
    assert_eq!(
        replay_checkpoint.committed_objects[0].object_id,
        replay.object_id
    );
    assert_eq!(replay_checkpoint.committed_objects[0].copies.len(), 1);
    assert_eq!(
        replay_checkpoint.committed_objects[0].copies[0].tape_file_number,
        1
    );

    let claimed_again = crate::mount::checkpoint_write_session(
        &state,
        replay_session_id,
        CheckpointTrigger::Explicit,
    )
    .await
    .expect("repeat explicit replay checkpoint through mount dispatcher");
    assert!(
        claimed_again.committed_objects.is_empty(),
        "a replay receipt must be returned by exactly one explicit checkpoint"
    );

    let (close_tx, close_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::Close {
            session_id: replay_session_id,
            reply: close_tx,
        })
        .await
        .expect("send replay session close");
    close_rx
        .await
        .expect("replay session close reply")
        .expect("close replay session");
    pool.forget_session(replay_session_id);
}

struct RangeCatalogFixture {
    index: CatalogIndex,
    _temp: tempfile::TempDir,
    blocks: Vec<Vec<u8>>,
    layout: RemTarObjectLayout,
}

fn range_options(block_size: usize) -> RemTarObjectOptions {
    let mut opts = RemTarObjectOptions::new(
        RANGE_OBJECT_ID,
        "caller-range",
        "2026-06-16T12:00:00Z",
        "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
    );
    opts.chunk_size = block_size;
    opts
}

fn cataloged_payload_fixture(payload: &[u8]) -> RangeCatalogFixture {
    let opts = range_options(512);
    let files = [RemTarFile {
        path: "payload.rem-object",
        file_id: "payload-file",
        data: payload,
        mtime: Some("0"),
        executable: Some(false),
    }];
    let mut sink = VecBlockSink::new();
    let layout = write_rem_tar_object(&mut sink, &opts, &files).expect("write wrapped payload");
    let payload_layout = &layout.files[0];
    let temp = tempfile::Builder::new()
        .prefix("remanence-api-range-test-")
        .tempdir()
        .expect("tempdir");
    let mut index =
        CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: RANGE_TAPE_UUID,
            voltag: "RANGE01".to_string(),
            block_size: opts.chunk_size as u32,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_native_object_and_committed_tape_file_bundle(
            NativeObjectProjectionInput {
                object_id: RANGE_OBJECT_ID.to_string(),
                caller_object_id: Some("caller-range".to_string()),
                body_format: "rem-object-v1".to_string(),
                logical_size_bytes: Some(payload.len() as u64),
                content_hash: payload_layout.file_sha256.map(|hash| hash.to_vec()),
                metadata_hash: None,
                created_at_utc: Some("2026-06-16T12:00:00Z".to_string()),
            },
            &[NativeObjectFileProjectionInput {
                object_id: RANGE_OBJECT_ID.to_string(),
                file_id: "payload-file".to_string(),
                path: "payload.rem-object".to_string(),
                size_bytes: payload.len() as u64,
                file_sha256: payload_layout
                    .file_sha256
                    .expect("regular payload hash")
                    .to_vec(),
                first_chunk_lba: payload_layout.first_chunk_lba.map(|lba| lba.0),
                chunk_count: payload_layout.chunk_count,
                mtime: Some("0".to_string()),
                executable: Some(false),
            }],
            &[NativeObjectCopyProjectionInput {
                object_id: RANGE_OBJECT_ID.to_string(),
                tape_uuid: RANGE_TAPE_UUID,
                tape_file_number: 0,
                first_body_lba: 0,
                first_parity_data_ordinal: None,
                protected_until_ordinal: None,
                status: "committed".to_string(),
                representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                recipient_epoch_ids: None,
                metadata_frame_len: None,
                plaintext_digest: None,
                stored_digest: None,
            }],
            TapeJournalIndexInput {
                tape_uuid: RANGE_TAPE_UUID,
                block_size: opts.chunk_size as u32,
                scheme: None,
                journal_offset_bytes: 0,
            },
            &(&CommittedBundle {
                kind: CommittedBundleKind::Object,
                entries: vec![TapeFileEntry {
                    tape_file_number: 0,
                    kind: TapeFileKind::Object,
                    block_count: layout.projected_size_blocks,
                    physical_start_hint: Some(0),
                    object_id: Some(RANGE_OBJECT_ID.to_string()),
                    first_parity_data_ordinal: None,
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: None,
                    object_recovery_row: None,
                }],
                highest_protected_ordinal: 0,
                total_committed_ordinals: 0,
            })
                .try_into()
                .unwrap(),
        )
        .expect("project range fixture");
    RangeCatalogFixture {
        index,
        _temp: temp,
        blocks: sink.blocks,
        layout,
    }
}

#[test]
fn ranged_absolute_lba_derives_from_dense_filemark_prefix() {
    let tape_uuid = RANGE_TAPE_UUID.to_vec();
    let files = [
        TapeFileRecord {
            tape_uuid: tape_uuid.clone(),
            tape_file_number: 0,
            kind: "bootstrap".to_string(),
            block_count: 1,
            object_id: None,
            canonical_metadata_hash: None,
            canonical_metadata_hash_algorithm: None,
        },
        TapeFileRecord {
            tape_uuid: tape_uuid.clone(),
            tape_file_number: 1,
            kind: "object".to_string(),
            block_count: 10,
            object_id: Some("first".to_string()),
            canonical_metadata_hash: None,
            canonical_metadata_hash_algorithm: None,
        },
        TapeFileRecord {
            tape_uuid,
            tape_file_number: 2,
            kind: "object".to_string(),
            block_count: 3,
            object_id: Some("target".to_string()),
            canonical_metadata_hash: None,
            canonical_metadata_hash_algorithm: None,
        },
    ];
    assert_eq!(derive_physical_file_start_lba(&files, 2), Some(13));

    let mut incomplete = files.to_vec();
    incomplete.remove(1);
    assert_eq!(
        derive_physical_file_start_lba(&incomplete, 2),
        None,
        "a non-dense prefix must use the logical fallback"
    );
}

async fn collect_stream_chunks(
    mut rx: crate::read_core::ReadStreamReceiver,
) -> Result<Vec<u8>, Status> {
    let mut bytes = Vec::new();
    let mut saw_last = false;
    while let Some(item) = rx.next().await {
        let chunk = item?;
        bytes.extend_from_slice(&chunk.data);
        saw_last |= chunk.is_last;
        if chunk.is_last {
            break;
        }
    }
    assert!(saw_last, "range stream must send terminal frame");
    Ok(bytes)
}

#[tokio::test]
async fn l3_read_actor_batches_are_consumed_by_staged_sender() {
    let (tx, rx) = crate::read_core::read_stream_channel(4);

    let diagnostics = stream_with_staged_read_sender_diagnostics(tx, 4, |writer, _| {
        std::io::Write::write_all(writer, b"abcdef")
            .map_err(|err| Status::internal(format!("write staged bytes: {err}")))?;
        std::io::Write::write_all(writer, b"gh")
            .map_err(|err| Status::internal(format!("write staged bytes: {err}")))?;
        Ok(())
    })
    .expect("staged read sender succeeds");
    assert_eq!(diagnostics.bytes, 8);

    let bytes = collect_stream_chunks(rx)
        .await
        .expect("collect staged read stream");
    assert_eq!(bytes, b"abcdefgh");
}

#[tokio::test]
async fn staged_sender_surfaces_full_channel_stall_time() {
    let requested_chunk =
        u32::try_from(crate::read_core::READ_STREAM_CHANNEL_BYTE_BUDGET + 1).unwrap();
    let (tx, rx) = crate::read_core::read_stream_channel(requested_chunk as usize);
    let sender = tokio::task::spawn_blocking(move || {
        stream_with_staged_read_sender_diagnostics(tx, requested_chunk, |writer, _| {
            std::io::Write::write_all(writer, b"a")
                .map_err(|err| Status::internal(format!("write first byte: {err}")))?;
            std::io::Write::write_all(writer, b"b")
                .map_err(|err| Status::internal(format!("write second byte: {err}")))?;
            Ok(())
        })
    });
    tokio::time::sleep(StdDuration::from_millis(10)).await;
    let bytes = collect_stream_chunks(rx)
        .await
        .expect("drain staged stream");
    let diagnostics = sender
        .await
        .expect("sender task joins")
        .expect("staged sender succeeds");

    assert_eq!(bytes, b"ab");
    assert!(
        diagnostics.sender_stall >= StdDuration::from_millis(5),
        "full-channel wait must surface in restore diagnostics: {:?}",
        diagnostics.sender_stall
    );
}

#[tokio::test]
async fn l3_read_sink_error_drains_without_hanging_actor_writer() {
    let (tx, rx) = crate::read_core::read_stream_channel(1);
    drop(rx);

    let err = stream_with_staged_read_sender_diagnostics(tx, 1, |writer, _| {
        for _ in 0..8 {
            std::io::Write::write_all(writer, b"x").map_err(|err| {
                Status::internal(format!("actor observed staged sender failure: {err}"))
            })?;
        }
        Ok(())
    })
    .expect_err("closed gRPC receiver must fail staged sender");

    assert!(
        err.message().contains("read stream closed")
            || err.message().contains("staged read sender failed"),
        "sink error should be surfaced, got {err}"
    );
}

async fn stream_fixture_range(
    fixture: &RangeCatalogFixture,
    file_id: &str,
    start_byte: u64,
    end_byte: u64,
) -> Result<Vec<u8>, Status> {
    let request = file_range_read_request(
        &fixture.index,
        &RANGE_TAPE_UUID,
        RANGE_OBJECT_ID,
        file_id,
        start_byte,
        end_byte,
    )?;
    let mut source = VecBlockSource::new(fixture.blocks.clone());
    let (tx, rx) = crate::read_core::read_stream_channel(256);
    stream_file_range_from_source(
        &mut source,
        request,
        0,
        tx,
        &TapeIoConfig::default(),
        test_io_memory(),
    )?;
    collect_stream_chunks(rx).await
}

#[test]
fn append_gate_poisons_session_after_failed_append() {
    let mut gate = SessionAppendGate::default();
    assert!(gate.check().is_ok(), "fresh session must accept appends");

    gate.record_failure();

    let status = gate.check().expect_err("poisoned gate must refuse");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(status.message().contains("poisoned"));
    // Poisoning is permanent for the session's lifetime.
    assert!(gate.check().is_err());
}

#[test]
fn channel_and_command_bounds_hold() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}
    assert_send_sync::<mpsc::Sender<ChangerCommand>>();
    assert_send::<ChangerCommand>();
    assert_send_sync::<mpsc::Sender<DriveCommand>>();
    assert_send::<DriveCommand>();
    assert_send_sync::<mpsc::Sender<Result<pb::BytesChunk, Status>>>();
}

#[tokio::test]
async fn changer_move_succeeds_and_publishes_snapshot_when_catalog_observation_fails() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-api-move-observe-failure")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    CatalogIndex::open(&index_path).expect("create catalog");
    let sqlite = rusqlite::Connection::open(&index_path).expect("open raw sqlite");
    sqlite
        .execute_batch(
            "create trigger fail_drive_observation
                 before insert on drives
                 begin
                   select raise(fail, 'injected drive catalog observation failure');
                 end;",
        )
        .expect("install observation failure trigger");
    drop(sqlite);

    let mut world = VirtualWorld::single_drive("LIB_MOVE_OBS_FAIL", 0x0100, "DRV_A", 0x0400, 1);
    world.put_tape_in_slot(0x0400, "TAPE_MOVE", VirtualTape::default());
    let handle = open_model_library(Arc::new(Mutex::new(world)));
    let library = handle.library().clone();
    let snapshot_cell = library_snapshot_cell(library.clone());
    let changer = handle.into_changer();
    let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
    let cfg = WriteOwnerConfig {
        index_path: index_path.clone(),
        report: DiscoveryReport {
            unattached_drives: Vec::new(),
            complete: true,
            incomplete_evidence: Vec::new(),
            libraries: vec![library.clone()],
            warnings: Vec::new(),
        },
        policy,
        audit_dir: temp.path().join("audit"),
        audit_fsync: false,
        audit_append_lock: Arc::new(std::sync::Mutex::new(())),
        reservations: Arc::new(HashMap::new()),
        placement: crate::write_owner::ActorPlacement::Library(
            crate::write_owner::LibraryActorContext {
                serial: library.serial.clone(),
                snapshot: Arc::clone(&snapshot_cell),
            },
        ),
        snapshot_miss_alarm: 1,
        managed_library_serials: Arc::new(HashSet::from([library.serial.clone()])),
        cleaning: remanence_state::CleaningConfig::default(),
        tape_io: remanence_state::TapeIoConfig::default(),
        tape_pools: Vec::new(),
        verification_route: remanence_parity::VerificationRoute::Walk,
        io_memory: test_io_memory(),
        write_admissions: WriteAdmissionCoordinator::default(),
        checkpoint_journal_dir: {
            let p = temp.path().join("tapes");
            std::fs::create_dir_all(&p).unwrap();
            p
        },
        checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
        checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
        checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
        session_idle_seconds: 1800,
        lifecycle: None,
        changer_ready_wait: remanence_library::ChangerReadyWait::default(),
        calibration_store: remanence_state::CalibrationControlStore::open(
            temp.path().join("calibration"),
        )
        .expect("open test calibration store"),
    };
    let actor = spawn_changer_actor(changer, cfg);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

    actor
        .send(ChangerCommand::Move {
            src: 0x0400,
            dst: 0x0100,
            recovery_guard: None,
            caller_gone: None,
            reply: reply_tx,
        })
        .await
        .expect("send move command");
    let result = reply_rx.await.expect("move reply");

    assert!(
        result.is_ok(),
        "physical move success must not be converted to failure by catalog observation: {result:?}"
    );
    let published = snapshot_cell
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    let published_library = published
        .report
        .libraries
        .iter()
        .find(|candidate| candidate.serial == library.serial)
        .expect("published library");
    let bay = &published_library.drive_bays[0];
    assert!(bay.loaded, "published snapshot must include the moved tape");
    assert_eq!(bay.loaded_tape.as_deref(), Some("TAPE_MOVE"));
    assert_eq!(bay.source_slot, Some(0x0400));
    assert!(!published_library.slots[0].full);

    let alarm_key = library_snapshot_persist_alarm_key(library.serial.as_str());
    let alarm = CatalogIndex::open(&index_path)
        .expect("reopen catalog")
        .get_alarm(alarm_key.as_str())
        .expect("lookup alarm")
        .expect("observation failure alarm");
    assert_eq!(alarm.kind, "snapshot-persist-failing");
    assert_eq!(alarm.state, "open");
    assert!(
        alarm
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("injected drive catalog observation failure")),
        "alarm detail must surface the observation failure: {alarm:?}"
    );
}

#[test]
fn spool_enforces_size_cap() {
    let dir = std::env::temp_dir().join(format!("remanence-spool-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create spool test dir");
    let mut spool = Spool::create(&dir, 4).expect("create spool");
    assert!(spool.path().exists());
    assert!(spool.write_chunk(b"ab").is_ok());
    assert!(spool.write_chunk(b"cde").is_err());
    let path = spool.path().to_path_buf();
    drop(spool);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn spool_removes_unfinished_file_on_drop() {
    let dir = std::env::temp_dir().join(format!("remanence-spool-drop-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("create spool test dir");
    let path = {
        let mut spool = Spool::create(&dir, 4).expect("create spool");
        spool.write_chunk(b"ab").expect("write chunk");
        spool.path().to_path_buf()
    };
    assert!(!path.exists());
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn process_loss_without_drop_leaves_owned_spool_for_startup_reconciliation() {
    let dir = std::env::temp_dir().join(format!(
        "remanence-spool-process-loss-test-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&dir).expect("create spool test dir");
    let mut spool = Spool::create(&dir, 16).expect("create spool");
    spool.write_chunk(b"orphan").expect("write orphan bytes");
    let path = spool.path().to_path_buf();

    std::mem::forget(spool);

    assert!(path.exists(), "process loss bypasses Spool::drop");
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("UTF-8 spool name");
    assert!(name.starts_with("spool-") && name.ends_with(".bin"));
    std::fs::remove_file(path).expect("remove simulated orphan");
    std::fs::remove_dir(dir).expect("remove spool test dir");
}

#[test]
fn session_protos_include_drive_element_address() {
    let session_id = Uuid::from_u128(0x5E5510);
    let tape_uuid = [0xAB; 16];
    let opened_at = "2026-06-10T12:00:00Z";

    let write = session_proto(WriteSessionProtoInput {
        session_id,
        tape_uuid: &tape_uuid,
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
        state: pb::write_session::State::WriteSessionStateOpen,
        objects_committed: 0,
        bytes_committed: 0,
        opened_at_utc: opened_at,
        last_checkpoint_at_utc: None,
        drive_element_address: Some(0x0100),
        pending_batch: None,
    });
    let read = read_session_proto(
        session_id,
        &tape_uuid,
        pb::read_session::State::ReadSessionStateOpen,
        opened_at,
        0x0101,
        None,
        7,
    );

    assert_eq!(write.drive_element_address, Some(0x0100));
    assert_eq!(read.drive_element_address, Some(0x0101));
    assert_eq!(read.daemon_epoch, 7);
}

fn resume_target_for_fixture(fixture: &RangeCatalogFixture) -> ReadResumeTarget {
    let first_chunk_lba = fixture.layout.files[0]
        .first_chunk_lba
        .expect("fixture file has a body-chunk boundary")
        .0;
    ReadResumeTarget {
        tape_uuid: RANGE_TAPE_UUID,
        object_id: RANGE_OBJECT_ID.to_string(),
        file_id: "payload-file".to_string(),
        file_boundary_byte_offset: first_chunk_lba * 512,
        expected_position_lba: Some(first_chunk_lba),
        prior_daemon_epoch: Some(11),
    }
}

#[test]
fn cold_resume_relocates_returns_proof_and_mints_fresh_session() {
    let fixture = cataloged_payload_fixture(b"cold resume payload");
    let target = resume_target_for_fixture(&fixture);
    let request = file_range_read_request(
        &fixture.index,
        &target.tape_uuid,
        target.object_id.as_str(),
        target.file_id.as_str(),
        0,
        0,
    )
    .expect("resolve durable resume coordinates");

    let first_session_id = Uuid::new_v4();
    let first = read_session_proto(
        first_session_id,
        &target.tape_uuid,
        pb::read_session::State::ReadSessionStateOpen,
        "2026-07-12T00:00:00Z",
        0x0101,
        None,
        target.prior_daemon_epoch.expect("prior epoch"),
    );
    drop(first);

    let mut cold_source = VecBlockSource::new(fixture.blocks.clone());
    let proof_lba = position_read_resume_from_source(&mut cold_source, request, &target)
        .expect("cold resume position proof");
    let resumed_session_id = Uuid::new_v4();
    let resumed = read_session_proto(
        resumed_session_id,
        &target.tape_uuid,
        pb::read_session::State::ReadSessionStateOpen,
        "2026-07-12T00:01:00Z",
        0x0101,
        Some(proof_lba),
        12,
    );

    assert_ne!(resumed_session_id, first_session_id);
    assert_eq!(resumed.session_id, resumed_session_id.as_bytes());
    assert_eq!(resumed.daemon_epoch, 12);
    assert_eq!(
        resumed
            .position_proof
            .expect("resume open returns proof")
            .position_after_lba,
        target.expected_position_lba.expect("expected LBA")
    );
    assert!(cold_source.calls.iter().any(|call| matches!(
        call,
        VecBlockSourceCall::Space {
            kind: SpaceKind::Blocks,
            ..
        }
    )));
}

#[test]
fn wrong_tape_is_rejected_before_position_even_at_matching_lba() {
    let fixture = cataloged_payload_fixture(b"wrong tape position collision");
    let actual_tape_uuid = RANGE_TAPE_UUID;
    let requested_tape_uuid = [0xCD; 16];
    let payload = BootstrapPayload {
        scheme: None,
        no_parity_flag: true,
        filemark_map_digest: None,
        tape_uuid: actual_tape_uuid,
        written_by_version: "test".to_string(),
        written_at: "2026-07-12T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: 4096,
        drive_compression: false,
    };
    let mut block = vec![0u8; 4096];
    write_bootstrap_block(&payload, &mut block).expect("write wrong-tape bootstrap");
    let mut target = resume_target_for_fixture(&fixture);
    target.tape_uuid = requested_tape_uuid;
    let request = file_range_read_request(
        &fixture.index,
        &actual_tape_uuid,
        target.object_id.as_str(),
        target.file_id.as_str(),
        0,
        0,
    )
    .expect("resolve colliding physical position");
    let expected_lba = target.expected_position_lba.expect("expected LBA");
    let mut blocks = vec![block];
    blocks.resize_with(expected_lba as usize + 1, || vec![0u8; 512]);
    let mut source = VecBlockSource::new(blocks);

    let error = verify_and_position_read_resume_from_source(&mut source, request, &target)
        .expect_err("wrong tape must fail before trusting its matching LBA");

    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert!(error.message().contains("tape identity mismatch"));
    assert_eq!(
        source.cursor(),
        1,
        "identity read stops at an LBA from which the expected proof was reachable"
    );
    assert!(source.calls.iter().all(|call| !matches!(
        call,
        VecBlockSourceCall::Space { .. } | VecBlockSourceCall::Position
    )));
}

#[test]
fn resume_rejects_mid_file_offset_without_positioning() {
    let fixture = cataloged_payload_fixture(b"file-boundary payload");
    let mut target = resume_target_for_fixture(&fixture);
    target.file_boundary_byte_offset += 1;
    let request = file_range_read_request(
        &fixture.index,
        &target.tape_uuid,
        target.object_id.as_str(),
        target.file_id.as_str(),
        0,
        0,
    )
    .expect("resolve durable resume coordinates");
    let mut source = VecBlockSource::new(fixture.blocks.clone());

    let error = position_read_resume_from_source(&mut source, request, &target)
        .expect_err("mid-file resume must fail");

    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    assert!(error.message().contains("file boundary"));
    assert!(
        source.calls.is_empty(),
        "invalid offset must not move the tape"
    );
}

#[test]
fn serialized_resume_token_contains_no_session_id() {
    let persisted_session_id = [0xEE; 16];
    let token = pb::ReadResumeTarget {
        tape_uuid: RANGE_TAPE_UUID.to_vec(),
        object_id: Uuid::parse_str(RANGE_OBJECT_ID)
            .expect("object UUID")
            .as_bytes()
            .to_vec(),
        file_id: b"payload-file".to_vec(),
        file_boundary_byte_offset: 1024,
        expected_position_lba: Some(17),
        daemon_epoch: Some(41),
    };

    let encoded = token.encode_to_vec();

    assert!(
        !encoded
            .windows(persisted_session_id.len())
            .any(|window| window == persisted_session_id),
        "the durable resume token must not serialize a session id"
    );
}

#[test]
fn pool_write_status_maps_nested_input_and_capacity_errors() {
    let invalid = status_from_pool_write_error(PoolWriteError::Streaming(
        StreamingError::InvalidInput("bad archive path".to_string()),
    ));
    assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
    let invalid_prefix = status_from_pool_write_error(PoolWriteError::Streaming(
        StreamingError::InvalidXattrNamespacePrefix {
            prefix: "s".to_string(),
        },
    ));
    assert_eq!(invalid_prefix.code(), tonic::Code::InvalidArgument);

    let exhausted = status_from_pool_write_error(PoolWriteError::Parity(
        ParityError::ObjectTooLargeForEmptyTape {
            projected_object_blocks: 10,
            empty_tape_usable_blocks: 9,
            required_reserve_blocks: 1,
        },
    ));
    assert_eq!(exhausted.code(), tonic::Code::ResourceExhausted);

    let worm = status_from_pool_write_error(PoolWriteError::ObjectWriteMedia(
        crate::pool_write::ObjectWriteMediaError::Worm,
    ));
    assert_eq!(worm.code(), tonic::Code::FailedPrecondition);
    assert!(worm.message().contains("WORM tape"), "{worm}");

    let identity = status_from_pool_write_error(PoolWriteError::TapeIdentity(
        crate::pool_write::TapeIdentityError::Mismatch {
            expected: "11111111-1111-1111-1111-111111111111".to_string(),
            actual: "22222222-2222-2222-2222-222222222222".to_string(),
        },
    ));
    assert_eq!(identity.code(), tonic::Code::FailedPrecondition);
    assert!(identity.message().contains("mismatch"), "{identity}");

    let admission = status_from_pool_write_error(PoolWriteError::WriteAdmissionConflict(
        "same replay key is awaiting checkpoint".to_string(),
    ));
    assert_eq!(admission.code(), tonic::Code::Aborted);
    assert!(admission.message().contains("replay key"), "{admission}");
}

#[test]
fn session_close_snapshot_success_clears_snapshot_persist_alarm() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-api-snapshot-alarm")
        .tempdir()
        .expect("tempdir");
    let mut index =
        CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
    let drive_uuid = Uuid::new_v4().as_bytes().to_vec();
    let condition_key = snapshot_persist_alarm_key(&drive_uuid);
    index
        .raise_alarm(
            condition_key.as_str(),
            "snapshot-persist-failing",
            "warning",
            Some("{\"misses\":3}"),
        )
        .expect("raise snapshot alarm");

    index
        .clear_alarm(condition_key.as_str())
        .expect("clear snapshot alarm");

    assert_eq!(
        index
            .get_alarm(condition_key.as_str())
            .expect("alarm lookup")
            .expect("alarm row")
            .state,
        "cleared"
    );
}

#[test]
fn failure_snapshots_are_keyed_by_failing_session() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-api-failure-snapshots")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open catalog");
    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: "DRV-FAIL-SNAPSHOT".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("LIB-FAIL-SNAPSHOT".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-07-18T00:00:00Z".to_string()),
        })
        .expect("observe drive")
        .drive_uuid;
    let mut world =
        VirtualWorld::single_drive("LIB-FAIL-SNAPSHOT", 0x0100, "DRV-FAIL-SNAPSHOT", 0x0400, 1);
    world.put_tape_in_drive(0x0100, "FAIL001L9", Some(0x0400), VirtualTape::default());
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot);
    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let mut drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let append_session = Uuid::new_v4();
    let read_session = Uuid::new_v4();
    let tape_uuid = [0x77; 16];
    let mut misses = 0;

    record_session_snapshot(
        &mut index,
        &cfg,
        &mut drive,
        Some(drive_uuid.clone()),
        append_session,
        tape_uuid,
        "append-failure",
        &mut misses,
    );
    record_session_snapshot(
        &mut index,
        &cfg,
        &mut drive,
        Some(drive_uuid.clone()),
        read_session,
        tape_uuid,
        "read-failure",
        &mut misses,
    );

    let rows = index
        .list_drive_health_snapshots(&drive_uuid)
        .expect("list failure snapshots");
    let append_session = append_session.to_string();
    let read_session = read_session.to_string();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].trigger, "append-failure");
    assert_eq!(rows[0].session_id.as_deref(), Some(append_session.as_str()));
    assert_eq!(rows[1].trigger, "read-failure");
    assert_eq!(rows[1].session_id.as_deref(), Some(read_session.as_str()));
}

/// Build a virtual world with one drive holding a seated no-parity tape
/// whose BOT bootstrap matches `tape_uuid`, provisioned in the catalog
/// under `voltag`. Returns everything a read-open harvest test needs.
#[allow(clippy::type_complexity)]
fn read_harvest_world(
    temp: &tempfile::TempDir,
    tape_uuid: [u8; 16],
    voltag: &str,
) -> (
    std::path::PathBuf,
    remanence_state::CalibrationControlStore,
    mpsc::Sender<DriveCommand>,
    String,
    String,
) {
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open catalog");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: voltag.to_string(),
            block_size: 4096,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    drop(index);

    let bootstrap = BootstrapPayload {
        scheme: None,
        no_parity_flag: true,
        filemark_map_digest: None,
        tape_uuid,
        written_by_version: "test".to_string(),
        written_at: "2026-08-04T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: 4096,
        drive_compression: false,
    };
    let mut bootstrap_block = vec![0u8; 4096];
    write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, 4096);
    tape.records = vec![
        Record::Block(bootstrap_block),
        Record::Filemark,
        Record::Filemark,
    ];
    tape.written_bytes = 4096;
    let mut world =
        VirtualWorld::single_drive("LIB-READ-HARVEST", 0x0100, "DRV-READ-HARVEST", 0x0400, 1);
    world.put_tape_in_drive(0x0100, voltag, Some(0x0400), tape);
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
    let calibration_store = cfg.calibration_store.clone();
    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
    (
        index_path,
        calibration_store,
        drive_tx,
        serial,
        voltag.to_string(),
    )
}

async fn open_read_on_fresh_mount(
    drive_tx: &mpsc::Sender<DriveCommand>,
    tape_uuid: [u8; 16],
    serial: &str,
    barcode: &str,
) -> pb::ReadSession {
    let (open_read_tx, open_read_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenRead {
            tape_uuid,
            needs_drive_load: true,
            library_serial: serial.to_string(),
            barcode: Some(barcode.to_string()),
            source_slot: None,
            drive_uuid: None,
            drive_serial: Some("DRV-READ-HARVEST".to_string()),
            resume_target: None,
            daemon_epoch: 1,
            reply: open_read_tx,
        })
        .await
        .expect("send read open");
    open_read_rx
        .await
        .expect("read open reply")
        .expect("open read session")
}

/// The prompt's read-mount harvest fixture: a tape mounted purely to
/// read — no write session anywhere — calibrates the volume, asserted
/// through `servable_wrap_map`. Without the read-side harvest a
/// restore-only workload would leave every volume permanently
/// uncalibrated.
#[tokio::test]
async fn read_only_mount_harvest_calibrates_the_volume() {
    use crate::calibration::{servable_wrap_map, WrapMapServeOutcome};

    let temp = tempfile::Builder::new()
        .prefix("remanence-read-mount-harvest")
        .tempdir()
        .expect("tempdir");
    let tape_uuid = [0x79; 16];
    let (index_path, store, drive_tx, serial, voltag) =
        read_harvest_world(&temp, tape_uuid, "RDH001L9");
    assert_eq!(
        store.row(tape_uuid).state,
        remanence_state::VolumeCalibrationState::Uncalibrated,
        "no history before the read mount"
    );
    assert_eq!(store.row(tape_uuid).calibration_generation, 0);

    let session = open_read_on_fresh_mount(&drive_tx, tape_uuid, &serial, &voltag).await;
    let session_id = Uuid::from_slice(&session.session_id).expect("read session UUID");

    // The read-only mount harvested: the volume is durably calibrated
    // and its map is servable for planning.
    let row = store.row(tape_uuid);
    assert_eq!(
        row.state,
        remanence_state::VolumeCalibrationState::Calibrated,
        "the read mount's load harvest calibrated the volume"
    );
    assert!(row.calibration_generation > 0);
    let index = CatalogIndex::open(&index_path).expect("reopen catalog");
    match servable_wrap_map(&index, &store, tape_uuid).expect("serve") {
        WrapMapServeOutcome::Servable { map, .. } => {
            assert_eq!(map.wrap_count(), 1);
            assert!(map.mapped_extent_lba() > 0);
        }
        other => panic!("read-only mount must leave a servable map, got {other:?}"),
    }
    drop(index);

    let (close_tx, close_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::CloseRead {
            session_id,
            reply: close_tx,
        })
        .await
        .expect("send read close");
    close_rx
        .await
        .expect("read close reply")
        .expect("close read session");
}

/// The failure half of the same placement rule: when the harvest cannot
/// calibrate (a recognised-but-unsupported format — REOWP is never even
/// issued), no harvest outcome fails the open. The session opens, reads
/// can proceed, and the volume is honestly not servable.
#[tokio::test]
async fn read_open_succeeds_when_the_harvest_cannot_calibrate() {
    use crate::calibration::{servable_wrap_map, WrapMapServeOutcome, WrapMapServeRefusal};

    let temp = tempfile::Builder::new()
        .prefix("remanence-read-mount-unsupported")
        .tempdir()
        .expect("tempdir");
    let tape_uuid = [0x7A; 16];
    // M8 is recognised but unsupported (conflicting published geometry).
    let (index_path, store, drive_tx, serial, voltag) =
        read_harvest_world(&temp, tape_uuid, "RDH002M8");

    let session = open_read_on_fresh_mount(&drive_tx, tape_uuid, &serial, &voltag).await;
    assert!(
        !session.session_id.is_empty(),
        "no harvest outcome fails the open"
    );

    let row = store.row(tape_uuid);
    assert_eq!(
        row.state,
        remanence_state::VolumeCalibrationState::UnsupportedFormat,
        "the refusal is recorded durably, not silently dropped"
    );
    let index = CatalogIndex::open(&index_path).expect("reopen catalog");
    match servable_wrap_map(&index, &store, tape_uuid).expect("serve") {
        WrapMapServeOutcome::NotServable { refusal, .. } => {
            assert_eq!(refusal, WrapMapServeRefusal::UnsupportedFormat);
        }
        other => panic!("unsupported format must not serve a map, got {other:?}"),
    }
}

#[tokio::test]
async fn induced_append_and_read_failures_persist_session_snapshots() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-api-induced-failure-snapshots")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open catalog");
    let tape_uuid = [0x78; 16];
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: "failure-test".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: "FAIL002L9".to_string(),
            block_size: 4096,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_tape_pool_membership(tape_uuid, "failure-test")
        .expect("assign pool");
    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: "DRV-INDUCED-FAIL".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("LIB-INDUCED-FAIL".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-07-18T00:00:00Z".to_string()),
        })
        .expect("observe drive")
        .drive_uuid;

    let bootstrap = BootstrapPayload {
        scheme: None,
        no_parity_flag: true,
        filemark_map_digest: None,
        tape_uuid,
        written_by_version: "test".to_string(),
        written_at: "2026-07-18T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: 4096,
        drive_compression: false,
    };
    let mut bootstrap_block = vec![0u8; 4096];
    write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, 4096);
    tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
    tape.written_bytes = 4096;
    let mut world =
        VirtualWorld::single_drive("LIB-INDUCED-FAIL", 0x0100, "DRV-INDUCED-FAIL", 0x0400, 1);
    world.put_tape_in_drive(0x0100, "FAIL002L9", Some(0x0400), tape);
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);

    let (open_read_tx, open_read_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenRead {
            tape_uuid,
            needs_drive_load: false,
            library_serial: serial.clone(),
            barcode: Some("FAIL002L9".to_string()),
            source_slot: None,
            drive_uuid: Some(drive_uuid.clone()),
            drive_serial: Some("DRV-INDUCED-FAIL".to_string()),
            resume_target: None,
            daemon_epoch: 1,
            reply: open_read_tx,
        })
        .await
        .expect("send read open");
    let read_session = open_read_rx
        .await
        .expect("read open reply")
        .expect("open read session");
    let read_session_id = Uuid::from_slice(&read_session.session_id).expect("read session UUID");
    let (chunk_tx, mut chunk_rx) = crate::read_core::read_stream_channel(4096);
    drive_tx
        .send(DriveCommand::ReadFile {
            session_id: read_session_id,
            object_id: Uuid::new_v4().to_string(),
            file_id: Vec::new(),
            stream_chunk_bytes: 4096,
            chunk_tx,
        })
        .await
        .expect("send failing read");
    chunk_rx
        .next()
        .await
        .expect("read failure item")
        .expect_err("missing object induces read failure");
    let (close_read_tx, close_read_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::CloseRead {
            session_id: read_session_id,
            reply: close_read_tx,
        })
        .await
        .expect("send read close");
    close_read_rx
        .await
        .expect("read close reply")
        .expect("close read session");

    let pool_cfg = TapePoolConfig {
        scheme: None,
        id: "failure-test".to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: 4096,
        min_object_size_bytes: 0,
    };
    // A live actor observes the RPC release through its existing catalog connection.
    for endpoint in [None, Some(remanence_state::DriveKey::new(&serial, 0x0100))] {
        let fence = index
            .record_tape_io_fence_at_endpoint(
                remanence_state::TapeIoFenceInput {
                    tape_uuid: if endpoint.is_some() {
                        [0; 16]
                    } else {
                        tape_uuid
                    },
                    barcode: None,
                    reason: "transfer_error".into(),
                    evidence_json: None,
                },
                endpoint.as_ref(),
            )
            .unwrap();
        let commands_before = world.lock().unwrap().command_log.len();
        let (reply, received) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                actor: AuditActor::System,
                pool_cfg: pool_cfg.clone(),
                selected: SelectedTape {
                    standalone_endpoint: None,
                    pool_id: "failure-test".into(),
                    tape_uuid,
                    block_size: 4096,
                    parity_config: ParityConfig::None,
                },
                target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                needs_drive_load: false,
                library_serial: serial.clone(),
                barcode: Some("FAIL002L9".into()),
                source_slot: None,
                drive_uuid: Some(drive_uuid.clone()),
                drive_serial: Some("DRV-INDUCED-FAIL".into()),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            received.await.unwrap().unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        let (reply, received) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenRead {
                tape_uuid,
                needs_drive_load: false,
                library_serial: serial.clone(),
                barcode: Some("FAIL002L9".into()),
                source_slot: None,
                drive_uuid: Some(drive_uuid.clone()),
                drive_serial: Some("DRV-INDUCED-FAIL".into()),
                resume_target: None,
                daemon_epoch: 1,
                reply,
            })
            .await
            .unwrap();
        assert_eq!(
            received.await.unwrap().unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        assert_eq!(
            world.lock().unwrap().command_log.len(),
            commands_before,
            "fenced opens issue no tape commands or append recovery"
        );
        let api = crate::ApiState::new(CatalogIndex::open(&index_path).unwrap());
        <crate::LibraryServiceApi as pb::library_service_server::LibraryService>::release_tape_io_fence(
            &api.library_service(),
            tonic::Request::new(pb::ReleaseTapeIoFenceRequest {
                quarantine_id: fence.quarantine_id,
                acknowledgment: "verified tape and drive".into(),
            }),
        )
        .await
        .unwrap();
    }
    let (open_write_tx, open_write_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenWrite {
            actor: remanence_state::AuditActor::System,
            pool_cfg: pool_cfg.clone(),
            selected: SelectedTape {
                standalone_endpoint: None,
                pool_id: "failure-test".to_string(),
                tape_uuid,
                block_size: 4096,
                parity_config: ParityConfig::None,
            },
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: false,
            library_serial: serial,
            barcode: Some("FAIL002L9".to_string()),
            source_slot: None,
            drive_uuid: Some(drive_uuid.clone()),
            drive_serial: Some("DRV-INDUCED-FAIL".to_string()),
            reply: open_write_tx,
        })
        .await
        .expect("send write open");
    let write_session = open_write_rx
        .await
        .expect("write open reply")
        .expect("open write session");
    let write_session_id = Uuid::from_slice(&write_session.session_id).expect("write session UUID");
    let spool = temp.path().join("invalid-archive-path.spool");
    std::fs::write(&spool, b"induced append failure").expect("write spool");
    let (append_tx, append_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::AppendFinish {
            session_id: write_session_id,
            source: crate::WriteObjectSource::Path(spool),
            archive_path: PathBuf::from("../invalid"),
            caller_object_id: "failure-test-object".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            representation: crate::PoolWriteRepresentation::Plaintext,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            live_write_counter: None,
            reply: append_tx,
        })
        .await
        .expect("send failing append");
    append_rx
        .await
        .expect("append reply")
        .expect_err("invalid archive path induces append failure");

    let close_command_start = world.lock().expect("world lock").command_log.len();
    let (close_write_tx, close_write_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::Close {
            session_id: write_session_id,
            reply: close_write_tx,
        })
        .await
        .expect("send write close");
    let close_reply = close_write_rx
        .await
        .expect("write close reply")
        .expect("close write session");
    assert_eq!(
        close_reply.session.state,
        pb::write_session::State::WriteSessionStateClosed as i32
    );
    assert_eq!(
        close_reply.diagnostics.filemark_write_drain,
        StdDuration::ZERO
    );
    assert_eq!(
        close_reply.diagnostics.catalog_journal_fsync,
        StdDuration::ZERO
    );
    assert_eq!(close_reply.diagnostics.rewind, StdDuration::ZERO);
    assert_eq!(close_reply.diagnostics.ssc_unload, StdDuration::ZERO);
    let close_opcodes = world
        .lock()
        .expect("world lock")
        .command_log
        .iter()
        .skip(close_command_start)
        .map(|command| command.opcode)
        .collect::<Vec<_>>();
    assert!(
        !close_opcodes.contains(&0x1b),
        "session close must leave the cartridge seated: {close_opcodes:?}"
    );
    assert!(
        !close_opcodes.contains(&0x01),
        "diagnostics must not add a separate REWIND command: {close_opcodes:?}"
    );

    let check = CatalogIndex::open(&index_path).expect("reopen catalog");
    let rows = check
        .list_drive_health_snapshots(&drive_uuid)
        .expect("list snapshots");
    let read_session_text = read_session_id.to_string();
    let write_session_text = write_session_id.to_string();
    assert!(
        rows.iter().any(|row| {
            row.trigger == "read-failure"
                && row.session_id.as_deref() == Some(read_session_text.as_str())
        }),
        "missing read-failure snapshot: {rows:#?}"
    );
    assert!(
        rows.iter().any(|row| {
            row.trigger == "append-failure"
                && row.session_id.as_deref() == Some(write_session_text.as_str())
        }),
        "missing append-failure snapshot: {rows:#?}"
    );
}

#[test]
fn object_write_media_policy_requires_positive_rewritable_evidence() {
    let config = |write_protected, worm| TapeConfig {
        density_code: None,
        block_size: remanence_library::BlockSize::Variable,
        compression: false,
        max_block_size_bytes: 8 * 1024 * 1024,
        write_protected,
        worm,
    };

    validate_write_media_policy(
        config(false, WormMediaState::NotWorm),
        WriteMediaPolicy::RewritableObject,
    )
    .expect("positively identified rewritable media is admitted");

    let worm = validate_write_media_policy(
        config(false, WormMediaState::Worm),
        WriteMediaPolicy::RewritableObject,
    )
    .expect_err("WORM media cannot support whole-Object tail replacement");
    assert_eq!(worm.code(), tonic::Code::FailedPrecondition);
    assert!(worm.message().contains("WORM tape"), "{worm}");

    let unknown = validate_write_media_policy(
        config(false, WormMediaState::Unknown),
        WriteMediaPolicy::RewritableObject,
    )
    .expect_err("unknown WORM state must fail closed");
    assert_eq!(unknown.code(), tonic::Code::FailedPrecondition);
    assert!(unknown.message().contains("state is unknown"), "{unknown}");

    let protected = validate_write_media_policy(
        config(true, WormMediaState::NotWorm),
        WriteMediaPolicy::RewritableObject,
    )
    .expect_err("write-protected media must be refused");
    assert_eq!(protected.code(), tonic::Code::FailedPrecondition);
    assert!(
        protected.message().contains("write-protected"),
        "{protected}"
    );

    for worm in [WormMediaState::Worm, WormMediaState::Unknown] {
        validate_write_media_policy(config(false, worm), WriteMediaPolicy::TerminalAppend)
            .expect("terminal recovery retains its append-only WORM policy");
    }
}

#[test]
fn prepare_object_write_rejects_worm_before_mode_select_or_media_write() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    let tape_uuid = [0x4D; 16];
    let bootstrap = BootstrapPayload {
        scheme: None,
        no_parity_flag: true,
        filemark_map_digest: None,
        tape_uuid,
        written_by_version: "test".to_string(),
        written_at: "2026-08-11T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: BLOCK_SIZE,
        drive_compression: false,
    };
    let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
    write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
    let mut tape = VirtualTape::empty(1024 * 1024, BLOCK_SIZE);
    tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
    tape.written_bytes = u64::from(BLOCK_SIZE);
    tape.worm = true;

    let mut world = VirtualWorld::single_drive("LIB-WORM-GATE", 0x0100, "DRV-WORM-GATE", 0x0400, 1);
    world.put_tape_in_drive(0x0100, "WORM001L9", Some(0x0400), tape);
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(Arc::clone(&world));
    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let mut drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let command_start = world.lock().expect("world lock").command_log.len();

    let error = prepare_drive_for_write(
        &mut drive,
        &tape_uuid,
        BLOCK_SIZE,
        Uuid::new_v4(),
        WriteMediaPolicy::RewritableObject,
    )
    .expect_err("ordinary Object session must reject WORM media");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert!(error.message().contains("WORM tape"), "{error}");

    let opcodes = world.lock().expect("world lock").command_log[command_start..]
        .iter()
        .map(|command| command.opcode)
        .collect::<Vec<_>>();
    assert!(
        opcodes.contains(&0x1a),
        "the refusal must use current drive-reported media state: {opcodes:02x?}"
    );
    for forbidden in [0x15, 0x0a, 0x10] {
        assert!(
            !opcodes.contains(&forbidden),
            "WORM refusal issued forbidden opcode 0x{forbidden:02x}: {opcodes:02x?}"
        );
    }
}

#[test]
fn prepare_drive_for_read_sets_catalog_fixed_block_size_and_rejects_missing_geometry() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-read-mode-prepare-")
        .tempdir()
        .expect("tempdir");
    let mut index =
        CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
    let tape_uuid = [0x44; 16];
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: "DATA044L9".to_string(),
            block_size: 4096,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");

    let mut world = VirtualWorld::single_drive("LIB-READ-PREP", 0x0100, "DRV-READ", 0x0400, 1);
    world.put_tape_in_drive(
        0x0100,
        "DATA044L9",
        Some(0x0400),
        VirtualTape::empty(1024 * 1024, 1024),
    );
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(Arc::clone(&world));
    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let mut drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");

    prepare_drive_for_read(&index, &mut drive, &tape_uuid, Uuid::new_v4())
        .expect("prepare fixed read mode");

    assert_eq!(
        world
            .lock()
            .expect("world lock")
            .tapes
            .get("DATA044L9")
            .expect("model tape")
            .block_size,
        4096
    );

    let missing_tape_uuid = [0x45; 16];
    let error = prepare_drive_for_read(&index, &mut drive, &missing_tape_uuid, Uuid::new_v4())
        .expect_err("missing catalog geometry must fail closed");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert!(error.message().contains("catalog row is missing"));
}

#[tokio::test]
async fn empty_file_id_ranges_are_payload_relative_real_bytes() {
    let payload: Vec<u8> = (0..1600)
        .map(|value| u8::try_from(value % 251).unwrap())
        .collect();
    let fixture = cataloged_payload_fixture(&payload);
    assert!(fixture.layout.files[0].first_chunk_lba.is_some());

    let mid = stream_fixture_range(&fixture, "", 400, 900)
        .await
        .expect("mid range");
    assert_eq!(mid, payload[400..900]);

    let to_eof = stream_fixture_range(&fixture, "", 1200, payload.len() as u64)
        .await
        .expect("range to eof");
    assert_eq!(to_eof, payload[1200..]);

    let empty = stream_fixture_range(&fixture, "", 777, 777)
        .await
        .expect("empty range");
    assert!(empty.is_empty());

    let whole = stream_fixture_range(&fixture, "", 0, 0)
        .await
        .expect("whole payload range");
    assert_eq!(whole, payload);
}

#[tokio::test]
async fn member_scoped_ranges_still_resolve_file_id() {
    let payload = b"member scoped range bytes".to_vec();
    let fixture = cataloged_payload_fixture(&payload);

    let got = stream_fixture_range(&fixture, "payload-file", 7, 13)
        .await
        .expect("member range");

    assert_eq!(got, b"scoped");
}

#[test]
fn frequency_cap_alarm_triggers_on_recent_run() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-cleaning-cap")
        .tempdir()
        .expect("temp dir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: "DRV-CAP".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("mainlib".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-07-04T04:00:00Z".to_string()),
        })
        .expect("observe drive")
        .drive_uuid;

    let run = index
        .begin_clean_run(&drive_uuid, "mainlib", "periodic", None)
        .expect("begin run");
    index
        .terminalize_clean_run(run.run_id.as_str(), "done", Some("{\"stage\":\"done\"}"))
        .expect("finish run");

    assert!(
        cleaning_too_soon(&index, &drive_uuid, Duration::seconds(0), 1).expect("frequency check"),
        "one completed run in the current week must hit the weekly cap"
    );
}

#[test]
fn cleaning_alarm_failure_rolls_back_fence_before_error() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-cleaning-alarm-fail")
        .tempdir()
        .expect("temp dir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open");
    let db = rusqlite::Connection::open(&index_path).expect("open sqlite");
    db.execute_batch(
        "create trigger fail_alarm_insert
             before insert on alarms
             begin
               select raise(fail, 'injected alarm failure');
             end;",
    )
    .expect("install alarm failure trigger");
    drop(db);

    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: "DRV-ALARM".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("LIB-ALARM".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-07-04T05:00:00Z".to_string()),
        })
        .expect("observe drive")
        .drive_uuid;

    let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
        "LIB-ALARM",
        0x0100,
        "DRV-ALARM",
        0x0400,
        1,
    )));
    let library = open_model_library(std::sync::Arc::clone(&world));
    let snapshot_cell = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot_cell);
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(Uuid::new_v4(), "cleaning");
    let mut library = library;
    let err = run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
        .expect_err("alarm failure must fail cleaning");
    assert_eq!(err.code(), tonic::Code::Internal);
    assert!(
        !index
            .get_drive_by_uuid(&drive_uuid)
            .expect("drive lookup")
            .expect("drive row")
            .fenced,
        "fence must be rolled back when alarm insertion fails"
    );
}

#[test]
fn periodic_cleaning_defers_on_busy_drive_and_now_fences_after_session_end() {
    let busy_world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
        "LIB-POLICY",
        0x0100,
        "DRV-POLICY",
        0x0400,
        1,
    )));
    {
        let mut world = busy_world.lock().expect("world lock");
        world.put_tape_in_drive(0x0100, "DATA-BUSY", None, VirtualTape::default());
        world.put_tape_in_slot(
            0x0400,
            "CLN-POLICY",
            VirtualTape {
                cleaning_cart: true,
                ..VirtualTape::default()
            },
        );
    }
    let busy_library = open_model_library(std::sync::Arc::clone(&busy_world));
    let busy_snapshot = library_snapshot_cell(busy_library.library().clone());
    let busy_temp = tempfile::Builder::new()
        .prefix("remanence-cleaning-periodic")
        .tempdir()
        .expect("temp dir");
    let busy_index_path = busy_temp.path().join("rem-state.sqlite");
    let mut busy_index = CatalogIndex::open(&busy_index_path).expect("open");
    let busy_drive_uuid = busy_index
        .observe_drive(DriveObservationInput {
            serial: "DRV-POLICY".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("LIB-POLICY".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-07-04T05:10:00Z".to_string()),
        })
        .expect("observe busy drive")
        .drive_uuid;
    let cln_uuid = [0x91; 16];
    busy_index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: cln_uuid,
            voltag: "CLN-POLICY".to_string(),
            block_size: 4096,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision cleaning tape");
    busy_index
        .set_tape_kind(&cln_uuid, "cleaning")
        .expect("mark cleaning cart")
        .expect("cleaning tape row");
    busy_index
        .set_tape_cleaning_state(&cln_uuid, "ok")
        .expect("mark cleaning cart state")
        .expect("cleaning tape row");
    let busy_cfg = test_write_owner_config(
        busy_index_path.clone(),
        busy_temp.path().join("audit"),
        &busy_library,
        busy_snapshot,
    );
    std::fs::create_dir_all(&busy_cfg.audit_dir).expect("create audit dir");

    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(Uuid::new_v4(), "cleaning");
    let mut library = busy_library;
    assert!(
        run_cleaning_sequence(
            &mut busy_index,
            &busy_cfg,
            &handle,
            &mut library,
            &busy_drive_uuid,
            "periodic",
        )
        .is_ok(),
        "periodic cleaning must defer while the drive is busy"
    );
    assert!(
        !busy_index
            .get_drive_by_uuid(&busy_drive_uuid)
            .expect("drive lookup")
            .expect("drive row")
            .fenced,
        "periodic defer must not fence the drive"
    );
    assert!(
        busy_index
            .get_active_clean_run_by_drive(&busy_drive_uuid)
            .expect("active run lookup")
            .is_none(),
        "periodic defer must not create a clean run"
    );

    let now_world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
        "LIB-NOW", 0x0100, "DRV-NOW", 0x0400, 1,
    )));
    {
        let mut world = now_world.lock().expect("world lock");
        world.put_tape_in_drive(0x0100, "DATA-NOW", None, VirtualTape::default());
        world.put_tape_in_slot(
            0x0400,
            "CLN-NOW",
            VirtualTape {
                cleaning_cart: true,
                ..VirtualTape::default()
            },
        );
    }
    let now_library = open_model_library(std::sync::Arc::clone(&now_world));
    let now_snapshot = library_snapshot_cell(now_library.library().clone());
    let now_temp = tempfile::Builder::new()
        .prefix("remanence-cleaning-now")
        .tempdir()
        .expect("temp dir");
    let now_index_path = now_temp.path().join("rem-state.sqlite");
    let mut now_index = CatalogIndex::open(&now_index_path).expect("open");
    let now_drive_uuid = now_index
        .observe_drive(DriveObservationInput {
            serial: "DRV-NOW".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("LIB-NOW".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-07-04T05:11:00Z".to_string()),
        })
        .expect("observe now drive")
        .drive_uuid;
    let now_uuid = [0x92; 16];
    now_index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: now_uuid,
            voltag: "CLN-NOW".to_string(),
            block_size: 4096,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision cleaning tape");
    now_index
        .set_tape_kind(&now_uuid, "cleaning")
        .expect("mark cleaning cart")
        .expect("cleaning tape row");
    now_index
        .set_tape_cleaning_state(&now_uuid, "ok")
        .expect("mark cleaning cart state")
        .expect("cleaning tape row");
    let now_cfg = test_write_owner_config(
        now_index_path.clone(),
        now_temp.path().join("audit"),
        &now_library,
        now_snapshot,
    );
    std::fs::create_dir_all(&now_cfg.audit_dir).expect("create audit dir");

    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(Uuid::new_v4(), "cleaning");
    let mut library = now_library;
    let err = run_cleaning_sequence(
        &mut now_index,
        &now_cfg,
        &handle,
        &mut library,
        &now_drive_uuid,
        "now",
    )
    .expect_err("now cleaning should fence and then hit the busy-drive path");
    assert_ne!(err.code(), tonic::Code::Ok);
    assert!(
        now_index
            .get_drive_by_uuid(&now_drive_uuid)
            .expect("drive lookup")
            .expect("drive row")
            .fenced,
        "now cleaning must fence the drive"
    );
}

#[test]
fn no_cln_cart_branch_unfences_drive_and_raises_alarm() {
    let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
        "LIB-NOCART",
        0x0100,
        "DRV-NOCART",
        0x0400,
        1,
    )));
    let library = open_model_library(std::sync::Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let temp = tempfile::Builder::new()
        .prefix("remanence-cleaning-no-cart")
        .tempdir()
        .expect("temp dir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open");
    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: "DRV-NOCART".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("LIB-NOCART".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-07-04T05:20:00Z".to_string()),
        })
        .expect("observe drive")
        .drive_uuid;
    let cfg = test_write_owner_config(
        index_path.clone(),
        temp.path().join("audit"),
        &library,
        snapshot,
    );
    std::fs::create_dir_all(&cfg.audit_dir).expect("create audit dir");
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(Uuid::new_v4(), "cleaning");
    let mut library = library;
    let err = run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
        .expect_err("no-cart branch must stop cleaning");
    assert_ne!(err.code(), tonic::Code::Ok);
    assert!(
        !index
            .get_drive_by_uuid(&drive_uuid)
            .expect("drive lookup")
            .expect("drive row")
            .fenced,
        "no-cart branch must leave the drive unfenced"
    );
    assert!(
        index
            .get_alarm(format!("no-cln-cart:{}", library.library().serial).as_str())
            .expect("alarm lookup")
            .is_some_and(|alarm| alarm.state == "open"),
        "no-cart branch must raise the standing alarm"
    );
    assert!(
        index
            .get_active_clean_run_by_drive(&drive_uuid)
            .expect("active run lookup")
            .is_none(),
        "no-cart branch must not leave an active clean run"
    );
}

#[test]
fn cleaning_frequency_cap_refuses_before_fence_or_run() {
    let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
        "LIB-CAP", 0x0100, "DRV-CAP", 0x0400, 1,
    )));
    {
        let mut world = world.lock().expect("world lock");
        world.put_tape_in_slot(
            0x0400,
            "CLN-CAP",
            VirtualTape {
                cleaning_cart: true,
                ..VirtualTape::default()
            },
        );
    }
    let library = open_model_library(std::sync::Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let temp = tempfile::Builder::new()
        .prefix("remanence-cleaning-frequency-cap")
        .tempdir()
        .expect("temp dir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open");
    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: "DRV-CAP".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("LIB-CAP".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-07-04T05:30:00Z".to_string()),
        })
        .expect("observe drive")
        .drive_uuid;
    let completed = index
        .begin_clean_run(&drive_uuid, "LIB-CAP", "now", None)
        .expect("begin prior run");
    index
        .terminalize_clean_run(
            completed.run_id.as_str(),
            "done",
            Some("{\"stage\":\"done\"}"),
        )
        .expect("finish prior run");
    let cfg = WriteOwnerConfig {
        cleaning: remanence_state::CleaningConfig {
            weekly_cap: 1,
            min_interval: "0s".to_string(),
            ..remanence_state::CleaningConfig::default()
        },
        ..test_write_owner_config(
            index_path.clone(),
            temp.path().join("audit"),
            &library,
            snapshot,
        )
    };
    std::fs::create_dir_all(&cfg.audit_dir).expect("create audit dir");
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(Uuid::new_v4(), "cleaning");
    let mut library = library;
    let err = run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
        .expect_err("frequency cap must reject");
    assert_ne!(err.code(), tonic::Code::Ok);
    assert!(
        !index
            .get_drive_by_uuid(&drive_uuid)
            .expect("drive lookup")
            .expect("drive row")
            .fenced,
        "frequency cap must not fence the drive"
    );
    assert!(
        index
            .get_active_clean_run_by_drive(&drive_uuid)
            .expect("active run lookup")
            .is_none(),
        "frequency cap must not leave an active clean run"
    );
    assert!(
        index
            .get_alarm(
                format!(
                    "drive-cleaning-abnormal-frequency:{}",
                    crate::bytes_to_hex(&drive_uuid)
                )
                .as_str()
            )
            .expect("alarm lookup")
            .is_some_and(|alarm| alarm.state == "open"),
        "frequency cap must raise the abnormal-frequency alarm"
    );
}

#[test]
fn inventory_only_cleaning_cart_is_recognized_before_fast_eject() {
    let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
        "LIB-FAST", 0x0100, "DRV-FAST", 0x0400, 1,
    )));
    {
        let mut world = world.lock().expect("world lock");
        world.put_tape_in_slot(
            0x0400,
            "CLNU01L9",
            VirtualTape {
                cleaning_cart: true,
                cleaning_cart_expired: true,
                ..VirtualTape::default()
            },
        );
    }
    let library = open_model_library(std::sync::Arc::clone(&world));
    let snapshot = library_snapshot_cell(library.library().clone());
    let temp = tempfile::Builder::new()
        .prefix("remanence-cleaning-fast-eject")
        .tempdir()
        .expect("temp dir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open");
    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: "DRV-FAST".to_string(),
            identity_source: "DvcidAndInquiry".to_string(),
            vendor: Some("IBM".to_string()),
            product: Some("ULT3580".to_string()),
            firmware_rev: Some("A1".to_string()),
            managed: "rem".to_string(),
            library_serial: Some("LIB-FAST".to_string()),
            element_address: Some(0x0100),
            observed_at_utc: Some("2026-07-04T05:40:00Z".to_string()),
        })
        .expect("observe drive")
        .drive_uuid;
    let cfg = test_write_owner_config(
        index_path.clone(),
        temp.path().join("audit"),
        &library,
        snapshot,
    );
    std::fs::create_dir_all(&cfg.audit_dir).expect("create audit dir");
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(Uuid::new_v4(), "cleaning");
    let mut library = library;
    let err = run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
        .expect_err("fast-eject cart must be rejected");
    assert_ne!(err.code(), tonic::Code::Ok);
    assert!(
        !err.message().contains("no eligible cleaning cartridge"),
        "inventory-only cart must reach the physical cleaning path: {err}"
    );
    let cart = index
        .get_tape_by_voltag("CLNU01L9")
        .expect("cleaning cart lookup")
        .expect("inventory cart registered");
    assert_eq!(cart.kind, "cleaning");
    assert_eq!(
        index
            .get_tape_cleaning_state(cart.tape_uuid.as_slice())
            .expect("cleaning state lookup")
            .flatten()
            .as_deref(),
        Some("expired")
    );
    assert!(
        index
            .get_active_clean_run_by_drive(&drive_uuid)
            .expect("active run lookup")
            .is_none(),
        "fast-eject path should not leave the selected clean run active"
    );
}

#[tokio::test]
async fn encrypted_payload_is_served_opaque_and_decrypted_client_side() {
    let mut encrypted_opts = RemTarObjectOptions::new(
        "cccccccc-cccc-cccc-cccc-cccccccccccc",
        "caller-encrypted",
        "2026-06-16T12:00:00Z",
        "dddddddd-dddd-dddd-dddd-dddddddddddd",
    );
    encrypted_opts.chunk_size = 512;
    let secret: Vec<u8> = (0..1800)
        .map(|value| u8::try_from((value * 7) % 251).unwrap())
        .collect();
    let encrypted_files = [RemTarFile {
        path: "secret.bin",
        file_id: "secret-file",
        data: secret.as_slice(),
        mtime: Some("0"),
        executable: Some(false),
    }];
    let primary = RecipientPrivateKey::new([0x31; 16], "primary-2026", [0x41; 32]).unwrap();
    let recovery = RecipientPrivateKey::new([0x32; 16], "recovery-2026", [0x42; 32]).unwrap();
    let recipients = vec![
        primary.public_key(0).unwrap(),
        recovery.public_key(1).unwrap(),
    ];
    let mut encrypted_sink = VecBlockSink::new();
    let encrypted_report = write_encrypted_rem_object(
        &mut encrypted_sink,
        &encrypted_opts,
        &encrypted_files,
        &recipients,
    )
    .expect("write encrypted payload");
    let encrypted_payload: Vec<u8> = encrypted_sink.blocks.iter().flatten().copied().collect();
    assert_eq!(&encrypted_payload[0..4], b"REMO");

    let fixture = cataloged_payload_fixture(&encrypted_payload);
    let header = stream_fixture_range(&fixture, "", 0, 64)
        .await
        .expect("opaque header range");
    assert_eq!(header, encrypted_payload[0..64]);

    let opaque = stream_fixture_range(&fixture, "", 0, encrypted_payload.len() as u64)
        .await
        .expect("opaque encrypted payload");
    assert_eq!(opaque, encrypted_payload);

    let opened = read_encrypted_rem_object_file_range_to_vec(
        &opaque,
        &primary,
        encrypted_report.plaintext_layout.files[0].first_chunk_lba,
        secret.len() as u64,
        300,
        333,
    )
    .expect("client-side decrypt range");
    assert_eq!(opened.bytes, secret[300..633]);
}

#[tokio::test]
async fn invalid_payload_ranges_return_typed_status() {
    let payload = b"short payload".to_vec();
    let fixture = cataloged_payload_fixture(&payload);

    let past_eof = stream_fixture_range(&fixture, "", 99, 100)
        .await
        .expect_err("past EOF must fail");
    assert_eq!(past_eof.code(), tonic::Code::InvalidArgument);

    let overflow_request = file_range_read_request(
        &fixture.index,
        &RANGE_TAPE_UUID,
        RANGE_OBJECT_ID,
        "",
        u64::MAX - 1,
        u64::MAX,
    )
    .expect("request builder allows planner to catch arithmetic overflow");
    let mut source = VecBlockSource::new(fixture.blocks.clone());
    let (tx, _rx) = crate::read_core::read_stream_channel(8);
    let overflow = stream_file_range_from_source(
        &mut source,
        overflow_request,
        0,
        tx,
        &TapeIoConfig::default(),
        test_io_memory(),
    )
    .expect_err("overflow must fail");
    assert_eq!(overflow.code(), tonic::Code::InvalidArgument);

    let reversed =
        file_range_read_request(&fixture.index, &RANGE_TAPE_UUID, RANGE_OBJECT_ID, "", 5, 4)
            .expect_err("end before start must fail");
    assert_eq!(reversed.code(), tonic::Code::InvalidArgument);
}

#[test]
fn terminal_inventory_stream_proto_preserves_complete_book_rows() {
    let structural = terminal_inventory_event_to_proto(
        remanence_parity::TerminalInventoryStreamEvent::StructuralEntry {
            attempt_id: 7,
            replica_ordinal: 3,
            entry: remanence_parity::TapeIndexReplicaMapEntry {
                tape_file_number: u64::MAX,
                kind: remanence_parity::TapeIndexReplicaFileKind::ParitySidecar,
                block_count: u64::MAX - 1,
                first_parity_data_ordinal: None,
                protected_ordinal_start: Some(u64::MAX - 3),
                protected_ordinal_end_exclusive: Some(u64::MAX - 2),
                epoch_id: Some(u64::MAX - 4),
            },
        },
    );
    let Some(pb::tape_inventory_stream_item::Item::StructuralEntry(structural)) = structural.item
    else {
        panic!("structural event must remain structural")
    };
    assert_eq!(structural.attempt_id, 7);
    assert_eq!(structural.replica_ordinal, 3);
    assert_eq!(structural.tape_file_number, u64::MAX);
    assert_eq!(structural.block_count, u64::MAX - 1);
    assert_eq!(structural.protected_ordinal_start, Some(u64::MAX - 3));
    assert_eq!(structural.epoch_id, Some(u64::MAX - 4));

    let object = terminal_inventory_event_to_proto(
        remanence_parity::TerminalInventoryStreamEvent::ObjectRow {
            attempt_id: 7,
            replica_ordinal: 3,
            row: remanence_parity::TapeIndexReplicaObjectRow {
                tape_file_number: u64::MAX - 8,
                stored_block_count: u64::MAX - 9,
                object_id: b"complete-object-id".to_vec(),
                representation: remanence_parity::ObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: u64::MAX - 10,
                    manifest_size_bytes: u64::MAX - 11,
                    manifest_chunk_count: u64::MAX - 12,
                    manifest_sha256: [0x5a; 32],
                },
            },
        },
    );
    let Some(pb::tape_inventory_stream_item::Item::ObjectRow(object)) = object.item else {
        panic!("Object event must remain an Object row")
    };
    assert_eq!(object.object_id, b"complete-object-id");
    let Some(pb::tape_inventory_object_row::Representation::Plaintext(plaintext)) =
        object.representation
    else {
        panic!("plaintext recovery anchors must be present")
    };
    assert_eq!(plaintext.manifest_first_chunk_lba, u64::MAX - 10);
    assert_eq!(plaintext.manifest_sha256, vec![0x5a; 32]);
}

#[test]
fn bot_recovery_control_events_preserve_start_and_boundary_evidence() {
    let started = bot_recovery_control_event_to_proto(
        &BotStructuralRecoveryEvent::Started,
        RANGE_TAPE_UUID,
        256 * 1024,
        BotStructuralRecoveryReason::AllMembersInvalid,
    );
    let Some(pb::tape_inventory_stream_item::Item::BotRecoveryStarted(started)) = started.item
    else {
        panic!("BOT fallback must emit a typed start event")
    };
    assert_eq!(started.tape_uuid, RANGE_TAPE_UUID);
    assert_eq!(started.block_size, 256 * 1024);
    assert_eq!(
        started.reason,
        pb::TapeInventoryBotRecoveryReason::AllMembersInvalid as i32
    );

    let progress = bot_recovery_control_event_to_proto(
        &BotStructuralRecoveryEvent::Progress(remanence_parity::ScanWalkProgress {
            tape_file_number: u64::MAX - 1,
            position: PhysicalPositionHint {
                partition: 1,
                lba: u64::MAX,
            },
            structural_candidate_count: u64::MAX,
            elapsed: StdDuration::from_millis(12_345),
        }),
        RANGE_TAPE_UUID,
        256 * 1024,
        BotStructuralRecoveryReason::NoUsableTerminalLayout,
    );
    let Some(pb::tape_inventory_stream_item::Item::BotRecoveryProgress(progress)) = progress.item
    else {
        panic!("BOT boundary must emit typed progress")
    };
    assert_eq!(progress.tape_file_number, u64::MAX - 1);
    assert_eq!(progress.partition, 1);
    assert_eq!(progress.position_lba, u64::MAX);
    assert_eq!(progress.structural_candidate_count, u64::MAX);
    assert_eq!(progress.elapsed_millis, 12_345);

    let status = status_from_bot_structural_recovery_error(
        remanence_parity::BotStructuralRecoveryError::Aborted {
            last_tape_file_number: Some(7),
            structural_candidate_count: 8,
            position: Some(PhysicalPositionHint::new(99)),
            elapsed_millis: 250,
        },
    );
    assert_eq!(status.code(), tonic::Code::Cancelled);
    assert!(status.message().contains("tape file Some(7)"));
}

/// Extend the write-owner model harness with a cataloged tape and caller-chosen BOT bytes.
fn with_catalog_recovery_tape(
    parity: ParityConfig,
    bootstrap: Option<BootstrapPayload>,
    test: impl FnOnce(&mut CatalogIndex, &WriteOwnerConfig, &mut DriveHandle),
) {
    with_catalog_recovery_tape_options(parity, bootstrap, |_| {}, false, false, test);
}

/// Customize only fresh in-memory tapes; published fixtures are never modified.
fn with_catalog_recovery_tape_options(
    parity: ParityConfig,
    bootstrap: Option<BootstrapPayload>,
    edit_tape: impl FnOnce(&mut VirtualTape),
    medium_error: bool,
    short_reads: bool,
    test: impl FnOnce(&mut CatalogIndex, &WriteOwnerConfig, &mut DriveHandle),
) {
    with_catalog_recovery_tape_faults(
        parity,
        bootstrap,
        edit_tape,
        RecoveryReadFaults {
            medium_error,
            short_reads,
            overlength_reads: false,
        },
        test,
    );
}

/// Physical READ faults the recovery tests inject below the drive handle.
#[derive(Clone, Copy, Default)]
struct RecoveryReadFaults {
    /// The first READ reports a medium error.
    medium_error: bool,
    /// Every READ returns one byte less than the record.
    short_reads: bool,
    /// Every READ reports a record one byte longer than the host buffer.
    overlength_reads: bool,
}

fn with_catalog_recovery_tape_faults(
    parity: ParityConfig,
    bootstrap: Option<BootstrapPayload>,
    edit_tape: impl FnOnce(&mut VirtualTape),
    faults: RecoveryReadFaults,
    test: impl FnOnce(&mut CatalogIndex, &WriteOwnerConfig, &mut DriveHandle),
) {
    const BLOCK_SIZE: u32 = 512 * 1024;
    let temp = tempfile::tempdir().expect("recovery test directory");
    let index_path = temp.path().join("catalog.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open recovery catalog");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: RANGE_TAPE_UUID,
            voltag: "RECOVERY001".to_string(),
            block_size: BLOCK_SIZE,
            parity,
            force: false,
        })
        .expect("catalog recovery authority");
    let mut bot = vec![0; BLOCK_SIZE as usize];
    if let Some(bootstrap) = bootstrap {
        write_bootstrap_block(&bootstrap, &mut bot).expect("encode bootstrap");
    }
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK_SIZE);
    tape.records = vec![
        Record::Block(bot),
        Record::Filemark,
        Record::ZeroBlock(BLOCK_SIZE),
        Record::Filemark,
    ];
    tape.written_bytes = 2 * u64::from(BLOCK_SIZE);
    // Start in another mode to prove the recovery path takes geometry from the catalog.
    tape.block_size = 4096;
    edit_tape(&mut tape);
    let mut world = VirtualWorld::single_drive("LIB-RECOVERY", 0x0100, "DRV-RECOVERY", 0x0400, 1);
    world.put_tape_in_drive(0x0100, "RECOVERY001", Some(0x0400), tape);
    let world = Arc::new(Mutex::new(world));
    let model = world.lock().expect("world").library_snapshot();
    let policy = remanence_library::StaticAllowlist::new([model.serial.as_str()]);
    let mut library = model
        .open_with(&policy, move |path| {
            let role = world
                .lock()
                .expect("world")
                .role_for_path(path)
                .expect("model path");
            Ok(Box::new(RecoveryReadFaultTransport {
                inner: ModelTransport::new(Arc::clone(&world), role),
                medium_error: faults.medium_error,
                short_reads: faults.short_reads,
                overlength_reads: faults.overlength_reads,
            }))
        })
        .expect("model library");
    let snapshot = library_snapshot_cell(library.library().clone());
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).expect("recovery audit directory");
    let mut cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot);
    cfg.checkpoint_journal_dir = temp.path().join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    let mut drive = library
        .open_drive(0x0100, &cfg.policy)
        .expect("open recovery drive");
    test(&mut index, &cfg, &mut drive);
}

/// Inventory reaches terminal discovery and BOT fallback with either catalog scheme.
#[test]
fn terminal_inventory_unreadable_bootstrap_uses_catalog_hints() {
    for parity in [
        ParityConfig::None,
        ParityConfig::Scheme(default_scheme_for_block_size(512 * 1024)),
    ] {
        with_catalog_recovery_tape(parity, None, |index, cfg, drive| {
            let (tx, mut rx) = mpsc::channel(64);
            handle_drive_tape_inventory(
                0x0100,
                index,
                cfg,
                drive,
                RANGE_TAPE_UUID,
                false,
                "LIB-RECOVERY",
                Some("RECOVERY001"),
                None,
                Some("DRV-RECOVERY"),
                &tx,
            )
            .expect("inventory proceeds with unreadable BOT");
            let mut summary = None;
            while let Ok(item) = rx.try_recv() {
                if let Some(pb::tape_inventory_stream_item::Item::Summary(value)) =
                    item.expect("inventory item").item
                {
                    summary = Some(value);
                }
            }
            let summary = summary.expect("terminal discovery emits a recovery summary");
            assert_eq!(summary.tape_uuid, RANGE_TAPE_UUID);
            assert_eq!(summary.structural_entry_count, Some(2));
            assert_eq!(summary.unknown_object_count, Some(1));
            assert!(summary.detail.contains("bootstrap treated as unreadable"));
            assert_eq!(
                drive.read_config().expect("read mode").block_size,
                remanence_library::BlockSize::Fixed {
                    size_bytes: 512 * 1024
                }
            );
        });
    }
}

/// Full verification reports missing terminal authority after discovery, not a BOT refusal.
#[test]
fn verify_index_unreadable_bootstrap_uses_catalog_hints() {
    for parity in [
        ParityConfig::None,
        ParityConfig::Scheme(default_scheme_for_block_size(512 * 1024)),
    ] {
        with_catalog_recovery_tape(parity, None, |index, cfg, drive| {
            let report = handle_drive_verify_tape_index(
                0x0100,
                index,
                cfg,
                drive,
                RANGE_TAPE_UUID,
                false,
                "LIB-RECOVERY",
                Some("RECOVERY001"),
                None,
                Some("DRV-RECOVERY"),
                None,
            )
            .expect("verify-index proceeds with unreadable BOT");
            let io = report.io.as_ref().expect("recovery-required I/O");
            assert!(io.total.as_ref().unwrap().single_read > 0);
            assert!(
                io.phases
                    .iter()
                    .find(|p| p.phase == "setup")
                    .unwrap()
                    .counts
                    .as_ref()
                    .unwrap()
                    .other
                    > 0
            );
            assert!(
                io.phases
                    .iter()
                    .find(|p| p.phase == "bot_recovery")
                    .unwrap()
                    .counts
                    .as_ref()
                    .unwrap()
                    .single_read
                    > 0
            );
            drop(drive.begin_verification_io()); // prior session detached

            assert_eq!(report.tape_uuid, RANGE_TAPE_UUID);
            assert_eq!(
                report.state,
                pb::TapeIndexVerificationState::RecoveryRequired as i32
            );
            assert_eq!(report.measured_eod_lba, 4);
            let recovery = report.recovery_inventory.expect("BOT recovery evidence");
            assert_eq!(recovery.structural_entry_count, Some(2));
            assert_eq!(recovery.unknown_object_count, Some(1));
            assert!(recovery.detail.contains("bootstrap treated as unreadable"));
        });
    }
}

/// The mounted reconcile path scans and projects using the catalog despite unreadable BOT.
#[test]
fn reconcile_unreadable_bootstrap_uses_catalog_hints() {
    for parity in [
        ParityConfig::None,
        ParityConfig::Scheme(default_scheme_for_block_size(512 * 1024)),
    ] {
        with_catalog_recovery_tape(parity, None, |index, cfg, drive| {
            let registry = crate::operations::OperationRegistry::default();
            let operation = registry.register(Uuid::new_v4(), "reconcile_tape");
            super::reconcile::reconcile_loaded_tape(index, cfg, drive, RANGE_TAPE_UUID, operation);
            let audit = FileAuditLog::replay(&cfg.audit_dir).expect("reconcile audit");
            assert!(
                audit
                    .iter()
                    .any(|record| record.event == AuditEvent::OperationFinished),
                "{audit:?}"
            );
            assert!(!audit
                .iter()
                .any(|record| record.event == AuditEvent::OperationFailed));
            let finish = audit
                .iter()
                .find(|record| record.event == AuditEvent::OperationFinished)
                .expect("finish audit");
            assert_eq!(
                finish.detail.get("bootstrap_treated_as_unreadable"),
                Some(&CborValue::Bool(true))
            );
            assert_eq!(
                finish.detail.get("identity_and_geometry_source"),
                Some(&CborValue::Text("catalog_hints".to_string()))
            );
            let entries = index
                .list_tape_files(&RANGE_TAPE_UUID)
                .expect("reconciled tape files");
            assert_eq!(entries.len(), 2);
            assert!(entries.iter().all(|entry| entry.block_count == 1));
        });
    }
}

/// Every recovery entry point must preserve readable identity and geometry conflicts.
#[test]
fn catalog_recovery_paths_refuse_readable_bootstrap_disagreement() {
    let bootstrap = BootstrapPayload {
        scheme: None,
        no_parity_flag: true,
        filemark_map_digest: None,
        tape_uuid: RANGE_TAPE_UUID,
        written_by_version: "recovery-test".to_string(),
        written_at: "2026-09-27T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: 512 * 1024,
        drive_compression: false,
    };
    for mismatch in ["identity", "block size", "parity scheme"] {
        let mut bootstrap = bootstrap.clone();
        let parity = match mismatch {
            "identity" => {
                bootstrap.tape_uuid = [0xCD; 16];
                ParityConfig::None
            }
            "block size" => {
                bootstrap.block_size_bytes = 256 * 1024;
                ParityConfig::None
            }
            _ => ParityConfig::Scheme(default_scheme_for_block_size(512 * 1024)),
        };
        with_catalog_recovery_tape(parity, Some(bootstrap), |index, cfg, drive| {
            let (tx, _rx) = mpsc::channel(64);
            let error = handle_drive_tape_inventory(
                0x0100,
                index,
                cfg,
                drive,
                RANGE_TAPE_UUID,
                false,
                "LIB-RECOVERY",
                Some("RECOVERY001"),
                None,
                Some("DRV-RECOVERY"),
                &tx,
            )
            .expect_err("inventory rejects disagreement");
            assert!(error.message().contains(mismatch), "{error}");
            let error = handle_drive_verify_tape_index(
                0x0100,
                index,
                cfg,
                drive,
                RANGE_TAPE_UUID,
                false,
                "LIB-RECOVERY",
                Some("RECOVERY001"),
                None,
                Some("DRV-RECOVERY"),
                None,
            )
            .expect_err("verify-index rejects disagreement");
            assert!(error.message().contains(mismatch), "{error}");
            let registry = crate::operations::OperationRegistry::default();
            let operation = registry.register(Uuid::new_v4(), "reconcile_tape");
            super::reconcile::reconcile_loaded_tape(index, cfg, drive, RANGE_TAPE_UUID, operation);
            let audit = FileAuditLog::replay(&cfg.audit_dir).expect("reconcile audit");
            assert!(audit
                .iter()
                .any(|record| record.event == AuditEvent::OperationFailed));
            assert!(!audit
                .iter()
                .any(|record| record.event == AuditEvent::OperationFinished));
            assert!(index
                .list_tape_files(&RANGE_TAPE_UUID)
                .expect("no projection")
                .is_empty());
        });
    }
}

/// Inject exactly one physical READ failure to exercise the daemon gate itself.
struct RecoveryReadFaultTransport {
    inner: ModelTransport,
    medium_error: bool,
    short_reads: bool,
    overlength_reads: bool,
}

impl SgTransport for RecoveryReadFaultTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        if self.medium_error && cdb.first() == Some(&0x08) {
            self.medium_error = false;
            return Err(remanence_library::ScsiError::CheckCondition {
                sense: readiness_fixed_sense(0x03, 0x11, 0),
                bytes_transferred: 0,
            });
        }
        let mut outcome = self.inner.execute_in(cdb, buf)?;
        if self.overlength_reads && cdb.first() == Some(&0x08) {
            // Variable-block READ with SILI clear: ILI, VALID and a negative
            // INFORMATION field report a record longer than the host buffer.
            let mut sense = readiness_fixed_sense(0x00, 0x00, 0x00);
            sense[0] |= 0x80;
            sense[2] |= 0x20;
            sense[3..7].copy_from_slice(&(-1i32).to_be_bytes());
            return Err(remanence_library::ScsiError::CheckCondition {
                sense,
                bytes_transferred: outcome.bytes_transferred,
            });
        }
        if self.short_reads && cdb.first() == Some(&0x08) {
            assert!(
                outcome.bytes_transferred > 1,
                "short-read injection needs a block"
            );
            outcome.bytes_transferred -= 1;
        }
        Ok(outcome)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        self.inner.execute_none(cdb)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

/// All daemon entry points refuse incomplete catalog authority before rebuilding anything.
#[test]
fn catalog_recovery_paths_refuse_missing_or_incomplete_geometry() {
    for sql in [
        "DELETE FROM tapes",
        "UPDATE tapes SET block_size = NULL",
        "UPDATE tapes SET scheme_id = 'rs-cauchy-gf256-v1'",
        "UPDATE tapes SET data_blocks_per_stripe = 128",
        "UPDATE tapes SET scheme_id = 'rs-cauchy-gf256-v1', data_blocks_per_stripe = 128, parity_blocks_per_stripe = 4",
    ] {
        with_catalog_recovery_tape(ParityConfig::None, None, |index, cfg, drive| {
            rusqlite::Connection::open(&cfg.index_path).expect("catalog")
                .execute(sql, []).expect("remove catalog authority");
            assert_catalog_recovery_refusal(index, cfg, drive, "catalog");
        });
    }
}

/// Assert refusal through inventory, verify-index and reconcile, including the audit.
fn assert_catalog_recovery_refusal(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    reason: &str,
) {
    let (tx, _rx) = mpsc::channel(64);
    let error = handle_drive_tape_inventory(
        0x0100,
        index,
        cfg,
        drive,
        RANGE_TAPE_UUID,
        false,
        "LIB-RECOVERY",
        Some("RECOVERY001"),
        None,
        Some("DRV-RECOVERY"),
        &tx,
    )
    .expect_err("inventory refusal");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert!(error.message().contains(reason), "{error}");
    let error = handle_drive_verify_tape_index(
        0x0100,
        index,
        cfg,
        drive,
        RANGE_TAPE_UUID,
        false,
        "LIB-RECOVERY",
        Some("RECOVERY001"),
        None,
        Some("DRV-RECOVERY"),
        None,
    )
    .expect_err("verify refusal");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert!(error.message().contains(reason), "{error}");
    let registry = crate::operations::OperationRegistry::default();
    let operation_id = Uuid::new_v4();
    super::reconcile::reconcile_loaded_tape(
        index,
        cfg,
        drive,
        RANGE_TAPE_UUID,
        registry.register(operation_id, "reconcile_tape"),
    );
    let audit = FileAuditLog::replay(&cfg.audit_dir).expect("audit");
    let failed = audit
        .iter()
        .find(|record| record.event == AuditEvent::OperationFailed)
        .expect("failure audit");
    assert!(
        format!("{:?}", failed.detail).contains(reason),
        "{failed:?}"
    );
    tokio::runtime::Runtime::new()
        .expect("operation runtime")
        .block_on(async {
            use tokio_stream::StreamExt;
            let status = registry
                .watch(&operation_id)
                .expect("operation watch")
                .next()
                .await
                .expect("failure status")
                .expect("status");
            assert_eq!(status.state, pb::OperationState::Failed as i32);
            assert_eq!(
                status.progress.get("phase").map(String::as_str),
                Some("mount")
            );
        });
    assert!(!audit
        .iter()
        .any(|record| record.event == AuditEvent::OperationFinished));
}

/// Recovery never overrides compression refusal, including when catalog geometry agrees.
#[test]
fn catalog_recovery_paths_refuse_compressed_bootstrap() {
    let scheme = default_scheme_for_block_size(512 * 1024);
    let bootstrap = BootstrapPayload {
        scheme: Some(ParitySchemeRecord {
            id: scheme.id.as_str().to_string(),
            data_blocks_per_stripe: scheme.data_blocks_per_stripe,
            parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
            stripes_per_neighborhood: scheme.stripes_per_neighborhood,
            no_parity_flag: false,
        }),
        no_parity_flag: false,
        filemark_map_digest: Some(
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
                .expect("map")
                .digest(false)
                .expect("digest"),
        ),
        tape_uuid: RANGE_TAPE_UUID,
        written_by_version: "recovery-test".to_string(),
        written_at: "2026-09-27T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: 512 * 1024,
        drive_compression: false,
    };
    with_catalog_recovery_tape_options(
        ParityConfig::Scheme(scheme),
        Some(bootstrap),
        |tape| {
            let Record::Block(block) = &mut tape.records[0] else {
                panic!("BOT block")
            };
            let end = 56 + u32::from_le_bytes(block[44..48].try_into().unwrap()) as usize;
            assert_eq!(&block[end - 2..end], &[5, 0xf4]);
            block[end - 1] = 0xf5;
            let crc = crc64_xz(&block[56..end]);
            block[end..end + 8].copy_from_slice(&crc.to_le_bytes());
        },
        false,
        false,
        |index, cfg, drive| {
            assert_catalog_recovery_refusal(
                index,
                cfg,
                drive,
                "tape's bootstrap records drive compression; a parity tape must not record drive compression",
            );
        },
    );
}

/// An intact terminal tail must retain malformed or physically unreadable BOT provenance.
#[test]
fn catalog_recovery_terminal_tail_retains_bootstrap_provenance() {
    for medium_error in [false, true] {
        with_catalog_recovery_tape_options(
            ParityConfig::None,
            None,
            append_recovery_terminal_tail,
            medium_error,
            false,
            |index, cfg, drive| {
                let (tx, mut rx) = mpsc::channel(64);
                handle_drive_tape_inventory(
                    0x0100,
                    index,
                    cfg,
                    drive,
                    RANGE_TAPE_UUID,
                    false,
                    "LIB-RECOVERY",
                    Some("RECOVERY001"),
                    None,
                    Some("DRV-RECOVERY"),
                    &tx,
                )
                .expect("terminal inventory survives BOT damage");
                let mut summary = None;
                while let Ok(item) = rx.try_recv() {
                    if let Some(pb::tape_inventory_stream_item::Item::Summary(value)) =
                        item.expect("inventory item").item
                    {
                        summary = Some(value);
                    }
                }
                let summary = summary.expect("summary");
                assert_eq!(summary.inventory_basis, "terminal_index_fast");
                assert!(
                    summary.detail.contains("bootstrap treated as unreadable"),
                    "{}",
                    summary.detail
                );
                assert!(summary.detail.contains("catalog hints"));
                if medium_error {
                    assert!(summary.detail.contains("read BOT:"), "{}", summary.detail);
                } else {
                    assert!(
                        summary.detail.contains("magic mismatch"),
                        "{}",
                        summary.detail
                    );
                }
                let verification = handle_drive_verify_tape_index(
                    0x0100,
                    index,
                    cfg,
                    drive,
                    RANGE_TAPE_UUID,
                    false,
                    "LIB-RECOVERY",
                    Some("RECOVERY001"),
                    None,
                    Some("DRV-RECOVERY"),
                    None,
                )
                .expect("verification reports damaged BOT with surviving replicas");
                let io = verification.io.as_ref().expect("degraded I/O");
                assert!(io.total.as_ref().unwrap().read_position > 0);
                drop(drive.begin_verification_io());

                assert!(
                    verification
                        .detail
                        .contains("bootstrap treated as unreadable"),
                    "{}",
                    verification.detail
                );
                assert!(verification.detail.contains("catalog hints"));
                // Damage inside the prefix does not abandon the terminal route
                // when the surviving replicas' rows agree with the walk: the
                // damaged bootstrap is a finding, so the tape is not complete.
                assert_eq!(
                    verification.state,
                    pb::TapeIndexVerificationState::VerifiedDegraded as i32,
                    "{}",
                    verification.detail
                );
            },
        );
    }
}

/// Generate a terminal edition over a bootstrap-only prefix in fresh model memory.
fn append_recovery_terminal_tail(tape: &mut VirtualTape) {
    const BLOCK_SIZE: u32 = 512 * 1024;
    struct BootstrapRows;
    impl TapeIndexReplicaRecordSource for BootstrapRows {
        fn visit_structural_entries(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            visitor(&TapeIndexReplicaMapEntry {
                tape_file_number: 0,
                kind: TapeIndexReplicaFileKind::Bootstrap,
                block_count: 1,
                first_parity_data_ordinal: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            })
        }
        fn visit_object_rows(
            &mut self,
            _visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            Ok(())
        }
    }
    tape.records.truncate(2);
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: 1,
        object_row_count: 0,
    };
    let replica_records = checked_tape_index_replica_layout(BLOCK_SIZE, counts)
        .expect("replica layout")
        .replica_record_count;
    let layout =
        TerminalTailLayout::new(0, BLOCK_SIZE, 1, 2, replica_records, 3).expect("tail layout");
    let edition = plan_tape_index_edition(
        TapeIndexEditionDescriptor {
            tape_uuid: RANGE_TAPE_UUID,
            edition_id: [0x62; 16],
            edition_sequence: 1,
            scope: TapeIndexReplicaScope {
                covered_prefix_tape_file_count: 1,
                total_data_ordinals: 0,
                highest_protected_ordinal: 0,
            },
            counts,
            block_size: BLOCK_SIZE,
            compression_enabled: false,
            writer_version: "recovery-test".to_string(),
            write_timestamp: "2026-09-27T00:00:00Z".to_string(),
            terminal_layout: layout,
        },
        &mut BootstrapRows,
    )
    .expect("edition");
    for ordinal in 1..=3 {
        let plan = plan_tape_index_replica(edition.clone(), ordinal).expect("replica plan");
        write_tape_index_replica(
            &plan,
            TapeIndexReplicaObservation {
                tape_file_number: plan.component.planned_tape_file_number,
                start_lba: plan.component.planned_start_lba,
                record_count: plan.component.record_count,
            },
            &mut BootstrapRows,
            |block| {
                tape.records.push(Record::Block(block.to_vec()));
                Ok(())
            },
        )
        .expect("replica");
        tape.records.push(Record::Filemark);
        if ordinal != 3 {
            let plan = plan_index_separation(IndexSeparationDescriptor {
                tape_uuid: RANGE_TAPE_UUID,
                edition_id: edition.descriptor.edition_id,
                gap_ordinal: ordinal,
                block_size: BLOCK_SIZE,
                nominal_extent_bytes: 3 * u64::from(BLOCK_SIZE),
                total_records: 3,
                compression_enabled: false,
                terminal_layout: layout,
            })
            .expect("separation plan");
            write_index_separation(
                &plan,
                IndexSeparationObservation {
                    tape_file_number: plan.component.planned_tape_file_number,
                    start_lba: plan.component.planned_start_lba,
                    record_count: plan.component.record_count,
                },
                |block| {
                    tape.records.push(Record::Block(block.to_vec()));
                    Ok(())
                },
            )
            .expect("separation");
            tape.records.push(Record::Filemark);
        }
    }
    tape.written_bytes = tape
        .records
        .iter()
        .filter(|record| !matches!(record, Record::Filemark))
        .count() as u64
        * u64::from(BLOCK_SIZE);
}

/// Catalog recovery must reject checksummed foreign headers despite payload or schema damage.
#[test]
fn catalog_recovery_paths_refuse_checksummed_header_conflicts() {
    for field in ["identity", "block size"] {
        for failure in ["payload", "schema"] {
            let bootstrap = BootstrapPayload {
                scheme: None,
                no_parity_flag: true,
                filemark_map_digest: None,
                tape_uuid: RANGE_TAPE_UUID,
                written_by_version: "recovery-test".to_string(),
                written_at: "2026-09-27T00:00:00Z".to_string(),
                sequence: 0,
                block_size_bytes: 512 * 1024,
                drive_compression: false,
            };
            with_catalog_recovery_tape_options(
                ParityConfig::None,
                Some(bootstrap),
                |tape| {
                    let Record::Block(block) = &mut tape.records[0] else {
                        panic!("BOT")
                    };
                    if field == "identity" {
                        block[16] ^= 1;
                    } else {
                        block[32..36].copy_from_slice(&(256 * 1024u32).to_be_bytes());
                    }
                    if failure == "payload" {
                        block[80] ^= 1;
                    } else {
                        block[8..10].copy_from_slice(&99u16.to_be_bytes());
                    }
                    let crc = crc64_xz(&block[..48]);
                    block[48..56].copy_from_slice(&crc.to_le_bytes());
                },
                false,
                false,
                |index, cfg, drive| {
                    let reason = if failure == "schema" {
                        "got 99, accept 2"
                    } else {
                        field
                    };
                    assert_catalog_recovery_refusal(index, cfg, drive, reason);
                },
            );
        }
    }
}

/// Every daemon recovery entry point binds checked format fields before payload parsing.
#[test]
fn catalog_recovery_paths_refuse_checked_format_fields_and_short_bot() {
    for (offset, bytes, reason) in [
        (8, 1u16.to_be_bytes().to_vec(), "got 1, accept 2"),
        (8, 3u16.to_be_bytes().to_vec(), "got 3, accept 2"),
        (36, 7u64.to_be_bytes().to_vec(), "got sequence 7"),
        (12, 0u32.to_be_bytes().to_vec(), "no-parity flag"),
        (0, Vec::new(), "short fixed-block bootstrap read"),
    ] {
        for damaged_payload in [false, true] {
            let bootstrap = BootstrapPayload {
                scheme: None,
                no_parity_flag: true,
                filemark_map_digest: None,
                tape_uuid: RANGE_TAPE_UUID,
                written_by_version: "recovery-test".to_string(),
                written_at: "2026-09-27T00:00:00Z".to_string(),
                sequence: 0,
                block_size_bytes: 512 * 1024,
                drive_compression: false,
            };
            with_catalog_recovery_tape_options(
                ParityConfig::None,
                Some(bootstrap),
                |tape| {
                    let Record::Block(block) = &mut tape.records[0] else {
                        panic!("BOT")
                    };
                    block[offset..offset + bytes.len()].copy_from_slice(&bytes);
                    if damaged_payload {
                        block[80] ^= 1;
                    }
                    let crc = crc64_xz(&block[..48]);
                    block[48..56].copy_from_slice(&crc.to_le_bytes());
                },
                false,
                bytes.is_empty(),
                |index, cfg, drive| assert_catalog_recovery_refusal(index, cfg, drive, reason),
            );
        }
    }
}

/// A first record longer than the catalog's block size is refused for its
/// length before its content, like a short one (REM-PARITY 8.4).
#[test]
fn catalog_recovery_paths_refuse_an_overlength_bot_record() {
    let bootstrap = BootstrapPayload {
        scheme: None,
        no_parity_flag: true,
        filemark_map_digest: None,
        tape_uuid: RANGE_TAPE_UUID,
        written_by_version: "recovery-test".to_string(),
        written_at: "2026-09-27T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: 512 * 1024,
        drive_compression: false,
    };
    with_catalog_recovery_tape_faults(
        ParityConfig::None,
        Some(bootstrap),
        |_| {},
        RecoveryReadFaults {
            overlength_reads: true,
            ..RecoveryReadFaults::default()
        },
        |index, cfg, drive| {
            assert_catalog_recovery_refusal(
                index,
                cfg,
                drive,
                "bootstrap block larger than supplied block size",
            )
        },
    );
}

/// D5 closed resume goes through the private Layer-5 session opener, then the
/// ordinary reserved Object write and checkpoint on its returned session state.
#[test]
fn resume_vectors_closed_layer5() {
    resume_vectors_layer5_authorities(None);
}

#[test]
fn resume_vectors_cross_record_conflict() {
    resume_vectors_layer5_authorities(Some("conflict"));
}

#[test]
fn resume_vectors_missing_scheme_preserves_resume_append_error() {
    resume_vectors_layer5_authorities(Some("scheme"));
}

#[test]
fn resume_vectors_empty_checkpoint_authority() {
    resume_vectors_layer5_authorities(Some("empty"));
}

/// Exercise shared pool-write and Layer-5 authority checks before tape motion.
fn resume_vectors_layer5_authorities(refusal: Option<&str>) {
    use crate::resume_vectors::{
        adapt, append_object, assert_expected, model, recorded_input, EXPECTATIONS,
    };
    use remanence_parity::*;
    use sha2::Digest;
    let vector = crate::tape_image_vectors::generate("unfinalized-closed").unwrap();
    let input = recorded_input("resume-closed");
    let temp = tempfile::tempdir().unwrap();
    let (mut drive, world, _) = model(&vector, &input);
    let library = open_model_library(Arc::clone(&world));
    let mut cfg = test_write_owner_config(
        temp.path().join("catalog.sqlite"),
        temp.path().join("audit"),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.checkpoint_journal_dir = temp.path().join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "resume-vectors".into(),
        tape_uuid: vector.written.inputs.tape_uuid,
        block_size: crate::tape_image_vectors::BLOCK,
        parity_config: ParityConfig::Scheme(vector.written.inputs.scheme.clone()),
    };
    let journal = adapt(
        &input,
        &vector,
        &super::checkpoint::parity_journal_directory(&cfg, selected.tape_uuid).unwrap(),
    )
    .unwrap();
    let committed = journal.load_committed().unwrap();
    let append_lba = committed
        .filemark_map()
        .unwrap()
        .append_position_after_prefix()
        .unwrap()
        .lba;
    let source_object = &vector.written.inputs.objects[0];
    let object_id = source_object.options.object_id.clone();
    let mut entry = committed.entries[1].clone();
    entry.object_id = Some(object_id.clone());
    let row = &vector.written.object_rows[0];
    let ObjectRecoveryRepresentation::Plaintext {
        manifest_first_chunk_lba,
        manifest_size_bytes,
        manifest_chunk_count,
        manifest_sha256,
    } = &row.representation
    else {
        panic!("plaintext fixture required")
    };
    let recovery_row = remanence_state::CheckpointObjectRecoveryRow {
        tape_file_number: row.tape_file_number,
        stored_block_count: row.stored_block_count,
        object_id: row.object_id.clone(),
        representation: remanence_state::CheckpointObjectRecoveryRepresentation::Plaintext {
            manifest_first_chunk_lba: *manifest_first_chunk_lba,
            manifest_size_bytes: *manifest_size_bytes,
            manifest_chunk_count: *manifest_chunk_count,
            manifest_sha256: *manifest_sha256,
        },
    };
    let digest = sha2::Sha256::digest(&vector.image.files[1].bytes).to_vec();
    let mut checkpoint_record = remanence_state::CheckpointJournalRecord {
        ordinal: 1,
        committed_object_count: 1,
        eod_partition: 0,
        eod_lba: append_lba,
        tape_uuid: selected.tape_uuid,
        batch_id: [0x61; 16],
        next_tape_file_number: committed.entries.len() as u64,
        block_size: selected.block_size,
        objects: vec![remanence_state::CheckpointObjectProjection {
            object: NativeObjectProjectionInput {
                object_id: object_id.clone(),
                caller_object_id: Some(source_object.options.caller_object_id.clone()),
                body_format: "rem-object-v1".into(),
                logical_size_bytes: Some(
                    source_object.files.iter().map(|f| f.spec.size_bytes).sum(),
                ),
                content_hash: Some(digest.clone()),
                metadata_hash: Some(manifest_sha256.to_vec()),
                created_at_utc: Some(source_object.options.write_timestamp.clone()),
            },
            files: vec![],
            copy: NativeObjectCopyProjectionInput {
                object_id,
                tape_uuid: selected.tape_uuid,
                tape_file_number: 1,
                first_body_lba: 2,
                first_parity_data_ordinal: Some(0),
                protected_until_ordinal: Some(4),
                status: "committed".into(),
                representation: "plaintext".into(),
                recipient_epoch_ids: None,
                metadata_frame_len: None,
                plaintext_digest: Some(digest.clone()),
                stored_digest: Some(digest),
            },
            block_size: selected.block_size,
            block_count: 4,
            fresh_tape: true,
            total_committed_ordinals: 4,
            object_recovery_row: recovery_row,
        }],
        scheme: Some(vector.written.inputs.scheme.clone()),
        object_tape_file_bundles: vec![CommittedBundle {
            kind: CommittedBundleKind::Object,
            entries: vec![entry, committed.entries[2].clone()],
            highest_protected_ordinal: 4,
            total_committed_ordinals: 4,
        }],
        barrier_bundle: None,
        terminal_finalization: None,
        sealed_after_write: false,
        seal_due: None,
    };
    drop(journal);
    let checkpoint = remanence_state::authority::fixtures::AuthorityBuilder::new(
        &cfg.checkpoint_journal_dir,
        selected.tape_uuid,
    )
    .build()
    .unwrap();
    if refusal == Some("conflict") {
        checkpoint_record.block_size *= 2;
        for object in &mut checkpoint_record.objects {
            object.block_size = checkpoint_record.block_size;
        }
    }
    if refusal == Some("scheme") {
        checkpoint_record.scheme = None;
        checkpoint_record.object_tape_file_bundles.clear();
        checkpoint_record.next_tape_file_number = 2;
        checkpoint_record.eod_lba = 7;
        for object in &mut checkpoint_record.objects {
            object.copy.first_parity_data_ordinal = None;
            object.copy.protected_until_ordinal = None;
        }
    }
    if refusal != Some("empty") {
        checkpoint.append(&checkpoint_record).unwrap();
    }
    let mut lease = checkpoint.acquire_exclusive().unwrap();
    let checkpoints = lease.replay().unwrap();
    if let Some(kind) = refusal {
        world.lock().unwrap().command_log.clear();
        let journal = remanence_parity::authority::fixtures::RowAuthorityBuilder::new(
            super::checkpoint::parity_journal_directory(&cfg, selected.tape_uuid).unwrap(),
            selected.tape_uuid,
            selected.block_size,
            vector.written.inputs.scheme.clone(),
        )
        .build()
        .unwrap();
        let error = crate::pool_write::validate_parity_resume_checkpoint_authority(
            &lease,
            &journal,
            checkpoints.is_empty(),
            committed.entries.len() as u64,
        )
        .unwrap_err();
        let error = match error {
            crate::pool_write::ResumeAuthorityRefusal::Records(error) => error,
            other => panic!("expected a commit-record refusal, got {other:?}"),
        };
        let detail = match &error {
            ParityError::ResumeAppend(detail) => detail,
            other => panic!("expected ResumeAppend, got {other:?}"),
        };
        let expected_detail = if kind == "empty" {
            "structural rows exist without a committed tape-log group"
        } else {
            "structural authority disagrees with log"
        };
        assert!(detail.contains(expected_detail), "{detail}");
        let pool_status = super::restore::status_from_pool_write_error(PoolWriteError::from(error));
        assert_eq!(pool_status.code(), tonic::Code::FailedPrecondition);
        assert!(pool_status.message().contains(expected_detail));
        drop(journal);
        let result = super::checkpoint::validate_parity_actor_authority(
            &cfg,
            &selected,
            &mut lease,
            &checkpoints.iter().map(Into::into).collect::<Vec<_>>(),
        );
        if kind == "empty" {
            assert!(
                result.is_ok(),
                "a fresh native authority has no independent structural claims"
            );
        } else {
            let status = match result {
                Err(e) => e,
                Ok(_) => panic!("mismatched geometry accepted"),
            };
            assert_eq!(status.code(), tonic::Code::FailedPrecondition);
            assert!(
                status
                    .message()
                    .contains("header identity, geometry or CRC mismatch"),
                "{status}"
            );
        }
        assert!(world.lock().unwrap().command_log.is_empty());
        return;
    }
    let authority = super::checkpoint::validate_parity_actor_authority(
        &cfg,
        &selected,
        &mut lease,
        &checkpoints.iter().map(Into::into).collect::<Vec<_>>(),
    )
    .unwrap();
    let mut index = CatalogIndex::open(&cfg.index_path).unwrap();
    let last = checkpoints.last().unwrap();
    // This fixture tests parity journal reconstruction after an explicit discard.
    // Its synthetic fill is deliberately not automatic append authority.
    crate::resume_vectors::discard_synthetic_suffix(&world, last.eod_lba);
    let _admission = crate::admit_checkpointed_tape(
        &mut drive,
        selected.tape_uuid,
        selected.block_size,
        last.eod_partition,
        last.eod_lba,
    )
    .unwrap();
    let mut session = super::checkpoint::open_parity_actor_session(
        &mut index,
        &mut drive,
        &cfg,
        &selected,
        &checkpoints.iter().map(Into::into).collect::<Vec<_>>(),
        authority,
        &mut lease,
    )
    .unwrap();
    let mut raw = DriveHandleRawSink::new(&mut drive);
    raw.configure_parity_write_session(selected.block_size)
        .unwrap();
    let mut journal = lease.structural_mut().unwrap();
    let mut sink =
        ParitySink::from_session_state(&mut raw, &mut journal, session.sink_state.take().unwrap())
            .unwrap();
    let (row, _, _) = append_object(&mut sink, &world, &input, &vector).unwrap();
    let pending_state = sink.into_session_state().unwrap();
    assert_eq!(
        journal.freeze().unwrap().committed_entry_count(),
        committed.entries.len() as u64
    );
    let scan = scan_reconstruct_filemark_map_with_report(
        &mut DriveHandleRawSource::new(&mut drive),
        &selected.tape_uuid,
        selected.block_size,
    )
    .unwrap();
    let mut expected: Vec<_> = committed
        .entries
        .iter()
        .map(|e| remanence_parity::TapeFileMapEntry {
            tape_file_number: e.tape_file_number,
            kind: e.kind,
            block_count: e.block_count,
            first_parity_data_ordinal: e.first_parity_data_ordinal,
            protected_ordinal_start: e.protected_ordinal_start,
            protected_ordinal_end_exclusive: e.protected_ordinal_end_exclusive,
            epoch_id: e.epoch_id,
        })
        .collect();
    expected.extend_from_slice(pending_state.retained_live_structural_rows());
    assert_eq!(
        scan.map.entries(),
        expected,
        "committed plus pending physical filemark map"
    );
    let image = world.lock().unwrap().tapes["RESUME001"].export_image();
    let library_resume = crate::resume_vectors::execute_positive(&input, &vector).unwrap();
    for (actual, generated) in image.files.iter().zip(&library_resume.image.files) {
        assert_eq!(
            actual.bytes, generated.bytes,
            "Layer-5 and manifest bytes differ"
        );
    }
    assert_eq!(image.files.len(), library_resume.image.files.len());
    let appended = &image.files[row.tape_file_number as usize];
    let actual = serde_json::json!({"accepted":true,"append_lba":append_lba,"appended_object_tape_file":row.tape_file_number,"appended_object_first_lba":appended.start_record,
        "torn_records_superseded":appended.start_record as u64 == append_lba && appended.bytes.chunks_exact(selected.block_size as usize).all(|b|b != vec![0xa5;selected.block_size as usize]),
        "open_epoch_ordinals":(committed.highest_protected_ordinal..committed.total_committed_ordinals).collect::<Vec<_>>()});
    let source: serde_json::Value = serde_json::from_str(EXPECTATIONS).unwrap();
    let case = source["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "resume-closed")
        .unwrap();
    assert_expected("resume-closed", &case["expected"], &actual);
}

/// Exercise daemon session admission using the same rolled-back state as a
/// standalone invocation. An old released fence never grants append authority.
pub(crate) async fn daemon_rollback_refusal(
    state: &StateHandle,
    world: Arc<Mutex<VirtualWorld>>,
    selected: SelectedTape,
) -> String {
    let mut library = open_model_library(world.clone());
    let mut cfg = test_write_owner_config(
        state.paths().sqlite_path.clone(),
        state.paths().audit_dir.clone(),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.checkpoint_journal_dir = state.paths().journal_dir.join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    let mut index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
    let session_id = Uuid::new_v4();
    for event in [AuditEvent::SessionOpened, AuditEvent::SessionOrphaned] {
        record_session_event(
            &mut index,
            &cfg,
            SessionAuditInput {
                session_id,
                session_kind: "write",
                event,
                tape_uuid: Some(selected.tape_uuid),
                library_serial: Some(library.library().serial.clone()),
                drive_bay: Some(0x100),
                drive_uuid: None,
                drive_serial: None,
                abort_reason: None,
            },
        )
        .unwrap();
    }
    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let drive = library.open_drive(0x100, &policy).unwrap();
    let tape_uuid = selected.tape_uuid;
    let tx = spawn_drive_actor(0x100, drive, cfg);
    let (reply, rx) = oneshot::channel();
    tx.send(DriveCommand::OpenWrite {
        actor: remanence_state::AuditActor::System,
        pool_cfg: state.config().tape_pools[0].clone(),
        selected,
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
        needs_drive_load: false,
        library_serial: serial,
        barcode: Some("HAND".into()),
        source_slot: None,
        drive_uuid: None,
        drive_serial: None,
        reply,
    })
    .await
    .unwrap();
    let error = rx
        .await
        .unwrap()
        .expect_err("stale daemon state must refuse");
    assert!(error.message().contains("TailBeyondKnownState"), "{error}");
    let index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
    let fences = index.list_active_tape_io_fences().unwrap();
    assert!(fences.iter().all(|f| f.tape_uuid != tape_uuid));
    assert_eq!(
        index.get_tape(&tape_uuid).unwrap().unwrap().state,
        "recovery_required"
    );
    crate::startup_guard::reject_active_tape_io_fences_on_startup(&index).unwrap();
    let audit = FileAuditLog::replay(&state.paths().audit_dir).unwrap();
    assert!(audit.iter().any(|r| r.event == AuditEvent::RecoveryEvent));
    error.to_string()
}

/// Exercise real actor transitions on one handle, including same-load abandonment
/// and explicit recovery of the physical suffix retained by a simulated crash.
pub(crate) async fn daemon_append_recovery_roundtrip(
    state: &StateHandle,
    world: Arc<Mutex<VirtualWorld>>,
    selected: SelectedTape,
    root: &Path,
) {
    let mut library = open_model_library(world.clone());
    let mut cfg = test_write_owner_config(
        state.paths().sqlite_path.clone(),
        state.paths().audit_dir.clone(),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.checkpoint_journal_dir = state.paths().journal_dir.join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    cfg.checkpoint_max_objects = 100;
    cfg.checkpoint_max_age_seconds = 3600;
    let serial = library.library().serial.clone();
    let drive = library
        .open_drive(
            0x100,
            &remanence_library::StaticAllowlist::new([serial.as_str()]),
        )
        .unwrap();
    let tx = spawn_drive_actor(0x100, drive, cfg);
    let open = || {
        let tx = tx.clone();
        let serial = serial.clone();
        let selected = selected.clone();
        let pool_cfg = state.config().tape_pools[0].clone();
        async move {
            let (reply, rx) = oneshot::channel();
            tx.send(DriveCommand::OpenWrite {
                actor: remanence_state::AuditActor::System,
                pool_cfg,
                selected,
                target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                needs_drive_load: false,
                library_serial: serial,
                barcode: Some("HAND".into()),
                source_slot: None,
                drive_uuid: None,
                drive_serial: None,
                reply,
            })
            .await
            .unwrap();
            rx.await.unwrap()
        }
    };
    let first = open().await.unwrap();
    let id = Uuid::from_slice(&first.session_id).unwrap();
    let written = append_actor_test_file(
        &tx,
        id,
        root.join("aborted"),
        "aborted",
        "aborted",
        b"pending complete object",
    )
    .await;
    assert!(!written.replay);
    assert!(!written.record.object_id.is_empty());
    let crash_records = world.lock().unwrap().tapes["HAND"].records.clone();
    let (reply, rx) = oneshot::channel();
    tx.send(DriveCommand::Abort {
        session_id: id,
        reason: Some("cancel".into()),
        reply,
    })
    .await
    .unwrap();
    rx.await.unwrap().unwrap();
    assert!(world.lock().unwrap().tapes["HAND"].records.len() < crash_records.len());
    let second = open().await.unwrap();
    let id = Uuid::from_slice(&second.session_id).unwrap();
    append_actor_test_file(
        &tx,
        id,
        root.join("after-abort"),
        "after-abort",
        "after-abort",
        b"reopened successfully",
    )
    .await;
    {
        let parity_on = !matches!(selected.parity_config, ParityConfig::None);
        if parity_on {
            world
                .lock()
                .unwrap()
                .tapes
                .get_mut("HAND")
                .unwrap()
                .write_protected = true;
        }
        let failed = append_actor_test_file_result(
            &tx,
            id,
            root.join("bad"),
            if parity_on {
                "valid-path"
            } else {
                "../invalid"
            },
            "bad",
            b"failure",
        )
        .await
        .unwrap_err();
        world
            .lock()
            .unwrap()
            .tapes
            .get_mut("HAND")
            .unwrap()
            .write_protected = false;
        assert!(failed.message().contains("re-send all"), "{failed}");
    }
    let (reply, rx) = oneshot::channel();
    tx.send(DriveCommand::Abort {
        session_id: id,
        reason: None,
        reply,
    })
    .await
    .unwrap();
    rx.await.unwrap().unwrap();

    {
        let mut index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
        let fences = index.list_active_tape_io_fences().unwrap();
        assert!(!fences.is_empty());
        for fence in fences {
            index
                .release_tape_io_fence(&fence.quarantine_id, "release after failed batch")
                .unwrap();
        }
        let session = open().await.unwrap();
        let resend_id = Uuid::from_slice(&session.session_id).unwrap();
        append_actor_test_file(
            &tx,
            resend_id,
            root.join("resend"),
            "after-abort",
            "after-abort",
            b"reopened successfully",
        )
        .await;
        let (reply, rx) = oneshot::channel();
        tx.send(DriveCommand::Abort {
            session_id: resend_id,
            reason: None,
            reply,
        })
        .await
        .unwrap();
        rx.await.unwrap().unwrap();
    }

    // A process crash leaves these complete records but no checkpoint. Restore
    // exactly that media cut after closing the actor's live session.
    world.lock().unwrap().tapes.get_mut("HAND").unwrap().records = crash_records;
    let refusal = open().await.unwrap_err();
    assert!(
        refusal.message().contains("TailBeyondKnownState"),
        "{refusal}"
    );
    let index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
    assert_eq!(
        index.get_tape(&selected.tape_uuid).unwrap().unwrap().state,
        "recovery_required"
    );
    assert!(index.list_active_tape_io_fences().unwrap().is_empty());
    crate::startup_guard::reject_active_tape_io_fences_on_startup(&index).unwrap();
    let mut request = pb::SupersedeTailRequest {
        tape_uuid: selected.tape_uuid.to_vec(),
        ack_tape_uuid: selected.tape_uuid.to_vec(),
        confirm_supersede: None,
        expected_found: vec![],
    };
    let before = world.lock().unwrap().tapes["HAND"].records.clone();
    for completed in [false, true] {
        let (reply, rx) = oneshot::channel();
        tx.send(DriveCommand::SupersedeTail {
            request: request.clone(),
            actor: AuditActor::local_user(),
            needs_drive_load: false,
            library_serial: serial.clone(),
            barcode: Some("HAND".into()),
            source_slot: None,
            drive_serial: None,
            reply,
        })
        .await
        .unwrap();
        let response = rx.await.unwrap().unwrap();
        assert_eq!(response.completed, completed);
        if !completed {
            assert_eq!(world.lock().unwrap().tapes["HAND"].records, before);
        }
        request.confirm_supersede = Some(response.required_confirmation);
        request.expected_found = response.found;
    }
    assert_eq!(
        index.get_tape(&selected.tape_uuid).unwrap().unwrap().state,
        "ready"
    );
    let session = open().await.unwrap();
    let id = Uuid::from_slice(&session.session_id).unwrap();
    append_actor_test_file(
        &tx,
        id,
        root.join("committed"),
        "committed",
        "committed",
        b"healthy daemon append",
    )
    .await;
    let (reply, rx) = oneshot::channel();
    tx.send(DriveCommand::Close {
        session_id: id,
        reply,
    })
    .await
    .unwrap();
    let closed = rx.await.unwrap().unwrap();
    assert_eq!(closed.session.objects_committed, 1);
    assert_eq!(closed.session.checkpointed_objects.len(), 1);
    let durable = world.lock().unwrap().tapes["HAND"].records.clone();
    for commit in [false, true] {
        let session = open().await.unwrap();
        let id = Uuid::from_slice(&session.session_id).unwrap();
        append_actor_test_file(
            &tx,
            id,
            root.join("after-checkpoint"),
            "after-checkpoint",
            "after-checkpoint",
            b"pending after durable checkpoint",
        )
        .await;
        let (reply, rx) = oneshot::channel();
        if commit {
            tx.send(DriveCommand::Close {
                session_id: id,
                reply,
            })
            .await
            .unwrap();
        } else {
            tx.send(DriveCommand::Abort {
                session_id: id,
                reason: None,
                reply,
            })
            .await
            .unwrap();
        }
        rx.await.unwrap().unwrap();
        assert_eq!(
            &world.lock().unwrap().tapes["HAND"].records[..durable.len()],
            durable.as_slice()
        );
    }
}

/// Cancellation in the terminal walk aborts and leaves the loaded drive reusable.
#[test]
fn verification_walk_cancellation_has_no_verdict_and_can_restart() {
    use remanence_library::verification_io::VerificationPhase;
    use std::sync::atomic::{AtomicBool, Ordering};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("verification.log");
    let file = std::fs::File::create(&path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    let _log = tracing::subscriber::set_default(subscriber);
    with_catalog_recovery_tape_options(
        ParityConfig::None,
        None,
        append_recovery_terminal_tail,
        false,
        false,
        |index, cfg, drive| {
            let context = drive.verification_context();
            let flag = Arc::new(AtomicBool::new(false));
            let token = flag.clone();
            let control = context.control(
                flag.clone(),
                Arc::new(move |phase, _, _| {
                    if phase == VerificationPhase::Walk {
                        token.store(true, Ordering::SeqCst);
                    }
                }),
            );
            let cancelled = handle_drive_verify_tape_index(
                0x0100,
                index,
                cfg,
                drive,
                RANGE_TAPE_UUID,
                false,
                "LIB-RECOVERY",
                Some("RECOVERY001"),
                None,
                Some("DRV-RECOVERY"),
                None,
            )
            .unwrap_err();
            assert_eq!(cancelled.code(), tonic::Code::Cancelled);
            assert!(flag.load(Ordering::SeqCst));
            assert!(index
                .get_tape(&RANGE_TAPE_UUID)
                .unwrap()
                .unwrap()
                .last_verified_at_utc
                .is_none());
            drop(control);
            assert!(handle_drive_verify_tape_index(
                0x0100,
                index,
                cfg,
                drive,
                RANGE_TAPE_UUID,
                false,
                "LIB-RECOVERY",
                Some("RECOVERY001"),
                None,
                Some("DRV-RECOVERY"),
                None
            )
            .is_ok());
        },
    );
    let log = std::fs::read_to_string(path).unwrap();
    assert_eq!(
        log.lines()
            .filter(|line| line.contains("phase=\"total\""))
            .count(),
        2,
        "cancelled and completed attempts both log: {log}"
    );
    assert!(log
        .lines()
        .filter(|line| line.contains("phase=\"total\""))
        .all(|line| line.contains("route=\"walk\"")
            && line.contains("one_pass_fell_back=false")
            && line.contains("fallback_reason=\"\"")));
}

/// Transport-triggered cancellation goes through the real drive actor and mount owner.
struct VerificationCancelTransport {
    readiness: Arc<Mutex<std::collections::VecDeque<(u8, u8, u8)>>>,
    inner: ModelTransport,
    registry: crate::operations::OperationRegistry,
    trigger: Arc<Mutex<Option<(Uuid, &'static str)>>>,
    fired: Arc<AtomicBool>,
}
impl VerificationCancelTransport {
    fn before(&self, cdb: &[u8]) -> Result<bool, remanence_library::ScsiError> {
        let Some((id, phase)) = *self.trigger.lock().unwrap() else {
            return Ok(false);
        };
        let matches = if phase == "readiness" || phase == "refusal" {
            cdb.first() == Some(&0)
        } else if phase == "load" {
            cdb.first() == Some(&0x1b)
        } else if phase == "setup" {
            cdb.first() == Some(&0x05)
        } else {
            self.registry
                .latest(&id)
                .is_some_and(|s| s.progress.get("phase").is_some_and(|p| p == phase))
        };
        if matches && !self.fired.swap(true, Ordering::SeqCst) {
            self.registry.request_cancel(&id).unwrap();
            if phase == "refusal" {
                return Err(remanence_library::ScsiError::CheckCondition {
                    sense: readiness_fixed_sense(5, 0x20, 0),
                    bytes_transferred: 0,
                });
            }
            if phase == "setup" {
                return Err(remanence_library::ScsiError::InvalidResponse {
                    offset: 0,
                    detail: "injected transport response failure",
                });
            }
            return Ok(true);
        }
        Ok(false)
    }
}
impl SgTransport for VerificationCancelTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.before(cdb)?;
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        if cdb.first() == Some(&0) {
            if let Some((key, asc, ascq)) = self.readiness.lock().unwrap().pop_front() {
                return Err(remanence_library::ScsiError::CheckCondition {
                    sense: readiness_fixed_sense(key, asc, ascq),
                    bytes_transferred: 0,
                });
            }
        }
        let cancelled_here = self.before(cdb)?;
        self.inner.execute_none(cdb)?;
        if cdb.first() == Some(&0) {
            if cancelled_here {
                // The first TUR reaches the model, then requires another readiness probe.
                return Err(remanence_library::ScsiError::CheckCondition {
                    sense: readiness_fixed_sense(6, 0x28, 0),
                    bytes_transferred: 0,
                });
            }
            if !self.fired.load(Ordering::SeqCst)
                && self
                    .trigger
                    .lock()
                    .unwrap()
                    .is_some_and(|(_, phase)| phase == "load")
            {
                // Changer inventory has the tape, but it needs LOAD before becoming ready.
                return Err(remanence_library::ScsiError::CheckCondition {
                    sense: readiness_fixed_sense(2, 0x3a, 0),
                    bytes_transferred: 0,
                });
            }
        }
        Ok(())
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.before(cdb)?;
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

struct VerificationActorHarness {
    readiness: Arc<Mutex<std::collections::VecDeque<(u8, u8, u8)>>>,
    _temp: tempfile::TempDir,
    state: crate::ApiState,
    tape_uuid: [u8; 16],
    tx: mpsc::Sender<DriveCommand>,
    trigger: Arc<Mutex<Option<(Uuid, &'static str)>>>,
    fired: Arc<AtomicBool>,
    world: Arc<Mutex<VirtualWorld>>,
}
impl VerificationActorHarness {
    /// Build a complete protected tape entirely through in-memory transports.
    fn new() -> Self {
        Self::with_tape_in_slot(false)
    }

    /// A slotted cartridge exercises mount ownership and the needs-drive-load path.
    fn with_tape_in_slot(needs_drive_load: bool) -> Self {
        let inputs = crate::tape_image_vectors::inputs("a4-minimal");
        let tape_uuid = inputs.tape_uuid;
        let block_size = inputs.block_size;
        let parity = ParityConfig::Scheme(inputs.scheme.clone());
        let (_, image) = crate::tape_image_vectors::write_model(inputs).unwrap();
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, block_size);
        for file in image.files {
            tape.written_bytes += file.bytes.len() as u64;
            tape.records.extend(
                file.bytes
                    .chunks_exact(block_size as usize)
                    .map(|b| Record::Block(b.to_vec())),
            );
            if file.filemark_record.is_some() {
                tape.records.push(Record::Filemark);
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("catalog.sqlite");
        let mut index = CatalogIndex::open(&path).unwrap();
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "VERIFY001L9".into(),
                block_size,
                parity,
                force: false,
            })
            .unwrap();
        let mut state = crate::ApiState::new(index);
        let mut world = VirtualWorld::single_drive("LIB-VERIFY", 0x100, "DRV-VERIFY", 0x400, 1);
        if needs_drive_load {
            world.put_tape_in_slot(0x400, "VERIFY001L9", tape);
        } else {
            world.put_tape_in_drive(0x100, "VERIFY001L9", None, tape);
        }
        let world = Arc::new(Mutex::new(world));
        let model = world.lock().unwrap().library_snapshot();
        let readiness = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let trigger = Arc::new(Mutex::new(None));
        let fired = Arc::new(AtomicBool::new(false));
        let mut library = model
            .open_with(&remanence_library::StaticAllowlist::new(["LIB-VERIFY"]), {
                let readiness = readiness.clone();
                let world = world.clone();
                let trigger = trigger.clone();
                let fired = fired.clone();
                let registry = state.operations.clone();
                move |path| {
                    let role = world.lock().unwrap().role_for_path(path).unwrap();
                    Ok(Box::new(VerificationCancelTransport {
                        readiness: readiness.clone(),
                        inner: ModelTransport::new(world.clone(), role),
                        registry: registry.clone(),
                        trigger: trigger.clone(),
                        fired: fired.clone(),
                    }))
                }
            })
            .unwrap();
        let snapshot = library_snapshot_cell(library.library().clone());
        state.library_snapshot = Some(snapshot.clone());
        state.default_library_serial = Some(Arc::new("LIB-VERIFY".into()));
        state.drive_idle_unload_seconds = 0;
        let mut cfg =
            test_write_owner_config(path, state.audit_dir.as_ref().clone(), &library, snapshot);
        cfg.checkpoint_journal_dir = temp.path().join("tapes");
        std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
        let lifecycle = DrivePoolLifecycle::default();
        cfg.lifecycle = Some(lifecycle.clone());
        let drive = library.open_drive(0x100, &cfg.policy).unwrap();
        let tx = spawn_drive_actor(0x100, drive, cfg.clone());
        let changer = spawn_changer_actor(library.into_changer(), cfg);
        state.drive_pool = Some(DrivePool::new_with_lifecycle(
            HashMap::from([("LIB-VERIFY".into(), changer)]),
            HashMap::from([(DriveKey::new("LIB-VERIFY", 0x100), tx.clone())]),
            Arc::new(HashMap::from([(
                DriveKey::new("LIB-VERIFY", 0x100),
                AtomicBool::new(false),
            )])),
            lifecycle,
        ));
        world.lock().unwrap().command_log.clear();
        Self {
            readiness,
            _temp: temp,
            state,
            tape_uuid,
            tx,
            trigger,
            fired,
            world,
        }
    }

    /// Readiness-only cancellation must not create even a completed readiness row.
    fn assert_unfenced_and_released(&self) {
        let connection = rusqlite::Connection::open(self.state.index_path.as_ref()).unwrap();
        let count: i64 = connection
            .query_row("select count(*) from media_readiness_ops", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "media_readiness_ops");
        self.assert_no_active_fence_and_released();
    }

    /// A successful LOAD may retain ready evidence, but cannot leave admission fenced.
    fn assert_no_active_fence_and_released(&self) {
        assert!(self
            .state
            .index()
            .unwrap()
            .list_active_media_readiness_operations(None)
            .unwrap()
            .is_empty());
        let connection = rusqlite::Connection::open(self.state.index_path.as_ref()).unwrap();
        let count: i64 = connection
            .query_row("select count(*) from tape_io_fences", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "tape_io_fences");
        let pool = self.state.drive_pool().unwrap();
        assert!(pool.busy_bays("LIB-VERIFY").is_empty());
        drop(pool.reserve_tape(self.tape_uuid).unwrap());
    }

    async fn verify_again(&self) {
        *self.trigger.lock().unwrap() = None;
        let (_, rx) = crate::tape_verification::start(
            &self.state,
            AuditActor::System,
            self.tape_uuid,
            std::time::Duration::ZERO,
        )
        .unwrap();
        let report = rx.await.unwrap().unwrap();
        assert_eq!(
            report.state,
            pb::TapeIndexVerificationState::VerifiedComplete as i32,
            "{}",
            report.detail
        );
    }
}

#[tokio::test]
async fn verification_cancel_before_first_drive_command_leaves_no_fence() {
    let harness = VerificationActorHarness::new();
    let id = Uuid::new_v4();
    let handle = harness.state.operations.register(id, "tape_verification");
    harness.state.operations.request_cancel(&id).unwrap();
    let pool = harness.state.drive_pool().unwrap();
    let reservation = pool
        .reserve_drive(&DriveKey::new("LIB-VERIFY", 0x100))
        .unwrap();
    let tape_reservation = pool.reserve_tape(harness.tape_uuid).unwrap();
    let (reply, rx) = oneshot::channel();
    harness
        .tx
        .send(DriveCommand::VerifyTapeIndex {
            progress: crate::tape_verification::Progress::new(handle.clone(), harness.tape_uuid),
            handle,
            tape_uuid: harness.tape_uuid,
            needs_drive_load: false,
            library_serial: "LIB-VERIFY".into(),
            barcode: Some("VERIFY001L9".into()),
            source_slot: None,
            drive_serial: Some("DRV-VERIFY".into()),
            reply,
        })
        .await
        .unwrap();
    assert_eq!(
        rx.await.unwrap().unwrap_err().code(),
        tonic::Code::Cancelled
    );
    assert!(harness.world.lock().unwrap().command_log.is_empty());
    drop(reservation);
    drop(tape_reservation);
    harness.assert_unfenced_and_released();
    let handle = harness
        .state
        .operations
        .register(Uuid::new_v4(), "tape_verification");
    harness
        .state
        .operations
        .request_cancel(&handle.op_id_uuid())
        .unwrap();
    let error = crate::mount::verify_tape_index(
        &harness.state,
        harness.tape_uuid,
        handle.clone(),
        crate::tape_verification::Progress::new(handle, harness.tape_uuid),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Cancelled);
    assert!(harness.world.lock().unwrap().command_log.is_empty());
    harness.assert_unfenced_and_released();
    harness.verify_again().await;
}

#[tokio::test]
async fn verification_cancel_during_readiness_leaves_no_fence() {
    let harness = VerificationActorHarness::new();
    let (operation, rx) = crate::tape_verification::start(
        &harness.state,
        AuditActor::System,
        harness.tape_uuid,
        std::time::Duration::ZERO,
    )
    .unwrap();
    let id = Uuid::from_slice(&operation.operation_id).unwrap();
    *harness.trigger.lock().unwrap() = Some((id, "readiness"));
    assert_eq!(
        rx.await.unwrap().unwrap_err().code(),
        tonic::Code::Cancelled
    );
    assert!(harness.fired.load(Ordering::SeqCst));
    assert_eq!(
        harness
            .world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .map(|command| command.opcode)
            .collect::<Vec<_>>(),
        vec![0, 0]
    );
    assert_eq!(
        harness.state.operations.latest(&id).unwrap().state,
        pb::OperationState::Cancelled as i32
    );
    harness.assert_unfenced_and_released();
    harness.verify_again().await;
}

/// Cancellation during LOAD must allow its readiness tail to finish before releasing ownership.
#[tokio::test]
async fn verification_cancel_during_load_leaves_no_active_fence() {
    let harness = VerificationActorHarness::with_tape_in_slot(true);
    let (operation, rx) = crate::tape_verification::start(
        &harness.state,
        AuditActor::System,
        harness.tape_uuid,
        std::time::Duration::ZERO,
    )
    .unwrap();
    let id = Uuid::from_slice(&operation.operation_id).unwrap();
    *harness.trigger.lock().unwrap() = Some((id, "load"));
    assert_eq!(
        rx.await.unwrap().unwrap_err().code(),
        tonic::Code::Cancelled
    );
    assert!(harness.fired.load(Ordering::SeqCst));
    let commands = harness.world.lock().unwrap().command_log.clone();
    let readiness_commands: Vec<_> = commands
        .iter()
        .filter(|command| matches!(command.opcode, 0 | 0x1b))
        .map(|command| command.opcode)
        .collect();
    assert_eq!(readiness_commands, vec![0, 0x1b, 0]);
    assert_eq!(
        harness.state.operations.latest(&id).unwrap().state,
        pb::OperationState::Cancelled as i32
    );
    // LOAD always records readiness history; cancellation must not turn it into a fence.
    let connection = rusqlite::Connection::open(harness.state.index_path.as_ref()).unwrap();
    let rows: Vec<(String, String)> = connection
        .prepare("select state, dirty_scope from media_readiness_ops")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows, vec![("ready".into(), "none".into())]);
    harness.assert_no_active_fence_and_released();
    harness.verify_again().await;
}

#[tokio::test]
async fn verification_cancel_during_protected_pass_through_drive_actor() {
    let harness = VerificationActorHarness::new();
    let (operation, rx) = crate::tape_verification::start(
        &harness.state,
        AuditActor::System,
        harness.tape_uuid,
        std::time::Duration::ZERO,
    )
    .unwrap();
    let id = Uuid::from_slice(&operation.operation_id).unwrap();
    *harness.trigger.lock().unwrap() = Some((id, "protected_data"));
    assert_eq!(
        rx.await.unwrap().unwrap_err().code(),
        tonic::Code::Cancelled
    );
    assert!(harness.fired.load(Ordering::SeqCst));
    assert_eq!(
        harness.state.operations.latest(&id).unwrap().state,
        pb::OperationState::Cancelled as i32
    );
    assert!(harness
        .state
        .index()
        .unwrap()
        .get_tape(&harness.tape_uuid)
        .unwrap()
        .unwrap()
        .last_verified_at_utc
        .is_none());
    harness.assert_unfenced_and_released();
    harness.verify_again().await;
}

#[tokio::test]
async fn verification_concurrent_cancel_preserves_readiness_refusal_and_transport_error() {
    for phase in ["refusal", "setup"] {
        let harness = VerificationActorHarness::new();
        let (operation, rx) = crate::tape_verification::start(
            &harness.state,
            AuditActor::System,
            harness.tape_uuid,
            std::time::Duration::ZERO,
        )
        .unwrap();
        let id = Uuid::from_slice(&operation.operation_id).unwrap();
        *harness.trigger.lock().unwrap() = Some((id, phase));
        let error = rx.await.unwrap().unwrap_err();
        assert!(harness.fired.load(Ordering::SeqCst));
        if phase == "refusal" {
            assert_eq!(error.code(), tonic::Code::FailedPrecondition);
            let index = harness.state.index().unwrap();
            let fences = index.list_active_media_readiness_operations(None).unwrap();
            assert_eq!(fences.len(), 1);
            assert!(
                error
                    .message()
                    .contains(&fences[0].operation_id.to_string()),
                "{error}"
            );
        } else {
            assert_eq!(error.code(), tonic::Code::Internal);
            assert!(error
                .message()
                .contains("injected transport response failure"));
        }
        assert_eq!(
            harness.state.operations.latest(&id).unwrap().state,
            pb::OperationState::Failed as i32
        );
        assert!(harness
            .state
            .drive_pool()
            .unwrap()
            .busy_bays("LIB-VERIFY")
            .is_empty());
    }
}

/// The drive actor clears a load transient without any client resumption.
#[tokio::test]
async fn detached_verification_auto_resumes_fresh_load() {
    use tokio_stream::StreamExt;
    let harness = VerificationActorHarness::with_tape_in_slot(true);
    harness
        .readiness
        .lock()
        .unwrap()
        .extend([(2, 4, 1), (2, 4, 1), (6, 0x28, 0)]);
    crate::readiness_wait::spawn_auto_resume(harness.state.clone());
    let (op, result) = crate::tape_verification::start(
        &harness.state,
        AuditActor::System,
        harness.tape_uuid,
        std::time::Duration::from_secs(15),
    )
    .unwrap();
    let id = Uuid::from_slice(&op.operation_id).unwrap();
    let verdict = tokio::time::timeout(std::time::Duration::from_secs(20), result)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        verdict.state,
        pb::TapeIndexVerificationState::VerifiedComplete as i32
    );
    let events = harness
        .state
        .operations
        .watch(&id)
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert!(events.iter().any(|e| e
        .as_ref()
        .unwrap()
        .progress
        .get("phase")
        .is_some_and(|p| p == "readiness")));
    assert!(events.iter().any(|e| e
        .as_ref()
        .unwrap()
        .progress
        .contains_key("readiness_operation_id")));
    assert!(harness
        .state
        .index()
        .unwrap()
        .list_active_media_readiness_operations(None)
        .unwrap()
        .is_empty());
    crate::startup_media_readiness::ensure_media_readiness_admitted(
        &harness.state.index().unwrap(),
        "move",
        "LIB-VERIFY",
        None,
        None,
        true,
    )
    .unwrap();
    harness
        .state
        .readiness_shutdown
        .store(true, Ordering::Release);
}

/// Cancellation stops the detached operation while the independent readiness wait continues.
#[tokio::test]
async fn detached_verification_cancel_during_auto_readiness() {
    let harness = VerificationActorHarness::new();
    harness.readiness.lock().unwrap().extend([(2, 4, 1); 20]);
    crate::readiness_wait::spawn_auto_resume(harness.state.clone());
    let (op, result) = crate::tape_verification::start(
        &harness.state,
        AuditActor::System,
        harness.tape_uuid,
        std::time::Duration::from_secs(15),
    )
    .unwrap();
    let id = Uuid::from_slice(&op.operation_id).unwrap();
    let readiness_id = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(status) = harness.state.operations.latest(&id) {
                if let Some(readiness) = status.progress.get("readiness_operation_id") {
                    let id = Uuid::parse_str(readiness).unwrap();
                    if harness.state.operations.latest(&id).is_some() {
                        break id;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    harness.state.operations.request_cancel(&id).unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_millis(600), result)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Cancelled);
    assert_eq!(
        harness
            .state
            .operations
            .latest(&readiness_id)
            .unwrap()
            .state,
        pb::OperationState::Running as i32
    );
    harness.readiness.lock().unwrap().clear();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !harness
            .state
            .index()
            .unwrap()
            .list_active_media_readiness_operations(None)
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    harness
        .state
        .readiness_shutdown
        .store(true, Ordering::Release);
}

/// The detached deadline retains the original refusal and the fence state.
#[tokio::test]
async fn detached_verification_readiness_deadline() {
    let harness = VerificationActorHarness::new();
    harness.readiness.lock().unwrap().extend([(2, 4, 1); 20]);
    let (_, result) = crate::tape_verification::start(
        &harness.state,
        AuditActor::System,
        harness.tape_uuid,
        std::time::Duration::from_millis(20),
    )
    .unwrap();
    let error = result.await.unwrap().unwrap_err();
    assert!(
        error
            .message()
            .contains("fully verify terminal tape index blocked by media-readiness fence"),
        "{error}"
    );
    assert!(
        error.message().contains("state=media_initializing"),
        "{error}"
    );
    assert!(error.message().contains("deadline expired"));
}

/// A foreign bay's readiness fence blocks the robot and is visible to detached verification.
#[tokio::test]
async fn detached_verification_waits_for_another_drive_fence() {
    let mut harness = VerificationActorHarness::with_tape_in_slot(true);
    let old_pool = harness.state.drive_pool().unwrap();
    let (foreign_tx, _foreign_rx) = mpsc::channel(1);
    harness.state.drive_pool = Some(DrivePool::new_for_library(
        "LIB-VERIFY",
        old_pool.changer_tx("LIB-VERIFY").unwrap(),
        HashMap::from([(0x100, harness.tx.clone()), (0x101, foreign_tx)]),
        Arc::new(HashMap::from([
            (0x100, AtomicBool::new(false)),
            (0x101, AtomicBool::new(false)),
        ])),
    ));
    let readiness_id = Uuid::new_v4();
    let status = record_session_open_readiness_fence_on_operation;
    let mut index = harness.state.index_write().unwrap();
    let ctx = SessionOpenReadinessContext {
        action: "open read session",
        bay: 0x101,
        library_serial: "LIB-VERIFY",
        barcode: Some("OTHERL9"),
        source_slot: None,
        drive_serial: Some("OTHERDRIVE"),
        needs_drive_load: false,
    };
    record_session_open_readiness_operation(&mut index, readiness_id, &ctx).unwrap();
    status(
        &mut index,
        readiness_id,
        &ctx,
        "session_open_short_probe",
        &MediaReadiness::BecomingReady {
            ascq: 1,
            media_initializing: false,
        },
    );
    drop(index);
    let (op, result) = crate::tape_verification::start(
        &harness.state,
        AuditActor::System,
        harness.tape_uuid,
        StdDuration::from_secs(10),
    )
    .unwrap();
    let id = Uuid::from_slice(&op.operation_id).unwrap();
    tokio::time::timeout(StdDuration::from_secs(2), async {
        loop {
            if let Some(progress) = harness.state.operations.latest(&id) {
                if progress.progress.get("readiness_operation_id")
                    == Some(&readiness_id.to_string())
                {
                    assert_eq!(progress.progress["sense"], "02/04/01");
                    break;
                }
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    record_session_open_readiness_transition(
        &mut harness.state.index_write().unwrap(),
        readiness_id,
        "ready",
        &MediaReadiness::Ready,
    )
    .unwrap();
    let pool = harness.state.drive_pool().unwrap();
    let key = DriveKey::new("LIB-VERIFY", 0x101);
    let _client = pool.reserve_drive(&key).unwrap();
    pool.set_holder(&key, Some("OTHERDRIVE"), "read_session", Uuid::new_v4());
    assert_eq!(
        tokio::time::timeout(StdDuration::from_secs(2), result)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .state,
        pb::TapeIndexVerificationState::VerifiedComplete as i32
    );
}

/// Execute a controlled WaitReady command through the real actor and catalog writer.
pub(crate) async fn run_readiness_test_command(state: &crate::ApiState, command: DriveCommand) {
    let id = match &command {
        DriveCommand::WaitReady { operation_id, .. } => *operation_id,
        _ => panic!("wait command"),
    };
    let mut world = VirtualWorld::single_drive("DEC418146K_LL02", 1, "8031BDC7D1", 0x400, 1);
    world.put_tape_in_drive(
        1,
        "S30002L9",
        None,
        VirtualTape::empty(1024 * 1024, 256 * 1024),
    );
    let world = Arc::new(Mutex::new(world));
    let model = world.lock().unwrap().library_snapshot();
    let policy = remanence_library::StaticAllowlist::new(["DEC418146K_LL02"]);
    let mut library = model
        .open_with(&policy, move |path| {
            let role = world.lock().unwrap().role_for_path(path).unwrap();
            Ok(Box::new(ModelTransport::new(world.clone(), role)) as Box<dyn SgTransport>)
        })
        .unwrap();
    let snapshot = library_snapshot_cell(library.library().clone());
    let cfg = test_write_owner_config(
        state.index_path.as_ref().clone(),
        state.audit_dir.as_ref().clone(),
        &library,
        snapshot,
    );
    let drive = library.open_drive(1, &policy).unwrap();
    let tx = spawn_drive_actor(1, drive, cfg);
    tx.send(command).await.unwrap();
    tokio::time::timeout(StdDuration::from_secs(2), async {
        while !state
            .operations
            .latest(&id)
            .is_some_and(|status| crate::operations::is_terminal(&status))
        {
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

/// Startup reconciliation leaves a transient that the serving daemon clears itself.
#[tokio::test]
async fn startup_resumable_fence_gets_an_automatic_wait() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("catalog.sqlite");
    let mut index = CatalogIndex::open(&path).unwrap();
    let id = Uuid::new_v4();
    record_unthreaded_readiness_operation(&mut index, "LIB-STARTAUTO", id);
    index
        .record_media_readiness_transition(
            crate::startup_media_readiness::media_readiness_initializing_load_transition(
                id,
                &MediaReadiness::BecomingReady {
                    ascq: 2,
                    media_initializing: true,
                },
            ),
        )
        .unwrap();
    let (world, mut library) = unthreaded_cartridge_world("LIB-STARTAUTO");
    let snapshot = library.library().clone();
    let policy = remanence_library::StaticAllowlist::new(["LIB-STARTAUTO"]);
    let mut opened = vec![(0x100, library.open_drive(0x100, &policy).unwrap())];
    crate::startup_media_readiness::reconcile_library_media_readiness_on_startup(
        &mut index,
        &snapshot,
        &mut opened,
    )
    .unwrap();
    assert_eq!(
        index.media_readiness_operation(id).unwrap().unwrap().state,
        "media_initializing"
    );
    world
        .lock()
        .unwrap()
        .drive_states
        .get_mut(&0x100)
        .unwrap()
        .initializing_command_required = false;
    let mut state = crate::ApiState::new(index);
    let snapshot = library_snapshot_cell(snapshot);
    state.library_snapshot = Some(snapshot.clone());
    state.default_library_serial = Some(Arc::new("LIB-STARTAUTO".into()));
    let cfg = test_write_owner_config(path, state.audit_dir.as_ref().clone(), &library, snapshot);
    let (_, drive) = opened.pop().unwrap();
    let tx = spawn_drive_actor(0x100, drive, cfg.clone());
    let changer = spawn_changer_actor(library.into_changer(), cfg);
    state.drive_pool = Some(DrivePool::new_for_library(
        "LIB-STARTAUTO",
        changer,
        HashMap::from([(0x100, tx)]),
        Arc::new(HashMap::from([(0x100, AtomicBool::new(false))])),
    ));
    crate::readiness_wait::spawn_auto_resume(state.clone());
    tokio::time::timeout(StdDuration::from_secs(2), async {
        while !state
            .index()
            .unwrap()
            .list_active_media_readiness_operations(None)
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        state.operations.latest(&id).unwrap().progress["started_by"],
        "daemon"
    );
    state.readiness_shutdown.store(true, Ordering::Release);
}

/// Operator cancellation survives reconciliation; only an explicit resume restarts it.
#[tokio::test]
async fn operator_cancelled_auto_wait_stays_stopped() {
    let harness = VerificationActorHarness::new();
    harness.readiness.lock().unwrap().extend([(2, 4, 1); 20]);
    let (_, result) = crate::tape_verification::start(
        &harness.state,
        AuditActor::System,
        harness.tape_uuid,
        StdDuration::ZERO,
    )
    .unwrap();
    let refusal = result.await.unwrap().unwrap_err();
    let id = crate::readiness_wait::ReadinessFence::from_status(&refusal)
        .unwrap()
        .0;
    crate::readiness_wait::spawn_auto_resume(harness.state.clone());
    tokio::time::timeout(StdDuration::from_secs(2), async {
        while harness.state.operations.latest(&id).is_none() {
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    harness.state.operations.request_cancel(&id).unwrap();
    tokio::time::timeout(StdDuration::from_secs(2), async {
        while harness.state.operations.latest(&id).unwrap().state
            != pb::OperationState::Cancelled as i32
        {
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let record = harness
        .state
        .index()
        .unwrap()
        .media_readiness_operation(id)
        .unwrap()
        .unwrap();
    assert_eq!(record.cancel_source.as_deref(), Some("operator"));
    assert!(crate::readiness_wait::is_resumable(&record.state));
    let snapshot = harness.state.current_library_snapshot().unwrap();
    let (drive, _) =
        open_test_drive_with_tur_script("LIB-VERIFY", vec![Some(readiness_fixed_sense(2, 4, 1))]);
    assert_eq!(
        crate::startup_media_readiness::reconcile_library_media_readiness_on_startup(
            &mut harness.state.index_write().unwrap(),
            &snapshot.report.libraries[0],
            &mut [(0x100, drive)]
        )
        .unwrap(),
        1
    );
    tokio::time::sleep(StdDuration::from_millis(350)).await;
    assert_eq!(
        harness.state.operations.latest(&id).unwrap().state,
        pb::OperationState::Cancelled as i32
    );
    harness.readiness.lock().unwrap().clear();
    harness
        .state
        .start_readiness_wait(id, 9000, 30, "client", "session_open")
        .unwrap();
    tokio::time::timeout(StdDuration::from_secs(2), async {
        while harness.state.operations.latest(&id).unwrap().state
            != pb::OperationState::Succeeded as i32
        {
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(harness.state.drive_pool().unwrap().busy_drives().is_empty());
    harness
        .state
        .readiness_shutdown
        .store(true, Ordering::Release);
}

/// Shutdown interrupts daemon and client waits without turning them into operator cancellations.
#[tokio::test]
async fn readiness_shutdown_releases_reservation_and_restart_resumes() {
    for started_by in ["daemon", "client"] {
        let harness = VerificationActorHarness::new();
        harness.readiness.lock().unwrap().extend([(2, 4, 1); 100]);
        let (_, result) = crate::tape_verification::start(
            &harness.state,
            AuditActor::System,
            harness.tape_uuid,
            StdDuration::ZERO,
        )
        .unwrap();
        let id =
            crate::readiness_wait::ReadinessFence::from_status(&result.await.unwrap().unwrap_err())
                .unwrap()
                .0;
        if started_by == "daemon" {
            crate::readiness_wait::spawn_auto_resume(harness.state.clone());
        } else {
            harness
                .state
                .start_readiness_wait(id, 9000, 30, "client", "session_open")
                .unwrap();
        }
        tokio::time::timeout(StdDuration::from_secs(2), async {
            while !harness.state.operations.latest(&id).is_some_and(|s| {
                s.progress
                    .get("sense")
                    .is_some_and(|sense| sense == "02/04/01")
            }) {
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(
            StdDuration::from_secs(2),
            harness.state.shutdown_drive_pool(),
        )
        .await
        .unwrap()
        .unwrap();
        let record = harness
            .state
            .index()
            .unwrap()
            .media_readiness_operation(id)
            .unwrap()
            .unwrap();
        assert!(crate::readiness_wait::is_resumable(&record.state));
        assert_ne!(record.cancel_source.as_deref(), Some("operator"));
        let status = harness.state.operations.latest(&id).unwrap();
        assert_eq!(status.state, pb::OperationState::Cancelled as i32);
        assert!(status.error_summary.contains("shutdown"), "{status:?}");
        assert_eq!(status.progress["state"], record.state);
        assert!(harness.state.drive_pool().unwrap().busy_drives().is_empty());
        assert!(harness
            .state
            .start_readiness_wait(id, 9000, 30, "client", "session_open")
            .is_err());

        // Reopen the catalog and reconcile a still-transient drive before new admission.
        let mut restarted =
            crate::ApiState::new(CatalogIndex::open(harness.state.index_path.as_ref()).unwrap());
        restarted.library_snapshot = harness.state.library_snapshot.clone();
        restarted.default_library_serial = harness.state.default_library_serial.clone();
        let (probe, _) = open_test_drive_with_tur_script(
            "LIB-VERIFY",
            vec![Some(readiness_fixed_sense(2, 4, 1))],
        );
        crate::startup_media_readiness::reconcile_library_media_readiness_on_startup(
            &mut restarted.index_write().unwrap(),
            &restarted
                .current_library_snapshot()
                .unwrap()
                .report
                .libraries[0],
            &mut [(0x100, probe)],
        )
        .unwrap();
        let changer = harness
            .state
            .drive_pool()
            .unwrap()
            .changer_tx("LIB-VERIFY")
            .unwrap();
        restarted.drive_pool = Some(DrivePool::new_for_library(
            "LIB-VERIFY",
            changer,
            HashMap::from([(0x100, harness.tx.clone())]),
            Arc::new(HashMap::from([(0x100, AtomicBool::new(false))])),
        ));
        harness.readiness.lock().unwrap().clear();
        crate::readiness_wait::spawn_auto_resume(restarted.clone());
        tokio::time::timeout(StdDuration::from_secs(2), async {
            while restarted
                .operations
                .latest(&id)
                .is_none_or(|s| s.state != pb::OperationState::Succeeded as i32)
            {
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            restarted.operations.latest(&id).unwrap().progress["started_by"],
            "daemon"
        );
        restarted.begin_shutdown();
    }
}

/// Startup reconciles operator cancellations while preserving their automatic-start exclusion.
#[test]
fn startup_reconciles_operator_cancelled_ready_and_absent_media() {
    for (absent, needs_load) in [(false, false), (true, false), (false, true)] {
        let temp = tempfile::tempdir().unwrap();
        let mut index = CatalogIndex::open(temp.path().join("state.sqlite")).unwrap();
        let id = Uuid::new_v4();
        record_unthreaded_readiness_operation(&mut index, "LIB-CANCEL", id);
        let (world, mut library) = unthreaded_cartridge_world("LIB-CANCEL");
        let policy = remanence_library::StaticAllowlist::new(["LIB-CANCEL"]);
        let mut drive = library.open_drive(0x100, &policy).unwrap();
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(id, "media_readiness_wait");
        registry.request_cancel(&id).unwrap();
        handle_drive_wait_ready(
            &mut index,
            &mut drive,
            DriveReadinessPoll {
                bay: 0x100,
                operation_id: id,
                family: MediaFamily::Lto9OrLater,
                options: remanence_library::MediaReadinessWaitOptions {
                    wait: true,
                    timeout: StdDuration::from_secs(9000),
                    poll_interval: StdDuration::from_secs(30),
                },
                cartridge_in_drive: true,
                phase: "grpc_wait_ready",
            },
            &handle,
        );
        assert_eq!(
            index
                .media_readiness_operation(id)
                .unwrap()
                .unwrap()
                .cancel_source
                .as_deref(),
            Some("operator")
        );
        world
            .lock()
            .unwrap()
            .drive_states
            .get_mut(&0x100)
            .unwrap()
            .initializing_command_required = needs_load;
        let mut snapshot = library.library().clone();
        if absent {
            snapshot.drive_bays[0].loaded = false;
            snapshot.drive_bays[0].loaded_tape = None;
        }
        assert_eq!(
            crate::startup_media_readiness::reconcile_library_media_readiness_on_startup(
                &mut index,
                &snapshot,
                &mut [(0x100, drive)]
            )
            .unwrap(),
            1
        );
        let record = index.media_readiness_operation(id).unwrap().unwrap();
        assert_eq!(
            record.cancel_source.as_deref(),
            if absent {
                Some("startup_reconcile")
            } else {
                None
            }
        );
        if absent {
            assert!(
                crate::startup_media_readiness::media_readiness_state_requires_release(
                    &record.state
                )
            );
            assert!(record.quarantine_id.is_some());
        } else {
            assert_eq!(record.state, "ready");
            assert!(index
                .list_active_media_readiness_operations(None)
                .unwrap()
                .is_empty());
        }
    }
}

/// Corrupt stored ids produce row-specific errors at both admission boundaries.
#[test]
fn readiness_admission_reports_invalid_stored_id() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut index = CatalogIndex::open(&path).unwrap();
    record_unthreaded_readiness_operation(&mut index, "LIB-BAD-ID", Uuid::new_v4());
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute(
            "update media_readiness_ops set operation_id = 'invalid-row-id'",
            [],
        )
        .unwrap();
    for bay in [0x100, 0x101] {
        let error = crate::startup_media_readiness::ensure_media_readiness_admitted(
            &index,
            "move",
            "LIB-BAD-ID",
            Some(bay),
            None,
            true,
        )
        .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Internal);
        assert!(error.message().contains("invalid-row-id"));
    }
    let error = session_open_reject_admission_conflicts(
        &mut index,
        &SessionOpenReadinessContext {
            action: "open",
            bay: 0x100,
            library_serial: "LIB-BAD-ID",
            barcode: Some("UNT001L9"),
            source_slot: None,
            drive_serial: None,
            needs_drive_load: false,
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Internal);
    assert!(error.message().contains("invalid-row-id"));
}

/// The real refusing actor wakes discovery promptly after an initially empty scan.
#[tokio::test]
async fn actor_fence_wakes_auto_resume_before_safety_sweep() {
    let harness = VerificationActorHarness::new();
    crate::readiness_wait::spawn_auto_resume(harness.state.clone());
    tokio::time::sleep(StdDuration::from_millis(100)).await;
    harness.readiness.lock().unwrap().extend([(2, 4, 1); 100]);
    let (_, result) = crate::tape_verification::start(
        &harness.state,
        AuditActor::System,
        harness.tape_uuid,
        StdDuration::ZERO,
    )
    .unwrap();
    let id =
        crate::readiness_wait::ReadinessFence::from_status(&result.await.unwrap().unwrap_err())
            .unwrap()
            .0;
    tokio::time::timeout(StdDuration::from_secs(1), async {
        while harness.state.operations.latest(&id).is_none() {
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        harness.state.operations.latest(&id).unwrap().progress["started_by"],
        "daemon"
    );
    tokio::time::timeout(
        StdDuration::from_secs(2),
        harness.state.shutdown_drive_pool(),
    )
    .await
    .unwrap()
    .unwrap();
}

/// Exercise a real recording error without blocking the paused discovery clock.
pub(crate) fn fail_readiness_test_command(state: &crate::ApiState, command: DriveCommand) {
    let DriveCommand::WaitReady {
        operation_id,
        family,
        options,
        handle,
        reservation,
        ..
    } = command
    else {
        panic!("wait command")
    };
    let (_, mut library) = unthreaded_cartridge_world("DEC418146K_LL02");
    let policy = remanence_library::StaticAllowlist::new(["DEC418146K_LL02"]);
    let mut drive = library.open_drive(0x100, &policy).unwrap();
    let mut index = CatalogIndex::open_read_only(state.index_path.as_ref()).unwrap();
    super::readiness::handle_drive_wait_ready_and_release(
        &mut index,
        &mut drive,
        DriveReadinessPoll {
            bay: 0x100,
            operation_id,
            family,
            options,
            cartridge_in_drive: false,
            phase: "readiness_poll",
        },
        &handle,
        Some(reservation),
    );
}

/// Startup attributes every release-only transition to reconciliation.
#[test]
fn startup_release_transition_keeps_reconcile_cancel_source() {
    for source in [None, Some("transport"), Some("operator")] {
        for release_only in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut index = CatalogIndex::open(temp.path().join("state.sqlite")).unwrap();
            let id = Uuid::new_v4();
            record_unthreaded_readiness_operation(&mut index, "LIB-SOURCE", id);
            let mut transition =
                crate::startup_media_readiness::startup_media_readiness_probe_transition(
                    id,
                    &remanence_library::MediaReadiness::BecomingReady {
                        ascq: 1,
                        media_initializing: false,
                    },
                );
            transition.cancel_source = source.map(str::to_string);
            if release_only {
                transition.state = "transport_unknown".into();
            }
            index.record_media_readiness_transition(transition).unwrap();
            let (_, library) = unthreaded_cartridge_world("LIB-SOURCE");
            let mut snapshot = library.library().clone();
            snapshot.drive_bays[0].loaded = false;
            snapshot.drive_bays[0].loaded_tape = None;
            crate::startup_media_readiness::reconcile_library_media_readiness_on_startup(
                &mut index,
                &snapshot,
                &mut [],
            )
            .unwrap();
            let record = index.media_readiness_operation(id).unwrap().unwrap();
            assert_eq!(record.cancel_source.as_deref(), Some("startup_reconcile"));
        }
    }
}

/// A detached verifier waits for a LoadDrive library hold to end before opening.
#[tokio::test]
async fn detached_verification_waits_for_load_drive_release() {
    for publish_first in [false, true] {
        let harness = VerificationActorHarness::with_tape_in_slot(true);
        let id = Uuid::new_v4();
        let pool = harness.state.drive_pool().unwrap();
        pool.reserve_library_exclusive("LIB-VERIFY").unwrap();
        pool.set_library_holder("LIB-VERIFY", "load_drive", id);
        let load = harness.state.operations.register(id, "load_drive");
        load.publish_state(pb::OperationState::Running, &[]);
        harness
            .state
            .index_write()
            .unwrap()
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id: id,
                run_id: None,
                endpoint: DriveKey::new("LIB-VERIFY", 0x100),
                changer_sg: None,
                drive_sg: None,
                drive_serial: None,
                barcode: Some("VERIFY001L9".into()),
                source_slot: Some(0x400),
                media_generation: Some(9),
                phase: "load_drive_readiness".into(),
                state: "becoming_ready".into(),
                dirty_scope: Some("drive+tape".into()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .unwrap();
        let (operation, mut result) = crate::tape_verification::start(
            &harness.state,
            AuditActor::System,
            harness.tape_uuid,
            StdDuration::from_secs(10),
        )
        .unwrap();
        let verify_id = Uuid::from_slice(&operation.operation_id).unwrap();
        tokio::time::timeout(StdDuration::from_secs(2), async {
            while harness
                .state
                .operations
                .latest(&verify_id)
                .is_none_or(|s| s.progress.get("phase").is_none_or(|p| p != "readiness"))
            {
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        harness
            .state
            .index_write()
            .unwrap()
            .record_media_readiness_transition(
                crate::startup_media_readiness::startup_media_readiness_probe_transition(
                    id,
                    &remanence_library::MediaReadiness::Ready,
                ),
            )
            .unwrap();
        tokio::time::sleep(StdDuration::from_millis(400)).await;
        assert!(result.try_recv().is_err());
        if publish_first {
            load.publish_state(pb::OperationState::Succeeded, &[]);
            tokio::time::sleep(StdDuration::from_millis(400)).await;
            assert!(result.try_recv().is_err());
            pool.release_library("LIB-VERIFY");
        } else {
            pool.release_library("LIB-VERIFY");
            load.publish_state(pb::OperationState::Succeeded, &[]);
        }
        let verdict = tokio::time::timeout(StdDuration::from_secs(5), result)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            verdict.state,
            pb::TapeIndexVerificationState::VerifiedComplete as i32
        );
        harness.state.shutdown_drive_pool().await.unwrap();
    }
}

/// A LoadDrive poll observes shutdown promptly and leaves its durable fence resumable.
#[tokio::test]
async fn load_drive_readiness_poll_stops_on_shutdown() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut index = CatalogIndex::open(&path).unwrap();
    let id = Uuid::nil();
    let registry = crate::operations::OperationRegistry::default();
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = registry.register(id, "load_drive").with_readiness_lifetime(
        shutdown.clone(),
        Arc::new(crate::drive_pool::ReadinessWake::default()),
    );
    let actor_path = path.clone();
    let audit_dir = temp.path().join("audit");
    let load_registry = registry.clone();
    let task = tokio::task::spawn_blocking(move || {
        let registry = load_registry;
        let mut world = VirtualWorld::single_drive("LIB-LOADSTOP", 0x100, "DRV-LOADSTOP", 0x400, 1);
        world.put_tape_in_slot(
            0x400,
            "LOAD001L9",
            VirtualTape::empty(1024 * 1024, 256 * 1024),
        );
        let world = Arc::new(Mutex::new(world));
        let model = world.lock().unwrap().library_snapshot();
        let mut library = model
            .open_with(
                &remanence_library::StaticAllowlist::new(["LIB-LOADSTOP"]),
                {
                    let registry = registry.clone();
                    move |path| {
                        let role = world.lock().unwrap().role_for_path(path).unwrap();
                        Ok(Box::new(VerificationCancelTransport {
                            readiness: Arc::new(Mutex::new([(2, 4, 1); 100].into())),
                            inner: ModelTransport::new(world.clone(), role),
                            registry: registry.clone(),
                            trigger: Arc::new(Mutex::new(None)),
                            fired: Arc::new(AtomicBool::new(false)),
                        }) as Box<dyn SgTransport>)
                    }
                },
            )
            .unwrap();
        let snapshot = library_snapshot_cell(library.library().clone());
        let cfg = test_write_owner_config(actor_path, audit_dir, &library, snapshot);
        record_library_event(
            &mut index,
            &cfg,
            &handle,
            "LIB-LOADSTOP",
            AuditEvent::OperationStarted,
            Default::default(),
        )
        .unwrap();
        let result = super::robotics::run_load_sequence(
            &mut index,
            &cfg,
            &handle,
            &mut library,
            0x400,
            0x100,
            true,
        );
        super::robotics::finish_library_operation(
            &mut index,
            &cfg,
            &handle,
            "LIB-LOADSTOP",
            result.clone(),
            Ok(()),
        );
        result
    });
    tokio::time::timeout(StdDuration::from_secs(2), async {
        while !registry
            .latest(&id)
            .is_some_and(|s| s.progress.contains_key("sense"))
        {
            assert!(!task.is_finished(), "load ends before readiness polling");
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    shutdown.store(true, Ordering::Release);
    let error = tokio::time::timeout(StdDuration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error.contains(crate::readiness_wait::SHUTDOWN_REASON),
        "{error}"
    );
    let record = CatalogIndex::open_read_only(&path)
        .unwrap()
        .media_readiness_operation(id)
        .unwrap()
        .unwrap();
    assert!(crate::readiness_wait::is_resumable(&record.state));
    assert_ne!(record.cancel_source.as_deref(), Some("operator"));
    let terminal = registry.latest(&id).unwrap();
    assert_eq!(terminal.progress.get("state"), Some(&record.state));
    assert_eq!(terminal.progress["sense"], "02/04/01");
    assert_eq!(
        terminal.progress["sense_name"],
        remanence_library::media_readiness_sense_name(4, 1).unwrap()
    );
    let mut state = crate::ApiState::new(CatalogIndex::open(&path).unwrap());
    state.operations = registry.clone();
    for n in 1..=2048 {
        registry
            .register(Uuid::from_u128(n), "retention_test")
            .publish_state(pb::OperationState::Succeeded, &[]);
    }
    assert!(registry.latest(&id).is_none(), "load status is pruned");
    for state in [
        state,
        crate::ApiState::new(CatalogIndex::open(&path).unwrap()),
    ] {
        use crate::pb::daemon_server::Daemon;
        let status = state
            .daemon_service()
            .get_operation(tonic::Request::new(pb::GetOperationRequest {
                operation_id: id.as_bytes().to_vec(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(status.state, pb::OperationState::Failed as i32);
        assert!(status.error_summary.contains("shutdown"), "{status:?}");
    }
}

/// Shutdown ends an open verification watch before the readiness deadline.
#[tokio::test]
async fn detached_verification_shutdown_ends_readiness_watch() {
    use crate::pb::daemon_server::Daemon;
    let harness = VerificationActorHarness::new();
    harness.readiness.lock().unwrap().extend([(2, 4, 1); 100]);
    crate::readiness_wait::spawn_auto_resume(harness.state.clone());
    let (operation, result) = crate::tape_verification::start(
        &harness.state,
        AuditActor::System,
        harness.tape_uuid,
        StdDuration::from_secs(900),
    )
    .unwrap();
    let id = Uuid::from_slice(&operation.operation_id).unwrap();
    let mut watch = harness
        .state
        .daemon_service()
        .watch_operation(tonic::Request::new(pb::GetOperationRequest {
            operation_id: operation.operation_id,
        }))
        .await
        .unwrap()
        .into_inner();
    tokio::time::timeout(StdDuration::from_secs(2), async {
        loop {
            let event = watch
                .next()
                .await
                .expect("watch stays open until readiness")
                .unwrap();
            if event
                .progress
                .get("phase")
                .is_some_and(|p| p == "readiness")
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    harness.state.begin_shutdown();
    let error = tokio::time::timeout(StdDuration::from_secs(2), result)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.message().contains("shutdown"), "{error}");
    let events = tokio::time::timeout(StdDuration::from_secs(2), watch.collect::<Vec<_>>())
        .await
        .unwrap();
    let terminal = events.last().unwrap().as_ref().unwrap();
    assert_eq!(terminal.state, pb::OperationState::Failed as i32);
    assert!(terminal.error_summary.contains("shutdown"));
    assert!(!harness.state.operations.is_live(&id));
    tokio::time::timeout(
        StdDuration::from_secs(2),
        harness.state.shutdown_drive_pool(),
    )
    .await
    .unwrap()
    .unwrap();
}

/// Drain readiness commands through the catalog-open failure path.
pub(crate) fn fail_readiness_actor(rx: mpsc::Receiver<DriveCommand>) {
    super::actor_runtime::drain_failed_drive_commands(rx, "open catalog: injected failure".into());
}

/// Every load outcome retains the durable classification and sense evidence.
#[test]
fn load_drive_terminal_progress_uses_durable_readiness() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut index = CatalogIndex::open(&path).unwrap();
    let (_, library) = unthreaded_cartridge_world("LIB-END");
    let cfg = test_write_owner_config(
        path,
        temp.path().join("audit"),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    let registry = crate::operations::OperationRegistry::default();
    for state in [
        "ready",
        "becoming_ready",
        "timeout_unknown",
        "terminal_error",
        "transport_unknown",
        "reservation_conflict",
    ] {
        let id = Uuid::new_v4();
        let handle = registry.register(id, "load_drive");
        record_unthreaded_readiness_operation(&mut index, "LIB-END", id);
        let mut transition =
            crate::startup_media_readiness::startup_media_readiness_probe_transition(
                id,
                &MediaReadiness::BecomingReady {
                    ascq: 1,
                    media_initializing: false,
                },
            );
        transition.state = state.into();
        index.record_media_readiness_transition(transition).unwrap();
        record_library_event(
            &mut index,
            &cfg,
            &handle,
            "LIB-END",
            AuditEvent::OperationStarted,
            Default::default(),
        )
        .unwrap();
        let action = match state {
            "ready" => Ok(()),
            "becoming_ready" => Err(crate::readiness_wait::SHUTDOWN_REASON.into()),
            _ => Err("load does not reach READY".into()),
        };
        finish_library_operation(&mut index, &cfg, &handle, "LIB-END", action, Ok(()));
        let status = registry.latest(&id).unwrap();
        assert_eq!(
            status.progress.get("state").map(String::as_str),
            Some(state),
            "{status:?}"
        );
        assert_eq!(status.progress["sense"], "02/04/01");
        assert_eq!(
            status.progress["sense_name"],
            remanence_library::media_readiness_sense_name(4, 1).unwrap()
        );
        assert_eq!(
            status.progress["phase"],
            match state {
                "ready" => "done",
                "becoming_ready" => "shutdown",
                _ => "execute",
            }
        );
        assert_eq!(
            status.state,
            if state == "ready" {
                pb::OperationState::Succeeded
            } else {
                pb::OperationState::Failed
            } as i32
        );
        // Catalog and pre-dispatch cancellation paths retain the same evidence.
        finish_library_operation(
            &mut index,
            &cfg,
            &handle,
            "LIB-END",
            Ok(()),
            Err("catalog unavailable".into()),
        );
        assert_eq!(registry.latest(&id).unwrap().progress["state"], state);
        cancel_library_operation(
            &mut index,
            &cfg,
            &handle,
            "LIB-END",
            "cancelled before dispatch",
        );
        assert_eq!(registry.latest(&id).unwrap().progress["state"], state);
    }
}

/// Shutdown is an informational end, with the fence available for resumption.
#[test]
fn load_drive_shutdown_logs_at_info() {
    let temp = tempfile::tempdir().unwrap();
    let log_path = temp.path().join("shutdown.log");
    let file = std::fs::File::create(&log_path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    let _log = tracing::subscriber::set_default(subscriber);
    let path = temp.path().join("state.sqlite");
    let mut index = CatalogIndex::open(&path).unwrap();
    let (_, library) = unthreaded_cartridge_world("LIB-LOG");
    let cfg = test_write_owner_config(
        path,
        temp.path().join("audit"),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    let registry = crate::operations::OperationRegistry::default();
    let id = Uuid::new_v4();
    let handle = registry.register(id, "load_drive");
    record_unthreaded_readiness_operation(&mut index, "LIB-LOG", id);
    record_library_event(
        &mut index,
        &cfg,
        &handle,
        "LIB-LOG",
        AuditEvent::OperationStarted,
        Default::default(),
    )
    .unwrap();
    finish_library_operation(
        &mut index,
        &cfg,
        &handle,
        "LIB-LOG",
        Err(crate::readiness_wait::SHUTDOWN_REASON.into()),
        Ok(()),
    );
    let log = std::fs::read_to_string(log_path).unwrap();
    assert!(
        log.contains("INFO") && log.contains("library operation stops for shutdown"),
        "{log}"
    );
    assert!(!log.contains("ERROR"), "{log}");
}

/// A cancellation is terminal only after its durable transition succeeds.
#[test]
fn readiness_operator_cancel_requires_recorded_transition() {
    for writable in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.sqlite");
        let mut index = CatalogIndex::open(&path).unwrap();
        let id = Uuid::new_v4();
        record_unthreaded_readiness_operation(&mut index, "LIB-CANCEL", id);
        if !writable {
            index = CatalogIndex::open_read_only(&path).unwrap();
        }
        let (_, mut library) = unthreaded_cartridge_world("LIB-CANCEL");
        let policy = remanence_library::StaticAllowlist::new(["LIB-CANCEL"]);
        let mut drive = library.open_drive(0x100, &policy).unwrap();
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(id, "media_readiness_wait");
        registry.request_cancel(&id).unwrap();
        handle_drive_wait_ready(
            &mut index,
            &mut drive,
            DriveReadinessPoll {
                bay: 0x100,
                operation_id: id,
                family: MediaFamily::Unknown,
                options: remanence_library::MediaReadinessWaitOptions {
                    wait: true,
                    timeout: StdDuration::from_secs(1),
                    poll_interval: StdDuration::from_millis(1),
                },
                cartridge_in_drive: false,
                phase: "readiness_poll",
            },
            &handle,
        );
        let status = registry.latest(&id).unwrap();
        let record = index.media_readiness_operation(id).unwrap().unwrap();
        if writable {
            assert_eq!(status.state, pb::OperationState::Cancelled as i32);
            assert_eq!(record.cancel_source.as_deref(), Some("operator"));
        } else {
            assert_eq!(status.state, pb::OperationState::Failed as i32);
            assert_eq!(status.progress["state"], "recording_failed");
            assert!(status
                .error_summary
                .contains("operator cancel is not recorded"));
            assert_ne!(record.cancel_source.as_deref(), Some("operator"));
        }
    }
}

/// Pause the actor's final inventory refresh so terminal ordering is observable.
struct LoadRefreshGate {
    inner: ModelTransport,
    armed: Arc<AtomicBool>,
    entered: std::sync::mpsc::SyncSender<()>,
    release: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
}

impl SgTransport for LoadRefreshGate {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        if cdb.first() == Some(&0xb8) && self.armed.swap(false, Ordering::SeqCst) {
            self.entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(StdDuration::from_secs(5))
                .unwrap();
        }
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        self.inner.execute_none(cdb)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
}

/// An attached load watcher can reserve the drive on its first terminal retry.
#[tokio::test]
async fn attached_load_retry_follows_actor_refresh_and_release() {
    for cancelled in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.sqlite");
        let mut index = CatalogIndex::open(&path).unwrap();
        let id = Uuid::new_v4();
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(id, "load_drive");
        handle.publish_state(pb::OperationState::Running, &[("phase", "execute")]);
        let mut world = VirtualWorld::single_drive("LIB-RETRY", 0x100, "DRV-RETRY", 0x400, 1);
        world.put_tape_in_slot(
            0x400,
            "RETRY001L9",
            VirtualTape::empty(1024 * 1024, 256 * 1024),
        );
        let world = Arc::new(Mutex::new(world));
        let model = world.lock().unwrap().library_snapshot();
        let armed = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = Arc::new(Mutex::new(release_rx));
        let mut library = model
            .open_with(&remanence_library::StaticAllowlist::new(["LIB-RETRY"]), {
                let armed = armed.clone();
                move |path| {
                    let role = world.lock().unwrap().role_for_path(path).unwrap();
                    Ok(Box::new(LoadRefreshGate {
                        inner: ModelTransport::new(world.clone(), role),
                        armed: armed.clone(),
                        entered: entered_tx.clone(),
                        release: release_rx.clone(),
                    }) as Box<dyn SgTransport>)
                }
            })
            .unwrap();
        let snapshot = library_snapshot_cell(library.library().clone());
        let mut cfg = test_write_owner_config(
            path.clone(),
            temp.path().join("audit"),
            &library,
            snapshot.clone(),
        );
        let key = DriveKey::new("LIB-RETRY", 0x100);
        cfg.reservations = Arc::new(HashMap::from([(key.clone(), AtomicBool::new(true))]));
        let reservations = cfg.reservations.clone();
        let (drive_tx, _drive_rx) = mpsc::channel(1);
        let (changer_tx, _changer_rx) = mpsc::channel(1);
        let pool = DrivePool::new_with_lifecycle(
            HashMap::from([("LIB-RETRY".into(), changer_tx)]),
            HashMap::from([(key.clone(), drive_tx)]),
            reservations.clone(),
            crate::drive_pool::DrivePoolLifecycle::default(),
        );
        let mut state = crate::ApiState::new(CatalogIndex::open(&path).unwrap());
        state.operations = registry.clone();
        let attached = state
            .start_readiness_wait(id, 9000, 30, "client", "session_open")
            .unwrap();
        assert_eq!(attached.operation_id, id.as_bytes());
        let mut watch = registry.watch(&id).unwrap();
        let task = if cancelled {
            registry.request_cancel(&id).unwrap();
            armed.store(true, Ordering::SeqCst);
            let actor = spawn_changer_actor(library.into_changer(), cfg);
            actor
                .send(ChangerCommand::Robotics {
                    library_serial: "LIB-RETRY".into(),
                    action: super::actor_protocol::RoboticsAction::Load {
                        slot: 0x400,
                        bay: 0x100,
                        wait_ready: true,
                    },
                    handle,
                })
                .await
                .unwrap();
            None
        } else {
            record_library_event(
                &mut index,
                &cfg,
                &handle,
                "LIB-RETRY",
                AuditEvent::OperationStarted,
                Default::default(),
            )
            .unwrap();
            let action =
                run_load_sequence(&mut index, &cfg, &handle, &mut library, 0x400, 0x100, true);
            assert!(action.is_ok(), "{action:?}");
            let mut changer = library.into_changer();
            armed.store(true, Ordering::SeqCst);
            Some(tokio::task::spawn_blocking(move || {
                let guard =
                    ExclusiveGuard::from_reserved_library(cfg.reservations.clone(), "LIB-RETRY");
                super::actor_runtime::finish_robotics_and_release(
                    &mut index,
                    &cfg,
                    &mut changer,
                    "LIB-RETRY",
                    &handle,
                    RoboticsOutcome::Completed {
                        action,
                        observation: Ok(()),
                    },
                    guard,
                );
            }))
        };
        tokio::task::spawn_blocking(move || {
            entered_rx.recv_timeout(StdDuration::from_secs(5)).unwrap()
        })
        .await
        .unwrap();
        let live_during_refresh = registry.is_live(&id);
        let reserved_during_refresh = reservations[&key].load(Ordering::Acquire);
        release_tx.send(()).unwrap();
        assert!(
            live_during_refresh,
            "terminal status precedes the actor refresh"
        );
        assert!(reserved_during_refresh);
        let terminal = tokio::time::timeout(StdDuration::from_secs(5), async {
            loop {
                let status = watch.next().await.unwrap().unwrap();
                if crate::operations::is_terminal(&status) {
                    break status;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            terminal.state,
            if cancelled {
                pb::OperationState::Cancelled
            } else {
                pb::OperationState::Succeeded
            } as i32
        );
        let retry = pool
            .reserve_drive(&key)
            .expect("first attached-client retry acquires the drive");
        if !cancelled {
            assert!(
                snapshot
                    .read()
                    .unwrap()
                    .report
                    .library("LIB-RETRY")
                    .unwrap()
                    .drive_bays[0]
                    .loaded
            );
        }
        drop(retry);
        pool.reserve_library_exclusive("LIB-RETRY")
            .expect("the library is free for a retry that needs the robot");
        pool.release_library("LIB-RETRY");
        if let Some(task) = task {
            task.await.unwrap();
        }
    }
}

/// Exercise ingress, internal barriers, caller checkpoints, and close against virtual media.
pub(crate) async fn check_overlap_batch_receipts(
    state: &StateHandle,
    world: Arc<Mutex<VirtualWorld>>,
    selected: SelectedTape,
    serial_mode: bool,
) {
    use crate::pb::write_session_service_server::WriteSessionService;
    let mut config = state.config().clone();
    assert_eq!(
        config.daemon.append_staging_mode,
        remanence_state::AppendStagingMode::Overlap
    );
    config.daemon.append_overlap_min_bytes = Some(1024 * 1024);
    if serial_mode {
        config.daemon.append_staging_mode = remanence_state::AppendStagingMode::Serial;
    }
    let mut api_state = crate::ApiState::new_with_config(
        CatalogIndex::open(&state.paths().sqlite_path).unwrap(),
        &config,
    )
    .unwrap();
    let mut library = open_model_library(world);
    let mut cfg = test_write_owner_config(
        state.paths().sqlite_path.clone(),
        state.paths().audit_dir.clone(),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.checkpoint_journal_dir = state.paths().journal_dir.join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    cfg.checkpoint_max_objects = 100;
    cfg.checkpoint_max_age_seconds = 3600;
    let serial = library.library().serial.clone();
    let drive = library
        .open_drive(
            0x100,
            &remanence_library::StaticAllowlist::new([serial.as_str()]),
        )
        .unwrap();
    let tx = spawn_drive_actor(0x100, drive, cfg);
    let (reply, rx) = oneshot::channel();
    tx.send(DriveCommand::OpenWrite {
        actor: remanence_state::AuditActor::System,
        pool_cfg: state.config().tape_pools[0].clone(),
        selected: selected.clone(),
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
        needs_drive_load: false,
        library_serial: serial.clone(),
        barcode: Some("HAND".into()),
        source_slot: None,
        drive_uuid: None,
        drive_serial: None,
        reply,
    })
    .await
    .unwrap();
    let session = rx.await.unwrap().unwrap();
    let id = Uuid::from_slice(&session.session_id).unwrap();
    let (changer_tx, _changer_rx) = mpsc::channel(1);
    let pool = DrivePool::new_for_library(
        &serial,
        changer_tx,
        HashMap::from([(0x100, tx)]),
        Arc::new(HashMap::from([(0x100, AtomicBool::new(false))])),
    );
    pool.record_session(
        id,
        MountedSession {
            bay: 0x100,
            library_serial: serial,
            barcode: Some("HAND".into()),
            home_slot: None,
            tape_uuid: selected.tape_uuid,
            drive_uuid: None,
        },
    );
    let spool = state.paths().state_dir.join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    api_state.spool_dir = Some(Arc::new(spool));
    api_state.spool_budget_bytes = Some(config.daemon.io_memory_ceiling);
    api_state.drive_pool = Some(pool);
    let api = api_state.write_session_service();
    let append = |name: &'static str, size: usize| {
        let api = api.clone();
        async move {
            let bytes = vec![0x61; size];
            let digest = Sha256::digest(&bytes).to_vec();
            let messages = vec![
                pb::AppendObjectMessage {
                    payload: Some(pb::append_object_message::Payload::Start(
                        pb::AppendObjectStart {
                            session_id: id.as_bytes().to_vec(),
                            caller_object_id: name.into(),
                            declared_size_bytes: Some(size as u64),
                            expected_content_sha256: Some(digest.clone()),
                            source_replay_capability: pb::SourceReplayCapability::ReplayFromStart
                                as i32,
                            ..Default::default()
                        },
                    )),
                },
                pb::AppendObjectMessage {
                    payload: Some(pb::append_object_message::Payload::Chunk(
                        pb::AppendObjectChunk {
                            session_id: id.as_bytes().to_vec(),
                            data: bytes,
                        },
                    )),
                },
                pb::AppendObjectMessage {
                    payload: Some(pb::append_object_message::Payload::Finish(
                        pb::AppendObjectFinish {
                            session_id: id.as_bytes().to_vec(),
                            expected_content_sha256: Some(digest),
                            ..Default::default()
                        },
                    )),
                },
            ];
            api.append_object_from_stream(tonic::Request::new(tokio_stream::iter(
                messages.into_iter().map(Ok),
            )))
            .await
            .unwrap()
            .into_inner()
        }
    };
    let checkpoint = || async {
        api.checkpoint_session(tonic::Request::new(pb::CheckpointSessionRequest {
            session_id: id.as_bytes().to_vec(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner()
    };
    let mut seen = std::collections::HashSet::new();
    for names in [
        vec!["small-a", "small-b", "small-c"],
        vec!["ring-a"],
        vec!["mixed-a", "mixed-b", "ring-b"],
    ] {
        let mut expected = std::collections::HashSet::new();
        for name in names {
            let ring = name.starts_with("ring");
            let record = append(name, if ring { 1024 * 1024 } else { 31 }).await;
            assert_eq!(
                record.append_commit_info.as_ref().unwrap().durability,
                if ring && !serial_mode {
                    pb::AppendDurability::Checkpointed
                } else {
                    pb::AppendDurability::Written
                } as i32
            );
            expected.insert(record.object_id);
        }
        let reply = checkpoint().await;
        assert_eq!(
            reply
                .committed_objects
                .iter()
                .map(|r| r.object_id.clone())
                .collect::<std::collections::HashSet<_>>(),
            expected
        );
        assert_eq!(reply.committed_objects.len(), expected.len());
        for object in reply.committed_objects {
            assert!(seen.insert(object.object_id));
        }
        assert!(checkpoint().await.committed_objects.is_empty());
    }
    // Close delivers a retained ring receipt when no caller checkpoint claims it.
    let final_ring = append("ring-close", 1024 * 1024).await;
    let closed = api
        .close_write_session(tonic::Request::new(pb::CloseWriteSessionRequest {
            session_id: id.as_bytes().to_vec(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(closed.committed_copies.len(), 1);
    assert_eq!(
        closed.checkpointed_objects[0].object_id,
        final_ring.object_id
    );
    assert!(seen.insert(closed.checkpointed_objects[0].object_id.clone()));
    assert_eq!(seen.len(), 8);
    let journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
        state.paths().journal_dir.join("tapes"),
        selected.tape_uuid,
    )
    .build()
    .unwrap();
    let records = journal.replay().unwrap();
    assert_eq!(
        records.len(),
        4,
        "small objects share one barrier in either staging mode"
    );
    let sidecars = records
        .iter()
        .filter_map(|r| r.barrier_bundle.as_ref())
        .flat_map(|b| &b.entries)
        .filter(|e| e.kind == TapeFileKind::ParitySidecar)
        .count();
    assert_eq!(sidecars, 0, "ordinary barriers leave the short epoch open");
}

thread_local! {
    static FAIL_APPEND_CONTEXT_AFTER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Fail one context renewal on the selected actor thread after a durable barrier.
pub(crate) fn fail_append_context_now() -> bool {
    FAIL_APPEND_CONTEXT_AFTER.with(|count| {
        let remaining = count.get();
        count.set(remaining.saturating_sub(1));
        remaining == 1
    })
}

/// Exercise each post-barrier error path and both subsequent receipt delivery paths.
pub(crate) async fn check_context_failure_receipts(
    state: &StateHandle,
    world: Arc<Mutex<VirtualWorld>>,
    selected: SelectedTape,
    trigger: &str,
    deliver_on_close: bool,
) {
    let mut library = open_model_library(world);
    let mut cfg = test_write_owner_config(
        state.paths().sqlite_path.clone(),
        state.paths().audit_dir.clone(),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.checkpoint_journal_dir = state.paths().journal_dir.join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    cfg.checkpoint_max_objects = if trigger == "threshold" { 1 } else { 100 };
    cfg.checkpoint_max_age_seconds = 3600;
    let serial = library.library().serial.clone();
    let mut drive = library
        .open_drive(
            0x100,
            &remanence_library::StaticAllowlist::new([serial.as_str()]),
        )
        .unwrap();
    let (tx, mut rx) = mpsc::channel(4);
    let actor_tx = tx.clone();
    let (open_reply, open_rx) = oneshot::channel();
    let request = super::actor_runtime::OpenWriteActorRequest {
        actor: AuditActor::System,
        pool_cfg: state.config().tape_pools[0].clone(),
        selected: selected.clone(),
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
        needs_drive_load: false,
        library_serial: Some(serial),
        barcode: Some("HAND".into()),
        source_slot: None,
        drive_uuid: None,
        drive_serial: None,
        reply: open_reply,
    };
    let actor = tokio::task::spawn_blocking(move || {
        let mut index = CatalogIndex::open(&cfg.index_path).unwrap();
        FAIL_APPEND_CONTEXT_AFTER.with(|count| count.set(2));
        handle_drive_open_write(
            0x100, &mut index, &cfg, actor_tx, &mut rx, &mut drive, &mut 0, request, &mut None,
        );
        assert_eq!(
            FAIL_APPEND_CONTEXT_AFTER.with(|count| count.get()),
            0,
            "context failure is exercised"
        );
    });
    let session = open_rx.await.unwrap().unwrap();
    let id = Uuid::from_slice(&session.session_id).unwrap();
    let first = append_actor_test_file(
        &tx,
        id,
        state.paths().state_dir.join("first.bin"),
        "first.bin",
        "first",
        b"first",
    )
    .await;
    let checkpoint = |trigger| {
        let tx = tx.clone();
        async move {
            let (reply, rx) = oneshot::channel();
            tx.send(DriveCommand::Checkpoint {
                session_id: id,
                trigger,
                expected_batch_id: None,
                reply: Some(reply),
            })
            .await
            .unwrap();
            rx.await.unwrap()
        }
    };
    // Internal checkpoint replies retain the receipt for the external caller.
    let retained = checkpoint(CheckpointTrigger::Append).await.unwrap();
    assert_eq!(retained.committed_objects.len(), 1);
    assert_eq!(
        retained.committed_objects[0].object_id,
        first.record.object_id
    );
    let second = append_actor_test_file_result(
        &tx,
        id,
        state.paths().state_dir.join("second.bin"),
        "second.bin",
        "second",
        b"second",
    )
    .await;
    let close = || {
        let tx = tx.clone();
        async move {
            let (reply, rx) = oneshot::channel();
            tx.send(DriveCommand::Close {
                session_id: id,
                reply,
            })
            .await
            .unwrap();
            rx.await.unwrap()
        }
    };
    let error = match trigger {
        "threshold" => second.expect_err("threshold context renewal fails"),
        "explicit" => {
            second.unwrap();
            checkpoint(CheckpointTrigger::Explicit)
                .await
                .expect_err("explicit context renewal fails")
        }
        "close" => {
            second.unwrap();
            close().await.expect_err("close context renewal fails")
        }
        _ => panic!("unknown trigger"),
    };
    assert!(
        error
            .message()
            .contains("injected next append context failure"),
        "{error}"
    );
    let (session, receipts) = if deliver_on_close {
        let closed = close().await.unwrap();
        let receipts = closed.session.checkpointed_objects.clone();
        (closed.session, receipts)
    } else {
        let recovered = checkpoint(CheckpointTrigger::Explicit).await.unwrap();
        let empty = checkpoint(CheckpointTrigger::Explicit).await.unwrap();
        assert!(empty.committed_objects.is_empty());
        assert!(close()
            .await
            .unwrap()
            .session
            .checkpointed_objects
            .is_empty());
        (recovered.session, recovered.committed_objects)
    };
    assert_eq!(
        receipts.len(),
        2,
        "both durable receipts survive {trigger} context failure"
    );
    assert_eq!(
        receipts
            .iter()
            .map(|r| &r.object_id)
            .collect::<HashSet<_>>()
            .len(),
        2
    );
    assert_eq!(session.objects_committed, 2);
    assert_eq!(session.bytes_committed, 11);
    for record in receipts {
        assert_eq!(
            record.append_commit_info.unwrap().durability,
            pb::AppendDurability::Checkpointed as i32
        );
        let stored = CatalogIndex::open(&state.paths().sqlite_path)
            .unwrap()
            .get_native_object(&Uuid::from_slice(&record.object_id).unwrap().to_string())
            .unwrap()
            .unwrap();
        assert!(stored.copies.iter().any(|copy| copy.status == "committed"));
    }
    actor.await.unwrap();
    let journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
        state.paths().journal_dir.join("tapes"),
        selected.tape_uuid,
    )
    .build()
    .unwrap();
    assert_eq!(journal.replay().unwrap().len(), 2);
}

/// Changes that invalidate a seated accumulator before the next session.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetentionInvalidation {
    None,
    ParkedGeneration,
    IdentityGeneration,
    ExpectedEnd,
    OtherDrive,
    NoHomeSlot,
    Discarded,
    Fenced,
    WriteFailure,
    PositionFailure,
    JournalFailure,
    NoWriteShutdown,
    Finalize,
    FinalizeRebuilt,
    PlannerRefusesBeforeReplicaA,
    PlannerRefusesSealed,
    SealDue,
    ShutdownSealDue,
    ShutdownSealDueBusy,
    DeferredSealOnly,
    Disconnected,
    Removal(&'static str, &'static str),
    AutomaticSeal,
    ProjectionBusy(bool, &'static str),
    StaleProjection(bool, &'static str),
    ApiEpochFills,
}

/// Release the host lock before reopening the same durable state for recovery.
fn restart_state_for_replay(state: &mut StateHandle) {
    let paths = state.paths().clone();
    let config = state.config().clone();
    let temporary = tempfile::tempdir().unwrap();
    let mut placeholder = config.clone();
    placeholder.daemon.state_dir = temporary.path().to_path_buf();
    placeholder.audit.dir = temporary.path().join("audit");
    placeholder.journal.dir = temporary.path().join("journals");
    placeholder.index.sqlite_path = temporary.path().join("index.sqlite");
    placeholder.cache.tape_catalog_dir = temporary.path().join("cache");
    let placeholder_paths = remanence_state::StatePaths::from_config(
        temporary.path().join("config.toml"),
        &placeholder,
    );
    let placeholder = StateHandle::open_with_config(placeholder_paths, placeholder).unwrap();
    drop(std::mem::replace(state, placeholder));
    *state = StateHandle::open_with_config(paths, config).unwrap();
}

/// Compare every tape's safety projection and active fences across authoritative replay.
pub(crate) fn assert_rebuild_safety_equivalent(state: &mut StateHandle) {
    fn snapshot(state: &StateHandle) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        let conn = rusqlite::Connection::open(&state.paths().sqlite_path).unwrap();
        ["select tape_uuid, state, seal_due, highest_protected_ordinal, total_committed_ordinals,
            finalization_progress, finalization_trigger, finalization_operation_id,
            finalization_edition_digest, finalization_layout_digest, completed_replicas, finalization_outcome
            from tapes order by tape_uuid",
         "select tape_uuid, barcode, state, reason, evidence_json, quarantine_id, release_ack, endpoint_key
            from tape_io_fences where state = 'active' order by quarantine_id"]
            .into_iter().map(|sql| {
                let mut statement = conn.prepare(sql).unwrap();
                let columns = statement.column_count();
                statement.query_map([], |row| (0..columns).map(|i| row.get(i)).collect::<Result<Vec<_>, _>>())
                    .unwrap().collect::<Result<Vec<_>, _>>().unwrap()
            }).collect()
    }
    let before = snapshot(state);
    state.rebuild_index_from_journals().unwrap();
    crate::reconcile_checkpoint_journal_projections(state).unwrap();
    assert_eq!(
        snapshot(state),
        before,
        "live and rebuilt safety projections differ"
    );
}

/// Seated one-object sessions retain their accumulator and removal commits its sidecar.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn check_seated_parity_sessions(
    state: &mut StateHandle,
    world: Arc<Mutex<VirtualWorld>>,
    selected: SelectedTape,
    refuse_memory: bool,
    shutdown: bool,
    read_before_shutdown: bool,
    invalidation: RetentionInvalidation,
    failed_close: bool,
) {
    let barcode = CatalogIndex::open(&state.paths().sqlite_path)
        .unwrap()
        .get_tape(&selected.tape_uuid)
        .unwrap()
        .unwrap()
        .voltag
        .unwrap();
    let read_fault = Arc::new(AtomicU64::new(0));
    if matches!(
        invalidation,
        RetentionInvalidation::Removal(_, _)
            | RetentionInvalidation::ProjectionBusy(_, _)
            | RetentionInvalidation::StaleProjection(_, _)
    ) {
        let mut w = world.lock().unwrap();
        w.slots.push(remanence_chaos::model::SlotState {
            address: 0x400,
            full: false,
            barcode: None,
            access: true,
            except: false,
        });
        w.element_layout.slot_count = 1;
        w.drive_source_slots.insert(0x100, Some(0x400));
    }
    let model = world.lock().unwrap().library_snapshot();
    let mut library = model
        .open_with(
            &remanence_library::StaticAllowlist::new([model.serial.as_str()]),
            {
                let world = world.clone();
                let fault = read_fault.clone();
                let index_path = state.paths().sqlite_path.clone();
                move |path| {
                    let role = world.lock().unwrap().role_for_path(path).unwrap();
                    Ok(Box::new(EpochRemovalTransport {
                        inner: ModelTransport::new(world.clone(), role),
                        world: world.clone(),
                        fault: fault.clone(),
                        located: 0,
                        index_path: index_path.clone(),
                    }))
                }
            },
        )
        .unwrap();
    let mut cfg = test_write_owner_config(
        state.paths().sqlite_path.clone(),
        state.paths().audit_dir.clone(),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.tape_pools = state.config().tape_pools.clone();
    if matches!(invalidation, RetentionInvalidation::Removal("no_pool", _)) {
        cfg.tape_pools.clear();
    }
    if matches!(
        invalidation,
        RetentionInvalidation::SealDue | RetentionInvalidation::DeferredSealOnly
    ) {
        cfg.tape_pools[0].watermark_low = 1e-12;
    }

    cfg.checkpoint_journal_dir = state.paths().journal_dir.join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    cfg.checkpoint_max_age_seconds = 3600;
    let lifecycle = crate::drive_pool::DrivePoolLifecycle::default();
    cfg.lifecycle = Some(lifecycle.clone());
    let memory = cfg.io_memory.clone();
    let mut held = None;
    let serial = library.library().serial.clone();
    let drive = library
        .open_drive(
            0x100,
            &remanence_library::StaticAllowlist::new([serial.as_str()]),
        )
        .unwrap();
    let mut tx = spawn_drive_actor(0x100, drive, cfg.clone());
    if invalidation == RetentionInvalidation::ApiEpochFills {
        check_api_epoch_fills(state, world, selected, cfg, lifecycle, tx, library).await;
        return;
    }
    if let RetentionInvalidation::StaleProjection(fresh, path) = invalidation {
        check_open_checkpoint_projection_busy(
            state, world, selected, cfg, lifecycle, tx, library, fresh, path,
        )
        .await;
        return;
    }
    let mut prior_reads = None;
    for number in 0..if invalidation == RetentionInvalidation::NoWriteShutdown {
        3
    } else {
        2
    } {
        drop(held.take());
        if number == 1 && invalidation == RetentionInvalidation::ParkedGeneration {
            let mut parked = lifecycle.parked.lock().unwrap();
            for entry in parked.by_drive.values_mut() {
                entry.generation += 1;
            }
        }
        if number == 1 && invalidation == RetentionInvalidation::ExpectedEnd {
            // Another owner advances durable authority while the original
            // actor still has the earlier accumulator and parked generation.
            let drive = library
                .open_drive(
                    0x100,
                    &remanence_library::StaticAllowlist::new([serial.as_str()]),
                )
                .unwrap();
            let mut other_cfg = cfg.clone();
            other_cfg.lifecycle = None;
            let other = spawn_drive_actor(0x100, drive, other_cfg);
            let (reply, received) = oneshot::channel();
            other
                .send(DriveCommand::OpenWrite {
                    actor: remanence_state::AuditActor::System,
                    pool_cfg: state.config().tape_pools[0].clone(),
                    selected: selected.clone(),
                    target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                    needs_drive_load: false,
                    library_serial: serial.clone(),
                    barcode: Some(barcode.as_str().into()),
                    source_slot: None,
                    drive_uuid: None,
                    drive_serial: None,
                    reply,
                })
                .await
                .unwrap();
            let id = Uuid::from_slice(&received.await.unwrap().unwrap().session_id).unwrap();
            append_actor_test_file(
                &other,
                id,
                state.paths().state_dir.join("intervening.bin"),
                "intervening.bin",
                "intervening",
                b"intervening committed payload",
            )
            .await;
            let (reply, received) = oneshot::channel();
            other
                .send(DriveCommand::Close {
                    session_id: id,
                    reply,
                })
                .await
                .unwrap();
            received.await.unwrap().unwrap();
        }
        if number == 1 && invalidation == RetentionInvalidation::IdentityGeneration {
            memory.epoch_test_action.store(5, Ordering::Relaxed);
        }
        let discards_before = memory.epoch_discards();
        let before = world.lock().unwrap().command_log.len();
        let (reply, received) = oneshot::channel();
        let mut session_pool = state.config().tape_pools[0].clone();
        if number == 1 && invalidation == RetentionInvalidation::AutomaticSeal {
            let tape = CatalogIndex::open(&state.paths().sqlite_path)
                .unwrap()
                .get_tape(&selected.tape_uuid)
                .unwrap()
                .unwrap();
            // The second Object crosses this physical low watermark while its epoch remains short.
            session_pool.capacity_cap_bytes = Some(128 * 1024 * 1024 * 1024);
            session_pool.watermark_low = ((tape.written_extent_lba.unwrap() + 1)
                * u64::from(selected.block_size)) as f64
                / session_pool.capacity_cap_bytes.unwrap() as f64;
        }
        tx.send(DriveCommand::OpenWrite {
            actor: remanence_state::AuditActor::System,
            pool_cfg: session_pool,
            selected: selected.clone(),
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: false,
            library_serial: serial.clone(),
            barcode: Some(barcode.as_str().into()),
            source_slot: None,
            drive_uuid: None,
            drive_serial: None,
            reply,
        })
        .await
        .unwrap();
        let opened = received.await.unwrap().unwrap();
        memory.epoch_test_action.store(0, Ordering::Relaxed);
        let reads = world.lock().unwrap().command_log[before..]
            .iter()
            .filter(|command| command.opcode == 0x08)
            .count();
        if !refuse_memory {
            if let Some(first) = prior_reads {
                if matches!(
                    invalidation,
                    RetentionInvalidation::ParkedGeneration
                        | RetentionInvalidation::IdentityGeneration
                        | RetentionInvalidation::ExpectedEnd
                ) {
                    assert_eq!(
                        memory.epoch_discards(),
                        discards_before + 1,
                        "changed generation discards the epoch"
                    );
                } else {
                    assert!(
                        reads <= first,
                        "retained epoch avoids Object rereads: {reads} > {first}"
                    );
                }
            }
        }
        prior_reads = Some(reads);
        let id = Uuid::from_slice(&opened.session_id).unwrap();
        lifecycle.sessions.lock().unwrap().insert(
            id,
            MountedSession {
                bay: 0x100,
                library_serial: serial.clone(),
                barcode: Some(barcode.as_str().into()),
                home_slot: (invalidation != RetentionInvalidation::NoHomeSlot).then_some(0x400),
                tape_uuid: selected.tape_uuid,
                drive_uuid: None,
            },
        );
        if number < 2 {
            append_actor_test_file(
                &tx,
                id,
                state.paths().state_dir.join(format!("seated-{number}.bin")),
                "seated.bin",
                &format!(
                    "seated-{}-{number}",
                    crate::bytes_to_hex(&selected.tape_uuid)
                ),
                format!(
                    "committed seated payload {}",
                    crate::bytes_to_hex(&selected.tape_uuid)
                )
                .as_bytes(),
            )
            .await;
        }
        if refuse_memory {
            held = memory.try_reserve(memory.ceiling());
            assert!(held.is_some());
        }
        if number == 1
            && matches!(
                invalidation,
                RetentionInvalidation::ShutdownSealDue | RetentionInvalidation::ShutdownSealDueBusy
            )
        {
            lifecycle.shutting_down.store(true, Ordering::SeqCst);
            memory.epoch_test_action.store(
                if invalidation == RetentionInvalidation::ShutdownSealDueBusy {
                    11
                } else {
                    7
                },
                Ordering::Relaxed,
            );
        }
        let (reply, received) = oneshot::channel();
        if number == 1 && invalidation == RetentionInvalidation::AutomaticSeal {
            read_fault.store(1, Ordering::SeqCst);
        }
        tx.send(DriveCommand::Close {
            session_id: id,
            reply,
        })
        .await
        .unwrap();
        let result = received.await.unwrap();
        if number == 1 && invalidation == RetentionInvalidation::ShutdownSealDueBusy {
            assert!(result
                .unwrap_err()
                .message()
                .contains("SQLite projection failed"));
            memory.epoch_test_action.store(0, Ordering::Relaxed);
            let (reply, received) = oneshot::channel();
            tx.send(DriveCommand::Close {
                session_id: id,
                reply,
            })
            .await
            .unwrap();
            received.await.unwrap().unwrap();
            let journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
                &cfg.checkpoint_journal_dir,
                selected.tape_uuid,
            )
            .build()
            .unwrap();
            assert_eq!(
                journal.last().unwrap().unwrap().seal_due,
                Some(remanence_state::SealDueReason::HardwareEarlyWarning)
            );
            let before = CatalogIndex::open(&state.paths().sqlite_path)
                .unwrap()
                .get_tape(&selected.tape_uuid)
                .unwrap()
                .unwrap();
            assert!(before.seal_due.is_none());
            crate::reconcile_checkpoint_journal_projections(state).unwrap();
            assert_eq!(
                CatalogIndex::open(&state.paths().sqlite_path)
                    .unwrap()
                    .get_tape(&selected.tape_uuid)
                    .unwrap()
                    .unwrap()
                    .seal_due
                    .as_deref(),
                Some("hardware_early_warning")
            );
        } else {
            result.unwrap();
        }
        memory.epoch_test_action.store(0, Ordering::Relaxed);
        lifecycle.sessions.lock().unwrap().remove(&id);
        let index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
        let tape = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
        if number == 1 && invalidation == RetentionInvalidation::AutomaticSeal {
            assert_eq!(tape.state, "sealed");
            assert_eq!(
                read_fault.load(Ordering::SeqCst),
                1,
                "live automatic seal never rebuilds"
            );
            let authority = remanence_state::TapeAuthority::open(
                &cfg.checkpoint_journal_dir,
                selected.tape_uuid,
                &cfg.index_path,
            )
            .unwrap();
            let snapshot = authority.freeze().unwrap();
            let mut entries = Vec::new();
            remanence_state::authority::TapeAuthoritySnapshot::visit_file_rows(
                &snapshot,
                &mut |row| {
                    entries.push(row.clone());
                    Ok(())
                },
            )
            .unwrap();
            let entry = entries
                .iter()
                .find(|e| e.kind == TapeFileKind::ParityMap)
                .unwrap();
            let w = world.lock().unwrap();
            let start = entry.physical_start_hint.unwrap() as usize;
            let blocks = w.tapes[barcode.as_str()].records
                [start..start + entry.block_count as usize]
                .iter()
                .map(|r| match r {
                    Record::Block(b) => b.clone(),
                    _ => panic!("map block"),
                })
                .collect::<Vec<_>>();
            let map = parse_parity_map_tape_file(&blocks, &selected.tape_uuid).unwrap();
            assert_ne!(
                map.payload.directory.entries.last().unwrap().flags
                    & SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH,
                0
            );
            assert_eq!(memory.granted(), 0);
            return;
        }
        assert!(tape.total_committed_ordinals > 0);
        assert_eq!(
            tape.highest_protected_ordinal == tape.total_committed_ordinals,
            refuse_memory
        );
    }
    drop(held);
    if !refuse_memory && invalidation != RetentionInvalidation::ShutdownSealDueBusy {
        assert!(memory.granted() > 0);
    }
    if let RetentionInvalidation::ProjectionBusy(append, path) = invalidation {
        check_session_projection_busy(
            state, world, selected, cfg, library, lifecycle, append, path,
        )
        .await;
        return;
    }
    if let RetentionInvalidation::Removal(case, path) = invalidation {
        check_epoch_removal(
            state, world, selected, cfg, lifecycle, tx, library, case, path, read_fault,
        )
        .await;
        return;
    }
    if matches!(
        invalidation,
        RetentionInvalidation::Finalize
            | RetentionInvalidation::FinalizeRebuilt
            | RetentionInvalidation::PlannerRefusesBeforeReplicaA
            | RetentionInvalidation::PlannerRefusesSealed
    ) {
        let mut index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
        let assignment_generation = index
            .get_tape_assignment_snapshot(&selected.tape_uuid)
            .unwrap()
            .unwrap()
            .assignment_generation;
        let mut request = ManualFinalizeTapeActorRequest {
            candidate_operation_id: Uuid::new_v4(),
            actor: AuditActor::System,
            actor_fingerprint: "epoch-test".into(),
            idempotency_key: Uuid::new_v4(),
            request_fingerprint: [0x51; 32],
            tape_uuid: selected.tape_uuid,
            expected_pool_id: Some(selected.pool_id.clone()),
            assignment_generation,
            reason: "close committed epoch".into(),
            block_size: selected.block_size,
            parity_config: selected.parity_config.clone(),
            pool_config: Some(state.config().tape_pools[0].clone()),
        };
        if invalidation == RetentionInvalidation::FinalizeRebuilt {
            memory.epoch_test_action.store(4, Ordering::Relaxed);
        }
        let before = world.lock().unwrap().command_log.len();
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::PlanFinalization {
            ownership: crate::mount::PlanningOwnership::model(),
            request: request.clone(),
            needs_drive_load: false,
            barcode: Some(barcode.as_str().into()),
            source_slot: None,
            drive_serial: None,
            reply,
        })
        .await
        .unwrap();
        let prefix = received.await.unwrap().into_result().unwrap();
        memory.epoch_test_action.store(0, Ordering::Relaxed);
        assert!(world.lock().unwrap().command_log[before..]
            .iter()
            .all(|c| !matches!(c.opcode, 0x0a | 0x10)));
        super::terminal_finalize::preflight_manual_finalize_with_prefix(
            &mut index,
            ManualFinalizePreflightConfig {
                mounted_endpoint: None,
                checkpoint_journal_dir: &cfg.checkpoint_journal_dir,
                audit_dir: &cfg.audit_dir,
                audit_fsync: cfg.audit_fsync,
                audit_append_lock: &cfg.audit_append_lock,
            },
            Some(barcode.as_str()),
            &mut request,
            Some(&prefix),
        )
        .unwrap();
        if invalidation == RetentionInvalidation::PlannerRefusesBeforeReplicaA {
            use remanence_state::authority::{TapeAuthorityView, TapeAuthorityWriter};
            let authority = remanence_state::TapeAuthority::open(
                &cfg.checkpoint_journal_dir,
                selected.tape_uuid,
                &cfg.index_path,
            )
            .unwrap();
            let mut lease = authority.acquire_exclusive_for_terminal_recovery().unwrap();
            assert_eq!(
                lease.pending_finalization().unwrap().unwrap().progress,
                remanence_state::TerminalFinalizationProgress::BeforeReplicaA
            );
            let scheme = match &selected.parity_config {
                ParityConfig::Scheme(s) => s.clone(),
                _ => unreachable!(),
            };
            let rows = lease.open_structural(selected.block_size, scheme).unwrap();
            lease.retain_structural_publisher(rows).unwrap();
            let prefix_plan = remanence_state::authority::AuthorityPrefixPlan::from(&prefix);
            lease
                .publish_terminal_prefix(&prefix_plan.publication)
                .unwrap();
            drop(lease);
            assert_epoch_planner_refusal(&tx, &request, &barcode, &world).await;
            return;
        }
        let sealed_request = request.clone();
        read_fault.store(1, Ordering::SeqCst);
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::FinalizeTape {
            request,
            needs_drive_load: false,
            library_serial: serial,
            barcode: Some(barcode.as_str().into()),
            source_slot: None,
            drive_serial: None,
            drive_uuid: None,
            reply,
        })
        .await
        .unwrap();
        received.await.unwrap().unwrap();
        assert_eq!(
            read_fault.load(Ordering::SeqCst),
            1,
            "terminal execution never rereads the Object prefix"
        );
        let entry = prefix
            .committed_bundle
            .entries
            .iter()
            .find(|e| e.kind == TapeFileKind::ParityMap)
            .unwrap();
        {
            let world_guard = world.lock().unwrap();
            let start = entry.physical_start_hint.unwrap() as usize;
            let blocks = world_guard.tapes[barcode.as_str()].records
                [start..start + entry.block_count as usize]
                .iter()
                .map(|r| match r {
                    Record::Block(b) => b.clone(),
                    _ => panic!("ParityMap delimiter inside payload"),
                })
                .collect::<Vec<_>>();
            let parsed =
                remanence_parity::parse_parity_map_tape_file(&blocks, &selected.tape_uuid).unwrap();
            assert_ne!(
                parsed.payload.directory.entries.last().unwrap().flags
                    & remanence_parity::SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH,
                0
            );
            let records = remanence_state::authority::fixtures::AuthorityBuilder::new(
                &cfg.checkpoint_journal_dir,
                selected.tape_uuid,
            )
            .build()
            .unwrap()
            .replay()
            .unwrap();
            assert_eq!(
                records.len(),
                3,
                "finalization has no intervening barrier close"
            );
            assert!(records.last().unwrap().sealed_after_write);
            assert_eq!(memory.granted(), 0);
        }
        if invalidation == RetentionInvalidation::PlannerRefusesSealed {
            read_fault.store(0, Ordering::SeqCst);
            assert_epoch_planner_refusal(&tx, &sealed_request, &barcode, &world).await;
        }
        assert_rebuild_safety_equivalent(state);
        return;
    }
    if invalidation == RetentionInvalidation::Disconnected {
        let (reply, received) = oneshot::channel();
        drop(received);
        tx.send(DriveCommand::OpenWrite {
            actor: AuditActor::System,
            pool_cfg: state.config().tape_pools[0].clone(),
            selected: selected.clone(),
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: false,
            library_serial: serial.clone(),
            barcode: Some(barcode.as_str().into()),
            source_slot: None,
            drive_uuid: None,
            drive_serial: None,
            reply,
        })
        .await
        .unwrap();
    }
    let fault = match invalidation {
        RetentionInvalidation::WriteFailure => 1,
        RetentionInvalidation::PositionFailure => 2,
        RetentionInvalidation::JournalFailure => 3,
        RetentionInvalidation::Discarded => 4,
        RetentionInvalidation::SealDue => 0,
        _ => 0,
    };
    if fault != 0 {
        memory.epoch_test_action.store(fault, Ordering::Relaxed);
    }
    if invalidation == RetentionInvalidation::Fenced {
        CatalogIndex::open(&state.paths().sqlite_path)
            .unwrap()
            .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                tape_uuid: selected.tape_uuid,
                barcode: Some(barcode.as_str().into()),
                reason: "transfer_error".into(),
                evidence_json: None,
            })
            .unwrap();
    }
    if read_before_shutdown {
        let before = world.lock().unwrap().command_log.len();
        let (stream_tx, mut stream_rx) = mpsc::channel(64);
        let drain = tokio::spawn(async move { while stream_rx.recv().await.is_some() {} });
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::TapeInventory {
            tape_uuid: selected.tape_uuid,
            needs_drive_load: false,
            library_serial: serial.clone(),
            barcode: Some(barcode.as_str().into()),
            source_slot: None,
            drive_serial: None,
            stream_tx,
            reply,
        })
        .await
        .unwrap();
        let _ = received.await.unwrap();
        drain.await.unwrap();
        assert!(world.lock().unwrap().command_log[before..]
            .iter()
            .any(|command| command.opcode == 0x08));
    }
    if !shutdown && !failed_close && !refuse_memory && invalidation == RetentionInvalidation::None {
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::PrepareRemoval {
            reply,
            progress: None,
        })
        .await
        .unwrap();
        received.await.unwrap().unwrap();
        let index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
        let tape = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
        assert_eq!(
            tape.highest_protected_ordinal,
            tape.total_committed_ordinals
        );
        assert_eq!(memory.granted(), 0);
    }
    let saved_bootstrap = if failed_close {
        let mut world = world.lock().unwrap();
        let tape = world.tapes.get_mut(barcode.as_str()).unwrap();
        let saved = tape.records[0].clone();
        let bootstrap = crate::build_tape_bootstrap(
            [0x44; 16],
            selected.block_size,
            selected.parity_config.clone(),
            "2026-10-28T00:00:00Z",
            "test",
        );
        let mut bytes = vec![0; selected.block_size as usize];
        remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, &mut bytes).unwrap();
        tape.records[0] = Record::Block(bytes);
        Some(saved)
    } else {
        None
    };
    lifecycle.shutting_down.store(shutdown, Ordering::SeqCst);
    let before_unload = world.lock().unwrap().command_log.len();
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Unload { reply }).await.unwrap();
    let unloaded = received.await.unwrap();
    if invalidation == RetentionInvalidation::Fenced {
        assert_eq!(
            unloaded.unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        assert_eq!(
            world.lock().unwrap().command_log.len(),
            before_unload,
            "fenced retained epoch issues no tape commands"
        );
        assert_rebuild_safety_equivalent(state);
        return;
    }
    unloaded.unwrap();
    assert!(world.lock().unwrap().command_log[before_unload..]
        .iter()
        .any(|c| c.opcode == 0x1b));
    if read_before_shutdown {
        assert!(world.lock().unwrap().command_log[before_unload..]
            .iter()
            .all(|c| !matches!(c.opcode, 0x08 | 0x0a | 0x10 | 0x2b | 0x92)));
        assert!(FileAuditLog::replay(&state.paths().audit_dir)
            .unwrap()
            .iter()
            .any(|record| record.detail.get("session_kind")
                == Some(&CborValue::Text("epoch_left_open".into()))));
    }
    if (1..=3).contains(&fault) || invalidation == RetentionInvalidation::Fenced {
        let index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
        assert!(!index.list_active_tape_io_fences().unwrap().is_empty());
        let tape = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
        assert!(tape.highest_protected_ordinal < tape.total_committed_ordinals);
        memory.epoch_test_action.store(0, Ordering::Relaxed);
        let before = world.lock().unwrap().command_log.len();
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::PrepareRemoval {
            reply,
            progress: None,
        })
        .await
        .unwrap();
        assert_eq!(
            received.await.unwrap().unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        assert!(world.lock().unwrap().command_log[before..]
            .iter()
            .all(|c| !matches!(c.opcode, 0x0a | 0x10)));
        assert_eq!(memory.granted(), 0);
        assert_rebuild_safety_equivalent(state);
        return;
    }
    assert_eq!(memory.granted(), 0);
    if let Some(_saved) = saved_bootstrap {
        let index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
        let fences = index.list_active_tape_io_fences().unwrap();
        assert_eq!(fences.len(), 1);
        let evidence: serde_json::Value =
            serde_json::from_str(fences[0].evidence_json.as_deref().unwrap()).unwrap();
        assert!(evidence["error"].as_str().unwrap().contains("identity"));
        assert!(
            evidence["open_epoch_start"].as_u64().unwrap()
                < evidence["open_epoch_end"].as_u64().unwrap()
        );
        return;
    }
    let index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
    let tape = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
    assert_eq!(
        tape.highest_protected_ordinal == tape.total_committed_ordinals,
        !read_before_shutdown,
        "audit: {:?}",
        FileAuditLog::replay(&state.paths().audit_dir).unwrap()
    );
    let records = remanence_state::authority::fixtures::AuthorityBuilder::new(
        state.paths().journal_dir.join("tapes"),
        selected.tape_uuid,
    )
    .build()
    .unwrap()
    .replay()
    .unwrap();
    assert!(
        records.iter().all(|r| !r.sealed_after_write),
        "removal never finalizes"
    );
    assert_eq!(
        records.len(),
        if refuse_memory || read_before_shutdown {
            2
        } else if invalidation == RetentionInvalidation::ExpectedEnd {
            4
        } else {
            3
        }
    );
    if !refuse_memory && !read_before_shutdown {
        assert!(records.last().unwrap().objects.is_empty());
        assert!(records.last().unwrap().barrier_bundle.is_some());
    }
    if matches!(
        invalidation,
        RetentionInvalidation::SealDue
            | RetentionInvalidation::DeferredSealOnly
            | RetentionInvalidation::ShutdownSealDue
            | RetentionInvalidation::ShutdownSealDueBusy
    ) {
        let expected_reason = if matches!(
            invalidation,
            RetentionInvalidation::SealDue | RetentionInvalidation::DeferredSealOnly
        ) {
            "reached_low_watermark"
        } else {
            "hardware_early_warning"
        };
        assert_eq!(tape.seal_due.as_deref(), Some(expected_reason));
        lifecycle.shutting_down.store(false, Ordering::SeqCst);
        memory.epoch_test_action.store(0, Ordering::Relaxed);
        // Disconnect the old actor and reopen host state before startup and rebuild.
        // No retained accumulator or session state supplies the pending trigger.
        let drive = library
            .open_drive(
                0x100,
                &remanence_library::StaticAllowlist::new([serial.as_str()]),
            )
            .unwrap();
        tx = spawn_drive_actor(0x100, drive, cfg.clone());
        restart_state_for_replay(state);
        crate::reconcile_checkpoint_journal_projections(state).unwrap();
        assert_rebuild_safety_equivalent(state);
        assert_eq!(
            index
                .get_tape(&selected.tape_uuid)
                .unwrap()
                .unwrap()
                .seal_due
                .as_deref(),
            Some(expected_reason)
        );
        assert!(crate::catalog_conversion::tape_to_proto(tape)
            .seal_due
            .is_some());
        if invalidation == RetentionInvalidation::DeferredSealOnly {
            return;
        }
        let mut other_index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
        let other_uuid = [0x29; 16];
        other_index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: other_uuid,
                voltag: "AAAOTHER".into(),
                block_size: selected.block_size,
                parity: selected.parity_config.clone(),
                force: false,
            })
            .unwrap();
        other_index
            .set_tape_generation(
                other_uuid,
                remanence_state::MediaObservation {
                    generation: remanence_state::LtoGen::Lto9,
                    source: remanence_state::GenerationSource::Operator,
                },
            )
            .unwrap();
        other_index
            .project_tape_pool_membership(other_uuid, &selected.pool_id)
            .unwrap();
        let other = crate::select_tape_in_pool(
            &other_index,
            &state.config().tape_pools[0],
            1,
            &HashSet::from([selected.tape_uuid]),
        )
        .unwrap();
        assert_eq!(other.tape_uuid, other_uuid);
        assert!(other_index
            .get_tape(&selected.tape_uuid)
            .unwrap()
            .unwrap()
            .seal_due
            .is_some());
        let object_count = records.last().unwrap().committed_object_count;
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::OpenWrite {
            actor: AuditActor::System,
            pool_cfg: state.config().tape_pools[0].clone(),
            selected: selected.clone(),
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: true,
            library_serial: serial.clone(),
            barcode: Some(barcode.as_str().into()),
            source_slot: None,
            drive_uuid: None,
            drive_serial: None,
            reply,
        })
        .await
        .unwrap();
        let error = received.await.unwrap().unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(
            error.message().contains("seal") && error.message().contains("retry OpenWriteSession"),
            "{error}"
        );
        let recovered = remanence_state::authority::fixtures::AuthorityBuilder::new(
            &cfg.checkpoint_journal_dir,
            selected.tape_uuid,
        )
        .build()
        .unwrap()
        .replay()
        .unwrap();
        assert_eq!(
            recovered.last().unwrap().committed_object_count,
            object_count
        );
        assert!(recovered.last().unwrap().objects.is_empty());
        let tape = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
        assert_eq!(tape.state, "sealed");
        assert!(tape.seal_due.is_none());
        let expected_trigger = if matches!(
            invalidation,
            RetentionInvalidation::SealDue | RetentionInvalidation::DeferredSealOnly
        ) {
            remanence_state::TerminalFinalizationTrigger::ReachedLowWatermark
        } else {
            remanence_state::TerminalFinalizationTrigger::HardwareEarlyWarning
        };
        assert_eq!(
            tape.terminal_finalization.unwrap().trigger,
            expected_trigger
        );
        assert_rebuild_safety_equivalent(state);
        assert!(index
            .get_tape(&selected.tape_uuid)
            .unwrap()
            .unwrap()
            .seal_due
            .is_none());
    }
    if read_before_shutdown {
        // A fresh actor has no accumulator from the stopped actor. Its open
        // rebuilds the committed range before another Object reaches media.
        lifecycle.shutting_down.store(false, Ordering::SeqCst);
        let bay = if invalidation == RetentionInvalidation::OtherDrive {
            let mut world = world.lock().unwrap();
            let cartridge = world.drive_bays.get_mut(&0x100).unwrap().take();
            world.drive_bays.insert(0x101, cartridge);
            world.drive_serials.insert(0x101, "SECOND-DRIVE".into());
            world.drive_source_slots.insert(0x101, None);
            world.drive_states.insert(0x101, Default::default());
            world.drive_alert_flags.insert(0x101, Default::default());
            world.element_layout.drive_count = 2;
            0x101
        } else {
            0x100
        };
        let mut library = open_model_library(world.clone());
        let drive = library
            .open_drive(
                bay,
                &remanence_library::StaticAllowlist::new([serial.as_str()]),
            )
            .unwrap();
        let tx = spawn_drive_actor(bay, drive, cfg);
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::OpenWrite {
            actor: remanence_state::AuditActor::System,
            pool_cfg: state.config().tape_pools[0].clone(),
            selected: selected.clone(),
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: true,
            library_serial: serial.clone(),
            barcode: Some(barcode.as_str().into()),
            source_slot: None,
            drive_uuid: None,
            drive_serial: None,
            reply,
        })
        .await
        .unwrap();
        let id = Uuid::from_slice(&received.await.unwrap().unwrap().session_id).unwrap();
        lifecycle.sessions.lock().unwrap().insert(
            id,
            MountedSession {
                bay,
                library_serial: serial,
                barcode: Some(barcode.as_str().into()),
                home_slot: Some(0x400),
                tape_uuid: selected.tape_uuid,
                drive_uuid: None,
            },
        );
        append_actor_test_file(
            &tx,
            id,
            state.paths().state_dir.join("after-restart.bin"),
            "after-restart.bin",
            "after-restart",
            b"new committed payload",
        )
        .await;
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::Close {
            session_id: id,
            reply,
        })
        .await
        .unwrap();
        received.await.unwrap().unwrap();
        lifecycle.sessions.lock().unwrap().remove(&id);
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::Unload { reply }).await.unwrap();
        received.await.unwrap().unwrap();
        assert_eq!(memory.granted(), 0);
        let tape = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
        assert_eq!(
            tape.highest_protected_ordinal,
            tape.total_committed_ordinals
        );
    }
}
/// A resumed session with no retention grant preserves a durable close on SQLite busy.
#[allow(clippy::too_many_arguments)]
async fn check_session_projection_busy(
    state: &StateHandle,
    world: Arc<Mutex<VirtualWorld>>,
    selected: SelectedTape,
    cfg: WriteOwnerConfig,
    mut library: remanence_library::LibraryHandle,
    lifecycle: crate::drive_pool::DrivePoolLifecycle,
    append: bool,
    path: &str,
) {
    let memory = cfg.io_memory.clone();
    let held = memory
        .try_reserve(memory.ceiling() - memory.granted())
        .unwrap();
    let drive = library
        .open_drive(
            0x100,
            &remanence_library::StaticAllowlist::new([cfg.library().serial.as_str()]),
        )
        .unwrap();
    let tx = spawn_drive_actor(0x100, drive, cfg.clone());
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::OpenWrite {
        actor: AuditActor::System,
        pool_cfg: cfg.tape_pools[0].clone(),
        selected: selected.clone(),
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
        needs_drive_load: false,
        library_serial: cfg.library().serial.clone(),
        barcode: Some("HAND".into()),
        source_slot: None,
        drive_uuid: None,
        drive_serial: None,
        reply,
    })
    .await
    .unwrap();
    let session = received.await.unwrap().unwrap();
    let id = Uuid::from_slice(&session.session_id).unwrap();
    drop(held);
    if append {
        append_actor_test_file(
            &tx,
            id,
            state.paths().state_dir.join("projection.bin"),
            "projection.bin",
            "projection",
            b"committed before SQLite projection",
        )
        .await;
    }
    let _held = memory
        .try_reserve(memory.ceiling() - memory.granted())
        .unwrap();
    memory.epoch_test_action.store(10, Ordering::Relaxed);
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Close {
        session_id: id,
        reply,
    })
    .await
    .unwrap();
    let error = received.await.unwrap().unwrap_err();
    assert!(
        error.message().contains("SQLite projection failed"),
        "{error}"
    );
    assert!(
        error.message().contains("locked"),
        "real SQLite busy: {error}"
    );
    memory.epoch_test_action.store(0, Ordering::Relaxed);
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Close {
        session_id: id,
        reply,
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
    let mut index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
    assert!(index.list_active_tape_io_fences().unwrap().is_empty());
    let tape = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
    assert!(tape.highest_protected_ordinal < tape.total_committed_ordinals);
    if path != "startup" {
        check_epoch_removal(
            state,
            world,
            selected,
            cfg,
            lifecycle,
            tx,
            library,
            "replay_closed",
            path,
            Arc::new(AtomicU64::new(0)),
        )
        .await;
        return;
    }
    crate::startup_checkpoint::replay_checkpoint_journal_projections_with_audit(
        &mut index,
        &cfg.checkpoint_journal_dir,
        &cfg.audit_dir,
        &cfg.audit_append_lock,
    )
    .unwrap();
    let records = remanence_state::authority::fixtures::AuthorityBuilder::new(
        &cfg.checkpoint_journal_dir,
        selected.tape_uuid,
    )
    .build()
    .unwrap()
    .replay()
    .unwrap();
    assert_eq!(
        records.last().unwrap().eod_lba as usize,
        world.lock().unwrap().tapes["HAND"].records.len(),
        "no abandonment filemark follows the durable EOD"
    );
    let tape = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
    assert_eq!(
        tape.highest_protected_ordinal,
        tape.total_committed_ordinals
    );
}

/// A durable open checkpoint survives a stale closed or fresh catalog projection.
#[allow(clippy::too_many_arguments)]
async fn check_open_checkpoint_projection_busy(
    state: &StateHandle,
    world: Arc<Mutex<VirtualWorld>>,
    selected: SelectedTape,
    cfg: WriteOwnerConfig,
    lifecycle: crate::drive_pool::DrivePoolLifecycle,
    tx: mpsc::Sender<DriveCommand>,
    library: remanence_library::LibraryHandle,
    fresh: bool,
    path: &str,
) {
    for number in 0..if fresh { 1 } else { 2 } {
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::OpenWrite {
            actor: AuditActor::System,
            pool_cfg: cfg.tape_pools[0].clone(),
            selected: selected.clone(),
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: false,
            library_serial: cfg.library().serial.clone(),
            barcode: Some("HAND".into()),
            source_slot: Some(0x400),
            drive_uuid: None,
            drive_serial: None,
            reply,
        })
        .await
        .unwrap();
        let id = Uuid::from_slice(&received.await.unwrap().unwrap().session_id).unwrap();
        append_actor_test_file(
            &tx,
            id,
            state
                .paths()
                .state_dir
                .join(format!("projection-{number}.bin")),
            "projection.bin",
            &format!("projection-{number}"),
            b"durable open checkpoint",
        )
        .await;
        let fail_projection = fresh || number == 1;
        let held = if fail_projection {
            cfg.io_memory.epoch_test_action.store(10, Ordering::Relaxed);
            None
        } else {
            // The first session leaves a closed epoch in both authorities.
            Some(cfg.io_memory.try_reserve(cfg.io_memory.ceiling()).unwrap())
        };
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::Close {
            session_id: id,
            reply,
        })
        .await
        .unwrap();
        let result = received.await.unwrap();
        if fail_projection {
            let error = result.unwrap_err();
            assert!(
                error.message().contains("SQLite projection failed"),
                "{error}"
            );
            assert!(error.message().contains("locked"), "{error}");
            cfg.io_memory.epoch_test_action.store(0, Ordering::Relaxed);
            let (reply, received) = oneshot::channel();
            tx.send(DriveCommand::Close {
                session_id: id,
                reply,
            })
            .await
            .unwrap();
            received.await.unwrap().unwrap();
        } else {
            result.unwrap();
        }
        drop(held);
    }
    if path == "shutdown" {
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::OpenRead {
            tape_uuid: selected.tape_uuid,
            needs_drive_load: false,
            library_serial: cfg.library().serial.clone(),
            barcode: Some("HAND".into()),
            source_slot: Some(0x400),
            drive_uuid: None,
            drive_serial: None,
            resume_target: None,
            daemon_epoch: 1,
            reply,
        })
        .await
        .unwrap();
        let id = Uuid::from_slice(&received.await.unwrap().unwrap().session_id).unwrap();
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::CloseRead {
            session_id: id,
            reply,
        })
        .await
        .unwrap();
        received.await.unwrap().unwrap();
    }
    if fresh {
        // The failed first projection leaves no geometry in the fresh catalog row.
        rusqlite::Connection::open(&state.paths().sqlite_path).unwrap().execute(
                "UPDATE tapes SET scheme_id = NULL, data_blocks_per_stripe = NULL, parity_blocks_per_stripe = NULL, stripes_per_neighborhood = NULL WHERE tape_uuid = ?1",
                [selected.tape_uuid.as_slice()],
            ).unwrap();
    }
    let index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
    let tape = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
    assert_eq!(
        tape.highest_protected_ordinal,
        tape.total_committed_ordinals
    );
    assert_eq!(tape.scheme_id.is_none(), fresh);
    check_epoch_removal(
        state,
        world,
        selected,
        cfg,
        lifecycle,
        tx,
        library,
        if path == "shared_lease" {
            "stale_shared_lease"
        } else if fresh {
            "no_scheme"
        } else {
            "stale_open"
        },
        if path == "shared_lease" { "idle" } else { path },
        Arc::new(AtomicU64::new(0)),
    )
    .await;
}

/// A write that must first evict a seated cartridge: the operator pulls the
/// magazine holding its home slot after the mount was planned. Returns the
/// mount outcome, the world, the live-status waits observed, and the daemon
/// state for the caller to shut down.
async fn write_mount_with_magazine_opened_during_eviction(
    wait_seconds: u64,
) -> (
    Result<(u16, Option<u16>), Status>,
    Arc<Mutex<VirtualWorld>>,
    Vec<String>,
    crate::ApiState,
    tempfile::TempDir,
) {
    let (outcome, world, seen, api, temp) =
        magazine_eviction_scenario(wait_seconds, MagazineEvictionEntry::Mount).await;
    (
        outcome.map(|done| done.expect("mount outcome")),
        world,
        seen,
        api,
        temp,
    )
}

/// How the magazine-eviction scenario enters the write path.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MagazineEvictionEntry {
    /// Mount resolution and the load move only.
    Mount,
    /// The full pinned write open, whose caller goes away while the
    /// eviction's move home waits for the magazine.
    WriteOpenCallerGone,
}

async fn magazine_eviction_scenario(
    wait_seconds: u64,
    entry: MagazineEvictionEntry,
) -> (
    Result<Option<(u16, Option<u16>)>, Status>,
    Arc<Mutex<VirtualWorld>>,
    Vec<String>,
    crate::ApiState,
    tempfile::TempDir,
) {
    const SERIAL: &str = "LIB-MAGAZINE";
    const POOL: &str = "magazine-pool";
    const HOME: u16 = 0x0400;
    const NEW_SLOT: u16 = 0x0401;
    let temp = tempfile::Builder::new()
        .prefix("remanence-magazine-eviction-")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let new_uuid: TapeUuid = [0x5e; 16];
    let mut index = CatalogIndex::open(&index_path).expect("catalog");
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: new_uuid,
            voltag: "NEW001L9".to_string(),
            block_size: 262_144,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision target tape");
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: POOL.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .project_tape_pool_membership(new_uuid, POOL)
        .expect("assign pool tape");
    drop(index);
    let caller_gone = Arc::new(AtomicBool::new(false));
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        SERIAL, 0x0100, "DRV-MAG", HOME, 2,
    )));
    {
        let mut world = world.lock().unwrap();
        world.put_tape_in_drive(0x0100, "OLD001L9", Some(HOME), VirtualTape::default());
        world.put_tape_in_slot(NEW_SLOT, "NEW001L9", VirtualTape::default());
    }
    let mut library = open_model_library(world.clone());
    let mut cfg = test_write_owner_config(
        index_path.clone(),
        temp.path().join("audit"),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    std::fs::create_dir_all(&cfg.audit_dir).unwrap();
    let board = crate::live_status::ChangerWaitBoard::default();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observer = {
        let board_observer = board.observer();
        let board = board.clone();
        let seen = Arc::clone(&seen);
        let caller_gone = Arc::clone(&caller_gone);
        Arc::new(move |event: &remanence_library::ChangerWaitEvent<'_>| {
            if entry == MagazineEvictionEntry::WriteOpenCallerGone
                && event.phase == remanence_library::ChangerWaitPhase::Waiting
            {
                caller_gone.store(true, Ordering::SeqCst);
            }
            board_observer(event);
            if let Some(wait) = board.get(event.library_serial) {
                seen.lock().unwrap().push(wait);
            }
        })
    };
    cfg.changer_ready_wait =
        remanence_library::ChangerReadyWait::bounded(StdDuration::from_secs(wait_seconds))
            .with_poll_interval(StdDuration::from_millis(1))
            .with_observer(observer);
    cfg.tape_pools = vec![remanence_state::TapePoolConfig {
        scheme: None,
        id: POOL.to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: 262_144,
        min_object_size_bytes: 0,
    }];
    library.set_changer_ready_wait(cfg.changer_ready_wait.clone());
    let key = crate::drive_pool::DriveKey::new(SERIAL, 0x0100);
    let drive = library.open_drive(0x0100, &cfg.policy).unwrap();
    let lifecycle = crate::drive_pool::DrivePoolLifecycle::default();
    cfg.lifecycle = Some(lifecycle.clone());
    let actor = spawn_drive_actor(0x0100, drive, cfg.clone());
    let changer = spawn_changer_actor(library.into_changer(), cfg.clone());
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(SERIAL.to_string(), changer)]),
        HashMap::from([(key.clone(), actor)]),
        Arc::new(HashMap::from([(key, AtomicBool::new(false))])),
        lifecycle,
    );
    let mut api = crate::ApiState::new_with_pool_configs(
        CatalogIndex::open(&index_path).unwrap(),
        cfg.tape_pools.clone(),
    );
    api.audit_dir = Arc::new(cfg.audit_dir.clone());
    api.library_snapshot = Some(cfg.library().snapshot.clone());
    api.drive_pool = Some(pool);
    api.changer_waits = board.clone();
    {
        // The plan above saw an empty, reachable home slot; now the
        // magazine carrying it comes out, and goes back after three polls.
        let mut world = world.lock().unwrap();
        world.remove_magazine([HOME]);
        world
            .removed_magazine
            .as_mut()
            .unwrap()
            .reinsert_after_polls = Some(3);
    }
    let outcome = match entry {
        MagazineEvictionEntry::Mount => crate::mount::test_mount_for_write(&api, SERIAL, &new_uuid)
            .await
            .map(Some),
        MagazineEvictionEntry::WriteOpenCallerGone => {
            crate::mount::test_open_write_for_caller(&api, new_uuid, POOL, caller_gone)
                .await
                .map(|_| None)
        }
    };
    let seen = seen.lock().unwrap().clone();
    (outcome, world, seen, api, temp)
}

fn changer_wait_config(extra: &str) -> remanence_state::RemConfig {
    remanence_state::parse_config_toml(&format!(
        r#"
[daemon]
state_dir = "/nonexistent/rem"
default_idle_timeout_seconds = 1800
{extra}

[journal]
dir = "/nonexistent/rem/journals"
require_trusted_volume = false

[audit]
dir = "/nonexistent/rem/audit"

[index]
sqlite_path = "/nonexistent/rem/index/rem-state.sqlite"

[cache]
tape_catalog_dir = "/nonexistent/rem/cache/tapes"
"#
    ))
    .expect("parse config")
}

/// `[daemon] changer_not_ready_wait_seconds` becomes the actors' wait, which
/// daemon shutdown stops; robotics and reconcile handles add the operation's
/// own cancellation through `configure_operation_library`.
#[test]
fn daemon_config_sets_the_changer_wait_and_operation_handles_inherit_it() {
    let board = crate::live_status::ChangerWaitBoard::default();
    for (extra, timeout, enabled) in [
        ("", 600, true),
        ("changer_not_ready_wait_seconds = 120", 120, true),
        ("changer_not_ready_wait_seconds = 0", 0, false),
    ] {
        let shutdown = Arc::new(AtomicBool::new(false));
        let wait = crate::api_state::changer_ready_wait_from_config(
            &changer_wait_config(extra),
            &board,
            Arc::clone(&shutdown),
        );
        assert_eq!(wait.timeout, StdDuration::from_secs(timeout), "{extra}");
        assert_eq!(wait.is_enabled(), enabled, "{extra}");
        assert_eq!(wait.stop_reason(), None);
        shutdown.store(true, Ordering::SeqCst);
        assert_eq!(wait.stop_reason(), Some("daemon shutdown"));
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "LIB-WAIT-CFG",
        0x0100,
        "DRV-WAIT-CFG",
        0x0400,
        1,
    )));
    let mut library = open_model_library(world);
    let temp = tempfile::tempdir().unwrap();
    let mut cfg = test_write_owner_config(
        temp.path().join("rem-state.sqlite"),
        temp.path().join("audit"),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.changer_ready_wait = crate::api_state::changer_ready_wait_from_config(
        &changer_wait_config("changer_not_ready_wait_seconds = 120"),
        &board,
        Arc::clone(&shutdown),
    );
    let registry = crate::operations::OperationRegistry::default();
    let operation_id = Uuid::from_u128(0x77);
    let handle = registry.register(operation_id, "move_medium");
    cfg.configure_operation_library(&mut library, &handle);
    let wait = library.changer().ready_wait();
    assert_eq!(wait.timeout, StdDuration::from_secs(120));
    assert_eq!(wait.stop_reason(), None);
    registry.request_cancel(&operation_id).unwrap();
    assert_eq!(wait.stop_reason(), Some("operation cancelled"));
    shutdown.store(true, Ordering::SeqCst);
    assert!(wait.stop_reason().is_some());
}

/// A queued changer move stops waiting once its caller is gone, and does not
/// wait at all during daemon shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changer_actor_move_wait_stops_for_a_gone_caller_and_skips_during_shutdown() {
    for shutting_down in [false, true] {
        let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
            "LIB-WAIT-STOP",
            0x0100,
            "DRV-WAIT-STOP",
            0x0400,
            1,
        )));
        world
            .lock()
            .unwrap()
            .put_tape_in_slot(0x0400, "STOP01L9", VirtualTape::default());
        let library = open_model_library(world.clone());
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = test_write_owner_config(
            temp.path().join("rem-state.sqlite"),
            temp.path().join("audit"),
            &library,
            library_snapshot_cell(library.library().clone()),
        );
        std::fs::create_dir_all(&cfg.audit_dir).unwrap();
        CatalogIndex::open(&cfg.index_path).unwrap();
        let shutdown = Arc::new(AtomicBool::new(shutting_down));
        cfg.changer_ready_wait =
            remanence_library::ChangerReadyWait::bounded(StdDuration::from_secs(60))
                .with_poll_interval(remanence_library::MIN_CHANGER_NOT_READY_POLL)
                .with_stop_signal("daemon shutdown", Arc::clone(&shutdown));
        {
            let mut world = world.lock().unwrap();
            world.remove_magazine([]);
            world
                .removed_magazine
                .as_mut()
                .unwrap()
                .not_ready_while_removed = true;
        }
        let actor = spawn_changer_actor(library.into_changer(), cfg);
        let caller_gone = Arc::new(AtomicBool::new(false));
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        actor
            .send(ChangerCommand::Move {
                src: 0x0400,
                dst: 0x0100,
                recovery_guard: None,
                caller_gone: Some(Arc::clone(&caller_gone)),
                reply: reply_tx,
            })
            .await
            .unwrap();
        let started = std::time::Instant::now();
        if !shutting_down {
            // Polls run at least 100 ms apart; let a few happen first.
            tokio::time::sleep(StdDuration::from_millis(350)).await;
            caller_gone.store(true, Ordering::SeqCst);
        }
        let result = tokio::time::timeout(StdDuration::from_secs(10), reply_rx)
            .await
            .expect("the wait stops promptly")
            .unwrap();
        let error = result.expect_err("a stopped wait fails the move as before");
        assert!(error.message().contains("3b, 12"), "{error}");
        assert!(started.elapsed() < StdDuration::from_secs(10));
        let polls = world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .filter(|command| {
                command.role == remanence_chaos::model::DeviceRole::Changer && command.opcode == 0
            })
            .count();
        if shutting_down {
            assert_eq!(polls, 0, "no readiness poll during shutdown");
        } else {
            assert!(polls > 0, "the wait polled before the caller left");
        }
        assert_eq!(
            world.lock().unwrap().slots[0].barcode.as_deref(),
            Some("STOP01L9")
        );
    }
}

/// A write open whose client goes away while the eviction's move home waits
/// for the magazine: that move still completes, so the evicted cartridge is
/// not left unthreaded in the drive. Whatever the open does next, the old
/// cartridge is home.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_open_caller_gone_during_eviction_still_moves_the_old_cartridge_home() {
    let (outcome, world, seen, api, _temp) =
        magazine_eviction_scenario(60, MagazineEvictionEntry::WriteOpenCallerGone).await;

    assert!(
        seen.iter()
            .any(|wait| wait.contains("move 0x0100 -> 0x0400")),
        "the move home waited: {seen:?}"
    );
    {
        let world = world.lock().unwrap();
        assert!(world.removed_magazine.is_none());
        assert_eq!(
            world.slots[0].barcode.as_deref(),
            Some("OLD001L9"),
            "open outcome {outcome:?}"
        );
        assert_ne!(world.loaded_barcode(0x0100), Some("OLD001L9"));
    }
    let _ = api.shutdown_drive_pool().await;
}

/// The write's eviction rides out the open magazine: the unloaded cartridge
/// goes home once the magazine is back, and the target tape loads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_eviction_rides_out_a_magazine_opened_before_the_move_home() {
    let (outcome, world, seen, api, _temp) =
        write_mount_with_magazine_opened_during_eviction(60).await;

    assert_eq!(
        outcome.expect("write mount succeeds"),
        (0x0100, Some(0x0401))
    );
    {
        let world = world.lock().unwrap();
        assert!(world.removed_magazine.is_none());
        assert_eq!(world.slots[0].barcode.as_deref(), Some("OLD001L9"));
        assert_eq!(world.loaded_barcode(0x0100), Some("NEW001L9"));
    }
    assert!(
        seen.iter().any(|wait| wait
            .starts_with("waiting for library: element 0x0400 medium magazine removed (3b/12)")
            || wait.starts_with("waiting for library: medium magazine removed (02/3b/12)")),
        "{seen:?}"
    );
    assert_eq!(
        api.changer_waits.get("LIB-MAGAZINE"),
        None,
        "the wait clears once ready"
    );
    api.shutdown_drive_pool().await.unwrap();
}

/// With the wait disabled the same window fails the write as before, naming
/// the cartridge and asking for refreshed inventory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_eviction_without_the_wait_fails_as_before_on_an_open_magazine() {
    let (outcome, world, seen, api, _temp) =
        write_mount_with_magazine_opened_during_eviction(0).await;

    let error = outcome.expect_err("the move home fails at once");
    for text in [
        "OLD001L9",
        "location requires refreshed inventory",
        "0x0400",
        "3b, 12",
    ] {
        assert!(error.message().contains(text), "missing {text}: {error}");
    }
    assert!(seen.is_empty());
    {
        let mut world = world.lock().unwrap();
        assert_eq!(world.loaded_barcode(0x0100), Some("OLD001L9"));
        // The operator closes the magazine; shutdown can then park the cartridge.
        world.reinsert_magazine();
        world.changer_unit_attention = None;
    }
    api.shutdown_drive_pool().await.unwrap();
}

/// Remove an adopted cartridge through the idle actor path before journal import.
pub(crate) async fn unload_adopted_cartridge(
    state: &StateHandle,
    world: Arc<Mutex<VirtualWorld>>,
    tape_uuid: TapeUuid,
) {
    {
        let mut world = world.lock().unwrap();
        world.slots.push(remanence_chaos::model::SlotState {
            address: 0x400,
            full: false,
            barcode: None,
            access: true,
            except: false,
        });
        world.element_layout.slot_count = 1;
        world.drive_source_slots.insert(0x100, Some(0x400));
    }
    let mut library = open_model_library(world.clone());
    let mut cfg = test_write_owner_config(
        state.paths().sqlite_path.clone(),
        state.paths().audit_dir.clone(),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.checkpoint_journal_dir = state.paths().journal_dir.join("tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    cfg.tape_pools = state.config().tape_pools.clone();
    let lifecycle = crate::drive_pool::DrivePoolLifecycle::default();
    cfg.lifecycle = Some(lifecycle.clone());
    let key = crate::drive_pool::DriveKey::new(&cfg.library().serial, 0x100);
    let drive = library.open_drive(0x100, &cfg.policy).unwrap();
    let actor = spawn_drive_actor(0x100, drive, cfg.clone());
    let changer = spawn_changer_actor(library.into_changer(), cfg.clone());
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(cfg.library().serial.clone(), changer)]),
        HashMap::from([(key.clone(), actor)]),
        Arc::new(HashMap::from([(key, AtomicBool::new(false))])),
        lifecycle,
    );
    let seated = SeatedCartridge {
        bay: 0x100,
        library_serial: cfg.library().serial.clone(),
        barcode: Some("HAND".into()),
        home_slot: 0x400,
        tape_uuid: Some(tape_uuid),
        prior_session_id: None,
    };
    pool.park_cartridge(seated.clone());
    let mut api = crate::ApiState::new(CatalogIndex::open(&state.paths().sqlite_path).unwrap());
    api.audit_dir = Arc::new(cfg.audit_dir.clone());
    api.library_snapshot = Some(cfg.library().snapshot.clone());
    api.drive_pool = Some(pool);
    api.drive_idle_unload_seconds = 1;
    crate::mount::test_remove_seated_epoch(&api, "idle", seated).await;
    assert!(world.lock().unwrap().drive_bays[&0x100].is_none());
    assert!(!TapeAuthority::exists(&cfg.checkpoint_journal_dir, tape_uuid).unwrap());
    assert!(
        !TapeAuthority::exists(&cfg.checkpoint_journal_dir, tape_uuid).unwrap(),
        "idle removal creates no tape log"
    );
    api.shutdown_drive_pool().await.unwrap();
}

/// Admission fences carry a reason, phase and only a nonempty known epoch range.
#[test]
fn admission_fence_evidence_omits_closed_and_unknown_ranges() {
    for range in [None, Some((3, 3)), Some((3, 5))] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.sqlite");
        let mut index = CatalogIndex::open(&path).unwrap();
        let (_, library) = unthreaded_cartridge_world("LIB-ADMISSION");
        let cfg = test_write_owner_config(
            path,
            temp.path().join("audit"),
            &library,
            library_snapshot_cell(library.library().clone()),
        );
        let uuid = [0x42; 16];
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: uuid,
                voltag: "ADMISSION".into(),
                block_size: 262144,
                parity: remanence_parity::ParityConfig::None,
                force: false,
            })
            .unwrap();
        assert!(super::checkpoint::fence_admission_contradiction(
            &mut index,
            &cfg,
            uuid,
            262144,
            range,
            &crate::append_admission::AdmissionError::Io(TapeIoError::FilemarkEncountered),
        ));
        let fences = index.list_active_tape_io_fences().unwrap();
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].reason, "admission_contradiction");
        crate::tests::assert_fences_survive_empty_rebuild(&index, &cfg.audit_dir);
        let evidence: serde_json::Value =
            serde_json::from_str(fences[0].evidence_json.as_deref().unwrap()).unwrap();
        assert_eq!(evidence["phase"], "append_admission");
        assert_eq!(
            evidence.get("open_epoch_start").is_some(),
            range == Some((3, 5))
        );
        assert_eq!(
            evidence.get("open_epoch_end").is_some(),
            range == Some((3, 5))
        );
    }
}

/// Run removal through real drive and changer actors after changing close admission.
#[allow(clippy::too_many_arguments)]
async fn check_epoch_removal(
    state: &StateHandle,
    world: Arc<Mutex<VirtualWorld>>,
    selected: SelectedTape,
    cfg: WriteOwnerConfig,
    lifecycle: crate::drive_pool::DrivePoolLifecycle,
    tx: mpsc::Sender<DriveCommand>,
    library: remanence_library::LibraryHandle,
    case: &str,
    path: &str,
    read_fault: Arc<AtomicU64>,
) {
    use pb::library_service_server::LibraryService;
    let mut index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
    let before = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
    assert!(
        matches!(case, "stale_open" | "stale_shared_lease" | "no_scheme")
            || before.highest_protected_ordinal < before.total_committed_ordinals
    );
    let journal_before = if case == "replay_closed" {
        Some(
            remanence_state::authority::fixtures::AuthorityBuilder::new(
                &cfg.checkpoint_journal_dir,
                selected.tape_uuid,
            )
            .build()
            .unwrap()
            .replay()
            .unwrap(),
        )
    } else {
        None
    };
    let mut lease = None;
    let mut preflight = None;
    let mut shared_lease = None;
    match case {
        "retired" | "sealed" | "finalizing" | "ingested" | "foreign" => {
            let state_name = match case {
                "ingested" => "ingestion_pending",
                "foreign" => "inventoried",
                other => other,
            };
            rusqlite::Connection::open(&state.paths().sqlite_path)
                .unwrap()
                .execute(
                    "update tapes set state=?1 where tape_uuid=?2",
                    rusqlite::params![state_name, selected.tape_uuid.as_slice()],
                )
                .unwrap();
        }
        "stale_shared_lease" => {
            let journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
                &cfg.checkpoint_journal_dir,
                selected.tape_uuid,
            )
            .build()
            .unwrap();
            let records = journal.replay().unwrap();
            let last = records.last().unwrap();
            let bundle = last.object_tape_file_bundles.last().unwrap();
            assert!(bundle.highest_protected_ordinal < bundle.total_committed_ordinals);
            assert_eq!(
                before.highest_protected_ordinal,
                before.total_committed_ordinals
            );
            shared_lease =
                Some(remanence_state::authority::fixtures::hold_shared(&journal).unwrap());
        }
        "lease" => {
            lease = Some(
                remanence_state::authority::fixtures::AuthorityBuilder::new(
                    &cfg.checkpoint_journal_dir,
                    selected.tape_uuid,
                )
                .build()
                .unwrap()
                .acquire_exclusive()
                .unwrap(),
            );
        }
        "mode_sense" => {
            read_fault.store(4, Ordering::SeqCst);
        }
        "catalog_read" => {
            let connection = rusqlite::Connection::open(&state.paths().sqlite_path).unwrap();
            connection
                .pragma_update(None, "ignore_check_constraints", true)
                .unwrap();
            connection
                .execute(
                    "update tapes set total_committed_ordinals = X'01' where tape_uuid = ?1",
                    [selected.tape_uuid.as_slice()],
                )
                .unwrap();
        }
        "summary_mismatch" => {
            cfg.io_memory.epoch_test_action.store(8, Ordering::Relaxed);
        }
        "projection_busy" => {
            cfg.io_memory.epoch_test_action.store(10, Ordering::Relaxed);
        }
        "post_projection" => {
            cfg.io_memory.epoch_test_action.store(9, Ordering::Relaxed);
        }
        "missing_checkpoint" => {
            std::fs::remove_dir_all(&cfg.checkpoint_journal_dir).unwrap();
        }
        "tail" | "open_tail_catalog_read" => {
            if case == "open_tail_catalog_read" {
                read_fault.store(30, Ordering::SeqCst);
            }
            let mut w = world.lock().unwrap();
            let tape = w.tapes.get_mut("HAND").unwrap();
            let end = 2
                + tape.records[2..]
                    .iter()
                    .position(|r| matches!(r, Record::Filemark))
                    .unwrap()
                + 1;
            let object = tape.records[2..end].to_vec();
            tape.records.extend(object);
        }
        "absent_bootstrap" => {
            world.lock().unwrap().tapes.get_mut("HAND").unwrap().records[0] =
                Record::Block(vec![0x55; selected.block_size as usize]);
        }
        "success" | "replay_closed" | "stale_open" => {}
        "replay_failure" => {
            rusqlite::Connection::open(&state.paths().sqlite_path).unwrap()
                .execute_batch("DELETE FROM tape_log_projection; UPDATE tapes SET written_extent_lba = NULL; CREATE TRIGGER fail_checkpoint_projection BEFORE UPDATE ON tapes BEGIN SELECT RAISE(FAIL, 'checkpoint projection unavailable'); END;")
                .unwrap();
        }
        "suffix_medium"
        | "suffix_scan_medium"
        | "open_admission_blank"
        | "open_admission_medium"
        | "open_admission_unit_attention"
        | "open_admission_not_ready"
        | "open_admission_timeout" => {
            if case == "suffix_scan_medium" {
                let mut w = world.lock().unwrap();
                let tape = w.tapes.get_mut("HAND").unwrap();
                let end = 3 + tape.records[2..]
                    .iter()
                    .position(|r| matches!(r, Record::Filemark))
                    .unwrap();
                let object = tape.records[2..end].to_vec();
                tape.records.extend(object);
            }
            read_fault.store(
                match case {
                    "suffix_medium" => 25,
                    "suffix_scan_medium" => 28,
                    "open_admission_blank" => 26,
                    "open_admission_unit_attention" => 23,
                    "open_admission_not_ready" => 24,
                    "open_admission_timeout" => 27,
                    _ => 21,
                },
                Ordering::SeqCst,
            );
        }
        "bot_filemark" => {
            world.lock().unwrap().tapes.get_mut("HAND").unwrap().records[0] = Record::Filemark;
        }
        "bot_blank" => {
            world
                .lock()
                .unwrap()
                .tapes
                .get_mut("HAND")
                .unwrap()
                .records
                .clear();
        }
        "bot_medium" | "bot_oversized" | "bot_transport" => {
            read_fault.store(
                match case {
                    "bot_medium" => 17,
                    "bot_oversized" => 18,
                    _ => 19,
                },
                Ordering::SeqCst,
            );
        }
        "admission_filemark"
        | "admission_medium"
        | "admission_transport"
        | "admission_unit_attention"
        | "admission_not_ready" => {
            read_fault.store(
                match case {
                    "admission_filemark" => 20,
                    "admission_medium" => 21,
                    "admission_transport" => 22,
                    "admission_unit_attention" => 23,
                    _ => 24,
                },
                Ordering::SeqCst,
            );
        }
        "blank_locate" => {
            cfg.io_memory.epoch_test_action.store(4, Ordering::Relaxed);
            let mut w = world.lock().unwrap();
            let tape = w.tapes.get_mut("HAND").unwrap();
            let end = tape.records.len() - 1;
            assert!(end > 2);
            tape.records[2] = Record::Block(vec![0x55; selected.block_size as usize]);
            tape.records.truncate(end);
            read_fault.store(9, Ordering::SeqCst);
        }
        "identity" => {
            if path == "shutdown" {
                cfg.io_memory.epoch_test_action.store(5, Ordering::Relaxed);
            }
            let bootstrap = crate::build_tape_bootstrap(
                [0x44; 16],
                selected.block_size,
                selected.parity_config.clone(),
                "2026-10-28T00:00:00Z",
                "test",
            );
            let mut bytes = vec![0; selected.block_size as usize];
            write_bootstrap_block(&bootstrap, &mut bytes).unwrap();
            world.lock().unwrap().tapes.get_mut("HAND").unwrap().records[0] = Record::Block(bytes);
        }
        "unit_attention"
        | "timeout"
        | "not_ready"
        | "position_read"
        | "transport"
        | "open_unit_attention"
        | "open_timeout"
        | "filemark"
        | "eod"
        | "hardware_44"
        | "hardware_15"
        | "wrong_length"
        | "read"
        | "medium"
        | "open_read"
        | "open_medium"
        | "late_barcode_fence" => {
            cfg.io_memory.epoch_test_action.store(4, Ordering::Relaxed);
            read_fault.store(
                match case {
                    "read" | "open_read" => 1,
                    "medium" | "open_medium" => 2,
                    "unit_attention" | "open_unit_attention" => 5,
                    "timeout" | "open_timeout" => 6,
                    "filemark" => 7,
                    "eod" => 8,
                    "hardware_44" => 15,
                    "hardware_15" => 16,
                    "not_ready" => 10,
                    "transport" => 11,
                    "position_read" => 12,
                    "wrong_length" => 13,
                    _ => 3,
                },
                Ordering::SeqCst,
            );
        }
        "barcode_fence" => {
            index
                .record_tape_io_fence(TapeIoFenceInput {
                    tape_uuid: [0x55; 16],
                    barcode: Some("HAND".into()),
                    reason: "transfer_error".into(),
                    evidence_json: None,
                })
                .unwrap();
        }
        "write_protected" => {
            world
                .lock()
                .unwrap()
                .tapes
                .get_mut("HAND")
                .unwrap()
                .write_protected = true;
        }
        "pending" | "finalize_lease" => {
            let mut request = ManualFinalizeTapeActorRequest {
                candidate_operation_id: Uuid::new_v4(),
                actor: AuditActor::System,
                actor_fingerprint: "epoch-test".into(),
                idempotency_key: Uuid::new_v4(),
                request_fingerprint: [0x51; 32],
                tape_uuid: selected.tape_uuid,
                expected_pool_id: Some(selected.pool_id.clone()),
                assignment_generation: before.assignment_generation,
                reason: "close committed epoch".into(),
                block_size: selected.block_size,
                parity_config: selected.parity_config.clone(),
                pool_config: Some(state.config().tape_pools[0].clone()),
            };
            let (reply, received) = oneshot::channel();
            tx.send(DriveCommand::PlanFinalization {
                ownership: crate::mount::PlanningOwnership::model(),
                request: request.clone(),
                needs_drive_load: false,
                barcode: Some("HAND".into()),
                source_slot: None,
                drive_serial: None,
                reply,
            })
            .await
            .unwrap();
            let prefix = received.await.unwrap().into_result().unwrap();
            if case == "finalize_lease" {
                let (ready, received) = oneshot::channel();
                let (resume, resumed) = std::sync::mpsc::channel();
                let cfg = cfg.clone();
                let path = state.paths().sqlite_path.clone();
                let task = tokio::task::spawn_blocking(move || {
                    super::terminal_finalize::PREFLIGHT_LEASE_HOOK.with(|hook| {
                        *hook.borrow_mut() = Some(Box::new(move || {
                            ready.send(()).unwrap();
                            resumed.recv_timeout(StdDuration::from_secs(20)).unwrap();
                        }));
                    });
                    let mut index = CatalogIndex::open(path).unwrap();
                    super::terminal_finalize::preflight_manual_finalize_with_prefix(
                        &mut index,
                        ManualFinalizePreflightConfig {
                            mounted_endpoint: None,
                            checkpoint_journal_dir: &cfg.checkpoint_journal_dir,
                            audit_dir: &cfg.audit_dir,
                            audit_fsync: cfg.audit_fsync,
                            audit_append_lock: &cfg.audit_append_lock,
                        },
                        Some("HAND"),
                        &mut request,
                        Some(&prefix),
                    )
                });
                tokio::time::timeout(StdDuration::from_secs(10), received)
                    .await
                    .unwrap()
                    .unwrap();
                preflight = Some((resume, task));
            } else {
                super::terminal_finalize::preflight_manual_finalize_with_prefix(
                    &mut index,
                    ManualFinalizePreflightConfig {
                        mounted_endpoint: None,
                        checkpoint_journal_dir: &cfg.checkpoint_journal_dir,
                        audit_dir: &cfg.audit_dir,
                        audit_fsync: cfg.audit_fsync,
                        audit_append_lock: &cfg.audit_append_lock,
                    },
                    Some("HAND"),
                    &mut request,
                    Some(&prefix),
                )
                .unwrap();
            }
        }
        "no_pool" | "no_scheme" | "journal_corrupt" => {}
        _ => panic!("unknown removal case {case}"),
    }
    if matches!(
        case,
        "retired"
            | "sealed"
            | "finalizing"
            | "ingested"
            | "foreign"
            | "no_pool"
            | "journal_corrupt"
            | "barcode_fence"
    ) {
        let journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
            &cfg.checkpoint_journal_dir,
            selected.tape_uuid,
        )
        .build()
        .unwrap();
        remanence_state::authority::fixtures::damage_authority(journal).unwrap();
    }
    if matches!(case, "tail" | "identity" | "absent_bootstrap") {
        // Journal replay restores this deliberately stale projection before fencing.
        rusqlite::Connection::open(&state.paths().sqlite_path)
            .unwrap()
            .execute(
                "update tapes set total_committed_ordinals = ?2 where tape_uuid = ?1",
                rusqlite::params![
                    selected.tape_uuid.as_slice(),
                    (before.total_committed_ordinals - 1)
                        .to_be_bytes()
                        .as_slice()
                ],
            )
            .unwrap();
    }
    if case.starts_with("open_") {
        let transient = matches!(
            case,
            "open_unit_attention"
                | "open_timeout"
                | "open_admission_unit_attention"
                | "open_admission_not_ready"
                | "open_admission_timeout"
        );
        for attempt in 0..if transient || case == "open_tail_catalog_read" {
            1
        } else {
            2
        } {
            let commands = world.lock().unwrap().command_log.len();
            let (reply, received) = oneshot::channel();
            tx.send(DriveCommand::OpenWrite {
                actor: AuditActor::System,
                pool_cfg: state.config().tape_pools[0].clone(),
                selected: selected.clone(),
                target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                needs_drive_load: false,
                library_serial: cfg.library().serial.clone(),
                barcode: Some("HAND".into()),
                source_slot: Some(0x400),
                drive_uuid: None,
                drive_serial: None,
                reply,
            })
            .await
            .unwrap();
            let error = received.await.unwrap().unwrap_err();
            if case == "open_tail_catalog_read" {
                let connection = rusqlite::Connection::open(&state.paths().sqlite_path).unwrap();
                let recovery: String = connection
                    .query_row(
                        "SELECT state FROM tapes WHERE tape_uuid = ?1",
                        [selected.tape_uuid.as_slice()],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(recovery, "recovery_required", "{error}");
                assert!(error.message().contains("TailBeyondKnownState"), "{error}");
                assert_eq!(read_fault.load(Ordering::SeqCst), 0);
                connection.execute_batch("ALTER TABLE tapes RENAME COLUMN unavailable_total TO total_committed_ordinals").unwrap();
                break;
            }
            if case.starts_with("open_admission_") {
                assert_eq!(
                    read_fault.load(Ordering::SeqCst),
                    0,
                    "LOCATE fault is exercised"
                );
            }
            assert_eq!(
                index.list_active_tape_io_fences().unwrap().len(),
                usize::from(!transient),
                "{case}: {error}"
            );
            if transient && !case.starts_with("open_admission_") {
                assert!(
                    FileAuditLog::replay(&state.paths().audit_dir)
                        .unwrap()
                        .iter()
                        .any(|record| record.detail.get("session_kind")
                            == Some(&CborValue::Text("epoch_left_open".into()))),
                    "transient open failure audits the unclosed epoch"
                );
            }
            if attempt == 1 {
                assert!(
                    world.lock().unwrap().command_log[commands..]
                        .iter()
                        .all(|c| c.opcode != 0x08),
                    "later opens never reread the failing range"
                );
            }
        }
        if transient {
            // Keep the removal retry transient too, so it leaves the epoch open.
            read_fault.store(
                match case {
                    "open_unit_attention" => 5,
                    "open_admission_unit_attention" => 23,
                    "open_admission_not_ready" => 24,
                    "open_admission_timeout" => 27,
                    _ => 6,
                },
                Ordering::SeqCst,
            );
        }
    }
    if index
        .tape_io_is_fenced(
            Some(&selected.tape_uuid),
            Some("HAND"),
            Some(&remanence_state::DriveKey::new(
                &cfg.library().serial,
                0x100,
            )),
        )
        .unwrap()
    {
        let commands = world.lock().unwrap().command_log.len();
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::PrepareRemoval {
            reply,
            progress: None,
        })
        .await
        .unwrap();
        assert_eq!(
            received.await.unwrap().unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
        assert_eq!(
            world.lock().unwrap().command_log.len(),
            commands,
            "{case} through {path}: fenced epoch stays untouched"
        );
        assert!(
            index
                .get_tape(&selected.tape_uuid)
                .unwrap()
                .unwrap()
                .highest_protected_ordinal
                < before.total_committed_ordinals
        );
        return;
    }
    let mut api = crate::ApiState::new(index);
    api.audit_dir = Arc::new(cfg.audit_dir.clone());
    api.library_snapshot = Some(cfg.library().snapshot.clone());
    api.default_library_serial = Some(Arc::new(cfg.library().serial.clone()));
    api.drive_idle_unload_seconds = 1;
    let key = crate::drive_pool::DriveKey::new(&cfg.library().serial, 0x100);
    let changer = if path == "operator" {
        // Inject the model library at the robotics execution boundary; the API still
        // dispatches PrepareRemoval to the real drive actor before this command.
        let (sender, mut receiver) = mpsc::channel(4);
        let policy = cfg.policy.clone();
        let model_world = world.clone();
        let admission_path = cfg.index_path.clone();
        drop(library);
        tokio::task::spawn_blocking(move || {
            let mut library = open_model_library(model_world);
            let Some(ChangerCommand::Robotics {
                action: RoboticsAction::Unload { bay, destination },
                handle,
                ..
            }) = receiver.blocking_recv()
            else {
                panic!("operator unload command")
            };
            if let Err(error) = super::robotics::admit_fenced_elements(
                &CatalogIndex::open(&admission_path).unwrap(),
                library.library(),
                &[bay],
            ) {
                handle.publish_failed(error.message(), &[]);
                return;
            }
            library.unload(bay, destination, &policy).unwrap();
            handle.publish_state(pb::OperationState::Succeeded, &[]);
        });
        sender
    } else {
        spawn_changer_actor(library.into_changer(), cfg.clone())
    };
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(cfg.library().serial.clone(), changer)]),
        HashMap::from([(key.clone(), tx)]),
        Arc::new(HashMap::from([(key, AtomicBool::new(false))])),
        lifecycle,
    );
    let seated = SeatedCartridge {
        bay: 0x100,
        library_serial: cfg.library().serial.clone(),
        barcode: Some("HAND".into()),
        home_slot: 0x400,
        tape_uuid: Some(selected.tape_uuid),
        prior_session_id: None,
    };
    if pool.parked_at(&seated.drive_key()).is_none() {
        pool.park_cartridge(seated.clone());
    }
    api.drive_pool = Some(pool);
    let command_start = world.lock().unwrap().command_log.len();
    match path {
        "shutdown" => {
            if let Err(error) = api.shutdown_drive_pool().await {
                assert!(
                    api.index()
                        .unwrap()
                        .tape_io_is_fenced(Some(&selected.tape_uuid), Some("HAND"), None)
                        .unwrap(),
                    "{error}"
                );
            }
        }
        "operator" => {
            let operation = api
                .library_service()
                .unload_drive(tonic::Request::new(pb::UnloadDriveRequest {
                    library_uuid: crate::library::library_uuid(&cfg.library().serial).to_vec(),
                    drive_element_address: 0x100,
                    destination_slot_address: 0x400,
                    idempotency_key: None,
                }))
                .await
                .unwrap();
            tokio::time::timeout(StdDuration::from_secs(10), async {
                while world.lock().unwrap().drive_bays[&0x100].is_some() {
                    if api
                        .index()
                        .unwrap()
                        .tape_io_is_fenced(Some(&selected.tape_uuid), Some("HAND"), None)
                        .unwrap()
                    {
                        break;
                    }
                    tokio::time::sleep(StdDuration::from_millis(10)).await;
                }
            })
            .await
            .expect("operator removal completes");
            let id = Uuid::from_slice(&operation.into_inner().operation_id).unwrap();
            let mut events = api.operations.watch(&id).unwrap();
            let mut closes = false;
            use tokio_stream::StreamExt;
            while let Some(event) = tokio::time::timeout(StdDuration::from_secs(10), events.next())
                .await
                .unwrap()
            {
                let event = event.unwrap();
                closes |= event
                    .progress
                    .get("phase")
                    .is_some_and(|p| p == "epoch_close");
            }
            if case == "success" {
                assert!(closes, "real close publishes phase=epoch_close");
            }
            if matches!(case, "retired" | "foreign" | "barcode_fence") {
                assert!(!closes, "skipped close publishes no epoch_close phase");
            }
        }
        _ => crate::mount::test_remove_seated_epoch(&api, path, seated).await,
    }
    drop(lease);
    drop(shared_lease);
    if let Some((resume, task)) = preflight {
        resume.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
    if case == "projection_busy" {
        assert!(
            CatalogIndex::open(&state.paths().sqlite_path)
                .unwrap()
                .list_active_tape_io_fences()
                .unwrap()
                .is_empty(),
            "a durable journal commit with SQLite busy never fences"
        );
    }
    if matches!(case, "suffix_medium" | "suffix_scan_medium") {
        assert_eq!(
            read_fault.load(Ordering::SeqCst),
            0,
            "suffix read fault is exercised"
        );
    }
    if case == "missing_checkpoint" {
        assert!(
            !cfg.checkpoint_journal_dir.exists(),
            "removal never recreates an absent journal"
        );
    }
    let audit = FileAuditLog::replay(&state.paths().audit_dir).unwrap();
    if matches!(case, "suffix_medium" | "suffix_scan_medium") {
        assert!(
            audit.iter().any(|record| matches!(
                record.detail.get("abort_reason"), Some(CborValue::Text(reason))
                    if reason.contains("append suffix at LBA")
            )),
            "{case} through {path}: suffix audit preserves the read location: {:?}",
            audit
                .iter()
                .filter_map(|r| r.detail.get("abort_reason"))
                .collect::<Vec<_>>()
        );
    }
    if matches!(
        case,
        "replay_failure"
            | "journal_corrupt"
            | "lease"
            | "finalize_lease"
            | "stale_shared_lease"
            | "catalog_read"
    ) {
        let failures: Vec<_> = audit
            .iter()
            .filter(|r| {
                r.detail.get("session_kind")
                    == Some(&CborValue::Text("epoch_projection_failed".into()))
            })
            .collect();
        assert_eq!(
            failures.len(),
            1,
            "{case}: projection failure has its own audit"
        );
        if case == "stale_shared_lease" {
            assert!(
                matches!(failures[0].detail.get("abort_reason"), Some(CborValue::Text(reason)) if reason.contains("tape-log lease conflict"))
            );
            let tape = CatalogIndex::open(&state.paths().sqlite_path)
                .unwrap()
                .get_tape(&selected.tape_uuid)
                .unwrap()
                .unwrap();
            assert_eq!(
                tape.total_committed_ordinals,
                before.total_committed_ordinals
            );
            assert_eq!(
                tape.highest_protected_ordinal,
                before.highest_protected_ordinal
            );
        }
        for record in failures {
            assert!(!record.detail.contains_key("open_epoch_start"));
            assert!(!record.detail.contains_key("open_epoch_end"));
            assert!(
                matches!(record.detail.get("abort_reason"), Some(CborValue::Text(reason)) if reason.contains("unknown until replay"))
            );
        }
        assert!(audit
            .iter()
            .all(|r| r.detail.get("session_kind")
                != Some(&CborValue::Text("epoch_left_open".into()))));
    }
    if matches!(
        case,
        "retired"
            | "sealed"
            | "finalizing"
            | "ingested"
            | "foreign"
            | "no_pool"
            | "no_scheme"
            | "barcode_fence"
            | "missing_checkpoint"
    ) {
        assert!(
            audit
                .iter()
                .all(|record| record.event != AuditEvent::OperationFailed),
            "a skipped close records no OperationFailed audit"
        );
    }
    let open_audits: Vec<_> = audit
        .iter()
        .filter(|record| {
            record.detail.get("session_kind") == Some(&CborValue::Text("epoch_left_open".into()))
        })
        .collect();
    if matches!(
        case,
        "post_projection"
            | "projection_busy"
            | "success"
            | "replay_closed"
            | "retired"
            | "foreign"
            | "barcode_fence"
    ) {
        assert!(
            open_audits.is_empty(),
            "closed epochs leave no stale range audit"
        );
    } else if matches!(
        case,
        "mode_sense"
            | "summary_mismatch"
            | "unit_attention"
            | "timeout"
            | "not_ready"
            | "transport"
            | "position_read"
            | "open_unit_attention"
            | "open_timeout"
    ) {
        assert!(
            !open_audits.is_empty(),
            "{case}: epoch_left_open audit is present"
        );
        for record in open_audits {
            assert!(record.detail.contains_key("abort_reason"));
        }
    }
    let world = world.lock().unwrap();
    let fenced = api
        .index()
        .unwrap()
        .tape_io_is_fenced(Some(&selected.tape_uuid), Some("HAND"), None)
        .unwrap();
    if fenced {
        assert!(
            world.drive_bays[&0x100].is_some(),
            "{case} through {path}: a new fence keeps the cartridge seated"
        );
        assert!(
            world.command_log[command_start..]
                .iter()
                .all(|command| command.opcode != 0xa5),
            "no robot move after fencing"
        );
    } else {
        assert!(world.drive_bays[&0x100].is_none(), "{case} through {path}");
        assert_eq!(
            world
                .slots
                .iter()
                .find(|s| s.address == 0x400)
                .unwrap()
                .barcode
                .as_deref(),
            Some("HAND")
        );
    }
    assert!(
        matches!(
            case,
            "post_projection" | "projection_busy" | "success" | "stale_open"
        ) || world.command_log[command_start..]
            .iter()
            .all(|c| !matches!(c.opcode, 0x0a | 0x10)),
        "{case} through {path}: failed or skipped close writes no media"
    );
    if matches!(case, "open_read" | "open_medium") {
        assert!(
            world.command_log[command_start..]
                .iter()
                .all(|c| c.opcode != 0x08),
            "open compensation never rereads the failing range"
        );
    }
    let mut index = CatalogIndex::open(&state.paths().sqlite_path).unwrap();
    if case == "projection_busy" {
        crate::startup_checkpoint::replay_checkpoint_journal_projections_with_audit(
            &mut index,
            &cfg.checkpoint_journal_dir,
            &cfg.audit_dir,
            &cfg.audit_append_lock,
        )
        .unwrap();
        let records = remanence_state::authority::fixtures::AuthorityBuilder::new(
            &cfg.checkpoint_journal_dir,
            selected.tape_uuid,
        )
        .build()
        .unwrap()
        .replay()
        .unwrap();
        assert_eq!(
            records.last().unwrap().eod_lba as usize,
            world.tapes["HAND"].records.len(),
            "durable close writes no abandonment filemark after journal EOD"
        );
    }
    if case == "summary_mismatch" && path == "shutdown" {
        assert!(
            world.command_log[command_start..]
                .iter()
                .filter(|c| c.opcode == 0x08)
                .count()
                <= 1,
            "shutdown never rebuilds a mismatching retained summary"
        );
    }
    if matches!(
        case,
        "post_projection" | "projection_busy" | "success" | "replay_closed"
    ) {
        let tape = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
        assert_eq!(
            tape.highest_protected_ordinal,
            tape.total_committed_ordinals
        );
    }
    if case == "stale_open" {
        let tape = index.get_tape(&selected.tape_uuid).unwrap().unwrap();
        assert!(
            tape.total_committed_ordinals > before.total_committed_ordinals,
            "{path}: removal replays the open checkpoint"
        );
        if tape.highest_protected_ordinal < tape.total_committed_ordinals {
            assert_eq!(path, "shutdown");
            assert!(
                audit.iter().any(|r| r.detail.get("session_kind")
                    == Some(&CborValue::Text("epoch_left_open".into()))
                    && r.detail.get("open_epoch_start")
                        == Some(&CborValue::Integer(tape.highest_protected_ordinal.into()))
                    && r.detail.get("open_epoch_end")
                        == Some(&CborValue::Integer(tape.total_committed_ordinals.into()))),
                "shutdown audits the replayed open range"
            );
        } else {
            let records = remanence_state::authority::fixtures::AuthorityBuilder::new(
                &cfg.checkpoint_journal_dir,
                selected.tape_uuid,
            )
            .build()
            .unwrap()
            .replay()
            .unwrap();
            assert!(
                records.last().unwrap().objects.is_empty(),
                "removal commits a sidecar-only checkpoint"
            );
        }
    }
    if let Some(before) = journal_before {
        let after = remanence_state::authority::fixtures::AuthorityBuilder::new(
            &cfg.checkpoint_journal_dir,
            selected.tape_uuid,
        )
        .build()
        .unwrap()
        .replay()
        .unwrap();
        assert_eq!(
            after, before,
            "removal appends no checkpoint after the durable close"
        );
        assert_eq!(
            after.last().unwrap().eod_lba as usize,
            world.tapes["HAND"].records.len()
        );
    }
    let fences = index.list_active_tape_io_fences().unwrap();
    let expect_fence = matches!(
        case,
        "filemark"
            | "eod"
            | "blank_locate"
            | "admission_filemark"
            | "admission_medium"
            | "bot_filemark"
            | "bot_blank"
            | "bot_medium"
            | "bot_oversized"
            | "wrong_length"
            | "absent_bootstrap"
            | "tail"
            | "identity"
            | "read"
            | "medium"
            | "open_read"
            | "open_medium"
            | "open_admission_blank"
            | "open_admission_medium"
    ) && !(path == "shutdown"
        && matches!(case, "read" | "medium" | "identity" | "late_barcode_fence"));
    if expect_fence {
        assert_eq!(fences.len(), 1, "{case} through {path}");
        assert!(
            audit
                .iter()
                .any(|record| record.event == AuditEvent::TapeIoFenceRaised),
            "{case}: contradiction fence has durable audit evidence"
        );
        let evidence: serde_json::Value =
            serde_json::from_str(fences[0].evidence_json.as_deref().unwrap()).unwrap();
        if case == "tail" {
            assert_eq!(evidence["phase"], "append_suffix");
            assert!(evidence["suffix_start_lba"].as_u64().unwrap() > 2);
            assert!(evidence.get("open_epoch_start").is_none());
            assert!(evidence.get("open_epoch_end").is_none());
        } else {
            assert_eq!(
                evidence["open_epoch_start"],
                before.highest_protected_ordinal
            );
            assert_eq!(evidence["open_epoch_end"], before.total_committed_ordinals);
        }
        if matches!(case, "open_admission_blank" | "open_admission_medium") {
            assert_eq!(fences[0].reason, "admission_contradiction");
            crate::tests::assert_fences_survive_empty_rebuild(&index, &cfg.audit_dir);
            assert_eq!(evidence["phase"], "append_admission");
        }
        assert!(!evidence["error"].as_str().unwrap().is_empty());
    } else if case == "barcode_fence" || (case == "late_barcode_fence" && path != "shutdown") {
        assert_eq!(fences.len(), 1);
    } else {
        assert!(fences.is_empty(), "{case} through {path}: {fences:?}");
    }
}

/// Inject failures at BOT, the append boundary or the committed Object prefix.
struct EpochRemovalTransport {
    inner: ModelTransport,
    world: Arc<Mutex<VirtualWorld>>,
    fault: Arc<AtomicU64>,
    located: u64,
    index_path: PathBuf,
}
impl SgTransport for EpochRemovalTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        data: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        if matches!(cdb[0], 0x1a | 0x5a) && self.fault.load(Ordering::SeqCst) == 4 {
            self.fault.store(0, Ordering::SeqCst);
            return Err(remanence_library::ScsiError::Io(std::io::Error::other(
                "injected MODE SENSE failure",
            )));
        }
        if cdb[0] == 0x34 && self.located == 2 && self.fault.load(Ordering::SeqCst) == 12 {
            self.fault.store(0, Ordering::SeqCst);
            return Err(remanence_library::ScsiError::Io(std::io::Error::other(
                "injected READ POSITION failure",
            )));
        }
        if cdb[0] == 0x08 && self.located > 2 {
            let pending = self.fault.load(Ordering::SeqCst);
            if pending == 30 {
                self.fault.store(0, Ordering::SeqCst);
                rusqlite::Connection::open(&self.index_path).unwrap().execute_batch(
                    "ALTER TABLE tapes RENAME COLUMN total_committed_ordinals TO unavailable_total"
                ).unwrap();
            }
            if pending == 25 {
                self.fault.store(0, Ordering::SeqCst);
                return Err(remanence_library::ScsiError::CheckCondition {
                    sense: vec![0x70, 0, 3, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0x14, 3, 0, 0, 0, 0],
                    bytes_transferred: 0,
                });
            }
            if pending == 28 {
                self.fault.store(29, Ordering::SeqCst);
            }
            if pending == 29 {
                self.fault.store(25, Ordering::SeqCst);
            }
        }
        let pending = self.fault.load(Ordering::SeqCst);
        if cdb[0] == 0x08
            && ((self.located == 2 && pending != 9)
                || (self.located == 0 && matches!(pending, 17..=19)))
        {
            let fault = self.fault.swap(0, Ordering::SeqCst);
            if fault != 0 {
                if fault == 3 {
                    CatalogIndex::open(&self.index_path)
                        .unwrap()
                        .record_tape_io_fence(TapeIoFenceInput {
                            tape_uuid: [0x55; 16],
                            barcode: Some("HAND".into()),
                            reason: "transfer_error".into(),
                            evidence_json: None,
                        })
                        .unwrap();
                }
                if matches!(fault, 6 | 11 | 19) {
                    return Err(remanence_library::ScsiError::TransportError {
                        status: 0,
                        host_status: if fault == 6 { 3 } else { 7 },
                        driver_status: 0,
                        info: 1,
                        sense: vec![],
                    });
                }
                let mut sense = vec![0; 18];
                sense[0] = if fault == 18 { 0xf0 } else { 0x70 };
                if fault == 18 {
                    sense[3..7].copy_from_slice(&(-1i32).to_be_bytes());
                }
                sense[2] = match fault {
                    1 | 15 | 16 => 4,
                    5 => 6,
                    7 => 0x80,
                    8 => 8,
                    10 => 2,
                    13 | 18 => 0x20,
                    _ => 3,
                };
                sense[7] = 10;
                sense[12] = match fault {
                    15 => 0x44,
                    16 => 0x15,
                    _ => 0x11,
                };
                return Err(remanence_library::ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                });
            }
        }
        self.inner.execute_in(cdb, data)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        data: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_out(cdb, data)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        match cdb[0] {
            0x01 => self.located = 0,
            0x2b => self.located = u64::from(u32::from_be_bytes(cdb[3..7].try_into().unwrap())),
            0x92 => self.located = u64::from_be_bytes(cdb[4..12].try_into().unwrap()),
            _ => {}
        }
        let pending = self.fault.load(Ordering::SeqCst);
        if matches!(cdb[0], 0x2b | 0x92) && self.located > 2 && matches!(pending, 20..=24 | 26 | 27)
        {
            self.fault.store(0, Ordering::SeqCst);
            if matches!(pending, 22 | 27) {
                return Err(remanence_library::ScsiError::TransportError {
                    status: 0,
                    host_status: if pending == 27 { 3 } else { 7 },
                    driver_status: 0,
                    info: 1,
                    sense: vec![],
                });
            }
            let mut sense = vec![0; 18];
            sense[0] = 0x70;
            sense[2] = match pending {
                20 => 0x80,
                21 => 3,
                23 => 6,
                26 => 8,
                _ => 2,
            };
            sense[7] = 10;
            sense[12] = 0x11;
            return Err(remanence_library::ScsiError::CheckCondition {
                sense,
                bytes_transferred: 0,
            });
        }
        if matches!(cdb[0], 0x2b | 0x92)
            && self.fault.load(Ordering::SeqCst) == 9
            && self.located > self.world.lock().unwrap().tapes["HAND"].records.len() as u64
        {
            let mut sense = vec![0; 18];
            sense[0] = 0x70;
            sense[2] = 8;
            sense[7] = 10;
            return Err(remanence_library::ScsiError::CheckCondition {
                sense,
                bytes_transferred: 0,
            });
        }
        self.inner.execute_none(cdb)
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}
/// Public session admission and close retain an epoch until it fills across sessions.
#[allow(clippy::too_many_arguments)]
async fn check_api_epoch_fills(
    state: &StateHandle,
    world: Arc<Mutex<VirtualWorld>>,
    selected: SelectedTape,
    cfg: WriteOwnerConfig,
    lifecycle: crate::drive_pool::DrivePoolLifecycle,
    tx: mpsc::Sender<DriveCommand>,
    library: remanence_library::LibraryHandle,
) {
    use pb::write_session_service_server::WriteSessionService;
    let mut api = crate::ApiState::new(CatalogIndex::open(&state.paths().sqlite_path).unwrap());
    api.pool_configs = Arc::new(
        cfg.tape_pools
            .iter()
            .map(|p| (p.id.clone(), p.clone()))
            .collect(),
    );
    api.library_snapshot = Some(cfg.library().snapshot.clone());
    api.default_library_serial = Some(Arc::new(cfg.library().serial.clone()));
    api.checkpoint_journal_dir = Arc::new(cfg.checkpoint_journal_dir.clone());
    api.audit_dir = Arc::new(cfg.audit_dir.clone());
    api.audit_append_lock = cfg.audit_append_lock.clone();
    api.drive_idle_unload_seconds = 0;
    api.io_memory = cfg.io_memory.clone();
    let key = crate::drive_pool::DriveKey::new(&cfg.library().serial, 0x100);
    let changer = spawn_changer_actor(library.into_changer(), cfg.clone());
    api.drive_pool = Some(DrivePool::new_with_lifecycle(
        HashMap::from([(cfg.library().serial.clone(), changer)]),
        HashMap::from([(key.clone(), tx.clone())]),
        Arc::new(HashMap::from([(key, AtomicBool::new(false))])),
        lifecycle,
    ));
    let service = api.write_session_service();
    let mut protected = 0;
    let mut baseline_reads = None;
    for number in 0..7 {
        let start = world.lock().unwrap().command_log.len();
        let session = service
            .open_write_session(tonic::Request::new(pb::OpenWriteSessionRequest {
                target: Some(pb::open_write_session_request::Target::TapeTarget(
                    pb::TapeTarget {
                        tape_uuid: selected.tape_uuid.to_vec(),
                        mount_if_needed: true,
                        required_pool_id: selected.pool_id.clone(),
                        allow_unpooled: false,
                    },
                )),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        let reads = world.lock().unwrap().command_log[start..]
            .iter()
            .filter(|c| c.opcode == 0x08)
            .count();
        assert!(
            reads <= *baseline_reads.get_or_insert(reads),
            "session open rereads epoch: {reads}"
        );
        let id = Uuid::from_slice(&session.session_id).unwrap();
        let payload = vec![number as u8; selected.block_size as usize * 2 + 19];
        let spool = state.paths().state_dir.join(format!("api-epoch-{number}"));
        std::fs::write(&spool, &payload).unwrap();
        crate::mount::append_finish(
            &api,
            id,
            crate::mount::AppendFinishRequest {
                staging_reason: "configured_serial",
                mtime: None,
                content_sha256: Sha256::digest(&payload).into(),
                received_bytes: payload.len() as u64,
                spool_path: spool,
                archive_path: PathBuf::from("epoch.bin"),
                caller_object_id: format!("api-epoch-{number}"),
                expected_content_sha256: None,
                expected_object_id: None,
                input_kind: crate::WriteObjectInputKind::LogicalFile,
            },
        )
        .await
        .unwrap();
        service
            .close_write_session(tonic::Request::new(pb::CloseWriteSessionRequest {
                session_id: session.session_id,
                ..Default::default()
            }))
            .await
            .unwrap();
        let tape = api
            .index()
            .unwrap()
            .get_tape(&selected.tape_uuid)
            .unwrap()
            .unwrap();
        if number == 0 {
            assert_eq!(tape.highest_protected_ordinal, 0);
            assert!(tape.total_committed_ordinals < 16);
        }
        assert_eq!(
            tape.highest_protected_ordinal,
            (tape.total_committed_ordinals / 16) * 16
        );
        protected = tape.highest_protected_ordinal;
        assert!(world.lock().unwrap().drive_bays[&0x100].is_some());
    }
    assert!(protected >= 16, "an epoch fills across sessions");
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Unload { reply }).await.unwrap();
    received.await.unwrap().unwrap();
    assert_eq!(api.io_memory.granted(), 0);
}

/// Recovery-persistence failure cannot bypass the raw-write fence.
#[test]
fn terminal_prefix_fence_precedes_recovery_persistence_failure() {
    check_terminal_fence_precedes_recovery_failure(false);
}

/// Tail failures keep their fence when recovery marking itself fails.
#[test]
fn terminal_tail_fence_precedes_recovery_persistence_failure() {
    check_terminal_fence_precedes_recovery_failure(true);
}

/// A tail failure that dispatched nothing leaves the medium unchanged: it is
/// a plain error with recovery marking, not a tape fence.
#[test]
fn terminal_tail_failure_before_dispatch_is_not_fenced() {
    check_terminal_write_failure(true, false);
}

fn check_terminal_fence_precedes_recovery_failure(tail: bool) {
    check_terminal_write_failure(tail, true);
}

/// Exercise the production failure handlers without a recoverable terminal intent.
fn check_terminal_write_failure(tail: bool, reached_drive: bool) {
    let root = tempfile::tempdir().unwrap();
    let mut index = CatalogIndex::open(root.path().join("index.sqlite")).unwrap();
    let uuid = [0x68; 16];
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: uuid,
            voltag: "FENCE001".into(),
            block_size: 4096,
            parity: ParityConfig::None,
            force: false,
        })
        .unwrap();
    let cfg = super::terminal_types::TerminalWriteConfig {
        mounted_endpoint: None,
        checkpoint_journal_dir: {
            let p = root.path().join("tapes");
            std::fs::create_dir_all(&p).unwrap();
            p
        },
        audit_dir: root.path().join("audit"),
        audit_fsync: true,
        audit_append_lock: Arc::new(Mutex::new(())),
    };
    let mut lease = remanence_state::authority::fixtures::AuthorityBuilder::new(
        &cfg.checkpoint_journal_dir,
        uuid,
    )
    .build()
    .unwrap()
    .acquire_exclusive()
    .unwrap();
    let selected = SelectedTape {
        standalone_endpoint: None,
        tape_uuid: uuid,
        block_size: 4096,
        parity_config: ParityConfig::None,
        pool_id: "default".into(),
    };
    let spec = super::terminal_types::TerminalFinalizeSpec {
        actor: AuditActor::System,
        tape_uuid: uuid,
        block_size: 4096,
        pool_config: None,
        trigger: TerminalFinalizationTrigger::ReachedLowWatermark,
        operation_id: None,
        manual: None,
    };
    let status = if tail {
        super::terminal_finalize::terminal_tail_write_failure(
            &mut index,
            &cfg,
            &mut lease,
            &spec,
            &selected,
            None,
            reached_drive,
            &remanence_parity::TerminalTailWriteError::Media(ParityError::Invariant(
                "terminal write fails",
            )),
        )
    } else {
        super::terminal_finalize::terminal_prefix_write_failure(
            &mut index,
            &cfg,
            &mut lease,
            &spec,
            &selected,
            reached_drive,
            &ParityError::Invariant("terminal write fails"),
        )
    };
    assert!(
        status.message().contains("persist terminal recovery"),
        "{status}"
    );
    let fences = index.list_active_tape_io_fences().unwrap();
    if !reached_drive {
        assert!(fences.is_empty(), "{fences:?}");
        return;
    }
    assert_eq!(fences.len(), 1);
    assert!(fences[0]
        .evidence_json
        .as_deref()
        .unwrap()
        .contains("terminal write fails"));
}

/// Tape and endpoint scopes both stop actual changer dispatch admission, then release in place.
#[test]
fn robotics_fence_admission_reads_current_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let mut index = CatalogIndex::open(dir.path().join("index.sqlite")).unwrap();
    let mut library = test_changer_library("LIB");
    let bay = library.drive_bays[0].element_address;
    library.drive_bays[0].loaded_tape = Some("FENCED".into());
    for endpoint in [None, Some(remanence_state::DriveKey::new("LIB", bay))] {
        let fence = index
            .record_tape_io_fence_at_endpoint(
                remanence_state::TapeIoFenceInput {
                    tape_uuid: [0; 16],
                    barcode: if endpoint.is_none() {
                        Some("FENCED".into())
                    } else {
                        None
                    },
                    reason: "transfer_error".into(),
                    evidence_json: None,
                },
                endpoint.as_ref(),
            )
            .unwrap();
        assert_eq!(
            super::robotics::admit_fenced_elements(&index, &library, &[bay])
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition
        );
        remanence_state::fence::release(
            &mut index,
            &dir.path().join("audit"),
            &fence.quarantine_id,
            "verified",
            AuditActor::System,
        )
        .unwrap();
        super::robotics::admit_fenced_elements(&index, &library, &[bay]).unwrap();
    }
}

/// Separate tape leases and catalog writers allow simultaneous session checkpoints.
#[tokio::test]
async fn two_tape_sessions_checkpoint_concurrently() {
    const BLOCK: u32 = 256 * 1024;
    let temp = tempfile::tempdir().unwrap();
    let index_path = temp.path().join("state.sqlite");
    let mut index = CatalogIndex::open(&index_path).unwrap();
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).unwrap();
    let audit_lock = Arc::new(Mutex::new(()));
    let admissions = WriteAdmissionCoordinator::default();
    let mut actors = Vec::new();
    for n in [1u8, 2] {
        let pool_id = format!("copy-{n}");
        let tape_uuid = [n; 16];
        let barcode = format!("TP000{n}L9");
        let serial = format!("LIB{n}");
        let drive_serial = format!("DRV{n}");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: pool_id.clone(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .unwrap();
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: barcode.clone(),
                block_size: BLOCK,
                parity: ParityConfig::None,
                force: false,
            })
            .unwrap();
        index
            .project_tape_pool_membership(tape_uuid, &pool_id)
            .unwrap();
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: drive_serial.clone(),
                identity_source: "DvcidAndInquiry".into(),
                vendor: None,
                product: None,
                firmware_rev: None,
                managed: "rem".into(),
                library_serial: Some(serial.clone()),
                element_address: Some(0x0100),
                observed_at_utc: None,
            })
            .unwrap()
            .drive_uuid;
        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".into(),
            written_at: "2026-10-29T00:00:00Z".into(),
            sequence: 0,
            block_size_bytes: BLOCK,
            drive_compression: false,
        };
        let mut block = vec![0; BLOCK as usize];
        write_bootstrap_block(&bootstrap, &mut block).unwrap();
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK);
        tape.records = vec![Record::Block(block), Record::Filemark];
        tape.written_bytes = u64::from(BLOCK);
        let mut world = VirtualWorld::single_drive(&serial, 0x0100, &drive_serial, 0x0400, 1);
        world.put_tape_in_drive(0x0100, &barcode, Some(0x0400), tape);
        let mut library = open_model_library(Arc::new(Mutex::new(world)));
        let snapshot = library_snapshot_cell(library.library().clone());
        let mut cfg =
            test_write_owner_config(index_path.clone(), audit_dir.clone(), &library, snapshot);
        cfg.audit_append_lock = audit_lock.clone();
        cfg.write_admissions = admissions.clone();
        cfg.checkpoint_max_objects = 1;
        let drive = library.open_drive(0x0100, &cfg.policy).unwrap();
        let tx = spawn_drive_actor(0x0100, drive, cfg);
        let pool_cfg = TapePoolConfig {
            id: pool_id,
            scheme: None,
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK),
            min_object_size_bytes: 0,
        };
        let session = open_actor_test_write_session(
            &tx,
            &pool_cfg,
            tape_uuid,
            &serial,
            &barcode,
            &drive_uuid,
            &drive_serial,
        )
        .await;
        actors.push((tx, session));
    }
    let append = |i: usize| {
        append_actor_test_file_result(
            &actors[i].0,
            actors[i].1,
            temp.path().join(format!("source-{i}")),
            "payload",
            "shared-caller",
            b"payload",
        )
    };
    let (a, b) = tokio::join!(append(0), append(1));
    for result in [a, b] {
        assert_eq!(
            result
                .unwrap()
                .record
                .append_commit_info
                .unwrap()
                .durability,
            pb::AppendDurability::Checkpointed as i32
        );
    }
    for (tx, session_id) in actors {
        let (reply, response) = oneshot::channel();
        tx.send(DriveCommand::Close { session_id, reply })
            .await
            .unwrap();
        response.await.unwrap().unwrap();
    }
    assert!(index
        .get_native_object_by_pool_and_caller_object_id("copy-1", "shared-caller")
        .unwrap()
        .is_some());
    assert!(index
        .get_native_object_by_pool_and_caller_object_id("copy-2", "shared-caller")
        .unwrap()
        .is_some());
}

/// An out-of-band removal stops the real idle timer without an UNLOAD or MOVE.
#[tokio::test]
async fn idle_unload_stops_when_live_element_status_finds_empty_bay() {
    let temp = tempfile::tempdir().unwrap();
    let index_path = temp.path().join("state.sqlite");
    let index = CatalogIndex::open(&index_path).unwrap();
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).unwrap();
    let mut world = VirtualWorld::single_drive("LIB-IDLE", 0x100, "DRV-IDLE", 0x400, 1);
    world.put_tape_in_drive(0x100, "TAPE-IDLE", Some(0x400), VirtualTape::default());
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(world.clone());
    let mut cfg = test_write_owner_config(
        index_path,
        audit_dir,
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    let lifecycle = crate::drive_pool::DrivePoolLifecycle::default();
    cfg.lifecycle = Some(lifecycle.clone());
    let key = crate::drive_pool::DriveKey::new("LIB-IDLE", 0x100);
    let drive = library.open_drive(0x100, &cfg.policy).unwrap();
    let actor = spawn_drive_actor(0x100, drive, cfg.clone());
    let changer = spawn_changer_actor(library.into_changer(), cfg.clone());
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([("LIB-IDLE".into(), changer)]),
        HashMap::from([(key.clone(), actor)]),
        Arc::new(HashMap::from([(key, AtomicBool::new(false))])),
        lifecycle,
    );
    let seated = SeatedCartridge {
        bay: 0x100,
        library_serial: "LIB-IDLE".into(),
        barcode: Some("TAPE-IDLE".into()),
        home_slot: 0x400,
        tape_uuid: None,
        prior_session_id: None,
    };
    pool.park_cartridge(seated.clone());
    let mut api = crate::ApiState::new(index);
    api.library_snapshot = Some(cfg.library().snapshot.clone());
    api.drive_pool = Some(pool);
    api.drive_idle_unload_seconds = 1;
    {
        let mut world = world.lock().unwrap();
        world.drive_bays.insert(0x100, None);
        world.drive_source_slots.insert(0x100, None);
        world.command_log.clear();
    }
    crate::mount::test_remove_seated_epoch(&api, "idle", seated).await;
    assert!(!cfg.library().snapshot.read().unwrap().report.libraries[0].drive_bays[0].loaded);
    let commands = world.lock().unwrap().command_log.len();
    tokio::time::sleep(StdDuration::from_millis(1200)).await;
    let world = world.lock().unwrap();
    assert_eq!(
        world.command_log.len(),
        commands,
        "timer stops for the empty bay"
    );
    assert!(world
        .command_log
        .iter()
        .all(|c| !matches!(c.opcode, 0x1b | 0xa5)));
}

/// Exhausted SQLite waits retain WARN, operation, session and tape context.
#[test]
fn catalog_lock_refusal_logs_write_session_context() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut index = CatalogIndex::open(&path).unwrap();
    let holder = rusqlite::Connection::open(&path).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let log_path = temp.path().join("daemon.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || log.try_clone().unwrap())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let _span = tracing::warn_span!(
            "write_session",
            session_id = "test-session",
            tape_uuid = "test-tape"
        )
        .entered();
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: [91; 16],
                voltag: "LOCK01L9".into(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .unwrap_err();
    });
    let log = std::fs::read_to_string(log_path).unwrap();
    for expected in [
        "WARN",
        "catalog lock refusal",
        "operation=",
        "test-session",
        "test-tape",
    ] {
        assert!(log.contains(expected), "missing {expected}: {log}");
    }
}

/// A failed checkpoint batch retains its fence when the catalog disappears.
#[test]
fn checkpoint_batch_fence_survives_empty_catalog_rebuild() {
    let temp = tempfile::tempdir().unwrap();
    let index_path = temp.path().join("state.sqlite");
    let mut index = CatalogIndex::open(&index_path).unwrap();
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "LIB-CHECKPOINT-FENCE",
        0x0100,
        "DRV-CHECKPOINT-FENCE",
        0x0400,
        1,
    )));
    let mut library = open_model_library(world);
    let cfg = test_write_owner_config(
        index_path,
        temp.path().join("audit"),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    let selected = SelectedTape {
        standalone_endpoint: Some(remanence_library::DriveKey::Standalone {
            drive_serial: "CHECKPOINT-FENCE".into(),
        }),
        pool_id: "checkpoint-fence".into(),
        tape_uuid: [0x46; 16],
        block_size: 4096,
        parity_config: ParityConfig::None,
    };
    let policy = remanence_library::StaticAllowlist::new(["LIB-CHECKPOINT-FENCE"]);
    let mut drive = library.open_drive(0x0100, &policy).unwrap();
    let journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
        &cfg.checkpoint_journal_dir,
        selected.tape_uuid,
    )
    .build()
    .unwrap();
    let mut lease = journal.acquire_exclusive().unwrap();
    let batch = super::checkpoint::PendingCheckpointBatch::new(std::time::Duration::from_secs(1));
    super::checkpoint::fence_failed_checkpoint_batch(
        &mut drive,
        &mut lease,
        &mut index,
        &cfg,
        &selected,
        &batch,
        Status::unavailable("checkpoint barrier fails"),
    );
    assert_eq!(
        index.list_active_tape_io_fences().unwrap()[0].reason,
        "checkpoint_barrier_failed"
    );
    crate::tests::assert_fences_survive_empty_rebuild(&index, &cfg.audit_dir);
}

/// The mounted local failure envelope must not change daemon durable diagnostics.
#[test]
fn daemon_epoch_failure_audit_keeps_legacy_status_text() {
    use std::error::Error as _;
    let temp = tempfile::tempdir().unwrap();
    let mut index = CatalogIndex::open(temp.path().join("state.sqlite")).unwrap();
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "audit-library",
        0x100,
        "audit-drive",
        0x400,
        1,
    )));
    let library = open_model_library(world);
    let cfg = test_write_owner_config(
        temp.path().join("state.sqlite"),
        temp.path().join("audit"),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    let selected = crate::pool_write::SelectedTape {
        standalone_endpoint: None,
        tape_uuid: [0x42; 16],
        block_size: 262144,
        parity_config: ParityConfig::None,
        pool_id: "default".into(),
    };
    let status = status_from_selected_write_error(
        &selected,
        PoolWriteError::SelectedTapeInsufficientCapacity {
            object_size: 31,
            raw_capacity: 17,
            used: 9,
        },
    );
    assert!(
        status.source().is_none(),
        "daemon conversion has no local WriteCause at any sink"
    );
    let expected = "code: 'The system is not in a state required for the operation's execution', message: \"selected tape has insufficient capacity: object_size=31, raw_capacity=17, used=9\"";
    assert_eq!(status.to_string(), expected, "legacy log Display text");
    audit_removal_epoch(
        &mut index,
        &cfg,
        0x100,
        Some(selected.tape_uuid),
        "epoch_left_open",
        status.to_string(),
        Some((0, 1)),
    );
    let audit = FileAuditLog::replay(&cfg.audit_dir).unwrap();
    let record = audit
        .iter()
        .find(|r| r.detail.get("session_kind") == Some(&CborValue::Text("epoch_left_open".into())))
        .unwrap();
    assert_eq!(
        record.detail.get("abort_reason"),
        Some(&CborValue::Text(expected.into()))
    );
    let wrapped = super::ActorFailure::preserve(&status, Status::internal("secondary failure"));
    assert!(
        wrapped.source().is_none(),
        "fence wrapping cannot introduce a local cause on the daemon path"
    );
}

/// Exercise the actual actor's epoch planner, including media preparation and
/// replay, and require the base Absent-mode refusal before any terminal write.
async fn assert_epoch_planner_refusal(
    tx: &mpsc::Sender<DriveCommand>,
    request: &ManualFinalizeTapeActorRequest,
    barcode: &str,
    world: &Arc<Mutex<VirtualWorld>>,
) {
    let before = world.lock().unwrap().command_log.len();
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::PlanFinalization {
        ownership: crate::mount::PlanningOwnership::model(),
        request: request.clone(),
        needs_drive_load: false,
        barcode: Some(barcode.into()),
        source_slot: None,
        drive_serial: None,
        reply,
    })
    .await
    .unwrap();
    let status = received.await.unwrap().into_result().unwrap_err();
    assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status}");
    assert!(world.lock().unwrap().command_log[before..]
        .iter()
        .all(|c| !matches!(c.opcode, 0x0a | 0x10)));
}
