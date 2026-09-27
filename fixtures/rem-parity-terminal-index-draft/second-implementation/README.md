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
file named `fault-map.json`, its directory's name. For each case it builds
the named image, applies the case's faults, and then acts as a Reader. It
writes two files here:

- `decisions.json`, in the `rem-parity-second-implementation-decisions/1`
  schema;
- `decisions-trace.json`, which records what the Reader read and why each
  step ended as it did.

The tests accept two optional environment variables:

- `REM_PARITY_SECOND_IMPL_SCRATCH` names a working directory;
- `REM_PARITY_SECOND_IMPL_CASES` names a directory of case files for the
  determinism test.

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
