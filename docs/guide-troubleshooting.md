# Troubleshooting

This page lists the failure modes Remanence reports deliberately, what each
message means, and what to do about it. When the software cannot prove what
state the hardware is in, it stops and says so rather than guessing, so many
things that look like an outage are a fence (a recorded block on a tape or
drive) doing its job.

## WORM and unknown-media refusals

```text
ordinary Object writes require rewritable media; a WORM tape cannot replace an interrupted uncommitted Object tail
```

The loaded cartridge reports WORM (write once, read many). Use a compatible rewritable
cartridge. Recovery needs to replace an unfinished write, including one interrupted by
cancellation or a disconnected client; WORM media forbids that. Reading and adoption are
designed for WORM but untested on a physical WORM cartridge. See [Concepts §7](concepts.md#7-worm-media).

```text
ordinary Object writes require media positively identified as rewritable; the loaded tape's WORM state is unknown
```

The drive supplies neither a recognized header medium type nor usable WORM-mode evidence
from the Medium Configuration page. Remanence refuses the write because it cannot establish
that the cartridge is rewritable. Send the failing command and its output with a
`rem diagnostics capture --out diagnostics.tar` archive; the capture records both signals
and their combined verdict.

```text
tape init refused before any media write: <reason>
```

Both library and standalone init use the same media gate and give one of the reasons above.
A blank WORM cartridge is refused too, and clobber or force flags do not override this check.
Use rewritable media for the WORM refusal; send a diagnostics capture for an unknown-state
refusal. No media write occurs on either refusal.

<!-- code-anchor: crates/remanence-state/src/authority_layout.rs crates/remanence-state/src/state.rs crates/remanence-cli/src/lib.rs crates/remanence-state/src/projector/index.rs crates/remanence-state/src/state/initialization.rs crates/remanence-api/src/tape_identity.rs crates/remanence-api/src/pool_write/model.rs crates/remanence-api/src/mount_support.rs crates/remanence-api/src/daemon_catalog_services.rs crates/remanence-state/src/library_runtime.rs crates/remanence-cli/src/standalone.rs crates/remanence-state/src/authority_manifest.rs crates/remanence-state/src/projector/tape_log_projection.rs crates/remanence-state/src/state/authority_reset.rs @ 35fc920573f5b874c26c0dd9e2c443d43451bca5 -->
## Authority-layout refusals and declared reset

A state directory written by an earlier release, with library or tape authority
and no `authority-manifest.json`, is refused by daemon startup, catalog rebuild
and direct state opens. Their refusal begins:

```text
Legacy or unbound authority requires the declared state reset (`rem reset-catalog --config <config>`); no authority importer is available
```

Commands using the runtime configuration loader can refuse earlier at the layout
marker or catalog schema. In a current authority directory, daemon startup,
catalog rebuild and runtime commands refuse catalog schemas 1 through 25. The
layout/schema refusal contains this guidance:

```text
This state directory predates the current tape-log format. A declared state reset and reinitialization of its test tapes is required. Run rem reset-catalog --config <config> with the existing explicit reset confirmation. No importer is available.
```

`rem list` checks the catalog without opening state. `setup`, `doctor`, `drives`
and `diagnostics` can warn about a refused catalog while continuing discovery;
`doctor` exits 1 when its catalog check fails.

For state from an earlier release, stop the daemon. The following command
destroys active host authority and starts an empty catalog:

```sh
rem reset-catalog --config <PATH> --i-understand-this-erases-the-catalog
```

It archives the prior library-log (`audit`) and tape-log (`journals`)
directories under `state_dir/reset-archives/reset-<number>/`, along with
`catalog.sqlite` and its `-wal`/`-shm` files when present, `authority-format`,
`authority-manifest.json`, `config.snapshot` and any prior receipt as
`previous-reset-receipt.json`. The active receipt is
`state_dir/authority-reset-receipt.json`. Configuration and calibration
generation allocation survive; the reset imports no old state.

Reinitialize each approved scratch tape before returning it to write service.
For a library tape whose existing BOT UUID is absent from the reset catalog,
init prints `observed BOT predecessor has no typed identity; no Bootstrap write admitted`
and writes nothing. It prints no `next:` line. Keep the data by adopting the tape
with `rem-debug tape adopt-bootstrap`. To destroy the approved scratch tape,
name its exact observed tape UUID and barcode in the confirmed clobber command:

```sh
rem tape init <LABEL> --config <PATH> --clobber-data --clobber-unknown-tape <BOT-UUID> --confirm-clobber "CLOBBER <LABEL>"
```

Select the approved geometry with `--block-size <BYTES>` and
`--parity <none|default>`; a fresh initialization defaults to no parity.
Initialization assigns a new tape UUID and incarnation 1. It creates no log
until the first write-session open. That open writes the one-block BOT file and
its filemark, takes a synchronous barrier, then closes an `Initialized` record
before writing an Object. Both parity modes use this order. An idempotent init of an already
registered tape does not assign a new identity. `--preserve-tape-voltag` is
accepted but refused: preflight prints `declared reset does not import preserved catalog identities; reinitialize or adopt physical media afterward`,
and reset, given a well-formed `--expected-preflight-token` (64 lowercase hexadecimal characters), prints `declared reset cannot restore SQL tape identities; reinitialize or adopt afterward`.
A scoped erase reset requires the matching token from
`rem-debug catalog reset-preflight`; without it the CLI refuses with
`scoped catalog reset requires --expected-preflight-token from reset-preflight`.
See the [CLI reference](reference-cli.md#destructive-state-reset).

An earlier layout requires the declared reset. A restored state at different
paths needs its recorded paths instead. When the state directory, the tape-log
root or `audit.dir` differs from `authority-manifest.json`, daemon startup,
rebuild and direct state opens refuse with
`authority manifest format or configured root binding differs; declared reset required`.
A runtime-loader command with a different `journal.dir` can instead print
`This state directory predates the current tape-log format.` because the
marker binds that root. Reset also refuses this mismatch, beginning
`scoped reset cannot establish current typed inventory:`, and erases nothing.
Restore the recorded paths or point the configuration at them; keep those paths
on a replacement server. An interrupted reset
reports `durable fence {path} requires an exact reset rerun`; rerun the same
reset request rather than removing its fence.

In plain terms, a reset discards the old host authority. It does not make an
already written cartridge safe to append; the approved test tapes must be
initialized again.

## Refused physical reconciliation

The `ReconcileTape` RPC is refused with
`ReconcileTapeDeferred: reconciling a tape from a physical scan is deferred; no tape was loaded or scanned`.
It loads, scans and changes nothing. The refusal is counted in the library log;
there is no CLI client for this RPC.

## Interrupted initialization of a known tape

Init writes a new start-of-tape before the catalog records the replacement. If
it is interrupted between those steps, the old identity remains live in the
catalog while the cartridge carries the new UUID. Mounting it under the old
identity and writing refuse with `tape identity mismatch: expected ...`.
Another init refuses with
`observed BOT predecessor has no typed identity; no Bootstrap write admitted`.
No Bootstrap write is admitted on that retry.

With the daemon stopped, retire the old identity to release its barcode. This marks
its recorded copies missing, so identify the old catalog UUID before running.
Use the configuration for the affected catalog as `<PATH>`:

```sh
rem tape retire <OLD-UUID> --config <PATH> --reason "interrupted initialization" --i-understand-copies-become-unreadable
```

For a library cartridge, adopt the new identity with `rem-debug tape adopt-bootstrap`,
or destroy it with `rem tape init <LABEL> --clobber-data --clobber-unknown-tape <NEW-UUID>`
and the exact `CLOBBER <LABEL>` confirmation. For a standalone cartridge, adopt it
with `rem tape adopt`, or destroy it with
`rem tape init --label <LABEL> --clobber-data --clobber-unknown-tape <NEW-UUID>`
and the same confirmation. Retirement alone does not admit writes to the unknown
new identity.

## Damaged log tails and projection rebuild

To see the last valid closed boundary and any damage in a tape log:

```sh
rem-debug tape-log inspect --config <PATH> --tape-uuid <UUID>
```

Inspection returns JSON with `through_sequence`, `valid_bytes`, `original_bytes`
and `damage`. Sequence and byte counts are decimal strings.
`rem-debug tape-log repair-tail` prints
`TailRepairDeferred: repairing a tape-log tail is deferred; nothing was repaired or changed`.
It refuses before reading its flags or opening state, and repairs or changes
nothing. Stop the daemon and restore the exact tape log from a preserved copy.
If the logs are intact and only the catalog projection is damaged, use the
rebuild route below. Catalog rebuild does not repair a damaged tape log.

For a lost SQLite catalog, stop the daemon and run the offline rebuild below
before starting the daemon. Daemon startup does not check for a tape log that
the probe drops because of damage or a lone ambiguity marker. If the catalog
is damaged, first move the file named by
`index.sqlite_path` and its `-wal`/`-shm` sidecars aside without deleting them.
Then rebuild:

```sh
rem rebuild-catalog-from-journals --config <PATH>
```

This reads the library log (`*.remlib` segments in `audit.dir`) and the tape
logs, starting from an empty catalog. Nothing is carried over from the old
catalog, and the audit log is not a rebuild source. Rebuild requires
`authority-format` and `authority-manifest.json` and preserves both logs.
It does not clear ambiguity or repair fences or media. Cached wrap maps are
evicted and volumes become uncalibrated; calibration generation allocation
advances. A short log can refuse with `RebuildWouldDropMarkEvidence:` when
rebuild would discard the catalog's mark evidence.

`RebuildWouldDropLibraryRecords:` means the catalog holds library records that
the supplied log does not. The refusal lists the tape-standing changes and
counts the other records. Put back the library log this catalog was built from
and run again. If it no longer exists, keep the state directory and catalog as
they are; they hold the only copy of those records. The declared reset erases
them and is the only way to run sooner. Moving the catalog aside removes this
witness; there is no override for the refusal.

`RebuildWouldDropTapeLog:` means a damaged tape log or a tape with only an
ambiguity marker would disappear from the rebuilt catalog. Restore the named
tape log from a copy and run again, or keep the state directory as it is. For a
marker-only tape, follow the refusal's `rem-debug library-log recover
--confirm-clear-ambiguity` command with the named configuration, then rebuild.
Neither refusal clears catalog rows or changes either log.

With no catalog witness, a successful rebuild prints the library record and
time its tape standing reflects. Nothing on this host establishes that the
library log is the newest copy.

Catalog rebuild regenerates the queryable view of intact host records; it does
not change the cartridge or repair a tape log.

The library log (`audit.dir`) and the tape logs (`journal.dir`) are the records
that cannot be rebuilt. Both grow only at the end. Copy them to another machine
often, a minute or two apart on a working archive, and keep dated generations.
Keep the state directory's `authority-format` and `authority-manifest.json`
with them, together with the configuration. The catalog is rebuilt from the two
logs. The tape-log, library-log, catalog and state-directory paths are configured
independently, so check that a backup of "the state directory" includes each.

Restore both logs, both authority files and the configuration together, each
from its newest copy, to the configured paths. An older copy knows nothing
decided after it: a tape withdrawn or fenced since then is not withdrawn or
fenced in the restored state, and a tape first provisioned since then is unknown
to it. Before starting the service on restored state, list the tapes used,
withdrawn or fenced since the copy's date.

In a library, each tape the restored state knows and a pool has written
since the copy is checked on the pool's first write to it under the restored
state, and data there that the restored state does not account for can
refuse that write. If admission reports `TailBeyondKnownState`, the tape
is closed for writing, and a pool request can try another eligible tape.
The `append_admission_refused` fence itself permits read sessions, but
contradicted tape admission or another blocking fence prevents
them. A contradiction with the tape's recorded ending also blocks read
sessions. Before starting the service on restored state, each pool should
have a writable tape that the restored state knows and that the pool has not
written since the copy. The restored catalog does not list a tape's data
written since the copy. Do not discard that data unless its objects are
known to be unneeded.
If the restored state shows no open session on the tape (its last session
closed, or none opened since it was initialized here), that data cannot be
discarded in this release: keep the tape closed for writing, withdraw it
with `rem tape retire`, or start from a newer copy of the state directory.

On a parity pool, if the restored tape log still owes closeout, retirement
refuses with `CloseoutRequired`. Closeout stays refused under either
`append_admission_refused` or `session_suffix_contradiction`.
For that tape, use `rem tape supersede-tail` only where discard applies, or
start from a newer copy of the state directory.

Populated state without `authority-manifest.json` refuses with
`Legacy or unbound authority requires the declared state reset`. With a manifest
but no `authority-format`, layout validation refuses with
`This state directory predates the current tape-log format.`. Restore the
missing authority files with the logs before starting the service.

<!-- code-anchor: crates/remanence-library/src/error.rs crates/remanence-cli/src/lib.rs crates/remanence-scsi/src/error.rs @ 6ed03ef0 -->
## Discovery finds no libraries

`rem libraries` reporting `no tape libraries reachable on this host` has
three usual causes, and the warning list tells you which one you have.

**Every probe returned EPERM.** The kernel SCSI command filter refuses
READ ELEMENT STATUS from callers without `CAP_SYS_RAWIO`, even when the
device node is readable. The CLI detects this pattern and prints the fix:

```text
warnings (2):
  - scsi error on /dev/sg4: READ ELEMENT STATUS: SG_IO ioctl failed: Operation not permitted (os error 1)
  ...
hint: every SCSI probe returned EPERM. This is the kernel SCSI command
      filter refusing READ ELEMENT STATUS without CAP_SYS_RAWIO. Try:
          sudo setcap cap_sys_rawio+ep <path-to-rem>
```

Two separate gates must both be open: your user needs to be in the `tape`
group (that gates `open("/dev/sgN")`), and the binary needs
`CAP_SYS_RAWIO` (that gates the filtered SG_IO opcodes). `INQUIRY` is
whitelisted by the kernel, so `sg_inq` working proves nothing about the
second gate. (`sg_inq` comes from `sg3-utils` on Debian/Ubuntu and `sg3_utils`
on Fedora; Remanence itself does not use those tools.)

**The capability vanished after a rebuild.** File capabilities live in
xattrs on the binary's inode; `cargo build` replacing the binary drops
them. After `cargo build --release`, rerun
`sudo setcap cap_sys_rawio+ep target/release/rem`, and the same for
`rem-debug` or `rem-daemon` if you run them directly. `make rem-dev` rebuilds
and re-runs `setcap` in one step, but only for the debug binary
`target/debug/rem`. Run the binary directly rather than through `cargo run`.
`rem doctor` names the executable whose capability it checked.
For the daemon under systemd, grant the capability in the unit instead:
`AmbientCapabilities=CAP_SYS_RAWIO` plus
`CapabilityBoundingSet=CAP_SYS_RAWIO`.

**could not enumerate /dev/sg\*.** Directory-level permission problem;
check that `/dev/sg*` nodes exist at all (no HBA, no VTL, or the `sg`
kernel module is not loaded; `sudo modprobe sg` loads it).

<!-- code-anchor: crates/remanence-cli/src/standalone.rs crates/remanence-library/src/discovery.rs crates/remanence-api/src/mount_support.rs @ 01bda1baf13dd4b2060e18ef0959ea8244491e66 -->
## A standalone drive is missing or read-only

Start with:

```sh
rem drives --json
rem doctor --json
```

The drive you plan to write must appear with `kind: "standalone"`,
`ownership: "standalone"`, a `/dev/sgN` path, and a serial. The common
standalone refusals are:

| Message or observation | Meaning and action |
|---|---|
| No drive endpoint appears | Apply the same `tape`-group and `CAP_SYS_RAWIO` fixes described above. Check the cable, HBA, `/sys/class/scsi_generic`, and kernel log. |
| `--device requires explicit --allow-device` | Repeat the serial from `rem drives --json`: `--device /dev/sgN --allow-device SERIAL`. |
| `--allow-device does not authorize this discovered drive` | The selected path now resolves to another serial, or the allow value is wrong. Rediscover; do not broaden the allowlist blindly. |
| `device is not an unattached drive; library operations go through the daemon` | A changer claims this drive. Configure and use the library path instead. |
| `ownership unknown` | Discovery could not prove that no changer owns the drive. Fix failed tape/changer probes. Do not join `disk`: non-tape devices are skipped by sysfs type, and disk access does not prove drive ownership. If this is a known standalone drive on a partially discoverable host, add its exact serial under `[[standalone_drives]]`. |
| `unresolved changer bays prohibit device writes` | A reachable changer has a bay whose drive identity cannot be resolved. Fix that mapping; a standalone serial pin does not override it. |
| `only permitted for read verbs` with `--allow-unserialized-device` | A drive without a readable serial has only an ephemeral identity. Reads can be explicitly allowed; writes cannot. |
| State volume is untrusted | Put home-mode state on a local persistent filesystem. `--allow-untrusted-state-volume` is an explicit development override, not a normal archive setup. |
| `journal volume rejected` | Put `journal.dir` on a local ext3, ext4, XFS, btrfs, ZFS, f2fs or bcachefs filesystem with write barriers on. The detail names a filesystem, mount-option or ZFS flush-switch refusal; if it cannot determine the filesystem, check that `/proc` is mounted. `rem doctor` shows the blocking `journal volume` row and, on accepted ZFS volumes, an advisory `journal sync` row for the dataset's sync property. Neither `--allow-untrusted-state-volume` nor `journal.require_trusted_volume = false` changes the journal check. Remanence makes its records durable with fsync, as a database does; it cannot test whether a disk keeps what it confirms. If a power cut loses the last journal records, nothing on tape is lost or overwritten. The next write to that tape can stop with `TailBeyondKnownState`. Recoverable files can be copied out with `rem tape restore --scan --out rescued` on a standalone drive. After that refusal, if this state shows no open session on the tape (its last session closed, or none opened since it was initialized here), `supersede-tail` refuses discard in this release: keep the tape closed for writing, start from a newer copy of the state directory that accounts for the data, or withdraw the tape by the [`rem tape retire`](reference-cli.md#rem-tape-retire) procedure (in home mode, pass this configuration's path with `--config`; parity closeout or a pending finalization can block retirement). Otherwise discard can proceed if recovery checks pass. In standalone home mode use `rem tape supersede-tail --acknowledge-tape LABEL`; for an explicit drive add `--device PATH --allow-device SERIAL`. In a library use `rem tape supersede-tail --tape-uuid UUID --ack-tape-uuid UUID`. Its preview lists the objects and other findings it detected (a trailing torn fragment has no separate entry); confirm only after accounting for the whole suffix, then rerun an interrupted `put` whose sources still exist. A UPS, or a disk with power-loss protection, removes this power-cut exposure. Never edit `queue/write_cache` or mount with `nobarrier`, `barrier=0`, f2fs `fsync_mode=nobarrier` or `checkpoint=disable` (including `checkpoint=disable:…`), or bcachefs `journal_flush_disabled`. On ZFS keep `zfs_nocacheflush` and `zil_nocacheflush` at `0` and the dataset's `sync=standard`. |
| Cannot write `/run/lock/rem`, especially after reboot | Create it with `sudo install -d -o root -g tape -m 0770 /run/lock/rem`. Put `d /run/lock/rem 0770 root tape -` in `/etc/tmpfiles.d/rem.conf` so boot recreates it. |
| `TailBeyondKnownState` | Tape holds data this state does not record, often after restoring an older copy of the state directory. It stays readable and closed for writing. To keep the data, start from a newer copy of the state directory, or copy files out of this standalone drive with `rem tape restore --scan --out rescued`. Do not delete journals or discard data unless its objects are known to be unneeded. If this state shows no open session on the tape (its last session closed, or none opened since it was initialized here), discard is refused in this release. In other cases, `rem tape supersede-tail --acknowledge-tape <label\|uuid>` (home mode; for an explicit device add `--device <path> --allow-device <serial>`) can discard it if the recovery checks pass and you confirm the preview. |
| `holds … data at the beginning` | Nonblank media needs `rem tape init --label LABEL --generation <yours> --clobber-data`, then `CLOBBER LABEL`. For non-interactive use, pass `--confirm-clobber "CLOBBER LABEL"`. This also applies after retirement. Clobber also erases a Remanence tape whose files are still in your catalog; run `rem list --tape LABEL` first. It does not bypass write protection, partition, label/identity-swap or cross-pool checks. |
| `has 2 partitions` | LTFS media needs repartitioning, which erases the cartridge. Install `mt-st` with `sudo apt install mt-st` (Debian/Ubuntu) or `sudo dnf install mt-st` (Fedora). Find `nstN` with `ls /sys/class/scsi_generic/sgN/device/scsi_tape/` using the drive's sg name. The listing includes the node name; use `/dev/<the listed nstN>` (for example `/dev/nst0`). Run `sudo mt -f /dev/nstN stsetoptions can-partitions`, then `mt -f /dev/nstN mkpartition 0`. Re-run `rem tape init --label LABEL --generation <yours>`; add `--clobber-data` only if it still reports data at BOT. See [step 2](guide-standalone-drive.md#2-initialize-a-cartridge). |
| Active tape-I/O quarantine | Run `rem tape quarantine list`. For `native_log_behind_projection`, only `rem tape restore --scan --out rescued` proceeds; `get`, `tape identify`, `tape verify` and ordinary `tape restore` refuse. Contradicted tape admission refuses every read, including `--scan`. Other fence causes leave standalone reads available. The standalone release form has no `--force`; see [Tape-I/O fences](#tape-io-fences). Release does not bypass the physical append check. |
| Standalone drive hold (`DriveFenced`) | Inspect `standalone_holds` with `rem tape quarantine list --device PATH --allow-device <serial>`. After checking the drive, run `rem tape quarantine release library:<n> --device PATH --allow-device <serial> --ack "drive checked"`. This releases only the hold and prints the tape fences still active. Reads and `rem tape release` do not consult the hold. |
| Tape full / capacity refusal | Written receipts remain committed. Finalize, eject, initialize the next cartridge in the same pool and rerun put. Unchanged committed files skip. An uncertain I/O failure needs quarantine investigation first. |
| Ran with sudo / home mode refuses root | Never run rem with sudo. Use your normal account with the `tape` group and a capability on the binary. Check which state directory the earlier command used; preserve it before correcting ownership or choosing a config. Root's home state is not your user's state. |

The full setup and first-tape sequence is in [Using a standalone LTO
drive](guide-standalone-drive.md).

<!-- code-anchor: crates/remanence-library/src/handle/tape_io/readiness.rs crates/remanence-cli/src/lib.rs crates/remanence-api/src/mount_support.rs crates/remanence-state/src/library_runtime.rs crates/remanence-state/src/library_runtime/drive_fence.rs crates/remanence-state/src/library_runtime/native_floor.rs crates/remanence-api/src/write_owner/cleaning_source.rs crates/remanence-api/src/mount.rs crates/remanence-cli/src/standalone.rs crates/remanence-api/src/write_owner/cleaning.rs crates/remanence-api/src/standalone_init.rs crates/remanence-api/src/write_owner/terminal_finalize.rs crates/remanence-state/src/library_runtime/terminal.rs crates/remanence-state/src/library_log/native_view.rs crates/remanence-state/src/library_log/fold.rs crates/remanence-state/src/rebuild_sources.rs crates/remanence-state/src/state.rs crates/remanence-state/src/tape_log/store.rs crates/remanence-state/src/projector/tape_log_projection.rs crates/remanence-state/src/projector/apply.rs crates/remanence-cli/src/top.rs @ 4f804c6c2c9d33d333e71c79eaed2344c8835bf3 -->
## Tape-I/O fences

The daemon starts with tape-scoped and drive-scoped tape-I/O fences. Each fence
produces a startup WARN with its quarantine id, UUID, barcode or endpoint,
reason, and release command. `rem top`, `rem catalog tapes --json`, and
`Daemon.Health` list active fences and their count. A fence with missing or
unparseable scope still refuses startup.

Two tape-I/O recovery fences have different admission rules:

- `append_admission_refused` records a write this state could not admit at the
  tape's end: data it does not record, a missing bootstrap filemark or an
  unproved tail. Read sessions, robot motion, eviction and idle unload proceed.
  Writing, closeout, finalization and terminal-index reads (`rem tape inventory`,
  `rem tape verify-index`) stay refused.
- `session_suffix_contradiction` records a tape that contradicts its recorded
  session ending. Robot motion and eviction proceed, and a cartridge the running
  service parks idle-unloads; read sessions and writing stay refused.

These permissions apply only when all matching fences allow the operation.
Idle dismount and readiness waits can move a cartridge whose only fences are
these two causes. Startup readiness reconciliation, and the parking and idle
unload of a cartridge already seated when the service starts, proceed only when
the only fence is `append_admission_refused`. Other fenced tapes and drives are left
untouched by checkpoint replay and projection, append recovery, manual-finalize
relaunch, terminal recovery, readiness reconciliation and automatic waits,
session closeout handling, and idle dismount. Other media remain available.

Neither recovery fence is released by `rem tape quarantine release`.
`rem tape supersede-tail` recovers the tail where it can: it refuses a tape
that contradicts its recorded session ending, and refuses discard when this
state shows no open session on the tape (its last session closed, or none opened
since it was initialized here). Otherwise withdraw the tape with
`rem tape retire` or start from a newer copy of the state directory. Retirement
refuses with `CloseoutRequired` while parity closeout is due; closeout itself
stays refused under these fences.

For a releasable cause, investigate the recorded evidence and verify the medium
and drive before release:

```sh
rem tape quarantine release <ID> --after-settled-inventory --ack "what was checked"
```

For a present, noncontradicted `native_log_behind_projection` tape log, restore
the exact log or rebuild the catalog with `rem rebuild-catalog-from-journals`,
then release with
`rem tape quarantine release <ID> --after-settled-inventory --ack <TEXT> --force`.
Without `--force`, release is refused with
`native_log_behind_projection fence requires --force:`.
A running daemon keeps the tape log it read at startup: after restoring a log,
restart the daemon, or forced release refuses with
`the tape log on disk changed after this process read it`. A contradicted or
absent log still refuses forced release; restore the exact log or retire the
tape. A floor conflict means the library log's own records disagree about the
tape's log; restoring the tape log cannot cure it, so retire the tape.
Rebuilding the catalog cures none of these three. A log still shorter than, or
different from, what the catalog recorded also refuses forced release, until the
exact log is restored or the catalog is rebuilt. Retiring a tape whose log is
absent or shorter than the catalog's record is refused until the catalog is
rebuilt: rebuild first (`rem rebuild-catalog-from-journals`, with the daemon
stopped), then retire. If the rebuild refuses with
`RebuildWouldDropMarkEvidence:`, no fence records the catalog's mark yet: open
the state once (start and stop the daemon; on a standalone host, run
`rem tape quarantine list`), then rebuild. On a standalone host without
`/etc/rem/config.toml`, this release form uses the home configuration.
`rem rebuild-catalog-from-journals` and `rem tape retire` do not use the home
configuration: on a standalone host without `/etc/rem/config.toml`, pass them
`--config` with its path. On a home host, or with `--device`, the bare `--ack`
form is standalone release: with no daemon listening on the selected
configuration's socket it is not a release route for these tape fences, and with
one listening it hands the release to the daemon, which treats it as the release
form without `--force`. Otherwise it is refused with
`tape quarantine release requires --after-settled-inventory`. See
[release routing](reference-cli.md#rem-tape-quarantine-standalone).

The library command uses the configured daemon socket when it is listening.
An absent listener permits offline release,
which still requires the exclusive state lock. RPC and authentication failures
do not fall back offline. A tape-fence release appends a `FenceReleased` record
to the library log before clearing the catalog row. A crash between these
steps leaves the fence active; retry or startup completes the release from the
same evidence. The tape becomes admissible without restarting, subject
to the normal identity, readiness and append checks.

A drive fence refuses a write, mount or finalize that needs its bay with
`DriveFenced: <endpoint> library:<fence> <cause>`. That refusal is counted in the
library log. Cleaning raises bay-scoped drive fences; failed standalone writes
also raise drive fences, scoped to the standalone drive.
If the only usable bays are drive-fenced or busy and at least one is fenced,
a library mount refuses with `DriveFenced` immediately; it does not wait for a
busy bay. Only the cleaning run that raised a bay fence releases it.

A failed cleaning run gives the drive back when no cartridge movement was
admitted or a fresh inventory shows the drive bay empty. If the cleaning
cartridge may still be inside and the run cannot prove otherwise, the drive
stays out of data work and the `cleaning-needs-operator` alarm stays raised.
This can follow a failure to open the drive, read its alerts, move the cartridge
back, or read a fresh inventory. A new manual clean on that drive refuses with
`drive is already fenced`. `rem tape quarantine release` refuses a bay fence with
a message containing `fence is not tape-scoped`. There is no command to release a
retained bay fence; the declared reset,
`rem reset-catalog --config <PATH> --i-understand-this-erases-the-catalog`
([declared reset](#authority-layout-refusals-and-declared-reset)), is the only
route, and it erases the catalog.

The refusal `drive discovery cannot preserve the required identity alarm; motion is fenced`
occurs when discovery finds one serial twice in an inventory, two active
identities for one serial, a known serial with a different vendor or product
when both observations carry that field, or a retired drive reappearing.
Motion and data work stop, and a daemon starting with this condition does not
start. A drive whose host connection is down does not cause this identity
refusal.

A write, finalize or tail replacement on a standalone drive that ends in a
write fence also holds the drive, including when the tape fence itself cannot
be recorded and when a read during the write contradicts what was recorded.
The hold refuses `put`,
`tape finalize`, `tape supersede-tail`, `tape init`, `tape adopt`,
`tape assign-pool`, `tape rebind` and `tape set-generation` in that drive,
including on any other cartridge.
The `assign-pool --tape <UUID>` form opens no drive and does not consult its hold.
On the failed cartridge, put, finalize and the four catalogue commands report
the tape fence first; init reports `DriveFenced` on any cartridge. Reads and
`rem tape release` do not consult the hold; their other admission checks still
apply. An append-admission refusal (`append_admission_refused`) fences only
the tape; a session-suffix contradiction (`session_suffix_contradiction`)
found at append admission also holds the standalone drive.
The hold is recorded in the library log. If that record fails, the command prints
`error: cannot record the hold on standalone drive <serial>: <error>; motion is fenced and recovery is required`
and motion stops for the rest of the process. The hold is not on record, so a
later command does not find the drive held: check the drive and the tape before
using that drive again.

The standalone refusal names the hold and its release command:

```text
DriveFenced: drive <serial> library:<n> <cause>; release the hold with `rem tape quarantine release library:<n> --device PATH --allow-device <serial> --ack TEXT`
```

`rem tape quarantine list` on the standalone route includes `standalone_holds`:
each row has `quarantine_id`, `drive_serial`, `cause`, `detail` and `state`.
It lists all active standalone holds in the selected state root without opening
a device. After checking the drive, acknowledge its hold with:

```sh
rem tape quarantine release library:<n> --device PATH --allow-device <serial> --ack "drive checked"
```

This opens no device and sends no device command. It prints the released hold
with `state: "released"` and the tape fences still present in `active_fences`.
The hold and tape fences have separate releases: acknowledgement of the drive
never releases a tape fence. Use the library release form for releasable
tape fences; tape fences requiring physical recovery keep that requirement.

If a daemon answers on the selected configuration's socket, it refuses the hold
with a message containing this text; there is no offline fallback after that
refusal:

```text
fence library:<n> holds standalone drive <serial>; stop the daemon and run `rem tape quarantine release library:<n> --device PATH --allow-device <serial> --ack TEXT` as a standalone command
```

A standalone initialization write with an uncertain outcome holds the drive and
returns the write's own error. It creates no tape fence; the hold's
`evidence_json` names the planned tape UUID. The next init refuses with
`DriveFenced` before RESERVE or preparation, on any cartridge. If the hold
cannot be recorded, the refusal is:

```text
cannot durably record standalone write uncertainty for <uuid> at <endpoint> (<reason>): <detail>; the drive hold was not recorded: <error>; motion is fenced and recovery is required
```

That failure is counted as `StandaloneFenceUnrecorded` and stops further motion
for the rest of the process.

If a library-log write fails at the store, including while recording a refusal,
motion stops for the rest of that process. This is a storage failure, rather
than an admission refusal. Further loads, moves and writes refuse with
`library authority failed; motion is fenced until startup reconciliation`.
A restart can reconcile a failure before publication when no ambiguity marker
remains. A tape-log `<uuid>.ambiguous` or library-log `library-log.ambiguous`
marker blocks startup and is not cleared by restarting. Stop the daemon, correct
the storage problem and free enough space for the retained prefixes, then run:

```text
rem-debug library-log inspect --config /etc/rem/config.toml
rem-debug library-log recover --config /etc/rem/config.toml --confirm-clear-ambiguity
```

Inspection reports both marker families, owed fence UUIDs, segment sizes, required
and available space, and the library replay or why it was skipped. Recovery takes
the state lock, clears tape ambiguity before library ambiguity, republishes the
retained prefixes, reconciles owed tape fences, and records a synced recovery
event. An owed fence recorded in the reconciliation trigger is reconciled by
recovery: raised, found already covered, or skipped with a reason. The result is
reported in its `reconciliation` output. If the disk was too full even to record
the owed fence, the daemon
logged `could not record lost library fence` and no command can show or raise that
fence, so check the daemon's log for that line before returning the tape to service.
Restart the daemon after successful recovery.

`LibraryLogRepairDeferred` names the damaged segment, offset and sequence.
There is no library-log repair in this release; `reset-catalog` refuses too.
Keep the state directory and its evidence. The recovery command does not repair
a damaged log.

## Media-readiness fences and quarantine

Remanence classifies TEST UNIT READY results into an explicit readiness
state machine instead of retrying blindly. The states a command reports
(and the wait-ready exit codes they map to) fall into four groups:

- **Retryable** (exit 10): becoming ready, unit attention, target busy.
  The right move is to wait — `rem tape wait-ready --wait` polls for you.
- **Terminal** (exit 30): no medium, repeated identical unit attentions,
  a decoded check condition, or a state whose sense data says a reset or
  power cycle is required. An operator has to act.
- **Ownership** (exit 50): reservation conflict — another initiator holds
  the drive.
- **Unknown** (exit 40): the transport lost the answer. Nothing can be
  assumed about the drive.

For drive errors with sense residue correction, `bytes_transferred` is the corrected count; the kernel count follows in parentheses as `kernel reported`.

Two practical notes from real hardware. First, an LTO-9 cartridge's first
load triggers a one-time media optimization pass that can run well over an
hour; this is documented drive behavior, not a hang, and it is why
`wait-ready` defaults to a 2.5h timeout with 30s polls. Second, a readiness
wait that gets interrupted (signal, crash) does not evaporate: it leaves a
durable operation record, and `rem tape wait-ready --resume <OPERATION_UUID>`
continues it (`rem op list` prints the operation UUID) without moving media.

When an operation ends in a completion-unknown or interrupted state,
Remanence writes a **quarantine fence** against that tape or drive. Fenced
media is refused for init, daemon writes and reads, and robotics dispatch
until the fence is cleared. You will see messages like:

```text
... is blocked by active media-readiness fence <id> operation=... state=...;
run `rem tape quarantine show <id>` or wait-ready/resume before retrying
```

The daemon resumes load-transient fences itself with a 9000 s timeout and
30 s steady polls, including after startup reconciliation, unless a tape-I/O
fence blocks motion on the same tape or drive. Tape-scoped
`append_admission_refused` and `session_suffix_contradiction` fences allow that
motion when they are the only matching fences. Calls to
`ResumeMediaReadiness` attach to a running readiness wait or LoadDrive for the
same operation. `rem tape wait-ready --resume <UUID> --endpoint <URI>` watches
that operation and prints readiness state changes. Its local `--timeout` returns
20 while the
daemon keeps waiting. Without `--endpoint`, wait-ready uses direct SCSI and
requires the daemon to be stopped. Operator cancellation leaves a wait stopped;
an explicit resume may start it again. Startup still probes operator-cancelled
fences and clears a ready drive or requires release when the cartridge is absent.
Release-only states require release.

Shutdown stops readiness discovery and ends running waits, including the wait
inside LoadDrive, with a shutdown summary and their last durable readiness state
and sense keys. A standalone wait ends as Cancelled; LoadDrive ends as Failed.
LoadDrive publishes its terminal status after the changer refresh and release of
library ownership, so an attached client's open retry can acquire the library.
Shutdown fences stay resumable without an operator cancellation;
startup reconciliation and automatic discovery resume them on the next start.
`wait-ready --resume --endpoint` returns 10 for either resumable shutdown end,
0 for success, 20 for `timeout_unknown`, 40 for `transport_unknown` or
`aborted_unknown`, 50 for `reservation_conflict`, and 30 for other failed or
cancelled ends.
Discovery retries failed waits and non-busy start refusals on its 30 s sweep;
repeated readiness notifications do not restart the 5 s busy retry window.

Detached verification and manual finalize workers wait up to 15 minutes for
readiness. They keep waiting across failed readiness waits while the durable
record stays resumable, but stop on daemon shutdown, dispatch failure,
release-only states, or their deadline. Detached verification workers also stop on
operator cancellation of their own operation. Cancelling the underlying readiness wait
can also stop an attempt. Cancellation of an accepted manual finalization itself
is refused and recorded durably, without setting its cancel flag. A stopped
finalize attempt reports live FAILED with a summary while the finalization stays
accepted and FINALIZING. The daemon retries when a drive is ready; rerun with the
same idempotency key to follow or resume it. A new key is refused (`ALREADY_EXISTS`)
while that intent is pending. `rem tape finalize --wait` reports the stopped
attempt and exits non-zero. A tail write failure or an unproved tail leaves
COMPLETION_UNKNOWN and RECOVERY_REQUIRED with an explanatory detail; it needs
operator recovery, and a same-key retry is refused (`ClosedOperationRetry`).

`rem tape verify-index --tape-uuid <UUID>` resumes a media-readiness fence
and waits up to `15m` before retrying verification. It prints each readiness
state change to stderr. Use `--ready-wait <DURATION>` to set the wait, or
`rem tape verify-index --tape-uuid <UUID> --ready-wait 0` to keep the immediate
refusal. At the deadline the command returns the daemon's original message.

Inspect with `rem tape quarantine list` / `show <id>`. Both print the
drive's last sense with its name, for example `sense=02/04/02 (initializing
command required)`; a drive that reports `02/04/02` is covered under [A drive
stays not ready after a daemon crash](#a-drive-stays-not-ready-after-a-daemon-crash).

If an operator cancels a resumable wait, explicitly resume it to continue.
For a release-only media-readiness fence (`mrq-`), verify the physical state:
inventory has settled and the cartridge is where the library says it is.
Stop the daemon first; this release runs offline and needs the state lock.
Then release it explicitly:

```sh
rem tape quarantine release <id> --after-settled-inventory \
    --ack "verified slot inventory after power event; RCA in ticket 123"
```

The fence exists so that nobody writes to a tape whose state was last seen
mid-uncertainty.

<!-- code-anchor: crates/remanence-api/src/startup_media_readiness.rs crates/remanence-api/src/mount.rs crates/remanence-api/src/write_owner/write_session.rs crates/remanence-api/src/append_admission.rs crates/remanence-library/src/transport.rs crates/remanence-library/src/handle/tape_io/media_gate.rs @ cc8d69d5ed17e668613a3691f4b4496322ebc9ad -->
## A drive stays not ready after a daemon crash

A drive that answers `02/04/02` holds a cartridge it has not threaded. This
happens when a process is killed between the SSC UNLOAD of a dismount and
the changer move home, or when the changer move fails after a successful
UNLOAD: the drive has unthreaded the tape, the changer has not moved it, and
the cartridge stays in the drive. The drive answers NOT READY until it
receives a LOAD, and polling alone never ends that state. Remanence reads
that answer as a cartridge becoming ready, so a restarted daemon reconciles
a media-readiness fence for the drive.

Readiness waits (daemon readiness operations, startup reconciliation, `rem
tape wait-ready --wait`) send one SSC LOAD when changer inventory shows the
cartridge in the drive. They record the attempt in the operation before the
LOAD and send no second one until the drive has reported ready, also across
a restart. On SIGTERM, SIGINT or SIGHUP, the daemon stops admission, checkpoints
and closes open sessions, and leaves cartridges seated. A dismount waiting for closeout
stops before issuing UNLOAD. Once UNLOAD has been issued, its MOVE home
continues, and shutdown waits for it without the 600-second reservation deadline.
That MOVE retains the configured `changer_not_ready_wait_seconds` magazine/door
wait even during shutdown. Other reservations retain their 600-second bound.
The service manager's stop timeout (systemd `TimeoutStopSec`, for example) must
cover `daemon.shutdown_close_deadline_secs` plus command settlement (TapeIo permits
900 seconds), plus up to 600 seconds per configured drive, since ordinary
reservation waits run sequentially. Outstanding changer UNLOAD/MOVE work adds
its configured readiness waits and command timeouts. These are additive bounds,
not one global 600-second deadline. Every drive actor uses the shared cutoff from
creation, including maintenance on a drive with no prior write session. Shutdown
waits for actor ownership through issued-command settlement; it interrupts host
stream backpressure separately. Inventory and read sends waiting for consumers
are interrupted when shutdown begins. There is no startup probe of the service
manager's configuration. A failed MOVE or forced kill can still leave media
unthreaded.

Graceful shutdown gives every write session the same absolute deadline, starting
when shutdown begins: `daemon.shutdown_close_deadline_secs` (default 300 seconds,
zero refused). Sessions finish in parallel. The deadline covers the final barrier,
`Committed`, and `SessionEnded`. At expiry, no new drive command is issued. An
already issued command keeps its normal command timeout and is allowed to settle.
After settlement, the session records `SessionEnded` with the whole uncommitted
tail, including an interrupted closeout sidecar, and leaves closeout due. The
ending preserves a failed append, drive error or failed closeout; otherwise it
records `graceful_shutdown`. Shutdown does not start a separate closeout pass.
Already delivered receipts stay valid; a durable commit is never turned into a failed Close by a
later projection or cleanup error. One warning identifies each interrupted tape
and session and the work left due.

If command completion is unknown, or the native ending cannot be written, the
session stays open in the log and the next access takes the crash-recovery row.
A service-manager timeout that kills the daemon before settlement and the ending
has the same effect. Complete uncommitted Objects then require the operator's
`rem tape supersede-tail` acknowledgement; a recorded live ending needs none.
A failed closeout or drive error can also leave an uncovered committed range.
The daemon services that [closeout debt](#closeout-debt) automatically. A later
write open or `rem tape closeout <UUID>` can also request the work. Each path
proves identity, the committed cursor and the whole named suffix before
rebuilding and closing the range. A recorded live ending needs no supersede
acknowledgement. Memory refusal leaves closeout due; it does
not fall back to a different epoch policy. A durable log record remains authority
when SQLite projection fails; startup replay restores the projection.

Removal waits for ongoing closeout but does no parity work itself.
`drive_idle_unload_seconds` returns idle cartridges to their recorded source slots;
`0` disables that timer. Use `rem drive unload --library <serial> --bay <bay>` before
stopping the daemon when cartridges must be returned home.

When that LOAD fails, was already spent, or is never sent because changer
inventory does not show the cartridge in the drive, the fence stays. The
signs are:

- `rem tape quarantine list` shows a fence for that cartridge in state
  `media_initializing` with `sense=02/04/02 (initializing command
  required)`, updated at every poll, and the drive never comes ready.
- `rem top --once` shows the bay busy with the cartridge mounted and no
  I/O.
- Writes and restores that need the drive wait on the fence.

The same state appears during the one-time calibration of a new LTO-9
cartridge, which can last over an hour. A cartridge that has been loaded
before, and a stall that began with a daemon kill or crash, point to the
unthreaded case instead.

A LOAD resolves it. When the readiness wait's own LOAD has not, send one by
hand with `sg_start` from sg3_utils:

```sh
rem libraries --json      # the observed_endpoints entry for the library serial and bay gives /dev/sgN
sg_start --load /dev/sgN
```

The LOAD threads the cartridge that is already in the drive and moves
nothing in the library, so it is safe to send while the daemon runs. The
drive comes ready within seconds, sometimes after a unit attention that
reports the change to ready, and the daemon clears the fence at its next
poll. If the fence remains after the drive reports ready, handle it as
described in the previous section.

<!-- code-anchor: crates/remanence-library/src/handle/mod.rs crates/remanence-library/src/handle/tape_io/mod.rs @ 1dd451b2 -->
## Dirty snapshots and completion-unknown

For every state-changing SCSI command, Remanence distinguishes "the device
refused" (CHECK CONDITION — state is known, nothing moved) from "the
transport failed" (timeout, kernel I/O error — the command may have
completed anyway). The second case marks the in-memory library snapshot
dirty with cause `CompletionUnknown`; a multi-step operation that failed
partway marks it `PartialFailure`. A dirty snapshot means: refresh or
`rem-debug rescan` before trusting either endpoint of the last move. The
error string to look for is `transport error (completion unknown)`.

This is a *robotics/library-snapshot* concept, distinct from the
write-path's own fail-closed mechanisms (see [Writes are
refused](#writes-are-refused) below): a reset UNIT ATTENTION also
invalidates the drive handle's own notion of tape position and mode
until it's re-verified, and a failed data command can additionally
poison the write transfer or the whole write session. All three are
independent "don't trust what you can't prove" mechanisms at different
layers — seeing one does not imply the others tripped too.

<!-- code-anchor: crates/remanence-daemon/src/entry.rs crates/remanence-daemon/src/tls.rs crates/remanence-state/src/error.rs crates/remanence-state/src/config.rs crates/remanence-state/src/library_runtime.rs @ b00887a25e94ec016ace10ac726a3296346b0454 -->
## The daemon refuses to start

`rem-daemon` checks its world in order and exits 1 with a specific
`error:` line for each failure. The ones worth knowing:

| Message | Cause and fix |
|---|---|
| `error: load config ...` | TOML parse or validation failure. Unknown keys are hard errors; all paths must be absolute; `listen` requires `[daemon.tls]`; `tape_io.legacy_single_block`/`tape_io.pipelined_submission` are not accepted keys and fail to load with a specific message; the pipelined path is the only path for streaming reads and writes, so delete the key. (Full verification reads one block per command and does not use it.) |
| `daemon.io_memory_ceiling must be non-zero` / `... exceeds daemon.io_memory_ceiling` (on `spool_tmpfs_ram_budget` or `tape_io.read_reservoir_bytes`) | The shared I/O memory budget is zero, or a per-purpose reservation exceeds it. `io_memory_ceiling` (default 24 GiB) is the hard cap shared by the append spool and every drive's read reservoir. |
| `tape_io reservoir watermarks require 0 < read_reservoir_low_pct < read_reservoir_high_pct <= 100` | Watermark percentages misordered or out of range. |
| `tape_io.staging_ring_buffers` validation failure | Must be in `2..=16`. |
| `untrusted state volume ...` | State, journal, audit, index, cache, or socket path sits on tmpfs/NFS/SMB/overlayfs and `journal.require_trusted_volume` is `true` (the default). Move the state or consciously disable the filesystem-type check. The journal directory's own check (the local-filesystem allow-list, write-barrier options and ZFS flush switches described under `journal volume rejected` above) always applies, regardless of this setting or `--allow-untrusted-state-volume`. |
| `error: configure spool dir ...` | The spool resolved to tmpfs and `spool_tmpfs_ram_budget` is not set. Spooling to RAM needs an explicit budget acknowledgement. |
| `TLS private key ... has insecure permissions` | The key file is group- or world-accessible. `chmod 600` it. |
| `drive discovery cannot preserve the required identity alarm; motion is fenced` | Discovery found one serial twice, two active identities for one serial, a changed vendor or product, or a retired drive reappearing; see [Tape-I/O fences](#tape-io-fences). A drive whose host connection is down is not a cause. |
| `startup blocked by active tape-I/O fence <id> ...` | The fence has missing or unparseable scope. Inspect its evidence. There is no operator route to repair a fence without a typed library source identity; release refuses with a message containing `fence has no typed library source identity`. Scoped fences produce WARNs and allow startup. |
| `error: discover libraries: ...` | Same permission gates as the CLI (tape group + `CAP_SYS_RAWIO`, via systemd `AmbientCapabilities` for a service). |
| `error: acquire daemon state ownership ...` | Another daemon or offline state-mutating command holds `<state_dir>/state.lock`. Stop the other owner; do not delete the lock file or point a second daemon at the same state directory. The kernel releases the lock when its owner exits. |

`rem-daemon` acquires `<state_dir>/state.lock` before opening API state and
holds that kernel `flock` for its lifetime. Offline state-mutating commands use
the same lock, so another daemon or maintenance command receives the ownership
error above even if it uses a different socket path. The Unix-socket liveness
probe remains a separate check for a stale or already-listening socket.

<!-- code-anchor: crates/remanence-api/src/read_core.rs crates/remanence-daemon/src/entry.rs @ c6beb285 -->
## Non-fatal warnings

Read-reservoir warnings apply to both `rem` and the daemon. Reads continue
when minimum slabs fall back to unlocked memory.

| Message | Cause and fix |
|---|---|
| `read reservoir uses unlocked buffers` | The soft `RLIMIT_MEMLOCK` limit or lock permission prevents locking minimum read slabs. Reads continue, but tape data may be swapped. Check the limit, slab size, and count in the warning; raise the limit with `ulimit -l`, `LimitMEMLOCK=` for the daemon unit, or a `security/limits.d` entry for the user. `rem doctor` reports the limit and configured minimum without blocking readiness. |
| `read reservoir growth stopped at effective cap` | A slab above the minimum could not be added. The restore continues with the reservoir it has. `reason` names the cause: an `mlock` error means the lock limit is reached, and `daemon.io_memory_ceiling has no reservoir capacity` means the shared ceiling is full. `allocated` is the slab count the reservoir stopped at. See [Throughput far below the drive](#throughput-far-below-the-drive). |

The daemon also prints two non-fatal startup warnings to stderr:
`daemon.io_memory_ceiling=... exceeds startup MemAvailable=...;
the fixed ceiling remains authoritative` and `daemon.io_memory_ceiling=...
exceeds LimitMEMLOCK=...; reads fall back to unlocked buffers when minimum
read reservoirs cannot be locked`. Both check the host's memory and lock
limits once at startup; the ceiling itself is what the daemon enforces at
runtime.

### Throughput far below the drive

Two host limits can hold a drive far below its rated speed without any
error, and they show in different places.

The lock limit affects restores. A restore stream's read reservoir locks
every slab it adds. When a lock fails, the daemon logs
`read reservoir growth stopped at effective cap` (target
`remanence_read_reservoir`) with the lock error in `reason`, and the
restore continues with the reservoir it has. `rem doctor` marks the same
limit with `!` in its `memlock` check. A daemon started by hand inherits
the limit of the shell or tmux server it runs in, often 8 MiB, and a
running tmux server keeps its limit after a new `limits.d` entry. The
`Max locked memory` line of `/proc/<daemon pid>/limits` shows what the
daemon has, and as root this raises a running tmux server's limit:

```sh
prlimit --pid <tmux server pid> --memlock=unlimited:unlimited
```

The fix is the lock limit, not `read_reservoir_bytes` or the batch
settings: raise it to at least `io_memory_ceiling`, or to unlimited, and
restart the daemon. [Sizing the lock
limit](reference-configuration.md#sizing-the-lock-limit) gives the
arithmetic.

The transfer size affects every command. The write diagnostics
(`target: "remanence_write_diag"`, phases `drive_prepare` and
`staging_ring_open`), the read `restore_total` event and
`rem top --once --json` report `effective_batch_blocks`. When it is
smaller than the configured `write_batch_blocks`, the kernel granted the
drive a smaller sg reserved buffer than Remanence requested, and the batch
is that buffer divided by the block size: a 1 MiB grant with 256 KiB
blocks gives 4. The write staging ring is not locked memory, so the lock
limit does not change this value, and a larger `write_batch_blocks` does
not either. The grant is bounded by the HBA driver's maximum transfer size
and the sg driver's limits. With a smaller batch the host issues more
commands for the same data, and the feed can fall below the drive's
speed-matching floor.

A host that has never been tuned can show both at once. Each is fixed at
its own limit.

### Per-object write diagnostics

`remanence_write_diag` uses milliseconds for `elapsed_ms` and MiB/s
(1,048,576 bytes/s) for `throughput_mib_s`. Plain and parity appends emit
`prepare`, `transfer`, and `tail`. `prepare` covers validation, layout, and
capacity reservation. `transfer` covers the object writer, including parity
accumulation, CRC, hashes, copies, and object filemarks. `tail` covers the host
projection and session detach after transfer; it emits on success or error
and does not itself assert a commit. Shared checkpoint barriers retain their
separate diagnostics.

The parity `transfer` event adds `write_commands_ms`, the sum of time inside
raw fixed-block WRITE calls, and `between_writes_ms`, the sum of intervals
from one WRITE return to the next WRITE entry. Those intervals include host
work and any intervening filemark or position calls. `transfer_host_ms` is
`elapsed_ms - write_commands_ms`; it also includes work before the first and
after the last WRITE. These are host-side elapsed measurements, not drive
firmware counters. `object_id`, `pool_id`, and `tape_uuid` correlate phases.
The plain path reports the same elapsed fields; its
existing gap and staging-wait distributions describe command cadence.

One `drive_append_total` line identifies each completed actor append by
`object_id` and `caller_object_id` and reports:

| Field | Meaning |
|---|---|
| `staging_mode` | `overlap` for the receive ring, `serial` for a completed spool or local file. |
| `staging_reason` | `configured_overlap`, `configured_serial`, or the first failed admission condition: `no_declared_size`, `empty_object`, `no_start_digest`, `no_replay_capability`, `no_caller_object_id`, `below_overlap_min`, or `canonical_start`. In-process local files use `completed_path`. Unsupported manifests are refused before append. |
| `prefill_lead_bytes` | Actual ring occupancy when prefill completes, or zero for a completed spool. |
| `feed_wait_ms` | Ring consumer receive/resume wait time for overlap; elapsed time between WRITE calls for serial. |
| `feed_wait_measure` | `ring_consumer_wait` or `between_write_calls`, naming the proxy used. The latter spans the first WRITE entry through the last WRITE return, minus time inside WRITEs. It includes host work and intervening non-WRITE commands, so it is an upper bound on feed starvation. An error without transfer measurements uses `unavailable`. |
| `open_path` | `full`, the current session-open path. |

`replay=true` reports no new tape transfer and a zero append rate. Errors
carry `status=error` and may lack an object UUID. `drive_append_total` measures
the actor append before a threshold-triggered shared checkpoint; spool receive
and session open/close remain outside this rate.

The write and dual benchmarks match the receipt's object UUID to a
`drive_append_total` event within that command's log window. Their CSV and
evidence summary retain the client wall rate, daemon append rate, and staging
mode. The dual aggregate row names the append leg's object and daemon rate;
the client aggregate rate includes the concurrent restore. Missing diagnostics
are `unavailable`; the kit never substitutes another object's latest event.

<!-- code-anchor: crates/remanence-api/src/closeout_obligation.rs crates/remanence-api/src/write_owner/write_session.rs crates/remanence-api/src/write_owner/checkpoint.rs crates/remanence-api/src/append_admission.rs crates/remanence-api/src/tape_closeout.rs crates/remanence-api/src/write_owner/terminal_finalize.rs crates/remanence-state/src/projector/index.rs crates/remanence-state/src/library_runtime/terminal.rs @ 25d266fa0ef327b208bc45f872a6e8575d5a5399 -->
## Closeout debt

Committed data is on tape and has a durable record in the tape log. It is
protected only when a parity sidecar covers it. An uncovered committed range
is closeout debt: the data remains readable, but lacks that parity protection.
Use `rem catalog tapes --json` or `rem top` to see its byte count, original age
and retry reason. `OVERDUE` means it has reached the configured alarm age;
`DEBT?` means the native log could not be read, so the debt is unknown.

The daemon services eligible debt without a client request, including after a
session ends and at startup. It reconstructs its work list from native logs,
mounts tapes as needed, and attempts at most one maintenance operation per
library at a time. Media work stays within the configured libraries and drives.
Transient failures retry after 1, 5, 30 and then 60 seconds; the full work list is
rescanned every 60 seconds and on a wake. Pool selection prefers eligible due
tapes, so a write open can wait for a selected tape's closeout before admission.

Write protection, fences, wrong media and identity mismatches park the work
until the condition changes or an operator requests closeout. A parked tape
keeps its debt and retry reason visible. Pool selection ranks it behind other
fitting tapes; if none fit, it attempts the parked candidate so admission can
return its actionable refusal. Parked work wakes when a readiness fence clears, a
tape-I/O fence is released, the library's barcodes change, a load, unload or move
completes successfully, or an operator runs `rem tape closeout`.

Persistent typed medium or hardware failures in closeout maintenance receive
three attempts with the existing backoff, then park the tape. Debt views and
`rem top` show `parked: media fault (3/3 attempts)` with the diagnostic cause.
Diagnostic text changes do not reset the budget; ordinary contention does not
consume it. For manual finalization, untagged motion failures after reserving a
bay also count toward this budget when the existing close-out policy classifies
them as transient. This includes eviction, changer moves and drive-actor failures.
A tail write failure is the exception: it leaves completion unknown and parks the
tape at once.
Write protection and fences requiring operator action still park immediately;
pre-reservation refusals keep their existing retry policy. The wake conditions
above reset the budget. Other unrecognized failures remain transient.

After resolving the refusal, run `rem tape closeout <UUID>` to request and watch
closeout for that exact tape. It succeeds without media work if nothing is due.
An accepted finalization plan owns its uncovered range instead of ordinary
closeout. A stopped attempt leaves that finalization accepted for the daemon to
retry; use the original idempotency key to follow or resume it. A new key is
refused while its intent is pending, and cancellation after acceptance is refused.
Completion unknown requires operator recovery instead of another attempt. See the
[CLI reference](reference-cli.md#rem-tape-closeout) for retry arguments and
[manual finalization](reference-cli.md#rem-tape-finalize-library) for the
`CloseoutRequired` refusal.

A write session owns its parity accumulator until it ends. A checkpoint
commits data without necessarily closing the epoch. Clean close returns durable
receipts, then writes the short sidecar, synchronizes it, and records `ClosedOut`
and `SessionEnded`. The drive and tape stay owned during this work; a following
access waits. No accumulator or reservation carries into the next session.
After `Close` or `rem put` returns, `written_extent_lba` and physical EOD can
diverge until the ending records are published; wait for the session ending
and reread the catalog before comparing the settled extent.

`rem tape closeout <uuid-or-barcode>` waits for an idempotent `tape_closeout`
operation. Its progress phases are `queued`, `proving`, `rebuilding`,
`writing_sidecar`, `barrier`, `projecting`, and `complete`. Diagnostics bind the
tape incarnation and log sequence, W/T range, read attempt, elapsed time, and
current/peak reserved bytes; `source` is `live` or `rebuilt`. Reservations are
accounting limits, not process RSS. `CloseoutMemory` refuses before drive work.

A committed range without a sidecar remains readable as unprotected data.
After a durable abort, failed append, or client disconnect, the session's named
tail authorizes suffix proof without operator acknowledgement. Accepted buffered
writes and immediate filemarks are possible extents; only successful non-immediate
filemarks or barriers establish the synchronized lower bound. A failed barrier
leaves that bound unchanged. Recovery accepts EOD between the synchronized point
and the end of possible extents, recording its exact position in `SessionEnded`
before reuse when those bounds differ. An already exact ending adds no refinement
record. Lost buffered data therefore needs no supersede acknowledgement.
EOD below the synchronized point or committed cursor remains a contradiction.
No abandonment filemark is written. A drive error leaves closeout due for a later
access. A crash with complete uncommitted Objects still requires
`rem tape supersede-tail`; confirmation must carry the preview's log sequence.
`TailPreviewStale` requires a new preview. A crash-torn last file is bound by
its exact observed block count and SHA-256, then re-proved through EOD.

Rebuild compares the committed range to the original-write check and retries a
mismatch once after re-proving identity. A second mismatch raises
`CloseoutCheckMismatch`. Committed extent contradictions raise
`CloseoutReadContradiction`; a suffix contradicting its named ending raises
`SessionSuffixContradiction`. `TailBeyondKnownState` raises the
`append_admission_refused` fence, which blocks writes and does not itself block read
sessions. Neither recovery fence is released by `rem tape quarantine release`.
Use `rem tape supersede-tail` where the tail can be recovered; it refuses a
contradicting recorded ending or a state that shows no open session on the tape
(its last session closed, or none opened since it was initialized here).
Otherwise withdraw the tape with `rem tape retire`, subject to due
closeout, or start from a newer copy of the state directory. A durable live end
alone raises no tape-I/O fence.
An ambiguous native append still stops media work with `AuthorityAmbiguous`.
Checked resumes retain original-write check provenance. A commit disagreeing
with the frozen range is refused rather than used for reconstruction.

Manual finalization refuses a due closeout with `CloseoutRequired` and names
the closeout command. Retirement changes the offline catalog without media
work and refuses with `CloseoutRequired` while parity closeout is due.
`CloseoutUnprotectedFinalizationUnavailable` refuses finalization when that range
cannot be proved; operator acceptance of
unprotected finalization is not implemented. Cartridge removal performs no parity
work. Graceful shutdown closes sessions and leaves cartridges seated; its
shared shutdown deadline covers maintenance as well as write sessions.

<!-- code-anchor: crates/remanence-api/src/pool_write.rs crates/remanence-parity/src/error.rs crates/remanence-api/src/pool_write/selection.rs crates/remanence-api/src/pool_write/model.rs crates/remanence-api/src/write_admission.rs crates/remanence-api/src/drive_pool.rs @ e4d4f53f433998602c4e2baa56747f68e4e2684e -->
## Writes are refused

Pool writes fail closed on a set of preconditions. The common refusals:

- `tape <UUID> is sealed, seal-due or finalizing; maintenance must complete before Object admission`
  refuses a pinned write. Complete pending maintenance; a sealed tape accepts no
  more objects, so select another tape or open against the pool.
- `Busy: tape is finalizing; watch operation <UUID>` is gRPC `ABORTED`.
  The binary trailer `remanence-busy-operation-bin` carries that operation UUID.
  Follow it with `rem op watch <UUID>` before choosing the next write target.
- `active tape-I/O fence <id>: <reason>` — see fences above.
- `tape is not ready for writing: state=...` — the tape's catalog state
  is not `ready` (retired, quarantined, or mid-initialization).
- `insufficient tape capacity ...` / `no writable tapes ...` — the pool
  has no tape that can take the object under the configured watermarks.
- `LTO hardware compression is enabled; parity-protected writes require
  it disabled` — REM-PARITY sizes stripes in physical blocks, and drive
  compression would decouple logical from physical. Disable compression
  on the drive.
- `medium is write-protected` — the physical tab.
- `daemon has no write spool (read-only mode)` — the daemon is running
  with `read_only = true`.
- `write session poisoned by a failed append; abort the session and open
  a new one` — the session-level poison: the first failed `AppendObject`
  in a write session poisons it permanently, so a retry cannot silently
  continue writing at an uncertain physical position. There is no way to clear this
  short of closing the session and opening a new one. This is separate
  from (and stricter than) the per-transfer staging-ring poison, which is
  scoped to discarding the rest of one already-failing `AppendObject`
  call rather than the whole session.
- `terminal finalization requires recovery` — finalization crossed its first
  durable boundary and then failed. Object admission is permanently disabled;
  recovery may write only the missing terminal suffix components from the
  proved progress position. Do not open a new Object-writing session for that
  cartridge.
- `staging ring accounting imbalance` — an internal invariant violation
  in the pipelined write path (allocated/returned buffer counts don't
  match at transfer end); treat as a bug report, not an operator action.
- `selected tape sealed at the checkpoint boundary; reopen against the
  pool to roll placement` — not a failure: a batched-durability session's
  tape was sealed at a checkpoint barrier, so this session can no longer
  append to it. Open a new write session against the pool; placement
  rolls to the next tape automatically.
- `an append with the same pool/caller replay key or canonical Object UUID
  is still awaiting checkpoint; retry after that checkpoint completes` —
  the process-wide replay-key coordinator refuses a second concurrent
  attempt at what looks like the same retry; wait for the in-flight
  checkpoint and retry.

Pinning a specific cartridge with `rem put --tape` (see [`rem
put`](reference-cli.md#writing-to-tape)) adds its own admission refusals,
each naming the pinned tape UUID: `tape ... is not in the catalog; an
uninitialized cartridge has no tape UUID` (never initialized — run `rem tape
init` first), `tape ... is a <kind> tape, not a data tape` (for example a
cleaning cartridge), `tape ... is assigned to pool ..., not the required
pool ...` (the `--pool` guard named the wrong pool), `tape ... is not
writable: <reason>` (the same writability preconditions above), `tape ...
is fenced by quarantine ...` (see fences above). A tape with a known written
prefix but no matching per-tape log is also refused before append positioning.

<!-- code-anchor: crates/remanence-daemon/src/entry.rs crates/remanence-api/src/closeout_obligation.rs crates/remanence-api/src/mount.rs crates/remanence-api/src/pool_write/selection.rs crates/remanence-api/src/drive_pool.rs crates/remanence-api/src/write_owner/write_session.rs crates/remanence-api/src/write_owner/terminal_inventory.rs crates/remanence-library/src/handle/tape_io/media_gate.rs crates/remanence-api/src/daemon_catalog_services.rs crates/remanence-api/src/operation_source.rs @ b00887a25e94ec016ace10ac726a3296346b0454 -->
## Reading the logs

The daemon logs JSON to stderr, one flattened object per event, filtered
by the standard `RUST_LOG` variable (default `info`). Write-path events
carry `tape_uuid`, `pool_id`, and `session_id` fields, so `jq
'select(.tape_uuid=="...")'` (put the tape's UUID from `rem catalog tapes` in place of the dots) reconstructs one tape's story. The CLIs do
not use the tracing stack at all: `rem` and `rem-debug` print
human-readable text, or stable JSON with `--json`.

Read-pipeline health has its own structured event: every read session
emits exactly two JSON log lines, `target: "remanence_read_diag"`, one
`phase: "open"` and one `phase: "close"`, with reservoir occupancy,
park-cycle count, and feed-gap timing fields. `jq 'select(.target ==
"remanence_read_diag")'` isolates them; a `park_cycles` that keeps
climbing across sessions, or an `occupancy_bytes` close to
`reservoir_high_watermark` at close, points at a consumer that can't
keep up with the drive (increase `tape_io.read_reservoir_bytes`, or
speed up the reader) rather than a hardware fault.

Strings worth grepping for, mapped to the sections above. Some strings are
RPC refusals or debt diagnostics rather than standalone log events:

| String | Remarks |
|---|---|
| `closeout work-list scan failed; retrying` | WARN: the worker cannot reconstruct its work list. Inspect the attached error and native-log access; it retries the scan. |
| `closeout needs operator; waiting for wake` | WARN: work is parked. Resolve the reported refusal, then request closeout or allow the relevant readiness or inventory change to wake it. |
| `CloseoutOverdue` | WARN: debt has reached the alarm age. The operation kind is `closeout_overdue`; inspect the tape's retry reason. |
| `parked: media fault (3/3 attempts)` | Debt diagnostic: repeated medium or hardware failures exhausted the attempt budget. Inspect the drive and media before waking the work. |
| `shutdown close deadline interrupted session` | WARN: the shared shutdown deadline interrupted the session; the event identifies its tape, ending and remaining work. |
| `shutdown close deadline expired; no new drive command` | Drive-command refusal at the shared cutoff. Issued commands still settle; see the shutdown bound above. |
| `daemon shutdown; maintenance admission closed` | RPC refusal: the daemon is stopping. Submit fresh maintenance after restart. |
| `drives are busy` | A library-wide request (inventory refresh, load, unload, move) is refused while sessions or other owners hold the library's drives; let them finish and retry. |
| `terminal inventory interrupted by shutdown` | The inventory stops because shutdown began. Retry after restart. |
| `pool selection waiting for tape closeout` | INFO: the selected tape is held by session closeout or a worker. The open waits, then checks that tape again; this is not a free-bay queue. |
| `pool selection skipped tape` | INFO for the selection pass that decides admission; DEBUG for speculative passes. `pool_id`, `tape_uuid` and `reason` identify the excluded candidate, such as a live writer, finalization or parked debt. |
| `pool selection skipped tape after closeout; selecting again` | INFO: the tape fails a check after waiting, or finalization wins the reservation race. Selection considers another tape; inspect `reason`. |
| `pool selection skipped unreachable tape` | INFO: the selected tape has no usable route to a configured drive. Selection tries another candidate. |
| `pool selection skipped tape after admission refusal; selecting again` | INFO: actor admission returned an operator-required refusal for a pool-selected tape before admitting an object. Selection tries another candidate; pinned writes return the refusal. |
| `no tape libraries reachable` | permissions or no hardware |
| `every SCSI probe returned EPERM` | missing CAP_SYS_RAWIO |
| `was removed; the pipelined fixed-block path is now the only tape I/O path` | stale `tape_io.legacy_single_block`/`pipelined_submission` key in config |
| `blocked by active media-readiness fence` | quarantined media |
| `startup blocked by active tape-I/O fence` | missing or unparseable fence scope |
| `transport error (completion unknown)` | dirty snapshot; rescan |
| `write session poisoned by a failed append` | session-level write poison; open a new session |
| `sealed at the checkpoint boundary` | tape sealed at a checkpoint barrier; open a new session against the pool |
| `has insecure permissions` | TLS key mode |
| `read-only mode` | daemon started with `read_only = true` |
| `acquire daemon state ownership` | another daemon or offline state owner holds `state.lock`; stop that owner |
| `read reservoir uses unlocked buffers` | non-fatal memory-lock fallback; see warnings above |
| `read reservoir growth stopped at effective cap` | reservoir growth hit the lock limit or the I/O ceiling; see [Throughput far below the drive](#throughput-far-below-the-drive) |
| `remanence_read_reservoir` (as `target`) | read-reservoir memory-lock warning and remedy |
| `remanence_read_diag` (as `target`) | read-pipeline reservoir/backpressure diagnostics, see above |

<!-- code-anchor: none -->
## A tape that will not verify or identify itself

Two guides cover the tape-level cases. [Full verification](guide-full-verification.md)
explains what `rem tape verify-index` checks and how to read its findings.
[Damaged tapes](guide-damaged-tapes.md) covers a damaged terminal index, an
unreadable bootstrap, and the refusals the tools print for each.

<!-- code-anchor: none -->
## Known open issue

Tape recycling outside Remanence (for example re-creating a virtual
cartridge with the same barcode) can leave the catalog's tape identity
out of step with the UUID written at the beginning of tape. The retire and rebind machinery exists (`rem tape retire`); Remanence does
not yet reconcile this automatically.

## Element inventory changes before unload or move

The daemon reads element status with volume tags before SSC UNLOAD and MOVE
MEDIUM. If occupancy or the barcode changes, it refreshes its view, logs the
expected and found state, and refuses the command. An idle unload stops after
an inventory refusal. Check the refreshed inventory and other initiators before
requesting another move. A full home slot requires an explicit empty destination;
the daemon does not select a replacement automatically.

A failed move also refreshes inventory. Sense `05/3B/0D` reports a full destination
and `05/3B/0E` an empty source. An unload followed by a failed move does not prove that the
cartridge remains in the drive; use the fresh element status for recovery.

<!-- code-anchor: crates/remanence-library/src/handle/changer_ready.rs crates/remanence-api/src/live_status.rs crates/remanence-api/src/mount.rs crates/remanence-library/src/handle/mod.rs @ b00887a25e94ec016ace10ac726a3296346b0454 -->
## Waiting for the library (open magazine or door)

A library shared with another system has its magazines and doors opened
while Remanence works. A MOVE MEDIUM issued while a magazine is out fails
with `02/3B/12` (medium magazine removed) and is not executed, so the
cartridge stays where it was. The daemon waits for the operator to close the
magazine before retrying the move.

A wait starts when a move fails with
`02/3B/12`, `02/04/01` (becoming ready) or `02/04/18` (a door is open), or
when the element status read just before the move already shows the source
or destination with exception `3B/12`; in that case the wait starts before
the MOVE instead of after a MOVE that is bound to fail. The daemon logs a warning such as `library <serial> not ready (medium
magazine removed); waiting up to 10m before retrying MOVE MEDIUM 0x0101 ->
0x03ec`, polls the changer with TEST UNIT READY every five seconds, and logs
progress every minute. The changer counts as ready when it answers TEST UNIT
READY and element status shows both the source and the destination
accessible and without an exception. A unit attention reported after the
magazine closes (`06/28/00`, `06/29/xx`, `06/3B/13`) is consumed by the poll,
and a BUSY answer is polled again up to three times. While the wait runs,
`rem top` shows `waiting for library: ...` with the time since the wait began
in the drive band title, `rem top --once` prints it under the library line,
and the live status carries it in `changer_wait`.

When the changer is ready, the daemon reads element status again and
compares the source and destination with what it saw before the move. It
retries only if the source still holds the same cartridge, by occupancy and
barcode, and the destination is still empty. If the operator moved a
cartridge into or out of either element while the magazine was open, it
refuses as in [Element inventory changes before unload or move](#element-inventory-changes-before-unload-or-move)
and does not retry. At most three waits are spent on one move, a wait before
the first MOVE included, and one
deadline, `[daemon] changer_not_ready_wait_seconds` (default 600, at most
3600), covers all of them. A unit attention on the MOVE that follows a wait
is waited out like any retry.

The move ends without success in these cases. When a MOVE has already been
sent, it fails as it did before, with the original sense and the refreshed
inventory. When the wait began before any MOVE and ends by its deadline or
a stop, no MOVE is sent; the daemon refreshes the inventory and reports
`library not ready (...); MOVE MEDIUM
0x.... -> 0x.... not issued`. When such a wait ends because a poll answered
with something waiting does not clear, the daemon re-reads and compares
both elements, as after a successful wait, before sending the one MOVE that
follows.

- the deadline passes;
- a poll answers with something waiting does not clear, for example a
  hardware error;
- the source cartridge had no readable barcode before the move, because an
  unlabelled cartridge swapped in during the wait could not be told apart
  from it, so no wait starts at all;
- the daemon is shutting down. Shutdown park moves do not wait, and a wait
  already running stops within a fraction of a second, so a stop is not
  held up by an open magazine. A stop set before a move starts spends no
  time, and that move is sent as it would be without the wait. A cartridge whose move home fails this way
  stays unthreaded in its drive, as before, and the next start reconciles
  it as described in [A drive stays not ready after a daemon crash](#a-drive-stays-not-ready-after-a-daemon-crash);
- a robotics operation is cancelled while it waits;
- the client of a write session open goes away while the load move for its
  tape waits. A client that goes away after that move has completed still
  has its newly opened session disconnected immediately, and the open returns
  `cancelled` with `write session open caller disconnected`. The move home of an
  evicted cartridge always runs to the end of
  its wait, because stopping it would leave that cartridge unthreaded in the
  drive.

The drive bay stays reserved for the whole wait. The cartridge has not
moved, so the reservation still describes where it is, and no other
operation can load into the bay or unload it in between. Other changer
commands for the same library queue behind the wait.

Every other changer failure keeps its existing handling: `05/3B/0E` and
other ILLEGAL REQUEST senses, hardware and medium errors, deferred errors,
`02/04/02` (initializing command required) and transport failures fail at
once. Element status that reports the slots of an open magazine empty with
exception `3B/12` does not affect the rest of the library; a cartridge in
such a slot is not located until the magazine is back, so pool selection
does not pick it. A seated cartridge whose home slot is already reported
unavailable is not evicted, so it is never unloaded towards a slot the
changer cannot reach. `rem-debug` and other direct library callers do not
wait.

In plain terms: when the operator has a magazine open, the daemon holds the
move until the magazine is back, checks that nothing it was about to touch
has changed, and then finishes the move; it gives up as before when the
bound passes, the daemon stops, or nobody is waiting for the result.

## Catalog and checkpoint lock refusals

All catalog writers in a process share one writable SQLite connection behind
an in-process lock and take it in turn, so concurrent checkpoints wait for each
other in memory rather than on SQLite. Tape-log projections start immediate
transactions, which hold SQLite's writer from their first read. Read-only views
open their own connections; the writer's connection and the read-only views'
connections each have a five-second busy timeout.

A per-tape log lease is exclusive per tape and lasts through the write
session. A competing lease for that tape returns a refusal; it does not block
indefinitely. Separate tapes use separate lock files. A rejected session open releases its
journal leases before publishing the refusal, so an immediate retry or replay
does not race that cleanup. Lock refusals log at WARN
with the operation and write-session context. Check the session and tape ids
and allow the holder to close before retrying. Do not delete a held lock file.
