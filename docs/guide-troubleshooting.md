# Troubleshooting

This page lists the failure modes Remanence reports deliberately, what each
message means, and what to do about it. When the software cannot prove what
state the hardware is in, it stops and says so rather than guessing, so many
things that look like an outage are a fence (a recorded block on a tape or
drive) doing its job.

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

<!-- code-anchor: crates/remanence-cli/src/standalone.rs crates/remanence-library/src/discovery.rs @ 4dc2a3c6 -->
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
| Cannot write `/run/lock/rem`, especially after reboot | Create it with `sudo install -d -o root -g tape -m 0770 /run/lock/rem`. Put `d /run/lock/rem 0770 root tape -` in `/etc/tmpfiles.d/rem.conf` so boot recreates it. |
| `TailBeyondKnownState` | Tape holds complete data beyond local checkpoint authority, often after restoring old state. Restore newer state or rescue with `rem tape restore --scan --out rescued`. Do not delete journals. Only deliberately discard a rescued suffix with `rem tape supersede-tail --acknowledge-tape LABEL` and its exact confirmation. |
| `holds … data at the beginning` | Nonblank media needs `rem tape init --label LABEL --generation <yours> --clobber-data`, then `CLOBBER LABEL`. For non-interactive use, pass `--confirm-clobber "CLOBBER LABEL"`. This also applies after retirement. Clobber also erases a Remanence tape whose files are still in your catalog; run `rem list --tape LABEL` first. It does not bypass write protection, partition, label/identity-swap or cross-pool checks. |
| `has 2 partitions` | LTFS media needs repartitioning, which erases the cartridge. Install `mt-st` with `sudo apt install mt-st` (Debian/Ubuntu) or `sudo dnf install mt-st` (Fedora). Find `nstN` with `ls /sys/class/scsi_generic/sgN/device/scsi_tape/` using the drive's sg name. The listing includes the node name; use `/dev/<the listed nstN>` (for example `/dev/nst0`). Run `sudo mt -f /dev/nstN stsetoptions can-partitions`, then `mt -f /dev/nstN mkpartition 0`. Re-run `rem tape init --label LABEL --generation <yours>`; add `--clobber-data` only if it still reports data at BOT. See [step 2](guide-standalone-drive.md#2-initialize-a-cartridge). |
| Active tape-I/O quarantine | Run `rem tape quarantine list`. Reads remain available; rescue with `rem tape restore --scan --out rescued`. After investigating, use `rem tape quarantine release ID --ack "what was checked"`. Release does not bypass the physical append check. |
| Tape full / capacity refusal | Written receipts remain committed. Finalize, eject, initialize the next cartridge in the same pool and rerun put. Unchanged committed files skip. An uncertain I/O failure needs quarantine investigation first. |
| Ran with sudo / home mode refuses root | Never run rem with sudo. Use your normal account with the `tape` group and a capability on the binary. Check which state directory the earlier command used; preserve it before correcting ownership or choosing a config. Root's home state is not your user's state. |

The full setup and first-tape sequence is in [Using a standalone LTO
drive](guide-standalone-drive.md).

<!-- code-anchor: crates/remanence-library/src/handle/tape_io/readiness.rs crates/remanence-cli/src/lib.rs @ 6ed03ef0 -->
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

`rem tape verify-index --tape-uuid <UUID>` resumes a media-readiness fence
and waits up to `15m` before retrying verification. It prints each readiness
state change to stderr. Use `--ready-wait <DURATION>` to set the wait, or
`rem tape verify-index --tape-uuid <UUID> --ready-wait 0` to keep the immediate
refusal. At the deadline the command returns the daemon's original message.

Inspect with `rem tape quarantine list` / `show <id>`. Both print the
drive's last sense with its name, for example `sense=02/04/02 (initializing
command required)`; a drive that reports `02/04/02` is covered under [A drive
stays not ready after a daemon crash](#a-drive-stays-not-ready-after-a-daemon-crash).

If the fence's operation can still be resumed, resume it. Otherwise, once you
have verified the physical state (inventory settled, cartridge where the
library says it is), release it explicitly:

```sh
rem tape quarantine release <id> --after-settled-inventory \
    --ack "verified slot inventory after power event; RCA in ticket 123"
```

The `--ack` text is recorded. The fence exists so that nobody writes to a tape whose state was last seen
mid-uncertainty.

<!-- code-anchor: crates/remanence-library/src/handle/tape_io/readiness.rs @ 638210f7 -->
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
a restart. The daemon finishes every dismount it starts before it exits on
SIGTERM, SIGINT or SIGHUP, so give a stopping daemon time for a rewind, an
unload and a move before any hard kill.

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

<!-- code-anchor: crates/remanence-daemon/src/entry.rs crates/remanence-daemon/src/tls.rs crates/remanence-state/src/error.rs crates/remanence-state/src/config.rs @ 1dd451b2 -->
## The daemon refuses to start

`rem-daemon` checks its world in order and exits 1 with a specific
`error:` line for each failure. The ones worth knowing:

| Message | Cause and fix |
|---|---|
| `error: load config ...` | TOML parse or validation failure. Unknown keys are hard errors; all paths must be absolute; `listen` requires `[daemon.tls]`; `tape_io.legacy_single_block`/`tape_io.pipelined_submission` are not accepted keys and fail to load with a specific message; the pipelined path is the only path for streaming reads and writes, so delete the key. (Full verification reads one block per command and does not use it.) |
| `daemon.io_memory_ceiling must be non-zero` / `... exceeds daemon.io_memory_ceiling` (on `spool_tmpfs_ram_budget` or `tape_io.read_reservoir_bytes`) | The shared I/O memory budget is zero, or a per-purpose reservation exceeds it. `io_memory_ceiling` (default 24 GiB) is the hard cap shared by the append spool and every drive's read reservoir. |
| `tape_io reservoir watermarks require 0 < read_reservoir_low_pct < read_reservoir_high_pct <= 100` | Watermark percentages misordered or out of range. |
| `tape_io.staging_ring_buffers` validation failure | Must be in `2..=16`. |
| `untrusted state volume ...` | State, journal, audit, index, cache, or socket path sits on tmpfs/NFS/SMB/overlayfs and `journal.require_trusted_volume` is `true` (the default). Move the state or consciously disable the check. |
| `error: configure spool dir ...` | The spool resolved to tmpfs and `spool_tmpfs_ram_budget` is not set. Spooling to RAM needs an explicit budget acknowledgement. |
| `TLS private key ... has insecure permissions` | The key file is group- or world-accessible. `chmod 600` it. |
| `startup blocked by active tape-I/O fence <id> ...` | A write was interrupted in a completion-unknown state before shutdown. `rem tape quarantine release <id>` after verifying the tape, as the message says. |
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

<!-- code-anchor: crates/remanence-api/src/pool_write.rs crates/remanence-parity/src/error.rs crates/remanence-api/src/pool_write/selection.rs crates/remanence-api/src/write_admission.rs @ 1dd451b2 -->
## Writes are refused

Pool writes fail closed on a set of preconditions. The common refusals:

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
is fenced by quarantine ...` (see fences above), and `tape ... carries
committed data but no adopted checkpoint journal; batched append
positioning would be unsafe`.

<!-- code-anchor: crates/remanence-daemon/src/entry.rs @ 1dd451b2 -->
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

Strings worth grepping for, mapped to the sections above:

| String | Points at |
|---|---|
| `no tape libraries reachable` | permissions or no hardware |
| `every SCSI probe returned EPERM` | missing CAP_SYS_RAWIO |
| `was removed; the pipelined fixed-block path is now the only tape I/O path` | stale `tape_io.legacy_single_block`/`pipelined_submission` key in config |
| `blocked by active media-readiness fence` | quarantined media |
| `startup blocked by active tape-I/O fence` | interrupted write before restart |
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
