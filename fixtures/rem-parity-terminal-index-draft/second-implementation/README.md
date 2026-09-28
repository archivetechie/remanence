# REM-PARITY generation 2: second implementation

This directory holds the output of a second implementation of REM-PARITY
generation 2, written from the specification text alone and without the
reference implementation. It exists to supply the second, independent
derivation that REM-PARITY freeze criterion 2 asks for:

- every pinned candidate byte is re-derived from its recorded inputs; and
- every damage case is decided from the text, before it is compared with an
  expected outcome.

The program is `tools/rem_parity_second_implementation.py`, and its tests are
`tools/test_rem_parity_second_implementation.py`. Both use the Python 3
standard library and the two tool modules described below. The REM-OBJECT
module imports the `cryptography` package, so install it first
(`python3 -m pip install -r tools/requirements-rem-object-independent.txt`).

## What it was built from

It was built from four documents:

- the REM-PARITY preparing copy, `specs/in-progress/rem-parity-1-specification.md`;
- for the Object recovery rows, the REM-OBJECT and REM-ENCRYPT preparing copies;
- the candidate fixtures' inputs and pinned digests under
  `fixtures/rem-parity-terminal-index-draft/`.

It was not built from the reference implementation's source or
documentation, or from the Implementation and Operations Guide. Nothing it
decides needed the Guide.

## What it imports, and why

The program imports two groups of definitions.

| Imported from | Definitions | Why | How it was checked |
| --- | --- | --- | --- |
| `tools/rem_parity_rederive.py` | `gf_mul`, `gf_inv`, `cauchy_matrix`, `_multiplication_table`, `encode_parity`, `crc64_xz`, `_cbor_type_and_length`, `encode_deterministic_cbor`, `decode_deterministic_cbor`, and the error class `RederivationError` they raise. `gf_pow`, `_crc64_table_entry`, the CRC table and the CBOR head and item decoders are used through these. Three constants, `GF_REDUCTION_POLYNOMIAL`, `CRC64_XZ_REFLECTED_POLYNOMIAL` and `MASK64`, are imported so the tests can check them. | The field, generator, encoding, CRC and CBOR rules of Sections 5.1, 5.3 and 6 are short and fully specified. Definitions checked against the text are no weaker than a rewrite. | Each definition was read against Sections 5.1, 5.3, 6.1–6.3 and 6.8. The tests reproduce the CRC vectors of Section 5.1, the inverses, generator and parity of Section 6.8, and every erasure pattern at that geometry, as well as the canonical digest of Section 7.3 and Appendix A.3, the mapping of Appendix A.1 and the index layout of Appendix A.2. The encoder's key order is the length-first order that Section 5.3 states. `GAPS.md` entry A-1 records that the RFC section it cites specifies another order. |
| `tools/verify_rem_object_vectors_independent.py` | `build_plaintext_with_manifest` and `FileSpec` | It builds each image's plaintext REM-OBJECT Objects from their recorded inputs, and it was written from the REM-OBJECT text without the reference implementation. | Each choice it makes was checked against the REM-OBJECT sections listed in `GAPS.md` entry A-11. A wrapper enforces the REM-OBJECT Section 4.6.6 input rules that the builder leaves to its caller. |

Before these imports replaced verbatim copies of the same definitions,
`build` reproduced every artifact and `decide` wrote `decisions.json`. After
the change, `build` reproduces every artifact again with a byte-identical
`build-report.json`, the tests pass, and `decide` on the blind cases gives a
byte-identical `decisions.json`. A test checks that each imported name is
the tool module's own object.

## Running it

From the repository root:

```sh
python3 tools/rem_parity_second_implementation.py build
python3 tools/rem_parity_second_implementation.py decide <case.json> [<case.json> ...]
python3 tools/rem_parity_second_implementation.py resume <resume-case.json> [<resume-case.json> ...]
python3 tools/rem_parity_second_implementation.py negatives <negative-cases.json>
python3 tools/rem_parity_second_implementation.py negatives-supplement <supplement.json>
python3 tools/rem_parity_second_implementation.py negative-blocks
python3 tools/rem_parity_second_implementation.py mutations <mutations.json>
python3 tools/rem_parity_second_implementation.py selection <selection.json>
python3 -m unittest tools/test_rem_parity_second_implementation.py
```

`build` re-derives these artifacts and compares each with its candidate
bytes or pinned digests:

- the six full tape images, compared per tape file, torn tail and `ALL` with
  `tape-images/MANIFEST.tsv`;
- the six terminal profiles, compared byte for byte and on every column of
  `MANIFEST.tsv`;
- the maximum slots and footer (`MAXIMUMS.tsv`);
- the Object-row extension slots (`OBJECT_ROW_EXTENSIONS.tsv`);
- the million-row streaming digests (`STREAMING.tsv`), computed in constant
  memory.

For each artifact it prints `reproduced`, or the first differing byte and
the field that byte belongs to. It writes `build-report.json`. When a
refused input stops the build, the refusal names that input field.

`decide` takes damage-case files. A case's id is its file's stem, or, for a
file named `fault-map.json` (a damage case) or `inputs.json` (a resume case,
as the repository lays them out), its directory's name. For each case it builds
the named image, applies the case's faults, and then acts as a Reader. It
writes two files here:

- `decisions.json`, in the `rem-parity-second-implementation-decisions/1`
  schema;
- `decisions-trace.json`, which records what the Reader read and why each
  step ended as it did.

`resume` takes resume-case files. Each names an image, gives a committed
prefix as Section 7.1 entries with its `W` and `T`, and gives an Object to
append. For each case, the program acts as a Resumer under Section 14 on
the undamaged image, treating the prefix as the off-tape commit authority.
It writes `resume-decisions.json`; see "The resume schema" below.

`negatives` takes a negative-case file. For each case and variant it does
three things:
- it applies the text-level mutation to my own build of the base artifact,
  checking every stated `from` value against my bytes;
- it applies the listed repairs;
- it runs the target role on the mutated bytes.

The decision for each entry comes from reading the text: the Section 15
name, a set where Section 15 permits one, acceptance, or `undecided` with
the readings. The implementation's result is recorded beside it as a
self-check. The command writes `negative-decisions.json`; see "The
negatives schema" below.

`negatives-supplement` takes the supplemental single-rule variants. For each
variant it does three things:

- **Apply.** It applies the variant against my own bytes. That includes the
  recipes that rebuild a sidecar, a ParityMap or a terminal suffix, block
  faults presented as medium errors, and unit-level inputs.
- **Check isolation.** A rule-by-rule auditor checks the claim that only one
  rule fails. It evaluates each Section 9.2–9.5 rule of both sidecar copies
  on its own, under both readings of the locator symbols (GAPS G-1) and
  both readings of the Section 9.5 hash (F-1). It also runs the
  cross-structure rules, and evaluates the ParityMap and directory rules one
  by one. For terminal variants it uses the problems the replica validation
  collects.
- **Decide.** It decides the outcome under the text.

It writes `negative-supplement-decisions.json`, whose entries add an
`isolation` block (`claim`, `by_copy` failing rules per reading,
`cross_structure_failing`, `disputed_by_implementation`, `disputes`) to the
keys of the negatives schema below.

`negative-blocks` closes the first criterion-2 gap that Appendix D item TT-2
lists for the negative vectors. It re-runs only the apply step of every
negative case and supplement variant in `blind-inputs/`, on my own builds,
and emits every block whose bytes the mutation and its repairs change, with
its size and SHA-256. A block counts as changed when its bytes differ from the
base artifact's block at the same tape file and block, or when the base has no
block there. It relates the opaque ids to the real case ids through the two
mapping files, compares each block with `tape-images/negatives/MANIFEST.tsv`,
and reports every row as matched, mismatched or missing from one side. The
manifest pins only sizes and digests, so a mismatch can be located only
against a candidate that reproduces the pinned digest. The one such candidate
is a diagnostic for sup-14, whose variant says its parity bytes are
arbitrary; it keeps the base image's parity, and it never replaces the bytes
this implementation emits. The command writes `negative-block-digests.json`
and leaves both earlier decision files untouched.

`mutations` takes the 50 terminal-index mutations, described byte by byte
under opaque ids. For each row it does three things:

- **Apply.** It applies the description to my own build of the named
  profile. Every stated old value is checked against my bytes, every
  checksum or hash the row lists as recomputed is recomputed from my bytes
  and checked against the stated new value, and every value the row says is
  retained is checked in place. Length changes and record replacements are
  applied as described, from my own builds of the donor components. A row
  that is an event with no byte effect changes nothing.
- **Decide.** It decides from the text what happens to the mutated
  component (the Section 15 name, a permitted set, or `undecided` with its
  readings) and to the tape: an inventory with its acceptable selections and
  whether it is degraded, a conflict, or `BotStructuralRecoveryRequired`.
  It also states what a Verifier must report.
- **Run.** Its Scanner runs on the resulting record stream, and the result
  is recorded beside the decision as a self-check.

It writes `mutation-decisions.json`.

`selection` takes the 14 survivor sets. Each names a status (S0 to S4) for
replicas A, B and C, and each status is defined in the file by its bytes. For
each set the command builds the three replicas from my own builds (a byte
change is applied as `mutations` applies one; the foreign status takes my
build of the other profile's replica at that position), decides the outcome
from the text, and runs the Scanner as a self-check. Where the text does not
decide which of several valid, agreeing replicas is selected, every one of
them is listed as acceptable. It writes `selection-decisions.json`.

For both commands the tape is the profile's prefix, written as placeholder
records of the profile's recorded block counts, followed by the five
components. A component stream is written as records of the block size in
order, so a stream whose length is not a multiple of the block size ends in a
short record. The profiles pin no bootstrap bytes, so the Reader takes the
tape UUID and block size from the profile's inputs, as a readable bootstrap
would supply them. No bootstrap walk is run; where the outcome is
`BotStructuralRecoveryRequired`, that outcome is the decision.

The tests accept seven optional environment variables:

- `REM_PARITY_SECOND_IMPL_SCRATCH` names a working directory;
- `REM_PARITY_SECOND_IMPL_CASES` names a directory of case files for the
  determinism test;
- `REM_PARITY_SECOND_IMPL_RESUME` names a directory of resume-case files
  for the resume determinism test;
- `REM_PARITY_SECOND_IMPL_NEGATIVES` names a negative-case file for the
  negatives tests;
- `REM_PARITY_SECOND_IMPL_SUPPLEMENT` names a supplement-variant file for the
  supplement tests;
- `REM_PARITY_SECOND_IMPL_MUTATIONS` names a terminal-mutation file for the
  mutation tests (default `blind-inputs/mutations-blind.json`);
- `REM_PARITY_SECOND_IMPL_SELECTION` names a survivor-set file for the
  selection tests (default `blind-inputs/selection-blind.json`).

## How the Reader works

The Reader sees only three things:

- the damaged record stream, with its filemarks and EOD;
- the case's hints;
- the case's failed data addresses.

The fault model comes with the cases and is taken as given. An unreadable
record fails every READ that begins at it, positioning is unaffected, and a
removed filemark merges its two tape files. A hint of `"the image's"` for
the tape UUID is replaced by the image's UUID. Nothing else from the inputs
reaches the Reader. `bytes_match` and `inventory_equals_true_prefix` are
computed only after every decision in the case is fixed.

The steps follow the text in order:

1. Bootstrap discovery (Section 8.4, and the Section 8.4.1 hints).
2. Terminal discovery from EOD, replica validation under Section 10.6, and
   selection under Section 8.5.
3. When no replica validates, the BOT walk and its second pass (Sections
   8.4.1, 12.2 and 12.3), and the validation of the walked map against the
   final ParityMap (Section 13.1).
4. For each failed address, the Recoverer's refusals, index acquisition,
   erasure taxonomy, reconstruction and CRC check (Sections 13.2–13.5).
5. The Verifier. Its `result` is the terminal-suffix outcome that Sections
   10.6 and 12.6 define: complete, degraded, recovery required (no valid
   replica), or an error for a conflict. The text names no Verifier outcome
   for damage before replica A, so each such finding is listed in an
   `undecided` entry instead (`GAPS.md` entry B-3).

Where the text leaves an outcome open, the decision says `undecided` and
lists the readings. Each decision cites the sentences that decide it, and
the tests check that every quoted sentence occurs in the specification.
Where the text grants a permission that a case's inputs make usable, and
declining it leaves only a refusal, this implementation chooses to use it.
That is a documented choice, not a requirement: a Scanner that declines
the §8.4 hint path and reports `NoBootstrapFound` is also conformant
(`GAPS.md` entry B-1). Where declining a permission leads to a
different conformant outcome, the aspect is reported as `undecided`
(entry B-2).

## How the Resumer works

The steps follow Section 14:

1. W and T are derived from the prefix (Section 7.2) and compared with
   the values given. The append point is `Σ(block_count + 1)` over the
   prefix.
2. The step-2 rules that need no scheme are checked first. Then the
   bootstrap at LBA 0 is read for `S` and `k`, and the bound
   `T − W < S × k` is checked. The text does not say where a Resumer
   obtains the scheme (`GAPS.md` entry E-2).
3. The open epoch `[W, T)` is re-read from the tape. A filemark, EOD, a
   medium error or a record of the wrong size there is fatal.
4. The Resumer positions to the append point and writes the Object as the
   next tape file. At `S × k` ordinals the epoch closes at Object close.
   When the session ends, the Writer closes an epoch still open by writing
   its sidecar, even if it is short.

After each decision, and only for reporting, an accepted resume is compared
file by file with the same Objects written in one uninterrupted session.

## The resume schema

`resume-decisions.json` has `"schema": "rem-parity-second-implementation-resume/1"`
and a `cases` map keyed by case id. Every case has the same aspects:

| Key | Contents |
| --- | --- |
| `image` | The image the case names |
| `prefix` | `entries`, `derived_W`, `derived_T`, `given_W`, `given_T`, `W_equal`, `T_equal`, `append_point_lba`, `map_findings` (Section 7.2 findings that step 2 does not list), `citations` |
| `scheme` | `k`, `m`, `S` and their `source`, or nulls when step 2 refused before the scheme was needed |
| `step2` | `result` (`pass`, `violation`, `undecided` or `not_run`), `violations`, `citations` |
| `step3` | `result` (`run`, `fatal` or `not_run`), the `ordinals` re-read and their `lbas`, the `failure`, `citations` |
| `step4` | `result` (`run` or `not_run`), `append_point_lba`, `citations` |
| `decision` | `result` (`accepted` or `refused`), `error` (a Section 15 name, or `undecided`), `refused_at`, `before_any_tape_read` (`true`, `false` or `undecided`), `before_any_write`, `records_read` (LBAs, in order), `citations` |
| `append` | `null` for a refusal. Otherwise: `object_tape_file`, `object_first_lba`, `object_blocks`, `object_first_ordinal`, `object_row` (Section 10.3 keys as strings, byte strings as hex), `sidecars` (each with `tape_file`, `first_lba`, `epoch_id`, the protected range, `total_blocks` and `closed_by`), `tape_files` (every tape file of the resulting tape and `ALL`, in the `tape-images/MANIFEST.tsv` convention), `eod`, `uninterrupted_equal`, `uninterrupted_differences` and the `session_end_citations` |
| `undecided` | Aspects the text leaves open, each with its readings and citations |

## The negatives schema

`negative-decisions.json` has `"schema": "rem-parity-second-implementation-negatives/1"`
and an `entries` map keyed `neg-NN`, or `neg-NN/<variant>` for a case with
variants. Every entry has the same keys:

| Key | Contents |
| --- | --- |
| `id`, `variant`, `target` | The case, its variant, and the target as the case names it |
| `apply` | `resolved` (`true`, `false`, or `null` when there is no tape vector), `vector` (`bytes`, an injected commit record, or `none`), `checks` (each stated `from` value against the byte found), `repairs` applied, `mutated_blocks` (size and SHA-256 of each changed block after repair), `notes` |
| `decision` | The text decision. `outcome` is `rejected`, `accepted`, `undecided`, `no-vector` or `unresolved`. `error` is a Section 15 name, `undecided`, or `null` where no name applies. `error_set` holds the names where Section 15 permits a set. `readings` are given where the text leaves the outcome or the name open. `rules` are the quoted sentences that fail. `order` says which rule fires first, or that the text fixes no order. `reader_must_reject` is `true`, `false` or `undecided`. `formula` gives a named formula evaluated with the case's inputs, with its type and whether it overflows. `note` gives the consequence at the level of the whole tape. |
| `implementation` | What my implementation reported for each role run: a copy or footer validator, the Recoverer, a Verifier reading both copies, the bootstrap parser, ParityMap validation, terminal selection with separation checks, or the Resumer |
| `self_check` | Whether the implementation's result agrees with the text decision, with the detail |

Entries whose inputs are off tape, reported by the device, or evaluated by
no role (neg-04, neg-05, neg-26) have no vector. For them the decision
records what the text lets a Reader decide.

## The negative-blocks schema

`negative-block-digests.json` has
`"schema": "rem-parity-second-implementation-negative-blocks/1"`.

| Key | Contents |
| --- | --- |
| `counts` | `matched`, `mismatched`, `missing_from_mine`, `missing_from_manifest`, the number of manifest rows and the number of blocks emitted |
| `entries` | For each opaque id: its real id, its `vector` (`bytes`, `unit`, `resume` or `none`), the number of blocks emitted, and any stated old value that did not match my bytes |
| `rows` | One row per key in either set, keyed by real case id, artifact, tape file and block. A row gives my size and SHA-256, the pinned ones, and the `result`. A mismatch carries `first_differing_byte` and `field` when a diagnostic candidate reproduces the pinned digest, and says why not otherwise. A row missing from my side says whether my block at that key has the pinned digest. |

A terminal profile's blocks are keyed by the profile's own tape-file
numbering, in which replica A is the profile's first terminal tape file (6
for `multi-256k`, 1 for `minimal-256k`). The profiles that the supplement
generates are compared with `multi-256k`, component by component.

## The mutations and selection schemas

`mutation-decisions.json` has `"schema": "rem-parity-second-implementation-mutations/1"`
and an `entries` map keyed `mut-NN`.

| Key | Contents |
| --- | --- |
| `id`, `kind`, `base_profile`, `target`, `other_profile` | The row as the file gives it |
| `apply` | `resolved`, `vector` (`bytes` or `none`), `checks_total` and `checks_failed` (stated old values, recomputed checksums and retained values against my bytes), `repairs` performed, `notes` (including what the row leaves stale), `record_lengths` of each component as written to tape, and `changed_components` with their size and SHA-256 |
| `decision.component` | The mutated component (`replica A`, `separation extent A-B`, or `null`), `outcome` (`rejected`, `no-vector` or `unresolved`), `error`, `error_set` where more than one name is permitted, `readings`, the quoted `rules` and the reasoning (`why`) |
| `decision.tape` | `outcome` (`inventory`, `TerminalIndexReplicaConflict`, `BotStructuralRecoveryRequired` or `undecided`), `acceptable_selections`, `degraded` (`true`, `false` or `undecided`), each replica's status, the quoted `rules` and the reasoning |
| `decision.verifier` | What a Verifier must report: `result` (`complete`, `degraded` or `not complete`) and each separation extent's status |
| `decision.undecided` | Aspects the text leaves open, each with its readings and citations |
| `implementation` | What my Scanner and Verifier reported: the layout source, each replica's and extent's validity, error and reason, and the selection |
| `self_check` | Whether the implementation's result agrees with the decision; where the decision is `undecided`, it must be one of the readings |

`selection-decisions.json` has `"schema": "rem-parity-second-implementation-selection/1"`,
the class of each status, and an `entries` map keyed `sel-NN`. Each entry
gives the statuses of A, B and C, an `apply` block like the one above, the
`decision` (`outcome`, `acceptable_selections`, `degraded`, each replica's
status and reason, the quoted `rules`, the `readings` of an undecided
outcome, and the `undecided` aspects), the `implementation` result and the
`self_check`. A set that includes an absent replica is run twice, with and
without that position's trailing filemark, because the status does not say
whether the filemark remains.

## Files

| File | Contents |
| --- | --- |
| `README.md` | This description |
| `BUILD-LOG.md` | Every failed comparison, what was consulted and how it was resolved |
| `GAPS.md` | Text defects, silences and criterion-2 gaps, each with its section |
| `build-report.json` | The last `build` run's result for every artifact |
| `decisions.json` | The last `decide` run's decisions |
| `decisions-trace.json` | The reads and reasons behind each decision |
| `decisions-real-ids.json` | The same decisions, from the repository's `fault-map.json` files and keyed by the real case ids |
| `decisions-real-ids-trace.json` | Its trace |
| `COMPARISON.md` | The comparison of the blind decisions with each case's `expected.json` |
| `comparison.json` | The same comparison in machine-readable form, with the interpretation used for each expected key |
| `DECISION-LOG.md` | Every decision changed after the comparison (none) |
| `resume-decisions.json` | The Resumer's decisions on the resume cases |
| `negative-decisions.json` | The decisions on the generation-2 negative cases |
| `negative-supplement-decisions.json` | The decisions and isolation audits for the supplemental single-rule variants |
| `negative-block-digests.json` | Every block the negatives change, compared with `tape-images/negatives/MANIFEST.tsv` |
| `mutation-decisions.json` | The decisions on the 50 terminal-index mutations |
| `selection-decisions.json` | The decisions on the 14 terminal survivor sets |
| `blind-inputs/mutations-blind.json`, `blind-inputs/selection-blind.json` | The mutation and survivor-set descriptions this implementation decided from, exactly as it received them |
