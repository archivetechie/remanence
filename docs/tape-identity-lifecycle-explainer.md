# Tape identity and the retire operation

This page is for an operator who recycles cartridges, rebuilds a virtual tape library, or
meets an identity refusal from `rem tape init` or a write. It explains the two names a
Remanence cartridge carries, why Remanence refuses to write when they disagree, and how an
operator ends the life of a tape identity deliberately with `rem tape retire`, so that a
cartridge can be recycled or replaced without emptying the catalog. It also covers
`rem tape rebind`, which moves an operator's label from one identity to another. The page
is an explanation; nothing here is normative.

<!-- code-anchor: crates/remanence-state/src/projector/index.rs crates/remanence-api/src/tape_init.rs crates/remanence-api/src/pool_write/model.rs crates/remanence-state/src/state/authority_reset.rs crates/remanence-state/src/authority_manifest.rs crates/remanence-state/src/authority_layout.rs @ b00887a25e94ec016ace10ac726a3296346b0454 -->
## 1. A tape has two names, and they mean different things

A cartridge carries one name on the outside and one on the inside.

- The outside name is printed on the plastic shell. In a library it is the **barcode**, for
  example `RMJ101L9`, which the changer's scanner reads (the catalog stores it in the
  `voltag` column). On a standalone drive, which has no changer, it is the **label**: free
  text the operator writes on the shell and types into `rem tape init --label`. The catalog
  records where the name came from, `changer` or `operator`. An outside name can be peeled
  off, replaced, or by mistake duplicated, and it survives erasure of the tape.
- The inside name is the **tape UUID**, for example `5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f`.
  `rem tape init` writes it into the **bootstrap**, the first tape file at the beginning of
  the tape (BOT). The tape UUID names a logical tape: not the plastic, but one life of that
  plastic, with this geometry, this pool and these objects. It can be read only by loading
  the tape in a drive.

An analogy: the barcode is a licence plate and the tape UUID is the chassis number of one
car. Plates move between cars; a chassis number does not. When you wipe a tape and
initialize it again, you have not reused the old logical tape. You have created a new one
that lives in the same shell. The old life is over, its data is gone, and anything that
still refers to it refers to something that no longer exists.

The catalog (the SQLite file `rem-state.sqlite`) records the pairing in its `tapes` table.
Schematically, one row looks like this:

```text
tape_uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f   voltag RMJ101L9   state ready   pool_id archive
```

The write path uses the pairing as a safety check. Before any byte is written, the daemon
loads the tape, reads the bootstrap at BOT, and compares the tape UUID it found with the one
the catalog expects for the tape it selected. The check asks whether the tape in this drive
is the logical tape the system thinks it is writing to. On a physical library it prevents a
write to the wrong tape, which would destroy everything on that tape after the write point
(section 3).

## 2. When the two names disagree

The barcode and the tape UUID disagree whenever a cartridge's medium changes without the
catalog being told. Three situations show how this happens.

### Scenario A: a virtual tape library is rebuilt

Everything starts consistent. After `rem tape init`, the catalog pairs `RMJ101L9` with tape
UUID X, the bootstrap on the medium carries X, and the identity check passes, so writes
succeed.

Then the virtual tape library (the test setup that emulates cartridges) is rebuilt outside
Remanence. The virtual cartridges come back blank, or another tool initializes them and
writes a fresh bootstrap with tape UUID Y. The catalog is not told, so it still pairs
`RMJ101L9` with X and still says `ready`.

When the same workload runs again:

1. The pool selector consults the catalog and picks `RMJ101L9`, because the catalog says it
   is ready and has capacity.
2. The daemon loads it, reads BOT, and the check refuses the write as designed. For the
   re-initialized cartridge the client sees
   `FAILED_PRECONDITION: tape identity mismatch: expected 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f, found 3b158ca7-91d2-4e0f-8a6b-2c4d6e8f0a1b`;
   for the blank one it sees `absent bootstrap at BOT`.
3. Initializing the tape again is the natural next step, but `rem tape init RMJ101L9` also
   consults the catalog. It sees that the barcode is paired with X while the medium says Y,
   and classifies that as an anomaly. It writes nothing and prints, among other lines:

   ```text
   tape init RMJ101L9: refused-no-write
     decision: anomaly: barcode assigned to 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f; BOT uuid=3b158ca7-91d2-4e0f-8a6b-2c4d6e8f0a1b
   ```

   An anomaly has no override flag at all, neither `--force` nor `--clobber-data`. This is
   intentional, and section 3 explains why.
4. If every tape in the pool is in the same condition, the pool eventually reports
   `RESOURCE_EXHAUSTED: pool archive has no writable tapes (N rejection(s))`.
5. A full catalog reset,
   `rem reset-catalog --i-understand-this-erases-the-catalog` (`rem-debug catalog reset` is
   the same operation), removes the stale pairing, but also erases the whole catalog,
   the active library log (the hash-chained record of library decisions) and the tape logs
   (the per-tape records of committed content). The reset archives both log directories,
   the prior catalog, authority files and configuration snapshot under `reset-archives`, but
   the running system starts again from an empty catalog with no prior operation history. That is
   far more than the one stale pairing.

### Scenario B: recycling tapes between test runs

A script that initializes tapes again between runs is refused in one of two ways. If the
medium was erased or replaced outside Remanence, it meets the anomaly of Scenario A, step 3.
If the medium is intact and still holds data written under the catalog's tape UUID, init
returns a `refuse-clobber` decision: overwriting written media needs a separate clobber
confirmation, and is refused outright where the catalog records committed copies. (Init on
intact, unwritten media with a matching identity is an idempotent no-op.) A full catalog
reset between runs works only when the catalog is disposable. On a production catalog the
same sequence would empty the active catalog and library log every time a tape was recycled.

### Scenario C: losing a tape that holds copies

In a multi-copy setup every object lives on at least two tapes. If one tape is lost, the
objects on it should be copied again from the surviving copies, and the cartridge may be
reused.

1. A tape holding copies of objects A, B and C dies, or is wiped for reuse.
2. The operator needs to tell the system that the tape's contents are gone, so that it can
   list which objects are down to one copy and copy them again.
3. The catalog still lists the copies as `committed` on tape UUID X. Initializing the
   cartridge again meets the anomaly. Nothing can list the objects whose copies are gone,
   because nothing has recorded that the copies are gone.

Re-replication needs one operation: a way to end a tape's life and account for the
consequences. That operation is `rem tape retire` (section 4).

## 3. Why the catalog does not simply follow the medium

One possible rule is that the identity check should update the catalog whenever it finds a
different tape UUID. That rule would make every scenario above succeed. It would also
destroy data in the scenario that matters most.

### Scenario D: a mislabeled cartridge on a shared chassis

A person re-shelves cartridges after maintenance and swaps two barcode labels. This happens
in practice, and it is the reason archive software does not trust barcodes on their own. Or
a tape belonging to a second archive system that shares the same chassis ends up in a slot
visible to Remanence, wearing a reused label.

The catalog pairs `RMJ101L9` with X, and the medium in the slot carries tape UUID W, a live
tape full of another system's data. The write path selects `RMJ101L9`, loads it, and finds
W.

- With the identity check as it is, the mismatch is a refusal. The data on W survives, and
  the operator investigates.
- If the catalog followed the medium, the catalog would reconcile to W, the write would
  proceed, and the drive would truncate everything after the write point. The other
  system's archive would be destroyed by the check meant to protect it, and the write would
  report success.

From the drive, Scenario A (the medium was legitimately replaced) and Scenario D (the label
is wrong and the medium holds data that must be kept) look identical: the barcode says one
thing and BOT says another. Only the operator's intent separates them. For that reason the
way out is an explicit action performed by an operator, or by a script an operator wrote,
and the default stays refusal.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-state/src/state.rs crates/remanence-api/src/tape_init.rs crates/remanence-state/src/state/initialization.rs crates/remanence-state/src/projector/apply.rs crates/remanence-state/src/state/authority_reset.rs crates/remanence-state/src/authority_manifest.rs crates/remanence-state/src/authority_layout.rs @ b00887a25e94ec016ace10ac726a3296346b0454 -->
## 4. Retire: declaring a tape identity dead

`rem tape retire` is the explicit action. In words, the operator declares that logical tape
X is dead, that its contents are gone on purpose, that the system should stop expecting
them, and that its barcode is free for a new life.

The first argument is the barcode of the tape or its tape UUID as 32 hexadecimal digits.
`--reason` is free text recorded in the library-log `Retire` record, for example `recycled` or `vtl-rebuilt`.
The flag `--i-understand-copies-become-unreadable` is required on every run, including a
dry run. `--dry-run` reports what would be retired without writing, and `--json` prints the
`rem.tape.retire.v1` envelope.

To see what retiring `RMJ101L9` would do, before doing it:

```sh
rem tape retire RMJ101L9 --reason recycled \
    --i-understand-copies-become-unreadable --dry-run
```

The dry run names the identity and counts the copies and objects affected, then lists each
object that would lose its last committed copy:

```text
dry-run: would retire RMJ101L9 (uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f): 2 copies would be marked missing, 1 object would become degraded
  would lose last committed copy: <OBJECT_ID>
```

The same command without `--dry-run` retires the identity and prints
`retired RMJ101L9 (uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f): 2 copies marked missing, 1 object now degraded`.

Retirement first appends a `Retire` record to the library log, with the tape
identity, actor, time and reason. The record is synced before its catalog
projection. It has these effects:

1. The tape row changes to `state='retired'`, a terminal state. The pool selector never
   picks it again, since any state other than `ready` is already unwritable.
2. The barcode is released (`voltag` cleared). The catalog enforces barcode uniqueness, so
   the old pairing has to end before the same sticker can be paired with a new tape UUID.
3. Every copy the catalog had on that tape changes from `committed` to `missing`. This is
   the accurate answer to "can I restore object A from tape X?" after X was wiped. Objects
   with copies elsewhere remain restorable. Objects with no committed copy left are
   reported as degraded, and `retire` lists them: as "would lose last committed copy" lines
   under `--dry-run`, as a `warning:` line on standard error in text mode, and as the
   `degraded_objects` list in `--json` output (which carries no text warning). That list is
   the input re-replication needs.
4. The library-log `Retire` record keeps the decision through catalog rebuild.
   If the process stops after the record is durable but before projection,
   replay completes the catalog changes.
5. The retired row and its history stay in the catalog permanently. Retire is not delete.
   The purpose is to record what happened, not to rewrite it.

After `retire`, what `rem tape init RMJ101L9` does depends on what the medium carries:

- Blank media: the barcode is free, so this is a plain fresh initialization.
- Media still carrying the retired tape UUID's bootstrap and data: retirement changes the
  catalog but does not erase the cartridge. Initialization needs `--clobber-data` and the
  exact confirmation `CLOBBER RMJ101L9`, typed on standard input or given with
  `--confirm-clobber "CLOBBER RMJ101L9"`. It then writes a bootstrap with a fresh tape UUID.
  Rescue any bytes you need before confirming.
- Media carrying a tape UUID the catalog has never seen (Y in Scenario A): retiring X does
  not make Y writable. Init prints
  `observed BOT predecessor has no typed identity; no Bootstrap write admitted`,
  writes nothing and prints no `next:` line. The default is to adopt Y with
  `rem-debug tape adopt-bootstrap`, described in
  [Importing and recovering Remanence tapes](importing-and-recovering-remanence-tapes.md).
  To destroy Y instead, name it exactly: `--clobber-data --clobber-unknown-tape Y`, with the
  usual `CLOBBER` confirmation.
- A barcode paired with some other live tape UUID is still refused as an anomaly, so
  Scenario D stays protected. Retire covers one specific dead identity, never a whole
  category of tapes.

So in Scenario A, retire followed by init works for cartridges that come back blank.
Cartridges still carrying X also need the confirmed clobber. A cartridge that another tool
initialized under a new tape UUID Y needs adoption, or a clobber that names Y.

For a recycled tape, the two paths compare as follows. Without retire:

1. `rem tape init RMJ101L9` is refused.
2. A full catalog reset clears the active catalog, the library log and the tape logs (the
   inputs are archived under `reset-archives`).
3. The workload runs again against an empty catalog.

With retire:

1. `rem tape retire RMJ101L9 --reason recycled --i-understand-copies-become-unreadable`
   retires X.
2. `rem tape init RMJ101L9 --clobber-data`, confirmed with `CLOBBER RMJ101L9`, writes a new
   bootstrap.
3. The workload runs again. The existing history is retained. The library log
   records retirement and reprovisioning; new writes have their own tape log.

<!-- code-anchor: crates/remanence-state/src/state.rs crates/remanence-state/src/projector/tape_log_projection.rs crates/remanence-state/src/projector/index.rs crates/remanence-state/src/authority_manifest.rs @ b00887a25e94ec016ace10ac726a3296346b0454 -->
## 5. Retire and catalog rebuild

A retirement survives catalog rebuild because it is recorded in the library log.
The catalog is a SQLite projection of two durable sources: the library log and
the per-tape logs. The library log's `*.remlib` segments live in `audit.dir`,
with `authority-manifest.json` in the state directory binding their root and
identity. A tape log records committed content and finalization progress for
one tape. The audit log is not a rebuild source.

`rem rebuild-catalog-from-journals` starts from an empty catalog. It applies
the library log, including `Retire` and `Label` decisions, before the tape logs
in one catalog transaction. It carries over nothing from the old catalog.
A retired tape remains retired, its barcode remains released and its committed
copies are projected as missing. The tape's old content records remain history;
they do not undo the library-log decision.

## 6. What retire does not do

- It does not touch the tape. There is no load, no robot move, and no SCSI command. The
  medium may be in a drive, in a slot, or discarded. Retire records the decision
  in the library log and projects it into the catalog.
- It does not erase anything: not the row, not the tape log, not the copies' rows.
  Everything becomes history with an accurate status.
- It cannot be undone. There is no `un-retire`, and retired identities are permanent: an
  attempt to adopt a bootstrap whose tape UUID is retired is refused with a message that
  ends `is retired; retired identities are permanent`. If the wrong
  tape is retired, the medium's data is still physically intact until something overwrites
  it. `rem tape init` does overwrite it, so read what you need from the medium, read-only,
  before considering the cartridge for reuse. An undo flag would turn the retirement into a
  toggle, and every safety argument in section 3 assumes it is not one.
- Retiring an identity that is already retired is a no-op that reports success
  (`... is already retired; no change`) and appends no second library-log retirement record, but only when
  the target is the original tape UUID. Retirement clears the barcode, so a retry by barcode
  either fails (the barcode is no longer assigned) or, once the cartridge has been
  initialized again, retires the new identity that now holds the barcode. A script should
  record the tape UUID it retired and retry with that.
- It does not bypass any guard for unknown media. A tape the catalog never knew, or a
  barcode paired with a different live tape, is refused as before.
- A retire that changes anything needs exclusive access to the catalog. It takes the local
  state lock, so it fails cleanly while the daemon is running; stop the daemon first.
  `--dry-run` takes no state lock and opens the catalog read-only, so it can be used while
  the daemon runs.

<!-- code-anchor: crates/remanence-cli/src/standalone.rs crates/remanence-state/src/state.rs crates/remanence-state/src/projector/index.rs @ b00887a25e94ec016ace10ac726a3296346b0454 -->
## 7. Rebind: moving an operator label to another identity

On a standalone drive the outside name is the operator's label, and the operator may need
to move a label from one tape identity to another, both of them live. A typical case is a
shell relabelled by hand: the label `FIRST` used to belong to the cartridge whose tape UUID
is X, and it is now written on the shell of a cartridge whose tape UUID Y the catalog
already knows. Retire is the wrong tool here, because X is not dead; its objects are still
on it.

`rem tape init --label FIRST` on such a cartridge is refused before it writes anything, and
the refusal asks for an unused label:

```text
error: label FIRST belongs to tape 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f; choose a new label
```

For a blank rewritable cartridge, initialize it under an unused label first. A blank
cartridge has no bootstrap identity for rebind to read. Once the loaded
cartridge has a catalogued identity, rebind can move `FIRST` to it.

`rem tape rebind` is a standalone-drive command. It reads the tape UUID from the bootstrap of
the cartridge in the drive, so the cartridge that should carry the label must be loaded.
`--label <LABEL>` names the label, and `--previous-uuid <PREVIOUS_UUID>` names the identity
that holds it now. Naming the previous holder is the confirmation: the command proceeds only
if the catalog pairs the label with exactly that tape UUID.

To move the label `FIRST` from tape X to the cartridge now in the drive:

```sh
rem tape rebind --label FIRST --previous-uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f
```

The command prints the label it bound, as JSON:

```text
{
  "label": "FIRST"
}
```

A rebind does three things. It appends a `Label` record to the library log, naming
the loaded tape, the label and the previous tape UUID. It then clears the label from X and
gives it to the loaded tape, with `operator` as its provenance. Neither identity changes
state, and no copy changes status. X keeps its objects and simply has no label until the
operator gives it one. Like retirement, label rebinding is a library-log decision.
Catalog rebuild replays it before the tape logs (section 5), so the label stays
where the operator put it.

A rebind refuses with
`tape provisioning conflict: label prior holder differs from BOT-proven correction`
when the named previous tape does not hold the label, or is the loaded tape
itself. Check which identity holds the label before retrying. The loaded and
previous identities must both have library-log provisioning records; an unknown
cartridge cannot take a label through rebind.

## 8. Day-to-day use

| Situation | Without retire | With retire |
| --- | --- | --- |
| Test setup rebuilt (virtual tape library) | `tape identity mismatch`. A full catalog reset clears the active catalog, library log and tape logs (archiving them). | Retire each affected tape by tape UUID, then run `rem tape init` for rewritable cartridges that are blank or still carry the old tape UUID, then run the workload again. The catalog and library-log history stay intact. Cartridges initialized under an unknown tape UUID need adoption, or a clobber that names that UUID. |
| Recycling tapes between runs | Init is refused: an anomaly if the medium was changed outside Remanence, `refuse-clobber` if it holds written data. | A script calls retire, by the original tape UUID, and then init. A repeated retire of the same tape UUID is a no-op that reports success. |
| Which objects lost all their copies? | Nothing can answer this. | `rem tape retire --dry-run` lists the objects that would lose their last committed copy. |
| Mislabeled or foreign cartridge | Refused. | Refused, unchanged. |
| A standalone label moved to another live cartridge | `tape init` is refused because the label belongs to another tape. | Not a case for retire: `rem tape rebind --previous-uuid` moves the label and leaves both identities live. |

In plain terms: the catalog can record that a tape exists, and `retire` lets it record that
a tape identity has ended. The record is audited, survives catalog rebuilds, frees the
barcode for the next use of the cartridge, and tells restore and re-replication which copies
are gone. `rebind` does the smaller job of moving a label between two live tapes. Every
refusal that protects real data stays in force.
