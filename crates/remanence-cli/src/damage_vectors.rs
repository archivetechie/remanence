//! Execute the frozen damage matrix against production discovery, streamed
//! inventory, BOT recovery, sidecar recovery and full verification. Expected
//! results never affect reader behavior; only the comparison interprets them.

use crate::tape_image_vectors::{generate, hex, VectorImage, BLOCK, EXPECTATIONS};
use remanence_chaos::{
    model::{DeviceRole, ModelTransport, Record, VirtualTape, VirtualWorld},
    ChaosTransport, DeviceCtx, FaultEngine,
};
use remanence_library::DriveHandle;
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

fn source(vector: &VectorImage, faults: &Value) -> (DriveHandle, FaultEngine) {
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK);
    let removed = faults["removed_filemark_after_tape_file"].as_u64();
    for (i, file) in vector.image.files.iter().enumerate() {
        tape.records.extend(
            file.bytes
                .chunks_exact(BLOCK as usize)
                .map(|b| Record::Block(b.to_vec())),
        );
        if file.filemark_record.is_some() && removed != Some(i as u64) {
            tape.records.push(Record::Filemark);
        }
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
    let model = ModelTransport::new(
        Arc::new(Mutex::new(world)),
        DeviceRole::Drive { bay: 0x100 },
    );
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
fn execute_reader(vector: &VectorImage, faults: &Value) -> Value {
    let (mut drive, engine) = source(vector, faults);
    let mut raw = DriveHandleRawSource::new(&mut drive);
    let hints = if faults["hints"].is_object() {
        let h = &faults["hints"];
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
    let bootstrap =
        match discover_bootstrap_with_recovery_hints(&mut raw, &candidates, hints.as_ref()) {
            Ok(b) => Some(b),
            Err(ParityError::NoBootstrapFound | ParityError::BootstrapParse(_))
                if hints.is_some() =>
            {
                None
            }
            Err(e) => return parity_error(e),
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
    let mut out = json!({"records": raw.locate_end_of_data().unwrap().lba});
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
    out["observed_medium_error_lbas"] = json!(engine.observed_medium_error_lbas());
    out
}

/// Verification is an independent fixture check, including when discovery or
/// recovery returns early. Its required identity comes from the image inputs;
/// it never supplies that identity to the discovery/recovery reader.
fn execute(vector: &VectorImage, faults: &Value) -> Value {
    let mut out = execute_reader(vector, faults);
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
    let actual = execute(&vector, &faults);
    let pinned = case["pinned"].as_bool().unwrap();
    let failures = if pinned {
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
