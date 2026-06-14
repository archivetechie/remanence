//! Ingest-policy planning for `rem archive build --rules`.
//!
//! RAO itself only stores normalized entries. This module stays above the
//! format crate and turns messy filesystem trees into ordinary RAO inputs by
//! applying ordered blob/exclude rules, creating `.remwrap.tar` payloads with a
//! mainstream tar engine, and deriving sibling `.remwrap.idx` entries for blobs.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use remanence_format::RemTarEntryType;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::{
    archive_path_from_relative, bytes_to_hex, path_component_to_string,
    read_archive_build_directory, read_archive_build_file, read_archive_build_symlink, sha256_file,
    ArchiveBuildInputFile,
};

const WRAP_TAR_SUFFIX: &str = ".remwrap.tar";
const WRAP_INDEX_SUFFIX: &str = ".remwrap.idx";
const DEFAULT_SAMPLES_PER_CLUSTER: usize = 3;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScanTuning {
    pub(crate) blob_ratio: f64,
    pub(crate) blob_count: u64,
    pub(crate) sanity_ceiling: u64,
}

impl Default for ScanTuning {
    fn default() -> Self {
        Self {
            blob_ratio: 0.9,
            blob_count: 100,
            sanity_ceiling: 10_000,
        }
    }
}

#[derive(Debug)]
pub(crate) struct MaterializedArchiveInputs {
    pub(crate) inputs: Vec<ArchiveBuildInputFile>,
    pub(crate) report: IngestReport,
    pub(crate) manifest: CustomerManifest,
    _tempdir: TempDir,
}

#[derive(Debug, Serialize)]
pub(crate) struct IngestReport {
    pub(crate) ruleset: Option<RulesetReport>,
    pub(crate) tar_engine: TarEngineReport,
    pub(crate) scan: ScanReport,
    pub(crate) lints: Vec<RulesetLint>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RulesetReport {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) sha256: String,
    pub(crate) case_insensitive: bool,
    pub(crate) rule_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TarEngineReport {
    pub(crate) program: String,
    pub(crate) version: String,
    pub(crate) create_invocation: Vec<String>,
    pub(crate) extract_invocation: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RulesetLint {
    pub(crate) line: usize,
    pub(crate) kind: &'static str,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScanReport {
    pub(crate) totals: ScanTotals,
    pub(crate) clusters: Vec<ScanCluster>,
    pub(crate) blob_suggestions: Vec<BlobSuggestion>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ScanTotals {
    pub(crate) native_entries: u64,
    pub(crate) wrapped_entries: u64,
    pub(crate) blob_entries: u64,
    pub(crate) excluded_entries: u64,
    pub(crate) excluded_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ScanCluster {
    pub(crate) prefix: String,
    pub(crate) reason: String,
    pub(crate) count: u64,
    pub(crate) bytes: u64,
    pub(crate) samples: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct BlobSuggestion {
    pub(crate) prefix: String,
    pub(crate) noncompliant: u64,
    pub(crate) total: u64,
    pub(crate) ratio: f64,
    pub(crate) verdict: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CustomerManifest {
    pub(crate) format: &'static str,
    pub(crate) ruleset: Option<RulesetReport>,
    pub(crate) tar_engine: TarEngineReport,
    pub(crate) entries: Vec<CustomerManifestEntry>,
    pub(crate) exclusions: Vec<ScanCluster>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CustomerManifestEntry {
    pub(crate) path: String,
    pub(crate) kind: &'static str,
    pub(crate) size_bytes: u64,
    pub(crate) sha256: Option<String>,
    pub(crate) wrapper: Option<String>,
}

#[derive(Debug)]
struct Ruleset {
    report: RulesetReport,
    rules: Vec<Rule>,
    lints: Vec<RulesetLint>,
}

#[derive(Debug, Clone)]
struct Rule {
    line: usize,
    verb: RuleVerb,
    pattern: Pattern,
    no_index: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RuleVerb {
    Blob,
    Exclude,
}

#[derive(Debug, Clone)]
struct Pattern {
    raw: String,
    normalized: String,
    directory_only: bool,
    has_wildcard: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Decision {
    Granular,
    Blob { no_index: bool },
    Exclude,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum NativeStatus {
    Native,
    WrapFallback(&'static str),
}

#[derive(Debug, Default)]
struct PlannerState {
    files: Vec<ArchiveBuildInputFile>,
    manifest_entries: Vec<CustomerManifestEntry>,
    clusters: BTreeMap<(String, String), ClusterAccumulator>,
    dir_stats: BTreeMap<String, DirStats>,
    totals: ScanTotals,
    wrapper_counter: u64,
}

#[derive(Debug, Default)]
struct ClusterAccumulator {
    count: u64,
    bytes: u64,
    samples: Vec<String>,
}

#[derive(Debug, Default)]
struct DirStats {
    total: u64,
    noncompliant: u64,
}

#[derive(Clone, Copy)]
struct ProcessContext<'a> {
    ruleset: Option<&'a Ruleset>,
    tar_engine: &'a TarEngineReport,
    tempdir: &'a Path,
    no_index: bool,
}

impl<'a> ProcessContext<'a> {
    fn with_no_index(self, no_index: bool) -> Self {
        Self { no_index, ..self }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WrapIndex {
    format: String,
    tar_engine: TarEngineReport,
    entries: Vec<WrapIndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WrapIndexEntry {
    path: String,
    kind: String,
    offset: u64,
    length: u64,
    sha256: Option<String>,
}

pub(crate) fn materialize_inputs(
    input_paths: &[PathBuf],
    rules_path: Option<&Path>,
    no_index: bool,
    tuning: ScanTuning,
) -> Result<MaterializedArchiveInputs, String> {
    let tar_engine = detect_tar_engine()?;
    let ruleset = match rules_path {
        Some(path) => Some(load_ruleset(path)?),
        None => None,
    };
    let tempdir = tempfile::Builder::new()
        .prefix("remanence-remwrap-")
        .tempdir()
        .map_err(|error| format!("create wrapper tempdir: {error}"))?;
    let mut state = PlannerState::default();
    let context = ProcessContext {
        ruleset: ruleset.as_ref(),
        tar_engine: &tar_engine,
        tempdir: tempdir.path(),
        no_index,
    };

    for input in input_paths {
        process_input(input, context, &mut state)?;
    }
    state
        .files
        .sort_by(|a, b| a.archive_path.cmp(&b.archive_path));
    ensure_unique_archive_paths(&state.files)?;

    let lints = ruleset
        .as_ref()
        .map(|ruleset| ruleset.lints.clone())
        .unwrap_or_default();
    let ruleset_report = ruleset.as_ref().map(|ruleset| ruleset.report.clone());
    let scan = state.scan_report(tuning);
    let exclusions = scan
        .clusters
        .iter()
        .filter(|cluster| cluster.reason == "exclude-rule")
        .cloned()
        .collect();
    let manifest = CustomerManifest {
        format: "remanence-customer-manifest-v1",
        ruleset: ruleset_report.clone(),
        tar_engine: tar_engine.clone(),
        entries: state.manifest_entries,
        exclusions,
    };
    let report = IngestReport {
        ruleset: ruleset_report,
        tar_engine,
        scan,
        lints,
    };
    Ok(MaterializedArchiveInputs {
        inputs: state.files,
        report,
        manifest,
        _tempdir: tempdir,
    })
}

pub(crate) fn scan_only_report(
    input_paths: &[PathBuf],
    rules_path: Option<&Path>,
    no_index: bool,
    tuning: ScanTuning,
) -> Result<IngestReport, String> {
    let plan = materialize_inputs(input_paths, rules_path, no_index, tuning)?;
    Ok(plan.report)
}

pub(crate) fn write_customer_manifest(
    path: &Path,
    manifest: &CustomerManifest,
) -> Result<(), String> {
    if path.exists() {
        return Err(format!("--manifest-out {} already exists", path.display()));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create manifest directory {}: {error}", parent.display()))?;
    }
    let mut file = File::create(path)
        .map_err(|error| format!("create manifest {}: {error}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, manifest)
        .map_err(|error| format!("serialize manifest {}: {error}", path.display()))?;
    file.write_all(b"\n")
        .map_err(|error| format!("write manifest {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync manifest {}: {error}", path.display()))
}

pub(crate) fn unwrap_remwraps(root: &Path, overwrite: bool) -> Result<UnwrapReport, String> {
    let tar_engine = detect_tar_engine()?;
    let mut wrappers = Vec::new();
    collect_remwrap_files(root, &mut wrappers)?;
    wrappers.sort();
    let mut report = UnwrapReport {
        wrappers_unwrapped: 0,
        literal_entries_removed: 0,
        tar_engine,
    };
    for wrapper in wrappers {
        extract_wrapper_tar(&report.tar_engine, root, &wrapper, overwrite)?;
        fs::remove_file(&wrapper)
            .map_err(|error| format!("remove wrapper {}: {error}", wrapper.display()))?;
        report.literal_entries_removed += 1;
        if let Some(idx) = remwrap_index_filesystem_path(&wrapper).filter(|idx| idx.exists()) {
            fs::remove_file(&idx)
                .map_err(|error| format!("remove wrapper index {}: {error}", idx.display()))?;
            report.literal_entries_removed += 1;
        }
        report.wrappers_unwrapped += 1;
    }
    Ok(report)
}

#[derive(Debug, Serialize)]
pub(crate) struct UnwrapReport {
    pub(crate) wrappers_unwrapped: u64,
    pub(crate) literal_entries_removed: u64,
    pub(crate) tar_engine: TarEngineReport,
}

pub(crate) fn extract_blob_member_from_object(
    object: &remanence_format::RemTarReadObject,
    blob_entry_path: &str,
    member_path: &str,
) -> Result<Vec<u8>, String> {
    let blob_entry = object
        .entry(blob_entry_path)
        .ok_or_else(|| format!("RAO blob entry {blob_entry_path:?} not found"))?;
    if blob_entry.entry_type != RemTarEntryType::Regular {
        return Err(format!(
            "RAO blob entry {blob_entry_path:?} is not a regular file"
        ));
    }
    let idx_path = remwrap_index_path(blob_entry_path)?;
    let idx_entry = object
        .entry(&idx_path)
        .ok_or_else(|| format!("RAO blob index {idx_path:?} not found"))?;
    let index: WrapIndex = serde_json::from_slice(&idx_entry.data)
        .map_err(|error| format!("parse blob index {idx_path:?}: {error}"))?;
    let member = index
        .entries
        .iter()
        .find(|entry| entry.path == member_path)
        .ok_or_else(|| format!("blob member {member_path:?} not found in {idx_path:?}"))?;
    if member.kind != "regular" {
        return Err(format!(
            "blob member {member_path:?} is {}, not regular",
            member.kind
        ));
    }
    let start = usize::try_from(member.offset)
        .map_err(|_| format!("blob member {member_path:?} offset is too large"))?;
    let end_u64 = member
        .offset
        .checked_add(member.length)
        .ok_or_else(|| format!("blob member {member_path:?} offset/length overflows"))?;
    let end = usize::try_from(end_u64)
        .map_err(|_| format!("blob member {member_path:?} end offset is too large"))?;
    let bytes = blob_entry
        .data
        .get(start..end)
        .ok_or_else(|| format!("blob member {member_path:?} range exceeds wrapper bytes"))?;
    if let Some(expected) = &member.sha256 {
        let actual = bytes_to_hex(&sha256_bytes_local(bytes));
        if &actual != expected {
            return Err(format!(
                "blob member {member_path:?} digest mismatch: expected {expected}, got {actual}"
            ));
        }
    }
    Ok(bytes.to_vec())
}

fn process_input(
    input: &Path,
    context: ProcessContext<'_>,
    state: &mut PlannerState,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(input)
        .map_err(|error| format!("stat input {}: {error}", input.display()))?;
    if metadata.file_type().is_symlink() {
        let name = input.file_name().ok_or_else(|| {
            format!(
                "input symlink {} does not have a file name",
                input.display()
            )
        })?;
        let archive_path = path_component_to_string(name)?;
        process_leaf(
            input,
            &PathBuf::from(name),
            archive_path,
            &metadata,
            context,
            state,
        )
    } else if metadata.is_dir() {
        process_dir(input, input, Path::new(""), context, state).map(|_| ())
    } else if metadata.is_file() {
        let name = input
            .file_name()
            .ok_or_else(|| format!("input file {} does not have a file name", input.display()))?;
        let archive_path = path_component_to_string(name)?;
        process_leaf(
            input,
            &PathBuf::from(name),
            archive_path,
            &metadata,
            context,
            state,
        )
    } else {
        let relative = PathBuf::from(input.file_name().unwrap_or_else(|| OsStr::new("input")));
        wrap_leaf(input, &relative, "unsupported-file-type", context, state)
    }
}

fn process_dir(
    root: &Path,
    dir: &Path,
    relative: &Path,
    context: ProcessContext<'_>,
    state: &mut PlannerState,
) -> Result<bool, String> {
    let rel_text = relative_match_text(relative);
    match decide(context.ruleset, &rel_text, true) {
        Decision::Exclude => {
            let (count, bytes) = subtree_count_bytes(dir)?;
            state.record_cluster(&rel_text, "exclude-rule", count, bytes);
            state.totals.excluded_entries += count;
            state.totals.excluded_bytes += bytes;
            return Ok(true);
        }
        Decision::Blob {
            no_index: rule_no_index,
        } if !relative.as_os_str().is_empty() => {
            materialize_blob(
                root,
                dir,
                relative,
                context.with_no_index(context.no_index || rule_no_index),
                state,
            )?;
            return Ok(true);
        }
        Decision::Blob {
            no_index: rule_no_index,
        } => {
            let name = dir.file_name().ok_or_else(|| {
                format!(
                    "blob input directory {} does not have a file name",
                    dir.display()
                )
            })?;
            materialize_root_blob(
                dir,
                name,
                context.with_no_index(context.no_index || rule_no_index),
                state,
            )?;
            return Ok(true);
        }
        Decision::Granular => {}
    }

    let mut entries = fs::read_dir(dir)
        .map_err(|error| format!("read directory {}: {error}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read directory {}: {error}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    if entries.is_empty() {
        if !relative.as_os_str().is_empty() {
            let archive_path = format!("{}/", archive_path_from_relative(relative)?);
            state.record_native(&rel_text, 0);
            state
                .files
                .push(read_archive_build_directory(dir, archive_path)?);
            state.manifest_entries.push(CustomerManifestEntry {
                path: rel_text,
                kind: "directory",
                size_bytes: 0,
                sha256: None,
                wrapper: None,
            });
            return Ok(true);
        }
        return Ok(false);
    }

    let mut added_any = false;
    for entry in entries {
        let path = entry.path();
        let child_relative = relative.join(entry.file_name());
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("stat input {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() || metadata.is_file() || !metadata.is_dir() {
            let archive_path = sanitized_or_native_archive_path(&child_relative);
            process_leaf(
                &path,
                &child_relative,
                archive_path,
                &metadata,
                context,
                state,
            )?;
            added_any = true;
        } else if process_dir(root, &path, &child_relative, context, state)? {
            added_any = true;
        }
    }
    Ok(added_any)
}

fn process_leaf(
    path: &Path,
    relative: &Path,
    archive_path: String,
    metadata: &fs::Metadata,
    context: ProcessContext<'_>,
    state: &mut PlannerState,
) -> Result<(), String> {
    let rel_text = relative_match_text(relative);
    match decide(context.ruleset, &rel_text, false) {
        Decision::Exclude => {
            let bytes = metadata.len();
            state.record_cluster(&rel_text, "exclude-rule", 1, bytes);
            state.totals.excluded_entries += 1;
            state.totals.excluded_bytes += bytes;
            return Ok(());
        }
        Decision::Blob { .. } => {
            return Err(format!(
                "blob rule matched non-directory path {rel_text:?}; blob patterns must select directories"
            ))
        }
        Decision::Granular => {}
    }

    match native_status(path, relative, metadata) {
        NativeStatus::Native if metadata.file_type().is_symlink() => {
            state.record_native(&rel_text, 0);
            state
                .files
                .push(read_archive_build_symlink(path, archive_path)?);
            state.manifest_entries.push(CustomerManifestEntry {
                path: rel_text,
                kind: "symlink",
                size_bytes: 0,
                sha256: None,
                wrapper: None,
            });
        }
        NativeStatus::Native if metadata.is_file() => {
            let (size, hash) = hash_for_manifest(path)?;
            state.record_native(&rel_text, size);
            state
                .files
                .push(read_archive_build_file(path, archive_path)?);
            state.manifest_entries.push(CustomerManifestEntry {
                path: rel_text,
                kind: "regular",
                size_bytes: size,
                sha256: Some(bytes_to_hex(&hash)),
                wrapper: None,
            });
        }
        NativeStatus::Native => {
            return Err(format!(
                "internal native status bug for unsupported path {}",
                path.display()
            ))
        }
        NativeStatus::WrapFallback(reason) => {
            wrap_leaf(path, relative, reason, context, state)?;
        }
    }
    Ok(())
}

fn wrap_leaf(
    path: &Path,
    relative: &Path,
    reason: &'static str,
    context: ProcessContext<'_>,
    state: &mut PlannerState,
) -> Result<(), String> {
    let rel_text = relative_match_text(relative);
    let wrapper_name = format!(
        "{}{}",
        sanitized_or_native_archive_path(relative),
        WRAP_TAR_SUFFIX
    );
    let wrapper_archive_path = uniquify_archive_path(&wrapper_name, &state.files);
    let tar_path = next_temp_path(context.tempdir, &mut state.wrapper_counter, "wrap.tar");
    let name = path.file_name().unwrap_or_else(|| OsStr::new("entry"));
    create_wrapper_tar(
        context.tar_engine,
        path.parent().unwrap_or_else(|| Path::new(".")),
        name,
        &tar_path,
    )?;
    let (size, hash) = hash_for_manifest(&tar_path)?;
    state.record_wrapped(&rel_text, reason, size);
    state.files.push(read_archive_build_file(
        &tar_path,
        wrapper_archive_path.clone(),
    )?);
    state.manifest_entries.push(CustomerManifestEntry {
        path: rel_text,
        kind: "wrapped",
        size_bytes: size,
        sha256: Some(bytes_to_hex(&hash)),
        wrapper: Some(wrapper_archive_path),
    });
    Ok(())
}

fn materialize_blob(
    root: &Path,
    dir: &Path,
    relative: &Path,
    context: ProcessContext<'_>,
    state: &mut PlannerState,
) -> Result<(), String> {
    let rel_text = relative_match_text(relative);
    let wrapper_archive_path = format!(
        "{}{}",
        archive_path_from_relative(relative)?,
        WRAP_TAR_SUFFIX
    );
    let tar_path = next_temp_path(context.tempdir, &mut state.wrapper_counter, "blob.tar");
    create_wrapper_tar(context.tar_engine, root, relative.as_os_str(), &tar_path)?;
    add_blob_outputs(
        &rel_text,
        &wrapper_archive_path,
        &tar_path,
        context.tar_engine,
        context.tempdir,
        context.no_index,
        state,
    )?;
    let _ = dir;
    Ok(())
}

fn materialize_root_blob(
    dir: &Path,
    name: &OsStr,
    context: ProcessContext<'_>,
    state: &mut PlannerState,
) -> Result<(), String> {
    let rel_text = name.to_string_lossy().into_owned();
    let wrapper_archive_path = format!("{}{}", sanitized_component(name), WRAP_TAR_SUFFIX);
    let tar_path = next_temp_path(context.tempdir, &mut state.wrapper_counter, "blob.tar");
    create_wrapper_tar(
        context.tar_engine,
        dir.parent().unwrap_or_else(|| Path::new(".")),
        name,
        &tar_path,
    )?;
    add_blob_outputs(
        &rel_text,
        &wrapper_archive_path,
        &tar_path,
        context.tar_engine,
        context.tempdir,
        context.no_index,
        state,
    )
}

fn add_blob_outputs(
    rel_text: &str,
    wrapper_archive_path: &str,
    tar_path: &Path,
    tar_engine: &TarEngineReport,
    tempdir: &Path,
    no_index: bool,
    state: &mut PlannerState,
) -> Result<(), String> {
    let (tar_size, tar_hash) = hash_for_manifest(tar_path)?;
    let index = build_wrap_index(tar_path, tar_engine)?;
    state.record_blob(rel_text, tar_size);
    state.files.push(read_archive_build_file(
        tar_path,
        wrapper_archive_path.to_string(),
    )?);
    state.manifest_entries.push(CustomerManifestEntry {
        path: rel_text.to_string(),
        kind: "blob",
        size_bytes: tar_size,
        sha256: Some(bytes_to_hex(&tar_hash)),
        wrapper: Some(wrapper_archive_path.to_string()),
    });
    for entry in &index.entries {
        state.manifest_entries.push(CustomerManifestEntry {
            path: entry.path.clone(),
            kind: manifest_kind(entry.kind.as_str()),
            size_bytes: entry.length,
            sha256: entry.sha256.clone(),
            wrapper: Some(wrapper_archive_path.to_string()),
        });
    }
    if !no_index {
        let idx_path = next_temp_path(tempdir, &mut state.wrapper_counter, "blob.idx");
        write_wrap_index(&idx_path, &index)?;
        let idx_archive_path = remwrap_index_path(wrapper_archive_path)?;
        state
            .files
            .push(read_archive_build_file(&idx_path, idx_archive_path)?);
    }
    Ok(())
}

fn decide(ruleset: Option<&Ruleset>, rel_text: &str, is_dir: bool) -> Decision {
    let Some(ruleset) = ruleset else {
        return Decision::Granular;
    };
    for rule in &ruleset.rules {
        if rule
            .pattern
            .matches(rel_text, is_dir, ruleset.report.case_insensitive)
        {
            return match rule.verb {
                RuleVerb::Blob => Decision::Blob {
                    no_index: rule.no_index,
                },
                RuleVerb::Exclude => Decision::Exclude,
            };
        }
    }
    Decision::Granular
}

fn load_ruleset(path: &Path) -> Result<Ruleset, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("read ruleset {}: {error}", path.display()))?;
    let text = String::from_utf8(bytes.clone())
        .map_err(|error| format!("ruleset {} must be UTF-8: {error}", path.display()))?;
    let mut case_insensitive = false;
    let mut rules = Vec::new();
    for (line_index, original) in text.lines().enumerate() {
        let line_no = line_index + 1;
        let line = strip_comment(original).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(option) = line.strip_prefix("option ") {
            match option.trim() {
                "case-insensitive" => case_insensitive = true,
                other if other.starts_with("expect") => {}
                other => {
                    return Err(format!(
                        "{}:{line_no}: unsupported ruleset option {other:?}",
                        path.display()
                    ))
                }
            }
            continue;
        }
        if line.starts_with("expect") {
            continue;
        }
        let (verb_text, rest) = split_once_whitespace(line).ok_or_else(|| {
            format!(
                "{}:{line_no}: rule must be '<blob|exclude> <pattern>'",
                path.display()
            )
        })?;
        let verb = match verb_text {
            "blob" => RuleVerb::Blob,
            "exclude" => RuleVerb::Exclude,
            other => {
                return Err(format!(
                    "{}:{line_no}: unsupported ruleset verb {other:?}",
                    path.display()
                ))
            }
        };
        let (pattern_text, no_index) = parse_rule_pattern_and_flags(rest);
        if pattern_text.is_empty() {
            return Err(format!(
                "{}:{line_no}: missing rule pattern",
                path.display()
            ));
        }
        let pattern = Pattern::parse(pattern_text);
        if verb == RuleVerb::Blob && !pattern.directory_pattern() {
            return Err(format!(
                "{}:{line_no}: blob pattern {:?} must target directories",
                path.display(),
                pattern.raw
            ));
        }
        rules.push(Rule {
            line: line_no,
            verb,
            pattern,
            no_index,
        });
    }
    let sha256 = bytes_to_hex(&sha256_bytes_local(&bytes));
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("ruleset")
        .to_string();
    let report = RulesetReport {
        name,
        path: path.to_path_buf(),
        sha256,
        case_insensitive,
        rule_count: rules.len(),
    };
    let lints = lint_rules(&rules);
    Ok(Ruleset {
        report,
        rules,
        lints,
    })
}

fn lint_rules(rules: &[Rule]) -> Vec<RulesetLint> {
    let mut lints = Vec::new();
    let mut seen = BTreeMap::<String, usize>::new();
    let mut earlier_dirs = Vec::<(&Rule, String)>::new();
    for (index, rule) in rules.iter().enumerate() {
        if rule.pattern.is_catch_all() && index + 1 != rules.len() {
            lints.push(RulesetLint {
                line: rule.line,
                kind: "unreachable-after-catch-all",
                message: "catch-all rule is not last; later rules cannot be reached".to_string(),
            });
        }
        let key = format!("{:?}:{}", rule.verb, rule.pattern.normalized);
        if let Some(previous) = seen.insert(key, rule.line) {
            lints.push(RulesetLint {
                line: rule.line,
                kind: "duplicate-rule",
                message: format!("same verb and pattern already appeared at line {previous}"),
            });
        }
        if !rule.pattern.has_wildcard {
            let current = trim_dir_slash(&rule.pattern.normalized);
            for (earlier, earlier_pattern) in &earlier_dirs {
                if current != earlier_pattern
                    && current.starts_with(earlier_pattern)
                    && current.as_bytes().get(earlier_pattern.len()) == Some(&b'/')
                {
                    lints.push(RulesetLint {
                        line: rule.line,
                        kind: "literal-subpath-unreachable",
                        message: format!(
                            "literal subpath is consumed by earlier blob/exclude directory rule at line {}",
                            earlier.line
                        ),
                    });
                    break;
                }
            }
        }
        if rule.pattern.directory_pattern() && !rule.pattern.has_wildcard {
            earlier_dirs.push((rule, trim_dir_slash(&rule.pattern.normalized).to_string()));
        }
    }
    lints
}

fn strip_comment(line: &str) -> &str {
    line.split_once('#').map(|(head, _)| head).unwrap_or(line)
}

fn split_once_whitespace(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let split = trimmed.find(char::is_whitespace)?;
    Some((&trimmed[..split], trimmed[split..].trim_start()))
}

fn parse_rule_pattern_and_flags(rest: &str) -> (&str, bool) {
    let mut pattern_end = rest.len();
    let mut no_index = false;
    for (offset, token) in rest.match_indices("--no-index") {
        let before_ok = offset == 0 || rest.as_bytes()[offset - 1].is_ascii_whitespace();
        let after = offset + token.len();
        let after_ok = after == rest.len() || rest.as_bytes()[after].is_ascii_whitespace();
        if before_ok && after_ok {
            pattern_end = pattern_end.min(offset);
            no_index = true;
        }
    }
    (rest[..pattern_end].trim(), no_index)
}

impl Pattern {
    fn parse(raw: &str) -> Self {
        let raw = raw.trim().trim_start_matches('/').to_string();
        let directory_only = raw.ends_with('/');
        let normalized = raw.trim_end_matches('/').to_string();
        let has_wildcard = normalized.chars().any(|ch| matches!(ch, '*' | '?' | '['));
        Self {
            raw,
            normalized,
            directory_only,
            has_wildcard,
        }
    }

    fn directory_pattern(&self) -> bool {
        self.directory_only || self.normalized == "**" || self.normalized.ends_with("/**")
    }

    fn is_catch_all(&self) -> bool {
        matches!(self.normalized.as_str(), "**" | "**/*")
    }

    fn matches(&self, rel_text: &str, is_dir: bool, case_insensitive: bool) -> bool {
        if self.directory_only && !is_dir {
            return false;
        }
        let pattern = if case_insensitive {
            self.normalized.to_lowercase()
        } else {
            self.normalized.clone()
        };
        let text = if case_insensitive {
            rel_text.to_lowercase()
        } else {
            rel_text.to_string()
        };
        if pattern.is_empty() {
            return text.is_empty();
        }
        if self.is_catch_all() {
            return true;
        }
        if !pattern.contains('/') {
            let basename = text.rsplit('/').next().unwrap_or(text.as_str());
            glob_matches(&pattern, basename)
        } else {
            glob_matches(&pattern, &text)
        }
    }
}

fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let mut memo = BTreeSet::<(usize, usize)>::new();
    glob_matches_at(pattern, text, 0, 0, &mut memo)
}

fn glob_matches_at(
    pattern: &[u8],
    text: &[u8],
    pi: usize,
    ti: usize,
    memo: &mut BTreeSet<(usize, usize)>,
) -> bool {
    if !memo.insert((pi, ti)) {
        return false;
    }
    if pi == pattern.len() {
        return ti == text.len();
    }
    if pattern.get(pi..pi + 3) == Some(b"**/") {
        if glob_matches_at(pattern, text, pi + 3, ti, memo) {
            return true;
        }
        let mut cursor = ti;
        while cursor < text.len() {
            if text[cursor] == b'/' && glob_matches_at(pattern, text, pi + 3, cursor + 1, memo) {
                return true;
            }
            cursor += 1;
        }
        return false;
    }
    if pattern.get(pi..pi + 2) == Some(b"**") {
        if glob_matches_at(pattern, text, pi + 2, ti, memo) {
            return true;
        }
        let mut cursor = ti;
        while cursor < text.len() {
            cursor += 1;
            if glob_matches_at(pattern, text, pi + 2, cursor, memo) {
                return true;
            }
        }
        return false;
    }
    match pattern[pi] {
        b'*' => {
            if glob_matches_at(pattern, text, pi + 1, ti, memo) {
                return true;
            }
            let mut cursor = ti;
            while cursor < text.len() && text[cursor] != b'/' {
                cursor += 1;
                if glob_matches_at(pattern, text, pi + 1, cursor, memo) {
                    return true;
                }
            }
            false
        }
        b'?' => {
            ti < text.len()
                && text[ti] != b'/'
                && glob_matches_at(pattern, text, pi + 1, ti + 1, memo)
        }
        b'[' => match_class(pattern, text, pi, ti)
            .map(|(next_pi, next_ti)| glob_matches_at(pattern, text, next_pi, next_ti, memo))
            .unwrap_or(false),
        byte => {
            ti < text.len()
                && text[ti] == byte
                && glob_matches_at(pattern, text, pi + 1, ti + 1, memo)
        }
    }
}

fn match_class(pattern: &[u8], text: &[u8], pi: usize, ti: usize) -> Option<(usize, usize)> {
    if ti >= text.len() || text[ti] == b'/' {
        return None;
    }
    let mut cursor = pi + 1;
    let mut negated = false;
    if pattern.get(cursor).copied() == Some(b'!') {
        negated = true;
        cursor += 1;
    }
    let mut matched = false;
    while cursor < pattern.len() && pattern[cursor] != b']' {
        if cursor + 2 < pattern.len() && pattern[cursor + 1] == b'-' && pattern[cursor + 2] != b']'
        {
            matched |= pattern[cursor] <= text[ti] && text[ti] <= pattern[cursor + 2];
            cursor += 3;
        } else {
            matched |= pattern[cursor] == text[ti];
            cursor += 1;
        }
    }
    if cursor >= pattern.len() {
        return None;
    }
    if matched ^ negated {
        Some((cursor + 1, ti + 1))
    } else {
        None
    }
}

fn native_status(path: &Path, relative: &Path, metadata: &fs::Metadata) -> NativeStatus {
    if archive_path_from_relative(relative).is_err() {
        return NativeStatus::WrapFallback("non-utf8-path");
    }
    if metadata.file_type().is_symlink() {
        match fs::read_link(path)
            .ok()
            .and_then(|target| target.to_str().map(str::to_string))
        {
            Some(_) => NativeStatus::Native,
            None => NativeStatus::WrapFallback("non-utf8-symlink-target"),
        }
    } else if metadata.is_file() {
        if has_xattrs(path) {
            return NativeStatus::WrapFallback("xattr");
        }
        if has_multiple_hardlinks(metadata) {
            return NativeStatus::WrapFallback("hardlink");
        }
        NativeStatus::Native
    } else {
        NativeStatus::WrapFallback("unsupported-file-type")
    }
}

#[cfg(unix)]
fn has_multiple_hardlinks(metadata: &fs::Metadata) -> bool {
    metadata.nlink() > 1
}

#[cfg(not(unix))]
fn has_multiple_hardlinks(_metadata: &fs::Metadata) -> bool {
    false
}

fn has_xattrs(path: &Path) -> bool {
    let Ok(output) = Command::new("getfattr")
        .arg("--absolute-names")
        .arg("--dump")
        .arg(path)
        .output()
    else {
        return false;
    };
    output.status.success() && output.stdout.windows(5).any(|bytes| bytes == b"user.")
}

fn create_wrapper_tar(
    tar_engine: &TarEngineReport,
    base_dir: &Path,
    member: &OsStr,
    output: &Path,
) -> Result<(), String> {
    let command_output = Command::new(&tar_engine.program)
        .arg("-c")
        .arg("--format")
        .arg("pax")
        .arg("--xattrs")
        .arg("-f")
        .arg(output)
        .arg("-C")
        .arg(base_dir)
        .arg(member)
        .output()
        .map_err(|error| format!("run {} to create wrapper: {error}", tar_engine.program))?;
    if !command_output.status.success() {
        return Err(format!(
            "{} failed while creating wrapper {} from {}: {}",
            tar_engine.program,
            output.display(),
            base_dir.join(member).display(),
            String::from_utf8_lossy(&command_output.stderr)
        ));
    }
    Ok(())
}

fn extract_wrapper_tar(
    tar_engine: &TarEngineReport,
    dest: &Path,
    wrapper: &Path,
    overwrite: bool,
) -> Result<(), String> {
    validate_wrap_tar_paths(wrapper, tar_engine)?;
    let mut command = Command::new(&tar_engine.program);
    command
        .arg("-x")
        .arg("-p")
        .arg("--xattrs")
        .arg("-f")
        .arg(wrapper)
        .arg("-C")
        .arg(dest);
    if !overwrite {
        command.arg("-k");
    }
    let command_output = command
        .output()
        .map_err(|error| format!("run {} to extract wrapper: {error}", tar_engine.program))?;
    if !command_output.status.success() {
        return Err(format!(
            "{} failed while extracting wrapper {}: {}",
            tar_engine.program,
            wrapper.display(),
            String::from_utf8_lossy(&command_output.stderr)
        ));
    }
    Ok(())
}

fn validate_wrap_tar_paths(wrapper: &Path, tar_engine: &TarEngineReport) -> Result<(), String> {
    let index = build_wrap_index(wrapper, tar_engine)?;
    for entry in index.entries {
        validate_tar_member_path(&entry.path).map_err(|error| {
            format!(
                "wrapper {} has unsafe member path: {error}",
                wrapper.display()
            )
        })?;
    }
    Ok(())
}

fn validate_tar_member_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.as_bytes().contains(&0) || path.starts_with('/') {
        return Err(format!("{path:?} is not a normalized relative path"));
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(format!("{path:?} is not a normalized relative path"));
    }
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(format!("{path:?} is not a normalized relative path"));
        }
    }
    Ok(())
}

fn detect_tar_engine() -> Result<TarEngineReport, String> {
    let program = if command_available("bsdtar") {
        "bsdtar"
    } else if command_available("tar") {
        "tar"
    } else {
        return Err("no supported tar engine found; need bsdtar or tar".to_string());
    };
    let version_output = Command::new(program)
        .arg("--version")
        .output()
        .map_err(|error| format!("run {program} --version: {error}"))?;
    let version = String::from_utf8_lossy(&version_output.stdout)
        .lines()
        .next()
        .unwrap_or(program)
        .to_string();
    Ok(TarEngineReport {
        program: program.to_string(),
        version,
        create_invocation: vec![
            program.to_string(),
            "-c".to_string(),
            "--format".to_string(),
            "pax".to_string(),
            "--xattrs".to_string(),
            "-f".to_string(),
            "<output>".to_string(),
            "-C".to_string(),
            "<base-dir>".to_string(),
            "<member>".to_string(),
        ],
        extract_invocation: vec![
            program.to_string(),
            "-x".to_string(),
            "-p".to_string(),
            "--xattrs".to_string(),
            "-f".to_string(),
            "<wrapper>".to_string(),
            "-C".to_string(),
            "<dest>".to_string(),
        ],
    })
}

fn command_available(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn build_wrap_index(tar_path: &Path, tar_engine: &TarEngineReport) -> Result<WrapIndex, String> {
    let mut file = File::open(tar_path)
        .map_err(|error| format!("open wrapper {}: {error}", tar_path.display()))?;
    let mut entries = Vec::new();
    let mut offset = 0u64;
    let mut pending_pax = BTreeMap::<String, String>::new();
    loop {
        let mut header = [0u8; 512];
        file.read_exact(&mut header)
            .map_err(|error| format!("read wrapper header {}: {error}", tar_path.display()))?;
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let size = parse_tar_size(&header)?;
        let typeflag = header[156];
        let data_offset = offset + 512;
        if typeflag == b'x' {
            let mut data = vec![0u8; usize::try_from(size).map_err(|_| "pax header too large")?];
            file.read_exact(&mut data)
                .map_err(|error| format!("read pax data {}: {error}", tar_path.display()))?;
            pending_pax = parse_pax_records(&data);
            skip_tar_padding(&mut file, size)?;
            offset = next_tar_header_offset(data_offset, size)?;
            continue;
        }
        let name = pending_pax
            .remove("path")
            .unwrap_or_else(|| tar_header_path(&header));
        let kind = match typeflag {
            b'0' | 0 => "regular",
            b'2' => "symlink",
            b'5' => "directory",
            _ => "other",
        };
        let sha256 = if kind == "regular" {
            Some(hash_tar_range(&mut file, data_offset, size)?)
        } else {
            None
        };
        entries.push(WrapIndexEntry {
            path: name,
            kind: kind.to_string(),
            offset: data_offset,
            length: if kind == "regular" { size } else { 0 },
            sha256,
        });
        file.seek(SeekFrom::Start(data_offset))
            .map_err(|error| format!("seek wrapper {}: {error}", tar_path.display()))?;
        skip_tar_payload(&mut file, size)?;
        offset = next_tar_header_offset(data_offset, size)?;
        pending_pax.clear();
    }
    Ok(WrapIndex {
        format: "remanence-remwrap-idx-v1".to_string(),
        tar_engine: tar_engine.clone(),
        entries,
    })
}

fn manifest_kind(kind: &str) -> &'static str {
    match kind {
        "regular" => "regular",
        "directory" => "directory",
        "symlink" => "symlink",
        "other" => "other",
        _ => "other",
    }
}

fn write_wrap_index(path: &Path, index: &WrapIndex) -> Result<(), String> {
    let mut file = File::create(path)
        .map_err(|error| format!("create wrapper index {}: {error}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, index)
        .map_err(|error| format!("serialize wrapper index {}: {error}", path.display()))?;
    file.write_all(b"\n")
        .map_err(|error| format!("write wrapper index {}: {error}", path.display()))
}

fn parse_tar_size(header: &[u8; 512]) -> Result<u64, String> {
    let field = &header[124..136];
    if field[0] & 0x80 != 0 {
        let mut value = 0u64;
        for byte in &field[1..] {
            value = value
                .checked_mul(256)
                .and_then(|value| value.checked_add(u64::from(*byte)))
                .ok_or_else(|| "tar binary size overflows u64".to_string())?;
        }
        return Ok(value);
    }
    let text = field
        .iter()
        .copied()
        .take_while(|byte| *byte != 0 && *byte != b' ')
        .collect::<Vec<_>>();
    let text = String::from_utf8_lossy(&text);
    u64::from_str_radix(text.trim(), 8).map_err(|error| format!("parse tar size {text:?}: {error}"))
}

fn parse_pax_records(data: &[u8]) -> BTreeMap<String, String> {
    let mut records = BTreeMap::new();
    let mut cursor = 0usize;
    while cursor < data.len() {
        let Some(space) = data[cursor..].iter().position(|byte| *byte == b' ') else {
            break;
        };
        let len_text = String::from_utf8_lossy(&data[cursor..cursor + space]);
        let Ok(len) = len_text.parse::<usize>() else {
            break;
        };
        if len == 0 || cursor + len > data.len() {
            break;
        }
        let record = &data[cursor + space + 1..cursor + len - 1];
        if let Some(eq) = record.iter().position(|byte| *byte == b'=') {
            let key = String::from_utf8_lossy(&record[..eq]).to_string();
            let value = String::from_utf8_lossy(&record[eq + 1..]).to_string();
            records.insert(key, value);
        }
        cursor += len;
    }
    records
}

fn tar_header_path(header: &[u8; 512]) -> String {
    let name = nul_trim(&header[0..100]);
    let prefix = nul_trim(&header[345..500]);
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

fn nul_trim(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

fn hash_tar_range(file: &mut File, offset: u64, size: u64) -> Result<String, String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek wrapper payload: {error}"))?;
    let mut remaining = size;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let to_read = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| "tar range chunk too large")?;
        file.read_exact(&mut buffer[..to_read])
            .map_err(|error| format!("read wrapper payload: {error}"))?;
        hasher.update(&buffer[..to_read]);
        remaining -= to_read as u64;
    }
    Ok(bytes_to_hex(&finalize_sha256_local(hasher)))
}

fn skip_tar_payload(file: &mut File, size: u64) -> Result<(), String> {
    let skip = round_up_512(size);
    file.seek(SeekFrom::Current(
        i64::try_from(skip).map_err(|_| "tar payload too large to seek")?,
    ))
    .map_err(|error| format!("seek wrapper payload: {error}"))?;
    Ok(())
}

fn skip_tar_padding(file: &mut File, size: u64) -> Result<(), String> {
    let padding = round_up_512(size)
        .checked_sub(size)
        .ok_or_else(|| "tar padding underflow".to_string())?;
    file.seek(SeekFrom::Current(
        i64::try_from(padding).map_err(|_| "tar padding too large to seek")?,
    ))
    .map_err(|error| format!("seek wrapper padding: {error}"))?;
    Ok(())
}

fn next_tar_header_offset(data_offset: u64, size: u64) -> Result<u64, String> {
    data_offset
        .checked_add(round_up_512(size))
        .ok_or_else(|| "tar offset overflows".to_string())
}

fn round_up_512(value: u64) -> u64 {
    value.div_ceil(512) * 512
}

fn collect_remwrap_files(root: &Path, wrappers: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|error| format!("read {}: {error}", root.display()))? {
        let entry = entry.map_err(|error| format!("read {}: {error}", root.display()))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("stat restore path {}: {error}", path.display()))?;
        if metadata.is_dir() {
            collect_remwrap_files(&path, wrappers)?;
        } else if metadata.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.ends_with(WRAP_TAR_SUFFIX))
                .unwrap_or(false)
        {
            wrappers.push(path);
        }
    }
    Ok(())
}

impl PlannerState {
    fn record_native(&mut self, rel_text: &str, bytes: u64) {
        self.totals.native_entries += 1;
        self.record_dir_stats(rel_text, false);
        self.record_cluster(rel_text, "native", 1, bytes);
    }

    fn record_wrapped(&mut self, rel_text: &str, reason: &str, bytes: u64) {
        self.totals.wrapped_entries += 1;
        self.record_dir_stats(rel_text, true);
        self.record_cluster(rel_text, reason, 1, bytes);
    }

    fn record_blob(&mut self, rel_text: &str, bytes: u64) {
        self.totals.blob_entries += 1;
        self.record_dir_stats(rel_text, false);
        self.record_cluster(rel_text, "blob-rule", 1, bytes);
    }

    fn record_dir_stats(&mut self, rel_text: &str, noncompliant: bool) {
        let mut current = String::new();
        self.bump_dir_stat(".", noncompliant);
        for component in rel_text.split('/').filter(|part| !part.is_empty()) {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(component);
            self.bump_dir_stat(&current, noncompliant);
        }
    }

    fn bump_dir_stat(&mut self, prefix: &str, noncompliant: bool) {
        let stats = self.dir_stats.entry(prefix.to_string()).or_default();
        stats.total += 1;
        if noncompliant {
            stats.noncompliant += 1;
        }
    }

    fn record_cluster(&mut self, rel_text: &str, reason: &str, count: u64, bytes: u64) {
        let prefix = directory_prefix(rel_text);
        let cluster = self
            .clusters
            .entry((prefix, reason.to_string()))
            .or_default();
        cluster.count += count;
        cluster.bytes += bytes;
        if cluster.samples.len() < DEFAULT_SAMPLES_PER_CLUSTER {
            cluster.samples.push(rel_text.to_string());
        }
    }

    fn scan_report(&self, tuning: ScanTuning) -> ScanReport {
        let clusters = self
            .clusters
            .iter()
            .map(|((prefix, reason), cluster)| ScanCluster {
                prefix: prefix.clone(),
                reason: reason.clone(),
                count: cluster.count,
                bytes: cluster.bytes,
                samples: cluster.samples.clone(),
            })
            .collect::<Vec<_>>();
        let mut suggestions = self
            .dir_stats
            .iter()
            .filter_map(|(prefix, stats)| {
                if stats.total == 0 || stats.noncompliant == 0 {
                    return None;
                }
                let ratio = stats.noncompliant as f64 / stats.total as f64;
                if stats.noncompliant >= tuning.sanity_ceiling {
                    Some(BlobSuggestion {
                        prefix: prefix.clone(),
                        noncompliant: stats.noncompliant,
                        total: stats.total,
                        ratio,
                        verdict: "sanity-ceiling",
                    })
                } else if ratio >= tuning.blob_ratio && stats.noncompliant >= tuning.blob_count {
                    Some(BlobSuggestion {
                        prefix: prefix.clone(),
                        noncompliant: stats.noncompliant,
                        total: stats.total,
                        ratio,
                        verdict: "blob-suggest",
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        suggestions.sort_by(|a, b| {
            a.prefix
                .matches('/')
                .count()
                .cmp(&b.prefix.matches('/').count())
                .then_with(|| a.prefix.cmp(&b.prefix))
        });
        ScanReport {
            totals: self.totals.clone(),
            clusters,
            blob_suggestions: suggestions,
        }
    }
}

fn subtree_count_bytes(path: &Path) -> Result<(u64, u64), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.is_dir() {
        return Ok((1, metadata.len()));
    }
    let mut count = 1;
    let mut bytes = 0;
    for entry in fs::read_dir(path).map_err(|error| format!("read {}: {error}", path.display()))? {
        let entry = entry.map_err(|error| format!("read {}: {error}", path.display()))?;
        let (child_count, child_bytes) = subtree_count_bytes(&entry.path())?;
        count += child_count;
        bytes += child_bytes;
    }
    Ok((count, bytes))
}

fn ensure_unique_archive_paths(files: &[ArchiveBuildInputFile]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for file in files {
        if !seen.insert(file.archive_path.clone()) {
            return Err(format!("duplicate archive path {:?}", file.archive_path));
        }
    }
    Ok(())
}

fn hash_for_manifest(path: &Path) -> Result<(u64, [u8; 32]), String> {
    let hash = sha256_file(path)?;
    let size = fs::metadata(path)
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .len();
    Ok((size, hash))
}

fn relative_match_text(relative: &Path) -> String {
    if relative.as_os_str().is_empty() {
        String::new()
    } else {
        relative
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(part) => Some(part.to_string_lossy()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/")
    }
}

fn sanitized_or_native_archive_path(relative: &Path) -> String {
    archive_path_from_relative(relative).unwrap_or_else(|_| {
        format!(
            "{}-{}",
            sanitized_component(relative.file_name().unwrap_or_else(|| OsStr::new("entry"))),
            short_path_hash(relative.as_os_str())
        )
    })
}

fn sanitized_component(component: &OsStr) -> String {
    let mut out = String::new();
    for ch in component.to_string_lossy().chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('.');
    if trimmed.is_empty() {
        format!("entry-{}", short_path_hash(component))
    } else {
        trimmed.to_string()
    }
}

fn short_path_hash(value: &OsStr) -> String {
    #[cfg(unix)]
    let bytes = value.as_bytes();
    #[cfg(not(unix))]
    let owned = value.to_string_lossy();
    #[cfg(not(unix))]
    let bytes = owned.as_bytes();
    bytes_to_hex(&sha256_bytes_local(bytes))[..12].to_string()
}

fn uniquify_archive_path(candidate: &str, files: &[ArchiveBuildInputFile]) -> String {
    if !files.iter().any(|file| file.archive_path == candidate) {
        return candidate.to_string();
    }
    let (stem, suffix) = candidate
        .strip_suffix(WRAP_TAR_SUFFIX)
        .map(|stem| (stem, WRAP_TAR_SUFFIX))
        .unwrap_or((candidate, ""));
    for counter in 1u64.. {
        let next = format!("{stem}-{counter}{suffix}");
        if !files.iter().any(|file| file.archive_path == next) {
            return next;
        }
    }
    unreachable!("u64 archive path suffix space exhausted")
}

fn next_temp_path(tempdir: &Path, counter: &mut u64, suffix: &str) -> PathBuf {
    *counter += 1;
    tempdir.join(format!("remwrap-{counter:06}-{suffix}"))
}

fn directory_prefix(rel_text: &str) -> String {
    match rel_text.rsplit_once('/') {
        Some((prefix, _)) if !prefix.is_empty() => prefix.to_string(),
        _ => ".".to_string(),
    }
}

fn trim_dir_slash(value: &str) -> &str {
    value.trim_end_matches('/')
}

fn remwrap_index_path(wrapper_path: &str) -> Result<String, String> {
    wrapper_path
        .strip_suffix(WRAP_TAR_SUFFIX)
        .map(|stem| format!("{stem}{WRAP_INDEX_SUFFIX}"))
        .ok_or_else(|| format!("wrapper path {wrapper_path:?} does not end in {WRAP_TAR_SUFFIX}"))
}

fn remwrap_index_filesystem_path(wrapper_path: &Path) -> Option<PathBuf> {
    let file_name = wrapper_path.file_name()?.to_str()?;
    let stem = file_name.strip_suffix(WRAP_TAR_SUFFIX)?;
    let mut idx = wrapper_path.to_path_buf();
    idx.set_file_name(format!("{stem}{WRAP_INDEX_SUFFIX}"));
    Some(idx)
}

fn sha256_bytes_local(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    finalize_sha256_local(hasher)
}

fn finalize_sha256_local(hasher: Sha256) -> [u8; 32] {
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ruleset_first_match_and_lints_are_reported() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-ruleset-test")
            .tempdir()
            .unwrap();
        let rules = temp.path().join("fcp.rules");
        fs::write(
            &rules,
            "\
option case-insensitive
blob **
exclude **/Cache/
blob Project/Render Files/
blob Project/Render Files/
",
        )
        .unwrap();

        let ruleset = load_ruleset(&rules).unwrap();
        assert!(ruleset.report.case_insensitive);
        assert_eq!(
            decide(Some(&ruleset), "Project/Render Files", true),
            Decision::Blob { no_index: false }
        );
        assert!(ruleset
            .lints
            .iter()
            .any(|lint| lint.kind == "unreachable-after-catch-all"));
        assert!(ruleset
            .lints
            .iter()
            .any(|lint| lint.kind == "duplicate-rule"));
    }

    #[test]
    fn glob_patterns_match_gitignore_style_examples() {
        let p = Pattern::parse("**/Render Files/");
        assert!(p.matches("Project/Render Files", true, false));
        assert!(p.matches("Render Files", true, false));
        assert!(!p.matches("Project/Render Files/file.mov", false, false));
        let p = Pattern::parse("**/*.[fF][cC][pP]bundle/");
        assert!(p.matches("Cut/Show.fcpbundle", true, false));
        assert!(!p.matches("Cut/Show.fcpxml", false, false));
    }

    #[cfg(unix)]
    #[test]
    fn bsdtar_wrapper_round_trips_required_fidelity_cases() {
        if !command_available("bsdtar")
            || !command_available("setfattr")
            || !command_available("getfattr")
        {
            eprintln!("skipping wrapper fidelity test; bsdtar/setfattr/getfattr unavailable");
            return;
        }
        let temp = tempfile::Builder::new()
            .prefix("remanence-remwrap-fidelity")
            .tempdir()
            .unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("empty")).unwrap();
        fs::write(source.join("plain.txt"), b"plain").unwrap();
        fs::write(source.join("._plain.txt"), b"appledouble").unwrap();
        std::os::unix::fs::symlink("missing-target", source.join("dangling")).unwrap();
        let non_utf8 = source.join(OsStr::from_bytes(b"bad-\xff-name"));
        fs::write(&non_utf8, b"raw-name").unwrap();
        let xattr_file = source.join("xattr.txt");
        fs::write(&xattr_file, b"xattr").unwrap();
        let status = Command::new("setfattr")
            .arg("-n")
            .arg("user.remanence_test")
            .arg("-v")
            .arg("kept")
            .arg(&xattr_file)
            .status()
            .unwrap();
        assert!(status.success());

        let tar_engine = detect_tar_engine().unwrap();
        let tar_path = temp.path().join("source.remwrap.tar");
        create_wrapper_tar(
            &tar_engine,
            source.parent().unwrap(),
            source.file_name().unwrap(),
            &tar_path,
        )
        .unwrap();

        let restore = temp.path().join("restore");
        fs::create_dir_all(&restore).unwrap();
        extract_wrapper_tar(&tar_engine, &restore, &tar_path, true).unwrap();
        let restored_source = restore.join("source");
        assert!(restored_source.join("empty").is_dir());
        assert_eq!(
            fs::read(restored_source.join("plain.txt")).unwrap(),
            b"plain"
        );
        assert_eq!(
            fs::read(restored_source.join("._plain.txt")).unwrap(),
            b"appledouble"
        );
        assert_eq!(
            fs::read_link(restored_source.join("dangling")).unwrap(),
            PathBuf::from("missing-target")
        );
        assert_eq!(
            fs::read(restored_source.join(OsStr::from_bytes(b"bad-\xff-name"))).unwrap(),
            b"raw-name"
        );
        let output = Command::new("getfattr")
            .arg("--absolute-names")
            .arg("--dump")
            .arg(restored_source.join("xattr.txt"))
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("user.remanence_test=\"kept\""), "{stdout}");
    }
}
