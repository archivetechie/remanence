# Prompt: Unified RAO Object Format Specification — publication v1.0 (one document)

**Status: pending → dispatched 2026-07-11.** Direction: the owner 2026-07-11 — the
internal addendum specs are our revision history; the EXTERNAL publication is ONE
coherent document we call **Version 1.0**.

## Task

Produce `specs/publication/rao-object-format-1.0.md`: a single, standalone,
publication-grade **"RAO (Rem Archive Object) Format Specification, Version 1.0"**
that MERGES, into one coherent document, the content currently split across:
- `specs/publication/rao-1.0-specification.md` (base on-tape object format), and
- `specs/publication/rao-1.1-specification.md` (the xattr additive delta), and
- the **envelope-encryption** format (RAO header `format_version = 2`, the
  wrapped-DEK key frame) — NOT yet a publication spec; source it from the LANDED
  implementation on this branch (`crates/remanence-aead/src/{header,key_frame,
  seal,open,range,inspect}.rs`, `crates/remanence-cli/.../rao_recover*`) and the
  frozen design `docs/design-rao-wrapped-key-header.md`.

The three become ONE document with a unified section structure — the xattr and
envelope content folded INLINE as normative parts of the format, NOT as trailing
"delta" or "addendum" sections. A reader of the unified doc must never need the
1.0/1.1 files.

## Versioning discipline (critical — do not conflate two version axes)

- The **document/specification version is 1.0** (the published baseline).
- The **on-tape `format_version` field** it describes takes values **1**
  (plaintext / registry-symmetric) and **2** (envelope). The unified v1.0 document
  normatively describes BOTH format_version 1 and format_version 2 objects,
  version-gated. State this distinction explicitly in a "Versioning" section so no
  implementer confuses spec-doc 1.0 with wire format_version.

## Content requirements

1. **Every byte claim verified against the landed code** (the publication review's
   iron rule). Ground envelope claims in the actual header/key-frame/seal source;
   ground base+xattr claims against the existing specs AND the code they cite.
2. **Publication standards** (mirror `specs/publication/REVIEW-REPORT.md`):
   standalone (no repo paths, no internal tool surfaces), full BCP 14 keyword
   discipline, byte-exact header/key-frame/footer tables with worked examples,
   full retrievable citations (RFC 9180 for HPKE, RFC 3339, RFC 3629, FIPS 180-4,
   AGE, etc.), IANA Considerations, and a Security Considerations section that
   incorporates the envelope threat model (time-scoped attacker matrix;
   confidentiality + self-consistency NOT provenance; recipient-epoch custody;
   randomness per RFC 9180 §9.2.3; zeroization).
3. **Author's Address:** The ArchiveTech Project / https://archivetech.org /
   specs@archivetech.org / reference implementation
   https://github.com/archivetechie/remanence (matching the other publication docs).
4. **Test vectors:** cite the completed archive
   (`specs/publication/remanence-test-vectors.tar`, SHA-256
   `596e5ee7baffb355366407d6b4384fe7caafa64509e489508df2ed5dc2eadc7d`) — both the
   plaintext (§13) and the envelope vectors.
5. **Revision provenance:** a short "Revision History" appendix noting this v1.0
   consolidates internal revisions (base, +xattr, +envelope) — so the internal
   addendum files remain as history but the published doc is one v1.0.
6. **REM-PARITY** stays its own companion document (it is a separate format layer,
   already a single 1.0 doc) — reference it by title, do not merge it in. If the
   base RAO spec cross-references parity, keep the cross-reference.

## Constraints

- Do NOT modify the on-tape wire format or any code — this is a documentation
  consolidation. The internal addendum files (`rao-1.0-specification.md`,
  `rao-1.1-specification.md`) stay in place as revision history; you ADD the
  unified doc.
- The unified doc must pass the publication review's own lens checklist
  (standalone-ness, BCP 14, internal consistency, independent implementability).

## Definition of done (AGENTS.md)

The unified doc exists, is internally consistent, cites the correct vector-archive
SHA, and every envelope byte claim matches the landed code. You cannot commit;
end with a summary + any place the code and the pre-existing spec text disagreed
(resolve in favor of the code, flag prominently).
