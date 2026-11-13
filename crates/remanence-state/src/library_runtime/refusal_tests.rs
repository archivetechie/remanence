//! `record_refusal`, the one funnel every counted stand-in refusal goes through: each call is one
//! `RefusalRecorded` with the code's number and its own name, the subject is encoded as the
//! `Diagnostic` fields define it, and the records survive a reopen of the store.
use super::*;

struct Fixture {
    paths: StatePaths,
    runtime: LibraryRuntime,
    lease: Arc<StateLockGuard>,
    _root: tempfile::TempDir,
}

fn open() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let paths = StatePaths {
        config_path: root.path().join("config"),
        state_dir: root.path().join("state"),
        audit_dir: root.path().join("library"),
        journal_dir: root.path().join("native"),
        sqlite_path: root.path().join("catalog.sqlite"),
        tape_cache_dir: root.path().join("cache"),
        calibration_dir: root.path().join("calibration"),
    };
    let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
    let runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
    Fixture {
        paths,
        runtime,
        lease,
        _root: root,
    }
}

/// What a `RefusalRecorded` record says: its code and name, subject, actor and layer.
pub(crate) type Seen = (
    u64,
    String,
    String,
    Option<String>,
    lib::AuditActor,
    lib::SourceLayer,
);

pub(crate) fn refusals(runtime: &LibraryRuntime) -> Vec<Seen> {
    runtime
        .test_source_records()
        .into_iter()
        .filter_map(|record| {
            let lib::BodyView::Diagnostic(d) = record.body_view() else {
                return None;
            };
            assert_eq!(d.event, lib::DiagnosticEvent::RefusalRecorded);
            assert_eq!(d.detail.0.len(), 2, "a code and a name, nothing else");
            let (lib::DiagnosticValue::Unsigned(code), lib::DiagnosticValue::Text(name)) =
                (&d.detail.0[0].1, &d.detail.0[1].1)
            else {
                panic!("detail values: {d:?}");
            };
            assert_eq!(
                (d.detail.0[0].0.as_str(), d.detail.0[1].0.as_str()),
                ("code", "name")
            );
            Some((
                *code,
                name.as_str().to_owned(),
                d.subject_kind.as_str().to_owned(),
                d.subject_id.as_ref().map(|id| id.as_str().to_owned()),
                record.envelope.actor.clone(),
                record.envelope.source_layer.clone(),
            ))
        })
        .collect()
}

const NAMES: [&str; 14] = [
    "TapeLogBehindProjection",
    "ContradictedTapeAdmission",
    "ClosedOperationRetry",
    "RetainedIntentForeignOwner",
    "StandaloneFenceUnrecorded",
    "ReconcileTapeDeferred",
    "UnknownGenerationAdoption",
    "DriveIdentityConflictLatch",
    "ForeignDrivePollDisabled",
    "DriveStewardshipDeferred",
    "CheckpointBarrierAlarmUnrecorded",
    "DriveFenced",
    "TailRepairDeferred",
    "JournalImportDeferred",
];

/// Every allocated code is one record carrying its number and the variant's own name, appended
/// once per call, and a reopened store replays the same records.
#[test]
fn each_call_records_one_refusal_with_the_codes_own_name_and_survives_a_reopen() {
    let f = open();
    let sequence = |f: &Fixture| f.runtime.test_source_records().len();
    let baseline = sequence(&f);
    for (i, name) in NAMES.iter().enumerate() {
        let code = lib::RefusalCode::from_u32(u32::try_from(i + 1).unwrap()).unwrap();
        f.runtime
            .record_refusal(code, RefusalSubject::Library, lib::AuditActor::System);
        assert_eq!(
            sequence(&f),
            baseline + i + 1,
            "one record per call: {name}"
        );
    }
    let seen = refusals(&f.runtime);
    assert_eq!(seen.len(), 14);
    for (i, (code, name, ..)) in seen.iter().enumerate() {
        assert_eq!(*code, u64::try_from(i + 1).unwrap());
        assert_eq!(name, NAMES[i]);
    }

    let Fixture {
        paths,
        runtime,
        lease,
        _root,
    } = f;
    drop(runtime);
    let reopened = LibraryRuntime::open(&paths, true, lease).expect("the log reopens");
    assert_eq!(refusals(&reopened), seen, "the reopened store replays them");
}

/// The subject is the variant's kind, and the hyphenated UUID when one is named. The actor is the
/// caller's, and the layer is the runtime's.
#[test]
fn the_subject_actor_and_layer_are_recorded_as_given() {
    let f = open();
    let code = lib::RefusalCode::JournalImportDeferred;
    let actor = lib::AuditActor::User("operator".into());
    f.runtime
        .record_refusal(code, RefusalSubject::Tape(Some([0x11; 16])), actor.clone());
    f.runtime
        .record_refusal(code, RefusalSubject::Tape(None), actor.clone());
    f.runtime
        .record_refusal(code, RefusalSubject::Drive([0x22; 16]), actor.clone());
    f.runtime
        .record_refusal(code, RefusalSubject::Library, lib::AuditActor::System);
    let layer = lib::SourceLayer::Layer5;
    let name = "JournalImportDeferred".to_owned();
    assert_eq!(
        refusals(&f.runtime),
        [
            (
                14,
                name.clone(),
                "tape".to_owned(),
                Some("11111111-1111-1111-1111-111111111111".to_owned()),
                actor.clone(),
                layer.clone()
            ),
            (
                14,
                name.clone(),
                "tape".to_owned(),
                None,
                actor.clone(),
                layer.clone()
            ),
            (
                14,
                name.clone(),
                "drive".to_owned(),
                Some("22222222-2222-2222-2222-222222222222".to_owned()),
                actor,
                layer.clone()
            ),
            (
                14,
                name,
                "library".to_owned(),
                None,
                lib::AuditActor::System,
                layer
            ),
        ]
    );
}

fn drive_input(serial: &str, vendor: &str) -> crate::DriveObservationInput {
    crate::DriveObservationInput {
        serial: serial.to_string(),
        identity_source: "DvcidAndInquiry".to_string(),
        vendor: Some(vendor.to_string()),
        product: Some("Ultrium 9-SCSI".to_string()),
        firmware_rev: Some("R1.0".to_string()),
        managed: "rem".to_string(),
        library_serial: Some("LIB-REFUSAL".to_string()),
        element_address: Some(0x100),
        observed_at_utc: None,
    }
}

fn drive_uuid_text(outcome: &crate::DriveObservationOutcome) -> String {
    uuid::Uuid::from_slice(&outcome.drive_uuid)
        .unwrap()
        .to_string()
}

fn drive_conflict(drive: String) -> Seen {
    (
        8,
        "DriveIdentityConflictLatch".to_owned(),
        "drive".to_owned(),
        Some(drive),
        lib::AuditActor::System,
        lib::SourceLayer::Layer5,
    )
}

/// Code 8, through `observe_drives`, the method discovery calls. A new identity for a serial an
/// active drive already holds is refused: the motion fence latches, the refusal answers with its
/// text, and exactly one record names the drive that holds the serial.
#[test]
fn a_new_identity_for_an_active_serial_is_refused_and_recorded_with_the_holder() {
    let f = open();
    let held = f
        .runtime
        .observe_drives(vec![drive_input("SER-1", "HPE")])
        .unwrap()
        .remove(0);
    let before = f.runtime.test_source_records().len();
    assert!(!f.runtime.motion_fenced());

    let error = f
        .runtime
        .observe_drives(vec![drive_input("SER-1", "IBM")])
        .expect_err("a second identity for the serial is refused");
    assert!(
        error
            .to_string()
            .contains("new identity would collide with an active serial; drive discovery cannot preserve the required identity alarm; motion is fenced"),
        "{error}"
    );
    assert!(f.runtime.motion_fenced());
    assert_eq!(f.runtime.test_source_records().len(), before + 1);
    assert_eq!(
        refusals(&f.runtime),
        [drive_conflict(drive_uuid_text(&held))]
    );
}

/// Code 8: a retired drive that reappears is refused, naming the retired drive.
#[test]
fn a_retired_drive_that_reappears_is_refused_and_recorded_with_its_drive() {
    let f = open();
    let retired = f
        .runtime
        .observe_drives(vec![drive_input("SER-2", "HPE")])
        .unwrap()
        .remove(0);
    f.runtime
        .append(|a, v| {
            a.drive_decision(
                lib::DriveDecision {
                    drive_uuid: retired.drive_uuid.clone().try_into().unwrap(),
                    decision: lib::Stewardship::Retire(lib::Reason {
                        reason: "operator".into(),
                    }),
                    change: lib::GenerationChange {
                        expected: 1,
                        next: 2,
                    },
                },
                1,
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    let before = f.runtime.test_source_records().len();

    let error = f
        .runtime
        .observe_drives(vec![drive_input("SER-2", "HPE")])
        .expect_err("a retired drive does not reappear");
    assert!(
        error.to_string().contains("retired drive reappeared"),
        "{error}"
    );
    assert!(f.runtime.motion_fenced());
    assert_eq!(f.runtime.test_source_records().len(), before + 1);
    assert_eq!(
        refusals(&f.runtime),
        [drive_conflict(drive_uuid_text(&retired))]
    );
}

/// Code 8: two active drives that already hold one serial refuse any batch that names it, naming
/// the earliest-seen holder.
#[test]
fn a_historical_active_serial_collision_is_refused_and_recorded_with_its_earliest_holder() {
    let f = open();
    let first = f
        .runtime
        .observe_drives(vec![drive_input("SER-3", "HPE")])
        .unwrap()
        .remove(0);
    // A log written before this check existed can hold a second active drive for the serial.
    let second = [0xEE; 16];
    f.runtime
        .append(|a, v| {
            a.drive_observed(
                lib::DriveObserved {
                    drive_uuid: second,
                    serial: Some("SER-3".into()),
                    identity_source: lib::IdentitySource::DvcidAndInquiry,
                    endpoint: None,
                    managed: lib::Managed::Rem,
                    inventory_generation: 5,
                    vendor: Some("IBM".into()),
                    product: Some("Ultrium 9-SCSI".into()),
                    firmware_rev: None,
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    let before = f.runtime.test_source_records().len();

    let error = f
        .runtime
        .observe_drives(vec![drive_input("SER-3", "HPE")])
        .expect_err("two active holders are a collision");
    assert!(
        error
            .to_string()
            .contains("historical active serial collision"),
        "{error}"
    );
    assert_eq!(f.runtime.test_source_records().len(), before + 1);
    assert_eq!(
        refusals(&f.runtime),
        [drive_conflict(drive_uuid_text(&first))]
    );
}

/// A blank serial is a malformed observation, not a collision: it is refused as before and not
/// counted.
#[test]
fn a_blank_serial_is_refused_without_a_record() {
    let f = open();
    let before = f.runtime.test_source_records().len();
    let error = f
        .runtime
        .observe_drives(vec![drive_input("  ", "HPE")])
        .expect_err("a blank serial is refused");
    assert!(error.to_string().contains("blank drive serial"), "{error}");
    assert!(f.runtime.motion_fenced());
    assert_eq!(f.runtime.test_source_records().len(), before);
    assert!(refusals(&f.runtime).is_empty());
}

/// Code 8: one serial reported twice in a discovery snapshot is a serial collision. It is refused
/// and counted once, naming the known drive that holds the serial, or the library when the serial
/// has never been seen.
#[test]
fn a_duplicated_serial_in_a_snapshot_is_refused_and_recorded_with_its_known_drive_or_the_library() {
    let f = open();
    let known = f
        .runtime
        .observe_drives(vec![drive_input("SER-5", "HPE")])
        .unwrap()
        .remove(0);
    let before = f.runtime.test_source_records().len();

    let error = f
        .runtime
        .observe_drives(vec![
            drive_input("SER-NEW", "HPE"),
            drive_input("SER-6", "HPE"),
            drive_input("SER-6", "IBM"),
        ])
        .expect_err("a serial reported twice is a collision");
    assert!(
        error
            .to_string()
            .contains("duplicate serial in discovery snapshot; drive discovery cannot preserve the required identity alarm; motion is fenced"),
        "{error}"
    );
    assert!(f.runtime.motion_fenced());
    assert_eq!(f.runtime.test_source_records().len(), before + 1);
    let library = (
        8,
        "DriveIdentityConflictLatch".to_owned(),
        "library".to_owned(),
        None,
        lib::AuditActor::System,
        lib::SourceLayer::Layer5,
    );
    assert_eq!(refusals(&f.runtime), std::slice::from_ref(&library));

    let error = f
        .runtime
        .observe_drives(vec![
            drive_input("SER-5", "HPE"),
            drive_input("SER-5", "HPE"),
        ])
        .expect_err("a known serial reported twice is a collision");
    assert!(
        error
            .to_string()
            .contains("duplicate serial in discovery snapshot"),
        "{error}"
    );
    assert_eq!(f.runtime.test_source_records().len(), before + 2);
    assert_eq!(
        refusals(&f.runtime),
        [library, drive_conflict(drive_uuid_text(&known))]
    );
}

fn accepted(key: u8) -> lib::Accepted {
    lib::Accepted {
        actor_fingerprint: "operator".into(),
        idempotency_key: Some([key; 16]),
        request_fingerprint: [key; 32],
        subject: None,
    }
}

/// Code 3, through `begin_manual_finalization`, which the finalize admission calls. A retry whose
/// operation is open continues it and records nothing. Once the operation has closed, the same
/// retry is refused with the code's prefix, and exactly one record names the operation and the
/// actor that retried.
#[test]
fn a_retry_of_a_closed_manual_operation_is_refused_and_recorded_with_its_operation() {
    let f = open();
    let operator = lib::AuditActor::User("operator".into());
    let operation = f
        .runtime
        .begin_manual_finalization(accepted(7), operator.clone())
        .unwrap();
    let before = f.runtime.test_source_records().len();
    assert_eq!(
        f.runtime
            .begin_manual_finalization(accepted(7), operator.clone())
            .unwrap(),
        operation,
        "an open operation is continued"
    );
    assert!(refusals(&f.runtime).is_empty());

    f.runtime
        .record_operation_event(
            operation,
            lib::OperationKind::FinalizeTape,
            lib::OperationEvent::Failed(lib::Failed {
                retry: None,
                refusal_code: None,
                summary: "drive actor is unavailable".into(),
            }),
            lib::AuditActor::System,
        )
        .unwrap();
    let closed = f.runtime.test_source_records().len();
    assert_eq!(
        closed,
        before + 1,
        "the operation's own Failed is the only new record"
    );

    let retrier = lib::AuditActor::User("retrier".into());
    let error = f
        .runtime
        .begin_manual_finalization(accepted(7), retrier.clone())
        .expect_err("a closed operation admits no retry");
    assert!(
        error.to_string().starts_with(
            "tape provisioning conflict: ClosedOperationRetry: manual request operation is closed ("
        ),
        "{error}"
    );
    assert_eq!(f.runtime.test_source_records().len(), closed + 1);
    assert_eq!(
        refusals(&f.runtime),
        [(
            3,
            "ClosedOperationRetry".to_owned(),
            "operation".to_owned(),
            Some(uuid::Uuid::from_bytes(operation).to_string()),
            retrier,
            lib::SourceLayer::Layer5,
        )]
    );
}

/// T-X: resolution is read-only, serial-based, and unique among active drives;
/// a retired identity never competes with its replacement.
#[test]
fn wdrv_resolves_only_unique_active_serial() {
    let f = open();
    let before = f.runtime.test_source_records().len();
    assert_eq!(
        f.runtime.active_drive_uuid_for_serial("SER-WDRV").unwrap(),
        None
    );
    assert_eq!(f.runtime.test_source_records().len(), before);
    let retired = f
        .runtime
        .observe_drives(vec![drive_input("SER-WDRV", "HPE")])
        .unwrap()
        .remove(0)
        .drive_uuid;
    let before = f.runtime.test_source_records().len();
    assert_eq!(
        f.runtime
            .active_drive_uuid_for_serial(" SER-WDRV ")
            .unwrap(),
        Some(retired.clone().try_into().unwrap())
    );
    assert_eq!(f.runtime.test_source_records().len(), before);
    f.runtime
        .append(|a, v| {
            a.drive_decision(
                lib::DriveDecision {
                    drive_uuid: retired.clone().try_into().unwrap(),
                    decision: lib::Stewardship::Retire(lib::Reason {
                        reason: "operator".into(),
                    }),
                    change: lib::GenerationChange {
                        expected: 1,
                        next: 2,
                    },
                },
                1,
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    assert_eq!(
        f.runtime.active_drive_uuid_for_serial("SER-WDRV").unwrap(),
        None
    );
    let mut replacement = drive_input("SER-WDRV", "HPE");
    replacement.product = Some("Replacement product".into());
    let active = f
        .runtime
        .observe_drives(vec![replacement])
        .unwrap()
        .remove(0)
        .drive_uuid;
    assert_ne!(active, retired);
    let before = f.runtime.test_source_records().len();
    assert_eq!(
        f.runtime.active_drive_uuid_for_serial("SER-WDRV").unwrap(),
        Some(active.try_into().unwrap()),
        "retired-and-active serial resolves the active drive"
    );
    assert_eq!(f.runtime.test_source_records().len(), before);
    // The existing historical-collision fixture shows how older source records
    // can hold two active identities. Resolution must not choose either one.
    f.runtime
        .append(|a, v| {
            a.drive_observed(
                lib::DriveObserved {
                    drive_uuid: [0xee; 16],
                    serial: Some("SER-WDRV".into()),
                    identity_source: lib::IdentitySource::DvcidAndInquiry,
                    endpoint: None,
                    managed: lib::Managed::Rem,
                    inventory_generation: 5,
                    vendor: Some("IBM".into()),
                    product: Some("Another product".into()),
                    firmware_rev: None,
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    let before = f.runtime.test_source_records().len();
    assert_eq!(
        f.runtime.active_drive_uuid_for_serial("SER-WDRV").unwrap(),
        None
    );
    assert_eq!(f.runtime.test_source_records().len(), before);
}
