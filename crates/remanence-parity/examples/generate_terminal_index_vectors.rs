//! Generate review-only terminal triple-index candidate vectors and matrices.
//!
//! The checked-in fixtures are deliberately separate from `specs/publication/`:
//! draft.4 is not frozen. Healthy component bytes come from the Rust codecs;
//! compact manifests describe hostile mutations so redundant damaged copies do
//! not bloat the repository. The high-count source synthesizes one row at a
//! time and records its pass counts without materializing the complete index.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use ciborium::value::Value as CborValue;
use remanence_parity::{
    encode_tape_index_bootstrap_footer, encode_tape_index_replica_header, index_separation_records,
    plan_index_separation, plan_tape_index_edition, plan_tape_index_replica,
    write_index_separation, write_tape_index_replica, IndexSeparationDescriptor,
    IndexSeparationObservation, ObjectRecoveryRepresentation, ParityError,
    TapeIndexEditionDescriptor, TapeIndexEditionPlan, TapeIndexReplicaCounts,
    TapeIndexReplicaFileKind, TapeIndexReplicaMapEntry, TapeIndexReplicaObjectRow,
    TapeIndexReplicaObservation, TapeIndexReplicaRecordSource, TapeIndexReplicaScope,
    TerminalTailLayout, TERMINAL_INDEX_BLOCK_SIZES,
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
    let output = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("fixtures/rem-parity-terminal-index-draft"));
    fs::create_dir_all(&output)?;

    let mut manifest = String::from(
        "profile\tblock_size\tstructural_rows\tobject_rows\treplica_records\tgap_records\texpected_eod_lba\tcomponent\tbytes\tsha256\tedition_digest\tlayout_digest\tpayload_sha256\tcanonical_map_sha256\n",
    );
    for &block_size in TERMINAL_INDEX_BLOCK_SIZES {
        emit_profile(
            &output,
            "minimal",
            block_size,
            minimal_records(),
            &mut manifest,
        )?;
        emit_profile(&output, "multi", block_size, multi_records(), &mut manifest)?;
    }
    fs::write(output.join("MANIFEST.tsv"), manifest)?;
    emit_maximum_vectors(&output)?;
    emit_high_count_evidence(&output)?;
    emit_matrix_manifests(&output)?;
    fs::write(
        output.join("README.md"),
        "# REM-PARITY terminal-index candidate vectors\n\nReview-only draft.4 artifacts; nothing under this directory is a publication artifact. `MANIFEST.tsv` pins the healthy minimal and multi-Object A/gap-AB/B/gap-BC/C byte streams at every legal block size. Filemarks and EOD are structural expectations rather than bytes. Compact gaps contain three records (header, one zero interior, footer), while default one-GiB extents remain an integration obligation.\n\n`MAXIMUMS.tsv` pins maximum plaintext/encrypted recovery-row slots and the maximum diagnostic-envelope one-block footer. `STREAMING.tsv` records a million-Object constant-storage source pass and its independently reproducible digests without checking in the conceptual 320 MB payload. `MUTATIONS.tsv`, `SELECTION.tsv`, and `INTERRUPTIONS.tsv` are compact executable matrices: the independent Python verifier derives each damaged input in memory from healthy bytes and checks its typed result.\n",
    )?;
    println!(
        "generated 6 healthy profiles, 3 maximum artifacts, 1 high-count stream, and executable hostile matrices in {}",
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
    let edition = plan_records_edition(
        name,
        block_size,
        records.clone(),
        "remanence-terminal-vector-generator/1",
        VECTOR_TIMESTAMP,
    )?;
    let directory = root.join(format!("{name}-{}k", block_size / 1024));
    fs::create_dir_all(&directory)?;

    for ordinal in 1..=3 {
        let plan = plan_tape_index_replica(edition.clone(), ordinal)?;
        let observation = TapeIndexReplicaObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let mut source = records.clone();
        let mut bytes = Vec::new();
        write_tape_index_replica(&plan, observation, &mut source, |block| {
            bytes.extend_from_slice(block);
            Ok(())
        })?;
        let component = match ordinal {
            1 => "replica-a.bin",
            2 => "replica-b.bin",
            _ => "replica-c.bin",
        };
        write_component(&directory, component, &bytes, name, &edition, manifest)?;
    }

    for ordinal in 1..=2 {
        let plan = plan_index_separation(IndexSeparationDescriptor {
            tape_uuid: edition.descriptor.tape_uuid,
            edition_id: edition.descriptor.edition_id,
            gap_ordinal: ordinal,
            block_size,
            nominal_extent_bytes: COMPACT_GAP_RECORDS * u64::from(block_size),
            total_records: COMPACT_GAP_RECORDS,
            compression_enabled: false,
            terminal_layout: edition.descriptor.terminal_layout,
        })?;
        let observation = IndexSeparationObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: plan.component.record_count,
        };
        let mut bytes = Vec::new();
        write_index_separation(&plan, observation, |block| {
            bytes.extend_from_slice(block);
            Ok(())
        })?;
        let component = if ordinal == 1 {
            "gap-ab.bin"
        } else {
            "gap-bc.bin"
        };
        write_component(&directory, component, &bytes, name, &edition, manifest)?;
    }
    Ok(())
}

fn plan_records_edition(
    name: &str,
    block_size: u32,
    records: Records,
    writer_version: &str,
    write_timestamp: &str,
) -> Result<TapeIndexEditionPlan, Box<dyn std::error::Error>> {
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
    let descriptor = TapeIndexEditionDescriptor {
        tape_uuid: [0x11; 16],
        edition_id: match name {
            "minimal" => [0x21; 16],
            "multi" => [0x22; 16],
            _ => [0x23; 16],
        },
        edition_sequence: match name {
            "minimal" => 1,
            "multi" => 2,
            _ => 3,
        },
        scope,
        counts,
        block_size,
        compression_enabled: false,
        writer_version: writer_version.into(),
        write_timestamp: write_timestamp.into(),
        terminal_layout,
    };
    Ok(plan_tape_index_edition(descriptor, &mut records.clone())?)
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
    let plaintext = maximum_plaintext_slot()?;
    let encrypted = maximum_encrypted_slot()?;
    let block_size = TERMINAL_INDEX_BLOCK_SIZES[0];
    let records = minimal_records();
    let edition = plan_records_edition(
        "maximum-footer",
        block_size,
        records,
        &"V".repeat(128),
        MAX_TIMESTAMP,
    )?;
    let plan = plan_tape_index_replica(edition, 1)?;
    let header = encode_tape_index_replica_header(&plan)?;
    let footer = encode_tape_index_bootstrap_footer(
        &plan,
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

fn maximum_plaintext_slot() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    fixed_slot(CborValue::Map(vec![
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
    ]))
}

fn maximum_encrypted_slot() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    fixed_slot(CborValue::Map(vec![
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
    ]))
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
    let descriptor = TapeIndexEditionDescriptor {
        tape_uuid: [0x61; 16],
        edition_id: [0x62; 16],
        edition_sequence: 1,
        scope: TapeIndexReplicaScope {
            covered_prefix_tape_file_count: structural_rows,
            total_data_ordinals: HIGH_COUNT_OBJECT_ROWS,
            highest_protected_ordinal: 0,
        },
        counts,
        block_size,
        compression_enabled: false,
        writer_version: "synthetic-constant-storage-source/1".into(),
        write_timestamp: VECTOR_TIMESTAMP.into(),
        terminal_layout,
    };
    let mut source = SyntheticRecords {
        object_rows: HIGH_COUNT_OBJECT_ROWS,
        structural_passes: 0,
        object_passes: 0,
    };
    let edition = plan_tape_index_edition(descriptor, &mut source)?;
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
    fs::write(
        root.join("INTERRUPTIONS.tsv"),
        "case_id\tpresent_components\tfilemarks\tbarriers\texpected_progress\texpected_next\n\
before-a\t\t\t\tBeforeReplicaA\treplica-a.bin\n\
after-a-footer\treplica-a.bin\t\t\tBeforeReplicaA\treconcile-replica-a\n\
after-a-filemark\treplica-a.bin\treplica-a.bin\t\tBeforeReplicaA\treconcile-replica-a\n\
after-a-barrier\treplica-a.bin\treplica-a.bin\treplica-a.bin\tAfterReplicaA\tgap-ab.bin\n\
after-gap-ab-footer\treplica-a.bin,gap-ab.bin\treplica-a.bin\treplica-a.bin\tAfterReplicaA\treconcile-gap-ab\n\
after-gap-ab-filemark\treplica-a.bin,gap-ab.bin\treplica-a.bin,gap-ab.bin\treplica-a.bin\tAfterReplicaA\treconcile-gap-ab\n\
complete-gap-before-b\treplica-a.bin,gap-ab.bin\treplica-a.bin,gap-ab.bin\treplica-a.bin,gap-ab.bin\tAfterSeparationAb\treplica-b.bin\n\
after-b-footer\treplica-a.bin,gap-ab.bin,replica-b.bin\treplica-a.bin,gap-ab.bin\treplica-a.bin,gap-ab.bin\tAfterSeparationAb\treconcile-replica-b\n\
after-b-filemark\treplica-a.bin,gap-ab.bin,replica-b.bin\treplica-a.bin,gap-ab.bin,replica-b.bin\treplica-a.bin,gap-ab.bin\tAfterSeparationAb\treconcile-replica-b\n\
after-b-barrier\treplica-a.bin,gap-ab.bin,replica-b.bin\treplica-a.bin,gap-ab.bin,replica-b.bin\treplica-a.bin,gap-ab.bin,replica-b.bin\tAfterReplicaB\tgap-bc.bin\n\
after-gap-bc-footer\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin\treplica-a.bin,gap-ab.bin,replica-b.bin\treplica-a.bin,gap-ab.bin,replica-b.bin\tAfterReplicaB\treconcile-gap-bc\n\
after-gap-bc-filemark\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin\treplica-a.bin,gap-ab.bin,replica-b.bin\tAfterReplicaB\treconcile-gap-bc\n\
after-gap-bc-barrier\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin\tAfterSeparationBc\treplica-c.bin\n\
after-c-footer\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin,replica-c.bin\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin\tAfterSeparationBc\treconcile-replica-c\n\
after-c-filemark\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin,replica-c.bin\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin,replica-c.bin\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin\tAfterSeparationBc\treconcile-replica-c\n\
after-c-barrier\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin,replica-c.bin\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin,replica-c.bin\treplica-a.bin,gap-ab.bin,replica-b.bin,gap-bc.bin,replica-c.bin\tAfterReplicaC\tnone\n",
    )?;
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
