//! Completion evidence through scripted transports; no device I/O.

use super::*;
use crate::transport::{FixtureTransport, SgTransport, TransferOutcome};
use crate::{DriveHandle, StaticAllowlist};
use std::path::Path;
use std::sync::{Arc, Mutex};

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
        matches!(error, TapeIoError::PartialBatchUncommittable {
        requested_records: 4, written_records: 2, requested_bytes: 2048,
        written_bytes: 1024, end_of_medium: false, sense: Some(s),
    } if s == raw),
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

/// Both fixed WRITE arms preserve identical partial evidence; the pipeline retains its old cursor.
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
    assert_eq!(pipeline.known_position().map(|p| p.lba), Some(10));
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
            TapeIoError::CheckConditionWithResidue {
                bytes_transferred: 1536,
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
        Err(TapeIoError::InvalidRequest(_))
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
            Err(TapeIoError::UnexpectedStatus(_))
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
        let error = map_scsi(check(raw.clone(), 99));
        assert!(
            matches!(&error, TapeIoError::Transport(ScsiError::CheckCondition { sense, bytes_transferred: 99 }) if sense == &raw)
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
        matches!(error, TapeIoError::Transport(_)),
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
    assert!(matches!(error, TapeIoError::CheckCondition(_)), "{error:?}");
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

/// The real sg errno classifier feeds every WRITE form through the media gate.
/// A not-issued refusal preserves the cursor, dispatch watermark and clean snapshot.
#[test]
fn ioctl_capacity_write_refusals_keep_cursor_and_dispatch_watermark() {
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
                matches!(error, TapeIoError::InvalidRequest(_)),
                "{errno}, form {form}: {error:?}"
            );
            assert!(!error.is_completion_unknown());
            assert_eq!(drive.known_position().map(|p| p.lba), Some(10));
            assert_eq!(drive.identity_generation(), generation);
            assert_eq!(
                drive.media_write_dispatch_count(),
                before,
                "{errno}, form {form}"
            );
            assert_eq!(lock_drive_shared(&drive.shared).dirty.cause(), None);
            drive.flush_pending_pipeline_audit();
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
