//! Native lease, recovery, terminal, and lossless-compatibility contracts.
use super::*;
use crate::checkpoint::tests::{finalization_intent, record};
use remanence_parity::TapeIndexReplicaRecordSource;

#[test]
fn native_completion_freezes_all_five_components_and_retains_old_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x71; 16];
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    writer.publish_report(&(&record(id)).into()).unwrap();
    let before = writer.freeze().unwrap();
    let plan: AuthorityFinalization = (&finalization_intent(id)).into();
    writer.begin_finalization(&plan).unwrap();
    let mut p = Progress::BeforeReplicaA;
    while let Some(next) = p.successor() {
        writer.advance_finalization(p, next).unwrap();
        p = next;
    }
    let mut seam = false;
    writer
        .seal(|| {
            seam = true;
            Ok(())
        })
        .unwrap();
    assert!(seam);
    let sealed = writer.freeze().unwrap();
    assert!(sealed.summary().sealed);
    assert_eq!(sealed.summary().next_file, 7);
    assert!(writer.pending_finalization().unwrap().is_none());
    assert!(writer
        .recover_finalization()
        .unwrap()
        .finalization
        .is_none());
    assert_eq!(before.summary().next_file, 2);
    let mut rows = 0;
    sealed
        .visit_file_rows(&mut |_| {
            rows += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(rows, 7);
    let mut source = writer
        .terminal_source(None, TerminalPrefixMode::Absent)
        .unwrap();
    assert_eq!(source.summary().counts.structural_entry_count, 2);
    let mut rows = 0;
    source
        .visit_structural_entries(&mut |_| {
            rows += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(rows, 2);
    let mut objects = 0;
    source
        .visit_object_rows(&mut |_| {
            objects += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(objects, 1);
    assert!(
        source.reconstruct_final_plan(&plan).is_err(),
        "fixture digest is deliberately not the edition digest"
    );
    drop(writer);
    assert!(authority.freeze().unwrap().summary().sealed);
}

#[test]
fn weak_structural_capability_expires_and_loans_survive_unwind() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x72; 16];
    let (record, _, _) = crate::authority::contract_tests::structural_fixture(id);
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    writer.publish_report(&(&record).into()).unwrap();
    let mut facet = writer
        .open_structural(record.block_size, record.scheme.clone().unwrap())
        .unwrap();
    let owned = writer
        .open_structural(record.block_size, record.scheme.clone().unwrap())
        .unwrap();
    writer.retain_structural_publisher(owned).unwrap();
    let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = writer.with_structural::<()>(|_, _| panic!("operation panic"));
    }));
    assert!(failed.is_err());
    assert!(writer.structural().is_some());
    assert_eq!(facet.pending_count(), 0);
    facet.discard_pending(&[]).unwrap();
    assert!(facet.discard_suffix(0).is_err());
    writer
        .validate_append_rows(&facet.freeze().unwrap())
        .unwrap();
    let raw = writer.freeze().unwrap().into_file_rows().unwrap();
    drop(writer);
    assert!(facet.freeze().is_err());
    assert!(facet.discard_suffix(3).is_err());
    let mut cursor = raw.replay().unwrap();
    let mut rows = 0;
    while cursor.next_entry().unwrap().is_some() {
        rows += 1;
    }
    assert_eq!(rows, 3);
    // The factory was opened before the first parity header was published.
    assert_eq!(authority.freeze().unwrap().summary().next_file, 3);
}

#[test]
fn old_format_refusals_are_typed_and_leave_authority_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x73; 16];
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    let commit: AuthorityCommit = (&record(id)).into();
    writer.publish_report(&commit).unwrap();
    let before = writer.authority_summary().unwrap();
    let p = FilePublication {
        purpose: FilePurpose::Object,
        files: Vec::new(),
        protected: 0,
        total: 3,
        cursor: None,
        check: None,
        seal_due: None,
    };
    assert!(matches!(
        writer.publish_commit(&p),
        Err(JournalError::UnsupportedOperation(_))
    ));
    assert!(
        matches!(writer.publish_reports(&[commit.clone(),commit.clone()]),Err(StateError::TapeLog(error)) if matches!(*error,StoreError::Volume(JournalError::UnsupportedOperation(_))))
    );
    let mut called = false;
    assert!(writer
        .publish_sealed_reports(&[commit], || {
            called = true;
            Ok(())
        })
        .is_err());
    assert!(!called);
    assert_eq!(writer.authority_summary().unwrap(), before);
}

#[test]
fn automatic_finalization_carries_seal_reason_and_reports_after_durability_failure() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x74; 16];
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    let mut report: AuthorityCommit = (&record(id)).into();
    report.seal_due = Some(TerminalFinalizationTrigger::HardwareEarlyWarning);
    writer.publish_report(&report).unwrap();
    let mut plan: AuthorityFinalization = (&finalization_intent(id)).into();
    plan.trigger = TerminalFinalizationTrigger::HardwareEarlyWarning;
    plan.manual = None;
    writer.begin_finalization(&plan).unwrap();
    let mut p = Progress::BeforeReplicaA;
    while let Some(next) = p.successor() {
        writer.advance_finalization(p, next).unwrap();
        p = next;
    }
    let error = writer
        .seal(|| Err(invalid("after durability")))
        .unwrap_err();
    assert!(
        matches!(error,StateError::TapeLog(error) if matches!(*error,StoreError::AfterDurable(_)))
    );
    let summary = writer.authority_summary().unwrap();
    assert!(summary.sealed);
    assert_eq!(summary.seal_due, report.seal_due);
    assert!(
        writer.latest_commit().unwrap().unwrap().sealed,
        "the latest summary must reflect the durable seal"
    );
}

#[test]
fn derived_components_match_parity_codec_and_reconstruct_exact_edition() {
    use remanence_parity::{
        assemble_terminal_plan, checked_tape_index_replica_layout, terminal_component_publication,
        ParityMapDiagnostics, TerminalTailLayout, DEFAULT_INDEX_SEPARATION_BYTES,
    };
    let dir = tempfile::tempdir().unwrap();
    let id = [0x75; 16];
    let (record, intent, _) = crate::authority::contract_tests::structural_fixture(id);
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    writer.publish_report(&(&record).into()).unwrap();
    let mut intent: AuthorityFinalization = (&intent).into();
    let prefix = intent.terminal_prefix.as_ref().unwrap().clone();
    let mut source = writer
        .terminal_source(None, TerminalPrefixMode::Planned(&prefix))
        .unwrap();
    let scope = source.summary();
    let records = checked_tape_index_replica_layout(record.block_size, scope.counts)
        .unwrap()
        .replica_record_count;
    let layout = TerminalTailLayout::new(
        0,
        record.block_size,
        prefix.tail_start_tape_file_number,
        prefix.tail_start_lba,
        records,
        4096,
    )
    .unwrap();
    let triple = assemble_terminal_plan(
        id,
        record.block_size,
        false,
        intent.edition_sequence,
        scope.scope,
        scope.counts,
        &mut source,
        layout,
        ParityMapDiagnostics {
            writer_version: intent.writer_version.clone(),
            write_timestamp: intent.write_timestamp.clone(),
        },
        intent.edition_id,
        DEFAULT_INDEX_SEPARATION_BYTES,
    )
    .unwrap();
    intent.layout = layout.try_into().unwrap();
    intent.edition_digest = triple.edition.edition_digest;
    writer.begin_finalization(&intent).unwrap();
    writer.publish_terminal_prefix(&prefix.publication).unwrap();
    let mut durable = writer
        .terminal_source(None, TerminalPrefixMode::Durable(&prefix))
        .unwrap();
    assert_eq!(durable.reconstruct_final_plan(&intent).unwrap(), triple);
    let facet = writer
        .open_structural(record.block_size, record.scheme.unwrap())
        .unwrap();
    let expected = layout
        .components
        .iter()
        .map(|c| terminal_component_publication(&triple, *c).unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        facet.component_relation(0, &expected).unwrap(),
        remanence_parity::TerminalComponentAuthorityRelation::Aligned
    ));
    let mut progress = Progress::BeforeReplicaA;
    for publication in &expected {
        let next = progress.successor().unwrap();
        assert_eq!(
            writer
                .publish_terminal_component(publication)
                .unwrap()
                .items[0]
                .disposition,
            PublicationDisposition::Appended
        );
        assert_eq!(
            writer
                .advance_finalization(progress, next)
                .unwrap()
                .progress,
            next
        );
        assert_eq!(
            writer
                .publish_terminal_component(publication)
                .unwrap()
                .items[0]
                .disposition,
            PublicationDisposition::AlreadyPresent
        );
        progress = next;
    }
    assert!(matches!(
        facet.component_relation(5, &expected).unwrap(),
        remanence_parity::TerminalComponentAuthorityRelation::Aligned
    ));
    writer.seal(|| Ok(())).unwrap();
    let snapshot = writer.freeze().unwrap();
    let mut rows = Vec::new();
    snapshot
        .visit_file_rows(&mut |row| {
            if row.tape_file_number >= 4 {
                rows.push(row.clone());
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        rows,
        expected
            .into_iter()
            .flat_map(|p| p.files)
            .collect::<Vec<_>>()
    );
}

#[test]
fn leased_capture_reuses_validated_fold_and_external_capture_validates_again() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x76; 16];
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    writer.publish_report(&(&record(id)).into()).unwrap();
    let frozen = writer.freeze().unwrap();
    assert_eq!(frozen.raw.replay_metrics().validation_passes, 0);
    // Reusing trust does not mean that a compatibility replay emits zero frames.
    assert_eq!(
        writer.visit_commits(&mut |_| Ok(())).unwrap().frame_count,
        5
    );
    drop(writer);
    let external = authority.freeze().unwrap();
    assert_eq!(external.raw.replay_metrics().validation_passes, 1);
    assert_eq!(external.summary(), frozen.summary());
}

#[test]
fn filename_identity_is_canonical_and_poison_does_not_panic_uuid_access() {
    for invalid in [
        "01234567-89ab-cdef-0123-456789abcdef.remtape",
        "0123456789ABCDEF0123456789ABCDEF.remtape",
        "0123456789abcdef0123456789abcdef.other",
    ] {
        assert!(TapeAuthority::entry_tape_uuid(Path::new(invalid)).is_err());
    }
    assert!(
        TapeAuthority::entry_tape_uuid(Path::new("0123456789abcdef0123456789abcdef.remtape"))
            .is_ok()
    );
    let dir = tempfile::tempdir().unwrap();
    let id = [0x76; 16];
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let writer = authority.acquire_exclusive().unwrap();
    let core = writer.core.clone();
    let _ = std::thread::spawn(move || {
        let _guard = core.lock().unwrap();
        panic!("poison owner");
    })
    .join();
    assert_eq!(writer.tape_uuid(), id);
    assert!(writer.authority_summary().is_err());
}

#[test]
fn streaming_report_producer_and_visitor_errors_preserve_the_old_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x77; 16];
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    let report: AuthorityCommit = (&record(id)).into();
    let error = writer
        .publish_report_streaming(&report, &mut |_, _| Err(invalid("producer failed")))
        .unwrap_err();
    assert!(error.to_string().contains("producer failed"));
    assert_eq!(
        writer.authority_summary().unwrap().cursor,
        Some(CommitCursor {
            partition: 0,
            lba: 2
        })
    );
    writer
        .publish_report_streaming(&report, &mut |index, _| {
            assert_eq!(index, 0);
            Ok(())
        })
        .unwrap();
    let summary = writer.authority_summary().unwrap();
    let error = writer
        .visit_reports(&mut |view| {
            view.visit_payloads(&mut |_| Err(invalid("catalog visitor failed")))
        })
        .unwrap_err();
    assert!(error.to_string().contains("catalog visitor failed"));
    assert_eq!(writer.authority_summary().unwrap(), summary);
}

#[test]
fn streaming_report_preserves_original_write_check() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x78; 16];
    let (seed, _, _) = crate::authority::contract_tests::structural_fixture(id);
    let mut report: AuthorityCommit = (&seed).into();
    report.object_files[0].files.truncate(1);
    report.barrier = None;
    report.object_files[0].protected = 0;
    report.payloads[0].projection.copy.protected_until_ordinal = Some(0);
    report.protected = 0;
    report.next_file = 2;
    report.cursor.lba = 6;
    report.check = Some((CheckValue(7), CheckProvenance::OriginalWrite));
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    writer
        .publish_report_streaming(&report, &mut |_, _| Ok(()))
        .unwrap();
    let mut count = 0;
    writer
        .visit_reports(&mut |view| {
            assert_eq!(view.summary().check, report.check);
            count += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(writer.latest_commit().unwrap().unwrap().check, report.check);
    let mut observed = 0;
    writer
        .visit_commits(&mut |commit| {
            assert_eq!(commit.check, report.check);
            observed += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(observed, 1);
}

/// The writer holds the core across a slow media call; readers use its frozen
/// pre-barrier snapshot and must finish before the media callback can return.
#[test]
fn streaming_media_barrier_does_not_block_structural_readers() {
    let root = tempfile::tempdir().unwrap();
    let authority = TapeAuthority::open_fixture(root.path(), [0x81; 16]).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    let rows = writer
        .open_structural(262144, remanence_parity::default_scheme())
        .unwrap();
    let (start, started) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        started.recv().unwrap();
        let snapshot = rows.freeze().unwrap();
        done.send(snapshot.committed_entry_count()).unwrap();
    });
    writer
        .with_store(|_| {
            start.send(()).unwrap();
            assert_eq!(
                finished
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap(),
                0
            );
            Ok(())
        })
        .unwrap();
    reader.join().unwrap();
}

#[test]
fn staged_and_ambiguous_leases_explicitly_refuse_abandonment_and_stale_latest() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x82; 16];
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    let report: AuthorityCommit = (&record(id)).into();
    if writer.authority_summary().unwrap().cursor.is_none() {
        writer
            .publish_initialized(
                report.block_size,
                report.scheme.clone(),
                CommitCursor {
                    partition: 0,
                    lba: 2,
                },
            )
            .unwrap();
    }
    writer
        .open_session([9; 16], "2026-11-02T00:00:00Z".into())
        .unwrap();
    writer.stage_report(&report).unwrap();
    assert!(writer.ensure_abandonment_allowed().is_err());
    assert!(writer.latest_commit().is_err());
    writer.cancel_staged_report().unwrap();
    writer.ensure_abandonment_allowed().unwrap();
    assert_eq!(writer.latest_commit().unwrap().unwrap().ordinal, 0);
    if writer.authority_summary().unwrap().cursor.is_none() {
        writer
            .publish_initialized(
                report.block_size,
                report.scheme.clone(),
                CommitCursor {
                    partition: 0,
                    lba: 2,
                },
            )
            .unwrap();
    }
    writer.bind_session([9; 16]);
    writer.stage_report(&report).unwrap();
    writer
        .set_publication_observer(crate::tape_log::store::PublicationObserver(Arc::new(
            |_, _, _| Err(io::Error::other("ambiguous fsync")),
        )))
        .unwrap();
    let error = writer.complete_report(&report).unwrap_err();
    assert!(error.is_checkpoint_append_authority_uncertain());
    assert!(!error.is_checkpoint_append_durable());
    writer.cancel_staged_report().unwrap(); // no still-provisional group to cancel
    assert!(writer.ensure_abandonment_allowed().is_err());
    assert!(writer.latest_commit().is_err());
    assert!(writer.read_cache.lock().unwrap().is_none());
}
#[test]
fn after_durable_report_clears_cached_snapshot_and_has_distinct_error_type() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x83; 16];
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    let report: AuthorityCommit = (&record(id)).into();
    if writer.authority_summary().unwrap().cursor.is_none() {
        writer
            .publish_initialized(
                report.block_size,
                report.scheme.clone(),
                CommitCursor {
                    partition: 0,
                    lba: 2,
                },
            )
            .unwrap();
    }
    writer
        .open_session([9; 16], "2026-11-02T00:00:00Z".into())
        .unwrap();
    writer.stage_report(&report).unwrap();
    writer
        .set_publication_observer(crate::tape_log::store::PublicationObserver(Arc::new(
            |_, _, after| {
                if after {
                    Err(io::Error::other("after durable"))
                } else {
                    Ok(())
                }
            },
        )))
        .unwrap();
    let error = writer.complete_report(&report).unwrap_err();
    assert!(error.is_checkpoint_append_durable());
    assert!(!error.is_checkpoint_append_authority_uncertain());
    assert!(writer.read_cache.lock().unwrap().is_none());
    assert!(writer.latest_commit().is_err());
    drop(writer);
    assert!(authority.freeze().unwrap().log_reference().0 > 0);
}
#[test]
fn every_writer_acquisition_binds_the_current_projection_floor() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x84; 16];
    let mut index = crate::CatalogIndex::open(dir.path().join("index.sqlite")).unwrap();
    let authority = TapeAuthority::open(dir.path(), id, index.path()).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    writer.publish_report(&(&record(id)).into()).unwrap();
    let first = writer.freeze().unwrap();
    writer
        .publish_report(&(&crate::checkpoint::tests::second_record(id)).into())
        .unwrap();
    index.project_tape_log(&writer.freeze().unwrap()).unwrap();
    drop(writer);
    let path = authority.path();
    let bytes = std::fs::read(path).unwrap();
    std::fs::write(path, &bytes[..first.raw.summary().closed_end as usize]).unwrap();
    let before = std::fs::read(path).unwrap();
    assert!(authority.acquire_exclusive().is_err());
    assert!(authority.acquire_exclusive_for_terminal_recovery().is_err());
    assert!(authority.acquire_for_tail_reconciliation().is_err());
    assert!(index
        .project_tape_log(&authority.freeze().unwrap())
        .is_err());
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn structural_snapshot_contention_has_a_typed_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x86; 16];
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let writer = authority.acquire_exclusive().unwrap();
    let (record, _, _) = crate::authority::contract_tests::structural_fixture(id);
    let structural = writer
        .open_structural(record.block_size, record.scheme.unwrap())
        .unwrap();
    let core = writer.core.clone();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _guard = core.lock().unwrap();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    held_rx.recv().unwrap();
    let start = std::time::Instant::now();
    let error = structural.freeze().unwrap_err();
    assert!(
        matches!(error, JournalError::Io(ref e) if e.kind() == io::ErrorKind::TimedOut),
        "{error}"
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    release_tx.send(()).unwrap();
    thread.join().unwrap();
}

/// Native catalog headers preserve wide file numbers without narrowing.
#[test]
fn checkpoint_carriers_preserve_tape_file_numbers_beyond_u32() {
    use crate::tape_log::wire::Wire;
    let mut source = crate::checkpoint::tests::record([0x7a; 16]);
    let number = u64::from(u32::MAX) + 17;
    source.objects[0].copy.tape_file_number = number;
    source.objects[0].object_recovery_row.tape_file_number = number;
    let header = crate::tape_log::payload::CommittedObjectProjection::from(&source.objects[0]);
    let mut bytes = Vec::new();
    crate::tape_log::payload::encode_header(&mut bytes, &header).unwrap();
    let decoded =
        crate::tape_log::payload::CommittedObjectProjection::get(&mut bytes.as_slice()).unwrap();
    assert_eq!(decoded, header);
    assert_eq!(decoded.copy.tape_file_number, number);
    assert_eq!(decoded.object_recovery_row.tape_file_number, number);
}

/// Real owner/publication fixture with an open session and three uncovered
/// committed blocks. Unlike the semantic legacy fixture it writes SessionOpened.
fn session_owner_fixture() -> (
    tempfile::TempDir,
    TapeAuthority,
    TapeAuthorityLease,
    FilePublication,
) {
    let dir = tempfile::tempdir().unwrap();
    let id = [0x91; 16];
    let (seed, _, _) = crate::authority::contract_tests::structural_fixture(id);
    let mut report: AuthorityCommit = (&seed).into();
    let mut sidecar = report.barrier.take().unwrap();
    report.object_files[0].files.truncate(1);
    report.object_files[0].protected = 0;
    report.payloads[0].projection.copy.protected_until_ordinal = Some(0);
    report.protected = 0;
    report.next_file = 2;
    report.cursor.lba = 6;
    report.check = Some((CheckValue(7), CheckProvenance::OriginalWrite));
    report.session_id = Some([9; 16]);
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    writer
        .publish_initialized(
            report.block_size,
            report.scheme.clone(),
            CommitCursor {
                partition: 0,
                lba: 2,
            },
        )
        .unwrap();
    writer
        .open_session([9; 16], "2026-11-02T00:00:00Z".into())
        .unwrap();
    writer.stage_report(&report).unwrap();
    writer.complete_report(&report).unwrap();
    let scheme = report.scheme.unwrap();
    let parity =
        u64::from(scheme.stripes_per_neighborhood) * u64::from(scheme.parity_blocks_per_stripe);
    let h = remanence_parity::sidecar::checked_sidecar_index_capacity_layout(
        u64::from(report.block_size),
        parity,
        report.total,
    )
    .unwrap()
    .block_count;
    sidecar.files[0].block_count = 2 * h + parity + 1;
    sidecar.cursor = Some(CommitCursor {
        partition: 0,
        lba: 6 + sidecar.files[0].block_count + 1,
    });
    (dir, authority, writer, sidecar)
}

#[test]
fn session_owner_closeout_advances_latest_cursor_and_survives_reopen() {
    let (_dir, authority, mut writer, sidecar) = session_owner_fixture();
    let before = writer.freeze().unwrap();
    assert!(before.tape_summary().closeout_due());
    writer
        .publish_closed_out(
            [3; 16],
            Some([9; 16]),
            &sidecar,
            "2026-11-02T00:01:00Z".into(),
        )
        .unwrap();
    let current = writer.latest_commit().unwrap().unwrap();
    assert_eq!(current.cursor, sidecar.cursor.unwrap());
    assert_eq!(current.next_file, 3);
    assert_eq!((current.protected, current.total), (3, 3));
    assert_eq!(current.check, None);
    writer
        .end_session(
            [9; 16],
            "2026-11-02T00:02:00Z".into(),
            SessionEndReason::CleanClose,
            None,
            std::iter::empty(),
        )
        .unwrap();
    assert_eq!(writer.bound_session_id(), None);
    drop(writer);
    let after = authority.freeze().unwrap();
    assert!(!after.tape_summary().closeout_due());
    assert!(after.tape_summary().open_session.is_none());
    assert_eq!(
        after
            .tape_summary()
            .last_session_end
            .as_ref()
            .unwrap()
            .reason,
        SessionEndReason::CleanClose
    );
    assert!(before.tape_summary().open_session.is_some());
    assert_eq!(before.tape_summary().protected, 0);
    let mut writer = authority.acquire_exclusive().unwrap();
    writer
        .open_session([10; 16], "2026-11-02T00:03:00Z".into())
        .unwrap();
    assert_eq!(writer.bound_session_id(), Some([10; 16]));
    assert_eq!(
        writer
            .freeze()
            .unwrap()
            .tape_summary()
            .open_session
            .as_ref()
            .unwrap()
            .cursor
            .lba,
        sidecar.cursor.unwrap().lba
    );
}

#[test]
fn session_owner_streamed_end_preserves_tail_and_rejects_wrong_closeout_owner() {
    let (_dir, authority, mut writer, sidecar) = session_owner_fixture();
    let tail = TailFile {
        tape_file_number: 2,
        identity: TailIdentity::Object("abandoned".into()),
        known_blocks: 2,
        unknown_blocks: 4,
        filemark: TailFilemark::Absent,
    };
    let sequence = writer.log_reference().unwrap().0;
    let error = writer
        .end_session(
            [9; 16],
            "2026-11-02T00:01:00Z".into(),
            SessionEndReason::Abort,
            None,
            [Ok(tail.clone()), Err(invalid("tail producer failed"))],
        )
        .unwrap_err();
    assert!(error.to_string().contains("tail producer failed"));
    assert_eq!(writer.log_reference().unwrap().0, sequence);
    writer
        .end_session(
            [9; 16],
            "2026-11-02T00:01:00Z".into(),
            SessionEndReason::Abort,
            None,
            [Ok(tail.clone())],
        )
        .unwrap();
    let ended = writer.freeze().unwrap();
    assert!(writer
        .publish_closed_out(
            [3; 16],
            Some([10; 16]),
            &sidecar,
            "2026-11-02T00:02:00Z".into()
        )
        .is_err());
    writer
        .publish_closed_out(
            [3; 16],
            Some([9; 16]),
            &sidecar,
            "2026-11-02T00:02:00Z".into(),
        )
        .unwrap();
    drop(writer);
    let mut found = Vec::new();
    ended
        .visit_session_tail(&mut |row| {
            found.push(row.clone());
            Ok(())
        })
        .unwrap();
    assert_eq!(found, [tail]);
    let mut count = 0;
    authority
        .freeze()
        .unwrap()
        .visit_session_tail(&mut |_| {
            count += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn session_owner_every_new_record_lost_publication_response_poisoned() {
    for tag in [17, 18, 19] {
        for after in [false, true] {
            let (_dir, authority, mut writer, sidecar) = session_owner_fixture();
            if tag == 18 {
                writer
                    .publish_closed_out(
                        [3; 16],
                        Some([9; 16]),
                        &sidecar,
                        "2026-11-02T00:01:00Z".into(),
                    )
                    .unwrap();
                writer
                    .end_session(
                        [9; 16],
                        "2026-11-02T00:02:00Z".into(),
                        SessionEndReason::CleanClose,
                        None,
                        std::iter::empty(),
                    )
                    .unwrap();
            }
            writer
                .set_publication_observer(store::PublicationObserver(Arc::new(
                    move |record, _, completed| {
                        if record.tag() as u16 == tag && completed == after {
                            Err(io::Error::other("publication response lost"))
                        } else {
                            Ok(())
                        }
                    },
                )))
                .unwrap();
            let result = match tag {
                17 => writer.publish_closed_out(
                    [3; 16],
                    Some([9; 16]),
                    &sidecar,
                    "2026-11-02T00:03:00Z".into(),
                ),
                18 => writer.open_session([10; 16], "2026-11-02T00:03:00Z".into()),
                19 => writer.end_session(
                    [9; 16],
                    "2026-11-02T00:03:00Z".into(),
                    SessionEndReason::DriveError,
                    None,
                    std::iter::empty(),
                ),
                _ => unreachable!(),
            };
            let error = result.unwrap_err();
            assert_eq!(error.is_checkpoint_append_durable(), after, "tag {tag}");
            assert_eq!(
                error.is_checkpoint_append_authority_uncertain(),
                !after,
                "tag {tag}"
            );
            assert!(writer.ensure_abandonment_allowed().is_err());
            assert!(writer.latest_commit().is_err());
            assert!(writer.read_cache.lock().unwrap().is_none());
            drop(writer);
            if after {
                let state = authority.freeze().unwrap();
                match tag {
                    17 => assert!(!state.tape_summary().closeout_due()),
                    18 => assert_eq!(
                        state
                            .tape_summary()
                            .open_session
                            .as_ref()
                            .unwrap()
                            .session_id,
                        [10; 16]
                    ),
                    19 => assert_eq!(
                        state
                            .tape_summary()
                            .last_session_end
                            .as_ref()
                            .unwrap()
                            .reason,
                        SessionEndReason::DriveError
                    ),
                    _ => unreachable!(),
                }
            } else {
                assert!(authority.acquire_exclusive().is_err());
            }
        }
    }
}

#[test]
fn session_owner_terminal_plan_keeps_closed_out_media_base() {
    let (_dir, authority, mut writer, sidecar) = session_owner_fixture();
    writer
        .publish_closed_out(
            [3; 16],
            Some([9; 16]),
            &sidecar,
            "2026-11-02T00:01:00Z".into(),
        )
        .unwrap();
    writer
        .end_session(
            [9; 16],
            "2026-11-02T00:02:00Z".into(),
            SessionEndReason::CleanClose,
            None,
            std::iter::empty(),
        )
        .unwrap();
    let id = writer.tape_uuid();
    let (mut seed, _, _) = crate::authority::contract_tests::structural_fixture(id);
    seed.eod_lba = sidecar.cursor.unwrap().lba;
    let mut prefix = crate::checkpoint::tests::terminal_prefix_plan_for_parity_record(&seed);
    prefix.sidecar_directory_entries[0].sidecar_total_block_count = sidecar.files[0].block_count;
    let scheme = seed.scheme.as_ref().unwrap();
    let parity =
        u64::from(scheme.stripes_per_neighborhood) * u64::from(scheme.parity_blocks_per_stripe);
    prefix.sidecar_directory_entries[0].sidecar_header_block_count =
        remanence_parity::sidecar::checked_sidecar_index_capacity_layout(
            u64::from(seed.block_size),
            parity,
            3,
        )
        .unwrap()
        .block_count;
    prefix.sidecar_directory_entries[0].parity_shard_block_count = parity;
    let mut plan = finalization_intent(id);
    plan.layout = remanence_parity::TerminalTailLayout::new(
        0,
        seed.block_size,
        prefix.tail_start_tape_file_number,
        prefix.tail_start_lba,
        4,
        4096,
    )
    .unwrap()
    .try_into()
    .unwrap();
    plan.terminal_prefix = Some(prefix);
    let plan: AuthorityFinalization = (&plan).into();
    writer.begin_finalization(&plan).unwrap();
    writer.accept_preparation().unwrap();
    let base = writer.latest_commit().unwrap().unwrap();
    assert_eq!(base.cursor, sidecar.cursor.unwrap());
    assert_eq!((base.protected, base.total, base.next_file), (3, 3, 3));
    writer
        .publish_terminal_prefix(&plan.terminal_prefix.as_ref().unwrap().publication)
        .unwrap();
    assert_eq!(writer.latest_commit().unwrap().unwrap().cursor, base.cursor);
    drop(writer);
    assert_eq!(
        authority.latest_commit().unwrap().unwrap().cursor,
        base.cursor
    );
}

/// Display readers see the durable prefix during staging, then the next fold;
/// actual poison still fails, and an observer never extends the writer flock.
#[test]
fn b7_fix_observer_staging_poison_and_lease_lifetime() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0xb7; 16];
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    let report: AuthorityCommit = (&record(id)).into();
    writer
        .publish_initialized(
            report.block_size,
            None,
            CommitCursor {
                partition: 0,
                lba: 2,
            },
        )
        .unwrap();
    writer
        .open_session([9; 16], "2026-11-02T00:00:00Z".into())
        .unwrap();
    let before = authority.observe_summary().unwrap();
    writer.stage_report(&report).unwrap();
    assert_eq!(
        format!("{:?}", authority.observe_summary().unwrap()),
        format!("{before:?}")
    );
    writer.complete_report(&report).unwrap();
    assert!(
        authority.observe_summary().unwrap().last_closed_sequence > before.last_closed_sequence
    );
    let mirror = OBSERVED_OWNERS.get().unwrap().lock().unwrap()[authority.path()]
        .lock()
        .unwrap()
        .owner
        .upgrade()
        .unwrap();
    drop(writer);
    let writer = authority.acquire_exclusive().unwrap();
    assert!(
        mirror.read().unwrap().is_ok(),
        "old display snapshot can outlive a lease without retaining flock"
    );
    let core = writer.core.clone();
    assert!(std::thread::spawn(move || {
        let _guard = core.lock().unwrap();
        panic!("owner panic");
    })
    .join()
    .is_err());
    assert!(authority
        .observe_summary()
        .unwrap_err()
        .to_string()
        .contains("panicked"));
}

#[test]
fn b7_fix_observer_replays_once_per_change_and_rejects_after_durable_failure() {
    let dir = tempfile::tempdir().unwrap();
    let id = [0xb8; 16];
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    authority.append(&record(id)).unwrap();
    for _ in 0..20 {
        authority.observe_summary().unwrap();
    }
    assert_eq!(
        OBSERVED_OWNERS.get().unwrap().lock().unwrap()[authority.path()]
            .lock()
            .unwrap()
            .replays,
        1
    );
    // Same-length corruption must invalidate the cached answer too.
    let bytes = std::fs::read(authority.path()).unwrap();
    let mut corrupt = bytes.clone();
    corrupt[0] ^= 1;
    std::fs::write(authority.path(), corrupt).unwrap();
    assert!(authority.observe_summary().is_err());
    std::fs::write(authority.path(), bytes).unwrap();
    authority.observe_summary().unwrap();
    assert_eq!(
        OBSERVED_OWNERS.get().unwrap().lock().unwrap()[authority.path()]
            .lock()
            .unwrap()
            .replays,
        2
    );
    let mut writer = authority.acquire_exclusive().unwrap();
    writer
        .set_publication_observer(crate::tape_log::store::PublicationObserver(Arc::new(
            |_, _, durable| {
                if durable {
                    Err(io::Error::other("after durable fold failure"))
                } else {
                    Ok(())
                }
            },
        )))
        .unwrap();
    let report: AuthorityCommit = (&crate::checkpoint::tests::second_record(id)).into();
    assert!(writer
        .publish_report(&report)
        .unwrap_err()
        .is_checkpoint_append_durable());
    assert!(authority
        .observe_summary()
        .unwrap_err()
        .to_string()
        .contains("durable fold failed"));
}

/// A cold replay serializes only its own tape, including concurrent cache misses.
#[test]
fn b7_fix2_slow_replay_does_not_block_other_status_or_session_open() {
    let dir = tempfile::tempdir().unwrap();
    let slow = TapeAuthority::open_fixture(dir.path(), [0xc1; 16]).unwrap();
    slow.append(&record([0xc1; 16])).unwrap();
    let other = TapeAuthority::open_fixture(dir.path(), [0xc2; 16]).unwrap();
    other
        .acquire_exclusive()
        .unwrap()
        .publish_initialized(
            record([0xc2; 16]).block_size,
            None,
            CommitCursor {
                partition: 0,
                lba: 2,
            },
        )
        .unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let observation = slow.observation().unwrap();
    observation.lock().unwrap().before_replay = Some(Box::new(move || {
        entered_tx.send(()).unwrap();
        release_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
    }));
    let slow = Arc::new(slow);
    let first = {
        let slow = slow.clone();
        std::thread::spawn(move || slow.observe_summary().unwrap())
    };
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    let second = {
        let slow = slow.clone();
        std::thread::spawn(move || slow.observe_summary().unwrap())
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let independent = std::thread::spawn(move || {
        other.observe_summary().unwrap();
        let mut lease = other.acquire_exclusive().unwrap();
        lease
            .open_session([3; 16], "2026-11-02T00:00:00Z".into())
            .unwrap();
        done_tx.send(()).unwrap();
    });
    let result = done_rx.recv_timeout(std::time::Duration::from_secs(2));
    release_tx.send(()).unwrap();
    first.join().unwrap();
    second.join().unwrap();
    independent.join().unwrap();
    result.expect("another tape's status and session open must finish during slow replay");
    assert_eq!(observation.lock().unwrap().replays, 1);
}
