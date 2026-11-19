//! Real-file store tests reuse the exact B1 semantic fixtures.
use super::*;
use crate::tape_log::store::{Header, Lease, Store, StoreError};
use std::io::Write;
fn store(dir: &std::path::Path) -> Store {
    Store::new_fixture(
        dir,
        Header {
            tape_uuid: UUID,
            block_size: BLOCK,
            scheme: None,
        },
    )
    .unwrap()
}
fn open_fixture_session(lease: &mut Lease) {
    if lease.summary().open_session.is_none() {
        let s = lease.summary();
        let record = Record::SessionOpened(SessionOpened {
            session_id: [9; 16],
            incarnation: s.incarnation,
            base_media_sequence: s.last_media_commit.unwrap(),
            cursor: SessionCursor {
                partition: s.cursor.partition,
                lba: s.cursor.lba,
                next_file: s.next_file,
            },
            opened_at_utc: "2026-11-02T00:00:00Z".into(),
            drive: crate::tape_log::SessionDrive {
                serial: "DRV-TEST".into(),
                vendor: None,
                product: None,
                endpoint: crate::library_log::DriveKey::Standalone(
                    crate::library_log::Standalone {
                        drive_serial: "DRV-TEST".into(),
                    },
                ),
                drive_uuid: None,
            },
        });
        let group = lease.prepare().unwrap();
        lease.append(group.close(&record).unwrap()).unwrap();
    }
}
fn append_object(lease: &mut Lease) {
    let summary = lease.summary().clone();
    if summary.is_fresh() {
        let group = lease.prepare().unwrap();
        lease.append(group.close(&init(false)).unwrap()).unwrap();
    }
    open_fixture_session(lease);
    let mut group = lease.prepare().unwrap();
    let file = summary.next_file.max(1);
    let lba = if summary.is_fresh() {
        2
    } else {
        summary.cursor.lba
    };
    let row = object(file, lba, None, 3);
    group
        .push(
            &Record::PayloadContinuation(Continuation {
                group: group.sequence(),
                chunk: Chunk::FileRow(row.clone()),
            }),
            &[],
        )
        .unwrap();
    let mut payload = Vec::new();
    encode_header(&mut payload, &header(&row, row.tape_file_number * 3)).unwrap();
    group
        .push(
            &Record::PayloadContinuation(Continuation {
                group: group.sequence(),
                chunk: Chunk::Catalog {
                    tape_file_number: file,
                    ordinal: 0,
                    last: true,
                },
            }),
            &payload,
        )
        .unwrap();
    let commit = Record::Committed(commit(
        group.reference(0),
        file + 1,
        lba + 4,
        summary.object_count + 1,
    ));
    let prepared = group.close(&commit).unwrap();
    lease.append(prepared).unwrap();
}
#[test]
fn atomic_first_creation_and_existing_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    assert!(!store.path().exists());
    let group = lease.prepare().unwrap();
    drop(group);
    assert!(!store.path().exists());
    append_object(&mut lease);
    let bytes = std::fs::read(store.path()).unwrap();
    assert_eq!(&bytes[..8], b"REMTAPE\x01");
    assert_eq!(&bytes[10..26], &UUID);
    drop(lease);
    assert!(matches!(
        store.acquire(true),
        Err(StoreError::AlreadyExists)
    ));
    assert_eq!(std::fs::read(store.path()).unwrap(), bytes);
    assert_eq!(store.snapshot().unwrap().summary().object_count, 1);
}
#[test]
fn frozen_snapshot_append_and_copy_prefix_repair() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let frozen = lease.snapshot().unwrap();
    append_object(&mut lease);
    assert_eq!(frozen.summary().object_count, 1);
    assert_eq!(lease.snapshot().unwrap().summary().object_count, 2);
    drop(lease);
    let lock_inode = std::fs::metadata(store.path().with_extension("lock"))
        .unwrap()
        .ino();
    let inode = std::fs::metadata(store.path()).unwrap().ino();
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.path())
        .unwrap()
        .write_all(&[0; 100])
        .unwrap();
    let torn = store.snapshot().unwrap();
    assert!(matches!(torn.tail(), Tail::Torn { .. }));
    let mut lease = store.acquire(false).unwrap();
    append_object(&mut lease);
    assert_ne!(std::fs::metadata(store.path()).unwrap().ino(), inode);
    assert_eq!(
        std::fs::metadata(store.path().with_extension("lock"))
            .unwrap()
            .ino(),
        lock_inode
    );
    for (snapshot, rows) in [(frozen, 2), (torn, 3), (lease.snapshot().unwrap(), 4)] {
        let mut count = 0;
        snapshot
            .visit_file_rows(|_| {
                count += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(count, rows);
    }
}
#[test]
fn corruption_repair_archives_exact_bytes_and_refuses_below_mark() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let first = lease.summary().last_closed_sequence;
    let frozen = lease.snapshot().unwrap();
    append_object(&mut lease);
    let second = lease.summary().last_closed_sequence;
    drop(lease);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.path())
        .unwrap()
        .write_all(&[0xff; 28])
        .unwrap();
    let damaged = std::fs::read(store.path()).unwrap();
    assert!(store.acquire(false).is_err());
    assert!(matches!(
        store.repair_tail(first, second),
        Err(StoreError::BelowProjection { .. })
    ));
    assert_eq!(std::fs::read(store.path()).unwrap(), damaged);
    let evidence = store.repair_tail(second, second).unwrap();
    assert_eq!(std::fs::read(evidence.archive).unwrap(), damaged);
    assert_eq!(store.snapshot().unwrap().summary().object_count, 2);
    let mut count = 0;
    frozen
        .visit_file_rows(|_| {
            count += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(count, 2);
}
#[test]
fn wrong_header_and_candidate_are_refused_without_poisoning() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let group = lease.prepare().unwrap();
    let bad = group
        .close(&Record::Committed(commit(GroupRef::default(), 0, 0, 0)))
        .unwrap();
    assert!(lease.append(bad).is_err());
    assert!(!store.path().exists());
    append_object(&mut lease);
    drop(lease);
    let mut bytes = std::fs::read(store.path()).unwrap();
    bytes[10] ^= 1;
    std::fs::write(store.path(), bytes).unwrap();
    assert!(matches!(store.snapshot(), Err(StoreError::Header)));
}
#[test]
fn staged_first_group_is_invisible_and_drop_fences_lease() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let group = lease.prepare().unwrap();
    let staged = lease.stage(group).unwrap();
    assert!(!store.path().exists());
    drop(staged);
    assert!(matches!(lease.prepare(), Err(StoreError::Fenced)));
    drop(lease);
    assert!(store.acquire(true).unwrap().summary().is_fresh());
}
#[test]
fn three_thousand_short_epochs_and_streamed_plan_directory() {
    crate::tape_log::measure::reset();
    let started = std::time::Instant::now();
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
    // Scaled full-tape capacity: 3 data + delimiter + 2 sidecar + delimiter
    // per short epoch. Every 3-block epoch closes before the 8-block limit.
    const EPOCHS: u64 = 3_000;
    let capacity = 2 + 7 * EPOCHS;
    for epoch in 0..EPOCHS {
        let mut group = lease.prepare().unwrap();
        if epoch == 0 {
            lease.append(group.close(&init(true)).unwrap()).unwrap();
            open_fixture_session(&mut lease);
            group = lease.prepare().unwrap();
        }
        let file = 1 + 2 * epoch;
        let lba = 2 + 7 * epoch;
        let row = object(file, lba, Some(3 * epoch), 3);
        group
            .push(
                &Record::PayloadContinuation(Continuation {
                    group: group.sequence(),
                    chunk: Chunk::FileRow(row.clone()),
                }),
                &[],
            )
            .unwrap();
        let mut catalog = Vec::new();
        encode_header(&mut catalog, &header(&row, 3 * (epoch + 1))).unwrap();
        group
            .push(
                &Record::PayloadContinuation(Continuation {
                    group: group.sequence(),
                    chunk: Chunk::Catalog {
                        tape_file_number: file,
                        ordinal: 0,
                        last: true,
                    },
                }),
                &catalog,
            )
            .unwrap();
        group
            .push(
                &Record::PayloadContinuation(Continuation {
                    group: group.sequence(),
                    chunk: Chunk::FileRow(sidecar(
                        file + 1,
                        lba + 4,
                        epoch,
                        3 * epoch,
                        3 * (epoch + 1),
                    )),
                }),
                &[],
            )
            .unwrap();
        let mut c = commit(group.reference(0), file + 2, lba + 7, epoch + 1);
        c.total = 3 * (epoch + 1);
        c.protected = c.total;
        c.next_epoch = epoch + 1;
        let group = group.close(&Record::Committed(c)).unwrap();
        lease.append(group).unwrap();
        assert!(lease.summary().uncovered.is_empty());
    }
    assert_eq!(lease.summary().cursor.lba, capacity);
    let next = lease.summary().next_file;
    let mut map = plain_row(next, TapeFileKind::ParityMap, 1, capacity);
    map.canonical_metadata_hash = Some([5; 32]);
    let mut group = lease.prepare().unwrap();
    group
        .push(
            &Record::PayloadContinuation(Continuation {
                group: group.sequence(),
                chunk: Chunk::FileRow(map.clone()),
            }),
            &[],
        )
        .unwrap();
    for epoch in 0..EPOCHS {
        let row = sidecar(
            2 + 2 * epoch,
            6 + 7 * epoch,
            epoch,
            3 * epoch,
            3 * (epoch + 1),
        );
        group
            .push(
                &Record::PayloadContinuation(Continuation {
                    group: group.sequence(),
                    chunk: Chunk::PlanDirectory(directory(&row)),
                }),
                &[],
            )
            .unwrap();
    }
    let plan = plan(group.reference(0), next + 1, capacity + 2);
    let layout = plan.layout;
    let group = group
        .close(&Record::FinalizationDecided(Box::new(plan)))
        .unwrap();
    lease.append(group).unwrap();
    let before = lease.snapshot().unwrap();
    let decision = lease.summary().last_closed_sequence;
    let mut group = lease.prepare().unwrap();
    group
        .push(
            &Record::PayloadContinuation(Continuation {
                group: group.sequence(),
                chunk: Chunk::FileRow(map),
            }),
            &[],
        )
        .unwrap();
    let mut c = commit(group.reference(0), next + 1, capacity + 2, EPOCHS);
    c.total = 3 * EPOCHS;
    c.protected = c.total;
    c.next_epoch = EPOCHS;
    let group = group
        .close(&Record::TerminalPrefix {
            decision,
            commit: c,
        })
        .unwrap();
    lease.append(group).unwrap();
    for (index, component) in layout.components.iter().enumerate() {
        let mut group = lease.prepare().unwrap();
        let kind = match component.kind {
            TerminalTailComponentKind::TapeIndexReplica => TapeFileKind::TapeIndexReplica,
            TerminalTailComponentKind::IndexSeparationExtent => TapeFileKind::IndexSeparationExtent,
        };
        group
            .push(
                &Record::PayloadContinuation(Continuation {
                    group: group.sequence(),
                    chunk: Chunk::FileRow(plain_row(
                        component.planned_tape_file_number,
                        kind,
                        component.record_count,
                        component.planned_start_lba,
                    )),
                }),
                &[],
            )
            .unwrap();
        let mut c = commit(
            group.reference(0),
            component.planned_tape_file_number + 1,
            component.planned_start_lba + component.record_count + 1,
            EPOCHS,
        );
        c.total = 3 * EPOCHS;
        c.protected = c.total;
        c.next_epoch = EPOCHS;
        let group = group
            .close(&Record::ComponentWritten {
                decision,
                component: index as u8,
                commit: c,
            })
            .unwrap();
        lease.append(group).unwrap();
    }
    let group = lease.prepare().unwrap();
    let group = group
        .close(&Record::Sealed {
            decision,
            cursor: lease.summary().cursor,
            edition_digest: [2; 32],
        })
        .unwrap();
    lease.append(group).unwrap();
    assert!(lease.summary().sealed);
    assert_eq!(lease.summary().cursor.lba, layout.expected_eod_lba);
    drop(lease);
    let snapshot = store.snapshot().unwrap();
    assert!(snapshot.replay_metrics().peak_extent_capacity <= 32);
    assert!(snapshot.replay_metrics().peak_frame_buffer < 4096);
    let mut n = 0;
    before
        .visit_file_rows(|_| {
            n += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(n, next, "planned map is not durable before its barrier");
    let mut n = 0;
    snapshot
        .visit_file_rows(|row| {
            assert_eq!(row.tape_file_number, n);
            n += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(n, next + 6);
    let mut structural = 0;
    let mut frames = 0;
    snapshot
        .visit_frames(|frame| {
            structural +=
                frame.end - frame.offset - frame.payload.len() as u64 - FRAME_OVERHEAD as u64;
            frames += 1;
            Ok(())
        })
        .unwrap();
    // Run the actual terminal source over this many-epoch plan, with independent
    // structural/Object cursors and no compatibility commit materialization.
    use remanence_parity::authority::{
        TapeAuthoritySnapshot, TerminalAuthoritySource, TerminalPrefixMode,
    };
    use remanence_parity::TapeIndexReplicaRecordSource;
    let authority = crate::tape_log::authority::AuthoritySnapshot::new(before.clone()).unwrap();
    let plan = authority.finalization().unwrap();
    let mut terminal = crate::tape_log::terminal_source::TerminalSource::new(
        before.clone(),
        TerminalPrefixMode::Planned(plan.terminal_prefix.as_ref().unwrap()),
    )
    .unwrap();
    assert_eq!(terminal.summary().counts.structural_entry_count, next + 1);
    let mut structural_rows = 0;
    terminal
        .visit_structural_entries(&mut |_| {
            structural_rows += 1;
            Ok(())
        })
        .unwrap();
    let mut object_rows = 0;
    terminal
        .visit_object_rows(&mut |_| {
            object_rows += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!((structural_rows, object_rows), (next + 1, EPOCHS));
    let rss = std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|s| s.starts_with("VmHWM:"))
        .unwrap()
        .to_owned();
    let counts = crate::tape_log::measure::counts();
    let log_metrics = snapshot.replay_metrics();
    let frame_bytes = log_metrics.structural_bytes
        + log_metrics.payload_bytes
        + log_metrics.frames * FRAME_OVERHEAD as u64;
    eprintln!(
        "counted_passes full={:.6} structural={:.6}",
        counts.full_pass_equivalents(frame_bytes),
        counts.row_pass_equivalents(frame_bytes)
    );
    eprintln!("short_epochs L={} S={structural} N={n} M=0 P={EPOCHS} frames={frames} capacity_blocks={capacity} counts={:?} capture_us={} peak_RSS={rss} elapsed_s={:.3}", snapshot.captured_end(), crate::tape_log::measure::counts(), snapshot.capture_time().as_micros(), started.elapsed().as_secs_f64());
}

#[test]
fn snapshot_capture_releases_lock_before_validation() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    drop(lease);
    let shared = store.lock(false).unwrap();
    let captured = crate::tape_log::snapshot::Captured::open(&store).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let other = store.clone();
    let thread = std::thread::spawn(move || {
        ready_tx.send(()).unwrap();
        let mut lease = other.acquire(false).unwrap();
        append_object(&mut lease);
    });
    ready_rx.recv().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    drop(shared);
    thread.join().unwrap();
    let snapshot = captured.validate().unwrap();
    assert_eq!(snapshot.summary().object_count, 1);
    assert_eq!(store.snapshot().unwrap().summary().object_count, 2);
}

#[test]
fn writer_conflict_is_prompt_and_shared_reader_wait_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let writer = store.acquire(true).unwrap();
    let start = std::time::Instant::now();
    assert!(matches!(store.acquire(false), Err(StoreError::Locked(_))));
    assert!(start.elapsed() < std::time::Duration::from_millis(100));
    assert!(matches!(store.snapshot(), Err(StoreError::Locked(_))));
    let conflict = crate::StateError::from(store.acquire(false).unwrap_err());
    assert!(conflict.is_tape_log_busy(), "{conflict}");
    drop(writer);
    let shared = store.lock(false).unwrap();
    let start = std::time::Instant::now();
    assert!(matches!(store.acquire(false), Err(StoreError::Locked(_))));
    assert!(start.elapsed() >= std::time::Duration::from_millis(250));
    drop(shared);
    store.acquire(true).unwrap();
}

#[derive(Default)]
struct Faults {
    fail: std::sync::atomic::AtomicU8,
    events: std::sync::Mutex<Vec<String>>,
    writes: std::sync::Mutex<Vec<usize>>,
    replacement_writes: std::sync::atomic::AtomicUsize,
}
impl Faults {
    fn event(&self, kind: &str, path: &std::path::Path, fail: u8) -> std::io::Result<()> {
        let name = path.file_name().unwrap().to_string_lossy();
        if name.ends_with(".remtape")
            || name.starts_with(".initial-")
            || kind == "directory"
            || kind == "rename"
        {
            self.events.lock().unwrap().push(kind.into());
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) == fail {
                return Err(std::io::Error::from_raw_os_error(5));
            }
        }
        Ok(())
    }
}
struct FaultFile {
    faults: std::sync::Arc<Faults>,
    path: std::path::PathBuf,
}
impl remanence_library::durable_file::DurableFile for FaultFile {
    fn append(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let name = self.path.file_name().unwrap().to_string_lossy();
        if name.ends_with(".remtape") || name.starts_with(".initial-") {
            self.faults.writes.lock().unwrap().push(bytes.len());
        }
        if self
            .path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".replacement-")
            && self.faults.fail.load(std::sync::atomic::Ordering::SeqCst) == 5
            && self
                .faults
                .replacement_writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                > 0
        {
            return Err(std::io::Error::from_raw_os_error(28));
        }
        self.faults.event("append", &self.path, 1)
    }
    fn sync(&mut self) -> std::io::Result<()> {
        self.faults.event("fsync", &self.path, 2)
    }
    fn sync_data(&mut self) -> std::io::Result<()> {
        self.faults.event("fdatasync", &self.path, 7)
    }
}
struct FaultDirectory(std::sync::Arc<Faults>);
impl remanence_library::durable_file::DurableDirectory for FaultDirectory {
    fn create(
        &self,
        path: &std::path::Path,
    ) -> std::io::Result<Box<dyn remanence_library::durable_file::DurableFile>> {
        if path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".replacement-")
            && self.0.fail.load(std::sync::atomic::Ordering::SeqCst) == 6
        {
            std::process::exit(87);
        }
        self.open(path)
    }
    fn open(
        &self,
        path: &std::path::Path,
    ) -> std::io::Result<Box<dyn remanence_library::durable_file::DurableFile>> {
        Ok(Box::new(FaultFile {
            faults: self.0.clone(),
            path: path.to_owned(),
        }))
    }
    fn rename(&self, _: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
        self.0.event("rename", to, 3)
    }
    fn rename_noreplace(&self, _: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
        if to.exists() {
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        self.0.event("rename", to, 3)
    }
    fn sync(&self, path: &std::path::Path) -> std::io::Result<()> {
        self.0.event("directory", path, 4)
    }
    fn set_len(&self, _: &std::path::Path, _: u64) -> std::io::Result<()> {
        panic!("in-place truncation is forbidden")
    }
}
fn bot_group(lease: &Lease) -> crate::tape_log::store::PreparedGroup {
    lease.prepare().unwrap().close(&init(false)).unwrap()
}
#[test]
fn single_log_fault_observer_surrounds_fsync_and_first_directory_publication() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let faults = Arc::new(Faults::default());
    let _registration = remanence_library::durable_file::test_support::register(
        dir.path().to_owned(),
        Arc::new(FaultDirectory(faults.clone())),
    );
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let observed = faults.clone();
    lease.set_publication_observer(crate::tape_log::store::PublicationObserver(Arc::new(
        move |record, sequence, after| {
            assert!(matches!(record, Record::Initialized(_)));
            assert_eq!(sequence, 1);
            observed.events.lock().unwrap().push(
                if after {
                    "after_tape_log_fsync"
                } else {
                    "before_tape_log_fsync"
                }
                .into(),
            );
            Ok(())
        },
    )));
    lease.append(bot_group(&lease)).unwrap();
    let events = faults.events.lock().unwrap();
    assert_eq!(
        &events[events.len() - 6..],
        &[
            "append",
            "before_tape_log_fsync",
            "fsync",
            "rename",
            "directory",
            "after_tape_log_fsync"
        ]
    );
}

#[test]
fn provisional_data_sync_then_closing_fsync_and_first_directory_sync() {
    use std::sync::{atomic::Ordering::SeqCst, Arc};
    for failure in 0..=4 {
        let dir = tempfile::tempdir().unwrap();
        let faults = Arc::new(Faults::default());
        let _registration = remanence_library::durable_file::test_support::register(
            dir.path().to_owned(),
            Arc::new(FaultDirectory(faults.clone())),
        );
        let store = store(dir.path());
        let mut lease = store.acquire(true).unwrap();
        let group = bot_group(&lease);
        faults.fail.store(failure, SeqCst);
        let result = lease.append(group);
        if failure == 0 {
            result.unwrap();
            let events = faults.events.lock().unwrap();
            assert_eq!(events.iter().filter(|e| *e == "fdatasync").count(), 0);
            assert_eq!(events.iter().filter(|e| *e == "fsync").count(), 1);
            assert_eq!(events.iter().filter(|e| *e == "directory").count(), 1);
            assert_eq!(
                &events[events.len() - 4..],
                &["append", "fsync", "rename", "directory"]
            );
            drop(events);
            open_fixture_session(&mut lease);
            faults.events.lock().unwrap().clear();
            append_object(&mut lease);
            assert_eq!(
                faults
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| *e == "fsync")
                    .count(),
                1
            );
            assert_eq!(
                faults
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| *e == "fdatasync")
                    .count(),
                1
            );
            assert!(!faults
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e == "directory"));
        } else {
            assert!(result.is_err());
            if failure >= 2 {
                assert!(matches!(lease.prepare(), Err(StoreError::Fenced)));
            }
            if failure <= 3 {
                assert!(
                    !store.path().exists(),
                    "no first log before rename succeeds"
                );
            }
            if failure == 4 {
                assert!(
                    store.path().exists(),
                    "directory failure is an ambiguous published file"
                );
            }
        }
    }
}
#[test]
fn fsync_failure_after_existing_append_fences_even_if_bytes_replay() {
    use std::sync::{atomic::Ordering::SeqCst, Arc};
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
    let c = Record::Committed(commit(group.reference(0), 2, 6, 1));
    let prepared = group.close(&c).unwrap();
    faults.fail.store(2, SeqCst);
    assert!(matches!(
        lease.append(prepared),
        Err(StoreError::Ambiguous(_))
    ));
    assert!(matches!(lease.prepare(), Err(StoreError::Fenced)));
    assert!(matches!(lease.snapshot(), Err(StoreError::Fenced)));
    drop(lease);
    faults.fail.store(0, SeqCst);
    assert!(matches!(store.acquire(false), Err(StoreError::Fenced)));
    assert!(matches!(store.snapshot(), Err(StoreError::Fenced)));
    assert!(store.path().with_extension("ambiguous").exists());
    store.clear_ambiguity().unwrap();
    assert_eq!(store.acquire(false).unwrap().summary().barrier_count, 3);
}
#[test]
fn unvalidated_capture_survives_concurrent_torn_repair_and_append() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    drop(lease);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.path())
        .unwrap()
        .write_all(&[0; 512])
        .unwrap();
    let shared = store.lock(false).unwrap();
    let captured = crate::tape_log::snapshot::Captured::open(&store).unwrap();
    drop(shared);
    let other = store.clone();
    std::thread::spawn(move || {
        let mut lease = other.acquire(false).unwrap();
        append_object(&mut lease);
    })
    .join()
    .unwrap();
    let old = captured.validate().unwrap();
    assert_eq!(old.summary().object_count, 1);
    assert!(matches!(old.tail(), Tail::Torn { .. }));
    assert_eq!(store.snapshot().unwrap().summary().object_count, 2);
}
#[test]
fn existing_name_at_first_publish_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let group = bot_group(&lease);
    let staged = lease.stage(group.writer).unwrap();
    std::fs::write(store.path(), b"existing evidence").unwrap();
    assert!(matches!(
        staged.complete(&group.closer),
        Err(StoreError::Ambiguous(_))
    ));
    assert_eq!(std::fs::read(store.path()).unwrap(), b"existing evidence");
    assert!(matches!(lease.prepare(), Err(StoreError::Fenced)));
}
#[test]
fn durable_after_seam_runs_after_directory_sync_and_failure_is_distinct() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let faults = Arc::new(Faults::default());
    let _registration = remanence_library::durable_file::test_support::register(
        dir.path().to_owned(),
        Arc::new(FaultDirectory(faults.clone())),
    );
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let group = bot_group(&lease);
    let result = lease
        .stage(group.writer)
        .unwrap()
        .complete_with_after(&group.closer, || {
            assert_eq!(faults.events.lock().unwrap().last().unwrap(), "directory");
            Err(StoreError::Refused("injected projection error"))
        });
    assert!(matches!(result, Err(StoreError::AfterDurable(_))));
    assert_eq!(lease.summary().barrier_count, 1);
    assert!(lease.prepare().is_ok());
}
#[test]
fn explicit_repair_persists_append_fence_and_exact_boundary_record() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let through = lease.summary().last_closed_sequence;
    drop(lease);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.path())
        .unwrap()
        .write_all(&[0xff; 28])
        .unwrap();
    let evidence = store.repair_tail(through, through).unwrap();
    let record = std::fs::read_to_string(evidence.reconciliation_record).unwrap();
    assert!(record.contains(&format!("\"through_sequence\":{through}")));
    assert!(record.contains("\"physical_tail_reconciliation_required\":true"));
    let json: serde_json::Value = serde_json::from_str(&record).unwrap();
    assert_eq!(
        json["tape_uuid"],
        uuid::Uuid::from_bytes(store.header.tape_uuid)
            .simple()
            .to_string()
    );
    for digest in [&json["retained_digest"], &json["evidence"]["digest"]] {
        let text = digest.as_str().unwrap();
        assert_eq!(text.len(), 64);
        assert!(text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }
    assert!(!json["operator"].as_str().unwrap().contains("User("));
    assert!(matches!(store.acquire(false), Err(StoreError::Refused(_))));
    assert_eq!(
        store.snapshot().unwrap().summary().last_closed_sequence,
        through
    );
    let mut recovery = store.acquire_for_tail_reconciliation().unwrap();
    assert!(recovery.repair_pending());
    assert!(recovery.prepare().is_err());
    let cursor = recovery.summary().cursor;
    assert!(recovery
        .reconcile_repaired_tail(CommitCursor {
            lba: cursor.lba + 1,
            ..cursor
        })
        .is_err());
    assert!(store.path().with_extension("repair-required").exists());
    recovery.reconcile_repaired_tail(cursor).unwrap();
    assert!(!recovery.repair_pending());
    assert!(!store.path().with_extension("repair-required").exists());
    drop(recovery);
    assert!(store.acquire(false).is_ok());
}
#[test]
fn original_write_check_is_retained_in_disk_snapshot() {
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
    lease.append(group.close(&init(true)).unwrap()).unwrap();
    open_fixture_session(&mut lease);
    group = lease.prepare().unwrap();
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
    c.total = 3;
    c.check = CheckValue(7);
    c.provenance = CheckProvenance::OriginalWrite;
    let prepared = group.close(&Record::Committed(c)).unwrap();
    lease.append(prepared).unwrap();
    drop(lease);
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.summary().check, CheckValue(7));
    assert_eq!(
        snapshot.summary().provenance,
        CheckProvenance::OriginalWrite
    );
}

#[test]
fn high_file_count_small_objects_stream_without_a_batch_vector() {
    crate::tape_log::measure::reset();
    let started = std::time::Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let mut group = lease.prepare().unwrap();
    lease.append(group.close(&init(false)).unwrap()).unwrap();
    open_fixture_session(&mut lease);
    group = lease.prepare().unwrap();
    const OBJECTS: u64 = 25_000;
    for file in 1..=OBJECTS {
        let row = object(file, 2 + 4 * (file - 1), None, 3);
        group
            .push(
                &Record::PayloadContinuation(Continuation {
                    group: group.sequence(),
                    chunk: Chunk::FileRow(row.clone()),
                }),
                &[],
            )
            .unwrap();
        let h = header(&row, row.tape_file_number * 3);
        let mut bytes = Vec::new();
        encode_header(&mut bytes, &h).unwrap();
        encode_member(&mut bytes, &h, &member()).unwrap();
        group
            .push(
                &Record::PayloadContinuation(Continuation {
                    group: group.sequence(),
                    chunk: Chunk::Catalog {
                        tape_file_number: file,
                        ordinal: 0,
                        last: true,
                    },
                }),
                &bytes,
            )
            .unwrap();
    }
    let c = Record::Committed(commit(
        group.reference(OBJECTS),
        OBJECTS + 1,
        2 + 4 * OBJECTS,
        OBJECTS,
    ));
    let prepared = group.close(&c).unwrap();
    lease.append(prepared).unwrap();
    drop(lease);
    let snapshot = store.snapshot().unwrap();
    let metrics = snapshot.replay_metrics();
    assert_eq!(metrics.peak_extent_capacity, 0);
    assert!(metrics.peak_frame_buffer < 4096);
    assert!(metrics.peak_catalog_bytes < 4096);
    let mut rows = 0;
    snapshot
        .visit_file_rows(|row| {
            assert_eq!(row.tape_file_number, rows);
            rows += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(rows, OBJECTS + 1);
    let mut members = 0;
    snapshot
        .visit_catalog(|event| {
            if let crate::authority::CatalogPayload::Member(m) = event {
                assert_eq!(m, &member());
                members += 1;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(members, OBJECTS);
    let rss = std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|s| s.starts_with("VmHWM:"))
        .unwrap()
        .to_owned();
    let counts = crate::tape_log::measure::counts();
    let log_metrics = snapshot.replay_metrics();
    let frame_bytes = log_metrics.structural_bytes
        + log_metrics.payload_bytes
        + log_metrics.frames * FRAME_OVERHEAD as u64;
    eprintln!(
        "counted_passes full={:.6} structural={:.6}",
        counts.full_pass_equivalents(frame_bytes),
        counts.row_pass_equivalents(frame_bytes)
    );
    eprintln!("small_objects L={} S={} N={rows} M={members} P=0 frames={} counts={:?} capture_us={} peak_RSS={rss} elapsed_s={:.3}", snapshot.captured_end(), metrics.structural_bytes, metrics.frames, crate::tape_log::measure::counts(), snapshot.capture_time().as_micros(), started.elapsed().as_secs_f64());
}

#[test]
fn closed_initialized_is_valid_authority_but_never_a_fresh_name() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    drop(lease);
    // Ruling 19 permits a durable header-only prefix before a fresh supersede.
    // It is not a free filename: fresh creation still refuses it, while the
    // existing owner may append the barrier-proved initialization.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(store.path())
        .unwrap();
    file.set_len(51).unwrap();
    assert!(store.snapshot().unwrap().summary().is_fresh());
    assert!(matches!(
        store.acquire(true),
        Err(StoreError::AlreadyExists)
    ));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(store.path())
        .unwrap();
    encode_frame(&mut file, 1, &init(false), &[]).unwrap();
    let bytes = std::fs::read(store.path()).unwrap();
    let snapshot = store.snapshot().unwrap();
    assert!(!snapshot.summary().is_fresh());
    assert_eq!(snapshot.tail(), Tail::Complete);
    assert_eq!(std::fs::read(store.path()).unwrap(), bytes);
    assert!(matches!(
        store.acquire(true),
        Err(StoreError::AlreadyExists)
    ));
    assert!(store.acquire(false).is_ok());
    assert_eq!(std::fs::read(store.path()).unwrap(), bytes);
    let mut rows = 0;
    snapshot
        .visit_file_rows(|_| {
            rows += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(rows, 1);
}

#[path = "review_store_tests.rs"]
mod review;

#[test]
fn automatic_torn_tail_repair_refuses_below_projected_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let first_end = lease.summary().closed_end;
    append_object(&mut lease);
    let mark = lease.summary().last_closed_sequence;
    drop(lease);
    std::fs::OpenOptions::new()
        .write(true)
        .open(store.path())
        .unwrap()
        .set_len(first_end + 5)
        .unwrap();
    let before = std::fs::read(store.path()).unwrap();
    let mut lease = store.acquire(false).unwrap();
    assert!(matches!(
        lease.set_projection_floor(mark),
        Err(StoreError::BelowProjection { .. })
    ));
    let group = lease.prepare().unwrap();
    assert!(matches!(
        lease.stage(group),
        Err(StoreError::BelowProjection { .. })
    ));
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
}

#[test]
fn tail_inspection_preserves_complete_and_damaged_bytes() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let through = lease.summary().last_closed_sequence;
    drop(lease);
    let before = std::fs::read(store.path()).unwrap();
    let report = store.inspect_tail().unwrap();
    assert_eq!(report.through_sequence, through);
    assert!(report.damage.is_none());
    assert!(store.repair_tail(through, through).is_err());
    assert_eq!(std::fs::read(store.path()).unwrap(), before);
    let mut damaged = before.clone();
    damaged.extend_from_slice(&[0xff; 36]);
    std::fs::write(store.path(), &damaged).unwrap();
    let report = store.inspect_tail().unwrap();
    assert_eq!(report.through_sequence, through);
    assert_eq!(report.valid_bytes, before.len() as u64);
    assert!(report.damage.is_some());
    assert_eq!(std::fs::read(store.path()).unwrap(), damaged);
}

#[test]
fn native_writer_waits_for_startup_reader() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let reader = store.lock(false).unwrap();
    crate::tape_log::store::on_reader_contention(move || drop(reader));
    let mut writer = store.acquire(true).unwrap();
    append_object(&mut writer);
    drop(writer);
    assert_eq!(store.snapshot().unwrap().summary().object_count, 1);
}
#[test]
fn native_writer_bounds_reader_wait() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let _reader = store.lock(false).unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(store.acquire(true), Err(StoreError::Locked(_))));
    assert!(started.elapsed() >= std::time::Duration::from_millis(250));
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}
#[test]
fn native_writer_keeps_writer_conflict() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let _writer = store.acquire(true).unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(store.acquire(true), Err(StoreError::Locked(_))));
    assert!(started.elapsed() < std::time::Duration::from_millis(100));
}
#[test]
fn native_writer_refuses_writer_arriving_after_reader() {
    let root = tempfile::tempdir().unwrap();
    let store = store(root.path());
    let reader = store.lock(false).unwrap();
    let held = std::rc::Rc::new(std::cell::RefCell::new(None));
    let other = store.clone();
    let acquired = held.clone();
    crate::tape_log::store::on_reader_contention(move || {
        drop(reader);
        *acquired.borrow_mut() = Some(other.acquire(true).unwrap());
    });
    let started = std::time::Instant::now();
    assert!(matches!(store.acquire(true), Err(StoreError::Locked(_))));
    assert!(held.borrow().is_some());
    assert!(started.elapsed() < std::time::Duration::from_millis(100));
}

#[test]
fn post_barrier_prefix_and_component_use_only_one_closing_sync() {
    use crate::authority::{TapeAuthority, TapeAuthorityWriter};
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let id = [0x85; 16];
    let faults = Arc::new(Faults::default());
    let _registration = remanence_library::durable_file::test_support::register(
        root.path().into(),
        Arc::new(FaultDirectory(faults.clone())),
    );
    let (report, plan, _) = crate::authority::contract_tests::structural_fixture(id);
    let authority = TapeAuthority::open_fixture(root.path(), id).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    writer.publish_report(&(&report).into()).unwrap();
    let plan: crate::authority::AuthorityFinalization = (&plan).into();
    writer.begin_finalization(&plan).unwrap();
    writer.accept_preparation().unwrap();
    faults.events.lock().unwrap().clear();
    writer
        .publish_terminal_prefix(&plan.terminal_prefix.as_ref().unwrap().publication)
        .unwrap();
    let assert_syncs = || {
        let events = faults.events.lock().unwrap();
        assert_eq!(
            events.iter().filter(|s| *s == "fdatasync").count(),
            0,
            "{events:?}"
        );
        assert_eq!(
            events.iter().filter(|s| *s == "fsync").count(),
            1,
            "{events:?}"
        );
    };
    assert_syncs();
    faults.events.lock().unwrap().clear();
    writer
        .advance_finalization(
            crate::TerminalFinalizationProgress::BeforeReplicaA,
            crate::TerminalFinalizationProgress::AfterReplicaA,
        )
        .unwrap();
    assert_syncs();
}

#[test]
fn session_tail_snapshot_streams_only_the_last_closed_end() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let group = lease.prepare().unwrap();
    lease.append(group.close(&init(false)).unwrap()).unwrap();
    let open = SessionOpened {
        session_id: [9; 16],
        incarnation: 1,
        base_media_sequence: 1,
        cursor: SessionCursor {
            partition: 0,
            lba: 2,
            next_file: 1,
        },
        opened_at_utc: "2026-11-02T00:00:00Z".into(),
        drive: crate::tape_log::SessionDrive {
            serial: "DRV-TEST".into(),
            vendor: None,
            product: None,
            endpoint: crate::library_log::DriveKey::Standalone(crate::library_log::Standalone {
                drive_serial: "DRV-TEST".into(),
            }),
            drive_uuid: None,
        },
    };
    let group = lease.prepare().unwrap();
    lease
        .append(group.close(&Record::SessionOpened(open.clone())).unwrap())
        .unwrap();
    let before = lease.snapshot().unwrap();
    let mut group = lease.prepare().unwrap();
    let file = TailFile {
        tape_file_number: 1,
        identity: TailIdentity::Object("abandoned".into()),
        known_blocks: 3,
        synchronized_blocks: 3,
        unknown_blocks: 1,
        filemark: TailFilemark::Unknown,
        synchronized_filemark: false,
    };
    group
        .push(
            &Record::PayloadContinuation(Continuation {
                group: group.sequence(),
                chunk: Chunk::TailFile(file.clone()),
            }),
            &[],
        )
        .unwrap();
    let end = SessionEnded {
        session_id: open.session_id,
        incarnation: 1,
        base_media_sequence: 1,
        cursor: open.cursor,
        ended_at_utc: "2026-11-02T00:01:00Z".into(),
        reason: SessionEndReason::DriveError,
        tail: group.reference(0),
        tail_file_count: 1,
        crash_ack_audit_sequence: None,
        synchronized_tail_blocks: 3,
        possible_tail_blocks: 5,
        proved_eod: None,
    };
    lease
        .append(group.close(&Record::SessionEnded(end)).unwrap())
        .unwrap();
    let frozen = lease.snapshot().unwrap();
    before
        .visit_last_session_tail(|_| panic!("snapshot precedes the end"))
        .unwrap();
    let mut seen = Vec::new();
    frozen
        .visit_last_session_tail(|row| {
            seen.push(row.clone());
            Ok(())
        })
        .unwrap();
    assert_eq!(seen, [file]);
    let group = lease.prepare().unwrap();
    lease
        .append(group.close(&Record::SessionOpened(open)).unwrap())
        .unwrap();
    lease
        .snapshot()
        .unwrap()
        .visit_last_session_tail(|_| panic!("new session consumed last end"))
        .unwrap();
    let mut count = 0;
    frozen
        .visit_last_session_tail(|_| {
            count += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        count, 1,
        "frozen snapshot retains its own closed tail reference"
    );
    drop(lease);
    assert_eq!(
        store.snapshot().unwrap().summary().last_media_commit,
        Some(1)
    );
}

/// Library evidence uses complete bytes and only authoritative closing records.
#[test]
fn native_record_evidence_matches_bytes_and_excludes_continuations() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    assert!(lease
        .snapshot()
        .unwrap()
        .record_evidence(1)
        .unwrap()
        .is_none());
    append_object(&mut lease);
    let frozen = lease.snapshot().unwrap();
    let bytes = std::fs::read(store.path()).unwrap();
    let mut closers = 0;
    let mut continuations = 0;
    frozen
        .visit_frames(|frame| {
            let evidence = frozen.record_evidence(frame.sequence)?;
            if matches!(frame.record, Record::PayloadContinuation(_)) {
                assert!(evidence.is_none());
                continuations += 1;
            } else {
                let raw = &bytes[frame.offset as usize..frame.end as usize];
                let structural = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
                let payload = u32::from_le_bytes(raw[8..12].try_into().unwrap()) as usize;
                let mut digest = Sha256::new();
                digest.update(&raw[..20]);
                digest.update(&raw[28..28 + structural]);
                digest.update(&raw[36 + structural..36 + structural + payload]);
                let expected: [u8; 32] = digest.finalize().into();
                let evidence = evidence.unwrap();
                assert_eq!(evidence.kind, frame.record.tag());
                assert_eq!(evidence.digest, expected);
                closers += 1;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!((closers, continuations), (3, 2));
    assert!(frozen.record_evidence(0).unwrap().is_none());
    let next = frozen.summary().last_closed_sequence + 1;
    assert!(frozen.record_evidence(next).unwrap().is_none());
    append_object(&mut lease);
    assert!(frozen
        .record_evidence(lease.summary().last_closed_sequence)
        .unwrap()
        .is_none());
    assert!(lease
        .snapshot()
        .unwrap()
        .record_evidence(lease.summary().last_closed_sequence)
        .unwrap()
        .is_some());
}

/// Repair blocks appends but must leave its retained native proof readable.
#[test]
fn native_record_evidence_survives_torn_suffix_and_repair_fence() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    let frozen = lease.snapshot().unwrap();
    let through = frozen.summary().last_closed_sequence;
    let expected = frozen.record_evidence(through).unwrap().unwrap();
    drop(lease);
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.path())
        .unwrap()
        .write_all(&[0; 100])
        .unwrap();
    let torn = store.snapshot().unwrap();
    assert!(matches!(torn.tail(), Tail::Torn { .. }));
    assert_eq!(
        torn.record_evidence(through).unwrap(),
        Some(expected.clone())
    );
    assert!(torn.record_evidence(through + 1).unwrap().is_none());
    store.repair_tail(through, through).unwrap();
    assert!(store.path().with_extension("repair-required").exists());
    assert!(matches!(store.acquire(false), Err(StoreError::Refused(_))));
    assert_eq!(
        store.snapshot().unwrap().record_evidence(through).unwrap(),
        Some(expected.clone())
    );
    assert_eq!(frozen.record_evidence(through).unwrap(), Some(expected));
}

/// Post-capture read failures must not be reported as absent native evidence.
#[test]
fn native_record_evidence_propagates_changed_or_short_reads() {
    for corrupt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let mut lease = store.acquire(true).unwrap();
        append_object(&mut lease);
        let frozen = lease.snapshot().unwrap();
        let sequence = frozen.summary().last_closed_sequence;
        drop(lease);
        if corrupt {
            use std::os::unix::fs::FileExt;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(store.path())
                .unwrap();
            file.write_all_at(&[0xff], frozen.summary().closed_end - 1)
                .unwrap();
            assert!(matches!(
                frozen.record_evidence(sequence),
                Err(StoreError::Codec(_))
            ));
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .open(store.path())
                .unwrap()
                .set_len(0)
                .unwrap();
            assert!(
                matches!(frozen.record_evidence(sequence), Err(StoreError::Io(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof)
            );
        }
    }
}

/// Ruling 19 binds a supersede to a synced header without manufacturing a frame.
#[test]
fn empty_prefix_is_durable_and_keeps_its_digest_after_bootstrap() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    assert_eq!(lease.snapshot().unwrap().header_digest().unwrap(), None);
    lease.publish_empty_prefix().unwrap();
    let digest = lease.snapshot().unwrap().header_digest().unwrap().unwrap();
    assert_eq!(lease.summary().last_closed_sequence, 0);
    drop(lease);
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.header_digest().unwrap(), Some(digest));
    assert_eq!(snapshot.summary().last_closed_sequence, 0);
    let mut lease = store.acquire(false).unwrap();
    lease.publish_empty_prefix().unwrap();
    let group = lease.prepare().unwrap();
    lease.append(group.close(&init(false)).unwrap()).unwrap();
    drop(lease);
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.header_digest().unwrap(), Some(digest));
    assert_eq!(snapshot.summary().last_closed_sequence, 1);
    assert!(matches!(
        store.acquire(true),
        Err(StoreError::AlreadyExists)
    ));
}

/// Publish roughly one MiB in a real streamed group, with 64 KiB catalog
/// chunks. The fixed sizes are independent of the position index implementation.
fn position_group(lease: &mut Lease) -> (u64, u64, u64, u64) {
    if lease.summary().is_fresh() {
        let group = lease.prepare().unwrap();
        lease.stage(group).unwrap().complete(&init(false)).unwrap();
    }
    open_fixture_session(lease);
    let prior = lease.summary().clone();
    let mut group = lease.prepare().unwrap();
    let first = group.sequence();
    let start = prior.closed_end;
    let row = object(prior.next_file, prior.cursor.lba, None, 3);
    group
        .push(
            &Record::PayloadContinuation(Continuation {
                group: first,
                chunk: Chunk::FileRow(row.clone()),
            }),
            &[],
        )
        .unwrap();
    let h = header(&row, row.tape_file_number * 3);
    let mut payload = Vec::new();
    encode_header(&mut payload, &h).unwrap();
    let mut item = member();
    item.path = "p".repeat(1024 * 1024);
    encode_member(&mut payload, &h, &item).unwrap();
    let chunks = payload.chunks(64 * 1024);
    let count = chunks.len();
    for (ordinal, bytes) in chunks.enumerate() {
        group
            .push(
                &Record::PayloadContinuation(Continuation {
                    group: first,
                    chunk: Chunk::Catalog {
                        tape_file_number: row.tape_file_number,
                        ordinal: ordinal as u64,
                        last: ordinal + 1 == count,
                    },
                }),
                bytes,
            )
            .unwrap();
    }
    let closer = Record::Committed(commit(
        group.reference(1),
        row.tape_file_number + 1,
        prior.cursor.lba + 4,
        prior.object_count + 1,
    ));
    lease.stage(group).unwrap().complete(&closer).unwrap();
    (
        first,
        start,
        lease.summary().last_closed_sequence,
        lease.summary().closed_end,
    )
}

/// Check fixture geometry and return the largest actual frame span.
fn position_largest(snapshot: &crate::tape_log::snapshot::Snapshot) -> u64 {
    let mut largest = 0;
    let mut start = None;
    snapshot
        .visit_frames(|frame| {
            start.get_or_insert(frame.offset);
            largest = largest.max(frame.end - frame.offset);
            Ok(())
        })
        .unwrap();
    assert!(snapshot.summary().closed_end - start.unwrap() >= 4 * 1024 * 1024);
    assert!(largest < 1024 * 1024 / 2);
    largest
}

/// Match evidence and error variants/kinds, without comparing rendered text.
fn position_compare(snapshot: &crate::tape_log::snapshot::Snapshot, sequence: u64) {
    let indexed = snapshot.record_evidence(sequence);
    let replayed = snapshot.record_evidence_oracle(sequence);
    match (indexed, replayed) {
        (Ok(actual), Ok(expected)) => assert_eq!(actual, expected, "sequence {sequence}"),
        (Err(StoreError::Io(actual)), Err(StoreError::Io(expected))) => {
            assert_eq!(actual.kind(), expected.kind(), "sequence {sequence}");
        }
        (
            Err(StoreError::Codec(CodecError::Io(actual))),
            Err(StoreError::Codec(CodecError::Io(expected))),
        ) => {
            assert_eq!(actual.kind(), expected.kind(), "sequence {sequence}");
        }
        (actual, expected) => {
            panic!("sequence {sequence}: indexed {actual:?}, oracle {expected:?}")
        }
    }
}

/// T1: a narrowed group shares bookmarks on both sides of its bounds; neither
/// side may change the original lookup's answer, including its EOF errors.
#[test]
fn position_index_matches_replay_for_frozen_prefixes() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let groups: Vec<_> = (0..4).map(|_| position_group(&mut lease)).collect();
    let captured = lease.snapshot().unwrap();
    position_largest(&captured);
    let last = captured.summary().last_closed_sequence;
    position_compare(&captured, last);
    drop(lease);
    let validated = store.snapshot().unwrap();
    let before = validated.before(groups[2].0).unwrap();
    let (first, start, end_sequence, end) = groups[2];
    assert!(start - groups[0].1 >= 2 * 1024 * 1024);
    assert!(validated.summary().closed_end - end >= 1024 * 1024);
    let group = validated.group_snapshot(first, start, end);
    {
        let positions = group.position_index.lock().unwrap();
        assert!(positions.values().any(|&offset| offset < start));
        assert!(positions.values().any(|&offset| offset >= end));
    }
    // First group row: M1 must fail here if a bookmark below start is accepted.
    position_compare(&group, first - 1);
    for snapshot in [&captured, &validated, &before, &group] {
        let mut sequences: std::collections::BTreeSet<_> = (0..=last + 1).collect();
        sequences.extend([1, first - 1, first, end_sequence + 1]);
        for &entry in snapshot.position_index.lock().unwrap().keys() {
            sequences.extend([entry - 1, entry]);
        }
        assert!(sequences.len() >= 40);
        for sequence in sequences {
            position_compare(snapshot, sequence);
        }
    }
}

/// T2(a): validation seeds the very first lookup, independently of warm readers.
#[test]
fn position_index_bounds_first_validated_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    for _ in 0..4 {
        position_group(&mut lease);
    }
    let largest = position_largest(&lease.snapshot().unwrap());
    drop(lease);
    let snapshot = store.snapshot().unwrap();
    crate::tape_log::measure::reset();
    assert!(snapshot
        .record_evidence(snapshot.summary().last_closed_sequence)
        .unwrap()
        .is_some());
    let bytes = crate::tape_log::measure::counts().full_frame_bytes;
    assert!(
        bytes <= 1024 * 1024 + largest,
        "T2(a) first validated lookup: {bytes} bytes, largest {largest}"
    );
}

/// T4: validated and owner captures preserve the captured descriptor's identity
/// on clones and narrowed prefixes; an absent log has no file identity.
#[test]
fn position_index_preserves_captured_file_identity() {
    use std::os::unix::fs::MetadataExt;
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    assert_eq!(store.snapshot().unwrap().file_identity().unwrap(), None);
    let mut lease = store.acquire(true).unwrap();
    assert_eq!(lease.snapshot().unwrap().file_identity().unwrap(), None);
    let (first, start, _, end) = position_group(&mut lease);
    let pinned = std::fs::File::open(store.path()).unwrap();
    let metadata = pinned.metadata().unwrap();
    let identity = Some((metadata.dev(), metadata.ino()));
    let owned = lease.snapshot().unwrap();
    assert_eq!(
        owned.file_identity().unwrap(),
        identity,
        "T4 lease snapshot"
    );
    drop(lease);
    let validated = store.snapshot().unwrap();
    assert_eq!(
        validated.file_identity().unwrap(),
        identity,
        "T4 validated snapshot"
    );
    for snapshot in [owned, validated] {
        for narrowed in [
            snapshot.clone(),
            snapshot.before(first).unwrap(),
            snapshot.group_snapshot(first, start, end),
        ] {
            assert_eq!(narrowed.file_identity().unwrap(), identity);
        }
    }
}

/// T2(b): a second owner inherits validation bookmarks and extends them while
/// publishing, so its first lookup of the new end is still bounded.
#[test]
fn position_index_bounds_first_grown_lease_lookup() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    for _ in 0..4 {
        position_group(&mut lease);
    }
    drop(lease);
    let mut second = store.acquire(false).unwrap();
    let previous_end = second.summary().closed_end;
    for _ in 0..2 {
        position_group(&mut second);
    }
    assert!(second.summary().closed_end - previous_end >= 2 * 1024 * 1024);
    let snapshot = second.snapshot().unwrap();
    let largest = position_largest(&snapshot);
    crate::tape_log::measure::reset();
    assert!(snapshot
        .record_evidence(snapshot.summary().last_closed_sequence)
        .unwrap()
        .is_some());
    let bytes = crate::tape_log::measure::counts().full_frame_bytes;
    assert!(
        bytes <= 1024 * 1024 + largest,
        "T2(b) first grown lease lookup: {bytes} bytes, largest {largest}"
    );
}

/// T3: replacing a torn suffix resets only the owner's bookmarks. The old
/// snapshot still pins the old inode; a lookup rebuilds the new file's index.
#[test]
fn position_index_resets_after_torn_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let groups: Vec<_> = (0..4).map(|_| position_group(&mut lease)).collect();
    let largest = position_largest(&lease.snapshot().unwrap());
    drop(lease);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(store.path())
        .unwrap();
    std::io::Seek::seek(&mut file, std::io::SeekFrom::End(0)).unwrap();
    file.write_all(&[0; 100]).unwrap();
    drop(file);
    let mut lease = store.acquire(false).unwrap();
    let old = lease.snapshot().unwrap();
    position_group(&mut lease);
    let new = lease.snapshot().unwrap();
    assert_ne!(old.file_identity().unwrap(), new.file_identity().unwrap());
    let sequence = groups[2].0;
    assert!(groups[2].1 - groups[0].1 >= 2 * 1024 * 1024);
    crate::tape_log::measure::reset();
    let first = new.record_evidence(sequence).unwrap();
    let bytes = crate::tape_log::measure::counts().full_frame_bytes;
    assert!(
        bytes > 1024 * 1024 + largest,
        "T3 first lookup more than one interval plus the largest frame: {bytes}, largest {largest}"
    );
    crate::tape_log::measure::reset();
    let second = new.record_evidence(sequence).unwrap();
    let bytes = crate::tape_log::measure::counts().full_frame_bytes;
    assert!(
        bytes <= 1024 * 1024 + largest,
        "T3 second lookup bounded: {bytes}, largest {largest}"
    );
    assert_eq!(first, second);
    assert_eq!(first, old.record_evidence(sequence).unwrap());
}
