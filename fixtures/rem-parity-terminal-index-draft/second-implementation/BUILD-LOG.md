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

## Failed comparisons

| Artifact | First differing byte and field | Section consulted | Resolution |
| --- | --- | --- | --- |
| The quote `terminal_u64`, checked by the test that every quoted sentence occurs in the text | The quote read "checked in u64"; the text has "checked in `u64`" | 8.3 | A fix that reads the text correctly: the sentence is "Arithmetic on these fields is checked in `u64` (Section 2.4)." Only `negative-supplement-decisions.json` cites this quote, and it was regenerated. |

No comparison of a built artifact with a candidate or a pinned digest has
failed. The builder was written from the text before any comparison was
run, and the first run reproduced every artifact. The one row above is a
failed self-check: a quoted sentence that did not match the text.

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

## Known divergences, not changed

The supplement's rule-by-rule auditor found three places where this
implementation's parsers read the text differently from the most direct
reading. Changing them would alter earlier decision files, which must stay
byte-identical, and none changes a decision, so they are recorded here and
in GAPS.md instead:

- The sidecar header parser evaluates the locator formulas with the Section
  9.4 recomputed H and the 0x50 field P (GAPS G-1).
- The replica payload check evaluates "W equals T" on recomputed values, not
  on the recorded fields (GAPS G-7).
- The directory decoder checks the range chain in array order, where the
  text says "taken in ascending `tape_file_number` order" (GAPS G-6).
