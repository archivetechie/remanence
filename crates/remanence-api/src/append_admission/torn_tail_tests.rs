//! PLR-2a-1: frozen base observations and evidence proofs on measured model media.
use super::*;
use remanence_chaos::model::{
    DeviceRole, DriveProfile, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::{SgTransport, StaticAllowlist};
use remanence_state::tape_log::{
    SessionDrive, SessionEndReason, TailFile, TailFilemark, TailIdentity,
};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

const UUID: [u8; 16] = [0x42; 16];
const SIZE: u32 = 262144;
const ID: &str = "00000000-0000-4000-8000-000000000001";
const OTHER: &str = "00000000-0000-4000-8000-000000000002";
const BAY: u16 = 0x100;
const CASES: &[&str] = &[
    "first",
    "remo",
    "plain",
    "bootstrap",
    "after_file",
    "named",
    "named_second",
    "different_remo",
    "different_plain",
    "named_empty",
    "repaired",
    "expired",
    "deferred",
    "moved",
    "unscoped",
    "identity",
    "ended_empty",
    "beyond",
];

fn plain(caller_len: usize) -> Vec<Vec<u8>> {
    let mut options = remanence_format::RemTarObjectOptions::new(
        ID,
        "x".repeat(caller_len),
        "2026-11-23T00:00:00Z",
        OTHER,
    );
    options.chunk_size = SIZE as usize;
    let payload = vec![0x35; SIZE as usize * 3];
    let file = remanence_format::RemTarFile {
        path: "payload",
        file_id: "00000000-0000-4000-8000-000000000003",
        data: &payload,
        mtime: None,
        executable: Some(false),
    };
    let mut sink = remanence_library::VecBlockSink::new();
    remanence_format::write_rem_tar_object(&mut sink, &options, &[file]).unwrap();
    sink.blocks
}
fn remo() -> Vec<Vec<u8>> {
    let h = remanence_aead::RemObjectHeader::new_envelope(SIZE, [1; 16], 32, ID, 2048)
        .unwrap()
        .serialize()
        .unwrap();
    let mut first = vec![0; SIZE as usize];
    first[..128].copy_from_slice(&h);
    vec![first, vec![0x42; SIZE as usize]]
}
fn named_file(index: u64, blocks: usize, complete: bool, expected: &str) -> TailFile {
    TailFile {
        tape_file_number: index + 1,
        identity: TailIdentity::Object(expected.into()),
        known_blocks: blocks as u64,
        unknown_blocks: u64::from(!complete),
        filemark: if complete {
            TailFilemark::Present
        } else {
            TailFilemark::Absent
        },
        synchronized_blocks: 0,
        synchronized_filemark: false,
    }
}
/// MED-01 goes through the production chaos transport, with its durable event log.
fn injected_transport(inner: ModelTransport, root: &Path, case: &str) -> Box<dyn SgTransport> {
    let path = root.join("fault.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE scenarios (id TEXT PRIMARY KEY, seed TEXT NOT NULL, source_json TEXT NOT NULL, armed_at TEXT NOT NULL); CREATE TABLE faults (id INTEGER PRIMARY KEY, scenario_id TEXT NOT NULL, ordinal INTEGER NOT NULL, catalogue_id TEXT NOT NULL, target_json TEXT NOT NULL, trigger_json TEXT NOT NULL, action_json TEXT NOT NULL, scope TEXT NOT NULL); CREATE TABLE events (id INTEGER PRIMARY KEY, ts TEXT NOT NULL, scenario_id TEXT, fault_id INTEGER, catalogue_id TEXT, event_json TEXT NOT NULL); INSERT INTO scenarios VALUES ('plr2a1', 'plr2a1', '{}', '2026-11-23T00:00:00Z');").unwrap();
    let action = match case {
        "deferred" => serde_json::json!({"check_condition":{"rc":"71"}}),
        "moved" => serde_json::json!({"check_condition":{"bytes_transferred":1}}),
        _ => serde_json::json!({}),
    };
    conn.execute(
        "INSERT INTO faults VALUES (1,'plr2a1',0,'MED-01','{}',?1,?2,'transient')",
        rusqlite::params![
            serde_json::json!({"op":"read", "lba": if case == "identity" {0} else {2}}).to_string(),
            action.to_string()
        ],
    )
    .unwrap();
    drop(conn);
    Box::new(
        remanence_chaos::ChaosTransport::from_state_path(
            inner,
            path,
            remanence_chaos::DeviceCtx::new()
                .with_drive_id("PLR-DRIVE")
                .with_tape_id("TAIL"),
        )
        .unwrap(),
    )
}
struct Observation {
    error: AdmissionError,
    world: SharedVirtualWorld,
    before: u64,
    after: u64,
    events: Vec<serde_json::Value>,
}
/// Every tape has an initialized native cursor and no pending parity closeout.
fn observe(case: &str) -> Observation {
    let bootstrap = crate::build_tape_bootstrap(
        UUID,
        SIZE,
        remanence_parity::ParityConfig::None,
        "2026-11-23T00:00:00Z",
        "PLR-2a-1",
    );
    let mut boot = vec![0; SIZE as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, &mut boot).unwrap();
    let mut records = vec![Record::Block(boot.clone()), Record::Filemark];
    let mut files = Vec::new();
    if matches!(case, "after_file" | "named_second") {
        let full = plain(1);
        files.push(named_file(0, full.len(), true, ID));
        records.extend(full.into_iter().map(Record::Block));
        records.push(Record::Filemark);
    }
    let blocks = match case {
        "first" | "named_empty" | "repaired" | "expired" | "ended_empty" => vec![],
        "plain" | "different_plain" => plain(SIZE as usize + 100)[..2].to_vec(),
        "cap" => plain(4 * 1024 * 1024 + SIZE as usize)[..19].to_vec(),
        "bootstrap" => vec![boot, vec![0x33; SIZE as usize]],
        _ => remo(),
    };
    let expected = if matches!(case, "different_remo" | "different_plain") {
        OTHER
    } else {
        ID
    };
    let ended = matches!(
        case,
        "named"
            | "named_second"
            | "different_remo"
            | "different_plain"
            | "named_empty"
            | "repaired"
            | "expired"
            | "ended_empty"
            | "beyond"
    );
    if ended && !matches!(case, "ended_empty" | "beyond") {
        files.push(named_file(
            files.len() as u64,
            blocks.len(),
            false,
            expected,
        ));
    }
    records.extend(blocks.into_iter().map(Record::Block));
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, SIZE);
    tape.records = records;
    let mut world = VirtualWorld::single_drive("PLR", BAY, "PLR-DRIVE", 0x400, 0);
    world.put_tape_in_drive(BAY, "TAIL", None, tape);
    world.drive_states.get_mut(&BAY).unwrap().profile = DriveProfile::MeasuredHpLto6;
    let keep = world.tapes["TAIL"].records.len();
    world.power_cut_during_write(BAY, keep).unwrap();
    if matches!(case, "repaired" | "expired") {
        let tape = world.tapes.get_mut("TAIL").unwrap();
        tape.missing_eod_sense = None;
        tape.short_erased_end = Some(2);
        if case == "repaired" {
            world
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .measured_timings
                .read_at_short_erased_end_ms = 890_000;
        }
    }
    let world = Arc::new(Mutex::new(world));
    let temp = tempfile::tempdir().unwrap();
    let inner = ModelTransport::new(world.clone(), DeviceRole::Drive { bay: BAY });
    let injected = matches!(case, "deferred" | "moved" | "identity");
    let transport: Box<dyn SgTransport> = if injected {
        injected_transport(inner, temp.path(), case)
    } else {
        Box::new(inner)
    };
    let mut drive = DriveHandle::open_standalone_with_transport(
        Path::new("/virtual/plr2a1"),
        transport,
        &StaticAllowlist::new(["PLR-DRIVE"]),
    )
    .unwrap();
    drive.locate(0).unwrap();
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
            "2026-11-23T00:00:00Z".into(),
            SessionDrive {
                serial: "PLR-DRIVE".into(),
                vendor: None,
                product: None,
                endpoint: remanence_state::library_log::DriveKey::Standalone(
                    remanence_state::library_log::Standalone {
                        drive_serial: "PLR-DRIVE".into(),
                    },
                ),
                drive_uuid: None,
            },
        )
        .unwrap();
    if ended {
        lease
            .end_session(
                [0x61; 16],
                "2026-11-23T00:00:01Z".into(),
                SessionEndReason::DriveError,
                None,
                files.into_iter().map(Ok),
            )
            .unwrap();
    }
    let snapshot = lease.freeze().unwrap();
    assert!(!snapshot.tape_summary().closeout_due());
    if case == "unscoped" {
        drive.locate(2).unwrap();
    }
    world.lock().unwrap().command_log.clear();
    let before = drive.identity_generation();
    let error = if case == "unscoped" {
        crate::catalogless_restore::scan_append_suffix_observed(
            &mut drive, &UUID, SIZE, 2, None, None, true, None,
        )
        .unwrap_err()
    } else if case == "identity" {
        admit_append_boundary(
            &mut drive,
            AppendAuthority::Fresh,
            context(UUID, SIZE, 0, 0),
        )
        .unwrap_err()
    } else {
        admit_append_boundary(
            &mut drive,
            AppendAuthority::Session {
                snapshot: &snapshot,
                closeout: None,
            },
            context(UUID, SIZE, 0, 2),
        )
        .unwrap_err()
    };
    let after = drive.identity_generation();
    drop(drive);
    let events = if injected {
        std::fs::read_to_string(temp.path().join("fault.sqlite.events.jsonl"))
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .filter(|e: &serde_json::Value| e["catalogue_id"] == "MED-01")
            .collect()
    } else {
        vec![]
    };
    Observation {
        error,
        world,
        before,
        after,
        events,
    }
}
fn normalized(text: &str) -> String {
    let Some((left, right)) = text.split_once("proof_elapsed_ms=") else {
        return text.into();
    };
    let (digits, rest) = right.split_once(' ').unwrap();
    assert!(!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()));
    format!("{left}proof_elapsed_ms=<ms> {rest}")
}
/// G0b literals are frozen from the unchanged 9d1e1147 production funnel.
#[test]
fn baseline_funnel_observations() {
    for &case in CASES {
        let o = observe(case);
        let (message, opcodes, generation): (&str, &[u8], u64) = match case {
            "first" => ("append suffix at LBA 2: drive rejected the command: SCSI check condition (bytes_transferred=0): sense=[f0, 00, 03, 00, 04, 00, 00, 18, 00, 00, 00, 00, 11, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00]", &[146, 52, 8, 52, 8], 1),
            "remo" => ("append suffix at LBA 2: drive rejected the command: SCSI check condition (bytes_transferred=0): sense=[f0, 00, 03, 00, 04, 00, 00, 18, 00, 00, 00, 00, 11, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00]", &[146, 52, 8, 52, 8, 146, 52, 8, 52, 8, 8, 8], 1),
            "plain" => ("append suffix at LBA 2: drive rejected the command: SCSI check condition (bytes_transferred=0): sense=[f0, 00, 03, 00, 04, 00, 00, 18, 00, 00, 00, 00, 11, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00]", &[146, 52, 8, 52, 8, 146, 52, 8, 52, 8, 8, 8], 1),
            "bootstrap" => ("append suffix at LBA 2: drive rejected the command: SCSI check condition (bytes_transferred=0): sense=[f0, 00, 03, 00, 04, 00, 00, 18, 00, 00, 00, 00, 11, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00]", &[146, 52, 8, 52, 8, 146, 52, 8, 52, 8, 8, 8], 1),
            "after_file" => ("append suffix at LBA 2: drive rejected the command: SCSI check condition (bytes_transferred=0): sense=[f0, 00, 03, 00, 04, 00, 00, 18, 00, 00, 00, 00, 11, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00]", &[146, 52, 8, 52, 8, 146, 52, 8, 52, 8, 8, 8, 8, 8, 8, 8, 146, 52, 8, 8, 8, 8, 8, 8, 146, 52, 8, 8, 8], 1),
            "named" => ("tape identity invalidated: proved generation 0, current generation 1; re-establish media proof before writing; admission evidence: phase=probe; kind=io; step_target_lba=2; opcode=0x08; sense=03/11/00; sense_timing=current; host_status=unavailable; host_bytes_transferred=0; bytes_transferred=0; proof_elapsed_ms=<ms> (probe start to returned error)", &[146, 52, 8, 52, 52, 8, 8, 8], 1),
            "named_second" => ("tape identity invalidated: proved generation 0, current generation 1; re-establish media proof before writing; admission evidence: phase=probe; kind=io; step_target_lba=9; opcode=0x08; sense=03/11/00; sense_timing=current; host_status=unavailable; host_bytes_transferred=0; bytes_transferred=0; proof_elapsed_ms=<ms> (probe start to returned error)", &[146, 52, 8, 52, 52, 8, 8, 8, 8, 8, 8, 8, 52, 146, 52, 8, 146, 52, 52, 8, 8, 8], 1),
            "different_remo" => ("tape identity invalidated: proved generation 0, current generation 1; re-establish media proof before writing; admission evidence: phase=probe; kind=io; step_target_lba=2; opcode=0x08; sense=03/11/00; sense_timing=current; host_status=unavailable; host_bytes_transferred=0; bytes_transferred=0; proof_elapsed_ms=<ms> (probe start to returned error)", &[146, 52, 8, 52, 52, 8, 8, 8], 1),
            "different_plain" => ("tape identity invalidated: proved generation 0, current generation 1; re-establish media proof before writing; admission evidence: phase=probe; kind=io; step_target_lba=2; opcode=0x08; sense=03/11/00; sense_timing=current; host_status=unavailable; host_bytes_transferred=0; bytes_transferred=0; proof_elapsed_ms=<ms> (probe start to returned error)", &[146, 52, 8, 52, 52, 8, 8, 8], 1),
            "named_empty" => ("tape identity invalidated: proved generation 0, current generation 1; re-establish media proof before writing; admission evidence: phase=probe; kind=io; step_target_lba=2; opcode=0x08; sense=03/11/00; sense_timing=current; host_status=unavailable; host_bytes_transferred=0; bytes_transferred=0; proof_elapsed_ms=<ms> (probe start to returned error)", &[146, 52, 8, 52, 52, 8], 1),
            "repaired" => ("tape identity invalidated: proved generation 0, current generation 1; re-establish media proof before writing; admission evidence: phase=probe; kind=io; step_target_lba=2; opcode=0x08; sense=03/14/00; sense_timing=current; host_status=unavailable; host_bytes_transferred=0; bytes_transferred=0; proof_elapsed_ms=<ms> (probe start to returned error)", &[146, 52, 8, 52, 52, 8], 1),
            "expired" => ("tape identity invalidated: proved generation 0, current generation 3; re-establish media proof before writing; admission evidence: phase=probe; kind=io; step_target_lba=2; opcode=0x08; sense=unavailable; sense_timing=unavailable; host_status=0x0003; host_bytes_transferred=unavailable; bytes_transferred=unavailable; proof_elapsed_ms=<ms> (probe start to returned error)", &[146, 52, 8, 52, 52, 8], 3),
            "deferred" => ("append suffix at LBA 2: transport error (completion unknown): SCSI check condition (bytes_transferred=0): sense=[71, 00, 03, 00, 00, 00, 00, 18, 00, 00, 00, 00, 11, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00]", &[146, 52, 8, 52], 3),
            "moved" => ("append suffix at LBA 2: drive rejected the command: SCSI check condition (bytes_transferred=1): sense=[70, 00, 03, 00, 00, 00, 00, 18, 00, 00, 00, 00, 11, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00]", &[146, 52, 8, 52], 1),
            "unscoped" => ("drive rejected the command: SCSI check condition (bytes_transferred=0): sense=[f0, 00, 03, 00, 04, 00, 00, 18, 00, 00, 00, 00, 11, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00]", &[8, 8, 8], 1),
            "identity" => ("tape identity invalidated: proved generation 0, current generation 1; re-establish media proof before writing; admission evidence: phase=probe; kind=io; step_target_lba=0; opcode=0x08; sense=03/11/00; sense_timing=current; host_status=unavailable; host_bytes_transferred=0; bytes_transferred=0; proof_elapsed_ms=<ms> (probe start to returned error)", &[1, 146, 52, 146, 52], 1),
            "ended_empty" => ("tape identity invalidated: proved generation 0, current generation 1; re-establish media proof before writing; admission evidence: phase=probe; kind=io; step_target_lba=2; opcode=0x08; sense=03/11/00; sense_timing=current; host_status=unavailable; host_bytes_transferred=0; bytes_transferred=0; proof_elapsed_ms=<ms> (probe start to returned error)", &[146, 52, 8, 52, 8], 1),
            "beyond" => ("append suffix at LBA 2: drive rejected the command: SCSI check condition (bytes_transferred=0): sense=[f0, 00, 03, 00, 04, 00, 00, 18, 00, 00, 00, 00, 11, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00]", &[146, 52, 8, 52, 8, 146, 52, 8, 52, 8, 8, 8], 1),
            _ => unreachable!(),
        };
        match case {
            "first" | "remo" | "plain" | "bootstrap" | "after_file" | "deferred" | "moved"
            | "beyond" => assert!(
                matches!(
                    o.error.kind,
                    AdmissionErrorKind::SuffixIo { start_lba: 2, .. }
                ),
                "{case}: {:?}",
                o.error.kind
            ),
            "unscoped" => assert!(
                matches!(&o.error.kind, AdmissionErrorKind::Io(io) if matches!(io.kind, remanence_library::TapeIoErrorKind::CheckCondition(_)))
            ),
            _ => assert!(
                matches!(&o.error.kind, AdmissionErrorKind::Io(io) if matches!(io.kind, remanence_library::TapeIoErrorKind::IdentityInvalidated { proved: 0, current } if current == generation)),
                "{case}: {:?}",
                o.error.kind
            ),
        }
        assert_eq!(normalized(&o.error.to_string()), message, "{case}");
        assert_eq!(
            o.world
                .lock()
                .unwrap()
                .command_log
                .iter()
                .map(|c| c.opcode)
                .collect::<Vec<_>>(),
            opcodes,
            "{case}"
        );
        assert_eq!((o.before, o.after), (0, generation), "{case}");
        if matches!(case, "deferred" | "moved" | "identity") {
            assert_eq!(o.events.len(), 1);
            let mut event = o.events[0].clone();
            assert!(event
                .as_object_mut()
                .unwrap()
                .remove("ts")
                .unwrap()
                .as_str()
                .is_some_and(|s| !s.is_empty()));
            let (lba, requested, returned, rc) = match case {
                "deferred" => (2, 262144, 0, "0x71"),
                "moved" => (2, 262144, 1, "0x70"),
                "identity" => (0, 1048576, 0, "0x70"),
                _ => unreachable!(),
            };
            assert_eq!(
                event,
                serde_json::json!({
                    "backend":"fixture", "barcode":null, "catalogue_id":"MED-01", "cdb_opcode":"0x08", "data_direction":"in", "drive_id":"PLR-DRIVE", "element_status":null, "fault_id":1, "inner_called":false, "lba_after":lba, "lba_before":lba, "library_id":null, "mutation_summary":null, "operation":"read", "requested_bytes":requested, "returned_bytes":returned, "scenario_id":"plr2a1", "seed":"plr2a1", "sense":{"asc":"0x11", "ascq":"0x00", "eom":false, "fm":false, "ili":false, "information":null, "response_code":rc, "sk":"0x03"}, "state_delta":null, "status":"check_condition", "tape_alert":null, "tape_id":"TAIL"
                })
            );
        } else {
            assert!(o.events.is_empty());
        }
    }
}

/// Evidence is checked before any assertion about a captured record.
fn captured(o: &Observation) -> &TornTailEvidence {
    assert!(
        o.error.torn_tail().is_some(),
        "missing torn-tail evidence: {}",
        o.error
    );
    o.error.torn_tail().unwrap()
}
#[test]
fn t1_open_first_read() {
    let o = observe("first");
    let e = captured(&o);
    assert_eq!(e.site, Some(TornTailSite::OpenFirstRead));
    assert_eq!(e.cursor, Some(context(UUID, SIZE, 0, 2).boundary));
    assert_eq!((e.file_start_lba, e.blocks_read, e.failing_lba), (2, 0, 2));
    assert!(e.aligned);
    assert!(e.delimiters.is_empty());
    assert!(e.head.blocks.is_empty());
    assert_eq!(e.head.identity, TornTailIdentity::Undecided);
    assert_eq!(e.read.opcode, Some(0x08));
    assert_eq!(e.read.transferred_bytes, Some(0));
    let sense = e.read.decoded.unwrap();
    assert_eq!((sense.key, sense.asc, sense.ascq), (3, 0x11, 0));
}
#[test]
fn t3_scan_delimiters() {
    let o = observe("after_file");
    let e = captured(&o);
    assert_eq!(e.delimiters, [9]);
    assert_eq!(e.site, Some(TornTailSite::OpenScan));
    assert_eq!((e.file_start_lba, e.blocks_read, e.failing_lba), (9, 2, 11));
    assert_eq!(e.cursor.unwrap().lba, 2);
    assert!(e.aligned);
    assert_eq!(e.head.blocks, remo());
}
#[test]
fn t4b_named_delimiters() {
    let o = observe("named_second");
    let e = captured(&o);
    assert_eq!(e.delimiters, [9]);
    assert_eq!(e.site, Some(TornTailSite::NamedFile { index: 1 }));
    assert_eq!((e.file_start_lba, e.blocks_read, e.failing_lba), (9, 2, 11));
    assert_eq!(e.cursor.unwrap().lba, 2);
    assert!(e.aligned);
    assert_eq!(e.head.blocks, remo());
    let Some(AdmissionEvidence::Returned { error, .. }) = o.error.evidence.as_deref() else {
        panic!("probe evidence")
    };
    assert!(
        error.any_torn_tail().is_none(),
        "record must move to the refusal"
    );
}
#[test]
fn t7_repaired_end() {
    let o = observe("repaired");
    let e = captured(&o);
    assert_eq!(e.site, Some(TornTailSite::NamedFile { index: 0 }));
    assert_eq!((e.blocks_read, e.failing_lba), (0, 2));
    assert_eq!(e.read.transferred_bytes, Some(0));
    assert_eq!(e.read.host_transferred_bytes, Some(0));
    assert!(e.read.residual_valid);
    assert_eq!(e.read.residual, Some(u64::from(SIZE)));
    let sense = e.read.decoded.unwrap();
    assert_eq!((sense.key, sense.asc, sense.ascq), (3, 0x14, 0));
    let expired = observe("expired");
    assert!(expired.error.any_torn_tail().is_none());
}
#[test]
fn t8_deferred_and_transferred_controls() {
    use remanence_library::outcome::Completion;
    for case in ["deferred", "moved"] {
        let o = observe(case);
        let AdmissionErrorKind::SuffixIo { source, .. } = &o.error.kind else {
            panic!("suffix IO")
        };
        let read = source.completion_evidence();
        assert_eq!(read.opcode, Some(0x08));
        assert_eq!(read.decoded.unwrap().key, 3);
        if case == "deferred" {
            assert!(read.decoded.unwrap().is_deferred());
            assert_eq!(read.completion, Completion::Unknown);
        } else {
            assert!(!read.decoded.unwrap().is_deferred());
            assert_eq!(read.completion, Completion::Known);
            assert_eq!(read.transferred_bytes, Some(1));
        }
        assert!(!torn_tail_read(&read));
        assert!(o.error.any_torn_tail().is_none());
    }
}
#[test]
fn t8_unscoped_scan() {
    let o = observe("unscoped");
    assert!(
        o.error.any_torn_tail().is_none(),
        "tail None must never capture, even partially"
    );
}
#[test]
fn t8_other_exclusions() {
    for case in ["identity", "ended_empty", "beyond"] {
        let o = observe(case);
        assert!(o.error.any_torn_tail().is_none(), "{case}");
    }
    let o = observe("first");
    let e = captured(&o);
    let mut read = e.read.clone();
    read.transferred_bytes = None;
    assert!(!torn_tail_read(&read));
    read.transferred_bytes = Some(0);
    read.opcode = Some(0x11);
    assert!(!torn_tail_read(&read));
    read.opcode = Some(0x08);
    read.host_transferred_bytes = Some(1234);
    read.residual_valid = false;
    read.residual = None;
    assert!(torn_tail_read(&read));
    let mut primary = remanence_library::outcome::CompletionEvidence::unknown();
    primary.post_transfer = Some(Box::new(read));
    assert!(!torn_tail_read(&primary));
    assert!(AdmissionError::from("ordinary").any_torn_tail().is_none());
}

#[test]
fn t2_open_prefix_identities() {
    for (case, chunk_size) in [("remo", Some(SIZE)), ("plain", None)] {
        let o = observe(case);
        let e = captured(&o);
        assert_eq!(e.site, Some(TornTailSite::OpenScan));
        assert_eq!(e.cursor.unwrap().lba, 2);
        assert_eq!((e.file_start_lba, e.blocks_read, e.failing_lba), (2, 2, 4));
        assert!(e.aligned);
        assert_eq!(e.head.kind, Some(remanence_parity::TapeFileKind::Object));
        assert_eq!(e.head.bootstrap_magic, Some(false));
        assert_eq!(
            e.head.identity,
            TornTailIdentity::Object {
                object_id: ID.into(),
                chunk_size
            }
        );
    }
    let o = observe("bootstrap");
    let e = captured(&o);
    assert_eq!(e.head.bootstrap_magic, Some(true));
    assert_eq!(e.head.kind, Some(remanence_parity::TapeFileKind::Object));
    assert_eq!(e.head.identity, TornTailIdentity::Unrecognised);
}
#[test]
fn t2c_production_head_cap() {
    let o = observe("cap");
    let e = captured(&o);
    assert_eq!(
        e.head.blocks.iter().map(Vec::len).sum::<usize>(),
        4 * 1024 * 1024
    );
    assert_eq!(e.head.blocks, plain(4 * 1024 * 1024 + SIZE as usize)[..16]);
    assert_eq!((e.blocks_read, e.failing_lba), (19, 21));
    assert_eq!(e.head.identity, TornTailIdentity::Undecided);
}
#[test]
fn t4_named_identity_and_carriage() {
    let o = observe("named");
    let e = captured(&o);
    assert_eq!(e.site, Some(TornTailSite::NamedFile { index: 0 }));
    assert_eq!(e.head.identity, TornTailIdentity::Matches);
    assert_eq!(e.head.blocks, remo());
    let Some(AdmissionEvidence::Returned { error, .. }) = o.error.evidence.as_deref() else {
        panic!("probe evidence")
    };
    assert!(error.any_torn_tail().is_none());
}
#[test]
fn t5_named_mismatch_and_empty() {
    for (case, reason) in [
        ("different_remo", "named object identity differs"),
        ("different_plain", "named object header differs"),
    ] {
        let o = observe(case);
        let e = captured(&o);
        assert_eq!(e.head.identity, TornTailIdentity::Differs { reason });
    }
    let o = observe("named_empty");
    let e = captured(&o);
    assert_eq!(e.head.identity, TornTailIdentity::Undecided);
    assert!(e.head.blocks.is_empty());
    assert_eq!(e.head.kind, None);
    assert_eq!(e.head.bootstrap_magic, None);
}
/// Build only the global PAX header for precise reader-exhaustion boundaries.
fn pax_header(size: usize) -> Vec<u8> {
    let mut header = vec![0; 512];
    header[124..136].copy_from_slice(format!("{size:011o}\0").as_bytes());
    header[156] = b'g';
    header[257..263].copy_from_slice(b"ustar\0");
    header[148..156].fill(b' ');
    let sum: u64 = header.iter().map(|b| u64::from(*b)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    header
}
fn pax_id(value: &[u8]) -> Vec<u8> {
    let mut len = 1 + 1 + b"REMANENCE.object_id=".len() + value.len();
    loop {
        let next = len.to_string().len() + 1 + b"REMANENCE.object_id=".len() + value.len();
        if next == len {
            break;
        }
        len = next;
    }
    let mut bytes = pax_header(len);
    bytes.extend(format!("{len} REMANENCE.object_id=").as_bytes());
    bytes.extend(value);
    bytes
}
fn memory_head(blocks: Vec<Vec<u8>>) -> TornTailHead {
    let mut head = TornTailEvidence::partial(
        0,
        blocks.len() as u64,
        false,
        vec![],
        blocks,
        remanence_library::outcome::CompletionEvidence::success(),
    )
    .head;
    head.observe_open(&UUID, SIZE);
    head
}
#[test]
fn t6_parser_exhaustion_and_identity() {
    use crate::catalogless_restore::{parse_plain_identity, HeadReader, PlainIdentityPrefix};
    for suffix in [
        b"".as_slice(),
        b"100 REM",
        b"100 REMANENCE.object_id=x",
        b"100 XXXXXXXXXXXXXXXXXXXX",
    ] {
        let mut bytes = pax_header(100);
        bytes.extend(suffix);
        assert_eq!(
            parse_plain_identity(bytes.as_slice()),
            PlainIdentityPrefix::Exhausted,
            "{suffix:?}"
        );
    }
    assert_eq!(
        parse_plain_identity(&[0; 511][..]),
        PlainIdentityPrefix::ShortHeader
    );
    assert_eq!(
        parse_plain_identity(&[0; 512][..]),
        PlainIdentityPrefix::NotHeader
    );
    let value = format!("{ID}\n").into_bytes();
    let bytes = pax_id(&value);
    assert_eq!(
        parse_plain_identity(bytes.as_slice()),
        PlainIdentityPrefix::Id(value)
    );
    let mut refused = pax_header(100);
    refused.push(b'x');
    assert_eq!(
        parse_plain_identity(refused.as_slice()),
        PlainIdentityPrefix::Invalid
    );
    assert_eq!(
        parse_plain_identity(pax_header(0).as_slice()),
        PlainIdentityPrefix::Invalid
    );
    let partial = plain(SIZE as usize + 100)[..1].to_vec();
    assert_eq!(
        parse_plain_identity(HeadReader::new(&partial)),
        PlainIdentityPrefix::Exhausted
    );
    assert_eq!(memory_head(partial).identity, TornTailIdentity::Undecided);
    // The memory reader concatenates exactly the bytes returned, with no geometry padding.
    let split = vec![bytes[..17].to_vec(), vec![], bytes[17..].to_vec()];
    assert_eq!(
        memory_head(split).identity,
        TornTailIdentity::Object {
            object_id: ID.into(),
            chunk_size: None
        }
    );
}
#[test]
fn t6_open_invalid_and_non_object_heads() {
    for value in [b"\n".as_slice(), b"x\n\n", &[0xff, b'\n'], b"x"] {
        let head = memory_head(vec![pax_id(value)]);
        let TornTailIdentity::Invalid { reason } = head.identity else {
            panic!("invalid id: {value:?}")
        };
        assert!(!reason.is_empty());
    }
    let head = memory_head(vec![b"REMO".to_vec()]);
    assert_eq!(
        head.identity,
        TornTailIdentity::Invalid {
            reason: "short envelope header".into()
        }
    );
    let mut bad = vec![0; 128];
    bad[..4].copy_from_slice(b"REMO");
    let expected = remanence_aead::RemObjectHeader::parse(bad.as_slice().try_into().unwrap())
        .unwrap_err()
        .to_string();
    assert_eq!(
        memory_head(vec![bad]).identity,
        TornTailIdentity::Invalid { reason: expected }
    );
    let mut replica = vec![0; SIZE as usize];
    replica[..8].copy_from_slice(
        &remanence_parity::tape_index_replica::derive_tape_index_replica_header_magic(&UUID),
    );
    let head = memory_head(vec![replica]);
    assert_eq!(
        head.kind,
        Some(remanence_parity::TapeFileKind::TapeIndexReplica)
    );
    assert_eq!(head.bootstrap_magic, Some(false));
    assert_eq!(head.identity, TornTailIdentity::Undecided);
    assert_eq!(
        memory_head(vec![vec![0; 512]]).identity,
        TornTailIdentity::Unrecognised
    );
    let head = memory_head(vec![]);
    assert_eq!(head.kind, None);
    assert_eq!(head.bootstrap_magic, None);
    assert_eq!(head.identity, TornTailIdentity::Undecided);
}

#[test]
fn t4_daemon_named_refusal_label() {
    let o = observe("named");
    let e = captured(&o);
    assert_eq!(e.head.identity, TornTailIdentity::Matches);
    let outcome = admission_error_outcome(&o.error);
    let expected = o.error.to_string();
    let typed = crate::tests::TypedRoot::open();
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
        o.error,
        &outcome,
    );
    assert_eq!(status.code(), tonic::Code::Unavailable);
    assert_eq!(status.message(), expected);
    assert_eq!(
        status.metadata().get("remanence-retry").unwrap(),
        "transient"
    );
    assert_eq!(
        status.metadata().get("remanence-torn-tail").unwrap(),
        "inspect"
    );
}
