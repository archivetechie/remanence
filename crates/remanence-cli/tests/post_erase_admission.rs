//! PLR-T3: real standalone sessions prove post-ERASE resume and READ refusal.
//! The model cut returns to the live actor; all host history is production-written.
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record as MediaRecord, SharedVirtualWorld, VirtualTape,
    VirtualWorld,
};
use remanence_library::{
    scsi::{decode_usable_sense, DecodedSense},
    transport::TransferOutcome,
    DeviceAttachment, DriveHandle, ScsiError, SgTransport, SpaceKind, StaticAllowlist,
    TimeoutClass,
};
use remanence_state::tape_log::{Record, SessionEndReason, SessionEnded, TailIdentity};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
};
const DEVICE: &str = "/virtual/plr-t3-drive";
const SERIAL: &str = "PLR-T3-DRIVE";
const TAPE: &str = "PLRT03L9";
const BAY: u16 = 0x100;
const BLOCK: usize = 262144;

type RepairedIdentity = ([u8; 16], TailIdentity, [u8; 32], u64, [u8; 32]);

#[derive(Clone)]
struct Snapshot {
    world: VirtualWorld,
    disk: BTreeMap<PathBuf, Vec<u8>>,
    identity: RepairedIdentity,
}
/// Bind the native ending and boundary to a tagged, length-delimited media digest.
fn repaired_identity(root: &Path, id: [u8; 16], world: &VirtualWorld) -> RepairedIdentity {
    let state = native(root, id);
    let ending = state.summary().last_session_end.as_ref().unwrap();
    let mut tails = Vec::new();
    state
        .visit_last_session_tail(|tail| {
            tails.push(tail.identity.clone());
            Ok(())
        })
        .unwrap();
    assert_eq!(tails.len(), 1);
    assert!(matches!(&tails[0], TailIdentity::Object(_)));
    let records = &world.tapes[TAPE].records;
    let mut hash = Sha256::new();
    hash.update((records.len() as u64).to_be_bytes());
    for record in records {
        match record {
            MediaRecord::Block(bytes) => {
                hash.update([0]);
                hash.update((bytes.len() as u64).to_be_bytes());
                hash.update(bytes);
            }
            MediaRecord::ZeroBlock(size) => {
                hash.update([1]);
                hash.update(size.to_be_bytes());
            }
            MediaRecord::Filemark => hash.update([2]),
        }
    }
    (
        ending.session_id,
        tails.remove(0),
        ending.tail.digest,
        state.summary().cursor.lba,
        hash.finalize().into(),
    )
}
fn disk(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
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
#[derive(Clone, Debug)]
struct Command {
    cdb: Vec<u8>,
    lba: Option<u64>,
    buffer: usize,
    sense: Option<DecodedSense>,
    ok: bool,
}
#[derive(Clone, Debug, Default)]
struct Trace {
    commands: Vec<Command>,
    cut_after: Option<usize>,
    data_writes: usize,
    cut_keep: Option<usize>,
}
fn sense(error: &ScsiError) -> Option<DecodedSense> {
    match error {
        ScsiError::CheckCondition { sense, .. } => decode_usable_sense(sense),
        _ => None,
    }
}
struct CaptureTransport {
    inner: ModelTransport,
    world: SharedVirtualWorld,
    trace: Arc<Mutex<Trace>>,
    position: Option<u64>,
}
impl SgTransport for CaptureTransport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        let lba = self.position;
        let result = self.inner.execute_in(cdb, buf);
        self.trace.lock().unwrap().commands.push(Command {
            cdb: cdb.to_vec(),
            lba,
            buffer: buf.len(),
            sense: result.as_ref().err().and_then(sense),
            ok: result.is_ok(),
        });
        if cdb[0] == 0x34 && result.is_ok() && buf.len() >= 16 && buf[0] & 4 == 0 {
            self.position = Some(u64::from_be_bytes(buf[8..16].try_into().unwrap()));
        }
        if cdb[0] == 0x08 {
            if result.is_ok() && cdb[1] & 1 == 0 {
                self.position = self.position.map(|p| p + 1);
            } else if let Err(ScsiError::CheckCondition { sense, .. }) = &result {
                if decode_usable_sense(sense).is_some_and(|s| s.filemark) {
                    self.position = self.position.map(|p| p + 1);
                }
            }
        }
        result
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let result = self.inner.execute_none(cdb);
        self.trace.lock().unwrap().commands.push(Command {
            cdb: cdb.to_vec(),
            lba: self.position,
            buffer: 0,
            sense: result.as_ref().err().and_then(sense),
            ok: result.is_ok(),
        });
        if result.is_ok() {
            match cdb[0] {
                0x01 => self.position = Some(0),
                0x92 => self.position = Some(u64::from_be_bytes(cdb[4..12].try_into().unwrap())),
                _ => {}
            }
        }
        result
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        let result = self.inner.execute_out(cdb, buf);
        let mut trace = self.trace.lock().unwrap();
        trace.commands.push(Command {
            cdb: cdb.to_vec(),
            lba: self.position,
            buffer: buf.len(),
            sense: result.as_ref().err().and_then(sense),
            ok: result.is_ok(),
        });
        if cdb[0] == 0x0a && buf.len() == BLOCK && result.is_ok() {
            assert_eq!(cdb[1] & 1, 0, "object-data WRITE is variable-block");
            trace.data_writes += 1;
            self.position = self.position.map(|p| p + 1);
            if trace.cut_after == Some(trace.data_writes) {
                let mut world = self.world.lock().unwrap();
                let keep = world.tapes[TAPE].records.len();
                assert!(
                    keep > world.tapes[TAPE].durable_frontier,
                    "cut retains unsynchronized data"
                );
                world.power_cut_during_write(BAY, keep).unwrap();
                trace.cut_keep = Some(keep);
                return Err(ScsiError::TransportError {
                    status: 0,
                    host_status: 0x000b,
                    driver_status: 0,
                    info: 1,
                    sense: vec![],
                });
            }
        }
        result
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}
#[derive(Debug)]
struct Output {
    code: ExitCode,
    out: String,
    err: String,
    trace: Trace,
}
fn command(root: &Path, world: &SharedVirtualWorld, args: &[&str], cut: Option<usize>) -> Output {
    let trace = Arc::new(Mutex::new(Trace {
        cut_after: cut,
        ..Default::default()
    }));
    let mut full = vec!["rem", "--allow-untrusted-state-volume", "--json"];
    full.extend_from_slice(args);
    full.extend(["--device", DEVICE, "--allow-device", SERIAL]);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = remanence_cli::run_with_standalone_test_io(
        &full,
        &root.join("home"),
        &root.join("xdg-state"),
        &root.join("absent-config.toml"),
        &root.join("locks"),
        vec![DeviceAttachment {
            sg_path: DEVICE.into(),
            sysfs_path: "/virtual/plr-t3-sys".into(),
        }],
        |path| {
            assert_eq!(path, Path::new(DEVICE));
            Ok(Box::new(CaptureTransport {
                inner: ModelTransport::new(world.clone(), DeviceRole::Drive { bay: BAY }),
                world: world.clone(),
                trace: trace.clone(),
                position: Some(0),
            }) as Box<dyn SgTransport>)
        },
        &mut out,
        &mut err,
    );
    let trace = trace.lock().unwrap().clone();
    Output {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
        trace,
    }
}
fn result(output: &Output) -> serde_json::Value {
    let envelope: serde_json::Value = serde_json::from_str(
        output
            .out
            .lines()
            .rev()
            .find(|s| s.starts_with('{'))
            .expect("JSON envelope"),
    )
    .unwrap();
    assert_eq!(envelope["ok"], true, "{output:?}");
    envelope["result"].clone()
}
fn config(root: &Path) -> remanence_state::RemConfig {
    remanence_state::load_config(root.join("xdg-state/rem/config.toml")).unwrap()
}
fn native(root: &Path, id: [u8; 16]) -> remanence_state::tape_log::snapshot::Snapshot {
    let cfg = config(root);
    remanence_state::tape_log::store::Store::open(
        cfg.journal.dir.join("tapes"),
        id,
        &cfg.index.sqlite_path,
    )
    .unwrap()
    .snapshot()
    .unwrap()
}
fn history(root: &Path, id: [u8; 16]) -> Vec<Record> {
    let mut rows = Vec::new();
    native(root, id)
        .visit_frames(|f| {
            rows.push(f.record.clone());
            Ok(())
        })
        .unwrap();
    rows
}
fn put(root: &Path, world: &SharedVirtualWorld, n: usize) -> serde_json::Value {
    let path = root.join(format!("file-{n}.bin"));
    std::fs::write(&path, format!("committed file {n}: unique bytes\n")).unwrap();
    let output = command(root, world, &["put", path.to_str().unwrap()], None);
    assert_eq!(output.code, ExitCode::SUCCESS, "put {n}: {output:?}");
    let data = result(&output);
    let receipts = data["receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 1, "one fresh receipt: {data}");
    assert_eq!(receipts[0]["event"], "written", "{data}");
    receipts[0].clone()
}
fn get_first(root: &Path, world: &SharedVirtualWorld) {
    let out = root.join("readback");
    if out.exists() {
        std::fs::remove_dir_all(&out).unwrap();
    }
    let output = command(
        root,
        world,
        &["get", "file-1.bin", "--out", out.to_str().unwrap()],
        None,
    );
    assert_eq!(output.code, ExitCode::SUCCESS, "get: {output:?}");
    assert_eq!(
        std::fs::read(out.join("file-1.bin")).unwrap(),
        b"committed file 1: unique bytes\n"
    );
}
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    repaired: Snapshot,
    c: u64,
    id: [u8; 16],
    ending: SessionEnded,
    prefix: Vec<MediaRecord>,
    receipts: Vec<serde_json::Value>,
}
/// Serialize both branches while they restore the same absolute host paths.
fn repaired_fixture() -> MutexGuard<'static, Fixture> {
    // Statics are never dropped: the fixture's TempDir remains in TMPDIR until
    // the test-run harness removes TMPDIR after the process exits.
    static FIXTURE: OnceLock<Mutex<Fixture>> = OnceLock::new();
    FIXTURE
        .get_or_init(|| Mutex::new(Fixture::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_owned();
        let mut world = VirtualWorld::single_drive("PLR-T3-LIB", BAY, SERIAL, 0x400, 0);
        world.put_tape_in_drive(BAY, TAPE, None, VirtualTape::empty(1 << 30, BLOCK as u32));
        let world = Arc::new(Mutex::new(world));
        let output = command(
            &root,
            &world,
            &["tape", "init", "--label", TAPE, "--generation", "L9"],
            None,
        );
        assert_eq!(output.code, ExitCode::SUCCESS, "init: {output:?}");
        let mut receipts = Vec::new();
        let mut hashes = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for n in 1..=8 {
            let receipt = put(&root, &world, n);
            assert!(ids.insert(receipt["object_id"].as_str().unwrap().to_owned()));
            assert!(hashes.insert(
                Sha256::digest(std::fs::read(root.join(format!("file-{n}.bin"))).unwrap()).to_vec()
            ));
            receipts.push(receipt);
        }
        println!("committed receipts={receipts:?}; SHA256 hashes={hashes:02x?}");
        let id = *uuid::Uuid::parse_str(receipts[0]["tape_uuid"].as_str().unwrap())
            .unwrap()
            .as_bytes();
        let before = native(&root, id);
        let summary = before.summary();
        let c = summary.cursor.lba;
        assert_eq!(summary.block_size, BLOCK as u32);
        let scheme = summary.scheme.as_ref().unwrap();
        assert_eq!(
            (
                scheme.data_blocks_per_stripe,
                scheme.parity_blocks_per_stripe,
                scheme.stripes_per_neighborhood
            ),
            (128, 4, 1)
        );
        assert!(
            !summary.closeout_due(),
            "eight successfully finished puts closed out"
        );
        assert_eq!(summary.object_count, 8);
        assert!(summary.open_session.is_none());
        let config_text = std::fs::read_to_string(root.join("xdg-state/rem/config.toml")).unwrap();
        assert!(config_text.contains("custom:128,4,1"));
        let prior_history = history(&root, id);
        assert_eq!(
            prior_history
                .iter()
                .filter(|r| matches!(r, Record::SessionOpened(_)))
                .count(),
            8
        );
        assert_eq!(prior_history.iter().filter(|r|matches!(r,Record::SessionEnded(e) if e.reason==SessionEndReason::CleanClose)).count(),8);
        let prefix = world.lock().unwrap().tapes[TAPE].records[..c as usize].to_vec();
        assert_eq!(prefix.last(), Some(&MediaRecord::Filemark));
        let ninth = root.join("file-9.bin");
        let mut seed = 0x123456789abcdefu64;
        let bytes: Vec<u8> = (0..BLOCK * 5 + 123)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        assert!(bytes.len() > BLOCK);
        std::fs::write(&ninth, bytes).unwrap();
        let output = command(&root, &world, &["put", ninth.to_str().unwrap()], Some(3));
        assert_ne!(output.code, ExitCode::SUCCESS, "cut must fail: {output:?}");
        assert_eq!(output.trace.data_writes, 3);
        let cut_keep = output.trace.cut_keep.unwrap();
        assert!(cut_keep as u64 > c);
        let writes: Vec<_> = output
            .trace
            .commands
            .iter()
            .filter(|v| v.cdb[0] == 0x0a)
            .collect();
        assert_eq!(writes.len(), 3);
        assert!(writes
            .iter()
            .all(|v| v.buffer == BLOCK && v.cdb[1] & 1 == 0 && v.ok));
        assert!(
            !output
                .trace
                .commands
                .iter()
                .any(|v| matches!(v.cdb[0], 0x10 | 0x80)),
            "cut before synchronizing filemark"
        );
        let after = native(&root, id);
        let summary = after.summary();
        assert_eq!(summary.cursor.lba, c);
        assert!(!summary.closeout_due());
        assert!(summary.open_session.is_none());
        assert_eq!(summary.object_count, 8);
        let ending = summary
            .last_session_end
            .clone()
            .expect("live actor wrote native ending");
        assert_eq!(ending.reason, SessionEndReason::DriveError);
        assert!(ending.possible_tail_blocks > 0);
        assert_eq!(ending.synchronized_tail_blocks, 0);
        assert_eq!(ending.proved_eod, None);
        assert_eq!(ending.cursor.lba, c);
        let mut tails = Vec::new();
        after
            .visit_last_session_tail(|f| {
                tails.push(f.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(tails.len(), 1);
        assert!(matches!(&tails[0].identity,TailIdentity::Object(id) if !ids.contains(id)));
        assert!(!tails[0].synchronized_filemark);
        assert_eq!(tails[0].synchronized_blocks, 0);
        assert!(
            summary.last_ordinary_commit.is_some() && !summary.sealed && summary.seal_due.is_none()
        );
        let cfg = config(&root);
        let index = remanence_state::projector::open(cfg.index.sqlite_path)
            .unwrap()
            .write_handle();
        assert!(
            index.list_active_tape_io_fences().unwrap().is_empty(),
            "eligible unfenced checkpoint"
        );
        drop(index);
        let host_before = disk(&root);
        let mut drive = DriveHandle::open_standalone_with_transport(
            Path::new(DEVICE),
            Box::new(ModelTransport::new(
                world.clone(),
                DeviceRole::Drive { bay: BAY },
            )),
            &StaticAllowlist::new([SERIAL]),
        )
        .unwrap();
        match drive.test_unit_ready() {
            Ok(()) => {}
            Err(e) => {
                assert_eq!(
                    e.completion_evidence()
                        .decoded
                        .as_ref()
                        .map(|s| (s.key, s.asc, s.ascq)),
                    Some((6, 0x29, 0)),
                    "reset: {e}"
                );
                drive.test_unit_ready().unwrap();
            }
        }
        drive.locate(c).unwrap();
        {
            let mut world = world.lock().unwrap();
            let tape = world.tapes.get_mut(TAPE).unwrap();
            tape.records.truncate(c as usize);
            assert_eq!(tape.records.last(), Some(&MediaRecord::Filemark));
            tape.durable_frontier = c as usize;
            tape.written_bytes = tape
                .records
                .iter()
                .map(|r| match r {
                    MediaRecord::Block(b) => b.len() as u64,
                    MediaRecord::ZeroBlock(n) => u64::from(*n),
                    MediaRecord::Filemark => 0,
                })
                .sum();
            tape.missing_eod_sense = None;
        }
        assert_eq!(disk(&root), host_before, "ERASE does not alter host state");
        drive.locate(0).unwrap();
        drive.space(0, SpaceKind::EndOfData).unwrap();
        assert_eq!(drive.position().unwrap().lba, c);
        drop(drive);
        let output = command(&root, &world, &["tape", "identify"], None);
        assert_eq!(output.code, ExitCode::SUCCESS, "identify: {output:?}");
        assert_eq!(result(&output)["object_count"], 8);
        get_first(&root, &world);
        let repaired_world = world.lock().unwrap().clone();
        let repaired = Snapshot {
            identity: repaired_identity(&root, id, &repaired_world),
            world: repaired_world,
            disk: disk(&root),
        };
        println!("fixture: scheme=custom:128,4,1 block={BLOCK} eight_finished=8 C={c} ninth_variable_writes={} keep={} ending={ending:?} tail={tails:?}; SPACE(BOT->EOD)={c}; identify=8; get1=identical",writes.len(),cut_keep);
        Self {
            _temp: temp,
            root,
            repaired,
            c,
            id,
            ending,
            prefix,
            receipts,
        }
    }
    fn clone_repaired(&self) -> SharedVirtualWorld {
        for entry in std::fs::read_dir(&self.root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                std::fs::remove_dir_all(path).unwrap();
            } else {
                std::fs::remove_file(path).unwrap();
            }
        }
        for (path, bytes) in &self.repaired.disk {
            let path = self.root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let world = Arc::new(Mutex::new(self.repaired.world.clone()));
        let identity = repaired_identity(&self.root, self.id, &world.lock().unwrap());
        assert_eq!(identity, self.repaired.identity, "same repaired snapshot");
        println!("restored repaired snapshot: {identity:?}");
        world
    }
}
#[test]
fn post_erase_resumes() {
    let f = repaired_fixture();
    let world = f.clone_repaired();
    let mut ids: BTreeSet<_> = f
        .receipts
        .iter()
        .map(|r| r["object_id"].as_str().unwrap().to_owned())
        .collect();
    for n in 10..=12 {
        let receipt = put(&f.root, &world, n);
        assert!(ids.insert(receipt["object_id"].as_str().unwrap().into()));
    }
    assert_eq!(ids.len(), 11);
    assert_eq!(native(&f.root, f.id).summary().object_count, 11);
    let output = command(&f.root, &world, &["tape", "identify"], None);
    assert_eq!(output.code, ExitCode::SUCCESS);
    assert_eq!(result(&output)["object_count"], 11);
    assert_eq!(
        &world.lock().unwrap().tapes[TAPE].records[..f.c as usize],
        f.prefix.as_slice()
    );
    get_first(&f.root, &world);
    let rows = history(&f.root, f.id);
    let original = rows
        .iter()
        .position(|r| matches!(r,Record::SessionEnded(e) if e==&f.ending))
        .expect("original DriveError ending retained");
    let next_open = rows
        .iter()
        .enumerate()
        .skip(original + 1)
        .find(|(_, r)| matches!(r, Record::SessionOpened(_)))
        .map(|(n, _)| n)
        .unwrap();
    let mut refined = f.ending.clone();
    refined.proved_eod = Some(f.c);
    assert!(
        rows[original + 1..next_open]
            .iter()
            .any(|r| matches!(r,Record::SessionEnded(e) if e==&refined)),
        "prior ending refined to C"
    );
    println!("resume: original DriveError/None at {original}, matching Some({}) before SessionOpened at {next_open}; eleven distinct receipts; prefix and file1 unchanged",f.c);
}
#[test]
fn post_erase_read_refusal() {
    let f = repaired_fixture();
    let world = f.clone_repaired();
    {
        let mut w = world.lock().unwrap();
        let state = w.drive_states.get_mut(&BAY).unwrap();
        assert!(state.space_sense_at.is_none());
        state.read_sense_at = Some((f.c, (3, 0x14, 0)));
    }
    let before = world.lock().unwrap().tapes[TAPE].clone();
    let path = f.root.join("fresh-refused.bin");
    std::fs::write(&path, b"a fresh object must refuse\n").unwrap();
    let output = command(&f.root, &world, &["put", path.to_str().unwrap()], None);
    assert_ne!(output.code, ExitCode::SUCCESS, "{output:?}");
    let after = world.lock().unwrap().tapes[TAPE].clone();
    assert_eq!(after.records, before.records);
    assert_eq!(after.durable_frontier, before.durable_frontier);
    assert_eq!(after.records.len() as u64, f.c);
    assert_eq!(after.missing_eod_sense, None);
    assert_eq!(
        output
            .trace
            .commands
            .iter()
            .filter(|v| matches!(v.cdb[0], 0x0a | 0x8a | 0x10 | 0x80))
            .count(),
        0,
        "no WRITE/WFM before refusal"
    );
    let failed: Vec<_> = output
        .trace
        .commands
        .iter()
        .enumerate()
        .filter(|(_, v)| v.cdb[0] == 0x08 && v.lba == Some(f.c))
        .collect();
    assert_eq!(failed.len(), 1);
    let (at, read) = failed[0];
    assert_eq!(read.lba, Some(f.c));
    assert_eq!(read.buffer, BLOCK);
    assert_eq!(
        read.sense.map(|s| (s.key, s.asc, s.ascq, s.is_deferred())),
        Some((3, 0x14, 0, false)),
        "READ C decoded sense"
    );
    let before_read = &output.trace.commands[..at];
    let predecessor_reads: Vec<_> = before_read
        .iter()
        .enumerate()
        .filter(|(_, v)| v.cdb[0] == 0x08 && v.lba == Some(f.c - 1))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        predecessor_reads.len(),
        3,
        "parity reopening and admission predecessor proofs: {output:?}"
    );
    let predecessor_locates: Vec<_> = before_read
        .iter()
        .enumerate()
        .filter(|(_, v)| {
            v.cdb[0] == 0x92 && u64::from_be_bytes(v.cdb[4..12].try_into().unwrap()) == f.c - 1
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(predecessor_locates.len(), 3);
    // The two parity reopening proofs precede admission's session proof. Each
    // LOCATE also performs its own POSITION before the READ/POSITION pair.
    let mut previous_end = None;
    for (&locate, &read) in predecessor_locates.iter().zip(&predecessor_reads) {
        assert!(previous_end.is_none_or(|end| end < locate));
        assert!(locate < read && read + 1 < at);
        assert!(before_read[locate].ok);
        assert!(before_read[read].sense.is_some_and(|s| s.filemark));
        assert_eq!(before_read[read + 1].cdb[0], 0x34);
        assert!(before_read[read + 1].ok);
        previous_end = Some(read + 1);
    }
    assert_eq!(
        output.trace.commands[..at]
            .iter()
            .filter(|v| v.cdb[0] == 0x08 && v.lba == Some(f.c))
            .count(),
        0,
        "named_file first READ C, no own-closeout debt"
    );
    let text = format!("{} {}", output.out, output.err);
    let first="tape identity invalidated: proved generation 0, current generation 1; re-establish media proof before writing";
    assert!(text.contains(first), "unchanged identity refusal: {text}");
    println!("refusal: C={} predecessor_reads={predecessor_reads:?} predecessor_locates={predecessor_locates:?} READ_at={at} returned=03/14/00; no writes; records/EOD unchanged; text={text}",f.c);
    assert!(
        text.contains("opcode=0x08"),
        "refusal names opcode=0x08: {text}"
    );
    let envelope: serde_json::Value = serde_json::from_str(
        output
            .out
            .lines()
            .rev()
            .find(|s| s.starts_with('{'))
            .unwrap(),
    )
    .unwrap();
    let error = envelope["error"].as_str().unwrap();
    let clause = error.split_once(first).unwrap().1;
    let ms = clause
        .split_once("proof_elapsed_ms=")
        .unwrap()
        .1
        .strip_suffix(" (probe start to returned error)")
        .unwrap();
    assert!(!ms.is_empty() && ms.bytes().all(|b| b.is_ascii_digit()));
    let elapsed = ms.parse::<u128>().unwrap();
    assert_eq!(clause,format!("; admission evidence: phase=probe; kind=io; step_target_lba={}; opcode=0x08; sense=03/14/00; sense_timing=current; host_status=unavailable; host_bytes_transferred=0; bytes_transferred=0; proof_elapsed_ms={elapsed} (probe start to returned error)",f.c));
    assert!(!clause.contains("media_readiness_state="));
}
