//! Replay-derived parity debt, retry bookkeeping and operator notification.
//! The list is disposable: native logs remain authority across every failure.
use crate::{pb, ApiState};
use remanence_parity::authority::TapeAuthorityView;
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
    warned: Mutex<HashSet<(Key, tonic::Code)>>,
    libraries: Mutex<HashSet<String>>,
    pub(crate) wake: tokio::sync::Notify,
}
struct Retry {
    attempt: usize,
    next: Instant,
    running: bool,
    library: String,
    parked: bool,
    reason: Option<String>,
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

/// Recheck the actual route at dispatch as inventory can change after claim.
/// This guard enforces the per-library bound even across such a route change.
pub(crate) struct LibraryAttempt {
    work: std::sync::Arc<WorkList>,
    library: String,
}
impl Drop for LibraryAttempt {
    fn drop(&mut self) {
        self.work.libraries.lock().unwrap().remove(&self.library);
    }
}
impl WorkList {
    pub(crate) fn reserve_library(
        self: &std::sync::Arc<Self>,
        library: &str,
    ) -> Result<LibraryAttempt, Status> {
        if !self.libraries.lock().unwrap().insert(library.to_owned()) {
            return Err(Status::unavailable("library closeout worker is busy"));
        }
        Ok(LibraryAttempt {
            work: self.clone(),
            library: library.to_owned(),
        })
    }
}

/// Capped retry schedule for transient resource and dispatch failures only.
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
                library: "unlocated".into(),
                parked: false,
                reason: None,
            });
        } else if entries.get(&key).is_some_and(|r| !r.running) {
            entries.remove(&key);
        }
    }
    fn claim(&self, now: Instant, wake: bool) -> Vec<Key> {
        let mut entries = self.entries.lock().unwrap();
        if wake {
            for retry in entries.values_mut().filter(|r| !r.running) {
                retry.parked = false;
                retry.next = now;
            }
        }
        let mut busy = entries
            .values()
            .filter(|r| r.running)
            .map(|r| r.library.clone())
            .collect::<HashSet<_>>();
        entries
            .iter_mut()
            .filter_map(|(key, r)| {
                if !r.running
                    && (wake || (!r.parked && r.next <= now))
                    && busy.insert(r.library.clone())
                {
                    r.running = true;
                    Some(*key)
                } else {
                    None
                }
            })
            .collect()
    }
    fn forget(&self, uuid: [u8; 16]) {
        self.entries
            .lock()
            .unwrap()
            .retain(|(t, _), r| *t != uuid || r.running);
    }
    fn failed(&self, key: Key, error: &Status) {
        let transient = transient(error);
        if let Some(r) = self.entries.lock().unwrap().get_mut(&key) {
            let reason = error.message().to_string();
            if !transient && self.warned.lock().unwrap().insert((key, error.code())) {
                tracing::warn!(tape_uuid = %Uuid::from_bytes(key.0), %error, "closeout needs operator; waiting for wake");
            }
            r.parked = !transient;
            r.reason = Some(reason);
        }
    }
    fn reason(&self, uuid: [u8; 16]) -> Option<String> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .find(|((t, _), _)| *t == uuid)
            .and_then(|(_, r)| r.reason.clone())
    }

    fn finish(&self, key: Key) {
        if let Some(r) = self.entries.lock().unwrap().get_mut(&key) {
            r.running = false;
            r.next = Instant::now() + backoff(r.attempt);
            r.attempt = r.attempt.saturating_add(1);
        }
    }
}

/// Media/identity refusals require an explicit wake even if a lower layer
/// wrapped the cause in Unavailable. Busy and memory remain transient.
fn transient(error: &Status) -> bool {
    let message = error.message().to_ascii_lowercase();
    if [
        "write-protect",
        "write protect",
        "fence",
        "wrong medium",
        "identity",
        "supersede-tail",
    ]
    .iter()
    .any(|s| message.contains(s))
    {
        return false;
    }
    matches!(
        error.code(),
        tonic::Code::Unavailable | tonic::Code::ResourceExhausted
    ) || [
        "tape is already mounted",
        "busy",
        "reserved",
        "no free drive",
    ]
    .iter()
    .any(|s| message.contains(s))
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
        tape.unprotected_bytes = Some(bytes);
        tape.unprotected_since = since;
        tape.closeout_due = Some(s.closeout_due());
        tape.closeout_overdue = Some(overdue);
        tape.closeout_retry_reason = self.closeout_work.reason(uuid);
        Ok(tape)
    }
    /// Lists retain per-tape failures so one damaged log cannot hide inventory.
    pub(crate) fn tape_with_debt_or_error(&self, tape: pb::Tape) -> pb::Tape {
        match self.tape_with_debt(tape.clone()) {
            Ok(tape) => tape,
            Err(error) => pb::Tape {
                unprotected_bytes: None,
                unprotected_since: None,
                closeout_due: None,
                closeout_overdue: None,
                open_epoch_start: None,
                open_epoch_end: None,
                closeout_authority_error: Some(error.message().to_string()),
                ..tape
            },
        }
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
            let tape = self.tape_with_debt_or_error(crate::catalog_conversion::tape_to_proto(tape));
            all.insert(tape.tape_uuid.clone(), tape);
        }
        for drive in live.libraries.iter_mut().flat_map(|l| &mut l.drives) {
            if let Some(uuid) = &drive.loaded_tape_uuid {
                let missing = pb::Tape {
                    closeout_authority_error: Some("loaded tape authority is unavailable".into()),
                    ..Default::default()
                };
                let tape = all.get(uuid).unwrap_or(&missing);
                drive.open_epoch_start = tape.open_epoch_start;
                drive.open_epoch_end = tape.open_epoch_end;
                drive.unprotected_bytes = tape.unprotected_bytes;
                drive.unprotected_since = tape.unprotected_since;
                drive.closeout_due = tape.closeout_due;
                drive.closeout_overdue = tape.closeout_overdue;
                drive.closeout_authority_error = tape.closeout_authority_error.clone();
                drive.closeout_retry_reason = tape.closeout_retry_reason.clone();
            } else {
                drive.open_epoch_start = None;
                drive.open_epoch_end = None;
                drive.unprotected_bytes = None;
                drive.unprotected_since = None;
                drive.closeout_due = None;
                drive.closeout_overdue = None;
                drive.closeout_authority_error = None;
                drive.closeout_retry_reason = None;
            }
        }
        live.closeout_debt = all
            .into_values()
            .filter(|t| {
                t.unprotected_bytes.is_some_and(|b| b > 0)
                    || t.seal_due.is_some()
                    || t.closeout_authority_error.is_some()
            })
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
        let uuid = match TapeAuthority::entry_tape_uuid(&path) {
            Ok(uuid) => uuid,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "closeout discovery skipped invalid entry");
                continue;
            }
        };
        let catalog = state.index().and_then(|index| {
            index
                .get_tape(&uuid)
                .map_err(crate::status_from_state_error)
        });
        match catalog {
            Ok(None) => {
                state.closeout_work.forget(uuid);
                continue;
            }
            Ok(Some(tape)) if tape.state == "retired" => {
                state.closeout_work.forget(uuid);
                continue;
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "closeout discovery catalog read failed; continuing");
                continue;
            }
            _ => {}
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
                if let Ok(pool) = state.drive_pool() {
                    if let Ok(library) =
                        crate::mount::resolve_tape_library_serial(state, pool, &uuid)
                    {
                        if let Some(retry) =
                            state.closeout_work.entries.lock().unwrap().get_mut(&key)
                        {
                            if !retry.running {
                                retry.library = library;
                            }
                        }
                    }
                }
                // Projection is host-only and independent of mount scope. Retry
                // deferred startup projection without touching media or owning a drive.
                if let Err(error) = retry_projection(state, uuid, s.last_closed_sequence) {
                    tracing::debug!(tape_uuid = %Uuid::from_bytes(uuid), %error, "closeout host projection deferred");
                }
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
fn retry_projection(state: &ApiState, uuid: [u8; 16], sequence: u64) -> Result<(), Status> {
    if state
        .index()?
        .projected_through_sequence(uuid)
        .map_err(crate::status_from_state_error)?
        >= sequence
    {
        return Ok(());
    }
    let snapshot = TapeAuthority::open(
        state.checkpoint_journal_dir.as_ref(),
        uuid,
        state.index_path.as_ref(),
    )
    .and_then(|a| a.freeze())
    .map_err(crate::status_from_state_error)?;
    state
        .index_write()?
        .project_tape_log(&snapshot)
        .map_err(crate::status_from_state_error)
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
            let current_inventory = state.current_library_snapshot().map(|s| {
                s.report
                    .libraries
                    .iter()
                    .map(|l| {
                        (
                            l.serial.clone(),
                            l.drive_bays
                                .iter()
                                .map(|b| {
                                    (
                                        b.element_address,
                                        b.loaded,
                                        b.loaded_tape.clone(),
                                        b.source_slot,
                                    )
                                })
                                .collect::<Vec<_>>(),
                            l.slots
                                .iter()
                                .map(|slot| {
                                    (slot.element_address, slot.full, slot.cartridge.clone())
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect::<Vec<_>>()
            });
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
                        state.closeout_work.failed(key, &error);
                        tracing::debug!(tape_uuid = %Uuid::from_bytes(key.0), %error, "closeout obligation remains queued");
                    }
                    drop(attempt);
                    if state
                        .index()
                        .and_then(|i| i.get_tape(&key.0).map_err(crate::status_from_state_error))
                        .is_ok_and(|t| t.is_none_or(|t| t.state == "retired"))
                    {
                        state.closeout_work.forget(key.0);
                    } else if let Ok(s) = summary(&state, key.0) {
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

#[cfg(test)]
mod fix_tests {
    use super::*;
    #[test]
    fn b7_fix_operator_failures_wait_for_wake_and_one_attempt_per_library() {
        let work = WorkList::default();
        let keys = [([1; 16], 1), ([2; 16], 1), ([3; 16], 1)];
        for key in keys {
            work.observe(key, true);
        }
        work.entries
            .lock()
            .unwrap()
            .get_mut(&keys[2])
            .unwrap()
            .library = "other-library".into();
        let claimed = work.claim(Instant::now(), true);
        assert_eq!(claimed.len(), 2);
        assert!(work.claim(Instant::now(), true).is_empty());
        let key = *claimed.iter().find(|key| **key != keys[2]).unwrap();
        work.failed(key, &Status::unavailable("write-protected cartridge"));
        work.finish(key);
        let later = Instant::now() + Duration::from_secs(3600);
        assert!(!work.claim(later, false).contains(&key));
        // Release the other same-library attempt selected by the preceding poll.
        for key in keys {
            work.finish(key);
        }
        let reason = work.reason(key.0).unwrap();
        assert!(reason.contains("write-protected"));
        // Isolate the parked key to prove a wake bypasses the park.
        work.entries.lock().unwrap().retain(|k, _| *k == key);
        assert_eq!(work.claim(later, true), vec![key]);
        for error in [
            Status::resource_exhausted("memory"),
            Status::unavailable("no free drive"),
            Status::failed_precondition("tape is already mounted"),
        ] {
            assert!(transient(&error));
        }
        for error in [
            Status::failed_precondition("fenced"),
            Status::unavailable("identity mismatch"),
            Status::unavailable("wrong medium"),
        ] {
            assert!(!transient(&error));
        }
    }
}
