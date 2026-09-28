# Negatives comparison

This compares three things for the 58 generation-2 negative cases (84 entries counting variants). The
first is the expected outcomes, written from the text by the negatives author
(`../tape-images/negatives/negative-cases.json`). The second is this implementation's blind decisions
(`negative-decisions.json`). They were made under opaque ids, from the base artifact, the mutation,
the repairs and the target role, with every expected outcome withheld. The third is the reference
executor's observations. `blind-negatives-mapping.json` relates the opaque ids to the cases.

## Result

| Class | Entries | Remarks |
| --- | ---: | --- |
| Agree | 71 | Same §15 name, or overlapping permitted sets. |
| Agree at a different observation | 2 | `terminal-object-id-over-64/b-all-replicas`: each replica fails with `TerminalIndexReplicaParse`, which the expectation pins; this implementation also reports the tape-level outcome, `BotStructuralRecoveryRequired`. `overflow-13.3-tail-location/c-total-zero`: this implementation decides the Recoverer's outcome, `SidecarMetadataUnavailable{0}`. That is the corrected outcome in the adjudication's settled entry. The decoder observations keep the expected set. |
| Agree, text leaves it open | 10 | This implementation marks the name `undecided`, and one of its readings is the expected outcome. For `overflow-6.6-scheme-product`, `overflow-9.1-P` and `overflow-9.2-SxK` the readings are `BootstrapParse` or `NoBootstrapFound`; §15's bootstrap names overlap, and TT-7 is open (GAPS F-6). `bootstrap-payload-past-block/a` and `/b` add `BootstrapPayloadTooLarge`. For `sidecar-total-block-count/c-footer-zero-hostile` the readings are `SidecarParse` or no §15 error, because the footer's total has no rule of its own (GAPS F-7). The expectation left `overflow-12.2-walk-length`, `overflow-3.3-stripe-mapping-inverse` and `overflow-3.2-lba` unpinned too. For `overflow-9.1-total-2H-P-1/a-footer`, the label "SidecarParse (footer invalid)" names the same outcome. |
| No vector on either side | 1 | `overflow-10.3-close-reserve`: only a Writer evaluates it. |

There are no disagreements between the expected outcomes and this implementation.

The reference agrees with every pinned expectation, apart from the two cases in
`../tape-images/negatives/adjudications.json`:
- `sidecar-primary-tail-disagreement`, open. This implementation decides as the adjudication separates
  the case: a Verifier rejects the divergence with `SidecarParse`, and the text leaves the Recoverer's
  consequence open.
- `overflow-13.3-tail-location/c [recovery]`, settled. The reference and this implementation both give
  `SidecarMetadataUnavailable{0}`.

The full three-way table is kept with the review record.

## The single-violation supplement

This implementation decided the 49 variants of the supplement's first version under opaque ids
(`negative-supplement-decisions.json`; `blind-supplement-mapping.json` relates the ids to the variants). Compared with that
version's expectations:
- 44 agree. For the sidecar variants, the per-copy outcome (`SidecarParse`) and the Recoverer's
  outcome (`SidecarMetadataUnavailable{0}`) both agree.
- 5 are left open by the text. This implementation marks them `undecided`, and each expectation is
  one of its readings. Two of them turn on whether a Reader must reject an intermediate overflow
  whose final result fits (GAPS G-2).
- None disagrees.

For six variants, this implementation's own parsers find a second failing rule. That follows from
its reading choices, such as the recorded or the defined H and P, and the array or tape-file order
of the directory. None of the six changes a decision; they are listed in `BUILD-LOG.md`.

The supplement's author later issued an erratum, now in the fixture's version:
- `overflow-10.4-slot-product/isolated-2` changes from s = 1, o = 2^56 to s = 2^56 + 1, o = 2^56,
  because the first choice also broke the rule o ≤ s − 1.
- `overflow-10.1.2-2M-footer/isolated` moves to the list of cases that cannot be isolated.

These two were not decided again. This implementation's decision on the first version of the
slot-product variant rejects on the same `256 × o` overflow, which the corrected inputs also reach.

## Inputs

`blind-inputs/` holds the two files this implementation decided from, exactly as it received them. They
are the cases with every expected outcome, quote, shadowing and concurrent-check note removed, under
opaque ids. CI re-runs `negatives` and `negatives-supplement` on them and compares the output with the
committed decision files.
