//! Rebuild the candidate terminal profiles from their recorded writer inputs.
//! Byte equality with the existing components is a precondition for mutation.
use super::*;

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
fn n(v: &Value, key: &str) -> u64 {
    v[key].as_u64().expect("recorded integer")
}
fn fixed<const N: usize>(v: &Value) -> [u8; N] {
    unhex(v.as_str().expect("hex string"))
        .try_into()
        .expect("fixed hex width")
}

pub(super) fn build(profile: &str) -> Result<BTreeMap<usize, Vec<Vec<u8>>>, String> {
    let root = fixture_root().join(profile);
    let input: Value =
        serde_json::from_slice(&fs::read(root.join("inputs.json")).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let mut records = Records {
        entries: input["structural_entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| TapeIndexReplicaMapEntry {
                tape_file_number: n(e, "tape_file_number"),
                block_count: n(e, "block_count"),
                kind: match e["kind"].as_str().unwrap() {
                    "Bootstrap" => TapeIndexReplicaFileKind::Bootstrap,
                    "Object" => TapeIndexReplicaFileKind::Object,
                    "ParitySidecar" => TapeIndexReplicaFileKind::ParitySidecar,
                    "ParityMap" => TapeIndexReplicaFileKind::ParityMap,
                    other => panic!("unknown kind {other}"),
                },
                first_parity_data_ordinal: e["first_parity_data_ordinal"].as_u64(),
                protected_ordinal_start: e["protected_ordinal_start"].as_u64(),
                protected_ordinal_end_exclusive: e["protected_ordinal_end_exclusive"].as_u64(),
                epoch_id: e["epoch_id"].as_u64(),
            })
            .collect(),
        rows: input["object_rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let rep = &r["representation"];
                TapeIndexReplicaObjectRow {
                    tape_file_number: n(r, "tape_file_number"),
                    stored_block_count: n(r, "stored_block_count"),
                    object_id: unhex(r["object_id"].as_str().unwrap()),
                    representation: match rep["kind"].as_str().unwrap() {
                        "Plaintext" => ObjectRecoveryRepresentation::Plaintext {
                            manifest_first_chunk_lba: n(rep, "manifest_first_chunk_lba"),
                            manifest_size_bytes: n(rep, "manifest_size_bytes"),
                            manifest_chunk_count: n(rep, "manifest_chunk_count"),
                            manifest_sha256: fixed(&rep["manifest_sha256"]),
                        },
                        "Encrypted" => ObjectRecoveryRepresentation::Encrypted {
                            recipient_epoch_ids: rep["recipient_epoch_ids"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(fixed)
                                .collect(),
                            metadata_frame_len: n(rep, "metadata_frame_len"),
                            key_frame_len: n(rep, "key_frame_len").try_into().unwrap(),
                        },
                        other => panic!("unknown representation {other}"),
                    },
                }
            })
            .collect(),
    };
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: n(&input["counts"], "structural_entry_count"),
        object_row_count: n(&input["counts"], "object_row_count"),
    };
    let scope = TapeIndexReplicaScope {
        covered_prefix_tape_file_count: n(&input["scope"], "covered_prefix_tape_file_count"),
        total_data_ordinals: n(&input["scope"], "total_data_ordinals"),
        highest_protected_ordinal: n(&input["scope"], "highest_protected_ordinal"),
    };
    let block_size = n(&input, "block_size") as u32;
    let layout =
        checked_tape_index_replica_layout(block_size, counts).map_err(|e| e.to_string())?;
    let prefix_end = records.entries.iter().map(|e| e.block_count + 1).sum();
    let terminal = TerminalTailLayout::new(
        0,
        block_size,
        counts.structural_entry_count,
        prefix_end,
        layout.replica_record_count,
        n(&input["separation_extent"], "total_records"),
    )
    .map_err(|e| e.to_string())?;
    let plan = assemble_terminal_plan(
        fixed(&input["tape_uuid"]),
        block_size,
        input["compression_enabled"].as_bool().unwrap(),
        n(&input, "edition_sequence"),
        scope,
        counts,
        &mut records,
        terminal,
        ParityMapDiagnostics {
            writer_version: input["diagnostics"]["writer_version"]
                .as_str()
                .unwrap()
                .into(),
            write_timestamp: input["diagnostics"]["write_timestamp"]
                .as_str()
                .unwrap()
                .into(),
        },
        fixed(&input["edition_id"]),
        n(&input["separation_extent"], "nominal_extent_bytes"),
    )
    .map_err(|e| e.to_string())?;
    let mut files = BTreeMap::new();
    for (i, replica) in plan.replicas.iter().enumerate() {
        let c = replica.component;
        let mut blocks = Vec::new();
        write_tape_index_replica(
            replica,
            TapeIndexReplicaObservation {
                tape_file_number: c.planned_tape_file_number,
                start_lba: c.planned_start_lba,
                record_count: c.record_count,
            },
            &mut records,
            |block| {
                blocks.push(block.to_vec());
                Ok(())
            },
        )
        .map_err(|e| e.to_string())?;
        let name = format!("replica-{}.bin", ['a', 'b', 'c'][i]);
        if blocks.concat() != fs::read(root.join(&name)).map_err(|e| e.to_string())? {
            return Err(format!("rebuilt {profile}/{name} differs"));
        }
        files.insert(c.planned_tape_file_number as usize, blocks);
    }
    for (i, separation) in plan.separations.iter().enumerate() {
        let c = separation.component;
        let mut blocks = Vec::new();
        write_index_separation(
            separation,
            IndexSeparationObservation {
                tape_file_number: c.planned_tape_file_number,
                start_lba: c.planned_start_lba,
                record_count: c.record_count,
            },
            |block| {
                blocks.push(block.to_vec());
                Ok(())
            },
        )
        .map_err(|e| e.to_string())?;
        let name = if i == 0 { "gap-ab.bin" } else { "gap-bc.bin" };
        if blocks.concat() != fs::read(root.join(name)).map_err(|e| e.to_string())? {
            return Err(format!("rebuilt {profile}/{name} differs"));
        }
        files.insert(c.planned_tape_file_number as usize, blocks);
    }
    Ok(files)
}

/// The recorded prefix's record counts, one per tape file before replica A,
/// from the profile's writer inputs.
pub(super) fn prefix_block_counts(profile: &str) -> Result<Vec<u64>, String> {
    let input: Value = serde_json::from_slice(
        &fs::read(fixture_root().join(profile).join("inputs.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(input["structural_entries"]
        .as_array()
        .ok_or("structural entries")?
        .iter()
        .map(|e| n(e, "block_count"))
        .collect())
}
