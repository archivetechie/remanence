//! Catalogless recovery uses the terminal inventory or the existing BOT walk.
//! Candidates are spooled once, validated, and extracted through the normal
//! containment-enforcing filesystem sink; tape header IDs never name paths.
use crate::pool_write::ValidateCanonicalEntrySink;
use crate::{position_to_object, BotRelativeTapeFile};
use remanence_library::{BlockRead, BlockSize, DriveHandle, DriveHandleSource, FileBlockSource};
use remanence_parity::{
    DriveHandleRawSource, TerminalInventoryOutcome, TerminalInventoryStreamEvent,
};
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Restore all structurally complete BOT candidates without catalog writes.
pub fn scan_and_restore(
    drive: &mut DriveHandle,
    destination: &Path,
    private_key: Option<&remanence_aead::RecipientPrivateKey>,
) -> Result<Value, String> {
    restore_catalogless_selected(drive, Some(destination), private_key, true, false, &[])
}
/// Prefer a validated terminal index; fall back to structural BOT candidates.
pub fn restore_catalogless(
    drive: &mut DriveHandle,
    destination: &Path,
    private_key: Option<&remanence_aead::RecipientPrivateKey>,
) -> Result<Value, String> {
    restore_catalogless_selected(drive, Some(destination), private_key, false, false, &[])
}
/// Discover, validate, and optionally extract selected members through the common sink.
pub fn restore_catalogless_selected(
    drive: &mut DriveHandle,
    destination: Option<&Path>,
    key: Option<&remanence_aead::RecipientPrivateKey>,
    scan: bool,
    list: bool,
    members: &[String],
) -> Result<Value, String> {
    if !list && !members.is_empty() {
        let listing = restore_catalogless_selected(drive, None, key, scan, true, members)?;
        if listing["candidates"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|r| r["manifest"]["encrypted"] == true))
        {
            return Err("encrypted objects require --private-key to resolve --path".into());
        }
    }
    let destination = if list {
        None
    } else {
        Some(destination.ok_or("restore requires --out")?)
    };
    if let Some(destination) = destination {
        remanence_stream::ensure_restore_root(destination).map_err(error)?;
    }
    let mut matched = vec![false; members.len()];
    let bootstrap = remanence_parity::bootstrap::discover_bootstrap(
        &mut DriveHandleRawSource::new(drive),
        None,
    )
    .map_err(error)?;
    let size = bootstrap.block_size_bytes;
    let mut candidates = Vec::new();
    let mut terminal = false;
    if !scan {
        let outcome = remanence_parity::read_terminal_index_inventory_streamed(
            &mut DriveHandleRawSource::new(drive),
            &bootstrap.tape_uuid,
            size,
            |event| {
                match event {
                    TerminalInventoryStreamEvent::ReplicaAttemptStarted { .. }
                    | TerminalInventoryStreamEvent::ReplicaAttemptRejected { .. } => {
                        candidates.clear()
                    }
                    TerminalInventoryStreamEvent::ObjectRow { row, .. } => {
                        let anchor = match row.representation {
                            remanence_parity::ObjectRecoveryRepresentation::Plaintext {
                                manifest_sha256,
                                ..
                            } => Some(manifest_sha256),
                            _ => None,
                        };
                        candidates.push((
                            row.tape_file_number,
                            Some(row.stored_block_count),
                            Some(row.object_id),
                            anchor,
                        ));
                    }
                    _ => {}
                }
                Ok(())
            },
        )
        .map_err(error)?;
        terminal = matches!(outcome, TerminalInventoryOutcome::Inventory(_));
    }
    if !terminal {
        candidates.clear();
        remanence_parity::recover_terminal_inventory_from_bot(
            &mut DriveHandleRawSource::new(drive),
            &bootstrap.tape_uuid,
            size,
            |candidate| {
                candidates.push((
                    candidate.tape_file_number,
                    candidate.stored_block_count,
                    None,
                    None,
                ));
                Ok(())
            },
        )
        .map_err(error)?;
    }
    let config = drive.read_config().map_err(error)?;
    drive
        .write_config(remanence_library::TapeConfig {
            block_size: BlockSize::Fixed { size_bytes: size },
            compression: false,
            ..config
        })
        .map_err(error)?;
    let mut outcomes = Vec::new();
    for (ordinal, (file, count, id, anchor)) in candidates.into_iter().enumerate() {
        let name = format!("candidate-{}", ordinal + 1);
        let result = (|| {
            let count = count.ok_or_else(|| "incomplete candidate at EOD".to_string())?;
            position_to_object(&mut *drive, BotRelativeTapeFile(file)).map_err(error)?;
            if list {
                let start = drive
                    .known_position()
                    .ok_or("unknown candidate position")?
                    .lba;
                let mut source = TapeCandidate::new(drive, start, count, size).map_err(error)?;
                let mut magic = [0; 4];
                source.read_exact(&mut magic).map_err(error)?;
                source.seek(SeekFrom::Start(0)).map_err(error)?;
                let (validated, envelope_id) = if &magic == b"REMO" {
                    let inspected = remanence_aead::inspect_reader(&mut source).map_err(error)?;
                    if inspected.header.chunk_size != size {
                        return Err("encrypted chunk size differs from bootstrap".into());
                    }
                    let object_id = inspected.header.object_id.clone();
                    check_identity(&object_id, id.as_deref(), None)?;
                    let Some(key) = key else {
                        return Ok(
                            json!({"object_id": object_id, "encrypted": true, "message": "encrypted — add --private-key to list members"}),
                        );
                    };
                    source.seek(SeekFrom::Start(0)).map_err(error)?;
                    let opened =
                        remanence_aead::open(&mut source, std::io::sink(), key).map_err(error)?;
                    source.seek(SeekFrom::Start(0)).map_err(error)?;
                    let prefix_len = 128
                        + opened.header.key_frame_len as usize
                        + opened.header.metadata_frame_len as usize;
                    let mut prefix = vec![0; prefix_len];
                    source.read_exact(&mut prefix).map_err(error)?;
                    let blocks = opened.metadata.plaintext_size / u64::from(size);
                    let mut plain = EncryptedCandidate {
                        source,
                        prefix,
                        key,
                        offset: 0,
                    };
                    (
                        validate_candidate(&mut plain, size, blocks, anchor).map_err(error)?,
                        Some(object_id),
                    )
                } else {
                    (
                        validate_candidate(&mut source, size, count, anchor).map_err(error)?,
                        None,
                    )
                };
                let object_id = validated
                    .global_pax
                    .get("REMANENCE.object_id")
                    .ok_or("missing object id")?;
                check_identity(object_id, id.as_deref(), envelope_id.as_deref())?;
                let rows = selected_rows(&validated, object_id, members, &mut matched);
                return Ok(json!({"object_id": object_id, "rows": rows}));
            }
            let destination = destination.expect("restore destination checked");
            // Disk spooling bounds memory independently of object size and lets
            // validation finish before any untrusted member reaches extraction.
            let staging = remanence_stream::create_restore_staging(destination).map_err(error)?;
            let stored_path = staging.path().join("stored");
            let mut stored = File::create(&stored_path).map_err(error)?;
            let mut block = vec![0; size as usize];
            let mut source = DriveHandleSource(drive);
            for _ in 0..count {
                let n = source.read_block(&mut block).map_err(error)?;
                if n != block.len() {
                    return Err("short candidate block".into());
                }
                stored.write_all(&block).map_err(error)?;
            }
            drop(stored);
            let mut stored = File::open(&stored_path).map_err(error)?;
            let mut magic = [0; 4];
            stored.read_exact(&mut magic).map_err(error)?;
            stored.seek(SeekFrom::Start(0)).map_err(error)?;
            let mut envelope_id = None;
            let plain_path = if &magic == b"REMO" {
                let key =
                    key.ok_or_else(|| "encrypted candidate requires --private-key".to_string())?;
                let path = staging.path().join("plaintext");
                let opened = remanence_aead::open(stored, File::create(&path).map_err(error)?, key)
                    .map_err(error)?;
                if opened.header.chunk_size != size {
                    return Err("encrypted chunk size differs from bootstrap".into());
                }
                std::fs::remove_file(&stored_path).map_err(error)?;
                envelope_id = Some(opened.header.object_id);
                path
            } else {
                stored_path
            };
            let validated = validate_plain_candidate(&plain_path, size, anchor).map_err(error)?;
            let blocks = std::fs::metadata(&plain_path).map_err(error)?.len() / u64::from(size);
            let object_id = validated
                .global_pax
                .get("REMANENCE.object_id")
                .ok_or_else(|| "missing object id".to_string())?;
            check_identity(object_id, id.as_deref(), envelope_id.as_deref())?;
            let rows = selected_rows(&validated, object_id, members, &mut matched);
            if !members.is_empty() && rows.is_empty() {
                return Ok(json!({"object_id": object_id, "files_written": 0}));
            }
            let output = destination.join(&name);
            remanence_stream::ensure_restore_root(&output).map_err(error)?;
            // Preserve the established layout unless a member occupies the
            // report namespace. In that case every payload member goes beneath
            // payload/, and manifest.json remains metadata only.
            let payload = if validated.entries.iter().any(|entry| {
                Path::new(&entry.path).components().next()
                    == Some(std::path::Component::Normal(std::ffi::OsStr::new(
                        "manifest.json",
                    )))
            }) {
                "payload"
            } else {
                "."
            };
            let payload_root = if payload == "." {
                output.clone()
            } else {
                output.join(payload)
            };
            let mut source = FileBlockSource::open(&plain_path, size as usize).map_err(error)?;
            let report = remanence_stream::restore_object_to_directory(
                &mut source,
                size as usize,
                blocks,
                &payload_root,
                remanence_stream::FilesystemRestoreOptions {
                    members: members.to_vec(),
                    ..Default::default()
                },
            )
            .map_err(error)?;
            let manifest = json!({"payload": payload, "object_id": object_id, "tape_file_number": file, "files_written": report.files_written, "warnings": staging.warnings.iter().chain(&report.warnings).collect::<Vec<_>>()});
            // Metadata and payload have disjoint namespaces; member names cannot
            // collide with this report. create_new still refuses existing links.
            let manifest_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output.join("manifest.json"))
                .map_err(error)?;
            serde_json::to_writer_pretty(manifest_file, &manifest).map_err(error)?;
            Ok::<_, String>(manifest)
        })();
        outcomes.push(match result {
            Ok(manifest) => json!({"candidate": name, "status": if list { "listed" } else { "restored" }, "manifest": manifest}),
            Err(detail) => json!({"candidate": name, "object_id": id.as_deref().map(String::from_utf8_lossy), "status": "refused", "error": detail}),
        });
    }
    let missing: Vec<_> = members
        .iter()
        .zip(&matched)
        .filter(|(_, hit)| !**hit)
        .map(|(path, _)| path.as_str())
        .collect();
    let keyless = outcomes.iter().any(|r| r["manifest"]["encrypted"] == true);
    if !missing.is_empty() && !keyless {
        return Err(format!(
            "no members matched: {}; discovery report: {}",
            missing.join(", "),
            json!(outcomes)
        ));
    }
    Ok(json!({"terminal_index": terminal, "candidates": outcomes}))
}

/// Validate candidates through the same canonical reader used by restoration.
fn validate_plain_candidate(
    path: &Path,
    size: u32,
    anchor: Option<[u8; 32]>,
) -> Result<remanence_format::RemTarStreamReport, remanence_format::error::FormatError> {
    let mut source = FileBlockSource::open(path, size as usize)?;
    let blocks = source.block_count();
    validate_candidate(&mut source, size, blocks, anchor)
}

fn validate_candidate(
    source: &mut dyn BlockRead,
    size: u32,
    blocks: u64,
    anchor: Option<[u8; 32]>,
) -> Result<remanence_format::RemTarStreamReport, remanence_format::error::FormatError> {
    remanence_format::stream_rem_tar_object_with_manifest_anchor(
        source,
        size as usize,
        blocks,
        &mut ValidateCanonicalEntrySink,
        anchor,
    )
}

/// Keep terminal and envelope identities bound to the validated inner object.
fn check_identity(
    object_id: &str,
    terminal: Option<&[u8]>,
    envelope: Option<&str>,
) -> Result<(), String> {
    if envelope.is_some_and(|e| e != object_id)
        || terminal.is_some_and(|e| e != object_id.as_bytes())
    {
        return Err("object identity differs from recovery authority".into());
    }
    Ok(())
}

fn selected_rows(
    report: &remanence_format::RemTarStreamReport,
    object_id: &str,
    members: &[String],
    matched: &mut [bool],
) -> Vec<Value> {
    report
        .entries
        .iter()
        .filter(|entry| entry.path != remanence_format::MANIFEST_PATH)
        .filter_map(|entry| {
            for (member, hit) in members.iter().zip(matched.iter_mut()) {
                *hit |= remanence_stream::restore_member_selected(
                    &entry.path,
                    std::slice::from_ref(member),
                );
            }
            remanence_stream::restore_member_selected(&entry.path, members).then(
                || json!({"path": entry.path, "size": entry.size_bytes, "object_id": object_id}),
            )
        })
        .collect()
}

/// Bounded decrypted block view for write-free listing after envelope validation.
struct EncryptedCandidate<'a, 'b> {
    source: TapeCandidate<'a>,
    prefix: Vec<u8>,
    key: &'b remanence_aead::RecipientPrivateKey,
    offset: u64,
}
impl BlockRead for EncryptedCandidate<'_, '_> {
    fn read_block(&mut self, out: &mut [u8]) -> Result<usize, remanence_library::TapeIoError> {
        let run = (|| -> Result<usize, String> {
            let range = remanence_aead::covering_stored_range(
                &self.prefix,
                self.key,
                self.offset,
                out.len() as u64,
            )
            .map_err(error)?;
            let start = range.stored_range_start.ok_or("empty encrypted range")?;
            self.source.seek(SeekFrom::Start(start)).map_err(error)?;
            let mut writer = &mut out[..];
            remanence_aead::open_plaintext_range_from_reader(
                &self.prefix,
                &mut self.source,
                start,
                &mut writer,
                self.key,
                self.offset,
                range.plaintext_len,
            )
            .map_err(error)?;
            self.offset += range.plaintext_len;
            Ok(range.plaintext_len as usize)
        })();
        run.map_err(remanence_library::TapeIoError::OperationFailed)
    }
}

/// Physical evidence beyond the caller's checkpoint. No state rows authorize it.
#[derive(Clone, Debug, Default)]
pub struct AppendSuffix {
    /// Complete plaintext objects or keylessly validated encrypted envelopes.
    pub objects: Vec<String>,
    /// Terminal control files that cannot belong to the supplied plan.
    pub foreign_components: Vec<String>,
    /// At least one incomplete candidate was present.
    pub torn_fragment: bool,
}

/// Scan candidates directly from tape, stopping as soon as refusal is proved.
/// Physical EOD without a closing filemark proves the final candidate is torn.
pub(crate) fn scan_append_suffix(
    drive: &mut DriveHandle,
    tape_uuid: &[u8; 16],
    size: u32,
    start_lba: u64,
    terminal: Option<&remanence_parity::TerminalTripleWritePlan>,
    prefix: Option<&remanence_parity::TerminalPrefixPlan>,
    inspect_all: bool,
) -> Result<AppendSuffix, crate::append_admission::AdmissionError> {
    use crate::append_admission::AdmissionError;
    use remanence_library::TapeIoError;
    use remanence_parity::TapeFileKind;
    let mut result = AppendSuffix::default();
    let mut lba = start_lba;
    let mut block = vec![0; size as usize];
    let mut scanned = 0u64;
    let mut next_progress = 1u64 << 30;
    loop {
        let start = lba;
        let mut head = Vec::new();
        let mut aligned = true;
        let mut blocks = 0;
        let eod = loop {
            match drive.read_block(&mut block) {
                Ok(n) => {
                    if n == 0 {
                        return Err("zero-length successful read does not prove EOD".into());
                    }
                    if head.is_empty() {
                        head.extend_from_slice(&block[..n]);
                    }
                    aligned &= n == size as usize;
                    blocks += 1;
                    lba = lba.checked_add(1).ok_or("suffix LBA overflow")?;
                    if blocks == 1
                        && remanence_parity::scan::classify_tape_file_head(
                            &head, tape_uuid, size, false,
                        ) != TapeFileKind::Object
                    {
                        let spaced = drive
                            .space(1, remanence_library::SpaceKind::Filemarks)
                            .map_err(AdmissionError::Io)?;
                        if spaced.position_after.partition != 0
                            || spaced.position_after.lba <= start
                        {
                            return Err("control suffix position mismatch".into());
                        }
                        lba = spaced.position_after.lba;
                        if spaced.units_traversed == 1 {
                            break false;
                        }
                        match drive.read_block(&mut block) {
                            Err(e) if e.is_physical_eod() => break true,
                            Err(e) => return Err(AdmissionError::Io(e)),
                            Ok(_) => return Err("control suffix did not reach physical EOD".into()),
                        }
                    }
                    scanned += n as u64;
                    if scanned >= next_progress {
                        eprintln!(
                            "Inspecting tape suffix: {} GiB read, LBA {lba}",
                            scanned >> 30
                        );
                        next_progress = scanned.saturating_add(1 << 30);
                    }
                }
                Err(TapeIoError::FilemarkEncountered) => {
                    lba = lba.checked_add(1).ok_or("suffix LBA overflow")?;
                    break false;
                }
                Err(e) if e.is_physical_eod() => break true,
                Err(e) => return Err(AdmissionError::Io(e)),
            }
        };
        // Only physical EOD without a closing filemark proves truncation.
        // This applies to every candidate, even an incomplete or corrupt header.
        if eod && !head.is_empty() {
            result.torn_fragment = true;
            return Ok(result);
        }
        if !head.is_empty() {
            let kind =
                remanence_parity::scan::classify_tape_file_head(&head, tape_uuid, size, false);
            match kind {
                TapeFileKind::Object => {
                    let mut candidate = TapeCandidate::new(drive, start, blocks, size)?;
                    let classified = if !aligned {
                        Err("short or unaligned record".to_string())
                    } else if head.starts_with(b"REMO") {
                        // The header geometry is checked before accepting even an
                        // incomplete envelope; unsupported versions never become torn.
                        let header = head
                            .get(..128)
                            .and_then(|b| b.try_into().ok())
                            .ok_or_else(|| "short envelope header".to_string())
                            .and_then(|h| remanence_aead::RemObjectHeader::parse(h).map_err(error));
                        match header {
                            Ok(h)
                                if h.chunk_size == size
                                    && blocks * u64::from(size)
                                        >= 128 + u64::from(h.key_frame_len) =>
                            {
                                match remanence_aead::inspect_reader(&mut candidate) {
                                    Ok(report) => Ok(Some(report.header.object_id)),
                                    Err(e) => Err(e.to_string()),
                                }
                            }
                            Ok(h) if h.chunk_size != size => {
                                Err("envelope geometry differs from tape".into())
                            }
                            Ok(_) => {
                                Err("incomplete key frame cannot prove a valid envelope prefix"
                                    .into())
                            }
                            Err(e) => Err(e),
                        }
                    } else {
                        let mut prefix = ValidateCanonicalEntrySink;
                        match remanence_format::stream_rem_tar_object_with_manifest_anchor(
                            &mut candidate,
                            size as usize,
                            blocks,
                            &mut prefix,
                            None,
                        ) {
                            Ok(report) => report
                                .global_pax
                                .get("REMANENCE.object_id")
                                .cloned()
                                .map(Some)
                                .ok_or_else(|| "missing object id".into()),
                            Err(e) => Err(e.to_string()),
                        }
                    };
                    if let Some(error) = candidate.probe_error.take() {
                        return Err(AdmissionError::Io(TapeIoError::OperationFailed(error)));
                    }
                    match classified {
                        Ok(Some(id)) => result.objects.push(id),
                        Ok(None) => result.torn_fragment = true,
                        Err(detail) => result
                            .foreign_components
                            .push(format!("unclassifiable at LBA {start}: {detail}")),
                    }
                    drive.locate(lba).map_err(AdmissionError::Io)?;
                }
                TapeFileKind::TapeIndexReplica => {
                    let parsed =
                        remanence_parity::parse_tape_index_replica_header(&head, tape_uuid);
                    let own = terminal.is_some_and(|plan| {
                        parsed.as_ref().is_ok_and(|h| {
                            plan.replicas
                                .iter()
                                .any(|p| p == &h.plan && p.component.planned_start_lba == start)
                        })
                    });
                    let planned_start = terminal.is_some_and(|plan| {
                        plan.replicas
                            .iter()
                            .any(|p| p.component.planned_start_lba == start)
                    });
                    if !own && (parsed.is_ok() || !planned_start) {
                        result
                            .foreign_components
                            .push(format!("terminal replica at LBA {start}"));
                    }
                }
                TapeFileKind::IndexSeparationExtent => {
                    let parsed = remanence_parity::parse_index_separation_header(&head, tape_uuid);
                    let own = terminal.is_some_and(|plan| {
                        parsed.as_ref().is_ok_and(|h| {
                            plan.separations
                                .iter()
                                .any(|p| p == &h.plan && p.component.planned_start_lba == start)
                        })
                    });
                    let planned_start = terminal.is_some_and(|plan| {
                        plan.separations
                            .iter()
                            .any(|p| p.component.planned_start_lba == start)
                    });
                    if !own && (parsed.is_ok() || !planned_start) {
                        result
                            .foreign_components
                            .push(format!("terminal separation at LBA {start}"));
                    }
                }
                TapeFileKind::ParityMap => {
                    let hash = remanence_parity::parity_map::parse_parity_map_header_block(
                        &head, tape_uuid,
                    )
                    .ok()
                    .map(|h| h.payload_sha256);
                    let own = prefix.is_some_and(|p| {
                        p.committed_bundle.entries.iter().any(|e| {
                            e.kind == kind
                                && e.physical_start_hint == Some(start)
                                && e.canonical_metadata_hash == hash
                        })
                    });
                    if !own {
                        result
                            .foreign_components
                            .push(format!("terminal parity map at LBA {start}"));
                    }
                }
                TapeFileKind::ParitySidecar => {
                    let hash =
                        remanence_parity::sidecar::parse_sidecar_header_block(&head, tape_uuid)
                            .ok()
                            .map(|h| h.canonical_metadata_hash);
                    let own = prefix.is_some_and(|p| {
                        p.committed_bundle.entries.iter().any(|e| {
                            e.kind == kind
                                && e.physical_start_hint == Some(start)
                                && e.canonical_metadata_hash == hash
                        })
                    });
                    if !own {
                        result
                            .foreign_components
                            .push(format!("terminal sidecar at LBA {start}"));
                    }
                }
                _ => result
                    .foreign_components
                    .push(format!("unclassifiable at LBA {start}")),
            }
        } else if !eod {
            result.torn_fragment = true;
        }
        if eod
            || (!inspect_all
                && (!result.objects.is_empty() || !result.foreign_components.is_empty()))
        {
            return Ok(result);
        }
    }
}

/// Object-local block/byte view backed by LOCATE and READ, never a disk spool.
/// The initial walk measures geometry; existing validators then consume this view.
struct TapeCandidate<'a> {
    drive: &'a mut DriveHandle,
    start: u64,
    len: u64,
    size: u32,
    offset: u64,
    cached_lba: Option<u64>,
    probe_error: Option<String>,
    block: Vec<u8>,
}
impl<'a> TapeCandidate<'a> {
    fn new(
        drive: &'a mut DriveHandle,
        start: u64,
        blocks: u64,
        size: u32,
    ) -> Result<Self, crate::append_admission::AdmissionError> {
        let len = blocks
            .checked_mul(u64::from(size))
            .ok_or("candidate length overflow")?;
        drive
            .locate(start)
            .map_err(crate::append_admission::AdmissionError::Io)?;
        Ok(Self {
            drive,
            start,
            len,
            size,
            offset: 0,
            cached_lba: None,
            probe_error: None,
            block: vec![0; size as usize],
        })
    }
}
impl Read for TapeCandidate<'_> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() || self.offset == self.len {
            return Ok(0);
        }
        let lba = self.start + self.offset / u64::from(self.size);
        if self.cached_lba != Some(lba) {
            if self.drive.known_position().is_none_or(|p| p.lba != lba) {
                if let Err(error) = self.drive.locate(lba) {
                    self.probe_error = Some(error.to_string());
                    return Err(std::io::Error::other(error));
                }
            }
            let n = match self.drive.read_block(&mut self.block) {
                Ok(n) => n,
                Err(error) => {
                    self.probe_error = Some(error.to_string());
                    return Err(std::io::Error::other(error));
                }
            };
            if n != self.size as usize {
                self.probe_error = Some("candidate geometry changed during rescan".into());
                return Err(std::io::Error::other("short candidate block"));
            }
            self.cached_lba = Some(lba);
        }
        let within = (self.offset % u64::from(self.size)) as usize;
        let n = out.len().min(self.block.len() - within);
        out[..n].copy_from_slice(&self.block[within..within + n]);
        self.offset += n as u64;
        Ok(n)
    }
}
impl Seek for TapeCandidate<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let offset = match pos {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.offset) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.len) + i128::from(n),
        };
        if offset < 0 || offset > i128::from(self.len) {
            return Err(std::io::Error::other("candidate seek outside file"));
        }
        self.offset = offset as u64;
        Ok(self.offset)
    }
}
impl BlockRead for TapeCandidate<'_> {
    fn read_block(&mut self, out: &mut [u8]) -> Result<usize, remanence_library::TapeIoError> {
        self.read(out)
            .map_err(|e| remanence_library::TapeIoError::OperationFailed(e.to_string()))
    }
}
