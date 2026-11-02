use crate::authority::TapeAuthorityView;
pub(crate) fn terminal_prefix_plan_for_parity_record(
    record: &CommitFixture,
) -> TerminalFinalizationPrefixPlan {
    let mut parity_map = parity_entry(3, remanence_parity::TapeFileKind::ParityMap);
    parity_map.block_count = 2;
    parity_map.physical_start_hint = Some(record.eod_lba);
    parity_map.canonical_metadata_hash = Some([0x70; 32]);
    TerminalFinalizationPrefixPlan {
        diagnostics: remanence_parity::ParityMapDiagnostics {
            writer_version: "remanence-test".into(),
            write_timestamp: "2026-08-09T00:00:00Z".into(),
        },
        start_tape_file_number: 3,
        tail_start_tape_file_number: 4,
        start_lba: record.eod_lba,
        tail_start_lba: record.eod_lba + 3,
        parity_map_tape_file_number: Some(3),
        sidecar_directory_entries: vec![TerminalFinalizationSidecarDirectoryEntry {
            tape_file_number: 2,
            epoch_id: 0,
            protected_ordinal_start: 0,
            protected_ordinal_end_exclusive: 3,
            sidecar_total_block_count: 1,
            sidecar_header_block_count: 1,
            parity_shard_block_count: 1,
            canonical_metadata_hash: [0x61; 32],
            flags: remanence_parity::SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD
                | remanence_parity::SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
        }],
        committed_bundle: remanence_parity::FileRowsReport {
            kind: remanence_parity::FileRowsKind::TerminalPrefix,
            entries: vec![parity_map],
            highest_protected_ordinal: 3,
            total_committed_ordinals: 3,
        },
    }
}

use super::*;
use crate::authority::TapeAuthorityWriter;
use crate::{NativeObjectCopyProjectionInput, NativeObjectProjectionInput, StateError};
use remanence_parity::authority::StructuralAuthority;
use std::fs;

pub(crate) fn record(tape_uuid: [u8; 16]) -> CommitFixture {
    let object_uuid = uuid::Uuid::from_bytes([0x51; 16]);
    CommitFixture {
        ordinal: 1,
        committed_object_count: 1,
        eod_partition: 0,
        eod_lba: 6,
        tape_uuid,
        batch_id: [0x42; 16],
        next_tape_file_number: 2,
        block_size: 256 * 1024,
        objects: vec![ObjectProjectionFixture {
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
            object_recovery_row: ObjectRecoveryFixture {
                tape_file_number: 1,
                stored_block_count: 3,
                object_id: object_uuid.to_string().into_bytes(),
                representation: ObjectRecoveryFixtureRepresentation::Plaintext {
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
        seal_due: None,
    }
}

pub(crate) fn second_record(tape_uuid: [u8; 16]) -> CommitFixture {
    let mut record = record(tape_uuid);
    let object_uuid = uuid::Uuid::from_bytes([0x52; 16]);
    record.ordinal = 2;
    record.committed_object_count = 2;
    record.eod_lba = 10;
    record.batch_id = [0x43; 16];
    record.next_tape_file_number = 3;
    record.objects[0].object.object_id = object_uuid.to_string();
    record.objects[0].object.caller_object_id = Some("checkpoint-test-2".to_string());
    record.objects[0].copy.object_id = object_uuid.to_string();
    record.objects[0].copy.tape_file_number = 2;
    record.objects[0].fresh_tape = false;
    record.objects[0].total_committed_ordinals = 6;
    record.objects[0].object_recovery_row.tape_file_number = 2;
    record.objects[0].object_recovery_row.object_id = object_uuid.to_string().into_bytes();
    record
}

#[test]
fn bot_recovery_authority_does_not_create_a_missing_journal() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x31; 16];
    let path = dir.path().join(format!(
        "{}.remtape",
        uuid::Uuid::from_bytes(tape_uuid).simple()
    ));

    let authority = crate::authority::BotRecoveryAuthority::try_open(
        dir.path(),
        &{
            let p = dir.path().join("bot-index.sqlite");
            drop(crate::CatalogIndex::open(&p).unwrap());
            p
        },
        tape_uuid,
        256 * 1024,
    )
    .expect("probe missing checkpoint authority");

    assert!(authority.is_none());
    assert!(!path.exists(), "foreign recovery must not invent authority");
}

#[test]
fn bot_recovery_authority_streams_exact_checkpoint_rows_and_scope() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x32; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
    lease
        .append(&record(tape_uuid))
        .expect("append first checkpoint");
    lease
        .append(&second_record(tape_uuid))
        .expect("append second checkpoint");
    drop(lease);

    let mut authority = crate::authority::BotRecoveryAuthority::try_open(
        dir.path(),
        &{
            let p = dir.path().join("bot-index.sqlite");
            drop(crate::CatalogIndex::open(&p).unwrap());
            p
        },
        tape_uuid,
        256 * 1024,
    )
    .expect("open checkpoint recovery authority")
    .expect("checkpoint authority exists");
    let mut rows = Vec::new();
    let scope = remanence_parity::BotObjectRecoveryAuthority::visit_object_rows(
        &mut authority,
        &mut |row| {
            rows.push((
                row.tape_file_number,
                row.stored_block_count,
                row.object_id.clone(),
            ));
            Ok(())
        },
    )
    .expect("stream checkpoint Object authority");

    assert_eq!(scope.tape_uuid, tape_uuid);
    assert_eq!(scope.block_size, 256 * 1024);
    assert_eq!(scope.covered_prefix_tape_file_count, 3);
    assert_eq!(scope.object_row_count, 2);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, 1);
    assert_eq!(rows[0].1, 3);
    assert_eq!(rows[1].0, 2);
    assert_eq!(rows[1].1, 3);
}

#[test]
fn bot_recovery_authority_reads_the_ordinary_prefix_of_a_sealed_journal() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x33; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    let prior = record(tape_uuid);
    journal.append(&prior).expect("append ordinary authority");
    let initial = finalization_intent(tape_uuid);
    let completed = completed_finalization_intent(tape_uuid);
    let terminal = structured_terminal_record(&prior, completed);
    let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
    lease.begin_terminal_finalization(&initial).unwrap();
    let mut progress = TerminalFinalizationProgress::BeforeReplicaA;
    while let Some(next) = progress.successor() {
        lease.advance_terminal_finalization(progress, next).unwrap();
        progress = next;
    }
    lease
        .append_terminal_finalization(std::slice::from_ref(&terminal))
        .expect("append terminal completion");
    drop(lease);

    let mut authority = crate::authority::BotRecoveryAuthority::try_open(
        dir.path(),
        &{
            let p = dir.path().join("bot-index.sqlite");
            drop(crate::CatalogIndex::open(&p).unwrap());
            p
        },
        tape_uuid,
        256 * 1024,
    )
    .expect("open sealed checkpoint recovery authority")
    .expect("checkpoint authority exists");
    let mut rows = Vec::new();
    let scope = remanence_parity::BotObjectRecoveryAuthority::visit_object_rows(
        &mut authority,
        &mut |row| {
            rows.push(row.clone());
            Ok(())
        },
    )
    .expect("stream ordinary prefix from sealed checkpoint");

    assert_eq!(
        scope.covered_prefix_tape_file_count,
        prior.next_tape_file_number
    );
    assert_eq!(scope.object_row_count, prior.committed_object_count);
    assert_eq!(rows.len(), prior.objects.len());
    assert_eq!(rows[0].tape_file_number, 1);
}

/// Optional seal obligations survive actual native publication and reopen.
#[test]
fn checkpoint_seal_due_is_optional_and_round_trips() {
    for reason in [
        None,
        Some(TerminalFinalizationTrigger::HardwareEarlyWarning),
        Some(TerminalFinalizationTrigger::ReachedLowWatermark),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut source = record([0x72; 16]);
        source.seal_due = reason;
        let authority =
            crate::authority::fixtures::AuthorityBuilder::new(directory.path(), source.tape_uuid)
                .commit(source.clone())
                .build()
                .unwrap();
        assert_eq!(authority.last().unwrap().unwrap(), source);
    }
}

pub(crate) fn finalization_intent(tape_uuid: [u8; 16]) -> TerminalFinalizationIntent {
    let layout = remanence_parity::TerminalTailLayout::new(0, 256 * 1024, 2, 6, 4, 4_096)
        .expect("terminal layout");
    TerminalFinalizationIntent {
        tape_uuid,
        trigger: TerminalFinalizationTrigger::OperatorCloseOut,
        manual: Some(ManualTerminalFinalizationIdentity {
            operation_id: [0x61; 16],
            operation_kind: "finalize_tape".to_string(),
            actor_fingerprint: "sha256:operator".to_string(),
            idempotency_key: [0x62; 16],
            request_fingerprint: [0x63; 32],
            assigned_pool_id: Some("slow-offsite".to_string()),
            expected_pool_id: Some("slow-offsite".to_string()),
            assignment_generation: 7,
            reason: "ship this partial copy offsite".to_string(),
        }),
        progress: TerminalFinalizationProgress::BeforeReplicaA,
        recovery_required: false,
        edition_id: [0x64; 16],
        edition_sequence: 1,
        edition_digest: [0x65; 32],
        writer_version: "remanence-test".to_string(),
        write_timestamp: "2026-08-09T00:00:00Z".to_string(),
        terminal_prefix: None,
        layout: TerminalFinalizationLayout::try_from(layout).expect("persisted layout"),
    }
}

fn completed_finalization_intent(tape_uuid: [u8; 16]) -> TerminalFinalizationIntent {
    let mut intent = finalization_intent(tape_uuid);
    intent.progress = TerminalFinalizationProgress::AfterReplicaC;
    intent
}

pub(super) fn structured_terminal_record(
    prior: &CommitFixture,
    finalization: TerminalFinalizationIntent,
) -> CommitFixture {
    let replica_c = finalization.layout.components[4];
    CommitFixture {
        ordinal: prior.ordinal + 1,
        committed_object_count: prior.committed_object_count,
        eod_partition: finalization.layout.partition,
        eod_lba: finalization.layout.expected_eod_lba,
        tape_uuid: prior.tape_uuid,
        batch_id: [0x66; 16],
        next_tape_file_number: replica_c
            .tape_file_number
            .checked_add(1)
            .expect("replica C tape-file number"),
        block_size: finalization.layout.block_size,
        objects: Vec::new(),
        scheme: prior.scheme.clone(),
        object_tape_file_bundles: Vec::new(),
        barrier_bundle: Some(remanence_parity::FileRowsReport {
            kind: remanence_parity::FileRowsKind::TerminalComponent,
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
                canonical_metadata_hash: Some(finalization.edition_digest),
                object_recovery_row: None,
            }],
            highest_protected_ordinal: 0,
            total_committed_ordinals: prior
                .objects
                .last()
                .map_or(0, |object| object.total_committed_ordinals),
        }),
        terminal_finalization: Some(finalization),
        sealed_after_write: true,
        seal_due: None,
    }
}

fn advance_test_finalization_to_replica_c(
    lease: &mut crate::authority::TapeAuthorityLease,
    intent: &TerminalFinalizationIntent,
) {
    lease
        .begin_terminal_finalization(intent)
        .expect("publish structured finalization intent");
    for (expected, next) in [
        (
            TerminalFinalizationProgress::BeforeReplicaA,
            TerminalFinalizationProgress::AfterReplicaA,
        ),
        (
            TerminalFinalizationProgress::AfterReplicaA,
            TerminalFinalizationProgress::AfterSeparationAb,
        ),
        (
            TerminalFinalizationProgress::AfterSeparationAb,
            TerminalFinalizationProgress::AfterReplicaB,
        ),
        (
            TerminalFinalizationProgress::AfterReplicaB,
            TerminalFinalizationProgress::AfterSeparationBc,
        ),
        (
            TerminalFinalizationProgress::AfterSeparationBc,
            TerminalFinalizationProgress::AfterReplicaC,
        ),
    ] {
        lease
            .advance_terminal_finalization(expected, next)
            .expect("advance structured finalization component");
    }
}

fn parity_entry(
    tape_file_number: u64,
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
        object_recovery_row: None,
    }
}

pub(crate) fn partial_epoch_parity_record(tape_uuid: [u8; 16]) -> CommitFixture {
    let mut record = record(tape_uuid);
    record.eod_lba = 8;
    record.next_tape_file_number = 3;
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
    object.physical_start_hint = Some(2);
    object.object_id = Some(record.objects[0].object.object_id.clone());
    object.first_parity_data_ordinal = Some(0);
    object.object_recovery_row = Some(record.objects[0].object_recovery_row.to_parity_row());
    record.object_tape_file_bundles = vec![remanence_parity::FileRowsReport {
        kind: remanence_parity::FileRowsKind::Object,
        entries: vec![object],
        highest_protected_ordinal: 0,
        total_committed_ordinals: 3,
    }];
    let mut sidecar = parity_entry(2, remanence_parity::TapeFileKind::ParitySidecar);
    sidecar.epoch_id = Some(0);
    sidecar.protected_ordinal_start = Some(0);
    sidecar.protected_ordinal_end_exclusive = Some(3);
    sidecar.canonical_metadata_hash = Some([0x61; 32]);
    record.barrier_bundle = Some(remanence_parity::FileRowsReport {
        kind: remanence_parity::FileRowsKind::CheckpointSidecars,
        entries: vec![sidecar],
        highest_protected_ordinal: 3,
        total_committed_ordinals: 3,
    });
    record
        .barrier_bundle
        .as_mut()
        .expect("checkpoint bundle")
        .entries[0]
        .physical_start_hint = Some(6);
    record
}

fn second_partial_epoch_parity_record(prior: &CommitFixture) -> CommitFixture {
    let mut record = partial_epoch_parity_record(prior.tape_uuid);
    let object_uuid = uuid::Uuid::from_bytes([0x52; 16]);
    record.ordinal = 2;
    record.committed_object_count = 2;
    record.eod_lba = prior.eod_lba + 6;
    record.batch_id = [0x43; 16];
    record.next_tape_file_number = 5;
    record.objects[0].object.object_id = object_uuid.to_string();
    record.objects[0].object.caller_object_id = Some("checkpoint-test-2".to_string());
    record.objects[0].copy.object_id = object_uuid.to_string();
    record.objects[0].copy.tape_file_number = 3;
    record.objects[0].copy.first_parity_data_ordinal = Some(3);
    record.objects[0].copy.protected_until_ordinal = Some(3);
    record.objects[0].total_committed_ordinals = 6;
    record.objects[0].object_recovery_row.tape_file_number = 3;
    record.objects[0].object_recovery_row.object_id = object_uuid.to_string().into_bytes();

    let mut object = parity_entry(3, remanence_parity::TapeFileKind::Object);
    object.block_count = 3;
    object.physical_start_hint = Some(prior.eod_lba);
    object.object_id = Some(object_uuid.to_string());
    object.first_parity_data_ordinal = Some(3);
    object.object_recovery_row = Some(record.objects[0].object_recovery_row.to_parity_row());
    record.object_tape_file_bundles = vec![remanence_parity::FileRowsReport {
        kind: remanence_parity::FileRowsKind::Object,
        entries: vec![object],
        highest_protected_ordinal: 3,
        total_committed_ordinals: 6,
    }];

    let mut sidecar = parity_entry(4, remanence_parity::TapeFileKind::ParitySidecar);
    sidecar.epoch_id = Some(1);
    sidecar.protected_ordinal_start = Some(3);
    sidecar.protected_ordinal_end_exclusive = Some(6);
    sidecar.canonical_metadata_hash = Some([0x62; 32]);
    sidecar.physical_start_hint = Some(prior.eod_lba + 4);
    record.barrier_bundle = Some(remanence_parity::FileRowsReport {
        kind: remanence_parity::FileRowsKind::CheckpointSidecars,
        entries: vec![sidecar],
        highest_protected_ordinal: 6,
        total_committed_ordinals: 6,
    });
    record
}

#[test]
fn terminal_intent_validates_prefix_coordinates_and_digest_diagnostics() {
    let tape_uuid = [0x7B; 16];
    let record = partial_epoch_parity_record(tape_uuid);
    let prefix = terminal_prefix_plan_for_parity_record(&record);
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        record.block_size,
        prefix.tail_start_tape_file_number,
        prefix.tail_start_lba,
        4,
        4_096,
    )
    .expect("terminal layout after prefix");
    let mut intent = finalization_intent(tape_uuid);
    intent.layout = TerminalFinalizationLayout::try_from(layout).expect("persist layout");
    intent.terminal_prefix = Some(prefix);
    intent
        .validate_for_checkpoint_prefix(Some(&record))
        .expect("validate reconstructible parity intent");

    intent.writer_version.push('\n');
    let writer = intent
        .validate_for_checkpoint_prefix(Some(&record))
        .expect_err("non-printable writer version must fail closed");
    assert!(writer.to_string().contains("writer_version"), "{writer}");
    intent.writer_version = "remanence-test".to_string();
    intent.write_timestamp = "2026-08-09".to_string();
    let timestamp = intent
        .validate_for_checkpoint_prefix(Some(&record))
        .expect_err("non-RFC3339 write timestamp must fail closed");
    assert!(
        timestamp.to_string().contains("write_timestamp"),
        "{timestamp}"
    );
}

/// Decode must validate identity fields after framing and CRC checks succeed.

#[test]
fn fsynced_partial_epoch_parity_checkpoint_round_trips() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x25; 16];
    let record = partial_epoch_parity_record(tape_uuid);
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");

    journal.append(&record).expect("append sidecar checkpoint");
    assert_eq!(
        journal.replay().expect("replay parity checkpoint"),
        vec![record]
    );
}

/// One native writer retains ownership across replay and append.
#[test]
fn exclusive_checkpoint_lease_serializes_replay_through_append() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x24; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
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
fn parity_checkpoint_validator_accepts_sidecars_or_an_exact_boundary() {
    let tape_uuid = [0x26; 16];
    let sidecar_checkpoint = partial_epoch_parity_record(tape_uuid);

    let mut exact_boundary = sidecar_checkpoint.clone();
    let sidecar = exact_boundary
        .barrier_bundle
        .as_mut()
        .expect("checkpoint bundle")
        .entries
        .remove(0);
    exact_boundary.object_tape_file_bundles[0]
        .entries
        .push(sidecar);
    exact_boundary.object_tape_file_bundles[0].highest_protected_ordinal = 3;
    exact_boundary.barrier_bundle = None;

    for record in [sidecar_checkpoint, exact_boundary] {
        crate::authority::validation::validate_parity_barrier_bundles(&(&record).into())
            .unwrap_or_else(|err| panic!("valid checkpoint boundary rejected: {err}"));
    }
}

/// A later barrier must not hide an invalid full epoch at Object close.
#[test]
fn parity_checkpoint_rejects_per_object_epoch_overflow_before_barrier() {
    let mut record = partial_epoch_parity_record([0x28; 16]);
    record.scheme.as_mut().unwrap().data_blocks_per_stripe = 3;
    let error = crate::authority::validation::validate_parity_barrier_bundles(&(&record).into())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("object") && error.contains("open epoch bound"),
        "{error}"
    );
}

#[test]
fn parity_checkpoint_validator_accepts_open_epoch_and_rejects_misnumbered_barriers() {
    let tape_uuid = [0x27; 16];
    let mut unprotected = partial_epoch_parity_record(tape_uuid);
    unprotected.barrier_bundle = None;
    unprotected.next_tape_file_number = 2;
    let native = (&unprotected).into();
    crate::authority::validation::validate_parity_barrier_bundles(&native)
        .expect("short epoch stays open");
    assert_eq!((native.protected, native.total), (0, 3));

    let mut misnumbered = partial_epoch_parity_record(tape_uuid);
    misnumbered.next_tape_file_number = 9;
    assert!(
        crate::authority::validation::validate_parity_barrier_bundles(&(&misnumbered).into())
            .expect_err("record must identify the first free tape file")
            .to_string()
            .contains("expected 3")
    );

    let mut discontinuous_sidecar = partial_epoch_parity_record(tape_uuid);
    discontinuous_sidecar
        .barrier_bundle
        .as_mut()
        .expect("checkpoint bundle")
        .entries[0]
        .protected_ordinal_start = Some(1);
    assert!(
        crate::authority::validation::validate_parity_barrier_bundles(
            &(&discontinuous_sidecar).into()
        )
        .expect_err("sidecar range must start at the prior watermark")
        .to_string()
        .contains("starting at 0")
    );
}

#[test]
fn append_rejects_non_monotonic_count() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x22; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    journal
        .append(&record(tape_uuid))
        .expect("append checkpoint");
    let mut invalid = second_record(tape_uuid);
    invalid.committed_object_count = 9;
    let err = journal
        .append(&invalid)
        .expect_err("invalid count must reject");
    assert!(err.to_string().contains("commit counters/cursor"), "{err}");
}

#[test]
fn append_rejects_a_checkpoint_after_terminal_seal_authority() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x25; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    let prior = record(tape_uuid);
    journal
        .append(&prior)
        .expect("append ordinary checkpoint authority");
    let terminal = structured_terminal_record(&prior, completed_finalization_intent(tape_uuid));
    let err = journal
        .append(&terminal)
        .expect_err("terminal authority must require a structured finalization intent");
    assert!(
        err.to_string()
            .contains("ordinary report with terminal completion"),
        "{err}"
    );
    let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
    advance_test_finalization_to_replica_c(&mut lease, &finalization_intent(tape_uuid));
    lease
        .append_terminal_finalization(std::slice::from_ref(&terminal))
        .expect("append terminal checkpoint authority");
    drop(lease);

    let err = journal
        .append(&second_record(tape_uuid))
        .expect_err("a terminal checkpoint must permanently close the journal");
    assert!(err.to_string().contains("record after seal"), "{err}");
}

#[test]
fn structured_finalization_intent_is_idempotent_monotonic_and_append_fencing() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x71; 16];
    let base = partial_epoch_parity_record(tape_uuid);
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    journal.append(&base).expect("append ordinary checkpoint");
    let mut intent = finalization_intent(tape_uuid);
    let prefix = terminal_prefix_plan_for_parity_record(&base);
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        base.block_size,
        prefix.tail_start_tape_file_number,
        prefix.tail_start_lba,
        4,
        4096,
    )
    .expect("layout after persisted prefix");
    intent.layout = TerminalFinalizationLayout::try_from(layout).unwrap();
    intent.terminal_prefix = Some(prefix.clone());

    let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
    assert_eq!(
        lease
            .begin_terminal_finalization(&intent)
            .expect("publish finalization intent"),
        intent
    );
    assert_eq!(
        lease
            .begin_terminal_finalization(&intent)
            .expect("identical request joins durable intent"),
        intent
    );
    let mut conflicting = intent.clone();
    conflicting.manual.as_mut().expect("manual identity").reason =
        "different exact reason".to_string();
    assert!(matches!(
        lease.begin_terminal_finalization(&conflicting),
        Err(StateError::JournalReplayFailed(_))
    ));
    drop(lease);

    let error = journal
        .acquire_exclusive()
        .expect_err("ordinary Object admission remains fenced");
    assert!(
        error.to_string().contains("terminal finalization"),
        "{error}"
    );

    let mut recovery = journal
        .acquire_exclusive_for_terminal_recovery()
        .expect("recover structured finalization");
    let recovery_authority = recovery
        .replay_for_terminal_recovery()
        .expect("replay prefix without clearing pending fence");
    assert_eq!(recovery_authority.records, vec![base.clone()]);
    assert_eq!(recovery_authority.finalization_intent, Some(intent.clone()));
    let classified = recovery
        .mark_terminal_recovery_required()
        .expect("persist recovery-required classification");
    assert!(classified.recovery_required);
    assert_eq!(classified.terminal_prefix.as_ref(), Some(&prefix));
    assert_eq!(
        recovery
            .terminal_finalization_intent()
            .unwrap()
            .unwrap()
            .terminal_prefix,
        Some(prefix.clone())
    );
    assert_eq!(
        recovery
            .mark_terminal_recovery_required()
            .expect("repeat recovery-required classification"),
        classified
    );
    recovery
        .publish_terminal_prefix(&crate::authority::AuthorityPrefixPlan::from(&prefix).publication)
        .unwrap();
    let transitions = [
        (
            TerminalFinalizationProgress::BeforeReplicaA,
            TerminalFinalizationProgress::AfterReplicaA,
        ),
        (
            TerminalFinalizationProgress::AfterReplicaA,
            TerminalFinalizationProgress::AfterSeparationAb,
        ),
        (
            TerminalFinalizationProgress::AfterSeparationAb,
            TerminalFinalizationProgress::AfterReplicaB,
        ),
        (
            TerminalFinalizationProgress::AfterReplicaB,
            TerminalFinalizationProgress::AfterSeparationBc,
        ),
        (
            TerminalFinalizationProgress::AfterSeparationBc,
            TerminalFinalizationProgress::AfterReplicaC,
        ),
    ];
    for (expected, next) in transitions {
        let advanced = recovery
            .advance_terminal_finalization(expected, next)
            .expect("advance one proved component");
        assert_eq!(advanced.progress, next);
        assert_eq!(advanced.terminal_prefix.as_ref(), Some(&prefix));
        assert_eq!(
            recovery
                .terminal_finalization_intent()
                .unwrap()
                .unwrap()
                .terminal_prefix,
            Some(prefix.clone())
        );
        assert!(!advanced.recovery_required);
        assert_eq!(
            advanced.progress.completed_replicas(),
            next.completed_replicas()
        );
        assert_eq!(
            recovery
                .advance_terminal_finalization(expected, next)
                .expect("repeated progress publication is idempotent")
                .progress,
            next
        );
        drop(recovery);
        let error = journal
            .acquire_exclusive()
            .expect_err("every persisted terminal progress state fences Object admission");
        assert!(
            error.to_string().contains("terminal finalization"),
            "{next:?}: {error}"
        );
        recovery = journal
            .acquire_exclusive_for_terminal_recovery()
            .expect("reacquire terminal recovery after Object-refusal proof");
    }
    assert!(recovery
        .advance_terminal_finalization(
            TerminalFinalizationProgress::AfterReplicaC,
            TerminalFinalizationProgress::BeforeReplicaA,
        )
        .is_err());
    let classified = recovery
        .mark_terminal_recovery_required()
        .expect("classify completed tail before host-only recovery");
    assert_eq!(
        classified.progress,
        TerminalFinalizationProgress::AfterReplicaC
    );
    assert!(classified.recovery_required);
    assert_eq!(classified.terminal_prefix.as_ref(), Some(&prefix));
    assert_eq!(
        recovery
            .terminal_finalization_intent()
            .unwrap()
            .unwrap()
            .terminal_prefix,
        Some(prefix.clone())
    );
}

#[test]
fn provisional_manual_intent_retires_only_with_both_database_halves_absent() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x73; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    journal
        .append(&record(tape_uuid))
        .expect("append ordinary checkpoint");
    let intent = finalization_intent(tape_uuid);
    let mut lease = journal.acquire_exclusive().expect("acquire lease");
    lease
        .begin_terminal_finalization(&intent)
        .expect("publish provisional intent");
    for (projection, binding) in [(true, false), (false, true), (true, true)] {
        let error = lease
            .retire_provisional_manual_finalization(&intent, projection, binding)
            .expect_err("any database acceptance half forbids retirement");
        assert!(error
            .to_string()
            .contains("manual retirement guard refused"));
        assert_eq!(
            lease
                .terminal_finalization_intent()
                .expect("read retained intent"),
            Some(intent.clone())
        );
    }
    lease
        .retire_provisional_manual_finalization(&intent, false, false)
        .expect("both absent proves provisional intent");
    assert!(lease
        .terminal_finalization_intent()
        .expect("read retired provisional intent")
        .is_none());
}

#[test]
fn structured_completion_survives_intent_cleanup_and_checkpoint_only_replay() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x74; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    let prior = record(tape_uuid);
    journal.append(&prior).expect("append ordinary authority");
    let initial = finalization_intent(tape_uuid);
    let completed = completed_finalization_intent(tape_uuid);

    let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
    lease
        .begin_terminal_finalization(&initial)
        .expect("publish structured intent");
    for (expected, next) in [
        (
            TerminalFinalizationProgress::BeforeReplicaA,
            TerminalFinalizationProgress::AfterReplicaA,
        ),
        (
            TerminalFinalizationProgress::AfterReplicaA,
            TerminalFinalizationProgress::AfterSeparationAb,
        ),
        (
            TerminalFinalizationProgress::AfterSeparationAb,
            TerminalFinalizationProgress::AfterReplicaB,
        ),
        (
            TerminalFinalizationProgress::AfterReplicaB,
            TerminalFinalizationProgress::AfterSeparationBc,
        ),
        (
            TerminalFinalizationProgress::AfterSeparationBc,
            TerminalFinalizationProgress::AfterReplicaC,
        ),
    ] {
        lease
            .advance_terminal_finalization(expected, next)
            .expect("advance proved terminal component");
    }
    let terminal = structured_terminal_record(&prior, completed.clone());
    lease
        .append_terminal_finalization(std::slice::from_ref(&terminal))
        .expect("append completion and clear transient intent");
    drop(lease);

    assert!(journal.terminal_finalization_intent().unwrap().is_none());
    let replayed = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("reopen checkpoint-only authority")
        .replay()
        .expect("replay completed terminal authority");
    assert_eq!(replayed.len(), 2);
    assert_eq!(replayed[0], prior);
    assert!(replayed[1].sealed_after_write);
    assert_eq!(replayed[1].eod_lba, terminal.eod_lba);
    assert_eq!(
        replayed[1].next_tape_file_number,
        terminal.next_tape_file_number
    );
    assert!(replayed[1].objects.is_empty());
    let recovered = replayed
        .last()
        .and_then(|record| record.terminal_finalization.as_ref())
        .expect("terminal completion remains in checkpoint authority");
    assert_eq!(recovered, &completed);
    assert_eq!(
        recovered.progress,
        TerminalFinalizationProgress::AfterReplicaC
    );
    assert_eq!(recovered.progress.completed_replicas(), 3);
    assert_eq!(recovered.edition_digest, [0x65; 32]);
    assert_eq!(
        recovered.layout.layout_digest,
        completed.layout.layout_digest
    );
    assert!(replayed.last().expect("terminal record").sealed_after_write);
}

#[test]
fn structured_completion_rejects_partial_progress_and_replica_c_mismatch() {
    let tape_uuid = [0x76; 16];
    let prior = record(tape_uuid);
    let mut terminal = structured_terminal_record(&prior, completed_finalization_intent(tape_uuid));
    terminal
        .terminal_finalization
        .as_mut()
        .expect("completion")
        .progress = TerminalFinalizationProgress::AfterReplicaB;
    let partial = validate_next_record(Some(&prior), &terminal)
        .expect_err("partial terminal progress must not become completion");
    assert!(
        partial.to_string().contains("Sealed report differs"),
        "{partial}"
    );

    terminal = structured_terminal_record(&prior, completed_finalization_intent(tape_uuid));
    terminal
        .barrier_bundle
        .as_mut()
        .expect("final component")
        .entries[0]
        .canonical_metadata_hash = Some([0x77; 32]);
    let mismatch = validate_next_record(Some(&prior), &terminal)
        .expect_err("replica C digest mismatch must fail closed");
    assert!(
        mismatch
            .to_string()
            .contains("terminal component differs from plan"),
        "{mismatch}"
    );
}

#[test]
fn structured_finalization_rejects_pool_guard_and_corrupt_frame_before_motion() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x72; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    journal
        .append(&record(tape_uuid))
        .expect("append ordinary checkpoint prefix");
    let mut mismatch = finalization_intent(tape_uuid);
    mismatch
        .manual
        .as_mut()
        .expect("manual identity")
        .expected_pool_id = None;
    let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
    let error = lease
        .begin_terminal_finalization(&mismatch)
        .expect_err("pooled omission must fail before publication");
    assert!(error.to_string().contains("pool guard"), "{error}");
    assert!(lease.terminal_finalization_intent().unwrap().is_none());

    let intent = finalization_intent(tape_uuid);
    lease
        .begin_terminal_finalization(&intent)
        .expect("publish valid intent");
    drop(lease);
    let intent_path = journal.path();
    let mut bytes = fs::read(intent_path).unwrap();
    *bytes.last_mut().unwrap() ^= 1;
    fs::write(intent_path, bytes).unwrap();
    let error = journal
        .terminal_finalization_intent()
        .expect_err("corrupt finalization intent must fail closed");
    assert!(error.to_string().contains("CRC"), "{error}");
}

#[test]
fn no_parity_terminal_source_streams_dense_map_and_exact_object_bijection() {
    let tape_uuid = [0x73; 16];
    let records = vec![record(tape_uuid)];
    let mut source = fixture_source(&records).expect("validate no-parity terminal source");
    assert_eq!(
        source.summary(),
        TerminalIndexAuthoritySummary {
            scope: remanence_parity::TapeIndexReplicaScope {
                covered_prefix_tape_file_count: 2,
                total_data_ordinals: 3,
                highest_protected_ordinal: 0,
            },
            counts: remanence_parity::TapeIndexReplicaCounts {
                structural_entry_count: 2,
                object_row_count: 1,
            },
        }
    );
    let mut map = Vec::new();
    remanence_parity::TapeIndexReplicaRecordSource::visit_structural_entries(
        &mut source,
        &mut |entry| {
            map.push((entry.tape_file_number, entry.kind, entry.block_count));
            Ok(())
        },
    )
    .expect("stream structural map");
    assert_eq!(
        map,
        vec![
            (0, remanence_parity::TapeIndexReplicaFileKind::Bootstrap, 1,),
            (1, remanence_parity::TapeIndexReplicaFileKind::Object, 3,),
        ]
    );
    let mut object_rows = Vec::new();
    remanence_parity::TapeIndexReplicaRecordSource::visit_object_rows(&mut source, &mut |row| {
        object_rows.push((row.tape_file_number, row.stored_block_count));
        Ok(())
    })
    .expect("stream Object rows");
    assert_eq!(object_rows, vec![(1, 3)]);

    let mut mismatched = records;
    mismatched[0].objects[0]
        .object_recovery_row
        .tape_file_number = 2;
    let error =
        fixture_source(&mismatched).expect_err("map-to-row mismatch must fail before motion");
    assert!(
        error.to_string().contains("Object recovery identity"),
        "{error}"
    );
}

#[test]
fn replay_backed_no_parity_terminal_source_replays_each_replica_pass() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x74; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
    lease
        .append(&record(tape_uuid))
        .expect("append first checkpoint");
    lease
        .append(&second_record(tape_uuid))
        .expect("append second checkpoint");

    let mut source = crate::authority::TerminalIndexSource::new_replay_backed_no_parity(&lease)
        .expect("validate bounded checkpoint replay");
    assert_eq!(
        source.summary(),
        TerminalIndexAuthoritySummary {
            scope: remanence_parity::TapeIndexReplicaScope {
                covered_prefix_tape_file_count: 3,
                total_data_ordinals: 6,
                highest_protected_ordinal: 0,
            },
            counts: remanence_parity::TapeIndexReplicaCounts {
                structural_entry_count: 3,
                object_row_count: 2,
            },
        }
    );
    for _replica in 0..3 {
        let mut map_files = Vec::new();
        remanence_parity::TapeIndexReplicaRecordSource::visit_structural_entries(
            &mut source,
            &mut |entry| {
                map_files.push(entry.tape_file_number);
                Ok(())
            },
        )
        .expect("replay one structural pass");
        assert_eq!(map_files, vec![0, 1, 2]);

        let mut object_files = Vec::new();
        remanence_parity::TapeIndexReplicaRecordSource::visit_object_rows(
            &mut source,
            &mut |row| {
                object_files.push(row.tape_file_number);
                Ok(())
            },
        )
        .expect("replay one Object-row pass");
        assert_eq!(object_files, vec![1, 2]);
    }
}

#[test]
fn owned_no_parity_snapshot_allows_checkpoint_append_and_keeps_frozen_rows() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x7D; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
    lease
        .append(&record(tape_uuid))
        .expect("append first checkpoint");
    let mut source = crate::authority::TerminalIndexSource::new_replay_backed_no_parity(&lease)
        .expect("freeze bounded checkpoint source");

    lease
        .append(&second_record(tape_uuid))
        .expect("owned source must not borrow the mutable checkpoint lease");
    assert_eq!(source.summary().counts.structural_entry_count, 2);
    let mut files = Vec::new();
    remanence_parity::TapeIndexReplicaRecordSource::visit_structural_entries(
        &mut source,
        &mut |entry| {
            files.push(entry.tape_file_number);
            Ok(())
        },
    )
    .expect("replay frozen prefix after later append");
    assert_eq!(files, vec![0, 1]);
}

/// A reopened intent preserves the advancing clock's planned bytes during recovery.
#[test]
fn advancing_identity_survives_intent_reopen_and_recovery_encoding() {
    mod clock_fixture {
        use remanence_parity::WriterIdentity;
        use std::sync::Arc;
        use time::OffsetDateTime;
        include!("../../../remanence-parity/src/advancing_test_identity.rs");
    }
    let dir = tempfile::tempdir().expect("temporary authority directory");
    let tape_uuid = [0x7E; 16];
    let mut record = partial_epoch_parity_record(tape_uuid);
    // Two index copies, two parity shards, and the tail header form this sidecar.
    record.barrier_bundle.as_mut().unwrap().entries[0].block_count = 5;
    record.eod_lba = 12;
    record.barrier_bundle.as_mut().unwrap().entries[0].physical_start_hint = Some(6);
    let scheme = record.scheme.clone().expect("parity scheme");
    let checkpoint_journal =
        crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
            .build()
            .expect("open checkpoint journal");
    let mut checkpoint_lease = checkpoint_journal
        .acquire_exclusive()
        .expect("acquire checkpoint lease");
    checkpoint_lease
        .append(&record)
        .expect("append checkpoint authority");

    let mut parity = checkpoint_lease
        .open_structural(record.block_size, scheme)
        .unwrap();

    let snapshot = parity.freeze().unwrap();
    let (identity, calls) = clock_fixture::advancing_test_identity();
    let plan = remanence_parity::plan_checkpointed_terminal_index_close(
        &snapshot,
        identity.capture().unwrap(),
    )
    .unwrap();
    assert_ne!(identity.capture().unwrap(), plan.diagnostics);
    let calls_before = calls.load(std::sync::atomic::Ordering::SeqCst);
    let mut intent = finalization_intent(tape_uuid);
    intent.writer_version = plan.diagnostics.writer_version.clone();
    intent.write_timestamp = plan.diagnostics.write_timestamp.clone();
    intent.layout = TerminalFinalizationLayout::try_from(
        remanence_parity::TerminalTailLayout::new(
            0,
            record.block_size,
            plan.tail_start_tape_file_number,
            plan.tail_start_lba,
            4,
            4096,
        )
        .unwrap(),
    )
    .unwrap();
    intent.terminal_prefix = Some(TerminalFinalizationPrefixPlan::from(&plan));
    checkpoint_lease
        .begin_terminal_finalization(&intent)
        .unwrap();
    drop(checkpoint_lease);
    drop(checkpoint_journal);
    let reopened = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .unwrap();
    let restored = reopened
        .terminal_finalization_intent()
        .unwrap()
        .expect("persisted intent");
    assert_eq!(restored, intent);
    let restored_plan =
        remanence_parity::TerminalPrefixPlan::try_from(restored.terminal_prefix.as_ref().unwrap())
            .unwrap();
    assert_eq!(restored_plan, plan);
    assert_eq!(
        remanence_parity::plan_checkpointed_terminal_index_close(
            &snapshot,
            restored_plan.diagnostics.clone(),
        )
        .unwrap(),
        plan
    );
    let mut sink = remanence_library::block_io::VecBlockSink::new();
    sink.set_next_lba_for_test(restored_plan.start_lba);
    remanence_parity::close_checkpointed_terminal_index_prefix(
        &mut remanence_parity::BlockSinkRawTapeSink::new(&mut sink),
        &mut parity,
        &snapshot,
        &restored_plan,
        remanence_parity::TerminalPrefixReconcileEvidence::Absent,
    )
    .expect("re-encode reopened plan during recovery");
    let decoded = remanence_parity::parse_parity_map_tape_file(&sink.blocks, &tape_uuid)
        .expect("decode recovered ParityMap");
    assert_eq!(
        Some(decoded.header.payload_sha256),
        plan.committed_bundle.entries[0].canonical_metadata_hash
    );
    assert_eq!(
        decoded.payload.writer_version.as_ref(),
        Some(&plan.diagnostics.writer_version)
    );
    assert_eq!(
        decoded.payload.write_timestamp.as_ref(),
        Some(&plan.diagnostics.write_timestamp)
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        calls_before
    );
}

#[test]
fn planned_prefix_snapshot_survives_durable_parity_transition_without_duplication() {
    let dir = tempfile::tempdir().expect("temporary authority directory");
    let tape_uuid = [0x7E; 16];
    let record = partial_epoch_parity_record(tape_uuid);
    let scheme = record.scheme.clone().expect("parity scheme");
    let checkpoint_journal =
        crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
            .build()
            .expect("open checkpoint journal");
    let mut checkpoint_lease = checkpoint_journal
        .acquire_exclusive()
        .expect("acquire checkpoint lease");
    checkpoint_lease
        .append(&record)
        .expect("append checkpoint authority");

    let parity = checkpoint_lease
        .open_structural(record.block_size, scheme)
        .unwrap();

    let prefix = terminal_prefix_plan_for_parity_record(&record);
    let mut planned =
        crate::authority::TerminalIndexSource::new_replay_backed_with_planned_terminal_prefix(
            &checkpoint_lease,
            &parity,
            &(&prefix).into(),
        )
        .expect("freeze base plus virtual TerminalPrefix");
    assert_eq!(planned.summary().counts.structural_entry_count, 4);
    assert!(planned.replay_metrics().unwrap().parity.is_none());
    assert!(
        planned
            .replay_metrics()
            .unwrap()
            .checkpoint
            .peak_live_object_rows
            <= 1
    );

    let mut intent = finalization_intent(tape_uuid);
    intent.layout = TerminalFinalizationLayout::try_from(
        remanence_parity::TerminalTailLayout::new(
            0,
            record.block_size,
            prefix.tail_start_tape_file_number,
            prefix.tail_start_lba,
            4,
            4096,
        )
        .unwrap(),
    )
    .unwrap();
    intent.terminal_prefix = Some(prefix.clone());
    checkpoint_lease
        .begin_finalization(&(&intent).into())
        .unwrap();
    checkpoint_lease
        .publish_terminal_prefix(&crate::authority::AuthorityPrefixPlan::from(&prefix).publication)
        .unwrap();

    let mut planned_files = Vec::new();
    remanence_parity::TapeIndexReplicaRecordSource::visit_structural_entries(
        &mut planned,
        &mut |entry| {
            planned_files.push(entry.tape_file_number);
            Ok(())
        },
    )
    .expect("replay frozen base plus one virtual prefix");
    assert_eq!(planned_files, vec![0, 1, 2, 3]);

    checkpoint_lease
        .advance_finalization(
            TerminalFinalizationProgress::BeforeReplicaA,
            TerminalFinalizationProgress::AfterReplicaA,
        )
        .unwrap();

    let mut durable =
        crate::authority::TerminalIndexSource::new_replay_backed_after_terminal_prefix(
            &checkpoint_lease,
            &parity,
            &(&prefix).into(),
        )
        .expect("freeze durable prefix while excluding later component progress");
    let mut durable_files = Vec::new();
    remanence_parity::TapeIndexReplicaRecordSource::visit_structural_entries(
        &mut durable,
        &mut |entry| {
            durable_files.push(entry.tape_file_number);
            Ok(())
        },
    )
    .expect("replay durable prefix exactly once");
    assert_eq!(durable_files, planned_files);
}

#[test]
fn high_count_checkpoint_replay_reports_one_pass_and_bounded_live_rows() {
    const CHECKPOINT_COUNT: u64 = 128;
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x75; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
    let mut prior = None;
    for index in 0..CHECKPOINT_COUNT {
        let mut next = record(tape_uuid);
        let ordinal = index + 1;
        let object_uuid = uuid::Uuid::from_u128(u128::from(ordinal));
        next.ordinal = ordinal;
        next.committed_object_count = ordinal;
        next.eod_lba = ordinal * 4 + 2;
        next.batch_id = object_uuid.into_bytes();
        next.next_tape_file_number = ordinal + 1;
        next.objects[0].object.object_id = object_uuid.to_string();
        next.objects[0].copy.object_id = object_uuid.to_string();
        next.objects[0].copy.tape_file_number = ordinal;
        next.objects[0].fresh_tape = ordinal == 1;
        next.objects[0].total_committed_ordinals = ordinal * 3;
        next.objects[0].object_recovery_row.tape_file_number = ordinal;
        next.objects[0].object_recovery_row.object_id = object_uuid.to_string().into_bytes();
        lease.append(&next).expect("append high-count checkpoint");
        prior = Some(next);
    }
    assert_eq!(
        prior
            .expect("high-count fixture has a final record")
            .ordinal,
        CHECKPOINT_COUNT
    );

    drop(lease);
    let frozen = journal.freeze().expect("validate bounded native history");
    let metrics = frozen.raw.replay_metrics();
    assert_eq!(metrics.validation_passes, 1);
    assert!(metrics.peak_frame_buffer <= crate::tape_log::MAX_FRAME_BODY);
    assert!(metrics.peak_catalog_bytes < crate::tape_log::MAX_FRAME_BODY);
    let lease = journal.acquire_exclusive().unwrap();

    let mut source = crate::authority::TerminalIndexSource::new_replay_backed_no_parity(&lease)
        .expect("validate high-count replay-backed source");
    assert_eq!(
        source.summary().counts,
        remanence_parity::TapeIndexReplicaCounts {
            structural_entry_count: CHECKPOINT_COUNT + 1,
            object_row_count: CHECKPOINT_COUNT,
        }
    );
    let mut emitted = 0u64;
    remanence_parity::TapeIndexReplicaRecordSource::visit_object_rows(&mut source, &mut |_| {
        emitted += 1;
        Ok(())
    })
    .expect("stream high-count Object rows");
    assert_eq!(emitted, CHECKPOINT_COUNT);
}

#[test]
fn final_edition_reconstruction_binds_scope_counts_and_diagnostics() {
    let tape_uuid = [0x7C; 16];
    let records = vec![record(tape_uuid)];
    let mut source = fixture_source(&records).expect("validate terminal source");
    let summary = source.summary();
    let replica_records =
        remanence_parity::checked_tape_index_replica_layout(records[0].block_size, summary.counts)
            .expect("replica layout")
            .replica_record_count;
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        records[0].block_size,
        records[0].next_tape_file_number,
        records[0].eod_lba,
        replica_records,
        4_096,
    )
    .expect("terminal layout");
    let descriptor = remanence_parity::TapeIndexEditionDescriptor {
        tape_uuid,
        edition_id: [0x64; 16],
        edition_sequence: 1,
        scope: summary.scope,
        counts: summary.counts,
        block_size: records[0].block_size,
        compression_enabled: false,
        writer_version: "remanence-test".to_string(),
        write_timestamp: "2026-08-09T00:00:00Z".to_string(),
        terminal_layout: layout,
    };
    let planned = remanence_parity::plan_tape_index_edition(descriptor, &mut source)
        .expect("plan canonical edition");
    let mut intent = finalization_intent(tape_uuid);
    intent.layout = TerminalFinalizationLayout::try_from(layout).expect("persist layout");
    intent.edition_digest = planned.edition_digest;

    let reconstructed = source
        .reconstruct_final_edition(&(&intent).into())
        .expect("reconstruct exact persisted edition");
    assert_eq!(reconstructed, planned);

    intent.writer_version = "remanence-upgraded".to_string();
    let mismatch = source
        .reconstruct_final_edition(&(&intent).into())
        .expect_err("changed writer diagnostics must change the edition digest");
    assert!(mismatch.to_string().contains("digest"), "{mismatch}");
}

#[test]
fn finalization_resume_reconstructs_persisted_intent_through_shared_assembly() {
    let dir = tempfile::tempdir().expect("temporary checkpoint directory");
    let tape_uuid = [0x7C; 16];
    let journal = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("open journal");
    let mut lease = journal.acquire_exclusive().expect("acquire lease");
    let checkpoint = record(tape_uuid);
    lease.append(&checkpoint).expect("persist checkpoint");
    let mut source = crate::authority::TerminalIndexSource::new_replay_backed_no_parity(&lease)
        .expect("checkpoint authority");
    let summary = source.summary();
    let replica =
        remanence_parity::checked_tape_index_replica_layout(checkpoint.block_size, summary.counts)
            .expect("replica geometry");
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        checkpoint.block_size,
        checkpoint.next_tape_file_number,
        checkpoint.eod_lba,
        replica.replica_record_count,
        4096,
    )
    .expect("tail layout");
    let mut intent = finalization_intent(tape_uuid);
    let planned = remanence_parity::assemble_terminal_plan(
        tape_uuid,
        checkpoint.block_size,
        false,
        intent.edition_sequence,
        summary.scope,
        summary.counts,
        &mut source,
        layout,
        remanence_parity::ParityMapDiagnostics {
            writer_version: intent.writer_version.clone(),
            write_timestamp: intent.write_timestamp.clone(),
        },
        intent.edition_id,
        remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
    )
    .expect("shared assembly");
    intent.layout = TerminalFinalizationLayout::try_from(layout).expect("persist layout");
    intent.edition_digest = planned.edition.edition_digest;
    lease
        .begin_terminal_finalization(&intent)
        .expect("persist new intent");
    drop(lease);
    drop(journal);

    let reopened = crate::authority::fixtures::AuthorityBuilder::new(dir.path(), tape_uuid)
        .build()
        .expect("reopen journal");
    let lease = reopened
        .acquire_exclusive_for_terminal_recovery()
        .expect("resume lease");
    let persisted = lease
        .terminal_finalization_intent()
        .expect("read intent")
        .expect("existing intent");
    assert_eq!(persisted, intent);
    let mut source = crate::authority::TerminalIndexSource::new_replay_backed_no_parity(&lease)
        .expect("replay checkpoint authority");
    let resumed = source
        .reconstruct_final_edition(&(&persisted).into())
        .expect("resume exact edition");
    assert_eq!(resumed, planned.edition);
    assert_eq!(resumed.descriptor.edition_id, persisted.edition_id);
    let mut corrupt = persisted;
    corrupt.edition_digest[0] ^= 1;
    assert!(source
        .reconstruct_final_edition(&(&corrupt).into())
        .expect_err("digest mismatch must fail closed")
        .to_string()
        .contains("digest"));
}

#[test]
fn terminal_checkpoint_names_first_free_file_after_replica_c() {
    let tape_uuid = [0x26; 16];
    let prior = record(tape_uuid);
    let mut terminal = structured_terminal_record(&prior, completed_finalization_intent(tape_uuid));
    validate_next_record(Some(&prior), &terminal)
        .expect("replica C completion names the first free tape file");

    let replica_c = terminal
        .terminal_finalization
        .as_ref()
        .expect("structured completion")
        .layout
        .components[4];
    terminal.next_tape_file_number = replica_c.tape_file_number;
    let mismatch = validate_next_record(Some(&prior), &terminal)
        .expect_err("replica C itself is not the next free tape file");
    assert!(
        mismatch.to_string().contains("Sealed report differs"),
        "{mismatch}"
    );
}

#[test]
fn validation_rejects_parity_scheme_change_between_checkpoints() {
    let previous = partial_epoch_parity_record([0x23; 16]);
    let mut next = second_partial_epoch_parity_record(&previous);
    next.scheme.as_mut().unwrap().id = remanence_parity::SchemeId::new_static("different-scheme");
    let err = validate_next_record(Some(&previous), &next).expect_err("scheme change must refuse");
    assert!(err.to_string().contains("geometry"), "{err}");
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
    rec.objects[0].object_recovery_row.object_id = opaque.as_bytes().to_vec();

    validate_next_record(None, &rec).expect("a non-UUID opaque object_id must validate");
}
/// Validate candidates through the native writer, including its immutable terminal plan.
fn validate_next_record(
    previous: Option<&CommitFixture>,
    next: &CommitFixture,
) -> Result<(), StateError> {
    let directory = tempfile::tempdir().unwrap();
    let authority =
        crate::authority::fixtures::AuthorityBuilder::new(directory.path(), next.tape_uuid)
            .build()?;
    let mut writer = authority.acquire_exclusive()?;
    if let Some(previous) = previous {
        writer.publish_report(&previous.into())?;
    }
    if next.sealed_after_write {
        let plan = next.terminal_finalization.as_ref().unwrap();
        let mut initial: crate::authority::AuthorityFinalization = plan.into();
        initial.progress = TerminalFinalizationProgress::BeforeReplicaA;
        writer.begin_finalization(&initial)?;
        if let Some(prefix) = &initial.terminal_prefix {
            writer
                .publish_terminal_prefix(&prefix.publication)
                .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
        }
        let mut progress = initial.progress;
        while progress != plan.progress {
            let after = progress.successor().unwrap();
            if after == TerminalFinalizationProgress::AfterReplicaC {
                let publication = remanence_parity::authority::FilePublication::try_from(
                    next.barrier_bundle.as_ref().unwrap(),
                )
                .unwrap();
                writer
                    .publish_terminal_component(&publication)
                    .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
            } else {
                writer.advance_finalization(progress, after)?;
            }
            progress = after;
        }
        writer.publish_sealed_reports(&[next.into()], || Ok(()))
    } else {
        writer.publish_report(&next.into())
    }
}

impl TerminalFinalizationIntent {
    fn validate_for_checkpoint_prefix(
        &self,
        previous: Option<&CommitFixture>,
    ) -> Result<(), StateError> {
        let directory = tempfile::tempdir().unwrap();
        let authority =
            crate::authority::fixtures::AuthorityBuilder::new(directory.path(), self.tape_uuid)
                .build()?;
        let mut writer = authority.acquire_exclusive()?;
        if let Some(previous) = previous {
            writer.publish_report(&previous.into())?;
        }
        writer.begin_finalization(&self.into()).map(|_| ())
    }
}

/// A native frozen source keeps its inode alive after the temporary directory is removed.
fn fixture_source(records: &[CommitFixture]) -> Result<crate::TerminalIndexSource, StateError> {
    let directory = tempfile::tempdir().unwrap();
    let mut builder =
        crate::authority::fixtures::AuthorityBuilder::new(directory.path(), records[0].tape_uuid);
    for record in records {
        builder = builder.commit(record.clone());
    }
    let authority = builder.build()?;
    let writer = authority.acquire_exclusive()?;
    crate::TerminalIndexSource::new_replay_backed_no_parity(&writer)
}
