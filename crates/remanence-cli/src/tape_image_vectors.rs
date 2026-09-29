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
    // one authority the executor supplies, which is none.
    for key in case.as_object().expect("case object").keys() {
        assert!(
            matches!(
                key.as_str(),
                "id" | "image"
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
            ),
            "{}: unknown case key {key}",
            case["id"]
        );
    }
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
            "both ParityMap copies; epoch 0 sidecar primary header and footer"
        ), "unrecognized fault description: {description}");
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
    let records: Vec<_> = lbas.iter().map(|lba| {
        let (f, file) = image.files.iter().enumerate().rev().find(|(_, f)| f.start_record as u64 <= *lba).expect("fault within image");
        assert!(*lba < image.eod_record as u64);
        json!({"lba": lba, "tape_file": f, "record_index": lba - file.start_record as u64, "filemark": file.filemark_record == Some(*lba as usize)})
    }).collect();
    let mut map = json!({"image": case["image"], "unreadable_records": records, "removed_filemark_after_tape_file": case["fault"]["removed_filemark_after_tape_file"], "failed_data_addresses": addresses});
    if let Some(faults) = case["fault"].get("record_faults") {
        assert!(faults.is_array(), "{}: record_faults is a list", case["id"]);
        map["record_edits"] = record_edits(case, image);
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
fn record_edits(case: &Value, image: &ExportedTapeImage) -> Value {
    const KEYS: [&str; 7] = [
        "tape_file",
        "record_index",
        "length",
        "byte_edits",
        "payload",
        "header_crc",
        "payload_crc",
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
