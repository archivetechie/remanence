# Pool Membership Storage: Single-Model + Column Projection v0.1

Status: design decision. Refines `docs/pool-membership-design-v0.1.md` (the
barcode-prefix model) on two points: (1) the barcode-prefix rule model becomes
the **single** config-driven path — the per-tape static-row input is removed; and
(2) derived membership is stored as a **`tapes.pool_id` column**, not a separate
`tape_pool_memberships` projection table. Consumes/relates to
`docs/pool-tape-selection-design-v0.1.md` (selection reads membership) and
`docs/spec-v0.4.md` §11.4.2.

## Background

`pool-membership-design-v0.1.md` chose barcode-prefix rules as the way a tape's
pool is decided: `pool_id = derive_tape_pool_from_voltag(voltag, rules)`,
longest-prefix-wins, recomputed at reconcile. That model is implemented
(`config::TapePoolRuleConfig`, `derive_tape_pool_from_voltag`,
`index::reconcile_tape_pool_projection_from_rules`, the `tape_init` identity
logic). `rem tape init` already derives pool from the voltag with no `--pool`
flag, and `bringup/rem-init.sh` already provisions via a `[[tape_pool_rules]]`
rule. The originally-reported "pool membership isn't CLI-persistable" gap is
therefore closed by what shipped.

Two residual issues remain inside the codebase, and this doc resolves them.

## Decision

### 1. The barcode-prefix model is the single config-driven path

Remove the per-tape static-row **input** path (`[[tape_pool_memberships]]` config
rows + `index::reconcile_tape_pool_projection`). That model is listed under
*Rejected alternatives* in v0.1, yet it still exists and `state.rs` falls back to
it whenever `[[tape_pool_rules]]` is empty. The fallback is a footgun: an operator
who forgets a rule silently gets the rejected model instead of the v0.1 intent
("unmatched voltag ⇒ no pool ⇒ safe, visible failure"). The prefix model already
subsumes the only legitimate static use (an exact-voltag assignment is a
full-voltag prefix as a longest-match special case).

**Behaviour change.** With the fallback gone, an empty `[[tape_pool_rules]]`
yields zero derived memberships, so no tape is pool-eligible. This *is* the
fail-loud intent. To catch the likely-misconfiguration case, the daemon emits a
**startup warning** when `[[tape_pools]]` are defined but `[[tape_pool_rules]]` is
empty. It is **not** a hard error — a pool may legitimately have no labeled tapes
yet.

### 2. Derived membership is a `tapes.pool_id` column, not a table

Membership is a pure function of `(voltag, rules)`, and the voltag already lives
on the `tapes` row. The `tape_pool_memberships` table is therefore materialized
derived state whose only justification was query ergonomics and keeping
longest-prefix resolution out of SQL. The relationship is strictly
**1-to-(0 or 1)** — one barcode, one longest-matching prefix, one pool — so a
separate table over-normalizes a relationship that a nullable column expresses
directly.

Store the derived pool as a nullable `tapes.pool_id` column
(`foreign key → tape_pools(pool_id)`). Reconcile recomputes it from
`voltag × rules` and writes it onto the tape row. Selection becomes
`… where tapes.pool_id = ?` — still indexed, no join, correctly 1:1, and a natural
home alongside the committed-copy safety check. This keeps the read-path
ergonomics that motivated the table while dropping the table itself.

(The fully-derive-at-read-time alternative — store nothing, filter via
`derive_tape_pool_from_voltag` in Rust on every query — was considered and
rejected: it pushes config (`rules`) threading into every consumer and moves pool
filtering out of the SQL index, for a benefit that only matters if reconcile is
buggy.)

## Schema changes

- **Add** to `tapes`: `pool_id text` (nullable), **no foreign key** — mirrors the
  existing `object_copies.pool_id` precedent (also plain `text`). A FK would force
  the reconciler to null `tapes.pool_id` before deleting stale `tape_pools` rows;
  the reconciler already validates derived pool ids in-app, so the FK is redundant
  and the ordering hazard is avoided. Add a partial index
  `tapes_pool_idx on tapes(pool_id) where pool_id is not null` (mirrors
  `object_copies_pool_idx`).
- **Do not** carry an assignment timestamp. The old table's `assigned_at_utc` is
  write-only — never SELECTed anywhere — so it is dead data. No `pool_assigned_at_utc`
  column is added; `project_tape_pool_membership_tx` drops its `assigned_at_utc`
  parameter.
- **Drop** the `tape_pool_memberships` table and its `tape_pool_memberships_pool_idx`
  index.

## Code changes

### Remove (static-row input path)

- `config.rs`: `tape_pool_memberships` field, `TapePoolMembershipConfig` struct,
  its validation loop (the `for membership in &config.tape_pool_memberships` block),
  and `[[tape_pool_memberships]]` test fixtures.
- `index.rs`: `reconcile_tape_pool_projection` (the static reconciler) and its
  input type `TapePoolMembershipProjectionInput`; fix the stale doc comment at
  `index.rs:506` (provisioning no longer references `tape_pool_memberships`).
- `state.rs`: replace the `if config.tape_pool_rules.is_empty() { … } else { … }`
  (around 351-368) with an unconditional `reconcile_tape_pool_projection_from_rules`;
  drop the membership-mapping block; retire/replace the
  `reopen_reconciles_changed_config_tape_pool_membership` test.
- `lib.rs`: drop the `TapePoolMembershipConfig` and `TapePoolMembershipProjectionInput`
  re-exports.

### Rewrite (table → column)

- `reconcile_tape_pool_projection_from_rules`: instead of delete/insert into
  `tape_pool_memberships`, `update tapes set pool_id = ?, pool_assigned_at_utc = ?`
  for each derived membership, and clear (`set pool_id = null`) tapes whose voltag
  now matches no rule. Keep the committed-copy foreign-pool safety check.
- `project_tape_pool_membership_tx` (index.rs:2823) → `update tapes set pool_id = ?,
  pool_assigned_at_utc = ? where tape_uuid = ?`. Confirmed callers: the reconciler
  (index.rs:824, 917) in production, and the public `project_tape_pool_membership`
  wrapper (index.rs:931) used **only** by `#[cfg(test)]` test helpers
  (`lib.rs:2194/2243/2392`, `index.rs:4490/4625/4768`) — no non-reconcile production
  caller exists. Keep the public method as a test seam, retargeted to `tapes.pool_id`.
- `query_memberships_tx` → `select tape_uuid, pool_id from tapes where pool_id is not null`.
- `get_tape_pool_membership` (index.rs:1006) → `select pool_id from tapes where tape_uuid = ?1`.
- `list_tapes` (index.rs:1187-1299): drop the `left join tape_pool_memberships`;
  select `tapes.pool_id` directly; the `where … pool_id = ?1` clause moves onto
  `tapes.pool_id`.
- Commit snapshot (index.rs:2948): the correlated subquery becomes
  `(select pool_id from tapes where tape_uuid = ?2)`.
- Test schema invariant `MINIMUM_TABLES` (index.rs:3593, `#[cfg(test)]`): drop the
  `"tape_pool_memberships"` entry. This is the only place that lists the table for
  reset/inventory purposes — it is a test assertion, **not** a runtime reset. The
  sole runtime reset (index.rs:634-638) clears `idempotency_keys`/`operations`/
  `sessions` only and never touched membership, so no runtime membership-wipe path
  needs changing.

### Keep

- The barcode-prefix model: `TapePoolRuleConfig`, `derive_tape_pool_from_voltag`,
  rule validation, `reconcile_tape_pool_projection_from_rules` (now column-writing).
- The `tape_pools` definition table and `tape_init` identity/decision logic.
- `object_copies.pool_id` — the per-copy pool snapshot at commit time (historical
  record), unchanged in meaning.

## Migration

The index has a real migration mechanism: `migrate()` (index.rs:3333) applies
`MINIMUM_SCHEMA`, runs idempotent `ensure_column()` add-column steps, creates
indexes, then bumps `PRAGMA user_version` to `SCHEMA_VERSION` (currently `5`). Use
it — **in-place migration**, not a DB recreate, so akash's existing catalog is
handled:

1. Add `pool_id` to the `tapes` block in `MINIMUM_SCHEMA` (fresh DBs) **and** call
   `ensure_column(conn, "tapes", "pool_id", "pool_id text")` (existing DBs — the
   `create table if not exists` won't alter an existing `tapes`).
2. Backfill, guarded by table existence:
   `update tapes set pool_id = (select pool_id from tape_pool_memberships where
   tape_pool_memberships.tape_uuid = tapes.tape_uuid)`.
3. `drop table if exists tape_pool_memberships`; remove its `create` + index from
   `MINIMUM_SCHEMA`.
4. Add the `tapes_pool_idx` partial index.
5. Bump `SCHEMA_VERSION` `5 → 6`.

## Verification

- Unit: `reconcile_tape_pool_projection_from_rules` writes `tapes.pool_id`; a
  reopen test asserts derived membership survives daemon restart and that removing
  a rule clears the affected `tapes.pool_id`. `list_tapes(pool_id)` returns the
  right tapes via the column. Commit snapshot reads pool from `tapes.pool_id`.
- Config: an empty `[[tape_pool_rules]]` with non-empty `[[tape_pools]]` loads
  (no hard error) and produces the startup warning.
- End-to-end on the QuadStor fixture: run `bringup/rem-init.sh` (`RMN` prefix →
  `scenario-a`), confirm a catalog/pool query shows `RMN001L9` in `scenario-a`
  after a daemon restart.
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo check`, `cargo test`.

## Documentation

- Add a short note to `pool-membership-design-v0.1.md` (or a pointer here) stating
  config `[[tape_pool_rules]]` is the single source of truth for membership, stored
  as `tapes.pool_id`; `rem-init.sh` config-generation is normal provisioning, not a
  workaround.
- Update any config/README reference that documents `[[tape_pool_memberships]]`.

## Out of scope — tracked fast-follow

`rem tape relabel` (audited damaged-label / reassignment) and durable
`available/assigned/retired` barcode-lifecycle enforcement remain unbuilt. v0.1
flags relabel as a fast-follow (its open question #5); the realistic near-term
trigger is a physically damaged label. Build when that arises or before
production — not part of this slice.

## Resolved decisions

All previously-open questions are now decided (see *Schema changes* and
*Migration*):

1. **No assignment timestamp** — `assigned_at_utc` is write-only/dead, so it is
   dropped with the table and not replaced.
2. **In-place migration** via `migrate()`/`ensure_column` + backfill + drop +
   `SCHEMA_VERSION` bump (5 → 6), not a DB recreate.
3. **No non-reconcile production caller** of `project_tape_pool_membership(_tx)`
   exists — every non-reconcile caller is a `#[cfg(test)]` test helper. The column
   model changes no production write path beyond the reconciler.
