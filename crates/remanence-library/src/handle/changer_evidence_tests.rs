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
