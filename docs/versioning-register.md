# The Versioning Register — every versioned component, one entry each

*A companion reference to [versioning-explained.md](versioning-explained.md),
which tells the story; this page is the inventory. Every component of the
system that carries a version, an identifier that acts like one, or a frozen
constant that would change under a new major, has an entry here. Each entry
answers the same five questions: what is it, where does it live, what are
the current values, what does a reader do with a value it does not know, and
how does it ever change. This document is informative; the owning
specification section named in each entry is normative.*

The entries are grouped by layer: what is on the tape (REM-PARITY), what is
in an object (REM-OBJECT), what encryption adds (REM-ENCRYPT), and then the
documents and published artifacts themselves.

---

## Tape layer (REM-PARITY)

### 1. The bootstrap block — `schema_major`

- **What it is.** The bootstrap is the tape's self-description block,
  written at the start of the tape and repeated along it. `schema_major` is
  its generation number — the field that says "this is bootstrap format
  generation 1."
- **Where.** Two bytes at offset 0x08 of every bootstrap block.
  Normative: REM-PARITY §8.1.
- **Current value.** 1.
- **Unknown value.** A reader MUST reject a bootstrap whose `schema_major`
  is not 1. This is deliberate and safe: a different major means a
  different format generation, and refusing loudly is the correct answer.
- **How it changes.** Only a new major version of REM-PARITY — a separate
  document — may assign 2. Version-1 readers then correctly refuse
  generation-2 tapes by this very field, and generation-2 documents govern
  their own tapes.

### 2. The bootstrap block — `schema_minor`

- **What it is.** The bootstrap wire format's feature-generation number.
  It names which optional bootstrap features may be in force; it is **not**
  a revision counter and **not** the document's version.
- **Where.** Two bytes at offset 0x0A of every bootstrap block. Normative:
  REM-PARITY §8.1; its registry is §8.1.1.
- **Current values.** The registry assigns: `2` — object rows carry no
  `object_id` requirement; `3` (current) — `object_id` is required in every
  object row. Published test vectors legitimately carry both, because the
  field advanced while they were generated; all are conformant.
- **Unknown value.** Readers accept any value. This is safe because
  unknown bootstrap payload keys are ignorable by rule (§5.3), so a
  version-1 reader reads through tapes from any later 1.x — the
  read-through path of the policy, not the refuse path.
- **How it changes.** A future minor revision of REM-PARITY that changes
  what can appear in the bootstrap assigns the next value and adds a
  registry row naming itself. Most document revisions assign nothing.

### 3. The bootstrap block — `flags`

- **What it is.** A 32-bit field of on/off switches. One is assigned:
  bit 0 means "this tape was deliberately written without parity."
- **Where.** Offset 0x0C of every bootstrap block. Normative: REM-PARITY
  §8.1.
- **Current values.** Bit 0 assigned; all other bits are written as zero.
- **Unknown value.** Readers MUST ignore flag bits they do not recognise.
- **How it changes.** Because old readers ignore unknown bits, a future
  flag may only carry a meaning that is safe for an old reader to ignore.
  Anything stronger — a flag whose ignorance would mislead a reader —
  requires a new `schema_major`.

### 4. Bootstrap object rows — the integer-key vocabulary

- **What it is.** Inside the bootstrap payload, each stored object has a
  row: a small map of numbered fields (1 = tape file number, 4 = object
  identity, and so on). The set of assigned numbers is a vocabulary that
  can grow.
- **Where.** REM-PARITY §8.2.1.
- **Current values.** Keys 1–4, 10–13, 21–23, 30 are assigned; key 4
  (`object_id`) is required from `schema_minor` 3.
- **Unknown value.** Readers MUST ignore unknown integer keys — this is
  the format's main extension mechanism.
- **How it changes.** A minor revision may assign new keys. A new key MUST
  NOT change the meaning of existing keys or the recovery outcome of any
  tape written without it; a change that would do either is a new major.

### 5. The sidecar — `schema_version` and `footer_version`

- **What it is.** The sidecar is the tape file holding parity data and
  recovery metadata. Its header carries a `schema_version`; its footer
  carries a `footer_version`.
- **Where.** Header offset 0x2C; footer offset 0x08. Normative: REM-PARITY
  §9.2, §9.6.
- **Current values.** 1 and 1.
- **Unknown value.** Readers MUST reject any value other than 1 —
  fail-closed, unlike the bootstrap's `schema_minor`. The sidecar is
  recovery machinery; guessing about an unknown sidecar layout could
  corrupt a recovery, so refusal is the safe behaviour.
- **How it changes.** Assigning 2 to either field is a wire change a
  version-1 reader would refuse — permitted only through the change
  policy's conditions, and in practice this pairs naturally with a new
  major. The `copy_generation` field nearby is reserved and MUST be 0
  while sidecar `schema_version` = 1.

### 6. The parity map — `schema_version` and footer version

- **What it is.** The parity map is the index that locates every parity
  group on the tape. Header and footer each carry a version.
- **Where.** Both at offset 0x08 of their structures. Normative:
  REM-PARITY §10.3.
- **Current values.** 1 and 1.
- **Unknown value.** MUST be rejected — fail-closed, same reasoning as the
  sidecar.
- **How it changes.** As the sidecar: effectively a major-class change.

### 7. The magic labels — version bytes inside "fixed" constants

- **What it is.** Every structure announces itself with magic bytes, and
  each magic ends in a version byte: `"REM\0BOO\x01"`, `"REM\0PAR\x01"`,
  `"REM\0PARFOOT\x01"`, `"REM\0PMAP\x01"`, `"REM\0PMAPFOOT\x01"`.
- **Where.** REM-PARITY §2.5. Sidecar and parity-map magics are further
  keyed to the individual tape by HMAC, so structures cannot migrate
  between tapes.
- **Current values.** All end `\x01`.
- **Unknown value.** A non-matching magic is simply "not this structure" —
  the structure is not recognised at all.
- **How it changes.** A new magic (for example ending `\x02`) is one of
  the defining moves of a new major version. The trailing byte exists so a
  future generation can announce itself unambiguously.

### 8. The erasure scheme — `rs-cauchy-gf256-v1`

- **What it is.** The name of the mathematics used for parity: Reed–Solomon
  over GF(2⁸) with a Cauchy matrix, exactly as specified.
- **Where.** REM-PARITY §6; the identifier appears in the parity metadata.
- **Current value.** `rs-cauchy-gf256-v1` — frozen for the life of the
  1.x line.
- **Unknown value.** A reader meeting a different scheme identifier does
  not attempt recovery with the wrong mathematics; it reports the scheme
  as unimplemented.
- **How it changes.** A different scheme is a new identifier
  (`…-v2` or a new name), introduced only by a revision under the change
  policy; changing what `-v1` means is forbidden forever.

### 9. The discovery block-size candidate set

- **What it is.** Tapes are written in fixed-size blocks; a reader finding
  an unlabelled tape discovers the size by trying each legal size in turn.
  The legal set is therefore itself a versioned surface — and the sharpest
  edge in the whole policy.
- **Where.** REM-PARITY §8.4. Current set: 256 KiB, 512 KiB, 1 MiB.
- **Unknown value.** This is the one place there can be no clean refusal:
  a reader cannot recognise a tape at a block size it never tries — it
  reports no data, which is indistinguishable from a blank or damaged
  tape.
- **How it changes.** It does not, under `schema_major` 1. The set is
  declared frozen for the life of the major, and extending it is
  explicitly a new-major change. This is the worked example of the
  policy's second question.

---

## Object layer (REM-OBJECT)

### 10. The stream format identifier — `rem-object-v1`

- **What it is.** The name of the object stream format itself, recorded in
  every object.
- **Where.** `REMANENCE.format_id`; REM-OBJECT §10.
- **Current value.** `rem-object-v1` — a frozen wire constant.
- **Unknown value.** A reader that meets a different `format_id` knows it
  holds a different format and refuses by name.
- **How it changes.** A new major version of REM-OBJECT is, on the wire,
  a new `format_id` (`rem-object-v2`). Meaning-changing extension of any
  existing rule requires exactly this.

### 11. The stream schema version — a feature gate, not a revision

- **What it is.** `REMANENCE.schema_version`, a text value that is `1.0`
  when no extended attributes are preserved and `1.1` when they are. It
  answers "which optional capability is in use in this object", not "which
  edition of the document wrote it". The numeral coincidence with document
  versions is warned against in the specification itself: a future
  document revision 1.1 would not imply stream-schema 1.1, nor the
  reverse.
- **Where.** REM-OBJECT §10; both values defined by document 1.0.
- **Unknown value.** Readers gate on the major digit ("1") and ignore
  unknown pax keywords, so the read-through path applies.
- **How it changes.** A future minor revision could define a new gate
  value for a new optional capability, under the change policy's
  conditions.

### 12. The manifest schema version

- **What it is.** The manifest — the object's internal inventory of files,
  sizes, checksums, and positions — carries its own integer schema
  version.
- **Where.** Manifest CBOR field `schema_version`; REM-OBJECT §10.
- **Current value.** 1, defined by document 1.0.
- **Unknown value.** Unknown manifest keys are ignored (read-through);
  the version integer moves only if the manifest's structure itself
  changes incompatibly, which is major territory.

### 13. The pax keyword vocabulary — `REMANENCE.*`

- **What it is.** Objects are constrained tar streams, and the format's
  own metadata rides in named pax keywords (`REMANENCE.file_sha256`,
  `REMANENCE.object_id`, …). The keyword list is a growable vocabulary,
  exactly parallel to the bootstrap's integer keys.
- **Where.** REM-OBJECT §4.4; extension rule §4.4.3 and §10.
- **Unknown value.** Readers MUST ignore unknown keywords, and a
  preserving rewrite MUST carry them through unchanged.
- **How it changes.** A minor revision may assign new keywords; no
  extension may alter payload framing, existing meanings, or any enforced
  rule — that requires a new `format_id`.

---

## Encryption layer (REM-ENCRYPT)

### 14. The envelope format version

- **What it is.** The single byte that names the encrypted envelope's
  generation.
- **Where.** Scalar-header offset 0x06; registry REM-ENCRYPT §10.1.
- **Current value.** 2 (current). Value 1 is permanently forbidden — a
  pre-production assignment that never shipped and must never be accepted
  or reused.
- **Unknown value.** Hard error, `UnsupportedFormatVersion` — clean
  refusal by name. There is no read-through for cryptography, by nature.
- **How it changes.** A new envelope generation takes value 3 and a new
  major document. Old envelopes remain openable under the old document
  forever.

### 15. The cipher suite — `suite_id`

- **What it is.** The registered combination of key-derivation and
  encryption algorithms used inside the envelope.
- **Where.** Scalar-header offset 0x07; registry §10.2 with a Defined-by
  column.
- **Current value.** 0x01 = HKDF-SHA-256 + ChaCha20-Poly1305, defined by
  REM-ENCRYPT 1.0.
- **Unknown value.** Hard error, `InvalidSuite`, naming the number — the
  operator knows exactly which capability to obtain.
- **How it changes.** A future minor revision assigns the next value and
  its Defined-by row. Because old readers cannot open the new objects,
  such a revision must carry a prominent compatibility-impact statement.
  Superseded suites remain valid for opening forever; sealers must use the
  current one.

### 16. The key scheme — `wrap_suite`

- **What it is.** The registered method by which the content key is
  wrapped for each recipient — the post-quantum machinery.
- **Where.** Scalar-header offset 0x38; registry §10.3.
- **Current values.** 0x02 (current) = X-Wing hybrid KEM per
  `draft-connolly-cfrg-xwing-kem-10`; 0x01 permanently forbidden (legacy
  X25519-only, never shipped); 0x03 reserved for a future X-Wing
  construction that differs on the wire.
- **Unknown value.** Hard error, `InvalidWrapSuite`, by name.
- **How it changes.** As `suite_id`: registry assignment through a
  revision. One planned case is pre-decided: if the final X-Wing RFC is
  wire-identical to the pinned draft, 0x02 is retained and only the
  citation changes (an erratum); if the RFC differs on the wire, it
  consumes the reserved 0x03 through a minor revision.

### 17. The frozen cryptographic constants

- **What it is.** Values that are part of a suite's identity: the HPKE
  `kem_id` 0x647a frozen for suite 0x02; the envelope magic `REMO` and
  key-frame magic `REMK`; the derivation labels (`rem-encrypt-wrap-v1`,
  `rem-encrypt-salt-v1`, `rem-encrypt-object-v1`,
  `rem-encrypt-metadata-v1`, `rem-encrypt-payload-v1`).
- **Where.** REM-ENCRYPT §5, §10, §15.
- **How it changes.** It does not. Each is frozen for the suite or
  generation that defines it; a different value belongs to a different
  registered suite or a new major. The `-v1` suffixes exist so successors
  can be named without ambiguity.

---

## Documents and published artifacts

### 18. The specification documents

- **What.** Three normative specifications and one informative companion,
  each independently versioned major.minor.errata.
- **Current.** REM-PARITY 1.0.1, REM-OBJECT Core 1.0.1, REM-ENCRYPT
  1.0.1, companion 1.0.1.
- **How they change.** The three-question policy in each document's Status
  section — the subject of [versioning-explained.md](versioning-explained.md).
  Titles and filenames carry the major line only; each revision is
  archived under the document's own concept DOI with a per-revision DOI;
  a new major version, being a separate document, opens a new concept DOI
  series and cites its predecessor, so "latest revision" never crosses a
  compatibility break;
  each document's Revision History appendix records every change and its
  kind.

### 19. The conformance vector archives

- **What.** The published tape images, damage cases, and expected results
  that anchor conformance, pinned by SHA-256.
- **Current.** One archive, SHA `77be73e7…`, containing the 1.0-era
  vectors; images legitimately carry bootstrap `schema_minor` 2 and 3.
- **How they change.** They do not. A vector archive is a permanent anchor
  of the revision that generated it. A future revision needing new
  vectors publishes its own additional archive and cites it by name, DOI
  and digest; nothing is ever re-pinned.

### 20. The software

- **What.** The reference implementation, released under its own semantic
  version (currently 1.0.0) and archived in its own Zenodo record
  (concept DOI 10.5281/zenodo.21551570, Apache-2.0).
- **How it relates.** Deliberately, not at all: software versions and
  document versions move independently. The software's job is to conform
  to whatever the documents say; citing the format means citing a
  document DOI, not a software release.

---

## One habit that keeps this page honest

Several of the mistakes this register exists to prevent were made while
writing the policy itself — a constants table left saying `1 / 2` when the
writer emitted 3, one file carrying two different version numbers, a
Status section contradicting a later section of the same document. The
repository therefore runs `tools/check_spec_versioning.py` on every
verification pass: it checks that the policy text is identical across the
three specifications, that every version number in a document agrees with
itself, that the pinned archive digest matches at every place that quotes
it, that revision histories stay in order, and that references resolve.
When this register and the specifications drift, the build says so before
a reader has to.
