# Build log

This log records every comparison of a built artifact with a candidate file
or a pinned digest that failed, the section consulted, and the resolution.
No change to the builder may follow a failed comparison without a row here.

## Runs

| Run | Scope | Comparisons | Failed |
| --- | --- | ---: | ---: |
| 1 | the six images, six terminal profiles, maximums and Object-row extensions | 132 | 0 |
| 2 | the million-row streaming vector | 14 | 0 |
| 3 | everything, after the Writer-side input checks were added (no byte-producing code changed) | 146 | 0 |
| 4 | everything, after the copied definitions were replaced by imports; `build-report.json` and the blind `decisions.json` were byte-identical to run 3's | 146 | 0 |
| 5 | the two accepted resumes (resume-02, resume-05), compared file by file with the same Objects written in one uninterrupted session | 11 tape files | 0 |
| 6 | everything, after the internal `session-end` stop was added for run 5; `build-report.json` and the blind `decisions.json` were byte-identical | 146 | 0 |
| 7 | everything, after the checked-arithmetic changes below; `build-report.json`, the blind `decisions.json` and `resume-decisions.json` were byte-identical | 146 | 0 |
| 8 | the negative cases: 142 checks of a mutation's `from` value against my bytes, and the implementation's self-check against each text decision | 142 + 81 | 0 |
| 9 | the supplemental variants: `from` checks, isolation audits and self-checks; then `build`, `decide` (blind and real ids), `resume` and `negatives` rerun, all byte-identical | 49 variants | 1 (the quote below) |
| 10 | the negatives' mutated blocks: every block that the 84 negative entries and the 49 supplement variants change, against the 387 rows of `tape-images/negatives/MANIFEST.tsv` (`negative-block-digests.json`) | 400 keys | 19 mismatched; 41 missing from one side (below) |
| 11 | the 50 terminal-index mutations: stated old values, recomputed checksums and hashes, and retained values against my bytes; then the Scanner's self-check against each decision | 1,623 + 50 | 6 rows unresolved on the first run (applier defects, below); 0 after |
| 12 | the 14 survivor sets: the same checks for the two byte-change statuses, then the self-checks | 165 + 14 | 0 |
| 13 | everything, after the three new commands were added: `build` reproduced 146 with a byte-identical `build-report.json`; `negatives` and `negatives-supplement` on `blind-inputs/` byte-identical to the committed files; `decide` and `resume` on the tests' synthetic cases byte-identical to the tool as it was before this change | 146 + 4 files | 0 |
| 14 | after the resume case-id fix and the Section 14 quote fix: `resume` on `tape-images/resume/*/inputs.json` and on the same inputs under their opaque ids, and `negatives`, each equal to its committed file with the one quote replaced; then `build`, `decide`, `negatives-supplement` and the three new commands reproduced their committed files byte for byte | 146 + 9 files | 0 |
| 15 | everything, after the changes for the revised text (F0, below): `build` reproduced 146 with a byte-identical `build-report.json`; `negative-blocks` byte-identical; `decide`, `resume`, `negatives`, `negatives-supplement`, `mutations` and `selection` regenerated, each compared leaf by leaf with its predecessor (every change is in DECISION-LOG.md), and reproduced byte for byte on a second run; the blind `decisions.json` and `resume-decisions.json` equal the real-id files under the mappings (25 and 8 cases); every self-check agrees | 146 + 11 files | 0 |
| 16 | the e1 damage cases and the E1 negative (F1): the 15 cases' record edits against my build of a4-minimal (every stated old byte, every edited record's SHA-256, and my own CRC for the nine edits that say they recompute one); e1-16's mutated bootstrap against `tape-images/negatives/MANIFEST.tsv`; then every other decision file regenerated, byte-identical (`resume`, `negatives`, `negatives-supplement`, `mutations`, `selection`), or with every earlier entry unchanged and the new entries added (`decide`, blind and real ids, and their traces; `negative-block-digests.json`) | 15 records, 23 byte edits, 9 CRCs, 1 block | 0 |
| 17 | everything, after the changes for the owner's rulings (F-T1b, below): `build` reproduced 146 with a byte-identical `build-report.json`; `negative-blocks`, `negative-e1`, `resume`, `mutations` and `selection` byte-identical; `decide` (blind and real ids), `negatives` and `negatives-supplement` regenerated, and each compared leaf by leaf with its predecessor (every change is in DECISION-LOG.md); every self-check agrees; the blind `decisions.json` equals the real-id file under the mapping | 146 + 12 files | 0 |
| 18 | everything, after the narrowed rescue and the revised Section 2.2 (F-T1c, below): `build` reproduced 146 with a byte-identical `build-report.json`; `negative-blocks`, `negative-e1`, `resume`, `mutations` and `selection` byte-identical; `decide` (blind and real ids), `negatives` and `negatives-supplement` regenerated, and each compared leaf by leaf with its predecessor (every change is in DECISION-LOG.md); every self-check agrees; the blind `decisions.json` equals the real-id file under the mapping | 146 + 12 files | 0 |
| 19 | the e2 cases and the R2 manifest (F2): the record edits of e2-01 to e2-05 against my builds (every stated length and the SHA-256 of each result); the nine overflow-3.2-lba blocks against `tape-images/negatives/MANIFEST.tsv`; then every decision file regenerated, byte-identical (`negatives`, `negative-e1`, `negatives-supplement`, `mutations`, `selection`), or with every earlier entry unchanged and the new entries added (`decide` and `resume`, blind and real ids, and the traces; `negative-block-digests.json`) | 5 records, 9 blocks | 0 |
| 20 | the e2-08 to e2-13 and e3-01 to e3-07 damage cases, the full verification and S4 (F3): the record edits of e2-08 to e2-13 against my builds (10 records, 28 byte edits: every stated old byte and the SHA-256 of each result), the 24 edits that say they recompute a hash or CRC against mine (all 24 agree), the inserted record of e3-01, and the 10 derivable appended records of e3-02 to e3-06 (the 3 `second_edition_replica` records of e3-07 cannot be checked); then every decision file regenerated: `build`, `resume` (blind and real ids), `negatives`, `negative-e1`, `negatives-supplement`, `negative-blocks` and `mutations` byte-identical; `decide` (blind and real ids, and the traces) with every earlier leaf unchanged and the new cases, `verifier-full` and `walk.map` added; `selection` re-decided for sel-08, sel-10, sel-11 and sel-13 only; the blind `decisions.json` equals the real-id file under the mapping (57 cases); every self-check agrees | 10 records, 28 edits, 24 recomputations, 11 record digests | 1 (the foreign record's digest, below) |
| 21 | the fixtures' answers to GAPS M-3 and M-4 (F3b): the foreign `fill` (5 files, 9 records), and e3-07's three second-edition records built from `base` and `fields`, each SHA-256 checked (all 3 agree, and the 5 e3 files' other records are unchanged), the `replica` key's layout tuples, edition, planned position and EOD against the built header and footer (agree), my frame CRCs and the footer's header hash (agree); then every decision file regenerated: only `decisions.json`, `decisions-real-ids.json` and their traces change, and only in e3-07 | 12 records | 0 |

## Failed comparisons

| Artifact | First differing byte and field | Section consulted | Resolution |
| --- | --- | --- | --- |
| e2-08 and e2-12, record edits of tape file 4 (F3, first run) | Not a comparison of bytes: the fault reader refused the construction "the ParityMap re-encoded with the edited directory entry" as one it does not know | None; the file states each byte edit, each old byte and the result's SHA-256 | Fixed: the construction is read as "the record keeps its length, and the listed edits are the whole change", checked by the old bytes, the SHA-256 and, for the edits that say they recompute, my own CRC and payload SHA-256 (all agree). GAPS M-1. |
| e3-02 to e3-06, the `foreign` appended record (SHA-256 2c6afa39...) | My first reading, every byte the stated first byte (0x58), gives ce025caf... A further reading, the first byte then 0xFF, and a ramp from 1, also differ. The first byte then zeros reproduces the stated digest | None: the text does not say what a foreign Object's bytes are | The file states only `first_byte`, `length` and the digest. Zero fill is taken as the reading, checked by the digest but not derived. Four fills were tried, and the one that matches was kept. No decision depends on the bytes after the first (the ladder never reads Object content, and 0x58 begins none of the tape's role magics). GAPS M-3. |
| The quote `terminal_u64`, checked by the test that every quoted sentence occurs in the text | The quote read "checked in u64"; the text has "checked in `u64`" | 8.3 | A fix that reads the text correctly: the sentence is "Arithmetic on these fields is checked in `u64` (Section 2.4)." Only `negative-supplement-decisions.json` cites this quote, and it was regenerated. |
| The quote `resume_step1`, checked by the test that every quoted sentence occurs in the text (found in run 13; it failed on the tool as it was before this change too) | The quote read "dropping any torn tail"; the text now reads "dropping the tape's torn tail, if any" | 14 | A fix that reads the current text: the sentence is "Derive the committed prefix from the off-tape commit records (Section 3.4), dropping the tape's torn tail, if any, and compute `W` and `T` from it." The text was reworded after the resume and negative decisions were written (Appendix C records the wording follow-up). In run 14, `resume-decisions.json`, `resume-decisions-real-ids.json` and `negative-decisions.json` were regenerated; each differs from its predecessor only in this string (9, 9 and 1 occurrences). DECISION-LOG.md has the row. |
| Run 11, mut-06, mut-14, mut-23, mut-25 and mut-39: the retained-value checks of 32-byte digest fields | My applier compared 8 bytes, because it took a default width instead of the width of the stated value (for example "0x94 repeated 32 times") | None; an applier defect, not a reading of the text | Fixed: the width now comes from the stated value. The rows had not yet been decided. |
| Run 11, mut-15: the footer header-record SHA-256 and the footer CRC | My applier treated the edited header-record hash as a repair output, because the row recomputes a checksum, and so never applied the edit | The row's repairs | A fix that reads the row correctly: its only recomputed entry is "Footer CRC-64/XZ at 0x3F8, over footer bytes [0x000, 0x3F8).", so the header-record hash is the mutation itself. An edit is now a repair output only when its own checksum or hash is listed as recomputed. |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 6 block 0 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 6 block 1 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 6 block 2 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 7 block 0 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 7 block 2 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 8 block 0 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 8 block 1 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 8 block 2 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 9 block 0 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 9 block 2 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 10 block 0 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 10 block 1 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| overflow-7.2-T/isolated (sup-15), terminal profile multi-256k, tape file 10 block 2 | Not locatable: the manifest pins only the size and SHA-256, and no candidate reproduces it | 10.2–10.5; the variant | No fix. The variant leaves these bytes to the generator, and no rule of the text could fix them (GAPS H-2). |
| sidecar-real-data-shard-count/isolated-2 (sup-14), tape-image a4-minimal, tape file 2 block 0 | byte 152 (0x98): sidecar header copy: canonical_metadata_hash (frame offset 0x98, byte 0); located against the diagnostic candidate | 6.2, 9.3, 9.5, 10.1.3 | No fix. The text does not determine the parity of an epoch longer than `S × k`, and the variant says the parity bytes are arbitrary (GAPS H-1). |
| sidecar-real-data-shard-count/isolated-2 (sup-14), tape-image a4-minimal, tape file 2 block 5 | byte 152 (0x98): sidecar header copy: canonical_metadata_hash (frame offset 0x98, byte 0); located against the diagnostic candidate | 6.2, 9.3, 9.5, 10.1.3 | No fix. The text does not determine the parity of an epoch longer than `S × k`, and the variant says the parity bytes are arbitrary (GAPS H-1). |
| sidecar-real-data-shard-count/isolated-2 (sup-14), tape-image a4-minimal, tape file 2 block 6 | byte 96 (0x60): sidecar footer: canonical_metadata_hash (frame offset 0x60, byte 0); located against the diagnostic candidate | 6.2, 9.3, 9.5, 10.1.3 | No fix. The text does not determine the parity of an epoch longer than `S × k`, and the variant says the parity bytes are arbitrary (GAPS H-1). |
| sidecar-real-data-shard-count/isolated-2 (sup-14), tape-image a4-minimal, tape file 3 block 0 | byte 56 (0x38): ParityMap: payload_sha256 (frame offset 0x38, byte 0); located against the diagnostic candidate | 6.2, 9.3, 9.5, 10.1.3 | No fix. The text does not determine the parity of an epoch longer than `S × k`, and the variant says the parity bytes are arbitrary (GAPS H-1). |
| sidecar-real-data-shard-count/isolated-2 (sup-14), tape-image a4-minimal, tape file 3 block 1 | byte 56 (0x38): ParityMap: payload_sha256 (frame offset 0x38, byte 0); located against the diagnostic candidate | 6.2, 9.3, 9.5, 10.1.3 | No fix. The text does not determine the parity of an epoch longer than `S × k`, and the variant says the parity bytes are arbitrary (GAPS H-1). |
| sidecar-real-data-shard-count/isolated-2 (sup-14), tape-image a4-minimal, tape file 3 block 2 | byte 56 (0x38): ParityMap: payload_sha256 (frame offset 0x38, byte 0); located against the diagnostic candidate | 6.2, 9.3, 9.5, 10.1.3 | No fix. The text does not determine the parity of an epoch longer than `S × k`, and the variant says the parity bytes are arbitrary (GAPS H-1). |
| overflow-3.2-lba (neg-44): 9 blocks of replicas A, B and C in `multi-256k`, on my side only | Missing from the manifest, which pins no block for this case | 3.2 | Nothing to compare. |
| sidecar-real-data-shard-count/isolated-2 (sup-14): tape file 2 blocks 1–4 (the parity region), on my side only | Missing from the manifest: my parity differs from the base image's, and the pinned parity equals it | 6.2 | No fix (GAPS H-1). |
| 28 manifest rows with no block on my side: the footer and ParityMap of sup-09, sup-26 and sup-40, the ParityMap of sup-06, and unchanged blocks of the rebuilt images of sup-11, sup-14, sup-37 and sup-45 | Missing from my side: my mutation leaves each of these blocks byte-identical to the base, so it is not emitted. My block at each key has the pinned size and SHA-256. | None | No fix; the bytes agree. The manifest also lists blocks a repair or rebuild rewrote unchanged (GAPS H-3). |

No comparison of a built artifact with a candidate or a pinned digest
failed before run 10. The builder was written from the text before any
comparison was run, and the first run reproduced every artifact. The first
two rows above are failed self-checks: quoted sentences that did not match
the text; the second was fixed in run 14. Run 10 is the first comparison of
mutated blocks. Its 19
mismatches are all in two supplement variants whose bytes the text does not
determine (GAPS H-1 and H-2); no mismatch led to a change of any emitted
byte, and none was tuned to a digest. The two run-11 rows are defects in the
new applier, found by its own checks against my bytes before any decision
was written.

The choices the builder makes where the text is silent are recorded in
`GAPS.md` section A, with what the candidate uses. They were made from the
text and the recorded inputs before the first comparison, not tuned to it.

## Changes after the first run

These changes followed passing comparisons, not failed ones, and changed no
emitted byte:

- Writer-side checks were added to the terminal-profile path. They refuse
  inputs a conformant Writer could not produce: non-dense rows, a broken
  ordinal chain, an Object row that disagrees with its structural row, and
  scope values that disagree with the rows. Each refusal names the input
  field.
- A describer was factored out for the tests. It names the field at a byte
  offset of a terminal component.

## Changes for the negatives (D6)

These changes followed reading the text for the negative cases, not a failed
comparison, and changed no byte of any earlier output:

- Checked arithmetic (Section 2.4) was added where the Reader combines
  values read from tape. It covers:
  - the planned-layout formulas;
  - the replica `payload_len` and padding formulas;
  - the separation `actual_bytes` formula;
  - the sidecar footer's `H + P`;
  - the directory-assisted tail position;
  - the Resumer's W, T and append point.
  Before this, Python's unbounded integers would have hidden an overflow,
  and a negative tail position would have indexed from the end of the
  record list.
- The sidecar footer parser no longer requires `total = 2H + P + 1`, which
  Section 9.6 does not state (GAPS F-7). It now checks the Section 9.6
  rules: `primary_header_start_block = 0` and `tail_header_start_block = H + P`.

## Changes for TT-2: mutated blocks, terminal mutations and survivor sets

These changes add code and change no function that `build`, `decide`,
`resume`, `negatives` or `negatives-supplement` calls; run 13 confirms that
their outputs are byte-identical.

- `negative-blocks` re-runs the apply step of every negative entry and
  supplement variant and compares the changed blocks with the manifest.
  For sup-14 only, it builds a diagnostic candidate with the base image's
  parity, which the variant permits. It uses the candidate to locate
  differences only when the candidate reproduces the pinned digest, and
  never emits the candidate's bytes. The candidate reproduced all 26 pinned
  digests of the case.
- `mutations` and `selection` apply byte-level descriptions to my own
  profile builds, recompute every listed checksum and hash from my bytes,
  and check every stated value. Their Scanner is the existing replica,
  separation and layout checks, run on a record stream in which a component
  whose length is not a block multiple ends in a short record.

## Changes for CI (run 14)

- `case_id_of` again keys a resume case by its directory's name when the file
  is named `inputs.json`, as the repository lays resume cases out. Since the
  negatives were added, every such case had been keyed by the file's stem,
  `inputs`, so the eight cases collapsed into one key. No decision depends on
  the key.
- The Section 14 step 1 quote now matches the current text (the row above).

## Changes for the revised text (F0)

These changes follow the revised text, not a failed comparison. No
byte-producing code changed. `build-report.json` and
`negative-block-digests.json` are byte-identical. Every decision file was
regenerated, and each change to a decision has a row in `DECISION-LOG.md`.

- Quotes. The four stale quotes (`hint_discovery`, `replica_footer_type`,
  `separation_footer_type`, `sidecar_copy_agree`) were replaced with the
  revised sentences. The quotes the revised sections add were added too.
  `test_decision_quotes_occur_in_the_text` checks every quote against the
  text.
- Section 8.4 step 1: `discover_layout` spaces back over up to five
  filemarks from EOD. A footer supplies the layout only at its recorded
  position and when its planned EOD is at or after the tape's EOD. When no
  footer supplies one, no replica validates.
- Section 8.4's tables for the first record with supplied values
  (`judge_supplied_bootstrap`), and Section 15's bootstrap names by level.
  A parser reports `BootstrapParse`; discovery without supplied values
  reports `NoBootstrapFound`; a refusal with supplied values carries the
  table's name. The negatives' bootstrap entries run a second role,
  `bootstrap-discovery`.
- Section 12.3: rungs 1, 4, 5 and 6 recognise a file only when every check
  passes. Otherwise the failed classification is reported and item 7 makes
  the file an Object candidate. A matching, fully parsed footer establishes
  a terminal type when the head is unreadable.
- Section 13.3: index acquisition follows the revised steps.
  - A valid footer decides unless an available entry disagrees.
  - An available entry decides by hash.
  - Without an entry, the tail copy is read at `H + P`, and a differing
    hash leaves the epoch unavailable.
  - An entry is available only with `total = 2H + P + 1` and `H > 0`.
  - An unavailable epoch is always reported as `SidecarMetadataUnavailable`.
- Section 2.4: formulas denote exact values. A value that is only compared
  is compared exactly. The rejections of an intermediate overflow in the
  replica padding formula and in the ParityMap's `0xC8 + L` were removed.
- Section 7.2: every position a replica map describes must fit in u64
  (`validate_structural_rows`).
- Section 10.6: `W = T` is evaluated on the recorded fields. This removes
  the G-7 divergence.
- Sections 2.2 and 9.1: the Verifier reads every sidecar's footer, primary
  copy and tail copy. It reports a divergence as `SidecarParse`, and reports
  each finding before the terminal suffix with the error a Reader reports.
  Findings on data blocks and parity shards stay undecided (Appendix D).
  With no planned layout, a separation extent that the walk finds damaged is
  reported invalid.
- Section 12.6: an inventory's degraded flag follows the Scanner's reads
  (`per reads`).
- Section 14: step 2 refuses a prefix whose finalization has begun. In step
  3, a boundary or wrong-length read is `ResumeAppend` and a medium error is
  `TapeIo`.
- Terminal commands:
  - With no layout, the Scanner walks from BOT and reports the walk's
    classes, and the separation status comes from the walk.
  - A survivor set's layout comes from the first of C, B and A whose footer
    supplies one.
  - A new mutation kind covers an extent that gains a record (mut-38).

## Changes for the e1 cases (F1)

These changes add inputs and a command. They change no decision in any earlier
file, and run 16 confirms that.

- The fault reader (`load_fault_map`) knows its keys at every level and
  refuses any other, and any missing required key, before a case is decided.
  `decide` exits with status 2 and names the file and the key. Before F1 an
  unknown key was ignored without a word.
- `record_edits` replace a record before the other faults are applied. Each
  replacement is checked in four ways, and a failed check fails the run:
  - the stated LBA is the record's LBA in my build;
  - the stated original length is my record's length;
  - the construction agrees with the two lengths;
  - every stated old byte matches my byte, and the SHA-256 of the result
    matches the stated one.
  An edit that says it recomputes a CRC is also compared with my CRC. The
  result is recorded, not enforced.
- `observations` are decided one by one on the same damaged tape. The case's
  entry holds the edit checks and a decision per observation.
- Section 8.4's content table reads a decodable value without the Section
  5.3 canonical-form rules (`decode_cbor_lenient`; GAPS J-1). Before F1 the
  strict decoder was used, so a payload with its keys out of order could
  never yield a disagreeing value. No earlier decision reached that row with
  such a payload.
- A refusal with supplied values records the value it names
  (`discovery.refusal_names`), and cites the refusal's sentences instead of
  `NoBootstrapFound`'s.
- The Verifier reports a bootstrap the Scanner treated as unreadable with its
  cause: `TapeIo` for a medium error, `BootstrapParse` for damaged content
  (GAPS J-2). Before F1 every such bootstrap was a medium error.
- `negative-e1` decides the E1 negative from its own table (e1-16) and writes
  `negative-e1-decisions.json`. `negative-blocks` now includes that case, by
  default from `blind-inputs/negatives-e1-blind.json`.
- `blind-mapping.json` maps the e1 ids to themselves.

## Changes for the owner's rulings (F-T1b)

These changes follow two rulings the text now states. No byte-producing code
changed.

- Quotes. The Appendix D sentence "Whether a full verification checks every
  data block and parity shard, or only structure and metadata, is to be
  decided…" no longer occurs. It is replaced by Section 2.2's two sentences
  (`full_verification`, `structure_not_full`) and the revised Appendix D
  sentence (`full_verification_d`). The unused quote `tail_route_open`,
  whose sentence also no longer occurs, is removed. Section 13.3's new
  sentences are added (`map_rescue`, `map_rescue_position`,
  `map_rescue_requires`, `map_rescue_valid`, `map_rescue_fails`,
  `map_rescue_not_walk`).
- Section 2.2: the Verifier's findings on data blocks and parity shards are
  decided, so the `undecided` entry for them is gone. The findings themselves
  were already computed and are unchanged.
- Section 13.3 step 3 (`acquire_index`). When the footer and the primary have
  both failed and no directory entry is available, the Recoverer tries the
  tail copy at `H + P`, with `H = (total − 1 − P) / 2` from the map entry. It
  requires `total − 1 − P` to be even and `H > 0`, and uses the copy only if
  the copy is valid on its own, records `H`, and agrees with the map entry in
  epoch and range. It is not used on the walk route. The Verifier acquires
  the index the same way, with the walk route passed through, and locates
  the tail copy from the map entry when neither the footer nor the primary
  gives `H`.
- The negatives table re-decides neg-33 (a, b and c), and the supplement
  table sup-16 and sup-17 (DECISION-LOG).

## Changes for the narrowed rescue and the revised Section 2.2 (F-T1c)

- Quotes. The F-T1b rescue sentences and the first §2.2 sentence no longer
  occur. They are replaced by the paragraph "Tail rescue from the terminal
  index" (`map_rescue`, `map_rescue_position`, `map_rescue_requires`,
  `map_rescue_valid`, `map_rescue_fails`, `map_rescue_not_walk`,
  `map_rescue_not_directory`) and by §2.2's new sentences
  (`full_verification`, `report_by_address`, `opaque_bytes`).
- `acquire_index` applies the rescue only when no final ParityMap validates,
  and never on the walk route. The first condition is new: before, an entry
  that failed a precondition of a validated directory also let the rescue
  run.
- The Verifier addresses each data finding (`address`). A data block is
  named by its tape file and block, and a parity shard by its epoch, stripe
  and parity index. It skips data blocks that no sidecar protects. It
  locates each sidecar's tail copy as Section 13.3 does, through an available
  directory entry or, under the rescue's conditions, through the map entry.
- The negatives table re-decides neg-33/a and neg-33/b, and the supplement
  table sup-16 and sup-17, back to `SidecarMetadataUnavailable`.

## Changes for the e2 cases (F2)

These changes add inputs. They change no decision in any earlier file, and
run 19 confirms that.

- The fault reader knows `read_data_addresses`. Each address is a
  `[tape_file, block]` pair.
- `read_address` judges a requested block as Section 13.4 judges a stripe
  position (GAPS L-1). A read failure, a record of the wrong length, or a
  CRC mismatch sends it to recovery. Otherwise the block is returned as read.
- The Recoverer's peer reads, and the Verifier's data and parity reads,
  report a record of the wrong length as a read failure instead of a CRC
  mismatch. No earlier case had such a record.
- Resume inputs are checked like fault maps (`validate_resume_case`): the
  top level, prefix entries, `append_object` and its options, and
  `tape_faults`. `resume` exits with status 2 on a refusal, as `decide` does,
  and a case that reaches step 4 with no `append_object` fails the run.
- `tape_faults` damages the tape the Resumer re-reads. Record edits are
  checked as in `decide`, and each unreadable record's LBA is checked against
  the image layout.
- Step 2's refusal for a prefix whose finalization has begun now cites that
  sentence in the decision as well as in step 2.
- `blind-mapping.json` and `blind-resume-mapping.json` map the e2 ids to
  themselves.
- `test_unpinned_and_unit_cases` expects the nine overflow-3.2-lba rows to
  match, now that the manifest pins them. No decision changed.

## Changes for the e2-08 to e2-13 and e3 cases (F3)

These changes add inputs, one observation and one redefined status. They
change no earlier decision except the four selection rows below, and run 20
confirms that.

- The fault reader knows `record_insertions` and `appended_files`, and checks
  every level of each. An appended record has a `source`: `foreign` (the stated
  first byte, then zeros), `copy_of` (my build of a record) or
  `second_edition_replica` (not derivable; never read). A source it does not
  know fails the run. Faults are applied in one order: record edits,
  insertions, the removed filemark, appended files.
- `record_edits` accepts the construction "the ParityMap re-encoded with the
  edited directory entry". Hash and CRC recomputations are compared with mine
  on the finished record: a sidecar copy is parsed under Sections 9.2 to 9.5, a
  ParityMap's header CRC and payload SHA-256 are recomputed.
- Terminal discovery: spacing back from EOD now crosses records to the nearest
  filemark (GAPS M-5). A tape whose last file has no filemark no longer ends the
  search at once.
- The walk: an Object candidate after the exact terminal suffix is a
  nonconformant artifact and not admitted as an Object (Section 12.6; GAPS
  M-6); a missing trailing filemark is reported as structural damage (Section
  12.2; GAPS M-7); a file whose head or last block is not derivable is
  undecidable, and the decision says whether the walk produces a map
  (`walk.map`).
- A record the Scanner must read and cannot derive makes its decisions
  `undecided`: the Scanner, the Verifier, and any Recoverer address (GAPS M-4).
- The new observation `verifier-full` (Section 2.2): each failing data block and
  parity shard by address, every other finding, whether the terminal suffix is
  complete, and the coverage. `verifier_prefix_findings` now reads the parity
  region of an epoch whose index is unavailable (GAPS M-8).
- Quotes. Three sentences are added to the cited set: Section 12.6's artifact
  and normal-suffix sentences and Section 12.2's structural-damage sentence.
- Selection. The status S4 is now a second-edition replica. Its `foreign` class
  and the code that built another profile's replica are gone; the new class
  rebuilds the profile's replica with another edition ID and sequence (GAPS
  M-9). A decision among fully valid replicas of two editions is
  `TerminalIndexReplicaConflict`.
- `blind-mapping.json` maps the e2-08 to e2-13 and e3 ids to themselves.
- The unit tests replace the foreign-replica test with second-edition tests and
  add the new fault keys, the artifact and torn-tail decisions, the undecidable
  records and the full verification (95 tests).

## Changes for the e3 fixtures' answers (F3b)

- The fault reader requires `fill` on a foreign record, and it must be the sentence
  "first_byte, then zeros to the stated length". A `second_edition_replica` record
  needs `base`, `fields` and `role`; an appended file may carry `replica`. Every
  level is checked and an unknown key still fails the run.
- A second-edition record is my build of the base record with each field's hex
  written at its offset. Its SHA-256 is checked. The `replica` key is
  cross-checked (layout tuples, edition, planned position, EOD); my frame CRCs
  and the footer's header hash are reported.
- e3-07 is decided. The undecidable-record mechanism stays, and no case uses it.
  The unit tests replace the three undecidable-record tests.

## Known divergences, not changed

The supplement's rule-by-rule auditor found places where this
implementation's parsers read the text differently from the most direct
reading. None changes a decision, so they are recorded here and in GAPS.md:

- The sidecar header parser evaluates the locator formulas with the Section
  9.4 recomputed H and the 0x50 field P (GAPS G-1).
- The directory decoder checks the range chain in array order, where the
  text says "taken in ascending `tape_file_number` order" (GAPS G-6).
- `check_replica` and `check_separation` record a filemark or EOD met where
  the plan puts a data record with a reason that begins `TapeIo`. Section
  3.5 reserves `TapeIo` for "a transport or medium failure and for the
  device reports that Section 2.4 names", and Section 10.6 makes the
  measured location a validity condition. Every decision and every
  implementation `error` names the component's parse error. Only the
  reason string keeps the old prefix, in one place: `mut-04`'s
  implementation reason for extent A-B.

The replica payload check's evaluation of "W equals T" on recomputed values
(GAPS G-7) was fixed in F0: Section 10.6 now says the rule uses the recorded
fields.
