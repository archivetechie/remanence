//! Replay-derived parity debt, retry bookkeeping and operator notification.
//! The list is disposable: native logs remain authority across every failure.
use crate::{pb, ApiState};
use remanence_state::{tape_log::fold::TapeSummary, TapeAuthority};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tonic::Status;
use uuid::Uuid;

type Key = ([u8; 16], u64);

#[derive(Default)]
pub(crate) struct WorkList {
    entries: Mutex<HashMap<Key, Retry>>,
    alarmed: Mutex<HashSet<(Key, String)>>,
    started: AtomicBool,
    pub(crate) wake: tokio::sync::Notify,
}
struct Retry {
    attempt: usize,
    next: Instant,
    running: bool,
}
struct Attempt {
    work: std::sync::Arc<WorkList>,
    key: Key,
}
impl Drop for Attempt {
    fn drop(&mut self) {
        self.work.finish(self.key);
    }
}

/// One retry schedule for resource, projection, fence and dispatch failures.
fn backoff(attempt: usize) -> Duration {
    Duration::from_secs([1, 5, 30, 60][attempt.min(3)])
}
fn finalization_due(s: &TapeSummary) -> bool {
    !s.sealed && (s.seal_due.is_some() || s.finalization.as_ref().is_some_and(|p| !p.prepared))
}
impl WorkList {
    #[cfg(test)]
    pub(crate) fn contains(&self, key: Key) -> bool {
        self.entries.lock().unwrap().contains_key(&key)
    }
    fn observe(&self, key: Key, due: bool) {
        let mut entries = self.entries.lock().unwrap();
        if due {
            entries.entry(key).or_insert(Retry {
                attempt: 0,
                next: Instant::now(),
                running: false,
            });
        } else if entries.get(&key).is_some_and(|r| !r.running) {
            entries.remove(&key);
        }
    }
    fn claim(&self, now: Instant, wake: bool) -> Vec<Key> {
        self.entries
            .lock()
            .unwrap()
            .iter_mut()
            .filter_map(|(key, r)| {
                if !r.running && (wake || r.next <= now) {
                    r.running = true;
                    Some(*key)
                } else {
                    None
                }
            })
            .collect()
    }
    fn finish(&self, key: Key) {
        if let Some(r) = self.entries.lock().unwrap().get_mut(&key) {
            r.running = false;
            r.next = Instant::now() + backoff(r.attempt);
            r.attempt = r.attempt.saturating_add(1);
        }
    }
}

/// Read bounded native authority without any media command or catalog projection.
pub(crate) fn summary(state: &ApiState, uuid: [u8; 16]) -> Result<TapeSummary, Status> {
    if !TapeAuthority::exists(state.checkpoint_journal_dir.as_ref(), uuid)
        .map_err(crate::status_from_state_error)?
    {
        let index = state.index()?;
        if index
            .projected_through_sequence(uuid)
            .map_err(crate::status_from_state_error)?
            != 0
            || index
                .get_tape(&uuid)
                .map_err(crate::status_from_state_error)?
                .is_some_and(|t| t.total_committed_ordinals > 0)
        {
            return Err(Status::data_loss("projected tape authority is unavailable"));
        }
        return Ok(TapeSummary::default());
    }
    TapeAuthority::open(
        state.checkpoint_journal_dir.as_ref(),
        uuid,
        state.index_path.as_ref(),
    )
    .and_then(|a| a.observe_summary())
    .map_err(crate::status_from_state_error)
}

/// Raw uncovered bytes retain their original age even when an accepted plan owns closeout.
fn debt(
    s: &TapeSummary,
    alarm: u64,
    now: OffsetDateTime,
) -> Result<(u64, Option<prost_types::Timestamp>, bool), Status> {
    if s.scheme.is_none() || s.protected == s.total {
        return Ok((0, None, false));
    }
    let bytes = s
        .total
        .checked_sub(s.protected)
        .and_then(|n| n.checked_mul(u64::from(s.block_size)))
        .ok_or_else(|| Status::data_loss("unprotected byte count overflows u64"))?;
    let since = s
        .unprotected_since
        .as_deref()
        .ok_or_else(|| Status::data_loss("unprotected range has no age"))?;
    let since = OffsetDateTime::parse(since, &Rfc3339)
        .map_err(|e| Status::data_loss(format!("invalid unprotected_since: {e}")))?;
    let overdue = (now - since).whole_seconds() >= i64::try_from(alarm).unwrap_or(i64::MAX);
    Ok((
        bytes,
        Some(prost_types::Timestamp {
            seconds: since.unix_timestamp(),
            nanos: since.nanosecond() as i32,
        }),
        overdue,
    ))
}
impl ApiState {
    pub(crate) fn tape_with_debt(&self, mut tape: pb::Tape) -> Result<pb::Tape, Status> {
        let uuid = crate::catalog_conversion::decode_uuid_bytes(&tape.tape_uuid, "tape_uuid")?;
        let s = summary(self, uuid)?;
        // A missing log is legitimate only before the first native publication.
        if s.incarnation == 0
            && self
                .index()?
                .projected_through_sequence(uuid)
                .map_err(crate::status_from_state_error)?
                != 0
        {
            return Err(Status::data_loss("projected tape authority is unavailable"));
        }
        let (bytes, since, overdue) = debt(
            &s,
            self.closeout_alarm_after_secs,
            OffsetDateTime::now_utc(),
        )?;
        tape.open_epoch_start = (bytes > 0).then_some(s.protected);
        tape.open_epoch_end = (bytes > 0).then_some(s.total);
        tape.seal_due = s.seal_due.map(|r| r.as_str().to_string());
        tape.unprotected_bytes = bytes;
        tape.unprotected_since = since;
        tape.closeout_due = s.closeout_due();
        tape.closeout_overdue = overdue;
        Ok(tape)
    }
    pub(crate) fn refresh_closeout_surfaces(
        &self,
        live: &mut pb::GetLiveStatusResponse,
    ) -> Result<(), Status> {
        let tapes = self
            .index()?
            .list_tapes(None, remanence_state::TapeKindFilter::Data)
            .map_err(crate::status_from_state_error)?;
        let mut all = HashMap::new();
        for tape in tapes {
            if tape.state == "retired" {
                continue;
            }
            let tape = self.tape_with_debt(crate::catalog_conversion::tape_to_proto(tape))?;
            all.insert(tape.tape_uuid.clone(), tape);
        }
        for drive in live.libraries.iter_mut().flat_map(|l| &mut l.drives) {
            if let Some(uuid) = &drive.loaded_tape_uuid {
                let tape = all
                    .get(uuid)
                    .ok_or_else(|| Status::data_loss("loaded tape authority is unavailable"))?;
                drive.open_epoch_start = tape.open_epoch_start;
                drive.open_epoch_end = tape.open_epoch_end;
                drive.unprotected_bytes = tape.unprotected_bytes;
                drive.unprotected_since = tape.unprotected_since;
                drive.closeout_due = tape.closeout_due;
                drive.closeout_overdue = tape.closeout_overdue;
            } else {
                drive.open_epoch_start = None;
                drive.open_epoch_end = None;
                drive.unprotected_bytes = 0;
                drive.unprotected_since = None;
                drive.closeout_due = false;
                drive.closeout_overdue = false;
            }
        }
        live.closeout_debt = all
            .into_values()
            .filter(|t| t.unprotected_bytes > 0 || t.seal_due.is_some())
            .collect();
        live.closeout_debt
            .sort_by(|a, b| a.tape_uuid.cmp(&b.tape_uuid));
        Ok(())
    }
}

/// Scan every native log, retaining entries when reads or later projection fail.
pub(crate) fn scan(state: &ApiState) -> Result<(), Status> {
    for path in TapeAuthority::list_entries(state.checkpoint_journal_dir.as_ref())
        .map_err(crate::status_from_state_error)?
    {
        let uuid = TapeAuthority::entry_tape_uuid(&path).map_err(crate::status_from_state_error)?;
        if state
            .index()?
            .get_tape(&uuid)
            .map_err(crate::status_from_state_error)?
            .is_some_and(|t| t.state == "retired")
        {
            continue;
        }
        match summary(state, uuid) {
            Ok(s) => {
                let key = (uuid, s.incarnation);
                state.closeout_work.observe(
                    key,
                    s.closeout_due()
                        || finalization_due(&s)
                        || s.finalization.as_ref().is_some_and(|p| p.prepared),
                );
                if let Err(error) = alarm(state, key, &s) {
                    tracing::warn!(%error, "closeout overdue audit failed");
                }
            }
            Err(error) => {
                tracing::debug!(tape_uuid = %Uuid::from_bytes(uuid), %error, "closeout authority unavailable; debt retained")
            }
        }
    }
    Ok(())
}
fn alarm(state: &ApiState, key: Key, s: &TapeSummary) -> Result<(), Status> {
    let (bytes, _, overdue) = debt(
        s,
        state.closeout_alarm_after_secs,
        OffsetDateTime::now_utc(),
    )?;
    if !overdue {
        return Ok(());
    }
    let age = s.unprotected_since.clone().expect("debt validated age");
    if !state
        .closeout_work
        .alarmed
        .lock()
        .unwrap()
        .insert((key, age.clone()))
    {
        return Ok(());
    }
    let fields = [
        ("reason", "CloseoutOverdue".to_string()),
        ("tape_uuid", Uuid::from_bytes(key.0).to_string()),
        ("tape_incarnation", key.1.to_string()),
        ("tape_log_sequence", s.last_closed_sequence.to_string()),
        ("w", s.protected.to_string()),
        ("t", s.total.to_string()),
        ("unprotected_bytes", bytes.to_string()),
        ("unprotected_since", age),
        (
            "alarm_after_secs",
            state.closeout_alarm_after_secs.to_string(),
        ),
    ];
    tracing::warn!(tape_uuid = %Uuid::from_bytes(key.0), detail = ?fields, "CloseoutOverdue");
    crate::audit_projection::append_operation_audit(
        &mut state.index_write()?,
        &state.audit_dir,
        state.audit_fsync,
        &state.audit_append_lock,
        crate::audit_projection::OperationAuditInput {
            actor: remanence_state::AuditActor::System,
            operation_id: Uuid::new_v4(),
            operation_kind: "closeout_overdue",
            event: remanence_state::AuditEvent::OperationFailed,
            subject_kind: "tape",
            subject_id: Some(Uuid::from_bytes(key.0).to_string()),
            idempotency_key: None,
            detail: fields
                .into_iter()
                .map(|(k, v)| (k.into(), ciborium::Value::Text(v)))
                .collect(),
        },
    )
}

/// Start after admission and ownership. Concurrent tape attempts retain the
/// existing per-tape/drive reservations; no fair scheduler or placement map.
pub(crate) fn spawn(state: ApiState) {
    if state.closeout_work.started.swap(true, Ordering::SeqCst) {
        state.closeout_work.wake.notify_one();
        return;
    }
    tokio::spawn(async move {
        let mut wake = true;
        let mut inventory_at = None;
        let mut next_scan = Instant::now();
        loop {
            if state.readiness_shutdown.load(Ordering::SeqCst) {
                break;
            }
            let current_inventory = state.current_library_snapshot().map(|s| s.captured_at);
            wake |= inventory_at != current_inventory;
            inventory_at = current_inventory;
            // Full native replay is for discovery, not the retry clock. Keep
            // large cold logs off the one-second dispatch polling path.
            if wake || Instant::now() >= next_scan {
                if let Err(error) = scan(&state) {
                    tracing::warn!(%error, "closeout work-list scan failed; retrying");
                }
                next_scan = Instant::now() + Duration::from_secs(60);
            }
            for key in state.closeout_work.claim(Instant::now(), wake) {
                let state = state.clone();
                let attempt = Attempt {
                    work: state.closeout_work.clone(),
                    key,
                };
                tokio::spawn(async move {
                    let attempt = attempt;
                    if let Err(error) =
                        crate::mount::service_closeout_obligation(&state, key.0, key.1).await
                    {
                        tracing::debug!(tape_uuid = %Uuid::from_bytes(key.0), %error, "closeout obligation remains queued");
                    }
                    drop(attempt);
                    if let Ok(s) = summary(&state, key.0) {
                        state.closeout_work.observe(
                            key,
                            s.incarnation == key.1
                                && (s.closeout_due()
                                    || finalization_due(&s)
                                    || s.finalization.as_ref().is_some_and(|p| p.prepared)),
                        );
                    }
                });
            }
            wake = tokio::select! {
                _ = state.closeout_work.wake.notified() => true,
                _ = tokio::time::sleep(Duration::from_secs(1)) => false,
            };
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn aged() -> TapeSummary {
        let mut s = TapeSummary::default();
        s.tape_uuid = Some([1; 16]);
        s.incarnation = 1;
        s.block_size = 262144;
        s.scheme = Some(remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_static("rs-b7"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        });
        s.total = 3;
        s.unprotected_since = Some("2020-01-01T00:00:00Z".into());
        s
    }
    #[test]
    fn b7_backoff_deduplicates_merges_and_retains_failed_dispatch() {
        let work = WorkList::default();
        let key = ([1; 16], 1);
        work.observe(key, true);
        work.observe(key, true);
        assert_eq!(work.claim(Instant::now(), false), vec![key]);
        assert!(
            work.claim(Instant::now(), true).is_empty(),
            "running tape is not dispatched twice"
        );
        for seconds in [1, 5, 30, 60, 60] {
            work.finish(key);
            assert!(work.claim(Instant::now(), false).is_empty());
            let r = work.entries.lock().unwrap();
            assert!(r[&key].next.duration_since(Instant::now()).as_secs_f64() <= seconds as f64);
            assert!(
                r[&key].next.duration_since(Instant::now()).as_secs_f64() > seconds as f64 - 0.5
            );
            drop(r);
            assert_eq!(
                work.claim(Instant::now() + Duration::from_secs(seconds), false),
                vec![key]
            );
        }
        work.finish(key);
        assert_eq!(
            work.claim(Instant::now(), true),
            vec![key],
            "release/inventory bypasses backoff"
        );
        work.finish(key);
        work.observe(key, false);
        assert!(work.entries.lock().unwrap().is_empty());
    }
    #[test]
    fn b7_alarm_once_per_age_and_again_after_restart_even_without_drive() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("audit")).unwrap();
        let make = || {
            let mut state = ApiState::new(
                remanence_state::CatalogIndex::open(root.path().join("state.sqlite")).unwrap(),
            );
            state.audit_dir = std::sync::Arc::new(root.path().join("audit"));
            state
        };
        let state = make();
        let s = aged();
        let key = ([1; 16], 1);
        let warnings = crate::tests::capture_warnings(|| {
            alarm(&state, key, &s).unwrap();
            alarm(&state, key, &s).unwrap();
        });
        assert_eq!(
            warnings
                .iter()
                .filter(|s| s.contains("CloseoutOverdue"))
                .count(),
            1
        );
        assert!(state.drive_pool.is_none());
        assert_eq!(
            state
                .index()
                .unwrap()
                .list_operations()
                .unwrap()
                .iter()
                .filter(|o| o.operation_kind == "closeout_overdue")
                .count(),
            1
        );
        alarm(&make(), key, &s).unwrap();
        assert_eq!(
            state
                .index()
                .unwrap()
                .list_operations()
                .unwrap()
                .iter()
                .filter(|o| o.operation_kind == "closeout_overdue")
                .count(),
            2
        );
        assert_eq!(
            debt(&s, 259200, OffsetDateTime::now_utc()).unwrap().0,
            3 * 262144
        );
        let mut zero = s.clone();
        zero.protected = zero.total;
        assert_eq!(
            debt(&zero, 1, OffsetDateTime::now_utc()).unwrap(),
            (0, None, false)
        );
    }
}
