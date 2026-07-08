# Layer 5 multi-object tape append -- Design v0.2

**Status:** panel folded + verify-r1 fixes applied (2026-07-05);
MTA-1 implementation started. The first code slice enables core no-parity
pool-write/catalog append for sequential objects on the same tape session.
The second code slice adds a strict no-parity live append projection path that
rejects non-contiguous tape-file extension, overlapping tape-file rows,
geometry/protection mismatch, non-ready tape state, and object/copy identity
conflicts before object rows become visible. The third code slice exposes
locator-derived `AppendCommitInfo` through gRPC `ObjectRecord`, CLI locator
JSON, and `remfield-io` write evidence; journal ordinal, position proof, voltag,
sealed-after-write, and remaining-capacity fields are omitted/null until durable
append records land. The fourth code slice adds the physical fieldtest
`13-append-loop.sh` and updates the same-day runbooks to prove repeated
same-tape no-parity appends without requiring many fresh cartridges; the field
media guards now accept used ready tapes as appendable for ordinary daemon
writes. Durable append records, explicit device reposition/position proof,
system scenario coverage, append-specific rebuild/kill evidence, and MTA-2
parity append remain pending gates. The fifth code slice implements MTA-1 `(pool_id,
caller_object_id)` replay semantics for non-empty caller ids and keeps empty
caller ids accepted but non-idempotent.
**Problem source:** physical MSL3040 field-test preparation exposed that the
current pool writer treats a committed cartridge as unavailable for further
pool writes. That was acceptable for S4a "write one object", but it is not
acceptable for production LTO utilization.
**Panel 2026-07-05:** local lenses for failure modes, parity format/resume,
catalog/API/system contracts, and field-operator practicality, plus GLM 5.2
via OpenRouter (`z-ai/glm-5.2`). Folds are incorporated in this revision.
**Verify r1:** no criticals; five high-precision gaps found. MTA-1 decisions
were pinned in this revision; MTA-2 parity journal ownership and resume
idempotency remain gates before parity prompt cut.
**Precedent docs:** `docs/layer5-roadmap.md`,
`docs/historical/layer5-s4a-write-session-design-v0.1.md`,
`specs/rem-parity-1.0-specification.md`,
`docs/pool-tape-selection-design-v0.1.md`,
`docs/tape-read-object-design-v0.1.md`.

---

## 1. Executive decision

Remanence must support many independently committed objects on one tape.
The extension is an append-session model:

1. A tape has a durable **committed prefix**: every filemark-delimited tape
   file that Remanence has committed, including control files such as file-0
   bootstraps.
2. A pool write chooses either a fresh tape or an appendable tape.
3. On append, the writer validates tape identity/geometry, validates the
   prefix from the append journal and SQLite, positions after the committed
   prefix, writes one new object bundle, writes the synchronous filemark,
   durably records a full Layer 5 append commit record, then projects SQLite.
4. A client success response is impossible before the append commit record is
   fsynced.
5. On restart, SQLite is repaired from append commit records before a tape can
   be selected for another write.

This turns the current "one object per cartridge" behavior into "one atomic
object per append transaction, many transactions per cartridge."

The design has two implementation tracks:

- **MTA-1 no-parity append.** Production unblocker for physical MSL3040 tests:
  many plaintext/encrypted RAO objects on one no-parity tape, protection-neutral
  append journal, full catalog rebuild fidelity, append-aware field scripts,
  and scenario coverage.
- **MTA-2 parity append.** Full REM-PARITY append/resume: exact resume ordering,
  complete sidecar directory metadata, complete bootstrap object rows,
  checkpoint-before-unload, and final-seal behavior.

Both tracks share selection, positioning, capacity, locking, catalog, API
evidence, and scenario contracts.

## 2. Why this is unsupported today

This is an intentional S4a scope limit, not a hardware limitation and not a
format law.

### 2.1 Design scope

`docs/layer5-roadmap.md` names S4a as "write one object via the shared
pool_write core." The archived S4a design explicitly scopes the daemon write
session as one object and defers resume/idempotency/multi-object behavior to a
later slice. The current code follows that narrower contract.

### 2.2 Pool selection rejects used tapes

The S4a baseline in `crates/remanence-api/src/pool_write.rs` filtered every candidate through
`check_writability_preconditions`. That function returns
`NoParityAppendUnsupported` or `ParityAppendUnsupported` when
`tape.total_committed_ordinals > 0`. The selector therefore treats a tape with
one committed object as non-writable even if the tape has terabytes free.

`ensure_selected_tape_accepts_write` repeats the same guard after selection, so
manual selection cannot bypass the one-object policy.

### 2.3 No-parity writer always starts a fresh tape layout

The S4a baseline `write_no_parity_object_to_selected_tape` always called
`write_no_parity_bootstrap` before writing the object. The resulting report
hard-codes:

- `tape_file_number: 1`
- `first_parity_data_ordinal: 0`
- `total_committed_ordinals = layout.projected_size_blocks`

That is only correct for the first object on a no-parity tape, after the file-0
bootstrap. It is wrong for append and dangerous if the writer branch is not
enforced below the plan layer.

### 2.4 Parity writer always starts a fresh parity sink

`write_parity_object_to_selected_tape` creates a new
`ParitySink::new_sidecar_only`, calls `write_bootstrap`, writes one object, then
drops the sink. The parity layer can model multiple object tape files and
already exposes restart/resume helpers, but Layer 5 does not yet load the
committed prefix, emit rebuilt sidecars, reload the prefix, seed a resumed sink,
or journal resumed appends exactly once.

### 2.5 Catalog shape is necessary but not sufficient

The SQLite schema already has the right cardinality:

- `tape_files` is keyed by `(tape_uuid, tape_file_number)`.
- `object_copies` is keyed by `(object_id, tape_uuid, tape_file_number)`.
- `tapes.last_committed_tape_file` and `tapes.total_committed_ordinals` already
  move forward incrementally.

But a `CommittedBundle` alone is not enough to rebuild the full object catalog.
The live projection writes `objects`, object-file rows, object copies, and tape
file rows together. Append recovery needs a durable record with the same Layer 5
metadata, or a crash after journal fsync and before SQLite projection can leave
a valid committed object invisible to normal object listing.

## 3. Goals and non-goals

### Goals

- Append many independent objects to one tape until capacity, early warning,
  policy, or operator seal stops the tape.
- Keep the object commit boundary simple: one pool write commits one object
  append transaction.
- Preserve the REM-PARITY durable-boundary model: object/control bytes, then
  synchronous filemark, then durable off-tape commit record.
- Keep reads stable: existing object locators remain
  `(tape_uuid, tape_file_number, first_body_lba)`.
- Make the durable append record rebuild full Layer 5 object metadata, not only
  structural tape-file rows.
- Reuse the existing catalog cardinality and parity journal concepts where they
  are correct.
- Let physical field tests reuse one or two scratch data tapes for repeated
  append/read/verify loops.
- Support both no-parity and parity pools, with a staged rollout that unblocks
  no-parity physical testing before the full parity resume surface lands.

### Non-goals

- Spanning one logical object across multiple tapes.
- Multiple writers appending to the same tape concurrently.
- Multi-host shared ownership of a tape. The daemon still owns the drive/tape
  append lock.
- In-place modification or deletion of an object already committed to tape.
- Treating an uncommitted tail as valid data. MTA-1 fences any tape with a
  detected uncommitted tail; future repair work may add verified tail
  supersession.

## 4. Core model

### 4.1 Append plan

Replace the binary fresh/unsupported check with a resolved append plan:

```rust
enum TapeAppendPlan {
    Fresh {
        bootstrap_tape_file: u32,     // 0
        first_object_tape_file: u32,  // 1
    },
    Append {
        next_tape_file: u32,
        append_lba: u64,
        committed_prefix: CommittedState,
    },
}
```

The plan is built from the append journal and SQLite, then verified against the
mounted tape before bytes are written.

For `Fresh`, the writer writes file 0 bootstrap/control file and object file 1.
Both file 0 and file 1 are part of the durable prefix. File 0 is a
`TapeFileKind::Bootstrap` control entry with its real block count and filemark.

For `Append`, the writer must:

1. Verify tape UUID and block size match the selected catalog row.
2. Verify the file-0 bootstrap identity and protection config match the catalog
   and append journal. Mismatch is a hard stop.
3. Verify the per-tape journal replays to the same prefix as SQLite, or repair
   SQLite from the journal before continuing.
4. Compute `next_tape_file = last_committed_tape_file + 1`.
5. Compute `append_lba` from the committed filemark map, including file 0 and
   all object/control files.
6. Locate/space to `append_lba`.
7. Confirm the device reports the expected logical block position before
   writing.

The Fresh/Append branch must be enforced in the writer, not only in selection.
An append writer is forbidden to emit file 0.

### 4.2 Position validation

Before appending, Remanence must prove that the drive is at the append point for
the selected tape. MTA-1 requires at least:

- tape identity check from bootstrap/catalog;
- `READ POSITION` logical block identifier equals expected `append_lba`;
- prefix consistency check from the append journal and SQLite;
- refusal to append if a drive cannot locate to and report the expected
  position.

Where object headers make cheap back-read validation practical, MTA-1 should
also validate the prior object/control row by spacing back one filemark and
reading the expected header. If the device or transport cannot support that
check reliably, the implementation must document the limitation and keep the
operation fail-closed on any position mismatch.

### 4.3 Per-tape append lock

Append is serialized per tape UUID. Opening the append plan acquires an
exclusive per-tape journal lock before loading committed state or moving the
drive. The existing single-writer owner is enough for one daemon process, but
the file-backed journal lock is still required because it is the crash-recovery
commit point and protects against accidental second writers in tools/tests.

### 4.4 Append commit record

The durable append journal record is not just a `CommittedBundle`. It is a full
Layer 5 commit record:

```rust
struct AppendCommitRecord {
    tape_uuid: TapeUuid,
    protection: TapeProtectionConfig,
    block_size: u32,
    append_mode: AppendMode,          // enum: Fresh | Append | ResumeControl | Seal
    committed_bundle: CommittedBundle,
    object: Option<NativeObjectProjectionInput>,
    object_files: Vec<NativeObjectFileProjectionInput>,
    object_copies: Vec<NativeObjectCopyProjectionInput>,
    pool_id: String,
    copy_representation: CopyRepresentation,
    write_timestamp_utc: Timestamp,
    position_before_lba: u64,
    position_after_lba: u64,
    hardware_early_warning: bool,
}
```

Control-only records, such as file-0 bootstrap, checkpoint bootstrap, resume
sidecars, and final seal, use `object = None` and empty object-file/copy lists.
Object records carry enough metadata to rebuild `objects`, object-file rows,
object copies, tape rows, and audit-visible write results.

The same `AppendCommitRecord` drives both journal replay and SQLite projection.
Projection code may derive a `CommittedBundle` view from it, but it must not
invent catalog state that is absent from the durable record.

### 4.5 Commit ordering

The append commit order is:

1. Write object or control bytes.
2. Write one synchronous filemark.
3. Capture and validate the position after the filemark.
4. Build the full `AppendCommitRecord`.
5. Append and fsync the per-tape journal record.
6. Project the same record into SQLite.
7. Return success to the client.

The daemon must not acknowledge the object before step 5. If step 6 fails after
step 5 succeeds, startup replay or an immediate repair pass must reproject the
journaled record before the tape is eligible for another write.

### 4.6 Crash table

| Failure point | Recovery rule |
|---|---|
| Before any new block | No append record exists; next append locates to the prior committed prefix. |
| After object/control blocks, before filemark | No commit; tail is uncommitted. MTA-1 fences the tape for operator repair and keeps the prior prefix authoritative. |
| After filemark, before journal fsync | No commit; the durable-on-media tail is still uncommitted by Remanence. Recovery must not adopt it from tape because the full Layer 5 commit record is missing. MTA-1 fences the tape. |
| After journal fsync, before SQLite projection | Commit is valid; SQLite is repaired from the append record before further writes. |
| After SQLite projection | Commit is valid; normal append continues from the new prefix. |
| Early warning during object/filemark | Commit only if the synchronous filemark and journal fsync complete. Then seal or policy-hold the tape. |
| End of medium before filemark | Do not commit. MTA-1 fences the tape for operator inspection and keeps the prior prefix authoritative. |

MTA-1 chooses the fail-closed behavior: any detected uncommitted tail fences the
tape and removes it from writable selection until explicit operator repair. A
future repair feature may add verified reposition-and-overwrite, but that is not
part of MTA-1.

## 5. No-parity append

No-parity append is the smallest useful production change for the physical
library day because it lets repeated RAO objects occupy one LTO-9 cartridge.

### 5.1 Fresh no-parity write

Fresh behavior becomes:

1. Write file-0 no-parity bootstrap.
2. Write filemark.
3. Journal/project a control `AppendCommitRecord` for tape file 0.
4. Write object file 1.
5. Write filemark.
6. Journal/project the object `AppendCommitRecord` for tape file 1.

This makes the physical prefix dense and recoverable. Append-position math never
has to special-case an unjournaled file-0 bootstrap.

### 5.2 No-parity append write

Append behavior:

1. Do not write another bootstrap.
2. Read/verify the existing bootstrap identity and protection config.
3. Locate to the append point after the last committed tape file.
4. Write the RAO object as the next tape file.
5. Write one filemark.
6. Report the real `tape_file_number = next_tape_file`.
7. Report `total_committed_ordinals = previous_total + object_blocks`.
8. Journal the full append record, then project SQLite.

This requires changing `no_parity_write_report` and
`no_parity_encrypted_write_report` to accept the append context instead of
hard-coding tape file 1 and zero-based totals.

### 5.3 No-parity durable-boundary writer

The MTA-1 no-parity writer must use a durable-boundary write helper that:

- checks every `write_block` outcome for short write, early warning, and end of
  medium;
- rejects any data-block EOM before a filemark as uncommitted;
- checks `write_filemarks(1)` for EOM and position consistency;
- captures `position_before_lba` and `position_after_lba`;
- refuses to build an `AppendCommitRecord` after any poisoned write outcome;
- fences a partial tail before the tape becomes eligible again.

The current helper style that ignores write outcomes is not acceptable for
append.

### 5.4 Protection-neutral journal

The existing `FileTapeFileJournal` is parity-specific because its header
requires a `ParityScheme`. The append design needs a protection-neutral journal
format:

```rust
enum TapeProtectionConfig {
    None,
    Parity(ParityScheme),
}
```

The preferred fold is to keep the `.remjournal` discovery path and add a new
header version that records `TapeProtectionConfig`. Existing v2 parity journals
remain readable as `Parity(scheme)`. New no-parity journals are opened with
`None`.

MTA-1 must specify and test:

- exclusive append lock and shared replay lock behavior;
- trusted-volume checks for the journal path;
- header mismatch errors for tape UUID, block size, drive compression, and
  protection config;
- rebuild discovery for `protection = None`;
- ingestion-pending behavior when SQLite is stale;
- a stub parity-reader/no-parity-journal compatibility test so MTA-2 does not
  require migration of MTA-1 field tapes.

Catalog-less no-parity scan is not required for MTA-1. Journal/catalog recovery
is enough for the production unblocker, provided the append record contains full
Layer 5 metadata.

## 6. Parity append

Parity append must implement the restart obligations in
`specs/rem-parity-1.0-specification.md`: preserve prior object rows, append new
rows, rebuild the live partial epoch, and write control files at defined
barriers.

### 6.1 Fresh parity write

Fresh write:

1. Create/open per-tape protection-neutral journal with
   `TapeProtectionConfig::Parity(scheme)`.
2. Create a parity sink with journal ownership or an explicit Layer 5 journal
   callback.
3. Write file-0 bootstrap.
4. Journal/project the bootstrap control record.
5. Write object file 1.
6. Journal/project the object append record.

The current code gets close for the first object, but the journal must become
the primary durable commit point for the pool writer and must carry full Layer 5
metadata.

### 6.2 Append parity write

Parity append order is exact:

1. Open the per-tape journal with exclusive append lock.
2. Load `CommittedState`.
3. Validate `CommittedState::validate_v1_restart_bound`.
4. Build the filemark map from the committed prefix.
5. Position the drive to the append point.
6. Call the resume rebuild path to reread `[W,T)` and produce rebuilt sidecars
   for the live partial epoch.
7. Emit rebuilt sidecars with full filemark and journal commit cycles.
8. Reload or extend the committed prefix to include those resume sidecars.
9. Build `ResumeWriterSeed` from the updated prefix.
10. Construct the resumed parity sink.
11. Write the next object with bootstrap-object-row admission before bytes.
12. Finish the object, journal exactly one object append record, project SQLite.

The lower layer already contains the key pieces:

- `CommittedBundle` and `CommittedState`;
- file-backed journal replay with torn-tail truncation;
- restart-bound validation;
- `rebuild_open_epoch_from_committed_prefix`;
- `emit_resume_rebuilt_sidecars_to_raw`;
- `ParitySink::new_sidecar_only_from_resume`;
- parity sink support for multiple objects in one sink.

The Layer 5/API work is wiring those pieces in this order and making journal
ownership unambiguous.

### 6.3 Parity journal ownership

Every parity object/control bundle is journaled exactly once before SQLite
projection. MTA-2 must choose one ownership model:

- add a resume-with-journal constructor so the resumed `ParitySink` owns journal
  commits; or
- keep Layer 5 as the journal owner and require it to call
  `ObjectWriteSummary::committed_bundle()` or equivalent exactly once per
  committed bundle.

The design forbids a mixed model where fresh writes journal inside the sink and
resumed writes journal outside the sink without an explicit adapter. Tests must
inject journal failure after filemark and prove the session is poisoned and the
tape is not returned to the writable pool until repaired.

### 6.4 Resume seed completeness

MTA-2 must provide complete resume seed metadata:

- one `SidecarEpochDirectoryEntry` for every committed-prefix sidecar, including
  sidecar header block count and parity shard block count;
- one bootstrap object row for every object tape file in the committed prefix;
- complete carried-forward object rows for checkpoint/final bootstraps.

Current structural tape-file rows do not carry all sidecar-directory fields.
MTA-2 must either extend the durable append record/state projection with those
fields or define a sidecar-header scan that rebuilds them before resume. A
prefix that cannot supply complete rows is not appendable.

### 6.5 Checkpoint and close policy

Parity append control-file policy is:

- appendable parity tapes checkpoint before unload, long idle, or early-warning
  policy hold;
- sealed parity tapes call `finish()` and become non-appendable;
- final bootstrap is never emitted after an object if automatic append may
  continue;
- bootstrap object-row admission happens before object bytes are written;
- if the row set no longer fits v1 key 30, the tape is checkpointed/sealed and
  a fresh tape is selected before writing object bytes.

### 6.6 Resume idempotency

Resume is itself a crash window. MTA-2 must document and test each step:

- whether re-entry is idempotent;
- which journal record marks completion;
- which tape-side evidence validates completion;
- what happens if the crash occurs after sidecar filemark and before sidecar
  journal commit.

No parity append prompt is cut until this idempotency table exists.

## 7. Selection, capacity, and projection

### 7.1 Writability states

Replace "committed ordinals means unsupported" with these hard checks:

- tape row state is `ready`;
- tape kind is data;
- tape belongs to the requested pool;
- block size matches pool config;
- protection config matches pool config;
- append journal and SQLite committed prefix agree or SQLite can be repaired
  from the journal;
- no active append lock exists for the tape;
- remaining physical capacity can admit the object plus required filemarks and
  control files;
- the tape is not fenced for uncommitted-tail/EOM inspection.

Used tapes become eligible when those checks pass.

### 7.2 Ranking policy

For `CompleteOrFill` pools, rank appendable partially used tapes ahead of fresh
tapes when the object fits. That lets a small-object workload fill a cartridge
instead of burning one tape per object.

Fresh tapes remain eligible when:

- no appendable tape has enough remaining capacity;
- the appendable tape is sealed or in early-warning policy hold;
- the requested object's parity object rows/control overhead would not fit on
  the existing tape.

### 7.3 Capacity accounting

Current capacity checks use `total_committed_ordinals * block_size`, which is
only an object-data approximation. Append needs a physical committed-footprint
model:

```text
used_blocks = sum(tape_files.block_count)
used_filemarks = count(tape_files)
used_lba = latest committed position_after_lba
```

Admission then reserves:

- object data blocks;
- one object filemark;
- parity sidecars that may be emitted by this object;
- checkpoint/final bootstrap if policy requires one before the tape can be
  safely unloaded;
- implementation margin for device-reported early warning and vendor capacity
  granularity. MTA-1 uses a conservative 5% raw-capacity reserve until physical
  MSL3040 early-warning behavior is characterized.

MTA-1 acceptance requires seal/ranking logic to use physical post-filemark
position or full filemark-map footprint, not `total_committed_ordinals` alone.
For no-parity reports, `total_committed_ordinals` is an informational protection
counter and not a capacity source.

### 7.4 Projection fail-closed rules

SQLite projection of an append record must fail closed when:

- existing SQLite prefix differs from the append journal prefix;
- new entries are not contiguous with `last_committed_tape_file`;
- a `tape_files` row already exists for any new tape-file number with different
  content;
- object/copy IDs conflict with a different committed record;
- watermarks regress or skip unexpectedly.

The current max-style upsert behavior is too permissive for append. Append
projection is a prefix-extension operation, not a merge.

## 8. Catalog, read, rebuild, and API

### 8.1 Rebuild from journals

On daemon startup or catalog repair:

1. Replay every per-tape append journal.
2. Recreate `tape_files`, `objects`, object-file rows, `object_copies`,
   watermarks, pool linkage, and `last_committed_tape_file`.
3. Preserve authoritative non-rebuildable tables.
4. Refuse further appends for any tape whose journal and physical tape identity
   disagree.

This is especially important when filemark and journal commit succeeded but
SQLite projection did not.

### 8.2 Read path

No fundamental read API change is needed. A committed object copy already points
to:

- `tape_uuid`
- `tape_file_number`
- `first_body_lba`

Tests must prove reads and ranged reads work for object copies at tape files 1,
2, 3, and after parity control files. This catches hidden assumptions that "the
object is tape file 1."

### 8.3 Operator-visible append evidence

MTA-1 adds append evidence to daemon and CLI results. The gRPC wire change is
backward-compatible: add an optional `AppendCommitInfo append_commit_info` field
to `ObjectRecord`. Existing clients that ignore the field still receive the
same object/copy data, while daemon and CLI field-test clients can assert append
behavior without querying SQLite directly.

```proto
enum AppendMode {
  APPEND_MODE_UNSPECIFIED = 0;
  APPEND_MODE_FRESH = 1;
  APPEND_MODE_APPEND = 2;
  APPEND_MODE_RESUME_CONTROL = 3;
  APPEND_MODE_SEAL = 4;
}

message AppendCommitInfo {
  AppendMode append_mode = 1;
  bytes tape_uuid = 2;
  optional string voltag = 3;
  uint64 tape_file_number = 4;
  uint64 first_body_lba = 5;
  optional uint64 position_before_lba = 6;
  optional uint64 position_after_lba = 7;
  optional uint64 journal_record_ordinal = 8;
  optional uint64 estimated_remaining_bytes = 9;
  optional bool sealed_after_write = 10;
}
```

CLI JSON and `fieldtest/tools/remfield-io` must surface the same fields so the
physical test evidence can assert same-tape append without scraping SQLite.

### 8.4 Session rollover

MTA-1 does not implement automatic rollover inside an already-open daemon write
session. A session remains mounted to one selected tape. If an append seals or
fences that tape, the next `AppendObject` in the same session returns a
resource-exhausted/tape-sealed error telling the client to close and open a new
session.

Single-object pool write commands may open a fresh session per object and
therefore may select the same appendable tape repeatedly until it seals.

### 8.5 Idempotency

MTA-1 needs minimum retry safety. If a client times out after journal fsync but
before receiving success, a retry must not silently append a duplicate object.

MTA-1 uses non-empty `caller_object_id` as the scoped idempotency key. No new
proto field is introduced in MTA-1. Minimum acceptable behavior:

- scope the key to `(pool_id, caller_object_id)`;
- if the same key replays with the same content hash, return the committed
  object/copy result;
- if the same key replays with different content, fail with conflict;
- if `caller_object_id` is empty, the write is accepted but explicitly
  non-idempotent and duplicate retries are possible.

Field tests must use non-empty `caller_object_id` values so crash/retry evidence
is unambiguous.

## 9. Test and scenario coverage

### 9.1 Unit/API tests

Replace current "skip written tape" expectations with append expectations:

- selector includes a used no-parity tape when remaining capacity fits;
- selector rejects used tapes with mismatched protection config, corrupt journal
  prefix, or fenced uncommitted tail;
- fresh no-parity write journals/projects file-0 bootstrap and object file 1;
- no-parity append writes no file-0 bytes and commits objects at tape files 2
  and 3;
- encrypted no-parity append reports the real tape file number;
- no-parity short write, data EOM, and filemark EOM fail before journaling;
- journal commit before SQLite projection is enforced by a failure-injection
  test;
- restart repair reprojects full object metadata when SQLite missed it;
- append projection rejects non-contiguous/conflicting tape-file rows;
- read-core can read object copies from tape files greater than 1;
- `caller_object_id` replay returns the committed result for matching content.

### 9.2 Parity tests

- parity append writes at least three objects to one tape in one process;
- parity append after process restart follows the exact resume sequence and
  writes the next object;
- crash at each resume step is idempotent or fences the tape;
- checkpoint-before-unload writes a control bundle and preserves object rows;
- final seal refuses further append;
- object-row admission failure seals/selects fresh tape before writing object
  bytes;
- sidecar-directory metadata is complete enough to seed resumed writers.

### 9.3 System scenario

Add a clean-slate system scenario:

- `scenarios/scenario_append.py`;
- `[scenario.append]` in `scenarios/contracts.toml`;
- `needs_daemon = true`;
- one no-parity pool, tentatively `append-a`;
- two LTO-9 test tapes for clean reset/recycle coverage;
- `covers = ["rem.tape.write_object_via_daemon", "rem.tape.read_object_via_daemon", "rem.tape.append_same_tape"]`.

Add `rem.tape.append_same_tape = "Real(grpc)"` to `bindings.toml`; the seam
implementation writes multiple objects through the daemon and asserts that the
returned `ObjectRecord.append_commit_info` values prove same-tape dense append.
The scenario:

1. `make reset && make up`.
2. Initialize the append pool with two writable tapes.
3. Write three small RAO objects with non-empty `caller_object_id` values.
4. Assert all three object copies use the same tape UUID and tape files 1, 2,
   and 3.
5. Restart the daemon.
6. Write a fourth object and assert tape file 4.
7. Read/verify all four objects.
8. Seal the tape and assert the next write chooses a fresh tape or returns the
   defined no-rollover session error.

This scenario carries coverage for the new append capability.

### 9.4 Physical MSL3040 field test

MTA-1 is not complete until the field harness proves same-tape append on the
physical kit. Required changes:

- implemented: make fieldtest media guards append-aware; a used appendable tape
  must not be rejected solely because it has committed objects;
- implemented: add `13-append-loop.sh`: write N independent RAO objects to one
  pool, assert one `tape_uuid`, dense `tape_file_number` values, read all
  objects, and record SHA results;
- add append-specific rebuild evidence after 3+ objects on one tape;
- add kill-during-append evidence on an already-used tape, then restart and
  verify committed objects are not lost or renumbered; current field harness
  reads back a committed object from the killed-write pool after restart, but
  still needs the explicit uncommitted-tail/fenced-tape assertion;
- implemented: update runbooks around "2 scratch data tapes plus CLN" as the
  normal post-append field path;
- split benchmarks into "streaming" mode with fewer large objects and
  "append-stress" mode with many smaller objects, reporting first-write and
  append-write rates separately.

A same-day MSL3040 run should not define success as filling 18 TB. Success is
many append transactions plus a documented GiB/TiB moved and verified. Rollover
is tested by forced seal/low-watermark configuration, not by waiting for true
EOM.

## 10. Implementation prompts

### MTA-1 -- no-parity append production unblocker

- Add protection-neutral append journal with full `AppendCommitRecord`.
- Journal/project file-0 bootstrap as a control tape file on fresh no-parity
  tapes.
- Add `TapeAppendPlan` for fresh vs append and enforce it inside writers.
- Remove committed-ordinal append rejection for no-parity tapes that pass
  prefix/capacity checks.
- Position to append point and write next no-parity object without a new
  bootstrap.
- Add no-parity durable-boundary writer for short-write/EOM/filemark handling.
- Journal before SQLite projection and rebuild full object metadata from
  journal.
- Add fail-closed append projection rules.
- Add `(pool_id, caller_object_id)` replay behavior for non-empty
  `caller_object_id`. Implemented in the pool write core: same pool/caller and
  same content returns the committed object/copy without tape I/O; same
  pool/caller with different content returns conflict; empty caller ids remain
  accepted but non-idempotent.
- Add `ObjectRecord.append_commit_info` to daemon/CLI/field JSON.
- Add unit tests, `scenario-append`, and fieldtest `13-append-loop.sh` plus
  append-specific rebuild/kill evidence. The fieldtest append loop is present;
  `scenario-append` and append-specific rebuild/kill evidence remain pending.

### MTA-2 -- parity append/resume

- Wire pool writes to protection-neutral parity journal replay.
- Define exactly-once parity journal ownership for fresh and resumed writes.
- Implement exact resume sequence: rebuild live epoch, emit resume sidecars,
  reload prefix, seed resumed sink, append object.
- Extend append records/state or scan sidecar headers to provide complete
  sidecar-directory metadata.
- Preserve complete object rows in checkpoint/final bootstraps.
- Add checkpoint-before-unload and final-seal behavior.
- Add parity restart, resume-crash, sidecar seed, object-row, and checkpoint
  tests.

### MTA-3 -- operator polish

- Add explicit seal/unseal operator flow if needed.
- Add journal compaction/high-water policy after checkpoint/final evidence is
  strong enough to compact safely.
- Add richer capacity telemetry after physical MSL3040 early-warning behavior is
  characterized.

## 11. Resolved panel decisions

1. **No-parity catalog-less recovery:** not required for MTA-1. Full append
   commit records plus trusted local journal/catalog recovery are sufficient.
2. **No-parity file 0:** must be journaled/projected as a control file. No
   special unjournaled bootstrap prefix.
3. **Journal payload:** must be full Layer 5 `AppendCommitRecord`, not only
   `CommittedBundle`.
4. **Idempotency:** `(pool_id, caller_object_id)` replay behavior is in MTA-1
   for non-empty caller IDs; empty caller IDs remain accepted but explicitly
   non-idempotent.
5. **Fieldtest updates:** move to MTA-1 acceptance criteria.
6. **Parity checkpoint/final:** appendable tapes checkpoint; sealed tapes
   `finish()` and become non-appendable.

## 12. Remaining open questions

1. What exact SCSI/transport primitive should a future repair feature use to
   supersede an uncommitted tail after crash/EOM, and which devices in the
   MSL3040 support it reliably?
2. What journal high-water/compaction rule should MTA-3 use after checkpoint or
   final bootstrap evidence exists?
