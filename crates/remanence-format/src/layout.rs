//! Counting-mode `rao-v1` layout planner.

use std::collections::{BTreeMap, BTreeSet};

use ciborium::value::Value as CborValue;
use sha2::{Digest, Sha256};

use crate::error::FormatError;
use crate::manifest::validate_manifest_profile;
use crate::model::{
    BodyLba, RemTarCborValue, RemTarEntryType, RemTarExtensions, RemTarFileLayout, RemTarFileSpec,
    RemTarObjectOptions, FORMAT_ID, MANIFEST_PATH, MAX_FILE_ENTRIES, SCHEMA_VERSION,
    SCHEMA_VERSION_XATTRS, TAR_RECORD_SIZE,
};
use crate::pax::{
    encode_pax_records, round_up_usize, tar_padding_len, validate_chunk_size, with_alignment_pad,
};
use crate::tar::is_portable_ustar_linkname;

/// Complete planned layout for one `rao-v1` object.
#[derive(Debug, Clone)]
pub struct RemTarObjectLayout {
    /// Object identifier copied from the global pax header.
    pub object_id: String,
    /// Caller object identifier copied from the global pax header.
    pub caller_object_id: String,
    /// Body block size in bytes.
    pub chunk_size: usize,
    /// Effective RAO stream schema version written in the global pax header.
    pub schema_version: String,
    /// Total archive byte length after the final fixed-block zero fill.
    pub total_size_bytes: u64,
    /// Exact block count to pass as `projected_size_blocks`.
    pub projected_size_blocks: u64,
    /// Payload file layouts, excluding the generated manifest.
    pub files: Vec<RemTarFileLayout>,
    /// Generated manifest layout.
    pub manifest: RemTarFileLayout,
    /// SHA-256 of the generated manifest CBOR bytes.
    pub manifest_sha256: [u8; 32],
    /// Generated manifest CBOR bytes.
    pub manifest_cbor: Vec<u8>,
    /// Encoded global pax body byte length before tar record padding.
    pub global_pax_body_len: usize,
}

/// Plan a `rao-v1` object using the same pax sizing/alignment rules the
/// writer uses.
pub fn plan_rem_tar_object(
    options: &RemTarObjectOptions,
    files: &[RemTarFileSpec],
) -> Result<RemTarObjectLayout, FormatError> {
    validate_options(options)?;
    if files.len() > MAX_FILE_ENTRIES {
        return Err(FormatError::invalid(
            "file entry count exceeds MAX_FILE_ENTRIES",
        ));
    }

    let mut offset = 0u64;
    let schema_version = stream_schema_version(files);
    let global_records = global_pax_records(options, schema_version);
    let global_body = encode_pax_records(&global_records)?;
    let global_padded = round_up_usize(global_body.len(), TAR_RECORD_SIZE)?;
    offset = checked_add(offset, TAR_RECORD_SIZE as u64 + global_padded as u64)?;

    let mut seen_paths = BTreeSet::new();
    let mut seen_file_ids = BTreeSet::new();
    let mut seen_regular_paths = BTreeSet::new();
    let mut file_layouts = Vec::with_capacity(files.len());
    for spec in files {
        validate_file_spec(spec)?;
        if spec.entry_type == RemTarEntryType::Hardlink {
            let target =
                spec.link_target
                    .as_deref()
                    .ok_or_else(|| FormatError::InvalidHardlinkTarget {
                        path: spec.path.clone(),
                        target: String::new(),
                    })?;
            if !seen_regular_paths.contains(target) {
                return Err(FormatError::InvalidHardlinkTarget {
                    path: spec.path.clone(),
                    target: target.to_string(),
                });
            }
        }
        if !seen_paths.insert(spec.path.clone()) {
            return Err(FormatError::invalid(format!(
                "duplicate payload path {:?}",
                spec.path
            )));
        }
        if !seen_file_ids.insert(spec.file_id.clone()) {
            return Err(FormatError::invalid(format!(
                "duplicate file_id {:?}",
                spec.file_id
            )));
        }
        let (layout, next_offset) = plan_one_file(options.chunk_size, offset, spec, false, None)?;
        offset = next_offset;
        if spec.entry_type == RemTarEntryType::Regular {
            seen_regular_paths.insert(spec.path.clone());
        }
        file_layouts.push(layout);
    }
    if seen_file_ids.contains(&options.manifest_file_id) {
        return Err(FormatError::invalid(format!(
            "manifest file_id {:?} collides with a payload file_id",
            options.manifest_file_id
        )));
    }

    let manifest_cbor = encode_manifest(options, &file_layouts)?;
    let manifest_sha256 = Sha256::digest(&manifest_cbor);
    let mut manifest_hash = [0u8; 32];
    manifest_hash.copy_from_slice(&manifest_sha256);
    let manifest_spec = RemTarFileSpec {
        entry_type: RemTarEntryType::Regular,
        path: MANIFEST_PATH.to_string(),
        file_id: options.manifest_file_id.clone(),
        size_bytes: manifest_cbor.len() as u64,
        file_sha256: Some(manifest_hash),
        link_target: None,
        xattrs: BTreeMap::new(),
        extensions: BTreeMap::new(),
        mtime: None,
        executable: Some(false),
    };
    let (manifest_layout, next_offset) =
        plan_one_file(options.chunk_size, offset, &manifest_spec, true, None)?;
    offset = next_offset;

    offset = checked_add(offset, (2 * TAR_RECORD_SIZE) as u64)?;
    let total_size = round_up_u64(offset, options.chunk_size as u64)?;
    let projected_size_blocks = total_size / options.chunk_size as u64;

    Ok(RemTarObjectLayout {
        object_id: options.object_id.clone(),
        caller_object_id: options.caller_object_id.clone(),
        chunk_size: options.chunk_size,
        schema_version: schema_version.to_string(),
        total_size_bytes: total_size,
        projected_size_blocks,
        files: file_layouts,
        manifest: manifest_layout,
        manifest_sha256: manifest_hash,
        manifest_cbor,
        global_pax_body_len: global_body.len(),
    })
}

pub(crate) fn global_pax_records(
    options: &RemTarObjectOptions,
    schema_version: &str,
) -> BTreeMap<String, String> {
    let mut records = BTreeMap::new();
    records.insert("REMANENCE.format_id".to_string(), FORMAT_ID.to_string());
    records.insert(
        "REMANENCE.schema_version".to_string(),
        schema_version.to_string(),
    );
    records.insert("REMANENCE.object_id".to_string(), options.object_id.clone());
    records.insert(
        "REMANENCE.caller_object_id".to_string(),
        options.caller_object_id.clone(),
    );
    records.insert(
        "REMANENCE.chunk_size".to_string(),
        options.chunk_size.to_string(),
    );
    records.insert(
        "REMANENCE.metadata_preservation".to_string(),
        options.metadata_preservation.as_pax_value().to_string(),
    );
    records.insert(
        "REMANENCE.encryption".to_string(),
        options.encryption.clone(),
    );
    records.insert(
        "REMANENCE.write_timestamp".to_string(),
        options.write_timestamp.clone(),
    );
    records
}

pub(crate) fn stream_schema_version(files: &[RemTarFileSpec]) -> &'static str {
    if files.iter().any(|file| !file.xattrs.is_empty()) {
        SCHEMA_VERSION_XATTRS
    } else {
        SCHEMA_VERSION
    }
}

pub(crate) fn file_pax_records(
    spec: &RemTarFileSpec,
    chunk_size: usize,
    is_manifest: bool,
) -> Result<BTreeMap<String, String>, FormatError> {
    let mut records = BTreeMap::new();
    records.insert("path".to_string(), spec.path.clone());
    records.insert("size".to_string(), spec.size_bytes.to_string());
    if let Some(mtime) = &spec.mtime {
        records.insert("mtime".to_string(), mtime.clone());
    }
    records.insert("REMANENCE.file_id".to_string(), spec.file_id.clone());
    if let Some(file_sha256) = spec.file_sha256 {
        records.insert("REMANENCE.file_sha256".to_string(), hex_lower(&file_sha256));
    }
    records.insert(
        "REMANENCE.chunk_count".to_string(),
        chunk_count(spec.size_bytes, chunk_size)?.to_string(),
    );
    if let Some(executable) = spec.executable {
        records.insert("REMANENCE.executable".to_string(), executable.to_string());
    }
    records.insert("REMANENCE.compression".to_string(), "none".to_string());
    if is_manifest {
        records.insert("REMANENCE.is_manifest".to_string(), "true".to_string());
    }
    if matches!(
        spec.entry_type,
        RemTarEntryType::Hardlink | RemTarEntryType::Symlink
    ) {
        let target = spec
            .link_target
            .as_deref()
            .ok_or_else(|| FormatError::invalid("link entry missing link target"))?;
        if !is_portable_ustar_linkname(target) {
            records.insert("linkpath".to_string(), target.to_string());
        }
    }
    Ok(records)
}

pub(crate) fn plan_one_file(
    chunk_size: usize,
    offset: u64,
    spec: &RemTarFileSpec,
    is_manifest: bool,
    solved_records: Option<&BTreeMap<String, String>>,
) -> Result<(RemTarFileLayout, u64), FormatError> {
    let base_records = match solved_records {
        Some(records) => records.clone(),
        None => file_pax_records(spec, chunk_size, is_manifest)?,
    };
    let records = if solved_records.is_some() || spec.size_bytes == 0 {
        base_records
    } else {
        with_alignment_pad(offset, chunk_size, &base_records)?
    };
    let pax_body_len = encode_pax_records(&records)?.len();
    let pax_body_padded = round_up_usize(pax_body_len, TAR_RECORD_SIZE)?;
    let data_offset = checked_add(
        offset,
        TAR_RECORD_SIZE as u64 + pax_body_padded as u64 + TAR_RECORD_SIZE as u64,
    )?;
    if spec.size_bytes > 0 && data_offset % chunk_size as u64 != 0 {
        return Err(FormatError::layout("file data offset is not chunk aligned"));
    }

    let chunk_count = chunk_count(spec.size_bytes, chunk_size)?;
    let next_offset = checked_add(
        checked_add(data_offset, spec.size_bytes)?,
        tar_padding_len(spec.size_bytes) as u64,
    )?;
    Ok((
        RemTarFileLayout {
            entry_type: spec.entry_type,
            path: spec.path.clone(),
            file_id: spec.file_id.clone(),
            size_bytes: spec.size_bytes,
            file_sha256: spec.file_sha256,
            link_target: spec.link_target.clone(),
            xattrs: spec.xattrs.clone(),
            extensions: spec.extensions.clone(),
            executable: spec.executable,
            first_chunk_lba: if spec.size_bytes == 0 {
                None
            } else {
                Some(BodyLba(data_offset / chunk_size as u64))
            },
            chunk_count,
            pax_header_offset: offset,
            data_offset,
            pad_spaces: records
                .get("REMANENCE.pad")
                .map(|value| value.len())
                .unwrap_or(0),
            pax_body_len,
            is_manifest,
        },
        next_offset,
    ))
}

pub(crate) fn encode_manifest(
    options: &RemTarObjectOptions,
    files: &[RemTarFileLayout],
) -> Result<Vec<u8>, FormatError> {
    let mut map = vec![
        (
            "caller_object_id",
            CborValue::Text(options.caller_object_id.clone()),
        ),
        (
            "chunk_size",
            CborValue::Integer((options.chunk_size as u64).into()),
        ),
        ("external_references", CborValue::Array(Vec::new())),
        (
            "file_entries",
            CborValue::Array(files.iter().map(file_manifest_entry).collect()),
        ),
        ("object_id", CborValue::Text(options.object_id.clone())),
        ("object_metadata", object_metadata(options, files)),
        ("schema_version", CborValue::Integer(1u64.into())),
    ];
    map.sort_by_key(|entry| canonical_text_key(entry.0));

    let value = CborValue::Map(
        map.into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_string()), value))
            .collect(),
    );
    let mut bytes = Vec::new();
    ciborium::into_writer(&value, &mut bytes).map_err(|err| FormatError::cbor(err.to_string()))?;
    validate_manifest_profile(&bytes)?;
    Ok(bytes)
}

pub(crate) fn chunk_count(size_bytes: u64, chunk_size: usize) -> Result<u64, FormatError> {
    validate_chunk_size(chunk_size)?;
    if size_bytes == 0 {
        return Ok(0);
    }
    let chunk = chunk_size as u64;
    Ok((size_bytes - 1) / chunk + 1)
}

pub(crate) fn hex_lower(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn file_manifest_entry(layout: &RemTarFileLayout) -> CborValue {
    let mut map = vec![
        ("chunk_count", CborValue::Integer(layout.chunk_count.into())),
        (
            "executable",
            layout
                .executable
                .map(CborValue::Bool)
                .unwrap_or(CborValue::Null),
        ),
        ("file_id", CborValue::Text(layout.file_id.clone())),
        (
            "first_chunk_lba",
            layout
                .first_chunk_lba
                .map(|lba| CborValue::Integer(lba.0.into()))
                .unwrap_or(CborValue::Null),
        ),
        (
            "metadata_preservation_data",
            metadata_preservation_data(layout),
        ),
        ("path", CborValue::Text(layout.path.clone())),
        ("size_bytes", CborValue::Integer(layout.size_bytes.into())),
    ];
    if let Some(file_sha256) = layout.file_sha256 {
        map.push(("file_sha256", CborValue::Bytes(file_sha256.to_vec())));
    }
    if let Some(entry_type) = layout.entry_type.manifest_value() {
        map.push(("entry_type", CborValue::Text(entry_type.to_string())));
    }
    if let Some(link_target) = &layout.link_target {
        map.push(("link_target", CborValue::Text(link_target.clone())));
    }
    map.sort_by_key(|entry| canonical_text_key(entry.0));
    CborValue::Map(
        map.into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_string()), value))
            .collect(),
    )
}

fn metadata_preservation_data(layout: &RemTarFileLayout) -> CborValue {
    if layout.xattrs.is_empty() && layout.extensions.is_empty() {
        return CborValue::Map(Vec::new());
    }

    let mut map = Vec::new();
    if !layout.xattrs.is_empty() {
        let mut xattr_entries: Vec<(&String, CborValue)> = layout
            .xattrs
            .iter()
            .map(|(name, value)| (name, CborValue::Bytes(value.clone())))
            .collect();
        xattr_entries.sort_by_key(|(name, _)| canonical_text_key(name));
        let xattrs = CborValue::Map(
            xattr_entries
                .into_iter()
                .map(|(name, value)| (CborValue::Text(name.clone()), value))
                .collect(),
        );
        map.push(("xattrs", xattrs));
    }
    if !layout.extensions.is_empty() {
        map.push(("ext", extension_map(&layout.extensions)));
    }
    map.sort_by_key(|entry| canonical_text_key(entry.0));
    CborValue::Map(
        map.into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_string()), value))
            .collect(),
    )
}

fn object_metadata(options: &RemTarObjectOptions, files: &[RemTarFileLayout]) -> CborValue {
    let mut attribute_namespaces = BTreeSet::new();
    let mut extension_names = BTreeSet::new();
    for file in files {
        for name in file.xattrs.keys() {
            if let Some(namespace) = xattr_namespace(name) {
                if namespace != "user" {
                    attribute_namespaces.insert(namespace.to_string());
                }
            }
        }
        extension_names.extend(file.extensions.keys().cloned());
    }
    extension_names.extend(options.extensions.keys().cloned());

    let mut map = Vec::new();
    if !attribute_namespaces.is_empty() {
        map.push((
            "attribute_namespaces",
            CborValue::Array(canonical_name_values(attribute_namespaces)),
        ));
    }
    if !extension_names.is_empty() {
        map.push((
            "extensions",
            CborValue::Array(canonical_name_values(extension_names)),
        ));
    }
    if !options.extensions.is_empty() {
        map.push(("ext", extension_map(&options.extensions)));
    }
    map.sort_by_key(|(key, _)| canonical_text_key(key));
    CborValue::Map(
        map.into_iter()
            .map(|(key, value)| (CborValue::Text(key.to_string()), value))
            .collect(),
    )
}

fn canonical_name_values(names: BTreeSet<String>) -> Vec<CborValue> {
    let mut names: Vec<String> = names.into_iter().collect();
    names.sort_by_key(|name| canonical_text_key(name));
    names.into_iter().map(CborValue::Text).collect()
}

fn extension_map(extensions: &RemTarExtensions) -> CborValue {
    let mut entries: Vec<(&String, CborValue)> = extensions
        .iter()
        .map(|(name, value)| (name, profile_cbor_value(value)))
        .collect();
    entries.sort_by_key(|(name, _)| canonical_text_key(name));
    CborValue::Map(
        entries
            .into_iter()
            .map(|(name, value)| (CborValue::Text(name.clone()), value))
            .collect(),
    )
}

fn profile_cbor_value(value: &RemTarCborValue) -> CborValue {
    match value {
        RemTarCborValue::Unsigned(value) => CborValue::Integer((*value).into()),
        RemTarCborValue::Bytes(value) => CborValue::Bytes(value.clone()),
        RemTarCborValue::Text(value) => CborValue::Text(value.clone()),
        RemTarCborValue::Bool(value) => CborValue::Bool(*value),
        RemTarCborValue::Null => CborValue::Null,
        RemTarCborValue::Array(values) => {
            CborValue::Array(values.iter().map(profile_cbor_value).collect())
        }
        RemTarCborValue::Map(values) => {
            let mut entries: Vec<(&String, CborValue)> = values
                .iter()
                .map(|(key, value)| (key, profile_cbor_value(value)))
                .collect();
            entries.sort_by_key(|(key, _)| canonical_text_key(key));
            CborValue::Map(
                entries
                    .into_iter()
                    .map(|(key, value)| (CborValue::Text(key.clone()), value))
                    .collect(),
            )
        }
    }
}

fn validate_options(options: &RemTarObjectOptions) -> Result<(), FormatError> {
    validate_chunk_size(options.chunk_size)?;
    validate_non_empty("object_id", &options.object_id)?;
    validate_non_empty("caller_object_id", &options.caller_object_id)?;
    validate_non_empty("write_timestamp", &options.write_timestamp)?;
    validate_non_empty("manifest_file_id", &options.manifest_file_id)?;
    if options.encryption != "none" {
        return Err(FormatError::unsupported_feature(format!(
            "inner RAO stream encryption must be \"none\", got {:?}",
            options.encryption
        )));
    }
    Ok(())
}

pub(crate) fn canonical_text_key(key: &str) -> Vec<u8> {
    let len = key.len() as u64;
    let mut encoded = Vec::with_capacity(key.len() + 9);
    encode_cbor_major_len(3, len, &mut encoded);
    encoded.extend_from_slice(key.as_bytes());
    encoded
}

fn encode_cbor_major_len(major: u8, len: u64, out: &mut Vec<u8>) {
    let prefix = major << 5;
    if len < 24 {
        out.push(prefix | len as u8);
    } else if let Ok(value) = u8::try_from(len) {
        out.extend_from_slice(&[prefix | 24, value]);
    } else if let Ok(value) = u16::try_from(len) {
        out.push(prefix | 25);
        out.extend_from_slice(&value.to_be_bytes());
    } else if let Ok(value) = u32::try_from(len) {
        out.push(prefix | 26);
        out.extend_from_slice(&value.to_be_bytes());
    } else {
        out.push(prefix | 27);
        out.extend_from_slice(&len.to_be_bytes());
    }
}

fn validate_file_spec(spec: &RemTarFileSpec) -> Result<(), FormatError> {
    validate_non_empty("path", &spec.path)?;
    validate_non_empty("file_id", &spec.file_id)?;
    for name in spec.xattrs.keys() {
        validate_non_empty("xattr name", name)?;
        validate_pax_value("xattr name", name)?;
    }
    match spec.entry_type {
        RemTarEntryType::Regular => {
            if spec.file_sha256.is_none() {
                return Err(FormatError::invalid("regular file missing file_sha256"));
            }
            if spec.link_target.is_some() {
                return Err(FormatError::invalid(
                    "regular file must not have link_target",
                ));
            }
        }
        RemTarEntryType::Hardlink => {
            if !spec.xattrs.is_empty() || !spec.extensions.is_empty() {
                return Err(FormatError::invalid(
                    "hardlink entry must not have preservation metadata; metadata resolves through link_target",
                ));
            }
            if spec.size_bytes != 0 {
                return Err(FormatError::invalid("hardlink entry size must be zero"));
            }
            if spec.file_sha256.is_some() {
                return Err(FormatError::invalid(
                    "hardlink entry must not have file_sha256",
                ));
            }
            let target =
                spec.link_target
                    .as_deref()
                    .ok_or_else(|| FormatError::InvalidHardlinkTarget {
                        path: spec.path.clone(),
                        target: String::new(),
                    })?;
            if target.is_empty() {
                return Err(FormatError::InvalidHardlinkTarget {
                    path: spec.path.clone(),
                    target: String::new(),
                });
            }
            validate_pax_value("link_target", target)?;
            validate_canonical_relative_path(target, false)?;
        }
        RemTarEntryType::Symlink => {
            if spec.size_bytes != 0 {
                return Err(FormatError::invalid("symlink entry size must be zero"));
            }
            if spec.file_sha256.is_some() {
                return Err(FormatError::invalid(
                    "symlink entry must not have file_sha256",
                ));
            }
            let target = spec
                .link_target
                .as_deref()
                .ok_or_else(|| FormatError::invalid("symlink entry missing link_target"))?;
            validate_non_empty("link_target", target)?;
            validate_pax_value("link_target", target)?;
        }
        RemTarEntryType::Directory => {
            if spec.size_bytes != 0 {
                return Err(FormatError::invalid("directory entry size must be zero"));
            }
            if spec.file_sha256.is_some() {
                return Err(FormatError::invalid(
                    "directory entry must not have file_sha256",
                ));
            }
            if spec.link_target.is_some() {
                return Err(FormatError::invalid(
                    "directory entry must not have link_target",
                ));
            }
            if !spec.path.ends_with('/') {
                return Err(FormatError::invalid(
                    "directory entry path must end with '/'",
                ));
            }
        }
    }
    if spec.path == "_remanence" || spec.path.starts_with("_remanence/") {
        return Err(FormatError::invalid(
            "payload entries must not use reserved _remanence paths",
        ));
    }
    if spec.path.as_bytes().contains(&0) {
        return Err(FormatError::invalid("path must not contain NUL"));
    }
    if spec.path.bytes().any(|byte| byte < 0x20) {
        return Err(FormatError::invalid(
            "path must not contain ASCII control characters",
        ));
    }
    if let Some(mtime) = &spec.mtime {
        validate_pax_mtime(mtime)?;
    }
    validate_canonical_relative_path(&spec.path, spec.entry_type == RemTarEntryType::Directory)?;
    Ok(())
}

pub(crate) fn xattr_namespace(name: &str) -> Option<&str> {
    name.split_once('.').map(|(namespace, _)| namespace)
}

fn validate_pax_mtime(value: &str) -> Result<(), FormatError> {
    let (seconds, fraction) = value
        .split_once('.')
        .map_or((value, None), |(left, right)| (left, Some(right)));
    if seconds.is_empty() || !seconds.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(FormatError::invalid(
            "mtime must be non-negative decimal seconds",
        ));
    }
    if let Some(fraction) = fraction {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(FormatError::invalid(
                "mtime fractional part must contain decimal digits",
            ));
        }
    }
    Ok(())
}

fn validate_canonical_relative_path(path: &str, is_directory: bool) -> Result<(), FormatError> {
    if path.starts_with('/') {
        return Err(FormatError::invalid("path must be relative"));
    }
    let component_path = if is_directory {
        path.strip_suffix('/')
            .ok_or_else(|| FormatError::invalid("directory entry path must end with '/'"))?
    } else {
        if path.ends_with('/') {
            return Err(FormatError::invalid(
                "non-directory entry path must not end with '/'",
            ));
        }
        path
    };
    if component_path.is_empty() {
        return Err(FormatError::invalid("path must not be empty"));
    }
    for component in component_path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(FormatError::invalid(format!(
                "path contains non-canonical component {component:?}"
            )));
        }
    }
    Ok(())
}

fn validate_pax_value(field: &str, value: &str) -> Result<(), FormatError> {
    if value.as_bytes().contains(&0) {
        return Err(FormatError::invalid(format!(
            "{field} must not contain NUL"
        )));
    }
    if value.bytes().any(|byte| byte < 0x20) {
        return Err(FormatError::invalid(format!(
            "{field} must not contain ASCII control characters"
        )));
    }
    Ok(())
}

fn validate_non_empty(field: &str, value: &str) -> Result<(), FormatError> {
    if value.is_empty() {
        Err(FormatError::invalid(format!("{field} must not be empty")))
    } else {
        Ok(())
    }
}

fn checked_add(left: u64, right: u64) -> Result<u64, FormatError> {
    left.checked_add(right)
        .ok_or_else(|| FormatError::layout("offset overflow"))
}

fn round_up_u64(value: u64, unit: u64) -> Result<u64, FormatError> {
    if unit == 0 {
        return Err(FormatError::invalid("rounding unit must be non-zero"));
    }
    let rem = value % unit;
    if rem == 0 {
        Ok(value)
    } else {
        value
            .checked_add(unit - rem)
            .ok_or_else(|| FormatError::layout("round-up overflow"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{manifest_preservation_metadata, validate_manifest};

    fn options(chunk_size: usize) -> RemTarObjectOptions {
        let mut opts = RemTarObjectOptions::new(
            "11111111-1111-1111-1111-111111111111",
            "caller-1",
            "2026-05-27T21:45:00+05:30",
            "22222222-2222-2222-2222-222222222222",
        );
        opts.chunk_size = chunk_size;
        opts
    }

    fn file(path: &str, size: u64) -> RemTarFileSpec {
        RemTarFileSpec::new(path, format!("file-{path}"), size, [0xAB; 32])
    }

    fn map_field<'a>(value: &'a CborValue, key: &str) -> &'a CborValue {
        value
            .as_map()
            .expect("value is a map")
            .iter()
            .find_map(|(candidate, value)| {
                matches!(candidate, CborValue::Text(candidate) if candidate == key).then_some(value)
            })
            .unwrap_or_else(|| panic!("map contains {key:?}"))
    }

    fn text_array(value: &CborValue) -> Vec<&str> {
        value
            .as_array()
            .expect("value is an array")
            .iter()
            .map(|value| value.as_text().expect("array member is text"))
            .collect()
    }

    #[test]
    fn global_pax_body_is_counted_in_projection() {
        let opts = options(4096);
        let layout = plan_rem_tar_object(&opts, &[file("a.bin", 1)]).unwrap();
        assert!(layout.global_pax_body_len > 0);
        assert!(layout.files[0].pax_header_offset > 0);
        assert_eq!(layout.files[0].pax_header_offset, 512 + 512);
        assert_eq!(
            layout.total_size_bytes,
            layout.projected_size_blocks * opts.chunk_size as u64
        );
    }

    #[test]
    fn empty_caller_object_id_is_rejected_at_layout_admission() {
        let mut opts = options(4096);
        opts.caller_object_id.clear();
        let error = plan_rem_tar_object(&opts, &[file("a.bin", 1)])
            .expect_err("empty caller_object_id must be rejected");

        assert_eq!(
            error.to_string(),
            "invalid RAO input: caller_object_id must not be empty"
        );
    }

    #[test]
    fn every_regular_file_data_start_is_chunk_aligned() {
        let opts = options(4096);
        let layout = plan_rem_tar_object(
            &opts,
            &[
                file("small.txt", 17),
                file("large.bin", 9000),
                file("empty", 0),
            ],
        )
        .unwrap();
        for file in layout.files.iter().chain(std::iter::once(&layout.manifest)) {
            if file.size_bytes > 0 {
                assert_eq!(file.data_offset % opts.chunk_size as u64, 0, "{file:?}");
            }
        }
        assert_eq!(layout.files[2].first_chunk_lba, None);
        assert_eq!(layout.files[2].chunk_count, 0);
        assert_eq!(layout.files[2].pad_spaces, 0);
    }

    #[test]
    fn nonregular_entries_are_zero_payload_manifest_entries() {
        let opts = options(4096);
        let primary = file("target.mov", 17);
        let hardlink = RemTarFileSpec::hardlink("links/copy.mov", "hardlink-1", "target.mov");
        let symlink = RemTarFileSpec::symlink("links/latest", "link-1", "../target.mov");
        let directory = RemTarFileSpec::directory("empty/", "dir-1");

        let layout = plan_rem_tar_object(&opts, &[primary, hardlink, symlink, directory]).unwrap();

        assert_eq!(layout.files[1].entry_type, RemTarEntryType::Hardlink);
        assert_eq!(layout.files[1].size_bytes, 0);
        assert_eq!(layout.files[1].file_sha256, None);
        assert_eq!(layout.files[1].link_target.as_deref(), Some("target.mov"));
        assert_eq!(layout.files[1].first_chunk_lba, None);
        assert_eq!(layout.files[1].chunk_count, 0);
        assert_eq!(layout.files[1].pad_spaces, 0);

        assert_eq!(layout.files[2].entry_type, RemTarEntryType::Symlink);
        assert_eq!(layout.files[2].size_bytes, 0);
        assert_eq!(layout.files[2].file_sha256, None);
        assert_eq!(
            layout.files[2].link_target.as_deref(),
            Some("../target.mov")
        );
        assert_eq!(layout.files[2].first_chunk_lba, None);
        assert_eq!(layout.files[2].chunk_count, 0);
        assert_eq!(layout.files[2].pad_spaces, 0);

        assert_eq!(layout.files[3].entry_type, RemTarEntryType::Directory);
        assert_eq!(layout.files[3].path, "empty/");
        assert_eq!(layout.files[3].file_sha256, None);
        assert_eq!(layout.files[3].link_target, None);

        let manifest = String::from_utf8_lossy(&layout.manifest_cbor);
        assert!(manifest.contains("entry_type"));
        assert!(manifest.contains("hardlink"));
        assert!(manifest.contains("symlink"));
        assert!(manifest.contains("directory"));
        assert!(manifest.contains("link_target"));
        assert!(manifest.contains("target.mov"));
        assert!(manifest.contains("../target.mov"));
    }

    #[test]
    fn planner_rejects_invalid_hardlink_targets() {
        let opts = options(4096);

        let err = plan_rem_tar_object(
            &opts,
            &[RemTarFileSpec::hardlink(
                "copy.bin",
                "hardlink-1",
                "missing.bin",
            )],
        )
        .expect_err("missing hardlink target should fail");
        assert!(matches!(err, FormatError::InvalidHardlinkTarget { .. }));

        let err = plan_rem_tar_object(
            &opts,
            &[
                RemTarFileSpec::directory("target-dir/", "dir-1"),
                RemTarFileSpec::hardlink("copy.bin", "hardlink-1", "target-dir"),
            ],
        )
        .expect_err("hardlink to a nonregular target should fail");
        assert!(matches!(err, FormatError::InvalidHardlinkTarget { .. }));

        let err = plan_rem_tar_object(
            &opts,
            &[
                RemTarFileSpec::hardlink("copy.bin", "hardlink-1", "target.bin"),
                file("target.bin", 17),
            ],
        )
        .expect_err("forward hardlink target should fail");
        assert!(matches!(err, FormatError::InvalidHardlinkTarget { .. }));
    }

    #[test]
    fn planner_rejects_noncanonical_payload_paths() {
        let opts = options(4096);
        for path in [
            "/abs.bin",
            "a//b.bin",
            "a/./b.bin",
            "a/../b.bin",
            "trailing/",
        ] {
            let err = plan_rem_tar_object(&opts, &[file(path, 1)])
                .expect_err("noncanonical path should fail");
            assert!(err.to_string().contains("path"), "{path}: {err}");
        }
        let err = plan_rem_tar_object(
            &opts,
            &[RemTarFileSpec::directory("bad/../empty/", "dir-1")],
        )
        .expect_err("directory traversal component should fail");
        assert!(err.to_string().contains("path"), "{err}");
    }

    #[test]
    fn manifest_excludes_itself() {
        let opts = options(4096);
        let layout = plan_rem_tar_object(&opts, &[file("payload.bin", 4)]).unwrap();
        let manifest = String::from_utf8_lossy(&layout.manifest_cbor);
        assert!(manifest.contains("payload.bin"));
        assert!(!manifest.contains(MANIFEST_PATH));
        assert!(layout.manifest.is_manifest);
    }

    #[test]
    fn planner_accepted_manifests_validate_against_reader_schema() {
        let opts = options(4096);
        let mut xattr_file = file("meta/with-xattr.bin", 4096);
        xattr_file
            .xattrs
            .insert("user.note".to_string(), b"preserved".to_vec());

        let cases = vec![
            vec![file("single.bin", 1)],
            vec![
                file("target.mov", 17),
                RemTarFileSpec::hardlink("links/copy.mov", "hardlink-1", "target.mov"),
                RemTarFileSpec::symlink("links/latest", "link-1", "../target.mov"),
                RemTarFileSpec::directory("empty/", "dir-1"),
            ],
            vec![
                file("regular/first.bin", 4096),
                file("regular/empty.bin", 0),
                RemTarFileSpec::hardlink("links/empty-copy", "hardlink-empty", "regular/empty.bin"),
                xattr_file,
            ],
        ];

        for files in cases {
            let layout =
                plan_rem_tar_object(&opts, &files).expect("planner-accepted files should plan");
            let global_pax = global_pax_records(&opts, &layout.schema_version);
            validate_manifest(
                &layout.manifest_cbor,
                &layout.manifest_sha256,
                &global_pax,
                opts.chunk_size,
            )
            .expect("encoded production manifest should validate");
        }
    }

    #[test]
    fn manifest_inventory_tracks_extension_tier_names_only() {
        let opts = options(4096);
        let mut spec = file("metadata.bin", 1);
        spec.xattrs
            .insert("user.comment".to_string(), b"portable-value".to_vec());
        spec.xattrs.insert(
            "security.selinux".to_string(),
            b"nonportable-value".to_vec(),
        );
        spec.extensions.insert(
            "org.example.opaque".to_string(),
            RemTarCborValue::Map(BTreeMap::from([(
                "payload".to_string(),
                RemTarCborValue::Bytes(b"extension-value".to_vec()),
            )])),
        );

        let layout = plan_rem_tar_object(&opts, &[spec]).unwrap();
        let global_pax = global_pax_records(&opts, &layout.schema_version);
        validate_manifest(
            &layout.manifest_cbor,
            &layout.manifest_sha256,
            &global_pax,
            opts.chunk_size,
        )
        .unwrap();
        let manifest: CborValue = ciborium::from_reader(layout.manifest_cbor.as_slice()).unwrap();
        let object_metadata = map_field(&manifest, "object_metadata");

        assert_eq!(layout.schema_version, SCHEMA_VERSION_XATTRS);
        assert_eq!(
            text_array(map_field(object_metadata, "attribute_namespaces")),
            ["security"]
        );
        assert_eq!(
            text_array(map_field(object_metadata, "extensions")),
            ["org.example.opaque"]
        );
        assert_eq!(object_metadata.as_map().unwrap().len(), 2);

        let preservation = manifest_preservation_metadata(&layout.manifest_cbor).unwrap();
        let entry = &preservation.entries["metadata.bin"];
        assert_eq!(entry.xattrs["user.comment"], b"portable-value");
        assert_eq!(entry.xattrs["security.selinux"], b"nonportable-value");
        assert_eq!(
            entry.extensions["org.example.opaque"],
            RemTarCborValue::Map(BTreeMap::from([(
                "payload".to_string(),
                RemTarCborValue::Bytes(b"extension-value".to_vec()),
            )]))
        );
    }

    #[test]
    fn portable_core_leaves_object_metadata_empty_and_extensions_do_not_bump_schema() {
        let opts = options(4096);
        let mut core = file("core.bin", 1);
        core.xattrs
            .insert("user.comment".to_string(), b"portable".to_vec());
        let core_layout = plan_rem_tar_object(&opts, &[core]).unwrap();
        let manifest: CborValue =
            ciborium::from_reader(core_layout.manifest_cbor.as_slice()).unwrap();
        assert!(map_field(&manifest, "object_metadata")
            .as_map()
            .unwrap()
            .is_empty());

        let mut extension_only = file("extension.bin", 1);
        extension_only.extensions.insert(
            "org.example.opaque".to_string(),
            RemTarCborValue::Bool(true),
        );
        let extension_layout = plan_rem_tar_object(&opts, &[extension_only]).unwrap();
        assert_eq!(extension_layout.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn decoded_unknown_extensions_reemit_byte_identically() {
        let mut opts = options(4096);
        opts.extensions.insert(
            "org.example.object".to_string(),
            RemTarCborValue::Array(vec![RemTarCborValue::Unsigned(24), RemTarCborValue::Null]),
        );
        let mut spec = file("opaque.bin", 1);
        spec.extensions.insert(
            "Uppercase.Invalid".to_string(),
            RemTarCborValue::Map(BTreeMap::from([(
                "raw".to_string(),
                RemTarCborValue::Bytes(vec![0, 1, 2, 3]),
            )])),
        );
        let layout = plan_rem_tar_object(&opts, &[spec]).unwrap();
        let decoded = manifest_preservation_metadata(&layout.manifest_cbor).unwrap();

        let mut reemit_options = opts.clone();
        reemit_options.extensions = decoded.object_extensions;
        let mut reemit_files = layout.files.clone();
        for file in &mut reemit_files {
            file.extensions = decoded
                .entries
                .get(&file.path)
                .map(|entry| entry.extensions.clone())
                .unwrap_or_default();
        }
        let reemitted = encode_manifest(&reemit_options, &reemit_files).unwrap();

        assert_eq!(reemitted, layout.manifest_cbor);
    }

    #[test]
    fn planner_rejects_duplicate_payload_paths_and_file_ids() {
        let opts = options(4096);
        let err = plan_rem_tar_object(
            &opts,
            &[
                RemTarFileSpec::new("dup.bin", "file-a", 1, [0x11; 32]),
                RemTarFileSpec::new("dup.bin", "file-b", 1, [0x22; 32]),
            ],
        )
        .expect_err("duplicate path should fail");
        assert!(err.to_string().contains("duplicate payload path"));

        let err = plan_rem_tar_object(
            &opts,
            &[
                RemTarFileSpec::new("a.bin", "file-a", 1, [0x11; 32]),
                RemTarFileSpec::new("b.bin", "file-a", 1, [0x22; 32]),
            ],
        )
        .expect_err("duplicate file_id should fail");
        assert!(err.to_string().contains("duplicate file_id"));

        let mut opts = options(4096);
        opts.manifest_file_id = "file-a.bin".to_string();
        let err = plan_rem_tar_object(&opts, &[file("a.bin", 1)])
            .expect_err("manifest file_id collision should fail");
        assert!(err.to_string().contains("manifest file_id"));
    }

    #[test]
    fn planner_rejects_reserved_remanence_payload_paths() {
        let opts = options(4096);
        for path in ["_remanence", "_remanence/other.cbor", MANIFEST_PATH] {
            let err = plan_rem_tar_object(&opts, &[file(path, 1)])
                .expect_err("reserved payload path should fail");
            assert!(
                err.to_string().contains("reserved _remanence paths"),
                "{err}"
            );
        }
    }

    #[test]
    fn planner_rejects_malformed_pax_mtime() {
        let opts = options(4096);
        for mtime in ["", "-1", "abc", "1.", ".1", "1.abc", "1\n2"] {
            let mut spec = file("a.bin", 1);
            spec.mtime = Some(mtime.to_string());
            let err = plan_rem_tar_object(&opts, &[spec]).expect_err("malformed mtime should fail");
            assert!(err.to_string().contains("mtime"), "{mtime:?}: {err}");
        }

        let mut spec = file("a.bin", 1);
        spec.mtime = Some("0.123".to_string());
        plan_rem_tar_object(&opts, &[spec]).expect("decimal pax mtime should be accepted");
    }

    #[test]
    fn manifest_map_keys_use_rfc8949_encoded_order() {
        let opts = options(4096);
        let layout = plan_rem_tar_object(&opts, &[file("payload.bin", 4)]).unwrap();
        let value: ciborium::value::Value =
            ciborium::from_reader(layout.manifest_cbor.as_slice()).unwrap();
        let ciborium::value::Value::Map(entries) = value else {
            panic!("manifest root must be a CBOR map");
        };
        let keys = entries
            .into_iter()
            .map(|(key, _)| match key {
                ciborium::value::Value::Text(text) => text,
                other => panic!("manifest key must be text, got {other:?}"),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            keys,
            vec![
                "object_id",
                "chunk_size",
                "file_entries",
                "schema_version",
                "object_metadata",
                "caller_object_id",
                "external_references",
            ]
        );
    }
}
