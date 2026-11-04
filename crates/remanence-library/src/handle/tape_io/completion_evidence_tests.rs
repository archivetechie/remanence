//! Completion evidence through scripted transports; no device I/O.

use super::*;
use crate::transport::{FixtureTransport, SgTransport, TransferOutcome};
use crate::{DriveHandle, StaticAllowlist};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Shutdown refusals carry one host outcome, preserve position/proof, and do
/// not consume a transport completion; a later dispatched failure is judged once.
#[test]
fn b3b_shutdown_drive_admission_census() {
    use crate::outcome::{census, Dispatch, Effect, OutcomeReason, PositionEffect};
    for load in [false, true] {
        let (mut drive, log, _) = open(
            0x1b,
            check(sense(5, 0x24, 0, 0, 0), 0),
            vec![position_reply(10)],
        );
        drive.position().unwrap();
        let generation = drive.identity_generation();
        let dispatches = drive.media_write_dispatch_count();
        let before = (census::count(), census::total(), log.lock().unwrap().len());
        let mut refuse = || Err("shutdown".to_string());
        let error = if load {
            drive.load_immediate_with_admission(&mut refuse)
        } else {
            drive.unload_with_admission(&mut refuse)
        }
        .unwrap_err();
        assert!(matches!(
            (&error.kind, load),
            (crate::DriveOpErrorKind::LoadAdmission(_), true)
                | (crate::DriveOpErrorKind::UnloadAdmission(_), false)
        ));
        assert_eq!(error.outcome().evidence.dispatch, Dispatch::NotIssued);
        assert_eq!(error.outcome().reason, OutcomeReason::NotIssued);
        assert_eq!(error.outcome().effect, Effect::NoFence);
        assert_eq!(error.outcome().evidence.position, PositionEffect::Unchanged);
        assert_eq!(census::count(), before.0);
        assert_eq!(census::total() - before.1, 1);
        assert_eq!(log.lock().unwrap().len(), before.2);
        assert_eq!(drive.known_position().map(|p| p.lba), Some(10));
        assert_eq!(drive.identity_generation(), generation);
        assert_eq!(drive.media_write_dispatch_count(), dispatches);
        assert!(!drive.media_write_fenced_this_load());

        let commands_before = log.lock().unwrap().len();
        let before = (census::count(), census::total(), census::normalizations());
        let error = drive.load_immediate().unwrap_err();
        assert_eq!(error.outcome().evidence.dispatch, Dispatch::Issued);
        assert_eq!(census::count() - before.0, 1);
        assert_eq!(census::total() - before.1, 1);
        assert_eq!(census::normalizations() - before.2, 1);
        assert_eq!(log.lock().unwrap().len(), commands_before + 1);
        assert_eq!(drive.identity_generation(), generation + 1);
    }
}

type CdbLog = Arc<Mutex<Vec<Vec<u8>>>>;
type Outcomes = Arc<Mutex<Vec<AuditOutcome>>>;

/// One injected error for a selected opcode, with all CDBs recorded.
struct Scripted {
    inner: Box<dyn SgTransport>,
    opcode: u8,
    error: Option<ScsiError>,
    log: CdbLog,
}

impl Scripted {
    fn dispatch(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.log.lock().unwrap().push(cdb.to_vec());
        if cdb[0] == self.opcode {
            if let Some(error) = self.error.take() {
                return Err(error);
            }
        }
        Ok(())
    }
}

impl SgTransport for Scripted {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.dispatch(cdb)?;
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.dispatch(cdb)?;
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.dispatch(cdb)?;
        self.inner.execute_out(cdb, buf)
    }
}

fn position_reply(lba: u64) -> Vec<u8> {
    let mut reply = vec![0; 32];
    reply[8..16].copy_from_slice(&lba.to_be_bytes());
    reply
}

fn sense(key: u8, asc: u8, ascq: u8, flags: u8, residue: u32) -> Vec<u8> {
    let mut sense = vec![0; 32];
    sense[0] = 0xf0;
    sense[2] = key | flags;
    sense[3..7].copy_from_slice(&residue.to_be_bytes());
    sense[7] = 24;
    sense[12] = asc;
    sense[13] = ascq;
    sense
}

fn check(sense: Vec<u8>, bytes_transferred: u32) -> ScsiError {
    ScsiError::CheckCondition {
        sense,
        bytes_transferred,
    }
}

fn open(opcode: u8, error: ScsiError, replies: Vec<Vec<u8>>) -> (DriveHandle, CdbLog, Outcomes) {
    let fixture = FixtureTransport::new().with_responses(
        [
            include_bytes!("../../../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
            vec![8, 0x80, 0, 3, b'B', b'3', b'A'],
        ]
        .into_iter()
        .chain(replies),
    );
    let log = CdbLog::default();
    let mut drive = DriveHandle::open_standalone_with_transport(
        Path::new("/fixture/b3a-drive"),
        Box::new(Scripted {
            inner: Box::new(fixture),
            opcode,
            error: Some(error),
            log: log.clone(),
        }),
        &StaticAllowlist::new(["/fixture/b3a-drive"]),
    )
    .unwrap();
    let outcomes = Outcomes::default();
    let captured = outcomes.clone();
    drive.set_audit_hook(move |event| {
        if let AuditEvent::Finished { outcome, .. } = event {
            captured.lock().unwrap().push(outcome.clone());
        }
    });
    (drive, log, outcomes)
}

fn assert_partial(error: &TapeIoError, raw: &[u8]) {
    assert!(
        matches!(error, crate::TapeIoError { kind: crate::TapeIoErrorKind::PartialBatchUncommittable {
        requested_records: 4, written_records: 2, requested_bytes: 2048,
        written_bytes: 1024, end_of_medium: false, sense: Some(s),
    }, .. } if s == raw),
        "{error:?}"
    );
}

/// Audit A1 inverted: residue is evidence on an error, never an audited credit.
#[test]
fn b3a_s1_failed_fixed_write_batch_is_uncommittable_with_sense() {
    let raw = sense(3, 0x0c, 0, 0, 2);
    let (mut drive, _, outcomes) = open(10, check(raw.clone(), 2048), vec![position_reply(10)]);
    let generation = drive.identity_generation();
    let error = drive.write_block_batch(&[0; 2048], 512).unwrap_err();
    assert_partial(&error, &raw);
    assert_eq!(drive.known_position(), None);
    assert_ne!(drive.identity_generation(), generation);
    assert!(
        matches!(outcomes.lock().unwrap().last(), Some(AuditOutcome::ScsiError { sense: Some(s), dirty: false, .. }) if s == &raw)
    );
}

/// Both fixed WRITE arms preserve identical partial evidence and invalidate unproved position.
#[test]
fn b3a_s1_both_arms_share_one_mapping() {
    let raw = sense(3, 0x0c, 0, 0, 2);
    let (mut plain, _, _) = open(10, check(raw.clone(), 2048), vec![position_reply(10)]);
    let (mut pipeline, _, _) = open(10, check(raw.clone(), 2048), vec![position_reply(10)]);
    let a = plain.write_block_batch(&[0; 2048], 512).unwrap_err();
    let b = pipeline
        .write_block_batch_pipelined(
            &[0; 2048],
            512,
            &remanence_scsi::read_write::build_write_fixed_cdb(4),
        )
        .unwrap_err();
    assert_partial(&a, &raw);
    assert_partial(&b, &raw);
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert_eq!(pipeline.known_position(), None);
}

/// HARDWARE ERROR, ABORTED COMMAND, ILLEGAL REQUEST and unflagged NO SENSE cannot credit a failed batch.
#[test]
fn b3a_s1_other_failure_keys_never_succeed() {
    for key in [4, 0x0b, 5, 0] {
        let raw = sense(key, 0x0c, 0, 0, 2);
        let (mut drive, _, _) = open(10, check(raw.clone(), 2048), vec![position_reply(10)]);
        let error = drive.write_block_batch(&[0; 2048], 512).unwrap_err();
        assert_partial(&error, &raw);
        assert_eq!(drive.known_position(), None);
    }
}

/// Usable full recovered WRITE remains recovered, with four records credited.
#[test]
fn b3a_s1_recovered_full_write_keeps_cursor() {
    let raw = sense(1, 0, 0, 0, 0);
    let (mut drive, _, outcomes) = open(10, check(raw.clone(), 2048), vec![position_reply(10)]);
    assert_eq!(
        drive
            .write_block_batch(&[0; 2048], 512)
            .unwrap()
            .records_written,
        4
    );
    assert_eq!(drive.known_position().map(|p| p.lba), Some(14));
    assert!(
        matches!(outcomes.lock().unwrap().last(), Some(AuditOutcome::Recovered { sense: s, .. }) if s == &raw)
    );
}

/// Early warning still uses the post-command READ POSITION delta as its arbiter.
#[test]
fn b3a_s1_early_warning_write_uses_read_position() {
    let (mut drive, log, _) = open(
        10,
        check(sense(0, 0, 2, 0x40, 0), 2048),
        vec![position_reply(10), position_reply(14)],
    );
    let result = drive.write_block_batch(&[0; 2048], 512).unwrap();
    assert_eq!(result.records_written, 4);
    assert!(result.early_warning);
    assert_eq!(drive.known_position().map(|p| p.lba), Some(14));
    assert_eq!(
        log.lock().unwrap().iter().filter(|c| c[0] == 0x34).count(),
        2
    );
}

/// Audit A2 inverted: EOD discards the cursor; re-positioning establishes LBA 13 before the next batch.
#[test]
fn b3a_s2_blank_check_batch_forgets_cursor() {
    let (mut drive, log, _) = open(
        8,
        check(sense(8, 0, 5, 0, 1), 2048),
        vec![position_reply(10), position_reply(13), vec![0xaa; 2048]],
    );
    let generation = drive.identity_generation();
    let mut buf = [0; 2048];
    let error = drive.read_block_batch(&mut buf, 512, 4, 4).unwrap_err();
    assert!(
        matches!(
            error,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::CheckConditionWithResidue {
                    bytes_transferred: 1536,
                    ..
                },
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(drive.known_position(), None);
    assert_eq!(drive.identity_generation(), generation);
    let before = log.lock().unwrap().len();
    assert!(matches!(
        drive.read_block_batch(&mut buf, 512, 4, 4),
        Err(crate::TapeIoError {
            kind: crate::TapeIoErrorKind::InvalidRequest(_),
            ..
        })
    ));
    assert_eq!(log.lock().unwrap().len(), before);
    assert_eq!(drive.position().unwrap().lba, 13);
    assert_eq!(
        drive
            .read_block_batch(&mut buf, 512, 4, 4)
            .unwrap()
            .position_after
            .lba,
        17
    );
}

/// Other failed fixed READs invalidate both cursor and identity evidence.
#[test]
fn b3a_s2_medium_error_batch_marks_unknown() {
    for (key, flags) in [(3, 0), (4, 0), (5, 0), (0, 0x40)] {
        let (mut drive, _, _) = open(
            8,
            check(sense(key, 0x11, 0, flags, 1), 2048),
            vec![position_reply(10)],
        );
        let generation = drive.identity_generation();
        drive
            .read_block_batch(&mut [0; 2048], 512, 4, 4)
            .unwrap_err();
        assert_eq!(drive.known_position(), None, "key {key}");
        assert_ne!(drive.identity_generation(), generation);
    }
}

/// BUSY and reservation conflict after fixed READ dispatch cannot preserve the cached cursor.
#[test]
fn b3a_s2_busy_batch_marks_unknown() {
    for status in [8, 0x18] {
        let (mut drive, _, _) = open(
            8,
            ScsiError::UnexpectedStatus { status },
            vec![position_reply(10)],
        );
        let generation = drive.identity_generation();
        assert!(matches!(
            drive.read_block_batch(&mut [0; 2048], 512, 4, 4),
            Err(crate::TapeIoError {
                kind: crate::TapeIoErrorKind::UnexpectedStatus(_),
                ..
            })
        ));
        assert_eq!(drive.known_position(), None);
        assert_ne!(drive.identity_generation(), generation);
    }
}

/// Pre-dispatch cancellation and invalid-input refusals keep the position evidence.
#[test]
fn b3a_s2_predispatch_refusals_keep_cursor() {
    for error in [
        ScsiError::Cancelled,
        ScsiError::InvalidInput("scripted pre-dispatch refusal"),
    ] {
        let (mut drive, _, _) = open(8, error, vec![position_reply(10)]);
        let generation = drive.identity_generation();
        drive
            .read_block_batch(&mut [0; 2048], 512, 4, 4)
            .unwrap_err();
        assert_eq!(drive.known_position().map(|p| p.lba), Some(10));
        assert_eq!(drive.identity_generation(), generation);
    }
}

/// Successful filemark and recovered fixed READs retain their correctly advanced cursor.
#[test]
fn b3a_s2_filemark_and_recovered_reads_keep_cursor() {
    for (key, flags, residue, records) in [(0, 0x80, 2, 2), (1, 0, 0, 4)] {
        let (mut drive, _, _) = open(
            8,
            check(sense(key, 0, 0, flags, residue), 2048),
            vec![position_reply(10)],
        );
        let result = drive.read_block_batch(&mut [0; 2048], 512, 4, 4).unwrap();
        assert_eq!(result.records_read, records);
        assert_eq!(
            drive.known_position().map(|p| p.lba),
            Some(10 + u64::from(records) + u64::from(flags == 0x80))
        );
    }
}

/// Unusable CHECK CONDITION is classified at the mapper, not by constructing a variant directly.
#[test]
fn b3a_map_scsi_unusable_sense_is_completion_unknown() {
    for raw in [vec![], vec![0; 18], vec![0x70, 0, 5, 0, 0, 0, 0, 24]] {
        let error = finished_map_scsi(check(raw.clone(), 99));
        assert!(
            matches!(&error, crate::TapeIoError { kind: crate::TapeIoErrorKind::Transport(ScsiError::CheckCondition { sense, bytes_transferred: 99 }), .. } if sense == &raw)
        );
        assert!(error.is_completion_unknown());
    }
}

/// Check one barrier path's error, cursor, shared dirty state and audit evidence.
fn filemark_unknown(raw: Vec<u8>, form: u8) {
    let (mut drive, log, outcomes) = open(0x10, check(raw.clone(), 0), vec![position_reply(10)]);
    drive.position().unwrap();
    let error = match form {
        0 => drive.write_filemarks(0).unwrap_err(),
        1 => drive.write_filemarks(1).unwrap_err(),
        2 => drive.write_filemarks_immediate(1).unwrap_err(),
        _ => drive.write_filemarks_pipelined(1).unwrap_err(),
    };
    assert!(
        matches!(
            error,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::Transport(_),
                ..
            }
        ),
        "form {form}: {error:?}"
    );
    assert!(error.is_completion_unknown());
    assert_eq!(drive.known_position(), None);
    assert_eq!(
        lock_drive_shared(&drive.shared).dirty.cause(),
        Some(DirtyCause::CompletionUnknown)
    );
    let outcome = if form == 3 {
        drive
            .pending_pipeline_audits
            .last()
            .unwrap()
            .outcome
            .clone()
    } else {
        outcomes.lock().unwrap().last().unwrap().clone()
    };
    assert!(
        matches!(outcome, AuditOutcome::ScsiError { dirty: true, sense: Some(s), .. } if s == raw)
    );
    assert_eq!(
        log.lock().unwrap().iter().filter(|c| c[0] == 0x10).count(),
        1
    );
}

/// Zero-count barrier with empty sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_barrier_empty_is_unknown() {
    filemark_unknown(vec![], 0);
}

/// Synchronous filemark with empty sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_sync_empty_is_unknown() {
    filemark_unknown(vec![], 1);
}

/// Immediate filemark with empty sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_immediate_empty_is_unknown() {
    filemark_unknown(vec![], 2);
}

/// Pipelined filemark with empty sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_pipelined_empty_is_unknown() {
    filemark_unknown(vec![], 3);
}

/// Zero-count barrier with malformed sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_barrier_malformed_is_unknown() {
    filemark_unknown(vec![0; 18], 0);
}

/// Synchronous filemark with malformed sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_sync_malformed_is_unknown() {
    filemark_unknown(vec![0; 18], 1);
}

/// Immediate filemark with malformed sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_immediate_malformed_is_unknown() {
    filemark_unknown(vec![0; 18], 2);
}

/// Pipelined filemark with malformed sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_pipelined_malformed_is_unknown() {
    filemark_unknown(vec![0; 18], 3);
}

/// Zero-count barrier with truncated sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_barrier_truncated_is_unknown() {
    filemark_unknown(vec![0x70, 0, 5, 0, 0, 0, 0, 24], 0);
}

/// Synchronous filemark with truncated sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_sync_truncated_is_unknown() {
    filemark_unknown(vec![0x70, 0, 5, 0, 0, 0, 0, 24], 1);
}

/// Immediate filemark with truncated sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_immediate_truncated_is_unknown() {
    filemark_unknown(vec![0x70, 0, 5, 0, 0, 0, 0, 24], 2);
}

/// Pipelined filemark with truncated sense is completion-unknown and retains its captured sense.
#[test]
fn b3a_filemarks_pipelined_truncated_is_unknown() {
    filemark_unknown(vec![0x70, 0, 5, 0, 0, 0, 0, 24], 3);
}

/// Variable WRITE has no fixed residual proof and must invalidate an empty completion.
#[test]
fn b3a_variable_write_empty_sense_is_unknown() {
    let (mut drive, _, _) = open(10, check(vec![], 512), vec![position_reply(10)]);
    let error = drive.write_block(&[0; 512]).unwrap_err();
    assert!(error.is_completion_unknown(), "{error:?}");
    assert_eq!(drive.known_position(), None);
}

/// Three-byte RECOVERED ERROR cannot credit a fixed WRITE without captured ASC/ASCQ.
#[test]
fn b3a_fixed_write_truncated_recovered_is_unknown() {
    let (mut drive, _, _) = open(10, check(vec![0x70, 0, 1], 2048), vec![position_reply(10)]);
    let error = drive.write_block_batch(&[0; 2048], 512).unwrap_err();
    assert!(error.is_completion_unknown());
    assert_eq!(drive.known_position(), None);
}

/// Host failure survives filemark mapping with its captured sense.
#[test]
fn b3a_filemark_host_failure_is_unknown() {
    let raw = sense(3, 0x0c, 0, 0, 0);
    let (mut drive, _, outcomes) = open(
        0x10,
        ScsiError::TransportError {
            status: 2,
            host_status: 3,
            driver_status: 8,
            info: 1,
            sense: raw.clone(),
        },
        vec![position_reply(10)],
    );
    drive.position().unwrap();
    assert!(drive
        .write_filemarks(0)
        .unwrap_err()
        .is_completion_unknown());
    assert_eq!(drive.known_position(), None);
    assert!(
        matches!(outcomes.lock().unwrap().last(), Some(AuditOutcome::ScsiError { dirty: true, sense: Some(s), .. }) if s == &raw)
    );
}

/// A usable filemark refusal remains known and keeps the prior cursor.
#[test]
fn b3a_filemark_usable_illegal_request_stays_known() {
    let (mut drive, _, _) = open(
        0x10,
        check(sense(5, 0x24, 0, 0, 0), 0),
        vec![position_reply(10)],
    );
    drive.position().unwrap();
    let error = drive.write_filemarks(0).unwrap_err();
    assert!(
        matches!(
            error,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::CheckCondition(_),
                ..
            }
        ),
        "{error:?}"
    );
    assert!(!error.is_completion_unknown());
    assert_eq!(drive.known_position().map(|p| p.lba), Some(10));
}

/// Variable BLANK CHECK still proves physical EOD after correcting a host-omitted residual.
#[test]
fn b3a_variable_blank_check_is_physical_eod() {
    let (mut drive, _, _) = open(
        8,
        check(sense(8, 0, 5, 0, 512), 512),
        vec![position_reply(10)],
    );
    assert!(drive
        .read_block(&mut [0; 512])
        .unwrap_err()
        .is_physical_eod());
}

/// A completed fixed READ cannot retain a cursor whose advance overflowed.
#[test]
fn b3b_fixed_read_advance_overflow_invalidates_position() {
    let (mut drive, log, _) = open(
        0xff,
        ScsiError::Cancelled,
        vec![position_reply(u64::MAX - 1), vec![0; 2048]],
    );
    let error = drive
        .read_block_batch(&mut [0; 2048], 512, 4, 4)
        .unwrap_err();
    assert!(
        matches!(
            error,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::InvalidRequest(_),
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(drive.known_position(), None);
    let before = log.lock().unwrap().len();
    assert!(drive.read_block_batch(&mut [0; 2048], 512, 4, 4).is_err());
    assert_eq!(log.lock().unwrap().len(), before);
}

/// A completed fixed WRITE cannot retain a cursor whose advance overflowed.
#[test]
fn b3b_fixed_write_advance_overflow_invalidates_position() {
    let (mut drive, log, _) = open(
        0xff,
        ScsiError::Cancelled,
        vec![position_reply(u64::MAX - 1)],
    );
    let error = drive.write_block_batch(&[0; 2048], 512).unwrap_err();
    assert!(
        matches!(
            error,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::InvalidRequest(_),
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(drive.known_position(), None);
    let before = log.lock().unwrap().len();
    assert!(drive.write_block_batch(&[0; 2048], 512).is_err());
    assert_eq!(log.lock().unwrap().len(), before);
}

/// Uncommittable pipelined records invalidate cursor and identity before returning.
#[test]
fn b3b_pipelined_uncommittable_invalidates_position() {
    let (mut drive, _, _) = open(
        10,
        check(sense(3, 0x0c, 0, 0, 2), 2048),
        vec![position_reply(10)],
    );
    let generation = drive.identity_generation();
    let error = drive
        .write_block_batch_pipelined(
            &[0; 2048],
            512,
            &remanence_scsi::read_write::build_write_fixed_cdb(4),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        crate::TapeIoError {
            kind: crate::TapeIoErrorKind::PartialBatchUncommittable { .. },
            ..
        }
    ));
    assert_eq!(drive.known_position(), None);
    assert_ne!(drive.identity_generation(), generation);
}

/// Verify each variable WRITE entry point independently, including its dirty audit.
fn variable_write_medium_failure(unpositioned: bool) {
    let (mut drive, _, outcomes) = open(
        10,
        check(sense(3, 0x0c, 0, 0, 512), 0),
        vec![position_reply(10)],
    );
    drive.position().unwrap();
    let generation = drive.identity_generation();
    let error = if unpositioned {
        drive.write_block_unpositioned(&[0; 512]).map(|_| ())
    } else {
        drive.write_block(&[0; 512]).map(|_| ())
    }
    .unwrap_err();
    assert!(error.check_condition_transfer().is_some(), "{error:?}");
    assert_eq!(drive.known_position(), None, "unpositioned={unpositioned}");
    assert_ne!(drive.identity_generation(), generation);
    assert!(matches!(
        outcomes.lock().unwrap().last(),
        Some(AuditOutcome::ScsiError { dirty: true, .. })
    ));
}

/// Usable positioned variable WRITE medium failure invalidates the cursor.
#[test]
fn b3b_variable_write_medium_failure_invalidates_position() {
    variable_write_medium_failure(false);
}

/// Usable unpositioned variable WRITE medium failure invalidates the cursor.
#[test]
fn b3b_variable_write_unpositioned_medium_failure_invalidates_position() {
    variable_write_medium_failure(true);
}

/// Zero-transfer ILLEGAL REQUEST and DATA PROTECT leave the variable WRITE cursor known.
#[test]
fn b3b_variable_write_definite_rejection_keeps_position() {
    for (key, asc) in [(5, 0x24), (7, 0x27)] {
        for unpositioned in [false, true] {
            let (mut drive, _, _) = open(
                10,
                check(sense(key, asc, 0, 0, 512), 0),
                vec![position_reply(10)],
            );
            drive.position().unwrap();
            let generation = drive.identity_generation();
            let result = if unpositioned {
                drive.write_block_unpositioned(&[0; 512]).map(|_| ())
            } else {
                drive.write_block(&[0; 512]).map(|_| ())
            };
            assert!(result.is_err());
            assert_eq!(drive.known_position().map(|p| p.lba), Some(10));
            assert_eq!(drive.identity_generation(), generation);
        }
    }
}

/// Truncated fixed WRITE completion reports dirty in the final audit event.
#[test]
fn b3b_truncated_fixed_write_audits_dirty() {
    let (mut drive, _, outcomes) =
        open(10, check(vec![0x70, 0, 1], 2048), vec![position_reply(10)]);
    assert!(drive
        .write_block_batch(&[0; 2048], 512)
        .unwrap_err()
        .is_completion_unknown());
    assert!(matches!(
        outcomes.lock().unwrap().last(),
        Some(AuditOutcome::ScsiError { dirty: true, .. })
    ));
}

/// Every issued command in these real handle paths is classified once, including inline position probes.
#[test]
fn b3b_one_classification_per_command() {
    type Command = (&'static str, u8, fn(&mut DriveHandle));
    let commands: &[Command] = &[
        ("read fixed", 8, |d| {
            let _ = d.read_block_batch(&mut [0; 2048], 512, 4, 4);
        }),
        ("read variable", 8, |d| {
            let _ = d.read_block(&mut [0; 512]);
        }),
        ("write fixed", 10, |d| {
            let _ = d.write_block_batch(&[0; 2048], 512);
        }),
        ("write pipelined", 10, |d| {
            let _ = d.write_block_batch_pipelined(
                &[0; 2048],
                512,
                &remanence_scsi::read_write::build_write_fixed_cdb(4),
            );
        }),
        ("write variable", 10, |d| {
            let _ = d.write_block(&[0; 512]);
        }),
        ("write unpositioned", 10, |d| {
            let _ = d.write_block_unpositioned(&[0; 512]);
        }),
        ("filemarks sync", 0x10, |d| {
            let _ = d.write_filemarks(1);
        }),
        ("filemarks immediate", 0x10, |d| {
            let _ = d.write_filemarks_immediate(1);
        }),
        ("filemarks zero", 0x10, |d| {
            let _ = d.write_filemarks(0);
        }),
        ("position", 0x34, |d| {
            let _ = d.position();
        }),
        ("locate", 0x2b, |d| {
            let _ = d.locate(10);
        }),
        ("load", 0x1b, |d| {
            let _ = d.load();
        }),
        ("unload", 0x1b, |d| {
            let _ = d.unload();
        }),
        ("prevent", 0x1e, |d| {
            let _ = d.prevent_medium_removal();
        }),
        ("allow", 0x1e, |d| {
            let _ = d.allow_medium_removal();
        }),
    ];
    for &(name, opcode, command) in commands {
        for kind in 0..4 {
            let error = match kind {
                0 => ScsiError::Cancelled, // Unmatched opcode leaves the fixture successful.
                1 => check(sense(5, 0x24, 0, 0, 0), 0),
                2 => check(vec![0x70, 0, 5], 0),
                _ => ScsiError::Io(std::io::Error::other("position drift")),
            };
            let (mut drive, log, _) = open(
                if kind == 0 { 0xff } else { opcode },
                error,
                vec![position_reply(10); 8],
            );
            // Seed without consuming an injected READ POSITION failure.
            if opcode != 0x34 {
                drive.position().unwrap();
            }
            let _context = drive.scoped_outcome_context(crate::outcome::OperationContext {
                phase: crate::outcome::Phase::Append,
                region: crate::outcome::Region::AtOrAfterCursor(10),
                ..Default::default()
            });
            let dispatched = log.lock().unwrap().len();
            let classified = crate::outcome::census::count();
            command(&mut drive);
            let dispatched = log.lock().unwrap().len() - dispatched;
            assert!(dispatched > 0, "{name}/{kind} did not reach dispatch");
            assert_eq!(
                crate::outcome::census::count() - classified,
                dispatched as u64,
                "{name}/{kind}"
            );
            assert!(drive.last_command_outcome().is_some(), "{name}/{kind}");
        }
    }
}

/// The recorded WRITE must describe the final recovered result, once per dispatch.
#[test]
fn b3b_final_recovered_outcome_matches_returned_result() {
    let (mut drive, log, _) = open(
        10,
        check(sense(1, 0, 0, 0, 0), 2048),
        vec![position_reply(10)],
    );
    let before = log.lock().unwrap().len();
    let count = crate::outcome::census::count();
    drive.write_block_batch(&[0; 2048], 512).unwrap();
    let recorded = drive.last_command_outcome().unwrap();
    assert!(!recorded.evidence.failed, "{recorded:?}");
    assert!(recorded.evidence.boundary.recovered);
    assert_eq!(recorded.evidence.opcode, Some(10));
    assert_eq!(recorded.evidence.records_transferred, Some(4));
    assert_eq!(
        crate::outcome::census::count() - count,
        (log.lock().unwrap().len() - before) as u64
    );
}

/// READ POSITION arbitration must not replace the final WRITE outcome in the carrier.
#[test]
fn b3b_final_eom_outcome_matches_returned_result() {
    let (mut drive, log, _) = open(
        10,
        check(sense(0, 0, 2, 0x40, 0), 2048),
        vec![position_reply(10), position_reply(14)],
    );
    let before = log.lock().unwrap().len();
    let count = crate::outcome::census::count();
    let result = drive.write_block_batch(&[0; 2048], 512).unwrap();
    let recorded = drive.last_command_outcome().unwrap();
    assert_eq!(recorded.evidence.opcode, Some(10));
    assert!(!recorded.evidence.failed, "{recorded:?}");
    assert_eq!(
        recorded.evidence.records_transferred,
        Some(result.records_written)
    );
    assert_eq!(
        recorded.evidence.transferred_bytes,
        Some(u64::from(result.bytes_written))
    );
    assert_eq!(
        crate::outcome::census::count() - count,
        (log.lock().unwrap().len() - before) as u64
    );
}

/// A GOOD transfer followed by cursor overflow is recorded as a failed command.
#[test]
fn b3b_final_overflow_outcome_matches_returned_result() {
    let (mut drive, log, _) = open(
        0xff,
        ScsiError::Cancelled,
        vec![position_reply(u64::MAX - 1)],
    );
    let before = log.lock().unwrap().len();
    let count = crate::outcome::census::count();
    assert!(drive.write_block_batch(&[0; 2048], 512).is_err());
    let recorded = drive.last_command_outcome().unwrap();
    assert!(recorded.evidence.failed, "{recorded:?}");
    assert_eq!(recorded.evidence.dispatch, crate::outcome::Dispatch::Issued);
    assert_eq!(
        recorded.evidence.position,
        crate::outcome::PositionEffect::Invalidated
    );
    assert_eq!(
        crate::outcome::census::count() - count,
        (log.lock().unwrap().len() - before) as u64
    );
}

/// A successful CDB with an unusable payload is a final failure, classified once.
#[test]
fn b3b_final_control_parse_failure_matches_returned_result() {
    type Control = fn(&mut DriveHandle) -> Result<(), TapeIoError>;
    let cases: &[(&str, Control, Vec<Vec<u8>>)] = &[
        ("position", |d| d.position().map(|_| ()), vec![vec![0; 3]]),
        (
            "alerts",
            |d| d.read_tape_alerts().map(|_| ()),
            vec![vec![0; 3]],
        ),
        (
            "wrap",
            |d| d.read_end_of_wrap_position().map(|_| ()),
            vec![vec![0; 3]],
        ),
        (
            "support",
            |d| d.probe_read_end_of_wrap_position_support().map(|_| ()),
            vec![vec![0; 3]],
        ),
        (
            "block limits",
            |d| d.read_config().map(|_| ()),
            vec![vec![0; 3]],
        ),
        (
            "mode",
            |d| d.read_config().map(|_| ()),
            vec![vec![0; 6], vec![0; 3]],
        ),
        (
            "partition",
            |d| d.require_single_partition(),
            vec![vec![0; 3]],
        ),
    ];
    for (name, command, replies) in cases {
        let (mut drive, log, _) = open(0xff, ScsiError::Cancelled, replies.clone());
        let before = log.lock().unwrap().len();
        let count = crate::outcome::census::count();
        let error = command(&mut drive).expect_err(name);
        let recorded = drive.last_command_outcome().expect(name);
        assert!(recorded.evidence.failed, "{name}: {error:?}; {recorded:?}");
        assert_eq!(recorded.evidence.scsi_status, Some(0), "{name}");
        assert_eq!(
            crate::outcome::census::count() - count,
            (log.lock().unwrap().len() - before) as u64,
            "{name}"
        );
    }
}

/// READ POSITION validity is judged by its final caller, without a second classification.
#[test]
fn b3b_final_position_validity_matches_returned_result() {
    for report_only in [false, true] {
        let mut reply = position_reply(10);
        reply[0] = 4; // LONU: report API preserves this; position API refuses it.
        let (mut drive, log, _) = open(0xff, ScsiError::Cancelled, vec![reply]);
        let before = log.lock().unwrap().len();
        let count = crate::outcome::census::count();
        if report_only {
            assert!(
                drive
                    .position_report()
                    .unwrap()
                    .logical_object_number_unknown
            );
        } else {
            assert!(drive.position().is_err());
        }
        assert_eq!(
            drive.last_command_outcome().unwrap().evidence.failed,
            !report_only
        );
        assert_eq!(
            drive
                .last_command_outcome()
                .unwrap()
                .evidence
                .service_action,
            Some(6)
        );
        assert_eq!(crate::outcome::census::count() - count, 1);
        assert_eq!(log.lock().unwrap().len() - before, 1);
    }
}

/// A GOOD WRITE followed by failing position proof retains the proof's media retry.
#[test]
fn b3b_final_tripwire_retry_uses_position_evidence() {
    let (mut drive, log, _) = open(0x34, check(sense(3, 0x11, 0, 0, 0), 0), vec![]);
    drive.record_device_position(
        parse_read_position_long(&position_reply(10))
            .unwrap()
            .position,
    );
    drive.position_check_bytes = 1;
    let _context = drive.scoped_outcome_context(crate::outcome::OperationContext {
        phase: crate::outcome::Phase::Append,
        region: crate::outcome::Region::AtOrAfterCursor(10),
        ..Default::default()
    });
    let before = log.lock().unwrap().len();
    let count = crate::outcome::census::count();
    let error = drive
        .write_block_batch_pipelined(
            &[0; 512],
            512,
            &remanence_scsi::read_write::build_write_fixed_cdb(1),
        )
        .unwrap_err();
    assert!(
        matches!(
            error,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::GoodWriteTripwire { .. },
                ..
            }
        ),
        "{error:?}"
    );
    let recorded = drive.last_command_outcome().unwrap();
    assert_eq!(recorded.evidence.opcode, Some(10));
    assert_eq!(recorded.evidence.scsi_status, Some(0));
    assert_eq!(recorded.evidence.host_transferred_bytes, Some(512));
    assert!(recorded.evidence.sense.is_none());
    let proof = recorded.evidence.post_transfer.as_ref().unwrap();
    assert_eq!(proof.opcode, Some(0x34));
    assert_eq!(proof.current_sense().unwrap().key, 3);
    assert_eq!(
        recorded.retry,
        crate::outcome::RetryCause::Media,
        "{recorded:?}"
    );
    assert_eq!(
        recorded.effect,
        crate::outcome::Effect::WriteFence(crate::outcome::FenceCause::TransferError)
    );
    assert_eq!(crate::outcome::census::count() - count, 2);
    assert_eq!(log.lock().unwrap().len() - before, 2);
}

/// A local validation refusal must not expose the previous command as its result.
#[test]
fn b3b_final_preflight_does_not_borrow_previous_result() {
    let (mut drive, log, _) = open(0xff, ScsiError::Cancelled, vec![position_reply(10)]);
    drive.position().unwrap();
    assert!(drive.last_command_outcome().is_some());
    let before = log.lock().unwrap().len();
    let count = crate::outcome::census::count();
    assert!(drive.write_block_batch(&[], 512).is_err());
    assert!(drive.last_command_outcome().is_none());
    assert_eq!(crate::outcome::census::count(), count);
    assert_eq!(log.lock().unwrap().len(), before);
}

/// A multi-partition refusal follows a real MODE SENSE, but never fences the tape.
#[test]
fn b3b_final_partition_refusal_is_operator_no_fence() {
    let (mut drive, log, _) = open(
        0xff,
        ScsiError::Cancelled,
        vec![vec![11, 0, 0, 0, 0x11, 6, 1, 1, 0, 0, 0, 0]],
    );
    let _context = drive.scoped_outcome_context(crate::outcome::OperationContext {
        phase: crate::outcome::Phase::Initialization,
        region: crate::outcome::Region::Bot,
        ..Default::default()
    });
    let before = log.lock().unwrap().len();
    let count = crate::outcome::census::count();
    let error = drive.require_single_partition().unwrap_err();
    assert!(
        matches!(
            error,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::MultiPartitionMedium { .. },
                ..
            }
        ),
        "{error:?}"
    );
    let recorded = drive.last_command_outcome().unwrap();
    assert!(recorded.evidence.failed);
    assert_eq!(recorded.evidence.dispatch, crate::outcome::Dispatch::Issued);
    assert_eq!(recorded.effect, crate::outcome::Effect::NoFence);
    assert_eq!(recorded.retry, crate::outcome::RetryCause::Operator);
    assert_eq!(crate::outcome::census::count() - count, 1);
    assert_eq!(log.lock().unwrap().len() - before, 1);
}

/// A failed post-WRITE position read cannot replace validated partial WRITE evidence.
#[test]
fn b3b_final_partial_write_retains_secondary_position_failure() {
    let raw = sense(0, 0, 2, 0x40, 2);
    let (mut drive, log, _) = open(10, check(raw.clone(), 2048), vec![vec![0; 3]]);
    drive.record_device_position(
        parse_read_position_long(&position_reply(10))
            .unwrap()
            .position,
    );
    let _context = drive.scoped_outcome_context(crate::outcome::OperationContext {
        phase: crate::outcome::Phase::Append,
        region: crate::outcome::Region::AtOrAfterCursor(10),
        ..Default::default()
    });
    let before = log.lock().unwrap().len();
    let count = crate::outcome::census::count();
    let error = drive
        .write_block_batch_pipelined(
            &[0; 2048],
            512,
            &remanence_scsi::read_write::build_write_fixed_cdb(4),
        )
        .unwrap_err();
    assert!(
        matches!(
            error,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::WriteFailureWithPositionError { .. },
                ..
            }
        ),
        "{error:?}"
    );
    let recorded = drive.last_command_outcome().unwrap();
    assert_eq!(recorded.evidence.opcode, Some(10));
    assert_eq!(recorded.evidence.sense, Some(raw));
    assert_eq!(recorded.evidence.records_transferred, Some(2));
    let proof = recorded.evidence.post_transfer.as_ref().unwrap();
    assert_eq!(proof.opcode, Some(0x34));
    assert!(proof.failed);
    assert_eq!(recorded.retry, crate::outcome::RetryCause::Transient);
    assert_eq!(
        recorded.effect,
        crate::outcome::Effect::WriteFence(crate::outcome::FenceCause::PartialBatch)
    );
    assert_eq!(crate::outcome::census::count() - count, 2);
    assert_eq!(log.lock().unwrap().len() - before, 2);
}

/// A parsed post-WRITE position mismatch is recorded as drift, not transport text.
#[test]
fn b3b_final_tripwire_drift_uses_position_evidence() {
    let (mut drive, log, _) = open(0xff, ScsiError::Cancelled, vec![position_reply(12)]);
    drive.record_device_position(
        parse_read_position_long(&position_reply(10))
            .unwrap()
            .position,
    );
    drive.position_check_bytes = 1;
    let _context = drive.scoped_outcome_context(crate::outcome::OperationContext {
        phase: crate::outcome::Phase::Append,
        region: crate::outcome::Region::AtOrAfterCursor(10),
        ..Default::default()
    });
    let before = log.lock().unwrap().len();
    let count = crate::outcome::census::count();
    let error = drive
        .write_block_batch_pipelined(
            &[0; 512],
            512,
            &remanence_scsi::read_write::build_write_fixed_cdb(1),
        )
        .unwrap_err();
    assert!(
        matches!(
            error,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::GoodWriteTripwire { .. },
                ..
            }
        ),
        "{error:?}"
    );
    let recorded = drive.last_command_outcome().unwrap();
    assert_eq!(recorded.evidence.scsi_status, Some(0));
    assert_eq!(recorded.evidence.records_transferred, Some(1));
    let proof = recorded.evidence.post_transfer.as_ref().unwrap();
    assert_eq!(proof.fact, crate::outcome::CompletionFact::PositionDrift);
    assert!(proof.failed);
    assert_eq!(
        recorded.effect,
        crate::outcome::Effect::WriteFence(crate::outcome::FenceCause::PositionDrift)
    );
    assert_eq!(crate::outcome::census::count() - count, 2);
    assert_eq!(log.lock().unwrap().len() - before, 2);
}

/// Ordinary READ/WRITE tripwires retain position-failure retry without inventing read damage.
#[test]
fn b3b_final_plain_tripwire_uses_position_evidence() {
    for writing in [true, false] {
        let (mut drive, log, _) = open(0x34, check(sense(3, 0x11, 0, 0, 0), 0), vec![vec![0; 512]]);
        drive.record_device_position(
            parse_read_position_long(&position_reply(10))
                .unwrap()
                .position,
        );
        drive.position_check_bytes = 1;
        let _context = drive.scoped_outcome_context(crate::outcome::OperationContext {
            phase: if writing {
                crate::outcome::Phase::Append
            } else {
                crate::outcome::Phase::Read
            },
            region: crate::outcome::Region::Committed(10..11),
            ..Default::default()
        });
        let before = log.lock().unwrap().len();
        let count = crate::outcome::census::count();
        let error = if writing {
            drive
                .write_block_batch(&[0; 512], 512)
                .map(|_| ())
                .unwrap_err()
        } else {
            drive
                .read_block_batch(&mut [0; 512], 512, 1, 1)
                .map(|_| ())
                .unwrap_err()
        };
        assert!(
            matches!(
                error,
                crate::TapeIoError {
                    kind: crate::TapeIoErrorKind::CheckCondition(_),
                    ..
                }
            ),
            "{error:?}"
        );
        let recorded = drive.last_command_outcome().unwrap();
        assert_eq!(
            recorded.retry,
            crate::outcome::RetryCause::Media,
            "{recorded:?}"
        );
        assert_eq!(
            recorded.effect,
            if writing {
                crate::outcome::Effect::WriteFence(crate::outcome::FenceCause::TransferError)
            } else {
                crate::outcome::Effect::Transient
            }
        );
        assert_eq!(recorded.evidence.opcode, Some(if writing { 10 } else { 8 }));
        assert_eq!(recorded.evidence.records_transferred, Some(1));
        assert_eq!(
            recorded.evidence.post_transfer.as_ref().unwrap().opcode,
            Some(0x34)
        );
        assert_eq!(crate::outcome::census::count() - count, 2);
        assert_eq!(log.lock().unwrap().len() - before, 2);
    }
}

/// Refusing READ POSITION after a GOOD WRITE does not prove the WRITE was refused.
#[test]
fn b3b_final_tripwire_rejection_does_not_refuse_completed_write() {
    let mut raw = sense(5, 0x24, 0, 0, 0);
    raw[0] = 0x70; // No residual claim contradicting the zero-byte rejection.
    let (mut drive, log, _) = open(0x34, check(raw, 0), vec![]);
    drive.record_device_position(
        parse_read_position_long(&position_reply(10))
            .unwrap()
            .position,
    );
    drive.position_check_bytes = 1;
    let _context = drive.scoped_outcome_context(crate::outcome::OperationContext {
        phase: crate::outcome::Phase::Append,
        region: crate::outcome::Region::AtOrAfterCursor(10),
        durability: crate::outcome::DurabilityEvidence {
            reproof: crate::outcome::Reproof::Proved,
            ..Default::default()
        },
        ..Default::default()
    });
    let before = log.lock().unwrap().len();
    let count = crate::outcome::census::count();
    let error = drive
        .write_block_batch_pipelined(
            &[0; 512],
            512,
            &remanence_scsi::read_write::build_write_fixed_cdb(1),
        )
        .unwrap_err();
    assert!(
        matches!(
            error,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::GoodWriteTripwire { .. },
                ..
            }
        ),
        "{error:?}"
    );
    let recorded = drive.last_command_outcome().unwrap();
    assert_eq!(recorded.evidence.host_transferred_bytes, Some(512));
    assert!(recorded
        .evidence
        .post_transfer
        .as_ref()
        .unwrap()
        .definitely_rejected());
    assert_eq!(
        recorded.effect,
        crate::outcome::Effect::WriteFence(crate::outcome::FenceCause::TransferError)
    );
    assert_eq!(drive.known_position(), None);
    assert!(recorded.requires_reproof);
    assert_eq!(crate::outcome::census::count() - count, 2);
    assert_eq!(log.lock().unwrap().len() - before, 2);
}

/// LOCATE's successful motion and failed position query remain distinct command evidence.
#[test]
fn b3b_final_locate_retains_position_failure() {
    let (mut drive, log, _) = open(0x34, check(sense(3, 0x11, 0, 0, 0), 0), vec![]);
    let before = log.lock().unwrap().len();
    let count = crate::outcome::census::count();
    assert!(drive.locate(10).is_err());
    let recorded = drive.last_command_outcome().unwrap();
    assert_eq!(recorded.retry, crate::outcome::RetryCause::Media);
    assert_eq!(recorded.evidence.opcode, Some(0x92));
    assert_eq!(recorded.evidence.scsi_status, Some(0));
    assert_eq!(
        recorded.evidence.post_transfer.as_ref().unwrap().opcode,
        Some(0x34)
    );
    assert_eq!(recorded.effect, crate::outcome::Effect::Transient);
    assert_eq!(crate::outcome::census::count() - count, 2);
    assert_eq!(log.lock().unwrap().len() - before, 2);
}

/// Consumers retain the failed WRITE after a different command replaces the handle record.
#[test]
fn b3b_error_carrier_survives_later_command_and_consumers() {
    use crate::outcome::{census, OperationContext, Phase, Region, RetryCause};
    let (mut drive, log, _) = open(
        10,
        check(sense(3, 0x0c, 0, 0, 2), 2048),
        vec![position_reply(10), position_reply(27)],
    );
    drive.position().unwrap();
    let _context = drive.scoped_outcome_context(OperationContext {
        phase: Phase::Append,
        region: Region::AtOrAfterCursor(10),
        ..Default::default()
    });
    let commands = log.lock().unwrap().len();
    let before = (census::count(), census::normalizations());
    let error = drive.write_block_batch(&[0; 2048], 512).unwrap_err();
    assert_eq!(
        census::count() - before.0,
        (log.lock().unwrap().len() - commands) as u64
    );
    assert_eq!(census::normalizations() - before.1, 1);
    let carried = error.outcome().clone();
    assert_eq!(carried.evidence.opcode, Some(10));
    assert_eq!(carried.retry, RetryCause::Media);
    drive.position().unwrap();
    assert_ne!(drive.last_command_outcome().unwrap(), &carried);
    let before = (census::total(), census::normalizations());
    for _ in 0..3 {
        assert_eq!(RetryCause::tape_io(&error), carried.retry);
        assert_eq!(error.completion_evidence(), carried.evidence);
        assert!(!error.is_physical_eod());
        let _ = error.is_completion_unknown();
        let _ = crate::verification_read_completion(&error, 512, 4);
    }
    assert_eq!(error.outcome(), &carried);
    assert_eq!((census::total(), census::normalizations()), before);
}

/// Parser, drive-control and readiness renderers reuse one final result per dispatched CDB.
#[test]
fn b3b_control_and_readiness_consumers_do_not_reclassify() {
    use crate::outcome::census;
    let (mut drive, log, _) = open(0x1b, check(sense(2, 4, 1, 0, 0), 0), vec![]);
    let before = (
        log.lock().unwrap().len(),
        census::count(),
        census::normalizations(),
    );
    let error = drive.load().unwrap_err();
    assert_eq!(log.lock().unwrap().len() - before.0, 1);
    assert_eq!(census::count() - before.1, 1);
    assert_eq!(census::normalizations() - before.2, 1);
    let before = (census::total(), census::normalizations());
    let readiness = crate::classify_media_readiness_error_ref(&error, crate::MediaFamily::Unknown);
    assert!(readiness.is_retryable_wait());
    assert_eq!(readiness.outcome(), error.outcome());
    assert!(!error.is_medium_removal_prevented());
    assert_eq!(error.completion_evidence(), error.outcome().evidence);
    assert_eq!((census::total(), census::normalizations()), before);

    let (mut drive, log, _) = open(0, check(sense(2, 4, 3, 0, 0), 0), vec![]);
    let before = (
        log.lock().unwrap().len(),
        census::count(),
        census::normalizations(),
    );
    let poll = drive
        .wait_for_media_readiness(
            crate::MediaFamily::Unknown,
            None,
            MediaReadinessWaitOptions {
                wait: true,
                timeout: std::time::Duration::from_secs(1),
                poll_interval: std::time::Duration::from_millis(1),
            },
            || None,
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(log.lock().unwrap().len() - before.0, 1);
    assert_eq!(census::count() - before.1, 1);
    assert_eq!(census::normalizations() - before.2, 1);
    assert!(poll.readiness.outcome().evidence.failed);
    let before = (census::total(), census::normalizations());
    let _ = poll.readiness.to_string();
    assert!(!poll.readiness.is_retryable_wait());
    assert_eq!((census::total(), census::normalizations()), before);
}

/// A local refusal has its own outcome and never borrows the preceding command's evidence.
#[test]
fn b3b_synthetic_admission_refusal_is_finished_on_return() {
    use crate::outcome::{census, Dispatch};
    let (mut drive, log, _) = open(0xff, ScsiError::Cancelled, vec![position_reply(10)]);
    drive.position().unwrap();
    let before = (log.lock().unwrap().len(), census::total());
    let error = drive.check_append_admission().unwrap_err();
    assert_eq!(log.lock().unwrap().len(), before.0);
    assert_eq!(census::total() - before.1, 1);
    assert_eq!(error.outcome().evidence.dispatch, Dispatch::NotIssued);
    assert_eq!(error.outcome().evidence.opcode, None);
    let before = (census::total(), census::normalizations());
    assert_eq!(
        crate::outcome::RetryCause::tape_io(&error),
        error.outcome().retry
    );
    assert_eq!(error.completion_evidence(), error.outcome().evidence);
    assert_eq!((census::total(), census::normalizations()), before);
}

/// Adapt direct mapper fixtures to the public finalized error carrier.
fn finished_map_scsi(error: ScsiError) -> TapeIoError {
    TapeIoError::new(super::map_scsi(error).kind)
}

/// A synthetic fact above the drive inherits its owner context once, and wrappers retain it.
#[test]
fn b3b_synthetic_adapter_context_and_wrapper_census() {
    use crate::outcome::{census, CommandClass, Effect, FenceCause, OperationContext, Phase};
    let (mut drive, _, _) = open(10, check(sense(3, 0x11, 0, 0, 0), 0), vec![]);
    let _scope = drive.scoped_outcome_context(OperationContext {
        phase: Phase::Append,
        ..Default::default()
    });
    let before = census::total();
    let error = drive.finish_adapter_error(
        crate::TapeIoErrorKind::PartialBatchUncommittable {
            requested_records: 2,
            written_records: 1,
            requested_bytes: 1024,
            written_bytes: 512,
            end_of_medium: false,
            sense: None,
        },
        CommandClass::Write,
    );
    assert_eq!(census::total() - before, 1);
    assert_eq!(
        error.outcome().effect,
        Effect::WriteFence(FenceCause::PartialBatch)
    );
    let before = (census::total(), census::normalizations());
    let wrapped = TapeIoError::new(crate::TapeIoErrorKind::PartitionProbeFailed(Box::new(
        error,
    )));
    assert_eq!(
        wrapped.outcome().effect,
        Effect::WriteFence(FenceCause::PartialBatch)
    );
    assert_eq!((census::total(), census::normalizations()), before);
}

/// Unknown LOAD completion retains the endpoint partition without another classification.
#[test]
fn b3b_motion_carrier_partition() {
    use crate::outcome::{census, Effect, PartitionIdentity};
    let (mut drive, _, _) = open(0x1b, check(vec![], 0), vec![]);
    let partition = match drive.endpoint().durable_key().unwrap() {
        crate::DriveKey::Bay { library_serial, .. } => {
            PartitionIdentity::Changer(library_serial.as_bytes().to_vec())
        }
        crate::DriveKey::Standalone { drive_serial } => {
            PartitionIdentity::Standalone(drive_serial.as_bytes().to_vec())
        }
    };
    let before = census::count();
    let error = drive.load().unwrap_err();
    assert_eq!(census::count() - before, 1);
    assert_eq!(error.outcome().effect, Effect::InventoryUnknown(partition));
}

/// A barrier adapter's new hard-EOM fact keeps write authority without a second consumer decision.
#[test]
fn b3b_synthetic_barrier_failure_census() {
    use crate::outcome::{census, Effect, FenceCause, RetryCause};
    let before = census::total();
    let error =
        TapeIoError::new_barrier_failure(crate::TapeIoErrorKind::HardEndOfMedium { sense: vec![] });
    assert_eq!(census::total() - before, 1);
    assert_eq!(
        error.outcome().effect,
        Effect::WriteFence(FenceCause::TransferError)
    );
    let before = (census::total(), census::normalizations());
    assert_eq!(RetryCause::tape_io(&error), error.outcome().retry);
    assert_eq!((census::total(), census::normalizations()), before);
}

/// A control transport's local-invalid-input error retains the existing non-dirty policy.
#[test]
fn b3b_control_carrier_preserves_local_error_dirty_rule() {
    let (mut drive, _, outcomes) = open(
        0x16,
        ScsiError::InvalidInput("fixture control refusal"),
        vec![position_reply(10)],
    );
    drive.position().unwrap();
    let before = crate::outcome::census::count();
    let error = drive.reserve().unwrap_err();
    assert_eq!(crate::outcome::census::count() - before, 1);
    assert!(matches!(
        error.kind,
        crate::DriveOpErrorKind::ScsiError(ScsiError::InvalidInput(_))
    ));
    assert_eq!(drive.known_position().map(|p| p.lba), Some(10));
    assert!(matches!(
        outcomes.lock().unwrap().last(),
        Some(AuditOutcome::ScsiError { dirty: false, .. })
    ));
}

/// Pipelined failure invalidates every cached library position consumer until a new device proof arrives.
#[test]
fn b3b_pipelined_failure_position_consumers() {
    struct Proof(u64, u64);
    // SAFETY: test-only proof issued from this fixture's immediately observed READ POSITION.
    #[cfg(test)]
    unsafe impl crate::AppendBoundaryProof for Proof {
        fn into_boundary(self) -> ([u8; 16], u32, u64, u64) {
            ([7; 16], 0, self.0, self.1)
        }
    }
    let (mut drive, log, _) = open(
        10,
        check(sense(3, 0x0c, 0, 0, 2), 2048),
        vec![position_reply(10), position_reply(73)],
    );
    drive.position().unwrap();
    let generation = drive.identity_generation();
    let _admission = drive
        .install_append_admission(Proof(10, generation))
        .unwrap();
    let error = drive
        .write_block_batch_pipelined(
            &[0; 2048],
            512,
            &remanence_scsi::read_write::build_write_fixed_cdb(4),
        )
        .unwrap_err();
    assert!(matches!(
        error.kind,
        TapeIoErrorKind::PartialBatchUncommittable { .. }
    ));
    assert_eq!(
        error.outcome().evidence.position,
        crate::outcome::PositionEffect::Invalidated
    );
    assert_eq!(drive.known_position(), None);
    assert_ne!(drive.identity_generation(), generation);
    let dispatched = log.lock().unwrap().len();
    assert!(drive.check_append_admission().is_err());
    assert!(drive
        .install_append_admission(Proof(10, generation))
        .is_err());
    assert!(drive.write_block_batch(&[0; 512], 512).is_err());
    assert!(drive.read_block_batch(&mut [0; 512], 512, 1, 1).is_err());
    assert_eq!(log.lock().unwrap().len(), dispatched);
    // A later device proof says 73, deliberately different from both the stale
    // start (10) and the partial-count guess (12). No consumer may invent 12.
    assert_eq!(drive.position().unwrap().lba, 73);
    assert_eq!(drive.known_position().unwrap().lba, 73);
    assert!(
        drive.check_append_admission().is_err(),
        "position alone cannot renew old identity authority"
    );
    assert_eq!(
        error.outcome().evidence.position,
        crate::outcome::PositionEffect::Invalidated
    );
}

/// Admitted LOCATE cannot return the stale pre-WRITE cursor after a partial pipelined failure.
#[test]
fn b3b_locate_admitted_append_after_pipeline_failure() {
    struct Proof(u64);
    // SAFETY: fixture-only authority bound to the immediately proved position.
    #[cfg(test)]
    unsafe impl crate::AppendBoundaryProof for Proof {
        fn into_boundary(self) -> ([u8; 16], u32, u64, u64) {
            ([7; 16], 0, 10, self.0)
        }
    }
    let (mut drive, log, _) = open(
        10,
        check(sense(3, 0x0c, 0, 0, 2), 2048),
        vec![position_reply(10), vec![0; 4]],
    );
    drive.position().unwrap();
    let _admission = drive
        .install_append_admission(Proof(drive.identity_generation()))
        .unwrap();
    drive
        .write_block_batch_pipelined(
            &[0; 2048],
            512,
            &remanence_scsi::read_write::build_write_fixed_cdb(4),
        )
        .unwrap_err();
    let before = log.lock().unwrap().len();
    assert!(
        drive.locate_admitted_append(10).is_err(),
        "failed reproof cannot return stale C"
    );
    let log = log.lock().unwrap();
    assert_eq!(
        log.len() - before,
        1,
        "configuration reprobe starts with READ BLOCK LIMITS; malformed response refuses it"
    );
    assert_eq!(log[before][0], 0x05);
    assert!(drive.known_position().is_none());
}

/// A partition wrapper and its public source carry the same finalized MODE SENSE failure.
#[test]
fn b3b_partition_probe_source_keeps_final_outcome() {
    let (mut drive, log, _) = open(0x1a, check(sense(3, 0x11, 0, 0, 0), 0), vec![]);
    let before = (crate::outcome::census::total(), log.lock().unwrap().len());
    let error = drive.require_single_partition().unwrap_err();
    let crate::TapeIoErrorKind::PartitionProbeFailed(source) = &error.kind else {
        panic!("{error:?}");
    };
    assert_eq!(source.outcome(), error.outcome());
    assert_eq!(crate::outcome::census::total() - before.0, 1);
    assert_eq!(log.lock().unwrap().len() - before.1, 1);
}

/// Deferred usable 02/04/02 still requests exactly one initializing LOAD.
#[test]
fn b3b_fix1_deferred_initializing_load() {
    for response in [0x71, 0x73] {
        let raw = if response == 0x71 {
            let mut raw = sense(2, 4, 2, 0, 0);
            raw[0] = response;
            raw
        } else {
            vec![response, 2, 4, 2, 0, 0, 0, 0]
        };
        let (mut drive, log, _) = open(0, check(raw, 0), vec![]);
        let poll = drive
            .wait_for_media_readiness_with_initializing_load(
                crate::MediaFamily::Lto9OrLater,
                None,
                MediaReadinessWaitOptions {
                    wait: true,
                    timeout: std::time::Duration::from_secs(1),
                    poll_interval: std::time::Duration::from_millis(1),
                },
                1,
                || None,
                |_| Ok(()),
            )
            .unwrap();
        assert!(poll.readiness.is_ready(), "{poll:?}");
        assert_eq!(
            log.lock().unwrap().iter().filter(|c| c[0] == 0x1b).count(),
            1
        );
    }
}

/// Retry follows the base typed variant, rather than all definite 05h/07h sense.
#[test]
fn b3b_fix1_rejection_retry_classes() {
    use crate::outcome::RetryCause;
    for (key, asc) in [(5, 0x24), (7, 0x20)] {
        let (mut drive, _, _) = open(10, check(sense(key, asc, 0, 0, 512), 0), vec![]);
        let error = drive.write_block_unpositioned(&[0; 512]).unwrap_err();
        let expected = if matches!(error.kind, crate::TapeIoErrorKind::DataProtect(_)) {
            RetryCause::Operator
        } else {
            RetryCause::Transient
        };
        assert_eq!(RetryCause::tape_io(&error), expected, "{error:?}");
    }
    for kind in [
        crate::TapeIoErrorKind::EphemeralReadOnly,
        crate::TapeIoErrorKind::BlockTooLarge {
            requested: 1024,
            limit: 512,
        },
    ] {
        assert_eq!(
            RetryCause::tape_io(&TapeIoError::new(kind)),
            RetryCause::Transient
        );
    }
}

/// A successful second invocation cannot inherit the first invocation's failed WRITE.
#[test]
fn b3b_fix1_media_failure_watermark_excludes_previous_invocation() {
    let (mut drive, _, _) = open(10, check(sense(5, 0x24, 0, 0, 512), 0), vec![]);
    let first = drive.media_write_dispatch_count();
    drive.write_block_unpositioned(&[0; 512]).unwrap_err();
    assert!(drive.media_failure_outcome_after(first).is_some());
    let second = drive.media_write_dispatch_count();
    drive.write_block_unpositioned(&[0; 512]).unwrap();
    assert_eq!(drive.media_write_dispatch_count(), second + 1);
    assert!(drive.media_failure_outcome_after(second).is_none());
}

/// A committed variable EOM/EW followed by a known position-query failure stays clean.
#[test]
fn b3b_fix1_variable_eom_position_failure_does_not_dirty_parent() {
    for key in [0, 1, 0x0d] {
        let log = CdbLog::default();
        let fixture = FixtureTransport::new().with_responses([
            include_bytes!("../../../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
            vec![8, 0x80, 0, 3, b'E', b'O', b'M'],
        ]);
        let position = Scripted {
            inner: Box::new(fixture),
            opcode: 0x34,
            error: Some(check(sense(5, 0x24, 0, 0, 0), 0)),
            log: log.clone(),
        };
        let mut drive = DriveHandle::open_standalone_with_transport(
            Path::new("/fixture/eom"),
            Box::new(Scripted {
                inner: Box::new(position),
                opcode: 10,
                error: Some(check(sense(key, 0, 2, 0x40, 0), 512)),
                log,
            }),
            &StaticAllowlist::new(["/fixture/eom"]),
        )
        .unwrap();
        let outcomes = Outcomes::default();
        let captured = outcomes.clone();
        drive.set_audit_hook(move |event| {
            if let AuditEvent::Finished { outcome, .. } = event {
                captured.lock().unwrap().push(outcome.clone());
            }
        });
        let generation = drive.identity_generation();
        drive.write_block(&[0; 512]).unwrap_err();
        // The failed READ POSITION itself invalidates once at the base.
        // The committed WRITE must not add another invalidation.
        assert_eq!(drive.identity_generation(), generation + 1);
        assert!(
            matches!(
                outcomes.lock().unwrap().last(),
                Some(AuditOutcome::ScsiError { dirty: false, .. })
            ),
            "{:?}",
            outcomes.lock().unwrap()
        );
    }
}

/// The real sg errno classifier feeds every WRITE form through the media gate.
/// A not-issued refusal preserves the cursor, dispatch watermark and clean snapshot.
#[test]
fn ioctl_capacity_write_refusals_keep_cursor_and_dispatch_watermark() {
    use crate::outcome::{census, Completion, Dispatch, Effect, OutcomeReason, PositionEffect};
    for errno in [nix::errno::Errno::ENOMEM, nix::errno::Errno::EINVAL] {
        for form in 0..8 {
            let opcode = if form < 4 { 0x0a } else { 0x10 };
            let (mut drive, log, outcomes) = open(
                opcode,
                remanence_scsi::sg_io::classify_ioctl_error(errno),
                vec![position_reply(10)],
            );
            drive.position().unwrap();
            let before = drive.media_write_dispatch_count();
            let generation = drive.identity_generation();
            let counts = (census::total(), census::normalizations());
            let error = match form {
                0 => drive.write_block(&[0; 512]).unwrap_err(),
                1 => drive.write_block_unpositioned(&[0; 512]).unwrap_err(),
                2 => drive.write_block_batch(&[0; 2048], 512).unwrap_err(),
                3 => drive
                    .write_block_batch_pipelined(
                        &[0; 2048],
                        512,
                        &remanence_scsi::read_write::build_write_fixed_cdb(4),
                    )
                    .unwrap_err(),
                4 => drive.write_filemarks(0).unwrap_err(),
                5 => drive.write_filemarks(1).unwrap_err(),
                6 => drive.write_filemarks_immediate(1).unwrap_err(),
                _ => drive.write_filemarks_pipelined(1).unwrap_err(),
            };
            assert!(
                matches!(
                    error,
                    TapeIoError {
                        kind: crate::TapeIoErrorKind::InvalidRequest(_),
                        ..
                    }
                ),
                "{errno}, form {form}: {error:?}"
            );
            assert!(!error.is_completion_unknown());
            assert_eq!(error.outcome().evidence.dispatch, Dispatch::NotIssued);
            assert_eq!(error.outcome().completion, Completion::Known);
            assert_eq!(error.outcome().effect, Effect::NoFence);
            assert_eq!(error.outcome().reason, OutcomeReason::NotIssued);
            assert_eq!(error.outcome().evidence.position, PositionEffect::Unchanged);
            assert_eq!(drive.known_position().map(|p| p.lba), Some(10));
            assert_eq!(drive.identity_generation(), generation);
            assert_eq!(
                drive.media_write_dispatch_count(),
                before,
                "{errno}, form {form}"
            );
            assert_eq!(lock_drive_shared(&drive.shared).dirty.cause(), None);
            drive.flush_pending_pipeline_audit();
            assert_eq!(census::total() - counts.0, 1, "{errno}, form {form}");
            assert_eq!(
                census::normalizations() - counts.1,
                1,
                "{errno}, form {form}"
            );
            assert!(matches!(
                outcomes.lock().unwrap().last(),
                Some(AuditOutcome::ScsiError { dirty: false, .. })
            ));
            assert_eq!(
                log.lock()
                    .unwrap()
                    .iter()
                    .filter(|c| c[0] == opcode)
                    .count(),
                1
            );
        }
    }
}

/// Other ioctl errors still carry uncertain write evidence and must advance the watermark.
#[test]
fn ioctl_other_write_errors_remain_completion_unknown() {
    for errno in [nix::errno::Errno::EIO, nix::errno::Errno::EINTR] {
        for opcode in [0x0a, 0x10] {
            let (mut drive, _, _) = open(
                opcode,
                remanence_scsi::sg_io::classify_ioctl_error(errno),
                vec![position_reply(10)],
            );
            drive.position().unwrap();
            let before = drive.media_write_dispatch_count();
            let error = if opcode == 0x0a {
                drive.write_block(&[0; 512]).unwrap_err()
            } else {
                drive.write_filemarks(1).unwrap_err()
            };
            assert!(error.is_completion_unknown());
            assert_eq!(drive.media_write_dispatch_count(), before + 1);
            assert_eq!(drive.known_position(), None);
            assert_eq!(
                lock_drive_shared(&drive.shared).dirty.cause(),
                Some(DirtyCause::CompletionUnknown)
            );
        }
    }
}

/// A failed host-only reserve returns a finalized refusal without borrowing the last CDB.
#[test]
fn identity_buffer_configuration_refusal_is_classified_once() {
    use crate::outcome::{census, Dispatch, Effect};

    struct RefusingReserve {
        inner: FixtureTransport,
        configured: bool,
    }
    impl SgTransport for RefusingReserve {
        fn configure_reserved_buffer(&mut self, wanted: u32) -> Result<u32, ScsiError> {
            if std::mem::replace(&mut self.configured, true) {
                return Err(ScsiError::InvalidInput("fixture reserve refusal"));
            }
            Ok(wanted.min(4096))
        }
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            self.inner.execute_in(cdb, buf)
        }
        fn execute_none(&mut self, _: &[u8]) -> Result<(), ScsiError> {
            panic!("buffer configuration must not issue a no-data CDB")
        }
        fn execute_out(&mut self, _: &[u8], _: &[u8]) -> Result<TransferOutcome, ScsiError> {
            panic!("buffer configuration must not write")
        }
    }
    let mut drive = DriveHandle::open_standalone_with_transport(
        Path::new("/fixture/identity-reserve"),
        Box::new(RefusingReserve {
            inner: FixtureTransport::new().with_responses([
                include_bytes!("../../../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
                vec![8, 0x80, 0, 3, b'B', b'3', b'A'],
                position_reply(10),
            ]),
            configured: false,
        }),
        &StaticAllowlist::new(["/fixture/identity-reserve"]),
    )
    .unwrap();
    drive.position().unwrap();
    let generation = drive.identity_generation();
    let counts = (census::total(), census::normalizations());
    let error = drive.configure_identity_read_buffer(0xFF_FFFF).unwrap_err();
    assert_eq!(error.outcome().evidence.dispatch, Dispatch::NotIssued);
    assert_eq!(error.outcome().evidence.opcode, None);
    assert_eq!(error.outcome().effect, Effect::NoFence);
    assert!(!error.is_completion_unknown());
    assert_eq!(drive.known_position().map(|p| p.lba), Some(10));
    assert_eq!(drive.identity_generation(), generation);
    assert_eq!(drive.media_write_dispatch_count(), 0);
    assert_eq!(census::total() - counts.0, 1);
    assert_eq!(census::normalizations() - counts.1, 1);
}
