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
python3 tools/rem_parity_second_implementation.py negative-e1 <negatives-e1.json>
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
the named image, applies the case's faults, and then acts as a Reader.

The fault reader knows these keys and no others: `image`,
`failed_data_addresses`, `read_data_addresses`, `unreadable_records`,
`removed_filemark_after_tape_file`, `record_edits`, `record_insertions`,
`appended_files`, and exactly one of `hints` or `observations`. It checks
every level, including each hint, observation, record edit, byte edit,
insertion, appended file and appended record. An unknown or missing key, or an
appended record with an unknown `source`, fails the run before anything is
written, with the file and the key named, and the command exits with status 2.
An ignored key would decide a damaged tape as an intact one.

The faults are applied in a fixed order: record edits, record insertions, the
removed filemark, appended files. Every position a case states (an unreadable
record's LBA, the tape file that loses its filemark, an insertion's tape file
and record) is a position of the undamaged image.

- `record_edits` replaces a record. The new record is built from the
  original at the stated `length`: the original's own length, its first
  bytes, or the original followed by zero bytes. The construction "the
  ParityMap re-encoded with the edited directory entry" states no length of
  its own: the record keeps its length, and its listed byte edits are the
  whole change. The stated `construction` must agree with the two lengths.
  Every byte edit's `old_bytes` must be what my build holds at that offset,
  and the edited record's SHA-256 must equal the stated one; otherwise the
  run fails. An edit whose reason says a checksum or hash was recomputed is
  also checked against mine, on the finished record: a bootstrap's CRCs, a
  sidecar copy's canonical metadata hash and CRCs (the copy is parsed under
  Sections 9.2 to 9.5), and a ParityMap's header CRC and payload SHA-256. The
  result is recorded, though it is not enforced.
- `record_insertions` puts a record of `length` bytes, each the byte `fill`,
  into a tape file after the data record `after_record_index`. Its SHA-256
  must equal the stated one. Every later record, and the filemarks, move up by
  one position.
- `appended_files` puts tape files after the tape's last record. Each file
  lists its records and says whether a trailing filemark follows (`false` ends
  the tape in the file's last record). A record's `source` says how its bytes
  are built:
  - `foreign`: the stated first byte, then zeros, as its `fill` states (the
    `fill` must be that sentence). Its SHA-256 is checked.
  - `copy_of`: my build of the stated record of the stated tape file. Its
    SHA-256 is checked.
  - `second_edition_replica`: my build of the same-indexed record of the
    stated `base` tape file, with each entry of `fields` (offset, length, hex)
    written over it. Its SHA-256 is checked. A file-level `replica` key
    (ordinal, edition, planned position and EOD, the five layout tuples, the
    record indexes) is cross-checked against the built header and footer, and
    my own frame CRCs and the footer's header hash are compared and reported.

- `read_data_addresses` lists data addresses the Recoverer is asked to
  read. Each block is read and judged as Section 13.4 judges a stripe
  position. A read that succeeds, with a CRC that matches the sidecar index,
  returns the block (`result` `read`). A read failure (a medium error, or a
  record that is not one block long) or a CRC mismatch makes it a failed
  block, which is recovered as a failed address is. Each such outcome has
  `request: "read"` and the `read` it met (GAPS L-1).
- `observations` lists separate decisions on the same damaged tape, each
  with its own `hints` (or `null` for none). The case's entry in the
  decision file then holds `image`, `record_edits` (the checks, per record)
  and `observations`, which maps each observation's id to a decision of the
  usual form. The trace has the same shape.

It writes two files here:

- `decisions.json`, in the `rem-parity-second-implementation-decisions/1`
  schema;
- `decisions-trace.json`, which records what the Reader read and why each
  step ended as it did.

`resume` takes resume-case files. Each names an image and gives a committed
prefix as Section 7.1 entries with its `W` and `T`. A case that reaches step
4 also gives an Object to append (`append_object`). A case may damage the
tape with `tape_faults`: `record_edits`, applied and checked as in `decide`,
and `unreadable_records`, each a tape file, record index and LBA. For each
case, the program acts as a Resumer under Section 14 on that tape, treating
the prefix as the off-tape commit authority. Every key is checked, at every
level. An unknown or missing key fails the run with exit status 2, and so
does a case that reaches step 4 with no `append_object`. It writes
`resume-decisions.json`; see "The resume schema" below.

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

`negative-e1` does the same for the E1 negative case
(`blind-inputs/negatives-e1-blind.json`, a bootstrap-role case), from a
table of its own. It writes `negative-e1-decisions.json` in the same schema.
The case names a `role` where the others name a `target`, so its entry's
`target` is that role.

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
block there. It also covers the E1 negative (`--negatives-e1`, default
`blind-inputs/negatives-e1-blind.json`), whose id is already the case name.
It relates the opaque ids to the real case ids through the two
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
change is applied as `mutations` applies one; the second-edition status takes
my build of the profile's replica at that position with another edition ID and
sequence, GAPS M-9), decides the outcome
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
- the case's hints, or each observation's;
- the case's failed data addresses.

The fault model comes with the cases and is taken as given. An unreadable
record fails every READ that begins at it, positioning is unaffected, and a
removed filemark merges its two tape files. A record edit replaces a
record's bytes before the other faults are applied. A hint of `"the image's"` for
the tape UUID is replaced by the image's UUID. Nothing else from the inputs
reaches the Reader. `bytes_match` and `inventory_equals_true_prefix` are
computed only after every decision in the case is fixed.

The steps follow the text in order:

1. Bootstrap discovery (Section 8.4). Without supplied values, each
   discovery candidate is tried, and no usable bootstrap at any of them is
   `NoBootstrapFound`. With supplied values, which a Scanner that cannot read
   the bootstrap must use, the first record is judged in Section 8.4's two
   stages: the physical read, then the content of a record of the right
   length. A refusal records the value it names (`discovery.refusal_names`).
   A value "that can still be decoded" is read from the payload without
   Section 5.3's canonical-form rules, so a payload whose keys are out of
   order still yields its scheme and `drive_compression` (GAPS J-1).
2. Terminal discovery from EOD (Section 8.4 step 1): the Scanner spaces back
   over up to five filemarks and reads the record before each. Spacing back
   crosses the records between one filemark and the next, so a tape that ends
   in records with no filemark still reaches the last filemark (GAPS M-5). A
   replica footer supplies the planned layout only when it sits at its recorded
   position and plans an EOD at or after the tape's. Then replica validation
   under Section 10.6 and selection under Section 8.5.
3. When no replica validates, the BOT walk and its second pass (Sections
   8.4.1, 12.2 and 12.3), and the validation of the walked map against the
   final ParityMap (Section 13.1). A rung that fails a check, its count
   included, does not recognise a file: the failed classification is
   reported, and the file is an Object candidate. Any tape file that
   follows the exact terminal suffix (five undamaged files: replica,
   separation extent, replica, separation extent, replica) is an artifact, in no inventory, and is listed in `walk.artifacts` (Section 12.6; GAPS M-6). A missing trailing filemark is
   reported as structural damage (Section 12.2). The decision's `walk.map`
   says whether the walk produces a map and whether Section 13.1 validates it.
4. For each failed address, the Recoverer's refusals, index acquisition,
   erasure taxonomy, reconstruction and CRC check (Sections 13.2–13.5). In
   index acquisition (Section 13.3), the footer or an available directory
   entry decides between copies. The tail rescue from the terminal index
   tries the tail copy at `H + P`, with `H = (total − 1 − P) / 2` from the
   sidecar's map entry. It applies only under three conditions:
   - the footer and the primary have both failed;
   - no final ParityMap validates;
   - the map entry comes from a validated replica, not a walked map.
5. The Verifier. Its `result` is the terminal-suffix outcome that Sections
   10.6 and 12.6 define: complete, degraded, recovery required (no valid
   replica), or an error for a conflict. A separate observation,
   `verifier-full`, gives what its full verification reports (below). With no
   planned layout, a separation extent that the walk finds damaged is reported invalid
   (Section 12.3 items 2 and 3). Its `outside_terminal_suffix` lists
   each finding before replica A with the error a Reader reports for that
   component (Section 2.2), after reading every sidecar's two copies and
   footer (Section 9.1). A bootstrap that the supplied values made the
   Scanner treat as unreadable is reported as `TapeIo` for a medium error,
   and as `BootstrapParse`, the parser's name, for damaged content (GAPS J-2).
   The check is a full verification (Section 2.2). It reads every data block
   that a sidecar protects and every parity shard, and checks each against
   its sidecar's index. Each failure gets an `address`: a data block's tape
   file and block, or a parity shard's epoch, stripe and parity index. A
   finding's `checks` says whether it concerns structure and metadata or a
   data block or parity shard. Each sidecar's tail copy is located as
   Section 13.3 locates it.

   The observation `verifier-full` holds:
   - `data_blocks_failed` and `parity_shards_failed`: every data block a
     sidecar protects, and every parity shard, that fails, each by its
     address, with its reason and, for a medium error, `TapeIo`. A read
     failure, a record that is not one block long, and a CRC that differs
     from the index all fail. A shard of an epoch whose index is unavailable
     is located from the map entry's count (Section 9.1) and read; a failed
     read is reported, and the shard cannot be checked against a CRC (GAPS
     M-8);
   - `coverage`: what was read, and which epochs had no index, so that their
     blocks and shards were read but could not be checked. When no validated
     map says which blocks a sidecar protects, or discovery ended in an error,
     `performed` is false and the two lists are null;
   - `other_findings`: every other finding, before the terminal suffix and
     in it (each replica, each separation extent, the planned EOD, an artifact
     after the suffix, structural damage), each with its Section 15 name;
   - `tape_complete`: true when the suffix is complete, the full verification was
     performed and there is no failed block or shard and no finding (Section 2.2);
   - `terminal_suffix.complete`: whether the terminal suffix is complete.
     It is true only with three valid agreeing replicas, two valid separation
     extents and EOD right after C's trailing filemark (Sections 10.6 and
     12.6).

Where the text leaves an outcome open, the decision says `undecided` and
lists the readings. Each decision cites the sentences that decide it, and
the tests check that every quoted sentence occurs in the specification.
Where the text defines an outcome by what a role reads, the decision records
the outcome for each case: a Scanner's degraded flag is `per reads`, with
`degraded_by_reads` giving the flag for a Scanner that reads the damaged
payload and for one that does not (Section 12.6).

A decision that needs a record whose bytes the case states but the file and the
text do not determine is `undecided`, with the readings (the mechanism stays;
no case in the set now needs it, GAPS M-4).

## How the Resumer works

The steps follow Section 14:

1. W and T are derived from the prefix (Section 7.2) and compared with
   the values given. The append point is `Σ(block_count + 1)` over the
   prefix.
2. The step-2 rules that need no scheme are checked first, among them the
   refusal of a prefix that records a final ParityMap or a terminal
   component. Then the bootstrap at LBA 0 is read for `S` and `k`, and the
   bound `T − W < S × k` is checked. The text does not say where a Resumer
   obtains the scheme (`GAPS.md` entry E-2).
3. The open epoch `[W, T)` is re-read from the tape. A filemark, EOD or a
   record of the wrong size there contradicts the commit record and is
   `ResumeAppend`; a medium error is `TapeIo`.
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
| `tape_faults` | Present only when the case damages the tape: the record-edit checks and the unreadable LBAs |
| `step4` | `result` (`run` or `not_run`), `append_point_lba`, `citations` |
| `decision` | `result` (`accepted` or `refused`), `error` (a Section 15 name, or `undecided`), `refused_at`, `before_any_tape_read` (`true`, `false` or `undecided`), `before_any_write`, `records_read` (LBAs, in order), `citations` |
| `append` | `null` for a refusal. Otherwise: `object_tape_file`, `object_first_lba`, `object_blocks`, `object_first_ordinal`, `object_row` (Section 10.3 keys as strings, byte strings as hex), `sidecars` (each with `tape_file`, `first_lba`, `epoch_id`, the protected range, `total_blocks` and `closed_by`), `tape_files` (every tape file of the resulting tape and `ALL`, in the `tape-images/MANIFEST.tsv` convention), `eod`, `uninterrupted_equal`, `uninterrupted_differences` and the `session_end_citations` |
| `undecided` | Aspects the text leaves open, each with its readings and citations |

## The negatives schema

`negative-decisions.json` has `"schema": "rem-parity-second-implementation-negatives/1"`
and an `entries` map keyed `neg-NN`, or `neg-NN/<variant>` for a case with
variants. Every entry has the same keys, and a bootstrap entry whose Section
15 name depends on the level at which a Reader meets the block adds
`decision.by_level`:

| Key | Contents |
| --- | --- |
| `id`, `variant`, `target` | The case, its variant, and the target as the case names it |
| `apply` | `resolved` (`true`, `false`, or `null` when there is no tape vector), `vector` (`bytes`, an injected commit record, or `none`), `checks` (each stated `from` value against the byte found), `repairs` applied, `mutated_blocks` (size and SHA-256 of each changed block after repair), `notes` |
| `decision` | The text decision. `outcome` is `rejected`, `accepted`, `undecided`, `no-vector` or `unresolved`. `error` is a Section 15 name, `undecided`, or `null` where no name applies. `error_set` holds the names where Section 15 permits a set. `readings` are given where the text leaves the outcome or the name open. `rules` are the quoted sentences that fail. `order` says which rule fires first, or that the text fixes no order. `reader_must_reject` is `true`, `false` or `undecided`. `formula` gives a named formula evaluated with the case's inputs, with its type and whether it overflows. `note` gives the consequence at the level of the whole tape. |
| `implementation` | What my implementation reported for each role run: a copy or footer validator, the Recoverer, a Verifier reading both copies, the bootstrap parser, bootstrap discovery (without and with supplied values), ParityMap validation, terminal selection with separation checks, or the Resumer |
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
| `decision.tape` | `outcome` (`inventory`, `TerminalIndexReplicaConflict`, `BotStructuralRecoveryRequired` or `undecided`), `acceptable_selections`, `degraded` (`true`, `false`, `per reads` with `degraded_by_reads`, or `undecided`), each replica's status, the quoted `rules` and the reasoning |
| `decision.verifier` | What a Verifier must report: `result` (`complete`, `degraded`, `not complete`, or `recovery_required` when no replica validates) and each separation extent's status |
| `decision.undecided` | Aspects the text leaves open, each with its readings and citations |
| `implementation` | What my Scanner and Verifier reported: the layout source, each replica's and extent's validity, error and reason, and the selection; when no footer supplies a layout, the walk's classification of the terminal tape files |
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
| `DECISION-LOG.md` | Every decision changed after the blind decisions were written, with the sentence that decides it |
| `resume-decisions.json` | The Resumer's decisions on the resume cases |
| `negative-decisions.json` | The decisions on the generation-2 negative cases |
| `negative-e1-decisions.json` | The decision on the E1 negative case (e1-16) |
| `negative-supplement-decisions.json` | The decisions and isolation audits for the supplemental single-rule variants |
| `negative-block-digests.json` | Every block the negatives change, compared with `tape-images/negatives/MANIFEST.tsv` |
| `mutation-decisions.json` | The decisions on the 50 terminal-index mutations |
| `selection-decisions.json` | The decisions on the 14 terminal survivor sets |
| `blind-inputs/mutations-blind.json`, `blind-inputs/selection-blind.json` | The mutation and survivor-set descriptions this implementation decided from, exactly as it received them |
| `blind-inputs/negatives-e1-blind.json` | The E1 negative case's construction, as received |
