#![no_main]

//! Fuzz target for whole RAO object open/verify paths.
//!
//! Inputs beginning with `RAO1` exercise keyless encrypted inspection and
//! keyed open with a fixed test root key. Other inputs exercise the plaintext
//! object stream verifier. If encrypted open succeeds, the decrypted canonical
//! plaintext is also passed through the plaintext verifier.

use libfuzzer_sys::fuzz_target;
use remanence_aead::{inspect_bytes, open_to_vec, RootKey};
use remanence_format::{stream_rem_tar_object, FormatError, RemTarEntrySink, RemTarStreamEntry};
use remanence_library::VecBlockSource;

const MIN_CHUNK_SIZE: usize = 512;
const MAX_INPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_PARSE_CHUNK_SIZE: usize = 1024 * 1024;

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
    let data = &data[..data.len().min(MAX_INPUT_BYTES)];
    if data.starts_with(b"RAO1") {
        let inspected = inspect_bytes(data);
        let root = match RootKey::new([0x11; 32]) {
            Ok(root) => root,
            Err(_) => return,
        };
        if let Ok((plaintext, report)) = open_to_vec(data, &root) {
            let chunk_size = report.header.chunk_size as usize;
            if chunk_size <= MAX_PARSE_CHUNK_SIZE {
                parse_plaintext_blocks(&plaintext, chunk_size);
            }
        }
        let _ = inspected;
    } else {
        parse_plaintext_blocks(data, MIN_CHUNK_SIZE);
    }
});

fn parse_plaintext_blocks(data: &[u8], chunk_size: usize) {
    if chunk_size == 0 || chunk_size % MIN_CHUNK_SIZE != 0 {
        return;
    }
    let usable_len = data.len() / chunk_size * chunk_size;
    if usable_len == 0 {
        return;
    }
    let blocks: Vec<Vec<u8>> = data[..usable_len]
        .chunks_exact(chunk_size)
        .map(Vec::from)
        .collect();
    let block_count = blocks.len() as u64;
    let mut source = VecBlockSource::new(blocks);
    let mut sink = NoopSink;
    let _ = stream_rem_tar_object(&mut source, chunk_size, block_count, &mut sink);
}
