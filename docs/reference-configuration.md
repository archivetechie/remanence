# Configuration reference

<!-- code-anchor: crates/remanence-state/src/config.rs crates/remanence-daemon/src/main.rs @ 2a20106 -->
## The config file

Remanence reads a single TOML file. There is no config-file search path and
no environment variable that overrides its location: `rem-daemon` and every
config-consuming CLI subcommand take a `--config <PATH>` flag whose default
is `/etc/rem/config.toml`. Every table in the file rejects unknown keys, so a
typo in a key name is a hard error at load time, not a silently ignored
setting.

Four sections are required — `[daemon]`, `[journal]`, `[audit]`, `[index]` —
plus `[cache]`. Everything else may be omitted and takes the defaults listed
below. All path values must be absolute.

A minimal working config:

```toml
[daemon]
state_dir = "/var/lib/rem"
default_idle_timeout_seconds = 300
drive_idle_unload_seconds = 300

[journal]
dir = "/var/lib/rem/journal"

[audit]
dir = "/var/lib/rem/audit"

[index]
sqlite_path = "/var/lib/rem/rem-state.sqlite"

[cache]
tape_catalog_dir = "/var/lib/rem/tape-catalog"
```

Note that the CLI's default gRPC endpoint is `unix:/var/lib/rem/rem.sock`,
which only lines up with the daemon if `state_dir` is `/var/lib/rem` (the
socket defaults to `<state_dir>/rem.sock`). If you put state elsewhere, pass
`--endpoint` to the CLI or set `socket_path` explicitly.

### Byte sizes

Keys documented as accepting byte sizes take either a bare integer or a
string with a suffix: `B`, `KiB`/`K`/`KB`, `MiB`/`M`/`MB`, `GiB`/`G`/`GB`,
`TiB`/`T`/`TB`, `PiB`/`P`/`PB`. Every suffix is a power of 1024 — `KB` means
1024 bytes here, not 1000.

<!-- code-anchor: crates/remanence-state/src/config.rs crates/remanence-daemon/src/main.rs crates/remanence-daemon/src/tls.rs @ 2a20106 -->
## `[daemon]` (required)

| Key | Type | Default | Meaning |
|---|---|---|---|
| `state_dir` | absolute path | required | Root directory for mutable daemon state (default socket, default spool, and — only for `rem-debug` state-mutating subcommands, not `rem-daemon` itself — `state.lock`; see [What ends up on disk](#what-ends-up-on-disk)). |
| `default_idle_timeout_seconds` | integer > 0 | required | Default idle timeout for write/read sessions. |
| `drive_idle_unload_seconds` | integer ≥ 0 | `300` | Rewind, unload, and return a seated cartridge to its home slot after this many idle seconds. `0` keeps it seated until eviction or daemon shutdown. |
| `spool_dir` | absolute path | `<state_dir>/spool` | Pre-commit append spool. Created with mode `0700` at startup. |
| `spool_tmpfs_ram_budget` | byte size > 0 | unset | Required acknowledgement when the spool resolves to tmpfs/ramfs. Post-R2, spool growth reserves this fixed budget from the shared `io_memory_ceiling`; runtime `MemAvailable` never clamps or authorizes growth. Must be ≤ `io_memory_ceiling`. |
| `io_memory_ceiling` | byte size > 0 | `"24GiB"` | Fixed total for ALL pipeline I/O memory: append-spool reservations plus every drive's read reservoir, granted through one atomic permit manager. See the deployment note below. |
| `checkpoint_mode` | `"per_object"` or `"batched"` | `"per_object"` | Object durability policy. `per_object` preserves the historical synchronous delimiter and immediate catalog commit. `batched` uses advisory delimiters and checkpoint barriers, and is rejected at session admission for parity-enabled pools. |
| `checkpoint_max_bytes` | byte size > 0 | `"32GiB"` | In batched mode, request a barrier when pending logical bytes reach this limit. |
| `checkpoint_max_objects` | integer > 0 | `200` | In batched mode, request a barrier when the pending object count reaches this limit. |
| `checkpoint_max_age_seconds` | integer > 0 | `300` | Server-owned age deadline for an open batch. The timer queues its barrier through the drive actor, so an in-flight object finishes first. |
| `read_only` | bool | `false` | Reject state-changing operations; skips library discovery and the drive pool at startup. |
| `socket_path` | absolute path | `<state_dir>/rem.sock` | Unix-domain gRPC socket. Parent directory created `0700`; socket chmod `0660`; connecting peers must be root or the daemon's own user. |
| `listen` | `host:port` string | unset | TCP listen address for mTLS gRPC. Requires `[daemon.tls]`. |

### I/O memory ceiling deployment note (TIO-6 R2)

Specified by `design-tape-io-read-pipeline-v0.1.md` §4.6. The daemon unit must
run under a cgroup memory limit
(systemd `MemoryMax`) with `io_memory_ceiling` + daemon baseline headroom
≤ `MemoryMax` (guidance: leave ≥ 2 GiB), and `LimitMEMLOCK` sized ≥ the
ceiling (a safe upper bound: only read-reservoir slabs are actually
mlocked). Residency scoping: **read-reservoir slabs are mlocked —
never-swap holds for them**; the tmpfs spool is **ceiling-reserved but
NOT guaranteed-resident** (tmpfs pages are file-backed and swappable
under memory pressure — the reservation bounds its size, not its
residency). `MemAvailable` and `RLIMIT_MEMLOCK` are consulted once at
startup as loud sanity warnings only; the ceiling is the authority and
the cgroup limit is the enforcement backstop.

### `[daemon.tls]`

Optional, but must be present if and only if `listen` is set. All three keys
are required, with no defaults:

| Key | Meaning |
|---|---|
| `cert` | Server certificate PEM. |
| `key` | Server private key PEM. The file must not be group- or world-accessible (mode bits `077` must be clear) or the daemon refuses to start. |
| `client_ca` | CA certificate PEM that every client certificate must chain to. Clients without a valid certificate are rejected at the TLS layer. |

### gRPC transport flow control

The daemon explicitly advertises 4 MiB HTTP/2 initial stream and connection
flow-control windows on both its Unix-socket and TCP/mTLS servers. These values
are compiled transport defaults rather than configuration keys. The restore
server defaults a client-requested `stream_chunk_bytes = 0` to 256 KiB and
bounds its delivery queue to a 4 MiB byte budget (or one message when a client
explicitly requests a chunk larger than that budget).

TCP clients must configure matching HTTP/2 initial stream and connection
windows of at least 4 MiB; changing only the server cannot remove a smaller
client receive window. With tonic, apply `initial_stream_window_size(4 * 1024 *
1024)` and `initial_connection_window_size(4 * 1024 * 1024)` to the client
`Endpoint`. Client-side flag ownership is intentionally deferred; until that
surface exists, client implementations must set these values directly.

### HTTP/2 keepalive (dead-peer detection) — TIO-6 R2 transport defaults

Specified by `design-tape-io-read-pipeline-v0.1.md` §4.5. Like the windows,
these are compiled transport defaults, not configuration keys:

- **Server (tonic builder, both listeners — TCP/mTLS and Unix socket):**
  `http2_keepalive_interval = 30 s`, `http2_keepalive_timeout = 20 s`.
- **Client (tonic `Endpoint`):** `http2_keep_alive_interval = 30 s`,
  `keep_alive_timeout = 20 s`, `keep_alive_while_idle(true)`.

The PING must run while streams are idle or send-stalled — a parked read
reservoir behind a stalled sender is exactly the guarded state — so a
half-open peer (power loss, no FIN) tears down the connection, stream,
receiver, and session within roughly interval + timeout (≤ ~50 s), and a
drive parked for that session never moves. This bound is what makes the
park-indefinitely-for-slow-but-alive-clients policy coherent; it is
verified by the half-open-while-parked integration test required by the
TIO-6 design (§10).

<!-- code-anchor: crates/remanence-state/src/config.rs @ 2a20106 -->
## `[[libraries]]`

An array of tables, one per tape library the daemon may operate. This is the
daemon-side allowlist: libraries not listed here are visible to discovery
but never mutated. Serials must be non-empty and unique.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `serial` | string | required | Library serial (VPD page 0x80 of the medium changer; `rem libraries` shows it). |
| `allow_derived_drive_identity` | bool | `false` | Permit operating drive bays whose identity had to be derived rather than read directly from the device. |

```toml
[[libraries]]
serial = "DEC91001xx"
```

<!-- code-anchor: crates/remanence-state/src/config.rs @ 2a20106 -->
## `[[tape_pools]]` and `[[tape_pool_rules]]`

Tape pools group cartridges for write targeting; a write session names a
pool, and Remanence picks the tape. Pool ids may use letters, digits, and
`._:-`, and must be unique.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `id` | string | required | Stable pool identifier. |
| `display_name` | string | unset | Human-readable label. |
| `copy_class` | string | unset | Copy-segregation axis (for example `copy-a`), so redundant copies land on different pools. |
| `content_class` | string | unset | Content-segregation axis (for example `camera`). |
| `selection_policy` | string | `"complete-or-fill"` | Within-pool tape choice: `"complete-or-fill"` or `"fill-oldest"`. |
| `watermark_low` | float | `0.92` | Fill target as a fraction of capacity; a tape at or above it is a candidate for sealing. |
| `watermark_high` | float | `0.97` | Usable-capacity cap below end-of-media. Must satisfy `0 < low < high <= 1`. |
| `block_size` | byte size | `262144` (256 KiB) | Fixed tape block size applied when a fresh tape is initialized into this pool. Must be exactly 256 KiB, 512 KiB, or 1 MiB. |
| `min_object_size` | byte size | `0` | Minimum object/bundle size the orchestrator promises; checked against the watermark band. |

`[[tape_pool_rules]]` maps barcodes to pools by prefix. Prefixes are ASCII
alphanumeric, matched case-insensitively, longest match wins, and each
`pool_id` must reference a defined pool:

```toml
[[tape_pools]]
id = "archive-a"
display_name = "Archive copy A"

[[tape_pool_rules]]
prefix = "RMA"
pool_id = "archive-a"
```

<!-- code-anchor: crates/remanence-state/src/config.rs crates/remanence-api/src/lib.rs @ 2a20106 -->
## `[drives]`

Drive-stewardship settings. The whole section is optional.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `managed_libraries` | array of strings | `[]` | Library serials whose drives Remanence actively stewards (polling, cleaning, history). Empty means "the daemon-operated libraries". |
| `foreign_counter_poll` | duration string | `"60m"` | Error-counter poll cadence for foreign (unmanaged) drives. |
| `foreign_tapealert` | bool | `false` | Opt in to reading TapeAlert flags from foreign drives. |
| `heartbeat` | duration string | `"1h"` | Liveness cadence for managed drives. |
| `snapshot_miss_alarm` | integer | `3` | Consecutive missed health snapshots before an alarm is raised. |

## `[cleaning]`

Automatic drive cleaning. Optional.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `auto` | bool | `true` | Enable automatic cleaning runs. |
| `voltag_prefixes` | array of strings | `["CLN"]` | Barcode prefixes recognized as cleaning cartridges. |
| `use_warn` | integer | `45` | Cleaning-cartridge use count that triggers a warning. |
| `complete_timeout` | duration string | `"10m"` | Maximum duration of one cleaning run. |
| `min_cycle_duration` | duration string | `"60s"` | Shortest duration accepted as a genuine completed cleaning cycle. |
| `min_interval` | duration string | `"12h"` | Minimum interval between automatic cleans of the same drive. |
| `weekly_cap` | integer | `4` | Maximum automatic cleans per drive per week. |

## `[livestatus]`

Serves `rem top` and the live-status RPC. Optional.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `min_poll_interval` | duration string | `"250ms"` | Daemon-enforced minimum client poll interval. |
| `foreign_changer_poll` | duration string | `"60s"` | Inventory poll cadence for foreign changers while live-status clients are active. |
| `foreign_poll_lease` | duration string | `"5m"` | How recently a client must have polled to count as active. |

<!-- code-anchor: crates/remanence-state/src/config.rs crates/remanence-library/src/handle/mod.rs @ 2a20106 -->
## `[tape_io]`

Tape I/O batching and staging-ring settings. Optional. Fixed-block pipelined
submission is the only tape transfer path; removed mode keys are rejected.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `staging_ring_buffers` | integer 2..=16 | `4` | Page-aligned buffers allocated per active drive for staged submission. |
| `write_batch_blocks` | integer > 0 | `16` | Fixed-size records requested per WRITE(6) before the SG driver or HBA clamps the transfer. |
| `read_batch_blocks` | integer > 0 | `16` | Fixed-size records requested per READ(6). |
| `read_reservoir_bytes` | byte size > 0 | `"8GiB"` | Per-restore-stream reservoir target. Must hold at least `staging_ring_buffers × effective_read_batch_blocks × block_size` and must not exceed `io_memory_ceiling`. |
| `read_reservoir_high_pct` | integer 1..=100 | `90` | Stop issuing READs when reservoir occupancy reaches this percentage of its effective capacity. Must be greater than `read_reservoir_low_pct`. |
| `read_reservoir_low_pct` | integer 1..=99 | `25` | Resume READ submission after occupancy drains to this percentage. Must be less than `read_reservoir_high_pct`. |
| `position_check_bytes` | byte size | `1073741824` (1 GiB) | Cadence of mid-stream READ POSITION drift tripwires. `0` disables mid-stream checks (boundary checks remain). |
| `position_check_bytes_ranged` | byte size > 0 | `"256MiB"` | Proof cadence for hash-less ranged reads, effectively clamped to half the active reservoir. |

Two different points enforce these constraints. Config *loading* rejects
`high_pct ≤ low_pct` (or either out of `1..=100`/`1..=99`), a zero or
out-of-range `staging_ring_buffers`, and a `read_reservoir_bytes` that is
zero or exceeds `io_memory_ceiling` — these fail at daemon startup or CLI
config load, before anything touches hardware. A reservoir smaller than
the minimum staging pool (`staging_ring_buffers × read_batch_blocks ×
block_size`) is instead caught later, the first time a read pipeline
actually starts (`"read reservoir N bytes is smaller than minimum pool M
bytes"`) — a config that passes validation can still refuse the first
read if these two settings are mismatched.

<!-- code-anchor: crates/remanence-state/src/config.rs crates/remanence-state/src/paths.rs @ 2a20106 -->
## `[journal]`, `[audit]`, `[index]`, `[cache]` (required)

These four sections place the durable state. They are deliberately
independent paths — nothing forces them under `state_dir`, though the
minimal config above puts them there.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `journal.dir` | absolute path | required | Directory of per-tape journals, one `<32-hex-uuid>.remjournal` file per tape. |
| `journal.require_trusted_volume` | bool | `true` | Refuse to start if state, journal, audit, index, cache, or socket paths sit on tmpfs, ramfs, NFS, SMB/CIFS, or overlayfs. |
| `audit.dir` | absolute path | required | Directory of daily append-only `.remaudit` segments. |
| `audit.fsync` | bool | `true` | fsync each audit append before returning. |
| `audit.clock_forward_tolerance_seconds` | integer | `300` | Wall-clock forward jump tolerated before an audit warning. |
| `index.sqlite_path` | absolute path | required | The SQLite catalog projection file. The filename is yours to choose; `rem-state.sqlite` is the conventional name. This file is a rebuildable cache — see `rem rebuild-catalog-from-journals`. |
| `cache.tape_catalog_dir` | absolute path | required | Directory of per-tape catalog cache files. |

<!-- code-anchor: crates/remanence-daemon/src/main.rs crates/remanence-chaos/src/lib.rs crates/remanence-state/src/audit.rs @ 2a20106 -->
## Environment variables

Remanence reads very little from the environment; configuration belongs in
the config file.

| Variable | Read by | Meaning |
|---|---|---|
| `RUST_LOG` | `rem-daemon` | Standard tracing filter (via tracing-subscriber's env filter). Defaults to `info` when unset or unparsable. |
| `REM_CHAOS_ENABLED` | chaos-aware binaries | First gate for fault injection. Truthy values: `1`, `true`, `TRUE`, `yes`, `YES`, `on`, `ON`. |
| `REM_CHAOS_ALLOW_REAL` | `rem-debug` | Second gate, required in addition to `REM_CHAOS_ENABLED` before real hardware transports are wrapped with fault injection. Both unset is the safe production state. |
| `REM_CHAOS_STATE` | chaos engine | Path to the chaos scenario state file; required when chaos is enabled. |
| `USER` / `LOGNAME` | audit log | Recorded as the acting user for local CLI mutations; falls back to a system actor. |
| `HOSTNAME` | audit log, lock file | Last-resort host identity after `/etc/machine-id` and `/proc/sys/kernel/hostname`. |

Hardware integration tests read additional `REM_QUADSTOR_*` variables; they
are documented in the test modules and are never read by production code.

<!-- code-anchor: crates/remanence-state/src/paths.rs crates/remanence-state/src/lock.rs crates/remanence-daemon/src/lib.rs @ 2a20106 -->
## What ends up on disk

For the minimal config above, a running daemon owns:

```text
/var/lib/rem/                     state_dir
/var/lib/rem/state.lock           only written by rem-debug state-mutating commands (see below)
/var/lib/rem/rem.sock             gRPC Unix socket (mode 0660)
/var/lib/rem/spool/               pre-commit append spool (mode 0700, spool-<uuid>.bin files)
/var/lib/rem/journal/*.remjournal per-tape journals (source of truth on disk)
/var/lib/rem/audit/*.remaudit     daily append-only audit segments
/var/lib/rem/rem-state.sqlite     rebuildable SQLite catalog projection
/var/lib/rem/tape-catalog/        per-tape catalog cache files
```

`state.lock` is a kernel `flock`, and **`rem-daemon` never takes it** —
it opens the SQLite catalog and drive pool directly. Only `rem-debug`'s
offline state-mutating subcommands (tape init, pool ops, catalog reset,
and similar) acquire it, to serialize themselves against each other; the
lock is released automatically if the holding process dies, regardless
of what the file's `pid=`/`host_id=` diagnostics still say. This means
`rem-daemon`'s only defense against a second instance starting is a
liveness probe against its own Unix socket path at bind time — nothing
stops two daemons with *different* `socket_path`s from both starting
against the same `state_dir`. On startup, a non-read-only `rem-daemon`
also deletes any of its own leftover `spool-<uuid>.bin` files from a
prior unclean exit before resolving the spool budget.

The audit log and per-tape journals are append-only records; the SQLite
file is a projection that `rem rebuild-catalog-from-journals` can
regenerate from them. The tape itself stays the ultimate authority — its
bootstrap and parity structures support a catalog-less scan. TLS material
is read from wherever `[daemon.tls]` points and is never written by the
daemon.
