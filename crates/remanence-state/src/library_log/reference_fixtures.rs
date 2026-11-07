//! Store-written reference logs and exact production-reader manifests.
use super::store::*;
use super::tests::*;
use super::*;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};
fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}
fn body(l: &mut LibraryLog, b: Body) -> Receipt {
    put(l, b)
}
/// Every allocated record is published through the real append owner.
fn all_records(path: &Path) {
    let (mut l, _) = open(path, &Present);
    body(&mut l, provision(1, Some("A")));
    body(&mut l, provision(2, Some("B")));
    body(&mut l, provision(3, None));
    let defs = Definitions {
        pools: Bounded(vec![
            PoolDefinition {
                id: "pool-a".into(),
                display_name: Some("Pool A".into()),
                copy_class: None,
                content_class: None,
                scheme: None,
                selection_policy: SelectionPolicy::CompleteOrFill,
                watermark_low_bits: 0.8f64.to_bits(),
                watermark_high_bits: 0.9f64.to_bits(),
                capacity_cap_bytes: None,
                block_size_bytes: 1048576,
                min_object_size_bytes: 0,
            },
            PoolDefinition {
                id: "pool-b".into(),
                display_name: None,
                copy_class: Some("secondary".into()),
                content_class: Some("archive".into()),
                scheme: Some("rs-cauchy-gf256-v1".into()),
                selection_policy: SelectionPolicy::FillOldest,
                watermark_low_bits: 0.7f64.to_bits(),
                watermark_high_bits: 0.95f64.to_bits(),
                capacity_cap_bytes: Some(1_000_000),
                block_size_bytes: 1048576,
                min_object_size_bytes: 4096,
            },
        ]),
        rules: Bounded(vec![PoolRule {
            prefix: "A".into(),
            pool_id: "pool-a".into(),
        }]),
        cleaning_prefixes: Bounded(vec!["CLN".into()]),
    };
    let config = body(
        &mut l,
        Body::ConfigurationAccepted(ConfigurationAccepted {
            generation: 1,
            config_digest: codec::configuration_digest(&defs).unwrap(),
            definitions: defs,
        }),
    );
    body(&mut l, pool(1, 1));
    body(
        &mut l,
        Body::AssignPool(AssignPool {
            tape: tape(2),
            pool: None,
            change: GenerationChange {
                expected: 1,
                next: 2,
            },
            source: AssignmentSource::Rule,
            configuration: Some(reference(&config)),
        }),
    );
    body(
        &mut l,
        Body::Label(Label {
            tape: tape(2),
            label: Some("A".into()),
            provenance: LabelProvenance::Operator,
            change: GenerationChange {
                expected: 1,
                next: 2,
            },
            prior_holder: Some(PriorHolder {
                tape: tape(1),
                change: GenerationChange {
                    expected: 1,
                    next: 2,
                },
            }),
            observed_bot_uuid: Some(tape(2).uuid),
        }),
    );
    body(
        &mut l,
        Body::Label(Label {
            tape: tape(2),
            label: None,
            provenance: LabelProvenance::Changer,
            change: GenerationChange {
                expected: 2,
                next: 3,
            },
            prior_holder: None,
            observed_bot_uuid: None,
        }),
    );
    body(
        &mut l,
        Body::MediumSpecSet(MediumSpecSet {
            tape: tape(1),
            medium: MediumSpec {
                source: GenerationSource::Operator,
                ..medium_spec()
            },
            change: GenerationChange {
                expected: 1,
                next: 2,
            },
        }),
    );
    body(
        &mut l,
        Body::SupersedeTail(SupersedeTail {
            at: at(1, 1),
            operation: Some([9; 16]),
            session_id: Some([10; 16]),
            preview_digest: Some([11; 32]),
            tail_file_count: 2,
        }),
    );
    body(
        &mut l,
        Body::AcceptUnprotectedFinalization(AcceptUnprotectedFinalization {
            at: at(1, 1),
            operation: [12; 16],
            w: 2,
            t: 3,
            reason: "accepted uncovered tail".into(),
        }),
    );
    // B4b-3's explicitly ruled fence/alarm additions exercise the real writer.
    let mut barrier = fence(1);
    if let Body::FenceRaised(v) = &mut barrier {
        v.cause = FenceCause::CheckpointBarrierFailed;
    }
    let barrier = body(&mut l, barrier);
    body(
        &mut l,
        Body::FenceReleased(FenceReleased {
            detail: Bounded(vec![]),
            fence: barrier.sequence,
            evidence: "barrier restored".into(),
            forced: false,
        }),
    );
    for (key, kind) in [
        ("contradicted-tape", AlarmKind::ContradictedTape),
        ("pending-record-rejected", AlarmKind::PendingRecordRejected),
    ] {
        body(
            &mut l,
            Body::AlarmChanged(AlarmChanged {
                condition_key: key.into(),
                change: GenerationChange {
                    expected: 0,
                    next: 1,
                },
                action: AlarmAction::Raise(AlarmRaise {
                    kind,
                    severity: Severity::Critical,
                    reason: "authority evidence".into(),
                }),
                cause: None,
            }),
        );
    }
    let raise = body(&mut l, fence(1));
    body(
        &mut l,
        Body::FenceReleased(FenceReleased {
            detail: Bounded(vec![]),
            fence: raise.sequence,
            evidence: "settled".into(),
            forced: false,
        }),
    );
    let raise = body(
        &mut l,
        Body::FenceRaised(FenceRaised {
            detail: Bounded(vec![]),
            kind: FenceKind::DriveFault,
            scope: FenceScope::Drive(endpoint("LIBB0002")),
            cause: FenceCause::Cleaning,
            evidence: None,
            at: None,
            operation: Some([13; 16]),
        }),
    );
    body(
        &mut l,
        Body::FenceReleased(FenceReleased {
            detail: Bounded(vec![]),
            fence: raise.sequence,
            evidence: "operator proof".into(),
            forced: true,
        }),
    );
    for attempt in 1..=3 {
        let i = body(&mut l, intent(attempt));
        let confirmation = if attempt == 2 {
            let ready = body(
                &mut l,
                Body::ReadinessObserved(ReadinessObserved {
                    source_slot: None,
                    run: None,
                    cancel_source: None,
                    scope: ReadinessScope::Endpoint,
                    media: None,
                    operation: [0x71; 16],
                    endpoint: endpoint("LIBA0001"),
                    tape: None,
                    barcode: None,
                    phase: ReadinessPhase::Ready,
                    state: ReadinessState::Ready,
                    previous: None,
                    started_unix_ns: NOW,
                    limit_ms: Some(1000),
                    evidence: None,
                    load_intent: Some(reference(&i)),
                    ready_generation: 1,
                    result: ReadinessResult::Ready,
                }),
            );
            Some(Confirmation::Readiness(ready.sequence))
        } else {
            None
        };
        if attempt < 3 {
            body(
                &mut l,
                Body::MotionOutcome(MotionOutcome {
                    intent: reference(&i),
                    result: if attempt == 1 {
                        MotionResult::Failed
                    } else {
                        MotionResult::Done
                    },
                    evidence: evidence(),
                    confirmation,
                }),
            );
        } else {
            body(
                &mut l,
                Body::InventoryReconciled(InventoryReconciled {
                    subject: InventorySubject::Library("LIBA0001".into()),
                    observation_generation: 1,
                    resolves: Bounded(vec![reference(&i)]),
                    complete: false,
                    elements: Bounded(vec![Element {
                        address: 1,
                        kind: ElementKind::Drive,
                        barcode: Some("A".into()),
                        full: true,
                    }]),
                }),
            );
        }
    }
    let i = body(
        &mut l,
        Body::MotionIntent(MotionIntent {
            command: [20; 16],
            parent_operation: None,
            motion: Motion::Move(Move {
                library_serial: "LIBA0001".into(),
                source: 1,
                destination: 2,
            }),
            tape: None,
            inventory_generation: Some(1),
            readiness_generation: None,
        }),
    );
    body(
        &mut l,
        Body::MotionOutcome(MotionOutcome {
            intent: reference(&i),
            result: MotionResult::Unknown,
            evidence: CompletionEvidence {
                dispatch: Dispatch::NotIssued,
                completion: Completion::Unknown,
                scsi_status: None,
                host_status: None,
                driver_status: None,
                sense: None,
                residual: None,
            },
            confirmation: None,
        }),
    );
    let inventory_confirmation = body(
        &mut l,
        Body::InventoryReconciled(InventoryReconciled {
            subject: InventorySubject::Library("LIBA0001".into()),
            observation_generation: 2,
            resolves: Bounded(vec![reference(&i)]),
            complete: true,
            elements: Bounded(vec![]),
        }),
    );
    let standalone = DriveKey::Standalone(Standalone {
        drive_serial: "STANDALONE1".into(),
    });
    let i = body(
        &mut l,
        Body::MotionIntent(MotionIntent {
            command: [21; 16],
            parent_operation: None,
            motion: Motion::Unload(Unload {
                endpoint: standalone.clone(),
            }),
            tape: None,
            inventory_generation: None,
            readiness_generation: Some(1),
        }),
    );
    body(
        &mut l,
        Body::MotionOutcome(MotionOutcome {
            intent: reference(&i),
            result: MotionResult::Done,
            evidence: evidence(),
            confirmation: Some(Confirmation::Inventory(inventory_confirmation.sequence)),
        }),
    );
    body(&mut l, progress(1));
    let p = body(&mut l, progress(2));
    let mut v = verdict();
    if let Body::VerificationVerdict(v) = &mut v {
        v.progress = Some(reference(&p));
        v.edition_digest = Some([22; 32]);
    }
    body(&mut l, v);
    let mut v = verdict();
    if let Body::VerificationVerdict(v) = &mut v {
        v.operation = [23; 16];
        v.outcome = VerificationOutcome::VerifiedDegraded;
        v.damaged = Bounded(vec![DamagedRange {
            partition: 0,
            start_lba: 1,
            end_lba: 2,
        }]);
        v.unprotected = Some(Unprotected { w: 1, t: 2 });
    }
    body(&mut l, v);
    body(
        &mut l,
        Body::ManualFinalizationAccepted(ManualFinalizationAccepted {
            at: at(1, 1),
            operation: [24; 16],
            actor_fingerprint: "operator".into(),
            idempotency_key: [24; 16],
            request_fingerprint: [24; 32],
            assignment_generation: 2,
        }),
    );
    let i = body(
        &mut l,
        Body::MotionIntent(MotionIntent {
            command: [25; 16],
            parent_operation: None,
            motion: Motion::Load(Load {
                endpoint: standalone.clone(),
                attempt: 1,
            }),
            tape: Some(tape(3)),
            inventory_generation: None,
            readiness_generation: None,
        }),
    );
    let r = body(
        &mut l,
        Body::ReadinessObserved(ReadinessObserved {
            source_slot: None,
            run: None,
            cancel_source: None,
            scope: ReadinessScope::Endpoint,
            media: None,
            operation: [25; 16],
            endpoint: standalone.clone(),
            tape: Some(tape(3)),
            barcode: None,
            phase: ReadinessPhase::ReadinessPoll,
            state: ReadinessState::BecomingReady,
            previous: None,
            started_unix_ns: NOW,
            limit_ms: Some(1000),
            evidence: Some(evidence()),
            load_intent: Some(reference(&i)),
            ready_generation: 1,
            result: ReadinessResult::Pending,
        }),
    );
    body(
        &mut l,
        Body::ReadinessObserved(ReadinessObserved {
            source_slot: None,
            run: None,
            cancel_source: None,
            scope: ReadinessScope::Endpoint,
            media: None,
            operation: [25; 16],
            endpoint: standalone.clone(),
            tape: None,
            barcode: Some("A".into()),
            phase: ReadinessPhase::Ready,
            state: ReadinessState::Ready,
            previous: Some(reference(&r)),
            started_unix_ns: NOW,
            limit_ms: Some(1000),
            evidence: None,
            load_intent: None,
            ready_generation: 2,
            result: ReadinessResult::Ready,
        }),
    );
    for d in [30u8, 31] {
        body(
            &mut l,
            Body::DriveObserved(DriveObserved {
                drive_uuid: [d; 16],
                serial: Some(format!("DRIVE{d}").as_str().into()),
                identity_source: IdentitySource::DvcidInline,
                endpoint: Some(if d == 30 {
                    endpoint("LIBA0001")
                } else {
                    standalone.clone()
                }),
                managed: Managed::Rem,
                inventory_generation: 1,
                vendor: Some("vendor".into()),
                product: None,
                firmware_rev: None,
            }),
        );
    }
    for (g, decision) in [
        Stewardship::Retire(Reason {
            reason: "maintenance".into(),
        }),
        Stewardship::Reinstate(Reason {
            reason: "ready".into(),
        }),
        Stewardship::Annotate(Annotation {
            purchase_date: Some("2026-01-01".into()),
            warranty_until: None,
            cost: Some("100".into()),
            notes: None,
        }),
    ]
    .into_iter()
    .enumerate()
    {
        body(
            &mut l,
            Body::DriveDecision(DriveDecision {
                drive_uuid: [30; 16],
                decision,
                change: GenerationChange {
                    expected: g as u64 + 1,
                    next: g as u64 + 2,
                },
            }),
        );
    }
    body(
        &mut l,
        Body::DriveHealthObserved(DriveHealthObserved {
            drive_uuid: [30; 16],
            tape: Some(tape(1)),
            session_id: Some([33; 16]),
            operation: None,
            trigger: HealthTrigger::SessionClose,
            tape_alert_flags: 1,
            counters: Counters {
                write_corrected: Some(2),
                write_uncorrected: None,
                read_corrected: Some(3),
                read_uncorrected: None,
            },
            raw_pages: Some(Blob(vec![1, 2, 3])),
        }),
    );
    let cartridge = cleaning_cartridge_ref(&"CLN000".into());
    body(
        &mut l,
        Body::CleaningRecorded(CleaningRecorded {
            run_id: "cartridge".into(),
            previous: None,
            drive_uuid: None,
            cartridge: Some(cartridge),
            action: CleaningAction::Register(Register {
                barcode: "CLN000".into(),
                home_slot: 1,
            }),
        }),
    );
    let mut prev = None;
    for action in [
        CleaningAction::Begin(Begin {
            trigger: CleanTrigger::Manual,
        }),
        CleaningAction::Advance(Advance {
            phase: CleanPhase::Cleaning,
            home_slot: None,
        }),
        CleaningAction::Finish(Finish {
            result: CleanResult::Done,
            verified_credit: true,
        }),
    ] {
        let r = body(
            &mut l,
            Body::CleaningRecorded(CleaningRecorded {
                run_id: "run".into(),
                previous: prev,
                drive_uuid: Some([30; 16]),
                cartridge: Some(cartridge),
                action,
            }),
        );
        prev = Some(reference(&r));
    }
    body(
        &mut l,
        Body::CleaningRecorded(CleaningRecorded {
            run_id: "cartridge".into(),
            previous: None,
            drive_uuid: None,
            cartridge: Some(cartridge),
            action: CleaningAction::Expire(Reason {
                reason: "used".into(),
            }),
        }),
    );
    // Ruling 16: rejection is durable before the run's terminal record.
    let rejected = cleaning_cartridge_ref(&"CLNREJECT".into());
    body(
        &mut l,
        Body::CleaningRecorded(CleaningRecorded {
            run_id: "reject-cartridge".into(),
            previous: None,
            drive_uuid: None,
            cartridge: Some(rejected),
            action: CleaningAction::Register(Register {
                barcode: "CLNREJECT".into(),
                home_slot: 2,
            }),
        }),
    );
    let begun = body(
        &mut l,
        Body::CleaningRecorded(CleaningRecorded {
            run_id: "reject-run".into(),
            previous: None,
            drive_uuid: Some([30; 16]),
            cartridge: Some(rejected),
            action: CleaningAction::Begin(Begin {
                trigger: CleanTrigger::Manual,
            }),
        }),
    );
    body(
        &mut l,
        Body::CleaningRecorded(CleaningRecorded {
            run_id: "reject-cartridge".into(),
            previous: None,
            drive_uuid: None,
            cartridge: Some(rejected),
            action: CleaningAction::Reject(Reason {
                reason: "corroboration".into(),
            }),
        }),
    );
    body(
        &mut l,
        Body::CleaningRecorded(CleaningRecorded {
            run_id: "reject-run".into(),
            previous: Some(reference(&begun)),
            drive_uuid: Some([30; 16]),
            cartridge: Some(rejected),
            action: CleaningAction::Finish(Finish {
                result: CleanResult::Failed,
                verified_credit: false,
            }),
        }),
    );
    for (g, action) in [
        AlarmAction::Raise(AlarmRaise {
            kind: AlarmKind::SnapshotPersistFailing,
            severity: Severity::Warning,
            reason: "fault".into(),
        }),
        AlarmAction::Acknowledge(Unit),
        AlarmAction::Clear(Reason {
            reason: "cleared".into(),
        }),
    ]
    .into_iter()
    .enumerate()
    {
        body(
            &mut l,
            Body::AlarmChanged(AlarmChanged {
                condition_key: "drive-fault".into(),
                change: GenerationChange {
                    expected: g as u64,
                    next: g as u64 + 1,
                },
                action,
                cause: None,
            }),
        );
    }
    let mut prev = None;
    for event in [
        OperationEvent::Accepted(Accepted {
            actor_fingerprint: "operator".into(),
            idempotency_key: Some([40; 16]),
            request_fingerprint: [40; 32],
            subject: Some("drive".into()),
        }),
        OperationEvent::Started(Unit),
        OperationEvent::CancelRequested(Unit),
        OperationEvent::CancellationRejected(Unit),
        OperationEvent::CompletedAfterCancel(Response {
            response_fingerprint: Some([40; 32]),
        }),
    ] {
        let r = body(
            &mut l,
            Body::OperationRecorded(OperationRecorded {
                operation: [40; 16],
                kind: OperationKind::CleanDrive,
                previous: prev,
                event,
            }),
        );
        prev = Some(reference(&r));
    }
    for (id, event) in [
        OperationEvent::CancelledBeforeDispatch(Unit),
        OperationEvent::Finished(Response {
            response_fingerprint: None,
        }),
        OperationEvent::Failed(Failed {
            retry: Some(Retry::Transient),
            refusal_code: Some(1),
            summary: "verbatim".into(),
        }),
        OperationEvent::CompletionUnknown(Unit),
    ]
    .into_iter()
    .enumerate()
    {
        let op = [41 + id as u8; 16];
        let r = body(
            &mut l,
            Body::OperationRecorded(OperationRecorded {
                operation: op,
                kind: OperationKind::TapeVerification,
                previous: None,
                event: OperationEvent::Started(Unit),
            }),
        );
        body(
            &mut l,
            Body::OperationRecorded(OperationRecorded {
                operation: op,
                kind: OperationKind::TapeVerification,
                previous: Some(reference(&r)),
                event,
            }),
        );
    }
    for (i, event) in [
        SessionEvent::Closed,
        SessionEvent::Orphaned,
        SessionEvent::LostByRestart,
    ]
    .into_iter()
    .enumerate()
    {
        let session_id = [50 + i as u8; 16];
        let kind = if i == 0 {
            SessionKind::Read
        } else {
            SessionKind::Operation
        };
        let r = body(
            &mut l,
            Body::SessionObserved(SessionObserved {
                session_id,
                kind,
                endpoint: None,
                tape: Some(tape(1)),
                event: SessionEvent::Opened,
                previous: None,
                reason: None,
            }),
        );
        body(
            &mut l,
            Body::SessionObserved(SessionObserved {
                session_id,
                kind,
                endpoint: None,
                tape: Some(tape(1)),
                event,
                previous: Some(reference(&r)),
                reason: Some("ended".into()),
            }),
        );
    }
    let descriptors = vec![1, 2, 3];
    body(
        &mut l,
        Body::WrapMapCached(WrapMapCached {
            tape: tape(1),
            calibration_generation: 1,
            action: WrapAction::Cache(Cache {
                digest: Sha256::digest(&descriptors).into(),
                descriptors: Blob(descriptors),
                mapped_extent_lba: 3,
                write_epoch: 1,
            }),
        }),
    );
    body(
        &mut l,
        Body::WrapMapCached(WrapMapCached {
            tape: tape(1),
            calibration_generation: 2,
            action: WrapAction::Invalidate(Reason {
                reason: "epoch changed".into(),
            }),
        }),
    );
    let r = body(
        &mut l,
        Body::RecoveryRecorded(RecoveryRecorded {
            at: at(1, 1),
            operation: [60; 16],
            prior: None,
            action: RecoveryAction::RepairApplied(RepairApplied {
                archived_prefix_digest: [60; 32],
                retained_through: 1,
            }),
        }),
    );
    body(
        &mut l,
        Body::RecoveryRecorded(RecoveryRecorded {
            at: at(1, 2),
            operation: [60; 16],
            prior: Some(reference(&r)),
            action: RecoveryAction::AppendAuthorityRecovered(Recovered {
                source_digest: [61; 32],
            }),
        }),
    );
    body(
        &mut l,
        Body::Diagnostic(Diagnostic {
            event: DiagnosticEvent::HardwareWarning,
            subject_kind: "drive".into(),
            subject_id: Some("30".into()),
            detail: Bounded(vec![
                DiagnosticEntry("a".into(), DiagnosticValue::Text("text".into())),
                DiagnosticEntry("b".into(), DiagnosticValue::Unsigned(1)),
                DiagnosticEntry("c".into(), DiagnosticValue::Negative(Negative(-1))),
                DiagnosticEntry("d".into(), DiagnosticValue::Bytes(Blob(vec![0, 1]))),
                DiagnosticEntry("e".into(), DiagnosticValue::Bool(true)),
                DiagnosticEntry("f".into(), DiagnosticValue::Null(Unit)),
            ]),
        }),
    );
    optional_forms(&mut l, reference(&config));
    for (n, detail) in [(2, None), (3, Some("  Operator's text verbatim.  ".into()))] {
        body(
            &mut l,
            Body::Retire(Retire {
                tape: tape(n),
                reason: RetireReason::OperatorRequested,
                observed_tape_sequence: (n == 2).then_some(8),
                assignment_generation: if n == 2 { 2 } else { 1 },
                detail,
            }),
        );
    }
    let mut p = provision(4, Some("NEW"));
    if let Body::Reprovision(v) = &mut p {
        v.predecessor = Some(tape(2));
    }
    body(&mut l, p);
    let mut p = provision(5, None);
    if let Body::Reprovision(v) = &mut p {
        v.tape.uuid[6] = 0x45;
        v.tape.uuid[8] = 0x85;
        v.mode = ProvisionMode::Adopt;
        v.pool = Some("pool-a".into());
        v.endpoint = Some(standalone);
        v.adoption = Some(Adoption {
            request_fingerprint: [65; 32],
            operation: [65; 16],
            idempotency_key: [65; 16],
            physical_tail: PhysicalTail::ExactBootstrapFilemarkEod,
            state: AdoptedState::Ready,
            library_revision: None,
            home_slot: None,
            drive_serial: "STANDALONE1".into(),
            identity_only: true,
            bootstrap_drive_compression: false,
            configured_drive_compression: false,
        });
    }
    body(&mut l, p);
}
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.path().extension().is_some_and(|x| x == "remlib") {
            fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}
fn segments(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|s| s == "remlib"))
        .collect();
    v.sort();
    v
}
/// A Label-sourced incarnation accepts two consecutive generation corrections.
fn label_generation_corrections(dir: &Path) {
    let (mut l, _) = open(dir, &Present);
    let mut initial = provision(1, None);
    if let Body::Reprovision(v) = &mut initial {
        v.medium.source = GenerationSource::Label;
    }
    body(&mut l, initial);
    for (revision, generation) in [(1, LtoGen::Lto8), (2, LtoGen::Lto7)] {
        body(
            &mut l,
            medium_change(
                MediumSpec {
                    family: MediumFamily::Lto(generation),
                    source: GenerationSource::Label,
                    ..medium_spec()
                },
                revision,
            ),
        );
    }
}
fn generate(root: &Path) {
    all_records(&root.join("all-records"));
    label_generation_corrections(&root.join("medium-label-generation-correction"));
    review_replay_vectors(root);
    supersede_after_terminal_vector(&root.join("supersede-after-terminal"));
    pending_case(&root.join("pending-cross-log"));
    let rotation = root.join("rotation-clock-regression");
    let (mut l, c) = open(&rotation, &Present);
    for day in [0, 1, 0, 2] {
        c.0.store(NOW + day * 86_400_000_000_000, Ordering::SeqCst);
        body(&mut l, diagnostic());
    }
    drop(l);
    let healthy = root.join("healthy");
    let (mut l, _) = open(&healthy, &Present);
    body(&mut l, provision(1, None));
    body(&mut l, diagnostic());
    drop(l);
    for (name, zeros) in [("torn-tail", false), ("zero-filled-tail", true)] {
        let dst = root.join(name);
        copy_dir(&healthy, &dst);
        let p = segments(&dst).remove(0);
        if zeros {
            fs::OpenOptions::new()
                .append(true)
                .open(p)
                .unwrap()
                .write_all(&[0; 4096])
                .unwrap();
        } else {
            let mut b = fs::read(&p).unwrap();
            b.pop();
            fs::write(p, b).unwrap();
        }
    }
    let base = fs::read(segments(&healthy).remove(0)).unwrap();
    let first_len = 44 + u32::from_le_bytes(base[88..92].try_into().unwrap()) as usize + 32;
    let pos = 84 + first_len;
    let structural_len = u32::from_le_bytes(base[pos + 4..pos + 8].try_into().unwrap()) as usize;
    for (name, at) in [
        ("bad-header-crc", 76),
        ("bad-prefix-crc", pos + 20),
        ("bad-structural-crc", pos + 28 + structural_len),
        ("bad-payload-crc", base.len() - 1),
    ] {
        let dir = root.join("rejected").join(name);
        copy_dir(&healthy, &dir);
        let mut b = base.clone();
        b[at] ^= 1;
        fs::write(segments(&dir).remove(0), b).unwrap();
    }
    for (name, version, tag, seq, payload, structural) in [
        (
            "record-hash",
            1,
            32,
            2,
            vec![9; 32],
            base[pos + 28..pos + 28 + structural_len].to_vec(),
        ),
        (
            "payload-length",
            1,
            32,
            2,
            vec![],
            base[pos + 28..pos + 28 + structural_len].to_vec(),
        ),
        (
            "unknown-version",
            2,
            32,
            2,
            vec![9; 32],
            base[pos + 28..pos + 28 + structural_len].to_vec(),
        ),
        (
            "unknown-tag-30",
            1,
            30,
            2,
            vec![9; 32],
            base[pos + 28..pos + 28 + structural_len].to_vec(),
        ),
        (
            "sequence-gap",
            1,
            32,
            3,
            vec![9; 32],
            base[pos + 28..pos + 28 + structural_len].to_vec(),
        ),
        (
            "repeated-sequence",
            1,
            32,
            1,
            vec![9; 32],
            base[pos + 28..pos + 28 + structural_len].to_vec(),
        ),
        ("nonminimal-integer", 1, 32, 2, vec![9; 32], {
            let mut s = base[pos + 28..pos + 28 + structural_len].to_vec();
            s.splice(1..2, [0x18, 0]);
            s
        }),
        ("out-of-order-key", 1, 32, 2, vec![9; 32], {
            let mut s = base[pos + 28..pos + 28 + structural_len].to_vec();
            s[1] = 1;
            s
        }),
        ("unknown-key", 1, 32, 2, vec![9; 32], {
            let mut s = base[pos + 28..pos + 28 + structural_len].to_vec();
            s[1] = 2;
            s
        }),
        ("indefinite-map", 1, 32, 2, vec![9; 32], {
            let mut s = base[pos + 28..pos + 28 + structural_len].to_vec();
            s[0] = 0xbf;
            s
        }),
    ] {
        let dir = root.join("rejected").join(name);
        copy_dir(&healthy, &dir);
        let mut b = base[..pos].to_vec();
        crate::log_frame::encode_frame(&mut b, version, tag, seq, &structural, &payload).unwrap();
        fs::write(segments(&dir).remove(0), b).unwrap();
    }
    for name in ["wrong-log-uuid", "filename-mismatch"] {
        let dir = root.join("rejected").join(name);
        copy_dir(&healthy, &dir);
        let p = segments(&dir).remove(0);
        let mut b = fs::read(&p).unwrap();
        if name == "wrong-log-uuid" {
            b[10..26].fill(0xb2);
            let crc = remanence_crc::crc64_xz(&b[..76]);
            b[76..84].copy_from_slice(&crc.to_le_bytes());
            fs::write(p, b).unwrap();
        } else {
            fs::rename(p, dir.join("00000000000000000001-2026-11-04.remlib")).unwrap();
        }
    }
    for (name, expected, bad) in semantic_cases() {
        let dir = root.join("rejected").join(name);
        let (mut l, _) = open(&dir, &Present);
        body(&mut l, provision(1, Some("A")));
        body(&mut l, provision(2, Some("B")));
        let r = record(bad.clone(), 3);
        let actual = l
            .append(bad, AuditActor::System, SourceLayer::Layer4, &Present)
            .unwrap_err();
        assert!(
            matches!(actual, StoreError::Fold(ref e) if *e == expected),
            "{name}: {actual:?}, expected {expected:?}"
        );
        let hash = l.state().terminal_hash;
        drop(l);
        let (encoded, _) = codec::encode(3, hash, &r).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(segments(&dir).remove(0))
            .unwrap()
            .write_all(&encoded)
            .unwrap();
        let replay = replay(&dir, LOG, &Present).unwrap();
        assert_eq!(
            replay.refusal.unwrap().fault,
            RefusalKind::Fold(expected),
            "{name}"
        );
    }
    freeze_vectors(root);
    contradiction_vectors(root);
    extra_rejections(root);
    l1w_wire_rejections(root);
    historical_vectors(root);
    pending_limit_case(&root.join("rejected/pending-record-limit"));
    fs::remove_dir_all(healthy).unwrap();
}
pub(super) fn semantic_cases() -> Vec<(&'static str, FoldError, Body)> {
    vec![
        (
            "stale-assignment",
            FoldError::GenerationMismatch,
            pool(1, 0),
        ),
        (
            "label-held-twice",
            FoldError::LabelHeld,
            Body::Label(Label {
                tape: tape(2),
                label: Some("A".into()),
                provenance: LabelProvenance::Operator,
                change: GenerationChange {
                    expected: 1,
                    next: 2,
                },
                prior_holder: None,
                observed_bot_uuid: None,
            }),
        ),
        (
            "write-fence-drive-scope",
            FoldError::FenceScopeMismatch,
            Body::FenceRaised(FenceRaised {
                detail: Bounded(vec![]),
                kind: FenceKind::Write,
                scope: FenceScope::Drive(endpoint("LIBA0001")),
                cause: FenceCause::TransferError,
                evidence: None,
                at: None,
                operation: None,
            }),
        ),
        (
            "release-wrong-counterpart",
            FoldError::WrongCounterpart,
            Body::FenceReleased(FenceReleased {
                detail: Bounded(vec![]),
                fence: 1,
                evidence: "proof".into(),
                forced: false,
            }),
        ),
        (
            "outcome-without-intent",
            FoldError::OutcomeWithoutOpenIntent,
            Body::MotionOutcome(MotionOutcome {
                intent: DecisionRef {
                    sequence: 1,
                    record_uuid: [0; 16],
                },
                result: MotionResult::Unknown,
                evidence: evidence(),
                confirmation: None,
            }),
        ),
        ("load-attempt-gap", FoldError::LoadAttempt, intent(3)),
        ("impossible-cursor", FoldError::ImpossibleCursor, {
            let mut b = progress(1);
            if let Body::VerificationProgress(v) = &mut b {
                v.next.part = 2;
            }
            b
        }),
        (
            "reserved-empty-range",
            FoldError::InvalidSchema,
            Body::AcceptUnprotectedFinalization(AcceptUnprotectedFinalization {
                at: at(1, 1),
                operation: [3; 16],
                w: 1,
                t: 1,
                reason: "empty".into(),
            }),
        ),
        (
            "stale-manual-acceptance",
            FoldError::GenerationMismatch,
            Body::ManualFinalizationAccepted(ManualFinalizationAccepted {
                at: at(1, 1),
                operation: [3; 16],
                actor_fingerprint: "operator".into(),
                idempotency_key: [3; 16],
                request_fingerprint: [3; 32],
                assignment_generation: 0,
            }),
        ),
        (
            "forward-decision",
            FoldError::ForwardReference,
            Body::AssignPool(AssignPool {
                tape: tape(1),
                pool: None,
                change: GenerationChange {
                    expected: 1,
                    next: 2,
                },
                source: AssignmentSource::Rule,
                configuration: Some(DecisionRef {
                    sequence: 4,
                    record_uuid: [4; 16],
                }),
            }),
        ),
    ]
}
fn collect(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(root: &Path, at: &Path, m: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for e in fs::read_dir(at).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                walk(root, &e.path(), m);
            } else if e.path().extension().is_some_and(|x| x == "remlib") {
                m.insert(
                    e.path().strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(e.path()).unwrap(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}
fn metadata(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let files = collect(root);
    let cases: std::collections::BTreeSet<_> = files
        .keys()
        .map(|p| p.parent().unwrap().to_path_buf())
        .collect();
    let mut manifest=String::from("case\tsegments\tbytes\tsha256\tlog_uuid\tlast_sequence\tterminal_hash\ttail_or_error\tapplied_through\tpending\ttapes\topen_fences\tduplicates\tcontradictions\tpending_sequences\topen_fence_ids\tcontradicted_tapes\texcluded_diagnostics\n");
    let mut frames = String::from("case\tsequence\ttag\tsegment\toffset\tend\trecord_hash\n");
    let mut readme=String::from("# Library log v1 corpus\n\nGenerated through the real append store. Run `LIBRARY_LOG_FIXTURE_EXPORT_DIR=/empty/path cargo test --offline -p remanence-state --lib reference_library_logs_regenerate_byte_exactly`. Validated EOF-short or zero-filled final frames are absent; SCHEMA.md defines the checks that can refuse them, including an over-cap short prefix and EmptySegment for a torn first frame. Byte faults and library contradictions stop replay; native contradictions are scoped.\n\n");
    let mut views = String::from("case\ttape_uuid\tsequence\tanswer\tkind\tdigest\n");
    let mut view_prose = String::from("## Tape views\n\nNative tape views are replay inputs, not part of the library log. VIEWS.tsv has exactly one default row per MANIFEST case (`*`, `*`). `Present` with digest `echo` returns the reference's own digest and kind `FinalizationPrepared`; `NotYet` has empty kind and digest. Explicit rows override that default by tape UUID and sequence; their Present digest is literal hex. Contradiction and NotYet have empty kind and digest.\n\n");
    readme += "FRAMES.tsv inventories complete decodable frames, including non-authoritative frames in rejected cases; authoritative records have sequence at most MANIFEST.last_sequence. MANIFEST.log_uuid is the first segment's on-disk header value (wrong-log-uuid intentionally differs from the expected identity). The appended identity columns describe the authoritative prefix: pending_sequences and open_fence_ids are ascending comma-separated sequences; contradicted_tapes is ascending UUID hex:FoldError name. Counts retain their existing meanings. excluded_diagnostics is a comma-separated UUID hex:count list; these replay diagnostics are not fold authority. The deferred-fence-unknown-at-resolution case publishes the queued prefix; its injected liveness loss is specified in FOLD-VECTORS.json.\n\n";
    for case in cases {
        let dir = root.join(&case);
        let published_view = case_view(&case);
        views += &view_rows(&case, &published_view);
        view_prose += &format!(
            "### {}\n\nDefault: {}.\n\n",
            case.display(),
            if published_view.present_default {
                "Present(FinalizationPrepared, reference digest)"
            } else {
                "NotYet"
            }
        );
        for ((tape, sequence), answer) in &published_view.entries {
            let description = match answer {
                TapeLookup::Present { kind, digest } => {
                    format!("Present({kind:?}, {})", hex(digest))
                }
                TapeLookup::EmptyPrefix { digest } => format!("EmptyPrefix({})", hex(digest)),
                TapeLookup::NotYet => "NotYet".to_owned(),
                TapeLookup::Contradiction => "Contradiction".to_owned(),
            };
            view_prose += &format!(
                "Tape `{}` at sequence {sequence}: {description}.\n\n",
                hex(tape)
            );
        }
        let r = replay(&dir, LOG, &published_view).unwrap();
        let paths = segments(&dir);
        let bytes: Vec<_> = paths.iter().map(|p| fs::read(p).unwrap()).collect();
        let hashes: Vec<_> = bytes.iter().map(|b| hex(&Sha256::digest(b))).collect();
        let names: Vec<_> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let sizes: Vec<_> = bytes.iter().map(|b| b.len().to_string()).collect();
        let status = r
            .refusal
            .as_ref()
            .map(|e| format!("{}@{}:{:?}:{}", e.segment, e.offset, e.sequence, e.fault))
            .unwrap_or_else(|| format!("{:?}", r.segments.last().map(|s| s.tail)));
        let identities = fold_identities(&dir, &r);
        manifest += &format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            case.display(),
            names.join(","),
            sizes.join(","),
            hashes.join(","),
            hex(&bytes[0][10..26]),
            r.last_sequence,
            hex(&r.terminal_hash),
            status,
            r.fold.applied_through,
            r.fold.pending.len(),
            r.fold.tapes.len(),
            r.fold.open_fences(),
            r.fold.duplicates,
            r.fold.contradicted_tapes(),
            identities.0,
            identities.1,
            identities.2,
            r.fold
                .contradictions
                .iter()
                .filter(|(_, s)| s.excluded != 0)
                .map(|(scope, s)| {
                    let fold::Scope::Tape(tape) = scope else {
                        unreachable!()
                    };
                    format!("{}:{}", hex(tape), s.excluded)
                })
                .collect::<Vec<_>>()
                .join(",")
        );
        readme+=&format!("## {}\n\nTail: `{status}`. Authoritative sequence {}; applied {}; pending {}; tapes {}; fences {}; duplicates {}.\n\n",case.display(),r.last_sequence,r.fold.applied_through,r.fold.pending.len(),r.fold.tapes.len(),r.fold.open_fences(),r.fold.duplicates);
        for ((name, b), hash) in names.iter().zip(bytes.iter()).zip(hashes) {
            readme += &format!("`{name}`: {} bytes, SHA-256 `{hash}`.\n\n", b.len());
            if b.len() >= 84 {
                readme += &format!("Raw header (unvalidated): magic `{}`, version {}, log UUID `{}`, first sequence {}, previous hash `{}`, date `{}`, stored CRC `{}`, computed CRC `{}`.\n\n",
                    hex(&b[..8]), u16::from_le_bytes(b[8..10].try_into().unwrap()), hex(&b[10..26]),
                    u64::from_le_bytes(b[26..34].try_into().unwrap()), hex(&b[34..66]),
                    String::from_utf8_lossy(&b[66..76]), hex(&b[76..84]), hex(&remanence_crc::crc64_xz(&b[..76]).to_le_bytes()));
            }
            let mut input = b.as_slice();
            if let Ok(h) = codec::Header::decode(&mut input, LOG) {
                readme += &format!("Header: `{h:?}`.\n\n");
                if let Some(tail) = r.segments.iter().find(|s| s.name == *name) {
                    readme += &format!(
                        "Authoritative prefix end: {} bytes; tail {:?}.\n\n",
                        tail.through, tail.tail
                    );
                }
                readme += "| Sequence | Tag | Segment | Offset | End | Record hash |\n|---:|---|---|---:|---:|---|\n";
                let mut d = codec::Decoder::at(input, h.first_sequence, 84);
                while let Ok(Some(f)) = d.next_frame() {
                    readme += &format!(
                        "| {} | {:?} | {} | {} | {} | `{}` |\n",
                        f.sequence,
                        f.record.body.tag(),
                        name,
                        f.offset,
                        f.end,
                        hex(f.payload)
                    );
                    frames += &format!(
                        "{}\t{}\t{:?}\t{}\t{}\t{}\t{}\n",
                        case.display(),
                        f.sequence,
                        f.record.body.tag(),
                        name,
                        f.offset,
                        f.end,
                        hex(f.payload)
                    );
                }
            }
        }
    }
    readme += view_prose.trim_end();
    readme += "\n";
    let mut schema=String::from("# Library log v1 schema\n\nHeader: magic REMLIB\\0\\x01 (0..8), version u16 LE (8..10), log UUID b16 (10..26), first sequence u64 LE (26..34), previous hash b32 (34..66), valid ASCII date (66..76), CRC-64/XZ (76..84).\n\nFrame: prefix version:u16/tag:u16/structural_len:u32/payload_len:u32/sequence:u64, all LE; prefix CRC (20..28); structural CBOR; structural CRC; 32-byte record hash; payload CRC. Overhead 44 bytes, combined cap 64 MiB. The payload is the 32-byte record hash, so a valid frame has payload_len 32; the order of that check is given below. `FrameTooLarge` (\"frame exceeds 64 MiB\") is raised when `structural_len + payload_len` exceeds 64 MiB, before reading the body. For a complete 28-byte prefix the check order is prefix CRC, version, tag, sequence, then the cap; a cap refusal includes the decoded sequence. An EOF-short prefix instead checks the visible identity, then the cap, then the visible sequence, with no CRC check and no reported sequence; the exact tail rules follow below.\n\nrecord_hash = SHA-256(previous_hash || prefix20 || structural). First previous hash is zero. No CRC or hash covers itself. Structural map is {0: envelope, 1: body}. Structural CBOR is validated before the record hash, so malformed structure with a stale hash is `NoncanonicalCbor`. All keys and fields below are mandatory, with null for absent Option. TextValue/Text max 4096 UTF-8 bytes; LabelText max 32; Blob<N> and Bounded<T,N> enforce N before allocation. Pure enums are unsigned; unions are [code,payload], including null unit payloads.\n\n");
    schema += "| Header offset | Bytes | Field |\n|---:|---:|---|\n| 0 | 8 | REMLIB magic |\n| 8 | 2 | Version 1 |\n| 10 | 16 | Log UUID |\n| 26 | 8 | First sequence |\n| 34 | 32 | Previous segment hash |\n| 66 | 10 | ASCII date |\n| 76 | 8 | CRC of bytes 0..76 |\n\n## Segments\n\nNames are `<first_sequence:020>-<YYYY-MM-DD>.remlib`, using the header date. Discovery selects every directory entry whose extension is exactly `remlib`, without a regular-file check; other extensions, including `.REMLIB` and `.remlib.bak`, are ignored. A selected name differing from its decoded header name is `SegmentName`, never silently skipped; header and I/O failures can precede this check.\n\nSegments are read in lexical name order, which is first-sequence order for well-formed names. Each first sequence is the previous last sequence plus one (`SegmentSequence`), starting at 1; its previous hash is the previous segment's last record hash, zero for the first (`PreviousSegmentHash`). After header validation, the per-segment checks run name, then sequence, then previous hash; the first refusal wins.\n\nDates are diagnostic for replay ordering. Rotation occurs when the UTC date advances beyond the active segment's date; after clock regression the store keeps appending to the active segment.\n\nRotation publishes the header and first frame together. If there is no prior refusal, a segment whose authoritative end is the 84-byte header is `EmptySegment` at offset 84 with sequence last plus one, checked before `InteriorTorn`. This includes a header followed only by a torn first frame: `EmptySegment` wins over the torn-tail classification.\n\nAt a frame boundary, zero bytes read means a complete tail. If all bytes read into the prefix (up to 28) are zero, scan the suffix: a later valid prefix at any byte offset (valid prefix CRC, version, tag and lengths within the cap; sequence need not match) gives a prefix CRC refusal; otherwise a later nonzero byte gives `NonZeroAfterZeros` at that byte; otherwise the tail is `ZeroFilled`. These refusals have no sequence.\n\nFor a nonzero EOF-short prefix (1..27 bytes), a zero first byte gives `NonZeroAfterZeros` at its first nonzero byte, before any identity check. Otherwise check the present version bytes against version 1, then the tag if any tag byte is present (zero-fill its missing high byte). Next zero-fill missing length bytes and check the resulting lower-bound `structural_len + payload_len` against 64 MiB: an excess is `FrameTooLarge`, even though the prefix is short. Only then compare any present sequence bytes with the expected sequence's LE bytes. Every refusal on this path has sequence None; no prefix CRC is checked. Only a short prefix that passes these identity, cap and sequence checks is an absent frame with a `ShortPrefix` torn tail.\n\nFor a nonzero complete prefix, a failed prefix CRC is refused with sequence None, except that a zero first byte and no later valid prefix at any byte offset gives `NonZeroAfterZeros` at the first nonzero prefix byte. After a valid prefix CRC, check version, tag, sequence and cap in that order; these refusals include the decoded sequence. Only then read the body. Check any fully present structural CRC, then the payload CRC if the body is complete. A complete body whose CRCs pass is then decoded as canonical structural CBOR; after that the payload length is checked (`PayloadLength` unless the payload is exactly 32 bytes), and only then the record hash. `PayloadLength` is never checked on a short or damaged body: such a body goes to the damaged-body classifier below, and its refusals carry the sequence decoded from the frame's valid prefix, `NonZeroAfterZeros` on this path included. For a short body or failed body CRC, scan the unread suffix after the body bytes read, then apply these checks in order: a later valid prefix at any byte offset is a CRC refusal (the failed section, or Structural when no CRC failed); a complete damaged body starting with zero but containing nonzero bytes is `NonZeroAfterZeros`; a qualifying trailing zero run is `ZeroFilled` unless a later nonzero byte makes it `NonZeroAfterZeros`; otherwise a failed CRC is refused; otherwise an EOF-short body is `ShortBody`. A trailing zero run qualifies exactly when it is nonempty and starts at or before S, or when it is nonempty, P > 0, more than S+8 body bytes were read, and it starts before S+8+P; positions are measured from the body start. A structural CRC failure takes precedence over a payload CRC failure. Valid complete bodies bypass this damaged-body classifier.\n\nIn the last nonempty segment, a frame classified `ShortBody`, `ShortPrefix` or `ZeroFilled` is absent and the tail is torn. The same torn tail in an earlier nonempty segment is `InteriorTorn`; the empty-segment rule above takes precedence. A short header is `Header`.\n\n| Frame offset | Bytes | Field |\n|---:|---:|---|\n| 0 | 2 | Version 1 |\n| 2 | 2 | Tag |\n| 4 | 4 | Structural length S |\n| 8 | 4 | Payload length 32 |\n| 12 | 8 | Sequence |\n| 20 | 8 | Prefix CRC |\n| 28 | S | Structural CBOR |\n| 28+S | 8 | Structural CRC |\n| 36+S | 32 | Record hash |\n| 68+S | 8 | Payload CRC |\n\nAuditActor is map {0: kind uint (0 System, 1 User, 2 Service), 1: name text4096|null}; name is null exactly for System. Numeric Rust scalars map to CBOR unsigned integers with their declared bit width; Id is b16, Hash b32, Text text4096, LabelText text32. Pool watermarks preserve f64 bits as unsigned u64. Nested named types are maps in the tables below.\n\n";
    for (name, fields) in wire::SCHEMA_FIELDS {
        schema += &format!(
            "## {name}\n\n| Key | Field | CBOR type / bound | Optional |\n|---:|---|---|---|\n"
        );
        for (i, (field, ty)) in fields.iter().enumerate() {
            schema += &format!("| {i} | {field} | `{ty}` | {} |\n", ty.contains("Option"));
        }
        if *name == "Failed" {
            schema += "\nRefusalCode (also RefusalRecorded.detail code): zero is never emitted. The fold does not yet check `Failed.refusal_code` against this table.\n\n| Code | Name |\n|---:|---|\n";
            for code in 1..=14 {
                schema += &format!("| {code} | {:?} |\n", RefusalCode::from_u32(code).unwrap());
            }
        }
        if *name == "TapeAt" {
            schema += "\n`TapeAt.record_digest` is SHA-256 over the referenced tape-log frame's 20 prefix bytes, structural bytes and payload bytes, without any CRC.\n";
        }
        schema += '\n'.to_string().as_str();
    }
    schema += "## Integer and union tables\n\n";
    for line in include_str!("wire.rs").lines() {
        if line.starts_with("enumeration!(") || line.starts_with("union!(") {
            let declaration = line.split_once('(').unwrap().1.trim_end_matches(");");
            let (name, variants) = declaration.split_once(';').unwrap();
            schema += &format!("### {name}\n\n| Code | Variant / payload |\n|---:|---|\n");
            for variant in variants.split(',') {
                let (code, name) = variant.trim().split_once("=>").unwrap();
                schema += &format!("| {code} | `{name}` |\n");
            }
            schema += "\n";
        }
    }
    schema += "### MediumFamily\n\n| Code | Variant / payload |\n|---:|---|\n| 0 | Lto(LtoGen) |\n| 1 | Disk(null) |\n\n### FenceScope\n\n| Code | Variant / payload |\n|---:|---|\n| 0 | Tape(TapeRef) |\n| 1 | Drive(DriveKey) |\n| 2 | Model(ModelScope), reserved |\n\nMediumSpec stores capabilities, never role. rerecord implies regenerative; lifecycle_ops gates lifecycle operations independently of family. Source Measured on a revision attests a fresh measurement, not inherited provenance. Zero capacity is permitted (no usable space).\n\n";
    schema += "## Replay and pending\n\nTags 6, 7, 13, 15 and 26, and tag 14 only when at is Some, depend on TapeAt; tag 15 requires native FinalizationPrepared. Present has both kind and digest. NotYet queues all touched scopes in source order, except FenceRaised always applies. Replay receives every frame once, in sequence order, and never retries a pending record. Pending records are retried (`resolve`, to a fixed point) only before a live append, under the view current at that append. Except for deferred-fence liveness failures described in tag 8, if retrying a pending record returns a library FoldError (rather than remaining Unresolved or becoming a scoped tape contradiction), resolution stops at that error, retains the failing record and the unvisited remainder as pending, and keeps earlier progress from that resolution. The live append propagates that error before constructing or writing its new frame. Later appends retry the same pending record and fail again while that error persists. Byte faults and library FoldError stop at the previous authoritative prefix. TapeContradiction isolates its tape and continues. Contradiction status alone never refuses, excludes or holds a record in the fold. After shape, identity, repair-floor, pending-scope and capacity admission succeed, live validation judges a dependent record on its own reference: a match proceeds to library rules, a mismatch is refused TapeContradiction without appending an exclusion, and NotYet is refused Unresolved. Refusing write, verify and finalize on a contradicted tape is D3 admission policy for B4b-2/B5, not a fold rule. During replay and resolution each dependent record is judged on its own reference, after shape and tape identity checks and before coverage, chains or uniqueness; on receipt, the RepairBelowReference floor check also precedes the own-reference check. A below-floor repair is refused before that reference is consulted: it is not received, is not excluded, and changes no summary. A replay or resolution mismatch reached after those admission checks excludes the record without applying it or retaining any scope, updates the tape's minimum contradicting sequence, and increments its excluded diagnostic count. A matching reference proceeds to library rules when its scopes are free; NotYet and earlier pending scopes still hold records. Resolution checks a scope-held dependent record's reference just as receive does. Per-tape summaries retain the minimum contradiction and highest received dependent sequence; map membership alone does not mean contradicted. FenceRaised is the immediate-effect exception described in tag 8. applied_through never crosses pending. Live/replay equivalence under growing immutable tape views requires dependent references to be present and matching at each append; FenceRaised.at is evidence and may be NotYet at its append. Excluded counters and unresolved_fence_evidence are diagnostics excluded from the equivalence comparison: unresolved fence evidence may differ between live and replay without changing fence enforcement or tape authority.\n";
    schema += "\nLibraryHistory first uses retained state; otherwise the store binary-searches existing segment headers and scans one segment. DecisionRef binds sequence and UUID; only FenceId wire references may use sequence-only lookup in the header-bound log. Replay sees only lower sequences. FOLD-VECTORS.json freezes ReleaseWithoutFence with an explicitly empty injected history: a contiguous store log cannot lack an earlier sequence, and a diagnostic at that sequence is WrongCounterpart. I/O faults are HistoryUnavailable. Memory is O(resources), plus lifetime idempotency key/value payload at 80 bytes per keyed operation (SHA-256 of length-prefixed actor_fingerprint, u16 kind, and idempotency_key maps to operation id and request_fingerprint; allocator/map overhead is measured separately), plus one entry per crash supersede or unprotected acceptance (tags 6 and 7, retained for lifetime lookup and uniqueness), plus pending up to 4096 records and 64 MiB of canonical CBOR envelopes/bodies, plus one looked-up frame. Tags 13, 14 and 26 require an OperationRecorded chain ending in a terminal record to be pruned; writers that omit that chain retain those slots indefinitely. The pending byte limit counts the canonical encoding of the whole structural map `{0: envelope, 1: body}`. Pending bytes use exact-length boxed storage. Exceeding either total refuses with PendingLimitExceeded { scope, records, bytes }, preserving every prior pending record. Append to a blocked scope at capacity returns the same typed error.\n";
    schema += "\nFRAMES.tsv lists complete decodable frames, including frames beyond the authoritative prefix and the decoded refused frame of a rejected case; authoritative records have sequence at most MANIFEST.last_sequence. Frames whose header or encoding cannot be decoded are not listed. MANIFEST.log_uuid is the first segment's on-disk header value; in wrong-log-uuid it differs from the expected identity. Tape views are external replay inputs published in VIEWS.tsv and README.md.\n";
    schema += FOLD_SCHEMA_NOTES;
    let r = record(diagnostic(), 1);
    let (worked, hash) = codec::encode(1, [0; 32], &r).unwrap();
    schema += &format!(
        "\n## Worked Diagnostic at sequence 1\n\nRecord hash `{}`.\n\n```text\n{}\n```\n",
        hex(&hash),
        hex(&worked)
    );
    schema += "\n| Offset | Hex byte | Section |\n|---:|---|---|\n";
    let structural = u32::from_le_bytes(worked[4..8].try_into().unwrap()) as usize;
    for (i, b) in worked.iter().enumerate() {
        let section = match i {
            0..=1 => "version",
            2..=3 => "tag",
            4..=7 => "structural length",
            8..=11 => "payload length",
            12..=19 => "sequence",
            20..=27 => "prefix CRC",
            _ if i < 28 + structural => "canonical structural CBOR",
            _ if i < 36 + structural => "structural CRC",
            _ if i < 68 + structural => "record hash",
            _ => "payload CRC",
        };
        schema += &format!("| {i} | {b:02x} | {section} |\n");
    }
    BTreeMap::from([
        (PathBuf::from("FOLD-VECTORS.json"), absent_fence_vector()),
        (PathBuf::from("MANIFEST.tsv"), manifest.into_bytes()),
        (PathBuf::from("FRAMES.tsv"), frames.into_bytes()),
        (PathBuf::from("VIEWS.tsv"), views.into_bytes()),
        (PathBuf::from("README.md"), readme.into_bytes()),
        (PathBuf::from("SCHEMA.md"), schema.into_bytes()),
    ])
}
/// Regeneration compares every binary and metadata byte; export requires an empty directory.
#[test]
fn reference_library_logs_regenerate_byte_exactly() {
    let d = tempfile::tempdir().unwrap();
    generate(d.path());
    let mut files = collect(d.path());
    files.extend(metadata(d.path()));
    if let Some(path) = std::env::var_os("LIBRARY_LOG_FIXTURE_EXPORT_DIR") {
        let path = PathBuf::from(path);
        fs::create_dir_all(&path).unwrap();
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        for (p, b) in &files {
            let p = path.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b).unwrap();
        }
    } else {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/library-log");
        assert_eq!(collect(&root), collect(d.path()));
        for (p, b) in files {
            assert_eq!(fs::read(root.join(&p)).unwrap(), b, "{}", p.display());
        }
    }
}
/// All manifest fields and frame rows are computed independently by the production reader.
#[test]
fn reference_library_logs_production_reader_matches_manifest() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/library-log");
    assert!(root.exists());
    for (p, b) in metadata(&root) {
        assert_eq!(fs::read(root.join(&p)).unwrap(), b, "{}", p.display());
    }
}
/// Every all-records frame re-encodes byte for byte, including both retirement detail forms.
pub(super) fn every_tag_round_trips_canonically() {
    let d = tempfile::tempdir().unwrap();
    all_records(d.path());
    let mut tags = std::collections::BTreeSet::new();
    let mut details = std::collections::BTreeSet::new();
    for p in segments(d.path()) {
        let b = fs::read(p).unwrap();
        let mut r = b.as_slice();
        let h = codec::Header::decode(&mut r, LOG).unwrap();
        let mut dec = codec::Decoder::at(r, h.first_sequence, 84);
        let mut previous = h.previous_segment_hash;
        while let Some(f) = dec.next_frame().unwrap() {
            let (bytes, hash) = codec::encode(f.sequence, previous, &f.record).unwrap();
            assert_eq!(bytes, &b[f.offset as usize..f.end as usize]);
            previous = hash;
            tags.insert(f.record.body.tag());
            if let Body::Retire(r) = &f.record.body {
                details.insert(r.detail.is_some());
            }
        }
    }
    assert_eq!(tags.len(), 27);
    assert_eq!(details.len(), 2);
}
/// All semantic rejected vectors use the same refusal on append and replay.
pub(super) fn append_and_replay_share_validate() {
    let d = tempfile::tempdir().unwrap();
    generate(d.path());
    assert!(semantic_cases().len() >= 10);
}

/// Build typed native proof exclusively from decoded production tape frames.
pub(super) fn real_view() -> View {
    let bytes = include_bytes!("../../../../fixtures/tape-log/session-tail-synchronized.remtape");
    let mut r = bytes.as_slice();
    crate::tape_log::store::Header::decode(&mut r, [0xa1; 16]).unwrap();
    let offset = (bytes.len() - r.len()) as u64;
    let mut decoder = crate::tape_log::codec::Decoder::at(r, 1, offset);
    let mut view = View::default();
    while let Some(f) = decoder.next_frame().unwrap() {
        view.0.insert(
            ([0xa1; 16], f.sequence),
            TapeLookup::Present {
                kind: f.record.tag(),
                digest: crate::tape_log::codec::record_digest(&f),
            },
        );
    }
    assert_eq!(view.0.len(), 8);
    view
}
fn pending_case(dir: &Path) {
    let native = real_view();
    let (mut l, _) = open(dir, &Present);
    body(&mut l, provision(0xa1, None));
    body(&mut l, provision(2, None));
    for n in 1..=8 {
        let TapeLookup::Present { digest, .. } = native.0[&([0xa1; 16], n)] else {
            unreachable!()
        };
        body(
            &mut l,
            Body::RecoveryRecorded(RecoveryRecorded {
                at: TapeAt {
                    tape: tape(0xa1),
                    sequence: n,
                    record_digest: digest,
                },
                action: RecoveryAction::AppendAuthorityRecovered(Recovered {
                    source_digest: [n as u8; 32],
                }),
                operation: [n as u8; 16],
                prior: None,
            }),
        );
    }
    let mut v = verdict();
    if let Body::VerificationVerdict(v) = &mut v {
        v.operation = [99; 16];
        v.at = Some(at(0xa1, 9));
        v.tape = tape(0xa1);
    }
    body(&mut l, v);
    body(&mut l, pool(0xa1, 1));
    body(&mut l, fence(0xa1));
    body(&mut l, pool(2, 1));
    // The writer saw a different native digest; replay against the paired tape must isolate it.
    body(
        &mut l,
        Body::SupersedeTail(SupersedeTail {
            at: at(0xa1, 5),
            operation: Some([100; 16]),
            session_id: Some([100; 16]),
            preview_digest: Some([100; 32]),
            tail_file_count: 1,
        }),
    );
}
/// The real decoded tape fixture resolves exactly eight references and isolates disagreement.
pub(super) fn tape_reference_ahead_of_tape_log_waits() {
    let d = tempfile::tempdir().unwrap();
    pending_case(d.path());
    let r = replay(d.path(), LOG, &real_view()).unwrap();
    assert!(r.refusal.is_none());
    assert_eq!(r.fold.pending.len(), 3);
    assert_eq!(r.fold.contradicted_tapes(), 1);
    assert_eq!(r.fold.open_fences(), 1);
    assert_eq!(r.fold.tapes[&tape(2).uuid].assignment_generation, 2);
}
fn negative_histories() -> Vec<(&'static str, Vec<Body>, Body, FoldError)> {
    let dr = |n| DecisionRef {
        sequence: n,
        record_uuid: Ids.next(n),
    };
    let release = |n| {
        Body::FenceReleased(FenceReleased {
            detail: Bounded(vec![]),
            fence: n,
            evidence: "proof".into(),
            forced: false,
        })
    };
    let drive = Body::DriveObserved(DriveObserved {
        drive_uuid: [30; 16],
        serial: None,
        identity_source: IdentitySource::Standalone,
        endpoint: None,
        managed: Managed::Foreign,
        inventory_generation: 1,
        vendor: None,
        product: None,
        firmware_rev: None,
    });
    let alarm = Body::AlarmChanged(AlarmChanged {
        condition_key: "alarm".into(),
        change: GenerationChange {
            expected: 0,
            next: 1,
        },
        action: AlarmAction::Raise(AlarmRaise {
            kind: AlarmKind::SnapshotPersistFailing,
            severity: Severity::Warning,
            reason: "raised".into(),
        }),
        cause: None,
    });
    let mut coverage = progress(2);
    if let Body::VerificationProgress(v) = &mut coverage {
        v.modes = 2;
    }
    let mut cursor = progress(2);
    if let Body::VerificationProgress(v) = &mut cursor {
        v.next.file = 0;
    }
    let mut unseen = pool(9, 1);
    if let Body::AssignPool(v) = &mut unseen {
        v.tape.incarnation = 2;
    }
    let mut wrong_inc = pool(1, 1);
    if let Body::AssignPool(v) = &mut wrong_inc {
        v.tape.incarnation = 2;
    }
    let retire = Body::Retire(Retire {
        tape: tape(1),
        reason: RetireReason::OperatorRequested,
        observed_tape_sequence: None,
        assignment_generation: 1,
        detail: None,
    });
    let op = Body::OperationRecorded(OperationRecorded {
        operation: [30; 16],
        kind: OperationKind::TapeVerification,
        previous: None,
        event: OperationEvent::Started(Unit),
    });
    let accepted = Body::OperationRecorded(OperationRecorded {
        operation: [31; 16],
        kind: OperationKind::TapeVerification,
        previous: None,
        event: OperationEvent::Accepted(Accepted {
            actor_fingerprint: "operator".into(),
            idempotency_key: Some([31; 16]),
            request_fingerprint: [31; 32],
            subject: None,
        }),
    });
    let done = Body::MotionOutcome(MotionOutcome {
        intent: dr(3),
        result: MotionResult::Done,
        evidence: evidence(),
        confirmation: Some(Confirmation::Readiness(1)),
    });
    let second = Body::MotionOutcome(MotionOutcome {
        intent: dr(3),
        result: MotionResult::Failed,
        evidence: evidence(),
        confirmation: None,
    });
    let mut rule_wrong = pool(1, 1);
    if let Body::AssignPool(v) = &mut rule_wrong {
        v.source = AssignmentSource::Rule;
        v.configuration = Some(dr(4));
    }
    vec![
        (
            "reserved-container-quarantine",
            vec![],
            reserved_fence(false),
            FoldError::ReservedNotYetUsed,
        ),
        (
            "reserved-model-scope",
            vec![],
            reserved_fence(true),
            FoldError::ReservedNotYetUsed,
        ),
        (
            "medium-family-change",
            vec![],
            medium_change(
                MediumSpec {
                    family: MediumFamily::Disk,
                    ..medium_spec()
                },
                1,
            ),
            FoldError::MediumFamilyChanged,
        ),
        (
            "medium-measured-generation-change",
            vec![],
            medium_change(
                MediumSpec {
                    family: MediumFamily::Lto(LtoGen::Lto8),
                    ..medium_spec()
                },
                1,
            ),
            FoldError::MediumFamilyChanged,
        ),
        (
            "medium-measured-two-step-generation-change",
            vec![medium_change(
                MediumSpec {
                    source: GenerationSource::Operator,
                    ..medium_spec()
                },
                1,
            )],
            medium_change(
                MediumSpec {
                    family: MediumFamily::Lto(LtoGen::Lto8),
                    source: GenerationSource::Operator,
                    ..medium_spec()
                },
                2,
            ),
            FoldError::MediumFamilyChanged,
        ),
        (
            "medium-record-size-change",
            vec![],
            medium_change(
                MediumSpec {
                    record_size: 4096,
                    ..medium_spec()
                },
                1,
            ),
            FoldError::ImmutableMediumField {
                field: "record_size",
            },
        ),
        (
            "medium-capability-flip",
            vec![],
            medium_change(
                MediumSpec {
                    rerecord: true,
                    ..medium_spec()
                },
                1,
            ),
            FoldError::ImmutableMediumField { field: "rerecord" },
        ),
        (
            "capacity-increase-unmeasured",
            vec![],
            medium_change(
                MediumSpec {
                    usable_logical_bytes: 19_000_000_000_000,
                    source: GenerationSource::Operator,
                    ..medium_spec()
                },
                1,
            ),
            FoldError::CapacityIncreaseRequiresMeasurement,
        ),
        (
            "closed-operation-replay",
            vec![
                accepted.clone(),
                Body::OperationRecorded(OperationRecorded {
                    operation: [31; 16],
                    kind: OperationKind::TapeVerification,
                    previous: Some(dr(3)),
                    event: OperationEvent::Finished(Response {
                        response_fingerprint: None,
                    }),
                }),
            ],
            accepted,
            FoldError::ClosedOperationReplay {
                operation: [31; 16],
            },
        ),
        (
            "second-outcome-parented",
            vec![intent(1), done.clone()],
            second,
            FoldError::OutcomeWithoutOpenIntent,
        ),
        (
            "inventory-resolved-parented",
            vec![intent(1), done],
            Body::InventoryReconciled(InventoryReconciled {
                subject: InventorySubject::Library("LIBA0001".into()),
                observation_generation: 1,
                resolves: Bounded(vec![dr(3)]),
                complete: true,
                elements: Bounded(vec![]),
            }),
            FoldError::OutcomeWithoutOpenIntent,
        ),
        (
            "rule-wrong-counterpart",
            vec![configuration(1), diagnostic()],
            rule_wrong,
            FoldError::WrongCounterpart,
        ),
        (
            "progress-regressed",
            vec![progress(2)],
            progress(1),
            FoldError::ProgressRegressed,
        ),
        (
            "progress-cursor-regressed",
            vec![progress(1)],
            cursor,
            FoldError::ProgressRegressed,
        ),
        (
            "progress-coverage-changed",
            vec![progress(1)],
            coverage,
            FoldError::CoverageChanged,
        ),
        (
            "healthy-without-last-progress",
            vec![progress(1)],
            verdict(),
            FoldError::VerdictWithoutCoverage,
        ),
        (
            "load-three-after-one",
            vec![intent(1)],
            intent(3),
            FoldError::LoadAttempt,
        ),
        (
            "release-assign-pool",
            vec![pool(1, 1)],
            release(3),
            FoldError::WrongCounterpart,
        ),
        (
            "release-diagnostic",
            vec![diagnostic()],
            release(3),
            FoldError::WrongCounterpart,
        ),
        (
            "second-release",
            vec![fence(1), release(3)],
            Body::FenceReleased(FenceReleased {
                detail: Bounded(vec![]),
                fence: 3,
                evidence: "different".into(),
                forced: true,
            }),
            FoldError::FenceAlreadyReleased,
        ),
        ("unknown-tape", vec![], unseen, FoldError::UnknownTape),
        (
            "incarnation-mismatch",
            vec![],
            wrong_inc,
            FoldError::IncarnationMismatch,
        ),
        (
            "retired-identity",
            vec![retire],
            pool(1, 1),
            FoldError::RetiredIdentity,
        ),
        (
            "conflicting-duplicate",
            vec![pool(1, 1)],
            {
                let mut b = pool(1, 1);
                if let Body::AssignPool(v) = &mut b {
                    v.pool = None;
                }
                b
            },
            FoldError::ConflictingDuplicate,
        ),
        (
            "wrong-prior-holder",
            vec![],
            Body::Label(Label {
                tape: tape(2),
                label: Some("A".into()),
                provenance: LabelProvenance::Operator,
                change: GenerationChange {
                    expected: 1,
                    next: 2,
                },
                prior_holder: Some(PriorHolder {
                    tape: tape(2),
                    change: GenerationChange {
                        expected: 1,
                        next: 2,
                    },
                }),
                observed_bot_uuid: None,
            }),
            FoldError::PriorHolder,
        ),
        (
            "observed-identity",
            vec![],
            Body::Label(Label {
                tape: tape(1),
                label: None,
                provenance: LabelProvenance::Operator,
                change: GenerationChange {
                    expected: 1,
                    next: 2,
                },
                prior_holder: None,
                observed_bot_uuid: Some([9; 16]),
            }),
            FoldError::ObservedIdentity,
        ),
        (
            "done-without-confirmation",
            vec![intent(1)],
            Body::MotionOutcome(MotionOutcome {
                intent: dr(3),
                result: MotionResult::Done,
                evidence: evidence(),
                confirmation: None,
            }),
            FoldError::Confirmation,
        ),
        (
            "inventory-generation",
            vec![],
            Body::InventoryReconciled(InventoryReconciled {
                subject: InventorySubject::Library("LIBA0001".into()),
                observation_generation: 0,
                resolves: Bounded(vec![]),
                complete: true,
                elements: Bounded(vec![]),
            }),
            FoldError::InventoryGeneration,
        ),
        (
            "inventory-other-library",
            vec![intent(1)],
            Body::InventoryReconciled(InventoryReconciled {
                subject: InventorySubject::Library("LIBB0002".into()),
                observation_generation: 1,
                resolves: Bounded(vec![dr(3)]),
                complete: true,
                elements: Bounded(vec![]),
            }),
            FoldError::InventoryScope,
        ),
        (
            "unknown-drive",
            vec![],
            Body::DriveDecision(DriveDecision {
                drive_uuid: [30; 16],
                decision: Stewardship::Retire(Reason {
                    reason: "retire".into(),
                }),
                change: GenerationChange {
                    expected: 1,
                    next: 2,
                },
            }),
            FoldError::UnknownDrive,
        ),
        (
            "reinstate-active-drive",
            vec![drive],
            Body::DriveDecision(DriveDecision {
                drive_uuid: [30; 16],
                decision: Stewardship::Reinstate(Reason {
                    reason: "reinstate".into(),
                }),
                change: GenerationChange {
                    expected: 1,
                    next: 2,
                },
            }),
            FoldError::DriveState,
        ),
        (
            "alarm-clear-absent",
            vec![],
            Body::AlarmChanged(AlarmChanged {
                condition_key: "alarm".into(),
                change: GenerationChange {
                    expected: 0,
                    next: 1,
                },
                action: AlarmAction::Clear(Reason {
                    reason: "clear".into(),
                }),
                cause: None,
            }),
            FoldError::AlarmState,
        ),
        (
            "system-acknowledgement",
            vec![alarm],
            Body::AlarmChanged(AlarmChanged {
                condition_key: "alarm".into(),
                change: GenerationChange {
                    expected: 1,
                    next: 2,
                },
                action: AlarmAction::Acknowledge(Unit),
                cause: None,
            }),
            FoldError::SystemAcknowledgement,
        ),
        (
            "operation-broken-chain",
            vec![op.clone()],
            Body::OperationRecorded(OperationRecorded {
                operation: [30; 16],
                kind: OperationKind::TapeVerification,
                previous: Some(dr(1)),
                event: OperationEvent::Finished(Response {
                    response_fingerprint: None,
                }),
            }),
            FoldError::Chain,
        ),
        (
            "second-start",
            vec![op],
            Body::OperationRecorded(OperationRecorded {
                operation: [30; 16],
                kind: OperationKind::TapeVerification,
                previous: Some(dr(3)),
                event: OperationEvent::Started(Unit),
            }),
            FoldError::OperationState,
        ),
        (
            "session-close-absent",
            vec![],
            Body::SessionObserved(SessionObserved {
                session_id: [30; 16],
                kind: SessionKind::Read,
                endpoint: None,
                tape: None,
                event: SessionEvent::Closed,
                previous: None,
                reason: None,
            }),
            FoldError::SessionState,
        ),
        (
            "wrap-digest",
            vec![],
            Body::WrapMapCached(WrapMapCached {
                tape: tape(1),
                calibration_generation: 1,
                action: WrapAction::Cache(Cache {
                    descriptors: Blob(vec![1]),
                    mapped_extent_lba: 1,
                    write_epoch: 1,
                    digest: [0; 32],
                }),
            }),
            FoldError::CacheDigest,
        ),
    ]
}
fn extra_rejections(root: &Path) {
    for (name, prefix, bad, expected) in negative_histories()
        .into_iter()
        .chain(l1w_negative_histories())
    {
        let dir = root.join("rejected").join(name);
        let (mut l, _) = open(&dir, &Present);
        body(&mut l, provision(1, Some("A")));
        body(&mut l, provision(2, Some("B")));
        for b in prefix {
            body(&mut l, b);
        }
        let n = l.state().last_sequence + 1;
        assert!(
            matches!(l.append(bad.clone(),AuditActor::System,SourceLayer::Layer4,&Present),Err(StoreError::Fold(e)) if e==expected),
            "{name}"
        );
        let r = record(bad, n);
        let (bytes, _) = codec::encode(n, l.state().terminal_hash, &r).unwrap();
        drop(l);
        fs::OpenOptions::new()
            .append(true)
            .open(segments(&dir).remove(0))
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        assert_eq!(
            replay(&dir, LOG, &Present).unwrap().refusal.unwrap().fault,
            RefusalKind::Fold(expected),
            "{name}"
        );
    }
    for (name, b, key, code) in [
        (
            "session-kind-zero",
            Body::SessionObserved(SessionObserved {
                session_id: [1; 16],
                kind: SessionKind::Read,
                endpoint: None,
                tape: None,
                event: SessionEvent::Opened,
                previous: None,
                reason: None,
            }),
            1,
            0,
        ),
        (
            "recovery-action-zero",
            Body::RecoveryRecorded(RecoveryRecorded {
                at: at(1, 1),
                action: RecoveryAction::AppendAuthorityRecovered(Recovered {
                    source_digest: [0; 32],
                }),
                operation: [1; 16],
                prior: None,
            }),
            1,
            0,
        ),
        ("diagnostic-event-twenty-one", diagnostic(), 0, 21),
    ] {
        let dir = root.join("rejected").join(name);
        let (mut l, _) = open(&dir, &Present);
        body(&mut l, provision(1, None));
        let segment = l.state().segments[0].name.clone();
        drop(l);
        let mut s = Vec::new();
        wire::encode(&record(b.clone(), 2), &mut s).unwrap();
        let mut value: ciborium::value::Value = ciborium::de::from_reader(s.as_slice()).unwrap();
        let ciborium::value::Value::Map(top) = &mut value else {
            unreachable!()
        };
        let ciborium::value::Value::Map(fields) = &mut top[1].1 else {
            unreachable!()
        };
        if name == "recovery-action-zero" {
            let ciborium::value::Value::Array(v) = &mut fields[key].1 else {
                unreachable!()
            };
            v[0] = ciborium::value::Value::Integer(code.into());
        } else {
            fields[key].1 = ciborium::value::Value::Integer(code.into());
        }
        s.clear();
        ciborium::ser::into_writer(&value, &mut s).unwrap();
        let mut f = Vec::new();
        crate::log_frame::encode_frame(&mut f, 1, b.tag() as u16, 2, &s, &[0; 32]).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(dir.join(segment))
            .unwrap()
            .write_all(&f)
            .unwrap();
    }
    for name in [
        "segment-first-gap",
        "previous-segment-hash",
        "torn-interior",
    ] {
        let dir = root.join("rejected").join(name);
        copy_dir(&root.join("rotation-clock-regression"), &dir);
        let paths = segments(&dir);
        if name == "torn-interior" {
            let mut b = fs::read(&paths[1]).unwrap();
            b.pop();
            fs::write(&paths[1], b).unwrap();
        } else {
            let mut b = fs::read(&paths[1]).unwrap();
            if name == "segment-first-gap" {
                b[26..34].copy_from_slice(&9u64.to_le_bytes());
            } else {
                b[34] ^= 1;
            }
            let crc = remanence_crc::crc64_xz(&b[..76]);
            b[76..84].copy_from_slice(&crc.to_le_bytes());
            fs::write(&paths[1], &b).unwrap();
            if name == "segment-first-gap" {
                fs::rename(
                    &paths[1],
                    dir.join("00000000000000000009-2026-11-04.remlib"),
                )
                .unwrap();
            }
        }
    }
}

const FOLD_SCHEMA_NOTES:&str="## Record tags, causal keys and scopes\n\nEvery record except FenceRaised and VerificationVerdict with `at: None` also orders on the current label of each tape it names in its direct tape set. For this rule the direct tape set is the tape field of tags 1, 3, 5, 14 and 25, both holders for tag 2, the TapeAt tape of tags 6, 7, 13, 15 and 26, and the optional tape or cartridge of tags 10, 17, 20, 21 and 24; other tags have no direct tapes. Reprovision adds its new and predecessor Tape scopes and its explicit new Label, but does not add the predecessor current label. A DecisionRef adds only the referenced record's direct scopes, defined separately from the ordering column below. These are Tape for its direct tape set above (also the scope tape for FenceRaised, and the new and predecessor tapes for Reprovision); Operation for the operation of tags 6, 7, 13, 14, 15, 17, 23 and 26, the optional operation of tags 8 and 20, the adoption operation of tag 4, the parent operation of tag 10, and the session_id of tag 24. Additional direct scopes are: the explicit new Label of tags 2 and 4; Endpoint of a drive FenceRaised; Endpoint of a Load/Unload MotionIntent, plus Library for a bay endpoint, or Library of a Move MotionIntent; Library or standalone Endpoint of tag 12; Config of tag 16, and Config of a Rule AssignPool or Rule Reprovision that references a configuration; Endpoint of tag 17; Drive and optional Endpoint of tag 18; Drive of tags 19 and 20; CleaningRun and optional Drive of tag 21; Alarm of tag 22; and optional Endpoint of tag 24. Thus a reference to a fence inherits its scope Tape or drive Endpoint plus its optional Operation, independent of tag 8's deferral column. Inheritance is one level only: no current labels, no scopes the referenced record itself inherited, and no Config from a referenced Rule AssignPool or Rule Reprovision. A reference to FenceReleased inherits nothing. FenceReleased's own ordering instead adds only the target Tape or Endpoint of the fence it names, and only while that fence is open (a raised fence still awaiting its queued check counts as open), as described in tag 9. A release of a fence that is no longer open orders on nothing.\n\n| Tag | Record | Scopes | Causal rule |\n|---:|---|---|---|\n| 1 | AssignPool | Tape, current Label; also Config when `source` is `Rule` with a configuration reference | `change.expected` = the tape's `assignment_generation`. The tape is provisioned and not retired. `Rule` requires `configuration` to name the latest retained `ConfigurationAccepted`; an older reference is `StaleConfiguration`, including an identical retry. History classifies a mismatching reference: only an older ConfigurationAccepted is StaleConfiguration; another kind or absent reference is WrongCounterpart. `pool`, if present, is non-empty. An Operator assignment without a configuration reference has no Config scope; if it supplies a reference, the general historical-reference rule adds that referenced record's direct scopes |\n| 2 | Label | Tape (both holders), Label (old and new) | `change` against the tape's `label_generation`. Labels are unique among live tapes. If the label is held by live tape X, `prior_holder` must name X with X's current `label_generation`. The one record moves the label, clears X's label and bumps X's generation. If the label is free, `prior_holder` is null. `observed_bot_uuid`, if present, equals `tape.uuid`. **Recovery 4** |\n| 3 | Retire | Tape, Label | `assignment_generation` equals the current one. The tape is live. Retire ends the incarnation for good and releases its label and pool. `observed_tape_sequence` is evidence, never a prerequisite. Later live-target records cannot name a retired `TapeRef` (`RetiredIdentity`); `Reprovision.predecessor` may name its retired predecessor and `FenceRaised.at` remains evidential |\n| 4 | Reprovision | Tape (new and predecessor), Label, Operation (adoption); also Config when `pool_source` is `Rule` | A new UUID, never seen before. `predecessor`, if present, is retired. The label, if any, is free. `label_provenance` is null exactly when `label` is. `adoption` is present exactly for `Adopt`. Mandatory MediumSpec owns the sole fixed block size (record_size > 0, multiple of 512 for adoption). record_size, rerecord, lifecycle_ops and family kind are fixed for this incarnation; changing them requires a new incarnation. Former key 5 block_size is removed; parity_scheme is key 5, medium key 6, pool key 7, endpoint key 8 and adoption key 9. Sets assignment, label and media generations to 1 in one record; no separate `Label` or `AssignPool` for the initial state. Keys 10 pool_source, 11 operation and 12 replaced_bot are optional. PoolSource is Operator(Unit)=0 or Rule(DecisionRef)=1. Rule must name the latest retained ConfigurationAccepted, exactly as tag 1 (StaleConfiguration/WrongCounterpart); a Rule Reprovision, like a Rule AssignPool, orders after the configuration it references (Config scope). The fold does not check the presence coupling of pool_source to pool, or of operation to Initialize. replaced_bot is the overwritten BOT UUID, evidence only; it is InvalidSchema unless mode is Initialize and predecessor is None. No decision reads it. |\n| 5 | MediumSpecSet | Tape | `change` against the live tape's `media_revision`. Family kind (Lto/Disk) is fixed. Within Lto, generation may change only when the incarnation's derived ever_measured bit is false; otherwise MediumFamilyChanged. Provisioning (tag 4) initializes ever_measured from its own source, and every committed MediumSpecSet with source Measured sets it. The bit never clears within an incarnation, even if source is later revised to Label or Operator; it is derived from records, not stored on the wire. record_size, rerecord and lifecycle_ops must equal their current values, otherwise ImmutableMediumField { field }. Only usable_logical_bytes and source are generally revisable, with the stated LTO correction exception. Capacity may stay equal or decrease; an increase requires source Measured on this new revision (fresh measurement), otherwise CapacityIncreaseRequiresMeasurement. A retry is not a fresh revision. Fixed geometry and capabilities can change only through a new incarnation |\n| 6 | SupersedeTail | Tape, Operation | **Pending dependency** on `at`. Only this tag permits sequence 0, naming the existing native log's empty prefix: record_digest is SHA-256 of the exact immutable header bytes. Missing or replaced native identity contradicts it; it never raises the repair floor above 0. Other dependent tags and evidential FenceRaised.at still refuse sequence 0 as InvalidSchema. operation, session_id and preview_digest are nullable (keys remain mandatory); absent branch facts must not be fabricated. At most one per (`at.tape`, `session_id`), including session_id null. B4b's tape fold checks the `SessionEnded.crash_ack_audit_sequence` that names it (kind, tape, session, count); B2 provides `LibraryFold::supersede(sequence)` for that lookup |\n| 7 | AcceptUnprotectedFinalization | Tape, Operation | **Pending** on `at`. `w < t`. At most one per (incarnation, operation). Codec, fold and fixtures exist now. **No production constructor**; step 6 adds the emitter |\n| 8 | FenceRaised | scope Tape only for ordering; none for DriveFault | ContainerQuarantine (cause 12) and Model (scope 2) decode but are refused before queuing or committing with ReservedNotYetUsed. `Write` requires tape scope; `DriveFault` requires drive scope (`FenceScopeMismatch`). An unqueued tape-scoped fence requires a live tape. A live or replay fence takes effect immediately. It defers its live-tape check only behind pending records on `Scope::Tape` of its scope tape; its queue entry contains only that scope. Label, operation and endpoint scopes neither block it nor are blocked by it while queued. A DriveFault fence has no tape-ordered check and never defers. A fence on a tape unknown while that tape has queued records is accepted; if its tape is unknown, of another incarnation or retired at resolution, the check is dropped and the failure is attached to the open fence as its full TapeRef and cause. This creates no tape contradiction; releasing the fence removes the report. The check runs in tape queue order; B4b writers check fence targets themselves. Contradiction status never decides a fence check. A fence raised on a contradicted tape with no pending predecessor validates liveness immediately. A queued check remains pending during replay even if a predecessor is later excluded. The next `resolve`, which runs only before a live append, retries it in tape queue order; a remaining NotYet predecessor still holds the check. The fence stays open throughout. The fence id is this record's sequence. No deduplication: every raise is its own fence. **`at` is evidence, not a dependency:** the fence applies at once, and an unresolvable `at` is reported, never waited for. Key 6 detail is Bounded<DiagnosticEntry,256>, strictly ascending unique UTF-8 byte keys (InvalidSchema). The reason key is Text <=4096, presentation only. Cause 14 is NativeLogBehindProjection; cause 15 is unallocated and has no variant. Cause-14 typed evidence keys are listed below. A two-target event uses two records, one per scope; scope is never widened. |\n| 9 | FenceReleased | target Tape or Endpoint of the named fence while it is open; none otherwise | `fence` remains a u64 FenceId: the raise sequence within the header-bound log UUID. Only this reference form uses sequence-only LibraryHistory lookup; DecisionRef always matches the record UUID. An earlier raise no longer open is `FenceAlreadyReleased`, even after later fences; another tag is `WrongCounterpart`; absent history is `ReleaseWithoutFence`. Zero/forward references remain `ForwardReference`. The latest identical release retry remains a no-op. A release that differs from an earlier release of the same fence is `FenceAlreadyReleased`. While the named fence is open, the release orders on its target tape or endpoint, without inheriting the raise's operation. A release of a fence that is no longer open, including an identical retry of an earlier release, orders on nothing. A release cannot resolve a motion, a pending record or an ambiguity. Key 3 detail is Bounded<DiagnosticEntry,256>, strictly ascending unique byte keys. Nonempty detail is InvalidSchema except on a forced release of a cause-14 fence; its typed keys are below. Empty detail is the definite empty array 0x80 and the body map has four entries. forced=true is an operator release without the cause's release proof; forced=false is a release made under the cause's release rule, which is either a producer's proof or an operator's acknowledgement where the fence accepts one. |\n| 10 | MotionIntent | Tape, Endpoint, Library, Operation | `command` is unique among open intents; a `command` already open is `ConflictingDuplicate`. **The LOAD attempt count lives only here** (item 6): `attempt` is 1 plus the number of earlier `Load` intents with the same `parent_operation` and endpoint; with no parent it is 1. An intent with no outcome makes its library (a `Bay` endpoint's serial, a `Move`'s serial, or a standalone endpoint) **InventoryUnknown** |\n| 11 | MotionOutcome | intent scopes | `intent` names an open `MotionIntent` (`OutcomeWithoutOpenIntent`; `WrongCounterpart` if another tag). An intent reference naming an existing sequence with a different record UUID is `OutcomeWithoutOpenIntent`. One outcome per intent. `Done` requires a confirmation: Inventory for Move and Unload; Readiness for Load when a readiness flow follows, otherwise Inventory (including cleaning). The writer validates that the sequence exists and has the confirmation kind. An inventory confirmation must observe without resolving that intent; the following Done consumes it. `Unknown` leaves the library InventoryUnknown |\n| 12 | InventoryReconciled | observed Library or standalone Endpoint; resolved intent scopes | Key 0 is InventorySubject: [0, library serial] or [1, Standalone map]. Observation generation strictly increases per subject. Library observations use element status; a partial observation resolves only intents whose affected elements are all listed. Reconciliation clears exactly the named uncertainty and creates no placement ownership. Standalone observations use TEST UNIT READY plus READ POSITION, require complete=true and empty elements, and resolve only open or Unknown LOAD/UNLOAD intents on that exact endpoint. Naming another endpoint or a changer intent is InventoryScope; stale generation is InventoryGeneration. Each resolved intent is consumed once. Confirmation observations leave resolves empty, followed by MotionOutcome Done; never resolve and then append an outcome for the same intent |\n| 13 | VerificationProgress | Tape, Operation | **Pending** on `at`. Within one operation, `at`, `plan_digest` and `modes` never change (`CoverageChanged`). `parts_done` strictly increases and `next` strictly advances in lexicographic order (`ProgressRegressed`). `next.part = parts_done` (`ImpossibleCursor`). Progress is never a verdict. **No production constructor in step 4** (item 7); step 7 emits it. modes uses the nonzero seven-bit verification mask below; zero or any bit 7–31 is InvalidSchema. |\n| 14 | VerificationVerdict | Tape (`tape`), Operation; progress scopes when referenced | at (key 1) is optional; tape (key 7) and outcome (key 8) are mandatory. Some(at) requires at.tape == tape (InvalidSchema) and waits on that native proof. None is a producer assertion that no native reference existed at recording time; the fold does not check it. Receive and replay accept None without native lookup, preserve normal pending Tape/Operation order only (no label scopes), record no floor and invent no native sequence. Outcomes are VerifiedComplete=0, VerifiedDegraded=1, RecoveryRequired=2. VerifiedComplete requires damaged empty; the other outcomes may have no ranges. Each range requires start < end; unprotected retains its independent unprotected-tail meaning, w < t. modes uses the nonzero seven-bit table below (InvalidSchema for zero or bits 7–31). progress still names the latest progress for the operation with identical at and modes (VerdictWithoutCoverage). The producer records the verdict before the operation's `Finished`; the fold does not check this order. The verdict supersedes only that operation's progress. A referenced verdict at N says nothing about N+1; None says nothing about any native sequence |\n| 15 | ManualFinalizationAccepted | Tape, Operation | **Pending** on `at`, which names the tape log's `FinalizationPrepared` record; its digest binds the immutable plan, so there is no separate plan digest. `assignment_generation` equals the tape's current one (`GenerationMismatch`; item 9's \"Accepted with a stale generation\"). One per incarnation. Retired in step 6 |\n| 16 | ConfigurationAccepted | Config | `generation` is the previous one plus 1, starting at 1. `config_digest` = SHA-256 of the canonical bytes of `definitions` (`ConfigDigest`). It holds only catalog-relevant fields, never a secret or a TLS key. Assignments still need `AssignPool`; replay never reinterprets an older config |\n| 17 | ReadinessObserved | Tape, Endpoint, Operation, previous and intent scopes | `scope` (key 13) is Endpoint=0 by default or Library=1; Library requires a bay endpoint and projects selected-library-snapshot admission uncertainty across all bays. `media` (key 14) is an optional MediumFamily, allowed only without `tape`; with a tape the family is its MediumSpec and supplying media is InvalidSchema. `previous` names the last `ReadinessObserved` of the same operation, and is null for its first. `load_intent`, if present, names an open or resolved `Load` intent on the same endpoint; resolved records use LibraryHistory. **It carries no LOAD count** (item 6). `ready_generation` strictly increases per endpoint. A terminal `result` closes the operation. Terminal is enforced for chained successors; reuse of a closed unkeyed operation id is not detected in fold state. B4b writers mint fresh ids. limit_ms (key 8) is Option<u64>: None means no deadline, with no sentinel. Keys 15 source_slot (Option<u16>), 16 run (Option<Id>), and 17 cancel_source (Option<CancelSource>) are optional. CancelSource codes are Operator=0, Shutdown=1, StartupReconcile=2. Phases 49 LoadDriveReadiness and 50 LoadDriveCompletion are distinct, with no aliases. Device paths are not recorded; presentation belongs in Diagnostic. |\n| 18 | DriveObserved | Drive, Endpoint | The first observation creates the drive with `stewardship_generation = 1`. The observation time is the envelope's |\n| 19 | DriveDecision | Drive | The drive is known. `change` against `stewardship_generation`. Retire requires active; Reinstate requires retired. Fencing is tags 8 and 9, never this |\n| 20 | DriveHealthObserved | Drive, Tape, Operation | The drive is known. A non-null (`session_id`, `trigger`) matching the drive's latest retained snapshot is a no-op if the body is identical, otherwise `ConflictingDuplicate`. The fold makes no lifetime uniqueness claim. B4's health projector must retain the (`session_id`, `trigger`) unique key with first-wins: ignore an identical later body; report a different later body as a projection conflict and never apply it. Null-session snapshots are independent. Counters are cumulative per mount |\n| 21 | CleaningRecorded | CleaningRun, Drive, Tape | `Begin` carries only trigger, with no home slot. `Advance` key 1 is home_slot (Option<u16>), present exactly when phase is MovingIn; that record must also name the selected cartridge. Missing slot/cartridge at MovingIn or a slot at any other phase is InvalidSchema. MovingBack returns to the run's MovingIn slot, retained through omitted cartridge/slot fields and delayed drive binding. Register.home_slot remains the initial home, and the projected cartridge home follows the latest run. `previous` chains a run's records. At most one active run per drive and per cartridge (today's unique indexes). Register, Expire and Reject concern the cartridge, not a run. Reject(Reason), wire code 5, requires a registered cartridge that is neither expired nor rejected (UnknownTape, WrongMediumKind, or RetiredIdentity otherwise). It permanently prevents selection and verified credit, including credit through an inherited run binding. It projects cleaning_state=rejected, distinct from expired. Reject precedes the separate Finish(Failed): a crash between them leaves an ineligible cartridge and an active run; replay cannot make it eligible. Failed closure and return motion remain legal after rejection. Reject has no previous run reference and does not advance the run chain. Register creates an inventory-only cleaning entity without MediumSpec or BOT identity: cartridge must be UUIDv5(NAMESPACE_OID, UTF-8 `remanence:cleaning-cartridge:{barcode}`), incarnation 1, previously unknown as either medium kind, except that ruling 17 permits a retired data UUID after Retire releases its label. The data retirement remains historical; a live data UUID still conflicts. The barcode must be nonempty and trimmed. A known live data or any cleaning UUID is ConflictingDuplicate; a malformed identity is InvalidSchema. Only Register exempts its cartridge from UnknownTape. Expire ends the cleaning entity permanently; it cannot be registered again. An inherited run cartridge cannot bypass expiry to advance or receive verified credit; Finish without verified credit may close the run. Data-tape records naming a cleaning cartridge, including Reprovision.predecessor and every dependent record, fail WrongMediumKind before native lookup. WrongMediumKind is fold-only, with no new wire code. The projector creates a cleaning tape row with null block_size and the home slot. **The fold never derives a fence from cleaning** (item 11): a run that needs an operator is a separate `FenceRaised(DriveFault)` |\n| 22 | AlarmChanged | Alarm, cause scopes | `change` against the alarm generation (0 when never raised). Raise requires absent or cleared; Acknowledge requires raised and unacknowledged, with an envelope actor other than System (the acknowledger is the actor, recorded once); Clear requires raised. Refresh (action code 3, payload keys severity=0/reason=1/cause=2) requires raised, acknowledged or not, advances generation by CAS, replaces severity/reason/cause, and preserves acknowledgement and its actor; a subsequent acknowledgement still refuses. `cause` may name any earlier record, using LibraryHistory after replacement. AlarmKind adds DriveSerialCollision=11 and RetiredDriveReappeared=12. |\n| 23 | OperationRecorded | Operation | `previous` chains the operation's records. Accepted comes first or not at all. Started follows nothing or Accepted. Cancel records follow a non-terminal state. Finished, Failed, CancelledBeforeDispatch, CompletedAfterCancel and CompletionUnknown are terminal; nothing follows them. The idempotency scope (`actor_fingerprint`, `kind`, `idempotency_key`) is unique: a second Accepted for that scope with a different operation or fingerprint is a `ConflictingDuplicate`. An identical keyed retry yields `Duplicate(original receipt)` while Accepted is latest, `Chain` after Started or another non-terminal successor, or `ClosedOperationReplay { operation }` after the operation closes. B4b's API must answer all three from its projection, treating `Chain` for an identical keyed Accepted as already running and using the operation id for a closed retry because the fold does not retain terminal receipts. Terminal is enforced for chained successors; reuse of a closed unkeyed operation id is not detected in fold state. B4b writers mint fresh ids. **No progress variant**: progress gates nothing in step 4 and stays `Diagnostic`. Failed.refusal_code names the RefusalCode table under Failed. |\n| 24 | SessionObserved | Operation(session), Tape, Endpoint | Opened comes first; the other events are terminal. **A write session cannot be expressed**: write sessions are the tape log's (item 5b), and kind 0 is refused. It has no receipts, because durable receipts are tape-log only |\n| 25 | WrapMapCached | Tape | `digest` = SHA-256(`descriptors`), whose bytes are today's descriptor serialisation, opaque to the fold. Only digest and calibration_generation are retained per tape; the projector reads descriptor bytes from the source record. `calibration_generation` never decreases per tape. The RAO control store (the third authority) still decides servability; this rebuilds only the SQL cache |\n| 26 | RecoveryRecorded | Tape, Operation, prior scopes | **Pending** on `at`. RepairApplied requires `retained_through = at.sequence`; a mismatch is InvalidSchema in shape validation, before any view lookup. On receipt or live validation, RepairBelowReference { highest } refuses a repair below any earlier received dependent reference on that tape, whether applied, duplicate, pending or excluded. Duplicates count toward the floor; refused candidates do not. A prior repair is itself a reference, so repairs are monotone. LibraryFold::highest_reference(tape) exposes the same floor for the future repair writer. An admitted repair is an ordinary dependent record, judged on its own reference; it neither clears contradictions nor undoes records. Resolution does not reapply the floor against references received after that repair. **`AppendRecoveryRequired` does not exist** (item 5c): action 0 is refused. Missing native proof stays pending. `prior` must name an earlier RecoveryRecorded, including one replaced in retained state |\n| 32 | Diagnostic | none | Detail keys are strictly ascending and unique (InvalidSchema); a nested value is refused at encode. **The fold validates and ignores it.** `DiagnosticEvent` is B2's own closed enum (below). DiagnosticEvent 20 RefusalRecorded requires code (Unsigned, an allocated RefusalCode from the Failed table) and name (Text); subject_kind/subject_id carry its subject. |\n\nAll generation failures use GenerationMismatch. RepairBelowReference { highest } is a fold-only refusal with no new wire code. Shape violations use InvalidSchema; pair/type failures use WrongCounterpart or the named rule error. For a dependent record, digest mismatch is a scoped `TapeContradiction`, checked after shape and tape identity and before other library rules in validate_at. The receipt-time repair floor is also enforced for a scope-held repair, independent of its native view. A matching digest with the wrong native kind is the typed refusal `CounterpartKind`: replay stops there, and nothing is held for that record. Only tag 15 constrains the kind (`FinalizationPrepared`). Tag 3 adds mandatory key 4, detail: text <=4096 bytes or null; it is presentation only. Tag 21 is removed from production orchestration in step 5; its historical decoder remains.\n\n### Additional wire conventions\n\nId is b16; Hash is b32; Text/TextValue is UTF-8 text <=4096 bytes; LabelText is UTF-8 text <=32 bytes. Blob<N> is a CBOR byte string <=N bytes; Bounded<T,N> is a definite array <=N entries. Integer types are unsigned, except Negative, the diagnostic i64 <0. Struct names are mandatory ascending integer-keyed maps. Unit is null. DiagnosticEntry is [text,DiagnosticValue]. AuditActor is {0: kind (System=0, User=1, Service=2), 1: name (null exactly for System)}. Pool watermark fields encode the existing f64 IEEE-754 bits as u64, preserving exact configuration values without CBOR floats; they are not text.\n\n### Verification modes (tags 13 and 14)\n\nmodes is u32, nonzero; bits 7–31 must be zero (InvalidSchema).\n\n| Bit | Name | Meaning |\n|---:|---|---|\n| 0 | TERMINAL_INDEX | Five terminal components validated |\n| 1 | PHYSICAL_PREFIX | Every physical prefix row read; verified_prefix_record_count present |\n| 2 | PROTECTED_CONTENT | REM-PARITY 2.2 protected-content walk ran |\n| 3 | REPLICAS | replica_health evaluated |\n| 4 | SEPARATIONS | separation_health evaluated |\n| 5 | EOD_MEASURED | measured_eod_lba obtained from the drive |\n| 6 | BOT_RECOVERY | verification_basis=bot_structural_recovery; recovery_inventory attached |\n\n### Fence detail keys\n\nAll detail arrays reuse DiagnosticEntry and are limited to 256 entries. Keys are unique and strictly ascending in UTF-8 byte order; producers construct them through BTreeMap<Text, DiagnosticValue>. Human-readable reason is presentation only and never selects a decision.\n\nFenceRaised cause 14 uses the following typed evidence keys (in byte order):\n\n| Key | DiagnosticValue type / meaning |\n|---|---|\n| basis | Text: floor, mark, both |\n| conflict_digests | Bytes: 32 bytes per digest |\n| floor_digest | Bytes |\n| floor_named_by | Unsigned |\n| floor_sequence | Unsigned; may be zero with floor_digest |\n| mark_offset | Unsigned |\n| mark_sequence | Unsigned |\n| observed_closed_end | Unsigned |\n| observed_floor | Text: absent, behind, digest_conflict, digest_mismatch |\n| observed_last_closed | Unsigned |\n| observed_mark | Text: absent, behind, short_bytes, digest_mismatch |\n| source_byte_end | Unsigned |\n| source_digest | Bytes |\n\nA zero floor sequence is evidence in detail; FenceRaised.at.sequence must remain nonzero. The optional reason key is Text <=4096 bytes. The fold validates detail ordering; the typed evidence vocabulary describes the producer contract.\n\nFenceReleased detail is empty except for a forced release of a cause-14 fence. That release uses these seven keys in byte order:\n\n| Key | DiagnosticValue type / meaning |\n|---|---|\n| basis | Text: floor |\n| floor_digest | Bytes |\n| floor_sequence | Unsigned |\n| observed_closed_end | Unsigned |\n| observed_floor | Text |\n| observed_last_closed | Unsigned |\n| observed_prefix_digest | Bytes |\n";

fn optional_forms(l: &mut LibraryLog, config: DecisionRef) {
    l1w_optional_forms(l, config);
    for layer in [
        SourceLayer::Layer2,
        SourceLayer::Layer3b,
        SourceLayer::Layer3c,
        SourceLayer::Layer4,
        SourceLayer::Layer5,
    ] {
        l.append(diagnostic(), AuditActor::System, layer, &Present)
            .unwrap();
    }
    l.append(
        diagnostic(),
        AuditActor::Service("fixture-service".into()),
        SourceLayer::Layer5,
        &Present,
    )
    .unwrap();
    let mut p = provision(6, Some("SIX"));
    if let Body::Reprovision(v) = &mut p {
        v.medium.family = MediumFamily::Disk;
        v.medium.rerecord = true;
        v.medium.lifecycle_ops = true;
        v.parity_scheme = Some(ParityScheme {
            id: "rs-cauchy-gf256-v1".into(),
            data_blocks_per_stripe: 128,
            parity_blocks_per_stripe: 4,
            stripes_per_neighborhood: 128,
        });
        v.pool = Some("pool-a".into());
        v.endpoint = Some(endpoint("LIBB0002"));
    }
    body(l, p);
    let mut p = provision(7, Some("ADOPT"));
    if let Body::Reprovision(v) = &mut p {
        v.tape.uuid[6] = 0x47;
        v.tape.uuid[8] = 0x87;
        v.mode = ProvisionMode::Adopt;
        v.pool = Some("pool-a".into());
        v.endpoint = Some(endpoint("LIBB0002"));
        v.adoption = Some(Adoption {
            request_fingerprint: [67; 32],
            operation: [67; 16],
            idempotency_key: [67; 16],
            physical_tail: PhysicalTail::DataAfterBootstrap,
            state: AdoptedState::RecoveryRequired,
            library_revision: Some("r1".into()),
            home_slot: Some(2),
            drive_serial: "DRIVE-B".into(),
            identity_only: true,
            bootstrap_drive_compression: false,
            configured_drive_compression: false,
        });
    }
    body(l, p);
    body(
        l,
        Body::DriveObserved(DriveObserved {
            drive_uuid: [31; 16],
            serial: None,
            identity_source: IdentitySource::Derived,
            endpoint: None,
            managed: Managed::Foreign,
            inventory_generation: 2,
            vendor: None,
            product: Some("product".into()),
            firmware_rev: Some("rev".into()),
        }),
    );
    body(
        l,
        Body::DriveDecision(DriveDecision {
            drive_uuid: [30; 16],
            decision: Stewardship::Annotate(Annotation {
                purchase_date: None,
                warranty_until: Some("2028".into()),
                cost: None,
                notes: Some("notes".into()),
            }),
            change: GenerationChange {
                expected: 4,
                next: 5,
            },
        }),
    );
    body(
        l,
        Body::DriveHealthObserved(DriveHealthObserved {
            drive_uuid: [30; 16],
            tape: None,
            session_id: None,
            operation: Some([69; 16]),
            trigger: HealthTrigger::Manual,
            tape_alert_flags: 0,
            counters: Counters {
                write_corrected: None,
                write_uncorrected: Some(0),
                read_corrected: None,
                read_uncorrected: Some(0),
            },
            raw_pages: None,
        }),
    );
    body(
        l,
        Body::InventoryReconciled(InventoryReconciled {
            subject: InventorySubject::Library("LIBB0002".into()),
            observation_generation: 1,
            resolves: Bounded(vec![]),
            complete: true,
            elements: Bounded(vec![Element {
                address: 1,
                kind: ElementKind::Storage,
                barcode: None,
                full: false,
            }]),
        }),
    );
    let r = body(
        l,
        Body::CleaningRecorded(CleaningRecorded {
            run_id: "optional-run".into(),
            previous: None,
            drive_uuid: Some([31; 16]),
            cartridge: None,
            action: CleaningAction::Begin(Begin {
                trigger: CleanTrigger::Periodic,
            }),
        }),
    );
    body(
        l,
        Body::CleaningRecorded(CleaningRecorded {
            run_id: "optional-run".into(),
            previous: Some(reference(&r)),
            drive_uuid: Some([31; 16]),
            cartridge: None,
            action: CleaningAction::Finish(Finish {
                result: CleanResult::NeedsOperator,
                verified_credit: false,
            }),
        }),
    );
    body(
        l,
        Body::AlarmChanged(AlarmChanged {
            condition_key: "optional-alarm".into(),
            change: GenerationChange {
                expected: 0,
                next: 1,
            },
            action: AlarmAction::Raise(AlarmRaise {
                kind: AlarmKind::NoClnCart,
                severity: Severity::Critical,
                reason: "empty".into(),
            }),
            cause: Some(config),
        }),
    );
    let r = body(
        l,
        Body::OperationRecorded(OperationRecorded {
            operation: [70; 16],
            kind: OperationKind::TapeVerification,
            previous: None,
            event: OperationEvent::Accepted(Accepted {
                actor_fingerprint: "actor".into(),
                idempotency_key: None,
                request_fingerprint: [70; 32],
                subject: None,
            }),
        }),
    );
    body(
        l,
        Body::OperationRecorded(OperationRecorded {
            operation: [70; 16],
            kind: OperationKind::TapeVerification,
            previous: Some(reference(&r)),
            event: OperationEvent::Failed(Failed {
                retry: None,
                refusal_code: None,
                summary: "failed".into(),
            }),
        }),
    );
    let r = body(
        l,
        Body::SessionObserved(SessionObserved {
            session_id: [71; 16],
            kind: SessionKind::Operation,
            endpoint: Some(endpoint("LIBB0002")),
            tape: None,
            event: SessionEvent::Opened,
            previous: None,
            reason: None,
        }),
    );
    body(
        l,
        Body::SessionObserved(SessionObserved {
            session_id: [71; 16],
            kind: SessionKind::Operation,
            endpoint: Some(endpoint("LIBB0002")),
            tape: None,
            event: SessionEvent::Closed,
            previous: Some(reference(&r)),
            reason: None,
        }),
    );
}

/// Write every valid prefix record through append while native proof is present.
/// Replay with that proof absent then exercises the pending cap.
pub(super) fn pending_limit_prefix(dir: &Path, count: usize) {
    let (mut log, _) = open(dir, &Present);
    body(&mut log, provision(1, None));
    for i in 1..=count {
        body(&mut log, progress(i as u32));
    }
}
/// One extra valid frame must stop replay with all 4096 queued bodies intact.
pub(super) fn pending_limit_case(dir: &Path) {
    pending_limit_prefix(dir, MAX_PENDING_RECORDS + 1);
}

/// Ruling 15 rejects both directions of data/cleaning identity reuse.
fn cleaning_identity_cases() -> Vec<(&'static str, Vec<Body>, Body, Option<FoldError>)> {
    let registration = |tape| {
        Body::CleaningRecorded(CleaningRecorded {
            run_id: "register".into(),
            previous: None,
            drive_uuid: None,
            cartridge: Some(tape),
            action: CleaningAction::Register(Register {
                barcode: "CLN000".into(),
                home_slot: 1,
            }),
        })
    };
    let cartridge = cleaning_cartridge_ref(&"CLN000".into());
    let mut assign = pool(1, 1);
    if let Body::AssignPool(v) = &mut assign {
        v.tape = cartridge;
    }
    let action = |action| {
        Body::CleaningRecorded(CleaningRecorded {
            run_id: "cartridge".into(),
            previous: None,
            drive_uuid: None,
            cartridge: Some(cartridge),
            action,
        })
    };
    let reject = action(CleaningAction::Reject(Reason {
        reason: "corroboration".into(),
    }));
    let begin = action(CleaningAction::Begin(Begin {
        trigger: CleanTrigger::Manual,
    }));
    let expire = action(CleaningAction::Expire(Reason {
        reason: "used".into(),
    }));
    let inherited_finish = |credit| {
        Body::CleaningRecorded(CleaningRecorded {
            run_id: "cartridge".into(),
            previous: Some(record(begin.clone(), 4).reference(4)),
            drive_uuid: None,
            cartridge: None,
            action: CleaningAction::Finish(Finish {
                result: if credit {
                    CleanResult::Done
                } else {
                    CleanResult::Failed
                },
                verified_credit: credit,
            }),
        })
    };
    vec![
        (
            "cleaning-expired-inherited-credit",
            vec![registration(cartridge), begin.clone(), expire.clone()],
            inherited_finish(true),
            Some(FoldError::RetiredIdentity),
        ),
        (
            "cleaning-expired-failed-closure",
            vec![registration(cartridge), begin.clone(), expire],
            inherited_finish(false),
            None,
        ),
        (
            "cleaning-reject",
            vec![registration(cartridge)],
            reject.clone(),
            None,
        ),
        (
            "cleaning-reject-data",
            vec![],
            Body::CleaningRecorded(CleaningRecorded {
                run_id: "reject-data".into(),
                previous: None,
                drive_uuid: None,
                cartridge: Some(tape(1)),
                action: CleaningAction::Reject(Reason {
                    reason: "corroboration".into(),
                }),
            }),
            Some(FoldError::WrongMediumKind),
        ),
        (
            "cleaning-reject-unregistered",
            vec![],
            reject.clone(),
            Some(FoldError::UnknownTape),
        ),
        (
            "cleaning-reject-after-expire",
            vec![
                registration(cartridge),
                action(CleaningAction::Expire(Reason {
                    reason: "used".into(),
                })),
            ],
            reject.clone(),
            Some(FoldError::RetiredIdentity),
        ),
        (
            "cleaning-reject-twice",
            vec![registration(cartridge), reject.clone()],
            reject.clone(),
            Some(FoldError::RetiredIdentity),
        ),
        (
            "cleaning-select-after-reject",
            vec![registration(cartridge), reject],
            action(CleaningAction::Begin(Begin {
                trigger: CleanTrigger::Manual,
            })),
            Some(FoldError::RetiredIdentity),
        ),
        (
            "cleaning-register-over-data",
            vec![],
            registration(tape(1)),
            Some(FoldError::ConflictingDuplicate),
        ),
        (
            "data-record-names-cleaning",
            vec![registration(cartridge)],
            assign,
            Some(FoldError::WrongMediumKind),
        ),
    ]
}

/// Ruling 17's new layouts each have an admitted and a refused real-log case.
fn ruling17_cases() -> Vec<(&'static str, Vec<Body>, Body, Option<FoldError>)> {
    let ready = ReadinessObserved {
        source_slot: None,
        run: None,
        cancel_source: None,
        operation: [77; 16],
        endpoint: endpoint("R17"),
        tape: None,
        barcode: None,
        phase: ReadinessPhase::StartupReconcile,
        state: ReadinessState::AbortedUnknown,
        previous: None,
        started_unix_ns: NOW,
        limit_ms: Some(1000),
        evidence: None,
        load_intent: None,
        ready_generation: 1,
        result: ReadinessResult::AbortedUnknown,
        scope: ReadinessScope::Library,
        media: None,
    };
    let mut standalone = ready.clone();
    standalone.endpoint = DriveKey::Standalone(Standalone {
        drive_serial: "R17".into(),
    });
    let mut media = ready.clone();
    media.scope = ReadinessScope::Endpoint;
    media.media = Some(MediumFamily::Lto(LtoGen::Lto9));
    let mut bad_media = media.clone();
    bad_media.tape = Some(tape(1));
    let raise = Body::AlarmChanged(AlarmChanged {
        condition_key: "r17".into(),
        change: GenerationChange {
            expected: 0,
            next: 1,
        },
        action: AlarmAction::Raise(AlarmRaise {
            kind: AlarmKind::CleaningNeedsOperator,
            severity: Severity::Warning,
            reason: "first".into(),
        }),
        cause: None,
    });
    let refresh = Body::AlarmChanged(AlarmChanged {
        condition_key: "r17".into(),
        change: GenerationChange {
            expected: 1,
            next: 2,
        },
        action: AlarmAction::Refresh(AlarmRefresh {
            severity: Severity::Critical,
            reason: "updated".into(),
            cause: Some(record(provision(1, Some("A")), 1).reference(1)),
        }),
        cause: None,
    });
    let mut absent = refresh.clone();
    if let Body::AlarmChanged(v) = &mut absent {
        v.change = GenerationChange {
            expected: 0,
            next: 1,
        };
    }
    let cartridge = cleaning_cartridge_ref(&"CLNR17".into());
    let register = Body::CleaningRecorded(CleaningRecorded {
        run_id: "register-r17".into(),
        previous: None,
        drive_uuid: None,
        cartridge: Some(cartridge),
        action: CleaningAction::Register(Register {
            barcode: "CLNR17".into(),
            home_slot: 23,
        }),
    });
    let begin = Body::CleaningRecorded(CleaningRecorded {
        run_id: "run-r17".into(),
        previous: None,
        drive_uuid: None,
        cartridge: None,
        action: CleaningAction::Begin(Begin {
            trigger: CleanTrigger::Manual,
        }),
    });
    let moving_in = Body::CleaningRecorded(CleaningRecorded {
        run_id: "run-r17".into(),
        previous: Some(record(begin.clone(), 4).reference(4)),
        drive_uuid: None,
        cartridge: Some(cartridge),
        action: CleaningAction::Advance(Advance {
            phase: CleanPhase::MovingIn,
            home_slot: Some(42),
        }),
    });
    let mut missing_slot = moving_in.clone();
    if let Body::CleaningRecorded(v) = &mut missing_slot {
        v.action = CleaningAction::Advance(Advance {
            phase: CleanPhase::MovingIn,
            home_slot: None,
        });
    }
    let mut missing_cartridge = moving_in.clone();
    if let Body::CleaningRecorded(v) = &mut missing_cartridge {
        v.cartridge = None;
    }
    let mut wrong_phase = moving_in.clone();
    if let Body::CleaningRecorded(v) = &mut wrong_phase {
        v.action = CleaningAction::Advance(Advance {
            phase: CleanPhase::MovingBack,
            home_slot: Some(42),
        });
    }
    let failed_unselected = Body::CleaningRecorded(CleaningRecorded {
        run_id: "run-r17".into(),
        previous: Some(record(begin.clone(), 3).reference(3)),
        drive_uuid: None,
        cartridge: None,
        action: CleaningAction::Finish(Finish {
            result: CleanResult::Failed,
            verified_credit: false,
        }),
    });
    let mut data = provision(3, Some("CLNR17"));
    if let Body::Reprovision(v) = &mut data {
        v.tape = cartridge;
    }
    let retire = Body::Retire(Retire {
        tape: cartridge,
        reason: RetireReason::OperatorRequested,
        observed_tape_sequence: None,
        assignment_generation: 1,
        detail: Some("reclassified as cleaning by prefix".into()),
    });
    vec![
        (
            "readiness-library-scope",
            vec![],
            Body::ReadinessObserved(ready),
            None,
        ),
        (
            "readiness-library-scope-standalone",
            vec![],
            Body::ReadinessObserved(standalone),
            Some(FoldError::InvalidSchema),
        ),
        (
            "readiness-unprovisioned-family",
            vec![],
            Body::ReadinessObserved(media),
            None,
        ),
        (
            "readiness-family-with-tape",
            vec![],
            Body::ReadinessObserved(bad_media),
            Some(FoldError::InvalidSchema),
        ),
        ("alarm-refresh", vec![raise], refresh, None),
        (
            "alarm-refresh-absent",
            vec![],
            absent,
            Some(FoldError::AlarmState),
        ),
        (
            "cleaning-run-home",
            vec![register.clone(), begin.clone()],
            moving_in.clone(),
            None,
        ),
        (
            "cleaning-run-home-unregistered",
            vec![begin.clone()],
            {
                let mut absent = moving_in;
                if let Body::CleaningRecorded(v) = &mut absent {
                    v.previous = Some(record(begin.clone(), 3).reference(3));
                }
                absent
            },
            Some(FoldError::UnknownTape),
        ),
        (
            "cleaning-moving-in-missing-slot",
            vec![register.clone(), begin.clone()],
            missing_slot,
            Some(FoldError::InvalidSchema),
        ),
        (
            "cleaning-moving-in-missing-cartridge",
            vec![register.clone(), begin.clone()],
            missing_cartridge,
            Some(FoldError::InvalidSchema),
        ),
        (
            "cleaning-slot-outside-moving-in",
            vec![register.clone(), begin.clone()],
            wrong_phase,
            Some(FoldError::InvalidSchema),
        ),
        (
            "cleaning-failed-before-selection",
            vec![begin],
            failed_unselected,
            None,
        ),
        (
            "cleaning-retired-data-registration",
            vec![data.clone(), retire],
            register.clone(),
            None,
        ),
        (
            "cleaning-live-data-registration",
            vec![data],
            register,
            Some(FoldError::ConflictingDuplicate),
        ),
    ]
}

/// Ruling 2 permits inventory-confirmed LOAD when no readiness flow follows.
/// Confirmation observes without resolving; resolving first consumes the intent.
fn load_inventory_cases() -> Vec<(&'static str, Vec<Body>, Body, Option<FoldError>)> {
    let intent_ref = DecisionRef {
        sequence: 3,
        record_uuid: Ids.next(3),
    };
    let observation = |resolves| {
        Body::InventoryReconciled(InventoryReconciled {
            subject: InventorySubject::Library("LIBA0001".into()),
            observation_generation: 1,
            resolves: Bounded(resolves),
            complete: false,
            elements: Bounded(vec![Element {
                address: 1,
                kind: ElementKind::Drive,
                barcode: None,
                full: true,
            }]),
        })
    };
    let done = Body::MotionOutcome(MotionOutcome {
        intent: intent_ref,
        result: MotionResult::Done,
        evidence: evidence(),
        confirmation: Some(Confirmation::Inventory(4)),
    });
    vec![
        (
            "load-inventory-confirmed",
            vec![intent(1), observation(vec![])],
            done.clone(),
            None,
        ),
        (
            "load-inventory-resolved-before-outcome",
            vec![intent(1), observation(vec![intent_ref])],
            done,
            Some(FoldError::OutcomeWithoutOpenIntent),
        ),
    ]
}

/// Standalone reconciliation has its own generation and endpoint scope; neither
/// another drive nor a library with the same serial may resolve its motion.
fn standalone_inventory_cases() -> Vec<(&'static str, Vec<Body>, Body, Option<FoldError>)> {
    let endpoint = Standalone {
        drive_serial: "SAME".into(),
    };
    let subject = InventorySubject::Standalone(endpoint.clone());
    let intent_ref = DecisionRef {
        sequence: 3,
        record_uuid: Ids.next(3),
    };
    let load = Body::MotionIntent(MotionIntent {
        command: [0x79; 16],
        parent_operation: None,
        motion: Motion::Load(Load {
            endpoint: DriveKey::Standalone(endpoint),
            attempt: 1,
        }),
        tape: None,
        inventory_generation: None,
        readiness_generation: None,
    });
    let observe = |subject, generation, resolves| {
        Body::InventoryReconciled(InventoryReconciled {
            subject,
            observation_generation: generation,
            resolves: Bounded(resolves),
            complete: true,
            elements: Bounded(vec![]),
        })
    };
    let unknown = Body::MotionOutcome(MotionOutcome {
        intent: intent_ref,
        result: MotionResult::Unknown,
        evidence: evidence(),
        confirmation: None,
    });
    let done = Body::MotionOutcome(MotionOutcome {
        intent: intent_ref,
        result: MotionResult::Done,
        evidence: evidence(),
        confirmation: Some(Confirmation::Inventory(4)),
    });
    let observation = observe(subject.clone(), 1, vec![]);
    let resolved = observe(subject.clone(), 1, vec![intent_ref]);
    let mut partial = observation.clone();
    if let Body::InventoryReconciled(v) = &mut partial {
        v.complete = false;
    }
    let mut elements = observation.clone();
    if let Body::InventoryReconciled(v) = &mut elements {
        v.elements = Bounded(vec![Element {
            address: 1,
            kind: ElementKind::Drive,
            barcode: None,
            full: false,
        }]);
    }
    vec![
        (
            "standalone-resolve-open",
            vec![load.clone()],
            resolved.clone(),
            None,
        ),
        (
            "standalone-resolve-unknown",
            vec![load.clone(), unknown],
            resolved.clone(),
            None,
        ),
        (
            "standalone-load-inventory-confirmed",
            vec![load.clone(), observation.clone()],
            done.clone(),
            None,
        ),
        (
            "standalone-wrong-endpoint",
            vec![load.clone()],
            observe(
                InventorySubject::Standalone(Standalone {
                    drive_serial: "OTHER".into(),
                }),
                1,
                vec![intent_ref],
            ),
            Some(FoldError::InventoryScope),
        ),
        (
            "standalone-library-serial-alias",
            vec![load.clone()],
            observe(
                InventorySubject::Library("SAME".into()),
                1,
                vec![intent_ref],
            ),
            Some(FoldError::InventoryScope),
        ),
        (
            "standalone-stale-generation",
            vec![observation.clone()],
            observe(subject.clone(), 1, vec![]),
            Some(FoldError::InventoryGeneration),
        ),
        (
            "standalone-independent-generation",
            vec![observe(InventorySubject::Library("SAME".into()), 5, vec![])],
            observation,
            None,
        ),
        (
            "standalone-partial-observation",
            vec![],
            partial,
            Some(FoldError::InvalidSchema),
        ),
        (
            "standalone-changer-elements",
            vec![],
            elements,
            Some(FoldError::InvalidSchema),
        ),
        (
            "standalone-resolved-before-outcome",
            vec![load, resolved],
            done,
            Some(FoldError::OutcomeWithoutOpenIntent),
        ),
    ]
}

/// Positive historical references are store-written; negatives append a valid-CRC
/// frame only after checking that live preflight refuses the exact same error.
fn empty_prefix_cases() -> Vec<(&'static str, Vec<Body>, Body, Option<FoldError>)> {
    let fresh = Body::SupersedeTail(SupersedeTail {
        at: at(1, 0),
        operation: None,
        session_id: None,
        preview_digest: None,
        tail_file_count: 0,
    });
    let Body::VerificationVerdict(mut zero) = verdict() else {
        unreachable!()
    };
    zero.at = Some(at(1, 0));
    zero.tape = tape(1);
    vec![
        ("fresh-tape-supersede", vec![], fresh.clone(), None),
        ("empty-prefix-after-bootstrap", vec![], fresh.clone(), None),
        ("empty-prefix-header-mismatch", vec![], fresh, None),
        (
            "zero-dependent-verdict",
            vec![],
            Body::VerificationVerdict(zero),
            Some(FoldError::InvalidSchema),
        ),
    ]
}

fn historical_vectors(root: &Path) {
    for (name, prefix, candidate, expected) in historical_cases()
        .into_iter()
        .chain(health_cases())
        .chain(cleaning_identity_cases())
        .chain(ruling17_cases())
        .chain(load_inventory_cases())
        .chain(standalone_inventory_cases())
        .chain(empty_prefix_cases())
    {
        let dir = if expected.is_some() {
            root.join("rejected").join(name)
        } else {
            root.join(name)
        };
        let (mut log, _) = open(&dir, &Present);
        body(&mut log, provision(1, Some("A")));
        body(&mut log, provision(2, Some("B")));
        for b in prefix {
            body(&mut log, b);
        }
        if let Some(expected) = expected {
            assert!(
                matches!(log.append(candidate.clone(), AuditActor::System, SourceLayer::Layer4, &Present), Err(StoreError::Fold(e)) if e == expected),
                "{name}"
            );
            let n = log.state().last_sequence + 1;
            let (bytes, _) =
                codec::encode(n, log.state().terminal_hash, &record(candidate, n)).unwrap();
            let path = dir.join(&log.state().segments.last().unwrap().name);
            drop(log);
            fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(&bytes)
                .unwrap();
            assert_eq!(
                replay(&dir, LOG, &Present).unwrap().refusal.unwrap().fault,
                RefusalKind::Fold(expected),
                "{name}"
            );
        } else {
            body(&mut log, candidate.clone());
            if name == "health-identical-latest" {
                // Append preflight writes nothing. Encode an external duplicate to
                // freeze replay's identical-body no-op with a fresh envelope UUID.
                let n = log.state().last_sequence + 1;
                let (bytes, _) =
                    codec::encode(n, log.state().terminal_hash, &record(candidate, n)).unwrap();
                let path = dir.join(&log.state().segments.last().unwrap().name);
                drop(log);
                fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .unwrap()
                    .write_all(&bytes)
                    .unwrap();
                let replayed = replay(&dir, LOG, &Present).unwrap();
                assert!(replayed.refusal.is_none());
                assert_eq!(replayed.fold.duplicates, 1);
            } else {
                drop(log);
                assert!(
                    replay(&dir, LOG, &Present).unwrap().refusal.is_none(),
                    "{name}"
                );
            }
        }
    }
}

/// Freeze the absent-history interface case separately from contiguous store logs:
/// a validated on-disk prefix cannot have an absent earlier sequence.
fn absent_fence_vector() -> Vec<u8> {
    let prefix = record(diagnostic(), 1);
    let candidate = record(release(1), 2);
    let (prefix_bytes, hash) = codec::encode(1, [0; 32], &prefix).unwrap();
    let (candidate_bytes, _) = codec::encode(2, hash, &candidate).unwrap();
    let mut fold = LibraryFold::default();
    fold.apply(
        1,
        prefix.clone(),
        receipt(&prefix, 1),
        &Present,
        &History::default(),
    )
    .unwrap();
    assert_eq!(
        fold.validate(&candidate, &Present, &History::default())
            .unwrap_err(),
        FoldError::ReleaseWithoutFence
    );
    let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "case": "release-missing-fence",
        "contract": "Fold interface only: apply prefix; supply an empty LibraryHistory. This is not a store replay vector; contiguous store history includes the diagnostic and returns WrongCounterpart.",
        "history": [], "prefix_frame_hex": hex(&prefix_bytes),
        "candidate_frame_hex": hex(&candidate_bytes), "expected": "ReleaseWithoutFence",
        "additional_vectors": contradiction_fold_vectors()
    })).unwrap();
    bytes.push(b'\n');
    bytes
}

/// Writer-valid histories whose replay observes lagging or repaired native authority.
fn review_replay_vectors(root: &Path) {
    let dir = root.join("queued-successor-fence");
    let (mut log, _) = open(&dir, &Present);
    body(&mut log, provision(1, None));
    body(
        &mut log,
        Body::SupersedeTail(SupersedeTail {
            at: at(1, 9),
            operation: Some([3; 16]),
            session_id: Some([3; 16]),
            preview_digest: Some([0; 32]),
            tail_file_count: 1,
        }),
    );
    body(
        &mut log,
        Body::Retire(Retire {
            tape: tape(1),
            reason: RetireReason::OperatorRequested,
            observed_tape_sequence: None,
            assignment_generation: 1,
            detail: None,
        }),
    );
    let mut successor = provision(2, None);
    if let Body::Reprovision(v) = &mut successor {
        v.predecessor = Some(tape(1));
    }
    body(&mut log, successor);
    body(&mut log, fence(2));
    drop(log);
    let mut r = replay(
        &dir,
        LOG,
        corpus_view(Path::new("queued-successor-fence")).as_ref(),
    )
    .unwrap();
    assert!(r.refusal.is_none());
    assert_eq!(r.fold.open_fences(), 1);
    assert_eq!(r.fold.pending.len(), 4);
    r.fold.resolve(&Present, &History::default()).unwrap();
    assert!(r.fold.pending.is_empty());
    assert!(r.fold.contradicted_tapes() == 0);
    let dir = root.join("repaired-kind-scoped");
    let (mut log, _) = open(&dir, &Present);
    body(&mut log, provision(1, None));
    body(
        &mut log,
        Body::ManualFinalizationAccepted(ManualFinalizationAccepted {
            at: at(1, 4),
            operation: [3; 16],
            actor_fingerprint: "operator".into(),
            idempotency_key: [4; 16],
            request_fingerprint: [6; 32],
            assignment_generation: 1,
        }),
    );
    body(&mut log, provision(2, None));
    body(&mut log, pool(2, 1));
    drop(log);
    let view = corpus_view(Path::new("repaired-kind-scoped"));
    let r = replay(&dir, LOG, view.as_ref()).unwrap();
    assert!(r.refusal.is_none());
    assert!(r.fold.contradicted(tape(1)));
    assert_eq!(r.fold.tapes[&tape(2).uuid].assignment_generation, 2);
}

/// Freeze a persisted supersede retry after terminal pruning. Live retries write
/// nothing; the explicit duplicate frame also pins replay's lifetime receipt lookup.
fn supersede_after_terminal_vector(dir: &Path) {
    let (mut log, _) = open(dir, &Present);
    body(&mut log, provision(1, None));
    let started = body(
        &mut log,
        Body::OperationRecorded(OperationRecorded {
            operation: [7; 16],
            kind: OperationKind::MoveMedium,
            previous: None,
            event: OperationEvent::Started(Unit),
        }),
    );
    let supersede = Body::SupersedeTail(SupersedeTail {
        at: at(1, 9),
        operation: Some([7; 16]),
        session_id: Some([3; 16]),
        preview_digest: Some([0; 32]),
        tail_file_count: 1,
    });
    let original = body(&mut log, supersede.clone());
    body(
        &mut log,
        Body::OperationRecorded(OperationRecorded {
            operation: [7; 16],
            kind: OperationKind::MoveMedium,
            previous: Some(reference(&started)),
            event: OperationEvent::Finished(Response {
                response_fingerprint: None,
            }),
        }),
    );
    assert_eq!(body(&mut log, supersede.clone()), original);
    assert_eq!(log.state().last_sequence, 4);
    let (bytes, _) =
        codec::encode(5, log.state().terminal_hash, &record(supersede.clone(), 5)).unwrap();
    drop(log);
    fs::OpenOptions::new()
        .append(true)
        .open(segments(dir).remove(0))
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    let r = replay(dir, LOG, &Present).unwrap();
    assert!(r.refusal.is_none());
    assert_eq!(r.last_sequence, 5);
    assert_eq!(r.fold.duplicates, 1);
    assert!(r.fold.supersede(3).is_some());
    assert!(r.fold.supersede(5).is_none());
    let (mut reopened, _) = open(dir, &Present);
    assert_eq!(body(&mut reopened, supersede), original);
    assert_eq!(reopened.state().last_sequence, 5);
}

/// A corpus input: explicit native answers override either echo-Present or NotYet.
#[derive(Clone)]
struct CorpusView {
    present_default: bool,
    entries: BTreeMap<(Id, u64), TapeLookup>,
}
impl TapeView for CorpusView {
    fn lookup(&self, at: &TapeAt) -> Result<TapeLookup, FoldError> {
        Ok(self
            .entries
            .get(&(at.tape.uuid, at.sequence))
            .cloned()
            .unwrap_or_else(|| {
                if self.present_default {
                    Present.lookup(at).unwrap()
                } else {
                    TapeLookup::NotYet
                }
            }))
    }
}

/// The sole table for replay inputs, TSV rows, and README descriptions.
fn view_table() -> BTreeMap<&'static str, CorpusView> {
    let absent = CorpusView {
        present_default: false,
        entries: BTreeMap::new(),
    };
    let repaired = CorpusView {
        present_default: false,
        entries: BTreeMap::from([(
            (tape(1).uuid, 4),
            TapeLookup::Present {
                kind: crate::tape_log::Tag::SessionEnded,
                digest: [0xee; 32],
            },
        )]),
    };
    BTreeMap::from([
        (
            "empty-prefix-header-mismatch",
            CorpusView {
                present_default: true,
                entries: BTreeMap::from([(
                    (tape(1).uuid, 0),
                    TapeLookup::EmptyPrefix { digest: [0xff; 32] },
                )]),
            },
        ),
        (
            "lost-reference-drive-release",
            CorpusView {
                present_default: true,
                entries: BTreeMap::from([((tape(1).uuid, 1), TapeLookup::Contradiction)]),
            },
        ),
        (
            "retire-contradicted-tape",
            CorpusView {
                present_default: true,
                entries: BTreeMap::from([((tape(1).uuid, 1), TapeLookup::Contradiction)]),
            },
        ),
        (
            "excluded-progress-verdict",
            CorpusView {
                present_default: true,
                entries: BTreeMap::from([((tape(1).uuid, 1), TapeLookup::Contradiction)]),
            },
        ),
        (
            "blocked-reference-check-order",
            CorpusView {
                present_default: false,
                entries: BTreeMap::from([((tape(1).uuid, 1), TapeLookup::Contradiction)]),
            },
        ),
        ("notyet-reference-holds-endpoint", absent.clone()),
        ("deferred-fence-unknown-at-resolution", absent.clone()),
        ("rejected/repair-below-pending-reference", absent.clone()),
        (
            "rejected/repair-below-excluded-reference",
            CorpusView {
                present_default: true,
                entries: BTreeMap::from([((tape(1).uuid, 9), TapeLookup::Contradiction)]),
            },
        ),
        ("queued-successor-fence", absent.clone()),
        ("assign-pool-held-by-current-label", absent.clone()),
        ("alarm-held-by-drive-fence-operation", absent.clone()),
        ("rejected/pending-record-limit", absent),
        ("repaired-kind-scoped", repaired.clone()),
        ("contradicted-tape-holds-later-record", repaired),
        (
            "pending-cross-log",
            CorpusView {
                present_default: false,
                entries: real_view().0,
            },
        ),
        (
            "rejected/counterpart-kind",
            CorpusView {
                present_default: true,
                entries: BTreeMap::from([(
                    (tape(1).uuid, 4),
                    TapeLookup::Present {
                        kind: crate::tape_log::Tag::SessionOpened,
                        digest: at(1, 4).record_digest,
                    },
                )]),
            },
        ),
        (
            "fence-behind-contradicted-operation",
            CorpusView {
                present_default: true,
                entries: BTreeMap::from([((tape(1).uuid, 9), TapeLookup::Contradiction)]),
            },
        ),
        (
            "drive-fence-behind-contradicted-operation",
            CorpusView {
                present_default: true,
                entries: BTreeMap::from([((tape(1).uuid, 9), TapeLookup::Contradiction)]),
            },
        ),
    ])
}

/// Select a case's view from the published table, with the common echo default.
fn case_view(case: &Path) -> CorpusView {
    view_table()
        .remove(case.to_str().unwrap())
        .unwrap_or(CorpusView {
            present_default: true,
            entries: BTreeMap::new(),
        })
}
pub(super) fn corpus_view(case: &Path) -> Box<dyn TapeView> {
    Box::new(case_view(case))
}

/// Emit the same default and explicit entries that TapeView::lookup consumes.
fn view_rows(case: &Path, view: &CorpusView) -> String {
    let mut rows = format!(
        "{}\t*\t*\t{}\t{}\t{}\n",
        case.display(),
        if view.present_default {
            "Present"
        } else {
            "NotYet"
        },
        if view.present_default {
            "FinalizationPrepared"
        } else {
            ""
        },
        if view.present_default { "echo" } else { "" }
    );
    for ((tape, sequence), answer) in &view.entries {
        let (answer, kind, digest) = match answer {
            TapeLookup::Present { kind, digest } => ("Present", format!("{kind:?}"), hex(digest)),
            TapeLookup::EmptyPrefix { digest } => ("EmptyPrefix", String::new(), hex(digest)),
            TapeLookup::NotYet => ("NotYet", String::new(), String::new()),
            TapeLookup::Contradiction => ("Contradiction", String::new(), String::new()),
        };
        rows += &format!(
            "{}\t{}\t{sequence}\t{answer}\t{kind}\t{digest}\n",
            case.display(),
            hex(tape)
        );
    }
    rows
}

/// Every golden history and freshly generated B2 fixture uses the same oracle.
/// Check each authoritative record and final store replay, then resolve lagging
/// tape evidence on both sides and compare again. Byte-refused tails are covered
/// by the corpus's independent frame/manifest tests, never treated as records.
#[test]
fn corpus_and_b2_fixtures_live_append_matches_byte_replay() {
    let generated = tempfile::tempdir().unwrap();
    generate(generated.path());
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/library-log");
    let mut histories = 0;
    let mut records = 0;
    for root in [golden.as_path(), generated.path()] {
        let cases: std::collections::BTreeSet<_> = collect(root)
            .keys()
            .map(|p| p.parent().unwrap().to_path_buf())
            .collect();
        assert!(!cases.is_empty());
        for case in cases {
            let view = corpus_view(&case);
            let expected = replay(&root.join(&case), LOG, view.as_ref()).unwrap();
            let mut live = LibraryFold::default();
            let mut decoded = LibraryFold::default();
            let mut history = History::default();
            for path in segments(&root.join(&case)) {
                if live.received_through == expected.last_sequence {
                    break;
                }
                let bytes = fs::read(path).unwrap();
                let mut input = bytes.as_slice();
                let header = codec::Header::decode(&mut input, LOG).unwrap();
                let mut decoder = codec::Decoder::at(input, header.first_sequence, 84);
                let mut previous = header.previous_segment_hash;
                while live.received_through < expected.last_sequence {
                    let Some(frame) = decoder.next_frame().unwrap() else {
                        break;
                    };
                    previous = codec::verify(&frame, previous).unwrap();
                    let receipt = Receipt {
                        sequence: frame.sequence,
                        record_uuid: frame.record.envelope.record_uuid,
                        record_hash: previous,
                        synced: true,
                    };
                    eprintln!(
                        "differential {} sequence {}",
                        case.display(),
                        frame.sequence
                    );
                    super::differential_tests::step(
                        &mut live,
                        &mut decoded,
                        frame.record.clone(),
                        receipt,
                        view.as_ref(),
                        &history,
                    )
                    .unwrap();
                    history.0.insert(frame.sequence, frame.record);
                    records += 1;
                }
            }
            if let Some(Refusal {
                fault: RefusalKind::Fold(error),
                segment,
                offset,
                sequence,
            }) = &expected.refusal
            {
                let bytes = fs::read(root.join(&case).join(segment)).unwrap();
                let n = sequence.expect("semantic refusal has a decoded sequence");
                let mut decoder = codec::Decoder::at(&bytes[*offset as usize..], n, *offset);
                let frame = decoder.next_frame().unwrap().unwrap();
                let hash = codec::verify(&frame, expected.terminal_hash).unwrap();
                let receipt = Receipt {
                    sequence: n,
                    record_uuid: frame.record.envelope.record_uuid,
                    record_hash: hash,
                    synced: true,
                };
                assert_eq!(
                    super::differential_tests::step(
                        &mut live,
                        &mut decoded,
                        frame.record,
                        receipt,
                        view.as_ref(),
                        &history
                    )
                    .unwrap_err(),
                    *error,
                    "{}",
                    case.display()
                );
            }
            live.assert_same_state(&expected.fold);
            decoded.assert_same_state(&expected.fold);
            assert_eq!(
                live.resolve(&Present, &history),
                decoded.resolve(&Present, &history)
            );
            live.assert_same_state(&decoded);
            histories += 1;
        }
    }
    eprintln!("differential checked {histories} histories and {records} authoritative records");
    assert!(histories > 100);
}

/// A standard-library Python decoder pins the changed layouts independently of Rust's field lists.
#[test]
fn reference_library_logs_independent_decoder() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = independent_decoder(&root.join("../../fixtures/library-log"))
        .output()
        .expect("python3 is required for the independent corpus decoder");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    print!("{}", String::from_utf8_lossy(&output.stdout));
}

/// Isolate Python from optimization and import settings supplied by the calling environment.
fn independent_decoder(corpus: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("python3");
    command
        .env_remove("PYTHONOPTIMIZE")
        .arg("-I")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/library_corpus.py"))
        .arg(corpus);
    command
}

/// A corrupt Disk family code must fail even when the interpreter optimizes its bytecode.
#[test]
fn reference_library_logs_independent_decoder_rejects_corrupt_family() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/library-log");
    let d = tempfile::tempdir().unwrap();
    for (path, bytes) in collect(&root) {
        let path = d.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fs::copy(root.join("FRAMES.tsv"), d.path().join("FRAMES.tsv")).unwrap();
    let path = d
        .path()
        .join("all-records/00000000000000000001-2026-11-03.remlib");
    let mut bytes = fs::read(&path).unwrap();
    let positions: Vec<_> = bytes
        .windows(5)
        .enumerate()
        .filter_map(|(i, b)| (b == [0x00, 0x82, 0x01, 0xf6, 0x01]).then_some(i))
        .collect();
    assert_eq!(
        positions.len(),
        1,
        "one Disk family in the all-records vector"
    );
    bytes[positions[0] + 2] = 2;
    fs::write(path, bytes).unwrap();
    let command = independent_decoder(d.path());
    // Explicit -O proves checks cannot disappear even independently of the wrapper hardening.
    let args: Vec<_> = command.get_args().map(|arg| arg.to_owned()).collect();
    let output = std::process::Command::new("python3")
        .env_remove("PYTHONOPTIMIZE")
        .arg("-O")
        .args(args)
        .output()
        .expect("python3 is required for the corruption test");
    assert!(
        !output.status.success(),
        "corrupted family passed the independent decoder"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid medium family"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Refuse a typed candidate live, then encode it to pin the same byte-replay refusal.
fn freeze_rejection(
    dir: &Path,
    prefix: Vec<Body>,
    bad: Body,
    view: &dyn TapeView,
    expected: FoldError,
) {
    let (mut log, _) = open(dir, &Present);
    for b in prefix {
        body(&mut log, b);
    }
    let n = log.state().last_sequence + 1;
    assert!(
        matches!(log.append(bad.clone(), AuditActor::System,SourceLayer::Layer4,view), Err(StoreError::Fold(e)) if e==expected)
    );
    let (bytes, _) = codec::encode(n, log.state().terminal_hash, &record(bad, n)).unwrap();
    drop(log);
    fs::OpenOptions::new()
        .append(true)
        .open(segments(dir).remove(0))
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    let r = replay(dir, LOG, view).unwrap();
    assert_eq!(r.refusal.unwrap().fault, RefusalKind::Fold(expected));
    assert!(r.fold.pending.is_empty());
}

/// The freeze cases close decoder gaps without modifying any existing history.
fn freeze_vectors(root: &Path) {
    let mut label = provision(1, None);
    if let Body::Reprovision(v) = &mut label {
        v.medium.source = GenerationSource::Label;
    }
    let operator = MediumSpec {
        source: GenerationSource::Operator,
        ..medium_spec()
    };
    freeze_rejection(
        &root.join("rejected/medium-measured-by-revision-generation-change"),
        vec![
            label,
            medium_change(medium_spec(), 1),
            medium_change(operator.clone(), 2),
        ],
        medium_change(
            MediumSpec {
                family: MediumFamily::Lto(LtoGen::Lto8),
                ..operator
            },
            3,
        ),
        &Present,
        FoldError::MediumFamilyChanged,
    );
    freeze_rejection(
        &root.join("rejected/motion-command-open"),
        vec![provision(1, None), intent(1)],
        intent(1),
        &Present,
        FoldError::ConflictingDuplicate,
    );
    let case = Path::new("rejected/counterpart-kind");
    freeze_rejection(
        &root.join(case),
        vec![provision(1, None)],
        Body::ManualFinalizationAccepted(ManualFinalizationAccepted {
            at: at(1, 4),
            operation: [3; 16],
            actor_fingerprint: "operator".into(),
            idempotency_key: [4; 16],
            request_fingerprint: [6; 32],
            assignment_generation: 1,
        }),
        corpus_view(case).as_ref(),
        FoldError::CounterpartKind,
    );
    let dir = root.join("contradicted-tape-holds-later-record");
    copy_dir(&root.join("repaired-kind-scoped"), &dir);
    let (mut log, _) = open(&dir, &Present);
    body(&mut log, pool(1, 1));
    drop(log);
    let r = replay(
        &dir,
        LOG,
        corpus_view(Path::new("contradicted-tape-holds-later-record")).as_ref(),
    )
    .unwrap();
    assert!(r.refusal.is_none());
    assert_eq!(
        r.fold
            .pending
            .iter()
            .map(|p| p.sequence)
            .collect::<Vec<_>>(),
        Vec::<u64>::new()
    );
    assert_eq!(r.fold.applied_through, 5);
    let case = Path::new("fence-behind-contradicted-operation");
    let dir = root.join(case);
    let (mut log, _) = open(&dir, &Present);
    for b in freeze_history(false) {
        body(&mut log, b);
    }
    // Append only: the seven pre-review frames keep their exact bytes.
    body(&mut log, release(6));
    drop(log);
    let r = replay(&dir, LOG, corpus_view(case).as_ref()).unwrap();
    assert!(r.refusal.is_none());
    assert_eq!(
        r.fold
            .pending
            .iter()
            .map(|p| p.sequence)
            .collect::<Vec<_>>(),
        Vec::<u64>::new()
    );
    assert_eq!(r.fold.open_fences(), 0);
    assert_eq!(r.fold.tapes[&tape(2).uuid].media_revision, 2);

    let case = Path::new("drive-fence-behind-contradicted-operation");
    let dir = root.join(case);
    let (mut log, _) = open(&dir, &Present);
    // The drive history needs no healthy second tape or media revision.
    let mut history = freeze_history(true);
    history.remove(1);
    history.pop();
    history.push(release(5));
    for b in history {
        body(&mut log, b);
    }
    drop(log);
    let r = replay(&dir, LOG, corpus_view(case).as_ref()).unwrap();
    assert!(r.refusal.is_none());
    assert_eq!(
        r.fold
            .pending
            .iter()
            .map(|p| p.sequence)
            .collect::<Vec<_>>(),
        Vec::<u64>::new()
    );
    assert_eq!(r.fold.open_fences(), 0);

    alarm_fence_reference_vector(root);
    current_label_vector(root);
    for name in ["empty-segment", "segment-name-unparseable"] {
        let dir = root.join("rejected").join(name);
        copy_dir(&root.join("rotation-clock-regression"), &dir);
        let p = segments(&dir).pop().unwrap();
        if name == "empty-segment" {
            let b = fs::read(&p).unwrap();
            fs::write(&p, &b[..84]).unwrap();
        } else {
            let name = p.file_name().unwrap().to_str().unwrap();
            fs::rename(&p, dir.join(&name[1..])).unwrap();
        }
        let refusal = replay(&dir, LOG, &Present).unwrap().refusal.unwrap();
        assert_eq!(refusal.sequence, Some(4));
        assert_eq!(refusal.offset, if name == "empty-segment" { 84 } else { 0 });
        assert_eq!(
            refusal.fault,
            RefusalKind::Bytes(if name == "empty-segment" {
                codec::Fault::EmptySegment
            } else {
                codec::Fault::SegmentName
            })
        );
    }
    let dir = root.join("rejected/frame-too-large");
    let (mut log, _) = open(&dir, &Present);
    body(&mut log, provision(1, None));
    drop(log);
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&1u16.to_le_bytes());
    prefix.extend_from_slice(&(Tag::Diagnostic as u16).to_le_bytes());
    prefix.extend_from_slice(&(crate::log_frame::MAX_FRAME_BODY as u32).to_le_bytes());
    prefix.extend_from_slice(&32u32.to_le_bytes());
    prefix.extend_from_slice(&2u64.to_le_bytes());
    prefix.extend_from_slice(&remanence_crc::crc64_xz(&prefix).to_le_bytes());
    fs::OpenOptions::new()
        .append(true)
        .open(segments(&dir).remove(0))
        .unwrap()
        .write_all(&prefix)
        .unwrap();
    let refusal = replay(&dir, LOG, &Present).unwrap().refusal.unwrap();
    assert_eq!(refusal.sequence, Some(2));
    assert_eq!(
        refusal.fault,
        RefusalKind::Bytes(codec::Fault::Frame(crate::log_frame::Fault::FrameTooLarge))
    );
    let short = root.join("rejected/frame-too-large-short-prefix");
    copy_dir(&dir, &short);
    let path = segments(&short).remove(0);
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(refusal.offset + 12)
        .unwrap();
    let r = replay(&short, LOG, &Present).unwrap();
    let short_refusal = r.refusal.unwrap();
    assert_eq!(short_refusal.offset, refusal.offset);
    assert_eq!(short_refusal.sequence, None);
    assert_eq!(short_refusal.fault, refusal.fault);
    assert_eq!(
        r.segments.last().unwrap().tail,
        crate::log_frame::Tail::Complete
    );
}

/// A drive fence applies, but its alarm inherits the fence's pending operation.
/// Persist with native proof, then test replay and real append under the published NotYet view.
fn alarm_fence_reference_vector(root: &Path) {
    let case = Path::new("alarm-held-by-drive-fence-operation");
    let dir = root.join(case);
    let (mut log, _) = open(&dir, &Present);
    body(&mut log, provision(1, None));
    body(
        &mut log,
        Body::OperationRecorded(OperationRecorded {
            operation: [9; 16],
            kind: OperationKind::MoveMedium,
            previous: None,
            event: OperationEvent::Started(Unit),
        }),
    );
    body(
        &mut log,
        Body::SupersedeTail(SupersedeTail {
            at: at(1, 9),
            operation: Some([9; 16]),
            session_id: Some([3; 16]),
            preview_digest: Some([0; 32]),
            tail_file_count: 1,
        }),
    );
    drop(log);
    let view = corpus_view(case);
    let (mut log, _) = open(&dir, view.as_ref());
    let drive = DriveKey::Standalone(Standalone {
        drive_serial: "D1".into(),
    });
    let fence = log
        .append(
            Body::FenceRaised(FenceRaised {
                detail: Bounded(vec![]),
                kind: FenceKind::DriveFault,
                scope: FenceScope::Drive(drive.clone()),
                cause: FenceCause::TransferError,
                evidence: None,
                at: None,
                operation: Some([9; 16]),
            }),
            AuditActor::System,
            SourceLayer::Layer4,
            view.as_ref(),
        )
        .unwrap();
    assert_eq!(fence.sequence, 4);
    assert_eq!(log.state().fold.open_fences(), 1);
    let pending = |f: &LibraryFold| f.pending.iter().map(|p| p.sequence).collect::<Vec<_>>();
    assert_eq!(pending(&log.state().fold), [3]);
    let alarm = Body::AlarmChanged(AlarmChanged {
        condition_key: "drive-fence-alarm".into(),
        change: GenerationChange {
            expected: 0,
            next: 1,
        },
        action: AlarmAction::Raise(AlarmRaise {
            kind: AlarmKind::SnapshotPersistFailing,
            severity: Severity::Warning,
            reason: "drive fence".into(),
        }),
        cause: Some(DecisionRef {
            sequence: fence.sequence,
            record_uuid: fence.record_uuid,
        }),
    });
    let path = segments(&dir).remove(0);
    let before = fs::read(&path).unwrap();
    assert!(matches!(
        log.append(
            alarm.clone(),
            AuditActor::System,
            SourceLayer::Layer4,
            view.as_ref()
        ),
        Err(StoreError::Fold(FoldError::Unresolved))
    ));
    assert_eq!(log.state().last_sequence, 4);
    assert_eq!(pending(&log.state().fold), [3]);
    assert_eq!(fs::read(&path).unwrap(), before);
    // With native proof available the same alarm can be durably published.
    log.append(alarm, AuditActor::System, SourceLayer::Layer4, &Present)
        .unwrap();
    drop(log);
    let r = replay(&dir, LOG, view.as_ref()).unwrap();
    assert!(r.refusal.is_none());
    assert_eq!(r.last_sequence, 5);
    assert_eq!(pending(&r.fold), [3, 5]);
    assert_eq!(r.fold.applied_through, 2);
    assert_eq!(r.fold.open_fences(), 1);
    assert!(r.fold.contradicted_tapes() == 0);
    assert_eq!(
        r.fold.pending.last().unwrap().scope,
        std::collections::BTreeSet::from([
            fold::Scope::Endpoint(drive),
            fold::Scope::Operation([9; 16]),
            fold::Scope::Alarm("drive-fence-alarm".into()),
        ])
    );
}

/// Replay-only history: the queued successor claims B, already held by T6.
/// Its eventual label conflict is irrelevant until its predecessor queue drains.
fn current_label_vector(root: &Path) {
    let case = Path::new("assign-pool-held-by-current-label");
    let dir = root.join(case);
    let (mut log, _) = open(&dir, &Present);
    body(&mut log, provision(6, Some("B")));
    body(&mut log, provision(1, None));
    let mut previous = log.state().terminal_hash;
    drop(log);
    let mut successor = provision(5, Some("B"));
    if let Body::Reprovision(v) = &mut successor {
        v.predecessor = Some(tape(1));
    }
    let records = [
        Body::SupersedeTail(SupersedeTail {
            at: at(1, 9),
            operation: Some([7; 16]),
            session_id: Some([3; 16]),
            preview_digest: Some([0; 32]),
            tail_file_count: 1,
        }),
        Body::Retire(Retire {
            tape: tape(1),
            reason: RetireReason::OperatorRequested,
            observed_tape_sequence: None,
            assignment_generation: 1,
            detail: None,
        }),
        successor,
        pool(6, 1),
    ];
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(segments(&dir).remove(0))
        .unwrap();
    for (i, b) in records.into_iter().enumerate() {
        let n = i as u64 + 3;
        let (bytes, hash) = codec::encode(n, previous, &record(b, n)).unwrap();
        file.write_all(&bytes).unwrap();
        previous = hash;
    }
    drop(file);
    let r = replay(&dir, LOG, corpus_view(case).as_ref()).unwrap();
    assert!(r.refusal.is_none());
    assert_eq!(
        r.fold
            .pending
            .iter()
            .map(|p| p.sequence)
            .collect::<Vec<_>>(),
        [3, 4, 5, 6]
    );
    assert_eq!(r.fold.tapes[&tape(6).uuid].assignment_generation, 1);
    assert_eq!(
        r.fold.pending.last().unwrap().scope,
        std::collections::BTreeSet::from([
            fold::Scope::Tape(tape(6).uuid),
            fold::Scope::Label("B".into())
        ])
    );
}

/// D1/D4: replay never resolves; a NotYet predecessor also keeps its fence check queued.
#[test]
fn replay_never_retries_pending() {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/library-log/pending-cross-log");
    let view = corpus_view(Path::new("pending-cross-log"));
    let mut r = replay(&root, LOG, view.as_ref()).unwrap();
    assert!(r.refusal.is_none());
    let sequences =
        |fold: &LibraryFold| fold.pending.iter().map(|p| p.sequence).collect::<Vec<_>>();
    eprintln!(
        "pending-cross-log after store::replay: {:?}",
        sequences(&r.fold)
    );
    assert_eq!(sequences(&r.fold), [11, 12, 13]);
    r.fold.resolve(view.as_ref(), &History::default()).unwrap();
    eprintln!("pending-cross-log after resolve: {:?}", sequences(&r.fold));
    assert_eq!(sequences(&r.fold), [11, 12, 13]);
    assert_eq!(r.fold.open_fences(), 1);
}

/// Publish identities from the authoritative fold and its decoded fence transitions.
/// Raises take effect even when queued; pending releases have not taken effect.
/// This keeps the fixture generator out of the fold's private retention indexes.
fn fold_identities(dir: &Path, replay: &Replay) -> (String, String, String) {
    let pending: std::collections::BTreeSet<_> =
        replay.fold.pending.iter().map(|p| p.sequence).collect();
    let mut fences = std::collections::BTreeSet::new();
    for path in segments(dir) {
        let bytes = fs::read(path).unwrap();
        let mut input = bytes.as_slice();
        let Ok(header) = codec::Header::decode(&mut input, LOG) else {
            break;
        };
        if header.first_sequence > replay.last_sequence {
            break;
        }
        let mut decoder = codec::Decoder::at(input, header.first_sequence, 84);
        while let Ok(Some(frame)) = decoder.next_frame() {
            if frame.sequence > replay.last_sequence {
                break;
            }
            match frame.record.body {
                Body::FenceRaised(_) => {
                    fences.insert(frame.sequence);
                }
                Body::FenceReleased(release) if !pending.contains(&frame.sequence) => {
                    fences.remove(&release.fence);
                }
                _ => {}
            }
        }
    }
    assert_eq!(fences.len(), replay.fold.open_fences());
    // Probe membership through validation as well as counting: strip only blockers
    // on a disposable clone so a pending/contradicted target cannot mask a wrong ID.
    // No transition is applied to this clone or to the authoritative fold.
    let mut membership = replay.fold.clone();
    membership.pending.clear();
    membership.contradictions.clear();
    for id in &fences {
        assert!(
            matches!(
                membership.validate(
                    &record(release(*id), replay.last_sequence + 1),
                    &Present,
                    &History::default(),
                ),
                Ok(fold::Validated::Apply)
            ),
            "published fence {id} is not open in the authoritative fold"
        );
    }
    let join = |values: std::collections::BTreeSet<u64>| {
        values
            .into_iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    let contradictions = replay
        .fold
        .contradictions
        .iter()
        .filter(|(_, summary)| summary.contradicted())
        .map(|(scope, _)| {
            let fold::Scope::Tape(tape) = scope else {
                panic!("non-tape contradiction in corpus: {scope:?}")
            };
            format!("{}:TapeContradiction", hex(tape))
        })
        .collect::<Vec<_>>()
        .join(",");
    (join(pending), join(fences), contradictions)
}

/// Published rows encode exactly the default and overrides used for each case's replay.
#[test]
fn views_table_drives_replay_and_publication() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/library-log");
    let published = fs::read_to_string(root.join("VIEWS.tsv")).unwrap();
    let manifest = fs::read_to_string(root.join("MANIFEST.tsv")).unwrap();
    let mut expected = String::from("case\ttape_uuid\tsequence\tanswer\tkind\tdigest\n");
    let mut count = 0;
    for row in manifest.lines().skip(1) {
        let case = Path::new(row.split('\t').next().unwrap());
        let view = case_view(case);
        expected += &view_rows(case, &view);
        let replay_view = corpus_view(case);
        for ((uuid, sequence), answer) in &view.entries {
            let reference = TapeAt {
                tape: TapeRef {
                    uuid: *uuid,
                    incarnation: 1,
                },
                sequence: *sequence,
                record_digest: [0x44; 32],
            };
            assert_eq!(replay_view.lookup(&reference).unwrap(), *answer);
        }
        let probe = at(0xfa, 999);
        assert_eq!(
            replay_view.lookup(&probe).unwrap(),
            if view.present_default {
                Present.lookup(&probe).unwrap()
            } else {
                TapeLookup::NotYet
            }
        );
        count += 1;
    }
    assert_eq!(published, expected);
    assert_eq!(
        published.lines().filter(|r| r.contains("\t*\t*\t")).count(),
        count
    );
    assert!(count >= 102);
}

/// D1-D8: persist writer-valid histories, then replay against the published native views.
fn contradiction_vectors(root: &Path) {
    let motion_history = || {
        vec![
            provision(1, None),
            verdict(),
            intent(1),
            Body::FenceRaised(FenceRaised {
                detail: Bounded(vec![]),
                kind: FenceKind::DriveFault,
                scope: FenceScope::Drive(endpoint("LIBA0001")),
                cause: FenceCause::TransferError,
                evidence: None,
                at: None,
                operation: None,
            }),
            release(4),
        ]
    };
    let mut vv = verdict();
    if let Body::VerificationVerdict(v) = &mut vv {
        v.progress = Some(record(progress(1), 2).reference(2));
    }
    let supersede = |tape_id, sequence, operation| {
        Body::SupersedeTail(SupersedeTail {
            at: at(tape_id, sequence),
            operation: Some([operation; 16]),
            session_id: Some([sequence as u8; 16]),
            preview_digest: Some([0; 32]),
            tail_file_count: 1,
        })
    };
    for (name, bodies) in [
        ("lost-reference-drive-release", motion_history()),
        ("notyet-reference-holds-endpoint", motion_history()),
        (
            "excluded-progress-verdict",
            vec![provision(1, None), progress(1), vv],
        ),
        (
            "supersede-after-repair-above-reference",
            vec![provision(1, None), b4b0_repair(8, 8), supersede(1, 7, 3)],
        ),
        (
            "blocked-reference-check-order",
            vec![
                provision(1, None),
                provision(2, None),
                supersede(2, 30, 7),
                progress(1),
            ],
        ),
        (
            "retire-contradicted-tape",
            vec![
                provision(1, None),
                verdict(),
                Body::Retire(Retire {
                    tape: tape(1),
                    reason: RetireReason::OperatorRequested,
                    observed_tape_sequence: None,
                    assignment_generation: 1,
                    detail: None,
                }),
            ],
        ),
        // The persisted prefix pins the queued check. FOLD-VECTORS describes the
        // injected identity loss used to exercise UnknownTape at resolution.
        (
            "deferred-fence-unknown-at-resolution",
            vec![provision(1, None), verdict(), fence(1)],
        ),
    ] {
        let dir = root.join(name);
        let (mut log, _) = open(&dir, &Present);
        for b in bodies {
            body(&mut log, b);
        }
        drop(log);
        let replay = replay(&dir, LOG, &case_view(Path::new(name))).unwrap();
        assert!(replay.refusal.is_none(), "{name}: {:?}", replay.refusal);
        let expected = match name {
            "notyet-reference-holds-endpoint" => (vec![2, 3, 5], 1, 0, 1),
            "blocked-reference-check-order" => (vec![3], 0, 1, 2),
            "deferred-fence-unknown-at-resolution" => (vec![2, 3], 1, 0, 1),
            "lost-reference-drive-release" => (vec![], 0, 1, 5),
            "excluded-progress-verdict" | "retire-contradicted-tape" => (vec![], 0, 1, 3),
            _ => (vec![], 0, 0, 3),
        };
        assert_eq!(
            (
                replay
                    .fold
                    .pending
                    .iter()
                    .map(|p| p.sequence)
                    .collect::<Vec<_>>(),
                replay.fold.open_fences(),
                replay.fold.contradicted_tapes(),
                replay.fold.applied_through
            ),
            expected,
            "{name}"
        );
    }
    for (name, prefix, candidate, expected) in [
        (
            "repair-below-applied-reference",
            vec![provision(1, None), supersede(1, 9, 7)],
            b4b0_repair(8, 8),
            FoldError::RepairBelowReference { highest: 9 },
        ),
        (
            "repair-below-pending-reference",
            vec![provision(1, None), supersede(1, 9, 7)],
            b4b0_repair(8, 8),
            FoldError::RepairBelowReference { highest: 9 },
        ),
        (
            "repair-below-excluded-reference",
            vec![provision(1, None), supersede(1, 9, 7)],
            b4b0_repair(8, 8),
            FoldError::RepairBelowReference { highest: 9 },
        ),
        (
            "second-repair-below-first",
            vec![provision(1, None), b4b0_repair(9, 9)],
            b4b0_repair(8, 8),
            FoldError::RepairBelowReference { highest: 9 },
        ),
        (
            "malformed-repair",
            vec![provision(1, None)],
            b4b0_repair(8, 7),
            FoldError::InvalidSchema,
        ),
    ] {
        let case = PathBuf::from("rejected").join(name);
        let dir = root.join(&case);
        let (mut log, _) = open(&dir, &Present);
        for b in prefix {
            body(&mut log, b);
        }
        assert!(
            matches!(log.append(candidate.clone(), AuditActor::System, SourceLayer::Layer4, &Present), Err(StoreError::Fold(e)) if e == expected)
        );
        let n = log.state().last_sequence + 1;
        let (bytes, _) =
            codec::encode(n, log.state().terminal_hash, &record(candidate, n)).unwrap();
        drop(log);
        fs::OpenOptions::new()
            .append(true)
            .open(segments(&dir).remove(0))
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        let replay = replay(&dir, LOG, &case_view(&case)).unwrap();
        assert_eq!(
            replay.refusal.unwrap().fault,
            RefusalKind::Fold(expected),
            "{name}"
        );
        assert_eq!(replay.last_sequence, n - 1);
    }
}

/// D4 property: every writer-valid positive history agrees with replay under a growing view.
/// Each dependent reference is present and matching at append; fence evidence may be NotYet.
/// The seed varies when unrelated immutable proofs appear.
/// Native NotYet/contradiction corpus cases remain in the separate replay/bootstrap oracle.
#[test]
fn b4b0_d4_positive_corpus_growing_views_property() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/library-log");
    let cases: std::collections::BTreeSet<_> = collect(&root)
        .keys()
        .map(|p| p.parent().unwrap().to_path_buf())
        .collect();
    for seed in [0x4b4b0001_u64, 0x91e10da5, 0xc0ffee, 0x5eed, 0xf00d] {
        eprintln!("D4 growing-view property seed={seed:#x}");
        let mut random = seed;
        let mut absent_fence_evidence = 0;
        for case in &cases {
            if case.starts_with("rejected") {
                continue;
            }
            // The growing-view property uses original writer evidence, including
            // histories whose published restart view deliberately loses a reference.
            let expected = replay(&root.join(case), LOG, &Present).unwrap();
            if expected.refusal.is_some() {
                continue;
            } // byte-fault corpus is a different property
            let mut frames = Vec::new();
            for path in segments(&root.join(case)) {
                let bytes = fs::read(path).unwrap();
                let mut input = bytes.as_slice();
                let header = codec::Header::decode(&mut input, LOG).unwrap();
                let mut decoder = codec::Decoder::at(input, header.first_sequence, 84);
                let mut hash = header.previous_segment_hash;
                while let Ok(Some(frame)) = decoder.next_frame() {
                    if frame.sequence > expected.last_sequence {
                        break;
                    }
                    hash = codec::verify(&frame, hash).unwrap();
                    let receipt = Receipt {
                        sequence: frame.sequence,
                        record_uuid: frame.record.envelope.record_uuid,
                        record_hash: hash,
                        synced: true,
                    };
                    frames.push((frame.record, receipt));
                }
            }
            let mut references = BTreeMap::new();
            let mut consistent = true;
            for (r, _) in &frames {
                if let Some(at) = r.body.at() {
                    if references
                        .insert((at.tape.uuid, at.sequence), at.record_digest)
                        .is_some_and(|old| old != at.record_digest)
                    {
                        consistent = false;
                    }
                }
            }
            if !consistent {
                eprintln!("D4 seed={seed:#x} case={} requires changed native bytes; outside grow-only premise", case.display());
                assert_eq!(case.to_str().unwrap(), "pending-cross-log");
                continue;
            }
            let mut live = LibraryFold::default();
            let mut view = View::default();
            let mut history = History::default();
            for (record, receipt) in &frames {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                // Add extra immutable evidence at a seed-selected point. Nothing is removed.
                if random & 1 == 0 {
                    for (future, _) in &frames {
                        if let Some(at) = future.body.at() {
                            view.0
                                .entry((at.tape.uuid, at.sequence))
                                .or_insert_with(|| Present.lookup(at).unwrap());
                        }
                    }
                }
                if let Some(at) = record.body.at() {
                    view.0
                        .entry((at.tape.uuid, at.sequence))
                        .or_insert_with(|| Present.lookup(at).unwrap());
                }
                // Fence evidence is not a dependency: do not force it to be present.
                if let Body::FenceRaised(v) = &record.body {
                    if let Some(at) = &v.at {
                        if matches!(view.lookup(at).unwrap(), TapeLookup::NotYet) {
                            absent_fence_evidence += 1;
                        }
                    }
                }
                live.resolve(&view, &history).unwrap();
                live.apply(
                    receipt.sequence,
                    record.clone(),
                    receipt.clone(),
                    &view,
                    &history,
                )
                .unwrap_or_else(|e| {
                    panic!(
                        "seed={seed:#x} case={} sequence={}: {e:?}",
                        case.display(),
                        receipt.sequence
                    )
                });
                history.0.insert(receipt.sequence, record.clone());
            }
            let mut replayed = LibraryFold::default();
            let mut prefix = History::default();
            for (record, receipt) in frames {
                replayed
                    .receive(
                        receipt.sequence,
                        record.clone(),
                        receipt.clone(),
                        &view,
                        &prefix,
                    )
                    .unwrap();
                prefix.0.insert(receipt.sequence, record);
            }
            live.assert_same_state(&replayed);
            assert!(
                live.pending.is_empty(),
                "seed={seed:#x} case={}",
                case.display()
            );
        }
        assert!(absent_fence_evidence > 0, "seed={seed:#x}");
    }
}

/// D2/D4/D5/D6 fold-interface vectors include their native inputs and injection boundaries.
fn contradiction_fold_vectors() -> Vec<serde_json::Value> {
    let encode = |bodies: Vec<Body>| {
        let mut hash = [0; 32];
        bodies
            .into_iter()
            .enumerate()
            .map(|(i, b)| {
                let n = i as u64 + 1;
                let (bytes, next) = codec::encode(n, hash, &record(b, n)).unwrap();
                hash = next;
                hex(&bytes)
            })
            .collect::<Vec<_>>()
    };
    let mut vv = verdict();
    if let Body::VerificationVerdict(v) = &mut vv {
        v.progress = Some(record(progress(1), 2).reference(2));
    }
    let mut high = verdict();
    if let Body::VerificationVerdict(v) = &mut high {
        v.at = Some(at(1, 9));
        v.tape = tape(1);
    }
    vec![
        serde_json::json!({
            "case": "d2-verdict-after-excluded-progress", "rule": "D2 / Opus N1",
            "frames_hex": encode(vec![provision(1,None), progress(1), vv]),
            "view": {"default": "Present(FinalizationPrepared,echo)", "overrides": [{"tape_uuid":hex(&tape(1).uuid), "sequence":1, "answer":"Contradiction"}]},
            "expected": {"received_through":3, "applied_through":3, "pending_sequences":[], "contradicted_tapes":[hex(&tape(1).uuid)], "excluded_diagnostics":2, "refusal":null}
        }),
        serde_json::json!({
            "case": "d5-repair-below-received-reference", "rule":"D5",
            "frames_hex":encode(vec![provision(1,None),high,b4b0_repair(8,8)]),
            "views":["Present(FinalizationPrepared,echo)","NotYet","Contradiction"],
            "expected": {"refusal_sequence":3,"refusal":"RepairBelowReference { highest: 9 }", "highest_reference":9}
        }),
        serde_json::json!({
            "case":"d6-deferred-fence-unknown-at-resolution", "rule":"D6",
            "frames_hex":encode(vec![provision(1,None), verdict(), fence(1)]),
            "view":"NotYet",
            "injection":"After receive, remove pending sequence 2 and its byte charge; remove tape 01010101010101010101010101010101 from the identity map; resolve with Present. This injected impossible production state exercises the defensive unknown-target rule.",
            "expected":{"pending_sequences":[],"open_fence_ids":[3],"contradicted_tapes":[],"fence_liveness_failure":{"fence":3,"tape_uuid":hex(&tape(1).uuid),"incarnation":1,"cause":"UnknownTape"}},
            "release":"Receive FenceReleased(3); its liveness report disappears."
        }),
    ]
}

/// Positive corpus applications enter SQL twice through the same public apply.
#[test]
fn authority_positive_corpus_applies_twice() {
    use crate::projector::{self, apply::ApplyPath, thread::rebuild_library};
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/library-log");
    let cases: std::collections::BTreeSet<_> = collect(&root)
        .keys()
        .map(|p| p.parent().unwrap().to_path_buf())
        .collect();
    let mut failures = Vec::new();
    let mut tested = 0;
    for case in cases {
        let view = case_view(&case);
        let expected = replay(&root.join(&case), LOG, &view).unwrap();
        if expected.refusal.is_some() {
            continue;
        }
        let temp = tempfile::tempdir().unwrap();
        let mut writer = projector::open(temp.path().join("catalog.sqlite"))
            .unwrap()
            .write_handle();
        for pass in 0..2 {
            if let Err(error) = rebuild_library(
                &mut writer,
                &root.join(&case),
                LOG,
                &view,
                ApplyPath::Rebuild,
            ) {
                failures.push(format!("{} pass {pass}: {error}", case.display()));
                break;
            }
            // Even a stale individual retry must leave completed-prefix
            // progress intact, including excluded and pending tail positions.
            let before = writer.normalized_authority_dump();
            let mut first = None;
            super::store::replay_observed(&root.join(&case), LOG, &view, &mut |record| {
                if first.is_none() {
                    first = Some(record);
                }
                Ok(())
            })
            .unwrap();
            if let Some(record) = first {
                writer
                    .apply(
                        &crate::projector::apply::AuthorityRecord::Library(Box::new(record)),
                        ApplyPath::Startup,
                    )
                    .unwrap();
                assert_eq!(
                    before,
                    writer.normalized_authority_dump(),
                    "stale duplicate changed {} pass {pass}",
                    case.display()
                );
            }
        }
        tested += 1;
    }
    assert!(tested > 10, "no positive corpus coverage");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Each corpus case gets 200 deterministic shuffles of the library stream and
/// growing native streams. Each native stream retains its local sequence order.
/// The fold alone selects applications; SQL sees only its observer notifications.
#[test]
fn authority_corpus_200_source_interleavings() {
    use super::authority::AppliedLibraryRecord;
    use crate::projector::{
        self,
        apply::{ApplyError, ApplyPath, AuthorityRecord},
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/library-log");
    let cases: std::collections::BTreeSet<_> = collect(&root)
        .keys()
        .map(|p| p.parent().unwrap().to_path_buf())
        .collect();
    let mut tested = 0;
    for case in cases {
        let final_view = case_view(&case);
        let expected = replay(&root.join(&case), LOG, &final_view).unwrap();
        if expected.refusal.is_some() {
            continue;
        }
        let mut frames = Vec::new();
        for path in segments(&root.join(&case)) {
            let bytes = fs::read(path).unwrap();
            let mut input = bytes.as_slice();
            let header = codec::Header::decode(&mut input, LOG).unwrap();
            let mut decoder = codec::Decoder::at(input, header.first_sequence, 84);
            let mut hash = header.previous_segment_hash;
            while let Ok(Some(frame)) = decoder.next_frame() {
                if frame.sequence > expected.last_sequence {
                    break;
                }
                hash = codec::verify(&frame, hash).unwrap();
                let receipt = Receipt {
                    sequence: frame.sequence,
                    record_uuid: frame.record.envelope.record_uuid,
                    record_hash: hash,
                    synced: true,
                };
                frames.push((frame.record, receipt));
            }
        }
        let mut native: BTreeMap<Id, BTreeMap<u64, TapeLookup>> = BTreeMap::new();
        for (record, _) in &frames {
            let at = record.body.at().or({
                if let Body::FenceRaised(f) = &record.body {
                    f.at.as_ref()
                } else {
                    None
                }
            });
            if let Some(at) = at {
                native
                    .entry(at.tape.uuid)
                    .or_default()
                    .insert(at.sequence, final_view.lookup(at).unwrap());
            }
        }
        // A fixture that deliberately supplies multiple conflicting bytes for
        // one native position uses the published final view, never a last-wins map.
        struct Growing<'a> {
            view: &'a CorpusView,
            through: BTreeMap<Id, u64>,
        }
        impl TapeView for Growing<'_> {
            fn lookup(&self, at: &TapeAt) -> Result<TapeLookup, FoldError> {
                Ok(
                    if self.through.get(&at.tape.uuid).copied().unwrap_or(0) >= at.sequence {
                        self.view.lookup(at).unwrap()
                    } else {
                        TapeLookup::NotYet
                    },
                )
            }
        }
        let mut baseline = None;
        for iteration in 0..200u64 {
            let seed = 0xb4b1_0000u64 + iteration;
            if iteration == 0 {
                eprintln!(
                    "B4b1 interleavings case={} seeds={seed:#x}..{:#x} (200)",
                    case.display(),
                    seed + 199
                );
            }
            let mut random = seed;
            let mut cursor = 0;
            let mut remaining: BTreeMap<_, std::collections::VecDeque<_>> = native
                .iter()
                .map(|(id, rows)| (*id, rows.keys().copied().collect()))
                .collect();
            let mut view = Growing {
                view: &final_view,
                through: BTreeMap::new(),
            };
            let mut fold = LibraryFold::default();
            let mut history = History::default();
            let mut writer = projector::memory_projection_fixture();
            let mut applied_records = Vec::new();
            while cursor < frames.len() || remaining.values().any(|r| !r.is_empty()) {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let keys: Vec<_> = remaining
                    .iter()
                    .filter(|(_, rows)| !rows.is_empty())
                    .map(|(id, _)| *id)
                    .collect();
                let choice = (random as usize) % (keys.len() + usize::from(cursor < frames.len()));
                let mut applied = Vec::new();
                if choice < keys.len() {
                    let id = keys[choice];
                    let n = remaining.get_mut(&id).unwrap().pop_front().unwrap();
                    view.through.insert(id, n);
                    fold.resolve_observed(&view, &history, &mut |n| applied.push(n))
                        .unwrap_or_else(|e| panic!("case={} seed={seed:#x}: {e}", case.display()));
                } else {
                    let (record, receipt) = &frames[cursor];
                    fold.receive_observed(
                        receipt.sequence,
                        record.clone(),
                        receipt.clone(),
                        &view,
                        &history,
                        &mut |n| applied.push(n),
                    )
                    .unwrap_or_else(|e| panic!("case={} seed={seed:#x}: {e}", case.display()));
                    history.0.insert(receipt.sequence, record.clone());
                    cursor += 1;
                }
                for sequence in applied {
                    let record = AppliedLibraryRecord {
                        log_uuid: LOG,
                        record_hash: frames[(sequence - 1) as usize].1.record_hash,
                        sequence,
                        record: history.0[&sequence].clone(),
                        progress: super::authority::FoldProgress::capture(&fold),
                    };
                    let source = AuthorityRecord::Library(Box::new(record));
                    match writer.apply(&source, ApplyPath::Rebuild) {
                        Ok(_) | Err(ApplyError::HealthConflict { .. }) => (),
                        Err(e) => panic!(
                            "case={} seed={seed:#x} sequence={sequence}: {e}",
                            case.display()
                        ),
                    }
                    applied_records.push(source);
                }
            }
            writer.finish_observed_fold(&fold);
            let before = writer.normalized_authority_dump();
            for source in applied_records {
                match writer.apply(&source, ApplyPath::Startup) {
                    Ok(_) | Err(ApplyError::HealthConflict { .. }) => (),
                    Err(e) => panic!("case={} seed={seed:#x} retry: {e}", case.display()),
                }
            }
            writer.finish_observed_fold(&fold);
            assert_eq!(
                before,
                writer.normalized_authority_dump(),
                "retry case={} seed={seed:#x}",
                case.display()
            );
            if let Some(expected) = &baseline {
                assert_eq!(
                    &before,
                    expected,
                    "interleaving case={} seed={seed:#x}",
                    case.display()
                );
            } else {
                baseline = Some(before);
            }
        }
        tested += 1;
    }
    assert!(tested > 10);
}

/// Dispatch every canonical tag through its public typed writer method. The
/// source references remain stable while fresh operation IDs are remapped.
#[test]
fn authority_every_typed_method_uses_corpus_rules() {
    use std::sync::Arc;
    struct Free;
    impl FinalizationView for Free {
        fn committed_instance_count(&self, _: TapeRef) -> u64 {
            0
        }
        fn finalization_blocks_change(&self, _: TapeRef) -> bool {
            false
        }
    }
    let source = tempfile::tempdir().unwrap();
    all_records(source.path());
    let target = tempfile::tempdir().unwrap();
    let lease = Arc::new(crate::StateLockGuard::acquire(target.path()).unwrap());
    let authority = LibraryAuthority::open(
        &target.path().join("library"),
        LOG,
        Origin {
            host: "test".into(),
            pid: 1,
            build: "b4b1".into(),
        },
        Box::new(FixedClock(Arc::new(std::sync::atomic::AtomicU64::new(NOW)))),
        Box::new(Ids),
        false,
        &Present,
        lease,
    )
    .unwrap();
    let mut ids = BTreeMap::new();
    let mut tags = std::collections::BTreeSet::new();
    for path in segments(source.path()) {
        let bytes = fs::read(path).unwrap();
        let mut input = bytes.as_slice();
        let header = codec::Header::decode(&mut input, LOG).unwrap();
        let mut decoder = codec::Decoder::at(input, header.first_sequence, 84);
        while let Some(frame) = decoder.next_frame().unwrap() {
            let tag = frame.record.tag();
            tags.insert(tag as u16);
            let actor = frame.record.envelope.actor.clone();
            let layer = SourceLayer::Layer4;
            // Every generation-bearing public method refuses a stale expectation.
            let wrong = match frame.record.body.clone() {
                Body::AssignPool(v) => {
                    let expected = v.change.expected + 1;
                    Some(authority.assign_pool(
                        v,
                        expected,
                        &Free,
                        actor.clone(),
                        layer.clone(),
                        &Present,
                    ))
                }
                Body::Label(v) => {
                    let expected = v.change.expected + 1;
                    Some(authority.label(
                        v,
                        expected,
                        &Free,
                        actor.clone(),
                        layer.clone(),
                        &Present,
                    ))
                }
                Body::Retire(v) => {
                    let expected = v.assignment_generation + 1;
                    Some(authority.retire(
                        v,
                        expected,
                        &Free,
                        actor.clone(),
                        layer.clone(),
                        &Present,
                    ))
                }
                Body::MediumSpecSet(v) => {
                    let expected = v.change.expected + 1;
                    let source = v.medium.source;
                    Some(authority.medium_spec_set(
                        v,
                        expected,
                        source,
                        actor.clone(),
                        layer.clone(),
                        &Present,
                    ))
                }
                Body::DriveDecision(v) => {
                    let expected = v.change.expected + 1;
                    Some(authority.drive_decision(
                        v,
                        expected,
                        actor.clone(),
                        layer.clone(),
                        &Present,
                    ))
                }
                Body::AlarmChanged(v) => {
                    let expected = v.change.expected + 1;
                    Some(authority.alarm_changed(
                        v,
                        expected,
                        actor.clone(),
                        layer.clone(),
                        &Present,
                    ))
                }
                _ => None,
            };
            if let Some(wrong) = wrong {
                assert!(
                    matches!(
                        wrong,
                        Err(AuthorityError::Store(StoreError::Fold(
                            FoldError::GenerationMismatch
                        )))
                    ),
                    "tag={tag:?}: {wrong:?}"
                );
            }
            let result = match frame.record.body {
                Body::AssignPool(v) => {
                    let expected = v.change.expected;
                    authority.assign_pool(v, expected, &Free, actor, layer, &Present)
                }
                Body::Label(v) => {
                    let expected = v.change.expected;
                    authority.label(v, expected, &Free, actor, layer, &Present)
                }
                Body::Retire(v) => {
                    let expected = v.assignment_generation;
                    authority.retire(v, expected, &Free, actor, layer, &Present)
                }
                Body::MediumSpecSet(v) => {
                    let expected = v.change.expected;
                    let source = v.medium.source;
                    authority.medium_spec_set(v, expected, source, actor, layer, &Present)
                }
                Body::DriveDecision(v) => {
                    let expected = v.change.expected;
                    authority.drive_decision(v, expected, actor, layer, &Present)
                }
                Body::AlarmChanged(v) => {
                    let expected = v.change.expected;
                    authority.alarm_changed(v, expected, actor, layer, &Present)
                }
                Body::OperationRecorded(mut v) => {
                    if let OperationEvent::Accepted(a) = v.event {
                        authority
                            .begin_operation(v.kind, a, actor, layer, &Present)
                            .map(|(id, r)| {
                                ids.insert(v.operation, id);
                                r
                            })
                    } else if v.previous.is_none() && matches!(v.event, OperationEvent::Started(_))
                    {
                        authority
                            .start_operation(v.kind, actor, layer, &Present)
                            .map(|(id, r)| {
                                ids.insert(v.operation, id);
                                r
                            })
                    } else {
                        v.operation = ids.get(&v.operation).copied().unwrap_or(v.operation);
                        authority.operation_recorded(v, actor, layer, &Present)
                    }
                }
                Body::ReadinessObserved(mut v) => {
                    if v.previous.is_none() {
                        let old = v.operation;
                        authority
                            .begin_readiness(v, actor, layer, &Present)
                            .map(|(id, r)| {
                                ids.insert(old, id);
                                r
                            })
                    } else {
                        v.operation = ids.get(&v.operation).copied().unwrap_or(v.operation);
                        authority.readiness_observed(v, actor, layer, &Present)
                    }
                }
                Body::Reprovision(v) => authority.reprovision(v, actor, layer, &Present),
                Body::SupersedeTail(v) => authority.supersede_tail(v, actor, layer, &Present),
                Body::AcceptUnprotectedFinalization(v) => {
                    authority.accept_unprotected_finalization(v, actor, layer, &Present)
                }
                Body::FenceRaised(v) => authority.fence_raised(v, actor, layer, &Present),
                Body::FenceReleased(v) => authority.fence_released(v, actor, layer, &Present),
                Body::MotionIntent(v) => authority.motion_intent(v, actor, layer, &Present),
                Body::MotionOutcome(v) => authority.motion_outcome(v, actor, layer, &Present),
                Body::InventoryReconciled(v) => {
                    authority.inventory_reconciled(v, actor, layer, &Present)
                }
                Body::VerificationProgress(v) => {
                    authority.verification_progress(v, actor, layer, &Present)
                }
                Body::VerificationVerdict(v) => {
                    authority.verification_verdict(v, actor, layer, &Present)
                }
                Body::ManualFinalizationAccepted(v) => {
                    authority.manual_finalization_accepted(v, actor, layer, &Present)
                }
                Body::ConfigurationAccepted(v) => {
                    authority.configuration_accepted(v, actor, layer, &Present)
                }
                Body::DriveObserved(v) => authority.drive_observed(v, actor, layer, &Present),
                Body::DriveHealthObserved(v) => {
                    authority.drive_health_observed(v, actor, layer, &Present)
                }
                Body::CleaningRecorded(v) => authority.cleaning_recorded(v, actor, layer, &Present),
                Body::SessionObserved(v) => authority.session_observed(v, actor, layer, &Present),
                Body::WrapMapCached(v) => authority.wrap_map_cached(v, actor, layer, &Present),
                Body::RecoveryRecorded(v) => authority.recovery_recorded(v, actor, layer, &Present),
                Body::Diagnostic(v) => authority.diagnostic(v, actor, layer, &Present),
            };
            let receipt =
                result.unwrap_or_else(|e| panic!("tag={tag:?} sequence={}: {e}", frame.sequence));
            assert_eq!(receipt.receipt.sequence, frame.sequence);
            assert_eq!(receipt.receipt.synced, tag != Tag::Diagnostic);
        }
    }
    assert_eq!(tags.len(), 27);
}

/// A high applied sequence cannot cover an older pending row. The observer
/// reports the retry where it actually applies, and digest conflicts roll back.
#[test]
fn authority_observer_pending_lower_sequence_and_digest_conflict() {
    use super::authority::AppliedLibraryRecord;
    use crate::projector::apply::{ApplyError, ApplyPath, AuthorityRecord};
    let mut fold = LibraryFold::default();
    let mut history = History::default();
    let mut applied = Vec::new();
    let mut writer = crate::projector::memory_projection_fixture();
    for (i, body) in [provision(1, None), verdict(), provision(2, None)]
        .into_iter()
        .enumerate()
    {
        let sequence = i as u64 + 1;
        let record = record(body, sequence);
        let receipt = Receipt {
            sequence,
            record_uuid: record.envelope.record_uuid,
            record_hash: [0; 32],
            synced: true,
        };
        fold.receive_observed(
            sequence,
            record.clone(),
            receipt,
            &View::default(),
            &history,
            &mut |n| applied.push(n),
        )
        .unwrap();
        history.0.insert(sequence, record);
    }
    assert_eq!(applied, [1, 3]);
    let proof = |n, fold: &LibraryFold| {
        AuthorityRecord::Library(Box::new(AppliedLibraryRecord {
            log_uuid: LOG,
            record_hash: [0; 32],
            sequence: n,
            record: history.0[&n].clone(),
            progress: super::authority::FoldProgress::capture(fold),
        }))
    };
    for sequence in applied {
        let marks = writer
            .apply(&proof(sequence, &fold), ApplyPath::Live)
            .unwrap();
        if sequence == 3 {
            assert_eq!((marks.received, marks.applied, marks.pending), (3, 1, 1));
        }
    }
    let stale_fold = fold.clone();
    let mut retried = Vec::new();
    fold.resolve_observed(&Present, &history, &mut |n| retried.push(n))
        .unwrap();
    assert_eq!(retried, [2]);
    let marks = writer.apply(&proof(2, &fold), ApplyPath::Startup).unwrap();
    assert_eq!((marks.received, marks.applied, marks.pending), (3, 3, 0));
    let before = writer.normalized_authority_dump();
    writer.finish_observed_fold(&stale_fold);
    assert_eq!(before, writer.normalized_authority_dump());
    writer.apply(&proof(2, &fold), ApplyPath::Startup).unwrap();
    assert_eq!(before, writer.normalized_authority_dump());
    let mut conflict = proof(2, &fold);
    let AuthorityRecord::Library(ref mut entry) = conflict else {
        unreachable!()
    };
    entry.record.envelope.timestamp_unix_ns += 1;
    assert!(matches!(
        writer.apply(&conflict, ApplyPath::Live),
        Err(ApplyError::Contradiction(_))
    ));
    assert_eq!(before, writer.normalized_authority_dump());
    let mut conflict = proof(2, &fold);
    let AuthorityRecord::Library(ref mut entry) = conflict else {
        unreachable!()
    };
    entry.log_uuid = [0xff; 16];
    assert!(matches!(
        writer.apply(&conflict, ApplyPath::Live),
        Err(ApplyError::Contradiction(_))
    ));
    assert_eq!(before, writer.normalized_authority_dump());
    let mut changed_chain = proof(2, &fold);
    let AuthorityRecord::Library(ref mut entry) = changed_chain else {
        unreachable!()
    };
    entry.record_hash = [0x7e; 32];
    assert!(matches!(
        writer.apply(&changed_chain, ApplyPath::Live),
        Err(ApplyError::Contradiction(_))
    ));
    assert_eq!(before, writer.normalized_authority_dump());
}

/// Actual tape bytes and library receipts commute across source schedules too;
/// the corpus property above separately covers its synthetic native proof keys.
#[test]
fn authority_real_tape_and_library_200_interleavings() {
    use crate::{
        authority::TapeAuthorityView,
        projector::apply::{ApplyPath, AuthorityRecord},
    };
    use std::sync::Arc;
    struct Free;
    impl FinalizationView for Free {
        fn committed_instance_count(&self, _: TapeRef) -> u64 {
            0
        }
        fn finalization_blocks_change(&self, _: TapeRef) -> bool {
            false
        }
    }
    let root = tempfile::tempdir().unwrap();
    let lease = Arc::new(crate::StateLockGuard::acquire(root.path()).unwrap());
    let authority = LibraryAuthority::open(
        &root.path().join("library"),
        LOG,
        Origin {
            host: "test".into(),
            pid: 1,
            build: "b4b1".into(),
        },
        Box::new(FixedClock(Arc::new(std::sync::atomic::AtomicU64::new(NOW)))),
        Box::new(Ids),
        false,
        &Present,
        lease,
    )
    .unwrap();
    let Body::Reprovision(p) = provision(0xa3, None) else {
        unreachable!()
    };
    let mut library = authority
        .reprovision(p, AuditActor::System, SourceLayer::Layer4, &Present)
        .unwrap()
        .applied;
    library.extend(
        authority
            .retire(
                Retire {
                    tape: tape(0xa3),
                    reason: RetireReason::OperatorRequested,
                    observed_tape_sequence: None,
                    assignment_generation: 1,
                    detail: None,
                },
                1,
                &Free,
                AuditActor::System,
                SourceLayer::Layer4,
                &Present,
            )
            .unwrap()
            .applied,
    );
    let dir = root.path().join("tapes");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!(
            "{}.remtape",
            uuid::Uuid::from_bytes([0xa3; 16]).simple()
        )),
        include_bytes!("../../../../fixtures/tape-log/sealed-terminal.remtape"),
    )
    .unwrap();
    let native = crate::TapeAuthority::open_fixture(&dir, [0xa3; 16])
        .unwrap()
        .freeze()
        .unwrap();
    let Body::Reprovision(mut p) = provision(0xa1, None) else {
        unreachable!()
    };
    p.pool = Some("archive".into());
    p.medium.record_size = 262144;
    library.extend(
        authority
            .reprovision(p, AuditActor::System, SourceLayer::Layer4, &Present)
            .unwrap()
            .applied,
    );
    // This decision is admitted while the native tape is empty. Replay below
    // may deliver it between committed prefixes without changing that history.
    library.extend(
        authority
            .assign_pool(
                AssignPool {
                    tape: tape(0xa1),
                    pool: Some("new-pool".into()),
                    change: GenerationChange {
                        expected: 1,
                        next: 2,
                    },
                    source: AssignmentSource::Operator,
                    configuration: None,
                },
                1,
                &Free,
                AuditActor::System,
                SourceLayer::Layer4,
                &Present,
            )
            .unwrap()
            .applied,
    );
    let object_prefixes = object_tape_prefixes(root.path());
    let mut assignment_between_commits = false;
    let mut baseline = None;
    for iteration in 0..200u64 {
        let seed = 0xb4b1_a000 + iteration;
        let mut random = seed;
        let mut cursor = 0;
        let mut tape_pending = true;
        let mut objects = 0;
        let mut writer = crate::projector::memory_projection_without_instance_pool_fixture();
        while cursor < library.len() || tape_pending || objects < object_prefixes.len() {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            let source = if objects < object_prefixes.len()
                && (random % 3 == 0 || (cursor == library.len() && !tape_pending))
            {
                let source = AuthorityRecord::Tape(Box::new(object_prefixes[objects].clone()));
                objects += 1;
                source
            } else if tape_pending && (cursor == library.len() || random & 1 == 0) {
                tape_pending = false;
                AuthorityRecord::Tape(Box::new(native.clone()))
            } else {
                if library[cursor].record.tag() == Tag::AssignPool && objects == 1 {
                    assignment_between_commits = true;
                }
                let source = AuthorityRecord::Library(Box::new(library[cursor].clone()));
                cursor += 1;
                source
            };
            writer
                .apply(&source, ApplyPath::Rebuild)
                .unwrap_or_else(|e| panic!("seed={seed:#x}: {e}"));
        }
        let dump = writer.normalized_authority_dump();
        for _ in 0..2 {
            for record in &library {
                writer
                    .apply(
                        &AuthorityRecord::Library(Box::new(record.clone())),
                        ApplyPath::Startup,
                    )
                    .unwrap();
            }
            for snapshot in object_prefixes
                .last()
                .into_iter()
                .chain(std::iter::once(&native))
            {
                writer
                    .apply(
                        &AuthorityRecord::Tape(Box::new(snapshot.clone())),
                        ApplyPath::Startup,
                    )
                    .unwrap();
            }
            assert_eq!(
                dump,
                writer.normalized_authority_dump(),
                "retry seed={seed:#x}"
            );
        }
        eprintln!("B4b1 object-bearing native/library seed={seed:#x}");
        if let Some(expected) = &baseline {
            assert_eq!(&dump, expected, "actual tape source seed={seed:#x}");
        } else {
            baseline = Some(dump);
        }
    }
    assert!(
        assignment_between_commits,
        "must exercise AssignPool between native commits"
    );
    eprintln!("B4b1 actual native/library sources seeds=0xb4b1a000..0xb4b1a0c7 (200)");
}

/// Make two frozen native prefixes, each ending at a real object commit. The
/// first snapshot retains its original file after the source grows to two objects.
fn object_tape_prefixes(root: &Path) -> Vec<crate::tape_log::authority::AuthoritySnapshot> {
    use crate::authority::TapeAuthorityView;
    let dir = root.join("objects");
    fs::create_dir_all(&dir).unwrap();
    let bytes = include_bytes!("../../../../fixtures/tape-log/parity-off-two-objects.remtape");
    let mut cursor = std::io::Cursor::new(bytes);
    crate::tape_log::store::Header::decode(&mut cursor, [0xa1; 16]).unwrap();
    let offset = cursor.position();
    let mut decoder = crate::tape_log::codec::Decoder::at(cursor, 1, offset);
    let mut ends = Vec::new();
    let mut distinct = bytes[..offset as usize].to_vec();
    while let Some(frame) = decoder.next_frame().unwrap() {
        let mut record = frame.record;
        // Frozen fixtures reuse a session UUID. Give this independent tape its
        // own session; fixed-width edits leave group offsets/digests unchanged.
        match &mut record {
            crate::tape_log::Record::SessionOpened(v) => v.session_id = [0xd1; 16],
            crate::tape_log::Record::Committed(v) => v.session_id = Some([0xd1; 16]),
            _ => (),
        }
        crate::tape_log::codec::encode_frame(&mut distinct, frame.sequence, &record, frame.payload)
            .unwrap();
        assert_eq!(distinct.len(), frame.end as usize);
        if record.tag() == crate::tape_log::Tag::Committed {
            ends.push(distinct.len());
        }
    }
    let bytes = distinct;
    assert_eq!(ends.len(), 2);
    let path = dir.join(format!(
        "{}.remtape",
        uuid::Uuid::from_bytes([0xa1; 16]).simple()
    ));
    ends.into_iter()
        .map(|end| {
            // Replace the file, preserving earlier frozen descriptors.
            let temp = dir.join("next");
            fs::write(&temp, &bytes[..end]).unwrap();
            fs::rename(temp, &path).unwrap();
            crate::TapeAuthority::open_fixture(&dir, [0xa1; 16])
                .unwrap()
                .freeze()
                .unwrap()
        })
        .collect()
}

/// Gate reproduction: a real committed object and a provisioned pool commute
/// in both source orders, including repeated native and library delivery.
#[test]
fn authority_object_pool_both_orders_and_retries() {
    use crate::projector::apply::{ApplyPath, AuthorityRecord};
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let lease = Arc::new(crate::StateLockGuard::acquire(root.path()).unwrap());
    let authority = LibraryAuthority::open(
        &root.path().join("library"),
        LOG,
        Origin {
            host: "test".into(),
            pid: 1,
            build: "fix1".into(),
        },
        Box::new(FixedClock(Arc::new(std::sync::atomic::AtomicU64::new(NOW)))),
        Box::new(Ids),
        false,
        &Present,
        lease,
    )
    .unwrap();
    let Body::Reprovision(mut p) = provision(0xa1, None) else {
        unreachable!()
    };
    p.pool = Some("archive".into());
    p.medium.record_size = 262144;
    let library = authority
        .reprovision(p, AuditActor::System, SourceLayer::Layer4, &Present)
        .unwrap()
        .applied
        .remove(0);
    let native = object_tape_prefixes(root.path()).remove(0);
    let sources = [
        AuthorityRecord::Library(Box::new(library)),
        AuthorityRecord::Tape(Box::new(native)),
    ];
    let mut baseline = None;
    for order in [[0, 1], [1, 0]] {
        let mut writer = crate::projector::memory_projection_fixture();
        for _ in 0..3 {
            for i in order {
                writer.apply(&sources[i], ApplyPath::Live).unwrap();
            }
            let instances = writer.list_native_objects().unwrap();
            assert_eq!(instances.len(), 1);
            assert_eq!(
                instances[0].instances[0].pool_id.as_deref(),
                Some("archive")
            );
            let dump = writer.normalized_authority_dump();
            if let Some(expected) = &baseline {
                assert_eq!(&dump, expected);
            } else {
                baseline = Some(dump);
            }
        }
    }
}

/// A decided duplicate is not a pending dependency on the next live append.
#[test]
fn authority_health_duplicate_then_live_append_has_no_pending() {
    use crate::projector::{
        apply::{ApplyPath, AuthorityRecord},
        thread::{ProjectionClient, ProjectionResult},
    };
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("library");
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/library-log/health-identical-latest");
    fs::create_dir_all(&dir).unwrap();
    for entry in fs::read_dir(&corpus).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
        }
    }
    let view = case_view(Path::new("health-identical-latest"));
    let client = ProjectionClient::open(root.path().join("catalog.sqlite")).unwrap();
    let report = client
        .catch_up_library(dir.clone(), LOG, Box::new(view), ApplyPath::Startup)
        .unwrap();
    assert!(report.pending.is_empty());
    let lease = Arc::new(crate::StateLockGuard::acquire(root.path()).unwrap());
    let authority = LibraryAuthority::open(
        &dir,
        LOG,
        Origin {
            host: "test".into(),
            pid: 1,
            build: "fix1".into(),
        },
        Box::new(FixedClock(Arc::new(std::sync::atomic::AtomicU64::new(NOW)))),
        Box::new(Ids),
        false,
        &case_view(Path::new("health-identical-latest")),
        lease,
    )
    .unwrap();
    let Body::Diagnostic(d) = diagnostic() else {
        unreachable!()
    };
    let receipt = authority
        .diagnostic(d, AuditActor::System, SourceLayer::Layer4, &Present)
        .unwrap();
    assert_eq!(receipt.receipt.sequence, 6);
    for record in receipt.applied {
        let ProjectionResult::Applied(marks) = client
            .project(AuthorityRecord::Library(Box::new(record)), ApplyPath::Live)
            .unwrap()
        else {
            panic!("duplicate became pending");
        };
        assert_eq!(marks.pending, 0);
        assert_eq!(marks.applied_records, 5);
        assert_eq!(marks.non_applications, 1);
        assert_eq!(marks.applied, 4); // SQL prefix cannot cross the unapplied duplicate.
        assert_eq!(marks.received, 6);
    }
}

/// Build canonical evidence with byte-sorted keys, as required of future producers.
fn l1w_detail(entries: Vec<(&str, DiagnosticValue)>) -> Bounded<DiagnosticEntry, 256> {
    let sorted: BTreeMap<Text, DiagnosticValue> = entries
        .into_iter()
        .map(|(key, value)| (key.into(), value))
        .collect();
    Bounded(
        sorted
            .into_iter()
            .map(|(key, value)| DiagnosticEntry(key, value))
            .collect(),
    )
}

/// Exercise L1's added values through the same append owner as the existing corpus.
fn l1w_optional_forms(l: &mut LibraryLog, config: DecisionRef) {
    for (n, source) in [
        (240, PoolSource::Operator(Unit)),
        (241, PoolSource::Rule(config)),
    ] {
        let Body::Reprovision(mut v) = provision(n, None) else {
            unreachable!()
        };
        v.pool = Some("pool-a".into());
        v.pool_source = Some(source);
        v.operation = Some([n; 16]);
        v.replaced_bot = Some([n - 1; 16]);
        body(l, Body::Reprovision(v));
    }
    for (i, phase, cancel) in [
        (
            0,
            ReadinessPhase::LoadDriveReadiness,
            CancelSource::Operator,
        ),
        (
            1,
            ReadinessPhase::LoadDriveCompletion,
            CancelSource::Shutdown,
        ),
        (
            2,
            ReadinessPhase::LoadDriveReadiness,
            CancelSource::StartupReconcile,
        ),
    ] {
        body(
            l,
            Body::ReadinessObserved(ReadinessObserved {
                operation: [240 + i; 16],
                endpoint: DriveKey::Standalone(Standalone {
                    drive_serial: format!("L1-W-{i}").as_str().into(),
                }),
                tape: None,
                barcode: None,
                phase,
                state: ReadinessState::Planned,
                previous: None,
                started_unix_ns: NOW,
                limit_ms: None,
                evidence: None,
                load_intent: None,
                ready_generation: 1,
                result: ReadinessResult::Pending,
                scope: ReadinessScope::Endpoint,
                media: None,
                source_slot: Some(42),
                run: Some([240 + i; 16]),
                cancel_source: Some(cancel),
            }),
        );
    }
    for (name, kind) in [
        ("serial-collision", AlarmKind::DriveSerialCollision),
        ("retired-reappeared", AlarmKind::RetiredDriveReappeared),
    ] {
        body(
            l,
            Body::AlarmChanged(AlarmChanged {
                condition_key: name.into(),
                change: GenerationChange {
                    expected: 0,
                    next: 1,
                },
                action: AlarmAction::Raise(AlarmRaise {
                    kind,
                    severity: Severity::Critical,
                    reason: "fixture".into(),
                }),
                cause: None,
            }),
        );
    }
    use DiagnosticValue::{Bytes, Text as DText, Unsigned};
    for (basis, native, zero, conflict) in [
        ("floor", true, false, false),
        ("mark", false, false, false),
        ("both", true, false, false),
        ("floor", true, false, true),
        ("floor", false, true, false),
    ] {
        let mut entries = vec![
            ("basis", DText(basis.into())),
            ("reason", DText("native log behind projection".into())),
        ];
        if basis != "mark" {
            entries.extend([
                ("floor_digest", Bytes(Blob(vec![1; 32]))),
                ("floor_named_by", Unsigned(3)),
                ("floor_sequence", Unsigned(if zero { 0 } else { 1 })),
                ("observed_closed_end", Unsigned(84)),
                ("observed_floor", DText("behind".into())),
                ("observed_last_closed", Unsigned(0)),
            ]);
        }
        if basis != "floor" {
            entries.extend([
                ("mark_offset", Unsigned(128)),
                ("mark_sequence", Unsigned(1)),
                ("observed_mark", DText("short_bytes".into())),
                ("source_byte_end", Unsigned(256)),
                ("source_digest", Bytes(Blob(vec![2; 32]))),
            ]);
        }
        if conflict {
            entries.push(("conflict_digests", Bytes(Blob(vec![3; 64]))));
        }
        let raised = body(
            l,
            Body::FenceRaised(FenceRaised {
                kind: FenceKind::Write,
                scope: FenceScope::Tape(tape(1)),
                cause: FenceCause::NativeLogBehindProjection,
                evidence: None,
                at: native.then(|| at(1, 1)),
                operation: None,
                detail: l1w_detail(entries),
            }),
        );
        let detail = if basis == "floor" {
            l1w_detail(vec![
                ("basis", DText("floor".into())),
                ("floor_digest", Bytes(Blob(vec![1; 32]))),
                ("floor_sequence", Unsigned(if zero { 0 } else { 1 })),
                ("observed_closed_end", Unsigned(84)),
                ("observed_floor", DText("behind".into())),
                ("observed_last_closed", Unsigned(0)),
                ("observed_prefix_digest", Bytes(Blob(vec![4; 32]))),
            ])
        } else {
            Bounded(vec![])
        };
        body(
            l,
            Body::FenceReleased(FenceReleased {
                fence: raised.sequence,
                evidence: "operator release".into(),
                forced: true,
                detail,
            }),
        );
    }
    // Every assigned refusal number is independently round-tripped and documented.
    for code in 1..=14 {
        let refusal = RefusalCode::from_u32(code).unwrap();
        assert_eq!(refusal as u32, code);
        body(
            l,
            Body::Diagnostic(Diagnostic {
                event: DiagnosticEvent::RefusalRecorded,
                subject_kind: "tape".into(),
                subject_id: Some("fixture".into()),
                detail: l1w_detail(vec![
                    ("code", Unsigned(u64::from(code))),
                    ("name", DText(format!("{refusal:?}").as_str().into())),
                ]),
            }),
        );
    }
    assert_eq!(RefusalCode::from_u32(0), None);
    assert_eq!(RefusalCode::from_u32(15), None);
    for (i, outcome, native) in [
        (0, VerificationOutcome::VerifiedComplete, true),
        (1, VerificationOutcome::VerifiedDegraded, true),
        (2, VerificationOutcome::RecoveryRequired, true),
        (3, VerificationOutcome::VerifiedComplete, false),
    ] {
        let Body::VerificationVerdict(mut v) = verdict() else {
            unreachable!()
        };
        v.operation = [230 + i; 16];
        v.outcome = outcome;
        v.at = native.then(|| at(1, 1));
        v.modes = 0x7f;
        body(l, Body::VerificationVerdict(v));
    }
}

/// Shape and counterpart failures freeze both live refusal and byte-replay refusal.
fn l1w_negative_histories() -> Vec<(&'static str, Vec<Body>, Body, FoldError)> {
    let mut out = Vec::new();
    for (name, mode, predecessor) in [
        ("replaced-bot-adopt", ProvisionMode::Adopt, None),
        (
            "replaced-bot-predecessor",
            ProvisionMode::Initialize,
            Some(tape(1)),
        ),
    ] {
        let Body::Reprovision(mut v) = provision(3, None) else {
            unreachable!()
        };
        v.mode = mode;
        v.predecessor = predecessor;
        if mode == ProvisionMode::Adopt {
            // A body the fold accepts without `replaced_bot`, so that the evidence key is its
            // only violation. The fields mirror the corpus's accepted bay adoption.
            v.tape.uuid[6] = 0x43;
            v.tape.uuid[8] = 0x83;
            v.pool = Some("pool-a".into());
            v.endpoint = Some(endpoint("LIBB0002"));
            v.adoption = Some(Adoption {
                request_fingerprint: [67; 32],
                operation: [67; 16],
                idempotency_key: [67; 16],
                physical_tail: PhysicalTail::DataAfterBootstrap,
                state: AdoptedState::RecoveryRequired,
                library_revision: Some("r1".into()),
                home_slot: Some(2),
                drive_serial: "DRIVE-B".into(),
                identity_only: true,
                bootstrap_drive_compression: false,
                configured_drive_compression: false,
            });
        }
        v.replaced_bot = Some([99; 16]);
        out.push((name, vec![], Body::Reprovision(v), FoldError::InvalidSchema));
    }
    let defs = Definitions {
        pools: Bounded(vec![]),
        rules: Bounded(vec![]),
        cleaning_prefixes: Bounded(vec![]),
    };
    let config = |generation| {
        Body::ConfigurationAccepted(ConfigurationAccepted {
            generation,
            config_digest: codec::configuration_digest(&defs).unwrap(),
            definitions: defs.clone(),
        })
    };
    for (name, prefix, error) in [
        (
            "reprovision-stale-rule",
            vec![config(1), config(2)],
            FoldError::StaleConfiguration,
        ),
        (
            "reprovision-wrong-rule",
            vec![diagnostic()],
            FoldError::WrongCounterpart,
        ),
    ] {
        let Body::Reprovision(mut v) = provision(3, None) else {
            unreachable!()
        };
        v.pool_source = Some(PoolSource::Rule(DecisionRef {
            sequence: 3,
            record_uuid: Ids.next(3),
        }));
        out.push((name, prefix, Body::Reprovision(v), error));
    }
    let unordered = Bounded(vec![
        DiagnosticEntry("z".into(), DiagnosticValue::Unsigned(1)),
        DiagnosticEntry("a".into(), DiagnosticValue::Unsigned(2)),
    ]);
    let Body::FenceRaised(mut raised) = fence(1) else {
        unreachable!()
    };
    raised.cause = FenceCause::NativeLogBehindProjection;
    let valid_raise = Body::FenceRaised(raised.clone());
    raised.detail = unordered.clone();
    out.push((
        "fence-raised-detail-order",
        vec![],
        Body::FenceRaised(raised.clone()),
        FoldError::InvalidSchema,
    ));
    raised.detail = Bounded(vec![]);
    raised.at = Some(at(1, 0));
    out.push((
        "cause14-zero-native-sequence",
        vec![],
        Body::FenceRaised(raised),
        FoldError::InvalidSchema,
    ));
    for (name, prefix, forced, detail) in [
        (
            "fence-released-detail-order",
            valid_raise.clone(),
            true,
            unordered,
        ),
        (
            "release-detail-other-cause",
            fence(1),
            true,
            l1w_detail(vec![("basis", DiagnosticValue::Text("floor".into()))]),
        ),
        (
            "release-detail-not-forced",
            valid_raise,
            false,
            l1w_detail(vec![("basis", DiagnosticValue::Text("floor".into()))]),
        ),
    ] {
        out.push((
            name,
            vec![prefix],
            Body::FenceReleased(FenceReleased {
                fence: 3,
                evidence: "proof".into(),
                forced,
                detail,
            }),
            FoldError::InvalidSchema,
        ));
    }
    for (name, mode) in [("verdict-modes-zero", 0), ("verdict-modes-bit7", 128)] {
        let Body::VerificationVerdict(mut v) = verdict() else {
            unreachable!()
        };
        v.modes = mode;
        out.push((
            name,
            vec![],
            Body::VerificationVerdict(v),
            FoldError::InvalidSchema,
        ));
    }
    for (name, mode) in [("progress-modes-zero", 0), ("progress-modes-bit7", 128)] {
        let Body::VerificationProgress(mut v) = progress(1) else {
            unreachable!()
        };
        v.modes = mode;
        out.push((
            name,
            vec![],
            Body::VerificationProgress(v),
            FoldError::InvalidSchema,
        ));
    }
    for (name, tape_id, prefix, error) in [
        (
            "unreferenced-verdict-unknown-tape",
            9,
            vec![],
            FoldError::UnknownTape,
        ),
        (
            "unreferenced-verdict-retired-tape",
            1,
            vec![Body::Retire(Retire {
                tape: tape(1),
                reason: RetireReason::OperatorRequested,
                observed_tape_sequence: None,
                assignment_generation: 1,
                detail: None,
            })],
            FoldError::RetiredIdentity,
        ),
    ] {
        let Body::VerificationVerdict(mut v) = verdict() else {
            unreachable!()
        };
        v.at = None;
        v.tape = tape(tape_id);
        out.push((name, prefix, Body::VerificationVerdict(v), error));
    }
    let Body::VerificationVerdict(mut v) = verdict() else {
        unreachable!()
    };
    v.tape = tape(2);
    out.push((
        "verdict-tape-mismatch",
        vec![],
        Body::VerificationVerdict(v.clone()),
        FoldError::InvalidSchema,
    ));
    v.tape = tape(1);
    v.damaged = Bounded(vec![DamagedRange {
        partition: 0,
        start_lba: 1,
        end_lba: 2,
    }]);
    out.push((
        "complete-verdict-damaged",
        vec![],
        Body::VerificationVerdict(v),
        FoldError::InvalidSchema,
    ));
    for (name, entries) in [
        (
            "refusal-without-code",
            vec![("name", DiagnosticValue::Text("DriveFenced".into()))],
        ),
        (
            "refusal-code-zero",
            vec![
                ("code", DiagnosticValue::Unsigned(0)),
                ("name", DiagnosticValue::Text("unknown".into())),
            ],
        ),
        (
            "refusal-without-name",
            vec![("code", DiagnosticValue::Unsigned(12))],
        ),
    ] {
        out.push((
            name,
            vec![],
            Body::Diagnostic(Diagnostic {
                event: DiagnosticEvent::RefusalRecorded,
                subject_kind: "tape".into(),
                subject_id: None,
                detail: l1w_detail(entries),
            }),
            FoldError::InvalidSchema,
        ));
    }
    out
}

/// Mutate canonical maps only in the generator; freeze rejected wire boundaries with valid CRCs.
fn l1w_wire_rejections(root: &Path) {
    use ciborium::value::Value;
    let ready = Body::ReadinessObserved(ReadinessObserved {
        operation: [240; 16],
        endpoint: endpoint("L1-W"),
        tape: None,
        barcode: None,
        phase: ReadinessPhase::LoadDriveReadiness,
        state: ReadinessState::Planned,
        previous: None,
        started_unix_ns: NOW,
        limit_ms: None,
        evidence: None,
        load_intent: None,
        ready_generation: 1,
        result: ReadinessResult::Pending,
        scope: ReadinessScope::Endpoint,
        media: None,
        source_slot: None,
        run: None,
        cancel_source: None,
    });
    let alarm = Body::AlarmChanged(AlarmChanged {
        condition_key: "unknown-kind".into(),
        change: GenerationChange {
            expected: 0,
            next: 1,
        },
        action: AlarmAction::Raise(AlarmRaise {
            kind: AlarmKind::DriveSerialCollision,
            severity: Severity::Critical,
            reason: "fixture".into(),
        }),
        cause: None,
    });
    let Body::FenceReleased(release) = release(1) else {
        unreachable!()
    };
    for (name, b, field, replacement) in [
        (
            "readiness-phase-fifty-one",
            ready.clone(),
            4,
            Some(Value::Integer(51.into())),
        ),
        (
            "cancel-source-three",
            ready.clone(),
            17,
            Some(Value::Integer(3.into())),
        ),
        (
            "readiness-limit-bool",
            ready.clone(),
            8,
            Some(Value::Bool(false)),
        ),
        (
            "readiness-slot-overflow",
            ready.clone(),
            15,
            Some(Value::Integer(65536.into())),
        ),
        (
            "readiness-run-short",
            ready,
            16,
            Some(Value::Bytes(vec![1; 15])),
        ),
        (
            "reprovision-operation-short",
            provision(3, None),
            11,
            Some(Value::Bytes(vec![1; 15])),
        ),
        (
            "replaced-bot-short",
            provision(3, None),
            12,
            Some(Value::Bytes(vec![1; 15])),
        ),
        (
            "alarm-kind-thirteen",
            alarm,
            2,
            Some(Value::Integer(13.into())),
        ),
        (
            "fence-release-three-keys",
            Body::FenceReleased(release),
            3,
            None,
        ),
        (
            "fence-cause-fifteen",
            fence(1),
            2,
            Some(Value::Integer(15.into())),
        ),
        (
            "reprovision-source-two",
            provision(3, None),
            10,
            Some(Value::Array(vec![Value::Integer(2.into()), Value::Null])),
        ),
        (
            "verdict-outcome-three",
            verdict(),
            8,
            Some(Value::Integer(3.into())),
        ),
    ] {
        let dir = root.join("rejected").join(name);
        let (mut l, _) = open(&dir, &Present);
        body(&mut l, provision(1, None));
        let segment = l.state().segments[0].name.clone();
        drop(l);
        let mut s = Vec::new();
        wire::encode(&record(b.clone(), 2), &mut s).unwrap();
        let mut value: Value = ciborium::de::from_reader(s.as_slice()).unwrap();
        let Value::Map(top) = &mut value else {
            unreachable!()
        };
        let Value::Map(fields) = &mut top[1].1 else {
            unreachable!()
        };
        if let Some(replacement) = replacement {
            if name == "alarm-kind-thirteen" {
                let Value::Array(action) = &mut fields[field].1 else {
                    unreachable!()
                };
                let Value::Map(payload) = &mut action[1] else {
                    unreachable!()
                };
                payload[0].1 = replacement;
            } else {
                fields[field].1 = replacement;
            }
        } else {
            fields.remove(field);
        }
        s.clear();
        ciborium::ser::into_writer(&value, &mut s).unwrap();
        assert!(wire::decode(b.tag() as u16, &s).is_err(), "{name}");
        let mut frame = Vec::new();
        crate::log_frame::encode_frame(&mut frame, 1, b.tag() as u16, 2, &s, &[0; 32]).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(dir.join(segment))
            .unwrap()
            .write_all(&frame)
            .unwrap();
        assert!(
            matches!(
                replay(&dir, LOG, &Present).unwrap().refusal.unwrap().fault,
                RefusalKind::Bytes(codec::Fault::Cbor)
            ),
            "{name}"
        );
    }
}
