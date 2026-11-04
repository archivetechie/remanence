//! Inject typed admission refusals at the shared boundary, then run each real
//! caller through its renderer and durable destinations. Device normalization
//! itself is covered by the admission carrier and scripted-transport tests.
use super::*;
use crate::append_admission::{b3b_caller_injection, AdmissionError, AdmissionErrorKind};
use remanence_library::outcome::{census, CommandClass, OperationContext, Phase};

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

/// Audit-failure rows prove recovery precedes audit, while fence projection follows audit.
async fn caller_matrix(caller: &str) {
    for (kind, fail_audit) in [
        ("suffix", false),
        ("io", false),
        ("structural", false),
        ("suffix", true),
        ("structural", true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let world = if caller == "terminal" {
            setup(root.path())
        } else {
            b7_tests::due_tape(root.path()).await
        };
        let mut library = open_model_library(world.clone());
        let cfg = config(root.path(), &library);
        let mut index = CatalogIndex::open(&cfg.index_path).unwrap();
        let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
        let mut selection = selected();
        // The terminal caller needs a valid plan, but its admission refuses
        // before any component is written. An empty initialized authority is sufficient.
        if caller == "terminal" {
            selection.parity_config = ParityConfig::None;
            let authority =
                TapeAuthority::open(&cfg.checkpoint_journal_dir, TAPE, index.path()).unwrap();
            authority
                .acquire_exclusive()
                .unwrap()
                .publish_initialized(
                    BLOCK,
                    None,
                    CommitCursor {
                        partition: 0,
                        lba: 2,
                    },
                )
                .unwrap();
        }
        let audit_before = FileAuditLog::replay(&cfg.audit_dir).unwrap().len();
        rusqlite::Connection::open(&cfg.index_path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE b3b_mutations(kind TEXT NOT NULL);
             CREATE TRIGGER b3b_recovery AFTER UPDATE OF state ON tapes
             WHEN NEW.state = 'recovery_required'
             BEGIN INSERT INTO b3b_mutations VALUES ('recovery'); END;
             CREATE TRIGGER b3b_fence AFTER INSERT ON tape_io_fences
             BEGIN INSERT INTO b3b_mutations VALUES ('fence'); END;",
            )
            .unwrap();
        let before_state = index.get_tape(&TAPE).unwrap().unwrap().state;
        assert_eq!(before_state, "ready");
        let observed = Arc::new(Mutex::new(None));
        let capture = observed.clone();
        let audit_path = cfg.audit_dir.clone();
        b3b_caller_injection::set(move || {
            *capture.lock().unwrap() = Some((
                census::total(),
                FileAuditLog::replay(&audit_path).unwrap().len(),
            ));
            if fail_audit {
                // Keep the earlier audits intact and fail only the caller's new append.
                std::fs::rename(&audit_path, audit_path.with_extension("saved")).unwrap();
                std::fs::write(&audit_path, b"injected audit open failure").unwrap();
            }
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
            "{caller}/{kind}: admission was not reached: {status}"
        );
        assert_eq!(
            census::total() - observed.lock().unwrap().unwrap().0,
            1,
            "{caller}/{kind}"
        );
        let fences = index.list_active_tape_io_fences().unwrap();
        assert_eq!(
            fences.len(),
            usize::from(kind == "suffix" && !fail_audit),
            "{caller}/{kind}"
        );
        if let Some(fence) = fences.first() {
            assert_eq!(fence.reason, "SessionSuffixContradiction");
        }
        assert_eq!(
            index.get_tape(&TAPE).unwrap().unwrap().state,
            if kind == "structural" {
                "recovery_required"
            } else {
                "ready"
            },
            "{caller}/{kind}"
        );
        let connection = rusqlite::Connection::open(&cfg.index_path).unwrap();
        let mutations: Vec<String> = connection
            .prepare("SELECT kind FROM b3b_mutations ORDER BY rowid")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            mutations,
            match kind {
                "suffix" if !fail_audit => vec!["fence"],
                "structural" => vec!["recovery"],
                _ => vec![],
            },
            "{caller}/{kind}: each durable mutation occurs exactly once"
        );
        if !fail_audit {
            let audit = FileAuditLog::replay(&cfg.audit_dir).unwrap();
            let consequences: Vec<_> = audit[audit_before..]
                .iter()
                .filter(|r| {
                    matches!(
                        r.event,
                        AuditEvent::RecoveryEvent | AuditEvent::TapeIoFenceRaised
                    )
                })
                .map(|r| r.event.clone())
                .collect();
            assert_eq!(
                consequences,
                match kind {
                    "suffix" => vec![AuditEvent::TapeIoFenceRaised],
                    "structural" => vec![AuditEvent::RecoveryEvent],
                    _ => vec![],
                },
                "{caller}/{kind}"
            );
            assert_eq!(
                status.metadata().get("remanence-retry").unwrap(),
                if kind == "io" {
                    "transient"
                } else {
                    "operator"
                }
            );
        } else {
            assert_eq!(
                FileAuditLog::replay(cfg.audit_dir.with_extension("saved"))
                    .unwrap()
                    .len(),
                observed.lock().unwrap().unwrap().1,
                "failed audit leaves prior history unchanged"
            );
        }
    }
}
