# Tape identity lifecycle (retire / rebind) — Design v0.1

**Status:** draft for review (2026-06-10).
**Problem source:** `docs/tape-recycle-identity-reconciliation-concern.md`
(2026-06-09 steering-harness report, severity medium-high).
**Companion explainer:** `docs/tape-identity-lifecycle-explainer.md` —
the why, with worked scenarios; this document is the what.
**Review context:** code-review-2026-06-10.md findings H2 (rebuild vs
provisioning state), the `reset_catalog` blast-radius finding, and the
tape-init gauntlet notes.

**Design verification (rust-design-verification skill):** verified
against `cargo check --workspace --all-features` and
`cargo clippy -p remanence-state -p remanence-api -p remanence-cli
--all-targets -- -D warnings` on 2026-06-10 (clean). The skeleton
stubbed: `RetireTapeInput`/`RetireTapeOutcome`/`TapeRetireTarget`,
`CatalogIndex::retire_tape`, `CatalogIndex::
list_objects_with_no_committed_copies`, `StateHandle::retire_tape`, the
two `AuditEvent` variants with their serialization arms, the
`CatalogRowDisposition` enum, and the clap `TapeRetireArgs`. Skeleton
deleted after verification (design-only). Findings: (a)
`CatalogIndex::get_tape_by_voltag` already exists (index.rs:1238) —
reuse it; (b) the `AuditEvent` additions are additive-safe — only the
two serialization matches in audit.rs needed arms; no projection or
adapter match broke, confirming replay treats the new events as inert.
Five-category notes: Cat 1 — `retire_tape`'s real body must live in
`index.rs` (same module as the private `conn`), like every sibling
mutation; the skeleton verified the API surface only. Cat 2/3 — no new
threads, async, or reactor-bound types anywhere in this slice. Cat 4 —
no new borrow-holding structs. Cat 5 — `CatalogIndex` and `StateHandle`
have pub constructors already used by the CLI; no visibility widening
required.

---

## 1. Problem

The catalog binds **barcode (voltag) → tape UUID** and the write path
verifies the medium's BOT-bootstrap UUID against the catalog before
writing (`verify_tape_identity`, pool_write.rs:657-677). Both behaviors
are correct. But when a medium is *legitimately* replaced or cleared —
VTL rebuild, recycle script, future self-heal after a lost copy — there
is no operation that ends the old identity's life:

- the write session fails `tape identity mismatch: expected X, found Y`;
- `rem tape init` classifies the divergence as `Anomaly`
  (`BarcodeAssignedToDifferentUuid` / `MediaSwapReusedBarcode`,
  tape_init.rs:478-537), and **Anomaly never writes** — there is no
  override flag for it by design (`maybe_write_tape_init_bootstrap`,
  tape_init.rs:426-470);
- the only recovery is `rem-debug catalog reset`, which destroys the
  authoritative audit history and 3c journals to fix one row.

The vocabulary for the fix already half-exists and is explicitly
deferred: `BarcodeLifecycleState::Retired` and
`TapeInitError::BarcodeRetired` are defined but never produced
("retired barcode persistence is deferred", tape_init.rs:339-341).
This slice completes that vocabulary.

## 2. Design overview

One new terminal tape state, one new copy status, one new audited
operation, one new tape-init decision arm, rebuild preservation, and a
CLI surface:

```text
rem tape retire RMJ101L9 --reason recycled \
    --i-understand-copies-become-unreadable
        │
        ▼  (one transaction + one audit append)
tapes[X]:  state='retired', voltag=NULL          ── terminal, permanent
object_copies[*,X]: status 'committed'→'missing' ── derived consequence
audit:     TapeRetired { voltag, reason, copies_marked_missing }
        │
        ▼
rem tape init RMJ101L9                            ── barcode is free again
  BOT=ours(X), row(X) retired  → FreshInit (new uuid Z, no clobber rite)
  BOT=blank,   barcode free    → FreshInit (existing path, now reachable)
```

Naming: **`retire`**, not "reconcile" — `ReconcileTape` already exists
and means "rebuild filemark geometry from the physical tape"
(write_owner.rs:1391-1599); this operation is about identity, not
structure, and never touches hardware.

## 3. State model (D1)

### 3.1 `tapes.state` gains `'retired'`

Complete state set becomes: `ingestion_pending`, `ingested`, `ready`,
`sealed`, `retired`. Properties of `retired`:

- **Terminal.** No code path leaves it. `provision_tape` targeting a
  retired row MUST refuse with a `TapeProvisionConflict`-style error
  ("tape {hex} is retired; retired identities are permanent — init the
  medium under a fresh identity"), **including with `force`** — the
  `reprovision_tape_tx` escape hatch (index.rs:2034-2084) must check
  state first. Rationale: `force` reuses the *row*; a retired identity's
  history (its journals, its audit trail) must stay attached to its
  uuid forever.
- **Unwritable automatically**: `check_writability_preconditions`
  already rejects any `state != "ready"` as `NotReady` — no change.
- **`voltag` is detached** in the same transaction (the partial unique
  index `tapes_voltag_unique` makes detach-before-rebind mandatory).
  The released voltag is recorded in the audit detail, not in a column.
- **`pool_id` is kept** as history (selection gates on state, so a
  retired row in a pool is harmless and the provenance is useful).
- Proto mapping: `tape_state()` currently maps unknown states to
  `TAPE_STATE_UNSPECIFIED`; this slice maps `retired` explicitly once a
  proto enum value exists — until then UNSPECIFIED is acceptable and
  noted (the review separately flagged that `ready`/`sealed` are also
  unmapped; fix together).

### 3.2 `object_copies.status` gains `'missing'`

Today the only written value is `'committed'`. Retire transitions every
`committed` copy on the tape to `'missing'`. The existing proto health
mapping already renders non-committed as `OBJECT_COPY_HEALTH_SUSPECT`
(lib.rs:1943-1948) — no proto change in this slice.

**Invariant: copy status is derived from tape state.** A copy on a
retired tape is `missing`, always; this is enforced at retire time for
the live catalog and *re-derived* after rebuild (§6). No copy-level
preservation machinery is needed.

## 4. The retire operation (D2)

Verified signatures (remanence-state):

```rust
pub struct RetireTapeInput {
    pub tape_uuid: [u8; 16],
    pub reason: String,          // "recycled" | "vtl-rebuilt" | free text
}

pub struct RetireTapeOutcome {
    pub newly_retired: bool,             // false = idempotent no-op
    pub released_voltag: Option<String>,
    pub copies_marked_missing: u64,
}

impl CatalogIndex {
    pub fn retire_tape(&mut self, input: RetireTapeInput)
        -> Result<RetireTapeOutcome, StateError>;
    pub fn list_objects_with_no_committed_copies(&self)
        -> Result<Vec<String>, StateError>;
}

impl StateHandle {
    pub fn retire_tape(&mut self, input: RetireTapeInput)
        -> Result<RetireTapeOutcome, StateError>;
}
```

`CatalogIndex::retire_tape` is one transaction in `index.rs` (Cat 1:
needs the private `conn`, lives beside the sibling mutations):

1. Look up the row by uuid; unknown → error (mirror `seal_tape`'s
   "cannot seal unknown tape" shape).
2. `state == 'retired'` → return `{ newly_retired: false,
   released_voltag: None, copies_marked_missing: 0 }`. **Idempotent**:
   recycle scripts re-run safely.
3. Otherwise: `update tapes set state='retired', voltag=null,
   updated_at_utc=? where tape_uuid=?`, then `update object_copies set
   status='missing' where tape_uuid=? and status='committed'`
   (capturing the change count); return the outcome.

`StateHandle::retire_tape` wraps the index call and appends the
`TapeRetired` audit event after the transaction commits: subject
`{ kind: "tape", id: <uuid hex> }`, detail
`{ voltag, reason, copies_marked_missing }`, actor `User(<login>)`
from the CLI path. Ordering note (record in code): catalog first,
audit second; a failed audit append surfaces as an error but does not
roll back the retire — the same crash window exists for every audited
mutation in the codebase today, and audit-before-commit would invert
the lie (an audit record for a retire that never happened).

`list_objects_with_no_committed_copies` is the Scenario-Q hook: object
ids having at least one copy row but none with `status='committed'`.
Query only in this slice; surfacing (CLI/RPC) is OUT.

## 5. Audit events (D3)

Two additive `AuditEvent` variants (audit.rs:107 region + the
serialization arms at the `as_str`/`from_str` matches — compile-check
confirmed nothing else matches exhaustively on the enum):

- **`TapeRetired`** — emitted by `StateHandle::retire_tape` (§4).
- **`TapeProvisioned`** — emitted by the tape-init success path
  (`provision_initialized_tape`, cli lib.rs:3580-3589) with detail
  `{ voltag, block_size, geometry, forced: bool }`. Closes the
  review-noted gap that tape init emits no audit record at all.

Replay impact: none required — `replay_audit_records` and
`project_audit_record` project operations/sessions/idempotency only and
ignore other events; durability of the retired state rides §6, and the
events exist as the tamper-evident *record* of who declared the medium
dead, when, and why. A future H2-completion slice may additionally
project these events; nothing here precludes that.

## 6. Rebuild preservation — the resurrection trap (D4)

The 3c journal of the retired tape remains on disk (it is authoritative
history: "these objects were committed to identity X"). Without
changes, `rebuild_from_authoritative_sources` (index.rs:720-767) would
re-ingest that journal and resurrect X as `state='ingested'` with
copies `'committed'` — silently un-retiring it, because:

- `query_preserved_tape_rows_tx` (index.rs:2104-2117) preserves rows
  with a voltag/pool_id **or** `state in ('ready','sealed')` — a
  retired row has no voltag and the wrong state;
- the journal-ingest state CASEs preserve only `'sealed'`
  (index.rs:2247, 2458).

Four precise changes:

1. **Preserve predicate**: add `or state = 'retired'` to
   `query_preserved_tape_rows_tx`.
2. **Merge**: `merge_preserved_tape_operator_columns_tx`
   (index.rs:2182-2213) re-applies `state='retired'` over the
   journal-derived row (as it does for ready/sealed today).
3. **Ingest CASEs**: `index_committed_tape_journal_tx` (:2235) and
   `project_committed_tape_file_bundle_tx` (:2429) extend their
   sealed-preserving CASE to `state in ('sealed','retired')`. (The
   bundle path cannot legitimately fire for a retired tape — selection
   blocks it — defense in depth.)
4. **Post-merge derivation**: after journal replay + merge, one pass:
   `update object_copies set status='missing' where tape_uuid in
   (select tape_uuid from tapes where state='retired') and
   status='committed'`. This is the §3.2 invariant re-derived; copy
   statuses need no snapshot of their own.

Regression test (acceptance §10.2) pins all four.

## 7. Tape-init decision changes (D5)

Inputs: the catalog row resolved for a BOT uuid gains a disposition
(verified skeleton shape):

```rust
pub enum CatalogRowDisposition { Active, Retired }
```

plumbed through `project_tape_init_catalog_inputs`
(tape_init.rs:333-386). Decision-table changes:

| BOT | Catalog row for BOT uuid | Barcode lifecycle | Today | New |
| --- | --- | --- | --- | --- |
| ours(X) | X **retired** | Available (freed by retire) | `Anomaly { MissingCatalogRow-ish }` / `RefuseClobber { PhysicalDataPastBootstrap }` | **`FreshInit`** — new v4 uuid, normal provision + pool projection; the data-past-bootstrap probe is **skipped** (the retire ceremony already declared the data dead; demanding `CLOBBER` again is double ceremony for the same intent) |
| ours(X) | X retired | Assigned to *different* uuid W | `Anomaly` | `Anomaly` — unchanged; the barcode belongs to a live identity |
| blank | (barcode freed by retire) | Available | `FreshInit` | unchanged — this case starts working simply because retire freed the barcode |
| ours(Y) | no row for Y | Available | `MissingCatalogRow` mapping | unchanged — a rem bootstrap the catalog has never seen is still guarded (`--force` path exists); retire does not whitelist foreign tapes |

Also: `provision_tape` refuses retired rows (§3.1), and
`BarcodeLifecycleState::Retired` / `TapeInitError::BarcodeRetired`
**remain unproduced** — barcode decommissioning (a destroyed/lost
cartridge whose *label* must never be reused) is a separate, smaller
follow-up enabled by this slice, not part of it.

## 8. CLI surface (D6)

`rem tape retire` beside `rem tape init` (the recycle workflow runs
them back-to-back; retire touches catalog + audit only — no SCSI, so
no library allowlist involvement). Verified clap shape:

```text
rem tape retire <TARGET> --reason <TEXT> \
    --i-understand-copies-become-unreadable \
    [--config /etc/rem/config.toml] [--dry-run]
```

- **Target resolution**: try `get_tape_by_voltag` first; if no row,
  parse as 32-hex tape uuid. (LTO voltags are 6–8 chars; no collision
  space with 32-hex.)
- **Acknowledgement flag is required** (exit 1 without it — per the
  review's exit-code finding, not 2): retiring declares contents
  permanently unreadable; this is the same friction tier as the other
  destructive gates.
- `--dry-run` reports what would be retired (voltag, copy count, the
  objects that would lose their last committed copy) without writing.
- Output: one human line
  (`retired RMJ101L9 (uuid 5e8f…): 3 copies marked missing, 1 object now degraded`)
  and a `rem.tape.retire.v1` JSON envelope under `--json`, including
  the idempotent-rerun shape (`newly_retired: false`).
- **Locking**: `StateHandle::open` takes the state flock; with a
  running daemon the command fails cleanly. The recycle scripts already
  stop the daemon first; document the constraint in `--help`.

## 9. Script + operational changes (D7)

- `recycle-scenario-*-tapes.sh`: insert
  `rem tape retire $BARCODE --reason recycled --i-understand-…` before
  `rem tape init`; delete any `catalog reset` dependence. Back-to-back
  scenario reruns then need no reset.
- Make the recycle scripts restart the daemon they stop (the concern
  doc's "daemon outage" note was clean stops from asymmetric tooling).
- Small follow-up (not this slice's code, just confirm): whether a
  systemd stop during an in-flight tape op exits non-zero (the one
  `failed`-unit observation).

## 10. Acceptance criteria

1. **Unit — retire**: happy path sets state/voltag/copy statuses and
   reports counts; re-retire is an idempotent success with
   `newly_retired: false`; unknown uuid errors; `provision_tape` (with
   and without `force`) refuses a retired row; writability rejects
   `retired` as `NotReady`.
2. **Unit — rebuild regression (the resurrection trap)**: catalog with
   a retired tape whose 3c journal exists → `startup_replay` /
   `rebuild_from_authoritative_sources` → row still `retired`, voltag
   still NULL, copies still `missing`. This test MUST fail if any of
   the four §6 changes is reverted.
3. **Unit — tape-init decisions**: retired-row + ours-BOT + available
   barcode → `FreshInit` *with physical data present* (no clobber
   demanded); retired-row + barcode assigned elsewhere → `Anomaly`;
   foreign ours-BOT with no row → unchanged mapping.
4. **Unit — audit**: retire appends `TapeRetired` with the documented
   subject/detail; init success appends `TapeProvisioned`; audit
   replay of a log containing the new events still rebuilds
   operations/sessions correctly (inertness pin).
5. **CLI**: parse tests; missing ack-flag → exit 1, no catalog change;
   dry-run mutates nothing; JSON envelope shape pinned; voltag and
   uuid targeting both covered.
6. **Integration (VecBlockSource; VTL-gated variant optional)**: the
   concern doc's repro — init → write+commit → retire → re-init same
   barcode → write+commit — green with **no** `catalog reset`, and the
   first object's copies read back as `missing` while the second
   object's read back `committed`.
7. `cargo fmt --all` and
   `cargo clippy --workspace --all-targets -- -D warnings` clean;
   full test suite green.

## 11. Scope

**IN:** §3–§9 plus the §10 tests: the `retired` state, `missing` copy
status, `retire_tape` (index + StateHandle + audit), the two audit
events, the four rebuild-preservation changes, the tape-init
disposition arm and `provision_tape` gate, `rem tape retire`, the
degraded-objects query, recycle-script updates.

**OUT (recorded so they aren't accidentally absorbed):** barcode
decommissioning (producing `BarcodeLifecycleState::Retired`); un-retire
(deliberately impossible; a wrongly retired tape is re-ingested under a
*new* identity via reconcile/rebuild flows); a daemon gRPC retire RPC
(scripts use the CLI; the daemon-side surface belongs with the
idempotency/authz work); surfacing the degraded-objects query in
CLI/RPC; `ObjectCopyHealth` proto additions; self-heal itself; the
no-parity append redesign (separate roadmap TODO).

## 12. Open questions for review

1. Should `--reason` be a closed enum (`recycled|vtl-rebuilt|destroyed|
   other:<text>`) instead of free text? Free text proposed for v0.1;
   the audit detail is CBOR either way.
2. Should retire warn (or refuse without an extra flag) when it would
   create degraded objects — i.e. copies that are the *last* committed
   copy of an object? Current proposal: warn in output + dry-run shows
   it; no extra gate (self-heal scenarios retire precisely such tapes
   on purpose).
3. `TapeProvisioned` on every init vs only on state-changing init
   (skip `IdempotentNoOp`)? Proposed: only when a bootstrap was
   actually written (`action == WroteBootstrap`).
