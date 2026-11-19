//! Inject typed admission refusals at the shared boundary, then run each real
//! caller through its renderer and durable destinations. Device normalization
//! itself is covered by the admission carrier and scripted-transport tests.
use super::*;
use crate::append_admission::{b3b_caller_injection, AdmissionError, AdmissionErrorKind};
use remanence_library::outcome::{census, CommandClass, OperationContext, Phase};
use remanence_state::library_log as lib;

/// The object-session caller preserves the three admission consequences and classifies each once.
#[tokio::test]
async fn b3b_write_session_admission_caller_regression() {
    caller_matrix("write").await;
}

/// Maintenance closeout preserves the three admission consequences and classifies each once.
#[tokio::test]
async fn b3b_closeout_admission_caller_regression() {
    caller_matrix("closeout").await;
}

/// Terminal admission preserves the three admission consequences and classifies each once.
#[tokio::test]
async fn b3b_terminal_admission_caller_regression() {
    caller_matrix("terminal").await;
}

/// Each caller keeps ruling 13's typed fence and transient refusal consequences.
/// A projection failure leaves the library fence durable, with arm-specific status handling.
async fn caller_matrix(caller: &str) {
    for (kind, fail_projection) in [
        ("suffix", false),
        ("io", false),
        ("structural", false),
        ("suffix", true),
        ("structural", true),
    ] {
        let root = DurabilityRoot::new(BARCODE, caller != "terminal");
        let mut index = root.state.write_handle();
        let world = model_world(BARCODE, caller != "terminal");
        if caller != "terminal" {
            typed_due_tape(&root, world.clone()).await;
        }
        let mut library = open_model_library(world.clone());
        let cfg = root.config(&library);
        let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
        let mut selection = selected();
        // Terminal admission refuses before writing any component; initialize its plan authority.
        if caller == "terminal" {
            selection.parity_config = ParityConfig::None;
            let authority =
                TapeAuthority::open(&cfg.checkpoint_journal_dir, TAPE, index.path()).unwrap();
            let mut lease = authority.acquire_exclusive().unwrap();
            lease
                .publish_initialized(
                    BLOCK,
                    None,
                    CommitCursor {
                        partition: 0,
                        lba: 2,
                    },
                )
                .unwrap();
            cfg.authority
                .runtime()
                .unwrap()
                .refresh_native(&lease.freeze().unwrap())
                .unwrap();
        }
        let connection = rusqlite::Connection::open(&cfg.index_path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE b3b_mutations(kind TEXT NOT NULL);
                 CREATE TRIGGER b3b_fence AFTER INSERT ON tape_io_fences
                 BEGIN INSERT INTO b3b_mutations VALUES ('fence'); END;",
            )
            .unwrap();
        if fail_projection {
            connection
                .execute_batch(
                    "CREATE TRIGGER b3b_projection_fault BEFORE INSERT ON tape_io_fences
                     BEGIN SELECT RAISE(FAIL, 'injected b3b fence projection failure'); END;",
                )
                .unwrap();
        }
        let runtime = cfg.authority.runtime().expect("R5: shared runtime");
        assert!(
            runtime.tape_state(TAPE).is_some(),
            "R5: Reprovision identity"
        );
        assert!(
            runtime.current_native_reference(TAPE).is_ok(),
            "R5: native reference"
        );
        assert!(index
            .tape_io_admission_conflicts(&TAPE, Some(BARCODE))
            .unwrap()
            .is_empty());
        assert_eq!(index.get_tape(&TAPE).unwrap().unwrap().state, "ready");
        println!("R5 {caller}/{kind}/{fail_projection} ready");
        let observed = Arc::new(Mutex::new(None));
        let capture = observed.clone();
        b3b_caller_injection::set(move || {
            *capture.lock().unwrap() = Some(census::total());
            match kind {
                "suffix" => AdmissionError::new(AdmissionErrorKind::SessionSuffixContradiction {
                    tape_uuid: Uuid::from_bytes(TAPE),
                    partition: 0,
                    lba: 2,
                    sequence: 7,
                    incarnation: 1,
                    detail: "injected suffix mismatch".into(),
                }),
                "io" => AdmissionError::new(AdmissionErrorKind::SuffixIo {
                    start_lba: 2,
                    source: remanence_library::TapeIoError::new_with_context(
                        remanence_library::TapeIoErrorKind::Transport(
                            remanence_library::ScsiError::Io(std::io::Error::other(
                                "injected admission transport",
                            )),
                        ),
                        CommandClass::Read,
                        OperationContext {
                            phase: Phase::Admission,
                            ..Default::default()
                        },
                    ),
                }),
                "structural" => AdmissionError::new(AdmissionErrorKind::Unproved(
                    "injected structural refusal".into(),
                )),
                _ => unreachable!(),
            }
        });
        let status = match caller {
            "write" => {
                let (tx, mut rx) = mpsc::channel(1);
                let (reply, mut received) = oneshot::channel();
                handle_drive_open_write(
                    BAY,
                    &mut index,
                    &cfg,
                    tx,
                    &mut rx,
                    &mut drive,
                    &mut 0,
                    None,
                    super::super::super::actor_runtime::OpenWriteActorRequest {
                        actor: AuditActor::System,
                        pool_cfg: pool(),
                        selected: selection,
                        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                        needs_drive_load: false,
                        library_serial: Some(LIBRARY.into()),
                        barcode: Some(BARCODE.into()),
                        source_slot: None,
                        drive_uuid: None,
                        drive_serial: Some(DRIVE.into()),
                        reply,
                    },
                );
                received.try_recv().unwrap().unwrap_err()
            }
            "closeout" => {
                let permit = cfg
                    .io_memory
                    .reserve_closeout(&scheme(), BLOCK, 1, TAPE)
                    .unwrap();
                let progress =
                    crate::closeout_progress::CloseoutProgress::new(Uuid::new_v4(), TAPE, None)
                        .unwrap();
                super::super::super::closeout::handle(
                    &mut index,
                    &cfg,
                    &mut drive,
                    super::super::super::closeout::CloseoutWork {
                        tape_uuid: TAPE,
                        expected_pool: Some(POOL),
                        progress: &progress,
                        permit,
                    },
                    |_, _| Ok(()),
                )
                .unwrap_err()
            }
            "terminal" => {
                use super::super::super::terminal_finalize::{
                    admit_terminal_write, build_new_terminal_plan, TerminalPlanPosition,
                };
                use super::super::super::terminal_types::TerminalFinalizeSpec;
                let authority =
                    TapeAuthority::open(&cfg.checkpoint_journal_dir, TAPE, index.path()).unwrap();
                let lease = authority.acquire_exclusive().unwrap();
                let previous = lease.latest_commit().unwrap().unwrap();
                let mut source = TerminalIndexSource::new_replay_backed_no_parity(&lease).unwrap();
                let spec = TerminalFinalizeSpec::automatic(
                    &selection,
                    &pool(),
                    TerminalFinalizationTrigger::PoolCloseOut,
                );
                let (_, plan) = build_new_terminal_plan(
                    &index,
                    &selection,
                    &spec,
                    &previous,
                    &mut source,
                    TerminalPlanPosition {
                        first_tape_file_number: previous.next_file,
                        first_start_lba: previous.cursor.lba,
                        terminal_prefix: None,
                    },
                )
                .unwrap();
                admit_terminal_write(
                    &AuditActor::System,
                    &mut index,
                    &cfg,
                    &mut drive,
                    &previous,
                    &selection,
                    &plan,
                    None,
                )
                .unwrap_err()
            }
            _ => unreachable!(),
        };
        assert!(
            !b3b_caller_injection::pending(),
            "{caller}/{kind}/{fail_projection}: admission was not reached: {status}"
        );
        assert_eq!(
            census::total() - observed.lock().unwrap().unwrap(),
            1,
            "{caller}/{kind}/{fail_projection}"
        );
        let fencing = kind != "io";
        let expected_sql_fence = fencing && !fail_projection;
        let fences = index.list_active_tape_io_fences().unwrap();
        assert_eq!(
            fences.len(),
            usize::from(expected_sql_fence),
            "{caller}/{kind}/{fail_projection}: active SQL fences"
        );
        if let Some(fence) = fences.first() {
            assert_eq!(fence.tape_uuid, TAPE);
            assert_eq!(
                fence.reason,
                if kind == "structural" {
                    "append_admission_refused"
                } else {
                    "session_suffix_contradiction"
                }
            );
        }
        assert_eq!(
            index.tape_io_is_fenced(Some(&TAPE), None, None).unwrap(),
            expected_sql_fence,
            "{caller}/{kind}/{fail_projection}: tape exclusion"
        );
        assert_eq!(index.get_tape(&TAPE).unwrap().unwrap().state, "ready");
        let mutations: Vec<String> = connection
            .prepare("SELECT kind FROM b3b_mutations ORDER BY rowid")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            mutations,
            if expected_sql_fence {
                vec!["fence"]
            } else {
                vec![]
            },
            "{caller}/{kind}/{fail_projection}: each durable mutation occurs exactly once"
        );
        assert_eq!(
            status.code(),
            if kind == "io" {
                tonic::Code::Unavailable
            } else {
                tonic::Code::FailedPrecondition
            }
        );
        if fail_projection {
            assert_eq!(
                status
                    .message()
                    .contains("injected b3b fence projection failure"),
                kind == "structural",
                "{caller}/{kind}: projection status handling"
            );
        }
        if kind == "suffix" {
            assert!(status
                .message()
                .contains("SessionSuffixContradiction: tape "));
            assert!(status
                .message()
                .contains("does not match tape-log sequence 7"));
            assert!(!status
                .message()
                .contains("injected b3b fence projection failure"));
        }
        if !(kind == "structural" && fail_projection) {
            assert_eq!(
                status.metadata().get("remanence-retry").unwrap(),
                if kind == "io" {
                    "transient"
                } else {
                    "operator"
                }
            );
        }
        let retry = status
            .metadata()
            .get("remanence-retry")
            .map(|v| v.to_str().unwrap().to_owned());
        let library_uuid = cfg.authority.runtime().unwrap().manifest().library_uuid();
        drop((index, cfg, drive, library, connection));
        drop(root.state);
        let mut typed = root.typed;
        let raised = typed
            .records(|record| (record.tag() == lib::Tag::FenceRaised).then_some(()))
            .await;
        assert_eq!(
            raised.len(),
            usize::from(fencing),
            "{caller}/{kind}/{fail_projection}: durable FenceRaised count"
        );
        // Inspect the durable fold's typed sources; Record exposes only the tag for fences.
        let view = lib::native_view::NativeTapeView::probe(
            &typed.paths().journal_dir.join("tapes"),
            &typed.paths().sqlite_path,
        )
        .unwrap();
        let replay = lib::store::replay(&typed.paths().audit_dir, library_uuid, &view).unwrap();
        let sources: Vec<_> = replay.fold.active_tape_fences(&TAPE).collect();
        assert_eq!(sources.len(), raised.len());
        for (_, fence) in sources {
            assert_eq!(fence.kind, lib::FenceKind::Write);
            assert!(matches!(fence.scope, lib::FenceScope::Tape(tape) if tape.uuid == TAPE));
            assert_eq!(
                fence.cause,
                if kind == "structural" {
                    lib::FenceCause::AppendAdmissionRefused
                } else {
                    lib::FenceCause::SessionSuffixContradiction
                }
            );
        }
        println!("ROW {caller}/{kind}/{fail_projection} code={:?} retry={retry:?} fences={} mutations={mutations:?} FenceRaised={} status={:?}", status.code(), fences.len(), raised.len(), status.message());
    }
}
