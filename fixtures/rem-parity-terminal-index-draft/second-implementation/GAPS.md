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
