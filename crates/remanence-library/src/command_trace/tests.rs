//! Hermetic recorder proofs: file safety, error fidelity, privacy and run state.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

struct Trace {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    recorder: Recorder,
    clock: Arc<AtomicU64>,
    dev: u64,
}

impl Trace {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trace.jsonl");
        let clock = Arc::new(AtomicU64::new(100));
        let mut recorder = Recorder::new(File::create_new(&path).unwrap(), 123, UNIX_EPOCH);
        recorder.clock = Some(Arc::clone(&clock));
        let dev = recorder.device();
        Self {
            _directory: directory,
            path,
            recorder,
            clock,
            dev,
        }
    }

    fn rows(&self) -> Vec<Value> {
        rows(&self.path)
    }

    fn issue(
        &self,
        dev: u64,
        cdb: &[u8],
        dir: Direction,
        n: usize,
        result: Result<usize, &ScsiError>,
        response: &[u8],
    ) {
        let command = self.recorder.begin(dev, cdb, dir, n, 5000);
        self.clock.fetch_add(7, Ordering::Relaxed);
        self.recorder.complete(command, result, response);
    }

    fn read(&self, result: Result<usize, &ScsiError>) {
        self.issue(self.dev, &[8, 0, 0, 0, 4, 0], Direction::In, 4, result, &[]);
    }
}

fn rows(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.ends_with('\n'));
    text.lines()
        .map(|line| {
            let row: Value = serde_json::from_str(line).unwrap();
            assert!(row["event"].is_string());
            assert!(row["mono_ns"].is_u64());
            row
        })
        .collect()
}

#[test]
fn policy_handles_unset_empty_and_field_trace_even_when_empty() {
    assert_eq!(policy(None, None), Policy::Off);
    assert_eq!(policy(Some(OsStr::new("")), None), Policy::Off);
    assert_eq!(
        policy(Some(OsStr::new("trace")), None),
        Policy::Directory(Path::new("trace"))
    );
    for trace in [None, Some(OsStr::new("")), Some(OsStr::new("trace"))] {
        assert_eq!(policy(trace, Some(OsStr::new(""))), Policy::FieldTrace);
        assert_eq!(policy(trace, Some(OsStr::new("field"))), Policy::FieldTrace);
    }
}

#[test]
fn open_requires_existing_directory_and_never_reuses_a_file_or_symlink() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let ordinary_file = directory.path().join("ordinary");
    std::fs::write(&ordinary_file, "untouched").unwrap();
    let cases = [directory.path().join("missing"), ordinary_file.clone()];
    for path in cases {
        let mut messages = Vec::new();
        assert!(
            open_recorder(&path, 123, UNIX_EPOCH, &mut |message| messages
                .push(message.to_owned()))
            .is_none()
        );
        assert_eq!(messages.len(), 1);
        assert!(messages[0].starts_with("rem: command trace disabled: "));
    }
    let collision = directory.path().join("rem-trace-123-0.jsonl");
    std::fs::write(&collision, "existing evidence").unwrap();
    for symlink_case in [false, true] {
        if symlink_case {
            std::fs::remove_file(&collision).unwrap();
            symlink(&ordinary_file, &collision).unwrap();
        }
        let mut messages = Vec::new();
        assert!(
            open_recorder(directory.path(), 123, UNIX_EPOCH, &mut |message| messages
                .push(message.to_owned()))
            .is_none()
        );
        assert_eq!(messages.len(), 1);
        assert!(messages[0].starts_with("rem: command trace disabled: "));
        assert_eq!(
            std::fs::read_to_string(&ordinary_file).unwrap(),
            "untouched"
        );
        if !symlink_case {
            assert_eq!(
                std::fs::read_to_string(&collision).unwrap(),
                "existing evidence"
            );
        }
    }
    std::fs::remove_file(&collision).unwrap();
    symlink(directory.path().join("absent-target"), &collision).unwrap();
    let mut messages = Vec::new();
    assert!(
        open_recorder(directory.path(), 123, UNIX_EPOCH, &mut |message| messages
            .push(message.to_owned()))
        .is_none()
    );
    assert_eq!(messages.len(), 1);
    assert!(!directory.path().join("absent-target").exists());
}

#[test]
fn open_creates_private_regular_file_and_header_first() {
    let directory = tempfile::tempdir().unwrap();
    let mut messages = Vec::new();
    let recorder = open_recorder(directory.path(), 123, UNIX_EPOCH, &mut |message| {
        messages.push(message.to_owned())
    })
    .unwrap();
    assert!(messages.is_empty());
    let path = directory.path().join("rem-trace-123-0.jsonl");
    let metadata = path.symlink_metadata().unwrap();
    assert!(metadata.file_type().is_file());
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    let rows = rows(&path);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["event"], "header");
    assert_eq!(rows[0]["schema"], 1);
    assert_eq!(rows[0]["pid"], 123);
    assert_eq!(rows[0]["rem_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(rows[0]["wall"], "1970-01-01T00:00:00Z");
    assert_eq!(recorder.device(), 1);
    assert_eq!(recorder.device(), 2);
}

#[test]
fn utc_dates_cover_century_and_leap_day_boundaries() {
    for (seconds, expected) in [
        (0, "1970-01-01T00:00:00Z"),
        (951_782_400, "2000-02-29T00:00:00Z"),
        (1_609_459_199, "2020-12-31T23:59:59Z"),
        (1_609_459_200, "2021-01-01T00:00:00Z"),
        (4_107_542_400, "2100-03-01T00:00:00Z"),
    ] {
        assert_eq!(
            wall_utc(UNIX_EPOCH + Duration::from_secs(seconds)).as_deref(),
            Some(expected)
        );
    }
    assert_eq!(wall_utc(UNIX_EPOCH - Duration::from_secs(1)), None);
}

#[test]
fn all_error_variants_keep_every_field_and_unknown_counts_are_null() {
    let trace = Trace::new();
    let errors = [
        (ScsiError::Cancelled, "Cancelled", json!({}), None),
        (
            ScsiError::Truncated { got: 3, need: 36 },
            "Truncated",
            json!({"got":3,"need":36}),
            None,
        ),
        (
            ScsiError::InvalidResponse {
                offset: 4,
                detail: "bad length",
            },
            "InvalidResponse",
            json!({"offset":4,"detail":"bad length"}),
            None,
        ),
        (
            ScsiError::Io(std::io::Error::from_raw_os_error(libc::ENOTTY)),
            "Io",
            json!({"errno":libc::ENOTTY,"message":std::io::Error::from_raw_os_error(libc::ENOTTY).to_string()}),
            None,
        ),
        (
            ScsiError::Io(std::io::Error::other("synthetic")),
            "Io",
            json!({"errno":null,"message":"synthetic"}),
            None,
        ),
        (
            ScsiError::CheckCondition {
                sense: vec![0x70, 0xff, 0, 0x12],
                bytes_transferred: 3,
            },
            "CheckCondition",
            json!({"sense":"70ff0012","bytes_transferred":3}),
            Some(3),
        ),
        (
            ScsiError::UnexpectedStatus { status: 8 },
            "UnexpectedStatus",
            json!({"status":8}),
            None,
        ),
        (
            ScsiError::TransportError {
                status: 2,
                host_status: 3,
                driver_status: 4,
                info: 0x12345678,
                sense: vec![0x72, 0xff],
            },
            "TransportError",
            json!({"status":2,"host_status":3,"driver_status":4,"info":0x12345678,"sense":"72ff"}),
            None,
        ),
        (
            ScsiError::HostCapacityRefused("too big"),
            "HostCapacityRefused",
            json!({"detail":"too big"}),
            None,
        ),
        (
            ScsiError::InvalidInput("invalid cdb"),
            "InvalidInput",
            json!({"detail":"invalid cdb"}),
            None,
        ),
    ];
    for (error, name, fields, transferred) in &errors {
        trace.issue(trace.dev, &[], Direction::Out, 4, Err(error), &[]);
        let rows = trace.rows();
        let done = rows.last().unwrap();
        assert_eq!(done["outcome"], *name);
        assert_eq!(done["error"], *fields);
        assert_eq!(done["transferred_bytes"], json!(transferred));
        assert_eq!(done["opcode"], Value::Null);
        assert_eq!(done["duration_ns"], 7);
        assert_eq!(rows[rows.len() - 2]["event"], "start");
    }
    assert_eq!(trace.recorder.lock().sync_count, errors.len());
}

#[test]
fn success_counts_are_raw_and_none_is_zero_without_sync() {
    let trace = Trace::new();
    for (dir, opcode, requested, transferred) in [
        (Direction::In, 0x12, 96, 36),
        (Direction::Out, 0x15, 20, 12),
        (Direction::None, 0x01, 0, 0),
    ] {
        trace.issue(
            trace.dev,
            &[opcode, 0, 0, 0, 0, 0],
            dir,
            requested,
            Ok(transferred),
            &[],
        );
        let rows = trace.rows();
        let start = &rows[rows.len() - 2];
        let done = rows.last().unwrap();
        assert_eq!(start["dir"], dir.as_str());
        assert_eq!(start["requested_bytes"], requested);
        assert_eq!(start["timeout_ms"], 5000);
        assert_eq!(done["transferred_bytes"], transferred);
        assert_eq!(done["outcome"], "good");
        assert_eq!(done["error"], Value::Null);
    }
    assert_eq!(trace.recorder.lock().sync_count, 0);
}

#[test]
fn thousand_reads_have_one_pair_and_cumulative_timed_run_rows() {
    let trace = Trace::new();
    for index in 0..1000 {
        if index == 499 || index == 999 {
            trace.clock.fetch_add(RUN_INTERVAL_NS, Ordering::Relaxed);
        }
        trace.read(Ok(4));
    }
    let rows = trace.rows();
    assert_eq!(rows.iter().filter(|row| row["event"] == "start").count(), 1);
    assert_eq!(rows.iter().filter(|row| row["event"] == "done").count(), 1);
    let runs: Vec<_> = rows.iter().filter(|row| row["event"] == "run").collect();
    assert_eq!(runs.len(), 2);
    for (run, count) in runs.iter().zip([500, 1000]) {
        assert_eq!(run["count"], count);
        assert_eq!(run["bytes"], count * 4);
        assert_eq!(run["total_ns"], count * 7);
        assert_eq!(run["max_ns"], 7);
        assert_eq!(run["first_mono_ns"], 100);
        assert_eq!(run["cdb"], "080000000400");
    }
    trace.recorder.close_device(trace.dev);
    assert_eq!(trace.rows().last().unwrap()["count"], 1000);
}

#[test]
fn interval_is_checked_at_completion_and_resets_after_each_cumulative_row() {
    let trace = Trace::new();
    trace.read(Ok(4)); // first completion = 107
    trace.clock.store(RUN_INTERVAL_NS + 106, Ordering::Relaxed);
    let command = trace
        .recorder
        .begin(trace.dev, &[8, 0, 0, 0, 4, 0], Direction::In, 4, 5000);
    assert_eq!(trace.rows().len(), 3, "no timer or begin-time flush");
    trace.recorder.complete(command, Ok(4), &[]);
    assert_eq!(trace.rows().len(), 3, "one ns short of ten seconds");
    trace.clock.store(RUN_INTERVAL_NS + 107, Ordering::Relaxed);
    let command = trace
        .recorder
        .begin(trace.dev, &[8, 0, 0, 0, 4, 0], Direction::In, 4, 5000);
    trace.recorder.complete(command, Ok(4), &[]);
    assert_eq!(trace.rows().last().unwrap()["count"], 3);
    trace
        .clock
        .store(2 * RUN_INTERVAL_NS + 106, Ordering::Relaxed);
    let command = trace
        .recorder
        .begin(trace.dev, &[8, 0, 0, 0, 4, 0], Direction::In, 4, 5000);
    trace.recorder.complete(command, Ok(4), &[]);
    assert_eq!(trace.rows().len(), 4);
}

#[test]
fn failure_and_short_transfer_flush_prior_run_before_self_contained_done() {
    for result in [
        Err(ScsiError::CheckCondition {
            sense: vec![0x70],
            bytes_transferred: 2,
        }),
        Ok(2),
    ] {
        let trace = Trace::new();
        trace.read(Ok(4));
        trace.read(Ok(4));
        let start = trace.clock.load(Ordering::Relaxed);
        trace.read(result.as_ref().copied());
        let rows = trace.rows();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[3]["event"], "run");
        assert_eq!(rows[3]["count"], 2);
        assert_eq!(rows[4]["event"], "done");
        assert_eq!(rows[4]["transferred_bytes"], 2);
        assert_eq!(rows[4]["start_mono_ns"], start);
        assert_eq!(rows[4]["cdb"], "080000000400");
        trace.read(Ok(4));
        assert_eq!(trace.rows()[5]["event"], "start");
        assert_eq!(trace.rows()[6]["event"], "done");
        trace.recorder.close_device(trace.dev);
        assert_eq!(trace.rows().last().unwrap()["count"], 1);
    }
}

#[test]
fn every_key_component_closes_run_on_issue_and_only_read_write_summarise() {
    for (cdb, requested, timeout) in [
        (vec![0x0a, 0, 0, 0, 4, 0], 4, 5000),
        (vec![8, 0, 0, 0, 8, 0], 4, 5000),
        (vec![8, 0, 0, 0, 4, 0], 8, 5000),
        (vec![8, 0, 0, 0, 4, 0], 4, 9000),
    ] {
        let trace = Trace::new();
        trace.read(Ok(4));
        let command = trace
            .recorder
            .begin(trace.dev, &cdb, Direction::Out, requested, timeout);
        assert_eq!(trace.rows()[3]["event"], "run");
        assert_eq!(trace.rows()[4]["event"], "start", "flush before ioctl");
        trace.recorder.complete(command, Ok(requested), &[]);
    }
    let trace = Trace::new();
    for _ in 0..3 {
        trace.issue(
            trace.dev,
            &[0x28, 0, 0, 0, 4, 0],
            Direction::In,
            4,
            Ok(4),
            &[],
        );
    }
    assert_eq!(trace.rows().len(), 7, "READ(10) has every pair");
    let trace = Trace::new();
    for _ in 0..3 {
        trace.issue(
            trace.dev,
            &[0x0a, 0, 0, 0, 4, 0],
            Direction::Out,
            4,
            Ok(4),
            &[],
        );
    }
    trace.recorder.close_device(trace.dev);
    assert_eq!(trace.rows().len(), 4);
    assert_eq!(trace.rows()[3]["count"], 3);
}

#[test]
fn interleaved_devices_keep_independent_runs_and_monotone_indices() {
    let trace = Trace::new();
    let second = trace.recorder.device();
    for _ in 0..10 {
        trace.read(Ok(4));
        trace.issue(
            second,
            &[0x0a, 0, 0, 0, 4, 0],
            Direction::Out,
            4,
            Ok(4),
            &[],
        );
    }
    trace.recorder.close_device(trace.dev);
    trace.issue(
        second,
        &[0x0a, 0, 0, 0, 4, 0],
        Direction::Out,
        4,
        Ok(4),
        &[],
    );
    trace.recorder.close_device(second);
    let rows = trace.rows();
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[5]["dev"], 1);
    assert_eq!(rows[5]["count"], 10);
    assert_eq!(rows[6]["dev"], 2);
    assert_eq!(rows[6]["count"], 11);
    assert_eq!(trace.recorder.device(), 3);
}

fn inquiry() -> Vec<u8> {
    let mut bytes = vec![0u8; 96];
    bytes[0] = 1;
    bytes[3] = 2;
    bytes[4] = 91;
    bytes[8..16].copy_from_slice(b"VENDOR  ");
    bytes[16..32].copy_from_slice(b"MODEL           ");
    bytes[32..36].copy_from_slice(b"R001");
    bytes[36..48].copy_from_slice(b"SN-123456789");
    bytes
}

#[test]
fn inquiry_records_only_exact_standard_fields_once_and_never_serials_or_payload() {
    let trace = Trace::new();
    let bytes = inquiry();
    // The VPD payload is deliberately parseable as standard INQUIRY too.
    trace.issue(
        trace.dev,
        &[0x12, 1, 0x80, 0, 96, 0],
        Direction::In,
        96,
        Ok(96),
        &bytes,
    );
    trace.issue(
        trace.dev,
        &[0x12, 0, 0, 0, 96, 0],
        Direction::In,
        96,
        Err(&ScsiError::InvalidInput("failure")),
        &bytes,
    );
    trace.issue(
        trace.dev,
        &[0x12, 0, 0, 0, 96, 0],
        Direction::Out,
        96,
        Ok(96),
        &bytes,
    );
    trace.issue(trace.dev, &[0x12], Direction::In, 96, Ok(96), &bytes);
    trace.issue(
        trace.dev,
        &[0x12, 0, 0, 0, 96, 0],
        Direction::In,
        96,
        Ok(35),
        &bytes,
    );
    let mut bad = bytes.clone();
    bad[3] = 1;
    trace.issue(
        trace.dev,
        &[0x12, 0, 0, 0, 96, 0],
        Direction::In,
        96,
        Ok(96),
        &bad,
    );
    bad[3] = 2;
    bad[4] = 30;
    trace.issue(
        trace.dev,
        &[0x12, 0, 0, 0, 96, 0],
        Direction::In,
        96,
        Ok(96),
        &bad,
    );
    assert!(!trace.rows().iter().any(|row| row["event"] == "device"));
    trace.issue(
        trace.dev,
        &[0x12, 0, 0, 0, 96, 0],
        Direction::In,
        96,
        Ok(36),
        &bytes,
    );
    trace.issue(
        trace.dev,
        &[0x12, 0, 0, 0, 96, 0],
        Direction::In,
        96,
        Ok(96),
        &bytes,
    );
    trace.issue(
        trace.dev,
        &[8, 0, 0, 0, 96, 0],
        Direction::In,
        96,
        Ok(96),
        &bytes,
    );
    let rows = trace.rows();
    let devices: Vec<_> = rows.iter().filter(|row| row["event"] == "device").collect();
    assert_eq!(devices.len(), 1);
    let mut keys: Vec<_> = devices[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        ["dev", "event", "mono_ns", "product", "revision", "type", "vendor"]
    );
    assert_eq!(devices[0]["type"], 1);
    assert_eq!(devices[0]["vendor"], "VENDOR  ");
    assert_eq!(devices[0]["product"], "MODEL           ");
    assert_eq!(devices[0]["revision"], "R001");
    let text = std::fs::read_to_string(&trace.path).unwrap();
    for secret in [&bytes[36..48], b"SN-123456789".as_slice()] {
        assert!(!text.contains(std::str::from_utf8(secret).unwrap()));
        assert!(!text.contains(&hex(secret)));
    }
    assert!(!text.contains(&hex(&bytes)));
}

#[test]
fn poison_is_recovered_and_write_failures_warn_once_and_recording_continues() {
    let trace = Trace::new();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = trace.recorder.state.lock().unwrap();
        panic!("injected poisoned recorder lock");
    }));
    trace.recorder.lock().file = File::open(&trace.path).unwrap();
    trace.issue(
        trace.dev,
        &[1],
        Direction::None,
        0,
        Err(&ScsiError::InvalidInput("bad")),
        &[],
    );
    trace.issue(
        trace.dev,
        &[1],
        Direction::None,
        0,
        Err(&ScsiError::InvalidInput("bad")),
        &[],
    );
    assert_eq!(trace.recorder.lock().warning_count, 1);
    assert_eq!(trace.recorder.lock().sync_count, 2);
    trace.recorder.lock().file = OpenOptions::new().append(true).open(&trace.path).unwrap();
    trace.issue(trace.dev, &[1], Direction::None, 0, Ok(0), &[]);
    assert_eq!(trace.rows().last().unwrap()["outcome"], "good");
}
