//! Measured streaming replay over a fixed resource set, using the real append store.
#[path = "support/heap.rs"]
mod heap;
use remanence_state::library_log::{store::*, *};
use std::{
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
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
                block_size: 1048576,
                parity_scheme: None,
                media: None,
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
