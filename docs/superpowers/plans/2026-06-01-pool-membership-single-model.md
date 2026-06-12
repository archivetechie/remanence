# Pool Membership Single-Model + Column Projection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make barcode-prefix rules the single config-driven pool-membership path, and store derived membership as a `tapes.pool_id` column instead of the `tape_pool_memberships` table.

**Architecture:** Remove the rejected static-row config input (`[[tape_pool_memberships]]` + `reconcile_tape_pool_projection`). Replace the 1:1 `tape_pool_memberships` projection table with a nullable `tapes.pool_id` column written by the existing rules reconciler and read directly by `list_tapes`/`get_tape`/`get_tape_by_voltag` and the commit-time snapshot. Migrate in place via the established `migrate()`/`ensure_column` mechanism, bumping `SCHEMA_VERSION` 5 → 6.

**Tech Stack:** Rust, `rusqlite` (bundled SQLite), workspace crates `remanence-state` (schema/index/config/state), `remanence-api`, `remanence-cli`.

Spec: `docs/pool-membership-storage-design-v0.1.md`. Parent design: `docs/pool-membership-design-v0.1.md`.

**Gates (run before every commit):**
```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p remanence-state
```
(Run the full `cargo test` at the final task.)

---

## File Structure

- `crates/remanence-state/src/config.rs` — remove the static-row config type, field, and validation.
- `crates/remanence-state/src/index.rs` — schema/migration, reconciler, projection helpers, the three tape read queries, the static reconciler removal, test invariant.
- `crates/remanence-state/src/state.rs` — collapse the if/else reconcile dispatch; replace the membership-fallback test.
- `crates/remanence-state/src/lib.rs` — drop two re-exports.
- `docs/pool-membership-design-v0.1.md` — doc note (single source of truth).

No new files. Each task is self-contained and leaves the workspace compiling.

---

### Task 1: Add `tapes.pool_id` column + migration (schema first, still dual-write)

This task only adds the new column and its index and bumps the schema version. The reconciler still writes the old table; nothing reads the column yet. This keeps the build green and isolates the schema change.

**Files:**
- Modify: `crates/remanence-state/src/index.rs` (`SCHEMA_VERSION` at :22, `migrate` at :3333-3377, `MINIMUM_SCHEMA` `tapes` block at :3434)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `crates/remanence-state/src/index.rs`:

`CatalogIndex` has no public connection accessor (`self.conn` is private). Follow the existing test pattern (see index.rs:4387) and open a fresh raw `Connection` on the same file; module-private helpers like `table_column_exists` take `&Connection` and are callable from this test module:

```rust
#[test]
fn tapes_table_has_pool_id_column_after_migration() {
    let dir = TempDir::new("rem-pool-col").expect("tempdir");
    let path = dir.path().join("rem-state.sqlite");
    let index = CatalogIndex::open(&path).expect("open index");
    assert_eq!(index.schema_version().expect("schema version"), 6);

    let conn = Connection::open(&path).expect("open raw sqlite");
    assert!(
        table_column_exists(&conn, "tapes", "pool_id").expect("table_info"),
        "tapes.pool_id column must exist after migration"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-state tapes_table_has_pool_id_column_after_migration`
Expected: FAIL — `schema_version` is 5 and/or `pool_id` column missing.

- [ ] **Step 3: Bump schema version**

In `crates/remanence-state/src/index.rs:22`:

```rust
pub const SCHEMA_VERSION: u32 = 6;
```

- [ ] **Step 4: Add `pool_id` to the `tapes` block in `MINIMUM_SCHEMA`**

In the `create table if not exists tapes(...)` block (around :3434), add a `pool_id` column after `voltag`:

```
create table if not exists tapes(
  tape_uuid blob primary key,
  voltag text,
  pool_id text,
  block_size integer,
```

(Leave the rest of the column list unchanged.)

- [ ] **Step 5: Add the `ensure_column` + partial index in `migrate`**

In `migrate` (around :3351, right after the existing `object_copies` / `catalog_units` `ensure_column` calls and before the `object_copies_pool_idx` creation), add:

```rust
    ensure_column(conn, "tapes", "pool_id", "pool_id text")?;
```

Then, after the existing `tapes_voltag_unique` index creation (around :3367), add the partial index:

```rust
    conn.execute(
        "create index if not exists tapes_pool_idx
         on tapes(pool_id)
         where pool_id is not null",
        [],
    )
    .map_err(|err| sqlite_error("create tapes_pool_idx", err))?;
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p remanence-state tapes_table_has_pool_id_column_after_migration`
Expected: PASS.

- [ ] **Step 7: Run gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p remanence-state
git add crates/remanence-state/src/index.rs
git commit -m "Add tapes.pool_id column + tapes_pool_idx, bump schema to v6"
```

---

### Task 2: Switch the rules reconciler and projection helpers to `tapes.pool_id`

Retarget the write path (`project_tape_pool_membership_tx`, `query_memberships_tx`, and the reconciler's stale-clear loop) from the `tape_pool_memberships` table to `tapes.pool_id`, and drop the dead `assigned_at_utc` parameter. The table still exists (dropped in Task 4) but is no longer written by the reconciler.

**Files:**
- Modify: `crates/remanence-state/src/index.rs` (`reconcile_tape_pool_projection_from_rules` :837-924, `project_tape_pool_membership` :931-951, `project_tape_pool_membership_tx` :2823-2858, `query_memberships_tx` :2860-2882)

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `index.rs`:

```rust
#[test]
fn rules_reconcile_writes_and_clears_tapes_pool_id() {
    let dir = TempDir::new("rem-pool-recon").expect("tempdir");
    let mut index = CatalogIndex::open(dir.path().join("s.sqlite")).expect("open");
    let uuid = [7u8; 16];
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: uuid,
            voltag: "RMN001L9".to_string(),
            block_size: 65536,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision");

    let pools = vec![TapePoolProjectionInput {
        pool_id: "scenario-a".to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        created_at_utc: None,
    }];
    let rules = vec![TapePoolRuleConfig {
        prefix: "RMN".to_string(),
        pool_id: "scenario-a".to_string(),
    }];
    index
        .reconcile_tape_pool_projection_from_rules(&pools, &rules)
        .expect("reconcile with rule");
    assert_eq!(
        index.get_tape_pool_membership(&uuid).expect("lookup"),
        Some("scenario-a".to_string())
    );

    // Removing the rule clears the derived pool_id.
    index
        .reconcile_tape_pool_projection_from_rules(&pools, &[])
        .expect("reconcile no rules");
    assert_eq!(index.get_tape_pool_membership(&uuid).expect("lookup"), None);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-state rules_reconcile_writes_and_clears_tapes_pool_id`
Expected: FAIL — second assertion fails (clearing isn't wired to `tapes.pool_id`), or first assertion reads `None` because `get_tape_pool_membership` still reads the (un-updated by column) table. (Either failure is fine; the test passes only after the rewrite.)

- [ ] **Step 3: Rewrite `project_tape_pool_membership_tx` to write the column (drop `assigned_at_utc`)**

Replace the whole function (`index.rs:2823-2858`) with:

```rust
fn project_tape_pool_membership_tx(
    tx: &rusqlite::Transaction<'_>,
    tape_uuid: [u8; 16],
    pool_id: &str,
) -> Result<(), StateError> {
    let conflicting_pool: Option<Option<String>> = tx
        .query_row(
            "select pool_id
             from object_copies
             where tape_uuid = ?1
               and (pool_id is null or pool_id != ?2)
             order by pool_id is not null, pool_id
             limit 1",
            params![tape_uuid.to_vec(), pool_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| sqlite_error("check tape pool reassignment", err))?;
    if let Some(conflicting_pool) = conflicting_pool {
        let conflicting_pool = conflicting_pool.as_deref().unwrap_or("unassigned");
        return Err(StateError::TapePoolAssignmentConflict(format!(
            "tape {} already has committed copies in pool {conflicting_pool}; cannot assign to {pool_id}",
            hex_uuid(tape_uuid)
        )));
    }
    tx.execute(
        "update tapes set pool_id = ?2 where tape_uuid = ?1",
        params![tape_uuid.to_vec(), pool_id],
    )
    .map_err(|err| sqlite_error("project tape pool membership", err))?;
    Ok(())
}
```

- [ ] **Step 4: Rewrite `query_memberships_tx` to read the column**

Replace the SQL in `query_memberships_tx` (`index.rs:2860-2882`). Change the prepared statement and the error/label strings:

```rust
fn query_memberships_tx(
    tx: &rusqlite::Transaction<'_>,
) -> Result<Vec<(Vec<u8>, String)>, StateError> {
    let mut stmt = tx
        .prepare("select tape_uuid, pool_id from tapes where pool_id is not null")
        .map_err(|err| sqlite_error("prepare tape pool membership reconciliation query", err))?;
    let mut rows = stmt
        .query([])
        .map_err(|err| sqlite_error("query tape pool membership reconciliation", err))?;
    let mut memberships = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|err| sqlite_error("iterate tape pool membership reconciliation", err))?
    {
        memberships.push((
            row_get(row, 0, "tapes.tape_uuid")?,
            row_get(row, 1, "tapes.pool_id")?,
        ));
    }
    Ok(memberships)
}
```

- [ ] **Step 5: Update the reconciler's stale-clear loop and projection call**

In `reconcile_tape_pool_projection_from_rules`, the stale-membership loop (around :884-893) currently does `delete from tape_pool_memberships where tape_uuid = ?1`. Replace it with a column clear:

```rust
        let existing_memberships = query_memberships_tx(&tx)?;
        for (tape_uuid, pool_id) in existing_memberships {
            if !configured_memberships.contains(&(tape_uuid.clone(), pool_id)) {
                tx.execute(
                    "update tapes set pool_id = null where tape_uuid = ?1",
                    params![tape_uuid],
                )
                .map_err(|err| sqlite_error("clear stale derived tape pool membership", err))?;
            }
        }
```

And the projection loop (around :916-918) drops the timestamp argument:

```rust
        for (tape_uuid, pool_id) in normalized_memberships {
            project_tape_pool_membership_tx(&tx, tape_uuid, pool_id.as_str())?;
        }
```

- [ ] **Step 6: Update the public `project_tape_pool_membership` wrapper**

Replace the body of `project_tape_pool_membership` (`index.rs:931-951`) to drop `assigned_at_utc`:

```rust
    pub fn project_tape_pool_membership(
        &mut self,
        tape_uuid: [u8; 16],
        pool_id: &str,
    ) -> Result<(), StateError> {
        let pool_id = normalize_pool_id(pool_id)?;
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin tape pool membership projection", err))?;
        project_tape_pool_membership_tx(&tx, tape_uuid, pool_id.as_str())?;
        tx.commit()
            .map_err(|err| sqlite_error("commit tape pool membership projection", err))?;
        Ok(())
    }
```

- [ ] **Step 7: Point `get_tape_pool_membership` at the column**

Replace the SQL in `get_tape_pool_membership` (`index.rs:1006`):

```rust
                "select pool_id from tapes where tape_uuid = ?1",
```

(Keep the rest of the method, including the `params![tape_uuid.to_vec()]` and `.optional()` handling, unchanged.)

- [ ] **Step 8: Run the test to verify it passes**

Run: `cargo test -p remanence-state rules_reconcile_writes_and_clears_tapes_pool_id`
Expected: PASS.

- [ ] **Step 9: Run gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p remanence-state
git add crates/remanence-state/src/index.rs
git commit -m "Write derived pool membership to tapes.pool_id; drop dead assigned_at_utc"
```

---

### Task 3: Read `tapes.pool_id` directly in the three tape queries + commit snapshot

Drop the `left join tape_pool_memberships` from `list_tapes`, `get_tape`, and `get_tape_by_voltag`, and select `tapes.pool_id`. Point the commit-time `object_copies.pool_id` snapshot subquery at `tapes`.

**Files:**
- Modify: `crates/remanence-state/src/index.rs` (`list_tapes` :1191-1212, `get_tape` :1238-1257, `get_tape_by_voltag` :1281-1300, commit snapshot :2948)

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `index.rs`:

```rust
#[test]
fn list_tapes_filters_by_pool_id_column() {
    let dir = TempDir::new("rem-pool-list").expect("tempdir");
    let mut index = CatalogIndex::open(dir.path().join("s.sqlite")).expect("open");
    let uuid = [9u8; 16];
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: uuid,
            voltag: "RMN042L9".to_string(),
            block_size: 65536,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision");
    index
        .project_tape_pool_membership(uuid, "scenario-a")
        .expect("assign");

    let in_pool = index.list_tapes(Some("scenario-a")).expect("list in pool");
    assert_eq!(in_pool.len(), 1);
    assert_eq!(in_pool[0].pool_id.as_deref(), Some("scenario-a"));

    let other = index.list_tapes(Some("nope")).expect("list other pool");
    assert!(other.is_empty());
}
```

(If `TapeRecord`'s pool field is named differently than `pool_id`, match the field used by `tape_from_row` — confirm by reading `TapeRecord` near the top of `index.rs`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-state list_tapes_filters_by_pool_id_column`
Expected: FAIL — `list_tapes` still joins/filters on `tape_pool_memberships`, which the reconciler/projection no longer populate, so the pool filter returns nothing.

- [ ] **Step 3: Rewrite `list_tapes`**

In `list_tapes` (:1186-1211), change the `where_clause` and the `select`/`from`:

```rust
        let where_clause = if pool_id.is_some() {
            " where tapes.pool_id = ?1"
        } else {
            ""
        };
        let sql = format!(
            "select tapes.tape_uuid, tapes.voltag, tapes.pool_id,
                    (
                      select objects.body_format
                      from catalog_units
                      join objects on objects.object_id = catalog_units.native_object_id
                      where catalog_units.tape_uuid = tapes.tape_uuid
                        and catalog_units.origin_kind = 'native_object'
                        and objects.body_format is not null
                      group by objects.body_format
                      order by count(*) desc, objects.body_format
                      limit 1
                    ),
                    block_size, scheme_id,
                    data_blocks_per_stripe, parity_blocks_per_stripe,
                    stripes_per_neighborhood, last_committed_tape_file,
                    total_committed_ordinals, state, updated_at_utc
             from tapes{where_clause}
             order by hex(tapes.tape_uuid)"
        );
```

- [ ] **Step 4: Rewrite `get_tape`**

In `get_tape` (:1238-1257), change `tape_pool_memberships.pool_id` → `tapes.pool_id` and drop the join:

```rust
                "select tapes.tape_uuid, tapes.voltag, tapes.pool_id,
                        (
                          select objects.body_format
                          from catalog_units
                          join objects on objects.object_id = catalog_units.native_object_id
                          where catalog_units.tape_uuid = tapes.tape_uuid
                            and catalog_units.origin_kind = 'native_object'
                            and objects.body_format is not null
                          group by objects.body_format
                          order by count(*) desc, objects.body_format
                          limit 1
                        ),
                        block_size, scheme_id,
                        data_blocks_per_stripe, parity_blocks_per_stripe,
                        stripes_per_neighborhood, last_committed_tape_file,
                        total_committed_ordinals, state, updated_at_utc
                 from tapes
                 where tapes.tape_uuid = ?1",
```

- [ ] **Step 5: Rewrite `get_tape_by_voltag`**

In `get_tape_by_voltag` (:1281-1300), apply the same change, keeping the `where tapes.voltag = ?1` clause:

```rust
                "select tapes.tape_uuid, tapes.voltag, tapes.pool_id,
                        (
                          select objects.body_format
                          from catalog_units
                          join objects on objects.object_id = catalog_units.native_object_id
                          where catalog_units.tape_uuid = tapes.tape_uuid
                            and catalog_units.origin_kind = 'native_object'
                            and objects.body_format is not null
                          group by objects.body_format
                          order by count(*) desc, objects.body_format
                          limit 1
                        ),
                        block_size, scheme_id,
                        data_blocks_per_stripe, parity_blocks_per_stripe,
                        stripes_per_neighborhood, last_committed_tape_file,
                        total_committed_ordinals, state, updated_at_utc
                 from tapes
                 where tapes.voltag = ?1",
```

- [ ] **Step 6: Point the commit snapshot subquery at `tapes`**

In the `object_copies` insert (`index.rs:2948`), change the correlated subquery:

```rust
           (select pool_id from tapes where tape_uuid = ?2)
```

- [ ] **Step 7: Run the test to verify it passes**

Run: `cargo test -p remanence-state list_tapes_filters_by_pool_id_column`
Expected: PASS.

- [ ] **Step 8: Run gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p remanence-state
git add crates/remanence-state/src/index.rs
git commit -m "Read pool membership from tapes.pool_id in tape queries + commit snapshot"
```

---

### Task 4: Drop the `tape_pool_memberships` table (schema + backfill) and the test invariant entry

Now that nothing reads or writes the table, remove it: backfill any existing rows into `tapes.pool_id`, drop the table and its index from `MINIMUM_SCHEMA`, and remove it from the test `MINIMUM_TABLES` list.

**Files:**
- Modify: `crates/remanence-state/src/index.rs` (`migrate` :3343-3377, `MINIMUM_SCHEMA` :3461-3469, `MINIMUM_TABLES` :3593)

- [ ] **Step 1: Write the failing test**

Add to `#[cfg(test)] mod tests` in `index.rs`:

```rust
#[test]
fn tape_pool_memberships_table_is_dropped() {
    let dir = TempDir::new("rem-drop-tbl").expect("tempdir");
    let path = dir.path().join("s.sqlite");
    let _index = CatalogIndex::open(&path).expect("open");
    let conn = Connection::open(&path).expect("open raw sqlite");
    let exists: Option<String> = conn
        .query_row(
            "select name from sqlite_master where type='table' and name='tape_pool_memberships'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("query sqlite_master");
    assert!(exists.is_none(), "tape_pool_memberships table must be dropped");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-state tape_pool_memberships_table_is_dropped`
Expected: FAIL — the table is still created by `MINIMUM_SCHEMA`.

- [ ] **Step 3: Backfill + drop in `migrate`**

In `migrate`, after the `ensure_column(conn, "tapes", "pool_id", ...)` call (Task 1) and before the `user_version` bump, add a guarded backfill then drop. (Backfill must run before the table is removed from `MINIMUM_SCHEMA`, and must tolerate the table being absent on fresh DBs.)

```rust
    if table_exists(conn, "tape_pool_memberships")? {
        conn.execute(
            "update tapes set pool_id = (
                 select m.pool_id from tape_pool_memberships m
                 where m.tape_uuid = tapes.tape_uuid
             )
             where pool_id is null
               and exists (
                 select 1 from tape_pool_memberships m where m.tape_uuid = tapes.tape_uuid
               )",
            [],
        )
        .map_err(|err| sqlite_error("backfill tapes.pool_id", err))?;
        conn.execute("drop table tape_pool_memberships", [])
            .map_err(|err| sqlite_error("drop tape_pool_memberships", err))?;
    }
```

- [ ] **Step 4: Add the `table_exists` helper**

If no `table_exists` helper exists (there is a `table_column_exists` at :3395 but not a table-level one), add one next to it:

```rust
fn table_exists(conn: &Connection, table_name: &str) -> Result<bool, StateError> {
    let found: Option<String> = conn
        .query_row(
            "select name from sqlite_master where type = 'table' and name = ?1",
            params![table_name],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| sqlite_error("check sqlite table existence", err))?;
    Ok(found.is_some())
}
```

- [ ] **Step 5: Remove the table + index from `MINIMUM_SCHEMA`**

Delete this block from `MINIMUM_SCHEMA` (:3461-3469):

```
create table if not exists tape_pool_memberships(
  tape_uuid blob primary key,
  pool_id text not null,
  assigned_at_utc text not null,
  foreign key(pool_id) references tape_pools(pool_id)
);

create index if not exists tape_pool_memberships_pool_idx
  on tape_pool_memberships(pool_id);
```

- [ ] **Step 6: Remove the table from the `MINIMUM_TABLES` test list**

In `MINIMUM_TABLES` (:3593), delete the `"tape_pool_memberships",` entry.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p remanence-state tape_pool_memberships_table_is_dropped`
Expected: PASS.
Run: `cargo test -p remanence-state migrations_create_minimum_tables_and_pragmas`
Expected: PASS (the invariant list no longer expects the dropped table).

- [ ] **Step 8: Run gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p remanence-state
git add crates/remanence-state/src/index.rs
git commit -m "Drop tape_pool_memberships table with backfill into tapes.pool_id"
```

---

### Task 5: Remove the static reconciler and `TapePoolMembershipProjectionInput`

Delete the now-unused `reconcile_tape_pool_projection` (the static-row reconciler) and its input type. Fix the stale doc comment in `provision_tape`.

**Files:**
- Modify: `crates/remanence-state/src/index.rs` (`TapePoolMembershipProjectionInput` :100, `reconcile_tape_pool_projection` :751-829, doc comment :505-506)

- [ ] **Step 1: Confirm no remaining references**

Run: `grep -rn "reconcile_tape_pool_projection\b\|TapePoolMembershipProjectionInput" crates/ --include=*.rs | grep -v "_from_rules"`
Expected after Task 5: only the definitions in `index.rs` and the re-export in `lib.rs` (removed in Task 6). The `state.rs` caller is removed in Task 6 Step 3 — if it still appears here, do Task 6 first. (Tasks 5 and 6 may be done together; if you hit a compile error from `state.rs`/`lib.rs` referencing removed items, complete Task 6 before re-running gates.)

- [ ] **Step 2: Delete `reconcile_tape_pool_projection`**

Remove the entire method `pub fn reconcile_tape_pool_projection(...) { ... }` (`index.rs:751-829`, the one whose body deletes/inserts into `tape_pool_memberships` and takes `memberships: &[TapePoolMembershipProjectionInput]`). Leave `reconcile_tape_pool_projection_from_rules` intact.

- [ ] **Step 3: Delete `TapePoolMembershipProjectionInput`**

Remove the `pub struct TapePoolMembershipProjectionInput { ... }` definition (`index.rs:100`).

- [ ] **Step 4: Fix the stale doc comment**

In `provision_tape` (`index.rs:505-506`), update the comment that reads *"Pool membership remains config/policy-owned through `tape_pool_memberships`."* to:

```rust
    /// Provisioning owns only the `tapes` row. Pool membership is derived from
    /// the barcode via `[[tape_pool_rules]]` and projected onto `tapes.pool_id`
    /// by `reconcile_tape_pool_projection_from_rules`.
```

- [ ] **Step 5: Compile-check (gates run after Task 6)**

Run: `cargo build -p remanence-state 2>&1 | head -30`
Expected: errors only from `state.rs`/`lib.rs` still referencing removed items (fixed in Task 6). If `index.rs` itself errors, resolve before proceeding.

- [ ] **Step 6: (Commit happens at end of Task 6.)**

No commit yet — Tasks 5 and 6 land together because removing the type breaks `state.rs`/`lib.rs` until Task 6 is done.

---

### Task 6: Collapse the reconcile dispatch and remove the static-row config input

Make `state.rs` always reconcile from rules, remove the `[[tape_pool_memberships]]` config type/field/validation, drop the re-exports, and replace the fallback test.

**Files:**
- Modify: `crates/remanence-state/src/state.rs` (dispatch :340-369, test :567)
- Modify: `crates/remanence-state/src/config.rs` (field :29, struct :133, validation loop :271-287, fixtures :659-748)
- Modify: `crates/remanence-state/src/lib.rs` (re-exports :32, :41)

- [ ] **Step 1: Collapse the reconcile dispatch in `state.rs`**

Replace the `if config.tape_pool_rules.is_empty() { ... } else { ... }` block (`state.rs:351-368`) with an unconditional rules reconcile, plus the empty-rules-with-pools startup warning:

```rust
    if config.tape_pool_rules.is_empty() && !config.tape_pools.is_empty() {
        eprintln!(
            "warning: {} tape pool(s) defined but no [[tape_pool_rules]]; no tape will be pool-eligible until a rule is added",
            config.tape_pools.len()
        );
    }
    index.reconcile_tape_pool_projection_from_rules(&pools, &config.tape_pool_rules)
```

Delete the now-unused `memberships` binding above it (the `config.tape_pool_memberships.iter().map(...)` block at :352-364). If `Uuid` becomes an unused import after this, remove it (let clippy tell you).

(If the project has a structured logger rather than `eprintln!`, match the warn-level pattern used elsewhere in `state.rs`. Search for existing `warn!`/`eprintln!` usage; if none, `eprintln!` is acceptable.)

- [ ] **Step 2: Remove the config field, struct, and validation**

In `crates/remanence-state/src/config.rs`:

- Delete the field (`:29`): `pub tape_pool_memberships: Vec<TapePoolMembershipConfig>,` and its doc comment / `#[serde(default)]` attribute.
- Delete the `pub struct TapePoolMembershipConfig { ... }` (`:130-138`) and its doc comment.
- Delete the validation loop `for membership in &config.tape_pool_memberships { ... }` (`:271-287`), including the `membership_tapes` `HashSet` it populates. Keep the surrounding `pool_ids` collection and the `validate_tape_pool_rules(...)` call.

- [ ] **Step 3: Update config tests/fixtures in `config.rs`**

Remove the `[[tape_pool_memberships]]` blocks from the inline TOML fixtures (`:659`, `:710`) and the assertions that reference `config.tape_pool_memberships` (`:692`, `:723`, `:745`, `:748`). If a whole test exists only to validate static memberships, delete that test; otherwise just delete the membership-specific lines. After editing, the remaining assertions must still describe the fixture accurately (e.g. a fixture that declared a pool + a membership now declares a pool + a `[[tape_pool_rules]]` rule — add a rule line if the test still needs a derived pool).

- [ ] **Step 4: Drop the re-exports in `lib.rs`**

In `crates/remanence-state/src/lib.rs`, remove `TapePoolMembershipConfig` from the `:32` re-export list and `TapePoolMembershipProjectionInput` from the `:41` re-export list. Leave `TapePoolConfig`, `TapePoolRuleConfig`, `RemConfig`, etc. intact.

- [ ] **Step 5: Replace the fallback test in `state.rs`**

The test `reopen_reconciles_changed_config_tape_pool_membership` (`state.rs:567`) exercises the removed static-row path. Replace it with a rules-based reopen test asserting derived `tapes.pool_id` survives a reopen and tracks rule changes:

```rust
    #[test]
    fn reopen_reconciles_derived_pool_membership_from_rules() {
        let dir = TempDir::new("rem-reopen-rules").expect("tempdir");
        let sqlite = dir.path().join("rem-state.sqlite");

        // Provision a tape and reconcile with a matching rule.
        {
            let mut index = CatalogIndex::open(&sqlite).expect("open");
            index
                .provision_tape(ProvisionTapeInput {
                    tape_uuid: [3u8; 16],
                    voltag: "RMN007L9".to_string(),
                    block_size: 65536,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("provision");
            let pools = vec![TapePoolProjectionInput {
                pool_id: "scenario-a".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            }];
            let rules = vec![TapePoolRuleConfig {
                prefix: "RMN".to_string(),
                pool_id: "scenario-a".to_string(),
            }];
            index
                .reconcile_tape_pool_projection_from_rules(&pools, &rules)
                .expect("reconcile");
        }

        // Reopen and confirm the derived membership is still present.
        {
            let index = CatalogIndex::open(&sqlite).expect("reopen");
            assert_eq!(
                index.get_tape_pool_membership(&[3u8; 16]).expect("lookup"),
                Some("scenario-a".to_string())
            );
        }
    }
```

(Match the imports/helpers already used by neighboring `state.rs` tests; adjust the module path of `TapePoolProjectionInput`/`TapePoolRuleConfig`/`ProvisionTapeInput` to however that test module refers to them.)

- [ ] **Step 6: Run gates (full workspace) and commit Tasks 5 + 6**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p remanence-state
git add crates/remanence-state/src/index.rs crates/remanence-state/src/state.rs crates/remanence-state/src/config.rs crates/remanence-state/src/lib.rs
git commit -m "Remove static-row membership model; always reconcile pool membership from rules"
```

---

### Task 7: Full-workspace verification + doc note

Confirm the whole workspace builds/tests, no dangling references remain, and document config as the single source of truth.

**Files:**
- Modify: `docs/pool-membership-design-v0.1.md` (add a short note)

- [ ] **Step 1: Grep for dangling references**

Run:
```bash
grep -rn "tape_pool_memberships\|TapePoolMembershipConfig\|TapePoolMembershipProjectionInput\|reconcile_tape_pool_projection\b" crates/ --include=*.rs | grep -v "_from_rules"
```
Expected: no matches (all removed). If any remain, fix them.

- [ ] **Step 2: Full workspace gates**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test
```
Expected: all pass.

- [ ] **Step 3: Add the doc note**

Append to `docs/pool-membership-design-v0.1.md` (e.g. under *Implementation sketch* or as a closing note):

```markdown
## Storage (v0.1 storage refinement)

Membership is the single config-driven projection of `[[tape_pool_rules]]` and is
stored as the `tapes.pool_id` column (not a separate table). The static
`[[tape_pool_memberships]]` input path has been removed: config `[[tape_pool_rules]]`
is the single source of truth for membership. `bringup/rem-init.sh`'s config
generation is normal provisioning, not a workaround. See
`docs/pool-membership-storage-design-v0.1.md`.
```

- [ ] **Step 4: Commit**

```bash
git add docs/pool-membership-design-v0.1.md
git commit -m "Document config as single source of truth for pool membership"
```

---

### Task 8: End-to-end verification on the QuadStor fixture (manual)

This requires the akash hardware fixture and is a manual smoke test, not an automated step. Run it once after Task 7.

- [ ] **Step 1: Rebuild and re-apply capability**

The dev binary lives at `/home/user/remanence-phase2/target/release/rem` per `bringup/rem-init.sh`. Build it and re-apply `cap_sys_rawio` (xattrs don't survive `cargo build`):

```bash
cargo build --release -p remanence-cli
sudo setcap cap_sys_rawio+ep target/release/rem
```

- [ ] **Step 2: Run bringup against a fresh state space**

Run `bringup/rem-init.sh` (it generates `config.toml` with `[[tape_pool_rules]] prefix = "RMN"` → `scenario-a` and runs `rem tape init RMN001L9`). If an existing dev DB is present, this exercises the in-place migration (Task 4 backfill + drop); to exercise a fresh DB instead, point `REM_STATE` at a clean directory.

- [ ] **Step 3: Confirm derived membership after restart**

Query the catalog and confirm `RMN001L9` shows `pool=scenario-a`:

```bash
rem catalog tapes --pool scenario-a --config /var/lib/replica/rem/config.toml
```
Expected: the `RMN001L9` row with `pool=scenario-a`. (Restart the daemon / re-open the index first if the deployment runs a long-lived daemon, to confirm the column survives reopen.)

- [ ] **Step 4: Record the result in the journal**

Per project practice, append a dated entry to the journal noting the migration ran clean (or recreate path used) and the end-to-end check passed.

---

## Self-Review

**Spec coverage** (against `docs/pool-membership-storage-design-v0.1.md`):
- Single model / remove static reconciler + config input → Tasks 5, 6. ✓
- Empty-rules-with-pools startup warning → Task 6 Step 1. ✓
- `tapes.pool_id` column, no FK, partial index → Task 1. ✓
- Drop `assigned_at_utc`, no replacement → Task 2 Steps 3, 6. ✓
- Reconciler writes column / clears stale → Task 2 Step 5. ✓
- `query_memberships_tx`, `get_tape_pool_membership` → Task 2 Steps 4, 7. ✓
- `list_tapes`/`get_tape`/`get_tape_by_voltag` + commit snapshot → Task 3. ✓
- Drop table from `MINIMUM_SCHEMA` + `MINIMUM_TABLES`, backfill, `SCHEMA_VERSION` 5→6 → Tasks 1, 4. ✓
- Keep prefix model, `tape_pools`, `object_copies.pool_id`, identity logic → untouched (verified by grep in Task 7). ✓
- Doc note → Task 7 Step 3. ✓
- End-to-end fixture check → Task 8. ✓
- Out of scope (relabel, lifecycle) → not in plan, by design. ✓

**Type consistency:** `project_tape_pool_membership_tx` drops `assigned_at_utc` in its definition (Task 2 Step 3) and all three call sites — reconciler loop (Task 2 Step 5), public wrapper (Task 2 Step 6) — match the 3-arg signature `(tx, tape_uuid, pool_id)`. `reconcile_tape_pool_projection_from_rules` keeps its signature. `TapeRecord` pool field referenced as `pool_id` in Task 3 (flagged to confirm against `tape_from_row`).

**Verified against code:** `TapeRecord.pool_id` is the real field name (index.rs:366), so Task 3's test is correct as written. `CatalogIndex` has no public connection accessor; Task 1/Task 4 tests use the established raw-`Connection::open` pattern (index.rs:4387). `SCHEMA_VERSION` is currently 5 (index.rs:22). `object_copies.pool_id` (plain `text`, partial index `object_copies_pool_idx`) is the precedent Task 1 mirrors.

**Placeholder scan:** no TBD/TODO; every code step shows full code; commands have expected output. The remaining "match the neighboring test module's imports" notes are explicit verification instructions, not deferred work.
