# Layer 3c Design: Tape Parity

**Status:** **v0.5 — implementation-ready; v0.2 implementation
addendum fully integrated.** This release folds
`remanence-3c-implementation-addendum-v0.2.md` into the main spec
in response to external review. It closes the pre-go-live gaps
that prior drafts carried as "accepted v1 simplifications":
sidecar metadata is now replicated (primary + tail + footer,
§5.5); catalog-less map reconstruction is robust to a damaged
sidecar header via a bootstrap sidecar directory and an external
`parity_map` control file (§5.6, §8.1); volatile sidecar deferral
is prohibited and replaced by the **object commit bundle** with a
bounded one-epoch restart invariant (§7.4, §7.8); bulk/epoch
recovery with a memory cap is specified (§9.2–§9.3); drive
hardware compression is a verified hard-false precondition
(§6.5, §11.6, §12); the GF(2⁸) codec is owned in-tree (§13);
and the adoption scope is stated explicitly (§1). The parity
architecture is unchanged and settled. Mock/in-memory and codec
work (RS, sidecar, bootstrap, filemark-map, recovery) is ready to
build.
v0.4.3 added the contract that was genuinely missing for *live*
tape — crash/restart/append-resume (§7.8) and the commit
durability barrier (§7.7) — which an eighth review correctly
identified as the last live-tape gap. (Earlier drafts labeled
themselves "cleared for live tape" prematurely; the resume story
was under-specified until now.) The parity architecture itself is
unchanged and settled. Consolidates the filemark-aware
parity-epoch model (previously `layer3c-epoch-revision.md`, now
superseded) into the main 3c design, superseding v0.2's
uniform-inline-parity model. Sister documents:
`docs/layer3a-design.md` (tape mechanism),
`docs/layer3b-design.md` (tape format),
`docs/3b-catalog-schema-followup.md` (catalog schema),
`docs/rem-tar-v1-design.md` (default body format, **v0.9.3**),
`docs/remanence-testing-plan.md` (cross-layer implementation tests).
Spec reference: `docs/spec-v0.3.md` §5 (on-tape format), §9.4
(write verification policy).

**Open items carried into implementation (none block coding).**
(1) Spool thresholds, bootstrap-copy fractional positions, and
`filemark_overhead_estimate` are akash-specific tuning constants.
(2) The incremental RS encoder must be proven byte-identical to a
batch encode and to Appendix A's vectors — the single hardest
correctness gate (impl step 11.6). (3) The catalog's deferrable
constraint triggers want a real-Postgres prototype. (4) The
open-epoch rebuild re-read (§7.8 step 8) and the commit durability
barrier (§7.7) want validation on real hardware including a
deliberate power-loss cycle (impl steps 11.18a/b, 11.19). (5) The
exact MODE SELECT / MODE SENSE page used to disable and verify
drive compression must be proven on the deployed LTO-9 models
(§6.5, §11.6). (6) Upstream object splitting (§7.4) is the
mitigation for the large-object burst-rewrite cost; whether it is
a firm orchestrator policy or an advisory `SHOULD` depends on
whether the workload contains indivisible multi-TB single objects.

*Items closed in v0.5 (were "accepted v1 simplifications" in
v0.4.4):* sidecar metadata is now replicated and the
"damaged-sidecar-header degrades the whole scanned map" behavior
is eliminated (§5.5, §5.6, §8.1).

**Changes from v0.4.4 (v0.5 — implementation-addendum integration).**
No architectural change. Integrates the v0.2 implementation
addendum, which closed the pre-go-live review findings. The object
file remains a clean pax tar archive; parity remains in separate
sidecar tape files; epochs still span object boundaries; the
catalog remains authoritative for normal operation.

- **Sidecar metadata replication (§5.5).** Every sidecar now carries
  a primary header/index set at the front, a tail header/index copy
  at the end, and a one-block footer locator. A damaged primary
  header no longer demotes a sidecar to an object candidate: the
  scanner recovers classification and full metadata from the footer
  + tail copy. The 512 MiB parity-shard payload is deliberately
  *not* replicated (§16).
- **Sidecar directory and `parity_map` (§5.6).** Bootstraps carry a
  compact `SidecarEpochDirectory` (inline when it fits), or a
  `ParityMapReference` to a new replicated `parity_map` control tape
  file when the directory overflows the bootstrap block. This gives
  conservative/custom schemes the same catalog-less robustness as
  the default scheme; the v0.4.4 "roughly 1100 epochs fits inline"
  assumption is replaced by a computed sizing rule.
- **Catalog-less scan reconstruction (§8.1).** Replaced with a
  scan → directory-overlay → digest-validation algorithm. Map
  validity (structure) and sidecar usability (copy health) are
  separated; copy health is excluded from the canonical map digest.
  One damaged sidecar header now disables only that epoch's parity,
  never the whole tape's. This supersedes the v0.4.4
  damaged-header-degrades-whole-map behavior.
- **No volatile sidecar deferral; object commit bundles (§7.4, §7.8).**
  Completed-epoch sidecars produced by an object are emitted at that
  object's close and committed with the object and its bootstraps as
  one atomic `ObjectCommitBundle`. The hard invariant
  `total_committed_ordinals − highest_protected_ordinal <
  data_ordinals_per_epoch` after any committed bundle bounds restart
  rebuild to at most one partial epoch. Deferral is deferred to a
  future version behind named preconditions.
- **Bulk / epoch recovery (§9.2, §9.3).** Added `recover_region` /
  `recover_ordinal_range` alongside `recover_block_at`, with an
  epoch-grouped planner (load sidecar once, deduplicate peer reads,
  read in physical order) and a memory-bounded epoch recovery cache
  with windowed multi-pass fallback. Replaces the 4-stripe LRU as
  the contiguous-damage recovery path.
- **Drive compression hard-false precondition (§6.5, §11.6, §12).**
  Layer 3a must disable *and verify* LTO hardware compression before
  any parity-protected write, recorded in the bootstrap/catalog; a
  tape recording `drive_compression=true` is refused for 3c recovery.
- **Owned GF(2⁸) codec (§13).** The Appendix A RS math is implemented
  in-tree; `reed-solomon-erasure` is an optional accelerator gated by
  a byte-identical conformance test.
- **Object size / no-spanning preflight (§7.5, §11).** Objects too
  large for an empty tape after parity + reserve are rejected before
  any block is written.
- **Overhead terminology (§11).** Both `m/k` and `m/(k+m)` are named
  and their uses distinguished.
- **Scope statement (§1) and on-tape format docs (§10.5).** 3c
  protects only 3c-written tapes; every production tape set should
  carry a plain-text copy of the format docs as a normal tar object.
- **Hardware proof gates (§14).** Sidecar-damage, parity-map,
  catalog-less, bulk-recovery, power-loss/bundle-restart, filemark
  durability, and compression-disablement tests must pass on mock,
  QuadStor, then scratch LTO media before live use.

**Changes from v0.4.3 (ninth review — implementation-hardening patch).**
No architecture change. This pass tightens the last API/operational
contracts that code would otherwise have to infer:

- **Capacity reserve error now has a cause (§7.5, §12).** Tape
  capacity failure and parity-spool-capacity failure are distinct
  operator remedies, so `CapacityReserveExceeded` records
  `TapeCapacity` vs `ParitySpoolCapacity` and carries tape/spool
  numbers as optional fields.
- **Resume-emitted sidecars commit like ordinary sidecars (§7.8).**
  Open-epoch rebuild can emit sidecars before appending new object
  data. Those sidecars now have explicit catalog transaction
  semantics and a `ResumeAppendResult`, so resume is not a hidden
  special case.
- **Bootstrap discovery threads block size explicitly (§8.1).**
  `try_read_bootstrap_at` now takes the candidate/configured block
  size; a fixed-block read must return exactly that size. Short
  records are hard raw-I/O/format errors.
- **Scan reconstruction validates non-object headers (§8.1).**
  Bootstrap/sidecar classification requires valid magic *and* the
  relevant CRC/header checks. Anything else is an `object_candidate`.
- **Testing plan split out.** `remanence-testing-plan.md` defines the
  brutal cross-layer gate for mock tape, QuadStor, live scratch tape,
  crash windows, recovery, tar compatibility, and catalog invariants.

**Changes from v0.4.2 (eighth review — live-tape resume contract).**
No architectural change; adds the missing operational contract:

- **Commit durability barrier (§7.7).** `RawTapeSink::write_
  filemark` is now specified as a synchronous flush barrier
  (SCSI IMMED=0 / Linux MTWEOF, never the no-flush MTWEOFI), and
  the catalog-commit ordering is fixed: blocks → synchronous
  filemark → capture position → commit catalog row. Committing a
  catalog row before the durable filemark is forbidden.
- **Restart / append-after-crash (§7.8).** The append point is the
  trailing filemark of the last *catalog-committed* tape file
  (object, sidecar, or bootstrap) — not the last object, not
  derived from `highest_protected_ordinal`. Includes the
  **open-epoch rebuild** (Option A, default): on resume with
  `W < T`, re-read committed object blocks over `[W, T)`,
  re-accumulate parity, emit any now-complete-epoch sidecars, and
  load the partial epoch as the live `EpochState`, preserving the
  clean-finish-protects-everything guarantee. Option B (abandon
  `[W, T)`) is an explicit damaged-tape override only.
- **Impl/tests:** steps 11.18a (durability barrier) and 11.18b
  (resume + open-epoch rebuild, with seven crash-window tests);
  11.19 live smoke now includes a power-loss/restart cycle.

**Changes from v0.4.1 (seventh review — 3c-side patches).** No
architectural change:

- **Commit wording aligned with the parity-lag model (§7.5.1).**
  The v0.4.1 EOM "guiding rule" read as if an object had to be
  fully parity-protected to be committed, contradicting §7.2.1's
  `pending`/`partial` states. Reworded: an object is
  *data-committed* once its archive + filemark are durable and
  the catalog records its accurate `parity_state`; full
  protection (`ordinal_end_exclusive <= highest_protected_
  ordinal`) is reached later when a sidecar advances the
  watermark.
- **Sidecar index sizing wording (§5.5).** Clarified that the
  "whole entries" / no-split-entry rule applies to a
  *heterogeneous* packed stream (16-byte parity entries then
  8-byte data-CRC entries), not a uniform-width array.

**Changes from v0.4.0 (sixth review — pre-live-tape patches).**
No architectural change; wire/contract tightening required before
hardware:

- **Bootstrap payload-length CRC coverage (§5.6).** Moved
  `cbor_payload_len` ahead of `crc64_header` and gave the
  bootstrap a strict byte-offset table; the length is now inside
  the header CRC, so a corrupt length is caught before it bounds
  the payload read.
- **EOM/early-warning handling (§7.5.1).** A normative table for
  what `ParitySink` does when the drive asserts EW/EOM during
  object data, the object filemark, sidecar, or bootstrap writes —
  centered on the rule that an object is committed only once its
  data, filemark, and protecting parity are all durably on tape.
- **Object filemark in the capacity reserve (§7.5).** The reserve
  now counts the current object's trailing filemark explicitly,
  and clarifies that sidecar/bootstrap reserve entries include
  their own filemarks.
- **Provisional map-entry semantics (§7.5.2).** Sidecar/bootstrap
  map entries are provisional until their tape file + filemark are
  durably written; on write failure the session is dirty, no final
  bootstrap or catalog commit is made, and recovery falls back to
  the last committed (prefix) bootstrap — the same mechanism as
  §8.1 prefix validation, viewed from the write side.
- **Non-blocking:** strict bootstrap offset table (above);
  normative sidecar index sizing formula and no-split-entry
  invariant (§5.5); block-size-fallback drive-reconfiguration
  mechanics (§8.1); unique-sidecar-per-epoch catalog index;
  stale "v0.3" labels in §13.2 corrected to v1.

**Changes from v0.3.4 (fifth implementation-readiness review).**
Exact API and wire-format tightening; no architectural change:

- **Raw I/O outcome types (§4.5).** `RawTapeSource::read_record`
  returns `RawReadOutcome` (Block / Filemark / EndOfData) so
  catalog-less scanning can tell records apart; `RawTapeSink`
  write methods return `RawWriteOutcome` carrying physical
  position + early-warning/end-of-medium. `SpaceFilemarksOutcome`
  added.
- **Body vs raw write outcome (§6.1).** The body-facing
  `BlockSink::write_block` returns `BodyWriteOutcome` (object-local
  `BodyPosition` only); the raw `RawWriteOutcome` (physical
  position, EW/EOM) is consumed by 3c internally and never leaks
  to body formats. The body `BlockSink` has no `write_filemarks`.
- **Bootstrap block-size discovery (§8.1).** `TapeGeometryHint`
  carries `configured_block_size` (normal path: Layer 3a / catalog
  knows it) plus a `candidate_block_sizes` fallback for a tape of
  unknown provenance, resolving the read-before-you-know-the-size
  circularity.
- **Strict sidecar binary layout (§5.5).** Exact byte offsets,
  little-endian widths, CRC field positions, and packed
  parity-index (16 B) / data-CRC (8 B) entry layouts.
- **Prefix-map trust boundary (§6.2, §8.1).** `ScopedFilemarkMap`
  records `validated_prefix_tape_files`; `open_object` takes an
  `OpenTrust` and opens an unvalidated-suffix object only in
  tar-only, no-parity, explicitly-unauthenticated mode.
- **Damaged-sidecar-header semantics stated (§8.1).** v1 accepts
  that a damaged sidecar header defeats catalog-less map
  validation for the scanned map; fall back to catalog or another
  copy.
- **RS conformance vectors (Appendix A.7).** Fixed encode and
  reconstruct vectors plus the CRC-64/XZ check value, computed
  from the definitions, so an independent implementation can self-
  test. Versioning moved to A.8.
- **Catalog:** numeric CHECKs; `catalog_files`/object tape-file
  denormalization-drift trigger note; hardlink target FK to
  `catalog_files`; a partial index for the parity-state range
  update; and a staged `NULL → backfill → SET NOT NULL` migration
  for populated catalogs.

**The `layer3c-epoch-revision.md` proposal is superseded** by
this document; its decisions all live here.

**Changes from v0.3.3 (fourth implementation-readiness review).**
Architecture unchanged; these are API and wire-format contract
fixes — the kind of thing that must be exact before code:

- **`RawTapeSink` added; raw vs body-facing traits fully split
  (§3, §4.5, §6).** `ParitySink` wraps a `RawTapeSink` (not a
  body `BlockSink`) and *implements* the body-facing `BlockSink`;
  `ParitySource` wraps a `RawTapeSource`. v0.3.3 had `RawTapeSource`
  for reads but still wrapped a body `BlockSink` for writes while
  claiming body sinks return `BodyPosition` — contradictory.
- **Sidecar block-0 CRC gap closed (§5.5).** Added `block0_crc64`
  covering the header AND any inline index entries; previously
  index entries packed after `header_crc64` were uncovered.
- **All 3c CRCs unified to CRC-64/XZ (§5.5).** The bootstrap's
  `crc32_*` fields contradicted the CRC-64 rule; everything is now
  CRC-64/XZ with an explicit parameter tuple and the `123456789`
  check value `0x995DC9BBDF1939FA`.
- **RS appendix corrected (Appendix A).** Removed the false claim
  that `0x11D` is the AES field (AES is `0x11B`); fixed the Cauchy
  seeds to `Y_i = i, X_j = k+j`, disjoint for ALL `k+m ≤ 255` (the
  v0.3.3 `X_j = 0x80+j` overlapped `Y_i` once `k > 128`).
- **RS implementation prose subordinated to Appendix A (§7.1,
  §9.2).** Coefficients and reconstruction are defined by the
  appendix; `reed-solomon-erasure` is an optional accelerator
  gated by the impl-step-11.6 conformance test, never the
  definition.
- **Single source of truth for the watermark (§6.2).** Removed
  the duplicate `highest_protected_ordinal` field/param from
  `ParitySource`; it derives from `scope.watermark()`.
- **Digest cross-checks are runtime errors (§8.1).** The
  reconstructed projection's `tape_file_count`,
  `map_total_data_ordinals`, and `highest_protected_ordinal` are
  verified against the bootstrap digest as `FilemarkMapDigestMismatch`,
  not `debug_assert`.
- **Sink owns bootstrap sequence (§4.4, §6.1, §7.3.1).**
  `write_bootstrap()` takes no arguments; `finish()` writes the
  final bootstrap. Removed the vestigial `BootstrapHints`.
- **Catalog:** `ObjectCloseResult` carries the watermark
  explicitly; the 3b DDL gained a display-order warning on the
  FK-cycle blocks and a `data_block_count == block_count` trigger
  note.
- **Cleanups:** invalid `k=256,m=16` example replaced (GF(2⁸)
  limit); `stripes_for_tolerance` now ceiling-divides so tolerance
  is never below target.

**The `layer3c-epoch-revision.md` proposal is superseded** by
this document; its decisions all live here.

**Changes from v0.3.2 (third implementation-readiness review).**
Architecture confirmed sound; these are exact-contract fixes,
the most important being the bootstrap digest API and the
separation of map scope from protection watermark:

- **`write_bootstrap` computes its own digest (§4.4, §6.1,
  §7.3.1).** Removed `filemark_map_digest` from `BootstrapHints`;
  the signature is now `write_bootstrap(sequence, is_final_map)`
  and the sink computes the digest internally — a caller-supplied
  digest could not include the bootstrap's own map entry, the
  exact thing §7.3.1 requires.
- **Map scope vs protection watermark split (§4.4, §5.6, §8.1).**
  `FilemarkMapDigest` now carries both `map_total_data_ordinals`
  (object data described) and `highest_protected_ordinal`
  (sidecars emitted). Recovery requires `failed_ordinal` below
  *both*. v0.3.2 used the map extent as the recovery bound, which
  would have permitted "recovering" an on-tape-but-unprotected
  tail ordinal with no sidecar behind it. `MapScope`/
  `ScopedFilemarkMap` types added with a `recoverable()` check.
- **Normative RS encoding (Appendix A).** `rs-cauchy-gf256-v1` is
  now defined independently of the `reed-solomon-erasure` crate —
  GF(2⁸)/0x11D field, Cauchy matrix construction, systematic and
  parity shard ordering, incremental-compatible encoding,
  decoding, and a versioning rule. A 30-year format cannot rest
  on a library's incidental matrix choices.
- **Scan-reconstruct classifies objects by elimination (§8.1).**
  Object tape files are the residual class (not bootstrap, not
  sidecar magic) rather than identified by reading their pax
  header — which would fail when the very block needing recovery
  is the header block (a circular-recovery failure).
- **Reconstructed blocks are CRC-verified (§8.3).** Step g now
  checks the reconstructed block against the sidecar's data-shard
  CRC, not just its size; `ReconstructionIntegrityFailure` added.
- **Position types disambiguated (§6).** `BlockSource`/
  `BlockSink` return `BodyPosition` (object-local), not
  `TapePosition`; `PhysicalPositionHint` and `TapeFilePosition`
  name the other two address spaces.
- **Normative CRC (§5.5):** CRC-64/XZ, little-endian, with exact
  byte ranges; data-shard CRC covers the entire fixed object
  block (headers, data, manifest, tar EOF, zero-fill).
- **Cleanups:** `CapacityReserveExceeded` mapping no longer says
  "continues the object" (write whole object on another tape,
  §7.5); the §11.1 epoch-footprint cell no longer claims 16 GiB
  spooled (incremental model holds ~512 MiB parity).
- **Catalog (3b follow-up):** the real `catalog_tape_files.
  object_id` deferrable FK (the prose promised it; the DDL
  omitted it), explicit FK-cycle migration order, `block_count >
  0` and bootstrap-`block_count = 1` CHECKs, and the stale
  duplicate `ObjectWriteResult` snippet removed.

**The `layer3c-epoch-revision.md` proposal is superseded** by
this document; its decisions all live here.

**Changes from v0.3.1 (second implementation-readiness review).**
The architecture was confirmed sound; these are contract-edge
fixes that would otherwise have caused subtle bugs:

- **Recovery is per-block, not per-object (§7.2.1, §8.3).** v0.3.1
  refused to recover *any* block of an object whose overall range
  exceeded the watermark — but a large object completes many
  epochs (sidecars emitted) before it closes, so its early blocks
  are protected even while the object is globally not-yet-done.
  Recovery now resolves the failed block to its ordinal and tests
  *that* ordinal against the watermark. Added a third object
  state, `partial` (early epochs protected, tail open).
- **Partial-bootstrap prefix validation (§8.1).** If only an
  intermediate bootstrap survives, its digest covers a prefix of
  the map; `acquire_filemark_map` now validates only that prefix
  (via `tape_file_count`/`total_data_ordinals`) and returns a
  `ScopedFilemarkMap` that bounds recovery to the validated
  prefix, instead of failing outright.
- **Final-bootstrap write workflow (§7.3.1)** spelled out:
  allocate the bootstrap's own structural map entry, hash the
  projection including it, encode, write — content hash computed
  only after, never part of the digest.
- **`data_block_count` removed from the digest projection (§5.6)**
  in favor of the stated identity `data_block_count ==
  block_count` for object tape files; matches the catalog.
- **`RawTapeSource` trait (§4.5).** Bootstrap discovery and
  scan-reconstruct need physical-tape ops (locate-physical,
  filemark spacing) that don't belong on the object-scoped
  `BlockSource`; v0.3.1 wrongly typed them as `BlockSource`.
- **Capacity reserve uses full sidecar tape-file size + a local
  spool reserve (§7.5).** The reserve now counts header/index
  blocks and the filemark, not just parity shards, and separately
  checks that the parity this object will spool fits local disk.
- **No mid-object tape spanning (§7.5).** A failed `begin_object`
  means the object never started; Layer 5 rewrites it whole on
  another tape. rem-tar-v1 objects never straddle a tape boundary.
- **Data-shard CRCs in the sidecar index (§5.5, §8.3).** Recovery
  verifies each surviving peer against a recorded CRC and treats a
  silently-corrupt-but-clean-reading peer as an erasure, so a bad
  peer can't poison an RS reconstruction. ~512 KiB/epoch.
- **Catalog (3b follow-up):** three-state `parity_state`
  (`pending|partial|protected`), a generated `ordinal_end_
  exclusive`, the object↔tape-file FK (deferrable, resolving the
  insert cycle), and the strict sidecar range constraint
  (`end_exclusive > start`, since `D==0` sidecars are never
  emitted).

**The `layer3c-epoch-revision.md` proposal is superseded** by
this document; its decisions all live here.

**Changes from v0.3.0 (first implementation-readiness review).**

- **Object parity-protection lag is now modeled (§7.2.1).** Since
  filemarks don't flush epochs, an object can be on tape and
  tar-readable while its data still sits in an open, sidecar-less
  epoch. A four-state lifecycle (`ObjectDataCommitted` →
  `ParityPending` → `ParityProtected` → `CatalogCommitted`) plus
  a per-tape `highest_protected_ordinal` watermark make the
  catalog track real protection rather than assuming "on tape" =
  "protected." Catalog columns added in §10.1.
- **Capacity reserve includes this object's own future sidecars
  (§7.5).** The v0.3.0 formula counted only already-pending
  parity; for a 1 TiB object that omits ~32 GiB. `begin_object`
  now reserves for `epochs_completed_by_this_object`, and
  `projected_size_blocks` is a hard upper bound (overrun is an
  `Invariant` violation).
- **Bootstrap discovery split into scheme-discovery vs map-
  validation (§8.1).** First-valid (BOT) gives the scheme; the
  highest-sequence / `is_final_map` copy gives the digest used to
  validate a scan-reconstructed map. Validating the full map
  against BOT's empty/partial digest (the v0.3.0 behavior) always
  failed.
- **Filemark-map digest is now canonical and non-circular
  (§5.6).** Defined over a fixed projection (structural fields
  only; excludes content hashes and volatile fields), canonical
  CBOR, so the final bootstrap can commit to a map containing its
  own entry without hashing its own payload. Added `is_final_map`.
- **RS write model chosen: incremental parity accumulation
  (§7.1).** Keeps only the ~512 MiB parity accumulators, never
  buffers the ~16 GiB of epoch data, preserving the "spool stages
  only parity, never object data" claim — at the cost of needing
  GF(2⁸) accumulate ops rather than a black-box `encode()`.
  Option A (buffer 16 GiB) documented as a stated-tradeoff
  fallback.
- **Read API is object-scoped (§6.2).** `ParitySource::open_object`
  returns an `ObjectParitySource` that implements `BlockSource`
  over per-object `BodyLba`; the tape-scoped `ParitySource` is
  not itself a `BlockSource` (a flat `locate(lba)` can't name
  `(tape_file_number, body_lba)`). `write_block` outside an
  active object is rejected.
- **Sidecar magic is unambiguously the HMAC-derived value
  (§5.5)**, with `REM\x00PAR\x01` only as the HMAC label.
- **Smaller:** block-size-aware `default_scheme_for_block_size()`;
  `PhysicalPositionHint` naming for bootstrap seeks; explicit
  filemark-ownership rule (§7.3); skip the final partial-epoch
  sidecar when `D = 0` (§7.2); sidecar-clustering risk stated
  (§7.4).

**The `layer3c-epoch-revision.md` proposal is now superseded** by
this document and should not be treated as active; its decisions
all live here (the §6.4 object-commit model is integrated into
§7.2.1, extended for the parity-lag issue above).

**Changes from v0.2 (filemark-aware parity epochs).** v0.2
modeled the whole tape as a uniform parity-protected area with
parity interleaved inline into a single physical block stream.
That choice forced the body format to give up per-object tape
filemarks (no standard `mt`/`tar` navigation, no independently-
readable archives). v0.3 adopts the filemark-aware epoch model
so per-object filemarks and Reed-Solomon parity coexist:

- **Per-object tape files; parity in sidecar tape files.** The
  physical tape is a sequence of filemark-delimited tape files:
  object archives interspersed with parity-epoch sidecar tape
  files and bootstrap tape files. Each object archive is a
  clean pax tar stream with **no parity blocks inside it**.
  Parity is never interleaved into an object. (§5.1, §5.2, §5.5)
- **Three address spaces.** `TapePosition` (physical:
  `(tape_file_number, block_within_file)` + filemark map),
  `ParityDataOrdinal` (3c-internal: counts protected data
  records across objects, skipping filemarks and sidecars), and
  per-object `BodyLba` (what body formats use). (§4.2, §5.3)
- **Parity epochs span objects.** RS neighborhoods ("epochs")
  are defined over `ParityDataOrdinal`, so a stripe can span the
  filemark between two objects. Filemarks are physical
  separators, never RS shards, and never flush an epoch. (§5.1,
  §5.4)
- **Filemark map** is the new structural element, persisted in
  the catalog (`catalog_tape_files`, 3b) with a digest in the
  bootstrap for catalog-less scan-reconstruct recovery. (§5.6,
  §8.1, §10)
- **API: `begin_object`/`finish_object`** bracket each object;
  the body format owns the final-block flush, 3c owns the
  filemark and sidecar emission. `recover_block_at(tape_file,
  body_lba)` is the forced-erasure entry point. (§6.1, §6.2)
- **Block-size-aware geometry.** Default `S=512, m=4, k=128` at
  256 KiB blocks (was `S=128` assuming 1 MiB), retaining ~512
  MiB contiguous-loss tolerance. (§11.1)
- **Sidecar lifecycle:** capacity reservation, completed-vs-
  partial epoch emission, object commit semantics, final partial
  epoch zero-pad rule, sidecar redundancy decided (not
  replicated; rely on three-copy policy). (§5.4, §7, §10)
- **Bootstrap not parity-protected** (it defines the scheme, so
  parity can't recover it); replicated at known positions,
  CRC/hash-validated, with a filemark-map digest. (§5.6, §8.1)
- **Catalog gains the filemark map** (`catalog_tape_files`).
  v0.2's "no new catalog fields" is superseded. (§10)

**What carries over unchanged from v0.2:** the Reed-Solomon
core math and `ParityScheme` (§4.1), the recovery cache and
`RecoveryEvent` model (§4.3, §9.3, §9.4), the configuration
approach (§11), the error model (§12), and the goals/non-goals
(§1) — except where the uniform-area language is corrected to
the epoch model.

**Lineage:** v0.1 → v0.2 simplified to a uniform parity area
with bootstrap-as-root-of-trust; v0.3 keeps bootstrap-as-root-
of-trust but replaces the uniform inline area with filemark-
aware epochs + sidecars. The v0.2 changelog is preserved at the
end of this section for history.

<details>
<summary>v0.2 changelog (historical)</summary>

- All on-tape blocks live in the uniform parity-protected data
  area. No more carve-outs for catalog, bootstrap, or object
  headers. *(Superseded in v0.3: parity is in sidecar tape
  files; bootstraps/sidecars are outside the ordinal stream.)*
- Bootstrap is the canonical root of trust, replacing the
  catalog as the carrier of the parity scheme. *(Retained.)*
- Bootstrap blocks written at fractional tape positions, found
  by magic-scan. *(Retained, now as bootstrap tape files.)*
- Neighborhoods are fixed-size physical-tape abstractions with
  closed-form LBA-to-stripe math. *(Superseded: epochs are
  defined over ParityDataOrdinal, not physical LBA.)*
- Catalog no longer carries `ParityScheme`/`ParityGeometry`.
  *(Retained — but v0.3 adds the filemark map to the catalog.)*

</details>

---

## 1. Scope

Layer 3c is the **erasure-coded protection layer** for the
on-tape body. It sits between Layer 3a (the SSC primitive set
on `DriveHandle`) and Layer 3b (the pluggable body-format
layer), and wraps the `BlockSink` / `BlockSource` traits 3b
consumes. The body format writes and reads object data blocks;
3c accumulates those blocks into Reed-Solomon parity epochs and
writes the parity as separate **sidecar tape files** at object
boundaries. On read failure, 3c reconstructs missing data from
the relevant epoch's parity. Parity is never interleaved inside
an object's tape file — each object remains a clean,
independently-readable pax tar archive (§5.1).

### Goals

- Protect against **localized media damage** that defeats LTO's
  built-in inner+outer ECC: medium errors on individual blocks,
  multi-block contiguous damage stripes (servo-track damage,
  dust contamination, edge damage, head-clog moments that
  survived write-verify).
- Stay **format-agnostic.** A new body format inherits parity
  protection without modification. The parity layer wraps the
  sink/source traits; body formats see only per-object
  `BodyLba` and are oblivious to epochs, ordinals, and sidecars.
- **Preserve per-object tape filemarks.** Each object is its
  own filemark-delimited tape file, navigable with standard
  `mt`/`tar`. Parity epochs span object boundaries without
  flushing at filemarks, so objects of any size coexist without
  staging (§5.1). This is the property v0.2 sacrificed and v0.3
  restores.
- **Protect object data uniformly.** Every object data block is
  covered by an RS epoch. Bootstrap and parity-sidecar tape
  files are *not* in the parity-protected ordinal stream — they
  are protected by replication (bootstrap) or by the multi-copy
  archive policy (sidecars); see §5.6, §7.
- Be **transparent on the happy path.** When no damage is
  present, reads of an object's tape file incur zero parity-
  related overhead (there are no parity blocks inside the
  object to skip). Writes accumulate parity in memory/spool and
  emit sidecar tape files at object boundaries; the body format
  never sees parity.
- Be **configurable.** The parity scheme parameters (codeword
  size, parity blocks per codeword, interleave factor) are
  per-tape and recorded in the bootstrap. System-wide defaults
  are tuned for the target archive use case (see §11) but every
  parameter is changeable; defaults are block-size-aware (§11.1).
- Inherit Layer 3a's audit + dirty-state model. Parity-block
  writes pass through `DriveHandle::write_block` like any other
  block; transport errors flip the same dirty bit.
- Match the spec's 30-year-portability priority. Parity-sidecar
  and bootstrap tape files carry self-describing magic +
  metadata so a future reader with only the format documentation
  can identify them, skip them on a normal read, and use them on
  a recovery read. Every tape file is identifiable by magic,
  making the filemark map reconstructable by a forensic scan
  (§8.1).

### Non-goals (for this doc)

- **Replacing LTO's built-in protection.** Layer 3c sits above
  LTO's inner+outer Reed-Solomon. The drive's 10⁻²⁰ post-ECC
  BER continues to apply to every block read; 3c handles the
  cases where the drive's ECC gives up entirely.
- **Protection against complete tape destruction.** Fire, full
  delamination, total media failure are out of scope. Those
  are handled by the multi-copy redundancy at the orchestration
  layer.
- **Body-format awareness.** 3c does not know what's in the
  blocks it protects. It treats them as opaque bytes. Bootstrap
  blocks carry their own magic, but 3c doesn't interpret object
  headers, catalog entries, or any body-format structure.
- **Parity scrubbing.** Periodic background reads to detect
  bit rot before it becomes irrecoverable belongs in Layer 5
  operational tooling. The 3c read path supports it (any read
  can trigger recovery), but the scheduling and reporting of
  scrubs is policy.
- **Parity-only re-encoding.** Reading an existing tape and
  rewriting its parity blocks (e.g., to upgrade to a stronger
  scheme) is out of scope. A tape with insufficient protection
  is replaced, not re-paritied.
- **Parity for the LTO-7 → LTO-9 migration tapes.** Migration
  pre-dates 3c. Tapes written without parity remain readable;
  the bootstrap's absence (or its `parity_scheme = none` flag)
  signals no parity coverage.
- **External format authoring.** Same as 3b — the trait surface
  is fixed, no out-of-tree parity schemes.
- **Compression or encryption of parity shards.** Parity shards
  are written raw (no zstd, no AES). Format-level compression was
  removed from rem-tar-v1 (v0.8); compressible data is
  pre-compressed by the orchestrator upstream, so by the time
  bytes reach 3c they are already in final form and parity is
  computed over them as-is. Encryption is applied per-block by
  the drive (Layer 6) and applies uniformly to object-data blocks
  and parity-sidecar blocks alike; 3c is oblivious to it.

### Adoption scope (normative)

> Layer 3c protects only tapes written with a 3c parity scheme.
> It does not retrofit parity onto existing heterogeneous archives,
> legacy LTO-7 / LTO-9 migration tapes, DLT/IBM-format tapes, or any
> other tape written without a 3c bootstrap and sidecar set. Those
> tapes remain readable through their original format path but
> receive no 3c recovery benefit. Existing holdings continue to rely
> on migration, copy redundancy, and ordinary verification.

3c is a go-forward, per-copy recoverability improvement for new
rem-format writes — not a universal parity layer over the existing
multi-format archive.

---

## 2. Background — what 3a / 3b leave for 3c

Layer 3a exposes the SSC primitive set on `DriveHandle<'a>` as
laid out in `docs/layer3a-design.md` §2. 3c's raw adapters
(`DriveHandleRawSink` / `DriveHandleRawSource`, §4.5) surface
these as `write_fixed_block` / `read_record` / `write_filemark` /
`space_filemarks` / `locate_physical` / `position`, returning
`RawWriteOutcome` / `RawReadOutcome` for byte-accurate accounting
and filemark/EOD/EOM detection.

Layer 3b defines the body-facing `BlockSink` / `BlockSource`
traits in `crates/remanence-format/src/lib.rs`. Layer 3a's
`DriveHandle` is adapted to 3c's **raw** traits (§4.5) by
`DriveHandleRawSink<'a, 'b>` / `DriveHandleRawSource<'a, 'b>`
newtype wrappers implementing `RawTapeSink` / `RawTapeSource`.

**Where 3c fits.** Layer 3c wraps a *raw* tape trait and
*exposes* a body-facing trait (review #1). `ParitySink` wraps a
`RawTapeSink` and implements the body-facing `BlockSink`;
`ParitySource` wraps a `RawTapeSource` and, via `open_object`,
exposes an `ObjectParitySource` implementing `BlockSource`. The
body format consumes the parity-wrapped sink's `BlockSink`
surface; object data passes through to the raw sink unchanged,
while 3c accumulates it into RS epochs and emits parity as
separate sidecar tape files at object boundaries. The wrapping is
transparent to the body format, which sees only its own
per-object `BodyLba` / `BodyPosition` — never the physical raw
surface.

Composition at the daemon level:

```rust
let mut drive_sink   = DriveHandleRawSink(&mut drive);   // impl RawTapeSink
let mut parity_sink  = ParitySink::new(&mut drive_sink, scheme, tape_uuid, spool_cfg)?;

// Bootstrap copy 0 at BOT (its own tape file), before any object.
// The sink owns the sequence (0 here) and sets is_final_map=false.
parity_sink.write_bootstrap()?;

for object in objects {
    let tfn = parity_sink.begin_object(object.projected_size_blocks())?;
    let mut writer = format.begin_object_write(&mut parity_sink, &object, params)?;
    writer.write_all()?;                       // blocks + tar EOF + final-block flush
    let close = parity_sink.finish_object()?;  // filemark + completed-epoch sidecars
    // ... record (tfn, close) in the catalog ...

    // Writer policy decides when to emit further bootstrap copies
    // (at object boundaries near ~1/3, ~2/3, near-EOD). See §7.3.
}

parity_sink.finish()?;   // final partial epoch sidecar + final bootstrap
```

`DriveHandleRawSink` implements `RawTapeSink`; `ParitySink`
implements the body-facing `BlockSink`. The body format takes
`&mut dyn BlockSink` and is oblivious to the parity wrapping,
*and* oblivious to the bootstrap
calls happening between its writes.

**Crate boundary.** Layer 3c lives in a new crate,
`crates/remanence-parity`, that depends on `remanence-format`
(for `BlockSink` / `BlockSource` / `FormatError`) and indirectly
on `remanence-library` and `remanence-scsi`. The crate split
keeps the erasure-code dependency (`reed-solomon-erasure` and
its transitive crates) out of `remanence-format`, which body-
format authors should be able to depend on without pulling
in encoding-library weight.

```
remanence-cli ──┐
                ├─→ remanence-parity ──→ remanence-format ──→ remanence-library ──→ remanence-scsi
remanence-api ──┘                              ↑
                                               │
                                       (body formats depend
                                        on remanence-format only)
```

3c does not own:

- The body format's chunk / index / header layout (3b).
- Cartridge-level lifecycle (2b).
- Encryption (Layer 6 — applied per-block by the drive,
  transparent to 3c).
- The catalog's CBOR schema (3b owns it; 3c doesn't add to it).
- LBA range planning for the tape as a whole — the writer
  appends sequentially; 3c just maintains stripe accounting.

---

## 3. Position in the stack

```
Layer 5  (gRPC API)        ← caller: orchestrator, CLI
Layer 4  (local state)     ← catalog cache, audit log
Layer 3b (tape format)     ← rem-chunked-v1, pax-tar-v1, ...
Layer 3c (tape parity)     ← THIS DOC: ParitySink / ParitySource
Layer 3a (tape mechanism)  ← DriveHandle: rewind/locate/read/write
Layer 2  (identity + ops)  ← LibraryHandle: discover, move, load
Layer 1  (SCSI core)       ← remanence-scsi: CDB builders + sg_io
```

3c is the only layer in the stack with two adapter roles: it
both *consumes* a `BlockSink`/`BlockSource` (the one wrapping
`DriveHandle`) and *provides* a `BlockSink`/`BlockSource` (the
one body formats consume). This is intentional — it's the
property that makes parity transparent to body formats.

---

## 4. Domain model

All types live in `remanence_parity::model` unless noted.

### 4.1 `ParityScheme`

The configuration the writer uses and the bootstrap records.
Once a tape is written with scheme S, reading the tape requires
exactly S. Backward-incompatible scheme changes need a new
scheme ID; old tapes continue to use their original scheme,
recorded per-tape in the bootstrap.

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityScheme {
    /// Stable identifier. Format: "rs-cauchy-gf256-v1" for the
    /// initial scheme. New parameter ranges or algorithm
    /// changes get new IDs.
    pub id: SchemeId,

    /// Data blocks per stripe (k). Codeword data size.
    pub data_blocks_per_stripe: u16,

    /// Parity blocks per stripe (m). Each stripe survives up
    /// to m erasures.
    pub parity_blocks_per_stripe: u16,

    /// Stripes per epoch. Determines how many stripes are
    /// interleaved within one parity epoch (over ParityDataOrdinal,
    /// §4.2). Higher → better dispersion against contiguous
    /// damage, more memory/spool required during write. Default is
    /// block-size-aware (§11.1): S scales with block size to hold
    /// contiguous-loss tolerance roughly constant.
    pub stripes_per_epoch: u32,
}

impl ParityScheme {
    /// Total blocks per epoch (data + parity) =
    /// stripes_per_epoch × (data_blocks_per_stripe +
    /// parity_blocks_per_stripe). Of these, S×k are object-data
    /// shards (in object tape files) and S×m are parity shards
    /// (in the epoch's sidecar tape file).
    pub fn epoch_blocks(&self) -> u64 {
        self.stripes_per_epoch as u64
            * (self.data_blocks_per_stripe + self.parity_blocks_per_stripe) as u64
    }

    /// Data shards per epoch = S × k. An epoch completes (and its
    /// sidecar is emitted) once this many object-data shards have
    /// accumulated.
    pub fn data_shards_per_epoch(&self) -> u64 {
        self.stripes_per_epoch as u64 * self.data_blocks_per_stripe as u64
    }

    /// Parity shards per epoch = S × m (the sidecar's payload).
    pub fn parity_shards_per_epoch(&self) -> u64 {
        self.stripes_per_epoch as u64 * self.parity_blocks_per_stripe as u64
    }

    /// Capacity overhead, as a fraction of usable capacity.
    /// E.g. m=4, k=128 → 4/128 = 0.03125 = 3.125%.
    pub fn overhead_ratio(&self) -> f64 {
        self.parity_blocks_per_stripe as f64 / self.data_blocks_per_stripe as f64
    }

    /// Maximum contiguous damage (in blocks) that one epoch can
    /// recover from. Because the row-major interleave (§5.2.1)
    /// puts consecutive data ordinals in different stripes,
    /// physically-contiguous data damage up to S × m blocks lands
    /// one-per-stripe and is recoverable.
    pub fn contiguous_damage_threshold(&self) -> u64 {
        self.stripes_per_epoch as u64
            * self.parity_blocks_per_stripe as u64
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SchemeId(Cow<'static, str>);
```

### 4.2 Address spaces and `StripeAddress`

v0.3 has three distinct block-address spaces. Getting the
separation right is what lets per-object filemarks and RS parity
coexist (this is the core of the epoch model).

**`TapePosition`** — the physical tape address, expressed as
`(tape_file_number, block_within_file)` plus a filemark map.
The physical tape is a sequence of filemark-delimited tape
files: object archives, parity-epoch sidecars, and bootstraps.

**`ParityDataOrdinal`** *(3c-internal)* — a logical sequence
numbering only the protected **object-data** records, in order,
across object archives, **skipping filemarks, sidecars, and
bootstraps**. Object A's data blocks get ordinals 0..a; the
filemark after A gets none; object B's continue at a+1. RS
epochs are defined over this space, so a stripe can span the
filemark between two objects.

**`BodyLba`** *(per-object)* — the block stream within one
object archive, starting at 0 per object, paired with the
object's `tape_file_number`. This is what body formats use
(rem-tar-v1 §2.1). 3c maps `(tape_file_number, body_lba)` →
`ParityDataOrdinal` → `TapePosition` via the filemark map (§5.6).

`StripeAddress` is now expressed in **ordinal space**, not
physical LBA. Given a `StripeAddress` and the scheme, the set
of contributing `ParityDataOrdinal`s is recomputable; the
filemark map then resolves each to a `TapePosition`.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StripeAddress {
    pub epoch: u64,                   // parity epoch id (was: neighborhood)
    pub stripe_index: u32,            // 0..stripes_per_epoch (S)
    pub position: StripePosition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StripePosition {
    /// Data block at position 0..k within its stripe. Maps to a
    /// ParityDataOrdinal (and thence to a TapePosition via the
    /// filemark map). In the final partial epoch (§5.4) some
    /// data positions are implicit-zero padding, never on tape.
    Data { index: u16 },
    /// Parity block at position 0..m within its stripe. Lives in
    /// the epoch's parity-sidecar tape file (§5.5), addressed by
    /// (stripe_index, parity index) via the sidecar's shard
    /// index table.
    Parity { index: u16 },
}
```

The term **epoch** replaces v0.2's **neighborhood**: same RS
geometry (`k` data + `m` parity shards × `S` stripes), but
defined over `ParityDataOrdinal` rather than a fixed physical-
LBA window. "Neighborhood" no longer appears in v0.3.

### 4.3 `RecoveryEvent`

Emitted by the parity reader on every recovery attempt. Surfaced
to Layer 5 via the audit hook so operators see them in the
audit log. A tape that produces recovery events should be
flagged for replacement.

```rust
#[derive(Clone, Debug)]
pub struct RecoveryEvent {
    pub stripe: StripeAddress,
    pub lost_blocks: Vec<StripePosition>,
    pub outcome: RecoveryOutcome,
    /// What the caller asked for, in body-format terms.
    pub at_requested: (u32 /*tape_file_number*/, u64 /*body_lba*/),
}

#[derive(Clone, Debug)]
pub enum RecoveryOutcome {
    /// Reconstructed successfully from k surviving blocks.
    Recovered,
    /// More than m blocks lost; reconstruction failed. Caller
    /// gets a read error and (typically) falls back to another
    /// copy of the data.
    Unrecoverable { lost_count: u16 },
}
```

### 4.4 `FilemarkMapDigest`

The parity sink owns *all* bootstrap contents — parity scheme,
tape UUID, the monotonic `sequence` (sink-owned, review #8), the
`is_final_map` flag, and the **filemark-map digest**, which it
computes itself (the caller cannot, because the digest must
include the bootstrap's own structural map entry that the sink
assigns; §7.3.1, review #1). The write API is therefore purely
imperative — `write_bootstrap()` for a non-final copy,
`finish()` for the final one (§6.1) — and there is no
caller-supplied hints struct. (v0.3.3's `BootstrapHints` is
removed; it carried `sequence`/`is_final_map`/digest, all now
sink-owned. `is_final_map` is set by the sink: `false` for
`write_bootstrap`, `true` for the bootstrap written by
`finish`.)

```rust
#[derive(Clone, Debug)]
pub struct FilemarkMapDigest {
    /// SHA-256 over the CANONICAL MAP PROJECTION (§5.6), not over
    /// raw tape-file content and not over any bootstrap payload —
    /// see the non-circularity rule below.
    pub map_sha256: [u8; 32],
    /// Number of tape files in the projected map prefix this
    /// digest attests. A reader validating a scan-reconstructed
    /// map truncates to this many tape files before hashing (§8.1).
    pub tape_file_count: u32,
    /// Total object-data ordinals DESCRIBED by the map prefix —
    /// i.e. how much object data exists up to this point. This is
    /// NOT the same as the protected watermark (review #2): an
    /// intermediate bootstrap at an object boundary can describe
    /// object data whose tail is still in an open, sidecar-less
    /// epoch. Used to bound which ordinals the map prefix even
    /// names.
    pub map_total_data_ordinals: u64,
    /// The protection watermark as of this bootstrap: the highest
    /// ordinal for which a parity sidecar has been emitted
    /// (= max protected_ordinal_end_exclusive over the prefix's
    /// sidecar tape files). ALWAYS <= map_total_data_ordinals; the
    /// gap is object data on tape but not yet parity-protected.
    /// Recovery requires failed_ordinal < this value (§7.2.1,
    /// §8.1, review #2).
    pub highest_protected_ordinal: u64,
    /// True only in the bootstrap written by `finish()`, whose
    /// digest covers the complete tape. Earlier copies carry
    /// `false` and a partial-scope digest. The reader uses this
    /// (then `sequence`) to pick the authoritative copy for map
    /// validation (§8.1).
    pub is_final_map: bool,
}
```

Note: v0.2's `catalog_hint_lbas` is removed. The catalog now
holds the authoritative filemark map (`catalog_tape_files`,
§10), and catalog-less readers reconstruct-and-validate via the
digest (§8.1) rather than chasing on-tape LBA hints.

### 4.5 `RawTapeSource` / `RawTapeSink` — physical-tape operations

Bootstrap discovery (§8.1), catalog-less scan-reconstruction, and
**all of 3c's own writing** (object passthrough, sidecar tape
files, bootstrap tape files, filemarks) operate on the *physical*
tape — seeking to a physical block, spacing over filemarks,
reading/writing fixed blocks, writing filemarks — before any
object structure is known and outside any object's address space.
These operations do not belong on the object/block-oriented
`BlockSource`/`BlockSink` traits (which are addressed by
per-object `BodyLba` and return `BodyPosition`, §6, review #1).
3c therefore wraps a *raw* trait on each side and *exposes* a
body-facing trait:

```rust
/// Outcome of a raw read. Catalog-less scan-reconstruction needs
/// to distinguish a data block from a filemark from end-of-data
/// (review #1) — a bare `usize` cannot.
pub enum RawReadOutcome {
    Block { bytes: usize, position_after: PhysicalPositionHint },
    Filemark { position_after: PhysicalPositionHint },
    EndOfData { position_after: PhysicalPositionHint },
}

/// Outcome of a raw write. Carries post-write physical position
/// and the drive's early-warning / end-of-medium flags so 3c can
/// react to EOM at object boundaries (§7.5) rather than
/// discovering it as a hard error mid-write.
pub enum RawWriteOutcome {
    WroteBlock    { position_after: PhysicalPositionHint, early_warning: bool, end_of_medium: bool },
    WroteFilemark { position_after: PhysicalPositionHint, early_warning: bool, end_of_medium: bool },
}

pub struct SpaceFilemarksOutcome {
    pub filemarks_spaced: i64,
    pub position_after: PhysicalPositionHint,
    pub hit_end_of_data: bool,
}

/// Raw physical-tape READ access, backed directly by a
/// DriveHandle (Layer 3a). Used by 3c discovery/scan-reconstruct
/// and as the `inner` of ParitySource — never by body formats.
pub trait RawTapeSource {
    /// Seek near a physical block-address hint (READ POSITION /
    /// LOCATE). Not a BodyLba or ParityDataOrdinal.
    fn locate_physical(&mut self, hint: PhysicalPositionHint)
        -> Result<(), FormatError>;
    /// Space forward/back over `count` filemarks (mt fsf/bsf).
    fn space_filemarks(&mut self, count: i64)
        -> Result<SpaceFilemarksOutcome, FormatError>;
    /// Read one record at the current position, distinguishing
    /// block / filemark / end-of-data (review #1).
    fn read_record(&mut self, buf: &mut [u8])
        -> Result<RawReadOutcome, FormatError>;
    /// Current physical position (READ POSITION).
    fn position(&mut self) -> Result<PhysicalPositionHint, FormatError>;
}

/// Raw physical-tape WRITE access, backed directly by a
/// DriveHandle. The `inner` of ParitySink — never seen by body
/// formats (review #1). 3c uses it to write object-data blocks
/// (passthrough), sidecar tape files, bootstrap tape files, and
/// the filemarks delimiting them.
pub trait RawTapeSink {
    /// Append one fixed-size block; returns position + EW/EOM.
    fn write_fixed_block(&mut self, buf: &[u8])
        -> Result<RawWriteOutcome, FormatError>;
    /// Write one filemark (tape-file delimiter) as a SYNCHRONOUS
    /// DURABILITY BARRIER: on success, all preceding fixed blocks
    /// of this tape file AND the filemark are flushed to the
    /// medium and the boundary is repositionable after power loss
    /// (§7.7). The adapter MUST use IMMED=0 / MTWEOF (the flushing
    /// variant), never IMMED=1 / MTWEOFI. Layer 5 commits a
    /// catalog row only AFTER this returns. Returns position + EW/EOM.
    fn write_filemark(&mut self) -> Result<RawWriteOutcome, FormatError>;
    /// Current physical position (READ POSITION), for the filemark
    /// map's physical_start_hint and for capacity tracking.
    fn position(&mut self) -> Result<PhysicalPositionHint, FormatError>;
}
```

The layering is then explicit (review #1):

```
ParitySink   wraps a RawTapeSink   and IMPLEMENTS the body-facing
             BlockSink (per-object BodyLba, returns BodyPosition).
ParitySource wraps a RawTapeSource and, via open_object, exposes
             an ObjectParitySource that IMPLEMENTS the body-facing
             BlockSource (per-object BodyLba, returns BodyPosition).
Discovery / scan-reconstruct take &mut dyn RawTapeSource directly.
```

A `DriveHandle`-backed adapter implements both raw traits. Body
formats see only the body-facing `BlockSink`/`BlockSource` and
their object-local `BodyPosition`; the raw physical surface and
`PhysicalPositionHint` never leak to them.

---

## 5. On-tape layout

### 5.1 Tape files, epochs, and sidecars

The physical tape is a **sequence of filemark-delimited tape
files**, not a uniform block stream. Three kinds of tape file:

```
Physical tape (| = filemark):
| object A (pax tar) | object B (pax tar) | parity sidecar (epoch 0) |
| object C (pax tar) | bootstrap | object D | parity sidecar (epoch 1) | ... | EOD
```

- **Object archive tape files** — one Remanence object each, a
  clean pax tar stream (rem-tar-v1). **No parity blocks inside
  them.** Addressed internally by per-object `BodyLba`.
- **Parity-epoch sidecar tape files** — the RS parity for one
  completed epoch, written at an object boundary (§5.5).
- **Bootstrap tape files** — the root of trust, replicated at
  known fractional tape positions (§5.6).

**Parity epochs** replace v0.2's neighborhoods. An epoch is an
RS neighborhood defined over `ParityDataOrdinal` (§4.2): it
accumulates object-data blocks — across as many object archives
as it takes — until it has filled `S × k` data shards, then its
`S × m` parity shards are emitted as a sidecar tape file at the
next object boundary.

The defining properties:

- **Filemarks are physical separators, never RS shards, and
  never flush an epoch.** A stripe can have data shards in
  object A and object B with the A|B filemark between them; the
  filemark simply isn't counted in `ParityDataOrdinal`. This is
  what lets objects of any size coexist without staging to fill
  a neighborhood — the failure mode that "flush parity at every
  filemark" would have caused.
- **Parity is never interleaved inside an object.** It lives in
  separate sidecar tape files. So an object's tape file is
  always a clean pax tar archive a standard tool can read
  (`mt fsf N; tar -b 512 -xf`), with no parity bytes to trip
  over.
- **Epochs are invisible to body formats.** A body format sees
  only its own per-object `BodyLba` stream; it never knows which
  epoch its blocks landed in or where the sidecar went.

At the block-size-aware default (`S=512, m=4, k=128` at 256 KiB
blocks — §11.1), an epoch spans `S × (k+m) × block_size` ≈ 16.5
GiB of (data + parity), the same footprint as v0.2's
neighborhood, and tolerates ~512 MiB of contiguous data loss
per epoch.

### 5.2 No parity inside objects; data flows through unchanged

In v0.2, `ParitySink::write_block` interleaved parity into the
same physical stream as data. In v0.3 it does **not**: object
data blocks are forwarded to the inner sink unchanged (preserving
the clean pax tar stream) while 3c *also* accumulates them into
the current epoch's RS computation by `ParityDataOrdinal`. The
parity shards are materialized later, as a sidecar tape file
(§5.5), at the object boundary where the epoch completes (§7).

Critically (the invariant the API enforces, §6.1): only
object-data blocks written between `begin_object` and
`finish_object` get a `ParityDataOrdinal` and feed epoch
accumulation. Sidecar and bootstrap blocks are written by
internal helpers that forward to the inner sink **without**
ordinal assignment — otherwise sidecar bytes would corrupt the
next epoch's parity.

### 5.2.1 Stripe interleave within an epoch

Within an epoch, the `S × k` data shards are assigned to stripes
by **row-major interleave over `ParityDataOrdinal`**, exactly
the v0.2 pattern but in ordinal space rather than physical LBA.
For `stripes_per_epoch = S`, `data_blocks_per_stripe = k`: the
epoch-local ordinal `o` (0..S*k within the epoch) maps as

```
  o = 0      → stripe 0,   data 0
  o = 1      → stripe 1,   data 0
  ...
  o = S-1    → stripe S-1, data 0
  o = S      → stripe 0,   data 1
  ...
  o = S*k-1  → stripe S-1, data k-1
```

i.e. `stripe_index = o % S`, `data_index = o / S`. Parity shards
(`S × m` of them) live in the sidecar, addressed by
`(stripe_index, parity_index)` via the sidecar's shard index
table (§5.5).

This preserves the v0.2 interleave's three useful properties,
now stated in ordinal terms:

1. **Contiguous damage of N data blocks (which are physically
   contiguous within an object, since objects are contiguous
   tape files) hits stripes roughly uniformly.** Because
   consecutive `ParityDataOrdinal`s differ by stripe index
   (varying fastest), N physically-consecutive data blocks land
   in N different stripes (for N ≤ S). At `S=512, m=4`,
   contiguous damage up to `m × S = 4 × 512 = 2048` blocks ≈
   512 MiB is recoverable. (Damage spanning a filemark into a
   sidecar or the next object is handled by the ordinal mapping,
   which simply skips the non-data tape file.)
2. **The mapping ordinal ↔ (stripe, position) is closed-form
   integer arithmetic**, no per-stripe table — same as v0.2,
   just over ordinals.
3. **Data ordinals precede their parity.** All `S × k` data
   shards of an epoch are accumulated before the epoch's parity
   is computed and emitted, so parity computation at epoch
   completion is immediate from in-memory/spooled state.

### 5.3 Ordinal ↔ stripe mapping

Given the scheme and a global `ParityDataOrdinal` `o`, the
epoch and the epoch-local stripe address are closed-form:

```rust
fn ordinal_to_stripe(o: u64, scheme: &ParityScheme) -> StripeAddress {
    let s = scheme.stripes_per_epoch as u64;     // S
    let k = scheme.data_blocks_per_stripe as u64; // k
    let data_shards_per_epoch = s * k;            // S*k

    let epoch = o / data_shards_per_epoch;
    let o_in_epoch = o % data_shards_per_epoch;

    StripeAddress {
        epoch,
        stripe_index: (o_in_epoch % s) as u32,
        position: StripePosition::Data { index: (o_in_epoch / s) as u16 },
    }
}

// Reverse: a data shard's global ordinal from its stripe address.
fn stripe_data_to_ordinal(addr: &StripeAddress, scheme: &ParityScheme) -> u64 {
    let s = scheme.stripes_per_epoch as u64;
    let k = scheme.data_blocks_per_stripe as u64;
    let data_index = match addr.position {
        StripePosition::Data { index } => index as u64,
        StripePosition::Parity { .. } =>
            panic!("parity shards live in the sidecar, not the ordinal stream"),
    };
    addr.epoch * (s * k) + data_index * s + addr.stripe_index as u64
}
```

Note what is *absent* compared to v0.2: there is no
`lba_to_stripe`. The parity layer never maps a physical LBA to
a stripe, because data and parity no longer share a physical
LBA stream. Instead:

- A data block is identified by `(tape_file_number, body_lba)`,
  which the filemark map (§5.6) converts to a
  `ParityDataOrdinal`, which `ordinal_to_stripe` converts to a
  stripe address.
- A parity shard is identified by `(epoch, stripe_index,
  parity_index)` and located in the epoch's sidecar tape file
  via the sidecar's shard index table (§5.5).

### 5.4 End-of-tape: the final partial epoch

When the writer reaches end-of-tape (drive reports EOM) or the
write session simply ends, the current epoch is almost always
partial — it has accumulated `D < S × k` data shards. v0.2
zero-padded a partial *neighborhood* with real zero blocks on
tape; v0.3 handles the partial *epoch* without writing zero
blocks:

```
At ParitySink::finish():
1. Let D = real data shards accumulated in the current epoch.
2. Logically zero-pad to S × k: the missing (S×k − D) shards
   are treated as all-zero for the RS computation but are NOT
   written to tape — they are implicit zeros.
3. Compute the S × m parity shards over (real + implicit-zero)
   data shards.
4. Write the final parity-epoch sidecar tape file (§5.5). Its
   header records:
     - protected_ordinal_start
     - protected_ordinal_end_exclusive  (= start + D; REAL data only)
     - logical_shard_count = S × k       (padded width for RS math)
     - real_data_shard_count = D
   so a reader supplies implicit-zero blocks for the padded
   positions and reconstructs correctly.
5. Write the final bootstrap copy (§5.6), whose filemark-map
   digest covers the complete tape.
```

A reader recovering a block in the final epoch reconstructs
using the real shards it can read plus implicit zeros for the
padded positions — exactly as the writer computed parity. The
`real_data_shard_count` is the authority on where real data
ends.

This protects the tail of the tape fully, without writing zero
blocks and without leaving any object data unprotected. The
"few hundred MiB of unprotected tail" trade-off that v0.2
accepted is gone.

### 5.5 Parity-epoch sidecar tape file format

An epoch's parity is written as its own filemark-delimited tape
file — a **parity-epoch sidecar** — not interleaved into object
data. The critical constraint: an RS parity shard for a
`chunk_size` data shard is itself `chunk_size` bytes, so it
fills an entire tape block with no room for inline metadata. All
per-shard metadata therefore lives in a **header/index block(s)**
at the front of the sidecar; the parity shard blocks are full
raw bytes and nothing else.

```
Parity-epoch sidecar tape file (one filemark-delimited file).
All multi-byte integers LITTLE-ENDIAN; all CRCs CRC-64/XZ (§5.5).

[block 0: header + header extension + as many inline index entries as fit]
  off    type     field
  0x00   u8[8]    magic              (= parity_magic, HMAC-derived; see below)
  0x08   u8[16]   tape_uuid
  0x18   u64le    epoch_id
  0x20   u16le    k
  0x22   u16le    m
  0x24   u32le    S                  (stripes per epoch)
  0x28   u32le    block_size
  0x2C   u32le    schema_version     (sidecar struct version = 2)
  0x30   u64le    protected_ordinal_start
  0x38   u64le    protected_ordinal_end_exclusive   (half-open)
  0x40   u64le    logical_shard_count   (= S × k; padded width for RS)
  0x48   u64le    real_data_shard_count D (= end_exclusive − start)
  0x50   u32le    parity_block_count    P (= S × m)
  0x54   u32le    data_crc_count        (= D)
  0x58   u32le    shard_index_block_count H
  0x5C   u32le    inline_index_entry_bytes   (bytes of index in block 0)
  --- header extension (v0.5 metadata-replication fields) ---
  0x60   u8       copy_kind          (0 = primary, 1 = tail)
  0x61   u8       copy_generation    (= 0)
  0x62   u16le    reserved (zero)
  0x64   u32le    reserved (zero)
  0x68   u64le    sidecar_total_block_count   (= 2H + P + 1)
  0x70   u64le    primary_header_start_block  (= 0)
  0x78   u64le    tail_header_start_block     (= H + P)
  0x80   u64le    footer_block_index          (= H + P + H)
  0x88   u8[32]   canonical_metadata_hash     (see "metadata hash" below)
  --- end header extension ---
  0xA8   u64le    header_crc64       (covers 0x00 .. 0xA8, exclusive)
  0xB0   ...      inline index entries (see below), length =
                  inline_index_entry_bytes
  ...    (zero-fill to block_size − 8)
  block_size−8  u64le  block0_crc64   (covers 0x00 .. block_size−8,
                  i.e. header + extension + inline entries + zero-fill;
                  LAST field in block 0, review #2/#4)

[blocks 1 .. H-1: spilled index, only if the index exceeds block 0]
  each such block, for offset 0 .. block_size−8:
    packed index entries continuing the block-0 order
    (zero-fill any remainder)
  block_size−8  u64le  index_block_crc64  (covers 0 .. block_size−8)

Index entry layout (packed, in this exact order across block 0
then the spill blocks):
  PARITY index — parity_block_count entries, each 16 bytes:
    0x00  u32le  stripe_index
    0x04  u16le  parity_index
    0x06  u16le  reserved (zero)
    0x08  u64le  parity_shard_crc64
  DATA index — data_crc_count entries, each 8 bytes, in ordinal
    order (ordinal = protected_ordinal_start + d):
    0x00  u64le  data_shard_crc64

[blocks H .. H+P-1: parity shards]
  each block: exactly block_size bytes of one RS parity shard and
  NOTHING else; identity (stripe_index, parity_index) and CRC are
  in the parity index. Parity shards appear in (stripe_index
  major, parity_index minor) order, matching the parity index
  and Appendix A §A.6. Recovery reads a shard block as a full raw
  shard.

[blocks H+P .. H+P+H-1: TAIL header/index copy]
  Byte-identical to primary blocks 0..H-1 EXCEPT copy_kind = 1 and
  the per-block CRCs are recomputed (they cover copy_kind, which
  differs). canonical_metadata_hash is the same value in both
  copies. Purpose: a damage region at the front of the sidecar no
  longer destroys the only metadata.

[block H+P+H: footer locator]
  Fixed-size SidecarFooter (see below). Lets a scanner classify a
  sidecar and find the tail copy even when block 0 is unreadable.
```

**Data-shard CRCs (defense against silent peer corruption).**
The index also records a CRC-64 for each *real data shard* of
the epoch (the data lives in the object tape files, not the
sidecar — only its CRC is here). Reconstruction reads up to `k`
surviving stripe peers; if a peer data shard reads "clean" at
the drive level but is silently corrupt, using it would *poison*
the RS reconstruction (produce wrong bytes that pass size
checks). With per-data-shard CRCs, recovery verifies each peer
against its recorded CRC and treats a CRC-mismatched peer as an
additional erasure rather than trusting it. This makes 3c
recovery robust independent of the body format's own integrity
chain, and protects tar headers/manifest blocks (which rem-tar
also CRCs) and regular file chunks alike. Cost at default
geometry: `S × k = 65,536` data CRCs × 8 bytes = 512 KiB ≈ two
256 KiB index blocks — folded into `H`. (The final partial epoch
records only `D = real_data_shard_count` data CRCs; implicit-zero
padding shards have a known CRC and need not be stored.)

Sizing: at `S=512, m=4` an epoch has `S × m = 2048` parity index
entries (~16 B each ≈ 32 KiB) plus `S × k = 65,536` data CRCs
(8 B each = 512 KiB), so the index is ~544 KiB ≈ `H = 3` header
blocks at 256 KiB. `shard_index_block_count` records `H`; larger
schemes spill across more blocks.

The writer computes `H` and the inline split by this normative
formula (entries are never split across a block boundary):

```
total_index_bytes      = parity_block_count * 16 + data_crc_count * 8
block0_index_capacity   = block_size - 8 - 0xB0    // after header+extension, before block0_crc64
inline_index_entry_bytes = whole entries fitting in min(total_index_bytes,
                                                        block0_index_capacity)
remaining_index_bytes   = total_index_bytes - inline_index_entry_bytes
spill_block_capacity    = block_size - 8           // before each index_block_crc64
H = 1 + ceil(remaining_index_bytes / spill_block_capacity)
```

`inline_index_entry_bytes` and each spill block hold only whole
index entries — taken in order from the single packed stream of
*16-byte parity entries followed by 8-byte data-CRC entries* (the
stream is heterogeneous; "whole entries" means no 16-byte or
8-byte entry straddles a block boundary, not that all entries are
the same width). Any leftover bytes before the trailing CRC are
zero-filled and covered by that block's CRC. With LTO block sizes
(256 KiB+) and 8/16-byte entries this alignment falls out
naturally, but the "no split entry" invariant is asserted in
tests (impl step 11.5).

The magic is **derived per-tape** to avoid collision with user
data. It is the HMAC output, not the literal label bytes —
`b"REM\x00PAR\x01"` is only the HMAC message, never written to
tape as-is:

```
parity_magic = HMAC-SHA256(tape_uuid, b"REM\x00PAR\x01")[0..8]
```

A recovery scanner, knowing the tape UUID from the bootstrap,
recomputes the expected magic and compares. The header CRC
validates the metadata; each shard's CRC (in the index table)
validates that shard; RS reconstruction itself is the final
arbiter of payload integrity.

**CRC definition (normative).** Every CRC in 3c — header CRC,
the block-0 CRC, index-block CRC, parity-shard CRC, data-shard
CRC, **and the bootstrap header/payload CRCs (§5.6)** — is
**CRC-64/XZ**: width 64, polynomial `0x42F0E1EBA9EA3693`,
reflected input and reflected output (refin = refout = true),
init `0xFFFFFFFFFFFFFFFF`, final XOR `0xFFFFFFFFFFFFFFFF`. Check
value: CRC of the ASCII string `123456789` is
`0x995DC9BBDF1939FA`. (The parameter tuple above is the
definition; the alias "CRC-64/GO-ECMA" is mentioned only for
lookup and is not relied upon.) Each CRC is stored as a
little-endian `u64` in its field. Byte ranges:

- **header_crc64** covers header block 0 from offset 0 up to (not
  including) the `header_crc64` field itself.
- **block0_crc64** (review #2) covers ALL meaningful bytes of
  block 0 — the header AND any inline shard-index entries that
  fit after the header — from offset 0 up to (not including) the
  `block0_crc64` field, which is the last meaningful field in
  block 0. This closes the gap where index entries packed into
  block 0 after `header_crc64` would otherwise be uncovered.
- **index-block CRC** (one per spilled index block `1..H`) covers
  that block from offset 0 up to its own trailing CRC field.
- **parity-shard CRC** covers the entire `block_size`-byte parity
  shard block.
- **data-shard CRC** covers the **entire fixed `block_size`-byte
  object block as 3c sees it** — all of it: tar/pax headers, file
  data, manifest bytes, the tar EOF marker, and any post-EOF
  zero-fill in the final block (rem-tar-v1 §5.4). 3c is
  format-agnostic about a block's *contents*; the CRC is over the
  raw block bytes, so it protects header and manifest blocks
  exactly as it protects file-data blocks. The reconstructed
  block must reproduce these exact bytes (§8.3 step g).

The CRCs detect corruption; RS reconstruction repairs it.

**Sidecar metadata is replicated; the shard payload is not (v0.5).**
Each sidecar carries its header/index set twice — a primary copy
at the front (blocks `0..H-1`) and a tail copy at the end (blocks
`H+P..H+P+H-1`) — plus a one-block footer locator. The ~544 KiB
metadata is replicated because losing it is catastrophic
(catalog-less recovery can no longer classify or use the sidecar),
whereas the metadata is < 0.2% of the `S × m` ≈ 512 MiB shard
payload. The shard payload itself is *not* replicated: if a
contiguous damage event destroys the parity shards for an epoch,
that epoch's parity on this tape is lost and Layer 5 falls back to
another of the archive's three copies (§16). 3c is a per-copy
recoverability improvement, not the sole archive redundancy
mechanism. Sidecars are written via internal helpers that bypass
`ParityDataOrdinal` assignment (§5.2) so they never feed the next
epoch's accumulation.

**Canonical metadata hash.** `canonical_metadata_hash` is the
SHA-256 over a deterministic encoding of the sidecar's metadata
fields and its full index entries, **excluding** copy-local fields
(`copy_kind`, `copy_generation`) and all CRC fields. The primary
and tail copies therefore carry the same hash; a reader that can
read both verifies they agree. A header/index copy is *usable*
only if: its `magic` matches the per-tape `parity_magic`; its
header/index CRCs pass; its `canonical_metadata_hash` matches the
footer's; its scheme parameters match the bootstrap/catalog scheme;
and its protected ordinal range is sane and non-empty.

**Footer locator (block `H+P+H`).** A fixed-size block that lets a
scanner identify a sidecar and locate its tail copy even when
block 0 is unreadable:

```
SidecarFooter (one block; LITTLE-ENDIAN; CRC-64/XZ):
  0x00   u8[8]    magic = HMAC-SHA256(tape_uuid, b"REM\x00PARFOOT\x01")[0..8]
  0x08   u16le    sidecar_footer_version (= 1)
  0x0A   u16le    reserved (zero)
  0x0C   u32le    sidecar_header_block_count   H
  0x10   u8[16]   tape_uuid
  0x20   u64le    epoch_id
  0x28   u64le    protected_ordinal_start
  0x30   u64le    protected_ordinal_end_exclusive
  0x38   u64le    parity_shard_block_count     P
  0x40   u64le    sidecar_total_block_count    (= 2H + P + 1)
  0x48   u64le    primary_header_start_block   (= 0)
  0x50   u64le    tail_header_start_block      (= H + P)
  0x58   u8[32]   canonical_metadata_hash
  0x78   u64le    footer_crc64   (covers 0x00 .. 0x78, exclusive)
  ...    (zero-fill to block_size − 8)
  block_size−8  u64le  footer_block_crc64  (covers 0x00 .. block_size−8)
```

The footer is mandatory. If the primary header is unreadable, scan
reconstruction reads the footer, derives `tail_header_start_block`,
and validates the tail header/index copy.

**Recovery behavior by sidecar metadata state (normative):**

```
primary valid, tail valid, hashes match:
  Sidecar usable.
primary valid, tail invalid/missing:
  Sidecar usable; emit SidecarMetadataCopyLost audit event.
tail valid, primary invalid/missing:
  Sidecar usable; emit SidecarPrimaryHeaderLost audit event.
footer valid, both header copies invalid:
  Structurally classifiable, not usable for parity recovery.
  Map reconstruction continues; only this epoch is parity-unavailable.
footer invalid and primary invalid:
  Use bootstrap inline directory or parity_map directory (§5.6) if available.
  If a directory can classify the tape file, map reconstruction
  continues; parity recovery for this epoch is unavailable unless a
  header copy can be recovered.
all metadata sources unavailable:
  Catalog-less scan cannot classify this sidecar locally; if no
  directory entry covers it, map validation fails for that scope.
```

The hard rule: **one damaged sidecar header must never poison
catalog-less parity recovery for unrelated epochs.** At worst the
affected epoch becomes `SidecarMetadataUnavailable`; every other
epoch remains recoverable (§8.1).

**Small final partial sidecars.** For a small final partial epoch,
`P` can be small enough that the primary copy, tail copy, and
footer are physically close, so one contiguous hit can destroy all
local metadata. This is accepted: the bootstrap/`parity_map`
directory (§5.6) still classifies the tape file structurally, only
that final partial epoch's parity becomes unavailable, and the data
at risk is small. Do not add artificial padding solely to separate
final-partial sidecar metadata.

#### Alternative: pax-wrapped sidecar (not the default)

For the "every tape file is tar-inspectable" property, a sidecar
may instead be a minimal pax tar archive containing
`_remanence/parity/epoch-NNN.cbor` (header + shard index table)
and `_remanence/parity/epoch-NNN.bin` (raw shards, chunk_size
each). v1 uses the raw fixed-block sidecar above, because
recovery should depend on as little machinery as possible (no
tar/CBOR parsing exactly when the tape is already damaged). The
pax-wrapped form is documented for operators who prioritize
inspectability over recovery simplicity; it is not the default.

### 5.6 Bootstrap tape file format and the filemark map

The bootstrap is the **canonical root of trust** for the tape.
It's the first thing a reader finds, and it tells the reader
everything needed to interpret the rest of the tape — the parity
scheme, and a digest of the filemark map.

**Bootstrap is its own tape file, replicated, and NOT parity-
protected.** This is a deliberate change from v0.2's "bootstrap
is an ordinary parity-protected block." The reasoning is a
chicken-and-egg: the bootstrap defines the parity scheme, so a
reader can't use parity to recover the bootstrap — at discovery
time it hasn't parsed any scheme yet. Instead:

- The writer places **multiple bootstrap copies** at known
  fractional tape positions (BOT, ~1/3, ~2/3, near-EOD; §7.3),
  each its own filemark-delimited tape file, each self-validating
  by CRC/hash.
- A reader scans expected positions for the bootstrap magic and
  takes the first copy that validates. Losing some copies to
  damage is fine as long as one validates.
- Bootstrap blocks are written via the same internal "bypass"
  path as sidecars (§5.2): forwarded to the inner sink, **not**
  assigned a `ParityDataOrdinal`, **not** in any RS epoch.

This makes the v0.2 discovery model (magic-scan at expected
positions) also the *protection* model, rather than bolting the
bootstrap into an ordinal stream it logically precedes.

```
Bootstrap tape file (one filemark-delimited file; one block).
All multi-byte integers as noted; CRCs CRC-64/XZ (§5.5).

  off    type     field
  0x00   u8[8]    magic            ('R','E','M',0x00,'B','O','O',0x01; fixed)
  0x08   u16be    schema_major
  0x0A   u16be    schema_minor
  0x0C   u32be    flags            (bit 0 = no-parity tape, §5.6)
  0x10   u8[16]   tape_uuid
  0x20   u32be    block_size_bytes
  0x24   u32be    sequence
  0x28   u32le    cbor_payload_len (BEFORE the header CRC, review #1)
  0x2C   u64le    crc64_header     (covers 0x00 .. 0x2C, exclusive —
                  INCLUDING cbor_payload_len, so a corrupt length is
                  detected before it is used to bound the payload read)
  0x34   u8[len]  CBOR payload     (ParitySchemeRecord + FilemarkMapDigest),
                  length = cbor_payload_len
  0x34+len  u64le crc64_payload    (covers the cbor_payload_len payload bytes)
  ...      (zero-fill padding to the tape block size)
```

Both bootstrap CRCs are **CRC-64/XZ**, consistent with every
other 3c CRC (§5.5). `crc64_header` covers the fixed header
*including* `cbor_payload_len` (review #1): the length field is
now inside the header CRC, so a reader validates the length
before trusting it to bound the payload read — closing the
out-of-bounds / misleading-payload-CRC gap where a corrupt
`cbor_payload_len` sat between both CRCs. `crc64_payload` covers
the payload bytes. (v0.3.3 used CRC-32 here, contradicting §5.5's
CRC-64 rule.)

The bootstrap magic is **fixed** (not per-tape derived, unlike
the parity-sidecar magic): the reader must find a bootstrap
*before* it knows the tape UUID, so the magic must be
discoverable from the spec alone. The trade-off — user data that
happens to contain `REM\x00BOO\x01` near a known fractional
position — is mitigated by checking only at known position
regions and by the header CRC.

CBOR payload schema:

```cbor
BootstrapPayload = {
    1: ParitySchemeRecord,            ; the parity scheme for this tape
    2: FilemarkMapDigest,             ; validates a scan-reconstructed map (§8.1)
    3: ?tstr,                         ; rem software version that wrote this tape
    4: ?tstr,                         ; RFC3339 timestamp of this bootstrap's write
   20: ?SidecarEpochDirectory,        ; inline sidecar directory, when it fits (§5.6.1)
   21: ?ParityMapReference,           ; reference to external parity_map, when not (§5.6.1)
}

ParitySchemeRecord = {
    1: tstr,                          ; scheme ID (e.g. "rs-cauchy-gf256-v1")
    2: uint,                          ; data_blocks_per_stripe (k)
    3: uint,                          ; parity_blocks_per_stripe (m)
    4: uint,                          ; stripes_per_epoch (S)
}

FilemarkMapDigest = {
    1: bytes .size 32,                ; SHA-256 of the canonical map projection (below)
    2: uint,                          ; tape_file_count (prefix length, in tape files)
    3: uint,                          ; map_total_data_ordinals (object data DESCRIBED)
    4: uint,                          ; highest_protected_ordinal (sidecars EMITTED)
    5: bool,                          ; is_final_map
}
```

Note tags 3 and 4 are distinct (review #2). `map_total_data_
ordinals` is how much object data the map prefix names;
`highest_protected_ordinal` is how much of it has an emitted
parity sidecar. They are equal only when the map prefix ends on
a completed epoch; at an arbitrary object boundary the tail of
the last object sits in an open epoch, so
`highest_protected_ordinal < map_total_data_ordinals`, and that
gap is data on tape but not yet RS-protected. Recovery uses tag
4, never tag 3, as the protection bound.

#### The filemark map

The filemark map is the new structural element that ties the
three address spaces together. It records, for every tape file:
`tape_file_number`, `kind` (object / parity_sidecar / bootstrap /
parity_map), `block_count`, and for objects the
`first_parity_data_ordinal`
and data-block count, for sidecars the protected ordinal range.

It is persisted authoritatively in the **catalog**
(`catalog_tape_files`, §10). A **digest** of it is carried in
each bootstrap so a catalog-less reader can reconstruct the map
by scanning (§8.1) and validate the reconstruction. The map is
what `ParitySource` uses to translate `(tape_file_number,
body_lba)` ↔ `ParityDataOrdinal` ↔ `TapePosition` for both
normal reads and recovery.

#### Canonical map projection (digest input) — non-circular

The digest is a root-of-trust object, so its input must be
defined exactly and must **not** be self-referential. A
bootstrap is itself a tape file, so the final bootstrap's digest
covers a map that *includes the final bootstrap's own entry* —
if that entry contained the bootstrap's payload hash, hashing
the map would require hashing the digest that is being computed.
The projection therefore includes only structural,
position-independent fields and **excludes** content hashes and
volatile catalog-only fields:

```
For each tape file, in ascending tape_file_number order, the
canonical projection emits this fixed tuple:

  Included:
    tape_file_number              (uint)
    kind                          (uint enum: 0=object,1=parity_sidecar,2=bootstrap,3=parity_map)
    block_count                   (uint)
    first_parity_data_ordinal     (uint, or the sentinel ABSENT for non-objects)
    protected_ordinal_start       (uint, or ABSENT for non-sidecars)
    protected_ordinal_end_exclusive (uint, or ABSENT for non-sidecars)
    epoch_id                      (uint, or ABSENT for non-sidecars)

  Excluded (NOT hashed):
    physical_start_hint           (volatile positioning hint)
    per-tape-file content sha256  (would be circular for bootstraps)
    bootstrap payload bytes       (circular)
    any catalog-only / mutable column (parity_state, timestamps, ...)
```

There is no separate `data_block_count` field in the projection
or in `catalog_tape_files` (review #4). For an **object** tape
file every fixed block is protected object data — tar headers,
file data, manifest, tar EOF, and the post-EOF zero-filled final
block (rem-tar-v1 §5.4) all count — so:

```
For kind=object:                    data_block_count == block_count
For sidecar / bootstrap / parity_map: data_block_count is N/A (no ordinals)
```

`block_count` is therefore the single source of truth for an
object tape file's protected-data extent; an object's
`ordinal_end_exclusive = first_parity_data_ordinal + block_count`.
(The object-level `catalog_objects.data_block_count` column,
§10.1, is the same value copied into the object row in the same
transaction for query convenience, not an independent quantity;
if v1 ever introduces an object tape file with non-protected
blocks, this identity is where that distinction would be
reintroduced.)

Encoding is canonical CBOR (RFC 8949 §4.2): the projection is a
CBOR array of fixed-length arrays in `tape_file_number` order,
unsigned integers in shortest form, `ABSENT` encoded as CBOR
`null`. `map_sha256 = SHA-256(canonical_cbor(projection))`. A
bootstrap's own entry appears in the projection by
`(tape_file_number, kind=bootstrap, block_count)` only — never
by its content — so the final bootstrap can commit to a map that
includes itself without recursion. `tape_file_count` and
`map_total_data_ordinals` in the digest are redundant
cross-checks derivable from the projection; readers verify they
agree. `highest_protected_ordinal` is also derivable from the
projection — it is `max(protected_ordinal_end_exclusive)` over
the prefix's sidecar tape files — and readers verify the
recorded value matches that maximum.

A `flags` field with bit 0 set indicates this tape has no parity
(written with `--parity none`); all other fields except magic,
schema version, tape UUID, block size, sequence, and header CRC
may be absent. Readers seeing this flag treat the tape as
no-parity and bypass the parity source.

The bootstrap is small when its CBOR payload carries only scheme +
digest — well under 200 bytes, the full block fitting in 1 KiB. A
v0.5 bootstrap that also carries an *inline* `SidecarEpochDirectory`
(field 20) is larger; when the encoded directory would not fit
comfortably in the block, the directory is written to a separate
`parity_map` tape file and the bootstrap carries only a small
`ParityMapReference` (field 21) instead (§5.6.1). Either way the
block is padded to the tape's block size (default 256 KiB), so a
bootstrap is one block on tape. With a handful of bootstrap copies
per tape, total bootstrap overhead is well under 0.001% of an
18 TB tape.

### 5.6.1 Sidecar epoch directory and `parity_map` tape files

**Why a directory.** A whole-map digest (§5.6) can *validate* a
reconstructed map but cannot *localize* a single wrong entry. A
damaged sidecar header would otherwise cause exactly one wrong
entry (a sidecar misclassified as an object) and fail the whole
digest. The directory is the trusted, per-entry structural
attestation that lets a catalog-less scan correct that single
classification, so one damaged header degrades only its own epoch
(§8.1). The directory does not replicate the shard payload; it
repairs *classification*, not lost parity.

**Directory schema.** A compact structural list of the tape's
parity sidecars:

```cbor
SidecarEpochDirectory = {
    1: uint,                       ; directory_scope_tape_file_count
    2: uint,                       ; directory_scope_total_data_ordinals
    3: uint,                       ; directory_scope_highest_protected_ordinal
    4: bool,                       ; is_final_directory
    5: [* SidecarEpochDirectoryEntry],
}

SidecarEpochDirectoryEntry = {
    1: uint,                       ; tape_file_number
    2: uint,                       ; epoch_id
    3: uint,                       ; protected_ordinal_start
    4: uint,                       ; protected_ordinal_end_exclusive
    5: uint,                       ; sidecar_total_block_count
    6: uint,                       ; sidecar_header_block_count
    7: uint,                       ; parity_shard_block_count
    8: bytes .size 32,             ; canonical_metadata_hash (matches §5.5)
    9: uint,                       ; flags
}
; flags: 0x01 = final partial epoch
;        0x02 = sidecar primary header known good at write time
;        0x04 = sidecar tail header known good at write time
```

**Inline vs external — computed sizing rule (normative).** Do not
hard-code an epoch-count assumption. The writer computes the
encoded directory size against the actual block budget:

```
inline_bootstrap_directory_limit =
      block_size
    − bootstrap_fixed_header_bytes
    − bootstrap_payload_crc_bytes
    − bootstrap_required_payload_bytes      (scheme + digest + version/timestamp)
    − conservative_padding_margin           (>= 4 KiB)

if encoded(SidecarEpochDirectory).len <= inline_bootstrap_directory_limit:
    store inline in BootstrapPayload field 20
else:
    write a parity_map tape file and store a ParityMapReference in field 21
```

This applies to default, conservative, and custom schemes alike.
**No production tape may omit the directory merely because it does
not fit inline.** (At the default scheme, ~1100 epochs of ~90-byte
entries ≈ 99 KiB fit one 256 KiB block inline; the conservative
scheme, ~4000 epochs ≈ 360 KiB, exceeds it and uses a `parity_map`.)

**`ParityMapReference` (bootstrap field 21).**

```cbor
ParityMapReference = {
    1: uint,                        ; tape_file_number of the parity_map
    2: uint,                        ; block_count
    3: uint,                        ; directory_scope_tape_file_count
    4: uint,                        ; directory_scope_total_data_ordinals
    5: uint,                        ; directory_scope_highest_protected_ordinal
    6: bool,                        ; is_final_directory
    7: bytes .size 32,              ; parity_map_payload_sha256
    8: bytes .size 32,              ; canonical_map_digest for this scope
}
```

**`parity_map` tape file (new kind = 3).** A filemark-delimited
control tape file, outside `ParityDataOrdinal` like bootstrap and
sidecar files, and — like sidecars — internally replicated:

```
parity_map tape file:
  block 0     .. M-1     primary parity-map header + canonical-CBOR payload
  block M     .. 2M-1    tail parity-map copy
  block 2M               parity-map footer locator
  filemark
M = 1 for small maps; otherwise M = ceil(encoded_map_file_bytes / block_size)
plus any required header space.

ParityMapPayload (canonical CBOR):
  1: tstr,         ; format_id = "rem-parity-map-v1"
  2: bytes .size 16, ; tape_uuid
  3: uint,         ; sequence
  4: SidecarEpochDirectory,
  5: bytes .size 32, ; canonical_map_digest for this scope
  6: ?tstr,        ; writer_version
  7: ?tstr,        ; write_timestamp

parity_map header/footer fields (LITTLE-ENDIAN; CRC-64/XZ):
  magic = HMAC-SHA256(tape_uuid, b"REM\x00PMAP\x01")[0..8]
  tape_uuid, sequence, payload_len, payload_sha256,
  canonical_map_digest,
  directory_scope_tape_file_count / _total_data_ordinals /
    _highest_protected_ordinal, is_final_directory,
  copy_kind = primary | tail,
  footer locator block carrying primary/tail starts + payload_sha256,
  CRC-64/XZ per header/footer block.
```

**Bootstrap requirements.** A *final* bootstrap MUST carry a
complete directory for the full committed prefix (inline or via
`parity_map`). An *intermediate* bootstrap SHOULD carry a prefix
directory for all sidecars committed before it, with
`is_final_directory = false`.

**Commit ordering when an external map is needed:**

```
1. Build the directory over the intended committed map scope.
2. Write the parity_map tape file (primary, tail, footer) + synchronous filemark.
3. Treat its map entry as provisional until the filemark returns (§7.5.2).
4. Encode the bootstrap with a ParityMapReference to it.
5. Write the bootstrap tape file + synchronous filemark.
6. Commit catalog_tape_files rows for parity_map + bootstrap in one transaction.
```

The canonical map digest includes the structural tape-file entries
for both the `parity_map` and the bootstrap (by `(tape_file_number,
kind, block_count)` only), never their payload bytes — avoiding
self-reference exactly as for the bootstrap (§5.6).

**Reader behavior.** On mount or catalog-less scan: use the
bootstrap's inline directory if present; else read and validate the
referenced `parity_map` (verify `payload_sha256` and
`canonical_map_digest`, falling back primary→tail→footer copy) and
use its directory; else the bootstrap is scheme-only and
catalog-less parity recovery is degraded. The `parity_map` is a
robustness accelerator and a damaged-sidecar-header repair source —
**not** the sole path: if it is unreadable, the scanner still
reconstructs a map from sidecar primary/tail/footer metadata (§8.1).

---

## 6. Public API

```rust
// crates/remanence-parity/src/lib.rs

pub use crate::sink::ParitySink;
pub use crate::source::{ParitySource, ObjectParitySource};
pub use crate::raw::{
    RawTapeSource, RawTapeSink, RawReadOutcome, RawWriteOutcome,
    SpaceFilemarksOutcome, PhysicalPositionHint, TapeGeometryHint,
};
pub use crate::model::{
    ParityScheme, SchemeId, StripeAddress, StripePosition,
    BodyPosition, TapeFilePosition,
    FilemarkMapDigest, FilemarkMap, ScopedFilemarkMap, MapScope,
    RecoveryEvent, RecoveryOutcome,
};
pub use crate::error::ParityError;

/// Default parity scheme for new tapes, parameterized by block
/// size so the contiguous-loss tolerance (`S × m × block_size`)
/// stays ~512 MiB regardless of block size. RS(128, 4), 3.125%
/// overhead. At rem-tar-v1's 256 KiB default this yields S=512;
/// at 1 MiB it yields S=128 (see §11.1). The single hardcoded
/// `default_scheme()` of v0.3.0 is replaced by this, since a
/// fixed S=512 silently halves tolerance if the block size
/// doubles.
pub fn default_scheme_for_block_size(block_size: u32) -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 128,
        parity_blocks_per_stripe: 4,
        stripes_per_epoch: stripes_for_tolerance(block_size, 512 * MIB, 4),
    }
}

/// A more conservative scheme for tapes expected to see harsh
/// storage conditions. RS(64, 6), ~9.4% overhead, ~384 MiB
/// tolerance. At 256 KiB this yields S=256.
pub fn conservative_scheme_for_block_size(block_size: u32) -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 64,
        parity_blocks_per_stripe: 6,
        stripes_per_epoch: stripes_for_tolerance(block_size, 384 * MIB, 6),
    }
}

/// S such that S × m × block_size >= target_loss_bytes — i.e.
/// the contiguous-loss tolerance is at least the target, never
/// silently below it. Ceiling division, clamped to >= 1.
const MIB: u64 = 1024 * 1024;
fn stripes_for_tolerance(block_size: u32, target_loss_bytes: u64, m: u32) -> u32 {
    let denom = m as u64 * block_size as u64;
    let s = target_loss_bytes.div_ceil(denom);   // ceiling: tolerance >= target
    s.max(1) as u32
}
```

### 6.1 `ParitySink`

Wraps an inner `RawTapeSink` and implements the body-facing
`BlockSink`. Object data blocks are forwarded
unchanged (preserving the clean pax tar stream) while being
accumulated into RS epochs by `ParityDataOrdinal`. Parity is
materialized as sidecar tape files at object boundaries. The
sink brackets each object with `begin_object`/`finish_object`
so it knows object boundaries (needed to place sidecars and
assign tape-file numbers).

```rust
pub struct ParitySink<'a> {
    inner: &'a mut dyn RawTapeSink,        // raw physical-tape sink (review #1)
    scheme: ParityScheme,
    tape_uuid: [u8; 16],
    parity_magic: [u8; 8],                 // HMAC(tape_uuid) for sidecar magic
    epoch_state: EpochState,               // accumulates RS by ParityDataOrdinal
    spool: ParitySpool,                    // disk spool for pending epoch parity (§7)
    filemark_map: FilemarkMapBuilder,      // records tape files as written
    next_bootstrap_sequence: u32,          // sink-owned (review #8); 0,1,2,...
    audit_hook: Option<Arc<dyn ParityAuditHook>>,
}

impl<'a> ParitySink<'a> {
    pub fn new(
        inner: &'a mut dyn RawTapeSink,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        spool_config: SpoolConfig,         // dir, max bytes (§7)
    ) -> Result<Self, ParityError>;

    /// Begin a new object archive. Returns its assigned
    /// tape_file_number. The body format then writes the object's
    /// blocks via the BlockSink impl below; they accumulate into
    /// the current epoch by ParityDataOrdinal AND pass through to
    /// `inner` unchanged. Before returning, checks the sidecar
    /// capacity reserve (§7); returns CapacityReserveExceeded if
    /// starting this object would leave no room for pending
    /// parity before EOM.
    pub fn begin_object(&mut self, projected_size_blocks: u64)
        -> Result<u32, ParityError>;

    /// Finish the current object. PRECONDITION: the body format
    /// has already flushed its final (zero-filled) block via
    /// BodyBlockWriter::finish_after_tar_eof() (rem-tar-v1 §9.1.1).
    /// finish_object() does NOT hold or flush any partial tar
    /// buffer; it asserts the object's block stream ended on a
    /// block boundary, writes the terminating filemark, and emits
    /// any parity-epoch sidecar tape files whose epochs COMPLETED
    /// during this object (from spool, §7). It does NOT flush the
    /// current partial epoch (§5.4 — that happens only at finish()).
    pub fn finish_object(&mut self) -> Result<ObjectCloseResult, ParityError>;

    /// Emit a NON-FINAL bootstrap tape file at the current
    /// position (writer policy, §7.3). The sink owns everything:
    /// it assigns the next monotonic bootstrap `sequence`
    /// (review #8 — the sink already owns tape-file numbering and
    /// digest construction, so it owns sequence too), assigns the
    /// bootstrap's tape_file_number, appends its structural map
    /// entry, computes the FilemarkMapDigest over the map INCLUDING
    /// that entry (§7.3.1) with is_final_map=false, and encodes it.
    /// The caller does NOT supply a digest or a sequence
    /// (review #1, #8). The final bootstrap is NOT written here —
    /// it is written by finish(). Bootstrap blocks get no
    /// ParityDataOrdinal and are not in RS epochs; written to
    /// `inner` directly, protected by replication (§5.6).
    pub fn write_bootstrap(&mut self)
        -> Result<u32, ParityError>;       // returns bootstrap tape_file_number

    /// End of tape: close the final partial epoch per §5.4
    /// (zero-pad to S×k for the RS math, emit its sidecar when
    /// D>0), then write the FINAL bootstrap (the sink assigns its
    /// sequence and stamps is_final_map=true; §7.3.1), and return
    /// the complete filemark map for catalog recording. Sequence
    /// numbering for all bootstraps — intermediate and final — is
    /// sink-owned, so there is no caller parameter (review #8).
    pub fn finish(self) -> Result<TapeGeometry, ParityError>;
}

pub struct ObjectCloseResult {
    pub object_geometry: ObjectGeometry,
    /// Sidecar tape files emitted at this object boundary (epochs
    /// that completed during this object). Each carries its
    /// catalog_tape_files row data. May be empty (small objects).
    pub sidecars_emitted: Vec<SidecarTapeFile>,
    /// The tape's protection watermark AFTER these sidecars. Layer
    /// 5 writes this into catalog_tapes.highest_protected_ordinal
    /// and recomputes object parity_state in the same transaction
    /// (§10.1). Derivable from sidecars_emitted, but surfaced
    /// explicitly to avoid transaction bugs (review).
    pub highest_protected_ordinal: u64,
}

pub struct ObjectGeometry {
    pub tape_file_number: u32,
    pub data_block_count: u64,             // every fixed block in the object tape file
    pub first_parity_data_ordinal: u64,
}

pub struct TapeGeometry {
    pub filemark_map: FilemarkMap,
    pub total_tape_files: u32,
    pub total_data_ordinals: u64,
}

// The BlockSink impl writes OBJECT data blocks within the
// current object. INVARIANT (§5.2): only object-archive blocks
// written through this path get a ParityDataOrdinal and feed
// epoch accumulation. Sidecar and bootstrap blocks are written
// by internal helpers that forward to `inner` directly and are
// explicitly NOT given an ordinal — otherwise sidecar bytes
// would corrupt the next epoch's parity.
impl<'a> BlockSink for ParitySink<'a> {
    /// MUST be called only between begin_object and finish_object.
    /// A write_block outside an active object is an `Invariant`
    /// violation (review #6): object data is the only thing that
    /// gets an ordinal, so there is no meaningful place to put a
    /// stray block. (Symmetric with the read side, where only an
    /// ObjectParitySource — never the tape-scoped ParitySink/Source
    /// — exposes a block-addressed I/O surface.)
    ///
    /// Returns a BODY-facing BodyWriteOutcome (review #1, #2): it
    /// carries the object-local BodyPosition after the write, NOT
    /// the raw RawWriteOutcome with physical position / EW / EOM.
    /// 3c consumes the raw outcome from `inner` internally (EOM is
    /// handled via the §7.5 reserve, not surfaced to body formats).
    fn write_block(&mut self, buf: &[u8]) -> Result<BodyWriteOutcome, FormatError>;
    /// Object-local BodyPosition (NOT physical TapePosition): body
    /// formats address by per-object BodyLba (review #6).
    fn position(&mut self) -> Result<BodyPosition, FormatError>;
    // NOTE (review #2): the body-facing BlockSink trait has NO
    // write_filemarks method. In the filemark-aware model filemarks
    // are a 3c / write-session concern, written by begin_object /
    // finish_object / write_bootstrap / emit_sidecar, never by a
    // body format. (If a legacy BlockSink definition still declares
    // write_filemarks, ParitySink's impl returns
    // FormatError::Invariant("body formats must not write filemarks");
    // the v0.4 body trait drops it entirely.)
}

pub struct BodyWriteOutcome {
    /// Object-local position after the write. No physical tape
    /// position, no EW/EOM — those live only on RawWriteOutcome.
    pub position_after: BodyPosition,
}
```

#### 6.1.1 Drive-compression precondition (normative)

3c's capacity reserve (§7.5) and contiguous-loss tolerance (§5.2.1,
quoted as `S × m × block_size` bytes) both assume a predictable
logical-block → physical-extent mapping. LTO hardware compression
destroys that mapping (physical consumption becomes data-dependent),
so it MUST be disabled for every parity-protected write session:

```
LTO hardware compression MUST be disabled for parity-protected writes.
Layer 3a MUST configure compression = false before any
  bootstrap/object/sidecar/parity_map write, and MUST read back and
  VERIFY the drive's effective compression mode after configuring it.
If compression cannot be disabled or cannot be verified, the write
  session MUST fail before writing the BOT bootstrap.
```

```rust
pub struct TapeWriteConfig {
    pub fixed_block_size: u32,
    pub drive_compression: bool,        // MUST be false for parity-protected writes
    pub hardware_encryption: EncryptionConfig,
}
```

The bootstrap and catalog record `drive_compression = false` and
the `fixed_block_size`. On mount, a parity-protected tape that
records `drive_compression = true` has non-authoritative parity
geometry: the reader MUST refuse 3c recovery for it unless a future
spec defines compressed-physical-extent semantics. In the production
workflow this is moot in practice — per-block AES (Layer 6) makes
data incompressible and compressible data is pre-compressed upstream
(§1) — but the precondition is stated and enforced so the reserve
and tolerance figures are always meaningful. The exact MODE SELECT /
MODE SENSE page used to disable and read back compression must be
proven on the deployed LTO-9 drive models (do not trust a config
variable alone; §11.6, §14).

### 6.2 `ParitySource`

Wraps an inner `RawTapeSource` and exposes object-scoped
`ObjectParitySource` (which implements the body-facing
`BlockSource`). Reads within an object archive are pure
passthrough — there are no parity blocks inside an object
to skip (§5.1), so a clean read of an object's tape file is
directly a valid pax tar stream. On a clean-read failure
(MEDIUM_ERROR / transport) or a body-format-reported CRC
mismatch (`recover_block_at`), 3c maps the address to a
`ParityDataOrdinal`, finds the stripe peers and the epoch's
sidecar via the filemark map, and reconstructs.

Constructed *after* the reader has discovered a bootstrap (for
the scheme) and obtained the filemark map (from the catalog, or
scan-reconstructed and digest-validated — §8.1).

```rust
pub struct ParitySource<'a> {
    inner: &'a mut dyn RawTapeSource,      // raw physical-tape source (review #1)
    scheme: ParityScheme,
    tape_uuid: [u8; 16],
    parity_magic: [u8; 8],
    filemark_map: ScopedFilemarkMap,       // map + scope; scope carries the
                                           // watermark (review #6 — single source
                                           // of truth, see scope.watermark())
    cache: StripeCache,
    audit_hook: Option<Arc<dyn ParityAuditHook>>,
}

impl<'a> ParitySource<'a> {
    /// The watermark is read from the scoped map, never stored
    /// separately (review #6). `ScopedFilemarkMap` already carries
    /// it in both the Complete and Prefix arms.
    pub fn new(
        inner: &'a mut dyn RawTapeSource,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        filemark_map: ScopedFilemarkMap,   // .scope.watermark() is the protection bound
    ) -> Result<Self, ParityError>;

    /// Open one object archive for reading. Returns an
    /// object-scoped source addressed by per-object `BodyLba`,
    /// which is what implements `BlockSource` (see below). This
    /// is the clean resolution of the address-space mismatch:
    /// `ParitySource` is tape-scoped and NOT itself a
    /// `BlockSource` (a flat `locate(lba)` cannot name
    /// `(tape_file_number, body_lba)`); the object-scoped view is.
    ///
    /// Trust boundary (review #6): if `tape_file_number` is in the
    /// VALIDATED prefix (scope Complete, or Prefix with
    /// `is_validated(tfn)` true), the object opens normally with
    /// parity recovery available within the watermark. If it is in
    /// the UNVALIDATED suffix of a Prefix map (a scan-reconstructed
    /// tape file the surviving digest did not authenticate), it
    /// opens in `TarOnlyUnverified` mode: plain reads work (the pax
    /// stream is self-describing), but the source refuses parity
    /// recovery (`OutsideValidatedMapPrefix`) and flags the handle
    /// so callers never treat its bytes as authenticated. A caller
    /// may also pass `RequireValidated` to reject such objects
    /// outright.
    pub fn open_object(&mut self, tape_file_number: u32, trust: OpenTrust)
        -> Result<ObjectParitySource<'_, 'a>, ParityError>;

    /// Bulk / epoch recovery (§9.2.1). Reconstruct a contiguous
    /// region (or an explicit ordinal range) by planning per epoch:
    /// load each affected sidecar once, deduplicate and physically
    /// order the peer reads, and reconstruct all affected stripes
    /// together — the contiguous-damage recovery path that
    /// `recover_block_at` is too LOCATE-heavy for. Bounded by
    /// `BulkRecoveryPolicy::max_recovery_cache_bytes`, with windowed
    /// multi-pass fallback (§9.3).
    pub fn recover_region(&mut self, req: RecoveryRegionRequest)
        -> Result<impl Iterator<Item = RecoveredBlock>, ParityError>;
    pub fn recover_ordinal_range(&mut self, ordinals: Range<ParityDataOrdinal>)
        -> Result<impl Iterator<Item = RecoveredOrdinalBlock>, ParityError>;
}

pub enum OpenTrust {
    /// Reject objects outside the validated prefix.
    RequireValidated,
    /// Allow opening an unvalidated-suffix object in tar-only,
    /// no-parity, explicitly-unauthenticated mode.
    AllowTarOnlyUnverified,
}

/// Object-scoped reader over one object archive's BodyLba space.
/// THIS is the `BlockSource` the body format consumes. All
/// addressing is object-local; the parent `ParitySource` and the
/// filemark map translate to physical positions and to
/// `ParityDataOrdinal` for recovery.
pub struct ObjectParitySource<'p, 'a> {
    parent: &'p mut ParitySource<'a>,
    tape_file_number: u32,
    object_first_ordinal: u64,
    object_block_count: u64,
}

impl<'p, 'a> ObjectParitySource<'p, 'a> {
    /// Forced erasure recovery for the clean-read-but-CRC-failed
    /// case (rem-tar-v1 §13.2). Treats this object-local block as
    /// an erasure: resolve body_lba → ParityDataOrdinal, locate
    /// stripe peers (possibly across filemarks in other objects)
    /// and the parity shards in the epoch's sidecar, reconstruct.
    /// Returns `Unrecoverable` (→ caller falls back to another
    /// copy) if the object is only parity-pending (§7.2.1) — its
    /// epoch has no sidecar yet — or if >m shards are lost.
    pub fn recover_block_at(&mut self, body_lba: u64)
        -> Result<Vec<u8>, ParityError>;
}

impl<'p, 'a> BlockSource for ObjectParitySource<'p, 'a> {
    /// Object-local: 0 <= body_lba < object_block_count. Returns a
    /// BodyPosition, not a physical TapePosition (review #6).
    fn locate(&mut self, body_lba: u64) -> Result<BodyPosition, FormatError>;
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, FormatError>;
    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, FormatError>;
    /// Object-local BodyPosition (NOT physical TapePosition).
    fn position(&mut self) -> Result<BodyPosition, FormatError>;
}
```

The three position types are kept distinct so physical semantics
never leak into body formats (review #6):

```rust
/// Object-local address a body format uses. The ONLY position
/// type a BlockSource/BlockSink exposes.
pub struct BodyPosition { pub body_lba: u64 }

/// Physical block-address hint for RawTapeSource seeks (§4.5).
pub struct PhysicalPositionHint { /* drive READ POSITION value */ }

/// Logical tape-file address in the filemark map.
pub struct TapeFilePosition { pub tape_file_number: u32, pub block_within_file: u64 }
```

`TapePosition` is no longer used as a return type on the
body-facing `BlockSource`/`BlockSink` traits; those return
`BodyPosition`. `RawTapeSource` uses `PhysicalPositionHint`, and
the filemark map speaks `TapeFilePosition`. 3c internally
translates `BodyPosition` → `TapeFilePosition` →
`PhysicalPositionHint` via the filemark map; the body format
only ever sees `BodyPosition`.

Within an object there are no parity blocks to skip (§5.1), so
clean reads through `ObjectParitySource` are pure passthrough;
recovery is the only path that consults the filemark map and the
epoch's sidecar.

### 6.3 Bootstrap discovery and filemark-map acquisition

Finding a bootstrap is a prerequisite to constructing a
`ParitySource` — the source needs the scheme *and* the filemark
map. Discovery uses the *inner* source directly (no parity
reconstruction available yet), and separates two roles (§8.1):

```rust
/// Step 1: ANY valid bootstrap gives the scheme, tape_uuid, and
/// block_size. First valid copy (BOT in the common case) wins.
pub fn discover_scheme(
    raw: &mut dyn RawTapeSource,
    hint: Option<TapeGeometryHint>,
) -> Result<BootstrapPayload, ParityError>;

/// Step 2: the AUTHORITATIVE bootstrap for map validation — the
/// highest-scope valid copy (is_final_map, else highest sequence).
/// Needed for catalog-less recovery; skippable when the catalog
/// supplies the map.
pub fn discover_authoritative_bootstrap(
    raw: &mut dyn RawTapeSource,
    hint: Option<TapeGeometryHint>,
) -> Result<BootstrapPayload, ParityError>;

/// Obtain the filemark map, scoped to what is actually validated.
/// Preferred: from the catalog (Complete). Fallback (catalog-less):
/// scan-reconstruct and validate against the AUTHORITATIVE
/// bootstrap's digest — fully if is_final_map, else only the
/// attested prefix (MapScope::Prefix), bounding recovery (§8.1).
pub fn acquire_filemark_map(
    raw: &mut dyn RawTapeSource,
    authoritative_bootstrap: &BootstrapPayload,
    catalog_map: Option<FilemarkMap>,
) -> Result<ScopedFilemarkMap, ParityError>;
```

Step 1's bootstrap gives the scheme and `tape_uuid`; step 2's
gives the digest that validates the map. If the catalog is
available, its `catalog_tape_files` rows are the map (step 2 can
be skipped). If not, `acquire_filemark_map` scan-reconstructs by
walking the tape file by file, classifying each by magic (Option
B, §8.1), then checks it against the authoritative digest. Only
after both the scheme and a validated map are in hand is a
`ParitySource` constructed.

### 6.4 Composition with body formats

```rust
// In the daemon's write-session setup:
let mut drive_sink  = DriveHandleRawSink(&mut drive);   // impl RawTapeSink (§4.5)
let mut parity_sink = ParitySink::new(&mut drive_sink, scheme, tape_uuid, spool_cfg)?;

// Bootstrap copy 0 at BOT, before any object. The sink owns the
// sequence and computes the (empty-prefix) digest internally;
// is_final_map=false. No caller arguments (review #1, #8).
parity_sink.write_bootstrap()?;

for object in objects_to_write {
    // 3c brackets the object; capacity reserve checked here (§7).
    let tape_file_number = parity_sink.begin_object(object.projected_size_blocks())?;

    // The body format writes the object's blocks via the BlockSink
    // impl; on close it flushes its OWN final block (rem-tar-v1
    // §9.1.1) BEFORE 3c writes the filemark.
    let mut writer = format.begin_object_write(&mut parity_sink, &object, params)?;
    writer.write_all()?;                 // emits blocks + tar EOF + final-block flush
    let close = parity_sink.finish_object()?;   // filemark + completed-epoch sidecars
    layer5.record(tape_file_number, close);      // catalog_tape_files rows, etc.

    // Periodic bootstrap (writer policy, §7.3). The sink assigns
    // the next sequence and stamps the current prefix digest
    // (map_total_data_ordinals + highest_protected_ordinal as of
    // now), is_final_map=false; the reader prefers the final copy.
    if should_write_bootstrap_now(&parity_sink) {
        parity_sink.write_bootstrap()?;
    }
}

// finish(): closes the final partial epoch if D>0 (§5.4), emits
// its sidecar, writes the final bootstrap with is_final_map=true
// and a digest over the COMPLETE canonical map projection (§5.6),
// returns the complete filemark map for the catalog.
let geometry = parity_sink.finish()?;
layer5.record_tape_geometry(geometry);
```

The body format does not know whether its `BlockSink` is a
`ParitySink` or a plain drive adapter; it sees only per-object
`BodyLba`/`BodyPosition`. The `begin_object`/`finish_object`
brackets and bootstrap writes are the write-session manager's
concern, not the body format's.

---

## 7. Write flow

### 7.1 Epoch accumulation (incremental parity)

**Implementation model: incremental parity accumulation
(Option B).** The sink does *not* buffer the epoch's `S × k`
data shards (~16 GiB at default) to encode them in a single
black-box call at epoch completion. Instead it keeps only the
**parity accumulators** — `S × m` shards, `2048 × 256 KiB` ≈
512 MiB at default — and updates them as each data shard
arrives. This is what keeps the "never spool object data" claim
(§7.4) true: object data is forwarded to tape and dropped from
memory immediately; only ~512 MiB of evolving parity is held.

The cost of Option B is that it is *not* a plain
`reed_solomon_erasure::encode()` call (which expects all data
shards present at once). It requires either an incremental
encoder or a direct use of the generator-matrix GF(2⁸)
operations: for a systematic Cauchy/Vandermonde RS code, each
parity shard `p_j` of a stripe is a fixed linear combination
`p_j = Σ_i g_{j,i} · d_i` over GF(2⁸), so each arriving data
shard `d_i` contributes `g_{j,i} · d_i` into each accumulator
`p_j` independently of shard arrival order. The generator
coefficients `g_{j,i}` are defined by **normative Appendix A**
(`rs-cauchy-gf256-v1`: GF(2⁸)/`0x11D`, the Cauchy matrix
`G[j][i] = 1/(X_j XOR Y_i)`), NOT by whatever
`reed-solomon-erasure` happens to use internally — the on-tape
scheme ID binds to the appendix, not to a crate version. An
implementation MAY use the crate for the decode/`reconstruct`
path, but only after the impl-step-11.6 conformance test proves
the crate's field, matrix, and shard ordering match Appendix A
byte-for-byte; otherwise it MUST use its own decoder over the
appendix's definition.

(Option A — buffer/spool the full 16 GiB of current-epoch data
shards and call the black-box encoder at completion — is simpler
and matches the crate API directly, but then the "spool stages
only parity, never object data" claim is false: the spool would
transiently hold up to 16 GiB of object data per open epoch.
v1 chooses Option B; an implementation that finds the
incremental encoder too costly may fall back to Option A and
must then state the 16 GiB transient-data-buffer requirement
honestly and revise §7.4 accordingly.)

At write-session open the sink allocates the `S × m` parity
accumulators (~512 MiB), zeroes them, and initializes the global
`ParityDataOrdinal` counter and epoch counter to 0.

Object data flows in only between `begin_object` and
`finish_object`. For each `ParitySink::write_block(buf)` on an
object's blocks:

1. Forward `buf` to `inner.write_block(buf)` unchanged — the
   block lands on tape in the object's clean pax tar stream —
   then drop it (no retention of object data).
2. Assign it the next `ParityDataOrdinal` `o`; compute its
   stripe via `ordinal_to_stripe(o)` (§5.3). For each of the
   stripe's `m` parity accumulators `p_j`, add `g_{j,data_index}
   · buf` (GF(2⁸)) into `p_j`.
3. If this was the epoch's `(S × k)`-th data shard, the epoch is
   **complete**: its `m × S` parity accumulators are now the
   finished parity shards. Move them to the pending-sidecar
   spool (just the ~512 MiB of parity, §7.4) for emission at the
   next object boundary (§7.2), then zero the accumulators and
   start a new epoch.
4. Return `BodyWriteOutcome { position_after }` (object-local;
   the raw outcome from `inner.write_fixed_block` — physical
   position, EW/EOM — is consumed internally, §6.1).

Sidecar and bootstrap blocks do **not** go through this path
(§5.2); they are written by internal helpers that forward to
`inner` without an ordinal and without touching the
accumulators.

### 7.2 At `begin_object` / `finish_object` / `finish`

**`begin_object(projected_size_blocks)`**: assign the next
`tape_file_number`; record the object's `first_parity_data_
ordinal` (the current global ordinal) and remember
`projected_size_blocks`, which MUST be a conservative **upper
bound** on the object's block count, not a best estimate (§7.5).
Check the sidecar capacity reserve (§7.5) and reject with
`CapacityReserveExceeded` if starting this object would leave no
room for the parity it will generate *plus* already-pending
parity before EOM.

**`finish_object()`**: the body format has already flushed its
final block (rem-tar-v1 §9.1.1). The sink asserts the object's
stream ended on a block boundary (and that the object did not
exceed its declared `projected_size_blocks` — exceeding it is an
`Invariant` violation, not a recoverable error, because the
reserve was computed against the projection). It writes the
terminating filemark, then **emits a parity-epoch sidecar tape
file for each epoch that completed during this object** (its
parity is in the accumulator/spool from §7.1). It does **not**
flush the current partial epoch — that spans into the next
object (§5.4, the "pending epoch = completed-but-deferred" rule).

Crucially, `finish_object` does **not** by itself make the
object parity-protected. If the object's last blocks landed in
an epoch that has not yet completed, those tail ordinals have no
sidecar — so the object is `partial` (early epochs protected,
tail open) or, for a small object wholly inside one open epoch,
`pending`, until a later object (or `finish()`) completes the
covering epoch. See §7.2.1 for the per-block protection model
the catalog and recovery path must use. `finish_object` returns
the object geometry, the sidecar records emitted at this
boundary, and the current `highest_protected_ordinal` so Layer 5
can compute each object's parity state.

**`finish()`** (tape close): if the current epoch has `D > 0`
real data shards, close it per §5.4 (logically zero-pad to
`S × k`, compute parity, emit the final sidecar recording
`real_data_shard_count = D`), which advances
`highest_protected_ordinal` to the end of all written object
data. **If `D == 0`** (the last epoch boundary fell exactly on
the last object's close), no final partial-epoch sidecar is
written — there is nothing to protect. Then write the final
bootstrap (`is_final_map = true`, digest covering the complete
filemark map; §5.6, §7.3) and return the `TapeGeometry`. After
a successful `finish()`, every object on the tape is
parity-protected. If the write session dies before `finish()`,
the trailing objects are `pending` or `partial` (per §7.2.1),
and the catalog MUST reflect that rather than marking them
protected.

### 7.2.1 Object parity-protection lag (normative)

Because filemarks do not flush epochs (§5.1), an object can be
fully written to tape — tar archive plus terminating filemark —
while some or all of its data blocks still live in an **open
epoch** that has no sidecar yet. Such an object is readable by
standard `tar` but is **not yet RS-protected on this tape**.
Conflating "on tape" with "protected" would let the catalog
claim protection that does not exist; the model below prevents
that.

Object lifecycle states (Layer 5 owns the transitions):

```
ObjectDataCommitted
    tar archive + terminating filemark are on tape; the object
    is standard-tar readable. Its ordinals may still be in an
    open (sidecar-less) epoch.

ObjectParityPending
    NONE of the object's ordinals are below the watermark yet —
    its whole range is in open/un-emitted epochs. Readable, not
    RS-protected on this tape.

ObjectParityPartial
    SOME of the object's ordinals are below the watermark and
    some are not — the common state for a large object that
    completed several epochs (their sidecars emitted, so those
    ordinals are protected) while its tail sits in the current
    open epoch. Early blocks ARE recoverable; tail blocks are
    not, until a later sidecar covers them.

ObjectParityProtected
    EVERY ordinal in the object's range is at or below the
    watermark:
        first_parity_data_ordinal + data_block_count
            <= tape.highest_protected_ordinal

ObjectCatalogCommitted
    catalog rows reflect the correct parity state for this object
    and its tape files, written in one transaction.
```

The tape carries a single monotonic watermark, advanced only
when a sidecar is emitted:

```
tape.highest_protected_ordinal
    = the exclusive upper bound of the ordinal range for which a
      parity sidecar has been written. Starts at 0; advanced by
      each emitted epoch sidecar (completed mid-object, at
      finish_object, or the final partial epoch at finish) to
      that epoch's protected_ordinal_end_exclusive.
```

The catalog records, per object: `first_parity_data_ordinal`,
`data_block_count`, an `ordinal_end_exclusive`
(= `first_parity_data_ordinal + data_block_count`), and a
derived `parity_state ∈ {pending, partial, protected}`; and per
tape: `highest_protected_ordinal`. Given the watermark `W`:

```
protected  iff  ordinal_end_exclusive <= W
pending    iff  first_parity_data_ordinal >= W
partial    otherwise (straddles the watermark)
```

When a later sidecar advances `W`, Layer 5 recomputes affected
objects' states (a range update keyed on the ordinal columns).

**Recovery is per-block, not per-object (the critical rule).**
Whether a *specific* damaged block can be RS-recovered depends on
*that block's* ordinal, not the object's overall state:

```
a failed block at failed_ordinal is RS-recoverable on this tape
    iff  failed_ordinal < tape.highest_protected_ordinal
```

So an object in `partial` state recovers its early (sub-watermark)
blocks normally and falls back to another copy only for its
tail. The recovery path (§8.3 step a0) MUST test the failed
block's ordinal against the watermark, never the whole object's
range — testing the object's range would wrongly refuse to
recover the protected early blocks of a large `partial` object.
The `parity_state` column is operator-facing summary state; the
per-block ordinal test is the authority for an actual recovery.

Operational consequence: after a clean `finish()`, the watermark
reaches the end of the tape and every object is `protected`. A
crashed session leaves a tail of `pending`/`partial` objects —
acceptable (still on tape and tar-readable, three-copy policy
still applies), provided the catalog does not pretend the
uncovered ordinals are protected.

### 7.3 Bootstrap placement policy

The write-session manager (Layer 5), not the parity sink,
decides when to emit bootstrap tape files, because placement
depends on operational concerns (how many copies, acceptable
uncovered fraction).

Default policy: **a bootstrap copy at BOT, and additional copies
at roughly the 1/3, 2/3, and near-EOD points** of the tape,
emitted at object boundaries (never mid-object). This gives a
handful of well-dispersed copies; the final copy at tape close
carries the digest of the complete filemark map.

Bootstraps are their own filemark-delimited tape files (§5.6),
written between objects so they never interrupt an object's pax
tar stream. They are not parity-protected (the chicken-and-egg
of §5.6); replication is their protection. The reader relies on
finding *a* valid copy by magic-scan at the expected positions.

**Filemark ownership of non-object tape files.** To remove any
ambiguity about who writes the separating filemark after a
non-object tape file: `write_bootstrap()` writes exactly one
bootstrap tape file *and its trailing filemark*; the internal
`emit_sidecar()` writes exactly one sidecar tape file *and its
trailing filemark*. `finish_object()` writes the object's
trailing filemark. Every tape file — object, sidecar, bootstrap
— is terminated by exactly one filemark written by whoever wrote
the tape file, so the filemark map is unambiguous.

#### 7.3.1 Writing a bootstrap with a map digest (normative order)

A bootstrap commits to a filemark map that *includes the
bootstrap's own tape-file entry* (especially the final one,
whose digest covers the whole tape). The canonical projection
(§5.6) is non-circular because it excludes content hashes, but
the writer must still build its own structural entry before
hashing. The exact order:

```
0. Assign this bootstrap its `sequence` = next_bootstrap_sequence,
   then increment that sink-owned counter (review #8). Copy 0 at
   BOT gets sequence 0; intermediate copies 1, 2, …; the final
   bootstrap (written by finish()) gets the next sequence after
   the last intermediate. The caller never supplies a sequence.
1. Assign this bootstrap its tape_file_number (next tape file).
2. Append its STRUCTURAL map entry to the in-memory map:
     kind = bootstrap, block_count = 1
     (no content hash; bootstraps are one block, §5.6).
3. Compute map_sha256 = SHA-256(canonical_projection(map_so_far))
   over the map INCLUDING that entry (§5.6 projection rules).
4. Set the FilemarkMapDigest fields:
     map_sha256, tape_file_count,
     map_total_data_ordinals  (object data described so far),
     highest_protected_ordinal (= max protected_ordinal_end_
        exclusive over sidecars so far; the protection watermark),
     is_final_map  (= true only for the finish() bootstrap).
5. Encode the BootstrapPayload (scheme + digest) and write the
   bootstrap tape file + trailing filemark (write_bootstrap).
6. OPTIONALLY compute the bootstrap tape-file's own sha256 after
   writing, for catalog_tape_files.sha256 — this content hash is
   NEVER part of the map digest (that is what keeps step 3
   non-circular).
```

For the **final** bootstrap (written by `finish()`), the map at
step 2 already contains every object, sidecar, and prior
bootstrap entry, plus the final partial-epoch sidecar if one was
emitted (§7.2); the final bootstrap's own entry is appended last,
and `is_final_map = true`. Intermediate bootstraps follow the
same six steps with `is_final_map = false`; their digest
attests the prefix of the map known at that point (§8.1 prefix
validation).

### 7.4 The sidecar spool and sidecar clustering

A single huge object can complete several epochs before its
closing filemark, but a sidecar cannot be written *inside* the
object's tape file (that would corrupt its pax tar stream). So
each completed epoch's **parity** (the ~512 MiB of finished
accumulators, §7.1) is moved to a local-disk spool as it
completes mid-object, and the pending sidecar tape files are
emitted at `finish_object`. Per §7.1's Option B, the spool holds
**only parity, never object data** — ~3.125% of the object at
`k=128, m=4`, so ~32 GiB of spooled parity for a 1 TiB object.

Spool guardrails (mechanism fixed; thresholds are an
implementation/config detail): a free-space check before
spooling; a configurable spool directory and `max_spool_bytes`
cap; and crash semantics — if the writer dies mid-object with
spooled parity, the partial object is unrecoverable anyway
(rem-tar-v1 has no resumable write), so the spool is discarded
and the object rewritten on retry. On akash this spools to fast
local ZFS.

**No volatile sidecar deferral (v0.5).** A completed epoch's parity
is held in the local-disk spool until the object closes, then *all*
of that object's completed-epoch sidecars are emitted at
`finish_object`. v1 does **not** defer completed sidecars across
later object boundaries: deferral would leave completed parity
living only in volatile spool, so a crash could force restart to
re-read many completed epochs (an early draft's
`max_deferred_sidecars = 64` implied a ~1 TiB rebuild). The simpler,
safer rule is to bound what restart must rebuild to a single partial
epoch — see the object commit bundle below and §7.8.

**Object commit bundle (normative).** The object, every
completed-epoch sidecar it produced, and any bootstraps/`parity_map`
written during emission are committed as one atomic catalog
transaction:

```
At rem-tar object close:
  1. rem-tar writes tar EOF + final post-EOF zero-fill (rem-tar §9.1.1).
  2. 3c writes the object tape file's synchronous filemark (§7.7).
  3. 3c emits every completed-epoch sidecar generated by this object.
  4. 3c MAY write bracketing bootstraps/parity_map during a long burst.
  5. Layer 5 commits the object + sidecars + bootstraps as ONE
     ObjectCommitBundle transaction (after all filemarks have returned).

All tape files in the bundle are provisional (§7.5.2) until their
fixed blocks and synchronous filemarks have returned. There is no
catalog state where an object is committed but completed sidecars it
generated are absent.
```

The only parity state allowed to remain unprotected after a
committed bundle is the current **partial** epoch that has not yet
reached a full sidecar. This is the hard v1 invariant (also asserted
on restart, §7.8, and tested, §14):

```
After any committed object bundle:
    total_committed_ordinals − highest_protected_ordinal
        < data_ordinals_per_epoch
```

**Sidecar clustering risk (accepted, now metadata-recoverable).**
Because a large object's completed-epoch sidecars are still emitted
together at `finish_object`, they form a contiguous cluster right
after that object, and a localized damage event landing on the
cluster can remove the *parity payload* for many epochs at once. v1
accepts this, but the v0.5 mitigations make it far less sharp than
in v0.4.4: sidecar metadata is replicated (primary + tail + footer,
§5.5) and the bootstrap/`parity_map` directory (§5.6.1) is the
root-of-trust, so a damaged cluster no longer poisons catalog-less
parity recovery for the *whole tape* — at worst the clustered epochs
become parity-unavailable on this copy and fall back to another of
the three copies (§16). Dispersing sidecars would only change *which*
epochs a single damage event hits, not whether their lost shards are
recoverable (the other copy is the backstop either way), so v1 does
not contort the write path to disperse — it makes the metadata
robust and accepts the shard loss falls to multi-copy. An
implementation MUST NOT interleave a sidecar inside an object's tape
file, but SHOULD write a bracketing bootstrap/`parity_map` every
`bootstrap_every_sidecars` (default 8) during a long burst:

```rust
pub struct SidecarBurstPolicy {
    /// Write a non-final bootstrap (or parity_map+bootstrap) after every
    /// N sidecars in a burst. Default: 8.
    pub bootstrap_every_sidecars: u32,
    /// If a single object is projected to produce more than this many
    /// sidecars, emit an audit warning / optionally require operator
    /// approval. Default: 64.
    pub large_object_sidecar_warning_threshold: u32,
}
// There is no max_deferred_sidecars in v1 (deferral is prohibited).
```

**Large-object operational rule.** A crash *during* a huge object's
trailing sidecar burst abandons the whole (uncommitted) bundle and
the orchestrator rewrites the object from the beginning (§7.8) — a
cost that scales with object size. The mitigation is upstream
splitting: if a single object would generate many sidecars, Layer 5
SHOULD split the source into multiple Remanence objects, unless the
operator explicitly accepts the cluster/rewrite risk. If rem-tar
cannot split an indivisible single file, the write may proceed if it
fits capacity and spool reserve, but the audit log must record
`LargeObjectSidecarClusterRisk { object_id,
projected_completed_epochs, projected_sidecar_cluster_bytes,
bundle_commit_required: true }`.

**Future deferral (not v1).** A later version may add deferral only
if (A) the deferred spool is made crash-durable (fsynced, reloaded
on resume instead of re-reading object data), or (B) a multi-epoch
rebuild is explicitly supported, recorded as a catalog rebuild span,
budgeted against operator reserve/time, and proven on hardware, or
(C) deferral is bounded to a very small value (`max_deferred_sidecars
<= 4`) with the corresponding rebuild volume accepted. Until one of
these is designed and tested, v1 uses no volatile deferral.

### 7.5 Sidecar capacity reservation

Deferring parity to sidecars introduces a problem inline parity
did not have: the tape can fill with object data and leave no
room for the sidecars that protect it. And — the subtlety the
v0.3.0 draft missed — the reserve must include not only parity
that is *already* pending, but the parity *this object will
generate* as it fills epochs. For a large object that is the
dominant term: a 1 TiB object completes ~64 epochs and so will
emit ~32 GiB of sidecars *after* it closes.

`begin_object(projected_size_blocks)` therefore computes the
reserve that will be outstanding once this object is written.
The per-sidecar size is the **full tape-file size**, not just
the parity shards — it includes the header/index block(s) and
the trailing filemark (review #6):

```
sidecar_tape_file_blocks =
      2 * shard_index_block_count             // H primary + H tail (§5.5)
    + parity_shards_per_epoch                 // S × m raw parity shard blocks
    + 1                                       // footer locator block (§5.5)

sidecar_tape_file_bytes =
      sidecar_tape_file_blocks * block_size
    + filemark_overhead_estimate              // the trailing filemark

epochs_completed_by_this_object =
    floor((current_epoch_fill + projected_size_blocks)
          / data_shards_per_epoch)            // S × k

reserve_after_object =
      object_filemark_overhead_estimate             // THIS object's trailing
                                                    // filemark (review #3)
    + pending_completed_sidecar_tape_files_bytes    // not yet emitted
    + epochs_completed_by_this_object * sidecar_tape_file_bytes
    + final_partial_epoch_sidecar_tape_file_bytes   // the tail at finish (§5.4)
    + parity_map_tape_files_bytes                   // §5.6.1, if directory spills
    + remaining_bootstrap_tape_files_bytes          // §7.3; INCLUDES each
                                                    // bootstrap block + its
                                                    // trailing filemark
    + safety_margin
```

The per-sidecar size now counts the **replicated** metadata
(`2H` header/index blocks) plus the one-block footer (§5.5), not
just `H + S×m`. If the sidecar directory spills to a `parity_map`
(§5.6.1), its tape file (primary + tail + footer + filemark) is
reserved too.

**Object-too-large preflight (no spanning).** rem-tar v1 objects do
not span tapes (§5.4). Before writing an object to any tape, Layer 5
computes the full footprint — `projected_object_blocks +
object_filemark_overhead + sidecars_completed_by_this_object_worst_
case (replicated) + final_partial_epoch_sidecar_worst_case +
parity_map/bootstrap overhead + safety_margin` — and if it cannot fit
on an *empty* tape, rejects the object before writing any block with
`BeginObjectError::ObjectTooLargeForEmptyTape { projected_object_
blocks, empty_tape_usable_blocks, required_reserve_blocks }`. Such an
object must be split upstream by the orchestrator.

(`projected_size_blocks * block_size` counts the object's data
blocks only; its trailing filemark, written by `finish_object`,
is a separate cost — `object_filemark_overhead_estimate`, review
#3. Likewise each `sidecar_tape_file_bytes` and each remaining
bootstrap entry already include their own trailing filemark.)

and admits the object only if

```
remaining_tape_capacity
    >= projected_size_blocks * block_size + reserve_after_object
```

**Local-disk spool reserve.** Separately from tape capacity, the
parity that this object will spool to disk before its sidecars
are emitted must fit the spool (§7.4):

```
spool_needed_after_object =
      pending_completed_epoch_parity_bytes
    + epochs_completed_by_this_object * sidecar_tape_file_bytes
```

A 1 TiB object needs ~32 GiB of parity spool; a multi-TiB object
proportionally more. `begin_object` checks this against
`max_spool_bytes` / free disk and returns
`CapacityReserveExceeded { cause: ParitySpoolCapacity, ... }` if it
won't fit, so the shortfall is caught before the object starts,
not mid-stream. A tape-space failure uses
`CapacityReserveExceeded { cause: TapeCapacity, ... }`; Layer 5
surfaces these differently because the remedies differ (free local
spool disk vs. close this tape and continue on another tape).

**No mid-object tape spanning (review #7).** If `begin_object`
fails the tape-capacity check, **the object has not started** —
no blocks were written. Layer 5 closes the current tape cleanly
(emit pending sidecars + final bootstrap) and writes the *entire
object from the beginning* on another tape. rem-tar-v1 is
one-object-one-pax-archive and does **not** support mid-object
tape spanning; an object never straddles a tape boundary. If
`projected_size_blocks` exceeds the usable capacity of an *empty*
tape, the object cannot be stored as a single archive and the
orchestrator must reject it or split it into smaller objects
upstream — 3c does not split objects.

`projected_size_blocks` MUST be a conservative upper bound. If
the body format writes more blocks than projected, `ParitySink`
raises an `Invariant` violation at `write_block`/`finish_object`
rather than silently overrunning the reserve — discovering a
capacity shortfall mid-object, after parity for earlier epochs
has already been spooled, is too late to recover cleanly. Layer
5 obtains the upper bound from the object's pre-write size (it
knows the source size before streaming; rem-tar-v1's two-pass
large-file path also yields it) and pads it for headers,
manifest, and alignment.

#### 7.5.1 Early-warning / end-of-medium handling (normative, review #2)

The capacity reserve (§7.5) is designed to prevent surprise EOM,
but a real drive can assert EARLY WARNING (EW) or END OF MEDIUM
(EOM) earlier than the model predicts (vendor slack varies). The
`RawWriteOutcome` flags (§4.5) drive a fixed policy:

```
Phase                          EW asserted                EOM asserted
-----------------------------  -------------------------  ----------------------------
begin_object reserve check     reject object before any   reject object before any
(pre-write)                    block (CapacityReserve-    block; close tape; rewrite
                               Exceeded); close tape       whole object on next tape

object data block write        continue ONLY if the       hard FormatError; object is
                               remaining reserve still     incomplete; NO catalog commit
                               covers this object's        for it; tape marked dirty;
                               trailing filemark + its     write-session aborts (the
                               pending sidecars + the      partial object is abandoned,
                               final bootstrap; else        not spanned — §7.5)
                               abort session, mark tape
                               dirty/incomplete

object trailing filemark       finish the filemark;        treat as object-data EOM:
(finish_object)                proceed to drain pending    object incomplete, no commit,
                               sidecars if reserve holds   session dirty

sidecar write                  finish this sidecar tape    if the sidecar cannot be
                               file if reserve holds;      completed, its epoch has no
                               then write final bootstrap  parity on this tape; mark
                               and finish the tape          that epoch's objects parity-
                                                           pending in the catalog and
                                                           rely on another copy (§7.4)

bootstrap / final bootstrap    finish the bootstrap tape   if even the final bootstrap
write                          file (it is one block +     cannot be written, the tape
                               filemark; the reserve        is dirty: the last fully-
                               always holds it back)        written intermediate bootstrap
                                                           is the authoritative copy, and
                                                           Layer 5 records the session as
                                                           incomplete
```

The guiding rule (consistent with the parity-lag model of
§7.2.1, review #6): an object is **data-committed** once its tar
archive and trailing filemark are durably on tape, and the
catalog commit records the object's *accurate* `parity_state` at
that moment — `pending`, `partial`, or `protected` — based on how
many of its ordinals the watermark covers. An object does NOT
have to be fully parity-protected to be data-committed and
catalog-visible (filemarks don't flush epochs, so a freshly
closed object's tail is normally still in an open epoch). The
sidecars for epochs that *completed* during the object must each
be either durably emitted or explicitly recorded unavailable
before that object's catalog row is written, so the recorded
state is truthful. Full protection is still exactly:

```
parity-protected  iff  ordinal_end_exclusive <= highest_protected_ordinal
```

and is reached later, when a subsequent sidecar (or `finish()`)
advances the watermark past the object's range (§7.2.1). Anything
interrupted by EOM before the object's data and filemark are
durable is abandoned cleanly and rewritten whole on another copy,
never half-spanned.

#### 7.5.2 Provisional map entries and write-failure (review #4)

The filemark-map builder (§7.3.1) appends a tape file's
structural entry *before* the tape file is fully written —
necessarily so for a bootstrap, whose digest must include its own
entry. Such entries are **provisional** until the tape file and
its trailing filemark are confirmed written:

```
A MapBuilder entry for an object, sidecar, or bootstrap is
PROVISIONAL until write_fixed_block(s) + write_filemark for that
tape file all return success (no EOM mid-file).

On any write failure (EOM, transport, medium) before a tape
file's filemark is durably written:
  - the entry remains provisional and is NOT promoted;
  - the write session is marked dirty / incomplete;
  - finish() does NOT write a final bootstrap from a dirty map;
  - NO catalog commit is made from provisional entries;
  - recovery uses the last COMMITTED state — the catalog, or the
    last fully-written (intermediate) bootstrap whose digest
    covers only its committed prefix (§8.1 prefix validation).
```

So a crash or EOM can leave a provisional bootstrap entry in the
in-memory map, but it never reaches tape as an authoritative
(final) digest and never reaches the catalog. A reader recovering
such a tape falls back to the newest *committed* bootstrap, whose
prefix digest correctly excludes the never-completed tail — which
is exactly what the prefix-scope machinery (§8.1, `MapScope::
Prefix`) already handles. Provisional-entry rollback and prefix
validation are therefore the same mechanism viewed from the write
and read sides.

### 7.6 Performance

With incremental accumulation (§7.1), parity work is spread
across the epoch rather than batched at completion: each
arriving data shard contributes `m` GF(2⁸) multiply-accumulates
into its stripe's parity accumulators. The total GF work per
epoch is the same as a batch encode — `S × m` parity shards
derived from `S × k` data shards, ~16 GiB of throughput at
default geometry — and the `reed-solomon-erasure` crate's
Cauchy GF(2⁸) arithmetic sustains ~3 GB/s per core, so the ~5 s
of compute per epoch is amortized across the ~40 s the tape
takes to write that epoch's data. Because the work is per-block
rather than a 16 GiB burst, it overlaps the write pipeline
naturally; single-threaded suffices, and on a multi-core server
the accumulate step can run off the write thread. Memory
footprint is the ~512 MiB of accumulators (§7.1), not 16 GiB.
Bootstrap and sidecar-header writes are negligible.

### 7.7 Commit durability barrier

"Durably written" (§7.5.2) has a precise meaning that the write
sequence and Layer 5's catalog commit both depend on.
`RawTapeSink::write_filemark` is a **synchronous durability
barrier**, not a fire-and-forget operation:

```
On a successful return from write_filemark(), all preceding
fixed blocks of that tape file AND the trailing filemark itself
are physically committed to the medium — flushed out of the
drive's write buffer — so that after a power loss the drive can
later reposition to the boundary just past that filemark.
```

The Layer 3a raw adapter MUST enforce this:

```
- Direct SCSI: WRITE FILE MARKS with IMMED = 0 (the immediate
  bit CLEAR), so the command flushes buffered data and the
  filemark(s) before returning status. (IMMED = 1 returns early
  and is NOT a durability barrier — do not use it for the
  trailing filemark of a to-be-committed tape file.)
- Linux st/mt: the flushing filemark operation (MTWEOF), NOT the
  no-flush immediate variant (MTWEOFI). MTWEOFI explicitly does
  not guarantee the data is on the medium.
```

(Both behaviors are documented: the Linux SCSI tape driver
treats a non-immediate filemark as a synchronization point that
flushes the drive buffer before returning, and distinguishes it
from `MTWEOFI`; vendor SCSI references describe `WRITE FILE MARKS`
with `Immed` clear as flushing buffered data and filemarks before
returning status.)

**The commit ordering is therefore strict and must not be
reordered:**

```
1. write_fixed_block(s)   — the tape file's blocks
2. write_filemark()       — SYNCHRONOUS barrier (IMMED=0 / MTWEOF)
3. position()             — capture the post-barrier physical position
4. Layer 5 commits the catalog_tape_files row(s)
```

A "catalog-committed tape file" means exactly: steps 1–3
returned success, *then* step 4 ran. Layer 5 MUST NOT commit a
catalog row before the synchronous filemark of step 2 has
returned. The crash windows are then all safe:

```
crash between 2 and 4:  tape has a valid tape file the catalog
                        does not know about → restart truncates
                        after the last CATALOG-committed file
                        (the extra file is abandoned). Safe.
crash after 4:          catalog and tape agree. Safe.
UNSAFE (forbidden):     committing the catalog row before the
                        synchronous filemark returns — a power
                        loss could then leave the catalog
                        claiming a tape file that never reached
                        the medium.
```

### 7.8 Restart and append-after-crash (normative)

§7.5.2 covers a *failed write within one session*; this section
covers *reopening a partially-written tape in a later session*
(daemon restart, power loss, deliberate resume). The append point
is **catalog-driven**, never inferred from `highest_protected_
ordinal` and not simply "the last object."

**Append point = the trailing filemark of the last
catalog-committed *bundle*** on the cartridge (§7.4). A bundle is an
object plus the completed-epoch sidecars it produced plus any
bootstraps/`parity_map` written during emission, committed in one
transaction. Example: object X produced sidecars 0–2; the writer
wrote object X, its filemark, then sidecars 0 and 1, and crashed
midway through sidecar 2 before the bundle's DB commit. Because the
bundle is atomic and never committed, the append point is after the
last *previously* committed bundle — object X and its partial
sidecars are abandoned and the orchestrator retries object X whole.
The append point is never inferred from `highest_protected_ordinal`
and never "the last physical tape file."

**The subtle part: rebuilding the open epoch.** After a crash the
in-memory parity accumulators (§7.1) are gone. Let

```
W = highest_protected_ordinal of the committed prefix (sidecars emitted)
T = total committed object-data ordinals of the committed prefix
```

If `W < T` — the normal case, since filemarks don't flush epochs
— the committed prefix contains object data on tape with no
emitted sidecar yet. To preserve the "a clean finish() protects
everything" guarantee, 3c rebuilds rather than abandons that
range:

```
resume_append_from_committed_prefix(tape_id):
  1. Load the committed catalog_tape_files prefix for tape_id.
  2. N = highest committed tape_file_number.
  3. Position to N; VERIFY it matches the catalog:
       object   → object_id / tape_uuid / rem-tar global header
       sidecar  → sidecar magic / tape_uuid / epoch_id / CRCs
       bootstrap→ bootstrap magic / tape_uuid / sequence / digest scope
  4. Position just past N's trailing filemark — the append point.
  5. Treat any physical tape files after N as abandoned; the next
     write overwrites from the append point (so a stale provisional
     sidecar 2, etc., is physically superseded).
  6. Rebuild the FilemarkMapBuilder from the committed prefix.
  7. Compute W and T from the committed prefix.
  8. ASSERT the v0.5 committed-state invariant (§7.4):
         T − W < data_ordinals_per_epoch
     A committed prefix can never contain a completed-but-unemitted
     epoch, because objects and their completed sidecars commit as
     one atomic ObjectCommitBundle (§7.4). If T − W >=
     data_ordinals_per_epoch in a v1 catalog, treat it as catalog
     corruption or a legacy experimental tape — do NOT silently
     perform a multi-epoch rebuild in production mode.
  9. OPEN-EPOCH REBUILD (Option A, default): re-read the committed
     object blocks covering ordinals [W, T) — at most one partial
     epoch — and re-accumulate them through the §7.1 incremental
     encoder; leave that partial epoch loaded as the live EpochState.
 10. New object data continues from ordinal T.
 11. Optionally write a fresh non-final bootstrap at the append point.
 12. Append new objects normally (§7.2).
```

Step 8's invariant is what bounds restart. Because the
`ObjectCommitBundle` is atomic (§7.4), the committed prefix always
ends on a completed bundle, so the open-epoch re-read is **at most
one partial epoch (~16 GiB at default geometry)** — never the
multi-epoch backlog that volatile deferral would have created. The
crash windows for a bundle are all safe:

```
Crash before object filemark:
    Object incomplete; no bundle commit. Append after previous committed bundle.
Crash after object filemark, before all generated sidecars written:
    Bundle incomplete; object NOT committed. Append after previous committed bundle.
    (The durable-but-uncommitted object + partial sidecars are abandoned;
     the orchestrator retries the object from the beginning — see §7.4
     large-object rule for the rewrite cost.)
Crash after all sidecars written, before bundle DB commit:
    Bundle NOT committed. Append after previous committed bundle.
Crash after bundle DB commit:
    Bundle committed. Append after the last committed tape file in the bundle;
    rebuild only the trailing partial epoch if W < T.
```

This is intentionally stricter than committing each tape file as
soon as its filemark returns: it keeps the committed prefix's
rebuild cost bounded to one epoch at the price of a rare
whole-object rewrite on a mid-bundle crash.

**Sidecars emitted during resume are ordinary committed tape files.**
If step 8 emits one or more sidecars, each sidecar follows the same
commit discipline as a sidecar emitted at `finish_object()`:

```
1. write sidecar fixed block(s)
2. write synchronous trailing filemark (§7.7)
3. capture post-filemark physical position
4. insert the catalog_tape_files row for that sidecar
5. advance catalog_tapes.highest_protected_ordinal
6. recompute affected object parity_state values
```

Resume MUST NOT treat rebuilt sidecars as an in-memory-only repair.
They are durable tape files and catalog rows before any new object
blocks are accepted. The resume API therefore returns the emitted
sidecars and the new protection watermark explicitly:

```rust
pub struct ResumeAppendResult {
    pub append_after_tape_file_number: u32,
    pub sidecars_emitted: Vec<SidecarTapeFile>,
    pub highest_protected_ordinal: u64,
    pub live_epoch_start: u64,
    pub next_data_ordinal: u64,
}
```


**Failure during rebuild.** If an object block in `[W, T)` is
unreadable during step 8, that block has *no sidecar yet* (it is
above the old watermark by definition), so it cannot be
RS-recovered on this tape (§7.2.1). The rebuild fails cleanly:
the resume is aborted and Layer 5 falls back to another of the
three copies for that archive rather than appending to a tape
whose open epoch cannot be reconstructed.

**Option B (abandon `[W, T)`, non-default).** Start a fresh epoch
at T and permanently mark `[W, T)` unprotected. Simpler, but it
breaks the clean-finish-protects-everything guarantee, so it is
offered only as an explicit operator override for a damaged tape
where the Option-A re-read itself fails, never as the default.

---

## 8. Read flow

### 8.1 Bootstrap discovery and filemark-map acquisition (tape-mount time)

When a tape is loaded the reader does three things, in order:
find *a* valid bootstrap (for the scheme, `tape_uuid`, and
`block_size`); find the *authoritative* bootstrap (the
highest-scope copy); then acquire and validate the filemark map.
Conflating the first and second steps is the bug v0.3.0 had: the
BOT copy is found first but carries an empty/partial map digest,
so validating a full scan-reconstructed map against it would
always fail (review #3).

These functions take a **`RawTapeSource`**, not a `BlockSource`
(review #5). Bootstrap discovery and scan-reconstruction need
raw physical-tape operations — seek to a physical block, space
over filemarks, read fixed blocks — that the object/block-
oriented `BlockSource` does not (and should not) expose. The two
trait surfaces are distinct (§4.5).

**Block-size resolution (the bootstrap chicken-and-egg, review
#3).** A fixed-block tape must be read at the right block size,
but the block size is *recorded inside* the bootstrap. 3c
resolves this with a configured size plus a bounded fallback
scan, carried in `TapeGeometryHint`:

```rust
pub struct TapeGeometryHint {
    /// REQUIRED in the normal path: Layer 3a configures the drive
    /// to this fixed block size before discovery, and the reader
    /// reads the first bootstrap at exactly this size. Sourced
    /// from the catalog (per-tape block_size) or operator config.
    pub configured_block_size: Option<u32>,
    /// Fallback ONLY when configured_block_size is None (e.g. a
    /// foreign/unknown tape): try these sizes in order, accepting
    /// the first whose block parses as a valid bootstrap (magic +
    /// CRC-64 header check). Default list: 256 KiB, 512 KiB, 1 MiB.
    pub candidate_block_sizes: Vec<u32>,
    /// Physical positions to probe for bootstrap copies (BOT plus
    /// the fractional positions of §7.3).
    pub probe_positions: Vec<PhysicalPositionHint>,
}
```

Discovery uses `configured_block_size` if present (Option A — the
normal path: the catalog or Layer 3a always knows it), else walks
`candidate_block_sizes` validating bootstrap magic and the
CRC-64 header at each candidate (Option B fallback for a tape of
unknown provenance). Once a bootstrap parses, its
`block_size_bytes` field is authoritative for the rest of the
session, and discovery asserts it equals the size that was used
to read it. A Remanence v1 tape always records its block size in
every bootstrap copy, so a configured-size reader never needs the
fallback; the fallback exists only so a catalog-less reader can
recover a tape whose configured size was lost. The
candidate-block-size fallback is implemented by the Layer 3a raw
adapter: it reconfigures the drive's fixed block size to each
candidate *before* the corresponding `read_record`, so a
candidate read genuinely occurs at that block size — the fallback
is not merely allocating a different-sized buffer against a
wrongly-configured drive.

```rust
fn discover_scheme(raw: &mut dyn RawTapeSource,
                   hint: Option<TapeGeometryHint>)
    -> Result<BootstrapPayload, ParityError>
{
    // Step 1: ANY valid bootstrap gives scheme + tape_uuid +
    // block_size. First valid copy wins; BOT in the common case.
    for block_size in candidate_block_sizes(&hint) {
        raw.configure_fixed_block_size(block_size)?;   // Layer 3a adapter hook
        for pos in expected_bootstrap_positions(&hint) {
            if let Ok(bp) = try_read_bootstrap_at(raw, pos, block_size) {
                if bp.block_size_bytes != block_size {
                    return Err(ParityError::BootstrapParse(
                        "bootstrap block_size does not match read size".into()));
                }
                return Ok(bp);
            }
        }
    }
    Err(ParityError::NoBootstrapFound)
}

fn discover_authoritative_bootstrap(raw: &mut dyn RawTapeSource,
                                    hint: Option<TapeGeometryHint>,
                                    block_size: u32)
    -> Result<BootstrapPayload, ParityError>
{
    // Step 2: after discover_scheme, block_size is known. Collect ALL
    // valid bootstrap copies at that size, then pick the one whose map
    // digest has the widest scope. Prefer is_final_map=true; otherwise
    // highest sequence / largest map_total_data_ordinals.
    raw.configure_fixed_block_size(block_size)?;
    let mut best: Option<BootstrapPayload> = None;
    for pos in expected_bootstrap_positions(&hint) {
        if let Ok(bp) = try_read_bootstrap_at(raw, pos, block_size) {
            best = Some(match best {
                None => bp,
                Some(prev) => choose_wider_map_scope(prev, bp),
            });
        }
    }
    best.ok_or(ParityError::NoBootstrapFound)
}

// "Wider scope": is_final_map wins; else higher `sequence`; else
// larger map_total_data_ordinals. Sequence is monotonic, so ties
// are impossible on a correctly-written tape.
fn choose_wider_map_scope(a: BootstrapPayload, b: BootstrapPayload)
    -> BootstrapPayload { /* ... */ }

fn try_read_bootstrap_at(raw: &mut dyn RawTapeSource,
                         hint: PhysicalPositionHint,
                         block_size: u32)
    -> Result<BootstrapPayload, ParityError>
{
    // `hint` is a physical position to seek near (a READ POSITION
    // / LOCATE block-address hint), NOT a BodyLba or ordinal —
    // bootstraps are their own tape files outside the ordinal
    // space (§5.6). The bootstrap may be a little past the hint
    // depending on where the previous object boundary fell.
    raw.locate_physical(hint)?;
    for _ in 0..MAX_BOOTSTRAP_SCAN_BLOCKS {
        let mut buf = vec![0u8; block_size as usize];
        match raw.read_record(&mut buf) {
            Ok(RawReadOutcome::Block { bytes, .. })
                if bytes == block_size as usize
                   && has_bootstrap_magic(&buf)
                   && bootstrap_header_crc_valid(&buf)
                   && bootstrap_payload_crc_valid(&buf)
                => return parse_bootstrap(&buf),
            Ok(RawReadOutcome::Block { bytes, .. })
                if bytes != block_size as usize
                => return Err(ParityError::BootstrapParse(
                    "short fixed-block bootstrap read".into())),
            Ok(RawReadOutcome::EndOfData { .. })
                => break,                       // scanned past data; no bootstrap here
            Ok(_) => continue,                  // other block or a filemark: keep scanning
            Err(_) => continue,                 // medium error: keep scanning
        }
    }
    Err(ParityError::NoBootstrapAtPosition(hint))
}
```

A bootstrap copy always exists at BOT (§7.3), so step 1 succeeds
on the first try in the common case. Step 2 scans the other
expected positions to find the highest-scope copy; if only the
BOT copy survives, it is used for both — and on a tape that was
closed cleanly even the BOT copy is *not* the authoritative one,
so step 2 must run whenever the catalog is unavailable. (When the
catalog is present, step 2 can be skipped — the catalog map is
authoritative and self-consistent; the scheme from step 1 is all
that's needed.)

Once the authoritative bootstrap is in hand, acquire the map.
The subtlety (review #2): an intermediate copy's digest covers a
**prefix** of the tape, and within that prefix two ordinals
matter and differ:

- `map_total_data_ordinals` — how much object data the prefix
  *describes* (names in the map). Bounds what the map can
  address at all.
- `highest_protected_ordinal` — how much of that data has an
  *emitted sidecar*. Bounds what can actually be RS-recovered.

Because filemarks don't flush epochs, an intermediate copy
written at an object boundary describes object data whose tail
is in an open, sidecar-less epoch — so
`highest_protected_ordinal < map_total_data_ordinals`. Using the
former (the map extent) as the recovery bound, as v0.3.2 did,
would let recovery attempt an unprotected tail ordinal with no
sidecar behind it. The scope therefore carries both:

```rust
fn acquire_filemark_map(raw, authoritative_bootstrap, catalog_map)
    -> Result<ScopedFilemarkMap, ParityError>
{
    if let Some((map, watermark)) = catalog_map {
        // Catalog is authoritative and complete; it also stores
        // catalog_tapes.highest_protected_ordinal directly.
        return Ok(ScopedFilemarkMap {
            map,
            validated_prefix_tape_files: None,   // Complete: all validated
            scope: MapScope::Complete { highest_protected_ordinal: watermark },
        });
    }

    let d = &authoritative_bootstrap.filemark_map_digest;
    let full = scan_reconstruct_filemark_map(raw)?;   // whatever is on tape now

    if d.is_final_map {
        if full.canonical_digest() != d.map_sha256 {
            return Err(ParityError::FilemarkMapDigestMismatch);
        }
        // RUNTIME cross-checks (review #7): the digest's structural
        // fields are part of the bootstrap trust contract. Any
        // disagreement with the reconstructed projection means a
        // corrupt or forged bootstrap/map — a hard error, not a
        // debug-only assert.
        if full.tape_file_count()          != d.tape_file_count
        || full.total_data_ordinals()      != d.map_total_data_ordinals
        || full.max_sidecar_end_exclusive() != d.highest_protected_ordinal {
            return Err(ParityError::FilemarkMapDigestMismatch);
        }
        Ok(ScopedFilemarkMap {
            map: full,
            validated_prefix_tape_files: None,   // Complete: all validated
            scope: MapScope::Complete {
                highest_protected_ordinal: d.highest_protected_ordinal,
            },
        })
    } else {
        // Intermediate copy: validate the PREFIX of d.tape_file_count
        // tape files (describing d.map_total_data_ordinals object
        // ordinals), then bound RECOVERY to d.highest_protected_ordinal.
        let prefix = full.truncate_to_tape_files(d.tape_file_count);
        if prefix.canonical_digest() != d.map_sha256 {
            return Err(ParityError::FilemarkMapDigestMismatch);
        }
        // Same RUNTIME cross-checks against the validated PREFIX.
        if prefix.tape_file_count()           != d.tape_file_count
        || prefix.total_data_ordinals()       != d.map_total_data_ordinals
        || prefix.max_sidecar_end_exclusive() != d.highest_protected_ordinal {
            return Err(ParityError::FilemarkMapDigestMismatch);
        }
        Ok(ScopedFilemarkMap {
            map: full,                          // suffix is forensic/untrusted
            validated_prefix_tape_files: Some(d.tape_file_count),  // review #6
            scope: MapScope::Prefix {
                map_total_data_ordinals: d.map_total_data_ordinals,
                highest_protected_ordinal: d.highest_protected_ordinal,
            },
        })
    }
}
```

`ScopedFilemarkMap::scope` bounds recovery on **two** conditions
(review #2): a recovery for `failed_ordinal` is permitted only if

```
failed_ordinal < scope.map_total_data_ordinals   // the map even names it
AND failed_ordinal < scope.highest_protected_ordinal  // a sidecar exists
```

The first failing returns `OutsideValidatedMapPrefix`; the second
returns `UnrecoverablePendingEpoch`; either way the caller falls
back to another copy. This composes with the live per-block
watermark rule (§7.2.1): when the catalog is present its
watermark is authoritative; catalog-less, the validated digest's
`highest_protected_ordinal` is the watermark.

The scope type carries the protection watermark in both arms, so
recovery never has to reach outside it:

```rust
pub struct ScopedFilemarkMap {
    /// The full reconstructed/catalog map. In the Prefix case it
    /// may name MORE tape files than were validated — the suffix
    /// is FORENSIC / UNTRUSTED navigation, not authoritative
    /// (review #6).
    pub map: FilemarkMap,
    /// In the Prefix case, how many leading tape files the digest
    /// actually authenticated. `None` means the whole map is
    /// validated (Complete). Callers MUST treat tape files at index
    /// >= this value as unverified.
    pub validated_prefix_tape_files: Option<u32>,
    pub scope: MapScope,         // bounds what may be RS-recovered
}

pub enum MapScope {
    /// Catalog map, or a final-map bootstrap: the whole tape is
    /// described and the watermark is exact.
    Complete { highest_protected_ordinal: u64 },
    /// Only an intermediate bootstrap survived: recovery is bounded
    /// to its attested prefix AND to its protection watermark.
    Prefix {
        map_total_data_ordinals: u64,   // ordinals the prefix names
        highest_protected_ordinal: u64, // ordinals with an emitted sidecar
    },
}

impl ScopedFilemarkMap {
    /// Is `tape_file_number` inside the authenticated prefix? A
    /// caller opening an object outside it gets only unverified,
    /// tar-only access (see open_object, §6.2) — never treated as
    /// authoritative (review #6).
    pub fn is_validated(&self, tape_file_number: u32) -> bool {
        match self.validated_prefix_tape_files {
            None => true,                                 // Complete
            Some(n) => tape_file_number < n,              // Prefix
        }
    }
}

impl MapScope {
    /// The protection watermark in either arm.
    pub fn watermark(&self) -> u64 {
        match self {
            MapScope::Complete { highest_protected_ordinal }
          | MapScope::Prefix   { highest_protected_ordinal, .. } => *highest_protected_ordinal,
        }
    }
    /// May ordinal `o` be RS-recovered under this scope?
    pub fn recoverable(&self, o: u64) -> Result<(), ParityError> {
        if let MapScope::Prefix { map_total_data_ordinals, .. } = self {
            if o >= *map_total_data_ordinals {
                return Err(ParityError::OutsideValidatedMapPrefix {
                    ordinal: o, prefix_ordinals: *map_total_data_ordinals });
            }
        }
        if o >= self.watermark() {
            return Err(ParityError::UnrecoverablePendingEpoch {
                failed_ordinal: o, watermark: self.watermark() });
        }
        Ok(())
    }
}
```

**Scan-reconstruct (catalog-less recovery).** A digest validates
a map but cannot provide one. When the catalog is gone, the
reader walks the tape file by file (`mt fsf`), classifying each
**without trusting any block inside an object** (review #4), then
applies the bootstrap/`parity_map` sidecar directory (§5.6.1) as a
trusted overlay, then validates against the digest.

*Scan phase — for each filemark-delimited tape file:*

```
1. Count fixed blocks to the next filemark (block_count).
2. Capture: first block bytes if readable; last block bytes if
   readable; any readable sidecar tail header/index blocks the
   footer points to; any readable parity_map tail payload.
3. Classify in this order:
     valid bootstrap magic + CRCs                 -> bootstrap
     valid parity_map primary/tail + CRCs         -> parity_map
     valid sidecar PRIMARY header/index + CRCs    -> parity_sidecar
     valid sidecar FOOTER + valid TAIL header     -> parity_sidecar
     otherwise                                    -> object_candidate
```

Objects are classified **by elimination**, not by reading their
pax `REMANENCE.format_id` header. This is essential: an object
whose first block is the very block that needs parity recovery
must still be locatable in the map, but reading its header to
classify it would fail before parity could help — a circular
recovery failure. Note the v0.5 additions to classification: a
sidecar is now recognised from its **footer + tail header** even
when its primary header (block 0) is damaged (§5.5), and the new
`parity_map` control file is recognised by its own magic.

*Directory overlay phase — apply the best available directory:*

```
Source priority: final bootstrap inline directory; else final
bootstrap ParityMapReference (if the referenced parity_map
validates); else highest-sequence intermediate inline/parity_map
prefix; else a trusted operator-supplied catalog backup.

For each directory entry (keyed by tape_file_number):
  if scanned[tape_file_number].block_count == sidecar_total_block_count:
      force kind = parity_sidecar
      fill epoch_id and protected ordinal range from the directory
      if no primary/tail header copy is valid:
          mark sidecar status = MetadataUnavailable
      else:
          require canonical_metadata_hash to match
  else:
      mark map structurally inconsistent for this scope
```

This is the v0.5 fix. A sidecar whose primary header is damaged is
**no longer demoted to an object forever**: the directory (carried
in the replicated bootstrap / `parity_map` root of trust) restores
its structural map entry from the trusted `(tape_file_number →
sidecar, range, block_count)` attestation. The object candidate's
tar/pax header is still verified later, after the map is
reconstructed.

*Digest validation phase.* Build the canonical map projection
(§5.6) **after** the directory overlay, then compare to the
authoritative digest (final → `map_sha256`; intermediate →
prefix). Crucially, if a tape file is identified by the directory
but both sidecar header copies are unreadable, it is still included
in the projection as `kind=parity_sidecar` with the directory's
structural fields — **copy health is excluded from the canonical
map digest** (it is recovery availability, not map structure), so a
degrading tape still validates deterministically.

**Map validity and sidecar usability are separate (normative).**

```
Map-valid sidecar:
    correctly known to be a sidecar with epoch / range / block_count.
Recovery-usable sidecar:
    at least one header/index copy is valid AND the needed parity
    shard blocks pass CRC.
```

A map-valid but recovery-unusable sidecar makes **only that epoch**
parity-unavailable; it MUST NOT invalidate recovery for other
epochs. This supersedes the v0.4.4 behavior where one damaged
sidecar header returned `FilemarkMapDigestMismatch` for the whole
scanned map. The hard guarantee: *one damaged sidecar header
disables only its own epoch's parity, never the whole tape's.* If
all of a sidecar's metadata sources fail (primary, tail, footer,
**and** no directory entry covers it), only then does map
validation fail for the affected scope — and the fallbacks are the
catalog map (which never depends on scanning) or another of the
three copies. (Plain `tar` extraction by filemark works throughout;
only *parity recovery* needs the map.)

### 8.2 Clean read (happy path)

```
ObjectParitySource::read_block(buf), reading within an object:
  1. The reader holds an ObjectParitySource opened via
     ParitySource::open_object(tape_file_number, trust) and
     positioned by object-local BodyLba (locate). Object tape
     files contain ONLY object data — no parity or bootstrap
     blocks (§5.1).
  2. inner.read_record(buf) — RawReadOutcome::Block on success;
     a Filemark/EndOfData inside the object's extent is a
     FormatError (truncated object).
  3. Return Ok(bytes). There is nothing to filter: a clean read
     of an object's tape file is directly a valid pax tar stream.
```

Zero overhead from the parity layer on the happy path. Parity
shards and bootstraps live in *separate tape files* the reader
simply doesn't enter during a normal object read — there are no
non-data blocks to skip over inside an object. (This is cleaner
than v0.2, where the body format had to know which interleaved
LBAs to avoid.)

### 8.3 Error-triggered recovery

```
ObjectParitySource::read_block(buf):
  1. Attempt inner.read_record(buf) as in §8.2.
  2. On any of:
     - TapeIoError::CheckCondition with sense key MEDIUM_ERROR (0x03)
     - TapeIoError::CheckCondition with sense key HARDWARE_ERROR (0x04) and
       drive-specific positioning-failure ASC/ASCQ
     - TapeIoError::Transport (rare; usually means cabling/HBA
       issue, but if it happens mid-read of a known-good LBA range,
       recovery is worth trying)
     → fall through to recovery.

  3. Recovery:
     a. Resolve the failed (tape_file_number, body_lba) to a
        ParityDataOrdinal `failed_ordinal` via the filemark map.
     a0. Per-block protection check (§7.2.1): if
         `failed_ordinal >= tape.highest_protected_ordinal`, the
         epoch containing THIS block has no sidecar yet — there is
         nothing to reconstruct from. Emit RecoveryEvent::
         Unrecoverable (UnrecoverablePendingEpoch) and return
         FormatError so the caller falls back to another copy.
         NOTE: this is a per-ORDINAL test, not a whole-object
         test. A large object completes many epochs before it
         closes; the sidecars for those completed epochs are
         emitted at finish_object, so an early block of an object
         that is globally still `partial`/`pending` may sit below
         the watermark and BE recoverable. Testing the whole
         object's range here (the v0.3.1 bug) would wrongly refuse
         to recover protected early blocks of a large object.
     a1. Convert `failed_ordinal` to a StripeAddress via
        ordinal_to_stripe (§5.3).
     b. Build the list of stripe peers: the other (k−1) data
        shards (each a ParityDataOrdinal → (tape_file, body_lba)
        → physical position; they may live in OTHER objects
        across filemarks) and the m parity shards (in this
        epoch's sidecar tape file, located by (stripe_index,
        parity_index) in the sidecar's shard index table).
     c. Read each peer:
        - locate to its physical position (filemark map / sidecar
          index), read the block.
        - On a clean read, verify it against its recorded CRC
          (data-shard CRC in the sidecar data index for data
          peers; parity-shard CRC for parity peers, §5.5). If the
          CRC mismatches, the block is silently corrupt: treat it
          as an additional erasure (do NOT feed it to reconstruct
          — a poisoned peer yields wrong bytes that pass the size
          check). On a read error, likewise record an erasure.
        - On a verified read, store it in the recovery buffer.
        - Implicit-zero shards (final partial epoch, §5.4) are
          supplied as zero blocks without reading.
     d. Stop early once k surviving members are collected.
     e. If fewer than k surviving members: emit
        RecoveryEvent::Unrecoverable and return FormatError.
     f. Run reed-solomon-erasure's reconstruct() with the
        surviving members and the indices of the missing ones.
     g. Verify the reconstructed block: it must be exactly
        block_size bytes AND its CRC-64 must equal the sidecar's
        recorded data-shard CRC for `failed_ordinal` (§5.5). Size
        alone is not an integrity check. On CRC mismatch the
        reconstruction was poisoned by an undetected-bad peer
        (or a logic error): emit RecoveryEvent::Unrecoverable with
        `ReconstructionIntegrityFailure` and fall back to another
        copy rather than returning wrong bytes.
     h. Copy the reconstructed block into the caller's buf.
     i. Emit RecoveryEvent::Recovered via audit_hook.
     j. Locate inner back to where the caller expects to be
        (one past the failed body_lba within the object).
     k. Return Ok(reconstructed_size).
```

Recovery cost: in the worst case, k LOCATEs and k inner reads —
some crossing filemarks into other object tape files and into
the epoch's sidecar. At LTO-9 LOCATE speeds (a few seconds for a
long seek, sub-second for short ones within an object), recovery
of a single block takes 5–30 seconds. This is acceptable for an
error path — slow compared to a clean read but orders of
magnitude faster than fetching from another tape copy.

### 8.4 Recovery of bootstrap and sidecar tape files

Bootstrap and parity-sidecar tape files are *not* in the
parity-protected ordinal stream (§5.2, §5.6), so they are not
recovered by RS reconstruction. Their protection is different:

- **Bootstrap**: replicated at known fractional positions (§7.3).
  Discovery (§8.1) tries each; if the BOT copy is damaged, the
  reader proceeds to the next. As long as one copy validates,
  the scheme and map digest are recovered. (A bootstrap cannot
  be parity-protected — it defines the scheme parity needs;
  §5.6.)
- **Parity sidecar**: its **metadata** (header/index) is replicated
  within the sidecar — primary + tail copies + a footer locator
  (§5.5) — and is additionally attested by the bootstrap/`parity_map`
  directory (§5.6.1), so a damaged sidecar header degrades only its
  own epoch, never the whole tape's parity (§8.1). The ~512 MiB
  **shard payload** is not replicated: if it is destroyed, that epoch
  loses parity *on this tape*; the object data it protected is still
  present and tar-extractable, and recovery of damaged data in such an
  epoch falls back to another of the archive's three copies (§7, §16).
- **Object data**: recovered via its epoch's parity (§8.3),
  i.e. the normal RS path.

The defense-in-depth is therefore layered by structure type:
RS parity for object data; intra-tape replication for the
bootstrap (the must-find-first root of trust); and the
multi-copy archive policy for sidecars and for any damage that
exceeds a single tape's parity. This replaces v0.2's "parity for
everything plus replication for structural blocks," which is no
longer accurate now that bootstraps and sidecars sit outside the
ordinal stream.

### 8.5 Servo-damage handling

The LTO-4 dust failure mode (Appendix B) presents differently
from a typical MEDIUM_ERROR: the drive can't position the heads
to the affected LBA range at all. SCSI LOCATE returns a
CHECK_CONDITION with sense key HARDWARE_ERROR or NOT_READY plus
a positioning-related ASC/ASCQ.

The parity source treats this as the same kind of erasure as
MEDIUM_ERROR. The recovery flow doesn't care *why* a block is
unreadable — it only needs to know which data shards are
unreadable and gather enough surviving stripe peers (data shards
from possibly-other objects + parity shards from the epoch's
sidecar) to reconstruct.

Important subtlety: damage that spans from an object into the
adjacent parity-sidecar tape file can take out both data shards
and the parity that would recover them. The interleave (§5.2.1)
disperses contiguous *data* damage one-per-stripe, so:

- Contiguous data damage of N blocks where N < S: each stripe
  loses at most 1 data shard; all stripes recover (m ≥ 1).
- N where S ≤ N < 2S: at most 2 per stripe; recover if m ≥ 2.
- ... up to N < m·S: recoverable.
- N ≥ m·S: some stripes lose more than m shards; those stripes
  fail (fall back to another tape copy).

At defaults (S=512, m=4): contiguous data damage up to 4·512 −
1 = 2047 blocks (~512 MiB) is recoverable. If the damage also
destroys the epoch's sidecar (a separate tape file, typically
not physically adjacent to all of the epoch's objects), that
epoch loses parity entirely on this tape and recovery falls back
to another copy (§8.4).

---

## 9. Recovery in detail

### 9.1 Erasure detection

The parity source treats the following as erasures, all handled
by the same recovery path:

| SCSI condition                                | Erasure? | Reason |
|-----------------------------------------------|----------|--------|
| Clean read                                    | No       | Happy path. |
| CHECK_CONDITION sense key 0x03 (MEDIUM_ERROR) | Yes      | LTO ECC gave up. |
| CHECK_CONDITION sense key 0x04 (HARDWARE_ERROR) with positioning ASC | Yes | Servo damage. |
| CHECK_CONDITION sense key 0x02 (NOT_READY) post-LOCATE                | Yes | Probable servo damage at target LBA. |
| Transport error (timeout, etc.)               | Maybe    | Try once more; if it persists, treat as erasure. |
| CHECK_CONDITION sense key 0x05 (ILLEGAL_REQUEST) | No   | Programming error in rem, not media. |
| CHECK_CONDITION sense key 0x07 (DATA_PROTECT) | No       | Encryption or write-protect, not media damage. |

Sense codes are extracted from `TapeIoError::CheckCondition` via
the existing `ScsiCheckCondition` accessor.

### 9.2 Stripe reconstruction

Reconstruction is defined by **Appendix A §A.5** (invert the
`k×k` submatrix of the systematic generator over GF(2⁸),
multiply to recover the data shards). An implementation MAY
delegate to the `reed-solomon-erasure` crate's reconstruction
primitive **only after** the impl-step-11.6 conformance test
proves the crate matches Appendix A (field `0x11D`, the §A.3
Cauchy matrix, the §A.6 shard ordering) byte-for-byte; the crate
is an optional accelerator, not the definition. With that caveat,
the crate call looks like:

```rust
use reed_solomon_erasure::galois_8::ReedSolomon;

let rs = ReedSolomon::new(k, m)?;  // k=128, m=4 by default

// shards: Vec<Option<Vec<u8>>> with k+m entries
// None for missing shards, Some(bytes) for surviving ones.
// Shards 0..k are the stripe's DATA shards (each a
// ParityDataOrdinal resolved to a physical block via the
// filemark map — possibly in a different object across a
// filemark); shards k..k+m are the stripe's PARITY shards
// (read from the epoch's sidecar tape file, located by
// (stripe_index, parity_index) in the sidecar shard index).
// Final-epoch implicit-zero data shards (§5.4) are supplied as
// Some(vec![0; chunk_size]) without a read.
let mut shards: Vec<Option<Vec<u8>>> = build_shards_from_stripe_peers(...);

rs.reconstruct(&mut shards)?;
// shards[i] is now Some(_) for all i; missing data shards have
// been reconstructed.
```

The crate uses Cauchy matrices and SIMD-accelerated finite-field
arithmetic. On modern x86 it sustains ~3 GB/s per core for
encode and decode. Decoding cost for one stripe (k=128 surviving
256 KiB shards → one reconstructed shard): ~10 ms. Total
recovery cost per block is dominated by tape I/O — the up-to-k
LOCATEs and reads, some of which cross filemarks into other
object tape files and into the sidecar — not by CPU.

### 9.2.1 Bulk / epoch recovery

`recover_block_at()` is correct for an isolated block but
inefficient for the contiguous damage this design exists to handle:
a contiguous region spreads one-per-stripe across hundreds or
thousands of stripes (§5.2.1), and reconstructing block-by-block
with a small cache re-LOCATEs and re-reads heavily-overlapping peer
sets. v0.5 adds a region/epoch recovery path that reads each
surviving peer **once** in physical order.

```rust
pub struct RecoveryRegionRequest {
    pub tape_file_number: u32,
    pub body_lba_start: u64,
    pub block_count: u64,
    pub reason: RecoveryReason,    // MediumError | LocateFailure | CrcMismatch | OperatorRequested
}
pub struct RecoveredBlock {
    pub tape_file_number: u32,
    pub body_lba: u64,
    pub data: Vec<u8>,
    pub source: RecoverySource,    // CleanRead | Reconstructed | Unrecoverable
}
impl ParitySource<'_> {
    pub fn recover_region(&mut self, req: RecoveryRegionRequest)
        -> Result<impl Iterator<Item = RecoveredBlock>, ParityError>;
    pub fn recover_ordinal_range(&mut self, ordinals: Range<ParityDataOrdinal>)
        -> Result<impl Iterator<Item = RecoveredOrdinalBlock>, ParityError>;
}
```

**Planning algorithm (plan by epoch):**

```
1. Translate the requested BodyLba range to a ParityDataOrdinal range.
2. Group missing/suspect ordinals by epoch_id and stripe_index.
3. For each affected epoch, load the sidecar header/index ONCE.
4. Build the set of required peer reads for ALL affected stripes.
5. Deduplicate peer reads.
6. Sort peer reads by physical tape position (minimize LOCATE churn).
7. Read clean peers in physical order.
8. Verify every peer against sidecar data/parity CRCs (§5.5).
9. Treat failed CRCs as erasures.
10. Reconstruct all affected stripes in memory.
11. Verify every reconstructed data shard against its sidecar data CRC.
12. Return recovered blocks in the caller-requested logical order.
```

For a contiguous region the affected stripes share most of their
surviving data shards, so dedup + physical-order reading collapses
the O(N·k) random-LOCATE storm of per-block recovery into roughly
one sequential pass over the epoch's survivors.

**Automatic escalation.** A reader SHOULD switch from
`recover_block_at` to `recover_region` when any of: ≥ 4
failed/suspect blocks within one epoch; ≥ 2 failed/suspect adjacent
BodyLba blocks; an operator byte range overlaps a known damaged
interval; or ≥ 4 `RecoveryEvent`s have already fired on the same
tape file.

### 9.3 Recovery cache

**Per-block path.** The parity source keeps a small LRU of
recently-read stripes for isolated-error recovery:

```rust
struct StripeCache {
    entries: LruCache<StripeId, CachedStripe>,
    max_stripes: usize,  // default 4
}

struct CachedStripe {
    members: Vec<Option<Vec<u8>>>,  // k+m slots, populated on demand
    reconstructed: HashSet<StripePosition>,
}
```

When recovery reads stripe members, they're cached. Subsequent
recovery requests for the same stripe (e.g., a contiguous run
of bad blocks all in the same stripe) reuse cached members.

Cache invalidation:
- On `open_object()` / locate to a new epoch's region: keep the
  cache (still valid for the previous epoch if the reader comes
  back).
- Explicit `parity_source.invalidate_cache()`: provided for
  test scenarios; not called in production.

**Bulk path — memory-bounded epoch cache.** The 4-stripe LRU is
sized for isolated errors and thrashes on a contiguous region whose
stripe peer sets are large and overlapping. Bulk recovery
(§9.2.1) uses an epoch-scoped cache instead, explicitly bounded so
it is safe on small hosts and generous on akash-class hosts:

```rust
pub struct BulkRecoveryPolicy {
    /// Hard cap for in-memory recovery shards.
    /// Default: min(8 GiB, 25% of detected RAM), configurable.
    pub max_recovery_cache_bytes: u64,
    /// If true, the planner may make multiple physical passes over an
    /// epoch rather than exceeding max_recovery_cache_bytes.
    pub allow_windowed_recovery: bool,
    /// Upper bound on affected stripes per recovery window.
    pub max_stripes_per_window: u32,
}

pub struct EpochRecoveryCache {     // recovery-mode only; never used on clean reads
    pub epoch_id: u64,
    pub loaded_sidecar: SidecarHeaderIndexSet,
    pub data_shards:   HashMap<(u32 /*stripe*/, u16 /*data_index*/),   Vec<u8>>,
    pub parity_shards: HashMap<(u32 /*stripe*/, u16 /*parity_index*/), Vec<u8>>,
    pub max_bytes: u64,
}
```

Planner rule: if all affected stripe peer sets fit under
`max_recovery_cache_bytes`, recover the epoch in one physical-order
pass; else if `allow_windowed_recovery`, split affected stripes into
memory-fitting windows (accepting extra passes / seek cost); else
fail with `RecoveryPlanExceedsMemoryBudget`. A full epoch's useful
peer set can be many GiB, so this cache has a very different memory
profile from the ~512 MiB write-side accumulator and must stay
operator-bounded.

### 9.4 Recovery events

Every recovery — successful or not — emits a `RecoveryEvent` via
the audit hook. The audit hook is the same `Arc<dyn ParityAuditHook>`
the parity source was constructed with; it's wired by the daemon
to the same audit log as Layer 2's `LibraryAuditHook`.

Operators are expected to monitor recovery events. A tape that
produces recovery events is **flagged for replacement** by Layer 5
policy — even though the data is still readable, the trend is
toward more damage. Three copies plus parity is a strong
position; one copy plus parity that's actively being exercised
is a tape that's failing in slow motion.

---

## 10. Catalog integration

### 10.1 The catalog gains the filemark map

v0.2 claimed "no new catalog fields" — Layer 3c was invisible to
the catalog schema. **v0.3 supersedes that.** The filemark-aware
model requires one structural addition: the catalog must persist
the **filemark map** (`catalog_tape_files`), because the map is
*not* derivable from object rows alone — parity-sidecar and
bootstrap tape files have no object/file rows, yet they occupy
tape-file numbers and (for sidecars) define ordinal→parity
ranges. See the 3b catalog follow-up
(`3b-catalog-schema-followup.md`) for the full DDL; the relevant
table:

```
catalog_tape_files(
  tape_id, tape_file_number, kind ∈ {object, parity_sidecar, bootstrap},
  object_id?, epoch_id?, block_count,
  first_parity_data_ordinal?,                       -- objects
  protected_ordinal_start?, protected_ordinal_end_exclusive?,  -- sidecars (half-open)
  physical_start_hint?, sha256?,
  PRIMARY KEY (tape_id, tape_file_number)
  + kind-specific CHECK constraints
)
```

What the catalog still records as before: object/file entries
(now keyed including `tape_file_number`), the manifest size/
chunk-count for direct manifest reads, refresh pointers,
encryption parameters, schema-version extensibility. What it
does **not** record: the parity *scheme* parameters (k, m, S) —
those still live only in the bootstrap (§5.6); the catalog holds
the map (where tape files are and which ordinals they cover),
not the codec.

The map is authoritative in the catalog; a digest of it lives in
each bootstrap so a catalog-less reader can scan-reconstruct and
validate (§8.1).

#### Parity-protection state (review #1)

Because parity protection lags object close (§7.2.1), the
catalog must additionally track *which* objects are actually
protected, not assume "on tape" means "protected." This is a
small set of additions, specified fully in the 3b follow-up:

```
-- per tape: the monotonic watermark advanced by each emitted sidecar
catalog_tapes.highest_protected_ordinal   BIGINT NOT NULL DEFAULT 0

-- per object: enough to compare its ordinal range to the watermark
catalog_objects.first_parity_data_ordinal BIGINT NOT NULL
catalog_objects.data_block_count          BIGINT NOT NULL   -- == object's catalog_tape_files.block_count (§5.6 identity)
catalog_objects.ordinal_end_exclusive     BIGINT GENERATED   -- = first_parity_data_ordinal + data_block_count
catalog_objects.parity_state              TEXT NOT NULL DEFAULT 'pending'
                                          -- ∈ {pending, partial, protected}
```

Given the watermark `W`, an object is `protected` iff
`ordinal_end_exclusive <= W`, `pending` iff
`first_parity_data_ordinal >= W`, and `partial` otherwise (a
large object whose early epochs are protected while its tail is
still open — the common transient state). When a sidecar
emission advances `W`, Layer 5 recomputes affected objects'
states in the same transaction (a range update on the ordinal
columns; see the 3b follow-up for the exact SQL).

**Recovery does not consult `parity_state`.** Whether a specific
damaged block is recoverable is the per-block test of §7.2.1 —
`failed_ordinal < highest_protected_ordinal` — so an object in
`partial` recovers its sub-watermark blocks and falls back to
another copy only for its tail. `parity_state` is operator-facing
summary state. After a clean `finish()` the watermark reaches
end-of-data and all objects are `protected`; a crashed session
leaves a `pending`/`partial` tail the catalog represents
honestly.

Objects, parity sidecars, and bootstraps are all
filemark-delimited tape files (§5.1), addressed physically by
`(tape_file_number, block_within_file)`. The catalog's
`physical_start_hint` gives a SCSI block address for a fast
LOCATE to a tape file; the filemark map gives the logical
structure (kinds, ordinals). An object's data blocks are
contiguous within its tape file (a clean pax tar stream), so
`(tape_file_number, body_lba)` fully addresses any object block.

There is no separate "catalog on tape" parity concern in v0.3:
the catalog is a Postgres database (Layer 4), with the tape
itself made self-describing (every tape file identifiable by
magic) as the catalog-less fallback. v0.2's on-tape
catalog-replication-with-hint-LBAs model is dropped.

### 10.3 Object header redundancy

rem-tar-v1 keeps per-object identity in pax extended headers
(forward-scan recoverable) and a trailing CBOR manifest; the
trust chain (catalog → file_sha256 → manifest_sha256 → per-chunk
CRC) is the body format's concern (rem-tar-v1 §8). 3c adds
nothing here: object data blocks — including the blocks holding
pax headers and the manifest — are ordinary object-data shards
in the epoch, recovered by RS like any other object block (§8.3).

### 10.4 The unified picture

On a v0.3 parity-protected tape:

- **Object data blocks** belong to exactly one parity epoch
  (over `ParityDataOrdinal`) and are recoverable if the damage
  affecting their stripes stays within scheme limits (§8.5),
  using parity shards from the epoch's sidecar tape file.
- **Parity-sidecar tape files** sit outside the ordinal stream;
  not RS-protected. Their *metadata* is replicated (primary + tail
  + footer, §5.5) and attested by the bootstrap/`parity_map`
  directory (§5.6.1); their *shard payload* is not replicated, so a
  damaged payload falls back to another archive copy (§8.4).
- **`parity_map` tape files** (when the sidecar directory overflows
  the bootstrap, §5.6.1) also sit outside the ordinal stream and are
  internally replicated (primary + tail + footer).
- **Bootstrap tape files** sit outside the ordinal stream;
  protected by replication at known positions (§5.6, §7.3).
- **The filemark map** ties the three address spaces together
  and lives in the catalog, with a bootstrap digest for
  catalog-less scan-reconstruct.

Unlike v0.2, blocks are *not* all inside a uniform parity area:
object data is RS-protected, while the structural tape files
(bootstrap, sidecar, parity_map) are protected by replication and
the multi-copy archive policy respectively. The trade for that
asymmetry is what buys back per-object filemarks and clean,
independently-readable pax tar archives.

### 10.5 Format documentation on tape (long-horizon insurance)

The catalog is the component most likely to be lost, migrated into
uselessness, or orphaned over a 30-year horizon — yet §8.1's
catalog-less path leans on it less now (the replicated sidecar
directory makes scan-reconstruct robust), and plain `tar` always
works. To make the tape genuinely self-describing for a future
engineer, every production tape set SHOULD include a plain-text
documentation object as a normal rem-tar object (extractable with
standard `tar`, no special tooling):

```
_remanence/specs/
  layer3a-design.md
  layer3b-design.md
  layer3c-design.md
  rem-tar-v1-design.md
  3b-catalog-schema-followup.md
  remanence-testing-plan.md
  VERSION.txt
```

This is human-operational insurance, not a substitute for
bootstraps or catalog rows. Place it in the first archive batch's
first tape; an offline/printed copy of the spec set is cheap
additional insurance.

---

## 11. Configuration

### 11.1 Default parameters (target archive)

Defaults are **block-size-aware**: the contiguous-loss tolerance
is `S × m × block_size`, so to hold ~512 MiB tolerance constant,
`S` scales inversely with block size. At rem-tar-v1's 256 KiB
default block:

| Parameter                | Default | Rationale |
|--------------------------|---------|-----------|
| Block size               | 256 KiB | rem-tar-v1's chunk size; well below LTO-9's 16 MiB cap. |
| Data blocks per stripe (k) | 128   | Balances codeword math cost vs. dispersion. |
| Parity blocks per stripe (m) | 4   | ~3.125% overhead; survives 4 erasures per stripe. |
| Stripes per epoch (S)    | 512     | 512-way interleave defeats contiguous data loss up to `S×m×256KiB` = ~512 MiB per epoch. (v0.2 used S=128 assuming 1 MiB blocks.) |
| Epoch footprint          | `S×(k+m)` = 67,584 blocks (~16.5 GiB) | An epoch covers ~16 GiB of object data + ~512 MiB of parity. In the v1 incremental model (§7.1) object data is NOT spooled — only the ~512 MiB of parity accumulators are held, then spooled as a sidecar. |
| Bootstrap copies         | BOT, ~1/3, ~2/3, near-EOD | A handful of dispersed copies; final copy carries the whole-tape map digest. |
| Capacity overhead (parity)        | 3.125%  | m/k = 4/128. |
| Capacity overhead (bootstrap+sidecar headers) | <0.01% | Negligible. |
| Per-epoch contiguous-loss tolerance | ~512 MiB | `S × m × block_size`. |
| Epochs per LTO-9 tape    | ~1100   | 18 TB / 16.5 GiB. |

If the block size changes, `S` should change with it to preserve
tolerance: at 1 MiB blocks, `S=128` gives the same ~512 MiB; at
512 KiB, `S=256`. `ParityScheme::validate()` (§11.3) warns if
`S × m × block_size` falls below the configured tolerance floor.

These defaults are chosen for the target archive's failure profile
(servo damage of perhaps 100 MB to a few GB) and storage
environment (controlled, indoor, climate-managed). See Appendix
A for the source data.

### 11.2 Conservative scheme

For tapes expected to see harsher conditions (long-term offsite
storage, suspect climate control, older library hardware), at
256 KiB blocks:

| Parameter                | Conservative | Rationale |
|--------------------------|-------------|-----------|
| Data blocks per stripe (k) | 64        | Smaller stripes; lower codeword cost. |
| Parity blocks per stripe (m) | 6        | Higher recovery threshold per stripe. |
| Stripes per epoch (S)    | 256         | 256-way interleave at 256 KiB. |
| Epoch footprint          | `256×70` = 17,920 blocks (~4.4 GiB) | More, smaller independent failure domains. |
| Capacity overhead        | 9.4%        | m/k = 6/64 (parity-over-data; see below). |
| Per-epoch contiguous-loss tolerance | ~384 MiB | `S × m × block_size`. |

At ~4.4 GiB epochs the conservative scheme puts ~4000 epochs on an
18 TB tape, whose sidecar directory (~360 KiB) exceeds one
bootstrap block — so conservative tapes carry the directory in a
`parity_map` tape file (§5.6.1) rather than inline. This is handled
by the computed sizing rule, not a hard-coded epoch count.

**Overhead terminology (two ratios, do not mix).** Report both:

```rust
impl ParityScheme {
    /// Parity bytes per usable data byte. k=128,m=4 → 3.125%.
    /// Use for "how much extra tape for a given amount of user data?"
    pub fn parity_over_data_ratio(&self) -> f64 { m as f64 / k as f64 }
    /// Parity bytes as a fraction of all data+parity written.
    /// k=128,m=4 → 3.0303%. Use for "composition of bytes already written."
    pub fn parity_fraction_of_total_written(&self) -> f64 { m as f64 / (k + m) as f64 }
}
```

### 11.3 Validation

`ParityScheme::validate()` enforces:

- `data_blocks_per_stripe >= 2` (a stripe of 1 data + m parity
  has no advantage over just writing m+1 copies of the data).
- `parity_blocks_per_stripe >= 1` (m=0 is "no parity at all";
  use the `--parity none` flag instead).
- `parity_blocks_per_stripe <= data_blocks_per_stripe` (m > k
  is wasteful — m copies of the data would be smaller).
- `stripes_per_epoch >= 1`.
- `data_blocks_per_stripe + parity_blocks_per_stripe <= 255` for
  GF(2⁸) RS. The crate enforces this internally; we double-check
  at validation time for a friendlier error message.
- Total epoch blocks (`epoch_blocks()`) fits in u32 (far more
  than any realistic configuration).
- **Warn (not reject)** if `S × m × block_size` is below the
  configured contiguous-loss tolerance floor (default ~512 MiB),
  so a block-size change without an `S` adjustment is caught
  (§11.1).

Schemes that fail validation are rejected at write-session open
with `ParityError::InvalidScheme`. The bootstrap reader also
validates on tape mount; a tape recording an invalid scheme is
treated as suspect and the operator is alerted.

### 11.4 Per-tape configuration

The scheme is **per-tape**, recorded in the bootstrap at write
time. Once a tape is written with scheme S, it's read with
scheme S forever. Changing the system-wide default scheme
affects only new tapes; old tapes remain readable with their
original parameters.

Tapes can be written with a non-default scheme by passing an
explicit `ParityScheme` to the write-session creation. The
caller (Layer 5, typically driven by orchestrator policy) is
responsible for choosing the scheme. The CLI exposes `--parity
default`, `--parity conservative`, `--parity none`, and
`--parity custom:k,m,S` for operator control.

### 11.5 No-parity opt-out

A write session can explicitly request `parity = none`. The
bootstrap records this via the no-parity flag bit (§5.6).
Readers handle this transparently — when the bootstrap's
no-parity flag is set, the `ParitySource` is bypassed and the
body format reads directly from the inner source.

Use cases for no-parity:
- Scratch tapes for development and testing.
- Migration tapes that exist as a temporary intermediate.
- Tapes where the orchestrator has determined the data is not
  irreplaceable.

For the production archive workflow, all tapes use the
default scheme. The opt-out is for development.

### 11.6 Drive compression (parity-protected writes)

Independent of the parity scheme, every parity-protected write
session sets and verifies `drive_compression = false` via
`TapeWriteConfig` (§6.1.1). The no-parity opt-out (§11.5) does not
require this — a `--parity none` scratch tape may use whatever drive
settings the operator likes — but any tape carrying a 3c scheme
records `drive_compression = false` and `fixed_block_size` in its
bootstrap and catalog, and is refused for 3c recovery if it somehow
records compression enabled. Layer 3a owns the SCSI MODE SELECT /
MODE SENSE sequence and the read-back verification; the exact mode
page is drive-model-specific and is a hardware proof gate (§14).

---

## 12. Error model

```rust
#[derive(Debug, thiserror::Error)]
pub enum ParityError {
    #[error("format-layer error: {0}")]
    Format(#[from] FormatError),

    #[error("invalid parity scheme: {0}")]
    InvalidScheme(String),

    #[error("Reed-Solomon error: {0}")]
    ReedSolomon(reed_solomon_erasure::Error),

    #[error("stripe {stripe:?} unrecoverable: lost {lost_count} blocks (limit is {limit})")]
    Unrecoverable {
        stripe: StripeAddress,
        lost_count: u16,
        limit: u16,
    },

    #[error("block at ordinal {failed_ordinal} is in an open epoch (>= watermark {watermark}); no sidecar exists yet — fall back to another copy")]
    UnrecoverablePendingEpoch {
        failed_ordinal: u64,
        watermark: u64,
    },

    #[error("reconstructed block at ordinal {failed_ordinal} failed its sidecar data-shard CRC; reconstruction was poisoned — fall back to another copy")]
    ReconstructionIntegrityFailure {
        failed_ordinal: u64,
    },

    #[error("requested ordinal {ordinal} lies outside the validated map prefix (only the first {prefix_ordinals} ordinals are covered by the surviving intermediate bootstrap)")]
    OutsideValidatedMapPrefix {
        ordinal: u64,
        prefix_ordinals: u64,
    },

    #[error("invariant violation: {0}")]
    Invariant(&'static str),

    #[error("bootstrap not found anywhere on tape")]
    NoBootstrapFound,

    #[error("no bootstrap found near expected position {0:?}")]
    NoBootstrapAtPosition(PhysicalPositionHint),

    #[error("bootstrap parse error: {0}")]
    BootstrapParse(String),

    #[error("parity scheme mismatch: bootstrap says {tape}, reader expects {expected}")]
    SchemeMismatch { tape: String, expected: String },

    #[error("magic mismatch on expected parity sidecar: got {got:02x?}")]
    BadParityMagic { got: [u8; 8] },

    #[error("reconstructed filemark map does not match bootstrap digest")]
    FilemarkMapDigestMismatch,

    #[error("filemark map could not be reconstructed: {0}")]
    FilemarkMapReconstruct(String),

    #[error("starting object would exceed required reserve: {cause:?}")]
    CapacityReserveExceeded {
        cause: CapacityReserveCause,
        projected_object_blocks: u64,
        remaining_blocks: Option<u64>,       // present for TapeCapacity
        reserve_blocks: Option<u64>,         // present for TapeCapacity
        remaining_spool_bytes: Option<u64>,  // present for ParitySpoolCapacity
        required_spool_bytes: Option<u64>,   // present for ParitySpoolCapacity
    },

    // ---- v0.5 (addendum integration) ----

    #[error("object too large for an empty tape: {projected_object_blocks} blocks needed, empty tape holds {empty_tape_usable_blocks} usable, reserve {required_reserve_blocks}; split upstream")]
    ObjectTooLargeForEmptyTape {
        projected_object_blocks: u64,
        empty_tape_usable_blocks: u64,
        required_reserve_blocks: u64,
    },

    #[error("LTO hardware compression is enabled; parity-protected writes require it disabled (§6.1.1)")]
    DriveCompressionEnabled,

    #[error("could not read back the drive's effective compression mode; refusing to write parity-protected data (§6.1.1)")]
    DriveCompressionModeUnknown,

    #[error("bulk recovery plan needs {needed_bytes} bytes but the cap is {max_recovery_cache_bytes}; enable windowed recovery or raise the cap (§9.3)")]
    RecoveryPlanExceedsMemoryBudget {
        needed_bytes: u64,
        max_recovery_cache_bytes: u64,
    },

    #[error("sidecar metadata unavailable for epoch {epoch_id} (both header copies and footer damaged); only this epoch is parity-unavailable")]
    SidecarMetadataUnavailable { epoch_id: u64 },

    #[error("parity_map tape file {tape_file_number} is unreadable; falling back to sidecar primary/tail/footer metadata (§8.1)")]
    ParityMapUnreadable { tape_file_number: u32 },
}

/// `begin_object` preflight outcome (§7.5). Distinct from the
/// in-session CapacityReserveExceeded so Layer 5 can reject an
/// impossible object before opening a write at all.
#[derive(Debug, thiserror::Error)]
pub enum BeginObjectError {
    #[error("object cannot fit on an empty tape; must be split upstream")]
    ObjectTooLargeForEmptyTape {
        projected_object_blocks: u64,
        empty_tape_usable_blocks: u64,
        required_reserve_blocks: u64,
    },
    #[error("capacity reserve exceeded on the current tape")]
    CapacityReserveExceeded { cause: CapacityReserveCause },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityReserveCause {
    /// Not enough remaining tape capacity for the projected object,
    /// its trailing filemark, pending sidecars, final sidecar, and
    /// required bootstrap/control overhead. Remedy: close this tape
    /// cleanly and write the whole object from the beginning on
    /// another tape.
    TapeCapacity,
    /// Not enough local disk/spool capacity to hold parity sidecars
    /// completed by this object before they can be emitted. Remedy:
    /// free/increase parity spool space; do not switch tapes.
    ParitySpoolCapacity,
}
```

Errors map upward:

- `ParityError::Unrecoverable` → `FormatError::Unrecoverable` →
  body format's specific error type → Layer 5's gRPC error code
  for unrecoverable reads. Layer 5 then falls back to another
  archive copy.
- `ParityError::Format(TapeIo(..))` → propagated through.
- `ParityError::NoBootstrapFound` → tape can't be read with
  parity. Layer 5 reports to operator; may treat the tape as
  no-parity for emergency tar-only reads (with strong warning).
- `ParityError::FilemarkMapDigestMismatch` /
  `FilemarkMapReconstruct` → the tape is structurally
  inconsistent or too damaged to map; plain per-filemark tar
  extraction may still work, but parity recovery is unavailable.
  Layer 5 falls back to another copy.
- `ParityError::CapacityReserveExceeded` → not an error to
  surface raw. Layer 5 catches it at `begin_object` *before any
  object block is written* (the object has not started). If
  `cause == TapeCapacity`, Layer 5 closes the current tape cleanly
  (emit pending sidecars + final bootstrap) and writes the *entire
  object from the beginning* on another tape. If
  `cause == ParitySpoolCapacity`, the remedy is local: free or
  enlarge the parity spool and retry; switching tapes does not help.
  No mid-object spanning (§7.5).
- `ParityError::InvalidScheme` → write-session creation fails.
- `ParityError::SchemeMismatch` → bootstrap recorded a scheme
  the reader doesn't support; tape is suspect.

---

## 13. Implementation plan

Each step ends in `cargo fmt + cargo clippy --workspace
--all-targets -- -D warnings + cargo test --workspace + cargo doc
--workspace --no-deps`, all green. Steps are sized to be
commit-worthy individually.

| Step | Description |
|--|--|
| 11.0 | Crate skeleton: new `crates/remanence-parity` with `Cargo.toml` (reed-solomon-erasure, crc64fast, hmac, sha2, thiserror, lru, ciborium — no crc32fast, all 3c CRCs are CRC-64/XZ). `lib.rs` exporting the public module tree (model, raw, sink, source, bootstrap, sidecar, filemark_map, error). The `raw` module holds `RawTapeSource` + `RawTapeSink` + `PhysicalPositionHint` + `TapeGeometryHint` (§4.5), kept separate from the body-facing `BlockSource`/`BlockSink`. Empty stubs. Workspace `Cargo.toml` updated. |
| 11.1 | `ParityScheme`, `SchemeId`, `StripeAddress` (ordinal-based, §4.2), `StripePosition` value types. Validation (§11.3), including the `S × m × block_size` tolerance-floor warning. Unit tests for scheme math (`epoch_blocks`, `data_shards_per_epoch`, `parity_shards_per_epoch`, `overhead_ratio`, `contiguous_damage_threshold`). |
| 11.2 | Ordinal ↔ stripe mapping (`ordinal_to_stripe`, `stripe_data_to_ordinal`, §5.3). Property tests: round-trip every data ordinal in a small epoch and verify identity; verify the row-major interleave hits each (stripe, data_index) exactly once and that consecutive ordinals fall in distinct stripes (the dispersion property). |
| 11.3 | Filemark map (`FilemarkMap`, `FilemarkMapBuilder`, `FilemarkMapDigest`): build, canonical-serialize, digest, and look up `(tape_file_number, body_lba) → ParityDataOrdinal → physical`. Tests for digest stability and lookup across object/sidecar/bootstrap tape files. |
| 11.4 | Bootstrap tape-file format (§5.6): writer + parser, fixed magic, CBOR payload (scheme + `FilemarkMapDigest`), CRC. Tests for round-trip, version checking, CRC validation, no-parity flag. |
| 11.5 | Parity-epoch sidecar format (§5.5): header/index block(s) + raw shard blocks, per-tape HMAC-derived magic, half-open ordinal range, `real_data_shard_count`, **parity-shard CRCs and per-real-data-shard CRCs** in the index. Tests for magic derivation, header/index round-trip, both CRC kinds, and the multi-block index spill (`H > 1`, now ~3 blocks at default since data CRCs add ~512 KiB). |
| 11.6 | RS codec per **normative Appendix A** (`rs-cauchy-gf256-v1`): implement GF(2⁸)/0x11D arithmetic and the Cauchy generator `G[j][i] = 1/(X_j XOR Y_i)` directly, OR use `reed_solomon_erasure::galois_8` **after** a conformance test proves the crate's field, matrix, and shard ordering match Appendix A byte-for-byte. The encode path uses **incremental parity accumulation** (Option B): `accumulate(stripe, data_index, &shard)` adds `G[j][i] (×) shard` into the `m` accumulators. HARD GATES: (a) incremental accumulation of `S×k` shards == one-shot encode == Appendix A definition, over randomized shard orders; (b) crate-vs-appendix conformance vectors; (c) implicit-zero data shards for the final partial epoch (§5.4) reconstruct correctly. |
| 11.7 | `ParitySink::new`, `begin_object`, `write_block` (object data path: passthrough to inner + ordinal assignment + epoch accumulation, NO parity emission yet). Tests with an in-memory `BlockSink`: object blocks pass through unchanged, ordinals assigned only to object-data blocks. |
| 11.8 | Epoch completion + parity spool (§7.1, §7.4): when an epoch fills `S×k` data shards mid-object, move its ~512 MiB of parity accumulators to the spool (parity only, never object data); `finish_object` emits completed-epoch sidecars + the terminating filemark and asserts the body format already flushed its final block and did not exceed `projected_size_blocks`. Returns `ObjectCloseResult` incl. the current `highest_protected_ordinal`. Tests verify the "don't flush partial epoch at object close" rule (§5.4), the sidecar-bypass invariant (§5.2), and that an object closing inside an open epoch is reported parity-pending (§7.2.1). |
| 11.9 | `write_bootstrap` (bypass path, §5.2) + capacity reservation (`begin_object` reserve check, §7.5). Tests: bootstrap/sidecar blocks get no ordinal; the reserve includes `epochs_completed_by_this_object × sidecar_size` (not just already-pending parity); exceeding `projected_size_blocks` mid-object raises an `Invariant` violation; a too-large object at near-EOM returns `CapacityReserveExceeded { cause: TapeCapacity }`; insufficient local sidecar spool returns `CapacityReserveExceeded { cause: ParitySpoolCapacity }`. |
| 11.10 | `ParitySink::finish`: close the final partial epoch when `D>0` (§5.4, zero-pad-for-RS, `real_data_shard_count`); **skip the final sidecar when `D==0`**; emit the final bootstrap with `is_final_map=true` and a digest over the complete canonical map projection (§5.6); advance `highest_protected_ordinal` to EOD; return `TapeGeometry`. Tests cover full-epoch-at-EOM, partial epoch, the `D==0` skip, and the implicit-zero reconstruction round-trip. |
| 11.11 | Bootstrap discovery over `RawTapeSource` (§4.5, §8.1): `discover_scheme` (first valid copy) and `discover_authoritative_bootstrap` (highest-scope: `is_final_map` else highest `sequence`); the canonical map-projection digest (§5.6); and `acquire_filemark_map` returning a `ScopedFilemarkMap` — from the catalog (Complete), or scan-reconstruct validated against the authoritative copy, with **prefix validation** when only an intermediate copy survives (`MapScope::Prefix`). Tests: damaged-BOT-copy fallback; intermediate-copy prefix validation succeeds and bounds recovery (`OutsideValidatedMapPrefix` beyond the prefix); full-map digest mismatch; canonical-digest stability/non-circularity. |
| 11.12 | `ParitySource::new` + `open_object` + clean read: object-scoped `ObjectParitySource` passthrough within an object tape file (no parity to skip, §8.2). Tests confirm zero overhead when inner reads succeed, and that `write_block`/read outside an active object/open-object is rejected. |
| 11.13 | Recovery path (§8.3, §9.2) via `ObjectParitySource::recover_block_at`: resolve object-local `body_lba` → ordinal; **per-block** protection test (`failed_ordinal < highest_protected_ordinal`, §7.2.1) — NOT a whole-object test; map ordinal → stripe; gather data peers (across filemarks) + parity shards (from sidecar) + implicit zeros, **verifying each peer against its sidecar-recorded CRC** and treating a CRC mismatch as an erasure; reconstruct. Tests: recover ≤m erasures, fail >m; an early block of a large **partial** object (below the watermark) recovers while its tail returns `UnrecoverablePendingEpoch`; a silently-corrupt-but-clean-reading peer is rejected (not used to poison reconstruction); a recovery beyond a `Prefix` map scope returns `OutsideValidatedMapPrefix`. |
| 11.14 | Recovery cache (§9.3) with LRU eviction. Tests verify multi-block recovery within one stripe reuses cached peers. |
| 11.15 | `RecoveryEvent` + audit-hook plumbing (§9.4). Tests verify events fire on success and failure with correct `(tape_file_number, body_lba)` addressing. |
| 11.16 | Corruption injection suite: a `CorruptingBlockSource` simulating single-block MEDIUM_ERROR; contiguous runs of various sizes (incl. spanning a filemark into a sidecar); servo-style positioning failures; a destroyed sidecar (→ epoch-no-parity → caller falls back). Test matrix per §14. |
| 11.17 | Configuration + CLI: default/conservative scheme constants (block-size-aware S), validation, error messages; `--parity default|conservative|none|custom:k,m,S` in `remanence-cli`. |
| 11.18 | Integration test against QuadStor: `#[ignore]`-gated in `tests/quadstor_parity.rs`. Write a small multi-object parity-protected tape (a couple of epochs incl. one spanning a filemark), read back clean, simulate corruption, verify recovery and catalog-less scan-reconstruct. |
| 11.18a | **Commit durability barrier (§7.7).** The `DriveHandleRawSink` adapter implements `write_filemark` as a synchronous flush (IMMED=0 / MTWEOF); a mock `RawTapeSink` models the flush boundary explicitly. Tests assert Layer 5's commit ordering (blocks → sync filemark → position → catalog commit) and that the mock rejects a catalog commit attempted before the synchronous filemark returned. |
| 11.18b | **Restart / append-after-crash (§7.8).** `resume_append_from_committed_prefix`: reload the committed `catalog_tape_files` prefix, verify + position past the last committed tape file, rebuild the FilemarkMapBuilder, and perform the open-epoch rebuild — re-read object blocks over `[W, T)`, re-accumulate, emit any now-complete-epoch sidecars, commit those resume-generated sidecars with the ordinary sidecar catalog transaction, and load the partial epoch as live `EpochState`. Returns `ResumeAppendResult`. Tests (mock tape, then QuadStor, then one scratch LTO): (1) crash after object filemark, before any sidecar → resume at object boundary, rebuild accumulators; (2) crash after sidecar 1 of a cluster → resume after sidecar 1, not the object; (3) crash mid-sidecar → provisional entry, resume truncates before it; (4) crash after sync filemark, before DB commit → tape has an extra file, catalog doesn't, resume truncates from the committed prefix; (5) crash after DB commit → catalog and tape agree; (6) `W < T` open-epoch rebuild round-trips to byte-identical accumulators and a clean `finish()` after resume protects everything; (7) unreadable block during rebuild → fail cleanly, Layer 5 falls back to another copy. |
| 11.19 | Live smoke on production MSL3040: same against a scratch LTO-9 tape, **including a deliberate power-loss/restart cycle exercising §7.8**. Pending hardware access window. |
| 11.20 | Wrap-up: design-doc sync, journal entry, status table update. |

**v0.5 implementation steps (addendum integration).** These extend
the plan above; they reuse the same green-build discipline and slot
in as noted. The owned-GF(2⁸)-codec requirement is already step 11.6
(implement Appendix A directly; the crate is an optional accelerator
gated by a byte-identical conformance test) and is unchanged.

| Step | Description |
|--|--|
| 11.5b | **Sidecar metadata replication (§5.5).** Extend the sidecar writer/parser with the header extension (`copy_kind`, locators, `canonical_metadata_hash`, `schema_version = 2`), the tail header/index copy, and the `SidecarFooter`. Reader recovers classification + metadata from footer + tail when block 0 is damaged. Tests: primary-only, tail-only, footer-only, all-damaged; primary/tail hash agreement; small-final-sidecar proximity. |
| 11.5c | **Sidecar directory + `parity_map` (§5.6.1).** Directory model + CBOR; computed inline-vs-external sizing rule; `parity_map` tape file codec (primary/tail/footer) + `ParityMapReference`; bootstrap fields 20/21; commit ordering. Tests: force an overflowing scheme → `parity_map` emitted; catalog-less scan validates payload hash + map digest; damaged inline directory falls back to `parity_map`; damaged `parity_map` primary falls back to tail. |
| 11.8b | **No deferral + object commit bundle (§7.4, §7.8).** Emit all completed sidecars at `finish_object`; `ObjectCommitBundle` atomic catalog transaction; assert the `T − W < one epoch` invariant on commit and on restart; `SidecarBurstPolicy`; `LargeObjectSidecarClusterRisk` audit. Replaces any volatile-deferral path. |
| 11.11b | **Catalog-less scan v0.5 (§8.1).** Scan → directory-overlay → digest-validation; separate map validity from sidecar usability; copy-health excluded from the digest. Tests: one damaged sidecar header disables only its epoch (acceptance A/B). |
| 11.13b | **Bulk / epoch recovery (§9.2.1, §9.3).** `recover_region` / `recover_ordinal_range`; epoch-grouped planner (load sidecar once, dedup peers, physical-order read); `BulkRecoveryPolicy` / `EpochRecoveryCache` with windowed fallback; auto-escalation from `recover_block_at`. Tests: 64 MiB and 512 MiB contiguous regions recover reading each peer at most once per epoch plan; low memory cap windows correctly; 513 MiB region partial-fails as expected. |
| 11.9c | **Object-too-large preflight + drive compression (§7.5, §6.1.1, §11.6).** `BeginObjectError::ObjectTooLargeForEmptyTape` before any block; `TapeWriteConfig.drive_compression` configured + read-back-verified by the 3a adapter; bootstrap/catalog record `drive_compression=false`; refuse 3c recovery on a compression=true tape. |
| 11.19b | **v0.5 hardware proof gates (§14).** Sidecar-damage, parity-map, catalog-less, bulk-recovery, bundle power-loss/restart, filemark durability, and compression-disablement tests on mock → QuadStor → scratch LTO-9, including a deliberate power-loss cycle. |

### 13.1 Dependencies on other layers

3c's actual dependency on 3b is narrower than the layer naming
implies. 3c does **not** depend on:

- The body format trait shape (`TapeFormat`, `FileAddressable`,
  `ByteRangeAddressable`, `Verifiable`).
- The format registry (`FormatRegistry`).
- The catalog reader/writer or the catalog CBOR schema.
- `rem-chunked-v1` or any specific body format.

3c **does** depend on:

- The body-facing `BlockSink` / `BlockSource` traits (3b spec
  §4.5), which 3c *implements* on `ParitySink` / `ObjectParitySource`.
- A `DriveHandle` adapter for 3c's own raw traits `RawTapeSink` /
  `RawTapeSource` (§4.5): `DriveHandleRawSink` /
  `DriveHandleRawSource`. (3c wraps the raw traits and exposes the
  body-facing ones; the raw adapters may live in 3c or beside the
  3b adapters.)

These together correspond to **3b's step 10.2** in the 3b
implementation plan. After step 10.2 lands, 3c can be
implemented to completion (steps 11.0-11.18) in parallel with
the rest of 3b (steps 10.3-10.15).

**Optional structural refactor for true parallelism:** If both
3b and 3c are to be developed in genuine parallel rather than
"3b-step-10.2 first, then fork," consider moving `BlockSink`,
`BlockSource`, and the `DriveHandle` raw/body adapters out of
`remanence-format` and into `remanence-library` (or a small
dedicated `remanence-blockio` crate). The traits are about
block-level I/O abstraction (matching 3a's level), not about
format. Format-aware things (object writers, catalogs,
capabilities) stay in `remanence-format`. With this refactor,
3b and 3c become true siblings depending on `remanence-library`.

Lean: do the refactor. The traits are at the right conceptual
level for `remanence-library`, and the refactor is cheap. Either
crate location works; this is a soft preference.

Other layer dependencies:

- 3a: complete (steps 9.0-9.8 merged). No further work required.
- Layer 5 (gRPC API): consumes recovery events via the audit
  hook. Not on 3c's critical path; the audit hook is a trait
  object 3c can be tested against with a mock.

**Implementation ordering options:**

The dependency analysis above allows three possible orderings:

1. **3b before 3c (the original order).** Build 3b fully, then
   3c on top. Conservative; ensures the body format and parity
   layer are tested against each other from the start.

2. **3b skeleton, then 3c, then rest of 3b.** Build 3b steps
   10.0-10.2 (crate skeleton, traits, adapters), then 3c steps
   11.0-11.18 to completion, then return to 3b steps 10.3-10.15.
   Has the advantage that 3c (the simpler-surface layer) is
   complete and well-tested before the larger 3b surface gets
   built on top.

3. **3b and 3c in parallel after the refactor.** After moving
   `BlockSink`/`BlockSource` into `remanence-library` and
   completing 3b steps 10.0-10.1 (crate skeleton + capability
   types only), both 3b and 3c can be developed independently.

Recommendation: option (2) or (3) is generally preferable to
(1). 3c is the simpler artifact (one trait wrapper, one
encoding library, well-bounded surface). Building it first
exercises 3a more thoroughly than 3b would (3c writes thousands
of blocks per epoch and exercises filemark navigation heavily;
3b mostly writes objects with filemarks), surfacing 3a bugs
sooner. 3c also locks in the on-tape physical layout (tape
files, epochs, sidecars) before the format layer builds on it,
which makes 3b's job easier.

Option (2) has the lowest workflow friction for a single-
developer setup. Option (3) is appropriate if 3b and 3c will be
developed by different people (or different Claude Code
sessions) concurrently.

### 13.2 What's left out of v1

- **Parity scrubbing scheduler.** Belongs in Layer 5.
- **Parity-only re-encoding** of existing tapes. Out of scope.
- **Sidecar shard-payload redundancy / parity-on-parity.** v1
  replicates sidecar *metadata* (primary + tail + footer, §5.5) and
  attests it via the bootstrap/`parity_map` directory (§5.6.1), but
  does **not** replicate or RS-protect the ~512 MiB shard *payload*;
  a destroyed payload relies on the three-copy archive policy (§16).
  Revisit only if a single-copy tape tier is introduced.
- **Volatile sidecar deferral.** Prohibited in v1 (§7.4); a future
  version may add it only behind the named preconditions there
  (crash-durable spool, budgeted multi-epoch rebuild, or a very small
  bound).
- **Erasure-code variants other than Cauchy GF(2⁸).** The
  scheme ID is extensible; future versions can use GF(2¹⁶) or
  alternative codes (LDPC, etc.) if the math becomes more
  favorable. v1 ships only `rs-cauchy-gf256-v1`.

---

## 14. Testing strategy

This section is the short in-spec summary. The normative cross-layer
implementation gate lives in `docs/remanence-testing-plan.md`, which
spells out the brutal mock-tape, corruption-injection, crash-window,
QuadStor, and live-scratch-tape matrix that must pass before production
writes.

Five-tier shape:

1. **Math unit tests** (`remanence-parity/src/model.rs`,
   `src/mapping.rs`). Ordinal ↔ stripe mappings round-trip;
   scheme validation; epoch-size math. Property-based via
   `proptest` for the ordinal mapping (incl. the dispersion
   property: consecutive ordinals → distinct stripes).

2. **Encoder/decoder tests** (`remanence-parity/src/codec.rs`).
   The `reed-solomon-erasure` crate's own tests cover the RS
   math; our tests verify the integration, including implicit-
   zero shards for the final partial epoch. Tests at small
   stripe sizes for fast iteration; one comprehensive test at
   default parameters.

3. **Bootstrap + sidecar format tests**
   (`src/bootstrap.rs`, `src/sidecar.rs`). Round-trip CBOR/
   header payloads, validate magic + CRC, exercise the no-parity
   flag, version checking, and the sidecar index spill (H > 1).

4. **In-memory round-trip** (`remanence-parity/tests/round_trip.rs`).
   Use the in-memory `BlockSink`/`BlockSource` from
   `remanence-format`'s test infrastructure. Write a multi-
   epoch, multi-object tape (including an epoch that spans the
   filemark between two objects, and one large object that
   completes several epochs via the spool) with bootstraps at
   writer-policy points; reconstruct the filemark map by scan
   and validate against the digest; parse it back; verify every
   block reads correctly. The "happy path" baseline.

5. **Corruption injection** (`remanence-parity/tests/recovery.rs`).
   `CorruptingBlockSource` simulates failure modes. Test matrix:
   - Single-block erasure at various stripe positions.
   - Multi-block contiguous data erasure of N blocks for
     N ∈ {1, 50, 128, 512, 2048, 2049, 4000} (around the
     `S×m`=2048 tolerance at defaults).
   - Erasure that spans a filemark from an object into the
     adjacent sidecar.
   - Servo-style (LOCATE fails) vs MEDIUM_ERROR (READ fails).
   - Data-shard-only erasures; recovered from sidecar parity.
   - A destroyed parity sidecar: epoch loses parity on this
     tape; verify the caller is told to fall back to another
     copy (no false "recovered").
   - A destroyed bootstrap copy: discovery falls back to the
     next position.
   - Final-partial-epoch recovery using implicit-zero shards.
   - Catastrophic erasures exceeding m: graceful failure with
     correct `Unrecoverable` reporting.

6. **Live smoke** (`#[ignore]`-gated). QuadStor (step 11.17) and
   production MSL3040 (step 11.18).

The corruption-injection test suite is the most operationally
important tier. It's the only way to verify recovery semantics
without weeks of waiting for real-world damage. The matrix
should be exercised on every release.

### 14.1 Hardware proof gates (v0.5 — before live tape)

Mock and QuadStor tests are necessary but not sufficient. The
following must pass on scratch LTO media on the actual drive/library
model before production live-tape use:

**Sidecar metadata damage (§5.5, §8.1).**
1. Damage the primary header block only → scan uses footer + tail; map validates; epoch recovers.
2. Damage the tail header only → scan uses primary; map validates; epoch recovers.
3. Damage primary + tail, footer intact → bootstrap/`parity_map` directory classifies it; map validates; that epoch is `SidecarMetadataUnavailable`; unrelated epochs recover.
4. Damage primary + tail + footer → directory classifies it by `block_count` match; map validates; that epoch unavailable; unrelated epochs recover.
5. Remove/damage a whole sidecar tape file → map validates only if the directory accounts for it structurally; that epoch fails; unrelated epochs recover.

**`parity_map` (§5.6.1).** Force an overflowing scheme → writer emits a `parity_map` + `ParityMapReference`; catalog-less scanner validates payload hash + map digest; damaged inline directory still uses `parity_map`; damaged `parity_map` primary uses its tail; damaged `parity_map` + one sidecar primary falls back to that sidecar's tail/footer.

**Catalog-less recovery (§8.1).** With Postgres absent, reconstruct the map by scan, validate against the final bootstrap (or referenced `parity_map`) digest, recover a damaged object block; repeat with one damaged sidecar header and prove unrelated epochs still recover.

**Bulk recovery (§9.2.1).** Inject 64 MiB and 512 MiB contiguous regions; verify each peer shard is read at most once per epoch plan; compare per-block vs bulk timing; a 513 MiB region partial-fails as expected; a low `max_recovery_cache_bytes` windows without exceeding the cap.

**Power-loss / bundle restart (§7.4, §7.8).** Pull power: before the object filemark; after the object filemark but before the sidecar burst completes; mid-sidecar; after all sidecars but before the bundle DB commit; after the bundle DB commit; mid-burst. On restart, verify the append point is after the previous committed bundle when the current bundle was incomplete, and after the current bundle when it committed; prove the committed-prefix rebuild satisfies `T − W < data_ordinals_per_epoch`.

**Filemark durability (§7.7).** Prove on the target drive that a returned synchronous filemark (IMMED=0 / MTWEOF) is repositionable after power loss, and that the catalog row is committed only after it returns.

**Drive compression (§6.1.1, §11.6).** Attempt a parity write with compression externally enabled → 3a detects and disables it before write; if disabling/verifying fails, the write fails before the BOT bootstrap; bootstrap + catalog record `drive_compression=false`.

### 14.2 Acceptance criteria (production-readiness gate)

Implementation is not production-ready until all pass:

```
A. One damaged sidecar primary header does NOT cause FilemarkMapDigestMismatch.
B. A sidecar with both header copies damaged disables only that epoch's parity.
C. Catalog-less recovery works with Postgres absent.
D. Directory overflow uses parity_map; conservative/custom schemes keep directory robustness.
E. No committed v1 catalog state requires restart rebuild of >= one full epoch.
F. Crash mid-object-bundle retries from the previous committed bundle, not a multi-epoch rebuild.
G. Bulk recovery of a 512 MiB region uses each peer shard at most once per recovery window.
H. Bulk recovery respects max_recovery_cache_bytes.
I. Drive compression cannot be enabled for a parity-protected write session.
J. The in-house GF(2⁸) codec is used unless the crate passes byte-identical conformance.
K. Objects too large for an empty tape (after parity + reserve) are rejected before any block.
L. Synchronous filemark durability is proven on the actual LTO drive model by power-loss test.
M. Open-epoch rebuild after restart is proven by power-loss test.
N. Final and intermediate bootstraps carry an inline sidecar directory or a valid parity_map reference for their scope.
```

---

## 15. Open questions

### 15.1 Bootstrap discovery scan window

§7.3 places a bootstrap copy at BOT; §8.1 falls back to the
~1/3, ~2/3, and near-EOD copies if BOT is unreadable. Because
bootstraps are their own filemark-delimited tape files, a reader
in the catastrophic case scans forward across tape files looking
for the bootstrap magic.

**Normal operation:** the BOT bootstrap is intact. The reader
reads it, gets scheme + map digest, done. No scan window matters.

**Catastrophic recovery:** the BOT copy is unreadable and the
reader scans for any other bootstrap copy. With object tape
files up to ~1 TB in the target archive (4K masters, compound
archives), the distance between bootstrap copies can be large,
so this is a slow path (potentially hours of sequential
spacing). Falling back to another tape copy is the
operationally preferred response in this case.

The reader's logic stays simple: try BOT first; else scan from
each expected fractional position forward (by spacing over
filemarks) until a bootstrap tape file is found. The per-region
scan distance has no fixed upper bound in the catastrophic path.
This remains an open tuning question only in *where* exactly to
place the non-BOT copies for the best expected-recovery-time
trade-off; the mechanism is settled.

### 15.2 Parallelism in the writer

The proposal in §7 is single-threaded: parity computation
happens inline with the write pipeline. At default parameters
this is well within budget (CPU is not the bottleneck), but for
a wider scheme (say `k=200, m=10`, still within the GF(2⁸)
`k+m ≤ 255` limit of Appendix A) it might be worth running RS
encoding in a background thread. A scheme with `k+m > 255`
(e.g. `k=256`) is not representable in GF(2⁸) at all and would
require a future GF(2¹⁶) scheme ID, not just more threads.

Lean: defer. Performance is adequate at defaults; revisit if
empirical measurement shows a bottleneck.

### 15.3 Cache warming on read

The recovery cache (§9.3) is populated on demand. Pre-warming
based on access patterns is complex and unclear ROI.

Lean: do nothing. On-demand caching is simple and correct.

### 15.4 Bootstrap magic — fixed vs derived

§5.6 uses a fixed magic `REM\x00BOO\x01` for bootstrap blocks
because the reader needs to find a bootstrap before knowing the
tape UUID. The 2⁻⁶⁴ user-data collision concern is mitigated by
the magic check happening at known LBA regions plus CRC
validation. Open: should the bootstrap magic be derived from
some publicly-known parameter (e.g., the schema spec version)
to reduce collision risk further? Probably not worth the
complexity; the current scheme is fine.

### 15.5 Inter-epoch damage (largely resolved by the ordinal model)

In v0.2, damage straddling a physical neighborhood boundary hit
two neighborhoods and effectively halved tolerance there. In
v0.3 epochs are defined over `ParityDataOrdinal`, not physical
position, so "epoch boundaries" are not physical tape locations
a damage region can straddle in the same way — consecutive
object-data blocks belong to one epoch until `S×k` ordinals are
consumed, regardless of where filemarks fall. A contiguous
physical-damage region within an object stays within one epoch
unless it is large enough to cross from the tail of one epoch's
data into the next epoch's data, in which case each epoch's
portion is independently recoverable up to its own `S×m` budget.

This is no longer a notable failure mode; the only residual
question is empirical (do real damage events exceed `S×m`
blocks?), which Appendix B's data answers in the negative for
the target environment.

### 15.6 Recovery read-amplification (mitigated by bulk recovery)

A *per-block* recovery read requires up to k inner reads (plus k
LOCATEs). At default k=128 and worst-case LOCATE latency, recovery
of one isolated block can take 30+ seconds. For a contiguous
damaged region this would be catastrophic block-by-block — but that
is exactly the case `recover_region` / `recover_ordinal_range`
(§9.2.1) handle: the affected stripes share most of their surviving
peers, so the epoch planner reads each peer once in physical order
rather than re-LOCATEing per block, turning the region into roughly
one sequential pass over the epoch's survivors.

The residual point still holds: a recovery-heavy workload is
fundamentally limited by tape mechanics, and parity makes
individual reads slower in exchange for making them succeed at all.
The remaining open tuning question is the default
`max_recovery_cache_bytes` and window sizing on a given host (§9.3).

---

## 16. Out of scope

Already covered in §1 non-goals, but worth restating:

- **No replacement of LTO's built-in ECC.** Layer 3c is in
  addition to the drive's per-block inner+outer Reed-Solomon.
- **No protection against complete tape destruction.** Three-
  copy redundancy at Layer 5 handles that.
- **No format awareness.** 3c does not know what's in the
  blocks it protects; it treats them as opaque bytes.
- **No scrubbing scheduler.** Belongs in Layer 5.
- **No write verification policy.** That's spec §9.4 / Layer 5.
- **No PERSISTENT RESERVE handling.** That's spec §9.3 / Layer 5.
- **No encryption handling.** Encryption is per-block by the
  drive (Layer 6); parity blocks and data blocks are encrypted
  identically and 3c is oblivious.

---

## 17. References

- `docs/spec-v0.3.md` §5, §9.4 — on-tape format, write
  verification policy.
- `docs/layer3a-design.md` — SSC primitive set on `DriveHandle`.
- `docs/layer3b-design.md` — body format trait, `BlockSink` /
  `BlockSource`, catalog schema.
- `docs/3b-catalog-schema-followup.md` — the `catalog_tape_files`
  filemark-map table and related schema changes (§10).
- `docs/rem-tar-v1-design.md` v0.8.1 — the default body format;
  §9.1.1 (final-block flush), §13.2 (`recover_block_at`), §2.1
  (`BodyLba`).
- `docs/pfr-reference.md` — chunk-size rationale, encryption
  posture, integrity model.
- `reed-solomon-erasure` crate documentation:
  <https://docs.rs/reed-solomon-erasure>.
- IBM LTO SCSI Reference GA32-0928-08 — sense codes for
  MEDIUM_ERROR, HARDWARE_ERROR, and NOT_READY used in §9.1.
- Backblaze, "Reed-Solomon Coding for Data Backup":
  <https://www.backblaze.com/blog/reed-solomon/>.

---

## Appendix A: `rs-cauchy-gf256-v1` — normative encoding

The scheme ID `rs-cauchy-gf256-v1` (recorded in the bootstrap,
§5.6) names an exact, implementation-independent encoding. An
implementation MAY use the `reed-solomon-erasure` crate, but the
math below — not the crate's current internals — is the
authority. Two implementations that both claim
`rs-cauchy-gf256-v1` MUST produce byte-identical parity shards
for the same data shards; a 30-year format cannot depend on a
library's incidental matrix choices.

### A.1 Field

GF(2⁸) with the irreducible polynomial

```
x^8 + x^4 + x^3 + x^2 + 1     = 0x11D
```

Field elements are bytes `0x00..0xFF`. Addition is XOR.
Multiplication is carry-less polynomial multiplication reduced
modulo `0x11D`. The generator `g = 0x02` (the element `x`) has
multiplicative order 255 and is used to build log/antilog
tables. NOTE: `0x11D` is a common Reed-Solomon field polynomial;
it is **not** the AES/Rijndael polynomial (which is `0x11B`).
Do not assume an AES GF(2⁸) routine is compatible — it usually
reduces modulo `0x11B`. An implementation MUST verify its
multiply against published GF(2⁸)/`0x11D` vectors before use.

### A.2 Shards

For an epoch with `k = data_blocks_per_stripe` and
`m = parity_blocks_per_stripe`, each stripe has `k` data shards
`d_0 .. d_{k-1}` and `m` parity shards `p_0 .. p_{m-1}`, each
shard being one `block_size`-byte tape block. Encoding is
**systematic**: data shards are stored verbatim (in the object
tape files), only parity shards are computed (into the sidecar).
GF arithmetic is applied byte-wise and independently across the
`block_size` byte positions of a shard.

### A.3 Generator matrix (Cauchy construction)

The `m × k` parity generator matrix `G` over GF(2⁸) is the
Cauchy matrix

```
G[j][i] = 1 / (X_j XOR Y_i)        (GF(2⁸) inverse and XOR)

with the partition of distinct field elements:
    Y_i = i            for i in 0..k     (data-col seeds)
    X_j = k + j        for j in 0..m     (parity-row seeds)
```

The element sets `{Y_i} = {0..k-1}` and `{X_j} = {k..k+m-1}` are
disjoint and contiguous by construction for every valid scheme
(`k + m <= 255`, validated in §11.3): `Y_i < k <= X_j`, so every
`X_j XOR Y_i != 0` and the inverse exists. (This corrects the
v0.3.3 seeds `X_j = 0x80 + j`, which overlapped `Y_i = i` once
`k > 128` — wrong for large-`k` schemes; the contiguous `k + j`
partition is safe across the whole `k + m <= 255` domain.) A
Cauchy matrix has every square submatrix invertible, which is
exactly the MDS property erasure decoding needs: any `k` of the
`k+m` shards reconstruct the stripe. §11.3 additionally
validates `k + m <= 255` and rejects any scheme outside it
(GF(2⁸) cannot represent more than 255 distinct nonzero seeds).

### A.4 Encoding (systematic, incremental-compatible)

Parity shard `j` of a stripe is

```
p_j = XOR over i in 0..k  of  ( G[j][i] (×) d_i )      (GF mult, then XOR-accumulate)
```

where `(×)` is GF(2⁸) multiply applied to each byte of `d_i`.
This is a fixed linear combination, so it is independent of the
order in which the `d_i` arrive — which is what makes the
incremental accumulator model (§7.1) byte-identical to a batch
encode: each arriving `d_i` adds `G[j][i] (×) d_i` into
accumulator `p_j`. The hard correctness gate (impl step 11.6) is
exactly this identity: incremental accumulation == one-shot
encode == this appendix's definition, over randomized shard
orders.

### A.5 Decoding

Given any `k` surviving shards (data and/or parity) of a stripe,
form the `k × k` submatrix of the systematic generator
`[ I_k ; G ]` corresponding to the surviving shard indices,
invert it over GF(2⁸) (the Cauchy/MDS property guarantees it is
invertible), and multiply to recover the original `k` data
shards; recompute any still-missing parity shards by §A.4.
Implementations MAY delegate this to
`reed_solomon_erasure::galois_8::ReedSolomon::reconstruct`
**only after** verifying (impl step 11.6) that the crate's
field, matrix, and shard ordering match this appendix; otherwise
they MUST use their own decoder over this definition.

### A.6 Ordering and layout

- **Stripe assignment** of data shards is the row-major
  interleave over `ParityDataOrdinal` of §5.2.1
  (`stripe_index = o mod S`, `data_index = o div S` within the
  epoch).
- **Parity shard order** in the sidecar is `(stripe_index,
  parity_index)` ascending, `stripe_index` major — the order the
  §5.5 parity index lists them.
- **Final partial epoch** zero-pads missing data shards to `S×k`
  as implicit zeros for §A.4 (§5.4); they are never written.

### A.7 Conformance vectors

An implementation MUST reproduce these exactly before it is used
on tape. They were computed directly from the definitions above
(GF(2⁸)/`0x11D`, seeds `Y_i = i`, `X_j = k+j`).

GF(2⁸) inverses (sanity): `inv(0x02) = 0x8e`, `inv(0x03) = 0xf4`.

Encoding, `k = 2, m = 2` (so `Y = [0,1]`, `X = [2,3]`):

```
Generator G[j][i] = 1/(X_j XOR Y_i):
    G = [ 0x8e  0xf4 ]
        [ 0xf4  0x8e ]

Data shards (4 bytes each):
    d0 = 01 02 03 04
    d1 = 10 20 30 40

Parity shards p_j = XOR_i ( G[j][i] (×) d_i ):
    p0 = 75 ea 9f c9
    p1 = fc e5 19 d7
```

Reconstruction, recover `d1` from the survivors `{d0, p0}`: form
the 2×2 matrix mapping `[d0,d1]` to the survivors (`d0`'s row is
`[1,0]`; `p0`'s row is `G[0] = [0x8e,0xf4]`), invert over GF(2⁸):

```
M^{-1} = [ 0x01  0x00 ]
         [ 0x8f  0x03 ]

applied to [d0, p0] byte-wise recovers:
    d0 = 01 02 03 04
    d1 = 10 20 30 40      (the lost shard)
```

CRC-64/XZ check value (§5.5): CRC of ASCII `123456789` is
`0x995DC9BBDF1939FA`.

The implementation's RS test suite (impl step 11.6) MUST include
these vectors as fixed assertions in addition to the
incremental==batch and crate-conformance gates.

### A.8 Versioning

Any change to the field, the matrix construction, the seed
partition, the systematic/parity ordering, or the interleave
constitutes a new scheme ID (`rs-cauchy-gf256-v2`, …), never a
silent change to `v1`. A reader that does not recognize a scheme
ID treats the tape as unreadable-for-parity (clean tar still
works) rather than guessing. The scheme ID is bound to this
exact math, not to any crate version.

---

## Appendix B: The LTO-4 dust incident (motivating case study)

Around 2014-2016, the Archives team ran into a class of failures
on LTO-4 tapes that exposed the limits of the medium's built-in
protection and ultimately motivated this design.

Symptoms observed:

- Specific physical regions of tape became unreadable —
  contiguous block ranges in the middle of otherwise-healthy
  tapes.
- The drive could not LOCATE to LBAs within these regions; it
  returned positioning errors. SPACE past the region worked,
  and blocks on the other side read normally.
- Affected regions ranged from ~50 MB to ~2 GB of contiguous
  tape (estimated from the LBA gaps).
- LTO's built-in inner+outer Reed-Solomon ECC was bypassed
  entirely — the drive's heads could not be positioned to read
  the affected wraps in the affected longitudinal range.

Root cause hypothesis (consistent with all observations but not
formally confirmed): dust contamination damaged the pre-written
servo tracks in localized regions of tape. Without working servo,
the drive could not position the heads at the affected
longitudinal positions, so neither data nor the inner+outer ECC
covering it could be read.

At the time, the team attempted a remediation strategy of
post-hoc Reed-Solomon parity computation: read a tape's full
contents to a disk image, compute parity over the image, store
the parity as a separate file. The approach had problems:

- Computing RS over an 800 GB monolithic codeword took ~1 day
  per tape on the available hardware.
- The data-movement cost (full tape read to disk, full disk
  read to encoder) dominated.
- The remediation was post-hoc rather than inline, so the
  parity didn't exist for tapes already damaged.
- The strategy was abandoned after a few tapes due to
  prohibitive cost.

The lessons informing Layer 3c's design:

1. **The failure mode is positional contiguous damage**, not
   randomly distributed bit errors. The protection scheme
   must defend against contiguous loss, not against scattered
   single-block failure.
2. **Computation must be inline.** Post-hoc encoding requires
   reading the tape, which is exactly what's failing.
   Computing during the original write is essentially free.
3. **Per-stripe RS is dramatically faster than monolithic RS.**
   The LTO-4 attempt used one codeword for the whole tape;
   3c's per-stripe approach uses ~280,000 small codewords,
   each computed independently in milliseconds.
4. **Distribution matters more than overhead.** A naive
   "k consecutive data blocks, m consecutive parity blocks"
   layout would have left the LTO-4 damage entirely
   unrecoverable. The interleave pattern (§5.2.1) converts
   positional contiguous damage into approximately uniform
   per-stripe damage.
5. **Three copies plus light parity beats one copy plus heavy
   parity.** The three-copy redundancy at the orchestration
   layer means parity's job is "improve the recoverability of
   this copy," not "be the sole defense." 3% overhead with
   distributed parity is the right point in that trade-off
   space for the target archive.

The same LTO-4 damage, evaluated under the v0.3 scheme:

- A 500 MB contiguous damage region (well within the typical
  range observed): fully recovered by the affected epoch's
  parity (S=512, m=4 → ~512 MiB tolerance at 256 KiB blocks).
  No reader-visible impact beyond a logged RecoveryEvent.
- A 2 GB contiguous damage region (the worst observed): exceeds
  one epoch's `S×m` budget; some stripes lose more than m=4
  shards. Partial recovery with structured `Unrecoverable`
  errors for specific `(tape_file, body_lba)` ranges; the body
  format reports unrecoverable byte ranges within affected
  files; the orchestrator falls back to another tape copy for
  those ranges. (If the damage also took out the epoch's
  sidecar, that epoch's parity is gone on this tape and fallback
  is immediate — §8.4.)
- A bootstrap copy within a damaged region: discovery (§8.1)
  automatically falls back to the next bootstrap copy.

The takeaway: 3c protects against the failure mode that actually
happens in this archive, at a cost (3.125% capacity) that's
operationally trivial, and now without sacrificing per-object
filemarks. The worst-case damage events remain reliant on
multi-copy fallback — which is exactly what multi-copy is for.

---

*End of design v0.4.4 (implementation-ready; live-tape append/restart specified, implementation-test plan added). Comments and corrections welcome — please
annotate inline rather than rewriting.*
