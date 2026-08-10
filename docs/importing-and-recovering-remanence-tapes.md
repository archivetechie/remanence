# Importing and recovering Remanence tapes

Remanence can reconstruct a local tape identity from a checksum-valid
Remanence Bootstrap without rewriting the cartridge. This is useful after a
catalog loss, when an offsite cartridge returns without its local SQLite row,
or when a Remanence tape moves between sites.

## The short version

Think of a Remanence tape as carrying its own identity card. The first record
on the tape, called the **Bootstrap**, says which tape this is and how it was
written. A Remanence installation also keeps a local SQLite catalog, which is
more like the site's card index: it says which tapes and Objects that site
knows about.

Moving a cartridge to another site, or losing the SQLite database, can remove
the card-index entry without erasing the identity card on the tape. The new
recovery capability safely reads that on-tape identity and creates a minimal,
conservative local record for it. It does not initialize the tape and does not
write anything to it.

Import and recovery then happen in three separate stages:

| Stage | What Remanence learns | What it deliberately does not assume |
|---|---|---|
| **Probe** | Whether BOT contains a valid Remanence Bootstrap, plus its tape UUID and geometry | That the local catalog is correct or that the rest of the tape is healthy |
| **Adopt** | The tape's identity, barcode, pool mapping, geometry, and a conservative lifecycle state | Which Objects are present or whether they were committed |
| **Inventory/verify** | The contents and structural evidence that can be read from terminal indexes or a scan starting at BOT | That read-only inventory has rebuilt the site's writable catalog |

This separation is intentional. It lets Remanence recognize and inspect a
cartridge without inventing a history for it. A finalized tape normally offers
a fast terminal index. A partially written tape, or a tape whose terminal
indexes are unreadable, may require a complete scan from the beginning instead.

Current production code does **not** turn tape-only inventory rows into the
ordinary authoritative Object/copy/file catalog. `rebuild-catalog-from-journals`
replays surviving host audit logs and per-tape journals; it does not ingest a
terminal index or BOT scan. A site holding only a cartridge can identify and
inspect it, but tape-only catalog import remains future work.

## What “foreign tape” means

People use “foreign” for two quite different situations:

| Cartridge | Supported by this workflow? | Correct path |
|---|---:|---|
| A Remanence tape written at another site | Partly | Probe and adopt its identity, then inventory/verify; ordinary catalog import additionally needs transferred host journals today |
| A Remanence tape whose local SQLite row was lost | Yes | The same probe-and-adopt workflow |
| A Remanence tape with a damaged or unreadable Bootstrap | No | Do not adopt; preserve the cartridge for a deeper recovery procedure |
| An LTFS, tar, Dwara, or other non-Remanence tape | No | Use a separately supplied, read-only foreign-format adapter |
| A blank tape | No | Initialize it as new media instead of adopting it |

The important supported case is therefore **foreign to this installation,
but native to Remanence**. The adoption command recognizes the identity already
on that tape; it does not convert another tape format into Remanence.

Stock Remanence does not ship a concrete LTFS, tar, Dwara, or other legacy
parser. Its core exposes the adapter interface, while separately versioned
distributions may link supported read-only adapters. See
[Foreign-format adapters](reference-foreign-format-adapters.md) for that
separate path and its current availability.

Adoption restores identity only. It does not claim that the site's catalog
knows the tape's Objects, files, committed prefix, terminal indexes, or
finalization history.

For example, Site A can write a Remanence tape and send it to an offsite vault.
If Site B later receives the cartridge without Site A's SQLite catalog, Site B
can probe and adopt the Bootstrap identity, then inventory and verify the
terminal indexes. If Site A also transferred its audit and per-tape journals,
Site B can rebuild the ordinary catalog from those host records. With the
cartridge alone, Site B can inspect but cannot yet import those inventory rows
as ordinary catalog authority. An LTFS cartridge in the same shipment needs a
distribution with an LTFS adapter; the stock Remanence tools will reject it as
unrecognized or unsupported rather than guessing.

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

Terminal finalization has a useful no-media recovery boundary. Once the
checkpoint and parity journal prove that replica C completed its synchronizing
barrier, all three full indexes are already on tape. A restart completes the
remaining sealed-checkpoint, SQLite projection, intent cleanup, and audit work
without loading, locating, reading, or writing the cartridge. This applies to
automatic finalization and to an operator-requested close-out.

During finalization, Remanence keeps a small durable recovery record on the
host. The code calls this record the *companion intent*. It records what the
writer was doing and where a restart must resume. It is not another index or
checkpoint on the tape.

If the operation was already classified `recovery_required`, the companion
keeps that conservative flag until the sealed checkpoint is safely on disk. A
failure before that point cannot silently downgrade the tape to ordinary
`finalizing`. After the sealed checkpoint is durable, recovery can replay it
and remove the now-stale companion. When a sealed checkpoint and companion
coexist after a crash, recovery first proves they describe the same completion,
then repairs SQLite while retaining the companion. It removes the companion
only after that repair succeeds. Changes to a pool's capacity cap or
watermarks do not block this final host-only bookkeeping, because the complete
reserved tail is already barrier-proved on tape.

An ordinary write session cannot take ownership while that recovery companion
exists, even when the sealed checkpoint already matches it. The explicit
terminal-recovery path must project the durable completion first and remove the
companion second. This prevents a normal append opener from accidentally
discarding the information that tells a later restart how to finish recovery.

The same restart also repairs missing completion records in the append-only
audit log. It records one tape-sealed event and, for a manual close-out, one
operation-finished event. If either event was already written before the
crash, recovery reuses it instead of adding a duplicate. These two records are
always flushed to stable storage. Remanence also permits only one daemon or
offline state owner to use a state directory at a time, so another process
cannot race that decision.

Before that boundary, a failed or uncertain component is recorded durably as
`recovery_required`. Restart does not quietly turn that state back into ordinary
`finalizing`; it must reconcile the next physical component first. This
distinction prevents a host metadata failure after a proved replica C from
causing needless tape motion while still fencing genuinely uncertain media.

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

In practical terms, a successful adoption means “we now know exactly which
tape this is.” It does not yet mean “this tape is ready for new writes.” A
`recovery_required` tape stays fenced from normal write use until the inventory,
commit history, and local catalog agree.

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
   verification before restoring transferred journals or rebuilding the
   catalog. If no host journals survived, keep the tape read-only: tape-only
   inventory-to-catalog import is not implemented yet.
9. Resume daemon or site operations only after ownership and recovered catalog
   authority are reconciled.
