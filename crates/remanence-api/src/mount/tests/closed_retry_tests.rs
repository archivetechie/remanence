//! FinalizeTape retries after the first operation has closed (trial rung R3b-lite, the part
//! that does not wait for L1-W).
//!
//! These tests establish behaviour and decide none. Each one takes the operator's original
//! request through production admission over a typed library runtime until the tape holds a
//! native Prepared frame, the library log holds its `ManualFinalizationAccepted` record and the
//! operation is started. It then closes the operation, restarts, and sends the retry through
//! `manual_finalize_tape`, the entry point the gRPC service uses.
//!
//! A same-key retry whose operation has closed is refused by `ensure_manual_operation_continuable`.
//! Its refusal is pinned by its exact text, which starts with the countable code,
//! `ClosedOperationRetry` (ruling 25). The refusal is counted: the library log gains one
//! `RefusalRecorded` naming the operation and the actor that retried, and nothing else.
//!
//! A new-key request is refused by the retained intent, whatever became of the first operation.
//!
//! The daemon's startup never closes an operation: the only caller of the closing step,
//! `StateHandle::startup_replay`, is test code. `Closure::Restart` therefore builds the
//! restart-lost state by calling it on the reopened root, as the state crate's own tests do, and
//! `Closure::Open` is what a real restart leaves.
//!
//! The last two sections drive that open operation to its end. Each kills a first incarnation of
//! the daemon on a tokio runtime of its own, with the model drive actor's reply unanswered,
//! reopens the root as a second incarnation, and sends the same-key `FinalizeTape` RPC. They pin
//! the native frame counts, the operation's event chain, the drive commands each incarnation
//! received, and what the caller polls, for a kill after the acceptance, after the dispatch,
//! after two progress steps, and after the native seal.

use super::*;
use crate::tests::refusal_tests::{assert_grew_by, mark, refusals, RecordedRefusal};
use crate::tests::TypedRoot;
use remanence_state::library_log as lib;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

const BLOCK_SIZE: u32 = 256 * 1024;
const TAPE: TapeUuid = [0xB3; 16];
const LABEL: &str = "R3B001L9";
const REASON: &str = "close out the tape";

/// What became of the first operation before the daemon restarted.
#[derive(Clone, Copy, Debug)]
enum Closure {
    /// The restart's closing step ran: `Failed { summary: "daemon_restart" }`.
    Restart,
    /// The worker recorded `Failed` for a real attempt.
    Failed,
    /// The worker recorded `CompletionUnknown`.
    CompletionUnknown,
    /// Nothing closed it. The started operation is still open after the restart.
    Open,
}

/// The daemon's state after the restart, with the drive pool's command channels held so a test
/// can prove that no media command was sent.
struct Live {
    _handle: Option<remanence_state::StateHandle>,
    state: ApiState,
    drive_rx: tokio::sync::mpsc::Receiver<crate::write_owner::DriveCommand>,
    changer_rx: tokio::sync::mpsc::Receiver<crate::write_owner::ChangerCommand>,
}

impl Live {
    /// No drive command and no changer command reached the pool.
    fn assert_no_media_command(&mut self) {
        assert!(
            matches!(
                self.drive_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "the request sent a drive command"
        );
        assert!(
            matches!(
                self.changer_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "the request sent a changer command"
        );
    }
}

/// The operator's request under idempotency key `key`.
fn admission(key: u128) -> ManualFinalizeTapeAdmission {
    ManualFinalizeTapeAdmission {
        candidate_operation_id: Uuid::from_u128(0xB3_0000 + key),
        actor: remanence_state::AuditActor::User("operator@example.invalid".to_string()),
        actor_fingerprint: "sha256:r3b-operator".to_string(),
        idempotency_key: Uuid::from_u128(key),
        request_fingerprint: [0xB3; 32],
        tape_uuid: TAPE,
        expected_pool_id: None,
        reason: REASON.to_string(),
    }
}

/// The original request, through the calls `manual_finalize_tape` makes before it launches its
/// worker: the no-motion admission, the preflight that publishes the native Prepared frame and
/// the typed acceptance, and the attempt's `Started` record. The worker is not launched, because
/// it would race the closure each test installs.
fn admit_first_request(
    state: &ApiState,
    admission: ManualFinalizeTapeAdmission,
) -> crate::write_owner::ManualFinalizeTapeActorRequest {
    let mut index = state.catalog.write_handle();
    let (tape, mut request) =
        prepare_manual_finalize_request(&index, admission, |pool_id| state.pool_config(pool_id))
            .expect("no-motion admission");
    let accepted = crate::write_owner::preflight_manual_finalize_cancellable(
        &mut index,
        crate::write_owner::ManualFinalizePreflightConfig {
            authority: crate::write_owner::TerminalAuthority::from_api(state).unwrap(),
            mounted_endpoint: None,
            checkpoint_journal_dir: state.checkpoint_journal_dir.as_path(),
        },
        tape.voltag.as_deref(),
        &mut request,
        None,
        || false,
    )
    .expect("preflight publishes the Prepared frame and the acceptance");
    assert!(accepted.is_none(), "the physical tail is still to run");
    drop(index);
    drop(publish_finalize_attempt(state, &request).expect("started"));
    request
}

/// Reopen the root as a restarted daemon does. Only `closing_step` runs
/// `StateHandle::startup_replay`. The rest is the sequence `rem-daemon` follows for a read-only
/// service: the library runtime under the projection client, then `ApiState::new_with_config`
/// over it. The drive-pool constructor repeats that constructor's recovery calls and needs
/// hardware discovery, so these tests attach a channel-only pool afterwards.
fn restart(
    typed: &TypedRoot,
    closing_step: bool,
) -> (Option<remanence_state::StateHandle>, ApiState) {
    restart_with_config(typed, closing_step, &typed.config())
}

/// Reopen as the daemon does with the caller's pool configuration.
fn restart_with_config(
    typed: &TypedRoot,
    closing_step: bool,
    config: &remanence_state::RemConfig,
) -> (Option<remanence_state::StateHandle>, ApiState) {
    if closing_step {
        let mut handle =
            remanence_state::StateHandle::open_with_config(typed.paths().clone(), config.clone())
                .expect("reopen the state root");
        let report = handle.startup_replay().expect("startup replay");
        assert_eq!(report.lost_operations_marked, 1, "the started finalize");
        let state = ApiState::new_with_config(handle.projection_client(), config)
            .expect("service state after the restart");
        return (Some(handle), state);
    }
    let paths = typed.paths();
    let runtime = remanence_state::library_runtime::LibraryRuntime::open(
        paths,
        config.audit.fsync,
        Arc::new(remanence_state::StateLockGuard::acquire(&paths.state_dir).unwrap()),
    )
    .expect("reopen the library runtime");
    let catalog = remanence_state::projector::open(&paths.sqlite_path)
        .expect("reopen the catalog")
        .with_library_runtime(runtime);
    let state =
        ApiState::new_with_config(catalog, config).expect("service state after the restart");
    (None, state)
}

/// The tape's native log holds one committed object and nothing else, where the root keeps
/// tape logs. Returns that directory.
fn install_checkpoint(typed: &TypedRoot) -> PathBuf {
    let tapes = typed.paths().journal_dir.join("tapes");
    remanence_state::authority::fixtures::AuthorityBuilder::new(&tapes, TAPE)
        .build()
        .expect("open checkpoint journal")
        .append(&manual_finalize_checkpoint(TAPE, BLOCK_SIZE))
        .expect("append checkpoint");
    tapes
}

/// Give the service state a drive pool whose command channels the test holds.
fn attach_pool(
    state: &mut ApiState,
) -> (
    tokio::sync::mpsc::Receiver<crate::write_owner::DriveCommand>,
    tokio::sync::mpsc::Receiver<crate::write_owner::ChangerCommand>,
) {
    let (drive_tx, drive_rx) = tokio::sync::mpsc::channel(2);
    let (changer_tx, changer_rx) = tokio::sync::mpsc::channel(2);
    state.drive_pool = Some(crate::write_owner::DrivePool::new(
        changer_tx,
        std::collections::HashMap::from([(0x0101, drive_tx)]),
        Arc::new(std::collections::HashMap::from([(
            0x0101,
            AtomicBool::new(false),
        )])),
    ));
    (drive_rx, changer_rx)
}

/// Take the first request to RUNNING, close it as `closure` says, restart, and return the
/// daemon's new state, over an unsealed tape whose first operation has become `closure`.
fn finalize_then_restart(
    closure: Closure,
) -> (
    TypedRoot,
    ManualFinalizeTapeAdmission,
    crate::write_owner::ManualFinalizeTapeActorRequest,
    Live,
) {
    let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let first = admission(0x01);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let request = admit_first_request(&state, first.clone());
    match closure {
        Closure::Restart | Closure::Open => {}
        Closure::Failed => record_finalize_attempt(
            &state,
            &request,
            lib::OperationEvent::Failed(lib::Failed {
                retry: None,
                refusal_code: None,
                summary: format!(
                    "drive actor is unavailable; finalization operation {} remains incomplete",
                    request.candidate_operation_id
                )
                .as_str()
                .into(),
            }),
            None,
        )
        .expect("record Failed"),
        Closure::CompletionUnknown => record_finalize_attempt(
            &state,
            &request,
            lib::OperationEvent::CompletionUnknown(lib::Unit),
            None,
        )
        .expect("record CompletionUnknown"),
    }
    drop(state);
    typed.release();

    let (handle, mut state) = restart(&typed, matches!(closure, Closure::Restart));
    let (drive_rx, changer_rx) = attach_pool(&mut state);
    let live = Live {
        _handle: handle,
        state,
        drive_rx,
        changer_rx,
    };
    (typed, first, request, live)
}

/// Every file under `dir` with its bytes.
fn files_under(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, into: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, into);
            } else {
                into.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(dir, &mut files);
    files
}

/// Every byte a request could have appended to: the library log and the native tape logs.
pub(super) fn durable_bytes(paths: &remanence_state::StatePaths) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = files_under(&paths.audit_dir);
    files.extend(files_under(&paths.journal_dir.join("tapes")));
    files
}

/// What the tape's native log holds about finalization, counted from its decoded frames.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct NativeFinalization {
    prepared: usize,
    decided: usize,
    retired: usize,
    sealed: usize,
}

fn native_finalization(paths: &remanence_state::StatePaths) -> NativeFinalization {
    native_finalization_for(paths, TAPE)
}

/// Count finalization frames for the caller's tape, without opening a competing authority.
pub(super) fn native_finalization_for(
    paths: &remanence_state::StatePaths,
    tape: TapeUuid,
) -> NativeFinalization {
    use remanence_state::tape_log::{codec::Decoder, store::Header, Record};
    let directory = paths.journal_dir.join("tapes");
    let path = remanence_state::TapeAuthority::list_entries(&directory)
        .unwrap()
        .into_iter()
        .find(|path| remanence_state::TapeAuthority::entry_tape_uuid(path).unwrap() == tape)
        .expect("native tape log");
    let mut file = std::fs::File::open(path).unwrap();
    Header::decode(&mut file, tape).unwrap();
    let offset = std::io::Seek::stream_position(&mut file).unwrap();
    let mut decoder = Decoder::at(file, 1, offset);
    let mut counts = NativeFinalization {
        prepared: 0,
        decided: 0,
        retired: 0,
        sealed: 0,
    };
    while let Some(frame) = decoder.next_frame().unwrap() {
        match frame.record {
            Record::FinalizationPrepared(_) => counts.prepared += 1,
            Record::FinalizationDecided(_) => counts.decided += 1,
            Record::FinalizationPreparationRetired { .. } => counts.retired += 1,
            Record::Sealed { .. } => counts.sealed += 1,
            _ => {}
        }
    }
    counts
}

/// The operation's events as the library log recorded them, oldest first. Call after every state
/// built from the root is dropped.
async fn events_of(typed: &mut TypedRoot, operation: Uuid) -> Vec<lib::OperationEvent> {
    typed
        .operation_chain(operation)
        .await
        .into_iter()
        .map(|link| link.event)
        .collect()
}

/// The first operation's events, from its request.
async fn first_operation_events(
    typed: &mut TypedRoot,
    request: &crate::write_owner::ManualFinalizeTapeActorRequest,
) -> Vec<lib::OperationEvent> {
    events_of(typed, request.candidate_operation_id).await
}

/// The precondition every same-key test shares: the typed acceptance is authenticated and
/// applied, so a refusal below comes from the continuation guard and not from an earlier gate.
fn assert_acceptance_applied(live: &Live, admission: &ManualFinalizeTapeAdmission) {
    let accepted = live
        .state
        .catalog
        .library_runtime()
        .expect("typed runtime")
        .manual_acceptance_by_request(
            &admission.actor_fingerprint,
            *admission.idempotency_key.as_bytes(),
        )
        .expect("acceptance lookup")
        .expect("the first request left a typed acceptance");
    assert_eq!(
        accepted.status,
        remanence_state::library_runtime::terminal::ManualAcceptanceStatus::Applied
    );
}

/// Retry the original request under its own key. Checks that it is refused as a failed
/// precondition, that the native logs did not change, that the library log only grew, by the one
/// record that counts the refusal, and that no media command was sent. Returns the refusal so the
/// caller can pin its text.
async fn same_key_retry_is_refused(
    typed: &TypedRoot,
    admission: &ManualFinalizeTapeAdmission,
    live: &mut Live,
) -> Status {
    assert_acceptance_applied(live, admission);
    let before = durable_bytes(typed.paths());
    let counted = mark(&typed.paths().sqlite_path);
    let error = manual_finalize_tape(&live.state, admission.clone())
        .await
        .expect_err("a closed operation admits no same-key retry");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition, "{error}");
    let after = durable_bytes(typed.paths());
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "the refused retry created no file"
    );
    for (path, was) in &before {
        if path.starts_with(&typed.paths().audit_dir) {
            assert!(
                after[path].starts_with(was),
                "{}: the library log is append-only",
                path.display()
            );
        } else {
            assert_eq!(
                &after[path],
                was,
                "{}: a native log changed",
                path.display()
            );
        }
    }
    assert_grew_by(&typed.paths().sqlite_path, &counted, 1);
    live.assert_no_media_command();
    error
}

/// The refusal is the code's, and the library log counts it once, naming the first operation and
/// the actor that retried.
async fn assert_closed_retry_counted(
    typed: &mut TypedRoot,
    admission: &ManualFinalizeTapeAdmission,
    request: &crate::write_owner::ManualFinalizeTapeActorRequest,
    refusal: &Status,
) {
    assert!(
        refusal
            .message()
            .starts_with("tape provisioning conflict: ClosedOperationRetry: "),
        "{refusal}"
    );
    let log = typed.refusal_log();
    assert_eq!(
        refusals(&log),
        [RecordedRefusal::expect(
            3,
            "ClosedOperationRetry",
            "operation",
            Some(*request.candidate_operation_id.as_bytes()),
            admission.actor.clone(),
        )]
    );
    assert_eq!(log.last().unwrap().0, lib::Tag::Diagnostic);
}

/// A restart closed the started finalize as `Failed { summary: "daemon_restart" }`.
#[tokio::test]
async fn same_key_retry_after_failed_daemon_restart_is_refused_with_nothing_written() {
    let (mut typed, admission, request, mut live) = finalize_then_restart(Closure::Restart);
    let error = same_key_retry_is_refused(&typed, &admission, &mut live).await;
    assert_eq!(
        error.message(),
        "tape provisioning conflict: ClosedOperationRetry: manual operation cannot resume from \
         Some(Failed(Failed { retry: None, refusal_code: None, \
         summary: TextValue(\"daemon_restart\") })); \
         no permitted same-operation completion transition"
    );
    drop((live, error.clone()));
    assert!(matches!(
        first_operation_events(&mut typed, &request).await.as_slice(),
        [
            lib::OperationEvent::Accepted(_),
            lib::OperationEvent::Started(_),
            lib::OperationEvent::Failed(failed),
        ] if failed.summary.as_str() == "daemon_restart"
    ));
    assert_closed_retry_counted(&mut typed, &admission, &request, &error).await;
}

/// A real attempt failed and the worker recorded `Failed`.
#[tokio::test]
async fn same_key_retry_after_plain_failed_is_refused_with_nothing_written() {
    let (mut typed, admission, request, mut live) = finalize_then_restart(Closure::Failed);
    let error = same_key_retry_is_refused(&typed, &admission, &mut live).await;
    assert_eq!(
        error.message(),
        format!(
            "tape provisioning conflict: ClosedOperationRetry: manual operation cannot resume from \
             Some(Failed(Failed {{ retry: None, refusal_code: None, \
             summary: TextValue(\"drive actor is unavailable; finalization operation {} \
             remains incomplete\") }})); no permitted same-operation completion transition",
            request.candidate_operation_id
        )
    );
    drop((live, error.clone()));
    assert!(matches!(
        first_operation_events(&mut typed, &request).await.as_slice(),
        [
            lib::OperationEvent::Accepted(_),
            lib::OperationEvent::Started(_),
            lib::OperationEvent::Failed(failed),
        ] if failed.summary.as_str().starts_with("drive actor is unavailable")
    ));
    assert_closed_retry_counted(&mut typed, &admission, &request, &error).await;
}

/// The worker could not tell whether the physical work completed.
#[tokio::test]
async fn same_key_retry_after_completion_unknown_is_refused_with_nothing_written() {
    let (mut typed, admission, request, mut live) =
        finalize_then_restart(Closure::CompletionUnknown);
    let error = same_key_retry_is_refused(&typed, &admission, &mut live).await;
    assert_eq!(
        error.message(),
        "tape provisioning conflict: ClosedOperationRetry: manual operation cannot resume from \
         Some(CompletionUnknown(Unit)); no permitted same-operation completion transition"
    );
    drop((live, error.clone()));
    assert!(matches!(
        first_operation_events(&mut typed, &request)
            .await
            .as_slice(),
        [
            lib::OperationEvent::Accepted(_),
            lib::OperationEvent::Started(_),
            lib::OperationEvent::CompletionUnknown(_),
        ]
    ));
    assert_closed_retry_counted(&mut typed, &admission, &request, &error).await;
}

/// Not a closed state, and the one a real restart leaves: the daemon's own startup runs no
/// closing step, so the started operation stays open and the continuation guard lets the retry
/// through. This pins the guard's answer at the admission step, `prepare_manual_finalize_request`,
/// and sends nothing past it.
#[tokio::test]
async fn same_key_retry_after_a_restart_that_closes_nothing_passes_the_continuation_guard() {
    let (mut typed, admission, request, live) = finalize_then_restart(Closure::Open);
    assert_acceptance_applied(&live, &admission);
    live.state
        .catalog
        .library_runtime()
        .expect("typed runtime")
        .ensure_manual_operation_continuable(
            *request.candidate_operation_id.as_bytes(),
            admission.actor.clone(),
        )
        .expect("an open operation may resume");
    let index = live.state.catalog.write_handle();
    let (_, retry) = prepare_manual_finalize_request(&index, admission, |pool_id| {
        live.state.pool_config(pool_id)
    })
    .expect("the retry is admitted");
    assert_eq!(retry.candidate_operation_id, request.candidate_operation_id);
    drop((index, live));
    assert!(matches!(
        first_operation_events(&mut typed, &request)
            .await
            .as_slice(),
        [
            lib::OperationEvent::Accepted(_),
            lib::OperationEvent::Started(_)
        ]
    ));
}

/// D2. A new idempotency key for the same tape, after a restart, whatever became of the first
/// operation, is refused with `AlreadyExists` by `validate_manual_finalize_intent`: the tape's
/// retained Prepared and Decided intent belongs to the first request. The refusal comes before
/// any operation is minted, so nothing is written and no media command is sent. The tape stays
/// unsealed, with one Prepared and one Decided frame and one acceptance. Neither retry can finish
/// it until L2 gives the retained intent a resolution.
#[tokio::test]
async fn new_key_finalize_after_a_restart_is_refused_as_a_foreign_owner_and_changes_nothing() {
    for closure in [
        Closure::Restart,
        Closure::Failed,
        Closure::CompletionUnknown,
        Closure::Open,
    ] {
        let (mut typed, _first, request, mut live) = finalize_then_restart(closure);
        let second = admission(0x02);
        let events_before = durable_bytes(typed.paths());
        let native_before = native_finalization(typed.paths());
        assert_eq!(
            native_before,
            NativeFinalization {
                prepared: 1,
                decided: 1,
                retired: 0,
                sealed: 0,
            },
            "{closure:?}: the first request left its retained intent"
        );

        let error = manual_finalize_tape(&live.state, second.clone())
            .await
            .expect_err("a new key cannot take over the retained intent");
        // L1-W: RetainedIntentForeignOwner
        assert_eq!(
            error.code(),
            tonic::Code::AlreadyExists,
            "{closure:?}: {error}"
        );
        assert_eq!(
            error.message(),
            "pending terminal finalization belongs to a different exact request",
            "{closure:?}"
        );
        assert_eq!(
            durable_bytes(typed.paths()),
            events_before,
            "{closure:?}: the refused request appended no record"
        );
        live.assert_no_media_command();
        assert_eq!(
            native_finalization(typed.paths()),
            native_before,
            "{closure:?}: one Prepared, one Decided, no seal"
        );
        let paths = typed.paths().clone();
        drop((live, error));

        let acceptances = typed
            .records(|record| (record.tag() == lib::Tag::ManualFinalizationAccepted).then_some(()))
            .await;
        assert_eq!(acceptances.len(), 1, "{closure:?}: no second acceptance");
        let operations = typed
            .records(|record| {
                let value = record.operation_recorded()?;
                Some((
                    value.operation,
                    matches!(&value.event, lib::OperationEvent::Accepted(accepted)
                        if accepted.idempotency_key == Some(*second.idempotency_key.as_bytes())),
                ))
            })
            .await;
        assert!(
            operations.iter().all(|(operation, second_key)| !second_key
                && *operation == *request.candidate_operation_id.as_bytes()),
            "{closure:?}: the new key minted no operation: {operations:?}"
        );
        assert_eq!(
            native_finalization(&paths),
            native_before,
            "{closure:?}: the tape is not sealed"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The open operation, driven to completion by the same-key retry.
//
// The restart shape: the first incarnation runs on a tokio runtime of its own, with the model
// drive actor still holding the reply to the worker's `FinalizeTape` command. Dropping that
// runtime cancels the worker where it awaits, so no failure handler runs and nothing is recorded
// for the operation, as for a killed process. The second incarnation is `restart(.., false)`: a
// new library runtime and a new `ApiState` over the same state root, with no closing step.
// ---------------------------------------------------------------------------------------------

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// Load the tape into the drive in bay 0x0101 and give the state a pool whose channels the test
/// holds, so the finalize worker routes to the model drive actor.
fn load_drive(
    state: &mut ApiState,
) -> (
    tokio::sync::mpsc::Receiver<crate::write_owner::DriveCommand>,
    tokio::sync::mpsc::Receiver<crate::write_owner::ChangerCommand>,
) {
    state.default_library_serial = Some(Arc::new("LIB001".to_string()));
    let mut loaded = drive_bay(0x0101, true, Some(LABEL));
    loaded.source_slot = None;
    state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
        crate::live_status::LibrarySnapshot {
            report: remanence_library::DiscoveryReport {
                unattached_drives: Vec::new(),
                complete: true,
                incomplete_evidence: Vec::new(),
                libraries: vec![test_library(vec![loaded], vec![])],
                warnings: Vec::new(),
            },
            captured_at: time::OffsetDateTime::UNIX_EPOCH,
        },
    ))));
    attach_pool(state)
}

/// What sutradhara's caller sends: the `FinalizeTape` RPC under idempotency key `key`.
async fn finalize_rpc(state: &ApiState, key: Uuid) -> Result<pb::TapeFinalization, Status> {
    finalize_rpc_with_reason(state, key, REASON).await
}

/// The `FinalizeTape` RPC under idempotency key `key`, with the operator's `reason`.
async fn finalize_rpc_with_reason(
    state: &ApiState,
    key: Uuid,
    reason: &str,
) -> Result<pb::TapeFinalization, Status> {
    <crate::CatalogService as crate::pb::catalog_server::Catalog>::finalize_tape(
        &state.catalog_service(),
        tonic::Request::new(pb::FinalizeTapeRequest {
            tape_uuid: TAPE.to_vec(),
            expected_pool_id: None,
            reason: reason.to_string(),
            idempotency_key: Some(pb::IdempotencyKey {
                value: key.as_bytes().to_vec(),
            }),
        }),
    )
    .await
    .map(tonic::Response::into_inner)
}

/// What the caller polls: the `GetOperation` state of the finalize operation.
async fn operation_state(state: &ApiState, operation: Uuid) -> i32 {
    <crate::DaemonService as crate::pb::daemon_server::Daemon>::get_operation(
        &state.daemon_service(),
        tonic::Request::new(pb::GetOperationRequest {
            operation_id: operation.as_bytes().to_vec(),
        }),
    )
    .await
    .expect("GetOperation")
    .into_inner()
    .state
}

/// What the caller polls: the tape's terminal-finalization status.
async fn tape_finalization(state: &ApiState) -> pb::TapeFinalization {
    <crate::CatalogService as crate::pb::catalog_server::Catalog>::get_tape_finalization(
        &state.catalog_service(),
        tonic::Request::new(pb::GetTapeFinalizationRequest {
            tape_uuid: TAPE.to_vec(),
        }),
    )
    .await
    .expect("GetTapeFinalization")
    .into_inner()
}

/// The model drive actor's physical tail. It applies at most `at_most` of the five progress
/// steps, each from the durable progress and each projected as the real actor projects it. A step
/// that is already durable is not applied again, so a resumed tail never redoes finished media
/// work. Returns the number applied.
fn advance_tail(state: &ApiState, operation: Uuid, at_most: usize) -> usize {
    use remanence_state::TerminalFinalizationProgress::*;
    let authority = remanence_state::TapeAuthority::open(
        state.checkpoint_journal_dir.as_ref(),
        TAPE,
        state.index_path.as_ref(),
    )
    .unwrap();
    let mut lease = authority.acquire_exclusive_for_terminal_recovery().unwrap();
    let mut applied = 0;
    for (before, after) in [
        (BeforeReplicaA, AfterReplicaA),
        (AfterReplicaA, AfterSeparationAb),
        (AfterSeparationAb, AfterReplicaB),
        (AfterReplicaB, AfterSeparationBc),
        (AfterSeparationBc, AfterReplicaC),
    ] {
        if applied == at_most {
            break;
        }
        let durable = lease
            .terminal_finalization_intent()
            .unwrap()
            .expect("the retained intent");
        if durable.progress != before {
            continue;
        }
        let current = lease.advance_terminal_finalization(before, after).unwrap();
        state
            .catalog
            .write_handle()
            .project_terminal_finalization(remanence_state::TerminalFinalizationProjectionInput {
                tape_uuid: TAPE,
                trigger: current.trigger,
                operation_id: Some(operation),
                progress: current.progress,
                edition_digest: current.edition_digest,
                layout_digest: current.layout.layout_digest,
                outcome: remanence_state::TerminalFinalizationOutcome::InProgress,
                updated_at_utc: None,
            })
            .unwrap();
        applied += 1;
    }
    applied
}

/// The production host-only suffix after replica C: it writes the native seal, projects it and
/// records the operation's `Finished`.
fn seal_tail(
    state: &ApiState,
    request: &mut crate::write_owner::ManualFinalizeTapeActorRequest,
) -> crate::write_owner::ManualFinalizeTapeActorReply {
    let mut index = state.catalog.write_handle();
    crate::write_owner::preflight_manual_finalize_tape(
        &mut index,
        crate::write_owner::ManualFinalizePreflightConfig {
            authority: crate::write_owner::TerminalAuthority::from_api(state).unwrap(),
            mounted_endpoint: None,
            checkpoint_journal_dir: state.checkpoint_journal_dir.as_path(),
        },
        Some(LABEL),
        request,
    )
    .expect("host completion")
    .expect("the post-replica-C suffix completes")
}

/// Where the first incarnation is killed.
#[derive(Clone, Copy, Debug)]
enum KillAt {
    /// The first RPC was accepted and the incarnation was killed as soon as it answered. The
    /// attempt's `Started` is already durable, because admission records it before it answers;
    /// whether the worker had reached its drive dispatch is not waited for.
    Accepted,
    /// The worker dispatched its `FinalizeTape` command and the drive did nothing.
    Dispatched,
    /// The drive finished this many of the five progress steps.
    Replicas(usize),
    /// The native seal is durable; the operation's `Finished` was never appended.
    SealWritten,
}

/// What the killed first incarnation left behind.
struct Killed {
    operation: Uuid,
    /// The library log and the SQL catalog as they stood just before the seal, for
    /// `KillAt::SealWritten`.
    before_seal: Option<BeforeSeal>,
}

/// The files a crash just after the native seal would have left unchanged.
struct BeforeSeal {
    library_log: BTreeMap<PathBuf, Vec<u8>>,
    sql: BTreeMap<PathBuf, Vec<u8>>,
}

/// The SQL catalog's files, the database and its journal sidecars, where they exist.
fn sql_files(paths: &remanence_state::StatePaths) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", paths.sqlite_path.display()));
        if let Ok(bytes) = std::fs::read(&path) {
            files.insert(path, bytes);
        }
    }
    files
}

/// Run the original request through the `FinalizeTape` RPC and the real worker up to its drive
/// dispatch, let the model drive actor do what `kill_at` says, and kill the incarnation with the
/// actor's reply unanswered. For a kill straight after the answer the dead incarnation still holds
/// the pool's command channels, so its worker finds a live drive actor and records nothing.
fn first_incarnation(typed: &mut TypedRoot, key: Uuid, kill_at: KillAt) -> Killed {
    let tapes = typed.paths().journal_dir.join("tapes");
    let paths = typed.paths().clone();
    let catalog = typed.catalog();
    let incarnation = runtime();
    let (before_seal, held) = incarnation.block_on(async move {
        let mut state = ApiState::new(catalog);
        state.checkpoint_journal_dir = Arc::new(tapes);
        let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
        let first = finalize_rpc(&state, key)
            .await
            .expect("the first RPC over a typed runtime is accepted");
        assert_eq!(
            first.outcome,
            pb::TapeFinalizationOutcome::Finalizing as i32
        );
        let operation = Uuid::from_slice(first.operation_id.as_deref().unwrap()).unwrap();
        if matches!(kill_at, KillAt::Accepted) {
            return (
                None,
                Box::new((drive_rx, changer_rx)) as Box<dyn std::any::Any>,
            );
        }
        let command = tokio::time::timeout(Duration::from_secs(5), drive_rx.recv())
            .await
            .expect("the worker dispatches to the drive")
            .expect("a drive command");
        let crate::write_owner::DriveCommand::FinalizeTape {
            mut request, reply, ..
        } = command
        else {
            panic!("the first drive command is the finalize");
        };
        assert_eq!(request.candidate_operation_id, operation);
        let mut before_seal = None;
        match kill_at {
            KillAt::Accepted => unreachable!(),
            KillAt::Dispatched => {}
            KillAt::Replicas(steps) => assert_eq!(advance_tail(&state, operation, steps), steps),
            KillAt::SealWritten => {
                assert_eq!(advance_tail(&state, operation, 5), 5);
                before_seal = Some(BeforeSeal {
                    library_log: files_under(&paths.audit_dir),
                    sql: sql_files(&paths),
                });
                seal_tail(&state, &mut request);
            }
        }
        assert!(matches!(
            drive_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            changer_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        (before_seal, Box::new(reply) as Box<dyn std::any::Any>)
    });
    // The kill: the worker is dropped where it awaits the actor's reply. No failure handler runs.
    drop(incarnation);
    drop(held);
    let operations = runtime().block_on(typed.records(|record| {
        let value = record.operation_recorded()?;
        (value.kind == lib::OperationKind::FinalizeTape).then_some(value.operation)
    }));
    assert!(
        operations.windows(2).all(|pair| pair[0] == pair[1]) && !operations.is_empty(),
        "one finalize operation: {operations:?}"
    );
    Killed {
        operation: Uuid::from_bytes(operations[0]),
        before_seal,
    }
}

/// The second incarnation: a new library runtime and `ApiState` over the same root. With
/// `startup_repair` the state is built as the daemon builds it, which replays the checkpoint
/// journals into SQL and repairs a missing terminal `Finished` before any request arrives.
/// Without it the state is only the runtime and the catalog, so a request is the first thing that
/// can repair.
fn second_incarnation(
    typed: &TypedRoot,
    startup_repair: bool,
) -> (
    ApiState,
    tokio::sync::mpsc::Receiver<crate::write_owner::DriveCommand>,
    tokio::sync::mpsc::Receiver<crate::write_owner::ChangerCommand>,
) {
    let mut state = if startup_repair {
        restart(typed, false).1
    } else {
        let paths = typed.paths();
        let runtime = remanence_state::library_runtime::LibraryRuntime::open(
            paths,
            true,
            Arc::new(remanence_state::StateLockGuard::acquire(&paths.state_dir).unwrap()),
        )
        .expect("reopen the library runtime");
        let catalog = remanence_state::projector::open(&paths.sqlite_path)
            .expect("reopen the catalog")
            .with_library_runtime(runtime);
        let mut state = ApiState::new(catalog);
        state.checkpoint_journal_dir = Arc::new(paths.journal_dir.join("tapes"));
        state
    };
    let (drive_rx, changer_rx) = load_drive(&mut state);
    (state, drive_rx, changer_rx)
}

/// Poll the registry until the operation reaches `want`.
async fn wait_for_state(state: &ApiState, operation: Uuid, want: pb::OperationState) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while state
            .operations
            .latest(&operation)
            .is_none_or(|status| status.state != want as i32)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the operation reaches its state");
}

/// The first request is killed after `replicas` progress steps with its operation open. The
/// same-key retry after the restart is driven to completion by the model drive actor.
fn check_open_retry_completes(kill_at: KillAt) {
    let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    install_checkpoint(&typed);
    let key = Uuid::from_u128(0xC1);
    let replicas = match kill_at {
        KillAt::Replicas(steps) => steps,
        _ => 0,
    };
    let Killed { operation, .. } = first_incarnation(&mut typed, key, kill_at);
    let (state, mut drive_rx, mut changer_rx) = second_incarnation(&typed, true);
    let incarnation = runtime();
    incarnation.block_on(async {
        // Nothing has closed the operation; the retry is the first thing the new incarnation sees.
        assert_eq!(
            operation_state(&state, operation).await,
            pb::OperationState::Running as i32,
            "{kill_at:?}: nothing closed the operation, so GetOperation still reports it open"
        );
        let retry = finalize_rpc(&state, key)
            .await
            .expect("the same-key retry is admitted");
        assert_eq!(
            retry.operation_id.as_deref(),
            Some(operation.as_bytes().as_slice())
        );
        assert_eq!(
            retry.outcome,
            pb::TapeFinalizationOutcome::Finalizing as i32
        );

        let command = tokio::time::timeout(Duration::from_secs(5), drive_rx.recv())
            .await
            .expect("the retry's worker dispatches to the drive")
            .expect("a drive command");
        let crate::write_owner::DriveCommand::FinalizeTape {
            mut request, reply, ..
        } = command
        else {
            panic!("the retry's drive command is the finalize");
        };
        assert_eq!(request.candidate_operation_id, operation);
        assert_eq!(
            advance_tail(&state, operation, 5),
            5 - replicas,
            "only the progress not already durable is applied"
        );
        let completed = seal_tail(&state, &mut request);
        assert_eq!(
            completed.projection.outcome,
            remanence_state::TerminalFinalizationOutcome::Finalized
        );
        reply.send(Ok(completed)).unwrap();
        wait_for_state(&state, operation, pb::OperationState::Succeeded).await;

        // What the caller sees.
        assert_eq!(
            operation_state(&state, operation).await,
            pb::OperationState::Succeeded as i32
        );
        let finalization = tape_finalization(&state).await;
        assert_eq!(
            finalization.outcome,
            pb::TapeFinalizationOutcome::Finalized as i32
        );
        assert_eq!(
            finalization.operation_id.as_deref(),
            Some(operation.as_bytes().as_slice())
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while state
                .drive_pool()
                .unwrap()
                .mounted_tape_uuids()
                .contains(&TAPE)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the worker releases the tape");
        let again = finalize_rpc(&state, key)
            .await
            .expect("a repeat of the key");
        assert_eq!(again.outcome, pb::TapeFinalizationOutcome::Finalized as i32);
        assert_eq!(
            again.operation_id.as_deref(),
            Some(operation.as_bytes().as_slice())
        );

        // No second media command, and one of each native finalization frame.
        assert!(matches!(
            drive_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            changer_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(
            native_finalization(typed.paths()),
            NativeFinalization {
                prepared: 1,
                decided: 1,
                retired: 0,
                sealed: 1,
            }
        );
    });
    drop((state, drive_rx, changer_rx));
    let events = incarnation.block_on(events_of(&mut typed, operation));
    assert!(
        matches!(
            events.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_),
                lib::OperationEvent::Finished(_),
            ]
        ),
        "{events:?}"
    );
}

/// The first RPC was accepted and the process was killed as soon as it answered. The same-key
/// retry after the restart completes and seals once.
#[test]
fn open_finalize_cut_after_acceptance_same_key_retry_completes_and_seals_once() {
    check_open_retry_completes(KillAt::Accepted);
}

/// The worker had dispatched to the drive, which did nothing before the kill.
#[test]
fn open_finalize_dispatched_same_key_retry_after_restart_completes_and_seals_once() {
    check_open_retry_completes(KillAt::Dispatched);
}

/// The drive was killed after two progress steps: the retry resumes from the durable progress and
/// does not redo them.
#[test]
fn open_finalize_same_key_retry_after_restart_resumes_from_durable_progress() {
    check_open_retry_completes(KillAt::Replicas(2));
}

/// Put the files back as they stood in `before`: the crash cut. Library-log files the snapshot
/// does not hold are removed; the native tape logs are not touched. With `keep_sql` false the SQL
/// catalog is removed instead of restored.
fn cut_back_to(paths: &remanence_state::StatePaths, before: &BeforeSeal, keep_sql: bool) {
    for path in files_under(&paths.audit_dir).keys() {
        if !before.library_log.contains_key(path) {
            std::fs::remove_file(path).unwrap();
        }
    }
    for (path, bytes) in &before.library_log {
        std::fs::write(path, bytes).unwrap();
    }
    for path in sql_files(paths).keys() {
        std::fs::remove_file(path).unwrap();
    }
    if keep_sql {
        for (path, bytes) in &before.sql {
            std::fs::write(path, bytes).unwrap();
        }
    }
}

/// How the second incarnation meets a tape whose seal is durable and whose operation is open.
#[derive(Clone, Copy, Debug)]
enum SealCut {
    /// The SQL catalog is lost; startup rebuilds it.
    Lost,
    /// The SQL catalog is as it was before the seal; startup projects the seal.
    Stale,
    /// The SQL catalog is stale and startup repair does not run, so only the retry can repair.
    StaleNoStartupRepair,
    /// The SQL catalog already projects the seal, and only the operation's `Finished` is missing;
    /// startup finishes the operation.
    Current,
    /// The SQL catalog already projects the seal, only `Finished` is missing, and startup repair
    /// does not run, so only the retry can finish the operation.
    CurrentNoStartupRepair,
}

/// What the production tail does between the native seal and the operation's `Finished`: project
/// the sealed native log into the SQL catalog. Run on the files after `cut_back_to` has restored
/// the catalog as it stood before the seal.
fn project_native_seal_into_sql(paths: &remanence_state::StatePaths) {
    let catalog = remanence_state::projector::open(&paths.sqlite_path).expect("reopen the catalog");
    let mut index = catalog.write_handle();
    let lease = remanence_state::TapeAuthority::open(
        paths.journal_dir.join("tapes"),
        TAPE,
        &paths.sqlite_path,
    )
    .expect("open the native tape log")
    .acquire_exclusive()
    .expect("lease the native tape log");
    let predates_the_seal = index
        .terminal_finalization(&TAPE)
        .expect("read the finalization row")
        .expect("the restored catalog holds the in-progress row");
    assert_ne!(
        predates_the_seal.outcome,
        remanence_state::TerminalFinalizationOutcome::Finalized,
        "the restored catalog predates the seal"
    );
    index
        .project_tape_log(&lease.freeze().expect("freeze the sealed log"))
        .expect("project the seal");
    let row = index
        .terminal_finalization(&TAPE)
        .expect("read the finalization row")
        .expect("the catalog holds the finalization row");
    assert_eq!(
        row.outcome,
        remanence_state::TerminalFinalizationOutcome::Finalized,
        "the catalog is current: it projects the seal"
    );
}

/// The first incarnation is killed after the native seal was written and before the operation's
/// `Finished` was appended. After the restart the same-key retry must not seal again.
fn check_seal_written_then_retry(cut: SealCut) {
    let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    install_checkpoint(&typed);
    let key = Uuid::from_u128(0xC2);
    let Killed {
        operation,
        before_seal,
    } = first_incarnation(&mut typed, key, KillAt::SealWritten);
    let before_seal = before_seal.expect("the files before the seal");
    let incarnation = runtime();

    // The kill came after the seal and its `Finished`. Cut the files back to before both, and
    // keep the native seal.
    let events = incarnation.block_on(events_of(&mut typed, operation));
    assert!(
        matches!(
            events.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_),
                lib::OperationEvent::Finished(_)
            ]
        ),
        "{events:?}"
    );
    cut_back_to(typed.paths(), &before_seal, !matches!(cut, SealCut::Lost));
    if matches!(cut, SealCut::Current | SealCut::CurrentNoStartupRepair) {
        project_native_seal_into_sql(typed.paths());
    }
    let events = incarnation.block_on(events_of(&mut typed, operation));
    assert!(
        matches!(
            events.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_)
            ]
        ),
        "the cut left the operation open: {events:?}"
    );
    let sealed = NativeFinalization {
        prepared: 1,
        decided: 1,
        retired: 0,
        sealed: 1,
    };
    assert_eq!(
        native_finalization(typed.paths()),
        sealed,
        "the seal is durable"
    );

    let repaired_at_startup = !matches!(
        cut,
        SealCut::StaleNoStartupRepair | SealCut::CurrentNoStartupRepair
    );
    let (state, mut drive_rx, mut changer_rx) = second_incarnation(&typed, repaired_at_startup);
    incarnation.block_on(async {
        assert_eq!(
            operation_state(&state, operation).await,
            if repaired_at_startup {
                pb::OperationState::Succeeded
            } else {
                pb::OperationState::Running
            } as i32,
            "{cut:?}: whether startup already finished the operation"
        );
        let retry = finalize_rpc(&state, key)
            .await
            .expect("the same-key retry is admitted");
        assert_eq!(
            retry.operation_id.as_deref(),
            Some(operation.as_bytes().as_slice())
        );
        assert_eq!(retry.outcome, pb::TapeFinalizationOutcome::Finalized as i32);
        assert_eq!(
            operation_state(&state, operation).await,
            pb::OperationState::Succeeded as i32
        );
        let finalization = tape_finalization(&state).await;
        assert_eq!(
            finalization.outcome,
            pb::TapeFinalizationOutcome::Finalized as i32
        );
        assert!(matches!(
            drive_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            changer_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(native_finalization(typed.paths()), sealed, "no second seal");
    });
    drop((state, drive_rx, changer_rx));
    let events = incarnation.block_on(events_of(&mut typed, operation));
    assert!(
        matches!(
            events.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_),
                lib::OperationEvent::Finished(_)
            ]
        ),
        "{events:?}"
    );
}

/// The SQL catalog was lost with the seal unprojected: startup rebuilds it and finishes the
/// operation, and the retry then finds it finished.
#[test]
fn same_key_retry_after_restart_between_seal_and_sql_projection_seals_once() {
    check_seal_written_then_retry(SealCut::Lost);
}

/// The SQL catalog is as it was before the seal: startup projects the seal and finishes the
/// operation, and the retry then finds it finished.
#[test]
fn same_key_retry_after_restart_with_stale_sql_seals_once() {
    check_seal_written_then_retry(SealCut::Stale);
}

/// Startup repair does not run, so the retry itself repairs the open operation of a sealed tape.
#[test]
fn same_key_retry_repairs_the_open_operation_of_a_sealed_tape_without_sealing_again() {
    check_seal_written_then_retry(SealCut::StaleNoStartupRepair);
}

/// The SQL catalog already projects the seal and `Finished` is absent: startup finishes the
/// operation, and the retry then finds it finished and does not seal again.
#[test]
fn same_key_retry_after_restart_with_current_sql_and_no_finished_seals_once() {
    check_seal_written_then_retry(SealCut::Current);
}

/// The SQL catalog already projects the seal, `Finished` is absent and startup repair does not
/// run: the retry itself finishes the operation and does not seal again.
#[test]
fn same_key_retry_finishes_a_sealed_tape_with_current_sql_without_sealing_again() {
    check_seal_written_then_retry(SealCut::CurrentNoStartupRepair);
}

// ---------------------------------------------------------------------------------------------
// The first request over a typed runtime (trial rung F1)
// ---------------------------------------------------------------------------------------------

/// The first `FinalizeTape` over a typed library runtime is accepted. Its preflight publishes the
/// native Prepared frame and the typed acceptance, then appends Decided. The admission path then
/// reads the tape's SQL finalization row to build its answer, so that row must exist when the
/// preflight returns: the lease's Decided snapshot has to be projected before the runtime's
/// `catch_up`, which replays only the Prepared view the acceptance refreshed.
///
/// The caller gets an operation and `GetTapeFinalization` shows the tape finalizing, where the
/// tree before this rung answered `Internal` ("durably accepted FinalizeTape has no
/// finalization projection") and made the operator retry.
#[test]
fn the_first_finalize_over_a_typed_runtime_is_accepted_and_reports_finalizing() {
    let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    install_checkpoint(&typed);
    let key = Uuid::from_u128(0xF1);
    let tapes = typed.paths().journal_dir.join("tapes");
    let catalog = typed.catalog();
    let incarnation = runtime();
    let (operation, unanswered) = incarnation.block_on(async move {
        let mut state = ApiState::new(catalog);
        state.checkpoint_journal_dir = Arc::new(tapes);
        let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
        let first = finalize_rpc(&state, key)
            .await
            .expect("the first FinalizeTape over a typed runtime is accepted");
        assert_eq!(
            first.outcome,
            pb::TapeFinalizationOutcome::Finalizing as i32
        );
        let operation =
            Uuid::from_slice(first.operation_id.as_deref().expect("an operation id")).unwrap();

        // What the caller polls.
        let polled = tape_finalization(&state).await;
        assert_eq!(
            polled.outcome,
            pb::TapeFinalizationOutcome::Finalizing as i32
        );
        assert_eq!(polled.operation_id, first.operation_id);
        assert_eq!(
            operation_state(&state, operation).await,
            pb::OperationState::Running as i32
        );

        // The worker dispatches the one finalize command, for that operation.
        let command = tokio::time::timeout(Duration::from_secs(5), drive_rx.recv())
            .await
            .expect("the worker dispatches to the drive")
            .expect("a drive command");
        let crate::write_owner::DriveCommand::FinalizeTape { request, reply, .. } = command else {
            panic!("the first drive command is the finalize");
        };
        assert_eq!(request.candidate_operation_id, operation);
        assert!(matches!(
            drive_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            changer_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        (operation, reply)
    });
    drop(incarnation);
    drop(unanswered);
    assert_eq!(
        native_finalization(typed.paths()),
        NativeFinalization {
            prepared: 1,
            decided: 1,
            retired: 0,
            sealed: 0,
        }
    );
    let events = runtime().block_on(events_of(&mut typed, operation));
    assert!(
        matches!(
            events.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_)
            ]
        ),
        "{events:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Two resumers of one open operation (trial rung F1)
//
// The first incarnation is killed with the operation open and the drive command unanswered. In
// the second, two paths can resume it: startup recovery (`service_closeout_obligation`) and the
// operator's same-key `FinalizeTape`. Each order must leave one owner, one drive command, one
// native seal and the chain `[Accepted, Started, Finished]`.
// ---------------------------------------------------------------------------------------------

/// Which resumer reaches the open operation first.
#[derive(Clone, Copy, Debug)]
enum FirstResumer {
    /// Startup recovery is held after its tape reservation, while it owns the tape-log lease, when
    /// the operator's same-key request arrives.
    Recovery,
    /// The operator's same-key request owns the tape's worker when recovery runs.
    Retry,
}

/// Take the one finalize command off the drive channel.
async fn take_finalize_command(
    drive_rx: &mut tokio::sync::mpsc::Receiver<crate::write_owner::DriveCommand>,
    operation: Uuid,
) -> (
    crate::write_owner::ManualFinalizeTapeActorRequest,
    tokio::sync::oneshot::Sender<Result<crate::write_owner::ManualFinalizeTapeActorReply, Status>>,
) {
    let command = tokio::time::timeout(Duration::from_secs(30), drive_rx.recv())
        .await
        .expect("a resumer dispatches to the drive")
        .expect("a drive command");
    let crate::write_owner::DriveCommand::FinalizeTape { request, reply, .. } = command else {
        panic!("the drive command is the finalize");
    };
    assert_eq!(request.candidate_operation_id, operation);
    (request, reply)
}

/// The model drive actor finishes the finalize it was sent: the five progress steps, the seal,
/// the reply.
fn finish_finalize_command(
    state: &ApiState,
    operation: Uuid,
    mut request: crate::write_owner::ManualFinalizeTapeActorRequest,
    reply: tokio::sync::oneshot::Sender<
        Result<crate::write_owner::ManualFinalizeTapeActorReply, Status>,
    >,
) {
    assert_eq!(advance_tail(state, operation, 5), 5);
    let completed = seal_tail(state, &mut request);
    assert_eq!(
        completed.projection.outcome,
        remanence_state::TerminalFinalizationOutcome::Finalized
    );
    reply.send(Ok(completed)).unwrap();
}

/// Hold the next startup recovery of `TAPE` after its tape reservation, with the tape-log lease
/// taken. Returns the signal that it is held and the key that lets it go on.
fn hold_startup_recovery() -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
    *STARTUP_TERMINAL_LEASE_HOOK
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some((
        TAPE,
        Box::new(move || {
            let _ = held_tx.send(());
            let _ = resume_rx.recv_timeout(Duration::from_secs(10));
        }),
    ));
    (held_rx, resume_tx)
}

fn check_two_resumers(first: FirstResumer) {
    check_two_resumers_after_stop(first, false);
}

/// Exercise the same ownership race after a real stopped attempt as well as a crash.
fn check_two_resumers_after_stop(first: FirstResumer, stopped: bool) {
    let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    install_checkpoint(&typed);
    let key = Uuid::from_u128(0xC3);
    let Killed { operation, .. } = first_incarnation(&mut typed, key, KillAt::Dispatched);
    let (state, mut drive_rx, mut changer_rx) = second_incarnation(&typed, true);
    let incarnation = runtime();
    incarnation.block_on(async {
        assert_eq!(
            operation_state(&state, operation).await,
            pb::OperationState::Running as i32,
            "{first:?}: nothing closed the operation"
        );
        if stopped {
            let retry = finalize_rpc(&state, key).await.unwrap();
            assert_eq!(
                retry.operation_id.as_deref(),
                Some(operation.as_bytes().as_slice())
            );
            let (_, reply) = take_finalize_command(&mut drive_rx, operation).await;
            drop(reply);
            wait_for_state(&state, operation, pb::OperationState::Failed).await;
            let chain = operation_events_in_log(typed.paths(), operation);
            assert!(
                matches!(
                    chain.as_slice(),
                    [
                        lib::OperationEvent::Accepted(_),
                        lib::OperationEvent::Started(_)
                    ]
                ),
                "stopped attempt leaves the race resumable: {chain:?}"
            );
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Ok(guard) = state.drive_pool().unwrap().reserve_tape(TAPE) {
                        drop(guard);
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        match first {
            FirstResumer::Recovery => {
                let (held_rx, resume_tx) = hold_startup_recovery();
                let recovery = tokio::spawn({
                    let state = state.clone();
                    async move { crate::mount::service_closeout_obligation(&state, TAPE, 1).await }
                });
                tokio::time::timeout(Duration::from_secs(5), held_rx)
                    .await
                    .expect("startup recovery takes the tape-log lease")
                    .expect("the hook reports");

                // The operator's same-key request meets recovery while it holds the lease: it
                // rejoins the operation or is told Busy by it, and starts nothing.
                let before = durable_bytes(typed.paths());
                let retry = tokio::time::timeout(Duration::from_secs(5), finalize_rpc(&state, key))
                    .await
                    .expect("the same-key request returns while recovery is held")
                    .expect("the same-key request is not refused");
                let rejoined = retry.outcome == pb::TapeFinalizationOutcome::Finalizing as i32
                    && retry.operation_id.as_deref() == Some(operation.as_bytes().as_slice());
                let busy_by_it = retry.outcome == pb::TapeFinalizationOutcome::Busy as i32
                    && retry.detail.contains(&operation.to_string());
                assert!(
                    rejoined || busy_by_it,
                    "the retry rejoins operation {operation} or is Busy by it: {retry:?}"
                );
                assert_eq!(
                    durable_bytes(typed.paths()),
                    before,
                    "the retry appended no record while recovery held the tape"
                );
                assert!(matches!(
                    drive_rx.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                ));

                resume_tx.send(()).expect("recovery is still held");
                let (request, reply) = take_finalize_command(&mut drive_rx, operation).await;
                finish_finalize_command(&state, operation, request, reply);
                recovery
                    .await
                    .expect("recovery task")
                    .expect("recovery completes");
            }
            FirstResumer::Retry => {
                let retry = finalize_rpc(&state, key)
                    .await
                    .expect("the same-key request is admitted");
                assert_eq!(
                    retry.outcome,
                    pb::TapeFinalizationOutcome::Finalizing as i32
                );
                assert_eq!(
                    retry.operation_id.as_deref(),
                    Some(operation.as_bytes().as_slice())
                );
                let (request, reply) = take_finalize_command(&mut drive_rx, operation).await;

                // Recovery runs while the retry's worker owns the tape: it is refused at its
                // reservation, before any read or write, and exits.
                let before = durable_bytes(typed.paths());
                let refused = crate::mount::service_closeout_obligation(&state, TAPE, 1).await;
                let refusal =
                    refused.expect_err("recovery is refused while the retry owns the tape");
                assert!(
                    crate::drive_pool::TapeBusy::from_status(&refusal).is_some(),
                    "recovery is refused as busy: {refusal}"
                );
                assert_eq!(
                    durable_bytes(typed.paths()),
                    before,
                    "the refused recovery appended no record"
                );
                assert!(matches!(
                    drive_rx.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                ));

                finish_finalize_command(&state, operation, request, reply);
                wait_for_state(&state, operation, pb::OperationState::Succeeded).await;
                tokio::time::timeout(Duration::from_secs(5), async {
                    while state
                        .drive_pool()
                        .unwrap()
                        .mounted_tape_uuids()
                        .contains(&TAPE)
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("the retry's worker releases the tape");

                // Recovery runs once more, after the seal: it finds the tape sealed and exits.
                let before = durable_bytes(typed.paths());
                crate::mount::service_closeout_obligation(&state, TAPE, 1)
                    .await
                    .expect("recovery of a sealed tape ends cleanly");
                assert_eq!(
                    durable_bytes(typed.paths()),
                    before,
                    "recovery of a sealed tape appended no record"
                );
            }
        }
        wait_for_state(&state, operation, pb::OperationState::Succeeded).await;
        assert_eq!(
            operation_state(&state, operation).await,
            pb::OperationState::Succeeded as i32
        );
        let finalization = tape_finalization(&state).await;
        assert_eq!(
            finalization.outcome,
            pb::TapeFinalizationOutcome::Finalized as i32
        );
        assert_eq!(
            finalization.operation_id.as_deref(),
            Some(operation.as_bytes().as_slice())
        );

        // One drive command across both resumers, no changer command, one of each native frame.
        assert!(
            tokio::time::timeout(Duration::from_millis(300), drive_rx.recv())
                .await
                .is_err(),
            "{first:?}: a second drive command reached the pool"
        );
        assert!(matches!(
            changer_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(
            native_finalization(typed.paths()),
            NativeFinalization {
                prepared: 1,
                decided: 1,
                retired: 0,
                sealed: 1,
            },
            "{first:?}: one seal"
        );
    });
    drop((state, drive_rx, changer_rx));
    let events = incarnation.block_on(events_of(&mut typed, operation));
    assert!(
        matches!(
            events.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_),
                lib::OperationEvent::Finished(_),
            ]
        ),
        "{first:?}: {events:?}"
    );
}

/// Startup recovery owns the tape, held after its reservation, when the operator's same-key
/// request arrives. The request rejoins the operation or is told Busy by it, recovery dispatches
/// the one tail, and the tape is sealed once.
#[test]
fn a_same_key_retry_during_startup_recovery_joins_it_and_the_tape_is_sealed_once() {
    check_two_resumers(FirstResumer::Recovery);
}

/// The operator's same-key request owns the tape's worker when recovery runs. Recovery is refused
/// while the worker runs and finds the tape sealed after it, and the tape is sealed once.
#[test]
fn startup_recovery_after_a_same_key_retry_is_refused_or_finds_the_tape_sealed() {
    check_two_resumers(FirstResumer::Retry);
}

// ---------------------------------------------------------------------------------------------
// One idempotency-conflict text (TIX.10)
//
// A same-key `FinalizeTape` whose request differs from the one the key is bound to is refused
// `AlreadyExists` with one text, on admission, on preflight and through the RPC. The tests assert
// the literal text, never the code's constant.
// ---------------------------------------------------------------------------------------------

#[test]
fn admission_of_a_same_key_request_with_another_fingerprint_is_refused_with_the_conflict_text() {
    let (typed, first, _request, live) = finalize_then_restart(Closure::Open);
    let before = durable_bytes(typed.paths());
    let mut changed = first;
    changed.request_fingerprint = [0xB4; 32];
    let index = live.state.catalog.write_handle();
    let error =
        prepare_manual_finalize_request(&index, changed, |pool_id| live.state.pool_config(pool_id))
            .expect_err("a changed request under a bound key is refused");
    drop(index);
    assert_eq!(error.code(), tonic::Code::AlreadyExists, "{error}");
    assert_eq!(
        error.message(),
        "FinalizeTape idempotency key is already bound to a different request"
    );
    assert_eq!(durable_bytes(typed.paths()), before, "admission wrote");
}

#[test]
fn preflight_of_a_same_key_request_with_another_fingerprint_is_refused_with_the_conflict_text() {
    let (typed, _first, mut request, live) = finalize_then_restart(Closure::Open);
    let before = durable_bytes(typed.paths());
    request.request_fingerprint = [0xB4; 32];
    let mut index = live.state.catalog.write_handle();
    let error = crate::write_owner::preflight_manual_finalize_cancellable(
        &mut index,
        crate::write_owner::ManualFinalizePreflightConfig {
            authority: crate::write_owner::TerminalAuthority::from_api(&live.state).unwrap(),
            mounted_endpoint: None,
            checkpoint_journal_dir: live.state.checkpoint_journal_dir.as_path(),
        },
        None,
        &mut request,
        None,
        || false,
    )
    .expect_err("a changed request under a bound key is refused");
    drop(index);
    assert_eq!(error.code(), tonic::Code::AlreadyExists, "{error}");
    assert_eq!(
        error.message(),
        "FinalizeTape idempotency key is already bound to a different request"
    );
    assert_eq!(durable_bytes(typed.paths()), before, "preflight wrote");
}

/// A crash after typed operation acceptance leaves path A to detect the conflicting body.
#[test]
fn preflight_conflicting_typed_acceptance_without_intent_uses_the_conflict_text() {
    let typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let mut index = state.catalog.write_handle();
    let (tape, mut request) = prepare_manual_finalize_request(&index, admission(0xF3), |pool_id| {
        state.pool_config(pool_id)
    })
    .expect("no-motion admission before the conflicting acceptance");
    let runtime = state.catalog.library_runtime().unwrap();
    runtime
        .begin_manual_finalization(
            lib::Accepted {
                actor_fingerprint: request.actor_fingerprint.as_str().into(),
                idempotency_key: Some(*request.idempotency_key.as_bytes()),
                request_fingerprint: [0xB4; 32],
                subject: Some(
                    Uuid::from_bytes(request.tape_uuid)
                        .to_string()
                        .as_str()
                        .into(),
                ),
            },
            request.actor.clone(),
        )
        .expect("plant only the conflicting OperationRecorded Accepted");
    assert!(runtime
        .manual_acceptance_by_request(
            &request.actor_fingerprint,
            *request.idempotency_key.as_bytes(),
        )
        .unwrap()
        .is_none());
    assert!(remanence_state::TapeAuthority::open(
        state.checkpoint_journal_dir.as_ref(),
        TAPE,
        index.path(),
    )
    .unwrap()
    .pending_finalization()
    .unwrap()
    .is_none());
    let before = durable_bytes(typed.paths());
    let error = crate::write_owner::preflight_manual_finalize_cancellable(
        &mut index,
        crate::write_owner::ManualFinalizePreflightConfig {
            authority: crate::write_owner::TerminalAuthority::from_api(&state).unwrap(),
            mounted_endpoint: None,
            checkpoint_journal_dir: state.checkpoint_journal_dir.as_path(),
        },
        tape.voltag.as_deref(),
        &mut request,
        None,
        || false,
    )
    .expect_err("a conflicting typed acceptance under a bound key is refused");
    drop(index);
    assert_eq!(error.code(), tonic::Code::AlreadyExists, "{error}");
    assert_eq!(
        error.message(),
        "FinalizeTape idempotency key is already bound to a different request"
    );
    assert_eq!(durable_bytes(typed.paths()), before, "preflight wrote");
}

#[test]
fn the_finalize_rpc_with_a_changed_reason_under_a_bound_key_is_refused_with_the_conflict_text() {
    let typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    install_checkpoint(&typed);
    let key = Uuid::from_u128(0xF2);
    let tapes = typed.paths().journal_dir.join("tapes");
    let catalog = typed.catalog();
    let incarnation = runtime();
    let held = incarnation.block_on(async move {
        let mut state = ApiState::new(catalog);
        state.checkpoint_journal_dir = Arc::new(tapes);
        let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
        finalize_rpc(&state, key)
            .await
            .expect("the first FinalizeTape is accepted");
        let command = tokio::time::timeout(Duration::from_secs(5), drive_rx.recv())
            .await
            .expect("the worker dispatches to the drive")
            .expect("a drive command");
        let crate::write_owner::DriveCommand::FinalizeTape { reply, .. } = command else {
            panic!("the first drive command is the finalize");
        };
        let error = finalize_rpc_with_reason(&state, key, "a different reason")
            .await
            .expect_err("the same key with another reason is refused");
        assert_eq!(error.code(), tonic::Code::AlreadyExists, "{error}");
        assert_eq!(
            error.message(),
            "FinalizeTape idempotency key is already bound to a different request"
        );
        assert!(
            matches!(
                drive_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "the refused request sent a drive command"
        );
        assert!(
            matches!(
                changer_rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "the refused request sent a changer command"
        );
        Box::new((reply, drive_rx, changer_rx)) as Box<dyn std::any::Any>
    });
    drop(incarnation);
    drop(held);
}

/// Shutdown leaves an admitted operation open; its readiness deadline closes the attempt
/// with the retained-intent explanation.
#[tokio::test]
async fn manual_finalize_attempt_ends_at_shutdown_or_its_readiness_deadline_with_the_intent_summary(
) {
    let typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
    let first = admission(0x4A01);
    let request = admit_first_request(&state, first.clone());
    let operation = request.candidate_operation_id;
    let admitted = operation_events_in_log(typed.paths(), operation);
    assert!(matches!(
        admitted.as_slice(),
        [
            lib::OperationEvent::Accepted(_),
            lib::OperationEvent::Started(_)
        ]
    ));
    let pool = state.drive_pool().unwrap().clone();
    let readiness_id = Uuid::from_bytes(
        state
            .catalog
            .library_runtime()
            .unwrap()
            .start_owned_operation(
                remanence_state::library_log::OperationKind::MediaReadinessWait,
                remanence_state::AuditActor::System,
            )
            .expect("start the typed readiness owner"),
    );
    let mut index = state.catalog.write_handle();
    index.set_readiness_observer(pool.readiness_wake.clone());
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id: readiness_id,
            run_id: None,
            endpoint: remanence_state::DriveKey::new("LIB001", 0x0101),
            changer_sg: None,
            drive_sg: None,
            drive_serial: None,
            barcode: Some(LABEL.into()),
            source_slot: None,
            media_generation: Some(9),
            media_family: Some(remanence_state::LtoGen::Lto9),
            phase: "session_open_short_probe".into(),
            state: "becoming_ready".into(),
            dirty_scope: Some("drive+tape".into()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .unwrap();
    index
        .record_media_readiness_transition(
            crate::startup_media_readiness::startup_media_readiness_probe_transition(
                readiness_id,
                &remanence_library::MediaReadiness::new(
                    remanence_library::MediaReadinessKind::BecomingReady {
                        ascq: 1,
                        media_initializing: false,
                    },
                ),
            ),
        )
        .unwrap();
    drop(index);
    state
        .readiness_shutdown
        .store(true, std::sync::atomic::Ordering::Release);
    let guard = pool.reserve_tape(TAPE).unwrap();
    let before_shutdown = durable_bytes(typed.paths());
    let shutdown_error = tokio::time::timeout(
        Duration::from_secs(2),
        run_manual_finalize_worker_with_budget(
            state.clone(),
            pool.clone(),
            guard.into(),
            request.clone(),
            Duration::from_secs(900),
        ),
    )
    .await
    .expect("shutdown ends the finalize attempt")
    .unwrap_err();
    assert!(crate::drive_pool::DaemonShuttingDown::is_status(
        &shutdown_error
    ));
    assert_eq!(operation_events_in_log(typed.paths(), operation), admitted);
    assert_eq!(durable_bytes(typed.paths()), before_shutdown);
    assert_eq!(
        state.operations.latest(&operation).unwrap().state,
        pb::OperationState::Running as i32
    );
    state
        .readiness_shutdown
        .store(false, std::sync::atomic::Ordering::Release);
    let guard = pool.reserve_tape(TAPE).unwrap();
    let deadline_error = run_manual_finalize_worker_with_budget(
        state.clone(),
        pool.clone(),
        guard.into(),
        request,
        Duration::from_millis(20),
    )
    .await
    .unwrap_err();
    assert!(deadline_error.message().contains("deadline expired"));
    assert!(deadline_error
        .message()
        .starts_with("this attempt stopped:"));
    assert!(deadline_error
        .message()
        .contains("The finalization stays accepted; the daemon retries when a drive is ready."));
    assert_eq!(
        state
            .index()
            .unwrap()
            .get_operation(&operation.to_string())
            .unwrap()
            .unwrap()
            .state,
        "running"
    );
    assert_eq!(
        state
            .index()
            .unwrap()
            .terminal_finalization(&TAPE)
            .unwrap()
            .unwrap()
            .outcome,
        remanence_state::TerminalFinalizationOutcome::InProgress
    );
    assert!(drive_rx.try_recv().is_err());
    assert!(changer_rx.try_recv().is_err());
}

/// The worker's retained-intent explanation preserves an actor's typed retry cause.
#[tokio::test]
async fn manual_finalize_attempt_keeps_the_actors_retry_cause_beside_the_intent_summary() {
    let typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
    let first = admission(0x4A02);
    let request = admit_first_request(&state, first.clone());
    let operation = request.candidate_operation_id;
    let admitted = operation_events_in_log(typed.paths(), operation);
    assert!(matches!(
        admitted.as_slice(),
        [
            lib::OperationEvent::Accepted(_),
            lib::OperationEvent::Started(_)
        ]
    ));
    let pool = state.drive_pool().unwrap().clone();
    let guard = pool.reserve_tape(TAPE).unwrap();
    let typed_attempt = tokio::spawn(run_manual_finalize_worker_with_budget(
        state.clone(),
        pool.clone(),
        guard.into(),
        request,
        Duration::from_secs(1),
    ));
    let command = tokio::time::timeout(Duration::from_secs(2), drive_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let crate::write_owner::DriveCommand::FinalizeTape { reply, .. } = command else {
        panic!("expected finalization dispatch");
    };
    reply
        .send(Err(crate::closeout_obligation::RetryCause::Operator
            .attach(Status::failed_precondition("opaque media refusal"))))
        .unwrap();
    let typed_error = tokio::time::timeout(Duration::from_secs(2), typed_attempt)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(
        typed_error.metadata().get("remanence-retry").unwrap(),
        "operator"
    );
    assert!(typed_error.message().starts_with("this attempt stopped:"));
    assert!(typed_error
        .message()
        .contains("The finalization stays accepted; the daemon retries when a drive is ready."));
    assert!(drive_rx.try_recv().is_err());
    assert!(changer_rx.try_recv().is_err());
}

/// Decode the operation's durable chain at a quiescent boundary while its runtime still
/// owns the writer lock. Each byte snapshot is validated by the library frame decoder.
pub(super) fn operation_events_in_log(
    paths: &remanence_state::StatePaths,
    operation: Uuid,
) -> Vec<lib::OperationEvent> {
    use lib::codec::{Decoder, Header, HEADER_LEN};
    let mut entries: Vec<_> = std::fs::read_dir(&paths.audit_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "remlib"))
        .collect();
    entries.sort();
    let mut records = Vec::new();
    for path in entries {
        let bytes = std::fs::read(path).unwrap();
        let mut cursor = std::io::Cursor::new(&bytes);
        let header = Header::decode(&mut cursor, bytes[10..26].try_into().unwrap()).unwrap();
        let mut decoder = Decoder::at(cursor, header.first_sequence, HEADER_LEN);
        let mut previous = header.previous_segment_hash;
        while let Some(frame) = decoder.next_frame().unwrap() {
            previous = lib::codec::verify(&frame, previous).unwrap();
            if let Some(record) = frame.record.operation_recorded() {
                if record.operation == *operation.as_bytes() {
                    records.push(record.event.clone());
                }
            }
        }
    }
    records
}

/// A crash leaves the accepted operation open: its fence holds startup, release resumes
/// once, and a further crash after replica C leaves only the host suffix for the retry.
#[test]
fn manual_finalize_left_open_by_a_crash_waits_out_its_fence_then_resumes_once_and_completes_after_c(
) {
    let pool = TapePoolConfig {
        scheme: None,
        id: "async-finalize".into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(BLOCK_SIZE),
        min_object_size_bytes: 0,
    };
    let (mut typed, handle, config) = pooled_manual_finalize_root(TAPE, LABEL, pool);
    drop(handle);
    let mut first = admission(0x4A03);
    first.expected_pool_id = Some("async-finalize".into());

    let incarnation = runtime();
    let (state, held, quarantine_id, operation) = incarnation.block_on(async {
        let (_, mut state) = restart_with_config(&typed, false, &config);
        let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
        let accepted = manual_finalize_tape(&state, first.clone())
            .await
            .expect("durable acceptance");
        let crate::write_owner::ManualFinalizeTapeResult::Accepted(accepted) = accepted else {
            panic!("durable admission must not return BUSY");
        };
        let operation = accepted.operation_id;
        assert_eq!(
            accepted.projection.outcome,
            remanence_state::TerminalFinalizationOutcome::InProgress
        );
        let command = tokio::time::timeout(Duration::from_secs(2), drive_rx.recv())
            .await
            .expect("first worker dispatches")
            .expect("first drive command");
        let crate::write_owner::DriveCommand::FinalizeTape { request, reply, .. } = command else {
            panic!("expected first finalization dispatch");
        };
        assert_eq!(request.candidate_operation_id, operation);
        let fence_config = crate::write_owner::TerminalWriteConfig {
            authority: crate::write_owner::TerminalAuthority::Typed(
                state.catalog.library_runtime().unwrap(),
            ),
            io_memory: crate::io_memory::IoMemoryReservation::new(1024).unwrap(),
            mounted_endpoint: None,
            checkpoint_journal_dir: state.checkpoint_journal_dir.as_ref().clone(),
        };
        let fence = crate::write_owner::raise_tape_io_fence(
            &mut state.catalog.write_handle(),
            &fence_config,
            remanence_state::TapeIoFenceInput {
                tape_uuid: TAPE,
                barcode: None,
                reason: "transfer_error".into(),
                evidence_json: None,
            },
            None,
            None,
        )
        .expect("raise typed transfer fence");
        assert!(state
            .catalog
            .write_handle()
            .tape_io_is_fenced(Some(&TAPE), None, None)
            .unwrap());
        assert!(drive_rx.try_recv().is_err());
        assert!(changer_rx.try_recv().is_err());
        (
            state,
            (reply, drive_rx, changer_rx),
            fence.quarantine_id,
            operation,
        )
    });
    // Kill with the actor's reply unanswered, so no failure handler closes the operation.
    drop(incarnation);
    drop((state, held));
    let open = runtime().block_on(events_of(&mut typed, operation));
    assert!(matches!(
        open.as_slice(),
        [
            lib::OperationEvent::Accepted(_),
            lib::OperationEvent::Started(_)
        ]
    ));

    let incarnation = runtime();
    let (state, held) = incarnation.block_on(async {
        let (_, mut state) = restart_with_config(&typed, false, &config);
        let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
        let before = files_under(state.checkpoint_journal_dir.as_ref());
        spawn_startup_terminal_recoveries(state.clone());
        state.closeout_work.wait_for_fence_refusal(TAPE).await;
        assert!(drive_rx.try_recv().is_err(), "fenced manual intent never relaunches");
        assert!(changer_rx.try_recv().is_err(), "fenced manual intent never moves media");
        assert_eq!(files_under(state.checkpoint_journal_dir.as_ref()), before,
            "fenced startup changed the native tape log");
        <crate::LibraryServiceApi as pb::library_service_server::LibraryService>::release_tape_io_fence(
            &state.library_service(), tonic::Request::new(pb::ReleaseTapeIoFenceRequest {
                force: false, quarantine_id, acknowledgment: "checked terminal intent".into(),
            }),
        ).await.expect("release the typed transfer fence");
        // Release wakes the already-running dispatcher; it resumes this operation once.
        let resumed = tokio::time::timeout(Duration::from_secs(2), drive_rx.recv())
            .await.expect("startup resumes manual intent").expect("resumed drive command");
        let crate::write_owner::DriveCommand::FinalizeTape { request, reply, .. } = resumed else {
            panic!("expected resumed finalization dispatch");
        };
        assert_eq!(request.candidate_operation_id, operation);
        assert_eq!(request.idempotency_key, first.idempotency_key);
        assert_eq!(request.actor_fingerprint, first.actor_fingerprint);
        assert_eq!(request.request_fingerprint, first.request_fingerprint);
        assert_eq!(request.expected_pool_id, first.expected_pool_id);
        assert_eq!(request.reason, first.reason);
        assert_eq!(state.index().unwrap().get_operation(&operation.to_string()).unwrap().unwrap().state, "running");
        assert!(tokio::time::timeout(Duration::from_millis(300), drive_rx.recv()).await.is_err(),
            "startup duplicated tail dispatch");
        assert!(changer_rx.try_recv().is_err(), "loaded-tape recovery moved media");
        assert_eq!(state.index().unwrap().terminal_finalization(&TAPE).unwrap().unwrap().progress,
            remanence_state::TerminalFinalizationProgress::BeforeReplicaA);
        assert_eq!(advance_tail(&state, operation, 5), 5);
        let after_c = state.index().unwrap().terminal_finalization(&TAPE).unwrap().unwrap();
        assert_eq!(after_c.progress, remanence_state::TerminalFinalizationProgress::AfterReplicaC);
        assert_eq!(after_c.outcome, remanence_state::TerminalFinalizationOutcome::InProgress);
        (state, (reply, drive_rx, changer_rx))
    });
    drop(incarnation);
    drop((state, held));
    assert_eq!(runtime().block_on(events_of(&mut typed, operation)), open);

    let incarnation = runtime();
    let state = incarnation.block_on(async {
        let (_, mut state) = restart_with_config(&typed, false, &config);
        let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
        // No startup dispatcher: the operator's retry alone completes the post-C suffix.
        let completed = manual_finalize_tape(&state, first.clone()).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(300), drive_rx.recv())
                .await
                .is_err(),
            "manual post-C API route reached drive actor"
        );
        assert!(
            changer_rx.try_recv().is_err(),
            "manual post-C API route reached changer actor"
        );
        let completed = completed.expect("manual API entry completes the post-C host suffix");
        let crate::write_owner::ManualFinalizeTapeResult::Accepted(completed) = completed else {
            panic!("manual host-only completion must retain durable acceptance");
        };
        assert_eq!(completed.operation_id, operation);
        assert_eq!(
            completed.projection.outcome,
            remanence_state::TerminalFinalizationOutcome::Finalized
        );
        assert_eq!(
            operation_state(&state, operation).await,
            pb::OperationState::Succeeded as i32
        );
        state
    });
    drop(incarnation);
    drop(state);
    assert_eq!(
        native_finalization(typed.paths()),
        NativeFinalization {
            prepared: 1,
            decided: 1,
            retired: 0,
            sealed: 1
        }
    );
    assert!(matches!(
        runtime()
            .block_on(events_of(&mut typed, operation))
            .as_slice(),
        [
            lib::OperationEvent::Accepted(_),
            lib::OperationEvent::Started(_),
            lib::OperationEvent::Finished(_),
        ]
    ));
}

/// Install the same typed readiness fence used by the existing worker-deadline test.
fn u7_readiness_fence(state: &ApiState) -> Uuid {
    let pool = state.drive_pool().unwrap();
    let readiness_id = Uuid::from_bytes(
        state
            .catalog
            .library_runtime()
            .unwrap()
            .start_owned_operation(
                remanence_state::library_log::OperationKind::MediaReadinessWait,
                remanence_state::AuditActor::System,
            )
            .expect("start the typed readiness owner"),
    );
    let mut index = state.catalog.write_handle();
    index.set_readiness_observer(pool.readiness_wake.clone());
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id: readiness_id,
            run_id: None,
            endpoint: remanence_state::DriveKey::new("LIB001", 0x0101),
            changer_sg: None,
            drive_sg: None,
            drive_serial: None,
            barcode: Some(LABEL.into()),
            source_slot: None,
            media_generation: Some(9),
            media_family: Some(remanence_state::LtoGen::Lto9),
            phase: "session_open_short_probe".into(),
            state: "becoming_ready".into(),
            dirty_scope: Some("drive+tape".into()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .unwrap();
    index
        .record_media_readiness_transition(
            crate::startup_media_readiness::startup_media_readiness_probe_transition(
                readiness_id,
                &remanence_library::MediaReadiness::new(
                    remanence_library::MediaReadinessKind::BecomingReady {
                        ascq: 1,
                        media_initializing: false,
                    },
                ),
            ),
        )
        .unwrap();
    drop(index);
    readiness_id
}

/// The exact stopped-attempt contract, including what the CLI's operation read sees.
async fn u7_assert_stopped(typed: &TypedRoot, state: &ApiState, operation: Uuid, reason: &str) {
    let chain = operation_events_in_log(typed.paths(), operation);
    assert!(
        matches!(
            chain.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_)
            ]
        ),
        "stopped attempt must leave the chain open: {chain:?}"
    );
    let live = state.operations.latest(&operation).unwrap();
    assert_eq!(live.state, pb::OperationState::Failed as i32);
    assert_eq!(live.error_summary, format!("this attempt stopped: {reason}. The finalization stays accepted; the daemon retries when a drive is ready. Rerun with the same idempotency key to follow it."));
    assert_eq!(
        live.progress.get("attempt").map(String::as_str),
        Some("stopped")
    );
    assert_eq!(
        operation_state(state, operation).await,
        pb::OperationState::Failed as i32
    );
    assert_eq!(tape_finalization(state).await.detail, live.error_summary);
    let stored = state
        .index()
        .unwrap()
        .get_operation(&operation.to_string())
        .unwrap()
        .unwrap();
    assert_eq!(stored.state, "running");
    assert!(stored.error_summary.is_none());
}

/// A readiness deadline stops only the attempt; either existing resumer completes in process.
async fn u7_deadline_then_resume(recovery: bool, rebuild: bool) {
    let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
    let first = admission(0x7A01);
    let request = admit_first_request(&state, first.clone());
    let operation = request.candidate_operation_id;
    let readiness = u7_readiness_fence(&state);
    let before = durable_bytes(typed.paths());
    let pool = state.drive_pool().unwrap().clone();
    let reservation = pool.reserve_tape(TAPE).unwrap();
    let error = run_manual_finalize_worker_with_budget(
        state.clone(),
        pool.clone(),
        reservation.into(),
        request,
        Duration::from_millis(20),
    )
    .await
    .unwrap_err();
    let reason = error
        .message()
        .strip_prefix("this attempt stopped: ")
        .unwrap_or(error.message())
        .split(". The finalization stays accepted;")
        .next()
        .unwrap();
    u7_assert_stopped(&typed, &state, operation, reason).await;
    assert!(reason.contains("deadline expired"));
    assert_eq!(
        durable_bytes(typed.paths()),
        before,
        "a deadline appends no library or native record"
    );
    assert!(drive_rx.try_recv().is_err());
    assert!(changer_rx.try_recv().is_err());
    // A new key still cannot replace this accepted intent.
    let other = manual_finalize_tape(&state, admission(0x7A02))
        .await
        .unwrap_err();
    assert_eq!(other.code(), tonic::Code::AlreadyExists);
    assert_eq!(durable_bytes(typed.paths()), before);
    let mut ready = crate::startup_media_readiness::startup_media_readiness_probe_transition(
        readiness,
        &remanence_library::MediaReadiness::new(remanence_library::MediaReadinessKind::Ready),
    );
    // Match the model actor's successful TUR in the named acceptance/rejoin precedent.
    ready.completion_evidence =
        Some((&remanence_library::outcome::CompletionEvidence::success()).into());
    state
        .catalog
        .write_handle()
        .record_media_readiness_transition(ready)
        .unwrap();
    drop(pool);
    if rebuild {
        drop((state, drive_rx, changer_rx));
        typed.release();
        for path in sql_files(typed.paths()).keys() {
            std::fs::remove_file(path).unwrap();
        }
        let reopened = second_incarnation(&typed, true);
        state = reopened.0;
        drive_rx = reopened.1;
        changer_rx = reopened.2;
        assert_eq!(
            operation_state(&state, operation).await,
            pb::OperationState::Running as i32,
            "empty-SQL rebuild keeps the stopped operation open"
        );
    }
    let recovery_task = if recovery {
        Some(tokio::spawn({
            let state = state.clone();
            async move { service_closeout_obligation(&state, TAPE, 1).await }
        }))
    } else {
        let crate::write_owner::ManualFinalizeTapeResult::Accepted(accepted) =
            manual_finalize_tape(&state, first).await.unwrap()
        else {
            panic!("retry accepted");
        };
        assert_eq!(accepted.operation_id, operation);
        None
    };
    let (request, reply) = take_finalize_command(&mut drive_rx, operation).await;
    finish_finalize_command(&state, operation, request, reply);
    if let Some(task) = recovery_task {
        task.await.unwrap().unwrap();
    }
    wait_for_state(&state, operation, pb::OperationState::Succeeded).await;
    assert_eq!(
        native_finalization(typed.paths()),
        NativeFinalization {
            prepared: 1,
            decided: 1,
            retired: 0,
            sealed: 1
        }
    );
    assert!(drive_rx.try_recv().is_err());
    assert!(changer_rx.try_recv().is_err());
    drop((state, drive_rx, changer_rx));
    let chain = events_of(&mut typed, operation).await;
    assert!(
        matches!(
            chain.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_),
                lib::OperationEvent::Finished(_)
            ]
        ),
        "{chain:?}"
    );
    let accepted = typed
        .records(|r| (r.tag() == lib::Tag::ManualFinalizationAccepted).then_some(()))
        .await;
    assert_eq!(accepted.len(), 1);
    if rebuild {
        let (state, drive_rx, changer_rx) = second_incarnation(&typed, true);
        assert_eq!(
            operation_state(&state, operation).await,
            pb::OperationState::Succeeded as i32
        );
        drop((state, drive_rx, changer_rx));
    }
}

#[tokio::test]
async fn u7_deadline_same_key_retry_completes_in_process() {
    u7_deadline_then_resume(false, false).await;
}
#[tokio::test]
async fn u7_deadline_closeout_scan_completes_in_process() {
    u7_deadline_then_resume(true, false).await;
}
#[tokio::test]
async fn u7_deadline_empty_sql_rebuild_remains_open_then_finishes() {
    u7_deadline_then_resume(false, true).await;
}

/// A real catalog refusal after Sealed reaches the worker, which leaves Finished retryable.
#[tokio::test]
async fn sealed_manual_finalize_projection_error_does_not_poison_startup() {
    for mode in 0..3 {
        let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
        let tapes = install_checkpoint(&typed);
        let mut state = ApiState::new(typed.catalog());
        state.checkpoint_journal_dir = Arc::new(tapes);
        let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
        let request = admit_first_request(&state, admission(0x7F01));
        let operation = request.candidate_operation_id;
        assert_eq!(advance_tail(&state, operation, 5), 5);
        let fault = rusqlite::Connection::open(&typed.paths().sqlite_path).unwrap();
        fault.execute_batch("CREATE TRIGGER u7_refuse_sealed_projection BEFORE UPDATE ON tapes WHEN NEW.finalization_outcome = 'finalized' BEGIN SELECT RAISE(ABORT, 'u7 post-seal catalog refusal'); END;").unwrap();
        let pool = state.drive_pool().unwrap().clone();
        let reservation = pool.reserve_tape(TAPE).unwrap();
        let error = run_manual_finalize_worker_with_budget(
            state.clone(),
            pool,
            reservation.into(),
            request,
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert_eq!(
            native_finalization(typed.paths()).sealed,
            1,
            "fault follows Sealed: {error}"
        );
        assert!(error.message().contains("u7 post-seal catalog refusal"));
        let chain = operation_events_in_log(typed.paths(), operation);
        assert!(
            matches!(
                chain.as_slice(),
                [
                    lib::OperationEvent::Accepted(_),
                    lib::OperationEvent::Started(_)
                ]
            ),
            "post-seal error must leave the chain open: {chain:?}"
        );
        assert!(drive_rx.try_recv().is_err());
        assert!(changer_rx.try_recv().is_err());
        fault
            .execute_batch("DROP TRIGGER u7_refuse_sealed_projection;")
            .unwrap();
        drop((fault, state, drive_rx, changer_rx));
        typed.release();
        // The empty catalog first rebuilds the open operation, before startup repairs Finished.
        for path in sql_files(typed.paths()).keys() {
            std::fs::remove_file(path).unwrap();
        }
        let mut config = typed.config();
        let mut owner =
            remanence_state::StateHandle::open_with_config(typed.paths().clone(), config.clone())
                .unwrap();
        assert_eq!(
            owner
                .catalog()
                .get_operation(&operation.to_string())
                .unwrap()
                .unwrap()
                .state,
            "running"
        );
        match mode {
            0 => {
                drop(
                    ApiState::new_with_config(owner.projection_client(), &config)
                        .expect("configured service starts after the seal"),
                );
            }
            1 => {
                // Same model transport precedent as write_owner/r1c2_tests.rs::start_daemon.
                use remanence_chaos::model::{ModelTransport, VirtualWorld};
                let world = Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
                    "LIB001", 0x0101, "U7-DRIVE", 0x400, 1,
                )));
                config.libraries = vec![remanence_state::LibraryConfig {
                    serial: "LIB001".into(),
                    allow_derived_drive_identity: false,
                }];
                let report = remanence_library::DiscoveryReport {
                    libraries: vec![world.lock().unwrap().library_snapshot()],
                    unattached_drives: vec![],
                    complete: true,
                    incomplete_evidence: vec![],
                    warnings: vec![],
                };
                let spool = typed.paths().state_dir.join("spool");
                std::fs::create_dir_all(&spool).unwrap();
                let api = ApiState::with_drive_pool_opener(
                    owner.projection_client(),
                    &config,
                    report,
                    remanence_library::StaticAllowlist::new(["LIB001"]),
                    spool,
                    16 * 1024 * 1024,
                    move |library, policy| {
                        let world = world.clone();
                        library.open_with(policy, move |path| {
                            let role = world.lock().unwrap().role_for_path(path).unwrap();
                            Ok(Box::new(ModelTransport::new(world.clone(), role))
                                as Box<dyn remanence_library::SgTransport>)
                        })
                    },
                )
                .expect("drive-pool service starts after the seal");
                api.shutdown_drive_pool().await.unwrap();
                drop(api);
            }
            _ => crate::reconcile_checkpoint_journal_projections(&mut owner)
                .expect("standalone reconcile starts after the seal"),
        }
        assert_eq!(
            owner
                .catalog()
                .get_operation(&operation.to_string())
                .unwrap()
                .unwrap()
                .state,
            "finished"
        );
        crate::reconcile_checkpoint_journal_projections(&mut owner).unwrap();
        drop(owner);
        let chain = events_of(&mut typed, operation).await;
        assert!(
            matches!(
                chain.as_slice(),
                [
                    lib::OperationEvent::Accepted(_),
                    lib::OperationEvent::Started(_),
                    lib::OperationEvent::Finished(_)
                ]
            ),
            "one Finished after restart: {chain:?}"
        );
        assert_eq!(native_finalization(typed.paths()).sealed, 1);
    }
}

/// The request wins ownership after a stopped attempt; the scan cannot duplicate its seal.
#[test]
fn u7_two_resumers_after_stop_retry_first() {
    check_two_resumers_after_stop(FirstResumer::Retry, true);
}
/// The scan wins ownership after a stopped attempt; the same-key request rejoins it.
#[test]
fn u7_two_resumers_after_stop_scan_first() {
    check_two_resumers_after_stop(FirstResumer::Recovery, true);
}

/// A successful model TUR, as in the existing typed acceptance/rejoin test.
fn u7_ready(state: &ApiState, readiness: Uuid) {
    let mut ready = crate::startup_media_readiness::startup_media_readiness_probe_transition(
        readiness,
        &remanence_library::MediaReadiness::new(remanence_library::MediaReadinessKind::Ready),
    );
    ready.completion_evidence =
        Some((&remanence_library::outcome::CompletionEvidence::success()).into());
    state
        .catalog
        .write_handle()
        .record_media_readiness_transition(ready)
        .unwrap();
}

/// A resource refusal stops the attempt without closing its operation or consuming media retries.
#[tokio::test]
async fn u7_no_free_bay_stops_open_and_same_key_completes() {
    let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
    let first = admission(0x7B01);
    let request = admit_first_request(&state, first.clone());
    let operation = request.candidate_operation_id;
    let pool = state.drive_pool().unwrap().clone();
    let occupied = pool
        .reserve_drive(&crate::drive_pool::DriveKey::new("LIB001", 0x0101))
        .unwrap();
    let before = durable_bytes(typed.paths());
    let reserved = pool.reserve_tape(TAPE).unwrap();
    let error = run_manual_finalize_worker_with_budget(
        state.clone(),
        pool.clone(),
        reserved.into(),
        request,
        Duration::ZERO,
    )
    .await
    .unwrap_err();
    let reason = error
        .message()
        .strip_prefix("this attempt stopped: ")
        .unwrap_or(error.message())
        .split(". The finalization stays accepted;")
        .next()
        .unwrap();
    u7_assert_stopped(&typed, &state, operation, reason).await;
    assert!(reason.contains("busy") || reason.contains("no free drive"));
    assert!(error.metadata().get("remanence-retry").is_none());
    assert!(crate::closeout_obligation::transient(&error));
    assert_eq!(durable_bytes(typed.paths()), before);
    assert!(drive_rx.try_recv().is_err());
    assert!(changer_rx.try_recv().is_err());
    for _ in 0..5 {
        crate::closeout_obligation::scan(&state).unwrap();
        state.closeout_work.wake_tape(TAPE);
        assert_eq!(
            crate::closeout_obligation::service_unheld_for_test(&state).await,
            vec![(TAPE, 1)]
        );
        assert!(
            !state.closeout_work.parked_tapes().contains(&TAPE),
            "bay contention remains unbounded"
        );
        assert_eq!(durable_bytes(typed.paths()), before);
    }
    drop(occupied);
    manual_finalize_tape(&state, first).await.unwrap();
    let (request, reply) = take_finalize_command(&mut drive_rx, operation).await;
    finish_finalize_command(&state, operation, request, reply);
    wait_for_state(&state, operation, pb::OperationState::Succeeded).await;
    drop((state, pool, drive_rx, changer_rx));
    let chain = events_of(&mut typed, operation).await;
    assert!(
        matches!(
            chain.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_),
                lib::OperationEvent::Finished(_)
            ]
        ),
        "{chain:?}"
    );
    assert_eq!(native_finalization(typed.paths()).sealed, 1);
}

/// The real worker enters wait_after_refusal before shutdown; a fresh daemon resumes its intent.
#[tokio::test]
async fn u7_shutdown_during_readiness_wait_leaves_open_then_restart_completes() {
    let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
    let first = admission(0x7B02);
    let request = admit_first_request(&state, first.clone());
    let operation = request.candidate_operation_id;
    let readiness = u7_readiness_fence(&state);
    let before = durable_bytes(typed.paths());
    let pool = state.drive_pool().unwrap().clone();
    let reserved = pool.reserve_tape(TAPE).unwrap();
    let worker = tokio::spawn(run_manual_finalize_worker_with_budget(
        state.clone(),
        pool.clone(),
        reserved.into(),
        request,
        Duration::from_secs(900),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while state
            .operations
            .latest(&operation)
            .is_none_or(|s| s.progress.get("phase").map(String::as_str) != Some("readiness"))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("wait_after_refusal publishes readiness before shutdown");
    state
        .readiness_shutdown
        .store(true, std::sync::atomic::Ordering::Release);
    let error = tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(crate::drive_pool::DaemonShuttingDown::is_status(&error));
    let reason = error
        .message()
        .strip_prefix("this attempt stopped: ")
        .unwrap_or(error.message())
        .split(". The finalization stays accepted;")
        .next()
        .unwrap();
    u7_assert_stopped(&typed, &state, operation, reason).await;
    assert_eq!(durable_bytes(typed.paths()), before);
    assert!(drive_rx.try_recv().is_err());
    assert!(changer_rx.try_recv().is_err());
    drop((state, pool, drive_rx, changer_rx));
    typed.release();
    let (state, mut drive_rx, mut changer_rx) = second_incarnation(&typed, true);
    assert_eq!(
        operation_state(&state, operation).await,
        pb::OperationState::Running as i32
    );
    u7_ready(&state, readiness);
    manual_finalize_tape(&state, first).await.unwrap();
    let (request, reply) = take_finalize_command(&mut drive_rx, operation).await;
    finish_finalize_command(&state, operation, request, reply);
    wait_for_state(&state, operation, pb::OperationState::Succeeded).await;
    assert!(changer_rx.try_recv().is_err());
    drop((state, drive_rx, changer_rx));
    let chain = events_of(&mut typed, operation).await;
    assert!(
        matches!(
            chain.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_),
                lib::OperationEvent::Finished(_)
            ]
        ),
        "{chain:?}"
    );
}

/// An untagged write-protect refusal from the actor parks the first recovery attempt; a wake resumes it.
#[tokio::test]
async fn u7_write_protection_stops_open_parks_first_and_wake_completes() {
    let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
    let request = admit_first_request(&state, admission(0x7B03));
    let operation = request.candidate_operation_id;
    let pool = state.drive_pool().unwrap().clone();
    let reserved = pool.reserve_tape(TAPE).unwrap();
    let before = durable_bytes(typed.paths());
    let worker = tokio::spawn(run_manual_finalize_worker_with_budget(
        state.clone(),
        pool.clone(),
        reserved.into(),
        request,
        Duration::ZERO,
    ));
    let (_, reply) = take_finalize_command(&mut drive_rx, operation).await;
    const REASON: &str = "tape is write-protected and cannot be finalized";
    reply
        .send(Err(Status::failed_precondition(REASON)))
        .unwrap();
    worker.await.unwrap().unwrap_err();
    u7_assert_stopped(&typed, &state, operation, REASON).await;
    assert_eq!(durable_bytes(typed.paths()), before);
    crate::closeout_obligation::scan(&state).unwrap();
    let recovery = tokio::spawn({
        let state = state.clone();
        async move { crate::closeout_obligation::service_unheld_for_test(&state).await }
    });
    let (_, reply) = take_finalize_command(&mut drive_rx, operation).await;
    reply
        .send(Err(Status::failed_precondition(REASON)))
        .unwrap();
    assert_eq!(recovery.await.unwrap(), vec![(TAPE, 1)]);
    assert!(
        state.closeout_work.parked_tapes().contains(&TAPE),
        "write protection parks on the first recovery attempt"
    );
    assert_eq!(durable_bytes(typed.paths()), before);
    assert!(crate::closeout_obligation::service_unheld_for_test(&state)
        .await
        .is_empty());
    state.closeout_work.wake_tape(TAPE);
    let recovery = tokio::spawn({
        let state = state.clone();
        async move { crate::closeout_obligation::service_unheld_for_test(&state).await }
    });
    let (request, reply) = take_finalize_command(&mut drive_rx, operation).await;
    finish_finalize_command(&state, operation, request, reply);
    assert_eq!(recovery.await.unwrap(), vec![(TAPE, 1)]);
    assert!(changer_rx.try_recv().is_err());
    assert_eq!(native_finalization(typed.paths()).sealed, 1);
    drop((state, pool, drive_rx, changer_rx));
    let chain = events_of(&mut typed, operation).await;
    assert!(
        matches!(
            chain.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_),
                lib::OperationEvent::Finished(_)
            ]
        ),
        "{chain:?}"
    );
}

/// The model's real first WRITE runs, then reports a short transfer, as in write_owner's precedent.
struct U7ShortWrite {
    inner: remanence_chaos::model::ModelTransport,
    short: bool,
}
impl remanence_library::SgTransport for U7ShortWrite {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        self.inner.execute_none(cdb)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        let mut outcome = self.inner.execute_out(cdb, buf)?;
        if self.short && cdb.first() == Some(&0x0A) {
            self.short = false;
            outcome.bytes_transferred = outcome.bytes_transferred.saturating_sub(1);
        }
        Ok(outcome)
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
    fn configure_reserved_buffer(
        &mut self,
        bytes: u32,
    ) -> Result<u32, remanence_library::ScsiError> {
        self.inner.configure_reserved_buffer(bytes)
    }
}

/// Drive the unchanged CompletionUnknown producers through the worker and the shared physical funnel.
async fn u7_completion_unknown(unproved: bool) {
    use remanence_chaos::model::{ModelTransport, Record, VirtualTape, VirtualWorld};
    let typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
    let first = admission(0x7C01);
    let request = admit_first_request(&state, first.clone());
    let operation = request.candidate_operation_id;
    let pool = state.drive_pool().unwrap().clone();
    let reserved = pool.reserve_tape(TAPE).unwrap();
    let worker = tokio::spawn(run_manual_finalize_worker_with_budget(
        state.clone(),
        pool.clone(),
        reserved.into(),
        request,
        Duration::ZERO,
    ));
    let (request, reply) = take_finalize_command(&mut drive_rx, operation).await;
    let mut world = VirtualWorld::single_drive("LIB001", 0x0101, "U7-DRIVE", 0x400, 1);
    let mut tape = VirtualTape::empty(18_000_000_000_000, BLOCK_SIZE);
    let bootstrap = crate::build_tape_bootstrap(
        TAPE,
        BLOCK_SIZE,
        remanence_parity::ParityConfig::None,
        "2026-11-11T00:00:00Z",
        "test",
    );
    let mut block = vec![0; BLOCK_SIZE as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, &mut block).unwrap();
    tape.records = vec![
        Record::Block(block),
        Record::Filemark,
        Record::Block(vec![0; BLOCK_SIZE as usize]),
        Record::Block(vec![0; BLOCK_SIZE as usize]),
        Record::Block(vec![0; BLOCK_SIZE as usize]),
        Record::Filemark,
    ];
    tape.written_bytes = 4 * u64::from(BLOCK_SIZE);
    tape.durable_frontier = tape.records.len();
    if unproved {
        tape.worm = true;
        tape.records
            .push(Record::Block(vec![0x55; BLOCK_SIZE as usize]));
    }
    world.put_tape_in_drive(0x0101, LABEL, Some(0x400), tape);
    let world = Arc::new(std::sync::Mutex::new(world));
    let library = world.lock().unwrap().library_snapshot();
    let policy = remanence_library::StaticAllowlist::new(["LIB001"]);
    let mut library = library
        .open_with(&policy, {
            let world = world.clone();
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(U7ShortWrite {
                    inner: ModelTransport::new(world.clone(), role),
                    short: !unproved,
                })
                    as Box<dyn remanence_library::SgTransport>)
            }
        })
        .unwrap();
    let mut drive = library.open_drive(0x0101, &policy).unwrap();
    let cfg = crate::write_owner::TerminalWriteConfig {
        authority: crate::write_owner::TerminalAuthority::from_api(&state).unwrap(),
        io_memory: crate::io_memory::IoMemoryReservation::new(64 * 1024 * 1024).unwrap(),
        mounted_endpoint: None,
        checkpoint_journal_dir: state.checkpoint_journal_dir.as_ref().clone(),
    };
    let result = crate::write_owner::finalize_mounted_tape(
        &mut state.catalog.write_handle(),
        &cfg,
        &mut drive,
        &request,
        Some(LABEL),
        |_, drive| {
            drive.load().map_err(|e| Status::internal(e.to_string()))?;
            drive
                .locate(6)
                .map_err(|e| Status::internal(e.to_string()))?;
            Ok(())
        },
        |_, _| {},
    );
    assert_eq!(
        result.is_ok(),
        unproved,
        "the physical funnel's outcome: {result:?}"
    );
    let chain = operation_events_in_log(typed.paths(), operation);
    assert!(
        matches!(
            chain.last(),
            Some(lib::OperationEvent::CompletionUnknown(_))
        ),
        "the unchanged producer declared the incident: {chain:?}; {result:?}"
    );
    reply.send(result).unwrap();
    let stopped = worker.await.unwrap();
    if unproved {
        stopped.unwrap();
    } else {
        let error = stopped.unwrap_err();
        assert_eq!(error.metadata().get("remanence-retry").unwrap(), "operator");
    }
    let live = state.operations.latest(&operation).unwrap();
    assert_eq!(
        live.state,
        pb::OperationState::CompletionUnknown as i32,
        "completion-unknown must never publish success or failed"
    );
    assert!(
        live.error_summary.contains("completion is unknown")
            && live.error_summary.contains("operator recovery")
    );
    let polled = <crate::DaemonService as pb::daemon_server::Daemon>::get_operation(
        &state.daemon_service(),
        tonic::Request::new(pb::GetOperationRequest {
            operation_id: operation.as_bytes().to_vec(),
        }),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(polled.state, live.state);
    assert_eq!(polled.error_summary, live.error_summary);
    let finalization = tape_finalization(&state).await;
    assert_eq!(
        finalization.outcome,
        pb::TapeFinalizationOutcome::RecoveryRequired as i32
    );
    assert_eq!(finalization.detail, live.error_summary);
    let refused = manual_finalize_tape(&state, first).await.unwrap_err();
    assert!(refused.message().contains("ClosedOperationRetry"));
    let before = durable_bytes(typed.paths());
    crate::closeout_obligation::scan(&state).unwrap();
    assert_eq!(
        crate::closeout_obligation::service_unheld_for_test(&state).await,
        vec![(TAPE, 1)]
    );
    assert!(
        state.closeout_work.parked_tapes().contains(&TAPE),
        "closed head parks the first scan attempt"
    );
    assert_eq!(
        state.operations.latest(&operation).unwrap(),
        live,
        "recovery must not register over a closed head"
    );
    assert_eq!(
        operation_state(&state, operation).await,
        pb::OperationState::CompletionUnknown as i32
    );
    assert_eq!(durable_bytes(typed.paths()), before);
    assert!(drive_rx.try_recv().is_err());
    assert!(changer_rx.try_recv().is_err());
}

#[tokio::test]
async fn u7_unproved_tail_publishes_unknown_and_scan_parks_without_registering() {
    u7_completion_unknown(true).await;
}
#[tokio::test]
async fn u7_tail_write_failure_publishes_unknown_and_scan_parks_without_registering() {
    u7_completion_unknown(false).await;
}

/// Invoke the real cancel RPC with an authenticated request actor.
async fn u7_cancel(state: &ApiState, operation: Uuid) -> pb::CancelOperationResponse {
    let mut request = tonic::Request::new(pb::CancelOperationRequest {
        operation_id: operation.as_bytes().to_vec(),
        idempotency_key: None,
        force: false,
    });
    request
        .metadata_mut()
        .insert("x-remanence-actor", "u7-operator".parse().unwrap());
    <crate::DaemonService as pb::daemon_server::Daemon>::cancel_operation(
        &state.daemon_service(),
        request,
    )
    .await
    .expect("cancel answers from accepted source even without a live entry")
    .into_inner()
}

/// Assert the durable refusal before its response text, so the gate mutation check observes the event.
fn u7_assert_cancel_rejected(
    typed: &TypedRoot,
    operation: Uuid,
    response: &pb::CancelOperationResponse,
) {
    let chain = operation_events_in_log(typed.paths(), operation);
    assert!(
        matches!(
            chain.last(),
            Some(lib::OperationEvent::CancellationRejected(_))
        ),
        "accepted cancel must record CancellationRejected: {chain:?}"
    );
    assert_eq!(response.resulting_state, pb::OperationState::Running as i32);
    assert_eq!(
        response.detail,
        "an accepted finalization cannot be cancelled; it completes when a drive is ready"
    );
}

/// Cancelling a waiting attempt neither sets its cancel token nor ends its readiness wait.
#[tokio::test]
async fn u7_cancel_during_readiness_wait_is_rejected_and_attempt_finishes() {
    let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
    let request = admit_first_request(&state, admission(0x7D01));
    let operation = request.candidate_operation_id;
    let readiness = u7_readiness_fence(&state);
    let pool = state.drive_pool().unwrap().clone();
    let reserved = pool.reserve_tape(TAPE).unwrap();
    let mut worker = tokio::spawn(run_manual_finalize_worker_with_budget(
        state.clone(),
        pool.clone(),
        reserved.into(),
        request,
        Duration::from_secs(900),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while state
            .operations
            .latest(&operation)
            .is_none_or(|s| s.progress.get("phase").map(String::as_str) != Some("readiness"))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let response = u7_cancel(&state, operation).await;
    u7_assert_cancel_rejected(&typed, operation, &response);
    let before = durable_bytes(typed.paths());
    u7_assert_cancel_rejected(&typed, operation, &u7_cancel(&state, operation).await);
    assert_eq!(
        durable_bytes(typed.paths()),
        before,
        "repeated rejection deduplicates"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(350), &mut worker)
            .await
            .is_err(),
        "no cancel flag: the worker stays in readiness wait past its next cancellation check"
    );
    assert_eq!(
        state.operations.latest(&operation).unwrap().state,
        pb::OperationState::Running as i32
    );
    u7_ready(&state, readiness);
    let (request, reply) = take_finalize_command(&mut drive_rx, operation).await;
    finish_finalize_command(&state, operation, request, reply);
    worker.await.unwrap().unwrap();
    let before = durable_bytes(typed.paths());
    let terminal = u7_cancel(&state, operation).await;
    assert_eq!(
        terminal.resulting_state,
        pb::OperationState::Succeeded as i32
    );
    assert_eq!(terminal.detail, "operation is already terminal");
    assert_eq!(durable_bytes(typed.paths()), before);
    assert!(changer_rx.try_recv().is_err());
    drop((state, pool, drive_rx, changer_rx));
    let chain = events_of(&mut typed, operation).await;
    assert!(
        matches!(
            chain.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_),
                lib::OperationEvent::CancellationRejected(_),
                lib::OperationEvent::Finished(_)
            ]
        ),
        "{chain:?}"
    );
}

/// Live FAILED is an attempt result; cancel must still reject with durable RUNNING.
#[tokio::test]
async fn u7_cancel_after_stopped_attempt_answers_running_and_deduplicates() {
    let typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let (_drive_rx, _changer_rx) = load_drive(&mut state);
    let request = admit_first_request(&state, admission(0x7D02));
    let operation = request.candidate_operation_id;
    u7_readiness_fence(&state);
    let pool = state.drive_pool().unwrap().clone();
    let reserved = pool.reserve_tape(TAPE).unwrap();
    run_manual_finalize_worker_with_budget(
        state.clone(),
        pool,
        reserved.into(),
        request,
        Duration::from_millis(20),
    )
    .await
    .unwrap_err();
    assert_eq!(
        state.operations.latest(&operation).unwrap().state,
        pb::OperationState::Failed as i32
    );
    let response = u7_cancel(&state, operation).await;
    u7_assert_cancel_rejected(&typed, operation, &response);
    let before = durable_bytes(typed.paths());
    u7_assert_cancel_rejected(&typed, operation, &u7_cancel(&state, operation).await);
    assert_eq!(durable_bytes(typed.paths()), before);
    assert_eq!(
        state.operations.latest(&operation).unwrap().state,
        pb::OperationState::Failed as i32
    );
}

/// The crash-left operation is cancelled with no live handle, then restarted on CancellationRejected.
fn u7_cancel_crash_left_then_resume(recovery: bool) {
    let mut typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    install_checkpoint(&typed);
    let key = Uuid::from_u128(0x7D03);
    let Killed { operation, .. } = first_incarnation(&mut typed, key, KillAt::Dispatched);
    let incarnation = runtime();
    let (state, drive_rx, changer_rx) = second_incarnation(&typed, true);
    incarnation.block_on(async {
        assert!(state.operations.latest(&operation).is_none());
        let response = u7_cancel(&state, operation).await;
        u7_assert_cancel_rejected(&typed, operation, &response);
        assert!(
            state.operations.latest(&operation).is_none(),
            "cancel creates no live handle"
        );
        let before = durable_bytes(typed.paths());
        u7_cancel(&state, operation).await;
        assert_eq!(durable_bytes(typed.paths()), before);
    });
    drop((state, drive_rx, changer_rx));
    let (state, mut drive_rx, mut changer_rx) = second_incarnation(&typed, true);
    incarnation.block_on(async {
        assert!(matches!(
            operation_events_in_log(typed.paths(), operation).last(),
            Some(lib::OperationEvent::CancellationRejected(_))
        ));
        let task = if recovery {
            Some(tokio::spawn({
                let state = state.clone();
                async move { service_closeout_obligation(&state, TAPE, 1).await }
            }))
        } else {
            let resumed = finalize_rpc(&state, key)
                .await
                .expect("same-key resume with CancellationRejected head");
            assert_eq!(
                resumed.operation_id.as_deref(),
                Some(operation.as_bytes().as_slice())
            );
            None
        };
        let (request, reply) = take_finalize_command(&mut drive_rx, operation).await;
        finish_finalize_command(&state, operation, request, reply);
        if let Some(task) = task {
            task.await
                .unwrap()
                .expect("scan resume with CancellationRejected head");
        }
        wait_for_state(&state, operation, pb::OperationState::Succeeded).await;
        assert!(drive_rx.try_recv().is_err());
        assert!(changer_rx.try_recv().is_err());
        assert_eq!(
            native_finalization(typed.paths()),
            NativeFinalization {
                prepared: 1,
                decided: 1,
                retired: 0,
                sealed: 1
            }
        );
    });
    drop((state, drive_rx, changer_rx));
    let chain = incarnation.block_on(events_of(&mut typed, operation));
    assert!(
        matches!(
            chain.as_slice(),
            [
                lib::OperationEvent::Accepted(_),
                lib::OperationEvent::Started(_),
                lib::OperationEvent::CancellationRejected(_),
                lib::OperationEvent::Finished(_)
            ]
        ),
        "no second Started; one Finished: {chain:?}"
    );
}

#[test]
fn u7_cancel_crash_left_then_same_key_resume_from_rejected_head() {
    u7_cancel_crash_left_then_resume(false);
}
#[test]
fn u7_cancel_crash_left_then_scan_resume_from_rejected_head() {
    u7_cancel_crash_left_then_resume(true);
}

/// Untagged MOVE failures after reservation consume the existing three-attempt budget.
async fn u7_motion_bound(eviction: bool) {
    let typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL), ([0xB4; 16], "R3B002L9")]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
    let mut bay = drive_bay(0x0101, eviction, eviction.then_some("R3B002L9"));
    bay.source_slot = eviction.then_some(0x0400);
    state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
        crate::live_status::LibrarySnapshot {
            report: remanence_library::DiscoveryReport {
                unattached_drives: vec![],
                complete: true,
                incomplete_evidence: vec![],
                libraries: vec![test_library(
                    vec![bay],
                    vec![empty_slot(0x0400), slot(0x0401, LABEL)],
                )],
                warnings: vec![],
            },
            captured_at: time::OffsetDateTime::UNIX_EPOCH,
        },
    ))));
    let request = admit_first_request(&state, admission(0x7E01));
    let operation = request.candidate_operation_id;
    let before = durable_bytes(typed.paths());
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(if attempt == 1 {
                1050
            } else {
                5050
            }))
            .await;
        }
        crate::closeout_obligation::scan(&state).unwrap();
        let recovery = tokio::spawn({
            let state = state.clone();
            async move { crate::closeout_obligation::service_unheld_for_test(&state).await }
        });
        if eviction {
            let crate::write_owner::DriveCommand::Unload { reply, .. } =
                tokio::time::timeout(Duration::from_secs(3), drive_rx.recv())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("eviction unloads the spare");
            };
            reply.send(Ok(Duration::ZERO.into())).unwrap();
        }
        let crate::write_owner::ChangerCommand::Move {
            src, dst, reply, ..
        } = tokio::time::timeout(Duration::from_secs(3), changer_rx.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("attempt moves media");
        };
        assert_eq!(
            (src, dst),
            if eviction {
                (0x0101, 0x0400)
            } else {
                (0x0401, 0x0101)
            }
        );
        reply
            .send(Err(Status::unavailable("u7 changer motion failed")))
            .unwrap();
        assert_eq!(recovery.await.unwrap(), vec![(TAPE, 1)]);
        assert_eq!(
            state.closeout_work.parked_tapes().contains(&TAPE),
            attempt == 2,
            "untagged motion must park at the third attempt"
        );
        let (count, reason, running) = state.closeout_work.retry_evidence_for_test((TAPE, 1));
        assert_eq!(count, attempt + 1);
        assert!(!running);
        if attempt == 2 {
            assert!(reason.unwrap().contains("media fault (3/3 attempts)"));
        }
        assert_eq!(durable_bytes(typed.paths()), before);
        let live = state.operations.latest(&operation).unwrap();
        let reason = live
            .error_summary
            .strip_prefix("this attempt stopped: ")
            .unwrap()
            .split(". The finalization stays accepted;")
            .next()
            .unwrap();
        u7_assert_stopped(&typed, &state, operation, reason).await;
        assert!(drive_rx.try_recv().is_err());
        assert!(changer_rx.try_recv().is_err());
    }
    assert_eq!(
        state
            .closeout_work
            .parked_refusal(TAPE)
            .unwrap()
            .metadata()
            .get("remanence-retry")
            .unwrap(),
        "media"
    );
    assert!(crate::closeout_obligation::service_unheld_for_test(&state)
        .await
        .is_empty());
    state.closeout_work.wake_tape(TAPE);
    let recovery = tokio::spawn({
        let state = state.clone();
        async move { crate::closeout_obligation::service_unheld_for_test(&state).await }
    });
    if eviction {
        let crate::write_owner::DriveCommand::Unload { reply, .. } = drive_rx.recv().await.unwrap()
        else {
            panic!("wake unloads spare");
        };
        reply.send(Ok(Duration::ZERO.into())).unwrap();
        let crate::write_owner::ChangerCommand::Move {
            src, dst, reply, ..
        } = changer_rx.recv().await.unwrap()
        else {
            panic!("wake returns spare");
        };
        assert_eq!((src, dst), (0x0101, 0x0400));
        reply.send(Ok(())).unwrap();
    }
    let crate::write_owner::ChangerCommand::Move {
        src, dst, reply, ..
    } = changer_rx.recv().await.unwrap()
    else {
        panic!("wake loads finalizing tape");
    };
    assert_eq!((src, dst), (0x0401, 0x0101));
    reply.send(Ok(())).unwrap();
    let (request, reply) = take_finalize_command(&mut drive_rx, operation).await;
    finish_finalize_command(&state, operation, request, reply);
    assert_eq!(recovery.await.unwrap(), vec![(TAPE, 1)]);
    wait_for_state(&state, operation, pb::OperationState::Succeeded).await;
    assert_eq!(native_finalization(typed.paths()).sealed, 1);
    assert!(matches!(
        operation_events_in_log(typed.paths(), operation).as_slice(),
        [
            lib::OperationEvent::Accepted(_),
            lib::OperationEvent::Started(_),
            lib::OperationEvent::Finished(_)
        ]
    ));
}

#[tokio::test]
async fn u7_changer_failure_parks_third_then_wake_completes() {
    u7_motion_bound(false).await;
}
#[tokio::test]
async fn u7_eviction_move_failure_parks_third_then_wake_completes() {
    u7_motion_bound(true).await;
}

/// The read caller traverses the same reservation and eviction without finalization's tag.
#[tokio::test]
async fn u7_read_eviction_error_remains_unchanged() {
    let mut harness = three_bay_two_pool_harness(
        vec![drive_bay(0x0101, true, Some("FTA001L9"))],
        vec![empty_slot(0x0401), slot(0x0402, "FTB001L9")],
    );
    let mut drive_rx = harness.drive_rxs.remove(&0x0101).unwrap();
    let caller = harness.state.clone();
    let task = tokio::spawn(async move {
        open_read_session(
            &caller,
            ReadSessionTarget::Tape {
                tape_uuid: POOL_TAPE_B,
            },
            None,
        )
        .await
    });
    let crate::write_owner::DriveCommand::Unload { reply, .. } =
        tokio::time::timeout(Duration::from_secs(3), drive_rx.recv())
            .await
            .unwrap()
            .unwrap()
    else {
        panic!("read evicts spare");
    };
    let mut error = Status::unavailable("u7 read eviction unload failed");
    error
        .metadata_mut()
        .insert("u7-evidence", "preserved".parse().unwrap());
    reply.send(Err(error)).unwrap();
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unavailable);
    assert_eq!(error.message(), "u7 read eviction unload failed");
    assert_eq!(error.metadata().get("u7-evidence").unwrap(), "preserved");
    assert!(error.metadata().get("remanence-retry").is_none());
    assert!(harness.changer_rx.try_recv().is_err());
}

/// Repeated scan cycles while readiness cannot dispatch neither append source nor move media.
#[tokio::test]
async fn u7_readiness_never_returns_scan_cycles_append_nothing() {
    let typed = TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let tapes = install_checkpoint(&typed);
    let mut state = ApiState::new(typed.catalog());
    state.checkpoint_journal_dir = Arc::new(tapes);
    let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
    let request = admit_first_request(&state, admission(0x7E02));
    let operation = request.candidate_operation_id;
    let readiness = u7_readiness_fence(&state);
    state
        .operations
        .register(readiness, "media_readiness_wait")
        .publish_failed(
            "readiness actor unavailable",
            &[("phase", "dispatch"), ("state", "dispatch_failed")],
        );
    let pool = state.drive_pool().unwrap().clone();
    let reserved = pool.reserve_tape(TAPE).unwrap();
    let before = durable_bytes(typed.paths());
    run_manual_finalize_worker_with_budget(
        state.clone(),
        pool,
        reserved.into(),
        request,
        Duration::ZERO,
    )
    .await
    .unwrap_err();
    assert_eq!(durable_bytes(typed.paths()), before);
    for _ in 0..5 {
        crate::closeout_obligation::scan(&state).unwrap();
        state.closeout_work.wake_tape(TAPE);
        assert_eq!(
            crate::closeout_obligation::service_unheld_for_test(&state).await,
            vec![(TAPE, 1)]
        );
        assert!(!state.closeout_work.parked_tapes().contains(&TAPE));
        assert_eq!(durable_bytes(typed.paths()), before);
        assert!(drive_rx.try_recv().is_err());
        assert!(changer_rx.try_recv().is_err());
        let live = state.operations.latest(&operation).unwrap();
        let reason = live
            .error_summary
            .strip_prefix("this attempt stopped: ")
            .unwrap()
            .split(". The finalization stays accepted;")
            .next()
            .unwrap();
        u7_assert_stopped(&typed, &state, operation, reason).await;
    }
}
