# CLI reference

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-cli/src/main.rs crates/remanence-cli/src/rem_debug.rs @ 2a20106 -->
## The binaries

Remanence ships two command-line tools built from `crates/remanence-cli`,
the daemon binary from `crates/remanence-daemon`, and a standalone
disaster-recovery binary from its own crate `crates/rao-recover`:

- **`rem`** is the operator CLI. It talks to a running `rem-daemon` over
  gRPC for daemon-backed commands, and works directly against local state
  or local files for the rest. It cannot issue arbitrary SCSI mutations.
- **`rem-debug`** is the break-glass CLI. It contains everything `rem` has
  plus direct SCSI robotics (`move`, `load`, `unload`, `export`, `import`,
  `rescan`, `lock`, `unlock`), direct tape read/write commands, destructive
  catalog maintenance, and development helpers. Every state-changing
  `rem-debug` invocation must name the target library in a `--allow
  <SERIAL>` allowlist; without it the command refuses to run. This exists
  because a partitioned chassis can expose another system's partition on
  the same host, and a mistyped element address must not be able to touch
  it.
- **`rem-daemon`** is the long-running Layer 5 service. It takes only
  `--config <PATH>` (default `/etc/rem/config.toml`) and `--socket <PATH>`
  (overrides the config's socket path).
- **`rao-recover`** is a small, standalone catalogless disaster-recovery
  tool — see [`rao-recover`: standalone recovery](#rao-recover-standalone-recovery)
  below. It shares no code path with the daemon or the CLI's `--allow`
  gauntlet: it doesn't touch tape, the catalog, or a config file at all,
  only a RAO object file already sitting on disk.

`rem` and `rem-debug` are two separate binaries built from the same
`remanence-cli` crate — not aliases of one binary — sharing one large
`clap` command tree; `rem`'s direct-hardware verbs exist in that tree
only as hidden legacy-compatibility aliases and are documented under
`rem-debug` below, where they're first-class.

Conventions shared across both CLIs:

- Daemon-backed commands take `--endpoint <URI>`, default
  `unix:/var/lib/rem/rem.sock`. TCP endpoints work with the daemon's mTLS
  listener.
- Config-backed commands take `--config <PATH>`, default
  `/etc/rem/config.toml`.
- `--json` switches to stable, machine-readable JSON output intended for
  orchestrators and scripts.
- Durations accept `ms`/`s`/`m`/`h` suffixes; sizes accept byte counts or
  `KiB`/`MiB`/`GiB`-style suffixes (powers of 1024).

Both binaries print full usage with `--help` at every level; this page is a
map, not a substitute for it.

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-library/src/handle/tape_io/readiness.rs @ 2a20106 -->
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

The `--json` output carries the same decoding in structured form
(`recommended_next_command`, `operator_action` fields).

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 2a20106 -->
## Discovery and hot-plug

| Command | What it does |
|---|---|
| `rem libraries [--json]` | Discover every reachable tape library on this host: joins changer inventory (READ ELEMENT STATUS) with drive identities (INQUIRY + VPD 0x80) by serial number. |
| `rem library <SERIAL> [--slots] [--json]` | Focused view of one library: drives, loaded barcodes, and with `--slots` every storage slot. |
| `rem watch [--coalesce-window <DURATION>]` | Stream coalesced udev hot-plug events for SCSI tape/changer subsystems (default window 500ms). Requires a build with the `linux-udev` feature. |

Discovery is read-only but still issues SCSI commands, so it needs the
`tape` group and `CAP_SYS_RAWIO` (see the [quickstart](guide-quickstart.md)
and [troubleshooting](guide-troubleshooting.md)).

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 2a20106 -->
## Daemon queries

All of these speak gRPC to `rem-daemon` and take `--endpoint` and `--json`.

| Command | What it does |
|---|---|
| `rem daemon health` / `rem daemon version` | Liveness and version of the daemon and API. |
| `rem op list` / `rem op get <UUID>` | Enumerate or inspect tracked daemon operations. |
| `rem catalog tapes [--pool <POOL>]` | List cataloged tapes, optionally per pool. |
| `rem catalog tape <TAPE_UUID>` | One tape's catalog record. |
| `rem catalog tape-files <TAPE_UUID>` | Tape files recorded for one tape. |
| `rem catalog pools` / `rem catalog pool <POOL_ID>` | Tape pool definitions and membership. |
| `rem catalog units [--origin all\|native\|foreign]` | Catalog units across native RAO objects and foreign (scanned legacy) archives. |
| `rem catalog unit <UNIT_ID>` / `rem catalog entries <UNIT_ID>` | One unit, or the entries inside it. |
| `rem top [--once] [--json]` | Live TUI over daemon state; `--once` takes a single snapshot. |
| `rem alarms [--all]` / `rem alarms ack <CONDITION_KEY>` | List standing alarms (with `--all`, cleared ones too) or acknowledge one. |

Note: `GetLiveStatus` also carries an advisory `drive_assignments`
projection (per-bay busy/idle state, keyed `(library_serial, bay)`) for
an external arbitration client — `rem top` does not currently render
this field; it's wire-only today.

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 2a20106 -->
## Drive stewardship

Drive-fleet management through the daemon. A drive is addressed by serial
or UUID.

| Command | What it does |
|---|---|
| `rem drive list [--foreign] [--retired]` | List cataloged drives. |
| `rem drive show <DRIVE>` | One drive's stewardship record. |
| `rem drive history <DRIVE> [--events] [--snapshots]` | Lifecycle history, optionally with observational events and health snapshots. |
| `rem drive alerts <DRIVE>` | Active TapeAlert flags. (`rem tape alerts` is a deprecated alias.) |
| `rem drive poll <DRIVE>` / `rem drive clean <DRIVE>` | Poll health now, or run a cleaning cycle now. |
| `rem drive annotate <DRIVE> ...` | Record purchase date, warranty, cost, and notes. |
| `rem drive retire <DRIVE> --reason <TEXT> --i-understand-fleet-removal-is-permanent` | Permanently remove a drive from the managed fleet. |

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 2a20106 -->
## Tape lifecycle

These run against the local config/state (not the daemon) and drive real
hardware; they are gated by what the code calls the destructive-safety
gauntlet — a chain of identity, ownership, and data-presence checks that
each initialization must pass.

| Command | What it does |
|---|---|
| `rem tape init <TARGET> [--dry-run] [--force] [--clobber-data] [--block-size <BYTES>] [--library <SERIAL>]` | Initialize one tape (by barcode or element address) or an inclusive slot range like `0x0400..0x0407`. `--dry-run` runs every check and writes nothing. `--force` overrides only decisions classified as `RequireForce`; `--clobber-data` is the separate, stronger override for tapes that hold data, and is rejected for dry-run and batch init. |
| `rem tape wait-ready [--barcode <BC> \| --drive-element <ADDR>] [--already-loaded] [--wait] [--timeout 2.5h] [--poll 30s] [--resume <UUID>]` | Poll TEST UNIT READY until already-loaded media is usable. LTO-9 first loads can take hours (media optimization); the 2.5h default timeout exists for that. `--resume` continues a durable readiness operation without moving media. |
| `rem tape quarantine list [--library <SERIAL>]` | List active media-readiness fences. |
| `rem tape quarantine show <ID>` | One fence, by quarantine id or operation UUID. |
| `rem tape quarantine release <ID> --ack <TEXT> [--after-settled-inventory]` | Release a fence after operator root-cause acknowledgement. |
| `rem tape retire <TARGET> --reason <TEXT> --i-understand-copies-become-unreadable [--dry-run]` | Permanently retire a tape identity in the local catalog. Every copy on that tape becomes unreadable through the catalog. |

<!-- code-anchor: crates/remanence-cli/src/lib.rs crates/remanence-cli/src/archive_ingest.rs crates/remanence-cli/src/archive_map.rs @ 2a20106 -->
## Archive objects (local, no tape)

`rem archive` builds and reads portable RAO object files on local disk.
None of these touch tape; they are the easiest way to exercise the format.

| Command | What it does |
|---|---|
| `rem archive capabilities` | Print a one-line JSON list of this build's RAO capabilities (e.g. `rao-v2-envelope`, `wrap-suite-hpke-v1`, `ranged-ciphertext-extract`) with zero hardware discovery. Useful for scripts probing what a given binary supports before calling it. |
| `rem archive build --inputs <PATH>... --out <FILE>` | Build a plaintext `rao-v1` object from files/directories and print a JSON build report. `--encrypt --key-file <32-byte-key> --key-id <32-hex>` builds the encrypted `RAO1` v1 (registry-key) representation instead — `build` has no direct path to a v2/HPKE envelope; see `reseal` below for that. `--map`/`--source-root`/`--map-sha256` accept a planner-emitted source map instead of `--inputs`. `--scan-only` classifies inputs and reports without writing. `--rules` applies an ingest ruleset; `--manifest-out` (requires `--rules`) writes the member manifest JSON. |
| `rem archive reseal --object <FILE> --registry-key <KEY> --recipient <PUBKEY>... --out <FILE> [--staging-dir <DIR>]` | Convert an existing plaintext-registry-key (v1) object into a multi-recipient HPKE (v2) envelope. Takes 2-8 recipient public keys in ascending slot order. Verifies the resealed output's digest before an atomic `renameat2(RENAME_NOREPLACE)` publish (never clobbers an existing `--out`); staging happens next to the destination, never in `/tmp`. One-shot v1→v2 migration only — there is no v2→v2 rewrap/key-rotation mode and no v2→v1 downgrade. |
| `rem archive inspect --object <FILE> [--key-file <KEY>]` | Print an object's header, manifest digest, and member table as JSON. Works keylessly on both v1 and v2 headers (a v2 key frame is parsed and validated without needing any key). |
| `rem archive extract --object <FILE> --dest <DIR>` | Extract a whole object. `--path` plus `--range <START:LEN>` extracts a member byte range; `--blob-entry`/`--blob-member` restore a single member from inside a blob wrapper. **v1-only** — there is no `--private-key`/recipient flag here, so a v2 (HPKE) envelope cannot be opened through this command; use `rao-recover` instead. |
| `rem archive extract-stream --key-file <KEY> [--range <START:LEN> --authenticated-prefix <FILE> --stored-range-start <BYTE>]` | Stream-decrypt from stdin to stdout with per-chunk authentication and bounded memory. With the three ranged flags together, performs a bounded ranged-ciphertext decrypt: it re-authenticates the header/metadata prefix from `--authenticated-prefix`, then decrypts only the covering ciphertext frames fed on stdin (which must start exactly at `--stored-range-start`). **v1-only** (root key), same as `extract`. |
| `rem archive covering-range --key-file <KEY> --object-id <ID> --file-id <ID> --range <START:LEN>` | Given a requested plaintext byte range inside one member, authenticate the object's header/metadata prefix (read from stdin) and print the smallest covering stored-ciphertext byte range as JSON (`stored_range_start`/`stored_range_len`/`first_chunk`/`chunk_count`) — the geometry `extract-stream`'s ranged mode needs. **v1-only.** See [`reference-extract-stream-protocol.md`](reference-extract-stream-protocol.md) for the full stream framing and exit codes. |
| `rem restore --object <FILE> --dest <DIR>` | Top-level alias surface for native RAO restore, same flags as `extract`. |
| `rem archive list` | List native objects from the local catalog (no tape access). |
| `rem archive probe --format bru --dump <FILE>` | Identify a legacy archive dump without streaming it. |
| `rem archive scan --format bru --dump <FILE>` | Catalog entries from a legacy dump. |
| `rem archive restore --format bru --dump <FILE> --dest <DIR> [--overwrite]` | Restore a legacy dump into a directory. |
| `rem archive recover --format bru --dump <FILE> --dest <DIR>` | Best-effort recovery of a damaged dump into sparse partial files. |

The only foreign format driver today is `bru` (BRU/BRU-PE legacy
archives). It is not in the default build: the `--format bru` commands
exist only in binaries built with `--features remanence-cli/foreign-bru`,
and CI guards that the default dependency graph stays free of the legacy
reader.

**No `rem`/`rem-debug` command opens a whole v2 (HPKE) envelope end to
end.** The only ways to fully decrypt a v2 object are the standalone
[`rao-recover`](#rao-recover-standalone-recovery) binary, or a
crate-internal call to `remanence-aead::open_envelope` (not exposed by
either CLI).

<!-- code-anchor: crates/remanence-cli/src/rem_debug.rs crates/remanence-cli/src/lib.rs @ 2a20106 -->
## rem-debug extras

Everything above exists in `rem-debug` too. What `rem-debug` adds:

Robotics (direct SCSI against the changer; all require `--allow <SERIAL>`):

| Command | What it does |
|---|---|
| `rem-debug move --src <ADDR> --dst <ADDR> <SERIAL>` | MOVE MEDIUM between two element addresses. |
| `rem-debug load --slot <SLOT> --bay <BAY> <SERIAL>` | Composed changer MOVE MEDIUM + drive SSC LOAD. |
| `rem-debug unload --bay <BAY> [--dest <SLOT>] <SERIAL>` | Composed SSC UNLOAD + MOVE MEDIUM back to the recorded source slot (or `--dest`). |
| `rem-debug export --slot <SLOT> <SERIAL>` / `import --slot <SLOT> <SERIAL>` | Move a cartridge to the first free IE port, or in from the first occupied one. |
| `rem-debug rescan <SERIAL>` | INITIALIZE ELEMENT STATUS, then reconcile the snapshot. Shape mismatch is a hard error. |
| `rem-debug lock <SERIAL>` / `unlock <SERIAL>` | PREVENT/ALLOW MEDIUM REMOVAL on the changer. The lock is held by the changer itself and survives process exit. |

Direct tape data path (bypasses the daemon; uses config for catalog and
journals):

| Command | What it does |
|---|---|
| `rem-debug archive write --library <SERIAL> --file <PATH> --pool <POOL> [--encrypt ...] [--json]` | Write one local file to a pool-selected tape and emit a locator JSON line. |
| `rem-debug archive read --library <SERIAL> --locator <JSON> --out <PATH>` | Read an object back by locator. |
| `rem-debug archive export-object ...` | Export the complete stored object bytes (including envelope) by locator. |
| `rem-debug archive verify --locator <JSON> --expected-sha256 <HEX>` | Stream and hash an object on tape against an expected digest, restoring nothing. |
| `rem-debug archive probe/scan/restore/recover --tape <SERIAL> --bay <BAY> [--rewind]` | Run the foreign-format driver directly against a mounted tape instead of a dump file. |
| `rem-debug tape alerts --bay <BAY>` | Read the loaded drive's TapeAlert LOG SENSE page directly. |

Destructive maintenance:

| Command | What it does |
|---|---|
| `rem-debug catalog reset --i-understand-this-erases-the-catalog` | Destructively reset local catalog state from the configured paths. |
| `rem-debug dev write-dump-to-tape --dump <PATH> --tape <SERIAL> --bay <BAY> --i-understand-this-overwrites-the-loaded-tape` | Overwrite the loaded scratch tape with a BRU dump (test fixture setup). |

The `--allow-derived <SERIAL>` flag additionally permits operating drive
bays whose identity was derived rather than read from the device; it must
be a subset of `--allow`.

<!-- code-anchor: crates/remanence-cli/src/lib.rs @ 2a20106 -->
## Catalog rebuild

`rem rebuild-catalog-from-journals [--config <PATH>]` rebuilds the SQLite
catalog projection from the audit log and per-tape journals. This is the
recovery path that makes the SQLite file a disposable cache rather than a
single point of failure.

<!-- code-anchor: crates/rao-recover/src/main.rs @ 2a20106 -->
## `rao-recover`: standalone recovery

`rao-recover` is a separate crate and binary (`crates/rao-recover`), not
part of `remanence-cli`. It exists for the case where the rest of the
stack — daemon, catalog, config file — is unavailable, untrusted, or
simply not worth standing up: given one RAO object file and a key, it
recovers plaintext members with nothing else.

```
rao-recover --object <FILE> --out <DIR> (--private-key <FILE> | --registry-key <FILE>)
            [--staging-dir <DIR>] [--overwrite]
```

It reads the object's header directly (no daemon/catalog dependency),
detects the format version itself, and dispatches accordingly:
`--registry-key` opens a v1 (registry symmetric key) object;
`--private-key` opens a v2 (HPKE) envelope using one recipient's private
key — the two flags are mutually exclusive, and supplying the wrong one
for the object's actual version fails with a message naming which
recipient epoch labels the object actually wants. Decrypted plaintext is
staged in a temporary file next to `--out` (or inside `--staging-dir` if
given, never `/tmp`) and securely zeroed on drop before the restore
completes. It validates the embedded object id against the header before
writing files out, and refuses to overwrite existing output unless
`--overwrite` is given.

This is currently the **only** way to fully decrypt a v2 (HPKE) envelope
end to end — neither `rem` nor `rem-debug` exposes an equivalent
whole-object open for v2.
