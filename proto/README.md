# Remanence Layer 5 — gRPC API protos

**Status: implementation draft.** `layer5.proto` is compiled by
`crates/remanence-api`, but it is not a wire-stable contract and has no
published version. Expect breakage while Layer 5 fills out the remaining
services.

## Why this exists now

Layer 5 is partially implemented: the Daemon, Catalog, WriteSessionService,
ReadSessionService, operations, LibraryService inspection/mutation methods, UDS
transport, and mTLS TCP transport are live in `crates/remanence-api` and
`crates/remanence-daemon`. Authorization depth, audit-query RPCs, ranged reads,
and live library events remain. The orchestrator above Remanence —
[Sutradhara](https://github.com/archivetechie/sutradhara) — needs to design against
the gRPC contract to avoid pinning architecture to assumptions that do not
survive contact with the real API. Keeping the proto explicit:

1. Forces the contract into a single artifact, instead of being scattered across spec sections.
2. Lets Sutradhara design its `StorageBackend` trait against a concrete interface.
3. Surfaces holes in the Remanence spec that only show up when you try to write the messages.

## Authoritative references

The proto is derived from the consolidated spec; if they conflict, the spec wins:

- `docs/spec-v0.4.md` — consolidated source of truth, especially §4 (orchestrator boundary and caller-object-id pattern), §8 (rem-tar-v1 body format), §9 (Layer 3c parity/session semantics), §10 (Layer 4 state, audit, SQLite, idempotency), and §11 (Layer 5 gRPC API).

## What's pinned vs. what's not

| Pinned (high confidence) | Not pinned (placeholder) |
|---|---|
| mTLS transport | Authorization scopes / role model |
| Service boundary: Daemon, LibraryService, Catalog, WriteSessionService, ReadSessionService, Audit | Exact wire-stability promise / proto version field |
| caller-object-id + caller-metadata on every write | Body-format-specific `AppendObjectStart.body_format_manifest` shape |
| Idempotency key on every state-changing RPC | Notification streams for multi-tenant scenarios |
| Server-streaming for: enumerate, watch operation, read bytes, library events | Quota / rate-limit semantics |
| Object identity by Remanence UUID; content addressed by SHA-256 | Multi-library naming/routing details |
| Native object catalog hot path stays `Catalog.EnumerateObjects`; cross-source native/foreign discovery is additive via `Catalog.EnumerateUnits` | Persistent per-foreign-entry cache shape |
| Tape-pool visibility: `ListTapePools`, `Tape.pool_id`, `ObjectCopy.pool_id`, and pool-targeted write-session requests | Pool assignment management RPC shape |
| Sessions are first-class with explicit Open/Close/Abort/Checkpoint | Cross-tape transactional semantics |

## Generation

Rust generation is wired through `crates/remanence-api/build.rs` using
`tonic-prost-build`; generated bindings are included by
`crates/remanence-api/src/lib.rs` with `tonic::include_proto!`. The checked-in
source of truth remains this `.proto` file, not generated Rust.

Python generation for Sutradhara is not shipped from this repository.

## Versioning policy (when this graduates to v1)

- Package name `remanence.api.v1` is reserved for the first stable contract.
- Until then, breaking changes are allowed without ceremony — but they should be discussed in `docs/` first.
- After v1: additive only. Field deprecation rather than removal; new RPCs in new services rather than churning existing ones.

## Open questions tracked here

These are visible in the `.proto` as comments but worth surfacing:

1. **Format-adapter message shapes.** `AppendObjectStart.body_format_manifest` is currently `bytes`. The format adapter (rem-tar-v1, rem-tar-legacy, rem-bru) defines the inner shape. Should this be a `oneof` per known format, or stay opaque? Trade-off: type safety vs. format pluggability.
2. **Multi-library routing.** Most RPCs take a `library_uuid`. For deployments with multiple libraries this is fine; for the single-library default it's noise. Consider a `DefaultLibrary` convention.
3. **Operation cancellation completion-uncertainty.** `OperationStatus.state == UNKNOWN` covers Layer 4's `CompletionUnknown`. Make sure clients are guided to GetOperation again after a delay rather than treating UNKNOWN as terminal.
4. **Foreign entry caching.** `CatalogUnit` supports foreign read-only archive
   units, but v1 should not expose BRU/tar physical locators as public schema.
   Decide after real tape scans whether to persist per-entry cache rows or
   serve `ListEntriesInUnit` by re-running the driver's scan.
5. **Tape-pool assignment management.** The catalog exposes pool definitions
   and current tape membership, and write-session requests can target or guard
   by pool. The operator/API flow for assigning and moving blank tapes between
   pools is still a management-surface question.

Closed in the current implementation: `EnumerateObjects` / `EnumerateUnits`
server back-pressure uses bounded channel-backed streams over read-only SQLite
query handles, so the daemon does not materialize full catalog scans before
emitting the first response.
