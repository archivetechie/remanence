# Prompt RM3.1b — app-restart tape-read session contract (tape_uuid-bound cold resume + position proof)

**Status:** pending (gpt-5.6-sol). RM3.1b — the LAST RM3 correctness item (design-sequenced last: no
consumer until the RM2 agent resume path, which is now landed). Remanence read-path + proto. **Physical
resume acceptance is the MSL3040 window; this delivers the contract + hermetic tests.**
**Normative (read FIRST, binding — do NOT inline):** `docs/design-restore-tape-leg-v0.1.md` **§6.2** (the
resume token persists session-INDEPENDENT durable coordinates and re-opens COLD — NO session_id is ever
persisted; ADD `tape_uuid` — object_id does NOT identify the mount target; VERIFY tape identity (library
barcode/uuid) BEFORE trusting the position proof; resume is FILE-BOUNDARY, never mid-file, so per-file SHA
stays computable; the proof wire form reuses the write-side `AppendCommitInfo.position_after_lba` uint64
LBA; the `DevicePositionProof` is internal today and must get a serialized form) + §6 context. **The
design line-map predates recent changes — SURVEY the current code, cite real lines.**
**Survey + verify against CURRENT code:** `proto/layer5.proto` — `OpenReadSessionRequest` (`oneof
{drive_target, tape_target}` + idempotency_key; NO position/offset/resume field today), `ReadSession`,
`GetReadSessionRequest` (session-id only), and the WRITE-SIDE RESUME PRECEDENTS to port:
`OpenWriteSessionRequest.recover_session_id`, `APPEND_MODE_RESUME_CONTROL`, `AppendCommitInfo.{
position_before_lba, position_after_lba, journal_record_ordinal}`. `crates/remanence-api/src/read_core.rs`
— the `DevicePositionProof` handling (grep `DevicePositionProof`/`accept_proof`/`PositionAfter::Device`/
`proven_frontier`), `run_read_pipeline`, the `source.space(...)` locate. `mount.rs` — the tape-uuid /
barcode identity at mount + `OpenReadSession`'s tape-uuid dedup.

## Scope
1. **Extend `OpenReadSessionRequest` with an optional RESUME TARGET** `{tape_uuid, object_id, file_id,
   file_boundary_byte_offset, expected_position_lba?, daemon_epoch?}` — session-INDEPENDENT durable
   coordinates. Do NOT add a session_id resume field (resume always mints a FRESH session, cold-open).
2. **Surface the `DevicePositionProof` to the caller** — a new field on the read response / a
   `GetReadSessionPosition`-style read, serialized via the write-side encoding (`position_after_lba` uint64
   LBA). Define `DevicePositionProof`'s wire form (it is internal today).
3. **Read-path resume-open (cold):** on `OpenReadSession` with a resume target: mount the tape identified
   by `tape_uuid` (NOT object_id) and VERIFY the mounted tape's identity (library barcode/uuid) BEFORE
   trusting position — physical-position continuity is necessary but not sufficient. Then `space()`/locate
   to `(object_id, file_id, file_boundary_byte_offset)` (chunk/file aligned), issue a Read-Position proof,
   and RETURN it so the caller can verify continuity against `expected_position_lba`. Resume is FILE
   BOUNDARY only (the in-progress file re-streams from its start; per-file SHA owns integrity, so the proof
   is correctness-of-restart, not corruption-detection — deliberately weaker + cheaper than the in-session
   park-RP).

## Binding invariants
- NO session_id persisted (cold re-open, fresh session). `tape_uuid`-bound (verify tape identity at mount
  before trusting position). FILE-boundary resume (never mid-file). Proof reuses `position_after_lba` uint64
  LBA encoding (port the write-side precedent, don't invent). The in-session park-RP + TIO-6 reservoir are
  UNCHANGED (this is a new app-restart contract distinct from the internal per-command park-RP). Disk-tier
  restores unaffected.

## Tests (verification member — REQUIRED, non-vacuous, no skip)
- **Cold resume re-locates + returns a matching proof:** open a read session, note the position; drop the
  session (simulate daemon restart — no session state); re-open with the resume target → it mounts by
  tape_uuid, verifies tape identity, locates to the file boundary, returns a position proof matching
  `expected_position_lba`.
- **Wrong-tape rejection:** a resume target whose `tape_uuid` does not match the physically-mounted/loaded
  tape is REJECTED before trusting any position (the swapped/stale-tape case) — non-vacuous (would pass a
  matching physical LBA on the wrong tape if identity weren't checked).
- **File-boundary:** the resume offset is chunk/file aligned; the in-progress file re-streams from its
  start (no mid-file resume).
- **No session_id persistence:** the resume token carries no session id; a resumed open mints a fresh
  session/generation.
- Existing read-session + TIO-6 reservoir tests stay green.

## Definition of done (this repo's AGENTS.md)
`cargo build`+`cargo test`+`cargo fmt --check`+`cargo clippy --all-targets -- -D warnings` clean (paste
tallies). Summary: files touched (real current lines); the proto resume target + proof wire form; each
test → scope item; explicit statement that (a) no session_id is persisted (cold resume), (b) tape identity
is verified before position, (c) resume is file-boundary, (d) the proof reuses position_after_lba, (e) the
in-session park-RP/TIO-6 reservoir are unchanged, (f) physical resume acceptance is deferred to the
MSL3040 window. This is the LAST RM3 correctness item — after it, RM3's code is complete (physical
acceptance pending the field window). Do NOT implement the diag (RM3.2) or ranged AEAD (RM3.3).
