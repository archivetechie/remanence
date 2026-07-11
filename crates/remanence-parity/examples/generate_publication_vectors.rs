//! Generate deterministic, byte-pinned REM-PARITY 1.0 publication vectors.
//!
//! The publication packager invokes this example so image artifacts are made
//! by the same bootstrap, sidecar, parity-map, codec, map, and resume logic as
//! the crate's conformance tests. It never writes through a real tape sink.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use remanence_parity::bootstrap::write_bootstrap_block;
use remanence_parity::codec::ReedSolomonCodec;
use remanence_parity::{
    data_shard_crc64, default_scheme, encode_parity_map_tape_file, encode_sidecar_tape_file,
    plan_resume_append_from_committed_prefix, BootstrapPayload, FilemarkMap, FilemarkMapDigest,
    ParityMapPayload, ParityMapReference, ParityScheme, ParitySchemeRecord, SchemeId,
    SidecarDescriptor, SidecarEpochDirectory, SidecarEpochDirectoryEntry, TapeFileMapEntry,
    DEFAULT_SCHEME_BLOCK_SIZE_BYTES, SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH,
    SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD, SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
};
use sha2::{Digest, Sha256};

const BLOCK_SIZE: u32 = 4096;
const TAPE_UUID: [u8; 16] = [0x42; 16];
const WRITTEN_AT: &str = "2026-01-01T00:00:00Z";
const WRITTEN_BY: &str = "remanence-publication-vector-1";

fn small_scheme() -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 2,
        stripes_per_neighborhood: 2,
    }
}

fn partial_scheme() -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 4,
        parity_blocks_per_stripe: 3,
        stripes_per_neighborhood: 5,
    }
}

fn bootstrap_payload(
    scheme: Option<&ParityScheme>,
    digest: Option<FilemarkMapDigest>,
    sequence: u32,
) -> BootstrapPayload {
    let no_parity = scheme.is_none();
    BootstrapPayload {
        scheme: scheme.map(|scheme| ParitySchemeRecord {
            id: scheme.id.as_str().to_string(),
            data_blocks_per_stripe: scheme.data_blocks_per_stripe,
            parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
            stripes_per_neighborhood: scheme.stripes_per_neighborhood,
            no_parity_flag: no_parity,
        }),
        no_parity_flag: no_parity,
        filemark_map_digest: digest,
        tape_uuid: TAPE_UUID,
        written_by_version: WRITTEN_BY.to_string(),
        written_at: WRITTEN_AT.to_string(),
        sequence,
        block_size_bytes: BLOCK_SIZE,
        drive_compression: false,
        sidecar_epoch_directory: None,
        parity_map_reference: None,
        object_rows: Vec::new(),
    }
}

fn patterned_blocks(count: usize, seed: u8) -> Vec<Vec<u8>> {
    (0..count)
        .map(|index| {
            (0..BLOCK_SIZE as usize)
                .map(|offset| {
                    seed.wrapping_add((index as u8).wrapping_mul(0x31))
                        .wrapping_add(offset as u8)
                })
                .collect()
        })
        .collect()
}

fn encode_epoch(
    scheme: &ParityScheme,
    object_blocks: &[Vec<u8>],
    epoch_id: u64,
    ordinal_start: u64,
) -> Result<remanence_parity::EncodedSidecarTapeFile, Box<dyn std::error::Error>> {
    let codec = ReedSolomonCodec::new(scheme)?;
    let mut parity_shards = Vec::new();
    for stripe in 0..scheme.stripes_per_neighborhood as usize {
        let stripe_data = (0..scheme.data_blocks_per_stripe as usize)
            .map(|row| {
                let index = row * scheme.stripes_per_neighborhood as usize + stripe;
                object_blocks
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| vec![0; BLOCK_SIZE as usize])
            })
            .collect::<Vec<_>>();
        parity_shards.extend(codec.encode(&stripe_data)?);
    }
    let ordinal_end = ordinal_start + object_blocks.len() as u64;
    let descriptor = SidecarDescriptor {
        tape_uuid: TAPE_UUID,
        epoch_id,
        k: scheme.data_blocks_per_stripe,
        m: scheme.parity_blocks_per_stripe,
        stripes_per_epoch: scheme.stripes_per_neighborhood,
        block_size: BLOCK_SIZE,
        protected_ordinal_start: ordinal_start,
        protected_ordinal_end_exclusive: ordinal_end,
    };
    Ok(encode_sidecar_tape_file(
        &descriptor,
        &parity_shards,
        object_blocks
            .iter()
            .map(|block| data_shard_crc64(block))
            .collect(),
    )?)
}

fn directory_entry(
    tape_file_number: u32,
    sidecar: &remanence_parity::EncodedSidecarTapeFile,
    final_partial: bool,
) -> SidecarEpochDirectoryEntry {
    let mut flags =
        SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD;
    if final_partial {
        flags |= SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH;
    }
    SidecarEpochDirectoryEntry {
        tape_file_number,
        epoch_id: sidecar.header.epoch_id,
        protected_ordinal_start: sidecar.header.protected_ordinal_start,
        protected_ordinal_end_exclusive: sidecar.header.protected_ordinal_end_exclusive,
        sidecar_total_block_count: sidecar.header.sidecar_total_block_count,
        sidecar_header_block_count: sidecar.header.shard_index_block_count,
        parity_shard_block_count: sidecar.header.parity_block_count,
        canonical_metadata_hash: sidecar.header.canonical_metadata_hash,
        flags,
    }
}

fn write_block(path: &Path, block: &[u8]) -> std::io::Result<()> {
    fs::write(path, block)
}

fn write_blocks(path: &Path, blocks: &[Vec<u8>]) -> std::io::Result<()> {
    fs::write(path, blocks.concat())
}

fn write_bootstrap(
    path: &Path,
    payload: &BootstrapPayload,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut block = vec![0u8; payload.block_size_bytes as usize];
    write_bootstrap_block(payload, &mut block)?;
    write_block(path, &block)?;
    Ok(())
}

fn image_dir(root: &Path, id: &str) -> std::io::Result<PathBuf> {
    let path = root.join("positive").join(id);
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn write_initial_bootstrap(
    path: &Path,
    scheme: &ParityScheme,
) -> Result<(), Box<dyn std::error::Error>> {
    let bot_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])?;
    write_bootstrap(
        path,
        &bootstrap_payload(Some(scheme), Some(bot_map.digest(false)?), 0),
    )
}

fn emit_minimal(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let dir = image_dir(root, "minimal-image")?;
    let scheme = small_scheme();
    let object = patterned_blocks(4, 0);
    let sidecar = encode_epoch(&scheme, &object, 0, 0)?;
    let map = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::parity_sidecar(2, sidecar.blocks.len() as u64, 0, 0, 4),
        TapeFileMapEntry::bootstrap(3, 1),
    ])?;
    let directory = SidecarEpochDirectory {
        directory_scope_tape_file_count: map.tape_file_count(),
        directory_scope_total_data_ordinals: map.total_data_ordinals(),
        directory_scope_highest_protected_ordinal: map.max_sidecar_end_exclusive(),
        is_final_directory: true,
        entries: vec![directory_entry(2, &sidecar, false)],
    };
    write_initial_bootstrap(&dir.join("tape-file-000-bootstrap.bin"), &scheme)?;
    write_blocks(&dir.join("tape-file-001-object.bin"), &object)?;
    write_blocks(&dir.join("tape-file-002-sidecar.bin"), &sidecar.blocks)?;
    let mut final_payload = bootstrap_payload(Some(&scheme), Some(map.digest(true)?), 1);
    final_payload.sidecar_epoch_directory = Some(directory);
    write_bootstrap(
        &dir.join("tape-file-003-final-bootstrap.bin"),
        &final_payload,
    )?;
    fs::write(
        dir.join("expected.json"),
        format!(
            "{{\n  \"vector_id\": \"minimal-image\",\n  \"expected_outcome\": \"scan-and-recover\",\n  \"block_size\": {BLOCK_SIZE},\n  \"k\": 2,\n  \"m\": 2,\n  \"S\": 2,\n  \"map_sha256\": \"{}\",\n  \"sidecar_block_count\": {}\n}}\n",
            hex(&map.canonical_digest()?),
            sidecar.blocks.len()
        ),
    )?;
    Ok(())
}

fn emit_final_partial(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let dir = image_dir(root, "final-partial-epoch")?;
    let scheme = partial_scheme();
    let object = patterned_blocks(7, 0x51);
    let sidecar = encode_epoch(&scheme, &object, 0, 0)?;
    let map = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, object.len() as u64, 0),
        TapeFileMapEntry::parity_sidecar(2, sidecar.blocks.len() as u64, 0, 0, 7),
        TapeFileMapEntry::bootstrap(3, 1),
    ])?;
    let mut payload = bootstrap_payload(Some(&scheme), Some(map.digest(true)?), 1);
    payload.sidecar_epoch_directory = Some(SidecarEpochDirectory {
        directory_scope_tape_file_count: map.tape_file_count(),
        directory_scope_total_data_ordinals: 7,
        directory_scope_highest_protected_ordinal: 7,
        is_final_directory: true,
        entries: vec![directory_entry(2, &sidecar, true)],
    });
    write_initial_bootstrap(&dir.join("tape-file-000-bootstrap.bin"), &scheme)?;
    write_blocks(&dir.join("tape-file-001-object.bin"), &object)?;
    write_blocks(&dir.join("tape-file-002-sidecar.bin"), &sidecar.blocks)?;
    write_bootstrap(&dir.join("tape-file-003-final-bootstrap.bin"), &payload)?;
    fs::write(
        dir.join("expected.json"),
        format!(
            "{{\n  \"vector_id\": \"final-partial-epoch\",\n  \"expected_outcome\": \"recover-with-implicit-zero-shards\",\n  \"real_data_shards\": 7,\n  \"logical_data_shards\": 20,\n  \"recover_ordinal\": 4,\n  \"recovered_block_sha256\": \"{}\",\n  \"recovered_block_hex_prefix\": \"{}\"\n}}\n",
            sha256_hex(&object[4]),
            hex(&object[4][..32]),
        ),
    )?;
    Ok(())
}

fn emit_external_parity_map(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let dir = image_dir(root, "external-parity-map")?;
    let scheme = small_scheme();
    let object = patterned_blocks(4, 0x71);
    let sidecar = encode_epoch(&scheme, &object, 0, 0)?;
    let entry = directory_entry(2, &sidecar, false);
    let preliminary_directory = SidecarEpochDirectory {
        directory_scope_tape_file_count: 5,
        directory_scope_total_data_ordinals: 4,
        directory_scope_highest_protected_ordinal: 4,
        is_final_directory: true,
        entries: vec![entry.clone()],
    };
    let preliminary = encode_parity_map_tape_file(
        &ParityMapPayload {
            tape_uuid: TAPE_UUID,
            sequence: 0,
            directory: preliminary_directory.clone(),
            canonical_map_digest: [0; 32],
            writer_version: Some(WRITTEN_BY.to_string()),
            write_timestamp: Some(WRITTEN_AT.to_string()),
        },
        BLOCK_SIZE,
    )?;
    let map = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::parity_sidecar(2, sidecar.blocks.len() as u64, 0, 0, 4),
        TapeFileMapEntry::parity_map(3, preliminary.blocks.len() as u64),
        TapeFileMapEntry::bootstrap(4, 1),
    ])?;
    let map_digest = map.canonical_digest()?;
    let encoded = encode_parity_map_tape_file(
        &ParityMapPayload {
            tape_uuid: TAPE_UUID,
            sequence: 0,
            directory: preliminary_directory,
            canonical_map_digest: map_digest,
            writer_version: Some(WRITTEN_BY.to_string()),
            write_timestamp: Some(WRITTEN_AT.to_string()),
        },
        BLOCK_SIZE,
    )?;
    if encoded.blocks.len() != preliminary.blocks.len() {
        return Err("parity-map block count changed after digest pinning".into());
    }
    let mut payload = bootstrap_payload(Some(&scheme), Some(map.digest(true)?), 1);
    payload.parity_map_reference = Some(ParityMapReference {
        tape_file_number: 3,
        block_count: encoded.blocks.len() as u64,
        directory_scope_tape_file_count: 5,
        directory_scope_total_data_ordinals: 4,
        directory_scope_highest_protected_ordinal: 4,
        is_final_directory: true,
        parity_map_payload_sha256: encoded.header.payload_sha256,
        canonical_map_digest: map_digest,
    });
    write_initial_bootstrap(&dir.join("tape-file-000-bootstrap.bin"), &scheme)?;
    write_blocks(&dir.join("tape-file-001-object.bin"), &object)?;
    write_blocks(&dir.join("tape-file-002-sidecar.bin"), &sidecar.blocks)?;
    write_blocks(&dir.join("tape-file-003-parity-map.bin"), &encoded.blocks)?;
    write_bootstrap(&dir.join("tape-file-004-final-bootstrap.bin"), &payload)?;
    fs::write(
        dir.join("expected.json"),
        format!(
            "{{\n  \"vector_id\": \"external-parity-map\",\n  \"expected_outcome\": \"external-directory-selected\",\n  \"parity_map_tape_file_number\": 3,\n  \"parity_map_block_count\": {},\n  \"payload_sha256\": \"{}\"\n}}\n",
            encoded.blocks.len(),
            hex(&encoded.header.payload_sha256)
        ),
    )?;
    Ok(())
}

fn emit_no_parity(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let dir = image_dir(root, "no-parity")?;
    write_bootstrap(
        &dir.join("tape-file-000-bootstrap.bin"),
        &bootstrap_payload(None, None, 0),
    )?;
    fs::write(
        dir.join("expected.json"),
        "{\n  \"vector_id\": \"no-parity\",\n  \"expected_outcome\": \"accepted-without-scheme-or-digest\",\n  \"no_parity_flag\": true\n}\n",
    )?;
    Ok(())
}

fn emit_checkpoint(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let dir = image_dir(root, "checkpoint-prefix")?;
    let scheme = small_scheme();
    let object = patterned_blocks(4, 0x91);
    let sidecar = encode_epoch(&scheme, &object, 0, 0)?;
    let map = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::parity_sidecar(2, sidecar.blocks.len() as u64, 0, 0, 4),
        TapeFileMapEntry::bootstrap(3, 1),
    ])?;
    let mut payload = bootstrap_payload(Some(&scheme), Some(map.digest(false)?), 1);
    payload.sidecar_epoch_directory = Some(SidecarEpochDirectory {
        directory_scope_tape_file_count: 4,
        directory_scope_total_data_ordinals: 4,
        directory_scope_highest_protected_ordinal: 4,
        is_final_directory: false,
        entries: vec![directory_entry(2, &sidecar, false)],
    });
    write_initial_bootstrap(&dir.join("tape-file-000-bootstrap.bin"), &scheme)?;
    write_blocks(&dir.join("tape-file-001-object.bin"), &object)?;
    write_blocks(&dir.join("tape-file-002-sidecar.bin"), &sidecar.blocks)?;
    write_bootstrap(
        &dir.join("tape-file-003-checkpoint-bootstrap.bin"),
        &payload,
    )?;
    fs::write(
        dir.join("expected.json"),
        format!(
            "{{\n  \"vector_id\": \"checkpoint-prefix\",\n  \"expected_outcome\": \"prefix-digest-valid\",\n  \"is_final_map\": false,\n  \"tape_file_count\": 4,\n  \"map_sha256\": \"{}\"\n}}\n",
            hex(&map.canonical_digest()?)
        ),
    )?;
    Ok(())
}

fn emit_resume(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let dir = image_dir(root, "resume-round-trip")?;
    let scheme = small_scheme();
    let prefix_object = patterned_blocks(2, 0xA1);
    let appended_object = patterned_blocks(2, 0xC1);
    let committed = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 2, 0),
    ])?;
    let plan = plan_resume_append_from_committed_prefix(&committed, &scheme)?;
    let all_object = [prefix_object.clone(), appended_object.clone()].concat();
    let sidecar = encode_epoch(&scheme, &all_object, 0, 0)?;
    let resumed = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 2, 0),
        TapeFileMapEntry::object(2, 2, 2),
        TapeFileMapEntry::parity_sidecar(3, sidecar.blocks.len() as u64, 0, 0, 4),
        TapeFileMapEntry::bootstrap(4, 1),
    ])?;
    write_initial_bootstrap(&dir.join("committed-tape-file-000-bootstrap.bin"), &scheme)?;
    write_blocks(
        &dir.join("committed-tape-file-001-object.bin"),
        &prefix_object,
    )?;
    write_blocks(
        &dir.join("appended-tape-file-002-object.bin"),
        &appended_object,
    )?;
    write_blocks(
        &dir.join("appended-tape-file-003-sidecar.bin"),
        &sidecar.blocks,
    )?;
    let mut payload = bootstrap_payload(Some(&scheme), Some(resumed.digest(true)?), 2);
    payload.sidecar_epoch_directory = Some(SidecarEpochDirectory {
        directory_scope_tape_file_count: 5,
        directory_scope_total_data_ordinals: 4,
        directory_scope_highest_protected_ordinal: 4,
        is_final_directory: true,
        entries: vec![directory_entry(3, &sidecar, false)],
    });
    write_bootstrap(
        &dir.join("appended-tape-file-004-final-bootstrap.bin"),
        &payload,
    )?;
    fs::write(
        dir.join("expected.json"),
        format!(
            "{{\n  \"vector_id\": \"resume-round-trip\",\n  \"expected_outcome\": \"append-after-committed-prefix\",\n  \"append_after_tape_file_number\": {},\n  \"next_data_ordinal\": {},\n  \"live_epoch_start\": {},\n  \"final_map_sha256\": \"{}\"\n}}\n",
            plan.append_after_tape_file_number,
            plan.next_data_ordinal,
            plan.live_epoch_start,
            hex(&resumed.canonical_digest()?)
        ),
    )?;
    Ok(())
}

fn emit_default_geometry(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let dir = image_dir(root, "default-geometry-header")?;
    let scheme = default_scheme();
    let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])?;
    let mut payload = bootstrap_payload(Some(&scheme), Some(map.digest(false)?), 0);
    payload.block_size_bytes = DEFAULT_SCHEME_BLOCK_SIZE_BYTES;
    write_bootstrap(&dir.join("tape-file-000-bootstrap.bin"), &payload)?;
    fs::write(
        dir.join("expected.json"),
        format!(
            "{{\n  \"vector_id\": \"default-geometry-header\",\n  \"expected_outcome\": \"default-geometry-parses\",\n  \"k\": {},\n  \"m\": {},\n  \"S\": {},\n  \"declared_block_size\": {}\n}}\n",
            scheme.data_blocks_per_stripe,
            scheme.parity_blocks_per_stripe,
            scheme.stripes_per_neighborhood,
            DEFAULT_SCHEME_BLOCK_SIZE_BYTES
        ),
    )?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: generate_publication_vectors OUTPUT_DIRECTORY")?;
    fs::create_dir_all(&output)?;
    small_scheme().validate()?;
    partial_scheme().validate()?;

    emit_minimal(&output)?;
    emit_final_partial(&output)?;
    emit_external_parity_map(&output)?;
    emit_no_parity(&output)?;
    emit_checkpoint(&output)?;
    emit_resume(&output)?;
    emit_default_geometry(&output)?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}
