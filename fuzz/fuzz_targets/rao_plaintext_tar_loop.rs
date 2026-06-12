#![no_main]

//! Fuzz target for the RAO 1.0 plaintext tar/pax record loop.
//!
//! Arbitrary input is interpreted as 512-byte object blocks and streamed
//! through the production plaintext reader. Expected format errors are ignored;
//! panics, hangs, and unbounded allocation behavior remain fuzz findings.

use libfuzzer_sys::fuzz_target;
use remanence_format::{stream_rem_tar_object, FormatError, RemTarEntrySink, RemTarStreamEntry};
use remanence_library::VecBlockSource;

const CHUNK_SIZE: usize = 512;
const MAX_BLOCKS: usize = 2048;

struct NoopSink;

impl RemTarEntrySink for NoopSink {
    fn begin_file(&mut self, _entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        Ok(())
    }

    fn write_file_data(&mut self, _bytes: &[u8]) -> Result<(), FormatError> {
        Ok(())
    }

    fn end_file(&mut self, _entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    let usable_len = data.len().min(MAX_BLOCKS * CHUNK_SIZE) / CHUNK_SIZE * CHUNK_SIZE;
    if usable_len == 0 {
        return;
    }
    let blocks: Vec<Vec<u8>> = data[..usable_len]
        .chunks_exact(CHUNK_SIZE)
        .map(Vec::from)
        .collect();
    let block_count = blocks.len() as u64;
    let mut source = VecBlockSource::new(blocks);
    let mut sink = NoopSink;
    let _ = stream_rem_tar_object(&mut source, CHUNK_SIZE, block_count, &mut sink);
});
