use super::*;
use crate::error::{AuditOp, AuditOutcome, RescanError};
use crate::model::{DriveBay, ElementLayout, IePort, InstalledDrive, Slot};
use crate::transport::{FixtureTransport, RecordingLog, RecordingTransport, TransferOutcome};
use crate::StaticAllowlist;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Build a minimal Library with one drive bay (DvcidInline by
/// default) for the gate tests. Caller can post-mutate.
fn fake_library(serial: &str) -> Library {
    Library {
        serial: serial.to_string(),
        changer_sg: PathBuf::from("/dev/sg-mock"),
        changer_sysfs: PathBuf::from("/sys/class/scsi_device/mock"),
        changer_inquiry: remanence_scsi::Inquiry::parse(include_bytes!(
            "../../../../fixtures/inquiry/changer-msl-g3.bin"
        ))
        .unwrap(),
        chassis_designator: None,
        layout: ElementLayout {
            robot_address: 0,
            drive_start: 1,
            drive_count: 1,
            slot_start: 1000,
            slot_count: 0,
            ie_start: 0,
            ie_count: 0,
        },
        drive_bays: vec![DriveBay {
            element_address: 1,
            installed: Some(InstalledDrive {
                serial: "DRIVE12345".into(),
                identity_source: IdentitySource::DvcidInline,
                vendor: None,
                product: None,
                revision: None,
                sg_path: None,
                sysfs_path: None,
            }),
            loaded: false,
            loaded_tape: None,
            source_slot: None,
        }],
        slots: vec![],
        ie_ports: vec![],
    }
}

/// Build a VPD 0x80 response buffer with the given serial (no
/// padding; the page-length byte is set to the string's length).
fn vpd80_response(serial: &str) -> Vec<u8> {
    let bytes = serial.as_bytes();
    let mut v = vec![0x08u8, 0x80, 0x00, bytes.len() as u8];
    v.extend_from_slice(bytes);
    v
}

/// Standard INQUIRY response for a medium-changer device, reused as
/// the revalidation answer in tests where revalidation should
/// succeed.
fn changer_inquiry_response() -> Vec<u8> {
    include_bytes!("../../../../fixtures/inquiry/changer-msl-g3.bin").to_vec()
}

/// Standard INQUIRY response for an LTO-9 tape drive — used by the
/// "device under cached path is no longer a changer" test.
fn tape_inquiry_response() -> Vec<u8> {
    include_bytes!("../../../../fixtures/inquiry/drive1-lto9.bin").to_vec()
}

/// Make a transport factory that returns a single FixtureTransport
/// pre-loaded with the given canned responses, boxed.
fn fixture_factory(
    responses: Vec<Vec<u8>>,
) -> impl FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind> {
    let mut once = Some(responses);
    move |_path: &Path| {
        let r = once.take().ok_or_else(|| IoErrorKind {
            kind: "Other",
            message: "transport already opened once".into(),
            raw_os_error: None,
        })?;
        Ok(Box::new(FixtureTransport::new().with_responses(r)))
    }
}

#[test]
fn open_succeeds_when_policy_allows_and_serial_matches() {
    let lib = fake_library("LIB001");
    let policy = StaticAllowlist::new(["LIB001"]);
    let factory = fixture_factory(vec![changer_inquiry_response(), vpd80_response("LIB001")]);
    let handle = lib
        .open_with(&policy, factory)
        .expect("open should succeed");
    assert_eq!(handle.library().serial, "LIB001");
}

#[test]
fn open_refuses_when_policy_does_not_allow() {
    let lib = fake_library("LIB001");
    let policy = StaticAllowlist::new(["OTHER_LIB"]);
    // Transport factory should never even be consulted.
    let factory = |_p: &Path| -> Result<Box<dyn SgTransport>, IoErrorKind> {
        panic!("transport opened despite policy refusal")
    };
    let err = lib.open_with(&policy, factory).unwrap_err();
    match err {
        OpenError::NotAllowed { serial } => assert_eq!(serial, "LIB001"),
        other => panic!("expected NotAllowed, got {other:?}"),
    }
}

#[test]
fn open_refuses_derived_identity_without_opt_in() {
    let mut lib = fake_library("LIB002");
    // Flip the bay's identity source to Derived.
    lib.drive_bays[0]
        .installed
        .as_mut()
        .unwrap()
        .identity_source = IdentitySource::Derived;
    let policy = StaticAllowlist::new(["LIB002"]);
    let factory = |_p: &Path| -> Result<Box<dyn SgTransport>, IoErrorKind> {
        panic!("transport opened despite derived-identity refusal")
    };
    let err = lib.open_with(&policy, factory).unwrap_err();
    match err {
        OpenError::DerivedIdentityNotOptedIn { serial } => {
            assert_eq!(serial, "DRIVE12345");
        }
        other => panic!("expected DerivedIdentityNotOptedIn, got {other:?}"),
    }
}

#[test]
fn open_succeeds_when_derived_identity_is_explicitly_allowed() {
    let mut lib = fake_library("LIB003");
    lib.drive_bays[0]
        .installed
        .as_mut()
        .unwrap()
        .identity_source = IdentitySource::Derived;
    let policy = StaticAllowlist::new(["LIB003"]).with_derived_allowed("LIB003");
    let factory = fixture_factory(vec![changer_inquiry_response(), vpd80_response("LIB003")]);
    let _handle = lib
        .open_with(&policy, factory)
        .expect("derived opt-in allows open");
}

#[test]
fn open_fails_with_identity_changed_when_serial_drifts() {
    let lib = fake_library("LIB004");
    let policy = StaticAllowlist::new(["LIB004"]);
    // The device at /dev/sg-mock is still a changer (std INQUIRY
    // shows MediumChanger) but now reports a different VPD 0x80
    // serial — kernel re-enumeration swapped a different changer
    // into this path since discovery.
    let factory = fixture_factory(vec![
        changer_inquiry_response(),
        vpd80_response("SOMEONE_ELSE"),
    ]);
    let err = lib.open_with(&policy, factory).unwrap_err();
    match err {
        OpenError::IdentityChanged {
            path,
            expected,
            actual,
        } => {
            assert_eq!(path, PathBuf::from("/dev/sg-mock"));
            assert_eq!(expected, "LIB004");
            assert_eq!(actual.as_deref(), Some("SOMEONE_ELSE"));
        }
        other => panic!("expected IdentityChanged, got {other:?}"),
    }
}

#[test]
fn open_fails_with_identity_changed_when_device_is_no_longer_a_changer() {
    // The cached /dev/sg-mock used to be the medium changer for
    // LIB008; after kernel re-enumeration it now points at a tape
    // drive. Standard INQUIRY reports DeviceType::SequentialAccess,
    // not MediumChanger, so revalidation refuses immediately —
    // before even asking for VPD 0x80.
    let lib = fake_library("LIB008");
    let policy = StaticAllowlist::new(["LIB008"]);
    // Only one canned response: revalidation should fail on
    // std INQUIRY and never reach VPD 0x80.
    let factory = fixture_factory(vec![tape_inquiry_response()]);
    let err = lib.open_with(&policy, factory).unwrap_err();
    match err {
        OpenError::IdentityChanged {
            path,
            expected,
            actual,
        } => {
            assert_eq!(path, PathBuf::from("/dev/sg-mock"));
            assert_eq!(expected, "LIB008");
            assert!(
                actual.is_none(),
                "tape device has no comparable changer serial"
            );
        }
        other => panic!("expected IdentityChanged, got {other:?}"),
    }
}

#[test]
fn open_fails_with_device_unavailable_when_transport_open_errs() {
    let lib = fake_library("LIB005");
    let policy = StaticAllowlist::new(["LIB005"]);
    let factory = |_p: &Path| -> Result<Box<dyn SgTransport>, IoErrorKind> {
        Err(IoErrorKind {
            kind: "PermissionDenied",
            message: "EACCES from mock".into(),
            raw_os_error: Some(13),
        })
    };
    let err = lib.open_with(&policy, factory).unwrap_err();
    match err {
        OpenError::DeviceUnavailable { path, cause } => {
            assert_eq!(path, PathBuf::from("/dev/sg-mock"));
            assert_eq!(cause.kind, "PermissionDenied");
            assert_eq!(cause.raw_os_error, Some(13));
        }
        other => panic!("expected DeviceUnavailable, got {other:?}"),
    }
}

// =====================================================================
//  move_medium — §7.3
// =====================================================================

/// Build a library with a slot pre-loaded with TAPE_A and one empty
/// drive bay, suitable for slot → drive-bay tests.
fn move_medium_test_lib(serial: &str) -> Library {
    Library {
        serial: serial.to_string(),
        changer_sg: PathBuf::from("/dev/sg-mock"),
        changer_sysfs: PathBuf::from("/sys/class/scsi_device/mock"),
        changer_inquiry: remanence_scsi::Inquiry::parse(include_bytes!(
            "../../../../fixtures/inquiry/changer-msl-g3.bin"
        ))
        .unwrap(),
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
            installed: Some(InstalledDrive {
                serial: "DRV_A".into(),
                identity_source: IdentitySource::DvcidAndInquiry,
                vendor: None,
                product: None,
                revision: None,
                sg_path: None,
                sysfs_path: None,
            }),
            loaded: false,
            loaded_tape: None,
            source_slot: None,
        }],
        slots: vec![Slot {
            element_address: 0x0400,
            full: true,
            cartridge: Some("TAPE_A".into()),
        }],
        ie_ports: Vec::<IePort>::new(),
    }
}

/// Build a transport factory that returns the given seeded
/// FixtureTransport wrapped in a RecordingTransport. Captured-log
/// handle is returned for the test to inspect after.
#[allow(clippy::type_complexity)]
fn recording_factory(
    responses: Vec<Vec<u8>>,
) -> (
    impl FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>,
    RecordingLog,
) {
    let inner = FixtureTransport::new().with_responses(responses);
    let log: RecordingLog = RecordingLog::new();
    let recorded = RecordingTransport::with_log(inner, log.clone());
    let mut holder = Some(recorded);
    let factory = move |_path: &Path| -> Result<Box<dyn SgTransport>, IoErrorKind> {
        holder
            .take()
            .map(|t| Box::new(t) as Box<dyn SgTransport>)
            .ok_or_else(|| IoErrorKind {
                kind: "Other",
                message: "factory already produced its single transport".into(),
                raw_os_error: None,
            })
    };
    (factory, log)
}

/// Owned, comparable form of [`AuditEvent`] for test assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CapturedEvent {
    Started {
        op: AuditOp,
        cdb: Vec<u8>,
    },
    Refused {
        op: AuditOp,
        reason: &'static str,
    },
    FinishedSuccess {
        op: AuditOp,
        snapshot_patched: bool,
    },
    FinishedScsiError {
        op: AuditOp,
        summary: String,
    },
    Warning {
        op: AuditOp,
        warning: crate::error::RescanWarning,
    },
}

fn capture_audit() -> (
    impl FnMut(&AuditEvent<'_>) + Send + 'static,
    Arc<Mutex<Vec<CapturedEvent>>>,
) {
    let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_cl = Arc::clone(&captured);
    let hook = move |event: &AuditEvent<'_>| {
        let owned = match event {
            AuditEvent::Started { operation, cdb, .. } => CapturedEvent::Started {
                op: *operation,
                cdb: cdb.to_vec(),
            },
            AuditEvent::Refused {
                operation, reason, ..
            } => CapturedEvent::Refused {
                op: *operation,
                reason,
            },
            AuditEvent::Finished {
                operation, outcome, ..
            } => match outcome {
                AuditOutcome::Success {
                    snapshot_patched, ..
                } => CapturedEvent::FinishedSuccess {
                    op: *operation,
                    snapshot_patched: *snapshot_patched,
                },
                AuditOutcome::ScsiError { summary, .. } => CapturedEvent::FinishedScsiError {
                    op: *operation,
                    summary: summary.clone(),
                },
                AuditOutcome::Other { summary } => CapturedEvent::FinishedScsiError {
                    op: *operation,
                    summary: summary.clone(),
                },
            },
            AuditEvent::Warning {
                operation, warning, ..
            } => CapturedEvent::Warning {
                op: *operation,
                warning: warning.clone(),
            },
        };
        captured_cl.lock().unwrap().push(owned);
    };
    (hook, captured)
}

#[test]
fn move_medium_happy_path() {
    let lib = move_medium_test_lib("LIB_MM01");
    let policy = StaticAllowlist::new(["LIB_MM01"]);
    // Open responses (INQUIRY + VPD 0x80) then MOVE MEDIUM CDB
    // succeeds with no canned response needed (FixtureTransport
    // returns Ok(()) for execute_none).
    let (factory, log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/changer-msl-g3.bin").to_vec(),
        vpd80_response("LIB_MM01"),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("open");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    handle
        .move_medium(0x0400, 0x0100, &policy)
        .expect("move ok");

    // Snapshot patched.
    let lib_after = handle.library();
    let bay = &lib_after.drive_bays[0];
    assert!(bay.loaded);
    assert_eq!(bay.loaded_tape.as_deref(), Some("TAPE_A"));
    assert_eq!(bay.source_slot, Some(0x0400));
    let slot = &lib_after.slots[0];
    assert!(!slot.full);
    assert!(slot.cartridge.is_none());

    // MOVE MEDIUM CDB went out and matches the builder. Filter
    // out the open-time INQUIRY (0x12) probes.
    let move_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0xA5)
        .cloned()
        .collect();
    assert_eq!(move_cdbs.len(), 1, "exactly one MOVE MEDIUM CDB issued");
    assert_eq!(
        move_cdbs[0],
        vec![0xA5, 0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00],
    );

    // Audit: Started → FinishedSuccess, both with operation=Move{src,dst}.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    match &events[0] {
        CapturedEvent::Started { op, cdb } => {
            assert_eq!(
                *op,
                AuditOp::Move {
                    src: 0x0400,
                    dst: 0x0100
                }
            );
            assert_eq!(cdb[0], 0xA5);
        }
        other => panic!("expected Started, got {other:?}"),
    }
    assert!(matches!(
        events[1],
        CapturedEvent::FinishedSuccess {
            op: AuditOp::Move {
                src: 0x0400,
                dst: 0x0100
            },
            snapshot_patched: true,
        }
    ));
}

#[test]
fn audit_hook_panic_does_not_permanently_poison_shared_state() {
    let lib = move_medium_test_lib("LIB_MM_POISON");
    let policy = StaticAllowlist::new(["LIB_MM_POISON"]);
    let (factory, _log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/changer-msl-g3.bin").to_vec(),
        vpd80_response("LIB_MM_POISON"),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("open");

    let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fired_cl = fired.clone();
    handle.set_audit_hook(move |_event| {
        if !fired_cl.swap(true, std::sync::atomic::Ordering::SeqCst) {
            panic!("synthetic audit hook panic");
        }
    });

    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = handle.move_medium(0x0400, 0x0100, &policy);
    }));
    assert!(panic_result.is_err());

    assert!(
        !handle.is_dirty(),
        "started-event panic happens before a CDB is issued"
    );
    handle
        .move_medium(0x0400, 0x0100, &policy)
        .expect("poisoned shared state is recoverable");
    assert!(handle.library().drive_bays[0].loaded);
}

#[test]
fn changer_handle_is_usable_standalone() {
    let lib = move_medium_test_lib("LIB_CH01");
    let policy = StaticAllowlist::new(["LIB_CH01"]);
    let (factory, log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/changer-msl-g3.bin").to_vec(),
        vpd80_response("LIB_CH01"),
    ]);
    let handle = lib.open_with(&policy, factory).expect("open");
    let mut changer = handle.into_changer();

    changer
        .move_medium(0x0400, 0x0100, &policy)
        .expect("changer move");

    let lib_after = changer.library();
    assert!(lib_after.drive_bays[0].loaded);
    assert_eq!(
        lib_after.drive_bays[0].loaded_tape.as_deref(),
        Some("TAPE_A")
    );
    assert_eq!(lib_after.drive_bays[0].source_slot, Some(0x0400));
    assert!(!lib_after.slots[0].full);
    assert!(lib_after.slots[0].cartridge.is_none());

    let move_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0xA5)
        .cloned()
        .collect();
    assert_eq!(move_cdbs.len(), 1, "standalone changer issued MOVE");
}

#[test]
fn changer_handle_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<ChangerHandle>();
}

#[test]
fn move_medium_preflight_refused_emits_refused_no_cdb() {
    // Source is the empty bay 0x0100 — SourceEmpty refuses
    // before any CDB.
    let lib = move_medium_test_lib("LIB_MM02");
    let policy = StaticAllowlist::new(["LIB_MM02"]);
    let (factory, log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/changer-msl-g3.bin").to_vec(),
        vpd80_response("LIB_MM02"),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("open");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    let snapshot_before = handle.library().clone();
    let err = handle
        .move_medium(0x0100, 0x0400, &policy)
        .expect_err("move should refuse");
    assert!(matches!(err, MoveError::SourceEmpty { addr: 0x0100 }));

    // Snapshot unchanged.
    assert_eq!(handle.library(), &snapshot_before);

    // No MOVE MEDIUM CDB issued.
    let move_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0xA5)
        .cloned()
        .collect();
    assert!(
        move_cdbs.is_empty(),
        "MOVE MEDIUM CDB must not be issued on preflight refusal"
    );

    // Exactly one Refused event, no Started / Finished.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    match &events[0] {
        CapturedEvent::Refused { op, reason } => {
            assert_eq!(
                *op,
                AuditOp::Move {
                    src: 0x0100,
                    dst: 0x0400
                }
            );
            assert_eq!(*reason, "SourceEmpty");
        }
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[test]
fn move_medium_refused_when_drive_is_derived_and_policy_disallows() {
    // Demote the drive's identity to Derived. Policy allows the
    // library at the *open* level (we have to opt-in to derived
    // for the open itself to succeed), but we then construct a
    // *second* policy that doesn't opt in to derived for the
    // move_medium call. That triggers the defense-in-depth
    // re-check at move time.
    let mut lib = move_medium_test_lib("LIB_MM03");
    lib.drive_bays[0]
        .installed
        .as_mut()
        .unwrap()
        .identity_source = IdentitySource::Derived;

    let permissive_for_open = StaticAllowlist::new(["LIB_MM03"]).with_derived_allowed("LIB_MM03");
    let strict_for_move = StaticAllowlist::new(["LIB_MM03"]); // no derived opt-in

    let (factory, log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/changer-msl-g3.bin").to_vec(),
        vpd80_response("LIB_MM03"),
    ]);
    let mut handle = lib.open_with(&permissive_for_open, factory).expect("open");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    let err = handle
        .move_medium(0x0400, 0x0100, &strict_for_move)
        .expect_err("derived bay + non-permissive policy must refuse");
    match err {
        MoveError::DerivedDriveBay { addr, serial } => {
            // The drive bay address (0x0100), not the slot (0x0400).
            // The derived bay is the *destination* in this move.
            assert_eq!(addr, 0x0100);
            assert_eq!(serial, "DRV_A");
        }
        other => panic!("expected DerivedDriveBay, got {other:?}"),
    }

    // No MOVE MEDIUM CDB.
    let move_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0xA5)
        .cloned()
        .collect();
    assert!(move_cdbs.is_empty());

    // Audit: single Refused with DerivedDriveBay reason.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        CapturedEvent::Refused {
            op: AuditOp::Move { .. },
            reason: "DerivedDriveBay",
        }
    ));
}

#[test]
fn move_medium_derived_source_reports_bay_addr_not_dest() {
    // Symmetric test: derived bay is the *source* this time
    // (drive → slot unload). Pre-load the bay so it has a tape
    // to unload, and flip the drive's identity source. The
    // resulting error must again carry the bay's address (0x0100),
    // not the destination slot's (0x0400).
    let mut lib = move_medium_test_lib("LIB_MM05");
    lib.drive_bays[0]
        .installed
        .as_mut()
        .unwrap()
        .identity_source = IdentitySource::Derived;
    lib.drive_bays[0].loaded = true;
    lib.drive_bays[0].loaded_tape = Some("ALREADY_IN_BAY".into());
    lib.slots[0].full = false;
    lib.slots[0].cartridge = None;

    let permissive_for_open = StaticAllowlist::new(["LIB_MM05"]).with_derived_allowed("LIB_MM05");
    let strict_for_move = StaticAllowlist::new(["LIB_MM05"]);

    let (factory, _log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/changer-msl-g3.bin").to_vec(),
        vpd80_response("LIB_MM05"),
    ]);
    let mut handle = lib.open_with(&permissive_for_open, factory).expect("open");

    let err = handle
        .move_medium(0x0100, 0x0400, &strict_for_move)
        .expect_err("derived source + non-permissive policy must refuse");
    match err {
        MoveError::DerivedDriveBay { addr, serial } => {
            assert_eq!(addr, 0x0100, "bay address, not destination slot");
            assert_eq!(serial, "DRV_A");
        }
        other => panic!("expected DerivedDriveBay, got {other:?}"),
    }
}

#[test]
fn move_medium_succeeds_when_derived_is_explicitly_allowed() {
    // Same setup as the refused case, but the move-time policy
    // also opts derived in. CDB goes out; snapshot patches.
    let mut lib = move_medium_test_lib("LIB_MM04");
    lib.drive_bays[0]
        .installed
        .as_mut()
        .unwrap()
        .identity_source = IdentitySource::Derived;
    let policy = StaticAllowlist::new(["LIB_MM04"]).with_derived_allowed("LIB_MM04");

    let (factory, log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/changer-msl-g3.bin").to_vec(),
        vpd80_response("LIB_MM04"),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("open");
    handle
        .move_medium(0x0400, 0x0100, &policy)
        .expect("derived + opt-in → move ok");

    let bay = &handle.library().drive_bays[0];
    assert!(bay.loaded);

    let move_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0xA5)
        .cloned()
        .collect();
    assert_eq!(move_cdbs.len(), 1);
}

#[test]
fn open_fails_with_identity_changed_when_revalidation_response_is_malformed() {
    // The new device responds to INQUIRY but with a malformed VPD
    // 0x80 page (wrong page code byte). We treat that as
    // "unconfirmable identity" → IdentityChanged with actual=None.
    let lib = fake_library("LIB006");
    let policy = StaticAllowlist::new(["LIB006"]);
    // Page code 0x83 instead of 0x80 — UnitSerial::parse will reject.
    let mut malformed = vec![0x08u8, 0x83, 0x00, 0x04];
    malformed.extend_from_slice(b"GARB");
    let factory = fixture_factory(vec![changer_inquiry_response(), malformed]);
    let err = lib.open_with(&policy, factory).unwrap_err();
    match err {
        OpenError::IdentityChanged {
            expected, actual, ..
        } => {
            assert_eq!(expected, "LIB006");
            assert!(actual.is_none());
        }
        other => panic!("expected IdentityChanged, got {other:?}"),
    }
}

// =====================================================================
//  refresh — §7.4
// =====================================================================

/// Build a refresh-ready library from the in-tree real-MSL3040
/// full-DVCID RES capture: serial "DEC418146K_LL02", 2 drives, 40
/// slots, 0 IE. Pre-fills drive 0 (bay 0x0001) with sg_path and
/// vendor so the refresh test can confirm those are preserved.
fn real_msl3040_library() -> Library {
    let res = include_bytes!("../../../../fixtures/element-status/real-msl3040-full-dvcid.bin");
    let inq = include_bytes!("../../../../fixtures/inquiry/real/changer-msl3040.bin");
    let vpd80 = include_bytes!("../../../../fixtures/vpd-80/real/changer-msl3040.bin");

    let captures = crate::model::DeviceCaptures {
        changer_inquiry: remanence_scsi::Inquiry::parse(inq).unwrap(),
        unit_serial: remanence_scsi::UnitSerial::parse(vpd80)
            .unwrap()
            .as_str()
            .to_string(),
        device_id: None,
        element_status: remanence_scsi::read_element_status::parse(res).unwrap(),
        changer_sg: PathBuf::from("/dev/sg-mock"),
        changer_sysfs: PathBuf::from("/sys/class/scsi_device/mock"),
    };
    let mut lib = Library::from_captures(captures);

    // Simulate Layer 2a's tape-device join having bound /dev/sg0
    // + vendor for drive 0 (bay 0x0001 in the real capture).
    let bay = lib
        .drive_bays
        .iter_mut()
        .find(|b| b.element_address == 0x0001)
        .expect("bay 0x0001 in real capture");
    let inst = bay.installed.as_mut().expect("DVCID-inline drive");
    inst.identity_source = IdentitySource::DvcidAndInquiry;
    inst.sg_path = Some(PathBuf::from("/dev/sg0"));
    inst.vendor = Some("HPE".into());
    inst.product = Some("Ultrium 9-SCSI".into());
    inst.revision = Some("HH90".into());
    lib
}

#[test]
fn refresh_preserves_host_side_data_when_res_is_unchanged() {
    let lib = real_msl3040_library();
    let policy = StaticAllowlist::new([&lib.serial.clone()]);

    // FixtureTransport responses: 2 open-time responses, then
    // 2 for the refresh (8-byte probe + sized RES).
    let res_full =
        include_bytes!("../../../../fixtures/element-status/real-msl3040-full-dvcid.bin");
    let (factory, _log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/real/changer-msl3040.bin").to_vec(),
        include_bytes!("../../../../fixtures/vpd-80/real/changer-msl3040.bin").to_vec(),
        res_full[..8].to_vec(), // 8-byte probe → byte_count
        res_full.to_vec(),      // sized full read
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("open");

    handle.refresh().expect("refresh ok");

    assert!(
        !handle.is_dirty(),
        "snapshot is clean after successful refresh"
    );

    // Host-side fields preserved on bay 0x0001.
    let bay = handle
        .library()
        .drive_bays
        .iter()
        .find(|b| b.element_address == 0x0001)
        .unwrap();
    let inst = bay.installed.as_ref().unwrap();
    assert_eq!(inst.identity_source, IdentitySource::DvcidAndInquiry);
    assert_eq!(inst.sg_path.as_deref(), Some(Path::new("/dev/sg0")));
    assert_eq!(inst.vendor.as_deref(), Some("HPE"));
}

#[test]
fn refresh_shape_mismatch_sets_dirty_and_returns_ok() {
    // Open with a library that has 2 drives + 40 slots. Then
    // refresh against a synthesised RES that has only 1 drive →
    // shape mismatch → snapshot kept as-is, is_dirty=true, Ok,
    // AND one AuditEvent::Warning(ShapeMismatch) fires (the §5.3
    // soft-error path must still be operator-visible).
    let lib = real_msl3040_library();
    let serial = lib.serial.clone();
    let policy = StaticAllowlist::new([serial.as_str()]);

    // Build a small RES: just one DataTransfer element (no slots,
    // no IE). This will be wildly different shape from the
    // original 2-drive/40-slot snapshot.
    let mismatched = build_synthetic_es_one_drive();

    let (factory, _log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/real/changer-msl3040.bin").to_vec(),
        include_bytes!("../../../../fixtures/vpd-80/real/changer-msl3040.bin").to_vec(),
        mismatched[..8].to_vec(),
        mismatched.clone(),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("open");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);
    let before = handle.library().clone();

    handle
        .refresh()
        .expect("refresh returns Ok on shape mismatch");

    assert!(handle.is_dirty(), "shape mismatch sets is_dirty");
    // Snapshot left as-is — Layer 2b §5.3 contract.
    assert_eq!(handle.library(), &before);

    // §5.3 also requires the shape change be audit-visible.
    let events = audit.lock().unwrap().clone();
    assert_eq!(
        events.len(),
        1,
        "refresh fires exactly one Warning(ShapeMismatch) on the soft-error path"
    );
    match &events[0] {
        CapturedEvent::Warning {
            op: AuditOp::Rescan,
            warning: crate::error::RescanWarning::ShapeMismatch { summary },
        } => {
            assert!(
                summary.contains("differ from prior snapshot"),
                "summary should describe the mismatch, got {summary:?}"
            );
        }
        other => panic!("expected Warning(ShapeMismatch), got {other:?}"),
    }
}

// =====================================================================
//  rescan — §7.5
// =====================================================================

#[test]
fn rescan_happy_path_clears_dirty_and_emits_audit() {
    // Open the handle against the real-MSL3040 full-DVCID
    // capture, then call rescan() with the same capture as the
    // post-init RES. Expect: returns Ok, snapshot reconciled
    // (host-side data preserved on bay 0x0001 since serial
    // matches), is_dirty cleared, audit produces exactly one
    // Started{Move{0x07 INIT}} → Finished{Success} pair.
    let lib = real_msl3040_library();
    let policy = StaticAllowlist::new([&lib.serial.clone()]);

    let res_full =
        include_bytes!("../../../../fixtures/element-status/real-msl3040-full-dvcid.bin");
    let (factory, log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/real/changer-msl3040.bin").to_vec(),
        include_bytes!("../../../../fixtures/vpd-80/real/changer-msl3040.bin").to_vec(),
        // Rescan's post-init RES: two-phase probe (header + sized).
        res_full[..8].to_vec(),
        res_full.to_vec(),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("open");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    handle.rescan().expect("rescan ok");

    assert!(!handle.is_dirty(), "rescan clears is_dirty on success");

    // Host-side data preserved.
    let bay = handle
        .library()
        .drive_bays
        .iter()
        .find(|b| b.element_address == 0x0001)
        .unwrap();
    let inst = bay.installed.as_ref().unwrap();
    assert_eq!(inst.identity_source, IdentitySource::DvcidAndInquiry);
    assert_eq!(inst.sg_path.as_deref(), Some(Path::new("/dev/sg0")));

    // CDB log: 0x12 (INQUIRY x2 — std + VPD 0x80 at open time),
    // 0x07 (INIT), 0xb8 x2 (RES probe + sized). Filter for the
    // INIT opcode — there should be exactly one.
    let init_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0x07)
        .cloned()
        .collect();
    assert_eq!(init_cdbs.len(), 1, "exactly one INIT ELEMENT STATUS CDB");
    assert_eq!(init_cdbs[0], vec![0x07, 0x00, 0x00, 0x00, 0x00, 0x00]);

    // Audit events: Started{op=Rescan, cdb=0x07…} then
    // Finished{op=Rescan, Success}.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    match &events[0] {
        CapturedEvent::Started { op, cdb } => {
            assert_eq!(*op, AuditOp::Rescan);
            assert_eq!(cdb[0], 0x07);
        }
        other => panic!("expected Started, got {other:?}"),
    }
    assert!(matches!(
        events[1],
        CapturedEvent::FinishedSuccess {
            op: AuditOp::Rescan,
            snapshot_patched: true,
        }
    ));
}

#[test]
fn rescan_shape_mismatch_returns_error_and_audits_other() {
    // Open against the real MSL3040 capture (2 drives, 40 slots).
    // Hand rescan a one-drive synthetic RES → shape mismatch →
    // RescanError::SnapshotMismatch. Audit emits Finished{Other}.
    // is_dirty stays false (rescan returns hard error, doesn't
    // mutate the dirty flag — that's refresh's contract).
    let lib = real_msl3040_library();
    let serial = lib.serial.clone();
    let policy = StaticAllowlist::new([serial.as_str()]);

    let mismatched = build_synthetic_es_one_drive();
    let (factory, _log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/real/changer-msl3040.bin").to_vec(),
        include_bytes!("../../../../fixtures/vpd-80/real/changer-msl3040.bin").to_vec(),
        mismatched[..8].to_vec(),
        mismatched.clone(),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("open");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);
    let before = handle.library().clone();

    let err = handle.rescan().expect_err("shape mismatch is hard error");
    match err {
        RescanError::SnapshotMismatch(msg) => {
            assert!(
                msg.contains("differ from prior snapshot"),
                "msg should describe count or address mismatch, got {msg:?}"
            );
        }
        other => panic!("expected SnapshotMismatch, got {other:?}"),
    }

    // Snapshot left unchanged — rescan didn't mutate on the
    // hard-error path.
    assert_eq!(handle.library(), &before);
    // INIT succeeded → the changer's element state was re-derived,
    // so the cached snapshot is definitively stale. is_dirty=true
    // signals this to subsequent callers.
    assert!(
        handle.is_dirty(),
        "INIT-success + shape-mismatch must set is_dirty=true"
    );

    // Audit: Started, then Finished{Other(summary contains "shape mismatch")}.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    match &events[0] {
        CapturedEvent::Started { op, .. } => assert_eq!(*op, AuditOp::Rescan),
        other => panic!("expected Started, got {other:?}"),
    }
    match &events[1] {
        // AuditOutcome::Other maps to CapturedEvent::FinishedScsiError
        // in our test helper (it consolidates ScsiError + Other
        // into one variant).
        CapturedEvent::FinishedScsiError { op, summary } => {
            assert_eq!(*op, AuditOp::Rescan);
            assert!(
                summary.contains("shape mismatch"),
                "summary should name the outcome, got {summary:?}"
            );
        }
        other => panic!("expected Finished with Other outcome, got {other:?}"),
    }
}

#[test]
fn rescan_emits_warning_event_per_reconcile_warning() {
    // Open against the real-MSL3040 capture, then mutate bay
    // 0x0001's installed.serial to something that won't match the
    // RES we'll feed rescan. The reconcile result then carries a
    // DriveReplaced warning for that bay; the handle should fire
    // a corresponding AuditEvent::Warning between Started and
    // Finished.
    let mut lib = real_msl3040_library();
    // Force a serial that the new RES won't match.
    lib.drive_bays[0].installed.as_mut().unwrap().serial = "OLD_BAY1_SERIAL".into();
    let policy = StaticAllowlist::new([&lib.serial.clone()]);

    let res_full =
        include_bytes!("../../../../fixtures/element-status/real-msl3040-full-dvcid.bin");
    let (factory, _log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/real/changer-msl3040.bin").to_vec(),
        include_bytes!("../../../../fixtures/vpd-80/real/changer-msl3040.bin").to_vec(),
        res_full[..8].to_vec(),
        res_full.to_vec(),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("open");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    handle
        .rescan()
        .expect("rescan succeeds; warning is informational");

    // Post-rescan bay 0x0001 now carries the *new* serial
    // (the real one from the capture: 8031BDC7D1).
    let bay = handle
        .library()
        .drive_bays
        .iter()
        .find(|b| b.element_address == 0x0001)
        .unwrap();
    assert_eq!(bay.installed.as_ref().unwrap().serial, "8031BDC7D1");
    assert!(!handle.is_dirty());

    // Audit log: Started, Warning(DriveReplaced for bay 1), Finished.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 3, "expected Started + 1 Warning + Finished");
    assert!(matches!(
        events[0],
        CapturedEvent::Started {
            op: AuditOp::Rescan,
            ..
        }
    ));
    match &events[1] {
        CapturedEvent::Warning {
            op: AuditOp::Rescan,
            warning:
                crate::error::RescanWarning::DriveReplaced {
                    addr: 0x0001,
                    old_serial,
                    new_serial,
                },
        } => {
            assert_eq!(old_serial, "OLD_BAY1_SERIAL");
            assert_eq!(new_serial, "8031BDC7D1");
        }
        other => panic!("expected DriveReplaced Warning event, got {other:?}"),
    }
    assert!(matches!(
        events[2],
        CapturedEvent::FinishedSuccess {
            op: AuditOp::Rescan,
            snapshot_patched: true,
        }
    ));
}

#[test]
fn refresh_also_emits_warning_event_per_reconcile_warning() {
    // refresh() doesn't fire Started/Finished (read-only), but
    // reconciliation warnings still route to the audit log per
    // §5.2. The audit log should contain exactly one Warning
    // event after the refresh completes.
    let mut lib = real_msl3040_library();
    lib.drive_bays[0].installed.as_mut().unwrap().serial = "OLD_BAY1_SERIAL".into();
    let policy = StaticAllowlist::new([&lib.serial.clone()]);

    let res_full =
        include_bytes!("../../../../fixtures/element-status/real-msl3040-full-dvcid.bin");
    let (factory, _log) = recording_factory(vec![
        include_bytes!("../../../../fixtures/inquiry/real/changer-msl3040.bin").to_vec(),
        include_bytes!("../../../../fixtures/vpd-80/real/changer-msl3040.bin").to_vec(),
        res_full[..8].to_vec(),
        res_full.to_vec(),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("open");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    handle.refresh().expect("refresh ok");
    assert!(!handle.is_dirty());

    let events = audit.lock().unwrap().clone();
    assert_eq!(
        events.len(),
        1,
        "refresh fires only Warning events, no Started/Finished"
    );
    assert!(matches!(
        &events[0],
        CapturedEvent::Warning {
            op: AuditOp::Rescan,
            warning: crate::error::RescanWarning::DriveReplaced { addr: 0x0001, .. },
        }
    ));
}

// =====================================================================
//  §7.6 — open_drive + DriveHandle
// =====================================================================

use crate::error::DriveOpError;
use std::collections::HashMap;

/// Multi-path test factory: hands out a different
/// `RecordingTransport<FixtureTransport>` per `/dev/sgN`, all
/// sharing one CDB log. Used to drive `open_drive` tests where
/// the library handle's first open consumes the changer's
/// scripted responses and a later `open_drive` call consumes a
/// separate drive's responses.
#[allow(clippy::type_complexity)]
fn multi_recording_factory(
    scripts: Vec<(PathBuf, Vec<Vec<u8>>)>,
) -> (
    Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>>,
    RecordingLog,
) {
    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut bag: HashMap<PathBuf, FixtureTransport> = scripts
        .into_iter()
        .map(|(p, r)| (p, FixtureTransport::new().with_responses(r)))
        .collect();
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            let inner = bag.remove(path).ok_or_else(|| IoErrorKind {
                kind: "NotFound",
                message: format!("no fixture transport seeded for {path:?}"),
                raw_os_error: None,
            })?;
            let wrapped = RecordingTransport::with_log(inner, log_cl.clone());
            Ok(Box::new(wrapped) as Box<dyn SgTransport>)
        });
    (factory, log)
}

/// Build a fake library with a single drive bay at 0x0100 that
/// already has a bound sg_path — suitable for open_drive tests.
/// Drive serial defaults to "DRV_A".
fn open_drive_test_lib(library_serial: &str) -> Library {
    let mut lib = move_medium_test_lib(library_serial);
    let inst = lib.drive_bays[0].installed.as_mut().unwrap();
    inst.sg_path = Some(PathBuf::from("/dev/sg-drive-mock"));
    lib
}

fn lto9_inquiry() -> Vec<u8> {
    include_bytes!("../../../../fixtures/inquiry/drive1-lto9.bin").to_vec()
}

#[test]
fn open_drive_succeeds_for_resolved_bay() {
    let lib = open_drive_test_lib("LIB_OD01");
    let policy = StaticAllowlist::new(["LIB_OD01"]);
    let (factory, _log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_OD01")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A")],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let drive = handle.open_drive(0x0100, &policy).expect("drive opens");
    assert_eq!(drive.bay_address(), 0x0100);
    assert_eq!(drive.drive().serial, "DRV_A");
    assert_eq!(drive.library_serial(), "LIB_OD01");
}

#[test]
fn two_drives_open_simultaneously() {
    let mut lib = open_drive_test_lib("LIB_OD10");
    lib.layout.drive_count = 2;
    lib.drive_bays.push(DriveBay {
        element_address: 0x0101,
        installed: Some(InstalledDrive {
            serial: "DRV_B".into(),
            identity_source: IdentitySource::DvcidAndInquiry,
            vendor: None,
            product: None,
            revision: None,
            sg_path: Some(PathBuf::from("/dev/sg-drive-b-mock")),
            sysfs_path: None,
        }),
        loaded: false,
        loaded_tape: None,
        source_slot: None,
    });

    let policy = StaticAllowlist::new(["LIB_OD10"]);
    let (factory, _log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_OD10")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A")],
        ),
        (
            PathBuf::from("/dev/sg-drive-b-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_B")],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let drive_a = handle.open_drive(0x0100, &policy).expect("drive A opens");
    let drive_b = handle.open_drive(0x0101, &policy).expect("drive B opens");

    assert_eq!(drive_a.bay_address(), 0x0100);
    assert_eq!(drive_a.drive().serial, "DRV_A");
    assert_eq!(drive_b.bay_address(), 0x0101);
    assert_eq!(drive_b.drive().serial, "DRV_B");
    assert!(!handle.is_dirty());
}

#[test]
fn open_drive_refused_when_policy_does_not_allow_library() {
    // Open the LibraryHandle under a permissive policy, then call
    // open_drive with a *stricter* policy that doesn't allow this
    // library. open_drive must refuse with NotAllowed BEFORE
    // touching the drive transport. The factory has no entry for
    // the drive path; if open_drive incorrectly proceeded past
    // the policy check, the factory would fail with
    // DeviceUnavailable (NotFound), which is a different variant
    // — this test pins that the policy check fires first.
    let lib = open_drive_test_lib("LIB_OD09");
    let permissive = StaticAllowlist::new(["LIB_OD09"]);
    let denying = StaticAllowlist::new(["SOMEONE_ELSE"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        vec![changer_inquiry_response(), vpd80_response("LIB_OD09")],
    )]);
    let mut handle = lib
        .open_with(&permissive, factory)
        .expect("library opens under permissive policy");

    let err = handle.open_drive(0x0100, &denying).unwrap_err();
    match err {
        OpenError::NotAllowed { serial } => assert_eq!(serial, "LIB_OD09"),
        other => panic!("expected NotAllowed, got {other:?}"),
    }
}

#[test]
fn open_drive_refused_for_unknown_bay_address() {
    let lib = open_drive_test_lib("LIB_OD02");
    let policy = StaticAllowlist::new(["LIB_OD02"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        vec![changer_inquiry_response(), vpd80_response("LIB_OD02")],
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = handle.open_drive(0x9999, &policy).unwrap_err();
    match err {
        OpenError::BayNotFound { addr } => assert_eq!(addr, 0x9999),
        other => panic!("expected BayNotFound, got {other:?}"),
    }
}

#[test]
fn open_drive_refused_when_bay_is_unresolved() {
    let mut lib = open_drive_test_lib("LIB_OD03");
    lib.drive_bays[0].installed = None;
    let policy = StaticAllowlist::new(["LIB_OD03"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        vec![changer_inquiry_response(), vpd80_response("LIB_OD03")],
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = handle.open_drive(0x0100, &policy).unwrap_err();
    match err {
        OpenError::BayUnresolved { addr } => assert_eq!(addr, 0x0100),
        other => panic!("expected BayUnresolved, got {other:?}"),
    }
}

#[test]
fn open_drive_refused_when_bay_has_no_sg_path() {
    let mut lib = open_drive_test_lib("LIB_OD04");
    // Resolved identity (installed.is_some()) but no /dev/sgN bound.
    lib.drive_bays[0].installed.as_mut().unwrap().sg_path = None;
    let policy = StaticAllowlist::new(["LIB_OD04"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        vec![changer_inquiry_response(), vpd80_response("LIB_OD04")],
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = handle.open_drive(0x0100, &policy).unwrap_err();
    match err {
        OpenError::BayMissingDevice { addr, serial } => {
            assert_eq!(addr, 0x0100);
            assert_eq!(serial, "DRV_A");
        }
        other => panic!("expected BayMissingDevice, got {other:?}"),
    }
}

#[test]
fn open_drive_refused_on_drive_identity_mismatch() {
    // Drive's VPD 0x80 returns a serial that doesn't match the
    // snapshot's installed.serial — kernel re-enumeration swapped
    // a different drive into the path since discovery.
    let lib = open_drive_test_lib("LIB_OD05");
    let policy = StaticAllowlist::new(["LIB_OD05"]);
    let (factory, _log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_OD05")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_DIFFERENT")],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = handle.open_drive(0x0100, &policy).unwrap_err();
    match err {
        OpenError::IdentityChanged {
            expected, actual, ..
        } => {
            assert_eq!(expected, "DRV_A");
            assert_eq!(actual.as_deref(), Some("DRV_DIFFERENT"));
        }
        other => panic!("expected IdentityChanged, got {other:?}"),
    }
}

#[test]
fn open_drive_refused_when_device_is_not_sequential_access() {
    // The device behind /dev/sg-drive-mock now responds as a
    // medium changer (or anything not SequentialAccess). Refuse
    // before VPD 0x80 is even asked for.
    let lib = open_drive_test_lib("LIB_OD06");
    let policy = StaticAllowlist::new(["LIB_OD06"]);
    let (factory, _log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_OD06")],
        ),
        (
            // Drive path returns a changer-class INQUIRY by mistake.
            PathBuf::from("/dev/sg-drive-mock"),
            vec![changer_inquiry_response()],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = handle.open_drive(0x0100, &policy).unwrap_err();
    assert!(
        matches!(err, OpenError::IdentityChanged { actual: None, .. }),
        "expected IdentityChanged{{actual=None}}, got {err:?}"
    );
}

#[test]
fn drive_handle_unload_issues_correct_cdb_and_audits() {
    let lib = open_drive_test_lib("LIB_OD07");
    let policy = StaticAllowlist::new(["LIB_OD07"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_OD07")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A")],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.unload().expect("unload ok");
    }

    // Exactly one 0x1B CDB with byte 4 = 0x00 (UNLOAD).
    let lu_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0x1B)
        .cloned()
        .collect();
    assert_eq!(lu_cdbs.len(), 1);
    assert_eq!(lu_cdbs[0], vec![0x1B, 0x00, 0x00, 0x00, 0x00, 0x00]);

    // Audit: Started{DriveUnload{0x0100}} + FinishedSuccess.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[0],
        CapturedEvent::Started {
            op: AuditOp::DriveUnload { bay: 0x0100 },
            cdb,
        } if cdb[0] == 0x1B && cdb[4] == 0x00
    ));
    assert!(matches!(
        events[1],
        CapturedEvent::FinishedSuccess {
            op: AuditOp::DriveUnload { bay: 0x0100 },
            snapshot_patched: false,
        }
    ));
}

#[test]
fn drive_handle_load_issues_correct_cdb_and_audits() {
    let lib = open_drive_test_lib("LIB_OD08");
    let policy = StaticAllowlist::new(["LIB_OD08"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_OD08")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A")],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.load().expect("load ok");
    }

    let lu_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0x1B)
        .cloned()
        .collect();
    assert_eq!(lu_cdbs.len(), 1);
    assert_eq!(lu_cdbs[0], vec![0x1B, 0x00, 0x00, 0x00, 0x01, 0x00]);

    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[0],
        CapturedEvent::Started {
            op: AuditOp::DriveLoad { bay: 0x0100 },
            cdb,
        } if cdb[0] == 0x1B && cdb[4] == 0x01
    ));
    assert!(matches!(
        events[1],
        CapturedEvent::FinishedSuccess {
            op: AuditOp::DriveLoad { bay: 0x0100 },
            snapshot_patched: false,
        }
    ));

    // Suppress dead-code warning on DriveOpError when no failure
    // test exercises it — the From<ScsiError> impl is reachable
    // through DriveHandle::issue_load_unload's error path.
    let _: fn(ScsiError) -> DriveOpError = DriveOpError::from;
}

#[cfg(target_os = "linux")]
#[test]
fn drive_handle_load_transport_error_marks_parent_dirty() {
    // Step 9.1d (codex 97997d71) closed the TODO at the old
    // handle.rs:1308 that said the DriveHandle couldn't flip
    // the parent LibraryHandle's dirty bit. Now it can: a
    // direct DriveHandle::load() failing with a completion-
    // ambiguous transport error must leave the parent handle
    // with is_dirty() == true and dirty_cause() ==
    // CompletionUnknown — same signal the composed
    // LibraryHandle::load already produces.
    let lib = open_drive_test_lib("LIB_OD09");
    let policy = StaticAllowlist::new(["LIB_OD09"]);

    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_OD09")]);
    let mut drive_slot = Some(vec![lto9_inquiry(), vpd80_response("DRV_A")]);
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                let inner = FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                );
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                Ok(Box::new(recorded) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                // Drive's transport: identity-revalidation
                // responses succeed, then any state-changing
                // execute_none (the SSC LOAD) hits the fault
                // wrapper and returns TransportError.
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                let faulted = FailFirstNoneWithTransportError::new(recorded);
                Ok(Box::new(faulted) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    assert!(!handle.is_dirty(), "fresh handle is not dirty");

    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        let err = drive.load().expect_err("SSC LOAD must fail at transport");
        match err {
            DriveOpError::ScsiError(ScsiError::TransportError {
                driver_status: 0x06,
                ..
            }) => {}
            other => panic!("expected DriveOpError::ScsiError(TransportError), got {other:?}"),
        }
    }

    assert!(
        handle.is_dirty(),
        "direct DriveHandle::load() transport error must flip parent dirty bit"
    );
    assert_eq!(
        handle.dirty_cause(),
        Some(DirtyCause::CompletionUnknown),
        "completion-unknown transport error must categorise as CompletionUnknown"
    );

    // The SSC LOAD CDB (0x1B) went out via the drive's transport.
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(
        opcodes.contains(&0x1B),
        "SSC LOAD CDB went out: {opcodes:?}"
    );
}

// =====================================================================
//  Layer 3a Step 9.4 — DriveHandle::rewind + DriveHandle::position
// =====================================================================

use crate::handle::tape_io::TapeIoError;

/// Build a 32-byte READ POSITION long-form response. `lba`
/// goes into bytes 8..16; `flags` is byte 0; `partition` is a
/// u32 big-endian at bytes 4..8 per IBM Table 99 (codex 19:57
/// fix).
fn rp_long_response(flags: u8, partition: u32, lba: u64) -> Vec<u8> {
    let mut v = vec![0u8; 32];
    v[0] = flags;
    v[4..8].copy_from_slice(&partition.to_be_bytes());
    v[8..16].copy_from_slice(&lba.to_be_bytes());
    v
}

#[test]
fn drive_handle_rewind_emits_cdb_and_audits_success() {
    let lib = open_drive_test_lib("LIB_RW01");
    let policy = StaticAllowlist::new(["LIB_RW01"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_RW01")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A")],
        ),
    ]);
    let (hook, captured) = capture_audit();
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    handle.set_audit_hook(Box::new(hook));

    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.rewind().expect("REWIND ok");
    }

    // The REWIND CDB (0x01) went out via the drive's transport.
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(opcodes.contains(&0x01), "REWIND CDB went out: {opcodes:?}");

    // Snapshot is clean — REWIND completed cleanly.
    assert!(
        !handle.is_dirty(),
        "successful REWIND leaves snapshot clean"
    );

    // Audit emitted Started + FinishedSuccess for TapeRewind.
    let events = captured.lock().unwrap();
    let tape_events: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                CapturedEvent::Started {
                    op: AuditOp::TapeRewind { bay: 0x0100 },
                    ..
                } | CapturedEvent::FinishedSuccess {
                    op: AuditOp::TapeRewind { bay: 0x0100 },
                    ..
                }
            )
        })
        .collect();
    assert_eq!(tape_events.len(), 2, "Started + Finished: {tape_events:?}");
    assert!(matches!(
        &tape_events[1],
        CapturedEvent::FinishedSuccess {
            snapshot_patched: false,
            ..
        }
    ));
}

#[test]
fn drive_handle_rewind_transport_error_marks_parent_dirty() {
    let lib = open_drive_test_lib("LIB_RW02");
    let policy = StaticAllowlist::new(["LIB_RW02"]);

    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_RW02")]);
    let mut drive_slot = Some(vec![lto9_inquiry(), vpd80_response("DRV_A")]);
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                let inner = FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                );
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                Ok(Box::new(recorded) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                // REWIND is a no-data CDB → execute_none. Force
                // it to surface a transport error so we exercise
                // the dirty-flip path.
                let faulted = FailFirstNoneWithTransportError::new(recorded);
                Ok(Box::new(faulted) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    assert!(!handle.is_dirty());

    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        let err = drive.rewind().expect_err("REWIND must fail at transport");
        assert!(
            matches!(err, TapeIoError::Transport(_)),
            "expected Transport variant, got {err:?}"
        );
    }

    assert!(
        handle.is_dirty(),
        "REWIND transport error must flip parent dirty bit"
    );
    assert_eq!(handle.dirty_cause(), Some(DirtyCause::CompletionUnknown));

    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(opcodes.contains(&0x01), "REWIND CDB went out: {opcodes:?}");
}

#[test]
fn drive_handle_position_returns_parsed_lba() {
    let lib = open_drive_test_lib("LIB_RP01");
    let policy = StaticAllowlist::new(["LIB_RP01"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_RP01")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![
                lto9_inquiry(),
                vpd80_response("DRV_A"),
                // READ POSITION long-form response: BPEW=1, LBA=0xCAFEBABE.
                rp_long_response(0b0000_0001, 0, 0xCAFE_BABE),
            ],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let pos = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.position().expect("READ POSITION ok")
    };
    assert_eq!(pos.lba, 0xCAFE_BABE);
    assert!(pos.block_position_end_of_warning);
    assert!(!pos.beginning_of_partition);
    assert!(
        !handle.is_dirty(),
        "read-only position leaves snapshot clean"
    );

    // The READ POSITION CDB (0x34) went out.
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(
        opcodes.contains(&0x34),
        "READ POSITION CDB went out: {opcodes:?}"
    );
}

#[test]
fn drive_handle_position_maps_no_medium_from_sense() {
    // Wrap a fixture transport so that *the third* execute_in
    // (the READ POSITION) returns CHECK CONDITION with NOT-READY
    // sense. The first two execute_ins (INQUIRY + VPD 0x80) are
    // the drive identity revalidation and must succeed.
    struct FailNthInWithNotReady<T: SgTransport> {
        inner: T,
        count: usize,
        fail_at: usize,
    }
    impl<T: SgTransport> SgTransport for FailNthInWithNotReady<T> {
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            self.count += 1;
            if self.count == self.fail_at {
                // Don't forward — synthesise NOT READY sense.
                let mut sense = vec![0u8; 32];
                sense[0] = 0x70;
                sense[2] = 0x02; // NOT READY
                sense[7] = 24;
                sense[12] = 0x3A; // MEDIUM NOT PRESENT
                sense[13] = 0x00;
                return Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                });
            }
            self.inner.execute_in(cdb, buf)
        }
        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
            self.inner.execute_none(cdb)
        }
        fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
            self.inner.execute_out(cdb, buf)
        }
        fn set_timeout_for(&mut self, class: TimeoutClass) {
            self.inner.set_timeout_for(class);
        }
    }

    let lib = open_drive_test_lib("LIB_RP02");
    let policy = StaticAllowlist::new(["LIB_RP02"]);

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_RP02")]);
    let mut drive_slot = Some(vec![lto9_inquiry(), vpd80_response("DRV_A")]);
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                let inner = FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                );
                Ok(Box::new(inner) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                // 3rd execute_in fails — that's the READ POSITION
                // (the first two are open-time identity reads).
                let wrapped = FailNthInWithNotReady {
                    inner,
                    count: 0,
                    fail_at: 3,
                };
                Ok(Box::new(wrapped) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        let err = drive.position().expect_err("READ POSITION must fail");
        assert!(
            matches!(err, TapeIoError::NoMedium),
            "expected NoMedium, got {err:?}"
        );
    }

    // NoMedium is NOT a transport error — snapshot stays clean.
    assert!(
        !handle.is_dirty(),
        "NoMedium error must not mark snapshot dirty"
    );
}

// =====================================================================
//  Layer 3a Step 9.5 — DriveHandle::locate + DriveHandle::space
// =====================================================================

use crate::handle::tape_io::SpaceKind;

#[test]
fn drive_handle_locate_seeks_and_returns_position() {
    let lib = open_drive_test_lib("LIB_LC01");
    let policy = StaticAllowlist::new(["LIB_LC01"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_LC01")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![
                lto9_inquiry(),
                vpd80_response("DRV_A"),
                rp_long_response(0, 0, 0x4242),
            ],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let pos = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.locate(0x4242).expect("LOCATE ok")
    };
    assert_eq!(pos.lba, 0x4242);
    assert!(!handle.is_dirty());

    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(
        opcodes.windows(2).any(|w| w == [0x92, 0x34]),
        "LOCATE then READ POSITION: {opcodes:?}"
    );
}

#[test]
fn drive_handle_space_short_count_uses_space6_and_returns_full_count() {
    let lib = open_drive_test_lib("LIB_SP01");
    let policy = StaticAllowlist::new(["LIB_SP01"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_SP01")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![
                lto9_inquiry(),
                vpd80_response("DRV_A"),
                rp_long_response(0, 0, 100),
            ],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let result = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.space(100, SpaceKind::Blocks).expect("SPACE ok")
    };
    assert_eq!(result.units_traversed, 100);
    assert!(!result.stopped_at_boundary);
    assert_eq!(result.position_after.lba, 100);

    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(
        opcodes.contains(&0x11),
        "SPACE(6) opcode 0x11 went out: {opcodes:?}"
    );
    assert!(
        !opcodes.contains(&0x91),
        "SPACE(16) NOT used for short count: {opcodes:?}"
    );
}

#[test]
fn drive_handle_space_long_count_uses_space16() {
    let lib = open_drive_test_lib("LIB_SP02");
    let policy = StaticAllowlist::new(["LIB_SP02"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_SP02")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![
                lto9_inquiry(),
                vpd80_response("DRV_A"),
                rp_long_response(0, 0, 9_000_000),
            ],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let result = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.space(9_000_000, SpaceKind::Blocks).expect("SPACE ok")
    };
    assert_eq!(result.units_traversed, 9_000_000);

    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(
        opcodes.contains(&0x91),
        "SPACE(16) opcode 0x91 went out: {opcodes:?}"
    );
}

#[test]
fn drive_handle_space_early_stop_at_filemark_returns_boundary_signal() {
    struct EarlyStopOnSpace<T: SgTransport> {
        inner: T,
    }
    impl<T: SgTransport> SgTransport for EarlyStopOnSpace<T> {
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            self.inner.execute_in(cdb, buf)
        }
        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
            if cdb[0] == 0x11 || cdb[0] == 0x91 {
                let mut sense = vec![0u8; 32];
                sense[0] = 0x80 | 0x70;
                sense[2] = 0x00;
                sense[3..7].copy_from_slice(&3u32.to_be_bytes());
                sense[7] = 24;
                Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                })
            } else {
                self.inner.execute_none(cdb)
            }
        }
        fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
            self.inner.execute_out(cdb, buf)
        }
        fn set_timeout_for(&mut self, class: TimeoutClass) {
            self.inner.set_timeout_for(class);
        }
    }

    let lib = open_drive_test_lib("LIB_SP03");
    let policy = StaticAllowlist::new(["LIB_SP03"]);

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_SP03")]);
    let mut drive_slot = Some(vec![
        lto9_inquiry(),
        vpd80_response("DRV_A"),
        rp_long_response(0, 0, 700),
    ]);
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                let inner = FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                );
                Ok(Box::new(inner) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                Ok(Box::new(EarlyStopOnSpace { inner }) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let result = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive
            .space(10, SpaceKind::Filemarks)
            .expect("SPACE early-stop is success-with-boundary")
    };
    assert_eq!(result.units_traversed, 7, "10 requested - 3 residual = 7");
    assert!(result.stopped_at_boundary);
    assert_eq!(result.position_after.lba, 700);
    assert!(!handle.is_dirty(), "boundary stop is not a transport error");
}

#[test]
fn drive_handle_space_rejects_sequential_filemarks() {
    // IBM LTO drives only implement SPACE codes 0 / 1 / 3.
    // CODE 2 (SequentialFilemarks) is reserved — Layer 3a
    // rejects it at the API boundary (codex 20:00 catch).
    let lib = open_drive_test_lib("LIB_SP04");
    let policy = StaticAllowlist::new(["LIB_SP04"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_SP04")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A")],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        let err = drive
            .space(5, SpaceKind::SequentialFilemarks)
            .expect_err("SequentialFilemarks must be refused");
        match err {
            TapeIoError::InvalidRequest(ScsiError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("SequentialFilemarks"),
                    "error mentions the kind: {msg}"
                );
            }
            other => panic!("expected InvalidRequest(InvalidInput), got {other:?}"),
        }
    }

    // The SPACE CDBs (0x11 / 0x91) must NOT have gone out — the
    // rejection happens before the transport call.
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(
        !opcodes.contains(&0x11) && !opcodes.contains(&0x91),
        "no SPACE CDB on rejection: {opcodes:?}"
    );
}

#[test]
fn drive_handle_locate_transport_error_marks_parent_dirty() {
    let lib = open_drive_test_lib("LIB_LC02");
    let policy = StaticAllowlist::new(["LIB_LC02"]);

    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_LC02")]);
    let mut drive_slot = Some(vec![lto9_inquiry(), vpd80_response("DRV_A")]);
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                let inner = FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                );
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                Ok(Box::new(recorded) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                Ok(Box::new(FailFirstNoneWithTransportError::new(recorded))
                    as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        let err = drive.locate(42).expect_err("LOCATE must fail at transport");
        assert!(
            matches!(err, TapeIoError::Transport(_)),
            "expected Transport, got {err:?}"
        );
    }
    assert!(handle.is_dirty());
    assert_eq!(handle.dirty_cause(), Some(DirtyCause::CompletionUnknown));
}

// =====================================================================
//  Layer 3a Step 9.6 — DriveHandle::read_block
// =====================================================================

/// FixtureTransport-like helper that returns custom bytes for
/// the first execute_in CDB matching `opcode`, then forwards.
/// Used to seed READ(6) responses with arbitrary byte content.
struct InjectBytesForOpcode<T: SgTransport> {
    inner: T,
    opcode: u8,
    payload: Option<Vec<u8>>,
}
impl<T: SgTransport> SgTransport for InjectBytesForOpcode<T> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        if cdb[0] == self.opcode && self.payload.is_some() {
            let p = self.payload.take().unwrap();
            let n = p.len().min(buf.len());
            buf[..n].copy_from_slice(&p[..n]);
            return Ok(TransferOutcome::clean(n as u32));
        }
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

/// Helper that fails the first READ(6) execute_in with a
/// custom CHECK CONDITION (sense + bytes_transferred), then
/// forwards subsequent calls.
struct FailFirstReadWithCheckCondition<T: SgTransport> {
    inner: T,
    sense: Option<Vec<u8>>,
    bytes_transferred: u32,
}
impl<T: SgTransport> SgTransport for FailFirstReadWithCheckCondition<T> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        if cdb[0] == 0x08 && self.sense.is_some() {
            let sense = self.sense.take().unwrap();
            return Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred: self.bytes_transferred,
            });
        }
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

#[test]
fn drive_handle_read_block_happy_path_returns_full_count() {
    let lib = open_drive_test_lib("LIB_RD01");
    let policy = StaticAllowlist::new(["LIB_RD01"]);

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_RD01")]);
    let mut drive_slot = Some(vec![lto9_inquiry(), vpd80_response("DRV_A")]);
    // Build the READ response payload: 1024 bytes of incrementing
    // pattern so the test can verify content.
    let payload: Vec<u8> = (0..1024).map(|i| (i & 0xFF) as u8).collect();
    let payload_clone = payload.clone();
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                Ok(Box::new(InjectBytesForOpcode {
                    inner,
                    opcode: 0x08,
                    payload: Some(payload_clone.clone()),
                }) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let mut buf = vec![0u8; 1024];
    let n = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.read_block(&mut buf).expect("READ ok")
    };
    assert_eq!(n, 1024);
    assert_eq!(&buf[..16], &payload[..16]);
    assert!(!handle.is_dirty());
}

#[test]
fn drive_handle_read_block_short_read_via_ili_positive_information() {
    // Variable-block: host buffer 4096, on-tape block 1024.
    // Drive raises CHECK CONDITION with VALID + ILI +
    // INFORMATION = +3072 (requested - actual). read_block must
    // return Ok(1024), NOT ReadBufferTooSmall.
    let lib = open_drive_test_lib("LIB_RD02");
    let policy = StaticAllowlist::new(["LIB_RD02"]);

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_RD02")]);
    let mut drive_slot = Some(vec![lto9_inquiry(), vpd80_response("DRV_A")]);
    let mut sense = vec![0u8; 32];
    sense[0] = 0x80 | 0x70;
    sense[2] = 0x20; // ILI
    sense[3..7].copy_from_slice(&3072u32.to_be_bytes());
    sense[7] = 24;
    let sense_clone = sense.clone();
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                Ok(Box::new(FailFirstReadWithCheckCondition {
                    inner,
                    sense: Some(sense_clone.clone()),
                    bytes_transferred: 1024,
                }) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let mut buf = vec![0u8; 4096];
    let n = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.read_block(&mut buf).expect("short read is Ok")
    };
    assert_eq!(n, 1024, "actual = 4096 - 3072 = 1024");
    assert!(!handle.is_dirty());
}

#[test]
fn drive_handle_read_block_buffer_too_small_via_ili_negative_information() {
    // Variable-block: host buffer 1024, on-tape block 65536.
    // Drive raises CHECK CONDITION with VALID + ILI +
    // INFORMATION = -64512 (requested 1024 - actual 65536).
    // read_block must return ReadBufferTooSmall{ actual: 65536,
    // provided: 1024 }; snapshot stays clean.
    let lib = open_drive_test_lib("LIB_RD03");
    let policy = StaticAllowlist::new(["LIB_RD03"]);

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_RD03")]);
    let mut drive_slot = Some(vec![lto9_inquiry(), vpd80_response("DRV_A")]);
    let mut sense = vec![0u8; 32];
    sense[0] = 0x80 | 0x70;
    sense[2] = 0x20; // ILI
    sense[3..7].copy_from_slice(&(-64_512i32 as u32).to_be_bytes());
    sense[7] = 24;
    let sense_clone = sense.clone();
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                Ok(Box::new(FailFirstReadWithCheckCondition {
                    inner,
                    sense: Some(sense_clone.clone()),
                    bytes_transferred: 1024,
                }) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let mut buf = vec![0u8; 1024];
    let err = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.read_block(&mut buf).expect_err("buffer too small")
    };
    match err {
        TapeIoError::ReadBufferTooSmall { actual, provided } => {
            assert_eq!(actual, 65_536);
            assert_eq!(provided, 1024);
        }
        other => panic!("expected ReadBufferTooSmall, got {other:?}"),
    }
    assert!(
        !handle.is_dirty(),
        "ReadBufferTooSmall is not a transport error"
    );
}

#[test]
fn drive_handle_read_block_filemark_is_structured_boundary() {
    let lib = open_drive_test_lib("LIB_RD04");
    let policy = StaticAllowlist::new(["LIB_RD04"]);

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_RD04")]);
    let mut drive_slot = Some(vec![lto9_inquiry(), vpd80_response("DRV_A")]);
    let mut sense = vec![0u8; 32];
    sense[0] = 0x80 | 0x70; // VALID + fixed-format current
    sense[2] = 0x80; // FILEMARK + NO SENSE
    sense[7] = 24;
    sense[12] = 0x00;
    sense[13] = 0x01;
    let sense_clone = sense.clone();

    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                Ok(Box::new(FailFirstReadWithCheckCondition {
                    inner,
                    sense: Some(sense_clone.clone()),
                    bytes_transferred: 0,
                }) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        let mut buf = vec![0u8; 1024];
        drive.read_block(&mut buf).expect_err("filemark boundary")
    };

    assert!(matches!(err, TapeIoError::FilemarkEncountered));
    assert!(
        !handle.is_dirty(),
        "READ filemark is a known boundary, not completion unknown"
    );
}

#[test]
fn drive_handle_rejects_client_side_cdb_bounds_before_dispatch() {
    let lib = open_drive_test_lib("LIB_LIMIT01");
    let policy = StaticAllowlist::new(["LIB_LIMIT01"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_LIMIT01")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A")],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        let mut oversized_buf =
            vec![0u8; remanence_scsi::read_write::MAX_TRANSFER_LEN as usize + 1];

        assert_invalid_request_contains(
            drive
                .read_block(&mut oversized_buf)
                .expect_err("oversized READ buffer must be refused"),
            "READ(6)",
        );
        assert_invalid_request_contains(
            drive
                .write_block(&oversized_buf)
                .expect_err("oversized WRITE buffer must be refused"),
            "WRITE(6)",
        );
        assert_invalid_request_contains(
            drive
                .write_block_unpositioned(&oversized_buf)
                .expect_err("oversized unpositioned WRITE buffer must be refused"),
            "WRITE(6)",
        );
        assert_invalid_request_contains(
            drive
                .write_filemarks(remanence_scsi::write_filemarks::WRITE_FILEMARKS_6_MAX + 1)
                .expect_err("oversized WRITE FILEMARKS count must be refused"),
            "WRITE FILEMARKS(6)",
        );
    }

    let logged = log.borrow();
    assert!(
        !logged
            .iter()
            .any(|cdb| matches!(cdb.first().copied(), Some(0x08 | 0x0A | 0x10))),
        "limit failures must not dispatch tape data/filemark CDBs: {logged:?}"
    );
    assert!(
        !handle.is_dirty(),
        "client-side request validation is not a completion-unknown event"
    );
}

fn assert_invalid_request_contains(err: TapeIoError, needle: &str) {
    match err {
        TapeIoError::InvalidRequest(ScsiError::InvalidInput(msg)) => {
            assert!(msg.contains(needle), "{msg}");
        }
        other => panic!("expected InvalidRequest containing {needle:?}, got {other:?}"),
    }
}

// =====================================================================
//  Layer 3a Step 9.7 — DriveHandle::write_block + write_filemarks
// =====================================================================

/// Inject a CHECK CONDITION on the first WRITE(6) execute_out
/// (CDB 0x0A), then forward. Used to simulate near-EOM signals.
struct FailFirstWriteWithCheckCondition<T: SgTransport> {
    inner: T,
    sense: Option<Vec<u8>>,
    bytes_transferred: u32,
}
impl<T: SgTransport> SgTransport for FailFirstWriteWithCheckCondition<T> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        if cdb[0] == 0x0A && self.sense.is_some() {
            let sense = self.sense.take().unwrap();
            return Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred: self.bytes_transferred,
            });
        }
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

#[test]
fn drive_handle_write_block_happy_path() {
    let lib = open_drive_test_lib("LIB_WR01");
    let policy = StaticAllowlist::new(["LIB_WR01"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_WR01")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![
                lto9_inquiry(),
                vpd80_response("DRV_A"),
                rp_long_response(0, 0, 1),
            ],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let payload = vec![0xAAu8; 1_048_576]; // 1 MiB
    let outcome = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.write_block(&payload).expect("WRITE ok")
    };
    assert_eq!(outcome.bytes_written, 1_048_576);
    assert!(!outcome.early_warning);
    assert!(!outcome.end_of_medium);
    assert_eq!(outcome.position_after.lba, 1);

    // CDB log: 0x0A WRITE, then 0x34 READ POSITION.
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(opcodes.windows(2).any(|w| w == [0x0A, 0x34]));
    assert!(!handle.is_dirty());
}

#[test]
fn drive_handle_write_block_near_eom_returns_early_warning() {
    let lib = open_drive_test_lib("LIB_WR02");
    let policy = StaticAllowlist::new(["LIB_WR02"]);

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_WR02")]);
    let mut drive_slot = Some(vec![
        lto9_inquiry(),
        vpd80_response("DRV_A"),
        rp_long_response(0b0000_0001, 0, 999_999), // BPEW + LBA
    ]);
    // Sense: NO SENSE (key=0) + EOM bit set.
    let mut sense = vec![0u8; 32];
    sense[0] = 0x70;
    sense[2] = 0x40; // EOM bit, key=0
    sense[7] = 24;
    let sense_clone = sense.clone();
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                Ok(Box::new(FailFirstWriteWithCheckCondition {
                    inner,
                    sense: Some(sense_clone.clone()),
                    bytes_transferred: 1024,
                }) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let payload = vec![0u8; 1024];
    let outcome = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.write_block(&payload).expect("EW is success")
    };
    assert_eq!(outcome.bytes_written, 1024);
    assert!(outcome.early_warning);
    assert!(!outcome.end_of_medium);
    assert!(outcome.position_after.block_position_end_of_warning);
    assert!(!handle.is_dirty(), "EW is not a transport error");
}

#[test]
fn drive_handle_write_block_volume_overflow_returns_end_of_medium() {
    let lib = open_drive_test_lib("LIB_WR03");
    let policy = StaticAllowlist::new(["LIB_WR03"]);

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_WR03")]);
    let mut drive_slot = Some(vec![
        lto9_inquiry(),
        vpd80_response("DRV_A"),
        rp_long_response(0, 0, 1_000_000),
    ]);
    let mut sense = vec![0u8; 32];
    sense[0] = 0x70;
    sense[2] = 0x40 | 0x0D; // EOM bit + VOLUME OVERFLOW key
    sense[7] = 24;
    let sense_clone = sense.clone();
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                Ok(Box::new(FailFirstWriteWithCheckCondition {
                    inner,
                    sense: Some(sense_clone.clone()),
                    bytes_transferred: 0,
                }) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let payload = vec![0u8; 1024];
    let outcome = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive
            .write_block(&payload)
            .expect("VOLUME_OVERFLOW is EOM signal")
    };
    assert!(outcome.early_warning);
    assert!(outcome.end_of_medium);
    assert!(!handle.is_dirty());
}

#[test]
fn drive_handle_write_block_invalid_field_uses_cached_block_limit() {
    let lib = open_drive_test_lib("LIB_WR05");
    let policy = StaticAllowlist::new(["LIB_WR05"]);

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_WR05")]);
    let mut drive_slot = Some(vec![
        lto9_inquiry(),
        vpd80_response("DRV_A"),
        rbl_response(4096, 1),
        mode_sense_response(0, false),
    ]);
    let mut sense = vec![0u8; 32];
    sense[0] = 0x70;
    sense[2] = 0x05; // ILLEGAL REQUEST
    sense[7] = 24;
    sense[12] = 0x24; // INVALID FIELD IN CDB
    sense[13] = 0x00;
    let sense_clone = sense.clone();

    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                Ok(Box::new(FailFirstWriteWithCheckCondition {
                    inner,
                    sense: Some(sense_clone.clone()),
                    bytes_transferred: 0,
                }) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        let cfg = drive.read_config().expect("read_config caches block limit");
        assert_eq!(cfg.max_block_size_bytes, 4096);
        let payload = vec![0u8; 8192];
        drive
            .write_block(&payload)
            .expect_err("WRITE invalid-field maps to BlockTooLarge")
    };
    match err {
        TapeIoError::BlockTooLarge { requested, limit } => {
            assert_eq!(requested, 8192);
            assert_eq!(limit, 4096);
        }
        other => panic!("expected BlockTooLarge, got {other:?}"),
    }
    assert!(
        !handle.is_dirty(),
        "drive-rejected oversized block is not completion unknown"
    );
}

#[test]
fn drive_handle_write_filemarks_emits_cdb_and_returns_position() {
    let lib = open_drive_test_lib("LIB_WF01");
    let policy = StaticAllowlist::new(["LIB_WF01"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_WF01")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![
                lto9_inquiry(),
                vpd80_response("DRV_A"),
                rp_long_response(0, 0, 5_000),
            ],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let outcome = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.write_filemarks(2).expect("WRITE FILEMARKS ok")
    };
    assert_eq!(outcome.position_after.lba, 5_000);
    assert!(!outcome.early_warning);
    assert!(!outcome.end_of_medium);

    let write_filemarks_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|cdb| cdb[0] == 0x10)
        .cloned()
        .collect();
    assert_eq!(
        write_filemarks_cdbs.len(),
        1,
        "expected exactly one WRITE FILEMARKS CDB: {write_filemarks_cdbs:?}"
    );
    assert_eq!(
        write_filemarks_cdbs[0].as_slice(),
        &[0x10, 0x00, 0x00, 0x00, 0x02, 0x00],
        "WRITE FILEMARKS must keep IMMED clear for the synchronous barrier"
    );
}

#[test]
fn drive_handle_write_filemarks_zero_count_syncs_buffer_with_immed_clear_cdb() {
    let lib = open_drive_test_lib("LIB_WF00");
    let policy = StaticAllowlist::new(["LIB_WF00"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_WF00")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![
                lto9_inquiry(),
                vpd80_response("DRV_A"),
                rp_long_response(0, 0, 5_001),
            ],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let outcome = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.write_filemarks(0).expect("WRITE FILEMARKS sync ok")
    };
    assert_eq!(outcome.position_after.lba, 5_001);
    assert!(!outcome.early_warning);
    assert!(!outcome.end_of_medium);

    let write_filemarks_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|cdb| cdb[0] == 0x10)
        .cloned()
        .collect();
    assert_eq!(
        write_filemarks_cdbs.len(),
        1,
        "expected exactly one WRITE FILEMARKS CDB: {write_filemarks_cdbs:?}"
    );
    assert_eq!(
        write_filemarks_cdbs[0].as_slice(),
        &[0x10, 0x00, 0x00, 0x00, 0x00, 0x00],
        "WRITE FILEMARKS count=0 sync must keep IMMED clear and write no marks"
    );
}

/// Inject a CHECK CONDITION on the first execute_none for
/// WRITE FILEMARKS (CDB 0x10), then forward.
struct FailFirstWfWithCheckCondition<T: SgTransport> {
    inner: T,
    sense: Option<Vec<u8>>,
}
impl<T: SgTransport> SgTransport for FailFirstWfWithCheckCondition<T> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        if cdb[0] == 0x10 && self.sense.is_some() {
            let sense = self.sense.take().unwrap();
            return Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred: 0,
            });
        }
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

#[test]
fn drive_handle_write_filemarks_near_eom_returns_early_warning() {
    // Codex 20:17 (idref=6e9b56d9 High) regression: WRITE FILEMARKS
    // crossing PEWZ raises CHECK CONDITION with NO SENSE + EOM but
    // the marks ARE committed. Must surface as Ok(EW), not Err.
    let lib = open_drive_test_lib("LIB_WF02");
    let policy = StaticAllowlist::new(["LIB_WF02"]);

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_WF02")]);
    let mut drive_slot = Some(vec![
        lto9_inquiry(),
        vpd80_response("DRV_A"),
        rp_long_response(0b0000_0001, 0, 999_999),
    ]);
    let mut sense = vec![0u8; 32];
    sense[0] = 0x70;
    sense[2] = 0x40; // EOM bit, key=0 NO SENSE
    sense[7] = 24;
    let sense_clone = sense.clone();
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                Ok(Box::new(FailFirstWfWithCheckCondition {
                    inner,
                    sense: Some(sense_clone.clone()),
                }) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let outcome = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive
            .write_filemarks(1)
            .expect("PEWZ during write_filemarks must be Ok-with-EW")
    };
    assert!(outcome.early_warning);
    assert!(!outcome.end_of_medium);
    assert!(outcome.position_after.block_position_end_of_warning);
    assert!(!handle.is_dirty(), "EW is not a transport-dirty event");
}

#[test]
fn drive_handle_write_filemarks_transport_error_marks_parent_dirty() {
    let lib = open_drive_test_lib("LIB_WF03");
    let policy = StaticAllowlist::new(["LIB_WF03"]);

    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_WF03")]);
    let mut drive_slot = Some(vec![lto9_inquiry(), vpd80_response("DRV_A")]);
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                let inner = FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                );
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                Ok(Box::new(recorded) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                Ok(Box::new(FailFirstNoneWithTransportError::new(recorded))
                    as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        let err = drive.write_filemarks(1).expect_err("transport err on WF");
        assert!(matches!(err, TapeIoError::Transport(_)), "got {err:?}");
    }
    assert!(handle.is_dirty());
    assert_eq!(handle.dirty_cause(), Some(DirtyCause::CompletionUnknown));
}

#[test]
fn drive_handle_write_block_transport_error_marks_parent_dirty() {
    struct FailFirstOut<T: SgTransport> {
        inner: T,
        fired: bool,
    }
    impl<T: SgTransport> SgTransport for FailFirstOut<T> {
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            self.inner.execute_in(cdb, buf)
        }
        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
            self.inner.execute_none(cdb)
        }
        fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
            if !self.fired {
                self.fired = true;
                return Err(ScsiError::TransportError {
                    status: 0,
                    host_status: 0,
                    driver_status: 0x06,
                    info: 1,
                    sense: Vec::new(),
                });
            }
            self.inner.execute_out(cdb, buf)
        }
        fn set_timeout_for(&mut self, class: TimeoutClass) {
            self.inner.set_timeout_for(class);
        }
    }

    let lib = open_drive_test_lib("LIB_WR04");
    let policy = StaticAllowlist::new(["LIB_WR04"]);

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_WR04")]);
    let mut drive_slot = Some(vec![lto9_inquiry(), vpd80_response("DRV_A")]);
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                Ok(Box::new(FailFirstOut {
                    inner,
                    fired: false,
                }) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let payload = vec![0u8; 1024];
    let err = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.write_block(&payload).expect_err("transport err")
    };
    assert!(matches!(err, TapeIoError::Transport(_)), "got {err:?}");
    assert!(handle.is_dirty());
    assert_eq!(handle.dirty_cause(), Some(DirtyCause::CompletionUnknown));
}

#[test]
fn drive_handle_write_refuses_until_position_reestablished_after_transport_error() {
    struct FailFirstWrite<T: SgTransport> {
        inner: T,
        fired: bool,
    }
    impl<T: SgTransport> SgTransport for FailFirstWrite<T> {
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            self.inner.execute_in(cdb, buf)
        }
        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
            self.inner.execute_none(cdb)
        }
        fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
            if cdb[0] == 0x0A && !self.fired {
                self.fired = true;
                return Err(ScsiError::TransportError {
                    status: 0,
                    host_status: 0,
                    driver_status: 0x06,
                    info: 1,
                    sense: Vec::new(),
                });
            }
            self.inner.execute_out(cdb, buf)
        }
        fn set_timeout_for(&mut self, class: TimeoutClass) {
            self.inner.set_timeout_for(class);
        }
    }

    let lib = open_drive_test_lib("LIB_WR06");
    let policy = StaticAllowlist::new(["LIB_WR06"]);
    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();

    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_WR06")]);
    let mut drive_slot = Some(vec![
        lto9_inquiry(),
        vpd80_response("DRV_A"),
        rp_long_response(0, 0, 123),
        rp_long_response(0, 0, 124),
    ]);
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                let failing = FailFirstWrite {
                    inner,
                    fired: false,
                };
                Ok(
                    Box::new(RecordingTransport::with_log(failing, log_cl.clone()))
                        as Box<dyn SgTransport>,
                )
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        let payload = vec![0u8; 1024];
        assert!(matches!(
            drive.write_block(&payload),
            Err(TapeIoError::Transport(_))
        ));

        let err = drive
            .write_block(&payload)
            .expect_err("write must be refused until position is known again");
        match err {
            TapeIoError::InvalidRequest(ScsiError::InvalidInput(msg)) => {
                assert!(msg.contains("position is unknown"), "{msg}");
            }
            other => panic!("expected InvalidRequest for unknown position, got {other:?}"),
        }

        let pos = drive.position().expect("READ POSITION reestablishes latch");
        assert_eq!(pos.lba, 123);
        let outcome = drive
            .write_block(&payload)
            .expect("write allowed after position is known");
        assert_eq!(outcome.position_after.lba, 124);
    }

    let write_cdb_count = log.borrow().iter().filter(|cdb| cdb[0] == 0x0A).count();
    assert_eq!(
        write_cdb_count, 2,
        "first failed write and final allowed write issue CDBs; refused write does not"
    );
    assert!(handle.is_dirty());
    assert_eq!(handle.dirty_cause(), Some(DirtyCause::CompletionUnknown));
}

// =====================================================================
//  Layer 3a Step 9.7b — DriveHandle::read_config + write_config
// =====================================================================

use crate::handle::tape_io::{BlockSize, TapeConfig, WormMediaState};

fn mode_sense_response(block_length: u32, dce: bool) -> Vec<u8> {
    let mut buf = vec![0u8; 28];
    buf[0] = 27;
    buf[1] = 0x98;
    buf[2] = 0x10;
    buf[3] = 8;
    let bl = block_length.to_be_bytes();
    buf[9] = bl[1];
    buf[10] = bl[2];
    buf[11] = bl[3];
    buf[12] = 0x0F;
    buf[13] = 14;
    buf[14] = if dce { 0x80 } else { 0x00 };
    buf
}

fn rbl_response(max_block_length: u32, min_block_length: u16) -> Vec<u8> {
    let mut buf = vec![0u8; 6];
    let max = max_block_length.to_be_bytes();
    buf[1] = max[1];
    buf[2] = max[2];
    buf[3] = max[3];
    let min = min_block_length.to_be_bytes();
    buf[4] = min[0];
    buf[5] = min[1];
    buf
}

#[test]
fn drive_handle_read_config_combines_rbl_and_mode_sense() {
    let lib = open_drive_test_lib("LIB_RC01");
    let policy = StaticAllowlist::new(["LIB_RC01"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_RC01")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![
                lto9_inquiry(),
                vpd80_response("DRV_A"),
                rbl_response(0x80_0000, 1),
                mode_sense_response(0, false),
            ],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let cfg = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.read_config().expect("read_config ok")
    };
    assert_eq!(cfg.block_size, BlockSize::Variable);
    assert!(!cfg.compression);
    assert_eq!(cfg.max_block_size_bytes, 0x80_0000);
    assert!(!cfg.write_protected);
    assert_eq!(cfg.worm, WormMediaState::NotWorm);
    assert!(!handle.is_dirty());

    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(
        opcodes.windows(2).any(|w| w == [0x05, 0x1A]),
        "READ BLOCK LIMITS then MODE SENSE: {opcodes:?}"
    );
}

#[test]
fn drive_handle_read_config_detects_fixed_block_and_compression() {
    let lib = open_drive_test_lib("LIB_RC02");
    let policy = StaticAllowlist::new(["LIB_RC02"]);
    let (factory, _log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_RC02")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![
                lto9_inquiry(),
                vpd80_response("DRV_A"),
                rbl_response(0x80_0000, 1),
                mode_sense_response(65_536, true),
            ],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let cfg = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.read_config().expect("read_config ok")
    };
    assert_eq!(cfg.block_size, BlockSize::Fixed { size_bytes: 65_536 });
    assert!(cfg.compression);
    assert!(!cfg.write_protected);
    assert_eq!(cfg.worm, WormMediaState::NotWorm);
}

#[test]
fn drive_handle_read_config_surfaces_wp_and_worm() {
    let lib = open_drive_test_lib("LIB_RC05");
    let policy = StaticAllowlist::new(["LIB_RC05"]);
    let mut mode = mode_sense_response(0, false);
    mode[1] = 0x9C;
    mode[2] = 0x90;
    let (factory, _log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_RC05")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![
                lto9_inquiry(),
                vpd80_response("DRV_A"),
                rbl_response(0x80_0000, 1),
                mode,
            ],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let cfg = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.read_config().expect("read_config ok")
    };
    assert!(cfg.write_protected);
    assert_eq!(cfg.worm, WormMediaState::Worm);
}

#[test]
fn drive_handle_read_config_surfaces_malformed_mode_response() {
    let lib = open_drive_test_lib("LIB_RC03");
    let policy = StaticAllowlist::new(["LIB_RC03"]);

    let mut bad = mode_sense_response(0, false);
    bad[3] = 0; // BDL=0
    let (factory, _log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_RC03")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![
                lto9_inquiry(),
                vpd80_response("DRV_A"),
                rbl_response(0x80_0000, 1),
                bad,
            ],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.read_config().expect_err("malformed MODE response")
    };
    match err {
        TapeIoError::MalformedModeResponse(_) => {}
        other => panic!("expected MalformedModeResponse, got {other:?}"),
    }
    assert!(!handle.is_dirty(), "parse failure is not a transport error");
}

#[test]
fn drive_handle_read_tape_alerts_parses_log_sense() {
    let lib = open_drive_test_lib("LIB_TA01");
    let policy = StaticAllowlist::new(["LIB_TA01"]);
    let flags = BTreeSet::from([7, 19]);
    let page = remanence_scsi::log_sense::synthesize_tape_alert_page(&flags);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_TA01")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A"), page],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let alerts = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.read_tape_alerts().expect("read TapeAlert page")
    };

    assert_eq!(alerts.active(), &flags);
    assert!(alerts.is_set(7));
    assert!(!handle.is_dirty());
    let log = log.borrow();
    let cdb = log.iter().find(|cdb| cdb[0] == 0x4d).expect("LOG SENSE");
    assert_eq!(cdb[2], 0x6e);
    assert_eq!(&cdb[7..9], &[0x01, 0x44]);
}

#[test]
fn drive_handle_read_tape_alerts_rejects_short_successful_transfer() {
    let lib = open_drive_test_lib("LIB_TA02");
    let policy = StaticAllowlist::new(["LIB_TA02"]);
    let short_page =
        remanence_scsi::log_sense::synthesize_tape_alert_page(&BTreeSet::from([20]))[..8].to_vec();
    let (factory, _log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_TA02")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A"), short_page],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive
            .read_tape_alerts()
            .expect_err("short TapeAlert page is malformed")
    };

    assert!(matches!(err, TapeIoError::MalformedResponse(_)));
    assert!(!handle.is_dirty(), "parse failure is not a transport error");
}

#[test]
fn drive_handle_write_config_rejects_fixed_block_size_zero() {
    // Codex 20:22 (idref=01cf3e76 Medium): BlockSize::Fixed
    // { size_bytes: 0 } is invalid per model.rs §4.2 and must
    // be rejected at the API boundary, no CDB on the wire.
    let lib = open_drive_test_lib("LIB_WC02");
    let policy = StaticAllowlist::new(["LIB_WC02"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_WC02")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A")],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let bad = TapeConfig {
        block_size: BlockSize::Fixed { size_bytes: 0 },
        compression: false,
        max_block_size_bytes: 0,
        write_protected: false,
        worm: WormMediaState::Unknown,
    };
    let err = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.write_config(bad).expect_err("size_bytes=0 rejected")
    };
    match err {
        TapeIoError::InvalidRequest(ScsiError::InvalidInput(msg)) => {
            assert!(msg.contains("size_bytes: 0"), "{msg}");
        }
        other => panic!("expected InvalidRequest(InvalidInput), got {other:?}"),
    }
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(
        !opcodes.contains(&0x15),
        "no MODE SELECT on rejection: {opcodes:?}"
    );
}

#[test]
fn drive_handle_write_config_rejects_fixed_block_size_above_24_bits() {
    // size_bytes > 0x00FF_FFFF doesn't fit the 3-byte block-
    // descriptor field. Silent truncation would let 0x0100_0000
    // become variable-block (codex catch).
    let lib = open_drive_test_lib("LIB_WC03");
    let policy = StaticAllowlist::new(["LIB_WC03"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_WC03")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A")],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let bad = TapeConfig {
        block_size: BlockSize::Fixed {
            size_bytes: 0x0100_0000,
        },
        compression: false,
        max_block_size_bytes: 0,
        write_protected: false,
        worm: WormMediaState::Unknown,
    };
    let err = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive
            .write_config(bad)
            .expect_err("size_bytes > 24-bit rejected")
    };
    match err {
        TapeIoError::InvalidRequest(ScsiError::InvalidInput(msg)) => {
            assert!(msg.contains("24-bit"), "{msg}");
        }
        other => panic!("expected InvalidRequest(InvalidInput), got {other:?}"),
    }
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(!opcodes.contains(&0x15));
}

#[test]
fn drive_handle_read_config_rejects_short_rbl_response() {
    // Codex 20:22 (idref=01cf3e76 Medium): read_config slices
    // the RBL buffer to bytes_transferred before
    // parse_response, so a 4-byte transfer surfaces as
    // MalformedModeResponse instead of being silently parsed
    // with fabricated trailing zeros.
    struct ShortRblTransport<T: SgTransport> {
        inner: T,
    }
    impl<T: SgTransport> SgTransport for ShortRblTransport<T> {
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            if cdb[0] == 0x05 {
                buf[..4].copy_from_slice(&[0x00, 0x80, 0x00, 0x00]);
                return Ok(TransferOutcome::clean(4));
            }
            self.inner.execute_in(cdb, buf)
        }
        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
            self.inner.execute_none(cdb)
        }
        fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
            self.inner.execute_out(cdb, buf)
        }
        fn set_timeout_for(&mut self, class: TimeoutClass) {
            self.inner.set_timeout_for(class);
        }
    }

    let lib = open_drive_test_lib("LIB_RC04");
    let policy = StaticAllowlist::new(["LIB_RC04"]);
    let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response("LIB_RC04")]);
    let mut drive_slot = Some(vec![lto9_inquiry(), vpd80_response("DRV_A")]);
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                )) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                Ok(Box::new(ShortRblTransport { inner }) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive
            .read_config()
            .expect_err("short RBL must surface as malformed")
    };
    match err {
        TapeIoError::MalformedModeResponse(msg) => {
            assert!(msg.contains("READ BLOCK LIMITS"), "{msg}");
        }
        other => panic!("expected MalformedModeResponse, got {other:?}"),
    }
    assert!(!handle.is_dirty());
}

#[test]
fn drive_handle_write_config_emits_mode_select() {
    let lib = open_drive_test_lib("LIB_WC01");
    let policy = StaticAllowlist::new(["LIB_WC01"]);
    let (factory, log) = multi_recording_factory(vec![
        (
            PathBuf::from("/dev/sg-mock"),
            vec![changer_inquiry_response(), vpd80_response("LIB_WC01")],
        ),
        (
            PathBuf::from("/dev/sg-drive-mock"),
            vec![lto9_inquiry(), vpd80_response("DRV_A")],
        ),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let cfg = TapeConfig {
        block_size: BlockSize::Variable,
        compression: false,
        max_block_size_bytes: 0,
        write_protected: false,
        worm: WormMediaState::Unknown,
    };
    {
        let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        drive.write_config(cfg).expect("write_config ok");
    }
    assert!(!handle.is_dirty());

    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(
        opcodes.contains(&0x15),
        "MODE SELECT CDB 0x15 went out: {opcodes:?}"
    );
}

// =====================================================================
//  §7.7 — composed load / unload / export / import
// =====================================================================

use crate::error::{LoadError, UnloadError};

/// Build a library with one bay (drive A, sg_path /dev/sg-drive),
/// two slots (0x0400 full / 0x0401 empty), and TWO IE ports
/// (0x0300 empty, 0x0301 empty) — wider than the move_medium
/// test rig because export/import need IE ports to target.
fn composed_test_lib(serial: &str) -> Library {
    let mut lib = open_drive_test_lib(serial);
    lib.layout.ie_start = 0x0300;
    lib.layout.ie_count = 2;
    lib.ie_ports = vec![
        IePort {
            element_address: 0x0300,
            full: false,
            cartridge: None,
            import_enabled: true,
            export_enabled: true,
        },
        IePort {
            element_address: 0x0301,
            full: false,
            cartridge: None,
            import_enabled: true,
            export_enabled: true,
        },
    ];
    lib
}

/// Standard 4-response open-time script for the changer.
fn open_script(serial: &str) -> Vec<Vec<u8>> {
    vec![changer_inquiry_response(), vpd80_response(serial)]
}

fn drive_script() -> Vec<Vec<u8>> {
    vec![lto9_inquiry(), vpd80_response("DRV_A")]
}

#[test]
fn load_happy_path_audits_with_load_op_context() {
    let lib = composed_test_lib("LIB_LD01");
    let policy = StaticAllowlist::new(["LIB_LD01"]);
    let (factory, log) = multi_recording_factory(vec![
        (PathBuf::from("/dev/sg-mock"), open_script("LIB_LD01")),
        (PathBuf::from("/dev/sg-drive-mock"), drive_script()),
    ]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    handle
        .load(0x0400, 0x0100, &policy)
        .expect("composed load ok");

    // Snapshot patched: bay loaded, slot empty.
    let bay = &handle.library().drive_bays[0];
    assert!(bay.loaded);
    assert_eq!(bay.loaded_tape.as_deref(), Some("TAPE_A"));
    assert_eq!(bay.source_slot, Some(0x0400));
    assert!(!handle.library().slots[0].full);
    assert!(!handle.is_dirty(), "successful load leaves snapshot clean");

    // CDB log: one 0xA5 (MOVE), one 0x1B (SSC LOAD).
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(opcodes.contains(&0xA5));
    assert!(opcodes.contains(&0x1B));

    // Audit: 4 events, all tagged AuditOp::Load{slot, bay}.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 4);
    let expected_op = AuditOp::Load {
        slot: 0x0400,
        bay: 0x0100,
    };
    for (i, e) in events.iter().enumerate() {
        let op = match e {
            CapturedEvent::Started { op, .. } => *op,
            CapturedEvent::FinishedSuccess { op, .. } => *op,
            other => panic!("event[{i}] unexpected: {other:?}"),
        };
        assert_eq!(op, expected_op, "every event tagged Load{{slot,bay}}");
    }
    // CDB on the first Started is 0xA5 (changer MOVE), second is 0x1B (SSC LOAD).
    match &events[0] {
        CapturedEvent::Started { cdb, .. } => assert_eq!(cdb[0], 0xA5),
        _ => panic!(),
    }
    match &events[2] {
        CapturedEvent::Started { cdb, .. } => assert_eq!(cdb[0], 0x1B),
        _ => panic!(),
    }
}

#[test]
fn load_returns_move_phase_error_on_preflight_fail() {
    let lib = composed_test_lib("LIB_LD02");
    let policy = StaticAllowlist::new(["LIB_LD02"]);
    let (factory, log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_LD02"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    // Source bay is empty (the bay starts unloaded in this lib).
    let err = handle
        .load(0x0100 /* not a slot, the bay */, 0x0100, &policy)
        .unwrap_err();
    // SameElement triggers because src == dst.
    assert!(matches!(
        err,
        LoadError::Move(MoveError::SameElement { addr: 0x0100 })
    ));

    // No 0xA5 / 0x1B CDBs.
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(!opcodes.contains(&0xA5));
    assert!(!opcodes.contains(&0x1B));

    // Snapshot unchanged.
    assert!(!handle.is_dirty());
    assert!(!handle.library().drive_bays[0].loaded);
}

#[test]
fn unload_happy_path_uses_source_slot_when_no_destination_given() {
    // Pre-load bay 0x0100 from slot 0x0400; then unload without
    // an explicit destination → should go back to 0x0400.
    let lib = composed_test_lib("LIB_UL01");
    let policy = StaticAllowlist::new(["LIB_UL01"]);
    // First setup discarded — we re-build the factory below with
    // repeat_drive_factory so the drive path can be opened twice
    // (once for load(), once for unload()).
    let (factory, log) = multi_recording_factory(vec![
        (PathBuf::from("/dev/sg-mock"), open_script("LIB_UL01")),
        (PathBuf::from("/dev/sg-drive-mock"), drive_script()),
    ]);
    let handle = lib.open_with(&policy, factory).expect("library opens");
    // To allow the second open_drive (for unload), we need to
    // build a fresh factory. Re-do the setup with both load and
    // unload scripted.
    drop(handle);
    let (factory, _log_unused) = multi_recording_factory(vec![
        (PathBuf::from("/dev/sg-mock"), open_script("LIB_UL01")),
        // Drive opened TWICE: once for load(), once for unload().
        // Each open consumes std INQUIRY + VPD 0x80, but the
        // multi_recording_factory only returns each path once.
        // Use a fresh drive path entry per phase by mutating the
        // library between phases — keep it simple: seed enough
        // responses to drain over both opens. multi_recording_factory
        // serves a path *once*, so we need a different path each time.
        // Simplest: open drive twice on the same path by using a
        // factory that yields a single drive open with enough
        // canned responses for *both* phases.
        //
        // Actually multi_recording_factory takes a path -> Vec
        // mapping where the Vec is the entire response queue for
        // that path. The path is served once. After that the
        // factory errors with NotFound. So we can't open the same
        // drive path twice through this factory.
        //
        // Workaround: build a custom factory in this test that
        // serves /dev/sg-drive-mock twice, each time returning a
        // freshly-seeded FixtureTransport.
        (PathBuf::from("/dev/sg-drive-mock"), drive_script()),
    ]);
    let (factory, log) = repeat_drive_factory(
        factory,
        PathBuf::from("/dev/sg-drive-mock"),
        drive_script(),
        log,
    );
    // Re-open the library with the new factory.
    let lib = composed_test_lib("LIB_UL01");
    let mut handle = lib.open_with(&policy, factory).expect("library reopens");

    // Now load + unload.
    handle.load(0x0400, 0x0100, &policy).expect("load ok");
    assert_eq!(handle.library().drive_bays[0].source_slot, Some(0x0400));

    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);
    handle
        .unload(0x0100, /* destination */ None, &policy)
        .expect("unload ok (uses source_slot)");

    // After: slot 0x0400 is full again, bay 0x0100 is empty.
    assert!(handle.library().slots[0].full);
    assert!(!handle.library().drive_bays[0].loaded);
    assert!(!handle.is_dirty());

    // Audit: 4 events, all tagged AuditOp::Unload{bay,
    // dst:Some(0x0400)}. First Started has SSC UNLOAD CDB (0x1B
    // byte4=0), second has MOVE (0xA5).
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 4, "audit has 4 events for composed unload");
    let expected_op = AuditOp::Unload {
        bay: 0x0100,
        dst: Some(0x0400),
    };
    for e in &events {
        let op = match e {
            CapturedEvent::Started { op, .. } => *op,
            CapturedEvent::FinishedSuccess { op, .. } => *op,
            other => panic!("event unexpected: {other:?}"),
        };
        assert_eq!(op, expected_op);
    }
    match &events[0] {
        CapturedEvent::Started { cdb, .. } => assert_eq!(cdb[0], 0x1B), // SSC UNLOAD first
        _ => panic!(),
    }
    match &events[2] {
        CapturedEvent::Started { cdb, .. } => assert_eq!(cdb[0], 0xA5), // MOVE second
        _ => panic!(),
    }

    // sanity for log usage to silence unused-mut warnings if any.
    let _ = &log;
}

/// Wrap an existing factory so that the given path is served TWICE
/// (with a fresh seeded script each time). Used to test composed
/// flows that open the same drive twice.
#[allow(clippy::type_complexity)]
fn repeat_drive_factory(
    mut inner: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>>,
    repeat_path: PathBuf,
    second_script: Vec<Vec<u8>>,
    log: RecordingLog,
) -> (
    Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>>,
    RecordingLog,
) {
    let log_cl = log.clone();
    let mut second_slot = Some(second_script);
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| match inner(path) {
            Ok(t) => Ok(t),
            Err(_) if path == repeat_path => {
                let script = second_slot.take().ok_or_else(|| IoErrorKind {
                    kind: "Other",
                    message: "repeat slot exhausted".into(),
                    raw_os_error: None,
                })?;
                let inner_t = FixtureTransport::new().with_responses(script);
                let wrapped = RecordingTransport::with_log(inner_t, log_cl.clone());
                Ok(Box::new(wrapped) as Box<dyn SgTransport>)
            }
            Err(e) => Err(e),
        });
    (factory, log)
}

#[test]
fn unload_with_unknown_bay_returns_address_unknown() {
    // Pre-fix this returned UnloadError::Move(MoveError::SourceEmpty)
    // — same as a known bay with no source_slot. Now: unknown bay
    // address surfaces distinctly as AddressUnknown, matching §3.1.
    let lib = composed_test_lib("LIB_UL03");
    let policy = StaticAllowlist::new(["LIB_UL03"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_UL03"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    // 0x9999 is not in the library's drive_bays.
    let err = handle.unload(0x9999, None, &policy).unwrap_err();
    match err {
        UnloadError::Move(MoveError::AddressUnknown { library, addr }) => {
            assert_eq!(library, "LIB_UL03");
            assert_eq!(addr, 0x9999);
        }
        other => panic!("expected AddressUnknown, got {other:?}"),
    }

    // Single Refused audit event tagged AddressUnknown, not
    // SourceEmpty — the two cases must be distinguishable.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        CapturedEvent::Refused {
            op: AuditOp::Unload {
                bay: 0x9999,
                dst: None
            },
            reason: "AddressUnknown",
        }
    ));
}

#[test]
fn load_returns_open_drive_error_after_move_marks_dirty() {
    // Factory has the changer entry only — no drive entry. The
    // composed load() succeeds at MOVE MEDIUM (against the
    // changer) but then fails at open_drive (factory returns
    // NotFound for /dev/sg-drive-mock → DeviceUnavailable). Per
    // §5.1: snapshot is *patched* (cartridge in bay), is_dirty
    // becomes true.
    let lib = composed_test_lib("LIB_LD03");
    let policy = StaticAllowlist::new(["LIB_LD03"]);
    let (factory, log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_LD03"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = handle.load(0x0400, 0x0100, &policy).unwrap_err();
    match err {
        LoadError::OpenDrive(OpenError::DeviceUnavailable { path, .. }) => {
            assert_eq!(path, PathBuf::from("/dev/sg-drive-mock"));
        }
        other => panic!("expected OpenDrive(DeviceUnavailable), got {other:?}"),
    }

    // MOVE went out (changer received the CDB)…
    let move_count = log.borrow().iter().filter(|c| c[0] == 0xA5).count();
    assert_eq!(move_count, 1, "MOVE MEDIUM went out before the open failed");
    // …and patched the snapshot.
    let bay = &handle.library().drive_bays[0];
    assert!(bay.loaded);
    assert_eq!(bay.loaded_tape.as_deref(), Some("TAPE_A"));
    assert!(!handle.library().slots[0].full);
    // Per §5.1: load partial failure marks the snapshot dirty.
    assert!(
        handle.is_dirty(),
        "MOVE ok + downstream fail must set is_dirty=true"
    );
}

/// Transport wrapper that fails the first `execute_none` call
/// with a synthetic [`ScsiError::TransportError`] shaped like a
/// driver-timeout (`driver_status = 0x06`). Subsequent calls
/// pass through. Used to exercise the
/// "transport-error-on-state-changing-CDB → is_dirty = true"
/// path without needing a real flaky device.
#[cfg(target_os = "linux")]
struct FailFirstNoneWithTransportError<T: SgTransport> {
    inner: T,
    fired: bool,
}

#[cfg(target_os = "linux")]
impl<T: SgTransport> FailFirstNoneWithTransportError<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            fired: false,
        }
    }
}

#[cfg(target_os = "linux")]
impl<T: SgTransport> SgTransport for FailFirstNoneWithTransportError<T> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        // Forward the CDB so it's recorded by any wrapping
        // RecordingTransport (mirrors reality — the kernel saw
        // the CDB before the driver bailed).
        self.inner.execute_none(cdb)?;
        if !self.fired {
            self.fired = true;
            Err(ScsiError::TransportError {
                status: 0,
                host_status: 0,
                driver_status: 0x06, // SG_ERR_DRIVER_TIMEOUT
                info: 0x1,           // SG_INFO_CHECK
                sense: Vec::new(),
            })
        } else {
            Ok(())
        }
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class)
    }
}

/// Transport wrapper that fails on the (succeed_n + 1)-th call to
/// `execute_none` with a synthetic CheckCondition. Used to test
/// partial-failure paths in composed ops where a real SCSI error
/// after some CDBs have already gone out is the interesting case.
/// `execute_in` calls are forwarded unchanged.
struct FailExecuteNoneAfter<T: SgTransport> {
    inner: T,
    succeed_n: usize,
    count: usize,
}

impl<T: SgTransport> FailExecuteNoneAfter<T> {
    fn new(inner: T, succeed_n: usize) -> Self {
        Self {
            inner,
            succeed_n,
            count: 0,
        }
    }
}

impl<T: SgTransport> SgTransport for FailExecuteNoneAfter<T> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        // Forward first so the CDB is recorded — mirrors reality
        // (the kernel saw the CDB, the device replied CHECK
        // CONDITION). Then synthesise the failure if we're past
        // the threshold.
        self.inner.execute_none(cdb)?;
        self.count += 1;
        if self.count > self.succeed_n {
            Err(ScsiError::CheckCondition {
                sense: vec![0x70, 0, 0x05, 0, 0, 0, 0, 0x0a],
                bytes_transferred: 0,
            })
        } else {
            Ok(())
        }
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
}

#[test]
fn unload_returns_move_error_after_unload_keeps_snapshot() {
    // Drive UNLOAD succeeds, then the changer's MOVE fails. Per
    // §5.1: snapshot stays unchanged (the cartridge is still in
    // the bay from the snapshot's perspective AND in reality —
    // the drive released the cartridge mechanically but it
    // hasn't physically moved). is_dirty stays false.
    //
    // Construction: pre-load the bay so unload(bay, None) can
    // resolve a destination via source_slot. The changer
    // transport fails on its first execute_none call (the MOVE),
    // *after* the drive's UNLOAD execute_none has succeeded.
    let mut lib = composed_test_lib("LIB_UL04");
    lib.drive_bays[0].loaded = true;
    lib.drive_bays[0].loaded_tape = Some("TAPE_PRELOADED".into());
    lib.drive_bays[0].source_slot = Some(0x0400);
    lib.slots[0].full = false;
    lib.slots[0].cartridge = None;
    let policy = StaticAllowlist::new(["LIB_UL04"]);

    // Custom factory: changer wrapped in FailExecuteNoneAfter(0)
    // so its first execute_none (the MOVE) fails; drive
    // unwrapped so its UNLOAD succeeds.
    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut changer_slot = Some(open_script("LIB_UL04"));
    let mut drive_slot = Some(drive_script());
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                let inner = FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                );
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                // Fail on the very first execute_none (the MOVE).
                let faulted = FailExecuteNoneAfter::new(recorded, 0);
                Ok(Box::new(faulted) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                Ok(Box::new(recorded) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let snapshot_before = handle.library().clone();

    let err = handle.unload(0x0100, None, &policy).unwrap_err();
    match err {
        UnloadError::Move(MoveError::ScsiError(_)) => {}
        other => panic!("expected UnloadError::Move(ScsiError), got {other:?}"),
    }

    // Both CDBs went out: drive UNLOAD (0x1B) and the failing
    // changer MOVE (0xA5).
    let log_ref = log.borrow();
    let opcodes: Vec<u8> = log_ref.iter().map(|c| c[0]).collect();
    assert!(opcodes.contains(&0x1B), "SSC UNLOAD went out: {opcodes:?}");
    assert!(opcodes.contains(&0xA5), "MOVE MEDIUM went out: {opcodes:?}");
    drop(log_ref);

    // Per §5.1: snapshot unchanged on this path. is_dirty stays
    // as it was (false).
    assert_eq!(handle.library(), &snapshot_before);
    assert!(!handle.is_dirty());
}

#[test]
fn unload_refuses_when_no_destination_and_no_source_slot() {
    // Bay is loaded but source_slot is None (e.g., cartridge came
    // from IE port; no natural home). unload() with no explicit
    // destination has nothing to do.
    let mut lib = composed_test_lib("LIB_UL02");
    lib.drive_bays[0].loaded = true;
    lib.drive_bays[0].loaded_tape = Some("ORPHAN".into());
    lib.drive_bays[0].source_slot = None;
    let policy = StaticAllowlist::new(["LIB_UL02"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_UL02"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    let err = handle.unload(0x0100, None, &policy).unwrap_err();
    assert!(matches!(
        err,
        UnloadError::Move(MoveError::SourceEmpty { addr: 0x0100 })
    ));

    // Snapshot unchanged; is_dirty unchanged.
    assert!(handle.library().drive_bays[0].loaded);
    assert!(!handle.is_dirty());

    // Single Refused audit event with op = Unload{bay, dst: None}.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        CapturedEvent::Refused {
            op: AuditOp::Unload {
                bay: 0x0100,
                dst: None,
            },
            reason: "SourceEmpty",
        }
    ));
}

#[test]
fn ie_endpoint_move_marks_snapshot_dirty() {
    // Move slot → IE port. The CDB succeeded, but per the §7.10
    // live finding, IE-port destinations are vendor-specific:
    // HPE physical libraries park the cartridge in the IE port,
    // QuadStor's VTL vaults the cartridge immediately (IE port
    // returns to empty). We can't trust the snapshot patch, so
    // is_dirty must be set on success when either endpoint is
    // an IE port.
    let lib = composed_test_lib("LIB_IE_DIRTY");
    let policy = StaticAllowlist::new(["LIB_IE_DIRTY"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_IE_DIRTY"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);
    assert!(!handle.is_dirty(), "starts clean");

    // Slot 0x0400 (full) → IE port 0x0300 (empty).
    handle
        .move_medium(0x0400, 0x0300, &policy)
        .expect("move ok");

    // Snapshot was still patched (slot empty, IE full) — that
    // matches HPE behavior. But is_dirty=true because the patch
    // may not match QuadStor or other vendors.
    assert!(!handle.library().slots[0].full);
    assert!(handle.library().ie_ports[0].full);
    assert!(
        handle.is_dirty(),
        "IE-endpoint moves mark snapshot dirty (vendor-specific behavior)"
    );

    // Audit Finished{Success} carries dirty=true so consumers
    // filtering on dirty can pick this up without re-reading
    // is_dirty().
    let events = audit.lock().unwrap().clone();
    match events.last().expect("at least one event") {
        CapturedEvent::FinishedSuccess { .. } => {
            // CapturedEvent::FinishedSuccess in the test helper
            // doesn't surface the `dirty` field; verify via the
            // handle's is_dirty() (above).
        }
        other => panic!("expected FinishedSuccess, got {other:?}"),
    }
}

#[test]
fn ie_to_slot_move_also_marks_dirty() {
    // Symmetric: IE port → slot. Same vendor-specific concerns
    // for the source side.
    let mut lib = composed_test_lib("LIB_IE_DIRTY2");
    // Pre-occupy IE 0x0300 and empty slot 0x0400.
    lib.ie_ports[0].full = true;
    lib.ie_ports[0].cartridge = Some("INBOUND".into());
    lib.slots[0].full = false;
    lib.slots[0].cartridge = None;
    let policy = StaticAllowlist::new(["LIB_IE_DIRTY2"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_IE_DIRTY2"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    assert!(!handle.is_dirty());

    handle
        .move_medium(0x0300, 0x0400, &policy)
        .expect("move ok");
    assert!(
        handle.is_dirty(),
        "IE-endpoint moves (either side) mark dirty"
    );
}

#[test]
fn slot_to_bay_move_does_not_mark_dirty() {
    // Sanity: a move with NEITHER endpoint an IE port leaves
    // dirty unchanged. Pin this so the IE-dirty logic doesn't
    // accidentally over-fire.
    let lib = composed_test_lib("LIB_CLEAN");
    let policy = StaticAllowlist::new(["LIB_CLEAN"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_CLEAN"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    // slot 0x0400 (full) → bay 0x0100 (empty). Neither endpoint
    // is an IE port.
    handle
        .move_medium(0x0400, 0x0100, &policy)
        .expect("move ok");
    assert!(
        !handle.is_dirty(),
        "non-IE moves don't touch the dirty flag"
    );
}

#[test]
fn export_uses_first_available_ie_port() {
    // First IE port (0x0300) is full, second (0x0301) is empty.
    // export(slot=0x0400) should target 0x0301.
    let mut lib = composed_test_lib("LIB_EX01");
    lib.ie_ports[0].full = true;
    lib.ie_ports[0].cartridge = Some("OCCUPIED".into());
    let policy = StaticAllowlist::new(["LIB_EX01"]);
    let (factory, log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_EX01"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    handle.export(0x0400, &policy).expect("export ok");
    // Slot empty, IE 0x0301 now full with TAPE_A.
    assert!(!handle.library().slots[0].full);
    assert!(handle.library().ie_ports[1].full);
    assert_eq!(
        handle.library().ie_ports[1].cartridge.as_deref(),
        Some("TAPE_A")
    );

    // Audit: Started{Export{slot, ie:Some(0x0301)}} + Finished.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    let expected_op = AuditOp::Export {
        slot: 0x0400,
        ie: Some(0x0301),
    };
    for e in &events {
        let op = match e {
            CapturedEvent::Started { op, .. } => *op,
            CapturedEvent::FinishedSuccess { op, .. } => *op,
            _ => panic!(),
        };
        assert_eq!(op, expected_op);
    }
    // One 0xA5 MOVE went out.
    let move_count = log.borrow().iter().filter(|c| c[0] == 0xA5).count();
    assert_eq!(move_count, 1);
}

#[test]
fn export_refused_when_all_ie_ports_full() {
    let mut lib = composed_test_lib("LIB_EX02");
    for ie in &mut lib.ie_ports {
        ie.full = true;
        ie.cartridge = Some("FULL".into());
    }
    let policy = StaticAllowlist::new(["LIB_EX02"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_EX02"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    let err = handle.export(0x0400, &policy).unwrap_err();
    assert!(matches!(err, MoveError::DestinationFull { .. }));

    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        CapturedEvent::Refused {
            op: AuditOp::Export {
                slot: 0x0400,
                ie: None
            },
            reason: "DestinationFull",
        }
    ));
}

#[test]
fn import_uses_first_occupied_ie_port() {
    let mut lib = composed_test_lib("LIB_IM01");
    // Pre-occupy IE port 0x0301 (not 0x0300) — import should
    // target the first FULL one it finds (0x0301).
    lib.ie_ports[1].full = true;
    lib.ie_ports[1].cartridge = Some("INBOUND".into());
    // Make the destination slot empty.
    lib.slots[0].full = false;
    lib.slots[0].cartridge = None;
    let policy = StaticAllowlist::new(["LIB_IM01"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_IM01"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    handle.import(0x0400, &policy).expect("import ok");
    assert!(handle.library().slots[0].full);
    assert_eq!(
        handle.library().slots[0].cartridge.as_deref(),
        Some("INBOUND")
    );
    assert!(!handle.library().ie_ports[1].full);

    let events = audit.lock().unwrap().clone();
    let expected_op = AuditOp::Import {
        ie: Some(0x0301),
        slot: 0x0400,
    };
    for e in &events {
        let op = match e {
            CapturedEvent::Started { op, .. } => *op,
            CapturedEvent::FinishedSuccess { op, .. } => *op,
            _ => panic!(),
        };
        assert_eq!(op, expected_op);
    }
}

#[test]
fn import_refused_when_no_ie_port_occupied() {
    // All IE ports empty.
    let lib = composed_test_lib("LIB_IM02");
    // composed_test_lib leaves slot 0x0400 full — make it empty
    // so the destination is plausible (not required for the
    // refusal path, but keeps the snapshot internally consistent).
    let mut lib = lib;
    lib.slots[0].full = false;
    lib.slots[0].cartridge = None;
    let policy = StaticAllowlist::new(["LIB_IM02"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_IM02"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    let err = handle.import(0x0400, &policy).unwrap_err();
    assert!(matches!(err, MoveError::SourceEmpty { .. }));

    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        CapturedEvent::Refused {
            op: AuditOp::Import {
                ie: None,
                slot: 0x0400
            },
            reason: "SourceEmpty",
        }
    ));

    // Silence dead-code warning on LoadError / UnloadError if no
    // other test in this file uses these specific variants.
    let _: fn(MoveError) -> LoadError = LoadError::Move;
    let _: fn(OpenError) -> LoadError = LoadError::OpenDrive;
    let _: fn(DriveOpError) -> LoadError = LoadError::DriveLoad;
    let _: fn(OpenError) -> UnloadError = UnloadError::OpenDrive;
    let _: fn(DriveOpError) -> UnloadError = UnloadError::DriveUnload;
    let _: fn(MoveError) -> UnloadError = UnloadError::Move;
}

// =====================================================================
//  §7.8 — lock_removal / allow_removal + RemovalLockGuard
// =====================================================================

#[test]
fn lock_removal_issues_correct_cdb_and_audits() {
    let lib = composed_test_lib("LIB_LK01");
    let policy = StaticAllowlist::new(["LIB_LK01"]);
    let (factory, log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_LK01"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    // Hold the guard explicitly — don't auto-drop yet, so we can
    // observe just the PREVENT half before the ALLOW fires.
    {
        let _guard = handle.lock_removal().expect("lock ok");
        // Verify the PREVENT CDB went out.
        let pa_cdbs: Vec<Vec<u8>> = log
            .borrow()
            .iter()
            .filter(|c| c[0] == 0x1E)
            .cloned()
            .collect();
        assert_eq!(pa_cdbs.len(), 1, "exactly one PREVENT/ALLOW CDB so far");
        assert_eq!(pa_cdbs[0], vec![0x1E, 0x00, 0x00, 0x00, 0x01, 0x00]);

        let events = audit.lock().unwrap().clone();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            CapturedEvent::Started {
                op: AuditOp::LockRemoval,
                cdb,
            } if cdb[0] == 0x1E && cdb[4] == 0x01
        ));
        assert!(matches!(
            events[1],
            CapturedEvent::FinishedSuccess {
                op: AuditOp::LockRemoval,
                snapshot_patched: false,
            }
        ));
    }
    // Guard dropped here — best-effort ALLOW fires automatically.
    // After Drop, the log should contain a second 0x1E CDB with
    // byte 4 = 0x00 (ALLOW).
    let pa_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0x1E)
        .cloned()
        .collect();
    assert_eq!(pa_cdbs.len(), 2, "PREVENT then ALLOW after Drop");
    assert_eq!(pa_cdbs[0], vec![0x1E, 0x00, 0x00, 0x00, 0x01, 0x00]);
    assert_eq!(pa_cdbs[1], vec![0x1E, 0x00, 0x00, 0x00, 0x00, 0x00]);

    let events = audit.lock().unwrap().clone();
    // Started{Lock} + Finished{Lock} + Started{Allow} + Finished{Allow}
    assert_eq!(events.len(), 4);
    assert!(matches!(
        events[2],
        CapturedEvent::Started {
            op: AuditOp::AllowRemoval,
            ..
        }
    ));
    assert!(matches!(
        events[3],
        CapturedEvent::FinishedSuccess {
            op: AuditOp::AllowRemoval,
            snapshot_patched: false,
        }
    ));
}

#[test]
fn allow_removal_direct_issues_correct_cdb_and_audits() {
    let lib = composed_test_lib("LIB_LK02");
    let policy = StaticAllowlist::new(["LIB_LK02"]);
    let (factory, log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_LK02"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    // Direct call — no guard involved. Useful in daemon code that
    // tracks lock state separately.
    handle.allow_removal().expect("allow ok");

    let cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0x1E)
        .cloned()
        .collect();
    assert_eq!(cdbs.len(), 1);
    assert_eq!(cdbs[0], vec![0x1E, 0x00, 0x00, 0x00, 0x00, 0x00]);

    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        &events[0],
        CapturedEvent::Started {
            op: AuditOp::AllowRemoval,
            cdb,
        } if cdb[0] == 0x1E && cdb[4] == 0x00
    ));
    assert!(matches!(
        events[1],
        CapturedEvent::FinishedSuccess {
            op: AuditOp::AllowRemoval,
            ..
        }
    ));
}

#[test]
fn guard_release_returns_result_and_suppresses_drop() {
    // release() consumes the guard and returns the ALLOW result.
    // After that, Drop must NOT fire a second ALLOW.
    let lib = composed_test_lib("LIB_LK03");
    let policy = StaticAllowlist::new(["LIB_LK03"]);
    let (factory, log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_LK03"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let guard = handle.lock_removal().expect("lock ok");
    guard.release().expect("release ok");

    // Total: one PREVENT + one ALLOW. NOT two ALLOWs.
    let pa_cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0x1E)
        .cloned()
        .collect();
    assert_eq!(pa_cdbs.len(), 2, "exactly one PREVENT + one ALLOW");
    assert_eq!(pa_cdbs[0][4], 0x01, "first is PREVENT");
    assert_eq!(pa_cdbs[1][4], 0x00, "second is ALLOW");
}

#[test]
fn guard_supports_protected_move_via_deref_mut() {
    // The critical-section test: lock_removal → call move_medium
    // through the guard → guard drops → ALLOW fires. While the
    // guard is alive, the operator must be able to do moves. The
    // CDB sequence must be exactly:
    //   PREVENT (0x1E byte4=0x01) → MOVE MEDIUM (0xA5) → ALLOW
    //   (0x1E byte4=0x00)
    // — and ALL THREE must show up in the log, with MOVE between
    // PREVENT and ALLOW.
    let lib = composed_test_lib("LIB_LK05");
    let policy = StaticAllowlist::new(["LIB_LK05"]);
    let (factory, log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_LK05"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    {
        let mut guard = handle.lock_removal().expect("lock ok");
        // Call move_medium directly on the guard — DerefMut makes
        // every ChangerHandle method available. composed_test_lib
        // ships with one slot 0x0400 (full) and one IE port
        // 0x0300 (empty); move slot → IE.
        guard
            .move_medium(0x0400, 0x0300, &policy)
            .expect("move ok inside lock");
    } // guard drops here → ALLOW fires

    // Filter the CDB log down to the two opcodes we care about
    // and confirm the order.
    let interesting: Vec<u8> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0x1E || c[0] == 0xA5)
        .map(|c| if c[0] == 0x1E { c[4] } else { 0xA5 })
        .collect();
    // Coding: PREVENT = 0x01, MOVE = 0xA5, ALLOW = 0x00.
    assert_eq!(
        interesting,
        vec![0x01, 0xA5, 0x00],
        "PREVENT → MOVE → ALLOW in order"
    );

    // Snapshot was patched by the move.
    assert!(!handle.library().slots[0].full);
    assert!(handle.library().ie_ports[0].full);
    assert_eq!(
        handle.library().ie_ports[0].cartridge.as_deref(),
        Some("TAPE_A")
    );
}

#[test]
fn lock_removal_returns_scsi_error_when_cdb_fails() {
    // Custom factory: changer's execute_none fails on first call
    // (which is PREVENT). Caller sees Err. No guard is returned.
    let lib = composed_test_lib("LIB_LK04");
    let policy = StaticAllowlist::new(["LIB_LK04"]);
    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut changer_slot = Some(open_script("LIB_LK04"));
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                let inner = FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                );
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                let faulted = FailExecuteNoneAfter::new(recorded, 0);
                Ok(Box::new(faulted) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    let err = handle.lock_removal().unwrap_err();
    // The synthetic failure is a CheckCondition.
    assert!(matches!(err, ScsiError::CheckCondition { .. }));

    // CDB was attempted (recorded) — the failure is at the
    // device layer, not at the build layer.
    let cdbs: Vec<Vec<u8>> = log
        .borrow()
        .iter()
        .filter(|c| c[0] == 0x1E)
        .cloned()
        .collect();
    assert_eq!(cdbs.len(), 1, "the PREVENT CDB went out before failing");

    // Audit: Started{LockRemoval} + Finished{ScsiError}.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0],
        CapturedEvent::Started {
            op: AuditOp::LockRemoval,
            ..
        }
    ));
    assert!(matches!(
        &events[1],
        CapturedEvent::FinishedScsiError {
            op: AuditOp::LockRemoval,
            ..
        }
    ));

    // Silence dead-code on RemovalLockGuard's Debug impl: at
    // least one test surfaces it via the lock-removal sequence.
    // The struct is also re-exported at crate::RemovalLockGuard,
    // so this is just defensive.
    let _ = std::any::type_name::<RemovalLockGuard<'_>>();
}

// -- Operation-class timeouts + completion-unknown failures ------

#[test]
fn timeout_class_durations_match_operational_reality() {
    // Lock the numbers so a refactor that picks a too-short
    // window for the slow ops surfaces as a test failure
    // rather than as a real-hardware timeout in production.
    use crate::transport::TimeoutClass;
    assert_eq!(TimeoutClass::Inquiry.duration_ms(), 5_000);
    assert_eq!(TimeoutClass::PreventAllow.duration_ms(), 5_000);
    assert_eq!(TimeoutClass::ReadElementStatus.duration_ms(), 60_000);
    // MSL3040 MOVE is 8-20s in practice; window must allow
    // ~6× the typical so retries don't get torn down.
    assert_eq!(TimeoutClass::Move.duration_ms(), 120_000);
    // INIT and LOAD/UNLOAD can run to minutes on big libraries
    // / cold drives.
    assert_eq!(TimeoutClass::InitElementStatus.duration_ms(), 600_000);
    assert_eq!(TimeoutClass::LoadUnload.duration_ms(), 600_000);
}

#[cfg(target_os = "linux")]
#[test]
fn move_with_transport_error_marks_snapshot_dirty() {
    // Inject a driver-timeout-shaped TransportError on the MOVE
    // CDB. Completion is ambiguous (the robot may have moved
    // the cartridge even though we didn't get the status back),
    // so is_dirty must be true and the snapshot patch must NOT
    // have been applied.
    let lib = composed_test_lib("LIB_TX01");
    let policy = StaticAllowlist::new(["LIB_TX01"]);
    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut changer_slot = Some(open_script("LIB_TX01"));
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            assert_eq!(path, Path::new("/dev/sg-mock"));
            let inner =
                FixtureTransport::new().with_responses(changer_slot.take().ok_or_else(|| {
                    IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    }
                })?);
            let recorded = RecordingTransport::with_log(inner, log_cl.clone());
            let faulted = FailFirstNoneWithTransportError::new(recorded);
            Ok(Box::new(faulted) as Box<dyn SgTransport>)
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let snapshot_before = handle.library().clone();
    let (hook, audit) = capture_audit();
    handle.set_audit_hook(hook);

    let err = handle
        .move_medium(0x0400, 0x0100, &policy)
        .expect_err("MOVE must fail with the injected transport error");
    match err {
        MoveError::ScsiError(ScsiError::TransportError {
            driver_status: 0x06,
            ..
        }) => {}
        other => panic!("expected MoveError::ScsiError(TransportError), got {other:?}"),
    }

    // Snapshot patch was NOT applied (slot still full, bay still
    // empty), but is_dirty IS set because the CDB outcome is
    // ambiguous — the cartridge might actually be mid-flight.
    assert_eq!(handle.library().slots, snapshot_before.slots);
    assert_eq!(handle.library().drive_bays, snapshot_before.drive_bays);
    assert!(
        handle.is_dirty(),
        "transport-error MOVE must mark snapshot dirty (completion unknown)"
    );

    // CDB log: the MOVE went out before the synthetic failure.
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(opcodes.contains(&0xA5), "MOVE CDB went out: {opcodes:?}");

    // Audit: Started{Move} + Finished{ScsiError, dirty=true}.
    let events = audit.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    match &events[1] {
        CapturedEvent::FinishedScsiError { summary, .. } => {
            assert!(
                summary.contains("transport error"),
                "summary names the transport error: {summary}"
            );
        }
        other => panic!("expected FinishedScsiError, got {other:?}"),
    }
}

#[cfg(target_os = "linux")]
#[test]
fn rescan_with_init_transport_error_marks_snapshot_dirty() {
    // INITIALIZE ELEMENT STATUS times out mid-walk: the
    // changer's internal element-state cache may be partially
    // re-derived. Snapshot must be marked dirty alongside the
    // hard error so the caller doesn't keep trusting the
    // pre-INIT view.
    let lib = composed_test_lib("LIB_TX02");
    let policy = StaticAllowlist::new(["LIB_TX02"]);
    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut changer_slot = Some(open_script("LIB_TX02"));
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            assert_eq!(path, Path::new("/dev/sg-mock"));
            let inner =
                FixtureTransport::new().with_responses(changer_slot.take().ok_or_else(|| {
                    IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    }
                })?);
            let recorded = RecordingTransport::with_log(inner, log_cl.clone());
            let faulted = FailFirstNoneWithTransportError::new(recorded);
            Ok(Box::new(faulted) as Box<dyn SgTransport>)
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = handle
        .rescan()
        .expect_err("INIT must fail with the injected transport error");
    match err {
        RescanError::ScsiError(ScsiError::TransportError {
            driver_status: 0x06,
            ..
        }) => {}
        other => panic!("expected RescanError::ScsiError(TransportError), got {other:?}"),
    }
    assert!(
        handle.is_dirty(),
        "transport-error INIT must mark snapshot dirty (changer state may be partial)"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn composed_unload_with_drive_transport_error_marks_dirty() {
    // SSC UNLOAD times out on the drive (before the changer
    // MOVE phase). Per the new semantics, the drive may have
    // actually ejected the cartridge mechanically — bay state
    // is no longer trustworthy. Snapshot marked dirty even
    // though the composed unload's MOVE never ran.
    let mut lib = composed_test_lib("LIB_TX03");
    lib.drive_bays[0].loaded = true;
    lib.drive_bays[0].loaded_tape = Some("TAPE_TX03".into());
    lib.drive_bays[0].source_slot = Some(0x0400);
    lib.slots[0].full = false;
    lib.slots[0].cartridge = None;
    let policy = StaticAllowlist::new(["LIB_TX03"]);

    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut changer_slot = Some(open_script("LIB_TX03"));
    let mut drive_slot = Some(drive_script());
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                let inner = FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                );
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                Ok(Box::new(recorded) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                let faulted = FailFirstNoneWithTransportError::new(recorded);
                Ok(Box::new(faulted) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = handle.unload(0x0100, None, &policy).unwrap_err();
    match err {
        UnloadError::DriveUnload(DriveOpError::ScsiError(ScsiError::TransportError {
            driver_status: 0x06,
            ..
        })) => {}
        other => panic!("expected DriveUnload(ScsiError::TransportError), got {other:?}"),
    }
    assert!(
        handle.is_dirty(),
        "transport-error SSC UNLOAD must mark snapshot dirty (bay state ambiguous)"
    );

    // Only the drive UNLOAD CDB went out — no changer MOVE.
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(opcodes.contains(&0x1B), "SSC UNLOAD went out: {opcodes:?}");
    assert!(
        !opcodes.contains(&0xA5),
        "changer MOVE must NOT have been attempted: {opcodes:?}"
    );
}

// -- DirtyCause reporting ----------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn move_transport_error_records_completion_unknown_cause() {
    // The same scenario as
    // move_with_transport_error_marks_snapshot_dirty above —
    // re-run to assert the cause label, not just is_dirty.
    let lib = composed_test_lib("LIB_CAUSE01");
    let policy = StaticAllowlist::new(["LIB_CAUSE01"]);
    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut changer_slot = Some(open_script("LIB_CAUSE01"));
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            assert_eq!(path, Path::new("/dev/sg-mock"));
            let inner =
                FixtureTransport::new().with_responses(changer_slot.take().ok_or_else(|| {
                    IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    }
                })?);
            let recorded = RecordingTransport::with_log(inner, log_cl.clone());
            let faulted = FailFirstNoneWithTransportError::new(recorded);
            Ok(Box::new(faulted) as Box<dyn SgTransport>)
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let _ = handle
        .move_medium(0x0400, 0x0100, &policy)
        .expect_err("MOVE must fail");
    assert_eq!(
        handle.dirty_cause(),
        Some(DirtyCause::CompletionUnknown),
        "transport-error MOVE should report CompletionUnknown"
    );
}

#[test]
fn ie_endpoint_move_records_vendor_semantics_cause() {
    let mut lib = composed_test_lib("LIB_CAUSE02");
    lib.slots[0].full = true;
    lib.slots[0].cartridge = Some("CART_C2".into());
    let policy = StaticAllowlist::new(["LIB_CAUSE02"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_CAUSE02"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    handle
        .move_medium(0x0400, 0x0300, &policy)
        .expect("slot → IE move succeeds against fixture");
    assert_eq!(
        handle.dirty_cause(),
        Some(DirtyCause::VendorSemantics),
        "successful IE-touching move should report VendorSemantics"
    );
}

#[test]
fn composed_load_partial_failure_records_partial_failure_cause() {
    // The same setup as
    // composed_load_partial_failure_sets_is_dirty above:
    // MOVE succeeds against the changer, then open_drive
    // fails because /dev/sg-drive-mock isn't in the factory
    // bag. The composed `load`'s post-MOVE phase failed →
    // PartialFailure.
    let lib = composed_test_lib("LIB_CAUSE03");
    let policy = StaticAllowlist::new(["LIB_CAUSE03"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_CAUSE03"),
    )]);
    let mut handle = lib.open_with(&policy, factory).expect("library opens");
    let _ = handle
        .load(0x0400, 0x0100, &policy)
        .expect_err("load must fail at open_drive");
    assert_eq!(
        handle.dirty_cause(),
        Some(DirtyCause::PartialFailure),
        "post-MOVE composed-op failure should report PartialFailure"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn composed_load_drive_transport_error_records_completion_unknown() {
    // MOVE succeeds, drive opens, SSC LOAD then fails with a
    // synthetic transport timeout. Per the post-MOVE
    // classification: cartridge moved AND drive may have
    // actually loaded — `CompletionUnknown`, not the
    // collapsed-to-PartialFailure label.
    let lib = composed_test_lib("LIB_CAUSE05");
    let policy = StaticAllowlist::new(["LIB_CAUSE05"]);

    let log: RecordingLog = RecordingLog::new();
    let log_cl = log.clone();
    let mut changer_slot = Some(open_script("LIB_CAUSE05"));
    let mut drive_slot = Some(drive_script());
    #[allow(clippy::type_complexity)]
    let factory: Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>> =
        Box::new(move |path: &Path| {
            if path == Path::new("/dev/sg-mock") {
                let inner = FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer drained".into(),
                        raw_os_error: None,
                    })?,
                );
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                Ok(Box::new(recorded) as Box<dyn SgTransport>)
            } else if path == Path::new("/dev/sg-drive-mock") {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive drained".into(),
                            raw_os_error: None,
                        }
                    })?);
                let recorded = RecordingTransport::with_log(inner, log_cl.clone());
                let faulted = FailFirstNoneWithTransportError::new(recorded);
                Ok(Box::new(faulted) as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unknown path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
    let mut handle = lib.open_with(&policy, factory).expect("library opens");

    let err = handle
        .load(0x0400, 0x0100, &policy)
        .expect_err("load must fail at SSC LOAD");
    match err {
        LoadError::DriveLoad(DriveOpError::ScsiError(ScsiError::TransportError {
            driver_status: 0x06,
            ..
        })) => {}
        other => panic!("expected DriveLoad(ScsiError::TransportError), got {other:?}"),
    }
    assert_eq!(
        handle.dirty_cause(),
        Some(DirtyCause::CompletionUnknown),
        "drive-LOAD transport error must report CompletionUnknown, not PartialFailure"
    );

    // Both CDBs went out: MOVE (0xA5) and SSC LOAD (0x1B).
    let opcodes: Vec<u8> = log.borrow().iter().map(|c| c[0]).collect();
    assert!(opcodes.contains(&0xA5), "MOVE CDB went out: {opcodes:?}");
    assert!(
        opcodes.contains(&0x1B),
        "SSC LOAD CDB went out: {opcodes:?}"
    );
}

#[test]
fn fresh_handle_has_no_dirty_cause() {
    let lib = composed_test_lib("LIB_CAUSE04");
    let policy = StaticAllowlist::new(["LIB_CAUSE04"]);
    let (factory, _log) = multi_recording_factory(vec![(
        PathBuf::from("/dev/sg-mock"),
        open_script("LIB_CAUSE04"),
    )]);
    let handle = lib.open_with(&policy, factory).expect("library opens");
    assert!(!handle.is_dirty());
    assert_eq!(handle.dirty_cause(), None);
}

/// Build raw RES bytes for a 1-drive, 0-slot, 0-IE library —
/// guaranteed to mismatch the real-MSL3040 fixture's shape.
fn build_synthetic_es_one_drive() -> Vec<u8> {
    // SMC-3 layout:
    //   8-byte Element Status Data header:
    //     first_element_address = 0x0001, num_elements = 1,
    //     reserved, byte_count = 0x14 (= 8 page header + 12 desc)
    //   8-byte Page header:
    //     element_type = 0x04 (DataTransfer), flags = 0,
    //     desc_len = 0x000c, reserved, byte_count = 0x0c
    //   12-byte Element descriptor:
    //     address = 0x0001, flags = 0 (empty), remaining 10 zeros
    let mut bytes = vec![
        // Element Status Data header
        0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x14, // Page header
        0x04, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x0c,
        // Element descriptor: address 0x0001, flags = 0
        0x00, 0x01,
    ];
    bytes.extend_from_slice(&[0u8; 10]);
    bytes
}
