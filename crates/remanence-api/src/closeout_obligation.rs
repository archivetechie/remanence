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
    media_failures: usize,
    next: Instant,
    running: bool,
    library: String,
    parked: bool,
    reason: Option<String>,
    refusal: Option<Status>,
    woken: bool,
    readiness: Option<Uuid>,
}
struct Attempt {
    work: std::sync::Arc<WorkList>,
    key: Key,
    shutdown: bool,
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if self.shutdown {
            if let Some(entry) = self.work.entries.lock().unwrap().get_mut(&self.key) {
                entry.running = false;
            }
        } else {
            self.work.finish(self.key);
        }
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
    /// Scheduling snapshot only; actual admission still proves native authority.
    pub(crate) fn parked_tapes(&self) -> HashSet<[u8; 16]> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter_map(|((tape, _), retry)| retry.parked.then_some(*tape))
            .collect()
    }
    /// Preserve the original parked refusal even if it also changed the catalog
    /// to recovery_required, causing selection to stop before actor admission.
    pub(crate) fn parked_refusal(&self, tape: [u8; 16]) -> Option<Status> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .find_map(|((id, _), retry)| {
                (*id == tape && retry.parked)
                    .then(|| retry.refusal.clone())
                    .flatten()
            })
    }
    /// Exclude current owners at every dispatch tick, including between scans.
    fn claim_unheld(&self, now: Instant, held: &HashSet<[u8; 16]>) -> Vec<Key> {
        self.claim_filtered(now, false, held)
    }
    /// Seed and inspect prior evidence without changing eligibility in barrier tests.
    #[cfg(test)]
    pub(crate) fn seed_retry_reason_for_test(&self, key: ([u8; 16], u64), reason: &str) {
        self.entries.lock().unwrap().get_mut(&key).unwrap().reason = Some(reason.into());
    }

    #[cfg(test)]
    pub(crate) fn retry_evidence_for_test(
        &self,
        key: ([u8; 16], u64),
    ) -> (usize, Option<String>, bool) {
        let entries = self.entries.lock().unwrap();
        let retry = entries.get(&key).unwrap();
        (retry.attempt, retry.reason.clone(), retry.running)
    }

    /// Assert live ownership caused no work-list attempts or retry diagnostics.
    #[cfg(test)]
    pub(crate) fn assert_unattempted_for_test(&self, tape: [u8; 16]) {
        let entries = self.entries.lock().unwrap();
        let retries: Vec<_> = entries.iter().filter(|((id, _), _)| *id == tape).collect();
        assert!(!retries.is_empty());
        for (_, retry) in retries {
            assert_eq!(retry.attempt, 0);
            assert!(!retry.running);
            assert!(retry.reason.is_none(), "{:?}", retry.reason);
        }
    }
    #[cfg(test)]
    pub(crate) fn contains(&self, key: Key) -> bool {
        self.entries.lock().unwrap().contains_key(&key)
    }
    fn observe(&self, key: Key, due: bool) {
        let mut entries = self.entries.lock().unwrap();
        if due {
            entries.entry(key).or_insert(Retry {
                attempt: 0,
                media_failures: 0,
                next: Instant::now(),
                running: false,
                library: "unlocated".into(),
                parked: false,
                reason: None,
                refusal: None,
                woken: false,
                readiness: None,
            });
        } else if entries.get(&key).is_some_and(|r| !r.running) {
            entries.remove(&key);
        }
    }
    #[cfg(test)]
    fn claim(&self, now: Instant, wake: bool) -> Vec<Key> {
        self.claim_filtered(now, wake, &HashSet::new())
    }
    fn claim_filtered(&self, now: Instant, wake: bool, held: &HashSet<[u8; 16]>) -> Vec<Key> {
        let mut entries = self.entries.lock().unwrap();
        if wake {
            for retry in entries.values_mut() {
                retry.woken |= retry.running;
                retry.parked = false;
                retry.media_failures = 0;
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
                if !held.contains(&key.0)
                    && !r.running
                    && (wake || (!r.parked && r.next <= now))
                    && busy.insert(r.library.clone())
                {
                    r.running = true;
                    r.woken = false;
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
            r.refusal = Some(error.clone());
            if !transient && self.warned.lock().unwrap().insert((key, error.code())) {
                tracing::warn!(tape_uuid = %Uuid::from_bytes(key.0), %error, "closeout needs operator; waiting for wake");
            }
            let media = error
                .metadata()
                .get("remanence-retry")
                .is_some_and(|c| c == "media")
                && crate::readiness_wait::ReadinessFence::from_status(error).is_none()
                && crate::drive_pool::TapeBusy::from_status(error).is_none();
            if media {
                r.media_failures = r.media_failures.saturating_add(1);
            }
            r.parked = !transient || r.media_failures >= 3;
            r.readiness = crate::readiness_wait::ReadinessFence::from_status(error).map(|f| f.0);
            r.reason = Some(if media {
                format!(
                    "{}media fault ({}/3 attempts): {reason}",
                    if r.parked { "parked: " } else { "" },
                    r.media_failures
                )
            } else {
                reason
            });
        }
    }
    /// Record the wake synchronously, including during an in-flight attempt.
    /// Notify is only a scheduling hint; coalescing it cannot lose eligibility.
    pub(crate) fn wake_tape(&self, uuid: [u8; 16]) {
        for ((tape, _), retry) in self.entries.lock().unwrap().iter_mut() {
            if *tape == uuid {
                retry.woken |= retry.running;
                retry.parked = false;
                retry.media_failures = 0;
                retry.next = Instant::now();
                retry.readiness = None;
            }
        }
        self.wake.notify_one();
    }
    pub(crate) fn wake_all(&self) {
        let tapes: Vec<_> = self.entries.lock().unwrap().keys().map(|k| k.0).collect();
        for tape in tapes {
            self.wake_tape(tape);
        }
    }
    #[cfg(test)]
    pub(crate) fn park_for_test(&self, library: &str) {
        let key = ([0xa1; 16], 1);
        self.observe(key, true);
        self.entries.lock().unwrap().get_mut(&key).unwrap().library = library.into();
        self.failed(
            key,
            &RetryCause::Operator.attach(Status::failed_precondition("write protected")),
        );
    }
    #[cfg(test)]
    pub(crate) fn parked_for_test(&self) -> bool {
        self.entries.lock().unwrap().values().all(|r| r.parked)
    }
    /// Wait for the startup worker to relinquish a fenced tape before a test competes.
    #[cfg(test)]
    pub(crate) async fn wait_for_fence_refusal(&self, tape: [u8; 16]) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if self
                    .reason(tape)
                    .is_some_and(|r| r.contains("active tape-I/O fence"))
                    && !self.running_for_test(tape)
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("startup worker records the fence refusal and releases its attempt");
    }
    #[cfg(test)]
    fn running_for_test(&self, tape: [u8; 16]) -> bool {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .any(|((t, _), r)| *t == tape && r.running)
    }
    /// Explicit operator motion wakes only work in that library.
    pub(crate) fn wake_library(&self, library: &str) {
        let tapes: Vec<_> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, r)| r.library == library)
            .map(|(k, _)| k.0)
            .collect();
        for tape in tapes {
            self.wake_tape(tape);
        }
    }
    fn succeeded(&self, key: Key) {
        if let Some(retry) = self.entries.lock().unwrap().get_mut(&key) {
            retry.media_failures = 0;
            retry.reason = None;
            retry.refusal = None;
            retry.parked = false;
            retry.readiness = None;
        }
    }
    /// A ready/released fence wakes exactly its refused tape, even when the
    /// clearance preceded the failing attempt's publication of its reason.
    fn wake_ready(&self, state: &ApiState) -> Result<(), Status> {
        let waiting: Vec<_> = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(key, r)| r.readiness.map(|id| (key.0, id)))
            .collect();
        if waiting.is_empty() {
            return Ok(());
        }
        let index = state.index()?;
        for (tape, id) in waiting {
            if index
                .media_readiness_operation(id)
                .map_err(crate::status_from_state_error)?
                .is_some_and(|r| matches!(r.state.as_str(), "ready" | "released"))
            {
                self.wake_tape(tape);
            }
        }
        Ok(())
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
            r.next = if r.woken {
                r.parked = false;
                r.media_failures = 0;
                Instant::now()
            } else {
                Instant::now() + backoff(r.attempt)
            };
            r.attempt = r.attempt.saturating_add(1);
        }
    }
}

/// Preserve retry semantics when typed lower-layer causes become actor Status.
/// Metadata survives holder diagnostics and RPC forwarding; text is fallback only.
#[derive(Clone, Copy)]
pub(crate) enum RetryCause {
    Transient,
    Operator,
    Media,
}
impl RetryCause {
    pub(crate) fn tape_io(error: &remanence_library::TapeIoError) -> Self {
        use remanence_library::TapeIoError;
        match error {
            TapeIoError::CheckCondition(remanence_library::ScsiError::CheckCondition {
                sense,
                ..
            })
            | TapeIoError::CheckConditionWithResidue {
                source: remanence_library::ScsiError::CheckCondition { sense, .. },
                ..
            } if remanence_scsi::sense::decode_sense(sense)
                .is_some_and(|s| matches!(s.key, 3 | 4)) =>
            {
                Self::Media
            }
            TapeIoError::PartialBatchUncommittable {
                sense: Some(sense), ..
            } if remanence_scsi::sense::decode_sense(sense)
                .is_some_and(|s| matches!(s.key, 3 | 4)) =>
            {
                Self::Media
            }
            TapeIoError::GoodWriteTripwire { cause, .. } => Self::tape_io(cause),
            TapeIoError::WriteFailureWithPositionError { write_error, .. } => {
                Self::tape_io(write_error)
            }
            TapeIoError::WriteProtected
            | TapeIoError::DataProtect(_)
            | TapeIoError::MultiPartitionMedium { .. } => Self::Operator,
            _ => Self::Transient,
        }
    }
    pub(crate) fn identity(error: &crate::TapeIdentityError) -> Self {
        match error {
            crate::TapeIdentityError::Io(error) => Self::tape_io(error),
            _ => Self::Operator,
        }
    }
    pub(crate) fn attach(self, mut status: Status) -> Status {
        status.metadata_mut().insert(
            "remanence-retry",
            match self {
                Self::Transient => "transient",
                Self::Operator => "operator",
                Self::Media => "media",
            }
            .parse()
            .unwrap(),
        );
        status
    }
}
pub(crate) fn transient(error: &Status) -> bool {
    if crate::readiness_wait::ReadinessFence::from_status(error).is_some()
        || crate::drive_pool::TapeBusy::from_status(error).is_some()
    {
        return true;
    }
    if let Some(cause) = error.metadata().get("remanence-retry") {
        return cause != "operator";
    }
    // Holder labels describe other work, never the cause of this refusal.
    let message = error
        .message()
        .split("; held by:")
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if message.contains("media-readiness fence") {
        return true;
    }
    ![
        "write-protect",
        "write protect",
        "tape-i/o fence",
        "fenced",
        "wrong medium",
        "identity mismatch",
        "supersede-tail",
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
        // Retired catalog rows outlive their removed native logs. They have no
        // live debt and must not turn a successful inventory into a CLI error.
        if tape.state == pb::tape::State::TapeStateRetired as i32 {
            return Ok(tape);
        }
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
        let mut retired = HashSet::new();
        for tape in tapes {
            if tape.state == "retired" {
                retired.insert(tape.tape_uuid);
                continue;
            }
            let tape = self.tape_with_debt_or_error(crate::catalog_conversion::tape_to_proto(tape));
            all.insert(tape.tape_uuid.clone(), tape);
        }
        for drive in live.libraries.iter_mut().flat_map(|l| &mut l.drives) {
            if let Some(uuid) = &drive.loaded_tape_uuid {
                let missing = pb::Tape {
                    closeout_authority_error: Some("loaded tape authority is unavailable".into()),
                    ..Default::default()
                };
                let retired_tape = pb::Tape::default();
                let tape = if retired.contains(uuid) {
                    &retired_tape
                } else {
                    all.get(uuid).unwrap_or(&missing)
                };
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
    let _admission = state.operations.readiness_start.lock().unwrap();
    if state.readiness_shutdown.load(Ordering::Acquire) {
        return Ok(());
    }
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
fn inventory_membership(state: &ApiState) -> HashSet<(String, String)> {
    state
        .current_library_snapshot()
        .map(|snapshot| {
            snapshot
                .report
                .libraries
                .iter()
                .flat_map(|library| {
                    library
                        .drive_bays
                        .iter()
                        .filter_map(|bay| bay.loaded_tape.clone())
                        .chain(
                            library
                                .slots
                                .iter()
                                .filter_map(|slot| slot.cartridge.clone()),
                        )
                        .map(|barcode| (library.serial.clone(), barcode))
                        .collect::<Vec<_>>()
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn spawn(state: ApiState) {
    if state.closeout_work.started.swap(true, Ordering::SeqCst) {
        state.closeout_work.wake.notify_one();
        return;
    }
    tokio::spawn(async move {
        let mut wake = true;
        let mut inventory_at: Option<HashSet<(String, String)>> = None;
        let mut next_scan = Instant::now();
        loop {
            if state.readiness_shutdown.load(Ordering::SeqCst) {
                break;
            }
            let current_inventory = inventory_membership(&state);
            if let Some(previous) = &inventory_at {
                if previous != &current_inventory {
                    if let Ok(index) = state.index() {
                        if let Ok(tapes) =
                            index.list_tapes(None, remanence_state::TapeKindFilter::Data)
                        {
                            for tape in tapes {
                                if tape.voltag.as_ref().is_some_and(|barcode| {
                                    previous
                                        .symmetric_difference(&current_inventory)
                                        .any(|(_, b)| b == barcode)
                                }) {
                                    if let Ok(uuid) = tape.tape_uuid.try_into() {
                                        state.closeout_work.wake_tape(uuid);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            inventory_at = Some(current_inventory);
            if let Err(error) = state.closeout_work.wake_ready(&state) {
                tracing::debug!(%error, "closeout readiness wake check failed; retrying");
            }
            // Full native replay is for discovery, not the retry clock. Keep
            // large cold logs off the one-second dispatch polling path.
            if wake || Instant::now() >= next_scan {
                let scan_state = state.clone();
                if let Err(error) = tokio::task::spawn_blocking(move || scan(&scan_state))
                    .await
                    .unwrap_or_else(|e| Err(Status::internal(format!("closeout scan task: {e}"))))
                {
                    tracing::warn!(%error, "closeout work-list scan failed; retrying");
                }
                next_scan = Instant::now() + Duration::from_secs(60);
            }
            let claimed = {
                let _admission = state.operations.readiness_start.lock().unwrap();
                if state.readiness_shutdown.load(Ordering::Acquire) {
                    break;
                }
                let held = state
                    .drive_pool()
                    .map(|pool| pool.mounted_tape_uuids())
                    .unwrap_or_default();
                state.closeout_work.claim_unheld(Instant::now(), &held)
            };
            for key in claimed {
                let state = state.clone();
                let attempt = Attempt {
                    work: state.closeout_work.clone(),
                    key,
                    shutdown: false,
                };
                tokio::spawn(service_claimed(state, key, attempt));
            }
            wake = tokio::select! {
                _ = state.closeout_work.wake.notified() => true,
                _ = tokio::time::sleep(Duration::from_secs(1)) => false,
            };
        }
    });
}

/// Service one claimed entry and refresh its debt after releasing attempt ownership.
async fn service_claimed(state: ApiState, key: Key, mut attempt: Attempt) {
    if let Err(error) = crate::mount::service_closeout_obligation(&state, key.0, key.1).await {
        if crate::drive_pool::DaemonShuttingDown::is_status(&error) {
            // Release ownership without scheduling backoff or changing the
            // existing debt/retry evidence needed by the next daemon start.
            attempt.shutdown = true;
            return;
        }
        state.closeout_work.failed(key, &error);
        tracing::debug!(tape_uuid = %Uuid::from_bytes(key.0), %error, "closeout obligation remains queued");
    } else {
        state.closeout_work.succeeded(key);
    }
    drop(attempt);
    if state
        .index()
        .and_then(|i| i.get_tape(&key.0).map_err(crate::status_from_state_error))
        .is_ok_and(|t| t.is_none_or(|t| t.state == "retired"))
    {
        state.closeout_work.forget(key.0);
    } else if let Ok(Ok(s)) = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || summary(&state, key.0)).await
    } {
        state.closeout_work.observe(
            key,
            s.incarnation == key.1
                && (s.closeout_due()
                    || finalization_due(&s)
                    || s.finalization.as_ref().is_some_and(|p| p.prepared)),
        );
    }
}

/// Drive the production dispatcher without its polling clock or background scan.
#[cfg(test)]
pub(crate) async fn service_unheld_for_test(state: &ApiState) -> Vec<Key> {
    let held = state.drive_pool().unwrap().mounted_tape_uuids();
    let claimed = state.closeout_work.claim_unheld(Instant::now(), &held);
    for key in &claimed {
        let attempt = Attempt {
            work: state.closeout_work.clone(),
            key: *key,
            shutdown: false,
        };
        service_claimed(state.clone(), *key, attempt).await;
    }
    claimed
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
            work.claim(Instant::now(), false).is_empty(),
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
    /// Typed medium/hardware causes share a budget; text and contention cannot reset it.
    #[test]
    fn b9_media_faults_park_after_three_attempts_and_wake() {
        use remanence_library::{ScsiError, TapeIoError};
        let work = WorkList::default();
        let key = ([0xb9; 16], 1);
        work.observe(key, true);
        for (n, sense_key) in [3, 4, 3].into_iter().enumerate() {
            assert_eq!(
                work.claim(Instant::now() + Duration::from_secs(1000), false),
                vec![key]
            );
            let mut sense = vec![0; 18];
            sense[0] = 0x70;
            sense[2] = sense_key;
            sense[7] = 10;
            let cause =
                RetryCause::tape_io(&TapeIoError::CheckCondition(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                }));
            work.failed(
                key,
                &cause.attach(Status::unavailable(format!("different diagnostic {n}"))),
            );
            work.finish(key);
            assert_eq!(work.entries.lock().unwrap()[&key].parked, n == 2);
            if n < 2 {
                assert_eq!(
                    work.claim(Instant::now() + Duration::from_secs(1000), false),
                    vec![key]
                );
                work.failed(
                    key,
                    &Status::unavailable("no free drive; held by: medium failure"),
                );
                work.finish(key);
            }
        }
        assert!(work
            .claim(Instant::now() + Duration::from_secs(10000), false)
            .is_empty());
        assert!(work
            .reason(key.0)
            .unwrap()
            .contains("parked: media fault (3/3 attempts)"));
        work.wake_tape(key.0);
        assert_eq!(work.claim(Instant::now(), false), vec![key]);
        assert_eq!(work.entries.lock().unwrap()[&key].media_failures, 0);
    }

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
        assert!(work.claim(Instant::now(), false).is_empty());
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
    /// Each typed class beats misleading diagnostics; unknown failures retry.
    #[test]
    fn b7_fix2_retry_causes_and_unknown_backoff() {
        use crate::write_owner::status_from_pool_write_error;
        use remanence_library::TapeIoError;
        for error in [
            crate::PoolWriteError::TapeIo(TapeIoError::WriteProtected),
            crate::PoolWriteError::TapeIdentity(crate::TapeIdentityError::Mismatch {
                expected: "a".into(),
                actual: "b".into(),
            }),
            crate::PoolWriteError::TapeIo(TapeIoError::MultiPartitionMedium {
                count: 2,
                device: "model".into(),
            }),
        ] {
            let mut status = status_from_pool_write_error(error);
            // Retain only typed metadata, with text that cannot classify it.
            status = Status::with_details_and_metadata(
                status.code(),
                "opaque refusal",
                status.details().to_vec().into(),
                status.metadata().clone(),
            );
            assert!(!transient(&status));
        }
        assert!(transient(&status_from_pool_write_error(
            crate::PoolWriteError::TapeIo(TapeIoError::IdentityInvalidated {
                proved: 1,
                current: 2
            })
        )));
        let untyped = TapeIoError::IdentityInvalidated {
            proved: 1,
            current: 2,
        };
        assert!(transient(&Status::internal(untyped.to_string())));
        let fence = crate::status_from_state_error(remanence_state::StateError::from(
            remanence_state::tape_log::store::StoreError::Fenced,
        ));
        assert!(!transient(&fence));
        assert!(transient(
            &crate::readiness_wait::ReadinessFence(Uuid::new_v4())
                .status("identity mismatch; fence")
        ));
        let transport = crate::PoolWriteError::TapeIo(TapeIoError::OperationFailed(
            "identity mismatch in transport diagnostic".into(),
        ));
        assert!(transient(&status_from_pool_write_error(transport)));
        assert!(transient(&Status::unavailable(
            "no free drive; held by: identity mismatch fenced"
        )));
        for message in [
            "write-protected",
            "wrong medium",
            "identity mismatch",
            "tape-I/O fence",
        ] {
            assert!(!transient(&Status::failed_precondition(message)));
        }
        let work = WorkList::default();
        let key = ([0xf1; 16], 1);
        work.observe(key, true);
        for (attempt, seconds) in [1, 5, 30, 60, 60].into_iter().enumerate() {
            let now = Instant::now() + Duration::from_secs(1000 * attempt as u64);
            assert_eq!(work.claim(now, false), vec![key]);
            work.failed(key, &Status::internal("unrecognised temporary failure"));
            work.finish(key);
            let entries = work.entries.lock().unwrap();
            let retry = &entries[&key];
            assert!(!retry.parked);
            let remaining = retry.next.duration_since(Instant::now()).as_secs_f64();
            assert!(remaining <= seconds as f64 && remaining > seconds as f64 - 0.5);
        }
    }

    #[test]
    fn b7_fix2_wake_during_attempt_retries_once_and_success_clears_reason() {
        for before_failure in [true, false] {
            let work = WorkList::default();
            let key = ([0xf2; 16], 1);
            work.observe(key, true);
            assert_eq!(work.claim(Instant::now(), false), vec![key]);
            if before_failure {
                work.wake_tape(key.0);
            }
            work.failed(key, &Status::failed_precondition("write-protected"));
            if !before_failure {
                work.wake_tape(key.0);
            }
            assert!(work.claim(Instant::now(), false).is_empty());
            work.finish(key);
            assert_eq!(work.claim(Instant::now(), false), vec![key]);
            work.failed(key, &Status::failed_precondition("write-protected"));
            work.finish(key);
            assert!(work
                .claim(Instant::now() + Duration::from_secs(3600), false)
                .is_empty());
            work.wake_tape(key.0);
            assert_eq!(work.claim(Instant::now(), false), vec![key]);
            work.succeeded(key);
            work.finish(key);
            assert!(work.reason(key.0).is_none());
        }
    }

    #[test]
    fn b7_fix2_readiness_clearance_wakes_only_affected_tape() {
        let root = tempfile::tempdir().unwrap();
        let mut index =
            remanence_state::CatalogIndex::open(root.path().join("state.sqlite")).unwrap();
        let id = Uuid::new_v4();
        index
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id: id,
                run_id: None,
                endpoint: remanence_state::DriveKey::new("model", 1),
                changer_sg: None,
                drive_sg: None,
                drive_serial: None,
                barcode: Some("TAPE".into()),
                source_slot: None,
                media_generation: Some(9),
                phase: "poll".into(),
                state: "becoming_ready".into(),
                dirty_scope: Some("drive".into()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .unwrap();
        let state = ApiState::new(index);
        let key = ([0xf3; 16], 1);
        let other = ([0xf4; 16], 1);
        for key in [key, other] {
            state.closeout_work.observe(key, true);
            state
                .closeout_work
                .failed(key, &Status::failed_precondition("write-protected"));
            state.closeout_work.finish(key);
        }
        state.closeout_work.wake_tape(key.0);
        assert_eq!(state.closeout_work.claim(Instant::now(), false), vec![key]);
        // Clearance can precede publication of the refusal: polling the typed
        // operation after failure must still remember it.
        state
            .index_write()
            .unwrap()
            .record_media_readiness_transition(remanence_state::MediaReadinessTransitionInput {
                operation_id: id,
                phase: None,
                state: "ready".into(),
                dirty_scope: Some("none".into()),
                last_cdb_opcode: None,
                last_sense_raw: None,
                last_sense_key: None,
                last_asc: None,
                last_ascq: None,
                last_host_status: None,
                last_driver_status: None,
                target_status: None,
                transport_class: None,
                cancel_source: None,
                signal: None,
                evidence_path: None,
                last_error_json: None,
                quarantine_id: None,
            })
            .unwrap();
        state.closeout_work.failed(
            key,
            &crate::readiness_wait::ReadinessFence(id).status("opaque"),
        );
        state.closeout_work.wake_ready(&state).unwrap();
        state.closeout_work.finish(key);
        assert_eq!(state.closeout_work.claim(Instant::now(), false), vec![key]);
        assert!(state.closeout_work.entries.lock().unwrap()[&other].parked);
    }
}
