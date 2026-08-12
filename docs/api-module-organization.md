# Remanence API module organization

`remanence-api` is the Layer 5 composition crate. It owns the gRPC services,
daemon state, tape-pool write orchestration, and the drive/changer actors that
serialize hardware access.

The crate once concentrated most of that work in three files. Before the
organization pass, `lib.rs`, `write_owner.rs`, and `pool_write.rs` contained
15,423, 18,479, and 14,196 lines respectively. Their public paths were useful,
but their implementation boundaries were not: unrelated changes repeatedly
met in the same files and a write-admission dependency ran in both directions
between the pool writer and drive owner.

Those three files are now façades. Existing callers still use root paths such
as `remanence_api::ApiState`, `remanence_api::LtoGen`, and
`remanence_api::WriteObjectSource`; the façades re-export the implementations
from focused private modules. The refactor changes neither protobuf messages
nor media/state-machine behavior.

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

`write_owner.rs` retains the actor-facing façade. Its child modules each own
one operational concern:

- `actor_protocol.rs` and `actor_runtime.rs`: command types and actor loops.
- `readiness.rs`: load-time media-readiness probing and durable fences.
- `checkpoint.rs`: parity session authority and checkpoint barriers.
- `write_session.rs`: the mounted write-session state machine.
- `terminal_types.rs` and `terminal_finalize.rs`: irreversible finalization,
  recovery, and terminal-tail completion.
- `terminal_inventory.rs`: terminal-index inventory and verification.
- `read_session.rs` and `restore.rs`: read-session opening and streamed reads.
- `robotics.rs`, `cleaning.rs`, and `reconcile.rs`: changer motion, cleaning,
  and catalog reconciliation.

The append-finish state transition remains a single coherent method inside
`write_session.rs`. Moving it preserved the existing state machine; breaking
that transition into smaller behavioral steps would be a separate semantic
change and was intentionally not hidden inside this organization pass.

## Pool-writer modules

`pool_write.rs` retains the stable write API and re-exports:

- `model.rs`: public requests, results, errors, and copy records.
- `selection.rs`: pool selection and pinned-tape admission.
- `capacity.rs` and `media.rs`: geometry, capacity, watermarks, and LTO
  compatibility.
- `prepare.rs`: source validation and canonical object preparation.
- `staging.rs` and `overlap.rs`: bounded pipelined transfer machinery.
- `direct.rs`: direct checkpointed writes and terminal close.
- `no_parity.rs`: no-parity object write, projection, and replay.

## Size and review guardrails

After the pass, `lib.rs`, `write_owner.rs`, and `pool_write.rs` are 161, 139,
and 54 lines. The largest new implementation module is roughly 2,250 lines;
the largest pre-existing API implementation file is `mount.rs`, at roughly
4,200 lines. New concepts should receive focused modules rather than growing
one of the façades.

The former inline unit suites are now out-of-line test modules. They retain
their original test names and private-item access, keeping the organization
change reviewable without rewriting fixtures at the same time. The crate's
unit, documentation, formatting, lint, workspace, release-build, and
clean-slate scenario gates remain the behavioral proof for this refactor.

## Verification evidence

The decomposition is committed at Remanence
`f559f17d5581c29e0b8552172cc424837c22966b`. At that head:

- `cargo test -p remanence-api --all-targets` passed 515 unit tests and one
  documentation test.
- `cargo test --workspace --all-targets`, `cargo fmt --all -- --check`,
  warning-denied workspace Clippy, and `cargo build --release` all completed
  successfully.
- Clean-slate strict-freshness System scenarios passed for same-tape append,
  operator `put`, parity-checkpoint replay, whole-object crash recovery,
  terminal-index finalization/recovery, and live status. The terminal-index
  run completed all 12 steps, including manual early finalization and its
  fault/recovery matrix.

These are behavior checks, not merely source-layout checks: they exercise the
extracted ingress, pool selection, mounted write-session, checkpoint,
terminal-finalization, inventory, readback, and recovery paths through the
release daemon and CLI.

## Independent incremental review

The exact range from the last recorded clean baseline
`7f6861c2f5b7dcd8237e22696c15d229ee0ccb3e` through evidence head
`bbd28c4d3b4b55401cdee9a90fe187a3943d897c` passed two consecutive
independent incremental reviews on 2026-08-12 with zero actionable findings.
The reviewers checked the moved function set, public re-exports, module
dependency direction, focused façades, relevant live receipts, and the clean
worktree. Untouched regions retain their previously reviewed-clean status.
