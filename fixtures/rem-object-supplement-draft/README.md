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
| Source | Case | Recorded `spec_section` | Current owner |
| --- | --- | --- | --- |
| vectors.json | REM-OBJECT-TV-ATTRIBUTE-EXT-COMBINED | 13.1 and 13.6 | REM-OBJECT 13.1 |
| vectors.json | REM-OBJECT-TV-EXT-MEMBER | 13.1 and 13.6 | REM-OBJECT 13.1 |
| vectors.json | REM-OBJECT-TV-NONUSER-ATTRIBUTE | 13.1 and 13.6 | REM-OBJECT 13.1 |
| vectors.json | REM-OBJECT-TV-PORTABLE-CORE-ONLY | 13.1 and 13.6 | REM-OBJECT 13.1 |
| vectors.json | ext-member-noncanonical-cbor | 13.1 and 13.6 | REM-OBJECT 13.6 |
| vectors.json | ext-value-not-map | 13.1 and 13.6 | REM-OBJECT 13.6 |
| vectors.json | inventory-disagrees-with-entries | 13.1 and 13.6 | REM-OBJECT 13.6 |
| vectors.json | key-frame-len-above-maximum | 13.1 and 13.6 | REM-ENCRYPT 13.4 |
| vectors.json | key-frame-len-below-minimum | 13.1 and 13.6 | REM-ENCRYPT 13.4 |
| vectors.json | manifest-tamper-altered-first-chunk-lba | 13.1 and 13.6 | REM-OBJECT 13.6 |
| vectors.json | manifest-tamper-repointed-path | 13.1 and 13.6 | REM-OBJECT 13.6 |
| vectors.json | manifest-tamper-swapped-file-sha256 | 13.1 and 13.6 | REM-OBJECT 13.6 |
| vectors.json | reserved-wrap-suite-01 | 13.1 and 13.6 | REM-ENCRYPT 13.4 |
| vectors.json | encrypted-last-object-chunk | 13.1 and 13.6 | REM-ENCRYPT 13.2 |
| vectors.json | encrypted-last-object-chunk-wrong-finality | 13.1 and 13.6 | REM-ENCRYPT 13.2 |
| negative-envelope.json | wrong-magic | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | header-len-not-128 | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | unsupported-format-version | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | unknown-suite-id | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | chunk-size-zero | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | chunk-size-not-multiple-of-512 | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | flags-nonzero | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | reserved-bytes-nonzero | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | all-zero-hkdf-salt | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | object-id-all-nul | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | object-id-interior-nul | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | object-id-non-utf8 | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-frame-len-16 | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-frame-len-over-max | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | salt-bit-flipped | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | ciphertext-bit-flipped | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | payload-chunks-transposed | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | payload-final-flag-wrong | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | payload-extra-chunk-appended | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | sealed-metadata-wrong-plaintext-digest | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | sealed-under-non-derived-salt | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-float | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-top-level-array | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-key-text | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-tag | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-indefinite-length | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-simple-undefined | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-duplicate-key | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-non-shortest-integer | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-missing-plaintext-size | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-missing-version | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-missing-digest-alg | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-missing-digest | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-version-text | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-version-2 | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-plaintext-size-text | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-plaintext-size-not-multiple | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-plaintext-size-zero | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-plaintext-size-overflow | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-digest-alg-not-sha256 | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-digest-alg-bytes | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-digest-short | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-digest-text | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | metadata-trailing-byte | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | eof-inside-metadata-frame | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | eof-mid-chunk | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | payload-absent-after-metadata | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | footer-bytes-wrong | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | fill-byte-nonzero | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | trailing-byte | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | seal-plaintext-size-not-multiple | 13.6 | REM-ENCRYPT 13.4 |
| negative-envelope.json | seal-object-id-too-long | 13.6 | REM-ENCRYPT 13.4 |
| negative-inner.json | inner-object-id-differs | 13.6 | REM-ENCRYPT 13.4 |
| negative-inner.json | inner-chunk-size-differs | 13.6 | REM-ENCRYPT 13.4 |
| negative-inner.json | inner-encryption-not-none | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | version-flip | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | suite-flip | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | reserved-wrap-suite-01 | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | truncated-key-frame | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | duplicate-slots | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | misordered-slots | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | key-frame-trailing-byte | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | oversize-key-frame | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | key-frame-label-tamper | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | key-frame-enc-tamper | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | key-frame-ciphertext-tamper | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | key-frame-slot-inserted | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | key-frame-slot-removed | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | slot-count-zero | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | slot-count-nine | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | writer-zero-slots | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | writer-one-slot | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | writer-nine-slots | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | reader-one-slot | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | wrap-suite-zero-nonempty | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | hpke-zero-key-frame-len | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | hpke-undersized-key-frame-len | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | duplicate-recipient-epoch-id | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | internal-slot-truncation | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | nonzero-reserved-key-region | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | malformed-key-frame-magic | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | wrong-recipient-private-key | 13.6 | REM-ENCRYPT 13.4 |
| negative-key-frame.json | malformed-encapsulation | 13.6 | REM-ENCRYPT 13.4 |
| negative-manifest.json | non-canonical-key-order | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | non-shortest-integer | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | indefinite-length-item | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | float-item | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | tag-item | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | duplicate-map-key | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | schema-version-2 | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | file-sha256-wrong-length | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | nesting-depth-over-max | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | manifest-digest-mismatch | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | manifest-chunk-size-mismatch | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | duplicate-entry-path | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | duplicate-entry-file-id | 13.6 | REM-OBJECT 13.6 |
| negative-manifest.json | unknown-extra-key-accepted | 13.6 | REM-OBJECT 13.1 |
| negative-plaintext-reader.json | wrong-format-id | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | schema-major-2 | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | missing-compression | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | compression-gzip | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | encryption-aes-256-gcm | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | chunk-size-mismatch | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | corrupted-header-checksum | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | single-zero-eof-record | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | unknown-typeflag | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | misaligned-nonzero-payload | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | traversal-shaped-effective-path | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | entry-after-manifest | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | flipped-payload-bit | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | truncated-payload | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | truncated-pax-body | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | pax-record-length-out-of-bounds | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | pax-record-missing-equals | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | pax-record-missing-trailing-newline | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | pax-value-control-character | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | pax-value-non-utf8 | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | hardlink-missing-target | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | hardlink-forward-target | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | hardlink-nonregular-target | 13.6 | REM-OBJECT 13.6 |
| negative-plaintext-reader.json | missing-manifest | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | duplicate-path | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | duplicate-file-id | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | manifest-file-id-collision | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | reserved-remanence-path | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | control-character-path | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | absolute-path | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | parent-component-path | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | dot-component-path | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | empty-component-path | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | trailing-slash-file-path | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | malformed-mtime | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | streamed-wrong-hash | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | streamed-wrong-size | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | chunk-size-not-multiple-of-512 | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | symlink-nonzero-size | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | directory-nonzero-size | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | symlink-missing-target | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | hardlink-missing-target | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | hardlink-nonregular-target | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | hardlink-forward-target | 13.6 | REM-OBJECT 13.6 |
| negative-writer.json | directory-missing-trailing-slash | 13.6 | REM-OBJECT 13.6 |
| rem-object-tv-attribute-ext-combined.json | REM-OBJECT-TV-ATTRIBUTE-EXT-COMBINED | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-boundary.json | REM-OBJECT-TV-BOUNDARY | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-d1.json | REM-OBJECT-TV-D1 | 13.4 | REM-OBJECT 13.4; REM-ENCRYPT 13.2 |
| rem-object-tv-e2.json | REM-OBJECT-TV-E2 | 13.3 | REM-ENCRYPT 13.1 |
| rem-object-tv-empty-file.json | REM-OBJECT-TV-EMPTY-FILE | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-empty.json | REM-OBJECT-TV-EMPTY | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-ext-member.json | REM-OBJECT-TV-EXT-MEMBER | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-hardlinks.json | REM-OBJECT-TV-HARDLINKS | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-manifest.json | REM-OBJECT-TV-MANIFEST | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-metadata.json | REM-OBJECT-TV-METADATA | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-nonregular.json | REM-OBJECT-TV-NONREGULAR | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-nonuser-attribute.json | REM-OBJECT-TV-NONUSER-ATTRIBUTE | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-one-byte.json | REM-OBJECT-TV-ONE-BYTE | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-order.json | REM-OBJECT-TV-ORDER | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-p1.json | REM-OBJECT-TV-P1 | 13.2 | REM-OBJECT 13.2 |
| rem-object-tv-paths.json | REM-OBJECT-TV-PATHS | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-portable-core-only.json | REM-OBJECT-TV-PORTABLE-CORE-ONLY | 13.1 | REM-OBJECT 13.1 |
| rem-object-tv-xattrs.json | REM-OBJECT-TV-XATTRS | 13.1 | REM-OBJECT 13.5 |
<!-- vector-pointers:end -->
