# Publication Review Report — RAO 1.0 / RAO 1.1 / REM-PARITY 1.0

Date: 2026-07-08. Scope: prepare the three specs in `specs/` for standalone
international publication in the internet-standards (IETF/RFC) style. The
originals in `specs/` are untouched; the revised versions live in this
directory (`specs/publication/`).

## Process

A 12-reviewer panel (six lenses × two spec bundles: RFC structure/references,
BCP 14 keyword audit, standalone-ness, internal technical consistency,
independent implementability, editorial register) reviewed the originals,
followed by adversarial verification of the major technical findings
(26 CONFIRMED, 4 PARTIAL, 1 REFUTED; 11 verifications lost to a session
limit and re-checked by hand). 261 findings total: 9 critical, 106 major,
119 minor, 27 nit. The full findings list is in
`panel-findings-2026-07-07.md` (line numbers refer to the ORIGINALS in
`specs/`).

Where a fix could change emitted bytes, the reference implementation was
consulted as ground truth before editing (files cited below).

## What was fixed

### Standalone-ness (all three documents)

- Removed every repository path (`crates/…`, `fixtures/…`, `docs/…`,
  `specs/…`), the `rem archive inspect` / `--key-file` tool surfaces,
  `finish_object()` (now "the parity layer's durable object-commit
  operation"), QuadStor VTL / MSL3040 (now "virtualized and physical LTO
  tape hardware"), the PAR-KEY30-RECOVERY milestone code, "internal
  conformance backlog", and the two unpublished design-document citations
  in the 1.1 delta.
- "Editorial note (remove at freeze)" blocks became proper "Status of This
  Document" sections.
- Cross-document references now cite the companion specification by title
  ("published alongside this document") instead of file path.
- Test vectors are now "distributed alongside this specification";
  "orchestrator" became "the identifier-assigning / archiving system";
  Appendix C items were rephrased to be readable by outsiders and marked
  (Informative).

### RFC-skeleton completeness

- Added IANA Considerations (RAO §15, REM-PARITY §19; References renumbered
  to 16/20 — nothing referenced the old numbers).
- Added Author's Address back matter to all three documents.
- References rewritten as full retrievable citations (author, title,
  series, date, URL). Added missing normative references used by the text:
  RFC 3339 (timestamps), RFC 3629 (UTF-8), FIPS 180-4 (SHA-256, missing
  entirely from RAO), pinned POSIX edition (IEEE Std 1003.1-2017), full
  [AGE] and USENIX citations. Added informative RFC 2104/RFC 8446 (RAO
  §12.1 dual-PRF discussion) and RFC 9562 (tape_uuid).
- Reference classification fixed: [REMPARITY] is now normative for RAO's
  §8.2 tape binding; [RAO] is now normative for REM-PARITY's bootstrap
  key 30; [AGE] stays informative with an explicit statement that RAO
  §5.6.1–5.6.3 are self-contained.
- RAO 1.1 rebuilt with the full skeleton: Abstract, BCP 14 Conventions,
  Security Considerations, IANA Considerations, References.

### Byte-determinism defects (verified against the implementation)

1. **Pax record length double fixed point** (RAO §4.4.1). The claimed
   uniqueness is false (base lengths 8, 97, 996, … admit two lengths); the
   one-byte-file vector sits on the ambiguous case. Spec now pins the
   smaller fixed point — matching `remanence-format/src/pax.rs`
   (`pax_record_len` iterates upward from `digits(base)`).
2. **`g`/`x` header `mode` unspecified** (RAO §4.3.1). Pinned to
   `0000644\0`, matching `writer.rs` (`encode_header(…, 0o644)`).
3. **Long link-target linkname placeholder undefined** (RAO §4.3.1/4.6.1).
   Added constant `PAX_LINK_PLACEHOLDER` = `remanence/pax-linkpath`
   (from `tar.rs`) and mandated it; added a long-link-target vector to
   §13.1.
4. **`mtime` canonical form** (RAO §4.4.4). Pinned as caller-supplied
   string emitted verbatim after shape validation (matches
   `model.rs: mtime: Option<String>`).

### Internal contradictions (panel-confirmed, now resolved)

- **RAO §6.4 read-amplification bound** was wrong for large runs: now
  `k + ceil(16·k/C) + 1` (`k + 2` only when `16·k ≤ C`).
- **RAO Appendix A (TV-E1)** asserted the metadata/chunk-0 nonces do NOT
  collide; §5.5.2/§12.2 (and arithmetic) say they do. Sentence corrected.
- **RAO §5.2** cross-reference 13.4 → 13.5; §2.2 Builder/Planner
  references 4.8 → 4.9; `InvalidObjectIdField` taxonomy no longer claims
  an unreachable ">64 bytes" read-side case.
- **REM-PARITY §8.1 vs §16.2** (bootstrap trailing fill "readers ignore"
  vs "MUST be verified zero"): resolved as written-zero + Verifier-checked,
  but excluded from discovery/classification acceptance so a damaged fill
  byte cannot cost the tape its entry point; §16.2 carve-out added.
- **REM-PARITY §11.1 vs §3.4** (sidecars committing before the object):
  resolved per `remanence-parity/src/sink.rs` — the object's boundary
  advances before sidecar emission and the object-close bundle commits
  atomically (one record/transaction covers object + sidecars). §3.4 and
  §11.1 now say so; §14 step 3's unreachable "complete epochs" sentence
  replaced with the reasoning (matches `resume.rs`, which enforces
  `T − W < S × k` and keeps full-epoch rebuild out of the v1 path).
- **REM-PARITY §8.2** "REQUIRED unless *minimal* no-parity" (undefined
  term) → "REQUIRED unless no-parity"; key 5 presence now uses keyword
  language; key-30 final-bootstrap MUST now conditioned on implementing
  key 30.
- **REM-PARITY §8.2.1 key 10** (`manifest_first_chunk_lba`) now defined
  in-document (object-relative block index, not a §3.2 physical LBA),
  with keys 11–13/20–21 glossed and [RAO] cited normatively.
- **REM-PARITY §12.3** sidecar count mismatch now explicitly a hard error
  (matches `scan.rs`); §12.4 now states the converse-gap disposition
  (scanned sidecar absent from directory retains its scanned
  classification — matches `scan.rs` overlay); §7.3 digest vector marked
  as deliberately synthetic (a 2-block sidecar is not constructible).
- **REM-PARITY §8.3** MUSTs over an undefined checkpoint-policy structure
  replaced by the wire-observable constraints only.
- **REM-PARITY §8.4** discovery chapeau/keyword conflict resolved
  (strategies 1/2/4 REQUIRED, 3 OPTIONAL, 5 SHOULD) and the arrow-chain
  scan rules converted to a keyworded list.

### The one substantive gap — bootstrap re-typing (NEEDS YOUR DECISION)

The panel's top critical (CONFIRMED): a destroyed checkpoint-bootstrap
block is classified "object by elimination" (§12.3), the overlay re-types
only sidecars (§12.4), so every digest scope covering that file fails —
single-block damage defeating design goal 5 and the §17 damage-matrix
claim. The publication copy adds a SHOULD-level **bootstrap re-typing**
reconciliation to §12.4 (re-hypothesize unreadable 1-block files as
bootstraps before declaring digest mismatch), qualifies the §17 damage
matrix, and records promotion-to-MUST as Appendix C item 4. **The
implementation does not do this yet** — decide whether to implement it (and
pin a damage-matrix vector) or weaken goal 5 instead.

### BCP 14 hygiene

Keywords binding out-of-scope parties (deployments, catalogs, the key
registry) recast as guidance or rebound to conformance roles (Restorer,
Sealer, Writer, and a new "Restoring Consumer" clarification of the
Consumer role for §12.10's restore-safety MUSTs). Lowercase "may
panic"-type prohibitions promoted to MUST NOT in both documents;
non-keyword "REQUIRES" fixed; duplicate-strength statements deduplicated
(§4.3 vs §4.3.2; §1.4 vs §11.4 drive compression; §4.5.1 chunk_size
scoped "on tape").

### Additions that are new normative content (flag for your review)

- RAO §4.7.2: Consumers must reject manifests where two `file_entries`
  share a `path` or `file_id` (closes the dangling §4.6.6 promise).
  **Implementation gap:** `manifest.rs` does not currently check this.
- RAO §4.9 step 6: an object ending without a manifest entry is
  nonconformant; Verifiers MUST reject, restore-mode Readers SHOULD report.
- RAO §4.2: explicit `chunk_size ≤ 2^32 − 512` bound for encrypted-capable
  objects (was implicit in the u32 header field).
- REM-PARITY §17: added a derived-magics vector (sample `tape_uuid`);
  §14 step 5 resume bootstrap sequence must exceed every committed
  sequence; tape_uuid RECOMMENDED as v4 UUID.
- RAO 1.1: Writer MUST reject non-UTF-8 xattr names (`InvalidInput`);
  reader/restore obligations keyworded; Security Considerations added
  (privileged-namespace xattrs — Restoring Consumers MUST NOT reapply
  outside `user.` without explicit configuration); test vector RAO-TV-X1
  pinned concretely (`user.color` = `72 65 64` over the RAO-TV-P1 base).
  1.1 leaves xattrs permitted on symlink/directory entries (as the draft
  implied); restore-side symlink caveats are covered in Security
  Considerations. Confirm this scope choice.

## Deliberately NOT changed

- Wire format: no byte layouts, cryptography, or identifiers changed.
- The metadata front-matter tables (fine for Markdown publication; they
  become proper front matter on xml2rfc conversion).
- Markdown tables, non-ASCII math symbols (Σ, ×, ⊗, ≤), and residual bold
  emphasis — they render fine in Markdown/HTML; they only matter if you
  convert to plain-text RFC rendering (see below). The worst instances in
  normative sentences were fixed; a full sweep is a conversion-time task.
- Role-name capitalization (Reader vs reader) — partial; full sweep is a
  conversion-time task.
- Minor findings I judged not worth churn: unused error-taxonomy entries
  (SourceIo/TapeIo/Io — the taxonomy is their definition), payload-format
  requirements phrased on "a payload format" (standard practice),
  fuzzing SHOULDs (common in modern RFCs).
- Open clarification not resolved: whether `REMANENCE.executable` may
  appear on non-regular entries (RAO §4.6.2) — needs your call, then one
  sentence.

## Before you publish

1. Confirm the author line (name/affiliation/email) added to all three.
2. Decide the bootstrap re-typing question (above).
3. Close the two implementation gaps or soften the spec text (manifest
   duplicate check; missing-manifest reporting).
4. The test-vector distribution must actually exist at publication time —
   the docs now promise "distributed alongside this specification"; once
   generated, print the archive's SHA-256 in §13/§17.
5. Publication venue. Recommended: publish these as versioned, self-hosted
   specifications in the C2SP style (the model used by the age format the
   RAO spec already cites) — a public repo + tagged releases gives the
   stable citable locator the references now assume. If you also want an
   RFC number: convert to an Internet-Draft with kramdown-rfc
   (https://github.com/cabo/kramdown-rfc, the standard Markdown→RFC XML
   toolchain; see https://authors.ietf.org/drafting-in-markdown) and
   submit to the Independent Submission Stream — expect structural edits
   (front-matter, figure/table conventions, ASCII math) and a long review
   cycle. The documents as revised are one mechanical conversion away from
   I-D form; nothing in their content should need to change.
