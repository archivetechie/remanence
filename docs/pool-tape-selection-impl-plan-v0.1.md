# Pool Tape-Selection — Implementation Plan v0.1

Status: implementation plan for `docs/pool-tape-selection-design-v0.1.md`.
Authored by Codex (GPT-5.x, `codex` CLI, planner role) on 2026-05-29 in
read-only mode, retrieved via PAL `clink` and recorded here verbatim. The design
is final and Gemini-reviewed; this plans its implementation, it does not redesign
it.

## Summary

Implement the design as a sequence of dependent changes, with the write-session
data model first. Phase 1 changes Layer 5 and Layer 4 from a single session tape
to an ordered tape sequence, adds current-tape and pool-targeted session state,
records object-to-tape placement, and rebuilds recoverable orphan reservations
before new opens. Phase 2 adds per-pool selection config and validates
watermarks, policy names, byte-size parsing, and the watermark-band invariant
once tape capacity is available. Phase 3 fills in `pool_selection.rs` and wires
`select_tape_in_pool` through the existing integration point, keeping Tier-0
stickiness in session rollover. Phase 4 seals from actual post-write
BodyPosition/EOM state and adds a force-seal path. Phase 5 wires initial and
rollover drive/mount choice. Phase 6 hardens reservations for concurrent
sessions. Phase 7 adds explicit errors and gRPC mappings. Phase 8 adds policy,
property, and integration tests. Main risks are merge conflicts with concurrent
write-engine/state work, missing tape-capacity modeling, proto compatibility
around `WriteSession.tape_uuid`, and force-seal semantics requiring a
Sutradhara/operator signal.

---

## Phase 1 - Spanning Write-Session Data Model First

Design refs: §5, §7. Dependencies: none; this must be the first implementation change after the concurrent write-engine work is merged or rebased.

Files/modules touched: `proto/layer5.proto`, `crates/remanence-api/src/lib.rs`, `crates/remanence-state/src/index.rs`, `crates/remanence-state/src/state.rs`, `crates/remanence-state/src/audit.rs`, `crates/remanence-state/src/lib.rs`, `docs/spec-v0.4.md`.

Key changes:
- Replace internal `WriteSessionState.tape_uuid: Vec<u8>` with a pool-aware target model: `WriteSessionTarget::PinnedTape { tape_uuid, pool_id }` and `WriteSessionTarget::Pool { pool_id, tape_sequence, current_tape_index }`.
- Keep pinned `DriveTarget` / `TapeTarget` sessions semantically unchanged: one tape, tape-full ends the session.
- For `TapePoolTarget`, persist an ordered tape sequence and object-to-tape placements, not just the current tape.
- Extend the generated `WriteSession` protobuf surface compatibly. Keep existing `tape_uuid = 2` if wire compatibility matters, but add a repeated tape sequence field such as `repeated bytes tape_sequence = <new tag>` plus a target-kind/current-index field. Do not reuse tag 2 for a repeated field.
- Add a state projection/migration path so old one-tape sessions hydrate as a one-element sequence.
- Add durable journal/audit records for: session opened against pool, tape appended to session sequence, object committed to tape, session rolled over, session orphaned/resumed/closed.
- Add daemon-local reservation state keyed by tape UUID and session ID. It should reserve the current tape for every live pool session and every recoverable orphaned pool session.
- During daemon bootstrap, rebuild reservations for recoverable sessions from durable state before accepting `OpenWriteSession`.

Suggested internal types/signatures:
```rust
pub enum WriteSessionTarget {
    PinnedTape {
        tape_uuid: TapeUuid,
        pool_id: Option<String>,
    },
    Pool {
        pool_id: String,
        tapes: Vec<SessionTape>,
        current_tape_index: Option<usize>,
    },
}

pub struct SessionTape {
    pub tape_uuid: TapeUuid,
    pub first_object_index: Option<u64>,
    pub last_object_index: Option<u64>,
    pub sealed_at_rollover: bool,
}

pub struct TapeReservation {
    pub tape_uuid: TapeUuid,
    pub session_id: String,
    pub reason: ReservationReason,
}
```

Validation gate:
- Unit test old single-tape session hydration.
- Unit test pool session sequence append/current lookup.
- Unit test bootstrap reservation rebuild reserves tapes for orphaned recoverable sessions before a new session can select them.
- Run proto generation and workspace compile only after the concurrent edits are settled.

Risks/mitigations:
- Risk: proto compatibility break if `WriteSession.tape_uuid` is changed in place. Mitigation: leave it as legacy/current-tape projection and add new fields.
- Risk: state schema churn collides with concurrent edits in `index.rs` / `state.rs`. Mitigation: land this after those changes, or isolate migration helpers in a new module with small call-site patches.
- Risk: session recovery can reopen a tape that a new session has already selected. Mitigation: make reservation rebuild part of startup reconciliation and gate `OpenWriteSession` until it completes.

Ambiguities needing implementation decision:
- Exact protobuf field names/tags for tape sequence and target kind.
- Whether to expose both legacy `tape_uuid` and full `tape_sequence` in `WriteSession`, or only expose the full sequence and reserve legacy for compatibility.

## Phase 2 - Per-Pool Configuration And Validation

Design refs: §6, §11. Dependencies: Phase 1 data model can proceed independently, but selection wiring depends on this.

Files/modules touched: `crates/remanence-state/src/config.rs`, config loader modules, `crates/remanence-api/src/lib.rs` or daemon state construction, `docs/spec-v0.4.md`, example config fixtures/tests.

Key changes:
- Extend `[[tape_pools]]` config with:
```toml
selection_policy = "complete-or-fill"
watermark_low = 0.92
watermark_high = 0.97
min_object_size = "2GiB"
```
- Default `selection_policy` to `complete-or-fill` when omitted.
- Add typed pool config fields after parsing:
```rust
pub struct TapePoolConfig {
    pub pool_id: String,
    pub selection_policy: PoolSelectionPolicyName,
    pub watermark_low: f64,
    pub watermark_high: f64,
    pub min_object_size_bytes: u64,
}
```
- Validate `0 < watermark_low < watermark_high <= 1`.
- Validate `selection_policy` is known: `complete-or-fill` or `fill-oldest` for v1.
- Parse byte-size strings for `min_object_size`, preserving `0` as “no guaranteed floor.”
- Enforce `(watermark_high - watermark_low) * capacity >= min_object_size` once a pool’s usable cartridge capacity is known.
- Decide where capacity comes from. Prefer a catalog/library capacity field already attached to tape media if present; otherwise add a pool/media-class capacity source before making the invariant hard-fail.
- Store resolved policy objects in daemon/shared API state, likely as `Arc<dyn PoolSelectionPolicy>` per pool or policy name.

Validation gate:
- Config tests for valid defaults.
- Config tests for invalid ordering, unknown policy, invalid byte strings, and invariant failure.
- Mixed-capacity pool test if supported: invariant must be checked per tape capacity or against the smallest capacity in the pool.

Risks/mitigations:
- Risk: no durable tape capacity exists at config-load time. Mitigation: split validation into static config validation plus catalog/media validation during daemon startup, and block serving if the resolved pool fails.
- Risk: floating-point boundary errors near exact equality. Mitigation: convert watermarks to byte thresholds with deterministic rounding and validate on bytes, not only raw floats.
- Risk: wide bands are valid but wasteful. Mitigation: do not reject them unless the design adds a max-band rule; optionally warn.

Ambiguities needing implementation decision:
- Capacity source for the invariant.
- Whether invariant failure is always fatal or warning when `min_object_size = 0`.
- Rounding rule for `low_bytes` and `usable_bytes`; choose a conservative rule and use it everywhere. Recommended: floor both thresholds to avoid writing past high, and document exact behavior.

## Phase 3 - Policy Implementations And Selector Integration

Design refs: §4, §10, §12. Dependencies: Phase 2 for configured policy names; Phase 1 for session/rollover integration. The pure policy unit tests can land earlier.

Files/modules touched: `crates/remanence-api/src/pool_selection.rs`, `crates/remanence-api/src/pool_write.rs`, `crates/remanence-api/src/lib.rs`, state/catalog query helpers as needed.

Key changes:
- Replace `todo!()` in `CompleteOrFill::select`.
- Implement Tier 1: choose candidates where `used_bytes + projected_footprint >= low_bytes`, then best fit by smallest `usable_bytes - used_bytes - projected_footprint`, tie-break already-loaded, then lowest `barcode_order`.
- Implement Tier 2: if no Tier 1 candidate, choose an already-loaded candidate if any, otherwise lowest `barcode_order`.
- Implement Tier 3: return `Selection::NeedFreshTape` when no active candidate fits.
- Implement `FillOldest::select` as lowest-barcode first-fit over pre-filtered candidates, with the same deterministic tie handling.
- Add overflow-safe arithmetic helpers for `used + projected` and remaining bytes.
- Make `resolve_policy` public or expose a typed resolver used by config/daemon state.
- Add a projection function from catalog/session/drive facts into `TapeFitState`:
```rust
pub fn project_tape_fit_state(
    tape: &TapeRecord,
    pool_cfg: &ResolvedTapePoolConfig,
    drive_inventory: &DriveInventory,
) -> Result<TapeFitState, SelectTapeError>
```
- Keep candidate filtering outside the policy:
  - active tapes only;
  - same pool;
  - object fits within `usable_bytes`;
  - not reserved by another live/recoverable session;
  - valid UUID/capacity/geometry.
- Keep Tier-0 stickiness outside the policy. Session append flow first checks whether `T_cur` can take the projected object; only if it cannot does it call the configured policy.
- Replace the current `select_tape_in_pool` “exactly one eligible tape” behavior only after the session path can use the policy safely. Until then, preserve the Phase 1 non-hardware path or gate the new selector behind the session implementation.

Suggested signatures:
```rust
pub fn select_tape_in_pool_with_policy(
    state: &CatalogIndex,
    pool_id: &str,
    projected_footprint: u64,
    reservations: &TapeReservationTable,
    drive_inventory: &DriveInventory,
    policy: &dyn PoolSelectionPolicy,
) -> Result<Selection, SelectTapeError>
```

Validation gate:
- Unit tests in `pool_selection.rs`:
  - empty candidates => `NeedFreshTape`;
  - Tier 1 beats Tier 2;
  - Tier 1 best-fit chooses least leftover;
  - already-loaded tie-break wins;
  - barcode tie-break is deterministic;
  - exact `used + P == low` is selected as completing.
- Unit tests for candidate projection and reservation filtering.
- Regression tests for current `AmbiguousNeedsPolicy` behavior can be removed or rewritten only when the new policy is wired.

Risks/mitigations:
- Risk: conflict with in-flight `pool_write.rs` edits. Mitigation: implement pure `pool_selection.rs` first, then integrate through a narrow adapter after the write-engine changes settle.
- Risk: policy accidentally bakes in mount/session concerns. Mitigation: require tests to construct only `TapeFitState` values and assert no catalog/hardware dependencies.
- Risk: overflow in `used + projected`. Mitigation: use checked/saturating helpers and treat overflow as “does not fit” or invalid state.

Ambiguities needing implementation decision:
- `barcode_order` source when firmware/catalog lacks clean ordering.
- Whether `FillOldest` should prefer already-loaded on ties or strictly barcode-only. The design says Tier 2 prefers already-loaded among equally good candidates; pure d2 fallback can remain barcode-only if that is the intended compatibility behavior.

## Phase 4 - Eager Seal And Force-Seal Valve

Design refs: §4.1, §4.2. Dependencies: Phase 1 session sequence and Phase 3 selection/projection.

Files/modules touched: write engine/session append path in `crates/remanence-api/src/lib.rs` and/or `crates/remanence-api/src/pool_write.rs`, Layer 4 tape state in `crates/remanence-state/src/index.rs` / `state.rs`, audit/journal modules, protobuf status/error fields if seal state is exposed.

Key changes:
- Add explicit tape lifecycle state if not already present: active/open vs sealed/finalized.
- After every successful object commit, compute actual post-write used position from `BodyPosition`, committed bundle/tape-file extents, and hardware EOM/early-warning state.
- Seal immediately when actual `used >= low_bytes` or hardware early-warning fires.
- Do not use policy projection to decide sealing. `Selection` must remain only `UseTape` / `NeedFreshTape`.
- On seal:
  - persist tape sealed/finalized state;
  - release the session’s reservation for that tape only after the session has rolled to the next tape or closed the tape safely;
  - append audit/journal event with actual used position and reason.
- Implement force-seal valve for below-low tapes:
  - operator/API explicit close-out for a tape or pool;
  - optional scheduler-facing path where Sutradhara can request close-out when no pending/incoming object fits.
- Ensure a sealed tape is excluded from future candidate projection and cannot be auto-promoted back to active.
- Preserve object non-spanning: if an object does not fit current tape, rollover happens before writing the object; if it fits by projection but actual write hits hardware EOM unexpectedly, surface a write failure and recover according to existing partial-write rules.

Suggested types:
```rust
pub enum TapeSealReason {
    ReachedLowWatermark,
    HardwareEarlyWarning,
    OperatorCloseOut,
    PoolCloseOut,
    NoPendingObjectFits,
}

pub struct TapePositionAfterWrite {
    pub used_bytes: u64,
    pub early_warning: bool,
}
```

Validation gate:
- Unit test `used == low` seals.
- Unit test `used < low` remains active unless force-sealed.
- Unit test early-warning seals even below low.
- Integration-style test: object under-projected, actual used crosses low, tape still seals.
- Test sealed tapes are excluded from later selection.

Risks/mitigations:
- Risk: actual used calculation differs between parity and no-parity paths. Mitigation: centralize post-write position extraction and test both paths.
- Risk: force-seal API implies Sutradhara scheduling semantics. Mitigation: expose it as an explicit close-out mechanism only; Remanence does not decide job ordering.
- Risk: sealing before reservation release can deadlock rollover logic if the same session needs to select next tape. Mitigation: make rollover state transition explicit: write complete -> maybe seal current -> select/mount next -> release old reservation when no longer current.

Ambiguities needing implementation decision:
- Exact Layer 5 method or admin command for force-seal.
- Whether “no pending/incoming object fits” is represented by Sutradhara explicitly, by operator command, or by a daemon-local pending-object hint.
- Exact persisted tape status vocabulary.

## Phase 5 - Drive And Mount Selection

Design refs: §8. Dependencies: Phase 1 session ownership model; Phase 3 selection result; Phase 4 rollover/seal transition.

Files/modules touched: Layer 5 open/append session handling in `crates/remanence-api/src/lib.rs`, hardware/library abstraction modules, drive inventory projection, `proto/layer5.proto` if drive occupancy is exposed.

Key changes:
- At initial `OpenWriteSession(TapePoolTarget)`:
  - select the tape using pool policy;
  - if selected tape is already loaded in a drive, bind the session to that drive;
  - otherwise mount selected tape into any free drive;
  - if no free drive exists, fail before writing any data.
- At rollover:
  - reuse the session’s held drive by unloading current tape and loading the next tape;
  - if next tape is already loaded in another free drive, switch to that drive and release the previous drive;
  - do not require a new free drive for ordinary rollover.
- Keep `NoDriveAvailable` scoped to initial session open only.
- Keep drive choice out of `PoolSelectionPolicy`; the policy sees only projected `already_loaded`.

Suggested signatures:
```rust
pub enum DriveAssignment {
    ReuseLoaded { drive_id: String },
    MountIntoFreeDrive { drive_id: String },
}

pub fn assign_drive_for_initial_pool_session(
    selected_tape: TapeUuid,
    drive_inventory: &DriveInventory,
) -> Result<DriveAssignment, OpenWriteSessionError>

pub fn assign_drive_for_rollover(
    session_drive: DriveHandle,
    next_tape: TapeUuid,
    drive_inventory: &DriveInventory,
) -> Result<RolloverDriveAssignment, RolloverError>
```

Validation gate:
- Test initial open prefers already-loaded tape/drive.
- Test initial open mounts into a free drive when selected tape is not loaded.
- Test initial open returns `NoDriveAvailable` when no free drive exists.
- Test rollover reuses held drive when no other free loaded drive exists.
- Test rollover does not emit `NoDriveAvailable`.

Risks/mitigations:
- Risk: counted Sutradhara drive lease conflicts with Remanence choosing physical drives. Mitigation: expose occupancy facts and let Sutradhara limit session opens; Remanence still chooses the concrete drive.
- Risk: loaded-in-busy-drive ambiguity. Mitigation: `already_loaded` should mean loaded in a usable/free drive for selection tie-breaks.
- Risk: switching drives at rollover leaks a held drive. Mitigation: make release/claim an atomic session-state transition.

Ambiguities needing implementation decision:
- Exact drive inventory type and busy/free vocabulary.
- Whether rollover may switch to another free loaded drive in v1, or always reuse the held drive for simpler implementation.

## Phase 6 - Reservation And Concurrency Enforcement

Design refs: §7. Dependencies: Phase 1 reservation table and bootstrap rebuild; Phase 3 candidate filtering; Phase 5 session-drive binding.

Files/modules touched: `crates/remanence-api/src/lib.rs`, session manager/shared daemon state, possible new `crates/remanence-api/src/session_reservations.rs`, state recovery modules.

Key changes:
- Add daemon-local `TapeReservationTable`, keyed by tape UUID.
- Reserve a tape when a pool session is handed that tape.
- Exclude tapes reserved by other live/recoverable sessions before building `PoolSelectionContext`.
- Allow the owning session to keep using its current tape for Tier-0 stickiness.
- Release reservation when:
  - tape is sealed and no longer current;
  - session rolls past it safely;
  - session closes/aborts;
  - orphan recovery resolves the session as closed/aborted.
- Rebuild reservations for recoverable orphaned pool sessions during startup before serving opens.
- Add logging/audit around reservation rebuild and reservation conflicts.

Suggested types:
```rust
pub struct TapeReservationTable {
    reservations: HashMap<TapeUuid, TapeReservation>,
}

impl TapeReservationTable {
    pub fn reserve(
        &mut self,
        tape_uuid: TapeUuid,
        session_id: &str,
        reason: ReservationReason,
    ) -> Result<(), ReservationError>;

    pub fn release_if_owner(&mut self, tape_uuid: TapeUuid, session_id: &str);

    pub fn is_reserved_by_other(&self, tape_uuid: TapeUuid, session_id: &str) -> bool;
}
```

Validation gate:
- Unit test two sessions cannot reserve the same tape.
- Unit test owning session can continue using its current reserved tape.
- Unit test abort/close releases reservation.
- Unit test orphan rebuild reserves current tapes before new selection.
- Integration test two concurrent sessions targeting one pool select different tapes.

Risks/mitigations:
- Risk: daemon-local reservations are lost on crash. Mitigation: startup rebuild from durable session/journal state before serving.
- Risk: stale reservation blocks a tape forever after failed cleanup. Mitigation: tie reservation lifetime to session lifecycle and recovery state, not only in-memory handles.
- Risk: race between selection and reservation. Mitigation: candidate projection, policy selection, and reservation claim must happen under one session-manager lock or equivalent atomic critical section.

Ambiguities needing implementation decision:
- Whether the next rollover tape is reserved before mount begins or only once selected. Recommended v1: reserve at roll.
- Exact lock granularity for session manager vs catalog state.

## Phase 7 - Error Surface And gRPC Mapping

Design refs: §13. Dependencies: Phases 1-6 define where each failure arises.

Files/modules touched: `proto/layer5.proto`, generated error/status mapping in `crates/remanence-api/src/lib.rs`, `crates/remanence-api/src/pool_write.rs`, `crates/remanence-api/src/pool_selection.rs`, state error conversions.

Key changes:
- Add/normalize internal errors:
```rust
pub enum PoolWriteSessionError {
    UnknownPool { pool_id: String },
    PoolExhausted { pool_id: String },
    ObjectTooLargeForEmptyTape { pool_id: String, object_size: u64 },
    NoDriveAvailable { pool_id: String },
    TapePoolAssignmentConflict { tape_uuid: TapeUuid, pool_id: String },
    ReservationConflict { tape_uuid: TapeUuid },
    InvalidPoolConfig { pool_id: String, reason: String },
}
```
- Map `PoolExhausted` to the append that triggered it; session should remain checkpointable/recoverable so Sutradhara can add media and resume if supported.
- Preserve `ObjectTooLargeForEmptyTape` semantics from existing no-spanning preflight.
- Ensure `NoDriveAvailable` is returned only by initial open.
- Keep `TapePoolAssignmentConflict` unchanged for tapes with foreign committed copies.
- Replace or retire `AmbiguousNeedsPolicy` once the configured selector is wired; keep it only for legacy Phase 1 helper paths if still needed.
- Add protobuf status/error codes if the current Layer 5 surface lacks specific enough variants. Otherwise map to tonic statuses with stable machine-readable details.

Validation gate:
- Unit tests for each internal error conversion to gRPC status/details.
- Test `PoolExhausted` occurs at append/rollover, not initial config load.
- Test `NoDriveAvailable` cannot be produced by rollover.
- Test invalid config fails daemon startup/config load, not first write.

Risks/mitigations:
- Risk: overloading generic tonic statuses makes Sutradhara handling brittle. Mitigation: include stable enum/code in protobuf details or response status.
- Risk: old clients depend on `AmbiguousNeedsPolicy`. Mitigation: update tests/docs with policy-based behavior and preserve legacy helper only where required.
- Risk: session state after `PoolExhausted` is unclear. Mitigation: define it explicitly as checkpointed at last committed object, with no partial next object.

Ambiguities needing implementation decision:
- Exact protobuf error enum additions.
- Whether `PoolExhausted` is retryable in the same session after media/config changes.
- Whether config validation failures are exposed through admin APIs or only startup logs/errors.

## Phase 8 - Tests And Verification

Design refs: §14 plus rust-design-verification checklist in §12.1. Dependencies: unit tests can land throughout; integration tests require full session/mount path.

Files/modules touched: `crates/remanence-api/src/pool_selection.rs`, API/session tests, state/config tests, VTL/integration test harness, docs test fixtures.

Unit tests:
- `CompleteOrFill`:
  - no candidates => `NeedFreshTape`;
  - Tier 1 beats Tier 2;
  - Tier 1 best-fit minimizes leftover;
  - `already_loaded` tie-break wins;
  - lowest `barcode_order` final tie-break;
  - `used + projected == low` counts as complete.
- `FillOldest`:
  - first fitting barcode wins;
  - empty candidates => `NeedFreshTape`.
- Candidate projection:
  - excludes sealed tapes;
  - excludes non-fitting tapes;
  - excludes reservations owned by other sessions;
  - computes `usable_bytes` and `low_bytes` consistently with config rounding.
- Sealing:
  - `used == low` seals;
  - early-warning seals;
  - below-low stays open unless force-sealed;
  - sealed tapes never re-enter candidates.
- Config:
  - defaults;
  - invalid watermark ordering;
  - unknown policy;
  - byte-size parsing;
  - invariant boundary equality.

Property tests:
- Given the §6 invariant and object sizes `>= S_min`, no generated stream leaves an open tape with usable-free `< S_min`.
- Generated candidate sets always produce either a fitting selected tape or `NeedFreshTape`; never a non-fitting tape.
- Selection tie-breaks are deterministic under shuffled candidate order.

Integration tests:
- Pool session rolls across at least two tapes within one session; object order and tape sequence are durable.
- Orphan/recover mid-roll or after first tape: reservations are rebuilt and resume continues from last committed object.
- Two concurrent sessions targeting one pool never select the same tape.
- Initial open returns `NoDriveAvailable` when all drives are busy.
- Rollover succeeds using held drive even with no free drives.
- Under-projected object crossing low seals from actual post-write position.
- d2 regression scenarios:
  - big-object-skip-then-top-up;
  - complete-a-tape routing.

Verification commands after implementation:
```bash
cargo fmt --all
cargo check --workspace
cargo clippy -p remanence-api --all-targets -- -D warnings
cargo test --workspace
```

Risks/mitigations:
- Risk: hardware/VTL integration is slow or unavailable locally. Mitigation: keep policy/session tests pure and run VTL scenarios on akash as the final gate.
- Risk: property tests expose rounding ambiguity. Mitigation: centralize byte-threshold computation before writing properties.
- Risk: concurrent edit conflicts hide behavioral regressions. Mitigation: run full workspace tests after rebasing on the settled write-engine/state changes.

## Cross-Phase Sequencing Notes

1. Land or rebase after the concurrent edits in `pool_write.rs`, `index.rs`, and `state.rs` settle. Do not start by rewriting those files while another process owns them.
2. Implement Phase 1 before replacing the placeholder selector. The current single-tape session model cannot safely express rollover, recovery, or reservations.
3. Implement pure `pool_selection.rs` logic early if useful, but keep it unintegrated until session state, config, and reservations exist.
4. Add config validation before enabling policy selection in production paths.
5. Wire eager sealing before broad integration testing; otherwise tests can pass selection while violating the active/sealed invariant.
6. Treat drive choice and reservation as one critical session transition: select tape, reserve tape, assign/mount drive, then publish session state.
7. Remove `AmbiguousNeedsPolicy` expectations only at the final selector-integration step.

## Overall Risks

- **Data-model migration:** `WriteSession.tape_uuid` is embedded in proto/state assumptions. Keep legacy projection and add new sequence fields rather than changing tag semantics.
- **Capacity source:** the watermark invariant depends on reliable cartridge capacity. If capacity is not modeled today, add a media-class or catalog capacity source before making validation fatal.
- **Actual position:** parity and no-parity write paths may expose post-write position differently. Centralize extraction and test both.
- **Reservation races:** policy selection is pure, but the caller must make selection plus reservation atomic.
- **Scope creep into Sutradhara policy:** Remanence should expose state and explicit close-out/session controls, not priority, queueing, preemption, WIP caps, or production-hours rules.
- **Merge pressure:** the known in-flight edits touch the exact integration files. Prefer small adapters/new modules and rebase before Phase 1/3/4 wiring.
