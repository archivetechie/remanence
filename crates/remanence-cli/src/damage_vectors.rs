//! Execute the frozen damage matrix against production discovery, streamed
//! inventory, BOT recovery, sidecar recovery and full verification. Expected
//! results never affect reader behavior; only the comparison interprets them.

use crate::tape_image_vectors::{
    apply_record_edits, check_hints, check_observations, generate, hex, VectorImage, BLOCK,
    EXPECTATIONS,
};
use remanence_chaos::{
    model::{DeviceRole, ModelTransport, Record, VirtualTape, VirtualWorld},
    ChaosTransport, DeviceCtx, FaultEngine,
};
use remanence_library::scsi::ScsiError;
use remanence_library::transport::{SgTransport, TimeoutClass, TransferOutcome};
use remanence_library::{BlockRead, BlockSource, DriveHandle, TapeIoError};
use remanence_parity::bootstrap::discover_bootstrap_with_recovery_hints;
use remanence_parity::raw::tape_error_is_current_medium_damage;
use remanence_parity::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

/// Report a record longer than the host buffer as a drive does for a
/// variable-block READ(6) with SILI clear: CHECK CONDITION, ILI, VALID and
/// INFORMATION = requested − actual, with the record consumed. The chaos model
/// silently truncates such a read, so the record-length fault needs this to be
/// observed faithfully. Every other command passes through unchanged.
struct OverlengthReads<T> {
    inner: T,
    scratch: Vec<u8>,
}

impl<T: SgTransport> SgTransport for OverlengthReads<T> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        let variable_read = cdb.first() == Some(&0x08) && cdb.get(1).is_some_and(|b| b & 1 == 0);
        if !variable_read {
            return self.inner.execute_in(cdb, buf);
        }
        // Read with the largest transfer READ(6) allows, then report the fit.
        const MAX_TRANSFER: usize = 0x00ff_ffff;
        self.scratch.resize(MAX_TRANSFER, 0);
        let mut probe = cdb.to_vec();
        probe[2..5].copy_from_slice(&(MAX_TRANSFER as u32).to_be_bytes()[1..]);
        let outcome = self.inner.execute_in(&probe, &mut self.scratch)?;
        let actual = outcome.bytes_transferred as usize;
        let copied = actual.min(buf.len());
        buf[..copied].copy_from_slice(&self.scratch[..copied]);
        if actual <= buf.len() {
            return Ok(outcome);
        }
        let mut sense = vec![0u8; 18];
        sense[0] = 0xf0; // VALID, current fixed-format sense
        sense[2] = 0x20; // NO SENSE with ILI
        let information = buf.len() as i64 - actual as i64;
        sense[3..7].copy_from_slice(&(information as i32).to_be_bytes());
        sense[7] = 10;
        Err(ScsiError::CheckCondition {
            sense,
            bytes_transferred: buf.len() as u32,
        })
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

pub(crate) fn source(vector: &VectorImage, faults: &Value) -> (DriveHandle, FaultEngine) {
    // No key of the fault map may be ignored silently.
    for key in faults.as_object().expect("fault map object").keys() {
        assert!(
            matches!(
                key.as_str(),
                "image"
                    | "unreadable_records"
                    | "removed_filemark_after_tape_file"
                    | "failed_data_addresses"
                    | "hints"
                    | "record_edits"
                    | "observations"
                    | "read_data_addresses"
                    | "record_insertions"
                    | "appended_files"
            ),
            "unknown fault map key {key}"
        );
    }
    // An inserted record shifts every later position, so the faults that name
    // positions of the original image are not combined with it.
    if faults.get("record_insertions").is_some() {
        assert!(
            faults["unreadable_records"].as_array().unwrap().is_empty()
                && faults.get("record_edits").is_none(),
            "record insertions do not combine with position-named faults"
        );
    }
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK);
    let removed = faults["removed_filemark_after_tape_file"].as_u64();
    let edits = faults["record_edits"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let insertions = faults["record_insertions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for (i, file) in vector.image.files.iter().enumerate() {
        for (index, block) in file.bytes.chunks_exact(BLOCK as usize).enumerate() {
            let edited = edits
                .iter()
                .find(|e| e["tape_file"] == json!(i) && e["record_index"] == json!(index));
            tape.records.push(Record::Block(match edited {
                Some(resolved) => apply_record_edits(block, resolved),
                None => block.to_vec(),
            }));
            for inserted in insertions
                .iter()
                .filter(|r| r["tape_file"] == json!(i) && r["after_record_index"] == json!(index))
            {
                let bytes =
                    vec![
                        crate::tape_image_vectors::unhex_byte(inserted["fill"].as_str().unwrap());
                        inserted["length"].as_u64().unwrap() as usize
                    ];
                assert_eq!(
                    hex(&Sha256::digest(&bytes)),
                    inserted["sha256"].as_str().unwrap(),
                    "resolved inserted record digest"
                );
                tape.records.push(Record::Block(bytes));
            }
        }
        if file.filemark_record.is_some() && removed != Some(i as u64) {
            tape.records.push(Record::Filemark);
        }
    }
    for appended in faults["appended_files"]
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        for key in appended.as_object().expect("appended file object").keys() {
            assert!(
                matches!(key.as_str(), "records" | "trailing_filemark" | "replica"),
                "unknown appended file key {key}"
            );
        }
        if let Some(stated) = appended.get("replica") {
            // The stated plan must agree with the bytes the executor builds.
            let first = &appended["records"][0];
            let built = crate::tape_image_vectors::second_edition_replica_a(
                first["planned_tape_file_number"].as_u64().unwrap(),
                first["planned_start_lba"].as_u64().unwrap(),
            )
            .unwrap();
            assert_eq!(
                *stated,
                crate::tape_image_vectors::replica_summary(
                    &built,
                    first["planned_tape_file_number"].as_u64().unwrap(),
                    first["planned_start_lba"].as_u64().unwrap(),
                    stated["base"]["tape_file"].as_u64().unwrap() as usize,
                ),
                "second-edition replica plan"
            );
        }
        for record in appended["records"].as_array().unwrap() {
            tape.records.push(Record::Block(
                crate::tape_image_vectors::appended_record_bytes(record, &vector.image),
            ));
        }
        if appended["trailing_filemark"] == true {
            tape.records.push(Record::Filemark);
        }
    }
    for resolved in &edits {
        let file = &vector.image.files[resolved["tape_file"].as_u64().unwrap() as usize];
        assert_eq!(
            resolved["lba"].as_u64().unwrap() as usize,
            file.start_record + resolved["record_index"].as_u64().unwrap() as usize
                - usize::from(removed.is_some_and(|r| resolved["tape_file"].as_u64().unwrap() > r)),
            "record edit address"
        );
    }
    let lbas: Vec<u64> = faults["unreadable_records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["lba"].as_u64().unwrap())
        .collect();
    let engine = FaultEngine::for_read_medium_errors(lbas).unwrap();
    let mut world = VirtualWorld::single_drive("IMAGE-LIB", 0x100, "IMAGE-DRV", 0x400, 1);
    world.put_tape_in_drive(0x100, "IMAGE001", None, tape);
    let model = OverlengthReads {
        inner: ModelTransport::new(
            Arc::new(Mutex::new(world)),
            DeviceRole::Drive { bay: 0x100 },
        ),
        scratch: Vec::new(),
    };
    let transport = ChaosTransport::new(
        model,
        engine.clone(),
        DeviceCtx::new().with_backend("model"),
    );
    (
        DriveHandle::open_standalone_with_transport(
            Path::new("/dev/sg-image-model"),
            Box::new(transport),
        )
        .unwrap(),
        engine,
    )
}

fn scheme(k: u64, m: u64, s: u64) -> ParityScheme {
    let mut scheme = default_scheme_for_block_size(BLOCK);
    scheme.data_blocks_per_stripe = k.try_into().unwrap();
    scheme.parity_blocks_per_stripe = m.try_into().unwrap();
    scheme.stripes_per_neighborhood = s.try_into().unwrap();
    scheme.validate().unwrap();
    scheme
}

fn parity_error(error: ParityError) -> Value {
    match error {
        ParityError::Unrecoverable {
            stripe,
            lost_count,
            limit,
        } => {
            json!({"error":"Unrecoverable", "stripe":stripe.stripe_index, "lost":lost_count, "limit":limit})
        }
        ParityError::FilemarkMapReconstruct(_) => json!({"error":"FilemarkMapReconstruct"}),
        ParityError::ParityMapParse(_) => json!({"error":"ParityMapParse"}),
        ParityError::DirectoryInvalid(_) => json!({"error":"DirectoryInvalid"}),
        ParityError::NoBootstrapFound => json!({"error":"NoBootstrapFound"}),
        // Section 15's names, applied after every continuation decision.
        ParityError::BootstrapRefused { field, .. } => {
            json!({"error":"BootstrapParse", "field":field.section_8_4_name()})
        }
        ParityError::DriveCompressionEnabled { .. } => json!({"error":"DriveCompressionEnabled"}),
        ParityError::SchemeMismatch { .. } => json!({"error":"SchemeMismatch"}),
        ParityError::SidecarMetadataUnavailable { epoch_id } => {
            json!({"error":"SidecarMetadataUnavailable", "epoch":epoch_id})
        }
        error => json!({"error":format!("{error:?}")}),
    }
}

fn map_entry(row: TapeIndexReplicaMapEntry) -> TapeFileMapEntry {
    TapeFileMapEntry {
        tape_file_number: row.tape_file_number,
        block_count: row.block_count,
        kind: match row.kind {
            TapeIndexReplicaFileKind::Bootstrap => TapeFileKind::Bootstrap,
            TapeIndexReplicaFileKind::Object => TapeFileKind::Object,
            TapeIndexReplicaFileKind::ParitySidecar => TapeFileKind::ParitySidecar,
            TapeIndexReplicaFileKind::ParityMap => TapeFileKind::ParityMap,
            TapeIndexReplicaFileKind::TapeIndexReplica => TapeFileKind::TapeIndexReplica,
            TapeIndexReplicaFileKind::IndexSeparationExtent => TapeFileKind::IndexSeparationExtent,
        },
        first_parity_data_ordinal: row.first_parity_data_ordinal,
        protected_ordinal_start: row.protected_ordinal_start,
        protected_ordinal_end_exclusive: row.protected_ordinal_end_exclusive,
        epoch_id: row.epoch_id,
    }
}

/// What a BOT walk found beyond the classification of each tape file: how many
/// tape files it measured, whether it ended in a torn tail file, and which tape
/// files carry a damaged region (the file whose head the region starts at).
fn walk_extras(scan: &ScanWalkResult) -> serde_json::Map<String, Value> {
    let starts: Vec<(u64, u64)> = scan
        .map
        .entries()
        .iter()
        .filter_map(|entry| {
            scan.map
                .physical_position(TapeFilePosition {
                    tape_file_number: entry.tape_file_number,
                    block_within_file: 0,
                })
                .ok()
                .map(|position| (entry.tape_file_number, position.lba))
        })
        .collect();
    let mut extras = serde_json::Map::new();
    extras.insert(
        "walk_bootstrap_candidates".into(),
        json!(scan.bootstrap_candidates.len()),
    );
    extras.insert(
        "walk_object_blocks".into(),
        json!(scan
            .map
            .entries()
            .iter()
            .filter(|e| e.kind == TapeFileKind::Object)
            .map(|e| e.block_count)
            .collect::<Vec<_>>()),
    );

    extras.insert(
        "walk_tape_file_count".into(),
        json!(scan.map.tape_file_count()),
    );
    extras.insert("walk_truncated".into(), json!(scan.truncation.is_some()));
    extras.insert(
        "walk_damaged".into(),
        json!(scan
            .damaged_regions
            .iter()
            .map(|region| json!({
                "tape_file": starts.iter().find(|(_, lba)| *lba == region.start.lba).map(|(file, _)| file),
                "kind": format!("{:?}", region.kind),
            }))
            .collect::<Vec<_>>()),
    );
    extras
}

/// Decode only the explicitly supplied values, shared by both walk paths.
fn recovery_hints(vector: &VectorImage, hints: &Value) -> Option<ScanRecoveryHints> {
    check_hints(hints);
    if hints.is_object() {
        let h = hints;
        Some(ScanRecoveryHints {
            tape_uuid: vector.written.inputs.tape_uuid,
            block_size: h["block_size"].as_u64().unwrap() as u32,
            scheme: ParityConfig::Scheme(scheme(
                h["scheme"]["k"].as_u64().unwrap(),
                h["scheme"]["m"].as_u64().unwrap(),
                h["scheme"]["S"].as_u64().unwrap(),
            )),
        })
    } else {
        None
    }
}

/// Reader inputs come only from discovered bootstrap bytes or declared hints.
fn execute_reader(vector: &VectorImage, faults: &Value, hints: &Value) -> Value {
    let (mut drive, engine) = source(vector, faults);
    let mut raw = DriveHandleRawSource::new(&mut drive);
    let hints = recovery_hints(vector, hints);
    let mode = hints
        .as_ref()
        .map_or(ScanMode::Standard, ScanMode::Recovery);
    let mut candidates = DEFAULT_BOOTSTRAP_CANDIDATE_BLOCK_SIZES.to_vec();
    if let Some(hints) = &hints {
        candidates.push(hints.block_size);
    }
    // Section 15 names are rendered only after the continuation decision.
    let (bootstrap, discovery) =
        match discover_bootstrap_with_recovery_hints(&mut raw, &candidates, hints.as_ref()) {
            Ok(b) => (Some(b), "uses the bootstrap"),
            Err(ParityError::NoBootstrapFound | ParityError::BootstrapParse(_))
                if hints.is_some() =>
            {
                (None, "continues on the supplied values")
            }
            Err(e) => {
                let discovery = match &e {
                    ParityError::BootstrapRefused { .. }
                    | ParityError::DriveCompressionEnabled { .. } => "refused",
                    ParityError::NoBootstrapFound => "no bootstrap found",
                    _ => "failed",
                };
                let mut out = parity_error(e);
                out["discovery"] = json!(discovery);
                return out;
            }
        };
    let (uuid, block_size, scheme) = if let Some(b) = &bootstrap {
        let s = b
            .scheme
            .as_ref()
            .expect("parity fixture bootstrap must declare scheme");
        let parsed = ParityScheme {
            id: SchemeId::new_owned(s.id.clone()),
            data_blocks_per_stripe: s.data_blocks_per_stripe,
            parity_blocks_per_stripe: s.parity_blocks_per_stripe,
            stripes_per_neighborhood: s.stripes_per_neighborhood,
        };
        parsed.validate().unwrap();
        (b.tape_uuid, b.block_size_bytes, parsed)
    } else {
        let h = hints.as_ref().unwrap();
        (
            h.tape_uuid,
            h.block_size,
            match &h.scheme {
                ParityConfig::Scheme(s) => s.clone(),
                _ => panic!("parity fixture requires scheme"),
            },
        )
    };
    let mut out = json!({"records": raw.locate_end_of_data().unwrap().lba, "discovery": discovery});
    let mut entries = Vec::new();
    let mut raw_rows = Vec::new();
    let mut objects = Vec::new();
    let mut attempt = None;
    let inventory = read_terminal_index_inventory_streamed(&mut raw, &uuid, block_size, |event| {
        match event {
            TerminalInventoryStreamEvent::ReplicaAttemptStarted { attempt_id, .. } => {
                raw_rows.clear();
                entries.clear();
                objects.clear();
                attempt = Some(attempt_id);
            }
            TerminalInventoryStreamEvent::StructuralEntry {
                attempt_id, entry, ..
            } => {
                assert_eq!(attempt, Some(attempt_id));
                raw_rows.push(entry.clone());
                entries.push(map_entry(entry));
            }
            TerminalInventoryStreamEvent::ObjectRow {
                attempt_id, row, ..
            } => {
                assert_eq!(attempt, Some(attempt_id));
                objects.push(row);
            }
            TerminalInventoryStreamEvent::ReplicaAttemptRejected { .. } => {
                raw_rows.clear();
                entries.clear();
                objects.clear();
                attempt = None;
            }
        }
        Ok(())
    });
    let scoped = match inventory {
        Err(TerminalInventoryReadError::TerminalIndexReplicaConflict { .. }) => {
            out["error"] = json!("TerminalIndexReplicaConflict");
            return out;
        }
        Err(e) => {
            out["error"] = json!(format!("{e:?}"));
            return out;
        }
        Ok(TerminalInventoryOutcome::Inventory(selection)) => {
            assert_eq!(attempt, Some(selection.selected_attempt_id));
            out["inventory"] = json!(true);
            out["terminal_authority_recovered"] = json!(true);
            out["selected_replica"] = json!(selection.selected_replica_ordinal);
            out["degraded"] = json!(selection.is_degraded());
            out["replicas"] = json!(selection
                .replicas
                .iter()
                .map(|r| match r {
                    TerminalReplicaEvidence::Valid { .. } => "Valid".to_string(),
                    TerminalReplicaEvidence::ConsistentEnvelope => "ConsistentEnvelope".to_string(),
                    TerminalReplicaEvidence::Invalid(failure) => format!("Invalid: {failure:?}"),
                })
                .collect::<Vec<_>>());
            let map = FilemarkMap::new(entries).unwrap();
            // §8.3: replica payloads describe the prefix before A, excluding
            // the terminal suffix present in the writer's completed map.
            let original_scope = &vector
                .written
                .terminal_plan
                .as_ref()
                .unwrap()
                .edition
                .descriptor
                .scope;
            let replica_a_file = vector
                .image
                .files
                .iter()
                .filter(|file| file.filemark_record.is_some())
                .count()
                .checked_sub(5)
                .expect("Inventory requires the five-file terminal suffix");
            let (mut undamaged, _) = source(vector, &json!({"unreadable_records": []}));
            let image_map = scan_reconstruct_filemark_map_with_report(
                &mut DriveHandleRawSource::new(&mut undamaged),
                &uuid,
                block_size,
            )
            .unwrap();
            let image_object_count = image_map
                .map
                .entries()
                .iter()
                .filter(|entry| entry.kind == TapeFileKind::Object)
                .count();
            out["replica_a_file_number"] = json!(replica_a_file);
            out["inventory_structural_rows"] = json!(map.entries().len());
            out["inventory_object_rows"] = json!(objects.len());
            out["image_object_count"] = json!(image_object_count);
            let original_prefix = vector
                .written
                .map
                .truncate_to_tape_files(replica_a_file as u64)
                .unwrap();
            out["inventory_unaffected"] =
                json!(map == original_prefix && objects == vector.written.object_rows);
            out["inventory_prefix_consistent"] = json!(
                original_scope.covered_prefix_tape_file_count == replica_a_file as u64
                    && map.entries().len() == replica_a_file
                    && objects.len() == image_object_count
                    && out["inventory_unaffected"] == true
            );
            assert_eq!(
                out["inventory_prefix_consistent"], true,
                "inventory prefix disagreement: {out}"
            );
            // The selected replica's rows: the only way to a map that REM-PARITY
            // 13.3's tail rescue from the terminal index applies to.
            scoped_map_from_terminal_replica(&selection.edition, &raw_rows).unwrap()
        }
        Ok(TerminalInventoryOutcome::BotStructuralRecoveryRequired(required)) => {
            out["outcome"] = json!("BotStructuralRecoveryRequired");
            out["terminal_authority_recovered"] = json!(false);
            out["every_replica_invalid"] = json!(required
                .replicas
                .iter()
                .all(|r| matches!(r, TerminalReplicaEvidence::Invalid(_))));
            let scan = match scan_reconstruct_filemark_map_with_report_mode(
                &mut raw, &uuid, block_size, mode,
            ) {
                Ok(scan) => scan,
                Err(e) => {
                    out["error"] = parity_error(e)["error"].clone();
                    return out;
                }
            };
            out["walk_classes"] = json!(scan
                .map
                .entries()
                .iter()
                .map(|e| (e.tape_file_number.to_string(), format!("{:?}", e.kind)))
                .collect::<BTreeMap<_, _>>());
            out["walk_object_blocks"] = json!(scan
                .map
                .entries()
                .iter()
                .filter(|e| e.kind == TapeFileKind::Object)
                .map(|e| e.block_count)
                .collect::<Vec<_>>());
            out.as_object_mut().unwrap().extend(walk_extras(&scan));
            let mut bot_objects = Vec::new();
            match recover_terminal_inventory_from_bot_controlled_mode(
                &mut raw,
                &uuid,
                block_size,
                mode,
                |_| ScanWalkControl::Continue,
                |o| {
                    bot_objects.push(o.clone());
                    Ok(())
                },
            ) {
                Ok(_) => {
                    out["object_identity"] = json!(if !bot_objects.is_empty()
                        && bot_objects
                            .iter()
                            .all(|o| o.state == BotRecoveredObjectState::Unknown
                                && o.object_id.is_none())
                    {
                        "unknown"
                    } else {
                        "other"
                    });
                }
                Err(e) => {
                    out["error"] = json!(format!("{e:?}"));
                    return out;
                }
            }
            let Some(bootstrap) = bootstrap.as_ref() else {
                // §8.4.1 continues structural recovery with supplied UUID,
                // block size and scheme when the bootstrap is unreadable.
                // No authenticated bootstrap is required for that walk, so
                // NoBootstrapFound would contradict the supplied-values path.
                observe_parity_map(&mut raw, &scan, &uuid, block_size, &mut out);
                return out;
            };
            match validate_scan_reconstruction_with_report(&mut raw, bootstrap, scan) {
                Ok(validated) => {
                    out["walked_map_validated"] =
                        json!(validated.scoped_map.sidecar_directory.is_some());
                    out["walk_validated_prefix"] =
                        json!(validated.scoped_map.validated_prefix_tape_files);
                    out["walk_watermark"] = json!(validated.scoped_map.scope.watermark());
                    validated.scoped_map
                }
                Err(e) => {
                    out["walk_validation_error"] = json!(format!("{e:?}"));
                    return out;
                }
            }
        }
    };
    let addresses = faults["failed_data_addresses"].as_array().unwrap();
    let mut recovered = BTreeMap::new();
    let mut losses = BTreeMap::new();
    let mut all_recovered = !addresses.is_empty();
    for address in addresses {
        let file = address[0].as_u64().unwrap();
        let block = address[1].as_u64().unwrap();
        let ordinal = scoped.map.entries()[file as usize]
            .first_parity_data_ordinal
            .unwrap()
            + block;
        raw.locate_physical(PhysicalPositionHint::new(
            vector.image.files[file as usize].start_record as u64 + block,
        ))
        .unwrap();
        assert!(
            matches!(raw.read_record(&mut vec![0; block_size as usize]), Err(ParityError::TapeIo(ref e)) if tape_error_is_current_medium_damage(e)),
            "failed-data subject must report a real medium error"
        );
        let epoch = ordinal
            / (u64::from(scheme.data_blocks_per_stripe)
                * u64::from(scheme.stripes_per_neighborhood));
        match recover_ordinal_from_sidecar(&mut raw, &scoped, &scheme, uuid, block_size, ordinal) {
            Ok(result) => {
                out[format!("epoch_{epoch}")] = json!("recovered");
                out[format!("epoch_{epoch}_copy_health")] =
                    json!(format!("{:?}", result.sidecar_metadata_health));
                losses.insert(result.stripe.stripe_index, result.lost_shards.len());
                recovered.insert((file, block), result.recovered_block);
            }
            Err(e) => {
                all_recovered = false;
                let error = parity_error(e);
                out[format!("epoch_{epoch}")] = error["error"].clone();
                for (key, value) in error.as_object().unwrap() {
                    out[key] = value.clone();
                }
            }
        }
    }
    if !addresses.is_empty() {
        out["recovered"] = json!(all_recovered);
        out["losses_per_stripe"] = json!(losses.values().collect::<Vec<_>>());
        // Re-read every Object block from the damaged source, substituting only
        // recovered bytes. Hashing untouched healthy fixture bytes would hide gaps.
        let mut hashes_match = all_recovered;
        for file in addresses
            .iter()
            .map(|a| a[0].as_u64().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
        {
            let original = &vector.image.files[file as usize];
            let mut digest = Sha256::new();
            for block in 0..original.record_offsets.len() as u64 {
                if let Some(bytes) = recovered.get(&(file, block)) {
                    digest.update(bytes);
                } else {
                    raw.locate_physical(PhysicalPositionHint::new(
                        original.start_record as u64 + block,
                    ))
                    .unwrap();
                    let mut bytes = vec![0; block_size as usize];
                    match raw.read_record(&mut bytes) {
                        Ok(RawReadOutcome::Block { .. }) => digest.update(bytes),
                        _ => hashes_match = false,
                    }
                }
            }
            hashes_match &= digest.finalize() == Sha256::digest(&original.bytes);
        }
        out["object_sha256_matches"] = json!(hashes_match);
    }
    if let Some(reads) = faults.get("read_data_addresses") {
        read_objects(
            vector, &mut raw, &scoped, &scheme, uuid, block_size, reads, &mut out,
        );
    }
    out["observed_medium_error_lbas"] = json!(engine.observed_medium_error_lbas());
    out
}

/// Records every recovery attempt the object source reports.
#[derive(Default)]
struct RecoveryLog(Mutex<Vec<RecoveryEvent>>);
impl ParityAuditHook for RecoveryLog {
    fn on_recovery(&self, event: &RecoveryEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

/// Read Object addresses through the production object source, the Reader
/// that meets a record fault itself (REM-PARITY 3.5, 13.4). An address counts
/// as recovered only when the source released the original block and reported
/// reconstructing it from parity: a truncated over-length record would also
/// release the original bytes, and must not count.
#[allow(clippy::too_many_arguments)]
fn read_objects(
    vector: &VectorImage,
    raw: &mut DriveHandleRawSource<'_>,
    scoped: &ScopedFilemarkMap,
    scheme: &ParityScheme,
    uuid: [u8; 16],
    block_size: u32,
    reads: &Value,
    out: &mut Value,
) {
    let mut all = true;
    let mut results = serde_json::Map::new();
    for address in reads.as_array().unwrap() {
        let file = address[0].as_u64().unwrap();
        let block = address[1].as_u64().unwrap();
        let log = Arc::new(RecoveryLog::default());
        let original = &vector.image.files[file as usize];
        let offset = original.record_offsets[block as usize];
        let expected = &original.bytes[offset..offset + block_size as usize];
        let result = (|| -> Result<Vec<u8>, TapeIoError> {
            let mut source = ObjectParitySource::open(
                raw,
                scheme.clone(),
                uuid,
                scoped.clone(),
                block_size,
                file,
                OpenTrust::RequireValidated,
            )
            .map_err(|e| TapeIoError::OperationFailed(e.to_string()))?;
            source.set_audit_hook(Some(log.clone()));
            BlockSource::locate(&mut source, block)?;
            let mut buf = vec![0; block_size as usize];
            let n = source.read_block(&mut buf)?;
            buf.truncate(n);
            Ok(buf)
        })();
        let events = log.0.lock().unwrap().clone();
        let recovered_by_parity = events.iter().any(|e| {
            e.at_requested == (file, block) && matches!(e.outcome, RecoveryOutcome::Recovered)
        });
        let entry = match result {
            Ok(bytes) => {
                let returned_original = bytes == expected;
                all &= returned_original && recovered_by_parity;
                json!({"returned_original": returned_original, "recovered_by_parity": recovered_by_parity,
                    "lost_count": events.iter().find(|e| e.at_requested == (file, block)).map(|e| e.lost_blocks.len())})
            }
            Err(error) => {
                all = false;
                out["read_error"] = json!(format!("{error:?}"));
                json!({"error": format!("{error:?}")})
            }
        };
        results.insert(format!("({file}, {block})"), entry);
    }
    out["read"] = Value::Object(results);
    out["read_recovered"] = json!(all);
}

/// Verification is an independent fixture check, including when discovery or
/// recovery returns early. Its required identity comes from the image inputs;
/// it never supplies that identity to the discovery/recovery reader.
fn execute(vector: &VectorImage, faults: &Value, hints: &Value) -> Value {
    let mut out = execute_reader(vector, faults, hints);
    let (mut drive, _) = source(vector, faults);
    let mut raw = DriveHandleRawSource::new(&mut drive);
    let uuid = vector.written.inputs.tape_uuid;
    let block_size = vector.written.inputs.block_size;
    match verify_terminal_index_full(&mut raw, &uuid, block_size) {
        Ok(result) => {
            out["verifier_suffix_complete"] = json!(matches!(
                result,
                TerminalIndexVerificationOutcome::VerifiedComplete(_)
            ));
            out["verifier_reports_separation"] = json!(match &result {
                TerminalIndexVerificationOutcome::VerifiedComplete(v)
                | TerminalIndexVerificationOutcome::VerifiedDegraded(v) => v
                    .separations
                    .iter()
                    .any(|s| matches!(s, TerminalSeparationEvidence::Invalid { .. })),
                _ => false,
            });
            out["verifier_separations"] = json!(match &result {
                TerminalIndexVerificationOutcome::VerifiedComplete(v)
                | TerminalIndexVerificationOutcome::VerifiedDegraded(v) =>
                    format!("{:?}", v.separations),
                TerminalIndexVerificationOutcome::RecoveryRequired(v) => v.detail.clone(),
            });
        }
        Err(e) => {
            out["verifier_error"] = json!(format!("{e:?}"));
        }
    }
    out
}

/// Pin regenerated byte streams to the on-disk manifest, including torn tails.
fn check_manifest(name: &str, vector: &VectorImage, path: &Path) {
    let manifest = std::fs::read_to_string(path).expect("missing image manifest");
    let mut lines = manifest.lines();
    assert_eq!(
        lines.next().unwrap(),
        "image\ttape_file\tstart_record\tdata_records\tbytes\tsha256\tfilemark_record\teod_record"
    );
    let mut expected = BTreeMap::new();
    for line in lines {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 8, "malformed manifest row: {line}");
        if fields[0] == name {
            assert!(
                expected
                    .insert(
                        fields[1].to_string(),
                        (fields[4].parse::<usize>().unwrap(), fields[5].to_string())
                    )
                    .is_none(),
                "duplicate manifest row: {line}"
            );
        }
    }
    let mut actual = BTreeMap::new();
    let mut whole = Sha256::new();
    let mut size = 0;
    for (index, file) in vector.image.files.iter().enumerate() {
        whole.update(&file.bytes);
        size += file.bytes.len();
        actual.insert(
            index.to_string(),
            (file.bytes.len(), hex(&Sha256::digest(&file.bytes))),
        );
    }
    actual.insert("ALL".to_string(), (size, hex(&whole.finalize())));
    assert_eq!(
        actual, expected,
        "{name}: image size/SHA-256 differs from MANIFEST.tsv"
    );
}

/// Interpret prose labels from the frozen file; copy-selection annotations are
/// informative per D3. Unknown fields fail rather than silently losing coverage.
fn compare(expected: &Value, actual: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    for (key, value) in expected.as_object().unwrap() {
        let agrees = match key.as_str() {
            "note" | "informative" => continue,
            "inventory" => {
                let selected = actual["selected_replica"].as_u64();
                actual["inventory"] == true
                    && actual["inventory_unaffected"] == true
                    && match value.as_str().unwrap() {
                        "replica C" => selected == Some(3),
                        "replica A accepted" => selected == Some(1),
                        "the agreeing survivors A and C" => {
                            selected == Some(1) || selected == Some(3)
                        }
                        "an agreeing valid replica" | "as the image's expected.json" => {
                            actual["inventory_unaffected"] == true
                        }
                        other => panic!("unhandled inventory expectation {other}"),
                    }
            }
            "walk_classes" => value.as_object().unwrap().iter().all(|(file, class)| {
                if file == "4-8" {
                    (4..=8).all(|i| {
                        matches!(
                            actual["walk_classes"][i.to_string()].as_str(),
                            Some("TapeIndexReplica" | "IndexSeparationExtent")
                        )
                    })
                } else {
                    actual["walk_classes"][file] == *class
                }
            }),
            "walk" => {
                assert_eq!(
                    value,
                    "an 11-block Object candidate where the Object and the sidecar merged"
                );
                actual["walk_object_blocks"] == json!([11])
            }
            k if k.starts_with("epoch_")
                && value.as_str().is_some_and(|v| v.starts_with("recovered")) =>
            {
                actual[key] == "recovered"
            }
            _ => actual.get(key) == Some(value),
        };
        if !agrees {
            failures.push(format!(
                "{key}: expected {value}, observed {}",
                actual.get(key).unwrap_or(&Value::Null)
            ));
        }
    }
    failures
}

/// The observation vocabulary of the E1 expectations: REM-PARITY 8.4's four
/// discovery outcomes and the field names of 8.4, as the renderer prints them.
const DISCOVERY_OUTCOMES: [&str; 4] = [
    "continues on the supplied values",
    "uses the bootstrap",
    "refused",
    "no bootstrap found",
];
const REFUSED_FIELDS: [BootstrapRefusedField; 6] = [
    BootstrapRefusedField::FormatMajor,
    BootstrapRefusedField::TapeUuid,
    BootstrapRefusedField::BlockSize,
    BootstrapRefusedField::Sequence,
    BootstrapRefusedField::NoParityFlag,
    BootstrapRefusedField::Scheme,
];

/// An observation case's expectation names exactly the fault map's
/// observations, so no misspelt or missing name goes uncompared.
fn check_observation_names(id: &str, expected: &Value, observations: &[Value]) {
    let expected = expected
        .as_object()
        .unwrap_or_else(|| panic!("{id}: expectation missing or pending: {expected}"));
    let named: std::collections::BTreeSet<_> = expected.keys().map(String::as_str).collect();
    let observed: std::collections::BTreeSet<_> = observations
        .iter()
        .map(|observation| observation["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        named, observed,
        "{id}: the expectation's observation names differ from the fault map's"
    );
}

/// Validate one observation's expectation and project the compared outcome.
/// A pending expectation, an unknown key or an unknown value fails the case;
/// `sections`, `quotes` and `ambiguity` are the author's grounds, not compared.
fn expected_observation(id: &str, name: &str, expected: &Value) -> Value {
    let Some(fields) = expected.as_object() else {
        panic!("{id} [{name}]: expectation missing or pending: {expected}")
    };
    for key in fields.keys() {
        assert!(
            matches!(
                key.as_str(),
                "discovery" | "error" | "field" | "inventory" | "sections" | "quotes" | "ambiguity"
            ),
            "{id} [{name}]: unknown expectation key {key}"
        );
    }
    let discovery = expected["discovery"].as_str().unwrap_or_default();
    assert!(
        DISCOVERY_OUTCOMES.contains(&discovery),
        "{id} [{name}]: unknown discovery outcome {}",
        expected["discovery"]
    );
    assert!(
        expected["error"].is_string(),
        "{id} [{name}]: error must be a Section 15 name or none"
    );
    assert!(
        expected["field"].is_null()
            || REFUSED_FIELDS
                .iter()
                .any(|field| expected["field"] == field.section_8_4_name()),
        "{id} [{name}]: unknown field {}",
        expected["field"]
    );
    let inventory = &expected["inventory"];
    let returned = inventory["returned"]
        .as_bool()
        .unwrap_or_else(|| panic!("{id} [{name}]: inventory.returned must be a bool"));
    assert!(
        if returned {
            inventory["degraded"].is_boolean()
        } else {
            inventory["degraded"] == "not applicable"
        },
        "{id} [{name}]: inventory.degraded {} does not fit returned={returned}",
        inventory["degraded"]
    );
    assert_eq!(inventory.as_object().map(|o| o.len()), Some(2));
    json!({"discovery": discovery, "error": expected["error"], "field": expected["field"],
        "inventory": {"returned": returned, "degraded": inventory["degraded"]}})
}

/// Project an executor output onto the same vocabulary: no `error` key is
/// `none`, no `field` is null, and an inventory not returned has no degraded state.
fn observed_outcome(actual: &Value) -> Value {
    let returned = actual["inventory"] == true;
    json!({"discovery": actual["discovery"], "error": actual.get("error").cloned().unwrap_or(json!("none")),
        "field": actual["field"],
        "inventory": {"returned": returned, "degraded": if returned { actual["degraded"].clone() } else { json!("not applicable") }}})
}

/// Every compared part must equal the expectation. A returned inventory must
/// also be the image's own, since every replica of the image is intact.
fn compare_observation(wanted: &Value, observed: &Value, actual: &Value) -> Vec<String> {
    let mut failures: Vec<String> = ["discovery", "error", "field", "inventory"]
        .into_iter()
        .filter(|key| wanted[*key] != observed[*key])
        .map(|key| {
            format!(
                "{key}: expected {}, observed {}",
                wanted[key], observed[key]
            )
        })
        .collect();
    if observed["inventory"]["returned"] == true && actual["inventory_unaffected"] != true {
        failures.push("inventory: returned, but not the image's own".to_string());
    }
    failures
}

fn run(id: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/rem-parity-terminal-index-draft/tape-images/cases")
        .join(id);
    let case: Value = serde_json::from_slice(
        &std::fs::read(root.join("expected.json")).expect("missing expected fixture"),
    )
    .unwrap();
    let frozen: Value = serde_json::from_str(EXPECTATIONS).unwrap();
    assert_eq!(
        &case,
        frozen["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap(),
        "case differs from frozen source"
    );
    let faults: Value = serde_json::from_slice(
        &std::fs::read(root.join("fault-map.json")).expect("missing fault fixture"),
    )
    .unwrap();
    let vector = generate(case["image"].as_str().unwrap()).unwrap();
    check_manifest(
        case["image"].as_str().unwrap(),
        &vector,
        &root
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("MANIFEST.tsv"),
    );
    let pinned = case["pinned"].as_bool().unwrap();
    if let Some(observations) = faults.get("observations") {
        // Each observation is a separate reader run with its own supplied values
        // and its own expectation; a pending or unknown expectation fails.
        check_observations(observations);
        let observations = observations.as_array().unwrap();
        check_observation_names(id, &case["expected"], observations);
        let mut failures = Vec::new();
        for observation in observations {
            let name = observation["id"].as_str().unwrap();
            let wanted = expected_observation(id, name, &case["expected"][name]);
            let actual = execute(&vector, &faults, &observation["hints"]);
            let observed = observed_outcome(&actual);
            let disagreements = compare_observation(&wanted, &observed, &actual);
            println!(
                "CASE {id} {} observation={name:?} sections={} expected={wanted} observed={observed} outcome={actual}",
                if !pinned {
                    "INFORMATIVE"
                } else if disagreements.is_empty() {
                    "PASS"
                } else {
                    "DISAGREEMENT"
                },
                case["expected"][name]["sections"]
            );
            if pinned {
                failures.extend(disagreements.into_iter().map(|d| format!("[{name}] {d}")));
            }
        }
        assert!(failures.is_empty(), "{id}: {}", failures.join("; "));
        return;
    }
    let mut actual = execute(&vector, &faults, &faults["hints"]);
    if case["erratum"] == "E3" || case["erratum"] == "S6" {
        // The walk is an observation of its own: it runs whatever the Scanner
        // returned.
        actual["walk"] = walk_observation(&vector, &faults);
    }
    if case["erratum"] == "S6" {
        println!("S6 {id} expected={}", case["checks"]);
    }
    let failures = if case["erratum"] == "E2" {
        compare_e2(id, &case["expected"], &actual)
    } else if case["erratum"] == "E3" {
        compare_e3(id, &case["expected"], &actual)
    } else if case["erratum"] == "S6" {
        compare_s6(id, &case["expected"], &case["checks"], &actual)
    } else if case["erratum"] == "E4" {
        compare_e4(id, &case["expected"], &actual)
    } else if pinned {
        compare(&case["expected"], &actual)
    } else {
        Vec::new()
    };
    println!(
        "CASE {id} {} sections={} outcome={actual}",
        if !pinned {
            "INFORMATIVE"
        } else if failures.is_empty() {
            "PASS"
        } else {
            "DISAGREEMENT"
        },
        case["sections"]
    );
    // An unpinned case does not decide a pass. The erratum-set overlays are
    // pinned by their authors and always decide.
    let mut failures = if pinned { failures } else { Vec::new() };
    failures.extend(compare_e4_overlay(id, &vector, &faults, &actual));
    if !failures.is_empty() {
        assert!(
            failures.is_empty(),
            "{id}: specification sections {}: {}",
            case["sections"],
            failures.join("; ")
        );
    }
}

/// The walk classes the E2 author's phrases name: a phrase that starts with a
/// structural kind names that kind; one that describes an Object candidate by
/// elimination names Object. Any other phrase fails the case.
fn e2_walk_class(phrase: &str) -> &'static str {
    for kind in [
        "Bootstrap",
        "ParitySidecar",
        "ParityMap",
        "IndexSeparationExtent",
        "TapeIndexReplica",
    ] {
        if phrase.starts_with(kind) {
            return kind;
        }
    }
    // A phrase that states the ladder's classification and then that a second
    // pass identifies the file as a sidecar names the map's final class.
    if phrase.starts_with("ladder:") && phrase.contains("identified as ParitySidecar") {
        return "ParitySidecar";
    }
    if phrase.contains("Object candidate") {
        return "Object";
    }
    panic!("E2 classification phrase names no walk class: {phrase}")
}

/// Compare an E2 author's outcome with the executor's output. The author's
/// entry is verbatim; the compared parts are its error and the leading clause
/// of its result, through a closed vocabulary, and the component,
/// classification and map notes where it gives them. An unknown key, clause or
/// phrase fails the case rather than going uncompared.
fn compare_e2(id: &str, expected: &Value, actual: &Value) -> Vec<String> {
    for key in expected.as_object().expect("E2 expectation object").keys() {
        assert!(
            matches!(
                key.as_str(),
                "result"
                    | "error"
                    | "sections"
                    | "quotes"
                    | "component"
                    | "classification"
                    | "map"
                    | "open"
            ),
            "{id}: unknown E2 expectation key {key}"
        );
    }
    let mut failures = Vec::new();
    let mut check = |holds: bool, what: String| {
        if !holds {
            failures.push(what);
        }
    };
    let no_error = actual.get("error").is_none() && actual.get("read_error").is_none();
    match &expected["error"] {
        Value::Null => check(no_error, "error: expected none".into()),
        Value::String(name) if name == "BotStructuralRecoveryRequired" => check(
            actual["outcome"] == "BotStructuralRecoveryRequired" && no_error,
            "error: expected the explicit BotStructuralRecoveryRequired outcome and no failure"
                .into(),
        ),
        Value::String(name) if name == "SidecarMetadataUnavailable{epoch_id: 0}" => check(
            actual["epoch_0"] == "SidecarMetadataUnavailable" && actual["epoch"] == 0,
            "error: expected SidecarMetadataUnavailable{epoch_id: 0}".into(),
        ),
        other => panic!("{id}: no E2 vocabulary for error {other}"),
    }
    let result = expected["result"].as_str().expect("E2 result");
    if result.starts_with("recovered, using the primary copy.") {
        // The Recoverer recovered every failed block, with the primary copy in
        // use: it did not fall back to the tail copy.
        check(
            actual["recovered"] == true
                && actual["object_sha256_matches"] == true
                && actual["epoch_0_copy_health"] != "PrimaryHeaderLost",
            "result: expected every failed block recovered with the primary copy in use".into(),
        );
    } else if result.starts_with("SidecarMetadataUnavailable for epoch 0; epoch 1 is unaffected.")
    {
        check(
            actual["epoch_0"] == "SidecarMetadataUnavailable" && actual["epoch"] == 0,
            "result: expected SidecarMetadataUnavailable for epoch 0".into(),
        );
    } else if result.starts_with(
        "BotStructuralRecoveryRequired. The walk produces a map that is validated against the final ParityMap, with tape file 2 identified as a sidecar by the second pass, not by the ladder. Terminal authority is reported as not recovered.",
    ) {
        check(
            actual["walked_map_validated"] == true
                && actual["terminal_authority_recovered"] == false,
            "result: expected a walked map validated against the final ParityMap, with terminal authority not recovered".into(),
        );
    } else if result.starts_with("recovered.") {
        check(
            actual["read_recovered"] == true,
            "result: expected every read address recovered from parity and released as the original".into(),
        );
    } else if result.starts_with("inventory returned, degraded.") {
        check(
            actual["inventory"] == true
                && actual["inventory_unaffected"] == true
                && actual["degraded"] == true,
            "result: expected the image's own inventory, degraded".into(),
        );
    } else if result.starts_with(
        "BotStructuralRecoveryRequired, and the walk produces a map that is validated against the final ParityMap. Terminal authority is reported as not recovered.",
    ) {
        check(
            actual["walked_map_validated"] == true
                && actual["terminal_authority_recovered"] == false,
            "result: expected a walked map validated against the final ParityMap, with terminal authority not recovered".into(),
        );
    } else {
        panic!("{id}: no E2 vocabulary for result {result}");
    }
    if let Some(component) = expected.get("component") {
        for (who, what) in component.as_object().unwrap() {
            let evidence = |i: usize| {
                actual["replicas"][i]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            };
            match (who.as_str(), what.as_str().unwrap()) {
                ("replica C", "TerminalIndexReplicaParse (invalid, never TapeIo)") => check(
                    evidence(2).starts_with("Invalid"),
                    format!(
                        "component replica C: expected invalid, observed {}",
                        evidence(2)
                    ),
                ),
                ("replicas A, B", "valid, agreeing") => {
                    for i in [0, 1] {
                        check(
                            matches!(evidence(i).as_str(), "Valid" | "ConsistentEnvelope"),
                            format!(
                                "component replica {i}: expected valid, observed {}",
                                evidence(i)
                            ),
                        );
                    }
                }
                other => panic!("{id}: no E2 vocabulary for component {other:?}"),
            }
        }
    }
    if let Some(classification) = expected.get("classification") {
        let classification = classification.as_object().unwrap();
        check(
            actual["walk_classes"].as_object().map(|c| c.len()) == Some(classification.len()),
            format!(
                "classification: expected {} tape files, observed {}",
                classification.len(),
                actual["walk_classes"]
            ),
        );
        for (file, phrase) in classification {
            let class = e2_walk_class(phrase.as_str().unwrap());
            check(
                actual["walk_classes"][file] == class,
                format!(
                    "classification {file}: expected {class}, observed {}",
                    actual["walk_classes"][file]
                ),
            );
        }
    }
    if let Some(map) = expected.get("map") {
        assert!(
            map.as_str().unwrap().starts_with("produced."),
            "{id}: no E2 vocabulary for map {map}"
        );
        check(
            actual["walked_map_validated"] == true,
            "map: expected produced and validated".into(),
        );
    }
    failures
}

/// The E4 overlay: expectations of erratum set E4 for existing damage cases.
const E4_OVERLAY: &str =
    include_str!("../../../fixtures/rem-parity-terminal-index-draft/tape-images/expected-e4.json");

/// What a Verifier's full verification reported, projected onto the structure
/// an E4 author's entry states: the failed data blocks by tape-file position
/// and the failed parity shards by epoch, stripe and parity index, each with
/// its reason, the route by which the map was obtained, and whether the
/// terminal suffix and the tape are complete.
fn verification_projection(outcome: &TerminalIndexVerificationOutcome) -> Value {
    let reason = |reason: &BlockFailureReason| match reason {
        BlockFailureReason::Unreadable => "unreadable".to_string(),
        BlockFailureReason::CrcMismatch => "CRC mismatch".to_string(),
        other => format!("{other:?}"),
    };
    let protected = outcome.protected();
    json!({
        "route": match outcome {
            TerminalIndexVerificationOutcome::RecoveryRequired(_) => "BOT walk",
            _ => "terminal replicas",
        },
        "terminal_suffix_complete": outcome.is_terminal_suffix_complete(),
        "tape_complete": outcome.is_complete_tape(),
        "not_performed": protected.not_performed,
        "parity_map_findings": protected.parity_map_findings.iter().map(|f| json!({
            "tape_file": f.tape_file_number, "detail": f.detail})).collect::<Vec<_>>(),
        "failed_data_blocks": protected.failed_data_blocks().map(|b| json!({
            "tape_file_position": [b.position.tape_file_number, b.position.block_within_file],
            "lba": b.physical.lba, "reason": reason(&b.reason)})).collect::<Vec<_>>(),
        "failed_parity_shards": protected.failed_parity_shards().map(|p| json!({
            "epoch": p.epoch_id, "stripe": p.stripe_index, "parity_index": p.parity_index,
            "sidecar_block": p.sidecar_block, "lba": p.physical.lba,
            "reason": reason(&p.reason)})).collect::<Vec<_>>(),
        "sidecars": protected.sidecars.iter().map(|s| json!({
            "tape_file": s.tape_file_number, "epoch": s.epoch_id,
            "footer": format!("{:?}", s.footer), "primary": format!("{:?}", s.primary),
            "tail": format!("{:?}", s.tail), "copies_diverge": s.copies_diverge,
            "metadata_health": s.metadata_health.map(|h| format!("{h:?}")),
            "checked_against_index": s.checked_against_index,
            "findings": s.findings.iter().map(|f| json!({"kind": f.kind.section_15_name(), "detail": f.detail})).collect::<Vec<_>>()})).collect::<Vec<_>>(),
    })
}

/// A Verifier's full verification of the case's tape, with the supplied values
/// when the case gives them (the bootstrap is then unreadable).
fn verification_observation(vector: &VectorImage, faults: &Value) -> Value {
    let (mut drive, _) = source(vector, faults);
    let mut raw = DriveHandleRawSource::new(&mut drive);
    let uuid = vector.written.inputs.tape_uuid;
    let block_size = vector.written.inputs.block_size;
    let hints = faults["hints"].as_object().map(|h| ScanRecoveryHints {
        tape_uuid: uuid,
        block_size: h["block_size"].as_u64().unwrap() as u32,
        scheme: ParityConfig::Scheme(scheme(
            h["scheme"]["k"].as_u64().unwrap(),
            h["scheme"]["m"].as_u64().unwrap(),
            h["scheme"]["S"].as_u64().unwrap(),
        )),
    });
    let mode = match &hints {
        Some(hints) => ScanMode::Recovery(hints),
        None => ScanMode::Standard,
    };
    match verify_terminal_index_full_with_scan_mode(&mut raw, &uuid, block_size, None, mode) {
        Ok(outcome) => verification_projection(&outcome),
        Err(error) => json!({"error": format!("{error:?}")}),
    }
}

/// Compare an E4 author's full-verification entry with a Verifier's report.
/// The compared parts are the failed data blocks and parity shards (as sets,
/// by address and reason), the terminal suffix's completeness and the route by
/// which the map was obtained. The author's other findings, open questions and
/// derivations are informative, and are printed by the caller.
fn compare_verification(id: &str, entry: &Value, observed: &Value) -> Vec<String> {
    for key in entry.as_object().expect("E4 verification entry").keys() {
        assert!(
            matches!(
                key.as_str(),
                "image"
                    | "map_route"
                    | "failed_data_blocks"
                    | "failed_parity_shards"
                    | "unchecked"
                    | "other_findings"
                    | "terminal_suffix_complete"
                    | "tape_complete"
                    | "open"
                    | "sections"
                    | "quotes"
            ),
            "{id}: unknown E4 verification key {key}"
        );
    }
    let mut failures = Vec::new();
    if let Some(error) = observed.get("error") {
        return vec![format!("the Verifier failed: {error}")];
    }
    let route = entry["map_route"].as_str().unwrap();
    let expected_route = if route.starts_with("validated terminal replicas")
        || route.starts_with("bootstrap unreadable; discovery on the supplied values")
    {
        "terminal replicas"
    } else if route.starts_with("no replica validates; BOT walk") {
        "BOT walk"
    } else {
        panic!("{id}: no E4 vocabulary for map route {route}")
    };
    if observed["route"] != expected_route {
        failures.push(format!(
            "route: expected {expected_route}, observed {}",
            observed["route"]
        ));
    }
    if observed["terminal_suffix_complete"] != entry["terminal_suffix_complete"] {
        failures.push(format!(
            "terminal_suffix_complete: expected {}, observed {}",
            entry["terminal_suffix_complete"], observed["terminal_suffix_complete"]
        ));
    }
    let reason = |text: &Value| match text.as_str().unwrap() {
        "unreadable" | "CRC mismatch" => text.clone(),
        other => panic!("{id}: no E4 vocabulary for failure reason {other}"),
    };
    let blocks = |list: &Value, shard: bool| {
        let mut rows: Vec<String> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                if shard {
                    format!(
                        "epoch {} stripe {} parity {} block {} lba {}: {}",
                        row["epoch"],
                        row["stripe"],
                        row["parity_index"],
                        row["sidecar_block"],
                        row["lba"],
                        reason(&row["reason"])
                    )
                } else {
                    format!(
                        "position {} lba {}: {}",
                        row["tape_file_position"],
                        row["lba"],
                        reason(&row["reason"])
                    )
                }
            })
            .collect();
        rows.sort();
        rows
    };
    for (key, shard) in [
        ("failed_data_blocks", false),
        ("failed_parity_shards", true),
    ] {
        let (wanted, seen) = (blocks(&entry[key], shard), blocks(&observed[key], shard));
        if wanted != seen {
            failures.push(format!("{key}: expected {wanted:?}, observed {seen:?}"));
        }
    }
    // A tape with a failed block or shard is never reported complete.
    let any_failure = !observed["failed_data_blocks"]
        .as_array()
        .unwrap()
        .is_empty()
        || !observed["failed_parity_shards"]
            .as_array()
            .unwrap()
            .is_empty();
    if any_failure && observed["tape_complete"] != false {
        failures.push("a tape with a failed block or shard was reported complete".into());
    }
    // A ParityMap copy that could not be used is reported, and the tape is not
    // complete (Section 2.2).
    let map_findings = observed["parity_map_findings"].as_array().unwrap();
    if !map_findings.is_empty() && observed["tape_complete"] != false {
        failures.push("a tape with an unusable ParityMap copy was reported complete".into());
    }
    failures
}

/// Apply the E4 overlay to a case: its rescue outcomes (Section 13.3) and its
/// Verifier observation (Section 2.2). Returns the disagreements.
fn compare_e4_overlay(
    id: &str,
    vector: &VectorImage,
    faults: &Value,
    actual: &Value,
) -> Vec<String> {
    let overlay: Value = serde_json::from_str(E4_OVERLAY).unwrap();
    let mut failures = Vec::new();
    if id == "parity-map-and-sidecar" {
        // The Recoverer, asked for each failed data address, given the
        // validated terminal inventory: both are recovered (the epoch-0 tape
        // copy is reached by the tail rescue from the terminal index).
        for (observation, entry) in overlay["rescue"].as_object().unwrap() {
            let epoch = if observation.contains("(1,0)") {
                "epoch_0"
            } else if observation.contains("(1,4)") {
                "epoch_1"
            } else {
                panic!("{id}: no E4 vocabulary for rescue observation {observation}")
            };
            assert_eq!(entry["outcome"], "recovered", "{id}: E4 vocabulary");
            let holds = actual[epoch] == "recovered";
            println!(
                "E4 RESCUE {id} {} [{observation}]: expected recovered; observed {}",
                if holds { "PASS" } else { "DISAGREEMENT" },
                actual[epoch]
            );
            if !holds {
                failures.push(format!(
                    "[{observation}] expected recovered, observed {}",
                    actual[epoch]
                ));
            }
        }
    }
    if let Some(entry) = overlay["full_verification"].get(id) {
        assert_eq!(entry["image"], faults["image"], "{id}: E4 image");
        let observed = verification_observation(vector, faults);
        let disagreements = compare_verification(id, entry, &observed);
        println!(
            "E4 VERIFIER {id} {} sections={} expected_route={} expected_suffix_complete={} disagreements={disagreements:?} observed={observed}",
            if disagreements.is_empty() { "PASS" } else { "DISAGREEMENT" },
            entry["sections"], entry["map_route"], entry["terminal_suffix_complete"]
        );
        println!(
            "INFORMATIVE {id} E4 other_findings={} open={}",
            entry["other_findings"], entry["open"]
        );
        failures.extend(disagreements.into_iter().map(|d| format!("[verifier] {d}")));
    }
    failures
}

/// Observe directory validation separately from classification, including the
/// parser's error class. This also validates scope without authenticating BOT
/// bytes when supplied values were used.
fn observe_parity_map(
    raw: &mut dyn RawTapeSource,
    scan: &ScanWalkResult,
    uuid: &[u8; 16],
    block_size: u32,
    out: &mut Value,
) {
    out["walked_map_validated"] = json!(false);
    let Some(entry) = scan
        .map
        .entries()
        .iter()
        .rev()
        .find(|e| e.kind == TapeFileKind::ParityMap)
    else {
        return;
    };
    let mut blocks = Vec::new();
    for block_within_file in 0..entry.block_count {
        raw.locate_physical(
            scan.map
                .physical_position(TapeFilePosition {
                    tape_file_number: entry.tape_file_number,
                    block_within_file,
                })
                .unwrap(),
        )
        .unwrap();
        let mut block = vec![0; block_size as usize];
        let block = match raw.read_record(&mut block) {
            Ok(RawReadOutcome::Block { bytes, .. }) if bytes == block.len() => Some(block),
            Ok(_) => None,
            Err(ParityError::TapeIo(error)) if tape_error_is_current_medium_damage(&error) => None,
            Err(error) => panic!("ParityMap observation read failed: {error}"),
        };
        blocks.push(block);
    }
    match parse_parity_map_tape_file_with_unreadable_blocks(&blocks, uuid) {
        Ok(decoded) => {
            match ScopedFilemarkMap::validate_against_final_parity_map(scan.map.clone(), &decoded) {
                Ok(scoped) => {
                    out["walked_map_validated"] = json!(true);
                    out["walk_validated_prefix"] = json!(scoped.validated_prefix_tape_files);
                    out["walk_watermark"] = json!(scoped.scope.watermark());
                }
                Err(error) => out["walk_validation_error"] = parity_error(error)["error"].clone(),
            }
        }
        Err(error) => out["parity_map_error"] = parity_error(error)["error"].clone(),
    }
}

/// The BOT walk of REM-PARITY 8.4.1 as an observation of its own: the walk's
/// classification of each tape file, how it ended, which tape files it found
/// damaged, whether a walked map validates against the final ParityMap, and the
/// identity it gives each Object candidate.
fn walk_observation(vector: &VectorImage, faults: &Value) -> Value {
    let (mut drive, _) = source(vector, faults);
    let mut raw = DriveHandleRawSource::new(&mut drive);
    let uuid = vector.written.inputs.tape_uuid;
    let block_size = vector.written.inputs.block_size;
    let hints = recovery_hints(vector, &faults["hints"]);
    let mode = hints
        .as_ref()
        .map_or(ScanMode::Standard, ScanMode::Recovery);
    let mut out = json!({});
    let scan =
        match scan_reconstruct_filemark_map_with_report_mode(&mut raw, &uuid, block_size, mode) {
            Ok(scan) => scan,
            Err(error) => {
                out["walk_error"] = parity_error(error)["error"].clone();
                return out;
            }
        };
    out["walk_classes"] = json!(scan
        .map
        .entries()
        .iter()
        .map(|e| (e.tape_file_number.to_string(), format!("{:?}", e.kind)))
        .collect::<BTreeMap<_, _>>());
    out.as_object_mut().unwrap().extend(walk_extras(&scan));
    let mut bot_objects = Vec::new();
    match recover_terminal_inventory_from_bot_controlled_mode(
        &mut raw,
        &uuid,
        block_size,
        mode,
        |_| ScanWalkControl::Continue,
        |o| {
            bot_objects.push(o.clone());
            Ok(())
        },
    ) {
        Ok(_) => {
            // The identity of every structurally complete Object candidate is
            // unknown. A torn candidate at EOD is reported as incomplete and
            // is counted apart: it has no identity to give.
            let complete: Vec<_> = bot_objects
                .iter()
                .filter(|o| o.state != BotRecoveredObjectState::Incomplete)
                .collect();
            out["object_identity"] = json!(if !complete.is_empty()
                && complete
                    .iter()
                    .all(|o| o.state == BotRecoveredObjectState::Unknown && o.object_id.is_none())
            {
                "unknown"
            } else {
                "other"
            });
            out["torn_object_candidates"] = json!(bot_objects.len() - complete.len());
        }
        Err(error) => out["walk_error"] = json!(format!("{error:?}")),
    }
    observe_parity_map(&mut raw, &scan, &uuid, block_size, &mut out);
    if hints.is_some() && scan.authoritative_bootstrap().is_none() {
        return out;
    }
    let bootstrap = discover_bootstrap_with_recovery_hints(
        &mut raw,
        DEFAULT_BOOTSTRAP_CANDIDATE_BLOCK_SIZES,
        hints.as_ref(),
    );
    match bootstrap {
        Ok(bootstrap) => match validate_scan_reconstruction_with_report(&mut raw, &bootstrap, scan)
        {
            Ok(validated) => {
                out["walked_map_validated"] =
                    json!(validated.scoped_map.sidecar_directory.is_some());
                out["walk_validated_prefix"] =
                    json!(validated.scoped_map.validated_prefix_tape_files);
                out["walk_watermark"] = json!(validated.scoped_map.scope.watermark());
            }
            Err(error) => out["walk_validation_error"] = json!(format!("{error:?}")),
        },
        Err(error) => out["walk_validation_error"] = json!(format!("{error:?}")),
    }
    out
}

/// The kinds of Scanner outcome an E3 author names. A leading clause that is
/// none of these fails the case.
fn e3_scanner_kind(phrase: &str) -> &'static str {
    if phrase.starts_with("BotStructuralRecoveryRequired") {
        "BotStructuralRecoveryRequired"
    } else if phrase.starts_with("TerminalIndexReplicaConflict") {
        "TerminalIndexReplicaConflict"
    } else if phrase.starts_with("inventory from A2 alone, degraded") {
        "inventory, degraded"
    } else {
        panic!("no E3 vocabulary for Scanner outcome {phrase}")
    }
}

/// What a tape file of an E3 author's walk expectation may be: the walk
/// classes it may take (`absent` for a tape file the walk does not admit as
/// one), and whether it is stated damaged, stated intact, or open.
struct E3WalkFile {
    classes: &'static [&'static str],
    damaged: Option<bool>,
}

fn e3_walk_file(phrase: &str) -> E3WalkFile {
    const EXACT: fn(&'static [&'static str], Option<bool>) -> E3WalkFile =
        |classes, damaged| E3WalkFile { classes, damaged };
    let open = phrase.contains("Damaged or intact: open") || phrase.contains("OPEN");
    let damage = if open {
        None
    } else if phrase.contains("damaged:") || phrase.contains("), damaged") {
        Some(true)
    } else if phrase.ends_with("intact") {
        Some(false)
    } else {
        None
    };
    if phrase.starts_with("Bootstrap") {
        EXACT(&["Bootstrap"], damage)
    } else if phrase.starts_with("Object candidate by elimination") {
        EXACT(&["Object"], damage)
    } else if phrase.starts_with("ParitySidecar") {
        EXACT(&["ParitySidecar"], damage)
    } else if phrase.starts_with("ParityMap") {
        EXACT(&["ParityMap"], damage)
    } else if phrase.starts_with("TapeIndexReplica") {
        EXACT(&["TapeIndexReplica"], damage)
    } else if phrase.starts_with("IndexSeparationExtent") {
        EXACT(&["IndexSeparationExtent"], damage)
    } else if phrase.starts_with("foreign file of 2 records") {
        // The author permits an Object candidate by elimination, or a
        // nonconformant trailing file that is not admitted as an Object.
        EXACT(&["Object", "absent"], None)
    } else if phrase.starts_with("readable foreign head, last record a copy of C's footer") {
        // Additionally, on the weakest reading, a damaged replica.
        EXACT(&["Object", "absent", "TapeIndexReplica"], None)
    } else {
        panic!("no E3 vocabulary for walk phrase {phrase}")
    }
}

/// Compare an E3 author's two observations with the executor's output. The
/// vocabulary is closed: an unknown key, outcome kind or phrase fails the
/// case. Where the author lists permitted outcomes, or permits several
/// classes for a tape file, the observed outcome must be in the permitted set.
fn compare_e3(id: &str, expected: &Value, actual: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    let mut check = |holds: bool, what: String| {
        if !holds {
            failures.push(what);
        }
    };
    for key in expected.as_object().expect("E3 expectation").keys() {
        assert!(
            matches!(key.as_str(), "scanner" | "walk"),
            "{id}: unknown E3 observation {key}"
        );
    }
    // The Scanner.
    let scanner = &expected["scanner"];
    for key in scanner.as_object().expect("E3 scanner entry").keys() {
        assert!(
            matches!(
                key.as_str(),
                "outcome"
                    | "inventory"
                    | "determined"
                    | "reasoning"
                    | "then"
                    | "permitted_outcomes"
                    | "sections"
                    | "quotes"
            ),
            "{id}: unknown E3 scanner key {key}"
        );
    }
    let permitted: Vec<&str> = if scanner["determined"] == true {
        vec![e3_scanner_kind(scanner["outcome"].as_str().unwrap())]
    } else {
        scanner["permitted_outcomes"]
            .as_array()
            .expect("an undetermined Scanner outcome lists its permitted outcomes")
            .iter()
            .map(|o| e3_scanner_kind(o["outcome"].as_str().unwrap()))
            .collect()
    };
    let observed = if actual["outcome"] == "BotStructuralRecoveryRequired" {
        "BotStructuralRecoveryRequired"
    } else if actual["error"] == "TerminalIndexReplicaConflict" {
        "TerminalIndexReplicaConflict"
    } else if actual["inventory"] == true && actual["degraded"] == true {
        "inventory, degraded"
    } else {
        "another outcome"
    };
    check(
        permitted.contains(&observed),
        format!("scanner: expected one of {permitted:?}, observed {observed}"),
    );
    if observed == "BotStructuralRecoveryRequired" {
        // "Never an empty inventory": no inventory is returned, and the walk
        // reports that terminal authority was not recovered.
        check(
            actual["inventory"] != true && actual["terminal_authority_recovered"] == false,
            "scanner: expected no inventory and terminal authority not recovered".into(),
        );
    }
    // The walk.
    let walk = &expected["walk"];
    for key in walk.as_object().expect("E3 walk entry").keys() {
        assert!(
            matches!(
                key.as_str(),
                "tape_files"
                    | "ends"
                    | "produces_map"
                    | "map_validated_against_final_parity_map"
                    | "validated_scope"
                    | "outside_scope"
                    | "second_pass"
                    | "object_identities"
                    | "terminal_authority_recovered"
                    | "determined"
                    | "open_points"
                    | "sections"
                    | "quotes"
            ),
            "{id}: unknown E3 walk key {key}"
        );
    }
    let observed_walk = &actual["walk"];
    check(
        observed_walk.get("walk_error").is_none(),
        format!("walk: failed with {}", observed_walk["walk_error"]),
    );
    if walk["produces_map"] == true {
        check(
            observed_walk["walk_classes"].is_object(),
            "walk: expected a map".into(),
        );
    }
    if walk["map_validated_against_final_parity_map"] == true {
        check(
            observed_walk["walked_map_validated"] == true,
            "walk: expected the map validated against the final ParityMap".into(),
        );
    }
    // "tape files 0..3 (...); T = 4, W = 4": the validated scope is the first
    // four tape files, and W is 4.
    let scope = walk["validated_scope"].as_str().unwrap();
    assert!(
        scope.starts_with("tape files 0..3"),
        "{id}: no E3 vocabulary for validated scope {scope}"
    );
    check(
        observed_walk["walk_validated_prefix"] == 4 && observed_walk["walk_watermark"] == 4,
        format!(
            "walk: expected validated scope of 4 tape files with W = 4, observed {} and {}",
            observed_walk["walk_validated_prefix"], observed_walk["walk_watermark"]
        ),
    );
    let object_identities = walk["object_identities"].as_str().unwrap();
    assert!(
        object_identities.starts_with("tape file 1: unknown"),
        "{id}: no E3 vocabulary for object identities {object_identities}"
    );
    check(
        observed_walk["object_identity"] == "unknown",
        "walk: expected unknown Object identities".into(),
    );
    if walk["terminal_authority_recovered"] == false && observed == "BotStructuralRecoveryRequired"
    {
        check(
            actual["terminal_authority_recovered"] == false,
            "walk: expected terminal authority not recovered".into(),
        );
    }
    // How the walk ends.
    let ends = walk["ends"].as_str().unwrap();
    let (count, truncated) = if let Some(n) = ends.strip_prefix("EOD at the start of tape file ") {
        (n.parse::<u64>().expect("tape file number"), false)
    } else if ends.starts_with("EOD, reached while measuring the run that follows tape file 8") {
        (9, true)
    } else {
        panic!("{id}: no E3 vocabulary for walk end {ends}")
    };
    check(
        observed_walk["walk_tape_file_count"] == count
            && observed_walk["walk_truncated"] == truncated,
        format!(
            "walk: expected {count} tape files and truncated={truncated}, observed {} and {}",
            observed_walk["walk_tape_file_count"], observed_walk["walk_truncated"]
        ),
    );
    // Each tape file.
    for (key, phrase) in walk["tape_files"].as_object().unwrap() {
        if key == "trailing run" {
            assert!(
                phrase
                    .as_str()
                    .unwrap()
                    .starts_with("2 foreign records with no trailing filemark"),
                "{id}: no E3 vocabulary for the trailing run"
            );
            // Not a tape file, and not admitted as one: the walk ends torn.
            check(
                observed_walk["walk_truncated"] == true
                    && observed_walk["walk_classes"].get("9").is_none(),
                "walk: expected the trailing run to be a torn tail, not a tape file".into(),
            );
            continue;
        }
        let file = e3_walk_file(phrase.as_str().unwrap());
        let class = observed_walk["walk_classes"]
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("absent");
        check(
            file.classes.contains(&class),
            format!(
                "walk tape file {key}: expected one of {:?}, observed {class}",
                file.classes
            ),
        );
        if let Some(damaged) = file.damaged {
            let is_damaged = observed_walk["walk_damaged"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| {
                    d["tape_file"]
                        .as_u64()
                        .is_some_and(|file| file.to_string() == *key)
                });
            check(
                is_damaged == damaged,
                format!("walk tape file {key}: expected damaged={damaged}, observed {is_damaged}"),
            );
        }
    }
    failures
}

/// Compare an E4 author's entry with the executor's output. The entry is
/// keyed by its observation; its compared parts are the outcome and the error,
/// through a closed vocabulary.
fn compare_e4(id: &str, expected: &Value, actual: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    for (observation, entry) in expected.as_object().expect("E4 expectation") {
        assert_eq!(
            observation, "Recoverer asked for a data address in epoch 0",
            "{id}: no E4 vocabulary for observation {observation}"
        );
        for key in entry.as_object().expect("E4 entry").keys() {
            assert!(
                matches!(
                    key.as_str(),
                    "outcome"
                        | "error"
                        | "derivation"
                        | "construction_read"
                        | "sections"
                        | "quotes"
                ),
                "{id}: unknown E4 key {key}"
            );
        }
        match entry["outcome"].as_str().unwrap() {
            "recovered" => {
                assert!(entry["error"].is_null(), "{id}: recovered carries no error");
                if !(actual["recovered"] == true && actual["object_sha256_matches"] == true) {
                    failures.push(format!(
                        "[{observation}] expected recovered, observed {}",
                        actual["epoch_0"]
                    ));
                }
            }
            outcome if outcome.starts_with("SidecarMetadataUnavailable{epoch_id: 0}") => {
                assert_eq!(
                    entry["error"], "SidecarMetadataUnavailable",
                    "{id}: error name"
                );
                if actual["epoch_0"] != "SidecarMetadataUnavailable" || actual["epoch"] != 0 {
                    failures.push(format!(
                        "[{observation}] expected SidecarMetadataUnavailable{{epoch_id: 0}}, observed {}",
                        actual["epoch_0"]
                    ));
                }
            }
            other => panic!("{id}: no E4 vocabulary for outcome {other}"),
        }
    }
    failures
}

/// Compare the independent S6 author's pinned executable transcription. Sets
/// are explicit, and unknown keys fail rather than silently losing coverage.
fn compare_s6(id: &str, expected: &Value, checks: &Value, actual: &Value) -> Vec<String> {
    for key in expected.as_object().expect("S6 author entry").keys() {
        assert!(
            matches!(
                key.as_str(),
                "error"
                    | "classification"
                    | "produces_map"
                    | "map_validated_against_final_parity_map"
                    | "damage_reported"
                    | "outcome"
                    | "sections"
                    | "quotes"
                    | "open"
            ),
            "{id}: unknown S6 author key {key}"
        );
    }
    let required: &[&str] = if expected["error"].is_null() {
        &["walk_error", "walk_classes", "required_damage"]
    } else {
        &["walk_error"]
    };
    for key in required {
        assert!(checks.get(*key).is_some(), "{id}: missing S6 check {key}");
    }
    if expected["error"].is_null() {
        assert!(
            checks.get("parity_map_validation").is_some()
                || (checks.get("walked_map_validated").is_some()
                    && checks.get("parity_map_error").is_some()),
            "{id}: missing directory validation checks"
        );
    }
    assert_eq!(
        checks["walk_error"], expected["error"],
        "{id}: error transcription"
    );
    let walk = &actual["walk"];
    let mut failures = Vec::new();
    for (key, wanted) in checks.as_object().expect("S6 checks") {
        let agrees = match key.as_str() {
            "walk_error" => walk[key] == *wanted && actual["error"] == *wanted,
            "walk_classes" => {
                let observed = walk[key].as_object();
                observed.is_some_and(|observed| {
                    observed.keys().all(|k| wanted.get(k).is_some())
                        && wanted.as_object().unwrap().iter().all(|(file, permitted)| {
                            let kind = observed.get(file).cloned().unwrap_or(json!("absent"));
                            permitted.as_array().unwrap().contains(&kind)
                        })
                })
            }
            "walked_map_validated" | "parity_map_error" => wanted
                .as_array()
                .expect("permitted set")
                .contains(&walk[key]),
            "outcome" | "terminal_authority_recovered" => actual[key] == *wanted,
            "object_identity" => walk[key] == *wanted,
            "parity_map_validation" => wanted.as_array().expect("permitted pairs").contains(
                &json!({"validated": walk["walked_map_validated"], "error": walk["parity_map_error"]}),
            ),
            "required_damage" => wanted.as_array().unwrap().iter().all(|damage| {
                walk["walk_damaged"]
                    .as_array()
                    .is_some_and(|reported| reported.contains(damage))
            }),
            other => panic!("{id}: unknown S6 check {other}"),
        };
        if !agrees {
            failures.push(format!("{key}: expected {wanted}, observed {}", walk[key]));
        }
    }
    if expected["produces_map"] != json!(walk["walk_classes"].is_object()) {
        failures.push("walk map presence differs".into());
    }
    failures
}

macro_rules! cases { ($($name:ident => $id:literal),+ $(,)?) => { const CASE_IDS: &[&str] = &[$($id),+]; $(#[test] fn $name() { run($id); })+ }; }
cases! {
    object_head => "object-head", burst_m => "burst-m", burst_m_plus_one => "burst-m-plus-one",
    short_epoch_burst => "short-epoch-burst", short_epoch_recoverable => "short-epoch-recoverable",
    parity_and_data => "parity-and-data", sidecar_primary => "sidecar-primary", sidecar_footer => "sidecar-footer",
    sidecar_primary_and_footer => "sidecar-primary-and-footer", walk_directory_rescue => "walk-directory-rescue",
    walk_sidecar_isolation => "walk-sidecar-isolation", parity_map_both => "parity-map-both",
    parity_map_and_sidecar => "parity-map-and-sidecar", replica_c => "replica-c", replicas_a_b => "replicas-a-b",
    replica_c_payload => "replica-c-payload", replicas_all => "replicas-all", editions_conflict => "editions-conflict",
    editions_survivor => "editions-survivor", separation => "separation", filemark_prefix => "filemark-prefix",
    filemark_after_b => "filemark-after-b", bootstrap_hinted => "bootstrap-hinted", bootstrap_unhinted => "bootstrap-unhinted",
    bootstrap_wrong_scheme => "bootstrap-wrong-scheme",
    e1_01 => "e1-01", e1_02 => "e1-02", e1_03 => "e1-03", e1_04 => "e1-04", e1_05 => "e1-05",
    e1_06 => "e1-06", e1_07 => "e1-07", e1_08 => "e1-08", e1_09 => "e1-09", e1_10 => "e1-10",
    e1_11 => "e1-11", e1_12 => "e1-12", e1_13 => "e1-13", e1_14 => "e1-14", e1_15 => "e1-15",
    e2_01 => "e2-01", e2_02 => "e2-02", e2_03 => "e2-03", e2_04 => "e2-04",
    e2_08 => "e2-08", e2_09 => "e2-09", e2_10 => "e2-10", e2_11 => "e2-11", e2_12 => "e2-12",
    e2_13 => "e2-13",
    e3_01 => "e3-01", e3_02 => "e3-02", e3_03 => "e3-03", e3_04 => "e3-04", e3_05 => "e3-05",
    e3_06 => "e3-06", e3_07 => "e3-07",
    s6_01 => "s6-01",
    s6_02 => "s6-02",
    s6_03 => "s6-03",
    s6_04 => "s6-04",
    s6_05 => "s6-05",
    s6_06 => "s6-06",
    s6_07 => "s6-07",
    s6_08 => "s6-08",
    s6_09 => "s6-09",
    s6_10 => "s6-10",
    s6_11 => "s6-11",
    s6_12 => "s6-12",

}

/// Every E2 expectation quotes the specification text verbatim.
#[test]
fn e2_expectation_quotes_occur_in_the_specification() {
    let frozen: Value = serde_json::from_str(EXPECTATIONS).unwrap();
    let mut quotes = 0;
    for case in frozen["cases"].as_array().unwrap() {
        if case["erratum"] != "E2" {
            continue;
        }
        for quote in case["expected"]["quotes"].as_array().unwrap() {
            let quote = quote.as_str().unwrap();
            assert!(
                crate::tape_image_vectors::specification_quote_holds(quote),
                "{}: quote not in the specification: {quote}",
                case["id"]
            );
            quotes += 1;
        }
    }
    assert!(quotes > 0, "no E2 quotes checked");
}

/// Every E3 and E4 expectation quotes the specification text verbatim.
#[test]
fn e3_and_e4_expectation_quotes_occur_in_the_specification() {
    let frozen: Value = serde_json::from_str(EXPECTATIONS).unwrap();
    let mut quotes = 0;
    for case in frozen["cases"].as_array().unwrap() {
        let entries: Vec<&Value> = match case["erratum"].as_str() {
            Some("E3") => vec![&case["expected"]["scanner"], &case["expected"]["walk"]],
            Some("E4") => case["expected"].as_object().unwrap().values().collect(),
            Some("S6") => vec![&case["expected"]],
            _ => continue,
        };
        for entry in entries {
            for quote in entry["quotes"].as_array().unwrap() {
                let quote = quote.as_str().unwrap();
                assert!(
                    crate::tape_image_vectors::specification_quote_holds(quote),
                    "{}: quote not in the specification: {quote}",
                    case["id"]
                );
                quotes += 1;
            }
        }
    }
    let overlay: Value = serde_json::from_str(E4_OVERLAY).unwrap();
    for entry in std::iter::once(&overlay["rescue"])
        .flat_map(|rescue| rescue.as_object().unwrap().values())
        .chain(overlay["full_verification"].as_object().unwrap().values())
    {
        for quote in entry["quotes"].as_array().unwrap() {
            let quote = quote.as_str().unwrap();
            assert!(
                crate::tape_image_vectors::specification_quote_holds(quote),
                "E4 overlay: quote not in the specification: {quote}"
            );
            quotes += 1;
        }
    }
    assert!(quotes > 0, "no E3 or E4 quotes checked");
}

/// The E2 vocabulary fails closed on an unknown clause, key or phrase.
#[test]
fn e2_expectations_fail_closed() {
    let recovered = json!({"result": "recovered. The block is rebuilt.", "error": null});
    assert!(compare_e2("case", &recovered, &json!({"read_recovered": true})).is_empty());
    assert_eq!(
        compare_e2("case", &recovered, &json!({"read_recovered": false})).len(),
        1
    );
    assert_eq!(
        compare_e2(
            "case",
            &recovered,
            &json!({"read_recovered": true, "read_error": "TapeIo"})
        )
        .len(),
        1
    );
    for bad in [
        json!({"result": "recovered, probably.", "error": null}),
        json!({"result": "recovered.", "error": "TapeIo"}),
        json!({"result": "recovered.", "error": null, "reasoning": "not compared"}),
        json!({"result": "recovered.", "error": null, "component": {"replica D": "valid"}}),
    ] {
        assert!(
            std::panic::catch_unwind(|| compare_e2("case", &bad, &json!({}))).is_err(),
            "accepted {bad}"
        );
    }
    assert!(std::panic::catch_unwind(|| e2_walk_class("a tape file")).is_err());
    assert_eq!(
        e2_walk_class("replica B, as file 4: Object candidate, 3 blocks"),
        "Object"
    );
    assert_eq!(
        e2_walk_class("ParitySidecar (item 5): epoch 0"),
        "ParitySidecar"
    );
}

/// A pending or unknown expectation fails, and every compared difference is a
/// disagreement, including an inventory that is not the image's own.
#[test]
fn e1_expectations_fail_closed() {
    let wanted = json!({"discovery": "refused", "error": "BootstrapParse", "field": "tape UUID",
        "inventory": {"returned": false, "degraded": "not applicable"}, "sections": ["8.4"], "quotes": []});
    let projected = expected_observation("case", "observation", &wanted);
    let refused = json!({"discovery": "refused", "error": "BootstrapParse", "field": "tape UUID"});
    assert!(compare_observation(&projected, &observed_outcome(&refused), &refused).is_empty());
    for actual in [
        json!({"discovery": "refused", "error": "BootstrapParse", "field": "block size"}),
        json!({"discovery": "refused", "error": "DriveCompressionEnabled"}),
        json!({"discovery": "continues on the supplied values", "inventory": true,
            "inventory_unaffected": true, "degraded": false}),
    ] {
        assert!(!compare_observation(&projected, &observed_outcome(&actual), &actual).is_empty());
    }
    let inventory = expected_observation(
        "case",
        "observation",
        &json!({"discovery": "continues on the supplied values", "error": "none", "field": null,
            "inventory": {"returned": true, "degraded": false}}),
    );
    let foreign = json!({"discovery": "continues on the supplied values", "inventory": true,
        "inventory_unaffected": false, "degraded": false});
    assert_eq!(
        compare_observation(&inventory, &observed_outcome(&foreign), &foreign).len(),
        1
    );
    let mut unknown_key = wanted.clone();
    unknown_key["reasoning"] = json!("not a compared key");
    let mut unknown_field = wanted.clone();
    unknown_field["field"] = json!("UUID");
    let mut unknown_discovery = wanted.clone();
    unknown_discovery["discovery"] = json!("refused, probably");
    let mut unfit_degraded = wanted.clone();
    unfit_degraded["inventory"]["degraded"] = json!(false);
    for bad in [
        json!("pending (E1)"),
        Value::Null,
        unknown_key,
        unknown_field,
        unknown_discovery,
        unfit_degraded,
    ] {
        assert!(
            std::panic::catch_unwind(|| expected_observation("case", "observation", &bad)).is_err(),
            "accepted {bad}"
        );
    }
}

/// Every damage fault kind is an explicit key: an unknown key, in a case or
/// in a resolved fault map, fails a run instead of being ignored, and a kind's
/// own keys are checked as well.
#[test]
fn damage_fault_kinds_are_explicit_and_an_unknown_key_fails_a_run() {
    let vector = generate("a4-minimal").unwrap();
    let case = |fault: Value| {
        json!({"id": "vocabulary", "image": "a4-minimal", "fault": fault,
            "hints": null, "expected": {}, "pinned": false})
    };
    let foreign = json!({"foreign": {"first_byte": "58"}});
    // Each kind resolves when it is well formed.
    for fault in [
        json!({"extra_records": [{"tape_file": 5, "after": "last record", "length": 1, "fill": "00"}]}),
        json!({"appended_files": [{"records": [foreign.clone(), {"copy_of": {"tape_file": 8, "record": "last"}}], "trailing_filemark": false}]}),
        json!({"removed_filemark_after_tape_file": 8,
            "second_edition_replica": {"replica": "A", "planned_tape_file": 9, "after_last_record_of": 8}}),
    ] {
        fault_map_of(&case(fault), &vector);
    }
    let map = fault_map_of(
        &case(
            json!({"appended_files": [{"records": [foreign.clone()], "trailing_filemark": true}]}),
        ),
        &vector,
    );
    assert_eq!(map["appended_files"][0]["records"][0]["length"], 262144);
    // An unknown key of the fault, or of any kind, fails.
    for bad in [
        json!({"extra_record": []}),
        json!({"extra_records": [{"tape_file": 5, "after": "last record", "length": 1, "fill": "00", "kind": "x"}]}),
        json!({"extra_records": [{"tape_file": 5, "after": "first record", "length": 1, "fill": "00"}]}),
        json!({"appended_files": [{"records": [foreign.clone()], "trailing_filemark": true, "eod": true}]}),
        json!({"appended_files": [{"records": [foreign.clone()]}]}),
        json!({"appended_files": [{"records": [{"foreign": {"first_byte": "58", "length": 1}}], "trailing_filemark": true}]}),
        json!({"appended_files": [{"records": [{"copy_of": {"tape_file": 8, "record": "first"}}], "trailing_filemark": true}]}),
        json!({"appended_files": [{"records": [{"random": {}}], "trailing_filemark": true}]}),
        json!({"appended_files": [{"records": [foreign.clone()], "trailing_filemark": true}],
            "second_edition_replica": {"replica": "A", "planned_tape_file": 9, "after_last_record_of": 8}}),
        json!({"second_edition_replica": {"replica": "B", "planned_tape_file": 9, "after_last_record_of": 8}}),
        json!({"second_edition_replica": {"replica": "A", "planned_tape_file": 9, "after_last_record_of": 8, "edition": 3}}),
        json!({"parity_map_edits": [{"tape_file": 3, "directory_entry": 0, "field": "sidecar_total_block_count", "add": 1, "hash": "x"}]}),
        json!({"parity_map_edits": [{"tape_file": 3, "directory_entry": 0, "field": "flags", "add": 1}]}),
        json!({"record_faults": [{"tape_file": 2, "record_index": 0, "sidecar_hash": "stale"}]}),
    ] {
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fault_map_of(
                &case(bad.clone()),
                &vector
            )))
            .is_err(),
            "accepted {bad}"
        );
    }
    // The executor refuses a resolved fault map with a key it does not know.
    let mut resolved = fault_map_of(&case(json!({})), &vector);
    resolved["a_new_fault"] = json!(true);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source(&vector, &resolved)))
            .is_err()
    );
}

/// A Verifier's protected-content pass runs in strict and in degraded
/// verification (REM-PARITY 2.2): it reads every data block a sidecar protects
/// and every parity shard, checks each against the acquired index, reports a
/// failure by its address, reads the sidecar's copies and footer and reports a
/// divergence between the copies, and a tape with any failure is not complete.
#[test]
fn verifier_protected_pass_runs_in_strict_and_degraded_verification() {
    let vector = generate("a4-minimal").unwrap();
    let uuid = vector.written.inputs.tape_uuid;
    let verify = |fault: Value| {
        let case = json!({"id": "verification", "image": "a4-minimal", "fault": fault,
            "hints": null, "expected": {}, "pinned": false});
        let faults = fault_map_of(&case, &vector);
        let (mut drive, _) = source(&vector, &faults);
        let mut raw = DriveHandleRawSource::new(&mut drive);
        verify_terminal_index_full(&mut raw, &uuid, BLOCK).expect("verification")
    };
    // A healthy tape: every block and shard checks, and the tape is complete.
    let healthy = verify(json!({}));
    assert!(matches!(
        &healthy,
        TerminalIndexVerificationOutcome::VerifiedComplete(_)
    ));
    assert!(healthy.protected().is_clean(), "{:?}", healthy.protected());
    assert!(healthy.is_complete_tape());
    assert_eq!(healthy.protected().sidecars.len(), 1);
    // One corrupted data block (ordinal 2, tape file 1 block 2) and one
    // corrupted parity shard (stripe 0, parity 0: sidecar block H + 0 = 1).
    let corrupt_data = json!({"tape_file": 1, "record_index": 2,
        "byte_edits": [{"offset": "0x100", "field": "data byte", "xor": "01"}]});
    let corrupt_shard = json!({"tape_file": 2, "record_index": 1,
        "byte_edits": [{"offset": "0x100", "field": "parity byte", "xor": "01"}]});
    let strict = verify(json!({"record_faults": [corrupt_data, corrupt_shard]}));
    assert!(
        strict.is_terminal_suffix_complete(),
        "the terminal suffix is intact"
    );
    assert!(!strict.is_complete_tape());
    let protected = strict.protected();
    let data: Vec<_> = protected.failed_data_blocks().collect();
    assert_eq!(data.len(), 1);
    assert_eq!(
        (
            data[0].position.tape_file_number,
            data[0].position.block_within_file
        ),
        (1, 2)
    );
    assert_eq!(data[0].reason, BlockFailureReason::CrcMismatch);
    let shards: Vec<_> = protected.failed_parity_shards().collect();
    assert_eq!(shards.len(), 1);
    assert_eq!(
        (
            shards[0].epoch_id,
            shards[0].stripe_index,
            shards[0].parity_index
        ),
        (0, 0, 0)
    );
    assert_eq!(shards[0].reason, BlockFailureReason::CrcMismatch);
    // Degraded verification (replica C's payload is corrupt) runs the same
    // pass, and also reports the sidecar's diverging copies: the tail copy is
    // valid on its own and differs from the primary, which the footer vouches
    // for.
    let corrupt_replica = json!({"tape_file": 8, "record_index": 1,
        "byte_edits": [{"offset": "0x10", "field": "payload byte", "xor": "01"}]});
    let diverging_tail = json!({"tape_file": 2, "record_index": 5,
        "byte_edits": [{"offset": "0x108", "field": "tail data CRC entry 0", "xor": "01"}],
        "sidecar_hash": "recomputed", "sidecar_crcs": "recomputed"});
    let degraded = verify(json!({"record_faults": [
        corrupt_data, corrupt_shard, corrupt_replica, diverging_tail]}));
    assert!(
        matches!(
            &degraded,
            TerminalIndexVerificationOutcome::VerifiedDegraded(_)
        ),
        "{degraded:?}"
    );
    assert!(!degraded.is_complete_tape());
    let sidecar = &degraded.protected().sidecars[0];
    assert!(sidecar.copies_diverge);
    assert!(sidecar.has_finding(SidecarFindingKind::SidecarParse));
    assert_eq!(degraded.protected().failed_data_blocks().count(), 1);
    assert_eq!(degraded.protected().failed_parity_shards().count(), 1);
    // A divergence alone, with the terminal suffix intact, is reported by the
    // strict pass and is not a complete tape either.
    let divergent = verify(json!({"record_faults": [diverging_tail]}));
    assert!(divergent.is_terminal_suffix_complete());
    assert!(divergent.protected().sidecars[0].copies_diverge);
    assert!(!divergent.is_complete_tape());
}

/// REM-PARITY 2.2: one unreadable ParityMap copy is used around silently by a
/// Reader, and a full verification reports it and does not call the tape
/// complete, although no data block or shard failed.
#[test]
fn one_unreadable_parity_map_copy_is_a_finding_and_the_tape_is_not_complete() {
    let vector = generate("a4-minimal").unwrap();
    let uuid = vector.written.inputs.tape_uuid;
    let verify = |fault: Value| {
        let case = json!({"id": "verification", "image": "a4-minimal", "fault": fault,
            "hints": null, "expected": {}, "pinned": false});
        let faults = fault_map_of(&case, &vector);
        let (mut drive, _) = source(&vector, &faults);
        let mut raw = DriveHandleRawSource::new(&mut drive);
        verify_terminal_index_full(&mut raw, &uuid, BLOCK).expect("verification")
    };
    assert!(verify(json!({})).is_complete_tape());
    // LBA 15 is the primary ParityMap copy's header, LBA 16 the tail's, and
    // LBA 17 the footer.
    for (lba, copy) in [
        (15, "the primary"),
        (16, "the tail"),
        (17, "the ParityMap footer"),
    ] {
        let outcome = verify(json!({ "unreadable_lbas": [lba] }));
        let protected = outcome.protected();
        assert_eq!(protected.parity_map_findings.len(), 1, "{protected:?}");
        assert_eq!(protected.parity_map_findings[0].tape_file_number, 3);
        assert!(
            protected.parity_map_findings[0].detail.starts_with(copy),
            "{protected:?}"
        );
        assert_eq!(protected.failed_data_blocks().count(), 0);
        assert!(!outcome.is_complete_tape());
        if lba != 15 {
            // Nothing else is wrong: the finding alone blocks completeness.
            assert!(protected.prefix_damage.is_empty(), "{protected:?}");
            assert_eq!(protected.failed_parity_shards().count(), 0);
            assert!(protected.sidecars.iter().all(|s| s.is_clean()));
            assert!(outcome.is_terminal_suffix_complete());
        }
    }
}

/// Damage inside the pre-tail prefix does not by itself abandon the terminal
/// route: with the walked prefix agreeing with the selected replica's rows in
/// tape-file count and record counts, the route is the validated terminal
/// replicas, the protected pass is scoped by their rows, and the damage is a
/// finding. A prefix that disagrees with the rows still requires BOT recovery.
#[test]
fn prefix_damage_with_agreeing_rows_keeps_the_terminal_route() {
    let vector = generate("a4-minimal").unwrap();
    let uuid = vector.written.inputs.tape_uuid;
    let verify = |fault: Value| {
        let case = json!({"id": "verification", "image": "a4-minimal", "fault": fault,
            "hints": null, "expected": {}, "pinned": false});
        let faults = fault_map_of(&case, &vector);
        let (mut drive, _) = source(&vector, &faults);
        let mut raw = DriveHandleRawSource::new(&mut drive);
        verify_terminal_index_full(&mut raw, &uuid, BLOCK).expect("verification")
    };
    // An unreadable head record of the Object (tape file 1): walk damage.
    let damaged = verify(json!({"unreadable_lbas": [2]}));
    assert!(damaged.is_terminal_suffix_complete(), "{damaged:?}");
    assert!(!damaged.is_complete_tape());
    let protected = damaged.protected();
    assert!(!protected.prefix_damage.is_empty());
    let blocks: Vec<_> = protected.failed_data_blocks().collect();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].position.tape_file_number, 1);
    // A filemark missing inside the prefix: the walked entries disagree with
    // the rows, and the terminal layout no longer ends at EOD.
    let merged = verify(json!({"removed_filemark_after_tape_file": 1}));
    assert!(matches!(
        &merged,
        TerminalIndexVerificationOutcome::RecoveryRequired(_)
    ));
    // The tail route now preserves a ParityMap whose first magic byte is
    // damaged. Break its footer too to retain this test of the per-file kind
    // guard: damage to an Object elsewhere cannot excuse a kind mismatch.
    let corrupt_map_head = json!({"tape_file": 3, "record_index": 0,
        "byte_edits": [{"offset": "0x00", "field": "ParityMap magic byte", "xor": "01"}]});
    let corrupt_map_footer = json!({"tape_file": 3, "record_index": 2,
        "byte_edits": [{"offset": "0xc0", "field": "ParityMap footer CRC", "xor": "01"}]});
    let rescued =
        verify(json!({"unreadable_lbas": [2], "record_faults": [corrupt_map_head.clone()]}));
    assert!(rescued.is_terminal_suffix_complete(), "{rescued:?}");
    assert!(!rescued.is_complete_tape());
    let kind_mismatch = verify(json!({"unreadable_lbas": [2],
        "record_faults": [corrupt_map_head, corrupt_map_footer]}));
    assert!(
        matches!(
            &kind_mismatch,
            TerminalIndexVerificationOutcome::RecoveryRequired(_)
        ),
        "{kind_mismatch:?}"
    );
    // A file inserted record: the counts disagree.
    let longer = verify(
        json!({"extra_records": [{"tape_file": 1, "after": "last record", "length": 1, "fill": "00"}]}),
    );
    assert!(matches!(
        &longer,
        TerminalIndexVerificationOutcome::RecoveryRequired(_)
    ));
}

/// The E3 vocabulary fails closed, and its comparison decides.
#[test]
fn e3_expectations_fail_closed_and_decide() {
    let frozen: Value = serde_json::from_str(EXPECTATIONS).unwrap();
    let case = frozen["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "e3-01")
        .unwrap();
    let expected = &case["expected"];
    let walk = json!({"walk_classes": {"0": "Bootstrap", "1": "Object", "2": "ParitySidecar",
            "3": "ParityMap", "4": "TapeIndexReplica", "5": "IndexSeparationExtent",
            "6": "TapeIndexReplica", "7": "IndexSeparationExtent", "8": "TapeIndexReplica"},
        "walk_damaged": [{"tape_file": 5, "kind": "InvalidTerminalControl"}],
        "walk_tape_file_count": 9, "walk_truncated": false, "walk_validated_prefix": 4,
        "walk_watermark": 4, "walked_map_validated": true, "object_identity": "unknown"});
    let actual = json!({"outcome": "BotStructuralRecoveryRequired", "inventory": null,
        "terminal_authority_recovered": false, "walk": walk});
    assert_eq!(compare_e3("e3-01", expected, &actual), Vec::<String>::new());
    for (label, change) in [
        (
            "an inventory is returned",
            json!({"outcome": null, "inventory": true, "degraded": false}),
        ),
        (
            "authority recovered",
            json!({"terminal_authority_recovered": true}),
        ),
    ] {
        let mut wrong = actual.clone();
        for (k, v) in change.as_object().unwrap() {
            wrong[k] = v.clone();
        }
        assert!(!compare_e3("e3-01", expected, &wrong).is_empty(), "{label}");
    }
    for (label, key, value) in [
        ("an undamaged separation AB", "walk_damaged", json!([])),
        (
            "a walk of another length",
            "walk_tape_file_count",
            json!(10),
        ),
        ("a torn walk", "walk_truncated", json!(true)),
        ("an unvalidated map", "walked_map_validated", json!(false)),
        ("known identities", "object_identity", json!("other")),
        ("a scope of another size", "walk_validated_prefix", json!(3)),
    ] {
        let mut wrong = actual.clone();
        wrong["walk"][key] = value;
        assert!(!compare_e3("e3-01", expected, &wrong).is_empty(), "{label}");
    }
    let mut wrong = actual.clone();
    wrong["walk"]["walk_classes"]["6"] = json!("Object");
    assert!(!compare_e3("e3-01", expected, &wrong).is_empty());
    // Unknown keys and phrases fail rather than going uncompared.
    let mut unknown = expected.clone();
    unknown["scanner"]["surprise"] = json!(1);
    assert!(std::panic::catch_unwind(|| compare_e3("e3-01", &unknown, &json!({}))).is_err());
    let mut phrase = expected.clone();
    phrase["walk"]["tape_files"]["0"] = json!("a tape file of no kind");
    assert!(std::panic::catch_unwind(|| compare_e3("e3-01", &phrase, &actual)).is_err());
    // A permitted set decides: an observed outcome outside it disagrees.
    let seven = frozen["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "e3-07")
        .unwrap();
    let mut conflict = actual.clone();
    conflict["outcome"] = json!(null);
    conflict["error"] = json!("TerminalIndexReplicaConflict");
    let mut expected_seven = seven["expected"].clone();
    assert!(compare_e3("e3-07", &expected_seven, &conflict)
        .iter()
        .all(|f| !f.starts_with("scanner")));
    conflict["error"] = json!("SomethingElse");
    assert!(compare_e3("e3-07", &expected_seven, &conflict)
        .iter()
        .any(|f| f.starts_with("scanner")));
    expected_seven["walk"]["ends"] = json!("EOD somewhere");
    assert!(std::panic::catch_unwind(|| compare_e3("e3-07", &expected_seven, &actual)).is_err());
}

/// Observation names must match the fault map's exactly.
#[test]
fn e1_observation_names_must_match_the_fault_map() {
    let observations = [json!({"id": "supplied values", "hints": null})];
    let outcome = json!({"discovery": "refused"});
    check_observation_names("case", &json!({"supplied values": outcome}), &observations);
    for expected in [
        json!({"supplied value": outcome}),
        json!({"supplied values": outcome, "no supplied values": outcome}),
        json!({}),
        json!("pending (E1)"),
    ] {
        assert!(
            std::panic::catch_unwind(|| check_observation_names("case", &expected, &observations))
                .is_err(),
            "accepted {expected}"
        );
    }
}

/// Every key of the record-fault and observation vocabulary is recognised or
/// fails, including a replacement payload's scheme sub-keys.
#[test]
fn e1_fault_vocabulary_refuses_unknown_keys() {
    let vector = generate("a4-minimal").unwrap();
    let hints = json!({"tape_uuid": "the image's", "block_size": 262144,
        "scheme": {"k": 2, "m": 2, "S": 2}});
    let case = |record_fault: Value, observations: Value| {
        json!({"id": "vocabulary", "image": "a4-minimal",
            "fault": {"record_faults": [record_fault]},
            "observations": observations, "expected": {}, "pinned": false})
    };
    let payload = |scheme: Value| {
        json!({"tape_file": 0, "record_index": 0, "payload": {"scheme": scheme},
            "header_crc": "recomputed if the length changes", "payload_crc": "recomputed"})
    };
    let supplied = json!([{"id": "supplied values", "hints": hints}]);
    let map = fault_map_of(&case(payload(json!({"k": 3})), supplied.clone()), &vector);
    assert_eq!(map["record_edits"][0]["edits"].as_array().unwrap().len(), 2);
    let mut foreign = hints.clone();
    foreign["tape_uuid"] = json!("12345678-1234-4234-8234-123456789abc");
    let mut extra_hint = hints.clone();
    extra_hint["candidates"] = json!([524288]);
    let mut hint_scheme = hints.clone();
    hint_scheme["scheme"]["s"] = json!(2);
    for bad in [
        case(payload(json!({"K": 3})), supplied.clone()),
        case(payload(json!({"k": 3, "extra": 1})), supplied.clone()),
        case(payload(json!({"k": "3"})), supplied.clone()),
        case(payload(json!({})), supplied.clone()),
        case(
            json!({"tape_file": 0, "record_index": 0,
                "byte_edits": [{"offset": "0x00", "field": "magic byte 0", "xor": "01", "form": "52"}]}),
            supplied.clone(),
        ),
        case(
            json!({"tape_file": 0, "record_index": 0, "lenght": 1000}),
            supplied.clone(),
        ),
        case(
            json!({"tape_file": 0, "record_index": 0, "length": "1000"}),
            supplied.clone(),
        ),
        case(
            json!({"tape_file": 0, "record_index": 0, "header_crc": true}),
            supplied.clone(),
        ),
        case(
            json!({"tape_file": 0, "record_index": 0, "length": 1000}),
            json!([{"id": "supplied values", "hints": hints, "note": "extra"}]),
        ),
        case(
            json!({"tape_file": 0, "record_index": 0, "length": 1000}),
            json!([{"id": "supplied values", "hints": foreign}]),
        ),
        case(
            json!({"tape_file": 0, "record_index": 0, "length": 1000}),
            json!([{"id": "supplied values", "hints": extra_hint}]),
        ),
        case(
            json!({"tape_file": 0, "record_index": 0, "length": 1000}),
            json!([{"id": "supplied values", "hints": hint_scheme}]),
        ),
        json!({"id": "vocabulary", "image": "a4-minimal",
            "fault": {"record_fault": [{"tape_file": 0, "record_index": 0, "length": 1000}]},
            "expected": {}, "pinned": false}),
        json!({"id": "vocabulary", "image": "a4-minimal",
            "fault": {"record_faults": [{"tape_file": 0, "record_index": 0, "length": 1000}]},
            "observation": supplied, "expected": {}, "pinned": false}),
    ] {
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fault_map_of(&bad, &vector)))
                .is_err(),
            "accepted {bad}"
        );
    }
}

fn fault_map_of(case: &Value, vector: &VectorImage) -> Value {
    crate::tape_image_vectors::fault_map(case, &vector.image)
}

/// Every E1 expectation quotes the specification text verbatim.
#[test]
fn e1_expectation_quotes_occur_in_the_specification() {
    let frozen: Value = serde_json::from_str(EXPECTATIONS).unwrap();
    let mut quotes = 0;
    for case in frozen["cases"].as_array().unwrap() {
        let Some(expected) = case["expected"]
            .as_object()
            .filter(|_| case["observations"].is_array())
        else {
            continue;
        };
        for (name, outcome) in expected {
            for quote in outcome["quotes"].as_array().unwrap() {
                let quote = quote.as_str().unwrap();
                assert!(
                    crate::tape_image_vectors::specification_quote_holds(quote),
                    "{} [{name}]: quote not in the specification: {quote}",
                    case["id"]
                );
                quotes += 1;
            }
        }
    }
    assert!(quotes > 0, "no E1 quotes checked");
}

/// Every frozen case must have a separately named test, so no new row can be
/// omitted while the matrix remains green.
#[test]
fn frozen_matrix_has_a_test_for_every_case() {
    let frozen: Value = serde_json::from_str(EXPECTATIONS).unwrap();
    let cases = frozen["cases"].as_array().unwrap();
    assert_eq!(CASE_IDS.len(), cases.len());
    let registered: std::collections::BTreeSet<_> = CASE_IDS.iter().copied().collect();
    let declared: std::collections::BTreeSet<_> =
        cases.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(registered, declared);
}

/// Combined faults use modified physical LBAs while edits retain source
/// coordinates. Check both the pinned descriptor and the actual tape bytes.
#[test]
fn removed_filemark_combines_unreadable_records_and_edits() {
    let vector = generate("a4-minimal").unwrap();
    let case = json!({"id":"combined", "image":"a4-minimal", "fault": {
        "removed_filemark_after_tape_file":0, "unreadable_lbas":[18,26,34],
        "record_faults":[{"tape_file":4,"record_index":1,"byte_edits":[
            {"offset":"0x0","field":"payload byte","xor":"01"}]}]}});
    let faults = crate::tape_image_vectors::fault_map(&case, &vector.image);
    assert_eq!(
        faults["unreadable_records"],
        json!([
        {"lba":18,"tape_file":3,"record_index":0,"filemark":false},
        {"lba":26,"tape_file":5,"record_index":0,"filemark":false},
        {"lba":34,"tape_file":7,"record_index":0,"filemark":false}])
    );
    assert_eq!(faults["record_edits"][0]["lba"], 19);
    let (mut drive, _) = source(&vector, &faults);
    let mut raw = DriveHandleRawSource::new(&mut drive);
    raw.configure_fixed_block_size(BLOCK).unwrap();
    raw.locate_physical(PhysicalPositionHint::new(18)).unwrap();
    let mut block = vec![0; BLOCK as usize];
    assert!(
        matches!(raw.read_record(&mut block), Err(ParityError::TapeIo(error))
        if tape_error_is_current_medium_damage(&error))
    );
    raw.locate_physical(PhysicalPositionHint::new(19)).unwrap();
    assert!(
        matches!(raw.read_record(&mut block), Ok(RawReadOutcome::Block {bytes, ..}) if bytes == BLOCK as usize)
    );
    assert_eq!(
        block,
        apply_record_edits(
            &vector.image.files[4].bytes[BLOCK as usize..2 * BLOCK as usize],
            &faults["record_edits"][0]
        )
    );
}

/// The comparison must reject missing observations and unknown vocabulary,
/// including the error-only s6-03 case, rather than silently passing coverage.
#[test]
fn s6_comparison_fails_closed() {
    let frozen: Value = serde_json::from_str(EXPECTATIONS).unwrap();
    for case in frozen["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["erratum"] == "S6")
    {
        assert!(!compare_s6(
            case["id"].as_str().unwrap(),
            &case["expected"],
            &case["checks"],
            &json!({})
        )
        .is_empty());
    }
    let case = frozen["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "s6-03")
        .unwrap();
    let actual =
        json!({"error":"FilemarkMapReconstruct", "walk":{"walk_error":"FilemarkMapReconstruct"}});
    assert!(compare_s6("s6-03", &case["expected"], &case["checks"], &actual).is_empty());
    let mut unknown = case["checks"].clone();
    unknown["surprise"] = json!(true);
    assert!(
        std::panic::catch_unwind(|| compare_s6("s6-03", &case["expected"], &unknown, &actual))
            .is_err()
    );
}

/// The walk validates hints even when invoked without the reader executor.
#[test]
fn walk_observation_rejects_unknown_hint() {
    let frozen: Value = serde_json::from_str(EXPECTATIONS).unwrap();
    let case = frozen["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "s6-01")
        .unwrap();
    let vector = generate(case["image"].as_str().unwrap()).unwrap();
    let mut faults = fault_map_of(case, &vector);
    faults["hints"]["unknown"] = json!(true);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| walk_observation(
            &vector, &faults
        )))
        .is_err()
    );
}

/// Only the author's paired rejection or validation outcomes are permitted.
#[test]
fn s6_09_validation_and_error_are_paired() {
    let frozen: Value = serde_json::from_str(EXPECTATIONS).unwrap();
    let case = frozen["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "s6-09")
        .unwrap();
    let vector = generate(case["image"].as_str().unwrap()).unwrap();
    let faults = fault_map_of(case, &vector);
    let mut actual = execute(&vector, &faults, &faults["hints"]);
    actual["walk"] = walk_observation(&vector, &faults);
    for (validated, error, allowed) in [
        (false, json!("ParityMapParse"), true),
        (true, Value::Null, true),
        (false, Value::Null, false),
        (true, json!("ParityMapParse"), false),
    ] {
        actual["walk"]["walked_map_validated"] = json!(validated);
        actual["walk"]["parity_map_error"] = error;
        assert_eq!(
            compare_s6("s6-09", &case["expected"], &case["checks"], &actual).is_empty(),
            allowed
        );
    }
}
