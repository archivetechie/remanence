//! Review-only, deterministic full-tape fixtures. Recipes record all writer
//! inputs; the manifest pins expanded bytes without storing image streams.

use std::path::Path;
use std::sync::{Arc, Mutex};

use remanence_chaos::model::{
    DeviceRole, ExportedTapeImage, ModelTransport, VirtualTape, VirtualWorld,
};
use remanence_format::{RemTarFileSpec, RemTarObjectOptions};
use remanence_library::DriveHandle;
use remanence_parity::{default_scheme_for_block_size, WriterIdentity};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::tape_image::{
    write_tape_image, TapeImageFile, TapeImageInputs, TapeImageObject, TapeImageStop,
    WrittenTapeImage,
};

pub const BLOCK: u32 = 262144;
pub const EXPECTATIONS: &str = include_str!(
    "../../../fixtures/rem-parity-terminal-index-draft/tape-images/expected-cases.json"
);
const TIMESTAMP: &str = "2026-08-09T00:00:00Z";
const VERSION: &str = "remanence-terminal-vector-generator/1";
pub const IMAGE_NAMES: [&str; 6] = [
    "a4-minimal",
    "short-epoch",
    "two-epoch",
    "two-edition",
    "unfinalized-closed",
    "unfinalized-open",
];

/// Full model output and its independently replayable input description.
pub struct VectorImage {
    pub written: WrittenTapeImage,
    pub image: ExportedTapeImage,
    pub recipe: Value,
}

pub(crate) fn object(blocks: usize, index: usize) -> TapeImageObject {
    let mut options = RemTarObjectOptions::new(
        format!("00000000-0000-4000-8000-{:012}", 1 + index * 3),
        format!("parity-image-object-{index}"),
        TIMESTAMP,
        format!("00000000-0000-4000-8000-{:012}", 2 + index * 3),
    );
    options.chunk_size = BLOCK as usize;
    let files = if blocks == 2 {
        vec![]
    } else {
        // Global pax, payload header and manifest consume three chunks.
        let bytes = vec![0x35; (blocks - 3) * BLOCK as usize];
        vec![TapeImageFile::from_bytes(
            RemTarFileSpec::new(
                "payload.bin",
                format!("00000000-0000-4000-8000-{:012}", 3 + index * 3),
                bytes.len() as u64,
                Sha256::digest(&bytes).into(),
            ),
            bytes,
        )]
    };
    TapeImageObject { options, files }
}

pub(crate) fn inputs(name: &str) -> TapeImageInputs {
    assert!(IMAGE_NAMES.contains(&name), "unknown image {name}");
    let mut scheme = default_scheme_for_block_size(BLOCK);
    scheme.data_blocks_per_stripe = 2;
    scheme.parity_blocks_per_stripe = 2;
    scheme.stripes_per_neighborhood = if name == "short-epoch" { 4 } else { 2 };
    let objects = match name {
        "short-epoch" => vec![object(2, 0)],
        "two-epoch" => vec![object(8, 0)],
        "unfinalized-open" => vec![object(4, 0), object(2, 1)],
        _ => vec![object(4, 0)],
    };
    TapeImageInputs {
        tape_uuid: *uuid::Uuid::parse_str("12345678-1234-4234-8234-123456789abc")
            .unwrap()
            .as_bytes(),
        scheme,
        block_size: BLOCK,
        objects,
        checkpoint_after_objects: vec![0],
        nominal_extent_bytes: 3 * u64::from(BLOCK),
        writer_identity: WriterIdentity::fixed(
            VERSION.to_string(),
            time::OffsetDateTime::parse(TIMESTAMP, &time::format_description::well_known::Rfc3339)
                .unwrap(),
        ),
        edition_id: [0x54; 16],
        edition_sequence: 7,
        parity_map_sequence_start: 0,
        directory_flags: remanence_parity::SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD
            | remanence_parity::SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
        diagnostic_keys_present: true,
        capacity_bytes: 6_000_000_000_000,
        stop: if name.starts_with("unfinalized-") {
            TapeImageStop::CommittedPrefix {
                torn_records: 3,
                torn_record_fill: 0xa5,
            }
        } else {
            TapeImageStop::Finalized
        },
    }
}

pub(crate) fn recipe(inputs: &TapeImageInputs) -> Value {
    let identity = inputs
        .writer_identity
        .capture()
        .expect("fixed writer identity");
    let objects: Vec<_> = inputs.objects.iter().map(|o| {
        let options = &o.options;
        assert!(options.extensions.is_empty());
        assert_eq!(options.metadata_preservation, remanence_format::MetadataPreservation::Archival);
        let files: Vec<_> = o.files.iter().map(|f| {
            let spec = &f.spec;
            assert!(spec.extensions.is_empty());
            assert_eq!(spec.entry_type, remanence_format::RemTarEntryType::Regular);
            let mut payload = Vec::new();
            (f.open)().read_to_end(&mut payload).expect("replay recorded input");
            assert_eq!(payload.len() as u64, spec.size_bytes);
            assert!(payload.iter().all(|byte| *byte == 0x35), "repeat-byte recipe must describe actual input");
            assert_eq!(spec.file_sha256, Some(Sha256::digest(&payload).into()));
            json!({"entry_type": "regular", "path": spec.path, "file_id": spec.file_id,
                "size_bytes": spec.size_bytes, "file_sha256": spec.file_sha256.map(|h| hex(&h)),
                "link_target": spec.link_target, "xattrs": spec.xattrs, "extensions": {},
                "mtime": spec.mtime, "executable": spec.executable,
                "payload": {"encoding": "repeat-byte", "byte": 0x35, "count": spec.size_bytes}})
        }).collect();
        json!({"options": {"object_id": options.object_id, "caller_object_id": options.caller_object_id,
            "chunk_size": options.chunk_size, "metadata_preservation": "archival", "encryption": options.encryption,
            "write_timestamp": options.write_timestamp, "manifest_file_id": options.manifest_file_id,
            "extensions": {}}, "files": files})
    }).collect();
    json!({"tape_uuid": uuid::Uuid::from_bytes(inputs.tape_uuid).to_string(), "scheme": inputs.scheme,
        "block_size": inputs.block_size, "compression": false, "objects": objects,
        "checkpoint_after_objects": inputs.checkpoint_after_objects,
        "nominal_extent_bytes": inputs.nominal_extent_bytes,
        "writer_identity": {"writer_version": identity.writer_version, "write_timestamp": identity.write_timestamp},
        "edition_id_hex": hex(&inputs.edition_id), "edition_sequence": inputs.edition_sequence,
        "parity_map_sequence_start": inputs.parity_map_sequence_start, "directory_flags": inputs.directory_flags,
        "diagnostic_keys_present": inputs.diagnostic_keys_present, "capacity_bytes": inputs.capacity_bytes,
        "stop": match inputs.stop { TapeImageStop::Finalized => json!({"kind": "finalized"}),
            TapeImageStop::CommittedPrefix { torn_records, torn_record_fill } => json!({"kind": "committed-prefix", "torn_records": torn_records, "torn_record_fill": torn_record_fill}) }})
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn write_model(
    inputs: TapeImageInputs,
) -> Result<(WrittenTapeImage, ExportedTapeImage), String> {
    let mut world = VirtualWorld::single_drive("IMAGE-LIB", 0x100, "IMAGE-DRV", 0x400, 1);
    world.put_tape_in_drive(
        0x100,
        "IMAGE001",
        None,
        VirtualTape::empty(64 * 1024 * 1024, BLOCK),
    );
    let world = Arc::new(Mutex::new(world));
    let transport = ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: 0x100 });
    let mut drive = DriveHandle::open_standalone_with_transport(
        Path::new("/dev/sg-image-model"),
        Box::new(transport),
    )
    .map_err(|e| e.to_string())?;
    let written = write_tape_image(&mut drive, inputs)?;
    let image = world.lock().unwrap().tapes["IMAGE001"].export_image();
    Ok((written, image))
}

/// Generate via the shared production chain. Only the conflicting-edition
/// fixture replaces bytes, using the same chain with exactly two changed inputs.
pub fn generate(name: &str) -> Result<VectorImage, String> {
    let inputs = inputs(name);
    let mut recipe = recipe(&inputs);
    let (written, mut image) = write_model(inputs.clone())?;
    if name == "two-edition" {
        let mut second = inputs;
        second.edition_id = [0x55; 16];
        second.edition_sequence += 1;
        recipe["replica_b_replacement"] = json!({"source_tape_file": 6, "edition_id_hex": hex(&second.edition_id), "edition_sequence": second.edition_sequence, "other_inputs": "identical"});
        let (_, alternate) = write_model(second)?;
        assert_eq!(image.files[6].start_record, alternate.files[6].start_record);
        image.files[6] = alternate.files[6].clone();
    }
    check_layout(name, &written, &image)?;
    Ok(VectorImage {
        written,
        image,
        recipe,
    })
}

/// Compare the frozen table, never repair it from the writer's output.
fn check_layout(
    name: &str,
    written: &WrittenTapeImage,
    image: &ExportedTapeImage,
) -> Result<(), String> {
    let source: Value = serde_json::from_str(EXPECTATIONS).unwrap();
    let expected = &source["images"][if name == "two-edition" {
        "a4-minimal"
    } else {
        name
    }];
    let blocks: Vec<_> = image
        .files
        .iter()
        .filter(|f| f.filemark_record.is_some())
        .map(|f| f.record_offsets.len())
        .collect();
    let starts: Vec<_> = image
        .files
        .iter()
        .filter(|f| f.filemark_record.is_some())
        .map(|f| f.start_record)
        .collect();
    let actual = json!({"file_blocks": blocks, "file_starts": starts, "tape_files": blocks.len(), "records": image.eod_record});
    if name.starts_with("unfinalized-") {
        let append = image.files.last().unwrap().start_record;
        let w = written
            .sidecars
            .last()
            .unwrap()
            .protected_ordinal_end_exclusive;
        let t: u64 = written
            .object_rows
            .iter()
            .map(|r| r.stored_block_count)
            .sum();
        if actual["file_blocks"] != expected["committed_file_blocks"]
            || json!(append) != expected["append_lba"]
            || json!(image.eod_record) != expected["physical_eod"]
            || w != 4
            || t != if name == "unfinalized-open" { 6 } else { 4 }
        {
            return Err(format!("D2 layout disagreement {name}: {actual}; append={append}, W={w}, T={t}; expected {expected}"));
        }
        println!("layout {name}: {actual}; append={append}, W={w}, T={t}");
    } else {
        for key in ["file_blocks", "file_starts", "tape_files", "records"] {
            if actual[key] != expected[key] {
                return Err(format!(
                    "D2 layout disagreement {name}: {actual}; expected {expected}"
                ));
            }
        }
        println!("layout {name}: {actual}");
    }
    Ok(())
}

/// Resolve prose fault descriptions once, in the generator. The executor reads
/// only this concrete map; no expected outcome influences fault resolution.
pub fn fault_map(case: &Value, image: &ExportedTapeImage) -> Value {
    // No key of a case may be ignored silently. `sections`, `construction` and
    // `note` describe the case and are not read; `object_authority` names the
    // one authority the executor supplies, which is none. `read_data_addresses`
    // are Object addresses the Reader reads through the production object
    // source, which meets the fault itself; `erratum` names the erratum set
    // whose author wrote the case's expected outcome.
    for key in case.as_object().expect("case object").keys() {
        assert!(
            matches!(
                key.as_str(),
                "id" | "image"
                    | "checks"
                    | "fault"
                    | "failed_data_addresses"
                    | "hints"
                    | "observations"
                    | "expected"
                    | "pinned"
                    | "sections"
                    | "construction"
                    | "note"
                    | "object_authority"
                    | "read_data_addresses"
                    | "erratum"
            ),
            "{}: unknown case key {key}",
            case["id"]
        );
    }
    if let Some(erratum) = case.get("erratum") {
        assert!(
            matches!(erratum.as_str(), Some("E2" | "E3" | "E4" | "S6")),
            "{}: erratum set",
            case["id"]
        );
    }
    assert_eq!(
        case.get("checks").is_some(),
        case["erratum"] == "S6",
        "S6 requires explicit checks; other sets do not accept them"
    );
    if let Some(authority) = case.get("object_authority") {
        assert_eq!(
            authority, "none supplied",
            "{}: object authority",
            case["id"]
        );
    }
    for key in case["fault"].as_object().expect("fault object").keys() {
        assert!(
            matches!(
                key.as_str(),
                "unreadable_lbas"
                    | "unreadable"
                    | "removed_filemark_after_tape_file"
                    | "record_faults"
                    | "extra_records"
                    | "appended_files"
                    | "second_edition_replica"
                    | "parity_map_edits"
            ),
            "{}: unknown fault key {key}",
            case["id"]
        );
    }
    let mut lbas: Vec<u64> = case["fault"]["unreadable_lbas"]
        .as_array()
        .map(|v| v.iter().map(|n| n.as_u64().unwrap()).collect())
        .unwrap_or_default();
    let start = |f: usize| image.files[f].start_record as u64;
    let last = |f: usize| start(f) + image.files[f].record_offsets.len() as u64 - 1;
    let two = case["image"] == "two-epoch";
    let map_file = if two { 4 } else { 3 };
    if let Some(description) = case["fault"]["unreadable"].as_str() {
        assert!(matches!(description,
            "epoch 0 sidecar primary header" | "epoch 0 sidecar footer" |
            "epoch 0 sidecar primary header and footer" |
            "all three replica header records; the sidecar's primary header and footer" |
            "all three replica header records; epoch 0 sidecar primary header, tail copy and footer" |
            "both ParityMap copies (tape file 3)" |
            "both ParityMap copies; epoch 0 sidecar primary header and footer" |
            "epoch 0 sidecar footer; both ParityMap copies" |
            "every record of all three terminal replicas" |
            "every record of all three terminal replicas; epoch 0 sidecar primary header"
        ), "unrecognized fault description: {description}");
        if description.starts_with("every record of all three terminal replicas") {
            for replica in [map_file + 1, map_file + 3, map_file + 5] {
                lbas.extend(start(replica)..=last(replica));
            }
        }
        if description.contains("all three replica header") {
            lbas.extend([
                start(map_file + 1),
                start(map_file + 3),
                start(map_file + 5),
            ]);
        }
        if description.contains("primary header") {
            lbas.push(start(2));
        }
        if description.contains("footer") {
            lbas.push(last(2));
        }
        if description.contains("tail copy") {
            lbas.push(last(2) - 1);
        }
        if description.contains("both ParityMap copies") {
            lbas.extend([start(map_file), start(map_file) + 1]);
        }
    }
    let addresses = if case["failed_data_addresses"].is_string() {
        assert_eq!(
            case["failed_data_addresses"],
            "one data block in each epoch"
        );
        json!([[1, 0], [1, 4]])
    } else {
        case["failed_data_addresses"]
            .as_array()
            .map_or(json!([]), |v| json!(v))
    };
    for pair in addresses.as_array().unwrap() {
        lbas.push(start(pair[0].as_u64().unwrap() as usize) + pair[1].as_u64().unwrap());
    }
    lbas.sort_unstable();
    lbas.dedup();
    // Resolve the stated physical LBAs on the tape after filemark removal.
    let removed = case["fault"]["removed_filemark_after_tape_file"].as_u64();
    let mut physical_records = Vec::new();
    let (mut tape_file, mut record_index) = (0, 0);
    for (original_file, file) in image.files.iter().enumerate() {
        for _ in &file.record_offsets {
            physical_records.push((tape_file, record_index, false));
            record_index += 1;
        }
        if file.filemark_record.is_some() && removed != Some(original_file as u64) {
            physical_records.push((tape_file, record_index, true));
            tape_file += 1;
            record_index = 0;
        }
    }
    let records: Vec<_> = lbas
        .iter()
        .map(|lba| {
            let &(f, index, filemark) = physical_records
                .get(*lba as usize)
                .expect("fault within modified image");
            json!({"lba": lba, "tape_file": f, "record_index": index, "filemark": filemark})
        })
        .collect();
    let mut map = json!({"image": case["image"], "unreadable_records": records, "removed_filemark_after_tape_file": case["fault"]["removed_filemark_after_tape_file"], "failed_data_addresses": addresses});
    if let Some(reads) = case.get("read_data_addresses") {
        // The Reader meets the fault itself: these addresses are not made
        // unreadable, and no recovery is invoked for them directly.
        let reads = reads.as_array().expect("read_data_addresses is a list");
        for pair in reads {
            let pair = pair.as_array().expect("an address is a pair");
            assert!(
                pair.len() == 2 && pair.iter().all(Value::is_u64),
                "{}: an address is (tape file, block within file)",
                case["id"]
            );
            let (file, block) = (
                pair[0].as_u64().unwrap() as usize,
                pair[1].as_u64().unwrap(),
            );
            assert!(
                (block as usize) < image.files[file].record_offsets.len(),
                "{}: read address outside its tape file",
                case["id"]
            );
        }
        map["read_data_addresses"] = json!(reads);
    }
    if let Some(faults) = case["fault"].get("record_faults") {
        assert!(faults.is_array(), "{}: record_faults is a list", case["id"]);
        map["record_edits"] = record_edits(case, image);
    }
    if let Some(edits) = case["fault"].get("parity_map_edits") {
        // The ParityMap's three blocks are rebuilt through the production
        // codec, so every payload length, hash and CRC is recomputed.
        let mut resolved = map["record_edits"].as_array().cloned().unwrap_or_default();
        resolved.extend(parity_map_edits(case, edits, image));
        map["record_edits"] = json!(resolved);
    }
    if let Some(removed) = removed {
        let removed_lba = image.files[removed as usize]
            .filemark_record
            .expect("removed filemark exists") as u64;
        if let Some(edits) = map.get_mut("record_edits").and_then(Value::as_array_mut) {
            for edit in edits {
                let lba = edit["lba"].as_u64().unwrap();
                edit["lba"] = json!(lba - u64::from(lba > removed_lba));
            }
        }
    }
    if let Some(extra) = case["fault"].get("extra_records") {
        map["record_insertions"] = record_insertions(case, extra, image);
    }
    let appended = case["fault"].get("appended_files");
    let replica = case["fault"].get("second_edition_replica");
    assert!(
        appended.is_none() || replica.is_none(),
        "{}: appended_files and second_edition_replica are separate keys",
        case["id"]
    );
    if let Some(appended) = appended {
        map["appended_files"] = appended_files(case, appended, image);
    }
    if let Some(replica) = replica {
        map["appended_files"] = second_edition_replica_file(case, replica, image);
    }
    if let Some(observations) = case.get("observations") {
        // Each observation runs the reader separately, with its own supplied values.
        assert!(
            case.get("hints").is_none(),
            "a case gives hints or observations, not both"
        );
        check_observations(observations);
        map["observations"] = observations.clone();
    } else {
        check_hints(&case["hints"]);
        map["hints"] = case["hints"].clone();
    }
    map
}

/// Supplied values, or none (null). The executor always supplies the image's
/// own tape UUID, so that is the only UUID a case may name.
pub(crate) fn check_hints(hints: &Value) {
    if hints.is_null() {
        return;
    }
    let keys = hints.as_object().expect("hints are an object or null");
    assert_eq!(
        keys.keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        ["block_size", "scheme", "tape_uuid"].into(),
        "hints name the tape UUID, the block size and the scheme, and nothing else"
    );
    assert_eq!(hints["tape_uuid"], "the image's", "supplied tape UUID");
    assert!(hints["block_size"].is_u64(), "supplied block size");
    check_scheme(&hints["scheme"], true);
}

/// A scheme's k, m and S; every one is required in supplied values, and a
/// replacement payload names at least one. No other key is allowed.
fn check_scheme(scheme: &Value, complete: bool) {
    let keys = scheme.as_object().expect("a scheme is an object");
    for (key, value) in keys {
        assert!(
            matches!(key.as_str(), "k" | "m" | "S"),
            "unknown scheme key {key}"
        );
        assert!(value.is_u64(), "scheme {key} is an unsigned integer");
    }
    assert!(
        if complete {
            keys.len() == 3
        } else {
            !keys.is_empty()
        },
        "scheme keys {scheme}"
    );
}

/// Observations are `{id, hints}` with distinct ids.
pub(crate) fn check_observations(observations: &Value) {
    let observations = observations.as_array().expect("observations are a list");
    assert!(!observations.is_empty(), "at least one observation");
    let mut ids = std::collections::BTreeSet::new();
    for observation in observations {
        let keys = observation
            .as_object()
            .expect("an observation is an object");
        assert_eq!(
            keys.keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["hints", "id"].into(),
            "an observation names its id and its hints, and nothing else"
        );
        let id = observation["id"].as_str().expect("observation id");
        assert!(ids.insert(id), "duplicate observation {id}");
        check_hints(&observation["hints"]);
    }
}

/// One byte from a two-digit hex string.
#[cfg(test)]
pub(crate) fn unhex_byte(text: &str) -> u8 {
    let bytes = unhex(text);
    assert_eq!(bytes.len(), 1, "one byte");
    bytes[0]
}

fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "odd hex string {text}");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex bytes"))
        .collect()
}

/// The bootstrap frame of REM-PARITY 8.1, the only record whose CRCs and
/// payload the record-fault vocabulary knows how to rebuild.
const BOOTSTRAP_PAYLOAD_LEN_FIELD: usize = 0x2c;
const BOOTSTRAP_HEADER_CRC_FIELD: usize = 0x30;
const BOOTSTRAP_PAYLOAD_START: usize = 0x38;

/// Resolve the two record fault kinds into concrete ordered edits:
/// - a record length: the record is replaced by one of the stated length, the
///   original's leading bytes when shorter, the original followed by zero bytes
///   when longer;
/// - byte edits: bytes set, or XORed with a mask, at offsets within the record;
///   a replacement bootstrap payload built from the original payload; then the
///   bootstrap's header CRC and payload CRC recomputed or left stale as stated.
///
/// The executor applies only the resolved edits, checking each edit's old bytes.
pub(crate) fn record_edits(case: &Value, image: &ExportedTapeImage) -> Value {
    const KEYS: [&str; 9] = [
        "tape_file",
        "record_index",
        "length",
        "byte_edits",
        "payload",
        "header_crc",
        "payload_crc",
        "sidecar_hash",
        "sidecar_crcs",
    ];
    let faults = case["fault"]["record_faults"].as_array().unwrap();
    json!(faults
        .iter()
        .map(|fault| {
            for key in fault.as_object().expect("record fault object").keys() {
                assert!(KEYS.contains(&key.as_str()), "unknown record fault key {key}");
            }
            let tape_file = fault["tape_file"].as_u64().unwrap() as usize;
            let record_index = fault["record_index"].as_u64().unwrap() as usize;
            let file = &image.files[tape_file];
            let start = file.record_offsets[record_index];
            let end = file
                .record_offsets
                .get(record_index + 1)
                .copied()
                .unwrap_or(file.bytes.len());
            let original = &file.bytes[start..end];
            let length = fault.get("length").map_or(original.len(), |n| {
                n.as_u64().expect("a record length is an unsigned integer") as usize
            });
            let construction = match length.cmp(&original.len()) {
                std::cmp::Ordering::Less => format!("the first {length} bytes of the original record"),
                std::cmp::Ordering::Equal => "the original record's length".to_string(),
                std::cmp::Ordering::Greater => format!(
                    "the original record followed by {} zero bytes",
                    length - original.len()
                ),
            };
            let mut record = original[..length.min(original.len())].to_vec();
            record.resize(length, 0);
            let mut edits = Vec::new();
            let mut write = |record: &mut Vec<u8>, offset: usize, new: &[u8], reason: &str| {
                let old = record[offset..offset + new.len()].to_vec();
                if old != new {
                    edits.push(json!({"offset": offset, "old_bytes": hex(&old), "new_bytes": hex(new), "reason": reason}));
                    record[offset..offset + new.len()].copy_from_slice(new);
                }
            };
            let byte_edits = fault.get("byte_edits").map(|edits| {
                edits.as_array().expect("byte_edits is a list")
            });
            for edit in byte_edits.into_iter().flatten() {
                for key in edit.as_object().expect("byte edit object").keys() {
                    assert!(
                        matches!(key.as_str(), "offset" | "field" | "set" | "xor" | "from"),
                        "unknown byte edit key {key}"
                    );
                }
                let offset = usize::from_str_radix(
                    edit["offset"].as_str().unwrap().trim_start_matches("0x"),
                    16,
                )
                .expect("hex offset");
                let field = edit["field"].as_str().expect("byte edit field label");
                let width = match (edit["set"].as_str(), edit["xor"].as_str()) {
                    (Some(set), None) => unhex(set).len(),
                    (None, Some(mask)) => unhex(mask).len(),
                    _ => panic!("byte edit needs exactly one of set and xor: {edit}"),
                };
                if let Some(from) = edit.get("from") {
                    let from = from.as_str().expect("from is hex bytes");
                    assert_eq!(
                        hex(&record[offset..offset + width]),
                        from.to_ascii_lowercase(),
                        "{field}: the original bytes differ from the case's"
                    );
                }
                let (new, reason) = if let Some(set) = edit["set"].as_str() {
                    (unhex(set), format!("{field}: set"))
                } else {
                    let mask = unhex(edit["xor"].as_str().unwrap());
                    let new = record[offset..offset + width]
                        .iter()
                        .zip(&mask)
                        .map(|(byte, mask)| byte ^ mask)
                        .collect();
                    (new, format!("{field}: xor {}", hex(&mask)))
                };
                write(&mut record, offset, &new, &reason);
            }
            // A sidecar header/index copy whose index is one block: its
            // canonical metadata hash (Section 9.5) and its two CRCs (9.2),
            // recomputed after the edits above.
            let sidecar_treatment = |key: &str| {
                fault.get(key).map(|value| {
                    assert_eq!(value, "recomputed", "{key} is recomputed or absent");
                })
            };
            if sidecar_treatment("sidecar_hash").is_some() {
                assert_eq!(
                    u64::from_le_bytes(record[0x60..0x68].try_into().unwrap()),
                    1,
                    "sidecar_hash is defined for a one-block index"
                );
                let inline = u64::from_le_bytes(record[0x68..0x70].try_into().unwrap()) as usize;
                let mut hash = Sha256::new();
                hash.update(b"remanence-sidecar-metadata-v1");
                hash.update(&record[..0x90]);
                hash.update(&record[0xc8..0xc8 + inline]);
                let hash: [u8; 32] = hash.finalize().into();
                write(&mut record, 0x98, &hash, "canonical_metadata_hash recomputed");
            }
            if sidecar_treatment("sidecar_crcs").is_some() {
                let crc = remanence_parity::crc64_xz(&record[..0xc0]);
                write(&mut record, 0xc0, &crc.to_le_bytes(), "header_crc64 recomputed");
                let tail = record.len() - 8;
                let crc = remanence_parity::crc64_xz(&record[..tail]);
                write(&mut record, tail, &crc.to_le_bytes(), "block0_crc64 recomputed");
            }
            let bootstrap_frame = tape_file == 0 && record_index == 0;
            let old_len = |record: &[u8]| {
                u32::from_le_bytes(
                    record[BOOTSTRAP_PAYLOAD_LEN_FIELD..BOOTSTRAP_PAYLOAD_LEN_FIELD + 4]
                        .try_into()
                        .unwrap(),
                ) as usize
            };
            let mut length_changed = false;
            if let Some(changes) = fault.get("payload") {
                let changes = changes.as_object().expect("a payload change is an object");
                assert!(bootstrap_frame, "a replacement payload is defined for the bootstrap only");
                assert_eq!(
                    fault["payload_crc"], "recomputed",
                    "a replacement payload needs its CRC recomputed"
                );
                let len = old_len(&record);
                let payload_end = BOOTSTRAP_PAYLOAD_START + len;
                let mut payload: ciborium::value::Value =
                    ciborium::from_reader(&record[BOOTSTRAP_PAYLOAD_START..payload_end])
                        .expect("original bootstrap payload decodes");
                let entries = payload.as_map_mut().expect("bootstrap payload is a map");
                let key_is = |key: &ciborium::value::Value, n: i128| {
                    key.as_integer().map(i128::from) == Some(n)
                };
                for (change, value) in changes {
                    match change.as_str() {
                        "scheme" => {
                            check_scheme(value, false);
                            let scheme = entries
                                .iter_mut()
                                .find(|(key, _)| key_is(key, 1))
                                .and_then(|(_, scheme)| scheme.as_map_mut())
                                .expect("payload key 1 is the scheme map");
                            for (key, name) in [(2, "k"), (3, "m"), (4, "S")] {
                                let Some(n) = value.get(name) else { continue };
                                let n = n.as_u64().expect("checked scheme value");
                                scheme
                                    .iter_mut()
                                    .find(|(k, _)| key_is(k, key))
                                    .expect("scheme field")
                                    .1 = ciborium::value::Value::Integer(n.into());
                            }
                        }
                        "drive_compression" => {
                            entries
                                .iter_mut()
                                .find(|(key, _)| key_is(key, 5))
                                .expect("payload key 5")
                                .1 = ciborium::value::Value::Bool(value.as_bool().unwrap());
                        }
                        "key_order" => {
                            let order: Vec<i128> = value
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|n| i128::from(n.as_u64().unwrap()))
                                .collect();
                            assert_eq!(order.len(), entries.len(), "key_order lists every key once");
                            let mut reordered = Vec::with_capacity(entries.len());
                            for key in order {
                                let index = entries
                                    .iter()
                                    .position(|(k, _)| key_is(k, key))
                                    .expect("key_order names a payload key");
                                reordered.push(entries.remove(index));
                            }
                            *entries = reordered;
                        }
                        other => panic!("unknown payload change {other}"),
                    }
                }
                let mut new = Vec::new();
                ciborium::into_writer(&payload, &mut new).expect("payload encodes");
                let changes: Vec<_> = changes.keys().map(String::as_str).collect();
                assert!(
                    BOOTSTRAP_PAYLOAD_START + new.len() + 8 <= record.len(),
                    "replacement payload fits the record"
                );
                let reason = format!("replacement payload ({})", changes.join(", "));
                write(&mut record, BOOTSTRAP_PAYLOAD_START, &new, &reason);
                // A shorter payload leaves old bytes past its new CRC: zero them,
                // so the fill after the new framing is conformant.
                let new_framed_end = BOOTSTRAP_PAYLOAD_START + new.len() + 8;
                let old_framed_end = BOOTSTRAP_PAYLOAD_START + len + 8;
                if new_framed_end < old_framed_end {
                    let zeros = vec![0; old_framed_end - new_framed_end];
                    write(&mut record, new_framed_end, &zeros, "zero fill after the shorter payload");
                }
                if new.len() != len {
                    length_changed = true;
                    write(
                        &mut record,
                        BOOTSTRAP_PAYLOAD_LEN_FIELD,
                        &(new.len() as u32).to_le_bytes(),
                        "cbor_payload_len updated",
                    );
                }
            }
            let treatment = |key: &str| {
                fault
                    .get(key)
                    .map(|value| value.as_str().expect("a CRC treatment is a string"))
            };
            match treatment("header_crc") {
                None | Some("stale") => {}
                Some(treatment @ ("recomputed" | "recomputed if the length changes")) => {
                    assert!(bootstrap_frame, "header CRC treatment is defined for the bootstrap only");
                    if treatment == "recomputed" || length_changed {
                        let crc = remanence_parity::crc64_xz(&record[..BOOTSTRAP_HEADER_CRC_FIELD]);
                        write(&mut record, BOOTSTRAP_HEADER_CRC_FIELD, &crc.to_le_bytes(), "header CRC recomputed");
                    }
                }
                Some(other) => panic!("unknown header CRC treatment {other}"),
            }
            match treatment("payload_crc") {
                None | Some("stale") => {}
                Some("recomputed") => {
                    assert!(bootstrap_frame, "payload CRC treatment is defined for the bootstrap only");
                    let end = BOOTSTRAP_PAYLOAD_START + old_len(&record);
                    let crc = remanence_parity::crc64_xz(&record[BOOTSTRAP_PAYLOAD_START..end]);
                    write(&mut record, end, &crc.to_le_bytes(), "payload CRC recomputed");
                }
                Some(other) => panic!("unknown payload CRC treatment {other}"),
            }
            json!({"lba": file.start_record + record_index, "tape_file": tape_file, "record_index": record_index,
                "original_length": original.len(), "length": length, "construction": construction,
                "edits": edits, "sha256": hex(&Sha256::digest(&record))})
        })
        .collect::<Vec<_>>())
}

/// The concrete records of a tape file, as the image holds them.
fn file_records(image: &ExportedTapeImage, tape_file: usize) -> Vec<Vec<u8>> {
    let file = &image.files[tape_file];
    (0..file.record_offsets.len())
        .map(|i| {
            let start = file.record_offsets[i];
            let end = file
                .record_offsets
                .get(i + 1)
                .copied()
                .unwrap_or(file.bytes.len());
            file.bytes[start..end].to_vec()
        })
        .collect()
}

/// Resolve `parity_map_edits`: each names a ParityMap tape file, a directory
/// entry, a field and a change (`add`, `set` or `xor`). The ParityMap is
/// decoded and re-encoded through the production codec, which recomputes the
/// payload length, the payload hash and every CRC, so the result still
/// validates. The codec's re-encoding of the unchanged ParityMap is checked to
/// reproduce the image's bytes first. Each block that changes becomes a
/// resolved record edit.
fn parity_map_edits(case: &Value, edits: &Value, image: &ExportedTapeImage) -> Vec<Value> {
    let id = case["id"].as_str().unwrap_or_default();
    let edits = edits.as_array().expect("parity_map_edits is a list");
    let mut by_file: std::collections::BTreeMap<usize, Vec<&Value>> = Default::default();
    for edit in edits {
        for key in edit.as_object().expect("parity map edit object").keys() {
            assert!(
                matches!(
                    key.as_str(),
                    "tape_file" | "directory_entry" | "field" | "add" | "set" | "xor"
                ),
                "{id}: unknown parity map edit key {key}"
            );
        }
        by_file
            .entry(edit["tape_file"].as_u64().expect("parity map tape file") as usize)
            .or_default()
            .push(edit);
    }
    let uuid: [u8; 16] = image.files[0].bytes[0x10..0x20].try_into().unwrap();
    let mut resolved = Vec::new();
    for (tape_file, edits) in by_file {
        let blocks = file_records(image, tape_file);
        let decoded = remanence_parity::parse_parity_map_tape_file(&blocks, &uuid)
            .unwrap_or_else(|e| panic!("{id}: tape file {tape_file} is not a ParityMap: {e}"));
        let reencoded = remanence_parity::encode_parity_map_tape_file(&decoded.payload, BLOCK)
            .expect("the codec re-encodes a decoded ParityMap");
        assert_eq!(
            reencoded.blocks, blocks,
            "{id}: the codec's re-encoding of the unchanged ParityMap differs from the image"
        );
        let mut payload = decoded.payload.clone();
        for edit in edits {
            let index = edit["directory_entry"]
                .as_u64()
                .expect("directory entry index") as usize;
            let entry = &mut payload.directory.entries[index];
            match edit["field"].as_str().expect("parity map field") {
                "sidecar_total_block_count" => {
                    let add = edit["add"].as_u64().expect("a total change is an add");
                    entry.sidecar_total_block_count += add;
                }
                "canonical_metadata_hash" => {
                    let mask = unhex(edit["xor"].as_str().expect("a hash change is an xor"));
                    for (byte, mask) in entry.canonical_metadata_hash.iter_mut().zip(&mask) {
                        *byte ^= mask;
                    }
                }
                other => panic!("{id}: unknown parity map field {other}"),
            }
        }
        let rebuilt = remanence_parity::encode_parity_map_tape_file(&payload, BLOCK)
            .expect("the edited ParityMap still encodes");
        assert_eq!(
            rebuilt.blocks.len(),
            blocks.len(),
            "{id}: length is unchanged"
        );
        let file = &image.files[tape_file];
        for (record_index, (old, new)) in blocks.iter().zip(&rebuilt.blocks).enumerate() {
            if old == new {
                continue;
            }
            let mut edits = Vec::new();
            let mut offset = 0;
            while offset < old.len() {
                if old[offset] == new[offset] {
                    offset += 1;
                    continue;
                }
                let start = offset;
                while offset < old.len() && old[offset] != new[offset] {
                    offset += 1;
                }
                edits.push(json!({"offset": start, "old_bytes": hex(&old[start..offset]),
                    "new_bytes": hex(&new[start..offset]),
                    "reason": "ParityMap rebuilt through the codec with the edited directory entry (payload, hashes and CRCs recomputed)"}));
            }
            resolved.push(
                json!({"lba": file.start_record + record_index, "tape_file": tape_file,
                "record_index": record_index, "original_length": old.len(), "length": old.len(),
                "construction": "the ParityMap re-encoded with the edited directory entry",
                "edits": edits, "sha256": hex(&Sha256::digest(new))}),
            );
        }
    }
    resolved
}

/// Resolve `extra_records`: a record of stated length and fill inserted into a
/// tape file after a stated record (or its last record), before its filemark.
fn record_insertions(case: &Value, extra: &Value, image: &ExportedTapeImage) -> Value {
    let id = case["id"].as_str().unwrap_or_default();
    json!(extra
        .as_array()
        .expect("extra_records is a list")
        .iter()
        .map(|record| {
            for key in record.as_object().expect("extra record object").keys() {
                assert!(
                    matches!(key.as_str(), "tape_file" | "after" | "length" | "fill"),
                    "{id}: unknown extra record key {key}"
                );
            }
            let tape_file = record["tape_file"].as_u64().unwrap() as usize;
            let count = image.files[tape_file].record_offsets.len();
            let after = match record["after"].as_str() {
                Some("last record") => count - 1,
                other => panic!("{id}: an extra record goes after the last record, not {other:?}"),
            };
            let length = record["length"].as_u64().expect("extra record length") as usize;
            let fill = unhex(record["fill"].as_str().expect("extra record fill"));
            assert_eq!(fill.len(), 1, "{id}: one fill byte");
            let bytes = vec![fill[0]; length];
            json!({"tape_file": tape_file, "after_record_index": after, "length": length,
                "fill": hex(&fill), "sha256": hex(&Sha256::digest(&bytes))})
        })
        .collect::<Vec<_>>())
}

/// The bytes of one appended record, from its resolved description.
#[cfg(test)]
pub(crate) fn appended_record_bytes(record: &Value, image: &ExportedTapeImage) -> Vec<u8> {
    let bytes = match record["source"].as_str().expect("appended record source") {
        "foreign" => {
            check_keys(
                record,
                &["source", "length", "first_byte", "fill", "sha256"],
                "foreign record",
            );
            assert_eq!(record["fill"], FOREIGN_FILL, "foreign record fill");
            // Bytes that match no structure of the document: zeros, with the
            // stated first byte.
            let mut bytes = vec![0u8; record["length"].as_u64().unwrap() as usize];
            bytes[0] = unhex(record["first_byte"].as_str().unwrap())[0];
            bytes
        }
        "copy_of" => file_records(image, {
            check_keys(
                record,
                &["source", "tape_file", "record_index", "length", "sha256"],
                "copy_of record",
            );
            record["tape_file"].as_u64().unwrap() as usize
        })[record["record_index"].as_u64().unwrap() as usize]
            .clone(),
        "second_edition_replica" => {
            check_keys(
                record,
                &[
                    "source",
                    "planned_tape_file_number",
                    "planned_start_lba",
                    "record_index",
                    "length",
                    "sha256",
                    "role",
                    "base",
                    "fields",
                ],
                "second_edition_replica record",
            );
            let blocks = second_edition_replica_a(
                record["planned_tape_file_number"].as_u64().unwrap(),
                record["planned_start_lba"].as_u64().unwrap(),
            )
            .expect("second-edition replica");
            let index = record["record_index"].as_u64().unwrap() as usize;
            // The stated construction must agree with the bytes built: the
            // base record, patched with the stated fields and nothing else.
            let base = file_records(
                image,
                record["base"]["tape_file"].as_u64().unwrap() as usize,
            )[record["base"]["record_index"].as_u64().unwrap() as usize]
                .clone();
            assert_eq!(
                replica_record_description(&blocks, index, blocks.len()),
                json!({"role": record["role"], "base": record["base"], "fields": record["fields"]}),
                "second-edition replica record construction"
            );
            let mut patched = base;
            for field in record["fields"].as_object().unwrap().values() {
                let at = field["offset"].as_u64().unwrap() as usize;
                let value = unhex(field["hex"].as_str().unwrap());
                assert_eq!(value.len() as u64, field["length"].as_u64().unwrap());
                patched[at..at + value.len()].copy_from_slice(&value);
            }
            assert_eq!(patched, blocks[index], "base plus the stated fields");
            blocks[index].clone()
        }
        other => panic!("unknown appended record source {other}"),
    };
    assert_eq!(
        hex(&Sha256::digest(&bytes)),
        record["sha256"].as_str().unwrap(),
        "resolved appended record digest"
    );
    bytes
}

/// Resolve `appended_files`: tape files appended after the image's last
/// filemark, each a list of records of stated content, with or without a
/// trailing filemark. A record is foreign (zeros with a stated first byte) or
/// a byte copy of a given record of the image.
fn appended_files(case: &Value, appended: &Value, image: &ExportedTapeImage) -> Value {
    let id = case["id"].as_str().unwrap_or_default();
    json!(appended
        .as_array()
        .expect("appended_files is a list")
        .iter()
        .map(|file| {
            for key in file.as_object().expect("appended file object").keys() {
                assert!(
                    matches!(key.as_str(), "records" | "trailing_filemark"),
                    "{id}: unknown appended file key {key}"
                );
            }
            let trailing = file["trailing_filemark"]
                .as_bool()
                .expect("an appended file states its trailing filemark");
            let records: Vec<_> = file["records"]
                .as_array()
                .expect("appended file records")
                .iter()
                .map(|record| {
                    let kinds = record.as_object().expect("appended record object");
                    assert_eq!(kinds.len(), 1, "{id}: one record source");
                    let resolved = if let Some(foreign) = record.get("foreign") {
                        for key in foreign.as_object().unwrap().keys() {
                            assert_eq!(key, "first_byte", "{id}: unknown foreign record key");
                        }
                        json!({"source": "foreign", "length": BLOCK,
                            "first_byte": foreign["first_byte"], "fill": FOREIGN_FILL})
                    } else if let Some(copy) = record.get("copy_of") {
                        for key in copy.as_object().unwrap().keys() {
                            assert!(
                                matches!(key.as_str(), "tape_file" | "record"),
                                "{id}: unknown copy_of key {key}"
                            );
                        }
                        let tape_file = copy["tape_file"].as_u64().unwrap() as usize;
                        let index = match copy["record"].as_str() {
                            Some("last") => image.files[tape_file].record_offsets.len() - 1,
                            other => panic!("{id}: copy_of names the last record, not {other:?}"),
                        };
                        json!({"source": "copy_of", "tape_file": tape_file, "record_index": index})
                    } else {
                        panic!("{id}: an appended record is foreign or a copy_of");
                    };
                    let mut resolved = resolved;
                    let bytes = match resolved["source"].as_str().unwrap() {
                        "foreign" => {
                            let mut b = vec![0u8; BLOCK as usize];
                            b[0] = unhex(resolved["first_byte"].as_str().unwrap())[0];
                            b
                        }
                        _ => file_records(image, resolved["tape_file"].as_u64().unwrap() as usize)
                            [resolved["record_index"].as_u64().unwrap() as usize]
                            .clone(),
                    };
                    resolved["length"] = json!(bytes.len());
                    resolved["sha256"] = json!(hex(&Sha256::digest(&bytes)));
                    resolved
                })
                .collect();
            json!({"records": records, "trailing_filemark": trailing})
        })
        .collect::<Vec<_>>())
}

/// Resolve `second_edition_replica`: a replica of a second edition (its edition
/// id and edition sequence differ from the profile's and nothing else does),
/// planned as a later tape file and written directly after the last record of
/// a named tape file, with its own trailing filemark.
fn second_edition_replica_file(case: &Value, replica: &Value, image: &ExportedTapeImage) -> Value {
    let id = case["id"].as_str().unwrap_or_default();
    for key in replica
        .as_object()
        .expect("second edition replica object")
        .keys()
    {
        assert!(
            matches!(
                key.as_str(),
                "replica" | "planned_tape_file" | "after_last_record_of"
            ),
            "{id}: unknown second edition replica key {key}"
        );
    }
    assert_eq!(replica["replica"], "A", "{id}: only replica A is defined");
    let after = replica["after_last_record_of"].as_u64().unwrap() as usize;
    let planned_tape_file = replica["planned_tape_file"].as_u64().unwrap();
    let last =
        image.files[after].start_record as u64 + image.files[after].record_offsets.len() as u64 - 1;
    let start = last + 1;
    let records =
        second_edition_replica_a(planned_tape_file, start).expect("second-edition replica");
    let base_file = REPLICA_BASE_TAPE_FILE;
    let base = file_records(image, base_file);
    assert_eq!(base.len(), records.len(), "{id}: replica and base sizes");
    let resolved: Vec<_> = records
        .iter()
        .enumerate()
        .map(|(i, bytes)| {
            let mut record = json!({"source": "second_edition_replica",
                "planned_tape_file_number": planned_tape_file,
                "planned_start_lba": start, "record_index": i, "length": bytes.len(),
                "sha256": hex(&Sha256::digest(bytes))});
            record.as_object_mut().unwrap().extend(
                replica_record_description(&records, i, records.len())
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            record
        })
        .collect();
    let replica = replica_summary(&records, planned_tape_file, start, base_file);
    json!([{"records": resolved, "trailing_filemark": true, "replica": replica}])
}

/// The replica's stated plan, read back from the bytes of its header.
pub(crate) fn replica_summary(
    records: &[Vec<u8>],
    planned_tape_file: u64,
    start: u64,
    base_file: usize,
) -> Value {
    let header = &records[0];
    let read_u64 = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().unwrap());
    let components: Vec<_> = (0..5)
        .map(|c| {
            let at = 0x148 + c * 32;
            let kind = u16::from_le_bytes(header[at..at + 2].try_into().unwrap());
            json!({"kind": kind,
                "kind_name": match kind { 4 => "TapeIndexReplica", 5 => "IndexSeparationExtent", _ => unreachable!() },
                "ordinal": u16::from_le_bytes(header[at + 2..at + 4].try_into().unwrap()),
                "filemark_count": u32::from_le_bytes(header[at + 4..at + 8].try_into().unwrap()),
                "planned_tape_file_number": read_u64(at + 8),
                "planned_start_lba": read_u64(at + 16),
                "record_count": read_u64(at + 24)})
        })
        .collect();
    json!({
        "replica": "A",
        "replica_ordinal": u16::from_le_bytes(header[0x38..0x3a].try_into().unwrap()),
        "edition_id": hex(&header[0x20..0x30]),
        "edition_sequence": read_u64(0x30),
        "base_edition_sequence": read_u64(0x30) - 1,
        "planned_tape_file_number": planned_tape_file,
        "planned_start_lba": start,
        "expected_eod_lba": read_u64(0xa0),
        "planned_layout": components,
        "header_record_index": 0,
        "payload_record_index": 1,
        "footer_record_index": records.len() - 1,
        "base": {"tape_file": base_file, "meaning": "the image's own replica A (tape file 4 of the unmodified image); the second-edition replica is that tape file with only the listed fields of its header and footer replaced and its payload record byte for byte the same"},
    })
}

/// Tape file of the unmodified image that holds its replica A; the
/// second-edition replica is derived from it.
const REPLICA_BASE_TAPE_FILE: usize = 4;

/// The stated fill of a foreign record's bytes after its first byte.
pub(crate) const FOREIGN_FILL: &str = "first_byte, then zeros to the stated length";

/// Fail on any key of `value` that is not listed.
#[cfg(test)]
fn check_keys(value: &Value, allowed: &[&str], what: &str) {
    for key in value.as_object().expect("object").keys() {
        assert!(allowed.contains(&key.as_str()), "unknown {what} key {key}");
    }
}

/// How one record of the second-edition replica differs from its base record
/// (the same-indexed record of the image's own replica A): its role, the base,
/// and the named fields replaced, each with offset, length and new bytes.
/// The payload record replaces nothing.
fn replica_record_description(records: &[Vec<u8>], index: usize, count: usize) -> Value {
    let role = if index == 0 {
        "header"
    } else if index == count - 1 {
        "footer"
    } else {
        "payload"
    };
    let mut fields: Vec<(&str, usize, usize)> = Vec::new();
    if role != "payload" {
        fields.extend([
            ("edition_id", 0x20, 16),
            ("edition_sequence", 0x30, 8),
            ("planned_tape_file_number", 0x88, 8),
            ("planned_start_lba", 0x90, 8),
            ("expected_eod_lba", 0xa0, 8),
            ("edition_digest", 0xe8, 32),
            ("layout_digest", 0x108, 32),
            ("descriptor_digest", 0x128, 32),
            ("planned_layout_components", 0x148, 160),
        ]);
    }
    if role == "footer" {
        fields.extend([
            ("header_sha256", 0x2b8, 32),
            ("observed_tape_file", 0x2d8, 8),
            ("observed_start_lba", 0x2e0, 8),
            ("observed_footer_lba", 0x2f0, 8),
        ]);
    }
    if role != "payload" {
        fields.push(("frame_crc64", 0x3f8, 8));
    }
    let fields: serde_json::Map<String, Value> = fields
        .into_iter()
        .map(|(name, offset, length)| {
            (
                name.to_string(),
                json!({"offset": offset, "length": length,
                    "hex": hex(&records[index][offset..offset + length])}),
            )
        })
        .collect();
    json!({"role": role, "base": {"tape_file": REPLICA_BASE_TAPE_FILE, "record_index": index},
        "fields": fields})
}

/// The records of replica A of a second edition of `a4-minimal` (edition id
/// and sequence differ, as in `two-edition`), re-planned as tape file
/// `planned_tape_file` starting at `planned_start_lba`, with its own five-file
/// terminal layout. The production planner refuses a layout whose first
/// replica is not the covered prefix's next tape file, so the header and footer
/// are rewritten field by field, and every digest and CRC they carry is
/// recomputed: the replica validates on its own, and lies where it says it is.
pub(crate) fn second_edition_replica_a(
    planned_tape_file: u64,
    planned_start_lba: u64,
) -> Result<Vec<Vec<u8>>, String> {
    let mut second = inputs("a4-minimal");
    second.edition_id = [0x55; 16];
    second.edition_sequence += 1;
    let nominal = second.nominal_extent_bytes;
    let (_, image) = write_model(second)?;
    let mut blocks = file_records(&image, 4);
    let separation =
        remanence_parity::index_separation_records(BLOCK, nominal).map_err(|e| e.to_string())?;
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        BLOCK,
        planned_tape_file,
        planned_start_lba,
        blocks.len() as u64,
        separation,
    )
    .map_err(|e| e.to_string())?;
    let put = |block: &mut Vec<u8>, offset: usize, bytes: &[u8]| {
        block[offset..offset + bytes.len()].copy_from_slice(bytes)
    };
    let header_len = blocks.len();
    for index in [0, header_len - 1] {
        let block = &mut blocks[index];
        put(block, 0x88, &planned_tape_file.to_le_bytes());
        put(block, 0x90, &planned_start_lba.to_le_bytes());
        put(block, 0xa0, &layout.expected_eod_lba.to_le_bytes());
        put(block, 0x108, &layout.digest().map_err(|e| e.to_string())?);
        for (i, component) in layout.components.iter().enumerate() {
            let at = 0x148 + i * 32;
            put(block, at, &component.kind.code().to_le_bytes());
            put(block, at + 2, &component.ordinal.to_le_bytes());
            put(block, at + 4, &1u32.to_le_bytes());
            put(
                block,
                at + 8,
                &component.planned_tape_file_number.to_le_bytes(),
            );
            put(block, at + 16, &component.planned_start_lba.to_le_bytes());
            put(block, at + 24, &component.record_count.to_le_bytes());
        }
        let mut descriptor = Sha256::new();
        descriptor.update(b"REM-TAPE-INDEX-REPLICA-DESCRIPTOR-V1\0");
        descriptor.update(&block[0xe8..0x128]);
        descriptor.update(&block[0x38..0x3c]);
        descriptor.update(&block[0x148..0x168]);
        descriptor.update(&block[0x98..0xa0]);
        put(block, 0x128, &descriptor.finalize());
    }
    let footer = header_len - 1;
    let last_record = planned_start_lba + (header_len as u64) - 1;
    put(&mut blocks[footer], 0x2d8, &planned_tape_file.to_le_bytes());
    put(&mut blocks[footer], 0x2e0, &planned_start_lba.to_le_bytes());
    put(&mut blocks[footer], 0x2f0, &last_record.to_le_bytes());
    let crc = remanence_parity::crc64_xz(&blocks[0][..0x3f8]);
    put(&mut blocks[0], 0x3f8, &crc.to_le_bytes());
    let header_sha: [u8; 32] = Sha256::digest(&blocks[0]).into();
    put(&mut blocks[footer], 0x2b8, &header_sha);
    let crc = remanence_parity::crc64_xz(&blocks[footer][..0x3f8]);
    put(&mut blocks[footer], 0x3f8, &crc.to_le_bytes());
    Ok(blocks)
}

/// Whether an expectation's quote occurs verbatim in the in-progress
/// REM-PARITY text, whitespace aside. A leading bracketed locator, such as
/// `[Section 5.2, excerpt]`, names the excerpt's place and is not quoted text.
#[cfg(test)]
pub(crate) fn specification_quote_holds(quote: &str) -> bool {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let normalise = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = TEXT.get_or_init(|| {
        normalise(
            &std::fs::read_to_string(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../specs/in-progress/rem-parity-1-specification.md"),
            )
            .expect("REM-PARITY text"),
        )
    });
    let quote = match quote
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("] "))
    {
        Some((_, quoted)) => quoted,
        None => quote,
    };
    !quote.trim().is_empty() && text.contains(&normalise(quote))
}

/// Apply resolved record edits to one record, checking every old byte. The
/// damage executor uses this; it never rebuilds a record from the case text.
pub fn apply_record_edits(original: &[u8], resolved: &Value) -> Vec<u8> {
    for key in resolved.as_object().expect("record edit object").keys() {
        assert!(
            matches!(
                key.as_str(),
                "lba"
                    | "tape_file"
                    | "record_index"
                    | "original_length"
                    | "length"
                    | "construction"
                    | "edits"
                    | "sha256"
            ),
            "unknown record edit key {key}"
        );
    }
    for edit in resolved["edits"].as_array().expect("edits list") {
        for key in edit.as_object().expect("edit object").keys() {
            assert!(
                matches!(
                    key.as_str(),
                    "offset" | "old_bytes" | "new_bytes" | "reason"
                ),
                "unknown resolved edit key {key}"
            );
        }
    }
    let length = resolved["length"].as_u64().unwrap() as usize;
    assert_eq!(
        resolved["original_length"].as_u64().unwrap() as usize,
        original.len()
    );
    let mut record = original[..length.min(original.len())].to_vec();
    record.resize(length, 0);
    for edit in resolved["edits"].as_array().unwrap() {
        let offset = edit["offset"].as_u64().unwrap() as usize;
        let old = unhex(edit["old_bytes"].as_str().unwrap());
        let new = unhex(edit["new_bytes"].as_str().unwrap());
        assert_eq!(
            record[offset..offset + old.len()],
            old[..],
            "stale record edit at {offset}"
        );
        record[offset..offset + new.len()].copy_from_slice(&new);
    }
    assert_eq!(
        hex(&Sha256::digest(&record)),
        resolved["sha256"].as_str().unwrap(),
        "resolved record digest"
    );
    record
}
