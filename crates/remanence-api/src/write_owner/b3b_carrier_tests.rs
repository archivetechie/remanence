//! Command-to-consumer census: renderers and persistence must not normalize again.
use super::*;
use remanence_library::outcome::{census, OperationContext, Phase, Region};
use remanence_library::{
    transport::TransferOutcome, FixtureTransport, ScsiError, StaticAllowlist, TapeIoError,
};

struct FailCommand {
    inner: Box<dyn SgTransport>,
    injected_sense: Option<Vec<u8>>,
    initializing_tur: bool,
    opcode: u8,
    commands: Arc<Mutex<usize>>,
    successful_writes: usize,
    message: Option<&'static str>,
    short_write: bool,
}
impl FailCommand {
    fn execute(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        *self.commands.lock().unwrap() += 1;
        if cdb[0] == 0 && self.initializing_tur {
            self.initializing_tur = false;
            let mut sense = vec![0; 18];
            sense[0] = 0x70;
            sense[2] = 2;
            sense[7] = 10;
            sense[12] = 4;
            sense[13] = 2;
            return Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred: 0,
            });
        }
        if cdb[0] == self.opcode {
            if self.successful_writes > 0 {
                self.successful_writes -= 1;
                return Ok(());
            }
            if let Some(message) = self.message {
                return Err(ScsiError::Io(std::io::Error::other(message)));
            }
            let mut sense = vec![0; 18];
            sense[0] = 0x70;
            sense[2] = 3;
            sense[7] = 10;
            sense[12] = 0x11;
            return Err(ScsiError::CheckCondition {
                sense: self.injected_sense.clone().unwrap_or(sense),
                bytes_transferred: 0,
            });
        }
        Ok(())
    }
}
impl SgTransport for FailCommand {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.execute(cdb)?;
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.execute(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.execute(cdb)?;
        Ok(TransferOutcome::clean(
            buf.len() as u32 - u32::from(self.short_write),
        ))
    }
}
fn drive(opcode: u8) -> (DriveHandle, Arc<Mutex<usize>>) {
    drive_with_failure(opcode, 0, None)
}

/// Give the real handle successful commands followed by a transport failure with diagnostic text.
pub(crate) fn drive_with_failure(
    opcode: u8,
    successful_writes: usize,
    message: Option<&'static str>,
) -> (DriveHandle, Arc<Mutex<usize>>) {
    let commands = Arc::new(Mutex::new(0));
    let inner = FixtureTransport::new().with_responses([
        include_bytes!("../../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
        vec![8, 0x80, 0, 3, b'C', b'6', b'A'],
        vec![0; 32],
    ]);
    let drive = DriveHandle::open_standalone_with_transport(
        Path::new("/fixture/c6"),
        Box::new(FailCommand {
            inner: Box::new(inner),
            injected_sense: None,
            initializing_tur: false,
            opcode,
            commands: commands.clone(),
            successful_writes,
            message,
            short_write: false,
        }),
        &StaticAllowlist::new(["/fixture/c6"]),
    )
    .unwrap();
    (drive, commands)
}
fn failed_command(write: bool) -> (DriveHandle, TapeIoError) {
    let (mut drive, commands) = drive(if write { 10 } else { 8 });
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
fn counters() -> (u64, u64) {
    (census::total(), census::normalizations())
}
fn assert_media(status: &Status) {
    assert_eq!(status.metadata().get("remanence-retry").unwrap(), "media");
}

/// Pool, format, streaming, parity, terminal and closeout retry renderers borrow the same device outcome.
#[test]
fn b3b_status_consumer_census() {
    for consumer in ["pool", "format", "stream", "parity", "terminal", "closeout"] {
        let (_drive, error) = failed_command(false);
        let before = counters();
        let status = match consumer {
            "pool" => crate::write_owner::restore::status_from_pool_write_error(
                crate::PoolWriteError::TapeIo(error),
            ),
            "format" => crate::write_owner::restore::status_from_format_error(
                &FormatError::TapeIo(error),
                "recorded".into(),
            ),
            "stream" => crate::write_owner::restore::status_from_streaming_error(
                &StreamingError::Format(FormatError::TapeIo(error)),
                "recorded".into(),
            ),
            "parity" => crate::write_owner::restore::status_from_parity_error(
                &ParityError::TapeIo(error),
                "recorded".into(),
            ),
            "terminal" => crate::write_owner::terminal_finalize::status_from_terminal_tail_error(
                &TerminalTailWriteError::Media(ParityError::TapeIo(error)),
            ),
            "closeout" => {
                use crate::closeout_obligation::RetryCauseExt;
                crate::closeout_obligation::RetryCause::identity(&crate::TapeIdentityError::Io(
                    error,
                ))
                .attach(Status::unavailable("recorded"))
            }
            _ => unreachable!(),
        };
        assert_media(&status);
        assert_eq!(counters(), before, "{consumer}");
    }
}

/// Write-session and restore preparation attach retry metadata without a second classification.
#[test]
fn b3b_preparation_renderer_census() {
    for write in [false, true] {
        let (mut drive, commands) = drive(1);
        let before = (
            *commands.lock().unwrap(),
            census::count(),
            census::normalizations(),
        );
        let status = if write {
            crate::write_owner::write_session::prepare_drive_for_write(
                &mut drive,
                &[1; 16],
                512,
                Uuid::new_v4(),
                crate::write_owner::write_session::WriteMediaPolicy::RewritableObject,
            )
            .unwrap_err()
        } else {
            crate::write_owner::restore::verify_loaded_tape_identity(&mut drive, &[1; 16])
                .unwrap_err()
        };
        assert_media(&status);
        assert_eq!(
            census::count() - before.1,
            (*commands.lock().unwrap() - before.0) as u64
        );
        assert_eq!(census::normalizations() - before.2, 1);
    }
}

/// The parity fence helper persists the WRITE's carried cause without classifying or decoding it.
#[test]
fn b3b_fence_consumer_census() {
    let typed = crate::tests::TypedRoot::open_with_tapes(&[([93; 16], "CENSUS93L9")]);
    let mut index = typed.catalog().write_handle();
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "C6", 0x100, "C6D", 0x400, 1,
    )));
    let library = open_model_library(world);
    let snapshot = library_snapshot_cell(library.library().clone());
    let cfg = test_typed_write_owner_config(&typed, &library, snapshot);
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "c6".into(),
        tape_uuid: [93; 16],
        block_size: 512,
        parity_config: ParityConfig::None,
    };
    let (mut drive, error) = failed_command(true);
    let error = ParityError::TapeIo(error);
    let before = counters();
    let outcome =
        crate::write_owner::checkpoint::write_failure_outcome(&mut drive, 0, Some(&error));
    let (status, persisted) = fence_failed_parity_raw_write(
        &mut index,
        &cfg,
        &selected,
        "append",
        None,
        None,
        &outcome,
        &error.to_string(),
        Status::internal(error.to_string()),
    );
    assert!(persisted);
    assert_media(&status);
    assert_eq!(index.list_active_tape_io_fences().unwrap().len(), 1);
    assert_eq!(counters(), before);
}

/// A scoped cursor contradiction adds one host fact; an already-scoped suffix error is consumed unchanged.
#[test]
fn b3b_scoped_admission_fence_uses_classified_cause() {
    use crate::append_admission::{AdmissionError, AdmissionErrorKind};
    for scoped in [false, true] {
        let typed = crate::tests::TypedRoot::open_with_tapes(&[([93; 16], "SCOPED93L9")]);
        let mut index = typed.catalog().write_handle();
        let (_, library) = unthreaded_cartridge_world("B3B-SCOPED");
        let cfg = test_typed_write_owner_config(
            &typed,
            &library,
            library_snapshot_cell(library.library().clone()),
        );
        let error = AdmissionError::new(if scoped {
            AdmissionErrorKind::SessionSuffixContradiction {
                tape_uuid: Uuid::from_bytes([93; 16]),
                partition: 0,
                lba: 10,
                sequence: 1,
                incarnation: 1,
                detail: "scoped suffix".into(),
            }
        } else {
            AdmissionErrorKind::Io(TapeIoError::new(
                remanence_library::TapeIoErrorKind::FilemarkEncountered,
            ))
        });
        let before = counters();
        assert!(super::super::checkpoint::fence_admission_contradiction(
            &mut index,
            &cfg,
            [93; 16],
            512,
            Some((9, 10)),
            &error,
        ));
        assert_eq!(counters(), (before.0 + u64::from(!scoped), before.1));
        let fences = index.list_active_tape_io_fences().unwrap();
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].reason, "session_suffix_contradiction");
    }
}

/// Audit settlement consumes a failed WRITE outcome; a missing result adds exactly one host fact.
#[test]
fn b3b_settle_audit_consumer_census() {
    for scenario in ["device", "adapter"] {
        check_settle_audit(scenario);
    }
}

/// An abandoned operation with a successful dispatch adds one host fact and one transfer_error fence.
#[test]
fn b3b_mounted_unclassified_end_one_fence() {
    check_settle_audit("abandoned");
}

/// A prior applied named-tail outcome cannot suppress a later abandoned invocation's fence.
#[test]
fn b3b_mounted_abandoned_after_applied_outcome_one_fence() {
    check_settle_audit("abandoned-after-applied");
}

/// Exercise the production settlement boundary twice to pin idempotent consequence application.
fn check_settle_audit(scenario: &str) {
    let post_write = matches!(scenario, "overlap" | "parity-size" | "parity-poisoned");
    let abandoned = post_write || matches!(scenario, "abandoned" | "abandoned-after-applied");
    let mut typed = crate::tests::TypedRoot::open_with_tapes(&[([94; 16], "SETTLE94L9")]);
    let mut index = typed.catalog().write_handle();
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "C6", 0x100, "C6D", 0x400, 1,
    )));
    let library = open_model_library(world);
    let snapshot = library_snapshot_cell(library.library().clone());
    let mut cfg = test_typed_write_owner_config(&typed, &library, snapshot);
    let mut drive = if scenario != "device" {
        let (mut drive, _) = drive(255);
        drive.write_block_unpositioned(&[0; 512]).unwrap();
        drive
    } else {
        failed_command(true).0
    };
    let invocation_before = if scenario == "abandoned-after-applied" {
        let outcome =
            remanence_library::outcome::classify(remanence_library::outcome::OutcomeInput {
                class: remanence_library::outcome::CommandClass::Write,
                context: OperationContext {
                    phase: Phase::Append,
                    durability: remanence_library::outcome::DurabilityEvidence {
                        named_live_tail: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                evidence: remanence_library::outcome::CompletionEvidence::unknown(),
            });
        assert_eq!(outcome.effect, remanence_library::outcome::Effect::NoFence);
        drive.retain_operation_failure(0, outcome);
        drive.mark_media_failure_outcome_applied();
        let before = drive.media_write_dispatch_count();
        drive.write_block_unpositioned(&[0; 512]).unwrap();
        before
    } else {
        0
    };
    cfg.placement =
        crate::write_owner::ActorPlacement::Mounted(crate::write_owner::MountedDriveContext {
            endpoint: drive.endpoint().durable_key().unwrap().clone(),
            tape_uuid: [94; 16],
            barcode: None,
            cartridge_generation: None,
            identity_generation: drive.identity_generation(),
            actor: remanence_state::AuditActor::System,
            audit_error: Arc::new(Mutex::new(None)),
            audit_session: Arc::new(Mutex::new(None)),
            direct_replay_fault: None,
        });
    crate::write_owner::mounted_commands::open_audit(&mut index, &cfg, Uuid::new_v4()).unwrap();
    if scenario == "adapter" {
        let error = ParityError::TapeIo(TapeIoError::new_barrier_failure(
            remanence_library::TapeIoErrorKind::HardEndOfMedium { sense: vec![] },
        ));
        let before = counters();
        let outcome =
            crate::write_owner::checkpoint::write_failure_outcome(&mut drive, 0, Some(&error));
        assert_eq!(counters(), before);
        assert_eq!(drive.media_failure_outcome_after(0), Some(&outcome));
    }
    if post_write {
        let _scope = drive.scoped_outcome_context(OperationContext {
            phase: Phase::Append,
            mounted: true,
            ..Default::default()
        });
        let dispatches = drive.media_write_dispatch_count();
        let error = crate::pool_write::post_write_adapter_error(&mut drive, scenario);
        assert!(
            drive.media_write_dispatch_count() > dispatches,
            "adapter follows a WRITE"
        );
        assert!(error.outcome().write_context);
        assert_eq!(
            error.outcome().evidence.dispatch,
            remanence_library::outcome::Dispatch::NotIssued
        );
        assert_eq!(drive.media_failure_outcome_after(0), Some(error.outcome()));
    }
    let before = counters();
    crate::write_owner::mounted_commands::settle_audit(
        &mut index,
        &cfg,
        &mut drive,
        invocation_before,
    )
    .unwrap();
    assert_eq!(counters(), (before.0 + u64::from(abandoned), before.1));
    let fences = index.list_active_tape_io_fences().unwrap();
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].reason, "transfer_error");
    let before = counters();
    crate::write_owner::mounted_commands::settle_audit(
        &mut index,
        &cfg,
        &mut drive,
        invocation_before,
    )
    .unwrap();
    assert_eq!(counters(), before);
    assert_eq!(index.list_active_tape_io_fences().unwrap().len(), 1);
    // The typed fence record is the audit: one across both settlements.
    drop((index, cfg));
    let raised = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(typed.records(|record| {
            (record.tag() == remanence_state::library_log::Tag::FenceRaised).then_some(())
        }));
    assert_eq!(raised.len(), 1);
}

/// Two successful mounted WRITEs followed by unknown completion leave one transfer_error fence and one audit.
#[test]
fn b3b_mounted_write_failure_raises_one_fence() {
    mounted_message_failure("transport lost after earlier good writes");
}

/// The `FenceRaised` records the library log holds, read after every holder of the runtime is dropped.
fn fence_raised_records(typed: &mut crate::tests::TypedRoot) -> usize {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(typed.records(|record| {
            (record.tag() == remanence_state::library_log::Tag::FenceRaised).then_some(())
        }))
        .len()
}

/// Exercise a mounted operation without a durable named-tail ending and return its stored policy and retry.
fn mounted_message_failure(
    message: &'static str,
) -> (
    remanence_library::outcome::ClassifiedOutcome,
    String,
    String,
) {
    let mut typed = crate::tests::TypedRoot::open_with_tapes(&[([95; 16], "MSGFAIL95L9")]);
    let mut index = typed.catalog().write_handle();
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "C7", 0x100, "C7D", 0x400, 1,
    )));
    let library = open_model_library(world);
    let snapshot = library_snapshot_cell(library.library().clone());
    let mut cfg = test_typed_write_owner_config(&typed, &library, snapshot);
    let (mut drive, commands) = drive_with_failure(10, 2, Some(message));
    cfg.placement =
        crate::write_owner::ActorPlacement::Mounted(crate::write_owner::MountedDriveContext {
            endpoint: drive.endpoint().durable_key().unwrap().clone(),
            tape_uuid: [95; 16],
            barcode: None,
            cartridge_generation: None,
            identity_generation: drive.identity_generation(),
            actor: remanence_state::AuditActor::System,
            audit_error: Arc::new(Mutex::new(None)),
            audit_session: Arc::new(Mutex::new(None)),
            direct_replay_fault: None,
        });
    let _context = drive.scoped_outcome_context(OperationContext {
        phase: Phase::Append,
        mounted: true,
        ..Default::default()
    });
    let before = (*commands.lock().unwrap(), census::count());
    let status = crate::write_owner::mounted_commands::with_evidence(
        &mut index,
        &cfg,
        &mut drive,
        |_, drive| {
            drive.write_block_unpositioned(&[0; 512]).unwrap();
            drive.write_block_unpositioned(&[0; 512]).unwrap();
            let error = drive.write_block_unpositioned(&[0; 512]).unwrap_err();
            Err::<(), _>(crate::write_owner::restore::status_from_pool_write_error(
                crate::PoolWriteError::TapeIo(error),
            ))
        },
    )
    .unwrap_err();
    assert_eq!(*commands.lock().unwrap() - before.0, 3);
    assert_eq!(census::count() - before.1, 3);
    let fences = index.list_active_tape_io_fences().unwrap();
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].reason, "transfer_error");
    let reason = fences[0].reason.clone();
    let outcome = drive.media_failure_outcome_after(0).unwrap().clone();
    let retry: String = status
        .metadata()
        .get("remanence-retry")
        .unwrap()
        .to_str()
        .unwrap()
        .into();
    drop((index, cfg));
    assert_eq!(fence_raised_records(&mut typed), 1);
    (outcome, reason, retry)
}

/// Diagnostic collisions preserve equal outcomes, reasons and retry through mounted, parity, admission and closeout consumers.
#[test]
fn b3b_same_cause_two_messages() {
    use crate::closeout_obligation::RetryCauseExt;
    let messages = [
        "transport disconnected",
        "position drift; partial batch; reset unit attention",
    ];
    assert_eq!(
        mounted_message_failure(messages[0]),
        mounted_message_failure(messages[1])
    );
    for consumer in ["parity", "admission", "closeout"] {
        let results: Vec<_> = messages
            .into_iter()
            .map(|message| {
                let typed = crate::tests::TypedRoot::open_with_tapes(&[([96; 16], "TWOMSG96L9")]);
                let mut index = typed.catalog().write_handle();
                let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
                    "C7", 0x100, "C7D", 0x400, 1,
                )));
                let library = open_model_library(world);
                let snapshot = library_snapshot_cell(library.library().clone());
                let cfg = test_typed_write_owner_config(&typed, &library, snapshot);
                let write = consumer == "parity";
                let (mut drive, _) =
                    drive_with_failure(if write { 10 } else { 8 }, 0, Some(message));
                let _context = drive.scoped_outcome_context(OperationContext {
                    phase: if write {
                        Phase::Append
                    } else {
                        Phase::Admission
                    },
                    ..Default::default()
                });
                let error = if write {
                    drive.write_block_unpositioned(&[0; 512]).unwrap_err()
                } else {
                    drive.read_block(&mut [0; 512]).unwrap_err()
                };
                let outcome = error.outcome().clone();
                let before = counters();
                let status = match consumer {
                    "parity" => {
                        let selected = SelectedTape {
                            standalone_endpoint: None,
                            pool_id: "c7".into(),
                            tape_uuid: [96; 16],
                            block_size: 512,
                            parity_config: ParityConfig::None,
                        };
                        let error = ParityError::TapeIo(error);
                        fence_failed_parity_raw_write(
                            &mut index,
                            &cfg,
                            &selected,
                            "append",
                            None,
                            None,
                            &outcome,
                            &error.to_string(),
                            Status::internal(error.to_string()),
                        )
                        .0
                    }
                    "admission" => {
                        let error = crate::append_admission::AdmissionError::new(
                            crate::append_admission::AdmissionErrorKind::Io(error),
                        );
                        crate::append_admission::daemon_admission_error(
                            AuditActor::System,
                            &mut index,
                            &super::terminal_types::TerminalConfig::terminal_config(&cfg),
                            [96; 16],
                            error,
                            &outcome,
                        )
                    }
                    "closeout" => crate::closeout_obligation::RetryCause::identity(
                        &crate::TapeIdentityError::Io(error),
                    )
                    .attach(Status::unavailable(message)),
                    _ => unreachable!(),
                };
                assert_eq!(counters(), before, "{consumer}");
                let reasons: Vec<_> = index
                    .list_active_tape_io_fences()
                    .unwrap()
                    .into_iter()
                    .map(|f| f.reason)
                    .collect();
                assert_eq!(
                    reasons,
                    if write {
                        vec!["transfer_error".to_string()]
                    } else {
                        vec![]
                    }
                );
                (
                    outcome,
                    reasons,
                    status
                        .metadata()
                        .get("remanence-retry")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string(),
                )
            })
            .collect();
        assert_eq!(results[0], results[1], "{consumer}");
    }
}

/// A later definite rejection without a named ending bypasses the blanket fence and re-proves the exact C.
#[test]
fn b3b_mounted_definite_rejection_no_fence() {
    struct RejectLater {
        replies: std::collections::VecDeque<(u8, Result<Vec<u8>, ScsiError>)>,
        armed: Arc<Mutex<bool>>,
        commands: Arc<Mutex<Vec<u8>>>,
    }
    impl SgTransport for RejectLater {
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            self.commands.lock().unwrap().push(cdb[0]);
            let (opcode, reply) = self.replies.pop_front().expect("scripted input command");
            assert_eq!(cdb[0], opcode);
            let bytes = reply?;
            assert!(bytes.len() <= buf.len());
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(TransferOutcome::clean(bytes.len() as u32))
        }
        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
            self.commands.lock().unwrap().push(cdb[0]);
            assert!(matches!(cdb[0], 0x01 | 0x10 | 0x92));
            Ok(())
        }
        fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
            self.commands.lock().unwrap().push(cdb[0]);
            if cdb[0] == 10 && std::mem::take(&mut *self.armed.lock().unwrap()) {
                return Err(ScsiError::CheckCondition {
                    sense: vec![0x72, 5, 0x24, 0],
                    bytes_transferred: 0,
                });
            }
            assert_eq!(cdb[0], 10);
            Ok(TransferOutcome::clean(buf.len() as u32))
        }
    }
    const UUID: [u8; 16] = [97; 16];
    const BLOCK: u32 = 4096;
    let mut typed = crate::tests::TypedRoot::open_with_tapes(&[(UUID, "REJECT97L9")]);
    let mut index = typed.catalog().write_handle();
    let world = VirtualWorld::single_drive("C7", 0x100, "C7D", 0x400, 1);
    let bootstrap = crate::build_tape_bootstrap(
        UUID,
        BLOCK,
        ParityConfig::None,
        "2026-11-04T00:00:00Z",
        "b3b-test",
    );
    let mut block = vec![0; BLOCK as usize];
    write_bootstrap_block(&bootstrap, &mut block).unwrap();
    let world = Arc::new(Mutex::new(world));
    let library = open_model_library(world.clone());
    let snapshot = library_snapshot_cell(library.library().clone());
    let mut cfg = test_typed_write_owner_config(&typed, &library, snapshot);
    let armed = Arc::new(Mutex::new(false));
    let commands = Arc::new(Mutex::new(Vec::new()));
    let position = |lba: u64| {
        let mut reply = vec![0; 32];
        reply[8..16].copy_from_slice(&lba.to_be_bytes());
        (0x34, Ok(reply))
    };
    let filemark = || {
        // The synthetic zero-byte READ carries a full valid byte residual.
        let mut sense = vec![0xf0, 0, 0x80, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0, 1];
        sense[3..7].copy_from_slice(&BLOCK.to_be_bytes());
        (
            8,
            Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred: 0,
            }),
        )
    };
    // Initial WRITE/WFM prove 1/2. Both C-1 probes return FILEMARK,
    // the suffix probe returns EOD, and each restoration proves C=2 again.
    let replies = std::collections::VecDeque::from([
        (
            0x12,
            Ok(include_bytes!("../../../../fixtures/inquiry/drive1-lto9.bin").to_vec()),
        ),
        (0x12, Ok(vec![8, 0x80, 0, 3, b'C', b'7', b'D'])),
        position(1),
        position(2),
        position(1),
        filemark(),
        position(2),
        (
            8,
            Err(ScsiError::CheckCondition {
                sense: vec![0x72, 8, 0, 5],
                bytes_transferred: 0,
            }),
        ),
        position(1),
        filemark(),
        position(2),
    ]);
    let mut drive = DriveHandle::open_standalone_with_transport(
        Path::new("/fixture/c7-rejection"),
        Box::new(RejectLater {
            replies,
            armed: armed.clone(),
            commands: commands.clone(),
        }),
        &StaticAllowlist::new(["/fixture/c7-rejection"]),
    )
    .unwrap();
    cfg.placement =
        crate::write_owner::ActorPlacement::Mounted(crate::write_owner::MountedDriveContext {
            endpoint: drive.endpoint().durable_key().unwrap().clone(),
            tape_uuid: UUID,
            barcode: None,
            cartridge_generation: None,
            identity_generation: drive.identity_generation(),
            actor: AuditActor::System,
            audit_error: Arc::new(Mutex::new(None)),
            audit_session: Arc::new(Mutex::new(None)),
            direct_replay_fault: None,
        });
    let _context = drive.scoped_outcome_context(OperationContext {
        phase: Phase::Append,
        mounted: true,
        ..Default::default()
    });
    let result = crate::write_owner::mounted_commands::with_evidence(
        &mut index,
        &cfg,
        &mut drive,
        |_, drive| {
            drive.rewind().unwrap();
            drive.write_block(&block).unwrap();
            drive.write_filemarks(1).unwrap();
            *armed.lock().unwrap() = true;
            let error = drive.write_block(&block).unwrap_err();
            assert!(error.check_condition_transfer().is_some(), "{error}");
            assert!(!*armed.lock().unwrap());
            Err::<(), _>(crate::write_owner::restore::status_from_pool_write_error(
                crate::PoolWriteError::TapeIo(error),
            ))
        },
    );
    assert!(result.is_err());
    assert!(index.list_active_tape_io_fences().unwrap().is_empty());
    drop((index, cfg));
    assert_eq!(fence_raised_records(&mut typed), 0);
    let before = commands.lock().unwrap().len();
    let ctx = crate::append_admission::context(UUID, BLOCK, 0, 2);
    let admission = crate::append_admission::admit_append_boundary(
        &mut drive,
        crate::append_admission::AppendAuthority::Checkpoint { eod: ctx.boundary },
        ctx,
    )
    .unwrap();
    assert_eq!(admission.boundary().2, 2);
    assert_eq!(drive.known_position().unwrap().lba, 2);
    assert_eq!(
        &commands.lock().unwrap()[before..],
        &[0x92, 0x34, 0x08, 0x34, 0x08, 0x92, 0x34, 0x08, 0x34]
    );
    drive.check_append_admission().unwrap();
}

/// The bootstrap short-WRITE producer classifies one extra fact and retains it through Status rendering.
#[test]
fn b3b_bootstrap_partial_fact_classifies_once() {
    use remanence_library::outcome::{CompletionFact, Effect, FenceCause};
    // This narrow adapter exposes the same drive as DriveHandleSink, without
    // requiring an unrelated physical-admission fixture for the producer test.
    struct BootstrapSink(DriveHandle);
    impl remanence_library::BlockSink for BootstrapSink {
        fn append_admission_drive(&mut self) -> Option<&mut DriveHandle> {
            Some(&mut self.0)
        }
        fn write_block(
            &mut self,
            buf: &[u8],
        ) -> Result<remanence_library::WriteOutcome, TapeIoError> {
            self.0.write_block(buf)
        }
        fn write_filemarks(
            &mut self,
            _: u32,
        ) -> Result<remanence_library::WriteFilemarksOutcome, TapeIoError> {
            panic!("partial bootstrap must not write a filemark")
        }
        fn position(&mut self) -> Result<remanence_library::TapePosition, TapeIoError> {
            self.0.position()
        }
    }
    let commands = Arc::new(Mutex::new(0));
    let mut after = vec![0; 32];
    after[8..16].copy_from_slice(&1u64.to_be_bytes());
    let inner = FixtureTransport::new().with_responses([
        include_bytes!("../../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
        vec![8, 0x80, 0, 3, b'C', b'8', b'A'],
        after,
    ]);
    let drive = DriveHandle::open_standalone_with_transport(
        Path::new("/fixture/c8"),
        Box::new(FailCommand {
            inner: Box::new(inner),
            injected_sense: None,
            initializing_tur: false,
            opcode: 0xff,
            commands: commands.clone(),
            successful_writes: 0,
            message: None,
            short_write: true,
        }),
        &StaticAllowlist::new(["/fixture/c8"]),
    )
    .unwrap();
    let mut sink = BootstrapSink(drive);
    let _context = sink.0.scoped_outcome_context(OperationContext {
        phase: Phase::Append,
        mounted: true,
        ..Default::default()
    });
    let before = (*commands.lock().unwrap(), census::total());
    let error = crate::pool_write::write_fixed_blocks(&mut sink, 512, &[0; 512]).unwrap_err();
    let commands = *commands.lock().unwrap() - before.0;
    assert_eq!(commands, 2, "WRITE, READ POSITION");
    assert_eq!(census::total() - before.1, commands as u64 + 1);
    let crate::PoolWriteError::TapeIo(ref tape_error) = error else {
        panic!("{error:?}")
    };
    assert_eq!(
        tape_error.outcome().effect,
        Effect::WriteFence(FenceCause::PartialBatch)
    );
    assert_eq!(
        tape_error.outcome().evidence.fact,
        CompletionFact::PartialWrite
    );
    assert_eq!(
        sink.0.media_failure_outcome_after(0),
        Some(tape_error.outcome())
    );
    let before = census::total();
    let outcome = crate::write_owner::checkpoint::write_failure_outcome(&mut sink.0, 0, None);
    assert_eq!(outcome, *tape_error.outcome());
    let status = crate::write_owner::restore::status_from_pool_write_error(error);
    assert!(status.metadata().get("remanence-retry").is_some());
    assert_eq!(
        census::total(),
        before,
        "retention and rendering classify nothing"
    );
}

#[path = "b3b_census_matrix.rs"]
mod matrix;

/// A missing write context after dispatch is an abandoned write, even with a carried refusal.
#[test]
fn b3b_fix1_s15_rejects_contextless_carrier() {
    use remanence_library::outcome::{Effect, FenceCause};
    for kind in [
        remanence_library::TapeIoErrorKind::HardEndOfMedium { sense: vec![] },
        remanence_library::TapeIoErrorKind::OperationFailed("adapter failed".into()),
    ] {
        let (mut drive, _) = drive_with_failure(255, 0, None);
        let before = drive.media_write_dispatch_count();
        drive.write_block_unpositioned(&[0; 512]).unwrap();
        let error = ParityError::TapeIo(TapeIoError::new(kind));
        let outcome =
            crate::write_owner::checkpoint::write_failure_outcome(&mut drive, before, Some(&error));
        assert_eq!(
            outcome.effect,
            Effect::WriteFence(FenceCause::TransferError)
        );
    }
}

/// Production terminal-prefix hard EOM fences before fallible recovery persistence.
#[test]
fn b3b_fix1_terminal_prefix_eom_fence_precedes_recovery() {
    use remanence_parity::{FileRowsKind, FileRowsReport, TapeFileEntry, TapeFileKind};
    let root = tempfile::tempdir().unwrap();
    let uuid = [0x68; 16];
    let mut journal = remanence_state::authority::row_fixtures::RowAuthorityBuilder::new(
        root.path().join("rows"),
        uuid,
        262144,
        remanence_parity::default_scheme_for_block_size(262144),
    )
    .publication(remanence_parity::authority::ShadowPublication::one(
        FileRowsReport {
            kind: FileRowsKind::BotBootstrap,
            entries: vec![TapeFileEntry {
                tape_file_number: 0,
                kind: TapeFileKind::Bootstrap,
                block_count: 1,
                physical_start_hint: Some(0),
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: None,
                object_recovery_row: None,
            }],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
        },
    ))
    .publication(remanence_parity::authority::ShadowPublication::one(
        FileRowsReport {
            kind: FileRowsKind::FixtureBoundary,
            entries: vec![],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
        },
    ))
    .build()
    .unwrap();
    let snapshot = journal.committed_snapshot_bounded().unwrap();
    let plan = remanence_parity::plan_checkpointed_terminal_index_close(
        &snapshot,
        remanence_parity::ParityMapDiagnostics {
            writer_version: "fix1".into(),
            write_timestamp: "2026-11-04T00:00:00Z".into(),
        },
    )
    .unwrap();
    let mut pos = vec![0; 32];
    pos[8..16].copy_from_slice(&plan.tail_start_lba.to_be_bytes());
    let fixture = FixtureTransport::new().with_responses([
        include_bytes!("../../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
        vec![8, 0x80, 0, 3, b'E', b'O', b'M'],
        pos.clone(),
        pos,
    ]);
    let mut drive = DriveHandle::open_standalone_with_transport(
        Path::new("/fixture/eom"),
        Box::new(FailCommand {
            inner: Box::new(fixture),
            injected_sense: Some(vec![
                0x70, 0, 0x4d, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0,
            ]),
            initializing_tur: false,
            opcode: 0x10,
            commands: Arc::new(Mutex::new(0)),
            successful_writes: 0,
            message: None,
            short_write: false,
        }),
        &StaticAllowlist::new(["/fixture/eom"]),
    )
    .unwrap();
    let _scope = drive.scoped_outcome_context(OperationContext {
        phase: Phase::Finalization,
        ..Default::default()
    });
    let before = drive.media_write_dispatch_count();
    struct EomRaw<'a>(&'a mut DriveHandle);
    impl remanence_parity::RawTapeSink for EomRaw<'_> {
        fn write_fixed_block(
            &mut self,
            _: &[u8],
        ) -> Result<remanence_parity::RawWriteOutcome, ParityError> {
            panic!("empty checkpointed prefix must only dispatch its barrier")
        }
        fn write_filemarks(
            &mut self,
            count: u32,
            _: bool,
        ) -> Result<remanence_parity::RawWriteOutcome, ParityError> {
            let result = self.0.write_filemarks(count)?;
            Ok(remanence_parity::RawWriteOutcome::WroteFilemark {
                position_after: result.position_after.into(),
                early_warning: result.early_warning,
                end_of_medium: result.end_of_medium,
            })
        }
        fn position(&mut self) -> Result<remanence_parity::PhysicalPositionHint, ParityError> {
            Ok(self.0.position()?.into())
        }
        fn finish_adapter_error(
            &mut self,
            kind: remanence_library::TapeIoErrorKind,
            class: remanence_library::outcome::CommandClass,
        ) -> TapeIoError {
            self.0.finish_adapter_error(kind, class)
        }
    }
    let error = remanence_parity::close_checkpointed_terminal_index_prefix(
        &mut EomRaw(&mut drive),
        &mut journal,
        &snapshot,
        &plan,
        remanence_parity::TerminalPrefixReconcileEvidence::Complete,
    )
    .unwrap_err();
    assert!(
        matches!(&error, ParityError::TapeIo(e) if matches!(e.kind,
        remanence_library::TapeIoErrorKind::HardEndOfMedium { .. })),
        "{error:?}"
    );
    assert_eq!(drive.media_write_dispatch_count(), before + 1);
    let ParityError::TapeIo(ref carried) = error else {
        unreachable!()
    };
    assert_eq!(
        carried.outcome().effect,
        remanence_library::outcome::Effect::WriteFence(
            remanence_library::outcome::FenceCause::TransferError
        )
    );
    let outcome =
        crate::write_owner::checkpoint::write_failure_outcome(&mut drive, before, Some(&error));
    let typed = crate::tests::TypedRoot::open_with_tapes(&[(uuid, "EOM001L9")]);
    let mut index = typed.catalog().write_handle();
    let cfg = crate::write_owner::TerminalWriteConfig {
        authority: crate::write_owner::TerminalAuthority::Typed(
            typed.catalog().library_runtime().unwrap(),
        ),
        io_memory: crate::io_memory::IoMemoryReservation::new(1024 * 1024).unwrap(),
        mounted_endpoint: None,
        checkpoint_journal_dir: typed.paths().journal_dir.join("tapes"),
    };
    let mut lease = remanence_state::authority::fixtures::AuthorityBuilder::new(
        &cfg.checkpoint_journal_dir,
        uuid,
    )
    .build()
    .unwrap()
    .acquire_exclusive()
    .unwrap();
    let selected = SelectedTape {
        standalone_endpoint: None,
        tape_uuid: uuid,
        block_size: 262144,
        parity_config: ParityConfig::None,
        pool_id: "default".into(),
    };
    let spec = crate::write_owner::terminal_types::TerminalFinalizeSpec {
        actor: AuditActor::System,
        tape_uuid: uuid,
        block_size: 262144,
        pool_config: None,
        trigger: TerminalFinalizationTrigger::ReachedLowWatermark,
        operation_id: None,
        manual: None,
    };
    let status = crate::write_owner::terminal_finalize::terminal_prefix_write_failure(
        &mut index, &cfg, &mut lease, &spec, &selected, true, &error, &outcome,
    );
    assert!(
        status.message().contains("persist terminal recovery"),
        "{status}"
    );
    let fences = index.list_active_tape_io_fences().unwrap();
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].reason, "transfer_error");
}

/// Production overlap detects drift after a completed WRITE; an unclosed mounted invocation fences once.
#[test]
fn b3b_fix2_mounted_overlap_post_write() {
    check_settle_audit("overlap");
}

/// A production parity size refusal cannot erase the earlier writes at settlement.
#[test]
fn b3b_fix2_mounted_parity_size_post_write() {
    check_settle_audit("parity-size");
}

/// A production poisoned-sink refusal cannot erase the earlier writes at settlement.
#[test]
fn b3b_fix2_mounted_parity_poisoned_post_write() {
    check_settle_audit("parity-poisoned");
}

/// The same settlement change preserves R6's dispatched, zero-byte definite rejection.
#[test]
fn b3b_fix2_mounted_r6_definite_rejection() {
    b3b_mounted_definite_rejection_no_fence();
}

/// Reclassifying at the ending preserves the command outcome until a durable tail exempts it.
#[test]
fn live_ending_outcome_matches_the_command_outcome_without_a_named_tail() {
    use crate::write_owner::checkpoint::live_ending_outcome;
    use remanence_library::outcome::*;
    let context = OperationContext {
        phase: Phase::Append,
        region: Region::Unknown,
        mounted: true,
        ..Default::default()
    };
    let check = |key, asc| {
        let mut sense = vec![0; 18];
        sense[0] = 0x70;
        sense[2] = key;
        sense[7] = 10;
        sense[12] = asc;
        CompletionEvidence::from_scsi(&ScsiError::CheckCondition {
            sense,
            bytes_transferred: 0,
        })
    };
    let mut partial = CompletionEvidence::success();
    partial.failed = true;
    partial.fact = CompletionFact::PartialWrite;
    partial.requested_records = Some(4);
    partial.records_transferred = Some(2);
    let mut drift = CompletionEvidence::success();
    drift.failed = true;
    drift.fact = CompletionFact::PositionDrift;
    let mut filemark = CompletionEvidence::unknown();
    filemark.opcode = Some(0x10);
    let mut cases = vec![(
        "abandoned",
        finish_abandoned_write_with_retry(context.clone(), RetryCause::Media),
    )];
    for (name, evidence) in [
        ("unknown", CompletionEvidence::unknown()),
        ("partial", partial),
        ("reset", check(6, 0x29)),
        ("device", check(3, 0x0c)),
        ("drift", drift),
        ("filemark", filemark),
    ] {
        cases.push((
            name,
            classify(OutcomeInput {
                class: if evidence.opcode == Some(0x10) {
                    CommandClass::WriteFilemarks {
                        count: 0,
                        immediate: false,
                    }
                } else {
                    CommandClass::Write
                },
                context: context.clone(),
                evidence,
            }),
        ));
    }
    // Check every false half before collecting the expected K0 true-half failures.
    for (name, failed) in &cases {
        let ending = live_ending_outcome(failed, false);
        assert_eq!(ending.effect, failed.effect, "false effect {name}");
        assert_eq!(ending.retry, failed.retry, "false retry {name}");
        assert_eq!(ending.reason, failed.reason, "false reason {name}");
        assert_eq!(
            ending.completion, failed.completion,
            "false completion {name}"
        );
        assert_eq!(
            ending.requires_reproof, failed.requires_reproof,
            "false reproof {name}"
        );
    }
    let mut failures = Vec::new();
    for (name, failed) in cases {
        let ending = live_ending_outcome(&failed, true);
        let expected = if name == "drift" {
            Effect::WriteFence(FenceCause::PositionDrift)
        } else {
            Effect::NoFence
        };
        if ending.effect != expected {
            failures.push(format!(
                "true {name}: got {:?}, expected {expected:?}",
                ending.effect
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
