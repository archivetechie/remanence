//! Exercise file classification from addressed observations and directory proposals
//! without tape I/O, including the blocks each fallback is permitted to request.

use super::*;
use std::collections::BTreeMap;

/// Remove each observation on use so a repeated probe fails the test.
#[derive(Default)]
struct MemoryClassificationSource {
    blocks: BTreeMap<u64, ClassificationBlockRead>,
    requests: Vec<u64>,
}

impl MemoryClassificationSource {
    fn from_blocks(blocks: &[Vec<u8>]) -> Self {
        Self {
            blocks: blocks
                .iter()
                .enumerate()
                .map(|(position, block)| {
                    (
                        position as u64,
                        ClassificationBlockRead::Block(block.clone()),
                    )
                })
                .collect(),
            requests: Vec::new(),
        }
    }
}

impl ClassificationBlockSource for MemoryClassificationSource {
    fn read_block(
        &mut self,
        _file_start: PhysicalPositionHint,
        block_within_file: u64,
        block_size: u32,
    ) -> Result<ClassificationBlockRead, ParityError> {
        self.requests.push(block_within_file);
        let outcome = self.blocks.remove(&block_within_file).unwrap_or_else(|| {
            panic!("unexpected or repeated classification probe at {block_within_file}")
        });
        if let ClassificationBlockRead::Block(block) = &outcome {
            assert_eq!(block.len(), block_size as usize);
        }
        Ok(outcome)
    }
}

/// Classify a non-BOT file after two Object ordinals, retaining every finding.
fn classify(
    source: &mut dyn ClassificationBlockSource,
    head: Option<&[u8]>,
    block_count: u64,
    block_size: u32,
) -> (TapeFileMapEntry, Vec<ScanDamagedRegion>) {
    let mut builder = FilemarkMapBuilder::new();
    builder.push_bootstrap().unwrap();
    builder.push_object(2).unwrap();
    let mut damage = Vec::new();
    assert!(append_classified_file(
        source,
        &mut builder,
        head,
        &TAPE_UUID,
        block_size,
        PhysicalPositionHint::new(5),
        block_count,
        &mut damage,
    )
    .unwrap()
    .is_none());
    (builder.entries().last().unwrap().clone(), damage)
}

fn sidecar_blocks(stripes: u32) -> crate::EncodedSidecarTapeFile {
    crate::encode_sidecar_tape_file(
        &crate::SidecarDescriptor {
            tape_uuid: TAPE_UUID,
            epoch_id: 7,
            k: 2,
            m: 1,
            stripes_per_epoch: stripes,
            block_size: BLOCK_SIZE,
            protected_ordinal_start: 0,
            protected_ordinal_end_exclusive: 2 * u64::from(stripes),
        },
        &vec![block(0x33); stripes as usize],
        vec![0; 2 * stripes as usize],
    )
    .unwrap()
}

fn parity_map_blocks() -> crate::EncodedParityMapTapeFile {
    let (_, _, payload) = directory_scan_source();
    crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap()
}

/// Encode only the two framing blocks of a separation extent.
fn separation_frames() -> (Vec<u8>, Vec<u8>, u64) {
    use crate::index_separation::*;
    let block_size = 256 * 1024;
    let count = index_separation_records(block_size, DEFAULT_INDEX_SEPARATION_BYTES).unwrap();
    let plan = plan_index_separation(IndexSeparationDescriptor {
        tape_uuid: TAPE_UUID,
        edition_id: [0x11; 16],
        gap_ordinal: 1,
        block_size,
        nominal_extent_bytes: DEFAULT_INDEX_SEPARATION_BYTES,
        total_records: count,
        compression_enabled: false,
        terminal_layout: TerminalTailLayout::new(0, block_size, 20, 1_000, 7, count).unwrap(),
    })
    .unwrap();
    let head = encode_index_separation_header(&plan).unwrap();
    let header = parse_index_separation_header(&head, &TAPE_UUID).unwrap();
    let footer = encode_index_separation_footer(
        &plan,
        header.header_sha256,
        IndexSeparationObservation {
            tape_file_number: plan.component.planned_tape_file_number,
            start_lba: plan.component.planned_start_lba,
            record_count: count,
        },
    )
    .unwrap();
    (head, footer, count)
}

#[test]
fn seam_bootstrap_head_and_unreadable_bot_need_no_probes() {
    let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).unwrap();
    let mut head = bootstrap_block(map.digest(false).unwrap(), 0);
    *head.last_mut().unwrap() = 1;
    for readable in [true, false] {
        let mut source = MemoryClassificationSource::default();
        let mut builder = FilemarkMapBuilder::new();
        let mut damage = Vec::new();
        let candidate = append_classified_file(
            &mut source,
            &mut builder,
            readable.then_some(head.as_slice()),
            &TAPE_UUID,
            BLOCK_SIZE,
            PhysicalPositionHint::new(0),
            1,
            &mut damage,
        )
        .unwrap();
        assert_eq!(builder.build().unwrap(), map);
        assert_eq!(candidate.is_some(), readable);
        if let Some(candidate) = candidate {
            assert_eq!(candidate.tape_file_number, 0);
            assert!(candidate.nonzero_fill);
            assert_eq!(candidate.payload.tape_uuid, TAPE_UUID);
        }
        assert!(damage.is_empty());
        assert!(source.requests.is_empty());
    }
}

#[test]
fn seam_control_heads_need_no_probes() {
    let parity_map = parity_map_blocks();
    let sidecar = sidecar_blocks(1);
    let replica = terminal_replica_blocks();
    let (separation_head, _, separation_count) = separation_frames();
    for (head, count, expected) in [
        (
            &parity_map.blocks[0],
            parity_map.blocks.len() as u64,
            TapeFileKind::ParityMap,
        ),
        (
            &sidecar.blocks[0],
            sidecar.blocks.len() as u64,
            TapeFileKind::ParitySidecar,
        ),
        (
            &replica[0],
            replica.len() as u64,
            TapeFileKind::TapeIndexReplica,
        ),
        (
            &separation_head,
            separation_count,
            TapeFileKind::IndexSeparationExtent,
        ),
    ] {
        let mut source = MemoryClassificationSource::default();
        let (entry, damage) = classify(&mut source, Some(head), count, head.len() as u32);
        assert_eq!(entry.kind, expected);
        assert_eq!(entry.block_count, count);
        assert_eq!(entry.first_parity_data_ordinal, None);
        assert!(damage.is_empty());
        assert!(source.requests.is_empty());
    }
}

#[test]
fn seam_head_count_mismatch_stops_before_fallback() {
    let parity_map = parity_map_blocks();
    let sidecar = sidecar_blocks(1);
    for blocks in [parity_map.blocks, sidecar.blocks] {
        let mut source = MemoryClassificationSource::default();
        let count = blocks.len() as u64 + 1;
        let (entry, damage) = classify(&mut source, Some(&blocks[0]), count, BLOCK_SIZE);
        assert_eq!(entry, TapeFileMapEntry::object(2, count, 2));
        assert_eq!(
            damage,
            vec![ScanDamagedRegion {
                start: PhysicalPositionHint::new(5),
                block_count: count,
                kind: ScanDamageKind::ClassificationCountMismatch,
            }]
        );
        assert!(source.requests.is_empty());
    }
}

#[test]
fn seam_terminal_footers_preserve_reserved_types_and_damage() {
    let replica = terminal_replica_blocks();
    let (_, separation_footer, separation_count) = separation_frames();
    for (footer, count, expected) in [
        (
            replica.last().unwrap(),
            replica.len() as u64,
            TapeFileKind::TapeIndexReplica,
        ),
        (
            &separation_footer,
            separation_count,
            TapeFileKind::IndexSeparationExtent,
        ),
    ] {
        for bad_count in [false, true] {
            let measured = count + u64::from(bad_count);
            for head in [None, Some(vec![0x55; footer.len()])] {
                let mut source = MemoryClassificationSource::default();
                source
                    .blocks
                    .insert(measured - 1, ClassificationBlockRead::Block(footer.clone()));
                let (entry, damage) =
                    classify(&mut source, head.as_deref(), measured, footer.len() as u32);
                assert_eq!(entry.kind, expected);
                assert_eq!(entry.block_count, measured);
                assert_eq!(damage.len(), usize::from(bad_count));
                if bad_count {
                    assert_eq!(damage[0].kind, ScanDamageKind::InvalidTerminalControl);
                }
                assert_eq!(source.requests, [measured - 1]);
            }
        }
    }
}

#[test]
fn seam_parity_map_footer_reads_only_located_tail_header() {
    let encoded = parity_map_blocks();
    let count = encoded.blocks.len() as u64;
    for head in [None, Some(block(0x55))] {
        let mut source = MemoryClassificationSource::from_blocks(&encoded.blocks);
        let (entry, damage) = classify(&mut source, head.as_deref(), count, BLOCK_SIZE);
        assert_eq!(entry, TapeFileMapEntry::parity_map(2, count));
        assert!(damage.is_empty());
        assert_eq!(
            source.requests,
            [count - 1, encoded.header.tail_copy_start_block]
        );
    }
}

#[test]
fn seam_parity_map_tail_requires_valid_tail_copy() {
    let encoded = parity_map_blocks();
    let count = encoded.blocks.len() as u64;
    for tail in [
        ClassificationBlockRead::Unreadable,
        ClassificationBlockRead::Block(block(0x55)),
        ClassificationBlockRead::Block(encoded.blocks[0].clone()),
    ] {
        let mut source = MemoryClassificationSource::from_blocks(&encoded.blocks);
        source
            .blocks
            .insert(encoded.header.tail_copy_start_block, tail);
        let (entry, damage) = classify(&mut source, None, count, BLOCK_SIZE);
        assert_eq!(entry, TapeFileMapEntry::object(2, count, 2));
        assert!(damage.is_empty());
        assert_eq!(
            source.requests,
            [count - 1, encoded.header.tail_copy_start_block]
        );
    }
}

#[test]
fn seam_sidecar_footer_reads_each_located_header_block() {
    let encoded = sidecar_blocks(128);
    let header = &encoded.header;
    let count = header.sidecar_total_block_count;
    assert!(header.shard_index_block_count > 1);
    for head in [None, Some(block(0x55))] {
        let mut source = MemoryClassificationSource::from_blocks(&encoded.blocks);
        let (entry, damage) = classify(&mut source, head.as_deref(), count, BLOCK_SIZE);
        assert_eq!(entry, TapeFileMapEntry::parity_sidecar(2, count, 7, 0, 256));
        assert!(damage.is_empty());
        let mut expected = vec![count - 1];
        expected.extend(
            header.tail_header_start_block
                ..header.tail_header_start_block + header.shard_index_block_count,
        );
        assert_eq!(source.requests, expected);
    }
}

#[test]
fn seam_sidecar_tail_distinguishes_unavailable_from_invalid() {
    let encoded = sidecar_blocks(128);
    let header = &encoded.header;
    let count = header.sidecar_total_block_count;
    let tail_position = header.tail_header_start_block + 1;
    for tail in [
        ClassificationBlockRead::Unreadable,
        ClassificationBlockRead::WrongLength { measured_bytes: 31 },
        ClassificationBlockRead::Filemark,
        ClassificationBlockRead::EndOfData,
        ClassificationBlockRead::Block(block(0x55)),
    ] {
        let invalid = matches!(tail, ClassificationBlockRead::Block(_));
        let mut source = MemoryClassificationSource::from_blocks(&encoded.blocks);
        source.blocks.insert(tail_position, tail);
        let (entry, damage) = classify(&mut source, None, count, BLOCK_SIZE);
        if invalid {
            assert_eq!(entry, TapeFileMapEntry::object(2, count, 2));
            assert_eq!(damage.len(), 1);
            assert_eq!(damage[0].kind, ScanDamageKind::ClassificationTailMismatch);
        } else {
            assert_eq!(entry, TapeFileMapEntry::parity_sidecar(2, count, 7, 0, 256));
            assert!(damage.is_empty());
            assert_eq!(
                source.requests,
                [count - 1, header.tail_header_start_block, tail_position]
            );
        }
        assert_eq!(
            source
                .requests
                .iter()
                .filter(|&&pos| pos == count - 1)
                .count(),
            1
        );
    }
}

#[test]
fn seam_footer_count_mismatch_does_not_read_tail() {
    let parity_map = parity_map_blocks();
    let sidecar = sidecar_blocks(1);
    for blocks in [parity_map.blocks, sidecar.blocks] {
        let count = blocks.len() as u64 + 1;
        let mut source = MemoryClassificationSource::default();
        source.blocks.insert(
            count - 1,
            ClassificationBlockRead::Block(blocks.last().unwrap().clone()),
        );
        let (entry, damage) = classify(&mut source, None, count, BLOCK_SIZE);
        assert_eq!(entry, TapeFileMapEntry::object(2, count, 2));
        assert_eq!(damage.len(), 1);
        assert_eq!(damage[0].kind, ScanDamageKind::ClassificationCountMismatch);
        assert_eq!(source.requests, [count - 1]);
    }
}

#[test]
fn seam_object_fallback_shares_every_last_block_outcome() {
    for readable_head in [true, false] {
        for footer in [
            ClassificationBlockRead::Block(block(0x55)),
            ClassificationBlockRead::Unreadable,
            ClassificationBlockRead::WrongLength {
                measured_bytes: 1000,
            },
            ClassificationBlockRead::Filemark,
            ClassificationBlockRead::EndOfData,
        ] {
            let mut source = MemoryClassificationSource::default();
            source.blocks.insert(4, footer);
            let head = block(0x55);
            let (entry, damage) = classify(
                &mut source,
                readable_head.then_some(head.as_slice()),
                5,
                BLOCK_SIZE,
            );
            assert_eq!(entry, TapeFileMapEntry::object(2, 5, 2));
            assert!(damage.is_empty());
            assert_eq!(source.requests, [4]);
        }
    }
}

#[test]
fn positioned_seam_preserves_read_outcomes_and_error_precedence() {
    for (record, expected) in [
        (
            Some(Record::Block(block(0x55))),
            ClassificationBlockRead::Block(block(0x55)),
        ),
        (
            Some(Record::ReadFault(TestReadFault::Medium)),
            ClassificationBlockRead::Unreadable,
        ),
        (
            Some(Record::Block(vec![0; 31])),
            ClassificationBlockRead::WrongLength { measured_bytes: 31 },
        ),
        (
            Some(Record::Block(vec![0; 1000])),
            ClassificationBlockRead::WrongLength {
                measured_bytes: 1000,
            },
        ),
        (Some(Record::Filemark), ClassificationBlockRead::Filemark),
        (None, ClassificationBlockRead::EndOfData),
    ] {
        let mut records = vec![Record::Block(block(0x11)); 5];
        records.extend(record);
        let mut source = RecordingRawSource::new(records);
        let result = PositionedClassificationSource {
            source: &mut source,
        }
        .read_block(PhysicalPositionHint::new(2), 3, BLOCK_SIZE)
        .unwrap();
        assert_eq!(result, expected);
        assert_eq!(source.calls, [ScanCall::Locate(5), ScanCall::ReadRecord(5)]);
    }
    for fault in [
        TestReadFault::Hardware,
        TestReadFault::Transport,
        TestReadFault::DeferredFixedMedium,
        TestReadFault::DeferredDescriptorMedium,
    ] {
        let mut source = RecordingRawSource::new(vec![Record::ReadFault(fault)]);
        assert!(matches!(
            PositionedClassificationSource {
                source: &mut source
            }
            .read_block(PhysicalPositionHint::new(0), 0, BLOCK_SIZE),
            Err(ParityError::TapeIo(_))
        ));
    }
    let mut source = RecordingRawSource::new(vec![Record::Block(block(0x55))]);
    source.locate_fault = Some((0, TestReadFault::Medium));
    assert!(matches!(
        PositionedClassificationSource {
            source: &mut source
        }
        .read_block(PhysicalPositionHint::new(0), 0, BLOCK_SIZE),
        Err(ParityError::TapeIo(_))
    ));
    assert_eq!(source.calls, [ScanCall::Locate(0)]);
}

/// Include Objects both inside and outside the final directory's scope.
fn reconciliation_fixture() -> (FilemarkMap, DecodedParityMapTapeFile) {
    let (_, map, mut payload) = directory_scan_source();
    let mut entries = map.entries().to_vec();
    entries.insert(3, TapeFileMapEntry::object(3, 3, 2));
    entries[4].tape_file_number = 4;
    let prefix = FilemarkMap::new(entries.clone()).unwrap();
    payload.directory.directory_scope_tape_file_count = 5;
    payload.directory.directory_scope_total_data_ordinals = 5;
    payload.canonical_map_digest = prefix.canonical_digest().unwrap();
    let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
    assert_eq!(encoded.blocks.len() as u64, entries[4].block_count);
    let decoded = crate::parse_parity_map_tape_file(&encoded.blocks, &TAPE_UUID).unwrap();
    entries.push(TapeFileMapEntry::object(5, 4, 5));
    (FilemarkMap::new(entries).unwrap(), decoded)
}

fn object_candidate(map: &FilemarkMap, extra_blocks: u64) -> FilemarkMap {
    let mut entries = map.entries().to_vec();
    let count = entries[2].block_count + extra_blocks;
    entries[2] = TapeFileMapEntry::object(2, count, 2);
    entries[3].first_parity_data_ordinal = Some(2 + count);
    entries[5].first_parity_data_ordinal = Some(5 + count);
    FilemarkMap::new(entries).unwrap()
}

#[test]
fn pure_reconciliation_preserves_healthy_map() {
    let (map, decoded) = reconciliation_fixture();
    let proposed = reconcile_measured_sidecars(&map, &decoded)
        .unwrap()
        .unwrap();
    assert_eq!(proposed, map);
    assert_eq!(
        ScopedFilemarkMap::validate_against_final_parity_map(proposed, &decoded)
            .unwrap()
            .map,
        map
    );
}

#[test]
fn pure_reconciliation_identifies_sidecar_and_recounts_later_objects() {
    let (expected, decoded) = reconciliation_fixture();
    let measured = object_candidate(&expected, 0);
    let snapshot = measured.clone();
    let proposed = reconcile_measured_sidecars(&measured, &decoded)
        .unwrap()
        .unwrap();
    assert_eq!(proposed, expected);
    assert_eq!(measured, snapshot);
    assert_eq!(proposed.entries()[3].first_parity_data_ordinal, Some(2));
    assert_eq!(proposed.entries()[5].first_parity_data_ordinal, Some(5));
    assert_eq!(
        ScopedFilemarkMap::validate_against_final_parity_map(proposed, &decoded)
            .unwrap()
            .map,
        expected
    );
}

#[test]
fn pure_reconciliation_keeps_length_mismatch_as_object() {
    let (expected, decoded) = reconciliation_fixture();
    let measured = object_candidate(&expected, 1);
    let proposed = reconcile_measured_sidecars(&measured, &decoded)
        .unwrap()
        .unwrap();
    assert_eq!(proposed, measured);
    assert!(matches!(
        ScopedFilemarkMap::validate_against_final_parity_map(proposed, &decoded),
        Err(ParityError::FilemarkMapDigestMismatch { .. })
    ));
}

#[test]
fn pure_reconciliation_proposal_requires_whole_map_validation() {
    let (expected, decoded) = reconciliation_fixture();
    let measured = object_candidate(&expected, 0);
    for mismatch in ["digest", "total", "watermark", "count", "final"] {
        let mut invalid = decoded.clone();
        match mismatch {
            "digest" => invalid.payload.canonical_map_digest[0] ^= 1,
            "total" => {
                invalid
                    .payload
                    .directory
                    .directory_scope_total_data_ordinals += 1
            }
            "watermark" => {
                invalid
                    .payload
                    .directory
                    .directory_scope_highest_protected_ordinal -= 1;
                invalid.payload.directory.entries[0].protected_ordinal_end_exclusive -= 1;
            }
            "count" => invalid.header.parity_map_total_block_count += 1,
            "final" => invalid.payload.directory.is_final_directory = false,
            _ => unreachable!(),
        }
        let snapshot = measured.clone();
        let proposed = reconcile_measured_sidecars(&measured, &invalid)
            .unwrap()
            .unwrap();
        assert_eq!(proposed.entries()[2].kind, TapeFileKind::ParitySidecar);
        assert!(
            matches!(
                ScopedFilemarkMap::validate_against_final_parity_map(proposed, &invalid),
                Err(ParityError::FilemarkMapDigestMismatch { .. })
            ),
            "{mismatch}"
        );
        assert_eq!(measured, snapshot);
    }
}

#[test]
fn pure_reconciliation_requires_scope_at_last_classified_parity_map() {
    let (expected, mut decoded) = reconciliation_fixture();
    let measured = object_candidate(&expected, 0);
    decoded.payload.directory.directory_scope_tape_file_count -= 1;
    assert!(reconcile_measured_sidecars(&measured, &decoded)
        .unwrap()
        .is_none());
    let (_, decoded) = reconciliation_fixture();
    let mut entries = measured.entries().to_vec();
    entries.push(TapeFileMapEntry::parity_map(6, entries[4].block_count));
    assert!(
        reconcile_measured_sidecars(&FilemarkMap::new(entries).unwrap(), &decoded)
            .unwrap()
            .is_none()
    );
}
