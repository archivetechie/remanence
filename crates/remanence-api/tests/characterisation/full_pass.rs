//! Count history passes on a second mount/write/dismount round through the
//! in-process daemon. A child process isolates the process-wide measurements.
use super::*;
use remanence_state::full_pass::{self as passes, PassKind};

const COMPLETE: &str = "LLR2B_SESSION_COMPLETE";
const TEST: &str = "full_pass::everyday_round_counts_history_passes";

fn isolated() -> bool {
    if std::env::var("LLR2B_SESSION_CHILD").as_deref() == Ok(TEST) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--test-threads=1", "--nocapture"])
        .env("LLR2B_SESSION_CHILD", TEST)
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
        stdout.contains(COMPLETE),
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
    if isolated() {
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
