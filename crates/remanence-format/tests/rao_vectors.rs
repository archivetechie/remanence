//! Verifies current RAO vectors and retired RAO 1.0 publication pins.

use remanence_aead::{
    cipher_offset, decrypt_chunk, derive_keys, inspect_bytes, open_inner_range_to_vec, open_to_vec,
    seal_deterministic_for_test_vectors, DataEncryptionKey, EnvelopeSealOptions, RaoAeadError,
    RecipientPrivateKey, RecipientPublicKey, SealOptions, CHACHA20POLY1305_TAG_LEN,
    RAO_WRAP_SUITE_XWING,
};
use remanence_format::{
    read_rem_tar_object, write_rem_tar_object, write_rem_tar_object_from_readers,
    MetadataPreservation, RemTarCborValue, RemTarEntryType, RemTarFile, RemTarFileSpec,
    RemTarFileStream, RemTarObjectOptions,
};
use remanence_library::{VecBlockSink, VecBlockSource};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;

const D1_DEK: [u8; 32] = [0x5d; 32];
const D1_HPKE_RNG_SEED: [u8; 32] = [0xa7; 32];
const E2_DEK: [u8; 32] = [0x7d; 32];
const E2_HPKE_RNG_SEED: [u8; 32] = [0xc3; 32];

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

fn fixture_xattrs(value: Option<&Value>) -> BTreeMap<String, Vec<u8>> {
    let Some(value) = value else {
        return BTreeMap::new();
    };
    let object = value
        .as_object()
        .expect("xattrs fixture value is an object");
    object
        .iter()
        .map(|(name, value)| {
            let hex = value
                .as_str()
                .unwrap_or_else(|| panic!("xattr {name:?} value is hex text"));
            (name.clone(), hex_to_bytes(hex))
        })
        .collect()
}

fn fixture_extensions(value: Option<&Value>) -> BTreeMap<String, RemTarCborValue> {
    let Some(value) = value else {
        return BTreeMap::new();
    };
    value
        .as_object()
        .expect("extensions fixture value is an object")
        .iter()
        .map(|(name, value)| (name.clone(), fixture_cbor_value(value)))
        .collect()
}

fn fixture_cbor_value(value: &Value) -> RemTarCborValue {
    match value {
        Value::Null => RemTarCborValue::Null,
        Value::Bool(value) => RemTarCborValue::Bool(*value),
        Value::Number(value) => RemTarCborValue::Unsigned(
            value
                .as_u64()
                .expect("extension fixture number is unsigned"),
        ),
        Value::String(value) => RemTarCborValue::Text(value.clone()),
        Value::Array(values) => {
            RemTarCborValue::Array(values.iter().map(fixture_cbor_value).collect())
        }
        Value::Object(values) => RemTarCborValue::Map(
            values
                .iter()
                .map(|(key, value)| (key.clone(), fixture_cbor_value(value)))
                .collect(),
        ),
    }
}

fn hex_to_bytes(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len() % 2, 0, "hex string length is even");
    hex.as_bytes()
        .chunks_exact(2)
        .map(|chunk| {
            let text = std::str::from_utf8(chunk).expect("hex chunk is UTF-8");
            u8::from_str_radix(text, 16).expect("hex chunk parses")
        })
        .collect()
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

fn d1_recipient_pair() -> (RecipientPrivateKey, Vec<RecipientPublicKey>) {
    let primary = RecipientPrivateKey::new([0x31; 16], "primary-2026", [0x41; 32]).unwrap();
    let recovery = RecipientPrivateKey::new([0x32; 16], "recovery-2026", [0x42; 32]).unwrap();
    let recipients = vec![
        primary.public_key(0).unwrap(),
        recovery.public_key(1).unwrap(),
    ];
    (primary, recipients)
}

fn e2_recipient_pair() -> (RecipientPrivateKey, Vec<RecipientPublicKey>) {
    let primary = RecipientPrivateKey::new([0x61; 16], "archive-2026-01", [0x51; 32]).unwrap();
    let recovery = RecipientPrivateKey::new([0x62; 16], "recovery-2026-01", [0x52; 32]).unwrap();
    let recipients = vec![
        primary.public_key(0).unwrap(),
        recovery.public_key(1).unwrap(),
    ];
    (primary, recipients)
}

fn build_p1_plaintext() -> Vec<u8> {
    let hello = b"hello, rem archive object\n";
    let pattern: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
    let files = p1_files(hello, &pattern);
    let mut sink = VecBlockSink::new();
    write_rem_tar_object(&mut sink, &p1_options(), &files).unwrap();
    flatten_sink(&sink)
}

fn build_d1_plaintext() -> Vec<u8> {
    let payload: Vec<u8> = (0..262145).map(|i| (i % 256) as u8).collect();
    let files = [RemTarFile {
        path: "v.bin",
        file_id: "00000000-0000-4000-8000-000000000012",
        data: &payload,
        mtime: None,
        executable: None,
    }];
    let mut sink = VecBlockSink::new();
    write_rem_tar_object(&mut sink, &d1_options(), &files).unwrap();
    flatten_sink(&sink)
}

fn seal_fixed_vector(
    plaintext: &[u8],
    options: &RemTarObjectOptions,
    recipients: Vec<RecipientPublicKey>,
    dek: [u8; 32],
    hpke_rng_seed: [u8; 32],
) -> Vec<u8> {
    let envelope_options = EnvelopeSealOptions {
        allow_single_recipient: false,
        common: SealOptions {
            chunk_size: options.chunk_size as u32,
            object_id: options.object_id.clone(),
            plaintext_size: plaintext.len() as u64,
            plaintext_digest: Sha256::digest(plaintext).into(),
        },
        recipients,
    };
    let mut encrypted = Vec::new();
    seal_deterministic_for_test_vectors(
        Cursor::new(plaintext),
        &mut encrypted,
        &envelope_options,
        DataEncryptionKey::from_bytes(dek),
        hpke_rng_seed,
    )
    .unwrap();
    encrypted
}

fn current_xwing_encrypted_vectors() -> (Vec<u8>, Vec<u8>) {
    let p1_plaintext = build_p1_plaintext();
    let (_, e2_recipients) = e2_recipient_pair();
    let e2_first = seal_fixed_vector(
        &p1_plaintext,
        &p1_options(),
        e2_recipients.clone(),
        E2_DEK,
        E2_HPKE_RNG_SEED,
    );
    let e2_second = seal_fixed_vector(
        &p1_plaintext,
        &p1_options(),
        e2_recipients,
        E2_DEK,
        E2_HPKE_RNG_SEED,
    );
    assert_eq!(e2_first, e2_second, "RAO-TV-E2 regenerates byte-exactly");

    let d1_plaintext = build_d1_plaintext();
    let (_, d1_recipients) = d1_recipient_pair();
    let d1_first = seal_fixed_vector(
        &d1_plaintext,
        &d1_options(),
        d1_recipients.clone(),
        D1_DEK,
        D1_HPKE_RNG_SEED,
    );
    let d1_second = seal_fixed_vector(
        &d1_plaintext,
        &d1_options(),
        d1_recipients,
        D1_DEK,
        D1_HPKE_RNG_SEED,
    );
    assert_eq!(
        d1_first, d1_second,
        "RAO-TV-D1 encrypted half regenerates byte-exactly"
    );

    for (name, bytes) in [("RAO-TV-E2", &e2_first), ("RAO-TV-D1 encrypted", &d1_first)] {
        let inspected = inspect_bytes(bytes)
            .unwrap_or_else(|error| panic!("{name} X-Wing envelope inspects: {error}"));
        assert_eq!(
            inspected.header.wrap_suite, RAO_WRAP_SUITE_XWING,
            "{name} emits only the X-Wing discriminator"
        );
        assert!(
            (remanence_aead::RAO_KEY_FRAME_MIN_LEN..=remanence_aead::RAO_KEY_FRAME_MAX_LEN)
                .contains(&(inspected.header.key_frame_len as usize)),
            "{name} key frame is inside the RAO 2.0 bounds"
        );
    }

    (e2_first, d1_first)
}

fn fixture_object_path(filename: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/rao/objects")
        .join(filename)
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

fn hardlink_vector_entries() -> Vec<TestEntry> {
    let long_target = format!("hardlink-targets/{}.bin", "p".repeat(110));
    vec![
        TestEntry::regular(
            "primary.txt",
            "00000000-0000-4000-8000-000000000201",
            b"shared hardlink payload\n".to_vec(),
        ),
        TestEntry::hardlink(
            "links/copy.txt",
            "00000000-0000-4000-8000-000000000202",
            "primary.txt",
        ),
        TestEntry::regular(
            long_target.clone(),
            "00000000-0000-4000-8000-000000000203",
            b"long target hardlink payload\n".to_vec(),
        ),
        TestEntry::hardlink(
            "links/long-target-copy.bin",
            "00000000-0000-4000-8000-000000000204",
            long_target,
        ),
    ]
}

fn xattr_vector_entries() -> Vec<TestEntry> {
    vec![
        TestEntry::regular(
            "tagged.txt",
            "00000000-0000-4000-8000-000000000211",
            b"xattr payload\n".to_vec(),
        )
        .with_xattr("user.comment", b"blue".to_vec())
        .with_xattr("user.remanence.color", vec![0x01, 0x02, 0xff]),
        TestEntry::regular(
            "plain.txt",
            "00000000-0000-4000-8000-000000000212",
            b"plain payload\n".to_vec(),
        ),
    ]
}

fn publication_increment_vectors() -> Vec<(&'static str, RemTarObjectOptions, Vec<TestEntry>)> {
    let portable = vec![TestEntry::regular(
        "metadata/portable.txt",
        "00000000-0000-4000-8000-000000000221",
        b"portable metadata\n".to_vec(),
    )
    .with_xattr("user.comment", b"publication-core".to_vec())];
    let nonuser = vec![TestEntry::regular(
        "metadata/security.txt",
        "00000000-0000-4000-8000-000000000231",
        b"non-user metadata\n".to_vec(),
    )
    .with_xattr(
        "security.remanence.test",
        b"publication-secret-value".to_vec(),
    )];
    let mut extension_options = vector_options(114, "rao-tv-ext-member", "000000000114");
    extension_options.extensions.insert(
        "org.remanence.publication".to_string(),
        RemTarCborValue::Unsigned(1),
    );
    let extension = vec![TestEntry::regular(
        "metadata/extension.txt",
        "00000000-0000-4000-8000-000000000241",
        b"extension metadata\n".to_vec(),
    )];
    let combined = vec![TestEntry::regular(
        "metadata/combined.txt",
        "00000000-0000-4000-8000-000000000251",
        b"combined metadata\n".to_vec(),
    )
    .with_xattr("trusted.remanence.test", b"carry-only-value".to_vec())
    .with_extension(
        "org.remanence.entry-metadata",
        RemTarCborValue::Map(BTreeMap::from([(
            "opaque".to_string(),
            RemTarCborValue::Text("carried".to_string()),
        )])),
    )];
    vec![
        (
            "rao-tv-portable-core-only.rao",
            vector_options(112, "rao-tv-portable-core-only", "000000000112"),
            portable,
        ),
        (
            "rao-tv-nonuser-attribute.rao",
            vector_options(113, "rao-tv-nonuser-attribute", "000000000113"),
            nonuser,
        ),
        ("rao-tv-ext-member.rao", extension_options, extension),
        (
            "rao-tv-attribute-ext-combined.rao",
            vector_options(115, "rao-tv-attribute-ext-combined", "000000000115"),
            combined,
        ),
    ]
}

#[derive(Debug, Clone)]
struct TestEntry {
    entry_type: RemTarEntryType,
    path: String,
    file_id: String,
    data: Vec<u8>,
    link_target: Option<String>,
    xattrs: BTreeMap<String, Vec<u8>>,
    extensions: BTreeMap<String, RemTarCborValue>,
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
            xattrs: BTreeMap::new(),
            extensions: BTreeMap::new(),
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
            xattrs: BTreeMap::new(),
            extensions: BTreeMap::new(),
            mtime: None,
            executable: None,
        }
    }

    fn hardlink(
        path: impl Into<String>,
        file_id: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        Self {
            entry_type: RemTarEntryType::Hardlink,
            path: path.into(),
            file_id: file_id.into(),
            data: Vec::new(),
            link_target: Some(target.into()),
            xattrs: BTreeMap::new(),
            extensions: BTreeMap::new(),
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
            xattrs: BTreeMap::new(),
            extensions: BTreeMap::new(),
            mtime: None,
            executable: None,
        }
    }

    fn with_xattr(mut self, name: impl Into<String>, value: impl Into<Vec<u8>>) -> Self {
        self.xattrs.insert(name.into(), value.into());
        self
    }

    fn with_extension(mut self, name: impl Into<String>, value: RemTarCborValue) -> Self {
        self.extensions.insert(name.into(), value);
        self
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
            RemTarEntryType::Hardlink => RemTarFileSpec::hardlink(
                &self.path,
                &self.file_id,
                self.link_target
                    .as_deref()
                    .expect("hardlink test entry has a target"),
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
        spec.xattrs.clone_from(&self.xattrs);
        spec.extensions.clone_from(&self.extensions);
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
        RemTarEntryType::Hardlink => "hardlink",
        RemTarEntryType::Symlink => "symlink",
        RemTarEntryType::Directory => "directory",
    }
}

fn entry_type_from_fixture(value: &Value) -> RemTarEntryType {
    match value.as_str().expect("entry type is a string") {
        "regular" => RemTarEntryType::Regular,
        "hardlink" => RemTarEntryType::Hardlink,
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
    assert_eq!(
        options.extensions,
        fixture_extensions(inputs.get("extensions")),
        "input object extensions"
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
        assert_eq!(
            entry.xattrs,
            fixture_xattrs(expected_entry.get("xattrs")),
            "input xattrs for {}",
            entry.path
        );
        assert_eq!(
            entry.extensions,
            fixture_extensions(expected_entry.get("extensions")),
            "input extensions for {}",
            entry.path
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
    if let Some(schema_version) = expected.get("schema_version") {
        assert_eq!(
            layout.schema_version,
            schema_version
                .as_str()
                .expect("schema_version fixture value is text")
        );
    }
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

    let hardlink_entries: Vec<&TestEntry> = entries
        .iter()
        .filter(|entry| entry.entry_type == RemTarEntryType::Hardlink)
        .collect();
    if let Some(hardlinks) = expected.get("hardlinks") {
        let hardlinks = hardlinks.as_array().expect("hardlinks are an array");
        assert_eq!(hardlink_entries.len(), hardlinks.len());
        for (entry, hardlink) in hardlink_entries.iter().zip(hardlinks) {
            assert_eq!(entry.path, str_field(hardlink, "path"));
            assert_eq!(
                entry.link_target.as_deref(),
                Some(str_field(hardlink, "target").as_str())
            );
        }
    } else {
        assert!(hardlink_entries.is_empty());
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

    let xattrs = expected.get("xattrs").map_or(&[][..], |value| {
        value.as_array().expect("xattrs are an array").as_slice()
    });
    let xattr_entries: Vec<&TestEntry> = entries
        .iter()
        .filter(|entry| !entry.xattrs.is_empty())
        .collect();
    assert_eq!(xattr_entries.len(), xattrs.len());
    for (entry, expected_xattrs) in xattr_entries.iter().zip(xattrs) {
        assert_eq!(entry.path, str_field(expected_xattrs, "path"));
        assert_eq!(
            entry.xattrs,
            fixture_xattrs(Some(field(expected_xattrs, "xattrs")))
        );
    }
}

fn increment_fixture(filename: &str) -> (&'static str, &'static str) {
    match filename {
        "rao-tv-portable-core-only.rao" => (
            include_str!("../../../fixtures/rao/rao-tv-portable-core-only.json"),
            "RAO-TV-PORTABLE-CORE-ONLY",
        ),
        "rao-tv-nonuser-attribute.rao" => (
            include_str!("../../../fixtures/rao/rao-tv-nonuser-attribute.json"),
            "RAO-TV-NONUSER-ATTRIBUTE",
        ),
        "rao-tv-ext-member.rao" => (
            include_str!("../../../fixtures/rao/rao-tv-ext-member.json"),
            "RAO-TV-EXT-MEMBER",
        ),
        "rao-tv-attribute-ext-combined.rao" => (
            include_str!("../../../fixtures/rao/rao-tv-attribute-ext-combined.json"),
            "RAO-TV-ATTRIBUTE-EXT-COMBINED",
        ),
        other => panic!("unknown publication increment object {other:?}"),
    }
}

fn build_increment_object(
    filename: &str,
    options: &RemTarObjectOptions,
    entries: &[TestEntry],
) -> Vec<u8> {
    let (fixture_json, vector_id) = increment_fixture(filename);
    let fixture = fixture(fixture_json);
    let expected = field(&fixture, "expected");
    let (layout, bytes) = write_test_object(options, entries);

    assert_eq!(str_field(&fixture, "vector_id"), vector_id);
    assert_eq!(layout.schema_version, str_field(expected, "schema_version"));
    assert_eq!(sha256_hex(&bytes), str_field(expected, "stored_digest"));
    assert_eq!(
        sha256_hex(&bytes),
        str_field(expected, "full_object_sha256")
    );
    assert_eq!(sha256_hex(&bytes), str_field(expected, "plaintext_digest"));
    assert_eq!(
        sha256_hex(&bytes[..options.chunk_size]),
        str_field(expected, "first_block_sha256")
    );
    assert_eq!(
        hex(&layout.manifest_cbor),
        str_field(expected, "manifest_cbor_hex")
    );
    assert_eq!(
        hex(&layout.manifest_sha256),
        str_field(expected, "manifest_sha256")
    );
    bytes
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
fn rao2_xwing_envelopes_are_deterministic_and_range_safe() {
    let p1_plaintext = build_p1_plaintext();
    let (e2_primary, _) = e2_recipient_pair();
    let (e2_first, d1_first) = current_xwing_encrypted_vectors();
    let e2_inspect = inspect_bytes(&e2_first).expect("RAO 2.0 E2 envelope inspects");
    assert_eq!(
        e2_inspect.header.wrap_suite, RAO_WRAP_SUITE_XWING,
        "current format integration emits only the X-Wing discriminator"
    );
    let (e2_opened, _) = open_to_vec(&e2_first, &e2_primary).expect("RAO 2.0 E2 envelope opens");
    assert_eq!(e2_opened, p1_plaintext);

    let d1_plaintext = build_d1_plaintext();
    let (d1_primary, _) = d1_recipient_pair();
    let d1_fixture = fixture(include_str!("../../../fixtures/rao/rao-tv-d1.json"));
    let d1_expected_plaintext = field(field(&d1_fixture, "expected"), "plaintext");
    let d1_manifest = hex_to_bytes(&str_field(d1_expected_plaintext, "manifest_cbor_hex"));
    let d1_manifest_layout = field(d1_expected_plaintext, "manifest_layout");
    let d1_manifest_first_chunk = u64_field(d1_manifest_layout, "first_chunk_lba");
    let (manifest_range, range_report) = open_inner_range_to_vec(
        &d1_first,
        &d1_primary,
        d1_manifest_first_chunk,
        0,
        d1_manifest.len() as u64,
    )
    .expect("RAO-TV-D1 manifest range in the final object chunk authenticates");
    let d1_inspect = inspect_bytes(&d1_first).expect("RAO-TV-D1 encrypted object inspects");
    assert_eq!(manifest_range, d1_manifest);
    assert_eq!(
        range_report.first_chunk,
        Some(d1_inspect.chunk_count - 1),
        "RAO-TV-D1 manifest range reaches the true final object chunk"
    );
    assert_eq!(range_report.chunk_count, 1);

    let (d1_opened, d1_open) = open_to_vec(&d1_first, &d1_primary).expect("RAO-TV-D1 opens");
    assert_eq!(d1_opened, d1_plaintext);
    let d1_key_frame = d1_open
        .key_frame
        .serialize()
        .expect("RAO-TV-D1 key frame serializes");
    let d1_keys = derive_keys(
        &D1_DEK,
        &d1_open.header.hkdf_salt,
        &d1_open
            .header
            .header_hash_with_key_frame(&d1_key_frame)
            .expect("RAO-TV-D1 header hash derives"),
    )
    .expect("RAO-TV-D1 payload key derives");
    let final_chunk_index = d1_inspect.chunk_count - 1;
    let final_chunk_offset = usize::try_from(
        cipher_offset(
            d1_open.header.key_frame_len,
            d1_open.header.metadata_frame_len,
            d1_open.header.chunk_size,
            final_chunk_index,
        )
        .expect("RAO-TV-D1 final chunk offset derives"),
    )
    .expect("RAO-TV-D1 final chunk offset fits usize");
    let final_chunk_len = usize::try_from(d1_open.header.chunk_size).unwrap()
        + usize::try_from(CHACHA20POLY1305_TAG_LEN).unwrap();
    let wrong_finality = decrypt_chunk(
        &d1_keys.payload_key,
        final_chunk_index,
        false,
        &d1_first[final_chunk_offset..final_chunk_offset + final_chunk_len],
    )
    .expect_err("the final object chunk must reject a non-final AEAD nonce");
    assert!(
        matches!(wrong_finality, RaoAeadError::AeadAuthenticationFailed),
        "expected AeadAuthenticationFailed, got {wrong_finality:?}"
    );
}

#[test]
fn rao_publication_objects_regenerate_byte_exactly() {
    let (e2, d1_encrypted) = current_xwing_encrypted_vectors();
    let increment_exports: Vec<(&str, Vec<u8>)> = publication_increment_vectors()
        .into_iter()
        .map(|(filename, options, entries)| {
            let first = build_increment_object(filename, &options, &entries);
            let second = build_increment_object(filename, &options, &entries);
            assert_eq!(first, second, "{filename} regenerates byte-exactly");
            (filename, first)
        })
        .collect();

    if let Some(directory) = std::env::var_os("RAO_VECTOR_EXPORT_DIR") {
        let directory = PathBuf::from(directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("rao-tv-e2.rao"), e2).unwrap();
        fs::write(directory.join("rao-tv-d1-encrypted.rao"), d1_encrypted).unwrap();
        for (filename, bytes) in &increment_exports {
            fs::write(directory.join(filename), bytes).unwrap();
        }
    }
}

fn assert_retired_rao1_envelope(filename: &str, expected: &Value) {
    let bytes = fs::read(fixture_object_path(filename)).expect("retired RAO 1.0 fixture exists");
    assert_eq!(bytes.len() as u64, u64_field(expected, "stored_size_bytes"));
    assert_eq!(sha256_hex(&bytes), str_field(expected, "stored_digest"));
    assert_eq!(
        hex(&bytes[..128]),
        str_field(expected, "header_hex"),
        "{filename}: historical header remains pinned"
    );
    assert_eq!(
        bytes[0x38], 0x01,
        "{filename}: fixture is the retired X25519-only representation"
    );
    assert_eq!(
        u32::from_be_bytes(bytes[0x3c..0x40].try_into().unwrap()) as u64,
        u64_field(expected, "key_frame_len")
    );
    assert!(matches!(
        inspect_bytes(&bytes),
        Err(RaoAeadError::InvalidWrapSuite)
    ));
}

#[test]
fn retired_rao1_x25519_envelopes_remain_pinned_and_are_rejected() {
    let e2 = fixture(include_str!("../../../fixtures/rao/rao-tv-e2.json"));
    assert_eq!(str_field(&e2, "status"), "historical-rao1-retired");
    assert_retired_rao1_envelope("rao-tv-e2.rao", field(&e2, "expected"));

    let d1 = fixture(include_str!("../../../fixtures/rao/rao-tv-d1.json"));
    assert_eq!(
        str_field(&d1, "encrypted_status"),
        "historical-rao1-retired"
    );
    assert_retired_rao1_envelope(
        "rao-tv-d1-encrypted.rao",
        field(field(&d1, "expected"), "encrypted"),
    );
}

#[test]
fn rao_tv_d1_plaintext_matches_fixture_manifest() {
    let fixture = fixture(include_str!("../../../fixtures/rao/rao-tv-d1.json"));
    let inputs = field(&fixture, "inputs");
    let expected = field(&fixture, "expected");
    let plaintext = field(expected, "plaintext");
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
fn rao_tv_hardlinks_matches_fixture_manifest() {
    assert_plaintext_vector_fixture(
        include_str!("../../../fixtures/rao/rao-tv-hardlinks.json"),
        "RAO-TV-HARDLINKS",
        vector_options(110, "rao-tv-hardlinks", "000000000110"),
        hardlink_vector_entries(),
    );
}

#[test]
fn rao_tv_xattrs_matches_fixture_manifest() {
    assert_plaintext_vector_fixture(
        include_str!("../../../fixtures/rao/rao-tv-xattrs.json"),
        "RAO-TV-XATTRS",
        vector_options(111, "rao-tv-xattrs", "000000000111"),
        xattr_vector_entries(),
    );
}

#[test]
fn rao_publication_increment_matches_fixture_manifests() {
    for (filename, options, entries) in publication_increment_vectors() {
        let (fixture_json, vector_id) = increment_fixture(filename);
        assert_plaintext_vector_fixture(fixture_json, vector_id, options, entries);
    }
}

#[test]
fn rao_publication_extension_members_reemit_byte_identically() {
    for (filename, mut options, entries) in publication_increment_vectors()
        .into_iter()
        .filter(|(filename, _, _)| filename.contains("ext"))
    {
        let original = build_increment_object(filename, &options, &entries);
        let mut source = VecBlockSource::new(
            original
                .chunks_exact(options.chunk_size)
                .map(Vec::from)
                .collect(),
        );
        let decoded = read_rem_tar_object(
            &mut source,
            options.chunk_size,
            (original.len() / options.chunk_size) as u64,
        )
        .expect("publication extension object reads");

        options.extensions.clone_from(&decoded.object_extensions);
        let mut reemit_entries = entries;
        for entry in &mut reemit_entries {
            let decoded_entry = decoded
                .entry(&entry.path)
                .expect("payload entry is present");
            entry.xattrs.clone_from(&decoded_entry.xattrs);
            entry.extensions.clone_from(&decoded_entry.extensions);
        }
        let (_, reemitted) = write_test_object(&options, &reemit_entries);
        assert_eq!(reemitted, original, "{filename} canonical re-emission");
    }
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
