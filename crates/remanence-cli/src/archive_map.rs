//! Source-map frontend for `rem archive build --map`.
//!
//! The source map is a trusted planner's byte-for-byte member contract for a
//! RAO archive build. This module validates that contract, anchors every source
//! file under a canonical root, and then produces the same regular-file build
//! inputs used by the existing archive writer.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use remanence_format::RemTarEntryType;
use sha2::{Digest, Sha256};

use crate::{
    archive_ingest, bytes_to_hex, deterministic_archive_entry_file_id, ArchiveBuildInputFile,
};

const SOURCE_MAP_HEADER: &str = "archive_path\tsource_path\tsha256\tsize\tingest_item_id";

#[derive(Debug, Clone)]
pub(crate) struct SourceMapBuildInputs {
    pub(crate) inputs: Vec<ArchiveBuildInputFile>,
    pub(crate) map_sha256: [u8; 32],
    pub(crate) manifest: archive_ingest::CustomerManifest,
}

pub(crate) fn load_source_map(
    map_path: &Path,
    source_root: &Path,
    expected_map_sha256: Option<&str>,
) -> Result<SourceMapBuildInputs, String> {
    let bytes = fs::read(map_path)
        .map_err(|error| format!("read --map {}: {error}", map_path.display()))?;
    let map_sha256 = sha256_bytes(&bytes);
    if let Some(expected) = expected_map_sha256 {
        let expected = parse_expected_map_sha256(expected)?;
        if expected != map_sha256 {
            return Err(format!(
                "--map-sha256 mismatch for {}: expected {}, got {}",
                map_path.display(),
                bytes_to_hex(&expected),
                bytes_to_hex(&map_sha256)
            ));
        }
    }

    let text = std::str::from_utf8(&bytes)
        .map_err(|error| format!("source map {} is not UTF-8: {error}", map_path.display()))?;
    if !text.ends_with('\n') {
        return Err(format!(
            "source map {} must end with a trailing LF newline",
            map_path.display()
        ));
    }

    let canonical_root = fs::canonicalize(source_root).map_err(|error| {
        format!(
            "canonicalize --source-root {}: {error}",
            source_root.display()
        )
    })?;
    let root_metadata = fs::metadata(&canonical_root).map_err(|error| {
        format!(
            "stat canonical --source-root {}: {error}",
            canonical_root.display()
        )
    })?;
    if !root_metadata.is_dir() {
        return Err(format!(
            "--source-root {} is not a directory",
            canonical_root.display()
        ));
    }

    let mut lines = text.split_terminator('\n');
    let header = lines
        .next()
        .ok_or_else(|| format!("source map {} is empty", map_path.display()))?;
    if header != SOURCE_MAP_HEADER {
        return Err(format!(
            "source map {} header must be exactly {:?}",
            map_path.display(),
            SOURCE_MAP_HEADER
        ));
    }

    let mut inputs = Vec::new();
    for (offset, line) in lines.enumerate() {
        let line_number = offset + 2;
        inputs.push(parse_source_map_row(line, line_number, &canonical_root)?);
    }
    if inputs.is_empty() {
        return Err(format!(
            "source map {} did not contain any member rows",
            map_path.display()
        ));
    }
    archive_ingest::ensure_unique_archive_paths(&inputs)?;

    let manifest = source_map_customer_manifest(&inputs);
    Ok(SourceMapBuildInputs {
        inputs,
        map_sha256,
        manifest,
    })
}

fn parse_source_map_row(
    line: &str,
    line_number: usize,
    canonical_root: &Path,
) -> Result<ArchiveBuildInputFile, String> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() != 5 {
        return Err(format!(
            "source map line {line_number} must have 5 TAB-separated columns, got {}",
            fields.len()
        ));
    }
    for field in &fields {
        reject_control_characters(field, line_number)?;
    }

    let archive_path = validate_archive_path_field(fields[0], line_number)?;
    let source_path =
        validate_source_path_field(fields[1], fields[3], line_number, canonical_root)?;
    let file_sha256 = parse_lowercase_sha256(fields[2], line_number)?;
    let size_bytes = parse_size(fields[3], line_number)?;
    let file_id = deterministic_archive_entry_file_id(
        RemTarEntryType::Regular,
        &archive_path,
        Some(&file_sha256),
        None,
    );

    Ok(ArchiveBuildInputFile {
        source_path,
        entry_type: RemTarEntryType::Regular,
        archive_path,
        file_id,
        size_bytes,
        file_sha256: Some(file_sha256),
        link_target: None,
        xattrs: BTreeMap::new(),
        ingest_item_id: Some(fields[4].to_string()),
    })
}

fn reject_control_characters(field: &str, line_number: usize) -> Result<(), String> {
    if let Some(character) = field.chars().find(|character| character.is_control()) {
        return Err(format!(
            "source map line {line_number} contains control character U+{:04X}",
            character as u32
        ));
    }
    Ok(())
}

fn validate_archive_path_field(value: &str, line_number: usize) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!(
            "source map line {line_number} archive_path must not be empty"
        ));
    }
    if value.starts_with('/') || value.ends_with('/') {
        return Err(format!(
            "source map line {line_number} archive_path {value:?} must be relative and must not end with /"
        ));
    }
    for component in value.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(format!(
                "source map line {line_number} archive_path {value:?} contains invalid component {component:?}"
            ));
        }
    }
    Ok(value.to_string())
}

fn validate_source_path_field(
    source_path: &str,
    size_text: &str,
    line_number: usize,
    canonical_root: &Path,
) -> Result<PathBuf, String> {
    let declared_size = parse_size(size_text, line_number)?;
    let source = PathBuf::from(source_path);
    if !source.is_absolute() {
        return Err(format!(
            "source map line {line_number} source_path {source_path:?} must be absolute"
        ));
    }
    let canonical_source = fs::canonicalize(&source).map_err(|error| {
        format!(
            "source map line {line_number} canonicalize source_path {}: {error}",
            source.display()
        )
    })?;
    if !canonical_source.starts_with(canonical_root) {
        return Err(format!(
            "source map line {line_number} source_path {} escapes --source-root {}",
            canonical_source.display(),
            canonical_root.display()
        ));
    }
    let metadata = fs::metadata(&canonical_source).map_err(|error| {
        format!(
            "source map line {line_number} stat source_path {}: {error}",
            canonical_source.display()
        )
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "source map line {line_number} source_path {} is not a regular file",
            canonical_source.display()
        ));
    }
    if metadata.len() != declared_size {
        return Err(format!(
            "source map line {line_number} size mismatch for {}: map says {}, filesystem says {}",
            canonical_source.display(),
            declared_size,
            metadata.len()
        ));
    }
    Ok(canonical_source)
}

fn parse_size(value: &str, line_number: usize) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "source map line {line_number} size {value:?} must be a decimal u64"
        ));
    }
    value.parse::<u64>().map_err(|error| {
        format!("source map line {line_number} size {value:?} must be a decimal u64: {error}")
    })
}

fn parse_lowercase_sha256(value: &str, line_number: usize) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err(format!(
            "source map line {line_number} sha256 must be exactly 64 lowercase hex characters"
        ));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!(
            "source map line {line_number} sha256 must use lowercase hex"
        ));
    }
    decode_sha256_hex(value, true).map_err(|error| {
        format!("source map line {line_number} sha256 must be lowercase hex: {error}")
    })
}

fn parse_expected_map_sha256(value: &str) -> Result<[u8; 32], String> {
    let token = value
        .split_whitespace()
        .next()
        .ok_or_else(|| "--map-sha256 must contain a 64-character hex digest".to_string())?;
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("--map-sha256 must contain a 64-character hex digest".to_string());
    }
    decode_sha256_hex(token, false)
        .map_err(|error| format!("--map-sha256 must contain a valid hex digest: {error}"))
}

fn decode_sha256_hex(value: &str, lowercase_only: bool) -> Result<[u8; 32], String> {
    let mut out = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0], lowercase_only)?;
        let low = hex_nibble(pair[1], lowercase_only)?;
        out[index] = (high << 4) | low;
    }
    Ok(out)
}

fn hex_nibble(byte: u8, lowercase_only: bool) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' if !lowercase_only => Ok(byte - b'A' + 10),
        _ => Err(format!("invalid hex character {:?}", char::from(byte))),
    }
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn source_map_customer_manifest(
    inputs: &[ArchiveBuildInputFile],
) -> archive_ingest::CustomerManifest {
    archive_ingest::CustomerManifest {
        format: "remanence-customer-manifest-v1",
        ruleset: None,
        tar_engine: archive_ingest::TarEngineReport {
            program: "source-map".to_string(),
            version: "1.0".to_string(),
            create_invocation: vec![
                "rem".to_string(),
                "archive".to_string(),
                "build".to_string(),
                "--map".to_string(),
                "<source-map.tsv>".to_string(),
            ],
            extract_invocation: Vec::new(),
        },
        entries: inputs
            .iter()
            .map(|input| archive_ingest::CustomerManifestEntry {
                path: input.archive_path.clone(),
                kind: "regular",
                size_bytes: input.size_bytes,
                sha256: input.file_sha256.map(|hash| bytes_to_hex(&hash)),
                mtime: None,
                wrapper: None,
            })
            .collect(),
        exclusions: Vec::new(),
    }
}
