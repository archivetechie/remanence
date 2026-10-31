//! Reference per-tape logs for independent decoders. Each log is written by the
//! real authority writer from fixed inputs; damaged tails are cut or edited from
//! a healthy log afterwards. The checked-in `fixtures/tape-log/` bytes, README
//! and manifests are regenerated here and must match exactly. Set
//! `TAPE_LOG_FIXTURE_EXPORT_DIR` to an empty directory to export a fresh set.
use super::codec::{CodecError, Decoder, Tail, TornKind};
use super::store::{Header, Store};
use super::*;
use crate::authority::{AuthorityCommit, AuthorityFinalization, TapeAuthorityWriter};
use crate::checkpoint::tests::{partial_epoch_parity_record, record, second_record};
use crate::{TapeAuthority, TapeAuthorityLease, TerminalFinalizationProgress};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Cursor;
use std::path::{Path, PathBuf};

const EXPORT: &str = "TAPE_LOG_FIXTURE_EXPORT_DIR";
const PARITY_OFF: [u8; 16] = [0xa1; 16];
const PARITY_CLOSE: [u8; 16] = [0xa2; 16];
const SEALED: [u8; 16] = [0xa3; 16];

/// Expected tail class, asserted while generating so the README cannot drift
/// into describing a different case than the file name promises.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Complete,
    Torn,
    RecoveryRequired,
}

struct Case {
    name: &'static str,
    uuid: [u8; 16],
    class: Class,
    about: &'static str,
    bytes: Vec<u8>,
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/tape-log")
}

fn written(uuid: [u8; 16], build: impl FnOnce(&mut TapeAuthorityLease)) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let authority = TapeAuthority::open_fixture(dir.path(), uuid).unwrap();
    let mut writer = authority.acquire_exclusive().unwrap();
    build(&mut writer);
    drop(writer);
    std::fs::read(authority.path()).unwrap()
}

fn third_record(uuid: [u8; 16]) -> crate::CheckpointJournalRecord {
    let mut record = second_record(uuid);
    let object = uuid::Uuid::from_bytes([0x53; 16]).to_string();
    record.ordinal = 3;
    record.committed_object_count = 3;
    record.eod_lba = 14;
    record.batch_id = [0x44; 16];
    record.next_tape_file_number = 4;
    let projection = &mut record.objects[0];
    projection.object.object_id = object.clone();
    projection.object.caller_object_id = Some("checkpoint-test-3".into());
    projection.copy.object_id = object.clone();
    projection.copy.tape_file_number = 3;
    projection.total_committed_ordinals = 9;
    projection.object_recovery_row.tape_file_number = 3;
    projection.object_recovery_row.object_id = object.into_bytes();
    record
}

/// Offset of the last complete frame of a healthy log.
fn last_frame(bytes: &[u8], uuid: [u8; 16]) -> u64 {
    let mut reader = Cursor::new(bytes);
    Header::decode(&mut reader, uuid).unwrap();
    let start = reader.position();
    let mut decoder = Decoder::at(reader, 1, start);
    let mut last = None;
    while let Some(frame) = decoder.next_frame().unwrap() {
        last = Some(frame.offset);
    }
    last.unwrap()
}

fn cases() -> Vec<Case> {
    let two_objects = written(PARITY_OFF, |w| {
        w.publish_report(&(&record(PARITY_OFF)).into()).unwrap();
        w.publish_report(&(&second_record(PARITY_OFF)).into())
            .unwrap();
    });
    // An Object leaves its epoch open (W 0, T 3). The retained close is a
    // separate barrier with no Object: its sidecar protects the epoch (W = T).
    let retained_close = written(PARITY_CLOSE, |w| {
        let mut object = partial_epoch_parity_record(PARITY_CLOSE);
        object.object_tape_file_bundles[0].entries[0].physical_start_hint = Some(2);
        object.barrier_bundle = None;
        object.next_tape_file_number = 2;
        object.eod_lba = 6;
        // A fixed value stands in for the writer's check over the open [W,T).
        let mut report: AuthorityCommit = (&object).into();
        report.check = Some((
            CheckValue(0x0123_4567_89ab_cdef),
            CheckProvenance::OriginalWrite,
        ));
        w.publish_report(&report).unwrap();
        let mut close = partial_epoch_parity_record(PARITY_CLOSE);
        close.ordinal = 2;
        close.batch_id = [0x43; 16];
        close.objects.clear();
        close.object_tape_file_bundles.clear();
        close.barrier_bundle.as_mut().unwrap().entries[0].physical_start_hint = Some(6);
        close.eod_lba = 8;
        w.publish_report(&(&close).into()).unwrap();
    });
    let sealed = written(SEALED, |w| {
        let (record, mut intent, _) = crate::authority::contract_tests::structural_fixture(SEALED);
        intent.trigger = TerminalFinalizationTrigger::ReachedLowWatermark;
        intent.manual = None;
        w.publish_report(&(&record).into()).unwrap();
        let plan: AuthorityFinalization = (&intent).into();
        w.begin_finalization(&plan).unwrap();
        w.publish_terminal_prefix(&plan.terminal_prefix.as_ref().unwrap().publication)
            .unwrap();
        let mut progress = TerminalFinalizationProgress::BeforeReplicaA;
        while let Some(next) = progress.successor() {
            w.advance_finalization(progress, next).unwrap();
            progress = next;
        }
        w.seal(|| Ok(())).unwrap();
    });
    // A crash inside the third group's closing frame: its provisional frames
    // are complete, its closer stops a few bytes into the structural section.
    let three_objects = written(PARITY_OFF, |w| {
        for report in [
            record(PARITY_OFF),
            second_record(PARITY_OFF),
            third_record(PARITY_OFF),
        ] {
            w.publish_report(&(&report).into()).unwrap();
        }
    });
    let cut = last_frame(&three_objects, PARITY_OFF) as usize + PREFIX_LEN + 3;
    let torn = three_objects[..cut].to_vec();
    let mut zero_filled = two_objects.clone();
    zero_filled.extend_from_slice(&[0; 4096]);
    // One flipped bit in the final closing frame's structural section.
    let mut damaged = two_objects.clone();
    damaged[last_frame(&two_objects, PARITY_OFF) as usize + PREFIX_LEN] ^= 0x80;
    vec![
        Case {
            name: "parity-off-two-objects.remtape",
            uuid: PARITY_OFF,
            class: Class::Complete,
            about: "Parity-off tape: Initialized and two Object barriers.",
            bytes: two_objects,
        },
        Case {
            name: "parity-object-retained-close.remtape",
            uuid: PARITY_CLOSE,
            class: Class::Complete,
            about: "Parity tape: one Object barrier leaving its epoch open, then a retained close publishing the epoch's sidecar.",
            bytes: retained_close,
        },
        Case {
            name: "sealed-terminal.remtape",
            uuid: SEALED,
            class: Class::Complete,
            about: "Sealed parity tape: an Object and sidecar barrier, an automatic FinalizationDecided plan, TerminalPrefix, five ComponentWritten records and Sealed.",
            bytes: sealed,
        },
        Case {
            name: "torn-tail.remtape",
            uuid: PARITY_OFF,
            class: Class::Torn,
            about: "The parity-off tape with a third group whose closing frame is cut short by EOF. The first two groups remain authoritative.",
            bytes: torn,
        },
        Case {
            name: "zero-filled-tail.remtape",
            uuid: PARITY_OFF,
            class: Class::Torn,
            about: "The two-Object parity-off log followed by 4096 zero bytes, as a filesystem may leave an unsynced append after power loss.",
            bytes: zero_filled,
        },
        Case {
            name: "recovery-required-tail.remtape",
            uuid: PARITY_OFF,
            class: Class::RecoveryRequired,
            about: "The two-Object parity-off log with one bit flipped in the last closing frame's structural section. Only the first group is authoritative.",
            bytes: damaged,
        },
    ]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn torn_kind(kind: TornKind) -> String {
    match kind {
        TornKind::ShortPrefix => "short_prefix".into(),
        TornKind::ShortBody => "short_body".into(),
        TornKind::ZeroFilled => "zero_filled".into(),
        TornKind::UnclosedGroup { zero_filled_at } => match zero_filled_at {
            Some(offset) => format!("unclosed_group(zero_filled_at={offset})"),
            None => "unclosed_group".into(),
        },
    }
}

/// Decode one case with the codec and the store's own tail inspection, check
/// it against the expected class, and render its README section and TSV rows.
fn describe(case: &Case, frames_tsv: &mut String, summary_tsv: &mut String) -> String {
    let mut reader = Cursor::new(case.bytes.as_slice());
    let header = Header::decode(&mut reader, case.uuid).unwrap();
    let header_len = reader.position();
    let mut decoder = Decoder::at(reader, 1, header_len);
    let mut frames = Vec::new();
    let (class, tail) = loop {
        match decoder.next_frame() {
            Ok(Some(frame)) => {
                frames.push((frame.offset, frame.end, frame.sequence, frame.record.tag()))
            }
            Ok(None) => match decoder.tail() {
                Tail::Complete => break (Class::Complete, "complete".to_string()),
                Tail::Torn { offset, kind } => {
                    break (
                        Class::Torn,
                        format!("torn: {} at byte {offset}", torn_kind(kind)),
                    )
                }
            },
            Err(CodecError::Recovery(error)) => {
                break (
                    Class::RecoveryRequired,
                    format!(
                        "recovery required at byte {}, sequence {}: {}",
                        error.offset,
                        error.sequence.map_or("none".into(), |s| s.to_string()),
                        error.fault
                    ),
                )
            }
            Err(error) => panic!("{}: {error}", case.name),
        }
    };
    assert!(class == case.class, "{}: decoded {tail}", case.name);
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_fixture(dir.path(), case.uuid).unwrap();
    std::fs::write(store.path(), &case.bytes).unwrap();
    let inspection = store.inspect_tail().unwrap();
    assert_eq!(
        inspection.damage.is_some(),
        class != Class::Complete,
        "{}",
        case.name
    );
    let snapshot = if class == Class::Complete {
        store.snapshot().unwrap()
    } else {
        store.preview_repair(inspection.through_sequence).unwrap()
    };
    let s = snapshot.summary();
    assert_eq!(s.last_closed_sequence, inspection.through_sequence);
    assert_eq!(s.closed_end, inspection.valid_bytes);
    let scheme = header.scheme.as_ref().map_or("none".to_string(), |s| {
        format!(
            "{} k={} m={} S={}",
            s.id.as_str(),
            s.data_blocks_per_stripe,
            s.parity_blocks_per_stripe,
            s.stripes_per_neighborhood
        )
    });
    let digest = hex(&Sha256::digest(&case.bytes));
    let mut out = String::new();
    writeln!(out, "## {}\n\n{}\n", case.name, case.about).unwrap();
    writeln!(out, "- bytes: {}, SHA-256 {digest}", case.bytes.len()).unwrap();
    writeln!(
        out,
        "- header: tape UUID {}, block size {}, scheme {scheme}, {header_len} bytes",
        hex(&case.uuid),
        header.block_size
    )
    .unwrap();
    writeln!(out, "- tail: {tail}").unwrap();
    writeln!(
        out,
        "- authoritative prefix: last closed sequence {}, closed end {}",
        s.last_closed_sequence, s.closed_end
    )
    .unwrap();
    writeln!(
        out,
        "- counters: objects {}, next file {}, W {}, T {}, check {:016x} ({:?}), next epoch {}, cursor {}:{}, sealed {}\n",
        s.object_count,
        s.next_file,
        s.protected,
        s.total,
        s.check.0,
        s.provenance,
        s.next_epoch,
        s.cursor.partition,
        s.cursor.lba,
        s.sealed
    )
    .unwrap();
    writeln!(
        out,
        "| sequence | tag | offset | end |\n|---:|---|---:|---:|"
    )
    .unwrap();
    for (offset, end, sequence, tag) in &frames {
        writeln!(
            out,
            "| {sequence} | {tag:?} ({}) | {offset} | {end} |",
            *tag as u16
        )
        .unwrap();
        writeln!(
            frames_tsv,
            "{}\t{sequence}\t{tag:?}\t{}\t{offset}\t{end}",
            case.name, *tag as u16
        )
        .unwrap();
    }
    writeln!(
        summary_tsv,
        "{}\t{}\t{digest}\t{}\t{}\t{scheme}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        case.name,
        case.bytes.len(),
        hex(&case.uuid),
        header.block_size,
        frames.len(),
        s.last_closed_sequence,
        s.closed_end,
        s.object_count,
        s.next_file,
        s.protected,
        s.total,
        s.cursor.lba,
        s.sealed,
        tail
    )
    .unwrap();
    out
}

const README_HEAD: &str = "# Per-tape log reference fixtures

Host per-tape logs (`<tape-uuid>.remtape`, format version 1) for testing an
independent decoder. The Rust authority writer produced every healthy log from
fixed inputs; the damaged tails are cut or edited from a healthy log afterwards.
These are host authority files, not tape images, and carry no on-tape bytes.

Each file starts with its header: magic, version, tape UUID, incarnation,
geometry length, block size, geometry and a CRC-64/XZ. Frames follow. Each frame
has a 28-byte prefix with its own CRC, then structural and payload sections,
each with a CRC-64/XZ trailer. Sequences start at 1 and are contiguous. A group
is authoritative only once its closing record (any tag other than Initialized
or PayloadContinuation) has been decoded.

Tail classes:

- complete: every byte belongs to a complete frame.
- torn: an EOF-short or zero-filled suffix after the last closing record. It is
  not authority; the next exclusive lease removes it before appending.
- recovery required: bytes that are neither authority nor a permitted torn
  suffix. Only an explicit operator repair may cut them.

Each table lists every frame that decodes, in order. Frames past the closed
end belong to a group that never closed: they decode but are not authority.
W is the protected data ordinal, T the committed data ordinal. The counters
describe the authoritative prefix only. `FRAMES.tsv` lists every decoded frame;
`MANIFEST.tsv` lists each file's size, digest, header and counters. Both, and
this README, are generated: `cargo test -p remanence-state --lib
reference_tape_logs` fails if any checked-in byte differs. To regenerate, set
`TAPE_LOG_FIXTURE_EXPORT_DIR` to an empty directory, run that test, and copy
the exported files here.
";

fn generate() -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let mut readme = README_HEAD.to_string();
    let mut frames = "file\tsequence\ttag\ttag_value\toffset\tend\n".to_string();
    let mut summary = "file\tbytes\tsha256\ttape_uuid\tblock_size\tscheme\tframes\tlast_closed_sequence\tclosed_end\tobjects\tnext_file\tw\tt\tcursor_lba\tsealed\ttail\n".to_string();
    for case in cases() {
        readme.push('\n');
        readme.push_str(&describe(&case, &mut frames, &mut summary));
        files.insert(case.name.to_string(), case.bytes);
    }
    files.insert("README.md".into(), readme.into_bytes());
    files.insert("FRAMES.tsv".into(), frames.into_bytes());
    files.insert("MANIFEST.tsv".into(), summary.into_bytes());
    files
}

#[test]
fn reference_tape_logs_regenerate_byte_exactly() {
    let generated = generate();
    assert_eq!(generate(), generated, "generation is deterministic");
    for (name, bytes) in &generated {
        assert!(bytes.len() < 64 * 1024, "{name} stays small");
    }
    if let Some(directory) = std::env::var_os(EXPORT) {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            0,
            "export directory must be empty"
        );
        for (name, bytes) in &generated {
            std::fs::write(directory.join(name), bytes).unwrap();
        }
    }
    let mut checked_in = BTreeMap::new();
    for entry in std::fs::read_dir(root()).unwrap() {
        let path = entry.unwrap().path();
        checked_in.insert(
            path.file_name().unwrap().to_str().unwrap().to_string(),
            std::fs::read(&path).unwrap(),
        );
    }
    assert_eq!(
        checked_in.keys().collect::<Vec<_>>(),
        generated.keys().collect::<Vec<_>>()
    );
    for (name, bytes) in generated {
        assert!(
            bytes == checked_in[&name],
            "{name} drifted from fixtures/tape-log"
        );
    }
}
