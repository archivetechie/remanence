//! Count history passes on a second mount/write/dismount round through the
//! in-process daemon. A child process isolates the process-wide measurements.
use super::*;
use remanence_state::full_pass::{self as passes, PassKind};

const COMPLETE: &str = "LLR2B_SESSION_COMPLETE";
const TEST: &str = "full_pass::everyday_round_counts_history_passes";

fn isolated(test: &str, complete: &str) -> bool {
    if std::env::var("LLR2B_SESSION_CHILD").as_deref() == Ok(test) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--test-threads=1", "--nocapture"])
        .env("LLR2B_SESSION_CHILD", test)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "session child failed: {}",
        output.status
    );
    assert!(
        stdout.contains(complete),
        "missing session completion marker"
    );
    true
}

async fn round(client: &Client, fixture: &Harness, name: &str, bytes: &[u8]) -> pb::ObjectRecord {
    let session = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client
                .writer()
                .open_write_session(open_request("archive"))
                .await
            {
                Ok(session) => break session.into_inner(),
                Err(error) => {
                    assert_eq!(error.code(), tonic::Code::FailedPrecondition, "{error}");
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("loading admits a write within ten seconds");
    let object = client.append(&session, name, bytes).await;
    client.close(&session).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while fixture.world.lock().unwrap().drive_bays[&BAY].is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("idle unload returns the cartridge home within ten seconds");
    object
}

/// The counted round begins only after every log its everyday steps read exists.
#[tokio::test]
async fn everyday_round_counts_history_passes() {
    if isolated(TEST, COMPLETE) {
        return;
    }
    let mut fixture = Harness::new(false);
    {
        let mut world = fixture.world.lock().unwrap();
        world.drive_bays.insert(BAY, None);
        let tape = world.tapes[BARCODE].clone();
        world.put_tape_in_slot(0x400, BARCODE, tape);
    }
    fixture.config.daemon.drive_idle_unload_seconds = 1;
    let started = Instant::now();
    let client = fixture.start().await;
    let first = round(&client, &fixture, "full-pass-first", b"first round object").await;
    passes::reset();
    let second = round(
        &client,
        &fixture,
        "full-pass-second",
        b"second round object",
    )
    .await;
    assert_ne!(
        first.object_id, second.object_id,
        "rounds publish distinct objects"
    );

    // Capture before any test-side native-log or catalog reads.
    let observed = passes::counts();
    println!("LLR2B T3 counts: {observed:?}");
    println!(
        "LLR2B T3 elapsed_seconds: {:.6}",
        started.elapsed().as_secs_f64()
    );
    assert!(
        observed
            .iter()
            .any(|&(kind, _, count)| kind == PassKind::LibraryReplay && count > 0),
        "at least one LibraryReplay"
    );
    assert!(
        observed
            .iter()
            .any(|&(kind, _, count)| kind == PassKind::TapeValidate && count > 0),
        "at least one TapeValidate"
    );
    for (kind, file, count) in observed {
        println!("LLR2B T3 row\t{kind:?}\t{file}\t{count}");
        let row = format!("{kind:?}\t{file}");
        assert!(
            include_str!("full-pass-allowlist.tsv")
                .lines()
                .any(|allowed| allowed == row),
            "unallowlisted full pass: {row} (count {count})"
        );
    }
    client.state.shutdown_drive_pool().await.unwrap();
    client.shutdown().await;
    fixture.wait_for_teardown().await;
    println!("{COMPLETE}");
}

/// T6: production manual finalization checks a Prepared reference at append and
/// replay. Its counted round begins after the write has created every input log.
#[tokio::test]
async fn manual_finalization_uses_position_index() {
    const TEST: &str = "full_pass::manual_finalization_uses_position_index";
    const COMPLETE: &str = "LLR3_FINALIZATION_COMPLETE";
    if isolated(TEST, COMPLETE) {
        return;
    }
    remanence_state::tape_log::snapshot::set_position_index_interval(512);
    let fixture = Harness::new(true);
    let client = fixture.start().await;
    let session = client.open().await;
    client.append(&session, "final", b"finalized bytes").await;
    client.close(&session).await;
    passes::reset();
    let mut catalog = pb::catalog_client::CatalogClient::new(client.channel.clone());
    let request = pb::FinalizeTapeRequest {
        tape_uuid: UUID.to_vec(),
        expected_pool_id: Some("archive".into()),
        reason: "archive complete".into(),
        idempotency_key: Some(pb::IdempotencyKey {
            value: vec![0x37; 16],
        }),
    };
    let accepted = catalog.finalize_tape(request).await.unwrap().into_inner();
    let finalized = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let status = catalog
                .get_tape_finalization(pb::GetTapeFinalizationRequest {
                    tape_uuid: UUID.to_vec(),
                })
                .await
                .unwrap()
                .into_inner();
            if status.outcome != pb::TapeFinalizationOutcome::Finalizing as i32 {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("finalization completes");
    let observed = passes::counts();
    for (kind, file, count) in &observed {
        println!("LLR3 T6 row\t{kind:?}\t{file}\t{count}");
    }
    assert_eq!(finalized.operation_id, accepted.operation_id);
    assert_eq!(
        finalized.outcome,
        pb::TapeFinalizationOutcome::Finalized as i32,
        "{finalized:?}"
    );

    // These test-side source reads occur only after the counter was captured.
    let operation: [u8; 16] = accepted.operation_id.unwrap().try_into().unwrap();
    let directory = fixture.config.journal.dir.join("tapes");
    let path = directory.join(format!("{}.remtape", uuid::Uuid::from_bytes(UUID).simple()));
    let length = std::fs::metadata(&path).unwrap().len();
    // Release the actor and RPC connection before acquiring a test-side snapshot.
    client.state.shutdown_drive_pool().await.unwrap();
    drop(catalog);
    client.shutdown().await;
    fixture.wait_for_teardown().await;
    let acceptance = {
        let state =
            StateHandle::open_with_config(state_paths(&fixture.config), fixture.config.clone())
                .unwrap();
        state
            .library_runtime()
            .manual_acceptance(operation)
            .unwrap()
            .unwrap()
    };
    let cited = acceptance.value.at.sequence;
    let snapshot = remanence_state::tape_log::store::Store::open_fixture(&directory, UUID)
        .unwrap()
        .snapshot()
        .unwrap();
    let mut start = None;
    let mut prepared = None;
    snapshot
        .visit_frames(|frame| {
            start.get_or_insert(frame.offset);
            if frame.sequence == cited {
                assert!(matches!(
                    frame.record,
                    remanence_state::tape_log::Record::FinalizationPrepared(_)
                ));
                prepared = Some(frame.offset);
            }
            Ok(())
        })
        .unwrap();
    let start = start.unwrap();
    let offset = prepared.expect("cited FinalizationPrepared exists");
    println!("LLR3 T6 geometry: file_bytes={length} start={start} prepared_offset={offset} cited_sequence={cited}");
    assert!(
        length - start >= 4 * 512,
        "T6 premise: log spans four intervals"
    );
    assert!(
        offset - start >= 512,
        "T6 premise: Prepared starts at least one interval into the log"
    );
    for label in [
        "crates/remanence-state/src/library_log/native_view.rs",
        "crates/remanence-state/src/library_runtime.rs",
    ] {
        let count: u64 = observed
            .iter()
            .filter(|(kind, file, _)| *kind == PassKind::TapeFirstFrame && *file == label)
            .map(|(_, _, count)| count)
            .sum();
        assert_eq!(count, 0, "T6 indexed lookup zero TapeFirstFrame: {label}");
    }
    println!("{COMPLETE}");
}
