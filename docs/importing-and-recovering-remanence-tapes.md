# Importing and recovering Remanence tapes

Remanence can reconstruct a local tape identity from a checksum-valid
Remanence Bootstrap without rewriting the cartridge. This is useful after a
catalog loss, when an offsite cartridge returns without its local SQLite row,
or when a Remanence tape moves between sites.

This workflow distinguishes two meanings of "foreign" that should not be
mixed:

- A **Rem-native, locally unknown tape** has a valid Remanence Bootstrap but no
  matching identity in the current site's catalog. It may be imported through
  the adoption workflow below.
- A **foreign-format tape** was written in another on-tape format. A recognized
  foreign signature, damaged Remanence Bootstrap, or unrecognized BOT data is
  not adoptable as a Remanence identity. Reading it requires a separate,
  format-specific adapter and import pipeline.

Adoption restores identity only. It does not claim that the site's catalog
knows the tape's Objects, files, committed prefix, terminal indexes, or
finalization history.

For example, Site A can write a Remanence tape and send it to an offsite vault.
If Site B later receives the cartridge without Site A's SQLite catalog, Site B
can probe and adopt the Bootstrap identity, then inventory the terminal indexes
and rebuild the authority that actually survives. An LTFS cartridge in the same
shipment takes the separate LTFS import path; adoption will reject it.

In plain terms, adoption records the native tape's identity, geometry, pool,
and conservative lifecycle state. It does not rewrite the tape, decrypt or
restore Objects, or invent file and index records.

## When to use it

Typical cases include:

- the rebuildable SQLite catalog was lost, while cartridges and durable audit
  or journal material survived;
- an offsite or vault cartridge returned to a replacement host;
- a cartridge is being transferred between Remanence sites;
- a locally unknown cartridge must be identified before terminal-index
  inventory or a catalog rebuild.

Do not use adoption to relabel a cartridge, override a retired identity, turn a
foreign format into a Remanence format, or make an already-written tape
writable without recovering its real catalog authority.

For a site-to-site transfer, the destination must understand the tape's draft
Bootstrap and on-tape format version. Its configuration also needs a pool rule
that maps the barcode to an existing pool. Adoption does not decrypt or restore
Objects; encrypted Objects still need their recovery keys. A WORM cartridge is
safe for this read-only identity step, but remains non-writable. A genuine LTFS,
tar, or other foreign-format cartridge requires a separate format-specific
adapter and import pipeline. Adoption never converts it into a Remanence tape.

## Safety model

The workflow has two physical reads:

1. `bot-probe` is advisory. It resolves one exact barcode in one configured
   library, checks its exact home slot, opens one read-compatible drive, and
   reads BOT with exactly one `LOCATE(0)` and at most one `READ`.
2. `adopt-bootstrap` does not trust the earlier probe as authority. It acquires
   the exclusive `StateHandle`, performs fresh discovery, revalidates the exact
   barcode, library, home slot, and selected drive, and rereads the Bootstrap
   and its immediate physical tail under the same drive handle.

Both commands temporarily select fixed 1 MiB blocks with drive compression
disabled. They verify that setting, restore and verify the prior drive mode,
and park the cartridge in the exact expected home slot. A restore or park
failure fails the command. Neither command writes tape data, filemarks, or a
new Bootstrap.

The adoption read accepts only an RFC 4122 UUIDv4 and the canonical 1 MiB
default-parity Bootstrap geometry with compression disabled. The expected UUID
argument is compared with the UUID from this authoritative fresh BOT read, not
with discovery or catalog data.

`StateHandle` is a local advisory lock. These commands do not acquire a SCSI
persistent reservation and the daemon does not participate in the state lock.
Before importing, stop or quiesce the daemon and any other robotics or tape
software that could move or use the same cartridge or drive.

## Commands

First collect advisory identity evidence:

```text
rem-debug --allow LIBRARY_SERIAL \
  tape bot-probe BARCODE \
  --library LIBRARY_SERIAL \
  --expected-home-slot 0x0401 \
  --json
```

For a valid native Bootstrap, preserve the reported `tape_uuid` and confirm the
reported library, barcode, source slot, drive, compression flags, and geometry.
Then perform authoritative adoption. The command generates its own operation
UUID, so the caller does not need to create or retain an operation receipt.

```text
rem-debug --allow LIBRARY_SERIAL \
  tape adopt-bootstrap BARCODE \
  --library LIBRARY_SERIAL \
  --expected-home-slot 0x0401 \
  --expected-existing-tape-uuid TAPE_UUID_FROM_PROBE \
  --json
```

The probe emits schema `rem.tape.bot-probe.v1`. Important fields are
`physical_disposition`, `library_serial`, `library_revision`, `barcode`,
`source_slot`, `drive_element`, `drive_serial`, `configured_drive_compression`,
`bootstrap_drive_compression`, `tape_uuid`, `geometry`,
`canonical_adoption_geometry`, and `detail`.

Adoption emits schema `rem.tape.adopt-bootstrap.v1`. In addition to physical
identity and geometry it reports `physical_tail`, `newly_adopted`,
`operation_id`, `idempotency_fingerprint`, and the resulting identity-only
`catalog` projection, including its state, pool, and assignment generation.

## What adoption creates

A successful first adoption fsyncs a `TapeIdentityAdopted` audit record before
projecting the SQLite row. The row contains the exact tape UUID, barcode,
canonical geometry, configuration-derived pool, lifecycle state, and assignment
generation. It creates no tape-file, Object-copy, catalog-unit, wrap-map,
committed-prefix, session, fence, or finalization authority.

The lifecycle state is deliberately conservative:

- exactly `Bootstrap → one filemark → EOD` becomes `ready`; this is an
  identity-only Bootstrap tape with no later physical content;
- data after Bootstrap, a missing or extra filemark, or an unreadable tail after
  a valid Bootstrap becomes `recovery_required`.

`recovery_required` is not a damaged-identity verdict. It means the valid
identity was recovered but the tape has more, different, or uncertain physical
content that must be inventoried before normal use.

## Crash recovery and natural idempotency

If the process stops after the audit append but before the SQLite projection, a
later invocation replays the audit first and repairs the missing projection.
Internally, one operation UUID cannot be rebound to a different tape or changed
facts.

Each caller retry gets a newly generated operation UUID. After audit replay, it
returns an `already adopted` no-op only when the current row is still the exact
same identity-only UUID, barcode, geometry, pool, assignment generation, and
tail-derived state, with no Object, file, prefix, or finalization authority, no
open session or active fence, and no active media authority. No second adoption
audit record is needed. Once writes or lifecycle authority evolve that row,
re-adoption refuses rather than rolling state back.

## After adoption

For an exact Bootstrap-only tape reported `ready`, the catalog has enough
identity to treat it as an empty native cartridge, but still has no invented
Object history.

For `recovery_required`, keep the tape out of write service. With the daemon
running under controlled ownership, inspect its terminal indexes:

```text
rem tape inventory --tape-uuid TAPE_UUID --json
rem tape verify-index --tape-uuid TAPE_UUID --json
```

Inventory attempts terminal replicas in the defined fallback order and reports
BOT recovery explicitly if none survives. Full verification separately measures
and validates the physical prefix and terminal structures. Restore any surviving
per-tape journals and audit material to their configured locations, then rebuild
the SQLite projection when appropriate:

```text
rem rebuild-catalog-from-journals --config /path/to/config.toml
```

Do not treat streamed inventory rows as committed catalog authority unless the
documented inventory summary and subsequent recovery/rebuild procedure says
they are authoritative.

## Refusals to expect

The commands fail closed for an ambiguous barcode, wrong library or home slot,
UUID mismatch, damaged or non-v4 Bootstrap identity, foreign or unrecognized
format, noncanonical geometry, enabled compression, active media-readiness
ownership, barcode/pool conflict, conflict with any tape kind, retired identity,
or an existing row carrying file, Object, prefix, finalization, open-session,
active-fence, or other evolved authority. Physical readiness, mode restoration,
and exact parking failures also prevent catalog mutation.

## Practical import checklist

1. Quiesce the daemon, robotics, backup software, and any other drive owner.
2. Confirm the target library is configured and the cartridge barcode appears
   exactly once.
3. Record the exact storage/home element and ensure the destination is usable.
4. Run `bot-probe`; require `bootstrap_valid`, canonical geometry, compression
   off, and the expected library/barcode/home evidence.
5. Review the reported UUID rather than obtaining it from a stale catalog.
6. Run `adopt-bootstrap` with that UUID, the same exact library and home slot,
   and review the internally generated operation UUID in the JSON result.
7. Confirm the cartridge was parked and inspect the returned catalog state.
8. If `recovery_required`, keep it read-only and run terminal inventory and
   verification before restoring journals or rebuilding the catalog.
9. Resume daemon or site operations only after ownership and recovered catalog
   authority are reconciled.
