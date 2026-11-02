//! Model-actor coverage for detached finalization admission and readiness.
use super::*;
use remanence_library::BlockSink;
use remanence_parity::{
    ParityError, PhysicalPositionHint, RawTapeSink, RawWriteOutcome, TapeReportPublisher,
};

/// Count physical records while the real parity sink constructs a valid plan.
#[derive(Default)]
struct CounterSink(u64);
impl RawTapeSink for CounterSink {
    fn write_fixed_block(&mut self, bytes: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.0 += 1;
        Ok(RawWriteOutcome::WroteBlock {
            bytes_written: bytes.len() as u32,
            position_after: PhysicalPositionHint::new(self.0),
            early_warning: false,
            end_of_medium: false,
        })
    }
    fn write_filemarks(&mut self, count: u32, _: bool) -> Result<RawWriteOutcome, ParityError> {
        self.0 += u64::from(count);
        Ok(RawWriteOutcome::WroteFilemark {
            position_after: PhysicalPositionHint::new(self.0),
            early_warning: false,
            end_of_medium: false,
        })
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(PhysicalPositionHint::new(self.0))
    }
}

/// Keep sink bundles in memory so checkpoint annotations precede durable journal writes.
struct Bundles(Vec<FileRowsReport>);
impl TapeReportPublisher for Bundles {
    remanence_parity::fixture_publication_methods!();
    fn load_committed(
        &self,
    ) -> Result<remanence_parity::FileRowsState, remanence_parity::JournalError> {
        panic!("fixture only records new writes")
    }
    fn tape_uuid(&self) -> TapeUuid {
        [0x91; 16]
    }
    fn publish_fixture(
        &mut self,
        report: &remanence_parity::authority::ShadowPublication,
    ) -> Result<(), remanence_parity::JournalError> {
        for bundle in &report.bundles {
            self.0.push(bundle.clone());
        }
        Ok(())
    }
}

fn planning_harness(
    in_slot: bool,
) -> (
    DismountHarness,
    ManualFinalizeTapeAdmission,
    remanence_parity::TerminalPrefixPlan,
) {
    let mut h = dismount_harness(3600);
    let uuid = [0x91; 16];
    let block_size = 262144;
    let scheme = remanence_parity::ParityScheme {
        id: remanence_parity::SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 128,
        parity_blocks_per_stripe: 4,
        stripes_per_neighborhood: 1,
    };
    let mut raw = CounterSink::default();
    let mut bundles = Bundles(Vec::new());
    let identity = remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION"));
    let mut sink = remanence_parity::ParitySink::new_with_journal(
        &mut raw,
        &mut bundles,
        scheme.clone(),
        uuid,
        block_size,
        identity.clone(),
    )
    .unwrap();
    sink.write_bootstrap().unwrap();
    let runtime = sink.terminal_triple_capacity_runtime_state().unwrap();
    let input = remanence_parity::TerminalTripleCloseInput {
        projected_object_present: true,
        projected_object_blocks: 3,
        block_size_bytes: block_size,
        current_epoch_fill_blocks: 0,
        data_shards_per_epoch: 128,
        parity_shards_per_epoch: 4,
        pending_completed_sidecars: 0,
        sidecar_entries_before_object: 0,
        structural_entries_before_object: runtime.structural_entries_before_object,
        object_rows_before_object: 0,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        parity_map_filemark_blocks: 1,
        replica_filemark_blocks: 1,
        gap_filemark_blocks: 1,
        gap_nominal_bytes: remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        safety_margin_blocks: 3,
        remaining_tape_blocks: 1_000_000,
        capacity_basis_blocks: 1_000_000 + runtime.used_tape_blocks,
        low_watermark_blocks: 0,
        high_watermark_blocks: 1,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: u64::MAX,
    };
    sink.begin_object_with_terminal_triple_reservation(input.reserve_object().unwrap())
        .unwrap();
    for _ in 0..3 {
        sink.write_block(&vec![0x5a; block_size as usize]).unwrap();
    }
    let summary = sink.finish_object().unwrap();
    let plan = sink
        .plan_terminal_index_close(identity.capture().unwrap())
        .unwrap();
    let check = sink.into_session_state().unwrap().commit_check();
    let mut checkpoint = manual_finalize_checkpoint(uuid, block_size);
    checkpoint.scheme = Some(scheme.clone());
    checkpoint.eod_lba = raw.0;
    checkpoint.objects[0].copy.first_body_lba = summary.physical_start_lba.unwrap();
    checkpoint.objects[0].copy.first_parity_data_ordinal = Some(0);
    checkpoint.objects[0].copy.protected_until_ordinal = Some(
        summary
            .committed_bundle()
            .unwrap()
            .highest_protected_ordinal,
    );
    if let ObjectRecoveryFixtureRepresentation::Plaintext {
        manifest_first_chunk_lba,
        ..
    } = &mut checkpoint.objects[0].object_recovery_row.representation
    {
        *manifest_first_chunk_lba = summary.physical_start_lba.unwrap();
    }
    let mut object = summary.committed_bundle().unwrap();
    object.entries[0].object_id = Some(checkpoint.objects[0].object.object_id.clone());
    object.entries[0].object_recovery_row =
        Some(checkpoint.objects[0].object_recovery_row.to_parity_row());
    checkpoint.object_tape_file_bundles = vec![object.clone()];
    let mut report: remanence_state::authority::AuthorityCommit = (&checkpoint).into();
    report.check = Some(check);
    remanence_state::authority::fixtures::AuthorityBuilder::new(
        h.state.checkpoint_journal_dir.as_ref(),
        uuid,
    )
    .commit(report)
    .build()
    .unwrap();
    let mut index = h.state.index_write().unwrap();
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: uuid,
            voltag: "OLD001L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .unwrap();
    use remanence_state::authority::TapeAuthorityView;
    let authority = remanence_state::TapeAuthority::open(
        h.state.checkpoint_journal_dir.as_ref(),
        uuid,
        index.path(),
    )
    .unwrap();
    index
        .project_tape_log(&authority.freeze().unwrap())
        .unwrap();
    drop(index);
    h.seated.tape_uuid = Some(uuid);
    if in_slot {
        let mut snapshot = h.state.library_snapshot.as_ref().unwrap().write().unwrap();
        let mut report = snapshot.report.clone();
        report.libraries[0] = test_library(
            vec![drive_bay(0x0101, false, None)],
            vec![slot(0x0401, "OLD001L9")],
        );
        *snapshot = std::sync::Arc::new(crate::live_status::LibrarySnapshot {
            report,
            captured_at: snapshot.captured_at,
        });
    }
    let admission = ManualFinalizeTapeAdmission {
        candidate_operation_id: Uuid::new_v4(),
        actor: remanence_state::AuditActor::System,
        actor_fingerprint: "planning-test".into(),
        idempotency_key: Uuid::new_v4(),
        request_fingerprint: [0x91; 32],
        tape_uuid: uuid,
        expected_pool_id: None,
        reason: "close tape".into(),
    };
    (h, admission, plan)
}

/// A short epoch at rest is closed out separately, before any finalization
/// decision, changer move, or drive command can be accepted.
#[tokio::test]
async fn manual_finalization_requires_closeout_before_media_or_acceptance() {
    for in_slot in [false, true] {
        let (mut h, admission, _) = planning_harness(in_slot);
        let result = manual_finalize_tape(&h.state, admission.clone())
            .await
            .unwrap_err();
        assert_eq!(result.code(), tonic::Code::FailedPrecondition);
        assert!(
            result.message().starts_with("CloseoutRequired:"),
            "{result}"
        );
        assert!(result.message().contains("rem tape closeout"));
        assert!(h.drive_rx.try_recv().is_err());
        assert!(h.changer_rx.try_recv().is_err());
        assert!(h
            .state
            .operations
            .latest(&admission.candidate_operation_id)
            .is_none());
    }
}
