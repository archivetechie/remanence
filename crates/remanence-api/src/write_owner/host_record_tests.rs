//! Every authoritative closer loses its response on each side of fsync. The
//! chaos filesystem independently keeps or discards its dirty append bytes.
use super::*;
use remanence_state::tape_log::{
    codec::Decoder,
    store::{Header, PublicationObserver, Store},
    Record as LogRecord,
};

#[test]
fn every_authoritative_record_unknown_append_stops_publication() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/tape-log");
    let mut logs: Vec<_> = [
        ("parity-off-two-objects.remtape", [0xa1; 16]),
        ("parity-object-session-close.remtape", [0xa2; 16]),
        ("sealed-terminal.remtape", [0xa3; 16]),
    ]
    .into_iter()
    .map(|(name, uuid)| (uuid, std::fs::read(fixtures.join(name)).unwrap()))
    .collect();
    // Derive manual preparation/retirement/recovery from the exact immutable
    // terminal fixture, using the real native publishers rather than raw tags.
    let bytes = logs.last().unwrap().1.clone();
    let mut input = std::io::Cursor::new(&bytes);
    let header = Header::decode(&mut input, [0xa3; 16]).unwrap();
    let mut decoder = Decoder::at(&bytes[input.position() as usize..], 1, input.position());
    let mut group_start = input.position() as usize;
    let mut decision = None;
    while let Some(frame) = decoder.next_frame().unwrap() {
        if matches!(frame.record, LogRecord::FinalizationDecided(_)) {
            decision = Some((group_start, frame.end as usize));
            break;
        }
        if !matches!(frame.record, LogRecord::PayloadContinuation(_)) {
            group_start = frame.end as usize;
        }
    }
    let (before, end) = decision.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let source = Store::new_fixture(dir.path(), header.clone()).unwrap();
    std::fs::write(source.path(), &bytes[..end]).unwrap();
    let authority = TapeAuthority::open_fixture(dir.path(), header.tape_uuid).unwrap();
    let mut plan = authority.pending_finalization().unwrap().unwrap();
    plan.trigger = TerminalFinalizationTrigger::OperatorCloseOut;
    plan.manual = Some(
        remanence_parity::authority::ManualTerminalFinalizationIdentity {
            operation_id: [0x91; 16],
            operation_kind: "finalize_tape".into(),
            actor_fingerprint: "test".into(),
            idempotency_key: [0x92; 16],
            request_fingerprint: [0x93; 32],
            assigned_pool_id: None,
            expected_pool_id: None,
            assignment_generation: 0,
            reason: "manual crash coverage".into(),
        },
    );
    for retire in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new_fixture(dir.path(), header.clone()).unwrap();
        std::fs::write(store.path(), &bytes[..before]).unwrap();
        let authority = TapeAuthority::open_fixture(dir.path(), header.tape_uuid).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        writer.begin_finalization(&plan).unwrap();
        if retire {
            writer.retire_preparation(&plan, false, false).unwrap();
        } else {
            writer.accept_preparation().unwrap();
            writer.require_finalization_recovery().unwrap();
        }
        drop(writer);
        logs.push((header.tape_uuid, std::fs::read(store.path()).unwrap()));
    }
    let mut covered = BTreeSet::new();
    for (uuid, bytes) in logs {
        let mut input = std::io::Cursor::new(&bytes);
        let header = Header::decode(&mut input, uuid).unwrap();
        let mut decoder = Decoder::at(&bytes[input.position() as usize..], 1, input.position());
        let mut start = input.position() as usize;
        let mut frames = Vec::new();
        while let Some(frame) = decoder.next_frame().unwrap() {
            let tag = frame.record.tag() as u16;
            if matches!(frame.record, LogRecord::PayloadContinuation(_)) {
                frames.push((frame.record, frame.payload.to_vec()));
                continue;
            }
            for after in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let store = Store::new_fixture(dir.path(), header.clone()).unwrap();
                let files = MemoryDirectory::default();
                if start > input.position() as usize {
                    std::fs::write(store.path(), &bytes[..start]).unwrap();
                    files.seed(store.path().to_path_buf(), bytes[..start].to_vec());
                }
                let _registration = register(dir.path().to_owned(), Arc::new(files.clone()));
                let mut lease = store.acquire(start == input.position() as usize).unwrap();
                let mut group = lease.prepare().unwrap();
                for (record, payload) in &frames {
                    group.push(record, payload).unwrap();
                }
                lease.set_publication_observer(PublicationObserver(Arc::new(
                    move |_, _, completed| {
                        if completed == after {
                            Err(std::io::Error::other("completed but response lost"))
                        } else {
                            Ok(())
                        }
                    },
                )));
                assert!(
                    lease.append(group.close(&frame.record).unwrap()).is_err(),
                    "tag {tag} after={after}"
                );
                assert!(lease.prepare().is_err(), "tag {tag} poisoned owner");
                let snapshot = files.snapshot();
                for keep_dirty in [false, true] {
                    let tails = snapshot
                        .files()
                        .map(|(path, file)| {
                            (
                                path.to_owned(),
                                if keep_dirty {
                                    file.written.len() - file.durable.len()
                                } else {
                                    0
                                },
                            )
                        })
                        .collect();
                    let mut crash = snapshot.clone();
                    crash.crash(&tails, true).unwrap();
                    let destination = tempfile::tempdir().unwrap();
                    crash.materialize(dir.path(), destination.path()).unwrap();
                    let path = destination.path().join(store.path().file_name().unwrap());
                    if after {
                        assert_eq!(std::fs::read(path).unwrap(), bytes[..frame.end as usize]);
                    } else if path.exists() {
                        let observed = std::fs::read(path).unwrap();
                        assert!(bytes[..frame.end as usize].starts_with(&observed));
                    }
                }
                covered.insert(tag);
            }
            frames.clear();
            start = frame.end as usize;
        }
    }
    assert_eq!(
        covered,
        BTreeSet::from([1, 3, 4, 5, 6, 7, 8, 9, 10, 17, 18, 19])
    );
}
