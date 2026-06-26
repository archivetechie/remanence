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
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{
    archive_path_from_relative, bytes_to_hex, path_component_to_string,
    read_archive_build_directory, read_archive_build_file, read_archive_build_file_with_xattrs,
    read_archive_build_hardlink, read_archive_build_symlink, sha256_file, ArchiveBuildInputFile,
};

const WRAP_TAR_SUFFIX: &str = ".remwrap.tar";
const WRAP_INDEX_SUFFIX: &str = ".remwrap.idx";
const DEFAULT_SAMPLES_PER_CLUSTER: usize = 3;
const XATTR_SINGLE_VALUE_LIMIT: usize = 4 * 1024;
const XATTR_TOTAL_VALUE_LIMIT: usize = 16 * 1024;

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
    pub(crate) xattr_drops: Vec<XattrDropCluster>,
    pub(crate) blob_suggestions: Vec<BlobSuggestion>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct ScanTotals {
    pub(crate) native_entries: u64,
    pub(crate) wrapped_entries: u64,
    pub(crate) blob_entries: u64,
    pub(crate) excluded_entries: u64,
    pub(crate) excluded_bytes: u64,
    pub(crate) dropped_xattrs: u64,
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
pub(crate) struct XattrDropCluster {
    pub(crate) prefix: String,
    pub(crate) name: String,
    pub(crate) reason: String,
    pub(crate) count: u64,
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
    pub(crate) mtime: Option<String>,
    pub(crate) wrapper: Option<String>,
}

#[derive(Debug)]
struct Ruleset {
    report: RulesetReport,
    rules: Vec<Rule>,
    xattr_policy: XattrPolicy,
    lints: Vec<RulesetLint>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum XattrMode {
    Denylist,
    Allowlist,
}

#[derive(Debug, Clone)]
struct XattrPolicy {
    mode: XattrMode,
    keep: BTreeSet<String>,
    drop: BTreeSet<String>,
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

#[derive(Debug, Clone, Eq, PartialEq)]
enum NativeStatus {
    Native { xattrs: BTreeMap<String, Vec<u8>> },
    WrapFallback(&'static str),
}

#[derive(Debug, Clone, Eq, PartialEq)]
enum LeafClassification {
    Exclude,
    Native { xattrs: BTreeMap<String, Vec<u8>> },
    WrapFallback(&'static str),
}

#[derive(Debug, Default)]
struct PlannerState {
    files: Vec<ArchiveBuildInputFile>,
    manifest_entries: Vec<CustomerManifestEntry>,
    clusters: BTreeMap<(String, String), ClusterAccumulator>,
    xattr_drops: BTreeMap<(String, String, String), ClusterAccumulator>,
    dir_stats: BTreeMap<String, DirStats>,
    totals: ScanTotals,
    wrapper_counter: u64,
    hardlink_primaries: BTreeMap<FileKey, String>,
    hardlink_native_counts: BTreeMap<FileKey, u64>,
    hardlink_link_counts: BTreeMap<FileKey, u64>,
    hardlink_paths: BTreeMap<FileKey, Vec<String>>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct FileKey {
    dev: u64,
    ino: u64,
}

#[derive(Clone, Copy)]
struct ProcessContext<'a> {
    ruleset: Option<&'a Ruleset>,
    xattr_policy: &'a XattrPolicy,
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
    mtime: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlobMemberRange {
    pub(crate) offset: u64,
    pub(crate) length: u64,
    pub(crate) sha256: Option<String>,
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
    let default_xattr_policy = XattrPolicy::default();
    let xattr_policy = ruleset
        .as_ref()
        .map(|ruleset| &ruleset.xattr_policy)
        .unwrap_or(&default_xattr_policy);
    let tempdir = tempfile::Builder::new()
        .prefix("remanence-remwrap-")
        .tempdir()
        .map_err(|error| format!("create wrapper tempdir: {error}"))?;
    let mut state = PlannerState::default();
    for input in input_paths {
        let context = ProcessContext {
            ruleset: ruleset.as_ref(),
            xattr_policy,
            tar_engine: &tar_engine,
            tempdir: tempdir.path(),
            no_index,
        };
        process_input(input, context, &mut state)?;
    }
    state.record_hardlink_splits();
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
    let tar_engine = detect_tar_engine()?;
    let ruleset = match rules_path {
        Some(path) => Some(load_ruleset(path)?),
        None => None,
    };
    let default_xattr_policy = XattrPolicy::default();
    let xattr_policy = ruleset
        .as_ref()
        .map(|ruleset| &ruleset.xattr_policy)
        .unwrap_or(&default_xattr_policy);
    let mut state = PlannerState::default();
    let context = ProcessContext {
        ruleset: ruleset.as_ref(),
        xattr_policy,
        tar_engine: &tar_engine,
        tempdir: Path::new(""),
        no_index,
    };
    for input in input_paths {
        scan_input(input, context, &mut state)?;
    }
    state.record_hardlink_splits();
    let lints = ruleset
        .as_ref()
        .map(|ruleset| ruleset.lints.clone())
        .unwrap_or_default();
    let ruleset_report = ruleset.as_ref().map(|ruleset| ruleset.report.clone());
    Ok(IngestReport {
        ruleset: ruleset_report,
        tar_engine,
        scan: state.scan_report(tuning),
        lints,
    })
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

pub(crate) fn resolve_blob_member_from_index(
    index_bytes: &[u8],
    idx_path: &str,
    member_path: &str,
) -> Result<BlobMemberRange, String> {
    let index: WrapIndex = serde_json::from_slice(index_bytes)
        .map_err(|error| format!("parse blob index {idx_path:?}: {error}"))?;
    if index.format != "remanence-remwrap-idx-v1" {
        return Err(format!(
            "blob index {idx_path:?} has unsupported format {:?}",
            index.format
        ));
    }
    let requested = decode_member_name(member_path)
        .map_err(|error| format!("decode requested blob member {member_path:?}: {error}"))?;
    let mut member = None;
    for entry in &index.entries {
        let stored = decode_member_name(&entry.path)
            .map_err(|error| format!("decode blob index member {:?}: {error}", entry.path))?;
        if stored == requested {
            member = Some(entry);
            break;
        }
    }
    let member =
        member.ok_or_else(|| format!("blob member {member_path:?} not found in {idx_path:?}"))?;
    if member.kind != "regular" {
        return Err(format!(
            "blob member {member_path:?} is {}, not regular",
            member.kind
        ));
    }
    member
        .offset
        .checked_add(member.length)
        .ok_or_else(|| format!("blob member {member_path:?} offset/length overflows"))?;
    let sha256 = member
        .sha256
        .clone()
        .ok_or_else(|| format!("blob member {member_path:?} is missing sha256 in {idx_path:?}"))?;
    Ok(BlobMemberRange {
        offset: member.offset,
        length: member.length,
        sha256: Some(sha256),
    })
}

pub(crate) fn verify_blob_member_bytes(
    member_path: &str,
    expected_sha256: Option<&str>,
    bytes: &[u8],
) -> Result<(), String> {
    if let Some(expected) = expected_sha256 {
        let actual = bytes_to_hex(&sha256_bytes_local(bytes));
        if actual != expected {
            return Err(format!(
                "blob member {member_path:?} digest mismatch: expected {expected}, got {actual}"
            ));
        }
    }
    Ok(())
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
        wrap_leaf(
            input,
            &relative,
            &metadata,
            "unsupported-file-type",
            context,
            state,
        )
    }
}

fn scan_input(
    input: &Path,
    context: ProcessContext<'_>,
    state: &mut PlannerState,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(input)
        .map_err(|error| format!("stat input {}: {error}", input.display()))?;
    if metadata.file_type().is_symlink() || metadata.is_file() || !metadata.is_dir() {
        let relative = PathBuf::from(input.file_name().unwrap_or_else(|| OsStr::new("input")));
        scan_leaf(input, &relative, &metadata, context, state)
    } else {
        scan_dir(input, Path::new(""), context, state).map(|_| ())
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
            state.totals.excluded_entries = state.totals.excluded_entries.saturating_add(count);
            state.totals.excluded_bytes = state.totals.excluded_bytes.saturating_add(bytes);
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
                "blob-rule",
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
                "blob-rule",
                state,
            )?;
            return Ok(true);
        }
        Decision::Granular => {}
    }
    if !relative.as_os_str().is_empty() {
        let metadata = fs::symlink_metadata(dir)
            .map_err(|error| format!("stat input {}: {error}", dir.display()))?;
        if let NativeStatus::WrapFallback(reason) =
            native_status(dir, relative, &metadata, context, state)?
        {
            materialize_blob(root, dir, relative, context, reason, state)?;
            return Ok(true);
        }
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
            let metadata = fs::symlink_metadata(dir)
                .map_err(|error| format!("stat input {}: {error}", dir.display()))?;
            state.manifest_entries.push(CustomerManifestEntry {
                path: rel_text,
                kind: "directory",
                size_bytes: 0,
                sha256: None,
                mtime: metadata_mtime(dir, &metadata)?,
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

fn scan_dir(
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
            state.totals.excluded_entries = state.totals.excluded_entries.saturating_add(count);
            state.totals.excluded_bytes = state.totals.excluded_bytes.saturating_add(bytes);
            return Ok(true);
        }
        Decision::Blob { .. } if !relative.as_os_str().is_empty() => {
            let (_, bytes) = subtree_count_bytes(dir)?;
            state.record_blob(&rel_text, bytes, "blob-rule");
            return Ok(true);
        }
        Decision::Blob { .. } => {
            let (_, bytes) = subtree_count_bytes(dir)?;
            state.record_blob(&rel_text, bytes, "blob-rule");
            return Ok(true);
        }
        Decision::Granular => {}
    }

    let metadata = fs::symlink_metadata(dir)
        .map_err(|error| format!("stat input {}: {error}", dir.display()))?;
    if !relative.as_os_str().is_empty() {
        if let NativeStatus::WrapFallback(reason) =
            native_status(dir, relative, &metadata, context, state)?
        {
            let (_, bytes) = subtree_count_bytes(dir)?;
            state.record_wrapped(&rel_text, reason, bytes);
            return Ok(true);
        }
    }

    let mut entries = fs::read_dir(dir)
        .map_err(|error| format!("read directory {}: {error}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read directory {}: {error}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    if entries.is_empty() {
        if !relative.as_os_str().is_empty() {
            state.record_native(&rel_text, 0);
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
            scan_leaf(&path, &child_relative, &metadata, context, state)?;
            added_any = true;
        } else if scan_dir(&path, &child_relative, context, state)? {
            added_any = true;
        }
    }
    Ok(added_any)
}

fn scan_leaf(
    path: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
    context: ProcessContext<'_>,
    state: &mut PlannerState,
) -> Result<(), String> {
    let rel_text = relative_match_text(relative);
    match classify_leaf(path, relative, metadata, context, state)? {
        LeafClassification::Exclude => {
            let bytes = metadata.len();
            state.record_cluster(&rel_text, "exclude-rule", 1, bytes);
            state.totals.excluded_entries = state.totals.excluded_entries.saturating_add(1);
            state.totals.excluded_bytes = state.totals.excluded_bytes.saturating_add(bytes);
        }
        LeafClassification::Native { .. } if metadata.is_file() => {
            state.note_native_hardlink(path, metadata, &rel_text, "");
            state.record_native(&rel_text, metadata.len());
        }
        LeafClassification::Native { .. } => {
            state.record_native(&rel_text, 0);
        }
        LeafClassification::WrapFallback(reason) => {
            state.record_wrapped(&rel_text, reason, metadata.len());
        }
    }
    Ok(())
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
    match classify_leaf(path, relative, metadata, context, state)? {
        LeafClassification::Exclude => {
            let bytes = metadata.len();
            state.record_cluster(&rel_text, "exclude-rule", 1, bytes);
            state.totals.excluded_entries = state.totals.excluded_entries.saturating_add(1);
            state.totals.excluded_bytes = state.totals.excluded_bytes.saturating_add(bytes);
            Ok(())
        }
        LeafClassification::Native { .. } if metadata.file_type().is_symlink() => {
            state.record_native(&rel_text, 0);
            state
                .files
                .push(read_archive_build_symlink(path, archive_path)?);
            state.manifest_entries.push(CustomerManifestEntry {
                path: rel_text,
                kind: "symlink",
                size_bytes: 0,
                sha256: None,
                mtime: metadata_mtime(path, metadata)?,
                wrapper: None,
            });
            Ok(())
        }
        LeafClassification::Native { xattrs } if metadata.is_file() => {
            if let Some(link_target) =
                state.note_native_hardlink(path, metadata, &rel_text, &archive_path)
            {
                state.record_native(&rel_text, 0);
                state.files.push(read_archive_build_hardlink(
                    path,
                    archive_path,
                    link_target.clone(),
                )?);
                state.manifest_entries.push(CustomerManifestEntry {
                    path: rel_text,
                    kind: "hardlink",
                    size_bytes: 0,
                    sha256: None,
                    mtime: metadata_mtime(path, metadata)?,
                    wrapper: None,
                });
                return Ok(());
            }
            let (size, hash) = hash_for_manifest(path)?;
            state.record_native(&rel_text, size);
            state.files.push(read_archive_build_file_with_xattrs(
                path,
                archive_path,
                xattrs,
            )?);
            state.manifest_entries.push(CustomerManifestEntry {
                path: rel_text,
                kind: "regular",
                size_bytes: size,
                sha256: Some(bytes_to_hex(&hash)),
                mtime: metadata_mtime(path, metadata)?,
                wrapper: None,
            });
            Ok(())
        }
        LeafClassification::Native { .. } => Err(format!(
            "internal native status bug for unsupported path {}",
            path.display()
        )),
        LeafClassification::WrapFallback(reason) => {
            wrap_leaf(path, relative, metadata, reason, context, state)
        }
    }
}

fn classify_leaf(
    path: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
    context: ProcessContext<'_>,
    state: &mut PlannerState,
) -> Result<LeafClassification, String> {
    let rel_text = relative_match_text(relative);
    match decide(context.ruleset, &rel_text, false) {
        Decision::Exclude => Ok(LeafClassification::Exclude),
        Decision::Blob { .. } => Err(format!(
            "blob rule matched non-directory path {rel_text:?}; blob patterns must select directories"
        )),
        Decision::Granular => match native_status(path, relative, metadata, context, state)? {
            NativeStatus::Native { xattrs } => Ok(LeafClassification::Native { xattrs }),
            NativeStatus::WrapFallback(reason) => Ok(LeafClassification::WrapFallback(reason)),
        },
    }
}

fn wrap_leaf(
    path: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
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
        mtime: metadata_mtime(path, metadata)?,
        wrapper: Some(wrapper_archive_path),
    });
    Ok(())
}

fn materialize_blob(
    root: &Path,
    dir: &Path,
    relative: &Path,
    context: ProcessContext<'_>,
    reason: &'static str,
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
    let source_metadata = fs::symlink_metadata(dir)
        .map_err(|error| format!("stat input {}: {error}", dir.display()))?;
    let source_mtime = metadata_mtime(dir, &source_metadata)?;
    add_blob_outputs(
        &rel_text,
        &wrapper_archive_path,
        &tar_path,
        source_mtime,
        context,
        reason,
        state,
    )?;
    Ok(())
}

fn materialize_root_blob(
    dir: &Path,
    name: &OsStr,
    context: ProcessContext<'_>,
    reason: &'static str,
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
    let source_metadata = fs::symlink_metadata(dir)
        .map_err(|error| format!("stat input {}: {error}", dir.display()))?;
    let source_mtime = metadata_mtime(dir, &source_metadata)?;
    add_blob_outputs(
        &rel_text,
        &wrapper_archive_path,
        &tar_path,
        source_mtime,
        context,
        reason,
        state,
    )
}

fn add_blob_outputs(
    rel_text: &str,
    wrapper_archive_path: &str,
    tar_path: &Path,
    source_mtime: Option<String>,
    context: ProcessContext<'_>,
    reason: &'static str,
    state: &mut PlannerState,
) -> Result<(), String> {
    let (tar_size, tar_hash) = hash_for_manifest(tar_path)?;
    let index = build_wrap_index(tar_path, context.tar_engine)?;
    state.record_blob(rel_text, tar_size, reason);
    state.files.push(read_archive_build_file(
        tar_path,
        wrapper_archive_path.to_string(),
    )?);
    state.manifest_entries.push(CustomerManifestEntry {
        path: rel_text.to_string(),
        kind: "blob",
        size_bytes: tar_size,
        sha256: Some(bytes_to_hex(&tar_hash)),
        mtime: source_mtime,
        wrapper: Some(wrapper_archive_path.to_string()),
    });
    for entry in &index.entries {
        state.manifest_entries.push(CustomerManifestEntry {
            path: entry.path.clone(),
            kind: manifest_kind(entry.kind.as_str()),
            size_bytes: entry.length,
            sha256: entry.sha256.clone(),
            mtime: entry.mtime.clone(),
            wrapper: Some(wrapper_archive_path.to_string()),
        });
    }
    if !context.no_index {
        let idx_path = next_temp_path(context.tempdir, &mut state.wrapper_counter, "blob.idx");
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
    let mut xattr_policy = XattrPolicy::default();
    let mut rules = Vec::new();
    for (line_index, original) in text.lines().enumerate() {
        let line_no = line_index + 1;
        let line = strip_comment(original).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(option) = line.strip_prefix("option ") {
            let option = option.trim();
            if let Some(mode) = option.strip_prefix("xattr-mode ") {
                xattr_policy.mode = match mode.trim() {
                    "denylist" => XattrMode::Denylist,
                    "allowlist" => XattrMode::Allowlist,
                    other => {
                        return Err(format!(
                            "{}:{line_no}: unsupported xattr-mode {other:?}",
                            path.display()
                        ))
                    }
                };
                continue;
            }
            match option {
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
        if let Some(name) = line.strip_prefix("xattr-keep ") {
            let name = name.trim();
            validate_xattr_policy_name(path, line_no, name)?;
            xattr_policy.keep.insert(name.to_string());
            continue;
        }
        if let Some(name) = line.strip_prefix("xattr-drop ") {
            let name = name.trim();
            validate_xattr_policy_name(path, line_no, name)?;
            xattr_policy.drop.insert(name.to_string());
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
    validate_xattr_policy(path, &xattr_policy)?;
    Ok(Ruleset {
        report,
        rules,
        xattr_policy,
        lints,
    })
}

impl Default for XattrPolicy {
    fn default() -> Self {
        Self {
            mode: XattrMode::Denylist,
            keep: BTreeSet::new(),
            drop: BTreeSet::new(),
        }
    }
}

fn validate_xattr_policy_name(path: &Path, line_no: usize, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("{}:{line_no}: missing xattr name", path.display()));
    }
    if name.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
        return Err(format!(
            "{}:{line_no}: xattr name {name:?} contains a control character",
            path.display()
        ));
    }
    Ok(())
}

fn validate_xattr_policy(path: &Path, policy: &XattrPolicy) -> Result<(), String> {
    match policy.mode {
        XattrMode::Denylist if !policy.keep.is_empty() => Err(format!(
            "{}: xattr-keep requires 'option xattr-mode allowlist'",
            path.display()
        )),
        XattrMode::Allowlist if !policy.drop.is_empty() => Err(format!(
            "{}: xattr-drop requires 'option xattr-mode denylist'",
            path.display()
        )),
        _ => Ok(()),
    }
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

fn native_status(
    path: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
    context: ProcessContext<'_>,
    state: &mut PlannerState,
) -> Result<NativeStatus, String> {
    if archive_path_from_relative(relative).is_err() {
        return Ok(NativeStatus::WrapFallback("non-utf8-path"));
    }
    let rel_text = relative_match_text(relative);
    let status = if metadata.file_type().is_symlink() {
        match fs::read_link(path)
            .ok()
            .and_then(|target| target.to_str().map(str::to_string))
        {
            Some(_) => NativeStatus::Native {
                xattrs: BTreeMap::new(),
            },
            None => NativeStatus::WrapFallback("non-utf8-symlink-target"),
        }
    } else if metadata.is_file() {
        match collect_preserved_xattrs(path, &rel_text, context.xattr_policy, state)? {
            XattrCollection::Preserve(xattrs) => NativeStatus::Native { xattrs },
            XattrCollection::Wrap(reason) => NativeStatus::WrapFallback(reason),
        }
    } else if metadata.is_dir() {
        match collect_preserved_xattrs(path, &rel_text, context.xattr_policy, state)? {
            XattrCollection::Preserve(xattrs) if xattrs.is_empty() => NativeStatus::Native {
                xattrs: BTreeMap::new(),
            },
            XattrCollection::Preserve(_) => NativeStatus::WrapFallback("xattr"),
            XattrCollection::Wrap(reason) => NativeStatus::WrapFallback(reason),
        }
    } else {
        NativeStatus::WrapFallback("unsupported-file-type")
    };
    Ok(status)
}

enum XattrCollection {
    Preserve(BTreeMap<String, Vec<u8>>),
    Wrap(&'static str),
}

fn collect_preserved_xattrs(
    path: &Path,
    rel_text: &str,
    policy: &XattrPolicy,
    state: &mut PlannerState,
) -> Result<XattrCollection, String> {
    let names = match xattr::list(path) {
        Ok(names) => names.collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
            return Ok(XattrCollection::Preserve(BTreeMap::new()))
        }
        Err(error) => return Err(format!("list xattrs for {}: {error}", path.display())),
    };
    let mut kept = BTreeMap::new();
    let mut total_size = 0usize;
    for name in names {
        let Some(name_text) = name.to_str() else {
            return Ok(XattrCollection::Wrap("xattr-name"));
        };
        if should_drop_xattr(name_text, policy) {
            let reason = if builtin_junk_xattr(name_text) {
                "baseline"
            } else {
                "policy"
            };
            state.record_xattr_drop(rel_text, name_text, reason);
            continue;
        }
        let Some(value) = xattr::get(path, &name)
            .map_err(|error| format!("read xattr {name_text:?} for {}: {error}", path.display()))?
        else {
            continue;
        };
        if value.len() > XATTR_SINGLE_VALUE_LIMIT {
            return Ok(XattrCollection::Wrap("xattr-large"));
        }
        total_size = total_size
            .checked_add(value.len())
            .ok_or_else(|| format!("xattr total size overflows for {}", path.display()))?;
        if total_size > XATTR_TOTAL_VALUE_LIMIT {
            return Ok(XattrCollection::Wrap("xattr-large"));
        }
        kept.insert(name_text.to_string(), value);
    }
    Ok(XattrCollection::Preserve(kept))
}

fn should_drop_xattr(name: &str, policy: &XattrPolicy) -> bool {
    if builtin_junk_xattr(name) {
        return true;
    }
    match policy.mode {
        XattrMode::Denylist => policy.drop.contains(name),
        XattrMode::Allowlist => !policy.keep.contains(name),
    }
}

fn builtin_junk_xattr(name: &str) -> bool {
    matches!(
        name,
        "com.apple.quarantine"
            | "com.apple.metadata:kMDItemWhereFroms"
            | "com.apple.lastuseddate#PS"
            | "com.apple.FinderInfo"
            | "com.apple.metadata:_kMDItemFinderComment"
            | "com.apple.metadata:kMDItemDownloadedDate"
    )
}

#[cfg(unix)]
fn file_key(metadata: &fs::Metadata) -> Option<FileKey> {
    (metadata.nlink() > 1).then_some(FileKey {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn file_key(_metadata: &fs::Metadata) -> Option<FileKey> {
    None
}

#[cfg(unix)]
fn hardlink_count(metadata: &fs::Metadata) -> u64 {
    metadata.nlink()
}

#[cfg(not(unix))]
fn hardlink_count(_metadata: &fs::Metadata) -> u64 {
    1
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
        .arg("--")
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

fn validate_wrap_tar_paths(wrapper: &Path, _tar_engine: &TarEngineReport) -> Result<(), String> {
    let mut file = File::open(wrapper)
        .map_err(|error| format!("open wrapper {}: {error}", wrapper.display()))?;
    let mut offset = 0u64;
    let mut pending_pax = BTreeMap::<String, Vec<u8>>::new();
    loop {
        let mut header = [0u8; 512];
        file.read_exact(&mut header)
            .map_err(|error| format!("read wrapper header {}: {error}", wrapper.display()))?;
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let size = parse_tar_size(&header)?;
        let typeflag = header[156];
        let data_offset = offset
            .checked_add(512)
            .ok_or_else(|| "tar data offset overflows".to_string())?;
        if typeflag == b'x' {
            let mut data = vec![0u8; usize::try_from(size).map_err(|_| "pax header too large")?];
            file.read_exact(&mut data)
                .map_err(|error| format!("read pax data {}: {error}", wrapper.display()))?;
            pending_pax = parse_pax_records(&data)?;
            skip_tar_padding(&mut file, size)?;
            offset = next_tar_header_offset(data_offset, size)?;
            continue;
        }
        let name = pending_pax
            .get("path")
            .cloned()
            .unwrap_or_else(|| tar_header_path_bytes(&header));
        validate_tar_member_path(&name).map_err(|error| {
            format!(
                "wrapper {} has unsafe member path: {error}",
                wrapper.display()
            )
        })?;
        file.seek(SeekFrom::Start(data_offset))
            .map_err(|error| format!("seek wrapper {}: {error}", wrapper.display()))?;
        skip_tar_payload(&mut file, size)?;
        offset = next_tar_header_offset(data_offset, size)?;
        pending_pax.clear();
    }
    Ok(())
}

fn validate_tar_member_path(path: &[u8]) -> Result<(), String> {
    let escaped = escape_member_name(path);
    if path.is_empty() || path.contains(&0) || path.starts_with(b"/") {
        return Err(format!("{escaped:?} is not a normalized relative path"));
    }
    let trimmed = trim_trailing_slashes(path);
    if trimmed.is_empty() {
        return Err(format!("{escaped:?} is not a normalized relative path"));
    }
    for part in trimmed.split(|byte| *byte == b'/') {
        if part.is_empty() || part == b"." || part == b".." {
            return Err(format!("{escaped:?} is not a normalized relative path"));
        }
    }
    Ok(())
}

fn trim_trailing_slashes(path: &[u8]) -> &[u8] {
    let end = path
        .iter()
        .rposition(|byte| *byte != b'/')
        .map(|index| index + 1)
        .unwrap_or(0);
    &path[..end]
}

fn detect_tar_engine() -> Result<TarEngineReport, String> {
    let program = "bsdtar";
    if !command_available(program) {
        return Err("bsdtar/libarchive is required for pinned .remwrap.tar fidelity".to_string());
    }
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
            "--".to_string(),
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
    let mut pending_pax = BTreeMap::<String, Vec<u8>>::new();
    loop {
        let mut header = [0u8; 512];
        file.read_exact(&mut header)
            .map_err(|error| format!("read wrapper header {}: {error}", tar_path.display()))?;
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let size = parse_tar_size(&header)?;
        let typeflag = header[156];
        let data_offset = offset
            .checked_add(512)
            .ok_or_else(|| "tar data offset overflows".to_string())?;
        if typeflag == b'x' {
            let mut data = vec![0u8; usize::try_from(size).map_err(|_| "pax header too large")?];
            file.read_exact(&mut data)
                .map_err(|error| format!("read pax data {}: {error}", tar_path.display()))?;
            pending_pax = parse_pax_records(&data)?;
            skip_tar_padding(&mut file, size)?;
            offset = next_tar_header_offset(data_offset, size)?;
            continue;
        }
        let name_bytes = pending_pax
            .get("path")
            .cloned()
            .unwrap_or_else(|| tar_header_path_bytes(&header));
        let mtime = tar_entry_mtime(&header, &pending_pax)?;
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
            path: escape_member_name(&name_bytes),
            kind: kind.to_string(),
            offset: data_offset,
            length: if kind == "regular" { size } else { 0 },
            sha256,
            mtime,
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

fn parse_pax_records(data: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut records = BTreeMap::new();
    let mut cursor = 0usize;
    while cursor < data.len() {
        let space = data[cursor..]
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| "pax record is missing length separator".to_string())?;
        let len_text = std::str::from_utf8(&data[cursor..cursor + space])
            .map_err(|error| format!("pax record length is not UTF-8: {error}"))?;
        let len = len_text
            .parse::<usize>()
            .map_err(|error| format!("parse pax record length {len_text:?}: {error}"))?;
        if len == 0
            || len <= space + 1
            || cursor + len > data.len()
            || data[cursor + len - 1] != b'\n'
        {
            return Err("pax record length is invalid".to_string());
        }
        let record = &data[cursor + space + 1..cursor + len - 1];
        if let Some(eq) = record.iter().position(|byte| *byte == b'=') {
            let key = std::str::from_utf8(&record[..eq])
                .map_err(|error| format!("pax record key is not UTF-8: {error}"))?
                .to_string();
            let value = record[eq + 1..].to_vec();
            records.insert(key, value);
        }
        cursor += len;
    }
    Ok(records)
}

fn tar_header_path_bytes(header: &[u8; 512]) -> Vec<u8> {
    let name = nul_trim_bytes(&header[0..100]);
    let prefix = nul_trim_bytes(&header[345..500]);
    if prefix.is_empty() {
        name
    } else {
        let mut out = prefix;
        out.push(b'/');
        out.extend(name);
        out
    }
}

fn tar_entry_mtime(
    header: &[u8; 512],
    pax_records: &BTreeMap<String, Vec<u8>>,
) -> Result<Option<String>, String> {
    if let Some(value) = pax_records.get("mtime") {
        let value = std::str::from_utf8(value)
            .map_err(|error| format!("pax mtime is not UTF-8: {error}"))?;
        return unix_nanos_rfc3339(parse_pax_mtime_nanos(value)?).map(Some);
    }
    let Some(seconds) = parse_tar_octal_field(&header[136..148], "tar mtime")? else {
        return Ok(None);
    };
    let nanos = i128::from(seconds)
        .checked_mul(1_000_000_000)
        .ok_or_else(|| "tar mtime overflows nanoseconds".to_string())?;
    unix_nanos_rfc3339(nanos).map(Some)
}

fn parse_tar_octal_field(field: &[u8], label: &str) -> Result<Option<u64>, String> {
    if field.first().is_some_and(|byte| byte & 0x80 != 0) {
        return Ok(None);
    }
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    let text = String::from_utf8_lossy(&field[..end]);
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    u64::from_str_radix(text, 8)
        .map(Some)
        .map_err(|error| format!("parse {label} {text:?}: {error}"))
}

fn parse_pax_mtime_nanos(value: &str) -> Result<i128, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("pax mtime is empty".to_string());
    }
    let (negative, magnitude) = value
        .strip_prefix('-')
        .map(|rest| (true, rest))
        .unwrap_or((false, value));
    let (whole, fraction) = magnitude.split_once('.').unwrap_or((magnitude, ""));
    if whole.is_empty() && fraction.is_empty() {
        return Err(format!("pax mtime {value:?} is invalid"));
    }
    if !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("pax mtime {value:?} is invalid"));
    }
    let whole_seconds = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<i128>()
            .map_err(|error| format!("parse pax mtime seconds {whole:?}: {error}"))?
    };
    let whole_nanos = whole_seconds
        .checked_mul(1_000_000_000)
        .ok_or_else(|| format!("pax mtime {value:?} overflows nanoseconds"))?;
    let mut fraction_nanos = 0i128;
    let mut scale = 100_000_000i128;
    for digit in fraction.bytes().take(9) {
        fraction_nanos += i128::from(digit - b'0') * scale;
        scale /= 10;
    }
    let total = whole_nanos
        .checked_add(fraction_nanos)
        .ok_or_else(|| format!("pax mtime {value:?} overflows nanoseconds"))?;
    Ok(if negative { -total } else { total })
}

fn metadata_mtime(path: &Path, metadata: &fs::Metadata) -> Result<Option<String>, String> {
    let modified = metadata
        .modified()
        .map_err(|error| format!("read mtime for {}: {error}", path.display()))?;
    system_time_rfc3339(modified).map(Some)
}

fn system_time_rfc3339(value: SystemTime) -> Result<String, String> {
    let nanos = match value.duration_since(UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos())
            .map_err(|_| "mtime after epoch overflows nanoseconds".to_string())?,
        Err(error) => -i128::try_from(error.duration().as_nanos())
            .map_err(|_| "mtime before epoch overflows nanoseconds".to_string())?,
    };
    unix_nanos_rfc3339(nanos)
}

fn unix_nanos_rfc3339(nanos: i128) -> Result<String, String> {
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .map_err(|error| format!("format mtime: {error}"))?
        .format(&Rfc3339)
        .map_err(|error| format!("format mtime: {error}"))
}

fn nul_trim_bytes(bytes: &[u8]) -> Vec<u8> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    bytes[..end].to_vec()
}

pub(crate) fn escape_member_name(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match std::str::from_utf8(&bytes[cursor..]) {
            Ok(valid) => {
                escape_valid_member_name_chunk(valid, &mut out);
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    let valid = std::str::from_utf8(&bytes[cursor..cursor + valid_up_to])
                        .expect("valid_up_to returned valid UTF-8 prefix");
                    escape_valid_member_name_chunk(valid, &mut out);
                    cursor += valid_up_to;
                }
                if let Some(error_len) = error.error_len() {
                    for byte in &bytes[cursor..cursor + error_len] {
                        push_hex_escape(&mut out, *byte);
                    }
                    cursor += error_len;
                } else {
                    for byte in &bytes[cursor..] {
                        push_hex_escape(&mut out, *byte);
                    }
                    break;
                }
            }
        }
    }
    out
}

fn escape_valid_member_name_chunk(valid: &str, out: &mut String) {
    for ch in valid.chars() {
        if ch == '\\' {
            out.push_str("\\\\");
        } else if matches!(ch, '\u{0}'..='\u{1f}' | '\u{7f}') {
            for byte in ch.to_string().as_bytes() {
                push_hex_escape(out, *byte);
            }
        } else {
            out.push(ch);
        }
    }
}

fn push_hex_escape(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('\\');
    out.push('x');
    out.push(char::from(HEX[(byte >> 4) as usize]));
    out.push(char::from(HEX[(byte & 0x0f) as usize]));
}

pub(crate) fn decode_member_name(value: &str) -> Result<Vec<u8>, String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        if bytes[cursor] != b'\\' {
            out.push(bytes[cursor]);
            cursor += 1;
            continue;
        }
        let Some(next) = bytes.get(cursor + 1).copied() else {
            return Err("trailing backslash escape".to_string());
        };
        match next {
            b'\\' => {
                out.push(b'\\');
                cursor += 2;
            }
            b'x' => {
                let hi = bytes
                    .get(cursor + 2)
                    .copied()
                    .ok_or_else(|| "short \\xHH escape".to_string())?;
                let lo = bytes
                    .get(cursor + 3)
                    .copied()
                    .ok_or_else(|| "short \\xHH escape".to_string())?;
                let hi =
                    hex_value(hi).ok_or_else(|| "invalid hex digit in \\xHH escape".to_string())?;
                let lo =
                    hex_value(lo).ok_or_else(|| "invalid hex digit in \\xHH escape".to_string())?;
                out.push((hi << 4) | lo);
                cursor += 4;
            }
            other => {
                return Err(format!(
                    "unsupported backslash escape \\{}",
                    char::from(other)
                ))
            }
        }
    }
    Ok(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
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
    let skip = round_up_512(size)?;
    file.seek(SeekFrom::Current(
        i64::try_from(skip).map_err(|_| "tar payload too large to seek")?,
    ))
    .map_err(|error| format!("seek wrapper payload: {error}"))?;
    Ok(())
}

fn skip_tar_padding(file: &mut File, size: u64) -> Result<(), String> {
    let padding = round_up_512(size)?
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
        .checked_add(round_up_512(size)?)
        .ok_or_else(|| "tar offset overflows".to_string())
}

fn round_up_512(value: u64) -> Result<u64, String> {
    value
        .checked_add(511)
        .map(|value| value / 512 * 512)
        .ok_or_else(|| "tar size overflows while rounding to 512 bytes".to_string())
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
        self.totals.native_entries = self.totals.native_entries.saturating_add(1);
        self.record_dir_stats(rel_text, false);
        self.record_cluster(rel_text, "native", 1, bytes);
    }

    fn record_wrapped(&mut self, rel_text: &str, reason: &str, bytes: u64) {
        self.totals.wrapped_entries = self.totals.wrapped_entries.saturating_add(1);
        self.record_dir_stats(rel_text, true);
        self.record_cluster(rel_text, reason, 1, bytes);
    }

    fn record_blob(&mut self, rel_text: &str, bytes: u64, reason: &'static str) {
        self.totals.blob_entries = self.totals.blob_entries.saturating_add(1);
        self.record_dir_stats(rel_text, false);
        self.record_cluster(rel_text, reason, 1, bytes);
    }

    fn record_dir_stats(&mut self, rel_text: &str, noncompliant: bool) {
        let mut current = String::new();
        self.bump_dir_stat(".", noncompliant);
        let mut components = rel_text
            .split('/')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        components.pop();
        for component in components {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(component);
            self.bump_dir_stat(&current, noncompliant);
        }
    }

    fn bump_dir_stat(&mut self, prefix: &str, noncompliant: bool) {
        let stats = self.dir_stats.entry(prefix.to_string()).or_default();
        stats.total = stats.total.saturating_add(1);
        if noncompliant {
            stats.noncompliant = stats.noncompliant.saturating_add(1);
        }
    }

    fn record_cluster(&mut self, rel_text: &str, reason: &str, count: u64, bytes: u64) {
        let prefix = directory_prefix(rel_text);
        let cluster = self
            .clusters
            .entry((prefix, reason.to_string()))
            .or_default();
        cluster.count = cluster.count.saturating_add(count);
        cluster.bytes = cluster.bytes.saturating_add(bytes);
        if cluster.samples.len() < DEFAULT_SAMPLES_PER_CLUSTER {
            cluster.samples.push(rel_text.to_string());
        }
    }

    fn record_xattr_drop(&mut self, rel_text: &str, name: &str, reason: &str) {
        self.totals.dropped_xattrs = self.totals.dropped_xattrs.saturating_add(1);
        let prefix = directory_prefix(rel_text);
        let cluster = self
            .xattr_drops
            .entry((prefix, name.to_string(), reason.to_string()))
            .or_default();
        cluster.count = cluster.count.saturating_add(1);
        if cluster.samples.len() < DEFAULT_SAMPLES_PER_CLUSTER {
            cluster.samples.push(rel_text.to_string());
        }
    }

    fn note_native_hardlink(
        &mut self,
        _path: &Path,
        metadata: &fs::Metadata,
        rel_text: &str,
        archive_path: &str,
    ) -> Option<String> {
        let key = file_key(metadata)?;
        self.hardlink_paths
            .entry(key)
            .or_default()
            .push(rel_text.to_string());
        self.hardlink_link_counts
            .entry(key)
            .or_insert_with(|| hardlink_count(metadata));
        *self.hardlink_native_counts.entry(key).or_default() += 1;
        if let Some(primary) = self.hardlink_primaries.get(&key) {
            Some(primary.clone())
        } else {
            self.hardlink_primaries
                .insert(key, archive_path.to_string());
            None
        }
    }

    fn record_hardlink_splits(&mut self) {
        let split_keys = self
            .hardlink_native_counts
            .iter()
            .filter_map(|(key, native_count)| {
                let link_count = self.hardlink_link_counts.get(key).copied().unwrap_or(0);
                (*native_count > 0 && link_count > *native_count).then_some(*key)
            })
            .collect::<Vec<_>>();
        for key in split_keys {
            let sample = self
                .hardlink_paths
                .get(&key)
                .and_then(|paths| paths.first())
                .cloned()
                .unwrap_or_else(|| ".".to_string());
            self.record_cluster(&sample, "hardlink-split", 1, 0);
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
        let mut dense = self
            .dir_stats
            .iter()
            .filter_map(|(prefix, stats)| {
                if stats.total == 0 || stats.noncompliant == 0 {
                    return None;
                }
                let ratio = stats.noncompliant as f64 / stats.total as f64;
                if ratio >= tuning.blob_ratio && stats.noncompliant >= tuning.blob_count {
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
        dense.sort_by(|a, b| {
            a.prefix
                .matches('/')
                .count()
                .cmp(&b.prefix.matches('/').count())
                .then_with(|| a.prefix.cmp(&b.prefix))
        });
        let mut suggestions = Vec::new();
        for candidate in dense {
            if suggestions.iter().any(|accepted: &BlobSuggestion| {
                prefix_contains(&accepted.prefix, &candidate.prefix)
            }) {
                continue;
            }
            if self.has_substantial_compliant_descendant(&candidate.prefix, tuning) {
                continue;
            }
            suggestions.push(candidate);
        }

        let covered_noncompliant = suggestions
            .iter()
            .filter(|suggestion| suggestion.verdict == "blob-suggest")
            .fold(0u64, |acc, suggestion| {
                acc.saturating_add(suggestion.noncompliant)
            });
        let total_noncompliant = self
            .dir_stats
            .get(".")
            .map(|stats| stats.noncompliant)
            .unwrap_or_default();
        let residual_noncompliant = total_noncompliant.saturating_sub(covered_noncompliant);
        for cluster in &clusters {
            if !is_noncompliant_reason(&cluster.reason)
                || suggestions
                    .iter()
                    .any(|suggestion| prefix_contains(&suggestion.prefix, &cluster.prefix))
            {
                continue;
            }
            let Some(stats) = self.dir_stats.get(&cluster.prefix) else {
                continue;
            };
            if stats.noncompliant == 0 {
                continue;
            }
            let total = stats.total;
            suggestions.push(BlobSuggestion {
                prefix: cluster.prefix.clone(),
                noncompliant: cluster.count,
                total,
                ratio: if total == 0 {
                    0.0
                } else {
                    cluster.count as f64 / total as f64
                },
                verdict: "straggler",
            });
        }
        if residual_noncompliant >= tuning.sanity_ceiling {
            let total = self
                .dir_stats
                .get(".")
                .map(|stats| stats.total)
                .unwrap_or_default();
            suggestions.push(BlobSuggestion {
                prefix: ".".to_string(),
                noncompliant: residual_noncompliant,
                total,
                ratio: if total == 0 {
                    0.0
                } else {
                    residual_noncompliant as f64 / total as f64
                },
                verdict: "sanity-ceiling",
            });
        }
        ScanReport {
            totals: self.totals.clone(),
            clusters,
            xattr_drops: self
                .xattr_drops
                .iter()
                .map(|((prefix, name, reason), cluster)| XattrDropCluster {
                    prefix: prefix.clone(),
                    name: name.clone(),
                    reason: reason.clone(),
                    count: cluster.count,
                    samples: cluster.samples.clone(),
                })
                .collect(),
            blob_suggestions: suggestions,
        }
    }

    fn has_substantial_compliant_descendant(&self, prefix: &str, tuning: ScanTuning) -> bool {
        let threshold = substantial_compliant_threshold(tuning);
        self.dir_stats.iter().any(|(child, stats)| {
            child != prefix
                && prefix_contains(prefix, child)
                && stats.total.saturating_sub(stats.noncompliant) >= threshold
                && (stats.noncompliant as f64 / stats.total.max(1) as f64) < tuning.blob_ratio
        })
    }
}

fn substantial_compliant_threshold(tuning: ScanTuning) -> u64 {
    (tuning.blob_count / 2).max(1)
}

fn prefix_contains(parent: &str, child: &str) -> bool {
    parent.is_empty()
        || parent == "."
        || parent == child
        || child
            .strip_prefix(parent)
            .is_some_and(|remainder| remainder.starts_with('/'))
}

fn is_noncompliant_reason(reason: &str) -> bool {
    !matches!(reason, "native" | "exclude-rule" | "blob-rule")
}

fn subtree_count_bytes(path: &Path) -> Result<(u64, u64), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    if !metadata.is_dir() {
        return Ok((1, metadata.len()));
    }
    let mut count = 1u64;
    let mut bytes = 0u64;
    for entry in fs::read_dir(path).map_err(|error| format!("read {}: {error}", path.display()))? {
        let entry = entry.map_err(|error| format!("read {}: {error}", path.display()))?;
        let (child_count, child_bytes) = subtree_count_bytes(&entry.path())?;
        count = count.saturating_add(child_count);
        bytes = bytes.saturating_add(child_bytes);
    }
    Ok((count, bytes))
}

pub(crate) fn ensure_unique_archive_paths(files: &[ArchiveBuildInputFile]) -> Result<(), String> {
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

pub(crate) fn remwrap_index_path(wrapper_path: &str) -> Result<String, String> {
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
exclude Literal/
blob Literal/Sub/
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
        assert!(ruleset
            .lints
            .iter()
            .any(|lint| lint.kind == "literal-subpath-unreachable"));
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

    #[test]
    fn malformed_pax_record_is_error_not_panic() {
        assert!(parse_pax_records(b"01 x\n").is_err());
        assert!(parse_pax_records(b"5 x=1").is_err());
    }

    #[test]
    fn member_name_escape_is_readable_and_reversible() {
        let cases: &[(&[u8], &str)] = &[
            (b"report.pdf", "report.pdf"),
            ("résumé.doc".as_bytes(), "résumé.doc"),
            (b"r\xe9sum\xe9.doc", "r\\xe9sum\\xe9.doc"),
            (b"literal\\slash\t.txt", "literal\\\\slash\\x09.txt"),
            (b"del-\x7f.txt", "del-\\x7f.txt"),
        ];
        for (raw, escaped) in cases {
            assert_eq!(escape_member_name(raw), *escaped);
            assert_eq!(decode_member_name(escaped).unwrap(), *raw);
        }
    }

    #[test]
    fn member_name_escape_keeps_invalid_utf8_collisions_distinct() {
        let left = escape_member_name(b"r\xe9sume.doc");
        let right = escape_member_name(b"r\xeesume.doc");
        assert_eq!(left, "r\\xe9sume.doc");
        assert_eq!(right, "r\\xeesume.doc");
        assert_ne!(left, right);
        assert_eq!(decode_member_name(&left).unwrap(), b"r\xe9sume.doc");
        assert_eq!(decode_member_name(&right).unwrap(), b"r\xeesume.doc");
    }

    #[test]
    fn scan_suggests_dense_subtree_without_swallowing_clean_sibling() {
        let mut state = PlannerState::default();
        for index in 0..1_000 {
            state.record_wrapped(&format!("Users/bob/AppData/bad-{index}.bin"), "xattr", 1);
        }
        for index in 0..50 {
            state.record_native(&format!("Users/bob/Documents/good-{index}.mov"), 1);
        }
        let report = state.scan_report(ScanTuning {
            blob_ratio: 0.9,
            blob_count: 100,
            sanity_ceiling: 10_000,
        });
        let blob_prefixes = report
            .blob_suggestions
            .iter()
            .filter(|suggestion| suggestion.verdict == "blob-suggest")
            .map(|suggestion| suggestion.prefix.as_str())
            .collect::<Vec<_>>();
        assert_eq!(blob_prefixes, vec!["Users/bob/AppData"]);
    }

    #[test]
    fn scan_reports_sparse_noncompliance_as_straggler() {
        let mut state = PlannerState::default();
        state.record_wrapped("Media/bad-name.mov", "non-utf8-path", 1);
        for index in 0..100 {
            state.record_native(&format!("Media/good-{index}.mov"), 1);
        }
        let report = state.scan_report(ScanTuning {
            blob_ratio: 0.9,
            blob_count: 10,
            sanity_ceiling: 1_000,
        });
        assert!(report.blob_suggestions.iter().any(|suggestion| {
            suggestion.verdict == "straggler" && suggestion.prefix == "Media"
        }));
        assert!(!report
            .blob_suggestions
            .iter()
            .any(|suggestion| suggestion.verdict == "blob-suggest"));
    }

    #[test]
    fn scan_reports_sanity_ceiling_from_unabsorbed_residual() {
        let mut state = PlannerState::default();
        for index in 0..20 {
            state.record_wrapped(&format!("Spread/dir-{index}/bad.bin"), "xattr", 1);
            state.record_native(&format!("Spread/dir-{index}/good.bin"), 1);
        }
        let report = state.scan_report(ScanTuning {
            blob_ratio: 0.9,
            blob_count: 5,
            sanity_ceiling: 10,
        });
        assert!(report.blob_suggestions.iter().any(|suggestion| {
            suggestion.verdict == "sanity-ceiling" && suggestion.prefix == "."
        }));
    }

    #[test]
    fn partial_native_hardlink_coverage_records_split() {
        let mut state = PlannerState::default();
        let key = FileKey { dev: 1, ino: 2 };
        state.hardlink_paths.insert(key, vec!["a.bin".to_string()]);
        state.hardlink_native_counts.insert(key, 1);
        state.hardlink_link_counts.insert(key, 2);

        state.record_hardlink_splits();
        let report = state.scan_report(ScanTuning::default());

        assert!(report
            .clusters
            .iter()
            .any(|cluster| cluster.reason == "hardlink-split"));
    }

    #[cfg(unix)]
    #[test]
    fn wrapper_tar_treats_dash_prefixed_members_as_operands() {
        assert!(
            command_available("bsdtar"),
            "bsdtar/libarchive is required for wrapper fidelity tests"
        );
        let temp = tempfile::Builder::new()
            .prefix("remanence-remwrap-dash-member")
            .tempdir()
            .unwrap();
        let base = temp.path().join("base");
        fs::create_dir_all(base.join("--dash-dir")).unwrap();
        fs::write(base.join("--dash-dir/member.txt"), b"dir").unwrap();
        fs::write(base.join("--dash-file.txt"), b"file").unwrap();
        let tar_engine = detect_tar_engine().unwrap();

        let dir_tar = temp.path().join("dir.remwrap.tar");
        create_wrapper_tar(&tar_engine, &base, OsStr::new("--dash-dir"), &dir_tar).unwrap();
        let dir_index = build_wrap_index(&dir_tar, &tar_engine).unwrap();
        assert!(dir_index
            .entries
            .iter()
            .any(|entry| entry.path == "--dash-dir/member.txt"));

        let file_tar = temp.path().join("file.remwrap.tar");
        create_wrapper_tar(&tar_engine, &base, OsStr::new("--dash-file.txt"), &file_tar).unwrap();
        let file_index = build_wrap_index(&file_tar, &tar_engine).unwrap();
        assert!(file_index
            .entries
            .iter()
            .any(|entry| entry.path == "--dash-file.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn wrapper_index_escapes_non_utf8_member_names() {
        assert!(
            command_available("bsdtar"),
            "bsdtar/libarchive is required for wrapper fidelity tests"
        );
        let temp = tempfile::Builder::new()
            .prefix("remanence-remwrap-raw-name")
            .tempdir()
            .unwrap();
        let base = temp.path().join("base");
        fs::create_dir_all(&base).unwrap();
        fs::write(base.join(OsStr::from_bytes(b"r\xe9sume.doc")), b"latin1").unwrap();

        let tar_engine = detect_tar_engine().unwrap();
        let tar_path = temp.path().join("raw.remwrap.tar");
        create_wrapper_tar(&tar_engine, temp.path(), OsStr::new("base"), &tar_path).unwrap();
        let index = build_wrap_index(&tar_path, &tar_engine).unwrap();
        let entry = index
            .entries
            .iter()
            .find(|entry| entry.path == "base/r\\xe9sume.doc")
            .expect("escaped raw-name index entry");
        assert_eq!(
            decode_member_name(&entry.path).unwrap(),
            b"base/r\xe9sume.doc"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bsdtar_wrapper_round_trips_required_fidelity_cases() {
        assert!(
            command_available("bsdtar"),
            "bsdtar/libarchive is required for wrapper fidelity tests"
        );
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
        if xattr::set(&xattr_file, "user.remanence_test", b"kept").is_err() {
            return;
        }

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
        assert_eq!(
            xattr::get(restored_source.join("xattr.txt"), "user.remanence_test").unwrap(),
            Some(b"kept".to_vec())
        );
    }
}
