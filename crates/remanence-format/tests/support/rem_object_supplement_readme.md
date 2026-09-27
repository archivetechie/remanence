# REM-OBJECT supplement 1 candidate vectors

This set adds two negative cases and one Reader control to the REM-OBJECT and
REM-ENCRYPT vectors. Its status is review-only. These cases “become conformance
vectors only when the revision is frozen and publishes its own archive” and
“MUST NOT be copied into or substituted for publication artifacts before
independent review and freeze”, as the release record states in `specs/README.md`.

At the freeze of the revision that adds it, and not before, this set becomes a
separate supplement archive with its own manifest, checksums, verifier, name, DOI
and digest. The pinned archive is never changed or re-cut. The candidate set's
identity is `MANIFEST.tsv` plus the commit that holds it. No file records a digest
of the whole set. The manifest lists each other file by path, byte count and
SHA-256, using tab-separated records as in the REM-PARITY candidate set.

The Sealer case supplies distinct slots with one shared recipient epoch id and
expects `InvalidInput`. The metadata pair has all four required entries and an
unknown unsigned key 4. Its value is -1 in the negative case and 0 in the control.
Every other construction input is identical. Authenticated bytes derived from
that value, including the salt, necessarily differ. The negative case expects
`InvalidCborEncoding`; the control must recover the recorded plaintext digest.
Both envelopes use the existing deliberately defective Sealer builder because
REM-ENCRYPT 5.6 requires Sealers to emit exactly the four defined entries.

All secrets here are public deterministic test inputs. Each `input.json` records
P1 as the plaintext source, framing inputs, the DEK, HPKE RNG seed, both complete
recipients and the metadata plaintext. The HPKE seed keys a ChaCha20 stream with
an all-zero 12-byte nonce and initial counter zero; each slot consumes 64 bytes
for X-Wing encapsulation in slot order. The Sealer rejection records these inputs
too, although rejection precedes their use by encryption. Entropy failure has no
portable input and is exercised by each implementation's own tests, not by a
vector. Slot-order behaviour remains outside this supplement.

Run both executors from the repository root:

```sh
cargo test -p remanence-format --test rem_object_negative_vectors
python3 tools/verify_rem_object_vectors_independent.py --supplement fixtures/rem-object-supplement-draft
```

The independent executor requires the dependencies in
`tools/requirements-rem-object-independent.txt`. It checks the complete manifest,
re-derives both objects without Rust, opens them with each recipient, and checks
the duplicate-epoch inputs. The Rust executor calls the public Sealer for that
rejection, and compares regenerated artifacts with every checked-in file.
To export a review copy, set `REM_OBJECT_SUPPLEMENT_EXPORT_DIR` to an empty
scratch directory and run the same Rust test. Export never updates publication
artifacts. The generator uses this README's source template in
`crates/remanence-format/tests/support/` and the pointer script below.

The following erratum table is produced by
`python3 tools/rem_object_vector_pointers.py`, which reads the pinned tar directly.
The source column distinguishes repeated case ids. Index entries inherit the
index's recorded pointer. Current owners refer to `specs/in-progress/`; ownership
does not imply that the current prose describes every case. D1 names both its
plaintext and encrypted halves. Range cases belong to the D1 encrypted-copy section.
The positive unknown-manifest-key control belongs to REM-OBJECT 13.1.
`python3 -m unittest discover -s tools -p test_rem_object_vector_pointers.py`
checks this table against the script.

<!-- vector-pointers:start -->
