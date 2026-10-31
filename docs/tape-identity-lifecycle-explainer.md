# Tape identity and the retire operation

A Remanence tape cartridge is known by a shell label and a logical tape UUID.
This document explains what each one means, why Remanence refuses to write when they disagree,
and how an operator ends the life of a tape identity deliberately with
`rem tape retire`, so that a cartridge can be recycled or replaced without
emptying the catalog. It is an explainer; nothing here is normative.

---

<!-- code-anchor: crates/remanence-state/src/index.rs crates/remanence-api/src/mount_backend.rs @ 4dc2a3c6 -->
## 1. A tape has two names, and they mean different things

A cartridge has two names:

- The barcode (voltag), for example `RMJ101L9`, is a sticker on the plastic shell.
  A changer scanner is one reader of the label; the operator is another.
  A standalone label can be free text written on the shell. The catalog records
  provenance as `changer` or `operator`; neither replaces BOT identity.
  It can be peeled off, replaced, or (through human error) duplicated, and it
  survives erasure of the tape.
- The BOT uuid, for example `5e8f8d3b-…`, is a UUID inside the bootstrap block that
  `rem tape init` writes at the very beginning of the tape (Beginning Of Tape, BOT).
  It names a logical tape: not the plastic, but one life of that plastic, with this
  geometry, this pool and these objects. It can be read only by mounting the tape in
  a drive.

An analogy: the barcode is a license plate and the BOT uuid is the VIN of one car.
Plates move between cars; a VIN does not. When you wipe a tape and re-initialize it,
you have not reused the old logical tape. You have created a new one that lives in
the same shell. The old life is over, its data is gone, and anything that still
refers to it refers to something that no longer exists.

The catalog (`rem-state.sqlite`) records the pairing:

```text
tapes:  uuid 5e8f8d3b…  voltag=RMJ101L9  state=ready  pool=amber-aof
```

The write path uses the pairing as a safety check. Before any byte is written, the
daemon mounts the tape, reads the BOT bootstrap, and compares the uuid it found with
the uuid the catalog expects for the tape it selected. The check asks whether the
tape in this drive is the logical tape the system thinks it is writing to.

On a physical library this check prevents the worst archival accident, which is
writing over the wrong tape (§3).

## 2. When the two names disagree

The barcode and the BOT uuid disagree whenever a cartridge's medium changes without
the catalog being told. Three situations show how this happens.

### Scenario A: a virtual tape library is rebuilt

Everything starts consistent:

```text
Catalog:  RMJ101L9 → uuid X   state=ready    (after rem tape init)
Medium:   BOT bootstrap uuid X
          → identity check: expected X, found X ✓ writes succeed
```

Then the virtual tape library (the test setup that emulates cartridges) is rebuilt
outside Remanence. The virtual cartridges come back blank, or are re-initialized by
another tool and gain a fresh bootstrap with uuid Y. The catalog is not told:

```text
Catalog:  RMJ101L9 → uuid X   state=ready    (stale)
Medium:   blank, or BOT uuid Y
```

When the same workload runs again:

1. The pool selector consults the catalog and picks RMJ101L9, because the catalog
   says it is ready and has capacity.
2. The daemon mounts it, reads BOT, and the check fires as designed:
   `FAILED_PRECONDITION: tape identity mismatch: expected 5e8f8d3b…, found 3b158ca7…`
   (or `absent bootstrap at BOT` for the blank case).
3. Re-initializing the tape is the natural next step, but `rem tape init RMJ101L9`
   also consults the catalog. It sees that the barcode is already paired with uuid X
   while the medium says Y, and classifies that as an anomaly. The command prints an
   action line and, separately, a `decision:` line; in summary the result is
   `tape init RMJ101L9: refused-no-write`, with a decision naming the anomaly (barcode
   assigned to 5e8f8d3b…, BOT uuid 3b158ca7…).
   The anomaly case has no override flag at all, neither `--force` nor
   `--clobber-data`. This is intentional (§3 explains why).
4. If every tape in the pool is in the same condition, the pool eventually reports
   `RESOURCE_EXHAUSTED: pool amber-aof has no writable tapes (N rejection(s))`.
5. Without `retire`, the only remaining option is a full catalog reset:
   `rem-debug catalog reset --i-understand-this-erases-the-catalog`. It removes the
   stale pairing, but it also clears the entire SQLite catalog and the active audit log
   (the tamper-evident record of every operation) and the active 3c journals (the
   per-tape records of what was committed to each tape). The audit and journal inputs
   are archived under `reset-archives` in the state directory, but the running system
   starts again from an empty catalog and loses its operation history, which is far more
   than the one stale pairing.

### Scenario B: recycling tapes between test runs

A script that re-initializes tapes between runs gets refused in one of two ways. If
the medium was erased or replaced outside Remanence, it hits the anomaly of Scenario A
step 3. If the medium is intact and still holds data written under the catalog's uuid,
init reaches `RefuseClobber`: overwriting written media needs a separate clobber
confirmation, and is refused outright where committed copies are recorded. (Init on
intact, unwritten media with a matching identity is an idempotent no-op.) A full catalog
reset between runs works only when the catalog is disposable. On a production catalog
the same sequence would empty the active catalog and audit log every time a tape was
recycled.

### Scenario C: losing a tape that holds copies

In a multi-copy setup every object lives on at least two tapes. If one tape is lost,
the objects on it should be re-replicated from the surviving copies, and the cartridge
may be reused.

1. A tape holding copies of objects A, B and C dies, or is wiped for reuse.
2. The operator needs to tell the system that the tape's contents are gone, so that it
   can list which objects are down to one copy and re-replicate them.
3. The catalog still lists the copies as `committed` on uuid X. Re-initializing the
   cartridge hits the anomaly. Nothing can list the objects whose copies are gone,
   because nothing has recorded that the copies are gone.

Re-replication needs one operation: a way to end a tape's life and account for the
consequences. That operation is `rem tape retire` (§4).

## 3. Why the catalog does not simply follow the medium

One possible rule is that the identity check should update the catalog whenever it
finds a different uuid. That rule would make every scenario above succeed. It would
also destroy data in the scenario that matters most.

### Scenario D: a mislabeled cartridge on a shared chassis

A person re-shelves cartridges after maintenance and two barcode labels are swapped.
This happens, and it is the reason enterprise software does not trust barcodes. Or a
tape belonging to a second archive system that shares the same chassis ends up in a
slot visible to Remanence, wearing a reused label.

The catalog says `RMJ101L9 → X`, and the medium in the slot carries uuid `W`, a live
tape full of someone's data. The write path selects RMJ101L9, mounts it, and finds W.

- With the identity check as it is: the mismatch is a refusal, the data on W survives,
  and the operator investigates.
- If the catalog followed the medium: the catalog would reconcile to W, the write
  would proceed, and the tape write would truncate everything after the write point.
  Someone's archive would be destroyed by the safety system, with a success exit code.

From the drive, Scenario A (the medium was legitimately replaced) and Scenario D (the
label is wrong and the medium holds precious data) look identical: the barcode says one
thing and BOT says another. Only the operator's intent separates them. For that reason
the way out is an explicit action performed by an operator (or by a script an operator
wrote), and the default stays refusal.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-api/src/tape_init.rs @ 4dc2a3c6 -->
## 4. Retire: declaring a tape identity dead

`rem tape retire` is the explicit action. In words: the operator declares that logical
tape X is dead, that its contents are gone on purpose, that the system should stop
expecting them, and that its barcode is free for a new life.

```sh
rem tape retire RMJ101L9 --reason recycled \
    --i-understand-copies-become-unreadable
```

The first argument is the barcode of the tape, or its 32-hex tape UUID. `--reason` is
free text recorded in the audit log (for example `recycled` or `vtl-rebuilt`). The
acknowledgement flag is required. `--dry-run` reports what would be retired without
writing, and `--json` prints machine-readable output.

The command has five effects. Effects 1 to 3 and 5 are committed in one catalog
transaction; the audit event (effect 4) is appended after that transaction commits:

1. The tape row changes to `state='retired'`, a terminal state. The pool selector never
   picks it again, since any state other than `ready` is already unwritable.
2. The barcode is released (`voltag` cleared). The catalog enforces barcode uniqueness,
   so the old pairing has to end before the same sticker can be paired with a new uuid.
3. Every copy the catalog had on that tape changes from `committed` to `missing`. This
   is the accurate answer to "can I restore object A from tape X?" after X was wiped.
   Objects with copies elsewhere remain restorable. Objects with no committed copy left
   are reported as degraded, and `retire` lists them: as "would lose last committed
   copy" lines under `--dry-run`, as a warning on the error stream in text mode, and as
   the `degraded_objects` list in `--json` output (which carries no text warning). That
   list is the input re-replication needs.
4. An audit event (`TapeRetired`) is appended to the hash-chained log, recording who,
   when, which barcode, why, and how many copies were declared missing. A destructive
   declaration belongs in the permanent record. The catalog is committed first and the
   audit append follows. If the append fails, the command reports an error although the
   retirement has already taken effect and is not rolled back; check the tape's state
   (and the audit log) before retrying.
5. The retired row and its history stay in the catalog permanently. Retire is not
   delete. The purpose is to record what happened and not to rewrite it.

After `retire`, what `rem tape init RMJ101L9` does depends on what the medium carries:

- Blank media: the barcode is free, so this is a plain fresh init.
- Media still carrying the retired UUID's old bootstrap and data: retirement
  changes the catalog, but does not erase the cartridge. Re-initialization needs
  `--clobber-data` and the exact `CLOBBER RMJ101L9` confirmation, supplied on
  stdin or with `--confirm-clobber "CLOBBER RMJ101L9"`. Append admission refuses
  nonblank media as a fresh tape. Rescue any bytes you need before confirming.
- Media carrying a uuid the catalog has never seen (for example uuid Y in Scenario A):
  retiring X does not make Y writable. Init returns `NeedsExplicitRebuild` and does
  not write. Adopting Y by catalog rebuild is the default. To destroy Y instead,
  name it exactly: `--clobber-data --clobber-unknown-tape Y` with the usual
  `CLOBBER` confirmation.
- A barcode paired with some other live uuid is still refused as an anomaly. Scenario D
  stays protected. Retire covers one specific dead identity and never a whole category
  of tapes.

So in Scenario A the sequence retire, then init, works for cartridges that come back
blank; cartridges still carrying X also need confirmed clobber. A cartridge
re-initialized by another tool under a new uuid Y needs the explicit rebuild
path, or a clobber that names Y.

The flow for a recycled tape, with and without `retire`:

```text
WITHOUT RETIRE                          WITH RETIRE
─────────────────────────              ───────────────────────────────
rem tape init  → refused ✗             rem tape retire RMJ101L9 \
catalog reset  → active catalog,           --reason recycled \
                 audit log and             --i-understand-copies-become-unreadable ✓
                 journals cleared          rem tape init RMJ101L9 --clobber-data
                                           (confirm CLOBBER RMJ101L9)   ✓
                 (inputs archived)     (catalog, audit, journals:
rerun the workload                      intact; audit gained a record)
```

## 5. Retire and catalog rebuild

Retire has to be recorded in a way that survives a catalog rebuild, and this is why the
implementation is more than a single `UPDATE` statement.

Remanence treats SQLite as disposable. The durable truth lives in the append-only audit
log and the per-tape 3c journals, and the catalog can be wiped and rebuilt from them at
any time (`rem rebuild-catalog-from-journals`; the daemon also replays at startup).
Following a retired tape through a rebuild shows the problem:

1. Tape X was written once. Its journal says objects A and B were committed to X. That
   journal is history; it stays on disk and it is correct, because those objects were
   committed to X.
2. The operator retires X after wiping the medium. The catalog row is `retired` and the
   copies are `missing`.
3. Months later a rebuild runs. It clears the tapes table and replays the journals,
   which would recreate X as an ingested tape with committed copies, and the retire
   would silently be undone. The pool would still not write to X (state `ingested` is
   not writable), but restore planning would again believe copies exist on a wiped
   medium, and the next re-init or reconcile of that cartridge would produce Scenario A
   again.

Any identity-lifecycle fact held only in SQLite is lost when the catalog is rebuilt.
Remanence handles `retired` the same way it handles other operator-set state such as
`sealed`: the preserved-rows snapshot keeps the retired state, the journal replay is
told not to overwrite it, and the copy statuses are derived from it again. After replay,
every copy on a retired tape is marked `missing` again. Copy status is a consequence of
tape state and never an independent fact, so there is one thing to preserve and the two
cannot drift apart.

The journal is not deleted at retire time, because it is the authoritative history that
A and B were written there. Retire records that the present changed and does not
falsify the past. Append-only history, plus a state that overrides its present-tense
implications, keeps both true.

## 6. What retire does not do

- It does not touch the tape. There is no mount, no robot move, and no SCSI command.
  The medium may be in a drive, in a slot, or discarded. Retire is a catalog and audit
  operation about expectations.
- It does not erase anything: not the row, not the journal, not the copies' rows.
  Everything becomes history with an accurate status.
- It cannot be undone. There is no `un-retire`, and retired identities are permanent:
  the catalog refuses to adopt a bootstrap whose uuid is retired. If the wrong tape is
  retired, the medium's data is still physically intact until something overwrites it.
  `rem tape init` does overwrite it, so read what you need from the medium, read-only,
  before considering the cartridge for reuse. An undo flag would turn the retirement into
  a toggle, and every safety argument in §3 assumes it is not one.
  Retiring a tape identity that is already retired is a no-op that reports success, but
  only when the target is the original uuid. Retirement clears the barcode, so a retry
  by barcode either fails (the barcode is no longer assigned) or, once the cartridge has
  been initialized again, retires the new identity that now holds the barcode. A script
  should record the uuid it retired and retry with that.
- It does not bypass any guard for unknown media. A tape the catalog never knew, or a
  barcode paired with a different live tape, is refused as before.
- A retire that changes anything needs exclusive access to the catalog. It takes the
  local state lock, so it fails cleanly while the daemon is running; stop the daemon
  first. `--dry-run` takes no state lock and opens the catalog read-only, so it can be
  used while the daemon runs.

## 7. Day-to-day use

| Situation | Without retire | With retire |
| --- | --- | --- |
| Test setup rebuilt (virtual tape library) | `tape identity mismatch`; the only way forward is a full catalog reset, which clears the active catalog, audit log and journals (archiving the inputs) | `rem tape retire` each affected tape by uuid, then `rem tape init` for blank cartridges or ones still carrying the old uuid, then rerun. Catalog and audit history stay intact. Cartridges re-initialized under an unknown uuid need the explicit rebuild path. |
| Recycling tapes between runs | Init is refused (anomaly if the medium was altered outside Remanence, `RefuseClobber` if it holds written data) | A script calls retire, by the original uuid, and then init; a repeated retire of the same uuid is a no-op success |
| "Which objects lost all copies?" | Not answerable | `rem tape retire` (with `--dry-run` first) lists the objects that would lose their last committed copy |
| Mislabeled or foreign cartridge | Refused | Refused, unchanged |

In plain terms: the catalog can record that a tape exists, and `retire` lets it record
that a tape identity has ended. The record is audited, survives catalog rebuilds, frees
the barcode for the next use of the cartridge, and tells restore and re-replication which
copies are gone. Every refusal that protects real data stays in force.
