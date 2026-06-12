# Tape Read-Object (A.9 `rem.tape.read_object`) Design v0.1

Status: design decision. Implements the read mirror of `rem archive write`
(A.5), closing the scenario-A spine round-trip (A.9 → A.10 byte-equal). Relates
to `docs/rem-tar-v1-design.md` (object body format), `docs/layer3c-design-v0.7.2.md`
(sidecar parity), `docs/pool-tape-selection-design-v0.1.md`, and the
`~/system` harness seam `rem.tape.read_object`.

## Contract

The harness seam is `read_object(locator: dict) -> bytes`: given the canonical
locator A.5 emitted, restore the object's original payload bytes so A.10 can
assert bit-equality with the source file. The locator (canonical, hex identity
fields) carries:

```
tape_uuid, tape_file_number, first_body_lba, object_id,
caller_object_id, content_sha256, pool_id, body_format
```

It carries **no length** — so the read length (object body block count) is
looked up from the catalog (see §4).

## Command surface

`rem-debug archive read` — break-glass, like `archive write` (the new design
classifies state-changing/direct tape access as `rem-debug`-only; read is a
direct tape op, so it lives there too). Implemented in
`crates/remanence-cli/src/pool_ops.rs` next to `archive write`, gated as
"direct local tape archive access".

Arguments (`ArchiveReadArgs`): `--locator <json>` (the A.5 `--json` line),
`--out <path>` (destination for restored payload), and the shared
`--config / --allow / --library` flags. On success it writes the payload to
`--out` and prints **one JSON receipt line** to stdout:

```json
{"object_id":"…","bytes_written":1048576,"content_sha256":"ab…","verified":true}
```

The seam runs the command and returns `Path(out).read_bytes()`.

## Restore flow (`run_archive_read`)

1. **Decode locator** (`decode_locator`): parse JSON (`ObjectLocator`), hex-decode
   `tape_uuid`→`TapeUuid`, `content_sha256`→`[u8;32]`; keep `object_id` (dashed
   UUID string, as `find_native_object_copies` expects), `tape_file_number`,
   `first_body_lba`.
2. **Resolve read plan from catalog** (`resolve_object_read_plan`), while the
   catalog borrow is live, into owned values (so no borrow is held across tape
   I/O — see §10 Category 4):
   - `find_native_object_copies(object_id)` → confirm a copy at
     `(tape_uuid, tape_file_number, first_body_lba)` matching the locator;
     mismatch ⇒ error (`NativeObjectCopyRecord` carries exactly these fields).
   - `list_tape_files(tape_uuid)` → the `TapeFileRecord` with `kind == "object"`,
     `tape_file_number == locator.tape_file_number` → its `block_count` (the read
     length) and the tape's `block_size`. Returns `ObjectReadPlan { block_count,
     block_size_bytes }`.
3. **Open library + mount** (reuse): `report.library(--library)` → `open(policy)`;
   then the existing `load_tape_by_uuid(index, &mut library, &policy, &tape_uuid)`
   resolves voltag, loads the slot if needed, and returns an open `DriveHandle`.
4. **Identity check**: `rewind()`, then `verify_tape_identity(DriveHandleSource(&mut
   drive), &tape_uuid)` — confirms the right cartridge is loaded (mirrors write).
5. **Fixed-block mode**: `read_config()` then `write_config(Fixed { block_size })`
   so `read_block` returns whole fixed blocks.
6. **Position + read**: `DriveHandleSource(&mut drive).locate(first_body_lba)`,
   then `stream_rem_tar_object(source, block_size, block_count, &mut sink)`.
7. **Verify + emit**: compare the streamed payload's SHA-256 to the locator's
   `content_sha256`; write payload to `--out`; print the receipt. SHA mismatch ⇒
   non-zero exit with `verified:false` surfaced.

## Payload extraction (`CapturePayloadSink`)

A `rem-tar-v1` object contains the file entry **plus a generated manifest**
(`remanence_format::model::MANIFEST_PATH == "_remanence/manifest.cbor"`). The
sink implements `RemTarEntrySink` and captures the **single non-manifest
regular-file entry**, streaming its bytes to `--out` and into a `Sha256` as they
arrive (no full-object buffering). `finish()` requires **exactly one** payload
entry:
- zero non-manifest entries ⇒ error (empty/manifest-only object);
- more than one ⇒ error (`archive read` v1 restores a single logical file;
  multi-file object selection via `--path` is out of scope, see §9).

This matches the spine: A.5 writes one file, so the object has one payload entry
+ the manifest.

## Parity (sidecar) — why a single read path covers both tape types

Remanence parity is a **sidecar** architecture, not interleaved with object
data. `TapeFileKind` (in `remanence-parity`):
- `Object` — *"every fixed block gets a `ParityDataOrdinal`"*: an Object tape file
  is **all data blocks**, contiguous.
- `ParitySidecar` — *"raw parity-sidecar tape file for one completed epoch"*:
  parity lives in **separate tape files** written at epoch boundaries.

So an object's `[first_body_lba, +block_count)` range is pure `rem-tar-v1` data on
**both** no-parity and parity tapes; `locate + read + de-tar` touches zero parity
blocks. Parity is consulted only to **recover** an unreadable block from the
sidecar — which never happens on the spine's fresh, healthy VTL write. Therefore
A.9 reads parity-protected and no-parity tapes with the **same code** and no
special-casing. The locator's `content_sha256` provides end-to-end payload
integrity, so a "clean tar-only read" (no bootstrap-digest prefix validation) is
sound for v1.

## Error handling

Distinct, loud, non-zero exits for: locator parse/hex-decode failure; object not
in catalog; locator↔catalog copy mismatch; cartridge not in library / no free
drive / load failure (reuse `LoadByUuidError`); tape identity mismatch; drive
config or `locate` failure; short read / format-parse failure; zero or >1 payload
entries; SHA-256 mismatch.

## Testing

- **Pure/unit (no hardware):** `hex_to_bytes` round-trip; `decode_locator`
  (well-formed + malformed); `resolve_object_read_plan` against an in-memory
  `CatalogIndex` (block_count resolution + locator-mismatch error);
  `CapturePayloadSink` over a `VecBlockSource` rem-tar fixture — single-file
  extraction, manifest skipped, SHA computed, and the zero/multi-entry errors.
- **Hardware/manual:** the scenario-A round-trip — `make scenario-a` reaching
  A.10 byte-equal on the QuadStor fixture after the seam flips
  `rem.tape.read_object` to `Real(cli-subprocess)`.

## Scope

**In:** the no-parity-and-parity (healthy-media) single-logical-file
`archive read` above — decode, catalog plan, mount, identity, locate, stream,
extract, SHA-verify, `--out` + receipt.

**Out (fast-follow):** sidecar **recovery** of damaged/unreadable blocks via
`remanence-parity`'s recovery source (triggers only on read error); multi-file
object selection (`--path`); bootstrap-digest prefix **validation**; daemon
`ReadSessionService` wiring (this is CLI break-glass, like A.5).

## §10 — Rust design verification

Verified against `cargo check -p remanence-cli` and
`cargo clippy -p remanence-cli --all-targets -- -D warnings` (both clean) on
2026-06-01. Compiling skeleton committed at
`crates/remanence-cli/src/pool_ops.rs` (section "archive read (A.9) —
design-verification skeleton"): `ObjectLocator`, `DecodedLocator`,
`ObjectReadPlan`, `ArchiveReadArgs`, `ArchiveReadReceipt`, `CapturePayloadSink`
(real `RemTarEntrySink` impl signatures), and `run_archive_read` with its
**borrow-sensitive body written for real** (helpers/sink bodies `todo!()`).

Five-category result:
1. **Module privacy** — all cross-crate calls are `pub` APIs; `CapturePayloadSink`
   is a local type implementing the foreign `pub` trait `RemTarEntrySink`
   (orphan rules satisfied: foreign trait + local type). Pass.
2. **`!Send` in threading/async** — synchronous CLI; no thread or `tokio::spawn`
   boundary. Pass.
3. **Reactor-registration timing** — no tokio-aware types constructed. Pass.
4. **Borrowed-handle plumbing** — the catalog `ObjectReadPlan` is resolved into
   owned values before the drive is acquired, so no catalog borrow is held across
   the `DriveHandle` lifetime; `DriveHandleSource(&mut drive)` is taken
   sequentially (identity verify, then locate+stream), mirroring the proven write
   path. The compiled body surfaced one fix (`let mut state_handle`, because
   `catalog_index()` takes `&mut self`). Pass.
5. **Trait/method visibility traps** — `stream_rem_tar_object`, `RemTarEntrySink`,
   `DriveHandleSource`, `BlockSource::locate`, and the `CatalogIndex` lookups are
   all `pub` and callable from `remanence-cli` with constructible inputs. Pass.

The skeleton is retained as implementation Step 0 (its `todo!()` bodies + the CLI
command wiring are the work). New crate deps added for it: `serde`, `sha2`.
