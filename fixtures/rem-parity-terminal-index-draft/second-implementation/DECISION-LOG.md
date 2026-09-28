# Decision log

This log records every decision changed after the blind decisions were
compared with the expected outcomes. Each row gives the case, the aspect,
the old value, the new value, and the sentence that shows the blind
decision misread the text. Nothing may change without a row.

| Case | Aspect | Old value | New value | Sentence |
| --- | --- | --- | --- | --- |
| All eight resume cases (resume-01 to resume-08, and the same cases by real id) and neg-56 | The quoted text of Section 14 step 1 in their citations (nine citations per resume file, one in `negative-decisions.json`) | "Derive the committed prefix from the off-tape commit records (Section 3.4), dropping any torn tail, and compute `W` and `T` from it." | "Derive the committed prefix from the off-tape commit records (Section 3.4), dropping the tape's torn tail, if any, and compute `W` and `T` from it." | None: the text's wording changed after these decisions were written (Appendix C records it as a wording follow-up that changes no requirement). No decision changed; a leaf-by-leaf comparison of each regenerated file with its predecessor finds only this string. |

No decision has changed. The one row records a cited sentence whose wording
the text changed; the decisions that cite it are the same. The comparison
found one disagreement, filemark-prefix (case-20), and classified it as
(c): the text does not decide it. A class (c) disagreement is not a
misreading, so the blind `decisions.json` stands and no
`decisions-revised.json` exists.
