//! Explicit tempdir measurements use valid native writer fixtures. They measure
//! source/projection serialization, not hardware or object encoding throughput.
use super::*;
use crate::authority::TapeAuthorityView;

/// Two retained native admission owners publish real closed Objects groups and
/// share the production runtime's dispatch and registered SQL writer.
#[test]
#[ignore = "explicit dual-native-owner projection measurement"]
fn dual_native_owner_projection_measurement() {
    const COMMITS: u64 = 100;
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"[daemon]
state_dir="{0}/state"
default_idle_timeout_seconds=1800
[journal]
dir="{0}/native"
require_trusted_volume=false
[audit]
dir="{0}/library"
[index]
sqlite_path="{0}/catalog.sqlite"
[cache]
tape_catalog_dir="{0}/cache"
"#,
            root.path().display()
        ),
    )
    .unwrap();
    let mut state = crate::StateHandle::open_from_config_file(&config).unwrap();
    for drive in [1u8, 2] {
        state
            .provision_tape_with_media(
                crate::ProvisionTapeInput {
                    tape_uuid: [drive; 16],
                    voltag: format!("MEAS0{drive}L9"),
                    block_size: 256 * 1024,
                    parity: remanence_parity::ParityConfig::None,
                    force: false,
                },
                crate::LabelProvenance::Operator,
                crate::MediaObservation::label_hint(&format!("MEAS0{drive}L9")),
            )
            .unwrap();
    }
    let runtime = state.library_runtime();
    let ready = Arc::new(std::sync::Barrier::new(3));
    let start = Arc::new(std::sync::Barrier::new(3));
    let mut owners = Vec::new();
    for drive in [1u8, 2] {
        let paths = state.paths().clone();
        let runtime = runtime.clone();
        let ready = ready.clone();
        let start = start.clone();
        owners.push(std::thread::spawn(move || {
            let uuid = [drive; 16];
            let authority = crate::TapeAuthority::open(
                paths.journal_dir.join("tapes"),
                uuid,
                &paths.sqlite_path,
            )
            .unwrap();
            let mut writer = authority.acquire_exclusive().unwrap();
            writer
                .publish_initialized(
                    256 * 1024,
                    None,
                    remanence_parity::CommitCursor {
                        partition: 0,
                        lba: 2,
                    },
                )
                .unwrap();
            writer
                .open_session([drive + 20; 16], "2026-11-06T00:00:00Z".into())
                .unwrap();
            let mut index = crate::projector::open(&paths.sqlite_path)
                .unwrap()
                .with_library_runtime(runtime.clone())
                .write_handle();
            index.project_tape_log(&writer.freeze().unwrap()).unwrap();
            ready.wait();
            start.wait();
            let mut lags = Vec::new();
            let begin = std::time::Instant::now();
            for ordinal in 1..=COMMITS {
                let mut report = crate::checkpoint::tests::record(uuid);
                let mut id = [drive; 16];
                id[8..].copy_from_slice(&ordinal.to_le_bytes());
                let object = uuid::Uuid::from_bytes(id).to_string();
                report.ordinal = ordinal;
                report.committed_object_count = ordinal;
                report.eod_lba = 2 + 4 * ordinal;
                report.batch_id = id;
                report.next_tape_file_number = ordinal + 1;
                report.objects[0].object.object_id = object.clone();
                report.objects[0].object.caller_object_id =
                    Some(format!("drive-{drive}-{ordinal}"));
                report.objects[0].instance.object_id = object.clone();
                report.objects[0].instance.tape_file_number = ordinal;
                report.objects[0].fresh_tape = false;
                report.objects[0].total_committed_ordinals = 3 * ordinal;
                report.objects[0].object_recovery_row.tape_file_number = ordinal;
                report.objects[0].object_recovery_row.object_id = object.into_bytes();
                let report = (&report).into();
                writer.stage_report(&report).unwrap();
                writer.complete_report(&report).unwrap();
                let durable_return = std::time::Instant::now();
                index.project_tape_log(&writer.freeze().unwrap()).unwrap();
                lags.push(durable_return.elapsed());
            }
            let elapsed = begin.elapsed();
            writer
                .end_session(
                    [drive + 20; 16],
                    "2026-11-06T00:01:00Z".into(),
                    crate::tape_log::SessionEndReason::CleanClose,
                    None,
                    std::iter::empty(),
                )
                .unwrap();
            index.project_tape_log(&writer.freeze().unwrap()).unwrap();
            (elapsed, lags)
        }));
    }
    // Initialization is excluded from queue/lag and owner wall measurements.
    ready.wait();
    runtime
        .0
        .native_projection_high_water
        .store(0, Ordering::SeqCst);
    start.wait();
    let mut all = Vec::new();
    let mut seconds = Vec::new();
    for owner in owners {
        let (elapsed, lags) = owner.join().unwrap();
        seconds.push(elapsed.as_secs_f64());
        all.extend(lags);
    }
    assert_eq!(
        state.catalog().list_native_objects().unwrap().len(),
        2 * COMMITS as usize
    );
    all.sort();
    println!("CUTOVER_NATIVE drives=2 commits={} native_commit_return_to_projection_samples={} p50_ms={} p99_ms={} native_dispatch_waiting_high_water={} owner_seconds={seconds:?}",2*COMMITS,all.len(),all[all.len()/2].as_secs_f64()*1000.0,all[all.len()*99/100].as_secs_f64()*1000.0,runtime.0.native_projection_high_water.load(Ordering::SeqCst));
}
