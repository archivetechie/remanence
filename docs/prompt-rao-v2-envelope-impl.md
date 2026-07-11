# Prompt: RAO v2 envelope implementation (remanence, branch rao-wrapped-key-header)

**Status: pending → dispatched 2026-07-11.**
Normative: `docs/design-rao-wrapped-key-header.md` (**FROZEN 2026-07-11** — the
Format, Seal ordering, Mode rules, and Proof & test plan sections are the spec);
`docs/panel-rao-wrapped-key-2026-07-11.md` records rationale. Do not re-litigate
frozen decisions; deviations only where code reality forces them, recorded
prominently.

## Milestones, in order (each independently testable)

1. **Header v2 + key frame codec** (`remanence-aead`): parse/serialize per the
   frozen byte placements (`wrap_suite @0x38`, reserved 0x39..0x3C,
   `key_frame_len` BE @0x3C..0x40; `key_id` zero in envelope mode) and the frozen
   key-frame grammar (RAOK magic, slot rules, byte-exact canonical encoding).
   v1 parsing untouched; v1/v2 disjoint dispatch on `format_version`. Extend
   `rao_negative_vectors` for both geometries: version/suite flips, truncated key
   frame, duplicate/mis-ordered slots, trailing bytes, oversize frame.
2. **Wrap subsystem**: `rust-hpke` (pin exact version in Cargo.toml; record the
   version + rationale in the design's open-question 1), HPKE Base
   DHKEM(X25519)+HKDF-SHA256+ChaCha20-Poly1305, the frozen fixed-width `info`
   transcript, RFC 9180 test vectors + our byte-level wrap vectors. Fallible
   OS CSPRNG (getrandom) — seal fails closed without entropy; DEK/ephemerals in
   `zeroize` non-cloneable containers matching `RootKey` discipline.
3. **v2 seal/open/range/stream/inspect**: the 6-step transcript exactly (salt
   from DEK via `rao2-salt-v1`; `header_hash = SHA-256(header ‖ key_frame)`;
   `rao2-*` derivation labels); geometry per the frozen footer formula; mode
   rules (expected mode from caller config; header disagreement hard-rejects;
   NO registry↔envelope fallback). v1 paths byte-for-byte unaffected (existing
   vectors prove it).
4. **`rao-recover` bin**: standalone object+private-key → plaintext members; no
   catalog/daemon/network; enumerates slots, auto-selects epoch, actionable
   mismatch message with epoch labels; dispatches v1 objects to the
   registry-key path. Static-friendly build (no dynamic deps beyond libc).
5. **`rem archive capabilities` verb**: machine-readable (JSON) capability list
   including `rao-v2-envelope`, `wrap-suite-hpke-v1`.
6. **Proof follow-ups**: **RAO-V2-FORMAL-PREFIX** must carry (header,
   key-frame) lengths through the Aeneas extraction before proving both
   geometries; **RAO-V2-FORMAL-HEADER-KEY-FRAME** must extract the actual v2
   header/key-frame byte codecs before a round-trip theorem is claimed. Until
   then v2 is Rust-test/fuzz/drift-guard-covered, not formally proved.
7. **Re-seal tool**: `rem archive reseal` — reads a v1 AEAD object with a
   registry key file, re-seals v2 to configured recipients; used for the
   ~6-object migration. Verify-after-write (remote hash comparison pattern).

## Definition of done (AGENTS.md applies)

Full `cargo test` + negative vectors + updated fuzz targets (short smoke run) +
`make proof-inventory` green + lint. You cannot commit (.git read-only) — ordered
commit-ready summary per milestone at the end. Sutradhara-side integration
(recipient-epoch store, pool config, escrow export) is a SEPARATE later prompt —
do not touch ~/sutradhara.

## Verification member

Harness scenario extension `~/system/docs/prompt-scenario-rao-v2-envelope.md`
(cut with this set): extends RAO format coverage with a v2 seal → tape →
`rao-recover` round-trip through the real rem CLI, plus a v1/v2 mixed-object
tape read. Green required before this prompt is archived implemented.
