//! Verifies the RAO Section 13 fixture manifests against regenerated objects.

use remanence_aead::{derive_keys, inspect_bytes, open_to_vec, RootKey};
use remanence_format::{
    write_encrypted_rao_object, write_rem_tar_object, write_rem_tar_object_from_readers,
    MetadataPreservation, RemTarEntryType, RemTarFile, RemTarFileSpec, RemTarFileStream,
    RemTarObjectOptions,
};
use remanence_library::VecBlockSink;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Cursor;

const KEY_ID: [u8; 16] = *b"KID:rao-tv-e1.01";
const ROOT_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

fn fixture(json: &str) -> Value {
    serde_json::from_str(json).expect("fixture manifest is valid JSON")
}

fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    value
        .get(key)
        .unwrap_or_else(|| panic!("fixture field {key:?} exists"))
}

fn item(value: &Value, index: usize) -> &Value {
    value
        .as_array()
        .and_then(|items| items.get(index))
        .unwrap_or_else(|| panic!("fixture array item {index} exists"))
}

fn str_field(value: &Value, key: &str) -> String {
    field(value, key)
        .as_str()
        .unwrap_or_else(|| panic!("fixture field {key:?} is a string"))
        .to_string()
}

fn u64_field(value: &Value, key: &str) -> u64 {
    field(value, key)
        .as_u64()
        .unwrap_or_else(|| panic!("fixture field {key:?} is an unsigned integer"))
}

fn opt_u64_field(value: &Value, key: &str) -> Option<u64> {
    let value = field(value, key);
    if value.is_null() {
        None
    } else {
        Some(
            value
                .as_u64()
                .unwrap_or_else(|| panic!("fixture field {key:?} is null or unsigned integer")),
        )
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn flatten_sink(sink: &VecBlockSink) -> Vec<u8> {
    sink.blocks.iter().flatten().copied().collect()
}

fn root_key() -> RootKey {
    RootKey::new((0u8..=31).collect::<Vec<_>>()).expect("root key has 32 bytes")
}

fn p1_options() -> RemTarObjectOptions {
    let mut options = RemTarObjectOptions::new(
        "00000000-0000-4000-8000-000000000001",
        "rao-tv-1",
        "2026-01-01T00:00:00Z",
        "00000000-0000-4000-8000-0000000000ff",
    );
    options.chunk_size = 4096;
    options.metadata_preservation = MetadataPreservation::Minimal;
    options
}

fn d1_options() -> RemTarObjectOptions {
    let mut options = RemTarObjectOptions::new(
        "00000000-0000-4000-8000-000000000002",
        "rao-tv-d1",
        "2026-01-01T00:00:00Z",
        "00000000-0000-4000-8000-0000000000fe",
    );
    options.metadata_preservation = MetadataPreservation::Minimal;
    options
}

fn vector_options(
    suffix: u64,
    caller_object_id: &str,
    manifest_suffix: &str,
) -> RemTarObjectOptions {
    let mut options = RemTarObjectOptions::new(
        format!("00000000-0000-4000-8000-{suffix:012}"),
        caller_object_id,
        "2026-01-01T00:00:00Z",
        format!("00000000-0000-4000-8000-{manifest_suffix}"),
    );
    options.chunk_size = 4096;
    options.metadata_preservation = MetadataPreservation::Minimal;
    options
}

fn bytes_mod(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| seed.wrapping_add(index as u8))
        .collect()
}

#[derive(Debug, Clone)]
struct TestEntry {
    entry_type: RemTarEntryType,
    path: String,
    file_id: String,
    data: Vec<u8>,
    link_target: Option<String>,
    mtime: Option<String>,
    executable: Option<bool>,
}

impl TestEntry {
    fn regular(
        path: impl Into<String>,
        file_id: impl Into<String>,
        data: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            entry_type: RemTarEntryType::Regular,
            path: path.into(),
            file_id: file_id.into(),
            data: data.into(),
            link_target: None,
            mtime: None,
            executable: None,
        }
    }

    fn symlink(
        path: impl Into<String>,
        file_id: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        Self {
            entry_type: RemTarEntryType::Symlink,
            path: path.into(),
            file_id: file_id.into(),
            data: Vec::new(),
            link_target: Some(target.into()),
            mtime: None,
            executable: None,
        }
    }

    fn directory(path: impl Into<String>, file_id: impl Into<String>) -> Self {
        Self {
            entry_type: RemTarEntryType::Directory,
            path: path.into(),
            file_id: file_id.into(),
            data: Vec::new(),
            link_target: None,
            mtime: None,
            executable: None,
        }
    }

    fn with_mtime(mut self, mtime: impl Into<String>) -> Self {
        self.mtime = Some(mtime.into());
        self
    }

    fn with_executable(mut self, executable: bool) -> Self {
        self.executable = Some(executable);
        self
    }

    fn spec(&self) -> RemTarFileSpec {
        let mut spec = match self.entry_type {
            RemTarEntryType::Regular => RemTarFileSpec::new(
                &self.path,
                &self.file_id,
                self.data.len() as u64,
                Sha256::digest(&self.data).into(),
            ),
            RemTarEntryType::Symlink => RemTarFileSpec::symlink(
                &self.path,
                &self.file_id,
                self.link_target
                    .as_deref()
                    .expect("symlink test entry has a target"),
            ),
            RemTarEntryType::Directory => RemTarFileSpec::directory(&self.path, &self.file_id),
        };
        spec.mtime.clone_from(&self.mtime);
        spec.executable = self.executable;
        spec
    }
}

fn p1_files<'a>(hello: &'a [u8], pattern: &'a [u8]) -> [RemTarFile<'a>; 2] {
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

fn assert_layout(actual: &remanence_format::RemTarFileLayout, expected: &Value) {
    assert_eq!(actual.path, str_field(expected, "path"));
    assert_eq!(
        actual.pax_header_offset,
        u64_field(expected, "pax_header_offset")
    );
    assert_eq!(actual.data_offset, u64_field(expected, "data_offset"));
    assert_eq!(
        actual.first_chunk_lba.map(|lba| lba.0),
        Some(u64_field(expected, "first_chunk_lba"))
    );
    assert_eq!(actual.chunk_count, u64_field(expected, "chunk_count"));
    assert_eq!(actual.pad_spaces as u64, u64_field(expected, "pad_spaces"));
}

fn entry_type_name(entry_type: RemTarEntryType) -> &'static str {
    match entry_type {
        RemTarEntryType::Regular => "regular",
        RemTarEntryType::Symlink => "symlink",
        RemTarEntryType::Directory => "directory",
    }
}

fn entry_type_from_fixture(value: &Value) -> RemTarEntryType {
    match value.as_str().expect("entry type is a string") {
        "regular" => RemTarEntryType::Regular,
        "symlink" => RemTarEntryType::Symlink,
        "directory" => RemTarEntryType::Directory,
        other => panic!("unexpected fixture entry type {other:?}"),
    }
}

fn assert_layout_any(actual: &remanence_format::RemTarFileLayout, expected: &Value) {
    assert_eq!(
        actual.entry_type,
        entry_type_from_fixture(field(expected, "entry_type"))
    );
    assert_eq!(actual.path, str_field(expected, "path"));
    assert_eq!(
        actual.pax_header_offset,
        u64_field(expected, "pax_header_offset")
    );
    assert_eq!(actual.data_offset, u64_field(expected, "data_offset"));
    assert_eq!(
        actual.first_chunk_lba.map(|lba| lba.0),
        opt_u64_field(expected, "first_chunk_lba")
    );
    assert_eq!(actual.chunk_count, u64_field(expected, "chunk_count"));
    assert_eq!(actual.pad_spaces as u64, u64_field(expected, "pad_spaces"));
}

fn assert_manifest_layout(actual: &remanence_format::RemTarFileLayout, expected: &Value) {
    assert_eq!(
        actual.pax_header_offset,
        u64_field(expected, "pax_header_offset")
    );
    assert_eq!(actual.data_offset, u64_field(expected, "data_offset"));
    assert_eq!(
        actual.first_chunk_lba.map(|lba| lba.0),
        Some(u64_field(expected, "first_chunk_lba"))
    );
    assert_eq!(actual.chunk_count, u64_field(expected, "chunk_count"));
    assert_eq!(actual.pad_spaces as u64, u64_field(expected, "pad_spaces"));
}

fn assert_json_opt_string(actual: Option<&str>, value: &Value, label: &str) {
    if value.is_null() {
        assert_eq!(actual, None, "{label}");
    } else {
        assert_eq!(
            actual,
            Some(
                value
                    .as_str()
                    .unwrap_or_else(|| panic!("{label} is a string"))
            ),
            "{label}"
        );
    }
}

fn assert_json_opt_bool(actual: Option<bool>, value: &Value, label: &str) {
    if value.is_null() {
        assert_eq!(actual, None, "{label}");
    } else {
        assert_eq!(
            actual,
            Some(
                value
                    .as_bool()
                    .unwrap_or_else(|| panic!("{label} is a bool"))
            ),
            "{label}"
        );
    }
}

fn write_test_object(
    options: &RemTarObjectOptions,
    entries: &[TestEntry],
) -> (remanence_format::RemTarObjectLayout, Vec<u8>) {
    let specs: Vec<RemTarFileSpec> = entries.iter().map(TestEntry::spec).collect();
    let mut readers: Vec<Cursor<&[u8]>> = entries
        .iter()
        .map(|entry| Cursor::new(entry.data.as_slice()))
        .collect();
    let mut streams: Vec<RemTarFileStream<'_>> = specs
        .into_iter()
        .zip(readers.iter_mut())
        .map(|(spec, reader)| RemTarFileStream::new(spec, reader))
        .collect();
    let mut sink = VecBlockSink::new();
    let layout = write_rem_tar_object_from_readers(&mut sink, options, &mut streams).unwrap();
    (layout, flatten_sink(&sink))
}

fn assert_plaintext_vector_fixture(
    fixture_json: &str,
    vector_id: &str,
    options: RemTarObjectOptions,
    entries: Vec<TestEntry>,
) {
    let fixture = fixture(fixture_json);
    let inputs = field(&fixture, "inputs");
    let expected = field(&fixture, "expected");

    assert_eq!(str_field(&fixture, "vector_id"), vector_id);
    assert_eq!(options.chunk_size as u64, u64_field(inputs, "chunk_size"));
    assert_eq!(options.object_id.as_str(), str_field(inputs, "object_id"));
    assert_eq!(
        options.caller_object_id.as_str(),
        str_field(inputs, "caller_object_id")
    );
    assert_eq!(
        options.write_timestamp.as_str(),
        str_field(inputs, "write_timestamp")
    );
    assert_eq!(str_field(inputs, "metadata_preservation"), "minimal");
    assert_eq!(
        options.manifest_file_id.as_str(),
        str_field(inputs, "manifest_file_id")
    );

    let input_entries = field(inputs, "entries")
        .as_array()
        .expect("fixture entries are an array");
    assert_eq!(entries.len(), input_entries.len());
    for (entry, expected_entry) in entries.iter().zip(input_entries) {
        assert_eq!(
            entry_type_name(entry.entry_type),
            str_field(expected_entry, "entry_type")
        );
        assert_eq!(entry.path, str_field(expected_entry, "path"));
        assert_eq!(entry.file_id, str_field(expected_entry, "file_id"));
        assert_eq!(
            entry.data.len() as u64,
            u64_field(expected_entry, "size_bytes")
        );
        assert_json_opt_string(
            entry.link_target.as_deref(),
            field(expected_entry, "link_target"),
            "link_target",
        );
        assert_json_opt_string(
            entry.mtime.as_deref(),
            field(expected_entry, "mtime"),
            "mtime",
        );
        assert_json_opt_bool(
            entry.executable,
            field(expected_entry, "executable"),
            "executable",
        );
    }

    let (layout, bytes) = write_test_object(&options, &entries);
    assert_eq!(
        layout.total_size_bytes,
        u64_field(expected, "stored_size_bytes")
    );
    assert_eq!(
        layout.projected_size_blocks,
        u64_field(expected, "stored_size_blocks")
    );
    assert_eq!(sha256_hex(&bytes), str_field(expected, "stored_digest"));
    assert_eq!(
        sha256_hex(&bytes[..options.chunk_size]),
        str_field(expected, "first_block_sha256")
    );
    assert_eq!(
        layout.manifest_cbor.len() as u64,
        u64_field(expected, "manifest_cbor_len")
    );
    assert_eq!(
        hex(&layout.manifest_cbor),
        str_field(expected, "manifest_cbor_hex")
    );
    assert_eq!(
        hex(&layout.manifest_sha256),
        str_field(expected, "manifest_sha256")
    );

    let regular_entries: Vec<&TestEntry> = entries
        .iter()
        .filter(|entry| entry.entry_type == RemTarEntryType::Regular)
        .collect();
    let payloads = field(expected, "file_payloads")
        .as_array()
        .expect("file payloads are an array");
    assert_eq!(regular_entries.len(), payloads.len());
    for (entry, payload) in regular_entries.iter().zip(payloads) {
        assert_eq!(entry.path, str_field(payload, "path"));
        assert_eq!(entry.data.len() as u64, u64_field(payload, "size_bytes"));
        assert_eq!(sha256_hex(&entry.data), str_field(payload, "sha256"));
    }

    let symlink_entries: Vec<&TestEntry> = entries
        .iter()
        .filter(|entry| entry.entry_type == RemTarEntryType::Symlink)
        .collect();
    let symlinks = field(expected, "symlinks")
        .as_array()
        .expect("symlinks are an array");
    assert_eq!(symlink_entries.len(), symlinks.len());
    for (entry, symlink) in symlink_entries.iter().zip(symlinks) {
        assert_eq!(entry.path, str_field(symlink, "path"));
        assert_eq!(
            entry.link_target.as_deref(),
            Some(str_field(symlink, "target").as_str())
        );
    }

    let directory_entries: Vec<&TestEntry> = entries
        .iter()
        .filter(|entry| entry.entry_type == RemTarEntryType::Directory)
        .collect();
    let directories = field(expected, "directories")
        .as_array()
        .expect("directories are an array");
    assert_eq!(directory_entries.len(), directories.len());
    for (entry, directory) in directory_entries.iter().zip(directories) {
        assert_eq!(
            entry.path,
            directory
                .as_str()
                .expect("directory fixture value is a string")
        );
    }

    let file_layouts = field(expected, "file_layouts")
        .as_array()
        .expect("file layouts are an array");
    assert_eq!(layout.files.len(), file_layouts.len());
    for (actual, expected_layout) in layout.files.iter().zip(file_layouts) {
        assert_layout_any(actual, expected_layout);
    }
    assert_manifest_layout(&layout.manifest, field(expected, "manifest_layout"));
}

#[test]
fn rao_tv_p1_matches_fixture_manifest() {
    let fixture = fixture(include_str!("../../../fixtures/rao/rao-tv-p1.json"));
    let inputs = field(&fixture, "inputs");
    let expected = field(&fixture, "expected");
    let hello = b"hello, rem archive object\n";
    let pattern: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
    let files = p1_files(hello, &pattern);
    let options = p1_options();

    assert_eq!(options.chunk_size as u64, u64_field(inputs, "chunk_size"));
    assert_eq!(options.object_id.as_str(), str_field(inputs, "object_id"));
    assert_eq!(
        options.caller_object_id.as_str(),
        str_field(inputs, "caller_object_id")
    );
    assert_eq!(
        options.write_timestamp.as_str(),
        str_field(inputs, "write_timestamp")
    );
    assert_eq!(str_field(inputs, "metadata_preservation"), "minimal");
    assert_eq!(
        options.manifest_file_id.as_str(),
        str_field(inputs, "manifest_file_id")
    );

    assert_eq!(
        sha256_hex(hello),
        str_field(item(field(expected, "file_payloads"), 0), "sha256")
    );
    assert_eq!(
        sha256_hex(&pattern),
        str_field(item(field(expected, "file_payloads"), 1), "sha256")
    );

    let mut sink = VecBlockSink::new();
    let layout = write_rem_tar_object(&mut sink, &options, &files).unwrap();
    let bytes = flatten_sink(&sink);

    assert_eq!(
        layout.total_size_bytes,
        u64_field(expected, "stored_size_bytes")
    );
    assert_eq!(
        layout.projected_size_blocks,
        u64_field(expected, "stored_size_blocks")
    );
    assert_eq!(sha256_hex(&bytes), str_field(expected, "stored_digest"));
    assert_eq!(
        sha256_hex(&bytes[..4096]),
        str_field(expected, "first_block_sha256")
    );
    assert_eq!(
        layout.manifest_cbor.len() as u64,
        u64_field(expected, "manifest_cbor_len")
    );
    assert_eq!(
        hex(&layout.manifest_cbor),
        str_field(expected, "manifest_cbor_hex")
    );
    assert_eq!(
        hex(&layout.manifest_sha256),
        str_field(expected, "manifest_sha256")
    );

    let file_layouts = field(expected, "file_layouts");
    assert_layout(&layout.files[0], item(file_layouts, 0));
    assert_layout(&layout.files[1], item(file_layouts, 1));
    assert_manifest_layout(&layout.manifest, field(expected, "manifest_layout"));
}

#[test]
fn rao_tv_e1_matches_fixture_manifest() {
    let p1_fixture = fixture(include_str!("../../../fixtures/rao/rao-tv-p1.json"));
    let e1_fixture = fixture(include_str!("../../../fixtures/rao/rao-tv-e1.json"));
    let inputs = field(&e1_fixture, "inputs");
    let expected = field(&e1_fixture, "expected");
    let hello = b"hello, rem archive object\n";
    let pattern: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
    let files = p1_files(hello, &pattern);
    let key = root_key();

    assert_eq!(str_field(inputs, "plaintext_vector"), "RAO-TV-P1");
    assert_eq!(str_field(inputs, "root_key"), ROOT_KEY_HEX);
    assert_eq!(str_field(inputs, "key_id"), hex(&KEY_ID));

    let mut sink = VecBlockSink::new();
    let report =
        write_encrypted_rao_object(&mut sink, &p1_options(), &files, &key, KEY_ID).unwrap();
    let bytes = flatten_sink(&sink);
    let inspect = inspect_bytes(&bytes).unwrap();
    let (_plaintext, open) = open_to_vec(&bytes, &key).unwrap();
    let header_bytes = open.header.serialize().unwrap();
    let header_hash = open.header.header_hash().unwrap();
    let keys = derive_keys(&key, &open.header.hkdf_salt, &header_hash).unwrap();
    let metadata_start = 128usize;
    let metadata_end = metadata_start + open.header.metadata_frame_len as usize;
    let footer_offset = inspect.footer_offset as usize;

    assert_eq!(
        inspect.plaintext_size,
        u64_field(expected, "plaintext_size")
    );
    assert_eq!(inspect.chunk_count, u64_field(expected, "chunk_count"));
    assert_eq!(
        report.envelope.metadata_plaintext_len,
        u64_field(expected, "metadata_plaintext_len")
    );
    assert_eq!(
        report.envelope.metadata_frame_len,
        u64_field(expected, "metadata_frame_len")
    );
    assert_eq!(
        metadata_end as u64,
        u64_field(expected, "payload_frame_start")
    );
    assert_eq!(
        footer_offset as u64 - 1,
        u64_field(expected, "payload_frame_end_inclusive")
    );
    assert_eq!(inspect.footer_offset, u64_field(expected, "footer_offset"));
    assert_eq!(
        inspect.stored_size_bytes,
        u64_field(expected, "stored_size_bytes")
    );
    assert_eq!(
        report.envelope.stored_size_blocks,
        u64_field(expected, "stored_size_blocks")
    );
    assert_eq!(
        hex(&open.header.hkdf_salt),
        str_field(expected, "hkdf_salt")
    );
    assert_eq!(hex(&header_bytes), str_field(expected, "header_hex"));
    assert_eq!(hex(&header_hash), str_field(expected, "header_hash"));
    assert_eq!(hex(&keys.metadata_key), str_field(expected, "metadata_key"));
    assert_eq!(hex(&keys.payload_key), str_field(expected, "payload_key"));
    assert_eq!(
        hex(&bytes[metadata_start..metadata_end]),
        str_field(expected, "metadata_frame_hex")
    );
    assert_eq!(
        sha256_hex(&bytes[metadata_end..footer_offset]),
        str_field(expected, "payload_frame_sha256")
    );
    assert_eq!(
        hex(&inspect.stored_digest),
        str_field(expected, "stored_digest")
    );
    assert_eq!(
        hex(&open.metadata.plaintext_digest),
        str_field(expected, "plaintext_digest")
    );
    assert_eq!(
        str_field(field(&p1_fixture, "expected"), "stored_digest"),
        str_field(expected, "plaintext_digest")
    );
}

#[test]
fn rao_tv_d1_matches_fixture_manifest() {
    let fixture = fixture(include_str!("../../../fixtures/rao/rao-tv-d1.json"));
    let inputs = field(&fixture, "inputs");
    let expected = field(&fixture, "expected");
    let plaintext = field(expected, "plaintext");
    let encrypted = field(expected, "encrypted");
    let payload: Vec<u8> = (0..262145).map(|i| (i % 256) as u8).collect();
    let files = [RemTarFile {
        path: "v.bin",
        file_id: "00000000-0000-4000-8000-000000000012",
        data: &payload,
        mtime: None,
        executable: None,
    }];
    let options = d1_options();

    assert_eq!(options.chunk_size as u64, u64_field(inputs, "chunk_size"));
    assert_eq!(options.object_id.as_str(), str_field(inputs, "object_id"));
    assert_eq!(
        options.caller_object_id.as_str(),
        str_field(inputs, "caller_object_id")
    );
    assert_eq!(
        options.write_timestamp.as_str(),
        str_field(inputs, "write_timestamp")
    );
    assert_eq!(str_field(inputs, "metadata_preservation"), "minimal");
    assert_eq!(
        options.manifest_file_id.as_str(),
        str_field(inputs, "manifest_file_id")
    );
    assert_eq!(str_field(inputs, "encrypted_root_key"), ROOT_KEY_HEX);
    assert_eq!(str_field(inputs, "encrypted_key_id"), hex(&KEY_ID));

    assert_eq!(
        sha256_hex(&payload),
        str_field(field(plaintext, "file_payload"), "sha256")
    );

    let mut sink = VecBlockSink::new();
    let layout = write_rem_tar_object(&mut sink, &options, &files).unwrap();
    let bytes = flatten_sink(&sink);

    assert_eq!(layout.chunk_size as u64, u64_field(plaintext, "chunk_size"));
    assert_eq!(
        layout.total_size_bytes,
        u64_field(plaintext, "stored_size_bytes")
    );
    assert_eq!(
        layout.projected_size_blocks,
        u64_field(plaintext, "stored_size_blocks")
    );
    assert_eq!(sha256_hex(&bytes), str_field(plaintext, "stored_digest"));
    assert_eq!(
        sha256_hex(&bytes[..layout.chunk_size]),
        str_field(plaintext, "first_block_sha256")
    );
    assert_eq!(
        layout.manifest_cbor.len() as u64,
        u64_field(plaintext, "manifest_cbor_len")
    );
    assert_eq!(
        hex(&layout.manifest_cbor),
        str_field(plaintext, "manifest_cbor_hex")
    );
    assert_eq!(
        hex(&layout.manifest_sha256),
        str_field(plaintext, "manifest_sha256")
    );
    assert_layout(&layout.files[0], field(plaintext, "file_layout"));
    assert_manifest_layout(&layout.manifest, field(plaintext, "manifest_layout"));

    let key = root_key();
    let mut encrypted_sink = VecBlockSink::new();
    let report =
        write_encrypted_rao_object(&mut encrypted_sink, &options, &files, &key, KEY_ID).unwrap();
    let encrypted_bytes = flatten_sink(&encrypted_sink);
    let inspect = inspect_bytes(&encrypted_bytes).unwrap();
    let (_opened, open) = open_to_vec(&encrypted_bytes, &key).unwrap();
    let header_bytes = open.header.serialize().unwrap();
    let header_hash = open.header.header_hash().unwrap();
    let keys = derive_keys(&key, &open.header.hkdf_salt, &header_hash).unwrap();
    let metadata_start = 128usize;
    let metadata_end = metadata_start + open.header.metadata_frame_len as usize;
    let footer_offset = inspect.footer_offset as usize;

    assert_eq!(
        inspect.plaintext_size,
        u64_field(encrypted, "plaintext_size")
    );
    assert_eq!(inspect.chunk_count, u64_field(encrypted, "chunk_count"));
    assert_eq!(
        report.envelope.metadata_plaintext_len,
        u64_field(encrypted, "metadata_plaintext_len")
    );
    assert_eq!(
        report.envelope.metadata_frame_len,
        u64_field(encrypted, "metadata_frame_len")
    );
    assert_eq!(
        metadata_end as u64,
        u64_field(encrypted, "payload_frame_start")
    );
    assert_eq!(
        footer_offset as u64 - 1,
        u64_field(encrypted, "payload_frame_end_inclusive")
    );
    assert_eq!(inspect.footer_offset, u64_field(encrypted, "footer_offset"));
    assert_eq!(
        inspect.stored_size_bytes,
        u64_field(encrypted, "stored_size_bytes")
    );
    assert_eq!(
        report.envelope.stored_size_blocks,
        u64_field(encrypted, "stored_size_blocks")
    );
    assert_eq!(
        hex(&open.header.hkdf_salt),
        str_field(encrypted, "hkdf_salt")
    );
    assert_eq!(hex(&header_bytes), str_field(encrypted, "header_hex"));
    assert_eq!(hex(&header_hash), str_field(encrypted, "header_hash"));
    assert_eq!(
        hex(&keys.metadata_key),
        str_field(encrypted, "metadata_key")
    );
    assert_eq!(hex(&keys.payload_key), str_field(encrypted, "payload_key"));
    assert_eq!(
        hex(&encrypted_bytes[metadata_start..metadata_end]),
        str_field(encrypted, "metadata_frame_hex")
    );
    assert_eq!(
        sha256_hex(&encrypted_bytes[metadata_end..footer_offset]),
        str_field(encrypted, "payload_frame_sha256")
    );
    assert_eq!(
        hex(&inspect.stored_digest),
        str_field(encrypted, "stored_digest")
    );
    assert_eq!(
        hex(&open.metadata.plaintext_digest),
        str_field(encrypted, "plaintext_digest")
    );
    assert_eq!(
        str_field(plaintext, "stored_digest"),
        str_field(encrypted, "plaintext_digest")
    );
}

#[test]
fn rao_tv_empty_matches_fixture_manifest() {
    assert_plaintext_vector_fixture(
        include_str!("../../../fixtures/rao/rao-tv-empty.json"),
        "RAO-TV-EMPTY",
        vector_options(101, "rao-tv-empty", "000000000101"),
        Vec::new(),
    );
}

#[test]
fn rao_tv_empty_file_matches_fixture_manifest() {
    assert_plaintext_vector_fixture(
        include_str!("../../../fixtures/rao/rao-tv-empty-file.json"),
        "RAO-TV-EMPTY-FILE",
        vector_options(102, "rao-tv-empty-file", "000000000102"),
        vec![TestEntry::regular(
            "empty.bin",
            "00000000-0000-4000-8000-000000000120",
            Vec::new(),
        )],
    );
}

#[test]
fn rao_tv_one_byte_matches_fixture_manifest() {
    assert_plaintext_vector_fixture(
        include_str!("../../../fixtures/rao/rao-tv-one-byte.json"),
        "RAO-TV-ONE-BYTE",
        vector_options(103, "rao-tv-one-byte", "000000000103"),
        vec![TestEntry::regular(
            "one.bin",
            "00000000-0000-4000-8000-000000000130",
            vec![0x7f],
        )],
    );
}

#[test]
fn rao_tv_boundary_matches_fixture_manifest() {
    assert_plaintext_vector_fixture(
        include_str!("../../../fixtures/rao/rao-tv-boundary.json"),
        "RAO-TV-BOUNDARY",
        vector_options(104, "rao-tv-boundary", "000000000104"),
        vec![
            TestEntry::regular(
                "boundary/c-minus-1.bin",
                "00000000-0000-4000-8000-000000000141",
                bytes_mod(4095, 1),
            ),
            TestEntry::regular(
                "boundary/c.bin",
                "00000000-0000-4000-8000-000000000142",
                bytes_mod(4096, 2),
            ),
            TestEntry::regular(
                "boundary/c-plus-1.bin",
                "00000000-0000-4000-8000-000000000143",
                bytes_mod(4097, 3),
            ),
            TestEntry::regular(
                "boundary/multi.bin",
                "00000000-0000-4000-8000-000000000144",
                bytes_mod(9000, 4),
            ),
        ],
    );
}

#[test]
fn rao_tv_paths_matches_fixture_manifest() {
    let long_path = format!("long/{}.bin", "a".repeat(102));
    let inline_100 = format!("inline-{}", "b".repeat(93));
    assert_plaintext_vector_fixture(
        include_str!("../../../fixtures/rao/rao-tv-paths.json"),
        "RAO-TV-PATHS",
        vector_options(105, "rao-tv-paths", "000000000105"),
        vec![
            TestEntry::regular(
                "unicode/vid\u{e9}o.txt",
                "00000000-0000-4000-8000-000000000151",
                b"utf8 path\n".to_vec(),
            ),
            TestEntry::regular(
                long_path,
                "00000000-0000-4000-8000-000000000152",
                b"long path\n".to_vec(),
            ),
            TestEntry::regular(
                inline_100,
                "00000000-0000-4000-8000-000000000153",
                b"inline path\n".to_vec(),
            ),
        ],
    );
}

#[test]
fn rao_tv_metadata_matches_fixture_manifest() {
    assert_plaintext_vector_fixture(
        include_str!("../../../fixtures/rao/rao-tv-metadata.json"),
        "RAO-TV-METADATA",
        vector_options(106, "rao-tv-metadata", "000000000106"),
        vec![
            TestEntry::regular(
                "meta/mtime.txt",
                "00000000-0000-4000-8000-000000000161",
                b"mtime\n".to_vec(),
            )
            .with_mtime("1700000000.123456789"),
            TestEntry::regular(
                "meta/exec.sh",
                "00000000-0000-4000-8000-000000000162",
                b"#!/bin/sh\nexit 0\n".to_vec(),
            )
            .with_executable(true),
            TestEntry::regular(
                "meta/null-exec.txt",
                "00000000-0000-4000-8000-000000000163",
                b"null executable\n".to_vec(),
            ),
        ],
    );
}

#[test]
fn rao_tv_order_matches_fixture_manifest() {
    assert_plaintext_vector_fixture(
        include_str!("../../../fixtures/rao/rao-tv-order.json"),
        "RAO-TV-ORDER",
        vector_options(107, "rao-tv-order", "000000000107"),
        vec![
            TestEntry::regular(
                "z-last.txt",
                "00000000-0000-4000-8000-000000000171",
                b"first in caller order\n".to_vec(),
            ),
            TestEntry::regular(
                "a-first.txt",
                "00000000-0000-4000-8000-000000000172",
                b"second in caller order\n".to_vec(),
            ),
            TestEntry::regular(
                "m-middle.txt",
                "00000000-0000-4000-8000-000000000173",
                b"third in caller order\n".to_vec(),
            ),
        ],
    );
}

#[test]
fn rao_tv_manifest_matches_fixture_manifest() {
    assert_plaintext_vector_fixture(
        include_str!("../../../fixtures/rao/rao-tv-manifest.json"),
        "RAO-TV-MANIFEST",
        vector_options(108, "rao-tv-manifest", "000000000108"),
        vec![
            TestEntry::regular(
                "manifest/alpha.bin",
                "00000000-0000-4000-8000-000000000181",
                bytes_mod(17, 9),
            ),
            TestEntry::regular(
                "manifest/beta.bin",
                "00000000-0000-4000-8000-000000000182",
                bytes_mod(513, 10),
            ),
        ],
    );
}

#[test]
fn rao_tv_nonregular_matches_fixture_manifest() {
    let long_target = format!("targets/{}", "x".repeat(120));
    assert_plaintext_vector_fixture(
        include_str!("../../../fixtures/rao/rao-tv-nonregular.json"),
        "RAO-TV-NONREGULAR",
        vector_options(109, "rao-tv-nonregular", "000000000109"),
        vec![
            TestEntry::directory("empty/", "00000000-0000-4000-8000-000000000191"),
            TestEntry::symlink(
                "links/latest",
                "00000000-0000-4000-8000-000000000192",
                "target.txt",
            ),
            TestEntry::symlink(
                "links/long-target",
                "00000000-0000-4000-8000-000000000193",
                long_target,
            ),
            TestEntry::symlink(
                "links/dangling",
                "00000000-0000-4000-8000-000000000194",
                "missing.txt",
            ),
            TestEntry::regular(
                "target.txt",
                "00000000-0000-4000-8000-000000000195",
                b"target\n".to_vec(),
            ),
        ],
    );
}
