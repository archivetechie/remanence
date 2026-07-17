# P2 rem-vectors+spec — RAO-TV-E2, independent v2 verifier, publication excision

Do NOT read or execute any files under ~/.claude/, ~/.agents/, .claude/skills/, or agents/.

**Contract:** `docs/design-rao-v2-only.md` (v0.3) — decisions D1, D2, D5
(vector strategy + the full Q4 negative-case list), D3 (reader slot policy,
identity encodings). Read it in full; it wins on any disagreement.
**Precondition:** P1 (`docs/prompt-rao-v2-only-p1.md`) has fully landed —
the workspace is v2-only and `seal_envelope_deterministic_for_test_vectors`
exists. Verify that before starting; stop if not.

**Repo:** this repo only. Scope: `fixtures/rao/`, `tools/`,
`specs/publication/`, `Makefile` vector targets. Do not touch crate source
except where a test consumes a fixture you re-base.

## Guardrails

- The **positive v2 wire bytes are frozen** (D1): existing v2 fixtures must
  verify unchanged. The single sanctioned negative re-pin is
  `v2-version-flip` → `UnsupportedFormatVersion`.
- The independent Python verifier must stay INDEPENDENT: pure-Python (plus
  the `cryptography` package primitives), no calls into the Rust code, no
  shared constants files — it re-derives from the spec's prose.
- The publication specs are normative prose — surgical edits, preserve the
  document register; do not restructure beyond what excision requires.

## Work

1. **RAO-TV-E2** (D5): a positive whole-object v2 vector — fixed inputs,
   fixed DEK, seeded ephemeral RNG via
   `seal_envelope_deterministic_for_test_vectors`; pinned artifact
   (`rao/objects/rao-tv-e2.rao`) + fixture manifest with the full derivation
   chain (recipient keypairs included as test material). Byte-exact
   regeneration proven by generating twice.
2. **Independent verifier:** extend
   `tools/verify_rao_vectors_independent.py` with the v2 OPEN direction
   (X25519 + HKDF-SHA-256 + ChaCha20-Poly1305): unwrap the DEK from the
   pinned key frame with the vector's recipient private key, decrypt, verify
   `plaintext_digest` and per-file digests byte-exact. Remove its v1 seal
   support and v1 assertions (v1 objects no longer exist in the archive).
3. **Negative vectors** (D5's full list): re-base version-agnostic
   `negative-envelope.json` cases onto a v2 base object; drop/replace
   v1-specific cases; extend `negative-envelope-v2.json` with every case in
   design D5's Q4 paragraph (key-frame tampering set, slot counts 0/9 +
   writer 0/1/>8 rejection, one-slot READ ACCEPTANCE positive case,
   known-suite mismatches, duplicate recipient_epoch_id — parser must
   reject; add the parser check if P1 missed it — internal slot truncation,
   nonzero reserved key_id region, malformed key-frame magic, wrong
   recipient private key, malformed encapsulation). Rust tests consume every
   new case (completeness gate: each manifest case asserted by exactly one
   test).
4. **TV-D1**: re-base its encrypted half to v2; plaintext half unchanged.
5. **Regenerate the archive:** `make publication-test-vectors`; the
   verifier and the staged `verify.py` must pass; record the new SHA-256.
6. **Publication spec excision** — `specs/publication/rao-object-format-1.0.md`:
   - Front-matter table: single on-tape format version `2`; drop the
     registry-symmetric row; `format_version = 1` documented in §10 as
     permanently reserved, never reassigned.
   - Abstract + §1 design goals: v2-only (determinism claim scoped to the
     plaintext representation; envelope varies per seal).
   - §5: remove the v1 header form, v1 salt/key derivation, v1 seal/open;
     bytes `0x10..0x20` become reserved-MUST-be-zero; single header form.
   - §12.8: rotation text keeps the re-seal semantics (now the reseal verb's
     v2→v2 definition); §12 drops v1-determinism/equality-disclosure
     passages (no longer a mode).
   - §13: TV-E1 replaced by TV-E2 (document the deterministic-generation
     hook honestly — the "no test-only interfaces" claim is REMOVED per
     verify V-6/L1-3); §14 conformance rewritten v2-only ("MUST implement
     v1" clause gone); appendices A/B updated where they derive or
     rationalize v1 (A's envelope worked example re-derives around TV-E2's
     geometry; B.4/B.5 etc. keep their v2 rationale, v1 contrasts removed).
   - Re-pin the new archive SHA-256 (§13/§17 locations in BOTH publication
     docs — object spec and `rem-parity-1.0-specification.md`).
   - Gate greps (all must return zero hits in both publication docs):
     `format_version = 1`, `registry-symmetric`, `root key`, `key_id`,
     `MUST implement v1`, `no test-only`, `RAO-TV-E1`. Report any
     intentional survivor with justification.
7. **Internal spec docs:** `specs/rao-1.0-specification.md`,
   `specs/rao-1.1-specification.md`, `docs/rao-v1-specification.md` get a
   top banner: retained as historical v1 records; the publication doc is
   normative and v2-only. (Do not edit their bodies.)

## Verification (final)

`cargo test --workspace` green; `make publication-test-vectors` reproducible
(run twice, identical SHA-256); `python3 tools/verify_rao_vectors_independent.py`
green against the staged archive; the gate greps; summary lists the new tar
SHA-256, every fixture case added/dropped/re-based, and every spec section
touched.
