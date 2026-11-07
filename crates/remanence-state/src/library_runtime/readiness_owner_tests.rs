//! Every readiness owner produces a record (rulings 22 and 30). Each test calls the runtime
//! method production calls (`begin_owned_readiness`, `transition_readiness`) and reads the log
//! and the projection back: the decision-relevant facts are typed keys, the process-local device
//! paths are not recorded, the evidence path is one `Diagnostic`, an owner with no deadline
//! records `limit_ms: None`, and the two load-drive phases parse, record and replay.
use super::*;
use crate::{StateLockGuard, StatePaths};
use std::sync::Arc;

/// An `OperationProgress` diagnostic as (subject kind, subject id, detail entries).
type Progress = (String, Option<String>, Vec<(String, String)>);

struct Fixture {
    paths: StatePaths,
    runtime: LibraryRuntime,
    lease: Arc<StateLockGuard>,
    _root: tempfile::TempDir,
}

fn open() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let paths = StatePaths {
        config_path: root.path().join("config"),
        state_dir: root.path().join("state"),
        audit_dir: root.path().join("library"),
        journal_dir: root.path().join("native"),
        sqlite_path: root.path().join("catalog.sqlite"),
        tape_cache_dir: root.path().join("cache"),
        calibration_dir: root.path().join("calibration"),
    };
    let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
    let runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
    Fixture {
        paths,
        runtime,
        lease,
        _root: root,
    }
}

impl Fixture {
    /// A fresh owner input on a fresh operation: finite deadline, nothing else set.
    fn owner(&self) -> MediaReadinessOperationInput {
        let operation = self
            .runtime
            .start_owned_operation(
                lib::OperationKind::MediaReadinessWait,
                lib::AuditActor::System,
            )
            .unwrap();
        MediaReadinessOperationInput {
            operation_id: uuid::Uuid::from_bytes(operation),
            run_id: None,
            endpoint: crate::DriveKey::new("LIB", 0x100),
            changer_sg: None,
            drive_sg: None,
            drive_serial: None,
            barcode: Some("READY1L9".into()),
            source_slot: None,
            media_generation: Some(9),
            media_family: Some(crate::LtoGen::Lto9),
            phase: "readiness_poll".into(),
            state: "planned".into(),
            dirty_scope: Some("drive+tape".into()),
            deadline_at_utc: Some(deadline_in(60)),
            evidence_path: None,
        }
    }
    /// The observations of one operation, in log order.
    fn observations(&self, input: &MediaReadinessOperationInput) -> Vec<lib::ReadinessObserved> {
        self.runtime
            .test_source_records()
            .iter()
            .filter_map(|r| r.readiness_observation())
            .filter(|v| v.operation == *input.operation_id.as_bytes())
            .cloned()
            .collect()
    }
    /// `OperationProgress` diagnostics in log order, as (subject kind, subject id, detail).
    fn progress(&self) -> Vec<Progress> {
        self.runtime
            .test_source_records()
            .iter()
            .filter_map(|record| {
                let lib::BodyView::Diagnostic(d) = record.body_view() else {
                    return None;
                };
                assert_eq!(d.event, lib::DiagnosticEvent::OperationProgress);
                Some((
                    d.subject_kind.as_str().to_owned(),
                    d.subject_id.as_ref().map(|id| id.as_str().to_owned()),
                    d.detail
                        .0
                        .iter()
                        .map(|e| {
                            let lib::DiagnosticValue::Text(text) = &e.1 else {
                                panic!("a text detail: {e:?}");
                            };
                            (e.0.as_str().to_owned(), text.as_str().to_owned())
                        })
                        .collect(),
                ))
            })
            .collect()
    }
    /// The projected row after a rebuild from the log alone.
    fn projected(
        &self,
        input: &MediaReadinessOperationInput,
    ) -> crate::MediaReadinessOperationRecord {
        self.runtime
            .rebuild_projection(
                &mut crate::projector::open(&self.paths.sqlite_path)
                    .unwrap()
                    .with_library_runtime(self.runtime.clone())
                    .write_handle(),
            )
            .unwrap();
        self.runtime
            .reader()
            .unwrap()
            .media_readiness_operation(input.operation_id)
            .unwrap()
            .unwrap()
    }
}

fn deadline_in(seconds: i64) -> String {
    (OffsetDateTime::now_utc() + time::Duration::seconds(seconds))
        .format(&Rfc3339)
        .unwrap()
}

fn refused(error: RuntimeError, text: &str) {
    assert!(
        error.to_string().contains(text),
        "expected a refusal naming {text:?}, got {error}"
    );
}

/// `source_slot` and `run_id` are typed keys 15 and 16 of the owner's record, the projection
/// shows them, and a refresh keeps the owner's values.
#[test]
fn an_owner_with_a_source_slot_and_run_records_them_as_typed_keys() {
    let f = open();
    let run = uuid::Uuid::new_v4();
    let mut input = f.owner();
    input.source_slot = Some(0x0400);
    input.run_id = Some(run.to_string());
    f.runtime.begin_owned_readiness(&input).unwrap();

    let observed = f.observations(&input);
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].source_slot, Some(0x0400));
    assert_eq!(observed[0].run, Some(*run.as_bytes()));
    assert_eq!(observed[0].cancel_source, None);

    // A refresh that moves the phase is a new record carrying the creation facts forward.
    let mut refresh = input.clone();
    refresh.phase = "wait_ready_tur".into();
    refresh.source_slot = Some(0x0401);
    f.runtime.begin_owned_readiness(&refresh).unwrap();
    let observed = f.observations(&input);
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[1].source_slot, Some(0x0400));
    assert_eq!(observed[1].run, Some(*run.as_bytes()));

    let row = f.projected(&input);
    assert_eq!(row.source_slot, Some(0x0400));
    assert_eq!(row.run_id, Some(run.to_string()));
    assert_eq!(row.cancel_source, None);
}

/// The unowned entry (`begin_readiness`, which mints the operation) records the same typed keys
/// and the same absent limit.
#[test]
fn an_owner_that_mints_its_operation_records_the_same_keys() {
    let f = open();
    let run = uuid::Uuid::new_v4();
    let mut input = f.owner();
    input.source_slot = Some(0x0402);
    input.run_id = Some(run.to_string());
    input.deadline_at_utc = None;
    let minted = f.runtime.begin_readiness(&input).unwrap();

    let observed: Vec<_> = f
        .runtime
        .test_source_records()
        .iter()
        .filter_map(|r| r.readiness_observation())
        .filter(|v| v.operation == *minted.as_bytes())
        .cloned()
        .collect();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].source_slot, Some(0x0402));
    assert_eq!(observed[0].run, Some(*run.as_bytes()));
    assert_eq!(observed[0].limit_ms, None);
}

/// A run label that is not a UUID has no typed encoding and is refused, not dropped.
#[test]
fn a_run_id_that_is_not_a_uuid_is_refused_without_a_record() {
    let f = open();
    let mut input = f.owner();
    input.run_id = Some("fieldtest-run".into());
    let before = f.runtime.test_source_records().len();
    refused(
        f.runtime.begin_owned_readiness(&input).unwrap_err(),
        "run id is not a UUID",
    );
    assert_eq!(f.runtime.test_source_records().len(), before);
}

/// The changer and drive device paths are process-local: no record carries them, and the
/// projection has none.
#[test]
fn device_paths_are_not_recorded() {
    let f = open();
    let mut input = f.owner();
    input.changer_sg = Some("/dev/sg3-changer-path".into());
    input.drive_sg = Some("/dev/sg4-drive-path".into());
    f.runtime.begin_owned_readiness(&input).unwrap();

    let records = f.runtime.test_source_records();
    assert_eq!(f.observations(&input).len(), 1, "the owner is recorded");
    for record in &records {
        let text = format!("{record:?}");
        assert!(
            !text.contains("sg3-changer-path") && !text.contains("sg4-drive-path"),
            "a device path reached the log: {text}"
        );
    }
    assert!(
        f.progress().is_empty(),
        "a device path is not a diagnostic either"
    );
    let row = f.projected(&input);
    assert_eq!((row.changer_sg, row.drive_sg), (None, None));
}

/// `evidence_path` is presentation: one `OperationProgress` diagnostic naming the operation,
/// with the path as its one detail key. An exact retry adds none, an owner without one adds
/// none, and the projection's `evidence_path` stays empty.
#[test]
fn an_evidence_path_is_one_operation_progress_diagnostic_naming_the_operation() {
    let f = open();
    let path = "evidence/readiness.json";
    let mut input = f.owner();
    input.evidence_path = Some(path.into());
    f.runtime.begin_owned_readiness(&input).unwrap();
    f.runtime.begin_owned_readiness(&input).unwrap();

    assert_eq!(f.observations(&input).len(), 1, "the retry is exact");
    assert_eq!(
        f.progress(),
        vec![(
            "operation".to_owned(),
            Some(input.operation_id.to_string()),
            vec![("evidence_path".to_owned(), path.to_owned())],
        )],
        "one diagnostic, naming the operation"
    );
    assert_eq!(f.projected(&input).evidence_path, None);

    let plain = f.owner();
    f.runtime.begin_owned_readiness(&plain).unwrap();
    assert_eq!(f.progress().len(), 1, "an owner without a path adds none");
}

/// The diagnostic can fail without changing the owner: a path over the log's text bound is
/// refused by the fold (`InvalidSchema`, which is not a store failure), the owner is still
/// recorded, and motion is not fenced.
#[test]
fn an_unrecordable_evidence_path_leaves_the_owner_recorded_and_motion_unfenced() {
    let f = open();
    let mut input = f.owner();
    input.evidence_path = Some("e".repeat(5000));
    f.runtime.begin_owned_readiness(&input).unwrap();
    assert_eq!(f.observations(&input).len(), 1);
    assert!(f.progress().is_empty());
    f.runtime.ensure_motion_allowed().unwrap();
}

/// An owner with no deadline records `limit_ms: None`, and the projection has no deadline.
#[test]
fn an_owner_with_no_deadline_records_no_limit_and_projects_no_deadline() {
    let f = open();
    let mut input = f.owner();
    input.deadline_at_utc = None;
    f.runtime.begin_owned_readiness(&input).unwrap();

    assert_eq!(f.observations(&input)[0].limit_ms, None);
    let row = f.projected(&input);
    assert_eq!(row.deadline_at_utc, None);

    let finite = f.owner();
    f.runtime.begin_owned_readiness(&finite).unwrap();
    assert!(f.observations(&finite)[0].limit_ms.is_some_and(|ms| ms > 0));
    assert!(f.projected(&finite).deadline_at_utc.is_some());
}

/// D3, finite against finite: a refresh with the same deadline matches; one with another
/// deadline, earlier or later, is refused.
#[test]
fn a_finite_refresh_matches_a_finite_owner_only_at_its_own_deadline() {
    let f = open();
    let input = f.owner();
    f.runtime.begin_owned_readiness(&input).unwrap();
    f.runtime.begin_owned_readiness(&input).unwrap();
    assert_eq!(
        f.observations(&input).len(),
        1,
        "the same deadline is an exact retry"
    );

    for seconds in [30, 90] {
        let mut other = input.clone();
        other.deadline_at_utc = Some(deadline_in(seconds));
        refused(
            f.runtime.begin_owned_readiness(&other).unwrap_err(),
            "readiness owner refresh changed immutable creation facts",
        );
    }
    assert_eq!(f.observations(&input).len(), 1);
}

/// D3, none against none: a refresh of a no-deadline owner matches, as an exact retry and as a
/// phase change that carries `limit_ms: None` forward.
#[test]
fn a_no_deadline_refresh_matches_a_no_deadline_owner() {
    let f = open();
    let mut input = f.owner();
    input.deadline_at_utc = None;
    f.runtime.begin_owned_readiness(&input).unwrap();
    f.runtime.begin_owned_readiness(&input).unwrap();
    assert_eq!(
        f.observations(&input).len(),
        1,
        "the exact retry appends nothing"
    );

    let mut moved = input.clone();
    moved.phase = "wait_ready_tur".into();
    f.runtime.begin_owned_readiness(&moved).unwrap();
    let observed = f.observations(&input);
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[1].limit_ms, None);
}

/// D3, finite refresh of a no-deadline owner: refused.
#[test]
fn a_finite_refresh_of_a_no_deadline_owner_is_refused() {
    let f = open();
    let mut input = f.owner();
    input.deadline_at_utc = None;
    f.runtime.begin_owned_readiness(&input).unwrap();

    let mut finite = input.clone();
    finite.deadline_at_utc = Some(deadline_in(60));
    refused(
        f.runtime.begin_owned_readiness(&finite).unwrap_err(),
        "readiness owner refresh changed immutable creation facts",
    );
    assert_eq!(f.observations(&input).len(), 1);
}

/// D3, no-deadline refresh of a finite owner: refused.
#[test]
fn a_no_deadline_refresh_of_a_finite_owner_is_refused() {
    let f = open();
    let input = f.owner();
    f.runtime.begin_owned_readiness(&input).unwrap();

    let mut open_ended = input.clone();
    open_ended.deadline_at_utc = None;
    refused(
        f.runtime.begin_owned_readiness(&open_ended).unwrap_err(),
        "readiness owner refresh changed immutable creation facts",
    );
    assert_eq!(f.observations(&input).len(), 1);
}

/// Phases 49 and 50 parse from the spellings producers send, are recorded (as an owner's phase
/// and as a transition's), and a reopened store replays both into the projection.
#[test]
fn the_load_drive_phases_parse_record_and_replay() {
    assert_eq!(
        phase("load_drive_readiness").unwrap() as u16,
        49,
        "phase 49"
    );
    assert_eq!(
        phase("load_drive_completion").unwrap() as u16,
        50,
        "phase 50"
    );

    let f = open();
    let mut input = f.owner();
    input.phase = "load_drive_readiness".into();
    f.runtime.begin_owned_readiness(&input).unwrap();
    f.runtime
        .transition_readiness(&MediaReadinessTransitionInput {
            completion_evidence: None,
            operation_id: input.operation_id,
            phase: Some("load_drive_completion".into()),
            state: "becoming_ready".into(),
            dirty_scope: Some("drive+tape".into()),
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
    let phases = |observed: &[lib::ReadinessObserved]| {
        observed.iter().map(|v| v.phase as u16).collect::<Vec<_>>()
    };
    assert_eq!(phases(&f.observations(&input)), [49, 50]);
    assert_eq!(f.projected(&input).phase, "load_drive_completion");

    let Fixture {
        paths,
        runtime,
        lease,
        _root,
    } = f;
    drop(runtime);
    let reopened = LibraryRuntime::open(&paths, true, lease).expect("the log reopens");
    let replayed: Vec<_> = reopened
        .test_source_records()
        .iter()
        .filter_map(|r| r.readiness_observation())
        .map(|v| v.phase as u16)
        .collect();
    assert_eq!(replayed, [49, 50], "the reopened store replays both phases");
    assert_eq!(
        reopened
            .reader()
            .unwrap()
            .media_readiness_operation(input.operation_id)
            .unwrap()
            .unwrap()
            .phase,
        "load_drive_completion"
    );
}

/// The Ready observation that closes a readiness-owned LOAD carries the owner's `source_slot` and
/// `run` forward: the projection upserts them from every observation, so a record that dropped
/// them would blank the row.
#[test]
fn the_ready_observation_that_closes_a_load_keeps_the_owners_creation_facts() {
    use remanence_library::motion::{
        LoadConfirmation, MotionAuthority, MotionCommand, MotionContext, PendingMotion,
    };
    let f = open();
    let run = uuid::Uuid::new_v4();
    let mut input = f.owner();
    input.source_slot = Some(0x0400);
    input.run_id = Some(run.to_string());
    f.runtime.begin_owned_readiness(&input).unwrap();

    let command = MotionCommand::Load(input.endpoint.clone());
    let context = MotionContext {
        parent_operation: Some(*input.operation_id.as_bytes()),
        load_confirmation: LoadConfirmation::Readiness,
        ..Default::default()
    };
    let ticket = f.runtime.intent(&command, &context).unwrap();
    f.runtime
        .confirm_readiness(
            &PendingMotion {
                command,
                context,
                ticket,
                evidence: remanence_library::outcome::CompletionEvidence::success(),
            },
            &remanence_library::MediaReadiness::new(remanence_library::MediaReadinessKind::Ready),
            std::time::Duration::from_secs(1),
        )
        .unwrap();

    let observed = f.observations(&input);
    let ready = observed.last().unwrap();
    assert_eq!(ready.result, lib::ReadinessResult::Ready);
    assert_eq!(ready.source_slot, Some(0x0400));
    assert_eq!(ready.run, Some(*run.as_bytes()));
    let row = f.projected(&input);
    assert_eq!(row.source_slot, Some(0x0400));
    assert_eq!(row.run_id, Some(run.to_string()));
}

/// Key 17 projects as its legacy spelling when it is on an owner record. The owner input has no
/// cancel source (only transitions carry one, tested above), so this record is appended through
/// the authority's own owner writer.
#[test]
fn a_recorded_cancel_source_projects_as_its_legacy_spelling() {
    let f = open();
    for (source, legacy) in [
        (lib::CancelSource::Operator, "operator"),
        (lib::CancelSource::Shutdown, "shutdown"),
        (lib::CancelSource::StartupReconcile, "startup_reconcile"),
    ] {
        let input = f.owner();
        let operation = *input.operation_id.as_bytes();
        let started_unix_ns =
            u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos()).unwrap();
        let endpoint = readiness_endpoint(&input);
        f.runtime
            .append(|a, v| {
                let (_, ready_generation) = a.readiness_context(operation, &endpoint)?;
                a.begin_readiness_for_operation(
                    lib::ReadinessObserved {
                        source_slot: None,
                        run: None,
                        cancel_source: Some(source),
                        operation,
                        endpoint: endpoint.clone(),
                        tape: None,
                        barcode: None,
                        phase: lib::ReadinessPhase::Cancelled,
                        state: lib::ReadinessState::Cancelled,
                        previous: None,
                        started_unix_ns,
                        limit_ms: Some(60_000),
                        evidence: None,
                        load_intent: None,
                        ready_generation,
                        result: lib::ReadinessResult::Cancelled,
                        scope: lib::ReadinessScope::Endpoint,
                        media: None,
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap();
        assert_eq!(f.observations(&input)[0].cancel_source, Some(source));
        assert_eq!(f.projected(&input).cancel_source.as_deref(), Some(legacy));
    }
}

fn readiness_endpoint(input: &MediaReadinessOperationInput) -> lib::DriveKey {
    crate::library_runtime::motion::endpoint(&input.endpoint)
}

/// A transition of `input`'s operation that changes nothing but what a test sets.
fn transition(input: &MediaReadinessOperationInput) -> MediaReadinessTransitionInput {
    MediaReadinessTransitionInput {
        completion_evidence: None,
        operation_id: input.operation_id,
        phase: None,
        state: "planned".into(),
        dirty_scope: Some("drive+tape".into()),
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
    }
}

/// A transition's cancel source is typed key 17 for `operator`, `shutdown` and
/// `startup_reconcile`; the projection shows its legacy spelling; a later transition without one
/// clears it, as every legacy transition did; and a change of cancel source alone is not an exact
/// retry.
#[test]
fn a_transition_records_its_cancel_source_as_the_typed_key() {
    let f = open();
    for (spelling, source) in [
        ("operator", lib::CancelSource::Operator),
        ("shutdown", lib::CancelSource::Shutdown),
        ("startup_reconcile", lib::CancelSource::StartupReconcile),
    ] {
        let input = f.owner();
        f.runtime.begin_owned_readiness(&input).unwrap();
        let mut cancelled = transition(&input);
        cancelled.cancel_source = Some(spelling.into());
        // The same phase, state and scope as the owner: only the cancel source is new.
        f.runtime.transition_readiness(&cancelled).unwrap();
        f.runtime.transition_readiness(&cancelled).unwrap();
        let observed = f.observations(&input);
        assert_eq!(
            observed.len(),
            2,
            "{spelling}: appended once, then an exact retry"
        );
        assert_eq!(observed[1].cancel_source, Some(source));
        assert_eq!(f.projected(&input).cancel_source.as_deref(), Some(spelling));

        let mut resumed = transition(&input);
        resumed.state = "becoming_ready".into();
        f.runtime.transition_readiness(&resumed).unwrap();
        let observed = f.observations(&input);
        assert_eq!(observed[2].cancel_source, None, "{spelling}: cleared");
        assert_eq!(f.projected(&input).cancel_source, None);
    }
    assert!(
        f.progress().is_empty(),
        "a typed source needs no diagnostic"
    );
}

/// The CLI's `signal` spelling has no typed variant and no production reader branches on it: it
/// records no key 17, and the diagnostic names the spelling and the signal.
#[test]
fn the_cli_signal_spelling_records_no_key_and_is_named_in_a_diagnostic() {
    let f = open();
    let input = f.owner();
    f.runtime.begin_owned_readiness(&input).unwrap();
    let mut interrupted = transition(&input);
    interrupted.state = "aborted_unknown".into();
    interrupted.cancel_source = Some("signal".into());
    interrupted.signal = Some("SIGINT".into());
    interrupted.quarantine_id = Some(format!("mrq-{}", input.operation_id));
    f.runtime.transition_readiness(&interrupted).unwrap();

    let observed = f.observations(&input);
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[1].cancel_source, None);
    assert_eq!(observed[1].result, lib::ReadinessResult::AbortedUnknown);
    assert_eq!(
        f.progress(),
        vec![(
            "operation".to_owned(),
            Some(input.operation_id.to_string()),
            vec![
                ("cancel_source".to_owned(), "signal".to_owned()),
                ("signal".to_owned(), "SIGINT".to_owned()),
            ],
        )]
    );
}

/// `signal`, `last_error_json` and `evidence_path` are presentation: one diagnostic per appended
/// observation, keys ascending, nothing for an exact retry or a transition without them.
#[test]
fn a_transitions_presentation_fields_are_one_operation_progress_diagnostic() {
    let f = open();
    let input = f.owner();
    f.runtime.begin_owned_readiness(&input).unwrap();
    let mut failed = transition(&input);
    failed.state = "becoming_ready".into();
    failed.signal = Some("SIGTERM".into());
    failed.last_error_json = Some("{\"detail\":\"timed out\"}".into());
    failed.evidence_path = Some("evidence/readiness.json".into());
    f.runtime.transition_readiness(&failed).unwrap();
    f.runtime.transition_readiness(&failed).unwrap();
    assert_eq!(f.observations(&input).len(), 2, "the retry is exact");
    assert_eq!(
        f.progress(),
        vec![(
            "operation".to_owned(),
            Some(input.operation_id.to_string()),
            vec![
                (
                    "evidence_path".to_owned(),
                    "evidence/readiness.json".to_owned()
                ),
                (
                    "last_error_json".to_owned(),
                    "{\"detail\":\"timed out\"}".to_owned()
                ),
                ("signal".to_owned(), "SIGTERM".to_owned()),
            ],
        )]
    );

    let mut plain = transition(&input);
    plain.state = "target_busy".into();
    f.runtime.transition_readiness(&plain).unwrap();
    assert_eq!(f.progress().len(), 1, "a transition without them adds none");
}

/// A cancel source outside the four legacy spellings is refused without a record.
#[test]
fn an_unknown_cancel_source_is_refused_without_a_record() {
    let f = open();
    let input = f.owner();
    f.runtime.begin_owned_readiness(&input).unwrap();
    let mut bogus = transition(&input);
    bogus.cancel_source = Some("bogus".into());
    let before = f.runtime.test_source_records().len();
    refused(
        f.runtime.transition_readiness(&bogus).unwrap_err(),
        "unsupported cancel source",
    );
    assert_eq!(f.runtime.test_source_records().len(), before);
}

/// `load_drive_readiness` is a TUR phase: a Ready transition on it, with its successful probe
/// and opcode 0, is accepted, and the projection shows the phase and the opcode.
#[test]
fn a_ready_transition_on_the_load_drive_readiness_phase_is_accepted() {
    use remanence_library::motion::{
        LoadConfirmation, MotionAuthority, MotionCommand, MotionContext,
    };
    let f = open();
    let mut input = f.owner();
    input.phase = "load_drive_readiness".into();
    f.runtime.begin_owned_readiness(&input).unwrap();
    f.runtime
        .intent(
            &MotionCommand::Load(input.endpoint.clone()),
            &MotionContext {
                parent_operation: Some(*input.operation_id.as_bytes()),
                load_confirmation: LoadConfirmation::Readiness,
                ..Default::default()
            },
        )
        .unwrap();
    let mut ready = transition(&input);
    ready.state = "ready".into();
    ready.dirty_scope = Some("none".into());
    ready.last_cdb_opcode = Some(0x00);
    ready.completion_evidence =
        Some((&remanence_library::outcome::CompletionEvidence::success()).into());
    f.runtime.transition_readiness(&ready).unwrap();

    let observed = f.observations(&input);
    let last = observed.last().unwrap();
    assert_eq!(last.phase, lib::ReadinessPhase::LoadDriveReadiness);
    assert_eq!(last.result, lib::ReadinessResult::Ready);
    let row = f.projected(&input);
    assert_eq!(row.phase, "load_drive_readiness");
    assert_eq!(row.state, "ready");
    assert_eq!(row.last_cdb_opcode, Some(0));
}

/// The completion of the drive's LOAD, as a CHECK CONDITION (NOT READY, BECOMING READY).
fn load_check_condition() -> lib::CompletionEvidence {
    let mut sense = vec![0_u8; 32];
    sense[0] = 0x70;
    sense[2] = 0x02;
    sense[7] = 24;
    sense[12] = 0x04;
    sense[13] = 0x01;
    lib::CompletionEvidence {
        dispatch: lib::Dispatch::Issued,
        completion: lib::Completion::Known,
        scsi_status: Some(2),
        host_status: Some(0),
        driver_status: Some(0),
        sense: Some(lib::Blob(sense)),
        residual: None,
    }
}

/// `load_drive_completion` records the drive LOAD's completion, so its opcode is LOAD (0x1b): a
/// transition carrying LOAD evidence and that opcode is accepted and the projection shows it. A
/// TEST UNIT READY opcode on it, or a Ready, is refused, and is not recorded.
#[test]
fn a_load_drive_completion_transition_carrying_load_evidence_is_accepted() {
    let f = open();
    let mut input = f.owner();
    input.phase = "load_drive_readiness".into();
    f.runtime.begin_owned_readiness(&input).unwrap();

    let mut completion = transition(&input);
    completion.phase = Some("load_drive_completion".into());
    completion.state = "becoming_ready".into();
    completion.completion_evidence = Some(load_check_condition());
    completion.last_cdb_opcode = Some(0x1b);
    completion.last_sense_key = Some(2);
    completion.last_asc = Some(0x04);
    completion.last_ascq = Some(0x01);
    completion.target_status = Some(2);

    let before = f.runtime.test_source_records().len();
    let mut as_tur = completion.clone();
    as_tur.last_cdb_opcode = Some(0x00);
    refused(
        f.runtime.transition_readiness(&as_tur).unwrap_err(),
        "opcode cannot be derived from its source phase",
    );
    let mut ready = completion.clone();
    ready.state = "ready".into();
    ready.dirty_scope = Some("none".into());
    refused(
        f.runtime.transition_readiness(&ready).unwrap_err(),
        "ready observation requires an actual successful readiness probe",
    );
    assert_eq!(f.runtime.test_source_records().len(), before);

    f.runtime.transition_readiness(&completion).unwrap();
    let last = f.observations(&input).pop().unwrap();
    assert_eq!(last.phase, lib::ReadinessPhase::LoadDriveCompletion);
    assert_eq!(last.evidence, Some(load_check_condition()));
    let row = f.projected(&input);
    assert_eq!(row.phase, "load_drive_completion");
    assert_eq!(row.last_cdb_opcode, Some(0x1b));
}

/// A `transport_class` with no completion evidence behind it is presentation: the transition is
/// accepted and the diagnostic names the class. With evidence it is what the evidence says, and
/// nothing is recorded for it.
#[test]
fn an_unproven_transport_class_is_presentation_and_a_proven_one_is_not_recorded() {
    let f = open();
    let input = f.owner();
    f.runtime.begin_owned_readiness(&input).unwrap();
    let mut unresolved = transition(&input);
    unresolved.state = "aborted_unknown".into();
    unresolved.transport_class = Some("unknown".into());
    unresolved.quarantine_id = Some(format!("mrq-{}", input.operation_id));
    f.runtime.transition_readiness(&unresolved).unwrap();
    assert_eq!(
        f.progress(),
        vec![(
            "operation".to_owned(),
            Some(input.operation_id.to_string()),
            vec![("transport_class".to_owned(), "unknown".to_owned())],
        )]
    );

    let proven = f.owner();
    f.runtime.begin_owned_readiness(&proven).unwrap();
    let mut with_evidence = transition(&proven);
    with_evidence.state = "becoming_ready".into();
    with_evidence.transport_class = Some("unknown".into());
    with_evidence.completion_evidence = Some(load_check_condition());
    f.runtime.transition_readiness(&with_evidence).unwrap();
    assert_eq!(f.progress().len(), 1, "no diagnostic for a proven class");
}

/// Every (phase, opcode) pair production records with an opcode, from the producers of the CLI's
/// tape init and wait-ready, the daemon's session open and load-drive, and startup: the pair is
/// accepted and the opcode's command is the phase's. The `Before...`/`After...` phases issue no
/// command and carry none.
const PRODUCED_OPCODE_PAIRS: [(&str, u8); 15] = [
    ("readiness_poll", 0x00),
    ("wait_ready_tur", 0x00),
    ("pre_load_tur", 0x00),
    ("already_loaded_tur", 0x00),
    ("startup_reconcile_tur", 0x00),
    ("session_open_short_probe", 0x00),
    ("session_open_after_immediate_load", 0x00),
    ("load_drive_readiness", 0x00),
    ("load_drive_completion", 0x1b),
    ("initializing_command_load", 0x1b),
    ("conditional_immediate_load", 0x1b),
    ("session_open_immediate_load", 0x1b),
    ("move_medium", 0xa5),
    ("rewind_before_bot_read", 0x01),
    ("rewind_before_bootstrap_write", 0x01),
];

#[test]
fn every_produced_phase_opcode_pair_is_accepted_and_a_different_opcode_is_not() {
    let f = open();
    for (phase_name, opcode) in PRODUCED_OPCODE_PAIRS {
        let input = f.owner();
        f.runtime.begin_owned_readiness(&input).unwrap();
        let mut command = transition(&input);
        command.phase = Some(phase_name.into());
        command.state = "becoming_ready".into();
        command.last_cdb_opcode = Some(opcode);
        f.runtime
            .transition_readiness(&command)
            .unwrap_or_else(|e| panic!("({phase_name}, {opcode:#04x}) is refused: {e}"));
        assert_eq!(
            f.observations(&input).last().unwrap().phase,
            phase(phase_name).unwrap(),
            "{phase_name}"
        );

        let mut wrong = transition(&input);
        wrong.phase = Some(phase_name.into());
        wrong.state = "target_busy".into();
        wrong.last_cdb_opcode = Some(opcode ^ 0x40);
        refused(
            f.runtime.transition_readiness(&wrong).unwrap_err(),
            "opcode cannot be derived from its source phase",
        );
    }
    // No command is issued in the `Before...`/`After...` phases: an opcode on one is refused.
    let input = f.owner();
    f.runtime.begin_owned_readiness(&input).unwrap();
    for phase_name in [
        "before_open_library",
        "after_open_library",
        "before_move_medium",
        "after_move_medium",
        "before_pre_load_tur",
        "after_pre_load_tur",
    ] {
        let mut bogus = transition(&input);
        bogus.phase = Some(phase_name.into());
        bogus.state = "becoming_ready".into();
        bogus.last_cdb_opcode = Some(0x00);
        refused(
            f.runtime.transition_readiness(&bogus).unwrap_err(),
            "opcode cannot be derived from its source phase",
        );
    }
}

/// Only a TEST UNIT READY phase carries a Ready, with a successful probe: a Ready on a phase
/// whose command is a LOAD, MOVE MEDIUM or REWIND is refused even with a successful completion
/// and the phase's own opcode, and one on `pre_load_tur` or `already_loaded_tur` is accepted.
#[test]
fn a_ready_is_accepted_only_on_a_test_unit_ready_phase() {
    let f = open();
    for (phase_name, opcode) in PRODUCED_OPCODE_PAIRS {
        let input = f.owner();
        f.runtime.begin_owned_readiness(&input).unwrap();
        let mut ready = transition(&input);
        ready.phase = Some(phase_name.into());
        ready.state = "ready".into();
        ready.dirty_scope = Some("none".into());
        ready.last_cdb_opcode = Some(opcode);
        ready.completion_evidence =
            Some((&remanence_library::outcome::CompletionEvidence::success()).into());
        let result = f.runtime.transition_readiness(&ready);
        if opcode == 0x00 {
            result.unwrap_or_else(|e| panic!("a Ready on {phase_name} is refused: {e}"));
        } else {
            refused(
                result.unwrap_err(),
                "ready observation requires an actual successful readiness probe",
            );
        }
    }
}
