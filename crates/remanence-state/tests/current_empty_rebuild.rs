//! Baseline decision readings across complete SQLite loss. All writes use the
//! public state facade; only the disposable database, WAL and SHM are removed.
//! Reopening and startup_replay exercise today's actual restart/rebuild path.

use ciborium::value::Value;
use remanence_parity::ParityConfig;
use remanence_state::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

struct State {
    _root: tempfile::TempDir,
    config: PathBuf,
    handle: Option<StateHandle>,
}
impl State {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        fs::write(
            &config,
            format!(
                r#"
[daemon]
default_idle_timeout_seconds = 1800
read_only = false
state_dir = "{0}"
[[libraries]]
serial = "B1-LIBRARY"
[[tape_pools]]
id = "archive"
[[tape_pool_rules]]
prefix = "B1"
pool_id = "archive"
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
                root.path().display()
            ),
        )
        .unwrap();
        let handle = StateHandle::open_from_config_file(&config).unwrap();
        Self {
            _root: root,
            config,
            handle: Some(handle),
        }
    }
    fn state(&mut self) -> &mut StateHandle {
        self.handle.as_mut().unwrap()
    }
    fn index(&mut self) -> CatalogIndex {
        self.state().write_handle()
    }
    fn empty_rebuild(&mut self) {
        let path = self.state().paths().sqlite_path.clone();
        drop(self.handle.take());
        fs::remove_file(&path).unwrap();
        for suffix in ["-wal", "-shm"] {
            let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
            match fs::remove_file(sidecar) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => panic!("remove SQLite sidecar: {e}"),
            }
        }
        self.handle = Some(StateHandle::open_from_config_file(&self.config).unwrap());
        self.state().startup_replay().unwrap();
    }
    fn provision(&mut self, id: u8, label: &str) {
        self.state()
            .provision_tape(ProvisionTapeInput {
                tape_uuid: [id; 16],
                voltag: label.into(),
                block_size: 262144,
                parity: ParityConfig::None,
                force: false,
            })
            .unwrap();
    }
    fn audit(
        &mut self,
        event: AuditEvent,
        subject: &str,
        id: Option<String>,
        detail: BTreeMap<String, Value>,
    ) {
        FileAuditLog::open(&self.state().paths().audit_dir, true)
            .unwrap()
            .append(AuditEventRecord {
                actor: AuditActor::System,
                source_layer: SourceLayer::Layer4,
                operation_id: None,
                session_id: None,
                idempotency_key: None,
                event,
                subject: AuditSubject {
                    kind: subject.into(),
                    id,
                },
                detail,
            })
            .unwrap();
        self.state().replay_audit_projection().unwrap();
    }
    // Minimal projector fixture, deliberately independent of the CLI crate to
    // avoid a state -> CLI -> state dependency cycle. The real wire detail is
    // covered by remanence-cli's
    // current_empty_rebuild_cli_logless_provision_geometry_pool_kept, which runs
    // `rem tape init` dispatch with fixture transports before deleting SQLite.
    fn audited_provision(&mut self, id: u8, label: &str) {
        self.provision(id, label);
        self.audit(
            AuditEvent::TapeProvisioned,
            "tape",
            Some(Uuid::from_bytes([id; 16]).to_string()),
            BTreeMap::from([
                ("voltag".into(), Value::Text(label.into())),
                ("block_size".into(), Value::Integer(262144.into())),
                ("pool_id".into(), Value::Text("archive".into())),
            ]),
        );
    }
    fn drive(&mut self) -> Vec<u8> {
        self.index()
            .observe_drive(DriveObservationInput {
                serial: "B1-DRIVE".into(),
                identity_source: "vpd".into(),
                vendor: None,
                product: None,
                firmware_rev: None,
                managed: "rem".into(),
                library_serial: Some("B1-LIBRARY".into()),
                element_address: Some(1),
                observed_at_utc: None,
            })
            .unwrap()
            .drive_uuid
    }
}

#[test]
fn current_empty_rebuild_label_after_rebind_kept() {
    let mut s = State::new();
    s.audited_provision(1, "B1001L9");
    s.audited_provision(2, "B1002L9");
    s.state()
        .rebind_tape_label([2; 16], "B1001L9", [1; 16])
        .unwrap();
    let read = |s: &mut State| {
        (
            s.index()
                .get_tape_by_voltag("B1001L9")
                .unwrap()
                .unwrap()
                .tape_uuid,
            s.index().get_tape(&[1; 16]).unwrap().unwrap().voltag,
        )
    };
    let before = read(&mut s);
    assert_eq!(before, (vec![2; 16], None));
    s.empty_rebuild();
    assert_eq!(read(&mut s), before);
}

/// B4b flips this to kept.
#[test]
fn current_empty_rebuild_logless_provision_geometry_pool_lost() {
    let mut s = State::new();
    s.provision(1, "B1001L9");
    s.index()
        .project_tape_pool_membership([1; 16], "archive")
        .unwrap();
    let tape = s.index().get_tape(&[1; 16]).unwrap().unwrap();
    assert_eq!(tape.block_size, Some(262144));
    assert_eq!(tape.pool_id.as_deref(), Some("archive"));
    assert_eq!(
        fs::read_dir(s.state().paths().journal_dir.join("tapes"))
            .unwrap()
            .count(),
        0
    );
    s.empty_rebuild();
    assert!(s.index().get_tape(&[1; 16]).unwrap().is_none());
}

#[test]
fn current_empty_rebuild_audited_logless_provision_geometry_pool_kept() {
    let mut s = State::new();
    s.audited_provision(1, "B1001L9");
    let read = |s: &mut State| {
        let tape = s.index().get_tape(&[1; 16]).unwrap().unwrap();
        (tape.block_size, tape.pool_id, tape.voltag)
    };
    let before = read(&mut s);
    assert_eq!(
        before,
        (Some(262144), Some("archive".into()), Some("B1001L9".into()))
    );
    s.empty_rebuild();
    assert_eq!(read(&mut s), before);
}

/// B4b flips this to kept.
#[test]
fn current_empty_rebuild_readiness_operation_and_load_budget_lost() {
    let mut s = State::new();
    let operation_id = Uuid::new_v4();
    let endpoint = DriveKey::new("B1-LIBRARY", 1);
    s.index()
        .record_media_readiness_operation(MediaReadinessOperationInput {
            operation_id,
            run_id: None,
            endpoint: endpoint.clone(),
            changer_sg: None,
            drive_sg: None,
            drive_serial: Some("B1-DRIVE".into()),
            barcode: Some("B1001L9".into()),
            source_slot: None,
            media_generation: Some(9),
            media_family: None,
            phase: "load".into(),
            state: "planned".into(),
            dirty_scope: Some("drive+tape".into()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .unwrap();
    for _ in 0..2 {
        s.index()
            .record_media_readiness_transition(MediaReadinessTransitionInput {
                completion_evidence: None,
                operation_id,
                phase: Some("load".into()),
                state: "pre_ready_loading".into(),
                dirty_scope: Some("drive+tape".into()),
                last_cdb_opcode: Some(0x1b),
                last_sense_raw: None,
                last_sense_key: None,
                last_asc: None,
                last_ascq: None,
                last_host_status: None,
                last_driver_status: None,
                target_status: None,
                transport_class: None,
                cancel_source: None,
                signal: None,
                evidence_path: None,
                last_error_json: None,
                quarantine_id: None,
            })
            .unwrap();
    }
    assert_eq!(
        s.index()
            .media_readiness_load_attempts_since_ready(operation_id)
            .unwrap(),
        2
    );
    assert_eq!(
        s.index()
            .media_readiness_operation(operation_id)
            .unwrap()
            .unwrap()
            .state,
        "pre_ready_loading"
    );
    assert!(!s
        .index()
        .media_readiness_admission_conflicts(&endpoint, None, None, false)
        .unwrap()
        .is_empty());
    s.empty_rebuild();
    assert_eq!(
        s.index()
            .media_readiness_load_attempts_since_ready(operation_id)
            .unwrap(),
        0
    );
    assert!(s
        .index()
        .media_readiness_operation(operation_id)
        .unwrap()
        .is_none());
    assert!(s
        .index()
        .media_readiness_admission_conflicts(&endpoint, None, None, false)
        .unwrap()
        .is_empty());
}

/// Ruling 32-L1: tail repair is refused before any effect, so a damaged tail is never cut and the
/// projection floor stays where the log put it. (A repair that lowers the floor, and the empty
/// rebuild that must then keep it, return with the library log's repair record.)
#[test]
fn current_empty_rebuild_repair_is_refused_and_leaves_the_projection_floor() {
    let mut s = State::new();
    let id = [0xa1; 16];
    let path = s
        .state()
        .paths()
        .journal_dir
        .join("tapes")
        .join(format!("{}.remtape", Uuid::from_bytes(id).simple()));
    fs::write(
        &path,
        include_bytes!("../../../fixtures/tape-log/parity-off-two-objects.remtape"),
    )
    .unwrap();
    s.state().startup_replay().unwrap();
    let floor = s.index().projected_through_sequence(id).unwrap();
    assert!(floor > 1);
    let mut bytes = fs::read(&path).unwrap();
    let mut cursor = std::io::Cursor::new(&bytes);
    tape_log::store::Header::decode(&mut cursor, id).unwrap();
    let offset = cursor.position();
    let mut decoder = tape_log::codec::Decoder::at(cursor, 1, offset);
    let next = decoder.next_frame().unwrap().unwrap().end as usize;
    bytes[next + tape_log::PREFIX_LEN] ^= 0x80;
    fs::write(&path, &bytes).unwrap();
    let error = s.state().repair_tape_log_tail(id, 1, true).unwrap_err();
    assert!(error.to_string().contains("TailRepairDeferred"), "{error}");
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "the damaged log is untouched"
    );
    assert!(!path.with_extension("repair-required").exists());
    assert_eq!(s.index().projected_through_sequence(id).unwrap(), floor);
}

/// Retirement wins over an older native seal, even with an empty projection.
/// This pins no resurrection without asserting a duplicate TapeSealed audit.
#[test]
fn current_empty_rebuild_retired_sealed_identity_kept() {
    let mut s = State::new();
    let id = [0xa3; 16];
    let path = s
        .state()
        .paths()
        .journal_dir
        .join("tapes")
        .join(format!("{}.remtape", Uuid::from_bytes(id).simple()));
    fs::write(
        path,
        include_bytes!("../../../fixtures/tape-log/sealed-terminal.remtape"),
    )
    .unwrap();
    s.state().startup_replay().unwrap();
    assert_eq!(s.index().get_tape(&id).unwrap().unwrap().state, "sealed");
    s.state()
        .retire_tape(RetireTapeInput {
            tape_uuid: id,
            reason: "retired after seal".into(),
        })
        .unwrap();
    assert_eq!(s.index().get_tape(&id).unwrap().unwrap().state, "retired");
    s.empty_rebuild();
    assert_eq!(s.index().get_tape(&id).unwrap().unwrap().state, "retired");
}

/// B4b flips this to kept. The calibration directory is retained, not deleted;
/// today's startup itself invalidates the surviving control row.
#[test]
fn current_empty_rebuild_cached_wrap_map_lost() {
    let mut s = State::new();
    s.audited_provision(1, "B1001L9");
    let control = s.state().calibration_control();
    let epoch = control.row([1; 16]).write_epoch;
    control.record_harvest_success([1; 16], epoch).unwrap();
    let generation = control.last_generation();
    assert!(control.is_map_servable([1; 16], epoch));
    s.index()
        .upsert_wrap_map(&WrapMapCacheRecord {
            tape_uuid: [1; 16],
            descriptors: vec![StoredWrapDescriptor {
                partition: 0,
                wrap_number: 0,
                end_loi: 42,
            }],
            mapped_extent_lba: 42,
            write_epoch: epoch,
            calibration_generation: generation,
            harvested_at_utc: "2026-11-03T00:00:00Z".into(),
        })
        .unwrap();
    assert_eq!(
        s.index()
            .get_wrap_map(&[1; 16])
            .unwrap()
            .unwrap()
            .mapped_extent_lba,
        42
    );
    s.empty_rebuild();
    assert!(s.index().get_wrap_map(&[1; 16]).unwrap().is_none());
    assert_eq!(
        s.state().calibration_control().row([1; 16]).write_epoch,
        epoch
    );
    assert!(!s
        .state()
        .calibration_control()
        .is_map_servable([1; 16], epoch));
}

/// B4b flips this to kept. Each production non-terminal phase holds the drive.
#[test]
fn current_empty_rebuild_clean_run_nonterminal_states_lost() {
    for phase in [
        "fencing",
        "selecting",
        "moving-in",
        "cleaning",
        "moving-back",
        "verifying",
    ] {
        let mut s = State::new();
        let drive = s.drive();
        let run = s
            .index()
            .begin_clean_run(&drive, "B1-LIBRARY", "manual", None)
            .unwrap();
        s.index()
            .advance_clean_run(&run.run_id, phase, None)
            .unwrap();
        assert_eq!(
            s.index()
                .get_active_clean_run_by_drive(&drive)
                .unwrap()
                .unwrap()
                .phase,
            phase
        );
        s.empty_rebuild();
        assert!(
            s.index()
                .get_active_clean_run_by_drive(&drive)
                .unwrap()
                .is_none(),
            "{phase}"
        );
    }
}

/// B4b flips this to kept.
#[test]
fn current_empty_rebuild_drive_fence_lost() {
    let mut s = State::new();
    let drive = s.drive();
    s.index().set_drive_fenced(&drive, true).unwrap();
    assert!(s.index().get_drive_by_uuid(&drive).unwrap().unwrap().fenced);
    s.empty_rebuild();
    assert!(s.index().get_drive_by_uuid(&drive).unwrap().is_none());
    let observed = s.drive();
    assert!(
        !s.index()
            .get_drive_by_uuid(&observed)
            .unwrap()
            .unwrap()
            .fenced
    );
}

/// B4b flips this to kept.
#[test]
fn current_empty_rebuild_alarm_acknowledgement_lost() {
    let mut s = State::new();
    s.index()
        .raise_alarm("drive:B1", "drive-fault", "warning", None)
        .unwrap();
    s.index().ack_alarm("drive:B1", "operator").unwrap();
    assert_eq!(
        s.index().get_alarm("drive:B1").unwrap().unwrap().state,
        "acked"
    );
    s.empty_rebuild();
    assert!(s.index().get_alarm("drive:B1").unwrap().is_none());
}

/// B4b flips this to kept. The SQL reservation alone has no replay authority.
#[test]
fn current_empty_rebuild_idempotency_registration_lost() {
    let mut s = State::new();
    let key = Uuid::new_v4();
    let operation = Uuid::new_v4();
    s.index()
        .register_idempotency_request("operator", "verify_tape", key, [7; 32], operation, None)
        .unwrap();
    assert_eq!(
        s.index()
            .idempotency_scope_record("operator", "verify_tape", key)
            .unwrap()
            .unwrap()
            .operation_id,
        operation
    );
    s.empty_rebuild();
    assert!(s
        .index()
        .idempotency_scope_record("operator", "verify_tape", key)
        .unwrap()
        .is_none());
    assert!(s
        .index()
        .get_operation(&operation.to_string())
        .unwrap()
        .is_none());
}

#[test]
fn current_empty_rebuild_audited_idempotency_and_operation_kept() {
    let mut s = State::new();
    let key = Uuid::new_v4();
    let operation = Uuid::new_v4();
    for event in [AuditEvent::RequestReceived, AuditEvent::OperationFinished] {
        FileAuditLog::open(&s.state().paths().audit_dir, true)
            .unwrap()
            .append(AuditEventRecord {
                actor: AuditActor::System,
                source_layer: SourceLayer::Layer5,
                operation_id: Some(operation),
                session_id: None,
                idempotency_key: Some(key),
                event,
                subject: AuditSubject {
                    kind: "tape".into(),
                    id: Some(Uuid::from_bytes([1; 16]).to_string()),
                },
                detail: BTreeMap::from([
                    ("operation_kind".into(), Value::Text("verify_tape".into())),
                    ("actor_fingerprint".into(), Value::Text("operator".into())),
                    ("request_fingerprint".into(), Value::Bytes(vec![7; 32])),
                ]),
            })
            .unwrap();
    }
    s.state().replay_audit_projection().unwrap();
    let read = |s: &mut State| {
        let scope = s
            .index()
            .idempotency_scope_record("operator", "verify_tape", key)
            .unwrap()
            .unwrap();
        (
            scope.operation_id,
            scope.terminal_state,
            s.index()
                .get_operation(&operation.to_string())
                .unwrap()
                .unwrap()
                .state,
        )
    };
    let before = read(&mut s);
    assert_eq!(
        before,
        (operation, Some("finished".into()), "finished".into())
    );
    s.empty_rebuild();
    assert_eq!(read(&mut s), before);
}

/// B4b flips this to kept. Operator annotations are another SQL-only fact.
#[test]
fn current_empty_rebuild_drive_annotation_lost() {
    let mut s = State::new();
    let drive = s.drive();
    s.index()
        .annotate_drive(DriveAnnotationInput {
            drive_uuid: drive.clone(),
            note: Some("use for recovery".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(s
        .index()
        .get_drive_by_uuid(&drive)
        .unwrap()
        .unwrap()
        .notes
        .unwrap()
        .contains("use for recovery"));
    s.empty_rebuild();
    assert!(s.index().get_drive_by_uuid(&drive).unwrap().is_none());
}
