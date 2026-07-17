# RAO v2-only: v1 excision + v2 productionization

**Status:** design v0.1 — pending panel review
**Date:** 2026-07-17
**Owner decision:** the owner, 2026-07-17 — remove format_version 1 (registry-symmetric
encryption) from the publication spec AND the implementation before tagging
v1.0.0 / minting the Zenodo DOI. Publication 1.0 is v2-only.

## 1. Why

v1 (registry-symmetric) encrypts every object under a long-lived symmetric root
key that must be present, in secret form, on the sealing host. v2 (HPKE
wrapped-DEK) seals to recipient *public* keys — no long-term secret on the
write path, per-object DEKs, and an escrow/custody story. v1's one
distinguishing property (deterministic sealing) is also a confidentiality
defect the spec itself concedes (ciphertext equality disclosure, §12.7 area).
No external implementer exists, the spec has never been published, and the
project is pre-production — the cost of keeping v1 is permanent (frozen into
the citable 1.0); the cost of removing it is bounded and paid once, now.

## 2. What the surveys established (2026-07-17, two Sonnet sweeps)

The deletion itself is clean, but **v2 was never wired into the production
write path**. Current state:

| Layer | State |
| --- | --- |
| `remanence-aead` | v1 and v2 coexist as separately-named functions (`seal`/`seal_envelope`, `open`/`open_envelope`, unsuffixed/`_envelope` range API) sharing version-agnostic framing (`stream.rs`, `metadata.rs`). One dual struct (`RaoHeader`, `key_id` v1-only) and one shared options struct (`SealOptions.key_id` vestigial for v2). |
| `remanence-format` | **v1-only, end to end.** `write_encrypted_rao_object*` → `seal_to_vec`; `read_encrypted_rao_object*` → `open_to_vec`; `pfr.rs` → v1 range fns. Zero v2 functions. |
| `remanence-api` / `remanence-state` / `remanence-parity` | `PoolWriteRepresentation::{Plaintext, Encrypted{root_key,key_id}}`; catalog `object_copies.key_id` column with nonzero-required validation; `BootstrapObjectRepresentation::Encrypted{key_id,..}`. No v2 variant anywhere. |
| `remanence-cli` | All archive verbs carry v1 flags (`--encrypt --key-file --key-id`). v2 is producible **only** via `reseal` (v1→v2, refuses non-v1 input) and readable via `rao-recover --private-key`. `capabilities` already reports v2-only strings. |
| Vectors | `tools/verify_rao_vectors_independent.py` (the independent Python re-implementation) has **zero HPKE support**; the tar's only positive envelope vectors (TV-E1, TV-D1-encrypted) are v1. `negative-envelope-v2.json` is v2 and survives. No deterministic v2 seal hook is publicly reachable (`EphemeralRng::from_seed` is module-private; `seal_envelope` always uses OS randomness). |
| Proofs (`verif/`) | `aead-framing`, `rao-header`, `rao-archive` prove **v1 geometry only**; drift guards string-pin exact v1 source lines (`"if !matches!(format_version, 1 | 2)"`, `"if self.key_id == ZERO_16"`). v2 formal coverage is an open follow-up (RAO-V2-FORMAL-PREFIX / -HEADER-KEY-FRAME). |
| sutradhara | Everything funnels through `run_rem_archive_build()` + `RaoCliSealer`/`RaoCliOpener`; `KeyRegistry` is symmetric-only, dev-seed-derived (STANDING escalated item); `Copy.storage_metadata["key_epoch"]` is a single flat string, written in 2 places, read fail-closed in 2 places. hdcache uses the identical v1 seal in its own key domain (frozen hd-disk-tier design). |
| system harness | Duplicate registry seam (`harness/seams/keys.py`), v1 flags in `harness/seams/rao.py`, ~15 scenarios carrying v1 shapes, `bindings.toml` rows for root-key ops. |

Consequence: "remove v1" = **productionize v2 first, then excise v1** — one
tree, not separable, because deleting v1 breaks compilation of every layer
that only speaks v1.

## 3. Design decisions

**D1 — The v2 wire format is byte-invariant.** Spec §5.2 already requires
`key_id` (bytes `0x10..0x20`) all-zero for v2. Excision re-describes that
region as `reserved` (MUST be zero) and marks `format_version = 1`
**permanently reserved — never reassigned** (§10). `RAO1` magic, 128-byte
header, key frame, and all v2 test vectors are unchanged. Nothing sealed as
v2 before this change becomes invalid.

**D2 — `reseal` is deleted, not repurposed.** Its only valid input class (v1
objects) ceases to exist, and §12.8 already forbids rewrapping a key frame
without resealing (the frame is covered by `header_hash`). Rotation remains
what the spec says it is: re-seal (open + seal_envelope → new DEK, new
`stored_digest`), driven by the orchestrator. `rao-recover` drops its
`--registry-key`/v1 leg and keeps `--private-key`.

**D3 — v2 production path, single-funnel.** `remanence-format` gains
envelope writer/reader/PFR functions that **wrap `seal_envelope` /
`open_envelope` / `*_envelope` range fns — no parallel crypto, no copied
framing code**. `PoolWriteRepresentation::Encrypted` changes shape to carry
recipient public keys (≥2, per the spec's two-slot floor). The state schema
replaces the single `key_id` column semantics with the object's recipient
epoch id list; the parity bootstrap encrypted-row carries what the
publication spec already specifies for v2 rows. Old shapes are **replaced,
not versioned** — pre-production, no compat branches (hard rule). The
`_envelope` suffixes are dropped in the same pass (`open_envelope` → `open`,
etc.): with one version left, the suffix is noise.

**D4 — CLI surface.** `archive build`/`write`/`pool write` gain repeatable
`--recipient <public-key file>` (enforcing the ≥2-slot floor at arg
validation); `extract`/`extract-stream`/`covering-range`/`read`/
`export-object` replace `--key-file` (root key) with `--private-key`;
`inspect` stays keyless (reports recipient epoch ids instead of `key_id`).
`capabilities` output is already correct.

**D5 — Positive v2 vector (RAO-TV-E2) via deterministic-seal hook.**
Sealing is intentionally randomized, so the vector strategy is:
(a) add one narrow, clearly-labeled deterministic entry point for vector
generation — same `seal_envelope` internals with an injected `EphemeralRng`
seed and fixed DEK (`DataEncryptionKey::from_bytes` already exists) — so the
pinned artifact is *regenerable byte-exactly*; (b) pin the sealed object +
full derivation chain in the fixture manifest; (c) extend
`verify_rao_vectors_independent.py` with a v2 **open-direction** verifier
(X25519 + HKDF + ChaCha20-Poly1305 via the Python `cryptography` package) so
independent verification is preserved — the independent implementation
proves it can *read* the pinned artifact and recover byte-exact plaintext,
which is what a reader-conformance vector requires. `negative-envelope.json`
is re-based: the ~40 version-agnostic header/metadata/footer cases move onto
a v2 base object; the v1-specific cases (`all-zero-key-id`, `root-key-*`,
v1-salt cases) are dropped. TV-D1's encrypted half is re-based to v2.
Then `make publication-test-vectors`, and the new tar SHA-256 is re-pinned
in **both** publication specs.

**D6 — Proof estate: cheap-honest path, no new Lean derivation** (standing
rule). Proofs whose pinned source survives verbatim stay untouched. Proofs
that prove *deleted* v1 behavior are **retired**: drift guard removed with
the code it pins, `verif/STATUS.md` and `docs/formal-verification-status.md`
updated to record the retirement and the reason ("v1 removed from the
format; proof preserved in git history"), and the already-named follow-ups
(RAO-V2-FORMAL-PREFIX, RAO-V2-FORMAL-HEADER-KEY-FRAME) remain the v2 proof
targets. `make proof-inventory` must be green *and truthful* after.

**D7 — sutradhara registry pivot (closes the dev-seed STANDING row).**
`KeyRegistry` mints X25519 recipient **keypairs** per epoch per domain
(`archive`, `hdcache`, and the new `backup` domain), with **random-at-mint
via OS entropy**. The hard-coded `_TEST_SEED` derivation survives only
behind an explicit test switch (env/constructor), used by hermetic scenarios
— production default is random. `materialized_root_key` becomes
`materialized_private_key` (same 0600-temp-file discipline) for the open
path; the seal path passes public-key files (no secret). `storage_metadata`
carries the recipient epoch id list (replaces the single `key_epoch`
string); both writer sites and both fail-closed reader sites change in
lockstep. Existing pilot cloud blobs are **regenerated** by the normal job
path from the plaintext copies — derived artifacts, no reseal, no compat.
The `consult-backup-ledger-keys-2026-07-10.md` recommendation (extend v1
with a backup domain) is superseded by this design.

**D8 — hdcache pivots to v2 with a hot private key.** The frozen
hd-disk-tier design keeps cache decrypt unattended on the serving host; v2
preserves that by holding the hdcache-domain recipient private key hot on
that host — identical exposure to today's hot root key, same domain
isolation (cross-domain refused at seal), only the envelope changes. This is
an explicit amendment to the frozen design's letter, preserving its intent;
the hdcache prompt set (M1–M6) consumes the same `RaoCliSealer`/`Opener`
interfaces and is unaffected beyond the key-material type.

**D9 — harness.** `harness/seams/keys.py` mirrors the keypair contract;
`harness/seams/rao.py` mirrors the new flags; `bindings.toml` gains/renames
the seam rows; affected scenarios (~15) update mechanically (epoch ids stay
16-byte identifiers; root-key files become keypair files; `scenario_lbk`
moves to the `backup` domain). The RAOE envelope scenario
(`docs/historical/prompt-scenario-rao-v2-envelope.md`) is cut into a real
hermetic scenario as the harness-side verification member; its live-tape
legs remain post-merge smoke items for the next MSL3040 window.

**D10 — v2 fuzz coverage lands with the excision.** The whole-object fuzz
target (`rao_whole_object_open_verify`) is ported to `open_envelope` with a
fixed recipient key; deleting v1 without this leaves whole-object open/verify
fuzzing at zero.

## 4. Work breakdown → prompt set

| Prompt | Repo | Content | Depends on |
| --- | --- | --- | --- |
| **P1 rem-core** | remanence | W1 aead excision + renames + deterministic-seal hook + fuzz port; W2 v2 production path (format/api/state/parity/CLI flags, delete `reseal`, `rao-recover` v1 leg); W6 proof-estate retirement; repo docs (reference-cli, quickstart, glossary, architecture-overview, tape-layout + SVG caption, extract-stream protocol). | — |
| **P2 rem-vectors+spec** | remanence | W3 RAO-TV-E2 + Python v2 open-verifier + negative re-base + TV-D1 re-base + tar regen; W4 publication-spec excision (both docs) + SHA re-pin. | P1 |
| **P3 sutra-pivot** | sutradhara | W7: registry keypairs (random-at-mint + test switch), sealer/opener → new CLI flags, metadata shape, all callers, hdcache + backup domains, pilot blob regeneration path. | P1 |
| **P4 harness** | system | W8: seams, bindings, scenario sweep, RAOE hermetic scenario (verification member). | P1, P3 |

P2 ∥ P3 after P1. P4 last. Verification gates: P1 = workspace tests +
`make proof-inventory` green; P2 = vector build + independent verifier pass +
publication-doc grep gate (zero hits for `format_version = 1` /
`registry-symmetric` / root-key language); P3 = sutradhara pytest; P4 =
`make suite` from clean slate. Each prompt carries the standard preamble
(single-funnel, golden fixtures, wrap-don't-copy, no compat/backout paths).

## 5. Out of scope (follow-ups filed, not built now)

- RAO-V2-FORMAL-* Lean proofs (existing named follow-ups; no derivation now).
- KMS/HSM-backed key storage — 1.0 posture is OS-random local keypairs with
  0600 discipline; custody ceremony (Shamir escrow of recipient private
  keys) is the existing escrow thread, unblocked (not implemented) by D7.
- A convenience `rotate`/re-seal orchestration verb — rotation is
  open+seal by the orchestrator for now (§12.8 semantics).
- Live-tape RAOE legs (next MSL3040 window).

## 6. Publication sequencing

P1 → P2 land → tag `v1.0.0` → Zenodo DOI (org-only creator) → PRONOM
submission (unchanged: signature keys on `RAO1` magic + `rao-v1` pax keyword,
both untouched; one header form makes the signature note simpler). P3/P4 are
required for **suite-green main**, and land before the tag so the DOI
snapshot is coherent across the stack the README describes.
