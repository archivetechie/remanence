//! Changer and drive-control completion evidence over the existing inventory fixtures.

use super::tests::{composed_test_lib, drive_script, open_script, with_inventory};
use super::*;
use crate::error::{AuditOutcome, LoadError};
use crate::transport::{FixtureTransport, RecordingLog, RecordingTransport, TransferOutcome};
use crate::StaticAllowlist;
use changer_ready::{classify_changer_ready_transition, ChangerReadyWait};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SERIAL: &str = "B3A";
const SLOT: u16 = 0x400;
const BAY: u16 = 0x100;
type Outcomes = Arc<Mutex<Vec<AuditOutcome>>>;

/// A final MOVE refusal classifies only its host fact; inventory commands keep
/// their own classifications and no MOVE, dirty state, or snapshot patch occurs.
#[test]
fn b3b_shutdown_move_admission_census() {
    use crate::outcome::{census, Dispatch, Effect, OutcomeReason, PositionEffect};
    let (mut handle, log, _) = open(None, None);
    let before = (census::count(), census::total(), log.borrow().len());
    let error = handle
        .changer_mut()
        .move_medium_with_admission(SLOT, BAY, &policy(), &mut || Err("shutdown".into()))
        .unwrap_err();
    assert!(matches!(error.kind, crate::MoveErrorKind::Admission(_)));
    assert_eq!(error.outcome().evidence.dispatch, Dispatch::NotIssued);
    assert_eq!(error.outcome().reason, OutcomeReason::NotIssued);
    assert_eq!(error.outcome().effect, Effect::NoFence);
    assert_eq!(error.outcome().evidence.position, PositionEffect::Unchanged);
    let commands = (log.borrow().len() - before.2) as u64;
    assert_eq!(census::count() - before.0, commands);
    assert_eq!(census::total() - before.1, commands + 1);
    assert!(!log.borrow().iter().any(|cdb| cdb[0] == 0xa5));
    assert!(!handle.is_dirty());
    assert!(element_state(handle.library(), SLOT).unwrap().0);
    assert!(!element_state(handle.library(), BAY).unwrap().0);
}

/// Inject one failure before the fixture applies its successful command.
struct FailOpcode {
    inner: FixtureTransport,
    opcode: u8,
    error: Option<ScsiError>,
}
impl SgTransport for FailOpcode {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        if cdb[0] == self.opcode {
            if let Some(error) = self.error.take() {
                return Err(error);
            }
        }
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
}

fn check(raw: Vec<u8>) -> ScsiError {
    ScsiError::CheckCondition {
        sense: raw,
        bytes_transferred: 0,
    }
}
fn sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
    let mut raw = vec![0; 18];
    raw[0] = 0x70;
    raw[2] = key;
    raw[7] = 10;
    raw[12] = asc;
    raw[13] = ascq;
    raw
}
fn policy() -> StaticAllowlist {
    StaticAllowlist::new([SERIAL])
}

fn open(
    changer_error: Option<ScsiError>,
    drive_error: Option<ScsiError>,
) -> (LibraryHandle, RecordingLog, Outcomes) {
    let library = composed_test_lib(SERIAL);
    let mut changer_error = changer_error;
    let mut drive_error = drive_error;
    let factory = move |path: &Path| -> Result<Box<dyn SgTransport>, IoErrorKind> {
        let (responses, opcode, error) = if path == Path::new("/dev/sg-mock") {
            (open_script(SERIAL), 0xa5, changer_error.take())
        } else {
            assert_eq!(path, Path::new("/dev/sg-drive-mock"));
            let mut responses = drive_script();
            let mut position = vec![0; 32];
            position[8..16].copy_from_slice(&10u64.to_be_bytes());
            responses.push(position);
            (responses, 0x1b, drive_error.take())
        };
        Ok(Box::new(FailOpcode {
            inner: FixtureTransport::new().with_responses(responses),
            opcode,
            error,
        }))
    };
    let mut inventory = with_inventory(&library, factory);
    let log = RecordingLog::new();
    let captured_log = log.clone();
    let factory = move |path: &Path| -> Result<Box<dyn SgTransport>, IoErrorKind> {
        Ok(Box::new(RecordingTransport::with_log(
            inventory(path)?,
            captured_log.clone(),
        )))
    };
    let mut handle = library.open_with(&policy(), factory).unwrap();
    let outcomes = Outcomes::default();
    let captured = outcomes.clone();
    handle.set_audit_hook(move |event| {
        if let AuditEvent::Finished { outcome, .. } = event {
            captured.lock().unwrap().push(outcome.clone());
        }
    });
    (handle, log, outcomes)
}

fn assert_refreshed_once(log: &RecordingLog) {
    let log = log.borrow();
    let moves: Vec<_> = log
        .iter()
        .enumerate()
        .filter(|(_, c)| c[0] == 0xa5)
        .collect();
    assert_eq!(moves.len(), 1);
    assert!(
        log[moves[0].0 + 1..].iter().any(|c| c[0] == 0xb8),
        "failed MOVE must refresh"
    );
    assert!(!log.iter().any(|c| c[0] == 0), "no readiness polling");
}

fn unknown_move(raw: Vec<u8>) {
    let (mut handle, log, outcomes) = open(Some(check(raw.clone())), None);
    let waits = Arc::new(Mutex::new(0));
    let observed = waits.clone();
    handle.set_changer_ready_wait(
        ChangerReadyWait::bounded(Duration::from_secs(2)).with_observer(Arc::new(move |_| {
            *observed.lock().unwrap() += 1;
        })),
    );
    let error = handle.move_medium(SLOT, BAY, &policy()).unwrap_err();
    assert!(
        matches!(
            error,
            crate::MoveError {
                kind: crate::MoveErrorKind::ScsiError(_),
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(handle.dirty_cause(), Some(DirtyCause::CompletionUnknown));
    assert!(outcomes.lock().unwrap().iter().any(
        |o| matches!(o, AuditOutcome::ScsiError { dirty: true, sense: Some(s), .. } if s == &raw)
    ));
    assert_refreshed_once(&log);
    assert_eq!(*waits.lock().unwrap(), 0);
}

/// Empty autosense on MOVE is unknown even without host failure; it never starts a ready wait.
#[test]
fn b3a_move_empty_sense_is_unknown() {
    unknown_move(vec![]);
}

/// An undefined response code cannot prove a refused MOVE, regardless of captured length.
#[test]
fn b3a_move_malformed_sense_is_unknown() {
    unknown_move(vec![0; 18]);
}

/// A fixed key without ASC/ASCQ cannot prove a refused MOVE.
#[test]
fn b3a_move_truncated_sense_is_unknown() {
    unknown_move(vec![0x70, 0, 5, 0, 0, 0, 0, 24]);
}

/// A successful changer MOVE followed by unknown drive LOAD has the stronger dirty cause.
#[test]
fn b3a_composed_load_empty_sense_is_completion_unknown() {
    let (mut handle, log, _) = open(None, Some(check(vec![])));
    assert!(matches!(
        handle.load(SLOT, BAY, &policy()),
        Err(LoadError::DriveLoad(crate::DriveOpError {
            kind: crate::DriveOpErrorKind::ScsiError(_),
            ..
        }))
    ));
    assert_eq!(handle.dirty_cause(), Some(DirtyCause::CompletionUnknown));
    let log = log.borrow();
    let moved = log.iter().position(|c| c[0] == 0xa5).unwrap();
    assert!(log[moved + 1..].iter().any(|c| c[0] == 0x1b));
}

/// Unknown drive UNLOAD invalidates its cursor and the shared parent snapshot.
#[test]
fn b3a_unload_empty_sense_invalidates_parent_and_position() {
    let (mut handle, _, outcomes) = open(None, Some(check(vec![])));
    handle.move_medium(SLOT, BAY, &policy()).unwrap();
    let mut drive = handle.open_drive(BAY, &policy()).unwrap();
    drive.position().unwrap();
    assert_eq!(drive.known_position().map(|p| p.lba), Some(10));
    let error = drive.unload().unwrap_err();
    assert!(
        matches!(
            error,
            crate::DriveOpError {
                kind: crate::DriveOpErrorKind::ScsiError(_),
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(drive.known_position(), None);
    assert_eq!(handle.dirty_cause(), Some(DirtyCause::CompletionUnknown));
    assert!(matches!(
        outcomes.lock().unwrap().last(),
        Some(AuditOutcome::ScsiError { dirty: true, .. })
    ));
}

/// Missing ASCQ cannot manufacture a ready-transition unit attention from 06/29.
#[test]
fn b3a_truncated_ready_transition_is_not_retryable() {
    let mut raw = sense(6, 0x29, 0);
    raw.truncate(13);
    assert_eq!(
        classify_changer_ready_transition(&crate::MoveError::from(check(raw))),
        None
    );
}

/// A transport failure carrying CHECK CONDITION and usable sense remains unknown and unretried.
#[test]
fn b3a_move_host_failure_is_unknown() {
    let raw = sense(3, 0x0c, 0);
    let (mut handle, log, outcomes) = open(
        Some(ScsiError::TransportError {
            status: 2,
            host_status: 3,
            driver_status: 8,
            info: 1,
            sense: raw.clone(),
        }),
        None,
    );
    handle.set_changer_ready_wait(ChangerReadyWait::bounded(Duration::from_secs(2)));
    assert!(matches!(
        handle.move_medium(SLOT, BAY, &policy()),
        Err(crate::MoveError {
            kind: crate::MoveErrorKind::ScsiError(ScsiError::TransportError { .. }),
            ..
        })
    ));
    assert_eq!(handle.dirty_cause(), Some(DirtyCause::CompletionUnknown));
    assert_refreshed_once(&log);
    assert!(outcomes.lock().unwrap().iter().any(
        |o| matches!(o, AuditOutcome::ScsiError { dirty: true, sense: Some(s), .. } if s == &raw)
    ));
}

/// 3B/0E names an empty source; inventory refresh precedes the typed return.
#[test]
fn b3a_move_source_empty_mapping_refreshes() {
    let (mut handle, log, _) = open(Some(check(sense(5, 0x3b, 0x0e))), None);
    assert!(matches!(
        handle.move_medium(SLOT, BAY, &policy()),
        Err(crate::MoveError {
            kind: crate::MoveErrorKind::SourceEmpty { addr: SLOT },
            ..
        })
    ));
    assert!(!handle.is_dirty());
    assert_refreshed_once(&log);
}

/// 3B/0D names a full destination, stays known, and refreshes inventory before returning.
#[test]
fn b3a_move_destination_full_mapping_refreshes() {
    let (mut handle, log, _) = open(Some(check(sense(5, 0x3b, 0x0d))), None);
    assert!(matches!(
        handle.move_medium(SLOT, BAY, &policy()),
        Err(crate::MoveError {
            kind: crate::MoveErrorKind::DestinationFull { addr: BAY },
            ..
        })
    ));
    assert!(!handle.is_dirty());
    assert_refreshed_once(&log);
}

/// Usable becoming-ready sense still waits and retries MOVE once after fresh inventory.
#[test]
fn b3a_move_usable_not_ready_still_retries() {
    let (mut handle, log, _) = open(Some(check(sense(2, 4, 1))), None);
    let waits = Arc::new(Mutex::new(0));
    let observed = waits.clone();
    handle.set_changer_ready_wait(
        ChangerReadyWait::bounded(Duration::from_secs(2))
            .with_poll_interval(Duration::from_millis(100))
            .with_observer(Arc::new(move |_| {
                *observed.lock().unwrap() += 1;
            })),
    );
    handle.move_medium(SLOT, BAY, &policy()).unwrap();
    assert!(*waits.lock().unwrap() > 0);
    assert_eq!(log.borrow().iter().filter(|c| c[0] == 0xa5).count(), 2);
    assert!(!handle.is_dirty());
}

/// Composed UNLOAD preserves unknown completion after its fixture MOVE.
#[test]
fn b3b_composed_unload_empty_sense_is_completion_unknown() {
    let (mut handle, log, _) = open(None, Some(check(vec![])));
    handle.move_medium(SLOT, BAY, &policy()).unwrap();
    let error = handle.unload(BAY, Some(SLOT), &policy()).unwrap_err();
    assert!(
        matches!(
            error,
            UnloadError::DriveUnload(crate::DriveOpError {
                kind: crate::DriveOpErrorKind::ScsiError(_),
                ..
            })
        ),
        "{error:?}"
    );
    assert_eq!(handle.dirty_cause(), Some(DirtyCause::CompletionUnknown));
    assert_eq!(log.borrow().iter().filter(|c| c[0] == 0x1b).count(), 1);
}

/// The MOVE result survives the mandatory inventory refresh and both readiness consumers.
#[test]
fn b3b_move_carrier_consumer_census() {
    use crate::outcome::census;
    let (mut handle, log, _) = open(Some(check(sense(5, 0x3b, 0x0d))), None);
    let before = (
        census::count(),
        census::normalizations(),
        log.borrow().len(),
    );
    let error = handle.load(SLOT, BAY, &policy()).unwrap_err();
    let LoadError::Move(error) = error else {
        panic!("expected MOVE failure: {error:?}");
    };
    assert!(matches!(
        error.kind,
        crate::MoveErrorKind::DestinationFull { addr: BAY }
    ));
    assert_refreshed_once(&log);
    assert_eq!(
        census::count() - before.0,
        (log.borrow().len() - before.2) as u64
    );
    assert_eq!(census::normalizations() - before.1, 1);
    assert_eq!(error.outcome().evidence.opcode, Some(0xa5));
    let before = (census::total(), census::normalizations());
    assert!(crate::classify_changer_not_ready(&error).is_none());
    assert!(crate::classify_changer_ready_transition(&error).is_none());
    assert_eq!(error.completion_evidence(), error.outcome().evidence);
    assert_eq!((census::total(), census::normalizations()), before);
}

/// Captured host-error sense cannot enter the legacy CHECK CONDITION-only 3B remapping.
#[test]
fn b3b_move_carrier_preserves_host_error_kind() {
    let (mut handle, _, _) = open(
        Some(ScsiError::TransportError {
            status: 2,
            host_status: 3,
            driver_status: 0,
            info: 1,
            sense: sense(5, 0x3b, 0x0d),
        }),
        None,
    );
    let error = handle.load(SLOT, BAY, &policy()).unwrap_err();
    let LoadError::Move(error) = error else {
        panic!("expected MOVE failure")
    };
    assert!(matches!(
        error.kind,
        crate::MoveErrorKind::ScsiError(ScsiError::TransportError { .. })
    ));
    assert!(matches!(
        error.outcome().effect,
        crate::outcome::Effect::InventoryUnknown(_)
    ));
}

/// Inventory failures cannot authorize a drive-side removal-lock recovery.
#[test]
fn b3b_removal_predicate_preserves_drive_error_scope() {
    let error = crate::DriveOpError::from(crate::MoveError::from(check(sense(5, 0x53, 2))));
    let before = (
        crate::outcome::census::total(),
        crate::outcome::census::normalizations(),
    );
    assert!(!error.is_medium_removal_prevented());
    assert_eq!(
        (
            crate::outcome::census::total(),
            crate::outcome::census::normalizations()
        ),
        before
    );
}

/// Gate contract without filesystem substitutions: the authority sees the actual
/// CDB target/context and a refused intent dispatches no physical MOVE.
#[test]
fn typed_motion_gate_preserves_context_and_refusal_kind() {
    use crate::motion::*;
    #[derive(Default)]
    struct Authority {
        refusal: Option<MotionAuthorityFailure>,
        calls: Mutex<Vec<(MotionCommand, MotionContext)>>,
        confirmations: Mutex<usize>,
    }
    impl MotionAuthority for Authority {
        fn intent(
            &self,
            command: &MotionCommand,
            context: &MotionContext,
        ) -> Result<MotionTicket, MotionAuthorityError> {
            self.calls
                .lock()
                .unwrap()
                .push((command.clone(), context.clone()));
            if let Some(kind) = self.refusal {
                return Err(MotionAuthorityError {
                    message: "test refusal".into(),
                    kind,
                });
            }
            Ok(MotionTicket {
                sequence: 1,
                record_uuid: [1; 16],
            })
        }
        fn failed(
            &self,
            _: MotionTicket,
            _: &crate::outcome::CompletionEvidence,
        ) -> Result<(), MotionAuthorityError> {
            Ok(())
        }
        fn unconfirmed(&self, _: &PendingMotion) -> Result<(), MotionAuthorityError> {
            Ok(())
        }
        fn confirm_inventory(
            &self,
            pending: &PendingMotion,
            observed: &Library,
        ) -> Result<(), MotionAuthorityError> {
            assert_eq!(pending.ticket.sequence, 1);
            assert!(
                !observed
                    .slots
                    .iter()
                    .find(|s| s.element_address == SLOT)
                    .unwrap()
                    .full
            );
            assert!(
                observed
                    .drive_bays
                    .iter()
                    .find(|d| d.element_address == BAY)
                    .unwrap()
                    .loaded
            );
            *self.confirmations.lock().unwrap() += 1;
            Ok(())
        }
        fn confirm_readiness(
            &self,
            _: &PendingMotion,
            _: &crate::MediaReadiness,
            _: std::time::Duration,
        ) -> Result<(), MotionAuthorityError> {
            panic!("changer test");
        }
        fn confirm_standalone(&self, _: &PendingMotion) -> Result<(), MotionAuthorityError> {
            panic!("changer test");
        }
        fn ensure_motion_allowed(&self) -> Result<(), MotionAuthorityError> {
            Ok(())
        }
    }
    for refusal in [
        None,
        Some(MotionAuthorityFailure::SemanticRefusal),
        Some(MotionAuthorityFailure::StoreFailure),
    ] {
        let (mut handle, log, _) = open(None, None);
        let authority = Arc::new(Authority {
            refusal,
            ..Default::default()
        });
        handle.set_motion_authority(authority.clone());
        let context = MotionContext {
            parent_operation: Some([7; 16]),
            tape: Some(([8; 16], 1)),
            inventory_generation: Some(3),
            readiness_generation: Some(4),
            load_confirmation: LoadConfirmation::Inventory,
            maintenance: None,
        };
        let result = handle
            .changer_mut()
            .with_motion_context(context.clone(), |changer| {
                changer.move_medium(SLOT, BAY, &policy())
            });
        assert_eq!(
            authority.calls.lock().unwrap().as_slice(),
            &[(
                MotionCommand::Move {
                    library_serial: SERIAL.into(),
                    source: SLOT,
                    destination: BAY
                },
                context
            )]
        );
        if let Some(kind) = refusal {
            let error = result.unwrap_err();
            assert!(
                matches!(error.kind, crate::MoveErrorKind::MotionAuthority(ref e) if e.kind == kind)
            );
            assert!(!log.borrow().iter().any(|cdb| cdb[0] == 0xa5));
        } else {
            result.unwrap();
            assert_eq!(*authority.confirmations.lock().unwrap(), 1);
            let log = log.borrow();
            let moved = log.iter().position(|cdb| cdb[0] == 0xa5).unwrap();
            assert!(
                log[moved + 1..].iter().any(|cdb| cdb[0] == 0xb8),
                "confirmation follows physical MOVE"
            );
            assert!(handle.changer_mut().take_pending_motion().is_empty());
        }
    }
}

/// A GOOD MOVE followed by an unreadable inventory is Unknown, not a failed
/// MOVE eligible for retry. The original successful completion stays attached.
#[test]
fn typed_motion_confirmation_failure_never_repeats_move() {
    use crate::motion::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct FailConfirmation {
        inner: Box<dyn SgTransport>,
        moved: bool,
    }
    impl SgTransport for FailConfirmation {
        fn execute_in(&mut self, cdb: &[u8], out: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            if self.moved && cdb[0] == 0xb8 {
                return Err(ScsiError::InvalidInput("confirmation read failed"));
            }
            self.inner.execute_in(cdb, out)
        }
        fn execute_out(&mut self, cdb: &[u8], input: &[u8]) -> Result<TransferOutcome, ScsiError> {
            self.inner.execute_out(cdb, input)
        }
        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
            let result = self.inner.execute_none(cdb);
            if cdb[0] == 0xa5 && result.is_ok() {
                self.moved = true;
            }
            result
        }
    }
    struct Authority(AtomicUsize);
    impl MotionAuthority for Authority {
        fn intent(
            &self,
            _: &MotionCommand,
            _: &MotionContext,
        ) -> Result<MotionTicket, MotionAuthorityError> {
            Ok(MotionTicket {
                sequence: 1,
                record_uuid: [1; 16],
            })
        }
        fn failed(
            &self,
            _: MotionTicket,
            _: &crate::outcome::CompletionEvidence,
        ) -> Result<(), MotionAuthorityError> {
            panic!("GOOD MOVE must not become Failed");
        }
        fn unconfirmed(&self, pending: &PendingMotion) -> Result<(), MotionAuthorityError> {
            assert_eq!(pending.evidence.dispatch, crate::outcome::Dispatch::Issued);
            assert_eq!(
                pending.evidence.completion,
                crate::outcome::Completion::Known
            );
            assert!(!pending.evidence.failed);
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn confirm_inventory(
            &self,
            _: &PendingMotion,
            _: &Library,
        ) -> Result<(), MotionAuthorityError> {
            panic!("no physical observation exists");
        }
        fn confirm_readiness(
            &self,
            _: &PendingMotion,
            _: &crate::MediaReadiness,
            _: std::time::Duration,
        ) -> Result<(), MotionAuthorityError> {
            panic!("changer test");
        }
        fn confirm_standalone(&self, _: &PendingMotion) -> Result<(), MotionAuthorityError> {
            panic!("changer test");
        }
        fn ensure_motion_allowed(&self) -> Result<(), MotionAuthorityError> {
            Ok(())
        }
    }
    let (mut handle, log, _) = open(None, None);
    {
        let mut transport = handle.changer.transport.lock().unwrap();
        let inner = std::mem::replace(&mut *transport, Box::new(FixtureTransport::new()));
        *transport = Box::new(FailConfirmation {
            inner,
            moved: false,
        });
    }
    let authority = Arc::new(Authority(AtomicUsize::new(0)));
    handle.set_motion_authority(authority.clone());
    let error = handle
        .changer_mut()
        .move_medium(SLOT, BAY, &policy())
        .unwrap_err();
    assert!(
        matches!(error.kind, crate::MoveErrorKind::MotionAuthority(ref error) if error.kind == MotionAuthorityFailure::EvidenceFailure)
    );
    assert_eq!(authority.0.load(Ordering::SeqCst), 1);
    assert_eq!(log.borrow().iter().filter(|cdb| cdb[0] == 0xa5).count(), 1);
    assert!(handle.changer_mut().take_pending_motion().is_empty());
}

/// PFD4: a readiness-owned LOAD completed by `load_with_admission` is carried to the next
/// drive handle opened for that bay, once, and to no other bay.
#[test]
fn readiness_owned_load_is_handed_to_the_next_drive_handle_of_that_bay() {
    use crate::motion::*;
    struct Authority;
    impl MotionAuthority for Authority {
        fn intent(
            &self,
            _: &MotionCommand,
            _: &MotionContext,
        ) -> Result<MotionTicket, MotionAuthorityError> {
            Ok(MotionTicket {
                sequence: 1,
                record_uuid: [1; 16],
            })
        }
        fn failed(
            &self,
            _: MotionTicket,
            _: &crate::outcome::CompletionEvidence,
        ) -> Result<(), MotionAuthorityError> {
            Ok(())
        }
        fn unconfirmed(&self, _: &PendingMotion) -> Result<(), MotionAuthorityError> {
            Ok(())
        }
        fn confirm_inventory(
            &self,
            _: &PendingMotion,
            _: &Library,
        ) -> Result<(), MotionAuthorityError> {
            Ok(())
        }
        fn confirm_readiness(
            &self,
            _: &PendingMotion,
            _: &crate::MediaReadiness,
            _: Duration,
        ) -> Result<(), MotionAuthorityError> {
            panic!("the handoff only moves the pending LOAD; the composer closes it");
        }
        fn confirm_standalone(&self, _: &PendingMotion) -> Result<(), MotionAuthorityError> {
            panic!("changer test");
        }
        fn ensure_motion_allowed(&self) -> Result<(), MotionAuthorityError> {
            Ok(())
        }
    }
    const OTHER_BAY: u16 = 0x101;
    let mut library = composed_test_lib(SERIAL);
    library.layout.drive_count = 2;
    let mut second = library.drive_bays[0].clone();
    second.element_address = OTHER_BAY;
    second.installed.as_mut().unwrap().sg_path =
        Some(std::path::PathBuf::from("/dev/sg-drive-mock-b"));
    library.drive_bays.push(second);
    let factory = |path: &Path| -> Result<Box<dyn SgTransport>, IoErrorKind> {
        let responses = if path == Path::new("/dev/sg-mock") {
            open_script(SERIAL)
        } else {
            drive_script()
        };
        Ok(Box::new(FixtureTransport::new().with_responses(responses)))
    };
    let mut handle = library
        .open_with(&policy(), with_inventory(&library, factory))
        .unwrap();
    handle.set_motion_authority(Arc::new(Authority));
    let context = MotionContext {
        parent_operation: Some([7; 16]),
        load_confirmation: LoadConfirmation::Readiness,
        ..Default::default()
    };
    handle
        .with_motion_context(context.clone(), |handle| handle.load(SLOT, BAY, &policy()))
        .unwrap();

    let mut other = handle.open_drive(OTHER_BAY, &policy()).unwrap();
    assert!(other.take_pending_motion().is_empty());
    let mut loaded = handle.open_drive(BAY, &policy()).unwrap();
    let pending = loaded.take_pending_motion();
    assert_eq!(pending.len(), 1);
    assert!(matches!(pending[0].command, MotionCommand::Load(_)));
    assert_eq!(pending[0].context, context);
    let mut again = handle.open_drive(BAY, &policy()).unwrap();
    assert!(again.take_pending_motion().is_empty());
}

/// MR-A's funnel and abandonment helpers exercise the real handle paths over fixture CDBs.
mod mr_a_tests {
    use super::*;
    use crate::motion::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[derive(Default)]
    struct Authority {
        unresolved: AtomicBool,
        next: AtomicU64,
        events: Arc<Mutex<Vec<&'static str>>>,
        abandoned: Mutex<Vec<MotionTicket>>,
        refusal: Mutex<Option<MotionAuthorityFailure>>,
    }
    impl MotionAuthority for Authority {
        fn unowned_motion_unresolved(&self, serial: &str) -> bool {
            assert_eq!(serial, SERIAL);
            self.unresolved.load(Ordering::SeqCst)
        }
        fn reconcile_inventory(&self, observed: &Library) -> Result<(), MotionAuthorityError> {
            assert_eq!(observed.serial, SERIAL);
            self.events.lock().unwrap().push("reconcile");
            Ok(())
        }
        fn intent(
            &self,
            _: &MotionCommand,
            _: &MotionContext,
        ) -> Result<MotionTicket, MotionAuthorityError> {
            Ok(MotionTicket {
                sequence: self.next.fetch_add(1, Ordering::SeqCst) + 1,
                record_uuid: [1; 16],
            })
        }
        fn failed(
            &self,
            _: MotionTicket,
            _: &crate::outcome::CompletionEvidence,
        ) -> Result<(), MotionAuthorityError> {
            panic!("fixture commands succeed");
        }
        fn unconfirmed(&self, pending: &PendingMotion) -> Result<(), MotionAuthorityError> {
            self.abandoned.lock().unwrap().push(pending.ticket);
            match *self.refusal.lock().unwrap() {
                Some(kind) => Err(MotionAuthorityError {
                    kind,
                    message: "fixture outcome refusal".into(),
                }),
                None => Ok(()),
            }
        }
        fn confirm_inventory(
            &self,
            _: &PendingMotion,
            _: &Library,
        ) -> Result<(), MotionAuthorityError> {
            Ok(())
        }
        fn confirm_standalone(&self, _: &PendingMotion) -> Result<(), MotionAuthorityError> {
            panic!("bay fixture");
        }
        fn confirm_readiness(
            &self,
            _: &PendingMotion,
            _: &crate::MediaReadiness,
            _: Duration,
        ) -> Result<(), MotionAuthorityError> {
            panic!("abandonment never confirms readiness");
        }
        fn ensure_motion_allowed(&self) -> Result<(), MotionAuthorityError> {
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum Reply {
        Normal,
        FullRobot,
        ShapeMismatch,
        ReadFailure,
    }
    struct ObserveRead {
        inner: Box<dyn SgTransport>,
        events: Arc<Mutex<Vec<&'static str>>>,
        reply: Reply,
    }
    impl SgTransport for ObserveRead {
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            let outcome = self.inner.execute_in(cdb, buf)?;
            if cdb[0] == 0xb8 {
                // issue_res calls this once for its header probe and once for the full reply.
                if buf.len() == 8 {
                    self.events.lock().unwrap().push("probe");
                    return Ok(outcome);
                }
                self.events.lock().unwrap().push("read");
                match self.reply {
                    Reply::Normal => (),
                    Reply::FullRobot => {
                        // The fixture's first page is a single medium transport descriptor.
                        assert_eq!(buf[8], 1);
                        buf[18] |= 1;
                    }
                    Reply::ShapeMismatch => {
                        // Change a storage address while preserving the descriptor/count framing.
                        let mut page = 8;
                        loop {
                            assert!(page + 10 < outcome.bytes_transferred as usize);
                            if buf[page] == 2 {
                                buf[page + 9] ^= 0x20;
                                break;
                            }
                            let bytes = u32::from_be_bytes([
                                0,
                                buf[page + 5],
                                buf[page + 6],
                                buf[page + 7],
                            ]);
                            page += 8 + bytes as usize;
                        }
                    }
                    Reply::ReadFailure => return Err(check(sense(4, 0x44, 0))),
                }
            }
            Ok(outcome)
        }
        fn execute_none(&mut self, _: &[u8]) -> Result<(), ScsiError> {
            panic!("inventory reconciliation sends only read commands");
        }
        fn execute_out(&mut self, _: &[u8], _: &[u8]) -> Result<TransferOutcome, ScsiError> {
            panic!("inventory reconciliation sends only read commands");
        }
    }
    fn observe_read(handle: &mut LibraryHandle, authority: &Authority, reply: Reply) {
        let mut transport = handle.changer.transport.lock().unwrap();
        let inner = std::mem::replace(&mut *transport, Box::new(FixtureTransport::new()));
        *transport = Box::new(ObserveRead {
            inner,
            events: authority.events.clone(),
            reply,
        });
    }
    fn unresolved_handle(reply: Reply) -> (LibraryHandle, Arc<Authority>) {
        let (mut handle, _, _) = open(None, None);
        let authority = Arc::new(Authority::default());
        authority.unresolved.store(true, Ordering::SeqCst);
        handle.set_motion_authority(authority.clone());
        observe_read(&mut handle, &authority, reply);
        (handle, authority)
    }

    #[test]
    fn l1_funnel_skips_without_unowned_motion_then_reads_before_reconciling() {
        let (mut handle, log, _) = open(None, None);
        let before = log.borrow().len();
        assert!(!handle.changer_mut().reconcile_unowned_motion().unwrap());
        let authority = Arc::new(Authority::default());
        handle.set_motion_authority(authority.clone());
        observe_read(&mut handle, &authority, Reply::Normal);
        assert!(!handle.changer_mut().reconcile_unowned_motion().unwrap());
        assert_eq!(log.borrow().len(), before, "no CDB without unowned motion");
        assert!(authority.events.lock().unwrap().is_empty());
        authority.unresolved.store(true, Ordering::SeqCst);
        assert!(handle.changer_mut().reconcile_unowned_motion().unwrap());
        assert_eq!(
            *authority.events.lock().unwrap(),
            vec!["probe", "read", "reconcile"]
        );
        assert_eq!(log.borrow().len(), before + 2, "one two-phase RES");
        assert!(log.borrow()[before..].iter().all(|cdb| cdb[0] == 0xb8));
        assert_eq!(
            &log.borrow()[before][7..10],
            &[0, 0, 8],
            "header probe allocation"
        );
        assert_eq!(log.borrow().last().unwrap()[0], 0xb8);
        authority.events.lock().unwrap().clear();
        handle.reconcile_startup_motion().unwrap();
        assert_eq!(
            *authority.events.lock().unwrap(),
            vec!["probe", "read", "reconcile"],
            "startup delegates to the funnel"
        );
        assert_eq!(log.borrow().len(), before + 4);
    }

    #[test]
    fn l1_failed_read_records_no_reconciliation() {
        let (mut handle, authority) = unresolved_handle(Reply::ReadFailure);
        let result = handle.changer_mut().reconcile_unowned_motion();
        assert_eq!(
            *authority.events.lock().unwrap(),
            vec!["probe", "read"],
            "reconcile_inventory never called"
        );
        assert!(result.is_err());
    }

    #[test]
    fn l2_full_robot_hand_records_no_reconciliation() {
        let (mut handle, authority) = unresolved_handle(Reply::FullRobot);
        let result = handle.changer_mut().reconcile_unowned_motion();
        assert_eq!(
            *authority.events.lock().unwrap(),
            vec!["probe", "read"],
            "L2 reconcile_inventory never called"
        );
        assert!(result.is_err());
        assert!(
            !handle.is_dirty(),
            "shape matched; only the full hand refuses"
        );
    }

    #[test]
    fn l3_shape_mismatch_records_no_reconciliation() {
        let (mut handle, authority) = unresolved_handle(Reply::ShapeMismatch);
        let before = handle
            .library()
            .slots
            .iter()
            .map(|s| s.element_address)
            .collect::<Vec<_>>();
        let result = handle.changer_mut().reconcile_unowned_motion();
        assert_eq!(
            *authority.events.lock().unwrap(),
            vec!["probe", "read"],
            "L3 reconcile_inventory never called"
        );
        assert!(result.is_err());
        assert!(handle.is_dirty());
        assert_eq!(
            handle
                .library()
                .slots
                .iter()
                .map(|s| s.element_address)
                .collect::<Vec<_>>(),
            before,
            "refresh kept the old snapshot"
        );
    }

    fn parked_load() -> (LibraryHandle, Arc<Authority>, MotionTicket) {
        let (mut handle, _, _) = open(None, None);
        let authority = Arc::new(Authority::default());
        handle.set_motion_authority(authority.clone());
        handle
            .with_motion_context(
                MotionContext {
                    parent_operation: Some([7; 16]),
                    load_confirmation: LoadConfirmation::Readiness,
                    ..Default::default()
                },
                |handle| handle.load(SLOT, BAY, &policy()),
            )
            .unwrap();
        let pending = &handle.handed_off_motion[&BAY];
        assert_eq!(pending.len(), 1);
        assert!(matches!(pending[0].command, MotionCommand::Load(_)));
        assert_eq!(
            pending[0].context.load_confirmation,
            LoadConfirmation::Readiness
        );
        let ticket = pending[0].ticket;
        (handle, authority, ticket)
    }

    #[test]
    fn l4_drive_abandonment_removes_closed_loads_and_keeps_failed_appends() {
        for refusal in [
            None,
            Some(MotionAuthorityFailure::SemanticRefusal),
            Some(MotionAuthorityFailure::StoreFailure),
        ] {
            let (mut handle, authority, ticket) = parked_load();
            let mut drive = handle.open_drive(BAY, &policy()).unwrap();
            *authority.refusal.lock().unwrap() = refusal;
            let result = drive.abandon_readiness_loads();
            assert_eq!(*authority.abandoned.lock().unwrap(), vec![ticket]);
            if refusal == Some(MotionAuthorityFailure::StoreFailure) {
                assert_eq!(
                    result.unwrap_err().kind,
                    MotionAuthorityFailure::StoreFailure
                );
                assert_eq!(drive.pending_motion.len(), 1);
                assert_eq!(drive.pending_motion[0].ticket, ticket);
                // A subsequent successful append still has the original ticket to close.
                *authority.refusal.lock().unwrap() = None;
                drive.abandon_readiness_loads().unwrap();
            } else {
                result.unwrap();
            }
            assert!(drive.pending_motion.is_empty());
            let calls = authority.abandoned.lock().unwrap().clone();
            drive.abandon_readiness_loads().unwrap();
            assert_eq!(
                *authority.abandoned.lock().unwrap(),
                calls,
                "second drain does nothing"
            );
        }
    }

    #[test]
    fn l4_handoff_abandonment_removes_closed_loads_and_keeps_failed_appends() {
        for refusal in [
            None,
            Some(MotionAuthorityFailure::SemanticRefusal),
            Some(MotionAuthorityFailure::StoreFailure),
        ] {
            // load() handed off the pending LOAD before dropping its temporary drive handle.
            let (mut handle, authority, ticket) = parked_load();
            *authority.refusal.lock().unwrap() = refusal;
            handle.abandon_handed_off_loads(BAY + 1).unwrap();
            assert!(authority.abandoned.lock().unwrap().is_empty());
            let result = handle.abandon_handed_off_loads(BAY);
            assert_eq!(*authority.abandoned.lock().unwrap(), vec![ticket]);
            if refusal == Some(MotionAuthorityFailure::StoreFailure) {
                assert_eq!(
                    result.unwrap_err().kind,
                    MotionAuthorityFailure::StoreFailure
                );
                assert_eq!(handle.handed_off_motion[&BAY].len(), 1);
                assert_eq!(handle.handed_off_motion[&BAY][0].ticket, ticket);
                *authority.refusal.lock().unwrap() = None;
                handle.abandon_handed_off_loads(BAY).unwrap();
            } else {
                result.unwrap();
            }
            assert!(!handle.handed_off_motion.contains_key(&BAY));
            let calls = authority.abandoned.lock().unwrap().clone();
            handle.abandon_handed_off_loads(BAY).unwrap();
            assert_eq!(
                *authority.abandoned.lock().unwrap(),
                calls,
                "second drain does nothing"
            );
        }
    }

    #[test]
    fn l4_abandonment_filters_readiness_loads_and_stops_at_first_storage_error() {
        for handed_off in [false, true] {
            let (mut handle, authority, ticket) = parked_load();
            let mut drive = handle.open_drive(BAY, &policy()).unwrap();
            let mut other = drive.pending_motion[0].clone();
            other.ticket.sequence += 1;
            other.context.load_confirmation = LoadConfirmation::Inventory;
            drive.pending_motion.push(other.clone());
            other.ticket.sequence += 1;
            other.context.load_confirmation = LoadConfirmation::Readiness;
            other.command = MotionCommand::Unload(DriveKey::Bay {
                library_serial: SERIAL.into(),
                bay: BAY,
            });
            drive.pending_motion.push(other.clone());
            other.ticket.sequence += 1;
            other.command = MotionCommand::Load(DriveKey::Bay {
                library_serial: SERIAL.into(),
                bay: BAY,
            });
            drive.pending_motion.push(other);
            *authority.refusal.lock().unwrap() = Some(MotionAuthorityFailure::StoreFailure);
            if handed_off {
                handle
                    .handed_off_motion
                    .insert(BAY, drive.take_pending_motion());
                assert_eq!(
                    handle.abandon_handed_off_loads(BAY).unwrap_err().kind,
                    MotionAuthorityFailure::StoreFailure
                );
                assert_eq!(handle.handed_off_motion[&BAY].len(), 4);
            } else {
                assert_eq!(
                    drive.abandon_readiness_loads().unwrap_err().kind,
                    MotionAuthorityFailure::StoreFailure
                );
                assert_eq!(drive.pending_motion.len(), 4);
            }
            assert_eq!(
                *authority.abandoned.lock().unwrap(),
                vec![ticket],
                "later LOAD stays parked"
            );
            *authority.refusal.lock().unwrap() = None;
            let remaining = if handed_off {
                handle.abandon_handed_off_loads(BAY).unwrap();
                &handle.handed_off_motion[&BAY]
            } else {
                drive.abandon_readiness_loads().unwrap();
                &drive.pending_motion
            };
            assert_eq!(
                remaining.len(),
                2,
                "inventory LOAD and readiness-context UNLOAD stay parked"
            );
            assert_eq!(
                remaining
                    .iter()
                    .map(|p| p.ticket.sequence)
                    .collect::<Vec<_>>(),
                vec![ticket.sequence + 1, ticket.sequence + 2]
            );
            assert_eq!(
                authority
                    .abandoned
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|t| t.sequence)
                    .collect::<Vec<_>>(),
                vec![ticket.sequence, ticket.sequence, ticket.sequence + 3]
            );
        }
    }
}
