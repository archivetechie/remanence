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

## Failed comparisons

| Artifact | First differing byte and field | Section consulted | Resolution |
| --- | --- | --- | --- |
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

## Known divergences, not changed

The supplement's rule-by-rule auditor found the first three places below
where this implementation's parsers read the text differently from the most
direct reading; the fourth was found while writing the terminal commands.
Changing them would alter earlier decision files, which must stay
byte-identical, and none changes a decision, so they are recorded here and
in GAPS.md instead:

- The sidecar header parser evaluates the locator formulas with the Section
  9.4 recomputed H and the 0x50 field P (GAPS G-1).
- The replica payload check evaluates "W equals T" on recomputed values, not
  on the recorded fields (GAPS G-7).
- The directory decoder checks the range chain in array order, where the
  text says "taken in ascending `tape_file_number` order" (GAPS G-6).
- `check_replica` and `check_separation` report a filemark or EOD met where
  the plan puts a data record as a `TapeIo` read failure. Under Section 10.6
  ("The declared locations, the counts, and the trailing filemark agree with
  the device's measurements.") that is a failed validity condition, and
  Section 15 keeps I/O faults distinct from format violations. The new
  commands name it with the component's parse error. Changing the checks
  themselves would alter the reasons recorded in earlier files, and no
  earlier decision reports that name.
