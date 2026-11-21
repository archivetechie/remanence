//! Hermetic recorder proofs: file safety, error fidelity, privacy and run state.

use super::*;
use std::os::unix::fs::PermissionsExt;
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
        let mut recorder = Recorder::new(File::create_new(&path).unwrap(), 123, UNIX_EPOCH, None);
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
        .map(|line| checked_row(serde_json::from_str(line).unwrap()))
        .collect()
}

fn checked_row(row: Value) -> Value {
    assert!(row["event"].is_string());
    assert!(row["mono_ns"].is_u64());
    row
}

fn recovered_rows(bytes: &[u8], fragment: &[u8]) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut invalid = Vec::new();
    for line in bytes
        .strip_suffix(b"\n")
        .unwrap()
        .split(|byte| *byte == b'\n')
    {
        match serde_json::from_slice(line) {
            Ok(row) => rows.push(checked_row(row)),
            Err(_) => invalid.push(line),
        }
    }
    assert!(invalid.len() <= 1, "at most the failed-write fragment");
    assert!(
        invalid.iter().all(|line| *line == fragment),
        "only the exact failed-write fragment"
    );
    if !fragment.is_empty() {
        assert_eq!(invalid, [fragment]);
    }
    rows
}

fn child_output(test: &str, mode: &str) -> std::process::Output {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("CT1_FR2_CHILD", mode)
        .env("REM_TRACE", "")
        .env_remove("REM_FIELD_TRACE")
        .output()
        .unwrap()
}

fn assert_child(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn policy_only_reports_field_trace_when_command_trace_is_requested() {
    assert_eq!(policy(None, None), Policy::Off);
    assert_eq!(policy(Some(OsStr::new("")), None), Policy::Off);
    assert_eq!(
        policy(Some(OsStr::new("trace")), None),
        Policy::Directory(Path::new("trace"))
    );
    for field in [Some(OsStr::new("")), Some(OsStr::new("field"))] {
        assert_eq!(policy(None, field), Policy::Off);
        assert_eq!(policy(Some(OsStr::new("")), field), Policy::Off);
        assert_eq!(policy(Some(OsStr::new("trace")), field), Policy::FieldTrace);
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
    if std::env::var("CT1_FR2_CHILD").as_deref() != Ok("mode") {
        assert_child(&child_output(
            "command_trace::tests::open_creates_private_regular_file_and_header_first",
            "mode",
        ));
        return;
    }
    // SAFETY: this subprocess runs this test alone, before creating the file.
    unsafe {
        libc::umask(0o022);
    }
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
    for (dir, literal, opcode, requested, transferred) in [
        (Direction::In, "in", 0x12, 96, 36),
        (Direction::Out, "out", 0x15, 20, 12),
        (Direction::None, "none", 0x01, 0, 0),
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
        assert_eq!(start["dir"], literal);
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
fn interval_is_checked_at_completion_across_literal_ten_second_boundary() {
    let trace = Trace::new();
    trace.clock.store(0, Ordering::Relaxed);
    let first = trace
        .recorder
        .begin(trace.dev, &[8, 0, 0, 0, 4, 0], Direction::In, 4, 5000);
    trace.clock.store(7, Ordering::Relaxed);
    trace.recorder.complete(first, Ok(4), &[]);
    assert_eq!(trace.rows()[1]["mono_ns"], 0);
    assert_eq!(trace.rows()[2]["mono_ns"], 7);
    trace.clock.store(10_000_000_006, Ordering::Relaxed);
    let command = trace
        .recorder
        .begin(trace.dev, &[8, 0, 0, 0, 4, 0], Direction::In, 4, 5000);
    assert_eq!(trace.rows().len(), 3, "no run row before completion");
    trace.clock.store(10_000_000_007, Ordering::Relaxed);
    trace.recorder.complete(command, Ok(4), &[]);
    let rows = trace.rows();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[3]["event"], "run");
    assert_eq!(rows[3]["count"], 2);
    assert_eq!(rows[3]["mono_ns"], 10_000_000_007u64);
    trace.clock.store(20_000_000_006, Ordering::Relaxed);
    let command = trace
        .recorder
        .begin(trace.dev, &[8, 0, 0, 0, 4, 0], Direction::In, 4, 5000);
    trace.recorder.complete(command, Ok(4), &[]);
    assert_eq!(trace.rows().len(), 4, "the interval resets after a run row");
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
    trace.recorder.lock().writer = Box::new(File::open(&trace.path).unwrap());
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
    trace.recorder.lock().writer =
        Box::new(OpenOptions::new().append(true).open(&trace.path).unwrap());
    trace.issue(trace.dev, &[1], Direction::None, 0, Ok(0), &[]);
    let recovered = recovered_rows(&std::fs::read(&trace.path).unwrap(), b"");
    assert_eq!(recovered.last().unwrap()["outcome"], "good");
}

#[test]
fn real_file_zero_and_partial_errors_leave_only_the_failed_fragment() {
    struct PartialFile {
        file: File,
        prefix: Option<usize>,
    }
    impl Writer for PartialFile {
        fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            if let Some(prefix) = self.prefix.take() {
                Write::write_all(&mut self.file, &bytes[..prefix])?;
                return Err(std::io::Error::other("injected partial file write"));
            }
            Write::write_all(&mut self.file, bytes)
        }
        fn sync_data(&mut self) -> std::io::Result<()> {
            self.file.sync_data()
        }
    }
    for prefix in [0, 13] {
        let trace = Trace::new();
        let header_length = std::fs::read(&trace.path).unwrap().len();
        trace.recorder.lock().writer = Box::new(PartialFile {
            file: OpenOptions::new().append(true).open(&trace.path).unwrap(),
            prefix: Some(prefix),
        });
        trace.issue(trace.dev, &[1], Direction::None, 0, Ok(0), &[]);
        let bytes = std::fs::read(&trace.path).unwrap();
        let fragment = &bytes[header_length..header_length + prefix];
        let rows = recovered_rows(&bytes, fragment);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["event"], "header");
        assert_eq!(rows[1]["event"], "done");
        trace.issue(trace.dev, &[1], Direction::None, 0, Ok(0), &[]);
        let rows = recovered_rows(&std::fs::read(&trace.path).unwrap(), fragment);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[2]["event"], "start");
        assert_eq!(rows[3]["event"], "done");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Operation {
    Write(Vec<u8>),
    Sync,
}

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    operations: Vec<Operation>,
    fail_next: Option<usize>,
    sync_fails: bool,
    advance_ns: u64,
}

#[derive(Clone)]
struct Double {
    output: Arc<Mutex<Output>>,
    clock: Arc<AtomicU64>,
}

impl Double {
    fn new(clock: Arc<AtomicU64>) -> Self {
        Self {
            output: Arc::new(Mutex::new(Output::default())),
            clock,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Output> {
        self.output.lock().unwrap()
    }

    fn rows(&self) -> Vec<Value> {
        let output = self.lock();
        std::str::from_utf8(&output.bytes)
            .unwrap()
            .lines()
            .map(|line| checked_row(serde_json::from_str(line).unwrap()))
            .collect()
    }
}

impl Writer for Double {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut output = self.lock();
        output.operations.push(Operation::Write(bytes.to_vec()));
        self.clock.fetch_add(output.advance_ns, Ordering::Relaxed);
        if let Some(prefix) = output.fail_next.take() {
            output
                .bytes
                .extend_from_slice(&bytes[..prefix.min(bytes.len())]);
            return Err(std::io::Error::other("injected write failure"));
        }
        output.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn sync_data(&mut self) -> std::io::Result<()> {
        let mut output = self.lock();
        output.operations.push(Operation::Sync);
        if output.sync_fails {
            Err(std::io::Error::other("injected sync failure"))
        } else {
            Ok(())
        }
    }
}

fn recorder_double() -> (Recorder, Double, Arc<AtomicU64>, u64) {
    let clock = Arc::new(AtomicU64::new(100));
    let double = Double::new(Arc::clone(&clock));
    let mut recorder = Recorder::new_writer(Box::new(double.clone()), 123, UNIX_EPOCH);
    recorder.clock = Some(Arc::clone(&clock));
    let dev = recorder.device();
    (recorder, double, clock, dev)
}

#[test]
fn failure_sync_is_observed_on_writer_after_its_failure_row() {
    let (recorder, double, clock, dev) = recorder_double();
    double.lock().operations.clear();
    let command = recorder.begin(dev, &[1], Direction::None, 0, 5000);
    clock.fetch_add(3, Ordering::Relaxed);
    recorder.complete(command, Err(&ScsiError::InvalidInput("synthetic")), &[]);
    let output = double.lock();
    assert_eq!(output.operations.len(), 3);
    let Operation::Write(start) = &output.operations[0] else {
        panic!("start write");
    };
    let Operation::Write(done) = &output.operations[1] else {
        panic!("failure write");
    };
    assert_eq!(
        serde_json::from_slice::<Value>(start).unwrap()["event"],
        "start"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(done).unwrap()["outcome"],
        "InvalidInput"
    );
    assert_eq!(
        output.operations[2],
        Operation::Sync,
        "actual writer sync, independent of the counter"
    );
}

#[test]
fn sync_errors_are_nonfatal_warn_once_and_recording_continues() {
    let (recorder, double, _, dev) = recorder_double();
    double.lock().sync_fails = true;
    for _ in 0..2 {
        let command = recorder.begin(dev, &[1], Direction::None, 0, 5000);
        recorder.complete(command, Err(&ScsiError::InvalidInput("synthetic")), &[]);
    }
    let command = recorder.begin(dev, &[1], Direction::None, 0, 5000);
    recorder.complete(command, Ok(0), &[]);
    assert_eq!(double.rows().last().unwrap()["outcome"], "good");
    assert_eq!(recorder.lock().warning_count, 1);
    assert_eq!(
        double
            .lock()
            .operations
            .iter()
            .filter(|op| matches!(op, Operation::Sync))
            .count(),
        2
    );
}

#[test]
fn zero_and_partial_write_recovery_preserves_header_and_isolates_one_fragment() {
    for header_failure in [true, false] {
        for prefix in [0, 13] {
            let clock = Arc::new(AtomicU64::new(100));
            let double = Double::new(Arc::clone(&clock));
            if header_failure {
                double.lock().fail_next = Some(prefix);
            }
            let mut recorder = Recorder::new_writer(Box::new(double.clone()), 123, UNIX_EPOCH);
            recorder.clock = Some(clock);
            let dev = recorder.device();
            if !header_failure {
                double.lock().fail_next = Some(prefix);
            }
            let header_length = if header_failure {
                0
            } else {
                double.lock().bytes.len()
            };
            let initial_fragment = if header_failure {
                Some(double.lock().bytes.clone())
            } else {
                None
            };
            let command = recorder.begin(dev, &[1], Direction::None, 0, 5000);
            let fragment =
                initial_fragment.unwrap_or_else(|| double.lock().bytes[header_length..].to_vec());
            recorder.complete(command, Ok(0), &[]);
            let output = double.lock();
            let rows = recovered_rows(&output.bytes, &fragment);
            assert_eq!(
                rows[0]["event"], "header",
                "pending header precedes every successful data row"
            );
            let events: Vec<_> = rows
                .iter()
                .map(|row| row["event"].as_str().unwrap())
                .collect();
            assert_eq!(
                events,
                if header_failure {
                    vec!["header", "start", "done"]
                } else {
                    vec!["header", "done"]
                }
            );
            assert_eq!(recorder.lock().warning_count, 1);
        }
    }
}

#[test]
fn command_duration_excludes_start_row_writer_time() {
    let (recorder, double, clock, dev) = recorder_double();
    double.lock().advance_ns = 1_000;
    let command = recorder.begin(dev, &[1], Direction::None, 0, 5000);
    assert_eq!(clock.load(Ordering::Relaxed), 1_100);
    clock.fetch_add(7, Ordering::Relaxed);
    recorder.complete(command, Ok(0), &[]);
    let rows = double.rows();
    assert_eq!(rows[1]["mono_ns"], 100);
    assert_eq!(rows[2]["mono_ns"], 1_107);
    assert_eq!(rows[2]["duration_ns"], 7);
}

#[test]
fn varied_run_durations_have_exact_total_max_and_last_start() {
    let (recorder, double, clock, dev) = recorder_double();
    for (start, duration) in [(100, 3), (140, 11), (170, 5)] {
        clock.store(start, Ordering::Relaxed);
        let command = recorder.begin(dev, &[8, 0, 0, 0, 4, 0], Direction::In, 4, 5000);
        clock.fetch_add(duration, Ordering::Relaxed);
        recorder.complete(command, Ok(4), &[]);
    }
    recorder.close_device(dev);
    let rows = double.rows();
    let run = rows.last().unwrap();
    assert_eq!(run["count"], 3);
    assert_eq!(run["bytes"], 12);
    assert_eq!(run["total_ns"], 19);
    assert_eq!(run["max_ns"], 11);
    assert_eq!(run["first_mono_ns"], 100);
    assert_eq!(run["last_mono_ns"], 170);
}

#[test]
fn missing_device_entries_skip_without_panicking_or_writing() {
    let (recorder, double, _, dev) = recorder_double();
    assert!(recorder
        .begin(dev + 1, &[1], Direction::None, 0, 5000)
        .is_none());
    let command = recorder.begin(dev, &[1], Direction::None, 0, 5000);
    recorder.lock().devices.remove(&dev);
    let before = double.lock().operations.len();
    recorder.complete(command, Ok(0), &[]);
    recorder.close_device(dev);
    assert_eq!(double.lock().operations.len(), before);
}

#[test]
fn waiting_caller_rechecks_disabled_after_the_panicking_writer_unlocks() {
    if std::env::var("CT1_FR2_CHILD").as_deref() != Ok("race") {
        assert_child(&child_output("command_trace::tests::waiting_caller_rechecks_disabled_after_the_panicking_writer_unlocks", "race"));
        return;
    }
    struct BarrierWriter {
        writes: Arc<std::sync::atomic::AtomicUsize>,
        entered: Arc<std::sync::Barrier>,
        release: Arc<std::sync::Barrier>,
    }
    impl Writer for BarrierWriter {
        fn write_all(&mut self, _: &[u8]) -> std::io::Result<()> {
            if self.writes.fetch_add(1, Ordering::SeqCst) == 1 {
                self.entered.wait();
                self.release.wait();
                panic!("injected panic while the second caller waits");
            }
            Ok(())
        }
        fn sync_data(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let waiting = Arc::new(std::sync::Barrier::new(2));
    let mut recorder = Recorder::new_writer(
        Box::new(BarrierWriter {
            writes: Arc::clone(&writes),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }),
        123,
        UNIX_EPOCH,
    );
    let first_dev = recorder.device();
    let second_dev = recorder.device();
    recorder.lock_waiting = Some(Arc::clone(&waiting));
    let recorder = Arc::new(recorder);
    let first_recorder = Arc::clone(&recorder);
    let first =
        std::thread::spawn(move || first_recorder.begin(first_dev, &[1], Direction::None, 0, 5000));
    entered.wait();
    let second_recorder = Arc::clone(&recorder);
    let second = std::thread::spawn(move || {
        second_recorder.begin(second_dev, &[1], Direction::None, 0, 5000)
    });
    // This barrier is reached only after try_lock observed the held mutex,
    // inside a caller that already passed guarded()'s disabled check.
    waiting.wait();
    release.wait();
    assert!(first.join().unwrap().is_none());
    assert!(second.join().unwrap().is_none());
    assert!(DISABLED.load(Ordering::Acquire));
    assert_eq!(
        writes.load(Ordering::SeqCst),
        2,
        "the waiting caller must not write"
    );
}

#[test]
fn diagnostics_are_emitted_after_the_recorder_lock_is_released() {
    let (mut recorder, double, _, dev) = recorder_double();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observations = Arc::clone(&observed);
    recorder.diagnostic_hook = Some(Arc::new(move |_, free| {
        observations.lock().unwrap().push(free)
    }));
    for _ in 0..2 {
        double.lock().fail_next = Some(0);
        let command = recorder.begin(dev, &[1], Direction::None, 0, 5000);
        recorder.complete(command, Ok(0), &[]);
    }
    assert_eq!(
        *observed.lock().unwrap(),
        [true],
        "one diagnostic with a free recorder lock"
    );
    assert_eq!(recorder.lock().warning_count, 1);
}

#[test]
fn injected_file_limit_reserves_headroom_before_any_writer_call() {
    let mode = std::env::var("CT1_FR2_CHILD").unwrap_or_default();
    if !matches!(
        mode.as_str(),
        "limit-header" | "limit-exact" | "limit-partial"
    ) {
        for mode in ["limit-header", "limit-exact", "limit-partial"] {
            let output = child_output("command_trace::tests::injected_file_limit_reserves_headroom_before_any_writer_call", mode);
            assert_child(&output);
            assert_eq!(
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .filter(|line| line
                        .starts_with("rem: command trace disabled: file-size limit reached"))
                    .count(),
                1
            );
        }
        return;
    }
    if mode == "limit-header" {
        let double = Double::new(Arc::new(AtomicU64::new(100)));
        let _ = Recorder::new_writer_with_limit(
            Box::new(double.clone()),
            123,
            UNIX_EPOCH,
            Some(65_536),
        );
        assert!(
            double.lock().operations.is_empty(),
            "no write when the margin consumes the limit"
        );
        assert!(DISABLED.load(Ordering::Acquire));
        return;
    }
    let (mut recorder, double, _, dev) = recorder_double();
    let row = json!({"event":"probe", "mono_ns":123});
    let row_length = serde_json::to_vec(&row).unwrap().len() as u64 + 1;
    let initial = double.lock().bytes.len() as u64;
    recorder.lock().file_limit = Some(65_536 + initial + row_length);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let observations = Arc::clone(&observed);
    recorder.diagnostic_hook = Some(Arc::new(move |_, free| {
        observations.lock().unwrap().push(free)
    }));
    if mode == "limit-partial" {
        double.lock().fail_next = Some(13);
    }
    let _ = recorder.with_state(|state| state.row(row.clone(), false));
    assert!(
        !DISABLED.load(Ordering::Acquire),
        "a write ending exactly at the cap is permitted"
    );
    assert_eq!(
        double.lock().bytes.len() as u64,
        initial
            + if mode == "limit-partial" {
                13
            } else {
                row_length
            }
    );
    let bytes = double.lock().bytes.clone();
    let count = double.lock().operations.len();
    let _ = recorder.with_state(|state| state.row(row, false));
    assert!(DISABLED.load(Ordering::Acquire));
    assert_eq!(
        double.lock().operations.len(),
        count,
        "the over-limit row never reaches the writer"
    );
    assert_eq!(double.lock().bytes, bytes);
    assert!(recorder
        .begin(dev, &[1], Direction::None, 0, 5000)
        .is_none());
    assert_eq!(
        *observed.lock().unwrap(),
        if mode == "limit-partial" {
            vec![true, true]
        } else {
            vec![true]
        }
    );
}

// Test-only constructors and injection points, shared by the transport tests.
impl Recorder {
    pub(crate) fn for_test(directory: &Path) -> Arc<Self> {
        open_recorder(directory, 123, UNIX_EPOCH, &mut |reason| panic!("{reason}"))
            .expect("test trace opens")
    }

    pub(crate) fn sync_count(&self) -> usize {
        self.lock().sync_count
    }

    pub(crate) fn disabled_for_test() -> bool {
        DISABLED.load(Ordering::Acquire)
    }

    pub(crate) fn panicking_for_test(directory: &Path, panic_write: usize) -> Arc<Self> {
        struct PanickingWriter {
            file: File,
            writes: usize,
            panic_write: usize,
        }
        impl Writer for PanickingWriter {
            fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
                self.writes += 1;
                assert_ne!(self.writes, self.panic_write, "injected writer panic");
                Write::write_all(&mut self.file, bytes)
            }
            fn sync_data(&mut self) -> std::io::Result<()> {
                self.file.sync_data()
            }
        }
        let file = File::create_new(directory.join("panic.jsonl")).unwrap();
        Arc::new(Self::new_writer(
            Box::new(PanickingWriter {
                file,
                writes: 0,
                panic_write,
            }),
            123,
            UNIX_EPOCH,
        ))
    }
}
