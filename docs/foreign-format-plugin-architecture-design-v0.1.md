# Foreign-format plugin architecture — design v0.1

**Status:** approved direction (2026-06-15, owner + claude). Codex work order.
Structural refactor of how *foreign/legacy* tape formats live in remanence.
Decision record: `ingest-archive-deferred-items-design-v0.1.md` item 3. Builds
on the driver-trait sketch in `format-driver-streaming-boundary.md`; pairs with
`tape-platform-seam-design-v0.1.md` (this is the format-layer complement to the
platform seam's layout/catalog reuse).

## 1. Why

There are two categories of "format" in rem, and they should not be peers:

- **Native** (RAO `rao-plain-v1` / `rao-aead-v1`) — rem *writes* these; they're
  rem's identity; **core**.
- **Foreign / legacy** (BRU, old tar, vendor backup formats) — rem only
  *reads* them, to migrate off legacy tapes. They're reverse-engineered and
  inherently **per-deployment** (one deployment has BRU; another site has something
  else). Baking one organization's legacy into the core tool is wrong.

So foreign formats become **plugins**: rem core ships with **zero** foreign
formats; a deployment assembles its `rem` binary from core + the plugins it
needs. BRU moves out of core into its own project.

This also resolves the restore-naming collision (review item 3): once the
BRU-specific `archive restore` command leaves core, `rem restore` (or
`archive extract`) cleanly owns native RAO restore, and foreign operations go
through the generic `--format <plugin>` dispatch.

## 2. The model

**Compile-time plugins, not dynamic loading.** A plugin is a Rust crate
implementing a published trait; a deployment's `rem` binary is built from core
plus the chosen plugin crates. No `.so` loading, no plugin ABI — consistent
with `format-driver-streaming-boundary.md` and Rust's lack of a stable ABI.

Pieces:

1. **A published foreign-format-driver trait** — promote the
   `format-driver-streaming-boundary.md` sketch (`ForeignTapeFormat`,
   `ArchiveReader`, `FormatCapabilities`, `SourceRequirement`, the normalized
   `ArchiveReader::{scan, stream_all, stream_file}` + sink events) from sketch
   to a real, stable extension point in a dedicated crate.
2. **rem core depends only on the trait crate**, never on any foreign-format
   implementation.
3. **Each foreign format is its own crate** implementing the trait. `BRU` =
   the first one, moved out of the core workspace.
4. **The `rem` binary is assembled** from core + selected plugin crates, which
   register themselves into the `--format` dispatch at startup.
5. **Generic `--format` dispatch** (`rem archive probe|scan|restore|recover
   --format <id> …`, already designed in the note) is the *only* surface for
   foreign formats, and a given `<id>` is available only when its plugin is
   compiled in. No format-specific top-level commands in core.

## 3. Sub-decisions (resolve during implementation)

1. **Plugin coupling — linked-in crate (recommended) vs out-of-process
   subprocess.** Lean: linked-in crate (type-safe, matches the existing
   `remanence-bru`, the note's stance). A subprocess interface (for non-Rust
   plugins) is a possible *later* addition, not v1.
2. **Where the trait crate lives** — a new `remanence-format-driver` crate, vs
   folding into the platform/library layer. Lean: a dedicated
   `remanence-format-driver` crate (clean dependency boundary; foreign plugins
   depend only on it).
3. **Binary assembly** — a small `rem` distribution/binary crate that depends
   on core + chosen plugins and registers them, vs cargo feature flags on a
   meta-crate. Lean: a distribution crate (explicit, readable); features as an
   alternative.
4. **Sequencing — incremental (recommended) vs big-bang.** Incremental:
   (a) extract the trait into its crate and make core depend only on it;
   (b) make `remanence-bru` implement the trait and depend only on the trait
   crate; (c) feature-flag BRU out of the default core build; (d) move BRU to
   its own repo when convenient. Each step is independently green.

## 4. CLI surface changes

- **Remove** the BRU-specific `archive restore` (legacy dump) command from core.
- **Native restore** is `rem restore` (new top-level verb) / `rem archive
  extract` — owns RAO restore, no competitor. (Settle `rem restore` as the
  documented top-level verb aliasing the extract path; confirm against what the
  harness drives.)
- **Foreign operations** only via `rem archive <probe|scan|restore|recover>
  --format <id>`, present only when a plugin is built in; a `--format` with no
  registered plugin gives a clear "format not available in this build" error.

## 5. What does NOT change

- The RAO/REM-PARITY wire formats; native read/write paths.
- The normalized `ArchiveReader` event model and capability gating (already in
  the note) — this work *publishes* it, doesn't redesign it.
- BRU's actual reverse-engineered parsing logic moves verbatim into its plugin
  crate; behavior is preserved, location changes.

## 6. Spec / docs

- Update `format-driver-streaming-boundary.md` from "sketch" to "published
  extension point" (or supersede it with this doc + the trait crate's docs).
- The published RAO spec is unaffected (foreign formats are not RAO).
- `tape-platform-seam-design-v0.1.md` — note that foreign read-format plugins
  are the format-layer complement to the platform seam.

## 7. DoD

`cargo fmt`/`clippy -D warnings` clean; core builds and tests with **no**
foreign-format crate in its dependency graph (a CI check, like the platform
crate guarantee); a BRU-included build still passes the existing BRU read/
restore tests via the `--format bru` dispatch; the BRU-specific core command is
gone and `rem restore` handles native RAO.

## 8. Scope note

This is an architecture refactor, larger than the ingest items and independent
of them. Sequence it on its own; it does not block the ingest/format work and
is not blocked by it.
