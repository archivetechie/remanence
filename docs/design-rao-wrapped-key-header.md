# Design v2: RAO envelope encryption — wrapped-DEK key frame

**Status: FROZEN 2026-07-11** (panel 72 findings → v2 rewrite → verify-1 FAIL folded verbatim → verify-2 PASS, no new blockers/majors). Implementation sequences with the dev-seed replacement arc; prompts not yet cut.
`panel 2026-07-11: 72 findings (13 security / 17 failure / 15 contract / 8 cost /
19 DR-UX); folded as v2 rewrite` — consolidated report:
`panel-rao-wrapped-key-2026-07-11.md`; verbatim lenses: `panel-lenses-2026-07-11/`.
v1 draft superseded in-place (git history preserves it). Origin: 2026-07-10
key-architecture thread (the owner: envelope encryption, self-describing objects,
proof re-derivation accepted).

## Decision record (the owner, settled)

1. Envelope encryption for the archive AEAD (offsite) copy: sealing host holds
   encrypt-only capability; decrypt lives air-gapped + escrowed. Justified by the
   restore-order policy: plaintext on-site pools serve daily restores; the AEAD
   copy is the rare disaster path where a human ceremony is acceptable.
2. Self-containment is a FORMAT property: the wrapped key travels with the object
   (tape + safe = data, no catalog) — sibling objects rejected (tape streaming +
   convention rot).
3. Lean proof re-derivation is accepted work, not a constraint.

## What the panel changed (v1 draft → v2)

The draft's fixed 256-byte header could not hold its own fields (337 B needed);
v1 already binds the full header hash into key derivation (making in-header
wrapped keys a chicken-and-egg unless ordering is specified); the sealing stack is
symmetric-only and deliberately deterministic (no OS RNG exists in the seal path);
and the recovery story collapsed at the last step (the restore adapter is
catalog-coupled). v2 therefore specifies an envelope subsystem, not a header
tweak.

## Format: v2 objects

**Layout:** `[128-byte scalar header][key frame][encrypted metadata frame]
[body chunks][footer]`.

- **Scalar header** stays 128 bytes. `format_version = 2`. Byte placement
  (verify-round-pinned against header.rs: free regions are 0x0C..0x10 and
  0x38..0x40; byte 0x07 is the payload cipher-suite id and is NOT free):
  `wrap_suite: u8 @ 0x38` (0x00 = registry-symmetric, 0x01 = HPKE-v1 below),
  `0x39..0x3C` reserved-zero, `key_frame_len: u32 big-endian @ 0x3C..0x40`.
  `key_frame_len = 0` with `wrap_suite = 0` is the registry-symmetric v2 form
  (reserved; not emitted in phase 1). **v2 field dispositions:** `hkdf_salt[16]`
  keeps its role (now derived from the DEK per the seal transcript below);
  `key_id[16]` is zero in envelope mode — recipient identity lives ONLY in the
  key frame (single source; mode rules already forbid registry fallback).
  Old readers fail cleanly on `format_version` (the header stays 128 bytes, so
  the length check never trips first).
- **Key frame** (variable length, plaintext, authenticated — see Seal ordering).
  Frozen grammar (all integers big-endian): magic `RAOK` (4 B), `slot_count: u8`
  (1..=8), then per slot in ascending `slot_index` order (duplicates forbidden):
  `slot_index: u8`, `recipient_epoch_id[16]`, `label_len: u8` (<=32) +
  `epoch_label` (printable ASCII, human-eyeball diagnosable), HPKE
  encapsulation `enc[32]` (X25519), wrapped-DEK ciphertext `ct[48]`
  (32-byte key + 16-byte tag). `key_frame_len` counts EVERY framing byte from
  the magic through the last slot; hard maximum 4096 bytes. Canonical encoding
  is byte-exact: ordering violations, duplicate slots, or trailing bytes are
  parse rejects. Byte-level test vectors are a deliverable. Array-shaped from
  day one (adding/rotating recipients never bumps the format).
- **Wrap suite 0x01 (frozen):** HPKE Base mode, DHKEM(X25519, HKDF-SHA256),
  HKDF-SHA256, ChaCha20-Poly1305 (RFC 9180), via the `rust-hpke` crate
  (RFC test-vector conformance; hand-rolling from `ring` primitives and libsodium
  rejected — audit burden / C dependency). HPKE `info` is a frozen
  fixed-width byte transcript: `"rao-wrap-v1\0"` (12 B) ‖ canonical NUL-padded
  `object_id_field[64]` ‖ `recipient_epoch_id[16]` ‖ `slot_index: u8` ‖
  `format_version: u8` ‖ `wrap_suite: u8` — a wrapped DEK cannot be
  transplanted between objects or slots, and the transcript is interoperable by
  construction.
- **Footer:** unchanged literal. v2 `inspect` geometry (direct generalization
  of inspect.rs; zero-fill is shorter than one chunk so it cannot raise the
  quotient): `n = floor((stored_size − 128 − key_frame_len −
  metadata_frame_len − 16) / (chunk_size + 16))`; `footer_offset = 128 +
  key_frame_len + metadata_frame_len + n × (chunk_size + 16)`.

**Seal ordering (normative — verify-round transcript, mirrors the v1 flow in
seal.rs/open.rs/kdf.rs):**
1. Generate the random DEK and the canonical metadata plaintext.
2. Derive `hkdf_salt` from the DEK:
   `rao2-salt-v1 ‖ ctr ‖ object_id_field[64] ‖ plaintext_digest[32] ‖
   SHA-256(metadata_plaintext)`.
3. Wrap the DEK to ALL configured recipient slots; serialize the canonical key
   frame.
4. Serialize the scalar header (contains the salt and `key_frame_len`).
5. `header_hash = SHA-256(scalar header ‖ key frame)`.
6. Derive object/metadata/payload keys from (DEK, salt, header_hash) under
   distinct `rao2-*` labels; encrypt metadata + chunks.
The key frame does NOT participate in `derive_salt` — its binding through
`header_hash` suffices. Opening mirrors v1: derive keys from the header salt +
hash, decrypt metadata, recompute and compare the salt. This preserves v1's
authentication mechanism (full binding through key derivation — the draft's
"add AAD" idea is dropped as redundant-or-weaker): any tamper of
version/suite/slots/key-frame bytes is an immediate decrypt failure. **Consequence: re-wrap without re-seal is
forbidden** — recipient rotation applies to newly sealed objects; old epochs'
private keys are retained per the retention policy below.

**Randomness (new subsystem requirement):** the seal path gains a fallible
OS-backed CSPRNG (the current path is deliberately deterministic). Seal fails
closed when entropy is unavailable. Fresh DEK per object; fresh encapsulation
randomness per slot. DEK and ephemeral secrets live in zeroizing, non-cloneable
containers (matching existing `RootKey` discipline).

**Mode rules:** expected mode comes from pool configuration/catalog; header
disagreement is a hard reject; there is NO fallback between registry and envelope
paths after a failure (mode-confusion finding). v2 envelope headers are emitted
ONLY by envelope pools — hdcache per-asset objects, the hot backup domain, and
all other paths keep emitting v1 (no header growth or offset churn on high-count
paths).

## Recipients, custody, lifecycle

- **≥2 distinct-custody recipient slots are MANDATORY** for envelope pools
  (offsite-safe epoch + escrow epoch). Seal fails closed if any configured slot
  fails to wrap. The catalog records the epochs each object was ACTUALLY wrapped
  to; dual-coverage is queryable.
- **Recipient epochs** (renamed from "generations" — collides with
  `KeyEpoch.generation` and `media_generation`): keypairs live in a NEW store
  (not `KeyEpoch`, which is 32-byte-symmetric-only); public keys + fingerprints
  pinned via an offline-approved list with drift alarms; a human-readable epoch
  registry (id → label, created/retired, physical key location) is escrowed and
  printed.
- **Retirement is hard-gated**: an epoch's private key may not be destroyed while
  any live object references it (verify-sweep parses v2 key frames and reports
  epoch coverage; the gate mirrors durability-floor enforcement). Retired-epoch
  private keys are retained in the safe, labeled, forever by default.
- **Escrow exports**: wrapped-key copies + the epoch registry + a catalog/locator
  snapshot ride every export; export staleness ("objects sealed since last
  successful export") is counted and alarmed. The export file format is stock
  **`age`** (owned, boring, distro-packaged) so recovery tooling exists even if
  nothing of ours survives.

## Recovery product (first-class artifacts)

1. **`rao-recover`** (new remanence bin): standalone
   `object bytes + private key file → plaintext member bytes`. Parses scalar
   header + key frame, enumerates slots, auto-selects the matching epoch,
   unwraps, derives, decrypts, verifies, extracts — zero catalog, daemon, or
   network. On mismatch prints actionable labels: "object wants epoch
   <label-A>/<label-B>; you supplied <label-C>". Handles v1 objects by
   dispatching on version (v1 needs the escrowed registry key instead).
2. **Escrowed toolchain**: the safe/escrow kit holds the static `rao-recover`
   binary AND a pinned source + toolchain image; drills periodically REBUILD from
   the archived source (a binary that won't launch on a 2036 OS is a documented
   drill failure with a remediation playbook).
3. **DR runbook** (versioned artifact, printed + escrowed): the numbered ceremony
   — locate (via the escrowed catalog snapshot + per-tape manifests: "which tape"
   must be answerable without the host) → fetch shares → reassemble passphrase →
   open export → unwrap on the air-gapped machine → transport DEK (tmpfs 0600,
   single restore scope) → extract → verify → shred DEK.
4. **Custody**: Shamir **2-of-3** passphrase shares with named custodians,
   documented succession/re-split on any departure; paper never holds a whole
   key on one sheet (registry + runbook on paper; key material only as split
   shares in separate custody).
5. **Drills**: heavy annual end-to-end (a first-timer restores a >1-quarter-old
   canary object from cold media using only the safe + runbook; pass = plaintext
   byte-match), light quarterly checks (tool boots, shares present, safe opens,
   canary header parses). Every drill reassembles the passphrase from the actual
   shares. Drill failure = STANDING-style escalation with named remediation
   playbooks and a due date.

## Threat model (time-scoped, honest)

| Attacker | Can read | Cannot read |
|---|---|---|
| Steals offsite v2 tapes | nothing | everything on them |
| Compromises archive host at time T | plaintext on-site pools; v1 objects (registry); future DEKs sealed after T (memory capture); can substitute future recipient keys absent the fingerprint pin | v2 objects sealed before T |
| Holds ONE recipient private key (incl. escrow custodian) | every object wrapped to that epoch — stated unilateral authority; threshold recovery is an optional extension | objects of other epochs |
| Holds recipient PUBLIC keys | nothing — but can fabricate internally-valid objects: catalogless recovery gives confidentiality + self-consistency, NOT provenance; an independently-signed manifest is an optional extension | — |

## Crash windows & reconciliation

Source bytes are retained until the footer is confirmed AND the object's wrapped
keys are durably recorded (catalog + next escrow export). Reconciliation GCs
wrapped-key records pointing at footerless/incomplete objects. Restore output is
staged and published only on complete success (per-frame fail-closed semantics
documented; not whole-stream atomic).

## Migration & rollout

- Census today: ~6 sealed AEAD bundles. **All existing archive AEAD copies
  migrating into envelope pools are re-sealed to v2 during rollout** (hdcache
  and other non-envelope domains keep v1 per the scope rule; their dev-seed
  exposure is closed by the dev-seed replacement arc, not this format) —
  near-zero cost now, and mandatory rather than optional because v1 archive
  roots derive from the publicly-known dev seed (v1-forever would escrow a
  public secret). Standing rule for the future:
  re-seal rides tape-generation migrations, never a dedicated pass.
- Rollout order: every reader/verify host upgrades to v2-capable BEFORE the first
  v2 object lands on shared media; multi-object tape readers skip-and-continue
  past unparseable objects (one v2 object must not strand a tape's v1 objects
  for an old reader).
- Capability signaling: new `rem archive capabilities` verb; sutradhara selects
  the v2 sealer only after probing. Old rem + v2 config = clean unsupported
  error, not a clap arg failure.
- Prerequisite arc: the dev-seed replacement (real random registry keys) ships
  with or before this — envelope objects escape the seed by construction, but
  hdcache/backup domains and the transition period still need it.

## Proof & test plan

- **Parametrize once**: `aead-framing` offsets generalize over (header len, key
  frame len) — prove `∀ H, K` instead of re-proving per size (pinned-literal
  inventory: `checked_sub(144)`, `assert_eq!(header_len, 128)`, each
  `rw [header_len_val]` site, drift-guard string matches — updated deliberately).
- `rao-header` kernel: v1/v2 disjoint parsing, key-frame roundtrip.
- Negative vectors: version/suite/mode flips, slot transplantation, added slots,
  epoch mismatch, malformed encapsulation, truncated key frame, both header
  geometries, mixed-tape verify sweep.
- Fuzz: header + key-frame parser over both versions.
- HPKE: RFC 9180 test vectors + our byte-level wrap vectors.

## Impact map

- **remanence (large)**: key frame + v2 header, HPKE wrap subsystem, OS-RNG
  introduction, `rao-recover`, `rem archive capabilities`, proofs/vectors/fuzz
  per plan, re-seal tooling for the 6-object migration.
- **sutradhara (medium)**: recipient-epoch store + pinned fingerprint list,
  envelope pool config + sealer selection via capability probe, catalog columns
  for actual-wrapped epochs (dedicated tables, never `storage_metadata`),
  escrow-export job + staleness alarm, verify-sweep epoch-coverage report,
  `key_domain()` default-fork fix.
- **ops (small but human)**: safe contents, Shamir shares, epoch registry
  printing, runbook authoring, drill scheduling (gardener).

## Open questions

1. **Resolved 2026-07-11:** pin `hpke = 0.13.0` exactly, with only its `std` and
   `x25519` features. It is the latest stable release compatible with the
   workspace MSRV, includes RFC 9180 known-answer tests, is pure Rust, and its
   primitive versions align with this workspace. The upstream crate has not
   received a formal independent audit (Cloudflare reviewed v0.8); therefore
   keep the exact Cargo.lock pin and vendor the complete locked dependency set
   in the escrowed source/toolchain image rather than maintaining a fork.
(Verify round 1 answered the former questions 1-2: byte offsets pinned in the
Format section; salt transcript pinned in Seal ordering.)
