//! Regression coverage for typed automatic terminal-finalization plans.
use super::*;
use remanence_parity::{CommitCursor, ParityConfig};
use remanence_state::{ProvisionTapeInput, TapeAuthority, TerminalIndexSource};

#[test]
/// Durable typed reasons survive automatic planning and the native plan validator.
fn automatic_terminal_plan_preserves_non_operator_seal_reasons() {
    use remanence_state::TerminalFinalizationTrigger as Trigger;

    for trigger in [
        Trigger::ReachedLowWatermark,
        Trigger::HardwareEarlyWarning,
        Trigger::PoolCloseOut,
        Trigger::NoPendingObjectFits,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut index = CatalogIndex::open(temp.path().join("state.sqlite")).unwrap();
        let selected = SelectedTape {
            standalone_endpoint: None,
            pool_id: "automatic-reasons".into(),
            tape_uuid: [0xD5; 16],
            block_size: 256 * 1024,
            parity_config: ParityConfig::None,
        };
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: selected.tape_uuid,
                voltag: "REASONL9".into(),
                block_size: selected.block_size,
                parity: ParityConfig::None,
                force: false,
            })
            .unwrap();
        let pool = TapePoolConfig {
            id: selected.pool_id.clone(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            scheme: None,
            block_size_bytes: u64::from(selected.block_size),
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            min_object_size_bytes: 0,
        };
        std::fs::create_dir_all(temp.path().join("tapes")).unwrap();
        let authority =
            TapeAuthority::open(temp.path().join("tapes"), selected.tape_uuid, index.path())
                .unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        writer
            .publish_initialized(
                selected.block_size,
                None,
                CommitCursor {
                    partition: 0,
                    lba: 2,
                },
            )
            .unwrap();
        let mut report = writer.latest_commit().unwrap().unwrap();
        report.ordinal += 1;
        report.operation_id = [0x42; 16];
        report.seal_due = Some(trigger);
        writer.publish_report(&report).unwrap();
        // Reopen so the trigger comes from replayed native authority.
        drop(writer);
        let mut writer = authority.acquire_exclusive().unwrap();
        let previous = writer.latest_commit().unwrap().unwrap();
        let spec = TerminalFinalizeSpec::automatic(&selected, &pool, previous.seal_due.unwrap());
        let mut source = TerminalIndexSource::new_replay_backed_no_parity(&writer).unwrap();
        let (plan, _) = build_new_terminal_plan(
            &index,
            &selected,
            &spec,
            &previous,
            &mut source,
            TerminalPlanPosition {
                first_tape_file_number: previous.next_file,
                first_start_lba: previous.cursor.lba,
                terminal_prefix: None,
            },
        )
        .unwrap();
        drop(source);
        assert_eq!(plan.trigger, trigger);
        assert!(plan.manual.is_none());
        writer
            .begin_finalization(&plan)
            .expect("automatic plan passes validate_plan");
        assert_eq!(
            writer.pending_finalization().unwrap().unwrap().trigger,
            trigger
        );
    }
}
