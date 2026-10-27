//! Differential terminal fixtures with independently encoded, consistent replicas.
use super::*;

struct Rows(Vec<TapeIndexReplicaMapEntry>);
impl TapeIndexReplicaRecordSource for Rows {
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        for row in &self.0 {
            visitor(row)?;
        }
        Ok(())
    }
    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        for row in self
            .0
            .iter()
            .filter(|row| row.kind == TapeIndexReplicaFileKind::Object)
        {
            visitor(&TapeIndexReplicaObjectRow {
                tape_file_number: row.tape_file_number,
                stored_block_count: row.block_count,
                object_id: format!("object-{}", row.tape_file_number).into_bytes(),
                representation: crate::ObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: 0,
                    manifest_size_bytes: 1,
                    manifest_chunk_count: 1,
                    manifest_sha256: [0x33; 32],
                },
            })?;
        }
        Ok(())
    }
}

/// Build all replicas and separations against the supplied rows and actual tail start.
fn fixture(actual: &[u64], recorded: &[u64], different_kind: bool) -> TripleFixture {
    assert_eq!(actual.len(), recorded.len());
    assert_eq!(actual.iter().sum::<u64>(), recorded.iter().sum::<u64>());
    let mut rows = Rows(vec![TapeIndexReplicaMapEntry {
        tape_file_number: 0,
        kind: TapeIndexReplicaFileKind::Bootstrap,
        block_count: 1,
        first_parity_data_ordinal: None,
        protected_ordinal_start: None,
        protected_ordinal_end_exclusive: None,
        epoch_id: None,
    }]);
    let mut ordinal = 0;
    for (i, &count) in recorded.iter().enumerate() {
        let object = !(different_kind && i >= 1);
        let sidecar = different_kind && i == 1;
        rows.0.push(TapeIndexReplicaMapEntry {
            tape_file_number: i as u64 + 1,
            kind: if object {
                TapeIndexReplicaFileKind::Object
            } else if sidecar {
                TapeIndexReplicaFileKind::ParitySidecar
            } else {
                TapeIndexReplicaFileKind::ParityMap
            },
            block_count: count,
            first_parity_data_ordinal: object.then_some(ordinal),
            protected_ordinal_start: sidecar.then_some(0),
            protected_ordinal_end_exclusive: sidecar.then_some(ordinal),
            epoch_id: sidecar.then_some(0),
        });
        if object {
            ordinal += count;
        }
    }
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: rows.0.len() as u64,
        object_row_count: rows
            .0
            .iter()
            .filter(|row| row.kind == TapeIndexReplicaFileKind::Object)
            .count() as u64,
    };
    let replica_records = checked_tape_index_replica_layout(BLOCK_SIZE, counts)
        .unwrap()
        .replica_record_count;
    let start = 2 + actual.iter().map(|count| count + 1).sum::<u64>();
    let layout = TerminalTailLayout::new(
        0,
        BLOCK_SIZE,
        counts.structural_entry_count,
        start,
        replica_records,
        3,
    )
    .unwrap();
    let mut descriptor = edition_plan(triple_fixture().layout, [0x72; 16]).descriptor;
    descriptor.counts = counts;
    descriptor.scope = TapeIndexReplicaScope {
        covered_prefix_tape_file_count: counts.structural_entry_count,
        total_data_ordinals: ordinal,
        highest_protected_ordinal: if different_kind { recorded[0] } else { 0 },
    };
    descriptor.terminal_layout = layout;
    let edition = plan_tape_index_edition(descriptor, &mut rows).unwrap();
    let mut records = triple_fixture().records[..2].to_vec();
    for &count in actual {
        records.extend((0..count).map(|_| Record::Block(vec![0x35; BLOCK_SIZE as usize])));
        records.push(Record::Filemark);
    }
    for ordinal in 1..=3 {
        let plan = plan_tape_index_replica(edition.clone(), ordinal).unwrap();
        write_tape_index_replica(
            &plan,
            TapeIndexReplicaObservation {
                tape_file_number: plan.component.planned_tape_file_number,
                start_lba: plan.component.planned_start_lba,
                record_count: plan.component.record_count,
            },
            &mut rows,
            |block| {
                records.push(Record::Block(block.to_vec()));
                Ok(())
            },
        )
        .unwrap();
        records.push(Record::Filemark);
        if ordinal < 3 {
            let plan = crate::plan_index_separation(crate::IndexSeparationDescriptor {
                tape_uuid: TAPE_UUID,
                edition_id: edition.descriptor.edition_id,
                gap_ordinal: ordinal,
                block_size: BLOCK_SIZE,
                nominal_extent_bytes: u64::from(BLOCK_SIZE) * 3,
                total_records: 3,
                compression_enabled: false,
                terminal_layout: layout,
            })
            .unwrap();
            crate::write_index_separation(
                &plan,
                crate::IndexSeparationObservation {
                    tape_file_number: plan.component.planned_tape_file_number,
                    start_lba: plan.component.planned_start_lba,
                    record_count: plan.component.record_count,
                },
                |block| {
                    records.push(Record::Block(block.to_vec()));
                    Ok(())
                },
            )
            .unwrap();
            records.push(Record::Filemark);
        }
    }
    TripleFixture { records, layout }
}

#[test]
fn rebuilt_rows_require_independent_measurement() {
    let mut success = 0;
    let mut fallback = 0;
    let mut reasons = std::collections::BTreeMap::new();
    for (name, actual, recorded, kind, expected_fallback) in [
        ("healthy", vec![4, 4], vec![4, 4], false, false),
        ("row_longer", vec![4, 4], vec![5, 3], false, true),
        ("row_shorter", vec![4, 4], vec![3, 5], false, true),
        ("different_kind", vec![4, 7, 3], vec![4, 7, 3], true, true),
        (
            "extra_filemark_inside_object",
            vec![2, 2, 3],
            vec![4, 1, 2],
            false,
            true,
        ),
        (
            "extra_one_block_file_before_a",
            vec![4, 4, 1],
            vec![4, 4, 1],
            false,
            false,
        ),
    ] {
        let fixture = fixture(&actual, &recorded, kind);
        // Every rebuilt member validates before the physical-prefix disagreement.
        let collected = verify_terminal_replicas(
            &mut RecordingSource::new(fixture.records.clone()),
            &TAPE_UUID,
            BLOCK_SIZE,
            fixture.layout,
            ReplicaRowCheck::CollectOnly,
        )
        .unwrap();
        assert_eq!(collected.rows[0], collected.rows[1]);
        assert_eq!(collected.rows[1], collected.rows[2]);
        let expected = super::super::verify_terminal_index_full(
            &mut RecordingSource::new(fixture.records.clone()),
            &TAPE_UUID,
            BLOCK_SIZE,
        );
        let mut fell_back = false;
        let actual = verify_terminal_index_full_with_route(
            &mut RecordingSource::new(fixture.records),
            &TAPE_UUID,
            BLOCK_SIZE,
            None,
            ScanMode::Standard,
            VerificationRoute::OnePass,
            |status| {
                fell_back |= status.fell_back;
                if let Some(reason) = status.fallback_reason {
                    *reasons.entry(reason).or_insert(0usize) += 1;
                }
            },
        );
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"), "{name}");
        assert_eq!(fell_back, expected_fallback && kind, "{name}");
        if fell_back {
            fallback += 1
        } else {
            success += 1
        }
    }
    println!("rebuilt fixtures: success={success}, fallback={fallback}, reasons={reasons:?}");
}

#[test]
fn deferred_comparison_checks_later_rows_and_replica_two() {
    let fixture = fixture(&[4, 4], &[4, 4], false);
    let mut collected = verify_terminal_replicas(
        &mut RecordingSource::new(fixture.records.clone()),
        &TAPE_UUID,
        BLOCK_SIZE,
        fixture.layout,
        ReplicaRowCheck::CollectOnly,
    )
    .unwrap();
    let measured = [
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::object(2, 4, 4),
    ];
    for ordinal in [1, 2, 3] {
        check_rows_against_measured(
            ordinal,
            &collected.rows[usize::from(ordinal - 1)],
            &measured,
        )
        .unwrap();
    }
    collected.rows[1][2].block_count += 1;
    let error = check_rows_against_measured(2, &collected.rows[1], &measured).unwrap_err();
    assert!(matches!(
        error,
        TerminalIndexVerificationError::CanonicalMapMismatch {
            ordinal: 2,
            tape_file_number: 2,
            ..
        }
    ));
    check_rows_against_measured(1, &collected.rows[0], &measured).unwrap();
    check_rows_against_measured(3, &collected.rows[2], &measured).unwrap();
}

/// Repeated Object bytes make a shifted stream indistinguishable until its corrupt last block.
fn protected_fixture() -> TripleFixture {
    let mut fixture = fixture(&[4, 7, 3], &[4, 7, 3], true);
    let scheme = crate::ParityScheme {
        id: crate::SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 2,
        stripes_per_neighborhood: 2,
    };
    let mut bootstrap = vec![0; BLOCK_SIZE as usize];
    crate::bootstrap::write_bootstrap_block(
        &crate::BootstrapPayload {
            scheme: Some(crate::bootstrap::ParitySchemeRecord {
                id: "rs-cauchy-gf256-v1".into(),
                data_blocks_per_stripe: 2,
                parity_blocks_per_stripe: 2,
                stripes_per_neighborhood: 2,
                no_parity_flag: false,
            }),
            no_parity_flag: false,
            filemark_map_digest: Some(crate::filemark_map::sole_bot_filemark_map_digest().unwrap()),
            tape_uuid: TAPE_UUID,
            written_by_version: "verification-test".into(),
            written_at: "2026-08-09T00:00:00Z".into(),
            sequence: 0,
            block_size_bytes: BLOCK_SIZE,
            drive_compression: false,
        },
        &mut bootstrap,
    )
    .unwrap();
    fixture.records[0] = Record::Block(bootstrap);
    let block = vec![0x35; BLOCK_SIZE as usize];
    let codec = crate::codec::ReedSolomonCodec::new(&scheme).unwrap();
    let mut parity = codec.encode(&[block.clone(), block.clone()]).unwrap();
    parity.extend(parity.clone());
    let sidecar = crate::sidecar::encode_sidecar_tape_file(
        &crate::SidecarDescriptor {
            tape_uuid: TAPE_UUID,
            epoch_id: 0,
            k: 2,
            m: 2,
            stripes_per_epoch: 2,
            block_size: BLOCK_SIZE,
            protected_ordinal_start: 0,
            protected_ordinal_end_exclusive: 4,
        },
        &parity,
        vec![crate::sidecar::data_shard_crc64(&block); 4],
    )
    .unwrap();
    assert_eq!(sidecar.blocks.len(), 7);
    for (record, block) in fixture.records[7..14].iter_mut().zip(&sidecar.blocks) {
        *record = Record::Block(block.clone());
    }
    let mut entries = vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::parity_sidecar(2, 7, 0, 0, 4),
    ];
    entries.push(TapeFileMapEntry::parity_map(entries.len() as u64, 3));
    let map = crate::FilemarkMap::new(entries).unwrap();
    let payload = crate::ParityMapPayload {
        tape_uuid: TAPE_UUID,
        sequence: 0,
        canonical_map_digest: map.canonical_digest().unwrap(),
        writer_version: None,
        write_timestamp: None,
        directory: crate::SidecarEpochDirectory {
            directory_scope_tape_file_count: map.tape_file_count(),
            directory_scope_total_data_ordinals: map.total_data_ordinals(),
            directory_scope_highest_protected_ordinal: 4,
            is_final_directory: true,
            entries: vec![crate::SidecarEpochDirectoryEntry {
                tape_file_number: 2,
                epoch_id: 0,
                protected_ordinal_start: 0,
                protected_ordinal_end_exclusive: 4,
                sidecar_total_block_count: 7,
                sidecar_header_block_count: 1,
                parity_shard_block_count: 4,
                canonical_metadata_hash: sidecar.header.canonical_metadata_hash,
                flags: 0,
            }],
        },
    };
    let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
    assert_eq!(encoded.blocks.len(), 3);
    let map_start = 15;
    for (record, block) in fixture.records[map_start..map_start + 3]
        .iter_mut()
        .zip(encoded.blocks)
    {
        *record = Record::Block(block);
    }
    fixture
}

#[test]
fn spaced_file_retracts_duplicate_and_clean_prefix_observations() {
    for duplicate in [true, false] {
        let mut fixture = protected_fixture();
        if let Record::Block(bytes) = &mut fixture.records[5] {
            bytes[100] ^= 1;
        }
        let mut walk = RecordingSource::new(fixture.records.clone());
        let mut observed = RecordingSource::new(fixture.records);
        if duplicate {
            observed.duplicate_at = Some(4);
        } else {
            walk.read_fault = Some((4, TestReadFault::Medium));
            observed.read_fault = walk.read_fault;
        }
        let expected =
            super::super::verify_terminal_index_full(&mut walk, &TAPE_UUID, BLOCK_SIZE).unwrap();
        let mut status = None;
        let mut wrapped = crate::raw::CancellableRawSource(observed);
        let actual = verify_terminal_index_full_with_route(
            &mut wrapped,
            &TAPE_UUID,
            BLOCK_SIZE,
            None,
            ScanMode::Standard,
            VerificationRoute::OnePass,
            |s| status = Some(s),
        )
        .unwrap();
        assert!(!status.unwrap().fell_back);
        assert_eq!(actual, expected);
        assert!(actual
            .protected()
            .failed_data_blocks()
            .any(|failure| failure.position.block_within_file == 3));
        // The clean prefix is read again after the failed measurement, too.
        assert!(wrapped.0.read_lbas.iter().filter(|&&lba| lba == 3).count() >= 2);
    }
}

#[test]
fn no_parity_hardware_fault_in_object_interior_uses_walk() {
    let fixture = fixture(&[4], &[4], false);
    let mut walk =
        RecordingSource::new(fixture.records.clone()).with_read_fault(3, TestReadFault::Hardware);
    let expected =
        super::super::verify_terminal_index_full(&mut walk, &TAPE_UUID, BLOCK_SIZE).unwrap();
    assert!(expected.is_complete_tape());
    let mut actual_source =
        RecordingSource::new(fixture.records).with_read_fault(3, TestReadFault::Hardware);
    let mut status = None;
    let actual = verify_terminal_index_full_with_route(
        &mut actual_source,
        &TAPE_UUID,
        BLOCK_SIZE,
        None,
        ScanMode::Standard,
        VerificationRoute::OnePass,
        |s| status = Some(s),
    )
    .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        status.unwrap(),
        VerificationRouteStatus {
            route: VerificationRoute::Walk,
            fell_back: false,
            fallback_reason: Some("no_parity")
        }
    );
    assert!(!actual_source.read_lbas.contains(&3));
}

#[test]
fn prefix_truncation_wins_over_replica_payload_transport_fault() {
    let mut fixture = protected_fixture();
    fixture.records[2] = Record::Filemark;
    let payload_b = fixture.layout.components[2].planned_start_lba + 1;
    let mut walk = RecordingSource::new(fixture.records.clone())
        .with_read_fault(payload_b, TestReadFault::Transport);
    let expected =
        super::super::verify_terminal_index_full(&mut walk, &TAPE_UUID, BLOCK_SIZE).unwrap();
    assert!(matches!(
        expected,
        TerminalIndexVerificationOutcome::RecoveryRequired(_)
    ));
    assert!(!walk.read_lbas.contains(&payload_b));
    let mut observed =
        RecordingSource::new(fixture.records).with_read_fault(payload_b, TestReadFault::Transport);
    let mut status = None;
    let actual = verify_terminal_index_full_with_route(
        &mut observed,
        &TAPE_UUID,
        BLOCK_SIZE,
        None,
        ScanMode::Standard,
        VerificationRoute::OnePass,
        |s| status = Some(s),
    )
    .unwrap();
    assert_eq!(actual, expected);
    assert!(status.unwrap().fell_back);
    assert!(observed.read_lbas.contains(&payload_b));
}

#[test]
fn directory_with_wrong_map_digest_still_judges_sidecar_footer() {
    let mut fixture = protected_fixture();
    let blocks: Vec<_> = fixture.records[15..18]
        .iter()
        .map(|record| match record {
            Record::Block(b) => b.clone(),
            _ => unreachable!(),
        })
        .collect();
    let mut decoded = crate::parse_parity_map_tape_file(&blocks, &TAPE_UUID).unwrap();
    decoded.payload.canonical_map_digest[0] ^= 1;
    decoded.payload.directory.entries[0].canonical_metadata_hash[0] ^= 1;
    let encoded = crate::encode_parity_map_tape_file(&decoded.payload, BLOCK_SIZE).unwrap();
    for (record, block) in fixture.records[15..18].iter_mut().zip(encoded.blocks) {
        *record = Record::Block(block);
    }
    let mut source = RecordingSource::new(fixture.records.clone());
    let replica = verify_terminal_replicas(
        &mut source,
        &TAPE_UUID,
        BLOCK_SIZE,
        fixture.layout,
        ReplicaRowCheck::CollectOnly,
    )
    .unwrap();
    let scoped = scoped_map_from_terminal_replica(&replica.edition, &replica.rows[0]).unwrap();
    assert!(crate::ScopedFilemarkMap::validate_against_final_parity_map(
        scoped.map.clone(),
        &decoded
    )
    .is_err());
    let mut cache = crate::recovery::DirectoryCache::default();
    cache
        .preload(&mut source, &scoped, &TAPE_UUID, BLOCK_SIZE)
        .unwrap();
    assert!(matches!(cache, crate::recovery::DirectoryCache::Loaded(_)));
    let expected = super::super::verify_terminal_index_full(
        &mut RecordingSource::new(fixture.records.clone()),
        &TAPE_UUID,
        BLOCK_SIZE,
    )
    .unwrap();
    let mut status = None;
    let actual = verify_terminal_index_full_with_route(
        &mut RecordingSource::new(fixture.records),
        &TAPE_UUID,
        BLOCK_SIZE,
        None,
        ScanMode::Standard,
        VerificationRoute::OnePass,
        |s| status = Some(s),
    )
    .unwrap();
    assert!(!status.unwrap().fell_back);
    assert_eq!(actual, expected);
    assert!(!actual.is_complete_tape());
}

/// Finalized replica encoding forbids unprotected suffixes. Exercise permission
/// at the prefix seam and fallback on the corresponding unfinalized medium.
#[test]
fn unprotected_suffix_object_is_spaced() {
    let mut fixture = protected_fixture();
    fixture.records.truncate(19);
    fixture.records.splice(
        15..15,
        (0..4)
            .map(|_| Record::Block(vec![0x35; BLOCK_SIZE as usize]))
            .chain(std::iter::once(Record::Filemark)),
    );
    let map = crate::FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::parity_sidecar(2, 7, 0, 0, 4),
        TapeFileMapEntry::object(3, 4, 4),
        TapeFileMapEntry::parity_map(4, 3),
    ])
    .unwrap();
    let rows: Vec<_> = map
        .entries()
        .iter()
        .map(|e| TapeIndexReplicaMapEntry {
            tape_file_number: e.tape_file_number,
            block_count: e.block_count,
            kind: match e.kind {
                TapeFileKind::Bootstrap => TapeIndexReplicaFileKind::Bootstrap,
                TapeFileKind::Object => TapeIndexReplicaFileKind::Object,
                TapeFileKind::ParitySidecar => TapeIndexReplicaFileKind::ParitySidecar,
                TapeFileKind::ParityMap => TapeIndexReplicaFileKind::ParityMap,
                _ => unreachable!(),
            },
            first_parity_data_ordinal: e.first_parity_data_ordinal,
            epoch_id: e.epoch_id,
            protected_ordinal_start: e.protected_ordinal_start,
            protected_ordinal_end_exclusive: e.protected_ordinal_end_exclusive,
        })
        .collect();
    let rows = [rows.clone(), rows.clone(), rows];
    let mut source =
        RecordingSource::new(fixture.records.clone()).with_read_fault(16, TestReadFault::Hardware);
    let scoped = crate::ScopedFilemarkMap::from_catalog(map.clone(), 4);
    let mut cache = crate::recovery::DirectoryCache::default();
    cache
        .preload(&mut source, &scoped, &TAPE_UUID, BLOCK_SIZE)
        .unwrap();
    let mut checks = crate::verify_protected::one_pass::PrefixChecks::new(
        scoped,
        &rows,
        24,
        TAPE_UUID,
        BLOCK_SIZE,
        64 * 1024 * 1024,
        cache,
    )
    .unwrap();
    let measured = crate::scan::scan_observed_prefix(
        &mut source,
        &TAPE_UUID,
        BLOCK_SIZE,
        ScanMode::Standard,
        &mut checks,
        None,
    )
    .unwrap();
    assert_eq!(measured.map, map);
    assert!(!source.read_lbas.contains(&16));
    assert!(!source.read_lbas.contains(&17));
    assert_eq!(source.spaced_from.iter().filter(|&&p| p == 15).count(), 1);
    let mut walk =
        RecordingSource::new(fixture.records.clone()).with_read_fault(16, TestReadFault::Hardware);
    let expected = super::super::verify_terminal_index_full(&mut walk, &TAPE_UUID, BLOCK_SIZE);
    let mut observed =
        RecordingSource::new(fixture.records).with_read_fault(16, TestReadFault::Hardware);
    let mut status = None;
    let actual = verify_terminal_index_full_with_route(
        &mut observed,
        &TAPE_UUID,
        BLOCK_SIZE,
        None,
        ScanMode::Standard,
        VerificationRoute::OnePass,
        |s| status = Some(s),
    );
    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    assert!(status.unwrap().fell_back);
    assert!(!observed.read_lbas.contains(&16));
}

/// A sidecar measured by spacing also discards every streamed parity shard.
#[test]
fn spaced_sidecar_retracts_clean_shards() {
    let fixture = protected_fixture();
    let mut walk =
        RecordingSource::new(fixture.records.clone()).with_read_fault(9, TestReadFault::Medium);
    let expected =
        super::super::verify_terminal_index_full(&mut walk, &TAPE_UUID, BLOCK_SIZE).unwrap();
    let mut observed =
        RecordingSource::new(fixture.records).with_read_fault(9, TestReadFault::Medium);
    let mut status = None;
    let actual = verify_terminal_index_full_with_route(
        &mut observed,
        &TAPE_UUID,
        BLOCK_SIZE,
        None,
        ScanMode::Standard,
        VerificationRoute::OnePass,
        |s| status = Some(s),
    )
    .unwrap();
    assert_eq!(actual, expected);
    assert!(!status.unwrap().fell_back);
    assert!(observed.read_lbas.iter().filter(|&&p| p == 8).count() >= 2);
}

/// Cancellation preserves its typed result at both index and prefix boundaries.
#[test]
fn one_pass_cancellation_does_not_fall_back() {
    for lba in [3, 16, 20] {
        let fixture = protected_fixture();
        let mut source = crate::raw::CancellableRawSource(
            RecordingSource::new(fixture.records).with_read_fault(lba, TestReadFault::Cancelled),
        );
        let mut fell_back = false;
        let result = verify_terminal_index_full_with_route(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            None,
            ScanMode::Standard,
            VerificationRoute::OnePass,
            |s| fell_back |= s.fell_back,
        );
        assert!(
            matches!(result, Err(TerminalIndexVerificationError::Cancelled)),
            "{result:?}"
        );
        assert!(!fell_back);
    }
}
