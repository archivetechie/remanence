//! Specification-authored isolated variants. Reuse the one image builder and
//! production encoders; retain every edit, rebuild recipe and fault address.
//! Assertions check construction separately from the production rejection.
use super::*;
#[cfg(test)]
use remanence_chaos::{
    model::{DeviceRole, ModelTransport, Record, VirtualTape, VirtualWorld},
    ChaosTransport, DeviceCtx, FaultEngine,
};
#[cfg(test)]
use remanence_library::DriveHandle;
#[cfg(test)]
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

/// Parse the specification-authored supplement without accepting corpus drift.
pub fn parse_source(bytes: &[u8]) -> Result<Value, String> {
    if hex(&Sha256::digest(bytes))
        != "d0d83517ab405be5a216fbe4d33281d5ea40bf6fef499699d23dcab0dc2977e2"
    {
        return Err("negative-cases-supplement.json SHA-256 differs".into());
    }
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

/// Read the supplement copied verbatim by the generator.
pub fn source() -> Result<Value, String> {
    parse_source(
        &fs::read(fixture_root().join("tape-images/negatives/negative-cases-supplement.json"))
            .map_err(|e| e.to_string())?,
    )
}

impl Editor {
    fn set(&mut self, f: usize, b: usize, o: usize, n: u64, width: usize) {
        self.write(
            f,
            b,
            o,
            &n.to_le_bytes()[..width],
            "supplement field / dependent mirror",
        );
    }
    fn replace_file(&mut self, f: usize, blocks: Vec<Vec<u8>>, recipe: &str) {
        let old = self.files.get(&f).map(|b| hex(&Sha256::digest(b.concat())));
        self.edits.push(json!({"tape_file":f,"rebuild":recipe,"old_sha256":old,"new_sha256":hex(&Sha256::digest(blocks.concat())),"block_count":blocks.len()}));
        self.changed.retain(|(file, _)| *file != f);
        for b in 0..blocks.len() {
            self.changed.insert((f, b));
        }
        self.files.insert(f, blocks);
    }
}

fn pm_payload(e: &Editor, f: usize) -> Cbor {
    let len = u64_at(&e.files[&f][0], 0x30) as usize;
    ciborium::from_reader(&e.files[&f][0][0xc8..0xc8 + len]).expect("builder CBOR")
}
fn pm_edit(e: &mut Editor, f: usize, mutate: impl FnOnce(&mut Cbor)) {
    let len = u64_at(&e.files[&f][0], 0x30) as usize;
    let mut p = pm_payload(e, f);
    mutate(&mut p);
    e.parity_payload(f, &p, len);
}
fn directory_entry(p: &mut Cbor) -> &mut Cbor {
    &mut cbor_key(cbor_key(p, 4), 5).as_array_mut().unwrap()[0]
}

struct Records {
    entries: Vec<TapeIndexReplicaMapEntry>,
    rows: Vec<TapeIndexReplicaObjectRow>,
}
impl TapeIndexReplicaRecordSource for Records {
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        self.entries.iter().try_for_each(visitor)
    }
    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        self.rows.iter().try_for_each(visitor)
    }
}
fn records(files: &Files, f: usize, uuid: &[u8; 16]) -> Result<Records, String> {
    let h = parse_tape_index_replica_header(&files[&f][0], uuid).map_err(|e| e.to_string())?;
    let t = parse_tape_index_bootstrap_footer(&files[&f][2], uuid).map_err(|e| e.to_string())?;
    let mut r = Records {
        entries: vec![],
        rows: vec![],
    };
    validate_tape_index_replica_payload(
        &h,
        &t,
        &mut Payload(&files[&f][1..2]),
        |e| {
            r.entries.push(e.clone());
            Ok(())
        },
        |row| {
            r.rows.push(row.clone());
            Ok(())
        },
    )
    .map_err(|e| e.to_string())?;
    Ok(r)
}

/// Rebuild valid dependants of a malformed sidecar, using the production
/// ParityMap and terminal encoders. The builder cannot emit an invalid sidecar.
fn rebuild_suffix(e: &mut Editor, uuid: [u8; 16], real_five: bool) -> Result<(), String> {
    let base = generate("a4-minimal")?;
    let mut map_entries = base.written.map.entries()[..4].to_vec();
    map_entries[2].block_count = e.files[&2].len() as u64;
    if real_five {
        map_entries[1].block_count = 5;
        map_entries[2].protected_ordinal_end_exclusive = Some(5);
    }
    let map = FilemarkMap::new(map_entries).map_err(|e| e.to_string())?;
    let mut payload = parse_parity_map_tape_file(&e.files[&3], &uuid)
        .map_err(|e| e.to_string())?
        .payload;
    payload.canonical_map_digest = map.canonical_digest().map_err(|e| e.to_string())?;
    let h = &e.files[&2][0];
    let row = &mut payload.directory.entries[0];
    row.sidecar_total_block_count = e.files[&2].len() as u64;
    row.sidecar_header_block_count = u64_at(h, 0x60);
    row.canonical_metadata_hash = h[0x98..0xb8].try_into().unwrap();
    if real_five {
        row.protected_ordinal_end_exclusive = 5;
        payload.directory.directory_scope_total_data_ordinals = 5;
        payload.directory.directory_scope_highest_protected_ordinal = 5;
    }
    let encoded = encode_parity_map_tape_file(&payload, BLOCK).map_err(|e| e.to_string())?;
    e.replace_file(
        3,
        encoded.blocks,
        "encode_parity_map_tape_file with rebuilt directory and FilemarkMap::canonical_digest",
    );
    let mut r = records(&e.files, 4, &uuid)?;
    r.entries[2].block_count = e.files[&2].len() as u64;
    if real_five {
        r.entries[1].block_count = 5;
        r.entries[2].protected_ordinal_end_exclusive = Some(5);
        // The fifth-block Object came from the same builder; its recovery row
        // (including the manifest position/hash) follows that actual Object.
        let mut input = crate::tape_image_vectors::inputs("a4-minimal");
        input.objects = vec![crate::tape_image_vectors::object(5, 0)];
        let (written, _) = crate::tape_image_vectors::write_model(input)?;
        r.rows = written.object_rows;
    }
    let prefix_end = r.entries.iter().map(|r| r.block_count + 1).sum();
    let terminal =
        TerminalTailLayout::new(0, BLOCK, 4, prefix_end, 3, 3).map_err(|e| e.to_string())?;
    let scope = TapeIndexReplicaScope {
        covered_prefix_tape_file_count: 4,
        total_data_ordinals: if real_five { 5 } else { 4 },
        highest_protected_ordinal: if real_five { 5 } else { 4 },
    };
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: 4,
        object_row_count: 1,
    };
    let plan = assemble_terminal_plan(
        uuid,
        BLOCK,
        false,
        base.written.inputs.edition_sequence,
        scope,
        counts,
        &mut r,
        terminal,
        base.written.terminal_diagnostics,
        base.written.inputs.edition_id,
        3 * u64::from(BLOCK),
    )
    .map_err(|e| e.to_string())?;
    for replica in &plan.replicas {
        let c = replica.component;
        let mut blocks = vec![];
        write_tape_index_replica(
            replica,
            TapeIndexReplicaObservation {
                tape_file_number: c.planned_tape_file_number,
                start_lba: c.planned_start_lba,
                record_count: c.record_count,
            },
            &mut r,
            |b| {
                blocks.push(b.to_vec());
                Ok(())
            },
        )
        .map_err(|e| e.to_string())?;
        e.replace_file(
            c.planned_tape_file_number as usize,
            blocks,
            "assemble_terminal_plan + write_tape_index_replica",
        );
    }
    for sep in &plan.separations {
        let c = sep.component;
        let mut blocks = vec![];
        write_index_separation(
            sep,
            IndexSeparationObservation {
                tape_file_number: c.planned_tape_file_number,
                start_lba: c.planned_start_lba,
                record_count: c.record_count,
            },
            |b| {
                blocks.push(b.to_vec());
                Ok(())
            },
        )
        .map_err(|e| e.to_string())?;
        e.replace_file(
            c.planned_tape_file_number as usize,
            blocks,
            "assemble_terminal_plan + write_index_separation",
        );
    }
    Ok(())
}

/// Resolve every listed variant by its stable id, never by parser results.
pub fn resolve(v: &Value) -> Result<Resolved, String> {
    let path = v["id"].as_str().ok_or("supplement id missing")?;
    let (parent, variant) = path.split_once('/').ok_or("supplement variant missing")?;
    let unit = v["base"]["artifact"] == "unit level";
    let artifact = if unit {
        "unit level"
    } else if parent == "overflow-3.2-14-append-point" {
        "tape-image unfinalized-open"
    } else if parent == "paritymap-directory-not-ascending" {
        "tape-image two-epoch"
    } else if parent == "terminal-scope-scalars" && variant == "isolated-2" {
        "terminal profile minimal-256k"
    } else if parent.starts_with("terminal-")
        || parent == "overflow-7.2-T"
        || parent == "overflow-10.3-manifest-range"
    {
        "terminal profile multi-256k"
    } else {
        "tape-image a4-minimal"
    };
    let (files, uuid) = if unit {
        (BTreeMap::new(), [0; 16])
    } else {
        base_artifact(artifact)?
    };
    let mut e = Editor {
        artifact: artifact.into(),
        files,
        edits: vec![],
        changed: BTreeSet::new(),
    };
    let mut faults = vec![];
    let mut roles = vec![];
    let mut targets = vec![2];
    let mut block = 0;
    let mut entries = vec![];
    let mut commit_inputs = Value::Null;
    if unit {
        entries = unit_entries(parent, variant)?;
    } else if parent.starts_with("sidecar-") {
        roles = vec![Role::SidecarCopy];
        let mut tail = 5;
        let mut footer = 6;
        let mut stream_end = 0x128;
        let rebuild = matches!(
            parent,
            "sidecar-header-block-count" | "sidecar-m-zero" | "sidecar-total-block-count"
        ) || (parent == "sidecar-real-data-shard-count" && variant == "isolated-2");
        match parent {
            "sidecar-header-block-count" => {
                let b = e.files[&2].clone();
                let mut empty = vec![0; B];
                let crc = crc64_xz(&empty[..B - 8]);
                empty[B - 8..].copy_from_slice(&crc.to_le_bytes());
                let mut blocks = vec![b[0].clone(), empty.clone()];
                blocks.extend_from_slice(&b[1..5]);
                blocks.extend([b[5].clone(), empty, b[6].clone()]);
                e.replace_file(2,blocks,"builder sidecar parts: primary + empty CRC index + four parity blocks + tail + empty CRC index + footer");
                tail = 6;
                footer = 8;
                for b in [0, tail] {
                    for (o, n) in [(0x60, 2), (0x70, 9), (0x80, 6), (0x88, 8)] {
                        e.set(2, b, o, n, 8);
                    }
                }
                for (o, n) in [(0x38, 2), (0x48, 9), (0x58, 6)] {
                    e.set(2, footer, o, n, 8);
                }
            }
            "sidecar-m-zero" => {
                let b = e.files[&2].clone();
                e.replace_file(
                    2,
                    vec![b[0].clone(), b[5].clone(), b[6].clone()],
                    "builder primary/tail/footer, parity blocks removed",
                );
                tail = 1;
                footer = 2;
                stream_end = 0xe8;
                for b in [0, tail] {
                    let crc = e.files[&2][b][0x108..0x128].to_vec();
                    e.write(2, b, 0xc8, &[0; 96], "remove parity entries");
                    e.write(2, b, 0xc8, &crc, "builder data CRC entries");
                    e.set(2, b, 0x22, 0, 2);
                    for (o, n) in [(0x50, 0), (0x68, 32), (0x70, 3), (0x80, 1), (0x88, 2)] {
                        e.set(2, b, o, n, 8);
                    }
                }
                for (o, n) in [(0x40, 0), (0x48, 3), (0x58, 1)] {
                    e.set(2, footer, o, n, 8);
                }
            }
            "sidecar-total-block-count" => {
                let mut b = e.files[&2].clone();
                b.push(vec![0; B]);
                e.replace_file(2, b, "builder sidecar + all-zero trailing block");
                for b in [0, tail] {
                    e.set(2, b, 0x70, 8, 8);
                }
                e.set(2, footer, 0x48, 8, 8);
            }
            "sidecar-real-data-shard-count" => {
                let real = if variant == "isolated-1" { 3 } else { 5 };
                stream_end = 0x108 + real * 8;
                if real == 5 {
                    let mut input = crate::tape_image_vectors::inputs("a4-minimal");
                    input.objects = vec![crate::tape_image_vectors::object(5, 0)];
                    let (_, image) = crate::tape_image_vectors::write_model(input)?;
                    e.replace_file(
                        1,
                        image.files[1]
                            .bytes
                            .chunks_exact(B)
                            .map(<[u8]>::to_vec)
                            .collect(),
                        "write_tape_image with object(5,0); retain its exact Object bytes",
                    );
                    e.set(2, footer, 0x30, 5, 8);
                }
                for b in [0, tail] {
                    for (o, n) in [
                        (0x48, real as u64),
                        (0x58, real as u64),
                        (0x68, (64 + real * 8) as u64),
                    ] {
                        e.set(2, b, o, n, 8);
                    }
                    if real == 3 {
                        e.set(2, b, 0x120, 0, 8);
                    } else {
                        e.set(2, b, 0x38, 5, 8);
                        for i in 0..5 {
                            let crc = data_shard_crc64(&e.files[&1][i]);
                            e.set(2, b, 0x108 + 8 * i, crc, 8);
                        }
                    }
                }
            }
            "sidecar-primary-tail-disagreement" => {
                let n = e.files[&2][tail][0x108] ^ 1;
                e.write(2, tail, 0x108, &[n], "mutate tail data CRC entry");
                e.sidecar_repair(2, tail, "R-SC-HASHED", stream_end);
                faults.push((2, footer));
                roles = vec![];
                entries.push(
                    "scan_reconstruct_filemark_map_with_report (BOT Scanner; footer medium error)"
                        .into(),
                );
            }
            "sidecar-canonical-hash" => {
                for b in [0, tail] {
                    let n = e.files[&2][b][0x98] ^ 1;
                    e.write(2, b, 0x98, &[n], "same wrong canonical hash in both copies");
                }
            }
            "sidecar-tape-uuid" => {
                for b in [0, tail] {
                    e.write(2, b, 0x08, &[0xee; 16], "wire UUID; keep real-tape magic");
                }
            }
            _ => {
                for b in [0, tail] {
                    e.field(2, b, &v["mutation"])?;
                }
            }
        }
        if parent != "sidecar-primary-tail-disagreement" {
            let repair = if parent == "sidecar-canonical-hash"
                || (parent == "sidecar-primary-start" && variant == "isolated-2")
            {
                "CRCS"
            } else {
                "R-SC-HASHED"
            };
            for b in [0, tail] {
                e.sidecar_repair(2, b, repair, stream_end);
            }
            let hash = e.files[&2][0][0x98..0xb8].to_vec();
            let footer_fault = matches!(
                parent,
                "sidecar-parity-block-count"
                    | "sidecar-primary-start"
                    | "sidecar-tail-start"
                    | "sidecar-tape-uuid"
            );
            if footer_fault {
                faults.push((2, footer));
            } else {
                e.write(2, footer, 0x60, &hash, "ISO-BOTH footer hash");
                e.sidecar_repair(2, footer, "R-SC-FOOTER", 0);
            }
            if rebuild {
                rebuild_suffix(&mut e, uuid, parent == "sidecar-real-data-shard-count")?;
            } else {
                pm_edit(&mut e, 3, |p| {
                    *cbor_key(directory_entry(p), 8) = Cbor::Bytes(hash)
                });
            }
            entries.extend([
                "parse_sidecar_index_blocks (primary and tail)".into(),
                "recover_ordinal_from_sidecar (chaos medium faults, rebuilt map)".into(),
            ]);
        }
    } else if parent == "paritymap-directory-not-ascending" {
        pm_edit(&mut e, 4, |p| {
            cbor_key(cbor_key(p, 4), 5)
                .as_array_mut()
                .unwrap()
                .swap(0, 1)
        });
        roles = vec![Role::ParityMap, Role::Directory];
        targets = vec![4];
    } else if parent == "paritymap-footer-header-disagreement" {
        e.field(3, 2, &v["mutation"])?;
        e.crc(3, 2, 0xc0, "footer CRC");
        roles = vec![Role::ParityMap];
        targets = vec![3];
    } else if parent == "overflow-9.1-tail-start" {
        e.set(2, 6, 0x40, u64::MAX, 8);
        e.sidecar_repair(2, 6, "R-SC-FOOTER", 0);
        faults.extend([(2, 0), (2, 5)]);
        roles = vec![Role::SidecarFooter];
        block = 6;
        entries.extend([
            "parse_sidecar_footer_block".into(),
            "recover_ordinal_from_sidecar (both header blocks unreadable)".into(),
        ]);
    } else if parent == "overflow-13.3-tail-location" {
        pm_edit(&mut e, 3, |p| {
            *cbor_key(directory_entry(p), 6) = Cbor::Integer(
                if variant == "isolated-1" {
                    7u64
                } else {
                    u64::MAX
                }
                .into(),
            )
        });
        faults.extend([(2, 0), (2, 6)]);
        entries.push(
            "recover_ordinal_from_sidecar (directory rescue; primary/footer medium errors)".into(),
        );
    } else if parent == "overflow-3.2-14-append-point" {
        let base = generate("unfinalized-open")?;
        commit_inputs = crate::resume_vectors::portable_input("resume-open", &base);
        for row in commit_inputs["committed_prefix"].as_array_mut().unwrap() {
            let f = row["tape_file_number"].as_u64().unwrap() as usize;
            row["physical_start_override"] = json!(base.image.files[f].start_record);
        }
        commit_inputs["committed_prefix"][2]["block_count"] = json!(u64::MAX);
        commit_inputs
            .as_object_mut()
            .unwrap()
            .remove("append_object");
        entries.push("resume_record_result(FileTapeFileJournal::committed_snapshot_bounded) -> checked_bounded_resume_summary".into());
    } else if parent == "overflow-10.3-manifest-range" {
        e.slot(6, 384, 256, |row| {
            *cbor_key(row, 10) = Cbor::Integer(u64::MAX.into());
            Ok(())
        })?;
        e.replica_payload(6, false)?;
        roles = vec![Role::Replica];
        targets = vec![6];
    } else if parent == "terminal-scope-scalars" || parent == "overflow-7.2-T" {
        targets = terminal_variant(&mut e, parent, variant)?;
        roles = vec![Role::Replica];
    } else {
        return Err(format!("no supplement resolver for {path}"));
    }
    entries.extend(roles.iter().map(|role| {
        ROLES
            .iter()
            .find(|r| r.role == *role)
            .unwrap()
            .entry
            .to_string()
    }));
    let mut manifest = String::new();
    for &(f, b) in &e.changed {
        let bytes = &e.files[&f][b];
        manifest.push_str(&format!(
            "{path}\t{artifact}\t{f}\t{b}\t{}\t{}\n",
            bytes.len(),
            hex(&Sha256::digest(bytes))
        ));
    }
    let descriptor = json!({"case":path,"artifact":artifact,"entry_points":entries,"authored_mutation":v["mutation"],"commit_record_inputs":commit_inputs,"unit_inputs":if unit {v["mutation"].clone()}else{Value::Null},"faults":faults.iter().map(|(f,b)|json!({"tape_file":f,"block":b,"kind":"medium error","byte_changes":false})).collect::<Vec<_>>(),"edits":e.edits});
    let result = Resolved {
        path: path.into(),
        expected: v.clone(),
        descriptor,
        manifest,
        files: e.files,
        uuid,
        roles,
        targets,
        block,
    };
    assert_construction(&result)?;
    Ok(result)
}

/// Repack builder-produced fixed slots and regenerate all five frames' bindings.
/// Malformed scopes cannot be passed to the production writer; its healthy
/// frames provide the parts, and ordered byte edits record the invalid claims.
fn terminal_variant(e: &mut Editor, parent: &str, variant: &str) -> Result<Vec<usize>, String> {
    if parent == "terminal-scope-scalars" && variant == "isolated-2" {
        for b in [0, 2] {
            e.set(1, b, 0x50, 1, 8);
        }
        e.replica_repair(1, true);
        return Ok(vec![1]);
    }
    let mut first = 6;
    let start;
    let huge = parent == "overflow-7.2-T";
    let repack = huge || matches!(variant, "isolated-3" | "isolated-4");
    if repack {
        let payload = e.files[&6][1].clone();
        let mut slots = Vec::new();
        let indices = if huge {
            vec![0, 1, 3]
        } else {
            vec![0, 1, 2, 3, 5]
        };
        for i in &indices {
            slots.extend_from_slice(&payload[i * 64..(i + 1) * 64]);
        }
        slots.extend_from_slice(&payload[384..896]);
        first = indices.len();
        start = if huge { 8 } else { 19 };
        for f in [6, 8, 10] {
            let mut block = vec![0; B];
            block[..slots.len()].copy_from_slice(&slots);
            e.write(f,1,0,&block,"repack production fixed slots; remove second sidecar / both sidecars and ParityMap");
            if huge {
                e.slot(f, 64, 64, |r| {
                    r.as_array_mut().unwrap()[2] = Cbor::Integer(1.into());
                    Ok(())
                })?;
                e.slot(f, 128, 64, |r| {
                    let a = r.as_array_mut().unwrap();
                    a[0] = Cbor::Integer(2.into());
                    a[2] = Cbor::Integer(u64::MAX.into());
                    a[3] = Cbor::Integer(1.into());
                    Ok(())
                })?;
                // Both huge-profile Objects use the builder's plaintext row.
                let plain = e.files[&f][1][192..448].to_vec();
                e.write(f, 1, 448, &plain, "reuse plaintext Object row");
                e.slot(f, 192, 256, |r| {
                    *cbor_key(r, 3) = Cbor::Integer(1.into());
                    Ok(())
                })?;
                e.slot(f, 448, 256, |r| {
                    *cbor_key(r, 1) = Cbor::Integer(2.into());
                    *cbor_key(r, 3) = Cbor::Integer(u64::MAX.into());
                    Ok(())
                })?;
            } else {
                e.slot(f, 256, 64, |r| {
                    r.as_array_mut().unwrap()[0] = Cbor::Integer(4.into());
                    Ok(())
                })?;
            }
            for b in [0, 2] {
                for (o, n) in [
                    (0x48, first as u64),
                    (0x50, if huge { 0 } else { 5 }),
                    (
                        0x58,
                        if huge {
                            0
                        } else if variant == "isolated-3" {
                            2
                        } else {
                            5
                        },
                    ),
                    (0x60, first as u64),
                    (0x70, slots.len() as u64),
                ] {
                    e.set(f, b, o, n, 8);
                }
            }
        }
    } else {
        start = u64_at(&e.files[&6][0], 0x90);
    }
    let planned_first = if repack { first } else { 7 };
    let layout = TerminalTailLayout::new(0, BLOCK, planned_first as u64, start, 3, 3)
        .map_err(|e| e.to_string())?;
    let digest = layout.digest().map_err(|e| e.to_string())?;
    for (i, f) in [6, 7, 8, 9, 10].into_iter().enumerate() {
        let replica = i % 2 == 0;
        let (
            tuples,
            local_file,
            local_start,
            eod,
            hash,
            observed_file,
            observed_start,
            observed_footer,
        ) = if replica {
            (0x148, 0x88, 0x90, 0xa0, 0x108, 0x2d8, 0x2e0, 0x2f0)
        } else {
            (0xe0, 0x40, 0x48, 0xd8, 0x98, 0x1a0, 0x1a8, 0x1b8)
        };
        for b in [0, 2] {
            for (j, c) in layout.components.iter().enumerate() {
                e.set(f, b, tuples + 32 * j + 8, c.planned_tape_file_number, 8);
                e.set(f, b, tuples + 32 * j + 16, c.planned_start_lba, 8);
            }
            let c = layout.components[i];
            e.set(f, b, local_file, c.planned_tape_file_number, 8);
            e.set(f, b, local_start, c.planned_start_lba, 8);
            e.set(f, b, eod, layout.expected_eod_lba, 8);
            e.write(f, b, hash, &digest, "regenerated terminal layout digest");
            if !replica {
                e.set(
                    f,
                    b,
                    0x78,
                    layout.components[i - 1].planned_tape_file_number,
                    8,
                );
                e.set(
                    f,
                    b,
                    0x80,
                    layout.components[i + 1].planned_tape_file_number,
                    8,
                );
                e.set(f, b, 0x88, layout.components[i - 1].planned_start_lba, 8);
                e.set(f, b, 0x90, layout.components[i + 1].planned_start_lba, 8);
            }
        }
        let c = layout.components[i];
        e.set(f, 2, observed_file, c.planned_tape_file_number, 8);
        e.set(f, 2, observed_start, c.planned_start_lba, 8);
        e.set(f, 2, observed_footer, c.planned_start_lba + 2, 8);
        if replica {
            e.replica_payload(f, repack)?;
        } else {
            e.separation_repair(f);
        }
    }
    // File keys are local storage addresses. Planned numbers are deliberately
    // claims in isolated-1; no device counter is supplied to this observation.
    Ok(vec![6, 8, 10])
}

fn unit_entries(parent: &str, variant: &str) -> Result<Vec<String>, String> {
    let names: Vec<&str> = match parent {
        "overflow-8.3-component-starts" | "overflow-8.3-eod" => {
            vec!["TerminalTailLayout::validate"]
        }
        "overflow-9.1-P" | "overflow-9.2-SxK" => vec!["sidecar::checked_sidecar_shard_product"],
        "overflow-9.1-total-2H-P-1" => vec!["sidecar::checked_sidecar_total_blocks"],
        "overflow-9.1-block-locator" => vec!["parity_block_position"],
        "overflow-10.1.2-M" | "overflow-10.1.2-2M-footer" => {
            vec!["replicated_control::checked_replicated_control_layout (ParityMap error boundary)"]
        }
        "overflow-10.4-slot-product" => vec!["checked_tape_index_payload_len"],
        "overflow-10.4-record-geometry" => vec!["checked_tape_index_replica_layout"],
        "overflow-10.3-manifest-range" => {
            vec!["object_recovery::validate_object_recovery_row_fields (replica payload boundary)"]
        }
        "overflow-10.4-footer-delta" if variant == "isolated-1" => vec![
            "tape_index_replica::checked_replica_footer_position",
            "index_separation::checked_separation_footer_position",
        ],
        "overflow-10.4-footer-delta" => vec!["tape_index_replica::checked_replica_footer_position"],
        "overflow-10.5-extent-geometry" => vec!["index_separation::checked_index_separation_bytes"],
        _ => return Err(format!("no public formula resolver for {parent}/{variant}")),
    };
    Ok(names.into_iter().map(str::to_string).collect())
}
#[cfg(test)]
fn unit(v: &Resolved, separation: bool) -> Result<(), ObservedError> {
    let (p, variant) = v.path.split_once('/').unwrap();
    match p {
        "overflow-8.3-component-starts" | "overflow-8.3-eod" => {
            let mut l = TerminalTailLayout::new(0, BLOCK, 1, 2, 3, 3).unwrap();
            if p.ends_with("starts") {
                l.components[0].planned_start_lba = u64::MAX - 2;
                for i in 1..5 {
                    l.components[i].planned_start_lba = 2 + 4 * (i as u64 - 1);
                }
                l.expected_eod_lba = 18;
            } else {
                for i in 0..5 {
                    l.components[i].planned_start_lba = u64::MAX - 19 + 4 * i as u64;
                }
                l.expected_eod_lba = 0;
            }
            l.validate().map_err(TapeIndexReplicaError::from)?;
        }
        "overflow-9.1-P" | "overflow-9.2-SxK" => {
            sidecar::checked_sidecar_shard_product(1 << 63, 2)?;
        }
        "overflow-9.1-total-2H-P-1" => {
            sidecar::checked_sidecar_total_blocks(1 << 63, 4)?;
        }
        "overflow-9.1-block-locator" => {
            parity_block_position(0, 1, 2, 2, u64::MAX)?;
        }
        "overflow-10.1.2-M" => {
            let (b, l) = (u64::from(BLOCK), u64::MAX);
            replicated_control::checked_replicated_control_layout(b, 0xc8, l, "parity-map")
                .map_err(|e| ParityError::ParityMapParse(e.to_string()))?;
        }
        "overflow-10.4-slot-product" | "overflow-10.4-record-geometry" => {
            let (s, o) = if p.ends_with("geometry") {
                ((1 << 58) - 1, 0)
            } else {
                match variant {
                    "isolated-1" => (1 << 58, 0),
                    "isolated-2" => ((1 << 56) + 1, 1 << 56),
                    "isolated-3" => (1 << 57, 1 << 55),
                    _ => unreachable!(),
                }
            };
            let counts = TapeIndexReplicaCounts {
                structural_entry_count: s,
                object_row_count: o,
            };
            if p.ends_with("geometry") {
                checked_tape_index_replica_layout(BLOCK, counts)?;
            } else {
                checked_tape_index_payload_len(counts)?;
            }
        }
        "overflow-10.3-manifest-range" => {
            object_recovery::validate_object_recovery_row_fields(
                1 << 46,
                Some(b"unit"),
                &ObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: 0,
                    manifest_size_bytes: 1,
                    manifest_chunk_count: 1 << 46,
                    manifest_sha256: [1; 32],
                },
                Some(BLOCK),
            )
            .map_err(|e| TapeIndexReplicaError::Payload {
                message: e.to_string(),
            })?;
        }
        "overflow-10.4-footer-delta" => {
            let (start, count) = if variant == "isolated-1" {
                (0, 0)
            } else {
                (u64::MAX, 3)
            };
            if separation {
                index_separation::checked_separation_footer_position(start, count)?;
            } else {
                tape_index_replica::checked_replica_footer_position(start, count)?;
            }
        }
        "overflow-10.5-extent-geometry" => {
            index_separation::checked_index_separation_bytes(BLOCK, u64::MAX)?;
        }
        _ => unreachable!(),
    };
    Ok(())
}

/// Faults are commands through ChaosTransport; the model's blocks remain exact.
#[cfg(test)]
fn drive(v: &Resolved) -> (DriveHandle, FaultEngine) {
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK);
    let mut starts = BTreeMap::new();
    for (&f, blocks) in &v.files {
        starts.insert(f, tape.records.len() as u64);
        tape.records
            .extend(blocks.iter().cloned().map(Record::Block));
        tape.records.push(Record::Filemark);
    }
    let lbas = v.descriptor["faults"].as_array().unwrap().iter().map(|f| {
        starts[&(f["tape_file"].as_u64().unwrap() as usize)] + f["block"].as_u64().unwrap()
    });
    let engine = FaultEngine::for_read_medium_errors(lbas.collect::<Vec<_>>()).unwrap();
    let mut world = VirtualWorld::single_drive("SUPPLEMENT", 0x100, "SUPPLEMENT", 0x400, 1);
    world.put_tape_in_drive(0x100, "SUPP001", None, tape);
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
            Path::new("/dev/sg-supplement-model"),
            Box::new(transport),
        )
        .unwrap(),
        engine,
    )
}
#[cfg(test)]
fn recover(v: &Resolved) -> Result<(), ObservedError> {
    let base = generate("a4-minimal").unwrap();
    let mut rows = base.written.map.entries().to_vec();
    let real = if v.path == "sidecar-real-data-shard-count/isolated-2" {
        5
    } else {
        4
    };
    rows[1].block_count = real;
    rows[2].block_count = v.files[&2].len() as u64;
    rows[2].protected_ordinal_end_exclusive = Some(real);
    let map = ScopedFilemarkMap::from_catalog(FilemarkMap::new(rows)?, real);
    let (mut d, engine) = drive(v);
    let result = recover_ordinal_from_sidecar(
        &mut DriveHandleRawSource::new(&mut d),
        &map,
        &base.written.inputs.scheme,
        v.uuid,
        BLOCK,
        0,
    );
    assert_faults(v, &engine);
    result?;
    Ok(())
}
#[cfg(test)]
fn scanner(v: &Resolved) -> Result<(), ObservedError> {
    let (mut d, engine) = drive(v);
    let result = scan_reconstruct_filemark_map_with_report(
        &mut DriveHandleRawSource::new(&mut d),
        &v.uuid,
        BLOCK,
    );
    // This Scanner can classify a sidecar from its primary alone. Report
    // the unvisited fault as a coverage gap, never fabricate a medium read.
    println!(
        "FAULT {}: configured={}; observed_medium_lbas={:?}",
        v.path,
        v.descriptor["faults"],
        engine.observed_medium_error_lbas()
    );
    result?;
    Ok(())
}
#[cfg(test)]
fn resume(v: &Resolved) -> Result<(), ObservedError> {
    let base = generate("unfinalized-open").unwrap();
    let input = &v.descriptor["commit_record_inputs"];
    let temp = tempfile::tempdir().expect("journal temp directory");
    let j = crate::resume_vectors::adapt(input, &base, &temp.path().join("claims"))?;
    let snapshot = resume_record_result(j.committed_snapshot_bounded())?;
    checked_bounded_resume_summary(&snapshot)?;
    Ok(())
}

#[cfg(test)]
fn observation(
    location: String,
    key: &'static str,
    run: impl FnOnce() -> Result<(), ObservedError> + std::panic::UnwindSafe,
) -> Observation {
    let (name, detail, outcome) = match std::panic::catch_unwind(run) {
        Err(e) => (
            "PANIC",
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "non-string panic".into()),
            "PANIC".into(),
        ),
        Ok(Ok(())) => ("ACCEPTED", "Ok(())".into(), "ACCEPTED".into()),
        Ok(Err(e)) => {
            let name = section15(&e);
            let outcome = if let ObservedError::Parity(ParityError::SidecarMetadataUnavailable {
                epoch_id,
            }) = &e
            {
                format!("SidecarMetadataUnavailable{{epoch_id: {epoch_id}}}")
            } else {
                name.into()
            };
            (name, e.to_string(), outcome)
        }
    };
    Observation {
        location,
        key,
        name,
        detail,
        outcome,
    }
}
#[cfg(test)]
fn observations(v: &Resolved) -> Vec<Observation> {
    if v.descriptor["artifact"] == "unit level" {
        let mut out = vec![observation("unit".into(), "error", || unit(v, false))];
        if v.path == "overflow-10.4-footer-delta/isolated-1" {
            out.push(observation(
                "unit separation".into(),
                "separation_footer",
                || unit(v, true),
            ));
        }
        return out;
    }
    if v.path == "sidecar-primary-tail-disagreement/isolated" {
        return vec![observation(
            "BOT Scanner with footer medium error".into(),
            "per_sidecar_validation",
            || scanner(v),
        )];
    }
    if v.path == "overflow-3.2-14-append-point/isolated" {
        return vec![observation(
            "Resumer commit records".into(),
            "error",
            || resume(v),
        )];
    }
    if v.path.starts_with("sidecar-") {
        let h = u64_at(&v.files[&2][0], 0x60) as usize;
        let tail = if v.path == "sidecar-header-block-count/isolated" {
            6
        } else if v.path == "sidecar-m-zero/isolated" {
            1
        } else {
            5
        };
        let mut out = vec![];
        for b in [0, tail] {
            out.push(observation(
                format!("file=2 copy block={b}"),
                "per_copy",
                || {
                    parse_sidecar_index_blocks(&v.files[&2][b..b + h], &v.uuid)?;
                    Ok(())
                },
            ));
        }
        out.push(observation("Recoverer".into(), "recoverer", || recover(v)));
        return out;
    }
    if v.path.starts_with("overflow-13.3-tail-location/") {
        return vec![observation(
            "Recoverer directory rescue".into(),
            "error",
            || recover(v),
        )];
    }
    let mut out = observe(v);
    if v.path == "overflow-9.1-tail-start/isolated" {
        out.push(observation("Recoverer".into(), "recoverer", || recover(v)));
    }
    out
}

/// Check integrity, copies, physical layout and dependants independently of
/// the observed error. Target fields and explicitly unreadable blocks are the
/// only exclusions; an unaccounted construction error stops that variant.
fn assert_construction(v: &Resolved) -> Result<(), String> {
    if v.descriptor["artifact"] == "unit level" {
        return Ok(());
    }
    let fail = |s: &str| Err(format!("{}: isolation construction: {s}", v.path));
    if v.path.starts_with("sidecar-") {
        let b = &v.files[&2];
        let h = if v.path == "sidecar-header-block-count/isolated" {
            2
        } else {
            1
        };
        let p = if v.path == "sidecar-m-zero/isolated" {
            0
        } else {
            4
        };
        let tail = h + p;
        let footer = 2 * h + p;
        let fault = v.descriptor["faults"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["block"] == footer);
        for start in [0, tail] {
            let c = &b[start];
            if u64_at(c, 0xc0) != crc64_xz(&c[..0xc0]) || u64_at(c, B - 8) != crc64_xz(&c[..B - 8])
            {
                return fail("header CRC");
            }
            let real = if v.path == "sidecar-real-data-shard-count/isolated-1" {
                3
            } else if v.path == "sidecar-real-data-shard-count/isolated-2" {
                5
            } else {
                4
            };
            // Evaluate each non-target field rule from the actual copy bytes.
            let parent = v.path.split('/').next().unwrap();
            let k = u16::from_le_bytes(c[0x20..0x22].try_into().unwrap()) as u64;
            let m = u16::from_le_bytes(c[0x22..0x24].try_into().unwrap()) as u64;
            let stripes = u32::from_le_bytes(c[0x24..0x28].try_into().unwrap()) as u64;
            let recorded_b = u32::from_le_bytes(c[0x28..0x2c].try_into().unwrap()) as u64;
            let layout = checked_sidecar_index_capacity_layout(recorded_b, p as u64, real as u64)
                .map_err(|e| e.to_string())?;
            let checks = [
                ("sidecar-block-size", recorded_b == B as u64),
                ("sidecar-m-zero", k > 0 && m > 0 && stripes > 0),
                (
                    "sidecar-logical-shard-count",
                    u64_at(c, 0x40) == stripes * k,
                ),
                (
                    "sidecar-real-data-shard-count",
                    u64_at(c, 0x48) == u64_at(c, 0x38) - u64_at(c, 0x30)
                        && u64_at(c, 0x48) > 0
                        && u64_at(c, 0x48) <= stripes * k,
                ),
                ("sidecar-data-crc-count", u64_at(c, 0x58) == u64_at(c, 0x48)),
                ("sidecar-parity-block-count", u64_at(c, 0x50) == stripes * m),
                (
                    "sidecar-header-block-count",
                    u64_at(c, 0x60) == layout.block_count,
                ),
                (
                    "sidecar-inline-index-bytes",
                    u64_at(c, 0x68) == layout.inline_entry_bytes,
                ),
                (
                    "sidecar-total-block-count",
                    u64_at(c, 0x70) == 2 * h as u64 + stripes * m + 1,
                ),
                ("sidecar-primary-start", u64_at(c, 0x78) == 0),
                (
                    "sidecar-tail-start",
                    u64_at(c, 0x80) == h as u64 + stripes * m,
                ),
                (
                    "sidecar-footer-index",
                    u64_at(c, 0x88) == 2 * h as u64 + stripes * m,
                ),
                ("sidecar-copy-generation", c[0x94..0x98] == [0; 4]),
                ("sidecar-copy-kind-reserved", c[0x92..0x94] == [0; 2]),
                ("sidecar-header-reserved", c[0xb8..0xc0] == [0; 8]),
                ("sidecar-tape-uuid", c[0x08..0x18] == v.uuid),
            ];
            for (rule, holds) in checks {
                if rule != parent && !holds {
                    return fail(&format!("non-target rule {rule}"));
                }
            }
            if u64_at(c, 0x70) != b.len() as u64 {
                return fail("measured sidecar length");
            }
            if c[..8] != derive_sidecar_magic(&v.uuid) {
                return fail("magic");
            }
            if u16::from_le_bytes(c[0x90..0x92].try_into().unwrap())
                != if start == 0 { 1 } else { 2 }
            {
                return fail("copy kind / position");
            }
            for parity in 0..m as usize {
                for stripe in 0..stripes as usize {
                    let offset = 0xc8 + (stripe * m as usize + parity) * 16;
                    if u32::from_le_bytes(c[offset..offset + 4].try_into().unwrap()) as usize
                        != stripe
                        || u16::from_le_bytes(c[offset + 4..offset + 6].try_into().unwrap())
                            as usize
                            != parity
                    {
                        return fail("parity entry order");
                    }
                    if u64_at(c, offset + 8)
                        != parity_shard_crc64(&b[h + parity * stripes as usize + stripe])
                    {
                        return fail("parity entry CRC / physical position");
                    }
                    if parent != "sidecar-header-reserved" && c[offset + 6..offset + 8] != [0; 2] {
                        return fail("index reserved");
                    }
                }
            }
            let end = 0xc8 + 16 * p + 8 * real;
            let mut hash = Sha256::new();
            hash.update(b"remanence-sidecar-metadata-v1");
            let mut fields = c[..0x90].to_vec();
            if v.path == "sidecar-primary-start/isolated-2" {
                fields[0x78..0x80].fill(0);
            }
            hash.update(fields);
            hash.update(&c[0xc8..end]);
            let mut digest = hash.finalize().to_vec();
            if v.path == "sidecar-canonical-hash/isolated" {
                digest[0] ^= 1;
            }
            if c[0x98..0xb8] != digest {
                return fail("canonical hash (with explicitly declared hash mutation/reading)");
            }
            if c[end..B - 8].iter().any(|&n| n != 0) {
                return fail("index zero fill");
            }
            for spill in &b[start + 1..start + h] {
                if spill[..B - 8].iter().any(|&n| n != 0)
                    || u64_at(spill, B - 8) != crc64_xz(&spill[..B - 8])
                {
                    return fail("empty spill CRC/fill");
                }
            }
        }
        if v.path != "sidecar-primary-tail-disagreement/isolated" {
            for range in [0..0x90, 0x92..0xc0, 0xc8..B - 8] {
                if b[0][range.clone()] != b[tail][range] {
                    return fail("primary/tail agreement");
                }
            }
        } else {
            parse_sidecar_index_blocks(&b[0..1], &v.uuid).map_err(|e| e.to_string())?;
            parse_sidecar_index_blocks(&b[tail..tail + 1], &v.uuid).map_err(|e| e.to_string())?;
        }
        if !fault {
            if u64_at(&b[footer], 0x80) != crc64_xz(&b[footer][..0x80]) {
                return fail("footer CRC");
            }
            if b[footer][0x60..0x80] != b[0][0x98..0xb8] {
                return fail("footer hash mirror");
            }
            for (fo, ho) in [
                (0x28, 0x30),
                (0x30, 0x38),
                (0x38, 0x60),
                (0x40, 0x50),
                (0x48, 0x70),
                (0x50, 0x78),
                (0x58, 0x80),
            ] {
                if b[footer][fo..fo + 8] != b[0][ho..ho + 8] {
                    return fail("footer field mirror");
                }
            }
        } else {
            // ISO-FAULT-FOOTER changes no footer byte, including its hash.
            let (base, _) = base_artifact("tape-image a4-minimal")?;
            if b[footer] != base[&2][6] {
                return fail("unreadable footer was byte-mutated");
            }
        }
        let pm = parse_parity_map_tape_file(&v.files[&3], &v.uuid).map_err(|e| e.to_string())?;
        let d = &pm.payload.directory.entries[0];
        if d.canonical_metadata_hash != b[0][0x98..0xb8]
            || d.sidecar_total_block_count != b.len() as u64
            || d.sidecar_header_block_count != h as u64
        {
            return fail("directory mirrors");
        }
        for f in [4, 6, 8] {
            records(&v.files, f, &v.uuid)?;
        }
        for f in [5, 7] {
            let a = parse_index_separation_header(&v.files[&f][0], &v.uuid)
                .map_err(|e| e.to_string())?;
            let z = parse_index_separation_footer(&v.files[&f][2], &v.uuid)
                .map_err(|e| e.to_string())?;
            validate_index_separation_pair(&a, &z).map_err(|e| e.to_string())?;
        }
        // Physical dependent map and scope were regenerated, not just resigned.
        let rows = records(&v.files, 4, &v.uuid)?;
        if rows.entries[2].block_count != b.len() as u64 {
            return fail("terminal sidecar block count");
        }
        let terminal =
            parse_tape_index_replica_header(&v.files[&4][0], &v.uuid).map_err(|e| e.to_string())?;
        let actual_start: u64 = v.files.range(..4).map(|(_, b)| b.len() as u64 + 1).sum();
        if terminal.plan.component.planned_start_lba != actual_start
            || terminal.plan.edition.canonical_map_sha256 != pm.payload.canonical_map_digest
        {
            return fail("terminal placement/map digest");
        }
    }
    if (v.path.starts_with("terminal-scope-scalars/")
        && v.path != "terminal-scope-scalars/isolated-2")
        || v.path == "overflow-7.2-T/isolated"
    {
        for f in [7, 9] {
            let h = parse_index_separation_header(&v.files[&f][0], &v.uuid)
                .map_err(|e| e.to_string())?;
            let t = parse_index_separation_footer(&v.files[&f][2], &v.uuid)
                .map_err(|e| e.to_string())?;
            validate_index_separation_pair(&h, &t).map_err(|e| e.to_string())?;
        }
    }
    if v.path.starts_with("overflow-13.3-tail-location/") {
        let (base, _) = base_artifact("tape-image a4-minimal")?;
        if v.files[&2] != base[&2] {
            return fail("fault-only sidecar bytes changed");
        }
        parse_parity_map_tape_file(&v.files[&3], &v.uuid).map_err(|e| e.to_string())?;
    }
    if v.path == "overflow-9.1-tail-start/isolated" {
        let (base, _) = base_artifact("tape-image a4-minimal")?;
        for b in [0, 5] {
            if v.files[&2][b] != base[&2][b] {
                return fail("fault-only header bytes changed");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn expected_matches(expected: &Value, o: &Observation) -> bool {
    if let Some(s) = expected.as_str() {
        return s == o.outcome || (s.split('{').next() == Some(o.name) && !s.contains("epoch_id"));
    }
    if let Some(a) = expected["one_of"].as_array() {
        return a.iter().any(|e| expected_matches(e, o));
    }
    if !expected["error"].is_null() {
        return expected_matches(&expected["error"], o);
    }
    expected["outcome"]
        .as_str()
        .is_some_and(|s| s.starts_with("rejection"))
        && !matches!(
            o.name,
            "ACCEPTED" | "PANIC" | "RETURNED" | "UNMAPPED" | "Invariant"
        )
}
/// Map every supplement `violates_only` rule to its specific reference diagnostic.
/// Full rule messages distinguish formulae and reserved-field sites; numeric values
/// are fixed by the authored recipes. Wrapper errors may contain these messages.
/// None records the missing agreement check tracked by the open adjudication.
#[cfg(test)]
const ISOLATION_RULES: &[(&str, Option<&str>)] = &[
    ("sidecar-block-size/isolated", Some("sidecar parse error: sidecar block_size 524288 does not match block0 length 262144")),
    ("sidecar-canonical-hash/isolated", Some("sidecar parse error: sidecar canonical metadata hash mismatch")),
    ("sidecar-copy-generation/isolated", Some("sidecar parse error: unsupported sidecar copy_generation: 1")),
    ("sidecar-copy-kind-reserved/isolated", Some("sidecar parse error: sidecar copy-kind reserved field non-zero: 0x0001")),
    ("sidecar-data-crc-count/isolated", Some("sidecar parse error: sidecar data_crc_count 3 != real data count 4")),
    ("sidecar-footer-index/isolated", Some("sidecar parse error: sidecar footer_block_index 7 != expected 6")),
    ("sidecar-header-block-count/isolated", Some("sidecar parse error: sidecar shard_index_block_count 2 != expected 1")),
    ("sidecar-header-reserved/isolated-1", Some("sidecar parse error: sidecar header reserved field non-zero: 0x0000000000000001")),
    ("sidecar-header-reserved/isolated-2", Some("sidecar parse error: sidecar parity index reserved field non-zero: 0x0001")),
    ("sidecar-inline-index-bytes/isolated", Some("sidecar parse error: sidecar inline_index_entry_bytes 88 != expected 96")),
    ("sidecar-m-zero/isolated", Some("sidecar parse error: sidecar k, m, and S must all be non-zero")),
    ("sidecar-logical-shard-count/isolated", Some("sidecar parse error: sidecar logical_shard_count 5 != S*k 4")),
    ("sidecar-parity-block-count/isolated", Some("sidecar parse error: sidecar parity_block_count 3 != S*m 4")),
    ("sidecar-primary-start/isolated-1", Some("sidecar parse error: sidecar primary_header_start_block 1 != 0")),
    ("sidecar-primary-start/isolated-2", Some("sidecar parse error: sidecar primary_header_start_block 1 != 0")),
    ("sidecar-real-data-shard-count/isolated-1", Some("sidecar parse error: sidecar real_data_shard_count 3 != range length 4")),
    ("sidecar-real-data-shard-count/isolated-2", Some("sidecar parse error: sidecar real data count exceeds logical shard count")),
    ("sidecar-tail-start/isolated", Some("sidecar parse error: sidecar tail_header_start_block 6 != expected 5")),
    ("sidecar-tape-uuid/isolated", Some("sidecar parse error: sidecar tape UUID mismatch")),
    ("sidecar-total-block-count/isolated", Some("sidecar parse error: sidecar_total_block_count 8 != expected 7")),
    ("sidecar-primary-tail-disagreement/isolated", None),
    ("paritymap-directory-not-ascending/isolated", Some("directory sidecar entries must be in ascending tape-file order")),
    ("paritymap-footer-header-disagreement/isolated", Some("parity-map parse error: both parity-map metadata copies failed: primary=parity-map parse error: parity-map header copy does not match footer locator; tail=parity-map parse error: parity-map header copy does not match footer locator")),
    ("terminal-scope-scalars/isolated-1", Some("terminal index covered_prefix_tape_file_count 6, expected 7")),
    ("terminal-scope-scalars/isolated-2", Some("terminal index total data ordinals 0, expected 1")),
    ("terminal-scope-scalars/isolated-3", Some("terminal index payload authority failed: terminal parity closeout must protect every Object ordinal before replica A")),
    ("terminal-scope-scalars/isolated-4", Some("terminal index highest protected ordinal 2, expected 5")),
    ("overflow-3.2-14-append-point/isolated", Some("bounded resume append position overflows")),
    ("overflow-7.2-T/isolated", Some("terminal tape-index arithmetic overflow: Object data ordinal range")),
    ("overflow-8.3-component-starts/isolated", Some("invalid terminal layout for tape index: terminal-tail arithmetic overflow: next component start LBA")),
    ("overflow-8.3-eod/isolated", Some("invalid terminal layout for tape index: terminal-tail arithmetic overflow: next component start LBA")),
    ("overflow-9.1-P/isolated", Some("sidecar parse error: sidecar shard product overflows")),
    ("overflow-9.1-total-2H-P-1/isolated", Some("sidecar parse error: sidecar total block count overflows")),
    ("overflow-9.1-block-locator/isolated", Some("sidecar parse error: sidecar parity block position overflows u64")),
    ("overflow-9.1-tail-start/isolated", Some("sidecar parse error: sidecar tail header start overflows")),
    ("overflow-9.2-SxK/isolated", Some("sidecar parse error: sidecar shard product overflows")),
    ("overflow-10.1.2-M/isolated", Some("parity-map parse error: parity-map replicated-control layout error: header plus payload length overflows u64")),
    ("overflow-10.4-slot-product/isolated-1", Some("terminal index payload authority failed: terminal tape-index replica error: structural slot byte count overflows u64")),
    ("overflow-10.4-slot-product/isolated-2", Some("Object-row slot byte count overflows u64")),
    ("overflow-10.4-slot-product/isolated-3", Some("terminal index payload authority failed: terminal tape-index replica error: tape-index payload length overflows u64")),
    ("overflow-10.4-record-geometry/isolated", Some("terminal tape-index arithmetic overflow: payload record byte capacity")),
    ("overflow-10.3-manifest-range/isolated-1", Some("terminal index payload authority failed: terminal tape-index replica error: terminal tape-index replica error: plaintext manifest chunk range overflows")),
    ("overflow-10.3-manifest-range/isolated-2", Some("terminal index payload authority failed: terminal tape-index replica error: plaintext manifest byte capacity overflows")),
    ("overflow-10.4-footer-delta/isolated-1", Some("arithmetic overflow: footer backward delta")),
    ("overflow-10.4-footer-delta/isolated-2", Some("terminal tape-index arithmetic overflow: observed footer LBA")),
    ("overflow-10.5-extent-geometry/isolated", Some("index separation arithmetic overflow: actual extent bytes")),
    // The reference never evaluates total - 1 - H (see isolation-exceptions.json).
    ("overflow-13.3-tail-location/isolated-1", None),
    // The reference never evaluates total - 1 - H (see isolation-exceptions.json).
    ("overflow-13.3-tail-location/isolated-2", None),
];

#[cfg(test)]
fn detail_matches(path: &str, o: &Observation) -> bool {
    let rule = ISOLATION_RULES
        .iter()
        .find(|(id, _)| *id == path)
        .expect("every supplement variant has an isolation rule")
        .1;
    // Recoverer aggregates failed copies. The per-copy observations check the
    // target rule; acquisition itself reports only this epoch-level outcome.
    if o.key == "recoverer" && o.name == "SidecarMetadataUnavailable" {
        return true;
    }
    rule.is_some_and(|rule| o.detail.contains(rule))
}

/// Similar field names and overflow categories must not hide a different rule.
#[test]
fn isolation_rule_matching_is_specific() {
    let o = |detail: &str| Observation {
        key: "error",
        name: "SidecarParse",
        outcome: "SidecarParse".into(),
        location: "policy unit test".into(),
        detail: detail.into(),
    };
    assert!(detail_matches(
        "sidecar-copy-kind-reserved/isolated",
        &o("sidecar parse error: sidecar copy-kind reserved field non-zero: 0x0001")
    ));
    assert!(!detail_matches(
        "sidecar-copy-kind-reserved/isolated",
        &o("sidecar parse error: sidecar parity index reserved field non-zero: 0x0001")
    ));
    assert!(!detail_matches(
        "overflow-8.3-eod/isolated",
        &o("terminal-tail arithmetic overflow: dense terminal tape-file number")
    ));
    assert!(!detail_matches(
        "overflow-10.4-record-geometry/isolated",
        &o("terminal tape-index arithmetic overflow: payload record ceiling division")
    ));
    assert!(detail_matches(
        "overflow-10.4-record-geometry/isolated",
        &o("terminal tape-index arithmetic overflow: payload record byte capacity")
    ));
}

#[cfg(test)]
pub(super) fn execute(
    root: &Path,
    manifest: &mut String,
    failures: &mut Vec<String>,
    adjudications: &Adjudications,
    seen_adjudications: &mut BTreeSet<(String, String, String)>,
) {
    let source = source().expect("supplement source");
    let exceptions = parse_isolation_exceptions(
        &fs::read(root.join("isolation-exceptions.json")).expect("supervisor isolation exceptions"),
    )
    .expect("valid isolation exceptions");
    let mut seen_exceptions = BTreeSet::new();
    let variants = source["variants"].as_array().expect("supplement variants");
    assert_eq!(ISOLATION_RULES.len(), variants.len());
    assert_eq!(
        ISOLATION_RULES
            .iter()
            .map(|(id, _)| *id)
            .collect::<BTreeSet<_>>(),
        variants.iter().map(|v| v["id"].as_str().unwrap()).collect()
    );
    for entry in source["not_isolatable"]
        .as_array()
        .expect("not_isolatable array")
    {
        println!(
            "NOT-ISOLATABLE {}: {}",
            entry["case"].as_str().unwrap(),
            entry["reason"].as_str().unwrap()
        );
    }
    for variant in source["variants"].as_array().expect("supplement variants") {
        let path = variant["id"].as_str().unwrap();
        let v = match resolve(variant) {
            Ok(v) => v,
            Err(e) => {
                println!("UNRESOLVED {path}: {e}");
                failures.push(path.into());
                continue;
            }
        };
        for (name, wanted) in [
            ("expected.json", &v.expected),
            ("mutation.json", &v.descriptor),
        ] {
            let actual: Value = serde_json::from_slice(
                &fs::read(root.join(path).join(name)).expect("generated supplement file"),
            )
            .unwrap();
            assert_eq!(&actual, wanted, "{path}/{name}");
        }
        manifest.push_str(&v.manifest);
        println!("ROLE {path}: {}", v.descriptor["entry_points"]);
        println!(
            "ISOLATION {path}: construction assertions passed; violates_only={}",
            variant["violates_only"]
        );
        for o in observations(&v) {
            let (id, variant_name) = path.split_once('/').expect("supplement variant path");
            let expected = if !variant["expected"][o.key].is_null() {
                &variant["expected"][o.key]
            } else {
                &variant["expected"]
            };
            if adjudications.check_observation(
                (id.into(), variant_name.into(), o.key.into()),
                path,
                &o,
                expected,
                seen_adjudications,
                failures,
            ) {
                continue;
            }
            let matches = expected_matches(expected, &o);
            let pinned = variant["pinned"].as_bool().unwrap();
            let failed = o.name == "PANIC"
                || (!matches && pinned)
                || (v.descriptor["artifact"] == "unit level" && !matches);
            let status = if failed {
                "DISAGREEMENT"
            } else if !pinned {
                "INFORMATIVE"
            } else {
                "PASS"
            };
            println!(
                "{status} {path} [{}]: expected {expected}; observed {}; {}; detail={:?}",
                o.key, o.outcome, o.location, o.detail
            );
            let isolation = matches && !detail_matches(path, &o);
            match exceptions.check(path, o.key, isolation, &mut seen_exceptions) {
                Ok(Some(status)) => println!(
                    "{status} {path} [{}]: {}; target={}; detail={:?}",
                    o.key, o.location, variant["target"], o.detail
                ),
                Ok(None) => {}
                Err(error) => {
                    println!(
                        "{error}: {}; target={}; detail={:?}",
                        o.location, variant["target"], o.detail
                    );
                    failures.push(error);
                }
            }
            if failed {
                failures.push(format!("{path} [{}]", o.key));
            }
        }
    }
    if let Err(error) = exceptions.ensure_all_observed(&seen_exceptions) {
        println!("{error}");
        failures.push(error);
    }
}

#[cfg(test)]
fn assert_faults(v: &Resolved, engine: &FaultEngine) {
    let expected: BTreeSet<u64> = v.descriptor["faults"]
        .as_array()
        .unwrap()
        .iter()
        .map(|fault| {
            let f = fault["tape_file"].as_u64().unwrap() as usize;
            v.files
                .range(..f)
                .map(|(_, blocks)| blocks.len() as u64 + 1)
                .sum::<u64>()
                + fault["block"].as_u64().unwrap()
        })
        .collect();
    println!(
        "FAULT {}: configured_lbas={expected:?}; observed_medium_lbas={:?}",
        v.path,
        engine.observed_medium_error_lbas()
    );
    assert_eq!(
        engine.observed_medium_error_lbas(),
        expected,
        "{}: every prescribed medium fault must actually fire",
        v.path
    );
}
