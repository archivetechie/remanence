# Code review — chaos Phase E (changer/library faults, LIB-01..11), 2026-06-21

**Scope:** the implementation of `docs/prompt-chaos-phase-e.md` /
`docs/chaos-phase-e-changer-faults-design-v0.1.md` — commit `6b6361d`. Also
verified the Phase-D follow-up `c313a2a` ("Fix TapeAlert short allocation
reporting") which closes the Phase-D Low.

**Method:** read the production `Slot.accessible` change, the rem-scsi RES
mutators, and every cross-crate ripple line by line; dispatched a parallel deep
review of the chaos side (LIB sense table, RES mutator, LIB-03 pending-UA) that
built+ran the suites and verified claims against code; **independently
re-verified the one HIGH** (the prior phases taught me to check subagent
findings — and this time the subagent was right and my first glance at the gate
was wrong).

**Gates:** `cargo fmt --check` clean; tests **scsi 123 / library 259 / chaos 41,
0 failed**; **`cargo clippy --workspace --all-targets -- -D warnings` FAILS with
exactly one error** (H1 below).

## Verdict

**Strong, faithful implementation — one must-fix (a CI-gate-breaking clippy lint
in a test helper), otherwise no Critical/High correctness issues.** The
production delta is exactly the one additive `accessible` field as designed; the
rem-scsi RES mutators are bounds-checked and fail-closed; the chaos side keeps
the model=honest / chaos=fault invariant; LIB-03's cross-command UA is correctly
scoped to the changer, one-shot, and post-move; and the L1b tests drive the real
`LibraryHandle`. The single blocker is mechanical and test-only.

## H1 (High — must fix; breaks the CI clippy gate)
`crates/remanence-chaos/src/model.rs:1911`. The new test helper
`capture_finished_sense` returns `Arc<Mutex<Vec<(u8, u8, u8, bool)>>>`, tripping
`clippy::type_complexity`. `cargo clippy --workspace --all-targets -- -D
warnings` fails (exit 101, one error) — the project/CI standard
(`feedback_use_clippy_and_rustfmt`, CLAUDE.md "CI gates all three"). It slipped
through because plain `cargo clippy` doesn't lint `#[cfg(test)]` code and the
Phase-E prompt's clippy line omitted `--all-targets` (my under-specification —
noted so the next prompt includes it). **Fix (trivial, test-only):** add a type
alias in the test module — `type CapturedSense = Arc<Mutex<Vec<(u8,u8,u8,bool)>>>;`
— or annotate the fn `#[allow(clippy::type_complexity)]`. No logic change.

## Low / Nit (no blocker)

- **L1 (Low) — `execute_out` doesn't consume a pending LIB-03 UA.**
  `lib.rs` `take_pending_ua` is called in `execute_in`/`execute_none` only. A
  changer data-out CDB (e.g. MODE SELECT) between the move and the next in/none
  command wouldn't consume the queued UA, leaving it stranded one extra command.
  No real impact (the changer flows issue no data-out; design scopes the UA to
  "next execute_none/execute_in"). Optional: a symmetric check or a one-line
  comment.
- **L2 (Low) — LIB-04 "reset mid-move" models sense only; snapshot stays clean.**
  SK6 29/02 fires as a pre-call CHECK CONDITION, which `completion_unknown`
  treats as not-dirty, so the move is "did not happen." A real bus-reset-mid-move
  is completion-unknown and would arguably dirty the snapshot. **This matches the
  approved design** (§4.1/§7 route LIB-01/02/04/06/07 through the CC path and
  reserve `TransportError`/dirty for genuine completion-unknown) — a conscious
  call, flagged only for visibility.
- **Nit — no L1b end-to-end for LIB-02/04/06/07/10** (only the sense-table unit
  test). The design called for *representative* L1b (01/08/05/09/03/11), so this
  is acceptable; a `refresh → 04/00` LIB-10 case would be cheap symmetry.

## Verified conformant (re-checked against code)

- **Production delta is minimal + additive:** `Slot`/`DriveBay`/`IePort` gain
  `accessible: bool`, populated `el.access && !el.except`
  (`remanence-library/src/model.rs:401,423,435`). Every other cross-crate touch
  (api, cli, pool_ops, ops, discovery, handle/tests, **parity/raw.rs**) is an
  `accessible: true` field-add at a struct-literal site, almost all inside
  `#[cfg(test)]` — no behavioral change, no proto/CLI-schema change (report
  confirms). The Phase-C "avoidable production edit" lesson was heeded.
- **rem-scsi RES mutators** (`read_element_status.rs`): `blank_element_voltag` /
  `set_element_exception` walk the element-status framing with `checked_add`
  everywhere, validate `desc_len`/`page_bytes % desc_len`, bound every index, and
  **return `false` (never panic)** on malformed/truncated/missing descriptors.
  Mutate-target-only proven by unit tests (`..._mutates_target_only`,
  `..._fail_closed_on_truncation`). Excellent for medium-sourced framing.
- **LIB sense table** matches the design exactly (LIB-01 5/3B/0E, 02 5/3B/0D, 04
  6/29/02, 06 4/15/01, 07 4/44/00, 08 2/04/18, 10 2/04/00, 11 2/3A/00) with
  correct op gating; locked by `lib_fault_sense_defaults_cover_catalogue_rows`.
- **Changer targeting:** `DeviceCtx.library_id` populated only for the Changer
  role; `target_matches` honors `target.library`; `target.slot` is a RES-mutator
  selector, not a ctx match.
- **RES post-call mutator** applies LIB-05/09 to the `target.slot` descriptor via
  the helpers; model emits an honest clean page; LIB-05/09 do **not** pre-fire as
  CHECK CONDITION (no default-sense entry); JSONL `element_status` populated;
  respects `returned_bytes.min(buf.len())`.
- **LIB-03 pending-UA:** move still succeeds when armed (LIB-03 has no default
  sense, so `pre_call_decision` never fires it; queued only in the `Ok` branch of
  `execute_none`); fires on the *next* changer command once then clears
  (one-shot); **correctly scoped to the changer** (`take_pending_ua` early-returns
  unless `ctx.library_id.is_some()`, so it cannot leak to drive commands);
  re-arm guarded against double-queue.
- **Snapshot clean on LIB CHECK CONDITION** (`completion_unknown == false`), so
  LIB-01/08 surface as `MoveError::ScsiError` with sense in the audit log, not
  dirty — the LIB-01 test asserts `!is_dirty()` + the audit sense.
- **LIB-11 → `NoMedium`** end-to-end via `map_scsi (0x02,0x3A,_)`.
- **Model extensions:** `SlotState` gains full/access/except; seeders
  `set_slot_inaccessible` / full-without-voltag; `descriptor_with_voltag` now
  round-trips EXCEPT (new) + ACCESS; honest page parses clean by default; no
  panic on small buffers.
- **L1b drives the real handle:** honest changer, LIB-01, LIB-08 (move+refresh),
  LIB-05, LIB-09 (`slots[N].accessible == false` — proves Part A end-to-end),
  LIB-03, LIB-11 — all via `handle.refresh()/move_medium()/read_block()` over
  `ChaosTransport<ModelTransport>`, asserting exact sense tuples / dirty flag /
  snapshot / JSONL. Strong.
- **Phase-D follow-up `c313a2a`:** `merge_tape_alert_flags` now returns
  `TapeAlertMerge { returned_bytes, applied_flags }` and reports only the flags
  actually written (filtered by canonical offset within `visible_len`) —
  correctly closes the Phase-D Low (page vs reported-flags disagreement on
  sub-canonical alloc_len).

## Net

Phase E lands all of LIB-01..11 faithfully, with the genuine `accessible`
capability surfaced (closing a real `remanence-library` gap), bounds-safe RES
mutation, and correct cross-command UA semantics. **One must-fix before it's
gate-clean: the `type_complexity` clippy error in the test helper** (H1) — a
one-line change. Deferrals (LIB-07 offline-state, LIB-10 latency, changer-LUN
TapeAlert, nonexistent LIB-12) are as designed.
