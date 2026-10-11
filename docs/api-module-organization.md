# Remanence API module organization

`remanence-api` is the Layer 5 composition crate. It owns the gRPC services,
daemon state, tape-pool write orchestration, and the drive and changer actors
that serialize hardware access. This page maps its source files to those
responsibilities, for someone who needs to find where a behavior lives.

Three files, `lib.rs`, `write_owner.rs` and `pool_write.rs`, are façades: short
files that re-export the implementation from focused private modules.
Callers use root paths such as `remanence_api::ApiState`,
`remanence_api::LtoGen` and `remanence_api::WriteObjectSource`.

## Top-level responsibilities

| Area | Modules | Responsibility |
|---|---|---|
| Crate façade | `lib.rs` | generated protobuf modules, stable public re-exports, Unix-channel connector |
| Daemon composition | `api_state.rs`, `live_status.rs`, `startup_guard.rs`, `startup_checkpoint.rs`, `startup_media_readiness.rs`, `drive_collection.rs` | construct shared state, start actors and recovery, collect drive status, reject unsafe startup |
| gRPC services | `daemon_catalog_services.rs`, `write_session_ingress.rs`, `read_session_service.rs`, `audit_query_service.rs`, `library.rs`, `read_plan.rs` | authorize and translate individual Layer 5 RPC families |
| Catalog and audit boundaries | `catalog_request.rs`, `catalog_conversion.rs`, `audit_projection.rs` | validate catalog requests, convert catalog records, append and project audit facts |
| Shared write boundaries | `append_request.rs`, `append_spool.rs`, `write_admission.rs`, `auth.rs` | validate ingress, own temporary spools, coordinate tape-write admission, authorize requests |

`write_admission.rs` is deliberately neutral. Both the pool writer and the
drive owner depend on it, so neither implementation module needs to depend on
the other merely to coordinate an exclusive tape write.

## Drive-owner modules

`write_owner.rs` is the actor-facing façade. Its child modules each own one
operational concern:

- `actor_protocol.rs` and `actor_runtime.rs`: command types and actor loops.
- `readiness.rs`: load-time media-readiness probing and durable fences.
- `checkpoint.rs`: parity session authority and checkpoint barriers.
- `write_session.rs`: the mounted write-session state machine.
- `terminal_types.rs` and `terminal_finalize.rs`: irreversible finalization,
  recovery, and terminal-tail completion.
- `terminal_inventory.rs` and `bot_recovery.rs`: terminal-index inventory and
  verification, and recovery by walking the tape from its beginning when the
  terminal index cannot be used.
- `read_session.rs` and `restore.rs`: read-session opening and streamed reads.
- `robotics.rs`, `cleaning.rs`, and `reconcile.rs`: changer motion, cleaning,
  and catalog reconciliation.

The append-finish state transition is a single method inside
`write_session.rs`, so the state machine stays in one place.

## Pool-writer modules

`pool_write.rs` is the stable write API and re-exports:

- `model.rs`: public requests, results, errors, and copy records.
- `selection.rs`: pool selection and pinned-tape admission.
- `capacity.rs` and `media.rs`: geometry, capacity, watermarks, and LTO
  compatibility.
- `prepare.rs`: source validation and canonical object preparation.
- `staging.rs` and `overlap.rs`: bounded pipelined transfer machinery.
- `direct.rs`: direct checkpointed writes and terminal close.
- `no_parity.rs`: no-parity object write, projection, and replay.

## Size guideline

`lib.rs`, `write_owner.rs` and `pool_write.rs` are each under 200 lines. The
largest implementation file is `mount.rs`, at roughly 4,200 lines. New concepts
get focused modules instead of growing one of the façades.

Unit tests live in three shared out-of-line suites, `src/tests.rs`,
`src/write_owner/tests.rs` and `src/pool_write/tests.rs`, which keep access to
the modules' private items. Some other modules, such as `read_core.rs`, keep
their tests inline.
