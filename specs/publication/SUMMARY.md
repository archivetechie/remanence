# Spec Review — Summary of Work

Date: 2026-07-08. This is the executive summary; the full record is in
[REVIEW-REPORT.md](REVIEW-REPORT.md), and all 261 raw panel findings are in
[panel-findings-2026-07-07.md](panel-findings-2026-07-07.md).

> **Resolution (2026-07-11).** The three specs have since been combined into one
> normative publication document,
> [`rao-object-format-1.0.md`](rao-object-format-1.0.md) (base + 1.1 xattr delta
> + on-tape v2 HPKE envelope). The four "Needs your decision before publishing"
> items below are all resolved: (1) bootstrap re-typing and (2) the two
> manifest gaps landed in the scanner/reader; (3) the author line is **The
> ArchiveTech Project** (archivetech.org / specs@archivetech.org), replacing an
> earlier personal-name-and-affiliation placeholder; (4) the test-vector
> archive exists with SHA-256 `596e5ee7…` pinned. This summary is retained as a
> dated historical record.

## Task

Take the three Remanence specifications — RAO 1.0 (the on-tape file format),
RAO 1.1 (the xattr delta), and REM-PARITY 1.0 (the tape/parity format) — and
prepare them for standalone international publication in the IETF/RFC style, so
anyone in the world can implement them without access to the repository or prior
context. The originals in `specs/` are untouched; all work landed in
`specs/publication/`.

## Process

A 12-reviewer panel — six lenses (RFC structure/references, BCP 14 keyword
audit, standalone-ness, internal technical consistency, independent
implementability, editorial register) across the two spec bundles — followed by
adversarial verification of the major technical findings (26 confirmed, 4
partial, 1 refuted).

**261 findings total: 9 critical, 106 major, 119 minor, 27 nit.** Every fix that
could change emitted bytes was checked against the Rust reference implementation
before editing, since the wire format is settled.

## What changed, by category

### Standalone-ness

Removed everything that only meant something inside the project's world:
repository paths (`crates/…`, `fixtures/…`), the `rem archive` tool surface,
`finish_object()`, QuadStor VTL / MSL3040 hardware, milestone codes
(PAR-KEY30-RECOVERY), the "internal conformance backlog", and two unpublished
design-doc citations. "Editorial note (remove at freeze)" blocks became proper
*Status of This Document* sections; cross-references now cite the companion spec
by title, not by filename.

### RFC skeleton

Added the sections a citable standard is expected to have: IANA Considerations,
Author's Address, and Security Considerations (1.1) where missing. Rewrote every
reference as a full retrievable citation and added the normative ones the text
actually relies on but never listed (RFC 3339, RFC 3629, FIPS 180-4 for SHA-256,
a pinned POSIX edition, full AGE/USENIX citations). RAO 1.1 was rebuilt from a
bare delta into a complete document.

### Byte-determinism defects

The ones that could make two honest implementations disagree — all verified
against code:

- **Pax record self-length isn't unique** — base lengths 8, 97, 996, … admit two
  self-consistent values, and the one-byte-file test vector sits exactly on the
  ambiguous case. Now pinned to the smaller fixed point, matching `pax.rs`.
- **`g`/`x` header `mode` was unspecified** — pinned to `0000644`, matching
  `writer.rs`.
- **Long-link linkname placeholder** (`remanence/pax-linkpath`) existed in code
  but not the spec — now mandated.
- **`mtime` canonical form undefined** — pinned as caller-string-verbatim.

### Confirmed contradictions, now resolved

The §6.4 `k+2` read-amplification bound (wrong for large ranges →
`k + ceil(16·k/C) + 1`); the TV-E1 worked example that claimed nonces don't
collide when they do; REM-PARITY's §11.1 commit cycle vs §3.4's sequential rule
(resolved per `sink.rs`: the object-close bundle commits atomically); §14's dead
"complete epochs" resume sentence; the bootstrap trailing-fill
"ignore vs verify-zero" conflict; and the undefined `manifest_first_chunk_lba`
(key 10), now defined in-document.

### BCP 14 hygiene

Keywords that bound out-of-scope parties (deployments, catalogs, the key
registry) recast as guidance or rebound to real conformance roles; lowercase
prohibitions promoted to MUST NOT; duplicate-strength statements deduplicated.

## Needs your decision before publishing

1. **Bootstrap re-typing** — the panel's top critical: a destroyed
   checkpoint-bootstrap block gets classified as a 1-block object, poisoning
   every digest scope over it — single-block damage defeating design goal 5. A
   SHOULD-level reconciliation was added to §12.4 and flagged in Appendix C, **but
   the scanner does not do this yet.** Implement it, or narrow the claim.
2. **Two spec-over-implementation gaps** created by closing dangling promises:
   manifest duplicate path/file_id rejection, and missing-manifest reporting —
   spec'd now, not yet in `manifest.rs`/`reader.rs`.
3. **Author line** — confirm the name/affiliation/email added to all three
   (personal name / employer affiliation / work email).
4. **Test-vector distribution** — the docs now promise vectors "distributed
   alongside this specification"; that archive has to exist at freeze, with its
   SHA-256 printed in §13/§17.

## Recommended path

Publish as versioned, self-hosted specs in the **C2SP style** (the model the age
format cited by RAO uses) — a public repo with tagged releases gives the stable
citable locator the references now assume. If an RFC number is wanted later, the
documents are one mechanical `kramdown-rfc` conversion away from an
Independent-Submission Internet-Draft; no content needs to change.
