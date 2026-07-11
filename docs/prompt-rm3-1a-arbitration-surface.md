# Prompt RM3.1a — per-drive arbitration surface (advisory) + sutradhara (library,bay) queue

**Status:** pending (gpt-5.6-sol). RM3.1a (per the folded design's corrected sequence, after RM3.0).
Spans **remanence** (an ADVISORY drive-assignment surface over the EXISTING atomic bay reservation) +
**sutradhara** (a `(library_serial, bay)`-keyed restore queue that re-queues on a lost race). RM1+RM2
COMPLETE; RM3.0 (timeout fix) landed.
**Normative (read FIRST, binding — do NOT inline):** `docs/design-restore-tape-leg-v0.1.md` **§6.3**
(the arbitration decision — the enforcement is ALREADY race-free via the atomic bay reservation, so the
new surface is ADVISORY ONLY; queue key MUST be `(library_serial, bay)` the enforcement unit, NOT the
floatable `drive_uuid` which is a display hint; sutradhara treats a lost-race `FailedPrecondition` as
RE-QUEUE, never fail) + §6.7 (RM3.1a scope) + §2 (ground truth). **The design's file:line map predates
TIO-6 R2 — SURVEY THE CURRENT CODE and cite the real current lines; do NOT trust the doc's line numbers.**
**Survey + verify against the CURRENT code:**
- remanence: the per-bay atomic reservation (a `DrivePool`-like structure with an atomic `AtomicBool`/
  `compare_exchange` per bay, held for a whole `ReadSession`, process-global across the UDS+mTLS
  listeners; a 2nd `OpenReadSession` on a busy bay is REJECTED with a "read session already active"
  `FailedPrecondition`; tape-uuid dedup). Find it (grep `read session already active`, `OpenReadSession`,
  the reservation struct). `proto/layer5.proto` `GetLiveStatusResponse` (libraries/operations/alarms/
  daemon_epoch — NO per-drive queued/active surface today) + `GetLiveStatus` RPC. `mount.rs` for the
  bay↔drive_uuid derivation + retired-drive float.
- sutradhara: the restore admission / OpenReadSession client path (`src/sutradhara/backend/remanence.py`
  `OpenReadSession`/`open_read_session`) — where a 2nd concurrent tape restore currently gets a bare
  `FailedPrecondition`; and the restore admission (hdcache/RestoreService) where a bay-keyed queue belongs.

## Scope
1. **Remanence advisory drive-assignment surface.** Expose per-drive `{drive_uuid, library_serial, bay,
   state: idle|active, current_session_id?, ...}` — as a NEW field on `GetLiveStatusResponse` OR a new
   `GetDriveAssignments` RPC (pick per the current proto style; a field on GetLiveStatus is lighter). It
   READS the existing reservation state (advisory — the atomic reservation stays the sole enforcement;
   this surface never gates). Key/report by `(library_serial, bay)` as the enforcement unit; include
   `drive_uuid` as an operator-legibility HINT (document that it floats across bays on swap/retire). Do
   NOT add an atomic queue-and-open RPC (the reservation already arbitrates — §6.3).
2. **Sutradhara `(library_serial, bay)`-keyed restore queue.** In the restore admission/dispatch, before
   opening a tape `ReadSession`, consult the advisory surface and QUEUE additional tape restores per
   `(library_serial, bay)` (one active `ReadSession` per bay). On a lost race — a `FailedPrecondition`
   "read session already active" from `OpenReadSession` — **RE-QUEUE the item, never fail it** (§6.3;
   this is the entire substance of the TOCTOU concern GLM raised, resolved by re-queue). The queue keys on
   `(library_serial, bay)`, NOT drive_uuid.

## Binding invariants
- **Advisory only:** the atomic bay reservation remains the SOLE enforcement (race-free already); the new
  surface never gates a mount. No atomic queue-and-open RPC. Queue key = `(library_serial, bay)`.
  Lost-race `FailedPrecondition` ⇒ re-queue, never fail. Disk-tier (hdcache) restores UNAFFECTED (this is
  tape-only). No change to the existing OpenReadSession enforcement semantics. TIO-6 read pipeline
  untouched.

## Tests (verification member — REQUIRED, non-vacuous, no skip)
- **remanence:** the drive-assignment surface reports `idle` for a free bay and `active` (with the
  session) for a bay holding a live `ReadSession`, keyed by `(library_serial, bay)`; a 2nd OpenReadSession
  on a busy bay still gets `FailedPrecondition` (enforcement unchanged); drive_uuid reported as a hint.
- **sutradhara:** a 2nd concurrent tape restore for a busy bay QUEUES (not fails); when the first
  releases, the queued one proceeds; a lost-race `FailedPrecondition` re-queues (a test that would FAIL if
  it were treated as a hard failure); the queue keys on `(library_serial, bay)` (a drive_uuid float across
  bays does not misroute).

## Definition of done (each repo's AGENTS.md)
remanence: `cargo build` + `cargo test` + `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings`
clean (paste tallies). sutradhara: `uv run pytest -q` + ruff + mypy clean on touched files. Summary per
repo: files touched (with the REAL current line cites you surveyed), each test → the scope item, and an
explicit statement that (a) the surface is advisory (enforcement unchanged), (b) the queue keys on
`(library_serial, bay)`, (c) a lost race re-queues, (d) disk-tier restores are unaffected. Land as two
commits (remanence proto+surface first, sutradhara queue second) or note the ordering. Do NOT implement
the app-restart contract (RM3.1b), the diag harness (RM3.2), or ranged AEAD (RM3.3).
