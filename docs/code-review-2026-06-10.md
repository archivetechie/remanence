# Whole-project code review — 2026-06-10

**Scope:** all workspace crates except `remanence-chaos`, reviewed at the working tree of
2026-06-10 (including the uncommitted api/state/cli changes). ~95k lines of Rust.
**Method:** 11 parallel review passes — one per crate (the two largest crates split in
half), plus dedicated passes for test-suite depth and for cross-cutting
architecture/security/documentation. Every finding was verified against the actual code
(and, for the SCSI layer, against the HPE/IBM reference PDFs in `docs/`); line numbers
are real as of this date.
**Review dimensions:** architectural cleanliness, code quality, Rust idioms,
robustness, extensibility, test depth, security, and documentation (especially "why"
comments that protect design decisions from future refactoring).

---

## 1. Executive summary

Remanence is in unusually good shape for a pre-alpha system of this ambition. The
layering documented in spec-v0.4 §3.2 holds in the *actual* `cargo metadata` dependency
graph, not just the diagram; the journal durability story in `remanence-parity` is
genuinely sound (fsync placement, torn-tail replay, rollback all verified); discovery's
read-only property is structural (data-direction split on the transport trait) and
pinned by a test; unsafe code is confined to three verified-sound SG_IO blocks; the
test suite (1,046 tests, hermetic, clippy/fmt clean) reaches top-decile fault-injection
depth in the parity crate; and the "why"-comment culture — spec citations, review
provenance, failure-mode rationale — is the best part of the codebase.

The weaknesses are concentrated, not diffuse:

1. **The Layer 5 orchestration layer is the soft spot.** `remanence-api` scored 4/10 on
   robustness — one Critical data-clobber path (parity append), cancellation-unsafe
   RPCs that leak drive reservations, and zero tests on `mount.rs` (the code that
   physically moves tapes, currently under active modification).
2. **Degraded-mode code is stricter than its own design docs.** The recovery scanner,
   sidecar footer handling, and BRU reader all hard-abort on single-block damage in
   exactly the scenarios they exist to survive — each contradicting a specific design
   section.
3. **Three "rebuildable/recoverable" promises don't hold yet:** catalog rebuild destroys
   provisioning state; resume loses pre-crash sidecar directory entries; restores never
   check the per-file SHA-256.
4. **Two verified hardware-protocol bit bugs** in the SCSI layer (DVCID/CurData swap,
   IE-port flag masks), one of them pinned by a unit test asserting the wrong encoding
   and "explained" by an incorrect empirical-lore comment.
5. **Security is strong at the bottom, unwired at the top:** mTLS client certs are
   mandatory, SQL fully parameterized, restore traversal-safe — but the unauthenticated
   Unix socket serves the full mutating API with no socket permissions, there is no
   authorization or client identity in the audit log, and the spool cap is
   client-controlled.
6. **Documentation rot at the entry points** (no root README; `docs/README.md` claims
   Layer 5 doesn't exist; spec §0 three months stale) against excellent depth docs.
7. **Throughput:** the parity encode hot path (bitwise GF(2⁸) + bitwise CRC-64) and the
   per-block READ POSITION round-trips cannot stream an LTO-9 drive; this cluster will
   defeat the purpose of the block-size work.

**No CI exists.** Given the codex-implements/Claude-reviews workflow, a push-triggered
fmt+clippy+test run is the single cheapest robustness win available to the project.

---

## 2. Consolidated scorecard

Scores are per the reviewing agent for each area, /10.

| Crate / area | Arch | Idioms | Robust | Extens | Tests | Security | Docs |
|---|---|---|---|---|---|---|---|
| remanence-scsi | 8 | 8 | 7 | 8 | 8 | 8 | 8 |
| remanence-library (L2) | 9 | 8 | 7 | 7 | 9 | 8 | 9 |
| remanence-library (handle/tape_io) | 7 | 7 | 8 | 7 | 8 | 8 | 9 |
| remanence-parity (write) | 7 | 7 | 8 | 7 | 8 | 7 | 8 |
| remanence-parity (recovery) | 8 | 8 | 7 | 7 | 9 | 8 | 8 |
| remanence-format/bru/stream | 7 | 8 | 6 | 6 | 7 | 7 | 7 |
| remanence-state | 7 | 7 | 7 | 6 | 7 | 8 | 8 |
| remanence-api | 8 | 7 | **4** | 7 | 7 | **5** | 7 |
| remanence-cli/daemon | 7 | 7 | 6 | 6 | 7 | 7 | 8 |
| Whole-system architecture | 8 | — | — | — | — | — | — |
| Whole-system security | — | — | — | — | — | **5** | — |
| Whole-system documentation | — | — | — | — | — | — | 6 |
| Test suite overall | — | — | — | — | 7.5 | — | — |
| Operations (CI/observability/runbook) | — | — | — | — | — | — | **5** (ops) |

---

## 3. Critical and High findings (ranked)

### C1. Parity-tape appends rewrite the tape from BOT and corrupt catalog locators
**[Critical] [Robustness] `crates/remanence-api/src/pool_write.rs:946`**
Every `AppendFinish` constructs a fresh `ParitySink::new_sidecar_only` and calls
`parity.write_bootstrap()` (pool_write.rs:947-950), and `prepare_drive_for_write`
leaves the drive at BOT at session open (rewind → verify → rewind,
write_owner.rs:842-857). On hardware: (a) opening a second write session on a
partially-written, unsealed parity tape physically overwrites it from block 0 — tape
writes truncate everything downstream — while the catalog still lists the old objects
as committed; (b) even within one session, a second `AppendFinish` writes a duplicate
bootstrap mid-tape and reports tape-file numbering relative to a fresh tape, so the
catalog bundle upsert (pool_write.rs:1131-1157) collides and reads via
`space(tape_file_number)` (read_core.rs:34) land on the wrong file. The no-parity path
has an explicit guard (`check_writability_preconditions` pool_write.rs:835-839;
`ensure_selected_tape_accepts_write` pool_write.rs:1584-1601) and the roadmap documents
that TODO (layer5-roadmap.md:188-198) — but parity tapes have **no** guard, and the
default `CompleteOrFill` policy actively prefers partially-filled tapes
(pool_selection.rs:80-120; test lib.rs:2772-2803 proves a used parity tape is
selected). Phase-1 `VecBlockSink` tests can't see this because each test gets a fresh
in-memory sink. **Fix:** until resume-append is wired
(`ParitySink::new_sidecar_only_from_resume` already exists), extend the
`total_committed_ordinals > 0` refusal to parity tapes and enforce one object per
session, or position to EOD via the committed prefix before writing.

### H1. DVCID and CurData bits are swapped in the READ ELEMENT STATUS CDB
**[High] [Correctness] `crates/remanence-scsi/src/read_element_status.rs:197-203`**
The builder sets `byte6 |= 0x02` for `dvcid` and `0x01` for `curdata`; the HPE MSL
SCSI Reference (CDB table, PDF p.119) defines byte 6 bit 0 = DVCID, bit 1 = CurData —
backwards. Masked on the primary path because production callers pass both flags
(0x03 either way), but: (1) the discovery fallback ladder's second rung
(`remanence-library/src/discovery.rs:276`, `issue_res(t, 4, true, false)`) actually
transmits DVCID=0/CurData=1 — a request that can never return drive serials, so that
rung is dead code that will never gap-fill a bay; (2) the "empirical firmware
behavior" lore at read_element_status.rs:176-182 is an artifact of the swap — that
combination never set the DVCID bit at all — and the wrong lore has propagated into
fixture comments (:474-475) and the layer-2 design docs; (3) `cdb_layout_matches_spec`
(:498-499) asserts the swapped encoding, locking the bug in. **Fix:** swap the masks,
fix the test, rewrite the lore comment, re-validate the ladder rung against the VTL.

### H2. Catalog rebuild silently destroys provisioning state — every tape becomes unwritable
**[High] [Architecture/Robustness] `crates/remanence-state/src/index.rs:2066`**
`clear_rebuildable_tables` deletes `tapes`, and journal re-ingest repopulates rows
with no `voltag`, no `pool_id`, `state='ingested'`. But `voltag` and `state='ready'`
are written **only** by `provision_tape` (index.rs:1967-1995), which is neither
audited nor journaled. The write gate requires `state == "ready"` plus a voltag
(remanence-api/src/pool_write.rs:823, 840-843) and pool membership re-derives from
voltag rules at open (state.rs:384). After `rem rebuild-catalog-from-journals` (wired
at remanence-cli/src/lib.rs:2837) or the future daemon `startup_replay`
(state.rs:150-159): every provisioned tape is write-ineligible, written tapes lose
pool membership, and provisioned-but-unwritten tapes vanish. The
rebuild-equivalence test passes only because its fixture never provisions a tape.
This breaks the central "SQLite is a rebuildable projection" promise. **Fix:** make
provisioning an audited event and replay it during rebuild, or have rebuild
preserve/merge the provisioning columns.

### H3. Catalog-less scan aborts on one unreadable head block — the circular recovery failure §8.1 forbids
**[High] [Robustness] `crates/remanence-parity/src/scan.rs:148`**
In `scan_reconstruct_filemark_map`, a medium error reading any tape file's first block
propagates out of the whole scan, making the tape unmappable without a catalog. Design
v0.7.2 §8.1 specifies "first block bytes **if readable**" and classification by
elimination precisely so "an object whose first block is the very block that needs
parity recovery must still be locatable… a circular recovery failure". The footer/tail
probe already tolerates read failures, so the fix is contained: on head-block read
error, skip header classification, run the footer/tail probe, fall through to
object-candidate. No scan test currently injects a medium error (only structural byte
corruption), which is why this is invisible to the suite.

### H4. Sidecar footer is a single point of failure despite two intact header copies
**[High] [Robustness] `crates/remanence-parity/src/recovery.rs:858-871`**
`read_and_parse_sidecar_index` reads only the last block; any footer failure returns
`SidecarMetadataUnavailable` without trying the primary header at fixed block 0. The
normative rule (§5.5/§8.1: at least one header/index copy valid ⇒ recovery-usable)
says footer loss with a valid primary must stay usable — the scan side already
classifies that case. One damaged 256 KiB block silently disables parity recovery for
an entire (~16 GiB) epoch. **Fix:** fall back to parsing block 0 as primary header,
cross-check `sidecar_total_block_count` against the map entry; add the missing
footer-only-damage test (the existing test at recovery.rs:2093 damages all three
copies, never the footer alone).

### H5. Bitwise GF(2⁸) multiply and bitwise CRC-64 on the streaming write path cannot feed an LTO drive
**[High] [Performance] `crates/remanence-parity/src/codec.rs:331`**
Every data block goes through `codec.accumulate` (one `gf_mul_slice_xor` per parity
row, m=4 default) and `data_shard_crc64`. `gf_mul` (codec.rs:331-345) is a
Russian-peasant bit loop; `crc64_xz` (sidecar.rs:289-302) is bit-at-a-time with no
table. Back-of-envelope sustained encode is ~15-40 MB/s/core against LTO-9's
~300 MB/s native rate — guaranteeing the shoe-shining the design exists to avoid, and
nullifying the tape-block-size work. **Fix:** table-driven GF (256-byte
per-coefficient tables, ~10×) or the already-imported `reed-solomon-erasure` SIMD
kernels; slice-by-8 CRC-64 or the `crc` crate. At minimum record the measured ceiling
in codec.rs so it's a known decision before the first hardware soak test.

### H6. Resume-seeded sink loses pre-crash sidecar directory entries
**[High] [Correctness] `crates/remanence-parity/src/sink.rs:771`**
`new_sidecar_only_from_resume` seeds `sidecar_directory_entries` only from
`resume_result.sidecars_emitted`; committed-prefix sidecars are never represented.
(a) `SidecarEpochDirectory::validate` (parity_map.rs:143-148) then fails
`checkpoint()`/`write_bootstrap()` immediately after a resume that emitted no sidecars
— the common crash-mid-epoch case; (b) once a new sidecar is emitted, every subsequent
bootstrap/parity_map directory — including the **final root-of-trust directory** —
silently omits all pre-crash epochs (observable at tests/scan_round_trip.rs:1035),
losing the per-epoch classification-repair protection §5.6.1 defines. **Fix:** extend
`ResumeWriterSeed` to carry directory entries for the committed prefix (the resume
scan already reads those headers); add a checkpoint-right-after-resume regression
test.

### H7. Client disconnect mid-RPC leaks drive-bay reservations and orphans actor sessions
**[High] [Robustness] `crates/remanence-api/src/mount.rs:150`**
Tonic drops the handler future on client disconnect. The bay reservation is a raw
`AtomicBool` set by `reserve_free_drive()` (write_owner.rs:161-173) with no drop guard
(unlike `TapeReservation`); a drop between reservation and the success/error paths
leaves the bay busy until daemon restart. Dropped after the actor consumed `OpenWrite`
but before `reply_rx` resolves: the actor enters its session loop with a session id
nobody recorded (write_owner.rs:657-665 ignores the failed reply send), permanently
wedging the drive. Same family: `close_write_like` dying between the actor's `Close`
and `finish_mounted_session` (mount.rs:428) leaks the session entry and bay;
`append_object` leaks the finished spool file. **Fix:** RAII guard for the bay
reservation handed into the recorded session; run open/close critical sections in a
spawned task so they complete regardless of disconnect; have the actor own release.

### H8. Append spool cap is client-controlled
**[High] [Security] `crates/remanence-api/src/lib.rs:1030`**
`cap = if declared_size_bytes == 0 { SPOOL_MAX_BYTES } else { declared_size_bytes }` —
a client declaring `u64::MAX` gets an unlimited spool, defeating the 64 GiB cap. No
limit on concurrent `AppendObject` streams either, so even honest caps multiply; the
spool filesystem likely shares a disk with the SQLite index and audit log. This is
the network-facing surface once the mTLS listener is enabled. **Fix:**
`cap = declared.min(SPOOL_MAX_BYTES)` plus an aggregate in-flight spool budget
(semaphore + free-space check).

### H9. Mount compensation skips drive unload, wedging the bay on real hardware
**[High] [Robustness] `crates/remanence-api/src/mount.rs:545`**
When the actor fails after a successful physical load, `compensate_open_mount` issues
only `changer_move(bay → slot)` (mount.rs:545-549) — but MOVE MEDIUM from a
loaded/threaded drive generally fails on real changers (the normal close path unloads
first, write_owner.rs:736-743). The compensation error is swallowed (`let _ =`), the
bay reservation is released, and every subsequent session reserving this bay fails its
slot→bay move against a full drive. **Fix:** route compensation through the drive
actor (unload, then move); surface compensation failures into the snapshot/operation
state.

### H10. Manifest CBOR is not RFC 8949 canonical despite the trust chain depending on it
**[High] [Format/Security] `crates/remanence-format/src/layout.rs:237`**
`encode_manifest` sorts map keys **alphabetically**; rem-tar-v1-design.md §8.1 and
spec-v0.4 §8.7.5 both mandate RFC 8949 §4.2 canonical order (bytewise lexicographic of
the *encoded* form — length-first for definite-length text keys), so canonical order
is `object_id, chunk_size, file_entries, schema_version, …`, not alphabetical. Output
is deterministic for this implementation, but any second implementation following the
spec produces a different `manifest_sha256`, breaking the documented
cross-implementation trust chain. The doc-required byte-identity fixture test doesn't
exist. **This is permanent on-tape format — decide before production tapes:** either
implement encoded-form ordering or amend both docs to define canonical as UTF-8 key
order; add the fixture test either way.

### H11. Readers never validate `format_id`, `schema_version`, or `compression`
**[High] [Robustness/Extensibility] `crates/remanence-format/src/reader.rs:144-219`**
`stream_rem_tar_object` / `parse_rem_tar_bytes` parse any pax tar and return
`global_pax` unchecked; no caller in stream/api/cli checks either. The design is
explicit (format_id must equal `rem-tar-v1`; major version mismatch rejected; any
compression other than `none` → `UnsupportedFeature`). Today a hypothetical rem-tar-v2
or compressed entry restores silently as garbage-with-success. ~15 lines to fix;
also export `SCHEMA_VERSION` (model.rs:13 is missing from the lib.rs:31-34 export
list) so downstream could even implement the gate.

### H12. Restore writes files without verifying `REMANENCE.file_sha256`
**[High] [Security/Robustness] `crates/remanence-stream/src/lib.rs:802-825`**
`FilesystemRestoreSink::end_file` checks only byte count; no restore/recovery sink
compares streamed bytes against the per-file SHA-256 sitting in
`entry.pax_records` (populated by reader.rs:191) — the hash the design calls "the
cryptographic anchor for tamper detection". A bit flip inside a 256 KiB chunk that
misses tar headers restores silently as success. Hashing during `write_file_data` is
nearly free; mismatch should surface at minimum as a damage report.

### H13. Unauthenticated Unix socket always serves the full mutating API, with no socket permissions
**[High] [Security] `crates/remanence-daemon/src/lib.rs:32,89-112`**
The UDS listener is unconditional and registers the same five services (WriteSession,
ReadSession, robotics) as the mTLS TCP listener; no `chmod`, no `SO_PEERCRED` check;
`state_dir` (default socket location) is never permission-set — the only hardened path
in the codebase is the spool dir (0700). In a production mTLS deployment the socket is
a full-control bypass for any local user; spec §12.3.1's threat model explicitly
includes compromised local accounts. **Fix:** chmod socket 0660 + dedicated group
after bind (and/or peer-cred allowlist); create state_dir 0700; consider UDS
read-only-services mode in mTLS deployments.

### H14. No authorization and no client identity anywhere downstream of mTLS
**[High] [Security] whole-system (api/daemon)**
Client-cert verification is mandatory (tls.rs:40-42) — but no code reads peer
certificates/subjects; spec §11.6's four-role model is entirely unimplemented; every
gRPC operation is audited as `AuditActor::System` (lib.rs:513, 2613 are the only actor
assignments). Any cert signed by the client CA gets full write/robotics control, and
the hash-chained audit log can prove *what* but never *who* — defeating the
insider-threat story before S7 lands. **Fix:** plumb the client cert subject (tonic
peer-certs connect-info) into `AuditActor::User/Service` on every state-changing RPC;
even a two-role readonly/full check closes most of the gap.

### H15. No root README, and the de-facto README is materially false
**[High] [Docs] repo root / `docs/README.md`**
`README.md` doesn't exist at the root although `[workspace.package] readme` points at
it. `docs/README.md` states "**Not yet implemented:** the Layer 5 gRPC API" while
S1/S2/S3a/S4a/S5a/S6a/S6b are done and a daemon ships; calls the proto "design-only";
its layout tree omits 7 of 11 crates. The front door tells a new contributor the most
advanced half of the system doesn't exist. **Fix:** create a real root README from
`layer5-roadmap.md` (which *is* current); same pass updates spec §0/§11 banner and
proto/README.md (says "Not generated" three lines after saying remanence-api compiles
it).

---

## 4. Cross-cutting themes

1. **Quality is inversely proportional to altitude.** L1-L3 (scsi, library, parity,
   format) are strong; `remanence-api` — the layer that orchestrates everything and
   faces the network — has the Critical, three Highs, the weakest tests (mount.rs: 0
   tests; the ~420-line drive-actor session loop: never executed in tests), and is the
   only crate missing the `[lints.rust]` block. The chaos-transport seam from the
   recent commits is the natural test harness for the actor loop.
2. **Degraded-mode strictness.** H3, H4, plus BRU's fatal aborts (corrupt continuation
   magic, header-after-valid-magic) and recovery's fatal non-normalizable path: in
   five places the code refuses where the design says "salvage and continue". Worth a
   sweep with the rule "in scan/recover paths, damage is data, not an error".
3. **Confidently wrong comments.** The DVCID lore (H1), the IE bit-map module doc,
   LOCATE's CP/IMMED doc, tape_io's stale "Step 9.7b lands later", the dead sticky-OR
   comment in sink.rs, scan.rs's stale module doc, and the coalescer test comment that
   contradicts the code. In a codebase whose documentation is otherwise its greatest
   strength, a wrong "why" comment is more dangerous than none — each of these would
   actively misdirect a refactor. Worth treating wrong-comment reports as bugs.
4. **Duplication drift hazards.** ~600 lines of identical audit/dirty epilogues in
   tape_io (44 `fire_audit` sites); three independent sense decoders upstream of a
   crate whose job is response parsing (none handle descriptor-format sense); erasure
   policy implemented twice in recovery; `timestamp_from_rfc3339` ×3,
   `status_from_state_error` ×3, `bytes_to_hex` ×4, `host_id()` ×2, hex helpers ×4,
   trim helpers ×3; the CLI command surface in two-and-a-half copies. Each is a
   future fix applied to one copy and silently missed in the others.
5. **Trust-the-device / silent-truncation at Layer 1.** `debug_assert!`-only guards on
   CDB builders (silent 24-bit truncation in release); ILI short-read trusting sense
   INFORMATION over the kernel byte count; unvalidated SPACE residuals; invalid-UTF-8
   serials becoming `""` identity keys. The crate's stated posture is "don't trust the
   target" — these are the residual gaps.
6. **The streaming-throughput cluster.** H5 (bitwise GF/CRC), the unconditional READ
   POSITION after every `write_block` (tape_io/mod.rs:913-915 — doubles SG_IO on the
   hot path) and after every `read_record` (parity raw.rs:407-415, physical_io.rs:131),
   and the RAM-resident triple-copied epoch parity (sink.rs:2107, ~1.5-2 GiB transient
   per epoch boundary; the design's §7 disk spool was never built and
   `ParitySpoolCapacity` models a spool that doesn't exist). These should be fixed as
   one program; the block-size config work addresses none of them by itself.
7. **Accepted-but-ignored wire contract.** Idempotency keys are accepted on every
   state-changing RPC and enforced on almost none (only Cancel/Reconcile decode them;
   library.rs:763-823 hardcodes `None`); `session_proto` always emits
   `drive_element_address: 0`; `library_uuid` has two incompatible encodings between
   LibraryService (UUIDv5) and the write path (raw serial bytes, lib.rs:1195-1199).
   Contract drift now is cheap; after an external client ships it isn't.
8. **Operational wiring below spec.** Spec §12.4 promises structured `tracing`;
   reality is one file using tracing, no subscriber installed, and a homegrown
   `eprintln!` diag format (diagnostics.rs + 10 sites) reinventing it. No CI, no
   production runbook (notably: no "lost the host, rebuild from tapes" walkthrough —
   which, per H2, would currently fail), no systemd unit despite spec §12.3.2
   describing one.

---

## 5. Per-crate findings

Severity tags: [C]ritical, [H]igh, [M]edium, [L]ow, [N]it. Critical/High items above
are not repeated. Line refs are working-tree of 2026-06-10.

### 5.1 remanence-scsi

**Strengths:** exemplary unsafe hygiene (3 SG_IO blocks verified field-for-field
against `scsi/sg.h`, compile-time layout guard sg_io.rs:69-73); defensive resid
handling (range-checked both directions, partial transfer preserved before
CHECK CONDITION bail, sg_io.rs:181-233); hostile-response-resistant RES parser with
per-check attack rationale and adversarial tests (read_element_status.rs:269-391,
657-750); two-phase allocation probe; real MSL3040+QuadStor fixtures; "why" docs with
review provenance throughout.

- **[M] read_element_status.rs:360-361 — EXENAB/INENAB masks off by one bit.** Code
  reads 0x20/0x40; HPE p.129 says ExEnab=0x10, InEnab=0x20 (0x40 is CMC). QuadStor
  fixture IE flags 0x38 decode as export=true/import=false when both are true. Wrong
  module doc at line 30. Latent (no mailslots deployed) but already API-visible in
  `IePort`. No test asserts these fields. Fix masks, doc, add fixture assertions.
- **[M] read_write.rs:42-46 (also space.rs:56-63, write_filemarks.rs:47-50,
  read_element_status.rs:204-217) — CDB builders silently truncate in release.**
  `debug_assert!`-only guards on 24-bit fields; RES build_cdb has no assert at all
  (16 MiB alloc → truncates to 0). Use `assert!` or return `Result` —
  `debug_assert` is for internal invariants, not cross-crate input validation.
- **[M] error.rs:47-52 — no sense parsing in Layer 1; three upstream decoders.**
  `SenseInfo` (library/transport.rs), `fixed_sense_byte2` (physical_io.rs), and the
  chaos crate each re-implement fixed-format-only sense decode; none handle
  descriptor format (0x72/0x73). A `sense.rs` here consolidates the offset math with
  fixture tests.
- **[M] vpd.rs:123-127 (inquiry.rs:202-208) — invalid-UTF-8 serial becomes `""`** and
  flows into identity keys (discovery.rs:172); two garbage-serial devices collide as
  `""`. Return `Option`/`Result` or reject empty-after-trim.
- **[M] sg_io.rs:204-207 — BUSY / RESERVATION CONFLICT / TASK SET FULL conflated into
  `TransportError`** despite a genuinely multi-initiator deployment (dwara2 shares
  the chassis). Add `ScsiError::UnexpectedStatus { status }`.
- [L] sg_io.rs:123-430 — execute_in/none/out triplicate ~70 lines of header setup and
  completion decode; drift already visible in comments. Extract `make_hdr` +
  `decode_completion`.
- [L] sg_io.rs:69-73 — layout guard checks size/align only, x86_64 only; use
  `core::mem::offset_of!` (stable since 1.77, MSRV is 1.80).
- [L] locate.rs:20-23 — CP/IMMED bit positions documented wrong (IBM Table 53: bit 1
  is CP, bit 2 reserved). Hard-coded 0x00 so functionally inert, but a future
  multi-partition implementation would follow the wrong recipe.
- [L] read_element_status.rs:377-382 — strict `elements.len() == num_elements`
  equality is safe only because of the two-phase probe; document the dependency.
- [L] Tests: no coverage of IE flags, AVOLTAG skip path, binary-code-set DVCID,
  non-UTF-8 voltag — i.e. exactly the bits that turned out wrong.
- [N] sg_io.rs:51 garbled timeout comment (`!0 = infinite` vs sg.h's `~0`);
  duplicate trim helpers with a third different semantic in vpd.rs; inconsistent
  root re-exports; stale crate doc ("future element_status"); dead
  `#[allow(missing_docs)]`; blanket `From<io::Error>` mislabels non-ioctl failures
  as "SG_IO ioctl failed".

### 5.2 remanence-library — Layer 2 (discovery/ops/transport/watch)

**Strengths:** structural read-only discovery (transport.rs:16-20) pinned by an
opcode-allowlist test (discovery.rs:777-819); sound fatal/non-fatal error taxonomy
with `is_dirty()` state tables (error.rs:309-395); DVCID fallback ladder with
gap-fill semantics documented and tested; pure-logic core with injected time/IO
(plan/apply split ops.rs:71-81, deterministic coalescer); shape check by address sets
not counts (ops.rs:332-341); `!Send` udev handling documented (watch/linux.rs:24-34);
spec-cited fixture semantics; occupancy/barcode separation designed out of the trap.

- **[M] model.rs:237 — `resolve_load_target` can pick an unresolved/device-less bay
  while a usable one is free.** `find(|bay| !bay.loaded)` ignores `bay.installed`;
  production loads via remanence-api/mount.rs:112 will intermittently refuse
  depending on which bay is free first. Prefer `installed.is_some()` (+
  `sg_path.is_some()`), return `NoFreeDrive` only when no usable bay exists.
- **[M] discovery.rs:464-473 — fatal `SerialAmbiguous` aborts the whole host pass,
  undermining partition independence.** A duplicated serial caused by the legacy
  LTO-7 partition's firmware would blind Remanence to its own healthy LTO-9
  partition. Emit a warning and leave the tape unbound in all claimants — the
  unresolved-bay machinery already refuses operations safely. Message also misleading
  for intra-library duplicates (`["LIB1", "LIB1"]`).
- **[M] watch/coalesce.rs:50-62 — sliding debounce window has no max-age cap;** a
  device flapping faster than the window defers notification indefinitely while
  `touched_paths` grows unboundedly. `first_at` is already tracked — add a
  max-latency condition. The `biased` select in linux.rs:231 compounds this in a
  storm.
- **[M] discovery.rs:461 — `IdentitySource::Derived` is consumer-complete but
  producer-absent.** The whole policy chain exists (model/error/CLI/MovePlan) but
  nothing produces `Derived` or `DriveMappingDerived`; the topology rung the
  identity model describes was never built. Implement it or comment its status at the
  ladder-exhausted point.
- [L] transport.rs:142-195 — `SenseInfo` is a promised-but-never-produced API
  surface; both sg execute paths always return `TransferOutcome::clean`. Wire it or
  collapse with a doc note.
- [L] transport.rs:226-235 — sticky `set_timeout_for` protocol invites misuse; pass
  `TimeoutClass` as an execute parameter so a CDB can't be issued without stating its
  class.
- [L] error.rs:476-772 — audit vocabulary (~300 lines, 40% of "error.rs") isn't
  errors; move to `audit.rs`.
- [L] ops.rs:312-315 — `reconcile`/`SnapshotMismatch` stringly typed; tests assert
  substrings. Two-variant enum keeps Display, restores structure.
- [L] discovery.rs:144 — enumeration failure cause discarded
  (`map_err(|_| EnumerationDenied)`); add the `IoErrorKind`.
- [L] transport.rs:285-295 — read-only open falls back to RW on *any* error, not
  just EACCES as the comment claims; match the code to the comment.
- [L] watch/source.rs:18-27 — receiver contract docs omit
  `SourceUnavailable`-via-channel (linux.rs:91-104 delivers it).
- [L] discovery.rs:362-373 — zero-drive library walks the ladder and emits a
  misleading warning; test comment at :866 contradicts the code.
- [L] discovery.rs:353-356 — phase-2 RES "slack" comment claims headroom that doesn't
  exist (`bc + 8` is exactly the header); degrades safely but fix the comment or add
  real slack.
- [L] physical_io.rs:131-147 — READ POSITION round trip per record doubles CDB count
  for bulk foreign reads, and a position failure discards an already-delivered
  record. Track position locally; resync at seek boundaries.
- [N] sysfs.rs:70-73 silent drop of uncanonicalizable devices, no char-device check
  (undocumented test-enabling decision); coalesce.rs:45-48 zero-window path
  duplicates merge logic with `event.at` vs `now` asymmetry; block_io.rs:415-424
  `VecBlockSource::position()` never reports `end_of_partition` (inconsistent with
  locate/space); error.rs:88 blanket `#[allow(missing_docs)]`; ops.rs:159
  `apply_planned_move` could `debug_assert` plan-matches-snapshot.

### 5.3 remanence-library — handle/ + tape_io (Layer 3a)

**Strengths:** completion-unknown taxonomy with per-variant rationale
(handle/mod.rs:1608-1618, DirtyCause split mod.rs:198-210); spec-cited ILI
two's-complement decode, SPACE residual gating, EW/EOM as `Ok(WriteOutcome)`
(tape_io/mod.rs:786-833, 1834-1854, 1765-1788); MODE SELECT pins non-changeable
fields with IBM citations (1659-1741); identity revalidation on every open; RAII
removal-lock guard with `#[must_use]` rationale; CDB byte-log test assertions; no
unsafe in scope.

- **[M] handle/mod.rs:1785 — 6,585-line mod.rs is 73% inline tests.** Production code
  ends at 1779. Move tests to `handle/tests.rs` (`#[cfg(test)] mod tests;`), consider
  sibling files for ChangerHandle/DriveHandle. Same at smaller scale for
  tape_io/mod.rs (tests from 1856).
- **[M] tape_io/mod.rs:298-320 — audit/dirty epilogue copy-pasted ~15× (44
  `fire_audit` sites, 19 identical dirty-mark blocks).** A `finish_with_error` helper
  or `run_audited` combinator cuts ~600 lines and makes the dirty-marking invariant
  single-sourced.
- **[M] tape_io/mod.rs:510-514 — client-side validation errors masquerade as
  `CheckCondition`** (no CDB issued, no sense bytes, contradicting the variant's own
  rustdoc); `map_scsi` folds parse failures in too (158-160). Add
  `TapeIoError::InvalidRequest` (+ `MalformedResponse`).
- **[M] tape_io/mod.rs:1018-1036 — `BlockTooLarge` documented in detail, never
  constructed.** The "Step 9.7b lands later" comment is stale (9.7b landed at :1291);
  callers matching the documented arm match an unreachable one. Wire it (cache
  `max_block_size_bytes` from `read_config`) or delete it.
- **[M] tape_io/mod.rs:780-835 — filemark-on-read undecoded;** READ at a filemark
  (FM bit, NO SENSE, ILI clear) falls through to opaque `CheckCondition` —
  asymmetric with `WriteEomSignal`/`stopped_at_boundary` and a problem for
  scan/recovery paths. Return a `ReadOutcome { bytes, filemark }` or
  `FilemarkEncountered`.
- **[M] tape_io/mod.rs:48-53 — no position-unknown latch after transport errors;**
  the documented recovery ("refresh/rescan") reconciles the *changer snapshot*, not
  head position; next `write_block` writes wherever the head is. Enforce a
  `position_known` latch or document the real recovery (re-establish with
  `position()`/`locate()`).
- **[M] handle/mod.rs:214 — panicking audit hook poisons the shared mutex** (93
  `expect` sites) bricking every handle in the family; hooks run under the guard.
  Use `unwrap_or_else(PoisonError::into_inner)` (poisoning carries no information
  for these small value writes) or document hooks-must-not-panic + non-reentrancy.
- **[M] tape_io/mod.rs:913-915 — unconditional READ POSITION after every
  `write_block`** doubles SG_IO on the hot path (parity raw.rs calls write_block per
  tape block); LBA is predictable for sequential writes. Add
  `write_block_unpositioned()` or elide RP except on EW/EOM.
- [L] tape_io/mod.rs:787-810 — ILI short-read trusts sense INFORMATION over
  `bytes_transferred` (clamped against overrun but stale-buffer bytes can be
  reported as data); same unvalidated-residual trust in `space` (:574) violating the
  documented `SpaceResult` invariant. One-line `min`/clamp each.
- [L] handle/mod.rs:1180-1183 — `RemovalLockGuard` mutably borrows the changer, so
  load/unload/open_drive are unrepresentable inside the critical section; document
  or redesign with a token.
- [L] handle/mod.rs:800-884 — same-bay double-open is possible (no open-bay
  registry, no O_EXCL); exclusivity is not part of the documented contract — state
  it or enforce `OpenError::BayBusy`.
- [L] handle/mod.rs:1060-1069 — composed load/unload re-derives dirty classification
  that `issue_load_unload` already applied; let the inner layer own it.
- [L] docs/README.md:17 — "303 tests pass" not reproducible (227 lib tests today;
  +95 scsi = 322). Update or drop the number.
- [N] open_drive rustdoc stage labels duplicated ("-- 2." twice); `position()` builds
  the CDB twice; `read_config` audit omits the MODE SENSE CDB (document); missing
  tests for oversize-buffer rejections and `space(EndOfData)` success.

### 5.4 remanence-parity — write/encode side

**Strengths:** uniform commit-point order (blocks → sync filemark → map →
durable-boundary → journal) with abandon-on-failure at each step; journal durability
verified real (single fsync per record, CRC torn-tail replay, rollback-with-truncate
tested via fault injection, flock single-writer, fail-closed volume policy with FUA
rationale journal.rs:721-834); EW pre-admission reserve with ~40-test co-firing
matrix; codec cross-checked against an independent slow implementation + Appendix A
vectors + incremental==batch proof; strict sidecar parsing (HMAC magic, three CRC
levels, reserved-zero checks, dual-copy fallback); pure fully-checked capacity math;
excellent why-docs with design-section citations.

- **[M] sink.rs:2107 — pending epoch parity is RAM-resident and copied three times;
  the design's §7 disk spool does not exist.** ~1.5-2 GiB transient per epoch
  boundary at default geometry; 512 MiB held per pending sidecar;
  `ParitySpoolCapacity` and its operator remedy refer to nothing real. Short term:
  move accumulators into `PendingSidecar` by move, emit streamily; long term:
  implement the spool or document RAM-spool semantics and bound `pending_sidecars`.
- **[M] sink.rs:2532 — block size pinned before validation;** one wrong-length first
  write pins the wrong size, then correct writes fail forever with the unrelated
  "heterogeneous block sizes" error. Validate against `block_size_bytes` first;
  delete the redundant `Option<usize>` pin.
- **[M] sink.rs:2613 — dead v0.2 inline-parity remnants actively mislead:** the
  sticky-OR comment + `Ok(Some(parity_outcome))` arm describe behavior that no
  longer exists (EW from sidecars actually surfaces at `finish_object`);
  `finishing_padding` guards in write_block are unreachable. Delete.
- **[M] journal.rs:519 — writer accepts records the replayer will silently truncate**
  (no `MAX_RECORD_LEN` check at commit; a 64 MiB-4 GiB bundle would be fsynced then
  destroyed at restart). Also W/T monotonicity enforced only at replay — a regressed
  bundle commits fine then renders the journal unloadable (test journal.rs:1592).
  Check both at commit time.
- [L] sink.rs:1514 — control flow dispatched on an error-message substring
  (`"bootstrap CBOR + CRC exceed"`); use a typed variant.
- [L] sink.rs:2183 — two early error paths in `emit_pending_sidecars` drop un-emitted
  sidecars without poisoning; later `finish()` writes a bootstrap over silently
  unprotected ranges. Poison before propagating.
- [L] sink.rs:613 — `FinalGeometry::unprotected_ranges` is structurally dead plumbing
  documented with v0.2 semantics; populate or remove.
- [L] sink.rs:1399 — `next_*_sequence()` are overflow-checked getters that read as
  mutators, with asymmetric increment sites; rename to `peek_*`, unify increments.
- [L] sink.rs:2352 — capacity reserve trusts caller-supplied pending-sidecar state
  the sink could verify; assert the pending-empty invariant.
- [L] journal.rs:484 — writer-side `load_committed(&self)` truncates through a shared
  ref (document); btrfs volumes fail `validate_journal_write_cache` with a confusing
  message (anonymous dev numbers) — fail-closed but needs a hint.
- [L] journal.rs:83 — parity_map journal rows never carry the documented
  `canonical_metadata_hash` (`from_map_entry` hardcodes `None`); thread it or fix
  the doc.
- [L] source.rs:470 — `affected_stripe_count` walks recovery ranges block-by-block;
  closed-form for contiguous ordinals.
- [N] hand-rolled `div_ceil_u64` shadows `u64::div_ceil` with a silent
  `.max(1)` semantic change; the only panic on the write path
  (`expect("emit_parity called before any data write")`) should be
  `ParityError::Invariant`; v0.2 "neighborhood" and v0.4.4 "epoch" vocabulary
  interleaved for the same concept; 6,000-line embedded test module makes sink.rs
  the largest file in the project — move to `sink/tests.rs`; ~15 repetitions of the
  poison+abandon pattern want a guard helper.

### 5.5 remanence-parity — read/recovery side

**Strengths:** erasure-not-poison policy (failures/CRC mismatches → erasure, never
trusted shards; reconstructed block re-verified against sidecar CRC
recovery.rs:417-425); fail-before-I/O fencing (scope/watermark/durable checks before
any tape read); checked arithmetic on all untrusted values; allocations cross-checked
against physically measured block counts (closes allocation bombs); replicated
metadata fallback ladder in scan with epoch-isolation tested end-to-end; resume
durability discipline with decode-what-you-wrote round-trip; forward-compat done
right (minor-version acceptance + unknown-CBOR-key tolerance, reserved-zero
enforcement); offset-precise corruption tests, RS-limit matrices, 30-scenario
crash-window suite, env-gated VTL tests with transport-fault injection.

- **[M] bootstrap.rs:400-407 — known-block-size discovery gives up on first parse
  error** instead of probing remaining copies (the candidate-size path treats the
  same errors as continuable — the asymmetry looks accidental); a damaged BOT region
  with wrong-length records defeats the multi-copy design even with intact copies at
  the 5% marks.
- **[M] recovery.rs:336-394 vs 535-772 — erasure policy implemented twice**
  (single-block vs bulk paths; implicit-zero shards, boundary peers, CRC-then-trust,
  post-reconstruction check all duplicated); same pattern in scan's overlay
  renumbering (scan.rs:659-670 vs 528-541). One shared per-stripe shard-gathering
  function.
- **[M] resume.rs:109, 654 — open-epoch rebuild buffers the whole epoch in RAM**
  (~16 GiB at production geometry vs ~512 MiB if parity accumulators were carried);
  the bridge comment is honest but doesn't state the bound. Also `push_block` clones
  each block needlessly (take by value).
- [L] parity_map.rs:686-697 — `parse_parity_map_tape_file` has a footer SPOF despite
  a fixed-position primary at block 0; in-tree caller degrades gracefully but the
  public fn inherits the SPOF — add fallback or document.
- [L] raw.rs:407-415 — READ POSITION per block on the production source adapter
  (same issue as 5.3; cache LBA, resync at boundaries).
- [L] bootstrap.rs:591-612 / raw.rs:455-493 — descriptor-format sense never
  recognized; if Layer 3a guarantees D_SENSE=0 say so, else bad blocks in the BOT
  window become hard discovery failures on drives configured for descriptor sense.
- [L] recovery.rs:109-204 — region recovery is all-or-nothing (one unrecoverable
  stripe discards everything already reconstructed) and the rustdoc doesn't say so;
  return per-stripe results or document atomicity.
- [N] scan.rs:1-8 module doc understates what the scanner reads (predates the
  footer/tail fallback); scan.rs:134 avoidable per-file block clone
  (`buf.clone()` where `&buf` suffices); bootstrap.rs:500 unchecked multiply on the
  caller tape-size hint (saturating_mul for uniformity); bootstrap.rs:216-226 mixed
  big/little endianness in the header is deliberate-per-design but uncommented —
  one sentence prevents a catastrophic "normalization" refactor.

### 5.6 remanence-format / remanence-bru / remanence-stream

**Strengths:** driver traits match the streaming-boundary doc nearly verbatim
including the prior review's fixes; exemplary untrusted-length handling
(`bytes_remaining()` check before `try_reserve_exact`, reader.rs:336-347); §7.6 pad
solver correct with digit-boundary tests; writer re-hashes streamed bytes (§9.2 "the
second hash is not optional"); python3 `tarfile` interop oracle; thorough restore
traversal defense (component-wise normalization, per-component symlink rejection,
O_NOFOLLOW, three escape tests); BRU damage model the right shape
(deliver-and-flag, truncation→Missing, resync gaps); cross-layer 3b/3c integration
test through the real ParitySink.

- **[M] driver.rs:74-84 — native half of the driver architecture unwired;
  capabilities constant overpromises.** Zero implementors of
  `NativeBodyFormat`/`ArchiveWriter`, no registry; `FormatCapabilities::REM_TAR_V1`
  claims `indexed_file_restore/range_read/verify: true` — none implemented. Build a
  `RemTarV1Format` adapter or `#[doc(hidden)]` the constant until honest.
- **[M] writer.rs:91 — `BodyBlockWriter` discards `WriteOutcome`,** losing
  short-write and EOM signals the `BlockSink` contract documents; rem-tar written
  directly over a drive near EOM records a full block that isn't on tape. Check
  `bytes_written == buffer.len()`, surface `end_of_medium`.
- **[M] pax.rs:62-66 — `with_alignment_pad` loops ~2^51 iterations for
  non-record-aligned offsets** (public fn, no `offset % 512` validation). Add the
  entry check; a safe public fn must not hang for any input.
- **[M] bru/lib.rs:536-560 — top-level file headers trusted on magic alone;** a
  checksum-failed header's garbage fields are parsed and a parse failure after valid
  magic aborts the whole stream — inconsistent with the resync path's own
  `is_resync_file_header` predicate. Route through the same predicate; treat
  magic-but-invalid as gap + resync.
- **[M] bru/lib.rs:636-655 — corrupted continuation magic or dlen aborts the entire
  stream** (one bit flip in one block kills a whole-tape recovery) — the remaining
  piece of archive-recovery-claude-review item 3. Emit damage + gap + resync.
- **[M] stream/recovery.rs:367-392 — recovery sink lacks the symlink/O_NOFOLLOW
  hardening its restore siblings have** (bare `create_dir_all` + bare open into a
  reused multi-pass destination). Share the hardened helpers.
- **[M] stream/recovery.rs:365-366 — one non-normalizable legacy path aborts the
  whole recovery run;** BRU archives commonly contain absolute paths. For `recover`,
  sanitize + record original path + continue (the manifest `path` field exists).
- [L] recovery.rs:389-393 — `set_len` driven by untrusted declared size (u64 from
  on-tape hex); disk-exhaustion DoS on non-sparse/quota'd filesystems. Cap or
  lazy-extend.
- [L] reader.rs:104-107 — materializing reader pre-allocates
  `block_count × chunk_size` with an unchecked u64→usize cast from semi-trusted
  catalog metadata.
- [L] layout.rs:307-324 — no duplicate path/file_id validation across entries; also
  reject payload paths under `_remanence/` generally, not just the exact manifest
  path.
- [L] driver.rs:115-142 — `NormalizedEntry` can't represent symlink targets; add
  `link_target: Option<String>` before a second foreign driver entrenches the
  adapter_state workaround.
- [L] **docs contradiction:** rem-tar-v1-design.md §8.1 (marked "implementation-
  ready") specifies an integer-tagged manifest with per-chunk CRC-64s and `mtime` —
  none of which exist; the implemented manifest matches spec-v0.4 §8.7.5. Mark §8.1
  superseded; record whether per-chunk CRCs were dropped or deferred.
- [N] BRU layout constants have no provenance reference (no BRU doc in docs/);
  `PHYSICAL_READ_BUFFER_BYTES`, the 1024/1792 bounds uncommented (the prior review
  asked for the dlen rationale); tar.rs:88 re-declares the pax-path marker locally;
  back-to-back pax `x` headers silently overwritten; bru read_ascii unchecked
  slicing vs read_hex_u64's `.get()`; whole `nix` crate pulled in for one
  O_NOFOLLOW constant; recovery.rs:322-331 per-record flush — if crash-durability,
  say so.

### 5.7 remanence-state

**Strengths:** lock protocol matches spec §10.3 exactly (flock authority,
diagnostics-only contents, stale-contents test); audit log is a properly framed
hash chain (CRC-64/XZ check value pinned, cross-segment chaining, torn-tail
tolerance vs mid-log hard fail, clock anomaly records); real rollback tests
(mid-transaction failure → zero orphans; failed rebuild preserves prior
projection); wipe-and-rebuild equivalence test encodes spec criterion F; zero SQL
injection surface; pool-safety invariant enforced at the storage layer with tests;
typed adapter boundary (exhaustive matches make new lower-layer variants compile
errors); `no_hosted_database.rs` mechanically enforces the no-standalone-DB
principle.

- **[M] index.rs:2104 — full-journal ingest un-seals sealed tapes**
  (`state = excluded.state` unconditional, vs the live bundle path's
  sealed-preserving CASE at :2321-2325). Use the same CASE; comment which states are
  projection-derived vs operator-derived.
- **[M] audit.rs:470 — partial audit append poisons the segment for the process
  lifetime;** a torn frame followed by a later successful append is mid-log
  corruption → daemon can never start. Truncate back on write error or latch a
  fatal flag until reopen.
- **[M] audit.rs:248 — every open and every midnight rotation replays and
  materializes the entire audit history** (O(total history) time+memory, forever).
  Stream replay keeping only terminal hash/sequence/timestamp; full verification as
  an explicit op.
- **[M] index.rs:3564 — `IndexMigrationFailed` absorbs all SQLite failures**
  (including formatting and range errors); operators can't distinguish "schema
  problem" from "disk dying", and a test already matches the wrong variant. Add
  `StateError::Index { context, source }`.
- **[M] index.rs:2557 — idempotency conflict in the audit log permanently bricks
  startup replay** (conflict aborts the rebuild transaction; only escape is
  `catalog reset`, which destroys the audit history). During replay a conflict is
  historical fact — record and continue; hard-error only on live admission.
- **[M] state.rs:100 — `reset_catalog` erases the authoritative audit log under a
  "catalog" name** (flag says "erases the catalog"; it deletes audit + 3c
  journals). Rename/split (`reset-index` vs `reset-all-local-state`) or archive
  audit segments aside.
- **[M] config.rs:451 — trusted-volume validation skips `state_dir`, the SQLite
  path, and the cache dir** (flock on NFS, SQLite WAL on NFS both silently broken).
  Extend or document the exemption.
- **[M] index.rs:4426 — the only non-trivial migration (legacy pool-membership
  backfill) has zero real coverage** (test opens a fresh DB; backfill SQL never
  exercised). Build a legacy-table fixture in-test. The "idempotent ensure"
  migration convention itself is sound but uncommented — a maintainer expecting
  versioned steps will add a column to `MINIMUM_SCHEMA` only, silently no-op for
  existing DBs.
- **[M] index.rs:352 — no ID newtypes:** tape UUIDs pass as `[u8; 16]`, `Vec<u8>`,
  `&[u8]`, `String` interchangeably; runtime 16-byte checks; `normalize_pool_id`
  duplicated character-for-character (config.rs:469 vs index.rs:3210). `TapeUuid`,
  `PoolId`, `ObjectId` newtypes with ToSql/FromSql.
- [L] error.rs:9 — `StateError::Io` carries a path never shown in the message.
- [L] index.rs:2905 — re-ingest overwrites the commit-time pool snapshot on
  `object_copies` (`pool_id = excluded.pool_id` rewrites history after reconcile);
  use `coalesce(object_copies.pool_id, excluded.pool_id)`.
- [L] index.rs:921 etc. — use `prepare_cached` for repeated queries.
- [L] index.rs:3267 — read-only connection gets no `busy_timeout` (CLI vs daemon WAL
  checkpoint → immediate SQLITE_BUSY).
- [L] audit.rs:1253 / lock.rs:109 — `host_id()` duplicated; hex helper ×4.
- [L] index.rs:23 — `concat!("tape_pool_", "memberships")` grep-evasion is
  uncommented and will be "simplified" back by a future cleanup.
- [L] state.rs:379 — library code prints warnings via `eprintln!`; return in a
  report type or audit event.
- [N] config.rs:549 — `KB`/`MB` parsed as binary multiples (see CLI inconsistency
  below); `validate_segment_date` accepts month 99; `tempdir 0.3` dev-dependency
  deprecated; several dead-until-Layer-5 variants want a `// consumed by layer 5`
  note.

### 5.8 remanence-api

**Strengths:** actor-per-drive architecture matches the multidrive design doc
(blocking SCSI confined to dedicated OS threads, bounded channels, oneshot
replies); pure hardware-free selection policy with compile-time object-safety
assertions; good RAII where it exists (TapeReservation, ExclusiveGuard,
self-cleaning Spool, exclusive-reservation rollback); watch streams subscribe under
the snapshot lock (no missed/duplicate events); layered error-to-Status mapping;
fail-closed tape-init gauntlet exhaustively tested; catalog streaming uses
spawn_blocking + bounded channels correctly; high rustdoc discipline.

(C1, H7, H8, H9 above. Also:)

- **[M] pool_write.rs:1622 — hardware early-warning never reaches the seal
  decision:** `seal_selected_tape_if_needed` hardcodes `early_warning: false` and
  `CountingBlockSink` discards `outcome.early_warning`; a tape hitting physical EW
  below the software watermark stays "ready" and gets selected again. Thread the
  flag through `BlockSinkStats`.
- **[M] lib.rs:1060 / 360-389 — blocking filesystem/SQLite/fsync work runs directly
  in async handlers** (spool chunk writes; `record_request_received` opening a RW
  SQLite connection + audit append + fsync under a std::Mutex). The streaming RPCs
  use spawn_blocking correctly — the rule isn't internalized. One slow fsync stalls
  every in-flight RPC on that worker thread.
- **[M] mount.rs:589 — tape loaded in a non-reserved bay is a deterministic dead
  end:** `reserve_free_drive` always returns the lowest free bay; a tape parked in
  bay 2 can never be opened while bay 1 is free. Resolve the load plan first, then
  reserve the specific bay it names.
- **[M] lib.rs:959 — idempotency keys accepted but never enforced** (proto promises
  dedup; OpenWriteSession never reads the key; a retried open mounts a second tape
  and leaks the first session). Document the gap per-RPC at minimum.
- **[M] operations.rs:48 — OperationRegistry grows without bound** (no
  remove/retain anywhere; periodic RefreshInventory = slow OOM + authenticated
  DoS). Evict terminal entries after a grace period.
- **[M] lib.rs:1622 — `tape_state()` doesn't map "ready" or "sealed"** — every
  writable or sealed tape reports `TAPE_STATE_UNSPECIFIED` to clients.
- **[M] lib.rs:1195 — `library_uuid` has two incompatible encodings across
  services** (raw UTF-8 serial bytes in the write path vs UUIDv5 in
  LibraryService). Converge on UUIDv5 before any external client ships.
- **[M] tape_init.rs:713 — bootstrap trailing filemark likely defeats the
  idempotent no-op on hardware:** the post-bootstrap probe reads at the filemark →
  FM-bit CHECK CONDITION → `Err(_) => true` → `physical_data_past_bootstrap` →
  `RefuseClobber` for a perfectly initialized tape. Space over the filemark or
  treat FM sense as no-data. (Fails safe, but breaks documented re-init.)
- **[M] read_core.rs:168 — a stalled (not disconnected) read client parks the drive
  actor forever** (`blocking_send` into a 16-slot channel, no idle timeout; Close
  queues behind the in-flight ReadFile). Add send_timeout/watchdog.
- [L] diagnostics.rs — eprintln-based structured diag reinvents `tracing` (already
  a workspace dep) without levels/filtering; field sets across the four files
  already diverge. Convert before it calcifies.
- [L] lib.rs:1051 — manual spool cleanup duplicates the Drop impl on six early
  returns; also any `write_chunk` failure (incl. ENOSPC) is reported as
  "exceeds spool size cap".
- [L] lib.rs:1762 / 499-503 — internal paths and error chains leak into Status
  messages; decide a redaction policy before the TCP surface goes live. mTLS is
  all-or-nothing: any valid client cert can run reconcile (which takes *all*
  drives exclusive) — no per-RPC authz (see H14).
- [L] write_owner.rs:607 — the ~420-line drive-actor session loop has no direct
  test coverage (wrong-session-id routing, unload-failure retry, open-while-active
  all untested); the chaos-transport seam is the natural harness.
- [L] mount.rs:237 — load-bearing ordering decisions uncommented: record_session
  must precede `_tape_reservation` drop; finish_mounted_session forgets the
  session even when move-home fails (intentional?); `session_proto` emits
  `drive_element_address: 0` with no "unwired" marker.
- [N] `timestamp_from_rfc3339` ×3, `status_from_state_error` ×3, `bytes_to_hex` ×2;
  lib.rs at 4k lines (five services + 2.1k-line test module) wants splitting;
  `bytes_committed` silently reports 0 on stat failure; stream transport errors
  mapped to `invalid_argument`. Missing `[lints.rust]` + `description` in
  Cargo.toml (only crate without them).

### 5.9 remanence-cli / remanence-daemon

**Strengths:** rem/rem-debug split enforced at runtime and pinned by tests
(refusals before discovery, help surfaces); pre-discovery allowlist gate with
written rationale; excellent testability seam (`run_with_mode` generic over
discovery + writers; in-process gRPC round-trip); DirtyCause-driven recovery hints
with per-flavor tests cross-referenced to design §5.1; mandatory mTLS
(client_ca_root set, optional never enabled) with all-or-nothing config
validation; layered destructive-op friction (`--i-understand`, `--clobber-data`
rejected for dry-run, typed `CLOBBER <voltag>` prompt); 0700 spool; no shell-outs.

- **[M] daemon/lib.rs:45-86 — lost-wakeup race in shutdown:**
  `notify_waiters()` only wakes registered waiters and the `Notified` futures
  register on first poll inside the spawned servers; a shutdown resolving in the
  startup window is silently dropped, the servers never stop, and installed signal
  handlers swallow subsequent SIGTERM — needs SIGKILL. Pin + `.enable()` before
  spawning, or use `CancellationToken`/watch (level-triggered).
- **[M] daemon/tls.rs:36-50 — no permission/ownership check on the TLS private
  key** (world-readable key silently accepted). OpenSSH-style refuse/warn via
  `MetadataExt::mode()`.
- **[M] cli/lib.rs:571-602 — `parse_tape_block_size` (binary) vs
  `parse_record_size` (decimal) disagree about what `1MB` means** — in the same
  binary, for tape geometry where a 24 KiB/MB discrepancy breaks the
  multiple-of-512 rule. One shared parser; binary-only suffixes (reject MB/KB) is
  least surprising for block sizes. (remanence-state's `parse_byte_size` is binary
  too — align all three.)
- **[M] cli/lib.rs:1064-1075 — `rem archive write/read/verify` advertised in help
  but unconditionally refused** (`tape_target()` returns Some for all three →
  rem-mode gate refuses; pool_ops module doc claims "rem archive write"). Hide
  them as compat parsers or drop from the rem enum; fix the module doc.
- **[M] cli/lib.rs:2261-2281 — JSON error envelope discards the gRPC status code**
  (everything becomes `"code": "daemon_client_error"`; scripts must regex
  messages — exactly what the stable contract exists to avoid). Map
  `Status::code()` to snake_case names.
- **[M] cli/lib.rs:71 — default endpoint (`http://127.0.0.1:8443`) can never reach
  a real daemon** (plaintext h2c to the mTLS port; daemon's plaintext surface is
  UDS-only) and the CLI's tonic has no TLS feature, so `--endpoint https://` can't
  work either. The doc's "until the daemon lands" justification has expired.
  Default to `unix:` + socket_path_or_default; decide whether rem ever speaks TLS
  (H14/S7 related).
- **[M] pool_ops.rs:157-268, 595-666 — pool write/read paths bypass the
  dirty-snapshot recovery machinery** (never consult `handle.dirty_cause()`,
  never print hints, leave the tape loaded silently on failure) — precisely the
  situation the hints were built for. Route through the `open_and_run` epilogue or
  comment why leaving the tape loaded is deliberate.
- **[M] cli — no signal handling for long-running tape operations, and the gap is
  undocumented:** Ctrl-C mid-write leaves a partially-written tape, loaded drive,
  dirty snapshot, no hint (process is gone). Minimum: document interruption
  consequences + `rescan` recovery on the destructive commands; better: a flag
  checked between records for clean finish.
- **[M] daemon/lib.rs:29-31 — unconditional stale-socket unlink can hijack a live
  daemon's socket** (second instance silently steals new connections; both pool
  the same drives if not read-only). flock a lockfile next to the socket or
  try-connect before unlinking.
- **[M] cli — command surface exists in 2.5 parallel copies** (`RemCommand` /
  `Command` near-duplicate enums bridged by hand-written From impls, plus a third
  args copy for archive). The two-enum design is defensible for separate help
  surfaces; the third copy and field-by-field clones buy nothing —
  `#[command(flatten)]` shared Args structs (the pattern TapeInitArgs already
  uses).
- [L] `rem libraries --json` predates and ignores the schema envelope (pinned by
  test); two JSON dialects coexist (envelope vs §4 locator lines) — grandfather
  explicitly or migrate; `tape_uuid` spelled bare-hex in locators vs hyphenated in
  catalog JSON.
- [L] lib.rs:1537-1559 — panicking `source()`/`format()` accessors + six
  unreachable! arms rely on dispatch order; split the enum so the panics are
  structurally impossible.
- [L] lib.rs:2753-2823 — hand-rolled i32→name tables duplicate the prost enums;
  match the typed enums for exhaustiveness.
- [L] lib.rs:3150-3154 — dry-run exit code masks failures (`--dry-run` always
  exits 0 even when candidates errored); intentional? comment + distinct code.
- [L] lib.rs:2869-2875 — `catalog reset` without confirmation exits 2, colliding
  with the documented "library not found" meaning.
- [L] tls.rs:36-43 — PEM validity unchecked at load (test pins that garbage PEM
  "builds successfully"); design doc promises unparseable → TlsConfigError. Parse
  eagerly or amend the doc.
- [L] pool_ops.rs:686-692 — `archive read` truncates `--out` before any
  validation (destroys an existing file on every failure path); temp file +
  rename.
- [L] lib.rs:458-479 — two undocumented divergences from cli-design-v0.1: tape
  init (destructive) gated by config allowlist rather than `--allow` and living
  in `rem`; and the doc-promised daemon-ownership lock for rem-debug doesn't
  exist — rem-debug can race a live daemon's drive pool today. Comment both at
  `state_changing_target` + TODO for the lock plan.
- [L] output-contract tests cover 2 of ~10 envelope schemas; no NDJSON tests
  (`rem op watch` unimplemented; Units streaming buffers instead of emitting
  NDJSON).
- [N] `bytes_to_hex` duplicated; `tempdir 0.3` dev-dep; daemon logs "serving
  mTLS" before bind; hyphenated barcodes unrepresentable in
  `parse_tape_init_target`; TOCTOU-shaped exists()+metadata(); three
  `run_*_client_command` wrappers share a collapsible skeleton.

---

## 6. Test suite (workspace)

`cargo test --workspace --exclude remanence-chaos`: **1,046 passed, 0 failed,
13 ignored** (all hardware/memory-gated, documented). Clippy `-D warnings` clean;
fmt clean. Warm runtime 26.5 s (parity lib tests 22 s). No flakiness across two
runs; no timing-dependent sleeps on hot paths; fully hermetic by default.

**Top under-tested risks (ranked):**
1. `remanence-api/src/mount.rs` — 658 lines, **0 tests**, modified in the working
   tree. Mock infrastructure already exists in remanence-library.
2. `write_owner.rs` actor loops (~1,500 lines, 5 narrow tests, loops never
   executed). Chaos-transport seam is the natural harness.
3. No concurrency tests for DrivePool/reservations (single-threaded only) — races
   here mean two writers on one drive.
4. **changer2 (LTO-7/dwara2) fixtures are captured but never consumed** — no test
   that a two-partition inventory is treated as independent or that the wrong
   partition is excluded. This is the project's loudest safety requirement.
5. Standard-tar compatibility gate ⅓ done (python3 only; plan §6.3 names GNU tar +
   bsdtar; no empty/boundary±1 fixtures). `tar -b 512 -xf` is the on-call recovery
   story.
6. No fuzz/property testing on device-input parsers (RES, VPD, tar/pax reader) —
   cargo-fuzz targets are cheap here.
7. Daemon TCP+TLS accept path untested (rcgen in-process handshake is the named
   future add).
8. mode-sense parsers never validated against the captured hardware pages sitting
   in fixtures/real-hardware.
9. State lock cross-process semantics untested (both tests in-process; spawn a
   child).
10. Read-session surface shallow (Tier 1/2 deliberately unimplemented — fine — but
    no scaffolding).

**Unconsumed fixture classes:** log-sense/ and mode-sense/ captures, inquiry
vpd-00/-85/-c0/-cc/-d0 (forward captures), read-element-status-probes/, all
changer2 captures, two earlier capture sets entirely.

**Plan-vs-reality:** §11 demands "real Postgres"; reality is SQLite and
`no_hosted_database.rs` *enforces* the divergence — update the plan doc. §6.3 and
§7 gates missing as above. §15 artifact retention machinery absent.

**Test hygiene:** `scheme()`/`capacity_input()` helpers re-implemented ≥5×;
`PhysicalVecTapeSource` duplicated; no shared test-util crate; python3 hard-fails
rather than skips when absent.

---

## 7. Whole-system architecture, security, operations

- **Layer graph verified clean** (scsi → library → {format, parity} → {state,
  stream} → api → {cli, daemon}); both intentional irregularities documented at
  the seam (block_io.rs:1-33; parity Cargo.toml). Nothing depends on chaos. The
  previous architecture review's headline finding (global hardware lock) is
  demonstrably fixed (Phases 1→3c); mid-stream cancellation (checks only between
  phases) and actor self-healing (bare `std::thread::spawn`, no catch_unwind — a
  panicking drive actor leaks its bay until restart) remain open as Phase 4;
  **SPOOL_MAX_BYTES is now a de-facto 64 GiB max object size** (objects spool
  fully before the drive sees them) — decide whether that's a contract (document
  in spec+proto) or design streaming pass-through.
- **`remanence-api` is becoming a grab-bag:** ~40 internals re-exported solely for
  the CLI's break-glass path; gRPC + actors + mount + pool-write + tape-init in
  one crate; the only crate with no `[lints.rust]`. Split the non-gRPC core out.
- **Spec §3.2 diagram drift:** still marks api/state/format "(planned)", has no
  place for stream/bru/daemon/cli (4 of 11 crates), draws 3b above 3c.
- **Format registry (spec §8.5) doesn't exist** — BruFormat imported directly;
  fine until a second foreign driver lands.
- **Dependencies:** `tempdir 0.3` (unmaintained, drags rand 0.4) in 6 crates'
  dev-deps → `tempfile`; `reed-solomon-erasure` retained for one error variant
  (either use its SIMD kernels per H5 or drop it); `tokio features=["full"]`
  workspace-wide; tonic/prost pinned exact (good); 242 lock packages, modest.
- **Security posture:** privilege model + partition default-deny verified sound
  end-to-end (INSTALL.md → error vocab → AccessPolicy at Library::open →
  per-invocation `--allow`). Gaps are H13/H14 plus: world-readable state dir
  contents (audit log with VOLSERs, SQLite catalog — no set_permissions anywhere
  in remanence-state); idempotency keys ignored (S3); internal paths in Status
  messages; CLI plaintext-only (S7). The systemd hardening spec §12.3.2 describes
  does not exist in the repo.
- **Docs:** supersede scheme half-applied — superseded docs carry no in-file
  banners ("v0.7.2 — implementation-ready" still heads layer3c-design.md), §16.2's
  deletions never happened, and **60+ code comments cite the superseded docs**
  (layer2b ×20, layer3c ×13, layer2 ×11, and layer3c-design-v0.2 ×6 — the last on
  the delete list). Executing the documented cleanup would orphan the codebase's
  why-comments. Recommendation: one-line "Superseded by spec-v0.4 §N" banners,
  drop the deletion plan — the docs are load-bearing comment targets now.
  Journal practice is a genuine strength (17 dated files, consistent schema;
  wobble: 2026-06-07.json is a bare dict breaking the array schema).
- **Operations:** no CI (cheapest win); tracing promised in spec §12.4 but used in
  one file with no subscriber, eprintln diag instead; no production runbook —
  especially no "lost the host, rebuild from tapes" walkthrough (which today
  would fail per H2); root clutter (`lto.txt` 61k-line SCSI dump → docs/reference
  or gitignore; `plan.txt` superseded); all versions 0.0.1, no tags/CHANGELOG
  (fine for pre-alpha, needs a story by v1.0).

---

## 8. Recommended priority order

**Before any further hardware write testing:**
1. C1 parity append clobber guard (small, blocks data loss on the dev VTL today).
2. H1 DVCID/CurData swap + H-adjacent IE masks (small, while the VTL window is
   open to re-validate).
3. H10 manifest CBOR canonicality decision (permanent format; cheapest now).

**Before relying on recovery/rebuild promises:**
4. H2 provisioning survives rebuild; then write the DR runbook that exercises it.
5. H3 + H4 degraded-mode scan/footer fixes + the missing fault-injection tests.
6. H6 resume directory entries + checkpoint-after-resume regression.
7. H12 restore SHA-256 verification; H11 reader format gates.

**Before enabling the network listener for real clients:**
8. H13 socket permissions + state-dir 0700; H8 spool cap; H14 minimal identity
   (cert subject → audit actor) even before RBAC.
9. H7 cancellation-safe RPCs + H9 compensation unload; mount.rs/actor-loop tests
   via the chaos seam.

**Throughput program (one coordinated effort):**
10. H5 table/SIMD GF + CRC; write_block_unpositioned; epoch parity by-move/spool.

**Hygiene wave (mechanical, good codex fodder):**
11. CI (fmt+clippy+test on push); root README + spec §0 sync + supersede banners;
    tape_io audit-epilogue helper; test-module extraction from the two giant
    files; duplicated-helper consolidation; tempdir→tempfile; eprintln→tracing.

---

*Review artifacts: 11 agent reports synthesized 2026-06-10. Previous review:
docs/architectural-review-2026.md.*
