//! Full catalogue rebuilds keep large native projections atomic across tapes.
//! Exercise both public rebuild entries and compare the prior derived catalogue.
#[path = "support/tape_log_stream.rs"]
#[allow(dead_code)]
mod stream;

use remanence_parity::ParityConfig;
use remanence_state::{
    authority::TapeAuthorityView, NativeObjectFileRecord, ProvisionTapeInput, StateHandle,
    TapeAuthority,
};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

fn write_config(root: &Path) -> PathBuf {
    let config = root.join("config.toml");
    std::fs::write(
        &config,
        format!(
            r#"[daemon]
state_dir = "{0}/state"
default_idle_timeout_seconds = 1800
[journal]
dir = "{0}/native"
require_trusted_volume = false
[audit]
dir = "{0}/library"
fsync = true
[index]
sqlite_path = "{0}/index.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
"#,
            root.display()
        ),
    )
    .unwrap();
    config
}

fn project_large_tape(state: &mut StateHandle, id: u8, object_id: &str) {
    let tape_uuid = [id; 16];
    state
        .provision_tape(ProvisionTapeInput {
            tape_uuid,
            voltag: format!("WBR00{id}L9"),
            block_size: 262144,
            parity: ParityConfig::None,
            force: false,
        })
        .unwrap();
    let authority =
        TapeAuthority::open_fixture(state.paths().journal_dir.join("tapes"), tape_uuid).unwrap();
    let mut owner = authority.acquire_exclusive().unwrap();
    let mut report = stream::semantic_report();
    report.tape_uuid = tape_uuid;
    report.session_id = Some(tape_uuid);
    report.operation_id = tape_uuid;
    let projection = &mut report.payloads[0].projection;
    projection.object.object_id = object_id.into();
    projection.instance.object_id = object_id.into();
    projection.instance.tape_uuid = tape_uuid;
    projection.object_recovery_row = projection
        .object_recovery_row
        .clone()
        .with_object_id(object_id.as_bytes().to_vec());
    owner
        .publish_report_streaming(&report, &mut |_, emit| {
            for n in 0..2000 {
                let mut member = stream::semantic_member(n);
                member.object_id = object_id.into();
                emit(&member)?;
            }
            Ok(())
        })
        .unwrap();
    state
        .write_handle()
        .project_tape_log(&owner.freeze().unwrap())
        .unwrap();
}

fn derived_counts(sql: &Connection) -> [u64; 5] {
    [
        "objects",
        "object_instances",
        "object_files",
        "tape_files",
        "tapes",
    ]
    .map(|table| {
        sql.query_row(&format!("select count(*) from {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    })
}

fn assert_members_unchanged(
    state: &StateHandle,
    object_id: &str,
    before: &[NativeObjectFileRecord],
) {
    let after = state
        .write_handle()
        .list_native_object_files(object_id)
        .unwrap();
    assert!(
        after == before,
        "prior catalogue members for {object_id} differ: before={}, after={}",
        before.len(),
        after.len()
    );
}

/// A large live and offline rebuild preserves ordered members and SQL row counts.
#[test]
fn large_typed_rebuild_matches_live_and_offline() {
    let root = tempfile::tempdir().unwrap();
    let config = write_config(root.path());
    let mut state = StateHandle::open_from_config_file(&config).unwrap();
    project_large_tape(&mut state, 7, "o");
    let before = state.write_handle().list_native_object_files("o").unwrap();
    assert_eq!(before.len(), 2000);
    let counts_before =
        derived_counts(&Connection::open(root.path().join("index.sqlite")).unwrap());

    let result = state.rebuild_projection_from_tape_logs();
    assert!(result.is_ok(), "live rebuild must succeed: {result:?}");
    assert_members_unchanged(&state, "o", &before);
    assert_eq!(
        derived_counts(&Connection::open(root.path().join("index.sqlite")).unwrap()),
        counts_before,
        "live rebuild row counts"
    );

    drop(state);
    let result = StateHandle::rebuild_projection_from_config_file(&config);
    assert!(result.is_ok(), "offline rebuild must succeed: {result:?}");
    let state = StateHandle::open_from_config_file(&config).unwrap();
    assert_members_unchanged(&state, "o", &before);
    assert_eq!(
        derived_counts(&Connection::open(root.path().join("index.sqlite")).unwrap()),
        counts_before,
        "offline rebuild row counts"
    );
}

/// A failure in the second tape also rolls back the first tape's released savepoint.
#[test]
fn large_typed_rebuild_failure_retains_prior_catalogue() {
    let root = tempfile::tempdir().unwrap();
    let config = write_config(root.path());
    let mut state = StateHandle::open_from_config_file(&config).unwrap();
    let objects = ["object-6", "object-7"];
    for (id, object_id) in [6, 7].into_iter().zip(objects) {
        project_large_tape(&mut state, id, object_id);
    }
    let before = objects.map(|object_id| {
        let members = state
            .write_handle()
            .list_native_object_files(object_id)
            .unwrap();
        assert_eq!(members.len(), 2000);
        members
    });
    let sql = Connection::open(root.path().join("index.sqlite")).unwrap();
    let counts_before = derived_counts(&sql);
    sql.execute_batch("create trigger stop_rebuild before insert on object_files when (select count(*) from object_files)>=2100 begin select raise(abort,'injected projection failure'); end;").unwrap();

    let result = state.rebuild_projection_from_tape_logs();
    assert!(result.is_err(), "second-tape rebuild must fail");
    for (object_id, members) in objects.into_iter().zip(&before) {
        assert_members_unchanged(&state, object_id, members);
    }
    assert_eq!(
        derived_counts(&sql),
        counts_before,
        "prior catalogue row counts"
    );
    sql.execute_batch("drop trigger stop_rebuild").unwrap();
}
