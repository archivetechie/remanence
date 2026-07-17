# P1 rem-core — RAO v2-only: productionize v2, excise v1

Do NOT read or execute any files under ~/.claude/, ~/.agents/, .claude/skills/, or agents/.

**Contract:** `docs/design-rao-v2-only.md` (v0.3, panel-reviewed + verify-round
folded). Read it in full first. It is the single source of truth for every
design decision below; this prompt only sequences the work and pins the
guardrails. Where this prompt and the design disagree, the design wins.

**Repo:** this repo (`~/remanence`) only. P2 (vectors + publication specs), P3
(sutradhara), P4 (harness) are separate prompts — do not touch
`specs/publication/`, `fixtures/rao/*.json` vector *manifests* beyond what
P1-EXCISE requires for compiling tests, or anything outside this repo.

## Guardrails (non-negotiable)

- **Single funnel / wrap-don't-copy:** the new `remanence-format` v2
  writer/reader/PFR functions WRAP `remanence-aead`'s `seal_envelope` /
  `open_envelope` / range functions. No re-implemented crypto, no copied
  framing. The CLI and `remanence-api::pool_write` route through
  `remanence-format`. The reseal implementation's hand-rolled envelope
  orchestration (`lib.rs:~3802-3954`) is replaced by calls into the same
  funnel.
- **No compatibility paths, no backout flags** (pre-production hard rule).
  v1 shapes are REPLACED, not versioned. `git revert` is the backout.
- **The v2 wire format must not change.** Design D1: byte-invariant. The
  positive v2 fixtures must still pass unmodified. The one sanctioned
  negative re-pin is `v2-version-flip` → `UnsupportedFormatVersion` (D1/V-6).
- **Three checkpoint commits, whole workspace green at each**
  (`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace`). Commit messages:
  `P1-ADD: ...`, `P1-EXCISE: ...`, `P1-RETIRE: ...`.

## Stage P1-ADD — add v2 everywhere, delete nothing

Per design D3/D4/D5/D2/D11:

1. `remanence-aead`: refactor `wrap_dek` to accept `rng: &mut R` (production
   callers pass `EphemeralRng::from_os()`; the internal `from_os` call moves
   out). Add the deterministic-seal entry point for vector generation
   (injected RNG seed + `DataEncryptionKey::from_bytes`) — same
   `seal_envelope` internals, one clearly-named function, doc comment stating
   it exists for test vectors only. Enforce the ≥2-distinct-recipient floor
   in `seal_envelope` itself (library-level), keeping reader acceptance at
   ≥1 slot (D3 reader policy).
2. `remanence-format`: v2 envelope writer/reader/PFR functions wrapping the
   aead crate (mirror the v1 function surface: write/read/read-range with
   mode and manifest-anchor variants as the callers need).
3. `remanence-api`: `PoolWriteRepresentation::Encrypted` → recipient public
   keys (≥2); `seal_prepared_object` routes through the new format fns.
4. `remanence-state`: recipient-epoch-id list replaces `key_id` semantics
   per design D3's pinned encodings (JSON array of 32-hex strings);
   `validate_object_copy_envelope` requires a non-empty list for encrypted
   rows.
5. `remanence-parity`: `BootstrapObjectRepresentation::Encrypted` carries
   the epoch-id array (16-byte byte strings) + `key_frame_len` under NEW
   CBOR keys; old `key_id` tag retired, never reused.
6. `remanence-cli`: `--recipient <RAOR file>` (repeatable, 2–8, distinct
   epochs — lift reseal's parsing/validation) on `archive build`,
   `archive write`, `pool write`; `--private-key` on `extract`,
   `extract-stream`, `covering-range`, `read`, `verify`; `inspect` gains
   keyless key-frame parsing reporting `recipient_epochs` (D3 JSON shape:
   `[{"epoch_id": "<32hex>", "label": "<string>"}]`) + `format_version`;
   `reseal` gains the v2-input leg: `--object <v2> --private-key <path>
   --recipient ... --out <path>`, preserving `object_id`, `chunk_size`,
   `plaintext_digest` (D2).
7. Tests for all of the above (round-trips through the new surfaces; the
   deterministic entry point pinned byte-exact against a checked-in
   expectation).

## Stage P1-EXCISE — delete v1 in one slice

Per design D1/D2/D4 + survey inventory (design §2):

- `remanence-aead`: delete `seal`/`seal_to_vec`, `open`/`open_to_vec`, v1
  range fns, `derive_salt`/`derive_keys` + v1 labels, `RootKey`,
  `RaoHeader::new`, the v1 `validate()`/`parse()` arms (version gate becomes
  `!= 2` → `UnsupportedFormatVersion`), `SealOptions.key_id` field; rename
  `_envelope`-suffixed API to the bare names across all callers; `key_id`
  header region becomes reserved-must-be-zero.
- `remanence-format`: delete v1 encrypted writer/reader/pfr fns.
- `remanence-api`/`state`/`parity`: delete v1 `Encrypted{root_key,key_id}`
  shapes and `key_id` validation/columns per D3.
- `remanence-cli`: delete `--encrypt`/`--key-file`/`--key-id` everywhere
  (presence of `--recipient` IS the encryption switch), reseal's v1 leg +
  `--registry-key`, `read_root_key_file`/`parse_key_id` helpers;
  `pool_ops` v1 representation.
- `rao-recover`: delete `--registry-key`/`open_v1` leg and the `1 =>` match
  arm.
- Tests/fixtures: delete v1-only tests (TV-E1 test, extract-stream memory
  test, sealed_stream_fixture and dependents…); re-base the version-agnostic
  negative-envelope cases and TV-D1's encrypted half onto v2 bases built via
  the deterministic entry point; split mixed tests (rao-recover, TV-D1).
  Fixture-manifest JSON edits needed for compiling tests are in scope; the
  publication tar/manifest regeneration is NOT (P2).
- Fuzz (D10): port `rao_whole_object_open_verify` to `open_envelope` with a
  fixed recipient key; seed its corpus with a valid v2 object sealed to that
  key (deterministic entry point); add a direct `KeyFrame::parse` fuzz
  target with 1/2/8-slot valid seeds + malformed/truncated variants; prune
  the v1 arm from `rao_envelope_header`; migrate/minimize the old corpus.

## Stage P1-RETIRE — proofs + docs

Per design D6:

- A proof survives ONLY if its drift-guard-pinned snippets are byte-identical
  post-excision. Expected: retire `verif/aead-framing`, `verif/rao-header`,
  `verif/rao-archive` (remove drift guards, remove from
  `verif/check-inventory.sh` build list, record retirement + reason in
  `verif/STATUS.md` and `docs/formal-verification-status.md`, name
  RAO-V2-FORMAL-PREFIX / RAO-V2-FORMAL-HEADER-KEY-FRAME as the follow-ups).
  Do NOT write or modify any Lean proof. `make proof-inventory` green and
  truthful.
- Repo docs: `reference-cli.md` (new verb/flag matrix, reseal v2→v2,
  report schemas), `guide-quickstart.md` (v2-only walkthrough),
  `reference-glossary.md` (drop v1 entries, keep "format version 1
  (reserved)"), `architecture-overview.md`, `reference-tape-layout.md`
  (+ SVG caption note), `reference-extract-stream-protocol.md`
  (private-key + epoch selection), `amber-architecture.md` gets a top note
  that its crypto section describes the retired v1 design.

## Verification (final, after P1-RETIRE)

`cargo fmt --all --check` && `cargo clippy --workspace --all-targets -- -D
warnings` && `cargo test --workspace` && `make proof-inventory` && `cargo
build --release -p remanence-cli -p rao-recover` && confirm
`rem-debug archive capabilities` still emits the v2 capability strings and
that `git grep -nE 'RootKey|--key-file|registry-symmetric|format_version.*=.*1\b' crates/`
returns only intentional remnants (report them in your summary).

Your final summary must list: every deleted public API symbol, every new
public API symbol, the three checkpoint commit SHAs, test counts per stage,
and any deviation from the design (with justification).
