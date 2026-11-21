//! PLR-T3 admission-owned evidence: real proof failures and the daemon renderer.
//! Scripted completions stay in this concrete model forwarder; production policy is unchanged.
use super::*;
use remanence_chaos::model::{DeviceRole, ModelTransport, Record, VirtualTape, VirtualWorld};
use remanence_library::{
    transport::{SenseInfo, TransferOutcome},
    ScsiError, SgTransport, StaticAllowlist,
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
const UUID: [u8; 16] = [0x42; 16];
const SIZE: u32 = 262144;
#[derive(Clone)]
enum Reply {
    Sense {
        deferred: bool,
        key: u8,
        asc: u8,
        ascq: u8,
        bytes: u32,
    },
    Host,
    Io,
    Lonu,
    Reset(Option<u64>),
    Pass,
}
struct Hook {
    opcode: u8,
    nth: usize,
    reply: Reply,
    delay: Duration,
}
#[derive(Clone, Debug)]
struct Event {
    opcode: u8,
    nth: usize,
    start: Instant,
    end: Instant,
}
#[derive(Default)]
struct Script {
    hooks: Vec<Hook>,
    counts: BTreeMap<u8, usize>,
    events: Vec<Event>,
}
struct EvidenceTransport {
    inner: ModelTransport,
    script: Arc<Mutex<Script>>,
}
impl EvidenceTransport {
    fn begin(&self, opcode: u8) -> (usize, Instant, Reply) {
        let mut script = self.script.lock().unwrap();
        let n = script.counts.entry(opcode).or_default();
        *n += 1;
        let nth = *n;
        let hook = script
            .hooks
            .iter()
            .position(|h| h.opcode == opcode && h.nth == nth)
            .map(|at| script.hooks.remove(at));
        drop(script);
        let start = Instant::now();
        let reply = hook.map_or(Reply::Pass, |h| {
            std::thread::sleep(h.delay);
            h.reply
        });
        (nth, start, reply)
    }
    fn end(&self, opcode: u8, nth: usize, start: Instant) {
        self.script.lock().unwrap().events.push(Event {
            opcode,
            nth,
            start,
            end: Instant::now(),
        });
    }
}
fn error(reply: &Reply) -> Option<ScsiError> {
    match reply {
        Reply::Sense {
            deferred,
            key,
            asc,
            ascq,
            bytes,
        } => Some(ScsiError::CheckCondition {
            sense: vec![
                if *deferred { 0x71 } else { 0x70 },
                0,
                *key,
                0,
                0,
                0,
                0,
                10,
                0,
                0,
                0,
                0,
                *asc,
                *ascq,
                0,
                0,
                0,
                0,
            ],
            bytes_transferred: *bytes,
        }),
        Reply::Host => Some(ScsiError::TransportError {
            status: 0,
            host_status: 0x000b,
            driver_status: 0,
            info: 1,
            sense: vec![],
        }),
        Reply::Io => Some(ScsiError::Io(std::io::Error::other(
            "media_readiness_state=private transport detail",
        ))),
        _ => None,
    }
}
impl SgTransport for EvidenceTransport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        let (nth, start, reply) = self.begin(cdb[0]);
        let result = if let Some(e) = error(&reply) {
            Err(e)
        } else {
            let mut result = self.inner.execute_in(cdb, buf);
            if let Ok(outcome) = &mut result {
                match reply {
                    Reply::Lonu => buf[0] |= 0x04,
                    Reply::Reset(lba) => {
                        if let Some(lba) = lba {
                            buf[8..16].copy_from_slice(&lba.to_be_bytes());
                        }
                        outcome.sense = Some(SenseInfo {
                            key: 6,
                            asc: 0x29,
                            ascq: 0,
                            information: None,
                            ili: false,
                            eom: false,
                            filemark: false,
                        });
                    }
                    _ => {}
                }
            }
            result
        };
        self.end(cdb[0], nth, start);
        result
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let (nth, start, reply) = self.begin(cdb[0]);
        let result = if let Some(e) = error(&reply) {
            Err(e)
        } else {
            self.inner.execute_none(cdb)
        };
        self.end(cdb[0], nth, start);
        result
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        let (nth, start, reply) = self.begin(cdb[0]);
        let result = if let Some(e) = error(&reply) {
            Err(e)
        } else {
            self.inner.execute_out(cdb, buf)
        };
        self.end(cdb[0], nth, start);
        result
    }
}
struct Fixture {
    drive: DriveHandle,
    world: Arc<Mutex<VirtualWorld>>,
    script: Arc<Mutex<Script>>,
}
fn fixture() -> Fixture {
    let bootstrap = crate::build_tape_bootstrap(
        UUID,
        SIZE,
        remanence_parity::ParityConfig::None,
        "2026-11-21T00:00:00Z",
        "PLR-T3",
    );
    let mut block = vec![0; SIZE as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, &mut block).unwrap();
    let mut tape = VirtualTape::empty(16 * 1024 * 1024, SIZE);
    tape.records = vec![Record::Block(block), Record::Filemark];
    let mut world = VirtualWorld::single_drive("EVIDENCE", 0x100, "EVIDENCE-DRIVE", 0x400, 0);
    world.put_tape_in_drive(0x100, "TAIL", None, tape);
    let world = Arc::new(Mutex::new(world));
    let script = Arc::new(Mutex::new(Script::default()));
    let drive = DriveHandle::open_standalone_with_transport(
        Path::new("/virtual/plr-t3-api"),
        Box::new(EvidenceTransport {
            inner: ModelTransport::new(world.clone(), DeviceRole::Drive { bay: 0x100 }),
            script: script.clone(),
        }),
        &StaticAllowlist::new(["EVIDENCE-DRIVE"]),
    )
    .unwrap();
    *script.lock().unwrap() = Script::default();
    Fixture {
        drive,
        world,
        script,
    }
}
fn hook(f: &Fixture, opcode: u8, nth: usize, reply: Reply, delay_ms: u64) {
    f.script.lock().unwrap().hooks.push(Hook {
        opcode,
        nth,
        reply,
        delay: Duration::from_millis(delay_ms),
    });
}
fn missing() -> Reply {
    Reply::Sense {
        deferred: false,
        key: 3,
        asc: 0x14,
        ascq: 0,
        bytes: 0,
    }
}
fn fresh(f: &mut Fixture) -> Result<AppendAdmission, AdmissionError> {
    admit_append_boundary(
        &mut f.drive,
        AppendAuthority::Fresh,
        context(UUID, SIZE, 0, 0),
    )
}
fn checkpoint(f: &mut Fixture) -> Result<AppendAdmission, AdmissionError> {
    let ctx = context(UUID, SIZE, 0, 2);
    admit_append_boundary(
        &mut f.drive,
        AppendAuthority::Checkpoint { eod: ctx.boundary },
        ctx,
    )
}
fn no_writes(f: &Fixture) {
    assert!(!f
        .script
        .lock()
        .unwrap()
        .events
        .iter()
        .any(|e| matches!(e.opcode, 0x0a | 0x8a | 0x10 | 0x80)));
}
fn reference(error: &AdmissionError) -> AdmissionError {
    let AdmissionErrorKind::Io(io) = &error.kind else {
        panic!("outer Io: {error:?}")
    };
    let remanence_library::TapeIoErrorKind::IdentityInvalidated { proved, current } = io.kind
    else {
        panic!("outer identity: {error:?}")
    };
    AdmissionError::new(AdmissionErrorKind::Io(TapeIoError::new(
        remanence_library::TapeIoErrorKind::IdentityInvalidated { proved, current },
    )))
}
fn common(error: &AdmissionError, f: &Fixture) {
    let reference = reference(error);
    assert_eq!(
        admission_error_outcome(error),
        admission_error_outcome(&reference)
    );
    assert_eq!(format!("{error:?}"), format!("{reference:?}"));
    assert_eq!(
        std::error::Error::source(error).map(ToString::to_string),
        std::error::Error::source(&reference).map(ToString::to_string)
    );
    assert!(error.to_string().starts_with(&reference.to_string()));
    no_writes(f);
}
fn clause(error: &AdmissionError) -> String {
    error
        .to_string()
        .split_once("; admission evidence: ")
        .expect("admission evidence clause")
        .1
        .into()
}
fn fields(error: &AdmissionError) -> BTreeMap<String, String> {
    clause(error)
        .split("; ")
        .map(|s| {
            let (k, v) = s.split_once('=').unwrap();
            (k.into(), v.into())
        })
        .collect()
}
fn elapsed(error: &AdmissionError, phase: &str) -> u128 {
    let data = fields(error);
    let (key, label) = if phase == "probe" {
        ("proof_elapsed_ms", " (probe start to returned error)")
    } else {
        (
            "restoration_elapsed_ms",
            " (restoration start to returned error)",
        )
    };
    let value = data[key]
        .strip_suffix(label)
        .expect("exact elapsed phase label");
    assert!(!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()));
    value.parse().unwrap()
}
fn event(f: &Fixture, opcode: u8, nth: usize) -> Event {
    f.script
        .lock()
        .unwrap()
        .events
        .iter()
        .find(|e| e.opcode == opcode && e.nth == nth)
        .unwrap()
        .clone()
}
fn ended(f: &mut Fixture) -> (AdmissionError, crate::tests::TypedRoot) {
    use remanence_state::tape_log::{
        SessionDrive, SessionEndReason, TailFile, TailFilemark, TailIdentity,
    };
    let typed = crate::tests::TypedRoot::open();
    let index = typed.catalog().write_handle();
    let runtime = index.library_runtime().unwrap();
    let authority =
        remanence_state::TapeAuthority::open(runtime.manifest().tape_root(), UUID, index.path())
            .unwrap();
    let mut lease = authority.acquire_exclusive().unwrap();
    lease
        .publish_initialized(
            SIZE,
            None,
            remanence_parity::CommitCursor {
                partition: 0,
                lba: 2,
            },
        )
        .unwrap();
    lease
        .open_session(
            [0x61; 16],
            "2026-11-21T00:00:00Z".into(),
            SessionDrive {
                serial: "EVIDENCE-DRIVE".into(),
                vendor: None,
                product: None,
                endpoint: remanence_state::library_log::DriveKey::Standalone(
                    remanence_state::library_log::Standalone {
                        drive_serial: "EVIDENCE-DRIVE".into(),
                    },
                ),
                drive_uuid: None,
            },
        )
        .unwrap();
    lease
        .end_session(
            [0x61; 16],
            "2026-11-21T00:00:01Z".into(),
            SessionEndReason::DriveError,
            None,
            std::iter::once(Ok(TailFile {
                tape_file_number: 1,
                identity: TailIdentity::Object("00000000-0000-4000-8000-000000000009".into()),
                known_blocks: 0,
                unknown_blocks: 2,
                filemark: TailFilemark::Absent,
                synchronized_blocks: 0,
                synchronized_filemark: false,
            })),
        )
        .unwrap();
    let snapshot = lease.freeze().unwrap();
    assert!(!snapshot.tape_summary().closeout_due());
    let err = admit_append_boundary(
        &mut f.drive,
        AppendAuthority::Session {
            snapshot: &snapshot,
            closeout: None,
        },
        context(UUID, SIZE, 0, 2),
    )
    .unwrap_err();
    (err, typed)
}
fn assert_fields(error: &AdmissionError, expected: &[(&str, &str)]) {
    let got = fields(error);
    for (k, v) in expected {
        assert_eq!(got[*k], *v, "admission evidence {k}: {error}");
    }
    assert!(!clause(error).contains("media_readiness_state="));
}
fn unavailable(error: &AdmissionError) {
    assert_fields(
        error,
        &[
            ("opcode", "unavailable"),
            ("sense", "unavailable"),
            ("sense_timing", "unavailable"),
            ("host_status", "unavailable"),
            ("host_bytes_transferred", "unavailable"),
            ("bytes_transferred", "unavailable"),
        ],
    );
}
#[test]
fn probe_error_precedes_restoration() {
    let mut f = fixture();
    hook(&f, 0x08, 2, missing(), 10);
    hook(
        &f,
        0x34,
        2,
        Reply::Sense {
            deferred: false,
            key: 4,
            asc: 0x44,
            ascq: 0,
            bytes: 0,
        },
        25,
    );
    let start = Instant::now();
    let err = fresh(&mut f).unwrap_err();
    let end = Instant::now();
    common(&err, &f);
    assert!(
        err.to_string().contains("admission evidence: phase=probe"),
        "probe error clause: {err}"
    );
    assert_fields(
        &err,
        &[
            ("kind", "io"),
            ("step_target_lba", "1"),
            ("opcode", "0x08"),
            ("sense", "03/14/00"),
            ("sense_timing", "current"),
            ("host_status", "unavailable"),
            ("host_bytes_transferred", "0"),
            ("bytes_transferred", "0"),
        ],
    );
    let restore = event(&f, 0x34, 2);
    let reported = elapsed(&err, "probe");
    let probe = event(&f, 0x08, 2);
    let measured = probe.end.duration_since(probe.start).as_millis();
    assert!(measured >= 10, "probe hook measured delay");
    assert!(
        reported >= measured,
        "probe elapsed includes measured delay"
    );
    assert!(
        reported <= restore.start.duration_since(start).as_millis(),
        "probe elapsed excludes measured restoration delay: {reported} {restore:?}"
    );
    assert!(
        end.duration_since(start).as_millis()
            >= reported + restore.end.duration_since(restore.start).as_millis(),
        "probe elapsed boundary"
    );
    let mut marker_assertions_ran = false;
    for (reply, expected) in [
        (
            Reply::Host,
            vec![
                ("host_status", "0x000b"),
                ("sense", "unavailable"),
                ("sense_timing", "unavailable"),
                ("host_bytes_transferred", "unavailable"),
                ("bytes_transferred", "unavailable"),
            ],
        ),
        (
            Reply::Sense {
                deferred: true,
                key: 3,
                asc: 0x14,
                ascq: 0,
                bytes: 17,
            },
            vec![
                ("host_status", "unavailable"),
                ("sense", "03/14/00"),
                ("sense_timing", "deferred"),
                ("host_bytes_transferred", "17"),
                ("bytes_transferred", "17"),
            ],
        ),
        (
            Reply::Io,
            vec![
                ("host_status", "unavailable"),
                ("sense", "unavailable"),
                ("sense_timing", "unavailable"),
                ("host_bytes_transferred", "unavailable"),
                ("bytes_transferred", "unavailable"),
            ],
        ),
    ] {
        let mut f = fixture();
        hook(&f, 0x08, 2, reply.clone(), 0);
        let err = fresh(&mut f).unwrap_err();
        common(&err, &f);
        assert_fields(&err, &expected);
        assert_fields(
            &err,
            &[
                ("opcode", "0x08"),
                ("step_target_lba", "1"),
                ("phase", "probe"),
                ("kind", "io"),
            ],
        );
        if matches!(reply, Reply::Io) {
            // Admission consumed READs #1 (identity) and #2 (bootstrap mark).
            // Read #3 on this same fixture exposes the source's arbitrary text.
            assert_eq!(f.script.lock().unwrap().counts[&0x08], 2);
            hook(&f, 0x08, 3, Reply::Io, 0);
            let source = f.drive.read_block(&mut vec![0; SIZE as usize]).unwrap_err();
            assert!(source.to_string().contains("media_readiness_state="));
            assert_eq!(f.script.lock().unwrap().counts[&0x08], 3);
            marker_assertions_ran = true;
        }
    }
    assert!(marker_assertions_ran, "Reply::Io marker assertions ran");
    let mut f = fixture();
    hook(&f, 0x08, 2, missing(), 0);
    let (err, _typed) = ended(&mut f);
    common(&err, &f);
    assert_fields(
        &err,
        &[
            ("phase", "probe"),
            ("step_target_lba", "2"),
            ("opcode", "0x08"),
            ("sense", "03/14/00"),
        ],
    );
    // Missing bootstrap and structural data refusals retain only their fixed kind labels.
    for (record, kind) in [
        (None, "missing-bootstrap-filemark"),
        (Some(Record::ZeroBlock(SIZE)), "unproved"),
    ] {
        let mut f = fixture();
        {
            let mut w = f.world.lock().unwrap();
            let tape = w.tapes.get_mut("TAIL").unwrap();
            tape.records.truncate(1);
            if let Some(record) = record {
                tape.records.push(record);
            }
        }
        hook(&f, 0x08, 1, Reply::Reset(None), 0);
        let err = fresh(&mut f).unwrap_err();
        common(&err, &f);
        assert_fields(
            &err,
            &[("kind", kind), ("step_target_lba", "1"), ("phase", "probe")],
        );
        unavailable(&err);
    }
    let mut f = fixture();
    f.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("TAIL")
        .unwrap()
        .records
        .extend([
            Record::ZeroBlock(SIZE),
            Record::ZeroBlock(SIZE),
            Record::ZeroBlock(SIZE),
        ]);
    hook(&f, 0x08, 2, Reply::Reset(None), 0);
    let (err, _typed) = ended(&mut f);
    common(&err, &f);
    assert_fields(
        &err,
        &[
            ("kind", "session-suffix-contradiction"),
            ("step_target_lba", "2"),
        ],
    );
    unavailable(&err);
    // A complete foreign object is still a structural refusal after a successful
    // scanner READ invalidates generation; its target remains the scan start.
    let mut f = fixture();
    let mut options = remanence_format::RemTarObjectOptions::new(
        "00000000-0000-4000-8000-000000000001",
        "PLR-T3",
        "2026-11-21T00:00:00Z",
        "00000000-0000-4000-8000-000000000002",
    );
    options.chunk_size = SIZE as usize;
    let payload = vec![0x35; SIZE as usize * 2];
    let file = remanence_format::RemTarFile {
        path: "payload",
        file_id: "00000000-0000-4000-8000-000000000003",
        data: &payload,
        mtime: None,
        executable: Some(false),
    };
    let mut sink = remanence_library::VecBlockSink::new();
    remanence_format::write_rem_tar_object(&mut sink, &options, &[file]).unwrap();
    f.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("TAIL")
        .unwrap()
        .records
        .extend(
            sink.blocks
                .into_iter()
                .map(Record::Block)
                .chain([Record::Filemark]),
        );
    hook(&f, 0x08, 4, Reply::Reset(None), 0);
    let err = checkpoint(&mut f).unwrap_err();
    common(&err, &f);
    assert_fields(
        &err,
        &[
            ("kind", "tail-beyond-known-state"),
            ("step_target_lba", "2"),
        ],
    );
    unavailable(&err);
    // SuffixIo always wins, including Fresh, even when restoration also fails.
    let mut f = fixture();
    hook(&f, 0x08, 3, missing(), 0);
    hook(&f, 0x34, 3, Reply::Io, 0);
    let err = fresh(&mut f).unwrap_err();
    let AdmissionErrorKind::SuffixIo { start_lba, source } = &err.kind else {
        panic!("universal SuffixIo precedence: {err:?}")
    };
    assert_eq!(*start_lba, 2);
    assert_eq!(admission_error_outcome(&err), *source.outcome());
    assert_eq!(err.to_string(), format!("append suffix at LBA 2: {source}"));
    assert!(!err.to_string().contains("admission evidence:"));
    no_writes(&f);
    // Stable generation still returns restoration before an independent structural probe error.
    let mut f = fixture();
    f.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("TAIL")
        .unwrap()
        .records[1] = Record::ZeroBlock(SIZE);
    hook(
        &f,
        0x92,
        2,
        Reply::Sense {
            deferred: false,
            key: 5,
            asc: 0x24,
            ascq: 0,
            bytes: 0,
        },
        0,
    );
    let err = fresh(&mut f).unwrap_err();
    assert_eq!(f.drive.identity_generation(), 0);
    assert!(matches!(err.kind, AdmissionErrorKind::Io(_)));
    assert!(!err.to_string().contains("admission evidence:"));
    no_writes(&f);
}
#[test]
fn restoration_error_after_successful_probe() {
    let mut f = fixture();
    hook(&f, 0x34, 4, missing(), 0);
    let err = checkpoint(&mut f).unwrap_err();
    common(&err, &f);
    assert!(
        err.to_string()
            .contains("admission evidence: phase=restoration"),
        "restoration error clause: {err}"
    );
    assert_fields(
        &err,
        &[
            ("kind", "io"),
            ("step_target_lba", "2"),
            ("opcode", "0x34"),
            ("sense", "03/14/00"),
            ("sense_timing", "current"),
            ("host_status", "unavailable"),
            ("host_bytes_transferred", "0"),
            ("bytes_transferred", "0"),
        ],
    );
    let events = f.script.lock().unwrap().events.clone();
    let position = events
        .iter()
        .position(|e| e.opcode == 0x34 && e.nth == 4)
        .unwrap();
    assert_eq!(
        events[position - 1].opcode,
        0x08,
        "final restoration POSITION follows predecessor READ"
    );
    for (reply, bytes) in [
        (Reply::Io, "unavailable"),
        (Reply::Lonu, "32"),
        (Reply::Reset(Some(123)), "unavailable"),
    ] {
        let mut f = fixture();
        hook(&f, 0x34, 4, reply.clone(), 0);
        let err = checkpoint(&mut f).unwrap_err();
        common(&err, &f);
        assert_fields(
            &err,
            &[
                ("phase", "restoration"),
                ("step_target_lba", "2"),
                ("sense", "unavailable"),
                ("sense_timing", "unavailable"),
                ("host_status", "unavailable"),
                ("host_bytes_transferred", bytes),
                ("bytes_transferred", bytes),
            ],
        );
        assert_fields(
            &err,
            &[(
                "opcode",
                if matches!(reply, Reply::Reset(_)) {
                    "unavailable"
                } else {
                    "0x34"
                },
            )],
        );
    }
    // A successful probe's delay is excluded from restoration's own time origin.
    let mut f = fixture();
    hook(&f, 0x34, 1, Reply::Pass, 25);
    hook(&f, 0x34, 4, missing(), 10);
    let start = Instant::now();
    let err = checkpoint(&mut f).unwrap_err();
    let end = Instant::now();
    let probe_delay = event(&f, 0x34, 1);
    let probe_last = event(&f, 0x08, 2);
    let reported = elapsed(&err, "restoration");
    let restore = event(&f, 0x34, 4);
    let measured = restore.end.duration_since(restore.start).as_millis();
    assert!(measured >= 10, "restoration hook measured delay");
    assert!(
        reported >= measured,
        "restoration elapsed includes measured delay"
    );
    assert!(
        reported <= end.duration_since(probe_last.start).as_millis(),
        "restoration uses its own origin"
    );
    assert!(
        end.duration_since(start).as_millis()
            >= reported
                + probe_delay
                    .end
                    .duration_since(probe_delay.start)
                    .as_millis()
    );
    // LOCATE owns its opcode even when its nested POSITION returns the error.
    let mut f = fixture();
    hook(&f, 0x34, 3, missing(), 0);
    let err = fresh(&mut f).unwrap_err();
    common(&err, &f);
    assert_fields(
        &err,
        &[
            ("phase", "restoration"),
            ("step_target_lba", "0"),
            ("opcode", "0x92"),
            ("sense", "unavailable"),
            ("sense_timing", "unavailable"),
            ("host_status", "unavailable"),
            ("host_bytes_transferred", "unavailable"),
            ("bytes_transferred", "unavailable"),
            ("post_transfer.opcode", "0x34"),
            ("post_transfer.sense", "03/14/00"),
            ("post_transfer.sense_timing", "current"),
            ("post_transfer.host_status", "unavailable"),
            ("post_transfer.host_bytes_transferred", "0"),
            ("post_transfer.bytes_transferred", "0"),
        ],
    );
    elapsed(&err, "restoration");
    let clause = clause(&err);
    let keys: Vec<_> = clause
        .split("; ")
        .map(|s| s.split_once('=').unwrap().0)
        .collect();
    assert_eq!(
        keys,
        [
            "phase",
            "kind",
            "step_target_lba",
            "opcode",
            "sense",
            "sense_timing",
            "host_status",
            "host_bytes_transferred",
            "bytes_transferred",
            "post_transfer.opcode",
            "post_transfer.sense",
            "post_transfer.sense_timing",
            "post_transfer.host_status",
            "post_transfer.host_bytes_transferred",
            "post_transfer.bytes_transferred",
            "restoration_elapsed_ms"
        ]
    );
}
#[test]
fn generation_moved_without_returned_error() {
    let mut f = fixture();
    hook(&f, 0x34, 3, Reply::Reset(Some(0)), 0);
    let err = fresh(&mut f).unwrap_err();
    common(&err, &f);
    assert_eq!(f.drive.identity_generation(), 1);
    assert!(
        err.to_string()
            .contains("; admission evidence: no probe or restoration error was returned"),
        "no probe or restoration error was returned: {err}"
    );
    assert_eq!(
        err.to_string(),
        format!(
            "{}; admission evidence: no probe or restoration error was returned",
            reference(&err)
        )
    );
    // Neither a later successful proof nor a constructor carries stale evidence.
    *f.script.lock().unwrap() = Script::default();
    let admission = fresh(&mut f).unwrap();
    drop(admission);
    assert!(!AdmissionError::from("ordinary structural error")
        .to_string()
        .contains("admission evidence:"));
    assert!(!reference(&err).to_string().contains("admission evidence:"));
}
#[test]
fn daemon_identity_refusal_preserves_evidence() {
    let mut f = fixture();
    hook(&f, 0x08, 2, missing(), 0);
    let (err, typed) = ended(&mut f);
    common(&err, &f);
    let AdmissionErrorKind::Io(io) = &err.kind else {
        panic!("outer Io: {err:?}")
    };
    let remanence_library::TapeIoErrorKind::IdentityInvalidated { proved, current } = io.kind
    else {
        panic!("outer identity: {err:?}")
    };
    assert_eq!(proved, 0, "daemon proved generation");
    assert_eq!(current, 1, "daemon current generation");
    let expected = reference(&err);
    let outcome = admission_error_outcome(&err);
    assert_eq!(outcome, admission_error_outcome(&expected));
    let display = err.to_string();
    let mut index = typed.catalog().write_handle();
    let runtime = index.library_runtime().unwrap();
    let cfg = crate::write_owner::TerminalWriteConfig {
        checkpoint_journal_dir: runtime.manifest().tape_root().to_path_buf(),
        authority: crate::write_owner::TerminalAuthority::Typed(runtime),
        io_memory: crate::io_memory::IoMemoryReservation::new(1024).unwrap(),
        mounted_endpoint: None,
    };
    let status = daemon_admission_error(
        remanence_state::AuditActor::System,
        &mut index,
        &cfg,
        UUID,
        err,
        &outcome,
    );
    assert_eq!(status.code(), tonic::Code::Unavailable);
    assert_eq!(
        status.metadata().get("remanence-retry").unwrap(),
        "transient"
    );
    assert!(index.list_active_tape_io_fences().unwrap().is_empty());
    no_writes(&f);
    assert_eq!(status.message(), display);
    assert!(
        status.message().contains("opcode=0x08"),
        "status names opcode=0x08: {status}"
    );
    assert!(status
        .message()
        .contains("phase=probe; kind=io; step_target_lba=2; opcode=0x08; sense=03/14/00"));
    assert!(!status.message().contains("media_readiness_state="));
    let mut f = fixture();
    hook(&f, 0x08, 2, Reply::Io, 0);
    let (err, typed) = ended(&mut f);
    let text = err.to_string();
    let outcome = admission_error_outcome(&err);
    let mut index = typed.catalog().write_handle();
    let status = daemon_admission_error(
        remanence_state::AuditActor::System,
        &mut index,
        &cfg,
        UUID,
        err,
        &outcome,
    );
    assert_eq!(status.message(), text);
    assert!(!status.message().contains("media_readiness_state="));
}
