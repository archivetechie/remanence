//! RAO manifest-profile CBOR validation and schema checks.

use std::collections::{BTreeMap, BTreeSet};

use ciborium::value::Value as CborValue;
use sha2::{Digest, Sha256};

use crate::error::FormatError;
use crate::layout::{canonical_text_key, xattr_namespace};
use crate::model::{RemTarCborValue, RemTarExtensions, RemTarXattrs, MAX_FILE_ENTRIES};

const MANIFEST_MAX_DEPTH: usize = 8;

pub(crate) fn validate_manifest(
    bytes: &[u8],
    anchor_sha256: &[u8; 32],
    global_pax: &BTreeMap<String, String>,
    reader_chunk_size: usize,
) -> Result<(), FormatError> {
    let digest = Sha256::digest(bytes);
    let mut actual = [0u8; 32];
    actual.copy_from_slice(&digest);
    if &actual != anchor_sha256 {
        return Err(FormatError::ManifestDigestMismatch);
    }

    validate_manifest_profile(bytes)?;
    let value: CborValue =
        ciborium::from_reader(bytes).map_err(|err| FormatError::cbor(err.to_string()))?;
    validate_manifest_schema(&value, global_pax, reader_chunk_size)
}

pub(crate) struct EntryPreservationMetadata {
    pub(crate) xattrs: RemTarXattrs,
    pub(crate) extensions: RemTarExtensions,
}

pub(crate) struct ManifestPreservationMetadata {
    pub(crate) entries: BTreeMap<String, EntryPreservationMetadata>,
    pub(crate) object_extensions: RemTarExtensions,
}

pub(crate) fn manifest_preservation_metadata(
    bytes: &[u8],
) -> Result<ManifestPreservationMetadata, FormatError> {
    validate_manifest_profile(bytes)?;
    let value: CborValue =
        ciborium::from_reader(bytes).map_err(|err| FormatError::cbor(err.to_string()))?;
    let map = value
        .as_map()
        .ok_or_else(|| FormatError::manifest_invalid("top-level manifest item must be a map"))?;
    let object_extensions = extensions_from_container(required_map(map, "object_metadata")?)?;
    let file_entries = required_array(map, "file_entries")?;
    let mut entries = BTreeMap::new();
    for entry in file_entries {
        let entry = entry
            .as_map()
            .ok_or_else(|| FormatError::manifest_invalid("file_entries item must be a map"))?;
        let path = required_text(entry, "path")?;
        let metadata = required_map(entry, "metadata_preservation_data")?;
        let xattrs = xattrs_from_metadata_preservation_data(metadata)?;
        let extensions = extensions_from_container(metadata)?;
        if !xattrs.is_empty() || !extensions.is_empty() {
            entries.insert(
                path.to_string(),
                EntryPreservationMetadata { xattrs, extensions },
            );
        }
    }
    Ok(ManifestPreservationMetadata {
        entries,
        object_extensions,
    })
}

pub(crate) fn validate_manifest_profile(bytes: &[u8]) -> Result<(), FormatError> {
    let mut decoder = ProfileDecoder::new(bytes);
    decoder.skip_item(1)?;
    if decoder.pos != bytes.len() {
        return Err(FormatError::cbor("manifest contains trailing bytes"));
    }
    Ok(())
}

/// Validate only the RAO manifest deterministic-CBOR profile.
///
/// This is exposed solely for the in-tree coverage-guided fuzz harness named
/// by RAO 1.0 Section 14.8. Production readers validate both the profile and
/// the manifest schema through `validate_manifest`.
#[cfg(feature = "fuzzing")]
pub fn validate_manifest_cbor_for_fuzz(bytes: &[u8]) -> Result<(), FormatError> {
    validate_manifest_profile(bytes)
}

fn validate_manifest_schema(
    value: &CborValue,
    global_pax: &BTreeMap<String, String>,
    reader_chunk_size: usize,
) -> Result<(), FormatError> {
    let map = value
        .as_map()
        .ok_or_else(|| FormatError::manifest_invalid("top-level manifest item must be a map"))?;

    let schema_version = required_u64(map, "schema_version")?;
    if schema_version != 1 {
        return Err(FormatError::manifest_invalid(format!(
            "schema_version {schema_version} is not 1"
        )));
    }

    let manifest_chunk_size = required_u64(map, "chunk_size")?;
    if manifest_chunk_size != reader_chunk_size as u64 {
        return Err(FormatError::manifest_invalid(format!(
            "manifest chunk_size {manifest_chunk_size} does not match reader chunk_size {reader_chunk_size}"
        )));
    }
    if let Some(global_chunk_size) = global_pax.get("REMANENCE.chunk_size") {
        let parsed = global_chunk_size
            .parse::<u64>()
            .map_err(|_| FormatError::manifest_invalid("global chunk_size is not decimal"))?;
        if manifest_chunk_size != parsed {
            return Err(FormatError::manifest_invalid(format!(
                "manifest chunk_size {manifest_chunk_size} does not match global chunk_size {parsed}"
            )));
        }
    }

    let manifest_object_id = required_text(map, "object_id")?;
    if let Some(global_object_id) = global_pax.get("REMANENCE.object_id") {
        if manifest_object_id != global_object_id {
            return Err(FormatError::manifest_invalid(
                "manifest object_id does not match global pax",
            ));
        }
    }
    let manifest_caller_object_id = required_text(map, "caller_object_id")?;
    if let Some(global_caller_object_id) = global_pax.get("REMANENCE.caller_object_id") {
        if manifest_caller_object_id != global_caller_object_id {
            return Err(FormatError::manifest_invalid(
                "manifest caller_object_id does not match global pax",
            ));
        }
    }

    let file_entries = required_array(map, "file_entries")?;
    if file_entries.len() > MAX_FILE_ENTRIES {
        return Err(FormatError::manifest_invalid(
            "manifest file_entries exceeds MAX_FILE_ENTRIES",
        ));
    }
    let object_metadata = required_map(map, "object_metadata")?;
    let object_extensions = extensions_from_container(object_metadata)?;
    let mut expected_attribute_namespaces = BTreeSet::new();
    let mut expected_extensions: BTreeSet<String> = object_extensions.keys().cloned().collect();
    let mut seen_paths = BTreeSet::new();
    let mut seen_file_ids = BTreeSet::new();
    let mut seen_regular_paths = BTreeSet::new();
    for entry in file_entries {
        let entry_map = entry
            .as_map()
            .ok_or_else(|| FormatError::manifest_invalid("file_entries item must be a map"))?;
        let path = required_text(entry_map, "path")?;
        if !seen_paths.insert(path.to_string()) {
            return Err(FormatError::manifest_invalid(format!(
                "duplicate file_entries path {path:?}"
            )));
        }
        let file_id = required_text(entry_map, "file_id")?;
        if !seen_file_ids.insert(file_id.to_string()) {
            return Err(FormatError::manifest_invalid(format!(
                "duplicate file_entries file_id {file_id:?}"
            )));
        }
        let validated = validate_file_entry(entry, reader_chunk_size, &seen_regular_paths)?;
        for name in validated.xattrs.keys() {
            if let Some(namespace) = xattr_namespace(name) {
                if namespace != "user" {
                    expected_attribute_namespaces.insert(namespace.to_string());
                }
            }
        }
        expected_extensions.extend(validated.extensions.keys().cloned());
        if let Some(path) = validated.regular_path {
            seen_regular_paths.insert(path);
        }
    }

    validate_inventory(
        object_metadata,
        &expected_attribute_namespaces,
        &expected_extensions,
    )?;
    let _ = required_array(map, "external_references")?;
    Ok(())
}

struct ValidatedFileEntry {
    regular_path: Option<String>,
    xattrs: RemTarXattrs,
    extensions: RemTarExtensions,
}

fn validate_file_entry(
    value: &CborValue,
    reader_chunk_size: usize,
    seen_regular_paths: &BTreeSet<String>,
) -> Result<ValidatedFileEntry, FormatError> {
    let map = value
        .as_map()
        .ok_or_else(|| FormatError::manifest_invalid("file_entries item must be a map"))?;
    let path = required_text(map, "path")?;
    let _ = required_text(map, "file_id")?;
    let size_bytes = required_u64(map, "size_bytes")?;
    let chunk_count = required_u64(map, "chunk_count")?;
    let expected_chunk_count = if size_bytes == 0 {
        0
    } else {
        (size_bytes - 1) / reader_chunk_size as u64 + 1
    };
    if chunk_count != expected_chunk_count {
        return Err(FormatError::manifest_invalid(format!(
            "chunk_count {chunk_count} does not match size_bytes {size_bytes}"
        )));
    }
    let first_chunk_lba = required_u64_or_null(map, "first_chunk_lba")?;
    if size_bytes == 0 {
        if first_chunk_lba.is_some() {
            return Err(FormatError::manifest_invalid(
                "first_chunk_lba must be null when size_bytes is zero",
            ));
        }
    } else if first_chunk_lba.is_none() {
        return Err(FormatError::manifest_invalid(
            "first_chunk_lba must be unsigned when size_bytes is nonzero",
        ));
    }
    let _ = required_bool_or_null(map, "executable")?;
    let metadata = required_map(map, "metadata_preservation_data")?;
    let xattrs = xattrs_from_metadata_preservation_data(metadata)?;
    let extensions = extensions_from_container(metadata)?;
    let entry_type = optional_text(map, "entry_type")?;
    let link_target = optional_text(map, "link_target")?;
    let regular_path = match entry_type {
        None => {
            if link_target.is_some() {
                return Err(FormatError::manifest_invalid(
                    "regular entry must not have link_target",
                ));
            }
            let file_sha256 = optional_bytes(map, "file_sha256")?.ok_or_else(|| {
                FormatError::manifest_invalid("regular entry missing file_sha256")
            })?;
            if file_sha256.len() != 32 {
                return Err(FormatError::manifest_invalid(
                    "file_sha256 must be exactly 32 bytes",
                ));
            }
            Some(path.to_string())
        }
        Some("hardlink") => {
            if !metadata.is_empty() {
                return Err(FormatError::manifest_invalid(
                    "hardlink entry metadata_preservation_data must be empty",
                ));
            }
            if size_bytes != 0 {
                return Err(FormatError::manifest_invalid(
                    "hardlink entry size_bytes must be zero",
                ));
            }
            if optional_bytes(map, "file_sha256")?.is_some() {
                return Err(FormatError::manifest_invalid(
                    "hardlink entry must not have file_sha256",
                ));
            }
            let target = link_target.ok_or_else(|| {
                FormatError::manifest_invalid("hardlink entry missing link_target")
            })?;
            if target.is_empty() {
                return Err(FormatError::manifest_invalid(
                    "hardlink entry link_target must not be empty",
                ));
            }
            if !seen_regular_paths.contains(target) {
                return Err(FormatError::InvalidHardlinkTarget {
                    path: path.to_string(),
                    target: target.to_string(),
                });
            }
            None
        }
        Some("symlink") => {
            if size_bytes != 0 {
                return Err(FormatError::manifest_invalid(
                    "symlink entry size_bytes must be zero",
                ));
            }
            if optional_bytes(map, "file_sha256")?.is_some() {
                return Err(FormatError::manifest_invalid(
                    "symlink entry must not have file_sha256",
                ));
            }
            let target = link_target.ok_or_else(|| {
                FormatError::manifest_invalid("symlink entry missing link_target")
            })?;
            if target.is_empty() {
                return Err(FormatError::manifest_invalid(
                    "symlink entry link_target must not be empty",
                ));
            }
            None
        }
        Some("directory") => {
            if size_bytes != 0 {
                return Err(FormatError::manifest_invalid(
                    "directory entry size_bytes must be zero",
                ));
            }
            if optional_bytes(map, "file_sha256")?.is_some() {
                return Err(FormatError::manifest_invalid(
                    "directory entry must not have file_sha256",
                ));
            }
            if link_target.is_some() {
                return Err(FormatError::manifest_invalid(
                    "directory entry must not have link_target",
                ));
            }
            None
        }
        Some(other) => {
            return Err(FormatError::manifest_invalid(format!(
                "entry_type {other:?} is unsupported"
            )));
        }
    };
    Ok(ValidatedFileEntry {
        regular_path,
        xattrs,
        extensions,
    })
}

fn validate_inventory(
    object_metadata: &[(CborValue, CborValue)],
    expected_attribute_namespaces: &BTreeSet<String>,
    expected_extensions: &BTreeSet<String>,
) -> Result<(), FormatError> {
    let actual_attribute_namespaces = inventory_names(object_metadata, "attribute_namespaces")?;
    let actual_extensions = inventory_names(object_metadata, "extensions")?;
    let expected_attribute_namespaces = canonical_names(expected_attribute_namespaces);
    let expected_extensions = canonical_names(expected_extensions);
    if actual_attribute_namespaces != expected_attribute_namespaces {
        return Err(FormatError::manifest_invalid(
            "object_metadata attribute_namespaces inventory does not match file entries",
        ));
    }
    if actual_extensions != expected_extensions {
        return Err(FormatError::manifest_invalid(
            "object_metadata extensions inventory does not match extension containers",
        ));
    }
    Ok(())
}

fn inventory_names(map: &[(CborValue, CborValue)], key: &str) -> Result<Vec<String>, FormatError> {
    let Some(value) = find_key(map, key) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| FormatError::manifest_invalid(format!("{key} must be an array")))?;
    values
        .iter()
        .map(|value| match value {
            CborValue::Text(value) => Ok(value.clone()),
            _ => Err(FormatError::manifest_invalid(format!(
                "{key} members must be text"
            ))),
        })
        .collect()
}

fn canonical_names(names: &BTreeSet<String>) -> Vec<String> {
    let mut names: Vec<String> = names.iter().cloned().collect();
    names.sort_by_key(|name| canonical_text_key(name));
    names
}

fn find_key<'a>(map: &'a [(CborValue, CborValue)], key: &str) -> Option<&'a CborValue> {
    map.iter().find_map(|(candidate, value)| match candidate {
        CborValue::Text(candidate) if candidate == key => Some(value),
        _ => None,
    })
}

fn required_value<'a>(
    map: &'a [(CborValue, CborValue)],
    key: &str,
) -> Result<&'a CborValue, FormatError> {
    find_key(map, key).ok_or_else(|| FormatError::manifest_invalid(format!("missing key {key}")))
}

fn required_text<'a>(map: &'a [(CborValue, CborValue)], key: &str) -> Result<&'a str, FormatError> {
    match required_value(map, key)? {
        CborValue::Text(value) => Ok(value),
        _ => Err(FormatError::manifest_invalid(format!("{key} must be text"))),
    }
}

fn required_u64(map: &[(CborValue, CborValue)], key: &str) -> Result<u64, FormatError> {
    match required_value(map, key)? {
        CborValue::Integer(value) => (*value)
            .try_into()
            .map_err(|_| FormatError::manifest_invalid(format!("{key} must be unsigned"))),
        _ => Err(FormatError::manifest_invalid(format!(
            "{key} must be unsigned"
        ))),
    }
}

fn required_u64_or_null(
    map: &[(CborValue, CborValue)],
    key: &str,
) -> Result<Option<u64>, FormatError> {
    match required_value(map, key)? {
        CborValue::Integer(value) => (*value)
            .try_into()
            .map(Some)
            .map_err(|_| FormatError::manifest_invalid(format!("{key} must be unsigned or null"))),
        CborValue::Null => Ok(None),
        _ => Err(FormatError::manifest_invalid(format!(
            "{key} must be unsigned or null"
        ))),
    }
}

fn required_bool_or_null(
    map: &[(CborValue, CborValue)],
    key: &str,
) -> Result<Option<bool>, FormatError> {
    match required_value(map, key)? {
        CborValue::Bool(value) => Ok(Some(*value)),
        CborValue::Null => Ok(None),
        _ => Err(FormatError::manifest_invalid(format!(
            "{key} must be bool or null"
        ))),
    }
}

fn required_array<'a>(
    map: &'a [(CborValue, CborValue)],
    key: &str,
) -> Result<&'a [CborValue], FormatError> {
    match required_value(map, key)? {
        CborValue::Array(value) => Ok(value),
        _ => Err(FormatError::manifest_invalid(format!(
            "{key} must be array"
        ))),
    }
}

fn required_map<'a>(
    map: &'a [(CborValue, CborValue)],
    key: &str,
) -> Result<&'a [(CborValue, CborValue)], FormatError> {
    match required_value(map, key)? {
        CborValue::Map(value) => Ok(value),
        _ => Err(FormatError::manifest_invalid(format!("{key} must be map"))),
    }
}

fn xattrs_from_metadata_preservation_data(
    map: &[(CborValue, CborValue)],
) -> Result<RemTarXattrs, FormatError> {
    let Some(value) = find_key(map, "xattrs") else {
        return Ok(BTreeMap::new());
    };
    let entries = value
        .as_map()
        .ok_or_else(|| FormatError::manifest_invalid("xattrs must be a map"))?;
    let mut xattrs = BTreeMap::new();
    for (key, value) in entries {
        let name = match key {
            CborValue::Text(name) => name,
            _ => return Err(FormatError::manifest_invalid("xattr names must be text")),
        };
        if name.is_empty() {
            return Err(FormatError::manifest_invalid(
                "xattr names must not be empty",
            ));
        }
        if name.bytes().any(|byte| byte < 0x20) {
            return Err(FormatError::manifest_invalid(
                "xattr names must not contain ASCII control characters",
            ));
        }
        let value = match value {
            CborValue::Bytes(bytes) => bytes,
            _ => return Err(FormatError::manifest_invalid("xattr values must be bytes")),
        };
        xattrs.insert(name.clone(), value.clone());
    }
    Ok(xattrs)
}

fn extensions_from_container(
    map: &[(CborValue, CborValue)],
) -> Result<RemTarExtensions, FormatError> {
    let Some(value) = find_key(map, "ext") else {
        return Ok(BTreeMap::new());
    };
    let entries = value
        .as_map()
        .ok_or_else(|| FormatError::manifest_invalid("ext must be a map"))?;
    let mut extensions = BTreeMap::new();
    for (key, value) in entries {
        let name = match key {
            CborValue::Text(name) => name,
            _ => return Err(FormatError::cbor("extension names must be text")),
        };
        extensions.insert(name.clone(), profile_value(value)?);
    }
    Ok(extensions)
}

fn profile_value(value: &CborValue) -> Result<RemTarCborValue, FormatError> {
    match value {
        CborValue::Integer(value) => (*value)
            .try_into()
            .map(RemTarCborValue::Unsigned)
            .map_err(|_| FormatError::cbor("extension integer must be unsigned")),
        CborValue::Bytes(value) => Ok(RemTarCborValue::Bytes(value.clone())),
        CborValue::Text(value) => Ok(RemTarCborValue::Text(value.clone())),
        CborValue::Bool(value) => Ok(RemTarCborValue::Bool(*value)),
        CborValue::Null => Ok(RemTarCborValue::Null),
        CborValue::Array(values) => values
            .iter()
            .map(profile_value)
            .collect::<Result<Vec<_>, _>>()
            .map(RemTarCborValue::Array),
        CborValue::Map(values) => {
            let mut map = BTreeMap::new();
            for (key, value) in values {
                let key = match key {
                    CborValue::Text(key) => key,
                    _ => return Err(FormatError::cbor("extension map keys must be text")),
                };
                map.insert(key.clone(), profile_value(value)?);
            }
            Ok(RemTarCborValue::Map(map))
        }
        _ => Err(FormatError::cbor(
            "extension contains a value outside the manifest profile",
        )),
    }
}

fn optional_bytes<'a>(
    map: &'a [(CborValue, CborValue)],
    key: &str,
) -> Result<Option<&'a [u8]>, FormatError> {
    match find_key(map, key) {
        Some(CborValue::Bytes(value)) => Ok(Some(value)),
        Some(_) => Err(FormatError::manifest_invalid(format!(
            "{key} must be bytes"
        ))),
        None => Ok(None),
    }
}

fn optional_text<'a>(
    map: &'a [(CborValue, CborValue)],
    key: &str,
) -> Result<Option<&'a str>, FormatError> {
    match find_key(map, key) {
        Some(CborValue::Text(value)) => Ok(Some(value)),
        Some(_) => Err(FormatError::manifest_invalid(format!("{key} must be text"))),
        None => Ok(None),
    }
}

struct ProfileDecoder<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ProfileDecoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn skip_item(&mut self, depth: usize) -> Result<(), FormatError> {
        if depth > MANIFEST_MAX_DEPTH {
            return Err(FormatError::cbor("manifest nesting depth exceeds limit"));
        }
        let (major, len, _encoding) = self.read_type_len()?;
        self.skip_item_payload(depth, major, len)
    }

    fn skip_item_payload(&mut self, depth: usize, major: u8, len: u64) -> Result<(), FormatError> {
        match major {
            0 => Ok(()),
            2 => {
                self.take_len(len)?;
                Ok(())
            }
            3 => {
                let bytes = self.take_len(len)?;
                std::str::from_utf8(bytes)
                    .map(|_| ())
                    .map_err(|_| FormatError::cbor("manifest text string is not UTF-8"))
            }
            4 => {
                for _ in 0..len {
                    self.skip_item(depth + 1)?;
                }
                Ok(())
            }
            5 => self.skip_map(len, depth),
            7 if matches!(len, 20..=22) => Ok(()),
            _ => Err(FormatError::cbor("manifest contains disallowed CBOR item")),
        }
    }

    fn skip_map(&mut self, len: u64, depth: usize) -> Result<(), FormatError> {
        let mut previous_key = None::<Vec<u8>>;
        for _ in 0..len {
            let key_start = self.pos;
            let (major, key_len, _encoding) = self.read_type_len()?;
            if major != 3 {
                return Err(FormatError::cbor("manifest map key is not text"));
            }
            let key = self.take_len(key_len)?;
            std::str::from_utf8(key)
                .map_err(|_| FormatError::cbor("manifest map key is not UTF-8"))?;
            let key_bytes = self.bytes[key_start..self.pos].to_vec();
            if previous_key
                .as_ref()
                .is_some_and(|previous| previous >= &key_bytes)
            {
                return Err(FormatError::cbor(
                    "manifest map keys are not in deterministic order",
                ));
            }
            let is_top_level_file_entries = depth == 1 && key == b"file_entries";
            previous_key = Some(key_bytes);
            let (value_major, value_len, _encoding) = self.read_type_len()?;
            if is_top_level_file_entries {
                if value_major != 4 {
                    return Err(FormatError::manifest_invalid("file_entries must be array"));
                }
                if value_len > MAX_FILE_ENTRIES as u64 {
                    return Err(FormatError::manifest_invalid(
                        "manifest file_entries exceeds MAX_FILE_ENTRIES",
                    ));
                }
            }
            self.skip_item_payload(depth + 1, value_major, value_len)?;
        }
        Ok(())
    }

    fn read_type_len(&mut self) -> Result<(u8, u64, Vec<u8>), FormatError> {
        let start = self.pos;
        let first = self.take_one()?;
        let major = first >> 5;
        let ai = first & 0x1f;
        let value = match ai {
            0..=23 => u64::from(ai),
            24 => {
                let value = u64::from(self.take_one()?);
                if value < 24 {
                    return Err(FormatError::cbor("manifest integer/length is not shortest"));
                }
                value
            }
            25 => {
                let value = u64::from(u16::from_be_bytes(self.take_array::<2>()?));
                if value <= 0xff {
                    return Err(FormatError::cbor("manifest integer/length is not shortest"));
                }
                value
            }
            26 => {
                let value = u64::from(u32::from_be_bytes(self.take_array::<4>()?));
                if value <= 0xffff {
                    return Err(FormatError::cbor("manifest integer/length is not shortest"));
                }
                value
            }
            27 => {
                let value = u64::from_be_bytes(self.take_array::<8>()?);
                if value <= 0xffff_ffff {
                    return Err(FormatError::cbor("manifest integer/length is not shortest"));
                }
                value
            }
            _ => return Err(FormatError::cbor("manifest uses invalid additional info")),
        };
        Ok((major, value, self.bytes[start..self.pos].to_vec()))
    }

    fn take_len(&mut self, len: u64) -> Result<&'a [u8], FormatError> {
        let len =
            usize::try_from(len).map_err(|_| FormatError::cbor("manifest length too large"))?;
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| FormatError::cbor("manifest offset overflow"))?;
        let bytes = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| FormatError::cbor("manifest item is truncated"))?;
        self.pos = end;
        Ok(bytes)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], FormatError> {
        let bytes = self.take_len(N as u64)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    fn take_one(&mut self) -> Result<u8, FormatError> {
        let byte = *self
            .bytes
            .get(self.pos)
            .ok_or_else(|| FormatError::cbor("manifest item is truncated"))?;
        self.pos += 1;
        Ok(byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/rao/negative-manifest.json"))
            .expect("manifest negative fixture is valid JSON")
    }

    fn cases(fixture: &Value) -> &[Value] {
        fixture
            .get("cases")
            .and_then(Value::as_array)
            .expect("manifest negative fixture cases exist")
    }

    fn assert_complete_case_ids(fixture: &Value, expected: &[&str]) {
        assert_eq!(str_field(fixture, "status"), Some("complete"));
        let actual = cases(fixture)
            .iter()
            .map(|case| str_field(case, "id").expect("case id exists"))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
        value.get(key).and_then(Value::as_str)
    }

    fn cbor_type_len(major: u8, value: u64) -> Vec<u8> {
        let mut out = Vec::new();
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
        out
    }

    fn cbor_uint(value: u64) -> Vec<u8> {
        cbor_type_len(0, value)
    }

    fn cbor_null() -> Vec<u8> {
        vec![0xf6]
    }

    fn cbor_bool(value: bool) -> Vec<u8> {
        vec![if value { 0xf5 } else { 0xf4 }]
    }

    fn cbor_bytes(bytes: &[u8]) -> Vec<u8> {
        let mut out = cbor_type_len(2, bytes.len() as u64);
        out.extend_from_slice(bytes);
        out
    }

    fn cbor_text(text: &str) -> Vec<u8> {
        let mut out = cbor_type_len(3, text.len() as u64);
        out.extend_from_slice(text.as_bytes());
        out
    }

    fn cbor_array(items: Vec<Vec<u8>>) -> Vec<u8> {
        let mut out = cbor_type_len(4, items.len() as u64);
        for item in items {
            out.extend_from_slice(&item);
        }
        out
    }

    fn cbor_map(mut items: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
        items.sort_by_key(|(key, _)| cbor_text(key));
        let mut out = cbor_type_len(5, items.len() as u64);
        for (key, value) in items {
            out.extend_from_slice(&cbor_text(key));
            out.extend_from_slice(&value);
        }
        out
    }

    fn base_manifest_with(chunk_size: u64, extra: Vec<(&'static str, Vec<u8>)>) -> Vec<u8> {
        let mut fields = vec![
            ("object_id", cbor_text("object-1")),
            ("chunk_size", cbor_uint(chunk_size)),
            ("file_entries", cbor_array(Vec::new())),
            ("schema_version", cbor_uint(1)),
            ("object_metadata", cbor_map(Vec::new())),
            ("caller_object_id", cbor_text("caller-1")),
            ("external_references", cbor_array(Vec::new())),
        ];
        fields.extend(extra);
        cbor_map(fields)
    }

    fn manifest_with_file_entries(entries: Vec<Vec<u8>>) -> Vec<u8> {
        manifest_with_file_entries_and_object_metadata(entries, cbor_map(Vec::new()))
    }

    fn manifest_with_file_entries_and_object_metadata(
        entries: Vec<Vec<u8>>,
        object_metadata: Vec<u8>,
    ) -> Vec<u8> {
        cbor_map(vec![
            ("object_id", cbor_text("object-1")),
            ("chunk_size", cbor_uint(512)),
            ("file_entries", cbor_array(entries)),
            ("schema_version", cbor_uint(1)),
            ("object_metadata", object_metadata),
            ("caller_object_id", cbor_text("caller-1")),
            ("external_references", cbor_array(Vec::new())),
        ])
    }

    fn file_entry_with_bad_hash() -> Vec<u8> {
        cbor_map(vec![
            ("path", cbor_text("a.bin")),
            ("file_id", cbor_text("file-a")),
            ("executable", vec![0xf6]),
            ("size_bytes", cbor_uint(1)),
            ("chunk_count", cbor_uint(1)),
            ("file_sha256", cbor_bytes(&[0x11; 31])),
            ("first_chunk_lba", cbor_uint(1)),
            ("metadata_preservation_data", cbor_map(Vec::new())),
        ])
    }

    fn regular_file_entry_with(mut extra: Vec<(&'static str, Vec<u8>)>) -> Vec<u8> {
        let mut fields = vec![
            ("path", cbor_text("a.bin")),
            ("file_id", cbor_text("file-a")),
            ("executable", cbor_null()),
            ("size_bytes", cbor_uint(1)),
            ("chunk_count", cbor_uint(1)),
            ("file_sha256", cbor_bytes(&[0x11; 32])),
            ("first_chunk_lba", cbor_uint(1)),
            ("metadata_preservation_data", cbor_map(Vec::new())),
        ];
        for (key, value) in extra.drain(..) {
            if let Some((_, existing)) = fields
                .iter_mut()
                .find(|(existing_key, _)| *existing_key == key)
            {
                *existing = value;
            } else {
                fields.push((key, value));
            }
        }
        cbor_map(fields)
    }

    fn nonregular_file_entry(
        entry_type: &str,
        link_target: Option<&str>,
        extra: Vec<(&'static str, Vec<u8>)>,
    ) -> Vec<u8> {
        let mut fields = vec![
            ("path", cbor_text("entry")),
            ("file_id", cbor_text("file-a")),
            ("entry_type", cbor_text(entry_type)),
            ("executable", cbor_null()),
            ("size_bytes", cbor_uint(0)),
            ("chunk_count", cbor_uint(0)),
            ("first_chunk_lba", cbor_null()),
            ("metadata_preservation_data", cbor_map(Vec::new())),
        ];
        if let Some(target) = link_target {
            fields.push(("link_target", cbor_text(target)));
        }
        for (key, value) in extra {
            if let Some((_, existing)) = fields
                .iter_mut()
                .find(|(existing_key, _)| *existing_key == key)
            {
                *existing = value;
            } else {
                fields.push((key, value));
            }
        }
        cbor_map(fields)
    }

    fn assert_file_entry_manifest_invalid(entry: Vec<u8>) {
        let bytes = manifest_with_file_entries(vec![entry]);
        let anchor = sha256_array(&bytes);
        let err = validate_manifest(&bytes, &anchor, &global_pax(), 512).unwrap_err();
        assert!(matches!(err, FormatError::ManifestInvalid(_)), "{err}");
    }

    fn manifest_case(id: &str) -> Vec<u8> {
        match id {
            "non-canonical-key-order" => {
                let mut out = cbor_type_len(5, 2);
                out.extend_from_slice(&cbor_text("chunk_size"));
                out.extend_from_slice(&cbor_uint(512));
                out.extend_from_slice(&cbor_text("object_id"));
                out.extend_from_slice(&cbor_text("object-1"));
                out
            }
            "non-shortest-integer" => {
                let mut out = cbor_type_len(5, 7);
                for (key, value) in [
                    ("object_id", cbor_text("object-1")),
                    ("chunk_size", cbor_uint(512)),
                    ("file_entries", cbor_array(Vec::new())),
                    ("schema_version", vec![0x18, 0x01]),
                    ("object_metadata", cbor_map(Vec::new())),
                    ("caller_object_id", cbor_text("caller-1")),
                    ("external_references", cbor_array(Vec::new())),
                ] {
                    out.extend_from_slice(&cbor_text(key));
                    out.extend_from_slice(&value);
                }
                out
            }
            "indefinite-length-item" => {
                base_manifest_with(512, vec![("zzzzzzzzzzzzzzzzzzzz", vec![0x5f, 0xff])])
            }
            "float-item" => {
                base_manifest_with(512, vec![("zzzzzzzzzzzzzzzzzzzz", vec![0xf9, 0x3c, 0x00])])
            }
            "tag-item" => base_manifest_with(512, vec![("zzzzzzzzzzzzzzzzzzzz", vec![0xc0, 0x00])]),
            "duplicate-map-key" => {
                let mut out = cbor_type_len(5, 2);
                out.extend_from_slice(&cbor_text("object_id"));
                out.extend_from_slice(&cbor_text("object-1"));
                out.extend_from_slice(&cbor_text("object_id"));
                out.extend_from_slice(&cbor_text("object-1"));
                out
            }
            "schema-version-2" => {
                let mut fields = vec![
                    ("object_id", cbor_text("object-1")),
                    ("chunk_size", cbor_uint(512)),
                    ("file_entries", cbor_array(Vec::new())),
                    ("schema_version", cbor_uint(2)),
                    ("object_metadata", cbor_map(Vec::new())),
                    ("caller_object_id", cbor_text("caller-1")),
                    ("external_references", cbor_array(Vec::new())),
                ];
                cbor_map(std::mem::take(&mut fields))
            }
            "file-sha256-wrong-length" => {
                manifest_with_file_entries(vec![file_entry_with_bad_hash()])
            }
            "nesting-depth-over-max" => {
                let mut nested = vec![0xf6];
                for _ in 0..8 {
                    nested = cbor_array(vec![nested]);
                }
                base_manifest_with(512, vec![("zzzzzzzzzzzzzzzzzzzz", nested)])
            }
            "manifest-digest-mismatch" | "unknown-extra-key-accepted" => {
                base_manifest_with(512, vec![("zzzzzzzzzzzzzzzzzzzz", cbor_uint(1))])
            }
            "manifest-chunk-size-mismatch" => base_manifest_with(1024, Vec::new()),
            "duplicate-entry-path" => manifest_with_file_entries(vec![
                regular_file_entry_with(Vec::new()),
                regular_file_entry_with(vec![("file_id", cbor_text("file-b"))]),
            ]),
            "duplicate-entry-file-id" => manifest_with_file_entries(vec![
                regular_file_entry_with(Vec::new()),
                regular_file_entry_with(vec![("path", cbor_text("b.bin"))]),
            ]),
            other => panic!("unhandled manifest vector {other:?}"),
        }
    }

    fn global_pax() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("REMANENCE.chunk_size".to_string(), "512".to_string()),
            ("REMANENCE.object_id".to_string(), "object-1".to_string()),
            (
                "REMANENCE.caller_object_id".to_string(),
                "caller-1".to_string(),
            ),
        ])
    }

    fn sha256_array(bytes: &[u8]) -> [u8; 32] {
        let digest = Sha256::digest(bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }

    fn error_name(error: &FormatError) -> &'static str {
        match error {
            FormatError::Cbor(_) => "Cbor",
            FormatError::ManifestInvalid(_) => "ManifestInvalid",
            FormatError::ManifestDigestMismatch => "ManifestDigestMismatch",
            _ => "Other",
        }
    }

    #[test]
    fn manifest_profile_rejects_oversized_file_entries_before_walking_array() {
        let mut huge_file_entries = cbor_type_len(4, MAX_FILE_ENTRIES as u64 + 1);
        let bytes = cbor_map(vec![(
            "file_entries",
            std::mem::take(&mut huge_file_entries),
        )]);
        let err = validate_manifest_profile(&bytes).unwrap_err();
        assert!(matches!(err, FormatError::ManifestInvalid(_)), "{err}");
    }

    #[test]
    fn manifest_negative_vectors_match_manifest_errors() {
        let fixture = fixture();
        assert_complete_case_ids(
            &fixture,
            &[
                "non-canonical-key-order",
                "non-shortest-integer",
                "indefinite-length-item",
                "float-item",
                "tag-item",
                "duplicate-map-key",
                "schema-version-2",
                "file-sha256-wrong-length",
                "nesting-depth-over-max",
                "manifest-digest-mismatch",
                "manifest-chunk-size-mismatch",
                "duplicate-entry-path",
                "duplicate-entry-file-id",
                "unknown-extra-key-accepted",
            ],
        );
        for case in cases(&fixture) {
            let id = str_field(case, "id").expect("case id exists");
            let bytes = manifest_case(id);
            let mut anchor = sha256_array(&bytes);
            if id == "manifest-digest-mismatch" {
                anchor[0] ^= 1;
            }
            let result = validate_manifest(&bytes, &anchor, &global_pax(), 512);
            if str_field(case, "expected_outcome") == Some("accepted") {
                result.unwrap_or_else(|err| panic!("{id}: expected accepted, got {err}"));
            } else {
                let err = result.unwrap_err();
                assert_eq!(
                    error_name(&err),
                    str_field(case, "expected_error").expect("expected_error exists"),
                    "{id}: {err}"
                );
            }
        }
    }

    #[test]
    fn manifest_rejects_invalid_file_entry_schema() {
        assert_file_entry_manifest_invalid(regular_file_entry_with(vec![(
            "chunk_count",
            cbor_uint(2),
        )]));
        assert_file_entry_manifest_invalid(regular_file_entry_with(vec![(
            "first_chunk_lba",
            cbor_null(),
        )]));
        assert_file_entry_manifest_invalid(regular_file_entry_with(vec![(
            "executable",
            cbor_text("yes"),
        )]));
        assert_file_entry_manifest_invalid(cbor_map(vec![
            ("path", cbor_text("a.bin")),
            ("file_id", cbor_text("file-a")),
            ("executable", cbor_bool(false)),
            ("size_bytes", cbor_uint(1)),
            ("chunk_count", cbor_uint(1)),
            ("first_chunk_lba", cbor_uint(1)),
            ("metadata_preservation_data", cbor_map(Vec::new())),
        ]));
        assert_file_entry_manifest_invalid(regular_file_entry_with(vec![(
            "link_target",
            cbor_text("target"),
        )]));
        assert_file_entry_manifest_invalid(nonregular_file_entry("symlink", None, Vec::new()));
        assert_file_entry_manifest_invalid(nonregular_file_entry(
            "symlink",
            Some("target"),
            vec![("file_sha256", cbor_bytes(&[0x11; 32]))],
        ));
        assert_file_entry_manifest_invalid(nonregular_file_entry(
            "directory",
            None,
            vec![("link_target", cbor_text("target"))],
        ));
        assert_file_entry_manifest_invalid(nonregular_file_entry("device", None, Vec::new()));
        assert_file_entry_manifest_invalid(regular_file_entry_with(vec![(
            "metadata_preservation_data",
            cbor_map(vec![(
                "xattrs",
                cbor_map(vec![("bad\nname", cbor_bytes(b"value"))]),
            )]),
        )]));
        assert_file_entry_manifest_invalid(nonregular_file_entry(
            "hardlink",
            Some("a.bin"),
            vec![(
                "metadata_preservation_data",
                cbor_map(vec![(
                    "xattrs",
                    cbor_map(vec![("user.note", cbor_bytes(b"value"))]),
                )]),
            )],
        ));
    }

    #[test]
    fn manifest_rejects_inventory_that_disagrees_with_entries() {
        let entry = regular_file_entry_with(vec![(
            "metadata_preservation_data",
            cbor_map(vec![(
                "xattrs",
                cbor_map(vec![("security.selinux", cbor_bytes(b"secret"))]),
            )]),
        )]);
        let object_metadata = cbor_map(vec![(
            "attribute_namespaces",
            cbor_array(vec![cbor_text("trusted")]),
        )]);
        let bytes = manifest_with_file_entries_and_object_metadata(vec![entry], object_metadata);
        let anchor = sha256_array(&bytes);

        let err = validate_manifest(&bytes, &anchor, &global_pax(), 512).unwrap_err();

        assert!(matches!(err, FormatError::ManifestInvalid(_)), "{err}");
    }

    #[test]
    fn manifest_requires_ext_containers_to_be_maps() {
        let entry = regular_file_entry_with(vec![(
            "metadata_preservation_data",
            cbor_map(vec![("ext", cbor_uint(1))]),
        )]);
        let bytes = manifest_with_file_entries(vec![entry]);
        let anchor = sha256_array(&bytes);
        let err = validate_manifest(&bytes, &anchor, &global_pax(), 512).unwrap_err();
        assert!(matches!(err, FormatError::ManifestInvalid(_)), "{err}");

        let bytes = manifest_with_file_entries_and_object_metadata(
            Vec::new(),
            cbor_map(vec![("ext", cbor_null())]),
        );
        let anchor = sha256_array(&bytes);
        let err = validate_manifest(&bytes, &anchor, &global_pax(), 512).unwrap_err();
        assert!(matches!(err, FormatError::ManifestInvalid(_)), "{err}");
    }

    #[test]
    fn manifest_profile_rejects_noncanonical_ext_member_value() {
        let extension_name = "org.example.opaque";
        let mut ext = cbor_type_len(5, 1);
        ext.extend_from_slice(&cbor_text(extension_name));
        ext.extend_from_slice(&[0x18, 0x01]);
        let object_metadata = cbor_map(vec![
            ("ext", ext),
            ("extensions", cbor_array(vec![cbor_text(extension_name)])),
        ]);
        let bytes = manifest_with_file_entries_and_object_metadata(Vec::new(), object_metadata);
        let anchor = sha256_array(&bytes);

        let err = validate_manifest(&bytes, &anchor, &global_pax(), 512).unwrap_err();

        assert!(matches!(err, FormatError::Cbor(_)), "{err}");
    }

    #[test]
    fn reserved_bare_metadata_keys_and_empty_inventory_arrays_are_accepted() {
        let entry = regular_file_entry_with(vec![(
            "metadata_preservation_data",
            cbor_map(vec![("future_entry_field", cbor_uint(1))]),
        )]);
        let object_metadata = cbor_map(vec![
            ("future_object_field", cbor_bool(true)),
            ("attribute_namespaces", cbor_array(Vec::new())),
            ("extensions", cbor_array(Vec::new())),
        ]);
        let bytes = manifest_with_file_entries_and_object_metadata(vec![entry], object_metadata);
        let anchor = sha256_array(&bytes);

        validate_manifest(&bytes, &anchor, &global_pax(), 512).unwrap();
    }

    #[test]
    fn malformed_extension_name_is_carry_only_not_invalid() {
        let extension_name = "Uppercase.Invalid";
        let entry = regular_file_entry_with(vec![(
            "metadata_preservation_data",
            cbor_map(vec![(
                "ext",
                cbor_map(vec![(extension_name, cbor_bytes(b"opaque"))]),
            )]),
        )]);
        let object_metadata = cbor_map(vec![(
            "extensions",
            cbor_array(vec![cbor_text(extension_name)]),
        )]);
        let bytes = manifest_with_file_entries_and_object_metadata(vec![entry], object_metadata);
        let anchor = sha256_array(&bytes);

        validate_manifest(&bytes, &anchor, &global_pax(), 512).unwrap();
        let preservation = manifest_preservation_metadata(&bytes).unwrap();
        assert_eq!(
            preservation.entries["a.bin"].extensions[extension_name],
            RemTarCborValue::Bytes(b"opaque".to_vec())
        );
    }
}
