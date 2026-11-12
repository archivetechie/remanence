//! N-2 integration evidence on typed roots: startup retains native obligations,
//! and host retries recover tape-local catalog failures without changing status.
use super::replay_checkpoint_journal_projections_with_audit;
use remanence_state::authority::{TapeAuthorityView, TapeAuthorityWriter};
use remanence_state::{
    CommitFixture, NativeObjectInstanceProjectionInput, NativeObjectProjectionInput,
    ObjectProjectionFixture, ObjectRecoveryFixture, ObjectRecoveryFixtureRepresentation,
    StateHandle, StatePaths, TapeAuthority,
};
use std::io::{Read, Seek};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const A: [u8; 16] = [0x71; 16];
const B: [u8; 16] = [0x72; 16];
const BLOCK: u32 = 262144;
const INJECTED: &str = "injected N-2 catalog failure";

struct Fixture {
    state: Option<StateHandle>,
    paths: StatePaths,
    config: remanence_state::RemConfig,
    _root: tempfile::TempDir,
}

fn scheme() -> remanence_parity::ParityScheme {
    remanence_parity::ParityScheme {
        id: remanence_parity::SchemeId::new_static(remanence_parity::SCHEME_ID_RS_CAUCHY_GF256_V1),
        data_blocks_per_stripe: 8,
        parity_blocks_per_stripe: 2,
        stripes_per_neighborhood: 1,
    }
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let text = format!(
            r#"[daemon]
state_dir = "{0}/state"
default_idle_timeout_seconds = 1800
[journal]
dir = "{0}/native"
require_trusted_volume = false
[audit]
dir = "{0}/library"
fsync = true
[index]
sqlite_path = "{0}/index.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
"#,
            root.path().display()
        );
        let config = remanence_state::parse_config_toml(&text).unwrap();
        let paths = StatePaths::from_config(root.path().join("config.toml"), &config);
        let state = StateHandle::open_with_config(paths.clone(), config.clone()).unwrap();
        Self {
            state: Some(state),
            paths,
            config,
            _root: root,
        }
    }

    fn state(&self) -> &StateHandle {
        self.state.as_ref().unwrap()
    }

    fn native(
        &mut self,
        tape_uuid: [u8; 16],
        owes_work: bool,
    ) -> remanence_state::tape_log::authority::AuthoritySnapshot {
        self.state
            .as_mut()
            .unwrap()
            .provision_tape_with_media(
                remanence_state::ProvisionTapeInput {
                    tape_uuid,
                    voltag: if tape_uuid == A {
                        "N2A001L9"
                    } else {
                        "N2B001L9"
                    }
                    .into(),
                    block_size: BLOCK,
                    parity: remanence_parity::ParityConfig::Scheme(scheme()),
                    force: false,
                },
                remanence_state::LabelProvenance::Operator,
                remanence_state::MediaObservation::label_hint("N2A001L9"),
            )
            .unwrap();
        let authority = TapeAuthority::open(
            self.paths.journal_dir.join("tapes"),
            tape_uuid,
            &self.paths.sqlite_path,
        )
        .unwrap();
        let mut owner = authority.acquire_exclusive().unwrap();
        if owes_work {
            let mut record = checkpoint(tape_uuid);
            record.scheme = Some(scheme());
            record.objects[0].fresh_tape = false;
            record.objects[0].instance.first_parity_data_ordinal = Some(0);
            record.objects[0].instance.protected_until_ordinal = Some(0);
            record.object_tape_file_bundles = vec![remanence_parity::FileRowsReport {
                kind: remanence_parity::FileRowsKind::Object,
                entries: vec![remanence_parity::TapeFileEntry {
                    tape_file_number: 1,
                    kind: remanence_parity::TapeFileKind::Object,
                    block_count: 3,
                    physical_start_hint: Some(2),
                    object_id: Some(record.objects[0].object.object_id.clone()),
                    first_parity_data_ordinal: Some(0),
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: None,
                    object_recovery_row: Some(
                        record.objects[0].object_recovery_row.to_parity_row(),
                    ),
                }],
                highest_protected_ordinal: 0,
                total_committed_ordinals: 3,
            }];
            let mut report: remanence_state::authority::AuthorityCommit = (&record).into();
            report.check = Some((
                remanence_parity::CheckValue(7),
                remanence_parity::CheckProvenance::OriginalWrite,
            ));
            owner.publish_report(&report).unwrap();
        } else {
            owner
                .publish_initialized(
                    BLOCK,
                    Some(scheme()),
                    remanence_state::tape_log::CommitCursor {
                        partition: 0,
                        lba: 2,
                    },
                )
                .unwrap();
        }
        let snapshot = owner.freeze().unwrap();
        assert_eq!(snapshot.tape_summary().closeout_due(), owes_work);
        assert!(snapshot.tape_summary().seal_due.is_none());
        assert!(snapshot.tape_summary().finalization.is_none());
        snapshot
    }

    fn sql(&self, sql: &str) {
        rusqlite::Connection::open(&self.paths.sqlite_path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    fn fail_a(&self) {
        self.sql(&format!("CREATE TRIGGER fail_n2 BEFORE INSERT ON tape_projection_sources WHEN NEW.tape_uuid = X'{}' BEGIN SELECT RAISE(ABORT, '{INJECTED}'); END;", Uuid::from_bytes(A).simple()));
    }

    fn reopen(&mut self) {
        drop(self.state.take());
        self.state =
            Some(StateHandle::open_with_config(self.paths.clone(), self.config.clone()).unwrap());
    }

    fn startup(&self) -> Result<(), tonic::Status> {
        replay_checkpoint_journal_projections_with_audit(
            &mut self.state().write_handle(),
            &self.paths.journal_dir.join("tapes"),
            &self.paths.audit_dir,
            &Arc::new(Mutex::new(())),
        )
    }
}

/// Capture the startup thread's exact warning/error, including its tape identity.
fn capture_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
    let file = tempfile::tempfile().unwrap();
    let mut reader = file.try_clone().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer(move || file.try_clone().unwrap())
        .finish();
    let result = tracing::subscriber::with_default(subscriber, f);
    reader.rewind().unwrap();
    let mut logs = String::new();
    reader.read_to_string(&mut logs).unwrap();
    (result, logs)
}

#[test]
fn t3_startup_defers_only_owed_native_work_and_keeps_error_status() {
    for owes_work in [true, false] {
        let mut f = Fixture::new();
        let a = f.native(A, owes_work);
        let b = f.native(B, false);
        f.fail_a();
        f.reopen(); // The daemon opens LibraryRuntime before its checkpoint replay.
        let runtime = f.state().library_runtime();
        assert!(runtime.projection_lag(A).unwrap().contains(INJECTED));
        let direct_error = f.state().write_handle().project_tape_log(&a).unwrap_err();
        assert!(matches!(
            &direct_error,
            remanence_state::StateError::JournalReplayFailed(_)
        ));
        let expected = crate::startup_media_readiness::status_from_state_error(direct_error);
        let (result, logs) = capture_logs(|| f.startup());
        let uuid = Uuid::from_bytes(A).to_string();
        if owes_work {
            result.expect("owed native work must allow startup deferral");
            assert!(
                logs.lines().any(|line| line.contains("WARN")
                    && line.contains(
                        "startup projection deferred; native closeout obligation retained"
                    )
                    && line.contains(&uuid)
                    && line.contains(INJECTED)),
                "{logs}"
            );
            assert!(runtime.projection_lag(A).is_some());
            assert_eq!(
                f.state().catalog().projected_through_sequence(A).unwrap(),
                0
            );
        } else {
            let error = result.expect_err("a tape with no owed work must still fail startup");
            assert_eq!(error.code(), tonic::Code::FailedPrecondition);
            assert_eq!(error.code(), expected.code());
            assert_eq!(error.message(), expected.message());
            assert!(
                logs.lines().any(|line| line.contains("ERROR")
                    && line.contains("startup tape projection failed")
                    && line.contains(&uuid)
                    && line.contains(INJECTED)),
                "{logs}"
            );
            assert!(!logs.contains("startup projection deferred"));
        }
        assert_eq!(
            f.state().catalog().projected_through_sequence(B).unwrap(),
            b.tape_summary().last_closed_sequence
        );
        assert!(runtime.projection_lag(B).is_none());
    }
}

/// Publish the same complete semantic Object shape used by native state fixtures.
fn checkpoint(tape_uuid: [u8; 16]) -> CommitFixture {
    let object_uuid = uuid::Uuid::from_bytes(tape_uuid.map(|byte| byte ^ 0x10));
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
            instance: NativeObjectInstanceProjectionInput {
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

#[test]
fn t4_tail_recovery_refuses_lag_before_catalog_admission() {
    use remanence_chaos::model::{DeviceRole, ModelTransport, VirtualWorld};
    use remanence_library::{DriveHandle, StaticAllowlist};
    use remanence_state::AuditActor;

    let mut f = Fixture::new();
    let a = f.native(A, true);
    f.native(B, false);
    f.fail_a();
    let runtime = f.state().library_runtime();
    let error = f.state().write_handle().project_tape_log(&a).unwrap_err();
    assert!(error.to_string().contains(INJECTED));
    let cfg = crate::write_owner::TerminalWriteConfig {
        authority: crate::write_owner::TerminalAuthority::Typed(runtime.clone()),
        io_memory: crate::io_memory::IoMemoryReservation::new(64 * 1024 * 1024).unwrap(),
        mounted_endpoint: None,
        checkpoint_journal_dir: f.paths.journal_dir.join("tapes"),
    };
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "N2-LIBRARY",
        0x100,
        "N2-DRIVE",
        0x400,
        0,
    )));
    let mut drive = DriveHandle::open_standalone_with_transport(
        std::path::Path::new("/virtual/n2-drive"),
        Box::new(ModelTransport::new(
            world.clone(),
            DeviceRole::Drive { bay: 0x100 },
        )),
        &StaticAllowlist::new(["/virtual/n2-drive"]),
    )
    .unwrap();
    let before_commands = world.lock().unwrap().command_log.len();
    let mut recover = |tape| {
        crate::write_owner::supersede_mounted_tail(
            &mut f.state().write_handle(),
            &cfg,
            &mut drive,
            tape,
            1,
            "wrong-acknowledgment",
            AuditActor::System,
            |_, _, _| panic!("refusal must precede confirmation and media I/O"),
        )
        .unwrap_err()
    };
    let lag = recover(A);
    assert_eq!(lag.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        lag.message(),
        format!(
            "the tape's catalog is behind its native log; retry after it catches up: {}",
            runtime.projection_lag(A).unwrap(),
        )
    );
    assert!(lag.message().contains(INJECTED));
    let healthy = recover(B);
    assert!(runtime.projection_lag(B).is_none());
    assert_eq!(healthy.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        healthy.message(),
        "acknowledgment does not identify the loaded tape"
    );
    f.sql("DROP TRIGGER fail_n2;");
    let recovered = recover(A);
    assert!(runtime.projection_lag(A).is_none());
    assert_eq!(recovered.code(), healthy.code());
    assert_eq!(recovered.message(), healthy.message());
    assert_eq!(world.lock().unwrap().command_log.len(), before_commands);
}

#[test]
fn t2_other_tape_append_motion_and_scan_recover_locally() {
    use remanence_state::library_log as lib;
    let mut f = Fixture::new();
    let a = f.native(A, true);
    let b = f.native(B, false);
    f.fail_a();
    let runtime = f.state().library_runtime();
    let mut index = f.state().write_handle();
    let error = index.project_tape_log(&a).unwrap_err();
    assert!(matches!(
        &error,
        remanence_state::StateError::JournalReplayFailed(_)
    ));
    assert!(
        error.to_string().contains(INJECTED),
        "the injected A trigger must fire"
    );
    assert!(runtime.projection_lag(A).unwrap().contains(INJECTED));

    index
        .project_tape_log(&b)
        .expect("B must project while A lags");
    assert_eq!(
        index.projected_through_sequence(B).unwrap(),
        b.tape_summary().last_closed_sequence
    );
    assert_eq!(index.projected_through_sequence(A).unwrap(), 0);
    let receipt = runtime
        .append(|authority, view| {
            authority.diagnostic(
                lib::Diagnostic {
                    event: lib::DiagnosticEvent::HardwareWarning,
                    subject_kind: "library".into(),
                    subject_id: None,
                    detail: lib::Bounded(vec![]),
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                view,
            )
        })
        .expect("a library append must succeed while A lags");
    assert!(receipt.sequence > 0);
    runtime
        .ensure_motion_allowed()
        .expect("motion admission must succeed while A lags");
    assert!(runtime.projection_lag(B).is_none());
    assert!(runtime.projection_lag(A).is_some());

    let mut api = crate::ApiState::new(f.state().projection_client());
    api.checkpoint_journal_dir = Arc::new(f.paths.journal_dir.join("tapes"));
    api.audit_dir = Arc::new(f.paths.audit_dir.clone());
    f.sql("DROP TRIGGER fail_n2;");
    crate::closeout_obligation::scan(&api).unwrap();
    assert!(runtime.projection_lag(A).is_none(), "scan must clear A lag");
    assert_eq!(
        index.projected_through_sequence(A).unwrap(),
        a.tape_summary().last_closed_sequence
    );
    assert!(
        api.closeout_work.contains((A, 1)),
        "host projection must retain native closeout work"
    );
    crate::write_owner::tests::assert_rebuild_safety_equivalent(f.state.as_mut().unwrap());
    assert!(runtime.projection_lag(A).is_none());
    assert!(runtime.projection_lag(B).is_none());
}
