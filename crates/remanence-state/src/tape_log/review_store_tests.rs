//! Review regressions at the storage durability and media-barrier boundaries.
use super::*;
use std::sync::{atomic::Ordering::SeqCst, Arc};

#[test]
fn directory_fsync_failure_fences_reacquisition_and_explicit_clear_rewrites() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let faults = Arc::new(Faults::default());
    let _registration = remanence_library::durable_file::test_support::register(
        dir.path().to_owned(),
        Arc::new(FaultDirectory(faults.clone())),
    );
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let group = bot_group(&lease);
    faults.fail.store(4, SeqCst);
    assert!(matches!(lease.append(group), Err(StoreError::Ambiguous(_))));
    assert!(matches!(lease.prepare(), Err(StoreError::Fenced)));
    drop(lease);
    faults.fail.store(0, SeqCst);
    assert!(matches!(store.acquire(false), Err(StoreError::Fenced)));
    assert!(store.path().with_extension("ambiguous").exists());
    let inode = std::fs::metadata(store.path()).unwrap().ino();
    store.clear_ambiguity().unwrap();
    assert_ne!(std::fs::metadata(store.path()).unwrap().ino(), inode);
    assert!(!store.path().with_extension("ambiguous").exists());
    let mut lease = store.acquire(false).unwrap();
    append_object(&mut lease);
    assert_eq!(lease.summary().object_count, 1);
}

#[test]
fn process_fence_survives_loss_of_best_effort_fence_file() {
    let dir = tempfile::tempdir().unwrap();
    let faults = Arc::new(Faults::default());
    let _registration = remanence_library::durable_file::test_support::register(
        dir.path().to_owned(),
        Arc::new(FaultDirectory(faults.clone())),
    );
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let group = lease.prepare().unwrap();
    let closer = Record::Committed(commit(group.reference(0), 2, 6, 1));
    faults.fail.store(2, SeqCst);
    assert!(matches!(
        lease.append(group.close(&closer).unwrap()),
        Err(StoreError::Ambiguous(_))
    ));
    drop(lease);
    faults.fail.store(0, SeqCst);
    std::fs::remove_file(store.path().with_extension("ambiguous")).unwrap();
    let reopened = Store::open_fixture(dir.path(), UUID).unwrap();
    assert!(matches!(reopened.acquire(false), Err(StoreError::Fenced)));
    let authority = crate::TapeAuthority::open_fixture(dir.path(), UUID).unwrap();
    authority.reconcile_restart(0).unwrap();
    assert_eq!(reopened.acquire(false).unwrap().summary().barrier_count, 2);
}

#[test]
fn repair_resumes_after_enospc_and_refuses_changed_boundary_or_digest() {
    let dir = tempfile::tempdir().unwrap();
    let faults = Arc::new(Faults::default());
    let _registration = remanence_library::durable_file::test_support::register(
        dir.path().to_owned(),
        Arc::new(FaultDirectory(faults.clone())),
    );
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    for _ in 0..200 {
        append_object(&mut lease);
    }
    let through = lease.summary().last_closed_sequence;
    let end = lease.summary().closed_end;
    assert!(
        end > 65536,
        "exercise a partial multi-write replacement copy"
    );
    drop(lease);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.path())
        .unwrap()
        .write_all(&[0xff; 28])
        .unwrap();
    let original = std::fs::read(store.path()).unwrap();
    faults.fail.store(5, SeqCst);
    let error = store.repair_tail(through, through).unwrap_err();
    assert!(matches!(error, StoreError::Io(ref e) if e.raw_os_error() == Some(28)));
    assert!(faults.replacement_writes.load(SeqCst) >= 2);
    assert!(store.path().with_extension("repair-required").exists());
    assert_eq!(std::fs::read(store.path()).unwrap(), original);
    faults.fail.store(0, SeqCst);
    assert!(store.repair_tail(through + 1, through).is_err());
    let mut changed = original.clone();
    *changed.last_mut().unwrap() ^= 1;
    std::fs::write(store.path(), &changed).unwrap();
    assert!(store.repair_tail(through, through).is_err());
    std::fs::write(store.path(), &original).unwrap();
    let evidence = store.repair_tail(through, through).unwrap();
    assert_eq!(evidence.retained_end, end);
    assert_eq!(std::fs::read(&evidence.archive).unwrap(), original);
    assert_eq!(std::fs::metadata(store.path()).unwrap().len(), end);
    // Also resume after replacement completed but its response was lost.
    store.repair_tail(through, through).unwrap();
    assert!(matches!(store.acquire(false), Err(StoreError::Refused(_))));
}

#[test]
fn repair_resumes_after_process_crash_between_fence_and_replacement() {
    const CHILD: &str = "REMANENCE_B3A_REPAIR_CRASH_DIR";
    if let Some(path) = std::env::var_os(CHILD) {
        let faults = Arc::new(Faults::default());
        faults.fail.store(6, SeqCst);
        let _registration = remanence_library::durable_file::test_support::register(
            path.clone().into(),
            Arc::new(FaultDirectory(faults)),
        );
        let store = store(std::path::Path::new(&path));
        store.repair_tail(4, 4).unwrap();
        panic!("crash hook was not reached");
    }
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    assert_eq!(lease.summary().last_closed_sequence, 4);
    let end = lease.summary().closed_end;
    drop(lease);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.path())
        .unwrap()
        .write_all(&[0xff; 28])
        .unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tape_log::tests::disk::review::repair_resumes_after_process_crash_between_fence_and_replacement", "--nocapture"])
        .env(CHILD, dir.path()).status().unwrap();
    assert_eq!(status.code(), Some(87));
    assert!(store.path().with_extension("repair-required").exists());
    assert_eq!(std::fs::metadata(store.path()).unwrap().len(), end + 28);
    store.repair_tail(4, 4).unwrap();
    assert_eq!(std::fs::metadata(store.path()).unwrap().len(), end);
}

#[test]
fn closer_can_first_observe_seal_due_at_the_media_barrier() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let group = lease.prepare().unwrap();
    let staged = lease.stage(group).unwrap();
    // This proof only exists after staging. It was absent from retained authority.
    let reason = SealDueReason::HardwareEarlyWarning;
    let mut c = commit(staged.reference(0), 2, 6, 1);
    c.seal_due = Some(reason);
    staged.complete(&Record::Committed(c)).unwrap();
    assert_eq!(lease.summary().seal_due, Some(reason));
    drop(lease);
    assert_eq!(store.snapshot().unwrap().summary().seal_due, Some(reason));
}

#[test]
fn disappeared_active_log_is_refused_and_temporary_cleanup_is_uuid_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let id = uuid::Uuid::from_bytes(UUID).simple().to_string();
    for purpose in ["spool", "initial", "replacement", "repair-record"] {
        std::fs::write(
            dir.path().join(format!(".{purpose}-{id}-orphan")),
            b"orphan",
        )
        .unwrap();
    }
    let foreign = dir
        .path()
        .join(".spool-00000000000000000000000000000001-live");
    std::fs::write(&foreign, b"another tape").unwrap();
    let mut lease = store.acquire(true).unwrap();
    for purpose in ["spool", "initial", "replacement", "repair-record"] {
        assert!(!dir.path().join(format!(".{purpose}-{id}-orphan")).exists());
    }
    assert!(foreign.exists());
    append_object(&mut lease);
    let group = lease.prepare().unwrap();
    std::fs::remove_file(store.path()).unwrap();
    assert!(matches!(lease.stage(group), Err(StoreError::Refused(_))));
    assert!(!store.path().exists());
}

#[test]
fn truncated_header_has_one_typed_error_on_every_open_path() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    drop(lease);
    let bytes = std::fs::read(store.path()).unwrap();
    for cut in 0..51 {
        std::fs::write(store.path(), &bytes[..cut]).unwrap();
        assert!(
            matches!(
                Store::open_fixture(dir.path(), UUID),
                Err(StoreError::Header)
            ),
            "cut={cut}"
        );
        assert!(
            matches!(store.acquire(false), Err(StoreError::Header)),
            "cut={cut}"
        );
        assert!(
            matches!(store.snapshot(), Err(StoreError::Header)),
            "cut={cut}"
        );
    }
}

#[test]
fn staged_prefix_and_closer_are_written_one_frame_per_call() {
    let dir = tempfile::tempdir().unwrap();
    let faults = Arc::new(Faults::default());
    let _registration = remanence_library::durable_file::test_support::register(
        dir.path().to_owned(),
        Arc::new(FaultDirectory(faults.clone())),
    );
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let bytes = std::fs::read(store.path()).unwrap();
    let mut decoder = Decoder::at(&bytes[51..], 1, 51);
    let mut writes = vec![51];
    while let Some(frame) = decoder.next_frame().unwrap() {
        writes.push((frame.end - frame.offset) as usize);
    }
    assert_eq!(*faults.writes.lock().unwrap(), writes);
    assert_eq!(writes.len(), 5, "one header and four separate frame writes");
}

#[test]
fn unverified_check_reaches_the_compatibility_report_unchanged() {
    use crate::authority::TapeAuthorityView;
    // Seed the native cold-rebuild fixture, then visit through the B2 adapter.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new_fixture(
        dir.path(),
        Header {
            tape_uuid: UUID,
            block_size: BLOCK,
            scheme: Some(scheme()),
        },
    )
    .unwrap();
    let mut lease = store.acquire(true).unwrap();
    let mut group = lease.prepare().unwrap();
    group.push(&init(true), &[]).unwrap();
    let row = object(1, 2, Some(0), 3);
    group
        .push(
            &Record::PayloadContinuation(Continuation {
                group: group.sequence(),
                chunk: Chunk::FileRow(row.clone()),
            }),
            &[],
        )
        .unwrap();
    let mut bytes = Vec::new();
    encode_header(&mut bytes, &header(&row, 3)).unwrap();
    group
        .push(
            &Record::PayloadContinuation(Continuation {
                group: group.sequence(),
                chunk: Chunk::Catalog {
                    tape_file_number: 1,
                    ordinal: 0,
                    last: true,
                },
            }),
            &bytes,
        )
        .unwrap();
    let mut c = commit(group.reference(0), 2, 6, 1);
    c.check = CheckValue(7);
    c.provenance = CheckProvenance::UnverifiedRebuild;
    c.unverified_end = Some(3);
    lease
        .append(group.close(&Record::Committed(c)).unwrap())
        .unwrap();
    drop(lease);
    let authority =
        crate::tape_log::authority::TapeAuthority::open_fixture(dir.path(), UUID).unwrap();
    let mut seen = 0;
    authority
        .visit_commits(&mut |report| {
            assert_eq!(
                report.check,
                Some((CheckValue(7), CheckProvenance::UnverifiedRebuild))
            );
            seen += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(seen, 1);
}

#[test]
fn every_byte_crash_cut_of_a_multiframe_append_is_torn_or_complete() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let boundary = lease.summary().closed_end as usize;
    append_object(&mut lease);
    drop(lease);
    let full = std::fs::read(store.path()).unwrap();
    let mut decoder = Decoder::at(&full[boundary..], 5, boundary as u64);
    let mut boundaries = vec![boundary];
    while let Some(frame) = decoder.next_frame().unwrap() {
        boundaries.push(frame.end as usize);
    }
    assert_eq!(boundaries.len(), 4, "row, catalog continuation, closer");
    for cut in boundary..=full.len() {
        std::fs::write(store.path(), &full[..cut]).unwrap();
        let snapshot = store
            .snapshot()
            .unwrap_or_else(|e| panic!("cut={cut}: {e}"));
        let complete = cut == boundary || cut == full.len();
        assert_eq!(snapshot.tail() == Tail::Complete, complete, "cut={cut}");
        let expected = if cut == full.len() { 2 } else { 1 };
        assert_eq!(snapshot.summary().object_count, expected, "cut={cut}");
        // Check byte classification independently from group closure.
        let relative = cut - boundary;
        let mut bytes = Decoder::at(&full[boundary..cut], 5, boundary as u64);
        while bytes
            .next_frame()
            .unwrap_or_else(|e| panic!("cut={cut}: {e}"))
            .is_some()
        {}
        if boundaries.contains(&cut) {
            assert_eq!(bytes.tail(), Tail::Complete);
        } else if relative < PREFIX_LEN {
            assert!(matches!(
                bytes.tail(),
                Tail::Torn {
                    kind: TornKind::ShortPrefix,
                    ..
                }
            ));
        } else {
            assert!(matches!(bytes.tail(), Tail::Torn { .. }));
        }
        assert_eq!(
            store.acquire(false).unwrap().summary().object_count,
            expected
        );
    }
    eprintln!(
        "append_crash_cuts offsets={} frames={}",
        full.len() - boundary + 1,
        boundaries.len() - 1
    );
    // Any torn cut is repaired under the next exclusive append and never removes
    // the preceding acknowledged Object.
    std::fs::write(store.path(), &full[..boundary + 13]).unwrap();
    let mut lease = store.acquire(false).unwrap();
    append_object(&mut lease);
    assert_eq!(lease.summary().object_count, 2);
}

#[test]
fn durable_ambiguity_fence_refuses_a_new_process_and_clear_failure_stays_fenced() {
    const CHILD: &str = "REMANENCE_B3A_AMBIGUITY_DIR";
    if let Some(path) = std::env::var_os(CHILD) {
        let store = Store::open_fixture(std::path::Path::new(&path), UUID).unwrap();
        assert!(matches!(store.acquire(false), Err(StoreError::Fenced)));
        assert!(matches!(store.snapshot(), Err(StoreError::Fenced)));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let faults = Arc::new(Faults::default());
    let _registration = remanence_library::durable_file::test_support::register(
        dir.path().to_owned(),
        Arc::new(FaultDirectory(faults.clone())),
    );
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let group = lease.prepare().unwrap();
    let closer = Record::Committed(commit(group.reference(0), 2, 6, 1));
    faults.fail.store(2, SeqCst);
    assert!(
        matches!(lease.append(group.close(&closer).unwrap()), Err(StoreError::Ambiguous(ref e)) if e.raw_os_error() == Some(5))
    );
    drop(lease);
    faults.fail.store(0, SeqCst);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tape_log::tests::disk::review::durable_ambiguity_fence_refuses_a_new_process_and_clear_failure_stays_fenced", "--nocapture"])
        .env(CHILD, dir.path()).status().unwrap();
    assert!(status.success());
    faults.fail.store(4, SeqCst);
    assert!(store.clear_ambiguity().is_err());
    assert!(matches!(store.acquire(false), Err(StoreError::Fenced)));
    faults.fail.store(0, SeqCst);
    store.clear_ambiguity().unwrap();
    assert_eq!(store.acquire(false).unwrap().summary().barrier_count, 2);
}

#[test]
fn invalid_post_barrier_closer_does_not_publish_or_poison_the_old_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let before = std::fs::read(store.path()).unwrap();
    let group = lease.prepare().unwrap();
    let staged = lease.stage(group).unwrap();
    let mut c = commit(staged.reference(0), 2, 6, 1);
    c.cursor.lba += 1;
    assert!(staged.complete(&Record::Committed(c)).is_err());
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    assert!(matches!(lease.prepare(), Err(StoreError::Fenced)));
    drop(lease);
    assert_eq!(store.acquire(false).unwrap().summary().object_count, 1);
}

#[test]
fn stage_data_sync_failure_rolls_back_before_media_without_fencing() {
    for fresh in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let faults = Arc::new(Faults::default());
        let _registration = remanence_library::durable_file::test_support::register(
            dir.path().to_owned(),
            Arc::new(FaultDirectory(faults.clone())),
        );
        let store = store(dir.path());
        let mut lease = store.acquire(true).unwrap();
        if !fresh {
            append_object(&mut lease);
        }
        let frozen = lease.snapshot().unwrap();
        let before = std::fs::read(store.path()).ok();
        let mut group = lease.prepare().unwrap();
        if fresh {
            group.push(&init(false), &[]).unwrap();
        } else {
            group
                .push(
                    &Record::PayloadContinuation(Continuation {
                        group: group.sequence(),
                        chunk: Chunk::FileRow(object(2, 6, None, 3)),
                    }),
                    &[],
                )
                .unwrap();
        }
        faults.fail.store(7, SeqCst);
        let mut media_calls = 0;
        let error = match lease.stage(group) {
            Ok(_) => {
                media_calls += 1;
                panic!("media barrier must not be reached: {media_calls}");
            }
            Err(error) => error,
        };
        assert_eq!(media_calls, 0);
        assert!(matches!(error, StoreError::Io(ref e) if e.raw_os_error() == Some(5)));
        assert_eq!(std::fs::read(store.path()).ok(), before);
        assert!(!store.path().with_extension("ambiguous").exists());
        assert!(lease.prepare().is_ok());
        assert_eq!(
            lease.snapshot().unwrap().summary().object_count,
            u64::from(!fresh)
        );
        assert_eq!(frozen.summary().object_count, u64::from(!fresh));
        faults.fail.store(0, SeqCst);
        append_object(&mut lease);
        drop(lease);
        assert_eq!(
            store.acquire(false).unwrap().summary().object_count,
            1 + u64::from(!fresh)
        );
    }
}

#[test]
fn staged_data_sync_precedes_media_and_empty_stage_needs_no_data_sync() {
    let dir = tempfile::tempdir().unwrap();
    let faults = Arc::new(Faults::default());
    let _registration = remanence_library::durable_file::test_support::register(
        dir.path().to_owned(),
        Arc::new(FaultDirectory(faults.clone())),
    );
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let mut group = lease.prepare().unwrap();
    group.push(&init(false), &[]).unwrap();
    let staged = lease.stage(group).unwrap();
    assert_eq!(faults.events.lock().unwrap().last().unwrap(), "fdatasync");
    assert!(
        !store.path().exists(),
        "synced provisional initialization is not authority"
    );
    faults.events.lock().unwrap().push("media".into());
    let c = Record::Committed(commit(staged.reference(0), 1, 2, 0));
    staged.complete(&c).unwrap();
    assert_eq!(
        &faults.events.lock().unwrap()[..],
        &[
            "append",
            "append",
            "fdatasync",
            "media",
            "append",
            "fsync",
            "rename",
            "directory"
        ]
    );
    faults.events.lock().unwrap().clear();
    let group = lease.prepare().unwrap();
    let staged = lease.stage(group).unwrap();
    assert!(faults.events.lock().unwrap().is_empty());
    let c = Record::Committed(commit(staged.reference(0), 1, 2, 0));
    staged.complete(&c).unwrap();
    assert_eq!(&faults.events.lock().unwrap()[..], &["append", "fsync"]);
}
