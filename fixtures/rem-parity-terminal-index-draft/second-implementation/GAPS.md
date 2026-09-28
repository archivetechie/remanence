# Gaps found by the second implementation

Each entry is one text defect, one silence of the text, or one gap in the
criterion-2 evidence, with the section it concerns. Where the builder had to
make a choice the text does not determine, the entry says what the candidate
uses. Section numbers are REM-PARITY's unless another document is named.

Entries marked *decided* are silences that this implementation resolved by
a stated reading. Entries marked *undecided* are the ones the `decide` output
reports as `undecided`. The remaining entries affect no decision.

Sections A to C were written before the expected outcomes were read. After
the comparison (`COMPARISON.md`), each entry that an expected outcome depends
on carries a line beginning **Expectation**, which names the case and the
reading it takes. Section D lists the gaps the comparison itself surfaced.

## A. Building the artifacts

**A-1. Section 5.3: the key-order rule cites the wrong RFC 8949 section.**
The text says map keys are "compared first by encoded length, then
lexicographically by encoded bytes, as [RFC8949] Section 4.2.1 specifies".
RFC 8949 Section 4.2.1 specifies bytewise lexicographic order of the encoded
keys. The length-first order is RFC 8949 Section 4.2.3. The two orders differ
for integer keys of different lengths or major types. For example, key −1
(encoded `20`) sorts before key 24 (encoded `18 18`) under the length-first
order, and after it under the bytewise order. The imported encoder and decoder
use the length-first order, as the sentence states. No candidate tells the
two apart: `unknown-negative-key` places key −1 only beside keys of 13 and
below, where both orders agree.

**A-2. Section 8.2: the `sha256` of the bootstrap's BOT-only digest record
is not defined.** Section 8.2 fixes the shape of key 2 and its scalars
(`tape_file_count=1`, zero ordinals, `is_final_map=false`), but not what its
`sha256` is computed over. Section 7.4 defines a digest record only "In each
replica envelope". The text does not determine the value. The candidate uses
the Section 7.3 canonical digest of the one-entry map `[bootstrap(#0, 1
block)]` (`4844cab0…`, the same value as the minimal profiles'
canonical-map digest). That is what reading key 2 as a Section 7.4 digest
record over its `tape_file_count` leading entries gives.

**A-3. Section 10.1.5: no rule says when a Writer sets the known-good
flags.** The flags 0x02 (primary copy known good) and 0x04 (tail copy known
good) are defined, and unknown bits are rejected. No Writer rule says when to
set them, and no Reader decision uses them. The text does not determine the
flags a Writer records. The candidate sets both (6) on every directory entry.
The image inputs record this as `directory_flags`, so the value was taken
from the inputs, not derived.

**A-4. Section 11.2: whether a checkpoint closes a short epoch is left to
the Writer.** "A barrier may close a non-empty short epoch and emit its
sidecar". The image inputs record where the checkpoints fall
(`checkpoint_after_objects`), but not whether each one closes the pending
epoch. The text does not determine this. The candidate `short-epoch` image
closes its two-ordinal epoch at the checkpoint after Object 0: its directory
entry lacks `FINAL_PARTIAL_EPOCH`, which Section 10.1.5 permits only on an
epoch closed by finalization. The builder reads a checkpoint as closing any
non-empty pending epoch.

**A-5. Sections 8.2, 10.1.4 and 10.4: nothing ties the three copies of the
diagnostic text together.** Bootstrap keys 3 and 4 are optional, ParityMap
keys 6 and 7 are optional, and the replica frame carries a writer version
and a timestamp of its own. The text relates none of them. The image inputs
carry one `diagnostic_keys_present` flag and one `writer_identity`. The
builder applies them to all three places, and the candidate carries the
same text in all three.

**A-6. Section 10.1.4: the starting value of the ParityMap `sequence` is not
defined.** "`sequence` is a per-tape counter over ParityMap emissions".
The candidate uses 0, recorded in the inputs as
`parity_map_sequence_start`.

**A-7. Section 10.1.3: `copy_block_count` is named but not defined.** The
header table lists `copy_block_count u64` at 0x98, and no sentence gives its
value. The builder writes `M`, the blocks per copy of Section 10.1.2. The
candidate agrees, and the Reader validates the field as `M`.

**A-8. Section 2.5: `MAX_BOOTSTRAP_SCAN_BLOCKS` is used by no rule.** The
constant (1024) appears only in the constants table. No discovery step
scans for the bootstrap beyond the block at BOT.

**A-9. Section 10.4: the replica frame without diagnostics is not
described.** The frame's writer-version and timestamp rows are described as
"printable ASCII writer version" and "valid RFC3339 timestamp", with their
lengths stored separately. The text does not say whether a length of zero is
valid. Every candidate carries both fields, so none exercises this.

**A-10. Section 8.1.1: `schema_minor` is a recommendation.** A Writer
"SHOULD emit the current value". The candidate bootstrap carries 3, and so
does the builder's.

**A-11. REM-OBJECT Section 4.6.3, checked against the imported builder.** The
text bounds the pad search at "4 × `chunk_size` above `Rmin`". The builder's
`with_alignment_pad` bounds it at four chunks above the first congruent
target, which is at least `Rmin`. The two differ only in when a Builder must
fail with `Layout`, never in the bytes of a successful build, and the
recorded inputs never reach the bound. Every other choice the builder makes
for the recorded inputs is determined by the REM-OBJECT text: the ustar
field values (4.3.1), names and sizes (4.3.2), the checksum encoding
(4.3.3), record length and order (4.4.1, 4.4.2), the global and per-entry
keywords (4.5.1, 4.6.2), the alignment rule (4.6.3), chunk geometry (4.6.4),
the manifest schema and encoding (4.7.1, 4.7.2), an empty `object_metadata`
(4.7.6), and the EOF records and final fill (4.8). The builder does not
itself enforce Section 4.6.6's path and identity rules, so the wrapper that
calls it does. How the Object stream is laid onto tape records is
determined by REM-PARITY Section 4.1 and REM-OBJECT Section 8.2: one chunk
per block, in order.

## B. Deciding the cases

**B-1. Section 8.4: the hint path is a permission (cases 09 and 22).**
"A Scanner that cannot read the bootstrap MAY perform the discovery above
when it is given the three values Section 8.4.1 names." A Scanner that
declines has no other path forward: Section 8.4.1 offers the walk only when
A, B and C are all absent or invalid, which such a Scanner has not
established. *Decided:* this Reader takes the hint path. Section 12.1 names
the supplied UUID as the tape's identity when the bootstrap is unreadable,
and Section 8.4.1 says a Scanner "MUST accept" the supplied values. A
Scanner that declines the permission stops with `NoBootstrapFound`, so two
conformant Scanners can reach different outcomes. Section 1.5 says the text
exists to prevent that.
**Expectation:** bootstrap-hinted (case-09) and bootstrap-wrong-scheme
(case-22) both take the hint path, as this Reader does.

**B-2. Section 12.3: a terminal file with an unreadable head is classified
two ways (cases 05, 10 and 11).** "When the head is unreadable, a matching,
fully parsed terminal footer may establish the same type". The
unreadable-head rule, however, says the Scanner "MUST ... otherwise classify
the file as an object candidate". A Scanner that uses the permission reports
`TapeIndexReplica`; one that does not reports an Object candidate. The
validated scope is unaffected, because it ends at the ParityMap.
*Undecided:* `walk.classes`.
**Expectation:** replicas-all (case-05) takes the first reading: files 4 to 8
are "terminal components".

**B-3. The text names no Verifier outcome for damage before replica A
(cases 02, 03, 08–12, 14, 15, 19 and 21–25).** Enumerated with a search: the
text binds the Verifier only in Section 2.2 (its definition), Section 8.1 (the
bootstrap's trailing fill) and Section 10.6 (separation extents). The
Verifier "validates a tape's structures and digests end to end", yet no
outcome is given for an unreadable Object block, sidecar copy, parity shard
or ParityMap copy, or for a `SchemeMismatch` found while checking indexes.
*Decided:* `verifier.result` reports the terminal-suffix outcome, which
Sections 10.6 and 12.6 do define. *Undecided:*
`verifier.outside_terminal_suffix` lists what the Verifier finds before A.

**B-4. Section 10.6: an invalid separation extent has no outcome category
(case 17).** "A Verifier that finds a separation extent invalid MUST report
it and MUST NOT report the terminal suffix as complete." With all three
replicas valid, Section 12.6's categories (complete, degraded, conflict,
walk) do not apply. *Undecided:* `verifier`, between degraded and an
error. separation (case-17) pins only that the suffix is not complete,
which Section 10.6 decides; no expectation depends on the category.

**B-5. Section 10.6: "Its edition ID MUST equal the replicas'" has no
referent when the replicas disagree or none validates (cases 04, 05, 10, 11
and 18).** *Undecided* for case 04, where the three valid replicas carry two
edition IDs. *Decided* for the others. In case 18 the extents are compared
with the accepted edition, because an invalid replica "does not invalidate
an agreeing survivor" (Section 8.5). In cases 05, 10 and 11 no replica is
valid, and the extents are compared with the one edition ID that every
readable replica frame carries.

**B-6. Section 8.4: whether an unread payload counts as degraded evidence
is not settled (case 06).** A replica's payload "need be validated only for
the replica to be accepted", and Appendix C records that the selection order
among agreeing replicas moved to the Guide. When C's envelope validates but
its payload is unreadable, a Scanner that accepts A or B without reading C's
payload reports no degraded evidence; one that reads it reports the result
as degraded. *Undecided:* `scanner.degraded`. The acceptable selections (A,
B) are determined. The informative note of replica-c-payload (case-06)
describes the same gap, and its pinned key does not depend on it.

**B-7. Section 13.3: can the tail copy be located by arithmetic (case
25)?** The steps reach the tail copy through the footer (step 1) or through
the directory (step 3). When neither is available, the text neither permits
nor forbids locating the tail copy by the Section 9.4 layout
(`H + P` from block size, `S`, `m` and the range length). Appendix B.11 says
the footer "makes the tail copy findable without trusting block
arithmetic". *Decided:* the procedure is read as the complete list of
routes, so epoch 0 of case 25 is `SidecarMetadataUnavailable`. The case,
parity-map-and-sidecar, is not pinned, and its informative note says the
text does not decide epoch 0. The two readings survive, and the blind
decision took the first.

**B-8. Section 8.4 step 1: how to find the footer "from EOD" is not
specified.** This Reader spaces back over at most five filemarks from EOD
and takes the first record before a filemark that parses as a replica
footer. The text also does not say whether the layout may come from a footer
that is itself ineligible, as in cases 07 and 20: after the removed
filemark, C's footer sits one LBA below its recorded position. Neither
reading changes a decision here. In case 07, replica A is fully eligible
and its footer carries the same layout. In case 20 every footer sits one
LBA below its recorded position, so no replica can be eligible under any
layout, and the walk follows either way.

**B-9. Section 12.3: a sidecar rung's count mismatch leaves the file's
class unstated (case 20).** When the sidecar footer probe parses but its
total differs from the measured length, the result is "a hard error ...
scoped to that rung", and "Item 7 applies to a tape file that none of items
1 to 6 recognises". The text does not say what class the file then has.
*Decided:* the file's classification fails, so the walk cannot produce a
valid map, which is `FilemarkMapReconstruct`. The ParityMap's scope count
(4) also differs from its walked tape-file number plus one (3), which
Section 15 would call `FilemarkMapDigestMismatch`. The earlier failure is
reported.
**Expectation:** filemark-prefix (case-20) takes the other reading. Rung 6
does not classify the file, so item 7 applies and the file is an 11-block
Object candidate. This is the comparison's one disagreement, class (c). No
sentence excludes either reading: "recognises" is not defined, and "reports
the failed classification" can name the rung's failure or the file's.

**B-10. Section 12.3: a ParityMap that fails validation has no stated
class.** Items 2 and 3 say a matching terminal magic commits the file, and
Section 10.6 says malformed control never falls through to Object. Item 4
says nothing similar for a ParityMap whose magic matches but whose copies
fail. No case reaches this, because the walk here only meets ParityMaps that
validate.

**B-11. Section 13.2: the third refusal has no error name.** The text lists
three refusals and names two (`OutsideValidatedMapPrefix`,
`UnrecoverablePendingEpoch`). The third, "failed blocks or sidecars in tape
files outside the durable boundary", has none. No case reaches it.

**B-12. Section 13.5: `lost_count` is not defined.** The text gives
`Unrecoverable{stripe, lost_count, limit}` "carrying the stripe and the
counts" but does not define `lost_count`. This Reader reports the number of
erasures in the stripe, implicit zeros excluded, and `m` as `limit` (cases
21 and 23).
**Expectation:** burst-m-plus-one (case-21) and short-epoch-burst (case-23)
pin `lost` 3, and burst-m (case-19) pins `losses_per_stripe` [2, 2]. Both
take the same reading: every erasure in the stripe, the failed block
included, and no implicit zero.

**B-13. Appendix D TT-7: which bootstraps count as unreadable is open.** No
case depends on it: every unreadable bootstrap in these cases is a medium
error on the block itself.

## C. Criterion-2 evidence

**C-1. The image bytes are pinned only by size and SHA-256.** The images are
regenerated, not checked in, so a digest mismatch could be reported but not
located in the candidate. None occurred. The tests locate differences
against a build whose every digest matches the manifest.

**C-2. `STREAMING.tsv` records process properties as well as bytes.**
`structural_passes`, `object_passes` and `retained_rows` describe how the
generating program ran. This implementation also makes one pass over each
row kind and retains no rows, so its values agree, but the agreement says
nothing about the text.

**C-3. The maximum footer is named `bootstrap-footer.bin`.** It is the
footer record of terminal replica A. Section 10.4 says "The replica footer is
a record of the kind-4 tape file; it is not a kind-2 bootstrap". The name is
a fixture-naming defect, not a text defect.

**C-4. Some inputs are derived values.** The inputs record several derived
values: the profiles' `counts`, `terminal_layout` and `separation_extent.total_records`,
and the images' `stripes_per_neighborhood`, which is `S`. The builder
recomputes the derived values and cross-checks them rather than using
them.

**C-5. Nothing failed to re-derive.** Every artifact listed in the task was
re-derived from its inputs. No Object bytes had to be supplied from outside.

## D. Gaps the comparison surfaced

**D-1. Some expected-case fault lists omit faults their fault maps carry.**
`expected-cases.json` gives bootstrap-wrong-scheme as LBA 0 only, but its
`fault-map.json` also makes LBA 2, the failed data address, unreadable.
parity-map-both lists only the two ParityMap copies, and its fault map also
makes LBA 2 unreadable. The sidecar and walk cases describe their faults in
prose and omit the medium errors on their failed data blocks. The
`tape-images/README.md` explains this: "failed data addresses also produce
real medium errors". No outcome depends on the difference here. In
bootstrap-wrong-scheme the Recoverer stops at `SchemeMismatch` before
reading any shard. Elsewhere the extra unreadable block is the failed
address itself, which the Recoverer treats as an erasure either way.

**D-2. Three pinned keys have no field in the decisions schema.**
`object_sha256_matches`, `losses_per_stripe` and `records` have no place in
`rem-parity-second-implementation-decisions/1`. They were compared through
values computed after the decisions, in the same way as `bytes_match`. A
later schema could carry them.

**D-3. Some expected labels use terms the text does not define.** "Terminal
components" (replicas-all) was read as `TapeIndexReplica` or
`IndexSeparationExtent`. "Inventory as the image's expected.json"
(bootstrap-hinted) was read as the structural rows before replica A, with
the block counts, start LBAs and Object sizes that the image's
`expected.json` lists. That file also lists the terminal suffix, which the
inventory excludes (Section 10.2).

## E. Resuming a tape (Section 14)

These were found while deciding the eight resume cases, before any
expected outcome for them was read.

**E-1. Section 14 step 3 names no error for its fatal read
(resume-01).** "a boundary or short read where data is expected is fatal",
but no error is named. Section 15's `ResumeAppend` is "Section 14 invariant
violation", and `TapeIo` is a "transport/medium failure (not a format
violation)". In resume-01 the prefix records a 3-block tape file 3, but
the tape holds 2 blocks and a filemark. The re-read of ordinal 6 meets that
filemark at LBA 17. *Undecided:* `decision.error`, between `ResumeAppend`
and `TapeIo`.

**E-2. Section 14 does not say where a Resumer obtains `S` and `k`
(resume-03).** Step 2's bound `T − W < S × k` needs the scheme, and a
committed prefix of Section 7.1 entries does not carry it. The claim
rests on a search of every mention of the Resumer and of resuming: in
Sections 1.3, 2.2, 3.4, 11.1 and 14, and in Appendix B.10. None gives a
source. This Resumer reads the bootstrap at LBA 0. A Resumer whose own
off-tape state carries the scheme refuses resume-03 before any tape read.
*Undecided:* `decision.before_any_tape_read` for resume-03. The other
step-2 refusals are decided from the prefix alone, before any read.

**E-3. Section 14 does not check the prefix against the tape outside
`[W, T)`.** Step 3 re-reads only the open epoch, and step 4 computes the
append point from the prefix alone. A prefix that misstated the length of
a protected tape file would put the append point inside committed data or
short of it, and nothing in Section 14 would notice. resume-01's
mismatch is caught only because the misstated file lies in `[W, T)`. No
case exercises a mismatch elsewhere.

**E-4. Section 14 has no rule for a finalized tape.** Section 11.3 says
"A finalized tape accepts no further appends", but Section 14 gives a
Resumer that relies on a portable prefix no way to detect finalization.
The prefixes of resume-04 and resume-06 cover only tape files 0–3 of the
finalized two-epoch image, which also carries a ParityMap and a terminal
suffix. Step 2 refuses both first, so no decision depends on this.

**E-5. Step 2 does not list the density of Object first ordinals.**
Section 7.2 requires Object first ordinals to be "dense and contiguous from
0 in tape order", but step 2 lists only four rules. resume-08 breaks
density: its second Object starts at ordinal 1. It is refused by the listed
rule that the final Object entry end exactly at `T`. A prefix whose Objects
overlap but whose last Object still ends at `T` would pass step 2's list,
unless Section 3.4's "validated combination" is read to include Section
7.2's validity rules.

**E-6. Closing a short epoch at the end of the session is a Writer
choice.** A barrier "may" close a non-empty short epoch (Section 11.2). The
resume task says the session ends with a sidecar for the open epoch, and
resume-05's short sidecar (epoch 1, `[4, 6)`) exists only under that
choice. The uninterrupted-session check makes the same choice (compare
A-4).

**E-7. Section 14 step 3 gives the re-read blocks nothing to be verified
against.** No sidecar yet covers `[W, T)`, so a Resumer that re-reads a
silently corrupted block there cannot detect it, and its new parity would
protect the corrupted bytes. No case exercises this.

## F. Generation-2 negatives (design D6)

These were found while applying and deciding the 58 negative cases, before
any expected outcome for them was read.

**F-1. Section 9.5: "with `primary_header_start_block` as 0" can be read two
ways (neg-06).** It can describe a field that must be 0, in which case the
hash covers the exact wire bytes. Or it can tell the Reader to substitute 0,
in which case a nonzero wire value is hashed as 0. The two readings give
opposite hash results for neg-06's two variants. The outcome does not
depend on the reading, because the rule "`primary_header_start_block` MUST
= 0" fails in both.

**F-2. Appendix C's statement about geometry disagreements (neg-08, 19, 20,
23, 34).** Appendix C (informative) says "Geometry and ordinal-range
disagreements raise `SchemeMismatch`". The normative text makes `k`, `m`
and `S` ≠ 0, and the block size equal to the actual one, copy-validity
rules of Section 9.2 (`SidecarParse`). It pins only an acquired index
against the scheme ("then pinned", Section 13.3). *Decided:* `SidecarParse`
for a copy that breaks a Section 9.2 rule. `SchemeMismatch` applies to a
copy that validates but disagrees with the bootstrap or the supplied
scheme. An implementation that follows the appendix could report the other
name.

**F-3. Section 9.1: "reject divergence" does not say what is rejected
(neg-11).** When both copies parse and disagree, the text does not say
whether the Reader rejects one copy, both, or the epoch. A Verifier reports
the divergence (`SidecarParse`). A Recoverer on step 1 finds that only the
primary matches the footer's hash, and may use it. It may also refuse the
epoch because the copies diverge. This implementation uses the primary.

**F-4. Section 10.1.4: which block size the ParityMap locator uses
(neg-14).** "`M` from `payload_len` and `block_size`" may mean the header
field or the tape's block size, and Section 10.1.3 gives the header's
`block_size` field no constraint. The sidecar's field, by contrast, "MUST
equal the actual block size". neg-14 is rejected under either reading.
This implementation also requires the field to equal the tape's block
size, which is stricter than the text.

**F-5. No role checks an inventory's counts against positions (neg-44).**
No Section 10.6 condition bounds a sidecar row's block count beyond
non-zero. The text does not require replica A's planned start LBA to equal
`Σ(block_count + 1)` over the structural rows. An inventory can therefore
claim a 2^64 − 1-block sidecar and still validate. A role that later
computes a position from it must reject the overflow (Section 2.4), but the
text names no such role and no error. *Decided:* the replicas are
accepted. Which role rejects, and with which name, is undecided.

**F-6. Section 15's names overlap for an invalid bootstrap (neg-02, 12, 30,
57).** `NoBootstrapFound` is "absent or invalid" and `BootstrapParse` is
"violates Section 8". For a declared length beyond the block,
`BootstrapPayloadTooLarge` ("cannot fit the block") also fits. No rule
picks one, and Appendix D TT-7 records that which bootstraps count as
unreadable is open. *Undecided:* the error name; rejection itself is
decided.

**F-7. Section 9.6 gives the footer's total no rule of its own (neg-40a,
54c).** The footer table states `tail_header_start_block = H + P` and
`primary_header_start_block = 0`, but no formula for
`sidecar_total_block_count`. A footer whose total is wrong is rejected only
by comparison: with the measured length (Section 12.3 item 6) or with the
map entry (Section 13.3 step 1). Neither names an error. *Undecided* for
neg-54c: `SidecarParse`, or no typed error. Before D6 this implementation
also checked `total = 2H + P + 1` in the footer, which is stricter than the
text; that check was removed (BUILD-LOG).

**F-8. Section 13.4 defines implicit zeros by ordinal but prescribes no
computation (neg-05).** For a descriptor near 2^64, forming the peer
ordinal overflows, while classifying the peer by position (`data_index·S +
stripe ≥ real_data_shard_count`) does not. *Undecided:* whether a Recoverer
must reject. The case is unit-level only.

**F-9. The walk's outcome for an inconsistent device report is unstated
(neg-04).** Section 16.2 makes arithmetic on tape-derived values checked,
so a zero position delta must not wrap. The text does not say whether the
walk reports structural damage or a transport failure. *Undecided.*

**F-10. Fixture notes (no text defect).**
- neg-02, neg-30 and neg-57 describe the same bytes, the bootstrap with
  S = 2^63, under three targets.
- neg-27 gives its mutation as "as overflow-10.1.2-M", and neg-02 and
  neg-57 give their repairs as "as overflow-6.6-scheme-product". These name
  other cases by labels that `cases.json` does not define. I resolved them
  to neg-45 (the formula `M = ceil((0xC8 + L) / B)`) and to neg-30 (the
  formula `S × (k + m)`). Both resolutions follow the formulas, not a
  guess about the outcome.
- R-SC-HASHED recomputes the hash "over the mutated copy's own bytes". Where
  a mutation changes the entry counts (neg-08, 09, 19, 20, 34, 40b, 43, 49),
  I hash the entry bytes physically present: 96 bytes, or 88 and 104 for
  neg-58's variants, whose companion edits change the stream. Every such
  copy is rejected by a Section 9.2 rule whatever its hash.
- The neg-33 precondition says "flip a byte". I XOR 0x01 into the CRC byte
  named.

## G. Supplemental single-rule negatives

These were found while applying, isolating and deciding the 49
supplemental variants, before any expected outcome for them was read. The
comparison of the earlier negatives (`NEGATIVES-COMPARISON.md`) was not read
before these decisions were written.

**G-1. Section 9.2: which H and P the locator formulas use (sup-10,
sup-37).** The rules `sidecar_total_block_count = 2H + P + 1`,
`tail_header_start_block MUST = H + P` and `footer_block_index MUST = 2H +
P` do not say which H and P they mean when a header's fields disagree with
their definitions. The table labels the 0x60 field "(H)". Section 9.1
defines "`P = S × m`" and says H is "the header/index copy block count
(Section 9.4)". Under the reading "H = the 0x60 field, P = S × m", each of
sup-10 and sup-37 breaks only its named rule. This implementation's parser
uses the other reading, "H = the Section 9.4 recompute, P = the 0x50
field", under which each breaks three more rules. The name is
`SidecarParse` either way.

**G-2. Checked arithmetic when the formula's result is representable
(sup-01, sup-08).** For sup-01, M = 2^46 + 1 fits in u64 although
0xC8 + L does not. For sup-08, the inequality 1 ≤ 2^64 holds although
count × B overflows. For the Section 10.4 formulas, Section 10.6 settles
the question ("Every size formula of Section 10.4 evaluates without
overflow"), and Section 10.6 extends it to Section 10.5. For the ParityMap's
M and the Section 10.3 byte-length rule, the text states the formula but
not the order of its operations. It does not say whether a Reader must
evaluate the formula as written, overflowing intermediate included.
*Undecided:* sup-01 and sup-08.

**G-3. Unit-level rejections without a §15 name (sup-23, sup-25,
sup-27).**
- **sup-25:** an overflow in the Recoverer's parity locator has no name.
- **sup-23 and sup-27:** S × k and S × m take their name from the role
  that evaluates them. From a bootstrap the name is open (F-6); from a
  Resumer's bound it is `ResumeAppend`.

Rejection itself is decided for all three: the value is not representable
in u64.

**G-4. A sidecar range longer than S × k (sup-14).** The copy validator
rejects it (Section 9.2, "real ... MUST be in 1..=S × k"). The text does not
say whether replica validation ("Every structural and ordinal-range
invariant is validated", Section 10.2), with an epoch covering "at most `S ×
k` data ordinals" (Section 2.3), or the directory decoder must also reject
it. This implementation checks neither, so its Scanner accepts the
inventory.

**G-5. Section 10.1.4: the scope of "reject disagreement between header,
footer, and the measured tape file length" (sup-12).** It may cover only
the locator arithmetic named in the same sentence, or every header field.
This implementation's ParityMap parser takes the broad reading, so it
rejects sup-12 on the header/footer comparison rather than on the
payload/footer match the variant names. The name is `ParityMapParse` either
way.

**G-6. Section 10.1.5: the order of the epoch-id rule (sup-35).** "epoch_id
values are unique and consecutive starting from 0 (0, 1, …, count−1)" does
not say whether the entries are read in array order or in tape-file order.
The partition rule does say ("taken in ascending `tape_file_number` order").
With the entries reversed, the ids hold as a set and in tape-file order, and
fail in array order. This implementation's decoder checks both the range
chain and the ids in array order. For the chain that is stricter than the
text, a known divergence recorded in BUILD-LOG; it changes no decision,
because the ascending rule fails first.

**G-7. Section 7.4's cross-checks and their §15 name (sup-32, sup-41).**
Section 15's `TerminalIndexReplicaParse` cites Sections 8.3, 10.2–10.4 and
10.6, not Section 7.4. Section 10.6's list includes "scope", so a failed
scope cross-check is read as `TerminalIndexReplicaParse`.
`FilemarkMapDigestMismatch` does not apply, because the projection digest
matches. For sup-32 this implementation evaluates "W equals T" on
recomputed values rather than on the recorded fields, so it also reports
that rule. The text names the recorded fields
(`highest_protected_ordinal`, `total_data_ordinals`). This is a known
divergence in BUILD-LOG.

**G-8. Fixture notes (no text defect).**
- The variants name one another by labels `supplement.json` does not
  define. I resolved each to the variant with the same parent that states
  the base in full: "isolated-1" to sup-17 (for sup-16) and to sup-29 (for
  sup-07 and sup-20), and "isolated-3" to sup-21 (for sup-32).
- They also cite a `NOTES.md` that I was not given.
- sup-14's five-block Object and sup-15's generated profile have details
  the variants do not specify. My choices are listed in the entries'
  `apply.notes`, and no decision depends on them.


## H. Mutated-block digests, terminal mutations and survivor sets

These were found while comparing the negatives' mutated blocks with
`tape-images/negatives/MANIFEST.tsv`, and while deciding the 50 terminal-index
mutations and the 14 survivor sets. The mutations and survivor sets were
decided before any expected outcome for them was read, and none has been
compared with one.

Three earlier entries recur. B-6 (an unread payload and degraded evidence)
leaves `tape.degraded` open for the five mutations that damage only replica
A's payload: mut-09, mut-22, mut-30, mut-34 and mut-36. B-4 (no category for
an invalid separation extent) leaves the Verifier's category open for the 14
mutations that damage only extent A-B. B-5 is decided as before: in mut-05
the extents are compared with the accepted edition.

**H-1. Section 6.2: no parity is defined for an epoch longer than `S × k`
(sup-14).** The generator is defined by "X_j = k + j (j in 0..m) Y_i = i (i in
0..k)", so it has a column only for data indices below `k`. sup-14's sidecar
protects `[0, 5)` at `k = 2` and `S = 2`. Ordinal 4 then has data index 2,
which has no column, and the text determines no parity for the epoch. The
variant says the same: "Parity bytes are arbitrary". My build encodes the
first `S × k` ordinals. The pinned blocks keep the base image's parity. Six
pinned blocks therefore differ. In each, the first differing byte is a field
derived from the parity CRCs: `canonical_metadata_hash` in both header copies
(offset 0x98) and in the footer (0x60), and `payload_sha256` in the three
ParityMap blocks (0x38). My four parity blocks appear only on my side,
because the pinned parity equals the base's. A diagnostic candidate with the
base parity reproduces all 26 pinned digests of the case; it is used only to
locate the differences.

**H-2. The profile that sup-15 generates is not determined by its
variant.** The variant fixes the structural rows, the Object rows' counts and
manifest fields, and `T = W = 0`, and leaves the rest to the generator: the
tape UUID, edition, diagnostic text and extent size, the Object ids and
manifest digests, and replica A's start LBA ("the profile may present A at
any LBA the harness chooses"). No rule of the text could fix those values. My
choices are listed in the supplement decision's `apply.notes`. All 13 pinned
blocks of the case differ from mine. The manifest pins only digests and no
candidate reproduces them, so the first differing byte cannot be located.

**H-3. Fixture note: the manifest lists some blocks whose bytes did not
change.** 28 manifest rows name a block that my mutation leaves
byte-identical to the base. Twelve are the footer and ParityMap blocks of the
three supplement variants confined to hash-excluded fields (sup-09, sup-26,
sup-40), whose convention says such a mutation "changes neither the footer
nor the directory". Three are the ParityMap blocks of sup-06, which keeps the
original hash. The other 13 are blocks of the four rebuilt images (sup-11,
sup-14, sup-37, sup-45) that the rebuild wrote with unchanged bytes: the
separation-extent interiors, sup-45's four parity blocks and sup-14's Object
block 1. In every one, the pinned digest equals my block's. This implementation emits a block only when its bytes
differ from the base's at the same key. No text defect is involved.

**H-4. Section 8.4 step 1: an ineligible footer as the source of the layout
now decides outcomes (sel-08, sel-10; extends B-8).** Step 1 says "locate,
from EOD, the local footer of a terminal replica, and from it the planned
terminal layout". It does not require that footer to be eligible first. The
eligibility rule ("Footer-local observed positions MUST agree with the
footer's planned shared layout before that replica is eligible") speaks of
the replica, not of the layout. "When footers propose different planned
layouts, Section 8.5 decides which replicas are accepted" implies that a
Scanner may meet more than one layout, but not that it must look for one. In
sel-08 and sel-10 the first replica footer found from EOD is that of a
replica from the minimal profile, planned for the minimal tape's
coordinates. A Scanner that takes its layout finds no valid replica at any
coordinate it plans, and walks from BOT (`BotStructuralRecoveryRequired`). A
Scanner that passes over a footer whose positions the device contradicts, or
that considers every footer's layout, reaches B's footer and the profile's
plan. It then accepts B (sel-08) or A and B (sel-10), degraded. Two
conformant Scanners reach different outcomes. *Undecided:* `tape.outcome`.
This implementation's Scanner takes the first footer that parses (B-8), so
it reports the walk.

**H-5. Section 3.5 and Section 10.6: a record that is not one full block
(mut-01, mut-18, mut-26, mut-27, sel-01).** Section 3.5 says that "a read
returning other than exactly one block is an error" and classifies only the
Filemark and EndOfData outcomes. Section 8.4 says that "a non-medium
transport error aborts discovery". A short record could therefore be read as
a transport error that aborts discovery. Section 10.6, however, lists
"record length" among the frame conditions a replica or extent must meet,
and Section 15 requires I/O faults to "remain distinct from format
violations". A short record on the medium is a format violation, not a
fault of the transport. *Decided:* a record that is not one full block
fails the record-length condition, so the component is invalid and discovery
continues. Under the other reading, a Reader that reads the short record
would abort: mut-26 would end in a transport error at replica A's footer
instead of an inventory from B and C, and sel-01 would end at the first
footer read from EOD instead of in `BotStructuralRecoveryRequired`.

**H-6. Fixture note: the event row mut-35 defines no mutation.** Its
description says that "No exact byte effect is defined". The decision is
made for the bytes as they stand, which are the base profile's. Its note
says what the text would decide under the two meanings the description
names, the removal of replica A's trailing filemark and an edit of a
component tuple's filemark count, and marks both as informative.

**H-7. Fixture note: status S3 does not say whether an absent replica's
trailing filemark remains (sel-07).** "the component's records are absent"
leaves the filemark unstated. With all three replicas absent, no replica
footer exists under either model, so no replica validates and the outcome is
`BotStructuralRecoveryRequired` under both. The selection run checks both
models.

**H-8. Sections 10.6 and 12.3: a separation position that holds a replica
header (mut-33).** Read as the planned extent A-B, the tape file's header
fails the separation role magic, and "A magic or CRC miss invalidates that
candidate": `TerminalIndexSeparationParse`. The header carries the
terminal-replica header magic, however, and "a matching role magic commits
the tape file to its control type". Read that way, tape file 7 is a damaged
terminal replica whose frame plans itself at tape file 6:
`TerminalIndexReplicaParse`. The text fixes no order between the two
readings. *Decided:* the component is rejected, and the name is the set
{`TerminalIndexSeparationParse`, `TerminalIndexReplicaParse`}. The tape-level
outcome does not depend on it.

**H-9. Section 10.4: digest preimages that hold a constant or the tape's
identity where the frame holds a field (mut-02, mut-17, mut-28, mut-32).**
The edition preimage holds "compression_mode:u32=0", the replica descriptor
preimage holds "replica_count:u16=3", and each preimage holds a
`tape_uuid[16]` that may be the tape's identity or the frame's own field.
When a mutation changes the frame field and leaves the digest stale, a
Reader that hashes the constant or the tape's UUID finds the digest valid,
and one that hashes the field finds it invalid. Each such mutation also
breaks the field's own rule, so no decision depends on the reading. This
implementation hashes the constants and the tape's UUID.

**H-10. Section 15: the canonical-map digest condition has two names
(mut-39).** Section 10.6 lists the canonical-map digest among the payload
conditions of a replica, whose failures Section 15 names
`TerminalIndexReplicaParse`. Section 15 also names a "replica structural
projection digest mismatch" `FilemarkMapDigestMismatch`. In mut-39 the stale
edition digest fails as well, and the text fixes no order. *Decided:* the
component is rejected, and the name is the set
{`TerminalIndexReplicaParse`, `FilemarkMapDigestMismatch`}.
