# Layer 3b Catalog Schema — Per-File Rows Follow-up

**Status:** Follow-up note. Records a schema extension needed
for `rem-tar-v1` (default body format, see
`rem-tar-v1-design.md` §10.1, §16.7, and §16.10;
`docs/remanence-testing-plan.md` for catalog transaction and
crash-window tests).

## Context

The schema is part of the write-safety contract, not just an
indexing convenience. The cross-layer test plan requires catalog
transaction tests for object commit, sidecar commit, resume-generated
sidecar commit, parity-state watermark updates, and crash-window
reconciliation.

The original 3b design recorded one catalog row per **object** —
the orchestrator's archival unit, which typically contains many
files. To locate a specific file within an object, a reader had
to first read the on-tape manifest.

`rem-tar-v1` v0.9.3 assumes per-file rows are also present in the
catalog. This enables:

- Direct LOCATE to any file's first chunk in one catalog lookup,
  with no manifest read in the common case.
- Multi-file restore batching: the orchestrator can sort N file
  requests by `(tape_id, first_chunk_lba)` and dispatch them as
  forward seeks, minimizing total backward LOCATE distance.
- Byte-range arithmetic becomes pure arithmetic from the
  catalog row (no manifest needed for the LBA computation).

## Schema change

Add a new table (or columns to an existing per-file table)
parallel to the existing per-object table. Also add a
`manifest_sha256` column to the existing per-object table
(needed for rem-tar-v1's manifest trust chain):

```sql
-- Existing object table gains manifest + tape-file columns
ALTER TABLE catalog_objects
    ADD COLUMN tape_file_number         INTEGER NOT NULL,
    ADD COLUMN manifest_sha256          BYTEA NOT NULL,
    ADD COLUMN manifest_first_chunk_lba BIGINT NOT NULL,
    ADD COLUMN manifest_size_bytes      BIGINT NOT NULL,   -- review #4: direct manifest read
    ADD COLUMN manifest_chunk_count     INTEGER NOT NULL,  -- review #4: direct manifest read
    ADD COLUMN metadata_preservation    TEXT NOT NULL DEFAULT 'archival',
    -- Parity-protection state (3c v0.4.4 §7.2.1, §10.1). Because
    -- filemarks do not flush parity epochs, an object can be on
    -- tape and tar-readable while still in an open, sidecar-less
    -- epoch. These columns let the catalog represent that honestly
    -- rather than assuming "on tape" == "parity-protected".
    ADD COLUMN first_parity_data_ordinal BIGINT NOT NULL,   -- object's first protected ordinal
    -- data_block_count is the object-row copy of the object's
    -- catalog_tape_files.block_count (they are equal by the §5.6
    -- identity: every block in an object tape file is protected
    -- data). It is written in the SAME transaction as the
    -- tape-file row and is not an independent quantity; the FK
    -- below keeps the two rows from drifting.
    ADD COLUMN data_block_count          BIGINT NOT NULL,
    -- Generated upper bound of the object's ordinal range, so
    -- "is this object protected?" and "which object holds this
    -- ordinal?" are simple range comparisons against the tape
    -- watermark (review: ordinal_end_exclusive).
    ADD COLUMN ordinal_end_exclusive     BIGINT
        GENERATED ALWAYS AS (first_parity_data_ordinal + data_block_count) STORED,
    -- Three states (review #1): a large object commonly sits in
    -- 'partial' — early epochs protected, tail still open. The
    -- per-block recovery test (3c §7.2.1) uses the ordinal vs the
    -- tape watermark; parity_state is the operator-facing summary.
    ADD COLUMN parity_state              TEXT NOT NULL DEFAULT 'pending'
        CHECK (parity_state IN ('pending', 'partial', 'protected'));

-- Referential integrity: an object row's (tape_id, tape_file_number)
-- must name a real tape file, and the trigger/transaction invariant
-- requires that tape file to have kind='object'. (Postgres FKs
-- can't express the kind predicate directly; enforce kind via a
-- deferrable constraint trigger or the same-transaction insert.)
--
-- DISPLAY-ORDER NOTE: this ALTER references catalog_tape_files,
-- which is defined later in this document. In the ACTUAL
-- migration it runs AFTER catalog_tape_files is created — see
-- "Implementation order" step 1 (1a creates the table; 1c adds
-- this FK; 1d adds the reverse FK). Do not copy these blocks in
-- document order.
ALTER TABLE catalog_objects
    ADD CONSTRAINT catalog_objects_tape_file_fk
        FOREIGN KEY (tape_id, tape_file_number)
        REFERENCES catalog_tape_files(tape_id, tape_file_number)
        DEFERRABLE INITIALLY DEFERRED;
-- NOTE: catalog_tape_files.object_id also references
-- catalog_objects(object_id), so the two tables form an FK cycle.
-- Both rows are inserted in ONE transaction (the object-commit
-- transaction, 3c §7.2.1 ObjectCatalogCommitted) and at least one
-- side must be DEFERRABLE INITIALLY DEFERRED so the cycle resolves
-- at COMMIT. The object row and its 'object'-kind tape-file row are
-- thus created atomically and cannot drift apart.
--
-- data_block_count integrity (review): catalog_objects.data_block_count
-- MUST equal the referenced object tape file's catalog_tape_files.
-- block_count (§5.6 identity). Postgres can't express this as a
-- plain FK; enforce it with a DEFERRABLE CONSTRAINT TRIGGER that
-- fires on the object-commit transaction, or assert it in the
-- Layer 5 insert path.

-- New per-file table
CREATE TABLE catalog_files (
    file_id          UUID NOT NULL,
    object_id        UUID NOT NULL REFERENCES catalog_objects(object_id),
    tape_id          UUID NOT NULL,
    path             TEXT NOT NULL,
    size_bytes       BIGINT NOT NULL,
    tape_file_number INTEGER NOT NULL,  -- which filemark-delimited tape file holds the object
    first_chunk_lba  BIGINT,            -- per-object BodyLba; NULL for empty files (review #9)
    chunk_count      INTEGER NOT NULL,  -- 0 for empty files
    chunk_size       INTEGER NOT NULL,
    file_sha256      BYTEA NOT NULL,
    mtime_ns         BIGINT,            -- NULL in minimal preservation mode
    compression      TEXT NOT NULL DEFAULT 'none',  -- reserved; always 'none' in v1 (§6.2)
    encryption_kek_ref BYTEA,
    executable       BOOLEAN,           -- the +x bit; NULL in minimal mode
    -- The following are populated ONLY when the object was written
    -- with MetadataPreservation::Full (see rem-tar-v1-design §9.4):
    mode             INTEGER,           -- full Unix mode bits; NULL otherwise
    uid              INTEGER,           -- NULL unless Full
    gid              INTEGER,           -- NULL unless Full
    uname            TEXT,              -- NULL unless Full
    gname            TEXT,              -- NULL unless Full
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- review #9: file_id is NOT globally unique — the same logical file
    -- exists on multiple tape copies and across re-archives. The natural
    -- key is (object_id, file_id); an object lives on exactly one tape,
    -- so this is unique per physical instance.
    PRIMARY KEY (object_id, file_id)
);

-- review #9: enforce the empty-file invariant — a file has a first_chunk_lba
-- iff it has at least one chunk.
ALTER TABLE catalog_files
    ADD CONSTRAINT catalog_files_empty_chk
    CHECK ((chunk_count = 0) = (first_chunk_lba IS NULL));

-- Basic numeric sanity (review): reject negative/zero where meaningless.
ALTER TABLE catalog_files
    ADD CONSTRAINT catalog_files_size_chk        CHECK (size_bytes >= 0),
    ADD CONSTRAINT catalog_files_chunkcount_chk  CHECK (chunk_count >= 0),
    ADD CONSTRAINT catalog_files_chunksize_chk   CHECK (chunk_size > 0),
    ADD CONSTRAINT catalog_files_lba_chk         CHECK (first_chunk_lba IS NULL OR first_chunk_lba >= 0);

-- catalog_objects numeric sanity (review):
ALTER TABLE catalog_objects
    ADD CONSTRAINT catalog_objects_manifest_size_chk  CHECK (manifest_size_bytes >= 0),
    ADD CONSTRAINT catalog_objects_manifest_count_chk CHECK (manifest_chunk_count >= 0),
    ADD CONSTRAINT catalog_objects_dbc_chk            CHECK (data_block_count > 0);

-- v1 stores no format-level compression (rem-tar §6.2); the reader
-- already rejects non-'none', but a DB constraint blocks bad rows
-- at the source (review).
ALTER TABLE catalog_files
    ADD CONSTRAINT catalog_files_compression_chk CHECK (compression = 'none');

-- catalog_files.tape_id / tape_file_number DUPLICATE the object row's
-- (review): they are denormalized for index locality. They MUST equal
-- the parent object's values; enforce with a deferrable constraint
-- trigger (or assert in the Layer 5 insert path) so a file row cannot
-- drift from its object's tape-file location. Do not let them diverge.

CREATE INDEX catalog_files_object_idx ON catalog_files(object_id);
CREATE INDEX catalog_files_tape_lba_idx ON catalog_files(tape_id, tape_file_number, first_chunk_lba);
CREATE INDEX catalog_files_path_idx ON catalog_files(object_id, path);
CREATE INDEX catalog_files_sha256_idx ON catalog_files(file_sha256);
```

The ownership/permission columns (`mode`, `uid`, `gid`,
`uname`, `gname`) are nullable and populated only for objects
written with `MetadataPreservation::Full`. Under the default
`Archival` mode they are NULL; `executable` and `mtime_ns`
carry the meaningful metadata. Under `Minimal` mode even those
are NULL. The per-object `metadata_preservation` value is
recorded on the `catalog_objects` table so queries can tell
which mode produced a given row.

The indexes support the three common lookup patterns:
- `(tape_id, tape_file_number, first_chunk_lba)` — multi-file restore batching
- `(object_id, path)` — find file by path within an object
- `file_sha256` — deduplication / catalog reconciliation

The `manifest_sha256` field on the objects table is the
cryptographic anchor for the manifest's per-chunk CRC codes
(see rem-tar-v1 §8.3 trust chain). Without it, an attacker
who modifies the manifest can also modify the per-chunk CRCs
inside, and per-chunk integrity verification becomes useless.
With it, manifest tampering is detected before any of its
contents are trusted.

## Population

The rem-tar-v1 writer surfaces per-file LBA data to Layer 5 at
object close (see rem-tar-v1-design §9 step 9). Layer 5 inserts
both the per-object row and the per-file rows atomically:

```rust
pub struct ObjectWriteResult {
    pub object_entry: ObjectEntry,      // existing
    pub file_entries: Vec<FileEntry>,   // new
}
```

The transaction order is: BEGIN, insert object, insert files
(batch), COMMIT. A failure between object and file inserts is
recoverable via catalog reconciliation (read the on-tape
manifest, regenerate file rows).

## Migration

Existing tapes written before this schema change have no
per-file rows. For those tapes, reads fall back to the
manifest-read path (rem-tar-v1 §10.5). Operators can run a
background migration that reads each old object's manifest and
populates `catalog_files` rows.

The reader code handles both cases transparently: if the
per-file row exists, use it; if not, fall back to the
manifest. No on-tape data changes.

## Storage impact

For archive's archive: ~10M files over a decade gives ~10M rows.
At ~500 bytes per row (including indexes), ~5 GB total catalog
storage. PostgreSQL handles this trivially.

## Symlink and external-reference tables (v0.5)

The v0.5 spec of rem-tar-v1 (see `rem-tar-v1-design.md` §5.8)
introduces symlink entries and external references. These get
their own catalog tables so they're queryable independently
of file entries:

```sql
-- Symlinks within an object (parallel to catalog_files)
CREATE TABLE catalog_symlinks (
    symlink_id       UUID NOT NULL,
    object_id        UUID NOT NULL REFERENCES catalog_objects(object_id),
    tape_id          UUID NOT NULL,
    path             TEXT NOT NULL,           -- path within object
    target_string    TEXT NOT NULL,           -- verbatim from source
    classification   SMALLINT NOT NULL,       -- 1=Internal, 2=ExtAbs, 3=ExtRel, 4=InternallyBroken
    mtime_ns         BIGINT,                  -- NULL in minimal mode (matches SymlinkEntry.mtime ?uint)
    mode             INTEGER,
    uid              INTEGER,
    gid              INTEGER,
    uname            TEXT,
    gname            TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- review #17: symlink_id is not globally unique (same logical symlink
    -- exists on multiple tape copies / re-archives). An object lives on one
    -- tape, so (object_id, symlink_id) is the natural per-instance key.
    PRIMARY KEY (object_id, symlink_id)
);

CREATE INDEX catalog_symlinks_object_idx ON catalog_symlinks(object_id);
CREATE INDEX catalog_symlinks_classification_idx ON catalog_symlinks(classification);
CREATE INDEX catalog_symlinks_target_idx ON catalog_symlinks(target_string);

-- External references (a query-friendly view of External-classified symlinks)
CREATE TABLE catalog_external_refs (
    symlink_id              UUID NOT NULL,
    object_id               UUID NOT NULL REFERENCES catalog_objects(object_id),
    tape_id                 UUID NOT NULL,
    archive_relative_path   TEXT NOT NULL,    -- symlink path within object
    target_string           TEXT NOT NULL,    -- verbatim
    resolved_target         TEXT NOT NULL,    -- absolute form (computed)
    classification          SMALLINT NOT NULL, -- 2=ExtAbs or 3=ExtRel
    -- review #17: same composite key, and FK references the composite
    -- key of catalog_symlinks.
    PRIMARY KEY (object_id, symlink_id),
    FOREIGN KEY (object_id, symlink_id)
        REFERENCES catalog_symlinks(object_id, symlink_id)
);

CREATE INDEX catalog_external_refs_resolved_idx ON catalog_external_refs(resolved_target);
CREATE INDEX catalog_external_refs_object_idx ON catalog_external_refs(object_id);
```

These tables enable several useful operator queries:

```sql
-- "Which archives reference paths under /raid/shared_audio/show01/?"
SELECT DISTINCT object_id FROM catalog_external_refs
WHERE resolved_target LIKE '/raid/shared_audio/show01/%';

-- "What are all external dependencies of object X?"
SELECT archive_relative_path, target_string, classification
FROM catalog_external_refs
WHERE object_id = $1
ORDER BY target_string;

-- "Are there any objects with internally-broken symlinks?"
-- (only possible if Permissive policy was used)
SELECT object_id, path, target_string
FROM catalog_symlinks
WHERE classification = 4;
```

The `resolved_target` field for relative external symlinks is
computed by the writer at archive time (target resolved
relative to the symlink's parent directory, then made
absolute). This makes cross-archive queries possible without
re-resolving paths at query time.

### Symlinks in ObjectWriteResult

The `ObjectWriteResult` struct gains a `symlink_entries` field
(and, alongside the symlinks added here, an
`external_references` field). To avoid two competing definitions
drifting apart, the **authoritative, complete struct** — with
the later hardlink and tape-file additions — is given once below
in "ObjectWriteResult gains the new collections." Symlinks and
external references are two of its collections; Layer 5 inserts
all collections atomically at object close.

## Tape-file map table (v0.7 epoch model — review #7)

The filemark-aware parity-epoch model (rem-tar-v1 §5.1, §16.14;
`layer3c-design.md` §5.1) makes the physical tape a sequence
of filemark-delimited tape files: object archives interspersed
with parity-epoch sidecar tape files and bootstrap tape files.
3c needs a durable **filemark map** to translate
`ParityDataOrdinal ↔ (tape_file_number, block) ↔ stripe
position` for recovery, and readers need it to seek to a given
object's tape file.

This map is **not** fully derivable from object/file rows alone:
sidecar and bootstrap tape files have no file rows, yet they
occupy tape-file numbers and (for sidecars) define the
ordinal→parity mapping. So the catalog needs a tape-file table:

```sql
CREATE TABLE catalog_tape_files (
    tape_id                   UUID NOT NULL,
    tape_file_number          INTEGER NOT NULL,
    kind                      TEXT NOT NULL,   -- 'object' | 'parity_sidecar' | 'bootstrap'
    object_id                 UUID,            -- set when kind='object'
    epoch_id                  BIGINT,          -- set when kind='parity_sidecar'
    -- block_count = every fixed chunk_size block in this tape file.
    --   object:         headers + file data + manifest + tar EOF + post-EOF
    --                   zero-filled final block (rem-tar-v1 §5.4, §9.1.1)
    --   parity_sidecar: header/index block(s) + raw parity shard blocks
    --                   (layer3c-design §5.5)
    --   bootstrap:      the bootstrap block(s)
    block_count               BIGINT NOT NULL,
    -- For kind='object': the ParityDataOrdinal of this object's first data block.
    first_parity_data_ordinal BIGINT,
    -- For kind='parity_sidecar': the HALF-OPEN ordinal range this epoch
    -- protects, [start, end_exclusive) — matches the sidecar header and
    -- Rust slicing; avoids off-by-one (review #15).
    protected_ordinal_start         BIGINT,
    protected_ordinal_end_exclusive BIGINT,
    physical_start_hint       BIGINT,          -- nullable: SCSI block-addr hint for fast LOCATE;
                                               -- 3c can always navigate by filemark number alone
                                               -- (space_filemarks), so the hint is an optimization,
                                               -- not a correctness dependency. NULL = "no hint;
                                               -- seek by filemark." (review: deliberately nullable.)
    sha256                    BYTEA,           -- integrity anchor for the tape file
    PRIMARY KEY (tape_id, tape_file_number),

    -- review #11: every tape file is at least one block; a bootstrap
    -- is exactly one block (§5.6). (The sidecar block_count identity
    -- block_count == shard_index_block_count + S*m is enforced at the
    -- app layer, since S/m are scheme params not stored per row.)
    CONSTRAINT catalog_tape_files_blockcount_chk CHECK (block_count > 0),
    CONSTRAINT catalog_tape_files_bootstrap_one_block_chk
        CHECK (kind <> 'bootstrap' OR block_count = 1),

    -- review #10: the object_id <-> object FK that the prose
    -- promised but the DDL omitted. Deferrable because catalog_objects
    -- also references catalog_tape_files (the cycle resolved in §...).
    CONSTRAINT catalog_tape_files_object_fk
        FOREIGN KEY (object_id) REFERENCES catalog_objects(object_id)
        DEFERRABLE INITIALLY DEFERRED,

    -- review #16: enforce kind-specific field invariants
    CONSTRAINT catalog_tape_files_kind_chk CHECK (kind IN ('object','parity_sidecar','bootstrap')),
    CONSTRAINT catalog_tape_files_shape_chk CHECK (
        (kind = 'object'
            AND object_id IS NOT NULL
            AND epoch_id IS NULL
            AND first_parity_data_ordinal IS NOT NULL
            AND protected_ordinal_start IS NULL
            AND protected_ordinal_end_exclusive IS NULL)
        OR
        (kind = 'parity_sidecar'
            AND object_id IS NULL
            AND epoch_id IS NOT NULL
            AND first_parity_data_ordinal IS NULL
            AND protected_ordinal_start IS NOT NULL
            AND protected_ordinal_end_exclusive IS NOT NULL
            -- STRICT: a sidecar always protects >=1 real ordinal.
            -- finish() skips emission when D==0 (3c §7.2), so a
            -- zero-length sidecar must never exist (review).
            AND protected_ordinal_end_exclusive > protected_ordinal_start)
        OR
        (kind = 'bootstrap'
            AND object_id IS NULL
            AND epoch_id IS NULL
            AND first_parity_data_ordinal IS NULL
            AND protected_ordinal_start IS NULL
            AND protected_ordinal_end_exclusive IS NULL)
    )
);

CREATE INDEX catalog_tape_files_object_idx ON catalog_tape_files(object_id);
CREATE INDEX catalog_tape_files_ordinal_idx
    ON catalog_tape_files(tape_id, first_parity_data_ordinal);
CREATE INDEX catalog_tape_files_epoch_idx
    ON catalog_tape_files(tape_id, epoch_id);
-- review: at most ONE parity sidecar tape file per (tape, epoch).
-- A duplicate sidecar row for an epoch is a write-session bug;
-- enforce it in the schema rather than trusting the writer.
CREATE UNIQUE INDEX catalog_tape_files_sidecar_epoch_uniq
    ON catalog_tape_files(tape_id, epoch_id)
    WHERE kind = 'parity_sidecar';

-- Per-tape parity-protection watermark (3c v0.4.4 §7.2.1, §10.1).
-- Advanced by each emitted parity-epoch sidecar to that epoch's
-- protected_ordinal_end_exclusive. An object is parity-protected
-- iff first_parity_data_ordinal + data_block_count <= this value.
-- On a cleanly closed tape it reaches end-of-data; a crashed
-- write session leaves it short, marking the trailing objects
-- parity-pending (catalog_objects.parity_state).
ALTER TABLE catalog_tapes
    ADD COLUMN highest_protected_ordinal BIGINT NOT NULL DEFAULT 0;
```

The map is authoritative in the catalog. A compact **digest**
of it is also written into each (replicated) bootstrap tape
file; a catalog-less reader reconstructs the map by scanning
tape files (each identifiable by magic) and validates the
reconstruction against the digest — the digest validates a map
but does not provide one (see `layer3c-design.md` §5.6, §8.1,
Option B; the digest is taken over a canonical, non-circular
map projection). The "just derivable" framing of the v0.1 epoch
draft is corrected: object-tape-file ordinals are derivable
from per-object rows, but sidecars and bootstraps must be
catalogued explicitly, which `catalog_tape_files` does.

When Layer 5 emits a sidecar and advances
`highest_protected_ordinal` to `$W`, it recomputes the state of
every not-yet-fully-protected object on the tape in the same
transaction. With the three states (3c §7.2.1), an object is
`protected` when its whole range is covered, `partial` when the
new watermark covers only part of it, and otherwise stays
`pending`:

```sql
UPDATE catalog_objects
   SET parity_state = CASE
         WHEN ordinal_end_exclusive      <= $W THEN 'protected'
         WHEN first_parity_data_ordinal  <  $W THEN 'partial'
         ELSE 'pending'
       END
 WHERE tape_id = $1
   AND parity_state <> 'protected'        -- protected is terminal
   AND first_parity_data_ordinal < $W;    -- only objects the advance can touch
```

(`ordinal_end_exclusive` is the generated column above, so the
comparison is a plain indexed range test. Recovery does not
consult `parity_state` — it tests the failed block's ordinal
against the live watermark, 3c §7.2.1 — so this column is purely
operator-facing summary state.)

The watermark-advance UPDATE is range-driven, so it gets a
matching partial index (review):

```sql
CREATE INDEX catalog_objects_parity_range_idx
    ON catalog_objects(tape_id, first_parity_data_ordinal, ordinal_end_exclusive)
    WHERE parity_state <> 'protected';   -- protected is terminal; skip it
```

## Hard links, directories, special files (review #10)

rem-tar-v1 §5.9 added `HardlinkEntry`, `DirectoryEntry`, and
`SpecialFileEntry` to the manifest. Catalog treatment differs
by type, based on whether users restore by that entry's path:

**Hard links — catalog table.** Users may restore by a
hard-linked path, so hard links are directly catalog-
addressable:

```sql
CREATE TABLE catalog_hardlinks (
    hardlink_id    UUID NOT NULL,
    object_id      UUID NOT NULL REFERENCES catalog_objects(object_id),
    tape_id        UUID NOT NULL,
    path           TEXT NOT NULL,          -- the hard-link path within the object
    target_file_id UUID NOT NULL,          -- the regular catalog_files row it links to
    target_path    TEXT NOT NULL,          -- that file's path (denormalized for queries)
    mtime_ns       BIGINT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (object_id, hardlink_id),
    -- review: the link target must be a real file row in the SAME
    -- object (hard links never cross object boundaries in v1).
    -- Deferrable so it resolves within the object-commit transaction
    -- regardless of file-vs-hardlink insert order.
    CONSTRAINT catalog_hardlinks_target_fk
        FOREIGN KEY (object_id, target_file_id)
        REFERENCES catalog_files(object_id, file_id)
        DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX catalog_hardlinks_object_idx ON catalog_hardlinks(object_id);
CREATE INDEX catalog_hardlinks_path_idx ON catalog_hardlinks(object_id, path);
CREATE INDEX catalog_hardlinks_target_idx ON catalog_hardlinks(object_id, target_file_id);
```

A restore of a hard-link path resolves `target_file_id` →
`catalog_files` row → chunks, then recreates the link (or, for
partial restore of just the link path, extracts the target's
content as an independent file — rem-tar-v1 §5.9).

**Directories and special files — manifest-only.** Directory
metadata (`DirectoryEntry`) and the rare `SpecialFileEntry`
(only in `system-backup` mode) are **not** given catalog
tables. They are not independently restore-addressable the way
files are: directories are recreated as a side effect of
extracting their contents (with metadata applied from the
manifest), and special files only exist in a non-default mode.
A reader that needs directory metadata or special-file details
reads the object manifest (§10.5). This is an explicit
decision, not an omission: adding tables for entries nobody
queries by path would be schema weight with no query benefit.
If a future workload needs directory-metadata queries (e.g.
"which objects preserve directory xattrs"), a
`catalog_directories` table can be added then.

### ObjectWriteResult gains the new collections

```rust
pub struct ObjectWriteResult {
    pub object_entry: ObjectEntry,        // now carries tape_file_number,
                                          //   manifest_size_bytes, manifest_chunk_count
    pub file_entries: Vec<FileEntry>,
    pub symlink_entries: Vec<SymlinkEntry>,
    pub external_references: Vec<ExternalReference>,
    pub hardlink_entries: Vec<HardlinkEntry>,   // new: catalog_hardlinks rows
    pub tape_file_entry: TapeFileEntry,         // new: this object's catalog_tape_files row
    // directories/specials remain manifest-only; not surfaced as catalog rows
}
```

Layer 5 inserts the file, symlink, external-ref, hardlink, and
tape-file rows atomically at object close. Parity-sidecar and
bootstrap `catalog_tape_files` rows are inserted by the
write-session manager when 3c emits those tape files (see
`layer3c-design.md` §6.1, §7.2 `finish_object`/`finish`).

## Implementation order

1. Schema migration (alembic-style) on the catalog Postgres.
   Because `catalog_objects` and `catalog_tape_files` reference
   each other (an FK cycle), the migration MUST run in this order
   so neither FK targets a not-yet-existing table (review #10):
   1a. `CREATE TABLE catalog_tape_files` **without** its
       `object_id` FK (table + non-FK constraints + indexes).
   1b. `ALTER TABLE catalog_objects ADD COLUMN ...` for the
       parity/manifest/tape-file columns.
   1c. `ALTER TABLE catalog_objects ADD CONSTRAINT
       catalog_objects_tape_file_fk ... REFERENCES
       catalog_tape_files (...) DEFERRABLE INITIALLY DEFERRED`.
   1d. `ALTER TABLE catalog_tape_files ADD CONSTRAINT
       catalog_tape_files_object_fk ... REFERENCES
       catalog_objects(object_id) DEFERRABLE INITIALLY DEFERRED`.
   Both FKs deferrable so the object-commit transaction (3c
   §7.2.1) can insert both rows and resolve the cycle at COMMIT.
   The DDL above shows the FKs inline for readability; the actual
   migration adds them last per 1c/1d.
   1e. STAGED `NOT NULL` for a POPULATED catalog (review): the
       new `catalog_objects` columns are shown as `NOT NULL`, but
       a direct `ADD COLUMN ... NOT NULL` without a default fails
       on a table that already has rows. On an existing catalog,
       add each as nullable, backfill (from the manifests / a
       re-scan), then promote:
       `ADD COLUMN x ... NULL;` → backfill → `ALTER COLUMN x SET NOT NULL;`.
       A fresh catalog can use the `NOT NULL` shape directly. The
       DDL blocks above show the FINAL shape, not the migration
       transcript.
2. Wire `ObjectWriteResult` through Layer 5's write-session
   manager.
3. Read-path: query `catalog_files` first, fall back to
   manifest if no row.
4. rem-tar-v1 writer integration: surface per-file LBA data
   at object close.

Steps 1-2 are 3b work. Steps 3-4 are part of rem-tar-v1 impl
plan (see §14 of rem-tar-v1-design.md).

## Database charset requirement (UTF-8 only)

**The catalog database must use UTF-8 encoding for all
filename columns.** This is a hard requirement of rem-tar-v1
v0.4+ (see `rem-tar-v1-design.md` §5.7, §16.10).

### Why

The format enforces strict UTF-8 for all on-tape strings via
pre-write validation. If the catalog DB silently truncates,
transliterates, or rejects characters during INSERT, the
catalog's stored filename will not match the on-tape filename,
and subsequent restore lookups will fail. This is the failure
mode observed in production BRU/TOLIS deployments.

### Concrete requirements

**PostgreSQL (the default catalog backend):**

Database created with UTF-8 encoding:
```sql
CREATE DATABASE remanence_catalog
    ENCODING 'UTF8'
    LC_COLLATE 'C.UTF-8'
    LC_CTYPE 'C.UTF-8'
    TEMPLATE template0;
```

`TEXT` columns inherit the database encoding, so the `path`,
`uname`, `gname`, and other string columns will accept any
valid UTF-8 byte sequence.

Modern PostgreSQL installations default to UTF-8; the
`ENCODING 'UTF8'` clause makes this explicit and prevents
accidental misconfiguration.

**MySQL/MariaDB (if used as alternative backend):**

Database and table must use `utf8mb4` charset (NOT legacy
`utf8`, which is a 3-byte subset that excludes 4-byte UTF-8
characters used in many real filenames):
```sql
CREATE DATABASE remanence_catalog
    CHARACTER SET utf8mb4
    COLLATE utf8mb4_bin;
```

The `utf8mb4_bin` collation preserves byte-exact comparison
(case-sensitive, no normalization), which matches POSIX
filesystem semantics.

**SQLite (if used for local catalog cache):**

Uses UTF-8 by default for `TEXT` columns; no configuration
needed.

### Startup verification

Layer 5 must verify the catalog DB's encoding at startup and
refuse to operate if misconfigured. The check is one query:

```sql
-- PostgreSQL:
SHOW server_encoding;
-- Expected: UTF8

-- MySQL/MariaDB:
SHOW VARIABLES LIKE 'character_set_database';
-- Expected: utf8mb4
```

If the check fails, Layer 5 emits a fatal error with
remediation instructions and exits. This prevents the silent-
corruption failure mode that would otherwise compound over
weeks or months before being noticed.

### Migration

For existing deployments with misconfigured catalog encoding:

1. Dump the catalog to UTF-8-encoded SQL using
   `pg_dump --encoding=UTF8 ...` (or equivalent).
2. Create new database with correct UTF-8 encoding.
3. Restore from dump.
4. Run a scan job that re-reads each tape's manifest and
   verifies catalog filenames match exactly. Any mismatches
   flag the catalog rows as "encoding-suspect" and require
   manual operator review.

The scan job is operationally important: it's the only way
to detect filenames that were already corrupted in the old
catalog and would carry the corruption forward.
