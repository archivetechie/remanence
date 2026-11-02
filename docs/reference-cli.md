# CLI reference

This page is for operators and script authors. It describes every command of `rem`,
`rem-debug`, `rem-daemon` and `rem-recover`, one command per section. Each section gives
the command's purpose, its synopsis (the usage line `--help` prints), its options, an
example, and what it prints. `--help` at any level remains the complete list of options.

Examples that touch only local files were run against a current build and show its real
output, shortened where marked. Examples that need a tape library, a drive or a running
daemon show the output format from the code, with illustrative values: barcode `RMJ101L9`,
pool `archive`, library serial `LIB001`, drive bay `0x0100`, slot `0x0400`, device
`/dev/sg5`, and tape UUID `5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f`.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-cli/src/main.rs crates/remanence-cli/src/rem_debug.rs crates/remanence-daemon/src/entry.rs @ 625f7044 -->
## The binaries

Remanence builds four programs that operators use. The first two come from the
`remanence-cli` crate.

- `rem` is the operator CLI. Library operations talk to a running `rem-daemon` over gRPC.
  Standalone-drive operations run the same engine in process, against one explicitly
  selected and authorized drive. `rem` also works on local state and local files. It
  cannot issue arbitrary SCSI commands.
- `rem-debug` is the break-glass CLI. It shares discovery, the local tape-lifecycle
  commands and the local `archive` commands with `rem`, and adds direct SCSI robotics,
  direct tape reads and writes, tape-log repair and destructive catalog maintenance. It has
  neither the standalone-drive commands nor the daemon client commands.
  [rem-debug extras](#rem-debug-extras) lists exactly which commands each binary has.
- `rem-daemon` is the long-running library service (see [rem-daemon](#rem-daemon)).
- `rem-recover` is a small catalogless recovery tool for one encrypted object file on disk
  (see [rem-recover: standalone recovery](#rem-recover-standalone-recovery)). It does not
  touch tape, the catalog or a configuration file.

A fifth binary, `rem-dev`, holds development drills and is described under
[Development tools](#development-tools).

The `rem-debug` commands that move cartridges or write tape directly must name the target
library in `--allow <SERIAL>`; without it the command refuses before it opens any device.
The commands gated this way are the robotics, the direct tape-path `archive` commands,
`tape bot-probe`, `tape read-rate` and `tape adopt-bootstrap`. The reason is that a
partitioned chassis can expose another system's partition on the same host, and a mistyped
element address must not be able to touch it. `--allow-derived <SERIAL>` additionally
permits drive bays whose identity was derived rather than read from the device; it must be
a subset of `--allow`. For library and daemon commands, `rem` accepts `--allow` and
`--allow-derived` as hidden compatibility flags and ignores them. For the standalone-drive
commands it rejects them as unexpected arguments.

## Conventions

These conventions apply to both CLIs.

- Daemon-backed commands take `--endpoint <URI>`, default `unix:/var/lib/rem/rem.sock`. A
  TCP endpoint reaches the daemon's mTLS (mutually authenticated TLS) listener.
- Library commands that read local configuration take `--config <PATH>`, default
  `/etc/rem/config.toml`. Standalone commands use that file when it exists; otherwise they
  create a home-mode configuration and state directory under
  `${XDG_STATE_HOME:-$HOME/.local/state}/rem`.
- `--json` switches to stable, machine-readable JSON intended for orchestrators and
  scripts. Library and daemon commands print one envelope per result,
  `{"schema": ..., "kind": ..., "data": ..., "operation": null}`, where `schema` names the
  output format (for example `rem.catalog.tape.list.v1`) and `kind` is `item`, `list` or
  `error`. Standalone commands print `{"ok": ..., "result": ...}`, or `"error"` in place of
  `"result"`. In `rem`, both forms also carry `supported_endpoint_kinds` and
  `observed_endpoints`, and the standalone form carries `active_fences`; `rem-debug` omits
  the first two.
- Tape-structural unsigned 64-bit values (file numbers, LBAs, block and row counts,
  scopes, edition sequences) are decimal JSON strings. This is a lossless contract up to
  `u64::MAX`; consumers must not convert them through an IEEE-754 number. Human output uses
  ordinary decimal text.
- Durations accept `ms`, `s`, `m` and `h` suffixes. Sizes accept byte counts or `KiB`,
  `MiB` and `GiB` suffixes (powers of 1024).
- `rem --version`, `rem-debug --version` and `rem-daemon --version` print the package
  version. `rem-recover` has no `--version` flag.

### Placeholders

Angle brackets mark a value you supply. The names below are used throughout this page.

| Placeholder | What to type, and where the value comes from |
|---|---|
| `<SERIAL>` | A library's serial number, as printed by `rem libraries`. |
| `<DRIVE_SERIAL>` | A standalone drive's VPD page 0x80 serial (vital product data, the drive's serial number page), as printed by `rem drives --json`. |
| `<ADDR>`, `<SLOT>`, `<BAY>` | A changer element address (for example `0x0400`), a storage-slot address, and a drive-bay address. `rem library <SERIAL> --slots` lists them. |
| `<BARCODE>` | The barcode printed on a cartridge, as shown by `rem library <SERIAL> --slots`. |
| `<LABEL>` | A standalone cartridge's operator label: free text chosen with `rem tape init --label`, unique in the catalog. It is not the tape's identity. |
| `<TARGET>` | A cartridge, given either as a barcode or as an element address, or as an inclusive slot range such as `0x0400..0x0407` where the command says a range is accepted. |
| `<UUID>`, `<TAPE_UUID>` | A tape's UUID, as printed by `rem catalog tapes`, or on a standalone drive by `rem tape identify` and in `rem put` receipts. Other `<UUID>` values are named where they appear (an operation UUID from `rem op list`, an object UUID from `rem put`). |
| `<DRIVE>` | A drive's serial number or UUID, as printed by `rem drive list`. |
| `<POOL>`, `<POOL_ID>` | The `id` of a `[[tape_pools]]` entry in the [configuration](reference-configuration.md#tape_pools-and-tape_pool_rules). In home mode the generated configuration defines `default`, and `rem pool new` creates further pools. |
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
| `<SG>` | The Linux SCSI generic device path of a tape drive, for example `/dev/sg5`. |

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-cli/src/media_readiness.rs crates/remanence-cli/src/recovery_report.rs crates/remanence-daemon/src/entry.rs @ 625f7044 -->
## Exit codes

Most subcommands exit 0 on success and 1 on failure. Exit status 2 has four causes: a
command line that clap rejects, a command that names a library serial not found on this
host, a catalog reset missing its confirmation flag or its preflight token, and a
`rem-debug tape recovery-report` that was produced but contains failures. `rem-daemon`
exits 1 on any startup or serve failure.

`rem tape wait-ready` and the readiness phase of `rem tape init` use a finer set of codes,
so that scripts can branch on what the drive reported:

| Code | Meaning | What to do |
|---|---|---|
| 0 | Medium ready. | Continue. |
| 10 | Retryable wait state (becoming ready, unit attention, target busy). | Leave the cartridge alone and resume later. |
| 20 | Timed out while still retryable. | Resume the wait; the drive has not failed. |
| 30 | Terminal not-ready (no medium, repeated unit attention, check condition, reset required). | An operator has to act. |
| 40 | Transport outcome unknown: the command may or may not have completed. | Verify the drive's state before retrying. |
| 50 | Reservation conflict (another initiator owns the drive), or a resume target that cannot be resolved. | Find the other initiator or correct the resume target. |
| 130 | Interrupted by a signal. A durable readiness record fences the operation. | Continue with `--resume`. |

For `wait-ready --resume <UUID> --endpoint <URI>`, code 20 means the local `--timeout`
expired while the daemon's wait continues; it does not cancel or shorten the daemon's
9000-second wait. Codes 0, 30, 40 and 50 mean ready, terminal, transport unknown and
ownership respectively. A daemon readiness timeout also returns 20. The resume JSON schema
is `rem.tape.wait_ready_resume.v1`, with `operation_id` and `exit_code`. The direct form's
`--json` output carries the same decoding in structured form, in the
`recommended_next_command` and `operator_action` fields.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-cli/src/standalone.rs @ 625f7044 -->
## Discovery and hot-plug

Discovery needs read and write access to the tape and changer `sg` nodes, usually through
the `tape` group. Writes need `CAP_SYS_RAWIO`; changer inventory can also need it for
filtered SCSI commands. See [Installing Remanence](guide-install.md) and
[troubleshooting](guide-troubleshooting.md).

### rem libraries

Discover every tape library this host can reach.

```text
rem libraries [OPTIONS]
```

- `--json` emits machine-readable JSON.

The command joins changer inventory (READ ELEMENT STATUS, the changer's inventory command)
with drive identities (INQUIRY and VPD page 0x80) by serial number. It reads the changer and
the drives and moves nothing.

To list the libraries on this host:

```sh
rem libraries
```

The command prints one line per library, of this form (values illustrative):

```text
LIB001  HPE MSL3040  /dev/sg9  (4 drives, 40 slots [12 loaded], 0 IE)
```

### rem library

Show one library: its drives, their loaded barcodes and, optionally, every slot.

```text
rem library [OPTIONS] <SERIAL>
```

- `<SERIAL>` is the changer's serial (VPD 0x80). `rem libraries` lists the serials.
- `--slots` also lists every storage slot with its state (full or empty) and barcode.
- `--json` emits JSON with drives, slots and loaded barcodes.

An unknown serial exits 2 with `error: no library with serial "<SERIAL>" on this host`.

To see the drives and slots of library `LIB001`:

```sh
rem library LIB001 --slots
```

### rem drives

List the tape endpoints discovered on this host and who owns them.

```text
rem drives [OPTIONS]
```

The standalone parser serves this command, so it takes the standalone global flags
described under [Standalone drives](#standalone-drives). It reports unattached drives
proved to be standalone as well as drives in libraries. `--json` reports `path`, serial,
endpoint kind and ownership for each endpoint.

To list the endpoints:

```sh
rem drives
```

The command prints a table with the columns `PATH`, `KIND`, `OWNERSHIP`,
`VENDOR PRODUCT` and `SERIAL`. A value discovery could not determine prints as `-`.

### rem watch

Stream hot-plug events for SCSI tape and changer devices.

```text
rem watch [OPTIONS]
```

- `--coalesce-window <DURATION>` sets the sliding window in which bursts of events are
  merged. Default `500ms`.

The command subscribes to udev and prints coalesced bursts until it is interrupted. It
issues no SCSI commands. It needs a build with the `linux-udev` feature; without it, the
command exits 1 with `` error: `rem watch` requires the `linux-udev` build feature. ``

<!-- code-anchor: crates/remanence-cli/src/standalone.rs crates/remanence-cli/src/standalone_writing.rs crates/remanence-api/src/engine/drive_adapter.rs crates/remanence-library/src/discovery.rs @ 625f7044 -->
## Standalone drives

The standalone interface operates one manually loaded drive directly. It needs no
`rem-daemon`, no changer and no `[[libraries]]` entry. The complete walkthrough is
[Using a standalone LTO drive](guide-standalone-drive.md).

### Routing

`rem` has two command parsers: the library parser and the standalone parser. Which one
handles an invocation depends on the command and its flags, and the same rule decides which
help text `--help` shows:

| Invocation | Route |
|---|---|
| Explicit `--device` | Standalone |
| Standalone-only verb (`setup`, `drives`, `doctor`, `diagnostics`, `list`, `pool new`, or a standalone-only tape verb: `tape identify`, `verify`, `adopt`, `restore`, `assign-pool`, `rebind`, `set-generation`, `import-journals`, `release`) | Standalone |
| Shared verb with `--config`, `--endpoint`, `--library` or `--after-settled-inventory`, without `--device` | Library/daemon |
| Shared verb (`put`, `get`, `tape init`, `tape finalize`, `tape supersede-tail`, `tape quarantine`) with none of those routing flags, no `/etc/rem/config.toml`, and home `[home] drive_serial` set | Standalone |
| Other shared invocations | Library/daemon |

**Home mode** is the last standalone row: no system configuration exists, and `rem setup`
has saved a drive serial in the home configuration. In home mode the shared verbs operate
the saved drive without a daemon.

### Global standalone flags

Every standalone command accepts these flags:

- `--config <CONFIG>` loads this configuration instead of the home default.
- `--device <DEVICE>` operates this discovered SCSI tape device directly.
- `--allow-device <ALLOW_DEVICE>` authorizes a drive by its discovered serial or device
  path.
- `--allow-unserialized-device` allows reads from a drive that has no VPD serial.
- `--allow-untrusted-state-volume` accepts a state filesystem that fails the durability
  checks.
- `--acknowledge-unknown-ownership` acknowledges incomplete discovery, for read-only
  access.
- `--reserve` adds SCSI RESERVE(6) and RELEASE(6) around the invocation, for drives that
  other initiators can reach.
- `--json` emits the standalone JSON envelope.

`rem setup` saves a home serial that supplies the `--device` and `--allow-device` defaults;
discovery must then find exactly one matching path. The saved serial never authorizes a
different explicit device. Ownership and the serial read when the drive is opened are still
checked, and unknown ownership is not permission to write. A `[[standalone_drives]]`
configuration entry pins known ownership but cannot override an unresolved or positive
changer claim. `--allow-unserialized-device` and `--acknowledge-unknown-ownership` permit
reads only. Running home mode as root is refused; use the permissions in the standalone
guide.

Commands without a dedicated text form print their result as indented JSON. With `--json`
they print the envelope described under [Conventions](#conventions) on one line.

Aliases accepted by both parsers are `--dest` for `--out`, `--tape-uuid` for `--tape`, and
`--ack-tape-uuid` for `--acknowledge-tape`, on verbs with those options. The library parser
keeps its own canonical spellings and accepts the corresponding shorter names. Aliases do
not make the library finalize defaults or label handling the same as standalone.

### rem setup

Check this host and save a default standalone drive.

```text
rem setup [OPTIONS]
```

- `--yes` accepts the drive selection and any ownership assertion without prompting.

The command runs `rem doctor` and discovery, shows the selected drive, and after
confirmation saves `[home] drive_serial` in the home configuration. When discovery is
incomplete it asks for a separate ownership assertion; `--yes` accepts both. It reads
device identity only and does not touch a cartridge.

To check the host, choose its drive and save it:

```sh
rem setup
```

On success it prints `saved <VENDOR PRODUCT> serial <DRIVE_SERIAL> as this host's drive`.

### rem doctor

Check that this host is ready to use a standalone drive.

```text
rem doctor [OPTIONS]
```

The command checks the effective group, the binary's capability, the lock directory, the
state directory, discovery, active fences and the memory-lock limit. It loads no tape. It
prints a table with one row per check: a mark (passed, failed, or `!` for an advisory
warning), the check's name, and its detail with a `fix:` where one applies. The last line is
`ready to use the drive` or `fix the items above`. The memory-lock check is advisory and
never blocks readiness.

To check the host:

```sh
rem doctor
```

### rem list

List committed archive paths from the local catalog.

```text
rem list [OPTIONS]
```

- `--tape <TAPE>` keeps only entries on this tape, by label or tape UUID.
- `--path-prefix <PATH_PREFIX>` keeps only archive paths that begin with this prefix.

The command reads the catalog only; it needs no device. It prints a table with the columns
`POOL`, `PATH`, `SIZE`, `VERSIONS`, `NEWEST OBJECT`, `TAPES` and `FINALIZED`. `FINALIZED`
is true when every tape that holds the path is finalized.

To list everything archived under `photos/`:

```sh
rem list --path-prefix photos/
```

### rem pool new

Create a tape pool in the standalone configuration.

```text
rem pool new [OPTIONS] <NAME>
```

- `<NAME>` is the new pool's id.
- `--scheme <SCHEME>` chooses the parity geometry. The default for home-mode pools is
  `custom:128,4,1`; `--scheme default` selects the block-size-aware library geometry.
- `--block-size <BLOCK_SIZE>` sets the fixed tape block size in bytes.

At 256 KiB blocks the home default tolerates the loss of 1 MiB of contiguous data, against
512 MiB for the library geometry, and costs less parity for small writes.

To create a pool `archive` with the library geometry:

```sh
rem pool new archive --scheme default
```

### rem tape init (standalone)

Initialize the loaded cartridge with an operator label.

```text
rem tape init [OPTIONS] --label <LABEL>
```

- `--label <LABEL>` is the operator label printed on the cartridge. It must be unique in
  the catalog.
- `--pool <POOL>` selects the tape pool. Default `default`.
- `--generation <GENERATION>` names the cartridge generation, such as `L8`.
- `--block-size <BLOCK_SIZE>` sets the fixed tape block size in bytes.
- `--clobber-data` erases foreign or existing data after the exact label confirmation.
- `--confirm-clobber <CONFIRM_CLOBBER>` gives that confirmation non-interactively:
  `CLOBBER` followed by the exact label.
- `--clobber-unknown-tape <UUID>` erases a Remanence tape this catalog does not know. It
  names the exact tape UUID found at BOT (beginning of tape), and requires `--clobber-data`
  and its confirmation.

The command writes the bootstrap at BOT immediately; there is no dry run. Nonblank media,
including media whose identity was retired, requires the clobber confirmation, and the other
safety refusals still apply. A Remanence tape whose tape UUID this catalog does not know is
refused: adopt it with `rem tape adopt` (the default), or erase it with
`--clobber-data --clobber-unknown-tape <UUID>` naming that exact UUID, plus the clobber
confirmation. The refusal prints the UUID to copy. A different UUID, or the flag on any
other tape, refuses without writing. A label that already belongs to another tape is refused
with `label <LABEL> belongs to tape <UUID>; use tape rebind --previous-uuid <UUID> or choose a new label`.

The parser accepts `--generation` values `L1` to `L9`, `LZ` and `M8`. The drive's measured
density wins and is recorded. Without `--generation`, a label that ends in a generation code
must agree with it.

To initialize the loaded cartridge as `FIRST` in pool `archive`:

```sh
rem tape init --label FIRST --pool archive --generation L9
```

### rem tape identify

Read the loaded tape's identity and terminal summary.

```text
rem tape identify [OPTIONS]
```

The command reads the bootstrap and the terminal index, writes nothing, and prints the
facts as JSON, including `tape_uuid`, `label` and `generation`, and for a finalized tape its
finalization outcome.

### rem tape adopt

Import the loaded tape's identity into the catalog.

```text
rem tape adopt [OPTIONS]
```

- `--label <LABEL>` records an operator label for the cartridge.
- `--pool <POOL>` assigns the tape to a pool. Default `default`.

Adoption records the bootstrap identity only: no object rows, and no media rewrite. It
prints `{"adopted": <UUID>, "state": <STATE>}`. To list or restore files without the
original catalog, use `rem tape restore`.

### rem put (standalone)

Store files on the loaded tape.

```text
rem put [OPTIONS] <PATHS>...
```

- `<PATHS>...` are files or directories; directories are walked recursively and each
  regular file becomes one object.
- `--pool <POOL>` selects the pool, which must match the loaded tape.
- `--recipient <RECIPIENT>`, repeated 2 to 8 times, encrypts each object for these
  recipient public keys.
- `--allow-plaintext-copy` permits a plaintext version when the newest committed version of
  the path is encrypted.
- `--tape <TAPE>` names the tape you expect, by label or tape UUID, as an identity guard.

Put compares size and SHA-256 against the newest committed version in the pool, ordered by
creation time and then object row order. Equal newest content is skipped; a change writes a
new object UUID, even when it matches an older version. The loaded cartridge must be
positively reported as rewritable; WORM (write once, read many) or unknown media are refused
before the drive mode changes.

Each file produces one receipt line, `written`, `already_stored`, `skipped_unsupported` or
`refused`, and the command ends with a `Total:` line. On a capacity refusal the summary
tells you to `finalize this tape, init the next in pool <POOL>, and re-run the same put`.
`--json` streams one JSON line per receipt and one summary line, then the envelope with all
receipts.

To store the directory `photos` in pool `archive`:

```sh
rem put photos --pool archive
```

The output has this form (values illustrative):

```text
written              photos/img-0001.raw  1b4e28ba-2fa1-41d2-883f-0016d3cca427  FIRST (5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f)  300000 bytes
written              photos/notes.txt  6fa459ea-ee8a-4ca4-894e-db77e160355e  FIRST (5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f)  11 bytes
Total: 2 written, 0 already_stored, 0 skipped, 0 refused
```

Reads report active tape-I/O fences and remain available. Writes refuse fences
on the loaded tape or endpoint. Append admission also checks the physical tail:
`TailBeyondKnownState` means local state cannot authorize overwriting complete
data. Restore newer state or rescue with `tape restore --scan` before considering
supersede-tail.

### rem get (standalone)

Restore an object, or the newest version of an archive path, from the loaded tape.

```text
rem get [OPTIONS] --out <OUT> <OBJECT>
```

- `<OBJECT>` is an object UUID or an archive path.
- `--out <OUT>` (alias `--dest`) is the destination directory.
- `--version <VERSION>` selects an older version by its object UUID.
- `--private-key <PRIVATE_KEY>` decrypts with this REMP file.

The command stages each file on the destination filesystem and restores its recorded
modification time. Given a path, it restores the newest committed version on the loaded tape.

To restore `photos/notes.txt` into `restored`:

```sh
rem get photos/notes.txt --out restored
```

### rem tape verify

Verify the loaded tape.

```text
rem tape verify [OPTIONS]
```

- `--full` verifies all sidecar-protected data and parity, not only the structure.

Without `--full` the command checks the bootstrap and the terminal structure, which usually
takes minutes. With `--full` it walks the whole tape and prints the I/O report: per-phase
timing in the `io` JSON object, and `io:` and `io phase` lines in text. A full verification
that does not complete exits non-zero with `error: full verification incomplete`.

### rem tape restore

List or restore archive members from the loaded tape without the catalog.

```text
rem tape restore [OPTIONS]
```

- `--out <OUT>` (alias `--dest`) is the destination; required unless listing.
- `--list` lists recoverable members without extracting them.
- `--path <PATH>` restores only this member path; repeat it to select several.
- `--private-key <PRIVATE_KEY>` decrypts with this REMP file. Listing the members of an
  encrypted object also needs the key.
- `--scan` walks the tape from BOT instead of using the terminal index.

Each recovered object is restored into its own directory under `--out`, named `candidate-1`,
`candidate-2` and so on, with a `manifest.json` that records its object id, tape file number
and files written. The command exits non-zero with `restore incomplete:` when any candidate
was not restored.

### rem tape finalize (standalone)

Close the loaded tape and write its terminal index.

```text
rem tape finalize [OPTIONS] --acknowledge-tape <ACKNOWLEDGE_TAPE>
```

- `--acknowledge-tape <ACKNOWLEDGE_TAPE>` confirms the loaded tape by label or tape UUID.
- `--tape <TAPE>` selects or verifies the tape identity. Default: the loaded tape's UUID.
- `--reason <REASON>` records a reason. Default `finalized with rem`.
- `--idempotency-key <IDEMPOTENCY_KEY>` overrides the request key. The default is a stable
  UUIDv5 derived from the tape UUID and a fixed namespace, so a retry deduplicates.
- `--expected-pool <EXPECTED_POOL>` requires this pool instead of the catalogued one.

Finalization is permanent: the tape accepts no more objects afterwards.

To finalize the loaded tape `FIRST`:

```sh
rem tape finalize --acknowledge-tape FIRST
```

### rem tape assign-pool

Change a tape's catalogued pool assignment.

```text
rem tape assign-pool [OPTIONS]
```

- `--tape <TAPE>` names the tape by UUID; the change is then made offline. Without it, the
  command reads the loaded tape's identity.
- `--pool <POOL>` assigns the tape to this pool.
- `--unassign` removes the tape from its pool. One of `--pool` and `--unassign` is
  required.

It prints `{"tape": <UUID>, "pool": <POOL>}`.

### rem tape set-generation

Record the loaded cartridge's generation.

```text
rem tape set-generation [OPTIONS] --generation <GENERATION>
```

- `--generation <GENERATION>` names the generation, such as `L8`. The parser accepts `L1`
  to `L9`, `LZ` and `M8`.

It prints `{"generation": <GENERATION>}`.

### rem tape rebind

Move an operator label from one live tape identity to the loaded tape.

```text
rem tape rebind [OPTIONS] --label <LABEL> --previous-uuid <PREVIOUS_UUID>
```

- `--label <LABEL>` is the label to move.
- `--previous-uuid <PREVIOUS_UUID>` is the tape UUID that holds the label now. The command
  proceeds only if the catalog pairs the label with exactly this UUID.

The loaded tape must be a catalogued identity that is not retired. The rebind is an audit
record (`TapeLabelRebound`) that catalog rebuild replays; neither identity changes state.
It prints `{"label": <LABEL>}`. The refusals are
`tape provisioning conflict: previous UUID does not match the current label binding` and
`tape provisioning conflict: rebind target must be an existing live identity`. See
[Tape identity and the retire operation](tape-identity-lifecycle-explainer.md) for when to
rebind and when to retire.

To give the label `FIRST`, now held by tape `5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f`, to the
loaded tape:

```sh
rem tape rebind --label FIRST --previous-uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f
```

### rem tape import-journals

Import tape logs from another state directory for append recovery.

```text
rem tape import-journals [OPTIONS] --from <FROM>
```

- `--from <FROM>` is the directory to import from.

The name keeps an older word: the command validates the loaded tape's tape log in
`<FROM>/tapes/` and imports it. The tape must be adopted first. It prints
`{"imported_from": <FROM>}`.

### rem tape supersede-tail (standalone)

```text
rem tape supersede-tail --acknowledge-tape <LABEL|UUID> [--confirm-supersede "SUPERSEDE <LABEL>"]
```

List and explicitly discard the suffix beyond local authority, with typed confirmation and audit. Rescue files first.

### rem tape quarantine (standalone)

List and release tape fences in the home state.

```text
rem tape quarantine list [OPTIONS]
rem tape quarantine release [OPTIONS] --ack <ACK> <ID>
```

- `<ID>` is the quarantine id of the fence to release.
- `--ack <ACK>` records why the fence is being released. It must not be empty.

`list` prints the active tape-I/O fences and media-readiness records. `release` goes
through the configured daemon socket when it is listening; otherwise it takes the exclusive
offline state lock. The acknowledgement is durable before the fence clears, and release does
not bypass append admission. A fence raised by a library daemon must be released on the host
that owns it.

### rem tape release

Clear the drive's medium-removal guard.

```text
rem tape release [OPTIONS]
```

An interrupted command can leave the drive refusing to eject the cartridge. This command
clears that guard only; quarantine fences stay active.

### rem diagnostics capture

Capture diagnostic evidence into a tar file.

```text
rem diagnostics capture [OPTIONS] --out <OUT>
```

- `--out <OUT>` is the tar archive to write.
- `--device` and `--allow-device` select the drive to describe.

The command reads no tape data and does not position the media.

To capture diagnostics for the drive at `/dev/sg5`:

```sh
rem diagnostics capture --out rem-diagnostics.tar --device /dev/sg5 --allow-device /dev/sg5
```

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 625f7044 -->
## Choosing a verification command

| Command | What it reads | Existing catalog needed? | Daemon? | Duration |
|---|---|---|---|---|
| Standalone `tape verify` | Bootstrap and terminal structure and index | No object catalog | No | Usually minutes; positioning dominates |
| Standalone `tape verify --full` | Full structural walk, all sidecar-protected data and parity | No object catalog | No | Hours on a full tape |
| Library `tape inventory --tape-uuid <UUID>` | Bootstrap and usable terminal index; walk from BOT if no index works | Yes, for tape selection | Yes | Usually minutes; the fallback can take much longer |
| Library `tape verify-index --tape-uuid <UUID>` | Full structural walk, all sidecar-protected data and parity | Yes, for tape selection | Yes | Hours on a full tape |

On a tape without parity, the full structural check does not checksum object interiors,
because there are no sidecar-protected blocks. Inventory is not a content verification.
Standalone verify still opens local operational state, but does not require catalogued
object locations.

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 625f7044 -->
## Daemon queries

All of these speak gRPC to `rem-daemon` and take `--endpoint <URI>` and `--json`. None of
them moves media.

### rem daemon health, rem daemon version

Report the daemon's health, or its version.

```text
rem daemon health [OPTIONS]
rem daemon version [OPTIONS]
```

`health` prints `status:` (`healthy`, `read_only`, `degraded` or `failed`), an optional
`detail:`, and a `components:` list with one status per component. Its JSON schema is
`rem.daemon.health.v1` and includes the active tape-I/O fence count and records. `version`
prints `daemon:`, `api:` and `target:` lines; its schema is `rem.daemon.version.v1`.

To check that the daemon is up:

```sh
rem daemon health
```

### rem op list, rem op get

List the daemon's tracked operations, or show one.

```text
rem op list [OPTIONS]
rem op get [OPTIONS] <OPERATION_ID>
```

Each operation prints as one line, `<UUID>  <STATE>  kind=<KIND>  updated=<TIME>`, where the
state is one of `queued`, `running`, `succeeded`, `failed`, `cancelled`,
`completion_unknown` and `legacy_unknown`. `get` adds its `progress:` keys and any `error:`.
The schemas are `rem.operation.status.list.v1` and `rem.operation.status.v1`. An empty list
prints `(no operations)`. `rem operation` is an accepted alias of `rem op`.

### rem op watch, rem op cancel

Follow an operation to completion, or ask it to stop.

```text
rem op watch [OPTIONS] <OPERATION_ID>
rem op cancel [OPTIONS] <OPERATION_ID>
```

- `--force` (on `cancel`) sends `force=true`; without it the request is `force=false`.

`watch` streams progress until the operation reaches a terminal state. `cancel` requests
cooperative cancellation: an in-flight tape command finishes before the cancellation takes
effect.

To follow a detached verification:

```sh
rem op watch 9b1d2c3e-4f50-4a61-8b72-93a4b5c6d7e8
```

### rem catalog tapes, rem catalog tape

List the catalogued tapes, or show one.

```text
rem catalog tapes [OPTIONS]
rem catalog tape [OPTIONS] <TAPE_UUID>
```

- `--pool <POOL>` keeps only tapes in this pool.
- `--kind <KIND>` keeps tapes of this kind: `data` (default), `cleaning`, or `all`.

Each tape prints as one line:

```text
5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f  RMJ101L9  rem-object-v1  kind=data  state=ready  pool=archive  last_file=14  last_verified_at=-
```

A value the catalog does not have prints as `-`; `last_file=-` means no tape file is
committed yet. The JSON schemas are `rem.catalog.tape.list.v1` and `rem.catalog.tape.v1`.
JSON also carries `last_verification_state` (the state name) and
`last_verified_edition_digest` (hexadecimal); an absent value means no full-verification
verdict is recorded. The list includes `active_tape_io_fence_count` and
`active_tape_io_fences`, with drive-only fences even when no tape matches the filter. Each
fence record identifies the quarantine, tape UUID, barcode, endpoint, reason and state; the
daemon's release RPC is described in [the API reference](reference-api.md).

To list the tapes in pool `archive`:

```sh
rem catalog tapes --pool archive
```

### rem catalog tape-files

List the tape files the catalog records for one tape.

```text
rem catalog tape-files [OPTIONS] <TAPE_UUID>
```

### rem catalog pools, rem catalog pool

List the tape pools and their membership, or show one pool.

```text
rem catalog pools [OPTIONS]
rem catalog pool [OPTIONS] <POOL_ID>
```

### rem catalog units, rem catalog unit, rem catalog entries

List catalog units, show one, or list the entries inside one.

```text
rem catalog units [OPTIONS]
rem catalog unit [OPTIONS] <UNIT_ID>
rem catalog entries [OPTIONS] <UNIT_ID>
```

- `--origin <ORIGIN>` (on `units`) filters by origin: `all` (default), `native` (objects
  Remanence wrote) or `foreign` (archives scanned through an adapter).

### rem top

Show live daemon state.

```text
rem top [OPTIONS]
```

- `--once` fetches one snapshot and exits.
- `--json` emits JSON instead of the TUI.

In the TUI, `l` cycles the focused library when more than one is configured, `s` toggles the
slot view, `p` or space pauses, and `?` shows help. Live status refreshes fence records even
when the inventory snapshot is cached. The daemon's live status also carries an advisory
`drive_assignments` projection (busy or idle per drive bay, keyed by library serial and
bay) for an external scheduling client; `rem top` does not render it, and it is available
only over gRPC.

`rem top` shows unprotected bytes, the UTC time since they became unprotected,
and `OVERDUE` when their age reaches `daemon.closeout_alarm_after_secs`. Its
wide drive table adds `unprot bytes` and `since` columns; narrow terminals keep
these details in the separate debt list. The debt band shows barcodes and UUIDs,
uses at most five rows, and adds `+N more` when needed. Its debt list includes
unseated, unavailable and out-of-scope tapes. Media maintenance stays within the
configured library/drive scope; deferred SQL projection can retry without media.

An unreadable native log appears as `DEBT?` with its error. `rem tape list` and
`rem top --once` print the complete response and exit nonzero if any authority
is unreadable. Interactive top keeps running. JSON debt values are `null` when
unknown, with `closeout_authority_error` explaining why. Known byte counts are
decimal strings, `unprotected_since` is RFC3339 UTC or null, and `closeout_due`
and `closeout_overdue` are booleans. Vacant drives have absent debt values.
`closeout_debt` contains catalog-shaped entries, including unknown tapes.

`OVERDUE` can coexist with `closeout_due=false` when an accepted finalization
plan owns the range. That tape needs finalization to resume, not another
`rem tape closeout`. The original unprotected age still determines the alarm.
`closeout_retry_reason` shows the last worker refusal. Busy drives and memory
shortages retry with capped backoff; write protection, fences, wrong media and
identity mismatches wait for fence release, inventory change or operator
closeout. Pinned writes, closeout and finalization wait for a worker holding
their tape. Status reads during a checkpoint return the last committed summary.

### rem alarms, rem alarms ack

List standing alarms, or acknowledge one.

```text
rem alarms [OPTIONS] [COMMAND]
rem alarms ack [OPTIONS] <CONDITION_KEY>
```

- `--all` includes cleared alarms.

### rem audit query

Stream audit-log entries for a time interval.

```text
rem audit query [OPTIONS] --since <RFC3339> --until <RFC3339>
```

- `--since <RFC3339>` is the inclusive lower bound.
- `--until <RFC3339>` is the exclusive upper bound.
- `--filter <K=V>` keeps entries whose field `K` equals `V` exactly; repeat it for several
  fields. The keys are `event` (also spelled `event_kind` or `kind`), `actor`,
  `source_layer`, `subject_kind`, `subject_id`, `session_id` and `operation_id`; any other
  key is refused.

`--json` prints one JSON object per entry.

To see the retirements recorded on 2 November 2026:

```sh
rem audit query --since 2026-11-02T00:00:00Z --until 2026-11-03T00:00:00Z --filter event=TapeRetired
```

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 625f7044 -->
## Drive stewardship

Drive-fleet management through the daemon. A drive is addressed by serial or UUID.
`--allow-derived-identity`, where offered, permits changing a drive row whose identity was
derived rather than read from the device.

### rem drive list, rem drive show

List the catalogued drives, or show one drive's stewardship record.

```text
rem drive list [OPTIONS]
rem drive show [OPTIONS] <DRIVE>
```

- `--foreign` (on `list`) includes drives that belong to other systems.
- `--retired` (on `list`) includes retired drives.

### rem drive history

Show one drive's lifecycle history.

```text
rem drive history [OPTIONS] <DRIVE>
```

- `--events` includes observational events.
- `--snapshots` includes health snapshots.

### rem drive alerts

Show one drive's active TapeAlert flags.

```text
rem drive alerts [OPTIONS] <DRIVE>
```

`rem tape alerts <DRIVE>` is a deprecated alias with the same arguments; it has no `--bay`.
Reading the TapeAlert page directly by drive bay is `rem-debug tape alerts --bay <BAY>`.

### rem drive poll, rem drive clean

Poll a drive's health now, or run a cleaning cycle now.

```text
rem drive poll [OPTIONS] <DRIVE>
rem drive clean [OPTIONS] <DRIVE>
```

### rem drive annotate

Record purchase, warranty, cost and notes for a drive.

```text
rem drive annotate [OPTIONS] <DRIVE>
```

- `--purchase-date <PURCHASE_DATE>` and `--warranty-until <WARRANTY_UNTIL>` take
  `YYYY-MM-DD` dates.
- `--cost <COST>` is a display-only string.
- `--note <NOTE>` appends a timestamped note; `--notes-set <NOTES_SET>` replaces the notes.
- `--allow-derived-identity` permits changing a derived-identity row.

### rem drive retire, rem drive reinstate

Remove a drive from the managed fleet, or return it to service.

```text
rem drive retire [OPTIONS] --reason <REASON> <DRIVE>
rem drive reinstate [OPTIONS] --reason <REASON> <DRIVE>
```

- `--reason <REASON>` is recorded; on `reinstate` it is recorded beside the retirement it
  reverses.
- `--allow-derived-identity` permits changing a derived-identity row.

A retired drive keeps its identity and still appears in `rem top`, marked retired, but no
selection or mount path uses it. Reinstating does not clear `fenced` or restore
`actionable`; those are separate judgements.

### rem drive load

Load a tape into a drive bay through the daemon, and wait for completion.

```text
rem drive load [OPTIONS] --library <LIBRARY> --bay <BAY> <--barcode <BARCODE>|--slot <SLOT>>
```

- `--library <LIBRARY>` takes a library serial or UUID.
- `--bay <BAY>` is the destination drive bay.
- `--barcode <BARCODE>` names the tape, found in the current slot inventory; or
  `--slot <SLOT>` names its source slot.
- `--no-wait` returns when LOAD completes, without waiting for the cartridge to become
  ready.

To load `RMJ101L9` into bay `0x0100` of library `LIB001`:

```sh
rem drive load --library LIB001 --bay 0x0100 --barcode RMJ101L9
```

### rem drive unload

```text
rem drive unload --library <SERIAL> --bay <BAY> [--dest <SLOT>]
```

Through the daemon, unload the cartridge in a drive bay to a storage slot and wait for completion. Without `--dest` the cartridge goes back to the source slot the changer recorded for it. `--library` takes a library serial or UUID. Before the cartridge leaves the drive the daemon closes any parity epoch the drive retains and reports `phase=epoch_close` in the operation progress while it does. If the close fails, or does not run (for example because the tape is fenced, the medium is not writable or the drive owner stopped), the daemon logs the reason and removes the cartridge anyway, leaving any open epoch open; the operation can still succeed, and then its progress and its finish audit carry `epoch_close=failed` (a close ran and failed) or `epoch_close=not_attempted` (no close ran) with the reason in `epoch_close_error`. Neither value says an epoch existed. The text output prints a `warning: closing the parity epoch failed on <BARCODE> (<REASON>)` or `warning: no parity epoch close ran on <BARCODE> (<REASON>)` line before the `unloaded` line. The daemon refuses with `failed_precondition` while a session or another operation holds a drive in the library; an empty bay or a full destination fails the operation. Either way the command prints the daemon's message and exits 1. On success it prints the operation and then `unloaded <BARCODE> from bay <BAY> to slot <SLOT>`; with `--json`, the `rem.operation.status.v1` item carries a `location` object with `library_serial`, `bay`, `barcode` and `slot`. A retry sends a new request. `rem-debug unload` is the break-glass form that bypasses the daemon.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-cli/src/tape_finalize.rs crates/remanence-cli/src/tape_inventory.rs crates/remanence-cli/src/tape_read_rate.rs @ 625f7044 -->
## Tape lifecycle

Initialization, readiness and retirement run against local configuration and state, and
initialization and readiness against real hardware. Initialization passes a chain of
identity, ownership, readiness and data-presence checks before it writes anything. In
library mode, finalization, inventory, verification and supersede-tail go through the
daemon and its tape ownership and fencing checks; the CLI performs no local SCSI discovery
for those verbs.

### rem tape init (library)

Initialize one tape, or every tape in a slot range.

```text
rem tape init [OPTIONS] <TARGET>
```

- `<TARGET>` is a barcode, an element address, or an inclusive slot range such as
  `0x0400..0x0407`.
- `--library <SERIAL>` selects a library when the target is not unique across libraries.
- `--dry-run` runs every check and writes nothing. On a state with no catalog yet it
  decides against an empty catalog and creates none.
- `--force` overrides only decisions classified as `require-force`.
- `--clobber-data` is the separate, stronger override for tapes that hold data. It is
  rejected for dry runs and batch initialization.
- `--confirm-clobber <CONFIRM_CLOBBER>` gives the destructive confirmation
  `CLOBBER <LABEL>` non-interactively; otherwise type or pipe the same text on standard
  input. Retired nonblank cartridges need this confirmation too.
- `--clobber-unknown-tape <UUID>` erases a Remanence tape whose tape UUID the catalog does
  not know (see below).
- `--block-size <BYTES>` sets the fixed block size of a fresh initialization.
- `--parity <PARITY>` sets the parity geometry of a fresh initialization: `none` (the
  default) or `default`, the block-size-aware default scheme. An idempotent initialization
  keeps the geometry already recorded at BOT.

A real (not dry-run) init runs the library's startup readiness reconciliation first; it may
send one initializing SSC LOAD per operation to a fenced drive and wait up to 60 s for it.
An active tape-I/O fence that names the target barcode or the tape UUID at BOT refuses init
before any media write, dry run included. No override (`--force`, `--clobber-data`,
`--clobber-unknown-tape`) relaxes it, and the refusal names the fence and the
`rem tape quarantine release` command to run first.

A Remanence tape whose tape UUID has no catalog row decides `needs-explicit-rebuild` and
writes nothing; neither `--force` nor `--clobber-data` alone overrides it. Adopting the tape
with `rem-debug tape adopt-bootstrap` is the default, and the refusal's `next:` line names
both that command and the override. To destroy the identity instead, pass
`--clobber-unknown-tape <UUID>` with `--clobber-data` and the confirmation. The UUID, in
32-hex or hyphenated form, must equal the tape UUID at BOT exactly, or init refuses and
names both. The override never relaxes any other decision. It refuses when committed copies
still reference that UUID, re-reads BOT just before writing and refuses unless it still
carries that UUID, and is rejected for dry runs and batch initialization. The tape gets a
fresh UUID; the output's `clobbered:` line and the `TapeProvisioned` audit record
(`clobbered_identity = "unknown-to-catalog"`, `clobbered_uuid`) record the destroyed
identity.

To check what initializing `RMJ101L9` with default parity would do, without writing:

```sh
rem tape init RMJ101L9 --parity default --dry-run
```

The command prints a result block of this form (values illustrative):

```text
tape init RMJ101L9: dry-run-would-write
  uuid: 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f
  library: LIB001
  source: 0x0400; drive: 0x0100
  pool: archive
  generation: L9
  capacity_bytes: 18000000000000
  geometry: block_size=262144 parity=<SCHEME_ID>
  decision: fresh-init
```

The first line's action is one of `wrote-bootstrap`, `dry-run-would-write`,
`idempotent-no-op` and `refused-no-write`. The `decision:` line is one of `fresh-init`,
`idempotent-no-op`, `require-force: ...`, `refuse-clobber: ...`, `anomaly: ...` and
`needs-explicit-rebuild: ...`, with the reason after the colon. `note:` lines may follow.
[Tape identity and the retire operation](tape-identity-lifecycle-explainer.md) explains the
anomaly and rebuild decisions.

### rem tape wait-ready

Poll TEST UNIT READY until an already-loaded cartridge is usable.

```text
rem tape wait-ready [OPTIONS]
```

- `--barcode <BARCODE>` or `--drive-element <DRIVE_ELEMENT>` names the drive to poll; one
  of them, or `--resume`, is required.
- `--already-loaded` acknowledges that the cartridge is already in the drive.
  `--drive-element` requires it, and `--resume` refuses it.
- `--wait` keeps polling retryable states until the timeout.
- `--timeout <TIMEOUT>` is the longest wait. Default `2.5h`.
- `--poll <POLL>` is the poll interval. Default `30s`.
- `--resume <UUID>` continues a durable readiness operation without moving media.
- `--endpoint <URI>` attaches to the daemon's running wait instead of using the drive
  directly; it requires `--resume`.
- `--library <SERIAL>` selects a configured library.

TEST UNIT READY is the SCSI command that asks a drive whether it can accept media commands.
An LTO-9 cartridge's first load runs a one-time media optimization that can take hours; the
2.5-hour default timeout exists for that. With `--wait`, a drive that asks for an
initializing command (sense `02/04/02`) for a cartridge the changer places in it gets one
SSC LOAD per operation, and polling continues. Without `--endpoint` the command uses direct
SCSI and requires the daemon to be stopped. With `--endpoint` it prints each readiness state
change to standard error and leaves the daemon waiting if the local `--timeout` ends. The
exit codes are listed under [Exit codes](#exit-codes).

To wait for the cartridge `RMJ101L9` that is already in a drive:

```sh
rem tape wait-ready --barcode RMJ101L9 --already-loaded --wait
```

The command prints one line, of this form (values illustrative):

```text
ready operation_id=9b1d2c3e-4f50-4a61-8b72-93a4b5c6d7e8 library=LIB001 drive=0x0100 barcode=RMJ101L9 attempts=3 <SUMMARY>
```

The first word is `ready`, `timeout` or `not-ready`. On a non-zero exit, `operator_action:`
and `recommended_next_command:` lines follow on standard error. The JSON schema is
`rem.tape.wait_ready.v1`.

### rem tape quarantine list, show and release (library)

Inspect and release fences in the library state.

```text
rem tape quarantine list [OPTIONS]
rem tape quarantine show [OPTIONS] <QUARANTINE>
rem tape quarantine release [OPTIONS] --ack <TEXT> <QUARANTINE>
```

- `<QUARANTINE>` is a quarantine id or a media-readiness operation UUID.
- `--library <SERIAL>` (on `list`) restricts the list to one library.
- `--ack <TEXT>` (on `release`) is the acknowledgement, recorded in the audit log.
- `--after-settled-inventory` (on `release`) confirms that the library inventory has
  settled after the unknown operation.

`list` and `show` cover media-readiness fences, whose quarantine ids begin with `mrq-`.
`list` prints one line per fence with its operation, library and drive, barcode, state,
last sense and update time, for example `sense=02/04/02 (initializing command required)`;
an empty list prints `(no active media-readiness quarantines)`. `show` adds the phase, and
its JSON carries `last_sense` (`KK/AA/QQ`) and `last_sense_name` beside the raw sense
fields. The JSON schema of `list` is `rem.tape.quarantine.list.v1`.

`release` also releases tape-I/O fences, whose ids begin with `tioq-`; `rem catalog tapes`,
`rem top` and `rem daemon health` list those. A tape-I/O fence is released through the
configured daemon socket after the operator's acknowledgement. Only an absent listener
permits the offline fallback, which needs the exclusive state lock. Repeating a release
returns its original evidence. Media-readiness release keeps its existing offline
behaviour.

To release a fence after checking the drive and the slot inventory:

```sh
rem tape quarantine release mrq-9b1d2c3e-4f50-4a61-8b72-93a4b5c6d7e8 \
    --after-settled-inventory --ack "inventory settled; cartridge is in its home slot"
```

### rem tape retire

Retire one tape identity permanently in the local catalog.

```text
rem tape retire [OPTIONS] --reason <TEXT> <TARGET>
```

- `<TARGET>` is a barcode or a 32-hex tape UUID.
- `--reason <TEXT>` is free text recorded in the audit log, such as `recycled`.
- `--i-understand-copies-become-unreadable` is required on every run, dry run included.
- `--dry-run` reports what would be retired without writing. It opens the catalog read-only
  and takes no state lock.
- `--json` emits `rem.tape.retire.v1`.

Every copy on the tape becomes `missing` in the catalog, and the barcode is released. The
command touches no media. A retire that changes anything takes the state lock, so stop the
daemon first. [Tape identity and the retire operation](tape-identity-lifecycle-explainer.md)
explains the effects and how they survive a catalog rebuild.

To see what retiring `RMJ101L9` would do:

```sh
rem tape retire RMJ101L9 --reason recycled --i-understand-copies-become-unreadable --dry-run
```

The command prints, for example:

```text
dry-run: would retire RMJ101L9 (uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f): 2 copies would be marked missing, 1 object would become degraded
  would lose last committed copy: <OBJECT_ID>
```

Without `--dry-run` the first line is
`retired RMJ101L9 (uuid ...): 2 copies marked missing, 1 object now degraded`, followed on
standard error by a `warning:` line naming the objects that lost their last committed copy.
An identity that is already retired prints `... is already retired; no change`.

### rem tape finalize (library)

Finalize one exact tape through the daemon.

```text
rem tape finalize [OPTIONS] --tape-uuid <UUID> --reason <EXACT> --idempotency-key <UUID> --ack-tape-uuid <UUID>
```

- `--tape-uuid <UUID>` is the exact tape UUID. Barcodes are not accepted.
- `--ack-tape-uuid <UUID>` acknowledges the same UUID explicitly.
- `--expected-pool <ID>` is the tape's current canonical pool id, required for a pooled
  tape.
- `--reason <EXACT>` is the reason, sent exactly as supplied.
- `--idempotency-key <UUID>` deduplicates retries (see [Placeholders](#placeholders)).
- `--wait` polls until the daemon reports a terminal outcome.

Finalization is irreversible, and works below the automatic low watermark too. Without
`--wait` the command submits once and prints the current status. With it, the command polls
the read-only status RPC across daemon reconnects until a terminal outcome and never
resubmits. While the tape is `FINALIZING` it also checks the operation, and exits non-zero
with its summary on `FAILED`, `CANCELLED` or `COMPLETION_UNKNOWN`. Reuse the same
idempotency key to resume the durable request under the same operation id.

The JSON schema is `rem.tape.finalization.v2`. Its `replica_progress` is historical barrier
evidence, not a current media-health observation. A value the daemon does not have is
`null`, never `0`: `operation_id` is `null` for a finalization that an automatic trigger
started, and a BUSY response has `null` `operation_id`, `completed_replicas` and digests.
`completed_replicas` `0` is a real value (before replica A).

To finalize tape `5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f` in pool `archive` and wait:

```sh
rem tape finalize --tape-uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f \
    --ack-tape-uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f --expected-pool archive \
    --reason "pool archive: tape full" \
    --idempotency-key 2c6f1e4a-8b3d-4f7e-9a10-5d2c7b8e9f01 --wait
```

### rem tape supersede-tail (library)

```text
rem tape supersede-tail --tape-uuid <UUID> --ack-tape-uuid <UUID> [--confirm-supersede <TEXT>]
```

Through the daemon, list and explicitly discard a suffix beyond checkpoint authority using the same append checks. Rescue first; confirm the exact text requested.

### rem tape inventory

Read a tape's terminal-index inventory: the quick structural check.

```text
rem tape inventory [OPTIONS] --tape-uuid <UUID>
```

- `--tape-uuid <UUID>` is the exact tape UUID. Barcodes are not accepted.
- `--json` emits NDJSON events (see below).

When the terminal index is readable, the command reads the first record of the tape and the
terminal replicas at the end of the data, and no object data. It locates EOD (end of data)
and selects replica C, then B, then A. If the terminal index cannot be used, it falls back
to a walk from the beginning of the tape that reads records throughout the prefix; that walk
classifies tape files and is not a content verification. `verify-index` below is the
whole-tape check.

Human output labels every row before the summary `provisional`, and the final summary names
the authoritative `attempt_id`. `--json` emits NDJSON events under
`rem.tape.inventory.stream.v1` and ends with exactly one `summary` event. Consumers must
commit only that summary's selected attempt. Rejected attempts make fallback evidence
explicit. When no replica survives, a `bot_recovery_started` notice is followed by
`bot_recovery_progress` events for each tape file and by streamed BOT classifications,
rather than an empty success. Cancellation is honoured at those boundaries between files.
During the walk from BOT, an exact surviving tape log can classify matching complete
objects as recovered; later or foreign objects without that authority remain unknown. In
the summary, a selection, count or digest the outcome does not carry is `null`, never
`"0"`: a BOT recovery that is required has no counts, fast outcomes have no BOT
classification counts, and a torn BOT object's `stored_block_count` is `null`.

### rem tape verify-index

Run a full verification of one tape through the daemon.

```text
rem tape verify-index [OPTIONS]
```

- `--tape-uuid <UUID>` is the exact tape UUID. Barcodes are refused.
- `--ready-wait <DURATION>` is how long to wait for a media-readiness fence before
  retrying. Default `900s`; `0` returns the refusal at once.
- `--detach` starts a watchable `tape_verification` operation and prints its UUID.
- `--result <OPERATION_ID>` fetches a retained verdict by operation UUID.
- `--json` emits `rem.tape.verify-index.v1`.

The command verifies the tape's structure and, on a tape written with parity, reads and
checksums all the data that the parity sidecars protect and all the parity shards. This is
a full verification in the sense of REM-PARITY Section 2.2: it measures EOD, walks the
prefix, compares the canonical map, validates all three replicas and both separation
extents, and reads every protected data block and every parity shard, checking each against
its sidecar's index. On a tape written without parity there is no sidecar-protected
content, so this pass does not read or checksum object interiors; only the structure and the
terminal index are verified. There is no shallow option; `rem tape inventory` is the quicker
structural check. Use `verify-index` for the periodic check of an archive and for any tape
that is suspect.

A finding about any data block, parity shard, sidecar copy or footer, ParityMap copy or
footer, or the bootstrap's trailing fill leaves the tape not complete: the state is
`verified_degraded` and `complete` is `false`. `verified` says that the verification ran to
the end; `complete` says whether the tape passed. The findings are printed as described
under [Recovery report and verification output](#recovery-report-and-verification-output).
When every replica is invalid, the walk from BOT applies the same recovered, unknown and
incomplete classification as inventory. `verified_interior_record_count` is present only for
a valid separation extent (`"0"` for a two-record extent) and `null` for an invalid or
unknown one; the verified-prefix counts and digests are `null` when recovery is required.

When a media-readiness fence blocks the tape, the command watches it locally for up to the
`--ready-wait` time, then retries verification once the drive reports ready. The daemon
resumes the wait with a 9000-second timeout independent of `--ready-wait`, and keeps waiting
after the command returns, so the fence clears when the drive becomes ready within that
timeout. An operator release is required if the daemon's wait times out. If readiness fails,
or the resume or watch fails, the command returns the original refusal with the wait error
appended after a semicolon. A busy drive bay gets one retry after 1 s.

With `--detach`, follow the operation with `rem op watch <UUID>` and stop it with
`rem op cancel <UUID> [--force]`; cancellation sends `force=false` unless `--force` is
given. With `--result`, the command prints the retained verdict in the same rendering. The
daemon keeps results in memory beside its operations, so a restart or pruning removes them,
and a running, failed or cancelled operation has no verdict.

A full verification keeps running when its client disconnects. Cancellation releases the
tape and drive and records no verdict, and a cancelled verification starts over; there is
no pause or resume. The daemon does not pre-empt verification for writes or restores.
Progress includes tape UUID, drive serial, bay, phase, tape file, bytes read and elapsed
milliseconds; unknown positions are `unknown`. Phase changes publish immediately; other
updates publish at most once every five seconds. An in-flight READ can wait for its
900-second command timeout. The catalog records the verdict state and the terminal edition
digest when available, and `rem catalog tapes` shows them.

To start a verification of tape `5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f` in the background and
follow it:

```sh
rem tape verify-index --tape-uuid 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f --detach
rem op watch <OPERATION_UUID>
```

See [Full verification](guide-full-verification.md) and [Damaged tapes](guide-damaged-tapes.md).

### rem-debug tape bot-probe

Read only the first record at BOT and report the tape's physical identity.

```text
rem-debug --allow <SERIAL> tape bot-probe [OPTIONS] --library <SERIAL> --expected-home-slot <EXPECTED_HOME_SLOT> --json <TARGET>
```

- `<TARGET>` is the exact barcode.
- `--library <SERIAL>` selects the configured library.
- `--expected-home-slot <EXPECTED_HOME_SLOT>` is the exact storage element the cartridge
  must return to.
- `--json` is required; it is the only rendering.

The probe is advisory and identity-only. Under one stable drive handle it verifies
readiness, temporarily selects a fixed 1 MiB, compression-off read mode, issues exactly one
LOCATE(0) and at most one READ, restores the prior drive mode, and parks the cartridge in
the exact expected home slot. It never reads past BOT and never consults or changes the
catalog. The JSON schema is `rem.tape.bot-probe.v1`; a valid native identity has
`physical_disposition="bootstrap_valid"`. See
[Importing and recovering Remanence tapes](importing-and-recovering-remanence-tapes.md).

### rem-debug tape adopt-bootstrap

Adopt a checksum-valid existing bootstrap without writing tape.

```text
rem-debug --allow <SERIAL> tape adopt-bootstrap [OPTIONS] --expected-existing-tape-uuid <UUID> --expected-home-slot <EXPECTED_HOME_SLOT> --library <SERIAL> --json <TARGET>
```

- `<TARGET>` is the exact barcode.
- `--expected-existing-tape-uuid <UUID>` is the tape UUID the advisory probe reported.
- `--expected-home-slot` and `--library` are as for `bot-probe`.
- `--json` is required.

The command holds the exclusive state handle, runs fresh discovery, verifies the exact UUID,
home slot and library, checks that the block size the bootstrap records is one every drive in
the library supports and that the tape was written with drive compression off, restores the drive
mode, parks the medium, then records a durable `TapeIdentityAdopted` event and an
identity-only catalog row. A tape with exactly bootstrap, filemark and EOD becomes `ready`;
every other tail after a valid bootstrap becomes `recovery_required`. The command generates
its own operation UUID; a retry may be a no-op only while the replayed row is still the
exact same identity-only state. The JSON schema is `rem.tape.adopt-bootstrap.v1`; a repeat
adoption reports `physical_disposition: "bootstrap_already_adopted"` and
`newly_adopted: false`. See
[Importing and recovering Remanence tapes](importing-and-recovering-remanence-tapes.md).

### rem-debug tape recovery-report

Produce a read-only recovery report for a tape without using the catalog.

```text
rem-debug tape recovery-report [OPTIONS] <SOURCE>
```

- `<SOURCE>` is either the SCSI generic path of the drive that holds the tape (for example
  `/dev/sg5`) or a directory that holds a tape image in the published layout, with files
  named `tape-file-NNN-*.bin`. It is not a tape UUID, and a single image file is not
  accepted.
- `--tape-uuid <TAPE_UUID>`, `--block-size <BLOCK_SIZE>` and `--scheme <SCHEME>` are hints
  used when the tape's bootstrap cannot be read. `--block-size` is the fixed block size in
  bytes (256 KiB, 512 KiB or 1 MiB; `KiB` and `MiB` suffixes are accepted). `--scheme` is the
  parity scheme as `k,m,S` (data blocks per stripe, parity blocks per stripe, stripes per
  neighborhood, for the scheme `rs-cauchy-gf256-v1`), or `none` for a tape written without
  parity. Supply all three or none; when the bootstrap is readable, leave them out.
- `--json` emits the machine-readable report.

The report performs terminal discovery first, through the same library entry as the
daemon's inventory, and walks from BOT only when discovery yields no inventory. Fields
derived from the bootstrap are `null` when it is unreadable; the hints are recorded
separately under `supplied`, and the scan records when they provided identity and geometry
because the bootstrap was treated as unreadable. A valid terminal replica can supply object
identifiers, while a walk from BOT without separate host authority reports complete
candidates as unknown. The command does not adopt the tape or rebuild the catalog. A report
that contains failures exits 2. What it prints when it cannot use the bootstrap is under
[Recovery report and verification output](#recovery-report-and-verification-output).

To report on a tape image in the directory `image`:

```sh
rem-debug tape recovery-report image --json
```

### rem-debug tape read-rate

Measure forward read or space throughput across a run of tape files.

```text
rem-debug --allow <SERIAL> tape read-rate [OPTIONS] --library <SERIAL> --expected-home-slot <EXPECTED_HOME_SLOT> --json --files <FILES> --mode <MODE> <TARGET>
```

- `<TARGET>` is the exact barcode.
- `--library` and `--expected-home-slot` are as for `bot-probe`.
- `--from-file <FROM_FILE>` is the zero-based tape file to start at (file 0 holds the
  bootstrap). Default `1`.
- `--files <FILES>` is the number of tape files to measure.
- `--mode <MODE>` is `read` (read the records through the filemarks) or `space` (space
  over whole files).
- `--json` is required.

The command is read-only. It takes the state lock, so the daemon must be stopped. It loads the
cartridge into a drive that can read its generation, requires a valid Remanence bootstrap,
measures with fixed-block READ or SPACE (the SCSI command that moves over
records or filemarks without reading them), and parks the cartridge in its home slot. It
performs no seek fallback and no record retry. A medium error, a wrong length or end of data
during measurement exits 0 with `ok: false` and the outcome in the JSON; command and cleanup
errors exit non-zero. The JSON schema is `rem.tape.read-rate.v1`, with per-file and total
`bytes`, `elapsed_seconds`, `mb_per_second` and `seconds_per_gib`, and the boundary gaps
between files.

To measure reading tape files 1 to 20 of `RMJ101L9`:

```sh
rem-debug --allow LIB001 tape read-rate RMJ101L9 --library LIB001 \
    --expected-home-slot 0x0400 --files 20 --mode read --json
```

<!-- code-anchor: crates/remanence-cli/src/put.rs @ 625f7044 -->
## Writing to tape

### rem put (library)

Archive local files onto tape through the daemon.

```text
rem put [OPTIONS] <PATH>...
```

- `<PATH>...` are files or directories. Directories are walked recursively, `tar`-style,
  and each regular file becomes one object whose single member carries the file's archive
  path. Archive-path collisions, `..` components and non-UTF-8 paths are refused;
  non-regular files are skipped with a warning.
- `--pool <POOL_ID>` is the target pool. When it is omitted and exactly one pool is
  configured, that pool is used; with several, the choice is real policy and must be
  explicit. With `--tape` it is the mandatory guard: the pool you believe that tape belongs
  to.
- `--tape <UUID>` pins one tape instead of letting pool policy choose (see below).
- `--library <SERIAL>` restricts selection and mounting to one configured library. It cannot
  be combined with `--tape`, which already implies the library.
- `--id <ID>` is the caller object id recorded in the catalog. It needs exactly one input
  file; the default is the member's archive path.
- `--meta <KEY=VALUE>` records extra caller metadata; repeat it for several keys. The `path`
  key is reserved, because it carries the archive path.
- `--stored-object` and `--stored-object-id <UUID>` write an already-built object (see
  below).
- `--chunk-bytes <BYTES>` is the append stream's chunk size. Default `1048576` (1 MiB).
- `--no-wait` fails at once instead of watching a media-readiness load.
- `--drive <ELEMENT>` is accepted by the parser and rejected by the daemon, which opens pool
  and tape targets only.

The command uses the same write-session path an orchestrator uses, so pool policy, capacity
reserve, media-readiness fences and checkpoint durability apply unchanged. Without `--tape`,
the daemon may choose an eligible cartridge in any configured library. The selected tape's
barcode must appear in exactly one operated library before robotics begins. The daemon loads
it into a free drive in that library and seals tapes by watermark. If the open is blocked by
a media-readiness operation (a cold tape load), put watches it to completion and retries
once. The loaded cartridge must be positively reported as rewritable (`NotWorm`); WORM or
unknown media are refused before the drive mode changes, because restart recovery may need
to replace an uncommitted tail.

With `--tape`, pinning replaces pool selection but not admission. The tape still passes
every pool eligibility check (data kind, lifecycle state, block size, fences, checkpoint
batch eligibility), and a pool mismatch is refused with a message naming both pools,
because pools carry copy-class separation. A cartridge that was never initialized has no
UUID to pin; `rem tape init` assigns the tape UUID.

`--stored-object` validates one already-built plaintext REM-OBJECT object, then writes its
fixed blocks byte for byte through the normal write path. The `--stored-object-id` UUID and
the `--id` caller id must match the identities embedded in the object. The object must use
the selected tape block size, exactly match the deterministic writer output, contain a valid
final manifest and regular payload members, and have canonical zero fill after tar EOF. It
is fully spooled and validated before tape moves, and it is never wrapped inside another
object. An exact retry with the same pool, caller id, input kind, content and stored
representation is idempotent; changing the input kind or stored representation under the
same caller id is refused, both before and after the checkpoint. This surface refuses an
object UUID that already exists outside that exact retry, rather than attaching an additional
copy after media motion. Targeting a tape with no pool assignment (`allow_unpooled` in the
API) is likewise rejected.

Each input file is a separate object with its own trailing filemark. To store many small
files behind one filemark, build one multi-member object with `rem archive build` and write
it with `--stored-object`, which is a library-mode option; standalone `put` does not accept
it. Every member stays separately named and checksummed in the manifest and can be restored
on its own. Each non-empty member starts on a chunk boundary and therefore costs at least
one tape block; for trees of very small files, and for files the format cannot represent
natively, see the blob wrapper in REM-OBJECT Core Format Appendix E.
[Object sizing](guide-object-sizing.md) discusses the trade-offs.

The receipt reports committed state. It is built from the daemon's checkpoint barrier, with
a catalog lookup for objects an automatic checkpoint committed earlier, and never from the
append acknowledgement alone. When some object cannot be confirmed committed, the confirmed
receipts still print, the exit status is non-zero, and the message distinguishes
`commit state unknown` (a lookup failed) from `no committed copy` (the daemon says it is not
on tape). A failed append aborts the whole batch, and the error states what the abort
achieved, including the case where the abort itself failed and the session remains
recoverable.

To archive the directory `photos` to pool `archive`:

```sh
rem put photos --pool archive
```

Progress lines go to standard error, and one receipt line per object copy and a session
line go to standard output (values illustrative):

```text
writing 2 files (293.0 KiB) to pool archive
  photos/img-0001.raw (293.0 KiB) spooled at 95 MiB/s, pending checkpoint
  photos/notes.txt (11 B) spooled at 95 MiB/s, pending checkpoint
photos/img-0001.raw  object 1b4e28ba-2fa1-41d2-883f-0016d3cca427  tape RMJ101L9 file 13  pool archive
photos/notes.txt  object 6fa459ea-ee8a-4ca4-894e-db77e160355e  tape RMJ101L9 file 14  pool archive
session 0f8c2d1e-3b4a-4c5d-8e6f-7a8b9c0d1e2f closed: 2 objects, 293.0 KiB committed
```

With `--json` the command prints one object with `session_id`, `target`,
`objects_committed`, `bytes_committed` and `objects`, where each object lists its
`archive_path` and copies.

The append locator's `append_commit_info` includes `tape_log_sequence` as a decimal string
and `tape_incarnation` as the number 1 when the catalog join is available. Both are `null`
without that join. `journal_record_ordinal` remains present and is always `null`.
Checkpoint and session ordinals keep their own meaning; they are not log sequence numbers.

<!-- code-anchor: crates/remanence-cli/src/get.rs @ 625f7044 -->
## Restoring from tape

### rem get (library)

Restore an object's members from tape through the daemon.

```text
rem get [OPTIONS] --dest <DIR> <OBJECT>
```

- `<OBJECT>` is an object UUID by default.
- `--caller-id` reads `<OBJECT>` as the caller object id recorded at put time, and
  `--pool <POOL>` names the pool that scopes it. Without `--pool`, a caller id present in
  several pools is refused with a message that names the pools; a caller id in only one pool
  still resolves.
- `--sha256` reads `<OBJECT>` as the object's content SHA-256.
- `--dest <DIR>` is the destination directory.
- `--tape <UUID>` reads the copy on this tape instead of the first catalogued copy, to
  exercise a particular cartridge or to steer away from one.
- `--path <REM_OBJECT_PATH>` restores only this member.
- `--overwrite` replaces existing destination files.
- `--stream-chunk-bytes <BYTES>` is the suggested read-stream chunk size. Default
  `1048576`.
- `--no-wait` fails at once instead of watching a media-readiness load.

The daemon resolves the object format and streams each member's payload, so the client
interprets no format. The daemon loads the copy's tape as needed and watches a
media-readiness fence to completion. Every member is proved before it is published: bytes
stream to a temporary file, are hashed as they land, and are renamed into place only when the
hash matches the catalog's SHA-256 for that file. Member paths from the catalog are sanitized
with the rules put applies on ingest (`..` is refused, absolute prefixes are stripped), so a
restore never writes outside `--dest`.

On a digest mismatch nothing is published. The temporary file is kept for inspection, and
the error names both hashes and the two possible causes: a damaged copy (treat it as
suspect, read it again or verify the tape), or an encrypted object whose ciphertext this
command does not decrypt. Restore encrypted objects with the `rem archive` tools and the
private key.

To restore object `1b4e28ba-2fa1-41d2-883f-0016d3cca427` into `restored`:

```sh
rem get 1b4e28ba-2fa1-41d2-883f-0016d3cca427 --dest restored
```

Progress goes to standard error; each member and a summary line go to standard output
(values illustrative):

```text
restoring 1 member (293.0 KiB) of object 1b4e28ba-2fa1-41d2-883f-0016d3cca427 from tape 5e8f8d3b-0c1a-4d2e-9f60-7a1b2c3d4e5f file 13
photos/img-0001.raw  293.0 KiB  sha256 864e10a02a94  → restored/photos/img-0001.raw
object 1b4e28ba-2fa1-41d2-883f-0016d3cca427 restored into restored (1 member, 293.0 KiB, all digests verified)
```

### rem archive verify

Read one object from the tape in a chosen drive bay, through the daemon, and compare its
digest.

```text
rem archive verify [OPTIONS] --library <LIBRARY> --drive <DRIVE> --locator <JSON> --expected-sha256 <HEX>
```

- `--library <LIBRARY>` takes a library serial or UUID.
- `--drive <DRIVE>` is the drive bay that holds the tape.
- `--locator <JSON>` is the locator JSON that `rem-debug archive write --json` prints.
- `--expected-sha256 <HEX>` is the expected payload SHA-256.

The command restores nothing. It has no `--private-key` option; `rem-debug archive verify`
is the direct form that takes one.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-cli/src/archive_ingest.rs crates/remanence-cli/src/archive_map.rs crates/remanence-aead/src/wrap.rs @ 625f7044 -->
## Archive objects (local, no tape)

`rem archive` builds and reads portable REM-OBJECT object files on local disk. None of these
commands touches tape; they are the easiest way to exercise the format. The examples in this
section were run in one directory, in order, against the same build; JSON output is shown
indented and trimmed where marked.

### rem archive capabilities

Print this build's REM-OBJECT capabilities as one JSON line.

```text
rem archive capabilities
```

The command does no hardware discovery, so a script can probe what a binary supports before
calling it. `wrap-suite-hpke-v1` names the HPKE (Hybrid Public Key Encryption) Base-mode
construction; the KEM (key-encapsulation mechanism) it runs is the X-Wing hybrid, not a plain
X25519 KEM despite the string (see
[the encrypted envelope](reference-tape-layout.md#the-encrypted-envelope-remo)).

```sh
rem archive capabilities
```

```text
{"capabilities":["rem-object-envelope","wrap-suite-hpke-v1","ranged-ciphertext-extract"],"observed_endpoints":[],"supported_endpoint_kinds":["library","standalone"]}
```

### rem archive formats

List the foreign-format adapters linked into this binary.

```text
rem archive formats
```

Stock Remanence links none:

```text
{"foreign_formats":[],"observed_endpoints":[],"supported_endpoint_kinds":["library","standalone"]}
```

### rem archive recipient keygen

Generate a private key file and its slot-0 public recipient file.

```text
rem archive recipient keygen [OPTIONS] --out <FILE.remp>
```

- `--out <FILE.remp>` is the new private-key file, created with mode `0600`.
- `--label <TEXT>` is stored in both files: at most 32 printable ASCII bytes. Default empty.
- `--public <FILE.remr>` is the new public-key file. Default: `--out` with the extension
  `.remr`.

The command refuses existing paths. On a terminal it prints a warning about keeping the
private file safe.

To create a key pair labelled `alice`:

```sh
rem archive recipient keygen --out alice.remp --label alice
```

```text
{"epoch_id":"5f6de43b3cc18062f4b48c6c0c6df034","label":"alice","observed_endpoints":[],"private":"alice.remp","public":"alice.remr","supported_endpoint_kinds":["library","standalone"]}
```

### rem archive recipient derive

Derive the canonical public REMR file, for any slot, from a private REMP file.

```text
rem archive recipient derive --private-key <REMP> --slot-index <0..255>
```

- `--private-key <REMP>` is the private-key file.
- `--slot-index <0..255>` is the key-frame slot to encode.

An encrypted object needs its recipients in distinct, ascending slots, and `keygen` always
writes slot 0, so a second recipient's public file is derived for slot 1. The command prints
the REMR file as standard base64 in `canonical_public_key_base64`, with its epoch identity.
It never prints the private seed. Shortened:

```sh
rem archive recipient keygen --out bob.remp --label bob
rem archive recipient derive --private-key bob.remp --slot-index 1
```

```text
{"canonical_public_key_base64":"UkVNUgFs0On/8lkAGupQ...","epoch_label":"bob","observed_endpoints":[],"recipient_epoch_id":"6cd0e9fff259001aea50119d737f9f81","slot_index":1,"supported_endpoint_kinds":["library","standalone"]}
```

Decoding `canonical_public_key_base64` gives the REMR file, here `bob-slot1.remr`.

### rem archive recipient inspect

Parse and validate a public REMR file.

```text
rem archive recipient inspect --public-key <REMR>
```

```sh
rem archive recipient inspect --public-key alice.remr
```

```text
{"epoch_label":"alice","observed_endpoints":[],"public_key_bytes":1216,"recipient_epoch_id":"5f6de43b3cc18062f4b48c6c0c6df034","slot_index":0,"supported_endpoint_kinds":["library","standalone"]}
```

### rem archive build

Build a portable REM-OBJECT object file from local inputs.

```text
rem archive build [OPTIONS]
```

- `--inputs <PATH>...` are input files or directories, expanded recursively. A directory's
  contents are stored relative to it.
- `--map <PATH>`, `--source-root <DIR>` and `--map-sha256 <HEX>` take a source map written
  by a trusted planner instead of `--inputs`.
- `--out <PATH>` is the output object file.
- `--recipient <REMR>`, repeated 2 to 8 times with distinct, nonzero epochs in ascending
  slot order, writes the encrypted envelope. Without it the output is the plaintext
  `rem-object-v1` body; the presence of recipients is the encryption switch, and there is no
  `--encrypt` flag.
- `--rules <PATH>` applies an ingest ruleset for blob and exclude policy;
  `--manifest-out <PATH>` (requires `--rules`) writes the member manifest JSON.
- Capture preserves only `user.*` extended attributes natively. Other namespaces
  are reported by name and count, without values. See [Extended attributes and
  file metadata](reference-extended-attributes.md) for ruleset selection and wrappers.
- `--scan-only` classifies the inputs and prints the ingest report without writing an
  object.
- `--no-index` skips the generated `.remwrap.idx` siblings of blob wrappers.
- `--blob-suggest-ratio <RATIO>` (default `0.9`), `--blob-suggest-count <COUNT>` (default
  `100`) and `--sanity-ceiling-count <COUNT>` (default `10000`) set the scan's
  blob-suggestion and sanity-ceiling thresholds.
- `--object-id <ID>`, `--caller-object-id <ID>`, `--manifest-file-id <ID>` and
  `--timestamp <RFC3339>` record the given values instead of fresh UUIDs and the current
  time.
- `--chunk-size <BYTES>` is the block and chunk size. Default `256KiB`.

To build a plaintext object from the directory `photos`, which holds `img-0001.raw` and
`notes.txt`:

```sh
rem archive build --inputs photos --out photos.rem
```

The command prints the build report as one JSON line. Indented, with the `ingest` report
and the second file omitted:

```text
{
    "body_format": "rem-object-v1",
    "caller_object_id": "c9347590-9046-4c83-afca-17d7132b7a9d",
    "chunk_size": 262144,
    "encryption": "none",
    "files": [
        {
            "chunk_count": 2,
            "data_offset": 262144,
            "entry_type": "regular",
            "file_id": "31fd42022d27d9926a35bc0bbd80fd570c93f5f81ed60ea77f624075d43e46d8",
            "file_sha256": "864e10a02a942968fbb957fa446c3a181494118eec4dd4b3330495013db81080",
            "first_chunk_lba": 1,
            "path": "img-0001.raw",
            "size_bytes": 300000,
            "source_path": "photos/img-0001.raw",
            ...
        },
        ...
    ],
    "format_version": null,
    "manifest_sha256": "0a6d6f814f4eeda5b2e709f7bc254a8743afe5a9e797ecbaeabb331fd6c0c0ea",
    "object_id": "c9347590-9046-4c83-afca-17d7132b7a9d",
    "out": "photos.rem",
    "plaintext_digest": "5b7cd98ed089c5b8fe0f8e99b0b53b237231bb10a9f15fdc004a6ecfccd59a9e",
    "recipient_epochs": null,
    "representation": "plaintext",
    "stored_digest": "5b7cd98ed089c5b8fe0f8e99b0b53b237231bb10a9f15fdc004a6ecfccd59a9e",
    "stored_size_blocks": 5,
    "stored_size_bytes": 1310720,
    ...
}
```

For a plaintext object the stored digest equals the plaintext digest. To build the same
files encrypted for two recipients:

```sh
rem archive build --inputs photos --out photos-enc.rem --recipient alice.remr --recipient bob-slot1.remr
```

The report then has `"representation": "encrypted"`, `"encryption": "REMO"`,
`"format_version": 2`, a `stored_digest` that differs from the `plaintext_digest`, and:

```text
"recipient_epochs": [
    {"epoch_id": "5f6de43b3cc18062f4b48c6c0c6df034", "label": "alice"},
    {"epoch_id": "6cd0e9fff259001aea50119d737f9f81", "label": "bob"}
]
```

### rem archive inspect

Inspect an object file without a key.

```text
rem archive inspect [OPTIONS] --object <PATH>
```

- `--object <PATH>` is the object file.
- `--chunk-size <BYTES>` is the block and chunk size of a plaintext object. Default
  `256KiB`.

For a plaintext object the command lists the members, as in the build report, with
`schema_version`. For an encrypted object it parses and validates the header and key frame
and reports `"keyed": false`, `format_version`, `recipient_epochs`, the salt, the prefix
geometry, the stored digest and size, and the plaintext size and chunk count. It cannot show
the encrypted manifest's members. Trimmed:

```sh
rem archive inspect --object photos-enc.rem
```

```text
{
    "chunk_size": 262144,
    "encryption": "REMO",
    "format_version": 2,
    "keyed": false,
    "object_id": "38e1d59d-7443-4d8c-8f18-c2f55388a690",
    "plaintext_chunk_count": 5,
    "plaintext_size_bytes": 1310720,
    "recipient_epochs": [...],
    "representation": "encrypted",
    "stored_digest": "ca68958ddaa24e431fd7747ddeb4ae1ea5a7bc7fa3e1534435537578745bab53",
    "stored_size_bytes": 1572864,
    ...
}
```

### rem archive extract, rem restore

Extract an object file into a directory.

```text
rem archive extract [OPTIONS] --object <PATH> --dest <DIR>
rem restore [OPTIONS] --object <PATH> --dest <DIR>
```

`rem restore` is a top-level alias with the same options and the same key rules.

- `--object <PATH>` is the object file; `--dest <DIR>` is the destination.
- `--private-key <PATH>` is required for an encrypted object and rejected for a plaintext
  one. The key's epoch id selects its key-frame slot.
- `--path <REM_OBJECT_PATH>` with `--range <START:LEN>` extracts a byte range of one member.
  `--path` without `--range` is rejected with
  `--path, --first-chunk-lba, and --file-size-bytes require --range`; to restore one whole
  member by path, give a range that covers it, such as `--range 0:<size>`.
- `--first-chunk-lba <LBA>` and `--file-size-bytes <BYTES>` optionally give the member's
  first object-local block and full size for a range read, from the build report or the
  catalog row.
- `--blob-entry <REM_OBJECT_PATH>` with `--blob-member <TAR_PATH>` restores one member from
  a blob wrapper (REM-OBJECT Core Format Appendix E describes the wrapper and the range read
  this performs). `--blob-member` cannot be combined with `--path` or `--range`.
- `--overwrite` replaces existing files.
- Restore applies only manifest `user.*` attributes. Other names are skipped and
  reported; no option widens this policy.
- `--no-unwrap` keeps wrapper entries literal.
- `--chunk-size <BYTES>` is the block and chunk size of a plaintext object. Default
  `256KiB`.

To extract the encrypted object with Bob's key:

```sh
rem archive extract --object photos-enc.rem --dest restored-enc --private-key bob.remp
```

The command prints a JSON report. Trimmed:

```text
{
    "bytes_written": 300011,
    "encryption": "REMO",
    "files_written": 2,
    "recipient_epochs": [...],
    "representation": "encrypted",
    "unwrap": {"enabled": true, "literal_entries_removed": 0, "tar_engine": null, "wrappers_unwrapped": 0},
    "warnings": [],
    ...
}
```

Without `--private-key` the same command exits 1 with
`error: encrypted REM-OBJECT operation requires --private-key`. A ranged extract,
`--path notes.txt --range 0:5` on the plaintext object, writes those 5 bytes to `notes.txt`
under `--dest` and reports `"mode": "range"`, `"range_start": 0` and `"range_len": 5`.

Every extract report carries `warnings`, an array of warning names. A plaintext object that
reaches tar EOF without its `_remanence/manifest.cbor` entry is nonconformant (REM-OBJECT
§4.9). The reader does not fail a restore-mode read for that reason alone: it returns the
member entries it recovered with the warning `MissingManifest`. A whole-object extract and a
plaintext `--path`/`--range` extract read the object in restore mode, so their array holds
`MissingManifest` for an object without its manifest. An encrypted `--range` extract and a
`--blob-entry`/`--blob-member` extract read only the members they need and never the
manifest, so their array is always empty.

### rem archive extract-stream

Decrypt an encrypted object from standard input to standard output.

```text
rem archive extract-stream [OPTIONS]
```

- `--private-key <PATH>` is the REMP file.
- `--range <START:LEN>`, `--authenticated-prefix <PATH>` and `--stored-range-start <BYTE>`,
  given together, select ranged mode: the command re-authenticates the header, key frame and
  metadata prefix from the file, and decrypts only the covering ciphertext frames, which
  standard input supplies starting at `--stored-range-start`.

Each chunk is authenticated before its plaintext is written, and memory use is bounded. The
plaintext tar goes to standard output and a JSON report to standard error:

```sh
rem archive extract-stream --private-key alice.remp < photos-enc.rem > photos.tar
```

```text
{"bytes_written":1310720,"chunk_size":262144,"command":"archive extract-stream","format_version":2,"object_id":"38e1d59d-7443-4d8c-8f18-c2f55388a690","observed_endpoints":[],"plaintext_sha256":"b0a9d1e82846aa901eec711a4ad99f92c19fa64da75ce6853dec26b760684c36","plaintext_size_bytes":1310720,"range":null,"recipient_epochs":[...],"status":"ok","stored_size_bytes":1572864,"supported_endpoint_kinds":["library","standalone"]}
```

`bsdtar -tf photos.tar` then lists `img-0001.raw`, `notes.txt` and
`_remanence/manifest.cbor`. The full protocol is in
[the extract-stream protocol](reference-extract-stream-protocol.md).

### rem archive covering-range

Print the stored ciphertext range that covers a plaintext member range.

```text
rem archive covering-range [OPTIONS] --object-id <ID> --file-id <ID> --range <START:LEN>
```

- `--private-key <PATH>` is the REMP file used to authenticate the envelope prefix.
- `--object-id <ID>` is the object id, echoed in the response.
- `--file-id <ID>` is the caller's member identifier, echoed in the response.
- `--range <START:LEN>` is the plaintext range.

The command reads the envelope's header, key frame and metadata prefix from standard input,
authenticates it, and prints the smallest stored-ciphertext range to fetch. To find the bytes
that cover the first 5 bytes of `notes.txt`:

```sh
rem archive covering-range --private-key alice.remp \
    --object-id 38e1d59d-7443-4d8c-8f18-c2f55388a690 \
    --file-id 8d090da96510023bcd54dde2fd9b78dabb78eb4943158f06979be075197a2bcc \
    --range 0:5 < photos-enc.rem
```

```text
{"authenticated_prefix_len":2581,"chunk_count":1,"command":"archive covering-range","envelope_object_id":"38e1d59d-7443-4d8c-8f18-c2f55388a690","file_id":"8d090da96510023bcd54dde2fd9b78dabb78eb4943158f06979be075197a2bcc","first_chunk":0,"object_id":"38e1d59d-7443-4d8c-8f18-c2f55388a690","observed_endpoints":[],"plaintext_len":5,"plaintext_start":0,"status":"ok","stored_range_end":264741,"stored_range_len":262160,"stored_range_start":2581,"supported_endpoint_kinds":["library","standalone"]}
```

See [the extract-stream protocol](reference-extract-stream-protocol.md).

### rem archive reseal

Re-seal an encrypted object to a new set of recipients.

```text
rem archive reseal [OPTIONS] --object <PATH> --private-key <PATH> --out <PATH>
```

- `--object <PATH>` is the existing encrypted object.
- `--private-key <PATH>` is a REMP file that opens it.
- `--recipient <PATH> <PATH>...` is the new list of 2 to 8 distinct REMR files, in ascending
  slot order, given as one list after a single `--recipient` (unlike `build`, where the flag
  repeats).
- `--out <PATH>` is the new object; it must not exist.
- `--staging-dir <DIR>` holds the temporary plaintext. Default: next to `--out`.

The command opens the object, fully re-seals the same authenticated plaintext, verifies the
new stored digest, and publishes atomically without replacing anything. To rotate Bob out
and Carol in:

```sh
rem archive reseal --object photos-enc.rem --private-key bob.remp \
    --recipient alice.remr carol-slot1.remr --out photos-enc2.rem
```

Trimmed:

```text
{
    "input_format_version": 2,
    "output_format_version": 2,
    "plaintext_digest": "b0a9d1e82846aa901eec711a4ad99f92c19fa64da75ce6853dec26b760684c36",
    "published_sha256": "c854563252dcee8a55aae46f0219369d21a5bd3c9eedf7eeae5cacf87bc38cdb",
    "recipient_epochs": [
        {"epoch_id": "5f6de43b3cc18062f4b48c6c0c6df034", "label": "alice"},
        {"epoch_id": "b0d7979cf1c6695bf5456096b1233860", "label": "carol"}
    ],
    "verified_after_write": true,
    ...
}
```

### rem archive list

List native objects from the local catalog.

```text
rem archive list [OPTIONS]
```

- `--config <PATH>` is the configuration. Default `/etc/rem/config.toml`.

The command reads the catalog only and accesses no tape. An empty catalog prints nothing.

### rem archive probe, scan, restore, recover

Run a foreign-format adapter against an archive dump.

```text
rem archive probe [OPTIONS] --format <ID>
rem archive scan [OPTIONS] --format <ID>
rem archive restore [OPTIONS] --format <ID> --dest <DIR>
rem archive recover [OPTIONS] --format <ID> --dest <DIR>
```

- `--format <ID>` names the adapter.
- `--dump <PATH>` is the dump file; `rem` requires it.
- `--dest <DIR>` (on `restore` and `recover`) is the destination; `restore` also takes
  `--overwrite`. Restore applies only `user.*` attributes.

`probe` identifies a dump without streaming it, `scan` catalogs its normalized entries,
`restore` restores it into a directory, and `recover` recovers a damaged dump into sparse
partial files. The stock binaries register no foreign formats and fail closed:

```sh
rem archive probe --format tar --dump photos.rem
```

```text
error: unsupported format operation: foreign format tar is not registered in this distribution
```

An adapter distribution supplies the accepted ids and the parser code; see the
[foreign-format adapter reference](reference-foreign-format-adapters.md).

### Blob wrappers

When `rem archive build` wraps a subtree (REM-OBJECT Appendix E), it creates the
`.remwrap.tar` member by running the system tar program with `-c` and
`--format pax --no-xattrs --no-acls --no-fflags --no-mac-metadata`, and records
the program, its version and the exact create and
extract argument lists in the index's `tar_engine` field. That program is `bsdtar`, and only
wrappers need it. `rem archive build`, with or without `--scan-only`, looks for it once the
plan holds a wrapper. A whole-object `rem archive extract` looks for it before writing
anything when the object holds a `.remwrap.tar`, so a host without `bsdtar` fails the command
before the destination changes. It then unwraps exactly the object's own wrappers and leaves
any other `.remwrap.tar` already under `--dest` as it is. `--no-unwrap`, `--path`/`--range`
and `--blob-entry`/`--blob-member` never run `bsdtar`.

The `tar_engine` field of the build report's `ingest`, of the scan report, of the manifest
that `--manifest-out` writes, and of the extract report's `unwrap` is `null` when no wrapper
was involved: always for a `--map` build, which never creates one, and always under
`--no-unwrap`. An index always carries its `tar_engine`, since it exists only beside a
wrapper. `rem archive extract` with `--blob-entry` and `--blob-member` recovers one inner file
by the procedure of Appendix E.4, for both representations. The reference tools offer no
byte-range restore inside an inner file.

### Recipient key-file formats

REMP and REMR are complete binary records, not raw key bytes. All offsets are from the start
of the file, labels are 0 to 32 printable ASCII bytes (`0x20` to `0x7e`), and parsers reject
trailing bytes.

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

The exact REMR length is `1238 + L` bytes. The slot, epoch id and label are copied into
envelope recipient slots; the public-key encoding is validated when the record is parsed.

### Encryption flag matrix

| Operation | Seal or open key input | Result |
|---|---|---|
| `archive build`, `rem-debug archive write` | no recipient flags | Plaintext `rem-object-v1` body. |
| `archive build`, `rem-debug archive write` | `--recipient <REMR>` repeated 2 to 8 times | REM-ENCRYPT 1.0 with on-tape `format_version = 2`. Epoch ids must be distinct and nonzero, and slot order ascending. |
| `archive reseal` | one matching `--private-key <REMP>` plus one `--recipient` list of 2 to 8 new REMR files | Full re-seal for recipient rotation. |
| `archive inspect`, `rem-debug archive export-object` | none | Keyless envelope inspection, or opaque export of the stored bytes. |
| `archive extract`, `restore`, `rem-debug archive read`, `rem-debug archive verify` | `--private-key <REMP>` for encrypted input | The private key's epoch id selects its matching key-frame slot. |
| `archive extract-stream`, `archive covering-range` | `--private-key <REMP>` | Encrypted streaming or bounded ranged open. |

The flags `--encrypt`, `--key-file`, `--key-id` and `--registry-key` do not exist; the
argument parser rejects them.

### Encrypted report schemas

`recipient_epochs` is always an ordered JSON array of objects shaped as
`{"epoch_id":"<32 lowercase hex>","label":"<epoch label or null>"}`. Fresh encrypted writes
and reads take the label from the authenticated key frame. A host-only idempotent replay
uses the committed catalog epoch ids and reports `label: null`, because the catalog does not
keep the on-tape labels; it never substitutes labels from the new retry request.

- An encrypted `archive build` report has `representation: "encrypted"`,
  `encryption: "REMO"`, `format_version: 2`, `recipient_epochs`, stored and plaintext
  digests and sizes, the manifest digest, and the file layout. The plaintext form has
  `format_version: null` and `recipient_epochs: null`.
- Encrypted `archive inspect` is keyless (`keyed: false`) and reports the object id, format
  version, recipients, salt, prefix geometry, stored digest and size, and plaintext size and
  chunk count. It does not claim to show the encrypted manifest.
- `archive reseal` reports `input_format_version: 2`, `output_format_version: 2`, the new
  `recipient_epochs`, plaintext and published stored digests, sizes, and
  `verified_after_write: true`.
- Encrypted local-extract reports and tape-write locators carry `recipient_epochs`. Direct
  tape read and verify receipts keep their payload byte and hash fields; recipient
  provenance stays in the locator and the catalog. Whole-object `extract-stream` also
  reports `format_version: 2`; ranged mode reports the recipients parsed from the
  authenticated prefix and the authenticated-chunk and stored-range geometry.

<!-- code-anchor: crates/remanence-cli/src/rem_debug.rs crates/remanence-cli/src/lib.rs @ 625f7044 -->
## rem-debug extras

`rem-debug` has only part of the surface above. It shares discovery (`libraries`,
`library`, `watch`), `rebuild-catalog-from-journals`, the local tape-lifecycle commands
(`tape init`, `tape wait-ready`, `tape quarantine list`, `show` and `release`, `tape retire`)
and the local `archive` commands (`capabilities`, `formats`, `recipient`, `build`, `reseal`,
`inspect`, `extract`, `extract-stream`, `covering-range`, `list`, and the foreign-format
`probe`, `scan`, `restore` and `recover`). It has none of the standalone-drive commands, no
top-level `restore`, and none of the daemon client commands (`daemon`, `op`, the `catalog`
queries, `audit`, `drive`, `alarms`, `top`, `put`, `get`, `tape finalize`,
`tape supersede-tail`, `tape inventory`, `tape verify-index`). It refuses the daemon client
commands with an error saying they are available through `rem`. Its top-level help also
lists `tape-supersede-client`; that is the daemon client for `tape supersede-tail`, and it is
refused in the same way.

Some names mean different things in the two binaries. `rem-debug catalog` is local catalog
maintenance (`reset-preflight`, `reset`), not the daemon catalog queries.
`rem-debug tape alerts` reads the drive directly by `--bay`, while `rem tape alerts <DRIVE>`
is the daemon alias of `rem drive alerts`. `rem archive verify` verifies through the daemon
(see [Restoring from tape](#restoring-from-tape)); `rem-debug archive verify` reads the tape
directly.

In the other direction, `rem` refuses the robotics and the direct tape-path `archive write`,
`archive read` and `archive export-object` with an error saying they are available through
`rem-debug`. It has no `tape bot-probe`, `tape read-rate`, `tape adopt-bootstrap`,
`tape recovery-report`, `catalog reset` or `catalog reset-preflight`.

### rem-debug tape-log inspect

Report the last valid closed boundary of a tape log, and any damage, without changing it.

```text
rem-debug tape-log inspect [OPTIONS] --config <CONFIG> --tape-uuid <TAPE_UUID>
```

Its JSON fields are `tape_uuid`, `tape_incarnation`, `through_sequence`, `valid_bytes`,
`original_bytes` and `damage`. The UUID is hyphenated, the incarnation is the number 1,
sequence and byte counts are decimal strings, and `damage` is a string or `null`.

### rem-debug tape-log repair-tail

Archive a damaged suffix of a tape log and keep an exact validated closed sequence.

```text
rem-debug tape-log repair-tail [OPTIONS] --config <CONFIG> --tape-uuid <TAPE_UUID> --through-sequence <THROUGH_SEQUENCE>
```

- `--through-sequence <THROUGH_SEQUENCE>` is the last validated closed sequence to keep.
- `--confirm-discard-corrupt-tail` is required before the state is opened.
- `--rebuild-projection` rebuilds this tape's catalog rows in the same offline operation; it
  is required when the cut falls below the catalog's projection mark.

The JSON reports `tape_uuid`, `tape_incarnation`, `through_sequence`, `archive`,
`archive_sha256`, `reconciliation_record`, `original_bytes`, `retained_bytes`,
`projection_rebuilt` and `physical_tail_reconciliation_required`. Sequence and byte counts
are decimal strings, the digest is lowercase hexadecimal, and the last field is `true`. The
durable repair fence remains until a confirmed `rem tape supersede-tail` reconciles the
physical media. See the
[repair procedure](guide-troubleshooting.md#damaged-log-tails-and-projection-rebuild).

### Robotics

These commands send SCSI directly to the changer, bypassing the daemon, and all require
`--allow <SERIAL>`.

| Command | What it does |
|---|---|
| `rem-debug move --src <SRC> --dst <DST> <SERIAL>` | MOVE MEDIUM between two element addresses (slot, IE port or drive bay). |
| `rem-debug load --slot <SLOT> --bay <BAY> <SERIAL>` | Changer MOVE MEDIUM, then drive SSC LOAD. |
| `rem-debug unload --bay <BAY> [--dest <DEST>] [--config <PATH>] <SERIAL>` | SSC UNLOAD, then MOVE MEDIUM back to the bay's recorded source slot or to `--dest`, with locked recovery for a stranded medium-removal prevent (below). |
| `rem-debug export --slot <SLOT> <SERIAL>` | Move a cartridge from a slot to the first free IE (import/export) port. |
| `rem-debug import --slot <SLOT> <SERIAL>` | Move a cartridge from the first occupied IE port to a slot. |
| `rem-debug rescan <SERIAL>` | INITIALIZE ELEMENT STATUS, then reconcile the snapshot. A shape mismatch is a hard error. |
| `rem-debug lock <SERIAL>`, `rem-debug unlock <SERIAL>` | PREVENT or ALLOW MEDIUM REMOVAL on the changer. The changer holds the lock, so it survives process exit; while it is held, the front-panel eject and operator mailslot eject are refused. A device reset or power cycle also releases it. |

To move the cartridge in slot `0x0400` of library `LIB001` into bay `0x0100` and load it:

```sh
rem-debug --allow LIB001 load --slot 0x0400 --bay 0x0100 LIB001
```

`unload` first tries SSC UNLOAD without taking new locks or reading configuration. Only
ILLEGAL REQUEST with sense `05/53/02` (MEDIUM REMOVAL PREVENTED) triggers recovery: take the
drive's device lock `/run/lock/rem/<drive-serial>.lock` without waiting, issue ALLOW MEDIUM
REMOVAL (audited as `AllowRemoval`), retry UNLOAD, then MOVE. The lock stays held through the
MOVE. A busy lock refuses recovery without ALLOW; a failed ALLOW stops before the retry or
MOVE. Other UNLOAD errors do not trigger recovery.

Without `--config`, the device lock is the recovery exclusion, and no configuration file,
including `/etc/rem/config.toml`, is read. With an explicit `--config <PATH>`, recovery also
takes that configuration's state-directory lock without waiting and records the recovery in
its audit log. It takes only the bare state lock: it does not open or migrate the catalog or
reconcile reset fences, so a catalog-reset fence does not block cartridge recovery. Normal
unloads do not take this state lock even when `--config` is given. `rem-debug unlock`
addresses the changer, so it does not clear a drive's PREVENT.

### Direct tape data path

These commands bypass the daemon. They use the configuration for the catalog and the tape
logs, and take `--config <PATH>` (default `/etc/rem/config.toml`).

| Command | What it does |
|---|---|
| `rem-debug archive write --library <SERIAL> --file <PATH> --pool <POOL> [--archive-path <PATH>] [--caller-object-id <ID>] [--recipient <REMR>]... [--json]` | Write one local file to a pool-selected tape through the in-process drive actor on the placed library drive, and print its locator. `--archive-path` overrides the in-archive path (default: the file's basename); `--caller-object-id` records an opaque caller object id (default: a fresh UUID). It applies the same positive `NotWorm` admission as daemon-owned object writes before MODE SELECT; write-protected, WORM and unknown media fail closed. Repeating `--recipient` 2 to 8 times writes the encrypted envelope. The locator includes `format_version` and `recipient_epochs`. |
| `rem-debug archive read --library <SERIAL> --locator <JSON> --out <PATH> [--private-key <REMP>]` | Read an object by locator and write its payload to `--out`; an encrypted copy needs the matching private key. |
| `rem-debug archive export-object --library <SERIAL> --locator <JSON> --out <PATH>` | Export the complete stored object bytes, envelope included, by locator. |
| `rem-debug archive verify --library <SERIAL> --locator <JSON> --expected-sha256 <HEX> [--private-key <REMP>]` | Stream and hash an object on tape against an expected digest, restoring nothing; encrypted copies need the matching private key. |
| `rem-debug archive probe/scan/restore/recover --format <ID> --tape <SERIAL> --bay <BAY> [--rewind]` | Run a registered foreign-format adapter directly against a loaded tape instead of a dump file. `--tape` takes the serial of the library that holds the tape and `--bay` the drive bay; that library must be on `--allow`. `restore` and `recover` also take `--dest <DIR>`. |
| `rem-debug tape alerts --bay <BAY> [--library <SERIAL>]` | Read the loaded drive's TapeAlert LOG SENSE page directly. `--library` selects a configured library when there is more than one. `rem tape alerts` has no `--bay`; it is the daemon alias that takes a drive serial or UUID. |

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-state/src/state.rs @ 625f7044 -->
## Destructive state reset

### rem-debug catalog reset-preflight

```text
rem-debug catalog reset-preflight [OPTIONS]
```

- `--config <PATH>` is the configuration. Default `/etc/rem/config.toml`.
- `--preserve-tape-voltag <VOLTAG>` keeps this exact tape identity and geometry across the
  reset; repeat it for several.
- `--allow-erase-tape-voltag <VOLTAG>` permits this exact bound tape to be erased instead;
  repeat it for several.

Read-only admission for a scoped catalog reset. It enumerates every data and cleaning tape,
rejects any bound label outside the two exact selector sets, and returns a source-bound token
plus `source_schema_version`. A predecessor catalog of schema 16 through 21 with the
required tape identity and geometry columns is inspectable only when the preservation set is
empty and every bound tape is explicitly allowed to be erased; such a catalog is never
migrated and never accepted for preservation. Schema 22 has a separate declared-reset reader
that permits identity preservation without retaining old commit authority.

### rem-debug catalog reset

```text
rem-debug catalog reset [OPTIONS]
```

- `--i-understand-this-erases-the-catalog` is the required confirmation. Without it the
  command exits 2 and names what the reset erases: the catalog, audit history, drive
  history, drive health snapshots, cleaning runs and alarms.
- `--expected-preflight-token <SHA256>` is the token from `reset-preflight`, required for a
  scoped reset.
- `--preserve-tape-voltag` and `--allow-erase-tape-voltag` are as for `reset-preflight`.
  Written or finalized tapes that are preserved return as `recovery_required`; no object,
  tape-file, audit, tape-log or physical-prefix authority is kept.

Destructively reset local catalog state from the configured paths. Any scoped reset requires
the exact current preflight token, and repeats selector and source-schema admission under
the exclusive state lock before it changes anything. The reset moves the audit log and the
tape logs into a numbered directory under `reset-archives` in the state directory.

### rem reset-catalog

`rem reset-catalog --config <PATH> --i-understand-this-erases-the-catalog`
declares the offline reset required by an authority-layout change. It uses the
same implementation and accepts the same preservation, erase and preflight
arguments as `rem-debug catalog reset`. Stop the daemon first. Old state is
refused rather than imported; reset archives the prior authority and installs
the per-tape log layout. Configuration and calibration generation allocation
survive. Reinitialize approved scratch tapes afterwards, with fresh UUIDs;
see the [reset procedure](guide-troubleshooting.md#authority-layout-refusals-and-declared-reset).

`tape init` is not gated by `--allow`: it operates only on the libraries in the
configuration. `tape retire` and `catalog reset` change local state and carry their own
confirmations. `tape quarantine release` uses the daemon for tape-I/O fences when it runs;
offline release needs exclusive state ownership.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-state/src/state.rs crates/remanence-state/src/tape_log/projection.rs @ 625f7044 -->
## Catalog rebuild

### rem rebuild-catalog-from-journals

Rebuild the SQLite catalog from the audit log and the tape logs.

```text
rem rebuild-catalog-from-journals [OPTIONS]
```

- `--config <PATH>` is the configuration. Default `/etc/rem/config.toml`.

The name keeps an older word; the command reads the per-tape logs and the audit log. Stop the
daemon first: the rebuild takes the state lock. It requires an existing `authority-format`
marker and never installs one. It refuses when a tape's log ends below the sequence the
catalog has already projected. It keeps the log and audit bytes, leaves ambiguity and repair
fences in place, and does not reconcile startup authority or touch media. It evicts wrap maps
and marks volumes uncalibrated while advancing calibration generation allocation. Operator
decisions recorded in the audit log, such as retirements and label rebinds, are applied after
the tape logs (see
[Tape identity and the retire operation](tape-identity-lifecycle-explainer.md#5-retire-and-catalog-rebuild)).

To rebuild the catalog of the default configuration:

```sh
rem rebuild-catalog-from-journals
```

The command prints a summary (values illustrative):

```text
rebuild-catalog-from-journals ok
  tapes: 3
  tape files: 41
  object copies: 38
  audit records: 112
  tape logs: 3
```

An audit record that disagrees with the rows the tape logs produced, or that the rebuild
cannot apply (for example one with no valid tape identity), is reported on standard error as
`WARN audit-replay-divergence: <DETAIL>; ledger value applied`.

<!-- code-anchor: crates/remanence-cli/src/recovery_report.rs crates/remanence-cli/src/tape_inventory.rs crates/remanence-parity/src/scan.rs crates/remanence-parity/src/bootstrap.rs crates/remanence-parity/src/verify_protected.rs @ 625f7044 -->
## Output formats

### Recovery report and verification output

`rem-debug tape recovery-report` treats the bootstrap as described below, following
REM-PARITY Section 8.4.

A bootstrap that cannot be read is treated as unreadable, and the report then uses the three
supplied values. The cases are a medium error, a filemark or EOD where the record should be,
a missing magic, a failed header CRC, a failed payload CRC, a payload length that runs past
the block, and a payload that breaks a later rule of Section 8 when no value that can still
be decoded disagrees with the supplied ones. Nothing is printed for these cases beyond the
report's own note that the bootstrap was treated as unreadable. Without the three values the
command exits with status 1 after printing:

```text
error: catalog-less recovery report: discover bootstrap: bootstrap not found anywhere on tape; unreadable bootstrap requires --tape-uuid, --block-size and --scheme
```

The specification calls this outcome `NoBootstrapFound`. The same line is printed when the
first record does not parse as a bootstrap and no values were supplied.

A bootstrap that reads is refused when its first record has the wrong length for the
supplied block size, when its header CRC is valid and a header field is impossible or
contradicts the supplied values (even if its payload is damaged), or when a scheme record
that can still be decoded disagrees. The refusal ends discovery, no report is produced, and
the command exits with status 1 after printing:

```text
error: catalog-less recovery report: discover bootstrap: bootstrap refused: DETAIL
```

DETAIL names what disagreed. The specification calls every such refusal `BootstrapParse`.
The texts are:

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

A no-parity bootstrap whose payload carries a scheme record is refused in one of these two
ways. If you supplied a parity scheme, DETAIL is
`readable bootstrap parity scheme differs from supplied hints: no-parity flag contradicts scheme`.
If you supplied `--scheme none` and the record can be decoded, DETAIL is
`readable bootstrap parity scheme differs from supplied hints`. If the record cannot be
decoded, the bootstrap is treated as unreadable.

A decodable `drive_compression` of true in the bootstrap of a parity tape is refused with the
following line, which the specification calls `DriveCompressionEnabled`:

```text
error: catalog-less recovery report: discover bootstrap: tape's bootstrap records drive compression; a parity tape must not record drive compression
```

When terminal discovery succeeds, the report does not list the walk's prefix damage:
`scan.damaged_regions` is empty. A full verification (`rem tape verify-index`) reports that
damage as `PREFIX_DAMAGE` findings.

`rem tape verify-index` prints one `protected_content_finding: KIND [address] detail` line
for each finding, after `verification_basis` and before `measured_eod_lba`, in the verified
forms only. KIND is one of `PROTECTED_CONTENT_FINDING_KIND_DATA_BLOCK`, `_PARITY_SHARD`,
`_SIDECAR`, `_PREFIX_DAMAGE`, `_NOT_PERFORMED` and `_PARITY_MAP`, each printed in full with
the `PROTECTED_CONTENT_FINDING_KIND` prefix. The address is empty for `PREFIX_DAMAGE` and
`NOT_PERFORMED`. With `--json`, the same findings are in `protected_content_findings`, an
array of `{kind, address, detail}` objects with `address` `null` where there is none, in both
the verified and the recovery-required envelope. The array is `[]` when the protected content
verified clean.

Nonzero trailing fill after the bootstrap payload is reported as a `PREFIX_DAMAGE` finding. A
bootstrap that was treated as unreadable is not reported for fill, because its bytes were not
read as a bootstrap; a bootstrap that validates, including under supplied or catalog values,
is checked for fill.

### The verification I/O report

The verification JSON also carries `io`, a diagnostic object under the same
`rem.tape.verify-index.v1` schema. It is present for complete, degraded and
recovery-required results when the daemon collects it, and `null` when the daemon supplies
none. Human output has one `io:` summary line and one `io phase` line per phase that issues a
command, with elapsed seconds, seconds inside commands, the command count and the slowest
command's seconds. Counts, milliseconds and physical addresses in JSON are decimal strings.
These measurements do not change the outcome or the findings.

`io.total` and each entry of `io.phases[].counts` contain attempted `locate`, `space`,
`read_position`, `single_read`, `batch_read` and `other` commands; `records_requested` and
`records_confirmed`; transport-reported `bytes`; `hardware_batch_fallbacks`;
`algorithmic_fallback_epochs`; and `recovered_reads`. The same objects carry these timing
fields:

| Field | Meaning |
|---|---|
| `elapsed_ms` | Wall milliseconds while the phase is current, excluding nested phases; `total` covers the whole collection. |
| `command_ms` | Wall milliseconds inside transport dispatches, including unsuccessful attempts. |
| `max_command_ms` | Milliseconds in the slowest command. |
| `command_latency_buckets` | Six command counts for under 5, 5 to 20, 20 to 100, 100 to 1000, 1000 to 10000, and 10000 ms or more, in that order. |

Time inside commands (`command_ms`) against time between commands
(`elapsed_ms - command_ms`) is the split operators use to tell drive motion and transfer
waits from host work on the drive actor's thread. These wall times include transport
overhead and scheduling waits. The daemon logs `remanence_verify_diag` INFO events per phase
with commands, and for `total`.

Inline positioning commands and retries count. Fixed-mode reads of one record count as
batches. Unclean batches contribute requested records and reported bytes, but only their
successful single-record retries confirm records. `other` includes setup and readiness
commands. Bytes include configuration responses and unconfirmed transfers; errors without a
byte count contribute no known bytes. A hardware fallback counts an unclean batch selected
for single retry, even if repositioning fails. An algorithmic fallback counts an epoch that
needed unbuffered data reads, independently of hardware retries.

The phases are `setup`, `walk_terminal`, `parity_map`, `sidecar`, `protected_data`,
`fallback_data` and `bot_recovery`. They attribute each command once, to the innermost
active boundary. `elapsed_ms` measures daemon elapsed time from readiness through completion
with a monotonic clock; it includes computation and excludes earlier drive queueing and
client overhead. `backward_data_seeks` counts protected-sweep LOCATE targets before the end
position of the previous confirmed protected read. `protected_bytes` and `protected_ranges`
describe confirmed forward-sweep coverage, excluding fallback reads. Ranges use inclusive
`start_lba` and exclusive `end_lba`; overlaps and duplicates are kept. `recovered_lbas` lists
accepted self-corrected read addresses; `recovered_unknown_addresses` counts events without a
known address. Together they account for `total.recovered_reads`. See
[The I/O report](guide-full-verification.md#the-io-report) for the phase boundaries and how to
read coverage.

An exact self-corrected single read is accepted during verification when it has no
incorrect-length, filemark, end-of-medium or residual anomaly. It is counted and does not
itself fail the tape. Its bytes still pass the normal checksum checks, so a mismatch remains
a finding. No extra drive commands are issued for measurement. An RPC error produces no
verification report.

<!-- code-anchor: crates/remanence-daemon/src/entry.rs @ 625f7044 -->
## rem-daemon

Run the library service.

```text
rem-daemon [--config <PATH>] [--socket <PATH>]
```

- `--config <PATH>` is the daemon configuration. Default `/etc/rem/config.toml`.
- `--socket <PATH>` overrides the listen socket. Default: the configuration's
  `[daemon] socket_path`, otherwise `<state_dir>/rem.sock`.

The daemon serves the gRPC API on the Unix socket, and on a TCP mTLS listener when one is
configured. SIGINT, SIGTERM and SIGHUP start a graceful shutdown. It exits 1 on any startup
or serve failure. The [configuration reference](reference-configuration.md) describes its
settings and [the API reference](reference-api.md) its services.

<!-- code-anchor: crates/rem-recover/src/main.rs @ 625f7044 -->
## rem-recover: standalone recovery

Recover the plaintext members of one encrypted object file, with nothing else.

```text
rem-recover --object <PATH> --private-key <PATH> --out <DIR>
            [--staging-dir <DIR>] [--overwrite]
```

- `--object <PATH>` is the complete stored object.
- `--private-key <PATH>` is a REMP file for one of its recipients.
- `--out <DIR>` is the destination directory.
- `--staging-dir <DIR>` holds the temporary plaintext. Default: inside `--out` when that
  directory already exists, otherwise next to it; never `/tmp`.
- `--overwrite` replaces existing destination members.
- Restore applies only manifest `user.*` attributes. Other names are skipped and
  reported; no option widens this policy.

`rem-recover` is a separate crate and binary (`crates/rem-recover`), for the case where the
daemon, the catalog and the configuration are unavailable or not trusted. It parses the
envelope header and key frame directly, selects the slot that matches the private key's
epoch id, and fails with a message naming the object's recipient labels when none matches.
The staged plaintext is securely zeroed on drop before the restore completes. It validates
the embedded object id against the header before writing files, and refuses to overwrite
existing output unless `--overwrite` is given. Ordinary local restores can use
`rem archive extract --private-key` instead.

To recover the encrypted object built above with Alice's key:

```sh
rem-recover --object photos-enc.rem --private-key alice.remp --out recovered
```

```text
recovered 2 files (300011 bytes) from REM-OBJECT v2 object 38e1d59d-7443-4d8c-8f18-c2f55388a690
skipped_xattrs: {}
applied_privileged_xattrs: {}
```

Given a plaintext object, it exits 4 with
`error: parse object header: invalid REM-OBJECT magic bytes`. Its exit codes are stable, so
unattended recovery tooling can tell failures apart:

| Code | Meaning |
| ---: | --- |
| 0 | Recovery completed. |
| 2 | Command-line usage error (from clap). |
| 3 | Malformed REMP key file, or a recipient epoch not present in the object. |
| 4 | Corrupt or nonconforming envelope or inner REM-OBJECT. Also an authenticated unwrap failure for a matching epoch, which cannot distinguish a damaged recipient slot from a wrong REMP seed. |
| 5 | Host filesystem, staging, input or output I/O failure. |

## Development tools

`rem-dev` holds the review-vector generator and scratch-tape drills. It is not part of the
release archives. It needs the `fixtures` feature:

```sh
cargo build -p remanence-cli --features fixtures --bin rem-dev
```

See [Development tools](development-tools.md) for its commands and arguments. Old
development invocations through `rem` or `rem-debug` fail with a one-line replacement
command.
