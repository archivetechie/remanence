//! Checked rebuild admission and durable fencing at the actual actor opener.
use super::*;

struct CorruptRebuildRead {
    inner: ModelTransport,
    target: u64,
    at_target: bool,
    remaining: usize,
    corruptions: Arc<AtomicU64>,
}
impl SgTransport for CorruptRebuildRead {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        let outcome = self.inner.execute_in(cdb, buf)?;
        if cdb.first() == Some(&0x08) && self.at_target && self.remaining > 0 {
            assert!(outcome.bytes_transferred > 0);
            buf[0] ^= 1;
            self.remaining -= 1;
            self.corruptions.fetch_add(1, Ordering::Relaxed);
            self.at_target = false;
        }
        Ok(outcome)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        self.inner.execute_none(cdb)?;
        match cdb[0] {
            0x92 => {
                self.at_target = u64::from_be_bytes(cdb[4..12].try_into().unwrap()) == self.target
            }
            0x2b => {
                self.at_target =
                    u32::from_be_bytes(cdb[3..7].try_into().unwrap()) as u64 == self.target
            }
            _ => {}
        }
        Ok(())
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

#[test]
fn checked_closeout_actor_retry_fence_legacy_and_reservation() {
    check_closeout_route(false);
}

#[test]
fn checked_closeout_terminal_retry_fence_legacy_and_reservation() {
    check_closeout_route(true);
}

fn check_closeout_route(terminal: bool) {
    for (corrupt, refuse_memory, legacy) in [
        (0, false, false),
        (1, false, false),
        (2, false, false),
        (0, true, false),
        (2, false, true),
    ] {
        let vector = crate::tape_image_vectors::generate("unfinalized-open").unwrap();
        let input = crate::resume_vectors::recorded_input("resume-open");
        let root = tempfile::tempdir().unwrap();
        let (_drive, world, _) = crate::resume_vectors::model(&vector, &input);
        let library = open_model_library(Arc::clone(&world));
        let mut cfg = test_write_owner_config(
            root.path().join("index.sqlite"),
            root.path().join("audit"),
            &library,
            library_snapshot_cell(library.library().clone()),
        );
        let mut index = CatalogIndex::open(&cfg.index_path).unwrap();
        let selected = SelectedTape {
            standalone_endpoint: None,
            pool_id: "closeout".into(),
            tape_uuid: vector.written.inputs.tape_uuid,
            block_size: crate::tape_image_vectors::BLOCK,
            parity_config: ParityConfig::Scheme(vector.written.inputs.scheme.clone()),
        };
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: selected.tape_uuid,
                voltag: "RESUME001".into(),
                block_size: selected.block_size,
                parity: selected.parity_config.clone(),
                force: false,
            })
            .unwrap();
        let mut journal =
            crate::resume_vectors::adapt(&input, &vector, &root.path().join("rows")).unwrap();
        let snapshot = journal.committed_snapshot_bounded().unwrap();
        let summary = checked_bounded_resume_summary(&snapshot).unwrap();
        let expected = check_value(
            input["data_shard_crc64"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| u64::from_str_radix(s.as_str().unwrap(), 16).unwrap()),
        );
        let original = remanence_state::authority::AuthorityCommit {
            ordinal: 1,
            object_count: summary.committed_object_count,
            protected: summary.highest_protected_ordinal,
            total: summary.total_committed_ordinals,
            cursor: CommitCursor {
                partition: summary.append_position.partition,
                lba: summary.append_position.lba,
            },
            tape_uuid: selected.tape_uuid,
            operation_id: [8; 16],
            next_file: summary.committed_tape_file_count,
            block_size: selected.block_size,
            payloads: Vec::new(),
            scheme: Some(summary.scheme().clone()),
            object_files: Vec::new(),
            barrier: None,
            finalization: None,
            sealed: false,
            seal_due: None,
            check: Some((
                expected,
                if legacy {
                    CheckProvenance::UnverifiedRebuild
                } else {
                    CheckProvenance::OriginalWrite
                },
            )),
        };
        let extent = &summary.open_epoch_object_extents[0];
        let target = extent.physical_start_lba + summary.highest_protected_ordinal
            - extent.first_parity_data_ordinal;
        let corruptions = Arc::new(AtomicU64::new(0));
        let transport = CorruptRebuildRead {
            inner: ModelTransport::new(
                Arc::clone(&world),
                remanence_chaos::model::DeviceRole::Drive { bay: 0x100 },
            ),
            target,
            at_target: false,
            remaining: corrupt,
            corruptions: Arc::clone(&corruptions),
        };
        let mut drive = DriveHandle::open_standalone_with_transport(
            Path::new("/virtual/closeout-drive"),
            Box::new(transport),
            &remanence_library::StaticAllowlist::new(["/virtual/closeout-drive"]),
        )
        .unwrap();
        let _admission = if refuse_memory {
            cfg.io_memory = crate::io_memory::IoMemoryReservation::new(1).unwrap();
            None
        } else {
            crate::resume_vectors::discard_synthetic_suffix(&world, summary.append_position.lba);
            Some(
                crate::admit_checkpointed_tape(
                    &mut drive,
                    selected.tape_uuid,
                    selected.block_size,
                    summary.append_position.partition,
                    summary.append_position.lba,
                )
                .unwrap(),
            )
        };
        world.lock().unwrap().command_log.clear();
        let before = world.lock().unwrap().tapes["RESUME001"].records.clone();
        let authority = ParityActorAuthority {
            scheme: summary.scheme().clone(),
            snapshot,
            summary,
        };
        let result = if terminal {
            let mut permit = None;
            super::super::terminal_finalize::terminal_epoch_state(
                &mut index,
                &cfg,
                &mut drive,
                &mut journal,
                &authority.snapshot,
                &selected,
                None,
                &original,
                &mut permit,
            )
            .map(|state| ParityActorSession {
                scheme: authority.scheme,
                sink_state: state,
                retention: permit.take(),
                close_at_barrier: false,
            })
        } else {
            open_parity_actor_session_with_rows(
                &mut index,
                &mut drive,
                &cfg,
                &selected,
                Some(&original),
                authority,
                &mut journal,
                None,
            )
        };
        let successful = !refuse_memory && (corrupt < 2 || legacy);
        if successful {
            let session = result.unwrap();
            let check = session.sink_state.as_ref().unwrap().commit_check();
            if legacy {
                assert_eq!(check.1, CheckProvenance::UnverifiedRebuild);
            } else {
                assert_eq!(check, (expected, CheckProvenance::OriginalWrite));
            }
            assert!(cfg.io_memory.granted() > 0);
            drop(session);
        } else {
            let status = result.err().expect("rebuild must refuse");
            assert_eq!(
                status.code(),
                if refuse_memory {
                    tonic::Code::ResourceExhausted
                } else {
                    tonic::Code::FailedPrecondition
                }
            );
            if !refuse_memory {
                assert!(status.message().contains("closeout check mismatch"));
            }
        }
        assert_eq!(cfg.io_memory.granted(), 0);
        assert_eq!(
            corruptions.load(Ordering::Relaxed),
            if legacy { 1 } else { corrupt as u64 }
        );
        let world = world.lock().unwrap();
        assert!(
            world
                .command_log
                .iter()
                .all(|c| !matches!(c.opcode, 0x0a | 0x10)),
            "rebuild dispatched a media write"
        );
        if refuse_memory {
            assert!(
                world.command_log.is_empty(),
                "allocation refusal dispatched a drive command"
            );
        }
        assert_eq!(world.tapes["RESUME001"].records, before);
        drop(world);
        drop(index);
        let reopened = CatalogIndex::open(&cfg.index_path).unwrap();
        let fences = reopened
            .tape_io_admission_conflicts(&selected.tape_uuid, Some("RESUME001"))
            .unwrap();
        if corrupt == 2 && !legacy {
            assert_eq!(fences.len(), 1);
            assert_eq!(fences[0].reason, "CloseoutCheckMismatch");
            assert!(fences[0]
                .evidence_json
                .as_ref()
                .unwrap()
                .contains("\"attempts\":2"));
        } else {
            assert!(fences.is_empty());
        }
    }
}
