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
//! Its refusal is pinned by its exact current text. The countable code, `ClosedOperationRetry`,
//! arrives with L1-W (ruling 25), so each pin carries a marker where that assertion will go.
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
    let config = typed.config();
    if closing_step {
        let mut handle =
            remanence_state::StateHandle::open_with_config(typed.paths().clone(), config.clone())
                .expect("reopen the state root");
        let report = handle.startup_replay().expect("startup replay");
        assert_eq!(report.lost_operations_marked, 1, "the started finalize");
        let state = ApiState::new_with_config(handle.projection_client(), &config)
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
        ApiState::new_with_config(catalog, &config).expect("service state after the restart");
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
fn durable_bytes(paths: &remanence_state::StatePaths) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = files_under(&paths.audit_dir);
    files.extend(files_under(&paths.journal_dir.join("tapes")));
    files
}

/// What the tape's native log holds about finalization, counted from its decoded frames.
#[derive(Debug, PartialEq, Eq)]
struct NativeFinalization {
    prepared: usize,
    decided: usize,
    retired: usize,
    sealed: usize,
}

fn native_finalization(paths: &remanence_state::StatePaths) -> NativeFinalization {
    use remanence_state::tape_log::{codec::Decoder, store::Header, Record};
    let directory = paths.journal_dir.join("tapes");
    let path = remanence_state::TapeAuthority::list_entries(&directory)
        .unwrap()
        .into_iter()
        .find(|path| remanence_state::TapeAuthority::entry_tape_uuid(path).unwrap() == TAPE)
        .expect("native tape log");
    let mut file = std::fs::File::open(path).unwrap();
    Header::decode(&mut file, TAPE).unwrap();
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
/// precondition, that the library log and the native logs did not change, and that no media
/// command was sent. Returns the refusal so the caller can pin its text.
async fn same_key_retry_is_refused(
    typed: &TypedRoot,
    admission: &ManualFinalizeTapeAdmission,
    live: &mut Live,
) -> Status {
    assert_acceptance_applied(live, admission);
    let before = durable_bytes(typed.paths());
    let error = manual_finalize_tape(&live.state, admission.clone())
        .await
        .expect_err("a closed operation admits no same-key retry");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition, "{error}");
    assert_eq!(
        durable_bytes(typed.paths()),
        before,
        "the refused retry appended no record"
    );
    live.assert_no_media_command();
    error
}

/// A restart closed the started finalize as `Failed { summary: "daemon_restart" }`.
#[tokio::test]
async fn same_key_retry_after_failed_daemon_restart_is_refused_with_nothing_written() {
    let (mut typed, admission, request, mut live) = finalize_then_restart(Closure::Restart);
    let error = same_key_retry_is_refused(&typed, &admission, &mut live).await;
    // L1-W: assert ClosedOperationRetry
    assert_eq!(
        error.message(),
        "tape provisioning conflict: manual operation cannot resume from \
         Some(Failed(Failed { retry: None, refusal_code: None, \
         summary: TextValue(\"daemon_restart\") })); \
         no permitted same-operation completion transition"
    );
    drop((live, error));
    assert!(matches!(
        first_operation_events(&mut typed, &request).await.as_slice(),
        [
            lib::OperationEvent::Accepted(_),
            lib::OperationEvent::Started(_),
            lib::OperationEvent::Failed(failed),
        ] if failed.summary.as_str() == "daemon_restart"
    ));
}

/// A real attempt failed and the worker recorded `Failed`.
#[tokio::test]
async fn same_key_retry_after_plain_failed_is_refused_with_nothing_written() {
    let (mut typed, admission, request, mut live) = finalize_then_restart(Closure::Failed);
    let error = same_key_retry_is_refused(&typed, &admission, &mut live).await;
    // L1-W: assert ClosedOperationRetry
    assert_eq!(
        error.message(),
        format!(
            "tape provisioning conflict: manual operation cannot resume from \
             Some(Failed(Failed {{ retry: None, refusal_code: None, \
             summary: TextValue(\"drive actor is unavailable; finalization operation {} \
             remains incomplete\") }})); no permitted same-operation completion transition",
            request.candidate_operation_id
        )
    );
    drop((live, error));
    assert!(matches!(
        first_operation_events(&mut typed, &request).await.as_slice(),
        [
            lib::OperationEvent::Accepted(_),
            lib::OperationEvent::Started(_),
            lib::OperationEvent::Failed(failed),
        ] if failed.summary.as_str().starts_with("drive actor is unavailable")
    ));
}

/// The worker could not tell whether the physical work completed.
#[tokio::test]
async fn same_key_retry_after_completion_unknown_is_refused_with_nothing_written() {
    let (mut typed, admission, request, mut live) =
        finalize_then_restart(Closure::CompletionUnknown);
    let error = same_key_retry_is_refused(&typed, &admission, &mut live).await;
    // L1-W: assert ClosedOperationRetry
    assert_eq!(
        error.message(),
        "tape provisioning conflict: manual operation cannot resume from \
         Some(CompletionUnknown(Unit)); no permitted same-operation completion transition"
    );
    drop((live, error));
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
        .ensure_manual_operation_continuable(*request.candidate_operation_id.as_bytes())
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
    <crate::CatalogService as crate::pb::catalog_server::Catalog>::finalize_tape(
        &state.catalog_service(),
        tonic::Request::new(pb::FinalizeTapeRequest {
            tape_uuid: TAPE.to_vec(),
            expected_pool_id: None,
            reason: REASON.to_string(),
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
    /// The first RPC answered with an error after the acceptance was durable; no worker ran.
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

/// The answer the first `FinalizeTape` RPC gets over a typed library runtime, observed at this
/// tree. The acceptance is durable (Prepared, `ManualFinalizationAccepted`, Decided), but the
/// admission path then finds no SQL finalization projection and answers `Internal`. The
/// projection appears only when the next preflight projects the Decided group, so the same-key
/// RPC that follows is accepted.
const FIRST_RPC_REFUSAL: &str = "durably accepted FinalizeTape has no finalization projection";

/// Run the original request through the `FinalizeTape` RPC and the real worker up to its drive
/// dispatch, let the model drive actor do what `kill_at` says, and kill the incarnation with the
/// actor's reply unanswered.
fn first_incarnation(typed: &mut TypedRoot, key: Uuid, kill_at: KillAt) -> Killed {
    let tapes = typed.paths().journal_dir.join("tapes");
    let paths = typed.paths().clone();
    let catalog = typed.catalog();
    let incarnation = runtime();
    let (before_seal, unanswered) = incarnation.block_on(async move {
        let mut state = ApiState::new(catalog);
        state.checkpoint_journal_dir = Arc::new(tapes);
        let (mut drive_rx, mut changer_rx) = load_drive(&mut state);
        let refusal = finalize_rpc(&state, key)
            .await
            .expect_err("the first RPC over a typed runtime is refused");
        assert_eq!(refusal.code(), tonic::Code::Internal);
        assert_eq!(refusal.message(), FIRST_RPC_REFUSAL);
        if matches!(kill_at, KillAt::Accepted) {
            return (None, None);
        }
        let first = finalize_rpc(&state, key)
            .await
            .expect("the same-key RPC after it is accepted");
        assert_eq!(
            first.outcome,
            pb::TapeFinalizationOutcome::Finalizing as i32
        );
        let operation = Uuid::from_slice(first.operation_id.as_deref().unwrap()).unwrap();
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
        (before_seal, Some(reply))
    });
    // The kill: the worker is dropped where it awaits the actor's reply. No failure handler runs.
    drop(incarnation);
    drop(unanswered);
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
            match kill_at {
                KillAt::Accepted => pb::OperationState::Queued,
                _ => pb::OperationState::Running,
            } as i32,
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

/// The first RPC was answered with an error after the acceptance was durable, then the process
/// was killed. The same-key retry after the restart completes and seals once.
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

    let repaired_at_startup = !matches!(cut, SealCut::StaleNoStartupRepair);
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
