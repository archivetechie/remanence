//! Real-file continuation pipeline, including disk preflight, provisional data sync, closing fsync, frozen
//! validation, structural skipping and complete streamed catalog comparisons.
#[path = "support/heap.rs"]
mod heap;
#[path = "support/tape_log_stream.rs"]
#[allow(dead_code)]
mod stream;
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
use remanence_state::{
    authority::{CatalogPayload, TapeAuthorityView},
    tape_log::{
        authority::TapeAuthority,
        payload::{encode_header, encode_member},
        store::{Header, Store},
    },
};
use sha2::{Digest, Sha256};
use std::time::Instant;
struct HashWriter<'a>(&'a mut Sha256);
impl std::io::Write for HashWriter<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn run(members: u64) {
    let _lock = TEST_LOCK.lock().unwrap();
    let initial_live = heap::reset_peak();
    remanence_state::tape_log::measure::reset();
    let started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new_fixture(
        dir.path(),
        Header {
            tape_uuid: [7; 16],
            block_size: 262144,
            scheme: None,
        },
    )
    .unwrap();
    let authority = TapeAuthority::open_fixture(dir.path(), [7; 16]).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    let report = stream::semantic_report();
    let mut expected_digest = Sha256::new();
    encode_header(
        &mut HashWriter(&mut expected_digest),
        &report.payloads[0].projection,
    )
    .unwrap();
    writer
        .publish_report_streaming(&report, &mut |object, emit| {
            assert_eq!(object, 0);
            for index in 0..members {
                let member = stream::semantic_member(index);
                encode_member(
                    &mut HashWriter(&mut expected_digest),
                    &report.payloads[0].projection,
                    &member,
                )
                .map_err(remanence_state::tape_log::store::StoreError::from)?;
                emit(&member)?;
            }
            Ok(())
        })
        .unwrap();
    let publish_peak = heap::peak();
    assert!(
        publish_peak - initial_live < 2 * 1024 * 1024,
        "publish_report retained member history"
    );
    let latest_live = heap::reset_peak();
    let before_latest = remanence_state::tape_log::measure::counts();
    let latest = writer.latest_commit().unwrap().unwrap();
    let latest_peak = heap::peak() - latest_live;
    assert_eq!(latest.total, 3);
    assert_eq!(latest.object_count, 1);
    assert!(latest.payloads.is_empty());
    assert!(latest_peak < 64 * 1024, "latest_commit decoded a catalog");
    assert_eq!(
        remanence_state::tape_log::measure::counts().full_frame_bytes,
        before_latest.full_frame_bytes
    );
    drop(writer);
    let cold_live = heap::reset_peak();
    assert_eq!(authority.latest_commit().unwrap(), Some(latest));
    let cold_latest_peak = heap::peak() - cold_live;
    assert!(
        cold_latest_peak < 2 * 1024 * 1024,
        "cold latest_commit retained a catalog"
    );
    let capture = Instant::now();
    let snapshot = store.snapshot().unwrap();
    assert!(snapshot.capture_time() < std::time::Duration::from_millis(100));
    let capture_us = snapshot.capture_time().as_micros();
    let metrics = snapshot.replay_metrics();
    assert!(metrics.peak_frame_buffer < 128 * 1024);
    let frames = metrics.frames;
    assert_eq!(metrics.peak_extent_capacity, 0);
    assert!(metrics.peak_catalog_bytes < 4096);
    let structural = metrics.structural_bytes;
    assert_eq!(
        snapshot.captured_end(),
        51 + structural
            + metrics.payload_bytes
            + metrics.frames * remanence_state::tape_log::FRAME_OVERHEAD as u64
    );
    let validation_ms = capture.elapsed().as_millis();
    let mut rows = 0;
    snapshot
        .visit_file_rows(|_| {
            rows += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(rows, 2);
    let mut actual_digest = Sha256::new();
    snapshot
        .visit_frames(|frame| {
            actual_digest.update(frame.payload);
            Ok(())
        })
        .unwrap();
    assert_eq!(actual_digest.finalize(), expected_digest.finalize());
    let mut count = 0u64;
    let mut objects = 0;
    let mut ends = 0;
    let mut reports = 0;
    let visit_live = heap::reset_peak();
    authority
        .visit_reports(&mut |view| {
            reports += 1;
            assert_eq!(view.summary().object_count, 1);
            assert_eq!(view.commit().group.member_count, members);
            view.visit_payloads(&mut |event| {
                match event {
                    CatalogPayload::Object(header) => {
                        objects += 1;
                        assert_eq!(header.object.object_id, "o");
                        assert_eq!(header.copy.tape_uuid, [7; 16]);
                    }
                    CatalogPayload::Member(member) => {
                        assert_eq!(member.file_id, format!("{count:016x}"));
                        assert_eq!(member.object_id, "o");
                        assert_eq!(member.path, "p");
                        assert_eq!(member.size_bytes, 12);
                        assert_eq!(member.file_sha256, [9; 32]);
                        assert_eq!(member.first_chunk_lba, Some(0));
                        assert_eq!(member.chunk_count, 1);
                        assert_eq!(member.mtime, None);
                        assert_eq!(member.executable, None);
                        count += 1;
                    }
                    CatalogPayload::ObjectEnd => ends += 1,
                }
                Ok(())
            })
        })
        .unwrap();
    assert_eq!(reports, 1);
    let visit_peak = heap::peak() - visit_live;
    assert!(
        visit_peak < 2 * 1024 * 1024,
        "visit_reports retained member history"
    );
    assert_eq!((objects, ends, count), (1, 1, members));
    // Exercise the actual authority and terminal-source adapters at the same
    // scale: their structural passes must skip the ten-million-member catalog.
    use remanence_parity::authority::{TerminalAuthoritySource, TerminalPrefixMode};
    use remanence_parity::TapeIndexReplicaRecordSource;
    let authority =
        remanence_state::tape_log::authority::TapeAuthority::open_fixture(dir.path(), [7; 16])
            .unwrap();
    let mut terminal = authority
        .terminal_source(None, TerminalPrefixMode::Absent)
        .unwrap();
    assert_eq!(terminal.summary().counts.object_row_count, 1);
    let mut physical = 0;
    terminal
        .visit_structural_entries(&mut |_| {
            physical += 1;
            Ok(())
        })
        .unwrap();
    let mut identities = 0;
    terminal
        .visit_object_rows(&mut |_| {
            identities += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!((physical, identities), (2, 1));
    let rss = std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|s| s.starts_with("VmHWM:"))
        .unwrap()
        .to_owned();
    eprintln!("heap publish_peak_delta={} latest_peak_delta={latest_peak} cold_latest_peak_delta={cold_latest_peak} visit_peak_delta={visit_peak}", publish_peak - initial_live);
    let counts = remanence_state::tape_log::measure::counts();
    let log_metrics = snapshot.replay_metrics();
    let frame_bytes = log_metrics.structural_bytes
        + log_metrics.payload_bytes
        + log_metrics.frames * remanence_state::tape_log::FRAME_OVERHEAD as u64;
    eprintln!(
        "counted_passes full={:.6} structural={:.6}",
        counts.full_pass_equivalents(frame_bytes),
        counts.row_pass_equivalents(frame_bytes)
    );
    eprintln!("disk catalog L={} S={structural} N=2 M={members} P=0 frames={frames} counts={:?} peak_RSS={rss} capture_us={capture_us} snapshot_validation_ms={validation_ms} elapsed_s={:.3}", std::fs::metadata(store.path()).unwrap().len(), remanence_state::tape_log::measure::counts(), started.elapsed().as_secs_f64());
}
#[test]
fn disk_large_member_catalog() {
    run(100_000);
}
#[test]
#[ignore = "explicit 10-million-member disk streaming and full payload comparison"]
fn disk_ten_million_members() {
    run(10_000_000);
}
