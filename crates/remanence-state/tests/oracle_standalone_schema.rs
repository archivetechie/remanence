//! Predecessor-reset and assignment-authority oracles for real v0.1.0-shaped data.
//! SQL is used only for fixture construction, observation, and a crash-cut trigger;
//! all assignments and reconciliation under test use CatalogIndex entry points.

use remanence_state::{CatalogIndex, TapePoolProjectionInput, TapePoolRuleConfig};
use rusqlite::{params, Connection};

const AT: &str = "2026-10-15T00:00:00Z";

#[test]
fn c2_schema_v010_requires_reset_without_backfilling_provenance() {
    // Exercise both the released schema (no assignment_generation column) and
    // a pre-migration additive shape with existing nonzero projection versions.
    for has_generation in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("legacy.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(include_str!(
            "../../../fixtures/standalone/index-v0.1.0.sql"
        ))
        .unwrap();
        if has_generation {
            db.execute_batch(
                "ALTER TABLE tapes ADD COLUMN assignment_generation INTEGER NOT NULL DEFAULT 0",
            )
            .unwrap();
        }
        let cases = [
            (1u8, Some("CAP001L7"), Some("L7"), Some("default"), 7i64),
            (2, Some("CAP002M8"), Some("M8"), Some("archive"), 19),
            (3, Some("handwritten"), None, Some("cold"), 3),
            (4, None, None, None, 11),
        ];
        for pool in ["default", "archive", "cold"] {
            db.execute(
                "INSERT INTO tape_pools(pool_id, created_at_utc) VALUES (?1, ?2)",
                params![pool, AT],
            )
            .unwrap();
        }
        for (id, label, _, pool, generation) in cases {
            db.execute(
                "INSERT INTO tapes(tape_uuid, voltag, pool_id, block_size, state, updated_at_utc)
                 VALUES (?1, ?2, ?3, 262144, 'ready', ?4)",
                params![[id; 16].as_slice(), label, pool, AT],
            )
            .unwrap();
            if has_generation {
                db.execute(
                    "UPDATE tapes SET assignment_generation = ?2 WHERE tape_uuid = ?1",
                    params![[id; 16].as_slice(), generation],
                )
                .unwrap();
            }
        }
        drop(db);
        let before = std::fs::read(&path).unwrap();
        for _ in 0..2 {
            let error = CatalogIndex::open(&path).unwrap_err();
            assert!(error
                .to_string()
                .contains(remanence_state::authority_layout::RESET_REQUIRED));
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }
}

fn pool() -> TapePoolProjectionInput {
    TapePoolProjectionInput {
        pool_id: "default".into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        created_at_utc: Some(AT.into()),
    }
}

fn ledger(db: &Connection, id: u8) -> Vec<(i64, Option<String>, String, i64)> {
    db.prepare(
        "SELECT seq, pool_id, source, assignment_generation
                FROM tape_pool_assignments WHERE tape_uuid = ?1 ORDER BY seq",
    )
    .expect("C2: append-only assignment ledger exists")
    .query_map([[id; 16].as_slice()], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[test]
fn c2_ledger_operator_precedence_rule_clear_and_atomic_projection() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("index.sqlite");
    let mut index = CatalogIndex::open(&path).unwrap();
    let db = Connection::open(&path).unwrap();
    // Existing provisioning is used so this test reaches the new ledger even
    // before provenance columns have been introduced.
    for (id, label) in [(1, "MAN001L7"), (2, "AUTO02L7")] {
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid: [id; 16],
                voltag: label.into(),
                block_size: 262144,
                parity: remanence_parity::ParityConfig::None,
                force: false,
            })
            .unwrap();
    }
    index.upsert_tape_pool_projection(pool()).unwrap();
    index
        .compare_and_set_tape_pool_assignment([1; 16], 0, Some("default"))
        .unwrap();
    let operator_rows = ledger(&db, 1);
    assert_eq!(operator_rows.len(), 1);
    assert_eq!(operator_rows[0].1.as_deref(), Some("default"));
    assert_eq!(operator_rows[0].2, "operator");
    let operator_before = index
        .get_tape_assignment_snapshot(&[1; 16])
        .unwrap()
        .unwrap();

    let rules = [TapePoolRuleConfig {
        pool_id: "default".into(),
        prefix: "AUTO".into(),
    }];
    index
        .reconcile_tape_pool_projection_from_rules(&[pool()], &rules)
        .unwrap();
    let rule_before = index
        .get_tape_assignment_snapshot(&[2; 16])
        .unwrap()
        .unwrap();
    let rule_rows = ledger(&db, 2);
    assert_eq!(rule_rows.len(), 1);
    assert_eq!(rule_rows[0].2, "rule");
    assert_eq!(rule_before.pool_id.as_deref(), Some("default"));

    // Cut once both changes have been reached, in either implementation order.
    // Aborting INSERT alone cannot distinguish one transaction from two: an
    // incorrect append-then-commit implementation would also roll that back.
    // The paired cuts expose a separately committed append OR projection.
    db.execute_batch(
        "CREATE TRIGGER oracle_cut_append AFTER INSERT ON tape_pool_assignments
        WHEN NEW.pool_id IS NULL AND EXISTS (
            SELECT 1 FROM tapes WHERE tape_uuid = NEW.tape_uuid AND pool_id IS NULL)
        BEGIN SELECT RAISE(ABORT, 'oracle-after-append'); END;
        CREATE TRIGGER oracle_cut_projection BEFORE UPDATE OF pool_id ON tapes
        WHEN NEW.pool_id IS NULL AND EXISTS (
            SELECT 1 FROM tape_pool_assignments
            WHERE tape_uuid = NEW.tape_uuid AND pool_id IS NULL)
        BEGIN SELECT RAISE(ABORT, 'oracle-after-append'); END;",
    )
    .unwrap();
    let error = index
        .reconcile_tape_pool_projection_from_rules(&[pool()], &[])
        .unwrap_err();
    assert!(error.to_string().contains("oracle-after-append"), "{error}");
    let after_cut = index
        .get_tape_assignment_snapshot(&[2; 16])
        .unwrap()
        .unwrap();
    assert_eq!(after_cut.pool_id, rule_before.pool_id);
    assert_eq!(
        after_cut.assignment_generation,
        rule_before.assignment_generation
    );
    assert_eq!(ledger(&db, 2), rule_rows);
    assert_eq!(ledger(&db, 1), operator_rows);

    db.execute_batch("DROP TRIGGER oracle_cut_append; DROP TRIGGER oracle_cut_projection")
        .unwrap();
    index
        .reconcile_tape_pool_projection_from_rules(&[pool()], &[])
        .unwrap();
    let operator_after = index
        .get_tape_assignment_snapshot(&[1; 16])
        .unwrap()
        .unwrap();
    assert_eq!(operator_after.pool_id, operator_before.pool_id);
    assert_eq!(
        operator_after.assignment_generation,
        operator_before.assignment_generation
    );
    assert_eq!(ledger(&db, 1), operator_rows);
    let cleared = index
        .get_tape_assignment_snapshot(&[2; 16])
        .unwrap()
        .unwrap();
    assert_eq!(cleared.pool_id, None);
    assert_eq!(
        cleared.assignment_generation,
        rule_before.assignment_generation + 1
    );
    let rows = ledger(&db, 2);
    assert_eq!(rows.len(), 2);
    assert!(rows[1].0 > rows[0].0);
    assert_eq!(rows[1].1, None);
    assert_eq!(rows[1].2, "rule");
    assert_eq!(rows[1].3 as u64, cleared.assignment_generation);
    index
        .reconcile_tape_pool_projection_from_rules(&[pool()], &[])
        .unwrap();
    assert_eq!(
        ledger(&db, 2),
        rows,
        "idempotent reconciliation appends nothing"
    );
}
