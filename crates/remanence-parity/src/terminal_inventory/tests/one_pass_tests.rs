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
    let mut records = triple_fixture().records[..2].to_vec();
    for &count in actual {
        records.extend((0..count).map(|_| Record::Block(vec![0x35; BLOCK_SIZE as usize])));
        records.push(Record::Filemark);
    }
    rebuild(records, rows)
}

/// Encode all terminal components from rows without consulting the physical prefix.
fn rebuild(mut records: Vec<Record>, mut rows: Rows) -> TripleFixture {
    let ordinal = rows
        .0
        .iter()
        .filter(|r| r.kind == TapeIndexReplicaFileKind::Object)
        .map(|r| r.block_count)
        .sum();
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
    let start = records.len() as u64;
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
        highest_protected_ordinal: rows
            .0
            .iter()
            .filter_map(|row| row.protected_ordinal_end_exclusive)
            .max()
            .unwrap_or(0),
    };
    descriptor.terminal_layout = layout;
    let edition = plan_tape_index_edition(descriptor, &mut rows).unwrap();
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

/// Recover the independently validated rows for rebuilding terminal components.
fn prefix_rows(fixture: &TripleFixture) -> Rows {
    Rows(
        verify_terminal_replicas(
            &mut RecordingSource::new(fixture.records.clone()),
            &TAPE_UUID,
            BLOCK_SIZE,
            fixture.layout,
            ReplicaRowCheck::CollectOnly,
        )
        .unwrap()
        .rows[0]
            .clone(),
    )
}

/// Compare a fresh walk and an attempt, checking prefix entry before fallback can hide it.
fn compare_prefix(name: &str, fixture: TripleFixture, expected_reason: Option<&str>) {
    let mut attempt = RecordingSource::new(fixture.records.clone());
    let result = super::super::one_pass::verify_one_pass(
        &mut attempt,
        &TAPE_UUID,
        BLOCK_SIZE,
        ScanMode::Standard,
        64 * 1024 * 1024,
    );
    assert!(
        attempt.forward_calls > 0,
        "{name}: attempt must read the prefix"
    );
    match (result, expected_reason) {
        (Ok(_), None) => {}
        (Err(super::super::one_pass::AttemptError::Rejected(reason)), Some(expected)) => {
            assert_eq!(reason, expected, "{name}")
        }
        (Err(super::super::one_pass::AttemptError::Rejected(reason)), _) => {
            panic!("{name}: rejected {reason}")
        }
        _ => panic!("{name}: unexpected attempt result"),
    }
    let expected = super::super::verify_terminal_index_full(
        &mut RecordingSource::new(fixture.records.clone()),
        &TAPE_UUID,
        BLOCK_SIZE,
    );
    let mut reason = None;
    let actual = verify_terminal_index_full_with_route(
        &mut RecordingSource::new(fixture.records),
        &TAPE_UUID,
        BLOCK_SIZE,
        None,
        ScanMode::Standard,
        VerificationRoute::OnePass,
        |s| reason = s.fallback_reason,
    );
    assert_eq!(reason, expected_reason, "{name}");
    assert_eq!(format!("{actual:?}"), format!("{expected:?}"), "{name}");
}

#[test]
fn rebuilt_rows_require_independent_measurement() {
    for name in [
        "healthy",
        "row_longer",
        "row_shorter",
        "different_kind",
        "extra_filemark_inside_object",
        "extra_one_block_file_before_a",
    ] {
        let fixture = protected_fixture();
        let mut rows = prefix_rows(&fixture);
        let mut records =
            fixture.records[..fixture.layout.components[0].planned_start_lba as usize].to_vec();
        let reason = match name {
            "row_longer" | "row_shorter" => {
                // Keep the ParityMap address fixed while moving the Object/sidecar boundary.
                if name == "row_longer" {
                    rows.0[1].block_count += 1;
                    rows.0[2].block_count -= 1;
                } else {
                    rows.0[1].block_count -= 1;
                    rows.0[2].block_count += 1;
                }
                rows.0[2].protected_ordinal_end_exclusive = Some(rows.0[1].block_count);
                Some("prefix_map")
            }
            "different_kind" => {
                // A sidecar header occupies a row that claims a protected Object.
                records[2] = records[7].clone();
                Some("prefix_damaged")
            }
            "extra_filemark_inside_object" => {
                records.insert(4, Record::Filemark);
                rows.0[1].block_count = 2;
                let mut second = rows.0[1].clone();
                second.first_parity_data_ordinal = Some(2);
                rows.0.insert(2, second);
                // Both shorter files contain the same protected ordinal stream.
                None
            }
            "extra_one_block_file_before_a" => {
                records.push(Record::Block(vec![0x35; BLOCK_SIZE as usize]));
                records.push(Record::Filemark);
                // Finalized rows require the ParityMap last. Match every physical length,
                // but claim the old map as an Object and the added file as the final map.
                rows.0[3].kind = TapeIndexReplicaFileKind::Object;
                rows.0[3].first_parity_data_ordinal = Some(4);
                rows.0[2].protected_ordinal_end_exclusive = Some(7);
                let mut last = rows.0[3].clone();
                last.kind = TapeIndexReplicaFileKind::ParityMap;
                last.block_count = 1;
                last.first_parity_data_ordinal = None;
                rows.0.push(last);
                Some("prefix_map")
            }
            _ => None,
        };
        for (file, row) in rows.0.iter_mut().enumerate() {
            row.tape_file_number = file as u64;
        }
        let fixture = rebuild(records, rows);
        compare_prefix(name, fixture, reason);
    }
    println!("rebuilt fixtures: prefix=6 success=2 rejected=4 reasons={{prefix_map: 3, prefix_damaged: 1}} direct_walk=0");
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
pub(super) fn protected_fixture() -> TripleFixture {
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
    let mut attempt = RecordingSource::new(fixture.records.clone());
    assert!(matches!(
        super::super::one_pass::verify_one_pass(
            &mut attempt,
            &TAPE_UUID,
            BLOCK_SIZE,
            ScanMode::Standard,
            64 * 1024 * 1024,
        ),
        Err(super::super::one_pass::AttemptError::Rejected("no_parity"))
    ));
    assert_eq!(attempt.forward_calls, 0);
    assert_eq!(
        attempt.read_lbas.len(),
        5,
        "only the existing tail probes read blocks"
    );
    for ordinal in 1..=3 {
        let component = fixture.layout.replica(ordinal).unwrap();
        for lba in component.planned_start_lba + 1
            ..component.planned_start_lba + component.record_count - 1
        {
            assert!(
                !attempt.read_lbas.contains(&lba),
                "no replica payload before the walk"
            );
        }
    }
    println!(
        "no-parity selection: prefix=0 success=0 rejected=0 direct_walk=1 reads={} bytes={}",
        attempt.read_lbas.len(),
        attempt.read_lbas.len() * BLOCK_SIZE as usize
    );
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

/// Row coverage still streams Objects when the bootstrap explicitly disables parity.
#[test]
fn contradictory_no_parity_bootstrap_preserves_walk_outcome() {
    for fault in [false, true] {
        let mut fixture = protected_fixture();
        let Record::Block(block) = &mut fixture.records[0] else {
            unreachable!()
        };
        let mut payload = crate::bootstrap::parse_bootstrap_block(block).unwrap();
        payload.no_parity_flag = true;
        payload.scheme = None;
        crate::bootstrap::write_bootstrap_block(&payload, block).unwrap();
        let mut walk = RecordingSource::new(fixture.records.clone());
        let mut attempt = RecordingSource::new(fixture.records.clone());
        let mut routed = RecordingSource::new(fixture.records);
        if fault {
            walk.read_fault = Some((3, TestReadFault::Hardware));
            attempt.read_fault = walk.read_fault;
            routed.read_fault = walk.read_fault;
        }
        let expected = super::super::verify_terminal_index_full(&mut walk, &TAPE_UUID, BLOCK_SIZE);
        assert!(
            !walk.read_lbas.contains(&3),
            "walk skips the Object interior"
        );
        let result = super::super::one_pass::verify_one_pass(
            &mut attempt,
            &TAPE_UUID,
            BLOCK_SIZE,
            ScanMode::Standard,
            64 * 1024 * 1024,
        );
        assert!(attempt.forward_calls > 0);
        assert!(
            attempt.read_lbas.contains(&3),
            "row coverage reads the Object interior"
        );
        if fault {
            assert!(matches!(
                result,
                Err(super::super::one_pass::AttemptError::Rejected("tape_io"))
            ));
        } else {
            assert!(result.is_ok());
        }
        let mut reason = None;
        let actual = verify_terminal_index_full_with_route(
            &mut routed,
            &TAPE_UUID,
            BLOCK_SIZE,
            None,
            ScanMode::Standard,
            VerificationRoute::OnePass,
            |s| reason = s.fallback_reason,
        );
        assert_eq!(reason, fault.then_some("tape_io"));
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    }
    println!("contradictory bootstrap: prefix=2 success=1 rejected=1 reasons={{tape_io: 1}} direct_walk=0");
}

/// Both full-verification routes accept committed data beyond the last sidecar.
#[test]
fn full_verification_routes_accept_unprotected_committed_tail() {
    let protected = protected_fixture();
    let mut records = protected.records[..19].to_vec();
    records.extend([
        Record::Block(vec![0x73; BLOCK_SIZE as usize]),
        Record::Block(vec![0x74; BLOCK_SIZE as usize]),
        Record::Filemark,
    ]);
    let mut results = Vec::new();
    for route in [VerificationRoute::Walk, VerificationRoute::OnePass] {
        let result = verify_terminal_index_full_with_route(
            &mut RecordingSource::new(records.clone()),
            &TAPE_UUID,
            BLOCK_SIZE,
            None,
            ScanMode::Standard,
            route,
            |_| {},
        )
        .unwrap();
        assert!(result.protected().is_clean(), "{route:?}: {result:?}");
        assert_eq!(result.protected().sidecars.len(), 1);
        assert_eq!(result.protected().unprotected_ordinals, Some(4..6));
        assert!(!result.is_complete_tape());
        results.push(result);
    }
    assert_eq!(results[0], results[1]);
}
