This is the open-items record for the [Rem Tape Parity (REM-PARITY)
Format](../rem-parity-1-specification.md), preparing revision 1.0.0-draft.5.
It is informative.

At cut, this record moves with the specification to `specs/publication/records/`
and is deposited with it as the snapshot taken at that revision's freeze, with
its own digest line in `DEPOSITED.sha256` and a listing in the deposit description.

Section references name that specification unless another document is named.
Appendix C is in the [revision-history record](rem-parity-1-revision-history.md).
Paths in the snapshot are relative to the repository root or the vector set
named in the specification.

## Appendix D. Open Items (Informative)

This is the live preparing-copy snapshot for generation 2.
The reference Reader still refuses a finalized tape with `W < T`; migration
step 6 relaxes it together with recovery and verification of such a tape,
and adds the writer-side vector. The draft vector
`terminal-scope-scalars/isolated-3` now expects a valid tape and reports
`[2, 5)` as unprotected; its reference acceptance test remains ignored until
that step.

1. **TT-1 — byte derivation by the terminal verifier (candidate evidence
   passed).** The terminal verifier, a Python implementation, reproduces all
   six review profiles and their 30 component streams without calling the Rust
   codec. Incremental reviews bind later changes to the last recorded clean
   baseline; an untouched byte-contract region does not lose its accepted
   review status.
2. **TT-2 — negative and interruption vectors (candidate evidence gathered;
   REM-PARITY freeze criterion 2, recorded in `specs/README.md`, remains open
   until the items below close).**
   The review-only set verifies 50 hostile mutations, 14 survivor selections,
   68 interruption cuts, seven Object-row extension cases and the
   million-Object streaming profile; the interruption cuts are cuts in
   finalization, and Section 17's resume vectors cover append resume. It also
   holds the tape images, damage matrix, resume vectors and negative vectors
   that Section 17 describes. A second implementation written from this
   document re-derives the pinned bytes Section 17 lists and decides the cases
   Section 17 describes. Publication promotion remains gated by TT-5 and the
   other freeze criteria. The items below record what is decided and what
   remains open:
   - The damage matrix has 72 cases: 71 have blind decisions, of which 69
     are pinned. E9, `short-epoch-followed-by-another`, is also pinned; it is
     added later and checked against the second implementation without a
     blind decision. Its independent byte rederivation and Reader checks
     supply no blind-decision evidence. Criterion 2 still requires every
     pinned-at-generation value to be independently re-derived before freeze;
     it does not require a blind decision for every case. Its wording and
     the remaining items here stay unchanged.
   - The second implementation now compares the mutated-block digests of the
     negative vectors (`tape-images/negatives/MANIFEST.tsv`) and decides the
     outcomes that `MUTATIONS.tsv` and `SELECTION.tsv` pin. Of the 387 digest
     rows, 340 match, and 28 are blocks a mutation leaves unchanged, whose
     digests equal the pinned ones. The other 19, in two supplement variants
     (`sidecar-real-data-shard-count/isolated-2` and
     `overflow-7.2-T/isolated`), pin bytes that this document does not
     determine: parity for an epoch longer than `S × k`, and a generated
     profile the variant leaves open. They record the reference's construction
     and are not criterion-2 items. The implementation's decisions agree with
     the Section 15 names written from this document on 49 of the 50 mutation
     rows; the fiftieth defines no bytes. `SELECTION.tsv`'s choice among valid
     replicas is the reference's policy; this document decides the outcome
     class and the set of acceptable replicas. `INTERRUPTIONS.tsv` records the
     reference Writer's states at cut points, which are implementation state
     and not format values, so criterion 2 does not ask a second
     implementation to re-derive them. At tape level, the decisions agree with
     the reference except on the five rows whose damaged payload the Scanner
     never reads, which stay informative because a Scanner need not read every
     payload (Section 12.6), and `gap-wrong-total-length`, whose outcome is
     the BOT walk (Section 8.4 step 1). The four conflict rows of
     `SELECTION.tsv` have been rebuilt so that each conflicting replica can be
     eligible, and the reference returns `TerminalIndexReplicaConflict` for
     each. The second implementation's decisions for `e3-02` and `e3-06`
     follow Section 12.6's scope rule.
   - Five formulas blocked freeze under the release record's minimum coverage
     for negative candidates, whose last item asks for overflow in every size
     and location formula, because this document left the outcome of their
     overflow undefined. It now decides each. Section 3.2's `LBA(f, b)` must
     fit for a map to be valid (Section 7.2). Section 3.3's inverse compares
     the exact ordinal, so an ordinal that would not fit is an implicit zero
     (Sections 2.4 and 3.3). The append point's inputs come from commit
     records, so its overflow is `ResumeAppend`, and the walk length's inputs
     are device reports, so its overflow is `TapeIo` (Section 2.4). Section
     13.3's tail location is `H + P`, with the precondition
     `total = 2H + P + 1` and `H > 0`. These stay open until their vectors are
     pinned to these outcomes. The `freeze_blocker` flags in
     `negative-cases.json` predate this classification; where they differ,
     this item governs, until the flags are cleared.
   - No vector can exist for four formulas, and they do not block freeze for
     that reason. No input a Reader reads can make Section 10.1.2's `2M + 1`
     overflow at a legal block size, and no operation in Section 3.3's forward
     mapping or in Section 9.4's computation of `H` can overflow. Only a
     Writer evaluates Section 10.3's close-reserve formula, from its own
     state.
   - Three formulas have a pinned overflow vector that the reference rejects
     at an earlier check: Section 7.2's `T`, Section 10.4's record-geometry
     product and Section 10.5's `actual_bytes`. Under Section 2.4's exact-value
     rule, three pinned vectors describe inputs whose exact values fit, and
     are accepted: `overflow-10.1.2-M/isolated`,
     `overflow-10.4-record-geometry/isolated` and
     `overflow-10.3-manifest-range/isolated-2`. Their expectations are to be
     revised, and every other pinned overflow vector checked against the rule.
     Whether Section 10.1.2's `M`, Section 10.4's record geometry and Section
     10.3's manifest capacity still have an overflow that a vector can reach
     under the exact-value rule is to be recorded when those vectors are
     revised: each either keeps a reachable vector or joins the formulas for
     which none can exist.
   - Of the four questions this item recorded, all four are now decided: a
     rung that fails on a count mismatch does not recognise the file (Section
     12.3; `filemark-prefix`); a sidecar's footer or directory decides between
     two copies that differ, and a Verifier reports the divergence (Sections
     9.1 and 13.3; `sidecar-primary-tail-disagreement`); which bootstraps
     count as unreadable (Section 8.4; TT-7, now closed); and when the footer,
     the primary copy and the final ParityMap are all unavailable, a Recoverer
     tries the tail copy that the terminal index's record of the sidecar
     locates (Section 13.3; `parity-map-and-sidecar`). The reference has that
     route. The tail
     rescue's formula `H = (total − 1 − P) / 2` keeps a vector, as the
     formulas above do: a replica row for a sidecar with a block count of at
     most `P` makes `total − 1 − P` negative, and the epoch is then
     metadata-unavailable (Section 13.3), not a rejection of the replica.
   - The three readings this item listed are now stated: exact integer values
     (Section 2.4), the recorded fields in Section 10.6's now-retired `W = T` rule, and the
     tail copy at `H + P`, with a directory entry's total equal to
     `2H + P + 1` (Section 13.3).
   - Section 2.2 includes in a Verifier's full check the Recoverer's index and
     CRC validation. The reference's verification reads a sidecar's footer and
     two header copies, and every data block it protects and every parity
     shard it holds, and checks each against the index. A full
     verification reads every data block and parity shard (Section 2.2), and
     the reference's verification does so.
   - Where this document leaves a name open, some vectors give a set of
     permitted Section 15 errors, or none. The second implementation's
     `GAPS.md` and the notes in `tape-images/negatives/` list these and the
     other places where this document is silent or ambiguous. Each is to be
     decided, or listed here, before freeze.
   - The source of resume geometry is decided: Section 14 step 2 permits a
     Resumer to take `S` and `k` from its commit records or from the bootstrap.
     Section 11.5 (W5) requires evidence recorded no later than commit before
     parity is computed from read-back. These Writer-side clarifications do
     not close the remaining Reader questions or vector coverage items.
   - Section 12.3 (item 4) finds a ParityMap whose block 0 is unreadable, or
     damaged so that no head rung takes it, through its footer and tail header
     copy, and the replica route survives that damage by locating it through
     structural rows. A ParityMap for which neither the header nor the footer
     route works is an Object candidate. The reference implementation follows
     both routes; the damage cases `s6-10`, `s6-12` and `s6-13` pin the footer
     route.
   - Section 12.3 leaves one point open. A readable, valid bootstrap at tape
     file 0 that measures more than one block is not recognised by item 1, and
     the reference implementation's walk ends at once with
     `FilemarkMapReconstruct`, because a map cannot hold anything but a
     one-block bootstrap at tape file 0. That differs from Section 12.3, which
     makes such a file an Object candidate (item 7), and from the 1.0.0-draft.1
     entry of Appendix C, which reads that a Scanner does not abort a whole
     catalog-less walk for a count mismatch. No vector pins it.
   - For item 6, a footer whose tail copy disagrees with it is reported and the
     file falls through, although the definition of "parses" would make that
     footer not parsed. No outcome turns on it, because item 6 is not part of
     the footer rule's gate.
   - Section 12.3's rule for tape file 0 speaks only of the walk with supplied
     values. Without them, the reference types an unreadable one-block tape
     file 0 a bootstrap as well, and this document leaves that mode
     unspecified.
   - Section 12.3 reports as damage a terminal footer that does not parse or
     whose count differs. Whether a replica whose head is unreadable but whose
     footer is good counts as damaged for Section 12.6's test of the exact
     terminal suffix is not stated.
   - A bootstrap of another `schema_major` is refused by name
     (`BootstrapParse`, format major) only when values are supplied. Without
     them, discovery reports `NoBootstrapFound`, because Section 15 gives
     discovery no other outcome for a bootstrap it cannot use.
   - A sidecar header whose metadata is wrong but whose checksums are valid
     makes the walked map's digest check fail, and so denies every epoch on
     the walk route. Section 13.1's check is all-or-nothing by design, and the
     replica route is not affected.
   - A Reader loads a ParityMap tape file of any length into memory; bounding
     that load belongs with the fuzzing campaign of TT-6.
   - A Verifier reports every finding it makes about one sidecar. This
     document does not order them.
   - The walk cannot type a terminal replica or separation extent none of
     whose records can be read. With no head and no footer to read, the file
     is an Object candidate outside the validated scope (Section 12.3),
     although the planned tuples in a readable neighbour name it.
   - No rule compares a directory entry's `parity_shard_block_count` with `S ×
     m`. The rescue takes `P` from the scheme, so an entry whose own count
     disagrees changes nothing a Reader does.
   - When a final ParityMap validates and its entry for a sidecar fails a
     precondition of the directory-assisted rescue, an intact tail copy is not
     read, and the epoch is metadata-unavailable, although the same tape would
     recover on the replica route if the ParityMap were lost.
   - A medium error at a filemark position has no outcome in this document.
     The vectors that list such a position take the filemark as present.
   - The bootstrap vectors all use the first candidate block size. A supplied
     size that is not the first candidate is covered by unit tests only.
   - A Verifier that finds physical damage in the prefix keeps the terminal
     route only while every undamaged file of the walked prefix agrees with
     the replica's rows in every field it records. A damaged sidecar that the
     walk types as an Object changes the recounted ordinals of later Objects,
     and the Verifier then takes the walk; this is conservative and loses no
     block.
   - A claim in `fixtures/rem-parity-1/vectors.json` still names the
     authoritative directory overlays that generation 2 removed; nothing reads
     the file.

   The generation-1 REM-PARITY cases of the pinned archive were read against
   generation 2. Most of the properties they test are kept. The reference
   implementation's tests cover them, and the generation-2 candidate vectors
   now also cover the groups that had no candidate coverage, among them the
   sidecar negatives.
3. **TT-3 — media exercise of the default separation extents (VTL passed;
   physical open).** The exact one-GiB layout has passed clean VTL writes and
   independent verification of the terminal suffix at all three legal block
sizes. The 256 KiB
   and 512 KiB legs respectively proved 4,096 and 2,048 records per separation
   extent, compression disabled, the five dense tape files of the terminal
   suffix, their filemarks, and exact EOD. At least two supervised
   physical-tape block sizes are still required, including footer,
   filemark/EOD, PEW, and EOM observations.
4. **TT-4 — end-to-end lifecycle reconciliation (implemented and rerun).** The
   VTL scenarios have exercised automatic and manual finalization, all 68
   interruption cuts, permanent Object refusal after finalization starts,
   eventual reconciliation of the complete terminal suffix, sole-BOT
   checkpoint replay, and the full matrix of default separation extents. Each
   affected member ran from its own clean slate at the recorded implementation
   head.
5. **TT-5 — external prose and incremental-diff review.** The preparing copy
   has no normative dependence on geometric placement, `2M+1` index copies,
   bootstrap Object-row ceilings, or singular final bootstrap authority.
   Review is incremental from the last recorded clean baseline; two
   consecutive clean reviews of the then-current increment satisfy the gate,
   and later commits reopen only the regions they touch.
6. **TT-6 — terminal-format fuzz plateaus (open).** Existing campaigns cover
   bootstrap, sidecar, ParityMap, and the pre-terminal scan walk. They predate
   the generation-2 terminal format introduced in draft.4: there is not yet a
   dedicated coverage-guided
   terminal-replica parser campaign, separation parser campaign, or scan-walk
   campaign whose generator reaches terminal kinds at the legal 256 KiB,
   512 KiB, and 1 MiB record sizes. REM-PARITY freeze criterion 3, recorded in
   `specs/README.md`, remains open until those targets, committed corpus
   replay, and measured plateau reports exist.
7. **TT-7 — which bootstraps count as unreadable (closed).** Section 8.4 now
   states it, in two stages: the physical read, then the content of a record
   of the right length. Section 15 states the bootstrap error names by the
   level at which a Reader meets the block.
