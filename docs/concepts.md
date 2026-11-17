# Concepts

This page is for someone new to Remanence who is about to read the guides. It introduces the
words the other pages use without stopping to explain them: what a tape file is, what it
means for data to be committed, what the tape log and the catalog are, what finalizing a tape
does, and why a tape can be fenced. The terms are introduced in the order a reader needs
them, and each builds on the ones before it. The [glossary](reference-glossary.md) has a
short entry for every term, and the [architecture overview](architecture-overview.md) is the
next step for a reader who wants the design in detail.

<!-- code-anchor: crates/remanence-library/src/handle/tape_io/mod.rs crates/remanence-api/src/write_owner/checkpoint.rs crates/remanence-state/src/tape_log/mod.rs crates/remanence-state/src/projector/tape_log_projection.rs crates/remanence-state/src/fence.rs crates/remanence-cli/src/standalone.rs crates/remanence-api/src/mount_support.rs crates/remanence-state/src/library_runtime.rs crates/remanence-state/src/library_runtime/native_floor.rs crates/remanence-cli/src/lib.rs crates/remanence-api/src/standalone_init.rs crates/remanence-state/src/projector/apply.rs crates/remanence-cli/src/top.rs crates/remanence-state/src/library_log/native_view.rs crates/remanence-state/src/library_log/fold.rs crates/remanence-state/src/rebuild_sources.rs crates/remanence-state/src/state.rs crates/remanence-state/src/tape_log/store.rs @ 4f804c6c2c9d33d333e71c79eaed2344c8835bf3 -->
## 1. How a tape behaves

Remanence's design only makes sense against the behaviour of the medium, so this page starts
there. An LTO cartridge holds one long ribbon of tape that a drive reads and writes in order, from
the beginning of the tape (**BOT**) towards its physical end. The drive writes data in
**blocks** of a fixed size, 256 KiB by default in Remanence. Between groups of blocks it can
write a **filemark**, a separator that it can find again quickly without reading the data in
between. The blocks between two filemarks form one **tape file**, and a tape holds a sequence
of tape files numbered from 0.

A tape has no free-space map and no way to rewrite a block in the middle. The drive records
where the data stops, the end of data (**EOD**). Writing at any position makes that position
the new end of data, and everything that was recorded after it can no longer be read. The
only safe place to add data is therefore the current end of data. A write that starts
anywhere else destroys what follows, which is why Remanence proves where the end of data is
before every append.

The drive also keeps written blocks in its own memory buffer for a while before they reach
the medium. A block the drive has accepted is not necessarily on the tape yet. If power fails, the
blocks still in the buffer are lost, so "the drive accepted my write" and "my data is on
tape" are different statements. Section 4 shows how Remanence turns the first into the
second.

## 2. Identity: the bootstrap and the tape UUID

When Remanence initializes a cartridge, it writes one small tape file at BOT, the
**bootstrap**. The bootstrap records the tape's block size, its parity geometry (section 6)
and its **tape UUID**, a random identifier written once. The tape UUID is the tape's
identity. It names one life of the cartridge: if the cartridge is erased and initialized
again, it gets a new tape UUID, and the old identity is over.

The outside of the cartridge carries a second name. In a library it is the **barcode**, a
sticker that the changer's scanner reads, for example `RMJ101L9`. On a standalone drive it is
the **label**, the name an operator writes on the shell and gives to `rem tape init --label`.
The barcode and the label are convenient but not trusted: stickers can be swapped, and two
archive systems can reuse the same codes. Before Remanence writes to a tape, it reads the
bootstrap and checks that the tape UUID is the one it expects. The
[tape identity explainer](tape-identity-lifecycle-explainer.md) walks through what happens
when the two names disagree.

## 3. What Remanence stores: objects, bundles and copies

An **object** is the unit Remanence stores: one REM-OBJECT archive that packages one or more
files, with their paths and checksums, into a single unit. On tape each object is exactly one
tape file. Other tape files on the same tape (the bootstrap, parity sidecars, the terminal
index) are not objects. So every object is a tape file, but not every tape file is an object.

An object can hold one file or many. `rem put` stores each input file as its own object. A
**bundle** is an object built to hold many files, usually small ones, so that they share one
tape file, one filemark and one row in the tape's index; `rem archive build` makes one, and
library-mode `rem put --stored-object` writes it. Every file inside a bundle keeps its own
name and checksum and can be restored on its own. [Object sizing](guide-object-sizing.md)
explains how to choose a bundle size.

A **copy** is one object as stored on one tape. An object written to two tapes has two
copies, and the catalog (section 5) records each one with a status. A copy is `committed`
when the tape log holds its commit record (section 4), and `missing` when the tape that held
it has been retired. Operators who want protection against the loss of a whole cartridge
write each object to two pools, so that its copies land on different tapes.

A **pool** is a named group of tapes that writes target, defined in the configuration. A
write names a pool, and Remanence chooses the tape within it. Pools carry the labels
(`copy_class`, `content_class`) that an orchestrator uses to keep redundant copies apart.

## 4. Writing: sessions, barriers, checkpoints and commits

In a library, a client writes through a **write session**: it opens a session on a pool, the
daemon loads a suitable tape into a drive, and the client streams objects into it. The
session ends when the client closes it. On a standalone drive, `rem put` runs the same
engine in process for the length of one command.

Streaming an object puts its blocks into the drive's buffer, which, as section 1 explained,
is not yet the tape. A **barrier** closes that gap. Remanence asks the drive to write zero
filemarks synchronously: the command returns only when everything in the buffer is on the
medium. Remanence then reads the drive's position (READ POSITION, the command that reports
the current block address) and checks that it matches the position the barrier reported. A
barrier therefore proves two things: the data is on the medium, and the tape is where
Remanence believes it is.

After a successful barrier, Remanence writes a **commit record** for the objects the barrier
covered and flushes it to disk. The commit record is written to the tape's log on the host
(section 5), not to the tape. An object is **committed** once its commit record is durable.
Committed is the promise Remanence makes to a client: the data is durably on tape, and the
host has a durable record of where.

One barrier and one commit record for a batch of objects is a **checkpoint**. Barriers cost
time, because the drive has to stop streaming, so the daemon does not run one per object. It
collects objects into a batch and checkpoints when the batch reaches a size, an object count
or an age limit (the `checkpoint_max_*` settings in the
[configuration reference](reference-configuration.md#daemon-required)), when the client
asks for one, and when the session closes.

After `Close` or `rem put` returns its durable receipts, the catalog's written
extent (`written_extent_lba`) and physical EOD can diverge while the session
writes its ending records. The catalog initially records the Object checkpoint;
parity closeout can advance physical EOD before `ClosedOut` and `SessionEnded`
are published. A consumer needing the settled extent waits for the session
ending, then reads the catalog again before comparing it with physical EOD.

The order matters for crash recovery. If the host loses power after the blocks were written
but before the barrier, the tape may hold the bytes, but no commit record names them, so they
are not committed and the client writes them again. If the power fails after the barrier and
before the commit record is durable, the result is the same: the bytes are on tape, but
Remanence does not count them. Only data covered by a durable commit record is committed.

Committed is not the same as protected. **Protected** means that a parity sidecar covers the
data (section 6). A commit does not wait for parity: data can be committed and not yet
protected, and parity catches up later. A commit is about the data being on tape at all;
parity is about recovering from damage to the tape years later.

<a id="5-the-tape-log-the-audit-log-and-the-catalog"></a>
## 5. The tape log, the library log and the catalog

The host keeps one **tape log** for each tape: an append-only file, `<uuid>.remtape` in the
`tapes` directory under the configured `[journal]` directory. It records, in order, the
commit records and everything else the host has decided about that tape, such as finalization
progress. The tape log is the host's authority for its tape. The command
`rem rebuild-catalog-from-journals` keeps an older word for these files; it
rebuilds the catalog from the library log and the tape logs. `rem tape import-journals` is refused with
`JournalImportDeferred: importing checkpoint journals is deferred; nothing was imported or changed`;
it imports or changes nothing.

Beside the tape logs sits the **library log**, a hash-chained, append-only record
of library decisions. Its `*.remlib` segments live in `audit.dir`; the state
directory's `authority-manifest.json` binds that directory and the tape-log
root. Retirement and label rebinding are recorded in the library log.

The **catalog** is a SQLite database (`rem-state.sqlite`) that answers questions: which
objects exist, which tapes hold their copies, which tapes in a pool are ready. The catalog is
a **projection**: its rows are derived from the tape logs and the library log, and can be
rebuilt from them at any time with `rem rebuild-catalog-from-journals`. When the two disagree,
the tape logs and the library log are the sources for the rebuilt catalog. For that reason Remanence never treats a catalog row on
its own as proof that data is on tape.

Each tape log also carries a **tape incarnation** number. It is always 1: when a cartridge is
initialized again, Remanence gives it a new tape UUID and therefore a new log, instead of
starting a second incarnation of the old one. Append receipts report the incarnation together
with the tape-log sequence number of their commit.

## 6. Parity, finalization and the sealed tape

Remanence writes Reed-Solomon parity on the same tape as the data, so that a damaged region
can be rebuilt years later from the cartridge alone. Parity is written per **parity epoch**: a
run of data blocks (16 GiB at the default geometry) followed by one **sidecar**, a tape file of
parity shards and checksums that protects them. On the current software an epoch that is not
yet full can stay open after its data is committed, and its sidecar is written later; the
[tape layout reference](reference-tape-layout.md) describes the parity geometry.

A tape is written until it is full or until an operator closes it. Closing it is
**finalization**: Remanence writes the tape's terminal index at the end of the data. The
terminal index lists every tape file on the tape and what each holds, and it is written three
times, as replicas A, B and C, separated by spacer tape files so that one damaged region is
unlikely to destroy all three. A reader with no catalog can find every object on a finalized
tape from that index. Finalization starts when a commit takes the tape past the pool's low
watermark, when the drive reports early warning near the physical end, when no queued object
fits in the space that is left, or when an operator asks for it with `rem tape finalize`. Once it starts, the tape accepts no more objects.

A **sealed** tape is one whose finalization has completed: all three replicas are on tape and
the tape log records the tape as sealed. Finalize is the operation; sealed is the state it
leaves. While the work is in progress the tape is reported as `Finalizing`, and an
interrupted finalization resumes from the last component it proved.

The facts that the three replicas share (the list of tape files, the counts and digests that
describe them) form the tape's **edition**. The **edition digest** is a SHA-256 over those
shared facts. Replicas of the same edition agree on it, and a full verification
(`rem tape verify-index`) records the edition digest it verified, so that a later check can
tell whether it looked at the same index.

## 7. WORM media

**WORM** (write once, read many) cartridges cannot be overwritten.
Remanence writes objects only to rewritable cartridges. After a write is interrupted it replaces the
unfinished end of the tape, and a WORM cartridge forbids that. Tapes written elsewhere on WORM
can still be read. An interrupted write includes a cancelled write or a disconnected client,
not only a crash. Reading, adoption and restore of supported tape formats on WORM are designed
for but untested on a physical WORM cartridge.

Buy rewritable cartridges, not WORM. Both library and standalone `tape init` refuse WORM
and media whose WORM state cannot be established, before any media write, even on a blank
cartridge. Ordinary object writes also require positive evidence of rewritable media.
[WORM writing is on the public roadmap](https://github.com/archivetechie/remanence/issues/70).

The drive's MODE SENSE header supplies the cartridge's medium type. Object-write admission,
tail recovery, finalization and init ask the Medium Configuration page for its WORM-mode bit
only when that header is unspecified or unrecognized. Plain settings reads used by mounting,
reading, verification, append re-proof and the all-bays check use the header alone and do not
request that page. `rem diagnostics capture` reads both signals, even for a typed header,
and reports a conflict if they disagree. A conflict is treated as WORM; neither signal can
overrule the other's positive WORM evidence.

## 8. Libraries, standalone drives and home mode

Remanence runs in two arrangements. In a **library** (a changer with drives), the long-running
daemon `rem-daemon` owns the changer and the drives, and `rem` talks to it over gRPC. With a
**standalone drive** (one drive and no changer, cartridges loaded by hand), `rem` operates the
drive directly, and no daemon runs.

**Home mode** is the simplest standalone set-up. When the host has no system configuration at
`/etc/rem/config.toml`, `rem setup` saves the chosen drive's serial in a configuration under
the user's state directory, and from then on `rem put`, `rem get` and the `rem tape` commands
operate that drive without further flags. [Using a standalone LTO
drive](guide-standalone-drive.md) walks through it, and the [CLI
reference](reference-cli.md#standalone-drives) lists which commands go where.

## 9. Fences

When an operation on a tape or drive ends in a state that Remanence cannot prove, for example
a load whose outcome is unknown, Remanence does not guess. It records
a **fence**: a durable refusal against that tape or drive, written to the library log and the
catalog, that survives restarts. Fenced media is refused for writes and initialization
until the cause is resolved. Read sessions and motion depend on the cause; other tapes
and drives stay available. Every fence has a **quarantine id**, and the `rem tape quarantine` commands list,
show and release fences by that id; in these pages "quarantine" and "fence" name the same
record. A tape-I/O fence release records the acknowledgement in the library log.

Fences can protect a tape or a drive.

- A **tape-I/O fence** (quarantine id beginning `library:`) records a
  partial batch, a transfer error, an uncertain step of finalization, or a tape log
  that ends before the point the catalog has already projected.
  `rem catalog tapes --json`, `rem top` and `rem daemon health --json` show these fences.
  In a library, a tape fenced only by `append_admission_refused` stays readable
  and closed for writing; robot motion, eviction and idle unload proceed. A tape fenced only by
  `session_suffix_contradiction` allows robot motion and eviction, and idle unload
  once the running service has parked it (a cartridge already seated when the
  service starts stays until moved), but refuses reads and writing.
  Neither is released by `rem tape quarantine release`. Use `rem tape supersede-tail`
  where recovery applies; it refuses a contradicting recorded session ending or a
  state that shows the tape's last session closed. Otherwise withdraw the tape with
  `rem tape retire` or start from a newer copy of the state directory. Retirement
  refuses while parity closeout is due. For a releasable cause, an operator
  investigates, verifies the tape and drive, and uses
  `rem tape quarantine release <ID> --after-settled-inventory --ack <TEXT>`.
  A present, noncontradicted tape log behind the catalog's projection
  (`native_log_behind_projection`) can be restored from an exact copy, or the
  catalog can be rebuilt, before release with `--force`. A running daemon keeps
  the tape log it read at startup: after restoring a log, restart the daemon, or
  forced release refuses with
  `the tape log on disk changed after this process read it`. A contradicted or
  absent log still refuses forced release; restore the exact log or retire the
  tape. A floor conflict means the library log's own records disagree about the
  tape's log; restoring the tape log cannot cure it, so retire the tape.
  Rebuilding the catalog cures none of these three. A log still shorter than, or
  different from, what the catalog recorded also refuses forced release, until
  the exact log is restored or the catalog is rebuilt. Retiring a tape whose log
  is absent or shorter than the catalog's record is refused until the catalog is
  rebuilt: rebuild first (`rem rebuild-catalog-from-journals`, with the daemon
  stopped), then retire. If the rebuild refuses with
  `RebuildWouldDropMarkEvidence:`, no fence records the catalog's mark yet: open
  the state once (start and stop the daemon; on a standalone host, run
  `rem tape quarantine list`), then rebuild.
  On a standalone host without `/etc/rem/config.toml`, the release form uses the
  home configuration. `rem rebuild-catalog-from-journals` and `rem tape retire`
  do not use the home configuration: on a standalone host without
  `/etc/rem/config.toml`, pass them `--config` with its path. On a home host, or
  with `--device`, the form without `--after-settled-inventory` is standalone
  release: with no daemon listening on the selected configuration's socket it is
  not a release route for these tape fences, and with one listening it hands the
  release to the daemon, which treats it as the release form without `--force`.
  Otherwise it is refused with
  `tape quarantine release requires --after-settled-inventory`. See the
  [CLI reference](reference-cli.md#rem-tape-quarantine-standalone).
  Under that cause, `rem get`, `rem tape identify`,
  `rem tape verify` and `rem tape restore` refuse on a standalone drive; only
  `rem tape restore --scan` proceeds, walking from the tape's start. Contradicted
  tape admission refuses every read, including `--scan`. Other fence causes leave
  standalone reads available.
- A **standalone drive hold** (`library:<n>`) records write uncertainty on the
  drive independently of its cartridge's tape fence. A write, finalize or
  tail replacement on a standalone drive that ends in a write fence also
  holds the drive, including when the tape fence itself cannot be recorded
  and when a read during the write contradicts what was recorded; an
  append-admission refusal (`append_admission_refused`) fences only the tape.
  It refuses writes, initialization and the four catalogue changes
  (adopt, drive-form assign-pool, rebind and set-generation), including on other
  cartridges. Reads and `rem tape release` do not consult the hold. The
  standalone quarantine list includes `standalone_holds`; release the hold with
  `rem tape quarantine release library:<n> --device PATH --allow-device <serial> --ack TEXT`.
  Tape fences remain active and keep their own release requirements.
  An uncertain initialization write holds the drive, returns the write's error
  and creates no tape fence; its detail names the planned tape UUID. The next
  init refuses before RESERVE or preparation. If the hold cannot be recorded,
  the refusal is
  `cannot durably record standalone write uncertainty for <uuid> at <endpoint> (<reason>): <detail>; the drive hold was not recorded: <error>; motion is fenced and recovery is required`,
  counted as `StandaloneFenceUnrecorded`; motion stops for the rest of the process.
- A **media-readiness fence** (quarantine id beginning `mrq-`) records a readiness wait, the
  wait for a drive to become ready after a load, that was interrupted or ended without a clear
  answer. A wait that was still progressing (loading, becoming ready, busy) is resumable: the
  daemon resumes it itself, and `rem tape wait-ready --resume` continues it. A wait that ended
  in an unknown state, a terminal error or a reservation conflict needs an operator release
  after the inventory has been checked. `rem tape quarantine list` shows these fences.

Two things that look similar are not fences. The **eject guard** is the medium-removal
prevention that Remanence sets on a drive while a command runs, so that the cartridge cannot
be ejected mid-operation; an interrupted command can leave it set, and `rem tape release`
clears it on a standalone drive. A tape log can also carry a mark on the
host that blocks appends to that tape after the host could not prove a log write;
[troubleshooting](guide-troubleshooting.md) covers this refusal.

## In plain terms

A tape is a long notebook that can only be written at the end, and that loses everything
after any line you write over. Remanence writes objects at the end, waits until the drive
confirms they are really on the tape, and then notes the fact in a ledger it keeps on the host
for that tape. Only what the ledger records counts as stored. The catalog is a searchable copy
of the ledgers, which can always be rebuilt from them. Parity is a later repair kit, the
terminal index is the table of contents written when the notebook is closed, and a fence is a
note on the cover that says "something went wrong here; check before you write again".
