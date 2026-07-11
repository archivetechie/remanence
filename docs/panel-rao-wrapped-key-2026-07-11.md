# Panel review — design-rao-wrapped-key-header.md (2026-07-11)

Five blind lenses: security/trust-boundary (codex gpt-5.6-sol), failure-modes/ops
(Opus), contract coherence (Opus), cost/efficiency (Opus), DR-ceremony UX (Opus).
**72 findings: ~10 blockers, ~30 majors.** One round interrupted by the org spend
limit and resumed (see `~/system/docs/process-panel-review.md` §Budget-wall
resilience — this incident created that section). Verbatim lens outputs preserved
in `panel-lenses-2026-07-11/`.

## Verdict before dispositions

The draft framed a new **envelope-sealing subsystem** as a header change. The fold
is therefore a full v2 rewrite, not a patch. All decisions below are Claude's
(technical, pre-delegated); the design stays DRAFT → verify round before freeze.

## Blocker themes → fold decisions

1. **Byte budget is broken** (security: fields total 337 B > 256; contract: exact
   v1 layout has a fixed 64-byte object_id at 0x40..0x80). → v2 uses a
   **variable-length authenticated KEY FRAME** between header and metadata frame
   (mirrors `metadata_frame_len` mechanics); the scalar header stays fixed-size
   with `key_frame_len` + `wrap_suite` in currently-reserved bytes. Also resolves
   third-recipient extensibility without a v3.
2. **Wrap suite unfrozen** (security). → Frozen: HPKE Base
   DHKEM(X25519)+HKDF-SHA256+ChaCha20-Poly1305, immutable wrap-suite id, byte-level
   test vectors as a deliverable. Implementation via `rust-hpke` (RFC-9180 vectors)
   preferred over hand-rolling from owned `ring` primitives; libsodium (C dep) and
   the full `age` crate rejected in-header (cost). Stock-`age` retained ONLY for
   the external escrow export file (cost: an age ciphertext is ~180-220 B and
   cannot fit an in-header slot anyway).
3. **KDF/authentication claims were wrong both ways** (security + failure-modes:
   v1 binds the FULL header via `SHA-256(header)` into HKDF in seal/open/range —
   `stream.rs` empty AAD notwithstanding; contract: wrapped-key bytes inside
   `header_hash` would break re-wrap). → v2 rule: `header_hash` covers the
   canonical header AND the key frame; **seal ordering fixed**: DEK → wrap slots →
   serialize key frame + header → derive keys → encrypt. Re-wrap without re-seal is
   explicitly forbidden (rotation = new objects wrap to new generation; old
   generations' private keys are retained per retention policy).
4. **Slot transplantation** (security). → HPKE `info` binds
   (protocol label, object_id, recipient_id, slot index, version, wrap suite).
5. **Randomness does not exist in the seal path** (security: deliberately
   deterministic today, no OS-RNG dependency). → Fallible OS CSPRNG introduced;
   seal fails closed without entropy; DEK/ephemerals in zeroizing containers
   (matches existing `RootKey` discipline).
6. **The recovery product was missing** (DR-UX blockers ×5; contract: restore
   adapter is AssetLocator/catalog-coupled, so "no catalog" collapsed at the last
   step; `rao-unwrap`/escrow export had no spec). → New first-class artifact
   **`rao-recover`**: standalone header+key-frame → DEK → plaintext-bytes decrypt,
   zero catalog/daemon/network; escrowed as a **static binary + pinned
   source/toolchain image**; drills periodically REBUILD from the archived source;
   enumerates slots, prints human-readable generation labels on mismatch.
7. **Human ceremony under-designed** (DR-UX). → Shamir **2-of-3** with named
   custodians + documented succession/re-split (2-of-2 was strictly worse than a
   single holder); paper holds the runbook + generation registry (labels,
   dates, locations) — key material only as split shares in separate custody;
   drills reassemble the passphrase from actual shares, restore a canary object
   on a cold machine, with a STANDING-style escalation + named remediation
   playbooks on failure; heavy annual end-to-end + light quarterly checks.
8. **Single-recipient loss = silent total loss** (failure-modes). → **≥2
   distinct-custody recipient slots MANDATORY** for envelope pools; seal fails
   closed if any configured slot fails to wrap; catalog records the recipients
   each object was ACTUALLY wrapped to; per-slot `recipient_id`.

## Major themes → fold decisions

- **Scope narrowed** (cost): v2 envelope headers apply ONLY to envelope pools (the
  offsite archive AEAD copy). hdcache per-asset objects, the backup domain, and
  every hot read path stay v1/H=128 — no header doubling or offset churn on
  high-count paths.
- **Proof strategy** (cost): parametrize `HeaderLen`/frame offsets in
  `aead-framing` ONCE (`∀ H` + frame lengths) instead of re-proving per size;
  impact map now enumerates the pinned literals (`checked_sub(144)`,
  `assert_eq!(header_len,128)`, each `rw [header_len_val]` site, drift-guard
  string matches).
- **Migration resolved** (cost × failure-modes): census today ≈ 6 sealed AEAD
  bundles → **re-seal ALL existing v1 AEAD objects to v2 during rollout** (cost
  ≈ nothing; removes objects encrypted under the public dev seed — v1-forever was
  unsafe, not conservative). v1 read support retained; rollout rule: every
  reader/verify host upgrades before the first v2 object lands on shared media;
  multi-object tape readers skip-and-continue past unparseable objects.
- **Mode confusion** (security): no registry↔envelope fallback after failure;
  expected-mode comes from the catalog/pool config and header disagreement is a
  hard reject; envelope-only entry points for v2 pools.
- **Provenance honesty** (security): catalogless recovery = confidentiality +
  self-consistency, NOT provenance (anyone with the recipient PUBLIC key can
  fabricate an internally-valid object). Threat model rewritten as a time-scoped
  attacker matrix; escrow-holder unilateral read authority stated explicitly;
  independently-signed manifest listed as an optional extension, not claimed.
- **Crash windows** (failure-modes): source retained until footer confirmed AND
  wrapped keys durably recorded; reconciliation GCs wrapped-key rows for
  footerless objects; DEK return transport = tmpfs 0600 + shred-after-extract in
  the runbook.
- **Lifecycle controls** (failure-modes + DR-UX): generation retirement
  hard-gated on zero live references (verify-sweep parses v2 key frames and
  reports recipient-generation coverage); escrow staleness counter + alarm
  ("objects sealed since last successful export"); recipient-key fingerprints
  pinned via an offline-approved list with drift alarms.
- **Catalog/locator index** (DR-UX): a catalog snapshot rides the LAN backup box
  + escrow (an index does not weaken the air gap — "which tape?" must be
  answerable without the host); per-tape object manifest in the runbook.
- **Naming** (contract): "recipient generation" → **recipient epoch** (collides
  with `KeyEpoch.generation` and `media_generation`); ASCII label field in the
  key frame for eyeball diagnosis (DR-UX minor).
- **Capability signaling** (contract): `rem archive capabilities` verb; sutradhara
  checks before selecting the v2 sealer; recipient keypairs live in a NEW store
  (not `KeyEpoch`); `key_domain()`'s default-to-archive fork fixed; wrapped-key
  operational copies live in the dedicated backup/bundle tables, never free-form
  `storage_metadata`.
- **Fail-open semantics documented** (security minor): per-frame fail-closed, not
  whole-stream atomic — restore callers stage output and publish on success (as
  sutradhara already does).

## Rejected / noted-only

- Fixed 256-byte header (original draft): rejected — byte math + extensibility.
- Hand-rolled HPKE from `ring` primitives: rejected as audit burden (cost lens
  priced both; boring wins).
- LTO back-read longevity (DR-UX nit): real, but belongs to the tape-migration
  cadence thread, noted in the runbook as an escrow-kit concern.

Panel stats: 72 findings (13 security / 17 failure / 15 contract / 8 cost / 19
DR-UX) → 8 blocker themes + 12 major themes folded; 0 business questions (all
technical; key decisions pre-delegated). Fold output: design v2 (full rewrite).

## Verify rounds

**Verify-1 (fresh codex): FAIL** — 2 blockers (incomplete wire spec; missing salt
transcript) + 2 majors (HPKE info not a byte transcript; migration wording vs
scope rule). All four arrived with dictated fixes, folded verbatim; verify-1 also
ANSWERED the design's open questions (exact free header offsets 0x0C..0x10 and
0x38..0x40; salt derivation order; footer geometry formula confirmed).

**Verify-2 (fresh codex): PASS** — all four resolved against header.rs/seal.rs/
open.rs/kdf.rs; key-frame grammar independently parseable; no refold-introduced
errors. **Design FROZEN at the 2-round structure (panel + fold + 2 verifies).**
