# Chaos Adapter Phase E — changer / library faults (LIB-01..11) — design v0.1

**Status:** design approved in discussion (2026-06-21, owner + claude); codex
implementation pending. Devloop: this doc + `prompt-chaos-phase-e.md` hand off to
codex. Refines `docs/chaos-adapter-design.md` (Phase E) with code-verified seams.
Builds on landed Phases B (`ChaosTransport`/`FaultEngine`), C (`ModelTransport` +
changer model), D (the post-call data-in injection seam pattern).

## 1. Scope

Phase E injects the **library/changer** fault family over the changer transport
(already `ChaosTransport`-wrapped in the Phase-C factory). Faults surface through
the *real* `LibraryHandle` paths — `move_medium` / `load` / `unload` (MOVE
MEDIUM `0xA5`) and `refresh` (READ ELEMENT STATUS `0xB8`).

**In — all of LIB-01..11:**
- Sense-based on MOVE MEDIUM: **LIB-01** source empty (SK5 3B/0E), **LIB-02**
  dest full (SK5 3B/0D), **LIB-04** reset mid-move (SK6 29/02), **LIB-06**
  gripper/picker (SK4 15/01), **LIB-07** accessor/teach (SK4 hardware error),
  **LIB-08** door open (SK2 04/18), **LIB-10** audit-window changer-not-ready
  (SK2 04/00).
- Sense-based on the drive path: **LIB-11** medium not present (SK2 3A/00 on
  READ/SPACE with no tape → `TapeIoError::NoMedium`).
- Cross-command UA: **LIB-03** inventory-changed UA (SK6 28/00) on the *next*
  changer command after a successful MOVE — needs the new pending-UA primitive.
- Stateful via the RES response: **LIB-05** barcode unreadable (blank VolumeTag →
  slot `full` but `cartridge==None`), **LIB-09** inaccessible slot (EXCEPT/ACCESS
  → slot marked inaccessible) — needs the additive model change (§2).

**Out (deliberately deferred), with reason:**
- **LIB-07 persistent "library offline until re-teach"** — no library-offline
  state exists (the changer has only the shared `DirtyCause`). E models LIB-07's
  *sense*; the offline-persistence is deferred until a library-health state is
  designed.
- **LIB-10 latency** (tens of seconds–minutes) — no delay/`time_scale`
  mechanism exists. E models LIB-10's *not-ready sense*; the latency is deferred
  to the Phase-D2 delay framework.
- **Changer-LUN TapeAlert page** (LOG SENSE 0x2E on the changer LUN, catalogue
  line 84) — no individual LIB fault requires it; defer to a later phase.
- **LIB-12** — does **not** exist in the catalogue (mailslot is prose-only,
  folds into LIB-08/LIB-10); the deployment has no mailslots. Dropped.
- MOVE-CDB src/dst element extraction for per-slot move targeting — move faults
  are library-scoped in E (the scenario arms them per move); element extraction
  is optional later polish.

## 2. Production change (additive) — surface EXCEPT/ACCESS in the model

The RES parser already extracts `except` (flags & 0x04) and `access` (flags &
0x08) per element (`remanence-scsi/read_element_status.rs`), but `Library`
construction (`remanence-library/src/model.rs:383-435`) **drops** them — so an
inaccessible slot is invisible to callers today. This is a genuine operational
gap (an operator/daemon should know which slots are inaccessible, e.g. MSL3040
lowest rows). Phase E closes it, additively:

- Add `accessible: bool` (default true) to `Slot` (and `DriveBay`/`IePort` if
  cheap and symmetric) in `model.rs`. Populate from the parsed element:
  `accessible = el.access && !el.except` (ACCESS set and no exception).
- This is the **only** production behavior change in E, and it is purely
  additive (a new field + carrying already-parsed data). Existing callers are
  unaffected (the field defaults to accessible). `RefreshInventory`/`GetLibrary`
  consumers gain visibility for free; wiring it into the proto/daemon surface is
  **not** required for E (the field on the `Library` value type is enough for the
  L1b assertion). (Heed the Phase-C lesson: keep the production delta to this one
  additive field; no behavioral edits elsewhere.)

## 3. Changer targeting — DeviceCtx + matcher

Today the changer `DeviceCtx` is empty (only `backend`) and `target_matches`
knows only `drive`/`tape`. Add:

- A `library_id: Option<String>` field on `DeviceCtx`; populate it for the
  `Changer` role in the model factory's `device_ctx` (e.g. the changer serial or
  a fixed model id). Drive ctx leaves it `None`.
- A `library` arm in `target_matches`: `target = { library = <id> }` matches the
  changer ctx. Most LIB faults use this (library-scoped).
- `target = { slot = N }` is **not** a `DeviceCtx` match (the RES command isn't
  slot-specific); it is a **parameter consumed by the RES post-call mutator**
  (§4) to pick which element descriptor to garble/mark. Keep it out of
  `target_matches` (unknown keys are already ignored there); read it in the
  mutator.

## 4. ChaosTransport — LIB sense defaults, RES mutator, pending-UA

Three additions, each mirroring an existing seam.

### 4.1 Sense-based LIB faults (reuse the Phase-B CC path)
Add to `default_sense_for_catalogue` and `sense_operation_allowed`:

| Catalogue | Sense (fixed-format) | Allowed op(s) |
|--|--|--|
| LIB-01 | SK 5 / 3B / 0E | move_medium |
| LIB-02 | SK 5 / 3B / 0D | move_medium |
| LIB-04 | SK 6 / 29 / 02 | move_medium |
| LIB-06 | SK 4 / 15 / 01 | move_medium |
| LIB-07 | SK 4 / 44 / 00 (hardware error) | move_medium |
| LIB-08 | SK 2 / 04 / 18 | move_medium, read_element_status |
| LIB-10 | SK 2 / 04 / 00 (not ready) | move_medium, read_element_status |
| LIB-11 | SK 2 / 3A / 00 | read, space |

These fire **pre-call** (CHECK CONDITION) exactly like MED-01/RDY-02. On the
changer path the inner `MoveError::ScsiError` carries the sense into the audit
log (the changer doesn't decode sense — it keys on the `ScsiError` *variant*),
so a CHECK CONDITION leaves the snapshot clean and surfaces as
`MoveError::ScsiError`. LIB-11 maps through `map_scsi` `(0x02,0x3A,_) →
NoMedium` on the drive path.

### 4.2 RES (0xB8) post-call mutator — the new data-in seam (LIB-05, LIB-09)
Mirror the Phase-D LOG SENSE merge: in `execute_in`, add an
`else if command.operation == "read_element_status"` arm calling
`post_read_element_status(ctx, command, cdb, buf, returned_bytes)`. The mutator,
for a matching `library`-scoped fault, edits the targeted slot's descriptor
**in-place** using new fail-closed helpers in
`remanence-scsi/read_element_status.rs` (analogous to `log_sense`'s
`set_tape_alert_flag`):

- `blank_element_voltag(page: &mut [u8], element_address: u16) -> bool` — space-
  fill the targeted descriptor's voltag (LIB-05) so the parser yields
  `primary_voltag == None` while `full == true`.
- `set_element_exception(page: &mut [u8], element_address: u16) -> bool` — set
  EXCEPT (0x04) / clear ACCESS (0x08) on the targeted descriptor (LIB-09) so the
  parser yields `except==true`/`access==false` → model `accessible==false`.

Both walk the element-status framing (header → per-type pages → descriptors),
bounds-checked, returning `false` on any malformation (never panic). The
**model emits an honest clean page**; ChaosTransport applies the fault — the
invariant holds. The mutator reads `target.slot` (or all matching elements if
absent) to pick the descriptor. Populate a new `element_status` attribution in
the JSONL event (mirroring `tape_alert`/`mutation_summary`).

### 4.3 Pending-UA-after-move primitive (LIB-03)
Add minimal cross-command state to `FaultEngineInner`: `pending_ua:
Option<PendingUa>` (the UA sense + the fault id). On a **successful** MOVE MEDIUM
(`execute_none` `0xA5` returns `Ok`) with a matching LIB-03 fault armed, queue
`PendingUa { sense: SK6 28/00, fault_id }` (do not fire on the move itself). At
the **start** of the next changer command (`execute_none` or `execute_in`), if
`pending_ua` is set, short-circuit with `Err(CheckCondition{UA})`, clear it, and
mark the fault fired (one-shot). Keep it scoped strictly to "UA after MOVE" — do
not generalize to arbitrary after-X sequencing.

## 5. ModelTransport — honest changer device (mostly already there)

Phase C's changer model already synthesizes RES and handles MOVE MEDIUM. Phase E
adds only:
- Optional per-element seeders for pre-seeded states:
  `VirtualWorld::set_slot_inaccessible(address)` and a blank-voltag-but-full
  slot seeder — so a test can also exercise the honest path (model emits the
  faulted page without the engine). The **primary** path is ChaosTransport
  injection (§4.2); seeders are for the honest-device baseline test.
- The model's RES descriptor emission must round-trip the new EXCEPT/ACCESS bits
  correctly when seeded (so the production model change in §2 has something to
  read). Phase-C `descriptor_with_voltag` sets ACCESS (0x08) but has no EXCEPT
  path — add it.

No changer LOG SENSE (TapeAlert) handler in E (deferred, §1).

## 6. L1b test suite (`remanence-chaos`, `#[cfg(target_os = "linux")]`)

Drive the **real** `LibraryHandle` paths over `ChaosTransport<ModelTransport>`
(Phase-C handle recipe; register an audit hook via `set_audit_hook` to assert on
sense bytes). Representative coverage across the fault shapes:

1. **Honest changer (chaos off)** — `refresh` returns the seeded inventory;
   `move_medium` succeeds; baseline.
2. **LIB-01 source-empty** — arm SK5 3B/0E on `move_medium`, `target.library`;
   assert `move_medium`/`load` returns `Err(MoveError::ScsiError)` with the
   3B/0E sense in the audit event; snapshot stays clean (not dirty).
3. **LIB-08 door-open** — arm SK2 04/18; assert the failed op + sense (the one
   fault with a shipped `sensectl` example — good provenance anchor).
4. **LIB-05 barcode-unreadable** — arm a RES voltag-blank for `target.slot = N`;
   `refresh`; assert `library().slots[N]` is `full` with `cartridge == None`.
5. **LIB-09 inaccessible-slot** — arm a RES exception for `target.slot = N`;
   `refresh`; assert `library().slots[N].accessible == false` (proves the §2
   production change end-to-end).
6. **LIB-03 post-move UA** — arm SK6 28/00 LIB-03; a successful `move_medium`
   then the next changer command returns the UA once; a second command does not
   (one-shot).
7. **LIB-11 no-medium** — arm SK2 3A/00 on `read`; a drive read with no tape →
   `TapeIoError::NoMedium`.
8. **rem-scsi unit tests** for `blank_element_voltag` / `set_element_exception`:
   synthesize a page, mutate, re-parse, assert the targeted descriptor changed
   and others didn't; truncated page → `false`, no panic.

## 7. Constraints / gotchas

- **The changer never decodes sense** — it routes on the `ScsiError` variant
  (CheckCondition → clean snapshot; TransportError → dirty
  `CompletionUnknown`). So LIB sense faults assert via the returned
  `MoveError`/`RescanError` + the audit hook's sense bytes, not via a decoded
  sense on the changer. Use `TransportError` only where the catalogue means a
  completion-unknown failure (not for LIB CHECK-CONDITION faults).
- **`refresh` fires no Started/Finished audit op** for the RES read (only
  `Warning`); a RES *mutation* (LIB-05/09) is asserted via the resulting
  snapshot, and a RES *CHECK CONDITION* (LIB-08/10) via the bubbled
  `Err(ScsiError)` from `refresh`.
- **Production delta is exactly the §2 field** — nothing else in
  `remanence-library` changes. (Phase-C lesson.)
- **Defensive, panic-free** RES mutation helpers (medium-sourced framing);
  bounds-check every descriptor walk, fail closed.
- Reuse: the Phase-B CC pre-call path, the Phase-D post-call data-in seam
  pattern, the one-shot `fired_once` delivery, `target_matches`. Add only the
  `library` ctx/matcher, the LIB sense table, the RES mutator + helpers, and the
  pending-UA state.
- `missing_docs = warn` on the touched crates — doc new `pub` items.

## 8. Acceptance (Phase E)

- `Slot.accessible` (additive) carried from the parsed element; existing tests
  unaffected.
- `remanence-scsi::read_element_status` gains `blank_element_voltag` +
  `set_element_exception` with round-trip + truncation-safe unit tests.
- ChaosTransport: LIB-01/02/04/06/07/08/10/11 sense defaults; RES post-call
  mutator (LIB-05/09) + JSONL `element_status` attribution; pending-UA-after-move
  (LIB-03).
- L1b: honest changer, LIB-01, LIB-08, LIB-05, LIB-09, LIB-03, LIB-11 — all
  green, driving the real `LibraryHandle`.
- `cargo test -p remanence-scsi -p remanence-library -p remanence-chaos` green;
  `cargo fmt --check` + `cargo clippy -p remanence-scsi -p remanence-library
  -p remanence-chaos -- -D warnings` clean; `cargo build --release`.
- Report: LIB rows now L1b-proven, the additive `accessible` capability, and the
  deferrals (LIB-07 offline-state, LIB-10 latency, changer-LUN TapeAlert).
