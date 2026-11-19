//! Read findings cross every restore renderer without another classification.
use super::*;
use remanence_library::outcome::{census, Effect, OperationContext, Phase, Region};

fn failed_command(write: bool) -> (DriveHandle, TapeIoError) {
    let (mut drive, commands) = crate::write_owner::tests::b3b_carrier_tests::drive_with_failure(
        if write { 10 } else { 8 },
        0,
        None,
    );
    let _context = drive.scoped_outcome_context(OperationContext {
        phase: if write { Phase::Append } else { Phase::Read },
        region: Region::Committed(0..1),
        ..Default::default()
    });
    let before = (
        *commands.lock().unwrap(),
        census::count(),
        census::normalizations(),
    );
    let error = if write {
        drive.write_block(&[0; 512]).unwrap_err()
    } else {
        drive.read_block(&mut [0; 512]).unwrap_err()
    };
    assert_eq!(
        census::count() - before.1,
        (*commands.lock().unwrap() - before.0) as u64
    );
    assert_eq!(census::normalizations() - before.2, 1);
    (drive, error)
}

/// Each adapter consumes the actual handle's classified error, including the write negative.
#[test]
fn read_finding_renderers_preserve_outcome_and_census() {
    for write in [false, true] {
        for consumer in ["format", "stream", "parity", "pool", "outcome"] {
            let (_drive, error) = failed_command(write);
            if write {
                assert!(!matches!(error.outcome().effect, Effect::ReadFinding(_)));
            } else {
                assert!(matches!(error.outcome().effect, Effect::ReadFinding(_)));
                assert!(!error.outcome().write_context);
                assert!(!error.outcome().recovery_required);
            }
            let before = (census::count(), census::total(), census::normalizations());
            let status = match consumer {
                "format" => {
                    status_from_format_error(&FormatError::TapeIo(error), "recorded".into())
                }
                "stream" => status_from_streaming_error(
                    &StreamingError::Format(FormatError::TapeIo(error)),
                    "recorded".into(),
                ),
                "parity" => {
                    status_from_parity_error(&ParityError::TapeIo(error), "recorded".into())
                }
                "pool" => status_from_pool_write_error(PoolWriteError::TapeIo(error)),
                "outcome" => attach_outcome_retry(error.outcome(), Status::internal("recorded")),
                _ => unreachable!(),
            };
            assert_eq!(
                (census::count(), census::total(), census::normalizations()),
                before,
                "{consumer}"
            );
            assert_eq!(status.code(), tonic::Code::Internal, "{consumer}");
            assert_eq!(
                status.metadata().get("remanence-retry").unwrap(),
                "media",
                "{consumer}"
            );
            assert_eq!(
                status
                    .metadata()
                    .get("remanence-finding")
                    .map(|value| value.to_str().unwrap()),
                if write { None } else { Some("media-damage") },
                "{consumer}, write={write}: finding label"
            );
        }
    }
}
