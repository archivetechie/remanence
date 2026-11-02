//! Native staged-authority RPC answers through the production CLI response paths.
//! Inventory is injected; no socket, changer or tape device is opened.
use super::*;
use pb::{catalog_server::Catalog, library_service_server::LibraryService};
use remanence_parity::authority::TapeAuthorityView;
use remanence_state::{CatalogIndex, ProvisionTapeInput, TapeAuthority};

#[tokio::test]
async fn b7_fix_top_once_json_reads_held_checkpoint_and_partial_lists_exit_nonzero() {
    let root = tempfile::tempdir().unwrap();
    let logs = root.path().join("tapes");
    std::fs::create_dir(&logs).unwrap();
    let mut index = CatalogIndex::open(root.path().join("state.sqlite")).unwrap();
    for (id, barcode) in [(1, "GOOD01L9"), (2, "FRESH1L9"), (3, "BAD001L9")] {
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: [id; 16],
                voltag: barcode.into(),
                block_size: 262144,
                parity: remanence_parity::ParityConfig::None,
                force: false,
            })
            .unwrap();
    }
    let mut world =
        remanence_chaos::model::VirtualWorld::single_drive("CLI-B7", 0x100, "MODEL", 0x400, 1);
    world.put_tape_in_drive(
        0x100,
        "GOOD01L9",
        Some(0x400),
        remanence_chaos::model::VirtualTape::empty(1024 * 1024, 262144),
    );
    let state = remanence_api::ApiState::new(index).with_test_inventory(
        remanence_library::DiscoveryReport {
            complete: true,
            unattached_drives: vec![],
            incomplete_evidence: vec![],
            libraries: vec![world.library_snapshot()],
            warnings: vec![],
        },
    );
    let authority = TapeAuthority::open_fixture(&logs, [1; 16]).unwrap();
    let mut lease = authority.acquire_exclusive().unwrap();
    lease
        .publish_initialized(
            262144,
            None,
            remanence_parity::CommitCursor {
                partition: 0,
                lba: 2,
            },
        )
        .unwrap();
    lease
        .open_session([9; 16], "2026-11-02T00:00:00Z".into())
        .unwrap();
    let mut staged = lease.latest_commit().unwrap().unwrap();
    staged.ordinal += 1;
    lease.stage_report(&staged).unwrap();
    let live = state
        .library_service()
        .get_live_status(tonic::Request::new(pb::GetLiveStatusRequest {}))
        .await
        .unwrap()
        .into_inner();
    let mut out = Vec::new();
    print_top_response(&live, true, &mut out).unwrap();
    let json: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        json["data"]["libraries"][0]["drives"][0]["unprotected_bytes"],
        "0"
    );
    assert_eq!(
        json["data"]["libraries"][0]["drives"][0]["closeout_due"],
        false
    );
    assert!(json["data"]["closeout_debt"].as_array().unwrap().is_empty());
    lease.cancel_staged_report().unwrap();
    drop(lease);
    std::fs::write(
        logs.join(format!("{}.remtape", Uuid::from_bytes([3; 16]).simple())),
        b"unreadable native authority",
    )
    .unwrap();
    let tapes = state
        .catalog_service()
        .list_tapes(tonic::Request::new(pb::ListTapesRequest::default()))
        .await
        .unwrap()
        .into_inner();
    let live = state
        .library_service()
        .get_live_status(tonic::Request::new(pb::GetLiveStatusRequest {}))
        .await
        .unwrap()
        .into_inner();
    for json_output in [true, false] {
        for top in [true, false] {
            let mut out = Vec::new();
            let result = if top {
                print_top_response(&live, json_output, &mut out)
            } else {
                print_catalog_tape_response(tapes.clone(), json_output, &mut out)
            };
            let mut err = Vec::new();
            let code = finish_daemon_client_result(
                result.map_err(DaemonClientError::from),
                json_output,
                &mut err,
            );
            assert_eq!(code, ExitCode::from(1));
            let text = String::from_utf8(out.clone()).unwrap();
            if json_output {
                let json: Value = serde_json::from_slice(&out).unwrap();
                let entries = &json["data"][if top { "closeout_debt" } else { "tapes" }];
                assert_eq!(entries.as_array().unwrap().len(), if top { 1 } else { 3 });
                let bad = entries
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|t| t["tape_uuid"] == Uuid::from_bytes([3; 16]).to_string())
                    .unwrap();
                for field in [
                    "unprotected_bytes",
                    "unprotected_since",
                    "closeout_due",
                    "closeout_overdue",
                ] {
                    assert!(bad[field].is_null(), "{field}: {bad}");
                }
                assert!(bad["closeout_authority_error"].is_string());
            } else {
                assert!(text.contains("DEBT?"));
                assert!(text.contains("BAD001L9"));
                if !top {
                    assert!(text.contains("GOOD01L9") && text.contains("FRESH1L9"));
                }
            }
        }
    }
}

#[test]
fn b7_fix_overdue_finalization_is_visible_and_documented() {
    let tape = pb::Tape {
        unprotected_bytes: Some(42),
        closeout_due: Some(false),
        closeout_overdue: Some(true),
        ..Default::default()
    };
    let mut out = Vec::new();
    print_tape_line(&tape, &mut out);
    assert!(String::from_utf8(out).unwrap().contains("OVERDUE"));
    let docs = include_str!("../../../docs/reference-cli.md");
    assert!(docs.contains("`OVERDUE` can coexist with `closeout_due=false`"));
    assert!(docs.contains("That tape needs finalization to resume"));
}
