//! Executes RAO Section 13.5 negative vector manifests.

use std::io::Cursor;

use remanence_aead::stream::{
    encrypt_chunk, encrypt_metadata, stored_size_from_parts, CHACHA20POLY1305_TAG_LEN,
};
use remanence_aead::{
    derive_keys, derive_salt, inspect_bytes, open_to_vec, seal_to_vec, RaoAeadError, RaoHeader,
    RaoMetadata, RootKey, SealOptions, RAO_FOOTER,
};
use remanence_format::{
    plan_rem_tar_object, read_encrypted_rao_object, read_rem_tar_object, stream_rem_tar_object,
    write_rem_tar_object, write_rem_tar_object_from_readers, FormatError, MetadataPreservation,
    RemTarEntrySink, RemTarEntryType, RemTarFile, RemTarFileLayout, RemTarFileSpec,
    RemTarFileStream, RemTarObjectLayout, RemTarObjectOptions, RemTarStreamEntry, TAR_RECORD_SIZE,
};
use remanence_library::{VecBlockSink, VecBlockSource};
use serde_json::Value;
use sha2::{Digest, Sha256};

fn fixture(json: &str) -> Value {
    serde_json::from_str(json).expect("negative fixture manifest is valid JSON")
}

fn cases(fixture: &Value) -> &[Value] {
    fixture
        .get("cases")
        .and_then(Value::as_array)
        .expect("negative fixture cases array exists")
}

fn assert_complete_case_ids(fixture: &Value, expected: &[&str]) {
    assert_eq!(str_field(fixture, "status"), "complete");
    let actual = cases(fixture)
        .iter()
        .map(|case| str_field(case, "id"))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("fixture field {key:?} is a string"))
}

fn u64_field(value: &Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("fixture field {key:?} is an unsigned integer"))
}

fn format_error_name(error: &FormatError) -> &'static str {
    match error {
        FormatError::InvalidInput(_) => "InvalidInput",
        FormatError::Layout(_) => "Layout",
        FormatError::Parse(_) => "Parse",
        FormatError::UstarChecksumMismatch { .. } => "UstarChecksumMismatch",
        FormatError::UnsupportedTarTypeflag { .. } => "UnsupportedTarTypeflag",
        FormatError::ChunkAlignmentViolation { .. } => "ChunkAlignmentViolation",
        FormatError::ChunkSizeMismatch { .. } => "ChunkSizeMismatch",
        FormatError::InvalidPath(_) => "InvalidPath",
        FormatError::TruncatedPayload => "TruncatedPayload",
        FormatError::PaxRecordMalformed(_) => "PaxRecordMalformed",
        FormatError::Cbor(_) => "Cbor",
        FormatError::ManifestDigestMismatch => "ManifestDigestMismatch",
        FormatError::ManifestInvalid(_) => "ManifestInvalid",
        FormatError::FileDigestMismatch { .. } => "FileDigestMismatch",
        FormatError::InnerObjectMismatch { .. } => "InnerObjectMismatch",
        FormatError::Aead(error) => aead_error_name(error),
        FormatError::UnsupportedOperation(_) => "UnsupportedOperation",
        FormatError::UnsupportedFeature(_) | FormatError::UnsupportedFormatGate { .. } => {
            "UnsupportedFeature"
        }
        FormatError::IncompleteBlockWrite { .. } => "IncompleteBlockWrite",
        FormatError::SourceIo { .. } => "SourceIo",
        FormatError::TapeIo(_) => "TapeIo",
    }
}

fn aead_error_name(error: &RaoAeadError) -> &'static str {
    match error {
        RaoAeadError::InvalidMagicBytes => "InvalidMagicBytes",
        RaoAeadError::InvalidHeaderLength => "InvalidHeaderLength",
        RaoAeadError::UnsupportedFormatVersion => "UnsupportedFormatVersion",
        RaoAeadError::InvalidSuite => "InvalidSuite",
        RaoAeadError::InvalidChunkSize => "InvalidChunkSize",
        RaoAeadError::ReservedBytesNotZero => "ReservedBytesNotZero",
        RaoAeadError::InvalidKeyIdentifier => "InvalidKeyIdentifier",
        RaoAeadError::InvalidRootKey => "InvalidRootKey",
        RaoAeadError::InvalidSalt => "InvalidSalt",
        RaoAeadError::MetadataFrameLengthInvalid => "MetadataFrameLengthInvalid",
        RaoAeadError::InvalidObjectIdField => "InvalidObjectIdField",
        RaoAeadError::InvalidInput(_) => "InvalidInput",
        RaoAeadError::UnexpectedEof => "UnexpectedEof",
        RaoAeadError::MissingFinalChunk => "MissingFinalChunk",
        RaoAeadError::InvalidFooter => "InvalidFooter",
        RaoAeadError::TrailingData => "TrailingData",
        RaoAeadError::FillNotZero => "FillNotZero",
        RaoAeadError::AeadAuthenticationFailed => "AeadAuthenticationFailed",
        RaoAeadError::InvalidCborEncoding => "InvalidCborEncoding",
        RaoAeadError::MissingRequiredMetadataField => "MissingRequiredMetadataField",
        RaoAeadError::InvalidMetadataField => "InvalidMetadataField",
        RaoAeadError::SaltDerivationMismatch => "SaltDerivationMismatch",
        RaoAeadError::PlaintextDigestMismatch => "PlaintextDigestMismatch",
        RaoAeadError::PlaintextSizeMismatch => "PlaintextSizeMismatch",
        RaoAeadError::SizeOverflow => "SizeOverflow",
        RaoAeadError::Io(_) => "Io",
        _ => "UnknownEnvelopeError",
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

fn sha256_array(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn base_options() -> RemTarObjectOptions {
    let mut options = RemTarObjectOptions::new(
        "99999999-9999-4999-8999-999999999999",
        "negative-vector",
        "2026-01-01T00:00:00Z",
        "manifest-file-id",
    );
    options.chunk_size = 4096;
    options.metadata_preservation = MetadataPreservation::Minimal;
    options
}

fn regular_spec(path: &str, file_id: &str, bytes: &[u8]) -> RemTarFileSpec {
    let digest = Sha256::digest(bytes);
    let mut file_sha256 = [0u8; 32];
    file_sha256.copy_from_slice(&digest);
    RemTarFileSpec {
        entry_type: RemTarEntryType::Regular,
        path: path.to_string(),
        file_id: file_id.to_string(),
        size_bytes: bytes.len() as u64,
        file_sha256: Some(file_sha256),
        link_target: None,
        mtime: None,
        executable: None,
    }
}

fn assert_writer_case(case: &Value) {
    let id = str_field(case, "id");
    let expected = str_field(case, "expected_error");
    let err = run_writer_case(id).unwrap_err();
    assert_eq!(format_error_name(&err), expected, "{id}: {err}");
}

fn run_writer_case(id: &str) -> Result<(), FormatError> {
    let data = b"payload";
    let options = base_options();
    match id {
        "duplicate-path" => {
            let specs = [
                regular_spec("dup.bin", "file-a", data),
                regular_spec("dup.bin", "file-b", data),
            ];
            plan_rem_tar_object(&options, &specs)?;
        }
        "duplicate-file-id" => {
            let specs = [
                regular_spec("a.bin", "file-a", data),
                regular_spec("b.bin", "file-a", data),
            ];
            plan_rem_tar_object(&options, &specs)?;
        }
        "manifest-file-id-collision" => {
            let mut options = options;
            options.manifest_file_id = "file-a".to_string();
            plan_rem_tar_object(&options, &[regular_spec("a.bin", "file-a", data)])?;
        }
        "reserved-remanence-path" => {
            plan_rem_tar_object(
                &options,
                &[regular_spec("_remanence/private", "file-a", data)],
            )?;
        }
        "control-character-path" => {
            plan_rem_tar_object(&options, &[regular_spec("bad\nname", "file-a", data)])?;
        }
        "absolute-path" => {
            plan_rem_tar_object(&options, &[regular_spec("/abs", "file-a", data)])?;
        }
        "parent-component-path" => {
            plan_rem_tar_object(&options, &[regular_spec("a/../b", "file-a", data)])?;
        }
        "dot-component-path" => {
            plan_rem_tar_object(&options, &[regular_spec("./a", "file-a", data)])?;
        }
        "empty-component-path" => {
            plan_rem_tar_object(&options, &[regular_spec("a//b", "file-a", data)])?;
        }
        "trailing-slash-file-path" => {
            plan_rem_tar_object(&options, &[regular_spec("a/", "file-a", data)])?;
        }
        "malformed-mtime" => {
            let mut spec = regular_spec("a.bin", "file-a", data);
            spec.mtime = Some("not-a-pax-time".to_string());
            plan_rem_tar_object(&options, &[spec])?;
        }
        "streamed-wrong-hash" => {
            let mut spec = regular_spec("a.bin", "file-a", b"expected");
            spec.size_bytes = data.len() as u64;
            let mut reader = Cursor::new(data);
            let mut files = [RemTarFileStream::new(spec, &mut reader)];
            let mut sink = VecBlockSink::new();
            write_rem_tar_object_from_readers(&mut sink, &options, &mut files)?;
        }
        "streamed-wrong-size" => {
            let mut spec = regular_spec("a.bin", "file-a", data);
            spec.size_bytes = data.len() as u64 + 1;
            let mut reader = Cursor::new(data);
            let mut files = [RemTarFileStream::new(spec, &mut reader)];
            let mut sink = VecBlockSink::new();
            write_rem_tar_object_from_readers(&mut sink, &options, &mut files)?;
        }
        "chunk-size-not-multiple-of-512" => {
            let mut options = options;
            options.chunk_size = 513;
            plan_rem_tar_object(&options, &[])?;
        }
        "symlink-nonzero-size" => {
            let spec = RemTarFileSpec {
                entry_type: RemTarEntryType::Symlink,
                path: "link".to_string(),
                file_id: "link-a".to_string(),
                size_bytes: 1,
                file_sha256: None,
                link_target: Some("target".to_string()),
                mtime: None,
                executable: None,
            };
            plan_rem_tar_object(&options, &[spec])?;
        }
        "directory-nonzero-size" => {
            let spec = RemTarFileSpec {
                entry_type: RemTarEntryType::Directory,
                path: "empty/".to_string(),
                file_id: "dir-a".to_string(),
                size_bytes: 1,
                file_sha256: None,
                link_target: None,
                mtime: None,
                executable: None,
            };
            plan_rem_tar_object(&options, &[spec])?;
        }
        "symlink-missing-target" => {
            let spec = RemTarFileSpec {
                entry_type: RemTarEntryType::Symlink,
                path: "link".to_string(),
                file_id: "link-a".to_string(),
                size_bytes: 0,
                file_sha256: None,
                link_target: None,
                mtime: None,
                executable: None,
            };
            plan_rem_tar_object(&options, &[spec])?;
        }
        "directory-missing-trailing-slash" => {
            plan_rem_tar_object(&options, &[RemTarFileSpec::directory("empty", "dir-a")])?;
        }
        other => panic!("unhandled writer negative vector {other:?}"),
    }
    Ok(())
}

struct PlaintextArchive {
    bytes: Vec<u8>,
    layout: RemTarObjectLayout,
    chunk_size: usize,
}

fn base_plaintext_archive() -> PlaintextArchive {
    let options = base_options();
    let files = [RemTarFile {
        path: "payload.bin",
        file_id: "file-a",
        data: b"payload bytes",
        mtime: None,
        executable: Some(false),
    }];
    let mut sink = VecBlockSink::new();
    let layout = write_rem_tar_object(&mut sink, &options, &files).expect("base RAO writes");
    let bytes = sink.blocks.into_iter().flatten().collect();
    PlaintextArchive {
        bytes,
        layout,
        chunk_size: options.chunk_size,
    }
}

fn assert_plaintext_reader_case(case: &Value) {
    let id = str_field(case, "id");
    let operation = str_field(case, "operation");
    let expected = str_field(case, "expected_error");
    let err = run_plaintext_reader_case(id, operation).unwrap_err();
    assert_eq!(format_error_name(&err), expected, "{id}: {err}");
}

fn run_plaintext_reader_case(id: &str, operation: &str) -> Result<(), FormatError> {
    let mut archive = base_plaintext_archive();
    mutate_plaintext_archive(id, &mut archive);
    match operation {
        "read" => read_archive_bytes(&archive.bytes, archive.chunk_size).map(|_| ()),
        "stream" => {
            let mut source = source_from_bytes(&archive.bytes, archive.chunk_size);
            let mut sink = NoopEntrySink;
            stream_rem_tar_object(
                &mut source,
                archive.chunk_size,
                block_count(&archive.bytes, archive.chunk_size),
                &mut sink,
            )
            .map(|_| ())
        }
        "restore" => {
            let mut source = source_from_bytes(&archive.bytes, archive.chunk_size);
            let mut sink = NoopEntrySink;
            stream_rem_tar_object(
                &mut source,
                archive.chunk_size,
                block_count(&archive.bytes, archive.chunk_size),
                &mut sink,
            )
            .map(|_| ())
        }
        other => panic!("unhandled plaintext reader operation {other:?}"),
    }
}

fn mutate_plaintext_archive(id: &str, archive: &mut PlaintextArchive) {
    match id {
        "wrong-format-id" => replace_once(&mut archive.bytes, b"rao-v1", b"rao-v2"),
        "schema-major-2" => replace_once(
            &mut archive.bytes,
            b"REMANENCE.schema_version=1.0",
            b"REMANENCE.schema_version=2.0",
        ),
        "missing-compression" => replace_once(
            &mut archive.bytes,
            b"REMANENCE.compression=none",
            b"REMANENCE.compressiom=none",
        ),
        "compression-gzip" => replace_once(
            &mut archive.bytes,
            b"REMANENCE.compression=none",
            b"REMANENCE.compression=gzip",
        ),
        "encryption-aes-256-gcm" => {
            update_pax_record_resizing_archive(
                &mut archive.bytes,
                0,
                archive.layout.global_pax_body_len,
                "REMANENCE.encryption",
                "REMANENCE.encryption",
                b"aes-256-gcm",
            );
            archive
                .bytes
                .resize(round_up(archive.bytes.len(), archive.chunk_size), 0);
        }
        "chunk-size-mismatch" => replace_once(
            &mut archive.bytes,
            b"REMANENCE.chunk_size=4096",
            b"REMANENCE.chunk_size=8192",
        ),
        "corrupted-header-checksum" => {
            archive.bytes[0] ^= 1;
        }
        "single-zero-eof-record" => {
            let eof = tar_eof_offset(&archive.layout);
            archive.bytes[eof + TAR_RECORD_SIZE] = 1;
        }
        "unknown-typeflag" => {
            let header = entry_header_offset(&archive.layout.files[0]);
            archive.bytes[header + 156] = b'7';
            rewrite_tar_checksum(&mut archive.bytes[header..header + TAR_RECORD_SIZE]);
        }
        "misaligned-nonzero-payload" => misalign_first_payload(archive),
        "traversal-shaped-effective-path" => update_pax_record_preserving_footprint(
            &mut archive.bytes,
            archive.layout.files[0].pax_header_offset as usize,
            archive.layout.files[0].pax_body_len,
            "path",
            "path",
            b"a/../b",
        ),
        "entry-after-manifest" => insert_entry_after_manifest(archive),
        "flipped-payload-bit" => {
            let offset = archive.layout.files[0].data_offset as usize;
            archive.bytes[offset] ^= 1;
        }
        "truncated-payload" => update_pax_record_preserving_footprint(
            &mut archive.bytes,
            archive.layout.files[0].pax_header_offset as usize,
            archive.layout.files[0].pax_body_len,
            "size",
            "size",
            b"999999999",
        ),
        "truncated-pax-body" => {
            set_tar_size(
                &mut archive.bytes[0..TAR_RECORD_SIZE],
                archive.layout.total_size_bytes + 1,
            );
        }
        "pax-record-length-out-of-bounds" => set_pax_body(
            &mut archive.bytes,
            archive.layout.files[0].pax_header_offset as usize,
            archive.layout.files[0].pax_body_len,
            b"999999 path=a\n",
        ),
        "pax-record-missing-equals" => set_pax_body(
            &mut archive.bytes,
            archive.layout.files[0].pax_header_offset as usize,
            archive.layout.files[0].pax_body_len,
            b"13 pathvalue\n",
        ),
        "pax-record-missing-trailing-newline" => set_pax_body(
            &mut archive.bytes,
            archive.layout.files[0].pax_header_offset as usize,
            archive.layout.files[0].pax_body_len,
            b"9 path=a!",
        ),
        "pax-value-control-character" => {
            let record = encode_raw_pax_record("path", b"bad\nname");
            set_pax_body(
                &mut archive.bytes,
                archive.layout.files[0].pax_header_offset as usize,
                archive.layout.files[0].pax_body_len,
                &record,
            );
        }
        "pax-value-non-utf8" => {
            let record = encode_raw_pax_record("path", &[0xff]);
            set_pax_body(
                &mut archive.bytes,
                archive.layout.files[0].pax_header_offset as usize,
                archive.layout.files[0].pax_body_len,
                &record,
            );
        }
        other => panic!("unhandled plaintext reader vector {other:?}"),
    }
}

fn read_archive_bytes(bytes: &[u8], chunk_size: usize) -> Result<(), FormatError> {
    let mut source = source_from_bytes(bytes, chunk_size);
    read_rem_tar_object(&mut source, chunk_size, block_count(bytes, chunk_size)).map(|_| ())
}

fn source_from_bytes(bytes: &[u8], chunk_size: usize) -> VecBlockSource {
    assert_eq!(
        bytes.len() % chunk_size,
        0,
        "archive bytes must be block-sized"
    );
    VecBlockSource::new(bytes.chunks_exact(chunk_size).map(Vec::from).collect())
}

fn block_count(bytes: &[u8], chunk_size: usize) -> u64 {
    assert_eq!(
        bytes.len() % chunk_size,
        0,
        "archive bytes must be block-sized"
    );
    (bytes.len() / chunk_size) as u64
}

fn replace_once(bytes: &mut [u8], needle: &[u8], replacement: &[u8]) {
    assert_eq!(needle.len(), replacement.len());
    let offset = find_bytes(bytes, needle);
    bytes[offset..offset + replacement.len()].copy_from_slice(replacement);
}

fn find_bytes(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap_or_else(|| panic!("needle {:?} not found", needle))
}

fn update_pax_record_preserving_footprint(
    bytes: &mut [u8],
    header_offset: usize,
    old_body_len: usize,
    key: &str,
    new_key: &str,
    new_value: &[u8],
) {
    let body_offset = header_offset + TAR_RECORD_SIZE;
    let old_padded = round_up_512(old_body_len);
    let old_body = &bytes[body_offset..body_offset + old_body_len];
    let mut new_body = replace_pax_record(old_body, key, new_key, new_value);
    if round_up_512(new_body.len()) != old_padded && key != "REMANENCE.pad" {
        if let Some(rebalanced) = rebalance_pax_pad(old_body, &new_body, old_padded) {
            new_body = rebalanced;
        }
    }
    assert_eq!(
        round_up_512(new_body.len()),
        old_padded,
        "pax replacement must preserve rounded footprint"
    );
    set_pax_body(bytes, header_offset, old_body_len, &new_body);
}

fn update_pax_record_resizing_archive(
    bytes: &mut Vec<u8>,
    header_offset: usize,
    old_body_len: usize,
    key: &str,
    new_key: &str,
    new_value: &[u8],
) {
    let body_offset = header_offset + TAR_RECORD_SIZE;
    let old_padded = round_up_512(old_body_len);
    let old_body = &bytes[body_offset..body_offset + old_body_len];
    let new_body = replace_pax_record(old_body, key, new_key, new_value);
    let new_padded = round_up_512(new_body.len());
    let mut header = bytes[header_offset..header_offset + TAR_RECORD_SIZE].to_vec();
    set_tar_size(&mut header, new_body.len() as u64);

    let mut rebuilt = Vec::with_capacity(bytes.len() + new_padded.saturating_sub(old_padded));
    rebuilt.extend_from_slice(&bytes[..header_offset]);
    rebuilt.extend_from_slice(&header);
    rebuilt.extend_from_slice(&new_body);
    rebuilt.resize(header_offset + TAR_RECORD_SIZE + new_padded, 0);
    rebuilt.extend_from_slice(&bytes[body_offset + old_padded..]);
    rebuilt.resize(round_up(rebuilt.len(), TAR_RECORD_SIZE), 0);
    *bytes = rebuilt;
}

fn set_pax_body(bytes: &mut [u8], header_offset: usize, old_body_len: usize, new_body: &[u8]) {
    let body_offset = header_offset + TAR_RECORD_SIZE;
    let old_padded = round_up_512(old_body_len);
    assert!(
        new_body.len() <= old_padded,
        "new pax body must fit old padded body"
    );
    set_tar_size(
        &mut bytes[header_offset..header_offset + TAR_RECORD_SIZE],
        new_body.len() as u64,
    );
    bytes[body_offset..body_offset + old_padded].fill(0);
    bytes[body_offset..body_offset + new_body.len()].copy_from_slice(new_body);
}

fn replace_pax_record(body: &[u8], key: &str, new_key: &str, new_value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body;
    let mut replaced = false;
    while !rest.is_empty() {
        let space = rest.iter().position(|&byte| byte == b' ').unwrap();
        let len = std::str::from_utf8(&rest[..space])
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let record = &rest[..len];
        let equals = record[space + 1..len - 1]
            .iter()
            .position(|&byte| byte == b'=')
            .map(|pos| space + 1 + pos)
            .unwrap();
        let record_key = std::str::from_utf8(&record[space + 1..equals]).unwrap();
        if record_key == key {
            out.extend_from_slice(&encode_raw_pax_record(new_key, new_value));
            replaced = true;
        } else {
            out.extend_from_slice(record);
        }
        rest = &rest[len..];
    }
    assert!(replaced, "pax key {key:?} not found");
    out
}

fn rebalance_pax_pad(old_body: &[u8], new_body: &[u8], target_padded: usize) -> Option<Vec<u8>> {
    let old_pad = pax_value(old_body, "REMANENCE.pad");
    for pad_len in 0..=old_pad.len() + 1024 {
        let pad = vec![b' '; pad_len];
        let candidate = replace_pax_record(new_body, "REMANENCE.pad", "REMANENCE.pad", &pad);
        if round_up_512(candidate.len()) == target_padded {
            return Some(candidate);
        }
    }
    None
}

fn encode_raw_pax_record(key: &str, value: &[u8]) -> Vec<u8> {
    let mut digits = 1usize;
    loop {
        let len = digits + 1 + key.len() + 1 + value.len() + 1;
        let next_digits = decimal_digits(len);
        if next_digits == digits {
            let mut out = format!("{len} {key}=").into_bytes();
            out.extend_from_slice(value);
            out.push(b'\n');
            return out;
        }
        digits = next_digits;
    }
}

fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        digits += 1;
        value /= 10;
    }
    digits
}

fn set_tar_size(header: &mut [u8], size: u64) {
    let encoded = format!("{size:011o}");
    assert!(encoded.len() <= 11, "tar size test value must fit");
    header[124..136].fill(0);
    header[124..124 + encoded.len()].copy_from_slice(encoded.as_bytes());
    rewrite_tar_checksum(header);
}

fn rewrite_tar_checksum(header: &mut [u8]) {
    assert_eq!(header.len(), TAR_RECORD_SIZE);
    header[148..156].fill(b' ');
    let checksum: u64 = header.iter().map(|&byte| u64::from(byte)).sum();
    let encoded = format!("{checksum:06o}\0 ");
    assert_eq!(encoded.len(), 8);
    header[148..156].copy_from_slice(encoded.as_bytes());
}

fn misalign_first_payload(archive: &mut PlaintextArchive) {
    let file = &archive.layout.files[0];
    let old_padded = round_up_512(file.pax_body_len);
    let body_offset = file.pax_header_offset as usize + TAR_RECORD_SIZE;
    let old_body = &archive.bytes[body_offset..body_offset + file.pax_body_len];
    let pad_value = pax_value(old_body, "REMANENCE.pad");
    let mut new_body = None;
    for shrink in 1..=pad_value.len() {
        let replacement = vec![b' '; pad_value.len() - shrink];
        let candidate =
            replace_pax_record(old_body, "REMANENCE.pad", "REMANENCE.pad", &replacement);
        if round_up_512(candidate.len()) + TAR_RECORD_SIZE == old_padded + TAR_RECORD_SIZE - 512 {
            new_body = Some(candidate);
            break;
        }
    }
    let new_body = new_body.expect("base file pax has shrinkable padding");
    let new_padded = round_up_512(new_body.len());
    assert_eq!(old_padded - new_padded, TAR_RECORD_SIZE);

    set_tar_size(
        &mut archive.bytes
            [file.pax_header_offset as usize..file.pax_header_offset as usize + TAR_RECORD_SIZE],
        new_body.len() as u64,
    );
    let mut rebuilt = Vec::with_capacity(archive.bytes.len());
    rebuilt.extend_from_slice(&archive.bytes[..body_offset]);
    rebuilt.extend_from_slice(&new_body);
    rebuilt.resize(body_offset + new_padded, 0);
    rebuilt.extend_from_slice(&archive.bytes[body_offset + old_padded..]);
    rebuilt.resize(round_up(rebuilt.len(), archive.chunk_size), 0);
    archive.bytes = rebuilt;
}

fn pax_value(body: &[u8], key: &str) -> Vec<u8> {
    let mut rest = body;
    while !rest.is_empty() {
        let space = rest.iter().position(|&byte| byte == b' ').unwrap();
        let len = std::str::from_utf8(&rest[..space])
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let record = &rest[..len];
        let equals = record[space + 1..len - 1]
            .iter()
            .position(|&byte| byte == b'=')
            .map(|pos| space + 1 + pos)
            .unwrap();
        if std::str::from_utf8(&record[space + 1..equals]).unwrap() == key {
            return record[equals + 1..len - 1].to_vec();
        }
        rest = &rest[len..];
    }
    panic!("pax key {key:?} not found");
}

fn insert_entry_after_manifest(archive: &mut PlaintextArchive) {
    let file_start = archive.layout.files[0].pax_header_offset as usize;
    let file_end = archive.layout.manifest.pax_header_offset as usize;
    let eof = tar_eof_offset(&archive.layout);
    let duplicate_entry = archive.bytes[file_start..file_end].to_vec();
    let mut rebuilt = Vec::with_capacity(archive.bytes.len() + duplicate_entry.len());
    rebuilt.extend_from_slice(&archive.bytes[..eof]);
    rebuilt.extend_from_slice(&duplicate_entry);
    rebuilt.extend_from_slice(&archive.bytes[eof..]);
    rebuilt.resize(round_up(rebuilt.len(), archive.chunk_size), 0);
    archive.bytes = rebuilt;
}

fn tar_eof_offset(layout: &RemTarObjectLayout) -> usize {
    let manifest_end = layout.manifest.data_offset + layout.manifest.size_bytes;
    round_up(manifest_end as usize, TAR_RECORD_SIZE)
}

fn entry_header_offset(file: &RemTarFileLayout) -> usize {
    file.data_offset as usize - TAR_RECORD_SIZE
}

fn round_up_512(value: usize) -> usize {
    round_up(value, TAR_RECORD_SIZE)
}

fn round_up(value: usize, unit: usize) -> usize {
    let rem = value % unit;
    if rem == 0 {
        value
    } else {
        value + unit - rem
    }
}

struct NoopEntrySink;

impl RemTarEntrySink for NoopEntrySink {
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

fn assert_inner_case(case: &Value) {
    let id = str_field(case, "id");
    let expected = str_field(case, "expected_error");
    let err = run_inner_case(id).unwrap_err();
    assert_eq!(format_error_name(&err), expected, "{id}: {err}");
}

fn run_inner_case(id: &str) -> Result<(), FormatError> {
    let mut archive = base_plaintext_archive();
    let mut header_object_id = archive.layout.object_id.clone();
    match id {
        "inner-object-id-differs" => {
            header_object_id = "header-object-id".to_string();
        }
        "inner-chunk-size-differs" => replace_once(
            &mut archive.bytes,
            b"REMANENCE.chunk_size=4096",
            b"REMANENCE.chunk_size=8192",
        ),
        "inner-encryption-not-none" => {
            update_pax_record_resizing_archive(
                &mut archive.bytes,
                0,
                archive.layout.global_pax_body_len,
                "REMANENCE.encryption",
                "REMANENCE.encryption",
                b"aes-256-gcm",
            );
            archive
                .bytes
                .resize(round_up(archive.bytes.len(), archive.chunk_size), 0);
        }
        other => panic!("unhandled inner negative vector {other:?}"),
    }
    let root = RootKey::new([0x33; 32]).expect("root key is valid");
    let key_id = [0x44; 16];
    let plaintext_digest = sha256_array(&archive.bytes);
    let seal_options = SealOptions {
        chunk_size: archive.chunk_size as u32,
        key_id,
        object_id: header_object_id,
        plaintext_size: archive.bytes.len() as u64,
        plaintext_digest,
    };
    let (sealed, report) = seal_to_vec(&archive.bytes, &root, &seal_options)?;
    let mut source = source_from_bytes(&sealed, archive.chunk_size);
    read_encrypted_rao_object(
        &mut source,
        archive.chunk_size,
        report.stored_size_blocks,
        &root,
    )
    .map(|_| ())
}

fn base_envelope() -> (Vec<u8>, RootKey) {
    let plaintext = base_envelope_plaintext();
    let root = base_root_key();
    let options = base_seal_options(&plaintext);
    let sealed = seal_to_vec(&plaintext, &root, &options)
        .expect("base envelope seals")
        .0;
    (sealed, root)
}

fn base_envelope_plaintext() -> Vec<u8> {
    vec![0x5a; 1024]
}

fn base_root_key() -> RootKey {
    RootKey::new([0x11; 32]).expect("root key is valid")
}

fn base_seal_options(plaintext: &[u8]) -> SealOptions {
    SealOptions {
        chunk_size: 512,
        key_id: [0x10; 16],
        object_id: "object-1".to_string(),
        plaintext_size: plaintext.len() as u64,
        plaintext_digest: sha256_array(plaintext),
    }
}

fn push_cbor_type_len(out: &mut Vec<u8>, major: u8, value: u64) {
    let prefix = major << 5;
    match value {
        0..=23 => out.push(prefix | value as u8),
        24..=0xff => out.extend_from_slice(&[prefix | 24, value as u8]),
        0x100..=0xffff => {
            out.push(prefix | 25);
            out.extend_from_slice(&(value as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(prefix | 26);
            out.extend_from_slice(&(value as u32).to_be_bytes());
        }
        _ => {
            out.push(prefix | 27);
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}

fn push_cbor_uint(out: &mut Vec<u8>, value: u64) {
    push_cbor_type_len(out, 0, value);
}

fn push_cbor_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    push_cbor_type_len(out, 2, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn push_cbor_text(out: &mut Vec<u8>, text: &str) {
    push_cbor_type_len(out, 3, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

fn push_cbor_metadata_fields(
    out: &mut Vec<u8>,
    plaintext_size: u64,
    plaintext_digest: [u8; 32],
    metadata_version: u64,
) {
    push_cbor_uint(out, 0);
    push_cbor_uint(out, metadata_version);
    push_cbor_uint(out, 1);
    push_cbor_uint(out, plaintext_size);
    push_cbor_uint(out, 2);
    push_cbor_text(out, "sha256");
    push_cbor_uint(out, 3);
    push_cbor_bytes(out, &plaintext_digest);
}

fn metadata_cbor(
    plaintext_size: u64,
    plaintext_digest: [u8; 32],
    metadata_version: u64,
) -> Vec<u8> {
    let mut out = Vec::new();
    push_cbor_type_len(&mut out, 5, 4);
    push_cbor_metadata_fields(&mut out, plaintext_size, plaintext_digest, metadata_version);
    out
}

fn metadata_cbor_with_extra(options: &SealOptions, extra_value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    push_cbor_type_len(&mut out, 5, 5);
    push_cbor_metadata_fields(
        &mut out,
        options.plaintext_size,
        options.plaintext_digest,
        1,
    );
    push_cbor_uint(&mut out, 4);
    out.extend_from_slice(extra_value);
    out
}

fn metadata_cbor_missing_plaintext_size(options: &SealOptions) -> Vec<u8> {
    let mut out = Vec::new();
    push_cbor_type_len(&mut out, 5, 3);
    push_cbor_uint(&mut out, 0);
    push_cbor_uint(&mut out, 1);
    push_cbor_uint(&mut out, 2);
    push_cbor_text(&mut out, "sha256");
    push_cbor_uint(&mut out, 3);
    push_cbor_bytes(&mut out, &options.plaintext_digest);
    out
}

fn metadata_cbor_duplicate_key(options: &SealOptions) -> Vec<u8> {
    let mut out = Vec::new();
    push_cbor_type_len(&mut out, 5, 5);
    push_cbor_uint(&mut out, 0);
    push_cbor_uint(&mut out, 1);
    push_cbor_uint(&mut out, 1);
    push_cbor_uint(&mut out, options.plaintext_size);
    push_cbor_uint(&mut out, 1);
    push_cbor_uint(&mut out, options.plaintext_size);
    push_cbor_uint(&mut out, 2);
    push_cbor_text(&mut out, "sha256");
    push_cbor_uint(&mut out, 3);
    push_cbor_bytes(&mut out, &options.plaintext_digest);
    out
}

fn metadata_cbor_non_shortest_integer(options: &SealOptions) -> Vec<u8> {
    let mut out = Vec::new();
    push_cbor_type_len(&mut out, 5, 4);
    out.extend_from_slice(&[0x18, 0x00]);
    push_cbor_uint(&mut out, 1);
    push_cbor_uint(&mut out, 1);
    push_cbor_uint(&mut out, options.plaintext_size);
    push_cbor_uint(&mut out, 2);
    push_cbor_text(&mut out, "sha256");
    push_cbor_uint(&mut out, 3);
    push_cbor_bytes(&mut out, &options.plaintext_digest);
    out
}

fn metadata_cbor_missing_key(options: &SealOptions, missing_key: u64) -> Vec<u8> {
    let mut out = Vec::new();
    push_cbor_type_len(&mut out, 5, 3);
    for key in 0..=3 {
        if key == missing_key {
            continue;
        }
        push_cbor_uint(&mut out, key);
        push_metadata_field_value(&mut out, key, options);
    }
    out
}

fn metadata_cbor_with_field_value(options: &SealOptions, field_key: u64, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    push_cbor_type_len(&mut out, 5, 4);
    for key in 0..=3 {
        push_cbor_uint(&mut out, key);
        if key == field_key {
            out.extend_from_slice(value);
        } else {
            push_metadata_field_value(&mut out, key, options);
        }
    }
    out
}

fn push_metadata_field_value(out: &mut Vec<u8>, key: u64, options: &SealOptions) {
    match key {
        0 => push_cbor_uint(out, 1),
        1 => push_cbor_uint(out, options.plaintext_size),
        2 => push_cbor_text(out, "sha256"),
        3 => push_cbor_bytes(out, &options.plaintext_digest),
        other => panic!("unknown metadata field key {other}"),
    }
}

fn metadata_cbor_with_trailing_byte(options: &SealOptions) -> Vec<u8> {
    let mut out = metadata_cbor(options.plaintext_size, options.plaintext_digest, 1);
    out.push(0);
    out
}

fn metadata_cbor_with_digest_len(options: &SealOptions, len: usize) -> Vec<u8> {
    let digest = vec![0u8; len];
    let mut value = Vec::new();
    push_cbor_bytes(&mut value, &digest);
    metadata_cbor_with_field_value(options, 3, &value)
}

fn metadata_plaintext_case(id: &str, options: &SealOptions) -> Option<Vec<u8>> {
    Some(match id {
        "metadata-top-level-array" => {
            let mut out = Vec::new();
            push_cbor_type_len(&mut out, 4, 0);
            out
        }
        "metadata-key-text" => {
            let mut out = Vec::new();
            push_cbor_type_len(&mut out, 5, 1);
            push_cbor_text(&mut out, "x");
            push_cbor_uint(&mut out, 0);
            out
        }
        "metadata-float" => metadata_cbor_with_extra(options, &[0xf9, 0x3c, 0x00]),
        "metadata-tag" => metadata_cbor_with_extra(options, &[0xc0, 0x00]),
        "metadata-indefinite-length" => metadata_cbor_with_extra(options, &[0x5f, 0xff]),
        "metadata-simple-undefined" => metadata_cbor_with_extra(options, &[0xf7]),
        "metadata-duplicate-key" => metadata_cbor_duplicate_key(options),
        "metadata-non-shortest-integer" => metadata_cbor_non_shortest_integer(options),
        "metadata-missing-plaintext-size" => metadata_cbor_missing_plaintext_size(options),
        "metadata-missing-version" => metadata_cbor_missing_key(options, 0),
        "metadata-missing-digest-alg" => metadata_cbor_missing_key(options, 2),
        "metadata-missing-digest" => metadata_cbor_missing_key(options, 3),
        "metadata-version-text" => metadata_cbor_with_field_value(options, 0, b"\x61\x31"),
        "metadata-version-2" => metadata_cbor(options.plaintext_size, options.plaintext_digest, 2),
        "metadata-plaintext-size-text" => {
            metadata_cbor_with_field_value(options, 1, b"\x64\x31\x30\x32\x34")
        }
        "metadata-plaintext-size-not-multiple" => metadata_cbor(513, options.plaintext_digest, 1),
        "metadata-plaintext-size-zero" => metadata_cbor(0, options.plaintext_digest, 1),
        "metadata-plaintext-size-overflow" => {
            metadata_cbor(u64::MAX - 511, options.plaintext_digest, 1)
        }
        "metadata-digest-alg-not-sha256" => {
            metadata_cbor_with_field_value(options, 2, b"\x66\x73\x68\x61\x35\x31\x32")
        }
        "metadata-digest-alg-bytes" => {
            metadata_cbor_with_field_value(options, 2, b"\x46\x73\x68\x61\x32\x35\x36")
        }
        "metadata-digest-short" => metadata_cbor_with_digest_len(options, 31),
        "metadata-digest-text" => {
            metadata_cbor_with_field_value(options, 3, b"\x66\x73\x68\x61\x32\x35\x36")
        }
        "metadata-trailing-byte" => metadata_cbor_with_trailing_byte(options),
        _ => return None,
    })
}

/// Builds authenticated but intentionally nonconformant envelopes for Section 13.5.
fn defective_envelope(
    plaintext: &[u8],
    root: &RootKey,
    options: &SealOptions,
    metadata_digest: [u8; 32],
    salt_override: Option<[u8; 16]>,
) -> Result<Vec<u8>, RaoAeadError> {
    let metadata = RaoMetadata::new(options.plaintext_size, metadata_digest, options.chunk_size)?;
    let metadata_plaintext = metadata.to_cbor_bytes(options.chunk_size)?;
    envelope_from_metadata_plaintext(
        plaintext,
        root,
        options,
        &metadata_plaintext,
        metadata_digest,
        salt_override,
        true,
    )
}

fn envelope_from_metadata_plaintext(
    plaintext: &[u8],
    root: &RootKey,
    options: &SealOptions,
    metadata_plaintext: &[u8],
    salt_digest: [u8; 32],
    salt_override: Option<[u8; 16]>,
    last_chunk_final: bool,
) -> Result<Vec<u8>, RaoAeadError> {
    let metadata_frame_len = u64::try_from(metadata_plaintext.len())
        .ok()
        .and_then(|len| len.checked_add(CHACHA20POLY1305_TAG_LEN))
        .ok_or(RaoAeadError::SizeOverflow)?;
    let object_id_field = remanence_aead::header::object_id_field(&options.object_id)?;
    let salt = match salt_override {
        Some(salt) => salt,
        None => derive_salt(root, &object_id_field, &salt_digest, metadata_plaintext)?,
    };
    let header = RaoHeader::new(
        options.chunk_size,
        options.key_id,
        salt,
        metadata_frame_len,
        options.object_id.clone(),
    )?;
    let keys = derive_keys(root, &header.hkdf_salt, &header.header_hash()?)?;

    let chunk_size =
        usize::try_from(options.chunk_size).map_err(|_| RaoAeadError::InvalidChunkSize)?;
    if plaintext.len() as u64 != options.plaintext_size
        || plaintext.len() % chunk_size != 0
        || plaintext.is_empty()
    {
        return Err(RaoAeadError::InvalidInput(
            "defective vector plaintext must match options".to_string(),
        ));
    }

    let mut out = Vec::new();
    out.extend_from_slice(&header.serialize()?);
    out.extend_from_slice(&encrypt_metadata(&keys.metadata_key, metadata_plaintext)?);
    let chunk_count = plaintext.len() / chunk_size;
    for index in 0..chunk_count {
        let start = index * chunk_size;
        let end = start + chunk_size;
        let final_chunk = if index + 1 == chunk_count {
            last_chunk_final
        } else {
            false
        };
        out.extend_from_slice(&encrypt_chunk(
            &keys.payload_key,
            index as u64,
            final_chunk,
            &plaintext[start..end],
        )?);
    }
    out.extend_from_slice(RAO_FOOTER);
    let stored_size = stored_size_from_parts(
        options.chunk_size,
        metadata_frame_len,
        options.plaintext_size,
    )?;
    let stored_size = usize::try_from(stored_size).map_err(|_| RaoAeadError::SizeOverflow)?;
    if out.len() > stored_size {
        return Err(RaoAeadError::SizeOverflow);
    }
    out.resize(stored_size, 0);
    Ok(out)
}

fn envelope_with_extra_payload_chunk(
    plaintext: &[u8],
    root: &RootKey,
    options: &SealOptions,
) -> Result<Vec<u8>, RaoAeadError> {
    let metadata = RaoMetadata::new(
        options.plaintext_size,
        options.plaintext_digest,
        options.chunk_size,
    )?;
    let metadata_plaintext = metadata.to_cbor_bytes(options.chunk_size)?;
    let metadata_frame_len = u64::try_from(metadata_plaintext.len())
        .ok()
        .and_then(|len| len.checked_add(CHACHA20POLY1305_TAG_LEN))
        .ok_or(RaoAeadError::SizeOverflow)?;
    let object_id_field = remanence_aead::header::object_id_field(&options.object_id)?;
    let salt = derive_salt(
        root,
        &object_id_field,
        &options.plaintext_digest,
        &metadata_plaintext,
    )?;
    let header = RaoHeader::new(
        options.chunk_size,
        options.key_id,
        salt,
        metadata_frame_len,
        options.object_id.clone(),
    )?;
    let keys = derive_keys(root, &header.hkdf_salt, &header.header_hash()?)?;
    let chunk_size =
        usize::try_from(options.chunk_size).map_err(|_| RaoAeadError::InvalidChunkSize)?;
    if plaintext.len() as u64 != options.plaintext_size
        || plaintext.len() % chunk_size != 0
        || plaintext.is_empty()
    {
        return Err(RaoAeadError::InvalidInput(
            "extra-chunk vector plaintext must match options".to_string(),
        ));
    }

    let mut out = Vec::new();
    out.extend_from_slice(&header.serialize()?);
    out.extend_from_slice(&encrypt_metadata(&keys.metadata_key, &metadata_plaintext)?);
    let chunk_count = plaintext.len() / chunk_size;
    for index in 0..chunk_count {
        let start = index * chunk_size;
        let end = start + chunk_size;
        out.extend_from_slice(&encrypt_chunk(
            &keys.payload_key,
            index as u64,
            false,
            &plaintext[start..end],
        )?);
    }
    let extra_plaintext = vec![0xA5; chunk_size];
    out.extend_from_slice(&encrypt_chunk(
        &keys.payload_key,
        chunk_count as u64,
        true,
        &extra_plaintext,
    )?);
    out.extend_from_slice(RAO_FOOTER);
    let apparent_plaintext_size = options
        .plaintext_size
        .checked_add(u64::from(options.chunk_size))
        .ok_or(RaoAeadError::SizeOverflow)?;
    let stored_size = stored_size_from_parts(
        options.chunk_size,
        metadata_frame_len,
        apparent_plaintext_size,
    )?;
    let stored_size = usize::try_from(stored_size).map_err(|_| RaoAeadError::SizeOverflow)?;
    if out.len() > stored_size {
        return Err(RaoAeadError::SizeOverflow);
    }
    out.resize(stored_size, 0);
    Ok(out)
}

fn non_derived_salt(root: &RootKey, options: &SealOptions) -> Result<[u8; 16], RaoAeadError> {
    let metadata = RaoMetadata::new(
        options.plaintext_size,
        options.plaintext_digest,
        options.chunk_size,
    )?;
    let metadata_plaintext = metadata.to_cbor_bytes(options.chunk_size)?;
    let object_id_field = remanence_aead::header::object_id_field(&options.object_id)?;
    let expected = derive_salt(
        root,
        &object_id_field,
        &options.plaintext_digest,
        &metadata_plaintext,
    )?;
    let mut salt = [0x77; 16];
    if salt == expected {
        salt[0] ^= 1;
    }
    Ok(salt)
}

fn assert_envelope_case(case: &Value) {
    let id = str_field(case, "id");
    let operation = str_field(case, "operation");
    let expected = str_field(case, "expected_error");
    let err = run_envelope_case(id, operation).unwrap_err();
    assert_eq!(aead_error_name(&err), expected, "{id}: {err}");
}

fn run_envelope_case(id: &str, operation: &str) -> Result<(), RaoAeadError> {
    let (mut sealed, root) = base_envelope();
    let inspected = inspect_bytes(&sealed).expect("base envelope inspects");
    let metadata_start = 128usize;
    let metadata_end = metadata_start + inspected.header.metadata_frame_len as usize;
    let chunk_frame_len = inspected.header.chunk_size as usize + 16;
    let footer_offset = inspected.footer_offset as usize;

    match id {
        "wrong-magic" => sealed[0] = b'X',
        "header-len-not-128" => sealed[5] = 127,
        "format-version-2" => sealed[6] = 2,
        "unknown-suite-id" => sealed[7] = 2,
        "chunk-size-zero" => sealed[8..12].copy_from_slice(&0u32.to_be_bytes()),
        "chunk-size-not-multiple-of-512" => sealed[8..12].copy_from_slice(&513u32.to_be_bytes()),
        "flags-nonzero" => sealed[15] = 1,
        "reserved-bytes-nonzero" => sealed[0x38] = 1,
        "all-zero-key-id" => sealed[0x10..0x20].fill(0),
        "all-zero-hkdf-salt" => sealed[0x20..0x30].fill(0),
        "object-id-all-nul" => sealed[0x40..0x80].fill(0),
        "object-id-interior-nul" => {
            sealed[0x40..0x80].fill(0);
            sealed[0x40] = b'a';
            sealed[0x41] = b'b';
            sealed[0x43] = b'c';
        }
        "object-id-non-utf8" => {
            sealed[0x40..0x80].fill(0);
            sealed[0x40] = 0xff;
        }
        "metadata-frame-len-16" => sealed[0x30..0x38].copy_from_slice(&16u64.to_be_bytes()),
        "metadata-frame-len-over-max" => {
            sealed[0x30..0x38].copy_from_slice(&(16u64 * 1024 * 1024 + 1).to_be_bytes());
        }
        "salt-bit-flipped" => sealed[0x20] ^= 1,
        "key-id-swapped" => sealed[0x10] ^= 1,
        "ciphertext-bit-flipped" => sealed[metadata_end] ^= 1,
        "payload-chunks-transposed" => {
            let first = metadata_end;
            let second = first + chunk_frame_len;
            for offset in 0..chunk_frame_len {
                sealed.swap(first + offset, second + offset);
            }
        }
        "payload-final-flag-wrong" => {
            let plaintext = base_envelope_plaintext();
            let options = base_seal_options(&plaintext);
            let metadata = RaoMetadata::new(
                options.plaintext_size,
                options.plaintext_digest,
                options.chunk_size,
            )?;
            let metadata_plaintext = metadata.to_cbor_bytes(options.chunk_size)?;
            sealed = envelope_from_metadata_plaintext(
                &plaintext,
                &root,
                &options,
                &metadata_plaintext,
                options.plaintext_digest,
                None,
                false,
            )?;
        }
        "payload-extra-chunk-appended" => {
            let plaintext = base_envelope_plaintext();
            let options = base_seal_options(&plaintext);
            sealed = envelope_with_extra_payload_chunk(&plaintext, &root, &options)?;
        }
        "sealed-metadata-wrong-plaintext-digest" => {
            let plaintext = base_envelope_plaintext();
            let options = base_seal_options(&plaintext);
            let mut wrong_digest = options.plaintext_digest;
            wrong_digest[0] ^= 1;
            sealed = defective_envelope(&plaintext, &root, &options, wrong_digest, None)?;
        }
        "sealed-under-non-derived-salt" => {
            let plaintext = base_envelope_plaintext();
            let options = base_seal_options(&plaintext);
            sealed = defective_envelope(
                &plaintext,
                &root,
                &options,
                options.plaintext_digest,
                Some(non_derived_salt(&root, &options)?),
            )?;
        }
        "metadata-float"
        | "metadata-tag"
        | "metadata-indefinite-length"
        | "metadata-simple-undefined"
        | "metadata-duplicate-key"
        | "metadata-non-shortest-integer"
        | "metadata-missing-plaintext-size"
        | "metadata-missing-version"
        | "metadata-missing-digest-alg"
        | "metadata-missing-digest"
        | "metadata-version-text"
        | "metadata-version-2"
        | "metadata-plaintext-size-text"
        | "metadata-plaintext-size-not-multiple"
        | "metadata-plaintext-size-zero"
        | "metadata-plaintext-size-overflow"
        | "metadata-digest-alg-not-sha256"
        | "metadata-digest-alg-bytes"
        | "metadata-digest-short"
        | "metadata-digest-text"
        | "metadata-top-level-array"
        | "metadata-key-text"
        | "metadata-trailing-byte" => {
            let plaintext = base_envelope_plaintext();
            let options = base_seal_options(&plaintext);
            let metadata_plaintext =
                metadata_plaintext_case(id, &options).expect("metadata vector is known");
            sealed = envelope_from_metadata_plaintext(
                &plaintext,
                &root,
                &options,
                &metadata_plaintext,
                options.plaintext_digest,
                None,
                true,
            )?;
        }
        "eof-inside-metadata-frame" => sealed.truncate(metadata_end - 1),
        "eof-mid-chunk" => sealed.truncate(metadata_end + chunk_frame_len / 2),
        "payload-absent-after-metadata" => sealed.truncate(metadata_end),
        "footer-bytes-wrong" => sealed[footer_offset] ^= 1,
        "fill-byte-nonzero" => sealed[footer_offset + 16] = 1,
        "trailing-byte" | "trailing-byte-keyed" => sealed.push(0),
        "seal-plaintext-size-not-multiple" => {
            let plaintext = vec![0x5a; 513];
            let digest = Sha256::digest(&plaintext);
            let mut plaintext_digest = [0u8; 32];
            plaintext_digest.copy_from_slice(&digest);
            let options = SealOptions {
                chunk_size: 512,
                key_id: [0x10; 16],
                object_id: "object-1".to_string(),
                plaintext_size: plaintext.len() as u64,
                plaintext_digest,
            };
            seal_to_vec(&plaintext, &root, &options)?;
            return Ok(());
        }
        "seal-object-id-too-long" => {
            let plaintext = vec![0x5a; 512];
            let digest = Sha256::digest(&plaintext);
            let mut plaintext_digest = [0u8; 32];
            plaintext_digest.copy_from_slice(&digest);
            let options = SealOptions {
                chunk_size: 512,
                key_id: [0x10; 16],
                object_id: "x".repeat(65),
                plaintext_size: plaintext.len() as u64,
                plaintext_digest,
            };
            seal_to_vec(&plaintext, &root, &options)?;
            return Ok(());
        }
        "root-key-too-short" => {
            RootKey::new([0x11; 16])?;
            return Ok(());
        }
        other => panic!("unhandled envelope negative vector {other:?}"),
    }

    match operation {
        "open" => {
            open_to_vec(&sealed, &root)?;
        }
        "inspect" => {
            inspect_bytes(&sealed)?;
        }
        "seal" | "root-key" => panic!("case {id:?} did not return from its {operation} branch"),
        other => panic!("unhandled envelope operation {other:?} for case {id:?}"),
    }
    Ok(())
}

fn assert_envelope_base(fixture: &Value) {
    let base = fixture.get("base").expect("envelope base record exists");
    assert_eq!(u64_field(base, "chunk_size"), 512);
    assert_eq!(u64_field(base, "plaintext_size"), 1024);
    assert_eq!(str_field(base, "plaintext_byte"), "5a");
    assert_eq!(str_field(base, "root_key"), hex(&[0x11; 32]));
    assert_eq!(str_field(base, "key_id"), hex(&[0x10; 16]));
    assert_eq!(str_field(base, "object_id"), "object-1");
}

#[test]
fn plaintext_writer_negative_vectors_match_manifest_errors() {
    let fixture = fixture(include_str!("../../../fixtures/rao/negative-writer.json"));
    assert_complete_case_ids(
        &fixture,
        &[
            "duplicate-path",
            "duplicate-file-id",
            "manifest-file-id-collision",
            "reserved-remanence-path",
            "control-character-path",
            "absolute-path",
            "parent-component-path",
            "dot-component-path",
            "empty-component-path",
            "trailing-slash-file-path",
            "malformed-mtime",
            "streamed-wrong-hash",
            "streamed-wrong-size",
            "chunk-size-not-multiple-of-512",
            "symlink-nonzero-size",
            "directory-nonzero-size",
            "symlink-missing-target",
            "directory-missing-trailing-slash",
        ],
    );
    for case in cases(&fixture) {
        assert_writer_case(case);
    }
}

#[test]
fn plaintext_reader_negative_vectors_match_manifest_errors() {
    let fixture = fixture(include_str!(
        "../../../fixtures/rao/negative-plaintext-reader.json"
    ));
    assert_complete_case_ids(
        &fixture,
        &[
            "wrong-format-id",
            "schema-major-2",
            "missing-compression",
            "compression-gzip",
            "encryption-aes-256-gcm",
            "chunk-size-mismatch",
            "corrupted-header-checksum",
            "single-zero-eof-record",
            "unknown-typeflag",
            "misaligned-nonzero-payload",
            "traversal-shaped-effective-path",
            "entry-after-manifest",
            "flipped-payload-bit",
            "truncated-payload",
            "truncated-pax-body",
            "pax-record-length-out-of-bounds",
            "pax-record-missing-equals",
            "pax-record-missing-trailing-newline",
            "pax-value-control-character",
            "pax-value-non-utf8",
        ],
    );
    let base = fixture.get("base").expect("plaintext reader base exists");
    assert_eq!(u64_field(base, "chunk_size"), 4096);
    assert_eq!(
        str_field(base, "object_id"),
        "99999999-9999-4999-8999-999999999999"
    );
    assert_eq!(str_field(base, "caller_object_id"), "negative-vector");
    for case in cases(&fixture) {
        assert_plaintext_reader_case(case);
    }
}

#[test]
fn encrypted_inner_negative_vectors_match_manifest_errors() {
    let fixture = fixture(include_str!("../../../fixtures/rao/negative-inner.json"));
    assert_complete_case_ids(
        &fixture,
        &[
            "inner-object-id-differs",
            "inner-chunk-size-differs",
            "inner-encryption-not-none",
        ],
    );
    let base = fixture.get("base").expect("inner base record exists");
    assert_eq!(u64_field(base, "chunk_size"), 4096);
    assert_eq!(
        str_field(base, "inner_object_id"),
        "99999999-9999-4999-8999-999999999999"
    );
    assert_eq!(str_field(base, "root_key"), hex(&[0x33; 32]));
    assert_eq!(str_field(base, "key_id"), hex(&[0x44; 16]));
    for case in cases(&fixture) {
        assert_inner_case(case);
    }
}

#[test]
fn envelope_negative_vectors_match_manifest_errors() {
    let fixture = fixture(include_str!("../../../fixtures/rao/negative-envelope.json"));
    assert_complete_case_ids(
        &fixture,
        &[
            "wrong-magic",
            "header-len-not-128",
            "format-version-2",
            "unknown-suite-id",
            "chunk-size-zero",
            "chunk-size-not-multiple-of-512",
            "flags-nonzero",
            "reserved-bytes-nonzero",
            "all-zero-key-id",
            "all-zero-hkdf-salt",
            "object-id-all-nul",
            "object-id-interior-nul",
            "object-id-non-utf8",
            "metadata-frame-len-16",
            "metadata-frame-len-over-max",
            "salt-bit-flipped",
            "key-id-swapped",
            "ciphertext-bit-flipped",
            "payload-chunks-transposed",
            "payload-final-flag-wrong",
            "payload-extra-chunk-appended",
            "sealed-metadata-wrong-plaintext-digest",
            "sealed-under-non-derived-salt",
            "metadata-float",
            "metadata-top-level-array",
            "metadata-key-text",
            "metadata-tag",
            "metadata-indefinite-length",
            "metadata-simple-undefined",
            "metadata-duplicate-key",
            "metadata-non-shortest-integer",
            "metadata-missing-plaintext-size",
            "metadata-missing-version",
            "metadata-missing-digest-alg",
            "metadata-missing-digest",
            "metadata-version-text",
            "metadata-version-2",
            "metadata-plaintext-size-text",
            "metadata-plaintext-size-not-multiple",
            "metadata-plaintext-size-zero",
            "metadata-plaintext-size-overflow",
            "metadata-digest-alg-not-sha256",
            "metadata-digest-alg-bytes",
            "metadata-digest-short",
            "metadata-digest-text",
            "metadata-trailing-byte",
            "eof-inside-metadata-frame",
            "eof-mid-chunk",
            "payload-absent-after-metadata",
            "footer-bytes-wrong",
            "fill-byte-nonzero",
            "trailing-byte",
            "trailing-byte-keyed",
            "seal-plaintext-size-not-multiple",
            "seal-object-id-too-long",
            "root-key-too-short",
        ],
    );
    assert_envelope_base(&fixture);
    for case in cases(&fixture) {
        assert_envelope_case(case);
    }
}
