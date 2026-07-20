//! Live Layer 5 operation registry.
//!
//! Durable operation history is projected from the Layer 4 audit log. This
//! module holds only process-local state that cannot be rebuilt from SQLite:
//! progress replay, watch streams, and cancellation tokens for operations that
//! are currently owned by this daemon process.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic::Status;
use uuid::Uuid;

use crate::pb;

const RING_CAP: usize = 256;
const BROADCAST_CAP: usize = 256;
const MAX_RETAINED_OPS: usize = 1024;

pub(crate) type OperationStatusStream =
    Pin<Box<dyn Stream<Item = Result<pb::OperationStatus, Status>> + Send + 'static>>;

struct OpEntry {
    ring: VecDeque<pb::OperationStatus>,
    tx: broadcast::Sender<pb::OperationStatus>,
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Default)]
pub(crate) struct OperationRegistry {
    ops: Arc<Mutex<HashMap<Uuid, OpEntry>>>,
}

#[derive(Clone)]
pub(crate) struct OperationHandle {
    op_id: Uuid,
    operation_kind: String,
    cancel: Arc<AtomicBool>,
    ops: Arc<Mutex<HashMap<Uuid, OpEntry>>>,
}

impl OperationRegistry {
    pub(crate) fn register(&self, op_id: Uuid, kind: &str) -> OperationHandle {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAP);
        let cancel = Arc::new(AtomicBool::new(false));
        let kind = kind.trim().to_string();
        let operation_kind = if kind.is_empty() {
            "unknown".to_string()
        } else {
            kind
        };
        let mut ops = self.ops.lock().expect("ops lock");
        prune_terminal_ops_locked(&mut ops);
        ops.insert(
            op_id,
            OpEntry {
                ring: VecDeque::new(),
                tx: tx.clone(),
                cancel: cancel.clone(),
            },
        );
        drop(ops);
        OperationHandle {
            op_id,
            operation_kind,
            cancel,
            ops: self.ops.clone(),
        }
    }

    pub(crate) fn request_cancel(&self, op_id: &Uuid) -> Result<pb::OperationState, Status> {
        let mut ops = self.ops.lock().expect("ops lock");
        let entry = ops
            .get_mut(op_id)
            .ok_or_else(|| Status::not_found("operation not found"))?;
        if let Some(last) = entry.ring.back() {
            let current =
                pb::OperationState::try_from(last.state).unwrap_or(pb::OperationState::Unspecified);
            if is_terminal_state(current) {
                return Ok(current);
            }
        }
        entry.cancel.store(true, Ordering::SeqCst);
        Ok(pb::OperationState::Running)
    }

    pub(crate) fn watch(&self, op_id: &Uuid) -> Result<OperationStatusStream, Status> {
        let ops = self.ops.lock().expect("ops lock");
        let entry = ops
            .get(op_id)
            .ok_or_else(|| Status::not_found("operation not found"))?;
        let snapshot = entry.ring.iter().cloned().collect::<Vec<_>>();
        let rx = entry.tx.subscribe();
        drop(ops);
        Ok(build_watch_stream(snapshot, rx))
    }

    #[cfg(test)]
    fn len_for_tests(&self) -> usize {
        self.ops.lock().expect("ops lock").len()
    }
}

impl OperationHandle {
    pub(crate) fn op_id_uuid(&self) -> Uuid {
        self.op_id
    }

    pub(crate) fn operation_kind(&self) -> &str {
        self.operation_kind.as_str()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub(crate) fn publish(&self, status: pb::OperationStatus) {
        if let Ok(mut ops) = self.ops.lock() {
            if let Some(entry) = ops.get_mut(&self.op_id) {
                push_locked(entry, status);
            }
        }
    }

    pub(crate) fn publish_state(&self, state: pb::OperationState, progress: &[(&str, &str)]) {
        self.publish(status(
            self.op_id,
            self.operation_kind.as_str(),
            state,
            progress,
        ));
    }

    pub(crate) fn publish_failed(&self, error_summary: &str, progress: &[(&str, &str)]) {
        let progress = progress
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        self.publish(status_with_error(
            self.op_id,
            self.operation_kind.as_str(),
            pb::OperationState::Failed,
            progress,
            error_summary,
        ));
    }
}

pub(crate) fn status(
    id: Uuid,
    kind: &str,
    state: pb::OperationState,
    progress: &[(&str, &str)],
) -> pb::OperationStatus {
    let progress = progress
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    status_with_error(id, kind, state, progress, "")
}

pub(crate) fn matches_filter(
    record_kind: &str,
    record_state: &str,
    started_at_utc: &str,
    filter: &HashMap<String, String>,
) -> bool {
    for (key, value) in filter {
        match key.as_str() {
            "kind" if record_kind != value => return false,
            "state" if !state_matches(record_state, value) => return false,
            "since" if !started_since(started_at_utc, value) => return false,
            _ => {}
        }
    }
    true
}

pub(crate) fn is_terminal(status: &pb::OperationStatus) -> bool {
    let state =
        pb::OperationState::try_from(status.state).unwrap_or(pb::OperationState::Unspecified);
    is_terminal_state(state)
}

fn build_watch_stream(
    snapshot: Vec<pb::OperationStatus>,
    mut rx: broadcast::Receiver<pb::OperationStatus>,
) -> OperationStatusStream {
    let (tx, out_rx) =
        tokio::sync::mpsc::channel::<Result<pb::OperationStatus, Status>>(BROADCAST_CAP);
    tokio::spawn(async move {
        for item in snapshot {
            let terminal = is_terminal(&item);
            if tx.send(Ok(item)).await.is_err() {
                return;
            }
            if terminal {
                return;
            }
        }
        loop {
            match rx.recv().await {
                Ok(item) => {
                    let terminal = is_terminal(&item);
                    if tx.send(Ok(item)).await.is_err() {
                        return;
                    }
                    if terminal {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
    Box::pin(ReceiverStream::new(out_rx))
}

fn push_locked(entry: &mut OpEntry, status: pb::OperationStatus) {
    if entry.ring.len() >= RING_CAP {
        entry.ring.pop_front();
    }
    entry.ring.push_back(status.clone());
    let _ = entry.tx.send(status);
}

fn prune_terminal_ops_locked(ops: &mut HashMap<Uuid, OpEntry>) {
    if ops.len() < MAX_RETAINED_OPS {
        return;
    }
    let mut terminal_ids = ops
        .iter()
        .filter_map(|(id, entry)| {
            entry
                .ring
                .back()
                .filter(|status| is_terminal(status))
                .map(|_| *id)
        })
        .collect::<Vec<_>>();
    terminal_ids.sort();
    for id in terminal_ids {
        if ops.len() < MAX_RETAINED_OPS {
            break;
        }
        ops.remove(&id);
    }
}

fn status_with_error(
    id: Uuid,
    kind: &str,
    state: pb::OperationState,
    progress: HashMap<String, String>,
    error_summary: &str,
) -> pb::OperationStatus {
    let now = timestamp_now();
    pb::OperationStatus {
        operation_id: id.as_bytes().to_vec(),
        operation_kind: kind.to_string(),
        state: state as i32,
        created_at: Some(now),
        updated_at: Some(now),
        progress,
        error_summary: error_summary.to_string(),
    }
}

fn timestamp_now() -> prost_types::Timestamp {
    let now = OffsetDateTime::now_utc();
    prost_types::Timestamp {
        seconds: now.unix_timestamp(),
        nanos: now.nanosecond() as i32,
    }
}

fn state_matches(record_state: &str, filter_state: &str) -> bool {
    let state = crate::operation_state(record_state);
    match filter_state.trim().to_ascii_lowercase().as_str() {
        "queued" => state == pb::OperationState::Queued,
        "running" => state == pb::OperationState::Running,
        "succeeded" | "success" | "finished" => state == pb::OperationState::Succeeded,
        "failed" => state == pb::OperationState::Failed,
        "cancelled" | "canceled" => state == pb::OperationState::Cancelled,
        "unknown" => state == pb::OperationState::CompletionUnknown,
        "unspecified" => state == pb::OperationState::Unspecified,
        other => record_state.eq_ignore_ascii_case(other),
    }
}

fn started_since(started_at_utc: &str, since_utc: &str) -> bool {
    let Ok(started) = OffsetDateTime::parse(started_at_utc, &Rfc3339) else {
        return false;
    };
    let Ok(since) = OffsetDateTime::parse(since_utc, &Rfc3339) else {
        return false;
    };
    started >= since
}

fn is_terminal_state(state: pb::OperationState) -> bool {
    matches!(
        state,
        pb::OperationState::Succeeded | pb::OperationState::Failed | pb::OperationState::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_publishes_replays_and_cancels() {
        let reg = OperationRegistry::default();
        let id = Uuid::from_u128(1);
        let handle = reg.register(id, "reconcile_tape");
        handle.publish(status(
            id,
            "reconcile_tape",
            pb::OperationState::Running,
            &[("scanned", "1")],
        ));
        let mut stream = reg.watch(&id).expect("watch");
        handle.publish(status(
            id,
            "reconcile_tape",
            pb::OperationState::Succeeded,
            &[],
        ));

        use tokio_stream::StreamExt as _;
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.state, pb::OperationState::Running as i32);
        let last = stream.next().await.unwrap().unwrap();
        assert_eq!(last.state, pb::OperationState::Succeeded as i32);
        assert!(stream.next().await.is_none(), "stream closes on terminal");

        let id2 = Uuid::from_u128(2);
        let h2 = reg.register(id2, "reconcile_tape");
        assert!(!h2.is_cancelled());
        reg.request_cancel(&id2).unwrap();
        assert!(h2.is_cancelled());
    }

    #[tokio::test]
    async fn registry_prunes_terminal_operations_at_retention_cap() {
        let reg = OperationRegistry::default();
        for raw in 0..MAX_RETAINED_OPS {
            let id = Uuid::from_u128(raw as u128 + 1);
            let handle = reg.register(id, "refresh_inventory");
            handle.publish(status(
                id,
                "refresh_inventory",
                pb::OperationState::Succeeded,
                &[],
            ));
        }

        let id = Uuid::from_u128(10_000);
        let _handle = reg.register(id, "refresh_inventory");

        assert_eq!(reg.len_for_tests(), MAX_RETAINED_OPS);
        assert!(reg.watch(&Uuid::from_u128(1)).is_err());
        assert!(reg.watch(&id).is_ok());
    }

    #[tokio::test]
    async fn registry_does_not_prune_running_operations() {
        let reg = OperationRegistry::default();
        for raw in 0..MAX_RETAINED_OPS {
            let id = Uuid::from_u128(raw as u128 + 1);
            let handle = reg.register(id, "refresh_inventory");
            handle.publish(status(
                id,
                "refresh_inventory",
                pb::OperationState::Running,
                &[],
            ));
        }

        let id = Uuid::from_u128(10_000);
        let _handle = reg.register(id, "refresh_inventory");

        assert_eq!(reg.len_for_tests(), MAX_RETAINED_OPS + 1);
        assert!(reg.watch(&Uuid::from_u128(1)).is_ok());
        assert!(reg.watch(&id).is_ok());
    }

    #[test]
    fn operation_filter_matches_kind_state_since() {
        fn f(pairs: &[(&str, &str)]) -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect()
        }

        assert!(matches_filter(
            "reconcile_tape",
            "running",
            "2026-06-03T10:00:00Z",
            &f(&[("kind", "reconcile_tape")])
        ));
        assert!(!matches_filter(
            "reconcile_tape",
            "running",
            "2026-06-03T10:00:00Z",
            &f(&[("state", "succeeded")])
        ));
        assert!(matches_filter(
            "reconcile_tape",
            "running",
            "2026-06-03T10:00:00Z",
            &f(&[("since", "2026-06-03T09:00:00Z")])
        ));
        assert!(!matches_filter(
            "reconcile_tape",
            "running",
            "2026-06-03T08:00:00Z",
            &f(&[("since", "2026-06-03T09:00:00Z")])
        ));
        assert!(matches_filter(
            "reconcile_tape",
            "running",
            "2026-06-03T10:00:00Z",
            &f(&[("unknown", "no-op")])
        ));
    }

    #[test]
    fn registry_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_send<T: Send>() {}

        assert_send_sync::<OperationRegistry>();
        assert_send::<OperationHandle>();
    }
}
