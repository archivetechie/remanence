//! Fix-round-2 adapters run real writes through DriveHandle and return production
//! overlap/parity errors for the mounted and initialization settlement tests.
use super::*;
use remanence_library::{BlockSink, DriveHandle, TapeIoError};
use remanence_parity::{
    ParityError, ParitySink, PhysicalPositionHint, RawTapeSink, RawWriteOutcome,
};

/// Produce an adapter refusal after successful media work without a durable object close.
pub(crate) fn post_write_adapter_error(drive: &mut DriveHandle, scenario: &str) -> TapeIoError {
    if scenario == "overlap" {
        let capacity = 2 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
        let manager = crate::io_memory::IoMemoryReservation::new(capacity).unwrap();
        let (_producer, _consumer, control) =
            crate::append_ring::create_append_ring(&manager, capacity, 50, 25, capacity).unwrap();
        // Forward to the drive without the unrelated append-admission wrapper.
        struct Inner<'a>(&'a mut DriveHandle);
        impl BlockSink for Inner<'_> {
            fn finish_adapter_error(
                &mut self,
                kind: remanence_library::TapeIoErrorKind,
            ) -> TapeIoError {
                self.0
                    .finish_adapter_error(kind, remanence_library::outcome::CommandClass::Write)
            }
            fn write_block(
                &mut self,
                bytes: &[u8],
            ) -> Result<remanence_library::WriteOutcome, TapeIoError> {
                self.0.write_block(bytes)
            }
            fn write_filemarks(
                &mut self,
                count: u32,
            ) -> Result<remanence_library::WriteFilemarksOutcome, TapeIoError> {
                self.0.write_filemarks(count)
            }
            fn position(&mut self) -> Result<remanence_library::TapePosition, TapeIoError> {
                self.0.position()
            }
        }
        let mut inner = Inner(drive);
        let mut overlap = overlap::OverlapBlockSink {
            inner: &mut inner,
            control,
            expected_initial_lba: 0,
            expected_next_lba: 999,
            initial_position_proved: true,
            write_started: false,
            low_water_events: 0,
        };
        let error = overlap.write_block(&vec![0; 262144]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("overlap write position drift during WRITE completion"),
            "{error}"
        );
        return error;
    }

    // This thin fixture supplies physical positions; all modifying commands and
    // adapter facts still pass through the real drive and its operation scope.
    struct Raw<'a> {
        drive: &'a mut DriveHandle,
        lba: u64,
        short: &'a std::cell::Cell<bool>,
    }
    impl RawTapeSink for Raw<'_> {
        fn finish_adapter_error(
            &mut self,
            kind: remanence_library::TapeIoErrorKind,
            class: remanence_library::outcome::CommandClass,
        ) -> TapeIoError {
            self.drive.finish_adapter_error(kind, class)
        }
        fn write_fixed_block(&mut self, bytes: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            let outcome = self.drive.write_block_unpositioned(bytes)?;
            self.lba += 1;
            Ok(RawWriteOutcome::WroteBlock {
                bytes_written: outcome.bytes_written - u32::from(self.short.get()),
                position_after: PhysicalPositionHint::new(self.lba),
                early_warning: false,
                end_of_medium: false,
            })
        }
        fn write_filemarks(
            &mut self,
            count: u32,
            _immed: bool,
        ) -> Result<RawWriteOutcome, ParityError> {
            self.drive.write_filemarks_immediate(count)?;
            self.lba += u64::from(count);
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.lba),
                early_warning: false,
                end_of_medium: false,
            })
        }
        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(self.lba))
        }
    }
    let short = std::cell::Cell::new(false);
    let mut raw = Raw {
        drive,
        lba: 0,
        short: &short,
    };
    let uuid = [0x52; 16];
    let block = 262144;
    let scheme = remanence_parity::default_scheme_for_block_size(block);
    let mut journal = overlap::PerObjectTestJournal {
        tape_uuid: uuid,
        bundles: vec![],
    };
    let mut sink = ParitySink::new_with_journal(
        &mut raw,
        &mut journal,
        scheme.clone(),
        uuid,
        block,
        remanence_parity::WriterIdentity::fixed(
            "fix2".into(),
            std::time::SystemTime::UNIX_EPOCH.into(),
        ),
    )
    .unwrap();
    sink.write_bootstrap().unwrap();
    let selected = model::SelectedTape {
        standalone_endpoint: None,
        pool_id: "fix2".into(),
        tape_uuid: uuid,
        block_size: block,
        parity_config: remanence_parity::ParityConfig::Scheme(scheme),
    };
    let cfg = remanence_state::TapePoolConfig {
        scheme: None,
        id: "fix2".into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block),
        min_object_size_bytes: 0,
    };
    let memory = crate::io_memory::IoMemoryReservation::new(1024 * 1024 * 1024).unwrap();
    let reservation = capacity::reserve_parity_object_capacity(
        sink.terminal_triple_capacity_runtime_state().unwrap(),
        sink.scheme(),
        &selected,
        (&cfg, 1, 0),
        1_000_000,
        2,
        &memory,
    )
    .unwrap();
    let (reservation, _permit) = reservation.into_parts();
    sink.begin_object_with_terminal_triple_reservation(reservation)
        .unwrap();
    sink.write_block(&vec![0; block as usize]).unwrap();
    let error = match scenario {
        "parity-size" => sink.write_block(&[0; 1]).unwrap_err(),
        "parity-poisoned" => {
            short.set(true);
            assert!(sink.write_block(&vec![0; block as usize]).is_err());
            sink.write_block(&vec![0; block as usize]).unwrap_err()
        }
        _ => panic!("unknown adapter scenario"),
    };
    assert!(
        error.to_string().contains(if scenario == "parity-size" {
            "configured fixed block size"
        } else {
            "ParitySink poisoned"
        }),
        "{error}"
    );
    error
}

/// Both raw decorators preserve a diagnostic caller's lack of write authority and retain its result.
#[test]
fn b3b_fix2_raw_decorators_forward_adapter_context() {
    use remanence_library::outcome::{CommandClass, OperationContext, Phase};
    let (mut drive, _) =
        crate::write_owner::tests::b3b_carrier_tests::drive_with_failure(255, 0, None);
    let _scope = drive.scoped_outcome_context(OperationContext {
        phase: Phase::Diagnostic,
        mounted: true,
        ..Default::default()
    });
    drive.write_block_unpositioned(&[0; 512]).unwrap();
    let error = {
        let mut raw = remanence_parity::DriveHandleRawSink::new(&mut drive);
        let stats = std::cell::Cell::new(staging::BlockSinkStats::default());
        let attempted = std::cell::Cell::new(false);
        let mut timed = staging::TimedRawTapeSink {
            inner: &mut raw,
            stats: &stats,
            last_write_finished: None,
        };
        let mut capacity = model::CapacityTrackingRawTapeSink::new(&mut timed, &attempted);
        capacity.finish_adapter_error(
            remanence_library::TapeIoErrorKind::OperationFailed("adapter".into()),
            CommandClass::Write,
        )
    };
    assert!(!error.outcome().write_context);
    assert_eq!(drive.media_failure_outcome_after(0), Some(error.outcome()));
}
