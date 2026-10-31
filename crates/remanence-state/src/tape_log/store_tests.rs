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
fn append_object(lease: &mut Lease) {
    let summary = lease.summary().clone();
    let mut group = lease.prepare().unwrap();
    if summary.is_fresh() {
        group.push(&init(false), &[]).unwrap();
    }
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
    let mut group = lease.prepare().unwrap();
    group.push(&init(false), &[]).unwrap();
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
            group.push(&init(true), &[]).unwrap();
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
    let mut group = lease.prepare().unwrap();
    group.push(&init(false), &[]).unwrap();
    let c = Record::Committed(commit(group.reference(0), 1, 2, 0));
    group.close(&c).unwrap()
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
            assert_eq!(events.iter().filter(|e| *e == "fdatasync").count(), 1);
            assert_eq!(events.iter().filter(|e| *e == "fsync").count(), 1);
            assert_eq!(events.iter().filter(|e| *e == "directory").count(), 1);
            assert_eq!(
                &events[events.len() - 5..],
                &["fdatasync", "append", "fsync", "rename", "directory"]
            );
            drop(events);
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
    assert_eq!(store.acquire(false).unwrap().summary().barrier_count, 2);
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
    assert!(matches!(store.acquire(false), Err(StoreError::Refused(_))));
    assert_eq!(
        store.snapshot().unwrap().summary().last_closed_sequence,
        through
    );
}
#[test]
fn unverified_cold_check_is_retained_in_disk_snapshot() {
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
    c.total = 3;
    c.check = CheckValue(7);
    c.provenance = CheckProvenance::UnverifiedRebuild;
    c.unverified_end = Some(3);
    let prepared = group.close(&Record::Committed(c)).unwrap();
    lease.append(prepared).unwrap();
    drop(lease);
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.summary().check, CheckValue(7));
    assert_eq!(
        snapshot.summary().provenance,
        CheckProvenance::UnverifiedRebuild
    );
    assert_eq!(snapshot.summary().unverified_end, Some(3));
}

#[test]
fn high_file_count_small_objects_stream_without_a_batch_vector() {
    crate::tape_log::measure::reset();
    let started = std::time::Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    let mut group = lease.prepare().unwrap();
    group.push(&init(false), &[]).unwrap();
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
fn unclosed_initialized_is_a_torn_group_but_never_a_fresh_name() {
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path());
    let mut lease = store.acquire(true).unwrap();
    append_object(&mut lease);
    drop(lease);
    // Construct the format-level interrupted initialization case. Atomic first
    // publication cannot produce it, but B1 still classifies this valid suffix.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(store.path())
        .unwrap();
    file.set_len(51).unwrap();
    assert!(
        store.snapshot().is_err(),
        "a clean header-only active file is invalid"
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(store.path())
        .unwrap();
    encode_frame(&mut file, 1, &init(false), &[]).unwrap();
    let bytes = std::fs::read(store.path()).unwrap();
    let snapshot = store.snapshot().unwrap();
    assert!(snapshot.summary().is_fresh());
    assert!(matches!(
        snapshot.tail(),
        Tail::Torn {
            kind: TornKind::UnclosedGroup { .. },
            ..
        }
    ));
    assert_eq!(std::fs::read(store.path()).unwrap(), bytes);
    assert!(matches!(
        store.acquire(true),
        Err(StoreError::AlreadyExists)
    ));
    assert!(matches!(store.acquire(false), Err(StoreError::Refused(_))));
    assert_eq!(std::fs::read(store.path()).unwrap(), bytes);
    let mut rows = 0;
    snapshot
        .visit_file_rows(|_| {
            rows += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(rows, 0);
}

#[path = "review_store_tests.rs"]
mod review;
