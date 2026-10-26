//! Exercise sense-residue accounting through fixture-backed READ/WRITE commands.

use super::transfer::*;
use super::*;
use crate::transport::{FixtureTransport, SgTransport, TransferOutcome};
use crate::{DriveHandle, StaticAllowlist};
use std::path::Path;

const REQUESTED: u32 = 262_144;
const EOD: [u8; 32] = [
    0xf0, 0x00, 0x08, 0x00, 0x04, 0x00, 0x00, 0x58, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x30, 0x00,
    0x64, 0x02, 0x20, 0x00, 0x06, 0x01, 0x41, 0x4f, 0x58, 0x30, 0x33, 0x32, 0x4c, 0x00, 0x00, 0x00,
];

/// Supply CHECK CONDITION only for the selected READ or WRITE CDB.
struct ResidueTransport {
    inner: FixtureTransport,
    sense: Vec<u8>,
    kernel_bytes: u32,
    fixed: bool,
}

impl SgTransport for ResidueTransport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        if cdb[0] == 8 {
            assert_eq!(cdb[1] & 1 != 0, self.fixed);
            return Err(ScsiError::CheckCondition {
                sense: self.sense.clone(),
                bytes_transferred: self.kernel_bytes,
            });
        }
        self.inner.execute_in(cdb, buf)
    }

    fn execute_out(&mut self, cdb: &[u8], _buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        assert_eq!(cdb[0], 10);
        assert_eq!(cdb[1] & 1 != 0, self.fixed);
        Err(ScsiError::CheckCondition {
            sense: self.sense.clone(),
            bytes_transferred: self.kernel_bytes,
        })
    }

    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        panic!("unexpected fixture command: {cdb:02x?}")
    }
}

fn drive(sense: Vec<u8>, kernel_bytes: u32, fixed: bool) -> DriveHandle {
    let inner = FixtureTransport::new().with_responses([
        include_bytes!("../../../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
        vec![8, 0x80, 0, 7, b'R', b'E', b'S', b'I', b'D', b'U', b'E'],
        vec![0; 32],
        vec![0; 32],
    ]);
    DriveHandle::open_standalone_with_transport(
        Path::new("/fixture/residue-drive"),
        Box::new(ResidueTransport {
            inner,
            sense,
            kernel_bytes,
            fixed,
        }),
        &StaticAllowlist::new(["/fixture/residue-drive"]),
    )
    .expect("fixture drive opens")
}

fn read_error(sense: Vec<u8>, kernel_bytes: u32, fixed: bool) -> TapeIoError {
    let mut drive = drive(sense, kernel_bytes, fixed);
    let mut buf = vec![0; REQUESTED as usize];
    if fixed {
        drive
            .read_block_batch(&mut buf, REQUESTED, 1, 1)
            .unwrap_err()
    } else {
        drive.read_block(&mut buf).unwrap_err()
    }
}

fn descriptor(residue: u64) -> Vec<u8> {
    let mut sense = vec![0x72, 8, 0, 5, 0, 0, 0, 16, 0x7e, 2, 0, 0, 0, 10, 0x80, 0];
    sense.extend_from_slice(&residue.to_be_bytes());
    sense
}

#[test]
fn sense_residue_variable_read_proves_physical_eod_and_preserves_kernel_count() {
    let error = read_error(EOD.to_vec(), REQUESTED, false);
    assert!(error.is_physical_eod());
    assert_eq!(error.check_condition_transfer(), Some((EOD.as_slice(), 0)));
    assert!(matches!(
        error,
        TapeIoError::CheckConditionWithResidue {
            source: ScsiError::CheckCondition {
                bytes_transferred: REQUESTED,
                ..
            },
            ..
        }
    ));
}

#[test]
fn sense_residue_fixed_read_converts_one_block_to_bytes_at_eod() {
    let mut sense = EOD.to_vec();
    sense[3..7].copy_from_slice(&1u32.to_be_bytes());
    assert!(read_error(sense, REQUESTED, true).is_physical_eod());
}

#[test]
fn sense_residue_nonzero_kernel_residual_is_authoritative() {
    for fixed in [false, true] {
        let error = read_error(EOD.to_vec(), 128, fixed);
        assert_eq!(error.check_condition_transfer().unwrap().1, 128);
        assert!(!error.is_physical_eod());
        assert!(matches!(error, TapeIoError::CheckCondition(_)));
    }
}

#[test]
fn sense_residue_fixed_format_without_valid_keeps_kernel_count() {
    let mut sense = EOD.to_vec();
    sense[0] = 0x70;
    let error = read_error(sense, REQUESTED, false);
    assert_eq!(error.check_condition_transfer().unwrap().1, REQUESTED);
    assert!(!error.is_physical_eod());
}

#[test]
fn sense_residue_descriptor_information_handles_variable_and_fixed_eod() {
    for fixed in [false, true] {
        let residue = if fixed { 1 } else { u64::from(REQUESTED) };
        assert!(read_error(descriptor(residue), REQUESTED, fixed).is_physical_eod());
    }
}

#[test]
fn sense_residue_invalid_or_truncated_descriptors_keep_kernel_count() {
    let valid = descriptor(u64::from(REQUESTED));
    let mut invalid = valid.clone();
    invalid[14] = 0;
    let mut outside_list = valid.clone();
    outside_list[7] = 4;
    for sense in [invalid, outside_list, valid[..23].to_vec(), vec![]] {
        assert_eq!(
            transfer_bytes_from_sense(&sense, REQUESTED, REQUESTED, None),
            REQUESTED
        );
    }
}

#[test]
fn sense_residue_clamps_large_residues_and_negative_variable_ili() {
    assert_eq!(
        transfer_bytes_from_sense(&descriptor(u64::MAX), REQUESTED, REQUESTED, Some(REQUESTED)),
        0
    );
    let mut sense = EOD.to_vec();
    sense[2] = 0x20;
    sense[3..7].copy_from_slice(&(-1i32).to_be_bytes());
    assert_eq!(
        transfer_bytes_from_sense(&sense, REQUESTED, REQUESTED, None),
        REQUESTED
    );
    assert_eq!(
        transfer_bytes_from_sense(&sense, REQUESTED, REQUESTED, Some(1)),
        0
    );
}

#[test]
fn sense_residue_deferred_eod_stays_completion_unknown() {
    for mut sense in [EOD.to_vec(), descriptor(u64::from(REQUESTED))] {
        sense[0] += 1;
        let error = read_error(sense, REQUESTED, false);
        assert!(error.is_completion_unknown());
        assert!(!error.is_physical_eod());
    }
}

#[test]
fn sense_residue_variable_writes_credit_only_transferred_bytes() {
    for key in [0x40, 0x4d] {
        let mut sense = EOD.to_vec();
        sense[2] = key;
        sense[3..7].copy_from_slice(&1024u32.to_be_bytes());
        let payload = vec![0; REQUESTED as usize];
        let outcome = drive(sense.clone(), REQUESTED, false)
            .write_block(&payload)
            .unwrap();
        assert_eq!(outcome.bytes_written, REQUESTED - 1024);
        assert!(outcome.early_warning);
        assert_eq!(outcome.end_of_medium, key == 0x4d);
        let error = drive(sense, REQUESTED, false)
            .write_block_unpositioned(&payload)
            .unwrap_err();
        assert!(matches!(error, TapeIoError::PartialBatchUncommittable {
            written_bytes, end_of_medium, ..
        } if written_bytes == u64::from(REQUESTED - 1024) && end_of_medium == (key == 0x4d)));
    }
}

#[test]
fn sense_residue_fixed_write_recovered_partial_is_completion_unknown() {
    for pipelined in [false, true] {
        let mut sense = EOD.to_vec();
        sense[2] = 1;
        sense[3..7].copy_from_slice(&1u32.to_be_bytes());
        let mut drive = drive(sense, REQUESTED, true);
        let payload = vec![0; REQUESTED as usize];
        let result = if pipelined {
            drive.write_block_batch_pipelined(
                &payload,
                REQUESTED,
                &remanence_scsi::read_write::build_write_fixed_cdb(1),
            )
        } else {
            drive.write_block_batch(&payload, REQUESTED)
        };
        assert!(result.unwrap_err().is_completion_unknown());
    }
}

#[test]
fn sense_residue_verification_batch_carries_corrected_eod() {
    let mut sense = EOD.to_vec();
    sense[3..7].copy_from_slice(&1u32.to_be_bytes());
    let error = drive(sense, REQUESTED, true)
        .read_block_batch_for_verification(&mut vec![0; REQUESTED as usize], REQUESTED, 1, 1)
        .unwrap_err();
    assert!(error.is_physical_eod());
}
