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
        let mut index = remanence_state::projector::open(temp.path().join("state.sqlite"))
            .map(|catalog| catalog.write_handle())
            .unwrap();
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

/// The production terminal publisher marks the runtime carried by a StateHandle's
/// client, even when its newly written native intent has not refreshed the view.
#[test]
fn terminal_intent_marks_runtime_tape() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"[daemon]
state_dir="{0}"
default_idle_timeout_seconds=1800
[journal]
dir="{0}/journals"
require_trusted_volume=false
[audit]
dir="{0}/audit"
[index]
sqlite_path="{0}/index.sqlite"
[cache]
tape_catalog_dir="{0}/cache"
"#,
            root.path().display()
        ),
    )
    .unwrap();
    let mut state = remanence_state::StateHandle::open_from_config_file(&config).unwrap();
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "automatic-reasons".into(),
        tape_uuid: [0xD5; 16],
        block_size: 256 * 1024,
        parity_config: ParityConfig::None,
    };
    state
        .provision_tape_with_media(
            ProvisionTapeInput {
                tape_uuid: selected.tape_uuid,
                voltag: "REASONL9".into(),
                block_size: selected.block_size,
                parity: ParityConfig::None,
                force: false,
            },
            remanence_state::LabelProvenance::Operator,
            remanence_state::MediaObservation::label_hint("REASONL9"),
        )
        .unwrap();
    let mut index = state.projection_client().write_handle();
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
    let authority = TapeAuthority::open(
        state.paths().journal_dir.join("tapes"),
        selected.tape_uuid,
        &state.paths().sqlite_path,
    )
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
    report.seal_due = Some(remanence_state::TerminalFinalizationTrigger::ReachedLowWatermark);
    writer.publish_report(&report).unwrap();
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
    let runtime = state.library_runtime();
    runtime.refresh_native(&writer.freeze().unwrap()).unwrap();
    runtime.catch_up().unwrap();
    runtime.take_projection_measurement();
    runtime.catch_up().unwrap();
    assert_eq!(
        runtime.take_projection_measurement().1.len(),
        0,
        "T3 precondition: 0 Apply"
    );

    publish_terminal_intent(&mut index, &mut writer, &spec, &plan).unwrap();
    // Keep the view frozen before the intent: only the direct writer's mark can
    // schedule this unchanged prefix for the following catch-up.
    runtime.take_projection_measurement();
    runtime.catch_up().unwrap();
    assert_eq!(
        runtime.take_projection_measurement().1.len(),
        1,
        "T3 exactly 1 Apply after publish_terminal_intent"
    );
    runtime.catch_up().unwrap();
    assert_eq!(
        runtime.take_projection_measurement().1.len(),
        0,
        "T3 second catch-up: 0 Apply"
    );
    assert_eq!(
        index.get_tape(&selected.tape_uuid).unwrap().unwrap().state,
        "finalizing"
    );
}
