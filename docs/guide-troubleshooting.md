# Troubleshooting

A field guide to the failure modes Remanence surfaces on purpose. The
recurring theme: when the stack cannot prove what state the hardware is in,
it stops and says so rather than guessing. Most of what looks like an outage
is a fence doing its job.

<!-- code-anchor: crates/remanence-library/src/error.rs crates/remanence-cli/src/lib.rs crates/remanence-scsi/src/error.rs @ 7fb10f8 -->
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
second gate.

**The capability vanished after a rebuild.** File capabilities live in
xattrs on the binary's inode; `cargo build` replacing the binary drops
them. Use `make rem-dev` after rebuilding (it rebuilds and re-runs
`setcap`), and run the binary directly rather than through `cargo run`.
For the daemon under systemd, grant the capability in the unit instead:
`AmbientCapabilities=CAP_SYS_RAWIO` plus
`CapabilityBoundingSet=CAP_SYS_RAWIO`.

**could not enumerate /dev/sg\*.** Directory-level permission problem;
check that `/dev/sg*` nodes exist at all (no HBA, no VTL, or the module
is not loaded).

<!-- code-anchor: crates/remanence-library/src/handle/tape_io/readiness.rs crates/remanence-cli/src/lib.rs @ 7fb10f8 -->
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

Two practical notes from real hardware. First, an LTO-9 cartridge's first
load triggers a one-time media optimization pass that can run well over an
hour; this is documented drive behavior, not a hang, and it is why
`wait-ready` defaults to a 2.5h timeout with 30s polls. Second, a readiness
wait that gets interrupted (signal, crash) does not evaporate: it leaves a
durable operation record, and `rem tape wait-ready --resume <UUID>`
continues it without moving media.

When an operation ends in a completion-unknown or interrupted state,
Remanence writes a **quarantine fence** against that tape or drive. Fenced
media is refused for init, daemon writes and reads, and robotics dispatch
until the fence is cleared. You will see messages like:

```text
... is blocked by active media-readiness fence <id> operation=... state=...;
run `rem tape quarantine show <id>` or wait-ready/resume before retrying
```

Inspect with `rem tape quarantine list` / `show <id>`. If the fence's
operation can still be resumed, resume it. Otherwise, once you have
verified the physical state (inventory settled, cartridge where the
library says it is), release it explicitly:

```sh
rem tape quarantine release <id> --after-settled-inventory \
    --ack "verified slot inventory after power event; RCA in ticket 123"
```

The `--ack` text is recorded. The fence is deliberately annoying: its
whole purpose is that nobody writes to a tape whose state was last seen
mid-uncertainty.

<!-- code-anchor: crates/remanence-library/src/handle/mod.rs crates/remanence-library/src/handle/tape_io/mod.rs @ 7fb10f8 -->
## Dirty snapshots and completion-unknown

For every state-changing SCSI command, Remanence distinguishes "the device
refused" (CHECK CONDITION — state is known, nothing moved) from "the
transport failed" (timeout, kernel I/O error — the command may have
completed anyway). The second case marks the in-memory library snapshot
dirty with cause `CompletionUnknown`; a multi-step operation that failed
partway marks it `PartialFailure`. A dirty snapshot means: refresh or
`rem-debug rescan` before trusting either endpoint of the last move. The
error string to look for is `transport error (completion unknown)`.

<!-- code-anchor: crates/remanence-daemon/src/main.rs crates/remanence-daemon/src/tls.rs crates/remanence-state/src/error.rs crates/remanence-state/src/config.rs @ 7fb10f8 -->
## The daemon refuses to start

`rem-daemon` checks its world in order and exits 1 with a specific
`error:` line for each failure. The ones worth knowing:

| Message | Cause and fix |
|---|---|
| `error: load config ...` | TOML parse or validation failure. Unknown keys are hard errors; all paths must be absolute; `listen` requires `[daemon.tls]`. |
| `state lock is already held: <path>` | Another daemon (or a crashed one's live process) owns the state dir. One daemon per state dir. |
| `untrusted state volume ...` | State, journal, audit, index, cache, or socket path sits on tmpfs/NFS/SMB/overlayfs and `journal.require_trusted_volume` is `true` (the default). Move the state or consciously disable the check. |
| `error: configure spool dir ...` | The spool resolved to tmpfs and `spool_tmpfs_ram_budget` is not set. Spooling to RAM needs an explicit budget acknowledgement. |
| `TLS private key ... has insecure permissions` | The key file is group- or world-accessible. `chmod 600` it. |
| `startup blocked by active tape-I/O fence <id> ...` | A write was interrupted in a completion-unknown state before shutdown. `rem tape quarantine release <id>` after verifying the tape, as the message says. |
| `error: discover libraries: ...` | Same permission gates as the CLI (tape group + `CAP_SYS_RAWIO`, via systemd `AmbientCapabilities` for a service). |

<!-- code-anchor: crates/remanence-api/src/pool_write.rs crates/remanence-parity/src/error.rs @ 7fb10f8 -->
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

<!-- code-anchor: crates/remanence-daemon/src/main.rs @ 7fb10f8 -->
## Reading the logs

The daemon logs JSON to stderr, one flattened object per event, filtered
by the standard `RUST_LOG` variable (default `info`). Write-path events
carry `tape_uuid`, `pool_id`, and `session_id` fields, so `jq
'select(.tape_uuid=="...")'` reconstructs one tape's story. The CLIs do
not use the tracing stack at all: `rem` and `rem-debug` print
human-readable text, or stable JSON with `--json`.

Strings worth grepping for, mapped to the sections above:

| String | Points at |
|---|---|
| `no tape libraries reachable` | permissions or no hardware |
| `every SCSI probe returned EPERM` | missing CAP_SYS_RAWIO |
| `state lock is already held` | second daemon on one state dir |
| `blocked by active media-readiness fence` | quarantined media |
| `startup blocked by active tape-I/O fence` | interrupted write before restart |
| `transport error (completion unknown)` | dirty snapshot; rescan |
| `has insecure permissions` | TLS key mode |
| `read-only mode` | daemon started with `read_only = true` |

<!-- code-anchor: none -->
## Known open issue

Tape recycling outside Remanence (for example re-creating a virtual
cartridge with the same barcode) can leave the catalog's tape identity
out of step with the UUID written at the beginning of tape. The retire
and rebind machinery exists (`rem tape retire`); the automated
reconciliation is still open work. Details and current status:
[tape-recycle-identity-reconciliation-concern.md](tape-recycle-identity-reconciliation-concern.md).
