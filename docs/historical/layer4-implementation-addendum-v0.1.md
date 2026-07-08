# Remanence Layer 4 Implementation Addendum

**Status:** Superseded by `docs/layer4-implementation-addendum-v0.2.md`. Kept as the v0.1 review snapshot.  
**Applies to:** `docs/spec-v0.3.md`, `docs/layer3c-design.md` (Layer 3c v0.7.2 active contract)  
**Purpose:** Turn the current "Layer 4 = local state" architecture sketch into an implementation contract that is precise enough to build before Layer 5.

This addendum does not change the current architecture:

- Remanence has no hosted database dependency.
- The tape and the per-tape 3c journal are the authoritative write/recovery record for tape contents.
- Layer 4 is local daemon state: config, audit log, and rebuildable query indexes.
- Layer 5 remains the API/session/orchestration layer.

The old "Layer 4 = Postgres catalog" model is superseded. A hosted RDBMS, if an external orchestrator wants one, is outside Remanence.

---

## 1. Scope

Layer 4 owns the local state needed by the daemon across restarts:

1. Operator configuration and allowlists.
2. Append-only audit log.
3. Rebuildable local query index over 3c journals and tape catalogs.
4. Idempotency and operation/session projections derived from the audit log.
5. State-directory locking and migrations.

Layer 4 does not own:

- SCSI commands, drive positioning, media movement, or hot-plug detection.
- Tape body formats, tar layout, object chunking, or file metadata encoding.
- Parity encoding, bootstraps, sidecars, or `TapeFileJournal` semantics.
- gRPC, mTLS, request routing, session policy, or cancellation policy.
- Cross-copy placement, retention policy, dedupe, scheduling, or fleet databases.

Layer 4 may expose helpers that Layer 5 uses, but it must not become an orchestrator.

---

## 2. Authority Model

Layer 4 has three kinds of state. They must stay separate.

| State | Authority | Loss behavior |
|--|--|--|
| Operator config | `/etc/rem/config.toml` | Daemon cannot safely start without valid config |
| Audit log | Local append-only hash chain | Authoritative for daemon-local actions and idempotency history |
| 3c tape-file journals | Per-tape `FileTapeFileJournal` files | Authoritative commit record for tape-file append/resume |
| SQLite query index | Derived projection | Can be deleted and rebuilt |
| Tape catalog cache | Derived from tape / 3b catalog | Can be deleted and rebuilt |

The SQLite index is never a commit point. It is a query accelerator. Losing it must not lose knowledge that cannot be rebuilt from the audit log, journals, config, or tape.

The 3c journal is also not a general catalog. It is deliberately narrow: the ordered committed tape files and parity watermarks needed for append/resume and recovery. Layer 4 indexes it for queries, but 3c remains generic over `TapeFileJournal`.

Layer 4 must read 3c journals only through the 3c read-only replay surface (`TapeFileJournal::load_committed()` or its successor). It must not reparse `.remjournal` bytes directly; the on-disk framing, CRC rules, torn-record handling, and trusted-volume policy remain owned by Layer 3c.

---

## 3. On-Disk Layout

Default paths:

```text
/etc/rem/config.toml

/var/lib/rem/
  state.lock
  audit/
    2026-05-25.remaudit
    2026-05-26.remaudit
  journals/
    <tape_uuid>.remjournal
  index/
    rem-state.sqlite
  cache/
    tapes/
      <tape_uuid>.cat
    aliases/
      <voltag>.json
```

Path roots are configurable for tests, but production deployments should keep config in `/etc/rem` and mutable state under `/var/lib/rem`.

Layer 4 must acquire an exclusive `state.lock` before opening mutable state. A second daemon instance must fail at startup rather than share the state directory.

State directories are created `0700` by default. Files containing only non-secret operational metadata may be group-readable if the operator explicitly configures a `rem-operators` group. Layer 4 must not store long-term secrets; mTLS private keys and encryption keys belong to their own security design.

---

## 4. Config

`/etc/rem/config.toml` is authoritative operator intent.

Minimum schema:

```toml
[daemon]
state_dir = "/var/lib/rem"
default_idle_timeout_seconds = 1800
read_only = false

[[libraries]]
serial = "7CBAD9CF74"
allow_derived_drive_identity = false

[journal]
dir = "/var/lib/rem/journals"
require_trusted_volume = true

[audit]
dir = "/var/lib/rem/audit"
fsync = true

[index]
sqlite_path = "/var/lib/rem/index/rem-state.sqlite"

[cache]
tape_catalog_dir = "/var/lib/rem/cache/tapes"
```

Validation rules:

- Library serials are unique.
- `allow_derived_drive_identity=true` is allowed only for an explicitly listed library serial.
- Journal and audit directories must not be tmpfs, network filesystems, or other untrusted-flush volumes.
- If `read_only=true`, Layer 5 may serve queries and reads but must reject state-changing writes, moves, imports, exports, and write-session opens.
- Unknown keys are rejected until a migration explicitly accepts them.

Config reload is not required for v0.1. Layer 5 may add a controlled reload later, but the first implementation should read config at daemon startup only.

---

## 5. Audit Log

The audit log is the durable record of daemon-local actions. It answers:

- who requested an operation;
- what the daemon attempted;
- what hardware/API result was observed;
- what idempotency key maps to which operation;
- what sessions were opened, closed, orphaned, failed, or lost by restart;
- what recovery warnings were emitted by lower layers.

### 5.1 File Format

Use one append-only file per UTC calendar day:

```text
audit/YYYY-MM-DD.remaudit
  fixed header:
    magic = b"REMAUD\x01"
    schema_version
    segment_date
    previous_segment_terminal_hash
    header_crc64

  repeated records:
    offset  size   field
    0x00    4      u32le record_len, counting the bytes after this length field
    0x04    N      canonical_cbor(AuditRecordWithoutRecordHash)
    0x04+N  32     record_hash
    0x24+N  8      u64le record_crc64
```

`record_hash` is:

```text
SHA256(previous_record_hash || canonical_cbor(AuditRecordWithoutRecordHash))
```

`record_crc64` is CRC-64/XZ over `record_len || canonical_cbor(...) || record_hash`, with `record_len` stored little-endian exactly as it appears on disk. The first record of a segment chains to `previous_segment_terminal_hash`. The first segment uses all zeroes. A segment header's `previous_segment_terminal_hash` is the explicit rotation-boundary marker; no in-band record is used for rotation.

On replay:

- a torn trailing record is ignored;
- a CRC/hash failure before EOF is tamper/corruption and startup fails closed;
- sequence gaps are corruption unless the segment header explicitly marks a rotation boundary.

Layer 4 should provide `rem audit export-json` later for human-readable inspection. The durable format should be robust first, pretty second.

### 5.2 Audit Record

Minimum record shape:

```rust
pub struct AuditRecord {
    pub schema_version: u16,
    pub record_uuid: Uuid,
    pub sequence: u64,
    pub timestamp_utc: String,
    pub host_id: String,
    pub process_id: u32,
    pub actor: AuditActor,
    pub source_layer: SourceLayer,
    pub operation_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub idempotency_key: Option<Uuid>,
    pub event: AuditEvent,
    pub subject: AuditSubject,
    pub detail: BTreeMap<String, CborValue>,
}
```

Timestamps are RFC3339 UTC. Ordering is by `sequence`, not by wall clock. If the wall clock moves backward, Layer 4 emits `ClockRegressionObserved` but continues with a monotonic sequence.

Minimum `AuditEvent` variants:

```text
RequestReceived
OperationStarted
OperationProgress
OperationFinished
OperationFailed
CancelRequested
CancelledBeforeDispatch
CompletedAfterCancel
CancellationRejected
CompletionUnknown
SessionOpened
SessionCheckpointed
SessionClosed
SessionOrphaned
SessionLostByRestart
HardwareWarning
RecoveryEvent
ConfigLoaded
ConfigRejected
IndexRebuilt
ReadOnlyModeEntered
ReadOnlyModeLeft
AuditWriteFailed
```

Layer 2 `LibraryAuditHook` events and Layer 3c recovery events must be representable without stringly typed lossy conversion. Stable enum tags are required.

### 5.3 Audit Durability Rules

For state-changing operations, Layer 5 must append and fsync `OperationStarted` before dispatching the first irreversible CDB or write call.

For completion, Layer 5 must append and fsync the terminal event after the lower layer returns. If that terminal audit append fails after an irreversible action has completed, Layer 5 must:

1. return an explicit audit-durability error to the client;
2. enter read-only/degraded mode;
3. require operator intervention or successful audit repair before accepting new state-changing work.

The data already committed to tape/journal remains valid. The degraded mode is about not continuing without an audit trail.

---

## 6. SQLite Query Index

Layer 4 owns an embedded SQLite file as a projection. It is not a database server and not an authority.

Some 3c text calls this the "Layer 5 local catalog" because Layer 5 consumes it for API queries. This addendum makes the implementation boundary explicit: the SQLite file and rebuild logic live in `remanence-state`; Layer 5 accesses them through typed Layer 4 methods.

Use SQLite for indexed queries that would be painful to answer by scanning every journal on every request:

- tapes known to the daemon;
- tape file layout by tape;
- object copies by object id;
- parity/protection watermarks;
- idempotency key lookup;
- operation/session summary lookup;
- ingestion offsets for audit and journal replay.

SQLite configuration:

```text
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
PRAGMA foreign_keys=ON;
PRAGMA user_version=<schema_version>;
```

The implementation may use `rusqlite`. It must be linked into the daemon; no external DB process is allowed.

### 6.1 Minimum Tables

```text
schema_meta(
  key text primary key,
  value blob not null
)

ingested_sources(
  source_kind text not null,       -- audit | tape_journal | tape_catalog
  source_id text not null,
  offset_bytes integer not null,
  terminal_hash blob,
  updated_at_utc text not null,
  primary key(source_kind, source_id)
)

tapes(
  tape_uuid blob primary key,
  voltag text,
  block_size integer,
  scheme_id text,
  data_blocks_per_stripe integer,
  parity_blocks_per_stripe integer,
  stripes_per_neighborhood integer,
  highest_protected_ordinal integer not null default 0,
  total_committed_ordinals integer not null default 0,
  last_committed_tape_file integer,
  state text not null,
  updated_at_utc text not null
)

tape_files(
  tape_uuid blob not null,
  tape_file_number integer not null,
  kind text not null,
  block_count integer not null,
  physical_start_hint integer,
  object_id text,
  first_parity_data_ordinal integer,
  epoch_id integer,
  protected_ordinal_start integer,
  protected_ordinal_end_exclusive integer,
  canonical_metadata_hash blob,
  bundle_uuid text,
  bundle_kind text,
  primary key(tape_uuid, tape_file_number)
)

objects(
  object_id text primary key,
  caller_object_id text,
  body_format text,
  logical_size_bytes integer,
  content_hash blob,
  metadata_hash blob,
  created_at_utc text not null
)

object_copies(
  object_id text not null,
  tape_uuid blob not null,
  tape_file_number integer not null,
  first_parity_data_ordinal integer,
  protected_until_ordinal integer,
  status text not null,
  primary key(object_id, tape_uuid, tape_file_number)
)

idempotency_keys(
  actor_fingerprint text not null,
  idempotency_key text not null,
  request_fingerprint blob not null,
  operation_id text not null,
  terminal_state text,
  response_fingerprint blob,
  updated_at_utc text not null,
  primary key(actor_fingerprint, idempotency_key)
)

operations(
  operation_id text primary key,
  operation_kind text not null,
  state text not null,
  session_id text,
  subject text,
  started_at_utc text not null,
  updated_at_utc text not null
)

sessions(
  session_id text primary key,
  session_kind text not null,      -- read | write | admin
  tape_uuid blob,
  library_serial text,
  drive_bay integer,
  state text not null,
  opened_at_utc text not null,
  updated_at_utc text not null
)
```

This is the minimum shape. Layer 5 may add service-specific projection tables, but those tables must be rebuildable from audit, journals, and tape catalogs.

### 6.2 Projection Rules

The index is updated only after the authoritative source has committed.

For a 3c write:

```text
write blocks
write synchronous filemark(s)
3c commit_bundle() returns after journal fsync
Layer 4 indexes the new CommittedBundle
Layer 5 emits terminal audit event
```

If the daemon crashes after `commit_bundle()` but before SQLite update, replaying the journal must bring SQLite up to date.

If the daemon crashes after SQLite update but before a terminal audit event, startup replay must infer the operation is non-terminal and append `SessionLostByRestart` / `OperationFailedByRestart` as appropriate before accepting new work.

SQLite must never contain committed tape files that are absent from `load_committed()` for the corresponding journal after replay.

### 6.3 SQLite Reconciliation

Startup treats SQLite as disposable projection state. After audit and journal replay, Layer 4 compares each projection table's recorded source offsets against the authoritative audit segments, 3c journal replay results, and tape catalog cache digests.

If SQLite contains rows absent from the authoritative source, or if source offsets cannot be trusted, Layer 4 must rebuild the affected projection from scratch before accepting work. The simplest v0.1 implementation may delete and rebuild the full SQLite database. It must not attempt to keep orphan rows and mark them stale.

---

## 7. Tape Catalog Cache

The tape catalog cache is a local copy of the 3b on-tape catalog for query speed.

Rules:

- Cache files are derived state.
- Cache keys use `tape_uuid` once known.
- `voltag` aliases are allowed only as pointers to a current `tape_uuid`.
- A cache entry must include the digest of the on-tape catalog it mirrors.
- If the cache digest disagrees with the mounted tape, discard and rebuild the cache.
- If the cache is missing, Layer 5 may still mount the tape and read the on-tape catalog.

The cache must not be used to decide append position for parity-protected tapes. Append/resume uses the 3c journal first, then tape scan/bootstrap validation if needed.

---

## 8. Idempotency

Layer 5 write-initiating RPCs use idempotency keys. Layer 4 provides the durable projection.

Rules:

- Key scope is `(actor_fingerprint, idempotency_key)`.
- The first request stores `request_fingerprint = SHA256(canonical request excluding transport metadata)`.
- A retry with the same key and same request fingerprint returns or reattaches to the original operation.
- A retry with the same key but different request fingerprint is rejected as `IdempotencyConflict`.
- Terminal response summaries are persisted in the audit log and projected into SQLite.
- If SQLite is lost, replaying audit reconstructs idempotency mappings.

In-progress operations after daemon restart become terminal `LostByRestart` records unless the operation's layer explicitly supports durable reattachment. For v0.1, write/read sessions are not reattached across daemon restart; the tape/journal recovery path handles the data state.

---

## 9. Startup and Replay

Daemon startup sequence:

1. Acquire `state.lock`.
2. Load and validate config.
3. Open audit log; verify hash chain.
4. Open SQLite; run migrations.
5. Replay audit segments into the operation/idempotency/session projections.
6. Replay all known 3c journals into tape/tape_file/object-copy projections.
7. Mark non-terminal sessions from the prior daemon process as `LostByRestart` by appending audit records and projecting those records directly into the in-memory/SQLite state; do not rerun a full audit replay during this step.
8. Rebuild or invalidate stale tape catalog caches.
9. Hand a ready `StateHandle` to Layer 5.

Startup must fail closed if:

- config is invalid;
- audit hash chain has non-trailing corruption;
- state directory lock cannot be acquired;
- migrations fail;
- journal replay returns a header mismatch for a known tape;
- the journal/audit directory is on an untrusted volume when trusted volume checking is enabled.

A torn trailing audit record or torn trailing 3c journal record is not corruption; it is an interrupted write that did not commit.

---

## 10. Layer 4 Public Surface

The crate should be `crates/remanence-state`.

Minimum Rust surface:

```rust
pub struct StatePaths {
    pub config_path: PathBuf,
    pub state_dir: PathBuf,
    pub audit_dir: PathBuf,
    pub journal_dir: PathBuf,
    pub sqlite_path: PathBuf,
    pub tape_cache_dir: PathBuf,
}

pub struct StateHandle { /* single-writer owner */ }

impl StateHandle {
    pub fn open(paths: StatePaths) -> Result<Self, StateError>;
    pub fn config(&self) -> &RemConfig;
    pub fn audit(&mut self) -> &mut dyn AuditSink;
    pub fn catalog_index(&mut self) -> &mut CatalogIndex;
    pub fn idempotency(&mut self) -> &mut IdempotencyStore;
    pub fn journal_path(&self, tape_uuid: [u8; 16]) -> PathBuf;
    pub fn rebuild_index_from_journals(&mut self) -> Result<RebuildReport, StateError>;
}

pub trait AuditSink {
    fn append(&mut self, event: AuditEventRecord) -> Result<AuditReceipt, AuditError>;
}
```

Layer 4 should also provide adapters:

- `LibraryAuditHook` adapter for Layer 2 events.
- 3c recovery/audit event adapter.
- JSON export for audit inspection.
- `rebuild-catalog-from-journals` command entry point for the CLI/API layer.

Do not expose raw SQLite connections to Layer 5. Layer 5 should use typed query/update methods so that "derived, rebuildable, never authoritative" remains enforceable.

---

## 11. Error Model

Minimum errors:

```text
ConfigInvalid
StateLockHeld
UntrustedStateVolume
AuditCorrupt
AuditTornTrailingRecord
AuditWriteFailed
DiskFull
JournalReplayFailed
IndexMigrationFailed
IndexCorrupt
IdempotencyConflict
CatalogCacheDigestMismatch
StateLockStale
ReadOnlyMode
PermissionDenied
```

Layer 5 maps these into gRPC status codes, but Layer 4 must preserve structured causes.

---

## 12. Implementation Steps

| Step | Description |
|--|--|
| 4.0 | Scaffold `crates/remanence-state`, errors, path handling, and exclusive state lock. |
| 4.1 | Implement config loading and validation for `/etc/rem/config.toml` plus test-path overrides. |
| 4.2 | Implement `FileAuditLog`: header, framed records, hash chain, fsync, rotation, replay, JSON export. |
| 4.3 | Implement SQLite migrations and typed `CatalogIndex` wrapper. |
| 4.4 | Implement journal ingestion through `TapeFileJournal::load_committed()` into `tapes`, `tape_files`, and object-copy projections. |
| 4.5 | Implement audit replay into `operations`, `sessions`, and `idempotency_keys`. |
| 4.6 | Add Layer 2 and 3c audit adapters. |
| 4.7 | Add `rebuild-catalog-from-journals` and wipe/rebuild tests. |
| 4.8 | Add startup replay tests for crash windows between journal commit, SQLite update, and terminal audit append. |
| 4.9 | Add no-database-process gate: tests must pass with no Postgres/mysql service available. |

Layer 5 should not start until 4.0-4.5 are usable. Steps 4.6-4.9 can continue in parallel with the first Layer 5 skeleton if the public surface is stable.

---

## 13. Required Tests

Unit and integration tests:

1. Config rejects duplicate libraries, unknown keys, bad paths, and untrusted journal/audit volumes.
2. State lock rejects a second writer.
3. Audit append/replay round-trips records and preserves sequence order.
4. Audit replay drops one torn trailing record but rejects mid-log corruption.
5. Audit rotation chains the next segment to the previous terminal hash.
6. Clock regression emits an audit warning but preserves sequence order.
7. SQLite migrations are idempotent and preserve existing rows.
8. Indexing a committed 3c bundle creates the expected `tapes` and `tape_files` rows.
9. Wiping SQLite and rebuilding from journals produces an equivalent query index.
10. Crash after journal commit but before SQLite update is repaired by replay.
11. Crash after SQLite update but before terminal audit append marks the prior session/operation lost by restart.
12. Idempotency retries with the same key/request return the original operation.
13. Idempotency retries with the same key but changed request reject with `IdempotencyConflict`.
14. Catalog cache digest mismatch discards the cache and requires rebuild from tape.
15. No test requires a hosted database process.

Hardware-adjacent tests:

1. Run Layer 4 journal ingestion against the QuadStor 3c live-test journals.
2. Run wipe/rebuild after a full QuadStor write/read/recover cycle.
3. On the deployed filesystem, forced power loss must not produce a replay state beyond the audit/journal records whose fsync returned.

The forced power-loss gate can be deferred until hardware qualification, but the test harness should be designed now.

---

## 14. Acceptance Criteria

Layer 4 v0.1 is complete when:

```text
A. The daemon has one exclusive local state owner.
B. Config is validated and rejects unsafe state/journal/audit locations.
C. Audit records are hash-chained, fsync'd, replayable, and tamper-evident.
D. A torn trailing audit record is ignored; mid-log corruption fails closed.
E. SQLite is a rebuildable projection, not an authority.
F. Rebuilding SQLite from 3c journals produces equivalent tape/tape-file/object-copy queries.
G. Idempotency survives process restart by audit replay.
H. Layer 2 and 3c events can be recorded without lossy string conversion.
I. Crash windows around journal commit, index update, and audit terminal events have tests.
J. No hosted database process is required or referenced by the implementation.
```

Only after A-J are true should Layer 5 rely on Layer 4 for write/read session orchestration.

---

## 15. Open Questions

These should be resolved before broad Layer 5 work:

1. Should JSON audit export be a stable compatibility surface, or only an operator/debug convenience? Recommendation: debug convenience until v1.
2. Should the tape catalog cache store exact 3b catalog bytes or a normalized CBOR form? Recommendation: exact bytes plus parsed projection.
3. Should `rem-state.sqlite` contain both query catalog and operation/idempotency projections, or split them into two SQLite files? Recommendation: one file for v0.1, with clear table ownership and rebuild rules.
4. Should a terminal audit append failure after a successful irreversible hardware action return success-with-warning or a hard error? Recommendation: hard error plus read-only/degraded mode, because audit durability is an explicit product promise.
