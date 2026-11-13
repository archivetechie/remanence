//! Baseline decision readings across complete SQLite loss. All writes use the
//! public state facade; only the disposable database, WAL and SHM are removed.
//! Reopening and startup_replay exercise today's actual restart/rebuild path.

use ciborium::value::Value;
use remanence_library::{
    motion::{LoadConfirmation, MotionAuthority, MotionCommand, MotionContext},
    outcome::CompletionEvidence,
};
use remanence_parity::ParityConfig;
use remanence_state::library_log as lib;
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
                identity_source: "DvcidAndInquiry".into(),
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

/// The first landing keeps geometry, pool and label from Reprovision without a native tape log.
/// D-C3b-S-1: the library record alone rebuilds the provisioned tape.
#[test]
fn current_empty_rebuild_logless_provision_geometry_pool_kept() {
    let mut s = State::new();
    s.provision(1, "B1001L9");
    let tape = s.index().get_tape(&[1; 16]).unwrap().unwrap();
    assert_eq!(tape.block_size, Some(262144));
    assert_eq!(tape.pool_id.as_deref(), Some("archive"));
    assert_eq!(
        fs::read_dir(s.state().paths().journal_dir.join("tapes"))
            .unwrap()
            .count(),
        0
    );
    let before = (tape.block_size, tape.pool_id, tape.voltag);
    assert_eq!(
        before,
        (Some(262144), Some("archive".into()), Some("B1001L9".into()))
    );
    s.empty_rebuild();
    let tape = s.index().get_tape(&[1; 16]).unwrap().unwrap();
    assert_eq!((tape.block_size, tape.pool_id, tape.voltag), before);
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

/// The first landing keeps the readiness owner and LOAD budget from typed observations and intents.
/// D-C3b-S-1: failed LOAD completions close motion without closing the readiness owner.
#[test]
fn current_empty_rebuild_readiness_operation_and_load_budget_kept() {
    let mut s = State::new();
    s.drive();
    let endpoint = DriveKey::new("B1-LIBRARY", 1);
    let operation_id = s
        .index()
        .begin_typed_media_readiness_operation(MediaReadinessOperationInput {
            // The unowned producer replaces this placeholder with its minted identity.
            operation_id: Uuid::nil(),
            run_id: None,
            endpoint: endpoint.clone(),
            changer_sg: None,
            drive_sg: None,
            drive_serial: Some("B1-DRIVE".into()),
            barcode: Some("B1001L9".into()),
            source_slot: None,
            media_generation: None,
            media_family: None,
            phase: "readiness_poll".into(),
            state: "planned".into(),
            dirty_scope: Some("drive+tape".into()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .unwrap()
        .operation_id;
    let operation_id = Uuid::parse_str(&operation_id).unwrap();
    let runtime = s.state().library_runtime();
    let context = MotionContext {
        parent_operation: Some(*operation_id.as_bytes()),
        load_confirmation: LoadConfirmation::Readiness,
        ..Default::default()
    };
    let failure = CompletionEvidence {
        failed: true,
        scsi_status: Some(2),
        opcode: Some(0x1b),
        ..CompletionEvidence::success()
    };
    for _ in 0..2 {
        let ticket = runtime
            .intent(&MotionCommand::Load(endpoint.clone()), &context)
            .unwrap();
        runtime.failed(ticket, &failure).unwrap();
    }
    drop(runtime);
    // The load count is read through the runtime from the library log's fold (its admitted LOAD
    // intents), so it is restored by reopening the log, not by the SQL rebuild.
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

/// The first landing keeps retirement over an older native seal, even with an empty projection.
/// This pins no resurrection without asserting a duplicate TapeSealed audit.
/// D-C3b-S-1: Reprovision supplies identity authority before the native seal and typed retirement.
#[test]
fn current_empty_rebuild_retired_sealed_identity_kept() {
    let mut s = State::new();
    s.provision(0xa3, "B1003L9");
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

/// D-C3b-S2-1: the first landing keeps the calibration harvest's wrap map as an SQL cache, never authority.
/// This seeds the harvest's record_harvest_success then upsert_wrap_map writes; an empty rebuild
/// evicts the map, the calibration control store (outside SQLite) survives with its write epoch,
/// and the map is not servable until the next harvest. L2 records the harvest through WrapMapCached.
#[test]
fn current_empty_rebuild_cached_wrap_map_evicted_not_servable() {
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

/// The first landing keeps each nonterminal cleaning phase holding its drive via CleaningRecorded.
/// D-C3b-S-1: Begin and the selected phase rebuild the active run after SQLite loss.
#[test]
fn current_empty_rebuild_clean_run_nonterminal_states_kept() {
    for (phase, expected) in [
        (lib::CleanPhase::Fencing, "fencing"),
        (lib::CleanPhase::Selecting, "selecting"),
        (lib::CleanPhase::MovingIn, "moving-in"),
        (lib::CleanPhase::Cleaning, "cleaning"),
        (lib::CleanPhase::MovingBack, "moving-back"),
        (lib::CleanPhase::Verifying, "verifying"),
    ] {
        let mut s = State::new();
        let drive = s.drive();
        let runtime = s.state().library_runtime();
        let home_slot = 0x0400;
        let cartridge = runtime
            .register_cleaning_cartridge("CLN001L9", home_slot)
            .unwrap();
        let run = Uuid::new_v4().to_string();
        runtime
            .record_cleaning_action(
                &run,
                Some(drive.clone().try_into().unwrap()),
                None,
                lib::CleaningAction::Begin(lib::Begin {
                    trigger: lib::CleanTrigger::Manual,
                }),
                lib::AuditActor::System,
            )
            .unwrap();
        let selecting = phase == lib::CleanPhase::MovingIn;
        runtime
            .record_cleaning_action(
                &run,
                None,
                selecting.then_some(cartridge),
                lib::CleaningAction::Advance(lib::Advance {
                    phase,
                    home_slot: selecting.then_some(home_slot),
                }),
                lib::AuditActor::System,
            )
            .unwrap();
        drop(runtime);
        assert_eq!(
            s.index()
                .get_active_clean_run_by_drive(&drive)
                .unwrap()
                .unwrap()
                .phase,
            expected
        );
        s.empty_rebuild();
        assert_eq!(
            s.index()
                .get_active_clean_run_by_drive(&drive)
                .unwrap()
                .unwrap()
                .phase,
            expected
        );
    }
}

/// The first landing keeps a cleaning drive fence from FenceRaised and the typed drive identity.
/// D-C3b-S-1: rebuilding and reobserving the same drive both retain its fence and UUID.
#[test]
fn current_empty_rebuild_drive_fence_kept() {
    let mut s = State::new();
    let drive = s.drive();
    s.state()
        .library_runtime()
        .append(|a, v| {
            a.fence_raised(
                lib::FenceRaised {
                    detail: lib::Bounded(vec![]),
                    kind: lib::FenceKind::DriveFault,
                    scope: lib::FenceScope::Drive(lib::DriveKey::Bay(lib::Bay {
                        library_serial: "B1-LIBRARY".into(),
                        bay: 1,
                    })),
                    cause: lib::FenceCause::Cleaning,
                    evidence: None,
                    at: None,
                    operation: None,
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    assert!(s.index().get_drive_by_uuid(&drive).unwrap().unwrap().fenced);
    s.empty_rebuild();
    let rebuilt = s.index().get_drive_by_uuid(&drive).unwrap().unwrap();
    assert_eq!((rebuilt.drive_uuid, rebuilt.fenced), (drive.clone(), true));
    let observed = s.drive();
    assert_eq!(observed, drive);
    assert!(
        s.index()
            .get_drive_by_uuid(&observed)
            .unwrap()
            .unwrap()
            .fenced
    );
}

/// D-C3b-S2-1: the typed alarm producer preserves the acknowledgement across complete SQLite loss.
#[test]
fn current_empty_rebuild_alarm_acknowledgement_kept() {
    let mut s = State::new();
    let runtime = s.state().library_runtime();
    assert!(runtime
        .observe_alarm(
            "no-cln-cart:B1-LIBRARY",
            Some(lib::AlarmRaise {
                kind: lib::AlarmKind::NoClnCart,
                severity: lib::Severity::Critical,
                reason: "no cleaning cartridge in the library".into(),
            }),
            "",
        )
        .expect("raise the alarm"));
    assert!(runtime
        .acknowledge_alarm(
            "no-cln-cart:B1-LIBRARY",
            lib::AuditActor::Service("operator".into()),
        )
        .expect("acknowledge the alarm"));
    drop(runtime);
    let alarm = s
        .index()
        .get_alarm("no-cln-cart:B1-LIBRARY")
        .unwrap()
        .unwrap();
    assert_eq!(alarm.state, "acked");
    assert_eq!(alarm.acked_by.as_deref(), Some("service:operator"));
    assert!(alarm.acked_at_utc.is_some());
    let before = (
        alarm.kind,
        alarm.severity,
        alarm.state,
        alarm.acked_by,
        alarm.acked_at_utc,
    );
    s.empty_rebuild();
    let alarm = s
        .index()
        .get_alarm("no-cln-cart:B1-LIBRARY")
        .unwrap()
        .unwrap();
    assert_eq!(alarm.state, "acked");
    assert_eq!(alarm.acked_by.as_deref(), Some("service:operator"));
    assert!(alarm.acked_at_utc.is_some());
    assert_eq!(
        (
            alarm.kind,
            alarm.severity,
            alarm.state,
            alarm.acked_by,
            alarm.acked_at_utc,
        ),
        before
    );
}

/// D-C3b-S2-1: this characterises SQL-only CatalogIndex::register_idempotency_request,
/// which no production code calls. The reservation writes only an idempotency scope (no
/// operation row and no OperationRecorded), so a rebuild restores no scope; the typed path is
/// current_empty_rebuild_idempotency_and_operation_kept.
#[test]
fn current_empty_rebuild_sql_only_idempotency_reservation_not_restored() {
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

/// The first landing keeps a finished request and its idempotency scope from OperationRecorded.
/// D-C3b-S-1: the runtime mints the accepted identity and records its terminal event.
/// A keyed verification is a synthetic pairing: production accepts a verification unkeyed, while
/// other kinds (the tape close-out, manual finalization) are keyed; the projection logic does not
/// branch on the kind.
#[test]
fn current_empty_rebuild_idempotency_and_operation_kept() {
    let mut s = State::new();
    let key = Uuid::new_v4();
    let runtime = s.state().library_runtime();
    let (operation, _) = runtime
        .accept_operation(
            lib::OperationKind::TapeVerification,
            lib::Accepted {
                actor_fingerprint: "operator".into(),
                idempotency_key: Some(*key.as_bytes()),
                request_fingerprint: [7; 32],
                subject: Some(Uuid::from_bytes([1; 16]).to_string().as_str().into()),
            },
            lib::AuditActor::System,
        )
        .unwrap();
    runtime
        .record_operation_event(
            operation,
            lib::OperationKind::TapeVerification,
            lib::OperationEvent::Started(lib::Unit),
            lib::AuditActor::System,
        )
        .unwrap();
    runtime
        .record_operation_event(
            operation,
            lib::OperationKind::TapeVerification,
            lib::OperationEvent::Finished(lib::Response {
                response_fingerprint: None,
            }),
            lib::AuditActor::System,
        )
        .unwrap();
    drop(runtime);
    let operation = Uuid::from_bytes(operation);
    let read = |s: &mut State| {
        let scope = s
            .index()
            .idempotency_scope_record("operator", "tape_verification", key)
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

/// Drive stewardship is deferred to L2 (DriveStewardshipDeferred): rem drive annotate is refused
/// before any drive record, proved by remanence-api stewardship_typed_annotate_is_deferred_before_any_write;
/// this body characterises main's lost SQL-only note and drive row, while the typed row now survives,
/// and is rewritten to kept with L2's stewardship order (D-C3b-S-1).
#[test]
#[ignore = "drive stewardship deferred to L2 (DriveStewardshipDeferred): rem drive annotate is refused before any drive record, proved by remanence-api stewardship_typed_annotate_is_deferred_before_any_write; this body characterises main (the SQL-only note and the drive row lost; the typed drive row now survives a rebuild) and is rewritten to kept with L2's stewardship order; D-C3b-S-1"]
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
