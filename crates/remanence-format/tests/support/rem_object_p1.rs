//! Shared published P1 construction for the positive and supplement harnesses.

use remanence_format::{
    write_rem_tar_object, MetadataPreservation, RemTarFile, RemTarObjectOptions,
};
use remanence_library::VecBlockSink;

pub fn build_p1_plaintext() -> Vec<u8> {
    let hello = b"hello, rem-object\n";
    let pattern: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
    let files = p1_files(hello, &pattern);
    let mut sink = VecBlockSink::new();
    write_rem_tar_object(&mut sink, &p1_options(), &files).unwrap();
    sink.blocks
        .iter()
        .flat_map(|block| block.iter().copied())
        .collect()
}

pub fn p1_options() -> RemTarObjectOptions {
    let mut options = RemTarObjectOptions::new(
        "00000000-0000-4000-8000-000000000001",
        "rem-object-tv-1",
        "2026-01-01T00:00:00Z",
        "00000000-0000-4000-8000-0000000000ff",
    );
    options.chunk_size = 4096;
    options.metadata_preservation = MetadataPreservation::Minimal;
    options
}

pub fn p1_files<'a>(hello: &'a [u8], pattern: &'a [u8]) -> [RemTarFile<'a>; 2] {
    [
        RemTarFile {
            path: "a/hello.txt",
            file_id: "00000000-0000-4000-8000-000000000010",
            data: hello,
            mtime: None,
            executable: None,
        },
        RemTarFile {
            path: "b/pattern.bin",
            file_id: "00000000-0000-4000-8000-000000000011",
            data: pattern,
            mtime: None,
            executable: None,
        },
    ]
}
