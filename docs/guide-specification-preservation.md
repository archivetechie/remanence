# Checking specification preservation

`tools/spec_preservation.py` compares a frozen baseline with a candidate. It
inventories affected prose blocks, headings, schemas, table rows, identifiers,
formulas, and normative statements. The complete baseline inventory is retained
in the JSON output, so a missing section remains visible after an edit.

```sh
python3 tools/spec_preservation.py inventory --baseline baseline.md --candidate candidate.md --out inventory.json
python3 tools/spec_preservation.py check --baseline baseline.md --candidate candidate.md --dispositions dispositions.json --receipt review.json
```

The disposition object must bind `baseline_sha256` and `candidate_sha256` from
the inventory and contain exactly one `items` row for each affected item ID.
Each row has `id` and `status`: `retained`, `moved`, `changed`, or `retired`.
All except retired rows identify an inclusive, one-based `candidate_span`.
An optional `candidate_file` names another document in the JSON file-to-text
map supplied with `--candidate-files`; cross-file folds are checked against that
complete candidate set. Review receipts also bind `candidate_files_sha256`: the
canonical hash of the file-name-to-content-SHA256 map.
Retained/moved spans must match the original content after whitespace normalization.
Changed and retired rows need a substantive `rationale` and independent review.
Changed spans preserve counts of normative keywords (including negated phrases
such as `MUST NOT`), backticked identifiers and RFC references, and schema items require a complete fenced destination that
retains field/type names and integer map-key tokens. This is a conservative
structural check, not a schema-equivalence proof. A
substantive change may name an explicit `exceptions` entry (`normative-keywords`,
`identifiers`, `rfc-references`, or `fence-structure`), bound into its independently
reviewed disposition and rationale. Every exception also requires explicit
`--edit-kind substantive`; a missing classification or wording edit cannot waive
these minimums.

Review receipts bind the baseline, candidate, canonical inventory, and canonical
dispositions hashes. They identify distinct author/reviewer sessions, a passing
verdict, and every changed/retired item in `reviewed_ids`. Canonical hashes use
UTF-8 JSON with sorted keys and compact separators. These receipts are evidence
references; the generic checker does not authenticate who created them. A trusted
supervisor must own review execution and approval storage.

Retirement additionally requires explicit `--edit-kind substantive`, approved
`--design` dispositions bound to the baseline, and a `design_item` naming a real
containing baseline section ID. The section must be retired in the design; a
retained containing section blocks retirement. Wording edits cannot retire content.
If design evidence is supplied, its canonical hash is bound by the review receipt.

The tool accepts plain UTF-8 or gzip inputs. Exit 0 means the mechanical checks
passed; 1 means findings; 2 means invalid input or execution failure. A pass does
not establish semantic correctness or implementability. New requirements and
architectural changes still need independent specification review.

Run the regression tests with:

```sh
python3 -m unittest discover -s tools -p 'test_spec*.py'
python3 -m unittest discover -s tools/oracle -p 'test_*.py'
```

The historical fixtures replay the draft.5 deletion and reject unmapped loss and
false mappings to surviving sections. They also check complete section moves and
unchanged documents. Frozen fixtures and independent oracle tests should change
only when their intended contract changes through separate review.
