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

fn source(vector: &VectorImage, faults: &Value) -> (DriveHandle, FaultEngine) {
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
            ),
            "unknown fault map key {key}"
        );
    }
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK);
    let removed = faults["removed_filemark_after_tape_file"].as_u64();
    let edits = faults["record_edits"]
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
        }
        if file.filemark_record.is_some() && removed != Some(i as u64) {
            tape.records.push(Record::Filemark);
        }
    }
    for resolved in &edits {
        let file = &vector.image.files[resolved["tape_file"].as_u64().unwrap() as usize];
        assert_eq!(
            resolved["lba"].as_u64().unwrap() as usize,
            file.start_record + resolved["record_index"].as_u64().unwrap() as usize,
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
        ParityError::NoBootstrapFound => json!({"error":"NoBootstrapFound"}),
        // Section 15's names, applied after every continuation decision.
        ParityError::BootstrapRefused { field, .. } => {
            json!({"error":"BootstrapParse", "field":field.section_8_4_name()})
        }
        ParityError::DriveCompressionEnabled => json!({"error":"DriveCompressionEnabled"}),
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

/// Reader inputs come only from discovered bootstrap bytes or declared hints.
fn execute_reader(vector: &VectorImage, faults: &Value, hints: &Value) -> Value {
    let (mut drive, engine) = source(vector, faults);
    let mut raw = DriveHandleRawSource::new(&mut drive);
    check_hints(hints);
    let hints = if hints.is_object() {
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
    };
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
                    ParityError::BootstrapRefused { .. } | ParityError::DriveCompressionEnabled => {
                        "refused"
                    }
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
    let mut objects = Vec::new();
    let mut attempt = None;
    let inventory = read_terminal_index_inventory_streamed(&mut raw, &uuid, block_size, |event| {
        match event {
            TerminalInventoryStreamEvent::ReplicaAttemptStarted { attempt_id, .. } => {
                entries.clear();
                objects.clear();
                attempt = Some(attempt_id);
            }
            TerminalInventoryStreamEvent::StructuralEntry {
                attempt_id, entry, ..
            } => {
                assert_eq!(attempt, Some(attempt_id));
                entries.push(map_entry(entry));
            }
            TerminalInventoryStreamEvent::ObjectRow {
                attempt_id, row, ..
            } => {
                assert_eq!(attempt, Some(attempt_id));
                objects.push(row);
            }
            TerminalInventoryStreamEvent::ReplicaAttemptRejected { .. } => {
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
            let scope = &selection.edition.descriptor.scope;
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
            ScopedFilemarkMap::validate_against_digest(
                map,
                &FilemarkMapDigest {
                    map_sha256: selection.edition.canonical_map_sha256,
                    tape_file_count: scope.covered_prefix_tape_file_count,
                    map_total_data_ordinals: scope.total_data_ordinals,
                    highest_protected_ordinal: scope.highest_protected_ordinal,
                    covers_complete_map: true,
                },
            )
            .unwrap()
        }
        Ok(TerminalInventoryOutcome::BotStructuralRecoveryRequired(required)) => {
            out["outcome"] = json!("BotStructuralRecoveryRequired");
            out["terminal_authority_recovered"] = json!(false);
            out["every_replica_invalid"] = json!(required
                .replicas
                .iter()
                .all(|r| matches!(r, TerminalReplicaEvidence::Invalid(_))));
            let scan = match scan_reconstruct_filemark_map_with_report(&mut raw, &uuid, block_size)
            {
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
            let mut bot_objects = Vec::new();
            match recover_terminal_inventory_from_bot(&mut raw, &uuid, block_size, |o| {
                bot_objects.push(o.clone());
                Ok(())
            }) {
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
                out["error"] = json!("NoBootstrapFound");
                return out;
            };
            match validate_scan_reconstruction_with_report(&mut raw, bootstrap, scan) {
                Ok(validated) => {
                    out["walked_map_validated"] =
                        json!(validated.scoped_map.sidecar_directory.is_some());
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
    let actual = execute(&vector, &faults, &faults["hints"]);
    let failures = if case["erratum"] == "E2" {
        compare_e2(id, &case["expected"], &actual)
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
    if pinned {
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
        other => panic!("{id}: no E2 vocabulary for error {other}"),
    }
    let result = expected["result"].as_str().expect("E2 result");
    if result.starts_with("recovered.") {
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
