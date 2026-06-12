# The Tape Platform Seam — v0.1

**Status:** Part 1 approved for implementation; Parts 2–3 **deferred by
decision** (2026-06-11, owner). This note records the design so it is not
re-derived — or re-proposed at full scope — later.

**Context:** REM-PARITY 1.0 (`rem-parity-1.0-specification.md`) reframed the
native on-tape layout as *a* format with an identifier, payload-agnostic by
its own Section 4. That raised the question this note answers: can remanence
serve as a format-agnostic tape library/drive management platform — the way
a disk takes any filesystem — rather than an HSM welded to its own format?

## 1. The seam

The answer is yes, and the cut already exists in the dependency graph:

| Disk world | Remanence |
| --- | --- |
| Controller driver + block layer | `remanence-scsi` + `remanence-library` |
| Raw block device access (`/dev/sdX`) | drive lease (Part 2, deferred) |
| Partition table | tape-ownership registry (Part 3, deferred) |
| ext4 = format + inodes | REM-PARITY + RAO + catalog = the bundled "tape filesystem" |

`remanence-scsi` has zero internal dependencies; `remanence-library` depends
only on `remanence-scsi`; every other crate sits above. The native catalog
is *intentionally* coupled to the native layout and object format — a
filesystem is exactly the combination of object format and catalog
structure (files and inodes). Format-agnosticism is a property of the
layers **below** the catalog, not of the catalog.

Note the existing read-side complement: pluggable *reading* of foreign
formats (BRU, legacy tar) is already designed and partially built via the
format-driver registry (`format-driver-streaming-boundary.md`). This note
is about the write side and the device platform.

## 2. Part 1 — crate platform guarantee (ACTIVE)

Layers 1–2 are reusable today by anyone implementing their own layout +
catalog on top. Make that an explicit contract instead of an accident:

1. **CI dependency guard**: a check that fails if
   `crates/remanence-scsi/Cargo.toml` gains any internal dependency, or
   `crates/remanence-library/Cargo.toml` gains any internal dependency
   other than `remanence-scsi`. A small script or test asserting on the
   parsed Cargo.toml `[dependencies]` sections is sufficient; wire it into
   the existing CI gates.
2. **Contract statement**: a short section in the workspace README (or a
   `crates/remanence-library/README.md`) stating the platform guarantee:
   these two crates are format-free — no knowledge of REM-PARITY, RAO, the
   catalog, or the daemon — and consumable standalone.
3. Already satisfied, keep it so: format-relevant device abstractions that
   foreign consumers need (`PhysicalTapeSource`, drive block-size and
   compression configuration) live in `remanence-library`, never in
   `remanence-parity`. (`RawTapeSource` in Layer 3c is parity-scoped and is
   not the platform contract — per `format-driver-streaming-boundary.md`
   §3.)

That is the entire active scope. No daemon changes, no schema changes.

## 3. Part 2 — drive lease service (DEFERRED, sketch)

Exclusive device-path grant via a new daemon gRPC service + CLI:

- **Acquire**: `rem drive lease --tape <barcode> | --scratch-from-pool <p>`
  `[--drive <bay>] [--compression on|off] [--unload-on-release]` → daemon
  loads the tape, applies drive config, persists the lease in
  `rem-state.sqlite`, emits an audit event, returns `lease_id` + the host
  device path (`/dev/nstX`). Device access control stays plain unix
  (`tape` group). Same `--allow <library-serial>` gate as other
  destructive ops.
- **While held**: changer moves touching the leased drive/tape are refused
  (typed error); native sessions cannot take the drive; the lease survives
  daemon restart and is re-verified against drive state on startup.
- **Release**: restore drive config (compression back off), rewind/unload
  per options, refresh inventory, update ownership, audit. TTL expiry
  never yanks the device — it flags the lease for the operator
  (`rem drive lease list` / `break`).
- Hand-off mechanism decided: **device-path grant** (option 1). An
  fd-passing wrapper (`rem drive lease run -- <cmd>` execing with
  `/dev/fd/N`) can be layered on later without API change; a gRPC
  streaming proxy was considered and rejected (no remote consumer, heavy
  tape-semantics-over-network lift).

## 4. Part 3 — tape-ownership registry (DEFERRED, sketch)

The "partition table": a small platform-level table keyed by barcode
(+ medium serial when known):

- `owner ∈ {native, foreign(format_id), leased(lease_id), unassigned,
  retired}`, plus optional layout identity once known (REM-PARITY tape
  UUID, ANSI VOL1 label, LTFS volume name).
- Rules: native pool selection draws only from `native`/`unassigned` per
  policy; `foreign` tapes are refused for recycle/overwrite by default;
  classification only via explicit `rem tape identify <barcode>` (load +
  probe BOT) — never automatic mass-probing of a library.
- Sits *under* the pool-membership design as its floor. Adjacent to, but
  deliberately not solving, the open recycle-identity-reconciliation
  concern.

## 5. Why deferred — and why copy-3 must never be the trigger

Parts 2–3 have **no current consumer**:

- **Copy-3 (shelf, plain GNU tar) is a fully independent backend by
  design**: driven by `mtx`/`mt` on a separate library partition. This is
  the implementation-diversity requirement doing its job — routing copy-3
  through the remanence daemon would put the same robotics/positioning/
  inventory code (and its latent bugs) in front of all three copies.
  Copy-3 is therefore an *anti*-requirement for the lease: it must never
  be migrated onto these APIs, and its existence must not be cited as the
  reason to build them.
- **dwara2** coexistence is isolated a level up, by library partition;
  per-tape ownership has no job when the changer scope is the ownership
  boundary.
- No third-party stack shares a remanence-managed logical library today.

**Revival triggers** — revisit Parts 2–3 when any of these become real:

1. A second software stack must share one *logical* library (not just one
   chassis) with remanence.
2. A genuine need to write a non-native layout on a remanence-managed
   tape (e.g. an LTFS interchange tape for a third party).
3. An external tool must borrow drives that remanence owns, with safe
   return.
4. Native multi-layout support (remanence-initialized tapes whose
   `layout_format_id` is not REM-PARITY).

## 6. Non-goals (unchanged on revival)

No streaming proxy; no LTFS implementation; no in-daemon third-party
layout *write* drivers (foreign-format reading stays on the existing
format-driver registry path); no native catalog schema generalization —
the catalog is part of the bundled filesystem, and that is correct.
