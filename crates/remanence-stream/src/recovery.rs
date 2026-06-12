//! Generic sparse recovery sink for normalized archive readers.
//!
//! Format drivers emit file-offset data, file-scoped damage ranges, and
//! archive-scoped gaps. This module turns those events into sparse filesystem
//! output plus a JSONL recovery manifest that later stitching tools can merge
//! across multiple recovery passes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use remanence_format::{
    ArchiveEventSink, ArchiveGapRange, ArchiveReader, DamageRange, DamageStatus, EntryKind, FileId,
    FormatError, NormalizedEntry, StreamReport,
};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    apply_restore_open_flags, create_restore_dirs_secure, create_restore_parent_dirs_secure,
    ensure_restore_root, format_sink_io, io_error, normalize_archive_path, StreamingError,
};

/// Relative path of the recovery manifest inside a destination directory.
pub const RECOVERY_MANIFEST_RELATIVE_PATH: &str = ".remanence/recovery.jsonl";

// Preserve small sparse holes, but do not let an untrusted archive header
// eagerly create a giant logical file before any payload bytes arrive.
const MAX_EAGER_RECOVERY_SET_LEN_BYTES: u64 = 64 * 1024 * 1024;

/// User-facing options for one archive recovery pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryOptions {
    /// Format identifier recorded in the manifest.
    pub format_id: String,
    /// Human-readable source description, such as `dump:/path/archive.bru`.
    pub source: String,
}

impl RecoveryOptions {
    /// Create options for one archive recovery pass.
    pub fn new(format_id: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            format_id: format_id.into(),
            source: source.into(),
        }
    }
}

/// Summary returned after recovering a normalized archive reader to disk.
#[derive(Debug, Clone)]
pub struct ArchiveRecoveryReport {
    /// Format-driver streaming summary.
    pub stream: StreamReport,
    /// Recovery manifest path.
    pub manifest_path: PathBuf,
    /// Per-file recovery records written during this run.
    pub files: Vec<RecoveryFileRecord>,
    /// Archive gaps reported during this run.
    pub archive_gaps: Vec<RecoveryArchiveGapRecord>,
    /// Number of regular files opened for recovery.
    pub files_seen: u64,
    /// Number of regular files that received at least one byte this run.
    pub files_written: u64,
    /// Number of file payload bytes written this run.
    pub bytes_written: u64,
}

/// One line in `.remanence/recovery.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryManifestRecord {
    /// Start of one recovery run.
    RunStart(RecoveryRunStartRecord),
    /// Source gap not attributable to one file.
    ArchiveGap(RecoveryArchiveGapRecord),
    /// Per-entry recovery result.
    File(RecoveryFileRecord),
    /// End of one recovery run.
    RunEnd(RecoveryRunEndRecord),
}

/// Start marker for one recovery run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRunStartRecord {
    /// Unique id for this recovery run.
    pub run_id: String,
    /// Format identifier used by the reader.
    pub format_id: String,
    /// Human-readable source description.
    pub source: String,
    /// UTC RFC 3339 timestamp.
    pub started_at: String,
}

/// End marker for one recovery run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryRunEndRecord {
    /// Unique id for this recovery run.
    pub run_id: String,
    /// UTC RFC 3339 timestamp.
    pub finished_at: String,
    /// Number of entries streamed by the format driver.
    pub entries: u64,
    /// Number of file payload bytes delivered by the format driver.
    pub bytes: u64,
    /// Number of file-scoped damage events.
    pub damage_events: u64,
    /// Number of archive-scoped gaps.
    pub archive_gaps: u64,
    /// Number of regular files opened for recovery.
    pub files_seen: u64,
    /// Number of regular files that received at least one byte this run.
    pub files_written: u64,
    /// Number of file payload bytes written this run.
    pub bytes_written: u64,
}

/// Half-open file byte range, `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryByteRange {
    /// Inclusive byte offset.
    pub start: u64,
    /// Exclusive byte offset.
    pub end: u64,
}

impl RecoveryByteRange {
    fn new(start: u64, end: u64) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }
}

/// Derived recovery status for one archive entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStatus {
    /// Declared file range is covered by clean and/or suspect bytes.
    Complete,
    /// Some bytes were written but declared coverage is incomplete or unknown.
    Partial,
    /// No bytes were recovered for the regular file.
    Missing,
    /// The entry type was not recovered as file bytes.
    Skipped,
}

/// File-scoped damage range recorded in the recovery manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryDamageRecord {
    /// Start byte offset within the file.
    pub start: u64,
    /// Exclusive end byte offset within the file.
    pub end: u64,
    /// Lowercase damage status name.
    pub status: String,
    /// Driver-owned source/provenance bytes encoded as lowercase hex.
    pub adapter_state_hex: String,
}

impl RecoveryDamageRecord {
    fn from_damage(range: &DamageRange) -> Self {
        Self {
            start: range.start,
            end: range.end,
            status: damage_status_name(range.status).to_string(),
            adapter_state_hex: hex_bytes(&range.adapter_state),
        }
    }
}

/// Archive-scoped gap recorded in the recovery manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryArchiveGapRecord {
    /// Unique id for this recovery run.
    pub run_id: String,
    /// Adapter-defined inclusive source start.
    pub source_start: u64,
    /// Adapter-defined exclusive source end.
    pub source_end: u64,
    /// Lowercase gap cause name.
    pub cause: String,
    /// Driver-owned source/provenance bytes encoded as lowercase hex.
    pub adapter_state_hex: String,
}

impl RecoveryArchiveGapRecord {
    fn from_gap(run_id: &str, range: &ArchiveGapRange) -> Self {
        Self {
            run_id: run_id.to_string(),
            source_start: range.source_start,
            source_end: range.source_end,
            cause: archive_gap_cause_name(range.cause).to_string(),
            adapter_state_hex: hex_bytes(&range.adapter_state),
        }
    }
}

/// Per-entry recovery record written to the JSONL manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryFileRecord {
    /// Unique id for this recovery run.
    pub run_id: String,
    /// Stable format-driver file id.
    pub file_id: String,
    /// Original archive path.
    pub path: String,
    /// Path actually written under the destination directory.
    pub output_path: String,
    /// Lowercase archive entry kind name.
    pub entry_kind: String,
    /// Declared file size, when the format provides it.
    pub declared_size: Option<u64>,
    /// Clean byte ranges written during this run.
    pub recovered_ranges: Vec<RecoveryByteRange>,
    /// Checksum-failed byte ranges written during this run.
    pub suspect_ranges: Vec<RecoveryByteRange>,
    /// File-scoped damage reported by the driver.
    pub damage_ranges: Vec<RecoveryDamageRecord>,
    /// Derived status for this entry.
    pub status: RecoveryStatus,
    /// File payload bytes written during this run.
    pub bytes_written: u64,
    /// Driver-owned entry metadata encoded as lowercase hex.
    pub adapter_state_hex: String,
}

/// Recover any normalized archive reader into sparse files and a JSONL manifest.
pub fn recover_archive_reader_to_directory(
    reader: &mut dyn ArchiveReader,
    destination_root: impl AsRef<Path>,
    options: RecoveryOptions,
) -> Result<ArchiveRecoveryReport, StreamingError> {
    let mut sink = RecoveryFilesystemSink::new(destination_root.as_ref(), options)?;
    let stream = reader.stream_all(&mut sink)?;
    sink.finish(stream)
}

struct RecoveryFilesystemSink {
    root: PathBuf,
    manifest_path: PathBuf,
    manifest: BufWriter<File>,
    run_id: String,
    current: RecoveryTarget,
    path_collision_counts: BTreeMap<String, u64>,
    used_output_paths: BTreeSet<String>,
    files: Vec<RecoveryFileRecord>,
    archive_gaps: Vec<RecoveryArchiveGapRecord>,
    files_seen: u64,
    files_written: u64,
    bytes_written: u64,
}

impl RecoveryFilesystemSink {
    fn new(root: &Path, options: RecoveryOptions) -> Result<Self, StreamingError> {
        ensure_restore_root(root)?;
        let manifest_dir =
            create_restore_dirs_secure(root, ".remanence").map_err(StreamingError::Format)?;
        let manifest_path = manifest_dir.join("recovery.jsonl");
        let mut manifest_options = OpenOptions::new();
        manifest_options.create(true).append(true);
        apply_restore_open_flags(&mut manifest_options);
        let manifest = manifest_options
            .open(&manifest_path)
            .map_err(|err| io_error("open recovery manifest", &manifest_path, err))?;
        let mut sink = Self {
            root: root.to_path_buf(),
            manifest_path,
            manifest: BufWriter::new(manifest),
            run_id: Uuid::new_v4().to_string(),
            current: RecoveryTarget::None,
            path_collision_counts: BTreeMap::new(),
            used_output_paths: BTreeSet::new(),
            files: Vec::new(),
            archive_gaps: Vec::new(),
            files_seen: 0,
            files_written: 0,
            bytes_written: 0,
        };
        let record = RecoveryManifestRecord::RunStart(RecoveryRunStartRecord {
            run_id: sink.run_id.clone(),
            format_id: options.format_id,
            source: options.source,
            started_at: timestamp_now(),
        });
        sink.write_manifest_record(&record)?;
        Ok(sink)
    }

    fn finish(mut self, stream: StreamReport) -> Result<ArchiveRecoveryReport, StreamingError> {
        if !matches!(self.current, RecoveryTarget::None) {
            return Err(StreamingError::Format(FormatError::Parse(
                "recovery stream ended with an active entry".to_string(),
            )));
        }
        let end = RecoveryManifestRecord::RunEnd(RecoveryRunEndRecord {
            run_id: self.run_id.clone(),
            finished_at: timestamp_now(),
            entries: stream.entries,
            bytes: stream.bytes,
            damage_events: stream.damage_events,
            archive_gaps: stream.archive_gaps,
            files_seen: self.files_seen,
            files_written: self.files_written,
            bytes_written: self.bytes_written,
        });
        self.write_manifest_record(&end)?;
        self.manifest
            .flush()
            .map_err(|err| io_error("flush recovery manifest", &self.manifest_path, err))?;
        Ok(ArchiveRecoveryReport {
            stream,
            manifest_path: self.manifest_path,
            files: self.files,
            archive_gaps: self.archive_gaps,
            files_seen: self.files_seen,
            files_written: self.files_written,
            bytes_written: self.bytes_written,
        })
    }

    fn write_manifest_record(
        &mut self,
        record: &RecoveryManifestRecord,
    ) -> Result<(), FormatError> {
        serde_json::to_writer(&mut self.manifest, record).map_err(|err| {
            FormatError::Parse(format!("serialize recovery manifest record: {err}"))
        })?;
        self.manifest
            .write_all(b"\n")
            .map_err(|err| format_sink_io("write recovery manifest", &self.manifest_path, err))?;
        self.manifest
            .flush()
            .map_err(|err| format_sink_io("flush recovery manifest", &self.manifest_path, err))?;
        Ok(())
    }

    fn assign_output_path(&mut self, relative_path: &str, file_id: &FileId) -> String {
        let count = self
            .path_collision_counts
            .entry(relative_path.to_string())
            .or_insert(0);
        loop {
            let candidate = if *count == 0 {
                relative_path.to_string()
            } else {
                format!(
                    "{relative_path}._{}_{}",
                    sanitize_for_suffix(file_id.as_str()),
                    count
                )
            };
            *count += 1;
            if self.used_output_paths.insert(candidate.clone()) {
                return candidate;
            }
        }
    }
}

impl ArchiveEventSink for RecoveryFilesystemSink {
    fn begin_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
        if !matches!(self.current, RecoveryTarget::None) {
            return Err(FormatError::Parse(
                "recovery entry began while another entry is active".to_string(),
            ));
        }

        let relative = recovery_relative_path(entry);
        match entry.kind {
            EntryKind::Directory => {
                create_restore_dirs_secure(&self.root, relative.as_str())?;
                self.current = RecoveryTarget::Directory;
            }
            EntryKind::RegularFile => {
                let output_path = self.assign_output_path(&relative, &entry.file_id);
                let destination =
                    create_restore_parent_dirs_secure(&self.root, output_path.as_str())?;
                let mut options = OpenOptions::new();
                options.create(true).truncate(false).write(true);
                apply_restore_open_flags(&mut options);
                let file = options
                    .open(&destination)
                    .map_err(|err| format_sink_io("open recovery file", &destination, err))?;
                if let Some(size) = entry.size_bytes {
                    if size <= MAX_EAGER_RECOVERY_SET_LEN_BYTES {
                        file.set_len(size).map_err(|err| {
                            format_sink_io("pre-size recovery file", &destination, err)
                        })?;
                    }
                }
                self.files_seen += 1;
                self.current = RecoveryTarget::File {
                    entry: entry.clone(),
                    output_path,
                    destination,
                    file,
                    bytes_written: 0,
                    recovered_ranges: Vec::new(),
                    suspect_ranges: Vec::new(),
                    checksum_failed_ranges: Vec::new(),
                    damage_ranges: Vec::new(),
                };
            }
            EntryKind::Symlink | EntryKind::Hardlink | EntryKind::Special => {
                let output_path = self.assign_output_path(&relative, &entry.file_id);
                self.current = RecoveryTarget::Skipped {
                    record: recovery_record(
                        &self.run_id,
                        entry,
                        RecoveryRecordParts {
                            output_path,
                            recovered_ranges: Vec::new(),
                            suspect_ranges: Vec::new(),
                            damage_ranges: Vec::new(),
                            status: RecoveryStatus::Skipped,
                            bytes_written: 0,
                        },
                    ),
                };
            }
        }
        Ok(())
    }

    fn write_file_data(&mut self, file_offset: u64, bytes: &[u8]) -> Result<(), FormatError> {
        let RecoveryTarget::File {
            entry,
            destination,
            file,
            bytes_written,
            recovered_ranges,
            suspect_ranges,
            checksum_failed_ranges,
            ..
        } = &mut self.current
        else {
            return Err(FormatError::Parse(
                "recovery data arrived without an active regular file".to_string(),
            ));
        };
        let end_offset = file_offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| FormatError::Parse("recovery file offset overflow".to_string()))?;
        if entry
            .size_bytes
            .is_some_and(|declared_size| end_offset > declared_size)
        {
            return Err(FormatError::Parse(format!(
                "recovery data for {} extends beyond declared size",
                entry.path
            )));
        }
        file.seek(SeekFrom::Start(file_offset))
            .map_err(|err| format_sink_io("seek recovery file", destination, err))?;
        file.write_all(bytes)
            .map_err(|err| format_sink_io("write recovery file", destination, err))?;

        if let Some(written) = RecoveryByteRange::new(file_offset, end_offset) {
            let suspect = intersect_one_with_many(written, checksum_failed_ranges);
            let clean = subtract_many(written, &suspect);
            recovered_ranges.extend(clean);
            suspect_ranges.extend(suspect);
        }
        *bytes_written += bytes.len() as u64;
        self.bytes_written += bytes.len() as u64;
        Ok(())
    }

    fn report_damage(&mut self, range: &DamageRange) -> Result<(), FormatError> {
        if let RecoveryTarget::File {
            entry,
            recovered_ranges,
            suspect_ranges,
            checksum_failed_ranges,
            damage_ranges,
            ..
        } = &mut self.current
        {
            if entry.file_id == range.file_id {
                let damage_record = RecoveryDamageRecord::from_damage(range);
                damage_ranges.push(damage_record);
                if range.status == DamageStatus::ChecksumFailed {
                    if let Some(damage_range) = RecoveryByteRange::new(range.start, range.end) {
                        let newly_suspect = intersect_many_with_one(recovered_ranges, damage_range);
                        *recovered_ranges = subtract_range_vec(recovered_ranges, damage_range);
                        suspect_ranges.extend(newly_suspect);
                        checksum_failed_ranges.push(damage_range);
                    }
                }
            }
        }
        Ok(())
    }

    fn report_archive_gap(&mut self, range: &ArchiveGapRange) -> Result<(), FormatError> {
        let record = RecoveryArchiveGapRecord::from_gap(&self.run_id, range);
        self.archive_gaps.push(record.clone());
        self.write_manifest_record(&RecoveryManifestRecord::ArchiveGap(record))
    }

    fn end_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
        match std::mem::replace(&mut self.current, RecoveryTarget::None) {
            RecoveryTarget::None => Err(FormatError::Parse(
                "recovery entry ended without an active entry".to_string(),
            )),
            RecoveryTarget::Directory => Ok(()),
            RecoveryTarget::Skipped { record } => {
                self.files.push(record.clone());
                self.write_manifest_record(&RecoveryManifestRecord::File(record))
            }
            RecoveryTarget::File {
                entry: active_entry,
                output_path,
                destination,
                mut file,
                bytes_written,
                mut recovered_ranges,
                mut suspect_ranges,
                damage_ranges,
                ..
            } => {
                if active_entry.file_id != entry.file_id {
                    return Err(FormatError::Parse(format!(
                        "recovery ended {} while {} was active",
                        entry.file_id.as_str(),
                        active_entry.file_id.as_str()
                    )));
                }
                normalize_ranges(&mut recovered_ranges);
                normalize_ranges(&mut suspect_ranges);
                let status = recovery_status(
                    active_entry.size_bytes,
                    bytes_written,
                    &recovered_ranges,
                    &suspect_ranges,
                );
                file.flush()
                    .map_err(|err| format_sink_io("flush recovery file", &destination, err))?;
                if bytes_written > 0 {
                    self.files_written += 1;
                }
                let record = recovery_record(
                    &self.run_id,
                    &active_entry,
                    RecoveryRecordParts {
                        output_path,
                        recovered_ranges,
                        suspect_ranges,
                        damage_ranges,
                        status,
                        bytes_written,
                    },
                );
                self.files.push(record.clone());
                self.write_manifest_record(&RecoveryManifestRecord::File(record))
            }
        }
    }
}

fn recovery_relative_path(entry: &NormalizedEntry) -> String {
    match normalize_archive_path(Path::new(&entry.path)) {
        Ok(relative) => relative,
        Err(_) => sanitized_recovery_path(entry),
    }
}

fn sanitized_recovery_path(entry: &NormalizedEntry) -> String {
    let file_id = sanitize_for_suffix(entry.file_id.as_str());
    let path = sanitize_for_suffix(&entry.path);
    let leaf = if path == "entry" {
        file_id
    } else {
        format!("{file_id}_{path}")
    };
    format!("_remanence_recovered/{leaf}")
}

enum RecoveryTarget {
    None,
    Directory,
    Skipped {
        record: RecoveryFileRecord,
    },
    File {
        entry: NormalizedEntry,
        output_path: String,
        destination: PathBuf,
        file: File,
        bytes_written: u64,
        recovered_ranges: Vec<RecoveryByteRange>,
        suspect_ranges: Vec<RecoveryByteRange>,
        checksum_failed_ranges: Vec<RecoveryByteRange>,
        damage_ranges: Vec<RecoveryDamageRecord>,
    },
}

struct RecoveryRecordParts {
    output_path: String,
    recovered_ranges: Vec<RecoveryByteRange>,
    suspect_ranges: Vec<RecoveryByteRange>,
    damage_ranges: Vec<RecoveryDamageRecord>,
    status: RecoveryStatus,
    bytes_written: u64,
}

fn recovery_record(
    run_id: &str,
    entry: &NormalizedEntry,
    mut parts: RecoveryRecordParts,
) -> RecoveryFileRecord {
    normalize_ranges(&mut parts.recovered_ranges);
    normalize_ranges(&mut parts.suspect_ranges);
    RecoveryFileRecord {
        run_id: run_id.to_string(),
        file_id: entry.file_id.as_str().to_string(),
        path: entry.path.clone(),
        output_path: parts.output_path,
        entry_kind: entry_kind_name(entry.kind).to_string(),
        declared_size: entry.size_bytes,
        recovered_ranges: parts.recovered_ranges,
        suspect_ranges: parts.suspect_ranges,
        damage_ranges: parts.damage_ranges,
        status: parts.status,
        bytes_written: parts.bytes_written,
        adapter_state_hex: hex_bytes(&entry.adapter_state),
    }
}

fn recovery_status(
    declared_size: Option<u64>,
    bytes_written: u64,
    recovered_ranges: &[RecoveryByteRange],
    suspect_ranges: &[RecoveryByteRange],
) -> RecoveryStatus {
    if bytes_written == 0 {
        return RecoveryStatus::Missing;
    }
    let Some(size) = declared_size else {
        return RecoveryStatus::Partial;
    };
    let mut covered = Vec::with_capacity(recovered_ranges.len() + suspect_ranges.len());
    covered.extend_from_slice(recovered_ranges);
    covered.extend_from_slice(suspect_ranges);
    normalize_ranges(&mut covered);
    if ranges_cover(&covered, size) {
        RecoveryStatus::Complete
    } else {
        RecoveryStatus::Partial
    }
}

fn normalize_ranges(ranges: &mut Vec<RecoveryByteRange>) {
    ranges.retain(|range| range.start < range.end);
    ranges.sort_unstable_by_key(|range| (range.start, range.end));
    let mut merged: Vec<RecoveryByteRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(last) = merged.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    *ranges = merged;
}

fn ranges_cover(ranges: &[RecoveryByteRange], expected_end: u64) -> bool {
    if expected_end == 0 {
        return true;
    }
    let mut covered_until = 0u64;
    for range in ranges {
        if range.start > covered_until {
            return false;
        }
        covered_until = covered_until.max(range.end);
        if covered_until >= expected_end {
            return true;
        }
    }
    false
}

fn intersect_one_with_many(
    range: RecoveryByteRange,
    ranges: &[RecoveryByteRange],
) -> Vec<RecoveryByteRange> {
    ranges
        .iter()
        .filter_map(|other| intersect_ranges(range, *other))
        .collect()
}

fn intersect_many_with_one(
    ranges: &[RecoveryByteRange],
    range: RecoveryByteRange,
) -> Vec<RecoveryByteRange> {
    ranges
        .iter()
        .filter_map(|other| intersect_ranges(*other, range))
        .collect()
}

fn intersect_ranges(
    left: RecoveryByteRange,
    right: RecoveryByteRange,
) -> Option<RecoveryByteRange> {
    RecoveryByteRange::new(left.start.max(right.start), left.end.min(right.end))
}

fn subtract_many(
    mut range: RecoveryByteRange,
    subtract: &[RecoveryByteRange],
) -> Vec<RecoveryByteRange> {
    let mut remaining = Vec::new();
    let mut cuts = subtract.to_vec();
    normalize_ranges(&mut cuts);
    for cut in cuts {
        if cut.end <= range.start || cut.start >= range.end {
            continue;
        }
        if range.start < cut.start {
            remaining.push(RecoveryByteRange {
                start: range.start,
                end: cut.start,
            });
        }
        range.start = range.start.max(cut.end);
        if range.start >= range.end {
            return remaining;
        }
    }
    remaining.push(range);
    remaining
}

fn subtract_range_vec(
    ranges: &[RecoveryByteRange],
    subtract: RecoveryByteRange,
) -> Vec<RecoveryByteRange> {
    let mut out = Vec::new();
    for range in ranges {
        out.extend(subtract_many(*range, &[subtract]));
    }
    out
}

fn timestamp_now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn sanitize_for_suffix(value: &str) -> String {
    let text: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if text.is_empty() {
        "entry".to_string()
    } else {
        text
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn entry_kind_name(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::RegularFile => "regular",
        EntryKind::Directory => "directory",
        EntryKind::Symlink => "symlink",
        EntryKind::Hardlink => "hardlink",
        EntryKind::Special => "special",
    }
}

fn damage_status_name(status: DamageStatus) -> &'static str {
    match status {
        DamageStatus::ChecksumFailed => "checksum_failed",
        DamageStatus::ReadError => "read_error",
        DamageStatus::Missing => "missing",
        DamageStatus::Unsupported => "unsupported",
    }
}

fn archive_gap_cause_name(cause: remanence_format::ArchiveGapCause) -> &'static str {
    match cause {
        remanence_format::ArchiveGapCause::UnrecognizedData => "unrecognized_data",
        remanence_format::ArchiveGapCause::ReadError => "read_error",
        remanence_format::ArchiveGapCause::Missing => "missing",
        remanence_format::ArchiveGapCause::Resync => "resync",
        remanence_format::ArchiveGapCause::Unsupported => "unsupported",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use remanence_format::{
        ArchiveGapCause, EntryCatalogSink, FileDataSink, FileStreamReport, ScanReport,
    };

    #[test]
    fn recovery_manifest_records_partial_sparse_file() {
        let root = tempfile::Builder::new()
            .prefix("remanence-recovery-partial")
            .tempdir()
            .unwrap();
        let entry = normalized_entry("file-a", "camera/a.txt", EntryKind::RegularFile, Some(5));
        let damage = damage_range("file-a", 2, 5, DamageStatus::Missing);
        let mut reader = ScriptedArchiveReader {
            entries: vec![entry],
            damages: vec![damage],
            gaps: vec![],
            data: vec![("file-a".into(), 0, b"ab".to_vec())],
        };

        let report = recover_archive_reader_to_directory(
            &mut reader,
            root.path(),
            RecoveryOptions::new("test-format", "dump:test"),
        )
        .unwrap();

        assert_eq!(report.files_seen, 1);
        assert_eq!(report.files_written, 1);
        assert_eq!(report.bytes_written, 2);
        assert_eq!(report.files[0].status, RecoveryStatus::Partial);
        assert_eq!(
            report.files[0].recovered_ranges,
            vec![RecoveryByteRange { start: 0, end: 2 }]
        );
        assert_eq!(report.files[0].suspect_ranges, Vec::new());
        assert_eq!(
            fs::read(root.path().join("camera/a.txt")).unwrap(),
            b"ab\0\0\0"
        );

        let records = read_manifest(&report.manifest_path);
        assert!(matches!(records[0], RecoveryManifestRecord::RunStart(_)));
        assert!(records
            .iter()
            .any(|record| matches!(record, RecoveryManifestRecord::File(file) if file.status == RecoveryStatus::Partial)));
        assert!(matches!(
            records.last(),
            Some(RecoveryManifestRecord::RunEnd(_))
        ));
    }

    #[test]
    fn checksum_failed_bytes_are_suspect_not_clean() {
        let root = tempfile::Builder::new()
            .prefix("remanence-recovery-suspect")
            .tempdir()
            .unwrap();
        let entry = normalized_entry("file-a", "camera/a.txt", EntryKind::RegularFile, Some(5));
        let damage = damage_range("file-a", 2, 5, DamageStatus::ChecksumFailed);
        let mut reader = ScriptedArchiveReader {
            entries: vec![entry],
            damages: vec![damage],
            gaps: vec![],
            data: vec![
                ("file-a".into(), 0, b"ab".to_vec()),
                ("file-a".into(), 2, b"cde".to_vec()),
            ],
        };

        let report = recover_archive_reader_to_directory(
            &mut reader,
            root.path(),
            RecoveryOptions::new("test-format", "dump:test"),
        )
        .unwrap();

        assert_eq!(report.files[0].status, RecoveryStatus::Complete);
        assert_eq!(
            report.files[0].recovered_ranges,
            vec![RecoveryByteRange { start: 0, end: 2 }]
        );
        assert_eq!(
            report.files[0].suspect_ranges,
            vec![RecoveryByteRange { start: 2, end: 5 }]
        );
        assert_eq!(
            fs::read(root.path().join("camera/a.txt")).unwrap(),
            b"abcde"
        );
    }

    #[test]
    fn archive_gaps_are_flushed_to_manifest() {
        let root = tempfile::Builder::new()
            .prefix("remanence-recovery-gap")
            .tempdir()
            .unwrap();
        let entry = normalized_entry("file-a", "camera/a.txt", EntryKind::RegularFile, Some(1));
        let gap = ArchiveGapRange {
            source_start: 2048,
            source_end: 4096,
            cause: ArchiveGapCause::Resync,
            adapter_state: vec![0xab],
        };
        let mut reader = ScriptedArchiveReader {
            entries: vec![entry],
            damages: vec![],
            gaps: vec![gap],
            data: vec![("file-a".into(), 0, b"x".to_vec())],
        };

        let report = recover_archive_reader_to_directory(
            &mut reader,
            root.path(),
            RecoveryOptions::new("test-format", "dump:test"),
        )
        .unwrap();

        assert_eq!(report.archive_gaps.len(), 1);
        assert_eq!(report.archive_gaps[0].cause, "resync");
        let records = read_manifest(&report.manifest_path);
        assert!(records
            .iter()
            .any(|record| matches!(record, RecoveryManifestRecord::ArchiveGap(gap) if gap.source_start == 2048)));
    }

    #[test]
    fn duplicate_paths_are_deduplicated_with_file_id_suffix() {
        let root = tempfile::Builder::new()
            .prefix("remanence-recovery-dupe")
            .tempdir()
            .unwrap();
        let entries = vec![
            normalized_entry("file-a", "camera/a.txt", EntryKind::RegularFile, Some(1)),
            normalized_entry("file-b", "camera/a.txt", EntryKind::RegularFile, Some(1)),
            normalized_entry(
                "file-c",
                "camera/a.txt._file-b_1",
                EntryKind::RegularFile,
                Some(1),
            ),
        ];
        let mut reader = ScriptedArchiveReader {
            entries,
            damages: vec![],
            gaps: vec![],
            data: vec![
                ("file-a".into(), 0, b"a".to_vec()),
                ("file-b".into(), 0, b"b".to_vec()),
                ("file-c".into(), 0, b"c".to_vec()),
            ],
        };

        let report = recover_archive_reader_to_directory(
            &mut reader,
            root.path(),
            RecoveryOptions::new("test-format", "dump:test"),
        )
        .unwrap();

        assert_eq!(report.files.len(), 3);
        assert_eq!(report.files[0].output_path, "camera/a.txt");
        assert_eq!(report.files[1].output_path, "camera/a.txt._file-b_1");
        assert_eq!(
            report.files[2].output_path,
            "camera/a.txt._file-b_1._file-c_1"
        );
        assert_eq!(fs::read(root.path().join("camera/a.txt")).unwrap(), b"a");
        assert_eq!(
            fs::read(root.path().join("camera/a.txt._file-b_1")).unwrap(),
            b"b"
        );
        assert_eq!(
            fs::read(root.path().join("camera/a.txt._file-b_1._file-c_1")).unwrap(),
            b"c"
        );
    }

    #[test]
    fn non_normalizable_recovery_path_is_sanitized_and_manifested() {
        let root = tempfile::Builder::new()
            .prefix("remanence-recovery-sanitize-path")
            .tempdir()
            .unwrap();
        let entry = normalized_entry(
            "file-a",
            "../legacy/escape.txt",
            EntryKind::RegularFile,
            Some(1),
        );
        let mut reader = ScriptedArchiveReader {
            entries: vec![entry],
            damages: vec![],
            gaps: vec![],
            data: vec![("file-a".into(), 0, b"x".to_vec())],
        };

        let report = recover_archive_reader_to_directory(
            &mut reader,
            root.path(),
            RecoveryOptions::new("test-format", "dump:test"),
        )
        .expect("invalid archive path is sanitized for recovery");

        let file = report.files.first().expect("file manifest record");
        assert_eq!(file.path, "../legacy/escape.txt");
        assert!(file.output_path.starts_with("_remanence_recovered/"));
        assert_eq!(fs::read(root.path().join(&file.output_path)).unwrap(), b"x");
        assert!(!root.path().join("legacy/escape.txt").exists());
    }

    #[test]
    fn recovery_does_not_preallocate_declared_sparse_size() {
        let root = tempfile::Builder::new()
            .prefix("remanence-recovery-no-presize")
            .tempdir()
            .unwrap();
        let declared_size = 1_u64 << 50;
        let entry = normalized_entry(
            "file-a",
            "camera/huge.bin",
            EntryKind::RegularFile,
            Some(declared_size),
        );
        let mut reader = ScriptedArchiveReader {
            entries: vec![entry],
            damages: vec![],
            gaps: vec![],
            data: vec![("file-a".into(), 0, b"x".to_vec())],
        };

        let report = recover_archive_reader_to_directory(
            &mut reader,
            root.path(),
            RecoveryOptions::new("test-format", "dump:test"),
        )
        .expect("sparse partial recovery should not pre-size to declared length");

        assert_eq!(report.files[0].declared_size, Some(declared_size));
        assert_eq!(report.files[0].status, RecoveryStatus::Partial);
        assert_eq!(
            fs::metadata(root.path().join("camera/huge.bin"))
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_symlink_parent_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::Builder::new()
            .prefix("remanence-recovery-symlink-root")
            .tempdir()
            .unwrap();
        let outside = tempfile::Builder::new()
            .prefix("remanence-recovery-symlink-outside")
            .tempdir()
            .unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        let entry = normalized_entry("file-a", "escape/file.txt", EntryKind::RegularFile, Some(1));
        let mut reader = ScriptedArchiveReader {
            entries: vec![entry],
            damages: vec![],
            gaps: vec![],
            data: vec![("file-a".into(), 0, b"x".to_vec())],
        };

        let err = recover_archive_reader_to_directory(
            &mut reader,
            root.path(),
            RecoveryOptions::new("test-format", "dump:test"),
        )
        .expect_err("symlinked recovery parent must be rejected");

        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(!outside.path().join("file.txt").exists());
    }

    fn read_manifest(path: &Path) -> Vec<RecoveryManifestRecord> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn normalized_entry(
        file_id: &str,
        path: &str,
        kind: EntryKind,
        size_bytes: Option<u64>,
    ) -> NormalizedEntry {
        NormalizedEntry {
            file_id: file_id.into(),
            path: path.to_string(),
            kind,
            link_target: None,
            size_bytes,
            adapter_state: Vec::new(),
        }
    }

    fn damage_range(file_id: &str, start: u64, end: u64, status: DamageStatus) -> DamageRange {
        DamageRange {
            file_id: file_id.into(),
            start,
            end,
            status,
            adapter_state: Vec::new(),
        }
    }

    struct ScriptedArchiveReader {
        entries: Vec<NormalizedEntry>,
        damages: Vec<DamageRange>,
        gaps: Vec<ArchiveGapRange>,
        data: Vec<(FileId, u64, Vec<u8>)>,
    }

    impl ArchiveReader for ScriptedArchiveReader {
        fn scan(&mut self, sink: &mut dyn EntryCatalogSink) -> Result<ScanReport, FormatError> {
            for entry in &self.entries {
                sink.entry(entry)?;
            }
            for damage in &self.damages {
                sink.damage(damage)?;
            }
            for gap in &self.gaps {
                sink.archive_gap(gap)?;
            }
            Ok(ScanReport {
                entries: self.entries.len() as u64,
                damage_events: self.damages.len() as u64,
                archive_gaps: self.gaps.len() as u64,
            })
        }

        fn stream_all(
            &mut self,
            sink: &mut dyn ArchiveEventSink,
        ) -> Result<StreamReport, FormatError> {
            let mut bytes = 0u64;
            for entry in &self.entries {
                sink.begin_entry(entry)?;
                for gap in &self.gaps {
                    sink.report_archive_gap(gap)?;
                }
                for damage in self
                    .damages
                    .iter()
                    .filter(|damage| damage.file_id == entry.file_id)
                {
                    sink.report_damage(damage)?;
                }
                for (_, file_offset, data) in self
                    .data
                    .iter()
                    .filter(|(file_id, _, _)| *file_id == entry.file_id)
                {
                    sink.write_file_data(*file_offset, data)?;
                    bytes += data.len() as u64;
                }
                sink.end_entry(entry)?;
            }
            Ok(StreamReport {
                entries: self.entries.len() as u64,
                bytes,
                damage_events: self.damages.len() as u64,
                archive_gaps: self.gaps.len() as u64,
            })
        }

        fn stream_file(
            &mut self,
            _file_id: &FileId,
            _sink: &mut dyn FileDataSink,
        ) -> Result<FileStreamReport, FormatError> {
            Err(FormatError::unsupported(
                "scripted archive reader does not implement stream_file",
            ))
        }
    }
}
