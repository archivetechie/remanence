# rem-tar-v1: Default Tape Body Format

**Status:** **v0.9.3 — implementation-ready** (alongside Layer
3c v0.4.4). The body format is settled; rem-tar development
against a mock per-object `BlockSink`/`BlockSource` can begin
now, in parallel with 3c's mock implementation. Live integration
follows once both pass their mock-tape, corruption-injection,
and (3c-side) crash/restart suites. Sister documents:
`docs/layer3a-design.md` (tape mechanism),
`docs/layer3b-design.md` (body format layer),
`docs/layer3c-design.md` (parity, **v0.4.4**),
`docs/3b-catalog-schema-followup.md` (catalog schema),
`docs/remanence-testing-plan.md` (cross-layer implementation tests).
Spec reference: `docs/spec-v0.3.md` §1 (format longevity priority),
§5 (on-tape format requirements).

**Changes from v0.9.2 (ninth review — implementation-hardening patch).**
No body-format architecture change. This pass removes the last places
where implementation would otherwise have to guess:

- **`projected_size_blocks` counts the global pax body (§9.1).**
  The planner now counts the global pax header record plus the
  rounded global pax body, not just the 512-byte record. The safest
  implementation is a counting-mode dry run that uses the same
  header-sizing/padding code as the writer.
- **Large-file hashing/spooling policy clarified (§9.2).** Immutable
  or snapshot-backed sources use a two-pass reread with no temp copy;
  mutable sources spool exact bytes during pass 1 and write the spool
  during pass 2. Pass 2 re-hashes and compares to pass 1 before the
  object can commit.
- **Testing plan split out.** `remanence-testing-plan.md` is the
  cross-layer test gate, including tar compatibility, pax-padding
  digit-boundary tests, mock-tape crash windows, catalog invariants,
  and live scratch-tape smoke.

**Changes from v0.9.1 (eighth review — prose/arithmetic cleanup).**
No body-format change. The substantive part of the eighth review
(crash/restart/append-resume, commit durability) is a 3c concern
and landed in 3c v0.4.4 §7.7–7.8; the rem-tar-side items were
cleanups:

- **Stale "parity insertion" prose removed (§7.2, §7.3,
  Appendix A).** Phrases implying parity is interleaved inside
  object archives ("inserts parity invisibly," "skips interleaved
  parity blocks") are replaced with the sidecar/filemark-map
  model: object tape files contain no parity blocks; 3c maps
  `(tape_file_number, BodyLba)` to physical position via the
  filemark map, and parity lives only in sidecar tape files.
- **Appendix A arithmetic fixed.** `clip_001.mxf` (1,258,291 B)
  has a 209,715-byte tail after four 256 KiB chunks, not 213,811.

**Changes from v0.9.0 (seventh review — pre-live-tape patches).**
No body-format design change; exact wire/API contract fixes
required before live hardware:

- **pax-padding equation corrected (§7.6).** The alignment
  equation now includes the pax extended-header's own 512-byte
  ustar record AND the file's ustar header:
  `O + 512 + roundup512(P) + 512 ≡ 0 (mod S)`, with `P` the whole
  pax body (rounded as a unit, not per-record). The v0.9.0 form
  omitted the two 512-byte records and would have misaligned file
  data by 512 bytes. This is the layout solver's core equation —
  fixed before any code depends on it.
- **`recover_block_at` unified to 3c's object-scoped shape
  (§13.2, step 12.11).** The call lives on `ObjectParitySource`
  (which knows `tape_file_number`) and takes only `body_lba`;
  rem-tar opens the object source, locates the chunk, then
  recovers. Removed the stale tape-scoped reader / two-arg variants.
- **`projected_size_blocks` planning before `begin_object`
  (§9.1).** rem-tar now computes a conservative object block-count
  upper bound (3c §7.5 requires it for the capacity reserve) in a
  pre-write planning step P1.
- **Drive `write_config` moved to write-session setup (§9.1).**
  Fixed block size + compression-off happen once, before
  `ParitySink::new` and the BOT bootstrap — not inside the
  per-object sequence.
- **Manifest excludes itself (§8).** Normative: the
  `ObjectManifest.FileEntry` array lists payload files only,
  never `_remanence/manifest.cbor`; the manifest's identity/hash
  live in its pax header and the catalog row.
- **Non-blocking:** completed the truncated §5.2 manifest-LOCATE
  sentence; renamed `RemTarError` `at_lba` fields to `at_body_lba`
  (object reads are object-local, not global-LBA).

**Changes from v0.8.1 (alignment with settled Layer 3c v0.4.1).**
No body-format change; the previously-"blocking" 3c interface is
now resolved, and this revision pins to it:

- **§16.14 rewritten** from "3c follow-ups BLOCKING for impl" to
  "Layer 3c interface RESOLVED in 3c v0.4.2." Every one of the
  former open items (three address spaces, per-object BodyLba,
  filemark-aware epochs, sidecar tape files, filemark map,
  forced-erasure recovery, block-size geometry, parity spool,
  sidecar redundancy) now points at where 3c v0.4.1 fixes it.
  Notably: the sidecar "raw vs pax-wrapped — pick one" question
  is decided (**raw**, 3c §5.5); the parity-spool model is
  incremental accumulation (3c §7.4); and sidecar non-replication
  is an accepted v1 simplification backed by the three-copy
  policy.
- **§16.15** bootstrap reference tightened to 3c's replicated
  bootstrap *tape file* at BOT with a filemark-map digest.
- The **final-block-flush boundary** (§9.1.1) is confirmed to
  match 3c v0.4.4 §6.1: `BodyBlockWriter::finish_after_tar_eof()`
  owns the zero-filled final block; `ParitySink::finish_object()`
  writes only the filemark and sidecars and asserts the body
  flushed a whole final block.

**Changes from v0.8 (v0.8.1 consistency + implementation-readiness pass).**
A review found stale contradictions and under-specified
contracts that could let an implementation recreate old bugs.
Fixes:

- **§5.4 contradictions removed.** The layout no longer says
  "NO block-level zero-fill" (it contradicted §5.2's correct
  final-block fill) or "3c inserts parity/bootstrap invisibly"
  (the v0.6 inline model, gone since v0.7). §5.4 now states the
  object-local-BodyLba, no-parity-inside-object, final-block-
  zero-fill, 3c-writes-filemark facts.
- **§10.5 manifest direct-read** uses catalog `manifest_size_bytes`
  / `manifest_chunk_count` (the pax header sits behind a direct
  seek and can't be used for sizing).
- **FileEntry `first_chunk_lba` is now optional** (`?uint`),
  absent iff `chunk_count == 0`, with the empty-file invariant
  stated (matches the nullable 3b column).
- **Final-block flush has one owner.** `BodyBlockWriter::
  finish_after_tar_eof()` (rem-tar) flushes the zero-filled
  final block; `ParitySink::finish_object()` (3c) asserts the
  buffer is empty, then writes the filemark and sidecars. No
  layer leak, no double flush.
- **Compression remnants removed:** `Capabilities::COMPRESSED`
  flag and `WriteParams.compression` deleted; impl step 12.8
  no longer implies a circular manifest self-hash.
- **`REMANENCE.pad` redescribed** as an inert keyword inside a
  real pax header (never a standalone member), in §7.1 and
  Appendix B; `REMANENCE.metadata_preservation` added to the
  §7.1 and Appendix B keyword tables.
- **Normative pax-padding algorithm (§7.6)** added — iterate-to-
  fixed-point, handling the self-referential pax `<len>` field
  across decimal-digit boundaries (impl step 12.6.1).
- **Smaller:** `SymlinkEntry.mtime` is now `?uint` (follows
  preservation mode); the terminology table attributes the
  filemark to 3c's `finish_object()`, not the body format.

**Changes from v0.7 (compression removed; review fixes).**

- **Format-level compression removed (§6.2, §6.3 deleted,
  §7.x, §8.1, §10).** rem-tar-v1 no longer compresses chunks.
  Compression is reframed as an **orchestrator-level
  function**, not a tape-format function. Rationale: the
  workload is video (already-compressed codecs), where
  format-level zstd buys little and sometimes expands; and
  compression was responsible for a disproportionate share of
  the format's wire-level complexity (the logical-vs-stored
  size split, per-chunk length framing, expansion fallback,
  the "standard tar only byte-correct for uncompressed"
  caveat). Removing it keeps the tape-writing mechanism simple
  and makes the 30-year fallback unconditionally true: every
  object is a clean pax tar archive whose files extract
  byte-exact with standard `tar`. An orchestrator that has
  genuinely compressible data compresses it into `.zst` files
  *before* handing them to rem-tar-v1, which stores those
  bytes faithfully like any other file. The `compression`
  field is retained as a reserved enum that only takes `none`
  in v1, so a future v1.x could reintroduce format-level
  compression without a format-version bump if real evidence
  ever justifies it.
- **(review #2) Fixed-block final zero-fill after tar EOF
  (§5.2).** On fixed-block LTO the writer must emit whole
  blocks. After the two tar EOF records, the writer zero-fills
  the remainder of the final `chunk_size` block. This is
  tar-safe (it's after the archive EOF, where standard tar
  already stops) and the padded block is parity-protected like
  any other. v0.7 over-corrected by forbidding this.
- **(review #3) `BodyBlockWriter` abstraction (§9.1).** The
  writer buffers the tar byte stream into `chunk_size` blocks,
  writes full blocks to the BlockSink, and flushes the final
  partial block (zero-filled) only after the object's tar EOF.
  This prevents illegal short fixed-block writes and prevents
  accidental zero padding between entries.
- **(review #4) Manifest direct-read addressing (§8, §10.5,
  3b).** The object catalog row gains `manifest_size_bytes`
  and `manifest_chunk_count` so a reader seeking directly to
  `manifest_first_chunk_lba` knows how much to read without
  the pax header (which is behind the data LBA).
- **(review #5) Forward-scan uses tar-header size (§10.7).**
  Catalog-corrupt forward scan advances by
  `tar_header.size + 512-byte tar padding`, not
  `chunk_count * chunk_size` (which was wrong now that the
  final chunk isn't padded inside the payload).
- **(review consistency) Stale v0.6 parity-insertion language
  removed (§2.1, §5.x, §7.3, Appendix A).** Under the epoch
  model parity is never inserted inside object archives;
  phrases like "inserts parity invisibly," "skips interleaved
  parity blocks," and Appendix A's "No in-body filemark" are
  corrected.
- **(review) Tar blocking-factor caveat (§12.1).** Standard
  `tar` extraction of a 256 KiB-block tape needs `-b 512`
  (512 × 512 B = 256 KiB).

**v0.7 (retained): filemark-aware parity-epoch model.** Each
object is a clean pax tar tape file terminated by a filemark;
parity epochs span objects via `ParityDataOrdinal`; parity is
written as sidecar tape files; catalog addresses objects by
`(tape_file_number, BodyLba)`. See the v0.7 block below.

**Changes from v0.6 (filemark/parity-epoch redesign).** A
follow-up review showed that v0.6's "no filemarks inside the
parity body" choice (Option 1) sacrificed an operationally
valuable property — per-archive tape marks, the initial spec's
"sequence of tar archives separated by filemarks" model — to
make parity geometry simpler. That optimized the wrong layer.
v0.7 adopts the **filemark-aware parity-epoch model** instead:

- **Per-object filemarks restored (§5.1 rewritten).** Each
  object is a clean, independently-readable pax tar tape file,
  terminated by a tape filemark. Standard `mt`/`tar` tape
  navigation works again. This reverses v0.6's Option 1.
- **Filemarks do NOT flush parity.** Parity neighborhoods
  ("epochs") span across object filemarks. A filemark is a
  physical separator, not a parity boundary and not an RS
  shard. This avoids the "batch archives to fill a
  neighborhood" clunkiness that flush-at-filemark would cause
  — small and huge objects coexist without staging.
- **Parity written as sidecar tape files (§5.1, §16.14).**
  Completed parity epochs are written as their own tape files
  at object boundaries, never injected mid-archive. So each
  object archive stays a clean pax tar stream and parity
  occupies separate, clearly-delimited tape files.
- **Three address spaces (§2.1 rewritten).** `BodyLba` is now
  **per-object** (resets to 0 at each archive; paired with a
  `tape_file_number`). `ParityDataOrdinal` (3c-internal)
  counts protected data records across objects, skipping
  filemarks. `PhysicalLba`/`TapePosition` is the physical tape
  address. rem-tar-v1 operates in per-object BodyLba within an
  archive; 3c owns the ordinal↔position mapping.
- **Catalog records `(tape_file_number, first_chunk_lba)`
  (§10.1, 3b follow-up).** Object addressing is per-object:
  seek to tape file N, then BodyLba within that object. This
  makes the self-contained-archive property real — a
  perturbation from parity sidecars between objects can't
  shift an object's internal block numbering.
- **3c epoch model is a substantial 3c redesign (§16.14
  expanded).** ParityDataOrdinal, stripe-peer location across
  filemarks, sidecar placement and redundancy, the filemark
  map, and the large-archive parity-spool (deferred details)
  are 3c-side requirements, tracked here and to be specified
  in the 3c design revision.

**Changes from v0.5.2 (design-review response):** see the
v0.6 block below for the address-space, tar-padding,
compression, special-file, ustar-sanitization, spooling,
CBOR-determinism, and cross-doc-drift changes; all retained.

**v0.6 changes (retained), originally a detailed-review
response that fixed three core correctness problems:**

- **(BLOCKING) Two address spaces: BodyLba vs PhysicalLba
  (§2.1 new, §5.2, §7.3).** The body format previously assumed
  `chunk_lba = first_chunk_lba + N` over physical tape LBAs,
  which conflicts with 3c interleaving parity blocks into the
  physical stream. Resolved by defining `BodyLba` (the logical,
  parity-hidden block stream body formats see) and
  `PhysicalLba` (actual tape blocks). `BlockSink`/`BlockSource`
  operate in `BodyLba`; 3c translates and hides parity. All
  rem-tar-v1 LBA arithmetic is in `BodyLba`. This is a 3c
  interface requirement, tracked as a 3c follow-up.
- **(BLOCKING) Tar padding is now tar-safe (§5.2 rewritten).**
  The format no longer appends zero padding after file data or
  inflates tar header sizes. File-data *start* alignment to a
  BodyLba boundary is achieved by sizing the real pax extended
  header that precedes the file (with a `REMANENCE.pad` key in
  that header's body, not as a standalone entry). The last
  chunk is NOT zero-padded within the tar payload; the reader
  trims by file size. Standard pax tools extract byte-correct
  files.
- **(BLOCKING) No standalone padding pax members (§5.2,
  §5.4).** Alignment padding lives inside the pax extended
  header attached to the next real entry, never as an
  independent typeflag-`x` member.
- **(BLOCKING) Filemark/parity rule chosen (§5.1, cross-ref
  3c).** Option 1 adopted: no physical filemarks inside the
  parity-protected body. Object boundaries are tar EOF +
  catalog metadata. A single filemark may terminate the whole
  tape's data area, outside the parity stream. This keeps
  parity neighborhoods clean.
- **(BLOCKING) Block-size / parity geometry reconciled (§6.1,
  cross-ref 3c).** At the 256 KiB default, 3c's default
  geometry must use S=512 (not S=128) to retain ~512 MiB
  contiguous-loss tolerance, OR accept ~128 MiB tolerance.
  Documented; the 3c default table is block-size-aware.
- **(BLOCKING) Parity unwrapping = logical body stream
  (§10.2, cross-ref 3c).** `ObjectParitySource` presents an
  object-local BodyLba stream with parity/bootstrap tape files
  hidden. This is what makes standard-tar extraction real.
- **CRC-failure forced recovery API (§13.2, cross-ref 3c).**
  Added a forced-recovery call (`recover_block_at(body_lba)`, on
  3c's object-scoped `ObjectParitySource` as of v0.4.1) for the
  clean-read-but-CRC-mismatch case, which 3c's error-triggered
  recovery didn't cover.
- **Compression honesty + rules (§6.2, §6.3 new).** Standard
  tar fallback is byte-correct only for uncompressed files,
  stated explicitly. zstd expansion handled: if any chunk
  would expand beyond chunk_size during the prepass, the whole
  file falls back to `compression=none`. Compressed chunks get
  an explicit length framing rather than relying on decoder
  tolerance of trailing zeros.
- **Special-file policy (§5.9 new).** Hard links →
  `HardlinkEntry`; device nodes / FIFOs / sockets → rejected
  by default, allowed only in a `system-backup` mode;
  directories → `DirectoryEntry` for metadata preservation.
- **USTAR header sanitization in non-Full modes (§5.10 new).**
  Defines exactly what uid/gid/mode go into the ustar header
  in Minimal/Archival so a root-run standard tar restore can't
  apply ownership the format claims not to preserve.
- **Large-file spooling guardrails (§9.2 expanded).** Free-
  space check, immutable-snapshot hashing option, change-
  detection between prehash and write, configurable spool
  location and cap.
- **CBOR determinism required (§8.1, §14).** Canonical CBOR:
  sorted keys, definite lengths, no encoder-dependent ordering.
  Required for the manifest_sha256 trust chain to be
  implementation-independent.
- **Cross-document drift noted (§16 new).** The 3c bootstrap-
  at-LBA-0 root-of-trust supersedes the initial spec's
  bootstrap-tar-archive-at-filemark-0 / MAM-index model.
  Recorded.

**Changes from v0.5.1:**

- **Metadata preservation is tiered and archival-focused by
  default.** The format no longer preserves full POSIX
  ownership and permission metadata by default. The default
  `MetadataPreservation::Archival` mode preserves what's
  meaningful for cross-system, cross-time restore (path,
  content, mtime, executable bit, xattrs) and deliberately
  omits uid/gid/uname/gname and non-executable mode bits.
  These don't transfer faithfully across systems or decades
  and create false fidelity expectations at restore time.
  Operators wanting backup-style preservation opt into
  `Full`; pure content preservation uses `Minimal`. (§7.2,
  §8.1, §9, §11 rewritten, §16 new question)
- **VERIFIABLE / METADATA_PRESERVING capability honesty.**
  The capability description now states clearly what is and
  isn't preserved at each level, rather than implying full
  POSIX fidelity that doesn't survive cross-system restore.
  (§11)

**Changes from v0.5:**

- **Symlink cycle immunity documented as architectural
  invariant.** The default `SymlinkPolicy` never follows
  symlinks during traversal; the writer cannot exponentially
  blow up an archive even when the source tree contains
  symlink cycles (e.g., FCP's `.fcpcache/` self-referential
  links that historically caused BRU to write hundreds of GB
  of content 20+ times before hitting a depth limit). The
  recursion-bomb failure mode is structurally impossible.
  (§5.8 expanded)
- **Input-list sanity validation.** Two new validation
  checks defend against orchestrator-side walker bugs that
  produce inflated input lists: (a) an absolute entry-count
  limit per object (default 10 million entries), and (b)
  detection of inodes appearing more than a threshold (default
  100) times in the input list when not declared as hard
  links. Catches walker cycle bugs as the orchestrator hands
  off to the writer, even if the walker itself didn't notice.
  (§9.0 expanded, §13.1 expanded)
- **Orchestrator walker requirement.** The spec now documents
  that filesystem walkers feeding the writer must be cycle-
  safe (track visited directory inodes; refuse re-entry).
  This is an orchestrator concern, but rem-tar-v1's
  documentation calls it out so it isn't forgotten. (§16
  new question)
- **Future `Dereference` policy noted.** A future
  `SymlinkPolicy::Dereference` would require explicit cycle
  detection during traversal. Recorded as a constraint for
  any future spec revision that adds this policy. (§16 new
  question)

**Changes from v0.4:**

- **Symlink handling rules (§5.8 new).** Editors' video
  projects routinely contain symlinks to external assets
  (shared audio libraries, master footage, network shares).
  The archive workflow (editor → staging system → archive)
  means external targets are typically unreachable at
  archive time. Symlinks are now classified by their target
  string (not by filesystem reachability) into Internal,
  External, and Internally-Broken cases. Only Internally-
  Broken — where the target *should* be in the archive but
  isn't — is rejected by default. External symlinks (target
  outside the archive root) pass validation and are recorded
  in a new `ExternalReference` manifest field for downstream
  visibility. (§5.8, §9.0, §8.1, §10.x)
- **Restore never fails on missing symlink targets.** The
  archive's job is byte-faithful reproduction of the symlink
  itself, not its target. Whether the target resolves on the
  restore system is a restore-time operator concern, not a
  format concern. Restore proceeds successfully whether or
  not symlink targets exist. The restore tool surfaces a
  pre-extraction summary of external references so operators
  know what dependencies the archive expects. (§10.x explicit)
- **ExternalReference manifest field.** Manifest gains a
  top-level list of external symlinks with their target
  strings and classification, enabling cross-archive
  dependency queries and operator visibility. (§8.1)
- **SymlinkPolicy enum.** WriteParams gains a policy choice:
  `Default` (reject internally-broken, accept external),
  `Strict` (reject any dangling symlink — self-contained
  archives), `Permissive` (accept everything — emergency
  archives). (§9)

**Changes from v0.3:**

- **Strict UTF-8 enforcement for all strings.** All strings
  in the format — pax `path`, all `REMANENCE.*` keywords,
  manifest fields, xattr keys/values — are UTF-8. The writer
  validates every path and string before any tape I/O and
  refuses to write if any string fails validation. The error
  contains the full list of failed paths with structured
  reasons so a fix-up tool can address all problems in one
  cycle. (§5.7 new, §9.0 new, §13 expanded)
- **No fallback for non-UTF-8 filenames.** The earlier idea
  of a `REMANENCE.path_bytes_hex` fallback keyword is
  rejected in favor of strict validation. The format never
  accepts ambiguous input; orchestrators that need lenient
  behavior must normalize before passing files to the writer.
- **Validation is stateless and parallel.** No caching of
  validation results — re-validation is fast (seconds even
  for tens of thousands of files) and parallelizable via
  `rayon`. The simpler design avoids cache-invalidation
  bugs that could silently produce stale results. (§9.0,
  §16 new question)
- **Catalog database charset requirement.** Documented
  explicitly: the catalog DB must use UTF-8 encoding (Postgres:
  `ENCODING 'UTF8'`; MySQL/MariaDB: `utf8mb4`). Layer 5 startup
  refuses to operate if the catalog is misconfigured. This is
  a 3b/Layer 5 concern but documented here for completeness.
  (§16 new question, 3b catalog follow-up updated)

**Changes from v0.2:**

- **Per-chunk integrity uses CRC-64 (XZ polynomial), not
  SHA-256.** At the per-chunk layer the actual threat is
  random bit errors and software bugs, not adversarial
  tampering. CRC-64 (false-negative rate ~5.4×10⁻²⁰) is the
  right tool for that threat and uses 8 bytes per chunk
  instead of 32. Tamper detection comes from the catalog →
  file_sha256 → manifest_sha256 chain of trust, not from
  per-chunk hash strength. (§7, §8, §10.6, §11)
- **External subclip catalog model.** Per-chunk LBAs are
  arithmetically derivable from `first_chunk_lba + N` under
  the chunk-alignment invariant, so the on-tape format no
  longer stores a per-chunk LBA table. Video-content offsets
  (timecode → byte offset, frame → byte offset, etc.) belong
  in an external application-layer catalog, not on tape.
  This simplifies the writer's pax header construction, slims
  the manifest, and makes the persistent/ephemeral distinction
  explicit. (§7.3 rewritten, §8.1 slimmed, §9 simplified, §16
  new question)
- **Manifest size reduced ~4×.** Combined effect of the two
  changes above. A 100 GiB file's per-file manifest entry
  drops from ~20 MiB (v0.2) to ~3 MiB (v0.3).

**Changes from v0.1:**

- Default chunk size is 256 KiB (was 1 MiB). Industry consensus
  for LTO performance sweet spot, matches the Archives team's
  existing LTO-6/7 operator practice, and is the value
  recommended by HPE's LTO-9 documentation. Format remains
  parameterized; chunk_size is per-object and any LTO-permitted
  size works. (§4, §6, §7.1, Appendix A)
- The 3b catalog schema is assumed to carry per-file LBA rows,
  not just per-object entries. This enables direct LOCATE to
  any file without first loading the manifest, removes the
  byte-range vs whole-file threshold question entirely, and
  makes multi-file restore batching natural at the orchestrator
  level. (§10.1, §10.3, §10.4; the schema change itself is a
  3b update tracked separately)
- Removed the byte-range threshold from the read API. Under
  the per-file LBA model, byte-range and whole-file reads have
  identical LOCATE cost, so the threshold has no operational
  effect. (§10.4 simplified)
- §6.2 expanded with explicit rationale for why we disable
  LTO-DC (drive-level compression) and use format-level zstd
  as the optional compression path. The capacity-determinism,
  byte-range, durability, and encryption-ordering arguments
  are now documented rather than implicit.

This document specifies the default Remanence body format,
`rem-tar-v1`. It is a `TapeFormat` implementation as defined by
the 3b trait surface, advertising capabilities Tier 0 + Tier 1
+ Tier 2 + VERIFIABLE + METADATA_PRESERVING.

The format is a constrained subset of POSIX pax tar with
Remanence-specific extensions in pax extended headers. The
constraints exist to satisfy 3c's parity layer (chunk-aligned
to tape block boundaries) and to enable efficient byte-range
access. The unconstrained tar stream remains extractable by
any pax-aware tar tool, with the chunk metadata simply ignored.

---

## 1. Scope

`rem-tar-v1` is the body format used by default for new
Remanence-written tapes. It addresses three requirements
simultaneously:

1. **Fast file-level restore.** A reader given a `FileId`
   should be able to LOCATE directly to the file's first chunk
   and read its data sequentially. No tape scanning required.
2. **Fast byte-range restore within files.** A reader given
   `(FileId, start_byte, end_byte)` should be able to LOCATE
   to the specific chunk containing `start_byte`, read only
   the chunks covering the requested range, and trim head/tail
   bytes. No reading beyond the range required.
3. **30-year readability with standard tools.** If Remanence
   ever disappears, the bytes on tape should remain
   recoverable using `tar` (or any pax-compatible tool) plus
   the parity layer's documented recovery procedure.

The format is also designed to be:

- **Simple to document.** A complete on-tape format reference
  fits in this single document. No supplementary RFCs, no
  CD-ROM-only SMPTE standards, no version proliferation.
- **Simple to implement.** A correct reader is a few hundred
  lines of Rust on top of an existing tar library; a correct
  writer is similarly compact.
- **Append-only.** No seek-back-and-rewrite. The writer
  emits tape blocks in order; the reader (in the happy path)
  needs only forward access plus targeted LOCATEs.
- **Self-describing.** Each object is a complete, standalone
  archive. Cross-object dependencies live only in the catalog,
  not in the on-tape data.

### Non-goals

- **Universal write compatibility.** A user cannot write
  arbitrary tar archives and expect them to become valid
  `rem-tar-v1` objects. The format adds chunk-alignment
  constraints standard tar tools don't impose.
- **No format-level compression.** `rem-tar-v1` writes
  uncompressed data; compression is an orchestrator-level
  function (§6.2). The `compression` field is reserved
  `none`-only in v1.
- **Random write access.** The format is append-only.
  Modifying a written tape is not supported.
- **Multiple objects per tar stream.** Each Remanence "object"
  is one complete tar stream (one tar archive) with its own
  EOF. Concatenating multiple objects on tape uses tape file
  marks between them, not tar concatenation.
- **Files smaller than 1 byte.** Empty files are supported
  via the empty-file convention (§5.5); files between 0 bytes
  and 1 byte aren't meaningful.

---

## 2. Position in the stack

```
Layer 5  (gRPC API)        ← caller: orchestrator, CLI
Layer 4  (local state)     ← catalog cache, audit log
Layer 3b (tape format)     ← TapeFormat trait
  ├─ rem-tar-v1            ← THIS DOC
  └─ (other formats)       ← future
Layer 3c (tape parity)     ← ParitySink / ObjectParitySource (transparent)
Layer 3a (tape mechanism)  ← DriveHandle: rewind/locate/read/write
Layer 2  (identity + ops)  ← LibraryHandle: discover, move, load
Layer 1  (SCSI core)       ← remanence-scsi: CDB builders + sg_io
```

`rem-tar-v1` is a body format. It writes blocks via a
`BlockSink` (which in production is wrapped by the parity
layer) and reads via a `BlockSource`. It does not interact
with parity or the SCSI layer directly.

Crate location: `crates/remanence-format/src/formats/rem_tar_v1.rs`,
or a sibling crate `crates/remanence-format-tar/` if the
implementation grows large. Tar parsing uses the existing
Rust `tar` crate (which handles ustar and pax correctly) plus
local code for the chunk-alignment and pax-extension logic.

### 2.1 Address spaces

There are three distinct block-address spaces in the stack.
`rem-tar-v1` operates in one of them (per-object `BodyLba`);
the other two are 3c's concern. Getting this separation right
is what lets per-object tape filemarks, clean pax archives,
and Reed-Solomon parity coexist.

**`PhysicalLba` / `TapePosition`** — the physical tape
address. In a per-object-filemark layout this is naturally
expressed as `(tape_file_number, block_within_file)`, or a
flat SCSI LOCATE address plus a filemark map. The physical
tape is a sequence of tape files separated by filemarks: object
archives and parity-epoch sidecar files (§5.1).

**`ParityDataOrdinal`** *(3c-internal)* — a logical sequence
numbering only the protected **data** records, in order,
across object archives, **skipping filemarks**. Object A's
blocks get ordinals 0..a, the filemark after A gets no
ordinal, object B's blocks continue at a+1, and so on. Reed-
Solomon neighborhoods ("parity epochs") are defined over
`ParityDataOrdinal`, so a stripe can span the filemark between
two objects. Filemarks are physical separators, never RS
shards. rem-tar-v1 never sees this space; it exists so 3c can
protect data uniformly without making filemarks parity
boundaries.

**`BodyLba`** *(per-object)* — the logical block stream
*within a single object archive*. `BodyLba` 0 is the first
block of that object's pax tar stream; it counts only that
object's blocks (headers, file data, manifest), resetting to 0
at each new object. It is paired with the object's
`tape_file_number` to form a complete address. Because each
object is its own clean pax tar tape file, an object's BodyLba
numbering is self-contained: parity sidecars or other objects
elsewhere on the tape cannot perturb it.

How the spaces relate:

```
TapePosition:      [obj A file][FM][obj B file][FM][parity sidecar][FM][obj C file][FM]...
ParityDataOrdinal:  0 1 2 .. a       a+1 .. b      (sidecar: not data)    b+1 ..
BodyLba (per obj):  0 1 2 .. a       0 1 .. (b-a)                          0 1 ..
```

**rem-tar-v1 stores and computes only per-object `BodyLba`,
paired with `tape_file_number`.** The catalog records
`(tape_file_number, first_chunk_lba)` where `first_chunk_lba`
is a BodyLba within that object (§10.1). The arithmetic
`chunk_body_lba = first_chunk_lba + chunk_index` (§7.3) is
valid because, within one object's clean pax tar stream,
BodyLba is contiguous with no gaps — there are no parity
blocks injected *inside* an object archive (parity lives in
separate sidecar tape files, §5.1).

What 3c provides to rem-tar-v1:

- A `BlockSink` that, given an object's blocks in BodyLba
  order, writes them as one contiguous pax tar tape file,
  then writes a filemark at object close. 3c separately
  accumulates these blocks into parity epochs (by
  ParityDataOrdinal) and emits parity sidecar tape files at
  object boundaries — invisibly to rem-tar-v1.
- A `BlockSource` that, given `(tape_file_number, body_lba)`,
  positions to that tape file and block and returns object
  data. Within an object there are no parity blocks to skip,
  so the returned stream is directly a valid pax tar archive.
- `recover_block_at` (§13.2) for parity recovery, which 3c
  resolves via ParityDataOrdinal → stripe peers → parity
  sidecars.

For audit/recovery, 3c can expose the full
`(tape_file_number, body_lba) → ParityDataOrdinal →
TapePosition` mapping, but rem-tar-v1's read/write path only
needs `(tape_file_number, body_lba)`.

**This is a substantial Layer 3c design change**, tracked in
§16.14: 3c moves from a "uniform physical-LBA interleaver" to
a "filemark-aware parity-epoch with sidecars" model. The
change is confined to 3c; rem-tar-v1's contract is just the
per-object BodyLba interface above.

---

## 3. Prior art summary

This format is informed by but distinct from:

**POSIX pax (IEEE Std 1003.1-2001) / ustar.** The tar
interchange format. `rem-tar-v1` is a constrained subset:
every entry is pax-format (typeflag `x` for extended header
plus the standard ustar header), file data is chunk-aligned,
and Remanence-specific keywords live in pax extended headers.
Standard pax tools extract `rem-tar-v1` objects correctly,
losing only the chunk metadata.

**Bacula BB02 volume format.** Bacula tapes have explicit
block headers and periodic "index records" inserted every
100 MB of data to speed up single-item restore. The chunk
alignment in `rem-tar-v1` borrows this idea but generalises
it: every chunk is independently addressable, so seek-and-read
works at chunk granularity, not just at 100-MB granularity.

**LTFS (ISO/IEC 20919).** Rejected as the primary format.
`rem-tar-v1` does not use tape partitioning, does not maintain
a mutable on-tape index (the per-object manifest is written
once at object close), and does not pretend tape is random-
access at the OS level.

**MXF (SMPTE ST 377).** Cautionary tale, not a precedent.
The proliferation of MXF operational patterns and vendor
incompatibilities is exactly what `rem-tar-v1` avoids by
constraining itself to a single, fully-specified layout.

**`rem-chunked-v1` (proposed in `docs/layer3b-design.md` §7).**
A purely Remanence-native format with CBOR headers and opaque
chunks. `rem-tar-v1` supersedes the proposed `pax-tar-v1`
sketch (3b §8) and is intended to be the default format,
displacing the `rem-chunked-v1` proposal. The 3b trait
surface and capability semantics are unchanged; only the
on-tape wire format differs.

The decision to make `rem-tar-v1` the default rather than a
fully-custom format is driven by the 30-year readability
priority. A tape that can be read with `tar` thirty years
from now is genuinely more durable than one that requires a
Remanence-aware reader to interpret.

---

## 4. Terminology

| Term | Definition |
|--|--|
| **Object** | One Remanence-archive unit. The orchestrator's notion of a single archival item — typically a folder of related files, sometimes a single large file. One object = one tar archive on tape. |
| **File** | A regular file inside an object. Stored as one pax tar entry. The orchestrator-visible content. |
| **Tape block** | One LTO physical block, sized per Layer 3a's tape configuration. Default 256 KiB. Always exactly aligned to this size by 3a. |
| **Chunk** | A slice of a file's data the size of one tape block, occupying exactly one tape block. A file of size `n` bytes has `ceil(n / chunk_size)` chunks. The last chunk may be shorter than chunk_size. |
| **chunk_size** | The Remanence-configured chunk size. Default 262144 (256 KiB). Recorded per-object in the manifest. See §6.1 for sizing rationale. |
| **Pax extended header** | A POSIX-standardized typeflag-`x` tar entry preceding a regular entry, carrying `keyword=value` pairs that augment the regular entry's metadata. |
| **REMANENCE.* keyword** | A pax extended header keyword in the `REMANENCE.` namespace. Carries `rem-tar-v1`-specific per-file metadata. |
| **Object manifest** | A regular file written at the end of each object, named `_remanence/manifest.cbor`, containing the complete chunk and file index for the object. |
| **Tar EOF** | The two consecutive 512-byte zero records that terminate a tar archive per POSIX. |
| **File mark** | A SCSI tape filemark separating tape files (objects and parity sidecars) on tape. Written by the filemark-aware 3c sink in `finish_object()` after the body format's tar EOF and final-block flush (§9.1.1, §16.14); recognized by Layer 3a. |

---

## 5. On-tape layout

### 5.1 Object framing

One Remanence object is one complete pax tar archive occupying
one tape file, terminated by a tape filemark. The physical
tape is a sequence of tape files separated by filemarks:
object archives interspersed with parity-epoch sidecar files
(emitted by 3c at object boundaries). The catalog records each
object's `tape_file_number` and, within that object,
`first_chunk_lba` per file and `manifest_first_chunk_lba` (all
per-object `BodyLba` values — §2.1).

```
Physical tape (filemarks shown as | ):
| object 0 (pax tar) | object 1 (pax tar) | parity sidecar | object 2 (pax tar) | ... | EOD
```

Each object archive begins with a global pax header (typeflag
`g`) and ends with the standard tar EOF (two zero records).
Between these markers, the archive contains file entries,
symlink entries, and the object manifest. Within the object,
addressing is per-object `BodyLba` starting at 0.

#### Filemark policy

**Per-object filemarks: yes. Filemarks do not flush parity.**
This reverses v0.6's "Option 1" (no filemarks in the parity
body), which had sacrificed standard tape navigation to make
parity geometry uniform — the wrong layer to optimize.

The model:

- **Each object archive is its own tape file**, terminated by
  a filemark. A reader can `mt fsf N` to seek to tape file N
  and find a complete, independently-readable pax tar archive.
  This matches the initial Remanence spec's "sequence of tar
  archives separated by filemarks" and decades of tape
  convention.
- **Filemarks are physical separators, not parity boundaries.**
  Reed-Solomon parity neighborhoods ("epochs") are defined
  over `ParityDataOrdinal` (§2.1), which counts data records
  across objects and skips filemarks. A parity stripe can
  therefore span the filemark between two objects. Filemarks
  are never RS shards.
- **Parity is written as sidecar tape files, never injected
  inside an object archive.** This is what keeps each object a
  clean pax tar stream: 3c does not interleave parity blocks
  mid-archive. Instead, as parity epochs complete, 3c emits
  the parity payload as a separate tape file (a "parity epoch
  sidecar") at an object boundary. See §16.14 for the 3c-side
  mechanics.

This avoids the two bad outcomes the design review identified:

1. *Flush-at-filemark* (making every object boundary a parity
   boundary) would waste padding on small objects and force
   the orchestrator to batch objects to fill neighborhoods —
   pushing erasure-code geometry up into the orchestrator,
   the wrong layer.
2. *No filemarks at all* (v0.6 Option 1) loses standard tape
   navigation and the independently-readable-archive property.

The epoch-sidecar model gets both: clean per-object pax
archives with standard `mt`/`tar` navigation, AND parity that
accumulates freely across objects of any size without staging.

Object lookup uses the catalog's `(tape_file_number,
first_chunk_lba)`: the reader positions to tape file N (via
3c, which knows the filemark map), then to the BodyLba within
that object. Filemark-based sequential scanning also works as
a fallback for bare-tape forensic tools, since each object is
a real filemark-delimited tar file.

### 5.2 Block alignment and tar safety

`rem-tar-v1` aligns the **start** of each file's data to a
per-object `BodyLba` block boundary (`chunk_size`, default
256 KiB). This is the one structural constraint beyond plain
tar, and it is implemented in a way that keeps the byte stream
a fully valid pax tar archive.

The critical correctness rule, from the design review:
**tar validity is non-negotiable.** Earlier drafts appended
zero padding after file data to fill the last tape block, or
inflated tar header sizes to include padding. Both break
standard tar extraction — trailing zero blocks can be read as
tar EOF, and inflated sizes make standard tools extract files
with extra trailing zero bytes. Neither is done in v0.6+.

How alignment actually works:

- **File-data start alignment.** To make a file's data begin
  at a `BodyLba` boundary, the writer sizes the **pax extended
  header that immediately precedes that file** so that the
  header + the file's ustar header together end exactly on a
  block boundary. Pax extended headers have arbitrary length
  (a length-prefixed sequence of `keyword=value` records), so
  the writer adds a single `REMANENCE.pad=<spaces>` record to
  the *real* pax header of the next file, padding it to the
  needed size. This is a legitimate pax header for the
  following entry, not a standalone padding member (§5.4).
- **File-data end: NO padding.** The file's data is written
  with its exact byte length in the ustar header, followed
  only by tar's normal padding to the next 512-byte record
  boundary (standard pax behavior). The last `BodyLba` block
  of a file may therefore contain the file's tail bytes plus
  the start of the next tar structure (the next file's pax
  header, or the tar EOF). The reader trims to the exact file
  size from the catalog/manifest; it never relies on
  block-level zero padding to find the file's end.
- **chunk_size and BodyLba blocks.** Because file data starts
  on a block boundary and chunks are `chunk_size` bytes, chunk
  `N` of a file begins at `BodyLba` `first_chunk_lba + N`. The
  last chunk is a *logical* chunk that may be shorter than
  `chunk_size` (the file's tail); it is not physically padded.
  Byte-range math (§7.3) accounts for the true file size.

The consequence: a standard pax tar tool reading the object's
tape file (which is a clean pax tar stream — no parity blocks
inside it, §5.1) sees a completely ordinary tar archive. Files
extract with exact byte length. The `REMANENCE.pad` records in
pax headers are ignored by standard tools (unknown vendor
keywords are skipped per POSIX). No standalone padding members,
no trailing zero blocks masquerading as EOF.

The remaining alignment points:

- **Object start.** The object's first tar structure (global
  pax header) begins at a `BodyLba` boundary (it's the first
  block of the object's BodyLba range).
- **Manifest start.** The manifest file's data begins at a
  `BodyLba` boundary, achieved the same way (sizing the
  manifest's preceding pax header). This lets the reader
  LOCATE directly to the manifest via its recorded
  `manifest_first_chunk_lba` (the per-object `BodyLba` in the
  catalog row) without scanning the whole archive — the
  verification/reconciliation read of §10.5.
- **Tar EOF.** The two zero records terminating the archive
  follow the manifest's data with normal 512-byte padding.
- **Final block zero-fill (fixed-block media).** On fixed-block
  LTO the writer must emit whole `chunk_size` blocks. After the
  tar EOF records, the writer zero-fills the remainder of the
  final block. This is **tar-safe**: it occurs *after* the
  archive EOF, where standard tar already stops reading, so the
  trailing zeros are never interpreted as data or as a spurious
  EOF mid-archive. The zero-filled final block is a normal
  object block, parity-protected like any other. (This is the
  *only* block-level zero-fill in the format. There is never
  zero-fill between entries or within a file's payload — that
  was the v0.6 tar-safety bug; v0.7 over-corrected by forbidding
  even this post-EOF fill, which fixed-block media requires.)

This alignment is what makes Tier 2 (byte-range access)
efficient: under the per-file LBA catalog model (§10.1), a
reader looking for byte offset N in file F computes
`first_chunk_lba + (N / chunk_size)` (in BodyLba) directly
from the catalog row, positions there via 3c, and reads exactly
the chunk(s) covering the range. No manifest read, no tape
scanning, no buffered reads.

### 5.3 Tar entry types used

| Typeflag | Use |
|--|--|
| `g` | Global pax header. Written once at object start; carries object-level keywords. |
| `x` | Extended pax header. Written before every regular file and symlink entry; carries per-entry keywords. May carry a `REMANENCE.pad` record to size the header for block alignment of the *following* file. Never used as a standalone padding member. |
| `0` (or NUL) | Regular file. Used for both archived files and the object manifest. |
| `1` | Hard link. Used for hard-linked files; see §5.9. |
| `2` | Symbolic link. Used for symlinks; see §5.8. |
| `5` | Directory. Used to preserve directory metadata; see §5.9. |
| `L` | GNU long-name extension. **Not used.** All long names are carried via pax `path` keyword instead. |
| `S` | Solaris sparse file. **Not used.** Sparse files are stored fully if needed; sparse-file optimization is a future format extension. |
| `3`/`4` (char/block device), `6` (FIFO) | **Rejected by default.** Allowed only in `system-backup` mode; see §5.9. |

### 5.4 Complete object layout

All positions below are **object-local `BodyLba`** values
(§2.1): this object's first block is BodyLba 0, paired with the
object's `tape_file_number` for a complete address. Within this
object tape file there are **no parity or bootstrap blocks** —
it is a clean pax tar stream. 3c writes the terminating filemark
at object close and may later write parity-epoch sidecar tape
files and bootstrap tape files *outside* this object (§5.1,
§16.14). "Block" means one `chunk_size` BodyLba block.

The structural invariant is: the tar byte sequence *before each
file's data* must end on a block boundary, so the file's data
starts block-aligned. This is achieved by sizing the
`REMANENCE.pad` record inside that file's real pax extended
header (§5.2, §7.5). The diagram shows where data lands, not a
claim about any single header's size in isolation.

```
[block 0 onward: global header + file 0's pax/ustar headers]
  Global pax header (typeflag 'g'):
    REMANENCE.format_id=rem-tar-v1
    REMANENCE.schema_version=1.0
    REMANENCE.object_id=<uuid>
    REMANENCE.caller_object_id=<string>
    REMANENCE.chunk_size=262144
    REMANENCE.metadata_preservation=<archival|full|minimal>
    REMANENCE.encryption=<flag>
    REMANENCE.write_timestamp=<RFC3339>
  Followed by file 0's pax extended header, whose REMANENCE.pad
  record is sized so that the whole preceding sequence (global
  header + file 0's pax header + file 0's ustar header) ends
  exactly on a block boundary — so file 0's DATA starts at the
  next block boundary. (No standalone padding member; no
  zero-fill between entries.)

[BodyLba 1..K: file 0 data]
  File 0 pax extended header (typeflag 'x'):
    path=<long path if needed>
    size=<exact size in bytes>
    mtime=<...>            (archival/full only)
    REMANENCE.file_id=<uuid>
    REMANENCE.file_sha256=<hex>
    REMANENCE.chunk_count=<n>
    REMANENCE.executable=<bool>   (archival/full)
    REMANENCE.compression=none
    REMANENCE.pad=<spaces>        (sizes this header so the
                                   FILE DATA below starts on a
                                   block boundary)
  + File 0 ustar header (typeflag '0'), ustar fields sanitized
    per §5.10
  + File 0 data: exact `size` bytes, chunk_count logical
    chunks. Data START is block-aligned (BodyLba 1). The final
    logical chunk is the file's tail and is NOT zero-padded to
    chunk_size — it's followed by tar's normal 512-byte-record
    padding, then file 1's pax header begins.

[file 1's pax header begins immediately after file 0's
 512-byte-record padding; it carries a REMANENCE.pad record
 sized so file 1's DATA starts on the next block boundary]
  ...

(repeat for all files, then all symlink/hardlink/dir entries)

[manifest: pax header sized so manifest DATA is block-aligned]
  Object manifest pax extended header (typeflag 'x'):
    path=_remanence/manifest.cbor
    size=<exact manifest size>
    REMANENCE.file_id=<manifest uuid>
    REMANENCE.is_manifest=true
    REMANENCE.file_sha256=<hex of manifest content>
    REMANENCE.chunk_count=<m>
    REMANENCE.pad=<spaces>
  + manifest ustar header (typeflag '0')
  + manifest data: canonical CBOR (§8). Final chunk trimmed by
    size, not zero-padded.

[tar EOF]
  Two consecutive 512-byte zero records (POSIX tar EOF),
  followed by tar's normal 512-byte-record padding.

[final-block zero-fill]
  On fixed-block media the writer must emit whole chunk_size
  blocks. After the tar EOF records, BodyBlockWriter zero-fills
  the remainder of the final block (§5.2, §9.1.1). This is the
  ONLY block-level zero-fill in the format; it is tar-safe
  because it follows the EOF, where standard tar already stops.
  The zero-filled final block is a normal object block.

[filemark]
  Written by 3c at object close (finish_object, §16.14), not by
  the body format. The next tape file is either another object
  archive, a parity-epoch sidecar, or a bootstrap tape file —
  all separate from this object.
```

The BodyLba values above are per-object: this object's first
block is BodyLba 0, and they're paired with the object's
tape_file_number for a complete address (§2.1).

Note how every file's *data* starts on a block boundary (so
byte-range LOCATE works), but no file's data is zero-padded at
the *end* (so the stream stays a byte-correct tar archive). The
alignment "slack" is absorbed inside the next entry's real pax
header via `REMANENCE.pad`, never as standalone padding or
trailing zeros.

### 5.5 Empty files

A file with size 0 has chunk_count = 0 and no chunk data. The
pax extended header records `size=0` and
`REMANENCE.chunk_count=0`. The regular tar entry follows
immediately, with zero data bytes. The next file's header
lives in the same tape block (no chunk alignment is needed
because there's no data to align).

### 5.6 Directories

Directory entries (typeflag `5`) have no file data. They carry
their pax extended header (with permissions, mtime, xattrs)
and ustar header but no chunks. The next entry follows
immediately without chunk_size alignment.

### 5.7 String encoding

All strings in `rem-tar-v1` are **UTF-8 encoded**. This is a
strict, format-wide requirement covering:

- POSIX pax `path` keyword (POSIX requires UTF-8 anyway).
- All `REMANENCE.*` keyword names and values in pax headers.
- All text strings in the manifest (paths, UUIDs as strings,
  timestamps, compression, encryption flags, xattr keys and
  values where stringly-typed).
- All field tags in the manifest CBOR (CBOR major type 3
  requires UTF-8).

#### Why strict UTF-8

POSIX pax already requires UTF-8 for the standard `path`,
`uname`, `gname`, and similar keywords. CBOR text strings
(major type 3) require UTF-8 per RFC 8949 §3.1. JSON requires
UTF-8 per RFC 8259 §8.1. The format aligns with all three.

The alternative — allowing arbitrary byte sequences in
filename fields — would either require:
- Switching all string fields to binary byte strings (CBOR
  major type 2 or base64-in-JSON), losing readability and
  POSIX pax compatibility, or
- A dual-encoding fallback (e.g., `REMANENCE.path_bytes_hex`
  carrying raw bytes alongside a "best-effort UTF-8" `path`),
  doubling format complexity and creating ambiguity about
  which form is canonical.

Both alternatives have hidden costs that compound over the
archive's lifetime. The strict-UTF-8 rule is simple, aligns
with all standards involved, and avoids creating ambiguous
or two-track filename handling.

#### Failure mode this prevents

A real failure mode from production tape archives (observed
on BRU/TOLIS deployments at archive and elsewhere): filenames
with non-ASCII characters are stored on tape in some encoding
(often Latin-1, Shift-JIS, or raw bytes from older
filesystems). When ingested into a catalog database with a
narrower charset, the database silently transliterates or
strips characters. The catalog's stored filename no longer
matches the on-tape filename. Subsequent restore requests
fail because the lookup key doesn't match.

Strict UTF-8 enforcement at write time, combined with
UTF-8-required catalog databases (§16.x), guarantees the
catalog and tape always agree on the byte-for-byte filename.
There is no encoding boundary that can silently mangle data.

#### Source filenames that aren't valid UTF-8

The orchestrator may attempt to archive files whose
filesystem-level names aren't valid UTF-8 (legacy filesystems,
files dragged from non-Linux sources, mount with wrong codepage).
The writer **refuses** such files at pre-write validation
(§9.0). The list of failing files is returned to the
orchestrator so the operator can fix the source filenames
before retrying.

The writer never silently transliterates, replaces, or
truncates filename characters. The choice of how to handle
broken source filenames belongs to the orchestrator and the
operator — not the format.

### 5.8 Symlinks

Video project archives frequently contain symbolic links to
external assets — shared audio libraries, master footage,
proxies, fonts, LUTs, or sibling project folders. The typical
workflow is:

1. Editor builds the project on a workstation where all
   external dependencies are mounted and reachable.
2. The project folder (with its symlinks) is copied to a
   staging system for analysis and archive submission.
3. The staging system does not have the same external mounts;
   most symlink targets are no longer reachable on the
   staging filesystem.
4. Remanence is asked to archive the staging copy.

The format must handle this workflow gracefully — meaning
external symlinks pass validation even when their targets are
unreachable at archive time.

#### Classification

Each symlink encountered during pre-write validation is
classified by **textual analysis of its target path**
(not by `stat()`-ing the target):

| Classification | Definition | Default action |
|---|---|---|
| **Internal** | Target resolves textually to a path within the archive root, AND that path is in the file list. | Archive normally. |
| **External-absolute** | Target is an absolute path (starts with `/`) outside the archive root. | Archive verbatim; record in `ExternalReferences` (§8.1). |
| **External-relative** | Target is a relative path that, when resolved against the symlink's directory, escapes the archive root (e.g., `../../shared/x.wav`). | Same as External-absolute. |
| **Internally-broken** | Target resolves textually to a path within the archive root, but that path is NOT in the file list. | **Reject** by default. |

The classification is **purely textual**. The validator does
not need to `stat()` the target. This is essential because
on the staging system most external targets won't exist as
files anyway.

The "would-this-path-be-inside-the-archive-root" test is
performed by:
1. If target is absolute (starts with `/`): check if it's a
   prefix-match of the archive root path.
2. If target is relative: lexically resolve it against the
   symlink's parent directory (no filesystem calls — just
   string manipulation, handling `..` and `.` components),
   then check if the result is under the archive root.

#### Why Internally-broken is rejected by default

This catches the genuine archive-integrity bug: an rsync that
didn't finish, an editor who deleted a file the project still
references, or an incomplete staging copy. The symlink's
intent — "this archive includes both me and my target" — is
violated. The operator should know.

External symlinks (the editor → staging → archive workflow)
don't carry this intent. The editor knows the target is
elsewhere; the archive is preserving the breadcrumb, not the
target.

#### Symlinks on tape

Symlinks are written using standard POSIX tar typeflag `2`
(symbolic link), with the `linkname` field carrying the target
string verbatim. No deviation from standard pax. Any pax-aware
tar tool can extract them correctly — the link will simply
point to whatever string was archived, dangling or not.

The symlink itself is **not chunk-aligned data**: it's a tar
header entry with no data section. No tape blocks are consumed
for symlink content. The next file entry follows immediately.

Symlinks are not counted in `chunk_count`, do not have CRC
codes, and do not get `first_chunk_lba` assignment — they're
metadata-only entries. The catalog records them with a
distinct row type (see 3b catalog schema follow-up).

#### Restore semantics

**Restore never fails because a symlink's target is missing.**
The format's promise is byte-faithful reproduction of the
symlink itself (path + target string), not of what the target
resolves to. The restored symlink may dangle on the target
system — that is the restore-time operator's concern, not
the format's.

The restore tool MUST:

- Re-create symlinks with their original target strings exactly.
- NOT attempt to resolve targets, follow them, or warn
  individually about each one during extraction.
- Surface external references upfront (before extraction
  begins) so the operator knows what dependencies to expect.

The restore tool MUST NOT:

- Fail the extraction because a target doesn't exist.
- Silently rewrite target paths to "fix" them.
- Skip symlinks whose targets can't be resolved.

A failed-resolution symlink at restore time is **not an
error condition**. It's the archive's intentional preservation
of the project's structure. If the operator needs the targets,
they were always responsible for ensuring those are available
at restore time (typically by archiving the shared libraries
separately).

#### External reference visibility

To make external dependencies tractable for downstream
tooling, the manifest records each external symlink in a
top-level `ExternalReferences` array (§8.1). This enables:

- The orchestrator generating a "dependency report" after
  archive completion: "this object has 47 references to
  `/raid/shared_audio/2026/show01/`."
- The 3b catalog indexing external references for cross-
  archive queries: "which archives reference paths under
  `/raid/shared_audio/show01/`?"
- The restore tool displaying the dependency list to the
  operator before extraction proceeds.

This is metadata — informational, queryable, and visible to
operators. It does not change the format's restore semantics
or its on-tape representation of the symlink itself.

#### Failure modes prevented

The default `SymlinkPolicy::Default` catches the genuine bug
(internally-broken symlinks) without breaking the normal
workflow (external symlinks). The full table from §9.0 lists
the precise check.

The historical BRU failure mode you described — archive
written successfully, restore fails because target missing —
**does not occur** under this design. Restore always succeeds;
the operator gets the symlinks back as written; whether the
external targets exist on the restore system is an explicit,
upfront concern visible in the manifest summary.

#### Cycle immunity

A separate historical failure mode worth calling out: some
Final Cut Pro versions wrote `.fcpcache/` cache folders that
contained **self-referential directory symlinks** (a symlink
inside the folder pointing back to the folder itself, or to
an ancestor). Older archive tools that dereferenced symlinks
during traversal would recurse into these cycles, writing the
same content 10-20 times before some internal depth limit
fired — by which time hundreds of GiB of duplicate data had
already been written to tape. BRU exhibited this behavior in
production.

**rem-tar-v1 is structurally immune to symlink cycle bombs**
under every defined `SymlinkPolicy`:

- `SymlinkPolicy::Default`: symlinks are recorded as symlinks
  (typeflag '2'). The writer never opens the target, never
  recurses into a symlinked directory. A symlink-to-self in
  `.fcpcache/` is recorded as a single tar entry with the
  link target string; nothing is followed.
- `SymlinkPolicy::Strict`: same behavior as Default for
  symlinks that pass validation; the only difference is which
  symlinks are rejected, not how they're traversed.
- `SymlinkPolicy::Permissive`: same; even internally-broken
  symlinks are stored as link entries, not by following them.

**There is no defined policy in v1.x that dereferences
symlinks during traversal.** A future `Dereference` policy
would require explicit cycle detection (visited-inode
tracking, depth limits) and is outside the scope of v1.0 —
see §16 for the open question.

The walker that builds the input file list (orchestrator
concern, not format) is the other half of this story. If the
orchestrator naively follows directory symlinks during its
own tree walk, the input list given to the writer could
already be inflated by cycles. The writer's input-list
validation (§9.0) defends against this with entry-count
limits and inode-duplicate detection — a defense in depth
even when the orchestrator's walker is buggy. But the
correct fix is at the walker layer, and the spec documents
this requirement (§16).

### 5.9 Special files

Beyond regular files and symlinks, a source tree may contain
hard links, directories, and special files (device nodes,
FIFOs, sockets). rem-tar-v1's policy:

**Hard links.** When the input list contains multiple paths
sharing the same inode (and the orchestrator declares them as
hard links rather than the walker-bug case of §9.0), the
first occurrence is archived as a regular file (typeflag `0`)
with its full data and chunks. Subsequent occurrences are
archived as tar hard-link entries (typeflag `1`) referencing
the first, with no data. The manifest records a `HardlinkEntry`:

```cbor
HardlinkEntry = {
    1: bytes .size 16,        ; hardlink_id (UUID)
    2: tstr,                  ; path within archive
    3: bytes .size 16,        ; target file_id (the regular FileEntry it links to)
    4: tstr,                  ; target path within archive
    5: ?uint,                 ; mtime (per metadata-preservation mode)
}
```

On restore, the reader recreates the regular file first, then
hard-links subsequent entries to it. If the target file isn't
present (partial restore of a single hard-linked path), the
reader falls back to extracting it as an independent regular
file by following the catalog to the target's chunks.

**Directories.** Directory entries (typeflag `5`) are recorded
when directory-level metadata (mtime, mode, xattrs) is part of
the metadata-preservation promise. The manifest records a
`DirectoryEntry`:

```cbor
DirectoryEntry = {
    1: tstr,                  ; path within archive
    2: ?uint,                 ; mtime (per preservation mode)
    3: ?bool,                 ; (reserved; directories have no exec-bit semantics)
    4: ?uint,                 ; mode (Full mode only)
    5: ?uint,                 ; uid (Full only)
    6: ?uint,                 ; gid (Full only)
    7: ?tstr,                 ; uname (Full only)
    8: ?tstr,                 ; gname (Full only)
    9: ?{ * tstr => bytes },  ; xattrs (archival/full)
}
```

Empty directories are preserved (a directory with no children
still gets a DirectoryEntry, so the structure is faithfully
reproduced). Non-empty directories may be implicit (created
on restore as a side effect of extracting their children) or
explicit (a DirectoryEntry preserving their metadata) — the
writer emits explicit entries when the preservation mode
includes directory metadata.

**Device nodes, FIFOs, sockets.** These (typeflags `3`, `4`,
`6`, and sockets which have no standard tar typeflag) are
**rejected by default** at pre-write validation. They are
meaningless in a video archive — a device node is a reference
to a kernel driver, not content; a FIFO/socket is a runtime
IPC object with no persistent content. Archiving them
faithfully would preserve nothing useful and could be a
security concern on restore (a restored device node with the
wrong major/minor could expose hardware).

A `system-backup` mode (a `WriteParams` flag, distinct from
`MetadataPreservation`) allows these to be archived as their
tar typeflags for the rare case where rem-tar-v1 is used to
back up a full system rather than archive media projects. In
that mode they're recorded as `SpecialFileEntry` with their
type and device numbers. This mode is off by default and
should be used only with explicit operator intent.

Validation reasons for rejected special files are surfaced in
the preflight failure list (§13.1) like any other validation
failure, so the operator sees exactly which paths were
rejected and why.

### 5.10 USTAR header field values in non-Full modes

A subtle but important correctness point: the ustar header of
every tar entry has fixed fields for `uid`, `gid`, `mode`,
`uname`, and `gname`. These fields always physically exist in
the header — they can't be "omitted." So even though
`Archival` and `Minimal` modes don't *preserve* ownership in
the manifest, the writer must put *something* in the ustar
header fields, and a standard `tar -x` run (especially as
root) would apply those values.

To prevent a standard tar restore from applying ownership/
permissions that the format claims not to preserve, the writer
sanitizes ustar header fields by preservation mode:

| Field | Minimal | Archival | Full |
|--|--|--|--|
| `uid` | 0 | 0 | source uid |
| `gid` | 0 | 0 | source gid |
| `uname` | "" (empty) | "" (empty) | source uname |
| `gname` | "" (empty) | "" (empty) | source gname |
| `mode` | 0644 (files) / 0755 (dirs) | 0644 or 0755, plus +x for executables | source mode |
| `mtime` | 0 | source mtime | source mtime |

In Minimal and Archival modes, the ustar headers carry neutral
values (uid/gid 0, empty owner names, default modes). A
standard `tar -x` as root would create files owned by root
with default permissions — which is the honest behavior:
the archive genuinely doesn't carry the source ownership, so
restore gets neutral defaults, not stale/wrong source values.
In Archival mode the executable bit is set in the mode field
where applicable (so a script restored with standard tar stays
executable), and mtime is preserved.

The Remanence-aware reader uses the manifest (not the ustar
header) as the source of truth for what to apply, and applies
exactly what the preservation mode recorded. The sanitized
ustar values exist only so that standard-tar fallback behaves
consistently with the format's stated preservation semantics.

---

## 6. Chunk size and compression

### 6.1 Chunk size rationale

Default chunk size: **256 KiB (262144 bytes)**. This is the
size of one tape block; each chunk occupies exactly one block
on tape.

The choice is anchored in industry consensus for LTO performance
and operator practice:

- **HPE's LTO-9 documentation** recommends 256 KiB as the
  default block size for general use, with 512 KiB available
  for high-throughput streaming workloads and 1 MiB reserved
  for tuned environments after testing on the specific drive,
  HBA, OS, and software stack.
- **The Archives team's existing LTO-6/7 production tapes**
  use 256 KiB blocks. Operator familiarity with this value
  reduces operational surprise.
- **Veeam and most major backup software** default to 256 KiB,
  meaning third-party tools that might be used for recovery
  in extreme scenarios handle this size without configuration.
- **Older Linux kernels (4.x and earlier)** could require
  `sg` driver buffer tuning above 512 KiB. Modern kernels
  (6.x like akash) handle larger blocks fine, but smaller
  blocks remain more universally compatible.

The format is fully parameterized — `REMANENCE.chunk_size` is
recorded per-object in the global pax header and can be any
LTO-permitted value from 4 KiB up to the drive's reported
maximum (8 MiB on LTO-9 with encryption, 16 MiB without).
Operators can override the default for specific tape pools:

| chunk_size | Use case |
|--|--|
| 64 KiB | Maximum compatibility, legacy environment |
| **256 KiB (default)** | Balanced default, matches industry practice |
| 512 KiB | High-throughput sequential video; tested LTO-9 only |
| 1 MiB | Tuned environment, multi-TB single files (smaller chunk index) |

Trade-offs at different chunk sizes:

- **Per-block CDB overhead** is irrelevant at any of these sizes
  (sub-ms per block; sub-1% of write rate at LTO-9 speeds).
- **Chunk index size** scales inversely with chunk size. At
  256 KiB and 48 bytes per chunk record, a 100 GiB file's
  chunk index is ~20 MiB. Single pax keywords above ~4 MiB
  use the multi-keyword split (§7.3); this is transparent.
- **Last-chunk padding waste** scales with chunk size. At
  256 KiB, the average waste per file is 128 KiB.
  Negligible for video archives; potentially significant
  for archives of very small files.
- **3c parity neighborhood block count** scales inversely
  with chunk size (3c is parameterized by block count, not
  bytes). Total memory footprint per neighborhood stays the
  same; just more bookkeeping at smaller chunk sizes.

#### Parity geometry must track the chunk size

The 3c default geometry was originally stated for 1 MiB
blocks: `S=128, m=4` giving ~512 MiB contiguous-loss
tolerance per neighborhood (S × m × block_size = 128 × 4 ×
1 MiB). At the rem-tar-v1 default of **256 KiB blocks**, the
same `S=128, m=4` gives only 128 × 4 × 256 KiB = ~128 MiB
tolerance — a 4× reduction, because tolerance scales with
block size.

This is a real inconsistency the design review caught. To
keep parity geometry coherent with the 256 KiB chunk default,
3c's default configuration must be **block-size-aware**. Two
options, and the spec picks the first:

1. **Retain ~512 MiB tolerance: use `S=512, m=4` at 256 KiB
   blocks** (512 × 4 × 256 KiB = 512 MiB). Neighborhood size
   becomes 512 × (128+4) × 256 KiB ≈ 16.5 GiB — same as the
   1 MiB / S=128 neighborhood, so memory footprint is
   unchanged. This is the recommended default: same tolerance,
   same neighborhood byte-size, just more (smaller) blocks
   per neighborhood.
2. **Accept ~128 MiB tolerance with `S=128, m=4`.** Simpler
   geometry, smaller neighborhoods (~4 GiB), but 4× less
   contiguous-damage tolerance. Acceptable only if the damage
   model (Appendix A of 3c — servo damage of 100 MB–2 GB)
   permits it; given the observed 2 GiB worst case, 128 MiB
   is too low. So option 1 is correct for archive's damage
   profile.

The 3c default table must be updated to express geometry as a
function of block size, defaulting to `S=512, m=4, k=128` at
256 KiB. This is tracked as a 3c follow-up (§16). rem-tar-v1
itself doesn't choose parity geometry — it just needs the
block sizes to agree — but the inconsistency is flagged here
because it surfaced from the rem-tar-v1 block-size default.

### 6.2 No format-level compression; drive compression disabled

`rem-tar-v1` does **not** compress data, and it disables the
LTO drive's built-in compression (LTO-DC). Chunks on tape are
raw source bytes. This is a deliberate simplification of the
tape-writing mechanism, made on the principle that **compression
is an orchestrator-level function, not a tape-format function.**

#### Compression belongs to the orchestrator, not the format

Three layers of compression could in principle apply to data on
a Remanence tape:

1. **Source-file compression** — whatever the producer already
   did (H.264/H.265 video, JPEG, FLAC, gzip on logs). This is
   the *only* compression in the system, and it's entirely the
   orchestrator's / producer's concern. rem-tar-v1 archives
   whatever bytes it's handed.
2. **Drive hardware compression (LTO-DC)** — disabled (below).
3. **Format-level compression** — removed from the format
   (below).

If a specific dataset is genuinely compressible (e.g. a large
pile of uncompressed TIFF/DPX image sequences, uncompressed
scan data, or text corpora), the **orchestrator** compresses it
into `.zst` (or any container) files *before* handing them to
rem-tar-v1, which then stores those already-compressed bytes
faithfully like any other file. The compressible case is still
served; it just lives one layer up, where it belongs. The tape
format stays simple and the 30-year fallback stays
unconditional.

#### Why the format itself doesn't compress

- **The workload is already compressed.** HD/4K video, MXF
  archives, JPEG sequences, MP3/FLAC audio — archive's archive is
  essentially all codec-compressed already. Format-level zstd
  on top buys low-single-digit percent at best, often nothing,
  and on incompressible input slightly *expands* the data.
- **Complexity cost is disproportionate.** Format-level
  compression forces a logical-size-vs-stored-size split, the
  tar `size` field no longer equals the file's logical size,
  per-chunk length framing, an expansion-fallback rule, and a
  weakened "standard tar is byte-correct only for uncompressed
  files" caveat. All of that complexity, in the tape-writing
  mechanism, to serve a case the workload almost never hits.
- **The 30-year fallback should be unconditional.** With no
  format-level compression, every object is a clean pax tar
  archive whose files extract byte-exact with standard `tar` —
  no codec, no Remanence-aware reader, no exceptions. That is
  the format's central durability promise; compression put an
  asterisk on it.
- **Right layer for the decision.** Whether a dataset is worth
  compressing is a content/policy judgement the orchestrator is
  positioned to make per dataset. Baking it into the tape
  format spreads that judgement into the lowest, least-flexible
  layer.

#### Why drive-level compression (LTO-DC) is also disabled

The drive is configured by Layer 3a's `write_config` with
`compression: false`. LTO-DC is rejected for reasons beyond the
"already compressed" point:

- **Unpredictable capacity.** LTO-DC's ratio is data-dependent,
  so a tape's data capacity *in source bytes* varies by
  1.0–2.0×. The 3c parity scheme is configured in tape blocks,
  and the orchestrator needs to predict how much fits on a
  tape; LTO-DC breaks both.
- **Breaks fixed-block byte-range math.** Tier 2 access relies
  on a fixed 1 chunk = 1 tape block mapping. Under LTO-DC the
  block↔source-byte relationship is non-linear and unknowable
  until the block is read.
- **Vendor-specific algorithm, 30-year risk.** LTO-DC is the
  LTO consortium's specific LZ variant; reading it in 30 years
  needs a drive that still implements it. Uncompressed blocks
  are recoverable by any device that can read tape blocks.
- **Encryption-ordering footgun.** With drive-level encryption
  (Layer 6), compression must precede encryption; the ordering
  is configurable per the LTO SCSI spec and getting it wrong
  silently produces unreadable tapes. Disabling LTO-DC
  sidesteps the whole class of bug.
- **Smaller error domain.** An uncorrectable error inside an
  LTO-DC compressed block loses the whole block's source data
  to a decompressor failure; without LTO-DC it loses exactly
  one tape block, recoverable by 3c parity.

#### The `compression` field is reserved, `none`-only

The `REMANENCE.compression` keyword and the manifest
`compression` field are **retained as a reserved enum that
only takes `none` in v1.** Writers always write `none`; readers
reject any other value as an unsupported-feature error
(`UnsupportedFeature`). Retaining the field means a future
v1.x could reintroduce format-level compression — if real,
measured evidence ever justifies it — without a format-version
bump. No v1 machinery exists for any other value.

#### Summary

| Layer | Used by rem-tar-v1? |
|--|--|
| Source-file compression | Out of scope (producer/orchestrator) |
| Orchestrator pre-compression to `.zst` etc. | Yes — the supported path for compressible data |
| LTO-DC (drive-level) | **No** — disabled via 3a write_config |
| Format-level compression | **No** — removed; field reserved `none`-only |

## 7. REMANENCE.* pax keywords

All `rem-tar-v1`-specific metadata lives in pax extended
headers using the `REMANENCE.` namespace, conforming to the
pax standard's vendor-namespace convention.

### 7.1 Global pax header keywords

These keywords appear in the typeflag-`g` global header at
object start. They apply to the entire object.

| Keyword | Required | Value | Description |
|--|--|--|--|
| `REMANENCE.format_id` | yes | string | Must equal `rem-tar-v1`. |
| `REMANENCE.schema_version` | yes | string | Format schema version, e.g., `1.0`. Major version mismatches are rejected; minor mismatches are forward-compatible. |
| `REMANENCE.object_id` | yes | string | UUID for this object (hex with dashes). |
| `REMANENCE.caller_object_id` | yes | string | The orchestrator's opaque identifier for this object. |
| `REMANENCE.chunk_size` | yes | decimal | Chunk size in bytes. Default 262144 (256 KiB). Must match the tape block size; see §6.1. |
| `REMANENCE.encryption` | no | string | If present, indicates encryption is in use. Values: `aes-gcm-256`. Absent means no encryption. Key management is Layer 6's concern; this is a flag for readers. |
| `REMANENCE.write_timestamp` | yes | string | RFC 3339 timestamp of object creation. |
| `REMANENCE.metadata_preservation` | yes | string | One of `archival` (default), `full`, `minimal`. Records which preservation mode produced this object (§9.4); tells readers which manifest/ustar fields to expect. |
| `REMANENCE.writer_version` | no | string | Software version that wrote this object, e.g., `remanence/0.1.0`. |
| `REMANENCE.pad` | no | string | An inert pax keyword placed *inside a real pax extended header* (the global header, or a file/manifest pax header) to enlarge it so the following file/manifest data starts on a BodyLba boundary (§5.2, §7.5). Its value is run of spaces; readers ignore it. It is **never** a standalone tar member or padding entry. |

### 7.2 Per-file pax extended header keywords

These keywords appear in the typeflag-`x` header that precedes
each regular file entry.

| Keyword | Required | Value | Description |
|--|--|--|--|
| `path` | sometimes | string | Standard pax keyword. Required if the file path exceeds the 100-character ustar limit. |
| `size` | sometimes | decimal | Standard pax keyword. Required if the file size exceeds the 8 GiB ustar limit. Almost always required for video. |
| `mtime` | archival/full | decimal | Standard pax keyword. Modification time with sub-second precision. Present in `archival` and `full` preservation; absent in `minimal`. |
| `uid` / `gid` / `uname` / `gname` | full only | various | Standard pax keywords for owner/group. Emitted **only** in `MetadataPreservation::Full`. See §9.4. |
| `REMANENCE.executable` | archival/full | bool | The executable (+x) bit. Emitted in `archival` and `full`; the only permission bit `archival` preserves. |
| `REMANENCE.file_id` | yes | string | UUID for this file (hex with dashes). Stable across re-archives. |
| `REMANENCE.file_sha256` | yes | string | Hex-encoded SHA-256 of the full file content (uncompressed bytes). Used as the cryptographic anchor for tamper detection. |
| `REMANENCE.chunk_count` | yes | decimal | Number of chunks for this file. Equal to `ceil(size / chunk_size)` for non-empty files; 0 for empty files. |
| `REMANENCE.compression` | yes | string | Reserved enum; v1 writers always emit `none`. Readers reject any other value (`UnsupportedFeature`). See §6.2. |
| `REMANENCE.encryption_kek_ref` | no | string | If encryption is in use, opaque reference to the KEK used to wrap this file's DEK. See Layer 6 for semantics. |

**No on-tape chunk index in pax headers.** Per-chunk LBAs are
not stored — they are arithmetically derivable from the file's
`first_chunk_lba` (a `BodyLba`, recorded in the catalog) and
the chunk ordinal: `chunk_body_lba = first_chunk_lba + N`.
This works because of the alignment invariant (§5.2): each
chunk occupies exactly one `BodyLba` block, in order, starting
at the file's first chunk block. Object tape files contain no
parity blocks; 3c maps `(tape_file_number, BodyLba)` to a
physical tape position via the filemark map, and parity lives
only in separate sidecar tape files (3c v0.4.2 §5.5) — none of
which affects this arithmetic.

Per-chunk integrity codes (CRC-64) live in the manifest only,
not in pax headers. The trade-off is documented in §10.7 —
catalog-loss recovery from pax headers alone loses per-chunk
integrity verification but retains file-level integrity via
`REMANENCE.file_sha256`.

### 7.3 Chunk LBA derivation

Given a file's catalog row containing `first_chunk_lba` (a
`BodyLba`) and `chunk_size`, and an in-file byte offset `b`:

```
chunk_index    = b / chunk_size
chunk_body_lba = first_chunk_lba + chunk_index
byte_in_chunk  = b % chunk_size
```

This is pure arithmetic with no table lookup, all in `BodyLba`
space. Byte-range restore for `[start_byte, end_byte)`
computes the first and last chunk BodyLbas directly and reads
only those chunks (3c maps each `(tape_file_number, BodyLba)` to
a physical tape position via the filemark map; object tape files
contain no parity blocks).

The alignment invariant guarantees these derivations are
correct for any file in a rem-tar-v1 object:
- Each chunk occupies exactly one `BodyLba` block.
- The last chunk is a *logical* chunk that may be shorter than
  `chunk_size` (the file's tail bytes); it is NOT physically
  zero-padded within the tar payload (§5.2). The reader trims
  to the true file size from the catalog/manifest.
- Chunks are contiguous in BodyLba starting at
  `first_chunk_lba`.
- No padding or interleave between chunks of the same file in
  BodyLba space. (Object tape files hold only object data;
  parity is in separate sidecar tape files, 3c §5.5, and does
  not affect BodyLba contiguity.)

All chunks are raw source bytes (no compression — §6.2). A
file's chunks are exactly `chunk_size` bytes each, except the
final logical chunk which is the file's tail and is **not**
padded within the tar payload — it is followed only by tar's
normal 512-byte-record padding, then the next entry's header
(§5.2). Standard tar therefore extracts every file byte-exact.

The one place block-level zero-fill occurs is at the very end
of an object, *after* the tar EOF records: on fixed-block LTO
the writer must emit whole `chunk_size` blocks, so the final
block is zero-filled past the EOF (§5.2). This is tar-safe
because standard tar stops at the EOF and never reads the
trailing zeros; the padded block is parity-protected like any
other.

### 7.4 Why no chunk index in pax headers anymore

Earlier drafts (v0.1, v0.2) carried a full chunk index in the
`REMANENCE.chunks` pax keyword. Removing it has several wins:

- **Smaller pax headers.** Files of any size fit comfortably
  in their pax extended header without split-keyword tricks.
  A 1 TiB file's pax header is small; only its `file_sha256`,
  `chunk_count`, and standard pax keys are needed.
- **Simpler writer.** The writer no longer needs to compute
  all chunk LBAs before emitting the pax header. The header
  can be written as soon as the file's `file_sha256` is known.
- **Trust-anchor separation.** Per-chunk integrity codes are
  ephemeral data that belongs with the manifest (which is
  itself anchored by SHA-256 in the catalog). Pax headers
  carry only durable per-file identity and the cryptographic
  file hash.
- **Forward-scan recovery still works.** A scanner walking
  the tape encounters file pax headers in order, knows the
  file's chunk_count (from the keyword), reads that many
  chunk_size blocks, and moves to the next file. The reader
  inherently knows each chunk's LBA from its tape position;
  no on-tape table is consulted. Per-chunk integrity is
  unavailable on forward-scan-only recovery (the manifest is
  the only source for that), but file-level integrity via
  `REMANENCE.file_sha256` is preserved.

### 7.5 Manifest-specific keywords

The object manifest file is identified by:

| Keyword | Value |
|--|--|
| `path` | `_remanence/manifest.cbor` |
| `REMANENCE.is_manifest` | `true` |
| `REMANENCE.file_id` | UUID for the manifest itself (separate from any file's UUID) |
| `REMANENCE.file_sha256` | SHA-256 of the manifest content |

The manifest is otherwise a regular file entry. Its data is
chunk-aligned like any other file.

### 7.6 Normative pax-header padding algorithm

Block alignment of file data (§5.2) is achieved by sizing the
`REMANENCE.pad` record inside the file's real pax extended
header. The arithmetic is subtle because a pax extended-header
record is self-describing:

```
"<len> <keyword>=<value>\n"
```

where `<len>` is the **total** record length in bytes *including
the digits of `<len>` itself*. Adding pad bytes to `<value>`
can push `<len>` across a decimal-digit boundary (e.g. 99→100),
which widens `<len>` by a digit, which changes the total again.
A single closed-form solve is therefore wrong in the boundary
cases; the writer MUST **iterate to a fixed point**.

Definitions (all in bytes):
- `S` = the chunk/block size; `chunk_size MUST be a multiple of
  512` (it is — 256 KiB = 512 × 512).
- `O` = the BodyLba-stream byte offset at the **start of this
  file's pax extended-header tar record** (always a 512 multiple,
  since the preceding entry ended on a record boundary).
- `P` = the **total encoded pax body length** in bytes — every
  pax record in this header, *including* the `REMANENCE.pad`
  record, concatenated. (The body as a whole is rounded to a
  512 record boundary; records are NOT individually rounded.)
- `roundup512(x)` = `((x + 511) / 512) * 512`.

The on-tape structure of a pax-prefixed file entry is three
parts before the file data (this is the source of the bug the
v0.9.0 equation had — it omitted the two 512-byte ustar records):

```
  [512]            pax extended-header ustar record (typeflag 'x')   <- at O
  [roundup512(P)]  pax body (the extended records, incl. REMANENCE.pad)
  [512]            the file's own ustar header record (typeflag '0')
  [file data ...]                                                    <- must be S-aligned
```

Goal: choose `REMANENCE.pad` (which sets `P`) so the file data
starts on a block boundary:

```
( O + 512 + roundup512(P) + 512 )  ≡  0  (mod S)
       ^pax hdr   ^pax body   ^file ustar hdr
```

Algorithm (iterate to fixed point):

```
1. Let H_other = encoded length of all pax records EXCEPT
   REMANENCE.pad. Compute the data offset assuming the smallest
   legal pad record:
     base = O + 512 + roundup512(H_other + min_pad_record_len) + 512
     target = roundup_to_S(base)            // next S multiple ≥ base
     gap = target - base                    // bytes still to absorb
2. Construct a pad record whose TOTAL length is min_pad_record_len
   + gap, by setting value = `gap`-adjusted spaces. Compute its
   actual encoded length L (which includes its own <len> digits),
   then P = H_other + L.
3. Recompute:
     total = O + 512 + roundup512(P) + 512
4. If total ≡ 0 (mod S): done.
   Else: set gap += (roundup_to_S(total) - total), go to 2.
   (The digit-boundary correction converges in ≤ 2 iterations,
   since each pass changes <len> width by at most one digit and
   the spaces value can absorb any residue ≥ the minimum record
   size.)
5. If the smallest pad record already overshoots S (the header
   without padding is within one record of the boundary), add a
   full S of padding spaces (one extra block) and re-solve —
   one wasted block, only on this rare boundary case.
```

The pad value is ASCII spaces (0x20), which are inert: standard
pax readers ignore the unknown `REMANENCE.pad` keyword, and the
spaces never affect the following entry. The writer applies the
same algorithm before the global header's following file, and
before the manifest's data.

Implementation MUST test this across many path lengths and pad
lengths, specifically exercising pad records whose `<len>`
crosses 9→10→100→1000-byte digit boundaries, because that is
where a naive single-pass solver produces an off-by-one
misalignment. (Impl plan step 12.6.1.)

---

## 8. Object manifest

The object manifest is a regular file in the tar archive,
always the last file before the tar EOF, named
`_remanence/manifest.cbor`. In the current implementation it
contains per-file layout and integrity metadata plus the complete
payload file list — in CBOR format. Older drafts also placed
per-chunk CRC-64 data here; that part is superseded and deferred.

**Current implementation note.** The active `remanence-format`
implementation follows `docs/spec-v0.4.md` §8.7.5, not the older
integer-tagged schema in §8.1 below. Current manifest CBOR uses
canonical string keys, records per-file SHA-256 plus file layout,
and deliberately omits per-chunk CRC-64 arrays and manifest-level
`mtime` fields. Per-chunk CRC verification is deferred to a future
minor format revision or an explicit design update; current integrity
is anchored by pax `REMANENCE.file_sha256` and whole-manifest
`manifest_sha256`.

**The manifest does not describe itself (normative, review #5).**
The `ObjectManifest.FileEntry` array lists archived *payload*
files only. It does NOT contain a `FileEntry` for
`_remanence/manifest.cbor` itself — that would be circular, since
the manifest's own SHA-256 cannot be inside the bytes being
hashed. The manifest file's identity, size, chunk count, and
SHA-256 live in its pax extended header (§7.5) and in the object
catalog row (`manifest_sha256`, `manifest_size_bytes`,
`manifest_chunk_count`), never in the manifest body.

The manifest complements the pax extended headers. Pax headers
carry per-file identity and the cryptographic file hash;
they're forward-scan-recoverable. The active manifest carries
the file layout and object-level file list needed for direct
catalog reconstruction. Loss of the manifest degrades direct
manifest-based enumeration; loss of pax headers (within the tar
stream) is unrecoverable for the affected entry.

### 8.1 CBOR schema (superseded)

**Superseded for current code.** The schema below is historical
design context from the pre-`spec-v0.4` draft. Do not use it to
implement or validate current `rem-tar-v1` objects. The current
schema is the `docs/spec-v0.4.md` §8.7.5 object manifest:

```text
ObjectManifest {
    schema_version,
    object_id,
    caller_object_id,
    chunk_size,
    file_entries,
    external_references,
    object_metadata,
}

FileEntry {
    file_id,
    path,
    size_bytes,
    file_sha256,
    first_chunk_lba,
    chunk_count,
    executable,
    metadata_preservation_data,
}
```

The active manifest excludes itself, uses canonical CBOR ordering
for string keys, and does not contain `chunk_crcs` or file `mtime`.
The old per-chunk CRC-64 discussion in this section and §§8.2-8.4
is retained only as rationale for a deferred feature.

```cbor
ObjectManifest = {
    1: ManifestHeader,
    2: [* FileEntry],
    3: ?[* SymlinkEntry],             ; new in v0.5 — symlink entries
    4: ?[* ExternalReference],         ; new in v0.5 — external dependencies
    5: ?[* HardlinkEntry],             ; new in v0.6 — hard links (§5.9)
    6: ?[* DirectoryEntry],            ; new in v0.6 — directory metadata (§5.9)
    7: ?[* SpecialFileEntry],          ; new in v0.6 — only in system-backup mode (§5.9)
}

ManifestHeader = {
    1: tstr,                          ; format_id ("rem-tar-v1")
    2: tstr,                          ; schema_version ("1.0")
    3: bytes .size 16,                ; object_id (UUID)
    4: tstr,                          ; caller_object_id
    5: uint,                          ; chunk_size in bytes
    6: tstr,                          ; write_timestamp (RFC3339)
    7: uint,                          ; total_file_count
    8: uint,                          ; total_chunk_count
    9: uint,                          ; total_bytes (sum of file sizes)
   10: ?tstr,                         ; encryption ("aes-gcm-256" or absent)
   11: ?tstr,                         ; writer_version
   12: ?uint,                         ; total_symlink_count
   13: ?uint,                         ; total_external_reference_count
   14: tstr,                          ; metadata_preservation ("archival"|"full"|"minimal")
}

FileEntry = {
    1: bytes .size 16,                ; file_id (UUID)
    2: tstr,                          ; path (filesystem path)
    3: uint,                          ; size in bytes
    4: ?uint,                         ; mtime (Unix epoch ns since 1970) —
                                      ;   present in archival/full, absent in minimal
    5: bytes .size 32,                ; sha256 of full file content
    6: uint,                          ; chunk_count (0 for empty files)
    7: ?uint,                         ; first_chunk_lba (per-object BodyLba);
                                      ;   ABSENT iff chunk_count == 0 (empty file)
    8: bytes,                         ; chunk_crcs (8 bytes × chunk_count,
                                      ;   packed CRC-64/XZ codes BE)
    9: ?[* uint],                     ; RESERVED (was compressed_chunk_sizes);
                                      ;   never present in v1, field tag retained
    10: tstr,                         ; compression — reserved enum, always "none"
                                      ;   in v1 (§6.2); readers reject other values
   11: ?bytes,                        ; encryption_kek_ref (opaque)
   12: ?bool,                         ; executable — the +x bit only.
                                      ;   present in archival/full, absent in minimal.
   13: ?uint,                         ; mode (full Unix permission bits) —
                                      ;   ONLY present in "full" preservation mode
   14: ?uint,                         ; uid — ONLY in "full"
   15: ?uint,                         ; gid — ONLY in "full"
   16: ?tstr,                         ; uname — ONLY in "full"
   17: ?tstr,                         ; gname — ONLY in "full"
   18: ?{ * tstr => bytes },          ; extended attributes (xattrs) —
                                      ;   present in archival/full, absent in minimal
}

SymlinkEntry = {
    1: bytes .size 16,                ; symlink_id (UUID)
    2: tstr,                          ; path within archive
    3: tstr,                          ; target string (verbatim from source)
    4: ?uint,                         ; mtime (Unix epoch ns) — present in
                                      ;   archival/full, ABSENT in minimal
                                      ;   (follows the preservation mode, §9.4)
    5: SymlinkClassification,         ; how this symlink was classified
    6: ?uint,                         ; mode (link permissions)
    7: ?uint,                         ; uid
    8: ?uint,                         ; gid
    9: ?tstr,                         ; uname
   10: ?tstr,                         ; gname
}

SymlinkClassification = uint
  ; 1 = Internal       (target resolves within archive root, in file list)
  ; 2 = ExternalAbsolute (target is absolute path outside archive root)
  ; 3 = ExternalRelative (target is relative path escaping archive root)
  ; 4 = InternallyBroken (only present if SymlinkPolicy::Permissive
  ;                       allowed it past validation)

ExternalReference = {
    1: bytes .size 16,                ; symlink_id (cross-reference to SymlinkEntry)
    2: tstr,                          ; archive-relative symlink path
    3: tstr,                          ; target string (verbatim)
    4: SymlinkClassification,         ; 2 or 3 (external categories)
    5: ?tstr,                         ; resolved-absolute target (helpful for queries;
                                      ;   for relative symlinks, the absolute form
                                      ;   computed against the symlink's parent dir)
}
```

Historical notes on the superseded encoding:

- In the superseded schema, **`chunk_crcs`** was one packed byte
  string of `8 * chunk_count` bytes, not a CBOR array of integers.
  Packed form was ~3× smaller than `[* uint]` of CRC-64 values
  (no per-element CBOR tags).
- **`first_chunk_lba`** is the per-object BodyLba of the file's
  first chunk. All other chunk BodyLbas are derivable as
  `first_chunk_lba + N`. Recorded in the manifest both for
  catalog-rebuild scenarios and as cross-check against the
  catalog's row for the same file. **Empty-file invariant:**
  `first_chunk_lba` is present iff `chunk_count > 0`; for an
  empty file (`chunk_count == 0`) it is absent (and the
  catalog's column is NULL — 3b follow-up). Readers MUST treat
  a present-but-with-zero-chunk_count or absent-but-nonzero
  combination as a corrupt manifest.
- **Field 9 is reserved** (formerly `compressed_chunk_sizes`).
  It is never present in v1 output. The tag is retained so a
  future v1.x could reintroduce format-level compression
  without disturbing other field tags (§6.2).
- **Metadata fields are tiered** (see §9.4 and §11). Field 12
  (`executable`), 4 (`mtime`), and 18 (`xattrs`) are present
  in `archival` and `full` modes. Fields 13-17
  (`mode`/`uid`/`gid`/`uname`/`gname`) are present **only in
  `full` mode**. In `minimal` mode only path, content,
  hashes, and chunk structure are present. The
  `metadata_preservation` field in `ManifestHeader` (field 14)
  records which mode the object was written with, so readers
  know which fields to expect.

The v0.6 collections (HardlinkEntry, DirectoryEntry,
SpecialFileEntry) are defined in §5.9. All are optional arrays
in the manifest; absent when the object contains none of that
entry type.

CBOR is chosen over JSON because:
- Hashes and identifiers are binary; CBOR handles binary natively,
  while JSON requires base64 or hex encoding.
- The current schema uses explicit semantic string keys; older
  numeric-key drafts are preserved above only as historical context.
- CBOR's canonical encoding rules allow byte-identical
  manifests for byte-identical inputs.
- The Rust ecosystem has solid CBOR support via `ciborium`.

#### Canonical CBOR is required

The manifest_sha256 trust chain (§8.3) depends on the manifest
encoding being byte-identical across implementations for
semantically identical content. The writer MUST emit
**canonical/deterministic CBOR** per RFC 8949 §4.2:

- Map keys sorted in bytewise lexicographic order of their
  encoded form.
- Definite-length encoding for all arrays, maps, strings, and
  byte strings (no indefinite-length items).
- Smallest-possible integer encoding (no padding to larger
  width than needed).
- No duplicate map keys.

Without this, two correct implementations could produce
semantically identical manifests with different byte
sequences and therefore different SHA-256 values, breaking
the trust chain and any cross-implementation verification.
`ciborium` supports canonical encoding; the implementation
must enable it and a test must verify byte-identical output
for a fixture manifest across encode cycles. (See §14 step
12.3.)

### 8.2 Manifest size

In the current string-keyed manifest, size is primarily proportional
to the number of archived entries, not to the number of file chunks.
Each payload file contributes one `FileEntry` with `file_sha256`,
`first_chunk_lba`, `chunk_count`, and preservation metadata. There
is no per-chunk CRC array in the current schema.

For very large single-file objects, the manifest remains small:
the file's chunk count is represented as one integer, not as one
record per chunk. For objects containing many small files, manifest
size is driven by paths and metadata fields.

Under the per-file LBA catalog model (§10.1), most reads don't
need the manifest at all — the catalog has enough information
for direct positioning to any file's first chunk. The manifest
is needed for object-level enumeration, extended metadata recovery,
and catalog reconstruction. If a future minor-format revision
reintroduces per-chunk verification, the manifest would also carry
that verification data.

### 8.3 Trust chain

This subsection describes the older per-chunk CRC trust chain. In
the current implementation, the active trust chain stops at per-file
SHA-256 plus whole-manifest SHA-256; the chunk-level CRC step is
deferred.

The older byte-range integrity design relied on this chain of
cryptographic anchors:

```
External trust:  catalog stored in Layer 4/5 (Postgres + backups)
                       ↓ records...
File-level:      file_sha256 (SHA-256, cryptographic)
                       ↓ matches REMANENCE.file_sha256 in pax
                       ↓ catalog also records manifest_sha256
Manifest-level:  manifest's own SHA-256 (cryptographic)
                       ↓ verified after manifest read
Chunk-level:     per-chunk CRC-64 codes (error detection)
                       ↓ verified after each chunk read
```

The chain anchors at the catalog: file and manifest hashes
stored there are the root of trust. Per-chunk CRCs inside the
manifest are then trusted because the manifest itself is
verified. If the manifest's SHA-256 matches the catalog's
record, the CRCs inside are also unmodified and can be used
for fast per-chunk verification.

This means **per-chunk CRC-64 is the right tool**: at this
layer the threat is random/software errors, not adversarial
tampering. Tampering is detected at the file-level
SHA-256 (catches modifications to file content) and
manifest-level SHA-256 (catches modifications to the
per-chunk codes themselves).

For tapes where the encrypted copy is in use, AES-GCM
provides additional per-block authentication (128-bit auth
tag) that catches both random and adversarial modification.
Per-chunk CRC-64 is then redundant for those tapes but
harmless.

### 8.4 Why duplicate the data in pax headers and manifest?

For the current implementation, the duplication is per-file identity,
layout, and file SHA-256. The per-chunk CRC references in this
subsection are historical rationale for the deferred chunk-verification
feature.

Each location serves a different access pattern and provides
a different guarantee:

- **Pax extended headers** are read sequentially as the reader
  walks the tar archive. They support catalog-loss recovery:
  a reader who locates to the object's start LBA can read
  through the archive without prior knowledge, building up
  the file list as it encounters pax headers. The cryptographic
  file hash is present (`REMANENCE.file_sha256`), so file-level
  integrity is verifiable even without the manifest.

- **Object manifest** is read once via a direct LOCATE to the
  manifest's known LBA. In the current schema it carries file
  layout, object-level enumeration data, and extended metadata
  that pax headers don't. Loss of the manifest degrades direct
  enumeration and extended-metadata recovery, but doesn't lose
  any file content or identity.

Defense-in-depth: the cryptographic file hash is duplicated
(pax + manifest) so any single source can confirm whole-file
integrity. A future per-chunk verification layer would belong
in the manifest because it is useful mainly for partial reads,
which are only meaningful when the manifest can be read. A
corrupted manifest can be reconstructed from pax headers (losing
manifest-only metadata); corrupted pax headers can be
cross-referenced against the manifest.

---

## 9. Write workflow

### 9.0 Pre-write validation

Before any tape I/O begins, the writer performs a complete
validation pass over the input file list. If any file fails
validation, the writer returns
`BeginWriteError::Preflight(PreflightError)` containing the
**full list** of failing files and the reason each failed.
No bytes are written to the tape sink in this case.

The validation is the first step of `begin_write`; the API
does not separate "validate" and "write" as two calls. Within
`begin_write`, the implementation is:

```
1. Validate every file in the input list (in parallel via rayon).
2. If any failures, return BeginWriteError::Preflight with
   the full failure list. No tape I/O has occurred.
3. Open ParitySink and proceed to step 1 of §9.1 below.
```

The single-call design is intentional: the orchestrator's
natural workflow is "hand the writer a list of files, get back
either success or a structured error." Splitting into two
calls would offer no useful intermediate state — if validation
succeeds, the next step is always to write.

#### Validation checks

For each file in the input list, the validator checks:

| Check | Detects | Action on failure |
|--|--|--|
| `path` is valid UTF-8 | Legacy encodings, mojibake | Fail |
| `path` contains no NUL bytes | Pax uses NUL-terminated fields internally | Fail |
| `path` length ≤ 4096 bytes | Practical upper bound | Fail |
| `path` is relative (no leading `/`) | Tar/pax convention | Fail or normalize |
| `path` contains no `..` components | Security against archive-extraction attacks | Fail |
| All xattr keys/values are valid UTF-8 (where stringly-typed) | Format requirement | Fail |
| `path` is unique within the object | Tar allows duplicates but they're confusing | Fail |
| File is readable | Permission/access check | Fail |
| File size matches `stat` | Detects concurrent modification | Fail |

For each symlink in the input list, the validator additionally:

| Check | Detects | Action |
|--|--|--|
| target string is valid UTF-8 | Encoding bug | Fail |
| target string non-empty | Filesystem corruption | Fail |
| target string ≤ 4096 bytes | Practical upper bound | Fail |
| Classify by target path (textual) | See §5.8 | Internal/External-Abs/External-Rel/Internally-Broken |
| Apply `SymlinkPolicy` to classification | Policy mismatch | Fail (Default + Internally-Broken; Strict + any non-Internal) |

The symlink check is **textual**: no filesystem `stat` of the
target. This is essential — the staging system typically
doesn't have the external mounts, so attempting to stat would
incorrectly classify legitimate external symlinks as broken.

For the input list as a whole, two sanity checks defend
against orchestrator-side walker bugs (e.g., walking a tree
with symlink cycles such as FCP's `.fcpcache/`):

| Check | Detects | Default threshold | Action |
|--|--|--|--|
| Total entry count ≤ `max_entries_per_object` | Walker that walked a cycle | 10,000,000 | Fail with `ExcessiveEntryCount` |
| No inode appears > `max_duplicate_inodes` times in input list (excluding declared hard links) | Walker that re-entered the same physical directory via a symlink cycle | 100 | Fail with `ExcessiveInodeDuplication` |

The inode-duplicate check is cheap because the validator
already calls `stat()` on each input file for the
readability/size check; it just records the `(st_dev, st_ino)`
tuple and counts occurrences. Hard links are detected
separately: if the orchestrator declares an entry as a hard
link (typeflag '1' in its input metadata) to a previous
entry, the second occurrence does not count toward the inode
duplicate limit.

Both thresholds are configurable per write session via
`WriteParams`:

```rust
pub struct WriteParams {
    // ... existing fields ...
    pub max_entries_per_object: usize,        // default 10_000_000
    pub max_duplicate_inodes: usize,          // default 100
}
```

These limits are intentionally generous — legitimate archives
will not approach them. Their purpose is to catch the
catastrophic walker-bug case (an archive being written 20×
its intended size) before tape I/O begins, not to constrain
normal usage.

The first six file checks plus the symlink-encoding checks
are format-correctness; the last three file checks are
operational hygiene; the symlink classification + policy
check is the project-integrity check; the input-list sanity
checks are the walker-bug defense. All run in pre-write
validation regardless. An orchestrator that wants relaxed
semantics (e.g., to allow non-UTF-8 paths by transliterating
first, or to accept internally-broken symlinks) must do its
own pre-processing or set the appropriate `SymlinkPolicy`
explicitly before calling the writer — the format itself
never accepts invalid input under the default policy.

#### Validation is stateless and parallel

Validation is **stateless**: no cache, no persistent index of
"known-good paths." Re-validating the same files after a
fix-up cycle re-runs all checks from scratch.

This is acceptable because validation is fast:
- String checks (UTF-8, NUL, length, traversal): nanoseconds per file.
- `stat()` syscall: ~10-100 µs per file on a warm cache, sub-ms even cold.
- For 50,000 files: ~2.5 seconds total on a warm cache,
  parallelizable to ~0.5 seconds with rayon on multi-core.

Caching validation results would require tracking
`(path, mtime, size, inode)` as cache keys, invalidating on
file modification, and persisting across orchestrator restarts —
all for a benefit measured in single-digit seconds. The
complexity costs outweigh the benefit, and a stale cache could
silently allow broken filenames onto tape. Stateless validation
is the safer and simpler design. (Open question §16.x records
this tradeoff and the conditions under which caching might
be revisited.)

#### Reporting

The `PreflightError` contains a `Vec<PathValidationFailure>`
with structured information for each failure:

```rust
pub struct PathValidationFailure {
    pub source_path: PathBuf,             // best-effort representation
    pub raw_bytes: Vec<u8>,               // exact source filesystem bytes
    pub reason: ValidationReason,
}

pub enum ValidationReason {
    NotValidUtf8 { error_offset: usize, bad_bytes: Vec<u8> },
    ContainsNul { offset: usize },
    TooLong { length: usize, limit: usize },
    AbsolutePath,
    PathTraversal { component_index: usize },
    InvalidXattrKey { key_bytes: Vec<u8> },
    InvalidXattrValue { key: String, value_bytes: Vec<u8> },
    DuplicatePath { other_index: usize },
    NotReadable { io_error_kind: io::ErrorKind },
    SizeMismatch { expected: u64, actual: u64 },

    // Symlink-specific (see §5.8):
    SymlinkTargetNotUtf8 { error_offset: usize, bad_bytes: Vec<u8> },
    SymlinkTargetEmpty,
    SymlinkTargetTooLong { length: usize, limit: usize },
    InternallyBrokenSymlink {
        symlink_path: PathBuf,
        target_string: String,
        resolved_path_in_archive: PathBuf,  // where it would point
        // (this path is inside the archive root but isn't in the file list)
    },
    SymlinkRejectedByStrictPolicy {
        symlink_path: PathBuf,
        target_string: String,
        classification: SymlinkClassification,
    },

    // Input-list sanity (defense against walker bugs, see §9.0):
    ExcessiveEntryCount {
        count: usize,
        limit: usize,
    },
    ExcessiveInodeDuplication {
        inode: (u64, u64),                   // (st_dev, st_ino)
        occurrence_count: usize,
        limit: usize,
        sample_paths: Vec<PathBuf>,           // first few paths sharing this inode
    },
}
```

The `raw_bytes` field captures the original filesystem bytes
exactly so a fix-up tool can identify which character(s) caused
the failure and propose a UTF-8-correct rename. The
`source_path: PathBuf` uses Rust's `OsString` semantics, which
can represent any byte sequence.

For `InternallyBrokenSymlink`, the fix-up tool can propose
either: (a) add the missing target file to the archive list,
or (b) remove the broken symlink from the source folder. The
operator chooses based on intent.

For `SymlinkRejectedByStrictPolicy` (only relevant when the
operator explicitly chose `SymlinkPolicy::Strict` for a
self-contained archive), the fix-up tool can propose either:
(a) dereference the symlink and include the target inline,
(b) remove the symlink, or (c) downgrade to
`SymlinkPolicy::Default` to accept external references.

#### Operator workflow

The intended orchestrator workflow on validation failure:

1. Writer returns `BeginWriteError::Preflight(failures)`.
2. Orchestrator presents the full failure list to the operator
   (or pipes it to a fix-up tool).
3. Fix-up tool generates a script of suggested renames:
   ```
   mv -- "$'\xe9\xe0\xfc.mxf'" "éàü.mxf"
   mv -- "$'\xc0\xc1\xc2.txt'" "???.txt"  # ambiguous, manual review
   ```
4. Operator reviews and applies the renames (with manual edit
   for ambiguous cases).
5. Orchestrator re-invokes `begin_write` with the same file list.
6. Validation passes; tape write proceeds.

This is more operator-time-efficient than per-file
fail-then-retry, because all problems are surfaced and
addressable in one cycle.

#### Symlink policy

The behavior of symlink validation is controlled by the
`SymlinkPolicy` field in `WriteParams`:

```rust
pub struct WriteParams {
    pub chunk_size: u32,
    // No `compression` field in v1 — format-level compression
    // was removed (§6.2). Compressible data is pre-compressed
    // by the orchestrator into .zst etc. files upstream.
    pub symlink_policy: SymlinkPolicy,
    // ... other fields ...
}

pub enum SymlinkPolicy {
    /// Default. Reject only internally-broken symlinks.
    /// Accept external symlinks (absolute or relative) regardless
    /// of whether their targets are reachable on the current
    /// filesystem. External symlinks are recorded in the
    /// manifest's ExternalReferences array. This is the right
    /// policy for the editor→staging→archive workflow.
    Default,

    /// Reject any symlink whose target is not within the archive
    /// root AND in the file list. Use for archives that must be
    /// fully self-contained (e.g., long-term portable archives
    /// where the operator wants no external dependencies).
    Strict,

    /// Accept all symlinks, including internally-broken ones.
    /// Use for emergency archives of partial projects where the
    /// operator accepts that some links won't resolve on restore.
    /// Records internally-broken symlinks in the manifest with
    /// classification InternallyBroken so they're queryable later.
    Permissive,
}

impl Default for SymlinkPolicy {
    fn default() -> Self {
        SymlinkPolicy::Default
    }
}
```

The choice of policy is per-write-session, not per-tape. A
single tape could contain objects written with different
policies; each object's manifest records its own
classifications.

### 9.1 Tape write sequence

**Write-session setup (once per tape, before any object).** Drive
configuration is session state, not an object-body action, so it
happens before the parity sink exists (review #4):

```
S1. Configure the drive via 3a's write_config: fixed block size =
    chunk_size (default 256 KiB) and drive compression OFF (§6.2).
S2. Construct the RawTapeSink over the configured DriveHandle, then
    ParitySink::new(raw_sink, scheme, tape_uuid, spool_cfg).
S3. ParitySink writes the BOT bootstrap (sink-owned sequence 0).
```

**Per-object planning (before begin_object).** 3c's
`begin_object` requires a conservative upper bound on the
object's block count, because the capacity reserve depends on the
object's future sidecars, its trailing filemark, the final
partial sidecar, and the final bootstrap (3c §7.5). rem-tar
computes `projected_size_blocks` first (review #3):

```
P1. projected_size_blocks = ceil_to_blocks of the sum of:
      global pax header record (512)
      + roundup512(global pax body)
      for each file/symlink/hardlink/directory entry:
        its pax extended header record (512)
        roundup512(its pax body, incl. REMANENCE.pad slack)
        its ustar header (512)
        its data payload plus normal tar 512-byte padding
      manifest pax header (512) + roundup512(manifest pax body)
        + manifest ustar header (512)
        + manifest payload plus normal tar 512-byte padding
      two 512-byte tar EOF records
      the final post-EOF zero-fill needed to complete the last block
    The estimate MUST be a conservative UPPER bound — never low.
    3c treats writing MORE than projected as an Invariant
    violation (3c §7.5), so rem-tar rounds pad/alignment slack UP.
    Source sizes are known pre-write (the orchestrator staged the
    object; the two-pass large-file path, §9.2, also yields size).

    Implementation guidance: do not maintain this as a hand-written
    parallel formula if avoidable. Run the same pax-header sizing,
    tar-padding, and BodyBlockWriter logic in a "counting mode" dry
    run. The dry run should produce projected_size_blocks >= the
    actual blocks emitted by the byte-for-byte writer, with equality
    expected for deterministic inputs.
```

**Per-object write.** The writer then streams the object in this
order:

```
1. ParitySink::begin_object(projected_size_blocks) (Layer 3c) —
   checks the capacity reserve, assigns this object's
   tape_file_number, and resets per-object BodyLba to 0. On a
   CapacityReserveExceeded the object has NOT started; close the
   tape and write the whole object on the next tape (3c §7.5).
2. All writes go through a BodyBlockWriter (§9.1.1) that buffers
   the tar byte stream into whole chunk_size blocks. (The drive
   was already configured in S1 — no per-object reconfiguration.)
3. Emit the global pax header (typeflag 'g') with object-level
   keywords from §7.1 into the BodyBlockWriter byte stream.

4. For each file in the object:
   a. Compute file_id (UUID). Begin streaming the file; compute
      a running SHA-256 over its full content (the file-level
      cryptographic hash). file_sha256 must be known before the
      pax header is emitted; for files too large to hash in
      advance, use the two-pass strategy (§9.2).
   b. Construct the pax extended header (typeflag 'x') with
      per-file keywords (§7.2): `REMANENCE.file_id`,
      `REMANENCE.file_sha256`, `REMANENCE.chunk_count`,
      `REMANENCE.compression=none`, plus standard pax keys.
      Size the header with a `REMANENCE.pad` record so the
      file's DATA will begin on a BodyLba block boundary
      (§5.2).
   c. Emit the pax extended header and the ustar header
      (typeflag '0') into the BodyBlockWriter stream. Because
      of (b)'s padding, the file data now starts exactly on a
      block boundary; record that BodyLba as first_chunk_lba.
   d. Emit the file's data bytes into the BodyBlockWriter
      stream. Then emit tar's normal 512-byte-record padding.
   e. Append a FileEntry to the in-progress manifest:
      file_id, path, size, mtime (per preservation mode),
      file_sha256, chunk_count, first_chunk_lba,
      compression=none, and tiered metadata (§9.4).
   f. Surface (tape_file_number, file_id, first_chunk_lba,
      chunk_count, file_sha256, size, ...) to Layer 5 for
      catalog recording.

4.5. For each symlink in the input list (in path order):
   a. Construct the pax extended header (typeflag 'x') with:
      REMANENCE.file_id (assigned at validation time),
      REMANENCE.symlink_classification, plus standard pax
      keys (`path`, `linkpath`, `mtime`, `uname`, `gname`).
   b. Write the pax extended header.
   c. Write a typeflag '2' (symbolic link) ustar entry with
      the symlink's path in `name` and the target string in
      `linkname`. No file data follows; no chunk_size alignment
      needed.
   d. Append a SymlinkEntry to the in-progress manifest. If
      classified as External-Absolute or External-Relative,
      also append an ExternalReference entry.
   e. Surface (symlink_id, path, target_string, classification)
      to Layer 5 for catalog recording.

5. After all files and symlinks:
   a. Serialize the manifest as canonical CBOR (§8.1; including
      the optional SymlinkEntry, ExternalReference,
      HardlinkEntry, DirectoryEntry, SpecialFileEntry arrays).
   b. Compute manifest_sha256 over the serialized bytes.
   c. Write the manifest's pax extended header, sized (via a
      REMANENCE.pad record if needed) so the manifest's data
      starts on a BodyLba boundary. Carries
      REMANENCE.is_manifest=true, REMANENCE.file_sha256 =
      manifest_sha256, REMANENCE.chunk_count for the manifest
      file itself. Record manifest_first_chunk_lba (BodyLba).
   d. Write the manifest's ustar header.
   e. Write the manifest's data, split into chunk_size pieces.
      The last piece is NOT zero-padded within the tar payload;
      it gets only tar's normal 512-byte-record padding (§5.2).

6. Write the tar EOF (two 512-byte zero records, then tar's
   normal padding to the 512-byte record boundary) into the
   BodyBlockWriter stream.

7. Object close — a two-step handoff with explicit ownership
   (review v0.8.1 #4):
   a. **rem-tar owns the final block.** Call
      `BodyBlockWriter::finish_after_tar_eof()`, which
      zero-fills and emits the object's final partial block
      (§9.1.1). Only rem-tar knows where tar EOF is, so only
      rem-tar may flush the last block.
   b. **3c owns the filemark and sidecars.** Call
      `ParitySink::finish_object()`, which asserts no partial
      object block is pending (it was just flushed in 7a),
      writes the terminating filemark, and emits any
      parity-epoch sidecar tape files for epochs completed
      during this object (§16.14). 3c never touches the partial
      tar buffer.

8. Return to Layer 5:
   - tape_file_number (which tape file this object occupies)
   - object block_count (every fixed block in the object tape
     file: headers, file data, manifest, tar EOF, and the
     post-EOF zero-filled final block)
   - manifest_first_chunk_lba (per-object BodyLba)
   - manifest_size_bytes and manifest_chunk_count (so a reader
     can seek directly to the manifest data and know how much
     to read — §10.5)
   - manifest_sha256 (for the catalog's record of trust)
   - metadata_preservation mode used for this object
   - the list of per-file rows (file_id, first_chunk_lba
     [per-object BodyLba, absent for empty files], chunk_count,
     file_sha256, size, mtime, ...)
   - the list of per-symlink rows (symlink_id, path,
     target_string, classification)
   - the list of external references for the dependency report
   - the lists of hardlink / directory / special-file rows
```

#### 9.1.1 BodyBlockWriter: byte-stream to fixed blocks

The writer does not write "chunks" directly to the BlockSink.
A tar archive is a byte stream in which headers, file data,
and 512-byte-record padding interleave; a single `chunk_size`
block can contain, e.g., a file's tail bytes + tar padding +
the start of the next file's pax header. The
**`BodyBlockWriter`** mediates this:

```
tar byte stream in (headers, file data, 512-byte padding)
  → buffer into chunk_size (e.g. 256 KiB) blocks
  → emit each FULL block to the 3c BlockSink (one block = one BodyLba)
  → finish_after_tar_eof(): after the caller has written the
    tar EOF records, zero-fill the remaining partial block and
    emit it (§5.2)
```

Rules the BodyBlockWriter enforces:

- **Never emit a short block mid-stream.** Fixed-block LTO
  requires whole blocks; a partial block is only ever the
  final one, emitted (zero-filled) by `finish_after_tar_eof()`.
- **Never zero-fill between entries or within a file payload.**
  The only zero-fill is the post-EOF tail of the final block.
- **rem-tar owns the final-block flush, 3c does not (review
  v0.8.1 #4).** `finish_after_tar_eof()` is the single owner of
  the partial-block flush, because only rem-tar knows where the
  tar EOF is. 3c's `finish_object()` asserts the buffer is empty
  and then writes the filemark and sidecars — it never holds or
  flushes a partial tar byte buffer. This prevents double
  flushes, illegal short writes, and layer leakage.
- **Track BodyLba.** The writer queries the BodyBlockWriter for
  the current BodyLba (= count of full blocks emitted so far)
  when it needs to record a file's `first_chunk_lba`. Because
  the preceding pax header was sized (via `REMANENCE.pad`) to
  land the file's data on a block boundary, that BodyLba is the
  file's first data block.

This abstraction is why the write workflow above can speak of
"emit the pax header" and "emit file data" without worrying
about block boundaries: the BodyBlockWriter turns the logical
tar byte stream into legal fixed-block writes, and guarantees
the only padding that ever reaches tape is (a) tar's standard
512-byte-record padding inside the stream and (b) the single
post-EOF zero-fill of the final block.

### 9.2 Memory management for very large files

A 100 GiB file at the default 256 KiB chunk size has about
400,000 chunks. The hard constraint is that `file_sha256` and
`chunk_count` must be known before the file's pax header is
emitted (§9.1), while LTO cannot seek back to rewrite that
header. Large files therefore use a two-pass plan.

The writer chooses between two large-file paths based on source
stability:

**Immutable / snapshot-backed source (preferred).** If the
orchestrator gives rem-tar a path that is guaranteed stable — for
example a read-only ZFS snapshot on akash — no temp copy of file
bytes is needed.

```
pass 1: read source; compute file_sha256
pass 2: reread source; emit pax header + ustar header + bytes
        through BodyBlockWriter; compute SHA-256 again while writing
        and compare against pass 1 before object commit
```

The second hash is not optional. It proves the bytes written to
tape are the bytes whose hash was placed in the pax header and
catalog. The snapshot guarantee should make mismatch impossible;
the check catches bugs or a bad stability assertion.

**Mutable / not-guaranteed-stable source.** If the source might
change between passes, pass 1 spools the exact bytes that will be
written. Pass 2 writes from the spool, not from the original path.

```
pass 1: read source once; compute file_sha256 and write the exact
        bytes to a temp spool file
pass 2: read the spool; emit pax header + ustar header + bytes
        through BodyBlockWriter; compute SHA-256 again over the spool
        and compare against pass 1 before object commit
```

Spooling is therefore not the default for large uncompressed video
when a stable snapshot exists. It is the safety path for mutable
sources. In both paths, the writer surfaces the same manifest and
catalog data; the choice affects only temporary local I/O.

#### Operational guardrails for large-file hashing/spooling

- **Free-space check before spooling.** If the mutable-source path
  is selected, the writer checks that the temp filesystem has at
  least the file's size plus a configurable margin before pass 1
  starts. It fails fast rather than filling the disk mid-write.
- **Configurable spool directory and cap.** `WriteParams` carries
  `spool_dir: PathBuf` (default `/var/tmp/remanence` or a configured
  scratch path) and `max_spool_bytes: u64` (cap on total concurrent
  spool usage). Exceeding the cap fails before tape I/O.
- **Change detection around non-spooled rereads.** For the immutable
  path, the orchestrator normally supplies `assume_immutable_source`
  because the path is a snapshot. If the caller does not assert
  immutability, the writer records `(size, mtime, inode)` at pass 1
  and checks it again before pass 2 and at final close. A change
  fails with `SourceChangedDuringWrite`. The second SHA-256 check is
  still the source of truth.
- **Pass-2 rehash is mandatory.** Both paths compute the SHA-256 of
  the bytes actually emitted to the BodyBlockWriter and compare it
  with the pass-1 `file_sha256`. A mismatch aborts the object before
  catalog commit. `stat()` checks are useful heuristics, not a
  replacement for content verification.
- **Spool cleanup.** Spool files are deleted after successful object
  commit. On write failure they are deleted unless an operator debug
  flag asks to retain them. A retained spool is never considered
  committed archive state; it is only a diagnostic artifact.

### 9.3 No seek-back

The writer emits tape blocks in strict order. There is no
seek-back-and-rewrite. The pax header before each file's data
records only `file_sha256` and `chunk_count` (plus standard pax
keys); the writer knows both before writing. The manifest is
computed in memory as files are written and emitted at object
close.

This satisfies the LTO append-only constraint and aligns with
3a's write semantics.

### 9.4 Metadata preservation policy

What filesystem metadata gets preserved is controlled by the
`MetadataPreservation` field in `WriteParams`:

```rust
pub enum MetadataPreservation {
    /// Default. Preserve metadata meaningful for cross-system,
    /// cross-time restore: path, content, mtime, the executable
    /// bit, and xattrs. Omit uid/gid/uname/gname and
    /// non-executable mode bits.
    Archival,

    /// Preserve all POSIX metadata as recorded on the source:
    /// full mode bits, uid, gid, uname, gname, mtime, xattrs.
    /// Use for backup-style archives where the restore target
    /// is the same OS and user environment as the source.
    Full,

    /// Content only. Preserve path, content, hashes, chunk
    /// structure. Omit mtime, executable bit, xattrs. Use for
    /// pure content preservation where filesystem metadata is
    /// irrelevant.
    Minimal,
}

impl Default for MetadataPreservation {
    fn default() -> Self {
        MetadataPreservation::Archival
    }
}
```

#### Rationale

`rem-tar-v1` is a long-term archive format, not a backup tool.
The dominant use case is restoring content to a *different*
system, often *years* later. In that context, most POSIX
ownership and permission metadata is not just useless but
actively misleading:

- **uid/gid** are numeric IDs meaningful only within one
  system's `/etc/passwd` and `/etc/group`. On a different
  system (or the same system years later), `uid=1042` belongs
  to a different user or to nobody. Restoring files with these
  IDs produces wrong ownership, not preserved ownership.
- **uname/gname** are slightly more durable (the string
  "sangeetha_editor" carries some meaning) but still don't
  map to anything on the restore system, and proper archive
  provenance lives in the manifest header
  (`caller_object_id`, `writer_version`, `write_timestamp`)
  anyway.
- **mode bits** (read/write/group/other permissions) are
  restore-time policy, not archive content. A restored video
  project's files should be readable by whoever is doing the
  restore — forcing the source's `0640` with a now-meaningless
  group is counterproductive. The one exception is the
  **executable bit**, which carries real semantic meaning
  (a script in a project folder needs to stay executable),
  so `Archival` preserves it specifically.
- **mtime** IS meaningful and durable: "when was this clip
  captured/edited" is real provenance, self-interpreting (no
  external mapping needed), and used by NLEs and asset
  managers. `Archival` preserves it.
- **xattrs** are app-specific (e.g., macOS Finder labels,
  color tags, comments) and may carry editor-meaningful
  metadata. `Archival` preserves them best-effort; restore
  applies them where the target filesystem supports them.

Preserving ownership/permission metadata that won't transfer
faithfully creates a *false sense of fidelity*: the data is
stored, implying it'll be honored, but cross-system restore
can't honor it. Worse, it pushes a policy decision onto the
restore operator ("should I apply uid=1042?") that the format
can't help them resolve.

The `Full` mode exists for the backup-style case (restore to
the same system/user environment soon after archive), where
the metadata genuinely round-trips. It's an explicit opt-in,
not the default, because it's the minority use case for an
archive format.

#### What each mode writes

| Field | Minimal | Archival (default) | Full |
|--|--|--|--|
| path | ✓ | ✓ | ✓ |
| content + chunks | ✓ | ✓ | ✓ |
| file_sha256 | ✓ | ✓ | ✓ |
| chunk CRCs | ✓ | ✓ | ✓ |
| mtime | — | ✓ | ✓ |
| executable bit | — | ✓ | ✓ |
| xattrs | — | ✓ | ✓ |
| full mode bits | — | — | ✓ |
| uid / gid | — | — | ✓ |
| uname / gname | — | — | ✓ |

The choice is per-object, recorded in the manifest header's
`metadata_preservation` field. Symlink entries follow the same
policy (their mtime/mode/ownership fields are present or absent
per the selected mode).

#### Restore behavior

The reader applies only the metadata present in the manifest,
according to the recorded preservation mode. It never
synthesizes missing metadata or applies defaults that imply
preservation. If a file was archived in `Archival` mode, the
restore tool sets mtime and the executable bit, applies xattrs
best-effort, and leaves ownership/permissions to the restore
system's defaults (typically: owned by the restoring user,
default umask permissions).

---

## 10. Read workflows

### 10.1 Catalog model assumed

These workflows assume the Layer 3b catalog carries
**per-file rows**, not just per-object entries. Each catalog
row includes:

| Field | Description |
|--|--|
| `file_id` | UUID of the file |
| `object_id` | Containing object's UUID |
| `tape_id` | Containing tape's UUID |
| `tape_file_number` | Which tape file (filemark-delimited) holds the containing object (§5.1) |
| `path` | Filesystem path within the object |
| `size` | File size in bytes |
| `first_chunk_lba` | **Per-object BodyLba** of the file's first chunk (§2.1) |
| `chunk_count` | Number of chunks for this file |
| `chunk_size` | Chunk size used for this object (typically the tape's chunk_size) |
| `file_sha256` | SHA-256 of the full file content |
| `mtime` | Modification time |
| `compression` | reserved; always `none` in v1 (§6.2) |

This enables direct positioning to any file's first chunk in
one catalog lookup, without reading the manifest: the address
is `(tape_file_number, first_chunk_lba)` where
`first_chunk_lba` is a per-object BodyLba (§2.1). 3c resolves
this to a physical tape position (via its filemark map) at read
time. The schema is owned by Layer 3b; the addition of per-file
rows plus `tape_file_number` is tracked as a separate 3b
update. The rem-tar-v1 writer surfaces the tape_file_number
and per-file BodyLba data at object close (§9 step 8) so
Layer 5 can populate the catalog rows atomically with the
object entry.

### 10.2 Full-object read (Tier 0)

```
Given: object catalog entry with tape_file_number.

1. Position to the start of tape file `tape_file_number` (3c
   resolves this via its filemark map; conceptually an
   `mt fsf` to the right tape file). The object archive is a
   clean, contiguous pax tar stream in per-object BodyLba —
   there are no parity blocks inside the archive to skip
   (parity lives in separate sidecar tape files, §5.1), so the
   bytes are directly a valid tar archive.
2. Open a streaming tar reader on the object's block stream.
3. Read entries sequentially. For each regular file entry:
   - Extract file_id, path, size from headers.
   - Read the file's data (trimming the final logical chunk to
     the exact size; no reliance on block zero-padding) into
     the output stream.
4. When the tar EOF is reached (and the terminating filemark),
   the object is fully read.
```

This is the slowest mode of access but the most format-
compatible. Any pax-aware tar tool can read tape file N
directly (`mt fsf N; tar xf /dev/nst0`) and extract the object,
because each object is a standalone filemark-delimited pax tar
archive with no parity blocks interleaved.

### 10.3 Single-file read (Tier 1)

```
Given: catalog row for the requested file_id (carries
       tape_file_number and per-object first_chunk_lba).

1. Position to (tape_file_number, first_chunk_lba): seek to the
   object's tape file, then to the file's first chunk BodyLba
   within it. (3c resolves to physical position via its
   filemark map.)
2. Read catalog.chunk_count chunks. For each:
   - The chunk is raw file bytes (no compression — §6.2). The
     final chunk is the file's tail (shorter than chunk_size);
     trim it to the file size.
   - Stream the chunk bytes to the output.
3. The file is complete after chunk_count chunks read. The
   running SHA-256 of the emitted bytes is compared against
   catalog.file_sha256.
```

Typical Tier 1 access cost: one seek to the first chunk, then
sequential reads of chunk_count chunks. No manifest read in
the common case (verification at file granularity is
sufficient). Total overhead vs. actual file data: one seek.

The manifest is genuinely not needed for a Tier 1 read: each
chunk is raw file bytes (the last trimmed by file size) and
the file_sha256 verifies the whole file at end. The manifest
is consulted for catalog reconstruction and for any future
per-chunk integrity feature once that deferred feature is
specified.

### 10.4 Byte-range read (Tier 2)

```
Given: catalog row for the requested file_id (tape_file_number,
       per-object first_chunk_lba), and [start_byte, end_byte).

1. Compute:
   - first_chunk = start_byte / chunk_size
   - last_chunk = (end_byte - 1) / chunk_size
   - chunk_body_lba = catalog.first_chunk_lba + first_chunk
   - chunk_count_to_read = last_chunk - first_chunk + 1
   - head_drop = start_byte % chunk_size
   - tail_keep_end = ((end_byte - 1) % chunk_size) + 1

2. Position to (tape_file_number, chunk_body_lba).
3. For i in 0..chunk_count_to_read:
   - Read one chunk block (raw file bytes — §6.2).
   - If i == 0: skip the first `head_drop` bytes.
   - If i == chunk_count_to_read - 1: truncate the trailing
     bytes to end at offset `tail_keep_end` within the chunk
     (if first_chunk == last_chunk, only the bytes between
     head_drop and tail_keep_end are emitted).
   - Emit the resulting bytes to the output.
```

Tier 2 cost: one seek plus exactly `last_chunk - first_chunk
+ 1` block reads. Identical seek structure as Tier 1; the
only difference is reading K chunks instead of N. There is no
threshold below which whole-file reads become preferable —
both paths have the same single-seek cost.

**Multi-file restore batching.** When the caller has multiple
byte-range or whole-file requests, the orchestrator sorts them
by `(tape_id, tape_file_number, chunk_body_lba)` and dispatches
them as a sequence of forward seeks. This minimizes total
backward seek distance across the restore batch. For a typical
restore of 50-100 files from a single tape, this can reduce
total restore time by an order of magnitude vs. unsorted
access. The batching happens at Layer 5, not in the format
implementation.

### 10.5 Object manifest read (verification + reconciliation)

```
Given: object catalog row with tape_file_number,
       manifest_first_chunk_lba, manifest_size_bytes,
       manifest_chunk_count, manifest_sha256 (all from the
       catalog — review v0.8.1 #2/#4).

1. Position to (tape_file_number, manifest_first_chunk_lba).
2. Read manifest_chunk_count BodyLba blocks. (The reader knows
   the count from the catalog; it does NOT rely on the
   manifest's pax header, which sits *before* this position and
   is therefore behind the reader after the direct seek.)
3. Trim the read bytes to manifest_size_bytes.
4. Verify SHA-256 of those bytes against manifest_sha256 before
   trusting any contents (§8.3 trust chain).
5. Parse the canonical CBOR ObjectManifest and return it.
```

Note: `manifest_first_chunk_lba` points at the manifest's
*data*, block-aligned like any file (§5.2). The manifest's own
pax header precedes that block, so a reader seeking directly to
the data cannot read the pax header for sizing — which is why
`manifest_size_bytes` and `manifest_chunk_count` are carried in
the catalog. (A reader doing a sequential Tier 0 pass *does*
see the pax header and could use it; the direct-seek path
above does not.)

Used in three scenarios:

1. **Catalog reconciliation.** The orchestrator periodically
   checks on-tape state against its catalog by reading
   manifests and comparing.
2. **Deferred per-chunk verification.** Older drafts placed
   per-chunk CRC-64 codes in the manifest. The current v1
   manifest does not; this access mode needs a future
   minor-format design update before it is implementable.
3. **Catalog reconstruction.** If the catalog is lost or
   damaged, the manifest contains complete file and chunk
   information needed to rebuild catalog rows for the object.

### 10.6 Verification policy

Current strictness levels, selected by the caller:

- **Per-file SHA-256 only** (default for full-file reads).
  The reader streams chunks and computes a running SHA-256
  of the emitted bytes, comparing against `catalog.file_sha256`
  at end. Cryptographic; detects any whole-file corruption
  including adversarial modification. Requires reading the
  full file to verify.
- **Deferred per-chunk verification.** Older drafts made this
  the default for byte-range reads, but the current manifest has
  no `chunk_crcs` field. Byte-range reads can still return the
  requested bytes, but cannot claim chunk-level integrity until
  this deferred feature is designed and written to tape.
- **None** (operator override). Skip all verification. Used
  only for emergency reads where speed matters more than
  integrity (e.g., last-chance recovery from a failing tape).

Full-file reads default to per-file SHA-256 verification (no
manifest read needed; cryptographic guarantee at file
granularity). Partial byte-range reads cannot validate the
whole-file hash unless the caller also reads the full file, and
per-chunk verification is deferred.

**On encrypted tapes** (Layer 6 with AES-GCM), each tape
block carries a 128-bit auth tag that catches both random
and adversarial modification at block granularity. The
deferred per-chunk CRC layer would be redundant for those tapes
if it is reintroduced later.

**Trust chain summary** (echoing §8.3):
```
catalog (trusted)
   ↓ records file_sha256 & manifest_sha256
file_sha256 (verifies full-file reads, cryptographic)
manifest_sha256 (verifies manifest before use, cryptographic)
deferred: chunk CRC-64 for partial-read verification
```

### 10.7 Catalog-corrupt forward scan

If the catalog is unreadable but the parity layer can still
read tape blocks, a forward scan recovers the object:

```
Given: a tape file (filemark-delimited) believed to contain
       an object archive, found by sequential filemark scan
       (mt fsf / fsr) of bare tape when the catalog is gone.

1. Position to the start of the candidate tape file.
2. Open a tar reader on it (it's a clean pax tar stream — no
   parity inside the object, §5.1; parity sidecar tape files
   are separate and are skipped by this scan).
3. Read entries sequentially. For each typeflag-'g' header:
   - Validate REMANENCE.format_id == "rem-tar-v1".
   - Extract object metadata.
4. For each typeflag-'x' header followed by a regular entry:
   - typeflag '0' (regular file): extract metadata from pax
     keywords. Record first_chunk_lba (the BodyLba where the
     file's data begins) for catalog reconstruction. Advance
     in **tar terms**: skip `tar_header.size` bytes of payload
     plus the normal 512-byte-record padding to reach the next
     header. Do NOT advance by `chunk_count * chunk_size` — the
     final chunk is the file's tail and is not padded inside
     the payload (§5.2), and the next header may begin in the
     same block. (`tar_header.size` equals the file's logical
     size, since there is no compression — §6.2.)
   - typeflag '1' (hard link): extract `linkname`; record a
     hardlink entry. No data follows.
   - typeflag '2' (symbolic link): extract pax keywords and
     `linkname`. Record symlink entry. No data follows.
   - typeflag '5' (directory): record directory metadata.
     No data follows.
5. The tar EOF terminates the object; the terminating filemark
   confirms the object's tape file boundary.
```

This recovery path is slower than catalog-guided reads (it
must read all pax headers sequentially) but gives complete
results without external state. Standard tar tools can do
this too, though they won't expose the REMANENCE.* keys to
the user without specific support.

### 10.8 Restore behavior for symlinks

**Restore never fails because a symlink target is missing on
the target filesystem.** This is an architectural invariant
of rem-tar-v1.

#### Restore behavior

The restore tool MUST:

1. **Re-create each symlink with its original target string
   exactly as recorded in the archive.** Use the OS's
   `symlink()` (or equivalent) system call with the target
   string verbatim — no resolution, no normalization, no
   substitution.
2. **NOT attempt to verify that targets resolve.** No `stat`
   on `readlink` results; no checks for target existence.
3. **NOT skip symlinks whose targets can't be resolved.**
   Every symlink in the archive must be re-created on
   extraction.
4. **NOT silently rewrite target paths.** If the operator
   wants to retarget symlinks (e.g., to point to a different
   mount point), that is a post-extraction step, not a
   restore-time decision.

The restore tool MAY (and SHOULD):

1. **Display a pre-extraction summary of external references**
   (from the manifest's `ExternalReferences` array) so the
   operator knows what dependencies the archive expects.
   Example:

   ```
   Restoring object show01-final.
   This archive contains 47 external symlink references:
     - audio_master.wav → /raid/shared_audio/2026/show01/audio_master.wav
     - drum_stems/01.wav → /raid/shared_audio/2026/show01/drum_stems/01.wav
     ... [42 more]

   These links will be restored as-is. Their targets are NOT
   part of this archive. If the targets are required, ensure
   they're available at the listed paths before opening the
   project.

   Proceed? [y/N]
   ```

2. **Generate a post-restore "external dependencies report"**
   listing which symlinks would dangle if extracted now,
   based on the target filesystem state at that moment.
   This is informational only; the restore proceeds either
   way.

3. **Offer a `--check-symlinks` post-extraction option** that
   walks the restored tree and reports which symlinks resolve
   and which dangle. Again purely informational.

#### Why restore must not fail on dangling symlinks

If restore failed on dangling symlinks, the entire editor →
staging → archive → restore workflow would be broken at the
restore step. External references are the *whole point* of
many video project archives — preserving the structure while
trusting external assets to be maintained elsewhere.

A dangling symlink at restore time is **not an error**. It is
the format's correct, intentional behavior: preserving the
project's structure as the editor designed it. The targets
are managed by some other system (asset library, shared
storage, secondary archives), and ensuring they're available
at restore time is the operator's responsibility — not
something the restore tool can decide unilaterally.

#### Operator workflow

The typical restore-to-edit workflow:

1. Operator initiates restore of object `show01-final`.
2. Restore tool reads manifest; displays external reference
   summary (47 references to `/raid/shared_audio/...`).
3. Operator verifies the target paths are mounted (e.g., the
   shared audio array is online).
4. Operator approves restore; tool extracts files and recreates
   symlinks.
5. Restored project opens correctly because symlinks resolve
   to the mounted shared paths.

If step 3 fails (target paths not mounted), the operator can:

- Mount the missing paths before restoring.
- Restore the related shared-asset archive first.
- Proceed anyway and accept that some links will dangle until
  the targets are made available.

In all cases, **the restore itself succeeds**. The operator's
decision space stays open.

---

## 11. Capability advertisement

`rem-tar-v1` implements the 3b `TapeFormat` trait with:

```rust
fn capabilities(&self) -> Capabilities {
    Capabilities::TIER_0
        | Capabilities::TIER_1
        | Capabilities::TIER_2
        | Capabilities::VERIFIABLE
        | Capabilities::METADATA_PRESERVING
    // No COMPRESSED — format-level compression removed (§6.2).
}

fn as_file_addressable(&self) -> Option<&dyn FileAddressable> { Some(self) }
fn as_byte_range_addressable(&self) -> Option<&dyn ByteRangeAddressable> { Some(self) }
fn as_verifiable(&self) -> Option<&dyn Verifiable> { Some(self) }
```

Tier 0/1/2: covered by §10.2-§10.4.

VERIFIABLE: a layered integrity model.

- **File-level (cryptographic):** `REMANENCE.file_sha256` in
  pax headers and in the manifest. Also recorded in the
  catalog's per-file row. Verified after a full-file read by
  computing a running SHA-256 of the emitted bytes.
- **Manifest-level (cryptographic):** the manifest's own
  SHA-256 is recorded in the catalog. Verified after a
  manifest read before any manifest-only metadata is trusted.
- **Deferred per-chunk (error-detection):** older drafts used
  CRC-64/XZ codes in a manifest `chunk_crcs` byte blob. The
  current v1 manifest does not write that field; chunk-level
  verification requires a future minor-format update.

Hash mismatch handling:
- A file-level SHA-256 mismatch indicates corrupted bytes, bytes
  delivered in the wrong order, or tampering. Always propagate it;
  automatic single-block recovery based on CRC mismatch is deferred
  with the per-chunk CRC feature.

Tamper detection at file granularity comes from the
cryptographic file hash anchored in the catalog. The
deferred per-chunk CRC layer would be for catching
random/software errors on partial reads where the full file hash
can't be checked.

For encrypted tapes (Layer 6 with AES-GCM), per-block auth
tags provide additional tamper detection independent of this
layer.

METADATA_PRESERVING: rem-tar-v1 preserves filesystem metadata
at the level chosen by `MetadataPreservation` (§9.4). The
capability is advertised as "best effort, tier-dependent" —
NOT as a promise of full POSIX-state reproduction across
systems.

- **Default (`Archival`):** preserves what's meaningful across
  systems and time — path, content, mtime, the executable bit,
  and xattrs. Deliberately omits uid/gid/uname/gname and
  non-executable mode bits, because these don't transfer
  faithfully across systems and create false fidelity
  expectations at restore time.
- **`Full`:** preserves all POSIX metadata (full mode bits,
  uid, gid, uname, gname, mtime, xattrs). For backup-style
  archives where the restore target matches the source
  environment.
- **`Minimal`:** content only.

The manifest's `metadata_preservation` field records the mode.
Readers apply only the metadata present and never synthesize
missing fields. Cross-system restores under `Archival` get
content + mtime + exec bit + xattrs; ownership and permissions
fall back to the restore system's defaults. This is the honest
behavior for a long-term archive — see §9.4 for the full
rationale and the §16 open question on why full POSIX
preservation is not the default.

COMPRESSED: **not advertised.** rem-tar-v1 does not compress
data (§6.2); compression is an orchestrator-level function.
The `REMANENCE.compression` field exists but is a reserved
enum that only takes `none` in v1.

`seek_granularity(object: &ObjectInfo)`: returns
`object.format_options["chunk_size"]`, which is the
`REMANENCE.chunk_size` value from the global header. At the
default chunk_size of 256 KiB, byte-range reads land within
256 KiB of the requested boundary.

Not advertised:

- **APPENDABLE**: not supported. An object is closed by tar
  EOF; appending would invalidate the EOF and the manifest.
- **RESUMABLE_WRITE**: not supported in v1. An interrupted
  write produces an unrecoverable partial object; the
  orchestrator must retry from the start.
- **SPARSE_PRESERVING**: not supported in v1. Sparse files are
  stored fully. Future v1.x extension via the standard pax
  `GNU.sparse.*` keys could add this.
- **ENCRYPTED**: not strictly a format capability; encryption
  is per-tape (Layer 6), applied to all blocks uniformly. The
  format records `REMANENCE.encryption` for documentation but
  doesn't manage keys.

---

## 12. Tar tool compatibility

A standard pax-aware tar tool extracting a `rem-tar-v1` object
sees:

1. A global pax header at the start with REMANENCE.* keywords
   (which the tool either ignores or stores as a `PaxHeaders`
   directory, depending on the tool's behavior).
2. Per-file pax extended headers that may carry a
   `REMANENCE.pad` record (used to size the header so the
   following file's data starts on a block boundary — §5.2).
   This is a normal pax header for the next file, not a
   standalone padding member; tools apply its standard keys to
   that file and ignore the vendor `REMANENCE.*` keys.
3. Per-file pax extended headers with both standard keys
   (`path`, `size`, `mtime`, etc.) and `REMANENCE.*` extensions.
   Standard keys are applied to the following file entry as
   expected; REMANENCE.* keys are typically preserved as
   sidecar metadata.
4. Regular file entries with the actual file data. File-data
   *starts* are block-aligned (so byte-range works), but file
   data is written at its exact size with only tar's normal
   512-byte-record padding — never zero-padded to a block
   (§5.2). Standard tar extracts byte-exact content for
   uncompressed files.
5. The object manifest as a regular file at path
   `_remanence/manifest.cbor`. Tools extract it as a regular
   file in a subdirectory.
6. The tar EOF (two zero records). Standard tools recognize
   this and stop reading.

A user can:

- Extract files by seeking to the object's tape file and
  running standard tar with the matching blocking factor:
  `mt -f /dev/nst0 fsf N; tar -b 512 -xf /dev/nst0`. The
  `-b 512` matches the 256 KiB fixed block size (512 × 512 B =
  262144 B); without it GNU tar's default blocking factor may
  not match the drive's fixed block size. Because each object
  is its own filemark-delimited pax tar tape file with **no
  parity blocks interleaved** (parity lives in separate sidecar
  tape files, §5.1) and **no format-level compression** (§6.2),
  the tape file is directly a valid tar archive — no
  "unwrapping" and no codec needed. All file data is recovered
  byte-exact with standard pax metadata applied. This holds
  unconditionally; there is no compressed-file exception.
- Verify files against the REMANENCE.file_sha256 keys (if
  extracted as sidecar metadata by the tool) for integrity
  checking.
- Inspect the object metadata via the `_remanence/manifest.cbor`
  file (decode with any CBOR tool such as `cbor-diag`).

Parity sidecar tape files appear between object tape files. A
user doing plain `tar` extraction of a specific object's tape
file simply ignores the sidecars (they're separate tape files,
skipped by `mt fsf` to the object of interest). The parity
sidecars are only consulted by a Remanence-aware reader during
recovery of a damaged object.

The user cannot easily do byte-range restore without a
Remanence-aware tool, because computing chunk positions
requires the catalog and positional math standard tools don't
expose. This is an acceptable trade-off: the 30-year fallback
path is "seek to the tape file, extract everything with tar,"
not "byte-range restore in 2056 without Remanence."

### 12.1 Worked example: extracting with GNU tar

Each object is a standalone filemark-delimited pax tar tape
file. A user with `tar` and `mt` can extract object in tape
file N directly — no parity unwrapping needed, because parity
is in separate sidecar tape files, not inside the object:

```
$ mt -f /dev/nst0 rewind
$ mt -f /dev/nst0 fsf N        # seek to the object's tape file
$ tar -b 512 -xf /dev/nst0    # -b 512 matches 256 KiB blocks (512×512B)
$ ls
file1.mxf  file2.mxf  ...  _remanence/
$ ls _remanence/
manifest.cbor
$ cbor-diag _remanence/manifest.cbor | head
{ 1: { 1: "rem-tar-v1", 2: "1.0", 3: h'...', ... } ... }
```

The files are recovered with their original names, sizes, and
mtimes (uncompressed files byte-exact). The
`_remanence/manifest.cbor` is available as a sidecar; tools
that don't understand it leave it alone. If the object's tape
file has damaged blocks, plain `tar` will fail on those blocks;
a Remanence-aware reader uses the parity sidecar tape files to
reconstruct them (§13.2) — but the common, undamaged case
needs no Remanence tooling at all.

---

## 13. Error model

### 13.1 Write-side: BeginWriteError

The writer's primary entry point returns this type. The
critical distinction is `Preflight` (no tape I/O has occurred;
fixable by the operator) vs. `Tape` (partial tape state may
exist; recovery is operator-coordinated).

```rust
#[derive(Debug, thiserror::Error)]
pub enum BeginWriteError {
    #[error("preflight validation failed for {count} files",
            count = .0.failures.len())]
    Preflight(PreflightError),

    #[error("tape I/O error: {0}")]
    Tape(#[from] RemTarError),
}

#[derive(Debug)]
pub struct PreflightError {
    pub failures: Vec<PathValidationFailure>,
}

#[derive(Debug)]
pub struct PathValidationFailure {
    pub source_path: PathBuf,             // best-effort representation
    pub raw_bytes: Vec<u8>,               // exact source filesystem bytes
    pub reason: ValidationReason,
}

#[derive(Debug)]
pub enum ValidationReason {
    NotValidUtf8 { error_offset: usize, bad_bytes: Vec<u8> },
    ContainsNul { offset: usize },
    TooLong { length: usize, limit: usize },
    AbsolutePath,
    PathTraversal { component_index: usize },
    InvalidXattrKey { key_bytes: Vec<u8> },
    InvalidXattrValue { key: String, value_bytes: Vec<u8> },
    DuplicatePath { other_index: usize },
    NotReadable { io_error_kind: std::io::ErrorKind },
    SizeMismatch { expected: u64, actual: u64 },

    // Symlink-specific variants (see §5.8):
    SymlinkTargetNotUtf8 { error_offset: usize, bad_bytes: Vec<u8> },
    SymlinkTargetEmpty,
    SymlinkTargetTooLong { length: usize, limit: usize },
    InternallyBrokenSymlink {
        symlink_path: PathBuf,
        target_string: String,
        resolved_path_in_archive: PathBuf,
    },
    SymlinkRejectedByStrictPolicy {
        symlink_path: PathBuf,
        target_string: String,
        classification: SymlinkClassification,
    },

    // Input-list sanity variants (see §9.0):
    ExcessiveEntryCount {
        count: usize,
        limit: usize,
    },
    ExcessiveInodeDuplication {
        inode: (u64, u64),
        occurrence_count: usize,
        limit: usize,
        sample_paths: Vec<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum SymlinkClassification {
    Internal,
    ExternalAbsolute,
    ExternalRelative,
    InternallyBroken,
}
```

Handling:

- **`Preflight`**: no tape I/O occurred. Return the
  `PreflightError` to the orchestrator unchanged. Operators
  use the full failure list to fix paths, then retry.
- **`Tape`**: tape may be in a partial state. The writer's
  audit hook records the LBA at which the failure occurred.
  Recovery depends on what was written so far (no object close
  yet → tape position has incomplete data that should be
  marked as such in the catalog).

### 13.2 Read-side: RemTarError

```rust
#[derive(Debug, thiserror::Error)]
pub enum RemTarError {
    #[error("format error: {0}")]
    Format(#[from] FormatError),

    #[error("parity error: {0}")]
    Parity(#[from] ParityError),

    #[error("malformed tar entry at {at_body_lba} (object-local BodyLba): {detail}")]
    MalformedTar { at_body_lba: u64, detail: String },

    #[error("expected pax extended header but got typeflag {got:?} at BodyLba {at_body_lba}")]
    MissingPaxHeader { at_body_lba: u64, got: u8 },

    #[error("required REMANENCE.* keyword missing: {keyword}")]
    MissingKeyword { keyword: &'static str },

    #[error("unsupported schema version {got}; this reader supports up to {supported}")]
    UnsupportedSchemaVersion { got: String, supported: &'static str },

    #[error("chunk crc failure: chunk {chunk_index} of file {file_id:?}: expected {expected:016x}, got {actual:016x}")]
    ChunkCrcFailure { file_id: [u8; 16], chunk_index: u32, expected: u64, actual: u64 },

    #[error("file integrity failure: file {file_id:?}: expected sha256 {expected}, got {actual}")]
    FileIntegrityFailure { file_id: [u8; 16], expected: String, actual: String },

    #[error("manifest integrity failure: expected sha256 {expected}, got {actual}")]
    ManifestIntegrityFailure { expected: String, actual: String },

    #[error("byte range out of bounds: file size is {file_size}, requested [{start}, {end})")]
    ByteRangeOutOfBounds { file_size: u64, start: u64, end: u64 },

    #[error("manifest parse error: {0}")]
    ManifestParse(String),

    #[error("file not found in object: {0:?}")]
    FileNotFound([u8; 16]),

    #[error("string encoding error at BodyLba {at_body_lba}: field {field} contains invalid UTF-8")]
    InvalidUtf8OnTape { at_body_lba: u64, field: &'static str },

    #[error("unsupported feature: {feature} = {value} (this reader supports only the v1 default)")]
    UnsupportedFeature { feature: &'static str, value: String },
}
```

Mapping to higher-level errors:

- `MalformedTar`, `MissingPaxHeader`, `MissingKeyword`,
  `ManifestParse`: structural errors in the on-tape data.
  Bubble up as `FormatError::CorruptHeader` to Layer 5.
- `ChunkCrcFailure`: deferred with the per-chunk CRC feature.
  Older drafts used this for the **clean-read-but-CRC-mismatch**
  case: 3c's read returned bytes without a SCSI error, but those
  bytes failed a higher-level CRC. If per-chunk CRCs are
  reintroduced, forced recovery should remain object-scoped on
  `ObjectParitySource`, which already knows `tape_file_number`:

  ```rust
  // On ObjectParitySource (3c v0.4.2 §6.2, §8.3):
  fn recover_block_at(&mut self, body_lba: u64)
      -> Result<Vec<u8>, ParityError>;

  // deferred rem-tar usage on a CRC mismatch:
  let mut obj = parity_source.open_object(
                    tape_file_number, OpenTrust::RequireValidated)?;
  obj.locate(chunk_body_lba)?;
  let repaired = obj.recover_block_at(chunk_body_lba)?;
  ```

  That API treats the block at `body_lba` as an *erasure*
  (even though it read cleanly) and reconstructs it from the
  surrounding stripe's parity.
- `FileIntegrityFailure`: a file's full content doesn't match
  its SHA-256. Indicates corrupted bytes, chunks delivered in
  the wrong order, a software bug in the read path, or deliberate
  tampering. Always propagated; recovery is not automatic.
- `ManifestIntegrityFailure`: the manifest's SHA-256 doesn't
  match the catalog's record. The manifest is not trustworthy;
  fall back to file-level verification only, or refuse to
  proceed depending on operator policy. Surfaced for security
  audit.
- `UnsupportedSchemaVersion`: the reader needs to be upgraded.
  Operators see a clear message.
- `FileNotFound`, `ByteRangeOutOfBounds`: caller-side errors;
  return to Layer 5 as-is.
- `InvalidUtf8OnTape`: a pax keyword or manifest field on
  tape contains non-UTF-8 bytes. This should be impossible
  for tapes written by `rem-tar-v1` (the writer enforces
  UTF-8 via pre-write validation). If encountered, indicates
  either a third-party tar archive being read as rem-tar-v1
  (genuine format mismatch) or rare tape corruption that
  changed the bytes. Treated as `FormatError::CorruptHeader`.
- `UnsupportedFeature`: a reserved field carries a non-v1
  value — in practice `REMANENCE.compression` (or the manifest
  `compression`) set to anything but `none` (§6.2). A v1 reader
  cannot interpret it; surfaced so an operator knows a
  newer-format-version reader is required. Should be
  unreachable from a v1 writer.

---

## 14. Implementation plan

Step numbering picks up after the 3b plan (steps 10.0-10.15).
Each step ends with `cargo fmt && cargo clippy --workspace
--all-targets -- -D warnings && cargo test --workspace &&
cargo doc --workspace --no-deps`, all green.

Steps assume 3b's trait skeleton (steps 10.0-10.3 of the 3b
plan) and 3c (steps 11.0-11.18) are complete. **They also
assume the 3c design revision of §16.14 is done**: `BlockSink`/
`BlockSource` operate in per-object `BodyLba` paired with
`tape_file_number`; the sink writes each object as a clean pax
tar tape file and writes a terminating filemark on
`finish_object()`; parity epochs span objects via
ParityDataOrdinal with sidecar tape files; `recover_block_at`
exists; default parity geometry is block-size-aware (S=512 at
256 KiB). rem-tar-v1 is written against this interface; if 3c
isn't yet updated, steps 12.0.5 onward can proceed against a
mock `BlockSink`/`BlockSource` implementing the per-object
BodyLba contract (write blocks → contiguous tar tape file,
`finish_object()` → filemark), with live-3c integration gated
until 3c lands the changes. Workspace dependencies added:
`tar` (pax tar parsing), `rayon` (parallel pre-write validation),
and `sha2` (file/manifest hashes). Do not add `crc64fast` for
current v1 manifests; per-chunk CRC-64 is deferred as noted in
§8.1.
No `zstd` dependency — format-level compression was removed in
v0.8 (§6.2). The existing `ciborium` dependency
from 3b is reused for the manifest, configured for canonical
encoding.

| Step | Description |
|--|--|
| 12.0 | Module skeleton: `crates/remanence-format/src/formats/rem_tar_v1/`. Submodules: `mod.rs` (public types + `TapeFormat` impl), `header.rs` (pax keyword parsing/encoding), `manifest.rs` (CBOR manifest read/write), `validate.rs` (pre-write validation), `block_writer.rs` (BodyBlockWriter, §9.1.1), `write.rs` (writer state machine), `read.rs` (Tier 0/1/2 readers). Stub everything. Workspace deps: `tar`, `ciborium`, `sha2`, `rayon`, `thiserror`. Do not add `crc.rs`/`crc64fast` unless the deferred per-chunk CRC feature is reintroduced by a minor-format design update. (No `zstd` — format-level compression removed, §6.2.) |
| 12.0.5 | Pre-write validation (§9.0). The `PathValidationFailure`, `ValidationReason`, `PreflightError`, `BeginWriteError`, `SymlinkPolicy`, `SymlinkClassification` types. The `validate` function: walks an input file list in parallel via rayon, runs UTF-8 / NUL / length / traversal / xattr / uniqueness / readability / size checks for files, classification + policy enforcement for symlinks, and input-list sanity checks (entry-count limit, inode-duplicate limit) for walker-bug defense. Symlink classification is **textual only** (no `stat` of targets). Returns `Result<Vec<ValidatedEntry>, PreflightError>` where ValidatedEntry distinguishes files vs symlinks. Tests cover: clean input passes; non-UTF-8 path captured with raw bytes; multiple failures collected (not just the first); duplicate paths detected; absolute/traversal paths detected; permission errors detected; symlink classification correct for Internal / ExternalAbsolute / ExternalRelative / InternallyBroken cases without any filesystem stat of targets; `SymlinkPolicy::Default` rejects only InternallyBroken; `SymlinkPolicy::Strict` rejects all non-Internal; `SymlinkPolicy::Permissive` accepts all; **FCP-style cycle-bomb input list (same inode appearing 100+ times under nested `.fcpcache/.fcpcache/...` paths) is rejected with `ExcessiveInodeDuplication`**; entry-count overrun is rejected with `ExcessiveEntryCount`; legitimate hard links (declared explicitly) do not trigger the inode duplicate check. Property tests on the parallel ordering preserving raw_bytes. |
| 12.1 | Pax header keyword codec. Parse and emit `REMANENCE.*` keys per §7.1-§7.4. Standard pax keyword handling delegated to the `tar` crate. Unit tests against fixture pax records. Verify no `REMANENCE.chunks` keyword is emitted or expected. Verify UTF-8 strings are written and parsed correctly; verify non-UTF-8 bytes in tape data trigger `InvalidUtf8OnTape` (defensive — should be unreachable from a rem-tar-v1 writer). Verify symlink classification fields (typeflag '2' entries with `linkname`) parse correctly. |
| 12.2 | Deferred: CRC-64/XZ per-chunk verification was part of the older §8.1 manifest draft, but is not present in the current `spec-v0.4` §8.7.5 manifest implemented by `remanence-format`. Do not add `chunk_crcs` without a new minor-format design update. |
| 12.3 | Object manifest CBOR codec. Use the active `spec-v0.4` §8.7.5 string-keyed schema: object header fields plus `file_entries` carrying `file_id`, `path`, `size_bytes`, `file_sha256`, `first_chunk_lba`, `chunk_count`, `executable`, and `metadata_preservation_data`. **Canonical CBOR required** (§8.1 supersession note): sorted keys by encoded form, definite lengths, smallest ints. A test must verify byte-identical output across encode cycles for a fixture manifest, and compute a stable `manifest_sha256`. Round-trip tests should match the implemented manifest shape; per-chunk CRC blobs are out of scope until the deferred feature is designed. |
| 12.4 | Writer: global header emission. Writes the typeflag-'g' header with REMANENCE.* keywords, pads to chunk_size. Tests verify exact byte layout. |
| 12.5 | Writer: `begin_write` entry point. Calls the validator from step 12.0.5 as its first action; on validation failure returns `BeginWriteError::Preflight` without any tape I/O. On validation success, proceeds to step 12.6's tape writing. Tests verify no bytes are written to a mock `BlockSink` when validation fails. Tests cover both validation-failure-then-bail and validation-success paths. |
| 12.6 | Writer: file entry emission (small-file path). Reads file into memory, computes `file_sha256`, emits pax extended header + ustar header + chunks. **BodyLba-boundary alignment of file-data start via sizing the preceding pax header with a `REMANENCE.pad` record (§5.2) — NO standalone padding members, NO zero-fill after file data, last chunk NOT zero-padded.** Honors `MetadataPreservation` (§9.4): emits mtime/executable-bit/xattrs in archival+full, full mode bits/uid/gid/uname/gname only in full, none of these in minimal. **Sanitizes ustar header fields per §5.10** (uid/gid 0, empty owner names, default modes in minimal/archival). Tests against synthetic files of various sizes; tests verify each preservation mode emits exactly the right field set, that minimal/archival omit ownership fields, that ustar headers carry sanitized values, and that the emitted byte stream has no trailing zero block that a tar reader could mistake for EOF. |
| 12.7 | Writer: large-file path (§9.2). Implement both branches: immutable/snapshot-backed source (pass 1 SHA-256, pass 2 reread source, rehash emitted bytes) and mutable-source spool (pass 1 SHA-256 + spool exact bytes, pass 2 write spool, rehash emitted bytes). Tests with a 2 GiB synthetic file (sparse fixture allowed); tests mutate the source between passes and require `SourceChangedDuringWrite` / SHA mismatch; tests prove pass-2 bytes are the bytes whose hash was placed in the pax header. |
| 12.6.1 | Normative pax-padding solver (§7.6): iterate-to-fixed-point sizing of the `REMANENCE.pad` record so file data lands on a BodyLba boundary, given the self-referential pax `<len>` field. Property tests across many path lengths and resulting pad lengths, **specifically asserting correct alignment when the pad record's `<len>` crosses 9→10, 99→100, and 999→1000 byte digit boundaries** (where a single-pass solver misaligns by one digit). Test the rare overshoot case (header within one record of the boundary → one extra full block). Verify the pad value is inert ASCII spaces and that GNU tar ignores the keyword. |
| 12.7.5 | Writer: symlink entry emission (§9.1 step 4.5). Constructs pax extended header with symlink classification, writes typeflag '2' ustar entry with verbatim target string, appends SymlinkEntry and (if external) ExternalReference to in-progress manifest. No file data; no chunk alignment. Tests verify symlinks with various target strings (internal, external-abs, external-rel) all encode/decode correctly. |
| 12.7.6 | Writer: special files (§5.9). Hard links → first occurrence as regular file, subsequent as typeflag '1' + HardlinkEntry. Directories → DirectoryEntry when preservation mode includes dir metadata. Device/FIFO/socket → rejected at validation by default; emitted as SpecialFileEntry only in `system-backup` mode. Tests cover hard-link dedup, directory metadata round-trip, default rejection of device nodes, and system-backup-mode acceptance. |
| 12.7.7 | `BodyBlockWriter` (§9.1.1): arbitrary tar byte stream in → whole `chunk_size` blocks out → final partial block zero-filled only after tar EOF. Tracks BodyLba (count of full blocks). Tests: a file whose tail doesn't fill a block produces a final data block that is NOT zero-padded inside the payload (next header packs into the same block); the only zero-fill is the post-EOF tail of the object's final block; no short fixed-block write is ever emitted mid-stream; BodyLba reported for a file's first data block matches the block where its data starts after pax-header sizing. |
| 12.8 | Writer: object close. Serializes the manifest as canonical CBOR, computes `manifest_sha256` over those bytes (the hash is NOT a field inside the manifest — it lives in the manifest's pax header and the catalog, avoiding circularity), emits manifest pax header + ustar header + manifest blocks, then the tar EOF records. Calls `BodyBlockWriter::finish_after_tar_eof()` to flush the zero-filled final block (rem-tar owns this — §9.1.1), then `ParitySink::finish_object()` for the filemark + sidecars (3c owns that — review #4). Tests verify the complete on-tape byte sequence for a small fixture object including symlinks and external references; verify per-file/per-symlink row data (with empty-file `first_chunk_lba` absent) is correctly surfaced; verify exactly one final-block flush occurs. |
| 12.9 | Reader: Tier 0 (full-object). Walks the tar stream sequentially; emits file entries with their data, recreates symlinks via `std::os::unix::fs::symlink`, and applies metadata per the manifest's recorded preservation mode (mtime + exec bit + xattrs for archival; full ownership/mode for full; nothing for minimal). The reader never synthesizes missing metadata. **Tests must verify that restore succeeds even when symlink targets don't exist on the test system, and that ownership fields absent in archival/minimal mode are not applied (files owned by restoring user, default umask).** Tests against fixtures written by step 12.8. |
| 12.10 | Reader: manifest load + verification. Position to (tape_file_number, manifest_first_chunk_lba), read, parse, verify manifest_sha256 against the catalog's recorded value before trusting contents. Tests verify correct parsing and integrity check, including manifests with SymlinkEntry, ExternalReference, HardlinkEntry, DirectoryEntry, and SpecialFileEntry arrays, and verify canonical-CBOR byte-identical round-trip. |
| 12.11 | Reader: Tier 1 (single-file). Uses catalog row for (tape_file_number, first_chunk_lba), positions there, streams chunks (raw file bytes — no decompression, §6.2; final chunk trimmed by file size). Per-file SHA-256 verification happens on completion. Per-chunk CRC verification and CRC-triggered single-block parity recovery are deferred with the manifest `chunk_crcs` feature; until that feature is designed, tests cover happy path, file_sha256 failure, and file not found. |
| 12.12 | Reader: Tier 2 (byte-range). Pure-arithmetic chunk LBA computation from first_chunk_lba; head/tail trimming. Tests cover the §10.4 algorithm with fixture byte ranges crossing chunk boundaries. |
| 12.12.5 | Reader: external references summary. Reads the manifest's ExternalReferences array, formats for operator display. The `rem restore --show-deps` subcommand surfaces the list before extraction. **Tests verify the restore tool does not call `stat` on symlink targets at any point during extraction** — symlinks are recreated unconditionally. |
| 12.13 | TapeFormat trait integration. `RemTarV1` struct implements `TapeFormat`, `FileAddressable`, `ByteRangeAddressable`, `Verifiable`. Capabilities flags. Tests verify the integration end-to-end against 3b's trait surface. |
| 12.14 | Forward-scan recovery. The `recover_object_from_stream` function that walks tar from a known start LBA without manifest, reconstructing per-file LBA and per-symlink data by tracking tape position and reading typeflag '2' entries. Tests verify recovery of an object whose manifest LBA the caller doesn't know, including objects containing symlinks. Per-chunk CRC verification is skipped on this path (manifest unavailable); file_sha256 still verified. |
| 12.15 | Tar tool compatibility test. After writing a fixture object to a mock per-object BodyLba `BlockSink`, take the resulting object tape file (a clean contiguous pax tar stream — no parity inside, §5.1; no compression, §6.2). Pass it to GNU `tar` (or `bsdtar`) via `Command` with the matching blocking factor (`-b 512` for 256 KiB blocks). **Verify all files extract with byte-exact content and right sizes/mtimes** — the unconditional 30-year fallback claim. Verify a file whose tail doesn't fill a block extracts at exact size (no trailing zeros), confirming §5.2. Verify the post-EOF final-block zero-fill doesn't cause a spurious early-EOF or extra extracted bytes. Verify symlinks are recreated with correct target strings (including dangling ones). Verify ustar-sanitized ownership: a root `tar -x` of an archival-mode fixture does NOT set source uid/gid. The `_remanence/manifest.cbor` extracts as a sidecar. |
| 12.16 | Fix-up tool: `rem-validate-paths` standalone binary or `rem validate` CLI subcommand. Accepts a file list (from stdin or as arguments), runs validation, emits a JSON failure report. Includes symlink classification info in the report. Companion tool to generate `mv` scripts from the report. Tests against synthetic file trees with various encoding issues and symlink configurations. |
| 12.17 | Integration test against QuadStor. `#[ignore]`-gated test in `crates/remanence-format/tests/quadstor_rem_tar.rs`. Writes a small `rem-tar-v1` object including external symlinks, reads back via Tier 0/1/2, verifies. |
| 12.18 | Live smoke on production MSL3040. Same as 12.17 against a scratch LTO-9 tape. Pending hardware access. |
| 12.19 | CLI integration: `rem write --format rem-tar-v1 <files> [--symlink-policy default|strict|permissive] [--metadata archival|full|minimal]` and `rem read --format rem-tar-v1 <object_id> [--file <path>] [--range <start>-<end>] [--show-deps]`. The default format is `rem-tar-v1` if not specified; default symlink policy is `Default`; default metadata preservation is `Archival`. Validation errors surfaced to user with actionable suggestions. |
| 12.20 | Documentation: format-reference markdown ready for inclusion in the project's user-facing docs. Operator runbook entries for handling preflight validation failures and for understanding external symlink dependencies. |
| 12.21 | Wrap-up: design-doc sync, journal entry, status table update. |

### 14.1 Dependencies on other layers

- 3a: complete.
- 3b trait surface (steps 10.0-10.3): required. The remaining 3b
  work (registry, other format implementations) is orthogonal.
- 3c (steps 11.0-11.18): required. `rem-tar-v1` writes through
  a `ParitySink` and reads through an `ObjectParitySource` in
  production. Unit tests can use plain `BlockSink`/`BlockSource`
  without parity.

### 14.2 What's left out of v1

- Sparse file optimization (SPARSE_PRESERVING).
- Append-to-object (APPENDABLE).
- Resumable writes (RESUMABLE_WRITE).
- Format-level compression of any kind (removed in v0.8;
  §6.2). Compressible data is compressed by the orchestrator
  upstream into `.zst`/etc. files and stored as opaque bytes.
- Erasure-coded individual files (file-level FEC) — duplicates
  3c's role.
- ACL preservation beyond xattrs.

All deferred to future minor versions (`rem-tar-v1.1`, etc.) or
to follow-on formats (`rem-tar-v2`).

---

## 15. Testing strategy

This section is the short in-spec summary. The detailed cross-layer
implementation gate lives in `docs/remanence-testing-plan.md`, including
the rem-tar pax-padding solver, GNU/bsdtar compatibility, mock-tape crash
windows, 3c recovery, catalog transactions, QuadStor, and live scratch-tape
gates.

Four-tier shape:

1. **Codec unit tests** (`rem_tar_v1/header.rs`, `manifest.rs`,
   `chunk.rs`). Pax keyword codec round-trips. CBOR manifest
   round-trips. Chunk-index encode/decode. Property-based via
   `proptest` for chunk-index packing.

2. **Layout integration tests** (`rem_tar_v1/tests/layout.rs`).
   Write a synthetic object to an in-memory `BlockSink`, verify
   the resulting byte stream matches the §5.4 layout exactly.
   Tests cover small objects (one file), medium (50 files of
   varying sizes), large (one 2 GiB file), edge cases (empty
   files, files smaller than chunk_size).

3. **Round-trip integration tests** (`rem_tar_v1/tests/round_trip.rs`).
   Write objects, read them back via Tier 0/1/2, verify
   byte-perfect recovery. Use synthetic data with known hashes.

4. **Tar tool compatibility** (`rem_tar_v1/tests/tar_compat.rs`).
   Write an object, extract the tar stream, hand it to GNU
   `tar` / `bsdtar` via `Command`. Verify all files extract
   correctly with correct metadata. Skipped if no system tar
   is available.

5. **Live smoke** (`#[ignore]`-gated). QuadStor and MSL3040.

The tar-tool compatibility test is the strongest guarantee of
the "30-year readability" property. Run it on every release
against multiple tar implementations (GNU tar, bsdtar, Python
`tarfile`) to catch any drift from POSIX compliance.

---

## 16. Open questions

### 16.1 Manifest location

The manifest is written *last* in the object (just before tar
EOF), so its LBA is known only at object close. This means a
torn write that loses the manifest leaves a partially-readable
object — Tier 0 still works (forward-scan), but Tier 1/2 are
degraded until the manifest is reconstructed.

Alternative considered: write the manifest *first*, at the
start of the object, with deferred chunk LBAs. Rejected because:
(a) the writer doesn't know per-chunk LBAs until it has written
the chunks; (b) writing the manifest twice (once placeholder,
once final) requires seek-back, which LTO doesn't support; (c)
the per-pax-header chunk indexes provide complete information
for forward-scan recovery, so the manifest is purely an
optimization.

Lean: leave it. Trade a small Tier 1/2 degradation on torn
writes for a simpler, append-only writer.

### 16.2 Chunk size as a tape-wide vs. per-object setting

Currently, `REMANENCE.chunk_size` is per-object. In practice,
every object on a tape will use the same chunk_size (whatever
the 3a `write_config` set the tape to). Storing it per-object
allows future tapes to have mixed-chunk-size objects, but
that's not a realistic use case.

Could be simplified to "tape-wide chunk_size recorded in the
3c bootstrap." Saves a few bytes per object. Probably not
worth the spec churn now.

Lean: keep it per-object for forward compatibility.

### 16.3 Format-level compression (removed in v0.8)

Format-level compression was removed in v0.8 (§6.2):
compression is an orchestrator-level function, not a tape-
format function, and removing it eliminated a disproportionate
share of the wire-format complexity while making the 30-year
standard-tar fallback unconditional. The `REMANENCE.compression`
field is retained as a reserved enum (`none`-only in v1).

If a future v1.x ever reintroduces format-level compression on
measured evidence, the open questions it must answer (recorded
so they aren't relitigated from scratch): level granularity
(`REMANENCE.compression_level`?), the logical-size-vs-stored-
size split for the tar `size` field, per-chunk length framing,
the expansion-fallback rule, and the fact that compressed
files would no longer be byte-correct under plain tar. The
reserved field-9 manifest slot and `compression` enum exist so
this could be added without a format-version bump. Lean:
don't — let the orchestrator compress to `.zst` upstream.

### 16.4 Streaming-read of manifest for very large objects

With the current string-keyed manifest, a multi-TB single-file
object does not produce a multi-MiB per-chunk manifest: the
file's chunk count is stored as one integer. Reading the whole
manifest into memory before any manifest-backed access is
therefore fine for large single-file objects.

For pathological cases with huge numbers of file entries, two
paths can mitigate manifest pressure:

1. Use the per-file LBA catalog rows (§10.1) and skip the
   manifest entirely for common-case reads. The manifest is
   needed for object-level enumeration, extended metadata, and
   catalog reconstruction; these are not the hot path.
2. If a future minor-format revision reintroduces per-chunk
   verification data, choose a larger chunk_size for tapes that
   would otherwise create very large verification arrays.

Lean: not a real problem with the current manifest + catalog model.
Revisit only if manifest-backed workflows become hot for objects
with very large entry counts, or if per-chunk verification is
reintroduced.

### 16.5 Forward compatibility for schema_version

The `REMANENCE.schema_version` is currently `1.0`. A future
`1.1` would add fields but not change existing ones (forward-
compatible). A future `2.0` would be a breaking change requiring
a new reader.

The reader policy is "accept `1.x` for any x; reject `2.x` and
above with `UnsupportedSchemaVersion`." This requires that any
breaking change rev the major version, which is straightforward.

Lean: explicit in the spec; readers follow this policy.

### 16.6 Cross-object dependencies

Currently `rem-tar-v1` objects are independent — each tar
archive is self-contained. A future use case might want
multi-object collections that reference each other (e.g.,
"this proxy file refers to the master in object X").

The catalog handles this naturally (the orchestrator owns the
relationship graph). The format doesn't need to support it
on-tape.

Lean: keep the format self-contained per object. Cross-object
relationships are an orchestrator concern.

### 16.7 Layer 3b catalog schema (follow-up)

This spec assumes Layer 3b's catalog carries per-file rows
with `(file_id, object_id, tape_id, path, size,
first_chunk_lba, chunk_count, chunk_size, file_sha256, mtime,
compression)`. This is a schema extension to the v0.1 3b spec
(which had only per-object entries).

The change is tracked separately as a 3b update; rem-tar-v1
implementation can proceed in parallel because:
- The writer (§9) computes per-file LBA data inherently
  during tape write; surfacing it to Layer 5 for catalog
  recording is a small interface addition.
- The reader (§10.3, §10.4) reads from the catalog row data;
  the format itself doesn't care where the row comes from.
- The catalog-corrupt forward-scan path (§10.7) doesn't
  need the catalog at all and reconstructs per-file LBA data
  from pax extended headers.

Implementation order: add the per-file row columns to 3b's
catalog schema (alembic-style migration on the catalog
Postgres database), then implement rem-tar-v1. Steps 12.0
onward in the implementation plan assume the schema change
is done.

### 16.8 External subclip/content catalog (out of scope)

A common operational need for video archives is locating
byte offsets corresponding to logical content positions:
- Timecode → byte offset within an MXF file
- Frame number → byte offset within a ProRes file
- Subclip range → byte range to restore

These mappings are content-level information, not tape-level.
They're derived from parsing the source file's container
format (MXF, MOV, etc.) rather than from anything the tape
format records. For some formats they're computable from a
fixed formula (uncompressed PCM/YUV); for others they require
a parsed index table (H.264/H.265 GOP structures).

This information belongs in an **external application-level
catalog** (a separate Postgres table or service), not in
rem-tar-v1's on-tape metadata. The reasons:

- It's ephemeral: re-derivable from the file content at any
  time. The 30-year case doesn't need it; whoever has the
  file in 2055 can re-parse it.
- It's application-specific: different consumers want
  different offset granularities (per-frame, per-GOP,
  per-keyframe, per-timecode).
- It's subject to change: new analysis tools, new codecs,
  new ways to slice. On-tape data should not.
- It would multiply manifest sizes by 10× or more if stored
  per-chunk.

The shape of such a service:

```sql
CREATE TABLE video_offsets (
    file_id     UUID NOT NULL REFERENCES catalog_files(file_id),
    kind        TEXT NOT NULL,    -- 'timecode', 'frame', 'gop_keyframe'
    key         TEXT NOT NULL,    -- format-specific lookup key
    byte_offset BIGINT NOT NULL,  -- offset within the file
    byte_length BIGINT,           -- nullable for offset-only entries
    PRIMARY KEY (file_id, kind, key)
);
```

A subclip restore becomes:

1. App layer: look up byte range in `video_offsets`
2. App layer: query catalog for first_chunk_lba, chunk_size
3. App layer: compute chunk LBAs from the byte range
4. Remanence layer: byte-range restore (§10.4)

No part of this involves rem-tar-v1 metadata; rem-tar-v1's
chunk-aligned + per-file-LBA model is what makes step 3
trivial pure arithmetic. The format provides the substrate;
application layers provide the content-aware indexing on top.

Lean: explicitly out of scope for the format. Document the
intended pattern for downstream consumers. Schema details
belong in Layer 5 / orchestrator documentation, not here.

### 16.9 Validation result caching

The writer's pre-write validation (§9.0) is stateless: every
`begin_write` call re-validates every file from scratch.
Caching validation results across calls would save a few
seconds for typical archive sizes (50,000 files: ~2.5 s warm-
cache validation, parallelizable to ~0.5 s).

Caching is rejected for v1 because:

- The benefit is small for typical workloads.
- A cache requires `(path, mtime, size, inode)` as keys with
  invalidation on file modification, plus persistence across
  orchestrator restarts. Implementing this correctly is a
  meaningful surface; getting it wrong silently produces stale
  results that could allow broken paths onto tape.
- Stateless validation runs in parallel via `rayon` and is
  CPU-bound (string checks) plus disk-cached `stat()` calls.
  For 50,000 files on warm cache: well under a second
  end-to-end with parallelism.
- The OS page cache handles the second-walk-after-fix-up case
  efficiently; re-validation after operator fixes is fast even
  without an application-level cache.

If a future workload hits the edge case (millions of files,
frequent retry cycles, validation becoming a measurable
bottleneck), the optimization is contained — caching can be
added inside the validator without changing the format spec
or external API. Defer until empirically justified.

### 16.10 Catalog database charset requirement

The strict-UTF-8 rule in the format only guarantees that
bytes written to tape are valid UTF-8. The catalog database
must also accept UTF-8 for filename columns, or the format-
level guarantee is undone at the database boundary.

Concrete requirements (Layer 5 / 3b concern, documented here):

- **PostgreSQL:** database created with `ENCODING 'UTF8'`
  (the default for modern installs). `TEXT` columns inherit
  the database encoding.
- **MySQL/MariaDB:** database and tables use `utf8mb4`
  charset (not legacy `utf8`, which is a 3-byte subset that
  excludes 4-byte characters used in many real filenames).
- **SQLite:** uses UTF-8 by default for `TEXT` columns;
  no configuration needed.

Layer 5 startup must verify the catalog DB's encoding and
refuse to operate if misconfigured. The check is one query
at boot (e.g., `SHOW server_encoding` in Postgres) and
prevents the silent-corruption failure mode that BRU
encountered in production (see §5.7).

The 3b catalog follow-up document captures the schema
details, including this charset requirement.

### 16.11 Orchestrator walker requirements (cycle safety)

The filesystem walker that builds the input file list (an
orchestrator/Layer 5 concern, not part of the format) MUST
be cycle-safe. If the walker follows directory symlinks
without cycle detection, it can produce an inflated input
list — the FCP `.fcpcache/` failure mode that historically
caused BRU to write hundreds of GiB of content many times
over.

Required walker behavior:

- **Default**: do not follow directory symlinks. Record them
  as symlink entries and continue. (This is what `walkdir`
  in Rust does by default; what `find` does without `-L`;
  what most modern walkers do.)
- **If following symlinks** (an explicit operator choice):
  track visited directory inodes (`(st_dev, st_ino)` tuple).
  Refuse to enter a directory whose inode is already in the
  current path's ancestor set. Equivalently, refuse to
  re-enter any directory already visited.
- **Optional but recommended**: a maximum depth limit
  (e.g., 256 directory levels) as a backstop against pathological
  trees even without symlinks.

The format spec cannot enforce walker behavior — by the time
the input list reaches `begin_write`, the walker has already
done its work. Defense in depth: the writer's input-list
sanity validation (§9.0) catches the catastrophic-inflation
case with entry-count and inode-duplicate limits, but the
correct fix is at the walker layer.

Lean: document this requirement loudly in Layer 5 / operator
documentation. Reference implementations (the CLI `rem`
command, the Dwara v3 orchestrator) must use cycle-safe
walkers.

### 16.12 Future SymlinkPolicy::Dereference

A future addition to `SymlinkPolicy` could be `Dereference`:
"treat each symlink as if it were the file it points to;
read the target's contents and archive that." This is useful
for making self-contained portable archives from projects
that currently rely on external assets.

Adding this policy in a future spec revision requires
explicit handling of:

1. **Cycle detection during dereference.** The dereferencer
   must track visited inodes and refuse re-traversal. Without
   this, the FCP cycle bomb returns.
2. **Depth limits.** A maximum symlink-following depth
   (e.g., 40, matching most OS-level `realpath` limits) as
   a safety net.
3. **Target encoding validation.** The target file's name
   becomes part of the archive structure (e.g., as the
   inlined file's path); its UTF-8 validity must be enforced
   like any other path.
4. **Size accounting.** Dereferenced content can balloon
   archive size unexpectedly. The writer should surface
   estimated final size before tape I/O begins.
5. **Choice of effective path.** When `clip.mxf` is a
   symlink to `/raid/shared/master.mxf`, the archived entry
   needs a canonical path within the archive — keeping the
   original symlink path (`clip.mxf`) is the natural choice
   but should be explicit.

`SymlinkPolicy::Dereference` is **out of scope for v1.0**.
This note records the requirements so a future implementer
doesn't reintroduce the cycle-bomb failure mode by accident.

### 16.13 Why full POSIX metadata is not preserved by default

The default `MetadataPreservation::Archival` mode (§9.4)
deliberately omits uid/gid/uname/gname and non-executable
mode bits. This is a departure from tar's "preserve
everything" default, and the reasoning is worth recording
because it tends to surprise people coming from a backup
mindset.

tar's defaults were designed for backup-and-restore on the
same Unix system, where uid/gid/mode round-trip faithfully.
rem-tar-v1 is a long-term archive format whose dominant use
case is cross-system, cross-time restore. In that context:

- Numeric uid/gid are meaningless on the restore system.
- uname/gname rarely map to anything on restore.
- Permission bits are restore-time policy, not archive content.
- Storing them implies fidelity that cross-system restore
  can't deliver, and forces a policy decision onto the
  restore operator.

The `Archival` default preserves what's genuinely durable
and meaningful (content, path, mtime, exec bit, xattrs) and
drops what isn't. The `Full` mode is available for the
backup-style minority case.

Possible future refinement: a per-object heuristic that
auto-selects `Full` when the archive is detected to be a
same-system backup (e.g., the orchestrator passes a flag
indicating restore-to-same-environment intent). Not needed
for v1.0; the explicit three-way choice is sufficient.

Open sub-question: should `Archival` preserve the setuid/
setgid/sticky bits (distinct from the rwx exec bit)? These
are security-sensitive and rarely meaningful in a video
archive. Current lean: no — `Archival` preserves only the
plain executable bit; setuid/setgid/sticky are dropped (they
belong to system binaries, not archived content, and
preserving them on restore could be a security footgun).
`Full` mode preserves them as part of the full mode bits.

### 16.14 Layer 3c interface (RESOLVED in 3c v0.4.2)

rem-tar-v1 is specified against Layer 3c's filemark-aware
parity-epoch-with-sidecars model. When this section was first
written that model was a *proposed* 3c revision; it is now
**settled and implementation-ready** in `layer3c-design.md`
v0.4.1. This section is retained as the rem-tar↔3c contract
checklist, with each item pointing at where 3c fixes it. None of
these are open against 3c any more.

**Address model:**

1. **Three address spaces** — `TapePosition` (physical:
   `tape_file_number` + `block_within_file`, via the filemark
   map), `ParityDataOrdinal` (3c-internal, protected-data stream
   skipping filemarks/sidecars/bootstraps), and per-object
   `BodyLba` (what rem-tar uses). 3c §4 / §6; rem-tar §2.1. 3c
   now exposes these as distinct types (`BodyPosition`,
   `PhysicalPositionHint`, `TapeFilePosition`) so physical
   semantics never leak to rem-tar.
2. **Per-object BodyLba interface** — 3c's body-facing
   `BlockSink`/`BlockSource` operate in per-object `BodyLba` and
   return `BodyPosition`; `ParitySink::finish_object()` writes
   the terminating filemark; `ObjectParitySource::open()`
   yields the object reader. 3c §6.1, §6.2.

**Parity-epoch model (all settled in 3c):**

3. **Filemark-aware parity epochs** over `ParityDataOrdinal`,
   spanning object filemarks; filemarks never flush an epoch. 3c
   §5.1–5.3, §7.1.
4. **Parity as sidecar tape files** — 3c chose the **raw
   fixed-block sidecar** (header/index blocks + raw shard
   blocks), not the pax-wrapped variant, with a strict byte-offset
   layout. 3c §5.5. (Item 4's "pick one" is decided: raw.)
5. **Filemark map** — durable, authoritative in the catalog
   (`catalog_tape_files`) and digested into bootstraps for
   catalog-less reconstruction. 3c §5.6, §8.1; 3b follow-up.
6. **Forced-erasure recovery** — `recover_block_at` resolves
   `(tape_file_number, body_lba)` → `ParityDataOrdinal` → stripe,
   verifies peers against sidecar data-shard CRCs, reconstructs,
   and verifies the reconstructed block against its CRC. 3c §8.3.
7. **Block-size-aware geometry** — `S=512, m=4, k=128` at 256 KiB.
   3c §5.4 / `default_scheme_for_block_size`.

**Formerly-open questions, now answered in 3c:**

8. **Large-archive parity spool** — RESOLVED: 3c uses incremental
   parity accumulation (Option B), spooling only the ~512 MiB of
   parity accumulators per epoch (never object data), with the
   free-space/cap/crash guardrails of §9.2 mirrored in 3c §7.4.
   The reserve includes the object's future sidecars (3c §7.5).
9. **Parity sidecar redundancy** — RESOLVED (as a deliberate v1
   simplification): sidecars are **not** replicated or
   parity-protected in v1; the three-copy archive policy is the
   redundancy. A damaged sidecar degrades only its epoch's parity
   on that copy. 3c §7.4, §13.2.

**The one contract rem-tar must hold up its end of:** the
final-block flush boundary (§9.1.1). `BodyBlockWriter::
finish_after_tar_eof()` (rem-tar) writes the zero-filled final
partial block *after* the two tar EOF records;
`ParitySink::finish_object()` (3c) then writes only the filemark
and any completed-epoch sidecars, and asserts the body already
flushed a whole final block. 3c depends on this; rem-tar
guarantees it. This boundary is unchanged from v0.8 and matches
3c v0.4.4 §6.1.

rem-tar-v1 development against a mock `BlockSink`/`BlockSource`
implementing the per-object `BodyLba` contract can proceed now,
in parallel with 3c's own mock implementation (§14 intro). Live
integration follows once both sides have passed their mock-tape
and corruption-injection suites.

### 16.15 Cross-document drift: bootstrap / index / MAM model

The initial Remanence spec (`docs/spec-v0.3.md`) described a
bootstrap *tar archive* at filemark 0, periodic/final tar
index archives, and a MAM (Medium Auxiliary Memory) pointer
to the latest index. Layer 3c's later design replaced this
with a **parity-layer root-of-trust bootstrap block at
PhysicalLba 0** carrying the parity scheme, and rem-tar-v1
places the per-object index in the in-archive manifest plus
the external catalog.

This is a reasonable evolution but the documents disagree on
the surface. The resolution: **the 3c bootstrap-block model
and the rem-tar-v1 manifest/catalog model supersede the
initial spec's bootstrap-tar-archive / tar-index / MAM
model.** The initial spec should get a "superseded by
3c/rem-tar-v1" note on those sections.

Specifically:
- "Bootstrap tar archive at filemark 0" → superseded by 3c's
  bootstrap **tape file** (one fixed block + filemark) at BOT,
  replicated at several fractional tape positions, each carrying
  the parity scheme and the filemark-map digest (3c §5.6, §7.3).
- "Periodic/final tar index archives" → superseded by
  rem-tar-v1's per-object manifest (§8) plus the Layer 3b
  catalog (§10.1).
- "MAM pointer to latest index" → MAM may still be used for
  tape identification and quick metadata (tape UUID, label),
  but is no longer the authority for index location; the
  catalog is. MAM usage is a 3a/Layer 5 concern.

This note is the reconciliation; the initial spec edit is
tracked as a separate documentation task.

---

## 17. References

- IEEE Std 1003.1-2001 (POSIX) — pax tar interchange format.
- POSIX pax extended header keyword namespacing convention:
  "Keys in all lowercase are standard keys. Vendors can add
  their own keys by prefixing them with an all uppercase
  vendor name and a period."
- `docs/spec-v0.3.md` §1, §5 — format longevity priority and
  on-tape format requirements.
- `docs/layer3b-design.md` — `TapeFormat` trait, capability
  semantics, `BlockSink`/`BlockSource`.
- `docs/layer3c-design.md` — parity layer (transparent to
  this format).
- Rust `tar` crate documentation: <https://docs.rs/tar>.
- `ciborium` crate documentation: <https://docs.rs/ciborium>.
- Backblaze, "Reed-Solomon Coding for Data Backup"
  (referenced by 3c; not directly used here).
- Bacula Storage Media Output Format documentation (the
  "BB02" format) — informs the periodic-index pattern.

---

## Appendix A: Worked example — minimal object layout

A small object containing two files, at the default 256 KiB
chunk size, illustrating tar-safe trimming (note the
non-multiple sizes):

- `clip_001.mxf`, 1.20 MiB = 1,258,291 bytes (5 chunks: four
  full 256 KiB chunks + a 209,715-byte tail)
- `clip_002.mxf`, 700 KiB = 716,800 bytes (3 chunks: two full +
  a 192,512-byte tail)

The layout, in `BodyLba` (3c maps `(tape_file_number, BodyLba)`
to physical tape positions via the filemark map; this object
tape file contains no parity blocks — parity is in separate
sidecar tape files, §2.1, 3c §5.5):

```
BodyLba 0:    Global pax header (typeflag 'g') + file 0's pax
              extended header (with REMANENCE.pad) sized so
              file 0 DATA starts at BodyLba 1.
BodyLba 1-5:  File 0 data, 1,258,291 bytes. Blocks 1-4 are full
              256 KiB; block 5 holds the 209,715-byte tail
              followed by tar's 512-byte-record padding and the
              start of file 1's pax header. NOT zero-padded to
              a full block.
BodyLba 6:    (file 1's pax header began in block 5's tail; its
              REMANENCE.pad sizes it so file 1 DATA starts at
              BodyLba 6.) Actually — see note below on alignment.
BodyLba 6-8:  File 1 data, 716,800 bytes. Blocks 6-7 full; block
              8 holds the 192,512-byte tail + record padding +
              start of manifest pax header.
BodyLba 9:    Manifest pax header (with REMANENCE.pad) sizing
              manifest DATA to start at BodyLba 9.
BodyLba 9:    Manifest data (canonical CBOR, ~1 KiB → fits in
              one block; trimmed by size, not zero-padded).
BodyLba 10:   Tar EOF (two zero records) + record padding, then
              the final block is zero-filled past the EOF
              (fixed-block media, §5.2).
[filemark]    Terminates this object's tape file (§5.1). 3c may
              emit a parity-epoch sidecar tape file next.
```

(The exact block at which each file's data starts depends on
how the preceding pax header's `REMANENCE.pad` is sized; the
writer computes this so that *data* starts are block-aligned.
The illustration above is schematic — the invariant that
matters is: file-data starts are block-aligned, file-data ends
are size-trimmed, never zero-padded.)

Overhead at this small size is dominated by per-file pax
headers and the partial final blocks; the ratio is high for
tiny files. For larger files it drops sharply: a single 100
GiB file at 256 KiB chunks is ~409,700 blocks (409,600 chunks
plus ~100 blocks of headers and manifest), ~0.025% overhead.
For a 1 TiB file, ~0.0025%.

Per-file catalog rows from this object (BodyLba values,
recorded by Layer 5 at object close):

```
file_id=<F0_uuid>, path=clip_001.mxf, size=1258291, first_chunk_lba=1, chunk_count=5
file_id=<F1_uuid>, path=clip_002.mxf, size=716800,  first_chunk_lba=6, chunk_count=3
```

A reader given `(F1_uuid, [262144, 524288))` (read the second
256 KiB of clip_002.mxf):
1. Catalog lookup: tape_file_number=<this object's file>,
   first_chunk_lba=6 (per-object BodyLba), chunk_size=262144
2. start_byte=262144 → chunk_index=1, head_drop=0
3. end_byte=524288 → last chunk_index=1, full chunk
4. Position to (tape_file_number, BodyLba 6+1=7). 3c resolves
   to physical position via its filemark map; there are no
   parity blocks inside the object archive to skip.
5. Read 1 block, emit all 256 KiB unchanged

Total cost: one seek plus one block read. No manifest read.

---

## Appendix B: REMANENCE.* keyword reference

Global header (typeflag 'g'):

| Keyword | Required | Value | Description |
|--|--|--|--|
| `REMANENCE.format_id` | yes | "rem-tar-v1" | Format identifier. |
| `REMANENCE.schema_version` | yes | "1.0" | Format schema version. |
| `REMANENCE.object_id` | yes | UUID string | Object identifier. |
| `REMANENCE.caller_object_id` | yes | string | Orchestrator's opaque ID. |
| `REMANENCE.chunk_size` | yes | decimal | Bytes per chunk, default 262144 (256 KiB). |
| `REMANENCE.encryption` | no | "aes-gcm-256" | If encryption used. |
| `REMANENCE.write_timestamp` | yes | RFC3339 | Object write time. |
| `REMANENCE.metadata_preservation` | yes | "archival"\|"full"\|"minimal" | Preservation mode for this object (§9.4). |
| `REMANENCE.writer_version` | no | string | Software version. |
| `REMANENCE.pad` | no | string (spaces) | Inert keyword inside a real pax header, sized for block alignment (§5.2, §7.5). Never a standalone member. |

Per-file extended header (typeflag 'x'):

| Keyword | Required | Value | Description |
|--|--|--|--|
| `path` | when needed | string | Standard pax; long paths. |
| `size` | when needed | decimal | Standard pax; >8 GiB files. |
| `mtime` | archival/full | decimal | Standard pax; sub-second mtime. Absent in minimal. |
| `uid`/`gid`/`uname`/`gname` | full only | various | Standard pax. Only in `Full` preservation mode. |
| `REMANENCE.file_id` | yes | UUID string | File identifier. |
| `REMANENCE.file_sha256` | yes | hex string | SHA-256 of file content (cryptographic). |
| `REMANENCE.chunk_count` | yes | decimal | Chunk count for this file. |
| `REMANENCE.executable` | archival/full | bool | The +x bit; only permission bit archival preserves. |
| `REMANENCE.compression` | yes | "none" | Reserved enum; always `none` in v1 (§6.2). |
| `REMANENCE.encryption_kek_ref` | no | opaque | If encrypted. |
| `REMANENCE.is_manifest` | only on manifest | "true" | Manifest file marker. |

Note: per-chunk integrity codes and per-chunk LBAs are intentionally
NOT in pax headers. The current manifest records `first_chunk_lba`
and `chunk_count`, but does not record per-chunk CRC-64 codes; that
integrity layer is deferred (§8.1). LBAs are derived arithmetically
from `first_chunk_lba` (recorded in the catalog and manifest, not in
pax headers) plus the chunk ordinal. Per-object full mode bits / uid /
gid / uname / gname appear only when the object was written with
`MetadataPreservation::Full` (§9.4).

---

*End of design v0.9.3 (implementation-ready, aligned with Layer 3c v0.4.4). Comments and corrections welcome — please
annotate inline rather than rewriting.*
