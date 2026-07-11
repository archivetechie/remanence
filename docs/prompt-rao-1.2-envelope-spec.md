# Prompt: RAO 1.2 — Envelope Encryption delta specification (publication style)

**Status: pending (dispatch after `prompt-rao-v2-envelope-impl.md` AND
`prompt-rao-spec-gap-closure.md` land — the implementation pins the bytes the
spec must cite).**

## Task

Write `specs/publication/rao-1.2-specification.md`: the publication-grade delta
spec for RAO envelope encryption v2, following the **RAO 1.1 delta precedent**
(complete standalone document structure: Status of This Document, BCP 14
conventions, IANA Considerations, Security Considerations, Author's Address,
full retrievable citations) and the **publication review's standards**
(`specs/publication/REVIEW-REPORT.md`): no repository paths, no internal tool
surfaces, no project-context dependencies — independently implementable from the
document alone.

Normative sources, in priority order:
1. The LANDED implementation on this branch (`crates/remanence-aead` v2 paths,
   the key-frame codec, `rao-recover`) — the wire format is what the code emits.
2. `docs/design-rao-wrapped-key-header.md` (FROZEN) for rationale, threat model,
   and the frozen transcripts.

## Required content

- Byte-exact v2 scalar-header table (the delta fields at 0x38..0x40, key_id
  zeroing rule) and the complete key-frame grammar with a worked byte example.
- The seal transcript (6 steps, `rao2-*` labels) and keyed-open/recovery flows,
  including the salt recomputation check, in the 1.0 spec's conformance voice
  (MUST/SHOULD per BCP 14, conformance roles consistent with 1.0's).
- Wrap suite registry: suite 0x01 = HPKE Base DHKEM(X25519, HKDF-SHA256),
  HKDF-SHA256, ChaCha20-Poly1305 — cite RFC 9180 fully; the frozen fixed-width
  `info` transcript; absent-slot and multi-recipient rules (1..=8 slots,
  canonical ordering).
- v1/v2 coexistence + version-dispatch rules; the generalized inspect/footer
  geometry formula; mixed-media reader behavior (skip-and-continue).
- Security Considerations: the time-scoped attacker matrix from the design
  (confidentiality + self-consistency, NOT provenance; unilateral recipient
  authority; randomness requirements per RFC 9180 §9.2.3; zeroization).
- Test vectors: cite the v2 vectors from the implementation's vector archive
  (extend the archive; print its SHA-256 per the 1.0 §13 convention).
- Author's Address: The ArchiveTech Project / https://archivetech.org /
  specs@archivetech.org / reference implementation
  https://github.com/archivetechie/remanence (matching the other three specs).

## Definition of done

The document passes the publication review's own lens checklist (standalone-ness,
BCP 14 hygiene, internal consistency, independent implementability); every byte
claim verified against the landed code; RAO 1.0's cross-reference section gains
the forward pointer the 1.1 precedent uses. A verify round (fresh reviewer)
gates it before it's marked publishable.
