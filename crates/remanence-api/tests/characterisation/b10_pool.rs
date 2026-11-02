//! Two seated cartridges exercise pool ownership and the Catalog RPC without hardware.
use super::*;
use remanence_state::authority::TapeAuthorityView;

const SECOND: [u8; 16] = [0x36; 16];

/// Seat a second fresh cartridge in the same pool and model library.
fn two_tapes(parity: bool) -> Harness {
    let fixture = Harness::new(parity);
    let mut index = CatalogIndex::open(&fixture.config.index.sqlite_path).unwrap();
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: "archive".into(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .unwrap();
    let first = index.get_tape(&UUID).unwrap().unwrap();
    let parity = first.scheme_id.map_or(ParityConfig::None, |id| {
        ParityConfig::Scheme(remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_owned(id),
            data_blocks_per_stripe: first.data_blocks_per_stripe.unwrap() as u16,
            parity_blocks_per_stripe: first.parity_blocks_per_stripe.unwrap() as u16,
            stripes_per_neighborhood: first.stripes_per_neighborhood.unwrap(),
        })
    });
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: SECOND,
            voltag: "CHAR02L9".into(),
            block_size: BLOCK,
            parity: parity.clone(),
            force: false,
        })
        .unwrap();
    index
        .project_tape_pool_membership(SECOND, "archive")
        .unwrap();
    let bootstrap =
        remanence_api::build_tape_bootstrap(SECOND, BLOCK, parity, "2026-11-03T00:00:00Z", "b10");
    let mut block = vec![0; BLOCK as usize];
    write_bootstrap_block(&bootstrap, &mut block).unwrap();
    let mut tape = VirtualTape::empty(18_000_000_000_000, BLOCK);
    tape.records = vec![Record::Block(block), Record::Filemark];
    tape.written_bytes = u64::from(BLOCK);
    {
        let mut world = fixture.world.lock().unwrap();
        world
            .drive_serials
            .insert(BAY + 1, "CHARACTERISATION-SECOND".into());
        world.drive_states.insert(BAY + 1, Default::default());
        world.drive_alert_flags.insert(BAY + 1, Default::default());
        world.element_layout.drive_count = 2;
        world.put_tape_in_drive(BAY + 1, "CHAR02L9", Some(0x401), tape);
    }
    fixture
}

fn pool_request() -> pb::OpenWriteSessionRequest {
    pb::OpenWriteSessionRequest {
        target: Some(pb::open_write_session_request::Target::PoolTarget(
            pb::TapePoolTarget {
                pool_id: "archive".into(),
                mount_if_needed: true,
                ..Default::default()
            },
        )),
        ..Default::default()
    }
}
async fn open_pool(client: &Client) -> pb::WriteSession {
    tokio::time::timeout(
        Duration::from_secs(10),
        client.writer().open_write_session(pool_request()),
    )
    .await
    .unwrap()
    .unwrap()
    .into_inner()
}
async fn catalog_tape(client: &Client, tape: [u8; 16]) -> pb::Tape {
    pb::catalog_client::CatalogClient::new(client.channel.clone())
        .get_tape(pb::GetTapeRequest {
            tape_uuid: tape.to_vec(),
        })
        .await
        .unwrap()
        .into_inner()
}
fn authority(fixture: &Harness, tape: [u8; 16]) -> remanence_state::TapeAuthority {
    remanence_state::TapeAuthority::open(
        fixture.config.journal.dir.join("tapes"),
        tape,
        &fixture.config.index.sqlite_path,
    )
    .unwrap()
}

/// Three immediate pool sessions append files 1, 2, 3 on the first cartridge.
#[tokio::test]
async fn b10_three_pool_sessions_keep_the_first_seated_tape() {
    use remanence_chaos::file_model::{FileEvent, MemoryDirectory};
    use remanence_library::durable_file::test_support::register;
    let fixture = two_tapes(false);
    let files = MemoryDirectory::default();
    let _registration = register(
        fixture.config.journal.dir.join("tapes"),
        Arc::new(files.clone()),
    );
    let client = fixture.start().await;
    let mut session = open_pool(&client).await;
    let mut objects = Vec::new();
    for file in 1..=3 {
        assert_eq!(session.tape_uuid, Some(UUID.to_vec()));
        objects.push(
            client
                .append(&session, &format!("b10-{file}"), b"single object")
                .await,
        );
        client.checkpoint(&session).await;
        // Hold SessionEnded's fsync after the close receipt, making the short
        // no-parity handoff deterministic without adding a gap between opens.
        let target =
            remanence_state::authority::fixtures::publication_target(&authority(&fixture, UUID));
        let (entered, at_end) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let hold = Mutex::new(Some((entered, gate)));
        files.observe(Some(Arc::new(move |event| {
            if let FileEvent::BeforeSync(path) = event {
                if target.matches(&path) {
                    if let Some((entered, gate)) = hold.lock().unwrap().take() {
                        entered.send(()).unwrap();
                        gate.recv_timeout(Duration::from_secs(10)).unwrap();
                    }
                }
            }
        })));
        client.close(&session).await;
        tokio::time::timeout(Duration::from_secs(5), at_end)
            .await
            .unwrap()
            .unwrap();
        if file < 3 {
            let mut writer = client.writer();
            let mut following =
                tokio::spawn(async move { writer.open_write_session(pool_request()).await });
            let pending = tokio::time::timeout(Duration::from_millis(50), &mut following)
                .await
                .is_err();
            release.send(()).unwrap();
            assert!(pending, "next session waits for the first tape's ending");
            session = tokio::time::timeout(Duration::from_secs(10), following)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_inner();
        } else {
            release.send(()).unwrap();
        }
        files.observe(None);
    }
    client.state.shutdown_drive_pool().await.unwrap();
    for (i, object) in objects.into_iter().enumerate() {
        let stored = pb::catalog_client::CatalogClient::new(client.channel.clone())
            .get_object(pb::GetObjectRequest {
                key: Some(pb::get_object_request::Key::ObjectId(object.object_id)),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(stored.copies.len(), 1);
        assert_eq!(stored.copies[0].tape_uuid, UUID);
        assert_eq!(stored.copies[0].tape_file_number, i as u64 + 1);
    }
}

/// A checkpointed live writer remains excluded; the fresh second tape's first
/// checkpoint is already projected through the public Catalog RPC.
#[tokio::test]
async fn b10_live_writer_is_skipped_and_fresh_second_extent_matches_cursor() {
    let fixture = two_tapes(true);
    let client = fixture.start().await;
    let first = open_pool(&client).await;
    client.append(&first, "first-live", b"first").await;
    client.checkpoint(&first).await;
    let second = open_pool(&client).await;
    assert_eq!(second.tape_uuid, Some(SECOND.to_vec()));
    client.append(&second, "fresh-second", b"second").await;
    client.checkpoint(&second).await;
    let summary = authority(&fixture, SECOND).observe_summary().unwrap();
    assert!(
        summary.cursor.lba > 2,
        "first Object advances beyond bootstrap"
    );
    assert_eq!(
        catalog_tape(&client, SECOND).await.written_extent_lba,
        Some(summary.cursor.lba)
    );
    client.close(&first).await;
    client.close(&second).await;
    client.state.shutdown_drive_pool().await.unwrap();
    let summary = authority(&fixture, SECOND).freeze().unwrap();
    assert_eq!(
        catalog_tape(&client, SECOND).await.written_extent_lba,
        Some(summary.tape_summary().cursor.lba)
    );
}

#[derive(Clone, Default)]
struct LogBytes(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A real post-session closeout hold retains the chosen pool tape.
#[tokio::test]
async fn b10_pool_waits_for_closing() {
    check_pool_closeout(false).await;
}

/// A fence published during that hold forces a logged pass through selection.
#[tokio::test]
async fn b10_pool_reselects_when_closing_ends_fenced() {
    check_pool_closeout(true).await;
}

async fn check_pool_closeout(fence: bool) {
    let logs = LogBytes::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    {
        let fixture = two_tapes(true);
        let client = fixture.start().await;
        let session = open_pool(&client).await;
        client
            .append(&session, "held-pool", b"one short epoch")
            .await;
        client.checkpoint(&session).await;
        let checkpoint_cursor = authority(&fixture, UUID)
            .observe_summary()
            .unwrap()
            .cursor
            .lba;
        assert_eq!(
            catalog_tape(&client, UUID).await.written_extent_lba,
            Some(checkpoint_cursor)
        );
        let (entered, at_barrier) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        fixture.injection.lock().unwrap().held_barrier = Some((entered, gate));
        client.close(&session).await;
        tokio::time::timeout(Duration::from_secs(5), at_barrier)
            .await
            .unwrap()
            .unwrap();
        let mut writer = client.writer();
        let following =
            tokio::spawn(async move { writer.open_write_session(pool_request()).await });
        // Publish the fence only after selection has chosen the held tape.
        // A scheduler delay must not turn this into a pre-selection fence test.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waited = String::from_utf8_lossy(&logs.0.lock().unwrap())
                    .contains("pool selection waiting for tape closeout");
                if waited {
                    break;
                }
                assert!(!following.is_finished(), "pool open skipped the held tape");
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("selection reaches the closeout wait");
        assert!(
            !following.is_finished(),
            "pool open waits for the chosen tape"
        );
        if fence {
            CatalogIndex::open(&fixture.config.index.sqlite_path)
                .unwrap()
                .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                    tape_uuid: UUID,
                    barcode: None,
                    reason: "b10 hold ended fenced".into(),
                    evidence_json: None,
                })
                .unwrap();
        }
        release.send(()).unwrap();
        let next = tokio::time::timeout(Duration::from_secs(10), following)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_inner();
        assert_eq!(
            next.tape_uuid,
            Some(if fence { SECOND } else { UUID }.to_vec())
        );
        client.close(&next).await;
        client.state.shutdown_drive_pool().await.unwrap();
        let summary = authority(&fixture, UUID).freeze().unwrap();
        assert!(summary.tape_summary().cursor.lba > checkpoint_cursor);
        assert_eq!(
            catalog_tape(&client, UUID).await.written_extent_lba,
            Some(summary.tape_summary().cursor.lba)
        );
    }
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("INFO")
            && text.contains("pool selection waiting for tape closeout")
            && text.contains("closing"),
        "{text}"
    );
    if fence {
        assert!(
            text.contains("pool selection skipped tape after closeout; selecting again")
                && text.contains("b10 hold ended fenced"),
            "{text}"
        );
    }
}

/// Roll off a finalized first cartridge, then compare the second cartridge's
/// first committed cursor through Catalog before and after ordinary closeout.
#[tokio::test]
async fn b10_rolled_tape_first_checkpoint_projects_extent_through_catalog_rpc() {
    let fixture = two_tapes(true);
    let client = fixture.start().await;
    let first = open_pool(&client).await;
    client
        .append(&first, "before-roll", b"first cartridge")
        .await;
    client.close(&first).await;
    let mut catalog = pb::catalog_client::CatalogClient::new(client.channel.clone());
    let accepted = catalog
        .finalize_tape(pb::FinalizeTapeRequest {
            tape_uuid: UUID.to_vec(),
            expected_pool_id: Some("archive".into()),
            reason: "roll pool".into(),
            idempotency_key: Some(pb::IdempotencyKey {
                value: uuid::Uuid::new_v4().as_bytes().to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let op = pb::daemon_client::DaemonClient::new(client.channel.clone())
                .get_operation(pb::GetOperationRequest {
                    operation_id: accepted.operation_id.clone().unwrap(),
                })
                .await
                .unwrap()
                .into_inner();
            if op.state == pb::OperationState::Succeeded as i32 {
                break;
            }
            assert!(
                op.state == pb::OperationState::Running as i32
                    || op.state == pb::OperationState::Queued as i32,
                "{op:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let second = open_pool(&client).await;
    assert_eq!(second.tape_uuid, Some(SECOND.to_vec()));
    client
        .append(&second, "after-roll", b"fresh second cartridge")
        .await;
    client.checkpoint(&second).await;
    let cursor = authority(&fixture, SECOND)
        .observe_summary()
        .unwrap()
        .cursor
        .lba;
    assert!(cursor > 2);
    assert_eq!(
        catalog_tape(&client, SECOND).await.written_extent_lba,
        Some(cursor)
    );
    client.close(&second).await;
    client.state.shutdown_drive_pool().await.unwrap();
    let cursor = authority(&fixture, SECOND)
        .freeze()
        .unwrap()
        .tape_summary()
        .cursor
        .lba;
    assert_eq!(
        catalog_tape(&client, SECOND).await.written_extent_lba,
        Some(cursor)
    );
}
