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
        Body::MediaGenerationSet(MediaGenerationSet {
            tape: tape(1),
            generation: LtoGen::Lto8,
            source: GenerationSource::Operator,
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
            operation: [9; 16],
            session_id: [10; 16],
            preview_digest: [11; 32],
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
    let raise = body(&mut l, fence(1));
    body(
        &mut l,
        Body::FenceReleased(FenceReleased {
            fence: raise.sequence,
            evidence: "settled".into(),
            forced: false,
        }),
    );
    let raise = body(
        &mut l,
        Body::FenceRaised(FenceRaised {
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
            fence: raise.sequence,
            evidence: "operator proof".into(),
            forced: true,
        }),
    );
    for attempt in 1..=3 {
        let i = body(&mut l, intent(attempt));
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
                    confirmation: if attempt == 1 {
                        None
                    } else {
                        Some(Confirmation::Readiness(1))
                    },
                }),
            );
        } else {
            body(
                &mut l,
                Body::InventoryReconciled(InventoryReconciled {
                    library_serial: "LIBA0001".into(),
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
    body(
        &mut l,
        Body::InventoryReconciled(InventoryReconciled {
            library_serial: "LIBA0001".into(),
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
            confirmation: Some(Confirmation::Inventory(1)),
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
            operation: [25; 16],
            endpoint: standalone.clone(),
            tape: Some(tape(3)),
            barcode: None,
            phase: ReadinessPhase::ReadinessPoll,
            state: ReadinessState::BecomingReady,
            previous: None,
            started_unix_ns: NOW,
            limit_ms: 1000,
            evidence: Some(evidence()),
            load_intent: Some(reference(&i)),
            ready_generation: 1,
            result: ReadinessResult::Pending,
        }),
    );
    body(
        &mut l,
        Body::ReadinessObserved(ReadinessObserved {
            operation: [25; 16],
            endpoint: standalone.clone(),
            tape: None,
            barcode: Some("A".into()),
            phase: ReadinessPhase::Ready,
            state: ReadinessState::Ready,
            previous: Some(reference(&r)),
            started_unix_ns: NOW,
            limit_ms: 1000,
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
    let mut prev = None;
    for action in [
        CleaningAction::Begin(Begin {
            trigger: CleanTrigger::Manual,
        }),
        CleaningAction::Advance(Advance {
            phase: CleanPhase::Cleaning,
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
                cartridge: Some(tape(3)),
                action,
            }),
        );
        prev = Some(reference(&r));
    }
    for action in [
        CleaningAction::Register(Register {
            barcode: "CLN000".into(),
            home_slot: 1,
        }),
        CleaningAction::Expire(Reason {
            reason: "used".into(),
        }),
    ] {
        body(
            &mut l,
            Body::CleaningRecorded(CleaningRecorded {
                run_id: "cartridge".into(),
                previous: None,
                drive_uuid: None,
                cartridge: Some(tape(3)),
                action,
            }),
        );
    }
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
fn generate(root: &Path) {
    all_records(&root.join("all-records"));
    review_replay_vectors(root);
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
    extra_rejections(root);
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
    let mut manifest=String::from("case\tsegments\tbytes\tsha256\tlog_uuid\tlast_sequence\tterminal_hash\ttail_or_error\tapplied_through\tpending\ttapes\topen_fences\tduplicates\tcontradictions\n");
    let mut frames = String::from("case\tsequence\ttag\tsegment\toffset\tend\trecord_hash\n");
    let mut readme=String::from("# Library log v1 corpus\n\nGenerated through the real append store. Run `LIBRARY_LOG_FIXTURE_EXPORT_DIR=/empty/path cargo test --offline -p remanence-state --lib reference_library_logs_regenerate_byte_exactly`. EOF-short or zero-filled final frames are absent. Byte faults and library contradictions stop replay; native contradictions are scoped.\n\n");
    for case in cases {
        let dir = root.join(&case);
        let view: Box<dyn TapeView> = if case == Path::new("queued-successor-fence") {
            Box::new(View::default())
        } else if case == Path::new("repaired-kind-scoped") {
            Box::new(View(BTreeMap::from([(
                (tape(1).uuid, 4),
                TapeLookup::Present {
                    kind: crate::tape_log::Tag::SessionEnded,
                    digest: [0xee; 32],
                },
            )])))
        } else if case == Path::new("pending-cross-log") {
            Box::new(real_view())
        } else if case == Path::new("rejected/pending-record-limit") {
            Box::new(View::default())
        } else {
            Box::new(Present)
        };
        let r = replay(&dir, LOG, view.as_ref()).unwrap();
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
        manifest += &format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
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
            r.fold.contradictions.len()
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
    let mut schema=String::from("# Library log v1 schema\n\nHeader: magic REMLIB\\0\\x01 (0..8), version u16 LE (8..10), log UUID b16 (10..26), first sequence u64 LE (26..34), previous hash b32 (34..66), valid ASCII date (66..76), CRC-64/XZ (76..84).\n\nFrame: prefix version:u16/tag:u16/structural_len:u32/payload_len:u32/sequence:u64, all LE; prefix CRC (20..28); structural CBOR; structural CRC; 32-byte record hash; payload CRC. Overhead 44 bytes, combined cap 64 MiB.\n\nrecord_hash = SHA-256(previous_hash || prefix20 || structural). First previous hash is zero. No CRC or hash covers itself. Structural map is {0: envelope, 1: body}. All keys and fields below are mandatory, with null for absent Option. TextValue/Text max 4096 UTF-8 bytes; LabelText max 32; Blob<N> and Bounded<T,N> enforce N before allocation. Pure enums are unsigned; unions are [code,payload], including null unit payloads.\n\n");
    schema += "| Header offset | Bytes | Field |\n|---:|---:|---|\n| 0 | 8 | REMLIB magic |\n| 8 | 2 | Version 1 |\n| 10 | 16 | Log UUID |\n| 26 | 8 | First sequence |\n| 34 | 32 | Previous segment hash |\n| 66 | 10 | ASCII date |\n| 76 | 8 | CRC of bytes 0..76 |\n\n| Frame offset | Bytes | Field |\n|---:|---:|---|\n| 0 | 2 | Version 1 |\n| 2 | 2 | Tag |\n| 4 | 4 | Structural length S |\n| 8 | 4 | Payload length 32 |\n| 12 | 8 | Sequence |\n| 20 | 8 | Prefix CRC |\n| 28 | S | Structural CBOR |\n| 28+S | 8 | Structural CRC |\n| 36+S | 32 | Record hash |\n| 68+S | 8 | Payload CRC |\n\nAuditActor is map {0: kind uint (0 System, 1 User, 2 Service), 1: name text4096|null}; name is null exactly for System. Numeric Rust scalars map to CBOR unsigned integers with their declared bit width; Id is b16, Hash b32, Text text4096, LabelText text32. Pool watermarks preserve f64 bits as unsigned u64. Nested named types are maps in the tables below.\n\n";
    for (name, fields) in wire::SCHEMA_FIELDS {
        schema += &format!(
            "## {name}\n\n| Key | Field | CBOR type / bound | Optional |\n|---:|---|---|---|\n"
        );
        for (i, (field, ty)) in fields.iter().enumerate() {
            schema += &format!("| {i} | {field} | `{ty}` | {} |\n", ty.contains("Option"));
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
    schema += "## Replay and pending\n\nTags 6, 7, 13, 14, 15 and 26 depend on TapeAt; tag 15 requires native FinalizationPrepared. Present has both kind and digest. NotYet queues all touched scopes in source order, except FenceRaised always applies. resolve retries to a fixed point. Byte faults and library FoldError stop at the previous authoritative prefix. TapeContradiction isolates its tape and continues. applied_through never crosses pending.\n";
    schema += "\nLibraryHistory first uses retained state; otherwise the store binary-searches existing segment headers and scans one segment. DecisionRef binds sequence and UUID; only FenceId wire references may use sequence-only lookup in the header-bound log. Replay sees only lower sequences. FOLD-VECTORS.json freezes ReleaseWithoutFence with an explicitly empty injected history: a contiguous store log cannot lack an earlier sequence, and a diagnostic at that sequence is WrongCounterpart. I/O faults are HistoryUnavailable. Memory is O(resources), plus lifetime idempotency key/value payload at 80 bytes per keyed operation (SHA-256 of length-prefixed actor_fingerprint, u16 kind, and idempotency_key maps to operation id and request_fingerprint; allocator/map overhead is measured separately), plus pending up to 4096 records and 64 MiB of canonical CBOR envelopes/bodies, plus one looked-up frame. Pending bytes use exact-length boxed storage. Exceeding either total refuses with PendingLimitExceeded { scope, records, bytes }, preserving every prior pending record. Append to a blocked scope at capacity returns the same typed error.\n";
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
        v.at = at(0xa1, 9);
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
            operation: [100; 16],
            session_id: [100; 16],
            preview_digest: [100; 32],
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
    assert_eq!(r.fold.contradictions.len(), 1);
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
            "second-outcome-parented",
            vec![intent(1), done.clone()],
            second,
            FoldError::OutcomeWithoutOpenIntent,
        ),
        (
            "inventory-resolved-parented",
            vec![intent(1), done],
            Body::InventoryReconciled(InventoryReconciled {
                library_serial: "LIBA0001".into(),
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
                library_serial: "LIBA0001".into(),
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
                library_serial: "LIBB0002".into(),
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
    for (name, prefix, bad, expected) in negative_histories() {
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
        ("diagnostic-event-twenty", diagnostic(), 0, 20),
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

const FOLD_SCHEMA_NOTES:&str="## Record tags, causal keys and scopes\n\n| Tag | Record | Scopes | Causal rule |\n|---:|---|---|---|\n| 1 | AssignPool | Tape, Config | `change.expected` = the tape's `assignment_generation`. The tape is provisioned and not retired. `Rule` requires `configuration` to name the latest retained `ConfigurationAccepted`; an older reference is `StaleConfiguration`, including an identical retry. History classifies a mismatching reference: only an older ConfigurationAccepted is StaleConfiguration; another kind or absent reference is WrongCounterpart. `pool`, if present, is non-empty |\n| 2 | Label | Tape (both holders), Label (old and new) | `change` against the tape's `label_generation`. Labels are unique among live tapes. If the label is held by live tape X, `prior_holder` must name X with X's current `label_generation`. The one record moves the label, clears X's label and bumps X's generation. If the label is free, `prior_holder` is null. `observed_bot_uuid`, if present, equals `tape.uuid`. **Recovery 4** |\n| 3 | Retire | Tape, Label | `assignment_generation` equals the current one. The tape is live. Retire ends the incarnation for good and releases its label and pool. `observed_tape_sequence` is evidence, never a prerequisite. No later record except `Diagnostic` may name a retired `TapeRef` (`RetiredIdentity`) |\n| 4 | Reprovision | Tape (new and predecessor), Label | A new UUID, never seen before. `predecessor`, if present, is retired. The label, if any, is free. `label_provenance` is null exactly when `label` is. `adoption` is present exactly for `Adopt`. Sets assignment, label and media generations to 1 in one record; no separate `Label` or `AssignPool` for the initial state |\n| 5 | MediaGenerationSet | Tape | `change` against the tape's `media_revision`. The tape is live |\n| 6 | SupersedeTail | Tape, Operation | **Pending dependency** on `at`. At most one per (`at.tape`, `session_id`). B4b's tape fold checks the `SessionEnded.crash_ack_audit_sequence` that names it (kind, tape, session, count); B2 provides `LibraryFold::supersede(sequence)` for that lookup |\n| 7 | AcceptUnprotectedFinalization | Tape, Operation | **Pending** on `at`. `w < t`. At most one per (incarnation, operation). Codec, fold and fixtures exist now. **No production constructor**; step 6 adds the emitter |\n| 8 | FenceRaised | Tape or Endpoint, Operation | `Write` requires tape scope; `DriveFault` requires drive scope (`FenceScopeMismatch`). The tape is live. A replay fence behind queued state takes effect immediately; its live-tape check runs in queue order and failure becomes a scoped contradiction. The fence id is this record's sequence. No deduplication: every raise is its own fence. **`at` is evidence, not a dependency:** the fence applies at once, and an unresolvable `at` is reported, never waited for |\n| 9 | FenceReleased | raised fence scopes | `fence` remains a u64 FenceId: the raise sequence within the header-bound log UUID. Only this reference form uses sequence-only LibraryHistory lookup; DecisionRef always matches the record UUID. An earlier raise no longer open is `FenceAlreadyReleased`, even after later fences; another tag is `WrongCounterpart`; absent history is `ReleaseWithoutFence`. Zero/forward references remain `ForwardReference`. The latest identical release retry remains a no-op. `forced` changes nothing in the fold but the fence: it cannot resolve a motion, a pending record or an ambiguity |\n| 10 | MotionIntent | Tape, Endpoint, Library, Operation | `command` is unique among open intents. **The LOAD attempt count lives only here** (item 6): `attempt` is 1 plus the number of earlier `Load` intents with the same `parent_operation` and endpoint; with no parent it is 1. An intent with no outcome makes its library (a `Bay` endpoint's serial, a `Move`'s serial, or a standalone endpoint) **InventoryUnknown** |\n| 11 | MotionOutcome | intent scopes | `intent` names an open `MotionIntent` (`OutcomeWithoutOpenIntent`; `WrongCounterpart` if another tag). One outcome per intent. `Done` requires a confirmation: Inventory for Move and Unload, Readiness for Load. `Unknown` leaves the library InventoryUnknown |\n| 12 | InventoryReconciled | Library, resolved intent scopes | `observation_generation` strictly increases per library. `resolves` names intents of this library whose outcome is `Unknown` or absent. A partial poll (`complete = false`) may resolve only intents whose elements it lists. It clears exactly the named uncertainty and creates no placement ownership |\n| 13 | VerificationProgress | Tape, Operation | **Pending** on `at`. Within one operation, `at`, `plan_digest` and `modes` never change (`CoverageChanged`). `parts_done` strictly increases and `next` strictly advances in lexicographic order (`ProgressRegressed`). `next.part = parts_done` (`ImpossibleCursor`). Progress is never a verdict. **No production constructor in step 4** (item 7); step 7 emits it |\n| 14 | VerificationVerdict | Tape, Operation, progress scopes | **Pending** on `at`. Healthy means `damaged` is empty and `unprotected` is null; findings may combine. Each range has `start < end`; `w < t`. If the operation has progress records, `progress` must name the last one, with the same `at` (`VerdictWithoutCoverage`). It supersedes that operation's progress only. A verdict at tape sequence N says nothing about N+1 |\n| 15 | ManualFinalizationAccepted | Tape, Operation | **Pending** on `at`, which names the tape log's `FinalizationPrepared` record; its digest binds the immutable plan, so there is no separate plan digest. `assignment_generation` equals the tape's current one (`GenerationMismatch`; item 9's \"Accepted with a stale generation\"). One per incarnation. Retired in step 6 |\n| 16 | ConfigurationAccepted | Config | `generation` is the previous one plus 1, starting at 1. `config_digest` = SHA-256 of the canonical bytes of `definitions` (`ConfigDigest`). It holds only catalog-relevant fields, never a secret or a TLS key. Assignments still need `AssignPool`; replay never reinterprets an older config |\n| 17 | ReadinessObserved | Tape, Endpoint, Operation, previous and intent scopes | `previous` names the last `ReadinessObserved` of the same operation, and is null for its first. `load_intent`, if present, names an open or resolved `Load` intent on the same endpoint; resolved records use LibraryHistory. **It carries no LOAD count** (item 6). `ready_generation` strictly increases per endpoint. A terminal `result` closes the operation |\n| 18 | DriveObserved | Drive, Endpoint | The first observation creates the drive with `stewardship_generation = 1`. The observation time is the envelope's |\n| 19 | DriveDecision | Drive | The drive is known. `change` against `stewardship_generation`. Retire requires active; Reinstate requires retired. Fencing is tags 8 and 9, never this |\n| 20 | DriveHealthObserved | Drive, Tape, Operation | The drive is known. A non-null (`session_id`, `trigger`) matching the drive's latest retained snapshot is a no-op if the body is identical, otherwise `ConflictingDuplicate`. The fold makes no lifetime uniqueness claim. B4's health projector must retain the (`session_id`, `trigger`) unique key with first-wins: ignore an identical later body; report a different later body as a projection conflict and never apply it. Null-session snapshots are independent. Counters are cumulative per mount |\n| 21 | CleaningRecorded | CleaningRun, Drive, Tape | `previous` chains a run's records. At most one active run per drive and per cartridge (today's unique indexes). Register and Expire concern the cartridge, not a run. **The fold never derives a fence from cleaning** (item 11): a run that needs an operator is a separate `FenceRaised(DriveFault)` |\n| 22 | AlarmChanged | Alarm, cause scopes | `change` against the alarm generation (0 when never raised). Raise requires absent or cleared; Acknowledge requires raised and unacknowledged, with an envelope actor other than System (the acknowledger is the actor, recorded once); Clear requires raised. `cause` may name any earlier record, using LibraryHistory after replacement |\n| 23 | OperationRecorded | Operation | `previous` chains the operation's records. Accepted comes first or not at all. Started follows nothing or Accepted. Cancel records follow a non-terminal state. Finished, Failed, CancelledBeforeDispatch, CompletedAfterCancel and CompletionUnknown are terminal; nothing follows them. The idempotency scope (`actor_fingerprint`, `kind`, `idempotency_key`) is unique: a second Accepted for that scope with a different operation or fingerprint is a `ConflictingDuplicate`. **No progress variant**: progress gates nothing in step 4 and stays `Diagnostic` |\n| 24 | SessionObserved | Operation(session), Tape, Endpoint | Opened comes first; the other events are terminal. **A write session cannot be expressed**: write sessions are the tape log's (item 5b), and kind 0 is refused. It has no receipts, because durable receipts are tape-log only |\n| 25 | WrapMapCached | Tape | `digest` = SHA-256(`descriptors`), whose bytes are today's descriptor serialisation, opaque to the fold. Only digest and calibration_generation are retained per tape; the projector reads descriptor bytes from the source record. `calibration_generation` never decreases per tape. The RAO control store (the third authority) still decides servability; this rebuilds only the SQL cache |\n| 26 | RecoveryRecorded | Tape, Operation, prior scopes | **Pending** on `at`. RepairApplied requires `retained_through = at.sequence`. **`AppendRecoveryRequired` does not exist** (item 5c): action 0 is refused. Missing native proof stays pending. `prior` must name an earlier RecoveryRecorded, including one replaced in retained state |\n| 32 | Diagnostic | none | Detail keys are strictly ascending and unique (InvalidSchema); a nested value is refused at encode. **The fold validates and ignores it.** `DiagnosticEvent` is B2's own closed enum (below) |\n\nAll generation failures use GenerationMismatch. Shape violations use InvalidSchema; pair/type failures use WrongCounterpart or the named rule error. Digest mismatch is scoped TapeContradiction and is checked first; only a matching digest with wrong kind is CounterpartKind. Tag 3 adds mandatory key 4, detail: text <=4096 bytes or null; it is presentation only. Tag 21 is removed from production orchestration in step 5; its historical decoder remains.\n\n### Additional wire conventions\n\nId is b16; Hash is b32; Text/TextValue is UTF-8 text <=4096 bytes; LabelText is UTF-8 text <=32 bytes. Blob<N> is a CBOR byte string <=N bytes; Bounded<T,N> is a definite array <=N entries. Integer types are unsigned, except Negative, the diagnostic i64 <0. Struct names are mandatory ascending integer-keyed maps. Unit is null. DiagnosticEntry is [text,DiagnosticValue]. AuditActor is {0: kind (System=0, User=1, Service=2), 1: name (null exactly for System)}. Pool watermark fields encode the existing f64 IEEE-754 bits as u64, preserving exact configuration values without CBOR floats; they are not text.\n";

fn optional_forms(l: &mut LibraryLog, config: DecisionRef) {
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
        v.media = None;
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
            library_serial: "LIBB0002".into(),
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
/// Positive historical references are store-written; negatives append a valid-CRC
/// frame only after checking that live preflight refuses the exact same error.
fn historical_vectors(root: &Path) {
    for (name, prefix, candidate, expected) in historical_cases().into_iter().chain(health_cases())
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
        "candidate_frame_hex": hex(&candidate_bytes), "expected": "ReleaseWithoutFence"
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
            operation: [3; 16],
            session_id: [3; 16],
            preview_digest: [0; 32],
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
    let mut r = replay(&dir, LOG, &View::default()).unwrap();
    assert!(r.refusal.is_none());
    assert_eq!(r.fold.open_fences(), 1);
    assert_eq!(r.fold.pending.len(), 4);
    r.fold.resolve(&Present, &History::default()).unwrap();
    assert!(r.fold.pending.is_empty());
    assert!(r.fold.contradictions.is_empty());
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
    let repaired = View(BTreeMap::from([(
        (tape(1).uuid, 4),
        TapeLookup::Present {
            kind: crate::tape_log::Tag::SessionEnded,
            digest: [0xee; 32],
        },
    )]));
    let r = replay(&dir, LOG, &repaired).unwrap();
    assert!(r.refusal.is_none());
    assert_eq!(
        r.fold.contradictions.get(&fold::Scope::Tape(tape(1).uuid)),
        Some(&FoldError::TapeContradiction)
    );
    assert_eq!(r.fold.tapes[&tape(2).uuid].assignment_generation, 2);
}
