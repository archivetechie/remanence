# The versioning register

This page is for a reviewer or an implementer of the Remanence formats. It lists every value
that carries a version, every identifier that acts as one, and every frozen constant that only
a new major version could change. [How the formats change](versioning-explained.md) explains
the policy behind these entries. This page is informative; the specification section named in
each entry is normative.

Each entry gives the same fields:

| Field | Meaning |
| --- | --- |
| Location | Where the value is stored, and the specification section that defines it. |
| Current value | The value current software writes. |
| Unknown value | What a reader does with a value it does not know. |
| How it changes | The kind of revision that may assign a new value. |
| Code | The constant in the reference implementation that holds the value. |

The values and section numbers are those of the preparing copies under `specs/in-progress/`,
which current software implements. For REM-OBJECT and REM-ENCRYPT the preparing copies changed
no stored byte, so their values are also those of the published copies. For REM-PARITY the
published copy, 1.0.0-draft.2, describes an earlier tape layout; entries 1, 2 and 9 say where
it differs.

The register covers the formats and the published documents and artifacts. The host's own
records (the tape log, the audit log and the catalog schema) are versioned by the software,
not by the specifications, and are described in the
[on-tape layout reference](reference-tape-layout.md#on-disk-durable-records-and-rebuildable-state).

<!-- code-anchor: crates/remanence-parity/src/bootstrap.rs crates/remanence-parity/src/sidecar.rs crates/remanence-parity/src/parity_map.rs crates/remanence-parity/src/index_separation.rs crates/remanence-parity/src/tape_index_replica.rs crates/remanence-parity/src/lib.rs @ 311cc6c3 -->
## Tape layer (REM-PARITY)

### 1. Bootstrap `schema_major`

The generation number of the tape layout, held in the bootstrap, the single block that
describes the tape at the beginning of tape (BOT).

| Field | Value |
| --- | --- |
| Location | Bootstrap fixed frame, offset `0x08`, 2 bytes, big-endian (REM-PARITY §8.1) |
| Current value | `2` |
| Unknown value | Refused. A reader rejects every value other than `2`. |
| How it changes | Only a new major version of REM-PARITY, which is a separate document, assigns another value. |
| Code | `BOOTSTRAP_SCHEMA_MAJOR` |

Generation 1 wrote further copies of the bootstrap along the tape, the last of them carrying
the tape's inventory. Generation 2 has one bootstrap at BOT and keeps the inventory in three
terminal replicas at the end of the tape (entry 4). Current software writes and reads only
generation 2, and it rejects generation-1 tapes, among them the tapes written by Remanence
0.1.0. The published REM-PARITY copy, 1.0.0-draft.2, describes generation 1. The preparing
copy, 1.0.0-draft.5, describes generation 2.

### 2. Bootstrap `schema_minor`

A feature-generation number for the bootstrap's contents. It is not a revision counter and not
the document's version number.

| Field | Value |
| --- | --- |
| Location | Bootstrap fixed frame, offset `0x0A`, 2 bytes, big-endian (REM-PARITY §8.1); registry in REM-PARITY §8.1.1 |
| Current value | `3` |
| Unknown value | Accepted. A reader accepts any value; a future payload rule may depend on it. |
| How it changes | A minor revision of REM-PARITY that changes what the bootstrap can carry assigns the next value and names itself in the registry. |
| Code | `BOOTSTRAP_SCHEMA_MINOR` |

The registry for `schema_major` 2 lists `0` to `2` as never assigned, `3` as the current value,
and `4` and above as unassigned. A reader can accept any value because unknown payload keys are
ignored by rule (REM-PARITY §5.3), so a newer bootstrap of the same major generation stays
readable. The registry for `schema_major` 1 is in the published copy, 1.0.0-draft.2. The
published vector archive contains generation-1 images at minor values `2` and `3`; they are not
generation-2 vectors, and a generation-2 reader rejects them.

### 3. Bootstrap `flags`

A field of 32 single-bit switches. Bit 0, the lowest-order bit, marks a tape written
deliberately without parity; when it is clear, the tape has parity.

| Field | Value |
| --- | --- |
| Location | Bootstrap fixed frame, offset `0x0C`, 4 bytes, big-endian (REM-PARITY §8.1); the constant is in REM-PARITY §2.5 |
| Current value | Bit 0 assigned. A writer sets every other bit to zero. |
| Unknown value | Ignored. A reader ignores bits it does not recognise. |
| How it changes | A new flag may carry only a meaning that an older reader can safely ignore. A flag whose meaning an older reader must not ignore needs a new `schema_major`. |
| Code | `FLAG_NO_PARITY` |

### 4. Object recovery rows: the integer keys

Each of the three terminal replicas, written at the end of the tape by finalization, holds one
fixed 256-byte slot for every object on the tape. Each slot carries a small map whose fields
are named by integers, such as 1 for the tape file number and 4 for the object identity. The
set of assigned keys is a vocabulary that can grow.

| Field | Value |
| --- | --- |
| Location | Terminal replica payload (REM-PARITY §10.3) |
| Current value | Keys 1 to 4 are assigned and required in every row. Keys 10 to 13 are assigned for plaintext objects only, and keys 21 to 23 for encrypted objects only. |
| Unknown value | Ignored. A reader ignores integer keys it does not know (REM-PARITY §5.3). |
| How it changes | A minor revision may assign new keys. A new key may not change the meaning of an existing key, or the recovery outcome of a tape written without it; a change that would do either needs a new major version. |
| Code | `crates/remanence-parity/src/tape_index_replica.rs` |

The gaps between the key groups are left unassigned so that a later revision can place a new
field beside related ones. The bootstrap payload is a separate integer-keyed map (REM-PARITY
§8.2). In it, keys 20, 21 and 30 are reserved and must be absent; they held generation-1
structures, and a generation-2 bootstrap that carries any of them is refused as a parse error.

### 5. Sidecar `schema_version` and `footer_version`

A sidecar is the tape file that holds parity data and recovery metadata for one parity epoch.
Its header carries a `schema_version` and its footer a `footer_version`.

| Field | Value |
| --- | --- |
| Location | Header offset `0x2C`, 4 bytes (REM-PARITY §9.2); footer offset `0x08`, 2 bytes (REM-PARITY §9.6); both little-endian |
| Current value | `2` and `2` |
| Unknown value | Refused. A reader rejects any other value. |
| How it changes | A new value is a change that earlier readers refuse, and is permitted only under the change policy's conditions. |
| Code | `SIDECAR_SCHEMA_VERSION`, `SIDECAR_FOOTER_VERSION` |

The sidecar is recovery machinery, so a reader refuses an unknown layout instead of guessing
at it; a wrong guess could damage a recovery. This differs from the bootstrap's
`schema_minor`, which a reader accepts. The header's `copy_generation` field is reserved and
must be `0` while `schema_version` is `2`.

### 6. ParityMap and terminal frame versions

The final ParityMap is the tape file before the terminal replicas that lists the tape's parity
sidecars. It, each terminal replica, and each separation extent between the replicas carry a
version in their header and footer frames.

| Field | Value |
| --- | --- |
| Location | Offset `0x08` of each frame, 2 bytes, little-endian (REM-PARITY §10.1.3, §10.4, §10.5) |
| Current value | `2` for the ParityMap header and footer, whose format identifier is `rem-parity-map-v1`; `1` for terminal replica and separation frames |
| Unknown value | Refused. A reader that guessed at a terminal layout could return a wrong inventory for a tape with no catalog. |
| How it changes | A new frame schema needs a compatible extension under the change policy, or a new major version. |
| Code | `PARITY_MAP_SCHEMA_VERSION`, `PARITY_MAP_FOOTER_VERSION`, `TAPE_INDEX_REPLICA_SCHEMA_VERSION`, `INDEX_SEPARATION_SCHEMA_VERSION` |

### 7. Magic labels and their version byte

Every REM-PARITY structure is recognised by its magic bytes, and each magic is built from an
ASCII label that ends in a version byte, `\x01`. The terminal replica and separation labels add
a role letter after it: `H` for a header, `F` for a footer.

| Field | Value |
| --- | --- |
| Location | REM-PARITY §2.5 (the labels) and §5.2 (how the magics are derived) |
| Current value | Version byte `\x01` in every label. The bootstrap magic is the fixed string `REM\0BOO\x01`. The other labels are `REM\0PAR\x01` and `REM\0PARFOOT\x01` (sidecar header and footer), `REM\0PMAP\x01` (ParityMap header and footer), `REM\0TIREP\x01H` and `REM\0TIREP\x01F` (terminal replica), and `REM\0TISEP\x01H` and `REM\0TISEP\x01F` (separation extent). |
| Unknown value | A block whose magic does not match is not that structure. |
| How it changes | A new magic, for example with version byte `\x02`, is one of the defining changes of a new major version. |
| Code | `BOOTSTRAP_MAGIC` and the `*_MAGIC_MESSAGE` constants in the parity crate |

Only the bootstrap stores its label on tape, because a reader does not yet know the tape UUID
when it looks for the bootstrap. Every other magic is the first 8 bytes of
HMAC-SHA-256 of its label, keyed by the tape UUID, so these labels never appear on tape and a
structure from one tape is not recognised on another. The derived magics identify a block's
tape and role; they do not authenticate it (REM-PARITY §16.1).

### 8. Erasure scheme `rs-cauchy-gf256-v1`

The name of the parity mathematics: Reed-Solomon over GF(2⁸) with a Cauchy matrix, as
REM-PARITY specifies it.

| Field | Value |
| --- | --- |
| Location | The scheme record in the bootstrap payload (REM-PARITY §8.2); the scheme is defined in REM-PARITY §6 |
| Current value | `rs-cauchy-gf256-v1`, frozen for the life of the 1.x line |
| Unknown value | Refused. A parity bootstrap must name `rs-cauchy-gf256-v1`; any other identifier makes the bootstrap invalid (`BootstrapParse`), so no recovery is attempted with the wrong mathematics. |
| How it changes | A different scheme takes a new identifier, introduced only by a revision under the change policy. The meaning of `rs-cauchy-gf256-v1` never changes. |
| Code | `SCHEME_ID_RS_CAUCHY_GF256_V1` |

### 9. Discovery block-size candidates

A tape is written in fixed-size blocks. A reader that finds a tape without its catalog learns
the block size by reading the bootstrap at each candidate size in turn, so the set of
candidates is itself a versioned surface.

| Field | Value |
| --- | --- |
| Location | REM-PARITY §8.4; the set is also listed in REM-PARITY §2.5 |
| Current value | 256 KiB, 512 KiB and 1 MiB |
| Unknown value | No refusal is possible. A reader never tries a size outside the set, so a tape written at another size looks to it like a tape with no bootstrap, which cannot be told apart from a blank or destroyed tape. |
| How it changes | It does not change within `schema_major` 2. The set is frozen for the life of the major version, and extending it is a new-major change. |
| Code | `DEFAULT_BOOTSTRAP_CANDIDATE_BLOCK_SIZES` |

The published copy, 1.0.0-draft.2, freezes its own set for the life of `schema_major` 1. This
entry is the worked example of the change policy's second question; see
[How the formats change](versioning-explained.md#minor-revisions-adding-with-care).

<!-- code-anchor: crates/remanence-format/src/model.rs crates/remanence-format/src/manifest.rs @ 311cc6c3 -->
## Object layer (REM-OBJECT)

### 10. Stream format identifier `rem-object-v1`

The name of the object format, recorded in every object.

| Field | Value |
| --- | --- |
| Location | The `REMANENCE.format_id` keyword in the global pax header that begins every object (REM-OBJECT §4.5.1, §10) |
| Current value | `rem-object-v1`, a frozen constant |
| Unknown value | Refused with `UnsupportedFeature` (REM-OBJECT §4.5.2). A reader that meets another identifier knows that it holds a different format. |
| How it changes | A new major version of REM-OBJECT is a new identifier, `rem-object-v2`. Any change to the meaning of an existing rule requires one. |
| Code | `FORMAT_ID` |

### 11. Stream schema version

`REMANENCE.schema_version`, a text value that says which optional capability an object uses.
It is not the document's version.

| Field | Value |
| --- | --- |
| Location | The `REMANENCE.schema_version` keyword in the global pax header (REM-OBJECT §10) |
| Current value | `1.0` when no extended attributes are preserved, `1.1` when any are. Document 1.0 defines both. |
| Unknown value | A reader checks only the major number, `1`, and refuses any other major with `UnsupportedFeature`. A minor value it does not know is read through, and unknown pax keywords are ignored. |
| How it changes | A minor revision may define a new value for a new optional capability, under the change policy's conditions. |
| Code | `SCHEMA_VERSION`, `SCHEMA_VERSION_XATTRS` |

The specification warns against reading this as a document version: a future revision 1.1 of
REM-OBJECT would not imply a stream schema `1.1`, nor the reverse.

### 12. Manifest schema version

The manifest is the object's own inventory of its files, with their sizes, checksums and
positions. It carries an integer schema version.

| Field | Value |
| --- | --- |
| Location | The manifest's CBOR `schema_version` key (REM-OBJECT §4.7, §10) |
| Current value | `1`, defined by document 1.0 |
| Unknown value | Refused. A manifest whose `schema_version` is not `1` is invalid (`ManifestInvalid`). Unknown manifest keys are ignored. |
| How it changes | The manifest grows through new keys, which older readers ignore, in a minor revision. A change to the meaning of existing fields requires a new stream format identifier (entry 10). |
| Code | `crates/remanence-format/src/manifest.rs` |

### 13. Pax keywords `REMANENCE.*`

An object is a constrained tar stream, and the format's own metadata travels in named pax
keywords such as `REMANENCE.file_sha256` and `REMANENCE.object_id`. The list of keywords is a
vocabulary that can grow, like the integer keys of entry 4.

| Field | Value |
| --- | --- |
| Location | REM-OBJECT §4.4, with the extension rule in REM-OBJECT §4.4.3 and §10 |
| Current value | The keywords REM-OBJECT 1.0 defines, among them the eight in every object's global header (REM-OBJECT §4.5.1) |
| Unknown value | Ignored. A reader ignores unknown keywords, and a rewrite that preserves an object must carry them through unchanged. |
| How it changes | A minor revision may assign new keywords. No keyword may change payload framing, an existing meaning or any enforced rule; that needs a new format identifier. |
| Code | `crates/remanence-format/src/model.rs` and the pax writer and reader in the same crate |

<!-- code-anchor: crates/remanence-aead/src/header.rs crates/remanence-aead/src/wrap.rs crates/remanence-aead/src/error.rs @ 311cc6c3 -->
## Encryption layer (REM-ENCRYPT)

### 14. Envelope `format_version`

The single byte that names the generation of the encrypted envelope.

| Field | Value |
| --- | --- |
| Location | Scalar header, offset `0x06`; registry in REM-ENCRYPT §10.1 |
| Current value | `2`. Value `1` is permanently forbidden: it was assigned before production, never shipped, and is never accepted or reused. |
| Unknown value | Refused with `UnsupportedFormatVersion`. Cryptography cannot be read through. |
| How it changes | A new envelope generation takes a new value in a new major version, which is a separate document. Envelopes of version `2` stay openable under the existing document. |
| Code | `REM_OBJECT_FORMAT_VERSION` |

### 15. Cipher suite `suite_id`

The registered combination of key-derivation and encryption algorithms used inside the
envelope.

| Field | Value |
| --- | --- |
| Location | Scalar header, offset `0x07`; registry in REM-ENCRYPT §10.2, with a column naming the defining revision |
| Current value | `0x01`: HKDF-SHA-256 with ChaCha20-Poly1305, defined by REM-ENCRYPT 1.0 |
| Unknown value | Refused with `InvalidSuite`, which names the number, so the operator knows which capability to obtain. |
| How it changes | A minor revision assigns the next value and its registry row. Because older readers cannot open the new objects, that revision must say so prominently. |
| Code | `SUITE_ID_HKDF_SHA256_CHACHA20POLY1305` |

Superseded suites stay valid for opening (REM-ENCRYPT §10.4). The REM Implementation and
Operations Guide recommends sealing new objects with the current suites only.

### 16. Key-wrapping scheme `wrap_suite`

The registered method by which each object's content key is wrapped for each recipient. It
holds the post-quantum part of the construction.

| Field | Value |
| --- | --- |
| Location | Scalar header, offset `0x38`; registry in REM-ENCRYPT §10.3 |
| Current value | `0x02`: the X-Wing hybrid key-encapsulation mechanism of `draft-connolly-cfrg-xwing-kem-10`. `0x00` is reserved and invalid. `0x01` (X25519 only) is permanently forbidden; it was assigned before production and never shipped. `0x03` is reserved for a later X-Wing construction that differs in its bytes. |
| Unknown value | Refused with `InvalidWrapSuite`. |
| How it changes | As for `suite_id`, through a registry assignment in a published revision. |
| Code | `REM_OBJECT_WRAP_SUITE_XWING` |

One case is decided in advance. If the final X-Wing RFC is byte-for-byte identical to the
pinned draft, `0x02` is kept and only the citation changes, by erratum. If the RFC differs in
its bytes, it takes the reserved `0x03` through a minor revision (REM-ENCRYPT §10.3).

### 17. Frozen cryptographic constants

Values that are part of a suite's identity: the HPKE (Hybrid Public Key Encryption, RFC 9180)
KEM identifier `0x647a`, frozen for suite `0x02`; the envelope magic `REMO` and the key-frame
magic `REMK`; and the derivation labels `rem-encrypt-wrap-v1`, `rem-encrypt-salt-v1`,
`rem-encrypt-object-v1`, `rem-encrypt-metadata-v1` and `rem-encrypt-payload-v1`.

| Field | Value |
| --- | --- |
| Location | REM-ENCRYPT §2.5 (the constants), §5.3.1 (the frozen X-Wing construction) and §15 |
| Current value | As listed above |
| Unknown value | A block without the `REMO` magic is not an encrypted envelope. |
| How it changes | It does not. Each constant is frozen for the suite or generation that defines it, and a different value belongs to a different registered suite or a new major version. The `-v1` suffixes let successors be named without ambiguity. |
| Code | `XWING_HPKE_KEM_ID` and the constants in `crates/remanence-aead/src/header.rs` |

## Documents and published artifacts

### 18. The specification documents

Three normative specifications and two informative companions, each versioned on its own.

| Document | Published copy | Preparing copy | Remarks |
| --- | --- | --- | --- |
| REM-OBJECT Core Format | 1.0.0-draft.3 | 1.0.0-draft.4 | Review draft. The preparing copy moved rules on tool behaviour to the Implementation Guide and changed no stored byte. |
| REM-ENCRYPT | 1.0.0-draft.3 | 1.0.0-draft.4 | Review draft. The same scope move as REM-OBJECT, with no stored byte changed. |
| REM-PARITY | 1.0.0-draft.2 | 1.0.0-draft.5 | Review draft. The published copy describes generation 1; the preparing copy describes generation 2, which current software writes (entry 1). |
| The formats, explained (`formats-explained.md`) | 1.1 | none | Informative companion. It does not freeze. |
| REM Implementation and Operations Guide | 2026-10-01 | none | Informative guide, versioned by date. |

The three specifications are review drafts. Comments close on 30 April 2027, and the documents
freeze on 31 July 2027. They started together at 1.0.0-draft.1, and their draft numbers have
since diverged because review changed some documents more than others. The preparing copies
under `specs/in-progress/` are not normative; [specs/in-progress/README.md](../specs/in-progress/README.md)
says what each one changes.

Each document has a reserved concept DOI (Digital Object Identifier) in its status table, and
the policy is that each published revision is deposited under that concept DOI with a DOI of
its own. No revision has been deposited yet. Titles carry the version line, 1.0, and file names
carry only the major number. Copies of the specifications were also distributed inside the
source archive of the withdrawn software release v1.0.0; like every copy outside a deposit,
they are convenience copies.

The change policy is in the "Status of This Document" section of REM-OBJECT and REM-ENCRYPT,
and in the
[Versioning and Change Policy](../specs/in-progress/rem-parity-1-specification.md#versioning-and-change-policy)
section of the REM-PARITY preparing copy. Each document records every change and its kind in a
revision history: an appendix in REM-OBJECT and REM-ENCRYPT, and a
[separate record](../specs/in-progress/records/rem-parity-1-revision-history.md) for the
REM-PARITY preparing copy.

### 19. The conformance vector archives

The published objects, tape images, damage cases and expected results that anchor
conformance, identified by their SHA-256 digest.

| Field | Value |
| --- | --- |
| Location | `specs/publication/remanence-test-vectors.tar`; REM-OBJECT §13 and REM-ENCRYPT §13 name it |
| Current value | One archive, SHA-256 `77be73e780e9ff2c265c8357b6ba684b4c69800213820ae1331850f742b1d83d`. It contains the REM-OBJECT and REM-ENCRYPT vectors, and REM-PARITY images of generation 1 (`schema_major` 1, at `schema_minor` `2` and `3`). It is the only file of the published release v0.1.0. |
| How it changes | It does not. An archive is never changed, replaced or re-cut. A revision that needs new vectors publishes an additional archive with its own name, DOI and digest, and cites it. |

Vectors for a revision in preparation are first prepared as review-only candidates, kept apart
from the published archive: the generation-2 tape images of REM-PARITY 1.0.0-draft.5 under
`fixtures/rem-parity-terminal-index-draft/`, and additional REM-ENCRYPT 1.0.0-draft.4 cases
under `fixtures/rem-object-supplement-draft/`. They become a supplement archive of that
revision when it is frozen.

### 20. The software

The reference implementation, Remanence, has a semantic version of its own.

| Field | Value |
| --- | --- |
| Current value | The source is at 0.2.0, which has not been released. The only published release is v0.1.0, which contains the test-vector archive and no binaries. [What works, and what does not](status.md#releases-and-versions) gives the release history. |
| Citation | Software concept DOI [10.5281/zenodo.21551570](https://doi.org/10.5281/zenodo.21551570), Apache-2.0 |
| How it relates | Not at all. Software versions and document versions move independently. The software conforms to whatever the documents say, and citing a format means citing its document, not a software release. |

## How this register is checked

`tools/check_guide.py` checks that every REM-OBJECT, REM-ENCRYPT and REM-PARITY section
citation on this page names a heading that exists in the preparing copy. No tool compares the
values on this page with the specifications or the code. Each entry therefore names the
specification section and the code constant to compare it with. `tools/check_spec_versioning.py`
checks the specifications themselves, as
[How the formats change](versioning-explained.md#who-keeps-these-promises) describes.
