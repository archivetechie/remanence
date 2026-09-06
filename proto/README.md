# Remanence Layer 5 — gRPC API protos

**Status: implemented, pre-stability contract.** `layer5.proto` is compiled by
`crates/remanence-api` and served by `rem-daemon`. It uses the
`remanence.api.v1` package namespace, but the project has not made a wire
stability promise for that namespace. Breaking changes remain possible while
the software is alpha and must be coordinated with known generated-binding
consumers.

## Scope

The file is the single source of truth for the Layer 5 wire contract. It
defines Daemon, LibraryService, Catalog, WriteSessionService,
ReadSessionService, ReadPlanService, and Audit services. The implementation
includes Unix-socket transport, optional mTLS TCP transport, role-based
authorization, catalog and audit streams, pool-targeted or pinned-tape write
sessions, whole-object and ranged reads, read planning, library inspection and
robotics, drive stewardship, alarms, and live status.

Some declared operations intentionally return `UNIMPLEMENTED`; the current
list belongs in [the status page](../docs/status.md), not in a second contract
copy here.

## Authoritative references

- [`layer5.proto`](layer5.proto) is authoritative for field numbers, presence,
  services, and RPC behavior.
- [REM-OBJECT](../specs/publication/rem-object-core-1-specification.md) and
  [REM-ENCRYPT](../specs/publication/rem-encrypt-1-specification.md) define the
  object and envelope bytes carried through the API.
- The current generation-2 tape implementation follows the
  [REM-PARITY revision in preparation](../specs/in-progress/rem-parity-1-specification.md),
  while the publication candidate still describes generation 1.
- [Architecture](../docs/architecture-overview.md) and
  [configuration](../docs/reference-configuration.md) describe deployment and
  transport trust boundaries.

## Stable intent and evolving surfaces

| Stable intent | Still evolving or explicitly unsupported |
| --- | --- |
| Explicit library identity; no implicit single-library default | Exact wire-stability declaration for the `remanence.api.v1` namespace |
| REM-OBJECT identity, digests, and opaque body-format manifests | Caller-supplied idempotency keys: fields are reserved and non-empty values are rejected with `UNIMPLEMENTED` |
| Native object catalog plus additive native/foreign unit discovery | Persistent per-entry cache shape for foreign formats |
| Pool-targeted and pinned-tape write admission | Runtime pool-assignment mutation; pool membership is configuration/catalog policy |
| Server streaming with bounded producer channels | Write-session restart and the operations listed as unimplemented in `docs/status.md` |
| Explicit open/close/abort/checkpoint session lifecycle | Cross-tape transaction semantics |

Foreign-format metadata stays behind opaque adapter-owned bytes rather than a
wire `oneof`; the core API does not acquire BRU- or tar-specific locator types.
Multiple-library routing stays explicit because several libraries can expose
the same element addresses and must never be selected by convenience default.

## Generation

Rust generation runs from `crates/remanence-api/build.rs` through
`tonic-prost-build`; generated bindings are included with
`tonic::include_proto!`. The checked-in `.proto` is the source of truth, not
generated Rust.

Python bindings used by consumers are generated and maintained by those
consumers; this repository does not ship them.

## Compatibility policy

Before a wire-stability declaration, breaking changes require coordinated
consumer updates but no compatibility guarantee is implied by the package
suffix. Once the namespace is declared stable, changes are additive: fields
are deprecated rather than reused or removed, and new behavior is introduced
through new fields, RPCs, or services. The wire-presence census in CI prevents
new scalar fields from entering the contract without an explicit presence
decision.
