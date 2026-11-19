//! PLR-1b production CLI probes on cloned model media and matching host state.
use remanence_chaos::model::{
    DeviceRole, ModelTransport, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::{
    transport::TransferOutcome, DeviceAttachment, DriveHandle, ScsiError, SgTransport,
    StaticAllowlist, TimeoutClass,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
};

const DEVICE: &str = "/virtual/plr-drive";
const SERIAL: &str = "PLR-CLI-DRIVE";
const TAPE: &str = "PLR001L9";
const BAY: u16 = 0x100;

#[derive(Clone)]
struct Snapshot {
    world: VirtualWorld,
    disk: BTreeMap<PathBuf, Vec<u8>>,
}
fn disk(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !path.exists() {
            return;
        }
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files)
            } else if path.extension().is_none_or(|x| x != "lock")
                && !path.to_string_lossy().ends_with("-shm")
            {
                files.insert(
                    path.strip_prefix(root).unwrap().into(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}
#[derive(Default, Clone, Debug)]
struct IoTrace {
    commands: Vec<Vec<u8>>,
    missing_reads: usize,
    fault_at: Option<u64>,
    eod_succeeded: bool,
    late_armed: bool,
}
fn missing_signature(error: &ScsiError) -> bool {
    matches!(error, ScsiError::CheckCondition { sense, .. }
        if sense.len() >= 14 && sense[0] & 0x7f == 0x70 && sense[2] & 0xf == 3 && sense[12] == 0x14 && matches!(sense[13], 0 | 3))
}
struct CaptureTransport {
    inner: ModelTransport,
    world: SharedVirtualWorld,
    root: PathBuf,
    capture: Arc<Mutex<Option<Snapshot>>>,
    trace: Arc<Mutex<IoTrace>>,
}
impl SgTransport for CaptureTransport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.trace.lock().unwrap().commands.push(cdb.to_vec());
        let result = self.inner.execute_in(cdb, buf);
        if cdb.first() == Some(&0x08) && result.as_ref().is_err_and(missing_signature) {
            self.trace.lock().unwrap().missing_reads += 1;
        }
        result
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let mut trace = self.trace.lock().unwrap();
        trace.commands.push(cdb.to_vec());
        if trace.eod_succeeded
            && !trace.late_armed
            && cdb.first() == Some(&0x92)
            && cdb.get(4..12) == Some(&[0; 8])
        {
            if let Some(at) = trace.fault_at {
                trace.late_armed = true;
                let mut world = self.world.lock().unwrap();
                let state = world.drive_states.get_mut(&BAY).unwrap();
                state.read_sense_at = Some((at, (3, 0x14, 0)));
                state.space_sense_at = Some((at, (3, 0x14, 0)));
            }
        }
        let result = self.inner.execute_none(cdb);
        if cdb.first() == Some(&0x11) && cdb[1] & 7 == 3 && result.is_ok() {
            trace.eod_succeeded = true;
        }
        if trace.late_armed
            && cdb.first() == Some(&0x11)
            && result.as_ref().is_err_and(missing_signature)
        {
            self.world
                .lock()
                .unwrap()
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .position_unknown = true;
        }
        result
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.trace.lock().unwrap().commands.push(cdb.to_vec());
        let result = self.inner.execute_out(cdb, buf);
        if result.is_ok() && matches!(cdb.first(), Some(0x0a | 0x8a)) {
            let mut capture = self.capture.lock().unwrap();
            if capture.is_none() {
                *capture = Some(Snapshot {
                    world: self.world.lock().unwrap().clone(),
                    disk: disk(&self.root),
                });
            }
        }
        result
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class)
    }
}
#[derive(Debug)]
struct Output {
    code: ExitCode,
    out: String,
    err: String,
    trace: IoTrace,
}
fn run(
    root: &Path,
    world: &SharedVirtualWorld,
    args: &[&str],
    capture: Arc<Mutex<Option<Snapshot>>>,
) -> Output {
    run_with_fault(root, world, args, capture, None)
}
fn run_with_fault(
    root: &Path,
    world: &SharedVirtualWorld,
    args: &[&str],
    capture: Arc<Mutex<Option<Snapshot>>>,
    fault_at: Option<u64>,
) -> Output {
    let trace = Arc::new(Mutex::new(IoTrace {
        fault_at,
        ..Default::default()
    }));
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = remanence_cli::run_with_standalone_test_io(
        args,
        &root.join("home"),
        &root.join("xdg-state"),
        &root.join("absent-config.toml"),
        &root.join("locks"),
        vec![DeviceAttachment {
            sg_path: DEVICE.into(),
            sysfs_path: "/virtual/plr-sys".into(),
        }],
        |path| {
            assert_eq!(path, Path::new(DEVICE));
            Ok(Box::new(CaptureTransport {
                inner: ModelTransport::new(world.clone(), DeviceRole::Drive { bay: BAY }),
                world: world.clone(),
                root: root.into(),
                capture: capture.clone(),
                trace: trace.clone(),
            }) as Box<dyn SgTransport>)
        },
        &mut out,
        &mut err,
    );
    let trace = trace.lock().unwrap().clone();
    Output {
        trace,
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}
fn command(root: &Path, world: &SharedVirtualWorld, args: &[&str]) -> Output {
    let mut full = vec!["rem", "--allow-untrusted-state-volume", "--json"];
    full.extend_from_slice(args);
    full.extend(["--device", DEVICE, "--allow-device", SERIAL]);
    run(root, world, &full, Arc::new(Mutex::new(None)))
}
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    committed: Snapshot,
    torn: Snapshot,
    fourth_start: u64,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_owned();
        let mut world = VirtualWorld::single_drive("PLR-LIB", BAY, SERIAL, 0x400, 0);
        world.put_tape_in_drive(BAY, TAPE, None, VirtualTape::empty(1 << 30, 262144));
        let world = Arc::new(Mutex::new(world));
        let output = command(
            &root,
            &world,
            &["tape", "init", "--label", TAPE, "--generation", "L9"],
        );
        assert_eq!(output.code, ExitCode::SUCCESS, "init: {output:?}");
        for n in 0..3 {
            let path = root.join(format!("file-{n}.txt"));
            std::fs::write(&path, format!("committed file {n}: distinct bytes\n")).unwrap();
            let output = command(&root, &world, &["put", path.to_str().unwrap()]);
            assert_eq!(output.code, ExitCode::SUCCESS, "commit {n}: {output:?}");
        }
        let committed = Snapshot {
            world: world.lock().unwrap().clone(),
            disk: disk(&root),
        };
        let fourth_start = committed.world.tapes[TAPE].records.len() as u64;
        let path = root.join("file-3.txt");
        std::fs::write(&path, b"uncommitted fourth payload\n").unwrap();
        let capture = Arc::new(Mutex::new(None));
        let output = run(
            &root,
            &world,
            &[
                "rem",
                "--allow-untrusted-state-volume",
                "--json",
                "put",
                path.to_str().unwrap(),
                "--device",
                DEVICE,
                "--allow-device",
                SERIAL,
            ],
            capture.clone(),
        );
        assert_eq!(output.code, ExitCode::SUCCESS, "capture fourth: {output:?}");
        let torn = capture
            .lock()
            .unwrap()
            .take()
            .expect("fourth data WRITE captured");
        assert!(torn.world.tapes[TAPE].records.len() as u64 > fourth_start);
        assert!(torn.world.tapes[TAPE].durable_frontier <= fourth_start as usize);
        Self {
            _temp: temp,
            root,
            committed,
            torn,
            fourth_start,
        }
    }
    fn reset(&self, torn: bool, missing: bool) -> SharedVirtualWorld {
        self.reset_snapshot(if torn { &self.torn } else { &self.committed }, missing)
    }
    fn reset_snapshot(&self, snapshot: &Snapshot, missing: bool) -> SharedVirtualWorld {
        for entry in std::fs::read_dir(&self.root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                std::fs::remove_dir_all(path).unwrap()
            } else {
                std::fs::remove_file(path).unwrap()
            }
        }
        for (path, data) in &snapshot.disk {
            let path = self.root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, data).unwrap();
        }
        let mut world = snapshot.world.clone();
        let keep = world.tapes[TAPE].records.len();
        assert!(keep >= world.tapes[TAPE].durable_frontier);
        if missing {
            world.power_cut_during_write(BAY, keep).unwrap()
        } else {
            world.power_cut(BAY, keep).unwrap()
        }
        let world = Arc::new(Mutex::new(world));
        let mut drive = DriveHandle::open_standalone_with_transport(
            Path::new(DEVICE),
            Box::new(ModelTransport::new(
                world.clone(),
                DeviceRole::Drive { bay: BAY },
            )),
            &StaticAllowlist::new([SERIAL]),
        )
        .unwrap();
        assert!(drive.test_unit_ready().is_err());
        drive.test_unit_ready().unwrap();
        drive.locate(0).unwrap();
        world.lock().unwrap().command_log.clear();
        world
    }
}
#[test]
fn marked_goldens() {
    let f = Fixture::new();
    println!("independent fourth_start={}", f.fourth_start);
    for torn in [false, true] {
        for args in [
            vec!["tape", "identify"],
            vec!["tape", "restore", "--list"],
            vec!["tape", "restore", "--scan", "--list"],
        ] {
            let world = f.reset(torn, false);
            let output = command(&f.root, &world, &args);
            println!(
                "marked torn={torn} args={args:?} stdout={} stderr={}",
                output.out, output.err
            );
            assert_eq!(output.code, ExitCode::SUCCESS, "marked golden: {output:?}");
        }
        for scan in [false, true] {
            let world = f.reset(torn, false);
            let out = f.root.join("restore-out");
            let mut args = vec!["tape", "restore", "--out", out.to_str().unwrap()];
            if scan {
                args.push("--scan");
            }
            let output = command(&f.root, &world, &args);
            println!(
                "marked extraction torn={torn} scan={scan} stdout={} stderr={}",
                output.out, output.err
            );
            assert_eq!(
                output.code,
                if torn {
                    ExitCode::from(1)
                } else {
                    ExitCode::SUCCESS
                },
                "marked extraction: {output:?}"
            );
            for n in 0..3 {
                assert_eq!(
                    std::fs::read(
                        out.join(format!("candidate-{}", n + 1))
                            .join(format!("file-{n}.txt"))
                    )
                    .unwrap(),
                    format!("committed file {n}: distinct bytes\n").as_bytes()
                );
            }
            assert!(!out.join("candidate-4/file-3.txt").exists());
        }
    }
}

fn envelope(output: &Output) -> serde_json::Value {
    serde_json::from_str(
        output
            .out
            .lines()
            .rev()
            .find(|line| line.starts_with('{'))
            .expect("JSON envelope"),
    )
    .unwrap()
}
fn report(output: &Output) -> serde_json::Value {
    let value = envelope(output);
    if value["ok"] == true {
        value["result"].clone()
    } else {
        serde_json::from_str(
            value["error"]
                .as_str()
                .unwrap()
                .strip_prefix("restore incomplete: ")
                .expect("extraction reached incomplete report"),
        )
        .unwrap()
    }
}
fn assert_files(root: &Path) {
    for n in 0..3 {
        assert_eq!(
            std::fs::read(
                root.join(format!("candidate-{}", n + 1))
                    .join(format!("file-{n}.txt"))
            )
            .unwrap(),
            format!("committed file {n}: distinct bytes\n").as_bytes()
        );
    }
    assert!(!root.join("candidate-4/file-3.txt").exists());
}
fn assert_mark(report: &serde_json::Value, position: u64) {
    assert_eq!(report["end_of_data_mark"], "not_found");
    assert_eq!(
        report["unresolved_from"],
        serde_json::json!({"partition":0,"lba":position.to_string()}),
        "report preserves independently captured unresolved position"
    );
}
fn no_write(output: &Output) {
    assert!(
        !output
            .trace
            .commands
            .iter()
            .any(|c| matches!(c.first(), Some(0x0a | 0x8a | 0x10 | 0x80))),
        "new put refused before WRITE/WFM: {} {}",
        output.out,
        output.err
    );
}
fn config(root: &Path) -> PathBuf {
    root.join("xdg-state/rem/config.toml")
}
fn index(root: &Path) -> remanence_state::CatalogIndex {
    let cfg = remanence_state::load_config(config(root)).unwrap();
    remanence_state::projector::open(cfg.index.sqlite_path)
        .unwrap()
        .write_handle()
}

#[test]
fn missing_mark_identify_succeeds() {
    let f = Fixture::new();
    for torn in [false, true] {
        let world = f.reset(torn, true);
        let output = command(&f.root, &world, &["tape", "identify"]);
        assert_eq!(
            output.code,
            ExitCode::SUCCESS,
            "identify succeeds: {} {}",
            output.out,
            output.err
        );
        let value = envelope(&output);
        assert_eq!(value["ok"], true);
        let result = &value["result"];
        assert_eq!(result["status"], "end_of_data_not_found");
        assert_eq!(result["end_of_data_mark"], "not_found");
        assert_eq!(result["finalized"], false);
        assert_eq!(result["object_count"], 3);
        assert_eq!(result["bytes"], 2359296);
        assert_eq!(result["inventory_source"], "catalog");
        for warning in [
            "drive reports no end-of-data mark",
            "power loss during writing can cause this",
            "committed files remain readable",
            "rem tape restore --scan --list",
            "not the actual end",
        ] {
            assert!(output.err.contains(warning), "{output:?}");
        }
        let world = f.reset(torn, true);
        let human = run(
            &f.root,
            &world,
            &[
                "rem",
                "--allow-untrusted-state-volume",
                "tape",
                "identify",
                "--device",
                DEVICE,
                "--allow-device",
                SERIAL,
            ],
            Arc::new(Mutex::new(None)),
        );
        assert_eq!(human.code, ExitCode::SUCCESS);
        assert!(human.err.contains("drive reports no end-of-data mark"));
        assert!(human.out.contains("end_of_data_not_found"));
    }
}

#[test]
fn missing_mark_scan_restore_produces_report() {
    let f = Fixture::new();
    for torn in [false, true] {
        for scan in [false, true] {
            let world = f.reset(torn, true);
            let mut args = vec!["tape", "restore", "--list"];
            if scan {
                args.push("--scan");
            }
            let output = command(&f.root, &world, &args);
            assert_eq!(
                output.code,
                ExitCode::SUCCESS,
                "scan restore produces report: {} {}",
                output.out,
                output.err
            );
            let report = report(&output);
            assert_mark(&report, f.fourth_start);
            assert_eq!(report["terminal_index"], false);
            let rows = report["candidates"].as_array().unwrap();
            assert_eq!(rows.len(), 3 + usize::from(torn));
            for (n, row) in rows.iter().take(3).enumerate() {
                assert_eq!(row["status"], "listed");
                assert_eq!(row["manifest"]["rows"][0]["path"], format!("file-{n}.txt"));
                assert_eq!(row["manifest"]["rows"][0]["size"], 33);
            }
            if torn {
                assert_eq!(rows[3]["status"], "refused");
                assert_eq!(rows[3]["error"],"incomplete candidate; closing filemark could not be established; remaining tape unresolved");
            }
        }
    }
}

#[test]
fn missing_mark_extraction_and_committed_get_keep_bytes() {
    let f = Fixture::new();
    for torn in [false, true] {
        for scan in [false, true] {
            let world = f.reset(torn, true);
            let destination = f.root.join("extracted");
            let mut args = vec!["tape", "restore", "--out", destination.to_str().unwrap()];
            if scan {
                args.push("--scan");
            }
            let output = command(&f.root, &world, &args);
            assert_eq!(
                output.code,
                if torn {
                    ExitCode::from(1)
                } else {
                    ExitCode::SUCCESS
                },
                "{} {}",
                output.out,
                output.err
            );
            assert_mark(&report(&output), f.fourth_start);
            assert_files(&destination);
        }
        for n in 0..3 {
            let world = f.reset(torn, true);
            let destination = f.root.join("get");
            let name = format!("file-{n}.txt");
            let output = command(
                &f.root,
                &world,
                &["get", &name, "--out", destination.to_str().unwrap()],
            );
            assert_eq!(
                output.code,
                ExitCode::SUCCESS,
                "{} {}",
                output.out,
                output.err
            );
            assert_eq!(
                std::fs::read(destination.join(&name)).unwrap(),
                format!("committed file {n}: distinct bytes\n").as_bytes()
            );
        }
    }
}

#[test]
fn new_put_finalize_and_supersede_refuse_missing_mark() {
    let f = Fixture::new();
    for action in ["put", "finalize", "supersede-tail"] {
        let world = f.reset(false, true);
        let catalog = index(&f.root);
        assert!(catalog.list_active_tape_io_fences().unwrap().is_empty());
        assert_eq!(
            catalog
                .get_tape_by_voltag(TAPE)
                .unwrap()
                .unwrap()
                .pool_id
                .as_deref(),
            Some("default")
        );
        drop(catalog);
        assert!(!world.lock().unwrap().tapes[TAPE].write_protected);
        let new = f.root.join("new-request-distinct-content.txt");
        std::fs::write(&new, b"PLR fresh content that is not already stored").unwrap();
        let ack = format!("SUPERSEDE {TAPE}");
        let args = match action {
            "put" => vec!["put", new.to_str().unwrap()],
            "finalize" => vec!["tape", "finalize", "--acknowledge-tape", TAPE],
            _ => vec![
                "tape",
                "supersede-tail",
                "--acknowledge-tape",
                TAPE,
                "--confirm-supersede",
                &ack,
            ],
        };
        let output = command(&f.root, &world, &args);
        assert_eq!(
            output.code,
            ExitCode::from(1),
            "new put refused before WRITE/WFM: {} {}",
            output.out,
            output.err
        );
        no_write(&output);
        for line in output.out.lines().filter(|line| line.starts_with('{')) {
            let item: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_ne!(
                item["event"], "already_stored",
                "fresh content cannot take already_stored shortcut"
            );
            if item["event"] == "summary" {
                assert_eq!(item["already_stored"], 0);
            }
        }
        println!(
            "admission {action}: missing_reads={} output={} stderr={}",
            output.trace.missing_reads, output.out, output.err
        );
        // Put and supersede-tail are refusal-only evidence: identity invalidation
        // refuses first. Finalize reaches the append-suffix guard.
        if action == "put" {
            assert!(
                output
                    .trace
                    .commands
                    .iter()
                    .any(|c| c.first() == Some(&0x08)),
                "read admission reached media"
            );
            assert!(
                output.trace.missing_reads >= 1,
                "put reads the missing mark"
            );
            let summary = output
                .out
                .lines()
                .filter(|line| line.starts_with('{'))
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .find(|item| item["event"] == "summary")
                .expect("put emits its failed-item summary");
            let error = summary["failed"]["error"]
                .as_str()
                .expect("failed put item names its error");
            assert!(
                error.contains("tape identity invalidated"),
                "put refused by identity invalidation: {error}"
            );
        } else {
            let envelope = envelope(&output);
            let error = envelope["error"].as_str().unwrap();
            if action == "finalize" {
                assert!(
                    error.contains("append suffix"),
                    "finalize refused by append suffix: {error}"
                );
            } else {
                assert!(
                    error.contains("tape identity invalidated"),
                    "supersede-tail refused by identity invalidation: {error}"
                );
            }
        }
    }
}

#[test]
fn standalone_verification_initial_and_post_damage_refuse() {
    let f = Fixture::new();
    for (full, after_damage) in [(false, false), (true, false), (true, true)] {
        let world = f.reset(false, true);
        if after_damage {
            world
                .lock()
                .unwrap()
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .normal_eod_spaces = 1;
        }
        let mut args = vec!["tape", "verify"];
        if full {
            args.push("--full");
        }
        let output = command(&f.root, &world, &args);
        assert_eq!(output.code, ExitCode::from(1));
        no_write(&output);
        let eods: Vec<_> = output
            .trace
            .commands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.first() == Some(&0x11) && c[1] & 7 == 3)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(eods.len(), if after_damage { 2 } else { 1 });
        assert!(
            !output.trace.commands[eods.last().unwrap() + 1..]
                .iter()
                .any(|c| matches!(c.first(), Some(0x92 | 0x2b | 0x08 | 0x11))),
            "no positioning after refused SPACE"
        );
        let error = envelope(&output)["error"].as_str().unwrap().to_owned();
        if full {
            assert!(error.contains("SPACE(EOD)"));
            assert!(
                error.contains("drive reports no end-of-data mark"),
                "diagnostic names missing mark: {error}"
            );
            if after_damage {
                assert!(
                    error.contains("sense=[70, 00, 03, 00, 00, 00, 00, 18, 00, 00, 00, 00, 14, 00"),
                    "post-damage refusal preserves drive sense: {error}"
                );
            }
        } else {
            assert_eq!(
                error,
                "verification failed: no usable terminal index; end_of_data_not_found"
            );
        }
    }
}

#[test]
fn finalized_catalog_override_and_late_prefix_verification() {
    let f = Fixture::new();
    let world = f.reset(false, false);
    let output = command(
        &f.root,
        &world,
        &["tape", "finalize", "--acknowledge-tape", TAPE],
    );
    assert_eq!(
        output.code,
        ExitCode::SUCCESS,
        "finalized fixture: {} {}",
        output.out,
        output.err
    );
    let finalized = Snapshot {
        world: world.lock().unwrap().clone(),
        disk: disk(&f.root),
    };
    let world = f.reset_snapshot(&finalized, true);
    let output = command(&f.root, &world, &["tape", "identify"]);
    assert_eq!(output.code, ExitCode::SUCCESS);
    let v = envelope(&output);
    assert_eq!(v["result"]["finalized"], true);
    assert_eq!(v["result"]["end_of_data_mark"], "not_found");
    assert_eq!(v["result"]["physical_status"], "end_of_data_not_found");
    assert_eq!(v["result"]["object_count"], 3);
    let world = f.reset_snapshot(&finalized, false);
    let output = run_with_fault(
        &f.root,
        &world,
        &[
            "rem",
            "--allow-untrusted-state-volume",
            "--json",
            "tape",
            "verify",
            "--full",
            "--device",
            DEVICE,
            "--allow-device",
            SERIAL,
        ],
        Arc::new(Mutex::new(None)),
        Some(3),
    );
    assert!(
        output.trace.eod_succeeded && output.trace.late_armed,
        "layout selection precedes prefix fault"
    );
    assert_eq!(output.code, ExitCode::from(1));
    let e = envelope(&output)["error"].as_str().unwrap().to_owned();
    assert!(
        e.contains("physical prefix walk failed"),
        "prefix error variant: {e}"
    );
    assert!(
        e.contains("drive reports no end-of-data mark"),
        "diagnostic names missing mark: {e}"
    );
    let world = f.reset_snapshot(&finalized, true);
    let path = f.root.join("fresh-finalized.txt");
    std::fs::write(&path, b"new finalized refusal content").unwrap();
    let output = command(&f.root, &world, &["put", path.to_str().unwrap()]);
    assert_eq!(output.code, ExitCode::from(1));
    no_write(&output);
}

#[test]
fn fenced_rescue_keeps_fence_and_write_refusal() {
    use remanence_state::library_log as lib;
    let f = Fixture::new();
    for cause in [
        lib::FenceCause::PartialBatch,
        lib::FenceCause::NativeLogBehindProjection,
    ] {
        let world = f.reset(false, true);
        let state = remanence_state::StateHandle::open_from_config_file(config(&f.root)).unwrap();
        let tape = state.catalog().get_tape_by_voltag(TAPE).unwrap().unwrap();
        let id = tape.tape_uuid.as_slice().try_into().unwrap();
        state
            .library_runtime()
            .append(|a, v| {
                a.fence_raised(
                    lib::FenceRaised {
                        kind: lib::FenceKind::Write,
                        scope: lib::FenceScope::Tape(lib::TapeRef {
                            uuid: id,
                            incarnation: 1,
                        }),
                        cause,
                        evidence: None,
                        at: None,
                        operation: None,
                        detail: lib::Bounded(vec![]),
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap();
        drop(state);
        let before = index(&f.root).list_active_tape_io_fences().unwrap();
        assert_eq!(before.len(), 1);
        let snapshot = Snapshot {
            world: world.lock().unwrap().clone(),
            disk: disk(&f.root),
        };
        for scan in [false, true] {
            let world = f.reset_snapshot(&snapshot, true);
            let mut args = vec!["tape", "restore", "--list"];
            if scan {
                args.push("--scan");
            }
            let output = command(&f.root, &world, &args);
            let admitted = scan || cause != lib::FenceCause::NativeLogBehindProjection;
            assert_eq!(
                output.code,
                if admitted {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                },
                "rescue admission preserved: {} {}",
                output.out,
                output.err
            );
            if admitted {
                assert_mark(&report(&output), f.fourth_start);
            } else {
                assert!(envelope(&output)["error"]
                    .as_str()
                    .unwrap()
                    .contains("native_log_behind_projection"));
            }
            assert_eq!(
                format!("{:?}", index(&f.root).list_active_tape_io_fences().unwrap()),
                format!("{before:?}")
            );
        }
        let world = f.reset_snapshot(&snapshot, true);
        let fresh = f.root.join("new-fenced-request.txt");
        std::fs::write(&fresh, b"new fenced payload").unwrap();
        let output = command(&f.root, &world, &["put", fresh.to_str().unwrap()]);
        assert_eq!(output.code, ExitCode::from(1));
        no_write(&output);
        assert_eq!(
            format!("{:?}", index(&f.root).list_active_tape_io_fences().unwrap()),
            format!("{before:?}")
        );
    }
}
