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
    json!({"image": case["image"], "unreadable_records": records, "removed_filemark_after_tape_file": case["fault"]["removed_filemark_after_tape_file"], "failed_data_addresses": addresses, "hints": case["hints"]})
}
