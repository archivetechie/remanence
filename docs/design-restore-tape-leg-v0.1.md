# Design — RM3: restore tape leg (app-restart contract + per-drive arbitration + measured backpressure)

**Status:** design v0.2 — panel folded (Claude 2026-07-12). **Build to §6 (the fold); §§3-5 below are
the SUPERSEDED v0.1 where §6 corrects them.** Panel = Opus code-grounded (SOUND-WITH-FIXES, authoritative)
+ GLM prose (NEEDS-REWORK). Reconciliation in §6. **RM3** of the restore-agent build — the
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

---

# 6. v0.2 PANEL FOLD (BINDING — build to this; §§3-5 superseded where corrected here)

**Reconciliation.** Opus reviewed code-grounded (file:line-verified) → **SOUND-WITH-FIXES, no blockers**;
GLM reviewed prose-only → NEEDS-REWORK (3 blockers). Per [[code-grounding-in-reviews]] the code-grounded
lens is authoritative: **2 of GLM's 3 blockers are refuted or dissolved by the actual code**, but GLM
independently **sharpened 3 real points** (tape identity, drive_uuid/bay desync, fail-fast grace) that
Opus also raised — those are confirmed-by-two. Net: SOUND-WITH-FIXES stands; the fixes below are binding.
(Scoreboard: GLM overstated blockers on the atomic-reservation and async-ripple axes that code inspection
settled; its value was reinforcing 3 majors, not the blocker verdict.)

## 6.1 Ground-truth correction (fixes §2 bullet 2)
`remanence-aead/range.rs` `open_plaintext_range*` is **NOT unwired** — it IS wired to the BUFFERED
`archive extract` path (`remanence-format/src/pfr.rs:85` → `remanence-cli/src/lib.rs:10114`, tested
`lib.rs:18043`). The accurate, narrower claim: the **streaming `extract-stream` path** (`lib.rs:9321/
9335/9348`) does not use it — it calls whole-object `open()` and trims plaintext OUTPUT only. This
STRENGTHENS RM3.3: the ranged primitive is proven + shipped; RM3.3 is a new SURFACE on a tested core, not
new crypto.

## 6.2 App-restart resume token (corrects §3.1) — confirmed-by-two: bind tape identity
The resume token persists **session-independent, durable** coordinates and re-opens COLD (no session_id
is ever persisted — resume always mints a fresh session): `{tape_uuid, object_id, file_id,
file_boundary_byte_offset, expected_position_lba, daemon_epoch}`.
- **ADD `tape_uuid` (both reviewers).** `object_id` does not identify the mount target (`OpenReadSession`
  takes a TapeTarget); a swapped/stale tape can present a matching physical LBA for the WRONG tape.
  Resume MUST verify the mounted tape's identity (library barcode/uuid) BEFORE trusting the position
  proof — physical-position continuity is necessary but not sufficient; tape-identity binding closes it.
- **Resume granularity is FILE-BOUNDARY, never mid-file (resolves GLM Major 2).** RM1's `committed_index`
  is per-file; a partially-received file re-streams from its START. So the per-file RAO SHA-256 is ALWAYS
  computable on the agent (it never holds a half-file it can't hash) — the weaker position proof
  (correctness-of-restart, not corruption-detection) is sound precisely because integrity lives at the
  file layer and resume never lands mid-file. `file_boundary_byte_offset` is chunk/file aligned.
- **Proof wire form:** reuse the proven WRITE-side precedent — `AppendCommitInfo.position_after_lba`
  (`proto:613`, a `uint64` LBA); the write path already has `OpenWriteSessionRequest.recover_session_id`
  (`proto:960-961`) + `APPEND_MODE_RESUME_CONTROL`. §3.1 is PORTING a shipped write-side resume pattern
  to reads, not inventing one. `DevicePositionProof` is internal today (`read_core.rs:163`) — define its
  serialized form via this encoding.

## 6.3 Per-drive arbitration (corrects §3.2) — advisory surface + re-queue, keyed on bay
- **Enforcement is ALREADY race-free** — the bay reservation is an atomic `compare_exchange`
  (`write_owner.rs:239`). So the query surface can only ever be ADVISORY; **no atomic queue-and-open RPC
  is needed** (refutes GLM Blocker 1) PROVIDED sutradhara treats a lost-race `FailedPrecondition`
  (`write_owner.rs:2531`) as **"re-queue," never "fail."** State this explicitly — it is the entire
  substance of GLM's TOCTOU concern and it costs one branch.
- **Queue key = `(library_serial, bay)`, the enforcement unit — NOT `drive_uuid`** (confirmed-by-two).
  `drive_uuid` is a point-in-time attribute of a bay (`mount.rs:761-767`) that can migrate across bays
  when an operator swaps/retires a drive (`mount.rs:747-760`); keying the queue on it desyncs from
  enforcement. Expose `drive_uuid` in the surface as an operator-legibility HINT only.

## 6.4 Measured backpressure (refines §3.3) — stable structured diag, still log-scraped
Keep the log-scraping acceptance harness (Opus: robust enough; a `GetReadSessionDiag` RPC would plumb
hot-path-adjacent `ReservoirState` for no acceptance benefit). Address GLM's brittleness minor by pinning
the close-line as a **stable structured JSON event** (versioned key set: `park_cycles`, `occupancy_bytes`,
etc.) emitted ONLY at pipeline open/close (`read_core.rs:617-626/885-896` — off the per-block hot path);
the harness contracts on the JSON, not on prose formatting. **Never add per-iteration diag to the drive
actor loop** (that WOULD be a hot-path risk).

## 6.5 extract-stream timeout (corrects §3.4) — PULL FIRST; two-phase, no proto change
This is a CURRENT live-path availability bug (a cold LTO mount+locate >120 s false-kills every physical
encrypted-tape leg) — **sequence it FIRST**, ahead of all RM3 remanence work (it is sutradhara-only).
Fix = two clocks in `archive_restore.py`, **no proto change** (rejects the async `OperationRef` option →
dissolves GLM Blocker 3's pump-thread ripple): a generous **mount-phase grace** (`mount_seek_grace_seconds`,
set from MEASURED MSL3040 load+locate latencies) that runs until the backend session opens
(`backend/remanence.py:344` returns after mount), then a separate **120 s streaming-phase inactivity**
clock stamped on each `ReadObjectRange` chunk. **Fail-fast on an explicit library mount ERROR** (GLM
minor) — do not wait out the grace on a dead tape / hardware fault.

## 6.6 Ranged-ciphertext AEAD (corrects §3.5) — mapping stays in Rust; N ranged opens
- **Keep the plaintext→ciphertext offset mapping in Rust** (`range.rs:117` `cipher_offset` + tag padding
  — security-critical). Sutradhara passes the **plaintext** member range; Rust computes the covering
  stored range. Adopt **option (c)**: a small remanence query returns the covering stored byte-range for
  `(object_id, file_id, plaintext_start, len)`; sutradhara issues a bounded `ReadObjectRange(start,end)`
  and pumps only that into a trimming reader/stream variant of `range.rs` (single source of truth, pure
  filter, one extra round-trip). Do NOT reimplement the mapping in Python.
- **N independent ranged opens (one per member) — sidesteps GLM Major 1.** Per-object opens keep each
  `(chunk_index, final_chunk)` nonce unambiguous (no multi-object boundary demarcation problem). N ranged
  opens ALREADY fix the O(N×object)→O(object) BYTE cost. **Defer** the single-stream multi-member linear
  pass (a locate-COUNT optimization only, and exactly where the multi-object boundary ambiguity would
  bite) until a physical leg proves locate overhead dominates.

## 6.7 Corrected milestones + sequence (supersedes §4)
1. **§6.5 extract-stream timeout fix** (sutradhara-only, no remanence change) — unblocks physical
   encrypted-tape legs. FIRST.
2. **RM3.1a — arbitration surface + bay-keyed queueing** (remanence advisory surface over the existing
   atomic reservation + sutradhara `(library_serial,bay)` queue with re-queue-on-FailedPrecondition).
   Lower risk, highest value (prevents the §8-B2 park-oscillation).
3. **RM3.2 — structured diag + measured-backpressure harness** (§6.4).
4. **RM3.3 — ranged-ciphertext AEAD extract** (§6.6, option c; N ranged opens).
5. **RM3.1b — app-restart session contract** (§6.2; the hard durable-token + read-path resume-open +
   proof wire form). LAST among correctness items — it has no consumer until the RM2 agent resume path
   lands.
6. **Physical acceptance:** MSL3040 leg — measured throughput + park behavior on a real 2-hop encrypted
   tape restore.
