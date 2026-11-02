//! Ingress abandonment against virtual tape, retaining an earlier checkpoint and readback.

use super::*;
use crate::write_session_ingress::WriteSessionApi;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Notify the test only after the body has passed spool/ring backpressure.
struct StalledBody {
    messages: VecDeque<Result<pb::AppendObjectMessage, Status>>,
    waiting: Option<oneshot::Sender<()>>,
}

impl tokio_stream::Stream for StalledBody {
    type Item = Result<pb::AppendObjectMessage, Status>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(message) = self.messages.pop_front() {
            return Poll::Ready(Some(message));
        }
        if let Some(waiting) = self.waiting.take() {
            waiting.send(()).unwrap();
        }
        Poll::Pending
    }
}

/// Both paths abandon only the incomplete object; committed receipts still read exact bytes.
#[tokio::test]
async fn ingress_deadline_preserves_committed_receipt_and_readable_object() {
    check_ingress_ending(false).await;
}

/// Handler cancellation must wake overlap consumers and end serial owners too.
#[tokio::test]
async fn client_disconnect_preserves_committed_receipt_and_names_tail() {
    check_ingress_ending(true).await;
}

async fn check_ingress_ending(disconnect: bool) {
    for overlap in [false, true] {
        for mid_object in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let world = setup(root.path());
            // Use the no-parity writer to inspect the committed archive's
            // physical blocks as well as reading it through the session engine.
            let mut index = CatalogIndex::open(root.path().join("state.sqlite")).unwrap();
            index
                .provision_tape(ProvisionTapeInput {
                    tape_uuid: TAPE,
                    voltag: BARCODE.into(),
                    block_size: BLOCK,
                    parity: ParityConfig::None,
                    force: false,
                })
                .unwrap();
            index.project_tape_pool_membership(TAPE, POOL).unwrap();
            {
                let mut model = world.lock().unwrap();
                let Record::Block(block) = &mut model.tapes.get_mut(BARCODE).unwrap().records[0]
                else {
                    panic!("bootstrap block");
                };
                let mut bootstrap = parse_bootstrap_block(block).unwrap();
                bootstrap.scheme = None;
                bootstrap.no_parity_flag = true;
                write_bootstrap_block(&bootstrap, block).unwrap();
            }
            let selection = SelectedTape {
                parity_config: ParityConfig::None,
                ..selected()
            };
            let (tx, opened, thread) = open_writer_with_selection(
                root.path(),
                open_model_library(world.clone()),
                selection,
            )
            .await;
            let id = opened.unwrap();
            let payload = b"checkpointed before a later client stall".to_vec();
            append_actor_test_file(
                &tx,
                id,
                root.path().join("input"),
                "saved",
                "saved",
                &payload,
            )
            .await;
            let (reply, received) = oneshot::channel();
            tx.send(DriveCommand::Checkpoint {
                session_id: id,
                trigger: CheckpointTrigger::Explicit,
                expected_batch_id: None,
                reply: Some(reply),
            })
            .await
            .unwrap();
            let checkpoint = received.await.unwrap().unwrap();
            let receipt = checkpoint.committed_objects[0].clone();
            assert_eq!(
                receipt.append_commit_info.as_ref().unwrap().durability,
                pb::AppendDurability::Checkpointed as i32
            );
            let object_id = Uuid::from_slice(&receipt.object_id).unwrap().to_string();
            let index = CatalogIndex::open(root.path().join("state.sqlite")).unwrap();
            let saved = index.get_native_object(&object_id).unwrap().unwrap();
            let copies = index.find_native_object_copies(&object_id).unwrap();
            let before = world.lock().unwrap().tapes[BARCODE].records.clone();
            let (changer_tx, _changer_rx) = mpsc::channel(1);
            let pool = DrivePool::new_for_library(
                LIBRARY,
                changer_tx,
                HashMap::from([(BAY, tx.clone())]),
                Arc::new(HashMap::from([(BAY, AtomicBool::new(false))])),
            );
            pool.record_session(
                id,
                MountedSession {
                    bay: BAY,
                    library_serial: LIBRARY.into(),
                    barcode: Some(BARCODE.into()),
                    home_slot: None,
                    tape_uuid: TAPE,
                    drive_uuid: None,
                },
            );
            let mut state =
                crate::ApiState::new(CatalogIndex::open(root.path().join("state.sqlite")).unwrap());
            let spool = root.path().join("spool");
            std::fs::create_dir_all(&spool).unwrap();
            state.spool_dir = Some(Arc::new(spool.clone()));
            state.spool_budget_bytes = Some(8 * 1024 * 1024);
            state.drive_pool = Some(pool);
            state.append_ingress_idle_secs = 10;
            state.append_staging_mode = if overlap {
                AppendStagingMode::Overlap
            } else {
                AppendStagingMode::Serial
            };
            let slab = crate::append_ring::APPEND_RING_SLAB_BYTES;
            // Prefill enough bytes to flush the writer's normal 4 MiB batch
            // before the held final slab and the later client stall.
            state.append_ring_bytes = (6 * slab) as u64;
            state.append_overlap_min_bytes = slab as u64;
            let api = WriteSessionApi { state };
            let mut messages = VecDeque::from([Ok(pb::AppendObjectMessage {
                payload: Some(pb::append_object_message::Payload::Start(
                    pb::AppendObjectStart {
                        session_id: id.as_bytes().to_vec(),
                        caller_object_id: "abandoned".into(),
                        declared_size_bytes: Some((8 * slab) as u64),
                        expected_content_sha256: Some(vec![0; 32]),
                        source_replay_capability: pb::SourceReplayCapability::ReplayFromStart
                            as i32,
                        ..Default::default()
                    },
                )),
            })]);
            if mid_object {
                messages.push_back(Ok(pb::AppendObjectMessage {
                    payload: Some(pb::append_object_message::Payload::Chunk(
                        pb::AppendObjectChunk {
                            session_id: id.as_bytes().to_vec(),
                            data: vec![0x61; 6 * slab],
                        },
                    )),
                }));
            }
            let (waiting, ready) = oneshot::channel();
            let append = tokio::spawn(async move {
                api.append_object_stream(StalledBody {
                    messages,
                    waiting: Some(waiting),
                })
                .await
            });
            tokio::time::timeout(StdDuration::from_secs(5), ready)
                .await
                .expect("body reaches client wait after ring/spool backpressure")
                .unwrap();
            if overlap && mid_object {
                tokio::time::timeout(StdDuration::from_secs(5), async {
                    while world.lock().unwrap().tapes[BARCODE].records.len() <= before.len() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("overlap writer reaches tape before the client stalls");
            }
            if disconnect {
                append.abort();
                assert!(append.await.unwrap_err().is_cancelled());
                // Join without blocking the async cleanup task that sends DisconnectWrite.
                tokio::time::timeout(StdDuration::from_secs(5), async {
                    while !thread.is_finished() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("disconnected owner ends promptly");
            } else {
                tokio::time::pause();
                tokio::time::advance(StdDuration::from_secs(9)).await;
                assert!(!append.is_finished());
                tokio::time::advance(StdDuration::from_secs(1)).await;
                // Give the synchronous writer real time to unwind; the client timer has fired.
                tokio::time::resume();
                let status = tokio::time::timeout(StdDuration::from_secs(5), append)
                    .await
                    .expect("overlap producer and writer exit")
                    .unwrap()
                    .unwrap_err();
                assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
                assert_eq!(status.message(), format!("AppendIngressDeadline: session {id} received no append data for 10 seconds; the incomplete object was abandoned."));
            }
            assert_eq!(std::fs::read_dir(&spool).unwrap().count(), 0);
            let after = world.lock().unwrap().tapes[BARCODE].records.clone();
            assert_eq!(
                &after[..before.len()],
                before.as_slice(),
                "committed tape prefix is intact"
            );
            if !overlap || !mid_object {
                assert_eq!(after, before, "pre-tape abandonment leaves tape untouched");
            } else {
                assert!(
                    matches!(after.last(), Some(Record::Block(_))),
                    "deadline writes no abandonment filemark"
                );
            }
            assert_eq!(index.get_native_object(&object_id).unwrap().unwrap(), saved);
            assert_eq!(index.find_native_object_copies(&object_id).unwrap(), copies);
            assert_eq!(index.list_native_objects().unwrap().len(), 1);
            // A failed append definitively ends its owner; wait for its durable named end.
            thread.join().expect("failed append owner exits");
            let authority =
                TapeAuthority::open(root.path().join("journals/tapes"), TAPE, index.path())
                    .unwrap();
            let snapshot = authority.freeze().unwrap();
            let end = snapshot.tape_summary().last_session_end.as_ref().unwrap();
            assert_eq!(
                end.reason,
                if disconnect {
                    remanence_state::tape_log::SessionEndReason::ClientDisconnected
                } else {
                    remanence_state::tape_log::SessionEndReason::ClientStall
                }
            );
            assert!(snapshot.tape_summary().open_session.is_none());
            snapshot
                .visit_session_tail(&mut |file| {
                    assert_eq!(
                        file.filemark,
                        remanence_state::tape_log::TailFilemark::Absent
                    );
                    assert_eq!(file.unknown_blocks, 0);
                    assert!(matches!(
                        file.identity,
                        remanence_state::tape_log::TailIdentity::Object(_)
                    ));
                    Ok(())
                })
                .unwrap();
            assert_eq!(end.tail_file_count > 0, overlap && mid_object);

            assert_eq!(checkpoint.committed_objects, vec![receipt]);
            let copy = &copies[0];
            let start = copy.global_start_block.unwrap() as usize;
            let end = copy.global_end_block.unwrap() as usize;
            let blocks = after[start..end]
                .iter()
                .map(|record| {
                    let Record::Block(bytes) = record else {
                        panic!("committed archive contains an unexpected filemark");
                    };
                    bytes.clone()
                })
                .collect::<Vec<_>>();
            let restored = remanence_format::read_rem_tar_object(
                &mut VecBlockSource::new(blocks),
                BLOCK as usize,
                (end - start) as u64,
            )
            .unwrap();
            assert_eq!(restored.entry("saved").unwrap().data, payload);
            assert!(
                index.list_active_tape_io_fences().unwrap().is_empty(),
                "a durable live end keeps reads available"
            );
            read_receipts(
                root.path(),
                world,
                &[Receipt {
                    id: object_id,
                    payload,
                    start: 0,
                    records: vec![],
                }],
            )
            .await
            .unwrap();
        }
    }
}
