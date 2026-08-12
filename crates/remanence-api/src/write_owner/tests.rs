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
    ChangerHandle, DiscoveryReport, DriveHandle, DriveOpError, LoadError, MediaFamily,
    MediaReadiness, SpaceKind, TapeConfig, TapeIoError,
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
    const BLOCK_SIZE: u32 = 1024;

    let temp = tempfile::Builder::new()
        .prefix("remanence-empty-automatic-preflight")
        .tempdir()
        .expect("create automatic preflight tempdir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite"))
        .expect("open automatic preflight catalog");
    let selected = SelectedTape {
        pool_id: "fresh-open".to_string(),
        tape_uuid: TAPE_UUID,
        block_size: BLOCK_SIZE,
        parity_config: ParityConfig::None,
    };
    let pool_cfg = TapePoolConfig {
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
    let checkpoint_dir = temp.path().join("checkpoints");
    let audit_append_lock = Arc::new(std::sync::Mutex::new(()));

    assert!(
        !preflight_automatic_terminal_completion(
            &mut index,
            ManualFinalizePreflightConfig {
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
    let checkpoint = remanence_state::FileCheckpointJournal::open(checkpoint_dir, TAPE_UUID)
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
    let checkpoint_journal =
        remanence_state::FileCheckpointJournal::open(temp.path().join("checkpoints"), TAPE_UUID)
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
    let mut journal = FileTapeFileJournal::open(
        temp.path().join("tape.remjournal"),
        TAPE_UUID,
        BLOCK_SIZE,
        scheme.clone(),
    )
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
    journal.commit_bundle(&bot_bundle).expect("journal BOT");
    journal
        .commit_bundle(&object_bundle)
        .expect("journal Object");
    journal
        .commit_bundle(&object_checkpoint)
        .expect("journal Object checkpoint");
    journal
        .commit_terminal_prefix_transition(
            &CommittedBundle {
                kind: CommittedBundleKind::TerminalPrefix,
                entries: Vec::new(),
                highest_protected_ordinal: 0,
                total_committed_ordinals: 1,
            },
            &sink_checkpoint,
        )
        .expect("journal terminal prefix");
    let replica_a = remanence_parity::terminal_component_bundle(&plan, layout.components[0])
        .expect("replica A bundle");
    journal
        .commit_bundle(&replica_a)
        .expect("simulate crash after sink component fsync");

    let spec = TerminalFinalizeSpec {
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
        intent,
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
            .terminal_component_authority_relation(1, &transitions)
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
        .commit_terminal_component_transition(&transitions[1].0, &transitions[1].1)
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
        checkpoint_intent,
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
            .commit_bundle(&transitions[component_index].0)
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
        .commit_terminal_component_transition(&transitions[4].0, &transitions[4].1)
        .expect("journal replica C transition");
    let lowered_pool = TapePoolConfig {
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
        pool_id: lowered_pool.id.clone(),
        tape_uuid: TAPE_UUID,
        block_size: BLOCK_SIZE,
        parity_config: ParityConfig::Scheme(scheme),
    };
    let lowered_spec = TerminalFinalizeSpec {
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
        pool_id: "host-authority-test".to_string(),
        tape_uuid: TAPE_UUID,
        block_size: BLOCK_SIZE,
        parity_config: ParityConfig::None,
    };
    let pool_cfg = TapePoolConfig {
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
            checkpoint_journal_dir: &temp.path().join("checkpoints"),
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
    let checkpoint_journal =
        remanence_state::FileCheckpointJournal::open(temp.path().join("checkpoints"), TAPE_UUID)
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
    assert_eq!(projected.selected_replica_ordinal, 0);
    assert_eq!(projected.structural_entry_count, 0);
    assert_eq!(projected.object_row_count, 0);
    assert_eq!(projected.replica_health.len(), 3);
    assert!(projected.detail.contains("structural recovery from BOT"));
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
            },
        )),
    );

    assert_eq!(
        projected.state,
        pb::TapeIndexVerificationState::RecoveryRequired as i32
    );
    assert_eq!(projected.measured_eod_lba, 123);
    assert_eq!(projected.measured_tape_file_count, 7);
    assert_eq!(
        projected.recovery_inventory.as_ref().map(|row| row.outcome),
        Some(pb::TapeInventoryOutcome::BotStructuralRecovered as i32)
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
fn session_open_media_family_uses_lto9_barcode_suffix() {
    assert!(matches!(
        session_open_media_family(Some("AOX030L9")),
        MediaFamily::Lto9OrLater
    ));
    assert!(matches!(
        session_open_media_family(Some("AOX030LZ")),
        MediaFamily::Lto9OrLater
    ));
    assert!(matches!(
        session_open_media_family(Some("AOX030L8")),
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
    assert_eq!(active[0].drive_element, 1);
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

#[tokio::test]
async fn daemon_rejects_corrupt_checkpoint_authority_before_position_or_config_commands() {
    const BLOCK_SIZE: u32 = 1024;
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
    cfg.checkpoint_journal_dir = temp.path().join("checkpoints");

    let checkpoint =
        remanence_state::FileCheckpointJournal::open(&cfg.checkpoint_journal_dir, tape_uuid)
            .expect("open checkpoint handle");
    std::fs::write(checkpoint.path(), b"legacy-or-torn-checkpoint")
        .expect("write corrupt checkpoint authority");

    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
    let command_start = world.lock().expect("world lock").command_log.len();
    let pool_cfg = TapePoolConfig {
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
            pool_cfg: pool_cfg.clone(),
            selected: SelectedTape {
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
    assert!(status.message().contains("torn header"), "{status}");

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
async fn daemon_rejects_missing_checkpoint_authority_for_catalog_written_media_before_motion() {
    const BLOCK_SIZE: u32 = 1024;

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
                pool_cfg: TapePoolConfig {
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
            status
                .message()
                .contains("checkpoint journal is empty but catalog records a written tape prefix"),
            "{case}: {status}"
        );
        assert!(
            world.lock().expect("world lock").command_log[command_start..].is_empty(),
            "{case}: missing authority must reject before any drive command"
        );
    }
}

#[tokio::test]
async fn daemon_checkpoint_lease_contention_precedes_conditional_load_for_no_parity() {
    const BLOCK_SIZE: u32 = 1024;
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
    cfg.checkpoint_journal_dir = temp.path().join("checkpoints");
    let checkpoint =
        remanence_state::FileCheckpointJournal::open(&cfg.checkpoint_journal_dir, tape_uuid)
            .expect("open checkpoint handle");
    let _held_lease = checkpoint
        .acquire_exclusive()
        .expect("hold competing checkpoint lease");

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
            pool_cfg: TapePoolConfig {
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
    assert!(
        world.lock().expect("world lock").command_log[command_start..].is_empty(),
        "checkpoint contention must reject before conditional LOAD or any drive command"
    );
}

#[test]
fn fresh_parity_bootstrap_short_completion_fences_without_journal_visibility() {
    const BLOCK_SIZE: u32 = 4096;

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
    cfg.checkpoint_journal_dir = temp.path().join("journals/checkpoints");
    std::fs::create_dir_all(cfg.checkpoint_journal_dir.parent().expect("journal parent"))
        .expect("create journal parent");
    let tape_uuid = [0x46; 16];
    let scheme = remanence_parity::default_scheme_for_block_size(BLOCK_SIZE);
    let selected = SelectedTape {
        pool_id: "fresh-parity-fence-test".to_string(),
        tape_uuid,
        block_size: BLOCK_SIZE,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };

    let mut drive = library.open_drive(0x0100, &policy).expect("open drive");
    let status = {
        let checkpoint_journal =
            remanence_state::FileCheckpointJournal::open(&cfg.checkpoint_journal_dir, tape_uuid)
                .expect("open fresh checkpoint journal");
        let checkpoint_lease = checkpoint_journal
            .acquire_exclusive()
            .expect("acquire fresh checkpoint lease");
        let authority = validate_parity_actor_authority(&cfg, &selected, &checkpoint_lease, &[])
            .expect("validate fresh parity authority");
        match open_parity_actor_session(&mut index, &mut drive, &cfg, &selected, &[], authority) {
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

    let journal_path = parity_journal_path(&cfg, tape_uuid).expect("parity journal path");
    let journal = FileTapeFileJournal::open(journal_path, tape_uuid, BLOCK_SIZE, scheme)
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
    let path = temp.path().join("terminal.remjournal");
    let journal = FileTapeFileJournal::open(&path, tape_uuid, 256 * 1024, scheme.clone())
        .expect("open session journal");
    let mut session = ParityActorSession {
        scheme: scheme.clone(),
        sink_state: None,
        journal: Some(journal),
    };

    let contended = FileTapeFileJournal::open(&path, tape_uuid, 256 * 1024, scheme.clone())
        .expect_err("a second open must contend with the session-owned journal");
    assert!(contended.is_lock_contended(), "{contended}");

    let terminal_journal = session
        .journal
        .take()
        .expect("transfer the session journal to terminal finalization");
    assert!(session.journal.is_none());
    drop(terminal_journal);
    FileTapeFileJournal::open(&path, tape_uuid, 256 * 1024, scheme)
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
    let checkpoint_dir = temp.path().join("checkpoints");
    std::fs::create_dir_all(&audit_dir).expect("create audit dir");
    let mut cfg =
        test_write_owner_config(index_path.clone(), audit_dir.clone(), &library, snapshot);
    cfg.checkpoint_journal_dir = checkpoint_dir.clone();
    cfg.checkpoint_max_objects = 2;
    cfg.checkpoint_max_age_seconds = 3600;
    let serial = library.library().serial.clone();
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
    let pool_cfg = TapePoolConfig {
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
            pool_cfg: pool_cfg.clone(),
            selected: SelectedTape {
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
    assert_eq!(bootstrap_files.len(), 1);
    assert_eq!(bootstrap_files[0].tape_file_number, 0);
    assert_eq!(bootstrap_files[0].kind, "bootstrap");
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
        remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("open durable parity checkpoint journal");
    let records = checkpoint_journal
        .replay()
        .expect("replay durable partial-epoch checkpoint");
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert!(
        record.object_tape_file_bundles[0].total_committed_ordinals
            < u64::from(scheme.data_blocks_per_stripe),
        "the regression must close a genuinely partial parity epoch"
    );
    let barrier_bundle = record
        .barrier_bundle
        .as_ref()
        .expect("parity checkpoint sidecar bundle");
    assert_eq!(
        barrier_bundle
            .entries
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![TapeFileKind::ParitySidecar],
        "the barrier must journal exactly the short sidecar"
    );
    assert_eq!(
        barrier_bundle
            .entries
            .last()
            .expect("checkpoint sidecar")
            .tape_file_number
            .checked_add(1)
            .expect("next tape-file number"),
        record.next_tape_file_number,
    );
    assert_eq!(
        barrier_bundle.highest_protected_ordinal,
        barrier_bundle.total_committed_ordinals
    );
    let mut replay_index = CatalogIndex::open(&index_path).expect("open replay projection");
    replay_index
        .project_checkpoint_record(record)
        .expect("idempotently replay the durable partial-epoch checkpoint into SQLite");
    drop(replay_index);

    let (resume_tx, resume_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenWrite {
            pool_cfg,
            selected: SelectedTape {
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
            .get_native_object_by_caller_object_id("parity-actor-second")
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
            library_serial: "DEC418146K_LL02".to_string(),
            changer_sg: Some("/dev/sg8".to_string()),
            drive_element: 0x0100,
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

fn open_test_changer(library: &Library) -> ChangerHandle {
    let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
    let serial = library.serial.clone();
    let mut responses = Some(vec![changer_inquiry_response(), vpd80_response(&serial)]);
    library
        .open_with(&policy, move |_| {
            let responses = responses
                .take()
                .expect("test changer transport opened once");
            Ok::<_, remanence_library::IoErrorKind>(Box::new(
                FixtureTransport::new().with_responses(responses),
            )
                as Box<dyn remanence_library::SgTransport>)
        })
        .expect("open test changer")
        .into_changer()
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
    WriteOwnerConfig {
        index_path,
        report: DiscoveryReport {
            libraries: vec![library.library().clone()],
            warnings: Vec::new(),
        },
        policy: remanence_library::StaticAllowlist::new([serial.as_str()]),
        audit_dir,
        audit_fsync: false,
        audit_append_lock: Arc::new(std::sync::Mutex::new(())),
        reservations: Arc::new(HashMap::new()),
        actor_library_serial: serial.clone(),
        library_snapshot,
        snapshot_miss_alarm: 1,
        managed_library_serials: Arc::new(HashSet::from([serial])),
        cleaning: remanence_state::CleaningConfig::default(),
        tape_io: remanence_state::TapeIoConfig::default(),
        io_memory: crate::io_memory::IoMemoryReservation::new(
            remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES,
        )
        .expect("test I/O memory manager"),
        write_admissions: WriteAdmissionCoordinator::default(),
        checkpoint_journal_dir: std::env::temp_dir().join("rem-checkpoint-tests"),
        checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
        checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
        checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
        session_idle_seconds: 1800,
        lifecycle: None,
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
            pool_cfg: pool_cfg.clone(),
            selected: SelectedTape {
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

    arm_checkpoint_timer(tx, session_id, batch_id, StdDuration::from_millis(0))
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

    send_checkpoint_actor_reply(reply, pb::WriteSession::default(), &mut receipts);

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
    cfg.checkpoint_journal_dir = temp.path().join("checkpoints");
    cfg.checkpoint_max_objects = 2;
    cfg.checkpoint_max_age_seconds = 3600;
    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);

    let pool_cfg = TapePoolConfig {
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
            pool_cfg,
            selected: SelectedTape {
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
    let journal =
        remanence_state::FileCheckpointJournal::open(temp.path().join("checkpoints"), tape_uuid)
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
        cfg.checkpoint_journal_dir = temp.path().join("checkpoints");
        cfg.checkpoint_max_objects = 1;
        cfg.checkpoint_max_age_seconds = 3600;
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
        let pool_cfg = TapePoolConfig {
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

        let checkpoint = remanence_state::FileCheckpointJournal::open(
            temp.path().join("checkpoints"),
            tape_uuid,
        )
        .expect("open checkpoint journal after lease release");
        let read_only = CatalogIndex::open_read_only(&index_path).expect("open catalog");
        let tape = read_only
            .get_tape(&tape_uuid)
            .expect("query tape")
            .expect("tape exists");
        if reject_terminal {
            let error = checkpoint
                .replay()
                .expect_err("terminal failure retains finalization intent");
            assert!(
                error
                    .to_string()
                    .contains("pending terminal finalization intent"),
                "{error}"
            );
            assert_eq!(tape.state, "recovery_required");
            assert!(read_only
                .get_native_object_by_caller_object_id("terminal-caller")
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
                .get_native_object_by_caller_object_id("terminal-caller")
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
    let checkpoint_dir = temp.path().join("checkpoints");
    cfg.checkpoint_journal_dir = checkpoint_dir.clone();
    cfg.checkpoint_max_objects = 1;
    cfg.checkpoint_max_age_seconds = 3600;
    let audit_append_lock = Arc::clone(&cfg.audit_append_lock);
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
    let pool_cfg = TapePoolConfig {
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

    let checkpoint =
        remanence_state::FileCheckpointJournal::open(temp.path().join("checkpoints"), tape_uuid)
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
    let mut blocked_intent_path = checkpoint.path().as_os_str().to_os_string();
    blocked_intent_path.push(".finalizing.new");
    let blocked_intent_path = std::path::PathBuf::from(blocked_intent_path);
    std::fs::create_dir(&blocked_intent_path).expect("block intent temporary file creation");
    let commands_before_crash_cut = world.lock().expect("world lock").command_log.len();
    let mut crash_index = CatalogIndex::open(&index_path).expect("open crash-cut catalog");
    let crash_error = preflight_manual_finalize_tape(
        &mut crash_index,
        ManualFinalizePreflightConfig {
            checkpoint_journal_dir: &checkpoint_dir,
            audit_dir: &audit_dir,
            audit_fsync: false,
            audit_append_lock: &audit_append_lock,
        },
        Some(barcode),
        &mut request.clone(),
    )
    .expect_err("intent publication failure rolls back acceptance");
    assert_eq!(crash_error.code(), tonic::Code::Internal);
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
    std::fs::remove_dir(&blocked_intent_path).expect("unblock intent publication");

    let mut recovered_request = request.clone();
    let recovered_operation_id = Uuid::from_u128(0x7d05);
    recovered_request.candidate_operation_id = recovered_operation_id;
    let mut retry_index = CatalogIndex::open(&index_path).expect("open retry catalog");
    assert!(preflight_manual_finalize_tape(
        &mut retry_index,
        ManualFinalizePreflightConfig {
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
    cfg.checkpoint_journal_dir = temp.path().join("checkpoints");
    cfg.checkpoint_max_objects = 2;
    cfg.checkpoint_max_age_seconds = 3600;
    let library_serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([library_serial.as_str()]);
    let drive = library
        .open_drive(0x0100, &policy)
        .expect("open model drive");
    let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
    let pool_cfg = TapePoolConfig {
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
            &CommittedBundle {
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
            },
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

    let library = test_changer_library("LIB_MOVE_OBS_FAIL");
    let snapshot_cell = library_snapshot_cell(library.clone());
    let changer = open_test_changer(&library);
    let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
    let cfg = WriteOwnerConfig {
        index_path: index_path.clone(),
        report: DiscoveryReport {
            libraries: vec![library.clone()],
            warnings: Vec::new(),
        },
        policy,
        audit_dir: temp.path().join("audit"),
        audit_fsync: false,
        audit_append_lock: Arc::new(std::sync::Mutex::new(())),
        reservations: Arc::new(HashMap::new()),
        actor_library_serial: library.serial.clone(),
        library_snapshot: Arc::clone(&snapshot_cell),
        snapshot_miss_alarm: 1,
        managed_library_serials: Arc::new(HashSet::from([library.serial.clone()])),
        cleaning: remanence_state::CleaningConfig::default(),
        tape_io: remanence_state::TapeIoConfig::default(),
        io_memory: test_io_memory(),
        write_admissions: WriteAdmissionCoordinator::default(),
        checkpoint_journal_dir: temp.path().join("checkpoints"),
        checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
        checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
        checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
        session_idle_seconds: 1800,
        lifecycle: None,
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
        drive_element_address: 0x0100,
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
    tape.records = vec![
        Record::Block(bootstrap_block),
        Record::Filemark,
        Record::Filemark,
    ];
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
    let (open_write_tx, open_write_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenWrite {
            pool_cfg: pool_cfg.clone(),
            selected: SelectedTape {
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
    const BLOCK_SIZE: u32 = 4096;
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
