//! Checked rebuild admission and durable fencing at the actual actor opener.
use super::*;

struct CorruptRebuildRead {
    inner: ModelTransport,
    target: u64,
    at_target: bool,
    remaining: usize,
    corruptions: Arc<AtomicU64>,
    world: Arc<Mutex<VirtualWorld>>,
    cursor: u64,
    located: u64,
    cursor_fault: Option<&'static str>,
    fault_armed: bool,
}
impl SgTransport for CorruptRebuildRead {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        if cdb[0] == 0x08 && self.at_target {
            if let Some(fault) = self.cursor_fault.and_then(|f| f.strip_prefix("read_")) {
                use remanence_library::ScsiError;
                let (key, asc, ascq) = match fault {
                    "medium" => (3, 0x11, 0),
                    "unit_attention" => (6, 0x28, 0),
                    "not_ready" => (2, 4, 1),
                    "hardware_44" => (4, 0x44, 0),
                    "hardware_15" => (4, 0x15, 0),
                    "eod" => (8, 0, 5),
                    "filemark" | "wrong_length" => (0, 0, 0),
                    "transport" => {
                        return Err(ScsiError::Io(std::io::Error::other(
                            "injected read transport",
                        )))
                    }
                    "timeout" => {
                        return Err(ScsiError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "injected read timeout",
                        )))
                    }
                    _ => panic!("unknown read fault"),
                };
                let mut sense = readiness_fixed_sense(key, asc, ascq);
                if fault == "filemark" {
                    sense[2] |= 0x80;
                }
                if fault == "wrong_length" {
                    sense[0] |= 0x80;
                    sense[2] |= 0x20;
                    sense[3..7].copy_from_slice(&(-1i32).to_be_bytes());
                }
                return Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                });
            }
        }
        if cdb[0] == 0x08
            && self.fault_armed
            && self.located == self.cursor - 1
            && self.cursor_fault == Some("io")
        {
            return Err(remanence_library::ScsiError::Io(std::io::Error::other(
                "cursor predecessor read failure",
            )));
        }
        let outcome = self.inner.execute_in(cdb, buf)?;
        if cdb[0] == 0x08 && self.at_target && self.cursor_fault.is_some() {
            self.fault_armed = true;
            let mut world = self.world.lock().unwrap();
            let records = &mut world.tapes.get_mut("RESUME001").unwrap().records;
            match self.cursor_fault {
                Some("data") => {
                    records[self.cursor as usize - 1] =
                        Record::Block(vec![0; crate::tape_image_vectors::BLOCK as usize])
                }
                Some("short") => records[self.cursor as usize - 1] = Record::Block(vec![0; 7]),
                Some("missing") => records.truncate(self.cursor as usize - 1),
                _ => {}
            }
        }
        if cdb[0] == 0x34
            && self.fault_armed
            && self.located == self.cursor - 1
            && u64::from_be_bytes(buf[8..16].try_into().unwrap()) == self.cursor
        {
            match self.cursor_fault {
                Some("lba") => buf[8..16].copy_from_slice(&(self.cursor + 1).to_be_bytes()),
                Some("partition") => buf[4..8].copy_from_slice(&1u32.to_be_bytes()),
                _ => {}
            }
        }
        if cdb.first() == Some(&0x08) && self.at_target && self.remaining > 0 {
            assert!(outcome.bytes_transferred > 0);
            buf[0] ^= 1;
            self.remaining -= 1;
            self.corruptions.fetch_add(1, Ordering::Relaxed);
            self.at_target = false;
        }
        Ok(outcome)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        self.inner.execute_none(cdb)?;
        match cdb[0] {
            0x92 => {
                self.located = u64::from_be_bytes(cdb[4..12].try_into().unwrap());
                self.at_target = self.located == self.target;
            }
            0x2b => {
                self.at_target =
                    u32::from_be_bytes(cdb[3..7].try_into().unwrap()) as u64 == self.target
            }
            _ => {}
        }
        Ok(())
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

#[test]
fn checked_closeout_actor_retry_fence_legacy_and_reservation() {
    check_closeout_route(false, None);
}

#[test]
fn checked_closeout_terminal_retry_fence_legacy_and_reservation() {
    check_closeout_route(true, None);
}

/// Exercise real catalog lookup and projection failures after both readback passes disagree.
#[derive(Clone, Copy)]
enum FenceFailure {
    BarcodeLookup,
    FenceWrite,
}

#[test]
fn checked_closeout_barcode_lookup_failure_still_fences_mismatch() {
    check_closeout_route(false, Some(FenceFailure::BarcodeLookup));
}

#[test]
fn checked_closeout_fence_write_failure_preserves_mismatch_and_detail() {
    check_closeout_route(false, Some(FenceFailure::FenceWrite));
}

fn check_closeout_route(terminal: bool, fence_failure: Option<FenceFailure>) {
    check_closeout_route_case(terminal, fence_failure, None, false);
}

/// Inject only after committed data has been read: initial admission is good.
#[test]
fn checked_rebuild_routes_refuse_bad_cursor_before_write() {
    for terminal in [false, true] {
        for fault in ["data", "short", "missing", "io", "lba", "partition"] {
            check_closeout_route_case(terminal, None, Some(fault), false);
        }
    }
}

/// Both rebuilt entry points distinguish unreadable committed bytes from
/// transient drive/transport errors, and neither dispatches a media write.
#[test]
fn checked_rebuild_read_fault_classification() {
    for terminal in [false, true] {
        for fault in [
            "read_medium",
            "read_unit_attention",
            "read_not_ready",
            "read_hardware_44",
            "read_hardware_15",
            "read_eod",
            "read_filemark",
            "read_wrong_length",
            "read_transport",
            "read_timeout",
        ] {
            check_closeout_route_case(terminal, None, Some(fault), false);
        }
    }
}

fn check_closeout_route_case(
    terminal: bool,
    fence_failure: Option<FenceFailure>,
    cursor_fault: Option<&'static str>,
    rebuild: bool,
) {
    let cases = if rebuild {
        &[(2, false, false)][..]
    } else if cursor_fault.is_some() {
        &[(0, false, false)][..]
    } else if fence_failure.is_some() {
        &[(2, false, false)][..]
    } else {
        &[
            (0, false, false),
            (1, false, false),
            (2, false, false),
            (0, true, false),
            (2, false, true),
        ][..]
    };
    for &(corrupt, refuse_memory, legacy) in cases {
        let vector = crate::tape_image_vectors::generate("unfinalized-open").unwrap();
        let input = crate::resume_vectors::recorded_input("resume-open");
        let root = tempfile::tempdir().unwrap();
        let (_drive, world, _) = crate::resume_vectors::model(&vector, &input);
        let library = open_model_library(Arc::clone(&world));
        let selected = SelectedTape {
            standalone_endpoint: None,
            pool_id: "closeout".into(),
            tape_uuid: vector.written.inputs.tape_uuid,
            block_size: crate::tape_image_vectors::BLOCK,
            parity_config: ParityConfig::Scheme(vector.written.inputs.scheme.clone()),
        };
        let mut typed = crate::tests::TypedRoot::open_with_tape_inputs([(
            ProvisionTapeInput {
                tape_uuid: selected.tape_uuid,
                voltag: "RESUME001".into(),
                block_size: selected.block_size,
                parity: selected.parity_config.clone(),
                force: false,
            },
            remanence_state::LabelProvenance::Operator,
            Some(remanence_state::MediaObservation {
                generation: remanence_state::LtoGen::Lto9,
                source: remanence_state::GenerationSource::Operator,
            }),
        )]);
        let mut cfg = test_typed_write_owner_config(
            &typed,
            &library,
            library_snapshot_cell(library.library().clone()),
        );
        let mut index = typed.catalog().write_handle();
        let mut journal =
            crate::resume_vectors::adapt(&input, &vector, &root.path().join("rows")).unwrap();
        let snapshot = journal.committed_snapshot_bounded().unwrap();
        let summary = checked_bounded_resume_summary(&snapshot).unwrap();
        let expected = check_value(
            input["data_shard_crc64"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| u64::from_str_radix(s.as_str().unwrap(), 16).unwrap()),
        );
        let original = remanence_state::authority::AuthorityCommit {
            session_id: Some([9; 16]),
            committed_at_utc: "2026-11-02T00:00:00Z".into(),
            ordinal: 1,
            object_count: summary.committed_object_count,
            protected: summary.highest_protected_ordinal,
            total: summary.total_committed_ordinals,
            cursor: CommitCursor {
                partition: summary.append_position.partition,
                lba: summary.append_position.lba,
            },
            tape_uuid: selected.tape_uuid,
            operation_id: [8; 16],
            next_file: summary.committed_tape_file_count,
            block_size: selected.block_size,
            payloads: Vec::new(),
            scheme: Some(summary.scheme().clone()),
            object_files: Vec::new(),
            barrier: None,
            finalization: None,
            sealed: false,
            seal_due: None,
            check: (!legacy).then_some((expected, CheckProvenance::OriginalWrite)),
        };
        let extent = &summary.open_epoch_object_extents[0];
        let target = extent.physical_start_lba + summary.highest_protected_ordinal
            - extent.first_parity_data_ordinal;
        let corruptions = Arc::new(AtomicU64::new(0));
        let transport = CorruptRebuildRead {
            inner: ModelTransport::new(
                Arc::clone(&world),
                remanence_chaos::model::DeviceRole::Drive { bay: 0x100 },
            ),
            target,
            at_target: false,
            remaining: corrupt,
            corruptions: Arc::clone(&corruptions),
            world: Arc::clone(&world),
            cursor: original.cursor.lba,
            located: 0,
            cursor_fault,
            fault_armed: false,
        };
        let mut drive = DriveHandle::open_standalone_with_transport(
            Path::new("/virtual/closeout-drive"),
            Box::new(transport),
            &remanence_library::StaticAllowlist::new(["/virtual/closeout-drive"]),
        )
        .unwrap();
        let _admission = if refuse_memory {
            cfg.io_memory = crate::io_memory::IoMemoryReservation::new(1).unwrap();
            None
        } else {
            crate::resume_vectors::discard_synthetic_suffix(&world, summary.append_position.lba);
            Some(
                crate::admit_checkpointed_tape(
                    &mut drive,
                    selected.tape_uuid,
                    selected.block_size,
                    summary.append_position.partition,
                    summary.append_position.lba,
                )
                .unwrap(),
            )
        };
        world.lock().unwrap().command_log.clear();
        let before = world.lock().unwrap().tapes["RESUME001"].records.clone();
        let authority = ParityActorAuthority {
            scheme: summary.scheme().clone(),
            snapshot,
            summary,
        };
        if matches!(fence_failure, Some(FenceFailure::BarcodeLookup)) {
            rusqlite::Connection::open(&cfg.index_path)
                .unwrap()
                .execute_batch("ALTER TABLE objects RENAME TO closeout_hidden_objects")
                .unwrap();
            assert!(index.get_tape(&selected.tape_uuid).is_err());
        }
        if matches!(fence_failure, Some(FenceFailure::FenceWrite)) {
            rusqlite::Connection::open(&cfg.index_path)
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER closeout_fence_projection_fault BEFORE INSERT ON tape_io_fences
                     BEGIN SELECT RAISE(FAIL, 'injected closeout fence projection failure'); END;",
                )
                .unwrap();
        }
        let result = if terminal {
            let mut permit = None;
            super::super::terminal_finalize::terminal_epoch_state(
                &mut index,
                &cfg,
                &mut drive,
                &mut journal,
                &authority.snapshot,
                &selected,
                None,
                &original,
                &mut permit,
            )
            .map(|state| ParityActorSession {
                read_attempts: 1,
                scheme: authority.scheme,
                sink_state: state,
                permit: permit.take(),
            })
        } else {
            open_parity_actor_session_with_rows(
                &mut index,
                &mut drive,
                &cfg,
                &selected,
                Some(&original),
                authority,
                &mut journal,
                None,
            )
        };
        let successful = !refuse_memory && corrupt < 2 && !legacy;
        if cursor_fault.is_some() {
            assert!(
                result.is_err(),
                "{cursor_fault:?}, terminal={terminal}: bad C admitted after read-back"
            );
            if !cursor_fault.is_some_and(|f| f.starts_with("read_")) {
                assert!(drive.check_append_admission().is_err());
            }
        } else if successful {
            let session = result.unwrap();
            let position = drive.position().unwrap();
            assert_eq!(
                (position.partition, position.lba),
                (original.cursor.partition, original.cursor.lba),
                "checked read-back must return to C before its caller can write"
            );
            drive
                .check_append_admission()
                .expect("checked read-back must renew the cursor proof");
            let check = session.sink_state.as_ref().unwrap().commit_check();
            assert_eq!(check, (expected, CheckProvenance::OriginalWrite));
            assert!(cfg.io_memory.granted() > 0);
            drop(session);
        } else {
            let status = result.err().expect("rebuild must refuse");
            assert_eq!(
                status.code(),
                if refuse_memory {
                    tonic::Code::ResourceExhausted
                } else {
                    tonic::Code::FailedPrecondition
                }
            );
            if legacy {
                assert!(status
                    .message()
                    .contains("open epoch lacks durable check provenance"));
            } else if !refuse_memory {
                assert!(status.message().contains("CloseoutCheckMismatch"));
            }
            if matches!(fence_failure, Some(FenceFailure::FenceWrite)) {
                assert!(
                    status
                        .message()
                        .contains("additionally failed to persist the required tape fence:"),
                    "{status}"
                );
                assert!(
                    status
                        .message()
                        .contains("injected closeout fence projection failure"),
                    "{status}"
                );
            }
        }
        assert_eq!(cfg.io_memory.granted(), 0);
        assert_eq!(
            corruptions.load(Ordering::Relaxed),
            if legacy { 0 } else { corrupt as u64 }
        );
        let world = world.lock().unwrap();
        assert!(
            world
                .command_log
                .iter()
                .all(|c| !matches!(c.opcode, 0x0a | 0x10)),
            "rebuild dispatched a media write"
        );
        if refuse_memory {
            assert!(
                world.command_log.is_empty(),
                "allocation refusal dispatched a drive command"
            );
        }
        if !matches!(cursor_fault, Some("data" | "short" | "missing")) {
            assert_eq!(world.tapes["RESUME001"].records, before);
        }
        drop(world);
        if matches!(fence_failure, Some(FenceFailure::BarcodeLookup)) {
            rusqlite::Connection::open(&cfg.index_path)
                .unwrap()
                .execute_batch("ALTER TABLE closeout_hidden_objects RENAME TO objects")
                .unwrap();
        }
        let paths = typed.paths().clone();
        let live_fences = if matches!(fence_failure, Some(FenceFailure::FenceWrite)) {
            assert!(index
                .tape_io_admission_conflicts(&selected.tape_uuid, Some("RESUME001"))
                .unwrap()
                .is_empty());
            rusqlite::Connection::open(&cfg.index_path)
                .unwrap()
                .execute_batch("DROP TRIGGER closeout_fence_projection_fault")
                .unwrap();
            None
        } else {
            Some(
                index
                    .tape_io_admission_conflicts(&selected.tape_uuid, Some("RESUME001"))
                    .unwrap(),
            )
        };
        drop(index);
        drop(cfg);
        typed.release();
        // A projection failure leaves a durable record; startup replays it after the fault is removed.
        let recovered = matches!(fence_failure, Some(FenceFailure::FenceWrite)).then(|| {
            remanence_state::library_runtime::LibraryRuntime::open(
                &paths,
                true,
                Arc::new(remanence_state::StateLockGuard::acquire(&paths.state_dir).unwrap()),
            )
            .unwrap()
        });
        let reopened = remanence_state::projector::open(&paths.sqlite_path)
            .map(|catalog| catalog.write_handle())
            .unwrap();
        let fences = reopened
            .tape_io_admission_conflicts(&selected.tape_uuid, Some("RESUME001"))
            .unwrap();
        if let Some(live) = live_fences {
            assert_eq!(
                fences, live,
                "reopened catalog must retain the live fence rows"
            );
        }
        if corrupt == 2 && !legacy {
            assert_eq!(fences.len(), 1);
            assert_eq!(fences[0].reason, "closeout_check_mismatch");
            if matches!(fence_failure, Some(FenceFailure::BarcodeLookup)) {
                assert!(fences[0].barcode.is_none());
            } else {
                assert_eq!(fences[0].barcode.as_deref(), Some("RESUME001"));
            }
            assert!(fences[0]
                .evidence_json
                .as_ref()
                .unwrap()
                .contains("\"attempts\":2"));
        } else if cursor_fault.is_some_and(|f| f.starts_with("read_")) {
            let contradiction = matches!(
                cursor_fault,
                Some("read_medium" | "read_eod" | "read_filemark" | "read_wrong_length")
            );
            assert_eq!(
                fences.len(),
                usize::from(contradiction),
                "{cursor_fault:?}, terminal={terminal}"
            );
            if contradiction {
                assert_eq!(fences[0].reason, "closeout_read_contradiction");
            }
        } else if cursor_fault.is_none() {
            assert!(fences.is_empty());
        }
        if rebuild {
            drop(reopened);
            drop(recovered);
            let rebuilt = rebuild_fence_catalog(&paths);
            let replayed = rebuilt
                .write_handle()
                .tape_io_admission_conflicts(&selected.tape_uuid, Some("RESUME001"))
                .unwrap();
            assert_eq!(
                replayed, fences,
                "library-log rebuild must preserve producer diagnostics"
            );
        }
    }
}

/// A real two-read mismatch keeps the producer's diagnostics through an empty-catalog rebuild.
#[test]
fn typed_closeout_fence_diagnostics_survive_catalog_rebuild() {
    check_closeout_route_case(false, None, None, true);
}

/// Recreate the disposable projection from the durable library log, with no old SQLite state.
fn rebuild_fence_catalog(paths: &remanence_state::StatePaths) -> remanence_state::ProjectionClient {
    for path in [
        paths.sqlite_path.clone(),
        PathBuf::from(format!("{}-wal", paths.sqlite_path.display())),
        PathBuf::from(format!("{}-shm", paths.sqlite_path.display())),
    ] {
        match std::fs::remove_file(path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => panic!("remove catalog: {error}"),
        }
    }
    let runtime = remanence_state::library_runtime::LibraryRuntime::open(
        paths,
        true,
        Arc::new(remanence_state::StateLockGuard::acquire(&paths.state_dir).unwrap()),
    )
    .unwrap();
    remanence_state::projector::open(&paths.sqlite_path)
        .unwrap()
        .with_library_runtime(runtime)
}

/// Oversized diagnostics must not lose the fence when a UTF-8 character straddles the wire bound.
#[test]
fn typed_tape_fence_cuts_diagnostics_at_utf8_boundary() {
    let tape_uuid = [0xc1; 16];
    let typed = crate::tests::TypedRoot::open_with_tapes(&[(tape_uuid, "DIAG01L9")]);
    let mut index = typed.catalog().write_handle();
    let cfg = TerminalWriteConfig {
        authority: TerminalAuthority::Typed(typed.catalog().library_runtime().unwrap()),
        io_memory: test_io_memory(),
        mounted_endpoint: None,
        checkpoint_journal_dir: typed.paths().journal_dir.join("tapes"),
    };
    let evidence = format!("{{\"error\":\"{}é trailing\"}}", "x".repeat(4085));
    assert_eq!(&evidence.as_bytes()[4095..4097], "é".as_bytes());
    let barcode = format!("{}é trailing", "b".repeat(4095));
    let fence = raise_tape_io_fence(
        &mut index,
        &cfg,
        remanence_state::TapeIoFenceInput {
            tape_uuid,
            barcode: Some(barcode),
            reason: "closeout_check_mismatch".into(),
            evidence_json: Some(evidence.clone()),
        },
        None,
        None,
    )
    .expect("oversized diagnostics must still persist the fence");
    assert_eq!(fence.evidence_json.as_deref(), Some(&evidence[..4095]));
    assert_eq!(fence.barcode.as_deref(), Some("b".repeat(4095).as_str()));
}

/// An ASCII value at the wire bound must survive in full, rather than lose its last byte.
#[test]
fn typed_tape_fence_keeps_4096_byte_ascii_evidence() {
    let evidence = "x".repeat(4096);
    check_typed_tape_fence_diagnostics("DIAG01L9", &evidence, Some("DIAG01L9"), Some(&evidence));
}

/// One ASCII byte beyond the wire bound must be cut without losing the fence.
#[test]
fn typed_tape_fence_cuts_4097_byte_ascii_evidence() {
    let evidence = "x".repeat(4097);
    check_typed_tape_fence_diagnostics(
        "DIAG01L9",
        &evidence,
        Some("DIAG01L9"),
        Some(&evidence[..4096]),
    );
}

/// Empty producer evidence is absent from the record and NULL in the projected row.
#[test]
fn typed_tape_fence_omits_empty_evidence() {
    check_typed_tape_fence_diagnostics("DIAG01L9", "", Some("DIAG01L9"), None);
}

/// A label containing only spaces contributes no barcode entry or projected barcode.
#[test]
fn typed_tape_fence_omits_blank_barcode() {
    let evidence = r#"{"attempts":2}"#;
    check_typed_tape_fence_diagnostics("  ", evidence, None, Some(evidence));
}

/// Check both the durable detail entries and their projection through the typed write funnel.
fn check_typed_tape_fence_diagnostics(
    barcode: &str,
    evidence: &str,
    expected_barcode: Option<&str>,
    expected_evidence: Option<&str>,
) {
    use remanence_state::library_log as lib;

    let tape_uuid = [0xc3; 16];
    let typed = crate::tests::TypedRoot::open_with_tapes(&[(tape_uuid, "DIAG01L9")]);
    let mut index = typed.catalog().write_handle();
    let cfg = TerminalWriteConfig {
        authority: TerminalAuthority::Typed(typed.catalog().library_runtime().unwrap()),
        io_memory: test_io_memory(),
        mounted_endpoint: None,
        checkpoint_journal_dir: typed.paths().journal_dir.join("tapes"),
    };
    let fence = raise_tape_io_fence(
        &mut index,
        &cfg,
        remanence_state::TapeIoFenceInput {
            tape_uuid,
            barcode: Some(barcode.into()),
            reason: "closeout_check_mismatch".into(),
            evidence_json: Some(evidence.into()),
        },
        None,
        None,
    )
    .expect("boundary diagnostics must still persist the fence");
    assert_eq!(fence.evidence_json.as_deref(), expected_evidence);
    assert_eq!(fence.barcode.as_deref(), expected_barcode);
    let source = cfg
        .authority
        .runtime()
        .unwrap()
        .fence_for_quarantine_id(&fence.quarantine_id)
        .expect("the projected fence has a durable typed source");
    let expected = [
        ("barcode", expected_barcode),
        ("evidence_json", expected_evidence),
    ]
    .into_iter()
    .filter_map(|(key, value)| {
        value
            .map(|value| lib::DiagnosticEntry(key.into(), lib::DiagnosticValue::Text(value.into())))
    })
    .collect();
    assert_eq!(source.detail, lib::Bounded(expected));
}

/// The recovery-required admission branch bypasses the funnel but must keep its own error text.
#[test]
fn typed_admission_recovery_fence_records_error() {
    use crate::append_admission::{
        admission_error_outcome, daemon_admission_error, AdmissionError,
    };
    let tape_uuid = [0xc2; 16];
    for fail_lookup in [false, true] {
        let mut typed = crate::tests::TypedRoot::open_with_tapes(&[(tape_uuid, "DIAG01L9")]);
        let mut index = typed.catalog().write_handle();
        let cfg = TerminalWriteConfig {
            authority: TerminalAuthority::Typed(typed.catalog().library_runtime().unwrap()),
            io_memory: test_io_memory(),
            mounted_endpoint: None,
            checkpoint_journal_dir: typed.paths().journal_dir.join("tapes"),
        };
        if fail_lookup {
            rusqlite::Connection::open(&typed.paths().sqlite_path)
                .unwrap()
                .execute_batch("ALTER TABLE objects RENAME TO admission_hidden_objects")
                .unwrap();
            assert!(index.get_tape(&tape_uuid).is_err());
        }
        let error = AdmissionError::from("injected recovery refusal \"detail\"".to_owned());
        let expected = serde_json::json!({"error": error.to_string()}).to_string();
        let outcome = admission_error_outcome(&error);
        assert!(outcome.recovery_required);
        assert!(!matches!(
            outcome.effect,
            remanence_library::outcome::Effect::WriteFence(_)
        ));
        let status = daemon_admission_error(
            AuditActor::System,
            &mut index,
            &cfg,
            tape_uuid,
            error,
            &outcome,
        );
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        let fences = index.tape_io_admission_conflicts(&tape_uuid, None).unwrap();
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].reason, "append_admission_refused");
        assert_eq!(fences[0].evidence_json.as_deref(), Some(expected.as_str()));
        assert_eq!(
            fences[0].barcode.as_deref(),
            (!fail_lookup).then_some("DIAG01L9")
        );
        if fail_lookup {
            rusqlite::Connection::open(&typed.paths().sqlite_path)
                .unwrap()
                .execute_batch("ALTER TABLE admission_hidden_objects RENAME TO objects")
                .unwrap();
        }
        drop(index);
        drop(cfg);
        typed.release();
        let rebuilt = rebuild_fence_catalog(typed.paths());
        let replayed = rebuilt
            .write_handle()
            .tape_io_admission_conflicts(&tape_uuid, None)
            .unwrap();
        assert_eq!(
            replayed, fences,
            "admission error and producer barcode survive log replay"
        );
    }
}
