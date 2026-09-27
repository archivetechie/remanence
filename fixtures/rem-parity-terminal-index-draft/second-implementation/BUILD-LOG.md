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

## Failed comparisons

| Artifact | First differing byte and field | Section consulted | Resolution |
| --- | --- | --- | --- |

No comparison has failed. The builder was written from the text before any
comparison was run, and the first run reproduced every artifact.

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
