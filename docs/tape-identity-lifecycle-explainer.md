# Tape identity and the retire operation

This page is for an operator who recycles cartridges, rebuilds a virtual tape library, or
meets an identity refusal from `rem tape init` or a write. It explains the two names a
Remanence cartridge carries, why Remanence refuses to write when they disagree, and how an
operator ends the life of a tape identity deliberately with `rem tape retire`, so that a
cartridge can be recycled or replaced without emptying the catalog. It also covers
`rem tape rebind`, which moves an operator's label from one identity to another. The page
is an explanation; nothing here is normative.

<!-- code-anchor: crates/remanence-state/src/index.rs crates/remanence-api/src/tape_init.rs crates/remanence-api/src/pool_write/model.rs @ 625f7044 -->
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
5. Without `retire`, the only remaining way forward is a full catalog reset,
   `rem reset-catalog --i-understand-this-erases-the-catalog` (`rem-debug catalog reset` is
   the same operation). It removes the stale pairing, but it also erases the whole catalog,
   the active audit log (the hash-chained record of every operation) and the tape logs (the
   per-tape records of what was committed to each tape). The reset moves the audit log and
   the tape logs into a numbered directory under `reset-archives` in the state directory, but
   the running system starts again from an empty catalog with no operation history. That is
   far more than the one stale pairing.

### Scenario B: recycling tapes between test runs

A script that initializes tapes again between runs is refused in one of two ways. If the
medium was erased or replaced outside Remanence, it meets the anomaly of Scenario A, step 3.
If the medium is intact and still holds data written under the catalog's tape UUID, init
returns a `refuse-clobber` decision: overwriting written media needs a separate clobber
confirmation, and is refused outright where the catalog records committed copies. (Init on
intact, unwritten media with a matching identity is an idempotent no-op.) A full catalog
reset between runs works only when the catalog is disposable. On a production catalog the
same sequence would empty the active catalog and audit log every time a tape was recycled.

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

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-state/src/state.rs crates/remanence-api/src/tape_init.rs @ 625f7044 -->
## 4. Retire: declaring a tape identity dead

`rem tape retire` is the explicit action. In words, the operator declares that logical tape
X is dead, that its contents are gone on purpose, that the system should stop expecting
them, and that its barcode is free for a new life.

The first argument is the barcode of the tape or its tape UUID as 32 hexadecimal digits.
`--reason` is free text recorded in the audit log, for example `recycled` or `vtl-rebuilt`.
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

A retire has five effects. Effects 1 to 3 and 5 are committed in one catalog transaction;
the audit record (effect 4) is appended after that transaction commits.

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
4. A `TapeRetired` record is appended to the hash-chained audit log. It records who, when,
   which barcode, why, and how many copies were declared missing. A destructive declaration
   belongs in the permanent record, and section 5 shows that this record is also what keeps
   the retirement through a catalog rebuild. The catalog is committed first and the audit
   append follows. If the append fails, the command reports an error although the
   retirement has already taken effect and is not rolled back; check the tape's state and
   the audit log before retrying.
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
  not make Y writable. Init returns a `needs-explicit-rebuild` decision, writes nothing, and
  prints a `next:` line with both ways out. The default is to adopt Y with
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
2. A full catalog reset clears the active catalog, the audit log and the tape logs (the
   inputs are archived under `reset-archives`).
3. The workload runs again against an empty catalog.

With retire:

1. `rem tape retire RMJ101L9 --reason recycled --i-understand-copies-become-unreadable`
   retires X.
2. `rem tape init RMJ101L9 --clobber-data`, confirmed with `CLOBBER RMJ101L9`, writes a new
   bootstrap.
3. The workload runs again. The catalog, the audit log and the tape logs are intact, and the
   audit log has gained one `TapeRetired` record.

<!-- code-anchor: crates/remanence-state/src/state.rs crates/remanence-state/src/tape_log/projection.rs crates/remanence-state/src/index.rs @ 625f7044 -->
## 5. Retire and catalog rebuild

A retirement has to survive a catalog rebuild. This section explains why that takes more
than a single `UPDATE` statement, and how the rebuild keeps it.

Remanence treats the catalog as a projection: a set of SQLite tables derived from two
durable sources, the per-tape logs and the audit log. The **tape log** is the host's
append-only record for one tape of what was committed to it. The audit log is the
hash-chained record of operator and system actions. The catalog can be rebuilt from them at
any time with `rem rebuild-catalog-from-journals` (the command keeps an older name; it reads
the tape logs and the audit log). The daemon does a smaller version of the same work when it
starts: it projects any tape-log records the catalog has not seen yet, and it skips retired
tapes.

Following a retired tape through a rebuild shows the problem.

1. Tape X was written once. Its tape log says that objects A and B were committed to X.
   That log is history. It stays on disk, and it is correct, because those objects were
   committed to X.
2. The operator wipes the medium and retires X. The catalog row is `retired` and the copies
   are `missing`.
3. Months later a rebuild runs. It clears the catalog tables and replays the tape logs,
   which recreate the copies of A and B on X as `committed`.

If nothing else happened, step 3 would undo the retirement without a word. Restore planning
would again believe that copies exist on a wiped medium, and the next initialization of that
cartridge would produce Scenario A again. An identity fact held only in SQLite is lost when
the catalog is rebuilt.

The rebuild prevents this because the retirement is not held only in SQLite. The `TapeRetired`
record in the audit log is the durable fact, and the rebuild applies the audit log after the
tape logs. The rebuild runs as one SQLite transaction, in this order:

1. It reads the operator-set columns of the existing tape rows: barcode, pool, cartridge
   generation, finalization progress, and lifecycle states such as `sealed` and `retired`.
2. It clears the rebuildable tables and puts those tape rows back.
3. It replays every tape log. For X this recreates the tape-file rows and the copies of A
   and B as `committed`.
4. It writes the operator-set columns from step 1 back over the rows the logs produced, so
   a row that was `retired` before the rebuild is `retired` again.
5. It replays the catalog records in the audit log (tape provisioning, adoption, label
   rebinds, seals and retirements), so that an audit fact wins over anything the tape logs
   implied. The `TapeRetired` record for X sets the state to `retired` and clears the
   barcode. This step needs nothing from the old catalog, so the retirement is restored even
   when the catalog file itself was lost or deleted before the rebuild.
6. It marks every `committed` copy on a retired tape `missing` again.

Copy status is a consequence of tape state, never an independent fact. The rebuild therefore
has one thing to keep, the tape's state, and recomputes the copies from it, so the two cannot
drift apart.

The tape log is not deleted at retire time, because it is the authoritative history that A
and B were written to X. Retire records a change in the present and leaves the past as it
was. Both statements stay true: the tape log says that A and B were written to X, and the
catalog says that X is gone.

## 6. What retire does not do

- It does not touch the tape. There is no load, no robot move, and no SCSI command. The
  medium may be in a drive, in a slot, or discarded. Retire is a catalog and audit
  operation about expectations.
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
  (`... is already retired; no change`) and appends no second audit record, but only when
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

<!-- code-anchor: crates/remanence-cli/src/standalone.rs crates/remanence-state/src/state.rs crates/remanence-state/src/index.rs @ 625f7044 -->
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

For a blank cartridge, initialize it under an unused label first. A blank
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

A rebind does three things. It appends a `TapeLabelRebound` record to the audit log, naming
the loaded tape, the label and the previous tape UUID. It then clears the label from X and
gives it to the loaded tape, with `operator` as its provenance. Neither identity changes
state, and no copy changes status. X keeps its objects and simply has no label until the
operator gives it one. Like a retirement, the rebind is an audit record, so a catalog rebuild
replays it (section 5, step 5) and the label stays where the operator put it.

A rebind is refused, and changes nothing, in two cases:

- `tape provisioning conflict: previous UUID does not match the current label binding`
  means the label is not paired with the tape UUID given in `--previous-uuid`. The init
  refusal shown above names the current holder; use that tape UUID.
- `tape provisioning conflict: rebind target must be an existing live identity` means the
  cartridge in the drive is not a catalogued, unretired tape. A blank cartridge, or one whose
  identity was retired, cannot take a label this way; initialize it with a new label, or
  retire the old identity first if its contents are really gone.

## 8. Day-to-day use

| Situation | Without retire | With retire |
| --- | --- | --- |
| Test setup rebuilt (virtual tape library) | `tape identity mismatch`. The only way forward is a full catalog reset, which clears the active catalog, audit log and tape logs (archiving them). | Retire each affected tape by tape UUID, then run `rem tape init` for blank cartridges and for those still carrying the old tape UUID, then run the workload again. The catalog and audit history stay intact. Cartridges initialized under an unknown tape UUID need adoption, or a clobber that names that UUID. |
| Recycling tapes between runs | Init is refused: an anomaly if the medium was changed outside Remanence, `refuse-clobber` if it holds written data. | A script calls retire, by the original tape UUID, and then init. A repeated retire of the same tape UUID is a no-op that reports success. |
| Which objects lost all their copies? | Nothing can answer this. | `rem tape retire --dry-run` lists the objects that would lose their last committed copy. |
| Mislabeled or foreign cartridge | Refused. | Refused, unchanged. |
| A standalone label moved to another live cartridge | `tape init` is refused because the label belongs to another tape. | Not a case for retire: `rem tape rebind --previous-uuid` moves the label and leaves both identities live. |

In plain terms: the catalog can record that a tape exists, and `retire` lets it record that
a tape identity has ended. The record is audited, survives catalog rebuilds, frees the
barcode for the next use of the cartridge, and tells restore and re-replication which copies
are gone. `rebind` does the smaller job of moving a label between two live tapes. Every
refusal that protects real data stays in force.
