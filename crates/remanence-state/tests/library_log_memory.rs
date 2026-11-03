//! Measured streaming replay over a fixed resource set, using the real append store.
#[path = "support/heap.rs"]
mod heap;
use remanence_state::library_log::{store::*, *};
use std::{
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
static MEMORY_MEASUREMENT: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct ClockSource(AtomicU64);
impl Clock for ClockSource {
    fn now(&self) -> u64 {
        self.0.fetch_add(1_000_000_000, Ordering::Relaxed)
    }
}
struct Uuids;
impl UuidSource for Uuids {
    fn next(&self, n: u64) -> Id {
        let mut b = [0; 16];
        b[8..].copy_from_slice(&n.to_be_bytes());
        b
    }
}
struct Absent;
impl TapeView for Absent {
    fn lookup(&self, _: &TapeAt) -> TapeLookup {
        TapeLookup::NotYet
    }
}
fn tape(n: u8) -> TapeRef {
    TapeRef {
        uuid: [n; 16],
        incarnation: 1,
    }
}
fn write(dir: &Path, count: u64) {
    let mut l = LibraryLog::open(
        dir,
        [0xb1; 16],
        Origin {
            host: "memory".into(),
            pid: 1,
            build: "fixture".into(),
        },
        Box::new(ClockSource(AtomicU64::new(1_793_664_000_000_000_000))),
        Box::new(Uuids),
        false,
        &Absent,
    )
    .unwrap();
    for n in 1..=10 {
        l.fixture_append(
            Body::Reprovision(Reprovision {
                tape: tape(n),
                predecessor: None,
                mode: ProvisionMode::Initialize,
                label: None,
                label_provenance: None,
                parity_scheme: None,
                medium: MediumSpec {
                    family: MediumFamily::Lto(LtoGen::Lto9),
                    usable_logical_bytes: 18_000_000_000_000,
                    record_size: 1048576,
                    rerecord: false,
                    lifecycle_ops: false,
                    source: GenerationSource::Measured,
                },
                pool: None,
                endpoint: None,
                adoption: None,
            }),
            AuditActor::System,
            &Absent,
        )
        .unwrap();
    }
    for n in 1..=2 {
        l.fixture_append(
            Body::DriveObserved(DriveObserved {
                drive_uuid: [n; 16],
                serial: None,
                identity_source: IdentitySource::Standalone,
                endpoint: None,
                managed: Managed::Rem,
                inventory_generation: 1,
                vendor: None,
                product: None,
                firmware_rev: None,
            }),
            AuditActor::System,
            &Absent,
        )
        .unwrap();
    }
    for n in 1..=3 {
        l.fixture_append(
            Body::AlarmChanged(AlarmChanged {
                condition_key: format!("alarm{n}").as_str().into(),
                change: GenerationChange {
                    expected: 0,
                    next: 1,
                },
                action: AlarmAction::Raise(AlarmRaise {
                    kind: AlarmKind::SnapshotPersistFailing,
                    severity: Severity::Warning,
                    reason: "measurement".into(),
                }),
                cause: None,
            }),
            AuditActor::System,
            &Absent,
        )
        .unwrap();
    }
    // The measurement concerns replay, not 100,000 fsync latencies. Seed resources
    // through the real store, then stream the repetitive suffix through its codec.
    let mut hash = l.state().terminal_hash;
    let path = dir.join(&l.state().segments[0].name);
    drop(l);
    use std::io::Write;
    let mut output =
        std::io::BufWriter::new(std::fs::OpenOptions::new().append(true).open(path).unwrap());
    for i in 0..count - 15 {
        let g = i / 10 + 1;
        let n = i + 16;
        let record = Record {
            envelope: Envelope {
                record_uuid: Uuids.next(n),
                timestamp_unix_ns: 1_793_664_000_000_000_000,
                host_id: "memory".into(),
                process_id: 1,
                software_build: "fixture".into(),
                actor: AuditActor::System,
                source_layer: SourceLayer::Layer4,
                operation_id: None,
                session_id: None,
                idempotency_key: None,
            },
            body: Body::AssignPool(AssignPool {
                tape: tape((i % 10 + 1) as u8),
                pool: None,
                change: GenerationChange {
                    expected: g,
                    next: g + 1,
                },
                source: AssignmentSource::Operator,
                configuration: None,
            }),
        };
        let (bytes, next) = codec::encode(n, hash, &record).unwrap();
        output.write_all(&bytes).unwrap();
        hash = next;
    }
    output.flush().unwrap();
}
/// Tenfold record growth retains the same ten tapes, two drives and three alarms.
#[test]
fn library_replay_memory_is_per_resource() {
    let _measurement = MEMORY_MEASUREMENT.lock().unwrap();
    let mut peaks = Vec::new();
    let mut capped_peaks = Vec::new();
    for n in [10_000, 100_000] {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), n);
        let baseline = heap::reset_peak();
        let r = replay(d.path(), [0xb1; 16], &Absent).unwrap();
        let peak = heap::peak().saturating_sub(baseline);
        assert!(r.refusal.is_none(), "{:?}", r.refusal);
        assert_eq!(r.last_sequence, n);
        assert_eq!(r.fold.tapes.len(), 10);
        assert!(r.fold.pending.is_empty());
        assert!(r.decoder_peak_bytes < 4096);
        assert!(peak < 512 * 1024, "{n}: {peak} bytes");
        eprintln!(
            "records={n} peak_live_delta={peak} decoder_buffer={}",
            r.decoder_peak_bytes
        );
        peaks.push(peak);
        // Repeat the same fixed-resource workload with exactly 4096 pending
        // records. Canonical pending bytes are bounded separately from metadata.
        let path = d.path().join(&r.segments.last().unwrap().name);
        let mut hash = r.terminal_hash;
        drop(r);
        use std::io::Write;
        let mut output =
            std::io::BufWriter::new(std::fs::OpenOptions::new().append(true).open(path).unwrap());
        for i in 1..=MAX_PENDING_RECORDS {
            let sequence = n + i as u64;
            let record = Record {
                envelope: Envelope {
                    record_uuid: Uuids.next(sequence),
                    timestamp_unix_ns: 1_793_664_000_000_000_000,
                    host_id: "memory".into(),
                    process_id: 1,
                    software_build: "fixture".into(),
                    actor: AuditActor::System,
                    source_layer: SourceLayer::Layer4,
                    operation_id: None,
                    session_id: None,
                    idempotency_key: None,
                },
                body: Body::VerificationProgress(fixture_progress(
                    [99; 16],
                    TapeAt {
                        tape: tape(1),
                        sequence: 1,
                        record_digest: [0; 32],
                    },
                    [0; 32],
                    1,
                    i as u32,
                    VerificationCursor {
                        partition: 0,
                        file: i as u64,
                        lba: i as u64,
                        ordinal: i as u64,
                        part: i as u32,
                    },
                    [0; 32],
                )),
            };
            let (bytes, next) = codec::encode(sequence, hash, &record).unwrap();
            output.write_all(&bytes).unwrap();
            hash = next;
        }
        output.flush().unwrap();
        drop(output);
        let baseline = heap::reset_peak();
        let r = replay(d.path(), [0xb1; 16], &Absent).unwrap();
        let peak = heap::peak().saturating_sub(baseline);
        assert!(r.refusal.is_none(), "{:?}", r.refusal);
        assert_eq!(r.fold.pending.len(), MAX_PENDING_RECORDS);
        assert_eq!(r.fold.applied_through, n);
        assert_eq!(r.last_sequence, n + MAX_PENDING_RECORDS as u64);
        assert!(r.fold.pending_bytes() <= MAX_PENDING_BYTES);
        assert!(peak < 16 * 1024 * 1024, "{peak}");
        assert!(r.decoder_peak_bytes < 4096);
        eprintln!(
            "records={n} pending={} pending_bytes={} peak_live_delta={peak} decoder_buffer={}",
            r.fold.pending.len(),
            r.fold.pending_bytes(),
            r.decoder_peak_bytes
        );
        capped_peaks.push(peak);
    }
    assert!(
        capped_peaks[1] <= capped_peaks[0] + 32 * 1024,
        "{capped_peaks:?}"
    );
    assert!(peaks[1] <= peaks[0] + 32 * 1024, "peaks: {peaks:?}");
}

/// Streams operations and opaque maps without retaining the source workload in RAM.
fn write_operations(dir: &Path, count: u64) {
    use sha2::{Digest, Sha256};
    use std::io::Write;
    let l = LibraryLog::open(
        dir,
        [0xb1; 16],
        Origin {
            host: "memory".into(),
            pid: 1,
            build: "fixture".into(),
        },
        Box::new(ClockSource(AtomicU64::new(1_793_664_000_000_000_000))),
        Box::new(Uuids),
        false,
        &Absent,
    )
    .unwrap();
    drop(l);
    let header = codec::Header {
        log_uuid: [0xb1; 16],
        first_sequence: 1,
        previous_segment_hash: [0; 32],
        date: *b"2026-11-03",
    };
    let mut out = std::io::BufWriter::new(std::fs::File::create(dir.join(header.name())).unwrap());
    out.write_all(&header.encode().unwrap()).unwrap();
    let mut hash = [0; 32];
    let mut sequence = 0;
    let mut emit = |body: Body| {
        sequence += 1;
        let r = Record {
            envelope: Envelope {
                record_uuid: Uuids.next(sequence),
                timestamp_unix_ns: 1_793_664_000_000_000_000,
                host_id: "memory".into(),
                process_id: 1,
                software_build: "fixture".into(),
                actor: AuditActor::System,
                source_layer: SourceLayer::Layer4,
                operation_id: None,
                session_id: None,
                idempotency_key: None,
            },
            body,
        };
        let (bytes, next) = codec::encode(sequence, hash, &r).unwrap();
        out.write_all(&bytes).unwrap();
        hash = next;
        r.reference(sequence)
    };
    for n in 1..=100 {
        emit(Body::Reprovision(Reprovision {
            tape: tape(n),
            predecessor: None,
            mode: ProvisionMode::Initialize,
            label: None,
            label_provenance: None,
            parity_scheme: None,
            medium: MediumSpec {
                family: MediumFamily::Lto(LtoGen::Lto9),
                usable_logical_bytes: 18_000_000_000_000,
                record_size: 1048576,
                rerecord: false,
                lifecycle_ops: false,
                source: GenerationSource::Measured,
            },
            pool: None,
            endpoint: None,
            adoption: None,
        }));
        let descriptors = vec![n; 64 * 1024];
        let digest = Sha256::digest(&descriptors).into();
        emit(Body::WrapMapCached(WrapMapCached {
            tape: tape(n),
            calibration_generation: 1,
            action: WrapAction::Cache(Cache {
                descriptors: Blob(descriptors),
                mapped_extent_lba: 1,
                write_epoch: 1,
                digest,
            }),
        }));
    }
    for i in 1..=count {
        let operation = Uuids.next(i);
        let first = emit(Body::OperationRecorded(OperationRecorded {
            operation,
            kind: OperationKind::MediaReadinessWait,
            previous: None,
            event: OperationEvent::Accepted(Accepted {
                actor_fingerprint: "actor".into(),
                idempotency_key: (i % 2 == 0).then_some(operation),
                request_fingerprint: [7; 32],
                subject: None,
            }),
        }));
        let ready = ReadinessObserved {
            operation,
            endpoint: DriveKey::Bay(Bay {
                library_serial: "LIB".into(),
                bay: 1,
            }),
            tape: None,
            barcode: None,
            phase: ReadinessPhase::ReadinessPoll,
            state: ReadinessState::Ready,
            previous: None,
            started_unix_ns: 1,
            limit_ms: 1,
            evidence: None,
            load_intent: None,
            ready_generation: i * 2 - 1,
            result: ReadinessResult::Pending,
        };
        let prior = emit(Body::ReadinessObserved(ready.clone()));
        emit(Body::ReadinessObserved(ReadinessObserved {
            previous: Some(prior),
            ready_generation: i * 2,
            result: ReadinessResult::Ready,
            ..ready
        }));
        emit(Body::OperationRecorded(OperationRecorded {
            operation,
            kind: OperationKind::MediaReadinessWait,
            previous: Some(first),
            event: OperationEvent::Finished(Response {
                response_fingerprint: None,
            }),
        }));
    }
    out.flush().unwrap();
}

/// Terminal unkeyed operations retain nothing; keyed operations retain only 80-byte pairs.
#[test]
fn library_replay_operations_and_wrap_maps_are_bounded() {
    let _measurement = MEMORY_MEASUREMENT.lock().unwrap();
    let mut costs = vec![];
    for operations in [10_000, 100_000] {
        let d = tempfile::tempdir().unwrap();
        write_operations(d.path(), operations);
        let baseline = heap::reset_peak();
        let r = replay(d.path(), [0xb1; 16], &Absent).unwrap();
        let peak = heap::peak().saturating_sub(baseline);
        assert!(r.refusal.is_none(), "{:?}", r.refusal);
        assert_eq!(r.fold.tapes.len(), 100);
        assert_eq!(r.fold.retained_records(), 100);
        assert_eq!(r.fold.idempotency_bytes(), operations as usize / 2 * 80);
        assert!(
            peak < r.fold.idempotency_bytes() * 2 + 2 * 1024 * 1024,
            "{peak}"
        );
        let probes = r.fold.lookup_count();
        let records = 4 * operations + 200;
        costs.push((probes, 4 * operations));
        eprintln!("operations={operations} keyed={} wrap_tapes=100 records={records} retained_records={} idempotency_payload={} peak_live_delta={peak} probes={probes} probes_per_record={:.4}", operations/2,
            r.fold.retained_records(), r.fold.idempotency_bytes(), probes as f64 / records as f64);
    }
    assert!(
        costs[1].0 * costs[0].1 <= costs[0].0 * costs[1].1,
        "{costs:?}"
    );
}
