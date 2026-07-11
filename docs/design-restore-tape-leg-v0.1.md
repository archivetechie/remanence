# Design — RM3: restore tape leg (app-restart contract + per-drive arbitration + measured backpressure)

**Status:** design, FOR PANEL REVIEW (Claude 2026-07-11). **RM3** of the restore-agent build — the
tape-specific pieces the disk-tier RM1/RM2 deferred. Spans **remanence** (the app-restart session
contract, the per-drive arbitration surface, the ranged-ciphertext AEAD extract, the diag) +
**sutradhara** (the arbitration consumer + the extract-stream timeout). Grounded in a file:line map of
the current remanence tape-read path (survey 2026-07-11). TIO-6 R2 is LANDED on remanence main.

## 1. Scope
Five items (per `design-restore-agent-v0.1.md` §21.1 RM3 + the RM0.3b follow-ups): (a) a NEW
application-restart session contract in remanence (resume a tape read across a process/app boundary —
locate to object/file/offset + a caller-visible position proof — DISTINCT from the TIO-6 reservoir's
internal per-command park-RP); (b) per-drive arbitration EXPOSED as a queued/active surface so
sutradhara can QUEUE additional tape restores (one active `ReadSession` per physical drive — the
§8-B2 concurrency invariant); (c) MEASURED backpressure on the real agent→sutradhara→remanence
two-hop chain; (d) fix the RM0.3b AEAD `extract-stream` inactivity timeout to survive cold tape
mount+seek first-byte latency; (e) fix the encrypted-bundle O(N) whole-object re-stream.
The disk tier (hdcache) is RM1/RM2 and unaffected.

## 2. Ground truth (from the survey — several pieces are less new than expected)
- **Per-drive exclusivity ALREADY exists** (`remanence-api/write_owner.rs:184-252` `DrivePool` — atomic
  `AtomicBool` per bay, held for the whole session, process-global across the UDS + mTLS listeners,
  `daemon/lib.rs:60-153`). A 2nd `OpenReadSession` on a busy bay is REJECTED (`write_owner.rs:2523-2537`
  "read session already active") + tape-uuid dedup (`write_owner.rs:320-341`). **Missing:** a
  QUEUED/ACTIVE surface for sutradhara to query/wait on (today it just gets `FailedPrecondition`); and
  the key is `bay`, not a stable `drive_uuid` (drive can float across bays, `mount.rs:716-769`).
- **The ranged AEAD decrypt primitive is BUILT but UNWIRED** (`remanence-aead/range.rs:41-192`
  `open_plaintext_range*` — chunk nonces from `(chunk_index, final_chunk)`, no chaining, only covering
  chunks fetched+authenticated, tested `range.rs:291-305`). RM0.3a's `extract-stream` uses whole-object
  `open()` (`remanence-cli/lib.rs:9321-9388`); `range.rs` is referenced by NO CLI subcommand. So the
  encrypted-bundle O(N) is pure impl debt — the primitive exists, the surface doesn't.
- **No app-restart contract** — the proto (`proto/layer5.proto:1062-1116`) has no position/offset/
  resume field; `GetReadSession` is session-id-only (in-memory, dies on daemon restart); the
  `DevicePositionProof` is trapped inside the pipeline (`read_core.rs:159-182` `ReadDelivery::
  ProofFrontier`), never surfaced to the caller. WITHIN a live session a caller CAN re-locate (each
  `ReadObjectRange` re-runs `source.space` to the object, `write_owner.rs:2420-2449`) — what's missing
  is restart ACROSS a session boundary + a caller-visible proof.
- **Diag is `tracing`-log-only** (target `remanence_read_diag`, `read_core.rs:617-626,885-896`:
  `occupancy_bytes/park_cycles/park_us/free_wait_us/feed_gap_*`). Counters on `ReservoirState`
  (`read_core.rs:322-338`) are in-process, scoped to one `run_read_pipeline`, dropped on return. No RPC
  carries them.
- **extract-stream timeout is Python-side** (`sutradhara/archive_restore.py:1376-1437`, 120 s
  `_REM_STREAM_INACTIVITY_TIMEOUT_SECONDS:63`), `last_activity` updated only on pipe I/O. The
  ciphertext pump (`pump_ciphertext:1284-1307`) calls `OpenReadSession` → drive mount + `space()` seek
  BEFORE the first write, while the poll loop ticks the same clock — a cold mount+seek >120 s
  false-kills. Python has NO visibility into remanence mount/seek progress (`OpenReadSession` is
  synchronous, not an `OperationRef`; `GetOperation`/`WatchOperation` exist for other flows).
- **Encrypted-bundle O(N):** the live AEAD path (`archive_restore.py:1234-1373`) pumps
  `_stored_object_range = ByteRange(0, whole object)` per member (`--range` only trims plaintext
  OUTPUT, not ciphertext INPUT) → N×(mount+whole-object-stream+decrypt-from-start). The materialize-once
  bundle path (`_extract_rao_bundle_with_rem_to_paths:1616-1639`) does it right (O(1) tape) but the live
  per-member streaming path bypasses it (AEAD members `buffered=False`).

## 3. Design

### 3.1 App-restart session contract (remanence proto + read path)
Extend `OpenReadSessionRequest` with an optional **resume target** `{object_id, file_id, byte_offset,
expected_position_proof?}` and surface the **`DevicePositionProof`** to the caller (a new field on the
ranged-read response / a `GetReadSessionPosition` RPC). On resume-open: locate via `space()` to the
object/file/offset, issue a Read-Position proof, and RETURN it so the caller (sutradhara→agent) can
verify continuity against `expected_position_proof`. **Guarantee decision (review):** the app-restart
proof = **correctness-of-restart + position-proof continuity** (the drive is physically where we asked,
and the returned proof matches the expected), NOT full corruption-detection — the RAO per-file SHA-256
(RM0 verified-sink + the agent's committed-file rehash) already owns content integrity, so RM3's
contract need not re-derive it. This is deliberately weaker than the in-session park-RP and cheaper.
The resume target aligns with RM1's file-boundary `committed_index` (agent resume_token → sutradhara
→ remanence resume-open at the committed file's tape position).

### 3.2 Per-drive arbitration surface (expose the existing reservation)
Add a queryable **drive-assignment surface**: a new field on `GetLiveStatusResponse` (or a
`GetDriveAssignments` RPC) exposing per-drive `{drive_uuid, bay, state: idle|active, current_session,
queued_count}`, keyed on a stable **`drive_uuid`** (resolved from the catalog,
`mount.rs:761-767`) with `bay` as the physical slot. sutradhara's `RestoreService` admission consults
this to **QUEUE** tape restores (one active `ReadSession` per drive_uuid) instead of getting a
`FailedPrecondition` on the 2nd attempt (§8-B2). The existing bay-level `DrivePool` reservation stays
the enforcement mechanism; RM3 adds the read-side visibility + the drive_uuid key + sutradhara-side
admission queueing. Drive↔bay float (swap/retire, `mount.rs:747-760`) is an operational event —
document it; the reservation stays bay-keyed, the surface reports drive_uuid.

### 3.3 Measured backpressure (test harness, not new hot-path state)
Do NOT add persistent live state to the hot drive-actor loop (TIO-6 R2 just landed there). Instead: a
**structured diag emission** (upgrade the `remanence_read_diag` `tracing` lines to a stable structured
event / a JSON diag line per pipeline close) + a **log-scraping acceptance harness** that drives a real
slow-consumer restore across the two-hop chain (agent→sutradhara→remanence) and asserts, from the diag,
that the reservoir parks the drive (park_cycles ≥ N) rather than shoe-shines, and that occupancy stays
bounded. Acceptance = the physical MSL3040 leg (measured, not asserted) — ties to
`design-restore-agent-v0.1.md` §8-M2 (the in-flight budget < reservoir hysteresis) + §15.

### 3.4 extract-stream timeout fix (cold mount+seek)
Two-part: (i) a distinct **first-byte grace period** (configurable, e.g. `mount_seek_grace_seconds`,
default generous — cover LTO load + locate-to-far-filemark under library contention) separate from the
post-first-byte 120 s inactivity; (ii) BETTER — expose the `OpenReadSession` mount+seek as a **pollable
operation** (return an `OperationRef`, or a mount-progress signal) so sutradhara resets/extends
`last_activity` on real progress instead of a blind fixed grace. Review picks (i) simple-now vs (ii)
correct. The pump thread (`pump_ciphertext`) and the poll loop must agree the clock only starts after
the mount/seek phase.

### 3.5 Encrypted-bundle O(N) fix (wire the built ranged decrypt)
Add a **ranged-ciphertext mode to `extract-stream`** (or a new `extract-stream-range`): instead of
whole-object `open()`, use `range.rs open_plaintext_range*` to fetch+authenticate ONLY the covering
ciphertext chunks — driven by `stored_range_start/len` (`range.rs:116-124`) mapped to
`ReadObjectRangeRequest.start_byte/end_byte`. sutradhara computes the member's stored ciphertext range
(not `_stored_object_range`'s whole object) and streams only that. **Preserve the RM0 bounded-memory
contract** — the ranged decrypt must consume a bounded ciphertext STREAM (a reader), not `range.rs`'s
current whole-`&[u8]` API (add a reader/stream variant). For a bundle: ideally a single linear multi-member
pass (open once, fan out to N destinations for contiguous members) vs N ranged opens. This makes an
N-member encrypted bundle O(object) not O(N×object) on tape.

## 4. Milestones (each → one gpt-5.6-sol prompt; remanence-first)
- **RM3.1 — app-restart contract + per-drive arbitration surface (remanence + sutradhara).** proto:
  resume target + caller-visible position proof + drive-assignment surface (drive_uuid); remanence
  read-path resume-open (locate + return proof); sutradhara admission queueing (one active session per
  drive_uuid, wire the agent resume_token → resume-open). Verify: resume-open re-locates + returns a
  matching proof; a 2nd tape restore QUEUES not fails.
- **RM3.2 — extract-stream timeout fix + structured diag + measured-backpressure harness.** the
  first-byte grace / mount-seek progress signal; structured `remanence_read_diag`; a slow-consumer
  two-hop test asserting drive-park-not-shoe-shine + bounded occupancy. Verify: a simulated cold mount
  doesn't false-kill; the harness measures park behavior.
- **RM3.3 — ranged-ciphertext AEAD extract (bundle O(N) fix).** the `extract-stream` ranged-ciphertext
  mode via `range.rs` (reader/stream variant, bounded); sutradhara computes the member ciphertext
  range; multi-member linear pass. Verify: an N-member encrypted bundle restores with O(object) tape
  reads (not O(N×object)); bounded RSS preserved; per-chunk auth intact.
- **Physical acceptance:** the MSL3040 leg — measured throughput + park behavior on a real 2-hop
  encrypted tape restore (ties to the TIO-6 physical legs).

## 5. For the panel / code-grounded review
1. **App-restart proof guarantee (§3.1)** — is "re-locate + return DevicePositionProof for continuity"
   the right level, given RAO per-file SHA owns content integrity? Or does a preservation-grade restart
   need more? What must the resume token persist (survive daemon restart)?
2. **drive_uuid vs bay keying (§3.2)** — is exposing drive_uuid while reserving on bay coherent when a
   drive floats across bays mid-operation? The queueing race (sutradhara sees idle → opens → another
   client races).
3. **Diag surface (§3.3)** — structured log vs a real RPC; is log-scraping robust enough for the
   acceptance gate, or is a minimal `GetReadSessionDiag` RPC worth the hot-path touch?
4. **Timeout fix (§3.4)** — simple first-byte grace vs a pollable mount/seek operation; the latter
   touches `OpenReadSession`'s synchronous contract.
5. **Ranged-ciphertext reader variant (§3.5)** — adding a stream/reader API to `range.rs` without
   regressing its bounded/authenticated guarantees; the multi-member linear pass complexity vs N
   independent ranged opens (simpler, still O(covering-chunks) per member).
