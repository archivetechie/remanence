# Design — P2.4: `rem archive build --map` + the source-map wire-spec

> Design by Claude + the owner (2026-06-26), for codex review then implementation. **Repo: remanence.**
> Plan item **P2.4** (sutradhara `docs/implementation-plan-ingest-v2.md`); resolves arrangement-arc
> §11.5 open-decision #5. Cross-repo: the **wire-spec** (§2) is a shared contract with sutradhara,
> which already *emits* this exact TSV (P2.3a `design-arrangement-submit.md`, shipped). P2.4 builds the
> **consumer**.
>
> **Reviewed 2026-06-26 (Claude), code-verified against both repos:** the §2 wire-spec matches the
> committed producer (`arrangement.py` `render_source_map`/`SOURCE_MAP_COLUMNS`) byte-for-byte (column
> order, header, TAB/LF/trailing-newline, lowercase-64-hex sha256, decimal size, control-char
> rejection, `ingest_item_id`). Round-1 findings folded in: the sha256 verify is **free** (the writer
> already recomputes + compares, `writer.rs:433`); `stat==size`, `is_absolute()`, the `--source-root`
> guard, and the raw member-path validator are **new code**; `--manifest-out` gating is a runtime `if`
> not a clap attr; `manifest-sha256.txt` is BagIt-format (`<hex>␣␣file`); the report already has a
> distinct `manifest_sha256` key.
>
> **Reviewed 2026-06-26 (codex), 4 findings folded in:** (1) **member `file_id`s are derived, not random
> UUIDs** (a random id breaks the byte-reproducibility claim — fix decision #1/#6; reproducibility also
> needs `--manifest-file-id` pinned); (2) **`archive_path` gets a raw split-on-`/` validator**, not
> `archive_path_from_relative`/`Path::components()` (which silently normalize `a//b`/`a/./b`/`a/`); (3)
> **`source_path` must be `is_absolute()`** before canonicalize (no CWD dependence); (4) **`ingest_item_id`
> is reported as an opaque JSON *string***, echoed verbatim (P2.5 keys on a string).
>
> **Reviewed 2026-06-26 (codex round 2), 4 more folded in (all CLI-wiring):** (1) the `--source-root`
> security requirement must go on **`--map` as `requires = "source_root"`** (clap `requires` is
> one-directional) + a runtime `map && !source_root` reject; (2) **`--map` conflicts with `--scan-only`**
> (the `if args.scan_only` branch at `lib.rs:5527` runs before input selection); (3) the new flags go in
> **both** build-arg structs (`RemArchiveBuildArgs` `lib.rs:1281` + `ArchiveBuildArgs` `lib.rs:1827`) and
> the `From` conversion (`lib.rs:1631`); (4) `--inputs`' `required_unless_present` is **extended** to
> `["scan_only", "map"]` (it's already `"scan_only"`, not replaced).

## 0. What this is
The Remanence side of the arrangement arc. sutradhara freezes an arrangement into a **source-map**
(`source-map.tsv`: `archive_path ← source_path, sha256, size, ingest_item_id`). P2.4 is the
**`rem archive build --map <tsv>`** flag that builds a RAO object whose **member names = the arranged
names** and **member bytes = the originals**, reading each original straight from its `source_path` —
**no copied 4K staging tree** — verifying every member's sha256 at build, writing members **in map
order** (so the arranged structure is contiguous on tape).

Two deliverables, both here: **(1)** the cross-repo wire-spec (§2 — the TSV contract), and **(2)** the
`--map` flag (§3) over the existing build core.

**The build core already does the hard part.** `archive build` separates name from source
(`ArchiveBuildInputFile { source_path, archive_path, file_id, size_bytes, file_sha256, … }`,
`crates/remanence-cli/src/lib.rs:5508`), validates/normalizes member paths (`archive_path_from_relative`
+ `path_component_to_string`, `lib.rs:7494`/`:7513`), enforces archive-path uniqueness
(`ensure_unique_archive_paths`, `archive_ingest.rs:2499`), and — load-bearing for `--map` — **the writer
recomputes each member's SHA-256 while streaming the source bytes and fails the build if it ≠ the
spec's `file_sha256`** (`remanence-format/src/writer.rs:433-440`, `stream_file_payload`), then
plans/writes the RAO through parity and emits a per-member build report with `first_chunk_lba`. **`--map`
is a CLI map-parser that builds the `Vec<ArchiveBuildInputFile>` from the TSV instead of from
`--inputs`/`--rules`** (a third input branch inside `build_archive_object_file`, which takes
`&ArchiveBuildArgs`). No RAO format change; no new command.

## 1. The load-bearing decisions (review focus)
1. **`--map` is an input *front-end*, not a new command.** It feeds the existing
   `build_archive_object_file` core a `Vec<ArchiveBuildInputFile>` built from the TSV. Mutually
   exclusive with `--inputs`/`--rules` (which walk a tree + apply ingest rules); `--inputs` becomes
   `required_unless_present = "map"`. `--out`, `--encrypt`/`--key-file`/`--key-id`, `--chunk-size`,
   `--object-id`, `--timestamp` all still compose (an encrypted RAO from a map = the offsite AEAD copy).
2. **The TSV sha256 is the value to *verify against*, never to trust-and-skip — and this is free.** The
   writer already recomputes each member's SHA-256 over the streamed source bytes and **errors if it ≠
   the spec's `file_sha256`** (`writer.rs:433-440`); it is an assertion today, not a trusted
   precompute-cache. So `--map` just sets `file_sha256 = Some(expected_from_tsv)` and the existing writer
   gives "per-member sha256 verified at build" with **no new verify code**. The **size check is the one
   genuinely new guarantee**: the writer reads exactly `size_bytes` and errors only if the source is
   *shorter* — it will silently truncate a source that is *longer* than the claimed size. So `--map`
   must add an explicit `stat(source).len() == row.size` check (the only thing that catches an over-long
   source); a mismatch fails the build. A map is untrusted input, not a shortcut to skip hashing.
3. **Source paths are security-anchored.** `--map` **requires `--source-root <DIR>`** (the intake's
   registered BagIt payload root). For every `source_path`: first **require `Path::is_absolute()`** (the
   wire spec says `source_path` is absolute; reject a relative path outright — never let CWD decide where
   it resolves), then `std::fs::canonicalize` it (which resolves symlinks, so a symlink that escapes the
   root is caught at check time) and a canonicalized `--source-root`, then require
   `canonical_source.starts_with(canonical_root)` and that it is a regular file — *before* rem opens it.
   Model the per-component symlink-rejecting walk on the restore `_secure` helpers
   (`remanence-stream/src/lib.rs:785-859`) if a stricter guard is wanted. The residual TOCTOU (a symlink
   swapped between canonicalize and open) is **accepted** here — the producer is trusted and the intake
   is single-tenant; note it rather than over-engineer. Without this anchor, a crafted/buggy map turns
   `rem` into an arbitrary-file reader (`/etc/shadow`, another tenant's data). `archive_path` stays inside
   the object via the **raw TSV-field validator** (§2 table / §3) — *not* `Path::components()`, which
   would silently normalize `a//b`/`a/./b`/`a/` into a different member name.
4. **Map order is authoritative; do not re-sort.** Members are written in the exact TSV row order (so
   the arrangement is contiguous on tape). Uniqueness of `archive_path` is still enforced
   (`ensure_unique_archive_paths` — don't trust input). sutradhara already emits the rows in
   `archive_path` lexical order, so the order is deterministic; rem honors *the given* order.
5. **Round-trip: the build report echoes `ingest_item_id` per member.** The map carries `ingest_item_id`
   *in*; the per-member build report carries it back *out* alongside the RAO locator
   (`archive_path`, `first_chunk_lba`, `size`, `sha256`). This is how sutradhara/P2.5 records an
   `AssetLocator` per `IngestItem` without re-deriving identity. rem treats `ingest_item_id` as an
   **opaque correlation token: it does not parse it, and the report emits it verbatim as a JSON
   *string*** (the TSV field as-read). P2.5/sutradhara must therefore key `AssetLocator` on a string,
   not a number — even though the producer's source value is an int rendered to decimal text. (Keeping
   it opaque-string decouples rem's report from sutradhara's id type; if a numeric contract is ever
   preferred, both repos change together — not this slice.)
6. **Determinism + provenance — reproducible requires deterministic ids.** The build report records
   `map_sha256` = the SHA-256 of the consumed TSV (= sutradhara's `manifest-sha256.txt`). For the object
   **bytes** to be reproducible, every id that lands in the manifest must be deterministic: member
   `file_id`s are **derived** (not random — decision #1 / §3), and the caller must pin **`--object-id`,
   `--timestamp`, *and* `--manifest-file-id`** (all three default to random `Uuid::new_v4()` otherwise —
   `lib.rs:5563`/`5568`). Given the same TSV + those three pinned, the build is byte-reproducible.

## 2. The wire-spec (the shared contract — both repos honor this)
The producer is sutradhara (P2.3a, shipped); the consumer is `rem archive build --map`. **rem
re-validates everything — the contract is enforced on read, not assumed.**

**Format.** UTF-8 text, **tab-separated**, LF line endings, one header row then one row per member,
trailing newline:
```
archive_path<TAB>source_path<TAB>sha256<TAB>size<TAB>ingest_item_id
satsang/day-1/A001.MOV<TAB>/replica/landing/intake-123/data/DCIM/A001.MOV<TAB>abc…<TAB>987654321<TAB>4101
```
**Columns:**
| column | meaning | rem validation |
|---|---|---|
| `archive_path` | the in-RAO member name (relative) | **raw-validate the field**: split on `/`, reject empty/`.`/`..` components, leading/trailing slash, non-UTF-8, then join exactly — do **not** route through `Path::components()` (it silently normalizes `a//b`/`a/./b`/`a/`); reject if duplicate (`ensure_unique_archive_paths`) |
| `source_path` | absolute path to the original bytes, under `--source-root` | **require `is_absolute()`** (reject relative — no CWD dependence); then `canonicalize`; require `starts_with(canonical(--source-root))` (no traversal/symlink escape); must be a regular file |
| `sha256` | content hash of the original (64 lowercase hex) | exactly 64 hex chars; **build recomputes and must match** |
| `size` | original size in bytes (decimal) | parse u64; **must equal `stat(source).len()`** |
| `ingest_item_id` | opaque caller correlation token | not parsed; echoed verbatim in the report **as a JSON string** (P2.5 keys on a string) |

**Escaping / encoding.** Paths are UTF-8 and contain **no control characters** — the producer rejects
any `archive_path`/`source_path` holding a tab/newline/CR/C0–C1 char, so a TSV field can never embed a
delimiter. rem **re-rejects** any row with the wrong column count, a non-UTF-8 field, or a control char
(fail the build; never guess a split). No quoting/escaping scheme is defined because control chars are
forbidden, not escaped.

**Per-entry size check, duplicate handling, deterministic map-manifest hash** — items 2/4/6 above:
size must match `stat`; duplicate `archive_path` fails; `map_sha256` (SHA-256 of the TSV bytes) is
recorded in the report and equals the digest **value** in the producer's `manifest-sha256.txt`.
**Format note:** that file is BagIt-style — `"<hex>␣␣source-map.tsv\n"` (digest, two spaces, filename),
**not** a bare hex digest. So `--map-sha256` / any comparison against it must parse the **first
whitespace-delimited field**, not read the whole file. (Producer: `arrangement.py` writes
`f"{digest}  {SOURCE_MAP_NAME}\n"`.)

## 3. The `--map` flag (over existing machinery)
**Add the three new fields to BOTH build-arg structs** — the user-facing `rem archive build` parser
`RemArchiveBuildArgs` (`lib.rs:1281`) **and** the direct/debug `ArchiveBuildArgs` (`lib.rs:1827`) — and
carry them through the `From<RemArchiveBuildArgs> for ArchiveBuildArgs` conversion (`lib.rs:1631`).
Adding them to only the shared struct would leave `rem archive build --map` unparsed.
- `--map <PATH>` — the source-map TSV. **`conflicts_with = ["inputs", "rules", "scan_only"]`**
  (a tree-walk scan makes no sense over a map) and **`requires = "source_root"`** — note the direction:
  the security invariant is *`--map` requires `--source-root`*, so the `requires` goes on `--map`, **not**
  on `--source-root` (clap's `requires` is one-directional). **Belt-and-braces:** also runtime-reject
  `map.is_some() && source_root.is_none()` before reading the TSV — never let a security guard rest on a
  clap attribute alone.
- `--source-root <DIR>` — the anchor root; only meaningful with `--map`.
- extend `--inputs`' existing `required_unless_present = "scan_only"` to
  `required_unless_present = ["scan_only", "map"]` (keep `scan_only`); `--out` stays required for `--map`
  (it produces an object file).
- (optional) `--map-sha256 <HEX>` — if given, rem verifies the TSV's SHA-256 equals it *before*
  building (catches a corrupted/truncated map in transit). Nice-to-have; see §7.

**Guard `--map` against the `--scan-only` branch:** the build fn checks `if args.scan_only` (`lib.rs:5527`)
*before* input selection and calls `scan_only_report(&args.inputs, …)` — so beyond the clap
`conflicts_with`, the runtime must also reject `--map` reaching that branch (defence in depth).

**Parse → `Vec<ArchiveBuildInputFile>`** (a new `archive_map.rs`, sibling to `archive_ingest.rs`):
for each row, after the §2 validations, emit
```rust
ArchiveBuildInputFile {
    source_path,                       // is_absolute() + canonicalized + verified under --source-root
    entry_type: RemTarEntryType::Regular,
    archive_path: validate_member_path(&row.archive_path)?,  // RAW split-on-'/' validator (NOT Path::components)
    file_id: deterministic_archive_entry_file_id(           // derived, NOT random — for byte-reproducibility
        RemTarEntryType::Regular, &archive_path, Some(&sha256), None),
    size_bytes: row.size,              // checked == stat(source).len() by the map front-end
    file_sha256: Some(hex_to_32(&row.sha256)?),  // [u8; 32] — decode the 64-hex; writer recomputes + compares
    link_target: None,
    xattrs: BTreeMap::new(),
}
```
Notes: (a) `file_sha256: Option<[u8; 32]>`, so decode the 64-hex to `[u8; 32]` — `Some(row.sha256)` as a
`String` won't compile. (b) `file_id` is **derived** via `deterministic_archive_entry_file_id` (the same
helper the `--inputs` path uses, `lib.rs:7305`), **not a fresh UUID** — a random id lands in the manifest
and breaks byte-reproducibility (decision #6). (c) `archive_path` uses a **new raw validator**
(`validate_member_path`: split on `/`, reject empty/`.`/`..`/leading/trailing-slash/non-UTF-8, join
exactly) — *not* `archive_path_from_relative`/`Path::components()`, which silently normalize `a//b` etc.
Then feed the `Vec` through the existing write path. **The sha256 verify
is already done for you:** the writer recomputes the streamed hash and errors if it ≠ `spec.file_sha256`
(`writer.rs:433-440`) — no new compare code. **The size check is new:** add `stat(source).len() ==
row.size` in the map front-end (the writer only catches a *short* source, not an over-long one). **Do
not sort** the vec for `--map` (both the `--inputs` path, `lib.rs:7229`, and the `--rules` path,
`archive_ingest.rs:330`, sort by `archive_path`; the map path must bypass both and preserve TSV order);
still call `ensure_unique_archive_paths` explicitly.

**`--manifest-out` with `--map`:** the "`--manifest-out` requires `--rules`" rule is a **runtime check**
(`build_archive_object_file`, `lib.rs:5521`), not a clap attribute — relax that `if` to also accept
`--map`. With `--map` the member list *is* the manifest, so `--manifest-out` can emit a customer manifest
derived from the map rows (no exclusions/wrappers to record). If that widens scope, defer it (§8) — the
map TSV + report already carry everything P2.5 needs.

**Report:** the per-member report (`archive_file_report_json`, `lib.rs:7148`) already emits
`path`/`first_chunk_lba`/`size_bytes`/`file_sha256`; add `ingest_item_id` per member **as a JSON string
(verbatim from the TSV field, not parsed to a number)**, and a top-level `map_sha256`. **Naming caution:**
the top-level report already has a `manifest_sha256` key
(`lib.rs:7134`) = the RAO's *internal* manifest hash — a **different** value from `map_sha256` (the TSV
hash). Keep both, named distinctly; don't conflate `manifest_sha256` with the producer's
`manifest-sha256.txt` (the latter equals `map_sha256`). `ArchiveBuildInputFile` has no `ingest_item_id`
field today, so carry it via a parallel vec keyed by row index (or add a field) into the report.

## 4. Reuse / non-goals in the code
**Reuses unchanged:** `ArchiveBuildInputFile`, `deterministic_archive_entry_file_id` (the `--inputs`
file-id derivation, `lib.rs:7305`), `ensure_unique_archive_paths`, the streaming sha256
recompute-and-compare in `writer.rs`, `plan_prepared_object` / `write_prepared_object_to_parity`, the
build-report emitter, the encryption path. **New code (not reuse):** the TSV parser, the **raw
`validate_member_path`** (the existing `archive_path_from_relative` normalizes via `Path::components()`
and so is *not* strict enough for the wire contract), the `is_absolute()` + `stat == size` checks, and the
`--source-root` canonicalize-and-contain guard. **Does not change:** the RAO on-tape format (map members
are ordinary regular-file entries — no spec bump), the `--rules`/`--inputs` modes, `archive
extract`/`inspect`/`verify`.

**`--map --out <file>` produces a RAO *object file*, not a tape write.** Getting that object onto tape
and recording `Copy`/`AssetLocator` rows is **P2.5** (sutradhara, driving `rem` + the daemon). P2.4
ends at the verified object file + report.

## 5. Tests & DoD (`cargo test` in remanence-cli + a `rem-debug` CLI test)
- **happy path** — a 2–3 member map → build → `archive extract` → member names == `archive_path`s,
  bytes == originals, per-member sha256 verified; the object embeds **no** staging-tree copy.
- **map order preserved** — members are laid out in TSV order (assert from the build report's
  `first_chunk_lba` ordering).
- **sha256 mismatch fails closed** — map claims hash X, source hashes to Y ⇒ build errors, **no object
  written**.
- **size mismatch fails** — TSV `size` ≠ `stat` ⇒ error.
- **security: source escapes the root** — a `source_path` of `../../etc/passwd`, or a symlink under the
  root pointing outside ⇒ rejected before any read.
- **relative source_path rejected** — a non-absolute `source_path` ⇒ rejected (`is_absolute()`), never
  resolved against CWD.
- **bad archive_path (raw)** — absolute / `..` / empty component / leading/trailing slash / non-UTF-8 ⇒
  rejected; and **`a//b` / `a/./b` / `a/` are rejected, not silently normalized** (proves the raw
  validator, not `Path::components()`).
- **`--map` without `--source-root`** ⇒ rejected (clap `requires` + runtime guard) before any TSV read.
- **`--map --scan-only`** ⇒ rejected (conflicts), never reaches the scan branch.
- **duplicate archive_path** ⇒ rejected.
- **malformed TSV** — wrong column count / control char / bad hex / non-decimal size ⇒ rejected, clear error.
- **encrypted map build** — `--encrypt --key-file … --map …` → AEAD RAO → extract round-trips (the
  offsite-copy path).
- **round-trip report** — the report echoes each member's `ingest_item_id` **as a JSON string** + the
  top-level `map_sha256`.
- **byte-reproducible** — two builds of the same TSV with pinned `--object-id` + `--timestamp` +
  `--manifest-file-id` produce **byte-identical** objects (proves derived member file_ids; would fail
  with random ids).
- **regression** — `--inputs`/`--rules` builds unchanged.
- **gates** — `cargo test` + `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` (note
  `--all-targets` — test code isn't linted otherwise); **rebuild the release
  binary** (the harness runs `target/release/rem-debug`). The full sutradhara→rem→extract loop is the
  **P2.5** `~/system` scenario, not P2.4's gate.

## 6. Scope (not built here)
- **No tape write / no catalog rows** — P2.5 (sutradhara) drives the object onto tape and records copies/locators.
- **No symlink-tree bridge** — explicitly rejected (arc §11.5): it re-materializes a possibly-million-entry
  tree and encodes the map in filesystem paths. `--map` is the target, not a bridge.
- **No RAO format change**, no sutradhara-side change (it already emits the TSV; the wire-spec just
  pins the contract it must keep).
- **No package/`.remwrap` wrapping** for map members — map entries are 1:1 regular-file RAO members
  (package normalization happened at receive, arc §2.5).

## 7. Shared-contract note (for the cross-repo codex prompt)
Per the system harness convention, a cross-repo change carries an identical **Shared contract** section
in each repo's prompt. Here it is §2 (the TSV). sutradhara is the *producer* and already emits exactly
this (`render_source_map` / `SOURCE_MAP_COLUMNS = (archive_path, source_path, sha256, size,
ingest_item_id)`); P2.4 is the *consumer* and must match it byte-for-byte. Any drift is a contract break
that the P2.5 `~/system` scenario will catch. The optional `--map-sha256` lets the caller assert the
TSV it handed rem is the one it built (defence against transit corruption), tying to sutradhara's
`manifest-sha256.txt`.

## 8. Open decisions
1. **`file_id` source** — **RESOLVED (2026-06-26, codex review): derive deterministically** via
   `deterministic_archive_entry_file_id(Regular, archive_path, sha256, None)` — the same helper the
   `--inputs` path uses (`lib.rs:7305`). A fresh UUID was the original draft, but `file_id` lands in the
   RAO manifest, so a random id makes the object non-byte-reproducible and contradicts decision #6. The
   caller's correlation key stays `ingest_item_id` (carried in the report); `file_id` is internal but
   must be **derived**, not random. (Full byte-reproducibility also requires pinning `--manifest-file-id`
   — see #5.)
2. **`--manifest-out` with `--map`** — support (emit from map rows) vs. defer. Lean support; defer only
   if it pulls in wrapper/exclusion manifest machinery that the map doesn't need.
3. **`--source-root` strictness** — **RESOLVED (2026-06-26): required with `--map`**, mandatory, for the
   security anchor (the owner). A tampered/buggy map can never make rem read outside the named root. (A
   single map spans one root today — single-intake, sutradhara §8; if multi-intake lands, `--source-root`
   could take multiple dirs. Not this slice.)
4. **`--map-sha256` pre-check** — **RESOLVED: build it now.** Cheap, high-value transit-integrity guard
   tying to sutradhara's `manifest-sha256.txt` (parse the first whitespace field — it's BagIt-format);
   an optional flag, no cost when unused.
5. **Byte-reproducibility recipe** — **RESOLVED (2026-06-26, codex review):** for byte-identical objects,
   member `file_id`s are derived (#1) and the caller pins **`--object-id` + `--timestamp` +
   `--manifest-file-id`** (all default to random `Uuid::new_v4()` — `lib.rs:5563`/`5568`). P2.5 should
   pin all three when it wants the offsite AEAD copy to reproduce the working-copy bytes. (No new flag —
   `--manifest-file-id` already exists.)
6. **`ingest_item_id` report type** — **RESOLVED (2026-06-26, codex review): opaque JSON *string*,
   echoed verbatim** from the TSV field; rem never parses it as a number. P2.5/sutradhara keys
   `AssetLocator` on the string. (Decouples rem from sutradhara's int id type; revisit only if a numeric
   contract is ever jointly preferred.)
