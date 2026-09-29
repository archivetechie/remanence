//! Execute the terminal mutation matrix with the shared production role table.
//! The matrix generator emits recipes, not damaged bytes. Its existing Python
//! recipe executor is invoked solely to construct bytes, never to name errors.
//! Tape observations use the production terminal Scanner's inventory entry point.
use super::*;
use remanence_chaos::model::{DeviceRole, ModelTransport, Record, VirtualTape, VirtualWorld};
use remanence_library::DriveHandle;
use std::{
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
};

/// Reuse the existing matrix mutations without porting their byte edits to Rust.
fn build_mutations(root: &Path, output: &Path) {
    let script = r#"
import sys
from pathlib import Path
sys.path.insert(0, str(Path(sys.argv[1]) / 'tools'))
import verify_terminal_index_vectors as candidate
root, output = Path(sys.argv[2]), Path(sys.argv[3])
for row in candidate.read_tsv(root / 'MUTATIONS.tsv'):
    profile = root / row['base_profile']
    data = (profile / row['target']).read_bytes()
    other = (root / row['other_profile']).read_bytes() if row['other_profile'] else None
    if row['kind'] == 'replica':
        data = candidate.mutate_replica(data, row['mutation'], 262144, other)
    elif row['kind'] == 'gap':
        data = candidate.mutate_gap(data, row['mutation'], 262144, other, (profile / 'replica-a.bin').read_bytes())
    else:
        assert row['kind'] == 'event' and row['mutation'] == 'missing-filemark'
    (output / row['case_id']).write_bytes(data)
"#;
    let result = Command::new("python3")
        .arg("-B")
        .arg("-c")
        .arg(script)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .arg(root)
        .arg(output)
        .output()
        .expect("Python mutation executor must be installed");
    assert!(
        result.status.success(),
        "mutation construction failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// Preserve physical record boundaries, short records and the missing filemark.
/// The terminal-only profiles contain no prefix bytes: opaque prefix records
/// supply their recorded geometry; fast inventory never walks those records.
fn tape(
    files: &BTreeMap<usize, Vec<Vec<u8>>>,
    input: &Value,
    missing_filemark: bool,
) -> DriveHandle {
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK);
    for entry in input["structural_entries"]
        .as_array()
        .expect("prefix geometry")
    {
        for _ in 0..entry["block_count"].as_u64().unwrap() {
            tape.records.push(Record::Block(vec![0; B]));
        }
        tape.records.push(Record::Filemark);
    }
    for (&file, blocks) in files {
        tape.records
            .extend(blocks.iter().cloned().map(Record::Block));
        if !(missing_filemark && file == 6) {
            tape.records.push(Record::Filemark);
        }
    }
    let mut world = VirtualWorld::single_drive("MUTATIONS", 0x100, "MUTATIONS", 0x400, 1);
    world.put_tape_in_drive(0x100, "MUT001", None, tape);
    let transport = ModelTransport::new(
        Arc::new(Mutex::new(world)),
        DeviceRole::Drive { bay: 0x100 },
    );
    DriveHandle::open_standalone_with_transport(
        Path::new("/dev/sg-mutation-model"),
        Box::new(transport),
    )
    .unwrap()
}

#[test]
fn terminal_mutation_section15() {
    let root = fixture_root();
    let bytes = fs::read(root.join("mutation-section15.json")).expect("authored mutation names");
    assert_eq!(
        hex(&Sha256::digest(&bytes)),
        "cfee8f27baaec890162a12ab6df450c53c01764f1b492e8ee390bb9bd7875313"
    );
    let names: Value = serde_json::from_slice(&bytes).unwrap();
    let entries = names["entries"].as_object().unwrap();
    let table = fs::read_to_string(root.join("MUTATIONS.tsv")).unwrap();
    let mut lines = table.lines();
    assert_eq!(
        lines.next().unwrap(),
        "case_id\tkind\tbase_profile\ttarget\tmutation\tother_profile\texpected\tsection15"
    );
    let temp = tempfile::tempdir().unwrap();
    build_mutations(&root, temp.path());
    let mut seen = BTreeSet::new();
    let mut failures = Vec::new();
    for line in lines {
        let row: Vec<_> = line.split('\t').collect();
        assert_eq!(row.len(), 8);
        let id = row[0];
        assert!(seen.insert(id));
        let authored = &entries[id];
        let expected = &authored["component_error"];
        let column = if let Some(name) = expected.as_str() {
            name.to_owned()
        } else {
            expected["one_of"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>()
                .join("|")
        };
        assert_eq!(row[7], column, "{id}: section15 column");
        assert_eq!(row[2], "multi-256k", "unsupported physical profile");
        let input: Value =
            serde_json::from_slice(&fs::read(root.join(row[2]).join("inputs.json")).unwrap())
                .unwrap();
        let mut files = profiles::build(row[2]).expect("candidate writer reproduces base bytes");
        let target = match row[3] {
            "replica-a.bin" => 6,
            "gap-ab.bin" => 7,
            other => panic!("unknown target {other}"),
        };
        files.insert(
            target,
            fs::read(temp.path().join(id))
                .unwrap()
                .chunks(B)
                .map(<[u8]>::to_vec)
                .collect(),
        );
        let role = match row[1] {
            "replica" | "event" => Role::Replica,
            "gap" => Role::Separation,
            other => panic!("unknown role {other}"),
        };
        let resolved = Resolved {
            path: id.into(),
            expected: authored.clone(),
            descriptor: Value::Null,
            manifest: String::new(),
            files,
            uuid: [0x11; 16],
            roles: vec![role],
            targets: vec![target],
            block: 0,
            injected: true,
        };
        let tape_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut drive = tape(&resolved.files, &input, row[1] == "event");
            read_terminal_index_inventory_summary(
                &mut DriveHandleRawSource::new(&mut drive),
                &resolved.uuid,
                BLOCK,
            )
        }));
        // Inspect the complete production result before summarizing it: an
        // invariant wrapped in per-member evidence must not disappear in a
        // degraded or fallback outcome.
        if let Ok(result) = &tape_result {
            let detail = format!("{result:?}");
            if detail.to_ascii_lowercase().contains("invariant") {
                failures.push(format!("{id}: Scanner {detail}"));
            }
        }
        // Event rows have unchanged component bytes. Only Scanner evidence can
        // observe their physical defect; never substitute the byte parser here.
        let (name, detail) = if row[1] == "event" {
            assert_eq!(target, 6, "event evidence currently targets replica A");
            let replicas = match &tape_result {
                Ok(Ok(TerminalInventoryOutcome::Inventory(selection))) => Some(&selection.replicas),
                Ok(Ok(TerminalInventoryOutcome::BotStructuralRecoveryRequired(recovery))) => {
                    Some(&recovery.replicas)
                }
                _ => None,
            };
            match replicas.map(|replicas| &replicas[0]) {
                Some(TerminalReplicaEvidence::Invalid(reason)) => {
                    let error = ObservedError::ScannerReplica(reason.clone());
                    (section15(&error), error.to_string())
                }
                evidence => (
                    "UNMAPPED",
                    format!("Scanner has no replica A invalidity reason: {evidence:?}"),
                ),
            }
        } else {
            let mut observations = observe(&resolved);
            assert_eq!(observations.len(), 1);
            let observation = observations.remove(0);
            (observation.name, observation.detail)
        };
        let matches = column.split('|').any(|expected| expected == name);
        let pinned = authored["pinned"].as_bool().expect("pinned flag") && column != "undecided";
        let status = if !pinned || (row[1] == "event" && name == "UNMAPPED") {
            "INFORMATIVE"
        } else if matches {
            "PASS"
        } else {
            "DISAGREEMENT"
        };
        let tape_observed = match tape_result {
            Err(_) => {
                failures.push(format!("{id}: Scanner PANIC"));
                "PANIC".into()
            }
            Ok(Err(e)) => {
                // Source adapters can wrap ParityError in diagnostic strings.
                let detail = format!("{e:?}");
                if detail.contains("Invariant") || detail.contains("invariant") {
                    failures.push(format!("{id}: {detail}"));
                }
                detail
            }
            Ok(Ok(TerminalInventoryOutcome::Inventory(selection))) => format!(
                "Inventory(selected={}, degraded={}, replicas={:?})",
                selection.selected_replica_ordinal,
                selection.is_degraded(),
                selection
                    .replicas
                    .iter()
                    .map(|e| match e {
                        TerminalReplicaEvidence::Invalid(_) => "invalid",
                        TerminalReplicaEvidence::Valid { .. } => "valid",
                        TerminalReplicaEvidence::ConsistentEnvelope => "consistent-envelope",
                    })
                    .collect::<Vec<_>>()
            ),
            Ok(Ok(TerminalInventoryOutcome::BotStructuralRecoveryRequired(e))) => {
                format!("BotStructuralRecoveryRequired({:?})", e.reason)
            }
        };
        println!("{status} {id}: expected={column}; observed={}; detail={:?}; tape_authored={}; tape_observed={tape_observed}", name, detail, authored["tape_level"]["outcome"]);
        if matches!(name, "PANIC" | "Invariant") {
            failures.push(format!("{id}: {detail}"));
        }
        // Disagreements are evidence for the supervisor, never repaired here.
    }
    assert_eq!(seen.len(), entries.len(), "every authored case must run");
    assert!(
        failures.is_empty(),
        "unsafe production outcomes: {failures:?}"
    );
}
