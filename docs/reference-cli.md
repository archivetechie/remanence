# CLI reference

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-cli/src/main.rs crates/remanence-cli/src/rem_debug.rs @ 6ed03ef0 -->
## The binaries

Remanence ships two command-line tools built from `crates/remanence-cli`,
the daemon binary from `crates/remanence-daemon`, and a standalone
disaster-recovery binary from its own crate `crates/rem-recover`:

- **`rem`** is the operator CLI. Library operations talk to a running
  `rem-daemon` over gRPC. Standalone-drive operations use the same command
  directly against one explicitly selected and authorized drive. It also
  works against local state or local files and cannot issue arbitrary SCSI
  commands.
- **`rem-debug`** is the break-glass CLI. It shares the discovery, local
  tape-lifecycle and local archive commands with `rem`, and adds direct SCSI
  robotics (`move`, `load`, `unload`, `export`, `import`, `rescan`, `lock`,
  `unlock`), direct tape read/write commands, destructive catalog
  maintenance, and development helpers. It has neither the standalone-drive
  commands nor the daemon client commands; those exist only in `rem`.
  [rem-debug extras](#rem-debug-extras) lists exactly which commands each
  binary has. The robotics, the direct tape-path `archive` commands, `dev`,
  `tape bot-probe` and `tape adopt-bootstrap` must name the target library
  in a `--allow <SERIAL>` allowlist; without it the command refuses to run.
  This exists because a partitioned chassis can expose another system's
  partition on the same host, and a mistyped element address must not be
  able to touch it.
- **`rem-daemon`** is the long-running Layer 5 service. It takes only
  `--config <PATH>` (default `/etc/rem/config.toml`) and `--socket <PATH>`
  (overrides the config's socket path).
- **`rem-recover`** is a small, standalone catalogless disaster-recovery
  tool — see [`rem-recover`: standalone recovery](#rem-recover-standalone-recovery)
  below. It shares no code path with the daemon or the CLI's `--allow`
  gauntlet: it doesn't touch tape, the catalog, or a config file at all,
  only a REM-OBJECT object file already sitting on disk.

`rem` and `rem-debug` are two separate binaries built from the same
`remanence-cli` crate. `rem` has a bounded direct-hardware surface for one
standalone drive; `rem-debug` carries the broader break-glass library and SCSI
surface. For library and daemon commands, `rem` accepts `--allow` and
`--allow-derived` as hidden compatibility flags and ignores them; for the
standalone-drive commands it rejects them as unexpected arguments.

Conventions shared across both CLIs:

- Daemon-backed commands take `--endpoint <URI>`, default
  `unix:/var/lib/rem/rem.sock`. TCP endpoints work with the daemon's mTLS
  listener.
- Daemon and library config-backed commands take `--config <PATH>`, default
  `/etc/rem/config.toml`. Standalone commands use that file when it exists;
  otherwise they create home-mode config and state under
  `${XDG_STATE_HOME:-$HOME/.local/state}/rem`.
- `--json` switches to stable, machine-readable JSON output intended for
  orchestrators and scripts.
- Tape-structural unsigned 64-bit values (file numbers, LBAs, block/row counts,
  scopes, and edition sequences) are canonical decimal JSON strings. This is a
  lossless contract through `u64::MAX`; consumers must not coerce them through
  an IEEE-754 number. Human output remains ordinary decimal text.
- Durations accept `ms`/`s`/`m`/`h` suffixes; sizes accept byte counts or
  `KiB`/`MiB`/`GiB`-style suffixes (powers of 1024).

Both binaries print full usage with `--help` at every level, and `--version`
(also `rem-daemon --version`) prints the package version. `rem-recover` prints
usage with `--help` and has no `--version` flag. This page lists the commands
and their main options; `--help` is the complete list.

### Placeholders

Angle brackets mark a value you supply. The names below are used throughout
this page.

| Placeholder | What to type, and where the value comes from |
|---|---|
| `<SERIAL>` | A library's serial number, as printed by `rem libraries`. |
| `<DRIVE_SERIAL>` | A standalone drive's VPD page 0x80 serial, as printed by `rem drives --json`. |
| `<ADDR>`, `<SLOT>`, `<BAY>` | A changer element address (for example `0x0400`), a storage-slot address, and a drive-bay address. `rem library <SERIAL> --slots` lists them. |
| `<BARCODE>` | The barcode printed on a cartridge, as shown by `rem library <SERIAL> --slots`. |
| `<LABEL>` | A standalone cartridge's operator label: free text chosen with `rem tape init --label`, unique in the catalog. It is not the tape's identity. |
| `<TARGET>` | A cartridge, given either as a barcode or as an element address, or as an inclusive slot range such as `0x0400..0x0407` where the command says a range is accepted. |
| `<UUID>`, `<TAPE_UUID>` | A tape's UUID, as printed by `rem catalog tapes`, or on a standalone drive by `rem tape identify` and in `rem put` receipts. Other `<UUID>` values are named where they appear (an operation UUID from `rem op list`, an object UUID from `rem put`). |
| `<DRIVE>` | A drive's serial number or UUID, as printed by `rem drive list`. |
| `<POOL>`, `<POOL_ID>` | The `id` of a `[[tape_pools]]` entry in the [configuration](reference-configuration.md#tape_pools-and-tape_pool_rules). In home mode the generated config defines `default`, and `rem pool new` creates further pools. |
| `<UNIT_ID>` | A catalog unit id, as printed by `rem catalog units`. |
| `<CONDITION_KEY>` | The key of a standing alarm, as printed by `rem alarms`. |
| `<ID>` (in `rem put`, `rem tape quarantine`) | For `rem put`, the caller's own identifier for the object. For quarantine commands, a quarantine id or operation UUID as printed by `rem tape quarantine list`. |
| `<PATH>`, `<FILE>`, `<DIR>` | A file path or directory path on the local machine. |
| `<REMP>`, `<REMR>` | A recipient private-key file and a recipient public-key file; see [Recipient key-file formats](#recipient-key-file-formats). |
| `<JSON>` | The locator JSON that a tape write (`rem-debug archive write`) returns, passed as the value of `--locator`. |
| `<MEMBER>` | A member's archive path inside the object, exactly as recorded in the catalog: the `objects[].archive_path` value that `rem put --json` prints (ingest strips a leading `/` or `./`, so `/data/file` is stored as `data/file`), or the member path in the object's manifest. |
| `--idempotency-key <UUID>` | A UUID you generate, not nil, that identifies one finalization request. Keep it and reuse the same value when you retry, so that the daemon deduplicates the request. It is not a tape UUID or an operation UUID. |
| `<DURATION>`, `<BYTES>` | A duration or a size, in the units described under the conventions above. |
| `<HEX>` | A SHA-256 digest written as 64 hexadecimal characters. |
| `<SG>` | The Linux SCSI generic device path of a tape drive, for example `/dev/sg3`. |

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-library/src/handle/tape_io/readiness.rs @ 6ed03ef0 -->
## Exit codes

Most subcommands exit 0 on success and 1 on failure (2 appears for a few
rejected-precondition cases and for clap usage errors). `rem-daemon` exits
1 on any startup or serve failure.

`rem tape wait-ready` and the readiness phase of `rem tape init` use a
finer taxonomy so scripts can branch on what the drive reported:

| Code | Meaning |
|---|---|
| 0 | Medium ready. |
| 10 | Retryable wait state (becoming ready, unit attention, target busy) — leave the cartridge alone and resume later. |
| 20 | Timed out while still retryable. |
| 30 | Terminal not-ready — operator intervention needed (no medium, repeated unit attention, check condition, reset required). |
| 40 | Transport outcome unknown — the command may or may not have completed; verify state before retrying. |
| 50 | Reservation conflict (another initiator owns the drive) or unresolvable resume target. |
| 130 | Interrupted by signal; a durable readiness record fences the operation for `--resume`. |

For `wait-ready --resume <UUID> --endpoint <URI>`, code 20 means the local
`--timeout` expires while the daemon wait continues; it does not cancel or shorten
the daemon's 9000 s wait. Codes 0, 30, 40 and 50 mean ready, terminal, transport
unknown and ownership respectively. A daemon readiness timeout also returns 20.
The resume JSON schema is `rem.tape.wait_ready_resume.v1`, with `operation_id`
and `exit_code`. The direct form retains the table above.

The direct form's `--json` output carries the same decoding in structured form
(`recommended_next_command`, `operator_action` fields).

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 6ed03ef0 -->
## Discovery and hot-plug

| Command | What it does |
|---|---|
| `rem libraries [--json]` | Discover every reachable tape library on this host: joins changer inventory (READ ELEMENT STATUS) with drive identities (INQUIRY + VPD 0x80) by serial number. |
| `rem library <SERIAL> [--slots] [--json]` | Focused view of one library: drives, loaded barcodes, and with `--slots` every storage slot. |
| `rem drives [--json]` | Discover drive endpoints, including unattached drives proved to be standalone. JSON reports `path`, serial, endpoint kind, and ownership. |
| `rem watch [--coalesce-window <DURATION>]` | Stream coalesced udev hot-plug events for SCSI tape/changer subsystems (default window 500ms). Requires a build with the `linux-udev` feature. |

Discovery needs read/write access to the tape and changer sg nodes, usually
through the `tape` group. Writes need `CAP_SYS_RAWIO`; changer inventory can
also need it for filtered SCSI commands. See the [quickstart](guide-quickstart.md)
and [troubleshooting](guide-troubleshooting.md).

<!-- code-anchor: crates/remanence-cli/src/standalone.rs crates/remanence-api/src/mount_backend.rs crates/remanence-library/src/discovery.rs @ 4dc2a3c6 -->
## Standalone drives

The standalone interface operates one manually loaded drive directly. It does
not need `rem-daemon`, a changer, or a `[[libraries]]` entry. The complete
walkthrough is [Using a standalone LTO drive](guide-standalone-drive.md).

Library operations go through the daemon; a standalone drive is operated
directly. Routing is part of the command interface, including help:

| Invocation | Route |
|---|---|
| Explicit `--device` | Standalone |
| Standalone-only verb (`setup`, `drives`, `doctor`, `diagnostics`, `list`, `pool new`, or a standalone-only tape verb) | Standalone |
| Shared verb with `--config`, `--endpoint`, `--library` or `--after-settled-inventory`, without `--device` | Library/daemon |
| Shared verb with none of those routing flags, no `/etc/rem/config.toml`, and home `[home] drive_serial` set | Standalone |
| Other shared invocations | Library/daemon |

Every device verb accepts `--device <SG> --allow-device <SERIAL|PATH>`.
Setup saves a home serial that supplies both defaults: discovery must find
exactly one matching path. It never authorizes a different explicit device.
Ownership and the serial read when opening the drive are still checked.
Unknown ownership is not permission to write. `[[standalone_drives]]` pins
known ownership but cannot override an unresolved or positive changer claim.

Global standalone flags are `--config <PATH>`, `--json`, `--reserve`,
`--allow-untrusted-state-volume`, `--acknowledge-unknown-ownership` and
`--allow-unserialized-device`. The last two permit reads only. `--reserve`
adds SCSI RESERVE(6)/RELEASE(6) for the invocation; the state-volume override
explicitly accepts a filesystem that fails the durability checks. Running
home mode as root is refused; use the permissions in the standalone guide.

The table omits the device/authorization flags, which all media verbs accept.

| Command | What it does |
|---|---|
| `rem setup [--yes] [--device <SG>]` | Run doctor and discovery, display the selected drive and save `[home] drive_serial` after confirmation. Incomplete discovery needs a separate ownership assertion; `--yes` accepts both. |
| `rem doctor [--json]` | Check effective group, binary capability, locks, state, discovery, fences and the memory-lock limit; the memory-lock check is advisory (marked `!`, never blocks readiness). Print a ready/not-ready verdict and fixes. |
| `rem drives [--json]` | List discovered endpoints with path, serial, model and ownership. |
| `rem list [--tape <LABEL\|UUID>] [--path-prefix <PREFIX>] [--json]` | List committed archive paths, version counts, newest UUID and tape locations; no device required. |
| `rem pool new <POOL> [--scheme <SCHEME>] [--block-size <BYTES>]` | Create a pool. Default scheme is `custom:128,4,1`; `--scheme default` uses the block-size-aware library geometry. |
| `rem tape init --label <LABEL> [--pool <POOL>] [--generation <GEN>] [--block-size <BYTES>] [--clobber-data [--confirm-clobber "CLOBBER <LABEL>"] [--clobber-unknown-tape <UUID>]]` | Initialize loaded media immediately, with no dry run. Default pool is `default`. Nonblank media, including retired media, requires clobber confirmation; other safety refusals still apply. A Remanence tape whose BOT UUID this catalog does not know is refused; adopt it with `rem tape adopt` (the default), or erase it with `--clobber-data --clobber-unknown-tape <UUID>` naming that exact BOT UUID, plus the clobber confirmation. The refusal prints the UUID to copy. A different UUID, or the flag on any other tape, refuses without writing. Supported GEN values are L5–L9 and M8. The drive's measured density wins and is recorded. Without `--generation`, a label ending in a generation code must agree with it. |
| `rem tape identify` | Read loaded tape identity and terminal summary. |
| `rem tape adopt [--label <LABEL>] [--pool <POOL>]` | Import bootstrap identity only; no object rows, no media rewrite. Use restore to list files without the original catalog. |
| `rem put <PATH>... [--pool <POOL>] [--recipient <REMR>]... [--tape <LABEL\|UUID>] [--allow-plaintext-copy]` | Recursively store regular files, one object per file. Pool must match loaded media. Tape selector guards identity. Plaintext over a newest encrypted version needs explicit `--allow-plaintext-copy`. |
| `rem get <OBJECT_UUID\|ARCHIVE_PATH> --out <DIR> [--version <OBJECT_UUID>] [--private-key <REMP>]` | Restore the selected object, or newest committed path version on the loaded tape. `--version` selects an older version. Stage on the destination filesystem and restore recorded modification times. |
| `rem tape verify [--full]` | Check terminal structure, optionally all sidecar-protected data and parity. `--full` prints the I/O report, with per-phase timing in `io` JSON and human-readable `io:` and `io phase …:` lines. |
| `rem tape restore [--out <DIR>] [--list] [--path <MEMBER>]... [--scan] [--private-key <REMP>]` | Catalogless listing or restore. Output is required unless listing. Repeat `--path` to select exact member paths; `--scan` walks from BOT instead of using the terminal index. Encrypted member listing needs a key. |
| `rem tape finalize --acknowledge-tape <LABEL\|UUID> [--tape <UUID>] [--expected-pool <POOL>] [--reason <TEXT>] [--idempotency-key <UUID>]` | Permanently close the loaded tape. Defaults are its UUID, catalogued pool, reason `finalized with rem`, and a stable UUIDv5 request key derived from the tape UUID and a fixed namespace. |
| `rem tape assign-pool [--tape <UUID>] (--pool <POOL> \| --unassign)` | With UUID, change assignment offline; otherwise identify loaded media. |
| `rem tape set-generation --generation <GEN>` | Record generation for loaded media; supported values are L5–L9 and M8. |
| `rem tape rebind --label <LABEL> --previous-uuid <UUID>` | Bind a reused operator label to loaded media, checking the previous UUID. |
| `rem tape import-journals --from <DIR>` | Validate and import matching checkpoint/parity journal authority for append recovery. |
| `rem tape supersede-tail --acknowledge-tape <LABEL\|UUID> [--confirm-supersede "SUPERSEDE <LABEL>"]` | List and explicitly discard the suffix beyond local authority, with typed confirmation and audit. Rescue files first. |
| `rem tape quarantine list` | List local active tape-I/O fences. |
| `rem tape quarantine release <ID> --ack <TEXT>` | Release through the configured daemon socket when it is listening; otherwise use the exclusive offline state lock. The acknowledgment is durable before the fence clears. This does not bypass append admission. |
| `rem tape release` | Clear medium-removal guard only; quarantine remains. |
| `rem diagnostics capture --out <FILE.tar> [--device <SG> --allow-device <SERIAL>]` | Capture diagnostics without reading tape data or positioning media. |

Aliases accepted by both parsers are `--dest` for `--out`, `--tape-uuid` for
`--tape`, and `--ack-tape-uuid` for `--acknowledge-tape` on verbs with those
options. The library parser retains its original canonical spellings and
accepts the corresponding shorter names. Aliases do not make the library
finalize defaults or label handling the same as standalone.

Put compares size and SHA-256 against the newest committed version in the
pool, ordered by creation time and then object row order. Equal newest
content skips; a change writes a new UUID, even when it matches an older
version. Receipts are `written`, `already_stored`, `skipped_unsupported`, or
`refused`; the summary names failures and counts committed files. On capacity
refusal it tells you to finalize, initialize the next tape in the same pool,
and rerun put. `--json` streams receipts and a summary, then an envelope with
all receipts. JSON envelopes carry `supported_endpoint_kinds` and
`observed_endpoints`.

Reads report active tape-I/O fences and remain available. Writes refuse fences
on the loaded tape or endpoint. Append admission also checks the physical tail:
`TailBeyondKnownState` means local state cannot authorize overwriting complete
data. Restore newer state or rescue with `tape restore --scan` before considering
supersede-tail.

### Which verification command?

| Command | What it reads | Existing catalog needed? | Daemon? | Duration |
|---|---|---|---|---|
| Standalone `tape verify` | Bootstrap and terminal structure/index | No object catalog | No | Usually minutes; positioning dominates |
| Standalone `tape verify --full` | Full structural walk, all sidecar-protected data and parity | No object catalog | No | Hours on a full tape |
| Library `tape inventory --tape-uuid <UUID>` | Bootstrap and usable terminal index; BOT walk if no index works | Yes, for tape selection | Yes | Usually minutes; fallback can take much longer |
| Library `tape verify-index --tape-uuid <UUID>` | Full structural walk, all sidecar-protected data and parity | Yes, for tape selection | Yes | Hours on a full tape |

On a tape without parity, the full structural check does not checksum object
interiors: there are no sidecar-protected blocks. Inventory is not a content
verification. Standalone verify still opens local operational state, but does
not require catalogued object locations.

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 6ed03ef0 -->
## Daemon queries

All of these speak gRPC to `rem-daemon` and take `--endpoint` and `--json`.

| Command | What it does |
|---|---|
| `rem daemon health` / `rem daemon version` | Liveness and version of the daemon and API. |
| `rem op list` / `rem op get <UUID>` | Enumerate or inspect tracked daemon operations. |
| `rem operation watch <UUID>` / `rem operation cancel <UUID>` | Stream progress until completion or request cooperative cancellation. An in-flight tape command finishes before cancellation takes effect. |
| `rem catalog tapes [--pool <POOL>] [--kind data\|cleaning\|all]` | List cataloged tapes, optionally per pool. `--kind` defaults to `data`; `all` includes data and cleaning tapes. |
| `rem catalog tape <TAPE_UUID>` | One tape's catalog record. |
| `rem catalog tape-files <TAPE_UUID>` | Tape files recorded for one tape. |
| `rem catalog pools` / `rem catalog pool <POOL_ID>` | Tape pool definitions and membership. |
| `rem catalog units [--origin all\|native\|foreign]` | Catalog units across native REM-OBJECT objects and foreign (scanned legacy) archives. |
| `rem catalog unit <UNIT_ID>` / `rem catalog entries <UNIT_ID>` | One unit, or the entries inside it. |
| `rem top [--once] [--json]` | Live TUI over daemon state; `--once` takes a single snapshot. In the TUI, `l` cycles the focused library when more than one is configured, `s` toggles the slot view, `p`/space pauses, and `?` shows help. |
| `rem alarms [--all]` / `rem alarms ack <CONDITION_KEY>` | List standing alarms (with `--all`, cleared ones too) or acknowledge one. |
| `rem audit query --since <RFC3339> --until <RFC3339> [--filter <K=V>]...` | Stream audit-log entries in the half-open interval from `--since` (inclusive) to `--until` (exclusive). Each `--filter` is an exact-match `key=value` filter; repeat it for several fields. `--json` prints one JSON object per entry. |

Note: `GetLiveStatus` also carries an advisory `drive_assignments`
projection (per-bay busy/idle state, keyed `(library_serial, bay)`) for
an external arbitration client — `rem top` does not render
this field; it is available only over gRPC.

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 6ed03ef0 -->
## Drive stewardship

Drive-fleet management through the daemon. A drive is addressed by serial
or UUID.

| Command | What it does |
|---|---|
| `rem drive list [--foreign] [--retired]` | List cataloged drives. |
| `rem drive show <DRIVE>` | One drive's stewardship record. |
| `rem drive history <DRIVE> [--events] [--snapshots]` | Lifecycle history, optionally with observational events and health snapshots. |
| `rem drive alerts <DRIVE>` | Active TapeAlert flags. `rem tape alerts <DRIVE>` is a deprecated alias with the same arguments; it has no `--bay`. Reading the TapeAlert page directly by drive bay is `rem-debug tape alerts --bay <BAY>`. |
| `rem drive poll <DRIVE>` / `rem drive clean <DRIVE>` | Poll health now, or run a cleaning cycle now. |
| `rem drive annotate <DRIVE> [--purchase-date <YYYY-MM-DD>] [--warranty-until <YYYY-MM-DD>] [--cost <TEXT>] [--note <TEXT>] [--notes-set <TEXT>] [--allow-derived-identity]` | Record the purchase date, the warranty end date, a display-only cost string, and notes. `--note` appends a timestamped note; `--notes-set` replaces the notes. |
| `rem drive retire <DRIVE> --reason <TEXT> [--allow-derived-identity]` | Remove a drive from the managed fleet. A retired drive keeps its identity and still appears in `rem top` marked retired, but is excluded from every selection and mount path. Reversible with `reinstate`. |
| `rem drive reinstate <DRIVE> --reason <TEXT> [--allow-derived-identity]` | Return a retired drive to service. Records the reason alongside the retirement it reverses. Does not clear `fenced` or restore `actionable` — those are separate judgements. |
| `rem drive load --library <SERIAL> --bay <BAY> (--barcode <BARCODE> \| --slot <SLOT>) [--no-wait]` | Through the daemon, load a tape into a drive bay and wait for completion. The tape is named by barcode, found in the current slot inventory, or by its source slot address. `--library` takes a library serial or UUID. With `--no-wait` the command returns when LOAD completes, without waiting for the cartridge to become ready. |
| `rem drive unload --library <SERIAL> --bay <BAY> [--dest <SLOT>]` | Through the daemon, unload the cartridge in a drive bay to a storage slot and wait for completion. Without `--dest` the cartridge goes back to the source slot the changer recorded for it. `--library` takes a library serial or UUID. Before the cartridge leaves the drive the daemon closes any parity epoch the drive retains and reports `phase=epoch_close` in the operation progress while it does. If the close cannot run, for example because the tape is fenced, the daemon logs the reason and removes the cartridge with the epoch still open; the operation can still succeed. The daemon refuses with `failed_precondition` while a session or another operation holds a drive in the library; an empty bay or a full destination fails the operation. Either way the command prints the daemon's message and exits 1. On success it prints the operation and then `unloaded <BARCODE> from bay <BAY> to slot <SLOT>`; with `--json`, the `rem.operation.status.v1` item carries a `location` object with `library_serial`, `bay`, `barcode` and `slot`. A retry sends a new request. `rem-debug unload` is the break-glass form that bypasses the daemon. |

`--allow-derived-identity` permits changing a drive row whose identity was
derived rather than read from the device.

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 4dc2a3c6 -->
## Tape lifecycle

Initialization, readiness, and retirement run against local
config/state and real hardware; they are gated by what the code calls the
destructive-safety gauntlet — a chain of identity, ownership, and
data-presence checks that each initialization must pass. In library mode,
finalization and supersede-tail go through the daemon and its tape ownership
and fencing checks. The CLI performs no local SCSI discovery for these verbs.

| Command | What it does |
|---|---|
| `rem-debug --allow <SERIAL> tape bot-probe <BARCODE> --library <SERIAL> --expected-home-slot <ADDR> --json [--config <PATH>]` | Advisory identity-only BOT probe. Under one stable drive handle it verifies readiness, temporarily selects fixed 1 MiB/compression-off read mode, issues exactly one LOCATE(0) and at most one READ, restores the prior drive mode, and parks the cartridge in the exact expected home slot. It never reads past BOT or consults/mutates the catalog. JSON schema is `rem.tape.bot-probe.v1`; a valid native identity uses `physical_disposition="bootstrap_valid"`. See [Importing and recovering Remanence tapes](importing-and-recovering-remanence-tapes.md). |
| `rem-debug --allow <SERIAL> tape adopt-bootstrap <BARCODE> --library <SERIAL> --expected-home-slot <ADDR> --expected-existing-tape-uuid <UUID> --json [--config <PATH>]` | Re-read and adopt one checksum-valid existing native bootstrap without writing tape. The command holds the exclusive `StateHandle`, performs fresh discovery, verifies the exact UUID/home/library and canonical 1 MiB default-parity geometry, restores drive mode, parks the medium, then records a durable `TapeIdentityAdopted` event and an identity-only catalog row. Exactly bootstrap→filemark→EOD becomes `ready`; every other tail after a valid bootstrap becomes `recovery_required`. The command generates its internal operation UUID; a retry may no-op only while the replayed row remains the exact same identity-only state. JSON schema is `rem.tape.adopt-bootstrap.v1`. See [Importing and recovering Remanence tapes](importing-and-recovering-remanence-tapes.md). |
| `rem-debug tape recovery-report <SOURCE> [--json] [--tape-uuid <UUID> --block-size <BYTES> --scheme <k,m,S\|none>]` | Produce a read-only recovery report for a tape without using the catalog. `<SOURCE>` is either the SCSI generic path of the drive that holds the tape (for example `/dev/sg3`) or a directory that holds a tape image in the published layout, with files named `tape-file-NNN-*.bin`. It is not a tape UUID, and a single image file is not accepted. The three options are hints used when the tape's bootstrap cannot be read. `--tape-uuid` is the tape's UUID (for example from `rem catalog tapes`); `--block-size` is the tape's fixed block size in bytes (256 KiB, 512 KiB or 1 MiB; KiB and MiB suffixes are accepted); `--scheme` is the parity scheme as three numbers `k,m,S` (data blocks per stripe, parity blocks per stripe, stripes per neighborhood, for the scheme `rs-cauchy-gf256-v1`), or `none` for a tape written without parity. Supply all three or none. When the bootstrap is readable, leave them out. The report performs terminal discovery first, through the same library entry as the daemon's inventory, and walks from BOT only when discovery yields no inventory. What the command prints when it cannot use the bootstrap is under [Recovery report and verification output](#recovery-report-and-verification-output). Bootstrap-derived report fields are null when unreadable; hints are recorded separately under `supplied`, and the scan records when they provided identity and geometry because the bootstrap was treated as unreadable. A valid terminal replica can supply Object identifiers, while BOT fallback without separate host authority reports complete candidates as unknown. It does not adopt the tape or rebuild SQLite. |
| `rem tape init <TARGET> [--dry-run] [--force] [--clobber-data [--confirm-clobber "CLOBBER <LABEL>"] [--clobber-unknown-tape <UUID>]] [--block-size <BYTES>] [--parity <none\|default>] [--library <SERIAL>]` | Initialize one tape (by barcode or element address) or an inclusive slot range like `0x0400..0x0407`. A real (non-dry-run) init runs the library's startup readiness reconcile first; it may send one initializing SSC LOAD per operation to a fenced drive and wait up to 60 s for it. `--dry-run` runs every check and writes nothing. `--force` overrides only decisions classified as `RequireForce`; `--clobber-data` is the separate, stronger override for tapes that hold data, and is rejected for dry-run and batch init. Confirm with `--confirm-clobber "CLOBBER <LABEL>"` or type/pipe the same text on stdin. Retired nonblank cartridges need this confirmation too. A Remanence tape whose BOT UUID has no catalog row decides `needs-explicit-rebuild` and writes nothing; neither `--force` nor `--clobber-data` alone overrides it. Adoption by catalog rebuild (`rem-debug tape adopt-bootstrap`) stays the default, and the refusal's `next:` line names both that command and the override. To destroy the identity instead, pass `--clobber-unknown-tape <UUID>` with `--clobber-data` and the confirmation; the UUID, in 32-hex or hyphenated form, must equal the BOT UUID exactly, or init refuses and names both. The override never relaxes any other decision, refuses when committed copies still reference that UUID, re-reads BOT just before writing and refuses unless it still carries that UUID, and is rejected for dry-run and batch init. The tape gets a fresh UUID; the output's `clobbered:` line and the `TapeProvisioned` audit record (`clobbered_identity = "unknown-to-catalog"`, `clobbered_uuid`) record the destroyed identity. Fresh media defaults to parity-off; `--parity default` writes the block-size-aware default parity geometry. An idempotent initialization preserves the geometry already recorded at BOT. |
| `rem tape wait-ready [--barcode <BARCODE> \| --drive-element <ADDR>] [--already-loaded] [--wait] [--timeout <DURATION>] [--poll <DURATION>] [--resume <UUID>] [--endpoint <URI>] [--library <SERIAL>]` | Poll TEST UNIT READY until already-loaded media is usable. LTO-9 first loads can take hours (media optimization); the default timeout of 2.5h exists for that, and the default poll interval is 30s. With `--wait`, a drive that asks for an initializing command (`02/04/02`) for a cartridge the changer places in it gets one SSC LOAD per operation; polling then continues. `--resume` continues a durable readiness operation without moving media. With `--endpoint`, it calls the daemon and attaches to its running wait, prints each readiness state change to stderr, and leaves the daemon waiting if the local `--timeout` ends. `--endpoint` requires `--resume`. Without it the command uses direct SCSI and requires the daemon to be stopped. `--library` selects a configured library. |
| `rem tape quarantine list [--library <SERIAL>]` | List active media-readiness fences, each with its last sense and the sense name when known, for example `sense=02/04/02 (initializing command required)`. |
| `rem tape quarantine show <ID>` | One fence, by quarantine id or operation UUID, with its phase and last sense. JSON carries `last_sense` (`KK/AA/QQ`) and `last_sense_name` beside the raw sense fields. |
| `rem tape quarantine release <ID> --ack <TEXT> [--after-settled-inventory]` | Release a tape-I/O fence through the configured daemon socket after operator acknowledgment. Only an absent listener permits offline fallback, which requires the state lock. Repeating a release returns its original evidence. Media-readiness release keeps its existing offline behavior. |
| `rem tape retire <TARGET> --reason <TEXT> --i-understand-copies-become-unreadable [--dry-run]` | Permanently retire a tape identity in the local catalog. Every copy on that tape becomes unreadable through the catalog. |
| `rem tape finalize --tape-uuid <UUID> [--expected-pool <ID>] --reason <EXACT> --idempotency-key <UUID> --ack-tape-uuid <UUID> [--wait] [--json] [--endpoint <URI>]` | Irreversibly finalize one exact reconciled tape, including below the automatic low watermark. Voltags are not accepted, the acknowledgement must name the same UUID, and a pooled tape requires its canonical current pool id. The reason is sent exactly as supplied. Without `--wait`, the command submits once and prints the current status; with it, the command polls the read-only status RPC across daemon reconnects until a terminal outcome and never resubmits finalization. While the projection is FINALIZING it also checks the operation and exits non-zero with its summary on FAILED, CANCELLED or COMPLETION_UNKNOWN. Reuse the same idempotency key to resume durable intent under the same operation id. JSON uses schema `rem.tape.finalization.v2`; `replica_progress` is historical barrier evidence, not a current media-health observation. A value the daemon does not have is `null`, never `0`: `operation_id` is `null` for a finalization an automatic trigger started, and a BUSY response has `null` `operation_id`, `completed_replicas` and digests. `completed_replicas` `0` is a real value (before replica A). |
| `rem tape supersede-tail --tape-uuid <UUID> --ack-tape-uuid <UUID> [--confirm-supersede <TEXT>]` | Through the daemon, list and explicitly discard a suffix beyond checkpoint authority using the same append checks. Rescue first; confirm the exact text requested. |
| `rem tape inventory --tape-uuid <UUID> [--json] [--endpoint <URI>]` | The quick check of a tape's structure. When the terminal index is readable, it reads the first record of the tape and the terminal replicas at the end of the data, and no Object data. If the terminal index cannot be used, it falls back to a walk from the beginning of tape that reads records throughout the prefix; that walk classifies tape files and is not a content verification. `verify-index` below is the whole-tape check. Stream the complete bounded terminal inventory by locating EOD and selecting C, then B, then A. Human output labels every pre-summary row `provisional`; the final summary names the authoritative `attempt_id`. `--json` emits NDJSON events under `rem.tape.inventory.stream.v1` and ends with exactly one `summary` event. Consumers must commit only that summary's selected attempt. Rejected attempts make fallback evidence explicit; no surviving replica triggers a `bot_recovery_started` notice followed by per-tape-file `bot_recovery_progress` events and streamed BOT classifications rather than empty success. Cancellation is honored at those between-file boundaries. During BOT fallback, an exact surviving checkpoint journal can classify matching complete Objects as recovered; later or foreign Objects without that authority remain unknown. In the summary, a selection, count or digest the outcome does not carry is `null`, never `"0"`: BOT-recovery-required has no counts, fast outcomes have no BOT classification counts, and a torn BOT Object's `stored_block_count` is `null`. |
| `rem tape verify-index --tape-uuid <UUID> [--ready-wait <DURATION>] [--json] [--endpoint <URI>]` | Watch a media-readiness fence locally for up to `15m`, then retry verification once it reports ready; `--ready-wait 0` returns the refusal immediately. The daemon resumes with a 9000 s timeout independent of `--ready-wait` and keeps waiting after the command returns, so the fence clears when the drive becomes ready within that timeout; an operator release is required if the daemon wait times out. Resume attaches to the same running readiness operation. If readiness fails or the resume or watch fails, return the original refusal with the wait error appended after a semicolon. A busy drive bay gets one retry after 1 s. Verify the tape's structure and, on a tape written with parity, read and checksum all the data that the parity sidecars protect and all the parity shards. On a tape written without parity there is no sidecar-protected content, so this pass does not read or checksum Object interiors; only the structure and the terminal index are verified. It has no shallow option; `rem tape inventory` is the quicker structural check, and it does not verify the protected content. Use `verify-index` for the periodic check of an archive and for any tape that is suspect. Perform a full verification in the sense of REM-PARITY Section 2.2: measure EOD, walk the prefix, compare the canonical map, validate all three replicas and both separation extents, and read every data block that a sidecar protects and every parity shard, checking each against its sidecar's index. A finding about any data block, parity shard, sidecar copy or footer, ParityMap copy or footer, or the bootstrap's trailing fill leaves the tape not complete: the state is `verified_degraded` and `complete` is `false`. `verified` says that the verification ran to the end; `complete` says whether the tape passed. The findings are printed as described under [Recovery report and verification output](#recovery-report-and-verification-output). See [Full verification](guide-full-verification.md) and [Damaged tapes](guide-damaged-tapes.md). Its all-replicas-invalid BOT outcome applies the same checkpoint-assisted recovered/unknown/incomplete classification as inventory. `verified_interior_record_count` is present only for a valid separation extent (`"0"` for a two-record extent) and `null` for an invalid or unknown one; the verified-prefix counts and digests are `null` when recovery is required. |
| `rem tape verify-index --tape-uuid <UUID> --detach [--json]` | Start a `tape_verification` operation and print its UUID. Use `rem operation watch <UUID>` and `rem operation cancel <UUID> [--force]` to control it. Cancellation sends `force=false` unless `--force` is supplied. |
| `rem tape verify-index --result <OPERATION_ID> [--json]` | Fetch the retained verdict with the verification rendering. The daemon retains results in memory alongside operations; a restart or pruning removes them. A running, failed or cancelled operation has no verdict. |

Full verification keeps running when its client disconnects. Cancellation releases
the tape and drive and records no verdict. A cancelled verification starts over;
pause and resume are unavailable. The daemon does not preempt verification for
writes or restores. Full verification progress includes tape UUID, drive serial,
bay, phase, tape file, bytes read and elapsed milliseconds. Unknown positions are
`unknown`. Phase changes publish immediately; other updates publish at most once
per five seconds. An in-flight READ can wait for its 900-second command timeout.
`rem catalog tapes` includes `last_verification_state` (the state name) and
`last_verified_edition_digest` (hexadecimal) in JSON, plus `last_verified_at` in JSON and table output; an absent
value means no full verification verdict is recorded. The catalog also records
the verdict state and the terminal edition digest when available.


<!-- code-anchor: crates/remanence-parity/src/scan.rs crates/remanence-parity/src/error.rs crates/remanence-parity/src/bootstrap.rs crates/remanence-cli/src/recovery_report.rs crates/remanence-cli/src/tape_inventory.rs crates/remanence-parity/src/verify_protected.rs @ c80a553a -->
### Recovery report and verification output

`rem-debug tape recovery-report` treats the bootstrap as described below,
following REM-PARITY Section 8.4.

A bootstrap that cannot be read is treated as unreadable, and the report then
uses the three supplied values. The cases are a medium error, a filemark or EOD
where the record should be, a missing magic, a failed header CRC, a failed
payload CRC, a payload length that runs past the block, and a payload that
breaks a later rule of Section 8 when no value that can still be decoded
disagrees with the supplied ones. Nothing is printed for these cases beyond
the report's own note that the bootstrap was treated as unreadable. Without the
three values the command exits with status 1 after printing:

```text
error: catalog-less recovery report: discover bootstrap: bootstrap not found anywhere on tape; unreadable bootstrap requires --tape-uuid, --block-size and --scheme
```

The specification calls this outcome `NoBootstrapFound`. The same line is
printed when the first record does not parse as a bootstrap and no values were
supplied.

A bootstrap that reads is refused when its first record has the wrong length
for the supplied block size, when its header CRC is valid and a header field is
impossible or contradicts the supplied values (even if its payload is damaged),
or when a scheme record that can still be decoded disagrees. The refusal ends
discovery, no report is produced, and the command exits with status 1
after printing:

```text
error: catalog-less recovery report: discover bootstrap: bootstrap refused: DETAIL
```

DETAIL names what disagreed. The specification calls every such refusal
`BootstrapParse`. The texts are:

```text
short fixed-block bootstrap read: got N bytes, supplied block size is M
bootstrap block larger than supplied block size: got N bytes, expected M
readable bootstrap block size differs from supplied hints: got N bytes, expected M
unsupported bootstrap schema major version: got N, accept 2
tape identity mismatch: readable bootstrap header differs from supplied hints
readable bootstrap block size differs from supplied hints
schema-major 2 permits only the sequence-0 BOT Bootstrap: got sequence N
readable bootstrap parity scheme differs from supplied hints: no-parity flag contradicts scheme
readable bootstrap parity scheme differs from supplied hints
```

A no-parity bootstrap whose payload carries a scheme record is refused in one
of these two ways. If you supplied a parity scheme, DETAIL is `readable
bootstrap parity scheme differs from supplied hints: no-parity flag contradicts
scheme`. If you supplied `--scheme none` and the record can be decoded, DETAIL
is `readable bootstrap parity scheme differs from supplied hints`. If the
record cannot be decoded, the bootstrap is treated as unreadable.

A decodable `drive_compression` of true in the bootstrap of a parity tape is
refused with the following line, which the specification calls
`DriveCompressionEnabled`:

```text
error: catalog-less recovery report: discover bootstrap: tape's bootstrap records drive compression; a parity tape must not record drive compression
```

When terminal discovery succeeds, the report does not list the walk's prefix
damage: `scan.damaged_regions` is empty. A full verification (`rem tape
verify-index`) reports that damage as `PREFIX_DAMAGE` findings.

`rem tape verify-index` prints one `protected_content_finding: KIND [address]
detail` line for each finding, after `verification_basis` and before
`measured_eod_lba`, in the verified forms only. KIND is one of
`PROTECTED_CONTENT_FINDING_KIND_DATA_BLOCK`, `_PARITY_SHARD`, `_SIDECAR`,
`_PREFIX_DAMAGE`, `_NOT_PERFORMED` and `_PARITY_MAP`, each printed in full with
the `PROTECTED_CONTENT_FINDING_KIND` prefix. The address is empty for
`PREFIX_DAMAGE` and `NOT_PERFORMED`. With `--json`, the same findings are in
`protected_content_findings`, an array of `{kind, address, detail}` objects
with `address` `null` where there is none, in both the verified and the
recovery-required envelope. The array is `[]` when the protected content
verified clean.

The verification JSON also carries `io`, a diagnostic object under the same
`rem.tape.verify-index.v1` schema. It is present for complete, degraded and
recovery-required results when the daemon collects it, and `null` when the
daemon supplies none. Human output has one `io:` summary line and one `io phase`
line per phase that issues a command, with elapsed seconds, seconds inside
commands, command count and slowest-command seconds. Counts, milliseconds and
physical addresses in JSON are decimal strings. These measurements do not
change the outcome or findings.

`io.total` and each entry of `io.phases[].counts` contain attempted `locate`,
`space`, `read_position`, `single_read`, `batch_read` and `other` commands;
`records_requested` and `records_confirmed`; transport-reported `bytes`;
`hardware_batch_fallbacks`; `algorithmic_fallback_epochs`; and `recovered_reads`.
The same objects carry these timing fields:

- `elapsed_ms`: wall milliseconds while the phase is current, excluding nested phases; `total` covers the whole collection.
- `command_ms`: wall milliseconds inside transport dispatches, including unsuccessful attempts.
- `max_command_ms`: milliseconds in the slowest command.
- `command_latency_buckets`: six command counts for <5, [5,20), [20,100), [100,1000), [1000,10000), and >=10000 ms, in that order.

Per-phase time inside commands (`command_ms`) against time between commands
(`elapsed_ms - command_ms`) is the split operators use to tell drive motion
and transfer waits from host work on the drive actor's thread. These wall times
include transport overhead and scheduling waits. The daemon logs
`remanence_verify_diag` INFO events per phase with commands and for `total`.

Inline positioning commands and retries count. Fixed-mode reads of one record
count as batches. Unclean batches contribute requested records and reported
bytes, but only their successful single-record retries confirm records.
`other` includes setup and readiness commands. Bytes include configuration
responses and unconfirmed transfers; errors without a byte count contribute no
known bytes. Hardware fallback counts an unclean batch selected for single
retry, even if repositioning fails. Algorithmic fallback counts an epoch needing
unbuffered data reads, independently of hardware retries.

The phases are `setup`, `walk_terminal`, `parity_map`, `sidecar`,
`protected_data`, `fallback_data` and `bot_recovery`. They attribute each command
once, to the innermost active boundary. `elapsed_ms` measures daemon elapsed
time from readiness through completion with a monotonic clock. It includes
computation and excludes earlier drive queueing and client overhead.
`backward_data_seeks` counts protected-sweep LOCATE targets before the previous
confirmed protected read's end position. `protected_bytes` and
`protected_ranges` describe confirmed forward-sweep coverage, excluding
fallback reads. Ranges use inclusive `start_lba` and exclusive `end_lba`;
overlaps and duplicates are retained. `recovered_lbas` lists accepted
self-corrected read addresses; `recovered_unknown_addresses` counts events
without a known address. Together they account for `total.recovered_reads`.
See [The I/O report](guide-full-verification.md#the-io-report) for the phase
boundaries and interpretation of coverage.

An exact self-corrected single read is accepted during verification when it has
no incorrect-length, filemark, end-of-medium or residual anomaly. It is counted
and does not itself fail the tape. Its bytes still undergo the normal checksum
checks, so a mismatch remains a finding. No extra drive commands are issued for
measurement. An RPC error produces no verification report.

Nonzero trailing fill after the bootstrap payload is reported as a
`PREFIX_DAMAGE` finding. A bootstrap that was treated as unreadable is not
reported for fill, because its bytes were not read as a bootstrap; a bootstrap
that validates, including under supplied or catalog values, is checked for
fill.

<!-- code-anchor: crates/remanence-cli/src/put.rs @ 244bc6de -->
## Writing to tape

In library mode, `rem put` archives local files onto tape through the daemon —
the same Layer 5 write-session path the orchestrator uses, so pool policy,
capacity reserve, media-readiness fences, and checkpoint durability apply
unchanged. It takes `--endpoint` and `--json` like the other daemon-backed
commands. With `--device` and `--allow-device`, the same command instead uses
the bounded [standalone-drive path](#standalone-drives), which carries the
write fence, audit, checkpoint, and catalog machinery into one direct
invocation without starting the daemon. The loaded cartridge must be
positively reported as rewritable (`NotWorm`); WORM or unknown media are
refused before drive mode is changed because whole-Object restart recovery may
need to replace an uncommitted tail.

One input file becomes one object whose single member carries the file's
archive path; directories are walked recursively, `tar`-style, one
object per file. Archive-path collisions, `..` components, and non-UTF-8
paths are refused; non-regular files are skipped with a warning.

| Command | What it does |
|---|---|
| `rem put <PATH>... [--pool <POOL_ID>] [--library <SERIAL>]` | Write each input file as one object to a tape pool. With no `--pool` and exactly one configured pool, that pool is used; with several, the choice is real policy and must be explicit. `--library` restricts selection and mounting to that configured library; without it, the daemon may choose an eligible cartridge in any configured library. The selected tape's barcode must appear in exactly one operated library before robotics begins. The daemon mounts it into a free drive in that library and seals tapes by watermark. If the open is fenced by a media-readiness operation (a cold tape load), put watches it to completion and retries once. |
| `rem put <PATH>... --tape <UUID> --pool <POOL_ID>` | Pin a specific cartridge instead of letting pool policy choose. `--pool` is the mandatory guard: the pool you believe that tape belongs to; `--tape` and `--library` are mutually exclusive, since pinning a tape already implies its library. Pinning replaces pool *selection*, never *admission* — the tape still passes every pool-mode eligibility check (data kind, lifecycle state, block size, io fences, checkpoint-batch eligibility), and a pool mismatch is a refusal naming both pools, because pools carry copy-class segregation and a silent cross-pool write is policy corruption. A cartridge that was never initialized has no UUID to pin: tape identity is minted by `rem tape init`. |
| `rem put ... [--id <ID>] [--meta <KEY=VALUE>]...` | Caller identity and opaque metadata recorded in the catalog. `--id` needs exactly one input file; the default caller id is the member's archive path. The `path` metadata key is reserved (it carries the archive path). |
| `rem put <OBJECT> --stored-object --stored-object-id <UUID> --id <ID> [--pool <POOL_ID>]` | Validate one already-built plaintext REM-OBJECT, then write its fixed blocks byte-for-byte through the normal daemon write path. The UUID and caller id must match the identities embedded in the object. The object must use the selected tape block size, exactly match the deterministic writer output, contain a valid final manifest and regular payload members, and have canonical zero fill after tar EOF. It is fully spooled and validated before tape moves; it is never wrapped inside another REM-OBJECT. An exact `(pool, caller id, canonical input kind, content, stored representation)` retry is idempotent; changing input kind or stored representation under the same replay key is refused both before and after checkpoint. Canonical bytes already bind their member paths, so an outer archive-path value is irrelevant for this input kind. This append surface refuses a UUID that already exists outside that exact replay key rather than attempting to attach an additional copy after media motion. |
| `rem put ... [--chunk-bytes <BYTES>] [--no-wait]` | Append stream chunk size (default 1 MiB), and `--no-wait` to fail fast instead of watching a media-readiness load. |

The ordinary command deliberately treats each input file as a separate Object,
so each receives its own trailing tape filemark. To store many small files with
one filemark, first build one multi-member REM-OBJECT with `rem archive build`,
then use `--stored-object`, which is a library-mode option; standalone `put`
does not accept it. Every member remains separately named and
checksummed in the manifest and can be restored individually; there are no
tape filemarks between members of that one Object. Each non-empty member starts on a chunk boundary and therefore
costs at least one tape block; for trees of very small files, and for files
the format cannot represent natively, see the blob wrapper in REM-OBJECT Core
Format Appendix E. [Object sizing](guide-object-sizing.md) discusses the
tradeoffs.

The receipt reports committed state. It is built from the daemon's
checkpoint barrier — with a catalog lookup for objects an automatic
checkpoint committed earlier — never from the append acknowledgement
alone. When some object cannot be verified committed, the confirmed
receipts still print, the exit is nonzero, and the message distinguishes
"commit state unknown" (a lookup failed) from "no committed copy" (the
daemon says it is not on tape). A failed append aborts the whole batch,
and the error states what the abort actually achieved — including the
case where the abort itself failed and the session remains recoverable.

`--drive` (write to whatever cartridge a named drive currently holds) is
accepted by the argument parser and rejected by the daemon. Targeting a tape
with no pool assignment (`allow_unpooled` in the API) is likewise rejected.

<!-- code-anchor: crates/remanence-cli/src/get.rs @ 4dc2a3c6 -->
## Restoring from tape

In library mode, `rem get` is put's counterpart: it restores an object's
members from tape through the daemon read path. With `--device` and
`--allow-device`, it restores a catalogued object from the cartridge already
loaded in one standalone drive; both `--out` and `--dest` are accepted.
The daemon path resolves the object format server-side and streams each
member's payload bytes, so there is no client-side format interpretation.
Every member is proven before it is
published: bytes stream to a temporary file, are hashed as they land, and
only rename into place when the hash matches the catalog's per-file
sha256.

| Command | What it does |
|---|---|
| `rem get <OBJECT> --dest <DIR> [--overwrite]` | Restore all members of an object into a directory. OBJECT is an object UUID by default; `--caller-id --pool <POOL>` selects by the caller id recorded in that pool at put time, `--sha256` by content hash. The daemon mounts the copy's tape as needed, and a media-readiness fence is watched to completion (`--no-wait` fails fast). |
| `rem get <OBJECT> --dest <DIR> --tape <UUID>` | Read from the copy on a specific tape instead of the first cataloged copy — for exercising a particular cartridge, or steering away from one. |
| `rem get <OBJECT> --dest <DIR> --path <MEMBER>` | Restore only the named member. |
| `rem get ... [--stream-chunk-bytes <BYTES>] [--no-wait]` | Suggested read-stream chunk size (default 1 MiB), and `--no-wait` to fail fast instead of watching a media-readiness load. |
| `rem archive verify --library <SERIAL> --drive <BAY> --locator <JSON> --expected-sha256 <HEX>` | Through the daemon, read one object from the tape currently loaded in drive bay `--drive` and compare the bytes read with the expected payload SHA-256, restoring nothing. `--library` takes a library serial or UUID. The locator is the JSON that `rem-debug archive write --json` prints. This form has no `--private-key` option; `rem-debug archive verify` is the direct form that takes one. |

On a digest mismatch nothing is published: the temporary file is kept for
forensics and the error names both hashes and both honest explanations —
a damaged copy (treat it as suspect: re-read or scrub), or an
encrypted-at-rest object whose ciphertext this command does not decrypt
(restore those via the spooled-object path: `rem archive` tooling with
the private key). Member paths from the catalog are sanitized with the
same rules put applies on ingest — `..` refuses, absolute prefixes strip
— so a restore never writes outside `--dest`, whatever the catalog says.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-cli/src/archive_ingest.rs crates/remanence-cli/src/archive_map.rs crates/remanence-aead/src/wrap.rs @ 6ed03ef0 -->
## Archive objects (local, no tape)

`rem archive` builds and reads portable REM-OBJECT object files on local disk.
None of these touch tape; they are the easiest way to exercise the format.

| Command | What it does |
|---|---|
| `rem archive capabilities` | Print a one-line JSON list of this build's REM-OBJECT capabilities (e.g. `rem-object-envelope`, `wrap-suite-hpke-v1`, `ranged-ciphertext-extract`) with zero hardware discovery. `wrap-suite-hpke-v1` names the HPKE Base-mode construction; the KEM it runs is the X-Wing hybrid (see [reference-tape-layout.md](reference-tape-layout.md#the-encrypted-envelope-remo)), not a plain X25519 KEM despite the string. Useful for scripts probing what a given binary supports before calling it. |
| `rem archive recipient keygen --out <FILE.remp> [--label <TEXT>] [--public <FILE.remr>]` | Generate a private REMP file with mode `0600` and a slot-0 REMR public file. The public path defaults to the private path with extension `.remr`. Refuse existing paths and labels outside 0–32 printable ASCII bytes. Print JSON with `private`, `public`, `epoch_id` (hex), and `label`; print a private-file recovery warning on a terminal. |
| `rem archive recipient derive --private-key <REMP> --slot-index <0..255>` | Parse a canonical private recipient file, derive its X-Wing public key, and print the canonical REMR file as standard base64 with its epoch identity. This hardware-free registry integration command never prints the private seed. |
| `rem archive recipient inspect --public-key <REMR>` | Parse and cryptographically validate a canonical public recipient file, then print its slot, epoch id, label, and 1216-byte X-Wing public-key size as JSON. |
| `rem archive build --inputs <PATH>... --out <FILE> [--recipient <REMR>]...` | Build a portable object and print a JSON build report. Capture keeps `user.*` xattrs by default, reports other dropped names per entry, and emits one warning with the drop count. `--full-fidelity-xattrs` is the explicit non-portable keep-all opt-in and cannot be combined with `--rules` or `--map`. Omitting `--recipient` writes the plaintext `rem-object-v1` body. Repeating it 2-8 times with distinct, nonzero epochs in ascending slot order writes the HPKE envelope; the presence of recipients is the encryption switch. `--map`/`--source-root`/`--map-sha256` accept a planner-emitted source map instead of `--inputs`. `--scan-only` classifies inputs and reports without writing. `--rules` applies an ingest ruleset; `--manifest-out` (requires `--rules`) writes the member manifest JSON. `--no-index` skips the generated `.remwrap.idx` siblings of blob wrappers. `--blob-suggest-ratio` (default 0.9), `--blob-suggest-count` (default 100) and `--sanity-ceiling-count` (default 10000) set the scan's blob-suggestion and sanity-ceiling thresholds. `--object-id`, `--caller-object-id`, `--manifest-file-id` and `--timestamp` record the supplied values instead of fresh UUIDs and the current time. `--chunk-size` sets the block/chunk size (default 256KiB). There is no `--encrypt` flag. |
| `rem archive reseal --object <FILE> --private-key <REMP> --recipient <REMR> <REMR>... --out <FILE> [--staging-dir <DIR>]` | Open one envelope with a matching recipient private key and fully re-seal the same authenticated plaintext to a new ordered set of 2-8 distinct recipients. Unlike build/write's repeatable flag, reseal takes its 2-8 paths as one `--recipient` value list. It verifies the new stored digest before an atomic no-replace publish; staging happens next to the destination unless `--staging-dir` is supplied. Input and output are both encrypted envelopes. |
| `rem archive inspect --object <FILE> [--chunk-size <BYTES>]` | Inspect a plaintext body or parse and validate an envelope header/key frame without a key. Encrypted output reports `format_version` and `recipient_epochs`; it cannot expose encrypted manifest members. `--chunk-size` (default 256KiB) is the block/chunk size of a plaintext object. |
| `rem archive extract --object <FILE> --dest <DIR> [--private-key <REMP>] [--overwrite] [--path <MEMBER> --range <START:LEN>]` | Extract a whole object. `--private-key` is required for an encrypted object and rejected for plaintext. `--path` and `--range` together extract a byte range of one member. `--path` without `--range` is rejected with `--path, --first-chunk-lba, and --file-size-bytes require --range`; to restore one whole member by path, give a range that covers it, such as `--range 0:<size>`. `--first-chunk-lba <LBA>` and `--file-size-bytes <BYTES>` optionally give the member's first object-local block and full size for a range read, from the build report or catalog row. `--blob-entry`/`--blob-member` restores a single member from a blob wrapper (REM-OBJECT Core Format Appendix E describes the wrapper and the range read this performs). `--overwrite` replaces existing files; `--xattr-namespace <PREFIX>` (repeatable) restores an additional xattr namespace besides `user.`; `--no-unwrap` keeps wrapper entries literal; `--chunk-size` (default 256KiB) is the block/chunk size of a plaintext object. |
| `rem archive extract-stream --private-key <REMP> [--range <START:LEN> --authenticated-prefix <FILE> --stored-range-start <BYTE>]` | Stream-decrypt an envelope from stdin to stdout with per-chunk authentication and bounded memory. With the three ranged flags together, re-authenticate the header/key-frame/metadata prefix and decrypt only the covering ciphertext frames beginning at `--stored-range-start`. |
| `rem archive covering-range --private-key <REMP> --object-id <ID> --file-id <ID> --range <START:LEN>` | Authenticate an envelope header/key-frame/metadata prefix from stdin and print the smallest covering stored-ciphertext range (`stored_range_start`, `stored_range_len`, `first_chunk`, `chunk_count`). See [`reference-extract-stream-protocol.md`](reference-extract-stream-protocol.md). |
| `rem restore --object <FILE> --dest <DIR> [--private-key <REMP>]` | Top-level alias of `rem archive extract`, with the same flags and the same key contract. |
| `rem archive list` | List native objects from the local catalog (no tape access). |
| `rem archive formats` | Print JSON describing the foreign-format adapters linked into this binary; stock Remanence returns an empty list. |
| `rem archive probe --format <ID> --dump <FILE>` | Ask a registered foreign-format adapter to identify a dump without streaming it. |
| `rem archive scan --format <ID> --dump <FILE>` | Catalog normalized entries from a foreign-format dump. |
| `rem archive restore --format <ID> --dump <FILE> --dest <DIR> [--overwrite] [--xattr-namespace <PREFIX>]...` | Restore a foreign-format dump into a directory. `--xattr-namespace` (repeatable) restores an additional xattr namespace besides `user.`. |
| `rem archive recover --format <ID> --dump <FILE> --dest <DIR>` | Best-effort recovery of a damaged dump into sparse partial files. |

These commands are generic, but the stock Remanence binaries register no
foreign formats and fail closed with a “not registered in this distribution”
error. An adapter distribution supplies the accepted IDs and parser code; see
the [foreign-format adapter reference](reference-foreign-format-adapters.md).

A plaintext object that reaches tar EOF without its `_remanence/manifest.cbor`
entry is nonconformant (REM-OBJECT §4.9). The REM-OBJECT reader in
`remanence-format` does not fail a restore-mode read for that reason alone: it
returns the member entries it recovered together with the non-fatal typed
warning `MissingManifest`, so the absence stays visible to its caller.
`rem archive extract` passes such warnings on: every one of its JSON reports
carries `warnings`, an array of warning names. A whole-object extract and a
plaintext `--path`/`--range` extract read the object in restore mode, so their
array holds `MissingManifest` for an object without its manifest. An encrypted
`--range` extract and a `--blob-entry`/`--blob-member` extract read only the
members they need and never the manifest, so their array is always empty.

Blob wrappers (REM-OBJECT Appendix E). When `rem archive build` wraps a
subtree, it creates the `.remwrap.tar` member by running the system tar program
with `-c` and `--format pax --xattrs`, and records the program, its version and the
exact create and extract argument lists in the index's `tar_engine` field.
That program is bsdtar, and only wrappers need it. `rem archive build`, with or
without `--scan-only`, looks for it once the plan holds a wrapper. A
whole-object `rem archive extract` looks for it before writing anything when
the object holds a `.remwrap.tar`, so a host without bsdtar fails the command
before the destination changes. It then unwraps exactly the object's own
wrappers and leaves any other `.remwrap.tar` already under `--dest` as it is.
`--no-unwrap`, `--path`/`--range` and `--blob-entry`/`--blob-member` never run
bsdtar. The `tar_engine` field of the build report's `ingest`, of the scan
report, of the manifest `--manifest-out` writes, and of the extract report's
`unwrap` is `null` when no wrapper was involved: always for a `--map` build,
which never creates one, and always under `--no-unwrap`. An index always
carries its `tar_engine`, since it exists only beside a wrapper.
`rem archive extract` with `--blob-entry` and `--blob-member` recovers one
inner file by the procedure of Appendix E.4, for both representations.
`--blob-member` cannot be combined with `--path` or `--range`, so the reference
tools offer no byte-range restore inside an inner file.

### Recipient key-file formats

REMP and REMR are complete binary records, not raw key bytes. All offsets are
from the start of the file, labels are 0–32 printable ASCII bytes (`0x20`–`0x7e`),
and parsers reject trailing bytes.

| REMP offset | Length | Field |
| ---: | ---: | --- |
| 0 | 4 | magic `REMP` |
| 4 | 16 | opaque recipient epoch id |
| 20 | 1 | label length `L` |
| 21 | `L` | epoch label |
| `21 + L` | 32 | canonical X-Wing private seed |

The exact REMP length is `53 + L` bytes. Treat the entire file as secret.

| REMR offset | Length | Field |
| ---: | ---: | --- |
| 0 | 4 | magic `REMR` |
| 4 | 1 | key-frame slot index |
| 5 | 16 | opaque recipient epoch id |
| 21 | 1 | label length `L` |
| 22 | `L` | epoch label |
| `22 + L` | 1216 | X-Wing public key: ML-KEM-768 encapsulation key followed by X25519 public key |

The exact REMR length is `1238 + L` bytes. The slot, epoch id, and label are
copied into envelope recipient slots; the public-key encoding is validated
when the record is parsed.

### Encryption flag matrix

| Operation | Seal/open key input | Result |
|---|---|---|
| `archive build`, `rem-debug archive write` | no recipient flags | Plaintext `rem-object-v1` body. |
| `archive build`, `rem-debug archive write` | `--recipient <REMR>` repeated 2-8 times | REM-ENCRYPT 1.0 with on-tape `format_version = 2`. Epoch ids must be distinct and nonzero and slot order ascending. |
| `archive reseal` | one matching `--private-key <REMP>` plus one `--recipient` list containing 2-8 new REMR files | Full re-seal for recipient rotation. |
| `archive inspect`, `rem-debug archive export-object` | none | Keyless envelope inspection, or opaque stored-byte export. |
| `archive extract`, `restore`, `rem-debug archive read`, `rem-debug archive verify` | `--private-key <REMP>` for encrypted input | The private key's epoch id selects its matching key-frame slot. |
| `archive extract-stream`, `archive covering-range` | `--private-key <REMP>` | Encrypted streaming or bounded ranged open. |

The flags `--encrypt`, `--key-file`, `--key-id`, and `--registry-key` do not
exist; the argument parser rejects them.

### Encrypted report schemas

`recipient_epochs` is consistently an ordered JSON array of objects shaped
as `{"epoch_id":"<32 lowercase hex>","label":"<epoch label or null>"}`.
Fresh encrypted writes and reads obtain the label from the authenticated key
frame. A host-only idempotent replay uses the committed catalog epoch ids and
reports `label: null`, because the catalog does not retain the on-tape labels;
it never substitutes labels from the new retry request.

- An encrypted `archive build` report has `representation: "encrypted"`,
  `encryption: "REMO"`, `format_version: 2`, `recipient_epochs`, stored and
  plaintext digests/sizes, manifest digest, and file layout. The plaintext
  form uses `format_version: null` and `recipient_epochs: null`.
- Encrypted `archive inspect` is explicitly keyless (`keyed: false`) and
  reports the object id, format version, recipients, salt, prefix geometry,
  stored digest/size, and plaintext size/chunk count. It does not claim to
  expose the encrypted manifest.
- `archive reseal` reports `input_format_version: 2`,
  `output_format_version: 2`, the new `recipient_epochs`, plaintext and
  published stored digests, sizes, and `verified_after_write: true`.
- Encrypted local-extract reports and tape-write locators carry
  `recipient_epochs`. Direct tape read/verify receipts keep their existing
  payload byte/hash fields; recipient provenance remains in the locator and
  catalog. Whole-object `extract-stream` additionally reports
  `format_version: 2`; ranged mode reports the recipients parsed from the
  authenticated prefix and the authenticated-chunk/stored-range geometry.

<!-- code-anchor: crates/remanence-cli/src/rem_debug.rs crates/remanence-cli/src/lib.rs @ bc2f9f90 -->
## rem-debug extras

`rem-debug` has only part of the surface above. It shares discovery
(`libraries`, `library`, `watch`), `rebuild-catalog-from-journals`, the local
tape-lifecycle commands (`tape init`, `tape wait-ready`, `tape quarantine
list`/`show`/`release`, `tape retire`) and the local `archive` commands
(`capabilities`, `formats`, `recipient`, `build`, `reseal`, `inspect`,
`extract`, `extract-stream`, `covering-range`, `list`, and the foreign-format
`probe`, `scan`, `restore` and `recover`). It has none of the standalone-drive
commands, no top-level `restore`, and none of the daemon client commands
(`daemon`, `op`, the `catalog` queries, `audit`, `drive`, `alarms`, `top`,
`put`, `get`, `tape finalize`, `tape supersede-tail`, `tape inventory`, `tape
verify-index`). It refuses the daemon client commands with an error saying
they are available through `rem`. Its top-level help also lists
`tape-supersede-client`; that is the daemon client for `tape supersede-tail`,
and it is refused in the same way.

Some names mean different things in the two binaries. `rem-debug catalog` is
local catalog maintenance (`reset-preflight`, `reset`), not the daemon catalog
queries. `rem-debug tape alerts` reads the drive directly by `--bay`, while
`rem tape alerts <DRIVE>` is the daemon alias of `rem drive alerts`. `rem
archive verify` verifies through the daemon (see [Restoring from
tape](#restoring-from-tape)); `rem-debug archive verify` reads the tape
directly.

In the other direction, `rem` refuses the robotics and the direct
tape-path `archive write`, `archive read` and `archive export-object` with an
error saying they are available through `rem-debug`. It has no `tape
bot-probe`, `tape adopt-bootstrap`, `tape recovery-report`, `catalog reset` or `catalog
reset-preflight`.

What `rem-debug` adds:

Robotics (direct SCSI against the changer; all require `--allow <SERIAL>`):

| Command | What it does |
|---|---|
| `rem-debug move --src <ADDR> --dst <ADDR> <SERIAL>` | MOVE MEDIUM between two element addresses. |
| `rem-debug load --slot <SLOT> --bay <BAY> <SERIAL>` | Composed changer MOVE MEDIUM + drive SSC LOAD. |
| `rem-debug unload --bay <BAY> [--dest <SLOT>] [--config <PATH>] <SERIAL>` | SSC UNLOAD + MOVE MEDIUM back to the recorded source slot (or `--dest`), with locked recovery for a stranded medium-removal prevent. |
| `rem-debug export --slot <SLOT> <SERIAL>` / `import --slot <SLOT> <SERIAL>` | Move a cartridge to the first free IE port, or in from the first occupied one. |
| `rem-debug rescan <SERIAL>` | INITIALIZE ELEMENT STATUS, then reconcile the snapshot. Shape mismatch is a hard error. |
| `rem-debug lock <SERIAL>` / `unlock <SERIAL>` | PREVENT/ALLOW MEDIUM REMOVAL on the changer. The lock is held by the changer itself and survives process exit. |

Unload first tries SSC UNLOAD without taking new locks or reading config. Only
ILLEGAL REQUEST / 05/53/02 (MEDIUM REMOVAL PREVENTED) triggers recovery: take the
VPD-serial device lock `/run/lock/rem/<drive-serial>.lock` without waiting, issue
ALLOW MEDIUM REMOVAL (audited as `AllowRemoval`), retry UNLOAD, then MOVE. The lock
stays held through MOVE. A busy lock refuses recovery without ALLOW; a failed
ALLOW stops before the retry or MOVE. Other UNLOAD errors do not trigger recovery.

Without `--config`, the device lock is the recovery exclusion; no config file,
including `/etc/rem/config.toml`, is read. If `--config <PATH>` is explicitly
supplied, recovery also acquires that config's state-directory lock without
waiting and records recovery in its audit log. It takes only the bare state lock:
it does not open/migrate the catalog or reconcile reset fences, so a catalog-reset
fence does not block cartridge recovery. Normal unloads do not acquire this state
lock even when `--config` is supplied. `rem-debug unlock` addresses the changer,
so it does not clear a drive's PREVENT.

Direct tape data path (bypasses the daemon; uses config for catalog and
journals):

| Command | What it does |
|---|---|
| `rem-debug archive write --library <SERIAL> --file <PATH> --pool <POOL> [--archive-path <PATH>] [--caller-object-id <ID>] [--recipient <REMR>]... [--json]` | Write one local file to a pool-selected tape through the in-process actor on the placed library drive. `--archive-path` overrides the in-archive path (default: the file's basename); `--caller-object-id` records an opaque caller object id (default: a fresh UUID). It applies the same positive `NotWorm` admission as daemon-owned Object writes before MODE SELECT; write-protected, WORM, and unknown media fail closed. Repeating `--recipient` 2-8 times writes the encrypted envelope; omitting it writes plaintext. The locator includes `format_version` and `recipient_epochs`. |
| `rem-debug archive read --library <SERIAL> --locator <JSON> --out <PATH> [--private-key <REMP>]` | Read an object by locator; an encrypted copy requires the matching private key. |
| `rem-debug archive export-object --library <SERIAL> --locator <JSON> --out <PATH>` | Export the complete stored object bytes (including envelope) by locator. |
| `rem-debug archive verify --library <SERIAL> --locator <JSON> --expected-sha256 <HEX> [--private-key <REMP>]` | Stream and hash an object on tape against an expected digest, restoring nothing; encrypted copies require the matching private key. |
| `rem-debug archive probe/scan/restore/recover --format <ID> --tape <SERIAL> --bay <BAY> [--rewind]` | Run a registered foreign-format adapter directly against a mounted tape instead of a dump file. `--tape` takes the serial of the library that holds the tape, and `--bay` the drive bay; that library must be on `--allow`. `restore` and `recover` also take `--dest <DIR>`, as with a dump file. |
| `rem-debug tape alerts --bay <BAY> [--library <SERIAL>]` | Read the loaded drive's TapeAlert LOG SENSE page directly. `--library` selects a configured library when more than one is present. `rem tape alerts` has no `--bay`; it is the daemon alias that takes a drive serial or UUID. |

Destructive maintenance:

| Command | What it does |
|---|---|
| `rem-debug catalog reset-preflight [--preserve-tape-voltag <VOLTAG>]... [--allow-erase-tape-voltag <VOLTAG>]...` | Read-only admission for a scoped catalog reset. It enumerates every data and cleaning tape, rejects any bound label outside the two exact selector sets, and returns a source-bound token plus `source_schema_version`. A catalog of schema 16 or 17 is inspectable only when the preservation set is empty and every bound tape is explicitly allowed to be erased; such a catalog is never migrated and never accepted for preservation. |
| `rem-debug catalog reset --i-understand-this-erases-the-catalog [--expected-preflight-token <SHA256>] [--preserve-tape-voltag <VOLTAG>]... [--allow-erase-tape-voltag <VOLTAG>]...` | Destructively reset local catalog state from the configured paths. Any scoped reset requires the exact current preflight token and repeats selector and source-schema admission under the exclusive state lock before mutation. |

The global `--allow-derived <SERIAL>` flag additionally permits operating
drive bays whose identity was derived rather than read from the device, in
the commands that `--allow` gates; it must be a subset of `--allow`. `tape
init` is not gated by `--allow`: it operates only on the libraries in the
configuration. `tape retire` and `catalog reset` change local state and carry
their own confirmations. `tape quarantine release` uses the daemon for tape-I/O
fences when it runs; offline release requires exclusive state ownership.

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 6ed03ef0 -->
## Catalog rebuild

`rem rebuild-catalog-from-journals [--config <PATH>]` rebuilds the SQLite
catalog projection from the audit log and per-tape journals. This is the
recovery path that makes the SQLite file a disposable cache rather than a
single point of failure.

<!-- code-anchor: crates/rem-recover/src/main.rs @ 1dd451b2 -->
## `rem-recover`: standalone recovery

`rem-recover` is a separate crate and binary (`crates/rem-recover`), not
part of `remanence-cli`. It exists for the case where the rest of the
stack — daemon, catalog, config file — is unavailable, untrusted, or
simply not worth standing up: given one REM-OBJECT object file and a key, it
recovers plaintext members with nothing else.

```
rem-recover --object <FILE> --private-key <REMP> --out <DIR>
            [--staging-dir <DIR>] [--overwrite] [--xattr-namespace <PREFIX>]...
```

It parses the envelope header and key frame directly (no daemon/catalog
dependency), selects the slot matching the private key's epoch id, and
fails with a message naming the object recipient labels when none match.
Decrypted plaintext is
staged in a temporary file inside `--out` when that directory already
exists, otherwise next to it (or inside `--staging-dir` if given, never
`/tmp`), and securely zeroed on drop before the restore completes. It
validates the embedded object id against the header before writing files
out, and refuses to overwrite existing output unless `--overwrite` is given.
`--xattr-namespace <PREFIX>` (repeatable) restores an additional xattr
namespace besides `user.`.

Use this deliberately small binary when the full CLI stack is unavailable;
ordinary local restores can use `rem archive extract --private-key`.

`rem-recover` uses stable process exit classes so unattended recovery tooling
can distinguish failures:

| Code | Meaning |
| ---: | --- |
| 0 | recovery completed |
| 2 | command-line usage error (from `clap`) |
| 3 | malformed REMP key file or recipient epoch not present in the object |
| 4 | corrupt/nonconforming envelope or inner REM-OBJECT; also a matching-epoch authenticated unwrap failure, which cannot distinguish a damaged recipient slot from an incorrect REMP seed |
| 5 | host filesystem, staging, input, or output I/O failure |

Tape-I/O fences appear in `rem catalog tapes --json` and `rem top` as
`active_tape_io_fence_count` and `active_tape_io_fences`. Each record identifies
the quarantine, UUID, barcode, endpoint, reason, and state. The daemon's release
RPC is described in [the API reference](reference-api.md).

For library-mode `rem get --caller-id`, `--pool <POOL>` selects the caller id's
pool. Without `--pool`, a caller id present in several pools returns a refusal
that names the pools. A caller id present in only one pool still resolves.

## Development tools

`rem-dev` holds the review-vector generator and scratch-tape drills. It is
absent from release archives and the field kit. Build it with
`cargo build -p remanence-cli --bin rem-dev`. See [Development tools](development-tools.md)
for commands and arguments. Old development invocations through `rem` or
`rem-debug` fail with a one-line replacement command.
