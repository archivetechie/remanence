# RAO v2-only: v1 excision + v2 productionization

**Status:** design v0.3 — panel-folded + codex verify round folded (2026-07-17); ready for prompt cutting
**Date:** 2026-07-17
**Owner decision:** the owner, 2026-07-17 — remove format_version 1 (registry-symmetric
encryption) from the publication spec AND the implementation before tagging
v1.0.0 / minting the Zenodo DOI. Publication 1.0 is v2-only.
**Panel:** L1 crypto/format = GLM 5.2 (packet-grounded), L2 systems = Kimi
K2.7 (packet-grounded), L3 orchestrator/ops = Opus (repo-grounded). All three
folded below; fold notes in §7.

## 1. Why

v1 (registry-symmetric) encrypts every object under a long-lived symmetric root
key that must be present, in secret form, on the sealing host. v2 (HPKE
wrapped-DEK) seals to recipient *public* keys — no long-term secret on the
write path, per-object DEKs, and an escrow/custody story. v1's one
distinguishing property (deterministic sealing) is also a confidentiality
defect the spec itself concedes (ciphertext equality disclosure). No external
implementer exists, the spec has never been published, and the project is
pre-production — the cost of keeping v1 is permanent (frozen into the citable
1.0); the cost of removing it is bounded and paid once, now.

## 2. What the surveys established (2026-07-17, two Sonnet sweeps + L3 verification)

The deletion itself is clean, but **v2 was never wired into the production
write path**. Current state:

| Layer | State |
| --- | --- |
| `remanence-aead` | v1 and v2 coexist as separately-named functions sharing version-agnostic framing (`stream.rs`, `metadata.rs`). One dual struct (`RaoHeader`, `key_id` v1-only) and one shared options struct (`SealOptions.key_id` vestigial for v2). `wrap_dek` hardcodes `EphemeralRng::from_os()` internally — no injection seam. |
| `remanence-format` | **v1-only, end to end.** Zero v2 functions. |
| `remanence-api` / `remanence-state` / `remanence-parity` | `PoolWriteRepresentation::{Plaintext, Encrypted{root_key,key_id}}`; catalog `object_copies.key_id` column with nonzero-required validation; `BootstrapObjectRepresentation::Encrypted{key_id,..}`. No v2 variant anywhere. |
| `remanence-cli` | All archive verbs carry v1 flags. v2 is producible **only** via `reseal` (v1→v2, refuses non-v1 input, already enforces 2–8 distinct recipients at `lib.rs:3835-3846`) and readable via `rao-recover --private-key`. `inspect` accepts `--key-file` and reports `key_id`; the reseal report already emits `recipient_epochs` (`lib.rs:3928`). `capabilities` already reports v2-only strings. |
| Vectors | Independent Python verifier has **zero HPKE support**; the tar's only positive envelope vectors are v1. `negative-envelope-v2.json` survives. No public deterministic v2 seal hook. |
| Proofs (`verif/`) | `aead-framing`, `rao-header`, `rao-archive` prove **v1 geometry only**; drift guards string-pin exact v1 source lines. v2 formal coverage is an open follow-up. |
| sutradhara | Seal funnels: `run_rem_archive_cli.py::run_rem_archive_build` + `RaoCliSealer/Opener`. **Three** seal-time root-key materialization sites (`sealing/rao.py:104`, `archive_fanout.py:407`, `jobs/handlers/cloud_blob.py:194`) plus open-time (`archive_fanout.py:489`, three `archive_restore.py` call sites, `restore.py` via gRPC). **Three** writers of `storage_metadata["key_epoch"]` (`replication.py:679`, `archive_fanout.py:901`, `cloud_blob.py:134`) and **three** fail-closed readers (`archive_fanout.py:1289`, `archive_restore.py:1900`, `restore.py:259`). `inspect_rao` hard-requires `report["key_id"]` (`sealing/rao.py:204-208`). Registry validators hardcode two domains (`KEY_DOMAINS`, prefix-based `key_domain()`, `_validate_key_id`). hdcache uses the identical v1 seal in its own hot key domain (frozen hd-disk-tier design §12.10; LUKS FDE below it is a separate, unaffected layer). |
| system harness | Duplicate registry seam (`harness/seams/keys.py`), v1 flags in `harness/seams/rao.py:213-218`, `bindings.toml:58-67` seam rows, **`scenarios/sealing_support.py` — the shared assertion library (require_key_id, require_keyless_rao_header, wrong_root_key_file) that the load-bearing scenarios import** — plus 16 scenario files. |

Consequence: "remove v1" = **productionize v2 first, then excise v1** — one
tree, not separable, because deleting v1 breaks compilation of every layer
that only speaks v1.

## 3. Design decisions

**D1 — The v2 wire format is byte-invariant.** Verified by L1 against
`header.rs`: the v2 `validate()` arm already rejects nonzero `key_id`;
`serialize()` writes zeros there for v2; the only acceptance-set change is
rejecting `format_version == 1`. Excision re-describes bytes `0x10..0x20` as
`reserved` (MUST be zero) and marks `format_version = 1` **permanently
reserved — never reassigned** (§10). `RAO1` magic, 128-byte header, key
frame, and all **positive** v2 test vectors are unchanged. One negative
expectation re-pins (verify V-6): the `v2-version-flip` case currently
expects `ReservedBytesNotZero` because the parser accepts version 1 before
interpreting v1 reserved bytes; with version 1 rejected at the gate, the
same bytes produce `UnsupportedFormatVersion`.

**D2 — `reseal` survives, redefined v2→v2 (verify-round reversal).** The
verify pass established that `extract` + `build` is a *rebuild* (fresh
object/manifest ids, regenerated canonical stream → different
`plaintext_digest`), while §12.8's rotation semantics promise a re-seal that
**preserves the canonical bytes, `object_id`, `chunk_size`, and
`plaintext_digest`** — exactly what the current reseal implementation does
(`lib.rs:3862` copies them into the new seal options). So the verb is
retained with its input leg swapped: input = a **v2** object +
`--private-key` (unwrap DEK path), output = the same canonical bytes sealed
to a new recipient set (new DEK, new salt, new `stored_digest`). This is
spec-legal — §12.8 forbids *rewrap-without-reseal*; this IS a re-seal. Only
the v1 input leg and `--registry-key` die. `rao-recover` drops its v1 leg.
The recipient handling (2–8 distinct epochs, RAOR parsing) is additionally
**lifted into `archive build`**.

**D3 — v2 production path, single-funnel.** `remanence-format` gains
envelope writer/reader/PFR functions that **wrap `seal_envelope` /
`open_envelope` / range fns — no parallel crypto, no copied framing code**;
the CLI's build/extract/read paths and `remanence-api::pool_write` route
through them (the reseal path's hand-rolled envelope orchestration dies with
the verb). `PoolWriteRepresentation::Encrypted` carries recipient public
keys. The **≥2-slot floor is enforced in the library (`seal_envelope`) AND
at CLI arg validation** — API callers and vector generators cannot emit
single-slot objects. Two data axes are distinct and both change shape:
the *write-policy axis* (pool → recipient set to seal new writes to;
`PoolTarget.key_epoch` / `PlacementTarget`) and the *record axis* (copy →
recipient epoch id list actually sealed; `storage_metadata`). The state
schema replaces `object_copies.key_id` semantics with the recipient epoch id
list (non-empty required for encrypted rows);
`BootstrapObjectRepresentation::Encrypted` extends to carry the epoch ids
and `key_frame_len` under **new** CBOR keys (the old `key_id` tag is
retired, never reused). Old shapes are **replaced, not migrated** —
pre-production, no compat branches (hard rule). The `_envelope` suffixes are
dropped in the same pass.

*Recipient identity — three distinct names, pinned (verify V-3):*
- **wire `recipient_epoch_id`**: the `[u8;16]` in the key-frame slot
  (`wrap.rs:111`) — canonical identity of an epoch on disk/tape.
- **registry epoch id**: the domain-prefixed string
  `<domain>-<32hex>`; its 32-hex payload IS the wire id, hex-encoded.
  `key_domain()` derives the domain from the prefix (no default-to-archive).
- **wire `epoch_label`**: human-readable slot label; set to the registry
  epoch id string at seal time.
Durable encodings: CLI report JSON emits
`recipient_epochs: [{"epoch_id": "<32hex>", "label": "<registry id>"}]`;
sutradhara `storage_metadata["recipient_epochs"]` is a JSON array of
registry epoch id strings; the state schema stores the same array
(JSON-encoded) in place of `key_id`; bootstrap CBOR carries an array of
16-byte byte-strings.

*Reader slot policy (verify Q4):* the ≥2-slot floor is a **Sealer**
obligation; Readers accept any structurally valid key frame with ≥1 slot
(robustness: a foreign one-slot object still opens). Negative vectors
reject 0 slots and >8; a positive one-slot read-acceptance vector documents
the asymmetry.

**D4 — CLI surface (a contract change, not a flag rename).**
`archive build`/`write`/`pool write` gain repeatable `--recipient
<public-key file>` (2–8, distinct epochs — semantics lifted from reseal).
**`--encrypt` is removed: the presence of `--recipient` IS the encryption
switch** (verify V-5; one knob, no ambiguity).
`extract`/`extract-stream`/`covering-range`/`read`/**`verify`** replace
`--key-file` with `--private-key` (`export-object` exports stored bytes and
carries no key — it was wrongly listed before; `verify` was wrongly
omitted). **`inspect` drops `--key-file` entirely**, parses the key frame,
and reports `recipient_epochs` + `format_version` instead of `key_id`.
Build/write **report JSON schemas change in lockstep** (per the D3 encoding)
and `docs/reference-extract-stream-protocol.md` is updated (epoch selection
by private key). `capabilities` output is already correct.

**D5 — Positive v2 vector (RAO-TV-E2) via deterministic-seal refactor.**
`wrap_dek` currently constructs its RNG internally; it is **refactored to
accept `rng: &mut R` as a parameter** — the production caller passes
`EphemeralRng::from_os()`, the vector path injects a seeded RNG and a fixed
DEK (`DataEncryptionKey::from_bytes`). Same internals, one signature change,
no parallel path. The pinned RAO-TV-E2 artifact is regenerable byte-exactly;
the fixture manifest pins the full derivation chain. Independent
verification: `verify_rao_vectors_independent.py` gains a v2 **open-direction**
verifier (X25519 + HKDF + ChaCha20-Poly1305 via Python `cryptography`) that
decrypts the pinned artifact and recovers byte-exact plaintext.
`negative-envelope.json`'s version-agnostic cases re-base onto a v2 object;
v1-specific cases drop (`all-zero-key-id`, `key-id-swapped`,
`root-key-too-short`; `reserved-bytes-nonzero` re-targets v2 reserved bytes
`0x39..0x3B`). v2-specific negative coverage extends
`negative-envelope-v2.json` per the verify-round Q4 list: structurally-valid
key-frame tampering (label, encapsulation, wrapped-DEK ciphertext, slot
insertion/removal), slot counts 0 and 9 + writer rejection of 0/1/>8,
one-slot **read acceptance** (D3 reader policy), known-suite mismatches
(suite 0 with nonempty frame; HPKE suite with zero/undersized
`key_frame_len`), duplicate `recipient_epoch_id` across distinct slots
(parser must reject, not just sealer), internal slot truncation →
`InvalidKeyFrame`, nonzero v2 reserved `key_id`, malformed key-frame magic,
wrong recipient private key, malformed encapsulation. TV-D1's encrypted
half re-bases to v2. Then `make publication-test-vectors` and the new tar
SHA-256 re-pins in **both** publication specs.

**D6 — Proof estate: cheap-honest path, no new Lean derivation** (standing
rule). A proof survives ONLY if its drift-guard-pinned snippets are
**byte-identical** after the edits; anything else is retired: drift guard
removed with the code it pins, the crate removed from the
`make proof-inventory` build list (`verif/check-inventory.sh`),
`verif/STATUS.md` + `docs/formal-verification-status.md` updated with the
retirement reason ("v1 removed from the format; proof preserved in git
history"), and RAO-V2-FORMAL-PREFIX / -HEADER-KEY-FRAME remain the named v2
follow-ups. Expected outcome: `aead-framing`, `rao-header`, `rao-archive`
retire; `rao-metadata`, `rao-manifest`, `parity-state` (zero v1 coupling)
survive untouched. `make proof-inventory` must be green *and truthful*.

**D7 — sutradhara registry pivot (closes the dev-seed STANDING row).**
`KeyRegistry` mints X25519 recipient **keypairs** per epoch per domain, with
**random-at-mint via OS entropy**. Full enumerated blast radius (L3-verified):

- *Domains:* `archive`, `hdcache`, new `backup`, new `recovery` (D11). Adding
  domains requires lockstep edits to `KEY_DOMAINS` (`registry.py:25`),
  `key_domain()` (`:273-278` — prefix table, no more "default archive"),
  and `_validate_key_id()` (`:303-315`).
- *Mint/read:* the root-material re-derivation mismatch check
  (`registry.py:79-80`) is deleted — random keys cannot be re-derived.
  `materialized_root_key` → `materialized_private_key` (same 0600-temp
  discipline); a non-secret public-key accessor serves the seal path.
- *Deterministic test mode — three interlocks, never env-alone:*
  (1) explicit constructor flag (`deterministic_test=True`) that production
  factory code never passes; (2) **path interlock** — refuse deterministic
  mode unless `registry_dir` resolves outside the production root (reject
  `_DEFAULT_REGISTRY_DIR` and anything under `/var/lib`); (3) every
  deterministic epoch is self-identifying in its state file so a scrub/CSO
  scan can flag one that reached production.
- *Seal sites (root-key file → recipient public keys):* `sealing/rao.py:104`,
  `archive_fanout.py:407`, `cloud_blob.py:194`.
- *Open sites (root-key file → private key + epoch selection):*
  `archive_fanout.py:489`, `archive_restore.py` ×3 (extract,
  extract-stream, covering-range), `restore.py`/gRPC path, hdcache
  manager/walker/repopulate.
- *Metadata (single string → epoch id list + selection):* writers
  `replication.py:679`, `archive_fanout.py:901`, `cloud_blob.py:134`;
  readers `archive_fanout.py:1289`, `archive_restore.py:1900`,
  `restore.py:259`. Readers gain **selection logic** — choose the recipient
  epoch whose private key this host holds (by domain), fail closed when none
  matches.
- *Inspect contract:* `inspect_rao` (`sealing/rao.py:185-209`) switches from
  `report["key_id"]` to `report["recipient_epochs"]`; `RaoInspection`
  consumers update.
- *Pilot data:* regenerate via the **documented pristine-wipe path**
  (`runbook-pilot-ingest.md`: remove `pilot.db` + stale `.rao` outputs,
  re-ingest) — the cloud-blob job's idempotency guard (`already_copied`
  short-circuit, `cloud_blob.py:72-80`) means "re-run the job" alone
  regenerates nothing. The re-ingest doubles as end-to-end verification of
  the new seal path.
- The `consult-backup-ledger-keys-2026-07-10.md` recommendation (extend v1
  with a backup domain) is superseded by this design.

**D8 — hdcache pivots to v2 with a hot private key.** The frozen
hd-disk-tier design (§12.10) isolates cache keys because they are hot by
design; v2 preserves exactly that — the hdcache-domain recipient private key
is held hot on the serving host, same 0600-temp lifecycle, same domain
isolation at seal time. Honest deltas stated: each open adds one X25519 DH +
HKDF unwrap (microseconds; the CLI subprocess spawn dominates), and the
LUKS-FDE layer beneath the cache disks is orthogonal and unaffected. This
amends the frozen design's letter while preserving its intent; the hdcache
M1–M6 prompt set consumes the same `RaoCliSealer`/`Opener` interfaces.

**D9 — harness.** `harness/seams/keys.py` mirrors the keypair contract
(public-key materialization for seal, private-key for open);
`harness/seams/rao.py` mirrors the new flags; `bindings.toml` seam rows
update. **`scenarios/sealing_support.py` changes first** — it is the shared
assertion library the load-bearing scenarios import. Load-bearing (rewritten
+ green as verification members): `scenario_rao`, `scenario_rao_archive`,
`scenario_rao_archive_bundling`, `scenario_lbk` (moves to `backup` domain),
`scenario_q`, `scenario_o`, `scenario_arrangement_submit_live`,
`scenario_rao_live` (source rewritten now — its `key_id` assertions and
root-key materialization are v1; live-tape *execution* stays deferred), plus
the new hermetic RAOE scenario. Incidental literal-fixture updates: `scenario_ag`,
`scenario_n`, `scenario_bsh`, `scenario_retention_gate`,
`scenario_rao_archive_policy`, `scenario_arrangement_submit`,
`scenario_pfr`, `scenario_restore_agent`. Live-tape RAOE legs remain
post-merge smoke items for the next MSL3040 window.

**D10 — v2 fuzz coverage lands with the excision (verify V-7 shape).** The
whole-object target ports to `open_envelope` with a fixed recipient key AND
gets seeded with a valid v2 object sealed to that exact key (generated via
the D5 deterministic hook) — without the seed, fuzzing dies at
header/key-frame rejection and never reaches the metadata/payload/footer/
digest paths v1's corpus exercised (current corpus: 195 v1 seeds, zero v2).
A **direct `KeyFrame::parse` fuzz target** is added (the header target only
reaches it behind a valid 128-byte header, and its whole corpus is <128
bytes), seeded with one-, two-, and eight-slot valid frames plus
malformed/truncated variants. The old v1 corpus is migrated/minimized.

**D11 — recovery epoch: the mandatory second recipient, everywhere.** The
spec's ≥2-slot floor collides with single-hot-key domains (hdcache, backup)
unless the second slot is defined. Resolution: a dedicated **`recovery`
domain** whose epoch public key is included as the second recipient in
**every** seal across all domains. Custody (verify V-4 fix): recovery
epochs are **offline-minted** — a `sutra admin keys mint-recovery` step run
on an operator/offline machine emits the keypair; only the **public half is
imported** into the serving-host registry (a new public-only epoch kind the
registry must support — no private material ever written under
`registry_dir` for this domain); the private half goes directly to escrow
(the existing Shamir 2-of-3 custody thread now has a concrete key object).
Loss/rotation: mint a new recovery epoch offline, import its public half;
subsequent seals use it; existing objects re-seal opportunistically via the
D2 verb (pre-production: acceptable to defer). This satisfies the floor
uniformly and makes every object recoverable if a hot domain key is lost.

**D12 — red-main window policy.** Between P1 landing (v1 flags gone from
`rem`) and P3/P4 landing, real-seam AEAD scenarios are necessarily red — no
compat shims (hard rule). Mitigation: P1→P4 land as one coordinated push
(codex prompts dispatched back-to-back, suite re-run at the end); the
transient RAO reds during the window are expected arc-state, **not**
STANDING-escalation material — GAPBOARD annotation carries a pointer to this
design until P4 lands. Nightly suite results from inside the window are
disregarded.

## 4. Work breakdown → prompt set

P1 is staged **additive-first** (verify V-1 restage: deleting the v1 API
first breaks every consumer, so no per-stage green checkpoint is possible in
excision order; the "no parallel paths" rule binds the FINAL state, not
intermediate commits). One codex dispatch, **three mandatory checkpoint
commits**, whole tree compiles + tests green at each:

| Stage | Content | Checkpoint |
| --- | --- | --- |
| P1-ADD | Add v2 everywhere, delete nothing yet: `wrap_dek` RNG-injection refactor + deterministic-seal entry (aead); `remanence-format` v2 writer/reader/PFR (wrapping aead); `remanence-api`/`state`/`parity` v2 representation + schema + bootstrap row (D3 encodings); CLI `--recipient` on build/write/pool-write, `--private-key` on extract/extract-stream/covering-range/read/verify, keyless inspect + `recipient_epochs` report, reseal v2-input leg | full workspace tests |
| P1-EXCISE | Delete v1 in one slice across all layers: aead v1 fns + `RaoHeader.key_id` field (region → reserved-zero) + `_envelope` renames; format v1 fns; api/state/parity v1 shapes; CLI v1 flags (`--encrypt`, `--key-file`, `--key-id`), reseal v1 leg, `rao-recover --registry-key`; v1 fixtures/tests deleted or re-based; fuzz port + seeds + new `KeyFrame::parse` target (D10) | full workspace tests + fuzz targets build |
| P1-RETIRE | Proof retirement (guards + `check-inventory.sh` build list + STATUS + formal-verification doc) + repo docs (reference-cli, quickstart, glossary, architecture-overview, tape-layout + SVG caption, extract-stream protocol, amber-architecture → historical note) | `make proof-inventory` + full workspace |

| Prompt | Repo | Content | Depends on |
| --- | --- | --- | --- |
| **P1 rem-core** | remanence | P1a–P1e above | — |
| **P2 rem-vectors+spec** | remanence | RAO-TV-E2 + Python v2 open-verifier + negative re-base + TV-D1 re-base + tar regen; publication-spec excision (both docs) + SHA re-pin. Spec-gate greps include: `format_version = 1`, `registry-symmetric`, `root key`, `MUST implement v1`, `no test-only`, plus §13.3's determinism sentence and §14's conformance clause rewritten by hand. | P1 |
| **P3 sutra-pivot** | sutradhara | D7 + D8 + D11 in full (enumerated sites above); pilot pristine-wipe + re-ingest as the closing verification step. | P1 |
| **P4 harness** | system | D9: `sealing_support.py` first, seams, bindings, load-bearing scenario rewrites, incidental fixture updates, RAOE hermetic scenario. | P1, P3 |

P2 ∥ P3 after P1. P4 last, then `make suite` from clean slate. Each prompt
carries the standard preamble (single-funnel, golden fixtures,
wrap-don't-copy, no compat/backout paths).

## 5. Out of scope (follow-ups filed, not built now)

- RAO-V2-FORMAL-* Lean proofs (existing named follow-ups; no derivation now).
- KMS/HSM-backed key storage — 1.0 posture is OS-random local keypairs with
  0600 discipline; the recovery-epoch escrow ceremony (Shamir) is the
  existing custody thread, now with a concrete key object.
- A convenience rotation verb — rotation is open+seal by the orchestrator.
- Live-tape RAOE legs (next MSL3040 window).

## 6. Publication sequencing

Pinned order (verify V-9): **P1 → (P2 ∥ P3) → P4 → clean-slate `make suite`
green → tag `v1.0.0` → Zenodo DOI (org-only creator) → PRONOM submission.**
The tag waits for the full stack; the DOI snapshot must be coherent across
everything the README describes. (PRONOM signature keys on `RAO1` magic +
`rao-v1` pax keyword, both untouched by this arc.)

## 7. Panel fold record (2026-07-17)

Accepted: L1-1 (D1 verified sound), L1-2 (`wrap_dek` refactor → D5), L1-3/L1-4
(spec-gate misses → P2 gate expanded, §13.3/§14 hand-rewrites), L2-1 (funnel
routed through remanence-format, reseal orchestration dies), L2-2 (schema +
bootstrap shape; *migration* clause rejected — pre-production, replace not
migrate), L2-3 (floor in library + CLI), L2-4 (proof-inventory build list;
byte-identical survival criterion), L2-5 (P1 staging), L2-6 (inspect/report
contract + protocol doc), L2-8 (seams lockstep), L3-1 (3×3 metadata sites +
selection logic — the fold's biggest correction), L3-2 (seal/open site
enumeration + inspect_rao), L3-3 (domain validator lockstep), L3-4
(deterministic-mode triple interlock), L3-5 (→ D11 recovery epoch; cost +
LUKS honesty), L3-6 (two axes), L3-7 (sealing_support linchpin + load-bearing
list), L3-8 (→ D12 red-window policy), L3-9 (pilot pristine-wipe).
Rejected: L2-2's "provide a migration for existing rows" (violates the
pre-production hard rule).

**Verify round (codex gpt-5.6-sol, 2026-07-17): BLOCK → folded into v0.3.**
V-1 (staging restage → additive-first P1-ADD/EXCISE/RETIRE), V-2 (**reversal
of v0.2's D2**: reseal retained as v2→v2 — extract+build is a rebuild and
cannot preserve `plaintext_digest`), V-3 (recipient identity triple pinned +
durable encodings), V-4 (recovery epochs offline-minted, public-only import),
V-5 (CLI matrix: `verify` in, `export-object` out; `--encrypt` removed),
V-6 (v2-version-flip negative re-pins to `UnsupportedFormatVersion`),
V-7 (fuzz seeds + direct KeyFrame target), V-8 (`scenario_rao_live` source
into P4 scope), V-9 (sequencing pinned P1→(P2∥P3)→P4→suite→tag). Verify
also answered the three questions GLM dropped: Q2 not-expressible-without-
reseal (→ V-2), Q4 missing-case list (→ D5), Q6 inadequate-without-seeds
(→ D10), and surfaced the one-slot reader-policy decision (→ D3: Sealer
floor, Reader ≥1).
