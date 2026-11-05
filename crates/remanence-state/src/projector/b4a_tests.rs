//! Reentrancy and unwind tests at the private core boundary.
use super::*;

fn provision(index: &mut CatalogIndex) {
    index
        .provision_tape(index::ProvisionTapeInput {
            tape_uuid: [1; 16],
            voltag: "B4A001L9".into(),
            block_size: 4096,
            parity: remanence_parity::ParityConfig::None,
            force: false,
        })
        .unwrap();
}

/// A transactional callback cannot recursively lock another handle of this catalog.
#[test]
fn b4a_nested_catalog_call_in_callback_is_refused() {
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let client = open(":memory:").unwrap();
        let mut outer = client.write_handle();
        provision(&mut outer);
        let inner = client.write_handle();
        let before = inner
            .get_tape_assignment_snapshot(&[1; 16])
            .unwrap()
            .unwrap();
        let result = outer.compare_and_set_tape_pool_assignment_with_audit(
            [1; 16],
            before.assignment_generation,
            None,
            "b4a",
            |_| inner.schema_version().map(|_| ()),
        );
        assert!(matches!(result, Err(StateError::CatalogReentered)));
        assert_eq!(
            inner
                .get_tape_assignment_snapshot(&[1; 16])
                .unwrap()
                .unwrap(),
            before
        );
        outer
            .compare_and_set_tape_pool_assignment_with_audit(
                [1; 16],
                before.assignment_generation,
                None,
                "b4a",
                |_| {
                    assert!(matches!(
                        inner.schema_version(),
                        Err(StateError::CatalogReentered)
                    ));
                    Ok(())
                },
            )
            .unwrap();
        send.send(()).unwrap();
    });
    receive
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("no recursive deadlock");
    worker.join().unwrap();
}

/// Unwinding rolls back the transaction before a recovered mutex is reused.
#[test]
fn b4a_panicking_method_leaves_the_catalog_usable() {
    let client = open(":memory:").unwrap();
    let writer = client.write_handle();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = writer.with_core(|core| core.b4a_panic_in_transaction());
    }))
    .expect_err("test hook must panic after the row insert");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"b4a transaction unwind")
    );
    let mut other = client.write_handle();
    assert!(other.list_tape_pools().unwrap().is_empty());
    provision(&mut other);
    assert!(other.get_tape(&[1; 16]).unwrap().is_some());
}

/// In-memory owners never share a registry entry, and clients cross thread boundaries.
#[test]
fn b4a_memory_clients_are_private_and_send_sync() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<ProjectionClient>();
    let a = open(":memory:").unwrap();
    let b = open(":memory:").unwrap();
    provision(&mut a.write_handle());
    assert!(b.write_handle().get_tape(&[1; 16]).unwrap().is_none());
}

/// A second in-process writer waits beyond SQLite's timeout; independent readers progress.
#[test]
fn b4a_writer_waits_without_busy_and_reader_keeps_wal_concurrency() {
    use std::sync::mpsc::{channel, RecvTimeoutError};
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.sqlite");
    let client = open(&path).unwrap();
    provision(&mut client.write_handle());
    let reader = CatalogReader::open(&path).unwrap();
    let (entered_tx, entered_rx) = channel();
    let (release_tx, release_rx) = channel();
    let mut owner = client.write_handle();
    let generation = owner
        .get_tape_assignment_snapshot(&[1; 16])
        .unwrap()
        .unwrap()
        .assignment_generation;
    let transaction = std::thread::spawn(move || {
        owner
            .compare_and_set_tape_pool_assignment_with_audit(
                [1; 16],
                generation,
                None,
                "b4a",
                |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                },
            )
            .unwrap();
    });
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(
        reader.schema_version().unwrap(),
        super::index::SCHEMA_VERSION
    );
    let mut waiting = client.write_handle();
    let (started_tx, started_rx) = channel();
    let (done_tx, done_rx) = channel();
    let waiter = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let started = Instant::now();
        waiting.set_drive_fenced(&[0; 16], true).unwrap();
        done_tx.send(started.elapsed()).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let writer_read = client.write_handle();
    let (read_started_tx, read_started_rx) = channel();
    let (read_done_tx, read_done_rx) = channel();
    let reading_writer = std::thread::spawn(move || {
        read_started_tx.send(()).unwrap();
        read_done_tx
            .send(writer_read.schema_version().unwrap())
            .unwrap();
    });
    read_started_rx
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert_eq!(
        done_rx.recv_timeout(Duration::from_millis(5200)),
        Err(RecvTimeoutError::Timeout)
    );
    assert!(reader.list_tape_pools().unwrap().is_empty());
    assert_eq!(
        read_done_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    );
    release_tx.send(()).unwrap();
    let elapsed = done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    eprintln!("B4a serialized writer waited {elapsed:?}; reader progressed during the transaction");
    assert_eq!(
        read_done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
        index::SCHEMA_VERSION
    );
    reading_writer.join().unwrap();
    waiter.join().unwrap();
    transaction.join().unwrap();
}

/// Reset refuses surviving writers before changing their journal mode or replacing their file.
#[test]
fn b4a_reset_refuses_live_shared_connection() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.sqlite");
    let client = open(&path).unwrap();
    let writer = client.write_handle();
    let version = writer.schema_version().unwrap();
    drop(client);
    let result = prepare_admitted_catalog_reset_source_atomic_swap(&path, version);
    assert!(
        matches!(result, Err(StateError::ConfigInvalid(_))),
        "{result:?}"
    );
    let journal_mode = || {
        rusqlite::Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .unwrap()
    };
    assert_eq!(journal_mode(), "wal");
    assert_eq!(writer.quick_check().unwrap(), "ok");
    drop(writer);
    prepare_admitted_catalog_reset_source_atomic_swap(&path, version).unwrap();
    assert_eq!(journal_mode(), "delete");
}
