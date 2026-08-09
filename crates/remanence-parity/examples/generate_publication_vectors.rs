//! Generate the schema-major 2 sole-BOT Bootstrap publication fixture.
//!
//! Frozen publication archives remain immutable. This helper emits only the
//! current clean-break Bootstrap surface and intentionally does not recreate
//! removed intermediate/final Bootstrap extensions.

use std::env;
use std::fs;
use std::path::PathBuf;

use remanence_parity::bootstrap::write_bootstrap_block;
use remanence_parity::{
    default_scheme, BootstrapPayload, FilemarkMap, ParitySchemeRecord, TapeFileMapEntry,
    DEFAULT_SCHEME_BLOCK_SIZE_BYTES,
};

const TAPE_UUID: [u8; 16] = [0x42; 16];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: generate_publication_vectors OUTPUT_DIRECTORY")?;
    fs::create_dir_all(&output)?;

    let scheme = default_scheme();
    let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])?;
    let payload = BootstrapPayload {
        scheme: Some(ParitySchemeRecord {
            id: scheme.id.as_str().to_string(),
            data_blocks_per_stripe: scheme.data_blocks_per_stripe,
            parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
            stripes_per_neighborhood: scheme.stripes_per_neighborhood,
            no_parity_flag: false,
        }),
        no_parity_flag: false,
        filemark_map_digest: Some(map.digest(false)?),
        tape_uuid: TAPE_UUID,
        written_by_version: "remanence-publication-vector-current".to_string(),
        written_at: "2026-08-09T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: DEFAULT_SCHEME_BLOCK_SIZE_BYTES,
        drive_compression: false,
    };
    let mut block = vec![0; DEFAULT_SCHEME_BLOCK_SIZE_BYTES as usize];
    write_bootstrap_block(&payload, &mut block)?;
    fs::write(output.join("bot-bootstrap.bin"), block)?;
    Ok(())
}
