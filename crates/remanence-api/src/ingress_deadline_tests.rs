//! Paused-clock coverage of client-only append ingress deadlines and ring wakeups.

use super::*;
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

const IDLE: u64 = 10;

/// Signal when ingress has processed every queued message and awaits the client.
struct StalledIngress {
    messages: VecDeque<Result<pb::AppendObjectMessage, tonic::Status>>,
    waiting: Option<oneshot::Sender<()>>,
}

impl tokio_stream::Stream for StalledIngress {
    type Item = Result<pb::AppendObjectMessage, tonic::Status>;

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

fn assert_deadline(status: &tonic::Status, session_id: Option<Uuid>) {
    assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
    let message = match session_id {
        Some(id) => format!(
            "AppendIngressDeadline: session {id} received no append data for {IDLE} seconds; the incomplete object was abandoned."
        ),
        None => format!("AppendIngressDeadline: no append message received for {IDLE} seconds."),
    };
    assert_eq!(status.message(), message);
}

/// All ingress entry points time out; staged spools and overlap reservations are released.
#[tokio::test(start_paused = true)]
async fn ingress_deadline_before_start_after_start_and_mid_object() {
    for mode in [
        remanence_state::AppendStagingMode::Serial,
        remanence_state::AppendStagingMode::Overlap,
    ] {
        for position in ["before_start", "after_start", "mid_object"] {
            let temp = tempfile::tempdir().unwrap();
            let slab = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
            let mut state = state_with_spool(temp.path().to_owned(), 2 * slab);
            state.append_staging_mode = mode;
            state.append_ingress_idle_secs = IDLE;
            state.append_ring_bytes = slab;
            state.append_overlap_min_bytes = slab;
            let memory = state.io_memory.clone();
            let id = Uuid::new_v4();
            let mut messages = VecDeque::new();
            if position != "before_start" {
                let mut start = append_start_message(id, 2 * slab);
                if let Some(pb::append_object_message::Payload::Start(start)) = &mut start.payload {
                    start.expected_content_sha256 = Some(vec![0; 32]);
                    start.source_replay_capability =
                        pb::SourceReplayCapability::ReplayFromStart as i32;
                }
                messages.push_back(Ok(start));
            }
            if position == "mid_object" {
                messages.push_back(Ok(append_chunk_message(id, b"partial".to_vec())));
            }
            let (waiting, ready) = oneshot::channel();
            let api = WriteSessionApi { state };
            let append = tokio::spawn(async move {
                api.append_object_stream(StalledIngress {
                    messages,
                    waiting: Some(waiting),
                })
                .await
            });
            ready.await.unwrap();
            if mode == remanence_state::AppendStagingMode::Serial && position != "before_start" {
                assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
            }
            tokio::time::advance(Duration::from_secs(IDLE - 1)).await;
            assert!(
                !append.is_finished(),
                "{mode:?} {position}: premature deadline"
            );
            tokio::time::advance(Duration::from_secs(1)).await;
            let status = tokio::time::timeout(Duration::from_secs(1), append)
                .await
                .expect("no ingress or prefill task left waiting")
                .unwrap()
                .unwrap_err();
            assert_deadline(&status, (position != "before_start").then_some(id));
            assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
            assert_eq!(memory.granted(), 0);
        }
    }
}

/// Every accepted progress message resets the budget, even over many idle periods.
#[tokio::test(start_paused = true)]
async fn ingress_deadline_slow_progress_and_finish_reset_the_budget() {
    let id = Uuid::new_v4();
    let (tx, rx) = mpsc::channel(1);
    let (seen, mut received) = mpsc::unbounded_channel();
    let receiver = tokio::spawn(async move {
        let mut stream = ReceiverStream::new(rx);
        let mut ingress = AppendIngressDeadline::new(IDLE);
        while let Some(message) = next_append_message(&mut stream, &mut ingress, Some(id)).await? {
            seen.send(message).unwrap();
        }
        Ok::<_, tonic::Status>(())
    });
    tokio::task::yield_now().await;
    let mut messages = vec![append_start_message(id, 5)];
    messages.extend((0..5).map(|_| append_chunk_message(id, vec![1])));
    messages.push(append_finish_message(id, [0; 32]));
    for message in messages {
        tokio::time::advance(Duration::from_secs(IDLE - 1)).await;
        tx.send(Ok(message.clone())).await.unwrap();
        assert_eq!(received.recv().await.unwrap(), message);
        assert!(!receiver.is_finished());
    }
    tokio::time::advance(Duration::from_secs(IDLE - 1)).await;
    drop(tx);
    receiver.await.unwrap().unwrap();
}

/// Empty chunks accumulate wait time; daemon work between receives consumes none of it.
#[tokio::test(start_paused = true)]
async fn ingress_deadline_empty_chunks_do_not_reset_and_daemon_work_is_excluded() {
    let id = Uuid::new_v4();
    let mut ingress = AppendIngressDeadline::new(IDLE);
    for _ in 0..2 {
        let mut stream = tokio_stream::once(Ok(append_chunk_message(id, Vec::new())));
        // Delay each empty message by four seconds inside the receive helper.
        let (tx, rx) = mpsc::channel(1);
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(4)).await;
            tx.send(stream.next().await.unwrap()).await.unwrap();
        });
        next_append_message(&mut ReceiverStream::new(rx), &mut ingress, Some(id))
            .await
            .unwrap();
        sender.await.unwrap();
        tokio::time::advance(Duration::from_secs(3 * IDLE)).await;
    }
    let started = tokio::time::Instant::now();
    let status = next_append_message(&mut tokio_stream::pending(), &mut ingress, Some(id))
        .await
        .unwrap_err();
    assert_eq!(started.elapsed(), Duration::from_secs(2));
    assert_deadline(&status, Some(id));
}

/// A full ring blocks daemon ingestion longer than the deadline, then completes normally.
#[tokio::test(start_paused = true)]
async fn ingress_deadline_excludes_full_ring_backpressure() {
    let id = Uuid::new_v4();
    let slab = crate::append_ring::APPEND_RING_SLAB_BYTES;
    let payload = vec![0x5a; 3 * slab];
    let digest: [u8; 32] = Sha256::digest(&payload).into();
    let manager = crate::io_memory::IoMemoryReservation::new(slab as u64).unwrap();
    let (producer, mut consumer, control) =
        crate::append_ring::create_append_ring(&manager, slab as u64, 90, 25, payload.len() as u64)
            .unwrap();
    let stream = tokio_stream::iter([
        Ok(append_chunk_message(id, payload.clone())),
        Ok(append_finish_message(id, digest)),
    ]);
    let receive = tokio::spawn(receive_overlap_messages(
        stream,
        producer,
        id,
        payload.len() as u64,
        digest,
        control.clone(),
        AppendIngressDeadline::new(IDLE),
    ));
    tokio::task::yield_now().await;
    assert_eq!(control.occupancy_bytes(), slab as u64);
    tokio::time::advance(Duration::from_secs(3 * IDLE)).await;
    assert!(!receive.is_finished());
    assert!(control.failure_message().is_none());
    let consumed = tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        consumer.read_to_end(&mut bytes).unwrap();
        bytes
    });
    assert_eq!(receive.await.unwrap().unwrap().bytes, payload.len() as u64);
    assert_eq!(consumed.await.unwrap(), payload);
    drop(control);
    assert_eq!(manager.granted(), 0);
}

/// Expiry wakes synchronous consumer/resume waits and joins the asynchronous producer.
#[tokio::test(start_paused = true)]
async fn ingress_deadline_wakes_overlap_consumer_and_resume_waiters() {
    let id = Uuid::new_v4();
    let slab = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(slab).unwrap();
    let (producer, mut consumer, control) =
        crate::append_ring::create_append_ring(&manager, slab, 90, 25, slab).unwrap();
    let resume_control = control.clone();
    let resume = tokio::task::spawn_blocking(move || resume_control.wait_for_resume());
    let consumer_task =
        tokio::task::spawn_blocking(move || consumer.read_to_end(&mut Vec::new()).unwrap_err());
    let receive = tokio::spawn(receive_overlap_messages(
        tokio_stream::pending(),
        producer,
        id,
        slab,
        [0; 32],
        control.clone(),
        AppendIngressDeadline::new(IDLE),
    ));
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(IDLE)).await;
    let status = receive.await.unwrap().unwrap_err();
    assert_deadline(&status, Some(id));
    assert_eq!(consumer_task.await.unwrap().to_string(), status.message());
    assert_eq!(
        resume.await.unwrap().unwrap_err().to_string(),
        status.message()
    );
    drop(control);
    assert_eq!(manager.granted(), 0);
}
