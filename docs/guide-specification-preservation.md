<!-- code-anchor: tools/spec_preservation.py tools/test_spec_preservation.py tools/oracle .github/workflows/ci.yml @ 8238b7b3 -->
# Checking specification preservation

This guide is for a contributor who edits one of the format specifications
under `specs/`. When a specification is reorganized or reworded, a sentence
can be lost without anyone noticing, and a lost requirement changes the
format as surely as a new one does. `tools/spec_preservation.py` guards
against that. It compares the text before the edit (the baseline) with the
text after it (the candidate), lists every piece of the baseline the edit
touched, and requires a decision, called a disposition, for each one. A piece
can be retained, moved, changed or retired.

The check is mechanical. It proves that nothing was dropped silently and that
each claimed location really holds the claimed text. It does not judge
whether a changed requirement is right; that is for review.

## What the tool counts as a piece of text

The baseline is cut into items. Most lines belong to more than one item.

| Kind | What it is |
| --- | --- |
| `block` | A paragraph or other run of lines between blank lines. |
| `fence` | A fenced code block, such as a CDDL schema or a byte table. |
| `heading` | A heading line. |
| `table-row` | One row of a table. |
| `identifier-line` | A line with text in backticks. |
| `normative-line` | A line with a requirement keyword: `MUST`, `MUST NOT`, `SHALL`, `SHOULD`, `MAY`, `REQUIRED`, `RECOMMENDED`, `OPTIONAL` and their negations. |
| `formula-line` | A line outside a table with `=`, `≤`, `≥`, `\frac`, `ceil(` or `floor(`. |

Each item has an ID of the form `<kind>:<first line>:<last line>:<hash>`,
where the hash is the first 20 hexadecimal digits of the SHA-256 of the item's
text with runs of whitespace collapsed. An item is affected when a line diff
between baseline and candidate deletes or replaces any of its lines. Only
affected items need a disposition, but the output keeps the inventory of the
whole baseline, so a deleted section stays visible after the edit.

## Steps

The example below moves the section "Padding" in front of "Records" in a
small specification. The baseline, `baseline.md`:

```text
# Example format

## Records

A reader MUST reject a record whose `length` field exceeds the block size.

## Padding

Writers SHOULD pad the final block with zero bytes.
```

The candidate, `candidate.md`, holds the same two sections in the other
order.

1. List the affected items:

   ```sh
   python3 tools/spec_preservation.py inventory --baseline baseline.md \
       --candidate candidate.md --out inventory.json
   ```

   `inventory.json` has the schema `spec-preservation.v1`. Its fields are
   `baseline_sha256` and `candidate_sha256` (the two texts' digests),
   `baseline_inventory` (every item), `items` (the affected items), `sections`
   (each heading and the lines its section spans) and `added_spans` (the
   candidate lines the diff inserted). Here `items` holds three entries, with
   their `id`, `kind` and `source_span`:

   ```text
   heading:7:7:d305fe058106b52405ba         heading         [7, 7]
   block:9:9:24919e1c9da6a4a21de3           block           [9, 9]
   normative-line:9:9:24919e1c9da6a4a21de3  normative-line  [9, 9]
   ```

2. Write `dispositions.json`. It repeats the two digests from the inventory
   and has exactly one row in `items` for each affected item. Each row has an
   `id` and a `status`, and every status except `retired` names the
   candidate lines that now hold the text, as `candidate_span`: the first and
   last line, counted from 1, inclusive.

   ```json
   {
     "baseline_sha256": "91158d05dabf521ed3b6c3d52857f96e3adfcf3272753b659f896335fa891acc",
     "candidate_sha256": "63678dd360166224deff9938d7d60bcff61726401f9d95c7b36fdd0c48f2b9a3",
     "items": [
       {"id": "heading:7:7:d305fe058106b52405ba", "status": "moved", "candidate_span": [3, 3]},
       {"id": "block:9:9:24919e1c9da6a4a21de3", "status": "moved", "candidate_span": [5, 5]},
       {"id": "normative-line:9:9:24919e1c9da6a4a21de3", "status": "moved", "candidate_span": [5, 5]}
     ]
   }
   ```

3. Check the dispositions:

   ```sh
   python3 tools/spec_preservation.py check --baseline baseline.md \
       --candidate candidate.md --dispositions dispositions.json
   ```

   ```text
   spec-preservation: PASS (0 finding(s))
   ```

   Run without `--dispositions`, the same check lists every affected item as
   unaccounted and fails. A disposition that points at the wrong lines fails
   too. With both spans set to `[9, 9]` the check prints:

   ```text
   block:9:9:24919e1c9da6a4a21de3: original content is absent from the claimed candidate span
   normative-line:9:9:24919e1c9da6a4a21de3: original content is absent from the claimed candidate span
   spec-preservation: FAIL (2 finding(s))
   ```

## The four dispositions

| Status | Requires | Checked |
| --- | --- | --- |
| `retained` | `candidate_span` | The span holds the item's text, ignoring differences in whitespace. |
| `moved` | `candidate_span` | The same as `retained`; the status records that the text now sits elsewhere. |
| `changed` | `candidate_span`, a `rationale` of at least four words, and a review receipt | The span must keep at least as many requirement keywords (counting negated forms such as `MUST NOT`), backticked identifiers and RFC references as the original. A changed fenced block must map to a complete fenced block that keeps its field and type names and its integer map keys. |
| `retired` | A `rationale`, a `design_item`, `--edit-kind substantive`, a `--design` file, and a review receipt | The design file must name the baseline's digest and mark the section given in `design_item` as retired. That section must contain the item, and no section containing the item may be marked retained. |

A changed row can list `exceptions` to the counting rules: `normative-keywords`,
`identifiers`, `rfc-references` or `fence-structure`. An exception is accepted
only with `--edit-kind substantive`. A wording edit, `--edit-kind wording`,
can neither waive these minimums nor retire anything. The keyword check is a
conservative structural test, not a proof that a schema means the same thing.

The design file has two fields: `baseline_sha256`, and `items`, which maps
section IDs (the heading item IDs listed in the inventory's `sections`) to
`retired` or `retained`.

A row may also name a `candidate_file`, another document that received the
text, for example when a section moves from a specification to the
implementation guide. The other documents are passed with
`--candidate-files`, a JSON object that maps each file name to its full text.

## Review receipts

A changed or retired item needs a review receipt, passed with `--receipt`. It
records that someone other than the editor reviewed exactly this edit:

| Field | Value |
| --- | --- |
| `baseline_sha256`, `candidate_sha256` | The two digests, as in the inventory. |
| `inventory_sha256`, `dispositions_sha256` | Canonical hashes of the inventory and of the dispositions. |
| `candidate_files_sha256` | When `--candidate-files` is given: the canonical hash of the map from each file name to the SHA-256 of its text. |
| `design_sha256` | When `--design` is given: the canonical hash of the design file. |
| `author`, `reviewer` | Two different, non-empty names. |
| `verdict` | `passed`. |
| `reviewed_ids` | Every changed and retired item ID. |

A canonical hash is the SHA-256 of the JSON value written as UTF-8 with its
keys sorted and no spaces between elements. To print the two that every
receipt needs:

```sh
python3 - <<'EOF'
import hashlib, json
for name in ("inventory.json", "dispositions.json"):
    value = json.load(open(name))
    text = json.dumps(value, sort_keys=True, separators=(",", ":"))
    print(name, hashlib.sha256(text.encode("utf-8")).hexdigest())
EOF
```

A receipt for a reworded sentence, checked with `--edit-kind wording`, looks
like this:

```json
{
  "baseline_sha256": "91158d05dabf521ed3b6c3d52857f96e3adfcf3272753b659f896335fa891acc",
  "candidate_sha256": "6008193baf98426d2aa94813429128d615125194844fca15a3342e78706295c2",
  "inventory_sha256": "7d7e80d5bf917c02ffdcf7fcfec938d7a7faa990c0669e6ddae83a980b4a83f1",
  "dispositions_sha256": "5bd5ddf5e68a02772a5093245a3df45669398e712c8c6e2d1fd5e83b7c279eac",
  "author": "editor-1",
  "reviewer": "reviewer-2",
  "verdict": "passed",
  "reviewed_ids": [
    "block:9:9:24919e1c9da6a4a21de3",
    "normative-line:9:9:24919e1c9da6a4a21de3"
  ]
}
```

Without a receipt, the same check fails with
`changed/retired content requires an independent review receipt`. The tool
checks that a receipt is bound to this exact evidence. It cannot check who
wrote the receipt; that rests on how the project runs its reviews, and the
receipts need not be published.

## Exit status and inputs

| Status | Meaning |
| --- | --- |
| 0 | `inventory` wrote its output, or `check` found nothing; `check` prints `spec-preservation: PASS (0 finding(s))`. |
| 1 | `check` found problems. It prints one line per finding, then `spec-preservation: FAIL (<N> finding(s))`. |
| 2 | An input could not be read or parsed; the tool prints `spec-preservation: ERROR: <reason>`. |

The baseline and candidate may be plain UTF-8 files or gzip files whose names
end in `.gz`.

A pass does not establish that the edited specification is correct or can be
implemented. A new requirement or a change to the design still needs
specification review.

## The tool's own tests

To run the tool's regression tests:

```sh
python3 -m unittest discover -s tools -p 'test_spec*.py'
python3 -m unittest discover -s tools/oracle -p 'test_*.py'
```

The fixtures in `tools/testdata/spec-preservation` are a real edit of the
REM-PARITY specification that deleted a section, and the tests require the
tool to reject the unmapped loss and false mappings to sections that
survived. Other tests cover complete section
moves and unchanged documents. Continuous integration runs these tests on
every push. The fixtures and the tests in `tools/oracle` pin the tool's
contract, so change them only when the contract itself changes, in a
separately reviewed edit.

## Related pages

- [`specs/README.md`](../specs/README.md): how the specifications are
  revised and frozen.
- [How the formats change](versioning-explained.md): the change policy that
  this check supports.
