# Decision log

This log records every decision changed after the blind decisions were
written. Each row gives the case, the aspect, the old value, the new value,
and the sentence that decides it. Nothing may change without a row. The
first section covers changes after the comparison with the expected
outcomes. The second covers changes made to follow the revised text (F0).
No row in either section follows an expected outcome.

| Case | Aspect | Old value | New value | Sentence |
| --- | --- | --- | --- | --- |
| All eight resume cases (resume-01 to resume-08, and the same cases by real id) and neg-56 | The quoted text of Section 14 step 1 in their citations (nine citations per resume file, one in `negative-decisions.json`) | "Derive the committed prefix from the off-tape commit records (Section 3.4), dropping any torn tail, and compute `W` and `T` from it." | "Derive the committed prefix from the off-tape commit records (Section 3.4), dropping the tape's torn tail, if any, and compute `W` and `T` from it." | None: the text's wording changed after these decisions were written (Appendix C records it as a wording follow-up that changes no requirement). No decision changed; a leaf-by-leaf comparison of each regenerated file with its predecessor finds only this string. |

No decision changed after the comparison. The one row above records a cited
sentence whose wording the text changed; the decisions that cite it are the
same. The comparison found one disagreement, filemark-prefix (case-20), and
classified it as (c): the text did not decide it. A class (c) disagreement is
not a misreading, so the blind `decisions.json` stood and no
`decisions-revised.json` exists. The revised text now decides it (below).

## Changes for the revised text (F0)

The text was revised in Sections 2.2, 2.4, 3.3, 3.5, 7.2, 8.4, 9.1, 9.6,
10.1.3, 10.1.4, 10.6, 12.3, 12.6, 13.3 to 13.5, 14, 15 and 16.3, and in
Appendix D. Each decision below was re-decided from the revised sentence it
cites, and no row follows an expected outcome. Every decision file was
regenerated. A leaf-by-leaf comparison with its predecessor finds only the
changes listed here, together with the replaced quotes and the citations
that follow from them. The damage-case rows apply to `decisions.json` under
the opaque ids in `blind-mapping.json` and to `decisions-real-ids.json`. The
resume row applies to `resume-decisions.json` (resume-01) and to
`resume-decisions-real-ids.json`.

### Decisions that changed

| Case | Aspect | Old value | New value | Sentence |
| --- | --- | --- | --- | --- |
| filemark-prefix (case-20) | `walk.classes` of tape file 1, and `walk.failed_classifications` | "classification failed" | `Object`; the failed classification is reported: "sidecar footer total 7 differs from the measured 11 blocks (Section 12.3 item 6)" | 12.3: "For items 1, 4, 5 and 6, a rung *recognises* a tape file only when the file passes every check of that rung, its measured block count included; a file that fails one of them is not recognised." "Under items 2 and 3 the file keeps its control type, damaged; under items 4, 5 and 6, as under item 1, the file is not recognised, and item 7 applies." "A rung that parses a file but fails its measured count reports the failed classification." |
| filemark-prefix (case-20) | `walk.error` | `FilemarkMapReconstruct` | `FilemarkMapDigestMismatch` | With file 1 an Object, the walk yields a map. Its prefix does not match the final ParityMap's scope fields. 13.1: "A walked map whose projection does not hash to a validated final ParityMap's `canonical_map_digest`, or whose prefix disagrees with those scope fields, is not validated and gives the Recoverer no map, with no fallback to the bootstrap's scope." 15: "FilemarkMapDigestMismatch replica structural projection digest mismatch, or a walked map's projection or scope fields disagree with the final ParityMap" |
| filemark-prefix (case-20) | `walk.object_identity` | null | `unknown` | 8.4.1: "It reports each candidate's identity as unknown unless a separate exact Object-recovery authority succeeds." |
| filemark-prefix (case-20) | `scanner.replicas.{A,B,C}` reasons (the outcome, `BotStructuralRecoveryRequired`, is unchanged) | "TerminalIndexReplicaParse: role magic does not match", measured against C's footer's plan | no planned layout: each footer found from EOD records its position one LBA above where it is read | 8.4 step 1: "A terminal replica's footer that parses supplies a planned layout only when its recorded footer position equals the position at which it was read, and the layout's planned EOD is at or after the tape's EOD." "... when no footer supplies a layout, no replica validates;" |
| replicas-all (case-05), walk-sidecar-isolation (case-10), walk-directory-rescue (case-11) | `walk.classes` of the three terminal replicas whose heads are unreadable | `undecided` | `TapeIndexReplica` | 12.3 item 2: "When the head is unreadable, a matching, fully parsed terminal footer establishes the same type, after its measured count is checked." |
| replica-c-payload (case-06) | `scanner.degraded` | `undecided` | `per reads`: false when the Scanner does not read C's payload, true when it does (`scanner.degraded_by_reads`) | 12.6: "An inventory is *degraded* when the Scanner found a replica of the planned layout missing or invalid." "A Scanner need not read every replica's payload (Section 8.4 step 2), so a replica whose payload it did not read is not found invalid for that reason; a Verifier's full check reports every replica." |
| bootstrap-hinted (case-09), bootstrap-wrong-scheme (case-22), burst-m (19), burst-m-plus-one (21), object-head (02), parity-and-data (12), parity-map-and-sidecar (25), parity-map-both (24), short-epoch-burst (23), short-epoch-recoverable (14), sidecar-footer (15), sidecar-primary (08), sidecar-primary-and-footer (03), walk-directory-rescue (11), walk-sidecar-isolation (10) | `verifier.outside_terminal_suffix` | `undecided` (whether and how the Verifier reports damage before replica A) | each finding before replica A, with the error a Reader reports: `TapeIo` for a medium error, `ParityMapParse`, `SidecarMetadataUnavailable`, `SchemeMismatch`. Findings on data blocks and parity shards stay `undecided` under a narrower aspect; bootstrap-hinted has none, so it is fully decided | 2.2: "It reports damage it finds before the terminal suffix with the error a Reader reports for that component." Appendix D keeps the data checks open: "Whether a full verification checks every data block and parity shard, or only structure and metadata, is to be decided, and the reference brought into line, before freeze." |
| resume-boundary-read (resume-01) | `decision.error` | `undecided` | `ResumeAppend` | 14 step 3: "A read that finds a filemark, EOD or a record shorter or longer than one block where the committed prefix places a data block contradicts the commit record, and is `ResumeAppend`; a medium or transport failure is `TapeIo`." |
| neg-02, neg-57 | `decision.error` | `undecided` (`BootstrapParse` or `NoBootstrapFound`) | `BootstrapParse`, the parser level; `by_level` gives `NoBootstrapFound` for discovery without supplied values, and `BootstrapParse` for discovery with them (the decodable scheme disagrees) | 15: "The bootstrap names depend on the level at which a Reader meets the block." "A parser given one block as the bootstrap reports a breach of Section 8 as `BootstrapParse`, including a missing magic and a failed CRC, except that a parity bootstrap recording drive compression is `DriveCompressionEnabled`." The targets name a Reader using the bootstrap's scheme, not discovery (GAPS I-5). |
| neg-12/a-one-past, neg-12/b-hostile-max, neg-30 | `decision.error` | `undecided` (`BootstrapParse`, `BootstrapPayloadTooLarge` or `NoBootstrapFound`) | `NoBootstrapFound`, the level the target names ("Scanner discovery"); `by_level` gives `BootstrapParse` for a parser | 15: "Discovery over the candidate block sizes, without supplied values, reports `NoBootstrapFound` when it finds no usable bootstrap at any candidate, and `DriveCompressionEnabled` when a parity bootstrap records compression; a record of another length only rules out that candidate." `BootstrapPayloadTooLarge` is a Writer error (Section 15). |
| neg-04 | `decision.error` | `undecided` (structural damage or `TapeIo`) | `TapeIo` | 2.4: "... and to positions a device reports, which are `TapeIo` when they do not fit or when they go backwards: the device, not the tape, is then at fault." 7.2: "A walked map's positions are device reports, so for it Section 2.4's `TapeIo` applies." |
| neg-05 | `decision.outcome` | `undecided` (reject or recover) | `accepted`: the peer is an implicit zero | 3.3: "That decision compares the exact value of `o` with the protected end, as Section 2.4 requires, so a position whose `o` would not fit in u64 is an implicit zero, and is not rejected." |
| neg-44 | `decision.outcome` and the tape-level selection | `accepted`, inventory from A, B and C | `rejected`, `TerminalIndexReplicaParse`; no replica validates, so `BotStructuralRecoveryRequired` | 7.2: "Every record position and every trailing filemark position that the map describes, by Section 3.2's `LBA(f, b)`, MUST fit in u64; a recorded map that breaks this is invalid." 15: "`TerminalIndexReplicaParse` includes a replica map that describes a position that does not fit (Section 7.2)." |
| neg-11 | `decision.note`: the Recoverer's consequence (the Verifier's `SidecarParse` is unchanged) | open (GAPS F-3): use the primary or refuse the epoch | the Recoverer uses the primary, which the valid footer vouches for, and recovers | 9.1: "When both copies are valid and their canonical metadata hashes (Section 9.5) differ, a Recoverer MUST use a copy that the footer or the sidecar epoch directory vouches for and neither contradicts, and MUST reject both when neither is available (Section 13.3)." 9.1: "A Verifier MUST read every sidecar's primary copy, tail copy and footer, and MUST report a divergence between the copies as `SidecarParse`, even when the footer or the directory decides it." |
| sup-01 | `decision.outcome` | `undecided` | `accepted`: M's exact value, 2^46 + 1, fits | 2.4: "Every formula in this document denotes its exact integer value." "An intermediate overflow in one way of writing a formula is not a format violation, and a Reader MUST NOT reject for it." |
| sup-08 | `decision.outcome` | `undecided` | `accepted`: the product 2^64 is only compared with `manifest_size_bytes` | 2.4: "A value that is only compared is compared exactly, and is never too large." |
| sup-23 | `decision.outcome` | `rejected` (name undecided) | `accepted`: in both roles the product `S × k` is only compared | 2.4: "A value that is only compared is compared exactly, and is never too large." |
| sup-31 | `decision.outcome` | `rejected`, `TerminalIndexReplicaParse` | `accepted`: `payload_padding_bytes` is exactly 64 | 10.6: "As Section 2.4 defines it, a formula evaluates without overflow when its exact value fits in u64." 2.4: "An intermediate overflow in one way of writing a formula is not a format violation, and a Reader MUST NOT reject for it." |
| sup-25 | `decision.error` | `undecided` | `SidecarParse` (the sidecar whose H places the read) | 2.4: "A Reader rejects a value that is recorded, or used as a position, a count or an ordinal, when its exact value does not fit the field or the u64 that holds it; the error is the one named for the structure that carries the value." |
| sup-27 | `decision.error` | `undecided` | `BootstrapParse` (the bootstrap's scheme record carries S; parser level) | The same sentence of 2.4, and 15: "A parser given one block as the bootstrap reports a breach of Section 8 as `BootstrapParse`, ..." |
| mut-38 | `tape.outcome`, `tape.acceptable_selections`, `tape.degraded`, `verifier.result` | inventory from A, degraded; Verifier degraded | `BotStructuralRecoveryRequired`; Verifier `recovery_required`. The extra record moves B, B-C and C one LBA later, so their footers are not where they say. A's footer is where it says, but its layout plans EOD 45 before the tape's EOD 46 | 8.4 step 1: "A terminal replica's footer that parses supplies a planned layout only when its recorded footer position equals the position at which it was read, and the layout's planned EOD is at or after the tape's EOD. A layout whose planned EOD lies before the tape's EOD may be followed by later tape files, so it is not used, and when no footer supplies a layout, no replica validates;" |
| mut-09, mut-22, mut-30, mut-34, mut-36 | `tape.degraded` | `undecided` | `per reads`: false when the Scanner does not read A's payload, true when it does | 12.6, as the replica-c-payload row. |
| sel-08 | `tape.outcome` | `undecided` (walk, or inventory from B) | inventory from B, degraded. C's footer, from the minimal profile, records the minimal tape's position, so it supplies no layout, and B's footer supplies the profile's | 8.4 step 1, as the mut-38 row. |
| sel-10 | `tape.outcome` | `undecided` (walk, or inventory from A and B) | inventory from A and B, degraded, for the same reason | 8.4 step 1, as the mut-38 row. |

### Reasoning that changed, with the decision unchanged

| Cases | What changed | Sentence |
| --- | --- | --- |
| neg-01, neg-03, neg-15/a, neg-15/b, neg-17, neg-35, neg-42 | The order and formula no longer reject a checked-arithmetic overflow. Each exact value is compared with its recorded field and differs. Outcome and name unchanged. neg-03 also cites Section 7.2, which its row 3 breaks | 2.4: "A value that is only compared is compared exactly, and is never too large." 7.2 (neg-03), as the neg-44 row |
| neg-27, neg-45, neg-46 | The intermediate sum or product is no longer a violation. The exact value (M = 2^46 + 1; padding 64) fits and differs from the recorded field, so the rejection comes from that rule. Outcome and name unchanged | 2.4: "An intermediate overflow in one way of writing a formula is not a format violation, and a Reader MUST NOT reject for it." 10.6: "As Section 2.4 defines it, a formula evaluates without overflow when its exact value fits in u64." |
| neg-09 | The closed-form overflow path is replaced by the exact-value reading; `SidecarParse` unchanged | 2.4, as above |
| neg-14 | The ParityMap's `block_size` (2) is not the tape's, which decides the first failing rule. The locator uses the tape's block size, and M = 2^46 is compared exactly; `ParityMapParse` unchanged | 10.1.3: "`block_size` MUST equal the tape's block size, which is the length of every block read, as the sidecar header's `block_size` must (Section 9.2)." |
| neg-33/a, neg-33/b, sup-16, sup-17, sup-11, sup-45 | The directory entry's total is not 2H + P + 1, so the entry is not available, no rescue read is placed and the epoch is `SidecarMetadataUnavailable`, as before. Previously a tail position underflowed | 13.3: "The rescue also requires the entry's `sidecar_total_block_count` to equal `2H + P + 1`, with `H > 0`. An entry that fails either precondition is not available." "The Recoverer then reports `SidecarMetadataUnavailable` for the epoch, whatever each copy's own failure was." |
| sup-44 | With the footer unreadable, the directory entry is available, and its hash equals the primary's, so the Recoverer recovers. The Verifier reports `SidecarParse`, as before | 13.3: "When a sidecar epoch directory entry is available (step 3), it decides as the footer would: a copy is used only if its `canonical_metadata_hash` equals the entry's." 9.1, as the neg-11 row |
| sup-12 | The dispute states the decided scope: the header/footer comparison fails as well as the payload/footer match | 10.1.4: "The agreement between a header copy and the footer covers every field that both carry, other than `copy_kind` and the CRC, not only the locator fields." |
| sup-32 | The implementation evaluates `W = T` on the recorded fields, so it finds one failing rule, not two. The isolation dispute is gone (GAPS G-7) | 10.6: "both are the recorded fields, and Section 7.4 separately requires each to equal the value recomputed from the map;" |
| sup-15 | The implementation finds a second failing rule, Section 7.2's position rule, so the variant is disputed as not isolated (GAPS I-1). The name, `TerminalIndexReplicaParse`, is unchanged | 7.2, as the neg-44 row |
| mut-01, mut-18, mut-26, mut-27, sel-01 | A record shorter than one block is cited as invalid content of the component, not a reading of GAPS H-5 | 3.5: "A Reader treats it as invalid content of the component it belongs to: an invalid candidate for a control component, an erasure when the Recoverer reads a data block or parity shard (Section 13.4), and, for the first record with a supplied or known block size, the refusal of Sections 8.4 and 15." |
| mut-04, mut-07 | The layout now comes from A's footer: C's footer is one LBA below its recorded position. Outcome (inventory from A, degraded) unchanged | 8.4 step 1, as the mut-38 row |
| sel-11, sel-13 | The foreign replica's footer supplies no layout, which is now stated. Outcome unchanged | 8.4 step 1, as the mut-38 row |
| bootstrap-hinted, bootstrap-wrong-scheme, bootstrap-unhinted | The hint path is required, not a permission, and discovery without supplied values reports `NoBootstrapFound`. Citations only | 8.4: "A Scanner that cannot read the bootstrap, and is given the three values Section 8.4.1 names, MUST perform the discovery above with them." |
| neg-07/c, neg-29/c, neg-40/a, neg-47/b, neg-47/c, neg-54/c; sup-03, sup-05, sup-09, sup-14, sup-21, sup-26, sup-33, sup-36 to sup-38, sup-40, sup-42, sup-43, sup-48, sup-49 | Implementation notes only (index acquisition under the revised Section 13.3, reworded reasons). No decision field changed | 13.3, as above |

## The e1 cases (F1)

F1 adds decisions: the 19 observations of e1-01 to e1-15 in `decisions.json` and
`decisions-real-ids.json`, and e1-16 in `negative-e1-decisions.json`. No
existing decision changed. A comparison of every earlier entry, and of its
trace, with its predecessor finds them identical. The other decision files
are byte-identical.

## The owner's rulings of F-T1b

The text now defines a full verification (Section 2.2) and adds a tail rescue
from the sidecar's map entry (Section 13.3 step 3). Each decision below was
re-decided from the sentence it cites. No row follows an expected outcome.
A leaf-by-leaf comparison of each regenerated file with its predecessor
finds only these changes, their citations, and the Recoverer's notes on index
acquisition. The damage-case rows apply to `decisions.json` under
`blind-mapping.json`'s opaque ids and to `decisions-real-ids.json`.

| Case | Aspect | Old value | New value | Sentence |
| --- | --- | --- | --- | --- |
| parity-map-and-sidecar (case-25) | `recoverer.addresses[(1, 0)]`: `result`, `error`, `stripe` (and `bytes_match`) | error, `SidecarMetadataUnavailable` | `recovered`, stripe 0, and the rebuilt bytes match. The footer and the primary are unreadable, and the ParityMap does not validate, so no entry is available. The map entry's total 7 with P = 4 gives H = 1, and the tail copy at block 5 is valid and agrees | 13.3 step 3: "When the footer and the primary copy have both failed and no directory entry is available, a Recoverer MUST try the tail copy from the sidecar's map entry, which a validated terminal replica's structural rows give. With `total` the entry's block count and `P = S × m`, the tail copy starts at block `H + P`, where `H = (total − 1 − P) / 2`. The rescue requires that `total − 1 − P` is even and that `H` is greater than zero. The tail copy is used only if it is valid on its own (step 2's sense), its recorded `sidecar_header_block_count` equals `H`, and its epoch and protected range agree with the map entry." |
| parity-map-and-sidecar (case-25) | `verifier.outside_terminal_suffix`: the sidecar's `SidecarMetadataUnavailable` finding | present | removed. The Verifier acquires the index through the same rescue, and reads a valid tail copy | The same sentences, and 2.2: the Verifier's full check includes "the Recoverer's index and CRC validation". |
| bootstrap-wrong-scheme (22), burst-m (19), burst-m-plus-one (21), object-head (02), parity-and-data (12), parity-map-and-sidecar (25), parity-map-both (24), short-epoch-burst (23), short-epoch-recoverable (14), sidecar-footer (15), sidecar-primary (08), sidecar-primary-and-footer (03), walk-directory-rescue (11), walk-sidecar-isolation (10) | the aspect `verifier.outside_terminal_suffix (data blocks and parity shards)` | `undecided`: a full verification reports these findings, or checks only structure and metadata | decided: the findings on data blocks and parity shards, already listed by address in `verifier.outside_terminal_suffix`, are what a full verification reports. The `undecided` entry is removed | 2.2: "A full verification reads every data block and every parity shard and checks each against its sidecar's index (Section 13.4), reporting each failure by address. A check of structure and metadata alone, which reads no data block or parity shard, is not a full verification." |
| neg-33/a-H-seven, neg-33/b-H-max | `decision.error` and `decision.note` (outcome `rejected` unchanged: no read is placed from the directory entry) | `SidecarMetadataUnavailable`; epoch 0 metadata-unavailable | null: an entry that fails a precondition is not available, and no Section 15 name applies. The failed address (1, 0) is recovered through the map-entry rescue (H = 1, tail at block 5); `expect.recoverer` is `recovered` | 13.3 step 3, as the parity-map-and-sidecar row; and "These are preconditions of the rescue, not invariants of Section 10.1.5: an entry that fails them affects only its own epoch." |
| neg-33/c-total-zero | `decision.error`, `decision.error_set` and `decision.note` | `SidecarMetadataUnavailable` | the ParityMap's category, {`DirectoryInvalid`, `ParityMapParse`} ("The name is normative as a category: this document does not specify which of the two a Reader reports once it has failed both ParityMap copies."). No entry is available, and (1, 0) is recovered through the map-entry rescue | 13.3 step 3, as above; 10.1.5 "non-zero `sidecar_total_block_count`…"; 15 |
| sup-16, sup-17 | `decision.error` and `decision.note` (outcome `rejected` unchanged) | `SidecarMetadataUnavailable{0}` | null. The entry is not available, the footer and the primary are unreadable, and (1, 0) is recovered through the map-entry rescue; `expect.recoverer` is `recovered` | 13.3 step 3, as the parity-map-and-sidecar row |

Checked and unchanged:
- sup-45: the map entry's total 8 gives `total − 1 − P = 3`, which is odd, so "The rescue requires that `total − 1 − P` is even" fails and the epoch stays metadata-unavailable. Only its note changed.
- sup-11 and sup-38: the footer is valid (sup-11) or an entry is available (sup-38), so the rescue is not reached.
- The walk cases: "On the walk route the sidecar is not identified without its primary copy or footer (Section 12.3), so this rescue is not reached there."

## The narrowed rescue and the revised Section 2.2 (F-T1c)

After a review, the text narrowed the F-T1b rescue. It is now its own
paragraph at the end of Section 13.3 step 3, headed "Tail rescue from the
terminal index", and it applies only under three conditions:
- the footer and the primary copy have both failed;
- no final ParityMap validates;
- the map entry comes from a validated terminal replica.

Section 2.2 now says which blocks a full verification reads and how it
addresses a failure. Each decision below was re-decided from whether the
three conditions hold, and no row follows an expected outcome.

| Case | Aspect | Old value (F-T1b) | New value | Sentence |
| --- | --- | --- | --- | --- |
| neg-33/a-H-seven, neg-33/b-H-max | `decision.error`, `decision.note`, `expect.recoverer` | null; (1, 0) recovered through the map-entry rescue | changes back to the pre-F-T1b decision: `SidecarMetadataUnavailable{0}`. The final ParityMap validates, and only the sidecar's entry fails the precondition `total = 2H + P + 1`, so condition 2 does not hold | 13.3: "When a final ParityMap validates but the sidecar's entry fails a precondition of the directory-assisted rescue, this rescue does not apply either, since the directory then contradicts the map entry." |
| sup-16, sup-17 | `decision.error`, `decision.note`, `expect.recoverer` | null; (1, 0) recovered | changes back to the pre-F-T1b decision: `SidecarMetadataUnavailable{0}`, for the same reason | The same sentence |
| walk-sidecar-isolation (case-10) | `verifier.outside_terminal_suffix` | the tail copy of sidecar tape file 2 was not read | a finding is added: "tail copy unreadable (medium error)", `TapeIo`. The Verifier now locates the tail copy as Section 13.3 does. Here that is through the available directory entry, which the walk's validated final ParityMap gives. The Recoverer's outcome is unchanged | 9.1: "A Verifier MUST read every sidecar's primary copy, tail copy and footer"; 13.3 step 3: "locate the tail copy at block `sidecar_total_block_count − 1 − sidecar_header_block_count` using the entry's counts" |
| The 14 cases with data-block or parity-shard findings (as in the F-T1b row) | each such finding's `address`, and a parity shard's `component` | a parity shard named by its LBA; no `address` | `address` gives a data block's tape file and block, and a parity shard's epoch, stripe and parity index; a parity shard's component names all three, with its LBA. A data block that no sidecar protects is not a full-verification finding (no case has one) | 2.2: "A Verifier's validation is a full verification: it reads every data block that a sidecar protects and every parity shard, and checks each against its sidecar's index (Section 13.4). It reports each block or shard that fails by its address: a data block's tape-file position, or a parity shard's epoch, stripe and parity index." |

Checked and unchanged from F-T1b:
- **parity-map-and-sidecar (case-25), epoch 0:** stays recovered. The sidecar's footer and primary are unreadable, the ParityMap does not validate, and the map entry comes from the validated replicas. 13.3: "If the footer and the primary copy have both failed, no final ParityMap validates (so the sidecar has no directory entry), and the sidecar's map entry comes from a validated terminal replica's structural rows, a Recoverer MUST try the tail copy that the map entry locates."
- **neg-33/c-total-zero:** stays recovered through the rescue. Its footer and primary fail, its ParityMap does not validate (`DirectoryInvalid` at my level), and its replicas validate. The ParityMap's name stays the category {`DirectoryInvalid`, `ParityMapParse`}.
- **sup-45:** stays `SidecarMetadataUnavailable`. The final ParityMap validates, so the rescue does not apply; before, it failed on an odd remainder. Only its note changed.
- **The walk cases:** "A walked map does not qualify: it gains a validated scope only through a final ParityMap (Section 13.1), which this case lacks."

## The e2 cases (F2)

F2 adds decisions: e2-01 to e2-04 in `decisions.json` and
`decisions-real-ids.json`, and e2-05 to e2-07 in the two resume files. No
existing decision changed. Every earlier entry and trace is identical to its
predecessor, and the other decision files are byte-identical.
`negative-block-digests.json` changes only because the manifest now pins the
nine overflow-3.2-lba blocks, which match.

## The e2-08 to e2-13 and e3 cases, the full verification and S4 (F3)

F3 adds decisions for thirteen damage cases (e2-08 to e2-13, e3-01 to e3-07),
adds one observation, `verifier-full`, to every damage case, adds `walk.map`
to every case whose walk runs, and re-decides the four survivor sets that use
S4, whose definition changed. A leaf-by-leaf comparison of `decisions.json` and
`decisions-real-ids.json` with their predecessors finds no changed value: every
earlier leaf is identical, and the differences are the new cases and the two
new keys. `decisions-trace.json` and `decisions-real-ids-trace.json` gain a
`record_edit_checks` entry for every case, and every observation, that has
record edits (the e1 cases, e2-01 to e2-04 and the new cases), and the fields
`artifact` and `undecidable` in each walked-file record; no existing trace
value changed.
`resume-decisions.json`, `resume-decisions-real-ids.json`,
`negative-decisions.json`, `negative-e1-decisions.json`,
`negative-supplement-decisions.json`, `negative-block-digests.json` and
`mutation-decisions.json` are byte-identical to their predecessors. No row
follows an expected outcome.

| Case | Aspect | Old value | New value | Sentence |
| --- | --- | --- | --- | --- |
| Every damage case (the 57 real ids, and the same by opaque id) | new observation `verifier-full` | absent | `data_blocks_failed`, `parity_shards_failed` (each failure by address, with its reason), `other_findings`, `terminal_suffix.complete` and `coverage`. It restates what the Verifier's `outside_terminal_suffix` findings already held, with the terminal suffix's findings added, and no earlier finding changed | 2.2: "A Verifier's validation is a full verification: it reads every data block that a sidecar protects and every parity shard, and checks each against its sidecar's index (Section 13.4). It reports each block or shard that fails by its address: a data block's tape-file position, or a parity shard's epoch, stripe and parity index." 10.6: "A Verifier that finds a separation extent invalid MUST report it and MUST NOT report the terminal suffix as complete." 12.6: "A normal finalized tape has the exact terminal suffix of Section 8.3 and EOD immediately after C's trailing filemark." |
| replicas-all (case-05), walk-sidecar-isolation (case-10), walk-directory-rescue (case-11), filemark-prefix (case-20), e2-04 | new key `walk.map` | absent | `{"produced": true, "validated": true}`; filemark-prefix `{"produced": true, "validated": false}` | 8.4.1: "The walk reconstructs tape-file boundaries, validates recognisable ParitySidecar and control structure, and measures complete Object candidates by elimination." 13.1: "A walked map whose projection does not hash to a validated final ParityMap's `canonical_map_digest`, or whose prefix disagrees with those scope fields, is not validated and gives the Recoverer no map, with no fallback to the bootstrap's scope." |
| sel-08 (S4, S0, S4) | `decision.outcome`, `acceptable_selections`, `degraded` | inventory from B, degraded (S4 was another profile's replica, which supplied no layout and was never eligible) | `TerminalIndexReplicaConflict`, no selection. S4 is now a replica that is locally eligible and differs from the profile's only in its edition ID and sequence, so A and C are one edition, B another, and all three are fully valid | 8.5: "A Scanner MUST NOT accept a replica while another fully valid replica differs from it in any edition-common field: any row of kind *common* in the replica frame of Section 10.4, which together are the replicas' *edition* (Section 2.3)." "A disagreement in any edition-common field is `TerminalIndexReplicaConflict` and is never resolved by ordinal preference; a Scanner MUST NOT choose one side of a conflict merely because it is newer in the terminal suffix." 15: "TerminalIndexReplicaConflict independently valid survivors disagree" |
| sel-10 (S0, S0, S4) | the same | inventory from A and B, degraded | `TerminalIndexReplicaConflict` | The same |
| sel-11 (S4, S0, S0) | the same | inventory from B and C, degraded | `TerminalIndexReplicaConflict` | The same |
| sel-13 (S0, S4, S0) | the same | inventory from A and C, degraded | `TerminalIndexReplicaConflict` | The same |

Checked and unchanged:
- The class of S4 in `selection-decisions.json` (`statuses`) changes from
  `foreign` to `second-edition`; the other four statuses and the other ten rows
  are identical.
- `verifier_prefix_findings` now reads the parity region of an epoch whose index
  is unavailable and reports a failed read by address (GAPS M-8). No earlier
  case has both an unavailable index and an unreadable parity shard, so no
  earlier finding changed.
- Section 8.4 step 1's spacing back now crosses records to the nearest filemark
  (GAPS M-5). Every earlier tape ends in a filemark, so no earlier layout
  discovery changed.

## The e3-07 replica and the foreign fill (F3b)

The fault maps of e3-02 to e3-07 now state what F3 could not derive. Each foreign
record carries `fill`; each e3-07 record carries `role`, `base` and `fields`; e3-07
carries a file-level `replica`. F3b builds those bytes from them. The e3-01 to
e3-06 decisions are identical to F3's, as are the resume, negative, mutation and
selection files. Only e3-07 changed, and no row follows an expected outcome.

| Case | Aspect | Old value (F3) | New value | Sentence |
| --- | --- | --- | --- | --- |
| e3-07 | `scanner.result` | `undecided` | `BotStructuralRecoveryRequired`. The last record before EOD is the second-edition replica's footer; it records LBA 40, where it is read, and plans EOD 58, at or after the tape's EOD 42, so it supplies the layout (replicas at files 9, 11 and 13; extents at 10 and 12). Replica A of that layout is not locally eligible, and B, C and both extents are absent, so no replica validates | 8.4: "A terminal replica's footer that parses supplies a planned layout only when its recorded footer position equals the position at which it was read, and the layout's planned EOD is at or after the tape's EOD." 10.6: "`covered_prefix_tape_file_count`, `structural_row_count`, and replica A's planned tape-file number are equal." (4, 4, 9). 8.3: "Planned future components never prove their existence." 8.4: "if no replica validates, perform the BOT structural recovery walk in Section 8.4.1." |
| e3-07 | `scanner.replicas` | `undecided` | A: payload fails the covered-count condition; B and C: EOD where the plan puts their records | The same |
| e3-07 | `verifier.result`, `verifier.separations` | `undecided` | `recovery_required`; both extents invalid (EOD where their records should be) | 12.6: "No valid replica invokes the explicit BOT structural walk, whose result is recovery evidence rather than a fabricated terminal edition." 10.6: "A Verifier that finds a separation extent invalid MUST report it and MUST NOT report the terminal suffix as complete." |
| e3-07 | `verifier-full.terminal_suffix.complete` | `undecided` | `false`: replicas A, B and C invalid, both extents invalid, EOD at 42 not the planned EOD | 10.6 and 12.6, as above |
| e3-07 | `undecided` | scanner, verifier | none | |
| e3-07 | `walk` | classes as F3, with a note that it holds either way | the same classes; the walk is offered because no replica validates (the note is gone) | 8.4.1: "When A, B, and C are all absent or invalid, the Scanner MUST offer a full structural walk from BOT." |

## The text after T2 (F4)

Section 12.2, 12.3, 12.6, 8.2, 8.4, 2.2 and 10.6 were revised. Each change below
was re-decided from its sentence, and no row follows an expected outcome. The
resume, negative-block and build files are byte-identical to F3b's. In the negative,
supplement, mutation and selection files only the `implementation` blocks change
(reason strings, and walk-based extent statuses); no `decision`, no `self_check`
verdict and no citation changed except as the first row says.

| Case | Aspect | Old value | New value | Sentence |
| --- | --- | --- | --- | --- |
| negative-e1-decisions.json (e1-16) | the cited §8.2 sentence (`decision.rules`, `decision.order`) | "It MAY omit the scheme record (key 1) and the digest record (key 2). A Reader MUST NOT require those records on it." | "It MUST omit the scheme record (key 1) and MAY omit the digest record (key 2). A Reader MUST NOT require those records on it. A no-parity bootstrap's payload carries no parity scheme; one that does is `BootstrapParse`." The outcome stays `accepted`: e1-16 removes key 1 | 8.2, as quoted; 15 |
| (parser) | a no-parity bootstrap whose payload carries key 1 | accepted | `BootstrapParse` | 8.2: "A no-parity bootstrap's payload carries no parity scheme; one that does is `BootstrapParse`." No case in the set has one, so no decision changes |
| e3-02 | `walk.classes["9"]`; new `walk.artifacts` = ["9"] | "nonconformant artifact after the terminal suffix (not admitted as an Object)" | `Object (incomplete candidate)`; file 9 is listed in `walk.artifacts`, in no inventory. Structural damage stays reported; it is no map entry | 12.6: "A structural artifact is any tape file, complete or torn, that follows the last of the five files of the terminal suffix, whatever it contains." "The rule is one of scope: an artifact is not an Object of any inventory. The walk may report it ... as an Object candidate of unknown identity or, when torn, as an incomplete candidate." 12.2: "A file whose trailing filemark is missing before EOD is not a tape file of the map. The walk reports structural damage, classifies the file by its head ... as a torn candidate (an incomplete Object candidate when nothing else fits ...), and ends there." |
| e3-06 | `walk.classes["9"]`; new `walk.artifacts` = ["9"] | the same artifact class | `Object` (a candidate of unknown identity), in `walk.artifacts` | 12.6, as above |
| e3-05 | `walk.classes["8"]` | `Object` | `TapeIndexReplica`, damaged (2 blocks, 3 planned). Its last record is a footer whose magic matches; the head is readable and foreign and no rung of items 1, 4 or 5 parses it | 12.3: "In items 2 and 3 a footer whose magic matches establishes the type also when the head is readable, is not a header of that type, and no rung of items 1, 4 or 5 parses it. If the footer does not parse or the count disagrees, the file keeps its control type, damaged." |
| e3-01, e3-04, e3-05, filemark-prefix | `verifier.separations`, and the walk's damage findings of extents and replicas whose count is right | `invalid` for extents and replicas that were only shifted | `not_run` (no layout validates them); the findings that remain are the count mismatches (A-B in e3-01; the merged extent in e3-03 to e3-05) | 12.3: "It does not compare the planned tape-file number or start position with the file's measured tape-file number or start position; Section 10.6 checks those when a replica is validated." |
| e3-07 | `scanner.replicas.A.reason` and its `verifier-full` finding (the outcome, `BotStructuralRecoveryRequired`, is unchanged) | payload: covered count, structural_row_count and A's tape-file number differ | header: the three are not equal (4, 4 and 9), and the payload is not read | 10.6: "A replica whose header breaks the covered-count relationship is not eligible, and a Reader need not read its payload." "replica A's planned start LBA equals the end of the covered prefix" (38 against 19) |
| Every case with an unreadable sidecar copy or footer, ParityMap copy, terminal replica or (supplied values) bootstrap (about 30 cases: burst-m, e2-08 to e2-13, replicas-all, sidecar-primary, the walk cases, and the rest) | the `error` of that finding in `verifier.outside_terminal_suffix` and `verifier-full.other_findings` | `TapeIo` | none: "an unreadable component" | 2.2: "a sidecar copy or footer, a copy of the ParityMap, or a terminal replica whose records cannot be read, even when another copy or replica is used, and a bootstrap that is unreadable while supplied values are used. A medium error on such a component is reported as an unreadable component. A non-medium fault of the transport stays `TapeIo`." Data blocks, parity shards and separation extents keep `TapeIo` (GAPS N-2) |
| bootstrap-wrong-scheme | `verifier-full.coverage` | the acquired index checked 4 data blocks and 4 shards | `data_blocks_read_but_not_checkable` 4, `parity_shards_read_but_not_checkable` 4, `epochs_index_failing_the_pin` [0]; the `SchemeMismatch` finding stays | 2.2: "An index that fails the pin of Section 13.3 is likewise not used for CRC checks: the Verifier reports the pin's error and the read failures, and takes the parity shards from the acquired index." |
| Every damage case | new `verifier-full.tape_complete` | absent | false in every case (each has a finding, a failed block or shard, or an incomplete suffix); it is true only for an undamaged tape | 2.2: "It reports the tape as complete when the terminal suffix is complete, the full verification was performed, and it finds no failed data block, no failed parity shard and no finding about a sidecar or about the prefix." |
| Quotes | `full_verification_d` (Appendix D TT-2) | "... and the reference's verification is to be brought into line before freeze." | "... and the reference's verification does so." | Appendix D TT-2 |

Checked and unchanged:
- Section 8.4 step 1 now says the Scanner does not stop at the first footer that supplies a
  layout, that a separation extent's footer supplies none, and that the first backspace
  crosses trailing records (GAPS M-5). `discover_layouts` collects every layout. No case has
  two, so no scanner decision changed.
- Section 10.6's new relationship, replica A's planned start LBA equals the end of the covered
  prefix, adds a failing rule to neg-03, neg-44 and sup-15's replica A; their decisions and
  self-checks are unchanged, and sup-15's dispute count rises from 2 rules to 3.
- e3-07's Scanner, walk and Verifier results, and e3-01's Scanner and walk kinds, are as in F3b.

## The staged text of Section 12.3 (Step 6, F0)

The staged text rewrites four points of the classification ladder: how tape file 0 is
typed when the bootstrap is unreadable on supplied values, what it means for a rung to
"parse" a file, the footer route of items 2 and 3 when the head is unreadable, and how a
ParityMap is recognised (from block 0 alone). Each existing decision whose deciding
sentence changed was re-decided from the new sentence, and no row follows an expected
outcome. The resume, negative, negative-e1, negative-supplement, negative-block, mutation and
selection files are byte-identical to the previous ones. `decisions.json`,
`decisions-real-ids.json` and their traces change only as listed. No `classes`, `error`,
`outcome`, `map` or `verifier.*` value of any damage case changes: none of the 57 cases has a
walk over an unreadable bootstrap, a ParityMap that fails its payload with a parsing header, or
a file that a rung parses but does not recognise, other than the ones already decided in F4.

### The three quotes

| Quote | Old text | New text (Section 12.3) |
| --- | --- | --- |
| `replica_footer_type` | "When the head is unreadable, a matching, fully parsed terminal footer establishes the same type, after its measured count is checked." | "When the head is unreadable, the file's last block is read, and a terminal footer whose magic matches establishes the same type, whether or not the footer parses." (item 2) |
| `separation_footer_type` | "When the head is unreadable, a matching, fully parsed footer establishes the type." | "When the head is unreadable, a footer whose magic matches establishes the type, and is checked and reported as item 2 says." (item 3) |
| `ladder_parity_map` | "**ParityMap**: a complete copy/header and payload validate, and the measured block count agrees with its locator footer." | "**ParityMap**: the first block carries the ParityMap magic and its header parses, and the measured block count equals the header's `parity_map_total_block_count`." (item 4) |

New quotes, for the sentences that now decide: `parse_def`, `footer_readable_head`,
`footer_rule_scope`, `file0_by_length`, `file0_one_block`, `file0_many_blocks`,
`file0_no_footer_rule`, `item1_file0`, `pm_block0`, `pm_still_the_map`, `pm_header_unparsed`.
`tail_probe_serves` is no longer cited for a file whose head is readable (e3-05): it belongs
to the unreadable-head paragraph only.

### Decisions revised (the rule, and the effect on the 57 cases)

| Point | Old outcome (my F4 walk) | New outcome | Deciding sentence | Effect on the cases |
| --- | --- | --- | --- | --- |
| Tape file 0, bootstrap unreadable on supplied values | Read like any file: an unreadable head gave an Object candidate (a foreign or unusable head, an Object by elimination). Nothing said the walk ends for a multi-block file 0 | Typed by measured length, no bootstrap read: one block is a bootstrap (the supplied values classify it, they do not authenticate it); more than one block gives no class and the walk ends with `FilemarkMapReconstruct`; a filemark or EOD first gives no tape file 0 and the same error; the unreadable-head paragraph and the footer sentences of items 2 and 3 do not apply to file 0 | 12.3: "In that case a tape file 0 that measures exactly one block is typed a bootstrap: the supplied values classify it and do not authenticate it, and no value is taken from its bytes." "In that case a tape file 0 that measures more than one block cannot be a bootstrap (Sections 3.1 and 8.1), and the walk ends with `FilemarkMapReconstruct` (Section 15), because it cannot produce a valid map." "A filemark or EOD where the first record should be leaves no tape file 0 to type, and the walk ends with `FilemarkMapReconstruct` for the same reason." | none: bootstrap-hinted, bootstrap-wrong-scheme and the other unreadable-bootstrap cases have valid replicas, so the walk is `not_run`. Unit tests only |
| Item 1 at another tape file | A bootstrap magic at any file with a parsing frame, this tape's UUID and one block was typed a bootstrap | Not parsed by item 1; the later rungs try it (footer rule, item 6, item 7) | 12.3: "Item 1 checks that the file is tape file 0: a block that carries the bootstrap magic at any other tape file is not parsed by item 1, and the later rungs try the file." | none |
| A rung that parses, item 1 | A bootstrap magic whose frame or UUID failed was `unrecognised` at once, so the footer rule never ran for it | It does not parse, so the footer rule and the later rungs try the file. A frame that parses on this tape but a count other than 1 still gives the failed classification and item 7, and the footer rule does not apply | 12.3: "A rung parses a file when the file passes every check of that rung except its measured block count." "... and no rung of items 1, 4 or 5 parses it." | none |
| ParityMap recognition | Item 4 read and validated both copies, the payload and the footer; a failure gave an Object with a failed classification, and the walk's map held an Object entry | Item 4 reads block 0 only. A parsing header and an equal count give a ParityMap even when the payload, tail copy or footer fail; it then does not validate, so the second pass and Section 13.1 take no final ParityMap from it. A header that does not parse is not recognised and the later rungs try the file. A parsing header with another count is the failed classification, and item 7 | 12.3: "**ParityMap**: the first block carries the ParityMap magic and its header parses, and the measured block count equals the header's `parity_map_total_block_count`." "This item reads neither the payload nor the footer." "... and a ParityMap that fails them is still the ParityMap of the walk." "A block that carries the magic and whose header does not parse is not recognised by this item, and the later rungs try the file." | none: every ParityMap in the cases validates (the note in the trace changes; the citations gain the three sentences) |
| Item 5 header | The primary "parses" only when the whole copy (every index block, the payload hash) verified | The primary header parses from block 0 alone: the magic, the tape UUID, both CRCs, every constraint of the table, `copy_kind` 1 and the zero fill of block 0. The count is then the only further check | 12.3: "A primary header parses when its block satisfies Section 9.2 on its own: the magic, the tape UUID, both CRCs, every constraint of the table, `copy_kind` 1 and the zero fill." | none: a primary whose later index block is damaged and whose header parses is not in the cases |
| Footer route, head unreadable | The footer had to parse and its count agree to establish the type; anything else gave an Object candidate (or the sidecar probe) | A footer whose magic matches establishes the type whether or not it parses. If it does not parse, or its count differs from the measured count, the file is reported as a damaged terminal replica (or separation extent); otherwise it is the type, not reported as damaged | 12.3 item 2: "When the head is unreadable, the file's last block is read, and a terminal footer whose magic matches establishes the same type, whether or not the footer parses. The footer takes the header's place in the check below: a footer that does not parse, or whose record count differs from the measured count, is reported as a damaged terminal replica." Item 3: "When the head is unreadable, a footer whose magic matches establishes the type, and is checked and reported as item 2 says." | Kinds unchanged. The trace note for the three unreadable-head replicas of replicas-all, walk-sidecar-isolation, walk-directory-rescue no longer says "damaged terminal replica": their footers parse and the counts agree. No `walk.classes`, `error` or verifier value changes |
| Footer route, head readable | The same rule, cited with the unreadable-head sentences | The rule of items 2 and 3 for a readable, foreign head when no rung of items 1, 4 or 5 parses the file; the citations are now the readable-head sentences, and e3-05's `verifier-full` finding names the head ("the head is not its header") | 12.3: "In items 2 and 3 a footer whose magic matches establishes the type also when the head is readable, is not a header of that type, and no rung of items 1, 4 or 5 parses it. If the footer does not parse or the count disagrees, the file keeps its control type, damaged." | e3-05 `verifier-full.other_findings[2].finding` (a reason string) |

### Rows of the decision files that change

- `decisions.json`, `decisions-real-ids.json`: the `walk.citations` of e2-04, e2-13, e3-01 to e3-07, filemark-prefix, replicas-all,
  walk-directory-rescue and walk-sidecar-isolation (three ParityMap sentences added; the two
  footer sentences replaced with the new ones where the head is unreadable; `parse_def` added to e2-13 and filemark-prefix,
  which report a failed classification; e3-05 drops `tail_probe_serves` and gains the readable-head sentences), and the
  reason string of e3-05's `verifier-full` finding of file 8.
- The traces: the note of each ParityMap ("ParityMap by its first block: header parses, measured count agrees") and of each
  unreadable-head replica ("terminal replica established by its footer ...").

Checked and unchanged: the bootstrap path of Section 8.4 (`judge_supplied_bootstrap`); the counts of files in e3-01 to e3-07;
Section 12.6's exact-suffix test, which reads "none of them damaged" and counts a replica reported by the two conditions of item 2
as damaged (GAPS O-4).

## The staged text of Section 12.3, again (Step 6, F0b)

Item 4 now also recognises a ParityMap by a tail route, the tape-file-0 carve-out excludes that
route, and Appendix D TT-2 was reworded. Every decision file was regenerated. `resume`,
`negatives`, `negatives-supplement`, `negative-e1`, `negative-blocks`, `mutations`, `selection` and
`build-report.json` are byte-identical to F0's. In the walk decisions only `walk.citations` change
(the four re-quoted sentences and the new ones). The traces are byte-identical to F0's. No `classes`,
`error`, `outcome`, `map` or verifier value of any case changes: no damage case has a ParityMap that
the head route does not take, and none has a walk over an unreadable bootstrap.

### The four quotes

| Quote | Old text | New text |
| --- | --- | --- |
| `pm_block0` | "This item reads neither the payload nor the footer." | "This item reads no payload, and reads a footer only on that route." (12.3 item 4) |
| `pm_header_unparsed` | "A block that carries the magic and whose header does not parse is not recognised by this item, and the later rungs try the file." | "A file that neither route recognises is not recognised by this item, and the later rungs try it." (12.3 item 4) |
| `tt2_walk_pm` | "The walk route cannot find a ParityMap whose block 0 is unreadable; the replica route survives that damage by locating it through structural rows." | "Section 12.3 (item 4) finds a ParityMap whose block 0 is unreadable, or damaged so that no head rung takes it, through its footer and tail header copy, and the replica route survives that damage by locating it through structural rows." (Appendix D TT-2) |
| `file0_no_footer_rule` | "In that case, of one block or more, neither the paragraph headed \u201cUnreadable head block\u201d, below, nor the footer sentences of items 2 and 3 apply to tape file 0." | The same, "... nor the footer sentences of items 2 and 3, nor the tail route of item 4 apply to tape file 0." |

New quotes: `pm_either_kind`, `pm_either_route`, `pm_tail_route`, `pm_tail_footer`, `pm_tail_mismatch`, `pm_tail_serves`, `file0_zero_block`.

### Decisions revised

| Point | Old outcome (F0 walk) | New outcome | Deciding sentence | Effect on the cases |
| --- | --- | --- | --- | --- |
| ParityMap whose first block is unreadable, or is not taken by any head rung, away from tape file 0 | An Object candidate (the head rung failed; nothing else read the last block) | The walk reads the last block. A footer that parses (10.1.3) with the ParityMap magic locates the tail header copy at its `tail_copy_start_block`; the file is a ParityMap if that tail header parses and agrees with the footer and the footer's total equals the measured count. No payload is read | 12.3 item 4: "Away from tape file 0, when the first block is unreadable, or no rung of items 1 or 5 parses it and it is not a ParityMap header that parses, the walk reads the file's last block. If that block is a footer whose magic matches and that parses (Section 10.1.3), the footer locates the tail header copy at its `tail_copy_start_block`, and the file is a ParityMap if that tail header parses and agrees with the footer and the footer's `parity_map_total_block_count` equals the measured block count." | none |
| The same, when the footer parses and its total differs from the measured count | (not reached) | The failed classification is reported, whether or not the tail header parses; the file is not recognised, so item 7 applies | "If the footer parses and its total differs from the measured count, the failed classification is reported whether or not the tail header parses, and the file is not recognised." | none |
| Header route | Only a copy-kind-1 header parsed | Either copy kind, 1 or 2, parses | "The header may be either copy kind, 1 or 2." | none |
| A file no route recognises | (same) | Not recognised; the later rungs (item 6, the footer sentences of items 2 and 3, item 7) try it | "A file that neither route recognises is not recognised by this item, and the later rungs try it." | none |
| Where the route applies | (new) | Not at tape file 0, and never in the unreadable-bootstrap carve-out | "... neither the paragraph headed \u201cUnreadable head block\u201d, below, nor the footer sentences of items 2 and 3, nor the tail route of item 4 apply to tape file 0." | none |
| Unreadable head paragraph | The last block served the footer probe of items 2 and 3 | It also serves the tail route; a file item 4 recognises by it is a ParityMap, not an object candidate | "It also serves the tail route of item 4, and a file that item 4 recognises by that route is a ParityMap and not an object candidate." | none |
| Reading of "parse" for item 4 | The header route only | Either route may be the one that parses, so the footer rule of items 2 and 3 does not run for a file the tail route parses | "For item 4 either route of that item may be the one that parses." | none |

Checked and unchanged: the sequence of the rungs (items 2 and 3's header magics commit first, as they state; the
tail route is tried where the footer rule was, before it).

## The twelve s6 cases (Step 6, F)

`decisions.json` and `decisions-real-ids.json` (and their traces) gain s6-01 to s6-12 under their own ids
(`blind-mapping.json` maps them to themselves). Every earlier case is unchanged except for one additive key:
`walk.damaged`, new for every walk that types a control file damaged (e3-01, e3-03, e3-04, e3-05, e3-07 gain it, and
`walk.citations` gains the sentence that decides it). No earlier value changed. `resume`, `negatives`, `negatives-supplement`,
`negative-e1`, `negative-blocks`, `mutations`, `selection` and `build-report.json` are byte-identical.
Each of the twelve cases is decided from the text; no expected outcome was read. Every case is the a4-minimal image (tape
files 0 bootstrap, 1 Object of 4 blocks, 2 sidecar of 7, 3 ParityMap of 3, 4 A, 5 A-B, 6 B, 7 B-C, 8 C). In all twelve
the Scanner returns `BotStructuralRecoveryRequired` (heads of A, B and C are unreadable, or a layout is unusable) and the Verifier
`recovery_required`; the rows below are the walk.

### What the reader does with the fault maps

- With a removed filemark, an unreadable record's `tape_file`, `record_index` and `lba` are stated on the modified tape (every
  later record moved down by one, two files joined). The reader computes those three values from the modified stream and fails
  the case if any differs. Without a removed filemark they still refer to the image. (s6-03: LBA 18 is A's head, tape file 3; s6-05:
  LBA 18 is A's head at record 3 of the joined tape file 3.) Record edits stay stated on the image, checked against every stated old
  byte and SHA-256, as before; no s6 case has both a removal and an edit (GAPS P-1).
- `hints` on a walk case are the supplied values, as for bootstrap-hinted. An unknown key still fails.
- The walk ends at tape file 0 when the carve-out finds it cannot be a bootstrap (below).

| Case | Fault | Walk decision | Deciding sentences |
| --- | --- | --- | --- |
| s6-01 | Supplied values; file 0 unreadable (medium error); the three replica heads unreadable | `0` Bootstrap, `1` Object, `2` ParitySidecar, `3` ParityMap, `4` TapeIndexReplica, `5` IndexSeparationExtent, `6` TapeIndexReplica, `7` IndexSeparationExtent, `8` TapeIndexReplica; map produced and validated against the final ParityMap; no failed classification; identity unknown | 8.4: "a medium error, or a filemark or EOD where the record should be \| treats the bootstrap as unreadable and continues on the supplied values". 12.3: "In that case a tape file 0 that measures exactly one block is typed a bootstrap: the supplied values classify it and do not authenticate it, and no value is taken from its bytes." Item 2: "When the head is unreadable, the file's last block is read, and a terminal footer whose magic matches establishes the same type, whether or not the footer parses." Their footers parse and the counts agree, so none is damaged |
| s6-02 | Supplied values; file 0's first byte 0x52 becomes 0x53 (a bootstrap magic that no longer matches); replica heads unreadable | The same classes and map as s6-01 | 8.4: "the magic is missing, or the header CRC fails \| treats the bootstrap as unreadable". Then 12.3 as s6-01: the record is unreadable in that sense, so file 0 is typed by measured length, one block, a bootstrap |
| s6-03 | Supplied values; file 0's filemark removed, so tape file 0 is 5 blocks (the bootstrap and the Object) with an unreadable first record; heads of A, B, C unreadable (LBAs 18, 26, 34 of the modified tape) | The walk ends at tape file 0 with `FilemarkMapReconstruct`: `0` has no type ("classification failed"), the failed classification is "tape file 0 measures 5 blocks and cannot be a bootstrap (Sections 3.1 and 8.1)"; no later file is typed; no map | 12.3: "In that case a tape file 0 that measures more than one block cannot be a bootstrap (Sections 3.1 and 8.1), and the walk ends with `FilemarkMapReconstruct` (Section 15), because it cannot produce a valid map." "In that case, of one block or more, neither the paragraph headed 'Unreadable head block', below, nor the footer sentences of items 2 and 3, nor the tail route of item 4 apply to tape file 0." I read "ends with" as ends: no later tape file is classified (GAPS P-2) |
| s6-04 | No supplied values (bootstrap readable); the three replica heads unreadable; a one-block file 9 appended, a copy of the bootstrap block | `0`..`8` as s6-01 (`0` from the bootstrap), `9` Object (a candidate of unknown identity), listed in `walk.artifacts`; map through file 8, produced and validated | 12.3: "Item 1 checks that the file is tape file 0: a block that carries the bootstrap magic at any other tape file is not parsed by item 1, and the later rungs try the file." Nothing later parses it, so item 7: "Object, by elimination". 12.6: "A structural artifact is any tape file, complete or torn, that follows the last of the five files of the terminal suffix, whatever it contains. The suffix is exact when the walk recognises, in order, replica, separation extent, replica, separation extent and replica, none of them damaged." The replicas of files 4, 6 and 8 are typed by their parsing footers and are not damaged (GAPS O-4) |
| s6-05 | The filemark after the ParityMap removed: tape file 3 is the ParityMap and replica A joined (6 blocks); A's head (LBA 18, record 3), B's and C's heads unreadable | `0` Bootstrap, `1` Object, `2` ParitySidecar, `3` Object (failed classification: "ParityMap header total 3 differs from the measured 6 blocks (Section 12.3 item 4)"), `4` IndexSeparationExtent, `5` TapeIndexReplica, `6` IndexSeparationExtent, `7` TapeIndexReplica; map produced, not validated (no ParityMap is recognised) | 12.3 item 4: "the first block carries the ParityMap magic and its header parses, and the measured block count equals the header's `parity_map_total_block_count`." The header parses, so the tail route (which needs a header that does not parse) does not apply. "A rung that parses a file but fails its measured count reports the failed classification." "…under items 4, 5 and 6, as under item 1, the file is not recognised, and item 7 applies." 13.1: a walked map validates only against a validated final ParityMap |
| s6-06 | Replica A's head unreadable and its footer edited (a CRC-covered byte) | Classes as s6-01; file 4 is a damaged TapeIndexReplica (`walk.damaged["4"]`: the footer does not parse); map produced and validated | Item 2: "…a terminal footer whose magic matches establishes the same type, whether or not the footer parses. … a footer that does not parse, or whose record count differs from the measured count, is reported as a damaged terminal replica." The ParityMap is intact, so the map validates |
| s6-07 | The A-B extent's head unreadable and its footer edited | Classes as s6-01 with file 5 a damaged IndexSeparationExtent (`walk.damaged["5"]`) | Item 3: "When the head is unreadable, a footer whose magic matches establishes the type, and is checked and reported as item 2 says." The extent stays in the map by its count, which is right |
| s6-08 | Both ParityMap copies' first payload byte edited | `3` ParityMap; map produced, not validated | Item 4: header parses, count agrees, so it is recognised from block 0; "This item reads no payload, and reads a footer only on that route." "…a ParityMap that fails them is still the ParityMap of the walk." 10.1.4: the payload must hash to `payload_sha256`; both copies fail, so the ParityMap does not validate and the second pass and 13.1 have no final ParityMap |
| s6-09 | The ParityMap footer's CRC-covered byte edited | `3` ParityMap; map produced, not validated | Item 4 as s6-08: the head route reads no footer. 10.1.3: "A footer that reads and is invalid rejects the ParityMap (`ParityMapParse`)." |
| s6-10 | The ParityMap primary header's CRC broken | `3` ParityMap (by the tail route); map produced and validated | Item 4: "Away from tape file 0, when the first block is unreadable, or no rung of items 1 or 5 parses it and it is not a ParityMap header that parses, the walk reads the file's last block. If that block is a footer whose magic matches and that parses (Section 10.1.3), the footer locates the tail header copy at its `tail_copy_start_block`, and the file is a ParityMap if that tail header parses and agrees with the footer and the footer's `parity_map_total_block_count` equals the measured block count." 10.1.3: "When one header copy is invalid and the other is valid, the valid copy is used", so it validates |
| s6-11 | The primary header's CRC broken and the footer edited | `3` Object (no failed classification); map produced, not validated | Item 4: the footer does not parse, so the tail route recognises nothing; "A file that neither route recognises is not recognised by this item, and the later rungs try it." Nothing later does, so item 7. The count rule applies only when "the footer parses and its total differs" |
| s6-12 | The ParityMap magic's last bit flipped in block 0 | `3` ParityMap (by the tail route); map produced and validated | Item 4's tail route: the first block matches no rung of items 1 or 5 and is not a header, so the last block is read, as s6-10 |

New key: `walk.damaged` (tape file to reason) for a file typed a control type and reported damaged. 12.3 item 2: "A file that fails
the check is reported as damage, not raised as an error, and is not an Object."


## The two ParityMap head cases (Step 6, F-s6b)

`decisions.json` and `decisions-real-ids.json` (and their traces) gain s6-13 and s6-14 under their own ids (`blind-mapping.json`
maps them to themselves). Every earlier case is byte-identical (each case's value compared leaf for leaf with the committed
files; only the two new keys appear). `resume`, `negatives`, `negatives-supplement`, `negative-e1`, `negative-blocks`,
`mutations`, `selection` and `build-report.json` are untouched. Both cases are decided from the text, and no expected outcome was
read. Both are the a4-minimal image with the heads of A, B and C unreadable; in both the Scanner returns
`BotStructuralRecoveryRequired` and the Verifier `recovery_required`. Both damage the ParityMap (tape file 3, 3 blocks).

| Case | Fault | Walk decision | Deciding sentences |
| --- | --- | --- | --- |
| s6-13 | The ParityMap's head block (LBA 15) unreadable; nothing else in the file changed | `0` Bootstrap, `1` Object, `2` ParitySidecar, `3` ParityMap (by the tail route), `4` TapeIndexReplica, `5` IndexSeparationExtent, `6` TapeIndexReplica, `7` IndexSeparationExtent, `8` TapeIndexReplica; map produced and validated; the Verifier lists file 3's primary copy as a medium error, with no Section 15 name (Section 2.2) | 12.3 item 4: "Away from tape file 0, when the first block is unreadable, or no rung of items 1 or 5 parses it and it is not a ParityMap header that parses, the walk reads the file's last block. If that block is a footer whose magic matches and that parses (Section 10.1.3), the footer locates the tail header copy at its `tail_copy_start_block`, and the file is a ParityMap if that tail header parses and agrees with the footer and the footer's `parity_map_total_block_count` equals the measured block count." The footer and tail copy are intact and the count is 3, so the file is a ParityMap. "Unreadable head block": "a file that item 4 recognises by that route is a ParityMap and not an object candidate." Validation: 10.1.3 "When one header copy is invalid and the other is valid, the valid copy is used", and item 4's "This is the copy fallback of Section 10.1.3, applied to the walk"; the payload is untouched, so 13.1 validates the map (GAPS P-5 for the reading of "invalid") |
| s6-14 | The ParityMap's head block unreadable, and its footer's CRC-covered byte edited (CRC left stale) | As s6-11: `3` Object, no failed classification; the other files as s6-13; map produced, not validated | 12.3 item 4: the footer is read on the tail route, and the route needs a footer "whose magic matches and that parses"; this footer does not parse, so the tail route recognises nothing. "A file that neither route recognises is not recognised by this item, and the later rungs try it." The count rule does not apply: "If the footer parses and its total differs from the measured count, the failed classification is reported". Items 2 and 3 do not take it: the footer's magic is the ParityMap's, not a replica's or an extent's. Nothing later parses it, so "Object, by elimination". No ParityMap is recognised, so 13.1's "when that ParityMap validates" is not met and the map is not validated (GAPS P-6 for the Verifier's silence about file 3) |
