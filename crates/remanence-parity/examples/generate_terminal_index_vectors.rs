//! Generate review-only terminal triple-index candidate vectors and matrices.
//!
//! The checked-in fixtures are deliberately separate from `specs/publication/`:
//! the current generation-2 preparing copy is not frozen. Healthy component
//! bytes come from the Rust codecs; compact manifests describe hostile mutations
//! so redundant damaged copies do not bloat the repository. The high-count
//! source synthesizes one row at a time and records its pass counts without
//! materializing the complete index.

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use ciborium::value::Value as CborValue;
use remanence_parity::{
    assemble_terminal_plan, encode_tape_index_bootstrap_footer, encode_tape_index_replica_header,
    index_separation_records, write_index_separation, write_tape_index_replica,
    IndexSeparationObservation, ObjectRecoveryRepresentation, ParityError, ParityMapDiagnostics,
    TapeIndexEditionPlan, TapeIndexReplicaCounts, TapeIndexReplicaFileKind,
    TapeIndexReplicaMapEntry, TapeIndexReplicaObjectRow, TapeIndexReplicaObservation,
    TapeIndexReplicaRecordSource, TapeIndexReplicaScope, TerminalTailLayout,
    TerminalTripleWritePlan, TERMINAL_INDEX_BLOCK_SIZES,
};
use sha2::{Digest, Sha256};

const COMPACT_GAP_RECORDS: u64 = 3;
const HIGH_COUNT_OBJECT_ROWS: u64 = 1_000_000;
const VECTOR_TIMESTAMP: &str = "2026-08-09T00:00:00Z";
const MAX_TIMESTAMP: &str = "2026-08-09T00:00:00.1111111111111111111111111111111111111111111Z";

#[derive(Clone)]
struct Records {
    entries: Vec<TapeIndexReplicaMapEntry>,
    rows: Vec<TapeIndexReplicaObjectRow>,
}

impl TapeIndexReplicaRecordSource for Records {
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        for entry in &self.entries {
            visitor(entry)?;
        }
        Ok(())
    }

    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        for row in &self.rows {
            visitor(row)?;
        }
        Ok(())
    }
}

/// A replayable million-Object authority with constant retained row storage.
struct SyntheticRecords {
    object_rows: u64,
    structural_passes: u64,
    object_passes: u64,
}

impl TapeIndexReplicaRecordSource for SyntheticRecords {
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        self.structural_passes += 1;
        visitor(&control_entry(0, TapeIndexReplicaFileKind::Bootstrap, 1))?;
        for ordinal in 0..self.object_rows {
            visitor(&object_entry(ordinal + 1, 1, ordinal))?;
        }
        Ok(())
    }

    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        self.object_passes += 1;
        for ordinal in 0..self.object_rows {
            visitor(&TapeIndexReplicaObjectRow {
                tape_file_number: ordinal + 1,
                stored_block_count: 1,
                object_id: b"x".to_vec(),
                representation: ObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: 0,
                    manifest_size_bytes: 1,
                    manifest_chunk_count: 1,
                    manifest_sha256: [0x51; 32],
                },
            })?;
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut check = false;
    let mut directory = None;
    for arg in env::args().skip(1) {
        if arg == "--check" && !check {
            check = true;
        } else if arg.starts_with('-') || directory.is_some() {
            return Err("usage: generate_terminal_index_vectors [--check] [directory]".into());
        } else {
            directory = Some(PathBuf::from(arg));
        }
    }
    let directory =
        directory.unwrap_or_else(|| PathBuf::from("fixtures/rem-parity-terminal-index-draft"));
    if check {
        let temporary = tempfile::tempdir()?;
        generate(temporary.path())?;
        compare_files(temporary.path(), &directory)?;
        println!("CHECK PASS: all candidate files and file sets match");
        return Ok(());
    }
    generate(&directory)
}

fn generate(output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(output)?;

    let mut manifest = String::from(
        "profile\tblock_size\tstructural_rows\tobject_rows\treplica_records\tgap_records\texpected_eod_lba\tcomponent\tbytes\tsha256\tedition_digest\tlayout_digest\tpayload_sha256\tcanonical_map_sha256\n",
    );
    for &block_size in TERMINAL_INDEX_BLOCK_SIZES {
        emit_profile(
            output,
            "minimal",
            block_size,
            minimal_records(),
            &mut manifest,
        )?;
        emit_profile(output, "multi", block_size, multi_records(), &mut manifest)?;
    }
    fs::write(output.join("MANIFEST.tsv"), manifest)?;
    emit_maximum_vectors(output)?;
    emit_high_count_evidence(output)?;
    emit_object_row_extension_vectors(output)?;
    emit_matrix_manifests(output)?;
    fs::write(
        output.join("README.md"),
        "# REM-PARITY terminal-index candidate vectors\n\nReview-only generation-2 candidate artifacts; nothing under this directory is a publication artifact. `MANIFEST.tsv` pins the healthy minimal and multi-Object A/gap-AB/B/gap-BC/C byte streams at every legal block size. Filemarks and EOD are structural expectations rather than bytes. `inputs.json` files record the inputs of each pinned artifact for independent re-derivation (the streaming recipe is `streaming-inputs.json`). Run `cargo run -p remanence-parity --example generate_terminal_index_vectors -- --check` to compare generated files and the file set. Compact gaps contain three records (header, one zero interior, footer), while default one-GiB extents remain an integration obligation.\n\n`MAXIMUMS.tsv` pins maximum plaintext/encrypted recovery-row slots and the maximum diagnostic-envelope one-block footer. `STREAMING.tsv` records a million-Object constant-storage source pass and its independently reproducible digests without checking in the conceptual 320 MB payload. `OBJECT_ROW_EXTENSIONS.tsv` pins Rust-generated fixed-slot artifacts under `object-row-extensions/` by encoded length, byte count, and SHA-256. The independent Python verifier consumes those exact bytes to check positive unknown keys (including nested false/true/null values) and fail-closed assigned/noncanonical extensions. `MUTATIONS.tsv` and `SELECTION.tsv` are compact executable hostile matrices. `INTERRUPTIONS.tsv` independently enumerates the 68 live prefix, component, journal, checkpoint, SQLite, and final-projection cut boundaries, including the sealed-checkpoint-to-intent-cleanup window, and pins each exact command-acceptance, media-proof, and durable host-authority state. A field ending in `_accepted` means the command returned successfully; only the corresponding media-barrier proof field (`*_barrier_proved` or `*_barriers_proved`) establishes media durability.\n",
    )?;
    println!(
        "generated 6 healthy profiles, 3 maximum artifacts, 1 high-count stream, 7 Object-row extension slots, and executable hostile matrices in {}",
        output.display()
    );
    Ok(())
}

fn emit_profile(
    root: &Path,
    name: &str,
    block_size: u32,
    records: Records,
    manifest: &mut String,
) -> Result<(), Box<dyn std::error::Error>> {
    let assembled = plan_records_edition(
        name,
        block_size,
        records.clone(),
        "remanence-terminal-vector-generator/1",
        VECTOR_TIMESTAMP,
    )?;
    let edition = &assembled.edition;
    let directory = root.join(format!("{name}-{}k", block_size / 1024));
    fs::create_dir_all(&directory)?;
    let mut inputs = plan_inputs(edition)?;
    inputs["description"] =
        json!("Records every input to this profile's terminal replicas and separation extents.");
    add_records(&mut inputs, &records);
    write_json(&directory.join("inputs.json"), &inputs)?;

    for (index, plan) in assembled.replicas.iter().enumerate() {
        let observation = TapeIndexReplicaObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let mut source = records.clone();
        let mut bytes = Vec::new();
        write_tape_index_replica(plan, observation, &mut source, |block| {
            bytes.extend_from_slice(block);
            Ok(())
        })?;
        let component = match index {
            0 => "replica-a.bin",
            1 => "replica-b.bin",
            _ => "replica-c.bin",
        };
        write_component(&directory, component, &bytes, name, edition, manifest)?;
    }

    for (index, plan) in assembled.separations.iter().enumerate() {
        let observation = IndexSeparationObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let mut bytes = Vec::new();
        write_index_separation(plan, observation, |block| {
            bytes.extend_from_slice(block);
            Ok(())
        })?;
        let component = if index == 0 {
            "gap-ab.bin"
        } else {
            "gap-bc.bin"
        };
        write_component(&directory, component, &bytes, name, edition, manifest)?;
    }
    Ok(())
}

fn plan_records_edition(
    name: &str,
    block_size: u32,
    records: Records,
    writer_version: &str,
    write_timestamp: &str,
) -> Result<TerminalTripleWritePlan, Box<dyn std::error::Error>> {
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: records.entries.len() as u64,
        object_row_count: records.rows.len() as u64,
    };
    let scope = scope(&records);
    let replica_layout = remanence_parity::checked_tape_index_replica_layout(block_size, counts)?;
    let gap_records =
        index_separation_records(block_size, COMPACT_GAP_RECORDS * u64::from(block_size))?;
    let prefix_end_lba = records.entries.iter().try_fold(0u64, |sum, entry| {
        entry
            .block_count
            .checked_add(1)
            .and_then(|span| sum.checked_add(span))
            .ok_or("prefix LBA overflow")
    })?;
    let terminal_layout = TerminalTailLayout::new(
        0,
        block_size,
        counts.structural_entry_count,
        prefix_end_lba,
        replica_layout.replica_record_count,
        gap_records,
    )?;
    Ok(assemble_terminal_plan(
        [0x11; 16],
        block_size,
        false,
        match name {
            "minimal" => 1,
            "multi" => 2,
            _ => 3,
        },
        scope,
        counts,
        &mut records.clone(),
        terminal_layout,
        ParityMapDiagnostics {
            writer_version: writer_version.into(),
            write_timestamp: write_timestamp.into(),
        },
        match name {
            "minimal" => [0x21; 16],
            "multi" => [0x22; 16],
            _ => [0x23; 16],
        },
        COMPACT_GAP_RECORDS * u64::from(block_size),
    )?)
}

fn write_component(
    directory: &Path,
    component: &str,
    bytes: &[u8],
    profile: &str,
    edition: &TapeIndexEditionPlan,
    manifest: &mut String,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::write(directory.join(component), bytes)?;
    let sha: [u8; 32] = Sha256::digest(bytes).into();
    manifest.push_str(&format!(
        "{}-{}k\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        profile,
        edition.descriptor.block_size / 1024,
        edition.descriptor.block_size,
        edition.descriptor.counts.structural_entry_count,
        edition.descriptor.counts.object_row_count,
        edition.replica_layout.replica_record_count,
        edition
            .descriptor
            .terminal_layout
            .separation(1)?
            .record_count,
        edition.descriptor.terminal_layout.expected_eod_lba,
        component,
        bytes.len(),
        hex(&sha),
        hex(&edition.edition_digest),
        hex(&edition.layout_digest),
        hex(&edition.payload_sha256),
        hex(&edition.canonical_map_sha256),
    ));
    Ok(())
}

fn emit_maximum_vectors(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let directory = root.join("maximums");
    fs::create_dir_all(&directory)?;
    let plaintext = fixed_slot(maximum_plaintext_value())?;
    let encrypted = fixed_slot(maximum_encrypted_value())?;
    let block_size = TERMINAL_INDEX_BLOCK_SIZES[0];
    let records = minimal_records();
    let assembled = plan_records_edition(
        "maximum-footer",
        block_size,
        records,
        &"V".repeat(128),
        MAX_TIMESTAMP,
    )?;
    let mut footer_inputs = plan_inputs(&assembled.edition)?;
    add_records(&mut footer_inputs, &minimal_records());
    footer_inputs["replica_ordinal"] = json!(assembled.replicas[0].replica_ordinal);
    let component = &assembled.replicas[0].component;
    footer_inputs["observation"] = json!({"tape_file_number": component.planned_tape_file_number,
        "start_lba": component.planned_start_lba, "record_count": component.record_count});
    write_json(
        &directory.join("inputs.json"),
        &json!({
            "description": "Records the CBOR fields of both maximum rows and the plan inputs of the maximum footer.",
            "artifacts": {
                "plaintext-row.slot": {"fields": typed_cbor(&maximum_plaintext_value())?},
                "encrypted-row.slot": {"fields": typed_cbor(&maximum_encrypted_value())?},
                "bootstrap-footer.bin": footer_inputs
            }
        }),
    )?;
    let plan = &assembled.replicas[0];
    let header = encode_tape_index_replica_header(plan)?;
    let footer = encode_tape_index_bootstrap_footer(
        plan,
        Sha256::digest(&header).into(),
        TapeIndexReplicaObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        },
    )?;
    let artifacts = [
        (
            "maximum-plaintext-row",
            "plaintext-row.slot",
            plaintext,
            164,
            0,
            0,
        ),
        (
            "maximum-encrypted-row",
            "encrypted-row.slot",
            encrypted,
            247,
            0,
            0,
        ),
        (
            "maximum-one-block-footer",
            "bootstrap-footer.bin",
            footer,
            0,
            128,
            MAX_TIMESTAMP.len(),
        ),
    ];
    let mut manifest = String::from(
        "vector\tartifact\tblock_size\tencoded_len\tbytes\tsha256\twriter_version_len\twrite_timestamp_len\n",
    );
    for (vector, artifact, bytes, encoded_len, writer_len, timestamp_len) in artifacts {
        fs::write(directory.join(artifact), &bytes)?;
        manifest.push_str(&format!(
            "{vector}\t{artifact}\t{block_size}\t{encoded_len}\t{}\t{}\t{writer_len}\t{timestamp_len}\n",
            bytes.len(),
            hex(&Sha256::digest(&bytes)),
        ));
    }
    fs::write(root.join("MAXIMUMS.tsv"), manifest)?;
    Ok(())
}

fn maximum_plaintext_value() -> CborValue {
    CborValue::Map(vec![
        integer_pair(1, u64::MAX),
        (
            CborValue::Integer(2.into()),
            CborValue::Text("plaintext".into()),
        ),
        integer_pair(3, u64::MAX),
        (
            CborValue::Integer(4.into()),
            CborValue::Bytes(vec![0xFF; 64]),
        ),
        integer_pair(10, 0x1_0000_0000),
        integer_pair(11, 0x1_0000_0000),
        integer_pair(12, 0x1_0000_0000),
        (
            CborValue::Integer(13.into()),
            CborValue::Bytes(vec![0xFF; 32]),
        ),
    ])
}

fn maximum_encrypted_value() -> CborValue {
    CborValue::Map(vec![
        integer_pair(1, u64::MAX),
        (
            CborValue::Integer(2.into()),
            CborValue::Text("encrypted".into()),
        ),
        integer_pair(3, u64::MAX),
        (
            CborValue::Integer(4.into()),
            CborValue::Bytes(vec![0xFF; 64]),
        ),
        integer_pair(21, 16 * 1024 * 1024),
        (
            CborValue::Integer(22.into()),
            CborValue::Array(
                (1u8..=8)
                    .map(|value| CborValue::Bytes(vec![value; 16]))
                    .collect(),
            ),
        ),
        integer_pair(23, 16_384),
    ])
}

fn integer_pair(key: u64, value: u64) -> (CborValue, CborValue) {
    (
        CborValue::Integer(key.into()),
        CborValue::Integer(value.into()),
    )
}

fn fixed_slot(value: CborValue) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut encoded = Vec::new();
    ciborium::into_writer(&value, &mut encoded)?;
    if encoded.len() > 254 {
        return Err("maximum Object row exceeds its 254-byte CBOR capacity".into());
    }
    let mut slot = vec![0u8; 256];
    slot[..2].copy_from_slice(&(encoded.len() as u16).to_le_bytes());
    slot[2..2 + encoded.len()].copy_from_slice(&encoded);
    Ok(slot)
}

fn emit_high_count_evidence(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let block_size = TERMINAL_INDEX_BLOCK_SIZES[0];
    let structural_rows = HIGH_COUNT_OBJECT_ROWS + 1;
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: structural_rows,
        object_row_count: HIGH_COUNT_OBJECT_ROWS,
    };
    let replica_layout = remanence_parity::checked_tape_index_replica_layout(block_size, counts)?;
    let prefix_end_lba = HIGH_COUNT_OBJECT_ROWS
        .checked_mul(2)
        .and_then(|value| value.checked_add(2))
        .ok_or("high-count prefix overflow")?;
    let terminal_layout = TerminalTailLayout::new(
        0,
        block_size,
        structural_rows,
        prefix_end_lba,
        replica_layout.replica_record_count,
        COMPACT_GAP_RECORDS,
    )?;
    let mut source = SyntheticRecords {
        object_rows: HIGH_COUNT_OBJECT_ROWS,
        structural_passes: 0,
        object_passes: 0,
    };
    let edition = assemble_terminal_plan(
        [0x61; 16],
        block_size,
        false,
        1,
        TapeIndexReplicaScope {
            covered_prefix_tape_file_count: structural_rows,
            total_data_ordinals: HIGH_COUNT_OBJECT_ROWS,
            highest_protected_ordinal: 0,
        },
        counts,
        &mut source,
        terminal_layout,
        ParityMapDiagnostics {
            writer_version: "synthetic-constant-storage-source/1".into(),
            write_timestamp: VECTOR_TIMESTAMP.into(),
        },
        [0x62; 16],
        COMPACT_GAP_RECORDS * u64::from(block_size),
    )?
    .edition;
    let mut inputs = plan_inputs(&edition)?;
    inputs["description"] = json!("Records the million-row stream's plan and zero-based row templates without materializing the stream.");
    inputs["parameters"] = json!({"object_rows": HIGH_COUNT_OBJECT_ROWS});
    inputs["structural_entries"] = json!({
        "initial": [entry_input(&control_entry(0, TapeIndexReplicaFileKind::Bootstrap, 1))],
        "repeat": {"i_start": 0, "i_end_exclusive": HIGH_COUNT_OBJECT_ROWS,
            "template": entry_input(&object_entry(0, 1, 0)),
            "i_dependent_fields": {"tape_file_number": {"i_plus": 1}, "first_parity_data_ordinal": {"i_plus": 0}}}
    });
    inputs["object_rows"] = json!({"i_start": 0, "i_end_exclusive": HIGH_COUNT_OBJECT_ROWS,
        "template": row_input(&TapeIndexReplicaObjectRow {
            tape_file_number: 0, stored_block_count: 1, object_id: b"x".to_vec(),
            representation: ObjectRecoveryRepresentation::Plaintext {
                manifest_first_chunk_lba: 0, manifest_size_bytes: 1, manifest_chunk_count: 1, manifest_sha256: [0x51; 32]
            }
        }),
        "i_dependent_fields": {"tape_file_number": {"i_plus": 1}}
    });
    write_json(&root.join("streaming-inputs.json"), &inputs)?;
    fs::write(
        root.join("STREAMING.tsv"),
        format!(
            "vector\tblock_size\tstructural_rows\tobject_rows\tpayload_bytes\tpayload_records\treplica_records\texpected_eod_lba\tstructural_passes\tobject_passes\tretained_rows\tpayload_sha256\tcanonical_map_sha256\tedition_digest\tlayout_digest\nlarge-count-million\t{block_size}\t{structural_rows}\t{HIGH_COUNT_OBJECT_ROWS}\t{}\t{}\t{}\t{}\t{}\t{}\t0\t{}\t{}\t{}\t{}\n",
            edition.replica_layout.payload_len,
            edition.replica_layout.payload_record_count,
            edition.replica_layout.replica_record_count,
            edition.descriptor.terminal_layout.expected_eod_lba,
            source.structural_passes,
            source.object_passes,
            hex(&edition.payload_sha256),
            hex(&edition.canonical_map_sha256),
            hex(&edition.edition_digest),
            hex(&edition.layout_digest),
        ),
    )?;
    Ok(())
}

fn emit_object_row_extension_vectors(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let plaintext = generated_object_row(root, 0)?;
    let encrypted = generated_object_row(root, 1)?;

    let mut changes = Vec::new();
    let unknown_positive = extension_slot(
        &mut changes,
        plaintext.clone(),
        24,
        CborValue::Bytes(b"future".to_vec()),
    )?;
    let unknown_negative_value = canonical_map(vec![
        (cbor_integer(1)?, CborValue::Integer(7.into())),
        (cbor_integer(2)?, CborValue::Null),
    ])?;
    let unknown_negative =
        extension_slot(&mut changes, plaintext.clone(), -1, unknown_negative_value)?;
    let nested_boolean_value = canonical_map(vec![
        (cbor_integer(1)?, CborValue::Bool(false)),
        (
            cbor_integer(2)?,
            CborValue::Array(vec![CborValue::Bool(true), CborValue::Null]),
        ),
    ])?;
    let unknown_nested_boolean =
        extension_slot(&mut changes, plaintext.clone(), 25, nested_boolean_value)?;
    let plaintext_with_encrypted =
        extension_slot(&mut changes, plaintext.clone(), 21, CborValue::Null)?;
    let encrypted_with_plaintext = extension_slot(&mut changes, encrypted, 10, CborValue::Null)?;

    let mut unknown_noncanonical_value = extension_slot(
        &mut changes,
        plaintext.clone(),
        24,
        CborValue::Integer(0.into()),
    )?;
    let canonical_len = usize::from(u16::from_le_bytes(
        unknown_noncanonical_value[..2].try_into()?,
    ));
    if canonical_len == 0 || canonical_len >= unknown_noncanonical_value.len() - 2 {
        return Err("canonical unknown extension has no room for a widened value".into());
    }
    let last_value = 2 + canonical_len - 1;
    if unknown_noncanonical_value[last_value] != 0 {
        return Err("unknown extension value is not the expected final zero".into());
    }
    unknown_noncanonical_value[last_value] = 0x18;
    unknown_noncanonical_value[last_value + 1] = 0;
    unknown_noncanonical_value[..2]
        .copy_from_slice(&u16::try_from(canonical_len + 1)?.to_le_bytes());

    let noncanonical_nested_map = CborValue::Map(vec![
        (cbor_integer(2)?, CborValue::Bool(true)),
        (cbor_integer(1)?, CborValue::Bool(false)),
    ]);
    let unknown_nested_map_order =
        extension_slot(&mut changes, plaintext, 24, noncanonical_nested_map)?;

    let vectors = [
        ("unknown-positive-key", 0, unknown_positive, "valid"),
        ("unknown-negative-key", 0, unknown_negative, "valid"),
        ("unknown-nested-boolean", 0, unknown_nested_boolean, "valid"),
        (
            "plaintext-with-encrypted-field",
            0,
            plaintext_with_encrypted,
            "cross-representation",
        ),
        (
            "encrypted-with-plaintext-field",
            1,
            encrypted_with_plaintext,
            "cross-representation",
        ),
        (
            "unknown-noncanonical-value",
            0,
            unknown_noncanonical_value,
            "cbor",
        ),
        (
            "unknown-nested-map-order",
            0,
            unknown_nested_map_order,
            "cbor",
        ),
    ];
    let directory = root.join("object-row-extensions");
    fs::create_dir_all(&directory)?;
    let mut manifest = String::from(
        "case_id\tbase_profile\trow_index\tartifact\tencoded_len\tbytes\tsha256\texpected\n",
    );
    assert_eq!(
        vectors.len(),
        changes.len(),
        "every extension must record its change"
    );
    let mut cases = Vec::new();
    for ((case_id, row_index, slot, expected), change) in vectors.into_iter().zip(changes) {
        let mut input = json!({"case_id": case_id, "base_profile": "multi-256k", "row_index": row_index, "change": change});
        if case_id == "unknown-noncanonical-value" {
            input["byte_edit"] = json!({"slot_offset": last_value, "old_bytes": "0000", "new_bytes": "1800", "new_length_prefix": hex(&slot[..2])});
        }
        cases.push(input);
        let artifact = format!("object-row-extensions/{case_id}.slot");
        fs::write(root.join(&artifact), &slot)?;
        manifest.push_str(&format!(
            "{case_id}\tmulti-256k\t{row_index}\t{artifact}\t{}\t{}\t{}\t{expected}\n",
            u16::from_le_bytes(slot[..2].try_into()?),
            slot.len(),
            hex(&Sha256::digest(&slot)),
        ));
    }
    write_json(
        &directory.join("inputs.json"),
        &json!({
            "description": "Records each extension slot's base row, typed CBOR field addition and optional byte edit.",
            "cases": cases
        }),
    )?;
    fs::write(root.join("OBJECT_ROW_EXTENSIONS.tsv"), manifest)?;
    Ok(())
}

fn generated_object_row(
    root: &Path,
    row_index: usize,
) -> Result<CborValue, Box<dyn std::error::Error>> {
    const BLOCK_SIZE: usize = 256 * 1024;
    const STRUCTURAL_SLOT_LEN: usize = 64;
    const OBJECT_SLOT_LEN: usize = 256;

    let replica = fs::read(root.join("multi-256k/replica-a.bin"))?;
    let structural_bytes = multi_records()
        .entries
        .len()
        .checked_mul(STRUCTURAL_SLOT_LEN)
        .ok_or("generated structural-slot span overflow")?;
    let object_offset = row_index
        .checked_mul(OBJECT_SLOT_LEN)
        .ok_or("generated Object-row relative offset overflow")?;
    let start = BLOCK_SIZE
        .checked_add(structural_bytes)
        .and_then(|offset| offset.checked_add(object_offset))
        .ok_or("generated Object-row offset overflow")?;
    let end = start
        .checked_add(OBJECT_SLOT_LEN)
        .ok_or("generated Object-row end overflow")?;
    let slot = replica
        .get(start..end)
        .ok_or("generated Object row is outside replica A")?;
    let encoded_len = usize::from(u16::from_le_bytes(slot[..2].try_into()?));
    if encoded_len == 0 || encoded_len > OBJECT_SLOT_LEN - 2 {
        return Err("generated Object row has an invalid encoded length".into());
    }
    let encoded_end = 2 + encoded_len;
    if slot[encoded_end..].iter().any(|byte| *byte != 0) {
        return Err("generated Object row has nonzero slot padding".into());
    }
    let encoded = &slot[2..encoded_end];
    let mut reader = Cursor::new(encoded);
    let value: CborValue = ciborium::from_reader(&mut reader)?;
    if reader.position() != encoded_len as u64 {
        return Err("generated Object row has trailing CBOR bytes".into());
    }
    let mut reencoded = Vec::new();
    ciborium::into_writer(&value, &mut reencoded)?;
    if reencoded != encoded {
        return Err("generated Object row is not canonical CBOR".into());
    }
    Ok(value)
}

fn cbor_integer(value: i128) -> Result<CborValue, Box<dyn std::error::Error>> {
    Ok(CborValue::Integer(value.try_into().map_err(|_| {
        format!("extension key {value} is outside the CBOR integer range")
    })?))
}

fn canonical_map(
    entries: Vec<(CborValue, CborValue)>,
) -> Result<CborValue, Box<dyn std::error::Error>> {
    let mut encoded_entries = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let mut encoded_key = Vec::new();
        ciborium::into_writer(&key, &mut encoded_key)?;
        encoded_entries.push((encoded_key, key, value));
    }
    encoded_entries.sort_by(|(left, _, _), (right, _, _)| {
        left.len().cmp(&right.len()).then_with(|| left.cmp(right))
    });
    Ok(CborValue::Map(
        encoded_entries
            .into_iter()
            .map(|(_, key, value)| (key, value))
            .collect(),
    ))
}

fn with_integer_field(
    value: CborValue,
    key: i128,
    field: CborValue,
) -> Result<CborValue, Box<dyn std::error::Error>> {
    let CborValue::Map(mut entries) = value else {
        return Err("generated Object row is not a CBOR map".into());
    };
    entries.push((cbor_integer(key)?, field));
    canonical_map(entries)
}

fn emit_matrix_manifests(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    fs::write(
        root.join("MUTATIONS.tsv"),
        "case_id\tkind\tbase_profile\ttarget\tmutation\tother_profile\texpected\n\
replica-header-damaged\treplica\tmulti-256k\treplica-a.bin\tdamage-header\t\tcrc-header\n\
replica-footer-damaged\treplica\tmulti-256k\treplica-a.bin\tdamage-footer\t\tcrc-footer\n\
replica-header-torn\treplica\tmulti-256k\treplica-a.bin\ttorn-header\t\twrong-length\n\
replica-footer-torn\treplica\tmulti-256k\treplica-a.bin\ttorn-footer\t\twrong-length\n\
replica-wrong-tape\treplica\tmulti-256k\treplica-a.bin\twrong-tape\t\twrong-tape\n\
replica-wrong-edition\treplica\tmulti-256k\treplica-a.bin\twrong-edition\t\twrong-edition\n\
replica-wrong-ordinal\treplica\tmulti-256k\treplica-a.bin\twrong-ordinal\t\twrong-ordinal\n\
replica-wrong-count\treplica\tmulti-256k\treplica-a.bin\twrong-count\t\twrong-replica-count\n\
replica-wrong-scope\treplica\tmulti-256k\treplica-a.bin\twrong-scope\t\twrong-scope\n\
replica-wrong-range\treplica\tmulti-256k\treplica-a.bin\twrong-range\t\twrong-scope\n\
replica-wrong-payload-digest\treplica\tmulti-256k\treplica-a.bin\twrong-payload-digest\t\tpayload-digest\n\
replica-wrong-map-digest\treplica\tmulti-256k\treplica-a.bin\twrong-map-digest\t\tmap-digest\n\
replica-wrong-edition-digest\treplica\tmulti-256k\treplica-a.bin\twrong-edition-digest\t\twrong-edition\n\
replica-wrong-descriptor-digest\treplica\tmulti-256k\treplica-a.bin\twrong-descriptor-digest\t\tdescriptor-digest\n\
replica-wrong-layout-digest\treplica\tmulti-256k\treplica-a.bin\twrong-layout-digest\t\tlayout-digest\n\
replica-wrong-start\treplica\tmulti-256k\treplica-a.bin\twrong-start\t\twrong-start\n\
replica-wrong-block-size\treplica\tmulti-256k\treplica-a.bin\twrong-block-size\t\twrong-block-size\n\
replica-compression-enabled\treplica\tmulti-256k\treplica-a.bin\tcompression-enabled\t\tcompression-enabled\n\
replica-mixed-header-footer\treplica\tmulti-256k\treplica-a.bin\tmixed-header-footer\tmulti-256k/replica-b.bin\tmixed-header-footer\n\
replica-wrong-header-hash\treplica\tmulti-256k\treplica-a.bin\twrong-header-hash\t\theader-hash\n\
replica-wrong-observed-start\treplica\tmulti-256k\treplica-a.bin\twrong-observed-start\t\twrong-observation\n\
replica-wrong-observed-count\treplica\tmulti-256k\treplica-a.bin\twrong-observed-count\t\twrong-observation\n\
replica-payload-corrupt\treplica\tmulti-256k\treplica-a.bin\tpayload-corrupt\t\tpayload-digest\n\
replica-slot-truncated\treplica\tmulti-256k\treplica-a.bin\tslot-length\t\tslot-length\n\
replica-map-row-mismatch\treplica\tmulti-256k\treplica-a.bin\tswap-object-rows\t\tmap-row-bijection\n\
replica-metadata-frame-too-short\treplica\tmulti-256k\treplica-a.bin\tmetadata-frame-too-short\t\tmap-row-bijection\n\
replica-payload-padding\treplica\tmulti-256k\treplica-a.bin\tpayload-padding\t\tpayload-padding\n\
replica-frame-padding\treplica\tmulti-256k\treplica-a.bin\tframe-padding\t\tframe-padding\n\
replica-reserved-nonzero\treplica\tmulti-256k\treplica-a.bin\treserved-nonzero\t\treserved-nonzero\n\
replica-structural-overflow\treplica\tmulti-256k\treplica-a.bin\tstructural-overflow\t\tarithmetic-overflow\n\
replica-object-overflow\treplica\tmulti-256k\treplica-a.bin\tobject-overflow\t\tarithmetic-overflow\n\
replica-payload-add-overflow\treplica\tmulti-256k\treplica-a.bin\tpayload-add-overflow\t\tarithmetic-overflow\n\
gap-header-damaged\tgap\tmulti-256k\tgap-ab.bin\tdamage-header\t\tcrc-header\n\
gap-footer-damaged\tgap\tmulti-256k\tgap-ab.bin\tdamage-footer\t\tcrc-footer\n\
gap-header-torn\tgap\tmulti-256k\tgap-ab.bin\ttorn-header\t\twrong-length\n\
gap-footer-torn\tgap\tmulti-256k\tgap-ab.bin\ttorn-footer\t\twrong-length\n\
gap-header-missing\tgap\tmulti-256k\tgap-ab.bin\tmissing-header\t\tgap-misclassification\n\
gap-footer-missing\tgap\tmulti-256k\tgap-ab.bin\tmissing-footer\t\tgap-misclassification\n\
gap-misclassified\tgap\tmulti-256k\tgap-ab.bin\tmisclassify-as-replica\t\tgap-misclassification\n\
gap-wrong-total-length\tgap\tmulti-256k\tgap-ab.bin\twrong-total-length\t\twrong-length\n\
gap-compression-enabled\tgap\tmulti-256k\tgap-ab.bin\tcompression-enabled\t\tcompression-enabled\n\
gap-wrong-nominal-range\tgap\tmulti-256k\tgap-ab.bin\twrong-range\t\twrong-range\n\
gap-wrong-record-count\tgap\tmulti-256k\tgap-ab.bin\twrong-count\t\twrong-count\n\
gap-wrong-tape\tgap\tmulti-256k\tgap-ab.bin\twrong-tape\t\twrong-tape\n\
gap-wrong-edition\tgap\tmulti-256k\tgap-ab.bin\twrong-edition\t\twrong-edition\n\
gap-wrong-ordinal\tgap\tmulti-256k\tgap-ab.bin\twrong-ordinal\t\twrong-ordinal\n\
gap-mixed-header-footer\tgap\tmulti-256k\tgap-ab.bin\tmixed-header-footer\tmulti-256k/gap-bc.bin\tmixed-header-footer\n\
gap-wrong-observed-start\tgap\tmulti-256k\tgap-ab.bin\twrong-observed-start\t\twrong-observation\n\
gap-interior-damaged\tgap\tmulti-256k\tgap-ab.bin\tinterior-nonzero\t\tdamaged-interior\n\
filemark-missing\tevent\tmulti-256k\treplica-a.bin\tmissing-filemark\t\tmissing-filemark\n",
    )?;
    fs::write(
        root.join("SELECTION.tsv"),
        "case_id\tbase_profile\ta\tb\tc\texpected\n\
healthy\tmulti-256k\tvalid\tvalid\tvalid\tselect-c\n\
damage-a\tmulti-256k\tdamaged\tvalid\tvalid\tselect-c\n\
damage-b\tmulti-256k\tvalid\tdamaged\tvalid\tselect-c\n\
damage-c\tmulti-256k\tvalid\tvalid\tdamaged\tselect-b\n\
damage-a-b\tmulti-256k\tdamaged\tdamaged\tvalid\tselect-c\n\
damage-a-c\tmulti-256k\tdamaged\tvalid\tdamaged\tselect-b\n\
damage-b-c\tmulti-256k\tvalid\tdamaged\tdamaged\tselect-a\n\
all-invalid\tmulti-256k\tdamaged\tdamaged\tdamaged\tbot-structural-recovery\n\
all-torn\tmulti-256k\ttorn\ttorn\ttorn\tbot-structural-recovery\n\
all-missing\tmulti-256k\tmissing\tmissing\tmissing\tbot-structural-recovery\n\
all-conflicting\tmulti-256k\tconflict-minimal\tvalid\tconflict-minimal\tconflict\n\
conflicting-a\tmulti-256k\tconflict-minimal\tvalid\tvalid\tconflict\n\
conflicting-b\tmulti-256k\tvalid\tconflict-minimal\tvalid\tconflict\n\
conflicting-c\tmulti-256k\tvalid\tvalid\tconflict-minimal\tconflict\n",
    )?;
    emit_interruption_matrix(root)?;
    Ok(())
}

fn emit_interruption_matrix(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    const COMPONENTS: [&str; 5] = [
        "replica_a",
        "separation_ab",
        "replica_b",
        "separation_bc",
        "replica_c",
    ];
    const COMPONENT_CUTS: [&str; 10] = [
        "before_footer",
        "after_footer",
        "before_filemark",
        "after_filemark",
        "before_barrier",
        "after_barrier",
        "before_parity_journal_fsync",
        "after_parity_journal_fsync",
        "before_checkpoint_journal_fsync",
        "after_checkpoint_journal_fsync",
    ];
    const PROGRESS: [&str; 6] = [
        "BeforeReplicaA",
        "AfterReplicaA",
        "AfterSeparationAb",
        "AfterReplicaB",
        "AfterSeparationBc",
        "AfterReplicaC",
    ];
    let mut matrix = String::from(
        "case_id\tphase\tcomponent\tcut\tprefix_parity_map_blocks_accepted\tprefix_parity_map_filemark_accepted\tprefix_media_barrier_proved\tprefix_sink_journal\tprefix_sink_checkpoint\tcomponent_block_streams_accepted\tcomponent_filemark_commands_accepted\tcomponent_media_barriers_proved\tsink_journal_components\tcheckpoint_components\tsqlite_components\tsealed_checkpoint\tintent_present\tfinal_sqlite\texpected_checkpoint_progress\texpected_completed_replicas\texpected_resume\n",
    );
    for (
        cut,
        prefix_blocks_accepted,
        prefix_filemark_accepted,
        prefix_barrier_proved,
        prefix_journal,
        resume,
    ) in [
        (
            "before_terminal_prefix",
            0,
            0,
            0,
            0,
            "finish-terminal-prefix",
        ),
        (
            "before_final_parity_map",
            0,
            0,
            0,
            0,
            "finish-terminal-prefix",
        ),
        (
            "after_final_parity_map",
            1,
            1,
            0,
            0,
            "finish-terminal-prefix",
        ),
        ("after_terminal_prefix", 1, 1, 1, 1, "replica_a"),
    ] {
        matrix.push_str(&format!(
            "terminal_prefix-{cut}\tprefix\tparity_closeout\t{cut}\t{prefix_blocks_accepted}\t{prefix_filemark_accepted}\t{prefix_barrier_proved}\t{prefix_journal}\t{prefix_journal}\t0\t0\t0\t0\t0\t0\t0\t1\t0\tBeforeReplicaA\t0\t{resume}\n"
        ));
    }
    for (index, component) in COMPONENTS.iter().enumerate() {
        for cut in COMPONENT_CUTS {
            let complete = index + usize::from(cut != "before_footer");
            let filemark_commands_accepted = index
                + usize::from(matches!(
                    cut,
                    "after_filemark"
                        | "before_barrier"
                        | "after_barrier"
                        | "before_parity_journal_fsync"
                        | "after_parity_journal_fsync"
                        | "before_checkpoint_journal_fsync"
                        | "after_checkpoint_journal_fsync"
                ));
            let media_barriers_proved = index
                + usize::from(matches!(
                    cut,
                    "after_barrier"
                        | "before_parity_journal_fsync"
                        | "after_parity_journal_fsync"
                        | "before_checkpoint_journal_fsync"
                        | "after_checkpoint_journal_fsync"
                ));
            let sink = index
                + usize::from(matches!(
                    cut,
                    "after_parity_journal_fsync"
                        | "before_checkpoint_journal_fsync"
                        | "after_checkpoint_journal_fsync"
                ));
            let checkpoint = index + usize::from(cut == "after_checkpoint_journal_fsync");
            let sqlite = index.min(4);
            let resume = match cut {
                "after_parity_journal_fsync" | "before_checkpoint_journal_fsync" => {
                    "promote-sink-transition"
                }
                "after_checkpoint_journal_fsync" if index == 4 => {
                    "repair-sqlite-then-finish-final-projection"
                }
                "after_checkpoint_journal_fsync" => "repair-sqlite-then-continue",
                _ => "reconcile-current",
            };
            matrix.push_str(&format!(
                "{component}-{cut}\tcomponent\t{component}\t{cut}\t1\t1\t1\t1\t1\t{complete}\t{filemark_commands_accepted}\t{media_barriers_proved}\t{sink}\t{checkpoint}\t{sqlite}\t0\t1\t0\t{}\t{}\t{resume}\n",
                PROGRESS[checkpoint],
                checkpoint.div_ceil(2),
            ));
        }
        for (cut, sqlite, resume) in [
            (
                "before_sqlite_projection",
                index,
                if index == 4 {
                    "repair-sqlite-then-finish-final-projection"
                } else {
                    "repair-sqlite-then-continue"
                },
            ),
            (
                "after_sqlite_projection",
                index + 1,
                if index == 4 {
                    "finish-final-projection"
                } else {
                    "continue"
                },
            ),
        ] {
            let count = index + 1;
            matrix.push_str(&format!(
                "{component}-{cut}\tcomponent\t{component}\t{cut}\t1\t1\t1\t1\t1\t{count}\t{count}\t{count}\t{count}\t{count}\t{sqlite}\t0\t1\t0\t{}\t{}\t{resume}\n",
                PROGRESS[count],
                count.div_ceil(2),
            ));
        }
    }
    for (cut, sealed, intent, finalized_sqlite, resume) in [
        (
            "before_final_checkpoint_fsync",
            0,
            1,
            0,
            "finish-final-projection",
        ),
        (
            "after_final_checkpoint_fsync",
            1,
            1,
            0,
            "replay-sealed-completion",
        ),
        (
            "before_final_sqlite_projection",
            1,
            0,
            0,
            "replay-sealed-completion",
        ),
        ("after_final_sqlite_projection", 1, 0, 1, "none"),
    ] {
        matrix.push_str(&format!(
            "final_projection-{cut}\tfinal_projection\tfinal_projection\t{cut}\t1\t1\t1\t1\t1\t5\t5\t5\t5\t5\t5\t{sealed}\t{intent}\t{finalized_sqlite}\tAfterReplicaC\t3\t{resume}\n"
        ));
    }
    fs::write(root.join("INTERRUPTIONS.tsv"), matrix)?;
    Ok(())
}

fn scope(records: &Records) -> TapeIndexReplicaScope {
    let total_data_ordinals = records
        .entries
        .iter()
        .filter(|entry| entry.kind == TapeIndexReplicaFileKind::Object)
        .map(|entry| entry.block_count)
        .sum();
    let highest_protected_ordinal = records
        .entries
        .iter()
        .filter_map(|entry| entry.protected_ordinal_end_exclusive)
        .max()
        .unwrap_or(0);
    TapeIndexReplicaScope {
        covered_prefix_tape_file_count: records.entries.len() as u64,
        total_data_ordinals,
        highest_protected_ordinal,
    }
}

fn minimal_records() -> Records {
    Records {
        entries: vec![control_entry(0, TapeIndexReplicaFileKind::Bootstrap, 1)],
        rows: vec![],
    }
}

fn multi_records() -> Records {
    Records {
        entries: vec![
            control_entry(0, TapeIndexReplicaFileKind::Bootstrap, 1),
            object_entry(1, 2, 0),
            sidecar_entry(2, 5, 0, 2, 0),
            object_entry(3, 3, 2),
            sidecar_entry(4, 5, 2, 5, 1),
            control_entry(5, TapeIndexReplicaFileKind::ParityMap, 3),
        ],
        rows: vec![
            TapeIndexReplicaObjectRow {
                tape_file_number: 1,
                stored_block_count: 2,
                object_id: b"minimal-plaintext-object".to_vec(),
                representation: ObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: 0,
                    manifest_size_bytes: 32,
                    manifest_chunk_count: 1,
                    manifest_sha256: [0x31; 32],
                },
            },
            TapeIndexReplicaObjectRow {
                tape_file_number: 3,
                stored_block_count: 3,
                object_id: b"minimal-encrypted-object".to_vec(),
                representation: ObjectRecoveryRepresentation::Encrypted {
                    recipient_epoch_ids: vec![[0x41; 16], [0x42; 16]],
                    metadata_frame_len: 4096,
                    key_frame_len: 1191,
                },
            },
        ],
    }
}

fn control_entry(
    tape_file_number: u64,
    kind: TapeIndexReplicaFileKind,
    block_count: u64,
) -> TapeIndexReplicaMapEntry {
    TapeIndexReplicaMapEntry {
        tape_file_number,
        kind,
        block_count,
        first_parity_data_ordinal: None,
        protected_ordinal_start: None,
        protected_ordinal_end_exclusive: None,
        epoch_id: None,
    }
}

fn object_entry(tape_file_number: u64, block_count: u64, first: u64) -> TapeIndexReplicaMapEntry {
    TapeIndexReplicaMapEntry {
        first_parity_data_ordinal: Some(first),
        ..control_entry(
            tape_file_number,
            TapeIndexReplicaFileKind::Object,
            block_count,
        )
    }
}

fn sidecar_entry(
    tape_file_number: u64,
    block_count: u64,
    start: u64,
    end: u64,
    epoch_id: u64,
) -> TapeIndexReplicaMapEntry {
    TapeIndexReplicaMapEntry {
        tape_file_number,
        kind: TapeIndexReplicaFileKind::ParitySidecar,
        block_count,
        first_parity_data_ordinal: None,
        protected_ordinal_start: Some(start),
        protected_ordinal_end_exclusive: Some(end),
        epoch_id: Some(epoch_id),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Record semantic inputs, retaining absent optional integers as JSON null.
fn entry_input(entry: &TapeIndexReplicaMapEntry) -> Value {
    json!({"tape_file_number": entry.tape_file_number, "kind": format!("{:?}", entry.kind),
        "block_count": entry.block_count, "first_parity_data_ordinal": entry.first_parity_data_ordinal,
        "protected_ordinal_start": entry.protected_ordinal_start,
        "protected_ordinal_end_exclusive": entry.protected_ordinal_end_exclusive, "epoch_id": entry.epoch_id})
}

fn row_input(row: &TapeIndexReplicaObjectRow) -> Value {
    let representation = match &row.representation {
        ObjectRecoveryRepresentation::Plaintext {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            manifest_sha256,
        } => json!({
            "kind": "Plaintext", "manifest_first_chunk_lba": manifest_first_chunk_lba,
            "manifest_size_bytes": manifest_size_bytes, "manifest_chunk_count": manifest_chunk_count,
            "manifest_sha256": hex(manifest_sha256)
        }),
        ObjectRecoveryRepresentation::Encrypted {
            recipient_epoch_ids,
            metadata_frame_len,
            key_frame_len,
        } => json!({
            "kind": "Encrypted", "recipient_epoch_ids": recipient_epoch_ids.iter().map(|id| hex(id)).collect::<Vec<_>>(),
            "metadata_frame_len": metadata_frame_len, "key_frame_len": key_frame_len
        }),
    };
    json!({"tape_file_number": row.tape_file_number, "stored_block_count": row.stored_block_count,
        "object_id": hex(&row.object_id), "representation": representation})
}

fn add_records(inputs: &mut Value, records: &Records) {
    inputs["structural_entries"] =
        json!(records.entries.iter().map(entry_input).collect::<Vec<_>>());
    inputs["object_rows"] = json!(records.rows.iter().map(row_input).collect::<Vec<_>>());
}

fn plan_inputs(edition: &TapeIndexEditionPlan) -> Result<Value, Box<dyn std::error::Error>> {
    let d = &edition.descriptor;
    let first = &d.terminal_layout.components[0];
    let separation_records = d.terminal_layout.separation(1)?.record_count;
    Ok(json!({
        "tape_uuid": hex(&d.tape_uuid), "edition_id": hex(&d.edition_id),
        "edition_sequence": d.edition_sequence, "block_size": d.block_size,
        "compression_enabled": d.compression_enabled,
        "scope": {"covered_prefix_tape_file_count": d.scope.covered_prefix_tape_file_count,
            "total_data_ordinals": d.scope.total_data_ordinals, "highest_protected_ordinal": d.scope.highest_protected_ordinal},
        "counts": {"structural_entry_count": d.counts.structural_entry_count, "object_row_count": d.counts.object_row_count},
        "terminal_layout": {"partition": d.terminal_layout.partition, "start_tape_file": first.planned_tape_file_number,
            "prefix_end_lba": first.planned_start_lba, "replica_record_count": edition.replica_layout.replica_record_count,
            "separation_records": separation_records},
        "separation_extent": {"nominal_extent_bytes": COMPACT_GAP_RECORDS * u64::from(d.block_size), "total_records": separation_records},
        "diagnostics": {"writer_version": d.writer_version, "write_timestamp": d.write_timestamp}
    }))
}

/// Typed CBOR preserves byte strings, signedness and map entry order.
fn typed_cbor(value: &CborValue) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(match value {
        CborValue::Integer(n) => {
            let n = i128::from(*n);
            if n >= 0 {
                json!({"uint": u64::try_from(n)?})
            } else {
                json!({"nint": i64::try_from(n)?})
            }
        }
        CborValue::Bytes(bytes) => json!({"bytes": hex(bytes)}),
        CborValue::Text(text) => json!({"text": text}),
        CborValue::Bool(value) => json!({"bool": value}),
        CborValue::Null => json!({"null": true}),
        CborValue::Array(items) => {
            json!({"array": items.iter().map(typed_cbor).collect::<Result<Vec<_>, _>>()?})
        }
        CborValue::Map(entries) => {
            json!({"map": entries.iter().map(|(k, v)| Ok([typed_cbor(k)?, typed_cbor(v)?])).collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?})
        }
        _ => return Err("input uses an unsupported CBOR value".into()),
    })
}

fn extension_slot(
    changes: &mut Vec<Value>,
    base: CborValue,
    key: i128,
    value: CborValue,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    changes.push(json!({"integer_key": i64::try_from(key)?, "value": typed_cbor(&value)?}));
    fixed_slot(with_integer_field(base, key, value)?)
}

fn write_json(path: &Path, value: &Value) -> Result<(), Box<dyn std::error::Error>> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

/// Only the two independently owned top-level directories are excluded.
fn candidate_files(
    root: &Path,
    relative: &Path,
    files: &mut BTreeSet<PathBuf>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let path = relative.join(entry.file_name());
        if relative.as_os_str().is_empty()
            && entry.file_type()?.is_dir()
            && matches!(
                entry.file_name().to_str(),
                Some("tape-images" | "second-implementation")
            )
        {
            continue;
        }
        if entry.file_type()?.is_dir() {
            candidate_files(root, &path, files)?;
        } else {
            files.insert(path);
        }
    }
    Ok(())
}

fn compare_files(generated: &Path, fixtures: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut expected = BTreeSet::new();
    let mut actual = BTreeSet::new();
    candidate_files(generated, Path::new(""), &mut expected)?;
    candidate_files(fixtures, Path::new(""), &mut actual)?;
    let mut errors = Vec::new();
    for path in expected.difference(&actual) {
        errors.push(format!("missing: {}", path.display()));
    }
    for path in actual.difference(&expected) {
        errors.push(format!("extra: {}", path.display()));
    }
    for path in expected.intersection(&actual) {
        if fs::read(generated.join(path))? != fs::read(fixtures.join(path))? {
            errors.push(format!("differs: {}", path.display()));
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("\n").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_reports_every_file_difference_and_ignores_other_owners() {
        let generated = tempfile::tempdir().unwrap();
        let fixtures = tempfile::tempdir().unwrap();
        for root in [generated.path(), fixtures.path()] {
            fs::write(root.join("same"), b"same").unwrap();
            fs::write(root.join("different"), b"original").unwrap();
        }
        fs::write(generated.path().join("missing"), b"missing").unwrap();
        fs::write(fixtures.path().join("extra"), b"extra").unwrap();
        fs::write(fixtures.path().join("different"), b"changed").unwrap();
        for owner in ["tape-images", "second-implementation"] {
            fs::create_dir(fixtures.path().join(owner)).unwrap();
            fs::write(fixtures.path().join(owner).join("owned"), b"ignored").unwrap();
        }
        let error = compare_files(generated.path(), fixtures.path())
            .unwrap_err()
            .to_string();
        assert_eq!(error, "missing: missing\nextra: extra\ndiffers: different");
        fs::remove_file(generated.path().join("missing")).unwrap();
        fs::remove_file(fixtures.path().join("extra")).unwrap();
        fs::write(fixtures.path().join("different"), b"original").unwrap();
        compare_files(generated.path(), fixtures.path()).unwrap();
    }
}
