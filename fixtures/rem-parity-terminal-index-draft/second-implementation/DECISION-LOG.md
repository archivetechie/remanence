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
