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
        purpose: FilePurpose::Checkpoint,
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
    report.seal_due = Some(SealDueReason::HardwareEarlyWarning);
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
        writer.latest_commit().is_err(),
        "legacy projection must not silently return the pre-seal commit"
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
        4
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
    assert!(writer.authority_summary().unwrap().cursor.is_none());
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
fn streaming_report_preserves_unverified_check_and_range_end() {
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
    report.check = Some((CheckValue(7), CheckProvenance::UnverifiedRebuild));
    let authority = TapeAuthority::open_fixture(dir.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    writer
        .publish_report_streaming(&report, &mut |_, _| Ok(()))
        .unwrap();
    let mut count = 0;
    writer
        .visit_reports(&mut |view| {
            assert_eq!(view.summary().check, report.check);
            assert_eq!(view.commit().unverified_end, Some(3));
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
