//! Generate the byte-pinned minimal REM-PARITY 1.0 tape-image vector.

use std::env;
use std::fs;
use std::path::PathBuf;

use remanence_parity::bootstrap::write_bootstrap_block;
use remanence_parity::codec::ReedSolomonCodec;
use remanence_parity::{
    data_shard_crc64, encode_sidecar_tape_file, BootstrapPayload, FilemarkMap, ParityScheme,
    ParitySchemeRecord, SchemeId, SidecarDescriptor, SidecarEpochDirectory,
    SidecarEpochDirectoryEntry, TapeFileMapEntry, SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD,
    SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
};

const BLOCK_SIZE: u32 = 4096;
const TAPE_UUID: [u8; 16] = [0x42; 16];

fn bootstrap_payload(
    scheme: &ParityScheme,
    digest: remanence_parity::FilemarkMapDigest,
    sequence: u32,
    directory: Option<SidecarEpochDirectory>,
) -> BootstrapPayload {
    BootstrapPayload {
        scheme: Some(ParitySchemeRecord {
            id: scheme.id.as_str().to_string(),
            data_blocks_per_stripe: scheme.data_blocks_per_stripe,
            parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
            stripes_per_neighborhood: scheme.stripes_per_neighborhood,
            no_parity_flag: false,
        }),
        no_parity_flag: false,
        filemark_map_digest: Some(digest),
        tape_uuid: TAPE_UUID,
        written_by_version: "publication-vector-1".to_string(),
        written_at: "2026-01-01T00:00:00Z".to_string(),
        sequence,
        block_size_bytes: BLOCK_SIZE,
        drive_compression: false,
        sidecar_epoch_directory: directory,
        parity_map_reference: None,
        object_rows: Vec::new(),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: generate_publication_vectors OUTPUT_DIRECTORY")?;
    fs::create_dir_all(&output)?;

    let scheme = ParityScheme {
        id: SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 2,
        stripes_per_neighborhood: 2,
    };
    scheme.validate()?;
    let object_blocks = (0u8..4)
        .map(|index| {
            (0..BLOCK_SIZE as usize)
                .map(|offset| index.wrapping_mul(0x31).wrapping_add(offset as u8))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let codec = ReedSolomonCodec::new(&scheme)?;
    let mut parity_shards = Vec::new();
    for stripe in 0..scheme.stripes_per_neighborhood as usize {
        let stripe_data = (0..scheme.data_blocks_per_stripe as usize)
            .map(|row| {
                object_blocks[row * scheme.stripes_per_neighborhood as usize + stripe].clone()
            })
            .collect::<Vec<_>>();
        parity_shards.extend(codec.encode(&stripe_data)?);
    }
    let descriptor = SidecarDescriptor {
        tape_uuid: TAPE_UUID,
        epoch_id: 0,
        k: scheme.data_blocks_per_stripe,
        m: scheme.parity_blocks_per_stripe,
        stripes_per_epoch: scheme.stripes_per_neighborhood,
        block_size: BLOCK_SIZE,
        protected_ordinal_start: 0,
        protected_ordinal_end_exclusive: 4,
    };
    let sidecar = encode_sidecar_tape_file(
        &descriptor,
        &parity_shards,
        object_blocks
            .iter()
            .map(|block| data_shard_crc64(block))
            .collect(),
    )?;
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
        entries: vec![SidecarEpochDirectoryEntry {
            tape_file_number: 2,
            epoch_id: 0,
            protected_ordinal_start: 0,
            protected_ordinal_end_exclusive: 4,
            sidecar_total_block_count: sidecar.header.sidecar_total_block_count,
            sidecar_header_block_count: sidecar.header.shard_index_block_count,
            parity_shard_block_count: sidecar.header.parity_block_count,
            canonical_metadata_hash: sidecar.header.canonical_metadata_hash,
            flags: SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD
                | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
        }],
    };

    let bot_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])?;
    let mut bot = vec![0u8; BLOCK_SIZE as usize];
    write_bootstrap_block(
        &bootstrap_payload(&scheme, bot_map.digest(false)?, 0, None),
        &mut bot,
    )?;
    let mut final_bootstrap = vec![0u8; BLOCK_SIZE as usize];
    write_bootstrap_block(
        &bootstrap_payload(&scheme, map.digest(true)?, 1, Some(directory)),
        &mut final_bootstrap,
    )?;

    fs::write(output.join("tape-file-000-bootstrap.bin"), bot)?;
    fs::write(
        output.join("tape-file-001-object.bin"),
        object_blocks.concat(),
    )?;
    fs::write(
        output.join("tape-file-002-sidecar.bin"),
        sidecar.blocks.concat(),
    )?;
    fs::write(
        output.join("tape-file-003-final-bootstrap.bin"),
        final_bootstrap,
    )?;
    fs::write(
        output.join("image.json"),
        format!(
            "{{\n  \"vector_id\": \"REM-PARITY-TV-MINIMAL\",\n  \"block_size\": {BLOCK_SIZE},\n  \"tape_uuid_hex\": \"{}\",\n  \"k\": 2,\n  \"m\": 2,\n  \"S\": 2,\n  \"sidecar_block_count\": {},\n  \"map_sha256\": \"{}\"\n}}\n",
            hex(&TAPE_UUID),
            sidecar.blocks.len(),
            hex(&map.canonical_digest()?),
        ),
    )?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
