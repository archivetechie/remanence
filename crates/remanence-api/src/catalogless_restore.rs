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
    restore(drive, destination, private_key, true)
}
/// Prefer a validated terminal index; fall back to structural BOT candidates.
pub fn restore_catalogless(
    drive: &mut DriveHandle,
    destination: &Path,
    private_key: Option<&remanence_aead::RecipientPrivateKey>,
) -> Result<Value, String> {
    restore(drive, destination, private_key, false)
}
fn restore(
    drive: &mut DriveHandle,
    destination: &Path,
    key: Option<&remanence_aead::RecipientPrivateKey>,
    scan: bool,
) -> Result<Value, String> {
    remanence_stream::ensure_restore_root(destination).map_err(error)?;
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
            // Disk spooling bounds memory independently of object size and lets
            // validation finish before any untrusted member reaches extraction.
            let staging = tempfile::tempdir().map_err(error)?;
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
                envelope_id = Some(opened.header.object_id);
                path
            } else {
                stored_path
            };
            let mut source = FileBlockSource::open(&plain_path, size as usize).map_err(error)?;
            let blocks = source.block_count();
            let validated = remanence_format::stream_rem_tar_object_with_manifest_anchor(
                &mut source,
                size as usize,
                blocks,
                &mut ValidateCanonicalEntrySink,
                anchor,
            )
            .map_err(error)?;
            let object_id = validated
                .global_pax
                .get("REMANENCE.object_id")
                .ok_or_else(|| "missing object id".to_string())?;
            if envelope_id
                .as_ref()
                .is_some_and(|expected| expected != object_id)
                || id
                    .as_ref()
                    .is_some_and(|expected| expected.as_slice() != object_id.as_bytes())
            {
                return Err("object identity differs from recovery authority".into());
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
                Default::default(),
            )
            .map_err(error)?;
            let manifest = json!({"payload": payload, "object_id": object_id, "tape_file_number": file, "files_written": report.files_written});
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
            Ok(manifest) => json!({"candidate": name, "status": "restored", "manifest": manifest}),
            Err(detail) => json!({"candidate": name, "status": "refused", "error": detail}),
        });
    }
    Ok(json!({"terminal_index": terminal, "candidates": outcomes}))
}
