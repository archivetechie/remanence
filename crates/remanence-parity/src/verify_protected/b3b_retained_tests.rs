//! Retained verification reads must replay the original command's carrier,
//! including its authority context, without another normalization or decision.
use super::*;
use remanence_library::outcome::{census, CommandClass, Effect, OperationContext, Phase, Region};
use remanence_library::{TapeIoError, TapeIoErrorKind};

struct FailedRead {
    small: bool,
    residual: bool,
    reads: usize,
    position: PhysicalPositionHint,
}

impl RawTapeSource for FailedRead {
    fn configure_fixed_block_size(&mut self, _: u32) -> Result<(), ParityError> {
        Ok(())
    }
    fn locate_physical(&mut self, position: PhysicalPositionHint) -> Result<(), ParityError> {
        self.position = position;
        Ok(())
    }
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        panic!("unexpected EOD positioning")
    }
    fn space_filemarks(&mut self, _: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        panic!("unexpected filemark spacing")
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(self.position)
    }
    fn read_record(&mut self, _: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        self.reads += 1;
        let kind = if self.small {
            TapeIoErrorKind::ReadBufferTooSmall {
                actual: 1024,
                provided: 512,
            }
        } else {
            let mut sense = vec![0; 18];
            sense[0] = 0xf0;
            sense[2] = 3;
            sense[7] = 10;
            sense[12] = 0x11;
            let source = remanence_library::ScsiError::CheckCondition {
                sense,
                bytes_transferred: 512,
            };
            if self.residual {
                TapeIoErrorKind::CheckConditionWithResidue {
                    source,
                    bytes_transferred: 0,
                }
            } else {
                TapeIoErrorKind::CheckCondition(source)
            }
        };
        Err(TapeIoError::new_with_context(
            kind,
            CommandClass::Read,
            OperationContext {
                phase: Phase::Verification,
                region: Region::Committed(10..11),
                ..Default::default()
            },
        )
        .into())
    }
}

/// Both retained failure forms preserve their complete carrier over repeated replay, with no live reads or classifications.
#[test]
fn b3b_retained_read_replays_original_outcome_once() {
    for (small, residual) in [(false, false), (false, true), (true, false)] {
        let cursor = PhysicalPositionHint {
            partition: 0,
            lba: 10,
        };
        let mut live = FailedRead {
            small,
            residual,
            reads: 0,
            position: cursor,
        };
        let mut source = RetainedSource::new(&mut live, 512);
        source.enabled = true;
        source.recording = true;
        source.locate_physical(cursor).unwrap();
        let ParityError::TapeIo(original) = source.read_record(&mut [0; 512]).unwrap_err() else {
            panic!("expected retained tape error")
        };
        assert_eq!(original.outcome().effect, Effect::ReadFinding(10..11));
        assert_eq!(source.metadata.len(), 1);
        source.recording = false;
        let before = (census::total(), census::normalizations());
        for _ in 0..2 {
            source.locate_physical(cursor).unwrap();
            let ParityError::TapeIo(replayed) = source.read_record(&mut [0; 512]).unwrap_err()
            else {
                panic!("expected replayed tape error")
            };
            assert_eq!(
                (census::total(), census::normalizations()),
                before,
                "replay must only consume"
            );
            assert_eq!(replayed.outcome(), original.outcome());
            assert_eq!(format!("{replayed:?}"), format!("{original:?}"));
            assert_eq!(
                replayed.check_condition_transfer(),
                original.check_condition_transfer()
            );
        }
        drop(source);
        assert_eq!(
            live.reads,
            if small { 1 } else { 2 },
            "only the existing medium retry reaches the source"
        );
    }
}
