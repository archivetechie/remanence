//! P2 regression coverage for durable endpoint keys, media evidence, and ledger
//! authority. Predecessor refusal uses the frozen released-schema fixture.
use remanence_parity::ParityConfig;
use remanence_state::{
    tape_generation, CatalogIndex, DriveKey, GenerationSource, LabelProvenance, LtoGen,
    MediaObservation, MediaReadinessOperationInput, ProvisionTapeInput, TapeIoFenceInput,
    TapePoolProjectionInput, TapePoolRuleConfig,
};
use rusqlite::{params, Connection};
use uuid::Uuid;

fn provision(index: &mut CatalogIndex, id: u8, label: &str) {
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: [id; 16],
            voltag: label.into(),
            block_size: 262144,
            parity: ParityConfig::None,
            force: false,
        })
        .unwrap();
}
fn pool() -> TapePoolProjectionInput {
    TapePoolProjectionInput {
        pool_id: "archive".into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        created_at_utc: None,
    }
}

#[test]
fn predecessor_geometry_and_bay_zero_rows_require_reset_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("legacy.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(include_str!(
        "../../../fixtures/standalone/index-v0.1.0.sql"
    ))
    .unwrap();
    db.execute("insert into tapes(tape_uuid, voltag, state, updated_at_utc, highest_protected_ordinal, total_committed_ordinals, written_extent_lba) values (?1, 'OLD001L7', 'ready', 'now', 7, 9, 42)", [[1u8;16].as_slice()]).unwrap();
    db.execute_batch("insert into media_readiness_ops(operation_id, library_serial, drive_element, phase, state, started_at_utc, updated_at_utc) values ('00000000-0000-0000-0000-000000000001', 'LIB:0', 0, 'poll', 'planned', 'now', 'now');
        insert into media_readiness_transitions(operation_id, state, at_utc) values ('00000000-0000-0000-0000-000000000001','planned','now');
        insert into sessions(session_id, session_kind, library_serial, drive_bay, state, opened_at_utc, updated_at_utc) values ('00000000-0000-0000-0000-000000000002','read','LIB:0',0,'open','now','now');
        insert into tape_files(tape_uuid, tape_file_number, kind, block_count) values (X'01010101010101010101010101010101',3,'object',23);").unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    for _ in 0..2 {
        let error = CatalogIndex::open(&path).unwrap_err();
        assert!(error
            .to_string()
            .contains(remanence_state::authority_layout::RESET_REQUIRED));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn clearing_bay_coordinates_clears_old_key_but_preserves_explicit_standalone_key() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.sqlite");
    let _index = CatalogIndex::open(&path).unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("insert into drives(drive_uuid, serial, identity_source, managed, first_seen_utc, last_seen_utc, last_library_serial, last_element_address)
        values (X'01010101010101010101010101010101','drive','vpd','rem','now','now','LIB',0);").unwrap();
    let key = || {
        db.query_row("select endpoint_key from drives", [], |r| {
            r.get::<_, Option<String>>(0)
        })
        .unwrap()
    };
    assert_eq!(key(), Some(DriveKey::new("LIB", 0).encode()));
    db.execute_batch("update drives set last_library_serial = null, last_element_address = null;")
        .unwrap();
    assert_eq!(key(), None);
    db.execute_batch("update drives set last_library_serial = 'LIB', last_element_address = 1;")
        .unwrap();
    assert_eq!(key(), Some(DriveKey::new("LIB", 1).encode()));
    let standalone = DriveKey::Standalone {
        drive_serial: "drive".into(),
    }
    .encode();
    db.execute("update drives set last_library_serial = null, last_element_address = null, endpoint_key = ?1", [&standalone]).unwrap();
    assert_eq!(key(), Some(standalone));
}

#[test]
fn rebuild_preserves_recorded_generation_without_a_label_or_assignment() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.sqlite");
    let mut index = CatalogIndex::open(&path).unwrap();
    provision(&mut index, 1, "OLD001L7");
    index
        .set_tape_generation(
            [1; 16],
            MediaObservation {
                generation: LtoGen::Lto8,
                source: GenerationSource::Operator,
            },
        )
        .unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("update tapes set voltag = null, voltag_source = null, state = 'ingested';")
        .unwrap();
    index.rebuild_from_authoritative_sources(&[], &[]).unwrap();
    let tape = index.get_tape(&[1; 16]).unwrap().unwrap();
    assert_eq!(tape.voltag, None);
    assert_eq!(tape.pool_id, None);
    assert_eq!(
        tape_generation(&tape),
        Some((LtoGen::Lto8, GenerationSource::Operator))
    );
}

#[test]
fn invalid_legacy_geometry_rolls_back_the_entire_migration() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("legacy.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(include_str!(
        "../../../fixtures/standalone/index-v0.1.0.sql"
    ))
    .unwrap();
    db.execute("insert into tapes(tape_uuid, state, updated_at_utc, total_committed_ordinals) values (?1, 'ready','now',-1)",[[2u8;16].as_slice()]).unwrap();
    assert!(CatalogIndex::open(&path).is_err());
    assert_eq!(
        db.query_row("pragma user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        16
    );
    assert_eq!(
        db.query_row("select total_committed_ordinals from tapes", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        -1
    );
    assert_eq!(
        db.query_row(
            "select count(*) from sqlite_master where name like 'p2_%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn standalone_readiness_and_fence_round_trip_without_library_coordinates() {
    let mut index = CatalogIndex::open(":memory:").unwrap();
    let endpoint = DriveKey::Standalone {
        drive_serial: "LIB:0\"/drive".into(),
    };
    assert_ne!(
        endpoint.encode(),
        DriveKey::new("LIB:0\"/drive", 0).encode()
    );
    assert_eq!(DriveKey::decode(&endpoint.encode()).unwrap(), endpoint);
    let row = index
        .record_media_readiness_operation(MediaReadinessOperationInput {
            operation_id: Uuid::new_v4(),
            run_id: None,
            endpoint: endpoint.clone(),
            changer_sg: None,
            drive_sg: Some("/dev/sg1".into()),
            drive_serial: Some("LIB:0\"/drive".into()),
            barcode: None,
            source_slot: None,
            media_generation: None,
            phase: "poll".into(),
            state: "planned".into(),
            dirty_scope: Some("drive+tape".into()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .unwrap();
    assert_eq!(row.endpoint, endpoint);
    assert_eq!((row.library_serial, row.drive_element), (None, None));
    assert_eq!(
        index
            .media_readiness_admission_conflicts(&endpoint, None, None, false)
            .unwrap()
            .len(),
        1
    );
    assert!(index
        .media_readiness_admission_conflicts(&DriveKey::new("LIB:0\"/drive", 0), None, None, false)
        .unwrap()
        .is_empty());
    let fence = index
        .record_tape_io_fence_at_endpoint(
            TapeIoFenceInput {
                tape_uuid: [1; 16],
                barcode: None,
                reason: "uncertain_write".into(),
                evidence_json: None,
            },
            Some(&endpoint),
        )
        .unwrap();
    assert_eq!(fence.endpoint, Some(endpoint));
}

#[test]
fn explicit_unassignment_and_same_pool_takeover_survive_rules() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.sqlite");
    let mut index = CatalogIndex::open(&path).unwrap();
    provision(&mut index, 1, "AUTO01L7");
    let rules = [TapePoolRuleConfig {
        pool_id: "archive".into(),
        prefix: "AUTO".into(),
    }];
    index
        .reconcile_tape_pool_projection_from_rules(&[pool()], &rules)
        .unwrap();
    let before = index
        .get_tape_assignment_snapshot(&[1; 16])
        .unwrap()
        .unwrap();
    let manual = index
        .compare_and_set_tape_pool_assignment(
            [1; 16],
            before.assignment_generation,
            Some("archive"),
        )
        .unwrap();
    assert_eq!(
        manual.assignment_generation,
        before.assignment_generation + 1
    );
    index
        .reconcile_tape_pool_projection_from_rules(&[pool()], &[])
        .unwrap();
    assert_eq!(
        index
            .get_tape_assignment_snapshot(&[1; 16])
            .unwrap()
            .unwrap(),
        manual
    );
    let cleared = index
        .compare_and_set_tape_pool_assignment([1; 16], manual.assignment_generation, None)
        .unwrap();
    index
        .reconcile_tape_pool_projection_from_rules(&[pool()], &rules)
        .unwrap();
    assert_eq!(
        index
            .get_tape_assignment_snapshot(&[1; 16])
            .unwrap()
            .unwrap(),
        cleared
    );
    assert!(index
        .compare_and_set_tape_pool_assignment(
            [1; 16],
            manual.assignment_generation,
            Some("archive")
        )
        .is_err());
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("select count(*) from tape_pool_assignments", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
    db.execute(
        "update tapes set state = 'sealed' where tape_uuid = ?1",
        params![[1u8; 16].as_slice()],
    )
    .unwrap();
    index
        .reconcile_tape_pool_projection_from_rules(&[pool()], &rules)
        .unwrap();
    assert_eq!(
        index
            .get_tape_assignment_snapshot(&[1; 16])
            .unwrap()
            .unwrap(),
        cleared
    );
}

#[test]
fn observed_generation_is_independent_of_label_and_operator_reuse_needs_rebind() {
    let mut index = CatalogIndex::open(":memory:").unwrap();
    let observation = MediaObservation {
        generation: LtoGen::Lto7,
        source: GenerationSource::Measured,
    };
    let input = ProvisionTapeInput {
        tape_uuid: [1; 16],
        voltag: "PEN001L9".into(),
        block_size: 262144,
        parity: ParityConfig::None,
        force: false,
    };
    index
        .provision_tape_with_media(input.clone(), LabelProvenance::Operator, Some(observation))
        .unwrap();
    // A legacy idempotent init hint must not overwrite measured capacity.
    index.provision_tape(input.clone()).unwrap();
    assert_eq!(
        tape_generation(&index.get_tape(&[1; 16]).unwrap().unwrap()),
        Some((LtoGen::Lto7, GenerationSource::Measured))
    );
    let mut reused = input;
    reused.tape_uuid = [2; 16];
    reused.force = true;
    assert!(index
        .provision_tape_with_media(reused, LabelProvenance::Operator, Some(observation))
        .is_err());
    provision(&mut index, 2, "PEN002L7");
    assert!(index.project_audit_record(&rebind_record(1, 3)).is_err());
    index.project_audit_record(&rebind_record(2, 1)).unwrap();
    assert_eq!(index.get_tape(&[1; 16]).unwrap().unwrap().voltag, None);
    assert_eq!(
        index.get_tape(&[2; 16]).unwrap().unwrap().voltag_source,
        Some(LabelProvenance::Operator)
    );
    let hint = MediaObservation::label_hint("PEN001L9");
    assert!(MediaObservation::measured(LtoGen::Lto7, hint, false).is_err());
    assert_eq!(
        MediaObservation::measured(LtoGen::Lto7, hint, true).unwrap(),
        observation
    );
}

fn audit(
    sequence: u64,
    event: remanence_state::AuditEvent,
    id: u8,
    detail: &[(&str, ciborium::value::Value)],
) -> remanence_state::AuditRecord {
    remanence_state::AuditRecord {
        schema_version: 1,
        record_uuid: Uuid::from_u128(sequence as u128),
        sequence,
        timestamp_utc: "2026-10-15T00:00:00Z".into(),
        host_id: "test".into(),
        process_id: 1,
        software_build: None,
        actor: remanence_state::AuditActor::User("tester".into()),
        source_layer: remanence_state::SourceLayer::Layer4,
        operation_id: None,
        session_id: None,
        idempotency_key: None,
        event,
        subject: remanence_state::AuditSubject {
            kind: "tape".into(),
            id: Some(Uuid::from_bytes([id; 16]).simple().to_string()),
        },
        detail: detail
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect(),
    }
}
fn assignment(sequence: u64, pool: Option<&str>) -> remanence_state::AuditRecord {
    use ciborium::value::Value;
    audit(
        sequence,
        remanence_state::AuditEvent::TapePoolAssigned,
        1,
        &[
            (
                "pool_id",
                pool.map(|p| Value::Text(p.into())).unwrap_or(Value::Null),
            ),
            ("assignment_source", Value::Text("operator".into())),
            ("assignment_generation", Value::Integer(sequence.into())),
        ],
    )
}
fn rebind_record(sequence: u64, previous: u8) -> remanence_state::AuditRecord {
    use ciborium::value::Value;
    audit(
        sequence,
        remanence_state::AuditEvent::TapeLabelRebound,
        2,
        &[
            ("voltag", Value::Text("PEN001L9".into())),
            ("previous_uuid", Value::Bytes(vec![previous; 16])),
        ],
    )
}

#[test]
fn assignment_replay_preserves_order_null_intent_and_projection_generation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.sqlite");
    let mut index = CatalogIndex::open(&path).unwrap();
    provision(&mut index, 1, "AUTO01L7");
    let mut pools = vec![pool()];
    pools.push(TapePoolProjectionInput {
        pool_id: "second".into(),
        ..pool()
    });
    for pool in &pools {
        index.upsert_tape_pool_projection(pool.clone()).unwrap();
    }
    // The live path already performed A→B before its audit is replayed.
    index
        .compare_and_set_tape_pool_assignment([1; 16], 0, Some("archive"))
        .unwrap();
    index
        .compare_and_set_tape_pool_assignment([1; 16], 1, Some("second"))
        .unwrap();
    let events = [
        assignment(1, Some("archive")),
        assignment(2, Some("second")),
    ];
    for _ in 0..2 {
        index.replay_audit_records(&events).unwrap();
    }
    assert_eq!(
        index
            .get_tape_assignment_snapshot(&[1; 16])
            .unwrap()
            .unwrap()
            .assignment_generation,
        2
    );
    let clear = assignment(3, None);
    index.project_audit_record(&clear).unwrap();
    index.project_audit_record(&events[0]).unwrap();
    let mut all = events.to_vec();
    all.push(clear);
    index.replay_audit_records(&all).unwrap();
    index.rebuild_from_authoritative_sources(&all, &[]).unwrap();
    index
        .reconcile_tape_pool_projection_from_rules(
            &pools,
            &[TapePoolRuleConfig {
                prefix: "AUTO".into(),
                pool_id: "archive".into(),
            }],
        )
        .unwrap();
    let current = index
        .get_tape_assignment_snapshot(&[1; 16])
        .unwrap()
        .unwrap();
    assert_eq!(current.pool_id, None);
    assert_eq!(current.assignment_generation, 3);
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("select count(*) from tape_pool_assignments", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
}

#[test]
fn historical_library_init_assignment_remains_rule_derived() {
    use ciborium::value::Value;
    let mut index = CatalogIndex::open(":memory:").unwrap();
    provision(&mut index, 1, "AUTO01L7");
    index.upsert_tape_pool_projection(pool()).unwrap();
    let events = [
        audit(
            1,
            remanence_state::AuditEvent::TapeProvisioned,
            1,
            &[
                ("voltag", Value::Text("AUTO01L7".into())),
                ("pool_id", Value::Text("archive".into())),
            ],
        ),
        audit(
            2,
            remanence_state::AuditEvent::TapePoolAssigned,
            1,
            &[("pool_id", Value::Text("archive".into()))],
        ),
    ];
    index.replay_audit_records(&events).unwrap();
    index
        .reconcile_tape_pool_projection_from_rules(&[pool()], &[])
        .unwrap();
    index.replay_audit_records(&events).unwrap();
    assert_eq!(index.get_tape(&[1; 16]).unwrap().unwrap().pool_id, None);
}

#[test]
fn provision_rebind_replay_and_rebuild_keep_final_label_binding() {
    use ciborium::value::Value;
    let mut index = CatalogIndex::open(":memory:").unwrap();
    let records = [
        audit(
            1,
            remanence_state::AuditEvent::TapeProvisioned,
            1,
            &[("voltag", Value::Text("PEN001L9".into()))],
        ),
        audit(
            2,
            remanence_state::AuditEvent::TapeProvisioned,
            2,
            &[("voltag", Value::Text("PEN002L9".into()))],
        ),
        rebind_record(3, 1),
    ];
    index.replay_audit_records(&records).unwrap();
    for _ in 0..2 {
        index.replay_audit_records(&records).unwrap();
        index
            .rebuild_from_authoritative_sources(&records, &[])
            .unwrap();
        index.project_audit_record(&records[0]).unwrap();
        let old = index.get_tape(&[1; 16]).unwrap().unwrap();
        assert_eq!((old.voltag, old.voltag_source), (None, None));
        let new = index.get_tape(&[2; 16]).unwrap().unwrap();
        assert_eq!(new.voltag.as_deref(), Some("PEN001L9"));
        assert_eq!(new.voltag_source, Some(LabelProvenance::Operator));
    }
    // The same authority also reconstructs the binding in a brand-new index.
    let mut fresh = CatalogIndex::open(":memory:").unwrap();
    fresh
        .rebuild_from_authoritative_sources(&records, &[])
        .unwrap();
    assert_eq!(fresh.get_tape(&[1; 16]).unwrap().unwrap().voltag, None);
    assert_eq!(
        fresh
            .get_tape_by_voltag("PEN001L9")
            .unwrap()
            .unwrap()
            .tape_uuid,
        vec![2; 16]
    );
}

#[test]
fn readiness_uses_recorded_generation_and_only_hints_before_row_creation() {
    let mut index = CatalogIndex::open(":memory:").unwrap();
    assert_eq!(
        index.readiness_media_observation(Some("CAP001L9")).unwrap(),
        MediaObservation::label_hint("CAP001L9")
    );
    provision(&mut index, 1, "CAP001L9");
    let measured = MediaObservation {
        generation: LtoGen::Lto7,
        source: GenerationSource::Measured,
    };
    index.set_tape_generation([1; 16], measured).unwrap();
    assert_eq!(
        index.readiness_media_observation(Some("CAP001L9")).unwrap(),
        Some(measured)
    );
    provision(&mut index, 2, "handwritten");
    assert_eq!(
        index
            .readiness_media_observation(Some("handwritten"))
            .unwrap(),
        None
    );
}

fn readiness(index: &mut CatalogIndex, endpoint: DriveKey, label: &str, scope: Option<&str>) {
    index
        .record_media_readiness_operation(MediaReadinessOperationInput {
            operation_id: Uuid::new_v4(),
            run_id: None,
            endpoint,
            changer_sg: None,
            drive_sg: None,
            drive_serial: None,
            barcode: Some(label.into()),
            source_slot: None,
            media_generation: None,
            phase: "poll".into(),
            state: "planned".into(),
            dirty_scope: scope.map(str::to_string),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .unwrap();
}
#[test]
fn endpoint_admission_preserves_cartridge_robotics_and_unknown_scope_guards() {
    let mut index = CatalogIndex::open(":memory:").unwrap();
    readiness(
        &mut index,
        DriveKey::new("LIB", 1),
        "TAPE1",
        Some("drive+tape"),
    );
    let second = DriveKey::new("LIB", 2);
    assert_eq!(
        index
            .media_readiness_admission_conflicts(&second, None, Some("TAPE1"), false)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        index
            .media_readiness_admission_conflicts(&second, None, None, true)
            .unwrap()
            .len(),
        1
    );
    assert!(index
        .media_readiness_admission_conflicts(&second, None, None, false)
        .unwrap()
        .is_empty());
    readiness(&mut index, DriveKey::new("LIB", 3), "TAPE3", None);
    assert_eq!(
        index
            .media_readiness_admission_conflicts(&second, None, None, false)
            .unwrap()
            .len(),
        1
    );
    let standalone = DriveKey::Standalone {
        drive_serial: "drive".into(),
    };
    assert_eq!(
        index
            .media_readiness_admission_conflicts(&standalone, None, Some("TAPE1"), false)
            .unwrap()
            .len(),
        1
    );
    assert!(index
        .media_readiness_admission_conflicts(&standalone, None, None, false)
        .unwrap()
        .is_empty());
    readiness(&mut index, standalone.clone(), "LOADED", Some("unknown"));
    assert_eq!(
        index
            .media_readiness_admission_conflicts(&standalone, None, None, false)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn predecessor_terminal_assignment_requires_reset_without_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("legacy.sqlite");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(include_str!(
        "../../../fixtures/standalone/index-v0.1.0.sql"
    ))
    .unwrap();
    db.execute_batch(
        "alter table tapes add column assignment_generation integer not null default 0;
        insert into tape_pools(pool_id,created_at_utc) values('archive','now');
        insert into tapes(tape_uuid,voltag,pool_id,state,updated_at_utc,assignment_generation)
        values(X'01010101010101010101010101010101','TERM01L7','archive','sealed','now',17);",
    )
    .unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    for _ in 0..2 {
        let error = CatalogIndex::open(&path).unwrap_err();
        assert!(error
            .to_string()
            .contains(remanence_state::authority_layout::RESET_REQUIRED));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn failed_assignment_replay_rolls_back_event_cursor_and_projection() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.sqlite");
    let mut index = CatalogIndex::open(&path).unwrap();
    provision(&mut index, 1, "CAP001L7");
    index.upsert_tape_pool_projection(pool()).unwrap();
    let db = Connection::open(&path).unwrap();
    db.execute_batch("create trigger crash before insert on tape_audit_cursors begin select raise(abort,'crash cut'); end;").unwrap();
    let event = assignment(1, Some("archive"));
    assert!(index.project_audit_record(&event).is_err());
    assert_eq!(
        index
            .get_tape(&[1; 16])
            .unwrap()
            .unwrap()
            .assignment_generation,
        0
    );
    assert_eq!(
        db.query_row("select count(*) from tape_pool_assignments", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.execute_batch("drop trigger crash").unwrap();
    index.project_audit_record(&event).unwrap();
    index.project_audit_record(&event).unwrap();
    assert_eq!(
        index
            .get_tape(&[1; 16])
            .unwrap()
            .unwrap()
            .assignment_generation,
        1
    );
}

/// Startup may reconcile a current rule binding before its first audit replay.
#[test]
fn rule_clear_or_change_survives_repeated_replay_and_rebuild() {
    use ciborium::value::Value;
    for target in [None, Some("second")] {
        for rebuild_first in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("legacy.sqlite");
            let mut index = CatalogIndex::open(&path).unwrap();
            provision(&mut index, 1, "AUTO01L7");
            let events = [
                audit(
                    1,
                    remanence_state::AuditEvent::TapeProvisioned,
                    1,
                    &[
                        ("voltag", Value::Text("AUTO01L7".into())),
                        ("pool_id", Value::Text("archive".into())),
                    ],
                ),
                audit(
                    2,
                    remanence_state::AuditEvent::TapePoolAssigned,
                    1,
                    &[("pool_id", Value::Text("archive".into()))],
                ),
            ];
            index.replay_audit_records(&events).unwrap();
            let db = Connection::open(&path).unwrap();
            let pools = [
                pool(),
                TapePoolProjectionInput {
                    pool_id: "second".into(),
                    ..pool()
                },
            ];
            let rules: Vec<_> = target
                .map(|pool| TapePoolRuleConfig {
                    prefix: "AUTO".into(),
                    pool_id: pool.into(),
                })
                .into_iter()
                .collect();
            index
                .reconcile_tape_pool_projection_from_rules(&pools, &rules)
                .unwrap();
            let expected = index
                .get_tape_assignment_snapshot(&[1; 16])
                .unwrap()
                .unwrap();
            assert_eq!(expected.pool_id.as_deref(), target);
            let expected_count: i64 = db
                .query_row("select count(*) from tape_pool_assignments", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(expected_count, if target.is_some() { 3 } else { 2 });
            drop(index);
            let mut index = CatalogIndex::open(&path).unwrap();
            for _ in 0..2 {
                if rebuild_first {
                    index
                        .rebuild_from_authoritative_sources(&events, &[])
                        .unwrap();
                }
                index.replay_audit_records(&events).unwrap();
                index
                    .rebuild_from_authoritative_sources(&events, &[])
                    .unwrap();
                assert_eq!(
                    index
                        .get_tape_assignment_snapshot(&[1; 16])
                        .unwrap()
                        .unwrap(),
                    expected
                );
                assert_eq!(
                    db.query_row("select count(*) from tape_pool_assignments", [], |r| r
                        .get::<_, i64>(0))
                        .unwrap(),
                    expected_count
                );
            }
            // Explicit operator null remains new intent, with or without a generation.
            let mut clear = assignment(3, None);
            clear.detail.insert(
                "assignment_generation".into(),
                Value::Integer((expected.assignment_generation + 1).into()),
            );
            if rebuild_first {
                clear.detail.remove("assignment_generation");
            }
            index.project_audit_record(&clear).unwrap();
            let final_row = index.get_tape(&[1; 16]).unwrap().unwrap();
            assert_eq!(final_row.pool_id, None);
            assert_eq!(
                final_row.assignment_generation,
                expected.assignment_generation + 1
            );
        }
    }
}

/// Both existing-row provisioning branches must establish label/source together.
#[test]
fn rebind_then_provision_released_uuid_updates_label_and_provenance_atomically() {
    for change_geometry in [false, true] {
        let mut index = CatalogIndex::open(":memory:").unwrap();
        provision(&mut index, 1, "PEN001L9");
        provision(&mut index, 2, "PEN002L9");
        index.project_audit_record(&rebind_record(1, 1)).unwrap();
        let released = index.get_tape(&[1; 16]).unwrap().unwrap();
        assert_eq!((released.voltag, released.voltag_source), (None, None));
        let observation = MediaObservation {
            generation: LtoGen::Lto7,
            source: GenerationSource::Measured,
        };
        index
            .provision_tape_with_media(
                ProvisionTapeInput {
                    tape_uuid: [1; 16],
                    voltag: "replacement".into(),
                    block_size: if change_geometry { 524288 } else { 262144 },
                    parity: ParityConfig::None,
                    force: false,
                },
                LabelProvenance::Operator,
                Some(observation),
            )
            .unwrap();
        let released = index.get_tape(&[1; 16]).unwrap().unwrap();
        assert_eq!(released.voltag.as_deref(), Some("replacement"));
        assert_eq!(released.voltag_source, Some(LabelProvenance::Operator));
        assert_eq!(
            tape_generation(&released),
            Some((LtoGen::Lto7, GenerationSource::Measured))
        );
        assert_eq!(
            index.get_tape(&[2; 16]).unwrap().unwrap().voltag.as_deref(),
            Some("PEN001L9")
        );
    }
}

/// Audit generations are transition identities, not audit sequence numbers.
#[test]
fn assignment_replay_reconstructs_recorded_generations_across_unaudited_changes() {
    use ciborium::value::Value;
    let mut index = CatalogIndex::open(":memory:").unwrap();
    provision(&mut index, 1, "AUTO01L7");
    index.upsert_tape_pool_projection(pool()).unwrap();
    let mut assigned = assignment(20, Some("archive"));
    assigned
        .detail
        .insert("assignment_generation".into(), Value::Integer(5.into()));
    let mut cleared = assignment(30, None);
    cleared
        .detail
        .insert("assignment_generation".into(), Value::Integer(9.into()));
    let events = [assigned, cleared];
    index.replay_audit_records(&events).unwrap();
    let expected = index
        .get_tape_assignment_snapshot(&[1; 16])
        .unwrap()
        .unwrap();
    assert_eq!(expected.pool_id, None);
    assert_eq!(expected.assignment_generation, 9);
    for _ in 0..2 {
        index.replay_audit_records(&events).unwrap();
        index
            .rebuild_from_authoritative_sources(&events, &[])
            .unwrap();
        assert_eq!(
            index
                .get_tape_assignment_snapshot(&[1; 16])
                .unwrap()
                .unwrap(),
            expected
        );
    }
}
