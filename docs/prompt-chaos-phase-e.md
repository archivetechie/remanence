# Codex Prompt — Chaos Adapter Phase E (changer / library faults, LIB-01..11)

Implement Phase E: library/changer fault injection over the changer transport,
surfacing through the real `LibraryHandle` paths. Phases A–D are landed; do not
reimplement them.

## Source of truth
- **Design (read fully): `docs/chaos-phase-e-changer-faults-design-v0.1.md`** —
  this prompt implements it; the design wins on any conflict.
- Parent: `docs/chaos-adapter-design.md` (Phase E).
- Catalogue: `/home/user/quadstor-chaos/quadstor-chaos.md` (LIB block; source of
  truth for sense tuples). Note: **no LIB-12 exists**; mailslot is prose-only.
- Phase B/C/D code: `crates/remanence-chaos/src/{lib.rs,model.rs}`,
  `crates/remanence-scsi/src/{read_element_status.rs,log_sense.rs}`.

## Part A — additive production change (the ONLY production behavior change)
`crates/remanence-library/src/model.rs` (Library construction, ~lines 383-435):
add `accessible: bool` (default true) to `Slot` (and `DriveBay`/`IePort` if
symmetric/cheap); populate from the already-parsed element flags as `accessible =
el.access && !el.except`. Purely additive — a new field carrying data the RES
parser already extracts; do not change any existing behavior. (Phase-C lesson:
keep the production delta to this.)

## Part B — chaos changer targeting
`crates/remanence-chaos/src/lib.rs` + `src/model.rs`:
- Add `library_id: Option<String>` to `DeviceCtx`; populate it for the `Changer`
  role in the model factory `device_ctx` (changer serial or a fixed model id);
  leave `None` for drives.
- Add a `library` arm to `target_matches` (`target = { library = <id> }` matches
  the changer ctx). `target.slot` is NOT a ctx match — it is read by the RES
  mutator (Part D).

## Part C — LIB sense defaults (reuse the Phase-B CC pre-call path)
In `default_sense_for_catalogue` + `sense_operation_allowed` add (fixed-format):
LIB-01 SK5/3B/0E (move_medium), LIB-02 SK5/3B/0D (move_medium), LIB-04 SK6/29/02
(move_medium), LIB-06 SK4/15/01 (move_medium), LIB-07 SK4/44/00 (move_medium),
LIB-08 SK2/04/18 (move_medium, read_element_status), LIB-10 SK2/04/00
(move_medium, read_element_status), LIB-11 SK2/3A/00 (read, space). These fire
pre-call as CHECK CONDITION; on the changer they surface as
`MoveError::ScsiError` (sense in the audit log; snapshot clean). LIB-11 maps via
`map_scsi` to `NoMedium` on the drive path.

## Part D — RES (0xB8) post-call mutator (LIB-05, LIB-09)
- Add fail-closed helpers to `crates/remanence-scsi/src/read_element_status.rs`
  (mirror `log_sense::set_tape_alert_flag`): `blank_element_voltag(page:&mut[u8],
  element_address:u16)->bool` (space-fill the targeted descriptor's voltag) and
  `set_element_exception(page:&mut[u8], element_address:u16)->bool` (set EXCEPT
  0x04 / clear ACCESS 0x08). Walk the element-status framing bounds-checked;
  return false (never panic) on malformation. Unit tests: synth→mutate→re-parse
  (targeted descriptor changed, others unchanged) + truncation-safe.
- In `ChaosTransport::execute_in`, add an `else if command.operation ==
  "read_element_status"` arm (mirroring the Phase-D log_sense merge) calling a new
  `post_read_element_status` that, for a matching `library` fault, applies the
  helper to the `target.slot` descriptor in `buf`. Model emits an honest clean
  page; ChaosTransport applies the fault. Populate a new JSONL `element_status`
  attribution.

## Part E — pending-UA-after-move (LIB-03)
Add `pending_ua: Option<PendingUa>` to `FaultEngineInner`. On a successful MOVE
MEDIUM (`execute_none` 0xA5 → Ok) with a matching LIB-03 fault armed, queue
`PendingUa { sense: SK6 28/00, fault_id }` (do NOT fire on the move). At the start
of the next changer command, if set, short-circuit with `Err(CheckCondition{UA})`,
clear it, mark the fault fired (one-shot). Scope strictly to "UA after MOVE".

## Part F — ModelTransport honest device
`src/model.rs`: add `set_slot_inaccessible(address)` + a full-but-blank-voltag
seeder; ensure RES descriptor emission round-trips EXCEPT/ACCESS (Phase-C
`descriptor_with_voltag` sets ACCESS but lacks an EXCEPT path — add it). No
changer LOG SENSE/TapeAlert handler (deferred).

## Part G — L1b tests (`remanence-chaos`, `#[cfg(target_os = "linux")]`)
Drive the real `LibraryHandle` over `ChaosTransport<ModelTransport>` (register
`set_audit_hook` to assert sense bytes):
1. Honest changer (chaos off): refresh returns seeded inventory; move succeeds.
2. LIB-01: arm SK5/3B/0E, `target.library`; `move_medium`/`load` →
   `Err(MoveError::ScsiError)` + 3B/0E in the audit event; not dirty.
3. LIB-08: arm SK2/04/18; failed op + sense.
4. LIB-05: arm RES voltag-blank `target.slot=N`; `refresh` → slot N `full`,
   `cartridge==None`.
5. LIB-09: arm RES exception `target.slot=N`; `refresh` → `slots[N].accessible ==
   false` (proves Part A end-to-end).
6. LIB-03: successful `move_medium`, next changer command returns UA once, second
   does not.
7. LIB-11: arm SK2/3A/00 on read; drive read with no tape → `NoMedium`.

## Constraints
- No root/QuadStor/`/dev/sg*`; hermetic `cargo test`.
- **No daemon/proto change.** Defer (do NOT implement): LIB-07 persistent offline
  state, LIB-10 latency/`time_scale`, changer-LUN TapeAlert page, LIB-12
  (nonexistent). Production delta = only the Part A `accessible` field.
- Defensive panic-free RES mutation helpers (medium-sourced framing).
- Reuse Phase B/C/D seams; add only the `library` ctx/matcher, LIB sense table,
  RES mutator+helpers, pending-UA state, model seeders.
- `cargo fmt --check` + `cargo clippy -p remanence-scsi -p remanence-library
  -p remanence-chaos -- -D warnings` clean; `cargo build --release` (harness
  freshness). Doc new `pub` items. Commit per `AGENTS.md` (journal + report; a
  test never silently passes).

## Acceptance (design §8)
- `Slot.accessible` carried from parsed element; existing tests unaffected.
- `read_element_status` helpers + unit tests (round-trip + truncation).
- LIB sense defaults; RES mutator (LIB-05/09) + JSONL `element_status`;
  pending-UA (LIB-03).
- L1b honest/LIB-01/08/05/09/03/11 green via the real `LibraryHandle`.
- Gates green (paste counts). Report: LIB rows L1b-proven, the additive
  `accessible` capability, deferrals.
