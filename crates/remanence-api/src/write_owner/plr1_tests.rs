//! PLR-1b daemon probes reuse the typed crash fixture and production handlers.
use super::*;
use remanence_library::ScsiError;

fn with_cut<R>(
    source: &Path,
    cut: &Cut,
    missing: bool,
    authority: bool,
    test: impl FnOnce(
        &mut CatalogIndex,
        &mut WriteOwnerConfig,
        &mut DriveHandle,
        &Arc<Mutex<VirtualWorld>>,
        &Path,
    ) -> R,
) -> R {
    with_cut_io(
        source,
        cut,
        missing,
        authority,
        Arc::new(Mutex::new(LateIo::default())),
        test,
    )
}
fn with_cut_io<R>(
    source: &Path,
    cut: &Cut,
    missing: bool,
    authority: bool,
    trace: Arc<Mutex<LateIo>>,
    test: impl FnOnce(
        &mut CatalogIndex,
        &mut WriteOwnerConfig,
        &mut DriveHandle,
        &Arc<Mutex<VirtualWorld>>,
        &Path,
    ) -> R,
) -> R {
    let temp = tempfile::tempdir().unwrap();
    restore_disk(temp.path(), &cut.disk);
    cut.files.clone().materialize(source, temp.path()).unwrap();
    let paths = rebind_typed_crash_root(temp.path()).unwrap();
    let mut world = cut.world.clone();
    world.tapes.get_mut(BARCODE).unwrap().records = cut.records.as_ref().clone();
    let keep = world.tapes[BARCODE].records.len();
    assert!(keep >= world.tapes[BARCODE].durable_frontier);
    if missing {
        world.power_cut_during_write(BAY, keep).unwrap()
    } else {
        world.power_cut(BAY, keep).unwrap()
    }
    let world = Arc::new(Mutex::new(world));
    let runtime = remanence_state::library_runtime::LibraryRuntime::open(
        &paths,
        true,
        Arc::new(remanence_state::StateLockGuard::acquire(&paths.state_dir).unwrap()),
    )
    .unwrap();
    let catalog = remanence_state::projector::open(&paths.sqlite_path)
        .unwrap()
        .with_library_runtime(runtime);
    let model = world.lock().unwrap().library_snapshot();
    let mut library = model
        .open_with(&remanence_library::StaticAllowlist::new([LIBRARY]), {
            let world = world.clone();
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(LateTransport {
                    inner: ModelTransport::new(world.clone(), role),
                    world: world.clone(),
                    trace: trace.clone(),
                }))
            }
        })
        .unwrap();
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut catalog.write_handle(),
        [library.library()],
        &HashSet::from([LIBRARY.to_owned()]),
    )
    .unwrap();
    let mut cfg = crash_typed_config(&paths, &catalog, &library);
    if !authority {
        cfg.checkpoint_journal_dir = temp.path().join("absent-journal");
    }
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    assert!(drive.test_unit_ready().is_err());
    drive.test_unit_ready().unwrap();
    drive.locate(0).unwrap();
    let mut index = catalog.write_handle();
    super::super::readiness::session_open_short_probe_or_load(
        &mut index,
        &mut drive,
        SessionOpenReadinessContext {
            action: "PLR readiness",
            bay: BAY,
            library_serial: LIBRARY,
            barcode: Some(BARCODE),
            source_slot: None,
            drive_serial: Some(DRIVE),
            needs_drive_load: false,
            load_continues_move: false,
            lifecycle: cfg.lifecycle.as_ref(),
        },
    )
    .unwrap();
    test(&mut index, &mut cfg, &mut drive, &world, temp.path())
}
use pb::tape_inventory_stream_item::Item;
use prost::Message;
use remanence_parity::{
    BotObjectRecoveryAuthority, ControlledScanWalkOutcome, EndOfDataMark, EndOfDataRule, ScanMode,
    ScanWalkControl, VerificationRoute,
};

fn inventory(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
) -> (Result<(), Status>, Vec<pb::TapeInventoryStreamItem>) {
    let (tx, mut rx) = mpsc::channel(128);
    let result = handle_drive_tape_inventory(
        BAY,
        index,
        cfg,
        drive,
        TAPE,
        false,
        LIBRARY,
        Some(BARCODE),
        None,
        Some(DRIVE),
        &tx,
    );
    let mut items = Vec::new();
    while let Ok(item) = rx.try_recv() {
        let mut item = item.unwrap();
        assert_eq!(
            pb::TapeInventoryStreamItem::decode(item.encode_to_vec().as_slice()).unwrap(),
            item
        );
        if let Some(Item::BotRecoveryProgress(p)) = &mut item.item {
            p.elapsed_millis = 0;
        }
        items.push(item);
    }
    (result, items)
}
fn summary(items: &[pb::TapeInventoryStreamItem]) -> &pb::TapeInventory {
    items
        .iter()
        .find_map(|i| {
            if let Some(Item::Summary(s)) = &i.item {
                Some(s.as_ref())
            } else {
                None
            }
        })
        .unwrap()
}
/// Permit only this tape's disposable journal temporaries to disappear.
#[track_caller]
fn unchanged(
    root: &Path,
    before: BTreeMap<PathBuf, Vec<u8>>,
    index: &CatalogIndex,
    fences: String,
) -> (BTreeSet<PathBuf>, BTreeSet<PathBuf>) {
    let after = disk_image(root);
    let tape_id = uuid::Uuid::from_bytes(TAPE).simple().to_string();
    let eligible: BTreeSet<_> = before
        .keys()
        .filter(|path| {
            path.parent() == Some(Path::new("native/tapes"))
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        ["spool", "initial", "replacement", "repair-record"]
                            .iter()
                            .any(|purpose| name.starts_with(&format!(".{purpose}-{tape_id}-")))
                    })
        })
        .cloned()
        .collect();
    let deleted: BTreeSet<_> = eligible
        .iter()
        .filter(|path| !after.contains_key(*path))
        .cloned()
        .collect();
    println!(
        "cleanup window at {}: deleted={deleted:?} eligible={eligible:?}",
        std::panic::Location::caller()
    );
    let changed: Vec<_> = before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .filter(|path| !eligible.contains(*path) || after.contains_key(*path))
        .collect();
    assert!(
        changed.is_empty(),
        "already-ready handler adds no durable record; changed={changed:?}"
    );
    assert_eq!(
        format!("{:?}", index.list_active_tape_io_fences().unwrap()),
        fences,
        "handler preserves fences"
    );
    (deleted, eligible)
}
fn scan(drive: &mut DriveHandle) -> remanence_parity::ScanWalkResult {
    let result = remanence_parity::scan_reconstruct_filemark_map_with_end_of_data_rule(
        &mut DriveHandleRawSource::new(drive),
        &TAPE,
        BLOCK,
        ScanMode::Standard,
        EndOfDataRule::StopWhenConfirmed,
        |_| ScanWalkControl::Continue,
    )
    .unwrap();
    let ControlledScanWalkOutcome::Complete(result) = result else {
        panic!("complete scan")
    };
    result
}
#[tokio::test]
async fn missing_mark_inventory_handler_returns_ok() {
    let (_typed, root, cuts) = trace_pooled_typed_session().await;
    let cut = cuts
        .iter()
        .find(|c| c.boundary == "client commit 2")
        .unwrap();
    let end = cut.records.len() as u64;
    assert_eq!(end, 24);
    for authority in [false, true] {
        let runs = [false,true].map(|missing|with_cut(&root,cut,missing,authority,|index,cfg,drive,_,root|{
            let before=disk_image(root);let fences=format!("{:?}",index.list_active_tape_io_fences().unwrap());
            let(result,items)=inventory(index,cfg,drive);assert!(result.is_ok(),"inventory handler returns Ok: {result:?}");
            let cleanup = unchanged(root,before,index,fences);
            let s=summary(&items);assert_eq!(s.outcome,pb::TapeInventoryOutcome::BotStructuralRecovered as i32);
            assert_eq!(s.structural_entry_count,Some(6));assert_eq!(s.object_row_count,Some(3));
            assert_eq!(s.recovered_object_count,Some(if authority{3}else{0}));assert_eq!(s.unknown_object_count,Some(if authority{0}else{3}));
            assert_eq!(s.incomplete_object_count,Some(0));assert_eq!(s.damaged_region_count,Some(u64::from(missing)));
            let started=items.iter().find_map(|i|if let Some(Item::BotRecoveryStarted(s))=&i.item{Some(s)}else{None}).unwrap();
            assert_eq!(started.reason,if missing{3}else{1});
            if missing {assert!(s.detail.ends_with(&format!("; end_of_data_mark=not_found; unresolved_from partition=0 lba={end}; remaining tape unresolved")),"{}",s.detail)}
            else{assert!(!s.detail.contains("end_of_data_mark"));}
            println!("authority={authority} missing={missing} items={items:?}");(items, cleanup)
        }));
        for (_, (deleted, eligible)) in &runs {
            if authority {
                assert_eq!(
                    deleted, eligible,
                    "authority removes exactly the eligible temporaries"
                );
            } else {
                assert!(deleted.is_empty(), "absent journal removes no temporaries");
            }
        }
        assert_eq!(
            runs[0].1, runs[1].1,
            "marked and missing cleanup agree within one authority setting"
        );
        if authority {
            assert!(!runs[0].1 .1.is_empty(), "authority cleanup is not vacuous");
        }
        let streams = runs.map(|(items, _)| items);
        let mut marked = streams[0].clone();
        let mut missing = streams[1].clone();
        for item in &mut marked {
            if let Some(Item::BotRecoveryStarted(s)) = &mut item.item {
                s.reason = 3;
            }
        }
        for item in &mut missing {
            if let Some(Item::Summary(s)) = &mut item.item {
                s.damaged_region_count = Some(0);
                s.detail = s
                    .detail
                    .split("; end_of_data_mark=")
                    .next()
                    .unwrap()
                    .to_owned();
            }
        }
        assert_eq!(
            marked, missing,
            "complete Object rows, progress, counts and provenance unchanged"
        );
    }
}
#[tokio::test]
async fn authority_scope_refuses_shorter_map_after_all_rows_validate() {
    let (_typed, root, cuts) = trace_pooled_typed_session().await;
    let cut = cuts
        .iter()
        .find(|c| c.boundary == "client commit 2")
        .unwrap();
    with_cut(&root, cut, true, true, |index, cfg, drive, world, root| {
        // Deliberate media damage after the cut, not a power cut below the durable frontier.
        let trailing_start = {
            let w = world.lock().unwrap();
            let t = &w.tapes[BARCODE];
            assert_eq!(t.durable_frontier, 24);
            t.records[..t.records.len() - 1]
                .iter()
                .rposition(|r| matches!(r, Record::Filemark))
                .unwrap()
                + 1
        };
        assert_eq!(trailing_start, 19);
        world
            .lock()
            .unwrap()
            .tapes
            .get_mut(BARCODE)
            .unwrap()
            .records
            .truncate(trailing_start);
        let walked = scan(drive);
        assert_eq!(walked.end_of_data_mark, EndOfDataMark::NotFound);
        assert_eq!(walked.map.tape_file_count(), 5);
        let mut authority = remanence_state::authority::BotRecoveryAuthority::try_open(
            &cfg.checkpoint_journal_dir,
            &cfg.index_path,
            TAPE,
            BLOCK,
        )
        .unwrap()
        .unwrap();
        let mut rows = Vec::new();
        let scope = authority
            .visit_object_rows(&mut |row| {
                let entry = &walked.map.entries()[row.tape_file_number as usize];
                assert_eq!(entry.kind, remanence_parity::TapeFileKind::Object);
                assert_eq!(entry.block_count, row.stored_block_count);
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.tape_file_number).collect::<Vec<_>>(),
            [1, 2, 4]
        );
        assert_eq!(scope.object_row_count, 3);
        assert_eq!(scope.covered_prefix_tape_file_count, 6);
        println!(
            "validated authority rows={rows:?}; scope={scope:?}; measured files={}",
            walked.map.tape_file_count()
        );
        drop(authority);
        let before = disk_image(root);
        let fences = format!("{:?}", index.list_active_tape_io_fences().unwrap());
        let (result, items) = inventory(index, cfg, drive);
        let error = result.expect_err("authority scope refused");
        assert_eq!(error.code(), tonic::Code::DataLoss);
        assert!(
            error
                .message()
                .contains("authority covers 6 tape files but the complete BOT map contains only 5"),
            "{error}"
        );
        assert!(!items
            .iter()
            .any(|i| matches!(i.item, Some(Item::BotObject(_) | Item::Summary(_)))));
        let (deleted, _) = unchanged(root, before, index, fences);
        assert!(
            deleted.is_empty(),
            "shorter-map setup already cleaned temporaries"
        );
    });
}
#[tokio::test]
async fn daemon_initial_and_post_damage_verification_refuse() {
    let (_typed, root, cuts) = trace_pooled_typed_session().await;
    let cut = cuts
        .iter()
        .find(|c| c.boundary == "client commit 2")
        .unwrap();
    for route in [VerificationRoute::Walk, VerificationRoute::OnePass] {
        for post in [false, true] {
            with_cut(&root, cut, true, true, |index, cfg, drive, world, root| {
                cfg.verification_route = route;
                if post {
                    world
                        .lock()
                        .unwrap()
                        .drive_states
                        .get_mut(&BAY)
                        .unwrap()
                        .normal_eod_spaces = if route == VerificationRoute::Walk {
                        1
                    } else {
                        2
                    };
                }
                let before = disk_image(root);
                let fences = format!("{:?}", index.list_active_tape_io_fences().unwrap());
                world.lock().unwrap().command_log.clear();
                let error = handle_drive_verify_tape_index(
                    BAY,
                    index,
                    cfg,
                    drive,
                    TAPE,
                    false,
                    LIBRARY,
                    Some(BARCODE),
                    None,
                    Some(DRIVE),
                    None,
                )
                .expect_err("no successful verdict or EOD_MEASURED");
                assert_eq!(error.code(), tonic::Code::Unavailable);
                assert!(error.message().contains("SPACE(EOD)"), "{error}");
                if route == VerificationRoute::OnePass && !post {
                    assert!(error.message().contains("one-pass attempt:"), "{error}");
                }
                assert!(
                    error
                        .message()
                        .contains("drive reports no end-of-data mark"),
                    "diagnostic names missing mark: {error}"
                );
                if post {
                    assert!(
                        error.message().contains(
                            "sense=[70, 00, 03, 00, 00, 00, 00, 18, 00, 00, 00, 00, 14, 00"
                        ),
                        "post-damage refusal preserves drive sense: {error}"
                    );
                }
                let ops = world
                    .lock()
                    .unwrap()
                    .command_log
                    .iter()
                    .map(|c| c.opcode)
                    .collect::<Vec<_>>();
                let last_space = ops.iter().rposition(|&op| op == 0x11).unwrap();
                assert!(
                    !ops[last_space + 1..]
                        .iter()
                        .any(|op| matches!(op, 0x92 | 0x2b | 0x34 | 0x08)),
                    "no further positioning after refusal: {ops:?}"
                );
                println!("route={route:?} post={post} error={error} ops={ops:?}");
                let (deleted, eligible) = unchanged(root, before, index, fences);
                assert_eq!(
                    deleted, eligible,
                    "verification authority cleans exactly its eligible temporaries"
                );
            });
        }
    }
}
#[test]
fn unscanned_missing_mark_projection_has_no_measured_counts_or_position() {
    let recovery = remanence_parity::BotStructuralRecoveryRequired {
        reason: BotStructuralRecoveryReason::EndOfDataNotFound,
        replicas: std::array::from_fn(|_| {
            remanence_parity::TerminalReplicaEvidence::Invalid(
                remanence_parity::TerminalReplicaFailure {
                    kind: remanence_parity::TerminalReplicaFailureKind::NotInspected,
                    detail: "not inspected".into(),
                },
            )
        }),
    };
    let s = terminal_inventory_to_proto(
        TAPE,
        TerminalInventoryOutcome::BotStructuralRecoveryRequired(Box::new(recovery)),
    );
    assert_eq!(
        s.detail,
        "end_of_data_not_found; structural recovery from BOT is required"
    );
    assert_eq!(s.structural_entry_count, None);
    assert_eq!(s.object_row_count, None);
    assert_eq!(s.damaged_region_count, None);
    assert_eq!(s.canonical_map_digest, None);
}

/// Independently encode a terminal edition over the measured committed prefix.
/// The late-failure tests must reach a prefix walk after a real layout selection.
struct TerminalRows(Vec<TapeIndexReplicaMapEntry>);
impl TapeIndexReplicaRecordSource for TerminalRows {
    fn visit_structural_entries(
        &mut self,
        visit: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        for row in &self.0 {
            visit(row)?;
        }
        Ok(())
    }
    fn visit_object_rows(
        &mut self,
        visit: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        for row in self
            .0
            .iter()
            .filter(|r| r.kind == TapeIndexReplicaFileKind::Object)
        {
            visit(&TapeIndexReplicaObjectRow {
                tape_file_number: row.tape_file_number,
                stored_block_count: row.block_count,
                object_id: format!("fixture-object-{}", row.tape_file_number).into_bytes(),
                representation: remanence_parity::ObjectRecoveryRepresentation::Plaintext {
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
fn append_terminal(world: &Arc<Mutex<VirtualWorld>>, map: &remanence_parity::FilemarkMap) {
    use remanence_parity::TapeFileKind as K;
    let mut entries = map.entries().to_vec();
    entries.push(TapeFileMapEntry::parity_map(map.tape_file_count(), 3));
    let map = remanence_parity::FilemarkMap::new(entries).unwrap();
    let mut directory = Vec::new();
    {
        let mut w = world.lock().unwrap();
        let tape = w.tapes.get_mut(BARCODE).unwrap();
        for row in map.entries().iter().filter(|r| r.kind == K::ParitySidecar) {
            let start = map
                .physical_position(remanence_parity::TapeFilePosition {
                    tape_file_number: row.tape_file_number,
                    block_within_file: 0,
                })
                .unwrap()
                .lba as usize;
            let Record::Block(block) = &tape.records[start] else {
                panic!("sidecar header")
            };
            let header =
                remanence_parity::sidecar::parse_sidecar_header_block(block, &TAPE).unwrap();
            directory.push(remanence_parity::SidecarEpochDirectoryEntry {
                tape_file_number: row.tape_file_number,
                epoch_id: header.epoch_id,
                protected_ordinal_start: header.protected_ordinal_start,
                protected_ordinal_end_exclusive: header.protected_ordinal_end_exclusive,
                sidecar_total_block_count: header.sidecar_total_block_count,
                sidecar_header_block_count: header.shard_index_block_count,
                parity_shard_block_count: header.parity_block_count,
                canonical_metadata_hash: header.canonical_metadata_hash,
                flags: 0,
            });
        }
        let payload = remanence_parity::ParityMapPayload {
            tape_uuid: TAPE,
            sequence: 1,
            canonical_map_digest: map.canonical_digest().unwrap(),
            writer_version: None,
            write_timestamp: None,
            directory: remanence_parity::SidecarEpochDirectory {
                directory_scope_tape_file_count: map.tape_file_count(),
                directory_scope_total_data_ordinals: map.total_data_ordinals(),
                directory_scope_highest_protected_ordinal: directory
                    .last()
                    .unwrap()
                    .protected_ordinal_end_exclusive,
                is_final_directory: true,
                entries: directory,
            },
        };
        let encoded = remanence_parity::encode_parity_map_tape_file(&payload, BLOCK).unwrap();
        assert_eq!(encoded.blocks.len(), 3);
        tape.records
            .extend(encoded.blocks.into_iter().map(Record::Block));
        tape.records.push(Record::Filemark);
    }

    let mut rows = TerminalRows(
        map.entries()
            .iter()
            .map(|r| TapeIndexReplicaMapEntry {
                tape_file_number: r.tape_file_number,
                kind: match r.kind {
                    K::Bootstrap => TapeIndexReplicaFileKind::Bootstrap,
                    K::Object => TapeIndexReplicaFileKind::Object,
                    K::ParitySidecar => TapeIndexReplicaFileKind::ParitySidecar,
                    K::ParityMap => TapeIndexReplicaFileKind::ParityMap,
                    K::TapeIndexReplica => TapeIndexReplicaFileKind::TapeIndexReplica,
                    K::IndexSeparationExtent => TapeIndexReplicaFileKind::IndexSeparationExtent,
                },
                block_count: r.block_count,
                first_parity_data_ordinal: r.first_parity_data_ordinal,
                protected_ordinal_start: r.protected_ordinal_start,
                protected_ordinal_end_exclusive: r.protected_ordinal_end_exclusive,
                epoch_id: r.epoch_id,
            })
            .collect(),
    );
    let counts = TapeIndexReplicaCounts {
        structural_entry_count: map.tape_file_count(),
        object_row_count: 3,
    };
    let replica_records = checked_tape_index_replica_layout(BLOCK, counts)
        .unwrap()
        .replica_record_count;
    let mut w = world.lock().unwrap();
    let tape = w.tapes.get_mut(BARCODE).unwrap();
    let layout = TerminalTailLayout::new(
        0,
        BLOCK,
        map.tape_file_count(),
        tape.records.len() as u64,
        replica_records,
        3,
    )
    .unwrap();
    let edition = plan_tape_index_edition(
        TapeIndexEditionDescriptor {
            tape_uuid: TAPE,
            edition_id: [0x63; 16],
            edition_sequence: 1,
            scope: TapeIndexReplicaScope {
                covered_prefix_tape_file_count: map.tape_file_count(),
                total_data_ordinals: map.total_data_ordinals(),
                highest_protected_ordinal: map
                    .entries()
                    .iter()
                    .filter_map(|r| r.protected_ordinal_end_exclusive)
                    .max()
                    .unwrap(),
            },
            counts,
            block_size: BLOCK,
            compression_enabled: false,
            writer_version: "plr-test".into(),
            write_timestamp: "2026-11-18T00:00:00Z".into(),
            terminal_layout: layout,
        },
        &mut rows,
    )
    .unwrap();
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
                tape.records.push(Record::Block(block.to_vec()));
                Ok(())
            },
        )
        .unwrap();
        tape.records.push(Record::Filemark);
        if ordinal < 3 {
            let plan = plan_index_separation(IndexSeparationDescriptor {
                tape_uuid: TAPE,
                edition_id: [0x63; 16],
                gap_ordinal: ordinal,
                block_size: BLOCK,
                nominal_extent_bytes: 3 * u64::from(BLOCK),
                total_records: 3,
                compression_enabled: false,
                terminal_layout: layout,
            })
            .unwrap();
            write_index_separation(
                &plan,
                IndexSeparationObservation {
                    tape_file_number: plan.component.planned_tape_file_number,
                    start_lba: plan.component.planned_start_lba,
                    record_count: plan.component.record_count,
                },
                |block| {
                    tape.records.push(Record::Block(block.to_vec()));
                    Ok(())
                },
            )
            .unwrap();
            tape.records.push(Record::Filemark);
        }
    }
}
#[derive(Default)]
struct LateIo {
    enabled: bool,
    eod: bool,
    armed: bool,
    commands: Vec<Vec<u8>>,
}
struct LateTransport {
    inner: ModelTransport,
    world: Arc<Mutex<VirtualWorld>>,
    trace: Arc<Mutex<LateIo>>,
}
fn missing(error: &ScsiError) -> bool {
    matches!(error,ScsiError::CheckCondition{sense,..} if sense.len()>=14 && sense[0]&0x7f==0x70 && sense[2]&0xf==3 && sense[12]==0x14 && sense[13]==0)
}
impl SgTransport for LateTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, ScsiError> {
        let mut trace = self.trace.lock().unwrap();
        trace.commands.push(cdb.to_vec());
        if trace.armed && cdb[0] == 0x08 {
            self.world
                .lock()
                .unwrap()
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .read_sense_at = Some((3, (3, 0x14, 0)));
        }
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let mut t = self.trace.lock().unwrap();
        t.commands.push(cdb.to_vec());
        if t.enabled && t.eod && !t.armed && cdb[0] == 0x92 && cdb.get(4..12) == Some(&[0; 8]) {
            t.armed = true;
            let mut w = self.world.lock().unwrap();
            let s = w.drive_states.get_mut(&BAY).unwrap();
            s.read_sense_at = Some((3, (3, 0x14, 0)));
            s.space_sense_at = Some((3, (3, 0x14, 0)));
        }
        let result = self.inner.execute_none(cdb);
        if t.enabled && cdb[0] == 0x11 && cdb[1] & 7 == 3 && result.is_ok() {
            t.eod = true;
        }
        if t.armed && cdb[0] == 0x11 && result.as_ref().is_err_and(missing) {
            self.world
                .lock()
                .unwrap()
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .position_unknown = true;
        }
        result
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, ScsiError> {
        self.trace.lock().unwrap().commands.push(cdb.to_vec());
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class)
    }
}
#[tokio::test]
async fn daemon_walk_and_one_pass_late_prefix_keep_missing_signature() {
    let (_typed, root, cuts) = trace_pooled_typed_session().await;
    let cut = cuts.last().unwrap();
    for route in [VerificationRoute::Walk, VerificationRoute::OnePass] {
        let trace = Arc::new(Mutex::new(LateIo::default()));
        with_cut_io(
            &root,
            cut,
            false,
            true,
            trace.clone(),
            |index, cfg, drive, world, root| {
                let walked = scan(drive);
                append_terminal(world, &walked.map);
                let selection = remanence_parity::read_terminal_index_inventory_streamed(
                    &mut DriveHandleRawSource::new(drive),
                    &TAPE,
                    BLOCK,
                    |_| Ok(()),
                )
                .unwrap();
                assert!(
                    matches!(selection, TerminalInventoryOutcome::Inventory(_)),
                    "layout fixture: {selection:?}"
                );
                cfg.verification_route = route;
                {
                    let mut t = trace.lock().unwrap();
                    t.enabled = true;
                    t.eod = false;
                    t.commands.clear();
                }
                let before = disk_image(root);
                let fences = format!("{:?}", index.list_active_tape_io_fences().unwrap());
                let error = handle_drive_verify_tape_index(
                    BAY,
                    index,
                    cfg,
                    drive,
                    TAPE,
                    false,
                    LIBRARY,
                    Some(BARCODE),
                    None,
                    Some(DRIVE),
                    None,
                )
                .expect_err("late prefix cannot emit a verdict/EOD_MEASURED");
                let t = trace.lock().unwrap();
                assert!(t.eod && t.armed, "plan selected before prefix failure");
                assert_eq!(error.code(), tonic::Code::Unavailable);
                if route == VerificationRoute::Walk {
                    assert!(
                        error.message().contains("prefix"),
                        "direct prefix failure: {error}"
                    );
                } else {
                    assert!(
                        error.message().contains("one-pass attempt:"),
                        "late prefix survived unknown-position fallback: {error}"
                    );
                }
                assert!(
                    error
                        .message()
                        .contains("drive reports no end-of-data mark"),
                    "late diagnostic names missing mark: {error}"
                );
                println!(
                    "late route={route:?} error={error}; commands={:?}",
                    t.commands
                );
                drop(t);
                let (deleted, eligible) = unchanged(root, before, index, fences);
                assert_eq!(
                    deleted, eligible,
                    "verification authority cleans exactly its eligible temporaries"
                );
            },
        );
    }
}
